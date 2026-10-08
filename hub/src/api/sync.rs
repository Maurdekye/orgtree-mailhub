//! G1: every device of an address gets everything. `POST /api/sync` is a
//! long poll, like `/api/poll`, that hands one device every change since
//! that device's own cursor: the messages the address received and sent
//! (with their receipts as they change), the roster's joins, edits and
//! leaves, and who is online. `GET /api/devices` lists the devices. v1's
//! queue (`/api/poll` + `/api/ack`) keeps working beside it, unchanged.
//!
//! Each address has a change log (`mailbox_log`): one entry per message,
//! moved to the end when the message changes, so a sync returns each
//! message once, as it is now. Sequence numbers are drawn under the
//! address's `mailbox_heads` row lock until commit, so a cursor can never
//! pass a change that commits late. The roster's changes are ordered the
//! same way under `db::ROSTER_LOCK`.

use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::time::{Duration, Instant};

use deadpool_postgres::GenericClient;
use http::StatusCode;
use serde_json::{json, Value};

use super::mail::{authed, envelope_v2, mark_seen, one_address, roster_json, ENVELOPE_COLS, POLL_CEILING};
use super::{ok, refuse, ApiResult, Hub, Req};
use crate::clock;
use crate::db;
use crate::presence::PRESENCE_WINDOW;
use crate::wire::{pg_text, py_strip};

/// Message changes one sync answer carries at most; `more` says to sync
/// again at once.
pub const SYNC_BATCH: usize = 500;
/// Roster changes one sync answer carries at most.
pub const ROSTER_BATCH: usize = 500;
pub const DEVICE_ID_MAX: usize = 64;
pub const DEVICE_NAME_MAX: usize = 64;
/// Devices kept per address; a new one beyond this replaces the one seen
/// longest ago.
pub const DEVICES_MAX: i64 = 100;

/// Append these messages to these addresses' change logs, moving an entry
/// that is already there to the end. Call it LAST in a transaction: it
/// holds the addresses' head rows, taken in slug order, until commit.
#[tracing::instrument(level = "debug", skip(tx), err(level = "debug", Debug))]
pub async fn log_changes(tx: &impl GenericClient, mut entries: Vec<(String, i64)>) -> Result<(), tokio_postgres::Error> {
    entries.sort();
    entries.dedup();
    let mut rest = entries.as_slice();
    while let Some((slug, _)) = rest.first() {
        let k = rest.iter().take_while(|e| &e.0 == slug).count();
        let ns: Vec<i64> = rest[..k].iter().map(|e| e.1).collect();
        db::execute(
            tx,
            "WITH head AS (
                INSERT INTO mailbox_heads (slug, seq) VALUES ($1, $3)
                ON CONFLICT (slug) DO UPDATE SET seq = mailbox_heads.seq + EXCLUDED.seq
                RETURNING seq)
             INSERT INTO mailbox_log (slug, seq, message_n)
             SELECT $1, (SELECT seq FROM head) - $3 + u.ord, u.n FROM unnest($2::bigint[]) WITH ORDINALITY AS u(n, ord)
             ON CONFLICT (slug, message_n) DO UPDATE SET seq = EXCLUDED.seq",
            &[slug, &ns, &(k as i64)],
        )
        .await?;
        rest = &rest[k..];
    }
    Ok(())
}

/// This address deleted its copies of these messages (`(n, id)`): their
/// entries in its change log move to the end as tombstones that keep the
/// id, so its devices learn of it even after the rows are gone. Call it
/// LAST in the transaction, as `log_changes`.
#[tracing::instrument(level = "debug", skip(tx, gone), fields(count = gone.len()), err(level = "debug", Debug))]
pub async fn log_deletions(tx: &impl GenericClient, slug: &str, gone: &[(i64, String)]) -> Result<(), tokio_postgres::Error> {
    if gone.is_empty() {
        return Ok(());
    }
    let ns: Vec<i64> = gone.iter().map(|g| g.0).collect();
    let ids: Vec<&str> = gone.iter().map(|g| g.1.as_str()).collect();
    db::execute(
        tx,
        "WITH head AS (
            INSERT INTO mailbox_heads (slug, seq) VALUES ($1, $4)
            ON CONFLICT (slug) DO UPDATE SET seq = mailbox_heads.seq + EXCLUDED.seq
            RETURNING seq)
         INSERT INTO mailbox_log (slug, seq, message_n, deleted_id)
         SELECT $1, (SELECT seq FROM head) - $4 + u.ord, u.n, u.id
           FROM unnest($2::bigint[], $3::text[]) WITH ORDINALITY AS u(n, id, ord)
         ON CONFLICT (slug, message_n) DO UPDATE SET seq = EXCLUDED.seq, deleted_id = EXCLUDED.deleted_id",
        &[&slug, &ns, &ids, &(gone.len() as i64)],
    )
    .await?;
    Ok(())
}

