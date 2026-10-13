//! Typing (feature `typing`, 2026-10-13): heartbeats. A sender beats about
//! every half second while it writes to an address. A sync that asks is
//! told each heartbeat once, and a parked sync of a device in use answers
//! with it at once, from memory. There is no stop: when the heartbeats stop,
//! nobody is woken and nothing stale is relayed. Devices not in use and
//! syncs that do not ask are never woken for it. Nothing is stored.
//!
//!     HUB_TEST_PG=<folder> cargo test --test typing -- --nocapture

mod support;

use std::sync::Arc;
use std::time::{Duration, Instant};

use mailhub::api::sync::presence_loop;
use mailhub::Hub;
use serde_json::{json, Value};
use support::*;
use tokio::task::JoinHandle;

type Id = (String, String);

async fn register(hub: &Arc<Hub>, name: &str, kind: &str) -> Id {
    let id = (format!("{name}.typing.{}", token_hex3()), token_hex16());
    let r = Call::new("POST", "/api/register")
        .auth(pair(&id.0, &id.1))
        .json(json!({ "slug": id.0, "org_name": name, "username": "typing", "kind": kind }))
        .send(hub)
        .await;
    assert_eq!(r.code(), 200, "{}", r.text());
    id
}

async fn typing(hub: &Arc<Hub>, who: &Id, body: Value) -> Resp {
    Call::new("POST", "/api/typing").auth(pair(&who.0, &who.1)).json(body).send(hub).await
}

/// One heartbeat from `who` to `to`: the recipients it counted for.
async fn beat(hub: &Arc<Hub>, who: &Id, to: &[&str]) -> u64 {
    let r = typing(hub, who, json!({ "to": to })).await;
    assert_eq!(r.code(), 200, "{}", r.text());
    let a = r.json();
    assert_eq!(a["ok"], json!(true), "{a}");
    a["counted"].as_u64().unwrap_or_else(|| panic!("no count in {a}"))
}

async fn send(hub: &Arc<Hub>, from: &Id, to: &str, id: &str) {
    let r = Call::new("POST", "/api/send")
        .auth(pair(&from.0, &from.1))
        .json(json!({ "id": id, "to": to, "body": format!("body of {id}"), "sent_at": "2026-10-13T22:40:00.000Z" }))
        .send(hub)
        .await;
    assert_eq!(r.code(), 200, "{}", r.text());
}

async fn in_use(hub: &Arc<Hub>, who: &Id, device: &str) {
    let r = Call::new("POST", "/api/devices/active").auth(pair(&who.0, &who.1)).json(json!({ "device_id": device, "active": true })).send(hub).await;
    assert_eq!(r.code(), 200, "{}", r.text());
}

/// A sync that answers at once (following `more`), asking for typing when
/// `asks` is given (true, or the last mark).
async fn sync_now(hub: &Arc<Hub>, who: &Id, device: &str, cursor: Option<&Value>, asks: Option<Value>) -> Value {
    let mut body = json!({ "device_id": device, "wait": 0 });
    if let Some(c) = cursor {
        body["cursor"] = c.clone();
    }
    if let Some(a) = asks {
        body["typing"] = a;
    }
    loop {
        let r = Call::new("POST", "/api/sync").auth(pair(&who.0, &who.1)).json(body.clone()).send(hub).await;
        assert_eq!(r.code(), 200, "{}", r.text());
        let a = r.json();
        if a["more"] != json!(true) {
            return a;
        }
        body["cursor"] = a["cursor"].clone();
        if let Some(m) = a.get("typing_seq") {
            body["typing"] = m.clone();
        }
    }
}

/// A parked sync (as Hubchat parks one), sending back the last answer's
/// cursor and typing mark: its answer and when it came.
fn park(hub: &Arc<Hub>, who: &Id, device: &str, last: &Value, wait: u64) -> JoinHandle<(Value, Instant)> {
    let (h, w, d) = (hub.clone(), who.clone(), device.to_string());
    let mut body = json!({ "device_id": d, "wait": wait, "cursor": last["cursor"] });
    if let Some(m) = last.get("typing_seq") {
        body["typing"] = m.clone();
    }
    tokio::spawn(async move {
        let r = Call::new("POST", "/api/sync").auth(pair(&w.0, &w.1)).json(body).send(&h).await;
        assert_eq!(r.code(), 200, "{}", r.text());
        (r.json(), Instant::now())
    })
}

