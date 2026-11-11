//! Web Push for browsers (Hubchat Web): a real HTTP delivery to a local
//! receiver, decrypted and checked. Run with --features push-test.
#![cfg(feature = "push-test")]
mod support;

use axum::{
    body::{Body, Bytes},
    extract::{Request, State},
    Router,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use http::{HeaderMap, Response};
use http_body_util::BodyExt;
use mailhub::Hub;
use p256::ecdsa::{signature::Verifier, Signature, SigningKey, VerifyingKey};
use serde_json::{json, Value};
use std::sync::{
    atomic::{AtomicU16, Ordering},
    Arc,
};
use support::*;
use tokio::sync::mpsc;
use web_push_native::{
    p256::{elliptic_curve::sec1::ToEncodedPoint, SecretKey},
    Auth,
};

#[derive(Clone)]
struct Receiver {
    received: mpsc::UnboundedSender<(HeaderMap, Bytes)>,
    status: Arc<AtomicU16>,
}

async fn receive(State(state): State<Receiver>, req: Request) -> Response<Body> {
    let (parts, body) = req.into_parts();
    let bytes = body.collect().await.unwrap().to_bytes();
    state.received.send((parts.headers, bytes)).unwrap();
    Response::builder().status(state.status.load(Ordering::SeqCst)).body(Body::empty()).unwrap()
}

async fn identity(h: &Arc<Hub>, name: &str) -> (String, String) {
    let slug = format!("{name}.wp.{}", token_hex3());
    let auth = pair(&slug, &token_hex16());
    let r = Call::new("POST", "/api/register").auth(&auth).json(json!({"slug":slug,"org_name":name,"username":"wp","kind":"person"})).send(h).await;
    assert_eq!(r.code(), 200);
    (slug, auth)
}

async fn post(h: &Arc<Hub>, auth: &str, path: &str, body: Value) -> Resp {
    Call::new("POST", path).auth(auth).json(body).send(h).await
}

async fn send(h: &Arc<Hub>, auth: &str, to: &str, id: &str, body: &str) {
    let r = post(h, auth, "/api/send", json!({"to":to,"id":id,"body":body,"kind":"message"})).await;
    assert_eq!(r.code(), 200, "{}", r.text());
}

/// Make every wake due, send what is due, and say how many were taken up.
async fn dispatch(h: &Arc<Hub>, c: &tokio_postgres::Client) -> usize {
    c.batch_execute("UPDATE device_push SET next_attempt = now() - interval '1 second' WHERE next_attempt IS NOT NULL").await.unwrap();
    mailhub::push::dispatch_due(h).await.unwrap()
}

const SUBSCRIPTION: [u8; 32] = [7u8; 32];
const AUTH: [u8; 16] = [9u8; 16];
const VAPID: [u8; 32] = [11u8; 32];

/// The notification in one received push, after its signature is checked
/// against the registered key.
fn opened(headers: &HeaderMap, bytes: &Bytes, endpoint: &str, contact: &str) -> Value {
    assert_eq!(headers["content-encoding"], "aes128gcm");
    assert_eq!(headers["ttl"], "86400");
    assert_eq!(headers["urgency"], "high");
    // a push that replaced an undelivered one would lose what that one said
    assert!(!headers.contains_key("topic"));
    assert!(!headers.contains_key("x-org-auth"));
    let header = headers["authorization"].to_str().unwrap();
    let (token, public) = header.strip_prefix("vapid t=").unwrap().split_once(", k=").unwrap();
    let parts: Vec<&str> = token.split('.').collect();
    assert_eq!(parts.len(), 3);
    let key = SigningKey::from_slice(&VAPID).unwrap();
    let verifying = VerifyingKey::from_sec1_bytes(&URL_SAFE_NO_PAD.decode(public).unwrap()).unwrap();
    assert_eq!(verifying, *key.verifying_key(), "signed with the key the browser registered");
    verifying
        .verify(format!("{}.{}", parts[0], parts[1]).as_bytes(), &Signature::from_slice(&URL_SAFE_NO_PAD.decode(parts[2]).unwrap()).unwrap())
        .unwrap();
    let claims: Value = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[1]).unwrap()).unwrap();
    let origin = endpoint.rsplit_once('/').unwrap().0;
    assert_eq!(claims["aud"], origin);
    assert_eq!(claims["sub"], contact);
    let now = chrono::Utc::now().timestamp();
    let exp = claims["exp"].as_i64().unwrap();
    assert!(exp > now + 11 * 3600 && exp <= now + 12 * 3600 + 5, "{exp} {now}");
    let private = SecretKey::from_slice(&SUBSCRIPTION).unwrap();
    let plain = web_push_native::decrypt(bytes.to_vec(), &private, &Auth::from(AUTH)).unwrap();
    assert!(plain.len() <= 2000);
    serde_json::from_slice(&plain).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn webpush() {
    let pg = TestPg::start();
    let database = pg.fresh_db("hubwebpush").await;
    let data = std::env::temp_dir().join(format!("hubwebpush-{}", token_hex16()));
    let hub = hub(&database, &data, &[("HUB_PUSH_CONTACT", "mailto:owner@example.com")]).await;
    let sql = sql_client(&database).await;
    let (alice, alice_auth) = identity(&hub, "alice").await;
    let (bob, bob_auth) = identity(&hub, "bob").await;
    let (carol, carol_auth) = identity(&hub, "carol").await;
    for device in ["web", "pc"] {
        assert_eq!(post(&hub, &alice_auth, "/api/sync", json!({"device_id":device,"wait":0})).await.code(), 200);
    }
    let health = Call::new("GET", "/healthz").send(&hub).await.json();
    assert!(health["features"].as_array().unwrap().contains(&json!("webpush")));

    let (tx, mut rx) = mpsc::unbounded_channel();
    let state = Receiver { received: tx, status: Arc::new(AtomicU16::new(201)) };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/subscription-secret", listener.local_addr().unwrap());
    let router = Router::new().fallback(receive).with_state(state.clone());
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let private = SecretKey::from_slice(&SUBSCRIPTION).unwrap();
    let p256dh = URL_SAFE_NO_PAD.encode(private.public_key().to_encoded_point(false).as_bytes());
    let vapid = URL_SAFE_NO_PAD.encode(VAPID);
    let registration = json!({"kind":"webpush","device_id":"web","endpoint":endpoint,"p256dh":p256dh,"auth":URL_SAFE_NO_PAD.encode(AUTH),
        "vapid":vapid,"navigate":"http://localhost:8080","preview":true});

    // ---- 1. registration
    for (field, bad) in [
        ("kind", json!("apns")),
        ("kind", json!(7)),
        ("vapid", json!("bad")),
        ("vapid", json!(URL_SAFE_NO_PAD.encode([0u8; 32]))),
        ("vapid", Value::Null),
        ("navigate", json!("http://example.com/")),
        ("navigate", json!("https://user:pw@example.com/")),
        ("navigate", json!("https://example.com/?a=1")),
        ("navigate", Value::Null),
        ("preview", json!("yes")),
        ("endpoint", json!("https://192.168.1.2/x")),
        ("p256dh", json!("bad")),
    ] {
        let mut body = registration.clone();
        body[field] = bad.clone();
        let r = post(&hub, &alice_auth, "/api/push", body).await;
        assert_eq!(r.code(), 422, "{field} {bad}: {}", r.text());
    }
    assert_eq!(post(&hub, "", "/api/push", registration.clone()).await.code(), 401);
    assert_eq!(post(&hub, &alice_auth, "/api/push", registration.clone()).await.json(), json!({"registered":true}));
    let generation: String = sql.query_one("SELECT registration FROM device_push", &[]).await.unwrap().get(0);
    // the same again changes nothing; without `preview` it is on
    let mut again = registration.clone();
    again.as_object_mut().unwrap().remove("preview");
    assert_eq!(post(&hub, &alice_auth, "/api/push", again).await.code(), 200);
    let row = sql.query_one("SELECT registration, kind, navigate, preview, octet_length(vapid) FROM device_push", &[]).await.unwrap();
    assert_eq!(row.get::<_, String>(0), generation);
    assert_eq!(row.get::<_, String>(1), "webpush");
    assert_eq!(row.get::<_, String>(2), "http://localhost:8080/");
    assert!(row.get::<_, bool>(3));
    assert_eq!(row.get::<_, i32>(4), 32);
    for path in ["/api/devices", "/ui/data"] {
        let r = Call::new("GET", path).auth(&alice_auth).send(&hub).await;
        assert!(!r.text().contains(&vapid) && !r.text().contains(&endpoint), "{path}");
    }
    // mail from before the registration is not announced: the first wake finds nothing to say
    assert_eq!(dispatch(&hub, &sql).await, 1);
    assert!(rx.try_recv().is_err());
    assert!(sql.query_one("SELECT next_attempt IS NULL FROM device_push", &[]).await.unwrap().get::<_, bool>(0));
    println!("ok 1 a browser registers with its own signing key; bad values refused; the key is never listed; nothing is sent with nothing to say");

    // ---- 2. one message
    send(&hub, &bob_auth, &alice, "m1", "Are we still on for 3?").await;
    assert_eq!(dispatch(&hub, &sql).await, 1);
    let (headers, bytes) = rx.recv().await.unwrap();
    assert_eq!(
        opened(&headers, &bytes, &endpoint, "mailto:owner@example.com"),
        json!({"web_push": 8030, "notification": {"title": "bob", "body": "Are we still on for 3?",
               "navigate": format!("http://localhost:8080/#/chat/{bob}"), "tag": format!("hc-{bob}"), "silent": false}, "app_badge": 1})
    );
    assert!(sql.query_one("SELECT next_attempt IS NULL AND notified_until IS NOT NULL FROM device_push", &[]).await.unwrap().get::<_, bool>(0));
    assert_eq!(dispatch(&hub, &sql).await, 0);
    println!("ok 2 one message: signed for the push service, decrypts to the sender and the text");

    // ---- 3. several, and only what was not announced yet
    send(&hub, &bob_auth, &alice, "m2", "second").await;
    send(&hub, &bob_auth, &alice, "m3", "third").await;
    assert_eq!(dispatch(&hub, &sql).await, 1);
    let (headers, bytes) = rx.recv().await.unwrap();
    let n = opened(&headers, &bytes, &endpoint, "mailto:owner@example.com");
    assert_eq!(n["notification"]["title"], "bob");
    assert_eq!(n["notification"]["body"], "third (+1 more)");
    assert_eq!(n["app_badge"], 3);
    send(&hub, &bob_auth, &alice, "m4", "fourth").await;
    send(&hub, &carol_auth, &alice, "m5", "from carol").await;
    assert_eq!(dispatch(&hub, &sql).await, 1);
    let (headers, bytes) = rx.recv().await.unwrap();
    let n = opened(&headers, &bytes, &endpoint, "mailto:owner@example.com");
    assert_eq!(n["notification"], json!({"title": "2 new messages", "body": "From carol and bob", "navigate": "http://localhost:8080/", "tag": "hc-many", "silent": false}));
    assert_eq!(n["app_badge"], 5);
    println!("ok 3 several from one sender, then several senders; each message is announced once");

    // ---- 4. without the text
    let mut quiet = registration.clone();
    quiet["preview"] = json!(false);
    assert_eq!(post(&hub, &alice_auth, "/api/push", quiet).await.code(), 200);
    assert_eq!(sql.query_one("SELECT registration FROM device_push", &[]).await.unwrap().get::<_, String>(0), generation);
    send(&hub, &carol_auth, &alice, "m6", "the secret plan").await;
    assert_eq!(dispatch(&hub, &sql).await, 1);
    let (headers, bytes) = rx.recv().await.unwrap();
    let n = opened(&headers, &bytes, &endpoint, "mailto:owner@example.com");
    assert_eq!(n["notification"]["title"], "Hubchat");
    assert_eq!(n["notification"]["body"], "New message");
    assert!(!n.to_string().contains("secret") && !n.to_string().contains("carol"), "{n}");
    println!("ok 4 with the text switched off the push carries nothing of the mail");

    // ---- 5. Do not disturb sends nothing, and what it held back is counted afterwards
    assert_eq!(post(&hub, &alice_auth, "/api/status", json!({"status":"dnd"})).await.code(), 200);
    send(&hub, &bob_auth, &alice, "m7", "while dnd").await;
    assert_eq!(dispatch(&hub, &sql).await, 1);
    assert!(rx.try_recv().is_err());
    assert!(sql.query_one("SELECT next_attempt IS NULL FROM device_push", &[]).await.unwrap().get::<_, bool>(0));
    assert_eq!(post(&hub, &alice_auth, "/api/status", json!({"status":"auto"})).await.code(), 200);
    send(&hub, &bob_auth, &alice, "m8", "after dnd").await;
    assert_eq!(dispatch(&hub, &sql).await, 1);
    let (headers, bytes) = rx.recv().await.unwrap();
    assert_eq!(opened(&headers, &bytes, &endpoint, "mailto:owner@example.com")["notification"]["body"], "2 new messages");
    println!("ok 5 Do not disturb: no push; afterwards the next one counts what waited");

    // ---- 6. held while any device of the address is in use
    assert_eq!(post(&hub, &alice_auth, "/api/devices/active", json!({"device_id":"pc","active":true})).await.code(), 200);
    send(&hub, &bob_auth, &alice, "m9", "while at the pc").await;
    assert_eq!(dispatch(&hub, &sql).await, 0);
    assert!(rx.try_recv().is_err());
    assert_eq!(post(&hub, &alice_auth, "/api/devices/active", json!({"device_id":"pc","active":false})).await.code(), 200);
    assert_eq!(dispatch(&hub, &sql).await, 1);
    let _ = rx.recv().await.unwrap();
    println!("ok 6 held while another device of the address is in use, sent when it is not");

    // ---- 7. read on another device meanwhile: nothing to say
    send(&hub, &bob_auth, &alice, "m10", "read elsewhere").await;
    let at = chrono::Utc::now().to_rfc3339();
    let r = post(&hub, &alice_auth, "/api/receipts", json!({"receipts":[{"id":"m10","state":"read","at":at}]})).await;
    assert_eq!(r.code(), 200, "{}", r.text());
    assert_eq!(dispatch(&hub, &sql).await, 1);
    assert!(rx.try_recv().is_err());
    assert!(sql.query_one("SELECT next_attempt IS NULL FROM device_push", &[]).await.unwrap().get::<_, bool>(0));
    println!("ok 7 a message read elsewhere before the push is not announced");

    // ---- 8. the push service says the subscription is gone
    state.status.store(410, Ordering::SeqCst);
    send(&hub, &bob_auth, &alice, "m11", "to a dead subscription").await;
    assert_eq!(dispatch(&hub, &sql).await, 1);
    let _ = rx.recv().await.unwrap();
    assert_eq!(sql.query_one("SELECT count(*) FROM device_push", &[]).await.unwrap().get::<_, i64>(0), 0);
    println!("ok 8 a subscription the push service no longer knows is forgotten");
    let _ = (carol, std::fs::remove_dir_all(&data));
}
