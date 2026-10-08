//! v1 store → v2 database: `hub.sqlite3` (orgs, messages, attachments) into
//! PostgreSQL, in ONE transaction, so an import either happened completely
//! or not at all. The SQLite file is only read and is never changed or
//! removed: it stays the rollback. Blob files are shared (same
//! `<HUB_DATA>/blobs/<id>` layout); an import from another folder copies
//! them across.
//!
//! Every value is carried as v1 holds it. What PostgreSQL cannot hold (a NUL
//! character, a timestamp v1 never writes) is repaired and counted in the
//! report's `anomalies` instead of failing the hub's start.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use chrono::{DateTime, NaiveDateTime, SubsecRound, Utc};
use rusqlite::types::Value as Sql;
use serde_json::{json, Value};
use tokio::sync::mpsc;

use crate::blobs::blob_path;
use crate::clock;
use crate::config::Config;
use crate::db::{self, Db};
use crate::wire::py_float_repr;

const BATCH: i64 = 500;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Mode {
    /// only into a database that holds no records (keeps v1's message
    /// numbers `n`, so operator-view cursors carry over)
    Empty,
    /// beside existing records: rows whose address or id already exists are
    /// skipped and counted
    Merge,
}

struct OrgRow {
    slug: String,
    fingerprint: String,
    org_name: String,
    username: String,
    blurb: String,
    kind: String,
    registered_at: Sql,
    last_seen: Sql,
}

struct AttRow {
    id: String,
    owner: String,
    name: String,
    bytes: Sql,
    created_at: Sql,
    message_id: Option<String>,
}

struct MsgRow {
    rowid: i64,
    id: String,
    from: String,
    to: String,
    body: String,
    kind: Option<String>,
    thread_id: Option<String>,
    sent_at: Option<String>,
    received_at: Sql,
    state: Option<String>,
    fetched_at: Sql,
    delivered_at: Option<String>,
    read_at: Option<String>,
    receipts_pushed: Sql,
    attachments: Option<String>,
}

/// slug, fingerprint, org_name, username, blurb, kind — one vector each
type OrgColumns = (Vec<String>, Vec<String>, Vec<String>, Vec<String>, Vec<String>, Vec<String>);

enum Batch {
    Orgs(Vec<OrgRow>),
    Atts(Vec<AttRow>),
    Msgs(Vec<MsgRow>),
}

/// SQLite is dynamically typed: whatever a TEXT column holds, as v1 read it.
fn text(v: Sql) -> Option<String> {
    match v {
        Sql::Null => None,
        Sql::Integer(i) => Some(i.to_string()),
        Sql::Real(f) => Some(py_float_repr(f)),
        Sql::Text(s) => Some(s),
        Sql::Blob(b) => Some(String::from_utf8_lossy(&b).into_owned()),
    }
}

