//! Resumable uploads, and the relay that links a new device through a hub
//! (Phase 2, slice 6).
//!
//! An upload can be sent in pieces and resumed after a dropped connection:
//! `POST /api/uploads` opens it with its size, `PATCH /api/uploads/{id}
//! ?offset=N` appends the next bytes (streamed to disk, flushed before the
//! new offset is confirmed), `GET` tells where it stands, `DELETE` cancels.
//! Complete, it is an ordinary attachment with the same id. A day without a
//! byte and it is swept. (`POST /api/attachments` keeps working for whole
//! files, and downloads already resume with `Range`.)
//!
//! The link relay carries a sealed payload — the identity key, hub list and
//! profile, encrypted by the clients — from a signed-in device to a new one
//! under a one-time code: `POST /api/link/put` (signed in), `POST
//! /api/link/take` (no sign-in: the new device has no key yet; taken once),
//! `POST /api/link/cancel`. Codes travel in bodies, never in paths, so no
//! request log ever holds one; the hub keeps only their sha256.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::StreamExt;
use http::StatusCode;
use serde_json::{json, Value};
use tokio::io::{AsyncSeekExt, AsyncWriteExt};

use super::mail::{authed, caller, mark_seen, one_address, POLL_CEILING};
use super::{ok, refuse, ApiError, ApiResult, Hub, Req};
use crate::blobs;
use crate::clock;
use crate::db;
use crate::wire::{pg_text, py_basename, py_prefix};

/// An upload that receives nothing for this long is swept.
pub const UPLOAD_IDLE_HOURS: i64 = 24;
/// How long a link payload waits to be taken.
pub const LINK_MINUTES: i64 = 10;
pub const LINK_SEALED_MAX: usize = 1024 * 1024;

fn partial(hub: &Hub, id: &str) -> PathBuf {
    hub.cfg.uploads_dir().join(format!("{id}.part"))
}

/// Upload ids are the hub's own (32 hex characters); nothing else names one.
fn valid_id(id: &str) -> bool {
    id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

/// One writer per upload at a time (in this process: one hub per database).
struct Writing<'a> {
    hub: &'a Hub,
    id: String,
}

impl<'a> Writing<'a> {
    fn claim(hub: &'a Hub, id: &str) -> ApiResult<Writing<'a>> {
        if !hub.writing.pin().insert(id.to_string()) {
            return refuse(StatusCode::CONFLICT, "another request is writing to this upload");
        }
        Ok(Writing { hub, id: id.to_string() })
    }
}

impl Drop for Writing<'_> {
    fn drop(&mut self) {
        self.hub.writing.pin().remove(&self.id);
    }
}

fn too_large(limit: u64) -> ApiError {
    ApiError::Body(
        StatusCode::PAYLOAD_TOO_LARGE,
        json!({ "detail": format!("attachment exceeds hub limit of {limit} bytes"), "max_attachment_bytes": limit }),
    )
}

fn expires(updated: chrono::DateTime<chrono::Utc>) -> String {
    clock::iso(updated + chrono::Duration::hours(UPLOAD_IDLE_HOURS))
}

