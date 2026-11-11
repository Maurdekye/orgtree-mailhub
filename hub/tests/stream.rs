//! The WebSocket stream (`GET /api/stream`, v2.5.0; Orgtree docket
//! mail-hub-websocket-for-new-mail-clients-prefer-i, spec
//! spec-hub-websocket.md T1 to T15): the long poll carried over one socket.
//! Each request frame (`sync` or `poll`) gets exactly one answer frame, built
//! by the same code as the HTTP answer; the hub never sends anything else.
//!
//! The timers (hello 10 s, ping 25 s, no-pong 60 s, idle 120 s, write 30 s)
//! are shortened by the test feature's `HUB_STREAM_TIMERS_MS=hello,ping,pong,idle,write`
//! (release builds ignore it).
//!
//!     HUB_TEST_PG=<folder> cargo test --features test-clock --test stream -- --nocapture

#![cfg(feature = "test-clock")]
mod support;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::{SinkExt, StreamExt};
use mailhub::{server, Hub};
use serde_json::{json, Value};
use support::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

type Id = (String, String);
type Sock = WebSocketStream<MaybeTlsStream<TcpStream>>;

const PROTO: &str = "orgtree-hub-stream.1";

// ---------------------------------------------------------------- the rig

struct Rig {
    hub: Arc<Hub>,
    full: SocketAddr,
    door: SocketAddr,
    serving: tokio::task::JoinHandle<()>,
    _pg: TestPg,
}

async fn rig(extra: &[(&str, &str)]) -> Rig {
    let pg = TestPg::start();
    let url = pg.fresh_db("hubstream").await;
    let data = std::env::temp_dir().join(format!("orgtree-hub-stream-{}", token_hex16()));
    std::fs::create_dir_all(&data).unwrap();
    let hub = hub(&url, &data, extra).await;
    let full_l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let door_l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (full, door) = (full_l.local_addr().unwrap(), door_l.local_addr().unwrap());
    let stop = hub.shutdown.clone();
    let (a, b) = (hub.clone(), hub.clone());
    let (s1, s2) = (stop.clone(), stop);
    let serving = tokio::spawn(async move {
        let f = axum::serve(full_l, server::full_app(a)).with_graceful_shutdown(s1.cancelled_owned());
        let d = axum::serve(door_l, server::public_app(b)).with_graceful_shutdown(s2.cancelled_owned());
        let _ = tokio::join!(std::future::IntoFuture::into_future(f), std::future::IntoFuture::into_future(d));
    });
    tokio::spawn(mailhub::api::sync::presence_loop(hub.clone()));
    Rig { hub, full, door, serving, _pg: pg }
}

async fn register(hub: &Arc<Hub>, name: &str, kind: &str) -> Id {
    let id = (format!("{name}.stream.{}", token_hex3()), token_hex16());
    let r = Call::new("POST", "/api/register")
        .auth(pair(&id.0, &id.1))
        .json(json!({ "slug": id.0, "org_name": name, "username": "stream", "kind": kind }))
        .send(hub)
        .await;
    assert_eq!(r.code(), 200, "{}", r.text());
    id
}

async fn send(hub: &Arc<Hub>, from: &Id, to: &str, id: &str, body: &str) {
    let r = Call::new("POST", "/api/send")
        .auth(pair(&from.0, &from.1))
        .json(json!({ "id": id, "to": to, "body": body, "sent_at": "2026-11-11T12:00:00.000Z" }))
        .send(hub)
        .await;
    assert_eq!(r.code(), 200, "{}", r.text());
}

/// One raw HTTP exchange: status, the head's lowercased text, the body.
async fn raw(addr: SocketAddr, method: &str, path: &str, headers: &[(&str, &str)]) -> (u16, String, String) {
    let mut s = TcpStream::connect(addr).await.unwrap();
    let mut req = format!("{method} {path} HTTP/1.1\r\nHost: hub\r\n");
    if !headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("connection")) {
        req.push_str("Connection: close\r\n");
    }
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str("\r\n");
    s.write_all(req.as_bytes()).await.unwrap();
    let mut buf = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(5), s.read_to_end(&mut buf)).await;
    let text = String::from_utf8_lossy(&buf).into_owned();
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let status = head.split_whitespace().nth(1).and_then(|c| c.parse().ok()).unwrap_or(0);
    (status, head.to_ascii_lowercase(), body.to_string())
}

/// The headers of a well-formed upgrade, then `extra`.
fn upgrade<'a>(extra: &[(&'a str, &'a str)]) -> Vec<(&'a str, &'a str)> {
    let mut v = vec![
        ("Upgrade", "websocket"),
        ("Connection", "Upgrade"),
        ("Sec-WebSocket-Version", "13"),
        ("Sec-WebSocket-Key", "dGhlIHNhbXBsZSBub25jZQ=="),
        ("Sec-WebSocket-Protocol", PROTO),
    ];
    v.extend_from_slice(extra);
    v
}

#[derive(Default, Clone)]
struct Open<'a> {
    auth: Option<String>,
    origin: Option<&'a str>,
}

async fn try_open(addr: SocketAddr, o: Open<'_>) -> Result<Sock, tokio_tungstenite::tungstenite::Error> {
    let mut req = format!("ws://{addr}/api/stream").into_client_request().unwrap();
    req.headers_mut().insert("sec-websocket-protocol", PROTO.parse().unwrap());
    if let Some(a) = &o.auth {
        req.headers_mut().insert("x-org-auth", a.parse().unwrap());
    }
    if let Some(origin) = o.origin {
        req.headers_mut().insert("origin", origin.parse().unwrap());
    }
    tokio_tungstenite::connect_async(req).await.map(|(s, _)| s)
}

