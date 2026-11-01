//! Translation between people (v2.4.0, schema 13; Orgtree docket
//! mail-hub-hubchat-automatic-translation-between-u, user 2026-11-01 "Build
//! it"): a person reads in a language (`POST /api/profile {language}`), and
//! mail to them in another language is translated for them on the hub after
//! it is delivered, never before. Only the reader's own sync and history
//! carry `translation`; the sender, orgs and everyone else see the message
//! as if nothing had happened. A sender with no language is taken to write
//! in the hub's default language (user 12:15Z). The provider here is a fake
//! on loopback (feature `translate-test`); no real key is ever used.
//!
//!     HUB_TEST_PG=<folder> cargo test --features translate-test --test translation -- --nocapture

#![cfg(feature = "translate-test")]
mod support;

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use axum::{body::Body, extract::State, http::HeaderMap, Router};
use http::Response;
use mailhub::Hub;
use serde_json::{json, Value};
use support::*;

type Id = (String, String);

/// What the fake answers next: a status, a reply text (or a raw body) and an
/// optional retry-after. With nothing scripted it translates by wrapping.
#[derive(Clone)]
enum Answer {
    Text(String),
    Status(u16, Option<u64>),
    Raw(Value),
}

#[derive(Clone, Default)]
struct Fake {
    script: Arc<Mutex<VecDeque<Answer>>>,
    seen: Arc<Mutex<Vec<(HeaderMap, Value)>>>,
}

impl Fake {
    fn then(&self, a: Answer) {
        self.script.lock().unwrap().push_back(a);
    }
    fn calls(&self) -> usize {
        self.seen.lock().unwrap().len()
    }
    fn last(&self) -> (HeaderMap, Value) {
        self.seen.lock().unwrap().last().cloned().expect("a call to the provider")
    }
}

fn inner(body: &Value) -> String {
    let text = body["messages"][0]["content"].as_str().unwrap_or_default();
    let start = text.find("<message>\n").map(|i| i + "<message>\n".len()).unwrap_or(0);
    let end = text.rfind("\n</message>").unwrap_or(text.len());
    text[start..end].to_string()
}

fn reply(text: &str) -> Value {
    json!({ "type": "message", "role": "assistant", "content": [{ "type": "text", "text": text }],
            "stop_reason": "end_turn", "usage": { "input_tokens": 400, "output_tokens": 150 } })
}

async fn provider(State(fake): State<Fake>, headers: HeaderMap, body: axum::body::Bytes) -> Response<Body> {
    let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    fake.seen.lock().unwrap().push((headers, body.clone()));
    let next = fake.script.lock().unwrap().pop_front();
    let (status, out, retry) = match next {
        None => (200, reply(&format!("TR[{}]", inner(&body))), None),
        Some(Answer::Text(t)) => (200, reply(&t), None),
        Some(Answer::Raw(v)) => (200, v, None),
        Some(Answer::Status(s, r)) => (s, json!({ "type": "error", "error": { "type": "x", "message": "x" } }), r),
    };
    let mut b = Response::builder().status(status).header("content-type", "application/json");
    if let Some(r) = retry {
        b = b.header("retry-after", r.to_string());
    }
    b.body(Body::from(serde_json::to_vec(&out).unwrap())).unwrap()
}

struct Rig {
    hub: Arc<Hub>,
    sql: tokio_postgres::Client,
    fake: Fake,
    runtime: PathBuf,
    _pg: TestPg,
}

