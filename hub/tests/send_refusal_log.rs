//! Refused sends are diagnosable from the hub's own log: a 4xx answer to
//! POST /api/send appends the refusal's `detail` (the same text the sender
//! is told) and the recipient slug (`to`) to that request's one log line.
//! Nothing else is added: no body, no attachment contents, no credentials,
//! and the response contract is unchanged.
//!
//!     HUB_TEST_PG=<folder> cargo test --test send_refusal_log -- --nocapture

mod support;

use std::sync::Arc;

use mailhub::Hub;
use serde_json::{json, Value};
use support::*;

type Id = (String, String);

async fn register(hub: &Arc<Hub>, name: &str) -> Id {
    let id = (format!("{name}.reflog.{}", token_hex3()), token_hex16());
    let r = Call::new("POST", "/api/register")
        .auth(pair(&id.0, &id.1))
        .json(json!({ "slug": id.0, "org_name": name, "username": "reflog", "kind": "person" }))
        .send(hub)
        .await;
    assert_eq!(r.code(), 200, "{}", r.text());
    id
}

/// The captured log lines for one path and status, parsed.
fn logged(status: u16) -> Vec<Value> {
    mailhub::log::captured_lines()
        .iter()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| v["path"] == json!("/api/send") && v["status"] == json!(status))
        .collect()
}

#[tokio::test]
async fn refused_send_logs_reason() {
    let pg = TestPg::start();
    let url = pg.fresh_db("hubreflog").await;
    let data = std::env::temp_dir().join(format!("orgtree-hub-reflog-{}", token_hex16()));
    std::fs::create_dir_all(&data).unwrap();
    let hub = hub(&url, &data, &[]).await;

    mailhub::log::arm_capture();
    let (ann, bob) = (register(&hub, "ann").await, register(&hub, "bob").await);
    let ghost = format!("ghost.reflog.{}", token_hex3());
    let secret_body = "a body that must never reach the log";

    // a send to an org that is not registered: refused 422, as v1 answered
    let r = Call::new("POST", "/api/send")
        .auth(pair(&ann.0, &ann.1))
        .json(json!({ "id": "stuck-1", "to": ghost, "body": secret_body }))
        .send(&hub)
        .await;
    assert_eq!(r.code(), 422, "{}", r.text());
    // the response contract is unchanged: one `detail` string, nothing else
    let resp = r.json();
    assert_eq!(resp.as_object().unwrap().len(), 1, "{resp}");
    let detail = resp["detail"].as_str().expect("detail is a string").to_string();
    assert!(detail.contains(&ghost), "{detail}");

    // that request's one log line carries the reason and the recipient
    let refused = logged(422);
    assert_eq!(refused.len(), 1, "{refused:?}");
    assert_eq!(refused[0]["to"], json!(ghost), "{}", refused[0]);
    assert_eq!(refused[0]["detail"], json!(detail), "{}", refused[0]);
    assert_eq!(refused[0]["slugs"], json!([ann.0]), "{}", refused[0]);
    for line in mailhub::log::captured_lines() {
        assert!(!line.contains(secret_body), "a body reached the log: {line}");
    }
    println!("  ok  refused send: the log line names the reason and the recipient, never the body");

    // another refusal site (a non-object meta) is logged the same way
    let r = Call::new("POST", "/api/send")
        .auth(pair(&ann.0, &ann.1))
        .json(json!({ "id": "bad-meta-1", "to": bob.0, "body": secret_body, "meta": 5 }))
        .send(&hub)
        .await;
    assert_eq!(r.code(), 422, "{}", r.text());
    let refused = logged(422);
    assert_eq!(refused.len(), 2, "{refused:?}");
    assert_eq!(refused[1]["to"], json!(bob.0), "{}", refused[1]);
    assert_eq!(refused[1]["detail"], json!("meta must be a JSON object"), "{}", refused[1]);
    for line in mailhub::log::captured_lines() {
        assert!(!line.contains(secret_body), "a body reached the log: {line}");
    }
    println!("  ok  a second refusal site (meta) logs its own reason and recipient");

    // an over-long recipient (named before auth, so sender-chosen) is cut
    // to 256 chars with an ellipsis, in both the `to` and the `detail`
    let long_ghost = format!("{}.reflog.{}", "g".repeat(300), token_hex3());
    let r = Call::new("POST", "/api/send")
        .auth(pair(&ann.0, &ann.1))
        .json(json!({ "id": "stuck-2", "to": long_ghost, "body": secret_body }))
        .send(&hub)
        .await;
    assert_eq!(r.code(), 422, "{}", r.text());
    let refused = logged(422);
    assert_eq!(refused.len(), 3, "{refused:?}");
    for key in ["to", "detail"] {
        let v = refused[2][key].as_str().expect("a logged string");
        assert_eq!(v.chars().count(), 256, "{key}: {v}");
        assert!(v.ends_with('\u{2026}'), "{key}: {v}");
    }
    assert!(refused[2]["detail"].as_str().unwrap().contains("no org registered as"), "{}", refused[2]);
    println!("  ok  an over-long recipient is capped at 256 chars with an ellipsis");

    // a send that goes through keeps the v1 log line: no reason appended
    let r = Call::new("POST", "/api/send")
        .auth(pair(&ann.0, &ann.1))
        .json(json!({ "id": "fine-1", "to": bob.0, "body": "hello" }))
        .send(&hub)
        .await;
    assert_eq!(r.code(), 200, "{}", r.text());
    let sent = logged(200);
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert!(sent[0].get("detail").is_none() && sent[0].get("to").is_none(), "{}", sent[0]);
    println!("  ok  a successful send's log line is unchanged");

    let _ = std::fs::remove_dir_all(&data);
}
