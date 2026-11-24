//! Last active (v2.6.0, schema 15; Orgtree docket
//! hubchat-last-seen-time-for-orgs-people-and-indep, user 2026-11-24 11:44Z):
//! "last seen" per kind. Every address has `active_at`, the last time it was
//! active (null: never, or not known), and is in the sync answers'
//! `active_now`
//! set while it is active now (shown online only, as `busy` and `dnd`):
//!
//! - a person: active while one of their devices is in use (`POST
//!   /api/devices/active`); `active_at` is when the last one stopped;
//! - an org: active while it says so (`POST /api/presence {active}`, Orgtree:
//!   an agent in charge of its org mailbox is mid-turn); `active_at` is what
//!   it says (`active_at`: when those agents last finished a turn), never
//!   later than now, or now when it went offline mid-turn;
//! - an agent session (kind `chat`): never active now; `active_at` is its last
//!   message sent to anyone.
//!
//! An Invisible person is never in `active_now`, and their `active_at` is held at
//! the moment they went Invisible, as `last_seen` is. Roster rows carry
//! `active_at` and `active`; `/healthz` lists the feature `last_active`.
//!
//!     HUB_TEST_PG=<folder> cargo test --features test-clock --test last_active -- --nocapture

#![cfg(feature = "test-clock")]
mod support;

use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use mailhub::api::sync::presence_loop;
use mailhub::api::sync::ACTIVE_FOR;
use mailhub::presence::{HANG_UP_GRACE, PRESENCE_WINDOW};
use mailhub::Hub;
use serde_json::{json, Value};
use support::*;
use tokio::task::JoinHandle;

type Id = (String, String);

async fn register(hub: &Arc<Hub>, name: &str, kind: &str) -> Id {
    let id = (format!("{name}.seen.{}", token_hex3()), token_hex16());
    let r = Call::new("POST", "/api/register")
        .auth(pair(&id.0, &id.1))
        .json(json!({ "slug": id.0, "org_name": name, "username": "seen", "kind": kind }))
        .send(hub)
        .await;
    assert_eq!(r.code(), 200, "{}", r.text());
    id
}

fn park_poll(hub: &Arc<Hub>, who: &Id) -> JoinHandle<Resp> {
    let (h, w) = (hub.clone(), who.clone());
    tokio::spawn(async move { Call::new("POST", "/api/poll?wait=55").auth(pair(&w.0, &w.1)).send(&h).await })
}

async fn post(hub: &Arc<Hub>, who: &Id, path: &str, body: Value) -> Resp {
    Call::new("POST", path).auth(pair(&who.0, &who.1)).json(body).send(hub).await
}

async fn in_use(hub: &Arc<Hub>, who: &Id, device: &str, on: bool) {
    let r = post(hub, who, "/api/devices/active", json!({ "device_id": device, "active": on })).await;
    assert_eq!(r.code(), 200, "{}", r.text());
}

async fn say(hub: &Arc<Hub>, who: &Id, body: Value) -> Value {
    let r = post(hub, who, "/api/presence", body).await;
    assert_eq!(r.code(), 200, "{}", r.text());
    r.json()
}

fn strings(v: &Value) -> Vec<String> {
    v.as_array().unwrap_or_else(|| panic!("not a list: {v}")).iter().map(|s| s.as_str().unwrap().to_string()).collect()
}

fn at(v: &Value) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(v.as_str().unwrap_or_else(|| panic!("not a time: {v}"))).unwrap().with_timezone(&Utc)
}

/// A watcher's device that syncs now and then, keeping the sets it was told.
struct Watcher {
    id: Id,
    cursor: Value,
    online: Vec<String>,
    active: Vec<String>,
}

impl Watcher {
    fn new(id: &Id) -> Watcher {
        Watcher { id: id.clone(), cursor: Value::Null, online: Vec::new(), active: Vec::new() }
    }

    async fn sync(&mut self, hub: &Arc<Hub>) {
        loop {
            let mut body = json!({ "device_id": "watcher", "wait": 0 });
            if !self.cursor.is_null() {
                body["cursor"] = self.cursor.clone();
            }
            let r = post(hub, &self.id, "/api/sync", body).await;
            assert_eq!(r.code(), 200, "{}", r.text());
            let a = r.json();
            self.cursor = a["cursor"].clone();
            if let Some(on) = a.get("online") {
                self.online = strings(on);
                // `active_now` comes with every `online` from a hub with the feature
                // (`active` is the asker's own devices in use, since 2.1)
                self.active = strings(a.get("active_now").unwrap_or_else(|| panic!("an answer with online and no active_now: {a}")));
            }
            if a["more"] != json!(true) {
                return;
            }
        }
    }

