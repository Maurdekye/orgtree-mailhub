//! Registration, the multiplexed long poll, custody (ack), send, receipts
//! and the roster: v1 `app.py`, route for route.

use std::sync::Arc;
use std::time::{Duration, Instant};

use deadpool_postgres::GenericClient;
use http::StatusCode;
use serde_json::{json, Value};

use super::sync;
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

pub const ENVELOPE_COLS: &str =
    "n, id, from_slug, to_slug, body, kind, thread_id, sent_at, received_at, state, fetched_at, delivered_at, read_at, attachments, reply_to";

/// The client kinds an address registers as. v1 knew org and chat (and
/// stored anything else as org); v2 adds person (G2). Fixed at the first
/// registration.
pub fn register_kind(asked: &str) -> &'static str {
    match asked {
        "chat" => "chat",
        "person" => "person",
        _ => "org",
    }
}

/// The longest `reply_to` a send may carry (message ids are client-minted;
/// this only stops a reference from being used as a payload).
pub const REPLY_TO_MAX: usize = 4096;
pub const PROFILE_NAME_MAX: usize = 48;
pub const PROFILE_ABOUT_MAX: usize = 200;

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
    // G3: present only when the sender set it, so v1 envelopes keep v1's keys
    if let Some(reply_to) = r.get::<_, Option<String>>("reply_to") {
        m.insert("reply_to".into(), json!(reply_to));
    }
    m
}

/// The roster every client is sent: all registered addresses with presence.
#[tracing::instrument(level = "debug", skip_all)]
pub async fn roster(hub: &Hub, c: &impl GenericClient) -> ApiResult<Vec<Value>> {
    let rows = db::query(c, &format!("SELECT {ROSTER_COLS} FROM identities ORDER BY slug"), &[]).await?;
    Ok(rows.iter().map(|r| roster_entry(hub, r)).collect())
}

const ROSTER_COLS: &str = "slug, org_name, username, blurb, last_seen, kind";

/// One roster row as v1 sent it (presence from this process).
pub fn roster_entry(hub: &Hub, r: &tokio_postgres::Row) -> Value {
    roster_json(hub, &r.get::<_, String>(0), r.get(1), r.get(2), r.get(3), r.get(4), r.get(5))
}

pub fn roster_json(
    hub: &Hub,
    slug: &str,
    org_name: String,
    username: String,
    blurb: String,
    last_seen: Option<chrono::DateTime<chrono::Utc>>,
    kind: String,
) -> Value {
    json!({
        "slug": slug,
        "org_name": org_name,
        "username": username,
        "blurb": blurb,
        "online": hub.presence.online(slug),
        "last_seen": last_seen.map(clock::iso),
        "kind": if kind.is_empty() { "org".to_string() } else { kind },
    })
}

/// The one address a v2 call acts for: the `slug` asked for, which the
/// header must sign in, or else the only address it signs in.
pub(super) fn one_address(slugs: &[String], asked: Option<&Value>) -> ApiResult<String> {
    match asked {
        Some(Value::String(s)) if slugs.contains(s) => Ok(s.clone()),
        Some(Value::Null) | None => {
            let mut distinct = slugs.to_vec();
            distinct.sort();
            distinct.dedup();
            if distinct.len() != 1 {
                return refuse(StatusCode::UNPROCESSABLE_ENTITY, "several addresses signed in: name the one to use (slug)");
            }
            Ok(distinct.remove(0))
        }
        Some(_) => refuse(StatusCode::UNAUTHORIZED, "no valid credentials for that address"),
    }
}

/// One address's roster row, if it is registered.
pub async fn roster_one(hub: &Hub, c: &impl GenericClient, slug: &str) -> ApiResult<Option<Value>> {
    let row = db::query_opt(c, &format!("SELECT {ROSTER_COLS} FROM identities WHERE slug = $1"), &[&slug]).await?;
    Ok(row.map(|r| roster_entry(hub, &r)))
}

/// v1 `_mark_seen`: the last authenticated call, in memory (presence) and
/// in the roster's `last_seen`.
#[tracing::instrument(level = "debug", skip(hub, c), ret(level = "debug"), err(level = "debug", Debug))]
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

