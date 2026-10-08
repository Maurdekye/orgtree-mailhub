//! Registration, the multiplexed long poll, custody (ack), send, receipts
//! and the roster: v1 `app.py`, route for route.

use std::sync::Arc;
use std::time::{Duration, Instant};

use deadpool_postgres::GenericClient;
use http::StatusCode;
use serde_json::{json, Value};

use super::{ok, refuse, ApiError, ApiResult, Hub, Req};
use crate::auth;
use crate::clock;
use crate::db;
use crate::presence::Listener;
use crate::wire::{get_or, pg_text, py_iter, py_prefix, py_repr_str, py_str, py_strip, sqlite_text};

/// v1 BODY_MAX: a longer body is cut, not refused (Phase 1 keeps this).
pub const BODY_MAX: usize = 20000;
pub const MAX_FILES_PER_MESSAGE: usize = 10;
/// v1 POLL_CEILING: the longest a poll parks, whatever `wait` asks.
pub const POLL_CEILING: f64 = 55.0;
/// Messages one poll answer carries at most (v1 had no cap; the rest come
/// with the next poll, which returns at once while mail is queued).
pub const POLL_BATCH: i64 = 500;

const ENVELOPE_COLS: &str =
    "n, id, from_slug, to_slug, body, kind, thread_id, sent_at, received_at, state, fetched_at, delivered_at, read_at, attachments";

/// `^[a-z0-9][a-z0-9._-]{0,127}$`
pub fn valid_slug(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty()
        && b.len() <= 128
        && (b[0].is_ascii_lowercase() || b[0].is_ascii_digit())
        && b.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, b'.' | b'_' | b'-'))
}

/// The wire shape a recipient sees on poll (v1 `row_to_envelope`).
pub fn envelope(r: &tokio_postgres::Row) -> serde_json::Map<String, Value> {
    let mut m = serde_json::Map::new();
    m.insert("id".into(), json!(r.get::<_, String>("id")));
    m.insert("from".into(), json!(r.get::<_, String>("from_slug")));
    m.insert("to".into(), json!(r.get::<_, String>("to_slug")));
    m.insert("body".into(), json!(r.get::<_, String>("body")));
    m.insert("kind".into(), json!(r.get::<_, Option<String>>("kind")));
    m.insert("thread_id".into(), json!(r.get::<_, Option<String>>("thread_id")));
    m.insert("sent_at".into(), json!(r.get::<_, Option<String>>("sent_at")));
    m.insert("received_at".into(), json!(clock::iso(r.get("received_at"))));
    m.insert("attachments".into(), r.get::<_, Value>("attachments"));
    m
}

/// The roster every client is sent: all registered addresses with presence.
pub async fn roster(hub: &Hub, c: &impl GenericClient) -> ApiResult<Vec<Value>> {
    let rows = db::query(c, "SELECT slug, org_name, username, blurb, last_seen, kind FROM identities ORDER BY slug", &[]).await?;
    Ok(rows
        .iter()
        .map(|r| {
            let slug: String = r.get(0);
            let kind: String = r.get(5);
            json!({
                "slug": slug,
                "org_name": r.get::<_, String>(1),
                "username": r.get::<_, String>(2),
                "blurb": r.get::<_, String>(3),
                "online": hub.presence.online(&slug),
                "last_seen": r.get::<_, Option<chrono::DateTime<chrono::Utc>>>(4).map(clock::iso),
                "kind": if kind.is_empty() { "org".to_string() } else { kind },
            })
        })
        .collect())
}

/// v1 `_mark_seen`: the last authenticated call, in memory (presence) and
/// in the roster's `last_seen`.
pub async fn mark_seen(hub: &Hub, c: &impl GenericClient, slugs: &[String]) -> ApiResult<()> {
    if slugs.is_empty() {
        return Ok(());
    }
    hub.presence.mark_seen(slugs);
    let mut sorted: Vec<&str> = slugs.iter().map(String::as_str).collect();
    sorted.sort_unstable();
    sorted.dedup();
    db::execute(c, "UPDATE identities SET last_seen = $1 WHERE slug = ANY($2)", &[&clock::now(), &sorted]).await?;
    Ok(())
}

