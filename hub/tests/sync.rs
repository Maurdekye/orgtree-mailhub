//! Phase 2, slice 2: every device gets everything (G1) — `POST /api/sync`
//! and `GET /api/devices`.
//!
//!     HUB_TEST_PG=<folder> cargo test --test sync -- --nocapture

mod support;

use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use mailhub::Hub;
use serde_json::{json, Value};
use support::*;

type Id = (String, String);

async fn register(hub: &Arc<Hub>, name: &str, kind: &str) -> Id {
    let id = (format!("{name}.sync.{}", token_hex3()), token_hex16());
    let r = Call::new("POST", "/api/register")
        .auth(pair(&id.0, &id.1))
        .json(json!({ "slug": id.0, "org_name": name, "username": "sync", "kind": kind }))
        .send(hub)
        .await;
    assert_eq!(r.code(), 200, "{}", r.text());
    id
}

async fn sync_call(hub: &Arc<Hub>, id: &Id, body: Value) -> Resp {
    Call::new("POST", "/api/sync").auth(pair(&id.0, &id.1)).json(body).send(hub).await
}

/// One sync answer (200 expected).
async fn sync(hub: &Arc<Hub>, id: &Id, device: &str, cursor: &Value, wait: f64) -> Value {
    let r = sync_call(hub, id, json!({ "device_id": device, "cursor": cursor, "wait": wait })).await;
    assert_eq!(r.code(), 200, "{}", r.text());
    r.json()
}

/// Sync until nothing more is waiting: every change, and the last answer.
async fn drain(hub: &Arc<Hub>, id: &Id, device: &str, cursor: &Value) -> (Vec<Value>, Value) {
    let mut cursor = cursor.clone();
    let mut changes = Vec::new();
    loop {
        let a = sync(hub, id, device, &cursor, 0.0).await;
        changes.extend(a["changes"].as_array().unwrap().iter().cloned());
        cursor = a["cursor"].clone();
        if a["more"] != json!(true) {
            return (changes, a);
        }
    }
}

fn ids(changes: &[Value]) -> Vec<String> {
    changes.iter().map(|c| c["message"]["id"].as_str().unwrap().to_string()).collect()
}

fn find<'a>(changes: &'a [Value], id: &str) -> &'a Value {
    &changes.iter().find(|c| c["message"]["id"] == json!(id)).unwrap_or_else(|| panic!("{id} not in {changes:?}"))["message"]
}

async fn send(hub: &Arc<Hub>, from: &Id, to: &str, id: &str) -> Resp {
    let r = Call::new("POST", "/api/send")
        .auth(pair(&from.0, &from.1))
        .json(json!({ "id": id, "to": to, "body": format!("body of {id}"), "sent_at": "2026-10-08T12:00:00.000Z" }))
        .send(hub)
        .await;
    assert_eq!(r.code(), 200, "{}", r.text());
    r
}

async fn receipt(hub: &Arc<Hub>, who: &Id, id: &str, state: &str, at: &str) -> Value {
    let r = Call::new("POST", "/api/receipts")
        .auth(pair(&who.0, &who.1))
        .json(json!({ "receipts": [{ "id": id, "state": state, "at": at }] }))
        .send(hub)
        .await;
    assert_eq!(r.code(), 200, "{}", r.text());
    r.json()
}

/// A sync parked in the background; `.await` it after the event.
fn parked(hub: &Arc<Hub>, id: &Id, device: &str, cursor: &Value, wait: f64) -> tokio::task::JoinHandle<(Value, Instant)> {
    let (hub, id, device, cursor) = (hub.clone(), id.clone(), device.to_string(), cursor.clone());
    tokio::spawn(async move {
        let a = sync(&hub, &id, &device, &cursor, wait).await;
        (a, Instant::now())
    })
}

