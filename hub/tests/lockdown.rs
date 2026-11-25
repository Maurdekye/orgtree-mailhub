//! Lockdown (feature `lockdown`, v2.6.0, schema 17; Orgtree docket
//! org-mailbox-lockdown-allow-list-mode-that-denies, user 2026-11-25
//! 13:26Z and 13:37Z): an address turns on lockdown with a list of
//! addresses that may still write to it (`POST /api/lockdown`), and the hub
//! refuses everyone else's sends to it with 403, drops their typing
//! heartbeats, and shows everyone the lock: those on the list see the
//! address as usual with `locked: "open"`, everyone else sees
//! `locked: "closed"` and none of its presence. A lockdown belongs to its
//! owner (the address and its secret's fingerprint), as a block does.
//!
//!     HUB_TEST_PG=<folder> cargo test --test lockdown -- --nocapture

mod support;

use std::sync::Arc;
use std::time::{Duration, Instant};

use mailhub::api::sync::presence_loop;
use mailhub::Hub;
use serde_json::{json, Value};
use support::*;

type Id = (String, String);

async fn register_with(hub: &Arc<Hub>, slug: &str, secret: &str, kind: &str) -> Id {
    let r = Call::new("POST", "/api/register")
        .auth(pair(slug, secret))
        .json(json!({ "slug": slug, "org_name": slug, "username": "lock", "kind": kind }))
        .send(hub)
        .await;
    assert_eq!(r.code(), 200, "{}", r.text());
    (slug.to_string(), secret.to_string())
}

async fn register(hub: &Arc<Hub>, name: &str, kind: &str) -> Id {
    register_with(hub, &format!("{name}.lock.{}", token_hex3()), &token_hex16(), kind).await
}

async fn post(hub: &Arc<Hub>, who: &Id, path: &str, body: Value) -> Resp {
    Call::new("POST", path).auth(pair(&who.0, &who.1)).json(body).send(hub).await
}

async fn lock(hub: &Arc<Hub>, who: &Id, allow: &[&str]) -> Resp {
    post(hub, who, "/api/lockdown", json!({ "on": true, "allow": allow })).await
}

async fn unlock(hub: &Arc<Hub>, who: &Id) -> Resp {
    post(hub, who, "/api/lockdown", json!({ "on": false })).await
}

async fn state(hub: &Arc<Hub>, who: &Id) -> Value {
    let r = Call::new("GET", "/api/lockdown").auth(pair(&who.0, &who.1)).send(hub).await;
    assert_eq!(r.code(), 200, "{}", r.text());
    r.json()
}

async fn send(hub: &Arc<Hub>, from: &Id, to: &str, id: &str) -> Resp {
    post(hub, from, "/api/send", json!({ "id": id, "to": to, "body": format!("hello {id}") })).await
}

fn strings(v: &Value) -> Vec<String> {
    v.as_array().unwrap_or_else(|| panic!("not a list: {v}")).iter().map(|s| s.as_str().unwrap().to_string()).collect()
}

/// A sync that answers at once, following `more`.
async fn sync_now(hub: &Arc<Hub>, who: &Id, device: &str, cursor: Option<&Value>) -> Value {
    let mut body = json!({ "device_id": device, "wait": 0 });
    if let Some(c) = cursor {
        body["cursor"] = c.clone();
    }
    loop {
        let r = Call::new("POST", "/api/sync").auth(pair(&who.0, &who.1)).json(body.clone()).send(hub).await;
        assert_eq!(r.code(), 200, "{}", r.text());
        let a = r.json();
        if a["more"] != json!(true) {
            return a;
        }
        body["cursor"] = a["cursor"].clone();
    }
}

fn roster_row<'a>(a: &'a Value, slug: &str) -> Option<&'a Value> {
    a["roster"].as_array().and_then(|r| r.iter().find(|e| e["slug"] == json!(slug)))
}

async fn test_hub(name: &str) -> (TestPg, Arc<Hub>, String) {
    let pg = TestPg::start();
    let url = pg.fresh_db(name).await;
    let data = std::env::temp_dir().join(format!("orgtree-hub-{name}-{}", token_hex16()));
    std::fs::create_dir_all(&data).unwrap();
    let hub = hub(&url, &data, &[]).await;
    tokio::spawn(presence_loop(hub.clone()));
    (pg, hub, url)
}

