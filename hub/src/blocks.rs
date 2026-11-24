//! Blocking (feature `blocking`, v2.6.0, schema 16; Orgtree docket
//! blocking-contacts-on-the-hub-hubchat-1-4-0-orgtr, user 2026-11-24
//! 18:56Z and 19:06Z): an address blocks another, and the hub refuses the
//! blocked address's sends to it, with the words the user chose. The
//! database is the truth and `send` asks it inside its transaction; this
//! module also keeps a mirror in memory (loaded at start, changed only by
//! this hub's own writes) for what must not touch the database: typing
//! heartbeats and the sync print.
//!
//! A block belongs to its owner (the blocker's address and the fingerprint
//! it was set with) and counts only while that owner holds the address.

use std::sync::Arc;

use deadpool_postgres::GenericClient;

use crate::db;

/// Two-way (user 2026-11-24 19:06Z, Q2): while A blocks B, A cannot write
/// to B either until A lifts it. `false` makes it one-way; nothing else
/// depends on it.
pub const TWO_WAY: bool = true;

/// Blocks one address may hold.
pub const MAX_BLOCKS: usize = 10_000;

/// The blocks that count now, in memory: by blocker (oldest first) and by
/// blocked.
#[derive(Default)]
pub struct Blocks {
    by_blocker: papaya::HashMap<String, Arc<Vec<String>>>,
    by_blocked: papaya::HashMap<String, Arc<Vec<String>>>,
}

/// The rows that count: their owner still holds the address.
const COUNTING: &str = "SELECT b.blocker, b.blocked FROM blocks b JOIN identities i
                          ON i.slug = b.blocker AND i.fingerprint = b.blocker_fp";

impl Blocks {
    /// Everything, at start.
    pub async fn load(&self, c: &impl GenericClient) -> Result<(), tokio_postgres::Error> {
        let rows = db::query(c, &format!("{COUNTING} ORDER BY b.blocker, b.created_at, b.blocked"), &[]).await?;
        let mut by_blocker: std::collections::BTreeMap<String, Vec<String>> = Default::default();
        for r in &rows {
            by_blocker.entry(r.get(0)).or_default().push(r.get(1));
        }
        self.by_blocker.pin().clear();
        self.by_blocked.pin().clear();
        for (k, v) in by_blocker {
            self.put(&k, v);
        }
        Ok(())
    }

    /// One blocker's list as the database holds it now (after its write,
    /// its registering again, or its leaving).
    pub async fn reload(&self, c: &impl GenericClient, blocker: &str) -> Result<Vec<String>, tokio_postgres::Error> {
        let rows = db::query(c, &format!("{COUNTING} WHERE b.blocker = $1 ORDER BY b.created_at, b.blocked"), &[&blocker]).await?;
        let list: Vec<String> = rows.iter().map(|r| r.get(1)).collect();
        self.put(blocker, list.clone());
        Ok(list)
    }

    /// The blocker left (its blocks stop counting until it comes back).
    pub fn forget(&self, blocker: &str) {
        self.put(blocker, Vec::new());
    }

    fn put(&self, blocker: &str, list: Vec<String>) {
        let map = self.by_blocker.pin();
        let old = map.get(blocker).cloned().unwrap_or_default();
        if list.is_empty() {
            map.remove(blocker);
        } else {
            map.insert(blocker.to_string(), Arc::new(list.clone()));
        }
        let rev = self.by_blocked.pin();
        for gone in old.iter().filter(|o| !list.contains(o)) {
            rev.update(gone.clone(), |v| Arc::new(v.iter().filter(|b| b.as_str() != blocker).cloned().collect()));
            if rev.get(gone).is_some_and(|v| v.is_empty()) {
                rev.remove(gone);
            }
        }
        for new in list.iter().filter(|n| !old.contains(n)) {
            rev.update_or_insert_with(
                new.clone(),
                |v| {
                    let mut v = (**v).clone();
                    v.push(blocker.to_string());
                    v.sort();
                    Arc::new(v)
                },
                || Arc::new(vec![blocker.to_string()]),
            );
        }
    }

    /// Whom this address blocks, oldest first.
    pub fn of(&self, blocker: &str) -> Arc<Vec<String>> {
        self.by_blocker.pin().get(blocker).cloned().unwrap_or_default()
    }

    /// Who blocks this address, sorted.
    pub fn by(&self, blocked: &str) -> Arc<Vec<String>> {
        self.by_blocked.pin().get(blocked).cloned().unwrap_or_default()
    }

    /// Is anything from `from` to `to` stopped (memory; for heartbeats)?
    pub fn stops(&self, from: &str, to: &str) -> bool {
        self.of(to).iter().any(|b| b == from) || (TWO_WAY && self.of(from).iter().any(|b| b == to))
    }
}

/// Why a send from `from` to `to` is refused, if it is, asked inside the
/// send's transaction (the database, not the mirror). The JSON body of the
/// 403: `blocked_by` when `to` blocks `from`; `blocking` when `from` blocks
/// `to` (two-way only).
pub async fn refusal(tx: &impl GenericClient, from: &str, to: &str) -> Result<Option<serde_json::Value>, tokio_postgres::Error> {
    let rows = db::query(
        tx,
        &format!("{COUNTING} WHERE (b.blocker = $2 AND b.blocked = $1) OR (b.blocker = $1 AND b.blocked = $2)"),
        &[&from, &to],
    )
    .await?;
    let blocked_by = rows.iter().any(|r| r.get::<_, String>(0) == to);
    let blocking = rows.iter().any(|r| r.get::<_, String>(0) == from);
    Ok(if blocked_by {
        Some(serde_json::json!({ "detail": format!("you have been blocked by @{to} and cannot message them"), "blocked_by": to }))
    } else if TWO_WAY && blocking {
        Some(serde_json::json!({ "detail": format!("you blocked @{to}; unblock them to message them"), "blocking": to }))
    } else {
        None
    })
}
