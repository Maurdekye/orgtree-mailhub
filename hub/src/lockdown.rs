//! Lockdown (feature `lockdown`, v2.6.0, schema 17; Orgtree docket
//! org-mailbox-lockdown-allow-list-mode-that-denies, user 2026-11-25 13:26Z
//! "a final last-straw defense against an onslaught of attackers creating
//! dozens of new accounts and flooding the inbox faster than they can be
//! blocked"): an address in lockdown takes messages only from the addresses
//! on its list. The hub refuses everyone else's sends to it (`refusal`,
//! asked inside `send`'s transaction) and drops their typing heartbeats.
//! Everyone sees the lock: a viewer on the list sees the address as usual,
//! `locked: "open"`; anyone else sees `locked: "closed"` and none of its
//! presence (user 13:37Z, Q2), in sync and poll answers, the roster and the
//! directory. The address itself may still write to anyone (Q4).
//!
//! As with blocks, the database is the truth for `send`, and a mirror in
//! memory (loaded at start, changed only by this hub's own writes) serves
//! what must not touch the database: heartbeats, the sync print and the
//! views. A lockdown belongs to its owner (the address and the fingerprint
//! it was set with) and counts only while that owner holds the address.

use std::sync::Arc;

use deadpool_postgres::GenericClient;
use serde_json::{json, Value};

use crate::db;

/// Addresses one list may hold.
pub const MAX_ALLOWED: usize = 1_000;

/// One lockdown that counts: since when, and its list (sorted).
#[derive(Clone, Debug, PartialEq)]
pub struct Lockdown {
    pub since: chrono::DateTime<chrono::Utc>,
    pub allow: Arc<Vec<String>>,
}

#[derive(Default)]
pub struct Lockdowns {
    by_address: papaya::HashMap<String, Lockdown>,
}

/// The rows that count: their owner still holds the address.
const COUNTING: &str = "SELECT l.address, l.since,
                               COALESCE((SELECT array_agg(a.allowed ORDER BY a.allowed) FROM lockdown_allow a WHERE a.address = l.address), '{}')
                          FROM lockdowns l JOIN identities i ON i.slug = l.address AND i.fingerprint = l.owner_fp";

impl Lockdowns {
    /// Everything, at start.
    pub async fn load(&self, c: &impl GenericClient) -> Result<(), tokio_postgres::Error> {
        let rows = db::query(c, COUNTING, &[]).await?;
        let map = self.by_address.pin();
        map.clear();
        for r in &rows {
            map.insert(r.get(0), Lockdown { since: r.get(1), allow: Arc::new(r.get(2)) });
        }
        Ok(())
    }

    /// One address's lockdown as the database holds it now (after its
    /// write, its registering again, or its leaving).
    pub async fn reload(&self, c: &impl GenericClient, address: &str) -> Result<Option<Lockdown>, tokio_postgres::Error> {
        let row = db::query_opt(c, &format!("{COUNTING} WHERE l.address = $1"), &[&address]).await?;
        let now = row.map(|r| Lockdown { since: r.get(1), allow: Arc::new(r.get(2)) });
        let map = self.by_address.pin();
        match &now {
            Some(l) => {
                map.insert(address.to_string(), l.clone());
            }
            None => {
                map.remove(address);
            }
        }
        Ok(now)
    }

    /// The address left (its lockdown stops counting until it comes back).
    pub fn forget(&self, address: &str) {
        self.by_address.pin().remove(address);
    }

    pub fn of(&self, address: &str) -> Option<Lockdown> {
        self.by_address.pin().get(address).cloned()
    }

    pub fn any(&self) -> bool {
        !self.by_address.pin().is_empty()
    }

    /// How `viewers` see `address`: None when it is not in lockdown (or is
    /// one of them), else "open" when any of them is on its list, else
    /// "closed".
    pub fn seen_by(&self, address: &str, viewers: &[String]) -> Option<&'static str> {
        if viewers.iter().any(|v| v == address) {
            return None;
        }
        let map = self.by_address.pin();
        let l = map.get(address)?;
        Some(if viewers.iter().any(|v| l.allow.binary_search(v).is_ok()) { "open" } else { "closed" })
    }

    /// Is `address` closed to all of `viewers`?
    pub fn closed_to(&self, address: &str, viewers: &[String]) -> bool {
        self.seen_by(address, viewers) == Some("closed")
    }

    /// Does the lockdown of `to` stop anything from `from` (heartbeats)?
    pub fn stops(&self, from: &str, to: &str) -> bool {
        self.closed_to(to, std::slice::from_ref(&from.to_string()))
    }

    /// `{address: "open"|"closed"}` for every address in lockdown but the
    /// viewers' own, as the viewers see it.
    pub fn view(&self, viewers: &[String]) -> serde_json::Map<String, Value> {
        let map = self.by_address.pin();
        let mut out = serde_json::Map::new();
        let mut keys: Vec<&String> = map.keys().collect();
        keys.sort();
        for a in keys {
            if let Some(s) = self.seen_by(a, viewers) {
                out.insert(a.clone(), json!(s));
            }
        }
        out
    }

    /// One roster or directory entry as `viewers` see it: `locked` when its
    /// address is in lockdown, and, closed, none of its presence.
    pub fn mask(&self, entry: &mut Value, viewers: &[String]) {
        let Some(slug) = entry["slug"].as_str().map(str::to_string) else { return };
        if let Some(s) = self.seen_by(&slug, viewers) {
            entry["locked"] = json!(s);
            if s == "closed" {
                for k in ["online", "busy", "dnd", "active"] {
                    entry[k] = json!(false);
                }
                for k in ["last_seen", "active_at"] {
                    entry[k] = Value::Null;
                }
            }
        }
    }

    pub fn mask_all(&self, entries: &mut [Value], viewers: &[String]) {
        if self.any() {
            for e in entries {
                self.mask(e, viewers);
            }
        }
    }

    /// A presence set without the addresses closed to `viewers`.
    pub fn filter_set(&self, set: &[String], viewers: &[String]) -> Vec<String> {
        set.iter().filter(|s| !self.closed_to(s, viewers)).cloned().collect()
    }
}

/// `{"on", "allow", "since"}` for an address's own lockdown.
pub fn own(l: Option<&Lockdown>) -> Value {
    match l {
        Some(l) => json!({ "on": true, "allow": *l.allow, "since": crate::clock::iso(l.since) }),
        None => json!({ "on": false, "allow": [], "since": null }),
    }
}

/// Why a send from `from` to `to` is refused, if `to` is in lockdown and
/// `from` is not on its list, asked inside the send's transaction (the
/// database, not the mirror). The JSON body of the 403.
pub async fn refusal(tx: &impl GenericClient, from: &str, to: &str) -> Result<Option<Value>, tokio_postgres::Error> {
    if from == to {
        return Ok(None);
    }
    let locked = db::query_opt(
        tx,
        "SELECT NOT EXISTS (SELECT 1 FROM lockdown_allow a WHERE a.address = l.address AND a.allowed = $1)
           FROM lockdowns l JOIN identities i ON i.slug = l.address AND i.fingerprint = l.owner_fp
          WHERE l.address = $2",
        &[&from, &to],
    )
    .await?;
    Ok(match locked {
        Some(r) if r.get::<_, bool>(0) => Some(json!({
            "detail": format!("@{to} is in lockdown and only takes messages from addresses on its list"),
            "locked": to,
        })),
        _ => None,
    })
}