/// `POST /api/uploads {bytes, name?, sha256?, slug?}`: open an upload.
#[tracing::instrument(level = "debug", skip_all, ret(level = "debug"), err(level = "debug", Debug))]
pub async fn start(hub: &Arc<Hub>, req: &mut Req) -> ApiResult {
    let asked = req.query("slug").map(|s| Value::String(s.to_string()));
    let body = req.json_object_strict().await?;
    let slugs = authed(hub, req).await?;
    if slugs.is_empty() {
        return refuse(StatusCode::UNAUTHORIZED, "no valid org credentials");
    }
    let owner = one_address(&slugs, body.get("slug").or(asked.as_ref()))?;
    let total = match body.get("bytes").and_then(Value::as_i64) {
        Some(t) if t >= 0 => t,
        _ => return refuse(StatusCode::UNPROCESSABLE_ENTITY, "bytes must be the upload's size: a whole number"),
    };
    let name = match body.get("name") {
        None | Some(Value::Null) => "file".to_string(),
        Some(Value::String(n)) => {
            let n = py_prefix(py_basename(n), 255);
            pg_text(if n.is_empty() { "file".to_string() } else { n.to_string() })
        }
        Some(_) => return refuse(StatusCode::UNPROCESSABLE_ENTITY, "name must be a string"),
    };
    let sha256 = match body.get("sha256") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) if s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit()) => Some(s.to_ascii_lowercase()),
        Some(_) => return refuse(StatusCode::UNPROCESSABLE_ENTITY, "sha256 must be 64 hexadecimal characters"),
    };
    let Ok(limit) = blobs::attachment_limit(&hub.cfg) else {
        return refuse(StatusCode::SERVICE_UNAVAILABLE, "attachment limit configuration is invalid");
    };
    if total as u64 > limit {
        return Err(too_large(limit));
    }
    let id = uuid::Uuid::new_v4().simple().to_string();
    tokio::fs::create_dir_all(hub.cfg.uploads_dir()).await?;
    tokio::fs::OpenOptions::new().write(true).create_new(true).open(partial(hub, &id)).await?;
    let now = clock::now();
    let c = hub.db.get().await?;
    db::execute(
        &c,
        "INSERT INTO uploads (id, owner_slug, name, total, received, sha256, created_at, updated_at) VALUES ($1, $2, $3, $4, 0, $5, $6, $6)",
        &[&id, &owner, &name, &total, &sha256, &now],
    )
    .await?;
    mark_seen(hub, &c, std::slice::from_ref(&owner)).await?;
    drop(c);
    if total == 0 {
        return ok(finish(hub, &id).await?);
    }
    ok(json!({ "id": id, "name": name, "bytes": total, "offset": 0, "complete": false, "expires_at": expires(now) }))
}

/// `PATCH /api/uploads/{id}?offset=N[&slug=]`: the bytes from N on.
#[tracing::instrument(level = "debug", skip(hub, req), ret(level = "debug"), err(level = "debug", Debug))]
pub async fn append(hub: &Arc<Hub>, req: &mut Req, id: &str) -> ApiResult {
    let offset = match req.query("offset").map(|o| o.trim().parse::<i64>()) {
        Some(Ok(o)) if o >= 0 => o,
        _ => return refuse(StatusCode::UNPROCESSABLE_ENTITY, "offset is required: where these bytes start, a whole number"),
    };
    let me = caller(hub, req).await?;
    if !valid_id(id) {
        return refuse(StatusCode::NOT_FOUND, "no such upload");
    }
    let _writing = Writing::claim(hub, id)?;
    let c = hub.db.get().await?;
    let row = db::query_opt(&c, "SELECT total, received FROM uploads WHERE id = $1 AND owner_slug = $2", &[&id, &me]).await?;
    let Some(row) = row else { return refuse(StatusCode::NOT_FOUND, "no such upload") };
    let (total, received): (i64, i64) = (row.get(0), row.get(1));
    drop(c);
    if offset != received {
        return Err(ApiError::Body(
            StatusCode::CONFLICT,
            json!({ "detail": format!("this upload is at offset {received}"), "offset": received }),
        ));
    }
    let path = partial(hub, id);
    let mut file = match tokio::fs::OpenOptions::new().write(true).open(&path).await {
        Ok(f) => f,
        Err(_) => return refuse(StatusCode::GONE, "the upload's bytes are gone; start it again"),
    };
    // bytes past the confirmed offset (a write cut short) are dropped
    file.set_len(received as u64).await?;
    file.seek(std::io::SeekFrom::Start(received as u64)).await?;
    let room = (total - received) as u64;
    let mut written: u64 = 0;
    let mut confirmed: u64 = 0;
    let mut overflow = false;
    let mut cut_short = false;
    let mut stream = std::mem::take(&mut req.body).into_data_stream();
    while let Some(chunk) = stream.next().await {
        let Ok(chunk) = chunk else {
            // the client went away: keep what arrived, so it can resume there
            cut_short = true;
            break;
        };
        let fits = (room - written).min(chunk.len() as u64) as usize;
        file.write_all(&chunk[..fits]).await?;
        written += fits as u64;
        if fits < chunk.len() {
            overflow = true;
            break;
        }
        // confirmed as it goes, so a connection dropped mid-piece loses at
        // most one checkpoint's worth (the request may never finish)
        if written - confirmed >= CHECKPOINT {
            confirm(hub, id, &mut file, received + written as i64).await?;
            confirmed = written;
        }
    }
    let now_at = received + written as i64;
    confirm(hub, id, &mut file, now_at).await?;
    drop(file);
    if overflow {
        return Err(ApiError::Body(
            StatusCode::BAD_REQUEST,
            json!({ "detail": format!("more bytes than the upload's size ({total})"), "offset": now_at }),
        ));
    }
    if cut_short {
        return Err(ApiError::Http(StatusCode::BAD_REQUEST, "upload interrupted".into()));
    }
    if now_at == total {
        return ok(finish(hub, id).await?);
    }
    ok(json!({ "id": id, "bytes": total, "offset": now_at, "complete": false }))
}

