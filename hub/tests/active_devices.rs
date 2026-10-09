//! Active devices: the device a person is using says so
//! (`POST /api/devices/active`), and the address's other devices see it in
//! every sync answer, so they can leave notifications to it.
//!
//!     HUB_TEST_PG=<folder> cargo test --test active_devices -- --nocapture

mod support;

use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::Engine;
use ed25519_dalek::{Signer, SigningKey};
use mailhub::api::keys::device_certificate;
use mailhub::auth::call_message;
use mailhub::Hub;
use serde_json::{json, Value};
use support::*;

type Id = (String, String);

fn new_key() -> SigningKey {
    let mut seed = [0u8; 32];
    seed[..16].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
    seed[16..].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
    SigningKey::from_bytes(&seed)
}

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn public(k: &SigningKey) -> String {
    b64(&k.verifying_key().to_bytes())
}

fn token(slug: &str, device: &str, key: &SigningKey) -> String {
    let ms = chrono::Utc::now().timestamp_millis();
    format!("{slug}:dev1.{device}.{ms}.{}", b64(&key.sign(call_message(slug, device, ms).as_bytes()).to_bytes()))
}

async fn register(hub: &Arc<Hub>, name: &str) -> Id {
    let id = (format!("{name}.active.{}", token_hex3()), token_hex16());
    let r = Call::new("POST", "/api/register")
        .auth(pair(&id.0, &id.1))
        .json(json!({ "slug": id.0, "org_name": name, "username": "active", "kind": "person" }))
        .send(hub)
        .await;
    assert_eq!(r.code(), 200, "{}", r.text());
    id
}

async fn sync(hub: &Arc<Hub>, auth: &str, device: &str, wait: f64) -> Value {
    let r = Call::new("POST", "/api/sync").auth(auth).json(json!({ "device_id": device, "wait": wait })).send(hub).await;
    assert_eq!(r.code(), 200, "{}", r.text());
    r.json()
}

async fn active(hub: &Arc<Hub>, auth: &str, body: Value) -> Resp {
    Call::new("POST", "/api/devices/active").auth(auth).json(body).send(hub).await
}

async fn device_list(hub: &Arc<Hub>, auth: &str) -> Vec<Value> {
    let r = Call::new("GET", "/api/devices").auth(auth).send(hub).await;
    assert_eq!(r.code(), 200, "{}", r.text());
    r.json()["devices"].as_array().unwrap().clone()
}

fn device<'a>(list: &'a [Value], id: &str) -> &'a Value {
    list.iter().find(|d| d["device_id"] == json!(id)).unwrap_or_else(|| panic!("{id} not in {list:?}"))
}

