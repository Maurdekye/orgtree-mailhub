//! Phase 2, slice 3: history kept until deleted (G4) — conversations,
//! history, per-copy deletes, and what retention means now.
//!
//!     HUB_TEST_PG=<folder> cargo test --test history -- --nocapture

mod support;

use std::collections::HashSet;
use std::sync::Arc;

use mailhub::Hub;
use serde_json::{json, Value};
use support::*;

type Id = (String, String);

async fn register(hub: &Arc<Hub>, name: &str) -> (Id, Value) {
    let id = (format!("{name}.hist.{}", token_hex3()), token_hex16());
    let r = Call::new("POST", "/api/register")
        .auth(pair(&id.0, &id.1))
        .json(json!({ "slug": id.0, "org_name": name, "username": "hist", "kind": "person" }))
        .send(hub)
        .await;
    assert_eq!(r.code(), 200, "{}", r.text());
    (id, r.json())
}

async fn send(hub: &Arc<Hub>, from: &Id, to: &str, id: &str, extra: Value) -> Value {
    let mut body = json!({ "id": id, "to": to, "body": format!("body of {id}") });
    if let Value::Object(e) = extra {
        for (k, v) in e {
            body[k] = v;
        }
    }
    let r = Call::new("POST", "/api/send").auth(pair(&from.0, &from.1)).json(body).send(hub).await;
    assert_eq!(r.code(), 200, "{}", r.text());
    r.json()
}

async fn get(hub: &Arc<Hub>, who: &Id, path: &str) -> Resp {
    Call::new("GET", path).auth(pair(&who.0, &who.1)).send(hub).await
}

async fn delete(hub: &Arc<Hub>, who: &Id, path: &str) -> Resp {
    Call::new("DELETE", path).auth(pair(&who.0, &who.1)).send(hub).await
}

/// Every page of a conversation, newest first.
async fn whole_history(hub: &Arc<Hub>, who: &Id, with: &str, limit: usize) -> Vec<Value> {
    let mut out = Vec::new();
    let mut before = String::new();
    loop {
        let r = get(hub, who, &format!("/api/history?with={with}&limit={limit}&before={before}")).await;
        assert_eq!(r.code(), 200, "{}", r.text());
        let j = r.json();
        let page = j["messages"].as_array().unwrap().clone();
        assert!(page.len() <= limit);
        out.extend(page);
        match j["before"].as_str() {
            Some(b) => before = b.to_string(),
            None => return out,
        }
    }
}

fn ids(v: &[Value]) -> Vec<String> {
    v.iter().map(|m| m["id"].as_str().unwrap().to_string()).collect()
}

async fn read(hub: &Arc<Hub>, who: &Id, id: &str) {
    let r = Call::new("POST", "/api/receipts")
        .auth(pair(&who.0, &who.1))
        .json(json!({ "receipts": [{ "id": id, "state": "read", "at": "2026-10-08T15:00:00.000Z" }] }))
        .send(hub)
        .await;
    assert_eq!(r.code(), 200);
}

async fn sync_from(hub: &Arc<Hub>, who: &Id, device: &str, cursor: &Value) -> Value {
    let r = Call::new("POST", "/api/sync").auth(pair(&who.0, &who.1)).json(json!({ "device_id": device, "cursor": cursor, "wait": 0 })).send(hub).await;
    assert_eq!(r.code(), 200, "{}", r.text());
    r.json()
}