async fn open(addr: SocketAddr, auth: Option<&str>) -> Sock {
    try_open(addr, Open { auth: auth.map(str::to_string), origin: None }).await.expect("the stream upgrades")
}

async fn say(ws: &mut Sock, v: Value) {
    ws.send(Message::Text(v.to_string().into())).await.expect("send a frame");
}

/// What the next frame is: a JSON frame, a close (its code) or the end of
/// the connection without a close frame.
#[derive(Debug)]
enum Got {
    Json(Value),
    Close(u16),
    Ended,
    Silence,
}

async fn next(ws: &mut Sock, wait: Duration) -> Got {
    let end = Instant::now() + wait;
    loop {
        let left = end.saturating_duration_since(Instant::now());
        match tokio::time::timeout(left, ws.next()).await {
            Err(_) => return Got::Silence,
            Ok(None) => return Got::Ended,
            Ok(Some(Err(_))) => return Got::Ended,
            Ok(Some(Ok(Message::Text(t)))) => return Got::Json(serde_json::from_str(t.as_str()).expect("a JSON frame")),
            Ok(Some(Ok(Message::Close(f)))) => return Got::Close(f.map(|f| u16::from(f.code)).unwrap_or(1005)),
            Ok(Some(Ok(_))) => continue, // ping, pong
        }
    }
}

async fn json_frame(ws: &mut Sock) -> Value {
    match next(ws, Duration::from_secs(10)).await {
        Got::Json(v) => v,
        other => panic!("expected a frame, got {other:?}"),
    }
}

async fn expect_close(ws: &mut Sock, code: u16) {
    match next(ws, Duration::from_secs(10)).await {
        Got::Close(c) => assert_eq!(c, code, "close code"),
        other => panic!("expected close {code}, got {other:?}"),
    }
}

/// hello (credentials in the header, none in the frame) and the welcome.
async fn hello(ws: &mut Sock, auth_in_frame: Option<&str>) -> Value {
    let mut h = json!({ "type": "hello", "v": 1, "client": "test/1" });
    if let Some(a) = auth_in_frame {
        h["auth"] = json!(a);
    }
    say(ws, h).await;
    let w = json_frame(ws).await;
    assert_eq!(w["type"], "welcome", "{w}");
    w
}

fn both(a: &Id, b: &Id) -> String {
    format!("{} {}", pair(&a.0, &a.1), pair(&b.0, &b.1))
}

// ------------------------------------------------------------------- T1

#[tokio::test]
async fn t1_healthz_lists_stream_and_the_operator_can_turn_it_off() {
    let r = rig(&[]).await;
    let h = Call::new("GET", "/healthz").send(&r.hub).await.json();
    assert!(h["features"].as_array().unwrap().contains(&json!("stream")), "{h}");

    let off = rig(&[("HUB_STREAM_MAX", "0")]).await;
    let h = Call::new("GET", "/healthz").send(&off.hub).await.json();
    assert!(!h["features"].as_array().unwrap().contains(&json!("stream")), "{h}");
    let (status, head, body) = raw(off.full, "GET", "/api/stream", &upgrade(&[])).await;
    assert_eq!((status, body.as_str()), (403, ""), "today's refusal when off: {head}");
    println!("  ok  /healthz lists stream; HUB_STREAM_MAX=0 hides it and keeps today's 403");
}

// ------------------------------------------------------------------- T2

#[tokio::test]
async fn t2_the_handshake_refusals_in_order() {
    let r = rig(&[]).await;
    let ann = register(&r.hub, "ann", "person").await;
    let good = pair(&ann.0, &ann.1);

    let (s, head, _) = raw(r.full, "POST", "/api/stream", &[]).await;
    assert_eq!(s, 405, "{head}");
    assert!(head.contains("allow: get"), "{head}");

    let (s, head, body) = raw(r.full, "GET", "/api/stream", &[]).await;
    assert_eq!(s, 426, "{head}");
    assert!(head.contains("upgrade: websocket"), "{head}");
    assert!(body.contains("this is a WebSocket endpoint"), "{body}");

    let mut bad_version = upgrade(&[]);
    bad_version.retain(|(k, _)| *k != "Sec-WebSocket-Version");
    bad_version.push(("Sec-WebSocket-Version", "12"));
    let (s, _, _) = raw(r.full, "GET", "/api/stream", &bad_version).await;
    assert_eq!(s, 400, "not a valid RFC 6455 upgrade");

    let mut no_proto = upgrade(&[]);
    no_proto.retain(|(k, _)| *k != "Sec-WebSocket-Protocol");
    let (s, _, body) = raw(r.full, "GET", "/api/stream", &no_proto).await;
    assert_eq!(s, 400);
    assert!(body.contains("offer the subprotocol orgtree-hub-stream.1"), "{body}");

    let (s, _, body) = raw(r.full, "GET", "/api/stream", &upgrade(&[("Origin", "https://evil.example")])).await;
    assert_eq!(s, 403);
    assert!(body.contains("this web app's address is not allowed by this hub"), "{body}");

    let (s, _, body) = raw(r.full, "GET", "/api/stream", &upgrade(&[("X-Org-Auth", "nobody.x:wrong")])).await;
    assert_eq!(s, 401);
    assert!(body.contains("no valid org credentials"), "{body}");

    let mixed = format!("{good} other.x:dev1.d.1.sig");
    let (s, _, body) = raw(r.full, "GET", "/api/stream", &upgrade(&[("X-Org-Auth", mixed.as_str())])).await;
    assert_eq!(s, 401);
    assert!(body.contains("device-signed credentials are not accepted on the stream"), "{body}");

    // anywhere else an upgrade is still today's empty 403, on both listeners
    for addr in [r.full, r.door] {
        for path in ["/api/poll", "/api/sync", "/healthz", "/api/streams"] {
            let (s, _, body) = raw(addr, "GET", path, &upgrade(&[])).await;
            assert_eq!((s, body.as_str()), (403, ""), "{path}");
        }
    }
    // and the valid one upgrades
    let (s, head, _) = raw(r.full, "GET", "/api/stream", &upgrade(&[("X-Org-Auth", good.as_str())])).await;
    assert_eq!(s, 101, "{head}");
    assert!(head.contains("sec-websocket-protocol: orgtree-hub-stream.1"), "{head}");
    println!("  ok  405, 426, 400 (version, subprotocol), 403 origin, 401, 401 dev1; other paths 403 empty; 101");
}

