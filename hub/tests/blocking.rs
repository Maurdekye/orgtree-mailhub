//! Blocking (feature `blocking`, v2.6.0, schema 16; Orgtree docket
//! blocking-contacts-on-the-hub-hubchat-1-4-0-orgtr, user 2026-11-24
//! 18:56Z): an address blocks another (`POST /api/blocks`), and the hub
//! refuses that address's sends to it with 403 and the words the user
//! chose ("you have been blocked by @<them> and cannot message them"),
//! drops its typing heartbeats, and tells every device of the blocker the
//! list in their sync answers. A block belongs to its owner (the address
//! and its secret's fingerprint): a new owner of a freed address does not
//! inherit it, and the same owner registering again keeps it.
//!
//!     HUB_TEST_PG=<folder> cargo test --test blocking -- --nocapture

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
        .json(json!({ "slug": slug, "org_name": slug, "username": "block", "kind": kind }))
        .send(hub)
        .await;
    assert_eq!(r.code(), 200, "{}", r.text());
    (slug.to_string(), secret.to_string())
}

async fn register(hub: &Arc<Hub>, name: &str, kind: &str) -> Id {
    register_with(hub, &format!("{name}.block.{}", token_hex3()), &token_hex16(), kind).await
}

async fn post(hub: &Arc<Hub>, who: &Id, path: &str, body: Value) -> Resp {
    Call::new("POST", path).auth(pair(&who.0, &who.1)).json(body).send(hub).await
}

async fn block(hub: &Arc<Hub>, who: &Id, address: &str, on: bool) -> Resp {
    post(hub, who, "/api/blocks", json!({ "address": address, "blocked": on })).await
}

