//! The mail hub, adversarially — v1's `tests/test_hub.py`, ported check for
//! check (same sections, same labels, same order) to the v2 hub.
//!
//! The hub's whole security model is two sentences — *joining is open, but
//! ADDRESSES are owned*, and *the direction of the connection is the
//! boundary* — so the checks that matter attack address ownership and the
//! custody rules, not the happy paths. The sharpest is the SUFFIX ATTACK: an
//! address ends in the first 6 hex characters of its fingerprint, a 24-bit
//! space a laptop collides in milliseconds; §2 builds the collision and
//! proves the hub refuses it.
//!
//!     §1  registration — first write wins, ownership, display refresh, slugs
//!     §2  ☞ the suffix attack, constructed for real
//!     §3  poll — multiplexing, partial credentials, ordering
//!     §4  custody — redelivery until ack, and only the recipient may ack
//!     §5  receipts — monotonic, one-sided, and the pushed lifecycle
//!     §6  send — idempotency, unknown recipient, truncation, caps
//!     §7  attachments — ownership, binding, download rights, size, blob paths
//!     §8  retention — the sweep removes row, metadata AND blob
//!     §9  the read-only UI and healthz
//!     §10 FR-10 the public face
//!     §11 the client group filter (+ §11b paging, §11c the group key)
//!
//! Hermetic: a throwaway database and data folder per run, requests driven
//! in-process (no socket), the sweep invoked directly.
//!
//!     HUB_TEST_PG=<folder> cargo test --test hub_suite -- --nocapture

mod support;

use std::alloc::{GlobalAlloc, Layout, System};
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use axum::body::Body;
use bytes::Bytes;
use futures::{FutureExt, StreamExt};
use mailhub::Hub;
use serde_json::{json, Value};
use support::*;

// ------------------------------------------------- measured peak allocation
// v1 measured its streamed upload with tracemalloc; here every allocation in
// the process is counted, on every thread.

struct Counting;
static CURRENT: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let p = System.alloc(l);
        if !p.is_null() {
            let now = CURRENT.fetch_add(l.size(), Ordering::Relaxed) + l.size();
            PEAK.fetch_max(now, Ordering::Relaxed);
        }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        System.dealloc(p, l);
        CURRENT.fetch_sub(l.size(), Ordering::Relaxed);
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, new: usize) -> *mut u8 {
        let q = System.realloc(p, l, new);
        if !q.is_null() {
            if new > l.size() {
                let now = CURRENT.fetch_add(new - l.size(), Ordering::Relaxed) + (new - l.size());
                PEAK.fetch_max(now, Ordering::Relaxed);
            } else {
                CURRENT.fetch_sub(l.size() - new, Ordering::Relaxed);
            }
        }
        q
    }
}

#[global_allocator]
static ALLOC: Counting = Counting;

// ------------------------------------------------------------------ harness

macro_rules! ensure {
    ($cond:expr) => {
        if !($cond) {
            return Err(format!("assertion failed: {}", stringify!($cond)));
        }
    };
    ($cond:expr, $($arg:tt)+) => {
        if !($cond) {
            return Err(format!($($arg)+));
        }
    };
}

type Check = Result<(), String>;

#[derive(Default)]
struct Tally {
    pass: usize,
    fail: Vec<(String, String)>,
}

impl Tally {
    async fn check(&mut self, label: &str, f: impl Future<Output = Check>) {
        match AssertUnwindSafe(f).catch_unwind().await {
            Ok(Ok(())) => {
                self.pass += 1;
                println!("  ok {:3}  {label}", self.pass);
            }
            Ok(Err(e)) => {
                println!("  FAIL     {label}");
                self.fail.push((label.to_string(), e));
            }
            Err(p) => {
                let msg = p.downcast_ref::<String>().cloned().or_else(|| p.downcast_ref::<&str>().map(|s| s.to_string()));
                println!("  FAIL     {label} (panicked)");
                self.fail.push((label.to_string(), format!("panic: {}", msg.unwrap_or_default())));
            }
        }
    }
}

type Org = (String, String);

struct Ctx {
    hub: Arc<Hub>,
    url: String,
    data: PathBuf,
    db: tokio_postgres::Client,
    n: AtomicUsize,
}

impl Ctx {
    /// Register a fresh org; returns (slug, secret).
    async fn new_org(&self, name: &str, blurb: &str) -> Org {
        let n = self.n.fetch_add(1, Ordering::Relaxed) + 1;
        let slug = format!("zz{n}.tester.{}", token_hex3());
        let secret = token_hex16();
        let org_name = if name.is_empty() { format!("Org {n}") } else { name.to_string() };
        let r = Call::new("POST", "/api/register")
            .auth(pair(&slug, &secret))
            .json(json!({ "slug": slug, "org_name": org_name, "username": "tester", "blurb": blurb }))
            .send(&self.hub)
            .await;
        assert_eq!(r.code(), 200, "{}", r.text());
        (slug, secret)
    }

    async fn org(&self) -> Org {
        self.new_org("", "").await
    }

    async fn send(&self, frm: &Org, to: &str, body: &str, extra: Value) -> Resp {
        let mut payload = json!({ "to": to, "body": body });
        if let Value::Object(e) = extra {
            for (k, v) in e {
                payload[k] = v;
            }
        }
        Call::new("POST", "/api/send").auth(pair(&frm.0, &frm.1)).json(payload).send(&self.hub).await
    }

    async fn poll(&self, creds: &[&Org]) -> Value {
        let auth = creds.iter().map(|c| pair(&c.0, &c.1)).collect::<Vec<_>>().join(" ");
        let r = Call::new("POST", "/api/poll?wait=0").auth(auth).json(json!({})).send(&self.hub).await;
        assert_eq!(r.code(), 200, "{}", r.text());
        r.json()
    }

    async fn ack(&self, who: &Org, ids: Value) -> Resp {
        Call::new("POST", "/api/ack").auth(pair(&who.0, &who.1)).json(json!({ "ids": ids })).send(&self.hub).await
    }

    async fn receipts(&self, who: &Org, receipts: Value) -> Resp {
        Call::new("POST", "/api/receipts").auth(pair(&who.0, &who.1)).json(json!({ "receipts": receipts })).send(&self.hub).await
    }

    async fn upload(&self, who: &Org, data: &'static [u8], name: &str) -> String {
        let path = format!("/api/attachments?name={}", percent_encoding::utf8_percent_encode(name, percent_encoding::NON_ALPHANUMERIC));
        let r = Call::new("POST", &path).auth(pair(&who.0, &who.1)).content(Body::from(data)).send(&self.hub).await;
        assert_eq!(r.code(), 200, "{}", r.text());
        r.json()["id"].as_str().unwrap().to_string()
    }

    async fn rows(&self, sql: &str, params: &[&(dyn tokio_postgres::types::ToSql + Sync)]) -> Vec<tokio_postgres::Row> {
        self.db.query(sql, params).await.unwrap_or_else(|e| panic!("{sql}: {e}"))
    }

    fn blob_dir(&self) -> PathBuf {
        self.data.join("blobs")
    }
}

fn old_stamp(days: i64) -> chrono::DateTime<chrono::Utc> {
    chrono::Utc::now() - chrono::Duration::days(days)
}

fn ids_of(v: &Value) -> Vec<String> {
    v.as_array().map(|a| a.iter().map(|m| m["id"].as_str().unwrap_or("").to_string()).collect()).unwrap_or_default()
}

fn bodies_of(v: &Value) -> Vec<String> {
    v.as_array().map(|a| a.iter().map(|m| m["body"].as_str().unwrap_or("").to_string()).collect()).unwrap_or_default()
}