fn reader(path: PathBuf, tx: mpsc::Sender<Result<Batch>>) {
    let run = || -> Result<()> {
        let con = rusqlite::Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .or_else(|_| rusqlite::Connection::open(&path))
            .with_context(|| format!("could not open {}", path.display()))?;
        con.busy_timeout(std::time::Duration::from_secs(10))?;
        let has = |table: &str, col: &str| -> bool {
            con.prepare(&format!("PRAGMA table_info({table})"))
                .and_then(|mut s| s.query_map([], |r| r.get::<_, String>(1)).map(|m| m.flatten().any(|c| c == col)))
                .unwrap_or(false)
        };
        if !has("orgs", "slug") || !has("messages", "id") || !has("attachments", "id") {
            bail!("{} is not a v1 mail hub store (orgs, messages, attachments)", path.display());
        }
        let kind_col = if has("orgs", "kind") { "kind" } else { "'org'" };
        {
            let mut st = con.prepare(&format!(
                "SELECT slug, fingerprint, org_name, username, blurb, registered_at, last_seen, {kind_col} FROM orgs ORDER BY rowid"
            ))?;
            let rows = st.query_map([], |r| {
                Ok(OrgRow {
                    slug: text(r.get(0)?).unwrap_or_default(),
                    fingerprint: text(r.get(1)?).unwrap_or_default(),
                    org_name: text(r.get(2)?).unwrap_or_default(),
                    username: text(r.get(3)?).unwrap_or_default(),
                    blurb: text(r.get(4)?).unwrap_or_default(),
                    registered_at: r.get(5)?,
                    last_seen: r.get(6)?,
                    kind: text(r.get(7)?).unwrap_or_default(),
                })
            })?;
            let mut batch = Vec::new();
            for row in rows {
                batch.push(row?);
                if batch.len() as i64 >= BATCH {
                    tx.blocking_send(Ok(Batch::Orgs(std::mem::take(&mut batch))))?;
                }
            }
            if !batch.is_empty() {
                tx.blocking_send(Ok(Batch::Orgs(batch)))?;
            }
        }
        {
            let mut st =
                con.prepare("SELECT id, owner_slug, name, bytes, created_at, message_id FROM attachments ORDER BY rowid")?;
            let rows = st.query_map([], |r| {
                Ok(AttRow {
                    id: text(r.get(0)?).unwrap_or_default(),
                    owner: text(r.get(1)?).unwrap_or_default(),
                    name: text(r.get(2)?).unwrap_or_default(),
                    bytes: r.get(3)?,
                    created_at: r.get(4)?,
                    message_id: text(r.get(5)?),
                })
            })?;
            let mut batch = Vec::new();
            for row in rows {
                batch.push(row?);
                if batch.len() as i64 >= BATCH {
                    tx.blocking_send(Ok(Batch::Atts(std::mem::take(&mut batch))))?;
                }
            }
            if !batch.is_empty() {
                tx.blocking_send(Ok(Batch::Atts(batch)))?;
            }
        }
        let mut after: i64 = 0;
        loop {
            let mut st = con.prepare(
                "SELECT rowid, id, from_slug, to_slug, body, kind, thread_id, sent_at, received_at, state,
                        fetched_at, delivered_at, read_at, receipts_pushed, attachments
                   FROM messages WHERE rowid > ?1 ORDER BY rowid LIMIT ?2",
            )?;
            let batch: Vec<MsgRow> = st
                .query_map(rusqlite::params![after, BATCH], |r| {
                    Ok(MsgRow {
                        rowid: r.get(0)?,
                        id: text(r.get(1)?).unwrap_or_default(),
                        from: text(r.get(2)?).unwrap_or_default(),
                        to: text(r.get(3)?).unwrap_or_default(),
                        body: text(r.get(4)?).unwrap_or_default(),
                        kind: text(r.get(5)?),
                        thread_id: text(r.get(6)?),
                        sent_at: text(r.get(7)?),
                        received_at: r.get(8)?,
                        state: text(r.get(9)?),
                        fetched_at: r.get(10)?,
                        delivered_at: text(r.get(11)?),
                        read_at: text(r.get(12)?),
                        receipts_pushed: r.get(13)?,
                        attachments: text(r.get(14)?),
                    })
                })?
                .collect::<Result<_, _>>()?;
            let Some(last) = batch.last() else { break };
            after = last.rowid;
            let full = batch.len() as i64 >= BATCH;
            tx.blocking_send(Ok(Batch::Msgs(batch)))?;
            if !full {
                break;
            }
        }
        Ok(())
    };
    if let Err(e) = run() {
        let _ = tx.blocking_send(Err(e));
    }
}

#[derive(Default)]
struct Tally {
    anomalies: std::collections::BTreeMap<String, u64>,
}

impl Tally {
    fn note(&mut self, what: &str) {
        *self.anomalies.entry(what.to_string()).or_default() += 1;
    }

    fn clean(&mut self, field: &str, s: String) -> String {
        if s.contains('\0') {
            self.note(&format!("NUL removed from {field}"));
            s.replace('\0', "")
        } else {
            s
        }
    }

    fn clean_opt(&mut self, field: &str, s: Option<String>) -> Option<String> {
        s.map(|s| self.clean(field, s))
    }

    /// A v1 hub-clock value (`2026-10-08T12:00:00.123Z`).
    fn time(&mut self, field: &str, v: Sql) -> Option<DateTime<Utc>> {
        let s = text(v)?;
        if let Some(t) = clock::parse(&s) {
            return Some(t.trunc_subsecs(3));
        }
        for f in ["%Y-%m-%d %H:%M:%S%.f", "%Y-%m-%dT%H:%M:%S%.f"] {
            if let Ok(t) = NaiveDateTime::parse_from_str(&s, f) {
                return Some(t.and_utc().trunc_subsecs(3));
            }
        }
        self.note(&format!("unreadable {field}"));
        None
    }
}