// ------------------------------------------------------------------- T3

#[tokio::test]
async fn t3_header_auth_welcome_lists_the_slugs_in_header_order() {
    let r = rig(&[]).await;
    let (ann, bob) = (register(&r.hub, "ann", "person").await, register(&r.hub, "bob", "org").await);
    let header = format!("{} {} {}", pair(&bob.0, &bob.1), pair(&ann.0, &ann.1), pair(&bob.0, &bob.1));
    let mut ws = open(r.full, Some(&header)).await;
    let w = hello(&mut ws, None).await;
    assert_eq!(w["v"], 1);
    assert_eq!(w["slugs"], json!([bob.0, ann.0]), "header order, no repeats");
    assert_eq!((w["hub"].clone(), w["ping_s"].clone(), w["max_request"].clone()), (json!("test-hub"), json!(25), json!(65536)));
    assert_eq!(w["version"], json!(mailhub::VERSION));

    let mut again = open(r.full, Some(&header)).await;
    say(&mut again, json!({ "type": "hello", "v": 1, "auth": header })).await;
    expect_close(&mut again, 4400).await;
    println!("  ok  welcome carries slugs in header order; hello auth beside a header closes 4400");
}

// ------------------------------------------------------------------- T4

#[tokio::test]
async fn t4_hello_auth() {
    let r = rig(&[("HUB_STREAM_TIMERS_MS", "400,25000,60000,120000,30000")]).await;
    let ann = register(&r.hub, "ann", "person").await;
    let good = pair(&ann.0, &ann.1);

    let mut ws = open(r.full, None).await;
    let w = hello(&mut ws, Some(&good)).await;
    assert_eq!(w["slugs"], json!([ann.0]));

    let mut ws = open(r.full, None).await;
    say(&mut ws, json!({ "type": "hello", "v": 1, "auth": "nobody.x:wrong" })).await;
    expect_close(&mut ws, 4401).await;

    let mut ws = open(r.full, None).await;
    say(&mut ws, json!({ "type": "hello", "v": 1, "auth": format!("{good} {}:dev1.d.1.sig", ann.0) })).await;
    expect_close(&mut ws, 4401).await;

    let mut ws = open(r.full, None).await;
    say(&mut ws, json!({ "type": "hello", "v": 1 })).await;
    expect_close(&mut ws, 4400).await; // auth is required when the upgrade carried none

    // spec 5.1 (amended, mailhub-opus 2026-11-11): a string `auth` within 8 KiB that holds no valid pair
    // is 4401 (empty, unparseable, unknown slug); over 8 KiB is a protocol violation: 4400
    for bad in [json!(""), json!("   "), json!("garbage"), json!("a".repeat(8192))] {
        let mut ws = open(r.full, None).await;
        say(&mut ws, json!({ "type": "hello", "v": 1, "auth": bad })).await;
        expect_close(&mut ws, 4401).await;
    }
    let mut ws = open(r.full, None).await;
    say(&mut ws, json!({ "type": "hello", "v": 1, "auth": "a".repeat(8193) })).await;
    expect_close(&mut ws, 4400).await;
    let mut ws = open(r.full, None).await;
    say(&mut ws, json!({ "type": "hello", "v": 1, "auth": 7 })).await;
    expect_close(&mut ws, 4400).await; // not a string: a malformed hello

    let mut ws = open(r.full, None).await;
    expect_close(&mut ws, 4408).await; // no hello within the hello timer

    let mut ws = open(r.full, None).await;
    ws.send(Message::Binary(vec![1, 2, 3].into())).await.unwrap();
    expect_close(&mut ws, 4400).await;

    let mut ws = open(r.full, None).await;
    ws.send(Message::Text("not json".into())).await.unwrap();
    expect_close(&mut ws, 4400).await;

    let mut ws = open(r.full, Some(&good)).await;
    say(&mut ws, json!({ "type": "poll", "id": 1, "body": {} })).await; // a request before the hello
    expect_close(&mut ws, 4400).await;
    println!("  ok  hello auth: welcome, 4401 (bad, dev1), 4400 (no auth anywhere), 4408 (no hello), 4400 (binary, bad JSON, no hello first)");
}

// ------------------------------------------------------------------- T5

