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

/// v1 BODY_MAX. v1 cut every body here; v2 keeps bodies whole (G6) and
/// shows v1's routes this much of a longer one, then says it continues, and
/// keeps it as the preview of a body stored in a file.
pub const BODY_MAX: usize = 20000;
/// A body up to this many bytes is kept in its row and carried whole by
/// sync and history; a longer one lives in a file fetched on demand
/// (`GET /api/messages/{id}/body`), so their answers stay small.
pub const INLINE_BODY_MAX: usize = 64 * 1024;
pub const MAX_FILES_PER_MESSAGE: usize = 10;
/// v1 POLL_CEILING: the longest a poll parks, whatever `wait` asks.
pub const POLL_CEILING: f64 = 55.0;
/// Messages one poll answer carries at most (v1 had no cap; the rest come
/// with the next poll, which returns at once while mail is queued).
pub const POLL_BATCH: i64 = 500;

pub const ENVELOPE_COLS: &str = "n, id, from_slug, to_slug, body, kind, thread_id, sent_at, received_at, state, fetched_at, delivered_at, \
                                 read_at, attachments, reply_to, body_bytes, body_part";

/// What a v1 route says after the first 20,000 characters of a longer body
/// (ruling 8 October: never a silent cut).
pub fn continues_line(total_bytes: i64) -> String {
    format!("\n\n[message continues: {total_bytes} bytes — open it in a client that supports long messages]")
}

/// The client kinds an address registers as. v1 knew org and chat (and
/// stored anything else as org); v2 adds person (G2). Fixed at the first
/// registration, except that a chat may become a person (`kept_kind`).
pub fn register_kind(asked: &str) -> &'static str {
    match asked {
        "chat" => "chat",
        "person" => "person",
        _ => "org",
    }
}

