//! Typing (feature `typing`; Orgtree docket
//! show-agent-typing-in-hubchat-using-ordered-messa): heartbeats. A sender
//! (Orgtree's engine while an agent writes mail to an address, Hubchat while
//! its user writes a message) sends one about every half second while it
//! writes, and the recipient's Hubchat shows "… is typing" while one arrived
//! within the last second by its own clock (user 2026-10-13 22:34Z). There is
//! no start and no stop: when the heartbeats stop, the indicator ends by
//! itself, so a sender that dies can never leave one standing.
//!
//! The hub only relays. A heartbeat wakes the recipient's parked syncs, and
//! one of a device in use that asked for typing answers with it at once.
//! Each heartbeat carries a number, so a sync is told each one once: its
//! answer gives the mark it sends back. A heartbeat is relayed while it is
//! `FRESH` and kept `KEEP` at most, for a device between two syncs. In memory
//! only: never stored, never mail, gone on restart.

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// how often a sender beats while it writes (the senders' side)
pub const BEAT: Duration = Duration::from_millis(500);
/// a heartbeat older than this is no longer relayed
pub const FRESH: Duration = Duration::from_secs(1);
/// how long a heartbeat is kept at all
pub const KEEP: Duration = Duration::from_secs(2);
/// a sender's heartbeats to one recipient closer together than this are
/// absorbed: about three a second count at most
pub const MIN_GAP: Duration = Duration::from_millis(300);
/// recipients per heartbeat (as many as a message may have)
pub const MAX_RECIPIENTS: usize = 10;
/// recipients one sender may beat to within `KEEP`
pub const MAX_LIVE_PER_SENDER: usize = 32;
/// the largest request body `POST /api/typing` reads
pub const MAX_BODY: usize = 4096;

struct Beat {
    from: String,
    at: Instant,
    seq: u64,
}

/// The heartbeats of the last `KEEP`, by recipient.
pub struct Typing {
    inner: Mutex<Inner>,
}

struct Inner {
    by_to: HashMap<String, Vec<Beat>>,
    /// the last number given. It starts at the hub's start time in
    /// microseconds, so a mark from an earlier run is below every new one.
    seq: u64,
}

impl Default for Typing {
    fn default() -> Self {
        let start = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_micros() as u64).unwrap_or(0);
        Typing { inner: Mutex::new(Inner { by_to: HashMap::new(), seq: start }) }
    }
}

/// A sender already beats to `MAX_LIVE_PER_SENDER` recipients.
#[derive(Debug, PartialEq, Eq)]
pub struct TooMany;

/// Whether `v` holds a heartbeat from `from` still kept at `now`.
fn holds(v: &[Beat], from: &str, now: Instant) -> bool {
    v.iter().any(|b| b.from == from && now.saturating_duration_since(b.at) < KEEP)
}

impl Typing {
    fn inner(&self) -> MutexGuard<'_, Inner> {
        // a panic elsewhere never leaves the map half-written: carry on
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// A heartbeat from `from` to each of `to` at `now`. The recipients it
    /// counted for: never the sender itself, nor one that had a heartbeat
    /// from it less than `MIN_GAP` ago (absorbed).
    pub fn beat(&self, from: &str, to: &[String], now: Instant) -> Result<Vec<String>, TooMany> {
        let mut g = self.inner();
        let inner = &mut *g;
        let live = inner.by_to.values().filter(|v| holds(v, from, now)).count();
        let new = to.iter().filter(|t| t.as_str() != from && !inner.by_to.get(*t).is_some_and(|v| holds(v, from, now))).count();
        if live + new > MAX_LIVE_PER_SENDER {
            return Err(TooMany);
        }
        let mut counted = Vec::new();
        for t in to {
            if t == from {
                continue;
            }
            let v = inner.by_to.entry(t.clone()).or_default();
            match v.iter().position(|b| b.from == from) {
                Some(i) if now.saturating_duration_since(v[i].at) < MIN_GAP => continue,
                Some(i) => {
                    inner.seq += 1;
                    v[i].at = now;
                    v[i].seq = inner.seq;
                }
                None => {
                    inner.seq += 1;
                    v.push(Beat { from: from.to_string(), at: now, seq: inner.seq });
                }
            }
            counted.push(t.clone());
        }
        Ok(counted)
    }

    /// A fresh heartbeat to `to` that a sync which last saw `seen` was not
    /// told yet?
    pub fn news(&self, to: &str, seen: u64, now: Instant) -> bool {
        let g = self.inner();
        g.by_to.get(to).is_some_and(|v| v.iter().any(|b| b.seq > seen && now.saturating_duration_since(b.at) < FRESH))
    }

    /// What an answer to a sync that last saw `seen` relays to `to`: the
    /// senders of the fresh heartbeats it was not told yet, sorted, and the
    /// mark it sends back next time.
    pub fn relay(&self, to: &str, seen: u64, now: Instant) -> (Vec<String>, u64) {
        let g = self.inner();
        let mut from: Vec<String> = g
            .by_to
            .get(to)
            .into_iter()
            .flatten()
            .filter(|b| b.seq > seen && now.saturating_duration_since(b.at) < FRESH)
            .map(|b| b.from.clone())
            .collect();
        from.sort_unstable();
        (from, g.seq)
    }

    /// A mark a sync sent back, as this run reads it: one beyond every
    /// number given so far (a clock set back since an earlier run) counts as
    /// none.
    pub fn seen(&self, mark: u64) -> u64 {
        if mark > self.inner().seq {
            0
        } else {
            mark
        }
    }