// ------------------------------------------------------ SQL statement capture
// §11 measures which statements an endpoint runs: every statement passes
// through the hub's `hub::sql` debug log, which this layer records.

struct Capture(Arc<Mutex<Vec<String>>>);

struct MsgVisitor(String);

impl tracing::field::Visit for MsgVisitor {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.0 = format!("{value:?}");
        }
    }
}

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Capture {
    fn on_event(&self, e: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
        if e.metadata().target() == "hub::sql" {
            let mut v = MsgVisitor(String::new());
            e.record(&mut v);
            self.0.lock().unwrap().push(v.0);
        }
    }
}

fn capture_sql() -> (Arc<Mutex<Vec<String>>>, tracing::subscriber::DefaultGuard) {
    use tracing_subscriber::layer::SubscriberExt;
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sub = tracing_subscriber::registry().with(Capture(seen.clone()));
    (seen, tracing::subscriber::set_default(sub))
}

// ===================================================================== §1
async fn sec_register(t: &mut Tally, c: &Ctx) {
    println!("\n§1  registration — the address is owned, not just claimed");

    t.check("a second identity cannot take a registered address", async {
        let (slug, _) = c.new_org("First Org", "").await;
        let r = Call::new("POST", "/api/register")
            .auth(pair(&slug, &token_hex16()))
            .json(json!({ "slug": slug, "org_name": "Impostor" }))
            .send(&c.hub)
            .await;
        ensure!(r.code() == 403 && r.text().contains("owned"), "{} {}", r.code(), r.text());
        let rows = c.rows("SELECT org_name FROM identities WHERE slug = $1", &[&slug]).await;
        ensure!(rows[0].get::<_, String>(0) == "First Org", "the refused registration still rewrote the display fields");
        Ok(())
    })
    .await;

    t.check("re-registering with the right secret refreshes the display fields", async {
        let (slug, secret) = c.new_org("Before", "old blurb").await;
        let r = Call::new("POST", "/api/register")
            .auth(pair(&slug, &secret))
            .json(json!({ "slug": slug, "org_name": "After", "username": "renamed", "blurb": "new blurb" }))
            .send(&c.hub)
            .await;
        ensure!(r.code() == 200, "{}", r.text());
        let got = &c.rows("SELECT org_name, username, blurb, fingerprint FROM identities WHERE slug = $1", &[&slug]).await[0];
        let fields: (String, String, String) = (got.get(0), got.get(1), got.get(2));
        ensure!(fields == ("After".into(), "renamed".into(), "new blurb".into()), "{fields:?}");
        ensure!(got.get::<_, String>(3) == sha256_hex(&secret));
        Ok(())
    })
    .await;

    t.check("registration answers with the hub's name, retention and roster", async {
        let (slug, secret) = c.org().await;
        let r = Call::new("POST", "/api/register").auth(pair(&slug, &secret)).json(json!({ "slug": slug })).send(&c.hub).await;
        let j = r.json();
        ensure!(j["ok"] == json!(true) && j["name"] == "test-hub" && j["retention_days"] == 30, "{j}");
        let me = j["roster"].as_array().and_then(|a| a.iter().find(|o| o["slug"] == json!(slug)).cloned());
        ensure!(me.is_some(), "{}", j["roster"]);
        ensure!(me.unwrap()["online"] == json!(true), "a just-registered org is present");
        Ok(())
    })
    .await;

    t.check("registration without the auth header is refused", async {
        let slug = "zz.nobody.aaaaaa";
        let r = Call::new("POST", "/api/register").json(json!({ "slug": slug })).send(&c.hub).await;
        ensure!(r.code() == 401, "{}", r.text());
        ensure!(c.rows("SELECT 1 FROM identities WHERE slug = $1", &[&slug]).await.is_empty());
        Ok(())
    })
    .await;

    t.check("a credential for a DIFFERENT slug does not register this one", async {
        // the header may carry several pairs; only the one matching the
        // body's slug counts, or an attacker could register X while
        // presenting their own credential for Y
        let victim = "zz.victim.aaaaaa";
        let (mine, sec) = c.org().await;
        let r = Call::new("POST", "/api/register").auth(pair(&mine, &sec)).json(json!({ "slug": victim })).send(&c.hub).await;
        ensure!(r.code() == 401, "{}", r.text());
        ensure!(c.rows("SELECT 1 FROM identities WHERE slug = $1", &[&victim]).await.is_empty());
        Ok(())
    })
    .await;

    t.check("malformed slugs are refused", async {
        let long = "a".repeat(129);
        for bad in ["", "   ", "UPPER.case.aaaaaa", ".leading.dot", "-leading-dash", "has space", long.as_str()] {
            let who = if bad.is_empty() { "x" } else { bad };
            let r = Call::new("POST", "/api/register").auth(pair(who, &"s".repeat(32))).json(json!({ "slug": bad })).send(&c.hub).await;
            ensure!(r.code() == 401 || r.code() == 422, "{bad:?} → {}", r.code());
        }
        // ⚠ a non-ASCII slug cannot even be presented: the credential rides
        // an HTTP HEADER, so the header carries an ASCII stand-in and the
        // body's slug is what is judged
        let r = Call::new("POST", "/api/register")
            .auth(pair("x", &"s".repeat(32)))
            .json(json!({ "slug": "emoji.\u{1f600}.aaaaaa" }))
            .send(&c.hub)
            .await;
        ensure!(r.code() == 401 || r.code() == 422, "{}", r.code());
        ensure!(c.rows("SELECT 1 FROM identities WHERE slug LIKE 'emoji%'", &[]).await.is_empty());
        Ok(())
    })
    .await;

    t.check("a 128-character slug is still legal (the boundary itself)", async {
        let ok = "a".repeat(128);
        let r = Call::new("POST", "/api/register").auth(pair(&ok, &token_hex16())).json(json!({ "slug": ok })).send(&c.hub).await;
        ensure!(r.code() == 200, "{}", r.text());
        Ok(())
    })
    .await;
}

// ===================================================================== §2
async fn sec_suffix_attack(t: &mut Tally, c: &Ctx) {
    println!("\n§2  ☞ the suffix attack — constructed, not imagined");

    /// Two secrets whose fingerprints share their first 6 hex characters:
    /// two identities with the SAME visible address suffix. Birthday-bounded,
    /// ~2^12 tries for 24 bits.
    fn collide() -> (String, String) {
        let mut seen: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        for _ in 0..400_000 {
            let s = token_hex16();
            let p = sha256_hex(&s)[..6].to_string();
            if let Some(prev) = seen.get(&p) {
                if *prev != s {
                    return (prev.clone(), s);
                }
            }
            seen.insert(p, s);
        }
        panic!("no collision found — the search bound is wrong");
    }

    t.check("a fingerprint that shares the 6-char SUFFIX is still refused", async {
        let (a, b) = collide();
        let (fa, fb) = (sha256_hex(&a), sha256_hex(&b));
        ensure!(fa[..6] == fb[..6] && fa != fb, "the fixture did not collide");
        let slug = format!("zzattack.tester.{}", &fa[..6]);
        let r = Call::new("POST", "/api/register").auth(pair(&slug, &a)).json(json!({ "slug": slug, "org_name": "Holder" })).send(&c.hub).await;
        ensure!(r.code() == 200, "{}", r.text());
        // the attacker's secret produces the SAME 6-character display suffix
        let r = Call::new("POST", "/api/register").auth(pair(&slug, &b)).json(json!({ "slug": slug, "org_name": "Thief" })).send(&c.hub).await;
        ensure!(r.code() == 403, "AN ADDRESS WAS STOLEN with a 6-character fingerprint collision — verification must compare the FULL digest");
        ensure!(c.rows("SELECT org_name FROM identities WHERE slug = $1", &[&slug]).await[0].get::<_, String>(0) == "Holder");
        // …and it must not authenticate anywhere else either
        for (method, path, body) in [
            ("POST", "/api/poll", Some(json!({}))),
            ("POST", "/api/ack", Some(json!({ "ids": [] }))),
            ("POST", "/api/receipts", Some(json!({ "receipts": [] }))),
            ("GET", "/api/roster", None),
        ] {
            let mut call = Call::new(method, path).auth(pair(&slug, &b));
            if let Some(j) = body {
                call = call.json(j);
            }
            let got = call.send(&c.hub).await;
            ensure!(got.code() == 401, "{path} accepted the collision: {}", got.code());
        }
        Ok(())
    })
    .await;

    t.check("neither the fingerprint itself nor a prefix works as the secret", async {
        let (slug, secret) = c.org().await;
        let fp = sha256_hex(&secret);
        for wrong in [fp.clone(), fp[..6].to_string(), secret[..secret.len() - 1].to_string(), format!("{secret}0"), secret.to_uppercase()] {
            let r = Call::new("GET", "/api/roster").auth(pair(&slug, &wrong)).send(&c.hub).await;
            ensure!(r.code() == 401, "{}… authenticated as {slug}", &wrong[..wrong.len().min(12)]);
        }
        Ok(())
    })
    .await;
}