#[tokio::test]
async fn history_kept_until_deleted() {
    let pg = TestPg::start();
    let url = pg.fresh_db("hubhist").await;
    let data = std::env::temp_dir().join(format!("orgtree-hub-hist-{}", token_hex16()));
    std::fs::create_dir_all(&data).unwrap();
    // v2's default: no HUB_RETENTION_DAYS at all
    let hub = hub(&url, &data, &[("HUB_RETENTION_DAYS", "")]).await;
    let sql = sql_client(&url).await;

    // ------------------------------------------------------------- kept
    let h = Call::new("GET", "/healthz").send(&hub).await.json();
    assert_eq!(h["retention_days"], Value::Null);
    for f in ["history", "delete", "sync"] {
        assert!(h["features"].as_array().unwrap().contains(&json!(f)), "{h}");
    }
    let (ann, reg) = register(&hub, "ann").await;
    assert_eq!(reg["retention_days"], Value::Null);
    assert_eq!(Call::new("GET", "/ui/data").send(&hub).await.json()["retention_days"], Value::Null);
    assert!(Call::new("GET", "/").send(&hub).await.text().contains("kept until deleted"));
    let (bob, _) = register(&hub, "bob").await;
    let (carl, _) = register(&hub, "carl").await;
    send(&hub, &bob, &ann.0, "old-1", json!({ "reply_to": "older" })).await;
    sql.execute("UPDATE messages SET received_at = now() - interval '90 days' WHERE id = 'old-1'", &[]).await.unwrap();
    let swept = mailhub::sweep::run_once(&hub).await.unwrap();
    assert_eq!(swept.messages, 0);
    let page = get(&hub, &ann, &format!("/api/history?with={}", bob.0)).await.json();
    assert_eq!(ids(page["messages"].as_array().unwrap()), ["old-1"]);
    let m = &page["messages"][0];
    assert_eq!((m["reply_to"].clone(), m["read_at"].clone(), m["delivered_at"].clone()), (json!("older"), Value::Null, Value::Null));
    assert!(m["received_at"].as_str().unwrap().ends_with('Z') && m.get("fetched_at").is_some());
    println!("  ok  with no HUB_RETENTION_DAYS nothing ages out: a 90-day-old message comes back from history (retention_days null everywhere)");

    // ----------------------------------------------- uploads never sent go
    let upload = |name: &'static str| {
        let (hub, bob) = (hub.clone(), bob.clone());
        async move {
            let r = Call::new("POST", &format!("/api/attachments?name={name}")).auth(pair(&bob.0, &bob.1)).content(&b"file bytes"[..]).send(&hub).await;
            assert_eq!(r.code(), 200, "{}", r.text());
            r.json()["id"].as_str().unwrap().to_string()
        }
    };
    let loose = upload("loose.txt").await;
    let kept = upload("kept.txt").await;
    send(&hub, &bob, &ann.0, "with-file", json!({ "attachments": [kept] })).await;
    sql.execute("UPDATE attachments SET created_at = now() - interval '8 days'", &[]).await.unwrap();
    let swept = mailhub::sweep::run_once(&hub).await.unwrap();
    assert_eq!(swept.attachments, 1);
    assert!(!data.join("blobs").join(&loose).exists(), "an upload never sent is still on disk");
    assert_eq!(get(&hub, &ann, &format!("/api/attachments/{kept}")).await.code(), 200);
    println!("  ok  an upload no send ever bound goes after a week; a sent one stays");

    // ------------------------------------------------------- conversations
    send(&hub, &bob, &ann.0, "b2", json!({})).await;
    send(&hub, &ann, &bob.0, "a1", json!({})).await;
    send(&hub, &ann, &carl.0, "to-carl", json!({})).await;
    send(&hub, &ann, &ann.0, "note-to-self", json!({})).await;
    let conv = get(&hub, &ann, "/api/conversations").await.json();
    let rows = conv["conversations"].as_array().unwrap();
    let order: Vec<&str> = rows.iter().map(|r| r["with"].as_str().unwrap()).collect();
    assert_eq!(order, [ann.0.as_str(), carl.0.as_str(), bob.0.as_str()], "{conv}");
    let bob_row = &rows[2];
    assert_eq!((bob_row["last"]["id"].clone(), bob_row["unread"].clone()), (json!("a1"), json!(3)));
    read(&hub, &ann, "b2").await;
    let conv = get(&hub, &ann, "/api/conversations").await.json();
    assert_eq!(conv["conversations"][2]["unread"], 2);
    let bobs = get(&hub, &bob, "/api/conversations").await.json();
    assert_eq!(bobs["conversations"].as_array().unwrap().len(), 1);
    assert_eq!((bobs["conversations"][0]["with"].clone(), bobs["conversations"][0]["unread"].clone()), (json!(ann.0), json!(1)));
    println!("  ok  conversations: one row per correspondent, newest first, with the last message and the unread count");

    // ------------------------------------------------------------- paging
    let (dee, _) = register(&hub, "dee").await;
    let mut sent = Vec::new();
    for i in 0..210 {
        let id = format!("p-{i:03}");
        if i % 2 == 0 {
            send(&hub, &dee, &ann.0, &id, json!({})).await;
        } else {
            send(&hub, &ann, &dee.0, &id, json!({})).await;
        }
        sent.push(id);
    }
    let all = whole_history(&hub, &ann, &dee.0, 50).await;
    let mut newest_first = sent.clone();
    newest_first.reverse();
    assert_eq!(ids(&all), newest_first);
    let first = get(&hub, &ann, &format!("/api/history?with={}&limit=500", dee.0)).await.json();
    assert_eq!(first["messages"].as_array().unwrap().len(), 200, "limit was not held to 200");
    let before = first["before"].as_str().expect("a next page").to_string();
    let rest = get(&hub, &dee, &format!("/api/history?with={}&limit=500&before={before}", ann.0)).await.json();
    assert_eq!((rest["messages"].as_array().unwrap().len(), rest["before"].clone()), (10, Value::Null));
    for (q, detail) in [
        ("", "with is required: the address of the conversation"),
        ("?with=x&limit=many", "limit must be a whole number"),
        ("?with=x&before=yesterday", "before is not a history cursor from this hub"),
    ] {
        let r = get(&hub, &ann, &format!("/api/history{q}")).await;
        assert_eq!((r.code(), r.json()["detail"].clone()), (422, json!(detail)), "{q}");
    }
    assert_eq!(Call::new("GET", "/api/history?with=x").send(&hub).await.code(), 401);
    println!("  ok  history pages newest first ({} messages, 50 a page, 200 at most), every message once; bad queries refused", all.len());

    // ------------------------------------------------- deleting one message
    let phone = sync_from(&hub, &ann, "ann-phone", &Value::Null).await;
    let mut phone_cursor = phone["cursor"].clone();
    let r = delete(&hub, &ann, "/api/messages/b2").await;
    assert_eq!((r.code(), r.json()["deleted"].clone()), (200, json!(1)));
    assert!(!ids(&whole_history(&hub, &ann, &bob.0, 50).await).contains(&"b2".to_string()));
    assert!(ids(&whole_history(&hub, &bob, &ann.0, 50).await).contains(&"b2".to_string()), "the other side lost its copy");
    let a = sync_from(&hub, &ann, "ann-phone", &phone_cursor).await;
    assert_eq!(a["changes"], json!([{ "type": "deleted", "id": "b2" }]));
    phone_cursor = a["cursor"].clone();
    assert_eq!(delete(&hub, &ann, "/api/messages/b2").await.json()["deleted"], 0);
    assert_eq!(delete(&hub, &ann, "/api/messages/no-such").await.code(), 404);
    assert_eq!(delete(&hub, &carl, "/api/messages/a1").await.code(), 404, "a stranger learned a message exists");
    let bob_sync = sync_from(&hub, &bob, "bob-phone", &Value::Null).await;
    let bob_ids: HashSet<&str> = bob_sync["changes"].as_array().unwrap().iter().filter_map(|c| c["message"]["id"].as_str()).collect();
    assert!(bob_ids.contains("b2"));
    println!("  ok  deleting a message removes the caller's copy only, reaches its devices by sync, and is idempotent (404 for others' mail)");

    // -------------- a file stays for the recipient after the sender deletes
    let r = delete(&hub, &bob, "/api/messages/with-file").await;
    assert_eq!(r.json()["deleted"], 1);
    assert_eq!(get(&hub, &ann, &format!("/api/attachments/{kept}")).await.code(), 200, "the recipient lost the file");
    assert_eq!(sql.query_one("SELECT count(*) FROM messages WHERE id = 'with-file'", &[]).await.unwrap().get::<_, i64>(0), 1);
    // a receipt now does not bring the sender's deleted copy back
    let bob_cursor = bob_sync["cursor"].clone();
    let b = sync_from(&hub, &bob, "bob-phone", &bob_cursor).await;
    let bob_cursor = b["cursor"].clone();
    read(&hub, &ann, "with-file").await;
    let b = sync_from(&hub, &bob, "bob-phone", &bob_cursor).await;
    assert!(b["changes"].as_array().unwrap().iter().all(|c| c["message"]["id"] != json!("with-file")), "{b}");
    // the last copy goes: the row and the file with it
    assert_eq!(delete(&hub, &ann, "/api/messages/with-file").await.json()["deleted"], 1);
    assert_eq!(sql.query_one("SELECT count(*) FROM messages WHERE id = 'with-file'", &[]).await.unwrap().get::<_, i64>(0), 0);
    assert!(!data.join("blobs").join(&kept).exists(), "the file outlived the last copy");
    assert_eq!(get(&hub, &ann, &format!("/api/attachments/{kept}")).await.code(), 404);
    // a device away all along still learns of it, after the row is gone
    let away = sync_from(&hub, &ann, "ann-phone", &phone_cursor).await;
    assert!(away["changes"].as_array().unwrap().contains(&json!({ "type": "deleted", "id": "with-file" })), "{away}");
    phone_cursor = away["cursor"].clone();
    println!("  ok  a file stays downloadable for the recipient after the sender deletes; it goes with the last copy, and the tombstone outlives the row");

    // -------------------------- deleting a conversation, from one device
    let laptop = sync_from(&hub, &ann, "ann-laptop", &Value::Null).await["cursor"].clone();
    let r = delete(&hub, &ann, &format!("/api/conversations/{}", dee.0)).await;
    assert_eq!((r.code(), r.json()["deleted"].clone()), (200, json!(210)));
    assert_eq!(whole_history(&hub, &ann, &dee.0, 50).await.len(), 0);
    assert_eq!(whole_history(&hub, &dee, &ann.0, 50).await.len(), 210, "the other person lost the conversation");
    let mut gone = HashSet::new();
    let mut cursor = laptop;
    loop {
        let a = sync_from(&hub, &ann, "ann-laptop", &cursor).await;
        for c in a["changes"].as_array().unwrap() {
            if c["type"] == "deleted" {
                gone.insert(c["id"].as_str().unwrap().to_string());
            }
        }
        cursor = a["cursor"].clone();
        if a["more"] != json!(true) {
            break;
        }
    }
    assert!(sent.iter().all(|id| gone.contains(id)), "another device kept part of the deleted conversation");
    let conv = get(&hub, &ann, "/api/conversations").await.json();
    assert!(conv["conversations"].as_array().unwrap().iter().all(|c| c["with"] != json!(dee.0)));
    assert_eq!(delete(&hub, &ann, &format!("/api/conversations/{}", dee.0)).await.json()["deleted"], 0);
    println!("  ok  deleting a conversation on one device removes it from the others, while the other person keeps theirs");

    // ------------------- the recipient deletes mail still in v1's queue
    send(&hub, &carl, &ann.0, "queued-then-deleted", json!({})).await;
    assert_eq!(delete(&hub, &ann, "/api/messages/queued-then-deleted").await.json()["deleted"], 1);
    let v1 = Call::new("POST", "/api/poll?wait=0").auth(pair(&ann.0, &ann.1)).send(&hub).await.json();
    assert!(v1["messages"].as_array().unwrap().iter().all(|m| m["id"] != "queued-then-deleted"), "{v1}");
    let carls = Call::new("POST", "/api/poll?wait=0").auth(pair(&carl.0, &carl.1)).send(&hub).await.json();
    assert!(carls["receipts"].as_array().unwrap().iter().any(|r| r["id"] == "queued-then-deleted" && r["state"] == "fetched"), "{carls}");
    println!("  ok  mail the recipient deletes before any client took it leaves v1's queue (its v1 sender sees \"fetched\")");

    // ------------------------------------- to oneself, and the same id again
    assert_eq!(delete(&hub, &ann, "/api/messages/note-to-self").await.json()["deleted"], 1);
    assert_eq!(sql.query_one("SELECT count(*) FROM messages WHERE id = 'note-to-self'", &[]).await.unwrap().get::<_, i64>(0), 0);
    send(&hub, &ann, &ann.0, "note-to-self", json!({})).await;
    let a = sync_from(&hub, &ann, "ann-phone", &phone_cursor).await;
    let kinds: Vec<(String, String)> = a["changes"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| c["id"] == "note-to-self" || c["message"]["id"] == "note-to-self")
        .map(|c| (c["type"].as_str().unwrap().to_string(), "note-to-self".to_string()))
        .collect();
    assert_eq!(kinds.last().map(|k| k.0.as_str()), Some("message"), "{a}");
    println!("  ok  a note to oneself has one copy; once both are gone an id can be sent again (devices see the delete, then the new one)");

    // ------------------------------------------------------------ routes
    let r = get(&hub, &ann, "/api/messages/a1").await;
    assert_eq!((r.code(), r.headers.get("allow").map(|v| v.to_str().unwrap().to_string())), (405, Some("DELETE".into())));
    let r = delete(&hub, &ann, "/api/history").await;
    assert_eq!((r.code(), r.headers.get("allow").map(|v| v.to_str().unwrap().to_string())), (405, Some("GET".into())));
    assert_eq!(Call::new("DELETE", "/api/messages/a1").send(&hub).await.code(), 401);
    println!("  ok  routes: methods and credentials");

    let _ = std::fs::remove_dir_all(&data);
    drop(pg);
}