/// The roster is about to change: order this transaction's change after
/// every earlier one (see `db::ROSTER_LOCK`).
pub async fn roster_lock(tx: &impl GenericClient) -> Result<(), tokio_postgres::Error> {
    db::execute(tx, "SELECT pg_advisory_xact_lock($1)", &[&db::ROSTER_LOCK]).await?;
    Ok(())
}

/// These addresses leave the roster (unregister, or the idle prune): their
/// rows and devices go and a tombstone tells syncing directories. Their mail
/// and change logs stay, as v1 kept their mail. Call under `roster_lock`.
pub async fn leave(tx: &impl GenericClient, slugs: &[String]) -> Result<Vec<String>, tokio_postgres::Error> {
    let rows = db::query(tx, "DELETE FROM identities WHERE slug = ANY($1) RETURNING slug", &[&slugs]).await?;
    let mut gone: Vec<String> = rows.iter().map(|r| r.get(0)).collect();
    gone.sort();
    if !gone.is_empty() {
        db::execute(
            tx,
            "INSERT INTO roster_gone (seq, slug, at) SELECT nextval('roster_seq'), s, $2 FROM unnest($1::text[]) AS s",
            &[&gone, &clock::now()],
        )
        .await?;
        db::execute(tx, "DELETE FROM devices WHERE slug = ANY($1)", &[&gone]).await?;
    }
    Ok(gone)
}

/// The operator removes these addresses from the roster (the CLI's
/// `remove-address`): as an unregister, under the roster lock. Returns the
/// ones that were registered.
pub async fn remove_addresses(db: &db::Db, slugs: &[String]) -> anyhow::Result<Vec<String>> {
    let mut c = db.get().await?;
    let tx = c.transaction().await?;
    roster_lock(&tx).await?;
    let gone = leave(&tx, slugs).await?;
    tx.commit().await?;
    Ok(gone)
}

/// A sync position: the address's change log, the roster, and the online
/// set the device was last told (`<mail>-<roster>-<16 hex>`; opaque to
/// clients).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Cursor {
    mail: i64,
    roster: i64,
    online: Option<u64>,
}

impl Cursor {
    fn parse(v: Option<&Value>) -> ApiResult<Cursor> {
        let bad = || refuse(StatusCode::UNPROCESSABLE_ENTITY, "cursor is not a sync cursor from this hub");
        let s = match v {
            None | Some(Value::Null) => return Ok(Cursor::default()),
            Some(Value::String(s)) if s.is_empty() => return Ok(Cursor::default()),
            Some(Value::String(s)) => s,
            Some(_) => return bad(),
        };
        let parts: Vec<&str> = s.split('-').collect();
        let [m, r, p] = parts.as_slice() else { return bad() };
        let num = |x: &str| x.bytes().all(|b| b.is_ascii_digit()).then(|| x.parse::<i64>().ok()).flatten();
        match (num(m), num(r), (p.len() == 16).then(|| u64::from_str_radix(p, 16).ok()).flatten()) {
            (Some(mail), Some(roster), Some(online)) => Ok(Cursor { mail, roster, online: Some(online) }),
            _ => bad(),
        }
    }

    fn render(&self, online: u64) -> String {
        format!("{}-{}-{online:016x}", self.mail, self.roster)
    }
}

fn online_print(online: &[String]) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    online.hash(&mut h);
    h.finish()
}

/// `device_id`: 1-64 printable ASCII characters, no spaces (client-made).
fn device_id(v: Option<&Value>) -> ApiResult<String> {
    match v {
        Some(Value::String(s)) if (1..=DEVICE_ID_MAX).contains(&s.len()) && s.bytes().all(|b| (0x21..=0x7e).contains(&b)) => {
            Ok(s.clone())
        }
        None | Some(Value::Null) => refuse(StatusCode::UNPROCESSABLE_ENTITY, "device_id is required"),
        Some(_) => refuse(
            StatusCode::UNPROCESSABLE_ENTITY,
            format!("device_id must be 1 to {DEVICE_ID_MAX} printable ASCII characters without spaces"),
        ),
    }
}

