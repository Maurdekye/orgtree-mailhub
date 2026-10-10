//! G5: per-device keys, and signing one device out.
//!
//! An address registers an identity key (Ed25519; the hub keeps the public
//! half). Each device enrols its own key with a certificate the identity key
//! signs, then signs its calls with it (`auth::call_message`). Signing a
//! device out revokes its key and rotates the identity key in one step
//! (ruling 8 October): the client makes a new identity key, seals its secret
//! to every remaining device's key, and proves the change with the old
//! identity key; the hub stores the new public key for the same address,
//! keeps each remaining device's sealed copy for it to collect, and from
//! then on refuses the old identity key and the shared v1 key. The hub only
//! verifies signatures; it never sees a private key.

use std::sync::Arc;

use deadpool_postgres::GenericClient;
use http::StatusCode;
use serde_json::{json, Map, Value};

use super::mail::{authed_callers, mark_seen, one_address};
use super::sync::{DEVICES_MAX, DEVICE_ID_MAX, DEVICE_NAME_MAX};
use super::{ok, refuse, ApiResult, Hub, Req};
use crate::auth::{signed_by, verifying_key};
use crate::clock;
use crate::db;
use crate::wire::{pg_text, py_strip};

/// A sealed identity key for one device: at most this many bytes.
pub const SEALED_MAX: usize = 64 * 1024;

/// What the identity key signs to enrol a device.
pub fn device_certificate(slug: &str, device: &str, public_key: &str, created: &str) -> String {
    format!("orgtree-hub device v2\naddress={slug}\ndevice_id={device}\npublic_key={public_key}\ncreated={created}")
}

/// What the current identity key signs to sign a device out and hand over
/// to the next identity key.
pub fn rotation_statement(slug: &str, device: &str, new_key: &str, version: i32) -> String {
    format!("orgtree-hub rotate v2\naddress={slug}\nsign_out={device}\nidentity_key={new_key}\nkey_version={version}")
}

fn string<'a>(body: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    body.get(key).and_then(Value::as_str)
}

/// The address a request acts for, and the device that signed it (if one
/// did). The request is read before the future starts.
fn acting<'a>(
    hub: &'a Hub,
    req: &Req,
    asked: Option<Value>,
) -> impl std::future::Future<Output = ApiResult<(String, Option<String>)>> + Send + 'a {
    let auth = authed_callers(hub, req);
    async move {
        let callers = auth.await?;
        if callers.is_empty() {
            return refuse(StatusCode::UNAUTHORIZED, "no valid org credentials");
        }
        let slugs: Vec<String> = callers.iter().map(|c| c.slug.clone()).collect();
        let slug = one_address(&slugs, asked.as_ref())?;
        let device = callers.iter().find(|c| c.slug == slug && c.device.is_some()).and_then(|c| c.device.clone());
        Ok((slug, device))
    }
}

async fn state(c: &impl GenericClient, slug: &str, device: Option<&str>) -> ApiResult<Value> {
    let r = db::query_one(c, "SELECT identity_key, key_version, shared_key_enabled FROM identities WHERE slug = $1", &[&slug]).await?;
    let version: i32 = r.get(1);
    let mut out = json!({ "slug": slug, "identity_key": r.get::<_, Option<String>>(0), "key_version": version, "shared_key": r.get::<_, bool>(2) });
    if let Some(d) = device {
        let drop = db::query_opt(
            c,
            "SELECT sealed FROM identity_key_drops WHERE slug = $1 AND device_id = $2 AND key_version = $3",
            &[&slug, &d, &version],
        )
        .await?;
        out["sealed"] = json!(drop.map(|r| r.get::<_, String>(0)));
    }
    Ok(out)
}

/// `GET /api/identity[?slug=]`: the address's identity key, its version and
/// whether the shared v1 key still works; for a device-signed call, also
/// that device's sealed copy of the current identity key (after a rotation).
#[tracing::instrument(level = "debug", skip_all, ret(level = "debug"), err(level = "debug", Debug))]
pub async fn get_identity(hub: &Arc<Hub>, req: &mut Req) -> ApiResult {
    let asked = req.query("slug").map(|s| Value::String(s.to_string()));
    let (slug, device) = acting(hub, req, asked).await?;
    let c = hub.db.get().await?;
    mark_seen(hub, &c, std::slice::from_ref(&slug)).await?;
    ok(state(&c, &slug, device.as_deref()).await?)
}