#[tracing::instrument(level = "debug", skip_all, ret(level = "debug"), err(level = "debug", Debug))]
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
    let kind = register_kind(&str_field(&body, "kind"));
    let org_name = pg_text(str_field(&body, "org_name"));
    let username = pg_text(str_field(&body, "username"));
    let blurb = pg_text(str_field(&body, "blurb"));
    let mut c = hub.db.get().await?;
    let mut changed = false;
    for _ in 0..3 {
        let row = db::query_opt(&c, "SELECT fingerprint, org_name, username, blurb FROM identities WHERE slug = $1", &[&slug]).await?;
        let Some(row) = row else {
            // first write wins the address
            let tx = c.transaction().await?;
            sync::roster_lock(&tx).await?;
            let now = clock::now();
            let inserted = db::execute(
                &tx,
                "INSERT INTO identities (slug, fingerprint, org_name, username, blurb, registered_at, last_seen, kind)
                 VALUES ($1, $2, $3, $4, $5, $6, $6, $7) ON CONFLICT (slug) DO NOTHING",
                &[&slug, &fp, &org_name, &username, &blurb, &now, &kind],
            )
            .await?;
            tx.commit().await?;
            if inserted == 1 {
                changed = true;
                break;
            }
            continue; // claimed in between: whose is it?
        };
        let stored: String = row.get(0);
        if !auth::ct_eq(stored.as_bytes(), fp.as_bytes()) {
            return refuse(StatusCode::FORBIDDEN, "slug is owned by another identity");
        }
        // re-registration refreshes the display fields (never the kind); one
        // that changes nothing leaves the roster's order alone
        if (row.get::<_, String>(1), row.get::<_, String>(2), row.get::<_, String>(3)) == (org_name.clone(), username.clone(), blurb.clone()) {
            break;
        }
        let tx = c.transaction().await?;
        sync::roster_lock(&tx).await?;
        let updated = db::execute(
            &tx,
            "UPDATE identities SET org_name = $2, username = $3, blurb = $4, roster_seq = nextval('roster_seq')
              WHERE slug = $1 AND fingerprint = $5",
            &[&slug, &org_name, &username, &blurb, &fp],
        )
        .await?;
        tx.commit().await?;
        if updated == 1 {
            changed = true;
            break;
        }
        // removed in between: claim it again
    }
    if changed {
        hub.presence.roster_changed();
    }
    mark_seen(hub, &c, std::slice::from_ref(&slug)).await?;
    let roster = roster(hub, &c).await?;
    ok(json!({ "ok": true, "name": hub.cfg.hub_name, "retention_days": hub.cfg.retention_days, "roster": roster }))
}

// ---------------------------------------------------------------- unregister

/// The polite exit: an authenticated client removes its own roster row(s).
/// Queued mail ages out through retention; the same secret re-mints the same
/// address later.
#[tracing::instrument(level = "debug", skip_all, ret(level = "debug"), err(level = "debug", Debug))]
pub async fn unregister(hub: &Arc<Hub>, req: &mut Req) -> ApiResult {
    let slugs = authed(hub, req).await?;
    if slugs.is_empty() {
        return refuse(StatusCode::UNAUTHORIZED, "no valid org credentials in X-Org-Auth");
    }
    let mut c = hub.db.get().await?;
    let tx = c.transaction().await?;
    sync::roster_lock(&tx).await?;
    let gone = sync::leave(&tx, &slugs).await?;
    tx.commit().await?;
    for s in &slugs {
        hub.presence.forget(s);
    }
    if !gone.is_empty() {
        hub.presence.roster_changed();
    }
    ok(json!({ "unregistered": slugs }))
}

// ---------------------------------------------------------------------- poll

/// One long poll carries, for every org whose credentials it presents: the
/// queued mail (until acked), the receipts its senders are owed (each pushed
/// once), and the roster.
#[tracing::instrument(level = "debug", skip_all, ret(level = "debug"), err(level = "debug", Debug))]
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

#[tracing::instrument(level = "debug", skip(hub))]
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

#[tracing::instrument(level = "debug", skip_all)]
async fn poll_answer(hub: &Hub, slugs: &[String], messages: Vec<Value>, receipts: Vec<Value>) -> ApiResult {
    let c = hub.db.get().await?;
    mark_seen(hub, &c, slugs).await?;
    let roster = roster(hub, &c).await?;
    ok(json!({ "name": hub.cfg.hub_name, "messages": messages, "receipts": receipts, "roster": roster }))
}

