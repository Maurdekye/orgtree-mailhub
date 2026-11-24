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
use mailhub::presence::HANG_UP_GRACE;
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

    // Invisible: never active, active_at held at the moment chosen
    in_use(&hub, &pat, "pat-phone", true).await;
    w.sees_active(&hub, &pat, true).await;
    let r = post(&hub, &pat, "/api/status", json!({ "status": "invisible" })).await;
    assert_eq!(r.code(), 200, "{}", r.text());
    w.sees_active(&hub, &pat, false).await;
    let held = roster_row(&hub, &wes, &pat.0).await["active_at"].clone();
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

    // what it says may move back (its holder changed), but never past now
    say(&hub, &acme, json!({ "busy": false, "active": false, "active_at": "2026-11-24T09:00:00.000Z" })).await;
    assert_eq!(roster_row(&hub, &wes, &acme.0).await["active_at"], json!("2026-11-24T09:00:00.000Z"));
    let a = say(&hub, &acme, json!({ "busy": false, "active": false, "active_at": "2999-01-01T00:00:00.000Z" })).await;
    assert!(at(&a["active_at"]) <= Utc::now(), "a time in the future is taken as now: {a}");
    assert!(at(&roster_row(&hub, &wes, &acme.0).await["active_at"]) <= Utc::now());
    say(&hub, &acme, json!({ "busy": false, "active": false, "active_at": null })).await;
    assert_eq!(roster_row(&hub, &wes, &acme.0).await["active_at"], Value::Null);
    println!("  ok  active_at may move back, is never later than now, and null clears it");

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
    // the hang-up lands when the aborted task is dropped
    tokio::time::sleep(Duration::from_millis(300)).await;
    hub.presence.advance(HANG_UP_GRACE + Duration::from_secs(1));
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
