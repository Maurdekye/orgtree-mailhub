//! Phase 2, slice 5: a complete roster for the directory (G9) — no idle
//! prune by default, search and paging, removal by the operator.
//!
//!     HUB_TEST_PG=<folder> cargo test --test directory -- --nocapture

mod support;

use std::sync::Arc;

use mailhub::Hub;
use serde_json::{json, Value};
use support::*;

type Id = (String, String);

async fn register(hub: &Arc<Hub>, slug: &str, name: &str, user: &str, about: &str, kind: &str) -> Id {
    let id = (slug.to_string(), token_hex16());
    let r = Call::new("POST", "/api/register")
        .auth(pair(&id.0, &id.1))
        .json(json!({ "slug": id.0, "org_name": name, "username": user, "blurb": about, "kind": kind }))
        .send(hub)
        .await;
    assert_eq!(r.code(), 200, "{}", r.text());
    id
}

async fn search(hub: &Arc<Hub>, who: &Id, query: &str) -> Value {
    let r = Call::new("GET", &format!("/api/directory{query}")).auth(pair(&who.0, &who.1)).send(hub).await;
    assert_eq!(r.code(), 200, "{query}: {}", r.text());
    r.json()
}

fn slugs(v: &Value) -> Vec<String> {
    v["entries"].as_array().unwrap().iter().map(|e| e["slug"].as_str().unwrap().to_string()).collect()
}