// ===================================================================== §3
async fn sec_poll(t: &mut Tally, c: &Ctx) {
    println!("\n§3  poll — multiplexed, partial-credential tolerant, ordered");

    t.check("one header, two orgs, both queues in one answer", async {
        let (a, b, s) = (c.org().await, c.org().await, c.org().await);
        c.send(&s, &a.0, "for a", json!({})).await;
        c.send(&s, &b.0, "for b", json!({})).await;
        let got = c.poll(&[&a, &b]).await;
        let mut tos: Vec<String> = got["messages"].as_array().unwrap().iter().map(|m| m["to"].as_str().unwrap().to_string()).collect();
        tos.sort();
        let mut want = vec![a.0.clone(), b.0.clone()];
        want.sort();
        ensure!(tos == want, "{}", got["messages"]);
        Ok(())
    })
    .await;

    t.check("an invalid pair does not spoil the valid one", async {
        let a = c.org().await;
        let bad = ("zz.nosuch.aaaaaa".to_string(), token_hex16());
        let s = c.org().await;
        c.send(&s, &a.0, "still delivered", json!({})).await;
        let got = c.poll(&[&a, &bad]).await;
        ensure!(bodies_of(&got["messages"]) == vec!["still delivered"], "{got}");
        // a KNOWN slug with the WRONG secret is the same case
        let wrong = (s.0.clone(), token_hex16());
        let got2 = c.poll(&[&a, &wrong]).await;
        ensure!(got2["messages"].as_array().unwrap().iter().all(|m| m["to"] == json!(a.0)));
        Ok(())
    })
    .await;

    t.check("no valid credential at all is a 401", async {
        let r = Call::new("POST", "/api/poll?wait=0").auth(pair("zz.nope.aaaaaa", &"x".repeat(32))).json(json!({})).send(&c.hub).await;
        ensure!(r.code() == 401, "{}", r.text());
        let r = Call::new("POST", "/api/poll?wait=0").json(json!({})).send(&c.hub).await;
        ensure!(r.code() == 401, "{}", r.text());
        Ok(())
    })
    .await;

    t.check("messages come back in hub-clock order", async {
        let (me, s) = (c.org().await, c.org().await);
        for i in 0..5 {
            c.send(&s, &me.0, &format!("m{i}"), json!({})).await;
        }
        let got = c.poll(&[&me]).await;
        ensure!(bodies_of(&got["messages"]) == (0..5).map(|i| format!("m{i}")).collect::<Vec<_>>(), "received_at (the hub clock) is the ordering authority");
        Ok(())
    })
    .await;

    t.check("the envelope carries the sender's claim and the hub's own clock", async {
        let (me, s) = (c.org().await, c.org().await);
        c.send(&s, &me.0, "shaped", json!({ "kind": "status", "sent_at": "2020-01-01T00:00:00Z" })).await;
        let m = c.poll(&[&me]).await["messages"][0].clone();
        let keys: Vec<&str> = m.as_object().unwrap().keys().map(String::as_str).collect();
        ensure!(keys == ["id", "from", "to", "body", "kind", "thread_id", "sent_at", "received_at", "attachments"], "{keys:?}");
        ensure!(m["from"] == json!(s.0) && m["kind"] == "status");
        ensure!(m["sent_at"] == "2020-01-01T00:00:00Z" && m["sent_at"] != m["received_at"], "sent_at is the sender's CLAIM and must not overwrite the hub's own received_at");
        Ok(())
    })
    .await;

    t.check("every poll carries the roster and the hub's name", async {
        let me = c.org().await;
        let got = c.poll(&[&me]).await;
        ensure!(got["roster"].as_array().unwrap().iter().any(|o| o["slug"] == json!(me.0)));
        ensure!(got["name"] == "test-hub");
        Ok(())
    })
    .await;
}

// ===================================================================== §4
async fn sec_custody(t: &mut Tally, c: &Ctx) {
    println!("\n§4  custody — at-least-once until the recipient acks");

    t.check("a message is redelivered until it is acked", async {
        let (me, s) = (c.org().await, c.org().await);
        let mid = c.send(&s, &me.0, "keep me", json!({})).await.json()["id"].as_str().unwrap().to_string();
        for attempt in 0..3 {
            let got = c.poll(&[&me]).await;
            ensure!(ids_of(&got["messages"]) == vec![mid.clone()], "attempt {attempt}: an un-acked message must be redelivered — the recipient may have died before persisting it");
        }
        let r = c.ack(&me, json!([mid])).await;
        ensure!(r.json()["acked"] == 1, "{}", r.text());
        ensure!(c.poll(&[&me]).await["messages"] == json!([]), "an acked message stops coming back");
        Ok(())
    })
    .await;

    t.check("only the recipient may take custody", async {
        let (me, s) = (c.org().await, c.org().await);
        let mid = c.send(&s, &me.0, "not yours to ack", json!({})).await.json()["id"].as_str().unwrap().to_string();
        ensure!(c.ack(&s, json!([mid])).await.json()["acked"] == 0, "the SENDER acked its own message");
        ensure!(ids_of(&c.poll(&[&me]).await["messages"]) == vec![mid.clone()], "…and the recipient still has it");
        let third = c.org().await;
        ensure!(c.ack(&third, json!([mid])).await.json()["acked"] == 0, "a third party acked someone's message");
        Ok(())
    })
    .await;

    t.check("acking an unknown id, or the same id twice, changes nothing", async {
        let (me, s) = (c.org().await, c.org().await);
        let mid = c.send(&s, &me.0, "hello", json!({})).await.json()["id"].as_str().unwrap().to_string();
        ensure!(c.ack(&me, json!(["no-such-id"])).await.json()["acked"] == 0);
        ensure!(c.ack(&me, json!([mid])).await.json()["acked"] == 1);
        ensure!(c.ack(&me, json!([mid])).await.json()["acked"] == 0, "a second ack of the same message must be a no-op");
        Ok(())
    })
    .await;
}

