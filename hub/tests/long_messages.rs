//! Phase 2, slice 4: long messages arrive whole (G6), and the hub's limit
//! bounds a whole message, body and files together (G8).
//!
//!     HUB_TEST_PG=<folder> cargo test --test long_messages -- --nocapture

mod support;

use std::sync::Arc;

use mailhub::api::mail::continues_line;
use mailhub::Hub;
use serde_json::{json, Value};
use support::*;

type Id = (String, String);

async fn register(hub: &Arc<Hub>, name: &str) -> Id {
    let id = (format!("{name}.long.{}", token_hex3()), token_hex16());
    let r = Call::new("POST", "/api/register")
        .auth(pair(&id.0, &id.1))
        .json(json!({ "slug": id.0, "org_name": name, "username": "long", "kind": "person" }))
        .send(hub)
        .await;
    assert_eq!(r.code(), 200, "{}", r.text());
    id
}

async fn send(hub: &Arc<Hub>, from: &Id, body: Value) -> Resp {
    Call::new("POST", "/api/send").auth(pair(&from.0, &from.1)).json(body).send(hub).await
}

async fn upload(hub: &Arc<Hub>, who: &Id, bytes: Vec<u8>) -> String {
    let r = Call::new("POST", "/api/attachments?name=body.txt").auth(pair(&who.0, &who.1)).content(bytes).send(hub).await;
    assert_eq!(r.code(), 200, "{}", r.text());
    r.json()["id"].as_str().unwrap().to_string()
}

async fn poll_body(hub: &Arc<Hub>, who: &Id, id: &str) -> String {
    let p = Call::new("POST", "/api/poll?wait=0").auth(pair(&who.0, &who.1)).send(hub).await.json();
    let m = p["messages"].as_array().unwrap().iter().find(|m| m["id"] == json!(id)).cloned().unwrap_or_else(|| panic!("{id} not polled"));
    m["body"].as_str().unwrap().to_string()
}

async fn synced(hub: &Arc<Hub>, who: &Id, id: &str) -> Value {
    let r = Call::new("POST", "/api/sync").auth(pair(&who.0, &who.1)).json(json!({ "device_id": "d1", "wait": 0 })).send(hub).await;
    let mut cursor = r.json()["cursor"].clone();
    let mut found = r.json()["changes"].as_array().unwrap().iter().find(|c| c["message"]["id"] == json!(id)).cloned();
    while found.is_none() && r.json()["more"] == json!(true) {
        let r = Call::new("POST", "/api/sync").auth(pair(&who.0, &who.1)).json(json!({ "device_id": "d1", "wait": 0, "cursor": cursor })).send(hub).await;
        cursor = r.json()["cursor"].clone();
        found = r.json()["changes"].as_array().unwrap().iter().find(|c| c["message"]["id"] == json!(id)).cloned();
    }
    found.unwrap_or_else(|| panic!("{id} not synced"))["message"].clone()
}

fn blob_count(data: &std::path::Path) -> usize {
    std::fs::read_dir(data.join("blobs")).map(|d| d.count()).unwrap_or(0)
}

