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

use crate::api::Hub;
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
        let n = db::execute(
            &c,
            &format!(
                "DELETE FROM messages WHERE n IN
                   (SELECT n FROM messages WHERE received_at < $1 ORDER BY received_at, n LIMIT {BATCH})"
            ),
            &[&cut],
        )
        .await?;
        messages += n;
        if (n as i64) < BATCH {
            break;
        }
    }
    let org_cut = cutoff(hub.cfg.org_retention_days)?;
    // rows a concurrent send holds are skipped this hour, never raced
    let rows = db::query(
        &c,
        "WITH cand AS (
            SELECT slug FROM identities WHERE COALESCE(last_seen, registered_at) < $1
             ORDER BY slug FOR UPDATE SKIP LOCKED)
         DELETE FROM identities i USING cand
          WHERE i.slug = cand.slug
            AND NOT EXISTS (SELECT 1 FROM messages m WHERE m.state = 'queued' AND m.to_slug = i.slug)
         RETURNING i.slug",
        &[&org_cut],
    )
    .await?;
    let mut pruned: Vec<String> = rows.iter().map(|r| r.get(0)).collect();
    pruned.sort();
    for s in &pruned {
        hub.presence.forget(s);
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