// ===================================================================== §5
async fn sec_receipts(t: &mut Tally, c: &Ctx) {
    println!("\n§5  receipts — one-sided, monotonic, pushed once");

    t.check("fetched → delivered → read, each pushed exactly once", async {
        let (me, s) = (c.org().await, c.org().await);
        let mid = c.send(&s, &me.0, "track me", json!({})).await.json()["id"].as_str().unwrap().to_string();
        ensure!(c.poll(&[&s]).await["receipts"] == json!([]), "nothing owed before an ack");
        c.ack(&me, json!([mid])).await;
        let got = c.poll(&[&s]).await["receipts"].clone();
        ensure!(ids_of(&got) == vec![mid.clone()] && got[0]["state"] == "fetched", "{got}");
        ensure!(c.poll(&[&s]).await["receipts"] == json!([]), "a receipt already pushed must not repeat forever");
        c.receipts(&me, json!([{ "id": mid, "state": "delivered" }])).await;
        let got = c.poll(&[&s]).await["receipts"].clone();
        ensure!(got[0]["state"] == "delivered" && got[0]["delivered_at"].is_string(), "{got}");
        c.receipts(&me, json!([{ "id": mid, "state": "read" }])).await;
        let got = c.poll(&[&s]).await["receipts"].clone();
        ensure!(got[0]["state"] == "read" && got[0]["read_at"].is_string(), "{got}");
        Ok(())
    })
    .await;

    t.check("a state already recorded is never overwritten", async {
        let (me, s) = (c.org().await, c.org().await);
        let mid = c.send(&s, &me.0, "hello", json!({})).await.json()["id"].as_str().unwrap().to_string();
        c.ack(&me, json!([mid])).await;
        let first = c.receipts(&me, json!([{ "id": mid, "state": "delivered", "at": "2026-01-01T00:00:00Z" }])).await;
        ensure!(first.json()["recorded"] == 1);
        let again = c.receipts(&me, json!([{ "id": mid, "state": "delivered", "at": "2030-01-01T00:00:00Z" }])).await;
        ensure!(again.json()["recorded"] == 0, "a receipt moved backwards");
        let got: Option<String> = c.rows("SELECT delivered_at FROM messages WHERE id = $1", &[&mid]).await[0].get(0);
        ensure!(got.as_deref() == Some("2026-01-01T00:00:00Z"), "{got:?}");
        Ok(())
    })
    .await;

    t.check("only the recipient's side may record delivery or reading", async {
        let (me, s) = (c.org().await, c.org().await);
        let mid = c.send(&s, &me.0, "hello", json!({})).await.json()["id"].as_str().unwrap().to_string();
        ensure!(c.receipts(&s, json!([{ "id": mid, "state": "read" }])).await.json()["recorded"] == 0, "the SENDER recorded a read receipt for its own message");
        let third = c.org().await;
        ensure!(c.receipts(&third, json!([{ "id": mid, "state": "read" }])).await.json()["recorded"] == 0, "a third party recorded a receipt");
        let got: Option<String> = c.rows("SELECT read_at FROM messages WHERE id = $1", &[&mid]).await[0].get(0);
        ensure!(got.is_none());
        Ok(())
    })
    .await;

    t.check("unknown receipt states are ignored, not guessed at", async {
        let (me, s) = (c.org().await, c.org().await);
        let mid = c.send(&s, &me.0, "hello", json!({})).await.json()["id"].as_str().unwrap().to_string();
        let r = c
            .receipts(&me, json!([{ "id": mid, "state": "received" }, { "id": mid, "state": "" }, { "id": mid, "state": "DELIVERED" }, { "id": mid }]))
            .await;
        ensure!(r.json()["recorded"] == 0, "only 'delivered' and 'read' are recordable states");
        Ok(())
    })
    .await;

    t.check("receipts_pushed is set by a state change and cleared by the poll", async {
        let (me, s) = (c.org().await, c.org().await);
        let mid = c.send(&s, &me.0, "hello", json!({})).await.json()["id"].as_str().unwrap().to_string();
        let pushed = |rows: Vec<tokio_postgres::Row>| rows[0].get::<_, bool>(0);
        ensure!(pushed(c.rows("SELECT receipts_pushed FROM messages WHERE id = $1", &[&mid]).await), "nothing owed at send");
        c.ack(&me, json!([mid])).await;
        ensure!(!pushed(c.rows("SELECT receipts_pushed FROM messages WHERE id = $1", &[&mid]).await), "the ack owes an update");
        c.poll(&[&s]).await;
        ensure!(pushed(c.rows("SELECT receipts_pushed FROM messages WHERE id = $1", &[&mid]).await), "the poll settled it");
        Ok(())
    })
    .await;
}

// ===================================================================== §6
async fn sec_send(t: &mut Tally, c: &Ctx) {
    println!("\n§6  send — idempotent, addressed, bounded");

    t.check("re-sending the same id is idempotent and keeps the first receipt", async {
        let (me, s) = (c.org().await, c.org().await);
        let mid = format!("fixed-id-{}", &token_hex16()[..8]);
        let first = c.send(&s, &me.0, "once", json!({ "id": mid })).await.json();
        let again = c.send(&s, &me.0, "DIFFERENT BODY", json!({ "id": mid })).await.json();
        ensure!(first["duplicate"] == json!(false) && again["duplicate"] == json!(true), "{first} {again}");
        ensure!(again["received_at"] == first["received_at"], "a retry must report the ORIGINAL receipt time — the sender uses it to order its own outbox");
        ensure!(c.rows("SELECT id FROM messages WHERE id = $1", &[&mid]).await.len() == 1);
        ensure!(c.rows("SELECT body FROM messages WHERE id = $1", &[&mid]).await[0].get::<_, String>(0) == "once", "a retry rewrote the body");
        Ok(())
    })
    .await;

    t.check("mail to an unregistered address is refused at the door", async {
        let s = c.org().await;
        let r = c.send(&s, "zz.nobody.ffffff", "hello", json!({})).await;
        ensure!(r.code() == 422 && r.text().contains("no org registered"), "{}", r.text());
        Ok(())
    })
    .await;

    t.check("the `from` must be one of the presented credentials", async {
        let (me, s, victim) = (c.org().await, c.org().await, c.org().await);
        let r = c.send(&s, &me.0, "forged", json!({ "from": victim.0 })).await;
        ensure!(r.code() == 401, "an org sent mail AS another org");
        Ok(())
    })
    .await;

    t.check("sending with no credentials is refused", async {
        let me = c.org().await;
        let r = Call::new("POST", "/api/send").json(json!({ "to": me.0, "body": "x" })).send(&c.hub).await;
        ensure!(r.code() == 401, "{}", r.text());
        Ok(())
    })
    .await;

    t.check("the body is truncated at 20000", async {
        let (me, s) = (c.org().await, c.org().await);
        let mid = c.send(&s, &me.0, &"x".repeat(25000), json!({})).await.json()["id"].as_str().unwrap().to_string();
        let got: String = c.rows("SELECT body FROM messages WHERE id = $1", &[&mid]).await[0].get(0);
        ensure!(got.chars().count() == mailhub::api::mail::BODY_MAX && mailhub::api::mail::BODY_MAX == 20000, "{}", got.len());
        Ok(())
    })
    .await;

    t.check("at most 10 attachments per message", async {
        let (me, s) = (c.org().await, c.org().await);
        let atts: Vec<String> = (0..11).map(|i| format!("a{i}")).collect();
        let r = c.send(&s, &me.0, "many", json!({ "attachments": atts })).await;
        ensure!(r.code() == 422 && r.text().contains("at most"), "{}", r.text());
        Ok(())
    })
    .await;
}