/// The kind a re-registration leaves an address with: the first one, except
/// that a chat asking to be a person becomes one. Hubchat registered its
/// people as chats on v1 hubs, which had no person kind; nothing else
/// changes kind (no person to chat, nothing into or out of org).
pub fn kept_kind<'a>(stored: &'a str, asked: &'a str) -> &'a str {
    if stored == "chat" && asked == "person" {
        "person"
    } else {
        stored
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

/// The wire shape a recipient sees on poll (v1 `row_to_envelope`). A body
/// over 20,000 characters is cut there with a line saying it continues
/// (v1 clients cannot fetch the rest).
pub fn envelope(r: &tokio_postgres::Row) -> serde_json::Map<String, Value> {
    let body: String = r.get("body");
    let (shown, cut) = match r.get::<_, Option<i64>>("body_bytes") {
        // a body kept in a file: the row holds its first 20,000 characters
        Some(total) => (body + &continues_line(total), Some(total)),
        None if body.len() > BODY_MAX && body.chars().nth(BODY_MAX).is_some() => {
            let total = body.len() as i64;
            (format!("{}{}", py_prefix(&body, BODY_MAX), continues_line(total)), Some(total))
        }
        None => (body, None),
    };
    let mut m = envelope_with_body(r, shown);
    // a client that knows it can fetch the whole body (GET /api/messages/{id}/body)
    if let Some(total) = cut {
        m.insert("body_bytes".into(), json!(total));
    }
    m
}

/// The same message as v2's routes (sync, history) carry it: the whole body
/// when it is kept in the row; otherwise its first 20,000 characters and
/// `body_bytes`, the whole body's size (fetch it from
/// `GET /api/messages/{id}/body`).
pub fn envelope_v2(r: &tokio_postgres::Row) -> serde_json::Map<String, Value> {
    let mut m = envelope_with_body(r, r.get("body"));
    if let Some(total) = r.get::<_, Option<i64>>("body_bytes") {
        m.insert("body_bytes".into(), json!(total));
    }
    m
}

fn envelope_with_body(r: &tokio_postgres::Row, body: String) -> serde_json::Map<String, Value> {
    let mut m = serde_json::Map::new();
    m.insert("id".into(), json!(r.get::<_, String>("id")));
    m.insert("from".into(), json!(r.get::<_, String>("from_slug")));
    m.insert("to".into(), json!(r.get::<_, String>("to_slug")));
    m.insert("body".into(), json!(body));
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

/// Every credential a request proves, with the device that signed it when
/// one did (G5). Read before the future starts, as `authed`.
pub(super) fn authed_callers<'a>(hub: &'a Hub, req: &Req) -> impl std::future::Future<Output = ApiResult<Vec<auth::Caller>>> + Send + 'a {
    let pairs = auth::pairs(req.auth_header());
    async move {
        if pairs.is_empty() {
            return Ok(Vec::new());
        }
        let c = hub.db.get().await?;
        Ok(auth::authenticate_callers(&c, &pairs).await?)
    }
}

/// The one address a v2 GET or DELETE acts for (`?slug=` when the header
/// signs in several). The request is read before the future starts, so the
/// future does not borrow it.
pub(super) fn caller<'a>(hub: &'a Hub, req: &Req) -> impl std::future::Future<Output = ApiResult<String>> + Send + 'a {
    let asked = req.query("slug").map(|s| Value::String(s.to_string()));
    let auth = authed(hub, req);
    async move {
        let slugs = auth.await?;
        if slugs.is_empty() {
            return refuse(StatusCode::UNAUTHORIZED, "no valid org credentials");
        }
        one_address(&slugs, asked.as_ref())
    }
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
        let row = db::query_opt(
            &c,
            "SELECT fingerprint, org_name, username, blurb, kind, shared_key_enabled FROM identities WHERE slug = $1",
            &[&slug],
        )
        .await?;
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
        // registering is a use of the shared secret, which stops working once
        // the address turns it off or rotates its identity key (G5)
        if !row.get::<_, bool>(5) {
            return refuse(StatusCode::UNAUTHORIZED, "this address no longer accepts its shared secret");
        }
        // re-registration refreshes the display fields, and the kind only as
        // `kept_kind` allows; one that changes nothing leaves the roster's
        // order alone
        let stored_kind: String = row.get(4);
        let kind_now = kept_kind(&stored_kind, kind);
        if (row.get::<_, String>(1), row.get::<_, String>(2), row.get::<_, String>(3)) == (org_name.clone(), username.clone(), blurb.clone())
            && kind_now == stored_kind
        {
            break;
        }
        let tx = c.transaction().await?;
        sync::roster_lock(&tx).await?;
        let updated = db::execute(
            &tx,
            "UPDATE identities SET org_name = $2, username = $3, blurb = $4, kind = $6, roster_seq = nextval('roster_seq')
              WHERE slug = $1 AND fingerprint = $5",
            &[&slug, &org_name, &username, &blurb, &fp, &kind_now],
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
    ok(json!({ "ok": true, "name": hub.cfg.hub_name, "version": crate::VERSION, "retention_days": hub.cfg.retention_days, "roster": roster }))
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
    ok(json!({ "name": hub.cfg.hub_name, "version": crate::VERSION, "messages": messages, "receipts": receipts, "roster": roster }))
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
    // rows locked in message order, as every writer of message rows does
    let rows = db::query(
        &c,
        "WITH due AS (
            SELECT n FROM messages WHERE id = ANY($2) AND state = 'queued' AND to_slug = ANY($3) ORDER BY n FOR UPDATE)
         UPDATE messages m SET state = 'fetched', fetched_at = $1, receipts_pushed = false
           FROM due WHERE m.n = due.n AND m.state = 'queued'
         RETURNING m.from_slug",
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
    // G6: the body is kept whole. One too long for its row goes to a file
    // first (or comes from an upload, `body_part`), before any lock is held.
    let mut long = match body.get("body_part") {
        None | Some(Value::Null) => {
            let text = pg_text(str_field(&body, "body"));
            if text.len() > INLINE_BODY_MAX {
                Some(LongBody::write(hub, &frm, text).await?)
            } else {
                None
            }
        }
        Some(Value::String(part)) => {
            if body.get("body").is_some_and(|b| !b.is_null() && b != &json!("")) {
                return refuse(StatusCode::UNPROCESSABLE_ENTITY, "give body or body_part, not both");
            }
            Some(LongBody::uploaded(hub, &frm, part).await?)
        }
        Some(_) => return refuse(StatusCode::UNPROCESSABLE_ENTITY, "body_part must be an upload id (a string)"),
    };
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
    let mut locking = lookup.clone();
    if let Some(LongBody { uploaded: true, part, .. }) = &long {
        if lookup.contains(part) {
            return refuse(StatusCode::UNPROCESSABLE_ENTITY, "body_part is also listed as an attachment");
        }
        locking.push(part.clone());
    }
    let rows = db::query(
        &tx,
        "SELECT id, name, bytes, owner_slug, message_id FROM attachments WHERE id = ANY($1) ORDER BY id FOR UPDATE",
        &[&locking],
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
    let (text, body_bytes, body_part) = match &long {
        Some(l) => (l.preview.clone(), Some(l.bytes), Some(l.part.clone())),
        None => (pg_text(str_field(&body, "body")), None, None),
    };
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
    if !known {
        // G8: the limit bounds one message, its body and files together
        let Ok(limit) = crate::blobs::attachment_limit(&hub.cfg) else {
            return refuse(StatusCode::SERVICE_UNAVAILABLE, "attachment limit configuration is invalid");
        };
        let files: i64 = metas.iter().map(|m| m["bytes"].as_i64().unwrap_or(0)).sum();
        let total = body_bytes.unwrap_or(text.len() as i64) + files;
        if total as u64 > limit {
            return Err(ApiError::Body(
                StatusCode::PAYLOAD_TOO_LARGE,
                json!({
                    "detail": format!("message exceeds hub limit of {limit} bytes (body and attachments come to {total})"),
                    "max_message_bytes": limit,
                }),
            ));
        }
    }
    let inserted = if known {
        None
    } else {
        db::query_opt(
            &tx,
            "INSERT INTO messages (id, from_slug, to_slug, body, kind, thread_id, sent_at, received_at, attachments, reply_to,
                                   body_bytes, body_part)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12) ON CONFLICT (id) DO NOTHING RETURNING n",
            &[&mid, &frm, &to, &text, &kind, &thread_id, &sent_at, &received, &Value::Array(metas), &reply_to, &body_bytes, &body_part],
        )
        .await?
    };
    let fresh = inserted.is_some();
    let received = if let Some(row) = inserted {
        db::execute(&tx, "UPDATE attachments SET message_id = $1 WHERE id = ANY($2)", &[&mid, &lookup]).await?;
        if let Some(l) = &long {
            l.bind(&tx, &mid, &frm).await?;
        }
        // G1: both sides' devices see it (last: this holds the logs' heads)
        sync::log_changes(&tx, vec![(frm.clone(), row.get(0)), (to.clone(), row.get(0))]).await?;
        received
    } else {
        db::query_one(&tx, "SELECT received_at FROM messages WHERE id = $1", &[&mid]).await?.get(0)
    };
    tx.commit().await?;
    if let (true, Some(l)) = (fresh, long.as_mut()) {
        l.keep();
    }
    mark_seen(hub, &c, std::slice::from_ref(&frm)).await?;
    if fresh {
        hub.presence.wake([to.as_str()]);
        hub.presence.wake_sync([frm.as_str()]);
    }
    ok(json!({ "id": mid, "received_at": clock::iso(received), "duplicate": !fresh }))
}

