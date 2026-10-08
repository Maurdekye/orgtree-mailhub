//! Phase 2, slice 1: the person kind and profiles (G2), inline replies (G3).
//!
//!     HUB_TEST_PG=<folder> cargo test --test people_and_replies -- --nocapture

mod support;

use std::sync::Arc;

use mailhub::Hub;
use serde_json::{json, Value};
use support::*;

type Org = (String, String);

async fn register(hub: &Arc<Hub>, o: &Org, extra: Value) -> Resp {
    let mut body = json!({ "slug": o.0, "org_name": "Name", "username": "people", "blurb": "about" });
    if let Value::Object(e) = extra {
        for (k, v) in e {
            body[k] = v;
        }
    }
    Call::new("POST", "/api/register").auth(pair(&o.0, &o.1)).json(body).send(hub).await
}

fn new_id(name: &str) -> Org {
    (format!("{name}.people.{}", token_hex3()), token_hex16())
}

fn row<'a>(roster: &'a Value, slug: &str) -> &'a Value {
    roster.as_array().unwrap().iter().find(|r| r["slug"] == json!(slug)).expect("in the roster")
}

async fn profile(hub: &Arc<Hub>, auth: &str, body: Value) -> Resp {
    Call::new("POST", "/api/profile").auth(auth).json(body).send(hub).await
}