// ===================================================================== §7
async fn sec_attachments(t: &mut Tally, c: &Ctx) {
    println!("\n§7  attachments — owned, bound once, downloadable by two parties");

    t.check("an upload lands as a real file plus an unbound row", async {
        let who = c.org().await;
        let aid = c.upload(&who, b"the bytes", "notes.txt").await;
        let p = c.blob_dir().join(&aid);
        ensure!(p.is_file() && std::fs::read(&p).unwrap() == b"the bytes");
        let row = &c.rows("SELECT owner_slug, bytes, message_id FROM attachments WHERE id = $1", &[&aid]).await[0];
        ensure!(row.get::<_, String>(0) == who.0 && row.get::<_, i64>(1) == 9);
        ensure!(row.get::<_, Option<String>>(2).is_none(), "an upload is not yet bound");
        Ok(())
    })
    .await;

    t.check("the declared filename is basenamed", async {
        let who = c.org().await;
        let aid = c.upload(&who, b"x", "../../etc/passwd").await;
        ensure!(c.rows("SELECT name FROM attachments WHERE id = $1", &[&aid]).await[0].get::<_, String>(0) == "passwd");
        Ok(())
    })
    .await;

    t.check("blob_path can never leave the blob directory", async {
        let root = c.blob_dir();
        for evil in ["../../etc/passwd", "..\\..\\win.ini", "a/b", "..", "%2e%2e", "....//....//x"] {
            if let Some(p) = mailhub::blobs::blob_path(&root, evil) {
                ensure!(p.parent() == Some(root.as_path()), "blob_path({evil:?}) escaped the blob directory: {}", p.display());
            }
        }
        Ok(())
    })
    .await;

    // v1 carried this as a known GAP (an id that sanitizes to nothing joined
    // onto the directory itself); v2 fixes it, so it is a plain check here
    t.check("an id that sanitizes to nothing does not resolve to the directory (v1 gap, fixed in v2)", async {
        for empty_ish in ["..", "///", "%%%", "-", "."] {
            ensure!(mailhub::blobs::blob_path(&c.blob_dir(), empty_ish).is_none(), "blob_path({empty_ish:?}) resolved to a path");
        }
        Ok(())
    })
    .await;

    t.check("an attachment is owned by its uploader and binds to ONE message", async {
        let (me, s) = (c.org().await, c.org().await);
        let mine = c.upload(&s, b"payload", "notes.txt").await;
        let theirs = c.upload(&me, b"payload", "notes.txt").await;
        let r = c.send(&s, &me.0, "not mine", json!({ "attachments": [theirs] })).await;
        ensure!(r.code() == 422 && r.text().contains("unknown attachment"), "an org attached a file it does not own");
        let ok = c.send(&s, &me.0, "mine", json!({ "attachments": [mine] })).await;
        ensure!(ok.code() == 200, "{}", ok.text());
        let bound: Option<String> = c.rows("SELECT message_id FROM attachments WHERE id = $1", &[&mine]).await[0].get(0);
        ensure!(bound.as_deref() == ok.json()["id"].as_str());
        let r2 = c.send(&s, &me.0, "again", json!({ "attachments": [mine] })).await;
        ensure!(r2.code() == 422 && r2.text().contains("already bound"), "the same upload was bound to a second message");
        Ok(())
    })
    .await;

    t.check("owner and recipient may download; nobody else", async {
        let (me, s, third) = (c.org().await, c.org().await, c.org().await);
        let aid = c.upload(&s, b"secret bytes", "notes.txt").await;
        let path = format!("/api/attachments/{aid}");
        ensure!(Call::new("GET", &path).auth(pair(&s.0, &s.1)).send(&c.hub).await.code() == 200, "the owner may read");
        ensure!(Call::new("GET", &path).auth(pair(&me.0, &me.1)).send(&c.hub).await.code() == 403, "an unbound attachment is readable by a stranger");
        c.send(&s, &me.0, "here", json!({ "attachments": [aid] })).await;
        let got = Call::new("GET", &path).auth(pair(&me.0, &me.1)).send(&c.hub).await;
        ensure!(got.code() == 200 && &got.body[..] == b"secret bytes", "the recipient of the bound message may read it");
        ensure!(Call::new("GET", &path).auth(pair(&third.0, &third.1)).send(&c.hub).await.code() == 403, "a third party read someone else's attachment");
        ensure!(Call::new("GET", &path).send(&c.hub).await.code() == 401);
        Ok(())
    })
    .await;

    t.check("a missing row is 404; a swept blob is 410, not a crash", async {
        let who = c.org().await;
        ensure!(Call::new("GET", "/api/attachments/nosuchid").auth(pair(&who.0, &who.1)).send(&c.hub).await.code() == 404);
        let aid = c.upload(&who, b"payload", "notes.txt").await;
        std::fs::remove_file(c.blob_dir().join(&aid)).unwrap();
        let r = Call::new("GET", &format!("/api/attachments/{aid}")).auth(pair(&who.0, &who.1)).send(&c.hub).await;
        ensure!(r.code() == 410, "{} {}", r.code(), r.text());
        Ok(())
    })
    .await;

    t.check("an upload over the configured limit is refused", async {
        let limited = hub(&c.url, &c.data, &[("HUB_MAX_FILE_BYTES", "1024")]).await;
        let who = c.org().await;
        let r = Call::new("POST", "/api/attachments?name=oversize-proof.bin")
            .auth(pair(&who.0, &who.1))
            .content(Body::from(vec![b'x'; 1025]))
            .send(&limited)
            .await;
        ensure!(r.code() == 413, "{}", r.code());
        ensure!(c.rows("SELECT 1 FROM attachments WHERE name = 'oversize-proof.bin'", &[]).await.is_empty());
        Ok(())
    })
    .await;

    t.check("env default, live advertised override, bounded streaming, cleanup and per-upload snapshot", async {
        ensure!(c.hub.cfg.max_file_bytes == 1024 * 1024 * 1024);
        let mut vars: std::collections::HashMap<String, String> =
            [("HUB_DATABASE_URL", "postgres://x@localhost/y"), ("HUB_MAX_FILE_BYTES", "12345")].iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        ensure!(mailhub::Config::from_vars(&vars).unwrap().max_file_bytes == 12345);
        vars.remove("HUB_MAX_FILE_BYTES");
        ensure!(mailhub::Config::from_vars(&vars).unwrap().max_file_bytes == 1024 * 1024 * 1024);
        let config = c.data.join("upload-limit.json");
        let set_limit = |n: u64| {
            let tmp = config.with_extension("json.tmp");
            std::fs::write(&tmp, json!({ "max_attachment_bytes": n }).to_string()).unwrap();
            std::fs::rename(&tmp, &config).unwrap();
        };
        let live = hub(&c.url, &c.data, &[("HUB_RUNTIME_CONFIG_FILE", config.to_str().unwrap())]).await;
        let who = c.org().await;
        let auth = pair(&who.0, &who.1);
        set_limit(32 * 1024 * 1024);
        ensure!(Call::new("GET", "/healthz").send(&live).await.json()["max_attachment_bytes"] == 32 * 1024 * 1024);
        let chunks = futures::stream::iter(0..512).map(|_| Ok::<_, std::io::Error>(Bytes::from(vec![b'x'; 65536])));
        let base = CURRENT.load(Ordering::Relaxed);
        PEAK.store(base, Ordering::Relaxed);
        let r = Call::new("POST", "/api/attachments?name=streamed.bin").auth(auth.clone()).content(Body::from_stream(chunks)).send(&live).await;
        let peak = PEAK.load(Ordering::Relaxed).saturating_sub(base);
        ensure!(r.code() == 200, "{}", r.text());
        ensure!(r.json()["bytes"] == 32 * 1024 * 1024);
        let id = r.json()["id"].as_str().unwrap().to_string();
        ensure!(std::fs::metadata(c.blob_dir().join(&id)).unwrap().len() == 32 * 1024 * 1024);
        ensure!(peak < 8 * 1024 * 1024, "peak allocation during a 32 MiB upload: {peak} bytes");
        println!("           (measured: peak allocation {:.2} MiB during the 32 MiB streamed upload)", peak as f64 / 1048576.0);
        let listing = || {
            let mut v: Vec<String> = std::fs::read_dir(c.blob_dir()).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
            v.sort();
            v
        };
        let before = listing();
        set_limit(8);
        ensure!(Call::new("GET", "/healthz").send(&live).await.json()["max_attachment_bytes"] == 8);
        let consumed = Arc::new(AtomicUsize::new(0));
        let counter = consumed.clone();
        let oversize = futures::stream::iter(0..20).map(move |_| {
            counter.fetch_add(1, Ordering::Relaxed);
            Ok::<_, std::io::Error>(Bytes::from_static(b"1234"))
        });
        let r = Call::new("POST", "/api/attachments").auth(auth.clone()).content(Body::from_stream(oversize)).send(&live).await;
        ensure!(r.code() == 413 && consumed.load(Ordering::Relaxed) == 3, "{} {}", r.code(), consumed.load(Ordering::Relaxed));
        ensure!(listing() == before, "partial blob survived");
        let interrupted = futures::stream::iter(0..2).map(|i| {
            if i == 0 {
                Ok(Bytes::from_static(b"1234"))
            } else {
                Err(std::io::Error::other("upload disconnected"))
            }
        });
        let _ = Call::new("POST", "/api/attachments").auth(auth.clone()).content(Body::from_stream(interrupted)).send(&live).await;
        ensure!(listing() == before, "interrupted blob survived");
        set_limit(16);
        let cfg_path = config.clone();
        let changing = futures::stream::iter(0..2).map(move |i| {
            if i == 1 {
                let tmp = cfg_path.with_extension("json.tmp");
                std::fs::write(&tmp, json!({ "max_attachment_bytes": 4 }).to_string()).unwrap();
                std::fs::rename(&tmp, &cfg_path).unwrap();
            }
            Ok::<_, std::io::Error>(Bytes::from_static(b"12345678"))
        });
        let r = Call::new("POST", "/api/attachments").auth(auth.clone()).content(Body::from_stream(changing)).send(&live).await;
        ensure!(r.code() == 200 && r.json()["bytes"] == 16, "{}", r.text());
        let r = Call::new("POST", "/api/attachments").auth(auth.clone()).content(Body::from(&b"12345"[..])).send(&live).await;
        ensure!(r.code() == 413, "{}", r.text());
        let _ = std::fs::remove_file(&config);
        Ok(())
    })
    .await;

    t.check("uploading without credentials is refused", async {
        let r = Call::new("POST", "/api/attachments?name=f").content(Body::from(&b"x"[..])).send(&c.hub).await;
        ensure!(r.code() == 401, "{}", r.text());
        Ok(())
    })
    .await;
}

