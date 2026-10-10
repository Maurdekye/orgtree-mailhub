//! Presence (v2.0.2): a client that hangs up on a parked poll or sync (its
//! process stopped, was killed or crashed) goes offline after a short grace,
//! not v1's 90-second window, and the parked sync of a device in use
//! answers as soon as the set online changes. A device not in use learns
//! it with its next answer, or at once when it comes into use. v2.0.3: that
//! answer carries the departed address's roster entry with `last_seen` its
//! last call, and a device put down mid-park stops being woken at once.
//!
//!     HUB_TEST_PG=<folder> cargo test --test presence -- --nocapture

mod support;

use std::sync::Arc;
use std::time::{Duration, Instant};

use mailhub::api::sync::presence_loop;
use mailhub::presence::HANG_UP_GRACE;
use mailhub::Hub;
use serde_json::{json, Value};
use support::*;
use tokio::task::JoinHandle;

type Id = (String, String);

async fn register(hub: &Arc<Hub>, name: &str, kind: &str) -> Id {
    let id = (format!("{name}.presence.{}", token_hex3()), token_hex16());
    let r = Call::new("POST", "/api/register")
        .auth(pair(&id.0, &id.1))
        .json(json!({ "slug": id.0, "org_name": name, "username": "presence", "kind": kind }))
        .send(hub)
        .await;
    assert_eq!(r.code(), 200, "{}", r.text());
    id
}

/// A long poll (as Orgtree's engine holds one), parked until aborted.
fn park_poll(hub: &Arc<Hub>, who: &Id) -> JoinHandle<Resp> {
    let (h, w) = (hub.clone(), who.clone());
    tokio::spawn(async move { Call::new("POST", "/api/poll?wait=55").auth(pair(&w.0, &w.1)).send(&h).await })
}

async fn sync_now(hub: &Arc<Hub>, who: &Id, device: &str, cursor: Option<&Value>) -> Value {
    let mut body = json!({ "device_id": device, "wait": 0 });
    if let Some(c) = cursor {
        body["cursor"] = c.clone();
    }
    loop {
        let r = Call::new("POST", "/api/sync").auth(pair(&who.0, &who.1)).json(body.clone()).send(hub).await;
        assert_eq!(r.code(), 200, "{}", r.text());
        let a = r.json();
        if a["more"] != json!(true) {
            return a;
        }
        body["cursor"] = a["cursor"].clone();
    }
}

/// A sync parked for up to 55 s (as Hubchat parks one): its answer and when
/// it came.
fn park_sync(hub: &Arc<Hub>, who: &Id, device: &str, cursor: &Value) -> JoinHandle<(Value, Instant)> {
    let (h, w, body) = (hub.clone(), who.clone(), json!({ "device_id": device, "cursor": cursor, "wait": 55 }));
    tokio::spawn(async move {
        let r = Call::new("POST", "/api/sync").auth(pair(&w.0, &w.1)).json(body).send(&h).await;
        assert_eq!(r.code(), 200, "{}", r.text());
        (r.json(), Instant::now())
    })
}

async fn in_use(hub: &Arc<Hub>, who: &Id, device: &str) {
    let r = Call::new("POST", "/api/devices/active").auth(pair(&who.0, &who.1)).json(json!({ "device_id": device, "active": true })).send(hub).await;
    assert_eq!(r.code(), 200, "{}", r.text());
}

async fn roster_online(hub: &Arc<Hub>, viewer: &Id, slug: &str) -> bool {
    let r = Call::new("GET", "/api/roster").auth(pair(&viewer.0, &viewer.1)).send(hub).await.json();
    r["roster"].as_array().unwrap().iter().find(|e| e["slug"] == json!(slug)).map(|e| e["online"] == json!(true)).unwrap()
}

fn lists(answer: &Value, slug: &str) -> bool {
    answer["online"].as_array().unwrap_or_else(|| panic!("no online in {answer}")).contains(&json!(slug))
}