#[tokio::test]
async fn people_and_replies() {
    let pg = TestPg::start();
    let url = pg.fresh_db("hubpeople").await;
    let data = std::env::temp_dir().join(format!("orgtree-hub-people-{}", token_hex16()));
    std::fs::create_dir_all(&data).unwrap();
    let hub = hub(&url, &data, &[]).await;

    // ------------------------------------------------------------- G2 kinds
    let ann = new_id("ann");
    let r = register(&hub, &ann, json!({ "kind": "person" })).await;
    assert_eq!(r.code(), 200, "{}", r.text());
    assert_eq!(row(&r.json()["roster"], &ann.0)["kind"], "person");
    let again = register(&hub, &ann, json!({ "kind": "chat" })).await;
    assert_eq!(row(&again.json()["roster"], &ann.0)["kind"], "person", "the kind changed on re-registration");
    for (asked, stored) in [("chat", "chat"), ("PERSON", "org"), ("people", "org"), ("org", "org")] {
        let o = new_id("k");
        let r = register(&hub, &o, json!({ "kind": asked })).await;
        assert_eq!(row(&r.json()["roster"], &o.0)["kind"], stored, "kind {asked:?}");
    }
    // the one change of kind: a chat that registers again as a person (a
    // Hubchat person registered as a chat on a v1 hub). A device syncing
    // elsewhere sees the change; nothing else changes kind.
    let watcher = new_id("watcher");
    register(&hub, &watcher, json!({ "kind": "org" })).await;
    let sync = |cursor: Value| {
        let hub = hub.clone();
        let auth = pair(&watcher.0, &watcher.1);
        async move { Call::new("POST", "/api/sync").auth(auth).json(json!({ "device_id": "w1", "cursor": cursor, "wait": 0 })).send(&hub).await.json() }
    };
    let mut cursor = Value::Null;
    loop {
        let s = sync(cursor.clone()).await;
        cursor = s["cursor"].clone();
        if s["more"] != json!(true) {
            break;
        }
    }
    let pat = new_id("pat");
    assert_eq!(row(&register(&hub, &pat, json!({ "kind": "chat" })).await.json()["roster"], &pat.0)["kind"], "chat");
    let s = sync(cursor.clone()).await;
    cursor = s["cursor"].clone();
    let up = register(&hub, &pat, json!({ "kind": "person" })).await;
    assert_eq!(up.code(), 200, "{}", up.text());
    assert_eq!(row(&up.json()["roster"], &pat.0)["kind"], "person", "a chat did not become a person");
    let seen = sync(cursor).await;
    assert_eq!(row(&seen["roster"], &pat.0)["kind"], "person", "a syncing device missed the change: {seen}");
    assert_eq!(row(&register(&hub, &pat, json!({ "kind": "chat" })).await.json()["roster"], &pat.0)["kind"], "person", "a person became a chat");
    for (first, then) in [("org", "person"), ("org", "chat"), ("chat", "org"), ("person", "org")] {
        let o = new_id("k2");
        register(&hub, &o, json!({ "kind": first })).await;
        let r = register(&hub, &o, json!({ "kind": then })).await;
        assert_eq!(row(&r.json()["roster"], &o.0)["kind"], first, "{first} became {then}");
    }
    let ui = Call::new("GET", "/ui/data").send(&hub).await.json();
    assert_eq!(row(&ui["orgs"], &ann.0)["kind"], "person");
    let page = Call::new("GET", "/").send(&hub).await.text();
    assert!(page.contains("k-person"), "the hub's page does not style the person kind");
    println!("  ok  person kind: registered, fixed at the first registration but for chat -> person (seen by sync), shown in roster, /ui/data and the page");

    // ----------------------------------------------------------- G2 profile
    let bob = new_id("bob");
    register(&hub, &bob, json!({})).await;
    let r = profile(&hub, &pair(&ann.0, &ann.1), json!({ "name": "  Ann Example  " })).await;
    assert_eq!(r.code(), 200, "{}", r.text());
    let p = &r.json()["profile"];
    assert_eq!(p["org_name"], "Ann Example");
    assert_eq!(p["blurb"], "about", "an omitted field changed");
    assert_eq!((p["slug"].clone(), p["kind"].clone(), p["username"].clone()), (json!(ann.0), json!("person"), json!("people")));
    let r = profile(&hub, &pair(&ann.0, &ann.1), json!({ "about": "on call this week" })).await;
    assert_eq!(r.json()["profile"]["blurb"], "on call this week");
    let r = profile(&hub, &pair(&ann.0, &ann.1), json!({ "name": null, "org_name": "Ann N." })).await;
    assert_eq!(r.json()["profile"]["org_name"], "Ann N.", "a null name hid org_name");
    let r = profile(&hub, &pair(&ann.0, &ann.1), json!({ "org_name": "Ann E.", "blurb": "roster spellings" })).await;
    assert_eq!((r.json()["profile"]["org_name"].clone(), r.json()["profile"]["blurb"].clone()), (json!("Ann E."), json!("roster spellings")));
    // another client sees it at its next poll
    let seen = Call::new("POST", "/api/poll?wait=0").auth(pair(&bob.0, &bob.1)).send(&hub).await.json();
    assert_eq!(row(&seen["roster"], &ann.0)["org_name"], "Ann E.");
    println!("  ok  profile: name and about edited (roster spellings too), the rest kept, seen in another client's next poll");

    let max_name = "n".repeat(48);
    let max_about = "a".repeat(200);
    assert_eq!(profile(&hub, &pair(&ann.0, &ann.1), json!({ "name": max_name, "about": max_about })).await.code(), 200);
    for (body, detail) in [
        (json!({ "name": "n".repeat(49) }), "name is longer than 48 characters"),
        (json!({ "about": "a".repeat(201) }), "about is longer than 200 characters"),
        (json!({ "name": 5 }), "name must be a string"),
        (json!({}), "nothing to update: give name and/or about"),
        (json!({ "name": null }), "nothing to update: give name and/or about"),
    ] {
        let r = profile(&hub, &pair(&ann.0, &ann.1), body).await;
        assert_eq!((r.code(), r.json()["detail"].clone()), (422, json!(detail)));
    }
    let r = Call::new("POST", "/api/profile").auth(pair(&ann.0, &ann.1)).content("not json").send(&hub).await;
    assert_eq!((r.code(), r.json()["detail"].clone()), (400, json!("the request body must be a JSON object")));
    assert_eq!(profile(&hub, "", json!({ "name": "x" })).await.code(), 401);
    assert_eq!(profile(&hub, &pair(&ann.0, "wrong"), json!({ "name": "x" })).await.code(), 401);
    let both = format!("{} {}", pair(&ann.0, &ann.1), pair(&bob.0, &bob.1));
    let r = profile(&hub, &both, json!({ "name": "who?" })).await;
    assert_eq!(r.code(), 422);
    let r = profile(&hub, &both, json!({ "slug": bob.0, "name": "Bob" })).await;
    assert_eq!(r.json()["profile"]["org_name"], "Bob");
    let r = profile(&hub, &pair(&ann.0, &ann.1), json!({ "slug": bob.0, "name": "hijack" })).await;
    assert_eq!(r.code(), 401, "an address edited another's profile");
    let r = profile(&hub, &pair(&ann.0, &ann.1), json!({ "name": "Ann\u{0}" })).await;
    assert_eq!(r.json()["profile"]["org_name"], "Ann", "NUL kept");
    println!("  ok  profile limits (48/200), refusals (type, empty, malformed, no or wrong credentials, ambiguous, another's address)");

    // -------------------------------------------------------------- G3 reply
    let send = |body: Value| {
        let (hub, ann) = (hub.clone(), ann.clone());
        async move { Call::new("POST", "/api/send").auth(pair(&ann.0, &ann.1)).json(body).send(&hub).await }
    };
    assert_eq!(send(json!({ "id": "orig-1", "to": bob.0, "body": "question?" })).await.code(), 200);
    assert_eq!(send(json!({ "id": "reply-1", "to": bob.0, "body": "follow-up", "reply_to": "orig-1" })).await.code(), 200);
    assert_eq!(send(json!({ "id": "reply-2", "to": bob.0, "body": "to the void", "reply_to": "no-such-message" })).await.code(), 200);
    assert_eq!(send(json!({ "id": "reply-1", "to": bob.0, "body": "retry", "reply_to": "something-else" })).await.json()["duplicate"], true);
    let got = Call::new("POST", "/api/poll?wait=0").auth(pair(&bob.0, &bob.1)).send(&hub).await.json();
    let msgs = got["messages"].as_array().unwrap();
    let by_id = |id: &str| msgs.iter().find(|m| m["id"] == json!(id)).cloned().unwrap();
    let orig = by_id("orig-1");
    let plain: Vec<&str> = orig.as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(plain, ["id", "from", "to", "body", "kind", "thread_id", "sent_at", "received_at", "attachments"], "a message without reply_to changed v1's envelope");
    let reply = by_id("reply-1");
    let keys: Vec<&str> = reply.as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(keys.last().copied(), Some("reply_to"));
    assert_eq!(reply["reply_to"], "orig-1", "the retry's reply_to replaced the first payload's");
    assert_eq!(by_id("reply-2")["reply_to"], "no-such-message");
    let ui = Call::new("GET", &format!("/ui/messages?org={}", bob.0)).send(&hub).await.json();
    let in_ui = ui["messages"].as_array().unwrap().iter().find(|m| m["id"] == "reply-1").cloned().unwrap();
    assert_eq!(in_ui["reply_to"], "orig-1");
    for (value, detail) in [
        (json!(5), "reply_to must be a message id (a string)"),
        (json!({ "id": "x" }), "reply_to must be a message id (a string)"),
        (json!("r".repeat(4097)), "reply_to is longer than 4096 characters"),
        (json!("a\u{0}b"), "reply_to contains a NUL character"),
    ] {
        let r = send(json!({ "to": bob.0, "body": "x", "reply_to": value })).await;
        assert_eq!((r.code(), r.json()["detail"].clone()), (422, json!(detail)));
    }
    assert_eq!(send(json!({ "to": bob.0, "body": "x", "reply_to": null })).await.code(), 200);
    println!("  ok  reply_to: stored as given (even for an unknown message), returned in poll and the operator view only when set, first payload wins, bad values refused");

    let _ = std::fs::remove_dir_all(&data);
    drop(pg);
}