// ===================================================================== §8
async fn sec_retention(t: &mut Tally, c: &Ctx) {
    println!("\n§8  retention — the sweep takes row, metadata and blob together");

    t.check("aged rows, metadata and blobs all go; fresh ones stay", async {
        let (me, s) = (c.org().await, c.org().await);
        let old_id = c.send(&s, &me.0, "ancient", json!({})).await.json()["id"].as_str().unwrap().to_string();
        let aid = c.upload(&s, b"old blob", "old.bin").await;
        let fresh_id = c.send(&s, &me.0, "recent", json!({})).await.json()["id"].as_str().unwrap().to_string();
        let stamp = old_stamp(c.hub.cfg.retention_days + 1);
        c.db.execute("UPDATE messages SET received_at = $1 WHERE id = $2", &[&stamp, &old_id]).await.unwrap();
        c.db.execute("UPDATE attachments SET created_at = $1 WHERE id = $2", &[&stamp, &aid]).await.unwrap();
        let blob = c.blob_dir().join(&aid);
        ensure!(blob.is_file());
        mailhub::sweep::run_once(&c.hub).await.map_err(|e| e.to_string())?;
        ensure!(c.rows("SELECT 1 FROM messages WHERE id = $1", &[&old_id]).await.is_empty(), "the aged message survived the sweep");
        ensure!(c.rows("SELECT 1 FROM attachments WHERE id = $1", &[&aid]).await.is_empty(), "the aged attachment row survived");
        ensure!(!blob.exists(), "the BLOB FILE survived — the row is gone but the bytes are still on disk, which is the failure mode a retention promise cannot have");
        ensure!(!c.rows("SELECT 1 FROM messages WHERE id = $1", &[&fresh_id]).await.is_empty(), "the sweep took a message that was still within retention");
        Ok(())
    })
    .await;

    t.check("the cutoff is the retention window, not a day either side", async {
        let (me, s) = (c.org().await, c.org().await);
        let mid = c.send(&s, &me.0, "just inside", json!({})).await.json()["id"].as_str().unwrap().to_string();
        c.db.execute("UPDATE messages SET received_at = $1 WHERE id = $2", &[&old_stamp(c.hub.cfg.retention_days - 1), &mid]).await.unwrap();
        mailhub::sweep::run_once(&c.hub).await.map_err(|e| e.to_string())?;
        ensure!(!c.rows("SELECT 1 FROM messages WHERE id = $1", &[&mid]).await.is_empty(), "a message one day INSIDE the window was swept");
        Ok(())
    })
    .await;
}

// ===================================================================== §9
async fn sec_ui(t: &mut Tally, c: &Ctx) {
    println!("\n§9  the read-only UI and healthz");

    t.check("healthz counts orgs and the queue, with no credentials", async {
        let j = Call::new("GET", "/healthz").send(&c.hub).await.json();
        ensure!(j["ok"] == json!(true) && j["name"] == "test-hub" && j["retention_days"] == 30, "{j}");
        let before = j["queued"].as_i64().unwrap();
        let (me, s) = (c.org().await, c.org().await);
        c.send(&s, &me.0, "counted", json!({})).await;
        let after = Call::new("GET", "/healthz").send(&c.hub).await.json();
        ensure!(after["queued"].as_i64() == Some(before + 1), "{before} {after}");
        ensure!(after["orgs"].as_i64().unwrap() >= 2);
        Ok(())
    })
    .await;

    t.check("/ui/data lists every org with its queue depth", async {
        let (me, s) = (c.org().await, c.org().await);
        c.send(&s, &me.0, "for the panel", json!({})).await;
        let j = Call::new("GET", "/ui/data").send(&c.hub).await.json();
        ensure!(j["name"] == "test-hub" && j["retention_days"] == 30);
        let row = j["orgs"].as_array().unwrap().iter().find(|o| o["slug"] == json!(me.0)).cloned().unwrap();
        ensure!(row["queued"].as_i64().unwrap() >= 1);
        for k in ["slug", "org_name", "username", "blurb", "online", "last_seen", "queued"] {
            ensure!(row.get(k).is_some(), "{k} missing: {row}");
        }
        Ok(())
    })
    .await;

    t.check("/ui/messages shows the global view and filters by org", async {
        let (me, s) = (c.org().await, c.org().await);
        let mid = c.send(&s, &me.0, "visible to the operator", json!({})).await.json()["id"].as_str().unwrap().to_string();
        let all = Call::new("GET", "/ui/messages").send(&c.hub).await.json()["messages"].clone();
        ensure!(ids_of(&all).contains(&mid));
        let mine = Call::new("GET", &format!("/ui/messages?org={}", me.0)).send(&c.hub).await.json()["messages"].clone();
        ensure!(mine.as_array().unwrap().iter().all(|m| m["to"] == json!(me.0) || m["from"] == json!(me.0)));
        ensure!(ids_of(&mine).contains(&mid));
        let other = Call::new("GET", "/ui/messages?org=zz.nobody.zzzzzz").send(&c.hub).await.json()["messages"].clone();
        ensure!(other == json!([]));
        let m = all.as_array().unwrap().iter().find(|m| m["id"] == json!(mid)).cloned().unwrap();
        ensure!(m["state"] == "queued" && m.get("delivered_at").is_some() && m.get("read_at").is_some(), "{m}");
        Ok(())
    })
    .await;

    t.check("the UI limit is clamped to 1..500", async {
        ensure!(Call::new("GET", "/ui/messages?limit=1").send(&c.hub).await.json()["messages"].as_array().unwrap().len() == 1);
        for bad in ["0", "-5", "10000"] {
            let r = Call::new("GET", &format!("/ui/messages?limit={bad}")).send(&c.hub).await;
            let n = r.json()["messages"].as_array().map(|a| a.len()).unwrap_or(0);
            ensure!(r.code() == 200 && (1..=500).contains(&n), "{bad} {}", r.code());
        }
        Ok(())
    })
    .await;

    t.check("the index page is served from the packaged static file", async {
        let r = Call::new("GET", "/").send(&c.hub).await;
        ensure!(r.code() == 200 && r.text().contains('<'));
        Ok(())
    })
    .await;

    t.check("the operator UI needs no credentials (ruled, not accidental)", async {
        // asserted so that making it authenticated is a DELIBERATE change:
        // on a closed network hub access IS read access to everyone's mail
        for p in ["/healthz", "/ui/data", "/ui/messages"] {
            ensure!(Call::new("GET", p).send(&c.hub).await.code() == 200, "{p}");
        }
        Ok(())
    })
    .await;
}