/// The authenticated slugs of a request. The header is read before the
/// returned future starts, so the future does not borrow the request (whose
/// body is not `Sync`).
pub(super) fn authed<'a>(hub: &'a Hub, req: &Req) -> impl std::future::Future<Output = ApiResult<Vec<String>>> + Send + 'a {
    let pairs = auth::pairs(req.auth_header());
    async move {
        if pairs.is_empty() {
            return Ok(Vec::new());
        }
        let c = hub.db.get().await?;
        Ok(auth::authenticate(&c, &pairs).await?)
    }
}

fn str_field(body: &serde_json::Map<String, Value>, key: &str) -> String {
    get_or(body, key).map(py_str).unwrap_or_default()
}

fn crash<T>(what: &str) -> ApiResult<T> {
    Err(ApiError::Internal(anyhow::anyhow!("{what}")))
}

// ------------------------------------------------------------------ register

pub async fn register(hub: &Arc<Hub>, req: &mut Req) -> ApiResult {
    let body = req.json_object().await?;
    let slug = py_strip(&str_field(&body, "slug")).to_string();
    if !valid_slug(&slug) {
        return refuse(StatusCode::UNPROCESSABLE_ENTITY, "malformed slug");
    }
    // the secret arrives in the auth header, never in the body or URL
    let secret = auth::register_secret(req.auth_header(), &slug);
    if secret.is_empty() {
        return refuse(
            StatusCode::UNAUTHORIZED,
            "registration requires X-Org-Auth: <slug>:<secret> for the slug being registered",
        );
    }
    let fp = auth::fingerprint(&secret);
    let kind = if str_field(&body, "kind") == "chat" { "chat" } else { "org" };
    let org_name = pg_text(str_field(&body, "org_name"));
    let username = pg_text(str_field(&body, "username"));
    let blurb = pg_text(str_field(&body, "blurb"));
    let c = hub.db.get().await?;
    for _ in 0..3 {
        // first write wins the address
        let now = clock::now();
        let inserted = db::execute(
            &c,
            "INSERT INTO identities (slug, fingerprint, org_name, username, blurb, registered_at, last_seen, kind)
             VALUES ($1, $2, $3, $4, $5, $6, $6, $7) ON CONFLICT (slug) DO NOTHING",
            &[&slug, &fp, &org_name, &username, &blurb, &now, &kind],
        )
        .await?;
        if inserted == 1 {
            break;
        }
        let Some(row) = db::query_opt(&c, "SELECT fingerprint FROM identities WHERE slug = $1", &[&slug]).await? else {
            continue; // removed in between: claim it again
        };
        let stored: String = row.get(0);
        if !auth::ct_eq(stored.as_bytes(), fp.as_bytes()) {
            return refuse(StatusCode::FORBIDDEN, "slug is owned by another identity");
        }
        // re-registration refreshes the display fields (never the kind)
        db::execute(
            &c,
            "UPDATE identities SET org_name = $2, username = $3, blurb = $4 WHERE slug = $1",
            &[&slug, &org_name, &username, &blurb],
        )
        .await?;
        break;
    }
    mark_seen(hub, &c, std::slice::from_ref(&slug)).await?;
    let roster = roster(hub, &c).await?;
    ok(json!({ "ok": true, "name": hub.cfg.hub_name, "retention_days": hub.cfg.retention_days, "roster": roster }))
}

// ---------------------------------------------------------------- unregister

/// The polite exit: an authenticated client removes its own roster row(s).
/// Queued mail ages out through retention; the same secret re-mints the same
/// address later.
pub async fn unregister(hub: &Arc<Hub>, req: &mut Req) -> ApiResult {
    let slugs = authed(hub, req).await?;
    if slugs.is_empty() {
        return refuse(StatusCode::UNAUTHORIZED, "no valid org credentials in X-Org-Auth");
    }
    let c = hub.db.get().await?;
    db::execute(&c, "DELETE FROM identities WHERE slug = ANY($1)", &[&slugs]).await?;
    for s in &slugs {
        hub.presence.forget(s);
    }
    ok(json!({ "unregistered": slugs }))
}

// ---------------------------------------------------------------------- poll

