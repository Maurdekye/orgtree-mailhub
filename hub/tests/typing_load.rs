//! Typing (feature `typing`): what relaying heartbeats costs the hub. A
//! sender beats every half second for ten seconds to a recipient whose
//! device in use syncs the way Hubchat does (parked, and back at once with
//! the same cursor after a relay). The run counts the requests and relays,
//! the gaps between relays (the indicator stays up while each comes within a
//! second of the last), the delay from heartbeat to relay, the bytes of a
//! relay, the database statements (every statement goes through
//! `mailhub::db`, which logs it at debug under `hub::sql`) and this test
//! process's CPU time, each against the same device idle. Requests are
//! dispatched in-process: no sockets, no TLS.
//!
//!     HUB_TEST_PG=<folder> cargo test --test typing_load -- --nocapture

mod support;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use mailhub::api::sync::presence_loop;
use mailhub::Hub;
use serde_json::{json, Value};
use support::*;
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::Layer;

type Id = (String, String);

static STATEMENTS: AtomicU64 = AtomicU64::new(0);

/// Counts the hub's SQL statements (`mailhub::db` logs each one).
struct CountSql;

impl<S: tracing::Subscriber> Layer<S> for CountSql {
    fn on_event(&self, e: &tracing::Event<'_>, _: Context<'_, S>) {
        if e.metadata().target() == "hub::sql" {
            STATEMENTS.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// This process's CPU time so far (user and kernel), where it can be read.
#[cfg(windows)]
fn cpu() -> Option<Duration> {
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};
    let mut c = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
    let mut e = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
    let mut k = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
    let mut u = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
    // SAFETY: the pseudo-handle of this process and four owned out-values
    let ok = unsafe { GetProcessTimes(GetCurrentProcess(), &mut c, &mut e, &mut k, &mut u) } != 0;
    let ns = |f: &FILETIME| (((f.dwHighDateTime as u64) << 32) | f.dwLowDateTime as u64) * 100;
    ok.then(|| Duration::from_nanos(ns(&k) + ns(&u)))
}

#[cfg(not(windows))]
fn cpu() -> Option<Duration> {
    None
}

async fn register(hub: &Arc<Hub>, name: &str, kind: &str) -> Id {
    let id = (format!("{name}.load.{}", token_hex3()), token_hex16());
    let r = Call::new("POST", "/api/register")
        .auth(pair(&id.0, &id.1))
        .json(json!({ "slug": id.0, "org_name": name, "username": "load", "kind": kind }))
        .send(hub)
        .await;
    assert_eq!(r.code(), 200, "{}", r.text());
    id
}

/// What a device saw over a stretch of time.
#[derive(Default)]
struct Seen {
    syncs: u64,
    /// when each answer relaying a heartbeat came, and its size in bytes
    relays: Vec<(Instant, usize)>,
    typing_only: u64,
}

/// A device's sync loop as Hubchat runs it, until `until`: parked, and back
/// at once after each answer with its cursor and typing mark.
async fn device(hub: Arc<Hub>, who: Id, device_id: &'static str, mut last: Value, until: Instant) -> (Seen, Value) {
    let mut seen = Seen::default();
    while Instant::now() < until {
        let wait = until.saturating_duration_since(Instant::now()).as_secs_f64();
        let body = json!({ "device_id": device_id, "wait": wait, "cursor": last["cursor"], "typing": last["typing_seq"] });
        let r = Call::new("POST", "/api/sync").auth(pair(&who.0, &who.1)).json(body).send(&hub).await;
        let at = Instant::now();
        assert_eq!(r.code(), 200, "{}", r.text());
        seen.syncs += 1;
        let a = r.json();
        if a["typing"].as_array().is_some_and(|t| !t.is_empty()) {
            seen.relays.push((at, r.body.len()));
        }
        if a["typing_only"] == json!(true) {
            seen.typing_only += 1;
        }
        last = a;
    }
    (seen, last)
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

/// The `q` quantile of `v` (sorted in place).
fn quantile(v: &mut [f64], q: f64) -> f64 {
    v.sort_by(|a, b| a.total_cmp(b));
    v.get(((v.len() as f64 - 1.0) * q).round() as usize).copied().unwrap_or(f64::NAN)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn relaying_heartbeats_costs_no_database_work_beyond_the_syncs() {
    let subscriber = tracing_subscriber::registry().with(CountSql.with_filter(tracing_subscriber::filter::Targets::new().with_target("hub::sql", tracing::Level::DEBUG)));
    tracing::subscriber::set_global_default(subscriber).expect("one subscriber for this test binary");

    let pg = TestPg::start();
    let url = pg.fresh_db("hubtypingload").await;
    let data = std::env::temp_dir().join(format!("orgtree-hub-typingload-{}", token_hex16()));
    std::fs::create_dir_all(&data).unwrap();
    let hub = hub(&url, &data, &[]).await;
    tokio::spawn(presence_loop(hub.clone()));

    let org = register(&hub, "org", "org").await;
    let ann = register(&hub, "ann", "person").await;
    let r = Call::new("POST", "/api/devices/active").auth(pair(&ann.0, &ann.1)).json(json!({ "device_id": "ann-desk", "active": true })).send(&hub).await;
    assert_eq!(r.code(), 200, "{}", r.text());
    let first = Call::new("POST", "/api/sync").auth(pair(&ann.0, &ann.1)).json(json!({ "device_id": "ann-desk", "wait": 0, "typing": true })).send(&hub).await;
    assert_eq!(first.code(), 200, "{}", first.text());
    let mut last = first.json();
    tokio::time::sleep(Duration::from_secs(1)).await;

    // idle: the same device parked, nobody writing
    let span = Duration::from_secs(5);
    let (s0, c0, t0) = (STATEMENTS.load(Ordering::Relaxed), cpu(), Instant::now());
    let (idle, l) = device(hub.clone(), ann.clone(), "ann-desk", last, t0 + span).await;
    last = l;
    let idle_statements = STATEMENTS.load(Ordering::Relaxed) - s0;
    let idle_cpu = cpu().zip(c0).map(|(a, b)| a - b);
    let idle_secs = t0.elapsed().as_secs_f64();

    // typing: a heartbeat every 500 ms for 10 s, the device syncing on for 1.5 s more
    let beats_for = Duration::from_secs(10);
    let (s1, c1, t1) = (STATEMENTS.load(Ordering::Relaxed), cpu(), Instant::now());
    let watcher = tokio::spawn(device(hub.clone(), ann.clone(), "ann-desk", last, t1 + beats_for + Duration::from_millis(1500)));
    let mut beats: Vec<Instant> = Vec::new();
    let mut tick = tokio::time::interval(mailhub::typing::BEAT);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    while t1.elapsed() < beats_for {
        tick.tick().await;
        let at = Instant::now();
        let r = Call::new("POST", "/api/typing").auth(pair(&org.0, &org.1)).json(json!({ "to": [&ann.0] })).send(&hub).await;
        assert_eq!(r.code(), 200, "{}", r.text());
        assert_eq!(r.json()["counted"], json!(1), "{}", r.text());
        beats.push(at);
    }
    let (typing, _) = watcher.await.unwrap();
    let typing_statements = STATEMENTS.load(Ordering::Relaxed) - s1;
    let typing_cpu = cpu().zip(c1).map(|(a, b)| a - b);
    let typing_secs = t1.elapsed().as_secs_f64();

    // what the receiver saw
    let relays: Vec<Instant> = typing.relays.iter().map(|r| r.0).collect();
    let mut gaps: Vec<f64> = relays.windows(2).map(|w| ms(w[1] - w[0])).collect();
    let max_gap = gaps.iter().copied().fold(0.0, f64::max);
    let mut delays: Vec<f64> = beats.iter().filter_map(|b| relays.iter().find(|r| *r >= b).map(|r| ms(*r - *b))).collect();
    let last_beat = *beats.last().unwrap();
    let after_last = relays.iter().filter(|r| **r > last_beat).map(|r| ms(*r - last_beat)).fold(0.0, f64::max);
    let mut sizes: Vec<f64> = typing.relays.iter().map(|r| r.1 as f64).collect();
    let idle_rate = idle_statements as f64 / idle_secs;
    let extra = typing_statements as f64 - idle_rate * typing_secs;
    let n = typing.relays.len().max(1) as f64;

    println!("  relay load (in-process, one sender, one device in use)");
    println!("    idle:   {} syncs, {idle_statements} statements in {idle_secs:.1} s ({idle_rate:.2}/s), CPU {idle_cpu:?}", idle.syncs);
    println!(
        "    typing: {} heartbeats, {} syncs ({} answered typing-only), {} relays, {typing_statements} statements in {typing_secs:.1} s, CPU {typing_cpu:?}",
        beats.len(),
        typing.syncs,
        typing.typing_only,
        typing.relays.len(),
    );
    println!("    beyond idle: {:.1} statements per heartbeat relayed ({:.1}/s)", extra / n, extra / typing_secs);
    if let (Some(i), Some(t)) = (idle_cpu, typing_cpu) {
        println!("    CPU beyond idle: {:.2} ms per relay ({:.2}% of one core)", (ms(t) - ms(i) * typing_secs / idle_secs) / n, (ms(t) / typing_secs - ms(i) / idle_secs) / 10.0);
    }
    println!(
        "    heartbeat to relay: median {:.1} ms, p95 {:.1} ms, max {:.1} ms; gap between relays: median {:.0} ms, max {max_gap:.0} ms",
        quantile(&mut delays.clone(), 0.5),
        quantile(&mut delays, 0.95),
        delays.iter().copied().fold(0.0, f64::max),
        quantile(&mut gaps, 0.5),
    );
    println!("    relay answer size: median {:.0} bytes; last relay {after_last:.0} ms after the last heartbeat", quantile(&mut sizes, 0.5));

    assert!(typing.relays.len() * 10 >= beats.len() * 9, "each heartbeat reaches the device: {} of {}", typing.relays.len(), beats.len());
    assert!(max_gap < 1000.0, "the indicator would drop between heartbeats: a {max_gap:.0} ms gap");
    assert!(after_last < 1000.0, "a relay {after_last:.0} ms after the last heartbeat");
    assert!(typing.typing_only * 10 >= typing.relays.len() as u64 * 9, "relays come typing-only: {} of {}", typing.typing_only, typing.relays.len());
    assert!(extra / n < 14.0, "{:.1} statements per relay", extra / n);
}