/// Bytes written to an upload since its last checkpoint.
const CHECKPOINT: u64 = 8 * 1024 * 1024;

/// Make what was written durable, then record the new offset.
async fn confirm(hub: &Hub, id: &str, file: &mut tokio::fs::File, at: i64) -> ApiResult<()> {
    file.flush().await?;
    file.sync_data().await?;
    let c = hub.db.get().await?;
    db::execute(&c, "UPDATE uploads SET received = $2, updated_at = $3 WHERE id = $1", &[&id, &at, &clock::now()]).await?;
    Ok(())
}

/// Every byte is in: check it against its sha256 (when given), move it among
/// the attachments, and close the upload.
async fn finish(hub: &Hub, id: &str) -> ApiResult<Value> {
    let c = hub.db.get().await?;
    let row = db::query_one(&c, "SELECT owner_slug, name, total, sha256 FROM uploads WHERE id = $1", &[&id]).await?;
    let (owner, name, total, sha): (String, String, i64, Option<String>) = (row.get(0), row.get(1), row.get(2), row.get(3));
    let from = partial(hub, id);
    let dir = hub.cfg.blob_dir();
    let Some(to) = blobs::blob_path(&dir, id) else { return refuse(StatusCode::NOT_FOUND, "no such upload") };
    // a hub stopped between the move and the row finds the file moved
    if tokio::fs::metadata(&from).await.is_ok() {
        if let Some(want) = &sha {
            let got = sha256_file(&from).await?;
            if &got != want {
                // the client starts it over from nothing
                tokio::fs::OpenOptions::new().write(true).open(&from).await?.set_len(0).await?;
                db::execute(&c, "UPDATE uploads SET received = 0, updated_at = $2 WHERE id = $1", &[&id, &clock::now()]).await?;
                return Err(ApiError::Body(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    json!({ "detail": "the bytes received do not match the upload's sha256; send them again from 0", "offset": 0 }),
                ));
            }
        }
        tokio::fs::create_dir_all(&dir).await?;
        tokio::fs::rename(&from, &to).await?;
    }
    let mut c = c;
    let tx = c.transaction().await?;
    db::execute(
        &tx,
        "INSERT INTO attachments (id, owner_slug, name, bytes, created_at) VALUES ($1, $2, $3, $4, $5) ON CONFLICT (id) DO NOTHING",
        &[&id, &owner, &name, &total, &clock::now()],
    )
    .await?;
    db::execute(&tx, "DELETE FROM uploads WHERE id = $1", &[&id]).await?;
    tx.commit().await?;
    Ok(json!({ "id": id, "name": name, "bytes": total, "offset": total, "complete": true }))
}

async fn sha256_file(path: &std::path::Path) -> std::io::Result<String> {
    use sha2::Digest;
    use tokio::io::AsyncReadExt;
    let mut f = tokio::fs::File::open(path).await?;
    let mut h = sha2::Sha256::new();
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        let n = f.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(hex::encode(h.finalize()))
}