#[tokio::test]
async fn a_lockdown_is_set_read_changed_and_lifted() {
    let (_pg, hub, _url) = test_hub("hublockset").await;
    let r = Call::new("GET", "/healthz").send(&hub).await;
    assert!(r.json()["features"].as_array().unwrap().contains(&json!("lockdown")), "{}", r.text());

    let org = register(&hub, "org", "org").await;
    let ann = register(&hub, "ann", "person").await;
    assert_eq!(state(&hub, &org).await["on"], json!(false));

    let r = lock(&hub, &org, &[&ann.0, "not.here.yet"]).await;
    assert_eq!(r.code(), 200, "{}", r.text());
    let j = r.json();
    assert_eq!(j["on"], json!(true));
    let mut allow = strings(&j["allow"]);
    allow.sort();
    let mut want = vec![ann.0.clone(), "not.here.yet".to_string()];
    want.sort();
    assert_eq!(allow, want, "an address the hub does not know may be listed ahead");
    assert!(j["since"].is_string(), "{j}");
    let since = j["since"].clone();
    // `on` without `allow` keeps the list; with it, replaces it
    let r = post(&hub, &org, "/api/lockdown", json!({ "on": true })).await;
    assert_eq!(r.code(), 200, "{}", r.text());
    assert_eq!(r.json()["allow"].as_array().unwrap().len(), 2, "kept: {}", r.text());
    assert_eq!(r.json()["since"], since, "still the same lockdown");
    let r = lock(&hub, &org, &[&ann.0]).await;
    assert_eq!(strings(&r.json()["allow"]), vec![ann.0.clone()]);
    assert_eq!(state(&hub, &org).await["allow"], json!([ann.0]));
    // an empty list is allowed: nobody on the hub may write
    assert_eq!(lock(&hub, &org, &[]).await.code(), 200);
    assert_eq!(state(&hub, &org).await["allow"], json!([]));
    println!("  ok  set, read, keep, replace, empty");

    for (bad, what) in [(json!({ "on": true, "allow": [org.0] }), "itself"),
                        (json!({ "on": true, "allow": ["Not A Slug"] }), "a malformed address"),
                        (json!({ "on": true, "allow": "x" }), "allow that is not a list"),
                        (json!({ "allow": [] }), "no on flag"),
                        (json!({ "on": "yes" }), "on that is not a boolean")] {
        let r = post(&hub, &org, "/api/lockdown", bad).await;
        assert_eq!(r.code(), 422, "{what}: {}", r.text());
    }
    let many: Vec<String> = (0..1001).map(|i| format!("x{i}")).collect();
    let r = post(&hub, &org, "/api/lockdown", json!({ "on": true, "allow": many })).await;
    assert_eq!(r.code(), 422, "over the cap: {}", r.text());
    assert_eq!(Call::new("GET", "/api/lockdown").send(&hub).await.code(), 401);
    assert_eq!(Call::new("POST", "/api/lockdown").json(json!({ "on": false })).send(&hub).await.code(), 401);
    println!("  ok  refusals: itself, malformed, not a list, flags, cap, no credentials");

    let r = unlock(&hub, &org).await;
    assert_eq!(r.code(), 200, "{}", r.text());
    assert_eq!(r.json()["on"], json!(false));
    assert_eq!(unlock(&hub, &org).await.code(), 200, "lifting twice is fine");
    let r = post(&hub, &org, "/api/lockdown", json!({ "on": true })).await;
    assert_eq!(r.json()["allow"], json!([]), "a new lockdown starts with no list: {}", r.text());
    println!("  ok  lifted, idempotent, the old list is gone");
}

#[tokio::test]
async fn only_listed_addresses_may_write_and_nothing_else_is_kept() {
    let (_pg, hub, url) = test_hub("hublocksend").await;
    let org = register(&hub, "org", "org").await;
    let ann = register(&hub, "ann", "person").await;
    let bob = register(&hub, "bob", "person").await;

    assert_eq!(send(&hub, &bob, &org.0, "m-before").await.code(), 200);
    assert_eq!(lock(&hub, &org, &[&ann.0]).await.code(), 200);
    assert_eq!(send(&hub, &bob, &org.0, "m-before").await.code(), 200, "a retry of mail accepted before the lockdown");

    let r = send(&hub, &bob, &org.0, "m-after").await;
    assert_eq!(r.code(), 403, "{}", r.text());
    let j = r.json();
    assert_eq!(j["detail"], json!(format!("@{} is in lockdown and only takes messages from addresses on its list", org.0)));
    assert_eq!(j["locked"], json!(org.0));
    let c = sql_client(&url).await;
    let kept = c.query_one("SELECT count(*) FROM messages WHERE id = 'm-after'", &[]).await.unwrap().get::<_, i64>(0);
    assert_eq!(kept, 0, "a refused message is not stored");
    println!("  ok  an unlisted sender: 403 with `locked`, nothing stored");

    assert_eq!(send(&hub, &ann, &org.0, "m-listed").await.code(), 200, "a listed sender");
    // the locked address may still write to anyone (user 13:37Z, Q4)
    assert_eq!(send(&hub, &org, &bob.0, "m-out").await.code(), 200);
    // a reply in a thread is mail like any other (Q1)
    let r = post(&hub, &bob, "/api/send", json!({ "id": "m-reply", "to": org.0, "body": "re", "reply_to": "m-out" })).await;
    assert_eq!(r.code(), 403, "{}", r.text());
    println!("  ok  listed passes; the org writes out; an unlisted reply is refused too");

    // a block is told before the lockdown
    let r = post(&hub, &org, "/api/blocks", json!({ "address": bob.0, "blocked": true })).await;
    assert_eq!(r.code(), 200, "{}", r.text());
    assert_eq!(send(&hub, &bob, &org.0, "m-blocked").await.json()["blocked_by"], json!(org.0));
    assert_eq!(post(&hub, &org, "/api/blocks", json!({ "address": bob.0, "blocked": false })).await.code(), 200);

    assert_eq!(unlock(&hub, &org).await.code(), 200);
    assert_eq!(send(&hub, &bob, &org.0, "m-lifted").await.code(), 200);
    println!("  ok  a block wins; lifted: everyone may write");
}

