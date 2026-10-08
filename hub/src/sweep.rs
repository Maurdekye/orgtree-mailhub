//! Retention, as v1: at startup and then hourly, messages and attachments
//! older than `HUB_RETENTION_DAYS` go (the attachment's file first, then its
//! row), and roster rows silent longer than `HUB_ORG_RETENTION_DAYS` go —
//! except an address still holding queued mail, so a delivery is never
//! stranded. Work happens in bounded batches, each its own short
//! transaction.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use chrono::Utc;
use serde_json::json;

use crate::api::{sync, Hub};
use crate::blobs::blob_path;
use crate::clock;
use crate::db;

const BATCH: i64 = 1000;
pub const INTERVAL: Duration = Duration::from_secs(3600);

#[derive(Debug)]
pub struct Swept {
    pub messages: u64,
    pub attachments: u64,
    pub pruned: Vec<String>,
}

fn cutoff(days: i64) -> Result<chrono::DateTime<Utc>> {
    let span = chrono::Duration::try_days(days).ok_or_else(|| anyhow!("retention of {days} days is out of range"))?;
    Utc::now().checked_sub_signed(span).ok_or_else(|| anyhow!("retention of {days} days is out of range"))
}

#[tracing::instrument(level = "debug", skip_all, ret(level = "debug"), err(level = "warn"))]
pub async fn run_once(hub: &Hub) -> Result<Swept> {
    let cut = cutoff(hub.cfg.retention_days)?;
    let dir = hub.cfg.blob_dir();
    let c = hub.db.get().await?;
    let mut attachments = 0u64;
    loop {
        let rows = db::query(
            &c,
            &format!("SELECT id FROM attachments WHERE created_at < $1 ORDER BY created_at LIMIT {BATCH}"),
            &[&cut],
        )
        .await?;
        if rows.is_empty() {
            break;
        }
        let ids: Vec<String> = rows.iter().map(|r| r.get(0)).collect();
        for id in &ids {
            if let Some(p) = blob_path(&dir, id) {
                let _ = tokio::fs::remove_file(p).await;
            }
        }
        attachments += db::execute(&c, "DELETE FROM attachments WHERE id = ANY($1)", &[&ids]).await?;
        if (ids.len() as i64) < BATCH {
            break;
        }
    }
    let mut messages = 0u64;
    loop {
        // their change-log entries go with them (a device that has a swept
        // message keeps its copy; one that has not never hears of it)
        let n: i64 = db::query_one(
            &c,
            &format!(
                "WITH gone AS (
                    DELETE FROM messages WHERE n IN
                      (SELECT n FROM messages WHERE received_at < $1 ORDER BY received_at, n LIMIT {BATCH})
                    RETURNING n),
                 logs AS (DELETE FROM mailbox_log WHERE message_n IN (SELECT n FROM gone))
                 SELECT count(*) FROM gone"
            ),
            &[&cut],
        )
        .await?
        .get(0);
        messages += n as u64;
        if n < BATCH {
            break;
        }
    }
    let org_cut = cutoff(hub.cfg.org_retention_days)?;
    let mut c = c;
    let tx = c.transaction().await?;
    sync::roster_lock(&tx).await?;
    // rows a concurrent send holds are skipped this hour, never raced
    let rows = db::query(
        &tx,
        "SELECT slug FROM identities i WHERE COALESCE(last_seen, registered_at) < $1
            AND NOT EXISTS (SELECT 1 FROM messages m WHERE m.state = 'queued' AND m.to_slug = i.slug)
          ORDER BY slug FOR UPDATE SKIP LOCKED",
        &[&org_cut],
    )
    .await?;
    let idle: Vec<String> = rows.iter().map(|r| r.get(0)).collect();
    let pruned = sync::leave(&tx, &idle).await?;
    tx.commit().await?;
    for s in &pruned {
        hub.presence.forget(s);
    }
    if !pruned.is_empty() {
        hub.presence.roster_changed();
    }
    Ok(Swept { messages, attachments, pruned })
}

/// One pass and its stdout line (v1 printed only when something went).
pub async fn pass(hub: &Hub) {
    match run_once(hub).await {
        Ok(s) => {
            if s.messages > 0 || !s.pruned.is_empty() {
                let mut v = json!({ "ts": clock::now_iso(), "sweep": s.messages });
                if !s.pruned.is_empty() {
                    v["orgs_pruned"] = json!(s.pruned);
                }
                crate::log::line(&v);
            }
        }
        Err(e) => {
            tracing::warn!(error = %format!("{e:#}"), "retention sweep failed");
            crate::log::line(&json!({ "ts": clock::now_iso(), "sweep_error": format!("{e:#}") }));
        }
    }
}

pub async fn sweep_loop(hub: Arc<Hub>) {
    loop {
        pass(&hub).await;
        tokio::select! {
            _ = tokio::time::sleep(INTERVAL) => {}
            _ = hub.shutdown.cancelled() => return,
        }
    }
}