async fn listed(hub: &Arc<Hub>, who: &Id) -> Vec<String> {
    let r = Call::new("GET", "/api/blocks").auth(pair(&who.0, &who.1)).send(hub).await;
    assert_eq!(r.code(), 200, "{}", r.text());
    strings(&r.json()["blocked"])
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

fn message_ids(a: &Value) -> Vec<String> {
    a["changes"].as_array().unwrap().iter().filter_map(|c| c["message"]["id"].as_str().or(c["id"].as_str()).map(str::to_string)).collect()
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
async fn a_block_is_set_listed_and_lifted() {
    let (_pg, hub, _url) = test_hub("hubblocklist").await;
    let r = Call::new("GET", "/healthz").send(&hub).await;
    assert!(r.json()["features"].as_array().unwrap().contains(&json!("blocking")), "{}", r.text());

    let ann = register(&hub, "ann", "person").await;
    let bob = register(&hub, "bob", "person").await;
    let org = register(&hub, "org", "org").await;

    assert_eq!(listed(&hub, &ann).await, Vec::<String>::new());
    let r = block(&hub, &ann, &bob.0, true).await;
    assert_eq!(r.code(), 200, "{}", r.text());
    assert_eq!(strings(&r.json()["blocked"]), vec![bob.0.clone()]);
    // idempotent, and an address the hub does not know may be blocked ahead
    let r = block(&hub, &ann, &bob.0, true).await;
    assert_eq!(strings(&r.json()["blocked"]), vec![bob.0.clone()]);
    let r = block(&hub, &ann, "not.here.yet", true).await;
    assert_eq!(r.code(), 200, "{}", r.text());
    assert_eq!(listed(&hub, &ann).await, vec![bob.0.clone(), "not.here.yet".to_string()], "oldest first");
    // an org blocks too
    assert_eq!(block(&hub, &org, &bob.0, true).await.code(), 200);
    println!("  ok  block, list, idempotent, ahead of time, an org");

    for (bad, what) in [(json!({ "address": ann.0, "blocked": true }), "itself"),
                        (json!({ "address": "Not A Slug", "blocked": true }), "a malformed address"),
                        (json!({ "blocked": true }), "no address"),
                        (json!({ "address": bob.0 }), "no blocked flag"),
                        (json!({ "address": bob.0, "blocked": "yes" }), "a blocked that is not a boolean")] {
        let r = post(&hub, &ann, "/api/blocks", bad).await;
        assert_eq!(r.code(), 422, "{what}: {}", r.text());
    }
    assert_eq!(Call::new("GET", "/api/blocks").send(&hub).await.code(), 401);
    assert_eq!(Call::new("POST", "/api/blocks").json(json!({ "address": bob.0, "blocked": true })).send(&hub).await.code(), 401);
    println!("  ok  refusals: itself, malformed, missing fields, no credentials");

    let r = block(&hub, &ann, &bob.0, false).await;
    assert_eq!(r.code(), 200, "{}", r.text());
    assert_eq!(strings(&r.json()["blocked"]), vec!["not.here.yet".to_string()]);
    assert_eq!(block(&hub, &ann, &bob.0, false).await.code(), 200, "lifting twice is fine");
    println!("  ok  unblock, idempotent");
}

#[tokio::test]
async fn a_blocked_sender_is_refused_with_the_users_words_and_nothing_is_kept() {
    let (_pg, hub, url) = test_hub("hubblocksend").await;
    let ann = register(&hub, "ann", "person").await;
    let bob = register(&hub, "bob", "person").await;
    let org = register(&hub, "org", "org").await;

    // accepted before the block: a retry of it stays accepted
    assert_eq!(send(&hub, &bob, &ann.0, "m-before").await.code(), 200);
    assert_eq!(block(&hub, &ann, &bob.0, true).await.code(), 200);
    assert_eq!(send(&hub, &bob, &ann.0, "m-before").await.code(), 200, "a retry of mail accepted before the block");

    let r = send(&hub, &bob, &ann.0, "m-after").await;
    assert_eq!(r.code(), 403, "{}", r.text());
    let j = r.json();
    assert_eq!(j["detail"], json!(format!("you have been blocked by @{} and cannot message them", ann.0)));
    assert_eq!(j["blocked_by"], json!(ann.0));
    println!("  ok  403 with the user's words and blocked_by");

    let c = sql_client(&url).await;
    let kept = c.query_one("SELECT count(*) FROM messages WHERE id = 'm-after'", &[]).await.unwrap().get::<_, i64>(0);
    assert_eq!(kept, 0, "a refused message is not stored");
    let pushes = c.query_one("SELECT count(*) FROM device_push WHERE slug = $1", &[&ann.0]).await.unwrap().get::<_, i64>(0);
    assert_eq!(pushes, 0);
    let a = sync_now(&hub, &ann, "ann-phone", None).await;
    let ids = message_ids(&a);
    assert!(ids.contains(&"m-before".to_string()) && !ids.contains(&"m-after".to_string()), "{ids:?}");
    println!("  ok  nothing stored; mail from before the block is still there");

    // other senders are not affected
    assert_eq!(send(&hub, &org, &ann.0, "m-org").await.code(), 200);

    // the other way round: the blocker cannot write to the one it blocked
    let r = send(&hub, &ann, &bob.0, "m-back").await;
    if mailhub::blocks::TWO_WAY {
        assert_eq!(r.code(), 403, "{}", r.text());
        assert_eq!(r.json()["blocking"], json!(bob.0));
        assert_eq!(r.json()["detail"], json!(format!("you blocked @{}; unblock them to message them", bob.0)));
        println!("  ok  two-way: the blocker is refused too");
    } else {
        assert_eq!(r.code(), 200, "{}", r.text());
        println!("  ok  one-way: the blocker may still write");
    }

    assert_eq!(block(&hub, &ann, &bob.0, false).await.code(), 200);
    assert_eq!(send(&hub, &bob, &ann.0, "m-lifted").await.code(), 200);
    assert_eq!(send(&hub, &ann, &bob.0, "m-back-2").await.code(), 200);
    println!("  ok  unblocked: both directions go through");
}

#[tokio::test]
async fn a_blocked_senders_heartbeats_are_not_relayed() {
    let (_pg, hub, _url) = test_hub("hubblocktyping").await;
    let ann = register(&hub, "ann", "person").await;
    let bob = register(&hub, "bob", "person").await;
    let cat = register(&hub, "cat", "person").await;
    assert_eq!(block(&hub, &ann, &bob.0, true).await.code(), 200);
    let first = sync_now(&hub, &ann, "ann-desk", None).await;
    for who in [&bob, &cat] {
        let r = post(&hub, who, "/api/typing", json!({ "to": [ann.0] })).await;
        assert_eq!(r.code(), 200, "{}", r.text());
    }
    let mut body = json!({ "device_id": "ann-desk", "wait": 0, "cursor": first["cursor"], "typing": true });
    body["typing"] = json!(true);
    let r = Call::new("POST", "/api/sync").auth(pair(&ann.0, &ann.1)).json(body).send(&hub).await;
    let from: Vec<String> = r.json()["typing"].as_array().unwrap().iter().map(|x| x["from"].as_str().unwrap().to_string()).collect();
    assert_eq!(from, vec![cat.0.clone()], "only the one not blocked");
    println!("  ok  the blocked sender's heartbeat is dropped");
}

#[tokio::test]
async fn every_device_of_the_blocker_is_told_the_list() {
    let (_pg, hub, _url) = test_hub("hubblocksync").await;
    let ann = register(&hub, "ann", "person").await;
    let bob = register(&hub, "bob", "person").await;

    let desk = sync_now(&hub, &ann, "ann-desk", None).await;
    assert_eq!(desk["blocks"], json!([]), "a first answer carries the list, empty: {desk}");
    let r = post(&hub, &ann, "/api/devices/active", json!({ "device_id": "ann-desk", "active": true })).await;
    assert_eq!(r.code(), 200, "{}", r.text());
    let desk = sync_now(&hub, &ann, "ann-desk", Some(&desk["cursor"])).await;

    // a parked sync of the desk (in use) answers when the phone blocks
    let (h, a, cur) = (hub.clone(), ann.clone(), desk["cursor"].clone());
    let parked = tokio::spawn(async move {
        let r = Call::new("POST", "/api/sync").auth(pair(&a.0, &a.1)).json(json!({ "device_id": "ann-desk", "wait": 20, "cursor": cur })).send(&h).await;
        (r.json(), Instant::now())
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    let t0 = Instant::now();
    assert_eq!(block(&hub, &ann, &bob.0, true).await.code(), 200);
    let (a, at) = tokio::time::timeout(Duration::from_secs(5), parked).await.expect("the parked sync never answered").unwrap();
    assert!(at.duration_since(t0) < Duration::from_secs(3), "answered after {:?}", at.duration_since(t0));
    assert_eq!(a["blocks"], json!([bob.0]), "{a}");
    println!("  ok  the device in use hears at once ({:?})", at.duration_since(t0));

    // a phone that syncs later is told too, and the next answer with nothing
    // new carries no list
    let phone = sync_now(&hub, &ann, "ann-phone", None).await;
    assert_eq!(phone["blocks"], json!([bob.0]));
    let again = sync_now(&hub, &ann, "ann-phone", Some(&phone["cursor"])).await;
    assert!(again.get("blocks").is_none(), "unchanged: not sent again: {again}");
    assert_eq!(block(&hub, &ann, &bob.0, false).await.code(), 200);
    let lifted = sync_now(&hub, &ann, "ann-phone", Some(&again["cursor"])).await;
    assert_eq!(lifted["blocks"], json!([]));
    println!("  ok  told once per change, on every device");

    // the one blocked is told who blocks them (user 2026-11-24 19:06Z: "Show
    // it in their DM"), and only that: never the blocker's other blocks
    assert_eq!(block(&hub, &ann, "someone.else", true).await.code(), 200);
    let b = sync_now(&hub, &bob, "bob-phone", None).await;
    assert_eq!(b["blocks"], json!([]));
    assert_eq!(b["blocked_by"], json!([]));
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
    assert_eq!(block(&hub, &ann, &bob.0, true).await.code(), 200);
    let (b, at) = tokio::time::timeout(Duration::from_secs(5), parked).await.expect("the blocked party's parked sync never answered").unwrap();
    assert!(at.duration_since(t0) < Duration::from_secs(3));
    assert_eq!(b["blocked_by"], json!([ann.0]), "{b}");
    assert_eq!(b["blocks"], json!([]), "{b}");
    println!("  ok  the blocked party's device in use hears at once ({:?})", at.duration_since(t0));
    assert_eq!(block(&hub, &ann, &bob.0, false).await.code(), 200);
    let b = sync_now(&hub, &bob, "bob-phone", Some(&b["cursor"])).await;
    assert_eq!(b["blocked_by"], json!([]), "lifted: {b}");
    println!("  ok  and hears when it is lifted");
}

#[tokio::test]
async fn a_block_belongs_to_its_owner_not_to_the_address() {
    let (_pg, hub, url) = test_hub("hubblockowner").await;
    let bob = register(&hub, "bob", "person").await;
    let slug = format!("org.owner.{}", token_hex3());
    let first = register_with(&hub, &slug, &token_hex16(), "org").await;
    assert_eq!(block(&hub, &first, &bob.0, true).await.code(), 200);

    // the same owner leaves and comes back: the block holds
    assert_eq!(post(&hub, &first, "/api/unregister", json!({})).await.code(), 200);
    let b = sync_now(&hub, &bob, "bob-phone", None).await;
    assert_eq!(b["blocked_by"], json!([]), "an address that left blocks nobody: {b}");
    register_with(&hub, &first.0, &first.1, "org").await;
    let b = sync_now(&hub, &bob, "bob-phone", Some(&b["cursor"])).await;
    assert_eq!(b["blocked_by"], json!([slug]), "back: {b}");
    assert_eq!(listed(&hub, &first).await, vec![bob.0.clone()]);
    assert_eq!(send(&hub, &bob, &slug, "m-1").await.code(), 403);
    println!("  ok  the same owner keeps its blocks");

    // someone else takes the freed address: it starts with none
    assert_eq!(post(&hub, &first, "/api/unregister", json!({})).await.code(), 200);
    let second = register_with(&hub, &slug, &token_hex16(), "org").await;
    assert_eq!(listed(&hub, &second).await, Vec::<String>::new());
    assert_eq!(send(&hub, &bob, &slug, "m-2").await.code(), 200);
    let c = sql_client(&url).await;
    let left = c.query_one("SELECT count(*) FROM blocks WHERE blocker = $1", &[&slug]).await.unwrap().get::<_, i64>(0);
    assert_eq!(left, 0, "the old owner's rows are gone");
    println!("  ok  a new owner inherits nothing");
}

#[tokio::test]
async fn an_address_may_block_at_most_the_limit() {
    let (_pg, hub, url) = test_hub("hubblockcap").await;
    let ann = register(&hub, "ann", "person").await;
    let c = sql_client(&url).await;
    let n = mailhub::blocks::MAX_BLOCKS as i64 - 1;
    c.execute(
        "INSERT INTO blocks (blocker, blocker_fp, blocked)
         SELECT $1, i.fingerprint, 'x' || g FROM identities i, generate_series(1, $2::bigint) g WHERE i.slug = $1",
        &[&ann.0, &n],
    )
    .await
    .unwrap();
    assert_eq!(block(&hub, &ann, "last.one", true).await.code(), 200, "the last place");
    let r = block(&hub, &ann, "one.more", true).await;
    assert_eq!(r.code(), 422, "{}", r.text());
    assert_eq!(block(&hub, &ann, "last.one", true).await.code(), 200, "an existing block is not a new one");
    assert_eq!(block(&hub, &ann, "x1", false).await.code(), 200);
    assert_eq!(block(&hub, &ann, "one.more", true).await.code(), 200, "a freed place");
    println!("  ok  {} blocks at most", mailhub::blocks::MAX_BLOCKS);
}

#[tokio::test]
async fn an_orgs_poll_carries_whom_it_blocks_and_who_blocks_it() {
    let (_pg, hub, _url) = test_hub("hubblockpoll").await;
    let org = register(&hub, "org", "org").await;
    let other = register(&hub, "acme", "org").await;
    let ann = register(&hub, "ann", "person").await;
    let poll = |who: Id| {
        let h = hub.clone();
        async move { Call::new("POST", "/api/poll?wait=0").auth(pair(&who.0, &who.1)).send(&h).await.json() }
    };
    let p = poll(org.clone()).await;
    assert!(p.get("blocking").is_none(), "nothing blocked: the answer is as before: {p}");
    assert_eq!(block(&hub, &org, &ann.0, true).await.code(), 200);
    assert_eq!(block(&hub, &ann, &org.0, true).await.code(), 200);
    let p = poll(org.clone()).await;
    assert_eq!(p["blocking"][&org.0], json!({ "blocks": [ann.0], "blocked_by": [ann.0] }), "{p}");
    // a poll for several orgs lists each one that has any, only its own
    let both = Call::new("POST", "/api/poll?wait=0").auth(format!("{} {}", pair(&org.0, &org.1), pair(&other.0, &other.1))).send(&hub).await.json();
    assert!(both["blocking"].get(&other.0).is_none(), "{both}");
    assert_eq!(both["blocking"][&org.0]["blocks"], json!([ann.0]));
    assert_eq!(block(&hub, &org, &ann.0, false).await.code(), 200);
    assert_eq!(block(&hub, &ann, &org.0, false).await.code(), 200);
    assert!(poll(org.clone()).await.get("blocking").is_none());
    println!("  ok  an org's poll carries blocks and blocked_by, left out when none");
}