/// A body too long for its row (G6): in a file under blobs/, an attachments
/// row the message binds. Either the hub wrote it from the JSON body, or the
/// sender uploaded it and named it `body_part`.
struct LongBody {
    part: String,
    bytes: i64,
    /// the first 20,000 characters, kept in the row
    preview: String,
    uploaded: bool,
    /// a file this send wrote: removed unless the message came to exist
    written: Option<crate::blobs::UploadFiles>,
}

impl LongBody {
    #[tracing::instrument(level = "debug", skip(hub, text), fields(bytes = text.len()), err(level = "debug", Debug))]
    async fn write(hub: &Hub, owner: &str, text: String) -> ApiResult<LongBody> {
        let dir = hub.cfg.blob_dir();
        tokio::fs::create_dir_all(&dir).await?;
        let part = uuid::Uuid::new_v4().simple().to_string();
        let files = crate::blobs::UploadFiles { partial: dir.join(format!("{part}.part")), final_path: dir.join(&part), committed: false };
        tokio::fs::write(&files.partial, text.as_bytes()).await?;
        tokio::fs::rename(&files.partial, &files.final_path).await?;
        tracing::debug!(owner, part, "long body written");
        Ok(LongBody { preview: py_prefix(&text, BODY_MAX).to_string(), bytes: text.len() as i64, part, uploaded: false, written: Some(files) })
    }

