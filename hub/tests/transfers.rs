//! Phase 2, slice 6: resumable uploads and the link relay.
//!
//!     HUB_TEST_PG=<folder> cargo test --test transfers -- --nocapture

mod support;

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Body;
use bytes::Bytes;
use mailhub::Hub;
use serde_json::{json, Value};
use support::*;

type Id = (String, String);

async fn register(hub: &Arc<Hub>, name: &str) -> Id {
    let id = (format!("{name}.xfer.{}", token_hex3()), token_hex16());
    let r = Call::new("POST", "/api/register")
        .auth(pair(&id.0, &id.1))
        .json(json!({ "slug": id.0, "org_name": name, "username": "xfer", "kind": "person" }))
        .send(hub)
        .await;
    assert_eq!(r.code(), 200, "{}", r.text());
    id
}

async fn start(hub: &Arc<Hub>, who: &Id, body: Value) -> Resp {
    Call::new("POST", "/api/uploads").auth(pair(&who.0, &who.1)).json(body).send(hub).await
}

async fn patch(hub: &Arc<Hub>, who: &Id, id: &str, offset: u64, bytes: &[u8]) -> Resp {
    Call::new("PATCH", &format!("/api/uploads/{id}?offset={offset}")).auth(pair(&who.0, &who.1)).content(bytes.to_vec()).send(hub).await
}

async fn status(hub: &Arc<Hub>, who: &Id, id: &str) -> Resp {
    Call::new("GET", &format!("/api/uploads/{id}")).auth(pair(&who.0, &who.1)).send(hub).await
}

fn sha256(b: &[u8]) -> String {
    use sha2::Digest;
    hex::encode(sha2::Sha256::digest(b))
}

