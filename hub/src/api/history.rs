//! G4: history kept until deleted. Mail stays on the hub until its owners
//! delete it (unless the operator sets `HUB_RETENTION_DAYS`):
//! `GET /api/conversations` lists who an address has mail with, `GET
//! /api/history` pages through one conversation, newest first, and `DELETE
//! /api/messages/{id}` and `DELETE /api/conversations/{address}` delete the
//! caller's copies only — the other side keeps theirs. A message's row (and
//! its files) goes when neither side has a copy any more. Deletions reach the
//! caller's other devices through sync.

use std::sync::Arc;

use chrono::{DateTime, TimeZone, Utc};
use deadpool_postgres::GenericClient;
use http::StatusCode;
use serde_json::{json, Map, Value};

use super::mail::{caller, envelope_v2, mark_seen, ENVELOPE_COLS};
use super::{ok, refuse, sync, ApiResult, Hub, Req};
use crate::blobs::blob_path;
use crate::clock;
use crate::db;

pub const HISTORY_DEFAULT: i64 = 50;
pub const HISTORY_MAX: i64 = 200;
pub const CONVERSATIONS_MAX: i64 = 1000;
/// Messages one step of a conversation delete handles (each step is its own
/// short transaction).
const DELETE_BATCH: i64 = 1000;

/// A message as history shows it: poll's envelope plus its receipt ladder
/// (`received_at` is in the envelope).
fn with_ladder(r: &tokio_postgres::Row) -> Map<String, Value> {
    let mut m = envelope_v2(r);
    m.insert("fetched_at".into(), json!(r.get::<_, Option<DateTime<Utc>>>("fetched_at").map(clock::iso)));
    m.insert("delivered_at".into(), json!(r.get::<_, Option<String>>("delivered_at")));
    m.insert("read_at".into(), json!(r.get::<_, Option<String>>("read_at")));
    m
}

/// `GET /api/conversations[?slug=]`: one row per address the caller has
/// mail with (newest first): the last message and how many are unread.
#[tracing::instrument(level = "debug", skip_all, ret(level = "debug"), err(level = "debug", Debug))]
pub async fn conversations(hub: &Arc<Hub>, req: &mut Req) -> ApiResult {
    let me = caller(hub, req).await?;
    let c = hub.db.get().await?;
    mark_seen(hub, &c, std::slice::from_ref(&me)).await?;
    // who the caller has mail with: a loose index scan per direction, so
    // the cost follows the number of correspondents, not of messages
    let rows = db::query(
        &c,
        &format!(
            "WITH RECURSIVE
               out_peers(p) AS (
                 (SELECT to_slug FROM messages WHERE from_slug = $1 ORDER BY to_slug LIMIT 1)
                 UNION ALL
                 SELECT (SELECT to_slug FROM messages WHERE from_slug = $1 AND to_slug > o.p ORDER BY to_slug LIMIT 1)
                   FROM out_peers o WHERE o.p IS NOT NULL),
               in_peers(p) AS (
                 (SELECT from_slug FROM messages WHERE to_slug = $1 ORDER BY from_slug LIMIT 1)
                 UNION ALL
                 SELECT (SELECT from_slug FROM messages WHERE to_slug = $1 AND from_slug > i.p ORDER BY from_slug LIMIT 1)
                   FROM in_peers i WHERE i.p IS NOT NULL),
               peers AS (SELECT p FROM out_peers WHERE p IS NOT NULL UNION SELECT p FROM in_peers WHERE p IS NOT NULL),
               latest AS (
                 SELECT peers.p,
                        (SELECT b.n FROM (
                            (SELECT n, received_at FROM messages
                              WHERE from_slug = $1 AND to_slug = peers.p AND sender_deleted_at IS NULL
                              ORDER BY received_at DESC, n DESC LIMIT 1)
                            UNION ALL
                            (SELECT n, received_at FROM messages
                              WHERE from_slug = peers.p AND to_slug = $1 AND recipient_deleted_at IS NULL
                              ORDER BY received_at DESC, n DESC LIMIT 1)
                          ) b ORDER BY b.received_at DESC, b.n DESC LIMIT 1) AS last_n
                   FROM peers)
             SELECT latest.p AS peer,
                    (SELECT count(*) FROM messages u
                      WHERE u.to_slug = $1 AND u.from_slug = latest.p AND u.read_at IS NULL AND u.recipient_deleted_at IS NULL) AS unread,
                    {cols}
               FROM latest JOIN messages m ON m.n = latest.last_n
              ORDER BY m.received_at DESC, m.n DESC
              LIMIT $2",
            cols = ENVELOPE_COLS.split(", ").map(|c| format!("m.{c}")).collect::<Vec<_>>().join(", ")
        ),
        &[&me, &CONVERSATIONS_MAX],
    )
    .await?;
    let list: Vec<Value> = rows
        .iter()
        .map(|r| json!({ "with": r.get::<_, String>("peer"), "unread": r.get::<_, i64>("unread"), "last": with_ladder(r) }))
        .collect();
    ok(json!({ "slug": me, "conversations": list }))
}