/// The answer of a parked sync that must come within `within` of `since`.
async fn answered(p: JoinHandle<(Value, Instant)>, since: Instant, within: Duration, what: &str) -> Value {
    let (a, at) = tokio::time::timeout(within + Duration::from_secs(2), p).await.unwrap_or_else(|_| panic!("{what}: no answer")).unwrap();
    let took = at.duration_since(since);
    assert!(took <= within, "{what}: answered only after {took:?}");
    println!("  ok  {what} ({took:?})");
    a
}

/// A parked sync that must still be parked after `quiet`.
async fn still_parked(p: &JoinHandle<(Value, Instant)>, quiet: Duration, what: &str) {
    tokio::time::sleep(quiet).await;
    assert!(!p.is_finished(), "{what}: it answered");
    println!("  ok  {what} (still parked after {quiet:?})");
}

fn senders(a: &Value) -> Vec<String> {
    a["typing"].as_array().unwrap_or_else(|| panic!("no typing in {a}")).iter().map(|r| r["from"].as_str().unwrap().to_string()).collect()
}

async fn test_hub(name: &str) -> (TestPg, Arc<Hub>, String) {
    let pg = TestPg::start();
    let url = pg.fresh_db(name).await;
    let data = std::env::temp_dir().join(format!("orgtree-hub-{name}-{}", token_hex16()));
    std::fs::create_dir_all(&data).unwrap();
    let hub = hub(&url, &data, &[]).await;
    tokio::spawn(presence_loop(hub.clone()));
    (pg, hub, url)
}