// ==================================================================== §10
async fn sec_public_face(t: &mut Tally, c: &Ctx) {
    println!("\n§10  FR-10 the public face — what the tunnel exposes");

    t.check("public · /, /ui/data and /ui/messages 404 through the public listener while the private one still serves them", async {
        for path in ["/", "/ui/data", "/ui/messages"] {
            let r = Call::new("GET", path).public().send(&c.hub).await;
            ensure!(r.code() == 404, "{path} → {}", r.code());
        }
        ensure!(Call::new("GET", "/ui/messages").send(&c.hub).await.code() == 200);
        Ok(())
    })
    .await;

    t.check("public · /api/* and /healthz still work through it", async {
        let me = c.org().await;
        let r = Call::new("GET", "/api/roster").auth(pair(&me.0, &me.1)).public().send(&c.hub).await;
        ensure!(r.code() == 200 && !r.json()["roster"].as_array().unwrap().is_empty(), "{}", r.text());
        ensure!(Call::new("GET", "/healthz").public().send(&c.hub).await.code() == 200);
        Ok(())
    })
    .await;

    t.check("public · every route the wrapper admits refuses an unauthenticated caller (401/403) — enumerated, not assumed", async {
        for (method, path, body) in [
            ("POST", "/api/register", Some(json!({ "slug": "x.y.z" }))),
            ("POST", "/api/poll", Some(json!({}))),
            ("POST", "/api/ack", Some(json!({ "ids": [] }))),
            ("POST", "/api/send", Some(json!({ "to": "x", "body": "y" }))),
            ("POST", "/api/receipts", Some(json!({ "ids": [] }))),
            ("GET", "/api/roster", None),
            ("GET", "/api/attachments/deadbeef", None),
        ] {
            let mut call = Call::new(method, path).public();
            if let Some(j) = body {
                call = call.json(j);
            }
            let r = call.send(&c.hub).await;
            ensure!(r.code() == 401 || r.code() == 403, "{method} {path} answered {} with NO credentials through the public face", r.code());
        }
        Ok(())
    })
    .await;

    t.check("public · dot-segments, double slashes, case and prefix tricks all 404 without reaching the UI", async {
        for p in ["//ui/messages", "/api/../ui/messages", "/API/roster", "/UI/messages", "/healthz/../ui/messages", "/ui//messages", "/./ui/messages", "/healthz/", "/api", "/apiX/roster"] {
            let r = Call::new("GET", p).public().send(&c.hub).await;
            ensure!(r.code() == 404, "{p} → {}", r.code());
            ensure!(!String::from_utf8_lossy(&r.body[..r.body.len().min(200)]).contains("messages"), "{p}");
        }
        Ok(())
    })
    .await;

    t.check("public · attachment custody (owner or recipient only) is unchanged through the wrapper", async {
        let (owner, other, rcpt) = (c.org().await, c.org().await, c.org().await);
        let aid = c.upload(&owner, b"secret bytes", "plan.md").await;
        c.send(&owner, &rcpt.0, "with a file", json!({ "attachments": [{ "id": aid, "name": "plan.md" }] })).await;
        let path = format!("/api/attachments/{aid}");
        ensure!(Call::new("GET", &path).auth(pair(&owner.0, &owner.1)).public().send(&c.hub).await.code() == 200);
        ensure!(Call::new("GET", &path).auth(pair(&other.0, &other.1)).public().send(&c.hub).await.code() == 403, "a stranger read it");
        Ok(())
    })
    .await;

    // v1 drove ASGI scopes directly (websocket, unknown types, lifespan);
    // the HTTP-level equivalent is a WebSocket handshake, which neither
    // listener lets through to a route
    t.check("public · non-HTTP scopes never reach the inner app (websocket closed, unknown types dropped, lifespan admitted)", async {
        let me = c.org().await;
        for public in [true, false] {
            let mut call = Call::new("GET", "/api/poll")
                .auth(pair(&me.0, &me.1))
                .header("connection", "Upgrade")
                .header("upgrade", "websocket")
                .header("sec-websocket-version", "13")
                .header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==");
            if public {
                call = call.public();
            }
            let r = call.send(&c.hub).await;
            ensure!(r.code() == 403, "a WebSocket handshake through the {} listener answered {}", if public { "public" } else { "full" }, r.code());
        }
        Ok(())
    })
    .await;
}

// ==================================================================== §11
async fn sec_client_filter(t: &mut Tally, c: &Ctx) {
    println!("\n§11  the client group filter (UI wave 2026-08-05)");

    t.check("client · the group filter matches the username segment and excludes everything else", async {
        let (me, s) = (c.org().await, c.org().await);
        c.send(&s, &me.0, "for the tester group", json!({})).await;
        let j = Call::new("GET", "/ui/messages?client=tester").send(&c.hub).await.json();
        ensure!(bodies_of(&j["messages"]).iter().any(|b| b == "for the tester group"));
        let none = Call::new("GET", "/ui/messages?client=nobody").send(&c.hub).await.json();
        ensure!(none["messages"] == json!([]), "{none}");
        Ok(())
    })
    .await;

    t.check("client · a partial username matches nothing (exact segment, not a LIKE)", async {
        let j = Call::new("GET", "/ui/messages?client=test").send(&c.hub).await.json();
        ensure!(j["messages"] == json!([]), "a partial username matched — the filter is a substring after all");
        Ok(())
    })
    .await;

    t.check("client · the group filter bounds its read in SQL like its two neighbours", async {
        for i in 0..30 {
            let (me, s) = (c.org().await, c.org().await);
            c.send(&s, &me.0, &format!("bulk {i}"), json!({})).await;
        }
        let (seen, guard) = capture_sql();
        let r = Call::new("GET", "/ui/messages?client=tester&limit=5").send(&c.hub).await;
        drop(guard);
        ensure!(r.code() == 200 && r.json()["messages"].as_array().unwrap().len() == 5);
        let stmts = seen.lock().unwrap().clone();
        let selects: Vec<&String> = stmts.iter().filter(|x| x.to_uppercase().starts_with("SELECT") && x.to_uppercase().contains("FROM MESSAGES")).collect();
        ensure!(!selects.is_empty(), "{stmts:?}");
        ensure!(selects.iter().all(|x| x.to_uppercase().contains("LIMIT")), "the client filter runs an unbounded SELECT: {:?}", selects.last());
        Ok(())
    })
    .await;
}