fn pattern(n: usize) -> Vec<u8> {
    (0..n).map(|i| (i * 7 % 251) as u8).collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn transfers() {
    let pg = TestPg::start();
    let url = pg.fresh_db("hubxfer").await;
    let data = std::env::temp_dir().join(format!("orgtree-hub-xfer-{}", token_hex16()));
    std::fs::create_dir_all(&data).unwrap();
    let limit = 64 * 1024 * 1024;
    let vars = [("HUB_RETENTION_DAYS", ""), ("HUB_MAX_FILE_BYTES", "67108864")];
    let hub = hub(&url, &data, &vars).await;
    let sql = sql_client(&url).await;
    let (ann, bob) = (register(&hub, "ann").await, register(&hub, "bob").await);

    // ------------------------------------------------------ in three pieces
    let file = pattern(3_000_000);
    let r = start(&hub, &ann, json!({ "bytes": file.len(), "name": "../photo.jpg", "sha256": sha256(&file) })).await;
    assert_eq!(r.code(), 200, "{}", r.text());
    let up = r.json();
    let id = up["id"].as_str().unwrap().to_string();
    assert_eq!((up["offset"].clone(), up["complete"].clone(), up["name"].clone()), (json!(0), json!(false), json!("photo.jpg")));
    let r = patch(&hub, &ann, &id, 0, &file[..1_000_000]).await;
    assert_eq!((r.code(), r.json()["offset"].clone()), (200, json!(1_000_000)));
    let s = status(&hub, &ann, &id).await.json();
    assert_eq!((s["offset"].clone(), s["bytes"].clone(), s["complete"].clone()), (json!(1_000_000), json!(3_000_000), json!(false)));
    let r = patch(&hub, &ann, &id, 0, &file[..10]).await;
    assert_eq!((r.code(), r.json()["offset"].clone()), (409, json!(1_000_000)), "a piece at the wrong offset was taken");
    let r = patch(&hub, &ann, &id, 1_000_000, &file[1_000_000..2_000_000]).await;
    assert_eq!(r.json()["offset"], 2_000_000);
    let r = patch(&hub, &ann, &id, 2_000_000, &file[2_000_000..]).await;
    assert_eq!((r.json()["complete"].clone(), r.json()["id"].clone()), (json!(true), json!(id)));
    assert_eq!(status(&hub, &ann, &id).await.json()["complete"], true);
    let r = Call::new("POST", "/api/send").auth(pair(&ann.0, &ann.1)).json(json!({ "to": bob.0, "body": "pieces", "attachments": [id] })).send(&hub).await;
    assert_eq!(r.code(), 200, "{}", r.text());
    let got = Call::new("GET", &format!("/api/attachments/{id}")).auth(pair(&bob.0, &bob.1)).send(&hub).await;
    assert!(got.code() == 200 && got.body.as_ref() == file.as_slice(), "the file came back changed");
    println!("  ok  an upload in three pieces: offsets confirmed, a wrong offset refused (409 with the right one), complete it is an attachment");

    // ------------------------------------------ a connection cut mid-piece
    let file = pattern(5_000_000);
    let id = start(&hub, &ann, json!({ "bytes": file.len() })).await.json()["id"].as_str().unwrap().to_string();
    let head = Bytes::copy_from_slice(&file[..1_500_000]);
    let cut = futures::stream::iter(vec![Ok::<Bytes, std::io::Error>(head), Err(std::io::Error::other("connection reset"))]);
    let r = Call::new("PATCH", &format!("/api/uploads/{id}?offset=0")).auth(pair(&ann.0, &ann.1)).content(Body::from_stream(cut)).send(&hub).await;
    assert_eq!(r.code(), 400);
    assert_eq!(status(&hub, &ann, &id).await.json()["offset"], 1_500_000, "what arrived before the cut was lost");
    let r = patch(&hub, &ann, &id, 1_500_000, &file[1_500_000..]).await;
    assert_eq!(r.json()["complete"], true);
    let got = Call::new("GET", &format!("/api/attachments/{id}")).auth(pair(&ann.0, &ann.1)).send(&hub).await;
    assert!(got.body.as_ref() == file.as_slice());
    println!("  ok  a piece cut short keeps what arrived; the upload resumes from there");

    // ------------- a request that never finishes still confirms as it goes
    let file = pattern(12 * 1024 * 1024);
    let id = start(&hub, &ann, json!({ "bytes": file.len() })).await.json()["id"].as_str().unwrap().to_string();
    let first: Vec<Result<Bytes, std::io::Error>> = file[..9 * 1024 * 1024].chunks(1024 * 1024).map(|c| Ok(Bytes::copy_from_slice(c))).collect();
    let stalls = futures::stream::StreamExt::chain(futures::stream::iter(first), futures::stream::pending());
    let path = format!("/api/uploads/{id}?offset=0");
    let call = Call::new("PATCH", &path).auth(pair(&ann.0, &ann.1)).content(Body::from_stream(stalls)).send(&hub);
    let busy = {
        let (hub, ann, id) = (hub.clone(), ann.clone(), id.clone());
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(500)).await;
            patch(&hub, &ann, &id, 0, b"x").await.code()
        })
    };
    assert!(tokio::time::timeout(Duration::from_secs(2), call).await.is_err(), "the stalled request finished");
    assert_eq!(busy.await.unwrap(), 409, "two requests wrote to one upload at once");
    let at = status(&hub, &ann, &id).await.json()["offset"].as_u64().unwrap();
    assert_eq!(at, 8 * 1024 * 1024, "the checkpoint before the stall was not kept");
    let r = patch(&hub, &ann, &id, at, &file[at as usize..]).await;
    assert_eq!(r.json()["complete"], true, "{}", r.text());
    let got = Call::new("GET", &format!("/api/attachments/{id}")).auth(pair(&ann.0, &ann.1)).send(&hub).await;
    assert!(got.body.as_ref() == file.as_slice());
    println!("  ok  a request dropped mid-piece keeps its last 8 MiB checkpoint; a second writer meanwhile is refused (409)");

    // ----------------------------------------------- survives a restart
    let file = pattern(2_000_000);
    let id = start(&hub, &ann, json!({ "bytes": file.len() })).await.json()["id"].as_str().unwrap().to_string();
    patch(&hub, &ann, &id, 0, &file[..700_000]).await;
    drop(hub);
    let hub = support::hub(&url, &data, &vars).await;
    assert_eq!(status(&hub, &ann, &id).await.json()["offset"], 700_000);
    assert_eq!(patch(&hub, &ann, &id, 700_000, &file[700_000..]).await.json()["complete"], true);
    println!("  ok  an upload in progress survives a hub restart (its bytes live outside blobs/)");

    // --------------------------------------------------------- refusals
    let r = start(&hub, &ann, json!({ "bytes": limit + 1 })).await;
    assert_eq!((r.code(), r.json()["max_attachment_bytes"].clone()), (413, json!(limit)));
    for (body, detail) in [
        (json!({}), "bytes must be the upload's size: a whole number"),
        (json!({ "bytes": -1 }), "bytes must be the upload's size: a whole number"),
        (json!({ "bytes": 5, "name": 5 }), "name must be a string"),
        (json!({ "bytes": 5, "sha256": "abc" }), "sha256 must be 64 hexadecimal characters"),
    ] {
        let r = start(&hub, &ann, body.clone()).await;
        assert_eq!((r.code(), r.json()["detail"].clone()), (422, json!(detail)), "{body}");
    }
    let id = start(&hub, &ann, json!({ "bytes": 10 })).await.json()["id"].as_str().unwrap().to_string();
    let r = Call::new("PATCH", &format!("/api/uploads/{id}")).auth(pair(&ann.0, &ann.1)).content(b"x".to_vec()).send(&hub).await;
    assert_eq!(r.code(), 422, "no offset");
    let r = patch(&hub, &ann, &id, 0, b"0123456789-too-many").await;
    assert_eq!((r.code(), r.json()["offset"].clone()), (400, json!(10)));
    for (method, who) in [("GET", &bob), ("DELETE", &bob)] {
        assert_eq!(Call::new(method, &format!("/api/uploads/{id}")).auth(pair(&who.0, &who.1)).send(&hub).await.code(), 404);
    }
    assert_eq!(patch(&hub, &bob, &id, 0, b"x").await.code(), 404, "someone else wrote to an upload");
    assert_eq!(status(&hub, &ann, "not-an-upload").await.code(), 404);
    let r = Call::new("PUT", &format!("/api/uploads/{id}")).auth(pair(&ann.0, &ann.1)).send(&hub).await;
    assert_eq!((r.code(), r.headers["allow"].to_str().unwrap()), (405, "GET, PATCH, DELETE"));
    assert_eq!(start(&hub, &("x".into(), "y".into()), json!({ "bytes": 1 })).await.code(), 401);
    let bad = pattern(1000);
    let id = start(&hub, &ann, json!({ "bytes": 1000, "sha256": sha256(b"something else") })).await.json()["id"].as_str().unwrap().to_string();
    let r = patch(&hub, &ann, &id, 0, &bad).await;
    assert_eq!((r.code(), r.json()["offset"].clone()), (422, json!(0)), "bytes that do not match their sha256 were kept");
    assert_eq!(Call::new("DELETE", &format!("/api/uploads/{id}")).auth(pair(&ann.0, &ann.1)).send(&hub).await.json()["cancelled"], true);
    assert_eq!(status(&hub, &ann, &id).await.code(), 404);
    let empty = start(&hub, &ann, json!({ "bytes": 0, "name": "empty.txt" })).await.json();
    assert_eq!((empty["complete"].clone(), empty["bytes"].clone()), (json!(true), json!(0)));
    println!("  ok  refusals: over the limit (413 names it), bad fields, no offset, too many bytes, someone else's upload, a sha256 mismatch; an empty upload is complete at once");

    // ------------------------------------------------- a day idle is swept
    let id = start(&hub, &ann, json!({ "bytes": 10 })).await.json()["id"].as_str().unwrap().to_string();
    patch(&hub, &ann, &id, 0, b"01234").await;
    sql.execute("UPDATE uploads SET updated_at = now() - interval '25 hours' WHERE id = $1", &[&id]).await.unwrap();
    mailhub::sweep::run_once(&hub).await.unwrap();
    assert_eq!(status(&hub, &ann, &id).await.code(), 404);
    assert!(!data.join("uploads").join(format!("{id}.part")).exists());
    println!("  ok  an upload a day idle is swept, bytes and all");

    // ------------------------------------------------------- the link relay
    let put = |code: &str, sealed: Value| {
        let (hub, ann, code) = (hub.clone(), ann.clone(), code.to_string());
        async move { Call::new("POST", "/api/link/put").auth(pair(&ann.0, &ann.1)).json(json!({ "code": code, "sealed": sealed })).send(&hub).await }
    };
    let take = |code: &str, wait: f64| {
        let (hub, code) = (hub.clone(), code.to_string());
        async move { Call::new("POST", "/api/link/take").json(json!({ "code": code, "wait": wait })).send(&hub).await }
    };
    let r = put("K7Q2-9XMB", json!("sealed-ciphertext")).await;
    assert_eq!(r.code(), 200, "{}", r.text());
    assert_eq!(put("K7Q2-9XMB", json!("another")).await.code(), 409, "a waiting code was taken over");
    let r = take("K7Q2-9XMB", 0.0).await;
    assert_eq!((r.code(), r.json()["sealed"].clone(), r.json()["from"].clone()), (200, json!("sealed-ciphertext"), json!(ann.0)));
    assert_eq!(take("K7Q2-9XMB", 0.0).await.code(), 404, "a payload was taken twice");
    assert_eq!(put("K7Q2-9XMB", json!("again")).await.code(), 200, "a taken code stayed in use");
    let stored: String = sql.query_one("SELECT code_hash FROM link_relay", &[]).await.unwrap().get(0);
    assert_ne!(stored, "K7Q2-9XMB", "the code itself was stored");
    // the new device waits; the payload arrives
    let waiting = tokio::spawn(take("WAIT-CODE-1", 10.0));
    tokio::time::sleep(Duration::from_millis(300)).await;
    let t0 = Instant::now();
    put("WAIT-CODE-1", json!("for the waiter")).await;
    let r = waiting.await.unwrap();
    let woke = t0.elapsed().as_millis();
    assert_eq!((r.code(), r.json()["sealed"].clone()), (200, json!("for the waiter")));
    assert!(woke < 2000, "the waiting take answered after {woke} ms");
    // expiry, cancel, refusals
    put("OLD-CODE-1", json!("stale")).await;
    sql.execute("UPDATE link_relay SET expires_at = now() - interval '1 second'", &[]).await.unwrap();
    assert_eq!(take("OLD-CODE-1", 0.0).await.code(), 404, "an expired payload was handed out");
    assert_eq!(put("OLD-CODE-1", json!("fresh")).await.code(), 200, "an expired code stayed in use");
    let cancel = |who: &Id, code: &str| {
        let (hub, who, code) = (hub.clone(), who.clone(), code.to_string());
        async move { Call::new("POST", "/api/link/cancel").auth(pair(&who.0, &who.1)).json(json!({ "code": code })).send(&hub).await.json() }
    };
    assert_eq!(cancel(&bob, "OLD-CODE-1").await["cancelled"], false, "someone else cancelled a payload");
    assert_eq!(cancel(&ann, "OLD-CODE-1").await["cancelled"], true);
    assert_eq!(take("OLD-CODE-1", 0.0).await.code(), 404);
    assert_eq!(put("short", json!("x")).await.code(), 422);
    assert_eq!(put("LONG-ENOUGH", json!(5)).await.code(), 422);
    assert_eq!(put("LONG-ENOUGH", json!("s".repeat(1024 * 1024 + 1))).await.code(), 413);
    assert_eq!(Call::new("POST", "/api/link/put").json(json!({ "code": "LONG-ENOUGH", "sealed": "x" })).send(&hub).await.code(), 401);
    assert_eq!(Call::new("POST", "/api/link/take").json(json!({ "wait": 0 })).send(&hub).await.code(), 422);
    let h = Call::new("GET", "/healthz").send(&hub).await.json();
    assert!(h["features"].as_array().unwrap().contains(&json!("uploads")) && h["features"].as_array().unwrap().contains(&json!("link")));
    println!("  ok  link relay: a payload is taken once, a waiting take wakes in {woke} ms, codes are stored hashed, expire, can be cancelled; refusals");

    let _ = std::fs::remove_dir_all(&data);
    drop(pg);
}