impl Rig {
    async fn start(name: &str) -> Rig {
        let pg = TestPg::start();
        let url = pg.fresh_db(name).await;
        let data = std::env::temp_dir().join(format!("{name}-{}", token_hex16()));
        std::fs::create_dir_all(&data).unwrap();
        let fake = Fake::default();
        let app = Router::new().fallback(provider).with_state(fake.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let key = data.join("translate.key");
        std::fs::write(&key, "test-key-not-real\n").unwrap();
        let runtime = data.join("runtime.json");
        let rig_url = format!("http://127.0.0.1:{port}/v1/messages");
        let hub = hub(
            &url,
            &data,
            &[
                ("HUB_RUNTIME_CONFIG_FILE", runtime.to_str().unwrap()),
                ("HUB_TRANSLATE_KEY_FILE", key.to_str().unwrap()),
                ("HUB_TRANSLATE_URL", &rig_url),
            ],
        )
        .await;
        let rig = Rig { sql: sql_client(&url).await, hub, fake, runtime, _pg: pg };
        rig.settings(json!({ "enabled": true, "default_language": "en", "monthly_limit_usd": 5 }));
        rig
    }

    fn settings(&self, t: Value) {
        std::fs::write(&self.runtime, serde_json::to_vec(&json!({ "max_attachment_bytes": 1 << 30, "translation": t })).unwrap()).unwrap();
    }

    async fn register(&self, name: &str, kind: &str) -> Id {
        let id = (format!("{name}.tr.{}", token_hex3()), token_hex16());
        let r = Call::new("POST", "/api/register")
            .auth(pair(&id.0, &id.1))
            .json(json!({ "slug": id.0, "org_name": name, "username": "tr", "kind": kind }))
            .send(&self.hub)
            .await;
        assert_eq!(r.code(), 200, "{}", r.text());
        id
    }

    async fn post(&self, who: &Id, path: &str, body: Value) -> Resp {
        Call::new("POST", path).auth(pair(&who.0, &who.1)).json(body).send(&self.hub).await
    }

    async fn reads(&self, who: &Id, language: &str) {
        let r = self.post(who, "/api/profile", json!({ "language": language })).await;
        assert_eq!(r.code(), 200, "{}", r.text());
        assert_eq!(r.json()["language"], if language.is_empty() { Value::Null } else { json!(language) }, "{}", r.text());
    }

    async fn send(&self, from: &Id, to: &Id, body: &str) -> String {
        let id = format!("m-{}", token_hex16());
        let r = self.post(from, "/api/send", json!({ "to": to.0, "id": id, "body": body, "kind": "message" })).await;
        assert_eq!(r.code(), 200, "{}", r.text());
        id
    }

    /// The whole sync answer `who`'s device gets from the start.
    async fn sync(&self, who: &Id) -> Value {
        let r = self.post(who, "/api/sync", json!({ "device_id": "d1", "wait": 0 })).await;
        assert_eq!(r.code(), 200, "{}", r.text());
        r.json()
    }

    /// The message `id` as `who`'s sync carries it now.
    async fn seen(&self, who: &Id, id: &str) -> Value {
        let all = self.sync(who).await;
        all["changes"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|c| c["type"] == "message" && c["message"]["id"] == id)
            .map(|c| c["message"].clone())
            .last()
            .unwrap_or_else(|| panic!("{id} not in {all}"))
    }

    async fn work(&self) -> usize {
        mailhub::translate::dispatch_due(&self.hub).await.expect("a translation pass")
    }

    /// Retries are due now (instead of after their backoff).
    async fn due(&self) {
        self.sql.batch_execute("UPDATE translate_jobs SET next_attempt = now() - interval '1 second'").await.unwrap();
    }

    async fn health(&self, public: bool) -> Value {
        let mut c = Call::new("GET", "/healthz");
        if public {
            c = c.public();
        }
        c.send(&self.hub).await.json()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_person_reads_in_a_language() {
    let rig = Rig::start("hubtrlang").await;
    let (ann, bob, org) = (rig.register("ann", "person").await, rig.register("bob", "person").await, rig.register("acme", "org").await);
    assert!(rig.health(false).await["features"].as_array().unwrap().contains(&json!("translate")));
    // none until chosen; every own sync answer says it
    assert_eq!(rig.sync(&ann).await["language"], Value::Null);
    rig.reads(&ann, "he").await;
    assert_eq!(rig.sync(&ann).await["language"], json!("he"));
    rig.reads(&ann, "pt-BR").await;
    assert_eq!(rig.sync(&ann).await["language"], json!("pt-BR"));
    // a language alone leaves the name and about line as they were
    let me = rig.post(&ann, "/api/profile", json!({ "language": "he" })).await.json();
    assert_eq!(me["profile"]["org_name"], json!("ann"), "{me}");
    // others see it on her roster entry (user 2026-11-01 16:13Z: a person's
    // language shows on their profile and to agents), never as their own
    let other = rig.sync(&bob).await;
    assert_eq!(other["language"], Value::Null, "{other}");
    let row = other["roster"].as_array().unwrap().iter().find(|r| r["slug"] == ann.0).cloned().unwrap();
    assert_eq!(row["language"], json!("he"), "{row}");
    // a change reaches the others' next sync as a roster change
    rig.reads(&ann, "fr").await;
    let later = rig.sync(&bob).await;
    let row = later["roster"].as_array().unwrap().iter().find(|r| r["slug"] == ann.0).cloned();
    assert_eq!(row.map(|r| r["language"].clone()), Some(json!("fr")), "{later}");
    rig.reads(&ann, "he").await;
    // an org's entry and a person without one carry none
    let row = rig.sync(&ann).await["roster"].as_array().unwrap().iter().find(|r| r["slug"] == org.0 || r["slug"] == bob.0).cloned().unwrap();
    assert!(row.get("language").is_none(), "{row}");
    // only a person reads in a language; a tag must look like one
    assert_eq!(rig.post(&org, "/api/profile", json!({ "language": "en" })).await.code(), 403);
    for bad in ["english", "e", "he_IL", "he-", "x".repeat(40).as_str()] {
        let r = rig.post(&ann, "/api/profile", json!({ "language": bad })).await;
        assert_eq!(r.code(), 422, "{bad}: {}", r.text());
    }
    // an empty language turns it off
    rig.reads(&ann, "").await;
    assert_eq!(rig.sync(&ann).await["language"], Value::Null);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mail_to_a_reader_in_another_language_is_translated_for_them_only() {
    let rig = Rig::start("hubtrflow").await;
    let (ann, bob) = (rig.register("ann", "person").await, rig.register("bob", "person").await);
    rig.reads(&ann, "en").await;
    rig.reads(&bob, "he").await;
    // a device of Bob's with a push subscription: its wake waits for the translation
    rig.sync(&bob).await;
    rig.sql
        .execute(
            "INSERT INTO device_push (slug, device_id, registration, endpoint, destination, p256dh, auth, pending, next_attempt)
             VALUES ($1, 'd1', 'r', 'https://push.invalid/x', 'h', $2, $3, 0, NULL)",
            &[&bob.0, &vec![4u8; 65], &vec![1u8; 16]],
        )
        .await
        .unwrap();
    let body = "Meet at **noon**: see https://example.com/plan and `cargo test` @ann";
    let id = rig.send(&ann, &bob, body).await;
    // delivered at once, untouched, saying a translation is on its way
    let m = rig.seen(&bob, &id).await;
    assert_eq!(m["body"], json!(body));
    assert_eq!(m["translation"], json!({ "lang": "he", "status": "pending" }), "{m}");
    let wake: chrono::DateTime<chrono::Utc> = rig.sql.query_one("SELECT next_attempt FROM device_push WHERE slug = $1", &[&bob.0]).await.unwrap().get(0);
    let held = (wake - chrono::Utc::now()).num_milliseconds();
    assert!((2500..=4100).contains(&held), "the wake is held about 4 s, not {held} ms");
    // the worker translates it once
    assert_eq!(rig.work().await, 1);
    assert_eq!(rig.work().await, 0);
    let m = rig.seen(&bob, &id).await;
    assert_eq!(m["body"], json!(body), "the original stays");
    assert_eq!(m["translation"], json!({ "lang": "he", "source_lang": "en", "status": "translated", "body": format!("TR[{body}]") }), "{m}");
    // and the held wake goes now
    let wake: chrono::DateTime<chrono::Utc> = rig.sql.query_one("SELECT next_attempt FROM device_push WHERE slug = $1", &[&bob.0]).await.unwrap().get(0);
    assert!(wake <= chrono::Utc::now() + chrono::Duration::milliseconds(200), "the wake waits no longer: {wake}");
    // what the provider was asked: the key from the file, the configured
    // model, thinking off, low effort, no sampling settings
    let (h, req) = rig.fake.last();
    assert_eq!(h.get("x-api-key").unwrap(), "test-key-not-real");
    assert_eq!(h.get("anthropic-version").unwrap(), "2026-10-21");
    assert_eq!(req["model"], json!("claude-haiku-5-5"));
    assert_eq!(req["thinking"], json!({ "type": "disabled" }));
    assert_eq!(req["output_config"], json!({ "effort": "low" }));
    assert!(req.get("temperature").is_none());
    assert_eq!(inner(&req), body);
    let system = req["system"].as_str().unwrap();
    assert!(system.contains("from en") && system.contains("into he"), "{system}");
    // the reader's history carries it too; the sender's sync and history never do
    let page = Call::new("GET", &format!("/api/history?with={}", ann.0)).auth(pair(&bob.0, &bob.1)).send(&rig.hub).await.json();
    assert_eq!(page["messages"][0]["translation"]["status"], json!("translated"), "{page}");
    let mine = rig.seen(&ann, &id).await;
    assert!(mine.get("translation").is_none(), "{mine}");
    let page = Call::new("GET", &format!("/api/history?with={}", bob.0)).auth(pair(&ann.0, &ann.1)).send(&rig.hub).await.json();
    assert!(page["messages"][0].get("translation").is_none(), "{page}");
    // the hub counts what it spent (400 in, 150 out at $0.10 / $0.50 per million)
    let t = &rig.health(false).await["translation"];
    assert_eq!((t["enabled"].clone(), t["state"].clone(), t["messages"].clone()), (json!(true), json!("ok"), json!(1)), "{t}");
    assert!((t["spent_usd"].as_f64().unwrap() - 0.000115).abs() < 1e-9, "{t}");
    // strangers at the door are not told any of it
    assert!(rig.health(true).await.get("translation").is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nothing_is_translated_that_needs_none() {
    let rig = Rig::start("hubtrnone").await;
    let (ann, bob, cy, acme) = (
        rig.register("ann", "person").await,
        rig.register("bob", "person").await,
        rig.register("cy", "person").await,
        rig.register("acme", "org").await,
    );
    rig.reads(&ann, "he").await;
    rig.reads(&bob, "he-IL").await;
    rig.reads(&cy, "en").await;
    let dee = rig.register("dee", "person").await;
    let mut ids = vec![
        rig.send(&ann, &bob, "same language, another region").await, // he -> he-IL
        rig.send(&bob, &acme, "an org reads the original").await,
        rig.send(&acme, &cy, "an org writes in the hub's default, en").await,
        rig.send(&ann, &dee, "a reader with no language").await,
        rig.send(&ann, &cy, "").await,
    ];
    // translation off at the hub: nothing either
    rig.settings(json!({ "enabled": false, "default_language": "en", "monthly_limit_usd": 5 }));
    ids.push(rig.send(&ann, &cy, "off").await);
    assert_eq!(rig.work().await, 0);
    assert_eq!(rig.fake.calls(), 0);
    for (who, id) in [(&bob, &ids[0]), (&acme, &ids[1]), (&cy, &ids[2]), (&dee, &ids[3]), (&cy, &ids[4]), (&cy, &ids[5])] {
        let m = rig.seen(who, id).await;
        assert!(m.get("translation").is_none(), "{m}");
    }
    // a hub whose default is not the reader's: an agent's (org's) mail stays
    // untranslated by default (user 2026-11-01 16:12Z: agents write in their
    // reader's language themselves), and so does a chat client's
    rig.settings(json!({ "enabled": true, "default_language": "fr", "monthly_limit_usd": 5 }));
    let bot = rig.register("bot", "chat").await;
    let quiet = [rig.send(&acme, &cy, "bonjour").await, rig.send(&bot, &cy, "salut").await];
    assert_eq!(rig.work().await, 0);
    for id in &quiet {
        assert!(rig.seen(&cy, id).await.get("translation").is_none(), "no translation and no reason line");
    }
    // a person without a language of their own is taken to write in the hub's default
    let id = rig.send(&dee, &cy, "bonjour de dee").await;
    assert_eq!(rig.work().await, 1);
    assert!(rig.fake.last().1["system"].as_str().unwrap().contains("from fr"));
    assert_eq!(rig.seen(&cy, &id).await["translation"]["status"], json!("translated"));
    // the owner's "Translate messages from agents": an org's mail is translated, from the default
    rig.settings(json!({ "enabled": true, "default_language": "fr", "monthly_limit_usd": 5, "translate_agents": true }));
    let id = rig.send(&acme, &cy, "bonjour").await;
    assert_eq!(rig.work().await, 1);
    assert!(rig.fake.last().1["system"].as_str().unwrap().contains("from fr"));
    assert_eq!(rig.seen(&cy, &id).await["translation"]["status"], json!("translated"));
    // mail to an agent stays the original even then
    let id = rig.send(&cy, &acme, "hello agent").await;
    assert_eq!(rig.work().await, 0);
    assert!(rig.seen(&acme, &id).await.get("translation").is_none());
    // already in the reader's language after all: the model says so and nothing shows
    rig.fake.then(Answer::Text("<same/>".into()));
    let id = rig.send(&acme, &cy, "this was English all along").await;
    assert_eq!(rig.work().await, 1);
    assert!(rig.seen(&cy, &id).await.get("translation").is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn when_it_cannot_translate_the_reader_is_told_why() {
    let rig = Rig::start("hubtrwhy").await;
    let (ann, bob) = (rig.register("ann", "person").await, rig.register("bob", "person").await);
    rig.reads(&ann, "en").await;
    rig.reads(&bob, "he").await;
    // too long: never sent to the provider
    let id = rig.send(&ann, &bob, &"word ".repeat(2001)).await;
    assert_eq!(rig.seen(&bob, &id).await["translation"], json!({ "lang": "he", "status": "too_long" }));
    assert_eq!(rig.work().await, 0);
    // the provider fails: retried, then given up on
    let id = rig.send(&ann, &bob, "hello").await;
    for _ in 0..6 {
        rig.fake.then(Answer::Status(500, None));
    }
    for _ in 0..6 {
        rig.due().await;
        rig.work().await;
    }
    assert_eq!(rig.fake.calls(), 6);
    assert_eq!(rig.seen(&bob, &id).await["translation"], json!({ "lang": "he", "status": "failed" }));
    // a rate limit is waited out without counting as a try
    let id = rig.send(&ann, &bob, "again").await;
    rig.fake.then(Answer::Status(429, Some(30)));
    assert_eq!(rig.work().await, 1);
    let (attempts, next): (i32, chrono::DateTime<chrono::Utc>) = {
        let r = rig.sql.query_one("SELECT j.attempts, j.next_attempt FROM translate_jobs j JOIN messages m ON m.n = j.message_n WHERE m.id = $1", &[&id]).await.unwrap();
        (r.get(0), r.get(1))
    };
    assert_eq!(attempts, 0);
    assert!(next >= chrono::Utc::now() + chrono::Duration::seconds(28), "{next}");
    rig.due().await;
    assert_eq!(rig.work().await, 1);
    assert_eq!(rig.seen(&bob, &id).await["translation"]["status"], json!("translated"));
    // a translation that dropped a link or code is not shown
    rig.fake.then(Answer::Text("שלום".into()));
    let id = rig.send(&ann, &bob, "see https://example.com/x and `ls -la`").await;
    rig.work().await;
    assert_eq!(rig.seen(&bob, &id).await["translation"]["status"], json!("failed"));
    // a cut-off reply or a refusal is not shown either
    let mut cut = reply("half");
    cut["stop_reason"] = json!("max_tokens");
    rig.fake.then(Answer::Raw(cut));
    let id = rig.send(&ann, &bob, "long enough").await;
    rig.work().await;
    assert_eq!(rig.seen(&bob, &id).await["translation"]["status"], json!("failed"));
    // a refused key: the hub owner is told; mail goes on
    rig.fake.then(Answer::Status(401, None));
    let id = rig.send(&ann, &bob, "key").await;
    rig.work().await;
    assert_eq!(rig.health(false).await["translation"]["state"], json!("key_refused"));
    assert_eq!(rig.seen(&bob, &id).await["translation"]["status"], json!("pending"));
    rig.due().await;
    rig.work().await;
    assert_eq!(rig.health(false).await["translation"]["state"], json!("ok"));
    // the monthly limit: once reached, mail is still delivered, untranslated
    rig.settings(json!({ "enabled": true, "default_language": "en", "monthly_limit_usd": 0.0001 }));
    let id = rig.send(&ann, &bob, "over the limit").await;
    assert_eq!(rig.seen(&bob, &id).await["translation"], json!({ "lang": "he", "status": "limit" }));
    assert_eq!(rig.work().await, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deleting_a_message_removes_the_readers_translation() {
    let rig = Rig::start("hubtrdel").await;
    let (ann, bob) = (rig.register("ann", "person").await, rig.register("bob", "person").await);
    rig.reads(&ann, "en").await;
    rig.reads(&bob, "he").await;
    let kept = rig.send(&ann, &bob, "kept by bob").await;
    let gone = rig.send(&ann, &bob, "deleted by bob").await;
    rig.work().await;
    let waiting = rig.send(&ann, &bob, "deleted before translation").await;
    let count = |id: String| {
        let sql = &rig.sql;
        async move {
            let r = sql
                .query_one(
                    "SELECT (SELECT count(*) FROM message_translations t JOIN messages m ON m.n = t.message_n WHERE m.id = $1)
                          + (SELECT count(*) FROM translate_jobs j JOIN messages m ON m.n = j.message_n WHERE m.id = $1)",
                    &[&id],
                )
                .await
                .unwrap();
            r.get::<_, i64>(0)
        }
    };
    // the sender deleting theirs leaves the reader's translation
    let r = Call::new("DELETE", &format!("/api/messages/{kept}")).auth(pair(&ann.0, &ann.1)).send(&rig.hub).await;
    assert_eq!(r.code(), 200, "{}", r.text());
    assert_eq!(count(kept.clone()).await, 1);
    for id in [&gone, &waiting] {
        let r = Call::new("DELETE", &format!("/api/messages/{id}")).auth(pair(&bob.0, &bob.1)).send(&rig.hub).await;
        assert_eq!(r.code(), 200, "{}", r.text());
        assert_eq!(count(id.clone()).await, 0, "{id}");
    }
    assert_eq!(rig.work().await, 0, "no job left for a deleted message");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nobody_else_can_tell_a_message_was_translated() {
    // two readers, one in Hebrew and one with no language: everything the
    // sender and an org on the hub read about the two messages is the same,
    // byte for byte, once the parts that differ anyway (ids, addresses,
    // times) are set aside. A person's chosen language is on their profile
    // for everyone (user 2026-11-01 16:13Z); whether a message was translated
    // for them is still nobody else's business.
    let rig = Rig::start("hubtrleak").await;
    let (ann, he, plain, acme) = (
        rig.register("ann", "person").await,
        rig.register("hera", "person").await,
        rig.register("pia", "person").await,
        rig.register("acme", "org").await,
    );
    rig.reads(&ann, "en").await;
    rig.reads(&he, "he").await;
    let a = rig.send(&ann, &he, "the same words").await;
    let b = rig.send(&ann, &plain, "the same words").await;
    rig.work().await;
    let strip = |mut m: Value| {
        for k in ["id", "to", "received_at", "sent_at"] {
            m.as_object_mut().unwrap().remove(k);
        }
        serde_json::to_string(&m).unwrap()
    };
    assert_eq!(strip(rig.seen(&ann, &a).await), strip(rig.seen(&ann, &b).await));
    let row = |all: &Value, slug: &str| {
        let mut r = all["roster"].as_array().unwrap().iter().find(|r| r["slug"] == slug).cloned().unwrap();
        for k in ["slug", "org_name", "picture_owner", "last_seen", "language"] {
            r.as_object_mut().unwrap().remove(k);
        }
        // compared as values: removing a key may reorder the rest
        r
    };
    for viewer in [&ann, &acme] {
        let all = rig.sync(viewer).await;
        assert_eq!(row(&all, &he.0), row(&all, &plain.0), "{all}");
    }
    let dir = |who: &Id| {
        let hub = rig.hub.clone();
        let who = who.clone();
        async move { Call::new("GET", "/api/directory").auth(pair(&who.0, &who.1)).send(&hub).await.text() }
    };
    let listing: Value = serde_json::from_str(&dir(&acme).await).unwrap();
    let langs: Vec<Value> = listing["entries"].as_array().cloned().unwrap_or_default()
        .iter().map(|e| e.get("language").cloned().unwrap_or(Value::Null)).collect();
    assert_eq!(langs.iter().filter(|l| **l == json!("he")).count(), 1, "the profile's language, once: {listing}");
}