// =================================================================== §11b
async fn sec_client_filter_paging(t: &mut Tally, c: &Ctx) {
    println!("\n§11b  the paged prefilter — completeness and the wildcard");

    t.check("paging · matches buried behind a full page of other clients' mail are still delivered (no under-delivery)", async {
        let target = format!("grp{}", &token_hex3()[..4]);
        let me = (format!("old.{target}.aaaaaa"), token_hex16());
        Call::new("POST", "/api/register").auth(pair(&me.0, &me.1)).json(json!({ "slug": me.0, "org_name": "Old", "username": target })).send(&c.hub).await;
        let s = c.org().await;
        for i in 0..3 {
            c.send(&s, &me.0, &format!("early {i}"), json!({})).await;
        }
        for i in 0..150 {
            let (a, b) = (c.org().await, c.org().await);
            c.send(&a, &b.0, &format!("noise {i}"), json!({})).await;
        }
        let j = Call::new("GET", &format!("/ui/messages?client={target}&limit=3")).send(&c.hub).await.json();
        let mut bodies = bodies_of(&j["messages"]);
        ensure!(bodies.len() == 3, "the filter returned {} of 3 existing matches — paging stopped before reaching them", bodies.len());
        bodies.sort();
        ensure!(bodies == ["early 0", "early 1", "early 2"], "{bodies:?}");
        Ok(())
    })
    .await;

    t.check("paging · the newest-first order survives the paging", async {
        let target = format!("ord{}", &token_hex3()[..4]);
        let me = (format!("o.{target}.bbbbbb"), token_hex16());
        Call::new("POST", "/api/register").auth(pair(&me.0, &me.1)).json(json!({ "slug": me.0, "org_name": "Ord", "username": target })).send(&c.hub).await;
        let s = c.org().await;
        for i in 0..5 {
            c.send(&s, &me.0, &format!("m{i}"), json!({})).await;
        }
        let j = Call::new("GET", &format!("/ui/messages?client={target}&limit=3")).send(&c.hub).await.json();
        ensure!(bodies_of(&j["messages"]) == ["m4", "m3", "m2"], "{j}");
        Ok(())
    })
    .await;

    t.check("wildcard · a LIKE metacharacter in the client name cannot re-create the unbounded walk", async {
        let rows_before = c.rows("SELECT id FROM messages", &[]).await.len();
        ensure!(rows_before > 150, "{rows_before}");
        let (seen, guard) = capture_sql();
        let r = Call::new("GET", "/ui/messages?client=%25&limit=5").send(&c.hub).await;
        drop(guard);
        ensure!(r.code() == 200 && r.json()["messages"] == json!([]), "{}", r.text());
        let likes = seen.lock().unwrap().iter().filter(|s| s.to_uppercase().contains("LIKE")).count();
        ensure!(likes <= 1, "a client name of '%' made a prefilter match everything and page the whole table ({likes} pages)");
        Ok(())
    })
    .await;
}

// =================================================================== §11c
async fn sec_group_key(t: &mut Tally, c: &Ctx) {
    println!("\n§11c  the group header's key vs the filter's key (user report)");

    async fn chat_shaped(c: &Ctx, name: &str, user_raw: &str, user_slug: &str) -> Org {
        let slug = format!("{name}.{user_slug}.{}", token_hex3());
        let secret = token_hex16();
        let r = Call::new("POST", "/api/register")
            .auth(pair(&slug, &secret))
            .json(json!({ "slug": slug, "org_name": name, "username": user_raw, "kind": "chat" }))
            .send(&c.hub)
            .await;
        assert_eq!(r.code(), 200, "{}", r.text());
        (slug, secret)
    }

    t.check("group · the key the UI groups by is the key the filter accepts", async {
        let a = chat_shaped(c, "alpha-chat", "grp_one", "grp-one").await;
        let b = chat_shaped(c, "beta-chat", "grp_one", "grp-one").await;
        c.send(&a, &b.0, "chat to chat, same client", json!({})).await;
        let j = Call::new("GET", "/ui/messages?client=grp_one").send(&c.hub).await.json();
        ensure!(bodies_of(&j["messages"]).iter().any(|x| x == "chat to chat, same client"), "the group header's own key returns nothing for chat traffic");
        Ok(())
    })
    .await;

    t.check("group · the chat traffic IS retrievable under the slug-segment key (so the report is a mismatch, not a loss)", async {
        let j = Call::new("GET", "/ui/messages?client=grp-one").send(&c.hub).await.json();
        ensure!(bodies_of(&j["messages"]).iter().any(|x| x == "chat to chat, same client"), "{j}");
        Ok(())
    })
    .await;

    t.check("group · org-to-org traffic answers to the same key either way (which is why the bug looks like 'only orgs show up')", async {
        let (me, s) = (c.org().await, c.org().await);
        c.send(&s, &me.0, "org to org", json!({})).await;
        let j = Call::new("GET", "/ui/messages?client=tester").send(&c.hub).await.json();
        ensure!(bodies_of(&j["messages"]).iter().any(|x| x == "org to org"), "{j}");
        Ok(())
    })
    .await;

    t.check("roster · the hub can forget a client that has gone away", async {
        let (slug, secret) = c.new_org("goes away", "").await;
        ensure!(!c.rows("SELECT slug FROM identities WHERE slug = $1", &[&slug]).await.is_empty(), "fixture: the org never registered");
        let mut gone = Vec::new();
        for p in ["/api/unregister", "/api/forget", "/api/leave"] {
            let r = Call::new("POST", p).auth(pair(&slug, &secret)).json(json!({ "slug": slug })).send(&c.hub).await;
            if r.code() != 404 {
                gone.push(p);
            }
        }
        c.db.execute("UPDATE identities SET last_seen = '2020-01-01T00:00:00Z' WHERE slug = $1", &[&slug]).await.unwrap();
        let left = c.rows("SELECT slug FROM identities WHERE slug = $1", &[&slug]).await;
        ensure!(left.is_empty() || !gone.is_empty(), "a client not seen since 2020 is still in the roster and there is no route to remove it");
        Ok(())
    })
    .await;
}

#[tokio::test]
async fn hub_suite() {
    println!("orgtree · the mail hub v2 (v1 suite, ported)");
    let pg = TestPg::start();
    let url = pg.fresh_db("hubsuite").await;
    let data = std::env::temp_dir().join(format!("orgtree-hub-{}", token_hex16()));
    std::fs::create_dir_all(&data).unwrap();
    let hub = hub(&url, &data, &[]).await;
    let db = sql_client(&url).await;
    let c = Ctx { hub, url, data: data.clone(), db, n: AtomicUsize::new(0) };
    let mut t = Tally::default();
    sec_register(&mut t, &c).await;
    sec_suffix_attack(&mut t, &c).await;
    sec_poll(&mut t, &c).await;
    sec_custody(&mut t, &c).await;
    sec_receipts(&mut t, &c).await;
    sec_send(&mut t, &c).await;
    sec_attachments(&mut t, &c).await;
    sec_retention(&mut t, &c).await;
    sec_ui(&mut t, &c).await;
    sec_public_face(&mut t, &c).await;
    sec_client_filter(&mut t, &c).await;
    sec_client_filter_paging(&mut t, &c).await;
    sec_group_key(&mut t, &c).await;
    println!();
    let _ = std::fs::remove_dir_all(&data);
    drop(pg);
    if !t.fail.is_empty() {
        for (label, why) in &t.fail {
            println!("\n✗ {label}\n{why}");
        }
        panic!("hub: {} passed · {} FAILED", t.pass, t.fail.len());
    }
    println!("hub: all {} checks passed", t.pass);
}