/// One long poll carries, for every org whose credentials it presents: the
/// queued mail (until acked), the receipts its senders are owed (each pushed
/// once), and the roster.
pub async fn poll(hub: &Arc<Hub>, req: &mut Req) -> ApiResult {
    // query parameters are validated before the handler body, as in FastAPI
    let wait = req.query_float("wait", 25.0)?;
    let slugs = authed(hub, req).await?;
    if slugs.is_empty() {
        return refuse(StatusCode::UNAUTHORIZED, "no valid org credentials in X-Org-Auth");
    }
    let wait = if wait.is_nan() { 0.0 } else { wait.clamp(0.0, POLL_CEILING) };
    let deadline = Instant::now() + Duration::from_secs_f64(wait);
    let _parked = hub.presence.park(&slugs);
    loop {
        let slots = hub.presence.slots_for(&slugs);
        let listener = Listener::new(&slots);
        let (messages, receipts) = poll_check(hub, &slugs).await?;
        if !messages.is_empty() || !receipts.is_empty() || Instant::now() >= deadline {
            return poll_answer(hub, &slugs, messages, receipts).await;
        }
        tokio::select! {
            _ = listener.wait() => {}
            _ = tokio::time::sleep_until(deadline.into()) => return poll_answer(hub, &slugs, Vec::new(), Vec::new()).await,
            _ = hub.shutdown.cancelled() => return poll_answer(hub, &slugs, Vec::new(), Vec::new()).await,
        }
    }
}

async fn poll_check(hub: &Hub, slugs: &[String]) -> ApiResult<(Vec<Value>, Vec<Value>)> {
    let c = hub.db.get().await?;
    let msgs = db::query(
        &c,
        &format!(
            "SELECT {ENVELOPE_COLS} FROM messages WHERE state = 'queued' AND to_slug = ANY($1)
             ORDER BY received_at, n LIMIT {POLL_BATCH}"
        ),
        &[&slugs],
    )
    .await?;
    // claimed and marked pushed in one statement: two parked polls for the
    // same sender can never both answer with the same receipt. Loss-tolerant
    // by design, as in v1: a dropped response self-heals at the next change.
    let owed = db::query(
        &c,
        "WITH owed AS (
            UPDATE messages SET receipts_pushed = true
             WHERE NOT receipts_pushed AND from_slug = ANY($1)
             RETURNING n, id, from_slug, fetched_at, delivered_at, read_at)
         SELECT id, fetched_at, delivered_at, read_at FROM owed ORDER BY from_slug, n",
        &[&slugs],
    )
    .await?;
    let messages = msgs.iter().map(|r| Value::Object(envelope(r))).collect();
    let receipts = owed
        .iter()
        .map(|r| {
            let fetched: Option<chrono::DateTime<chrono::Utc>> = r.get(1);
            let delivered: Option<String> = r.get(2);
            let read: Option<String> = r.get(3);
            let state = if read.as_deref().map(|s| !s.is_empty()).unwrap_or(false) {
                "read"
            } else if delivered.as_deref().map(|s| !s.is_empty()).unwrap_or(false) {
                "delivered"
            } else if fetched.is_some() {
                "fetched"
            } else {
                "received"
            };
            json!({
                "id": r.get::<_, String>(0),
                "state": state,
                "fetched_at": fetched.map(clock::iso),
                "delivered_at": delivered,
                "read_at": read,
            })
        })
        .collect();
    Ok((messages, receipts))
}

async fn poll_answer(hub: &Hub, slugs: &[String], messages: Vec<Value>, receipts: Vec<Value>) -> ApiResult {
    let c = hub.db.get().await?;
    mark_seen(hub, &c, slugs).await?;
    let roster = roster(hub, &c).await?;
    ok(json!({ "name": hub.cfg.hub_name, "messages": messages, "receipts": receipts, "roster": roster }))
}

// ----------------------------------------------------------------------- ack