#[derive(Debug)]
pub struct Report {
    pub json: Value,
}

/// Import the v1 store at `sqlite` into `db`; blob files land in
/// `blob_dir`.
#[tracing::instrument(level = "info", skip(db), ret(level = "info"), err(level = "warn"))]
pub async fn import(db: &Db, sqlite: &Path, blob_dir: &Path, mode: Mode) -> Result<Report> {
    if !sqlite.is_file() {
        bail!("no v1 store at {}", sqlite.display());
    }
    let source_blobs = sqlite.parent().map(|p| p.join("blobs")).unwrap_or_default();
    let same_blobs = match (std::fs::canonicalize(&source_blobs), std::fs::canonicalize(blob_dir)) {
        (Ok(a), Ok(b)) => a == b,
        _ => source_blobs == blob_dir,
    };
    std::fs::create_dir_all(blob_dir).ok();
    let meta = std::fs::metadata(sqlite)?;
    let mut c = db.get().await?;
    let txn = c.transaction().await?;
    if mode == Mode::Empty {
        let busy: bool = db::query_one(
            &txn,
            "SELECT EXISTS (SELECT 1 FROM identities) OR EXISTS (SELECT 1 FROM messages) OR EXISTS (SELECT 1 FROM attachments)",
            &[],
        )
        .await?
        .get(0);
        if busy {
            bail!("the database already holds hub records; import beside them with --merge");
        }
    }
    let (tx, mut rx) = mpsc::channel::<Result<Batch>>(2);
    let path = sqlite.to_path_buf();
    let reader_thread = std::thread::spawn(move || reader(path, tx));
    let now = clock::now();
    let mut t = Tally::default();
    let (mut orgs, mut atts, mut msgs, mut skipped, mut copied, mut missing_blobs) = (0u64, 0u64, 0u64, 0u64, 0u64, 0u64);
    while let Some(batch) = rx.recv().await {
        match batch? {
            Batch::Orgs(rows) => {
                let n = rows.len() as u64;
                let mut cols: OrgColumns = Default::default();
                let mut reg: Vec<DateTime<Utc>> = Vec::new();
                let mut seen: Vec<Option<DateTime<Utc>>> = Vec::new();
                for r in rows {
                    cols.0.push(t.clean("slug", r.slug));
                    cols.1.push(t.clean("fingerprint", r.fingerprint));
                    cols.2.push(t.clean("org_name", r.org_name));
                    cols.3.push(t.clean("username", r.username));
                    cols.4.push(t.clean("blurb", r.blurb));
                    cols.5.push(if r.kind.is_empty() { "org".into() } else { t.clean("kind", r.kind) });
                    reg.push(t.time("registered_at", r.registered_at).unwrap_or(now));
                    seen.push(t.time("last_seen", r.last_seen));
                }
                let done = db::execute(
                    &txn,
                    "INSERT INTO identities (slug, fingerprint, org_name, username, blurb, kind, registered_at, last_seen)
                     SELECT * FROM UNNEST($1::text[], $2::text[], $3::text[], $4::text[], $5::text[], $6::text[],
                                          $7::timestamptz[], $8::timestamptz[])
                     ON CONFLICT (slug) DO NOTHING",
                    &[&cols.0, &cols.1, &cols.2, &cols.3, &cols.4, &cols.5, &reg, &seen],
                )
                .await?;
                orgs += done;
                skipped += n - done;
            }
            Batch::Atts(rows) => {
                let n = rows.len() as u64;
                let mut ids = Vec::new();
                let mut owners = Vec::new();
                let mut names = Vec::new();
                let mut sizes: Vec<i64> = Vec::new();
                let mut created: Vec<DateTime<Utc>> = Vec::new();
                let mut bound: Vec<Option<String>> = Vec::new();
                for r in rows {
                    let id = t.clean("attachment id", r.id);
                    if !same_blobs {
                        if let (Some(from), Some(to)) = (blob_path(&source_blobs, &id), blob_path(blob_dir, &id)) {
                            if from.is_file() && !to.exists() {
                                tokio::fs::copy(&from, &to).await.with_context(|| format!("could not copy blob {id}"))?;
                                copied += 1;
                            } else if !from.is_file() {
                                missing_blobs += 1;
                            }
                        }
                    }
                    ids.push(id);
                    owners.push(t.clean("owner_slug", r.owner));
                    names.push(t.clean("attachment name", r.name));
                    sizes.push(match r.bytes {
                        Sql::Integer(i) => i,
                        other => text(other).and_then(|s| s.trim().parse().ok()).unwrap_or_else(|| {
                            t.note("unreadable attachment bytes");
                            0
                        }),
                    });
                    created.push(t.time("attachment created_at", r.created_at).unwrap_or(now));
                    bound.push(t.clean_opt("message_id", r.message_id));
                }
                let done = db::execute(
                    &txn,
                    "INSERT INTO attachments (id, owner_slug, name, bytes, created_at, message_id)
                     SELECT * FROM UNNEST($1::text[], $2::text[], $3::text[], $4::bigint[], $5::timestamptz[], $6::text[])
                     ON CONFLICT (id) DO NOTHING",
                    &[&ids, &owners, &names, &sizes, &created, &bound],
                )
                .await?;
                atts += done;
                skipped += n - done;
            }
            Batch::Msgs(rows) => {
                let n = rows.len() as u64;
                let mut num: Vec<i64> = Vec::new();
                let mut ids = Vec::new();
                let mut froms = Vec::new();
                let mut tos = Vec::new();
                let mut bodies = Vec::new();
                let mut kinds = Vec::new();
                let mut threads = Vec::new();
                let mut sents = Vec::new();
                let mut received: Vec<DateTime<Utc>> = Vec::new();
                let mut states = Vec::new();
                let mut fetched: Vec<Option<DateTime<Utc>>> = Vec::new();
                let mut delivered = Vec::new();
                let mut read = Vec::new();
                let mut pushed: Vec<bool> = Vec::new();
                let mut atts_json: Vec<Value> = Vec::new();
                for r in rows {
                    num.push(r.rowid);
                    ids.push(t.clean("message id", r.id));
                    froms.push(t.clean("from_slug", r.from));
                    tos.push(t.clean("to_slug", r.to));
                    bodies.push(t.clean("body", r.body));
                    kinds.push(t.clean_opt("kind", r.kind));
                    threads.push(t.clean_opt("thread_id", r.thread_id));
                    sents.push(t.clean_opt("sent_at", r.sent_at));
                    received.push(t.time("received_at", r.received_at).unwrap_or_else(|| {
                        t.note("received_at replaced by the import time");
                        now
                    }));
                    let f = t.time("fetched_at", r.fetched_at);
                    states.push(match r.state.as_deref() {
                        Some("queued") => "queued".to_string(),
                        Some("fetched") => "fetched".to_string(),
                        _ => {
                            t.note("unknown message state");
                            if f.is_some() { "fetched" } else { "queued" }.to_string()
                        }
                    });
                    fetched.push(f);
                    delivered.push(t.clean_opt("delivered_at", r.delivered_at));
                    read.push(t.clean_opt("read_at", r.read_at));
                    pushed.push(match r.receipts_pushed {
                        Sql::Null => true,
                        Sql::Integer(i) => i != 0,
                        other => text(other).map(|s| s.trim() != "0").unwrap_or(true),
                    });
                    let a = r.attachments.unwrap_or_else(|| "[]".into());
                    atts_json.push(match serde_json::from_str::<Value>(&a) {
                        Ok(v @ Value::Array(_)) => strip_nul_json(v),
                        _ => {
                            t.note("unreadable attachments list replaced by []");
                            json!([])
                        }
                    });
                }
                let sql = if mode == Mode::Empty {
                    "INSERT INTO messages (n, id, from_slug, to_slug, body, kind, thread_id, sent_at, received_at, state,
                                           fetched_at, delivered_at, read_at, receipts_pushed, attachments)
                     SELECT * FROM UNNEST($1::bigint[], $2::text[], $3::text[], $4::text[], $5::text[], $6::text[], $7::text[],
                                          $8::text[], $9::timestamptz[], $10::text[], $11::timestamptz[], $12::text[],
                                          $13::text[], $14::bool[], $15::jsonb[])
                     ON CONFLICT DO NOTHING"
                } else {
                    "INSERT INTO messages (id, from_slug, to_slug, body, kind, thread_id, sent_at, received_at, state,
                                           fetched_at, delivered_at, read_at, receipts_pushed, attachments)
                     SELECT i, f, t, b, k, th, s, r, st, fe, d, rd, p, a
                       FROM UNNEST($1::bigint[], $2::text[], $3::text[], $4::text[], $5::text[], $6::text[], $7::text[],
                                   $8::text[], $9::timestamptz[], $10::text[], $11::timestamptz[], $12::text[],
                                   $13::text[], $14::bool[], $15::jsonb[])
                            AS u(n, i, f, t, b, k, th, s, r, st, fe, d, rd, p, a)
                      ORDER BY n
                     ON CONFLICT DO NOTHING"
                };
                let done = db::execute(
                    &txn,
                    sql,
                    &[
                        &num, &ids, &froms, &tos, &bodies, &kinds, &threads, &sents, &received, &states, &fetched, &delivered,
                        &read, &pushed, &atts_json,
                    ],
                )
                .await?;
                msgs += done;
                skipped += n - done;
            }
        }
    }
    reader_thread.join().map_err(|_| anyhow::anyhow!("the SQLite reader thread failed"))?;
    db::execute(&txn, "SELECT setval(pg_get_serial_sequence('messages', 'n'), GREATEST((SELECT max(n) FROM messages), 1))", &[])
        .await?;
    // every imported message reaches its addresses' devices (G1)
    db::backfill_sync(&txn).await?;
    let modified: Option<DateTime<Utc>> = meta.modified().ok().map(Into::into);
    let report = json!({
        "source": sqlite.display().to_string(),
        "source_bytes": meta.len(),
        "source_modified": modified.map(clock::iso),
        "imported_at": clock::iso(now),
        "mode": if mode == Mode::Empty { "empty" } else { "merge" },
        "orgs": orgs,
        "messages": msgs,
        "attachments": atts,
        "skipped_existing": skipped,
        "blobs_copied": copied,
        "blobs_missing": missing_blobs,
        "anomalies": t.anomalies,
    });
    db::execute(
        &txn,
        "INSERT INTO hub_meta (k, v) VALUES ('sqlite_import', $1) ON CONFLICT (k) DO UPDATE SET v = EXCLUDED.v",
        &[&report],
    )
    .await?;
    txn.commit().await.context("the import could not be committed")?;
    Ok(Report { json: report })
}

