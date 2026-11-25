//! Lockdown (feature `lockdown`, v2.6.0, schema 17; Orgtree docket
//! org-mailbox-lockdown-allow-list-mode-that-denies, user 2026-11-25 13:26Z
//! "a final last-straw defense against an onslaught of attackers creating
//! dozens of new accounts and flooding the inbox faster than they can be
//! blocked"): an address in lockdown takes messages only from the
//! identities on its list. The hub refuses everyone else's sends to it
//! (`refusal`, asked inside `send`'s transaction) and drops their typing
//! heartbeats. Everyone sees the lock: a viewer on the list sees the address
//! as usual, `locked: "open"`; anyone else sees `locked: "closed"` and none
//! of its presence (user 13:37Z, Q2) in sync and poll answers (and so the
//! stream, which is built from them), the roster and the directory. The
//! address itself may still write to anyone (Q4).
//!
//! An allow entry is an identity: the address and the fingerprint that held
//! it when it was listed. It counts only while that identity holds the
//! address (review B1, 2026-11-25: a freed address re-registered by someone
//! else is not on the list). A lockdown belongs to its owner (address +
//! fingerprint), as a block does, and its changes are ordered by `rev`.
//!
//! As with blocks, the database is the truth for `send`, and a mirror in
//! memory (loaded at start, changed only by this hub's own writes) serves
//! what must not touch the database: heartbeats, the sync print and the
//! views. The mirror holds, per address in lockdown, the listed addresses
//! that count now; anything that changes who holds an address refreshes the
//! lockdowns listing it (`refresh_listing`).

use std::sync::Arc;

use deadpool_postgres::GenericClient;
use serde_json::{json, Value};

use crate::db;

/// Addresses one list may hold.
pub const MAX_ALLOWED: usize = 1_000;

/// One lockdown that is on and counts: since when, its revision, and the
/// listed addresses that count now (sorted).
#[derive(Clone, Debug, PartialEq)]
pub struct Lockdown {
    pub since: Option<chrono::DateTime<chrono::Utc>>,
    pub rev: i64,
    pub allow: Arc<Vec<String>>,
}

#[derive(Default)]
pub struct Lockdowns {
    by_address: papaya::HashMap<String, Lockdown>,
}

/// The lockdowns that count (on, and their owner still holds the address),
/// each with the entries that count (their identity still holds theirs).
const COUNTING: &str = "SELECT l.address, l.since, l.rev,
         COALESCE((SELECT array_agg(a.allowed ORDER BY a.allowed) FROM lockdown_allow a
                     JOIN identities j ON j.slug = a.allowed AND j.fingerprint = a.allowed_fp
                    WHERE a.address = l.address), '{}')
    FROM lockdowns l JOIN identities i ON i.slug = l.address AND i.fingerprint = l.owner_fp
   WHERE l.on_";

fn row_lockdown(r: &tokio_postgres::Row) -> Lockdown {
    Lockdown { since: r.get(1), rev: r.get(2), allow: Arc::new(r.get(3)) }
}

impl Lockdowns {
    /// Everything, at start.
    pub async fn load(&self, c: &impl GenericClient) -> Result<(), tokio_postgres::Error> {
        let rows = db::query(c, COUNTING, &[]).await?;
        let map = self.by_address.pin();
        map.clear();
        for r in &rows {
            map.insert(r.get(0), row_lockdown(r));
        }
        Ok(())
    }

    /// One address's lockdown as the database holds it now (after its
    /// write, its registering again, or its leaving).
    pub async fn reload(&self, c: &impl GenericClient, address: &str) -> Result<Option<Lockdown>, tokio_postgres::Error> {
        let row = db::query_opt(c, &format!("{COUNTING} AND l.address = $1"), &[&address]).await?;
        let now = row.as_ref().map(row_lockdown);
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

    /// Who holds `slug` changed (it registered, left or was pruned): reload
    /// every lockdown that lists it. Answers those whose view moved, whose
    /// roster entries must go out again.
    pub async fn refresh_listing(&self, c: &impl GenericClient, slug: &str) -> Result<Vec<String>, tokio_postgres::Error> {
        let rows = db::query(c, "SELECT DISTINCT address FROM lockdown_allow WHERE allowed = $1", &[&slug]).await?;
        let mut moved = Vec::new();
        for r in &rows {
            let a: String = r.get(0);
            let before = self.of(&a);
            if self.reload(c, &a).await? != before {
                moved.push(a);
            }
        }
        Ok(moved)
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

    /// Does the lockdown of `to` stop anything from `from` (heartbeats; the
    /// cheap first look of `send`)?
    pub fn stops(&self, from: &str, to: &str) -> bool {
        self.closed_to(to, std::slice::from_ref(&from.to_string()))
    }

    /// `{address: "open"|"closed"}` for every address in lockdown but the
    /// viewers' own, as the viewers see it.
    pub fn view(&self, viewers: &[String]) -> serde_json::Map<String, Value> {
        let map = self.by_address.pin();
        let mut keys: Vec<&String> = map.keys().collect();
        keys.sort();
        let mut out = serde_json::Map::new();
        for a in keys {
            if let Some(s) = self.seen_by(a, viewers) {
                out.insert(a.clone(), json!(s));
            }
        }
        out
    }

    /// What `viewers` may see of one roster or directory entry (review I7:
    /// the one place that decides it for every route): `locked` when its
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

    /// One answer shared by several viewers (a poll for several orgs): an
    /// entry shows its presence only when it is open to every one of them
    /// (review 5).
    pub fn mask_all_strict(&self, entries: &mut [Value], viewers: &[String]) {
        if !self.any() {
            return;
        }
        for e in entries {
            let Some(slug) = e["slug"].as_str().map(str::to_string) else { continue };
            let others: Vec<String> = viewers.iter().filter(|v| **v != slug).cloned().collect();
            if others.is_empty() {
                continue;
            }
            if others.iter().any(|v| self.closed_to(&slug, std::slice::from_ref(v))) {
                let closer: Vec<String> = others.iter().filter(|v| self.closed_to(&slug, std::slice::from_ref(*v))).take(1).cloned().collect();
                self.mask(e, &closer);
            } else {
                self.mask(e, &others);
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

/// An address's own lockdown from the mirror (sync and poll answers):
/// `{"on", "since", "rev", "allow"}`, `allow` being the listed addresses that
/// count now. `GET /api/lockdown` answers the full entries.
pub fn own(l: Option<&Lockdown>) -> Value {
    match l {
        Some(l) => json!({ "on": true, "allow": *l.allow, "since": l.since.map(crate::clock::iso), "rev": l.rev }),
        None => json!({ "on": false, "allow": [], "since": null }),
    }
}

/// Why a send from `from` to `to` is refused, if `to` is in lockdown and
/// `from` is not a listed identity, asked of the database (inside the send's
/// transaction for the decision; on any connection for the early look). The
/// JSON body of the 403.
pub async fn refusal(c: &impl GenericClient, from: &str, to: &str) -> Result<Option<Value>, tokio_postgres::Error> {
    if from == to {
        return Ok(None);
    }
    let locked = db::query_opt(
        c,
        "SELECT NOT EXISTS (SELECT 1 FROM lockdown_allow a JOIN identities j ON j.slug = a.allowed AND j.fingerprint = a.allowed_fp
                             WHERE a.address = l.address AND a.allowed = $1)
           FROM lockdowns l JOIN identities i ON i.slug = l.address AND i.fingerprint = l.owner_fp
          WHERE l.address = $2 AND l.on_",
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