    /// Sync until `who` is (or is not) in `active_now`, for up to 3 s.
    async fn sees_active(&mut self, hub: &Arc<Hub>, who: &Id, want: bool) {
        let t0 = Instant::now();
        loop {
            self.sync(hub).await;
            let now = self.active.iter().any(|s| s == &who.0);
            if now == want {
                if want {
                    assert!(self.online.contains(&who.0), "{} is active while offline", who.0);
                }
                return;
            }
            assert!(t0.elapsed() < Duration::from_secs(3), "{} active {now}, not {want} (active {:?})", who.0, self.active);
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    /// Sync until `who` is out of `online`, for up to 3 s.
    async fn sees_offline(&mut self, hub: &Arc<Hub>, who: &Id) {
        let t0 = Instant::now();
        loop {
            self.sync(hub).await;
            if !self.online.contains(&who.0) {
                return;
            }
            assert!(t0.elapsed() < Duration::from_secs(3), "{} is still online", who.0);
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }
}

async fn roster_row(hub: &Arc<Hub>, viewer: &Id, slug: &str) -> Value {
    let r = Call::new("GET", "/api/roster").auth(pair(&viewer.0, &viewer.1)).send(hub).await.json();
    r["roster"].as_array().unwrap().iter().find(|e| e["slug"] == json!(slug)).cloned().unwrap_or_else(|| panic!("no {slug} in {r}"))
}

/// The row once its `active_at` is no longer `was` (the presence loop writes
/// it within a second of what a sync already shows), for up to 3 s.
async fn row_moved(hub: &Arc<Hub>, viewer: &Id, slug: &str, was: &Value) -> Value {
    let t0 = Instant::now();
    loop {
        let row = roster_row(hub, viewer, slug).await;
        if &row["active_at"] != was || t0.elapsed() > Duration::from_secs(3) {
            return row;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Sync `device` until it is caught up; its cursor.
async fn caught_up(hub: &Arc<Hub>, who: &Id, device: &str) -> Value {
    let mut cursor = Value::Null;
    loop {
        let mut body = json!({ "device_id": device, "wait": 0 });
        if !cursor.is_null() {
            body["cursor"] = cursor.clone();
        }
        let a = post(hub, who, "/api/sync", body).await.json();
        cursor = a["cursor"].clone();
        if a["more"] != json!(true) {
            return cursor;
        }
    }
}

fn park_sync(hub: &Arc<Hub>, who: &Id, device: &str, cursor: &Value, wait: u64) -> JoinHandle<Value> {
    let (h, w, body) = (hub.clone(), who.clone(), json!({ "device_id": device, "cursor": cursor, "wait": wait }));
    tokio::spawn(async move { Call::new("POST", "/api/sync").auth(pair(&w.0, &w.1)).json(body).send(&h).await.json() })
}

fn data_dir(name: &str) -> std::path::PathBuf {
    let data = std::env::temp_dir().join(format!("orgtree-hub-{name}-{}", token_hex16()));
    std::fs::create_dir_all(&data).unwrap();
    data
}

#[tokio::test]
async fn a_person_is_active_while_a_device_is_in_use() {
    let pg = TestPg::start();
    let url = pg.fresh_db("hubseenperson").await;
    let data = data_dir("seen-person");
    let hub = hub(&url, &data, &[]).await;
    tokio::spawn(presence_loop(hub.clone()));

    let h = Call::new("GET", "/healthz").send(&hub).await.json();
    assert!(h["features"].as_array().unwrap().contains(&json!("last_active")), "{h}");
    println!("  ok  /healthz lists the feature last_active");

    let pat = register(&hub, "pat", "person").await;
    let wes = register(&hub, "wes", "person").await;
    let mut w = Watcher::new(&wes);
    let _pat_poll = park_poll(&hub, &pat);

    w.sees_active(&hub, &pat, false).await;
    let row = roster_row(&hub, &wes, &pat.0).await;
    assert_eq!((row["active"].clone(), row["active_at"].clone()), (json!(false), Value::Null), "{row}");
    println!("  ok  online with no device ever in use: not active, active_at null");

    in_use(&hub, &pat, "pat-phone", true).await;
    w.sees_active(&hub, &pat, true).await;
    assert_eq!(roster_row(&hub, &wes, &pat.0).await["active"], json!(true));
    println!("  ok  a device in use: in the sync answers' active_now set, roster row active true");

    let before = Utc::now() - chrono::Duration::seconds(1);
    in_use(&hub, &pat, "pat-phone", false).await;
    w.sees_active(&hub, &pat, false).await;
    let after = Utc::now() + chrono::Duration::seconds(1);
    let row = roster_row(&hub, &wes, &pat.0).await;
    assert_eq!(row["active"], json!(false), "{row}");
    let t = at(&row["active_at"]);
    assert!(before <= t && t <= after, "active_at {t} not when the device was put down ({before}..{after})");
    println!("  ok  put down: out of active, active_at = when it stopped (the roster row is sent again)");

    // Invisible: never active; active_at written by the presence loop's next
    // look (as for going offline), then held
    in_use(&hub, &pat, "pat-phone", true).await;
    w.sees_active(&hub, &pat, true).await;
    let in_use_at = roster_row(&hub, &wes, &pat.0).await["active_at"].clone();
    let r = post(&hub, &pat, "/api/status", json!({ "status": "invisible" })).await;
    assert_eq!(r.code(), 200, "{}", r.text());
    w.sees_active(&hub, &pat, false).await;
    let row = row_moved(&hub, &wes, &pat.0, &in_use_at).await;
    let held = row["active_at"].clone();
    assert_ne!(held, row["last_seen"], "no exact tie with the held last_seen (a custom client could tell Invisible by it): {row}");
    tokio::time::sleep(Duration::from_millis(1100)).await;
    in_use(&hub, &pat, "pat-phone", false).await;
    in_use(&hub, &pat, "pat-phone", true).await;
    in_use(&hub, &pat, "pat-phone", false).await;
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let row = roster_row(&hub, &wes, &pat.0).await;
    assert_eq!((row["active"].clone(), row["active_at"].clone()), (json!(false), held.clone()), "{row}");
    assert!(!held.is_null(), "an Invisible person who was active shows when they went Invisible");
    println!("  ok  Invisible: never active, active_at held while they stay Invisible");

    let r = post(&hub, &pat, "/api/status", json!({ "status": "auto" })).await;
    assert_eq!(r.code(), 200, "{}", r.text());
    in_use(&hub, &pat, "pat-phone", true).await;
    w.sees_active(&hub, &pat, true).await;
    in_use(&hub, &pat, "pat-phone", false).await;
    w.sees_active(&hub, &pat, false).await;
    let row = roster_row(&hub, &wes, &pat.0).await;
    assert!(at(&row["active_at"]) > at(&held), "back on Automatic: their use moves active_at again ({row})");
    println!("  ok  back on Automatic: active and active_at move with their use again");

    // a watcher's own use is never in its own answers' active_now
    in_use(&hub, &wes, "watcher", true).await;
    w.sync(&hub).await;
    assert!(!w.active.contains(&wes.0), "{:?}", w.active);
    println!("  ok  an address's own use is not in its own active_now");
}

#[tokio::test]
async fn an_org_is_active_while_it_says_so_and_says_when_it_last_was() {
    let pg = TestPg::start();
    let url = pg.fresh_db("hubseenorg").await;
    let data = data_dir("seen-org");
    let hub = hub(&url, &data, &[]).await;
    tokio::spawn(presence_loop(hub.clone()));

    let acme = register(&hub, "acme", "org").await;
    let wes = register(&hub, "wes", "person").await;
    let mut w = Watcher::new(&wes);
    let acme_poll = park_poll(&hub, &acme);

    // an engine from before last active: the same answer as ever, nothing stored
    let a = say(&hub, &acme, json!({ "busy": false, "dnd": false })).await;
    assert_eq!(a, json!({ "slug": acme.0, "busy": false, "dnd": false }));
    w.sees_active(&hub, &acme, false).await;
    assert_eq!(roster_row(&hub, &wes, &acme.0).await["active_at"], Value::Null);
    println!("  ok  an org that never says active: not active, active_at null, the answer unchanged");

    let a = say(&hub, &acme, json!({ "busy": false, "dnd": false, "active": true, "active_at": "2026-11-24T10:00:00.000Z" })).await;
    assert_eq!((a["active"].clone(), a["active_at"].clone()), (json!(true), json!("2026-11-24T10:00:00.000Z")), "{a}");
    w.sees_active(&hub, &acme, true).await;
    assert_eq!(roster_row(&hub, &wes, &acme.0).await["active"], json!(true));
    println!("  ok  mid-turn: in active, roster row active true");

    say(&hub, &acme, json!({ "busy": false, "active": false, "active_at": "2026-11-24T11:00:00.000Z" })).await;
    w.sees_active(&hub, &acme, false).await;
    let row = roster_row(&hub, &wes, &acme.0).await;
    assert_eq!((row["active"].clone(), row["active_at"].clone()), (json!(false), json!("2026-11-24T11:00:00.000Z")), "{row}");
    println!("  ok  the turn ended: out of active, active_at exactly as the org said");

    // an org's time never goes back (@coordinator-opus 2026-11-24 13:47Z): an
    // older time or null changes nothing; the answer says what the hub read
    let a = say(&hub, &acme, json!({ "busy": false, "active": false, "active_at": "2026-11-24T09:00:00.000Z" })).await;
    assert_eq!(a["active_at"], json!("2026-11-24T09:00:00.000Z"), "{a}");
    assert_eq!(roster_row(&hub, &wes, &acme.0).await["active_at"], json!("2026-11-24T11:00:00.000Z"));
    say(&hub, &acme, json!({ "busy": false, "active": false, "active_at": null })).await;
    assert_eq!(roster_row(&hub, &wes, &acme.0).await["active_at"], json!("2026-11-24T11:00:00.000Z"));
    println!("  ok  an org's active_at never moves back, and null leaves it");

    // leaving `active_at` out keeps the stored one
    say(&hub, &acme, json!({ "busy": false, "active": false, "active_at": "2026-11-24T11:00:00.000Z" })).await;
    say(&hub, &acme, json!({ "busy": true, "active": false })).await;
    assert_eq!(roster_row(&hub, &wes, &acme.0).await["active_at"], json!("2026-11-24T11:00:00.000Z"));
    println!("  ok  active_at left out keeps the stored one");

    for (body, why) in [
        (json!({ "busy": false, "active": "yes" }), "active must be true or false"),
        (json!({ "busy": false, "active_at": "yesterday" }), "active_at must be a time (RFC 3339) or null"),
        (json!({ "busy": false, "active_at": 5 }), "active_at must be a time (RFC 3339) or null"),
    ] {
        let r = post(&hub, &acme, "/api/presence", body).await;
        assert_eq!((r.code(), r.json()["detail"].clone()), (422, json!(why)), "{}", r.text());
    }
    println!("  ok  refused: active not a boolean, active_at not a time (422)");

    // mid-turn when it goes offline: last active then
    say(&hub, &acme, json!({ "busy": false, "active": true, "active_at": "2026-11-24T11:00:00.000Z" })).await;
    w.sees_active(&hub, &acme, true).await;
    let before = Utc::now() - chrono::Duration::seconds(1);
    acme_poll.abort();
    // the hang-up lands when the aborted task is dropped: wait for it
    let _ = acme_poll.await;
    // past the grace after a hang-up, and past the window after its last
    // call (should the poll have answered before the abort)
    hub.presence.advance(HANG_UP_GRACE + PRESENCE_WINDOW + Duration::from_secs(1));
    w.sees_offline(&hub, &acme).await;
    // the presence loop writes it as it sees the org go (a sync may see the
    // set first): wait up to 3 s for the row
    let t0 = Instant::now();
    let mut row = roster_row(&hub, &wes, &acme.0).await;
    while row["active_at"] == json!("2026-11-24T11:00:00.000Z") && t0.elapsed() < Duration::from_secs(3) {
        tokio::time::sleep(Duration::from_millis(100)).await;
        row = roster_row(&hub, &wes, &acme.0).await;
    }
    let after = Utc::now() + chrono::Duration::seconds(1);
    let t = at(&row["active_at"]);
    assert_eq!(row["active"], json!(false), "{row}");
    assert!(before <= t && t <= after, "gone offline mid-turn: active_at {t} is when it went ({before}..{after})");
    println!("  ok  offline mid-turn: out of active, active_at = when it went offline");

    // back online, active ended with going offline: it says it again
    let _again = park_poll(&hub, &acme);
    w.sees_active(&hub, &acme, false).await;
    println!("  ok  back online: not active until it says so again");

    // it says its last real turn end, older than when the hub saw it go: the
    // hub keeps the later (R3, @coordinator-opus 2026-11-24 13:47Z)
    say(&hub, &acme, json!({ "busy": false, "active": false, "active_at": "2026-11-24T11:00:00.000Z" })).await;
    assert_eq!(at(&roster_row(&hub, &wes, &acme.0).await["active_at"]), t, "the time the hub noted stays");
    println!("  ok  back online saying an older end: the time the hub noted stays");

    // never past now
    let a = say(&hub, &acme, json!({ "busy": false, "active": false, "active_at": "2999-01-01T00:00:00.000Z" })).await;
    assert!(at(&a["active_at"]) <= Utc::now(), "a time in the future is taken as now: {a}");
    assert!(at(&roster_row(&hub, &wes, &acme.0).await["active_at"]) <= Utc::now());
    println!("  ok  a time in the future is taken as now");
}

#[tokio::test]
async fn an_agent_session_was_last_active_when_it_last_sent() {
    let pg = TestPg::start();
    let url = pg.fresh_db("hubseenchat").await;
    let data = data_dir("seen-chat");
    let hub = hub(&url, &data, &[]).await;
    tokio::spawn(presence_loop(hub.clone()));

    let bot = register(&hub, "bot", "chat").await;
    let wes = register(&hub, "wes", "person").await;
    let other = register(&hub, "other", "person").await;
    let mut w = Watcher::new(&wes);
    let _bot_poll = park_poll(&hub, &bot);

    w.sees_active(&hub, &bot, false).await;
    assert_eq!(roster_row(&hub, &wes, &bot.0).await["active_at"], Value::Null);
    println!("  ok  a session that never sent: active_at null");

    // to someone else: wes still learns when it was last active
    let r = post(&hub, &bot, "/api/send", json!({ "to": other.0, "body": "hello" })).await;
    assert_eq!(r.code(), 200, "{}", r.text());
    let sent = at(&r.json()["received_at"]);
    w.sync(&hub).await;
    let row = roster_row(&hub, &wes, &bot.0).await;
    assert_eq!(at(&row["active_at"]), sent, "{row}");
    assert_eq!(row["active"], json!(false));
    w.sees_active(&hub, &bot, false).await;
    println!("  ok  it sent to anyone: active_at = that message's time; never in active");

    // a person's or an org's sends don't touch theirs
    let r = post(&hub, &wes, "/api/send", json!({ "to": other.0, "body": "hi" })).await;
    assert_eq!(r.code(), 200, "{}", r.text());
    assert_eq!(roster_row(&hub, &other, &wes.0).await["active_at"], Value::Null);
    println!("  ok  a person's send leaves their active_at alone (it is their devices' use)");
}

#[tokio::test]
async fn a_sync_carries_changed_active_at_rows() {
    let pg = TestPg::start();
    let url = pg.fresh_db("hubseensync").await;
    let data = data_dir("seen-sync");
    let hub = hub(&url, &data, &[]).await;
    tokio::spawn(presence_loop(hub.clone()));

    let acme = register(&hub, "acme", "org").await;
    let wes = register(&hub, "wes", "person").await;
    let _poll = park_poll(&hub, &acme);
    // a device caught up with the roster
    let mut cursor = Value::Null;
    for _ in 0..10 {
        let mut body = json!({ "device_id": "wes-desk", "wait": 0 });
        if !cursor.is_null() { body["cursor"] = cursor.clone(); }
        let a = post(&hub, &wes, "/api/sync", body).await.json();
        cursor = a["cursor"].clone();
        if a["more"] != json!(true) { break; }
    }
    say(&hub, &acme, json!({ "busy": false, "active": false, "active_at": "2026-11-24T11:00:00.000Z" })).await;
    let a = post(&hub, &wes, "/api/sync", json!({ "device_id": "wes-desk", "wait": 0, "cursor": cursor })).await.json();
    let row = a["roster"].as_array().unwrap().iter().find(|e| e["slug"] == json!(acme.0)).cloned();
    assert_eq!(row.map(|r| r["active_at"].clone()), Some(json!("2026-11-24T11:00:00.000Z")), "{a}");
    println!("  ok  a new active_at sends the org's roster row again to synced devices");
}

#[tokio::test]
async fn a_lease_that_runs_out_ends_the_visit() {
    let pg = TestPg::start();
    let url = pg.fresh_db("hubseenlapse").await;
    let data = data_dir("seen-lapse");
    let hub = hub(&url, &data, &[]).await;
    tokio::spawn(presence_loop(hub.clone()));
    let pat = register(&hub, "pat", "person").await;
    let wes = register(&hub, "wes", "person").await;
    let mut w = Watcher::new(&wes);
    let _pat_poll = park_poll(&hub, &pat);

    // Hubchat crashed in use: no `false` ever comes
    in_use(&hub, &pat, "pat-phone", true).await;
    w.sees_active(&hub, &pat, true).await;
    // the presence loop looks once a second (a real lease lasts 90 s)
    tokio::time::sleep(Duration::from_millis(1200)).await;
    let sql = sql_client(&url).await;
    sql.execute("UPDATE identities SET active_at = now() - interval '1 hour' WHERE slug = $1", &[&pat.0]).await.unwrap();
    let was = roster_row(&hub, &wes, &pat.0).await["active_at"].clone();
    let before = Utc::now() - chrono::Duration::seconds(1);
    hub.presence.advance(ACTIVE_FOR + Duration::from_secs(1));
    w.sees_active(&hub, &pat, false).await;
    let row = row_moved(&hub, &wes, &pat.0, &was).await;
    let after = Utc::now() + chrono::Duration::seconds(1);
    let t = at(&row["active_at"]);
    assert!(before <= t && t <= after, "the lease ran out: active_at {t} is when the hub saw it end ({before}..{after})");
    println!("  ok  a lease that runs out (no false): out of active_now, active_at = when the hub saw it end");

    // a `false` from a device that was never in use is no visit
    let r = post(&hub, &wes, "/api/devices/active", json!({ "device_id": "wes-tablet", "active": false })).await;
    assert_eq!(r.code(), 200, "{}", r.text());
    assert_eq!(roster_row(&hub, &pat, &wes.0).await["active_at"], Value::Null);
    println!("  ok  a put-down from a device that was not in use leaves active_at alone");
}

#[tokio::test]
async fn only_devices_in_use_hear_a_moved_time() {
    let pg = TestPg::start();
    let url = pg.fresh_db("hubseensoft").await;
    let data = data_dir("seen-soft");
    let hub = hub(&url, &data, &[]).await;
    tokio::spawn(presence_loop(hub.clone()));
    let bot = register(&hub, "bot", "chat").await;
    let other = register(&hub, "other", "person").await;
    let wes = register(&hub, "wes", "person").await;
    let desk = caught_up(&hub, &wes, "wes-desk").await;
    let phone = caught_up(&hub, &wes, "wes-phone").await;
    in_use(&hub, &wes, "wes-desk", true).await;
    let desk_sync = park_sync(&hub, &wes, "wes-desk", &desk, 8);
    let phone_sync = park_sync(&hub, &wes, "wes-phone", &phone, 8);
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(!desk_sync.is_finished() && !phone_sync.is_finished(), "both parked");

    let r = post(&hub, &bot, "/api/send", json!({ "to": other.0, "body": "hello" })).await;
    assert_eq!(r.code(), 200, "{}", r.text());
    let a = tokio::time::timeout(Duration::from_secs(3), desk_sync).await.expect("the desk (in use) answers at once").unwrap();
    let row = a["roster"].as_array().unwrap().iter().find(|e| e["slug"] == json!(bot.0)).cloned();
    assert!(row.is_some_and(|r| !r["active_at"].is_null()), "the desk is sent the session's new time: {a}");
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(!phone_sync.is_finished(), "a phone in a pocket is not woken by a session's send to someone else");
    println!("  ok  a session's send: a device in use hears its new time at once; a parked phone not in use sleeps on");
    let a = phone_sync.await.unwrap();
    assert!(a["roster"].as_array().unwrap().iter().any(|e| e["slug"] == json!(bot.0)), "and gets it with its next answer: {a}");
    println!("  ok  the phone gets it with its next answer");
}

#[tokio::test]
async fn the_times_hold_across_a_restart() {
    let pg = TestPg::start();
    let url = pg.fresh_db("hubseenrestart").await;
    let data = data_dir("seen-restart");
    let first = hub(&url, &data, &[]).await;
    let acme = register(&first, "acme", "org").await;
    let pat = register(&first, "pat", "person").await;
    let wes = register(&first, "wes", "person").await;
    say(&first, &acme, json!({ "busy": false, "active": true, "active_at": "2026-11-24T11:00:00.000Z" })).await;
    in_use(&first, &pat, "pat-phone", true).await;
    in_use(&first, &pat, "pat-phone", false).await;
    let pat_at = roster_row(&first, &wes, &pat.0).await["active_at"].clone();
    assert!(!pat_at.is_null());
    first.shutdown.cancel();
    drop(first);

    let hub = hub(&url, &data, &[]).await;
    tokio::spawn(presence_loop(hub.clone()));
    let row = roster_row(&hub, &wes, &acme.0).await;
    assert_eq!((row["active"].clone(), row["active_at"].clone()), (json!(false), json!("2026-11-24T11:00:00.000Z")), "{row}");
    assert_eq!(roster_row(&hub, &wes, &pat.0).await["active_at"], pat_at);
    println!("  ok  after a restart: times kept, an org's active ended (it says it again on reconnect)");
}

#[tokio::test]
async fn only_an_org_says_it_and_its_clock_is_corrected() {
    let pg = TestPg::start();
    let url = pg.fresh_db("hubseenkinds").await;
    let data = data_dir("seen-kinds");
    let hub = hub(&url, &data, &[]).await;
    tokio::spawn(presence_loop(hub.clone()));
    let acme = register(&hub, "acme", "org").await;
    let bot = register(&hub, "bot", "chat").await;
    let pat = register(&hub, "pat", "person").await;
    let wes = register(&hub, "wes", "person").await;
    let mut w = Watcher::new(&wes);
    let (_b, _p) = (park_poll(&hub, &bot), park_poll(&hub, &pat));
    for who in [&bot, &pat] {
        let a = say(&hub, who, json!({ "busy": false, "active": true, "active_at": "2026-11-24T11:00:00.000Z" })).await;
        assert_eq!(a, json!({ "slug": who.0, "busy": false, "dnd": false }), "not taken, not answered");
        w.sees_active(&hub, who, false).await;
        assert_eq!(roster_row(&hub, &wes, &who.0).await["active_at"], Value::Null);
    }
    println!("  ok  an agent session or a person saying active/active_at: ignored");

    // an hour ahead: moved back by the hour, not merely clamped to now (times
    // only move on here: an org's never goes back)
    let real = Utc::now();
    let iso = |t: DateTime<Utc>| t.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string();
    let a = say(&hub, &acme, json!({ "busy": false, "active": false,
        "active_at": iso(real + chrono::Duration::seconds(3500)), "now": iso(real + chrono::Duration::seconds(3600)) })).await;
    let t = at(&a["active_at"]);
    assert!((t - (real - chrono::Duration::seconds(100))).num_seconds().abs() <= 3, "ahead: moved back by the difference ({t})");
    assert_eq!(at(&roster_row(&hub, &wes, &acme.0).await["active_at"]), t);
    // the engine's clock a minute behind the hub's: its times read a minute later
    let a = say(&hub, &acme, json!({ "busy": false, "active": false,
        "active_at": iso(real - chrono::Duration::seconds(120)), "now": iso(real - chrono::Duration::seconds(60)) })).await;
    let t = at(&a["active_at"]);
    assert!((t - (real - chrono::Duration::seconds(60))).num_seconds().abs() <= 3, "behind: moved on by the difference ({t})");
    assert_eq!(at(&roster_row(&hub, &wes, &acme.0).await["active_at"]), t);
    // within 2 s the difference is latency: stored exactly as sent, and a
    // refresh of the same time changes nothing (no row bump, no wake)
    let sql = sql_client(&url).await;
    let exact = iso(real - chrono::Duration::seconds(30));
    let a = say(&hub, &acme, json!({ "busy": false, "active": false,
        "active_at": exact, "now": iso(Utc::now() - chrono::Duration::milliseconds(1500)) })).await;
    assert_eq!(a["active_at"], json!(exact));
    assert_eq!(roster_row(&hub, &wes, &acme.0).await["active_at"], json!(exact));
    let seq_sql = "SELECT roster_seq FROM identities WHERE slug = $1";
    let before: i64 = sql.query_one(seq_sql, &[&acme.0]).await.unwrap().get(0);
    let a = say(&hub, &acme, json!({ "busy": false, "active": true,
        "active_at": exact, "now": iso(Utc::now() + chrono::Duration::milliseconds(1500)) })).await;
    assert_eq!(a["active_at"], json!(exact));
    assert_eq!(sql.query_one(seq_sql, &[&acme.0]).await.unwrap().get::<_, i64>(0), before, "the same time said again (a turn start, a refresh): the row is not bumped");
    println!("  ok  within 2 s of the hub's clock: stored exactly, and saying it again leaves the row alone");
    let r = post(&hub, &acme, "/api/presence", json!({ "busy": false, "now": "soon" })).await;
    assert_eq!((r.code(), r.json()["detail"].clone()), (422, json!("now must be a time (RFC 3339)")), "{}", r.text());
    println!("  ok  an org's active_at is moved onto the hub's clock by its now (behind and ahead); a bad now is refused");
}

#[tokio::test]
async fn the_upgrade_fills_last_active_from_what_the_hub_has() {
    let pg = TestPg::start();
    let url = pg.fresh_db("hubseenbackfill").await;
    let data = data_dir("seen-backfill");
    let first = hub(&url, &data, &[]).await;
    let pat = register(&first, "pat", "person").await;
    let ivy = register(&first, "ivy", "person").await;
    let wes = register(&first, "wes", "person").await;
    let bot = register(&first, "bot", "chat").await;
    let acme = register(&first, "acme", "org").await;
    for who in [&pat, &ivy] {
        in_use(&first, who, "phone", true).await;
        in_use(&first, who, "phone", false).await;
    }
    let r = post(&first, &ivy, "/api/status", json!({ "status": "invisible" })).await;
    assert_eq!(r.code(), 200, "{}", r.text());
    let r = post(&first, &bot, "/api/send", json!({ "to": wes.0, "body": "hello" })).await;
    assert_eq!(r.code(), 200, "{}", r.text());
    let sent = at(&r.json()["received_at"]);
    first.shutdown.cancel();
    drop(first);

    // the database as a 2.5.1 hub left it: Ivy went Invisible an hour ago and
    // kept using the app since; schema 14, no active_at
    let sql = sql_client(&url).await;
    sql.batch_execute(&format!(
        "UPDATE identities SET status_since = now() - interval '1 hour' WHERE slug = '{ivy}';
         UPDATE devices SET active_until = now() - interval '2 minutes' WHERE slug IN ('{ivy}', '{pat}');
         DROP TABLE blocks;
         ALTER TABLE identities DROP COLUMN active_at;
         UPDATE hub_meta SET v = to_jsonb(14) WHERE k = 'schema_version';",
        ivy = ivy.0, pat = pat.0
    ))
    .await
    .unwrap();
    let seq: i64 = sql.query_one("SELECT max(roster_seq) FROM identities", &[]).await.unwrap().get(0);
    let ivy_since: DateTime<Utc> = sql.query_one("SELECT status_since FROM identities WHERE slug = $1", &[&ivy.0]).await.unwrap().get(0);
    let pat_until: DateTime<Utc> = sql.query_one("SELECT active_until FROM devices WHERE slug = $1", &[&pat.0]).await.unwrap().get(0);

    let hub = hub(&url, &data, &[]).await;
    let row = |slug: String| {
        let hub = hub.clone();
        let wes = wes.clone();
        async move { roster_row(&hub, &wes, &slug).await }
    };
    let ms = |a: DateTime<Utc>, b: DateTime<Utc>| (a - b).num_milliseconds().abs();
    assert!(ms(at(&row(pat.0.clone()).await["active_at"]), pat_until) <= 1, "a person: their devices' last use");
    assert!(ms(at(&row(ivy.0.clone()).await["active_at"]), ivy_since) <= 1, "an Invisible person: never later than when they chose it");
    assert!(ms(at(&row(bot.0.clone()).await["active_at"]), sent) <= 1, "an agent session: its last message sent");
    assert_eq!(row(acme.0.clone()).await["active_at"], Value::Null, "an org: until its engine says it");
    assert_eq!(row(wes.0.clone()).await["active_at"], Value::Null, "a person with no device ever in use: none");
    println!("  ok  schema 15 fills active_at: people from their devices (Invisible held), sessions from their sends");
    let moved: Vec<String> = sql
        .query("SELECT slug FROM identities WHERE roster_seq > $1 ORDER BY slug", &[&seq])
        .await
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect();
    let mut want = vec![pat.0.clone(), ivy.0.clone(), bot.0.clone()];
    want.sort();
    assert_eq!(moved, want, "each row filled is sent again to devices that hold a roster cursor");
    println!("  ok  the rows it fills move to the head of the roster's change log");
}
