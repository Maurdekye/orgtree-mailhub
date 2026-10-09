//! Presence and long-poll wake-ups, per address, in memory (as in v1).
//!
//! v1 kept two process-local dicts — parked polls per slug, and the
//! monotonic time of each slug's last authenticated call — plus one global
//! change counter that every parked poll re-checked twice a second. Here each
//! address has its own slot in a lock-free map: a parked count, the last-seen
//! instant, and a `Notify` that wakes only that address's parked polls when
//! mail or a receipt arrives for it. v2's syncs (G1) park the same way, with
//! a second `Notify` per address for what only its own devices care about,
//! and one more for roster changes.
//!
//! v2.0.2: a parked poll or sync that ends because its client hung up (the
//! process stopped or was killed, so the connection closed) leaves the
//! address online only for `HANG_UP_GRACE`, not v1's whole window, and
//! `presence_loop` wakes parked syncs when the set online changes.

use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::Notify;

/// seconds since the last authenticated call during which a client counts as
/// online (v1 PRESENCE_WINDOW)
pub const PRESENCE_WINDOW: Duration = Duration::from_secs(90);

/// how long an address stays online after a parked call ended because its
/// client hung up, with no call since: long enough for a poller to restart
/// or retry, short enough that a stopped client shows as gone (v2.0.2)
pub const HANG_UP_GRACE: Duration = Duration::from_secs(10);

#[derive(Default)]
pub struct Slot {
    parked: AtomicI64,
    /// milliseconds since `Presence::epoch`, plus one; 0 = never seen
    seen: AtomicU64,
    /// the same clock: when its last parked call ended with the client gone
    /// (and none was left parked); 0 = never
    hung_up: AtomicU64,
    /// the address's parked polls and syncs
    notify: Notify,
    /// its parked syncs only: what only its own devices care about (a
    /// message one of them sent, a receipt one of them wrote)
    sync: Notify,
}

pub struct Presence {
    slots: papaya::HashMap<String, Arc<Slot>>,
    epoch: Instant,
    /// every parked sync: the roster changed (a join, an edit, a leave)
    roster: Notify,
    /// every parked sync: the set of addresses online changed (v2.0.2)
    online: Notify,
    /// every waiting link take: a payload was left
    link: Notify,
}

impl Default for Presence {
    fn default() -> Self {
        Presence { slots: papaya::HashMap::new(), epoch: Instant::now(), roster: Notify::new(), online: Notify::new(), link: Notify::new() }
    }
}

/// Holds an address's poll as parked; dropping it un-parks. `finish` it when
/// the call answers; dropped unfinished, the client hung up (the server
/// dropped the call with its connection), and the address is held online
/// only for `HANG_UP_GRACE` from then on.
pub struct Parked {
    slots: Vec<Arc<Slot>>,
    epoch: Instant,
    finished: bool,
}

impl Parked {
    /// The call answered (or refused): an ordinary end, not a hang-up.
    pub fn finish(mut self) {
        self.finished = true;
    }
}

impl Drop for Parked {
    fn drop(&mut self) {
        let now = self.epoch.elapsed().as_millis() as u64 + 1;
        for s in &self.slots {
            // never below zero, as v1's max(0, n - 1)
            let before = s.parked.fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| Some((n - 1).max(0))).unwrap_or(0);
            if !self.finished && before <= 1 {
                s.hung_up.store(now, Ordering::Release);
            }
        }
    }
}

impl Presence {
    fn slot(&self, slug: &str) -> Arc<Slot> {
        let map = self.slots.pin();
        if let Some(s) = map.get(slug) {
            return s.clone();
        }
        map.get_or_insert_with(slug.to_string(), || Arc::new(Slot::default())).clone()
    }

    fn now_ms(&self) -> u64 {
        self.epoch.elapsed().as_millis() as u64 + 1
    }

    pub fn mark_seen(&self, slugs: &[String]) {
        let now = self.now_ms();
        for s in slugs {
            self.slot(s).seen.store(now, Ordering::Release);
        }
    }

    pub fn online(&self, slug: &str) -> bool {
        let map = self.slots.pin();
        map.get(slug).is_some_and(|s| s.online_at(self.now_ms()))
    }

    /// Park a poll for these addresses (one count per pair presented, as v1).
    pub fn park(&self, slugs: &[String]) -> Parked {
        let slots: Vec<Arc<Slot>> = slugs.iter().map(|s| self.slot(s)).collect();
        for s in &slots {
            s.parked.fetch_add(1, Ordering::AcqRel);
        }
        Parked { slots, epoch: self.epoch, finished: false }
    }

    /// The wake-up handles for these addresses: call `listen` on each BEFORE
    /// checking the database, so a write landing in between is not missed.
    pub fn slots_for(&self, slugs: &[String]) -> Vec<Arc<Slot>> {
        let mut seen = std::collections::HashSet::new();
        slugs.iter().filter(|s| seen.insert(s.as_str())).map(|s| self.slot(s)).collect()
    }

