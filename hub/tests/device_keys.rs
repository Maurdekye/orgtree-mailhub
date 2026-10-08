//! Phase 2, slice 7: per-device keys and signing a device out (G5).
//!
//!     HUB_TEST_PG=<folder> cargo test --test device_keys -- --nocapture

mod support;

use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use ed25519_dalek::{Signer, SigningKey};
use mailhub::api::keys::{device_certificate, rotation_statement};
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

fn sign(k: &SigningKey, message: &str) -> String {
    b64(&k.sign(message.as_bytes()).to_bytes())
}

/// `X-Org-Auth` for a call `device` signs with `key`, made `age_ms` ago.
fn token(slug: &str, device: &str, key: &SigningKey, age_ms: i64) -> String {
    let ms = chrono::Utc::now().timestamp_millis() - age_ms;
    format!("{slug}:dev1.{device}.{ms}.{}", sign(key, &call_message(slug, device, ms)))
}

async fn register(hub: &Arc<Hub>, name: &str) -> Id {
    let id = (format!("{name}.keys.{}", token_hex3()), token_hex16());
    let r = Call::new("POST", "/api/register")
        .auth(pair(&id.0, &id.1))
        .json(json!({ "slug": id.0, "org_name": name, "username": "keys", "kind": "person" }))
        .send(hub)
        .await;
    assert_eq!(r.code(), 200, "{}", r.text());
    id
}

async fn enrol(hub: &Arc<Hub>, slug: &str, device: &str, key: &SigningKey, identity: &SigningKey) -> Resp {
    let created = "2026-10-08T18:00:00.000Z";
    let cert = device_certificate(slug, device, &public(key), created);
    Call::new("POST", "/api/devices")
        .json(json!({ "slug": slug, "device_id": device, "public_key": public(key), "created": created, "signature": sign(identity, &cert), "name": device }))
        .send(hub)
        .await
}

async fn sync_as(hub: &Arc<Hub>, auth: &str, device: &str, wait: f64) -> Resp {
    Call::new("POST", "/api/sync").auth(auth).json(json!({ "device_id": device, "wait": wait })).send(hub).await
}