/// A history position: the (received_at, n) of the oldest message a page
/// returned, as `<unix ms>-<n>`.
fn parse_before(v: Option<&str>) -> ApiResult<Option<(DateTime<Utc>, i64)>> {
    let Some(v) = v.filter(|v| !v.is_empty()) else { return Ok(None) };
    let bad = || refuse(StatusCode::UNPROCESSABLE_ENTITY, "before is not a history cursor from this hub");
    let Some((ms, n)) = v.split_once('-') else { return bad() };
    let num = |x: &str| x.bytes().all(|b| b.is_ascii_digit()).then(|| x.parse::<i64>().ok()).flatten();
    match (num(ms).and_then(|ms| Utc.timestamp_millis_opt(ms).single()), num(n)) {
        (Some(t), Some(n)) => Ok(Some((t, n))),
        _ => bad(),
    }
}

/// `GET /api/history?with=<address>[&before=<cursor>][&limit=<n>][&slug=]`:
/// one conversation, newest first, `limit` (default 50, at most 200) per
/// page; `before` in the answer fetches the next older page (null at the
/// start of the conversation).
#[tracing::instrument(level = "debug", skip_all, ret(level = "debug"), err(level = "debug", Debug))]
pub async fn history(hub: &Arc<Hub>, req: &mut Req) -> ApiResult {
    let me = caller(hub, req).await?;
    let with = match req.query("with") {
        Some(w) if !w.is_empty() && !w.contains('\0') => w.to_string(),
        _ => return refuse(StatusCode::UNPROCESSABLE_ENTITY, "with is required: the address of the conversation"),
    };
    let limit = match req.query("limit") {
        None | Some("") => HISTORY_DEFAULT,
        Some(l) => match l.trim().parse::<i64>() {
            Ok(l) => l.clamp(1, HISTORY_MAX),
            Err(_) => return refuse(StatusCode::UNPROCESSABLE_ENTITY, "limit must be a whole number"),
        },
    };
    let before = parse_before(req.query("before"))?;
    // the start: later than any message the hub can hold
    let (at, n) = before.unwrap_or((Utc.with_ymd_and_hms(9999, 12, 31, 0, 0, 0).single().unwrap_or_else(Utc::now), i64::MAX));
    let c = hub.db.get().await?;
    mark_seen(hub, &c, std::slice::from_ref(&me)).await?;
    let rows = db::query(
        &c,
        &format!(
            "SELECT {ENVELOPE_COLS} FROM messages WHERE n IN (
                (SELECT n FROM messages
                  WHERE from_slug = $1 AND to_slug = $2 AND sender_deleted_at IS NULL AND (received_at, n) < ($3, $4)
                  ORDER BY received_at DESC, n DESC LIMIT $5)
                UNION
                (SELECT n FROM messages
                  WHERE from_slug = $2 AND to_slug = $1 AND recipient_deleted_at IS NULL AND (received_at, n) < ($3, $4)
                  ORDER BY received_at DESC, n DESC LIMIT $5))
             ORDER BY received_at DESC, n DESC LIMIT $5"
        ),
        &[&me, &with, &at, &n, &(limit + 1)],
    )
    .await?;
    let more = rows.len() as i64 > limit;
    let page: Vec<&tokio_postgres::Row> = rows.iter().take(limit as usize).collect();
    let next = if more {
        page.last().map(|r| format!("{}-{}", r.get::<_, DateTime<Utc>>("received_at").timestamp_millis(), r.get::<_, i64>("n")))
    } else {
        None
    };
    let messages: Vec<Value> = page.iter().map(|r| Value::Object(with_ladder(r))).collect();
    ok(json!({ "slug": me, "with": with, "messages": messages, "before": next }))
}