#[tokio::test]
async fn every_device_gets_everything() {
    let pg = TestPg::start();
    let url = pg.fresh_db("hubsync").await;
    let data = std::env::temp_dir().join(format!("orgtree-hub-sync-{}", token_hex16()));
    std::fs::create_dir_all(&data).unwrap();
    let hub = hub(&url, &data, &[]).await;
    let sql = sql_client(&url).await;

    let ann = register(&hub, "ann", "person").await;
    let bob = register(&hub, "bob", "org").await;

    // ------------------------------------------------ the first sync of a device
    let first = sync(&hub, &ann, "ann-phone", &Value::Null, 0.0).await;
    let roster: HashSet<&str> = first["roster"].as_array().unwrap().iter().map(|r| r["slug"].as_str().unwrap()).collect();
    assert!(roster.contains(ann.0.as_str()) && roster.contains(bob.0.as_str()), "{first}");
    let me = first["roster"].as_array().unwrap().iter().find(|r| r["slug"] == json!(ann.0)).unwrap();
    assert_eq!((me["kind"].clone(), me["online"].clone()), (json!("person"), json!(true)));
    assert!(first["online"].as_array().unwrap().contains(&json!(ann.0)), "{first}");
    assert_eq!((first["changes"].clone(), first["more"].clone(), first["name"].clone()), (json!([]), json!(false), json!("test-hub")));
    let mut phone = first["cursor"].clone();
    let mut laptop = sync(&hub, &ann, "ann-laptop", &Value::Null, 0.0).await["cursor"].clone();
    let again = sync(&hub, &ann, "ann-phone", &phone, 0.0).await;
    assert_eq!((again["changes"].clone(), again["roster"].clone(), again["roster_removed"].clone()), (json!([]), json!([]), json!([])));
    assert!(again.get("online").is_none(), "the online list came again unchanged: {again}");
    println!("  ok  a new device gets the roster and who is online; nothing again when nothing changed");

    // ------------------------------------------ both devices receive a message
    send(&hub, &bob, &ann.0, "m1").await;
    let on_phone = sync(&hub, &ann, "ann-phone", &phone, 0.0).await;
    let on_laptop = sync(&hub, &ann, "ann-laptop", &laptop, 0.0).await;
    for a in [&on_phone, &on_laptop] {
        assert_eq!(ids(a["changes"].as_array().unwrap()), ["m1"]);
        let m = find(a["changes"].as_array().unwrap(), "m1");
        assert_eq!((m["from"].clone(), m["to"].clone(), m["body"].clone()), (json!(bob.0), json!(ann.0), json!("body of m1")));
        assert_eq!((m["delivered_at"].clone(), m["read_at"].clone()), (Value::Null, Value::Null));
        assert_eq!(a["changes"][0]["type"], "message");
    }
    phone = on_phone["cursor"].clone();
    laptop = on_laptop["cursor"].clone();
    println!("  ok  two devices on one address both receive a message");

    // -------------------------- sent from one device, seen on the other with its receipts
    let before_receipts = laptop.clone();
    send(&hub, &ann, &bob.0, "m2").await;
    let a = sync(&hub, &ann, "ann-laptop", &laptop, 0.0).await;
    assert_eq!(ids(a["changes"].as_array().unwrap()), ["m2"]);
    assert_eq!(find(a["changes"].as_array().unwrap(), "m2")["from"], json!(ann.0));
    laptop = a["cursor"].clone();
    receipt(&hub, &bob, "m2", "delivered", "2026-10-08T12:01:00.000Z").await;
    let a = sync(&hub, &ann, "ann-laptop", &laptop, 0.0).await;
    let m = find(a["changes"].as_array().unwrap(), "m2");
    assert_eq!((m["delivered_at"].clone(), m["read_at"].clone()), (json!("2026-10-08T12:01:00.000Z"), Value::Null));
    laptop = a["cursor"].clone();
    receipt(&hub, &bob, "m2", "read", "2026-10-08T12:02:00.000Z").await;
    let a = sync(&hub, &ann, "ann-laptop", &laptop, 0.0).await;
    assert_eq!(find(a["changes"].as_array().unwrap(), "m2")["read_at"], "2026-10-08T12:02:00.000Z");
    laptop = a["cursor"].clone();
    // a device further behind sees the message once, as it is now
    let behind = sync(&hub, &ann, "ann-laptop", &before_receipts, 0.0).await;
    assert_eq!(ids(behind["changes"].as_array().unwrap()), ["m2"]);
    let m = find(behind["changes"].as_array().unwrap(), "m2");
    assert_eq!((m["delivered_at"].clone(), m["read_at"].clone()), (json!("2026-10-08T12:01:00.000Z"), json!("2026-10-08T12:02:00.000Z")));
    println!("  ok  a message sent from one device appears on the other, then its receipts; a device further behind gets it once, current");

    // ----------------------------------------- read on one device is read on all
    send(&hub, &bob, &ann.0, "m3").await;
    let a = sync(&hub, &ann, "ann-laptop", &laptop, 0.0).await;
    assert_eq!(find(a["changes"].as_array().unwrap(), "m3")["read_at"], Value::Null);
    laptop = a["cursor"].clone();
    let a = sync(&hub, &ann, "ann-phone", &phone, 0.0).await;
    phone = a["cursor"].clone();
    assert_eq!(receipt(&hub, &ann, "m3", "read", "2026-10-08T12:03:00.000Z").await["recorded"], 1);
    assert_eq!(receipt(&hub, &ann, "m3", "read", "2026-10-08T12:09:00.000Z").await["recorded"], 0, "a second device's read moved the time");
    let a = sync(&hub, &ann, "ann-laptop", &laptop, 0.0).await;
    assert_eq!(find(a["changes"].as_array().unwrap(), "m3")["read_at"], "2026-10-08T12:03:00.000Z");
    laptop = a["cursor"].clone();
    // and the sender's v1 poll still gets its receipts
    let p = Call::new("POST", "/api/poll?wait=0").auth(pair(&bob.0, &bob.1)).send(&hub).await.json();
    let r = p["receipts"].as_array().unwrap().iter().find(|r| r["id"] == "m3").cloned().unwrap_or_default();
    assert_eq!(r["state"], "read", "{p}");
    println!("  ok  read on one device is read on the others (first read wins); the v1 sender still gets the receipt by poll");

    // ------------------------------------------------- custody, as an ack
    let carl = register(&hub, "carl", "org").await;
    send(&hub, &carl, &ann.0, "m5").await;
    let state = |id: &'static str| {
        let sql = &sql;
        async move { sql.query_one("SELECT state FROM messages WHERE id = $1", &[&id]).await.unwrap().get::<_, String>(0) }
    };
    assert_eq!(state("m5").await, "queued");
    let a = sync(&hub, &ann, "ann-phone", &phone, 0.0).await;
    assert!(ids(a["changes"].as_array().unwrap()).contains(&"m5".to_string()));
    assert_eq!(state("m5").await, "queued", "custody moved before the device confirmed it");
    phone = a["cursor"].clone();
    let _ = sync(&hub, &ann, "ann-phone", &phone, 0.0).await;
    assert_eq!(state("m5").await, "fetched");
    let p = Call::new("POST", "/api/poll?wait=0").auth(pair(&carl.0, &carl.1)).send(&hub).await.json();
    assert_eq!(p["receipts"][0]["state"], "fetched", "{p}");
    let v1 = Call::new("POST", "/api/poll?wait=0").auth(pair(&ann.0, &ann.1)).send(&hub).await.json();
    let left: Vec<&Value> = v1["messages"].as_array().unwrap().iter().filter(|m| m["id"] == "m5").collect();
    assert!(left.is_empty(), "a message a device holds is still queued for v1 polls: {v1}");
    println!("  ok  custody: a device's next cursor hands its mail over (v1 senders see \"fetched\", v1 polls stop returning it)");

    // ------------------------------------------------------- long-poll wake-ups
    let (_, last) = drain(&hub, &ann, "ann-laptop", &laptop).await;
    laptop = last["cursor"].clone();
    let t = parked(&hub, &ann, "ann-laptop", &laptop, 20.0);
    tokio::time::sleep(Duration::from_millis(300)).await;
    let sent = Instant::now();
    send(&hub, &bob, &ann.0, "m6").await;
    let (a, woke) = t.await.unwrap();
    assert!(ids(a["changes"].as_array().unwrap()).contains(&"m6".to_string()), "{a}");
    let incoming_ms = woke.duration_since(sent).as_millis();
    laptop = a["cursor"].clone();

    let t = parked(&hub, &ann, "ann-laptop", &laptop, 20.0);
    tokio::time::sleep(Duration::from_millis(300)).await;
    let sent = Instant::now();
    send(&hub, &ann, &bob.0, "m7").await;
    let (a, woke) = t.await.unwrap();
    assert_eq!(ids(a["changes"].as_array().unwrap()), ["m7"]);
    let own_ms = woke.duration_since(sent).as_millis();
    laptop = a["cursor"].clone();

    let t = parked(&hub, &ann, "ann-laptop", &laptop, 20.0);
    tokio::time::sleep(Duration::from_millis(300)).await;
    let sent = Instant::now();
    let dee = register(&hub, "dee", "chat").await;
    let (a, woke) = t.await.unwrap();
    assert_eq!(a["roster"].as_array().unwrap().iter().map(|r| r["slug"].clone()).collect::<Vec<_>>(), [json!(dee.0)]);
    assert_eq!(a["roster"][0]["kind"], "chat");
    let join_ms = woke.duration_since(sent).as_millis();
    laptop = a["cursor"].clone();

    let t = parked(&hub, &ann, "ann-laptop", &laptop, 20.0);
    tokio::time::sleep(Duration::from_millis(300)).await;
    let r = Call::new("POST", "/api/profile").auth(pair(&bob.0, &bob.1)).json(json!({ "name": "Bob B." })).send(&hub).await;
    assert_eq!(r.code(), 200);
    let (a, _) = t.await.unwrap();
    assert_eq!((a["roster"][0]["slug"].clone(), a["roster"][0]["org_name"].clone()), (json!(bob.0), json!("Bob B.")));
    laptop = a["cursor"].clone();

    // an unchanged re-registration is not a roster change
    let r = Call::new("POST", "/api/register")
        .auth(pair(&bob.0, &bob.1))
        .json(json!({ "slug": bob.0, "org_name": "Bob B.", "username": "sync", "kind": "org" }))
        .send(&hub)
        .await;
    assert_eq!(r.code(), 200);
    let a = sync(&hub, &ann, "ann-laptop", &laptop, 0.0).await;
    assert_eq!(a["roster"], json!([]), "an unchanged re-registration showed up as a change");

    let t = parked(&hub, &ann, "ann-laptop", &laptop, 20.0);
    tokio::time::sleep(Duration::from_millis(300)).await;
    let r = Call::new("POST", "/api/unregister").auth(pair(&dee.0, &dee.1)).send(&hub).await;
    assert_eq!(r.code(), 200);
    let (a, _) = t.await.unwrap();
    assert_eq!((a["roster"].clone(), a["roster_removed"].clone()), (json!([]), json!([dee.0])));
    laptop = a["cursor"].clone();

    let t0 = Instant::now();
    let a = sync(&hub, &ann, "ann-laptop", &laptop, 1.0).await;
    let idle_ms = t0.elapsed().as_millis();
    assert!((900..3000).contains(&idle_ms), "an idle sync returned after {idle_ms} ms");
    assert_eq!(a["changes"], json!([]));
    println!(
        "  ok  a parked sync wakes for mail in ({incoming_ms} ms), for mail its address sent from another device ({own_ms} ms), \
         for a join ({join_ms} ms), a profile edit and a leave; an idle one returns at its wait ({idle_ms} ms)"
    );

    // ------------------------------------------------- who is online changes
    let eve = register(&hub, "eve", "person").await;
    let a = sync(&hub, &ann, "ann-laptop", &laptop, 0.0).await;
    assert!(a["online"].as_array().map(|o| o.contains(&json!(eve.0))).unwrap_or(false), "{a}");
    laptop = a["cursor"].clone();
    let a = sync(&hub, &ann, "ann-laptop", &laptop, 0.0).await;
    assert!(a.get("online").is_none(), "the online list came again unchanged: {a}");
    println!("  ok  presence: the online list comes when it changed, and only then");

    // ------------------------------------- a device off for a week catches up
    let tablet = sync(&hub, &ann, "ann-tablet", &Value::Null, 0.0).await["cursor"].clone();
    let mut sent_ids = Vec::new();
    for i in 0..600 {
        let id = format!("bulk-in-{i}");
        send(&hub, &bob, &ann.0, &id).await;
        sent_ids.push(id);
    }
    for i in 0..20 {
        let id = format!("bulk-out-{i}");
        send(&hub, &ann, &carl.0, &id).await;
        sent_ids.push(id);
    }
    for i in (0..600).step_by(7) {
        receipt(&hub, &ann, &format!("bulk-in-{i}"), "read", "2026-10-08T13:00:00.000Z").await;
    }
    // the phone catches up first; the tablet still gets everything after
    let (on_phone, last) = drain(&hub, &ann, "ann-phone", &phone).await;
    phone = last["cursor"].clone();
    assert!(ids(&on_phone).len() >= 620);
    let first_page = sync(&hub, &ann, "ann-tablet", &tablet, 0.0).await;
    assert_eq!((first_page["changes"].as_array().unwrap().len(), first_page["more"].clone()), (500, json!(true)));
    let (on_tablet, _) = drain(&hub, &ann, "ann-tablet", &tablet).await;
    let got = ids(&on_tablet);
    let unique: HashSet<&String> = got.iter().collect();
    assert_eq!(unique.len(), got.len(), "a change came twice");
    for id in &sent_ids {
        assert!(unique.contains(id), "{id} never reached the tablet");
    }
    assert_eq!(find(&on_tablet, "bulk-in-7")["read_at"], "2026-10-08T13:00:00.000Z");
    assert_eq!(find(&on_tablet, "bulk-in-8")["read_at"], Value::Null);
    println!("  ok  a device that was away catches up from its cursor: {} changes in pages of 500, each once, current", got.len());

    // ------------------------------------------------------------- the devices
    let r = sync_call(&hub, &ann, json!({ "device_id": "ann-phone", "device_name": "  Ann's Pixel  ", "cursor": phone, "wait": 0 })).await;
    assert_eq!(r.code(), 200);
    let _ = sync(&hub, &ann, "ann-phone", &r.json()["cursor"], 0.0).await; // no name: kept
    let d = Call::new("GET", "/api/devices").auth(pair(&ann.0, &ann.1)).send(&hub).await;
    assert_eq!(d.code(), 200, "{}", d.text());
    let d = d.json();
    let names: Vec<(String, String)> = d["devices"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| (x["device_id"].as_str().unwrap().to_string(), x["name"].as_str().unwrap().to_string()))
        .collect();
    assert_eq!(
        names,
        [("ann-phone".to_string(), "Ann's Pixel".to_string()), ("ann-laptop".into(), "".into()), ("ann-tablet".into(), "".into())]
    );
    assert!(d["devices"].as_array().unwrap().iter().all(|x| x["online"] == json!(true)));
    assert_eq!(d["slug"], json!(ann.0));
    let bobs = Call::new("GET", &format!("/api/devices?slug={}", bob.0)).auth(pair(&bob.0, &bob.1)).send(&hub).await.json();
    assert_eq!(bobs["devices"], json!([]));
    println!("  ok  devices: listed with their names (set by a sync, kept when a sync names none) and whether they are online");

    // ------------------------------------------------------------ refusals
    let both = format!("{} {}", pair(&ann.0, &ann.1), pair(&bob.0, &bob.1));
    for (body, code, detail) in [
        (json!({}), 422, "device_id is required"),
        (json!({ "device_id": "" }), 422, "device_id must be 1 to 64 printable ASCII characters without spaces"),
        (json!({ "device_id": "d".repeat(65) }), 422, "device_id must be 1 to 64 printable ASCII characters without spaces"),
        (json!({ "device_id": "has space" }), 422, "device_id must be 1 to 64 printable ASCII characters without spaces"),
        (json!({ "device_id": 5 }), 422, "device_id must be 1 to 64 printable ASCII characters without spaces"),
        (json!({ "device_id": "x", "device_name": "n".repeat(65) }), 422, "device_name is longer than 64 characters"),
        (json!({ "device_id": "x", "device_name": 5 }), 422, "device_name must be a string"),
        (json!({ "device_id": "x", "cursor": "garbage" }), 422, "cursor is not a sync cursor from this hub"),
        (json!({ "device_id": "x", "cursor": 5 }), 422, "cursor is not a sync cursor from this hub"),
        (json!({ "device_id": "x", "wait": "soon" }), 422, "wait must be a number of seconds"),
    ] {
        let r = sync_call(&hub, &ann, body.clone()).await;
        assert_eq!((r.code(), r.json()["detail"].clone()), (code, json!(detail)), "{body}");
    }
    let r = Call::new("POST", "/api/sync").auth(pair(&ann.0, &ann.1)).content("nope").send(&hub).await;
    assert_eq!((r.code(), r.json()["detail"].clone()), (400, json!("the request body must be a JSON object")));
    let r = Call::new("POST", "/api/sync").json(json!({ "device_id": "x" })).send(&hub).await;
    assert_eq!(r.code(), 401);
    let r = Call::new("POST", "/api/sync").auth(pair(&ann.0, "wrong")).json(json!({ "device_id": "x" })).send(&hub).await;
    assert_eq!(r.code(), 401);
    let r = Call::new("POST", "/api/sync").auth(both.clone()).json(json!({ "device_id": "x", "wait": 0 })).send(&hub).await;
    assert_eq!((r.code(), r.json()["detail"].clone()), (422, json!("several addresses signed in: name the one to use (slug)")));
    let r = Call::new("POST", "/api/sync").auth(both.clone()).json(json!({ "device_id": "x", "wait": 0, "slug": bob.0 })).send(&hub).await;
    assert_eq!(r.code(), 200);
    let r = Call::new("POST", "/api/sync").auth(pair(&ann.0, &ann.1)).json(json!({ "device_id": "x", "slug": bob.0 })).send(&hub).await;
    assert_eq!(r.code(), 401);
    assert_eq!(Call::new("GET", "/api/devices").auth(both.clone()).send(&hub).await.code(), 422);
    assert_eq!(Call::new("GET", "/api/devices").send(&hub).await.code(), 401);
    assert_eq!(Call::new("GET", "/api/sync").send(&hub).await.code(), 405);
    let r = Call::new("POST", "/api/sync?wait=x").auth(pair(&ann.0, &ann.1)).json(json!({ "device_id": "x" })).send(&hub).await;
    assert_eq!(r.code(), 422, "a bad wait query number");
    println!("  ok  refusals: device id and name, cursor, wait, body, credentials, several addresses");

    // -------------------------------------------- a cursor from another history
    let a = sync(&hub, &ann, "ann-phone", &json!("999999-0-0000000000000000"), 0.0).await;
    assert_eq!(a["reset"], true);
    assert!(a["changes"].as_array().unwrap().len() >= 3 && a["online"].is_array(), "{a}");
    assert_eq!(ids(a["changes"].as_array().unwrap())[0], "m1", "a reset did not start from the beginning");
    let a = sync(&hub, &ann, "ann-phone", &json!("0-999999-0000000000000000"), 0.0).await;
    assert_eq!(a["reset"], true);
    assert!(sync(&hub, &ann, "ann-phone", &phone, 0.0).await.get("reset").is_none());
    println!("  ok  a cursor beyond what the hub wrote (a restored database) starts the device over, saying so");

    // ---------------------------- leaving and coming back keeps the mailbox
    let r = Call::new("POST", "/api/unregister").auth(pair(&ann.0, &ann.1)).send(&hub).await;
    assert_eq!(r.code(), 200);
    assert_eq!(sql.query_one("SELECT count(*) FROM devices WHERE slug = $1", &[&ann.0]).await.unwrap().get::<_, i64>(0), 0);
    let r = Call::new("POST", "/api/register")
        .auth(pair(&ann.0, &ann.1))
        .json(json!({ "slug": ann.0, "org_name": "ann", "username": "sync", "kind": "person" }))
        .send(&hub)
        .await;
    assert_eq!(r.code(), 200);
    send(&hub, &bob, &ann.0, "after-return").await;
    let a = sync(&hub, &ann, "ann-phone", &phone, 0.0).await;
    assert!(a.get("reset").is_none(), "{a}");
    assert!(ids(a["changes"].as_array().unwrap()).contains(&"after-return".to_string()));
    phone = a["cursor"].clone();
    println!("  ok  unregister drops the devices but not the mailbox: back with the same key, a device continues from its cursor");

    // --------------------------------- mail that predates the change log (import)
    sql.execute(
        "INSERT INTO messages (id, from_slug, to_slug, body, received_at, attachments) VALUES ('imported-1', $1, $2, 'old', now(), '[]')",
        &[&bob.0, &ann.0],
    )
    .await
    .unwrap();
    let pooled = hub.db.get().await.unwrap();
    mailhub::db::backfill_sync(&pooled).await.unwrap();
    mailhub::db::backfill_sync(&pooled).await.unwrap(); // idempotent
    drop(pooled);
    let a = sync(&hub, &ann, "ann-phone", &phone, 0.0).await;
    assert_eq!(ids(a["changes"].as_array().unwrap()), ["imported-1"]);
    send(&hub, &bob, &ann.0, "after-import").await;
    let a = sync(&hub, &ann, "ann-phone", &a["cursor"], 0.0).await;
    assert_eq!(ids(a["changes"].as_array().unwrap()), ["after-import"]);
    println!("  ok  mail without change-log entries (an import) is given them once, in order, before newer mail");

    // ------------------------------------------------------- at most 100 devices
    let fay = register(&hub, "fay", "person").await;
    for i in 0..101 {
        let r = sync_call(&hub, &fay, json!({ "device_id": format!("dev-{i:03}"), "wait": 0 })).await;
        assert_eq!(r.code(), 200);
    }
    let d = Call::new("GET", "/api/devices").auth(pair(&fay.0, &fay.1)).send(&hub).await.json();
    let list: Vec<&str> = d["devices"].as_array().unwrap().iter().map(|x| x["device_id"].as_str().unwrap()).collect();
    assert_eq!(list.len(), 100);
    assert!(!list.contains(&"dev-000") && list.contains(&"dev-100"), "the device seen longest ago was not the one replaced");
    println!("  ok  an address keeps at most 100 devices: a new one replaces the one seen longest ago");

    let _ = std::fs::remove_dir_all(&data);
    drop(pg);
}