#[tokio::test]
async fn a_heartbeat_reaches_the_device_in_use_at_once_and_ends_by_itself() {
    let (_pg, hub, _url) = test_hub("hubtyping").await;
    let r = Call::new("GET", "/healthz").send(&hub).await;
    assert!(r.json()["features"].as_array().unwrap().contains(&json!("typing")), "{}", r.text());

    let org = register(&hub, "org", "org").await; // Orgtree's engine, for its agents
    let cat = register(&hub, "cat", "person").await; // a person on Hubchat, writing too
    let ann = register(&hub, "ann", "person").await; // Hubchat on a desk (in use) and a phone
    let bob = register(&hub, "bob", "person").await; // an older Hubchat: never asks
    in_use(&hub, &ann, "ann-desk").await;

    let desk = sync_now(&hub, &ann, "ann-desk", None, Some(json!(true))).await;
    assert_eq!(senders(&desk), Vec::<String>::new());
    assert!(desk["typing_seq"].as_u64().is_some(), "{desk}");
    let phone = sync_now(&hub, &ann, "ann-phone", None, Some(json!(true))).await;
    let older = sync_now(&hub, &bob, "bob-desk", None, None).await;
    assert!(older.get("typing").is_none() && older.get("typing_seq").is_none(), "a sync that does not ask is told nothing: {older}");
    println!("  ok  only a sync that asks is told (and /healthz lists typing)");

    // a heartbeat: the desk answers at once, from memory; the phone and the
    // older client do not
    let desk_wait = park(&hub, &ann, "ann-desk", &desk, 25);
    let phone_wait = park(&hub, &ann, "ann-phone", &phone, 4);
    let older_wait = park(&hub, &bob, "bob-desk", &older, 3);
    tokio::time::sleep(Duration::from_millis(300)).await;
    let t = Instant::now();
    assert_eq!(beat(&hub, &org, &[&ann.0, &bob.0]).await, 2);
    let a = answered(desk_wait, t, Duration::from_millis(500), "a heartbeat wakes the device in use at once").await;
    assert_eq!(senders(&a), [org.0.clone()]);
    assert_eq!(a["typing_only"], json!(true), "{a}");
    assert_eq!((&a["cursor"], &a["changes"]), (&desk["cursor"], &json!([])), "nothing else moved: {a}");
    assert!(a.get("active").is_none(), "{a}");
    assert!(a["typing_seq"].as_u64() > desk["typing_seq"].as_u64(), "{a}");
    let desk = a;
    let older = answered(older_wait, t, Duration::from_secs(3), "an older client answers only at its deadline").await;
    assert!(Instant::now().duration_since(t) >= Duration::from_millis(2400), "the older client was woken early");
    assert!(older.get("typing").is_none(), "and is told nothing: {older}");
    let phone = answered(phone_wait, t, Duration::from_secs(4), "a device not in use only answers at its deadline").await;
    assert!(Instant::now().duration_since(t) >= Duration::from_secs(3), "the phone was woken early");
    assert_eq!(senders(&phone), Vec::<String>::new(), "by then the heartbeat is stale, and nobody is told of it");
    assert!(phone.get("typing_only").is_none(), "{phone}");

    // each heartbeat is told once: the mark sent back parks the sync until
    // the next one, and two closer than 300 ms count once
    let desk_wait = park(&hub, &ann, "ann-desk", &desk, 25);
    still_parked(&desk_wait, Duration::from_millis(400), "a heartbeat already told wakes nobody").await;
    let t = Instant::now();
    assert_eq!(beat(&hub, &org, &[&ann.0]).await, 1);
    assert_eq!(beat(&hub, &org, &[&ann.0]).await, 0, "a second one within 300 ms is absorbed");
    let desk = answered(desk_wait, t, Duration::from_millis(500), "the next heartbeat is relayed at once").await;
    assert_eq!(senders(&desk), [org.0.clone()]);

    // no stop: when the heartbeats stop, nothing wakes the desk, and at its
    // deadline it is told nothing stale
    let desk_wait = park(&hub, &ann, "ann-desk", &desk, 2);
    still_parked(&desk_wait, Duration::from_millis(1500), "when the heartbeats stop, nobody is woken").await;
    let desk = answered(desk_wait, t, Duration::from_millis(2600), "the deadline answers").await;
    assert_eq!(senders(&desk), Vec::<String>::new(), "nothing stale is relayed");
    assert!(desk.get("typing_only").is_none(), "{desk}");

    // a heartbeat while the desk was between syncs answers its next one at once
    assert_eq!(beat(&hub, &org, &[&ann.0]).await, 1);
    let t = Instant::now();
    let desk_wait = park(&hub, &ann, "ann-desk", &desk, 25);
    let desk = answered(desk_wait, t, Duration::from_millis(300), "a heartbeat between two syncs answers the next at once").await;
    assert_eq!((senders(&desk), &desk["typing_only"]), (vec![org.0.clone()], &json!(true)));

    // two writers: each is relayed, once
    let desk_wait = park(&hub, &ann, "ann-desk", &desk, 25);
    tokio::time::sleep(Duration::from_millis(400)).await;
    let t = Instant::now();
    assert_eq!(beat(&hub, &org, &[&ann.0]).await, 1);
    assert_eq!(beat(&hub, &cat, &[&ann.0]).await, 1);
    let a = answered(desk_wait, t, Duration::from_millis(500), "two writers wake it").await;
    let mut seen = senders(&a);
    let desk = if seen.len() < 2 {
        // the second came after the first answer: its next sync is told
        let b = answered(park(&hub, &ann, "ann-desk", &a, 25), t, Duration::from_millis(500), "and the other comes with the next").await;
        seen.extend(senders(&b));
        b
    } else {
        a
    };
    seen.sort();
    let mut both = [org.0.clone(), cat.0.clone()];
    both.sort();
    assert_eq!(seen, both);

    // an answer with mail in it carries the heartbeats too (and is a whole answer)
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(beat(&hub, &org, &[&ann.0]).await, 1);
    send(&hub, &org, &ann.0, &format!("m-{}", token_hex3())).await;
    let a = sync_now(&hub, &ann, "ann-desk", Some(&desk["cursor"]), Some(desk["typing_seq"].clone())).await;
    assert_eq!(senders(&a), [org.0.clone()]);
    assert!(a.get("typing_only").is_none() && a["changes"].as_array().is_some_and(|c| c.len() == 1), "{a}");
    println!("  ok  a whole answer carries the heartbeats it relays");
}

