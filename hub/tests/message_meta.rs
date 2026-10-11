//! Message meta (docs/v2-additions.md): a client's small JSON object on a
//! message, stored as sent and handed back unchanged by poll, sync, history
//! and the operator view, only when set. Hubchat's question cards and
//! multi-replies travel in it.
//!
//!     HUB_TEST_PG=<folder> cargo test --test message_meta -- --nocapture

mod support;

use std::sync::Arc;

use mailhub::Hub;
use serde_json::{json, Value};
use support::*;

type Id = (String, String);

async fn register(hub: &Arc<Hub>, name: &str) -> Id {
    let id = (format!("{name}.meta.{}", token_hex3()), token_hex16());
    let r = Call::new("POST", "/api/register")
        .auth(pair(&id.0, &id.1))
        .json(json!({ "slug": id.0, "org_name": name, "username": "meta", "kind": "person" }))
        .send(hub)
        .await;
    assert_eq!(r.code(), 200, "{}", r.text());
    id
}

#[tokio::test]
async fn message_meta() {
    let pg = TestPg::start();
    let url = pg.fresh_db("hubmeta").await;
    let data = std::env::temp_dir().join(format!("orgtree-hub-meta-{}", token_hex16()));
    std::fs::create_dir_all(&data).unwrap();
    let hub = hub(&url, &data, &[]).await;

    let h = Call::new("GET", "/healthz").send(&hub).await.json();
    assert!(h["features"].as_array().unwrap().contains(&json!("meta")), "{h}");
    println!("  ok  /healthz lists the \"meta\" feature");

    let (ann, bob) = (register(&hub, "ann").await, register(&hub, "bob").await);
    // a device's first sync starts from now: both devices exist before the sends
    let sync = |who: Id, device: &'static str, cursor: Value| {
        let hub = hub.clone();
        async move {
            let r = Call::new("POST", "/api/sync").auth(pair(&who.0, &who.1)).json(json!({ "device_id": device, "cursor": cursor, "wait": 0 })).send(&hub).await;
            assert_eq!(r.code(), 200, "{}", r.text());
            r.json()
        }
    };
    let bob_cursor = sync(bob.clone(), "bob-phone", Value::Null).await["cursor"].clone();
    let ann_cursor = sync(ann.clone(), "ann-pc", Value::Null).await["cursor"].clone();
    let send = |body: Value| {
        let (hub, ann) = (hub.clone(), ann.clone());
        async move { Call::new("POST", "/api/send").auth(pair(&ann.0, &ann.1)).json(body).send(&hub).await }
    };
    let card = json!({ "question": { "options": ["Beta", "Stable"], "multiple": false }, "fallback": [31, 63], "x": [1, { "y": null }] });
    assert_eq!(send(json!({ "id": "card-1", "to": bob.0, "kind": "question", "body": "Which channel should rc4 go to?\n\nPick one:\n1. Beta\n2. Stable", "meta": card })).await.code(), 200);
    assert_eq!(send(json!({ "id": "plain-1", "to": bob.0, "body": "no meta here" })).await.code(), 200);
    assert_eq!(send(json!({ "id": "null-1", "to": bob.0, "body": "null meta", "meta": null })).await.code(), 200);
    let retry = send(json!({ "id": "card-1", "to": bob.0, "kind": "question", "body": "again", "meta": { "other": true } })).await;
    assert_eq!(retry.json()["duplicate"], true);

    // poll (v1's route): meta as sent, only on the message that has it
    let got = Call::new("POST", "/api/poll?wait=0").auth(pair(&bob.0, &bob.1)).send(&hub).await.json();
    let msgs = got["messages"].as_array().unwrap();
    let by_id = |id: &str| msgs.iter().find(|m| m["id"] == json!(id)).cloned().unwrap();
    assert_eq!(by_id("card-1")["meta"], card, "the meta changed on its way, or the retry's replaced it");
    for id in ["plain-1", "null-1"] {
        let keys: Vec<String> = by_id(id).as_object().unwrap().keys().cloned().collect();
        assert_eq!(keys, ["id", "from", "to", "body", "kind", "thread_id", "sent_at", "received_at", "attachments"], "{id}: v1's envelope changed");
    }
    println!("  ok  poll: meta returned unchanged and only when set; a retry keeps the first; null is no meta");

    // sync (v2): both sides' devices see it
    for (who, device, cursor) in [(&bob, "bob-phone", &bob_cursor), (&ann, "ann-pc", &ann_cursor)] {
        let changes = sync(who.clone(), device, cursor.clone()).await["changes"].as_array().unwrap().clone();
        let m = changes.iter().map(|c| &c["message"]).find(|m| m["id"] == "card-1").cloned().expect("card-1 in sync");
        assert_eq!(m["meta"], card, "{device}");
        assert!(changes.iter().map(|c| &c["message"]).find(|m| m["id"] == "plain-1").unwrap().get("meta").is_none());
    }
    println!("  ok  sync: the sender's and the recipient's devices get the meta");

    // history (v2) and the operator view
    let hist = Call::new("GET", &format!("/api/history?with={}", ann.0)).auth(pair(&bob.0, &bob.1)).send(&hub).await;
    assert_eq!(hist.code(), 200, "{}", hist.text());
    let hm = hist.json()["messages"].as_array().unwrap().iter().find(|m| m["id"] == "card-1").cloned().expect("card-1 in history");
    assert_eq!(hm["meta"], card);
    let ui = Call::new("GET", &format!("/ui/messages?org={}", bob.0)).send(&hub).await.json();
    assert_eq!(ui["messages"].as_array().unwrap().iter().find(|m| m["id"] == "card-1").unwrap()["meta"], card);
    println!("  ok  history and the operator view carry it too");

    // refusals: not an object, too big, a NUL; nothing stored
    let big = json!({ "pad": "x".repeat(4096) });
    for (value, detail) in [
        (json!(5), "meta must be a JSON object"),
        (json!(["a"]), "meta must be a JSON object"),
        (json!("text"), "meta must be a JSON object"),
        (big, "meta is larger than 4096 bytes"),
        (json!({ "a": "x\u{0}y" }), "meta contains a NUL character"),
    ] {
        let r = send(json!({ "id": "bad-meta", "to": bob.0, "body": "x", "meta": value })).await;
        assert_eq!((r.code(), r.json()["detail"].clone()), (422, json!(detail)));
    }
    let fits = json!({ "pad": "x".repeat(4096 - 10) });
    assert_eq!(serde_json::to_string(&fits).unwrap().len(), 4096);
    assert_eq!(send(json!({ "id": "fits-1", "to": bob.0, "body": "x", "meta": fits })).await.code(), 200);
    let again = Call::new("POST", "/api/poll?wait=0").auth(pair(&bob.0, &bob.1)).send(&hub).await.json();
    let ids: Vec<&str> = again["messages"].as_array().unwrap().iter().map(|m| m["id"].as_str().unwrap()).collect();
    assert!(!ids.contains(&"bad-meta"), "a refused send was stored");
    println!("  ok  refusals: not an object, over 4096 bytes, a NUL character; exactly 4096 bytes is fine");

    let _ = std::fs::remove_dir_all(&data);
}
