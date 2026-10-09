//! Lazy history (v2.0.1): a new device starts from now instead of
//! downloading every message so far, and fetches older mail page by page,
//! from a time as well as from a cursor.
//!
//!     HUB_TEST_PG=<folder> cargo test --test lazy_history -- --nocapture

mod support;

use std::sync::Arc;
use std::time::Duration;

use mailhub::Hub;
use serde_json::{json, Value};
use support::*;

type Id = (String, String);

async fn register(hub: &Arc<Hub>, name: &str, kind: &str) -> Id {
    let id = (format!("{name}.lazy.{}", token_hex3()), token_hex16());
    let r = Call::new("POST", "/api/register")
        .auth(pair(&id.0, &id.1))
        .json(json!({ "slug": id.0, "org_name": name, "username": "lazy", "kind": kind }))
        .send(hub)
        .await;
    assert_eq!(r.code(), 200, "{}", r.text());
    id
}

/// Each message a millisecond apart at least, so times order them.
async fn send(hub: &Arc<Hub>, from: &Id, to: &str, id: &str) {
    tokio::time::sleep(Duration::from_millis(3)).await;
    let r = Call::new("POST", "/api/send")
        .auth(pair(&from.0, &from.1))
        .json(json!({ "id": id, "to": to, "body": format!("body of {id}") }))
        .send(hub)
        .await;
    assert_eq!(r.code(), 200, "{}", r.text());
}

async fn sync_call(hub: &Arc<Hub>, id: &Id, body: Value) -> Resp {
    Call::new("POST", "/api/sync").auth(pair(&id.0, &id.1)).json(body).send(hub).await
}

async fn sync(hub: &Arc<Hub>, id: &Id, body: Value) -> Value {
    let r = sync_call(hub, id, body).await;
    assert_eq!(r.code(), 200, "{}", r.text());
    r.json()
}

async fn history(hub: &Arc<Hub>, who: &Id, query: &str) -> Value {
    let path = format!("/api/history?{query}");
    let r = Call::new("GET", &path).auth(pair(&who.0, &who.1)).send(hub).await;
    assert_eq!(r.code(), 200, "{}", r.text());
    r.json()
}

fn ids(v: &Value, key: &str) -> Vec<String> {
    v[key]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().or_else(|| m["message"]["id"].as_str()).unwrap().to_string())
        .collect()
}

fn ms(iso: &Value) -> i64 {
    chrono::DateTime::parse_from_rfc3339(iso.as_str().unwrap()).unwrap().timestamp_millis()
}

/// The hub's clock (`now`): unix milliseconds, this machine's time.
fn assert_close_to_now(now: &Value) {
    let hub = now.as_i64().unwrap_or_else(|| panic!("now is not whole milliseconds: {now}"));
    let here = chrono::Utc::now().timestamp_millis();
    assert!((here - hub).abs() < 5_000, "now {hub} is not this clock ({here})");
}

