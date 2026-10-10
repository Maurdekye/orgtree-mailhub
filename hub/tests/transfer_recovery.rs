//! Recovery contracts used by durable clients. All state is disposable.
mod support;

use axum::body::Body;
use bytes::Bytes;
use mailhub::Hub;
use serde_json::json;
use std::sync::Arc;
use support::*;

struct Fixture {
    _pg: TestPg,
    url: String,
    data: std::path::PathBuf,
    hub: Arc<Hub>,
    auth: String,
}

impl Fixture {
    async fn new() -> Self {
        let pg = TestPg::start();
        let url = pg.fresh_db("recovery").await;
        let data = std::env::temp_dir().join(format!("hub-recovery-{}", token_hex16()));
        let hub = support::hub(&url, &data, &[]).await;
        let slug = format!("recovery.{}", token_hex3());
        let auth = pair(&slug, &token_hex16());
        let r = Call::new("POST", "/api/register").auth(&auth).json(json!({"slug": slug, "org_name": "Recovery", "kind": "person"})).send(&hub).await;
        assert_eq!(r.code(), 200, "{}", r.text());
        Self { _pg: pg, url, data, hub, auth }
    }

    async fn start(&self, bytes: usize) -> String {
        let r = Call::new("POST", "/api/uploads").auth(&self.auth).json(json!({"bytes": bytes, "name": "proof.bin"})).send(&self.hub).await;
        assert_eq!(r.code(), 200, "{}", r.text());
        r.json()["id"].as_str().unwrap().to_owned()
    }

    async fn status(&self, id: &str) -> Resp {
        Call::new("GET", &format!("/api/uploads/{id}")).auth(&self.auth).send(&self.hub).await
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.data);
    }
}

#[tokio::test]
async fn completion_recovery_and_range_validation() {
    let mut f = Fixture::new().await;
    let content = b"abcdefghij";
    let sql = sql_client(&f.url).await;
    for moved in [false, true] {
        let id = f.start(content.len()).await;
        let part = f.data.join("uploads").join(format!("{id}.part"));
        std::fs::write(&part, content).unwrap();
        sql.execute("UPDATE uploads SET received = total WHERE id = $1", &[&id]).await.unwrap();
        if moved {
            std::fs::create_dir_all(f.data.join("blobs")).unwrap();
            std::fs::rename(&part, f.data.join("blobs").join(&id)).unwrap();
        }
        // Reopen the hub after each disk/database crash boundary.
        f.hub = support::hub(&f.url, &f.data, &[]).await;
        let complete = f.status(&id).await;
        assert_eq!(complete.code(), 200, "{}", complete.text());
        assert_eq!(complete.json()["complete"], true);
        // A lost completion response is reconciled without a new upload.
        assert_eq!(f.status(&id).await.json(), complete.json());
        let path = format!("/api/attachments/{id}");
        let whole = Call::new("GET", &path).auth(&f.auth).send(&f.hub).await;
        assert_eq!(whole.body.as_ref(), content);
        let etag = whole.headers["etag"].to_str().unwrap();
        let resumed = Call::new("GET", &path).auth(&f.auth).header("range", "bytes=4-").header("if-range", etag).send(&f.hub).await;
        assert_eq!(resumed.code(), 206);
        assert_eq!(resumed.body.as_ref(), &content[4..]);
        let changed = Call::new("GET", &path).auth(&f.auth).header("range", "bytes=4-").header("if-range", "\"wrong-validator\"").send(&f.hub).await;
        assert_eq!(changed.code(), 200);
        assert_eq!(changed.body.as_ref(), content);
        println!("ok: recovery moved={moved}, lost response, valid Range and changed validator");
    }
}

#[tokio::test]
async fn missing_finalization_bytes_are_never_published() {
    let f = Fixture::new().await;
    let sql = sql_client(&f.url).await;
    for state in ["missing", "short partial", "short blob"] {
        let id = f.start(10).await;
        sql.execute("UPDATE uploads SET received = total WHERE id = $1", &[&id]).await.unwrap();
        let part = f.data.join("uploads").join(format!("{id}.part"));
        if state == "short partial" {
            std::fs::write(&part, b"short").unwrap();
        } else {
            std::fs::remove_file(&part).unwrap();
            if state == "short blob" {
                std::fs::write(f.data.join("blobs").join(&id), b"short").unwrap();
            }
        }
        let r = f.status(&id).await;
        assert_eq!(r.code(), 410, "{state} reported as completed: {}", r.text());
        let count: i64 = sql.query_one("SELECT count(*) FROM attachments WHERE id = $1", &[&id]).await.unwrap().get(0);
        assert_eq!(count, 0, "a nonexistent blob was published");
    }
}

#[tokio::test]
async fn active_writer_survives_expiry_and_cancel_conflicts() {
    let f = Fixture::new().await;
    let sql = sql_client(&f.url).await;
    let id = f.start(10).await;
    let ready = Arc::new(tokio::sync::Notify::new());
    let signal = ready.clone();
    let stream = futures::stream::once(async move {
        signal.notify_one();
        Ok::<Bytes, std::io::Error>(Bytes::from_static(b"abc"))
    });
    let stream = futures::StreamExt::chain(stream, futures::stream::pending());
    let (hub, auth, upload) = (f.hub.clone(), f.auth.clone(), id.clone());
    let writer = tokio::spawn(async move {
        Call::new("PATCH", &format!("/api/uploads/{upload}?offset=0")).auth(auth).content(Body::from_stream(stream)).send(&hub).await
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), ready.notified()).await.unwrap();
    let cancel = Call::new("DELETE", &format!("/api/uploads/{id}")).auth(&f.auth).send(&f.hub).await;
    assert_eq!(cancel.code(), 409);
    sql.execute("UPDATE uploads SET updated_at = now() - interval '25 hours' WHERE id = $1", &[&id]).await.unwrap();
    let sweep_result = mailhub::sweep::run_once(&f.hub).await;
    let after = f.status(&id).await;
    // Always stop the pending writer before asserting, including on failure.
    writer.abort();
    assert!(matches!(writer.await, Err(e) if e.is_cancelled()));
    sweep_result.unwrap();
    assert_eq!(after.code(), 200, "expiry removed an active writer: {}", after.text());
    let cancel = Call::new("DELETE", &format!("/api/uploads/{id}")).auth(&f.auth).send(&f.hub).await;
    assert_eq!(cancel.code(), 200, "{}", cancel.text());
    assert_eq!(f.status(&id).await.code(), 404);
    assert!(!f.data.join("uploads").join(format!("{id}.part")).exists());
}