#[tokio::test]
async fn long_messages_arrive_whole() {
    let pg = TestPg::start();
    let url = pg.fresh_db("hublong").await;
    let data = std::env::temp_dir().join(format!("orgtree-hub-long-{}", token_hex16()));
    std::fs::create_dir_all(&data).unwrap();
    let limit_file = data.join("limit.json");
    let hub = hub(&url, &data, &[("HUB_RETENTION_DAYS", ""), ("HUB_RUNTIME_CONFIG_FILE", limit_file.to_str().unwrap())]).await;
    let sql = sql_client(&url).await;
    let (ann, bob, carl) = (register(&hub, "ann").await, register(&hub, "bob").await, register(&hub, "carl").await);

    let h = Call::new("GET", "/healthz").send(&hub).await.json();
    assert_eq!((h["max_message_bytes"].clone(), h["max_attachment_bytes"].clone()), (json!(1u64 << 30), json!(1u64 << 30)));
    for f in ["long_messages", "message_limit"] {
        assert!(h["features"].as_array().unwrap().contains(&json!(f)), "{h}");
    }

    // ------------------------------------------- 30,000 characters: in its row
    let medium = "abcdefghi\n".repeat(3000);
    assert_eq!(send(&hub, &bob, json!({ "id": "medium", "to": ann.0, "body": medium })).await.code(), 200);
    let m = synced(&hub, &ann, "medium").await;
    assert_eq!((m["body"].as_str().unwrap(), m.get("body_bytes")), (medium.as_str(), None));
    let hist = Call::new("GET", &format!("/api/history?with={}", bob.0)).auth(pair(&ann.0, &ann.1)).send(&hub).await.json();
    assert_eq!(hist["messages"][0]["body"].as_str().unwrap(), medium);
    let cut = format!("{}{}", &medium[..20000], continues_line(30000));
    assert_eq!(poll_body(&hub, &ann, "medium").await, cut);
    let ui = Call::new("GET", &format!("/ui/messages?org={}", ann.0)).send(&hub).await.json();
    assert_eq!(ui["messages"][0]["body"].as_str().unwrap(), cut);
    let r = Call::new("GET", "/api/messages/medium/body").auth(pair(&ann.0, &ann.1)).send(&hub).await;
    assert_eq!((r.code(), r.headers["content-type"].to_str().unwrap()), (200, "text/plain; charset=utf-8"));
    assert_eq!(r.text(), medium);
    println!("  ok  30,000 characters: whole through sync, history and the body route; v1's poll and the page show 20,000 and say it continues");

    // ------------------------------------------- 5 MB: in a file, byte-identical
    let mut big = String::new();
    let mut i = 0;
    while big.len() < 5_000_000 {
        big.push_str(&format!("Grüße ✓ 漢字 😀 line {i}\n"));
        i += 1;
    }
    let files_before = blob_count(&data);
    let r = send(&hub, &ann, json!({ "id": "big", "to": bob.0, "body": big, "reply_to": "medium" })).await;
    assert_eq!(r.code(), 200, "{}", r.text());
    let row = sql.query_one("SELECT body, body_bytes, body_part FROM messages WHERE id = 'big'", &[]).await.unwrap();
    let preview: String = big.chars().take(20000).collect();
    assert_eq!((row.get::<_, String>(0), row.get::<_, Option<i64>>(1)), (preview.clone(), Some(big.len() as i64)));
    let part: String = row.get::<_, Option<String>>(2).expect("a body file");
    assert_eq!(std::fs::read(data.join("blobs").join(&part)).unwrap(), big.as_bytes());
    assert_eq!(blob_count(&data), files_before + 1);
    assert_eq!(poll_body(&hub, &bob, "big").await, format!("{preview}{}", continues_line(big.len() as i64)));
    let m = synced(&hub, &bob, "big").await;
    assert_eq!((m["body"].as_str().unwrap(), m["body_bytes"].clone(), m["reply_to"].clone()), (preview.as_str(), json!(big.len()), json!("medium")));
    assert_eq!(m["attachments"], json!([]), "the body showed up as an attachment");
    let hist = Call::new("GET", &format!("/api/history?with={}", ann.0)).auth(pair(&bob.0, &bob.1)).send(&hub).await.json();
    assert_eq!(hist["messages"][0]["body_bytes"], json!(big.len()));
    let r = Call::new("GET", "/api/messages/big/body").auth(pair(&bob.0, &bob.1)).send(&hub).await;
    assert_eq!((r.code(), r.headers["content-type"].to_str().unwrap()), (200, "text/plain; charset=utf-8"));
    assert!(r.body.as_ref() == big.as_bytes(), "the body came back changed");
    let r = Call::new("GET", "/api/messages/big/body").auth(pair(&bob.0, &bob.1)).header("range", "bytes=0-99").send(&hub).await;
    assert_eq!((r.code(), r.body.as_ref()), (206, &big.as_bytes()[..100]));
    assert_eq!(Call::new("GET", "/api/messages/big/body").auth(pair(&ann.0, &ann.1)).send(&hub).await.code(), 200);
    assert_eq!(Call::new("GET", "/api/messages/big/body").auth(pair(&carl.0, &carl.1)).send(&hub).await.code(), 404);
    let p = Call::new("POST", "/api/poll?wait=0").auth(pair(&bob.0, &bob.1)).send(&hub).await;
    assert!(p.body.len() < 200_000, "a poll carried {} bytes", p.body.len());
    println!("  ok  5 MB ({} bytes): byte-identical through the body route (ranges too); poll, sync and history carry 20,000 characters and the size", big.len());

    // ----------------------------------------- a retry writes nothing lasting
    let files_before = blob_count(&data);
    let r = send(&hub, &ann, json!({ "id": "big", "to": bob.0, "body": big.replace('✓', "x") })).await;
    assert_eq!(r.json()["duplicate"], true);
    assert_eq!(blob_count(&data), files_before, "a duplicate's body file stayed");
    println!("  ok  a duplicate send of a long body answers duplicate and leaves no file behind");

    // ------------------------------------------- a body uploaded as a file
    let text = "uploaded ✓ text\n".repeat(150_000).into_bytes();
    let part = upload(&hub, &ann, text.clone()).await;
    let r = send(&hub, &ann, json!({ "id": "from-upload", "to": bob.0, "body_part": part })).await;
    assert_eq!(r.code(), 200, "{}", r.text());
    let r = Call::new("GET", "/api/messages/from-upload/body").auth(pair(&bob.0, &bob.1)).send(&hub).await;
    assert!(r.code() == 200 && r.body.as_ref() == text.as_slice());
    assert_eq!(synced(&hub, &bob, "from-upload").await["body_bytes"], json!(text.len()));
    let refusals = [
        (json!({ "to": bob.0, "body_part": part }), format!("body_part '{part}' already bound")),
        (json!({ "to": bob.0, "body_part": upload(&hub, &carl, b"carl's".to_vec()).await }), "unknown body_part".to_string()),
        (json!({ "to": bob.0, "body_part": upload(&hub, &ann, vec![b'a', 0xff, b'b']).await }), "body_part is not UTF-8 text".to_string()),
        (json!({ "to": bob.0, "body_part": upload(&hub, &ann, b"a\0b".to_vec()).await }), "body_part contains a NUL character".to_string()),
        (json!({ "to": bob.0, "body": "both", "body_part": upload(&hub, &ann, b"x".to_vec()).await }), "give body or body_part, not both".to_string()),
        (json!({ "to": bob.0, "body_part": 5 }), "body_part must be an upload id (a string)".to_string()),
    ];
    for (body, detail) in refusals {
        let r = send(&hub, &ann, body.clone()).await;
        assert_eq!(r.code(), 422, "{body}: {}", r.text());
        assert!(r.json()["detail"].as_str().unwrap().starts_with(&detail), "{}", r.text());
    }
    println!("  ok  a body uploaded first (body_part) arrives whole; someone else's, a bound one, non-UTF-8 or NUL text are refused");

    // ------------------------------------ the body file goes with the last copy
    let part_of_big = part_of(&sql, "big").await;
    assert_eq!(Call::new("DELETE", "/api/messages/big").auth(pair(&ann.0, &ann.1)).send(&hub).await.code(), 200);
    assert!(data.join("blobs").join(&part_of_big).exists(), "the recipient's copy lost its body");
    assert_eq!(Call::new("GET", "/api/messages/big/body").auth(pair(&bob.0, &bob.1)).send(&hub).await.code(), 200);
    assert_eq!(Call::new("GET", "/api/messages/big/body").auth(pair(&ann.0, &ann.1)).send(&hub).await.code(), 404);
    assert_eq!(Call::new("DELETE", "/api/messages/big").auth(pair(&bob.0, &bob.1)).send(&hub).await.code(), 200);
    assert!(!data.join("blobs").join(&part_of_big).exists(), "the body file outlived the message");
    println!("  ok  a long body stays while either side has the message, and goes with the last copy");

    // ------------------------------- G8: one limit for body and files together
    std::fs::write(&limit_file, r#"{"max_attachment_bytes": 100000}"#).unwrap();
    let h = Call::new("GET", "/healthz").send(&hub).await.json();
    assert_eq!((h["max_message_bytes"].clone(), h["max_attachment_bytes"].clone()), (json!(100000), json!(100000)));
    let file = upload(&hub, &ann, vec![b'f'; 50_000]).await;
    let r = send(&hub, &ann, json!({ "to": bob.0, "body": "b".repeat(60_000), "attachments": [file] })).await;
    assert_eq!(r.code(), 413, "{}", r.text());
    assert_eq!(r.json()["max_message_bytes"], 100000);
    assert!(r.json()["detail"].as_str().unwrap().contains("110000"), "{}", r.text());
    assert_eq!(send(&hub, &ann, json!({ "to": bob.0, "body": "b".repeat(50_000), "attachments": [file] })).await.code(), 200);
    assert_eq!(send(&hub, &ann, json!({ "id": "at-limit", "to": bob.0, "body": "b".repeat(100_000) })).await.code(), 200);
    assert_eq!(send(&hub, &ann, json!({ "to": bob.0, "body": "b".repeat(100_001) })).await.code(), 413);
    let (f1, f2) = (upload(&hub, &ann, vec![b'f'; 50_001]).await, upload(&hub, &ann, vec![b'f'; 50_000]).await);
    assert_eq!(send(&hub, &ann, json!({ "to": bob.0, "body": "", "attachments": [f1, f2] })).await.code(), 413);
    std::fs::write(&limit_file, r#"{"max_attachment_bytes": 1000}"#).unwrap();
    let r = send(&hub, &ann, json!({ "id": "at-limit", "to": bob.0, "body": "b".repeat(100_000) })).await;
    assert_eq!((r.code(), r.json()["duplicate"].clone()), (200, json!(true)), "a retry was judged against the new limit");
    std::fs::write(&limit_file, "not json").unwrap();
    assert_eq!(send(&hub, &ann, json!({ "to": bob.0, "body": "x" })).await.code(), 503);
    println!("  ok  the limit bounds body and files together (413 names it), follows live changes, and a retry of accepted mail stays a duplicate");

    let _ = std::fs::remove_dir_all(&data);
    drop(pg);
}

async fn part_of(sql: &tokio_postgres::Client, id: &str) -> String {
    sql.query_one("SELECT body_part FROM messages WHERE id = $1", &[&id]).await.unwrap().get::<_, Option<String>>(0).unwrap()
}