#[tokio::test]
async fn a_client_that_hangs_up_goes_offline_soon() {
    assert_eq!(HANG_UP_GRACE, Duration::from_secs(10));
    let pg = TestPg::start();
    let url = pg.fresh_db("hubpresence").await;
    let data = std::env::temp_dir().join(format!("orgtree-hub-presence-{}", token_hex16()));
    std::fs::create_dir_all(&data).unwrap();
    let hub = hub(&url, &data, &[]).await;
    tokio::spawn(presence_loop(hub.clone()));

    let ann = register(&hub, "ann", "org").await; // an Orgtree engine that is killed
    let dan = register(&hub, "dan", "org").await; // an engine whose poller restarts
    let cat = register(&hub, "cat", "person").await; // a Hubchat device that quits
    let wes = register(&hub, "wes", "person").await; // watching, on a desk (in use) and a phone (not)

    // ann's last call before she is killed: a poll that answers, well after
    // her registration (v2.0.3: the watcher must see this time as last_seen)
    tokio::time::sleep(Duration::from_millis(1200)).await;
    let ann_called = chrono::Utc::now();
    let r = Call::new("POST", "/api/poll?wait=0").auth(pair(&ann.0, &ann.1)).send(&hub).await;
    assert_eq!(r.code(), 200, "{}", r.text());
    let ann_poll = park_poll(&hub, &ann);
    let dan_poll = park_poll(&hub, &dan);
    let cat_first = sync_now(&hub, &cat, "cat-laptop", None).await;
    let cat_sync = park_sync(&hub, &cat, "cat-laptop", &cat_first["cursor"]);
    tokio::time::sleep(Duration::from_millis(300)).await;
    let desk_first = sync_now(&hub, &wes, "wes-desk", None).await;
    let phone_first = sync_now(&hub, &wes, "wes-phone", None).await;
    in_use(&hub, &wes, "wes-desk").await;
    let desk = park_sync(&hub, &wes, "wes-desk", &desk_first["cursor"]);
    let phone = park_sync(&hub, &wes, "wes-phone", &phone_first["cursor"]);
    // a tablet in use when it parks, then put down (v2.0.3: it must stop
    // being woken for presence at once, not when its park ends)
    let tablet_first = sync_now(&hub, &wes, "wes-tablet", None).await;
    in_use(&hub, &wes, "wes-tablet").await;
    let tablet = park_sync(&hub, &wes, "wes-tablet", &tablet_first["cursor"]);
    tokio::time::sleep(Duration::from_millis(300)).await;
    let r = Call::new("POST", "/api/devices/active").auth(pair(&wes.0, &wes.1)).json(json!({ "device_id": "wes-tablet", "active": false })).send(&hub).await;
    assert_eq!(r.code(), 200, "{}", r.text());
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!tablet.is_finished(), "putting the tablet down made its sync answer");
    for who in [&ann, &dan, &cat] {
        assert!(roster_online(&hub, &wes, &who.0).await, "{} online while parked", who.0);
    }

    // ann is killed and cat's Hubchat quits (the server drops their calls
    // with the connection); dan's poller restarts at once
    let t0 = Instant::now();
    ann_poll.abort();
    cat_sync.abort();
    dan_poll.abort();
    let _dan_again = park_poll(&hub, &dan);

    tokio::time::sleep(Duration::from_secs(8)).await;
    assert!(!desk.is_finished(), "the desk sync answered within the grace");
    for who in [&ann, &dan, &cat] {
        assert!(roster_online(&hub, &wes, &who.0).await, "{} online within the grace", who.0);
    }
    println!("  ok  within the grace (8 s) every client still shows online");

    let (desk_answer, at) = desk.await.unwrap();
    let after = at - t0;
    assert!(after >= HANG_UP_GRACE && after < Duration::from_millis(12_500), "the desk heard after {after:?}");
    assert!(!lists(&desk_answer, &ann.0) && !lists(&desk_answer, &cat.0), "{desk_answer}");
    assert!(lists(&desk_answer, &dan.0) && lists(&desk_answer, &wes.0), "{desk_answer}");
    assert!(!roster_online(&hub, &wes, &ann.0).await && !roster_online(&hub, &wes, &cat.0).await);
    assert!(roster_online(&hub, &wes, &dan.0).await);
    println!("  ok  a killed poll and a quit sync go offline after the grace; the desk in use hears it {:.1} s after (dan, re-parked at once, never dropped)", after.as_secs_f64());

    // v2.0.3: the same answer carries ann's roster entry, last_seen her last
    // call (not her registration, 1.2 s earlier)
    let entry = desk_answer["roster"].as_array().unwrap().iter().find(|e| e["slug"] == json!(ann.0)).cloned();
    let entry = entry.unwrap_or_else(|| panic!("no roster entry for ann in {desk_answer}"));
    let seen = chrono::DateTime::parse_from_rfc3339(entry["last_seen"].as_str().unwrap()).unwrap().with_timezone(&chrono::Utc);
    assert!((seen - ann_called).num_milliseconds().abs() < 1000, "ann's last_seen {seen} is not her last call {ann_called}");
    println!("  ok  that answer carries ann's roster entry with last_seen her last call ({} ms after it), not her registration",
        (seen - ann_called).num_milliseconds());

    assert!(!tablet.is_finished(), "the tablet, put down mid-park, was woken for presence");
    tablet.abort();
    println!("  ok  a device put down mid-park is not woken for presence (it re-read that it is not in use)");

    assert!(!phone.is_finished(), "the phone (not in use) was woken");
    let t1 = Instant::now();
    in_use(&hub, &wes, "wes-phone").await;
    let (phone_answer, at) = phone.await.unwrap();
    assert!(at - t1 < Duration::from_millis(1500), "the phone heard {:?} after coming into use", at - t1);
    assert!(!lists(&phone_answer, &ann.0) && lists(&phone_answer, &dan.0), "{phone_answer}");
    println!("  ok  a device not in use is not woken for it; coming into use, it hears at once ({:.2} s)", (at - t1).as_secs_f64());

    // ann comes back: a poll that answers
    let desk_again = park_sync(&hub, &wes, "wes-desk", &desk_answer["cursor"]);
    tokio::time::sleep(Duration::from_millis(300)).await;
    let t2 = Instant::now();
    let r = Call::new("POST", "/api/poll?wait=0").auth(pair(&ann.0, &ann.1)).send(&hub).await;
    assert_eq!(r.code(), 200, "{}", r.text());
    let (back, at) = desk_again.await.unwrap();
    assert!(at - t2 < Duration::from_millis(2500), "the desk heard {:?} after ann came back", at - t2);
    assert!(lists(&back, &ann.0), "{back}");
    println!("  ok  back online: the desk hears it {:.2} s after ann's poll", (at - t2).as_secs_f64());

    // that poll answered (no hang-up): v1's window holds, past the grace
    tokio::time::sleep(HANG_UP_GRACE + Duration::from_secs(2)).await;
    assert!(roster_online(&hub, &wes, &ann.0).await, "an answered poll keeps v1's 90 s window");
    println!("  ok  an answered poll keeps v1's 90 s window (still online {} s later)", (HANG_UP_GRACE + Duration::from_secs(2)).as_secs());

    let _ = std::fs::remove_dir_all(&data);
    drop(pg);
}