/// `device_name`: optional, up to 64 characters; absent keeps the stored one.
fn device_name(v: Option<&Value>) -> ApiResult<Option<String>> {
    match v {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => {
            let s = pg_text(py_strip(s).to_string());
            if s.chars().count() > DEVICE_NAME_MAX {
                return refuse(StatusCode::UNPROCESSABLE_ENTITY, format!("device_name is longer than {DEVICE_NAME_MAX} characters"));
            }
            Ok(Some(s))
        }
        Some(_) => refuse(StatusCode::UNPROCESSABLE_ENTITY, "device_name must be a string"),
    }
}

/// A device is seen at each sync; a new one beyond `DEVICES_MAX` replaces
/// the address's device seen longest ago.
#[tracing::instrument(level = "debug", skip(c, name), err(level = "debug", Debug))]
async fn seen_device(c: &impl GenericClient, slug: &str, device: &str, name: Option<&str>) -> Result<(), tokio_postgres::Error> {
    let now = clock::now();
    let row = db::query_one(
        c,
        "INSERT INTO devices (slug, device_id, name, created_at, last_seen) VALUES ($1, $2, COALESCE($3, ''), $4, $4)
         ON CONFLICT (slug, device_id) DO UPDATE SET last_seen = EXCLUDED.last_seen, name = COALESCE($3, devices.name)
         RETURNING xmax = 0",
        &[&slug, &device, &name, &now],
    )
    .await?;
    if row.get::<_, bool>(0) {
        // never the device that just arrived
        db::execute(
            c,
            "DELETE FROM devices WHERE slug = $1 AND device_id IN
               (SELECT device_id FROM devices WHERE slug = $1 AND device_id <> $2
                 ORDER BY last_seen DESC, created_at DESC, device_id DESC OFFSET $3)",
            &[&slug, &device, &(DEVICES_MAX - 1)],
        )
        .await?;
    }
    Ok(())
}

/// Custody, as an ack: the device's cursor says it holds everything up to
/// there, so mail to the address still queued below it is handed over
/// (and the senders' v1 polls are owed a "fetched" receipt). A message
/// another writer holds right now is left for the next sync, never waited
/// for, so this can never deadlock with a receipt or an ack.
#[tracing::instrument(level = "debug", skip(c), err(level = "debug", Debug))]
async fn take_custody(c: &impl GenericClient, slug: &str, upto: i64) -> Result<Vec<String>, tokio_postgres::Error> {
    if upto <= 0 {
        return Ok(Vec::new());
    }
    let rows = db::query(
        c,
        "WITH due AS (
            SELECT m.n FROM messages m
             WHERE m.to_slug = $1 AND m.state = 'queued'
               AND $2 <= COALESCE((SELECT seq FROM mailbox_heads WHERE slug = $1), 0)
               AND EXISTS (SELECT 1 FROM mailbox_log l WHERE l.slug = $1 AND l.message_n = m.n AND l.seq <= $2)
             ORDER BY m.n FOR UPDATE SKIP LOCKED)
         UPDATE messages m SET state = 'fetched', fetched_at = $3, receipts_pushed = false
           FROM due WHERE m.n = due.n AND m.state = 'queued'
         RETURNING m.from_slug",
        &[&slug, &upto, &clock::now()],
    )
    .await?;
    Ok(rows.iter().map(|r| r.get(0)).collect())
}

struct Batch {
    changes: Vec<Value>,
    roster: Vec<Value>,
    removed: Vec<String>,
    /// the position after this batch
    to: Cursor,
    more: bool,
    reset: bool,
}

impl Batch {
    fn news(&self) -> bool {
        !self.changes.is_empty() || !self.roster.is_empty() || !self.removed.is_empty() || self.reset || self.more
    }
}

