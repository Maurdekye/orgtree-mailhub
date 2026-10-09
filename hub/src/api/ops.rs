//! `/healthz` and the read-only operator view (`/`, `/ui/data`,
//! `/ui/messages`). The operator view is UNAUTHENTICATED by ruling: on the
//! closed network the hub serves, hub access is read access to all mail.
//! The public listener never serves it.

use std::sync::Arc;

use axum::body::Body;
use http::{header, HeaderValue, Response, StatusCode};
use serde_json::{json, Value};

use super::mail::{envelope, roster};
use super::{ok, refuse, ApiError, ApiResult, Hub, Req, Resp, INDEX_HTML};
use crate::blobs;
use crate::clock;
use crate::db;
use crate::wire::{self, pg_text};

#[tracing::instrument(level = "debug", skip_all, ret(level = "debug"), err(level = "debug", Debug))]
pub async fn healthz(hub: &Arc<Hub>, on_door: bool) -> ApiResult {
    let c = hub.db.get().await?;
    let row = db::query_one(
        &c,
        "SELECT (SELECT count(*) FROM identities), (SELECT count(*) FROM messages WHERE state = 'queued')",
        &[],
    )
    .await?;
    let Ok(limit) = blobs::attachment_limit(&hub.cfg) else {
        return refuse(StatusCode::SERVICE_UNAVAILABLE, "attachment limit configuration is invalid");
    };
    let mut health = json!({
        "ok": true,
        "name": hub.cfg.hub_name,
        "orgs": row.get::<_, i64>(0),
        "queued": row.get::<_, i64>(1),
        "retention_days": hub.cfg.retention_days,
        "max_attachment_bytes": limit,
        // G8: the same limit, named for what it bounds now: one message,
        // its body and files together
        "max_message_bytes": limit,
        "version": crate::VERSION,
        "features": FEATURES,
        // the hub's clock in unix milliseconds, so a client on several hubs
        // can estimate each one's offset (lazy history orders by hub times)
        "now": clock::now().timestamp_millis(),
    });
    // where the relay-only door listens, so a client on this machine can
    // hand its address to a phone. The main port only: a client on the door
    // already has its address, and strangers there get no internal ones.
    if let (false, Some(door), Value::Object(o)) = (on_door, hub.door.get(), &mut health) {
        o.insert("door".into(), json!({ "port": door.port(), "bind": door.ip().to_string() }));
    }
    ok(health)
}

/// The protocol additions this hub serves (docs/v2-additions.md), so a
/// client can tell what a hub supports without probing routes.
pub const FEATURES: &[&str] =
    &["person", "profile", "reply_to", "sync", "devices", "history", "delete", "long_messages", "message_limit", "directory", "uploads", "link", "device_keys", "active", "lazy_history", "door"];

/// v1 read the page in text mode, so line endings reached the browser as
/// `\n` whatever the checkout's were; the embedded copy is served the same.
static INDEX: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| INDEX_HTML.replace("\r\n", "\n").replace('\r', "\n"));

pub fn index() -> Resp {
    let mut r = Response::new(Body::from(INDEX.as_str()));
    r.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("text/html; charset=utf-8"));
    r
}

#[tracing::instrument(level = "debug", skip_all, ret(level = "debug"), err(level = "debug", Debug))]
pub async fn ui_data(hub: &Arc<Hub>) -> ApiResult {
    let c = hub.db.get().await?;
    let mut rows = roster(hub, &c).await?;
    let counts = db::query(&c, "SELECT to_slug, count(*) FROM messages WHERE state = 'queued' GROUP BY to_slug", &[]).await?;
    let counts: std::collections::HashMap<String, i64> = counts.iter().map(|r| (r.get(0), r.get(1))).collect();
    for r in rows.iter_mut() {
        let slug = r["slug"].as_str().unwrap_or("").to_string();
        if let Value::Object(o) = r {
            o.insert("queued".into(), json!(counts.get(&slug).copied().unwrap_or(0)));
        }
    }
    ok(json!({ "name": hub.cfg.hub_name, "version": crate::VERSION, "retention_days": hub.cfg.retention_days, "orgs": rows }))
}