/// The caller's copies of these (locked) messages are deleted: each row
/// whose last copy this was goes, with its files (returned, to remove after
/// commit). Mail to the caller still in v1's queue counts as handed over.
/// Returns the deleted `(n, id)`, the files to remove, and the senders owed a
/// "fetched" receipt.
async fn delete_copies(
    tx: &impl GenericClient,
    me: &str,
    ns: &[i64],
) -> Result<(Vec<(i64, String)>, Vec<String>, Vec<String>), tokio_postgres::Error> {
    let now = clock::now();
    let rows = db::query(
        tx,
        "UPDATE messages SET
            sender_deleted_at = CASE WHEN from_slug = $2 THEN COALESCE(sender_deleted_at, $3) ELSE sender_deleted_at END,
            recipient_deleted_at = CASE WHEN to_slug = $2 THEN COALESCE(recipient_deleted_at, $3) ELSE recipient_deleted_at END,
            fetched_at = CASE WHEN to_slug = $2 AND state = 'queued' THEN $3 ELSE fetched_at END,
            receipts_pushed = CASE WHEN to_slug = $2 AND state = 'queued' THEN false ELSE receipts_pushed END,
            state = CASE WHEN to_slug = $2 THEN 'fetched' ELSE state END
          WHERE n = ANY($1)
          RETURNING n, id, from_slug, sender_deleted_at IS NOT NULL AND recipient_deleted_at IS NOT NULL, fetched_at = $3",
        &[&ns, &me, &now],
    )
    .await?;
    let mut gone = Vec::with_capacity(rows.len());
    let mut purge: Vec<String> = Vec::new();
    let mut owed: Vec<String> = Vec::new();
    for r in &rows {
        let id: String = r.get(1);
        if r.get::<_, bool>(3) {
            purge.push(id.clone());
        }
        if r.get::<_, Option<bool>>(4).unwrap_or(false) {
            owed.push(r.get(2));
        }
        gone.push((r.get(0), id));
    }
    let mut files = Vec::new();
    if !purge.is_empty() {
        // neither side keeps a copy: the message and its files go
        let att = db::query(tx, "DELETE FROM attachments WHERE message_id = ANY($1) RETURNING id", &[&purge]).await?;
        files.extend(att.iter().map(|r| r.get::<_, String>(0)));
        db::execute(tx, "DELETE FROM messages WHERE id = ANY($1)", &[&purge]).await?;
    }
    sync::log_deletions(tx, me, &gone).await?;
    Ok((gone, files, owed))
}

async fn remove_files(hub: &Hub, files: &[String]) {
    let dir = hub.cfg.blob_dir();
    for id in files {
        if let Some(p) = blob_path(&dir, id) {
            let _ = tokio::fs::remove_file(p).await;
        }
    }
}

/// `DELETE /api/messages/{id}[?slug=]`: the caller's copy of one message.
/// `{"deleted": 1}`, or 0 when it was already deleted; 404 when the caller
/// has no such message.
#[tracing::instrument(level = "debug", skip(hub, req), ret(level = "debug"), err(level = "debug", Debug))]
pub async fn delete_message(hub: &Arc<Hub>, req: &mut Req, id: &str) -> ApiResult {
    let me = caller(hub, req).await?;
    if id.contains('\0') {
        return refuse(StatusCode::NOT_FOUND, "no such message");
    }
    let mut c = hub.db.get().await?;
    let tx = c.transaction().await?;
    let Some(row) = db::query_opt(
        &tx,
        "SELECT n, (from_slug = $2 AND sender_deleted_at IS NULL) OR (to_slug = $2 AND recipient_deleted_at IS NULL)
           FROM messages WHERE id = $1 AND (from_slug = $2 OR to_slug = $2) FOR UPDATE",
        &[&id, &me],
    )
    .await?
    else {
        return refuse(StatusCode::NOT_FOUND, "no such message");
    };
    let (n, mine): (i64, bool) = (row.get(0), row.get(1));
    if !mine {
        return ok(json!({ "deleted": 0 }));
    }
    let (gone, files, owed) = delete_copies(&tx, &me, &[n]).await?;
    tx.commit().await?;
    remove_files(hub, &files).await;
    mark_seen(hub, &c, std::slice::from_ref(&me)).await?;
    hub.presence.wake_sync([me.as_str()]);
    hub.presence.wake(owed.iter().map(String::as_str));
    ok(json!({ "deleted": gone.len() }))
}

