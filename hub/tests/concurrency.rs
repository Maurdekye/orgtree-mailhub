//! What v1 never had to prove: it ran on one event-loop thread, so every
//! handler's check-then-write was atomic by construction. v2 serves requests
//! on many threads at once, so each of those check-then-writes is attacked
//! here with real concurrency (a multi-thread runtime, in-process requests),
//! plus delivery under load and the long poll's wake-up latency.
//!
//!     HUB_TEST_PG=<folder> cargo test --test concurrency -- --nocapture

mod support;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use mailhub::Hub;
use serde_json::{json, Value};
use support::*;

type Org = (String, String);

async fn register(hub: &Arc<Hub>, slug: &str, secret: &str) -> u16 {
    Call::new("POST", "/api/register")
        .auth(pair(slug, secret))
        .json(json!({ "slug": slug, "org_name": slug, "username": "load" }))
        .send(hub)
        .await
        .code()
}

async fn org(hub: &Arc<Hub>, name: &str) -> Org {
    let o = (format!("{name}.load.{}", token_hex3()), token_hex16());
    assert_eq!(register(hub, &o.0, &o.1).await, 200);
    o
}

async fn send(hub: &Arc<Hub>, from: &Org, payload: Value) -> Resp {
    Call::new("POST", "/api/send").auth(pair(&from.0, &from.1)).json(payload).send(hub).await
}