    /// Mail or a receipt changed for these addresses: wake their parked
    /// polls and syncs.
    pub fn wake<'a>(&self, slugs: impl IntoIterator<Item = &'a str>) {
        let map = self.slots.pin();
        for s in slugs {
            if let Some(slot) = map.get(s) {
                slot.notify.notify_waiters();
                slot.sync.notify_waiters();
            }
        }
    }

    /// Something only these addresses' own devices see changed (a message
    /// they sent, a receipt they wrote): wake their parked syncs, not their
    /// polls, which carry neither.
    pub fn wake_sync<'a>(&self, slugs: impl IntoIterator<Item = &'a str>) {
        let map = self.slots.pin();
        for s in slugs {
            if let Some(slot) = map.get(s) {
                slot.sync.notify_waiters();
            }
        }
    }

    /// The roster changed: wake every parked sync.
    pub fn roster_changed(&self) {
        self.roster.notify_waiters();
    }

    /// The set online changed: wake every parked sync (only a device in use
    /// answers for it).
    pub fn online_changed(&self) {
        self.online.notify_waiters();
    }

    /// A link payload was left: every waiting take checks its own code.
    pub fn link_left(&self) {
        self.link.notify_waiters();
    }

    /// A link take's wake-up, enabled at once: call it BEFORE the check.
    pub fn link_waiter(&self) -> std::pin::Pin<Box<tokio::sync::futures::Notified<'_>>> {
        let mut w = Box::pin(self.link.notified());
        w.as_mut().enable();
        w
    }

    /// Wake every parked poll and sync (shutdown).
    pub fn wake_all(&self) {
        for (_, slot) in self.slots.pin().iter() {
            slot.notify.notify_waiters();
            slot.sync.notify_waiters();
        }
        self.roster.notify_waiters();
        self.online.notify_waiters();
        self.link.notify_waiters();
    }

    /// An address left (unregister or roster prune): forget its presence.
    pub fn forget(&self, slug: &str) {
        let map = self.slots.pin();
        if let Some(slot) = map.remove(slug) {
            slot.notify.notify_waiters();
            slot.sync.notify_waiters();
        }
    }

    /// Every address online now, sorted.
    pub fn online_now(&self) -> Vec<String> {
        let now = self.now_ms();
        let mut on: Vec<String> = self.slots.pin().iter().filter(|(_, s)| s.online_at(now)).map(|(k, _)| k.clone()).collect();
        on.sort_unstable();
        on
    }

    /// A sync's wake-ups: its address's syncs, the roster and the set
    /// online. Enabled at once, so call it BEFORE checking the database.
    pub fn sync_listener<'a>(&'a self, slot: &'a Slot) -> SyncListener<'a> {
        let mut mine = Box::pin(slot.sync.notified());
        mine.as_mut().enable();
        let mut roster = Box::pin(self.roster.notified());
        roster.as_mut().enable();
        SyncListener { mine, roster, online: enabled(&self.online), online_notify: &self.online }
    }

    /// The wake-up handle of one address.
    pub fn slot_of(&self, slug: &str) -> Arc<Slot> {
        self.slot(slug)
    }
}

impl Slot {
    pub fn notified(&self) -> tokio::sync::futures::Notified<'_> {
        self.notify.notified()
    }

    /// A parked poll or sync, or an authenticated call within the window —
    /// except that a client that hung up on a parked call, with no call
    /// since, counts only for the grace after it hung up.
    fn online_at(&self, now_ms: u64) -> bool {
        if self.parked.load(Ordering::Acquire) > 0 {
            return true;
        }
        let seen = self.seen.load(Ordering::Acquire);
        let hung_up = self.hung_up.load(Ordering::Acquire);
        if hung_up != 0 && hung_up >= seen {
            return now_ms.saturating_sub(hung_up) < HANG_UP_GRACE.as_millis() as u64;
        }
        seen != 0 && now_ms.saturating_sub(seen) < PRESENCE_WINDOW.as_millis() as u64
    }
}

fn enabled(n: &Notify) -> std::pin::Pin<Box<tokio::sync::futures::Notified<'_>>> {
    let mut w = Box::pin(n.notified());
    w.as_mut().enable();
    w
}

pub struct SyncListener<'a> {
    mine: std::pin::Pin<Box<tokio::sync::futures::Notified<'a>>>,
    roster: std::pin::Pin<Box<tokio::sync::futures::Notified<'a>>>,
    online: std::pin::Pin<Box<tokio::sync::futures::Notified<'a>>>,
    online_notify: &'a Notify,
}

/// What woke a parked sync.
#[derive(Debug, PartialEq, Eq)]
pub enum Woke {
    /// its mail, its devices' doings or the roster: check the database
    Changed,
    /// only the set online: no database check needed
    Online,
}

impl SyncListener<'_> {
    /// Wait for a wake-up. A presence wake re-arms itself before it
    /// returns, so the caller may check and wait again without missing the
    /// next one; after `Changed`, make a new listener.
    pub async fn wait(&mut self) -> Woke {
        tokio::select! {
            _ = &mut self.mine => Woke::Changed,
            _ = &mut self.roster => Woke::Changed,
            _ = &mut self.online => {
                self.online = enabled(self.online_notify);
                Woke::Online
            }
        }
    }
}

/// Wait until any of `slots` is woken. Each `Notified` is enabled before the
/// caller's database check, so a wake between the check and this wait
/// still counts.
pub struct Listener<'a> {
    futs: Vec<std::pin::Pin<Box<tokio::sync::futures::Notified<'a>>>>,
}

impl<'a> Listener<'a> {
    pub fn new(slots: &'a [Arc<Slot>]) -> Listener<'a> {
        let mut futs: Vec<_> = slots.iter().map(|s| Box::pin(s.notified())).collect();
        for f in futs.iter_mut() {
            f.as_mut().enable();
        }
        Listener { futs }
    }

    pub async fn wait(self) {
        if self.futs.is_empty() {
            return std::future::pending().await;
        }
        futures::future::select_all(self.futs).await;
    }
}