/// `DELETE /api/conversations/{address}[?slug=]`: every copy the caller has
/// of its conversation with that address, in short steps. `{"deleted": N}`.
#[tracing::instrument(level = "debug", skip(hub, req), ret(level = "debug"), err(level = "debug", Debug))]
pub async fn delete_conversation(hub: &Arc<Hub>, req: &mut Req, with: &str) -> ApiResult {
    let me = caller(hub, req).await?;
    let mut c = hub.db.get().await?;
    // the conversation as it stands now: mail arriving meanwhile stays
    let upto: i64 = db::query_one(&c, "SELECT COALESCE(max(n), 0) FROM messages", &[]).await?.get(0);
    let mut deleted = 0usize;
    let mut owed_all: Vec<String> = Vec::new();
    // rows another writer holds right now are left for a later step, never
    // waited for; a few rounds of that give up rather than spin
    let mut idle_rounds = 0;
    loop {
        let tx = c.transaction().await?;
        let rows = db::query(
            &tx,
            "SELECT n FROM messages WHERE n IN (
                (SELECT n FROM messages WHERE from_slug = $1 AND to_slug = $2 AND sender_deleted_at IS NULL AND n <= $4
                  ORDER BY n LIMIT $3)
                UNION
                (SELECT n FROM messages WHERE from_slug = $2 AND to_slug = $1 AND recipient_deleted_at IS NULL AND n <= $4
                  ORDER BY n LIMIT $3))
             ORDER BY n LIMIT $3 FOR UPDATE SKIP LOCKED",
            &[&me, &with, &DELETE_BATCH, &upto],
        )
        .await?;
        let ns: Vec<i64> = rows.iter().map(|r| r.get(0)).collect();
        if ns.is_empty() {
            tx.commit().await?;
            let left = db::query_opt(
                &c,
                "SELECT 1 FROM messages WHERE n <= $3 AND ((from_slug = $1 AND to_slug = $2 AND sender_deleted_at IS NULL)
                    OR (from_slug = $2 AND to_slug = $1 AND recipient_deleted_at IS NULL)) LIMIT 1",
                &[&me, &with, &upto],
            )
            .await?;
            idle_rounds += 1;
            if left.is_none() || idle_rounds > 40 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            continue;
        }
        let (gone, files, owed) = delete_copies(&tx, &me, &ns).await?;
        tx.commit().await?;
        remove_files(hub, &files).await;
        deleted += gone.len();
        owed_all.extend(owed);
        hub.presence.wake_sync([me.as_str()]);
    }
    mark_seen(hub, &c, std::slice::from_ref(&me)).await?;
    owed_all.sort();
    owed_all.dedup();
    hub.presence.wake(owed_all.iter().map(String::as_str));
    ok(json!({ "deleted": deleted }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_cursors() {
        assert_eq!(parse_before(None).ok(), Some(None));
        assert_eq!(parse_before(Some("")).ok(), Some(None));
        let t = Utc.timestamp_millis_opt(1_760_000_000_123).single().unwrap();
        assert_eq!(parse_before(Some("1760000000123-42")).ok(), Some(Some((t, 42))));
        for bad in ["x", "1-", "-1", "1-x", "+1-2", "1-2-3"] {
            assert!(parse_before(Some(bad)).is_err(), "{bad}");
        }
    }
}