fn seconds_ahead(iso: &Value) -> f64 {
    let t = chrono::DateTime::parse_from_rfc3339(iso.as_str().unwrap()).unwrap();
    (t.with_timezone(&chrono::Utc) - chrono::Utc::now()).num_milliseconds() as f64 / 1000.0
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn active_devices() {
    let pg = TestPg::start();
    let url = pg.fresh_db("hubactive").await;
    let data = std::env::temp_dir().join(format!("orgtree-hub-active-{}", token_hex16()));
    std::fs::create_dir_all(&data).unwrap();
    let hub = hub(&url, &data, &[]).await;
    let sql = sql_client(&url).await;
    let (ann, bob) = (register(&hub, "ann").await, register(&hub, "bob").await);
    let auth = pair(&ann.0, &ann.1);

    // ------------------------------------------------ the signal itself
    for d in ["phone", "laptop"] {
        assert_eq!(sync(&hub, &auth, d, 0.0).await["active"], json!([]), "a sync answer always says who is active");
    }
    let r = active(&hub, &auth, json!({ "device_id": "laptop", "active": true })).await;
    assert_eq!(r.code(), 200, "{}", r.text());
    let a = r.json();
    assert_eq!((a["device_id"].clone(), a["active"].clone()), (json!("laptop"), json!(true)));
    let ahead = seconds_ahead(&a["active_until"]);
    assert!((85.0..=91.0).contains(&ahead), "active for about 90 s, got {ahead}");
    assert_eq!(sync(&hub, &auth, "phone", 0.0).await["active"], json!(["laptop"]));
    assert_eq!(sync(&hub, &auth, "laptop", 0.0).await["active"], json!([]), "never the asking device itself");
    let list = device_list(&hub, &auth).await;
    assert_eq!((device(&list, "laptop")["active"].clone(), device(&list, "phone")["active"].clone()), (json!(true), json!(false)));
    assert!(device(&list, "laptop")["active_until"].is_string() && device(&list, "phone")["active_until"].is_null());
    println!("  ok  a device in use says so for 90 s; the others' sync answers and the devices list show it (never to itself)");

    // ------------------- the answer that brings mail says who is active
    let cursor = sync(&hub, &auth, "phone", 0.0).await["cursor"].clone();
    let parked = {
        let (hub, auth) = (hub.clone(), auth.clone());
        tokio::spawn(async move {
            let r = Call::new("POST", "/api/sync").auth(&auth).json(json!({ "device_id": "phone", "cursor": cursor, "wait": 20.0 })).send(&hub).await;
            assert_eq!(r.code(), 200, "{}", r.text());
            r.json()
        })
    };
    tokio::time::sleep(Duration::from_millis(300)).await;
    let r = Call::new("POST", "/api/send")
        .auth(pair(&bob.0, &bob.1))
        .json(json!({ "id": "m-active-1", "to": ann.0, "body": "are you there?", "sent_at": "2026-10-09T00:00:00.000Z" }))
        .send(&hub)
        .await;
    assert_eq!(r.code(), 200, "{}", r.text());
    let a = parked.await.unwrap();
    assert!(a["changes"].as_array().unwrap().iter().any(|c| c["message"]["id"] == json!("m-active-1")), "{a}");
    assert_eq!(a["active"], json!(["laptop"]), "the answer carrying the mail says the laptop is in use");
    println!("  ok  the answer that brings new mail carries the active devices, so the notify decision cannot race the mail");

    // ----------------------------------------- lapse, renewal, false
    sql.execute("UPDATE mailhub.devices SET active_until = now() - interval '1 second' WHERE slug = $1 AND device_id = 'laptop'", &[&ann.0])
        .await
        .unwrap();
    assert_eq!(sync(&hub, &auth, "phone", 0.0).await["active"], json!([]), "a lapsed active no longer counts");
    let list = device_list(&hub, &auth).await;
    assert_eq!((device(&list, "laptop")["active"].clone(), device(&list, "laptop")["active_until"].clone()), (json!(false), Value::Null));
    assert_eq!(active(&hub, &auth, json!({ "device_id": "laptop", "active": true })).await.code(), 200);
    assert_eq!(sync(&hub, &auth, "phone", 0.0).await["active"], json!(["laptop"]), "sending it again renews it");
    let r = active(&hub, &auth, json!({ "device_id": "laptop", "active": false })).await;
    assert_eq!((r.code(), r.json()["active_until"].clone()), (200, Value::Null));
    assert_eq!(sync(&hub, &auth, "phone", 0.0).await["active"], json!([]), "false ends it at once");
    println!("  ok  it lapses by itself, renews when sent again, and false ends it at once");

    // -------------------------------- a parked sync is not woken for it
    let cursor = sync(&hub, &auth, "phone", 0.0).await["cursor"].clone();
    let started = Instant::now();
    let parked = {
        let (hub, auth, cursor) = (hub.clone(), auth.clone(), cursor.clone());
        tokio::spawn(async move {
            let r = Call::new("POST", "/api/sync").auth(&auth).json(json!({ "device_id": "phone", "cursor": cursor, "wait": 2.0 })).send(&hub).await;
            assert_eq!(r.code(), 200, "{}", r.text());
            r.json()
        })
    };
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(active(&hub, &auth, json!({ "device_id": "laptop", "active": true })).await.code(), 200);
    let a = parked.await.unwrap();
    let waited = started.elapsed();
    assert!(waited >= Duration::from_millis(1800), "an active change ended a parked sync after {waited:?}");
    assert_eq!(a["active"], json!(["laptop"]), "but the answer, when it comes, has it");
    println!("  ok  an active change never ends a parked sync early ({waited:?} for wait=2); the next answer carries it");

    // ------------------------------------------------------- refusals
    let r = Call::new("POST", "/api/devices/active").json(json!({ "device_id": "laptop", "active": true })).send(&hub).await;
    assert_eq!(r.code(), 401);
    assert_eq!(active(&hub, &pair(&ann.0, &token_hex16()), json!({ "device_id": "laptop", "active": true })).await.code(), 401);
    for (body, detail) in [
        (json!({ "device_id": "laptop" }), "active is required: true or false"),
        (json!({ "device_id": "laptop", "active": "yes" }), "active must be true or false"),
        (json!({ "active": true }), "device_id is required"),
    ] {
        let r = active(&hub, &auth, body.clone()).await;
        assert_eq!((r.code(), r.json()["detail"].clone()), (422, json!(detail)), "{body}");
    }
    let r = active(&hub, &auth, json!({ "device_id": "has space", "active": true })).await;
    assert_eq!(r.code(), 422);
    let r = Call::new("GET", "/api/devices/active").auth(auth.clone()).send(&hub).await;
    assert_eq!(r.code(), 405);
    assert_eq!(active(&hub, &auth, json!({ "device_id": "tablet", "active": true })).await.code(), 200, "a device is made on first sight, as at a sync");
    assert_eq!(device(&device_list(&hub, &auth).await, "tablet")["active"], json!(true));
    println!("  ok  no or wrong credentials 401; a missing or non-boolean active, or a bad device_id, 422; GET 405; a new device is made");

    // ---------------------------------------- signed-out and signed devices
    sql.execute("UPDATE mailhub.devices SET revoked_at = now() WHERE slug = $1 AND device_id = 'tablet'", &[&ann.0]).await.unwrap();
    assert!(!sync(&hub, &auth, "phone", 0.0).await["active"].as_array().unwrap().contains(&json!("tablet")), "a signed-out device never counts");
    let r = active(&hub, &auth, json!({ "device_id": "tablet", "active": true })).await;
    assert_eq!((r.code(), r.json()["detail"].clone()), (401, json!("this device was signed out")));
    let (identity, key) = (new_key(), new_key());
    let r = Call::new("POST", "/api/identity").auth(auth.clone()).json(json!({ "identity_key": public(&identity) })).send(&hub).await;
    assert_eq!(r.code(), 200, "{}", r.text());
    let created = "2026-10-09T00:00:00.000Z";
    let cert = device_certificate(&ann.0, "desk", &public(&key), created);
    let r = Call::new("POST", "/api/devices")
        .json(json!({ "slug": ann.0, "device_id": "desk", "public_key": public(&key), "created": created,
                      "signature": b64(&identity.sign(cert.as_bytes()).to_bytes()), "name": "desk" }))
        .send(&hub)
        .await;
    assert_eq!(r.code(), 200, "{}", r.text());
    let r = active(&hub, &token(&ann.0, "desk", &key), json!({ "device_id": "phone", "active": true })).await;
    assert_eq!((r.code(), r.json()["detail"].clone()), (422, json!("device_id must be the signing device's own")));
    assert_eq!(active(&hub, &token(&ann.0, "desk", &key), json!({ "device_id": "desk", "active": true })).await.code(), 200);
    assert!(sync(&hub, &auth, "phone", 0.0).await["active"].as_array().unwrap().contains(&json!("desk")));
    println!("  ok  a signed-out device is refused and never counts; a device signing its calls speaks only for itself");

    // ----------------------------------------------- feature detection
    let h = Call::new("GET", "/healthz").send(&hub).await.json();
    assert!(h["features"].as_array().unwrap().contains(&json!("active")), "{h}");
    println!("  ok  /healthz lists the \"active\" feature");
}
