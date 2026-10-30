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
//!     HUB_TEST_PG=<folder> cargo test --features test-clock --test busy -- --nocapture

#![cfg(feature = "test-clock")]
mod support;

use std::sync::Arc;
use std::time::{Duration, Instant};

use mailhub::api::sync::presence_loop;
use mailhub::presence::{BUSY_AFTER, HANG_UP_GRACE};
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

/// The org says whether it is busy.
async fn say_busy(hub: &Arc<Hub>, who: &Id, busy: bool) {
    let r = post(hub, who, "/api/presence", json!({ "busy": busy })).await;
    assert_eq!(r.code(), 200, "{}", r.text());
    assert_eq!(r.json(), json!({ "slug": who.0, "busy": busy }));
}

fn strings(v: &Value) -> Vec<String> {
    v.as_array().unwrap_or_else(|| panic!("not a list: {v}")).iter().map(|s| s.as_str().unwrap().to_string()).collect()
}

/// How `slug` shows in these sets: online, busy or offline.
fn shown(online: &[String], busy: &[String], slug: &str) -> &'static str {
    match (online.iter().any(|s| s == slug), busy.iter().any(|s| s == slug)) {
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
}

impl Watcher {
    fn new(id: &Id, device: &'static str) -> Watcher {
        Watcher { id: id.clone(), device, cursor: Value::Null, online: Vec::new(), busy: Vec::new() }
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
            let now = shown(&self.online, &self.busy, &who.0);
            if now == want {
                return;
            }
            assert!(
                t0.elapsed() < Duration::from_secs(3),
                "{} shows {now}, not {want} (online {:?}, busy {:?})",
                who.0,
                self.online,
                self.busy
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