    /// An upload named as the body: the sender's own, not bound to another
    /// message, UTF-8 text without NUL (read through once, never whole).
    #[tracing::instrument(level = "debug", skip(hub), err(level = "debug", Debug))]
    async fn uploaded(hub: &Hub, owner: &str, part: &str) -> ApiResult<LongBody> {
        let unknown = || refuse(StatusCode::UNPROCESSABLE_ENTITY, format!("unknown body_part {}", py_repr_str(part)));
        if part.contains('\0') {
            return unknown();
        }
        let c = hub.db.get().await?;
        let Some(row) = db::query_opt(&c, "SELECT owner_slug, bytes FROM attachments WHERE id = $1", &[&part]).await? else {
            return unknown();
        };
        drop(c);
        if row.get::<_, String>(0) != owner {
            return unknown();
        }
        let Some(path) = crate::blobs::blob_path(&hub.cfg.blob_dir(), part) else { return unknown() };
        let Ok(file) = tokio::fs::File::open(&path).await else { return unknown() };
        let preview = match crate::wire::read_text_preview(file, BODY_MAX).await? {
            crate::wire::TextCheck::Text(preview) => preview,
            crate::wire::TextCheck::NotUtf8 => return refuse(StatusCode::UNPROCESSABLE_ENTITY, "body_part is not UTF-8 text"),
            crate::wire::TextCheck::Nul => return refuse(StatusCode::UNPROCESSABLE_ENTITY, "body_part contains a NUL character"),
        };
        Ok(LongBody { part: part.to_string(), bytes: row.get(1), preview, uploaded: true, written: None })
    }

    /// Bind the body to its (new) message, inside the send's transaction.
    async fn bind(&self, tx: &impl GenericClient, mid: &str, owner: &str) -> ApiResult<()> {
        if self.uploaded {
            let n = db::execute(
                tx,
                "UPDATE attachments SET message_id = $1 WHERE id = $2 AND (message_id IS NULL OR message_id = '' OR message_id = $1)",
                &[&mid, &self.part],
            )
            .await?;
            if n == 0 {
                return refuse(StatusCode::UNPROCESSABLE_ENTITY, format!("body_part {} already bound", py_repr_str(&self.part)));
            }
        } else {
            db::execute(
                tx,
                "INSERT INTO attachments (id, owner_slug, name, bytes, created_at, message_id) VALUES ($1, $2, 'body.txt', $3, $4, $5)",
                &[&self.part, &owner, &self.bytes, &clock::now(), &mid],
            )
            .await?;
        }
        Ok(())
    }

