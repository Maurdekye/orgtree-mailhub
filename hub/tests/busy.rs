//! Busy presence (v2.3.0; Orgtree docket
//! org-presence-busy-orange-when-no-org-inbox-holde): an address online can
//! be busy instead. An org says so (`POST /api/presence`) while none of its
//! org-inbox holders can start a turn, and is online again when it says
//! otherwise; a person is busy once none of their devices has been in use
//! (`POST /api/devices/active`, Hubchat in front) for `BUSY_AFTER`, an hour,
//! and online again at once when one is. Busy is a part of online: sync
//! answers carry `busy` beside `online`, roster rows a `busy` flag. The
//! hours pass on the presence clock (feature `test-clock`).
//!
//! Do not disturb (v2.3.0; the docket's addition, user 2026-10-31 08:51Z):
//! an org says it (`POST /api/presence {busy, dnd}`) while its outside mail
//! is set to Do not disturb. A DND address is listed in `dnd` and in `busy`
//! too, so a client that knows busy but not DND shows it busy; roster rows
//! carry a `dnd` flag. Offline wins over DND, as over busy (user 2026-10-31
//! 09:03Z, "Offline for both").
//!
//! A person's status (v2.3.0; the docket's addition, user 2026-10-31
//! 08:52Z): a person chooses Automatic, Online, Busy or Do not disturb in
//! Hubchat (`POST /api/status`). The hub keeps it (`identities.status`) until
//! they choose again, across a restart and while they are offline, and shows
//! it only while they are online (offline wins, 09:03Z). Online holds without
//! a device in use; Automatic is the hour's rule. Every sync answer of theirs
//! says `status`, and a device in use hears a change to it at once.
//!
//! Invisible (v2.3.0, schema 12; Orgtree docket hubchat-status-picker-add-invisible,
//! user 2026-10-31 14:13Z "add invisible as a selectable status in hubchat",
//! people only 14:14Z): a person connected but shown offline. Everyone else
//! gets exactly what a person really offline gives: out of every set, a
//! roster row saying offline, and `last_seen` held at the moment they went
//! Invisible (`identities.status_since`). Only their own sync answers say
//! `status: "invisible"`.
//!
//!     HUB_TEST_PG=<folder> cargo test --features test-clock --test busy -- --nocapture

#![cfg(feature = "test-clock")]
mod support;

use std::sync::Arc;
use std::time::{Duration, Instant};

use mailhub::api::sync::presence_loop;
use mailhub::presence::{Status, BUSY_AFTER, HANG_UP_GRACE, PRESENCE_WINDOW};
use tokio::time::timeout;
use mailhub::Hub;
use serde_json::{json, Value};
use support::*;
use tokio::task::JoinHandle;

type Id = (String, String);

async fn register(hub: &Arc<Hub>, name: &str, kind: &str) -> Id {
    let id = (format!("{name}.busy.{}", token_hex3()), token_hex16());
    let r = Call::new("POST", "/api/register")
        .auth(pair(&id.0, &id.1))
        .json(json!({ "slug": id.0, "org_name": name, "username": "busy", "kind": kind }))
        .send(hub)
        .await;
    assert_eq!(r.code(), 200, "{}", r.text());
    id
}

/// A long poll (as Orgtree's engine holds one), parked until aborted: it
/// keeps the address online however far the clock moves.
fn park_poll(hub: &Arc<Hub>, who: &Id) -> JoinHandle<Resp> {
    let (h, w) = (hub.clone(), who.clone());
    tokio::spawn(async move { Call::new("POST", "/api/poll?wait=55").auth(pair(&w.0, &w.1)).send(&h).await })
}

async fn post(hub: &Arc<Hub>, who: &Id, path: &str, body: Value) -> Resp {
    Call::new("POST", path).auth(pair(&who.0, &who.1)).json(body).send(hub).await
}

/// Hubchat on `device` comes to the front (`true`) or leaves it (`false`).
async fn in_use(hub: &Arc<Hub>, who: &Id, device: &str, on: bool) {
    let r = post(hub, who, "/api/devices/active", json!({ "device_id": device, "active": on })).await;
    assert_eq!(r.code(), 200, "{}", r.text());
}

/// The org says whether it is busy, as an engine from before DND does
/// (`busy` alone).
async fn say_busy(hub: &Arc<Hub>, who: &Id, busy: bool) {
    let r = post(hub, who, "/api/presence", json!({ "busy": busy })).await;
    assert_eq!(r.code(), 200, "{}", r.text());
    assert_eq!((r.json()["slug"].clone(), r.json()["busy"].clone()), (json!(who.0), json!(busy)), "{}", r.text());
}

/// The org says whether it is busy and whether its outside mail is set to Do
/// not disturb; the hub answers with what it took.
async fn say(hub: &Arc<Hub>, who: &Id, busy: bool, dnd: bool) {
    let r = post(hub, who, "/api/presence", json!({ "busy": busy, "dnd": dnd })).await;
    assert_eq!(r.code(), 200, "{}", r.text());
    assert_eq!(r.json(), json!({ "slug": who.0, "busy": busy, "dnd": dnd }));
}