/// Everything after `cur` (a bounded batch of it).
#[tracing::instrument(level = "debug", skip(hub))]
async fn sync_check(hub: &Hub, slug: &str, cur: Cursor) -> ApiResult<Batch> {
    let c = hub.db.get().await?;
    let heads = db::query_one(
        &c,
        "SELECT COALESCE((SELECT seq FROM mailbox_heads WHERE slug = $1), 0),
                (SELECT CASE WHEN is_called THEN last_value ELSE 0 END FROM roster_seq)",
        &[&slug],
    )
    .await?;
    let (mail_head, roster_head): (i64, i64) = (heads.get(0), heads.get(1));
    // a cursor beyond what this hub has written comes from another history
    // (a restored database, a re-created store): start the device over
    let reset = cur.mail > mail_head || cur.roster > roster_head;
    let from = if reset { Cursor { online: None, ..Cursor::default() } } else { cur };

    let rows = db::query(
        &c,
        &format!(
            "SELECT l.seq AS log_seq, l.deleted_id, m.sender_deleted_at, m.recipient_deleted_at, {ENVELOPE_COLS}
               FROM mailbox_log l LEFT JOIN messages m ON m.n = l.message_n
              WHERE l.slug = $1 AND l.seq > $2 ORDER BY l.seq LIMIT {}",
            SYNC_BATCH + 1
        ),
        &[&slug, &from.mail],
    )
    .await?;
    let more_mail = rows.len() > SYNC_BATCH;
    let mut to = from;
    let mut changes = Vec::new();
    for r in rows.iter().take(SYNC_BATCH) {
        to.mail = r.get("log_seq");
        // G4: this address deleted its copy (the row may be gone by now)
        if let Some(id) = r.get::<_, Option<String>>("deleted_id") {
            changes.push(json!({ "type": "deleted", "id": id }));
            continue;
        }
        // a message the retention sweep took between the two reads
        if r.get::<_, Option<i64>>("n").is_none() {
            continue;
        }
        let mine_gone = (r.get::<_, String>("from_slug") == slug && r.get::<_, Option<chrono::DateTime<chrono::Utc>>>("sender_deleted_at").is_some())
            || (r.get::<_, String>("to_slug") == slug && r.get::<_, Option<chrono::DateTime<chrono::Utc>>>("recipient_deleted_at").is_some());
        if mine_gone {
            changes.push(json!({ "type": "deleted", "id": r.get::<_, String>("id") }));
            continue;
        }
        let mut m = envelope_v2(r);
        m.insert("delivered_at".into(), json!(r.get::<_, Option<String>>("delivered_at")));
        m.insert("read_at".into(), json!(r.get::<_, Option<String>>("read_at")));
        changes.push(json!({ "type": "message", "message": m }));
    }

    let rows = db::query(
        &c,
        &format!(
            "SELECT seq, gone, slug, org_name, username, blurb, last_seen, kind FROM (
                SELECT roster_seq AS seq, false AS gone, slug, org_name, username, blurb, last_seen, kind
                  FROM identities WHERE roster_seq > $1
                UNION ALL
                SELECT seq, true, slug, '', '', '', NULL::timestamptz, '' FROM roster_gone WHERE seq > $1
             ) x ORDER BY seq LIMIT {}",
            ROSTER_BATCH + 1
        ),
        &[&from.roster],
    )
    .await?;
    let more_roster = rows.len() > ROSTER_BATCH;
    // the last word on each address wins (a leave, then a join again)
    let mut order: Vec<String> = Vec::new();
    let mut last: std::collections::HashMap<String, Option<Value>> = std::collections::HashMap::new();
    for r in rows.iter().take(ROSTER_BATCH) {
        to.roster = r.get(0);
        let slug: String = r.get(2);
        let entry = if r.get::<_, bool>(1) {
            None
        } else {
            Some(roster_json(hub, &slug, r.get(3), r.get(4), r.get(5), r.get(6), r.get(7)))
        };
        if last.insert(slug.clone(), entry).is_none() {
            order.push(slug);
        }
    }
    let mut roster = Vec::new();
    let mut removed = Vec::new();
    for slug in order {
        match last.remove(&slug).flatten() {
            Some(entry) => roster.push(entry),
            None => removed.push(slug),
        }
    }
    Ok(Batch { changes, roster, removed, to, more: more_mail || more_roster, reset })
}

#[tracing::instrument(level = "debug", skip_all)]
async fn sync_answer(hub: &Hub, slug: &str, cur: Cursor, b: Batch) -> ApiResult {
    let c = hub.db.get().await?;
    mark_seen(hub, &c, std::slice::from_ref(&slug.to_string())).await?;
    let online = hub.presence.online_now();
    let print = online_print(&online);
    let mut out = json!({
        "name": hub.cfg.hub_name,
        "cursor": b.to.render(print),
        "changes": b.changes,
        "roster": b.roster,
        "roster_removed": b.removed,
        "more": b.more,
    });
    if b.reset || cur.online != Some(print) {
        out["online"] = json!(online);
    }
    if b.reset {
        out["reset"] = json!(true);
    }
    ok(out)
}