#[tokio::test]
async fn t5_origin_allow_list() {
    // no list: any Origin is refused, no Origin passes
    let r = rig(&[]).await;
    let ann = register(&r.hub, "ann", "person").await;
    let auth = Some(pair(&ann.0, &ann.1));
    let with = |o| Open { auth: auth.clone(), origin: Some(o) };
    assert!(try_open(r.full, with("https://chat.example")).await.is_err(), "no list: refused");
    assert!(try_open(r.full, Open { auth: auth.clone(), origin: None }).await.is_ok(), "no Origin: a native client");

    // env list, on both listeners
    let r = rig(&[("HUB_WEB_ORIGINS", "https://chat.example, https://other.example:8443")]).await;
    let ann = register(&r.hub, "ann", "person").await;
    let auth = Some(pair(&ann.0, &ann.1));
    let with = |o| Open { auth: auth.clone(), origin: Some(o) };
    for addr in [r.full, r.door] {
        assert!(try_open(addr, with("https://chat.example")).await.is_ok());
        assert!(try_open(addr, with("https://other.example:8443")).await.is_ok());
        assert!(try_open(addr, with("https://evil.example")).await.is_err());
        assert!(try_open(addr, with("null")).await.is_err());
        assert!(try_open(addr, Open { auth: auth.clone(), origin: None }).await.is_ok());
    }

    // `*` allows any origin but never the literal null
    let r = rig(&[("HUB_WEB_ORIGINS", "*")]).await;
    let ann = register(&r.hub, "ann", "person").await;
    let auth = Some(pair(&ann.0, &ann.1));
    let with = |o| Open { auth: auth.clone(), origin: Some(o) };
    assert!(try_open(r.full, with("https://anything.example")).await.is_ok());
    assert!(try_open(r.full, with("null")).await.is_err(), "null is never allowed, even with *");

    // the runtime file's web_origins wins over the environment, read at each handshake
    let file = std::env::temp_dir().join(format!("orgtree-hub-origins-{}.json", token_hex16()));
    std::fs::write(&file, r#"{"web_origins": ["https://file.example"]}"#).unwrap();
    let r = rig(&[("HUB_WEB_ORIGINS", "https://chat.example"), ("HUB_RUNTIME_CONFIG_FILE", file.to_str().unwrap())]).await;
    let ann = register(&r.hub, "ann", "person").await;
    let auth = Some(pair(&ann.0, &ann.1));
    let with = |o| Open { auth: auth.clone(), origin: Some(o) };
    assert!(try_open(r.full, with("https://file.example")).await.is_ok());
    assert!(try_open(r.full, with("https://chat.example")).await.is_err(), "the file wins");
    std::fs::write(&file, r#"{"web_origins": ["https://later.example"]}"#).unwrap();
    assert!(try_open(r.full, with("https://later.example")).await.is_ok(), "read at each handshake");
    println!("  ok  Origin: unset refuses, env list, runtime file wins, * but never null, no Origin passes, both listeners");
}

// ------------------------------------------------------------------- T6

/// One scripted scenario on one hub, over HTTP or over the stream.
struct Driver {
    hub: Arc<Hub>,
    header: String,
    ws: Option<Sock>,
    next_id: u64,
}

impl Driver {
    async fn sync(&mut self, body: Value) -> (u64, Value) {
        match &mut self.ws {
            None => {
                let r = Call::new("POST", "/api/sync").auth(self.header.clone()).json(body).send(&self.hub).await;
                (r.code() as u64, r.json())
            }
            Some(ws) => {
                self.next_id += 1;
                say(ws, json!({ "type": "sync", "id": self.next_id, "body": body })).await;
                let a = json_frame(ws).await;
                assert_eq!((a["type"].clone(), a["id"].clone()), (json!("answer"), json!(self.next_id)));
                (a["status"].as_u64().unwrap(), a["body"].clone())
            }
        }
    }

    async fn poll(&mut self, wait: f64) -> (u64, Value) {
        match &mut self.ws {
            None => {
                let r = Call::new("POST", &format!("/api/poll?wait={wait}")).auth(self.header.clone()).send(&self.hub).await;
                (r.code() as u64, r.json())
            }
            Some(ws) => {
                self.next_id += 1;
                say(ws, json!({ "type": "poll", "id": self.next_id, "body": { "wait": wait } })).await;
                let a = json_frame(ws).await;
                assert_eq!(a["id"], json!(self.next_id));
                (a["status"].as_u64().unwrap(), a["body"].clone())
            }
        }
    }
}

/// Times and cursors differ between two runs; everything else must not.
fn scrub(v: &mut Value) {
    match v {
        Value::Object(o) => {
            for k in ["now", "ms", "cursor", "typing_seq"] {
                if o.contains_key(k) {
                    o.insert(k.into(), json!("<scrubbed>"));
                }
            }
            for x in o.values_mut() {
                scrub(x);
            }
        }
        Value::Array(a) => a.iter_mut().for_each(scrub),
        Value::String(s) if s.len() >= 20 && s.starts_with("20") && s.contains('T') && s.ends_with('Z') => *s = "<time>".into(),
        _ => {}
    }
}

async fn scenario(via_stream: bool) -> Vec<(u64, Value)> {
    let r = rig(&[]).await;
    let (ann, bob, cat) = (("ann.par".to_string(), "annsecret1".to_string()), ("bob.par".to_string(), "bobsecret1".to_string()), ("cat.par".to_string(), "catsecret1".to_string()));
    for (id, kind) in [(&ann, "person"), (&bob, "org")] {
        let reg = Call::new("POST", "/api/register").auth(pair(&id.0, &id.1)).json(json!({ "slug": id.0, "org_name": id.0, "username": "par", "kind": kind })).send(&r.hub).await;
        assert_eq!(reg.code(), 200, "{}", reg.text());
    }
    let header = both(&ann, &bob);
    let ws = if via_stream {
        let mut ws = open(r.full, Some(&header)).await;
        hello(&mut ws, None).await;
        Some(ws)
    } else {
        None
    };
    let mut d = Driver { hub: r.hub.clone(), header, ws, next_id: 0 };
    let mut out: Vec<(u64, Value)> = Vec::new();

    let a_sync = |d: &str, cursor: &Value| json!({ "slug": "ann.par", "device_id": d, "cursor": cursor, "wait": 0 });
    let first = d.sync(json!({ "slug": "ann.par", "device_id": "d1", "wait": 0 })).await;
    let mut cur = first.1["cursor"].clone();
    out.push(first);

    send(&r.hub, &bob, &ann.0, "m1", "one").await;
    let s = d.sync(a_sync("d1", &cur)).await;
    cur = s.1["cursor"].clone();
    out.push(s);
    let p = d.poll(0.0).await;
    let queued: Vec<String> = p.1["messages"].as_array().unwrap().iter().map(|m| m["id"].as_str().unwrap().to_string()).collect();
    out.push(p);
    let ack = Call::new("POST", "/api/ack").auth(pair(&ann.0, &ann.1)).json(json!({ "ids": queued })).send(&r.hub).await;
    assert_eq!(ack.code(), 200);
    out.push(d.poll(0.0).await);

    let rc = Call::new("POST", "/api/receipts").auth(pair(&ann.0, &ann.1)).json(json!({ "receipts": [{ "id": "m1", "state": "read", "at": "2026-11-11T12:01:00.000Z" }] })).send(&r.hub).await;
    assert_eq!(rc.code(), 200, "{}", rc.text());
    out.push(d.sync(json!({ "slug": "bob.par", "device_id": "b1", "wait": 0 })).await);
    out.push(d.poll(0.0).await);

    let reg = Call::new("POST", "/api/register").auth(pair(&cat.0, &cat.1)).json(json!({ "slug": cat.0, "org_name": "cat", "username": "par", "kind": "org" })).send(&r.hub).await;
    assert_eq!(reg.code(), 200);
    let s = d.sync(a_sync("d1", &cur)).await;
    cur = s.1["cursor"].clone();
    out.push(s);

    let del = Call::new("DELETE", "/api/messages/m1").auth(pair(&bob.0, &bob.1)).send(&r.hub).await;
    assert_eq!(del.code(), 200, "{}", del.text());
    let s = d.sync(a_sync("d1", &cur)).await;
    cur = s.1["cursor"].clone();
    out.push(s);

    out.push(d.sync(json!({ "slug": "ann.par", "device_id": "d1", "cursor": cur, "wait": 0, "typing": 0 })).await);
    let pr = Call::new("POST", "/api/presence").auth(pair(&bob.0, &bob.1)).json(json!({ "busy": true })).send(&r.hub).await;
    assert_eq!(pr.code(), 200, "{}", pr.text());
    out.push(d.sync(a_sync("d1", &cur)).await);

    out.push(d.sync(json!({ "slug": "ann.par", "device_id": "d2", "wait": 0 })).await);
    let so = Call::new("DELETE", "/api/devices/d2").auth(pair(&ann.0, &ann.1)).send(&r.hub).await;
    assert!(so.code() < 300, "{}", so.text());
    out.push(d.sync(json!({ "slug": "ann.par", "device_id": "d2", "wait": 0 })).await); // 401 signed out

    out.push(d.sync(a_sync("d1", &json!("garbage"))).await); // a foreign cursor, 422
    out.push(d.sync(json!({ "slug": "ann.par", "device_id": "d3", "start": "now", "wait": 0 })).await);
    out.push(d.sync(json!({ "slug": "ann.par", "device_id": "d1", "wait": "soon" })).await); // 422
    out.push(d.sync(json!({ "slug": "nobody", "device_id": "d1", "wait": 0 })).await); // 401
    r.hub.shutdown.cancel();
    for (_, v) in out.iter_mut() {
        scrub(v);
    }
    out
}

#[tokio::test]
async fn t6_the_stream_answers_exactly_what_http_answers() {
    let http = scenario(false).await;
    let stream = scenario(true).await;
    assert_eq!(http.len(), stream.len());
    for (i, (h, s)) in http.iter().zip(&stream).enumerate() {
        assert_eq!(h, s, "step {i} differs between HTTP and the stream");
    }
    assert!(http.iter().any(|(c, _)| *c == 401) && http.iter().any(|(c, _)| *c == 422), "the refusals are in the scenario");
    println!("  ok  {} scripted answers (status and body) equal over HTTP and over the stream", http.len());
}

// ------------------------------------------------------------------- T7

#[tokio::test]
async fn t7_a_parked_request_is_answered_at_once_and_only_once() {
    let r = rig(&[]).await;
    let (ann, bob) = (register(&r.hub, "ann", "person").await, register(&r.hub, "bob", "org").await);
    let mut ws = open(r.full, Some(&pair(&ann.0, &ann.1))).await;
    hello(&mut ws, None).await;

    say(&mut ws, json!({ "type": "sync", "id": 1, "body": { "device_id": "d1", "wait": 55 } })).await;
    let first = json_frame(&mut ws).await; // the first sync of a device answers at once (roster)
    let cursor = first["body"]["cursor"].clone();
    say(&mut ws, json!({ "type": "sync", "id": 2, "body": { "device_id": "d1", "cursor": cursor, "wait": 55 } })).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let t = Instant::now();
    send(&r.hub, &bob, &ann.0, "m1", "hello").await;
    let a = json_frame(&mut ws).await;
    assert!(t.elapsed() < Duration::from_millis(500), "answered in {:?}", t.elapsed());
    assert_eq!((a["id"].clone(), a["status"].clone()), (json!(2), json!(200)));
    assert_eq!(a["body"]["changes"][0]["message"]["id"], "m1");
    assert!(matches!(next(&mut ws, Duration::from_millis(700)).await, Got::Silence), "exactly one answer");

    say(&mut ws, json!({ "type": "poll", "id": 3, "body": { "wait": 0 } })).await;
    let _ = json_frame(&mut ws).await;
    let mut ws = open(r.full, Some(&pair(&bob.0, &bob.1))).await;
    hello(&mut ws, None).await;
    say(&mut ws, json!({ "type": "poll", "id": 1, "body": { "wait": 55 } })).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let t = Instant::now();
    send(&r.hub, &ann, &bob.0, "m2", "back").await;
    let a = json_frame(&mut ws).await;
    assert!(t.elapsed() < Duration::from_millis(500), "poll answered in {:?}", t.elapsed());
    assert_eq!(a["body"]["messages"][0]["id"], "m2");
    assert!(matches!(next(&mut ws, Duration::from_millis(700)).await, Got::Silence));
    println!("  ok  parked sync and poll answered within 500 ms of a send, one answer each");
}

// ------------------------------------------------------------------- T8

#[tokio::test]
async fn t8_a_newer_request_supersedes_the_parked_one() {
    let r = rig(&[]).await;
    let ann = register(&r.hub, "ann", "person").await;
    let mut ws = open(r.full, Some(&pair(&ann.0, &ann.1))).await;
    hello(&mut ws, None).await;
    say(&mut ws, json!({ "type": "poll", "id": 5, "body": { "wait": 55 } })).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    say(&mut ws, json!({ "type": "poll", "id": 6, "body": { "wait": 0 } })).await;
    let a = json_frame(&mut ws).await;
    assert_eq!(a["id"], 6, "only the newer request is answered: {a}");
    assert!(matches!(next(&mut ws, Duration::from_millis(700)).await, Got::Silence), "no answer for the superseded id 5");

    say(&mut ws, json!({ "type": "poll", "id": 6, "body": { "wait": 0 } })).await; // not greater
    expect_close(&mut ws, 4400).await;
    let mut ws = open(r.full, Some(&pair(&ann.0, &ann.1))).await;
    hello(&mut ws, None).await;
    say(&mut ws, json!({ "type": "poll", "id": 0, "body": {} })).await; // ids start at 1
    expect_close(&mut ws, 4400).await;
    println!("  ok  superseded request never answered; non-increasing id and id 0 close 4400");
}

// ------------------------------------------------------------------- T9

#[tokio::test]
async fn t9_the_poll_queue_repeats_until_acked_also_across_a_reconnect() {
    let r = rig(&[]).await;
    let (ann, bob) = (register(&r.hub, "ann", "person").await, register(&r.hub, "bob", "org").await);
    send(&r.hub, &bob, &ann.0, "m1", "one").await;
    let auth = pair(&ann.0, &ann.1);
    let ids = |a: &Value| a["body"]["messages"].as_array().unwrap().iter().map(|m| m["id"].as_str().unwrap().to_string()).collect::<Vec<_>>();

    let mut ws = open(r.full, Some(&auth)).await;
    hello(&mut ws, None).await;
    for id in [1, 2] {
        say(&mut ws, json!({ "type": "poll", "id": id, "body": { "wait": 0 } })).await;
        assert_eq!(ids(&json_frame(&mut ws).await), ["m1"], "repeats until acked");
    }
    drop(ws);
    let mut ws = open(r.full, Some(&auth)).await;
    hello(&mut ws, None).await;
    say(&mut ws, json!({ "type": "poll", "id": 1, "body": { "wait": 0 } })).await;
    assert_eq!(ids(&json_frame(&mut ws).await), ["m1"], "and across a reconnect");

    let ack = Call::new("POST", "/api/ack").auth(auth.clone()).json(json!({ "ids": ["m1"] })).send(&r.hub).await;
    assert_eq!(ack.json()["acked"], 1);
    say(&mut ws, json!({ "type": "poll", "id": 2, "body": { "wait": 0 } })).await;
    assert!(ids(&json_frame(&mut ws).await).is_empty(), "never again after the ack");
    println!("  ok  poll queue: m1 until acked, across a reconnect, then never");
}

// ------------------------------------------------------------------ T10

#[tokio::test]
async fn t10_a_lost_answer_loses_nothing() {
    let r = rig(&[]).await;
    let (ann, bob) = (register(&r.hub, "ann", "person").await, register(&r.hub, "bob", "org").await);
    let auth = pair(&ann.0, &ann.1);
    let mut ws = open(r.full, Some(&auth)).await;
    hello(&mut ws, None).await;
    say(&mut ws, json!({ "type": "sync", "id": 1, "body": { "device_id": "d1", "wait": 0 } })).await;
    let cursor = json_frame(&mut ws).await["body"]["cursor"].clone();
    send(&r.hub, &bob, &ann.0, "m1", "one").await;
    say(&mut ws, json!({ "type": "sync", "id": 2, "body": { "device_id": "d1", "cursor": cursor, "wait": 0 } })).await;
    let lost = json_frame(&mut ws).await; // the client dies before storing it
    assert_eq!(lost["body"]["changes"][0]["message"]["id"], "m1");
    drop(ws);

    let mut ws = open(r.full, Some(&auth)).await;
    hello(&mut ws, None).await;
    say(&mut ws, json!({ "type": "sync", "id": 1, "body": { "device_id": "d1", "cursor": cursor, "wait": 0 } })).await;
    let again = json_frame(&mut ws).await;
    assert_eq!(again["body"]["changes"], lost["body"]["changes"], "the old cursor gets the same changes again");
    println!("  ok  the answer dropped with its connection comes again from the old cursor");
}

// ------------------------------------------------------------------ T11

#[tokio::test]
async fn t11_limits_and_timers() {
    // 17th stream for one address
    let r = rig(&[]).await;
    let ann = register(&r.hub, "ann", "person").await;
    let auth = pair(&ann.0, &ann.1);
    let mut held = Vec::new();
    for _ in 0..16 {
        let mut ws = open(r.full, Some(&auth)).await;
        hello(&mut ws, None).await;
        held.push(ws);
    }
    let mut seventeenth = open(r.full, Some(&auth)).await;
    say(&mut seventeenth, json!({ "type": "hello", "v": 1 })).await;
    expect_close(&mut seventeenth, 4429).await;
    drop(held);

    // a message over 64 KiB, and more than 30 requests in 10 s
    let mut ws = open(r.full, Some(&auth)).await;
    hello(&mut ws, None).await;
    ws.send(Message::Text(format!("{{\"type\":\"poll\",\"id\":1,\"body\":{{\"pad\":\"{}\"}}}}", "x".repeat(65536)).into())).await.unwrap();
    expect_close(&mut ws, 1009).await;
    let mut ws = open(r.full, Some(&auth)).await;
    hello(&mut ws, None).await;
    for id in 1..=31 {
        let _ = ws.send(Message::Text(json!({ "type": "poll", "id": id, "body": { "wait": 0 } }).to_string().into())).await;
    }
    let end = Instant::now() + Duration::from_secs(10);
    loop {
        match next(&mut ws, end.saturating_duration_since(Instant::now())).await {
            Got::Close(c) => {
                assert_eq!(c, 4429);
                break;
            }
            Got::Json(_) => continue,
            other => panic!("expected 4429, got {other:?}"),
        }
    }

    // the global cap: 503 with Retry-After, before the upgrade
    let capped = rig(&[("HUB_STREAM_MAX", "2")]).await;
    let ann = register(&capped.hub, "ann", "person").await;
    let auth = pair(&ann.0, &ann.1);
    let _a = open(capped.full, Some(&auth)).await;
    let _b = open(capped.full, Some(&auth)).await;
    let (s, head, body) = raw(capped.full, "GET", "/api/stream", &upgrade(&[("X-Org-Auth", auth.as_str())])).await;
    assert_eq!(s, 503, "{head}");
    assert!(head.contains("retry-after: 30") && body.contains("too many streams"), "{head} {body}");

    // 128 streams not yet welcomed
    let many = rig(&[("HUB_STREAM_TIMERS_MS", "60000,25000,60000,120000,30000")]).await;
    let mut pending = Vec::new();
    for _ in 0..128 {
        pending.push(open(many.full, None).await);
    }
    let (s, _, _) = raw(many.full, "GET", "/api/stream", &upgrade(&[])).await;
    assert_eq!(s, 503, "the 129th handshake before any welcome");
    drop(pending);

    // idle with no request: 4408
    let idle = rig(&[("HUB_STREAM_TIMERS_MS", "10000,25000,60000,500,30000")]).await;
    let ann = register(&idle.hub, "ann", "person").await;
    let mut ws = open(idle.full, Some(&pair(&ann.0, &ann.1))).await;
    hello(&mut ws, None).await;
    expect_close(&mut ws, 4408).await;

    // a client that sends nothing, not even a pong, is dropped without a close handshake
    let quiet = rig(&[("HUB_STREAM_TIMERS_MS", "10000,100,700,60000,30000")]).await;
    let ann = register(&quiet.hub, "ann", "person").await;
    let mut ws = open(quiet.full, Some(&pair(&ann.0, &ann.1))).await;
    hello(&mut ws, None).await;
    say(&mut ws, json!({ "type": "poll", "id": 1, "body": { "wait": 55 } })).await;
    // do not read: the client library answers pings only while it reads
    tokio::time::sleep(Duration::from_millis(2500)).await;
    let mut ended = false;
    let end = Instant::now() + Duration::from_secs(5);
    while Instant::now() < end {
        match next(&mut ws, Duration::from_secs(1)).await {
            Got::Ended | Got::Close(_) => {
                ended = true;
                break;
            }
            _ => {}
        }
    }
    assert!(ended, "the silent client was dropped");
    println!("  ok  limits: 16 per address (17th 4429), 64 KiB (1009), 30 per 10 s (4429), 503 caps, idle 4408, no-pong drop");
}

// ------------------------------------------------------------------ T12

#[tokio::test]
async fn t12_every_request_re_checks_the_credentials() {
    let r = rig(&[]).await;
    let ann = register(&r.hub, "ann", "person").await;
    let auth = pair(&ann.0, &ann.1);
    let mut ws = open(r.full, Some(&auth)).await;
    hello(&mut ws, None).await;
    say(&mut ws, json!({ "type": "poll", "id": 1, "body": { "wait": 0 } })).await;
    assert_eq!(json_frame(&mut ws).await["status"], 200);

    let un = Call::new("POST", "/api/unregister").auth(auth.clone()).send(&r.hub).await;
    assert_eq!(un.code(), 200);
    say(&mut ws, json!({ "type": "poll", "id": 2, "body": { "wait": 0 } })).await;
    let a = json_frame(&mut ws).await;
    assert_eq!((a["status"].clone(), a["id"].clone()), (json!(401), json!(2)), "{a}");

    let again = Call::new("POST", "/api/register").auth(auth.clone()).json(json!({ "slug": ann.0, "org_name": "ann", "username": "stream", "kind": "person" })).send(&r.hub).await;
    assert_eq!(again.code(), 200, "{}", again.text());
    say(&mut ws, json!({ "type": "poll", "id": 3, "body": { "wait": 0 } })).await;
    assert_eq!(json_frame(&mut ws).await["status"], 200, "the stream stayed open and works again");
    println!("  ok  unregistered mid-stream: the next answer is 401 and the stream stays; registered again: 200");
}

// ------------------------------------------------------------------ T13

#[tokio::test]
async fn t13_shutdown_answers_then_closes_1012() {
    let r = rig(&[]).await;
    let ann = register(&r.hub, "ann", "person").await;
    let auth = pair(&ann.0, &ann.1);
    let mut socks = Vec::new();
    for _ in 0..16 {
        let mut ws = open(r.full, Some(&auth)).await;
        hello(&mut ws, None).await;
        socks.push(ws);
    }
    let bob = register(&r.hub, "bob", "person").await;
    for _ in 0..4 {
        let mut ws = open(r.full, Some(&pair(&bob.0, &bob.1))).await;
        hello(&mut ws, None).await;
        socks.push(ws);
    }
    say(&mut socks[0], json!({ "type": "poll", "id": 1, "body": { "wait": 55 } })).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let t = Instant::now();
    r.hub.shutdown.cancel();
    r.hub.presence.wake_all();
    let a = json_frame(&mut socks[0]).await; // the parked poll answers (empty)
    assert_eq!((a["id"].clone(), a["status"].clone()), (json!(1), json!(200)), "{a}");
    for ws in socks.iter_mut() {
        expect_close(ws, 1012).await;
    }
    tokio::time::timeout(Duration::from_secs(3), r.serving).await.expect("the hub exits within 3 s with 20 streams open").unwrap();
    assert!(t.elapsed() < Duration::from_secs(5), "{:?}", t.elapsed());
    println!("  ok  shutdown: parked poll answered, 20 streams closed 1012, the server ended in {:?}", t.elapsed());
}

// ------------------------------------------------------------------ T14

#[tokio::test]
async fn t14_the_log_lines() {
    mailhub::log::arm_capture();
    let r = rig(&[]).await;
    let (ann, bob) = (register(&r.hub, "ann", "person").await, register(&r.hub, "bob", "org").await);
    let text = format!("T14TEXT-{}", token_hex16());
    send(&r.hub, &bob, &ann.0, "m1", &text).await;
    let mut ws = open(r.full, Some(&pair(&ann.0, &ann.1))).await;
    say(&mut ws, json!({ "type": "hello", "v": 1, "client": "t14/1.0 (test)" })).await;
    let _ = json_frame(&mut ws).await;
    say(&mut ws, json!({ "type": "sync", "id": 1, "body": { "device_id": "d1", "wait": 0 } })).await;
    let a = json_frame(&mut ws).await;
    assert_eq!(a["status"], 200);
    let _ = ws.close(None).await;
    tokio::time::sleep(Duration::from_millis(500)).await;

    let lines: Vec<Value> = mailhub::log::captured_lines().iter().filter_map(|l| serde_json::from_str(l).ok()).collect();
    let mine = |l: &&Value| l["slugs"].as_array().is_some_and(|s| s.contains(&json!(ann.0)));
    let handshake = lines.iter().filter(mine).find(|l| l["path"] == "/api/stream").expect("the handshake line");
    assert_eq!(handshake["status"], 101);
    let answered = lines.iter().filter(mine).find(|l| l["path"] == "/api/sync" && l["via"] == "stream").expect("the answered request line, via stream");
    assert_eq!(answered["status"], 200);
    let open_line = lines.iter().find(|l| l["stream"] == "open" && l["slugs"] == json!([ann.0])).expect("the open line");
    assert_eq!((open_line["door"].clone(), open_line["client"].clone()), (json!(false), json!("t14/1.0 (test)")));
    let close_line = lines.iter().find(|l| l["stream"] == "close" && l["slugs"] == json!([ann.0])).expect("the close line");
    assert_eq!(close_line["answers"], 1);
    assert!(close_line["secs"].is_number() && close_line["code"].is_number() && close_line["by"].is_string(), "{close_line}");
    // plain HTTP lines carry no `via`
    assert!(lines.iter().filter(|l| l["path"] == "/api/send").all(|l| l.get("via").is_none()));
    let all = mailhub::log::captured_lines().join("\n");
    assert!(!all.contains(&ann.1) && !all.contains(&text), "no secret and no message text in the log");
    println!("  ok  handshake, via:stream, open and close lines; no secret or text logged");
}

// ------------------------------------------------------------------ T15

#[tokio::test]
async fn t15_a_parked_stream_request_is_presence() {
    let r = rig(&[]).await;
    let ann = register(&r.hub, "ann", "person").await;
    let mut ws = open(r.full, Some(&pair(&ann.0, &ann.1))).await;
    hello(&mut ws, None).await;
    say(&mut ws, json!({ "type": "poll", "id": 1, "body": { "wait": 55 } })).await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(r.hub.presence.online(&ann.0), "online while a stream request is parked");
    let info = Call::new("GET", "/ui/data").send(&r.hub).await.json();
    assert_eq!(info["streams"], json!({ "open": 1, "welcomed": 1 }), "the operator view counts streams: {info}");
    drop(ws);
    tokio::time::sleep(Duration::from_millis(500)).await;
    r.hub.presence.advance(Duration::from_secs(300));
    assert!(!r.hub.presence.online(&ann.0), "offline once the stream is gone and the grace has passed");
    println!("  ok  parked stream request = online; dropped stream ends it; /ui/data counts streams");
}