// ----------------------------------------------------------------------- ack

/// Custody transfer: the recipient persisted the mail. Only the addressee may
/// ack; a second ack is a no-op.
#[tracing::instrument(level = "debug", skip_all, ret(level = "debug"), err(level = "debug", Debug))]
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
#[tracing::instrument(level = "debug", skip_all, ret(level = "debug"), err(level = "debug", Debug))]
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
    // G3: the message this one answers, as the sender names it (never checked
    // for existence: it may be deleted, or on another hub)
    let reply_to = match body.get("reply_to") {
        None | Some(Value::Null) => None,
        Some(Value::String(r)) if r.contains('\0') => {
            return refuse(StatusCode::UNPROCESSABLE_ENTITY, "reply_to contains a NUL character");
        }
        Some(Value::String(r)) if r.chars().count() > REPLY_TO_MAX => {
            return refuse(StatusCode::UNPROCESSABLE_ENTITY, format!("reply_to is longer than {REPLY_TO_MAX} characters"));
        }
        Some(Value::String(r)) => Some(r.clone()),
        Some(_) => return refuse(StatusCode::UNPROCESSABLE_ENTITY, "reply_to must be a message id (a string)"),
    };
    let received = clock::now();
    // a retry is answered without touching the insert: v1's INSERT OR
    // IGNORE never used up a message number, and neither does this
    let known = db::query_opt(&tx, "SELECT 1 FROM messages WHERE id = $1", &[&mid]).await?.is_some();
    let inserted = if known {
        None
    } else {
        db::query_opt(
            &tx,
            "INSERT INTO messages (id, from_slug, to_slug, body, kind, thread_id, sent_at, received_at, attachments, reply_to)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) ON CONFLICT (id) DO NOTHING RETURNING n",
            &[&mid, &frm, &to, &text, &kind, &thread_id, &sent_at, &received, &Value::Array(metas), &reply_to],
        )
        .await?
    };
    let fresh = inserted.is_some();
    let received = if let Some(row) = inserted {
        db::execute(&tx, "UPDATE attachments SET message_id = $1 WHERE id = ANY($2)", &[&mid, &lookup]).await?;
        // G1: both sides' devices see it (last: this holds the logs' heads)
        sync::log_changes(&tx, vec![(frm.clone(), row.get(0)), (to.clone(), row.get(0))]).await?;
        received
    } else {
        db::query_one(&tx, "SELECT received_at FROM messages WHERE id = $1", &[&mid]).await?.get(0)
    };
    tx.commit().await?;
    mark_seen(hub, &c, std::slice::from_ref(&frm)).await?;
    if fresh {
        hub.presence.wake([to.as_str()]);
        hub.presence.wake_sync([frm.as_str()]);
    }
    ok(json!({ "id": mid, "received_at": clock::iso(received), "duplicate": !fresh }))
}

// ------------------------------------------------------------------ receipts