/// Custody transfer: the recipient persisted the mail. Only the addressee may
/// ack; a second ack is a no-op.
pub async fn ack(hub: &Arc<Hub>, req: &mut Req) -> ApiResult {
    let body = req.json_object().await?;
    let Ok(items) = py_iter(body.get("ids")) else { return crash("ack: `ids` is not iterable") };
    // an id PostgreSQL cannot hold matches no stored message
    let ids: Vec<String> = items.iter().map(py_str).filter(|i| !i.contains('\0')).collect();
    let slugs = authed(hub, req).await?;
    if slugs.is_empty() {
        return refuse(StatusCode::UNAUTHORIZED, "no valid org credentials");
    }
    let c = hub.db.get().await?;
    let rows = db::query(
        &c,
        "UPDATE messages SET state = 'fetched', fetched_at = $1, receipts_pushed = false
          WHERE id = ANY($2) AND state = 'queued' AND to_slug = ANY($3)
          RETURNING from_slug",
        &[&clock::now(), &ids, &slugs],
    )
    .await?;
    mark_seen(hub, &c, &slugs).await?;
    let senders: Vec<String> = rows.iter().map(|r| r.get(0)).collect();
    hub.presence.wake(senders.iter().map(String::as_str));
    ok(json!({ "acked": rows.len() }))
}

// ---------------------------------------------------------------------- send

/// Idempotent on the client-minted id: a retry answers `duplicate: true`
/// with the ORIGINAL `received_at`, and the first payload wins. The 200 IS
/// the "received" receipt.
pub async fn send(hub: &Arc<Hub>, req: &mut Req) -> ApiResult {
    let body = req.json_object().await?;
    let to = py_strip(&str_field(&body, "to")).to_string();
    let slugs = authed(hub, req).await?;
    let frm = get_or(&body, "from").map(py_str).unwrap_or_else(|| slugs.first().cloned().unwrap_or_default());
    if !slugs.contains(&frm) {
        return refuse(StatusCode::UNAUTHORIZED, "sender credentials required");
    }
    let mut c = hub.db.get().await?;
    let tx = c.transaction().await?;
    // the recipient's row is locked for the length of this short
    // transaction: per recipient, commit order is received_at order
    let known = !to.contains('\0')
        && db::query_opt(&tx, "SELECT 1 FROM identities WHERE slug = $1 FOR UPDATE", &[&to]).await?.is_some();
    if !known {
        return refuse(StatusCode::UNPROCESSABLE_ENTITY, format!("no org registered as {}", py_repr_str(&to)));
    }
    let mid = get_or(&body, "id").map(py_str).unwrap_or_else(|| uuid::Uuid::new_v4().simple().to_string());
    if mid.contains('\0') {
        // v1 stored it; PostgreSQL cannot, and stripping could collide
        return refuse(StatusCode::UNPROCESSABLE_ENTITY, "message id contains a NUL character");
    }
    let Ok(items) = py_iter(body.get("attachments")) else { return crash("send: `attachments` is not iterable") };
    let att_ids: Vec<String> = items.iter().map(py_str).collect();
    if att_ids.len() > MAX_FILES_PER_MESSAGE {
        return refuse(StatusCode::UNPROCESSABLE_ENTITY, format!("at most {MAX_FILES_PER_MESSAGE} attachments"));
    }
    let lookup: Vec<String> = att_ids.iter().filter(|a| !a.contains('\0')).cloned().collect();
    let rows = db::query(
        &tx,
        "SELECT id, name, bytes, owner_slug, message_id FROM attachments WHERE id = ANY($1) ORDER BY id FOR UPDATE",
        &[&lookup],
    )
    .await?;
    let found: std::collections::HashMap<String, &tokio_postgres::Row> = rows.iter().map(|r| (r.get::<_, String>(0), r)).collect();
    let mut metas = Vec::with_capacity(att_ids.len());
    for aid in &att_ids {
        let Some(row) = found.get(aid).filter(|r| r.get::<_, String>(3) == frm) else {
            return refuse(StatusCode::UNPROCESSABLE_ENTITY, format!("unknown attachment {}", py_repr_str(aid)));
        };
        if let Some(bound) = row.get::<_, Option<String>>(4) {
            if !bound.is_empty() && bound != mid {
                return refuse(StatusCode::UNPROCESSABLE_ENTITY, format!("attachment {} already bound", py_repr_str(aid)));
            }
        }
        metas.push(json!({ "id": row.get::<_, String>(0), "name": row.get::<_, String>(1), "bytes": row.get::<_, i64>(2) }));
    }
    let text = pg_text(py_prefix(&str_field(&body, "body"), BODY_MAX).to_string());
    let (Ok(kind), Ok(thread_id), Ok(sent_at)) =
        (sqlite_text(body.get("kind")), sqlite_text(body.get("thread_id")), sqlite_text(body.get("sent_at")))
    else {
        return crash("send: a list or object where text belongs");
    };
    let (kind, thread_id, sent_at) = (kind.map(pg_text), thread_id.map(pg_text), sent_at.map(pg_text));
    let received = clock::now();
    let inserted = db::query_opt(
        &tx,
        "INSERT INTO messages (id, from_slug, to_slug, body, kind, thread_id, sent_at, received_at, attachments)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9) ON CONFLICT (id) DO NOTHING RETURNING received_at",
        &[&mid, &frm, &to, &text, &kind, &thread_id, &sent_at, &received, &Value::Array(metas)],
    )
    .await?;
    let fresh = inserted.is_some();
    let received = if fresh {
        db::execute(&tx, "UPDATE attachments SET message_id = $1 WHERE id = ANY($2)", &[&mid, &lookup]).await?;
        received
    } else {
        db::query_one(&tx, "SELECT received_at FROM messages WHERE id = $1", &[&mid]).await?.get(0)
    };
    tx.commit().await?;
    mark_seen(hub, &c, std::slice::from_ref(&frm)).await?;
    if fresh {
        hub.presence.wake([to.as_str()]);
    }
    ok(json!({ "id": mid, "received_at": clock::iso(received), "duplicate": !fresh }))
}

