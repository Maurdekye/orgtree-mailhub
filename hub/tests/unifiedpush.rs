//! Isolated real HTTP delivery proof. Run with --features push-test.
#![cfg(feature = "push-test")]
mod support;

use axum::{
    body::{Body, Bytes},
    extract::{Request, State},
    Router,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use ed25519_dalek::{Signer, SigningKey};
use http::{HeaderMap, Response};
use http_body_util::BodyExt;
use mailhub::{api::keys::device_certificate, auth::call_message, Hub};
use serde_json::{json, Value};
use std::sync::{
    atomic::{AtomicU16, Ordering},
    Arc,
};
use support::*;
use tokio::sync::{mpsc, Semaphore};
use web_push_native::{
    p256::{elliptic_curve::sec1::ToEncodedPoint, SecretKey},
    Auth,
};

#[derive(Clone)]
struct Receiver {
    received: mpsc::UnboundedSender<(HeaderMap, Bytes)>,
    status: Arc<AtomicU16>,
    gate: Arc<Semaphore>,
}

async fn receive(State(state): State<Receiver>, req: Request) -> Response<Body> {
    let (parts, body) = req.into_parts();
    let bytes = body.collect().await.unwrap().to_bytes();
    state.received.send((parts.headers, bytes)).unwrap();
    let permit = state.gate.acquire().await.unwrap();
    permit.forget();
    Response::builder()
        .status(state.status.load(Ordering::SeqCst))
        .header("Location", "/redirected")
        .body(Body::empty())
        .unwrap()
}

async fn identity(h: &Arc<Hub>, name: &str) -> (String, String) {
    let slug = format!("{name}.push.{}", token_hex3());
    let auth = pair(&slug, &token_hex16());
    let r = Call::new("POST", "/api/register")
        .auth(&auth)
        .json(json!({"slug":slug,"org_name":name,"username":"push","kind":"person"}))
        .send(h)
        .await;
    assert_eq!(r.code(), 200);
    (slug, auth)
}

async fn register(h: &Arc<Hub>, auth: &str, body: &Value) -> Resp {
    Call::new("POST", "/api/push").auth(auth).json(body.clone()).send(h).await
}

async fn send(h: &Arc<Hub>, auth: &str, to: &str, id: &str) {
    let r = Call::new("POST", "/api/send")
        .auth(auth)
        .json(json!({"to":to,"id":id,"body":"private message; must never enter push","kind":"message"}))
        .send(h)
        .await;
    assert_eq!(r.code(), 200, "{}", r.text());
}

async fn due(c: &tokio_postgres::Client) {
    c.batch_execute("UPDATE device_push SET next_attempt = now() - interval '1 second'")
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unifiedpush() {
    let pg = TestPg::start();
    let database = pg.fresh_db("hubpush").await;
    let data = std::env::temp_dir().join(format!("hubpush-{}", token_hex16()));
    let hub = hub(&database, &data, &[]).await;
    let sql = sql_client(&database).await;
    let (alice, alice_auth) = identity(&hub, "alice").await;
    let (_, bob_auth) = identity(&hub, "bob").await;
    let sync = Call::new("POST", "/api/sync")
        .auth(&alice_auth)
        .json(json!({"device_id":"phone","wait":0}))
        .send(&hub)
        .await;
    assert_eq!(sync.code(), 200);
    let health = Call::new("GET", "/healthz").send(&hub).await.json();
    assert!(health["features"].as_array().unwrap().contains(&json!("unifiedpush")));

    let (tx, mut rx) = mpsc::unbounded_channel();
    let state = Receiver {
        received: tx,
        status: Arc::new(AtomicU16::new(201)),
        gate: Arc::new(Semaphore::new(50)),
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/subscription-secret", listener.local_addr().unwrap());
    let router = Router::new().fallback(receive).with_state(state.clone());
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let private = SecretKey::from_slice(&[7u8; 32]).unwrap();
    let key = URL_SAFE_NO_PAD.encode(private.public_key().to_encoded_point(false).as_bytes());
    let auth = URL_SAFE_NO_PAD.encode([9u8; 16]);
    let registration = json!({"device_id":"phone","endpoint":endpoint,"p256dh":key,"auth":auth});
    assert_eq!(register(&hub, "", &registration).await.code(), 401);
    assert_eq!(register(&hub, &bob_auth, &registration).await.code(), 409);
    assert_eq!(register(&hub, &alice_auth, &registration).await.json(), json!({"registered":true}));
    let generation: String = sql.query_one("SELECT registration FROM device_push", &[]).await.unwrap().get(0);
    assert_eq!(register(&hub, &alice_auth, &registration).await.code(), 200);
    assert_eq!(
        sql.query_one("SELECT registration FROM device_push", &[]).await.unwrap().get::<_, String>(0),
        generation
    );
    for (field, bad) in [
        ("endpoint", "http://example.com/"),
        ("endpoint", "https://user:pass@example.com/"),
        ("endpoint", "https://192.168.1.2/x"),
        ("p256dh", "bad"),
        ("auth", "bad"),
    ] {
        let mut body = registration.clone();
        body[field] = json!(bad);
        assert_eq!(register(&hub, &alice_auth, &body).await.code(), 422);
    }
    println!("ok 1 registration authenticated, device-scoped, idempotent; malformed/private endpoints refused");

    for path in ["/api/devices", "/ui/data"] {
        let r = Call::new("GET", path).auth(&alice_auth).send(&hub).await;
        assert!(!r.text().contains(&endpoint) && !r.text().contains(&auth));
    }
    // New registration gets one wake so already waiting mail is fetched.
    assert_eq!(mailhub::push::dispatch_due(&hub).await.unwrap(), 1);
    let (headers, bytes) = rx.recv().await.unwrap();
    assert_eq!(headers["content-encoding"], "aes128gcm");
    assert_eq!(headers["ttl"], "86400");
    assert!(!headers.contains_key("x-org-auth"));
    assert_eq!(web_push_native::decrypt(bytes.to_vec(), &private, &Auth::from([9u8; 16])).unwrap(), b"wake");
    assert!(sql
        .query_one("SELECT next_attempt IS NULL FROM device_push", &[])
        .await
        .unwrap()
        .get::<_, bool>(0));
    assert_eq!(mailhub::push::dispatch_due(&hub).await.unwrap(), 0);
    println!("ok 2 real HTTP request decrypts to constant wake; capabilities absent from API listings");

    for i in 0..30 {
        send(&hub, &bob_auth, &alice, &format!("burst-{i}")).await;
    }
    let pending: i64 = sql.query_one("SELECT pending FROM device_push", &[]).await.unwrap().get(0);
    send(&hub, &bob_auth, &alice, "burst-0").await;
    assert_eq!(sql.query_one("SELECT pending FROM device_push", &[]).await.unwrap().get::<_, i64>(0), pending);
    due(&sql).await;
    assert_eq!(mailhub::push::dispatch_due(&hub).await.unwrap(), 1);
    let _ = rx.recv().await.unwrap();
    assert!(rx.try_recv().is_err());
    println!("ok 3 committed burst coalesces; duplicate sends do not enqueue a wake");

    // In-flight completion must not erase new mail or an endpoint rotation.
    state.gate.forget_permits(state.gate.available_permits());
    send(&hub, &bob_auth, &alice, "inflight-1").await;
    due(&sql).await;
    let h = hub.clone();
    let dispatch = tokio::spawn(async move { mailhub::push::dispatch_due(&h).await.unwrap() });
    let _ = rx.recv().await.unwrap();
    send(&hub, &bob_auth, &alice, "inflight-2").await;
    state.gate.add_permits(1);
    dispatch.await.unwrap();
    assert!(sql
        .query_one("SELECT next_attempt IS NOT NULL FROM device_push", &[])
        .await
        .unwrap()
        .get::<_, bool>(0));
    due(&sql).await;
    let h = hub.clone();
    let dispatch = tokio::spawn(async move { mailhub::push::dispatch_due(&h).await.unwrap() });
    let _ = rx.recv().await.unwrap();
    let mut replacement = registration.clone();
    replacement["endpoint"] = json!(format!("{endpoint}-new"));
    assert_eq!(register(&hub, &alice_auth, &replacement).await.code(), 200);
    state.status.store(410, Ordering::SeqCst);
    state.gate.add_permits(1);
    dispatch.await.unwrap();
    assert_eq!(
        sql.query_one("SELECT endpoint FROM device_push", &[]).await.unwrap().get::<_, String>(0),
        format!("{endpoint}-new")
    );
    println!("ok 4 new mail during HTTP survives; stale 410 cannot erase replacement subscription");

    state.gate.add_permits(50);
    state.status.store(503, Ordering::SeqCst);
    assert_eq!(mailhub::push::dispatch_due(&hub).await.unwrap(), 1);
    let _ = rx.recv().await.unwrap();
    let row = sql.query_one("SELECT attempts, next_attempt > now() FROM device_push", &[]).await.unwrap();
    assert_eq!(row.get::<_, i32>(0), 1);
    assert!(row.get::<_, bool>(1));
    // Restarting the hub preserves pending work.
    let restarted = mailhub::server::prepare(hub.cfg.clone()).await.unwrap();
    due(&sql).await;
    state.status.store(307, Ordering::SeqCst);
    assert_eq!(mailhub::push::dispatch_due(&restarted).await.unwrap(), 1);
    let _ = rx.recv().await.unwrap();
    assert!(rx.try_recv().is_err(), "followed redirect");
    due(&sql).await;
    state.status.store(410, Ordering::SeqCst);
    mailhub::push::dispatch_due(&hub).await.unwrap();
    let _ = rx.recv().await.unwrap();
    assert_eq!(sql.query_one("SELECT count(*) FROM device_push", &[]).await.unwrap().get::<_, i64>(0), 0);
    println!("ok 5 failure retries persist over restart, redirects refused, 410 drops capabilities");

    register(&hub, &alice_auth, &registration).await;
    assert_eq!(
        Call::new("DELETE", "/api/push?device_id=phone").auth(&alice_auth).send(&hub).await.json(),
        json!({"registered":false})
    );
    assert_eq!(mailhub::push::dispatch_due(&hub).await.unwrap(), 0);
    register(&hub, &alice_auth, &registration).await;
    assert_eq!(Call::new("DELETE", "/api/devices/phone").auth(&alice_auth).send(&hub).await.code(), 200);
    assert_eq!(sql.query_one("SELECT count(*) FROM device_push", &[]).await.unwrap().get::<_, i64>(0), 0);
    assert_eq!(register(&hub, &alice_auth, &registration).await.code(), 409);

    // Enrolled-device credentials cannot register/remove another device.
    let identity_key = SigningKey::from_bytes(&[3u8; 32]);
    let device_key = SigningKey::from_bytes(&[4u8; 32]);
    let b64 = |v: &[u8]| URL_SAFE_NO_PAD.encode(v);
    let r = Call::new("POST", "/api/identity")
        .auth(&alice_auth)
        .json(json!({"identity_key":b64(&identity_key.verifying_key().to_bytes())}))
        .send(&hub)
        .await;
    assert_eq!(r.code(), 200);
    let public = b64(&device_key.verifying_key().to_bytes());
    let cert = device_certificate(&alice, "signed", &public, "2026-10-10T00:00:00Z");
    let r = Call::new("POST", "/api/devices")
        .json(json!({"slug":alice,"device_id":"signed","public_key":public,
        "created":"2026-10-10T00:00:00Z","signature":b64(&identity_key.sign(cert.as_bytes()).to_bytes())}))
        .send(&hub)
        .await;
    assert_eq!(r.code(), 200);
    let ms = chrono::Utc::now().timestamp_millis();
    let signed_auth = format!(
        "{alice}:dev1.signed.{ms}.{}",
        b64(&device_key.sign(call_message(&alice, "signed", ms).as_bytes()).to_bytes())
    );
    assert_eq!(register(&hub, &signed_auth, &registration).await.code(), 401);
    assert_eq!(Call::new("DELETE", "/api/push?device_id=phone").auth(&signed_auth).send(&hub).await.code(), 401);
    let mut own = registration.clone();
    own["device_id"] = json!("signed");
    assert_eq!(register(&hub, &signed_auth, &own).await.code(), 200);
    let r = Call::new("POST", "/api/unregister").auth(&alice_auth).json(json!({})).send(&hub).await;
    assert_eq!(r.code(), 200);
    assert_eq!(sql.query_one("SELECT count(*) FROM device_push", &[]).await.unwrap().get::<_, i64>(0), 0);
    println!("ok 6 DELETE, device revocation and identity unregister clear secrets; signed caller constrained to self");

    server.abort();
    let _ = server.await;
    hub.shutdown.cancel();
    restarted.shutdown.cancel();
    drop(sql);
    drop(restarted);
    drop(hub);
    drop(pg);
    let _ = std::fs::remove_dir_all(data);
}