/// `POST /api/identity {identity_key?, shared_key?: false, slug?}`: register
/// the identity key (once: later ones come by rotation), or turn the shared
/// v1 key off for good (not before a device has its own key).
#[tracing::instrument(level = "debug", skip_all, ret(level = "debug"), err(level = "debug", Debug))]
pub async fn set_identity(hub: &Arc<Hub>, req: &mut Req) -> ApiResult {
    let body = req.json_object_strict().await?;
    let (slug, _) = acting(hub, req, body.get("slug").cloned()).await?;
    let key = match body.get("identity_key") {
        None | Some(Value::Null) => None,
        Some(Value::String(k)) if verifying_key(k).is_some() => Some(k.clone()),
        Some(_) => return refuse(StatusCode::UNPROCESSABLE_ENTITY, "identity_key must be an Ed25519 public key (32 bytes, base64url)"),
    };
    let shared = match body.get("shared_key") {
        None | Some(Value::Null) => None,
        Some(Value::Bool(false)) => Some(false),
        Some(Value::Bool(true)) => return refuse(StatusCode::UNPROCESSABLE_ENTITY, "the shared key cannot be turned back on"),
        Some(_) => return refuse(StatusCode::UNPROCESSABLE_ENTITY, "shared_key must be false"),
    };
    if key.is_none() && shared.is_none() {
        return refuse(StatusCode::UNPROCESSABLE_ENTITY, "nothing to change: give identity_key and/or shared_key");
    }
    let mut c = hub.db.get().await?;
    let tx = c.transaction().await?;
    let r = db::query_one(&tx, "SELECT identity_key FROM identities WHERE slug = $1 FOR UPDATE", &[&slug]).await?;
    let current: Option<String> = r.get(0);
    if let Some(k) = &key {
        match &current {
            None => {
                db::execute(&tx, "UPDATE identities SET identity_key = $2 WHERE slug = $1", &[&slug, k]).await?;
            }
            Some(cur) if cur == k => {}
            Some(_) => return refuse(StatusCode::CONFLICT, "an identity key is set: a new one comes by signing a device out"),
        }
    }
    if shared == Some(false) {
        let enrolled: i64 = db::query_one(
            &tx,
            "SELECT count(*) FROM devices WHERE slug = $1 AND public_key IS NOT NULL AND revoked_at IS NULL",
            &[&slug],
        )
        .await?
        .get(0);
        if enrolled == 0 {
            return refuse(StatusCode::UNPROCESSABLE_ENTITY, "enrol a device with its own key before turning the shared key off");
        }
        db::execute(&tx, "UPDATE identities SET shared_key_enabled = false WHERE slug = $1", &[&slug]).await?;
    }
    tx.commit().await?;
    mark_seen(hub, &c, std::slice::from_ref(&slug)).await?;
    ok(state(&c, &slug, None).await?)
}