/// The person chooses their status; the hub answers with what it took.
async fn choose(hub: &Arc<Hub>, who: &Id, status: &str) {
    let r = post(hub, who, "/api/status", json!({ "status": status })).await;
    assert_eq!(r.code(), 200, "{}", r.text());
    assert_eq!(r.json(), json!({ "slug": who.0, "status": status }));
}

/// The status a device of `who` is told: a whole sync answer, without a
/// cursor.
async fn told(hub: &Arc<Hub>, who: &Id, device: &str) -> Value {
    let r = post(hub, who, "/api/sync", json!({ "device_id": device, "wait": 0 })).await;
    assert_eq!(r.code(), 200, "{}", r.text());
    r.json()["status"].clone()
}

fn strings(v: &Value) -> Vec<String> {
    v.as_array().unwrap_or_else(|| panic!("not a list: {v}")).iter().map(|s| s.as_str().unwrap().to_string()).collect()
}

/// How `slug` shows in these sets: offline, dnd, busy or online (offline wins
/// over DND, DND over busy).
fn shown(online: &[String], busy: &[String], dnd: &[String], slug: &str) -> &'static str {
    let has = |set: &[String]| set.iter().any(|s| s == slug);
    if has(dnd) {
        assert!(has(online), "{slug} is in dnd while offline: offline wins over DND");
        assert!(has(busy), "{slug} is in dnd but not in busy: a client that knows busy only would show it online");
        return "dnd";
    }
    match (has(online), has(busy)) {
        (true, true) => "busy",
        (true, false) => "online",
        (false, true) => panic!("{slug} is busy without being online"),
        (false, false) => "offline",
    }
}

/// A watcher's device that syncs now and then, keeping the sets as it was
/// last told them.
struct Watcher {
    id: Id,
    device: &'static str,
    cursor: Value,
    online: Vec<String>,
    busy: Vec<String>,
    dnd: Vec<String>,
}

impl Watcher {
    fn new(id: &Id, device: &'static str) -> Watcher {
        Watcher { id: id.clone(), device, cursor: Value::Null, online: Vec::new(), busy: Vec::new(), dnd: Vec::new() }
    }

    async fn sync(&mut self, hub: &Arc<Hub>) {
        loop {
            let mut body = json!({ "device_id": self.device, "wait": 0 });
            if !self.cursor.is_null() {
                body["cursor"] = self.cursor.clone();
            }
            let r = post(hub, &self.id, "/api/sync", body).await;
            assert_eq!(r.code(), 200, "{}", r.text());
            let a = r.json();
            self.cursor = a["cursor"].clone();
            if let Some(on) = a.get("online") {
                self.online = strings(on);
                // `busy` comes with every `online`
                self.busy = strings(a.get("busy").unwrap_or_else(|| panic!("an answer with online and no busy: {a}")));
                // `dnd` too, from a hub that knows it (`an_org_is_dnd_while_it_says_so` checks this one does)
                self.dnd = a.get("dnd").map(strings).unwrap_or_default();
            }
            if a["more"] != json!(true) {
                return;
            }
        }
    }