/// Newest first, `limit` clamped to 1..500, keyset-paged by
/// (`before_at`, `before_n`) — strictly older rows in the same
/// (received_at, n) order, so paging stays stable while mail arrives.
/// `org` filters to one address; `client` to every address of one client
/// (its registered username or the slug's middle segment, case and _/-
/// folded on both sides).
fn ui_params(req: &Req) -> ApiResult<(i64, i64, String, String, String)> {
    let mut errors = Vec::new();
    let mut int_param = |name: &str, default: i64| match req.query(name) {
        None => default,
        Some(v) => wire::parse_query_int(v).unwrap_or_else(|| {
            errors.push(wire::query_error(name, false, v)["detail"][0].clone());
            default
        }),
    };
    let limit = int_param("limit", 100);
    let before_n = int_param("before_n", 0);
    if !errors.is_empty() {
        return Err(ApiError::Body(StatusCode::UNPROCESSABLE_ENTITY, json!({ "detail": errors })));
    }
    Ok((limit, before_n, pg_text(req.query_str("org", "")), req.query_str("client", ""), req.query_str("before_at", "")))
}

#[tracing::instrument(level = "debug", skip_all, ret(level = "debug"), err(level = "debug", Debug))]
pub async fn ui_messages(hub: &Arc<Hub>, req: &mut Req) -> ApiResult {
    // FastAPI validated every declared parameter and reported all failures
    let (limit, before_n, org, client, before_at) = ui_params(req)?;
    let limit = limit.clamp(1, 500);
    let cursor = if before_at.is_empty() {
        None
    } else {
        match clock::parse(&before_at) {
            Some(t) => Some(t),
            None => return refuse(StatusCode::UNPROCESSABLE_ENTITY, "before_at is not a timestamp"),
        }
    };
    let c = hub.db.get().await?;
    let cols = super::mail::ENVELOPE_COLS;
    let page = "AND ($2::timestamptz IS NULL OR received_at < $2 OR (received_at = $2 AND n < $3))
                ORDER BY received_at DESC, n DESC LIMIT $4";
    let rows = if !org.is_empty() {
        db::query(
            &c,
            &format!("SELECT {cols} FROM messages WHERE (from_slug = $1 OR to_slug = $1) {page}"),
            &[&org, &cursor, &before_n, &limit],
        )
        .await?
    } else if !client.is_empty() {
        let fold = |s: &str| s.to_lowercase().replace('_', "-");
        let seg = |slug: &str| {
            let parts: Vec<&str> = slug.split('.').collect();
            if parts.len() > 2 {
                parts[1].to_string()
            } else {
                String::new()
            }
        };
        let want = fold(&client);
        let all = db::query(&c, "SELECT slug, username FROM identities", &[]).await?;
        let slugs: Vec<String> = all
            .iter()
            .filter(|r| {
                let slug: String = r.get(0);
                let user: String = r.get(1);
                !want.is_empty() && (fold(&user) == want || fold(&seg(&slug)) == want)
            })
            .map(|r| r.get(0))
            .collect();
        if slugs.is_empty() {
            Vec::new()
        } else {
            db::query(
                &c,
                &format!("SELECT {cols} FROM messages WHERE (from_slug = ANY($1) OR to_slug = ANY($1)) {page}"),
                &[&slugs, &cursor, &before_n, &limit],
            )
            .await?
        }
    } else {
        db::query(
            &c,
            &format!("SELECT {cols} FROM messages WHERE $1::text IS NULL {page}"),
            &[&None::<String>, &cursor, &before_n, &limit],
        )
        .await?
    };
    let messages: Vec<Value> = rows
        .iter()
        .map(|m| {
            let mut e = envelope(m);
            e.insert("state".into(), json!(m.get::<_, String>("state")));
            e.insert("delivered_at".into(), json!(m.get::<_, Option<String>>("delivered_at")));
            e.insert("read_at".into(), json!(m.get::<_, Option<String>>("read_at")));
            e.insert("n".into(), json!(m.get::<_, i64>("n")));
            Value::Object(e)
        })
        .collect();
    ok(json!({ "messages": messages }))
}