    fn keep(&mut self) {
        if let Some(f) = self.written.as_mut() {
            f.committed = true;
        }
    }
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
                  WHERE id = $2 AND delivered_at IS NULL AND to_slug = ANY($3)
                  RETURNING from_slug, to_slug, n, sender_deleted_at IS NULL, recipient_deleted_at IS NULL"
            }
            "read" => {
                "UPDATE messages SET read_at = $1, receipts_pushed = false
                  WHERE id = $2 AND read_at IS NULL AND to_slug = ANY($3)
                  RETURNING from_slug, to_slug, n, sender_deleted_at IS NULL, recipient_deleted_at IS NULL"
            }
            _ => continue,
        };
        let at = pg_text(get_or(r, "at").map(py_str).unwrap_or_else(clock::now_iso));
        todo.push((mid, sql, at));
    }
    // every row these receipts can touch is locked first, in message order
    // (the order every writer of message rows takes them in), so two devices
    // of one reader, or a receipt and a delete, cannot deadlock
    let ids: Vec<&str> = todo.iter().map(|t| t.0.as_str()).collect();
    db::query(&tx, "SELECT n FROM messages WHERE id = ANY($1) AND to_slug = ANY($2) ORDER BY n FOR UPDATE", &[&ids, &slugs]).await?;
    for (mid, sql, at) in &todo {
        let rows = db::query(&tx, sql, &[at, mid, &slugs]).await?;
        recorded += rows.len();
        for row in &rows {
            let (from, to, n): (String, String, i64) = (row.get(0), row.get(1), row.get(2));
            // a copy its owner deleted stays deleted (G4)
            if row.get::<_, bool>(3) {
                changed.push((from.clone(), n));
            }
            if row.get::<_, bool>(4) {
                changed.push((to.clone(), n));
            }
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
    ok(json!({ "name": hub.cfg.hub_name, "version": crate::VERSION, "roster": roster }))
}

// ----------------------------------------------------------------- directory

/// Entries one directory page carries at most.
pub const DIRECTORY_MAX: i64 = 500;

/// G9: `GET /api/directory[?q=][&after=][&limit=100]` — every registered
/// address (a page at a time, by address), or those whose address, name,
/// username or about line contains `q` (any case). `after` from an answer
/// fetches the next page; it is null on the last.
#[tracing::instrument(level = "debug", skip_all, ret(level = "debug"), err(level = "debug", Debug))]
pub async fn directory(hub: &Arc<Hub>, req: &mut Req) -> ApiResult {
    let q = req.query("q").map(py_strip).unwrap_or("").to_string();
    let after = req.query("after").unwrap_or("").to_string();
    let limit = match req.query("limit") {
        None | Some("") => 100,
        Some(l) => match l.trim().parse::<i64>() {
            Ok(l) => l.clamp(1, DIRECTORY_MAX),
            Err(_) => return refuse(StatusCode::UNPROCESSABLE_ENTITY, "limit must be a whole number"),
        },
    };
    let slugs = authed(hub, req).await?;
    if slugs.is_empty() {
        return refuse(StatusCode::UNAUTHORIZED, "no valid org credentials");
    }
    let c = hub.db.get().await?;
    mark_seen(hub, &c, &slugs).await?;
    // q matches literally: LIKE's own characters are escaped
    let pattern = format!("%{}%", pg_text(q).replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_"));
    let rows = db::query(
        &c,
        &format!(
            "SELECT {ROSTER_COLS} FROM identities
              WHERE slug > $1
                AND (slug ILIKE $2 OR org_name ILIKE $2 OR username ILIKE $2 OR blurb ILIKE $2)
              ORDER BY slug LIMIT $3"
        ),
        &[&pg_text(after), &pattern, &(limit + 1)],
    )
    .await?;
    let more = rows.len() as i64 > limit;
    let entries: Vec<Value> = rows.iter().take(limit as usize).map(|r| roster_entry(hub, r)).collect();
    let next = if more { entries.last().and_then(|e| e["slug"].as_str()).map(str::to_string) } else { None };
    ok(json!({ "name": hub.cfg.hub_name, "version": crate::VERSION, "entries": entries, "after": next }))
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