async fn sign_out(hub: &Arc<Hub>, auth: &str, device: &str, body: Value) -> Resp {
    Call::new("DELETE", &format!("/api/devices/{device}")).auth(auth).json(body).send(hub).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn device_keys() {
    let pg = TestPg::start();
    let url = pg.fresh_db("hubkeys").await;
    let data = std::env::temp_dir().join(format!("orgtree-hub-keys-{}", token_hex16()));
    std::fs::create_dir_all(&data).unwrap();
    let hub = hub(&url, &data, &[("HUB_RETENTION_DAYS", "")]).await;
    let (ann, bob) = (register(&hub, "ann").await, register(&hub, "bob").await);
    let shared = pair(&ann.0, &ann.1);
    let (id0, phone, laptop, tablet) = (new_key(), new_key(), new_key(), new_key());

    // ------------------------------------------------- the identity key
    let s = Call::new("GET", "/api/identity").auth(shared.clone()).send(&hub).await.json();
    assert_eq!((s["identity_key"].clone(), s["key_version"].clone(), s["shared_key"].clone()), (Value::Null, json!(0), json!(true)));
    assert_eq!(enrol(&hub, &ann.0, "phone", &phone, &id0).await.code(), 409, "a device enrolled before the identity key existed");
    let set = |body: Value| {
        let (hub, shared) = (hub.clone(), shared.clone());
        async move { Call::new("POST", "/api/identity").auth(shared).json(body).send(&hub).await }
    };
    assert_eq!(set(json!({ "identity_key": public(&id0) })).await.code(), 200);
    assert_eq!(set(json!({ "identity_key": public(&id0) })).await.code(), 200, "setting the same key again");
    assert_eq!(set(json!({ "identity_key": public(&new_key()) })).await.code(), 409, "the identity key was replaced without a rotation");
    assert_eq!(set(json!({ "identity_key": "not-a-key" })).await.code(), 422);
    println!("  ok  an address registers its identity key once (with its shared key); another needs a rotation");

    // ---------------------------------------------------------- enrolment
    for (name, key) in [("phone", &phone), ("laptop", &laptop), ("tablet", &tablet)] {
        let r = enrol(&hub, &ann.0, name, key, &id0).await;
        assert_eq!(r.code(), 200, "{}", r.text());
    }
    assert_eq!(enrol(&hub, &ann.0, "rogue", &new_key(), &phone).await.code(), 401, "a certificate not signed by the identity key");
    assert_eq!(enrol(&hub, &bob.0, "bobs", &new_key(), &id0).await.code(), 409, "bob has no identity key");
    let r = Call::new("POST", "/api/devices").json(json!({ "slug": ann.0, "device_id": "x" })).send(&hub).await;
    assert_eq!(r.code(), 422);
    let list = Call::new("GET", "/api/devices").auth(shared.clone()).send(&hub).await.json();
    let enrolled: Vec<&str> = list["devices"].as_array().unwrap().iter().filter(|d| d["public_key"].is_string()).map(|d| d["device_id"].as_str().unwrap()).collect();
    assert_eq!(enrolled, ["phone", "laptop", "tablet"]);
    println!("  ok  devices enrol their own keys by certificates the identity key signs (no other sign-in); others are refused");

    // --------------------------------------------- calls a device signs
    let r = sync_as(&hub, &token(&ann.0, "phone", &phone, 0), "phone", 0.0).await;
    assert_eq!(r.code(), 200, "{}", r.text());
    assert_eq!(r.json()["identity_key_version"], 0);
    let r = sync_as(&hub, &token(&ann.0, "phone", &phone, 0), "laptop", 0.0).await;
    assert_eq!((r.code(), r.json()["detail"].clone()), (422, json!("device_id must be the signing device's own")));
    let r = Call::new("POST", "/api/send").auth(token(&ann.0, "laptop", &laptop, 0)).json(json!({ "to": bob.0, "body": "signed by the laptop" })).send(&hub).await;
    assert_eq!(r.code(), 200, "{}", r.text());
    assert_eq!(Call::new("GET", "/api/roster").auth(token(&ann.0, "tablet", &tablet, 0)).send(&hub).await.code(), 200);
    for (auth, why) in [
        (token(&ann.0, "phone", &phone, 20 * 60 * 1000), "a call signed 20 minutes ago"),
        (token(&ann.0, "phone", &laptop, 0), "a call signed with another device's key"),
        (token(&ann.0, "ghost", &phone, 0), "a device never enrolled"),
        (format!("{}:dev1.phone.{}.AAAA", ann.0, chrono::Utc::now().timestamp_millis()), "a signature that is not one"),
        (token(&bob.0, "phone", &phone, 0), "ann's device signing for bob"),
    ] {
        assert_eq!(Call::new("GET", "/api/roster").auth(auth).send(&hub).await.code(), 401, "{why} was accepted");
    }
    println!("  ok  calls signed by an enrolled device's key work everywhere; old, misattributed or forged signatures are refused");

    // ------------------------------------------- signing the tablet out
    // the tablet catches up, then parks
    let mut cursor = Value::Null;
    loop {
        let a = Call::new("POST", "/api/sync")
            .auth(token(&ann.0, "tablet", &tablet, 0))
            .json(json!({ "device_id": "tablet", "wait": 0, "cursor": cursor }))
            .send(&hub)
            .await
            .json();
        cursor = a["cursor"].clone();
        if a["more"] != json!(true) {
            break;
        }
    }
    let parked = {
        let (hub, slug, tablet) = (hub.clone(), ann.0.clone(), tablet.clone());
        tokio::spawn(async move {
            Call::new("POST", "/api/sync")
                .auth(token(&slug, "tablet", &tablet, 0))
                .json(json!({ "device_id": "tablet", "wait": 20, "cursor": cursor }))
                .send(&hub)
                .await
                .code()
        })
    };
    tokio::time::sleep(Duration::from_millis(300)).await;
    let id1 = new_key();
    let statement = rotation_statement(&ann.0, "tablet", &public(&id1), 1);
    let by_phone = token(&ann.0, "phone", &phone, 0);
    let good = json!({ "identity_key": public(&id1), "signature": sign(&id0, &statement), "sealed": { "phone": "sealed-for-phone", "laptop": "sealed-for-laptop" } });
    for (body, code, why) in [
        (json!({}), 422, "no rotation"),
        (json!({ "identity_key": public(&id1), "signature": sign(&id0, &statement), "sealed": { "phone": "p" } }), 422, "the laptop got no sealed key"),
        (json!({ "identity_key": public(&id1), "signature": sign(&id0, &statement), "sealed": { "phone": "p", "laptop": "l", "tablet": "t" } }), 422, "a sealed key for the device signed out"),
        (json!({ "identity_key": public(&id1), "signature": sign(&id1, &statement), "sealed": { "phone": "p", "laptop": "l" } }), 401, "a rotation the old key did not sign"),
        (json!({ "identity_key": public(&id0), "signature": sign(&id0, &rotation_statement(&ann.0, "tablet", &public(&id0), 1)), "sealed": { "phone": "p", "laptop": "l" } }), 422, "the same key again"),
    ] {
        let r = sign_out(&hub, &by_phone, "tablet", body).await;
        assert_eq!(r.code(), code, "{why}: {}", r.text());
    }
    let r = sign_out(&hub, &by_phone, "tablet", good).await;
    assert_eq!(r.code(), 200, "{}", r.text());
    assert_eq!((r.json()["rotated"].clone(), r.json()["key_version"].clone()), (json!(true), json!(1)));
    assert_eq!(parked.await.unwrap(), 401, "the signed-out device's parked sync was not refused");
    assert_eq!(Call::new("GET", "/api/roster").auth(token(&ann.0, "tablet", &tablet, 0)).send(&hub).await.code(), 401);
    assert_eq!(sync_as(&hub, &token(&ann.0, "laptop", &laptop, 0), "laptop", 0.0).await.code(), 200, "a remaining device lost access");
    assert_eq!(Call::new("GET", "/api/roster").auth(shared.clone()).send(&hub).await.code(), 401, "the shared key still works after a rotation");
    let s = Call::new("GET", "/api/identity").auth(token(&ann.0, "laptop", &laptop, 0)).send(&hub).await.json();
    assert_eq!(
        (s["identity_key"].clone(), s["key_version"].clone(), s["shared_key"].clone(), s["sealed"].clone()),
        (json!(public(&id1)), json!(1), json!(false), json!("sealed-for-laptop"))
    );
    assert_eq!(s["slug"], json!(ann.0), "the address changed");
    let a = sync_as(&hub, &token(&ann.0, "phone", &phone, 0), "phone", 0.0).await.json();
    assert_eq!(a["identity_key_version"], 1);
    println!("  ok  signing the tablet out revokes it at once (its parked sync too), rotates the identity key sealed to the others, ends the shared key; same address");

    // ------------------------------------------------- after the rotation
    assert_eq!(enrol(&hub, &ann.0, "desktop", &new_key(), &id0).await.code(), 401, "the old identity key enrolled a device");
    assert_eq!(enrol(&hub, &ann.0, "desktop", &new_key(), &id1).await.code(), 200);
    assert_eq!(enrol(&hub, &ann.0, "tablet", &new_key(), &id1).await.code(), 409, "a signed-out device id came back");
    let statement2 = rotation_statement(&ann.0, "laptop", &public(&new_key()), 2);
    let r = sign_out(&hub, &by_phone, "laptop", json!({ "identity_key": "x", "signature": sign(&id0, &statement2) })).await;
    assert_eq!(r.code(), 422);
    let id2 = new_key();
    let r = sign_out(
        &hub,
        &token(&ann.0, "phone", &phone, 0),
        "laptop",
        json!({ "identity_key": public(&id2), "signature": sign(&id0, &rotation_statement(&ann.0, "laptop", &public(&id2), 2)), "sealed": { "phone": "p", "desktop": "d" } }),
    )
    .await;
    assert_eq!(r.code(), 401, "the first identity key signed the second rotation");
    let r = sign_out(
        &hub,
        &token(&ann.0, "phone", &phone, 0),
        "laptop",
        json!({ "identity_key": public(&id2), "signature": sign(&id1, &rotation_statement(&ann.0, "laptop", &public(&id2), 2)), "sealed": { "phone": "p", "desktop": "d" } }),
    )
    .await;
    assert_eq!((r.code(), r.json()["key_version"].clone()), (200, json!(2)), "{}", r.text());
    let list = Call::new("GET", "/api/devices").auth(token(&ann.0, "phone", &phone, 0)).send(&hub).await.json();
    let out: Vec<&str> = list["devices"].as_array().unwrap().iter().filter(|d| d["signed_out_at"].is_string()).map(|d| d["device_id"].as_str().unwrap()).collect();
    assert_eq!(out, ["laptop", "tablet"]);
    assert_eq!(sign_out(&hub, &token(&ann.0, "phone", &phone, 0), "tablet", json!({})).await.code(), 409);
    assert_eq!(sign_out(&hub, &token(&ann.0, "phone", &phone, 0), "never-was", json!({})).await.code(), 404);
    let r = Call::new("GET", "/api/devices/phone").auth(token(&ann.0, "phone", &phone, 0)).send(&hub).await;
    assert_eq!((r.code(), r.headers["allow"].to_str().unwrap()), (405, "DELETE"));
    println!("  ok  only the current identity key enrols and rotates; a signed-out id stays out; the list shows who signed out");

    // ------------------------------- an address without device keys
    let r = sync_as(&hub, &pair(&bob.0, &bob.1), "bob-old-phone", 0.0).await;
    assert_eq!(r.code(), 200);
    let r = sign_out(&hub, &pair(&bob.0, &bob.1), "bob-old-phone", json!({})).await;
    assert_eq!((r.code(), r.json()["rotated"].clone()), (200, json!(false)));
    let r = sync_as(&hub, &pair(&bob.0, &bob.1), "bob-old-phone", 0.0).await;
    assert_eq!((r.code(), r.json()["detail"].clone()), (401, json!("this device was signed out")));
    println!("  ok  without device keys, signing out removes the device from sync (nothing to rotate)");

    // ------------------------------------ the owner turns the shared key off
    let carol = register(&hub, "carol").await;
    let (cid, cphone) = (new_key(), new_key());
    let off = |auth: String| {
        let hub = hub.clone();
        async move { Call::new("POST", "/api/identity").auth(auth).json(json!({ "shared_key": false })).send(&hub).await }
    };
    Call::new("POST", "/api/identity").auth(pair(&carol.0, &carol.1)).json(json!({ "identity_key": public(&cid) })).send(&hub).await;
    assert_eq!(off(pair(&carol.0, &carol.1)).await.code(), 422, "the shared key went off with no device to sign in");
    assert_eq!(enrol(&hub, &carol.0, "cphone", &cphone, &cid).await.code(), 200);
    assert_eq!(off(pair(&carol.0, &carol.1)).await.code(), 200);
    assert_eq!(Call::new("GET", "/api/roster").auth(pair(&carol.0, &carol.1)).send(&hub).await.code(), 401);
    let again = Call::new("POST", "/api/register")
        .auth(pair(&carol.0, &carol.1))
        .json(json!({ "slug": carol.0, "org_name": "taken over", "username": "keys", "kind": "person" }))
        .send(&hub)
        .await;
    assert_eq!((again.code(), again.json()["detail"].clone()), (401, json!("this address no longer accepts its shared secret")));
    let roster = Call::new("GET", "/api/roster").auth(token(&carol.0, "cphone", &cphone, 0)).send(&hub).await.json();
    assert!(roster["roster"].as_array().unwrap().iter().any(|r| r["slug"] == json!(carol.0) && r["org_name"] == json!("carol")), "{roster}");
    assert_eq!(Call::new("GET", "/api/roster").auth(token(&carol.0, "cphone", &cphone, 0)).send(&hub).await.code(), 200);
    let r = Call::new("POST", "/api/identity").auth(token(&carol.0, "cphone", &cphone, 0)).json(json!({ "shared_key": true })).send(&hub).await;
    assert_eq!(r.code(), 422, "the shared key came back on");
    let h = Call::new("GET", "/healthz").send(&hub).await.json();
    assert!(h["features"].as_array().unwrap().contains(&json!("device_keys")));
    println!("  ok  the owner can turn the shared key off for good once a device has its own key (registering with it is refused too)");

    let _ = std::fs::remove_dir_all(&data);
    drop(pg);
}