/// `POST /api/devices {slug, device_id, public_key, created, signature, name?}`:
/// enrol a device's own key. No other sign-in: the identity key's signature
/// over `device_certificate` is the proof.
#[tracing::instrument(level = "debug", skip_all, ret(level = "debug"), err(level = "debug", Debug))]
pub async fn enrol(hub: &Arc<Hub>, req: &mut Req) -> ApiResult {
    let body = req.json_object_strict().await?;
    let (Some(slug), Some(device), Some(public_key), Some(created), Some(signature)) = (
        string(&body, "slug"),
        string(&body, "device_id"),
        string(&body, "public_key"),
        string(&body, "created"),
        string(&body, "signature"),
    ) else {
        return refuse(StatusCode::UNPROCESSABLE_ENTITY, "a certificate needs slug, device_id, public_key, created and signature (strings)");
    };
    if device.is_empty() || device.len() > DEVICE_ID_MAX || !device.bytes().all(|b| (0x21..=0x7e).contains(&b)) {
        return refuse(StatusCode::UNPROCESSABLE_ENTITY, format!("device_id must be 1 to {DEVICE_ID_MAX} printable ASCII characters without spaces"));
    }
    if verifying_key(public_key).is_none() {
        return refuse(StatusCode::UNPROCESSABLE_ENTITY, "public_key must be an Ed25519 public key (32 bytes, base64url)");
    }
    if created.len() > 64 || created.contains('\0') {
        return refuse(StatusCode::UNPROCESSABLE_ENTITY, "created must be a time (ISO 8601)");
    }
    let name = match body.get("name") {
        None | Some(Value::Null) => None,
        Some(Value::String(n)) if py_strip(n).chars().count() <= DEVICE_NAME_MAX => Some(pg_text(py_strip(n).to_string())),
        Some(_) => return refuse(StatusCode::UNPROCESSABLE_ENTITY, format!("name must be a string of at most {DEVICE_NAME_MAX} characters")),
    };
    let mut c = hub.db.get().await?;
    let tx = c.transaction().await?;
    let Some(r) = db::query_opt(&tx, "SELECT identity_key FROM identities WHERE slug = $1 FOR UPDATE", &[&slug]).await? else {
        return refuse(StatusCode::UNAUTHORIZED, "no such address");
    };
    let Some(identity) = r.get::<_, Option<String>>(0) else {
        return refuse(StatusCode::CONFLICT, "this address has no identity key: register it first (POST /api/identity)");
    };
    if !signed_by(&identity, &device_certificate(slug, device, public_key, created), signature) {
        return refuse(StatusCode::UNAUTHORIZED, "the certificate is not signed by this address's identity key");
    }
    let existing = db::query_opt(&tx, "SELECT revoked_at IS NOT NULL FROM devices WHERE slug = $1 AND device_id = $2", &[&slug, &device]).await?;
    if existing.as_ref().is_some_and(|r| r.get::<_, bool>(0)) {
        return refuse(StatusCode::CONFLICT, "that device was signed out: enrol it under a new device_id");
    }
    if existing.is_none() {
        let keyed: i64 = db::query_one(
            &tx,
            "SELECT count(*) FROM devices WHERE slug = $1 AND public_key IS NOT NULL AND revoked_at IS NULL",
            &[&slug],
        )
        .await?
        .get(0);
        if keyed >= DEVICES_MAX {
            return refuse(StatusCode::CONFLICT, format!("this address has {DEVICES_MAX} enrolled devices: sign one out first"));
        }
    }
    let now = clock::now();
    db::execute(
        &tx,
        "INSERT INTO devices (slug, device_id, name, created_at, last_seen, public_key, cert_created, cert_signature)
         VALUES ($1, $2, COALESCE($3, ''), $4, $4, $5, $6, $7)
         ON CONFLICT (slug, device_id) DO UPDATE SET public_key = EXCLUDED.public_key, cert_created = EXCLUDED.cert_created,
                cert_signature = EXCLUDED.cert_signature, name = COALESCE($3, devices.name), last_seen = EXCLUDED.last_seen",
        &[&slug, &device, &name, &now, &public_key, &created, &signature],
    )
    .await?;
    tx.commit().await?;
    ok(json!({ "slug": slug, "device_id": device, "enrolled": true }))
}

