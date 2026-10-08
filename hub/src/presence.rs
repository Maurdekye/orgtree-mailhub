//! Presence and long-poll wake-ups, per address, in memory (as in v1).
//!
//! v1 kept two process-local dicts — parked polls per slug, and the
//! monotonic time of each slug's last authenticated call — plus one global
//! change counter that every parked poll re-checked twice a second. Here each
//! address has its own slot in a lock-free map: a parked count, the last-seen
//! instant, and a `Notify` that wakes only that address's parked polls when
//! mail or a receipt arrives for it.

use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::Notify;

/// seconds since the last authenticated call during which a client counts as
/// online (v1 PRESENCE_WINDOW)
pub const PRESENCE_WINDOW: Duration = Duration::from_secs(90);

#[derive(Default)]
pub struct Slot {
    parked: AtomicI64,
    /// milliseconds since `Presence::epoch`, plus one; 0 = never seen
    seen: AtomicU64,
    notify: Notify,
}

pub struct Presence {
    slots: papaya::HashMap<String, Arc<Slot>>,
    epoch: Instant,
}

impl Default for Presence {
    fn default() -> Self {
        Presence { slots: papaya::HashMap::new(), epoch: Instant::now() }
    }
}

/// Holds an address's poll as parked; dropping it (the poll answered, or the
/// client hung up) un-parks.
pub struct Parked {
    slots: Vec<Arc<Slot>>,
}

impl Drop for Parked {
    fn drop(&mut self) {
        for s in &self.slots {
            // never below zero, as v1's max(0, n - 1)
            let _ = s.parked.fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| Some((n - 1).max(0)));
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
        let Some(s) = map.get(slug) else { return false };
        if s.parked.load(Ordering::Acquire) > 0 {
            return true;
        }
        let seen = s.seen.load(Ordering::Acquire);
        seen != 0 && self.now_ms().saturating_sub(seen) < PRESENCE_WINDOW.as_millis() as u64
    }

    /// Park a poll for these addresses (one count per pair presented, as v1).
    pub fn park(&self, slugs: &[String]) -> Parked {
        let slots: Vec<Arc<Slot>> = slugs.iter().map(|s| self.slot(s)).collect();
        for s in &slots {
            s.parked.fetch_add(1, Ordering::AcqRel);
        }
        Parked { slots }
    }

    /// The wake-up handles for these addresses: call `listen` on each BEFORE
    /// checking the database, so a write landing in between is not missed.
    pub fn slots_for(&self, slugs: &[String]) -> Vec<Arc<Slot>> {
        let mut seen = std::collections::HashSet::new();
        slugs.iter().filter(|s| seen.insert(s.as_str())).map(|s| self.slot(s)).collect()
    }

    /// Mail or a receipt changed for these addresses: wake their parked polls.
    pub fn wake<'a>(&self, slugs: impl IntoIterator<Item = &'a str>) {
        let map = self.slots.pin();
        for s in slugs {
            if let Some(slot) = map.get(s) {
                slot.notify.notify_waiters();
            }
        }
    }

    /// Wake every parked poll (shutdown).
    pub fn wake_all(&self) {
        for (_, slot) in self.slots.pin().iter() {
            slot.notify.notify_waiters();
        }
    }

    /// An address left (unregister or roster prune): forget its presence.
    pub fn forget(&self, slug: &str) {
        let map = self.slots.pin();
        if let Some(slot) = map.remove(slug) {
            slot.notify.notify_waiters();
        }
    }
}

impl Slot {
    pub fn notified(&self) -> tokio::sync::futures::Notified<'_> {
        self.notify.notified()
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