#[tokio::test]
async fn the_directory_is_complete() {
    let pg = TestPg::start();
    let data = std::env::temp_dir().join(format!("orgtree-hub-dir-{}", token_hex16()));
    std::fs::create_dir_all(&data).unwrap();

    // ------------------------------------------- no idle prune by default
    let url = pg.fresh_db("hubdir").await;
    let hub = hub(&url, &data, &[("HUB_RETENTION_DAYS", "")]).await;
    let sql = sql_client(&url).await;
    let me = register(&hub, "me.dir.aaaaaa", "Me", "dir", "", "person").await;
    let idle = register(&hub, "idle.dir.bbbbbb", "Idle One", "dir", "away a while", "person").await;
    sql.execute("UPDATE identities SET last_seen = now() - interval '60 days', registered_at = now() - interval '90 days' WHERE slug = $1", &[&idle.0])
        .await
        .unwrap();
    let swept = mailhub::sweep::run_once(&hub).await.unwrap();
    assert!(swept.pruned.is_empty(), "{swept:?}");
    let all = search(&hub, &me, "").await;
    assert!(slugs(&all).contains(&idle.0), "an address idle for 60 days left the directory");
    let roster = Call::new("GET", "/api/roster").auth(pair(&me.0, &me.1)).send(&hub).await.json();
    assert!(roster["roster"].as_array().unwrap().iter().any(|r| r["slug"] == json!(idle.0)));
    println!("  ok  an address idle for 60 days is still listed (no HUB_ORG_RETENTION_DAYS: no idle prune)");

    // ------------------------------------------------------- search, pages
    let people = [
        ("ann.dir.000001", "Ann Example", "annex", "on call this week", "person"),
        ("bob.dir.000002", "Bob", "builder", "Fixes 100% of bugs", "person"),
        ("chat.dir.000003", "chat session", "ncola_k8bx", "independent Claude Code chat", "chat"),
        ("org-a.dir.000004", "Research Org", "lab", "ANNUAL reports", "org"),
        ("under_score.dir.000005", "Under Score", "us", "snake_case fan", "person"),
    ];
    for (slug, name, user, about, kind) in people {
        register(&hub, slug, name, user, about, kind).await;
    }
    for i in 0..40 {
        register(&hub, &format!("filler-{i:02}.dir.ffffff"), &format!("Filler {i}"), "fill", "", "org").await;
    }
    // address, name, username and about line; any case
    assert_eq!(slugs(&search(&hub, &me, "?q=ann").await), ["ann.dir.000001", "org-a.dir.000004"]);
    assert_eq!(slugs(&search(&hub, &me, "?q=BUILDER").await), ["bob.dir.000002"]);
    assert_eq!(slugs(&search(&hub, &me, "?q=claude%20code").await), ["chat.dir.000003"]);
    assert_eq!(slugs(&search(&hub, &me, "?q=000004").await), ["org-a.dir.000004"]);
    // LIKE's own characters are plain text
    assert_eq!(slugs(&search(&hub, &me, "?q=100%25").await), ["bob.dir.000002"]);
    assert_eq!(slugs(&search(&hub, &me, "?q=e_c").await), ["under_score.dir.000005"]);
    assert_eq!(slugs(&search(&hub, &me, "?q=nobody-at-all").await), Vec::<String>::new());
    let ann = search(&hub, &me, "?q=Ann%20Example").await;
    let e = &ann["entries"][0];
    assert_eq!((e["kind"].clone(), e["blurb"].clone(), e["online"].clone()), (json!("person"), json!("on call this week"), json!(true)));
    assert!(e["last_seen"].is_string());
    // every address, a page at a time
    let mut seen = Vec::new();
    let mut after = String::new();
    let mut pages = 0;
    loop {
        let page = search(&hub, &me, &format!("?limit=10&after={after}")).await;
        let got = slugs(&page);
        assert!(got.len() <= 10);
        seen.extend(got);
        pages += 1;
        match page["after"].as_str() {
            Some(a) => after = a.to_string(),
            None => break,
        }
    }
    let mut sorted = seen.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!((seen.len(), sorted.len(), seen == sorted), (47, 47, true), "pages skipped, repeated or reordered entries");
    assert_eq!(search(&hub, &me, "?limit=100000").await["entries"].as_array().unwrap().len(), 47);
    let r = Call::new("GET", "/api/directory?limit=lots").auth(pair(&me.0, &me.1)).send(&hub).await;
    assert_eq!((r.code(), r.json()["detail"].clone()), (422, json!("limit must be a whole number")));
    assert_eq!(Call::new("GET", "/api/directory").send(&hub).await.code(), 401);
    println!("  ok  search over address, name, username and about line (any case, literal %/_), {pages} pages of 10 cover all 47 once, in order");

    // --------------------------------- the operator removes an address
    let cursor = Call::new("POST", "/api/sync").auth(pair(&me.0, &me.1)).json(json!({ "device_id": "d", "wait": 0 })).send(&hub).await.json();
    let mut cursor = cursor["cursor"].clone();
    loop {
        let a = Call::new("POST", "/api/sync").auth(pair(&me.0, &me.1)).json(json!({ "device_id": "d", "wait": 0, "cursor": cursor })).send(&hub).await.json();
        cursor = a["cursor"].clone();
        if a["more"] != json!(true) {
            break;
        }
    }
    let gone = mailhub::api::sync::remove_addresses(&hub.db, &[idle.0.clone(), "never.dir.registered".to_string()]).await.unwrap();
    assert_eq!(gone, std::slice::from_ref(&idle.0));
    assert!(!slugs(&search(&hub, &me, "?q=idle").await).contains(&idle.0));
    let a = Call::new("POST", "/api/sync").auth(pair(&me.0, &me.1)).json(json!({ "device_id": "d", "wait": 0, "cursor": cursor })).send(&hub).await.json();
    assert_eq!(a["roster_removed"], json!([idle.0]));
    println!("  ok  the operator's remove-address takes an address off the roster, and syncing directories hear of it");

    // ------------------------- an explicit HUB_ORG_RETENTION_DAYS prunes as v1
    let url2 = pg.fresh_db("hubdirprune").await;
    let data2 = data.join("second");
    std::fs::create_dir_all(&data2).unwrap();
    let hub2 = support::hub(&url2, &data2, &[("HUB_RETENTION_DAYS", ""), ("HUB_ORG_RETENTION_DAYS", "45")]).await;
    let sql2 = sql_client(&url2).await;
    let watcher = register(&hub2, "watcher.dir.cccccc", "Watcher", "dir", "", "person").await;
    let old = register(&hub2, "old.dir.dddddd", "Old", "dir", "", "org").await;
    let waiting = register(&hub2, "waiting.dir.eeeeee", "Waiting", "dir", "", "org").await;
    let r = Call::new("POST", "/api/send").auth(pair(&watcher.0, &watcher.1)).json(json!({ "to": waiting.0, "body": "for later" })).send(&hub2).await;
    assert_eq!(r.code(), 200);
    let first = Call::new("POST", "/api/sync").auth(pair(&watcher.0, &watcher.1)).json(json!({ "device_id": "w", "wait": 0 })).send(&hub2).await.json();
    sql2.execute("UPDATE identities SET last_seen = now() - interval '50 days' WHERE slug = ANY($1)", &[&vec![old.0.clone(), waiting.0.clone()]])
        .await
        .unwrap();
    let swept = mailhub::sweep::run_once(&hub2).await.unwrap();
    assert_eq!(swept.pruned, std::slice::from_ref(&old.0), "pruned the wrong addresses (one holding queued mail must stay)");
    let a = Call::new("POST", "/api/sync").auth(pair(&watcher.0, &watcher.1)).json(json!({ "device_id": "w", "wait": 0, "cursor": first["cursor"] })).send(&hub2).await.json();
    assert_eq!(a["roster_removed"], json!([old.0]));
    let h = Call::new("GET", "/healthz").send(&hub2).await.json();
    assert!(h["features"].as_array().unwrap().contains(&json!("directory")));
    println!("  ok  with HUB_ORG_RETENTION_DAYS set, silent addresses go as in v1 (not one holding queued mail), and syncing directories hear of it");

    let _ = std::fs::remove_dir_all(&data);
    drop(pg);
}