/// Concurrent writers never let a syncing device skip a change: every
/// message sent while a device syncs as fast as it can reaches it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn no_change_is_skipped_under_concurrent_writers() {
    let pg = TestPg::start();
    let url = pg.fresh_db("hubsyncrace").await;
    let data = std::env::temp_dir().join(format!("orgtree-hub-syncrace-{}", token_hex16()));
    std::fs::create_dir_all(&data).unwrap();
    let hub = hub(&url, &data, &[]).await;

    let target = register(&hub, "target", "person").await;
    let mut senders = Vec::new();
    for i in 0..8 {
        senders.push(register(&hub, &format!("s{i}"), "org").await);
    }
    let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let reader = {
        let (hub, target, done) = (hub.clone(), target.clone(), done.clone());
        tokio::spawn(async move {
            let mut cursor = Value::Null;
            let mut seen: HashSet<String> = HashSet::new();
            let mut last_mail = 0i64;
            let mut answers = 0;
            loop {
                let finished = done.load(std::sync::atomic::Ordering::Acquire);
                let a = sync(&hub, &target, "reader", &cursor, 0.0).await;
                answers += 1;
                for id in ids(a["changes"].as_array().unwrap()) {
                    seen.insert(id);
                }
                cursor = a["cursor"].clone();
                let mail: i64 = cursor.as_str().unwrap().split('-').next().unwrap().parse().unwrap();
                assert!(mail >= last_mail, "the cursor went backwards");
                last_mail = mail;
                if finished && a["more"] != json!(true) {
                    return (seen, answers);
                }
            }
        })
    };
    // eight senders, plus the target answering (both directions at once)
    let mut tasks = Vec::new();
    for (i, s) in senders.iter().enumerate() {
        let (hub, s, target) = (hub.clone(), s.clone(), target.clone());
        tasks.push(tokio::spawn(async move {
            for k in 0..100 {
                send(&hub, &s, &target.0, &format!("race-{i}-{k}")).await;
                if k % 10 == 0 {
                    send(&hub, &target, &s.0, &format!("back-{i}-{k}")).await;
                    receipt(&hub, &target, &format!("race-{i}-{k}"), "read", "2026-10-08T14:00:00.000Z").await;
                }
            }
        }));
    }
    for t in tasks {
        t.await.unwrap();
    }
    done.store(true, std::sync::atomic::Ordering::Release);
    let (seen, answers) = reader.await.unwrap();
    let mut missing = Vec::new();
    for i in 0..8 {
        for k in 0..100 {
            let id = format!("race-{i}-{k}");
            if !seen.contains(&id) {
                missing.push(id);
            }
            if k % 10 == 0 && !seen.contains(&format!("back-{i}-{k}")) {
                missing.push(format!("back-{i}-{k}"));
            }
        }
    }
    assert!(missing.is_empty(), "never reached the syncing device: {missing:?}");
    println!("  ok  880 messages and 80 receipts from 9 concurrent writers: every one reached a device syncing alongside ({answers} answers)");

    let _ = std::fs::remove_dir_all(&data);
    drop(pg);
}