/// `DELETE /api/devices/{device_id}[?slug=]`: sign a device out. With
/// device keys in use the body carries the rotation:
/// `{"identity_key": <new>, "signature": <old identity key over rotation_statement>,
///   "sealed": {<each remaining enrolled device_id>: <new identity key sealed to it>}}`.
#[tracing::instrument(level = "debug", skip(hub, req), ret(level = "debug"), err(level = "debug", Debug))]
pub async fn sign_out(hub: &Arc<Hub>, req: &mut Req, device: &str) -> ApiResult {
    let asked = req.query("slug").map(|s| Value::String(s.to_string()));
    // a body only when there is a rotation to carry
    let body = req.json_object_strict().await.unwrap_or_default();
    let (slug, _) = acting(hub, req, asked).await?;
    let mut c = hub.db.get().await?;
    let tx = c.transaction().await?;
    let id = db::query_one(&tx, "SELECT identity_key, key_version FROM identities WHERE slug = $1 FOR UPDATE", &[&slug]).await?;
    let (identity, version): (Option<String>, i32) = (id.get(0), id.get(1));
    let Some(row) = db::query_opt(&tx, "SELECT revoked_at IS NOT NULL FROM devices WHERE slug = $1 AND device_id = $2", &[&slug, &device]).await?
    else {
        return refuse(StatusCode::NOT_FOUND, "no such device");
    };
    if row.get::<_, bool>(0) {
        return refuse(StatusCode::CONFLICT, "that device is already signed out");
    }
    let now = clock::now();
    let Some(identity) = identity else {
        // no device keys in use: the device leaves the list, nothing to rotate
        db::execute(&tx, "DELETE FROM device_push WHERE slug = $1 AND device_id = $2", &[&slug, &device]).await?;
    db::execute(&tx, "UPDATE devices SET revoked_at = $3 WHERE slug = $1 AND device_id = $2", &[&slug, &device, &now]).await?;
        tx.commit().await?;
        hub.presence.wake_sync([slug.as_str()]);
        return ok(json!({ "signed_out": device, "rotated": false, "key_version": version }));
    };
    let (Some(new_key), Some(signature)) = (string(&body, "identity_key"), string(&body, "signature")) else {
        return refuse(StatusCode::UNPROCESSABLE_ENTITY, "signing a device out rotates the identity key: give identity_key, signature and sealed");
    };
    if verifying_key(new_key).is_none() {
        return refuse(StatusCode::UNPROCESSABLE_ENTITY, "identity_key must be an Ed25519 public key (32 bytes, base64url)");
    }
    if new_key == identity {
        return refuse(StatusCode::UNPROCESSABLE_ENTITY, "the new identity key must differ from the old one");
    }
    let next = version + 1;
    if !signed_by(&identity, &rotation_statement(&slug, device, new_key, next), signature) {
        return refuse(StatusCode::UNAUTHORIZED, "the rotation is not signed by this address's identity key");
    }
    let none = Map::new();
    let sealed = match body.get("sealed") {
        Some(Value::Object(m)) => m,
        None | Some(Value::Null) => &none,
        Some(_) => return refuse(StatusCode::UNPROCESSABLE_ENTITY, "sealed must map each remaining device_id to its sealed identity key"),
    };
    let remaining: Vec<String> = db::query(
        &tx,
        "SELECT device_id FROM devices WHERE slug = $1 AND device_id <> $2 AND public_key IS NOT NULL AND revoked_at IS NULL ORDER BY device_id",
        &[&slug, &device],
    )
    .await?
    .iter()
    .map(|r| r.get(0))
    .collect();
    for d in &remaining {
        match sealed.get(d) {
            Some(Value::String(s)) if s.len() <= SEALED_MAX && !s.is_empty() && !s.contains('\0') => {}
            _ => return refuse(StatusCode::UNPROCESSABLE_ENTITY, format!("seal the new identity key to every remaining device: {d} has none")),
        }
    }
    if let Some(stray) = sealed.keys().find(|k| !remaining.contains(k)) {
        return refuse(StatusCode::UNPROCESSABLE_ENTITY, format!("sealed names a device that is not enrolled here: {stray}"));
    }
    db::execute(&tx, "DELETE FROM device_push WHERE slug = $1 AND device_id = $2", &[&slug, &device]).await?;
    db::execute(&tx, "UPDATE devices SET revoked_at = $3 WHERE slug = $1 AND device_id = $2", &[&slug, &device, &now]).await?;
    db::execute(
        &tx,
        "UPDATE identities SET identity_key = $2, key_version = $3, shared_key_enabled = false WHERE slug = $1",
        &[&slug, &new_key, &next],
    )
    .await?;
    // copies of a superseded identity key are of no more use
    db::execute(&tx, "DELETE FROM identity_key_drops WHERE slug = $1 AND key_version < $2", &[&slug, &next]).await?;
    for d in &remaining {
        let s = sealed.get(d).and_then(Value::as_str).unwrap_or_default();
        db::execute(
            &tx,
            "INSERT INTO identity_key_drops (slug, device_id, key_version, sealed, created_at) VALUES ($1, $2, $3, $4, $5)",
            &[&slug, d, &next, &s, &now],
        )
        .await?;
    }
    tx.commit().await?;
    // the signed-out device's parked sync ends (and is refused); the others
    // see the new key version
    hub.presence.wake_sync([slug.as_str()]);
    ok(json!({ "signed_out": device, "rotated": true, "key_version": next }))
}