async fn poll(hub: &Arc<Hub>, who: &Org, wait: f64) -> Value {
    let r = Call::new("POST", &format!("/api/poll?wait={wait}")).auth(pair(&who.0, &who.1)).send(hub).await;
    assert_eq!(r.code(), 200, "{}", r.text());
    r.json()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrency() {
    let pg = TestPg::start();
    let url = pg.fresh_db("hubload").await;
    let data = std::env::temp_dir().join(format!("orgtree-hub-load-{}", token_hex16()));
    std::fs::create_dir_all(&data).unwrap();
    let hub = hub(&url, &data, &[("HUB_DB_POOL", "24")]).await;
    let db = sql_client(&url).await;

    // ---------------------------------------------- one address, ten claimants
    let slug = format!("contested.load.{}", token_hex3());
    let secrets: Vec<String> = (0..10).map(|_| token_hex16()).collect();
    let codes = futures::future::join_all(secrets.iter().map(|s| {
        let (hub, slug, s) = (hub.clone(), slug.clone(), s.clone());
        tokio::spawn(async move { register(&hub, &slug, &s).await })
    }))
    .await;
    let codes: Vec<u16> = codes.into_iter().map(Result::unwrap).collect();
    let winners: Vec<&String> = secrets.iter().zip(&codes).filter(|(_, c)| **c == 200).map(|(s, _)| s).collect();
    assert_eq!(winners.len(), 1, "ten simultaneous claims of one address: {codes:?}");
    assert_eq!(codes.iter().filter(|c| **c == 403).count(), 9, "{codes:?}");
    let stored: String = db.query_one("SELECT fingerprint FROM identities WHERE slug = $1", &[&slug]).await.unwrap().get(0);
    assert_eq!(stored, sha256_hex(winners[0]), "the stored fingerprint is not the winner's");
    println!("  ok  one address, ten simultaneous claimants: exactly one owns it");

    // -------------------------------------- one upload, ten messages binding it
    let owner = org(&hub, "owner").await;
    let up = Call::new("POST", "/api/attachments?name=one.bin").auth(pair(&owner.0, &owner.1)).content(axum::body::Body::from(&b"once"[..])).send(&hub).await;
    let aid = up.json()["id"].as_str().unwrap().to_string();
    let mut rcpts = Vec::new();
    for i in 0..10 {
        rcpts.push(org(&hub, &format!("rcpt{i}")).await);
    }
    let results = futures::future::join_all(rcpts.iter().enumerate().map(|(i, r)| {
        let (hub, owner, to, aid) = (hub.clone(), owner.clone(), r.0.clone(), aid.clone());
        tokio::spawn(async move { send(&hub, &owner, json!({ "id": format!("bind-{i}"), "to": to, "body": "x", "attachments": [aid] })).await })
    }))
    .await;
    let codes: Vec<u16> = results.iter().map(|r| r.as_ref().unwrap().code()).collect();
    assert_eq!(codes.iter().filter(|c| **c == 200).count(), 1, "{codes:?}");
    assert!(results.iter().filter(|r| r.as_ref().unwrap().code() == 422).all(|r| r.as_ref().unwrap().text().contains("already bound")));
    let bound: Option<String> = db.query_one("SELECT message_id FROM attachments WHERE id = $1", &[&aid]).await.unwrap().get(0);
    let sent: i64 = db.query_one("SELECT count(*) FROM messages WHERE id LIKE 'bind-%'", &[]).await.unwrap().get(0);
    assert_eq!(sent, 1, "a refused send left a message behind");
    assert!(bound.unwrap().starts_with("bind-"));
    println!("  ok  one upload, ten simultaneous sends binding it: one message carries it, nine are refused");

    // ---------------------------------------------- one message id, ten sends
    let (a, b) = (org(&hub, "dupa").await, org(&hub, "dupb").await);
    let results = futures::future::join_all((0..10).map(|i| {
        let (hub, a, to) = (hub.clone(), a.clone(), b.0.clone());
        tokio::spawn(async move { send(&hub, &a, json!({ "id": "same-id", "to": to, "body": format!("try {i}") })).await.json() })
    }))
    .await;
    let results: Vec<Value> = results.into_iter().map(Result::unwrap).collect();
    assert_eq!(results.iter().filter(|r| r["duplicate"] == json!(false)).count(), 1, "{results:?}");
    let stamps: HashSet<String> = results.iter().map(|r| r["received_at"].as_str().unwrap().to_string()).collect();
    assert_eq!(stamps.len(), 1, "retries reported different receipt times: {stamps:?}");
    let rows: i64 = db.query_one("SELECT count(*) FROM messages WHERE id = 'same-id'", &[]).await.unwrap().get(0);
    assert_eq!(rows, 1);
    println!("  ok  one message id, ten simultaneous sends: one message, nine duplicates, one receipt time");

    // --------------------------------- a receipt owed, ten polls racing for it
    let mid = send(&hub, &a, json!({ "to": b.0, "body": "track" })).await.json()["id"].as_str().unwrap().to_string();
    Call::new("POST", "/api/ack").auth(pair(&b.0, &b.1)).json(json!({ "ids": [mid] })).send(&hub).await;
    let polls = futures::future::join_all((0..10).map(|_| {
        let (hub, a) = (hub.clone(), a.clone());
        tokio::spawn(async move { poll(&hub, &a, 0.0).await })
    }))
    .await;
    let carried: usize = polls.iter().map(|p| p.as_ref().unwrap()["receipts"].as_array().unwrap().iter().filter(|r| r["id"] == json!(mid)).count()).sum();
    assert_eq!(carried, 1, "a receipt was pushed {carried} times");
    println!("  ok  one owed receipt, ten simultaneous polls by its sender: pushed exactly once");

    // ------------------------------------------- the long poll wakes at once
    let c = org(&hub, "sleeper").await;
    let parked = {
        let (hub, c) = (hub.clone(), c.clone());
        tokio::spawn(async move {
            let t0 = Instant::now();
            let got = poll(&hub, &c, 30.0).await;
            (t0.elapsed(), got)
        })
    };
    tokio::time::sleep(Duration::from_millis(500)).await;
    let t_send = Instant::now();
    send(&hub, &a, json!({ "to": c.0, "body": "wake" })).await;
    let (_, got) = parked.await.unwrap();
    let latency = t_send.elapsed();
    assert_eq!(got["messages"][0]["body"], "wake");
    assert!(latency < Duration::from_millis(500), "a parked poll took {latency:?} to answer");
    println!("  ok  a parked poll answers {:.1} ms after the send that wakes it (v1 re-checked every 500 ms)", latency.as_secs_f64() * 1000.0);

    // ------------------------------------------------------ delivery under load
    let receivers: Vec<Org> = futures::future::join_all((0..16).map(|i| {
        let hub = hub.clone();
        async move { org(&hub, &format!("in{i}")).await }
    }))
    .await;
    let senders: Vec<Org> = futures::future::join_all((0..16).map(|i| {
        let hub = hub.clone();
        async move { org(&hub, &format!("out{i}")).await }
    }))
    .await;
    const PER_SENDER: usize = 50;
    let total = senders.len() * PER_SENDER;
    let stop = Arc::new(tokio::sync::Notify::new());
    let t0 = Instant::now();
    // each receiver: long polls, acks what it got, records order
    let consumers: Vec<_> = receivers
        .iter()
        .map(|r| {
            let (hub, r, stop) = (hub.clone(), r.clone(), stop.clone());
            tokio::spawn(async move {
                let mut seen: Vec<(String, String, i64)> = Vec::new();
                loop {
                    let got = tokio::select! {
                        g = poll(&hub, &r, 2.0) => g,
                        _ = stop.notified() => break,
                    };
                    let msgs = got["messages"].as_array().cloned().unwrap_or_default();
                    if msgs.is_empty() {
                        continue;
                    }
                    let ids: Vec<String> = msgs.iter().map(|m| m["id"].as_str().unwrap().to_string()).collect();
                    for m in &msgs {
                        seen.push((m["id"].as_str().unwrap().to_string(), m["received_at"].as_str().unwrap().to_string(), 0));
                    }
                    let acked = Call::new("POST", "/api/ack").auth(pair(&r.0, &r.1)).json(json!({ "ids": ids })).send(&hub).await;
                    assert_eq!(acked.json()["acked"].as_u64(), Some(msgs.len() as u64), "an ack lost custody");
                }
                seen
            })
        })
        .collect();
    // two pollers per sender collect its receipts
    let receipt_takers: Vec<_> = senders
        .iter()
        .flat_map(|s| [s.clone(), s.clone()])
        .map(|s| {
            let (hub, stop) = (hub.clone(), stop.clone());
            tokio::spawn(async move {
                let mut got: Vec<String> = Vec::new();
                loop {
                    let r = tokio::select! {
                        g = poll(&hub, &s, 2.0) => g,
                        _ = stop.notified() => break,
                    };
                    got.extend(r["receipts"].as_array().cloned().unwrap_or_default().iter().map(|x| x["id"].as_str().unwrap().to_string()));
                }
                (s.0, got)
            })
        })
        .collect();
    let producers: Vec<_> = senders
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let (hub, s, receivers) = (hub.clone(), s.clone(), receivers.clone());
            tokio::spawn(async move {
                let mut sent: Vec<(String, String)> = Vec::new();
                for k in 0..PER_SENDER {
                    let to = receivers[(i * 7 + k * 3) % receivers.len()].0.clone();
                    let id = format!("load-{i}-{k}");
                    let r = send(&hub, &s, json!({ "id": id, "to": to, "body": format!("{i}/{k}") })).await;
                    assert_eq!(r.code(), 200, "{}", r.text());
                    sent.push((id, to));
                }
                sent
            })
        })
        .collect();
    let mut expected: HashMap<String, String> = HashMap::new();
    for p in producers {
        for (id, to) in p.await.unwrap() {
            expected.insert(id, to);
        }
    }
    let sent_at = t0.elapsed();
    // wait until every message is acked, then stop the pollers
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let queued: i64 = db.query_one("SELECT count(*) FROM messages WHERE id LIKE 'load-%' AND state = 'queued'", &[]).await.unwrap().get(0);
        let owed: i64 = db.query_one("SELECT count(*) FROM messages WHERE id LIKE 'load-%' AND NOT receipts_pushed", &[]).await.unwrap().get(0);
        if queued == 0 && owed == 0 {
            break;
        }
        assert!(Instant::now() < deadline, "still {queued} queued and {owed} receipts owed after 60 s");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let drained = t0.elapsed();
    tokio::time::sleep(Duration::from_millis(200)).await;
    for _ in 0..4 {
        stop.notify_waiters();
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let by_id: HashMap<String, String> = expected.clone();
    let mut delivered: HashMap<String, usize> = HashMap::new();
    for (r, c) in receivers.iter().zip(consumers) {
        let seen = c.await.unwrap();
        // per recipient, deliveries arrive in hub-clock order
        for w in seen.windows(2) {
            assert!(w[0].1 <= w[1].1, "{}: {} arrived before {}", r.0, w[1].0, w[0].0);
        }
        for (id, _, _) in seen {
            assert_eq!(by_id.get(&id), Some(&r.0), "{id} reached the wrong recipient");
            *delivered.entry(id).or_default() += 1;
        }
    }
    let missing: Vec<&String> = expected.keys().filter(|id| !delivered.contains_key(*id)).collect();
    assert!(missing.is_empty(), "{} messages never delivered: {:?}", missing.len(), &missing[..missing.len().min(5)]);
    let twice = delivered.values().filter(|n| **n > 1).count();
    assert_eq!(twice, 0, "{twice} messages delivered more than once although each was acked before the next poll");
    let mut receipts: HashMap<String, usize> = HashMap::new();
    for t in receipt_takers {
        let (sender, got) = t.await.unwrap();
        for id in got {
            assert!(id.starts_with(&format!("load-{}-", senders.iter().position(|s| s.0 == sender).unwrap())), "{id} went to the wrong sender");
            *receipts.entry(id).or_default() += 1;
        }
    }
    assert_eq!(receipts.len(), total, "receipts reached their senders for {} of {total} messages", receipts.len());
    assert!(receipts.values().all(|n| *n == 1), "a receipt was pushed twice");
    println!(
        "  ok  {total} messages from 16 concurrent senders to 16 long-polling receivers: each delivered once, in hub-clock \
         order per recipient, each receipt pushed once although two pollers per sender raced for them \
         (all sent in {:.2} s, all delivered and receipted after {:.2} s)",
        sent_at.as_secs_f64(),
        drained.as_secs_f64()
    );
    let _ = std::fs::remove_dir_all(&data);
    drop(pg);
}