    /// Drop the heartbeats older than `KEEP`.
    pub fn sweep(&self, now: Instant) {
        let mut g = self.inner();
        g.by_to.retain(|_, v| {
            v.retain(|b| now.saturating_duration_since(b.at) < KEEP);
            !v.is_empty()
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn to(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| x.to_string()).collect()
    }

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn a_heartbeat_is_relayed_once_while_fresh() {
        let t = Typing::default();
        let now = Instant::now();
        assert!(!t.news("ann", 0, now));
        assert_eq!(t.beat("org", &to(&["ann", "bob"]), now), Ok(to(&["ann", "bob"])));
        assert!(t.news("ann", 0, now));
        let (from, mark) = t.relay("ann", 0, now + ms(10));
        assert_eq!(from, ["org"]);
        // told once: the mark it sends back sees nothing new
        assert!(!t.news("ann", mark, now + ms(10)));
        assert_eq!(t.relay("ann", mark, now + ms(10)), (vec![], mark));
        // bob was not told yet; a sync asking for the first time is
        assert!(t.news("bob", 0, now + ms(900)));
        // once it is no longer fresh, nobody is told of it
        assert!(!t.news("bob", 0, now + FRESH));
        assert_eq!(t.relay("bob", 0, now + FRESH).0, Vec::<String>::new());
        // the next heartbeat is news again
        t.beat("org", &to(&["ann"]), now + ms(500)).unwrap();
        assert!(t.news("ann", mark, now + ms(500)));
        assert_eq!(t.relay("ann", mark, now + ms(510)).0, ["org"]);
    }

    #[test]
    fn a_sender_counts_at_most_every_min_gap() {
        let t = Typing::default();
        let now = Instant::now();
        t.beat("org", &to(&["ann"]), now).unwrap();
        let (_, mark) = t.relay("ann", 0, now);
        // absorbed: nothing new to tell, and its time is not moved
        assert_eq!(t.beat("org", &to(&["ann"]), now + ms(100)), Ok(vec![]));
        assert_eq!(t.beat("org", &to(&["ann"]), now + ms(299)), Ok(vec![]));
        assert!(!t.news("ann", mark, now + ms(299)));
        assert_eq!(t.beat("org", &to(&["ann"]), now + ms(300)), Ok(to(&["ann"])));
        assert!(t.news("ann", mark, now + ms(300)));
        // another recipient of the same heartbeat is not held back by it
        assert_eq!(t.beat("org", &to(&["ann", "bob"]), now + ms(350)), Ok(to(&["bob"])));
    }

    #[test]
    fn several_senders_are_relayed_together() {
        let t = Typing::default();
        let now = Instant::now();
        t.beat("org", &to(&["ann"]), now).unwrap();
        t.beat("cat", &to(&["ann"]), now + ms(20)).unwrap();
        assert_eq!(t.relay("ann", 0, now + ms(30)).0, ["cat", "org"]);
        let (_, mark) = t.relay("ann", 0, now + ms(30));
        t.beat("org", &to(&["ann"]), now + ms(500)).unwrap();
        // only the one with a new heartbeat
        assert_eq!(t.relay("ann", mark, now + ms(510)).0, ["org"]);
    }

    #[test]
    fn heartbeats_are_kept_only_briefly() {
        let t = Typing::default();
        let now = Instant::now();
        t.beat("org", &to(&["ann", "bob"]), now).unwrap();
        t.beat("cat", &to(&["ann"]), now + ms(1500)).unwrap();
        t.sweep(now + KEEP - ms(1));
        assert_eq!(t.inner().by_to.values().flatten().count(), 3);
        t.sweep(now + KEEP);
        assert_eq!(t.inner().by_to.keys().map(String::as_str).collect::<Vec<_>>(), ["ann"]);
        t.sweep(now + ms(1500) + KEEP);
        assert!(t.inner().by_to.is_empty(), "nothing is kept once it ended");
    }

    #[test]
    fn a_sender_beats_to_a_bounded_number_of_recipients() {
        let t = Typing::default();
        let now = Instant::now();
        let thirty: Vec<String> = (0..30).map(|i| format!("r{i}")).collect();
        for c in thirty.chunks(MAX_RECIPIENTS) {
            t.beat("org", c, now).unwrap();
        }
        // 30 live: two more fit, three do not; the same ones always do
        assert_eq!(t.beat("org", &to(&["x", "y", "z"]), now), Err(TooMany));
        assert!(t.beat("org", &to(&["x", "y"]), now).is_ok());
        assert!(t.beat("org", &thirty[..10], now + ms(400)).is_ok());
        assert_eq!(t.beat("org", &to(&["z"]), now + ms(400)), Err(TooMany));
        // other senders are not limited by it, and old heartbeats free room
        assert!(t.beat("cat", &to(&["z"]), now).is_ok());
        assert_eq!(t.beat("org", &to(&["z"]), now + KEEP + ms(400)), Ok(to(&["z"])));
    }

    #[test]
    fn the_sender_itself_is_skipped() {
        let t = Typing::default();
        let now = Instant::now();
        assert_eq!(t.beat("org", &to(&["org", "ann"]), now), Ok(to(&["ann"])));
        assert!(!t.news("org", 0, now));
    }

    #[test]
    fn a_mark_from_another_run_counts_as_none() {
        let t = Typing::default();
        let now = Instant::now();
        t.beat("org", &to(&["ann"]), now).unwrap();
        let (_, mark) = t.relay("ann", 0, now);
        assert_eq!(t.seen(mark), mark);
        assert_eq!(t.seen(mark + 1), 0);
        // a run that starts later numbers above an earlier run's marks
        std::thread::sleep(ms(2));
        let later = Typing::default();
        later.beat("org", &to(&["ann"]), Instant::now()).unwrap();
        assert!(later.news("ann", later.seen(mark), Instant::now()));
    }
}