fn strip_nul_json(v: Value) -> Value {
    match v {
        Value::String(s) if s.contains('\0') => Value::String(s.replace('\0', "")),
        Value::Array(a) => Value::Array(a.into_iter().map(strip_nul_json).collect()),
        Value::Object(o) => Value::Object(o.into_iter().map(|(k, v)| (k.replace('\0', ""), strip_nul_json(v))).collect()),
        other => other,
    }
}

/// At startup: a v1 store in `HUB_DATA` is imported once, into an empty
/// database (upgrading in place is "replace the image and restart", as it
/// was for v1). A database that already holds records is left alone.
#[tracing::instrument(level = "debug", skip_all)]
pub async fn auto_import(cfg: &Config, db: &Db) -> Result<Option<Report>> {
    let path = cfg.sqlite_path();
    if !cfg.import_sqlite || !path.is_file() {
        return Ok(None);
    }
    let c = db.get().await?;
    if db::query_opt(&c, "SELECT 1 FROM hub_meta WHERE k = 'sqlite_import'", &[]).await?.is_some() {
        return Ok(None);
    }
    let busy: bool = db::query_one(
        &c,
        "SELECT EXISTS (SELECT 1 FROM identities) OR EXISTS (SELECT 1 FROM messages) OR EXISTS (SELECT 1 FROM attachments)",
        &[],
    )
    .await?
    .get(0);
    drop(c);
    if busy {
        tracing::warn!(
            path = %path.display(),
            "a v1 store is present but the database already holds hub records; not importing it (orgtree-mailhub import-sqlite --merge imports it beside them)"
        );
        return Ok(None);
    }
    let report = import(db, &path, &cfg.blob_dir(), Mode::Empty).await?;
    let out = cfg.data_dir.join("v2-import-report.json");
    let _ = std::fs::write(&out, serde_json::to_vec_pretty(&report.json).unwrap_or_default());
    Ok(Some(report))
}