#[tokio::test]
async fn typing_is_bounded_and_never_stored() {
    let (_pg, hub, url) = test_hub("hubtypinglimits").await;
    let org = register(&hub, "org", "org").await;
    let ann = register(&hub, "ann", "person").await;
    let bob = register(&hub, "bob", "person").await;

    // refusals
    let r = Call::new("POST", "/api/typing").json(json!({ "to": [&ann.0] })).send(&hub).await;
    assert_eq!(r.code(), 401);
    let eleven: Vec<String> = (0..11).map(|i| format!("a{i}")).collect();
    for (body, detail) in [
        (json!({ "to": &ann.0 }), "to must be"),
        (json!({ "to": [] }), "to must be"),
        (json!({ "to": [""] }), "to must be"),
        (json!({ "to": [5] }), "to must be"),
        (json!({ "to": eleven }), "to must be"),
        (json!({}), "to must be"),
        (json!({ "to": [&ann.0], "text": "hello" }), "carries only slug and to, not text"),
        (json!({ "to": [&ann.0], "id": "x", "state": "typing" }), "carries only slug and to, not id"),
    ] {
        let r = typing(&hub, &org, body.clone()).await;
        assert_eq!(r.code(), 422, "{body}: {}", r.text());
        assert!(r.text().contains(detail), "{body}: {}", r.text());
    }
    let big = json!({ "to": [&ann.0], "pad": "p".repeat(5000) });
    assert_eq!(typing(&hub, &org, big).await.code(), 413);
    for bad in [json!("yes"), json!("12"), json!(-1), json!(1.5), json!([1])] {
        let r = Call::new("POST", "/api/sync").auth(pair(&ann.0, &ann.1)).json(json!({ "device_id": "d", "wait": 0, "typing": bad })).send(&hub).await;
        assert_eq!(r.code(), 422, "{}", r.text());
        assert!(r.text().contains("typing must be true or the typing_seq of your last answer"), "{}", r.text());
    }
    println!("  ok  malformed calls are refused with what is wrong; nothing but addresses rides along");

    // the sender itself is skipped; any address counts (nothing is looked up)
    assert_eq!(beat(&hub, &org, &[&ann.0, &org.0]).await, 1);
    assert_eq!(beat(&hub, &org, &["nobody.typing.000"]).await, 1);
    println!("  ok  the sender itself is skipped; recipients are not looked up");

    // nothing is stored: every table's rows, and the sender's last_seen, unchanged
    let db = sql_client(&url).await;
    async fn counts(db: &tokio_postgres::Client) -> Vec<(String, i64)> {
        let tables = db.query("SELECT table_name::text FROM information_schema.tables WHERE table_schema = current_schema() AND table_type = 'BASE TABLE' ORDER BY 1", &[]).await.unwrap();
        let mut out = Vec::new();
        for t in tables {
            let name: String = t.get(0);
            let n: i64 = db.query_one(&format!("SELECT count(*) FROM \"{name}\""), &[]).await.unwrap().get(0);
            out.push((name, n));
        }
        out
    }
    let seen = "SELECT last_seen FROM identities WHERE slug = $1";
    let before = (counts(&db).await, db.query_one(seen, &[&org.0]).await.unwrap().get::<_, Option<chrono::DateTime<chrono::Utc>>>(0));
    for _ in 0..10 {
        beat(&hub, &org, &[&ann.0, &bob.0]).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let after = (counts(&db).await, db.query_one(seen, &[&org.0]).await.unwrap().get::<_, Option<chrono::DateTime<chrono::Utc>>>(0));
    assert!(before.0.len() > 5, "{:?}", before.0);
    assert_eq!(before, after);
    println!("  ok  no table and no last_seen changed over 10 heartbeats ({} tables)", after.0.len());

    // a sender beats to at most 32 addresses at once; older heartbeats free room
    tokio::time::sleep(Duration::from_millis(2100)).await;
    let many: Vec<String> = (0..32).map(|i| format!("r{i}.typing.000")).collect();
    for c in many.chunks(8) {
        let c: Vec<&str> = c.iter().map(String::as_str).collect();
        assert_eq!(beat(&hub, &org, &c).await, 8);
    }
    let r = typing(&hub, &org, json!({ "to": ["r32.typing.000"] })).await;
    assert_eq!(r.code(), 429, "{}", r.text());
    assert_eq!(typing(&hub, &org, json!({ "to": [&many[0]] })).await.code(), 200, "the same addresses always fit");
    assert_eq!(typing(&hub, &ann, json!({ "to": ["r32.typing.000"] })).await.code(), 200, "another sender is not limited by it");
    tokio::time::sleep(Duration::from_millis(2100)).await;
    assert_eq!(typing(&hub, &org, json!({ "to": ["r32.typing.000"] })).await.code(), 200, "heartbeats past keeping free room");
    println!("  ok  32 recipients per sender at once (429 beyond); the same ones fit; old heartbeats free room");
}