#[tokio::test]
async fn a_new_device_starts_from_now() {
    let pg = TestPg::start();
    let url = pg.fresh_db("hublazy").await;
    let data = std::env::temp_dir().join(format!("orgtree-hub-lazy-{}", token_hex16()));
    std::fs::create_dir_all(&data).unwrap();
    let hub = hub(&url, &data, &[]).await;
    let sql = sql_client(&url).await;
    let state = |id: &'static str| {
        let sql = &sql;
        async move { sql.query_one("SELECT state FROM messages WHERE id = $1", &[&id]).await.unwrap().get::<_, String>(0) }
    };

    let ann = register(&hub, "ann", "person").await;
    let bob = register(&hub, "bob", "org").await;
    let carl = register(&hub, "carl", "org").await;
    for id in ["m1", "m2", "m3"] {
        send(&hub, &bob, &ann.0, id).await;
    }
    let health = Call::new("GET", "/healthz").send(&hub).await.json();
    assert!(health["features"].as_array().unwrap().contains(&json!("lazy_history")), "{health}");
    assert_close_to_now(&health["now"]);
    println!("  ok  /healthz names the feature (lazy_history) and gives the hub's clock (now, unix ms)");

    // ------------------------------------------------------ history by time
    let all = history(&hub, &ann, &format!("with={}", bob.0)).await;
    assert_eq!(ids(&all, "messages"), ["m3", "m2", "m1"]);
    let t2 = ms(&all["messages"][1]["received_at"]);
    let older = history(&hub, &ann, &format!("with={}&before={t2}", bob.0)).await;
    assert_eq!((ids(&older, "messages"), older["before"].clone()), (vec!["m1".to_string()], Value::Null), "strictly before the time");
    let at = history(&hub, &ann, &format!("with={}&before={}", bob.0, t2 + 1)).await;
    assert_eq!(ids(&at, "messages"), ["m2", "m1"], "ms + 1 takes in that millisecond");
    let page = history(&hub, &ann, &format!("with={}&before={}&limit=1", bob.0, t2 + 1)).await;
    assert_eq!(ids(&page, "messages"), ["m2"]);
    let next = history(&hub, &ann, &format!("with={}&before={}&limit=1", bob.0, page["before"].as_str().unwrap())).await;
    assert_eq!((ids(&next, "messages"), next["before"].clone()), (vec!["m1".to_string()], Value::Null), "the answer's cursor goes on from a time");
    // two messages in one millisecond: a time takes both or neither
    sql.execute("UPDATE messages SET received_at = (SELECT received_at FROM messages WHERE id = 'm2') WHERE id = 'm3'", &[]).await.unwrap();
    let tie = history(&hub, &ann, &format!("with={}&before={t2}", bob.0)).await;
    assert_eq!(ids(&tie, "messages"), ["m1"]);
    let tie = history(&hub, &ann, &format!("with={}&before={}", bob.0, t2 + 1)).await;
    assert_eq!(ids(&tie, "messages"), ["m3", "m2", "m1"]);
    for bad in ["12x", "1.5", "-5", "99999999999999999999"] {
        let path = format!("/api/history?with={}&before={bad}", bob.0);
        let r = Call::new("GET", &path).auth(pair(&ann.0, &ann.1)).send(&hub).await;
        assert_eq!(
            (r.code(), r.json()["detail"].clone()),
            (422, json!("before is not a history cursor from this hub")),
            "{bad}"
        );
    }
    println!("  ok  history: before=<unix ms> pages from a time (strictly before it), its cursor goes on, ties are kept together");

    // ------------------------------------------------- a device starting from now
    send(&hub, &bob, &ann.0, "q1").await;
    let first = sync(&hub, &ann, json!({ "device_id": "ann-tablet", "start": "now", "wait": 0 })).await;
    assert_eq!((first["changes"].clone(), first["start"].clone(), first["more"].clone()), (json!([]), json!("now"), json!(false)), "{first}");
    let roster: Vec<&str> = first["roster"].as_array().unwrap().iter().map(|r| r["slug"].as_str().unwrap()).collect();
    for who in [&ann.0, &bob.0, &carl.0] {
        assert!(roster.contains(&who.as_str()), "the whole roster: {roster:?}");
    }
    assert!(first["online"].is_array(), "{first}");
    assert_close_to_now(&first["now"]);
    let head: i64 = sql.query_one("SELECT seq FROM mailbox_heads WHERE slug = $1", &[&ann.0]).await.unwrap().get(0);
    let mut tablet = first["cursor"].clone();
    assert!(tablet.as_str().unwrap().starts_with(&format!("{head}-")) && tablet.as_str().unwrap().ends_with(&format!("-{head}")), "{tablet}");
    println!("  ok  start=now: no mail so far, the whole roster, who is online, and \"start\": \"now\" said back");

    let again = sync(&hub, &ann, json!({ "device_id": "ann-tablet", "cursor": tablet, "wait": 0 })).await;
    assert_eq!(again["changes"], json!([]));
    assert!(again.get("start").is_none(), "{again}");
    assert_close_to_now(&again["now"]);
    tablet = again["cursor"].clone();
    for id in ["m1", "m2", "m3", "q1"] {
        assert_eq!(state(id).await, "queued", "{id}: custody of mail the device was never sent");
    }
    println!("  ok  custody: mail still in v1's queue from before the start stays queued");

    send(&hub, &carl, &ann.0, "n1").await;
    let a = sync(&hub, &ann, json!({ "device_id": "ann-tablet", "cursor": tablet, "wait": 0 })).await;
    assert_eq!(ids(&a, "changes"), ["n1"]);
    tablet = a["cursor"].clone();
    let _ = sync(&hub, &ann, json!({ "device_id": "ann-tablet", "cursor": tablet, "wait": 0 })).await;
    assert_eq!((state("n1").await, state("q1").await), ("fetched".to_string(), "queued".to_string()));
    // a parked sync of a from-now device wakes for new mail
    let (h, i, c) = (hub.clone(), ann.clone(), tablet.clone());
    let parked = tokio::spawn(async move { sync(&h, &i, json!({ "device_id": "ann-tablet", "cursor": c, "wait": 10 })).await });
    tokio::time::sleep(Duration::from_millis(200)).await;
    send(&hub, &carl, &ann.0, "n2").await;
    let woke = tokio::time::timeout(Duration::from_secs(5), parked).await.expect("the parked sync did not wake").unwrap();
    assert_eq!(ids(&woke, "changes"), ["n2"]);
    tablet = woke["cursor"].clone();
    println!("  ok  mail after the start arrives (a parked sync wakes for it) and is handed over as usual");

    // a change to an older message comes as a change (here: read on another device)
    let r = Call::new("POST", "/api/receipts")
        .auth(pair(&ann.0, &ann.1))
        .json(json!({ "receipts": [{ "id": "m1", "state": "read", "at": "2026-10-09T09:00:00.000Z" }] }))
        .send(&hub)
        .await;
    assert_eq!(r.code(), 200, "{}", r.text());
    let a = sync(&hub, &ann, json!({ "device_id": "ann-tablet", "cursor": tablet, "wait": 0 })).await;
    assert_eq!(ids(&a, "changes"), ["m1"]);
    assert_eq!(a["changes"][0]["message"]["read_at"], json!("2026-10-09T09:00:00.000Z"));
    tablet = a["cursor"].clone();
    println!("  ok  a change to an older message (a read receipt) comes to the device as a change");

    // the v1 queue and an ordinary device are untouched
    let v1 = Call::new("POST", "/api/poll?wait=0").auth(pair(&ann.0, &ann.1)).send(&hub).await.json();
    let polled: Vec<&str> = v1["messages"].as_array().unwrap().iter().map(|m| m["id"].as_str().unwrap()).collect();
    assert!(polled.contains(&"q1") && polled.contains(&"m2"), "a v1 poll no longer gets the queued mail: {v1}");
    let phone = sync(&hub, &ann, json!({ "device_id": "ann-phone", "wait": 0 })).await;
    for id in ["m1", "m2", "m3", "q1", "n1", "n2"] {
        assert!(ids(&phone, "changes").contains(&id.to_string()), "{id} missing from a full sync: {phone}");
    }
    println!("  ok  v1 polls still get the queued mail; a device without start=now still gets everything");

    // --------------------------------------------------------------- refusals
    let r = sync_call(&hub, &ann, json!({ "device_id": "ann-tablet", "cursor": tablet, "start": "now", "wait": 0 })).await;
    assert_eq!((r.code(), r.json()["detail"].clone()), (422, json!("start is only for a device's first sync (send no cursor)")));
    for bad in [json!("later"), json!(true), json!("")] {
        let r = sync_call(&hub, &ann, json!({ "device_id": "ann-new", "start": bad, "wait": 0 })).await;
        assert_eq!((r.code(), r.json()["detail"].clone()), (422, json!("start must be \"now\"")), "{bad}");
    }
    let r = sync_call(&hub, &ann, json!({ "device_id": "ann-tablet", "cursor": format!("{}-0", tablet.as_str().unwrap()), "wait": 0 })).await;
    assert_eq!(r.code(), 422, "a five-part cursor");
    println!("  ok  refusals: start with a cursor, start other than \"now\", a malformed cursor");

    // ------------------------------------- a from-now cursor from another history
    let head: i64 = sql.query_one("SELECT seq FROM mailbox_heads WHERE slug = $1", &[&ann.0]).await.unwrap().get(0);
    let foreign = format!("{}-0-0000000000000000-{}", head + 100, head + 100);
    let a = sync(&hub, &ann, json!({ "device_id": "ann-tablet", "cursor": foreign, "wait": 0 })).await;
    assert_eq!((a["reset"].clone(), a["changes"].clone()), (json!(true), json!([])), "a reset from-now device started from the beginning: {a}");
    assert!(a["cursor"].as_str().unwrap().ends_with(&format!("-{head}")), "{a}");
    println!("  ok  a from-now cursor this hub did not write starts the device over from now, saying so");

    let _ = std::fs::remove_dir_all(&data);
    drop(pg);
}