#[tokio::test]
async fn an_unlisted_senders_heartbeats_are_not_relayed() {
    let (_pg, hub, _url) = test_hub("hublocktyping").await;
    let org = register(&hub, "org", "person").await;
    let ann = register(&hub, "ann", "person").await;
    let bob = register(&hub, "bob", "person").await;
    assert_eq!(lock(&hub, &org, &[&ann.0]).await.code(), 200);
    let first = sync_now(&hub, &org, "org-desk", None).await;
    for who in [&bob, &ann] {
        let r = post(&hub, who, "/api/typing", json!({ "to": [org.0] })).await;
        assert_eq!(r.code(), 200, "{}", r.text());
    }
    let body = json!({ "device_id": "org-desk", "wait": 0, "cursor": first["cursor"], "typing": true });
    let r = Call::new("POST", "/api/sync").auth(pair(&org.0, &org.1)).json(body).send(&hub).await;
    let from: Vec<String> = r.json()["typing"].as_array().unwrap().iter().map(|x| x["from"].as_str().unwrap().to_string()).collect();
    assert_eq!(from, vec![ann.0.clone()], "only the listed one");
    println!("  ok  the unlisted sender's heartbeat is dropped");
}

#[tokio::test]
async fn everyone_sees_the_lock_and_the_unlisted_see_nothing_else() {
    let (_pg, hub, _url) = test_hub("hublocksee").await;
    let org = register(&hub, "org", "org").await;
    let ann = register(&hub, "ann", "person").await;
    let bob = register(&hub, "bob", "person").await;
    // the org is online (it polls)
    let _ = Call::new("POST", "/api/poll?wait=0").auth(pair(&org.0, &org.1)).send(&hub).await;

    let b = sync_now(&hub, &bob, "bob-phone", None).await;
    assert!(b.get("locked").is_none() || b["locked"] == json!({}), "nobody locked: as before: {b}");
    assert!(strings(&b["online"]).contains(&org.0), "{b}");
    assert!(roster_row(&b, &org.0).unwrap().get("locked").is_none());

    // bob's phone in use, parked: the lockdown wakes it
    let r = post(&hub, &bob, "/api/devices/active", json!({ "device_id": "bob-phone", "active": true })).await;
    assert_eq!(r.code(), 200, "{}", r.text());
    let b = sync_now(&hub, &bob, "bob-phone", Some(&b["cursor"])).await;
    let (h, w, cur) = (hub.clone(), bob.clone(), b["cursor"].clone());
    let parked = tokio::spawn(async move {
        let r = Call::new("POST", "/api/sync").auth(pair(&w.0, &w.1)).json(json!({ "device_id": "bob-phone", "wait": 20, "cursor": cur })).send(&h).await;
        (r.json(), Instant::now())
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    let t0 = Instant::now();
    assert_eq!(lock(&hub, &org, &[&ann.0]).await.code(), 200);
    let (b, at) = tokio::time::timeout(Duration::from_secs(5), parked).await.expect("the parked sync never answered").unwrap();
    assert!(at.duration_since(t0) < Duration::from_secs(3), "answered after {:?}", at.duration_since(t0));
    let b = if b["more"] == json!(true) { sync_now(&hub, &bob, "bob-phone", Some(&b["cursor"])).await } else { b };
    assert_eq!(b["locked"][&org.0], json!("closed"), "{b}");
    println!("  ok  an unlisted device in use hears at once ({:?})", at.duration_since(t0));

    // closed: the lock and nothing of its presence
    let b = sync_now(&hub, &bob, "bob-laptop", None).await;
    assert_eq!(b["locked"], json!({ (org.0.clone()): "closed" }), "{b}");
    for set in ["online", "busy", "dnd", "active_now"] {
        assert!(!strings(&b[set]).contains(&org.0), "{set}: {b}");
    }
    let row = roster_row(&b, &org.0).expect("the org stays in the roster");
    assert_eq!(row["locked"], json!("closed"));
    assert_eq!((row["online"].clone(), row["busy"].clone(), row["dnd"].clone(), row["active"].clone()), (json!(false), json!(false), json!(false), json!(false)), "{row}");
    assert_eq!((row["last_seen"].clone(), row["active_at"].clone()), (Value::Null, Value::Null), "{row}");
    println!("  ok  closed: a lock in place of its presence");

    // open: as usual, with the lock
    let a = sync_now(&hub, &ann, "ann-phone", None).await;
    assert_eq!(a["locked"], json!({ (org.0.clone()): "open" }), "{a}");
    assert!(strings(&a["online"]).contains(&org.0), "{a}");
    let row = roster_row(&a, &org.0).unwrap();
    assert_eq!(row["locked"], json!("open"));
    assert_eq!(row["online"], json!(true));
    println!("  ok  open: its presence and a small lock");

    // the same through the roster, the directory and a poll
    let r = Call::new("GET", "/api/roster").auth(pair(&bob.0, &bob.1)).send(&hub).await.json();
    assert_eq!(roster_row(&r, &org.0).unwrap()["locked"], json!("closed"), "{r}");
    let d = Call::new("GET", &format!("/api/directory?q={}", org.0)).auth(pair(&bob.0, &bob.1)).send(&hub).await.json();
    let e = d["entries"].as_array().unwrap().iter().find(|e| e["slug"] == json!(org.0)).unwrap().clone();
    assert_eq!((e["locked"].clone(), e["online"].clone()), (json!("closed"), json!(false)), "{e}");
    let other = register(&hub, "acme", "org").await;
    let p = Call::new("POST", "/api/poll?wait=0").auth(pair(&other.0, &other.1)).send(&hub).await.json();
    assert_eq!(p["locked"][&org.0], json!("closed"), "{p}");
    assert_eq!(roster_row(&p, &org.0).unwrap()["online"], json!(false));
    println!("  ok  roster, directory and poll the same");

    // the locked org's own poll tells it its lockdown
    let p = Call::new("POST", "/api/poll?wait=0").auth(pair(&org.0, &org.1)).send(&hub).await.json();
    assert_eq!(p["lockdown"][&org.0]["allow"], json!([ann.0]), "{p}");
    println!("  ok  the locked org's poll carries its own lockdown");

    // a change of list moves an address between closed and open
    assert_eq!(lock(&hub, &org, &[&bob.0]).await.code(), 200);
    let b2 = sync_now(&hub, &bob, "bob-laptop", Some(&b["cursor"])).await;
    assert_eq!(b2["locked"][&org.0], json!("open"), "{b2}");
    assert_eq!(roster_row(&b2, &org.0).expect("its row is sent again")["locked"], json!("open"));
    // lifted: no lock at all
    assert_eq!(unlock(&hub, &org).await.code(), 200);
    let b3 = sync_now(&hub, &bob, "bob-laptop", Some(&b2["cursor"])).await;
    assert_eq!(b3["locked"], json!({}), "{b3}");
    assert!(roster_row(&b3, &org.0).expect("its row is sent again").get("locked").is_none());
    println!("  ok  changes and the lift are heard");
}

#[tokio::test]
async fn a_lockdown_belongs_to_its_owner_not_to_the_address() {
    let (_pg, hub, url) = test_hub("hublockowner").await;
    let bob = register(&hub, "bob", "person").await;
    let slug = format!("org.owner.{}", token_hex3());
    let first = register_with(&hub, &slug, &token_hex16(), "org").await;
    assert_eq!(lock(&hub, &first, &[]).await.code(), 200);

    // the same owner leaves and comes back: the lockdown holds
    assert_eq!(post(&hub, &first, "/api/unregister", json!({})).await.code(), 200);
    register_with(&hub, &first.0, &first.1, "org").await;
    assert_eq!(state(&hub, &first).await["on"], json!(true));
    assert_eq!(send(&hub, &bob, &slug, "m-1").await.code(), 403);
    println!("  ok  the same owner keeps its lockdown");

    // someone else takes the freed address: it starts open
    assert_eq!(post(&hub, &first, "/api/unregister", json!({})).await.code(), 200);
    let second = register_with(&hub, &slug, &token_hex16(), "org").await;
    assert_eq!(state(&hub, &second).await["on"], json!(false));
    assert_eq!(send(&hub, &bob, &slug, "m-2").await.code(), 200);
    let c = sql_client(&url).await;
    let left = c.query_one("SELECT count(*) FROM lockdowns WHERE address = $1", &[&slug]).await.unwrap().get::<_, i64>(0);
    assert_eq!(left, 0, "the old owner's lockdown is gone");
    println!("  ok  a new owner inherits nothing");
}