// ------------------------------------------------------------------ receipts

/// `delivered` and `read` from the recipient's side. Each is written only
/// while unset (the ladder never moves backwards) and only by the addressee;
/// other states are ignored, not refused.
pub async fn receipts(hub: &Arc<Hub>, req: &mut Req) -> ApiResult {
    let body = req.json_object().await?;
    let slugs = authed(hub, req).await?;
    if slugs.is_empty() {
        return refuse(StatusCode::UNAUTHORIZED, "no valid org credentials");
    }
    let Ok(items) = py_iter(body.get("receipts")) else { return crash("receipts: `receipts` is not iterable") };
    let mut c = hub.db.get().await?;
    let tx = c.transaction().await?;
    let mut recorded = 0usize;
    let mut senders: Vec<String> = Vec::new();
    for r in &items {
        let Value::Object(r) = r else { return crash("receipts: an entry is not an object") };
        let mid = str_field(r, "id");
        if mid.contains('\0') {
            continue; // matches no stored message
        }
        let sql = match str_field(r, "state").as_str() {
            "delivered" => {
                "UPDATE messages SET delivered_at = $1, receipts_pushed = false
                  WHERE id = $2 AND delivered_at IS NULL AND to_slug = ANY($3) RETURNING from_slug"
            }
            "read" => {
                "UPDATE messages SET read_at = $1, receipts_pushed = false
                  WHERE id = $2 AND read_at IS NULL AND to_slug = ANY($3) RETURNING from_slug"
            }
            _ => continue,
        };
        let at = pg_text(get_or(r, "at").map(py_str).unwrap_or_else(clock::now_iso));
        let rows = db::query(&tx, sql, &[&at, &mid, &slugs]).await?;
        recorded += rows.len();
        senders.extend(rows.iter().map(|r| r.get::<_, String>(0)));
    }
    tx.commit().await?;
    mark_seen(hub, &c, &slugs).await?;
    hub.presence.wake(senders.iter().map(String::as_str));
    ok(json!({ "recorded": recorded }))
}

// -------------------------------------------------------------------- roster

pub async fn roster_route(hub: &Arc<Hub>, req: &mut Req) -> ApiResult {
    let slugs = authed(hub, req).await?;
    if slugs.is_empty() {
        return refuse(StatusCode::UNAUTHORIZED, "no valid org credentials");
    }
    let c = hub.db.get().await?;
    mark_seen(hub, &c, &slugs).await?;
    let roster = roster(hub, &c).await?;
    ok(json!({ "name": hub.cfg.hub_name, "roster": roster }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_grammar() {
        for good in ["a", "zz1.tester.abc123", "0-x_y.z", &"a".repeat(128)] {
            assert!(valid_slug(good), "{good}");
        }
        for bad in ["", " ", "UPPER.case.aaaaaa", ".leading.dot", "-leading-dash", "has space", &"a".repeat(129), "emoji.😀.a"] {
            assert!(!valid_slug(bad), "{bad}");
        }
    }
}