/// `delivered` and `read` from the recipient's side. Each is written only
/// while unset (the ladder never moves backwards) and only by the addressee;
/// other states are ignored, not refused.
#[tracing::instrument(level = "debug", skip_all, ret(level = "debug"), err(level = "debug", Debug))]
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
    let mut recipients: Vec<String> = Vec::new();
    let mut changed: Vec<(String, i64)> = Vec::new();
    let mut todo: Vec<(String, &str, String)> = Vec::new();
    for r in &items {
        let Value::Object(r) = r else { return crash("receipts: an entry is not an object") };
        let mid = str_field(r, "id");
        if mid.contains('\0') {
            continue; // matches no stored message
        }
        let sql = match str_field(r, "state").as_str() {
            "delivered" => {
                "UPDATE messages SET delivered_at = $1, receipts_pushed = false
                  WHERE id = $2 AND delivered_at IS NULL AND to_slug = ANY($3) RETURNING from_slug, to_slug, n"
            }
            "read" => {
                "UPDATE messages SET read_at = $1, receipts_pushed = false
                  WHERE id = $2 AND read_at IS NULL AND to_slug = ANY($3) RETURNING from_slug, to_slug, n"
            }
            _ => continue,
        };
        let at = pg_text(get_or(r, "at").map(py_str).unwrap_or_else(clock::now_iso));
        todo.push((mid, sql, at));
    }
    // one lock order for every writer, so two devices of one reader sending
    // the same receipts in different orders cannot deadlock (stable: a
    // message's own receipts keep their order, and the first still wins)
    todo.sort_by(|a, b| a.0.cmp(&b.0));
    for (mid, sql, at) in &todo {
        let rows = db::query(&tx, sql, &[at, mid, &slugs]).await?;
        recorded += rows.len();
        for row in &rows {
            let (from, to, n): (String, String, i64) = (row.get(0), row.get(1), row.get(2));
            changed.push((from.clone(), n));
            changed.push((to.clone(), n));
            senders.push(from);
            recipients.push(to);
        }
    }
    // G1: the receipt reaches the sender's devices and the reader's other
    // devices (read on one is read on all)
    sync::log_changes(&tx, changed).await?;
    tx.commit().await?;
    mark_seen(hub, &c, &slugs).await?;
    hub.presence.wake(senders.iter().map(String::as_str));
    hub.presence.wake_sync(recipients.iter().map(String::as_str));
    ok(json!({ "recorded": recorded }))
}

// -------------------------------------------------------------------- roster

#[tracing::instrument(level = "debug", skip_all, ret(level = "debug"), err(level = "debug", Debug))]
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

// ------------------------------------------------------------------- profile

/// G2: the owner edits the display name (`name`, up to 48 characters) and
/// the about line (`about`, up to 200) after registration; the roster's
/// `org_name` and `blurb` carry them, so every directory shows the change at
/// its next poll. The address, the username and the kind never change.
/// `org_name`/`blurb` are accepted as the same fields' roster spellings.
#[tracing::instrument(level = "debug", skip_all, ret(level = "debug"), err(level = "debug", Debug))]
pub async fn profile(hub: &Arc<Hub>, req: &mut Req) -> ApiResult {
    let body = req.json_object_strict().await?;
    let slugs = authed(hub, req).await?;
    if slugs.is_empty() {
        return refuse(StatusCode::UNAUTHORIZED, "no valid org credentials");
    }
    let slug = one_address(&slugs, body.get("slug"))?;
    let field = |names: [&str; 2], max: usize, what: &str| -> ApiResult<Option<String>> {
        // `name` wins over its roster spelling `org_name`; a null counts as absent
        let value = names.iter().find_map(|n| body.get(*n).filter(|v| !v.is_null()));
        match value {
            None => Ok(None),
            Some(Value::String(s)) => {
                let s = pg_text(py_strip(s).to_string());
                if s.chars().count() > max {
                    return refuse(StatusCode::UNPROCESSABLE_ENTITY, format!("{what} is longer than {max} characters"));
                }
                Ok(Some(s))
            }
            Some(_) => refuse(StatusCode::UNPROCESSABLE_ENTITY, format!("{what} must be a string")),
        }
    };
    let name = field(["name", "org_name"], PROFILE_NAME_MAX, "name")?;
    let about = field(["about", "blurb"], PROFILE_ABOUT_MAX, "about")?;
    if name.is_none() && about.is_none() {
        return refuse(StatusCode::UNPROCESSABLE_ENTITY, "nothing to update: give name and/or about");
    }
    let mut c = hub.db.get().await?;
    let tx = c.transaction().await?;
    sync::roster_lock(&tx).await?;
    let updated = db::execute(
        &tx,
        "UPDATE identities SET org_name = COALESCE($2, org_name), blurb = COALESCE($3, blurb), roster_seq = nextval('roster_seq')
          WHERE slug = $1",
        &[&slug, &name, &about],
    )
    .await?;
    tx.commit().await?;
    if updated == 0 {
        // unregistered between the credential check and the update
        return refuse(StatusCode::UNAUTHORIZED, "no valid org credentials");
    }
    hub.presence.roster_changed();
    mark_seen(hub, &c, std::slice::from_ref(&slug)).await?;
    let me = roster_one(hub, &c, &slug).await?.unwrap_or(Value::Null);
    ok(json!({ "ok": true, "profile": me }))
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