/// `GET /api/uploads/{id}[?slug=]`: where an upload stands (also once it is
/// complete, while its attachment exists).
#[tracing::instrument(level = "debug", skip(hub, req), ret(level = "debug"), err(level = "debug", Debug))]
pub async fn status(hub: &Arc<Hub>, req: &mut Req, id: &str) -> ApiResult {
    let me = caller(hub, req).await?;
    if !valid_id(id) {
        return refuse(StatusCode::NOT_FOUND, "no such upload");
    }
    let c = hub.db.get().await?;
    if let Some(r) =
        db::query_opt(&c, "SELECT name, total, received, updated_at FROM uploads WHERE id = $1 AND owner_slug = $2", &[&id, &me]).await?
    {
        let (name, total, received): (String, i64, i64) = (r.get(0), r.get(1), r.get(2));
        drop(c);
        if received == total {
            // every byte arrived but the hub stopped before closing it
            let _writing = Writing::claim(hub, id)?;
            return ok(finish(hub, id).await?);
        }
        return ok(json!({ "id": id, "name": name, "bytes": total, "offset": received, "complete": false, "expires_at": expires(r.get(3)) }));
    }
    if let Some(r) = db::query_opt(&c, "SELECT name, bytes FROM attachments WHERE id = $1 AND owner_slug = $2", &[&id, &me]).await? {
        let total: i64 = r.get(1);
        return ok(json!({ "id": id, "name": r.get::<_, String>(0), "bytes": total, "offset": total, "complete": true }));
    }
    refuse(StatusCode::NOT_FOUND, "no such upload")
}

/// `DELETE /api/uploads/{id}[?slug=]`: give up an upload in progress.
#[tracing::instrument(level = "debug", skip(hub, req), ret(level = "debug"), err(level = "debug", Debug))]
pub async fn cancel(hub: &Arc<Hub>, req: &mut Req, id: &str) -> ApiResult {
    let me = caller(hub, req).await?;
    if !valid_id(id) {
        return refuse(StatusCode::NOT_FOUND, "no such upload");
    }
    let _writing = Writing::claim(hub, id)?;
    let c = hub.db.get().await?;
    let n = db::execute(&c, "DELETE FROM uploads WHERE id = $1 AND owner_slug = $2", &[&id, &me]).await?;
    if n == 0 {
        return refuse(StatusCode::NOT_FOUND, "no such upload");
    }
    let _ = tokio::fs::remove_file(partial(hub, id)).await;
    ok(json!({ "cancelled": true }))
}

/// Uploads idle for a day, and link payloads past their ten minutes.
pub async fn sweep(hub: &Hub) -> anyhow::Result<u64> {
    let c = hub.db.get().await?;
    let cut = clock::now() - chrono::Duration::hours(UPLOAD_IDLE_HOURS);
    let rows = db::query(&c, "DELETE FROM uploads WHERE updated_at < $1 RETURNING id", &[&cut]).await?;
    for r in &rows {
        // one being written right now is the writer's to finish
        let id: String = r.get(0);
        if !hub.writing.pin().contains(&id) {
            let _ = tokio::fs::remove_file(partial(hub, &id)).await;
        }
    }
    db::execute(&c, "DELETE FROM link_relay WHERE expires_at < $1", &[&clock::now()]).await?;
    Ok(rows.len() as u64)
}

// ------------------------------------------------------------- link relay

fn code_hash(v: Option<&Value>) -> ApiResult<String> {
    match v {
        Some(Value::String(c)) if (6..=128).contains(&c.chars().count()) && !c.contains('\0') => Ok(crate::auth::fingerprint(c)),
        _ => refuse(StatusCode::UNPROCESSABLE_ENTITY, "code must be 6 to 128 characters"),
    }
}