    /// Sync until `who` shows as `want`, for up to 3 s: the presence loop
    /// looks every second.
    async fn sees(&mut self, hub: &Arc<Hub>, who: &Id, want: &str) {
        let t0 = Instant::now();
        loop {
            self.sync(hub).await;
            let now = shown(&self.online, &self.busy, &self.dnd, &who.0);
            if now == want {
                return;
            }
            assert!(
                t0.elapsed() < Duration::from_secs(3),
                "{} shows {now}, not {want} (online {:?}, busy {:?}, dnd {:?})",
                who.0,
                self.online,
                self.busy,
                self.dnd
            );
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }
}

/// The address's roster row as a viewer gets it (`GET /api/roster`; a v1
/// poll carries the same rows).
async fn roster_row(hub: &Arc<Hub>, viewer: &Id, slug: &str) -> Value {
    let r = Call::new("GET", "/api/roster").auth(pair(&viewer.0, &viewer.1)).send(hub).await.json();
    r["roster"].as_array().unwrap().iter().find(|e| e["slug"] == json!(slug)).cloned().unwrap_or_else(|| panic!("no {slug} in {r}"))
}

async fn row_shows(hub: &Arc<Hub>, viewer: &Id, slug: &str) -> (Value, Value) {
    let row = roster_row(hub, viewer, slug).await;
    (row["online"].clone(), row["busy"].clone())
}

/// The row's online, busy and dnd.
async fn row_dnd(hub: &Arc<Hub>, viewer: &Id, slug: &str) -> (Value, Value, Value) {
    let row = roster_row(hub, viewer, slug).await;
    (row["online"].clone(), row["busy"].clone(), row["dnd"].clone())
}

fn data_dir(name: &str) -> std::path::PathBuf {
    let data = std::env::temp_dir().join(format!("orgtree-hub-{name}-{}", token_hex16()));
    std::fs::create_dir_all(&data).unwrap();
    data
}

#[tokio::test]
async fn an_org_is_busy_while_it_says_so() {
    let pg = TestPg::start();
    let url = pg.fresh_db("hubbusyorg").await;
    let data = data_dir("busy-org");
    let hub = hub(&url, &data, &[]).await;
    tokio::spawn(presence_loop(hub.clone()));

    let h = Call::new("GET", "/healthz").send(&hub).await.json();
    assert!(h["features"].as_array().unwrap().contains(&json!("busy")), "{h}");
    println!("  ok  /healthz lists the feature busy");

    let acme = register(&hub, "acme", "org").await; // an Orgtree engine
    let olde = register(&hub, "olde", "org").await; // an engine from before busy, which never says it
    let wes = register(&hub, "wes", "person").await; // watching from Hubchat
    let acme_poll = park_poll(&hub, &acme);
    let _olde_poll = park_poll(&hub, &olde);
    let mut w = Watcher::new(&wes, "wes-desk");

    w.sees(&hub, &acme, "online").await;
    assert_eq!(row_shows(&hub, &wes, &acme.0).await, (json!(true), json!(false)));
    println!("  ok  an org that polls is online; its roster row says busy false");

    say_busy(&hub, &acme, true).await;
    w.sees(&hub, &acme, "busy").await;
    assert_eq!(row_shows(&hub, &wes, &acme.0).await, (json!(true), json!(true)));
    println!("  ok  it says it is busy: sync answers list it in busy (still in online), its roster row says busy");

    say_busy(&hub, &acme, false).await;
    w.sees(&hub, &acme, "online").await;
    assert_eq!(row_shows(&hub, &wes, &acme.0).await, (json!(true), json!(false)));
    println!("  ok  online -> busy -> online: it says it can answer again");

    // only people go busy for want of a device in use; an org never does
    hub.presence.advance(BUSY_AFTER * 3);
    w.sees(&hub, &acme, "online").await;
    w.sees(&hub, &olde, "online").await;
    println!("  ok  three hours on, an org that does not say busy is online (one from before busy too)");

    // busy ends with going offline: a client that comes back says it again
    say_busy(&hub, &acme, true).await;
    w.sees(&hub, &acme, "busy").await;
    acme_poll.abort();
    tokio::time::sleep(Duration::from_millis(300)).await;
    hub.presence.advance(HANG_UP_GRACE + Duration::from_secs(1));
    w.sees(&hub, &acme, "offline").await;
    let _acme_again = park_poll(&hub, &acme);
    w.sees(&hub, &acme, "online").await;
    println!("  ok  gone offline, its busy ended with it: back, it is online until it says busy again");

    for (body, code, detail) in [
        (json!({}), 422, "busy is required: true or false"),
        (json!({ "busy": null }), 422, "busy is required: true or false"),
        (json!({ "busy": "yes" }), 422, "busy must be true or false"),
    ] {
        let r = post(&hub, &acme, "/api/presence", body.clone()).await;
        assert_eq!((r.code(), r.json()["detail"].clone()), (code, json!(detail)), "{body}");
    }
    let r = Call::new("POST", "/api/presence").json(json!({ "busy": true })).send(&hub).await;
    assert_eq!(r.code(), 401, "{}", r.text());
    println!("  ok  refused: busy missing or not a boolean (422), no credentials (401)");

    let _ = std::fs::remove_dir_all(&data);
    drop(pg);
}

#[tokio::test]
async fn an_org_is_dnd_while_it_says_so() {
    let pg = TestPg::start();
    let url = pg.fresh_db("hubdndorg").await;
    let data = data_dir("dnd-org");
    let hub = hub(&url, &data, &[]).await;
    tokio::spawn(presence_loop(hub.clone()));

    let acme = register(&hub, "acme", "org").await; // an Orgtree engine
    let olde = register(&hub, "olde", "org").await; // an engine from before DND: it says busy alone
    let wes = register(&hub, "wes", "person").await; // watching from Hubchat
    let acme_poll = park_poll(&hub, &acme);
    let _olde_poll = park_poll(&hub, &olde);
    let mut w = Watcher::new(&wes, "wes-desk");
    w.sees(&hub, &acme, "online").await;

    say(&hub, &acme, false, true).await;
    w.sees(&hub, &acme, "dnd").await;
    assert_eq!(row_dnd(&hub, &wes, &acme.0).await, (json!(true), json!(true), json!(true)));
    println!("  ok  its outside mail on Do not disturb: listed in dnd and in busy (a client that knows busy only shows it busy), still online; its roster row says dnd and busy");

    say(&hub, &acme, true, true).await;
    w.sees(&hub, &acme, "dnd").await;
    say(&hub, &acme, true, false).await;
    w.sees(&hub, &acme, "busy").await;
    assert_eq!(row_dnd(&hub, &wes, &acme.0).await, (json!(true), json!(true), json!(false)));
    say(&hub, &acme, false, false).await;
    w.sees(&hub, &acme, "online").await;
    assert_eq!(row_dnd(&hub, &wes, &acme.0).await, (json!(true), json!(false), json!(false)));
    println!("  ok  online -> dnd -> busy -> online: DND outranks busy, and each ends when it says so");

    // an engine from before DND says busy alone: never DND
    say_busy(&hub, &olde, true).await;
    w.sees(&hub, &olde, "busy").await;
    let r = post(&hub, &acme, "/api/presence", json!({ "busy": false })).await;
    assert_eq!(r.json(), json!({ "slug": acme.0, "busy": false, "dnd": false }));
    println!("  ok  busy alone (an engine from before DND) means not DND");

    // a device in use hears of a change in DND alone at once, as of one online
    say(&hub, &acme, true, false).await;
    w.sees(&hub, &acme, "busy").await;
    in_use(&hub, &wes, "wes-desk", true).await;
    w.sync(&hub).await;
    let (h, who, cursor) = (hub.clone(), wes.clone(), w.cursor.clone());
    let parked = tokio::spawn(async move { post(&h, &who, "/api/sync", json!({ "device_id": "wes-desk", "wait": 25, "cursor": cursor })).await });
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(!parked.is_finished(), "the sync did not park: nothing was new");
    let t0 = Instant::now();
    say(&hub, &acme, true, true).await;
    let a = tokio::time::timeout(Duration::from_secs(5), parked).await.expect("the parked sync was not woken").unwrap().json();
    assert!(t0.elapsed() < Duration::from_secs(3), "woken after {:?}", t0.elapsed());
    assert!(strings(&a["dnd"]).contains(&acme.0) && strings(&a["busy"]).contains(&acme.0), "{a}");
    println!("  ok  busy -> dnd wakes a device in use whose sync is parked, in {:?}; `dnd` comes with `online` and `busy`", t0.elapsed());

    // offline wins over DND, as over busy (user 2026-10-31 09:03Z, "Offline
    // for both"): its DND ends with going offline; back, it says it again
    acme_poll.abort();
    tokio::time::sleep(Duration::from_millis(300)).await;
    hub.presence.advance(HANG_UP_GRACE + Duration::from_secs(1));
    w.sees(&hub, &acme, "offline").await;
    assert_eq!(row_dnd(&hub, &wes, &acme.0).await, (json!(false), json!(false), json!(false)));
    let _acme_again = park_poll(&hub, &acme);
    w.sees(&hub, &acme, "online").await;
    println!("  ok  gone offline while DND, it shows offline (offline wins) and its DND ended with it: back, online until it says DND again");

    for (body, code, detail) in [
        (json!({ "busy": false, "dnd": "yes" }), 422, "dnd must be true or false"),
        (json!({ "dnd": true }), 422, "busy is required: true or false"),
    ] {
        let r = post(&hub, &acme, "/api/presence", body.clone()).await;
        assert_eq!((r.code(), r.json()["detail"].clone()), (code, json!(detail)), "{body}");
    }
    println!("  ok  refused: dnd not a boolean, dnd without busy (422)");

    let h = Call::new("GET", "/healthz").send(&hub).await.json();
    assert!(h["features"].as_array().unwrap().contains(&json!("dnd")), "{h}");
    println!("  ok  /healthz lists the feature dnd");

    let _ = std::fs::remove_dir_all(&data);
    drop(pg);
}

#[tokio::test]
async fn a_person_is_busy_an_hour_after_their_last_device_in_use() {
    let pg = TestPg::start();
    let url = pg.fresh_db("hubbusyperson").await;
    let data = data_dir("busy-person");
    let hub = hub(&url, &data, &[]).await;
    tokio::spawn(presence_loop(hub.clone()));

    let pat = register(&hub, "pat", "person").await; // Hubchat on a laptop and a phone
    let wes = register(&hub, "wes", "person").await;
    // pat stays connected throughout, as a phone that stays connected does
    let _pat_poll = park_poll(&hub, &pat);
    let mut w = Watcher::new(&wes, "wes-desk");

    in_use(&hub, &pat, "pat-laptop", true).await;
    w.sees(&hub, &pat, "online").await;
    in_use(&hub, &pat, "pat-laptop", false).await;
    hub.presence.advance(BUSY_AFTER - Duration::from_secs(60));
    w.sees(&hub, &pat, "online").await;
    println!("  ok  59 minutes after Hubchat left the front on the laptop: still online");

    hub.presence.advance(Duration::from_secs(120));
    w.sees(&hub, &pat, "busy").await;
    assert_eq!(row_shows(&hub, &wes, &pat.0).await, (json!(true), json!(true)));
    println!("  ok  an hour on: busy, in sync answers and the roster row");

    in_use(&hub, &pat, "pat-phone", true).await;
    w.sees(&hub, &pat, "online").await;
    assert_eq!(row_shows(&hub, &wes, &pat.0).await, (json!(true), json!(false)));
    println!("  ok  online -> busy -> online: Hubchat in front on another device, online again at once");

    // a device that stops renewing (it crashed or slept while in front)
    // counts until its 90-second lease ran out, then an hour
    hub.presence.advance(BUSY_AFTER);
    w.sees(&hub, &pat, "online").await;
    hub.presence.advance(Duration::from_secs(120));
    w.sees(&hub, &pat, "busy").await;
    println!("  ok  a device that stops renewing counts until its lease runs out, then busy an hour after");

    // someone the hub has seen no device of counts from when it first saw them
    let quinn = register(&hub, "quinn", "person").await;
    let _quinn_poll = park_poll(&hub, &quinn);
    w.sees(&hub, &quinn, "online").await;
    hub.presence.advance(BUSY_AFTER - Duration::from_secs(60));
    w.sees(&hub, &quinn, "online").await;
    hub.presence.advance(Duration::from_secs(120));
    w.sees(&hub, &quinn, "busy").await;
    println!("  ok  a person never seen in front counts from when the hub first saw them: busy an hour later");

    // a chat client (v1) has no devices in use and is never busy
    let cli = register(&hub, "cli", "chat").await;
    let _cli_poll = park_poll(&hub, &cli);
    hub.presence.advance(BUSY_AFTER * 2);
    w.sees(&hub, &cli, "online").await;
    println!("  ok  a chat client is never busy");

    let _ = std::fs::remove_dir_all(&data);
    drop(pg);
}

#[tokio::test]
async fn the_last_device_in_use_holds_across_a_restart() {
    let pg = TestPg::start();
    let url = pg.fresh_db("hubbusyrestart").await;
    let data = data_dir("busy-restart");
    let first = hub(&url, &data, &[]).await;
    let pat = register(&first, "pat", "person").await;
    let wes = register(&first, "wes", "person").await;
    in_use(&first, &pat, "pat-laptop", true).await;
    in_use(&first, &pat, "pat-laptop", false).await;

    // the device is not in use, as before 2.3.0, but its row keeps when it
    // stopped (it used to be cleared)
    let d = Call::new("GET", "/api/devices").auth(pair(&pat.0, &pat.1)).send(&first).await.json();
    let laptop = d["devices"].as_array().unwrap().iter().find(|x| x["device_id"] == json!("pat-laptop")).cloned().unwrap();
    assert_eq!((laptop["active"].clone(), laptop["active_until"].clone()), (json!(false), Value::Null), "{laptop}");
    let sql = sql_client(&url).await;
    let until: Option<chrono::DateTime<chrono::Utc>> =
        sql.query_one("SELECT active_until FROM devices WHERE slug = $1 AND device_id = 'pat-laptop'", &[&pat.0]).await.unwrap().get(0);
    let until = until.expect("active_until keeps when the device stopped being in use");
    assert!((chrono::Utc::now() - until).num_seconds().abs() < 30, "{until}");
    println!("  ok  a device put down is not in use (devices list, push), and its row keeps when it stopped");

    // say it was put down 50 minutes before the hub restarts
    sql.execute("UPDATE devices SET active_until = now() - interval '50 minutes' WHERE slug = $1", &[&pat.0]).await.unwrap();
    first.shutdown.cancel();
    drop(first);

    let hub = hub(&url, &data, &[]).await;
    tokio::spawn(presence_loop(hub.clone()));
    let _pat_poll = park_poll(&hub, &pat);
    let mut w = Watcher::new(&wes, "wes-desk");
    w.sees(&hub, &pat, "online").await;
    hub.presence.advance(Duration::from_secs(9 * 60));
    w.sees(&hub, &pat, "online").await;
    hub.presence.advance(Duration::from_secs(2 * 60));
    w.sees(&hub, &pat, "busy").await;
    println!("  ok  after a restart, busy an hour after the laptop was put down (not an hour after the restart)");

    let _ = std::fs::remove_dir_all(&data);
    drop(pg);
}

#[tokio::test]
async fn a_person_chooses_their_status() {
    let pg = TestPg::start();
    let url = pg.fresh_db("hubstatus").await;
    let data = data_dir("status");
    let first = hub(&url, &data, &[]).await;
    tokio::spawn(presence_loop(first.clone()));

    let h = Call::new("GET", "/healthz").send(&first).await.json();
    assert!(h["features"].as_array().unwrap().contains(&json!("status")), "{h}");
    println!("  ok  /healthz lists the feature status");

    let pat = register(&first, "pat", "person").await; // Hubchat on a laptop and a phone
    let wes = register(&first, "wes", "person").await; // watching from Hubchat
    let pat_poll = park_poll(&first, &pat);
    in_use(&first, &pat, "pat-laptop", true).await;
    let mut w = Watcher::new(&wes, "wes-desk");
    w.sees(&first, &pat, "online").await;
    assert_eq!(told(&first, &pat, "pat-phone").await, json!("auto"));
    println!("  ok  a person starts on Automatic: their sync answers say status auto");

    choose(&first, &pat, "dnd").await;
    w.sees(&first, &pat, "dnd").await;
    assert_eq!(row_dnd(&first, &wes, &pat.0).await, (json!(true), json!(true), json!(true)));
    assert_eq!(told(&first, &pat, "pat-phone").await, json!("dnd"));
    println!("  ok  Do not disturb: listed in dnd and busy, its roster row says dnd; every device of theirs is told status dnd");

    choose(&first, &pat, "busy").await;
    w.sees(&first, &pat, "busy").await;
    assert_eq!(row_dnd(&first, &wes, &pat.0).await, (json!(true), json!(true), json!(false)));
    choose(&first, &pat, "online").await;
    w.sees(&first, &pat, "online").await;
    println!("  ok  dnd -> busy -> online, each at once, with the laptop in front");

    // Online holds without a device in use; Automatic is the hour's rule
    in_use(&first, &pat, "pat-laptop", false).await;
    first.presence.advance(BUSY_AFTER + Duration::from_secs(60));
    w.sees(&first, &pat, "online").await;
    choose(&first, &pat, "auto").await;
    w.sees(&first, &pat, "busy").await;
    in_use(&first, &pat, "pat-phone", true).await;
    w.sees(&first, &pat, "online").await;
    println!("  ok  Online holds an hour after their last device in use; Automatic is busy then, and online with the phone in front");

    // a device in use hears of a change to its own status at once, even one
    // that changes no set (Automatic -> Online with the phone in front)
    let mut cursor = Value::Null;
    loop {
        let mut body = json!({ "device_id": "pat-phone", "wait": 0 });
        if !cursor.is_null() {
            body["cursor"] = cursor.clone();
        }
        let a = post(&first, &pat, "/api/sync", body).await.json();
        cursor = a["cursor"].clone();
        if a["more"] != json!(true) {
            break;
        }
    }
    let (h, who) = (first.clone(), pat.clone());
    let parked = tokio::spawn(async move { post(&h, &who, "/api/sync", json!({ "device_id": "pat-phone", "wait": 25, "cursor": cursor })).await });
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(!parked.is_finished(), "the sync did not park: nothing was new");
    let t0 = Instant::now();
    choose(&first, &pat, "online").await;
    let a = timeout(Duration::from_secs(5), parked).await.expect("the parked sync was not woken").unwrap().json();
    assert!(t0.elapsed() < Duration::from_secs(3), "woken after {:?}", t0.elapsed());
    assert_eq!(a["status"], json!("online"), "{a}");
    assert_eq!(shown(&strings(&a["online"]), &strings(&a["busy"]), &strings(&a["dnd"]), &pat.0), "online", "no set changed: {a}");
    println!("  ok  Automatic -> Online wakes their device in use whose sync is parked, in {:?}, though no set changed", t0.elapsed());

    // offline wins (user 2026-10-31 09:03Z, "Offline for both"); the hub
    // keeps the choice, so it shows again when they are back
    choose(&first, &pat, "dnd").await;
    w.sees(&first, &pat, "dnd").await;
    in_use(&first, &pat, "pat-phone", false).await;
    pat_poll.abort();
    tokio::time::sleep(Duration::from_millis(300)).await;
    first.presence.advance(HANG_UP_GRACE + Duration::from_secs(1));
    w.sees(&first, &pat, "offline").await;
    assert_eq!(row_dnd(&first, &wes, &pat.0).await, (json!(false), json!(false), json!(false)));
    let pat_again = park_poll(&first, &pat);
    w.sees(&first, &pat, "dnd").await;
    println!("  ok  offline on Do not disturb: shown offline; back, Do not disturb again (the hub kept it)");

    // kept in the database: a restart keeps it
    let sql = sql_client(&url).await;
    let stored: String = sql.query_one("SELECT status FROM identities WHERE slug = $1", &[&pat.0]).await.unwrap().get(0);
    assert_eq!(stored, "dnd");
    pat_again.abort();
    first.shutdown.cancel();
    drop(first);
    let hub = hub(&url, &data, &[]).await;
    tokio::spawn(presence_loop(hub.clone()));
    let _pat_poll = park_poll(&hub, &pat);
    let mut w = Watcher::new(&wes, "wes-desk");
    w.sees(&hub, &pat, "dnd").await;
    assert_eq!(told(&hub, &pat, "pat-phone").await, json!("dnd"));
    println!("  ok  kept in identities.status across a restart: Do not disturb as soon as they connect");

    for (body, code, detail) in [
        (json!({}), 422, "status is required: auto, online, busy, dnd or invisible"),
        (json!({ "status": null }), 422, "status is required: auto, online, busy, dnd or invisible"),
        (json!({ "status": "away" }), 422, "status must be auto, online, busy, dnd or invisible"),
        (json!({ "status": true }), 422, "status must be auto, online, busy, dnd or invisible"),
    ] {
        let r = post(&hub, &pat, "/api/status", body.clone()).await;
        assert_eq!((r.code(), r.json()["detail"].clone()), (code, json!(detail)), "{body}");
    }
    let acme = register(&hub, "acme", "org").await;
    let r = post(&hub, &acme, "/api/status", json!({ "status": "dnd" })).await;
    assert_eq!(
        (r.code(), r.json()["detail"].clone()),
        (403, json!("only a person chooses a status; an org says busy and Do not disturb with POST /api/presence"))
    );
    let r = Call::new("POST", "/api/status").json(json!({ "status": "dnd" })).send(&hub).await;
    assert_eq!(r.code(), 401, "{}", r.text());
    assert_eq!(told(&hub, &pat, "pat-phone").await, json!("dnd"), "a refusal changes nothing");
    println!("  ok  refused: status missing or not auto, online, busy, dnd or invisible (422), an org (403), no credentials (401)");

    let _ = std::fs::remove_dir_all(&data);
    drop(pg);
}

/// A person goes Invisible: everyone else sees them offline at once, with
/// `last_seen` held at that moment however much they do; their own devices
/// are told `invisible`; it holds across a restart; Automatic shows them again.
#[tokio::test]
async fn an_invisible_person_shows_offline_to_everyone_else() {
    let pg = TestPg::start();
    let url = pg.fresh_db("hubinvisible").await;
    let data = data_dir("invisible");
    let first = hub(&url, &data, &[]).await;
    tokio::spawn(presence_loop(first.clone()));

    let h = Call::new("GET", "/healthz").send(&first).await.json();
    assert!(h["features"].as_array().unwrap().contains(&json!("invisible")), "{h}");
    println!("  ok  /healthz lists the feature invisible");

    let pat = register(&first, "pat", "person").await;
    let wes = register(&first, "wes", "person").await;
    let acme = register(&first, "acme", "org").await;
    let pat_poll = park_poll(&first, &pat);
    in_use(&first, &pat, "pat-laptop", true).await;
    let mut w = Watcher::new(&wes, "wes-desk");
    w.sees(&first, &pat, "online").await;

    let t0 = Instant::now();
    choose(&first, &pat, "invisible").await;
    w.sees(&first, &pat, "offline").await;
    println!("  ok  Invisible with the laptop in front and a poll parked: offline to another person in {:?}", t0.elapsed());
    assert_eq!(row_dnd(&first, &wes, &pat.0).await, (json!(false), json!(false), json!(false)));
    assert_eq!(row_dnd(&first, &acme, &pat.0).await, (json!(false), json!(false), json!(false)));
    assert_eq!(told(&first, &pat, "pat-phone").await, json!("invisible"));
    println!("  ok  roster rows say offline to a person and to an org; their own devices are told status invisible");

    let held = roster_row(&first, &wes, &pat.0).await["last_seen"].clone();
    assert!(held.is_string(), "{held}");
    tokio::time::sleep(Duration::from_millis(1100)).await;
    in_use(&first, &pat, "pat-laptop", false).await;
    in_use(&first, &pat, "pat-laptop", true).await;
    told(&first, &pat, "pat-phone").await;
    assert_eq!(roster_row(&first, &wes, &pat.0).await["last_seen"], held, "last_seen moved on while Invisible");
    assert_eq!(roster_row(&first, &acme, &pat.0).await["last_seen"], held);
    let sql = sql_client(&url).await;
    let r = sql.query_one("SELECT status, status_since IS NOT NULL, last_seen > status_since FROM identities WHERE slug = $1", &[&pat.0]).await.unwrap();
    assert_eq!((r.get::<_, String>(0), r.get::<_, bool>(1), r.get::<_, bool>(2)), ("invisible".to_string(), true, true));
    println!("  ok  last_seen held at the moment they went Invisible while they keep using it (the real one, for the idle sweep, moves on)");

    pat_poll.abort();
    first.shutdown.cancel();
    drop(first);
    let hub = hub(&url, &data, &[]).await;
    tokio::spawn(presence_loop(hub.clone()));
    let pat_poll = park_poll(&hub, &pat);
    in_use(&hub, &pat, "pat-laptop", true).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let mut w = Watcher::new(&wes, "wes-desk");
    w.sync(&hub).await;
    assert_eq!(shown(&w.online, &w.busy, &w.dnd, &pat.0), "offline");
    assert_eq!(roster_row(&hub, &wes, &pat.0).await["last_seen"], held);
    assert_eq!(told(&hub, &pat, "pat-phone").await, json!("invisible"));
    println!("  ok  kept across a restart: still offline to others, last_seen still held, their devices told invisible");

    choose(&hub, &pat, "auto").await;
    w.sees(&hub, &pat, "online").await;
    let since: Option<chrono::DateTime<chrono::Utc>> = sql.query_one("SELECT status_since FROM identities WHERE slug = $1", &[&pat.0]).await.unwrap().get(0);
    assert!(since.is_none());
    assert_ne!(roster_row(&hub, &wes, &pat.0).await["last_seen"], held);
    println!("  ok  back to Automatic: online at once, last_seen their real one again");
    pat_poll.abort();

    let _ = std::fs::remove_dir_all(&data);
    drop(pg);
}

/// What a viewer can read of `who`: its row in `GET /api/roster`, its entry
/// in `GET /api/directory`, and, for a viewer that syncs, its row and the
/// three sets in a whole sync from scratch.
async fn everything_about(hub: &Arc<Hub>, viewer: &Id, device: Option<&str>, who: &Id) -> Value {
    let mut seen = json!({ "roster": roster_row(hub, viewer, &who.0).await });
    let d = Call::new("GET", &format!("/api/directory?q={}", who.0)).auth(pair(&viewer.0, &viewer.1)).send(hub).await;
    assert_eq!(d.code(), 200, "{}", d.text());
    seen["directory"] = d.json()["entries"].as_array().unwrap().iter().find(|e| e["slug"] == json!(who.0)).cloned().unwrap();
    if let Some(device) = device {
        let (mut cursor, mut row, mut sets) = (Value::Null, Value::Null, Value::Null);
        loop {
            let mut body = json!({ "device_id": device, "wait": 0 });
            if !cursor.is_null() {
                body["cursor"] = cursor.clone();
            }
            let a = post(hub, viewer, "/api/sync", body).await.json();
            if let Some(r) = a["roster"].as_array().unwrap().iter().find(|e| e["slug"] == json!(who.0)) {
                row = r.clone();
            }
            if a.get("online").is_some() {
                sets = json!({ "online": a["online"], "busy": a["busy"], "dnd": a["dnd"] });
            }
            cursor = a["cursor"].clone();
            if a["more"] != json!(true) {
                break;
            }
        }
        seen["sync_roster"] = row;
        seen["sync_sets"] = sets;
    }
    seen
}

/// The leak test: nobody else can tell Invisible from offline. A person and
/// an org read everything the hub gives about Pat while Pat is connected and
/// Invisible, then again when Pat is really offline (on Automatic, last seen
/// at the same moment, set so by construction): byte for byte the same.
#[tokio::test]
async fn nobody_can_tell_invisible_from_offline() {
    let pg = TestPg::start();
    let url = pg.fresh_db("hubinvisibleleak").await;
    let data = data_dir("invisibleleak");
    let hub = hub(&url, &data, &[]).await;
    tokio::spawn(presence_loop(hub.clone()));
    let pat = register(&hub, "pat", "person").await;
    let wes = register(&hub, "wes", "person").await;
    let acme = register(&hub, "acme", "org").await;
    let _wes_poll = park_poll(&hub, &wes);
    let _acme_poll = park_poll(&hub, &acme);
    in_use(&hub, &wes, "wes-desk", true).await;

    // A: connected (a parked poll, the laptop in front) and Invisible
    let pat_poll = park_poll(&hub, &pat);
    in_use(&hub, &pat, "pat-laptop", true).await;
    choose(&hub, &pat, "invisible").await;
    let mut w = Watcher::new(&wes, "wes-desk");
    w.sees(&hub, &pat, "offline").await;
    let held = roster_row(&hub, &wes, &pat.0).await["last_seen"].clone();
    let a_wes = everything_about(&hub, &wes, Some("wes-fresh-a"), &pat).await;
    let a_acme = everything_about(&hub, &acme, None, &pat).await;

    // B: really offline, on Automatic, last seen at that same moment
    pat_poll.abort();
    in_use(&hub, &pat, "pat-laptop", false).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    hub.presence.advance(PRESENCE_WINDOW + HANG_UP_GRACE + Duration::from_secs(1));
    let sql = sql_client(&url).await;
    let at = chrono::DateTime::parse_from_rfc3339(held.as_str().unwrap()).unwrap().with_timezone(&chrono::Utc);
    sql.execute("UPDATE identities SET status = 'auto', status_since = NULL, last_seen = $2 WHERE slug = $1", &[&pat.0, &at]).await.unwrap();
    hub.presence.set_status(&pat.0, Status::Auto);
    w.sees(&hub, &pat, "offline").await;
    let b_wes = everything_about(&hub, &wes, Some("wes-fresh-b"), &pat).await;
    let b_acme = everything_about(&hub, &acme, None, &pat).await;

    assert_eq!(serde_json::to_string(&a_wes).unwrap(), serde_json::to_string(&b_wes).unwrap(), "a person can tell Invisible from offline");
    assert_eq!(serde_json::to_string(&a_acme).unwrap(), serde_json::to_string(&b_acme).unwrap(), "an org can tell Invisible from offline");
    assert!(!a_wes["sync_sets"]["online"].as_array().unwrap().contains(&json!(pat.0)), "{a_wes}");
    println!("  ok  a person (roster, directory, a whole sync: row and sets) and an org (roster, directory) read the same bytes about Invisible Pat as about offline Pat");

    let _ = std::fs::remove_dir_all(&data);
    drop(pg);
}