/// `POST /api/sync {device_id, device_name?, cursor?, wait?, slug?}`.
#[tracing::instrument(level = "debug", skip_all, ret(level = "debug"), err(level = "debug", Debug))]
pub async fn sync(hub: &Arc<Hub>, req: &mut Req) -> ApiResult {
    let wait_query = req.query_float("wait", 25.0)?;
    let body = req.json_object_strict().await?;
    let slugs = authed(hub, req).await?;
    if slugs.is_empty() {
        return refuse(StatusCode::UNAUTHORIZED, "no valid org credentials");
    }
    let slug = one_address(&slugs, body.get("slug"))?;
    let device = device_id(body.get("device_id"))?;
    let name = device_name(body.get("device_name"))?;
    let cur = Cursor::parse(body.get("cursor"))?;
    let wait = match body.get("wait") {
        None | Some(Value::Null) => wait_query,
        Some(Value::Number(n)) => n.as_f64().unwrap_or(0.0),
        Some(_) => return refuse(StatusCode::UNPROCESSABLE_ENTITY, "wait must be a number of seconds"),
    };
    let wait = if wait.is_nan() { 0.0 } else { wait.clamp(0.0, POLL_CEILING) };
    let deadline = Instant::now() + Duration::from_secs_f64(wait);
    {
        let c = hub.db.get().await?;
        seen_device(&c, &slug, &device, name.as_deref()).await?;
        mark_seen(hub, &c, std::slice::from_ref(&slug)).await?;
        let senders = take_custody(&c, &slug, cur.mail).await?;
        hub.presence.wake(senders.iter().map(String::as_str));
    }
    let _parked = hub.presence.park(std::slice::from_ref(&slug));
    let slot = hub.presence.slot_of(&slug);
    let mut stop = false;
    loop {
        let listener = hub.presence.sync_listener(&slot);
        let batch = sync_check(hub, &slug, cur).await?;
        if batch.news() || stop || Instant::now() >= deadline {
            return sync_answer(hub, &slug, cur, batch).await;
        }
        tokio::select! {
            _ = listener.wait() => {}
            _ = tokio::time::sleep_until(deadline.into()) => {}
            _ = hub.shutdown.cancelled() => stop = true,
        }
    }
}

/// `GET /api/devices[?slug=]`: the devices the address syncs from.
#[tracing::instrument(level = "debug", skip_all, ret(level = "debug"), err(level = "debug", Debug))]
pub async fn devices(hub: &Arc<Hub>, req: &mut Req) -> ApiResult {
    let asked = req.query("slug").map(|s| Value::String(s.to_string()));
    let slugs = authed(hub, req).await?;
    if slugs.is_empty() {
        return refuse(StatusCode::UNAUTHORIZED, "no valid org credentials");
    }
    let slug = one_address(&slugs, asked.as_ref())?;
    let c = hub.db.get().await?;
    mark_seen(hub, &c, std::slice::from_ref(&slug)).await?;
    let rows = db::query(
        &c,
        "SELECT device_id, name, created_at, last_seen FROM devices WHERE slug = $1 ORDER BY created_at, device_id LIMIT $2",
        &[&slug, &DEVICES_MAX],
    )
    .await?;
    let now = clock::now();
    let window = chrono::Duration::from_std(PRESENCE_WINDOW).unwrap_or_default();
    let devices: Vec<Value> = rows
        .iter()
        .map(|r| {
            let seen: chrono::DateTime<chrono::Utc> = r.get(3);
            json!({
                "device_id": r.get::<_, String>(0),
                "name": r.get::<_, String>(1),
                "created_at": clock::iso(r.get(2)),
                "last_seen": clock::iso(seen),
                "online": now - seen < window,
            })
        })
        .collect();
    ok(json!({ "slug": slug, "devices": devices }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursors() {
        assert_eq!(Cursor::parse(None).ok(), Some(Cursor::default()));
        assert_eq!(Cursor::parse(Some(&json!(""))).ok(), Some(Cursor::default()));
        let c = Cursor { mail: 12, roster: 3, online: Some(0xabc) };
        assert_eq!(c.render(0xabc), "12-3-0000000000000abc");
        assert_eq!(Cursor::parse(Some(&json!("12-3-0000000000000abc"))).ok(), Some(c));
        for bad in [json!("12-3"), json!("x-3-0000000000000abc"), json!("-1-3-0000000000000abc"), json!("1-3-abc"), json!("+1-3-0000000000000abc"), json!(5)] {
            assert!(Cursor::parse(Some(&bad)).is_err(), "{bad}");
        }
    }

    #[test]
    fn device_ids() {
        assert!(device_id(Some(&json!("pixel-8_a.b:c/d+e=f"))).is_ok());
        assert!(device_id(Some(&json!("d".repeat(64)))).is_ok());
        for bad in [json!(""), json!("d".repeat(65)), json!("has space"), json!("tab\t"), json!("é"), json!(5)] {
            assert!(device_id(Some(&bad)).is_err(), "{bad}");
        }
    }
}