/// `POST /api/link/put {code, sealed, slug?}`: leave a sealed payload for a
/// new device, for ten minutes.
#[tracing::instrument(level = "debug", skip_all, ret(level = "debug"), err(level = "debug", Debug))]
pub async fn link_put(hub: &Arc<Hub>, req: &mut Req) -> ApiResult {
    let body = req.json_object_strict().await?;
    let slugs = authed(hub, req).await?;
    if slugs.is_empty() {
        return refuse(StatusCode::UNAUTHORIZED, "no valid org credentials");
    }
    let owner = one_address(&slugs, body.get("slug"))?;
    let hash = code_hash(body.get("code"))?;
    let sealed = match body.get("sealed") {
        Some(Value::String(s)) if s.len() <= LINK_SEALED_MAX && !s.contains('\0') => s.clone(),
        Some(Value::String(s)) if s.len() > LINK_SEALED_MAX => {
            return refuse(StatusCode::PAYLOAD_TOO_LARGE, format!("sealed is larger than {LINK_SEALED_MAX} bytes"));
        }
        _ => return refuse(StatusCode::UNPROCESSABLE_ENTITY, "sealed must be a string (the encrypted payload)"),
    };
    let now = clock::now();
    let until = now + chrono::Duration::minutes(LINK_MINUTES);
    let c = hub.db.get().await?;
    // a code still waiting is not taken over; an expired one is
    let stored = db::query_opt(
        &c,
        "INSERT INTO link_relay (code_hash, owner_slug, sealed, created_at, expires_at) VALUES ($1, $2, $3, $4, $5)
         ON CONFLICT (code_hash) DO UPDATE SET owner_slug = EXCLUDED.owner_slug, sealed = EXCLUDED.sealed,
                created_at = EXCLUDED.created_at, expires_at = EXCLUDED.expires_at
          WHERE link_relay.expires_at < $4
         RETURNING 1",
        &[&hash, &owner, &sealed, &now, &until],
    )
    .await?;
    if stored.is_none() {
        return refuse(StatusCode::CONFLICT, "that code is already in use: make a new one");
    }
    mark_seen(hub, &c, std::slice::from_ref(&owner)).await?;
    hub.presence.link_left();
    ok(json!({ "ok": true, "expires_at": clock::iso(until) }))
}

/// `POST /api/link/take {code, wait?}`: the new device collects the payload
/// (once). No sign-in. Waits up to `wait` seconds (default 25, at most 55)
/// for it; 404 when nothing is there by then.
#[tracing::instrument(level = "debug", skip_all, ret(level = "debug"), err(level = "debug", Debug))]
pub async fn link_take(hub: &Arc<Hub>, req: &mut Req) -> ApiResult {
    let body = req.json_object_strict().await?;
    let hash = code_hash(body.get("code"))?;
    let wait = match body.get("wait") {
        None | Some(Value::Null) => 25.0,
        Some(Value::Number(n)) => n.as_f64().unwrap_or(0.0),
        Some(_) => return refuse(StatusCode::UNPROCESSABLE_ENTITY, "wait must be a number of seconds"),
    };
    let wait = if wait.is_nan() { 0.0 } else { wait.clamp(0.0, POLL_CEILING) };
    let deadline = Instant::now() + Duration::from_secs_f64(wait);
    loop {
        let waiter = hub.presence.link_waiter();
        let c = hub.db.get().await?;
        let row = db::query_opt(
            &c,
            "DELETE FROM link_relay WHERE code_hash = $1 AND expires_at > $2 RETURNING sealed, owner_slug",
            &[&hash, &clock::now()],
        )
        .await?;
        drop(c);
        if let Some(r) = row {
            return ok(json!({ "sealed": r.get::<_, String>(0), "from": r.get::<_, String>(1) }));
        }
        if Instant::now() >= deadline {
            return refuse(StatusCode::NOT_FOUND, "nothing waits under that code");
        }
        tokio::select! {
            _ = waiter => {}
            _ = tokio::time::sleep_until(deadline.into()) => {}
            _ = hub.shutdown.cancelled() => return refuse(StatusCode::NOT_FOUND, "nothing waits under that code"),
        }
    }
}

/// `POST /api/link/cancel {code, slug?}`: take back a payload not yet taken.
#[tracing::instrument(level = "debug", skip_all, ret(level = "debug"), err(level = "debug", Debug))]
pub async fn link_cancel(hub: &Arc<Hub>, req: &mut Req) -> ApiResult {
    let body = req.json_object_strict().await?;
    let slugs = authed(hub, req).await?;
    if slugs.is_empty() {
        return refuse(StatusCode::UNAUTHORIZED, "no valid org credentials");
    }
    let owner = one_address(&slugs, body.get("slug"))?;
    let hash = code_hash(body.get("code"))?;
    let c = hub.db.get().await?;
    let n = db::execute(&c, "DELETE FROM link_relay WHERE code_hash = $1 AND owner_slug = $2", &[&hash, &owner]).await?;
    ok(json!({ "cancelled": n > 0 }))
}
