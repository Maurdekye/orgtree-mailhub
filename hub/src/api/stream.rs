//! The WebSocket stream (`GET /api/stream`, v2.5.0; Orgtree docket
//! mail-hub-websocket-for-new-mail-clients-prefer-i, spec
//! spec-hub-websocket.md): the long poll carried over one socket. A client
//! sends the requests it would long-poll (`sync` or `poll`, the same JSON
//! bodies) and gets one answer frame per request, built by the same cores
//! as the HTTP routes (`sync::sync_core`, `mail::poll_core`). The hub never
//! sends mail nobody asked for, which is also the whole backpressure story.
//!
//! Frames are JSON text. Credentials come on the upgrade (`X-Org-Auth`) or in
//! the `hello` frame (a browser cannot set headers) and are kept in memory
//! for the stream's life; every request authenticates with them again.

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::FromRequestParts;
use http::{header, HeaderName, HeaderValue, Method, Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{json, Map, Value};
use tokio::time::Instant;

use super::{json as json_resp, ApiError, ApiResult, Hub, Resp};
use crate::{auth, clock, log, wire};

pub const PATH: &str = "/api/stream";
const PROTO: &str = "orgtree-hub-stream.1";
/// largest client message
const MAX_MESSAGE: usize = 64 * 1024;
/// largest `auth` in a hello
const MAX_AUTH: usize = 8 * 1024;
/// streams one address may have welcomed at once
const PER_ADDRESS: usize = 16;
/// streams that have not been welcomed yet, hub-wide
const PRE_WELCOME: usize = 128;
/// requests one stream may make in `RATE_WINDOW`
const RATE_MAX: usize = 30;
const RATE_WINDOW: Duration = Duration::from_secs(10);
/// the highest request id (2^53 - 1: exact in every JSON reader)
const MAX_ID: u64 = (1 << 53) - 1;
const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);

#[derive(Default)]
struct Counts {
    open: usize,
    welcomed: usize,
    by_slug: HashMap<String, usize>,
}

/// How many streams are open (any state), welcomed, and per address.
#[derive(Default)]
pub struct Streams {
    inner: Mutex<Counts>,
}

impl Streams {
    /// (open, welcomed), for the operator view.
    pub fn counts(&self) -> (usize, usize) {
        let c = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        (c.open, c.welcomed)
    }
}

/// One open stream's place in the counts; leaving drops it.
struct Reservation {
    hub: Arc<Hub>,
    slugs: Option<Vec<String>>,
}

impl Reservation {
    /// Take a place, or refuse: `max` streams open, or 128 not yet welcomed.
    fn take(hub: &Arc<Hub>) -> Option<Reservation> {
        let mut c = hub.streams.inner.lock().unwrap_or_else(|e| e.into_inner());
        if c.open >= hub.cfg.stream_max || c.open - c.welcomed >= PRE_WELCOME {
            return None;
        }
        c.open += 1;
        drop(c);
        Some(Reservation { hub: hub.clone(), slugs: None })
    }

    /// Welcomed: count it against each address, unless one has 16 already.
    fn welcome(&mut self, slugs: &[String]) -> bool {
        let mut c = self.hub.streams.inner.lock().unwrap_or_else(|e| e.into_inner());
        if slugs.iter().any(|s| c.by_slug.get(s).copied().unwrap_or(0) >= PER_ADDRESS) {
            return false;
        }
        for s in slugs {
            *c.by_slug.entry(s.clone()).or_insert(0) += 1;
        }
        c.welcomed += 1;
        self.slugs = Some(slugs.to_vec());
        true
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        let mut c = self.hub.streams.inner.lock().unwrap_or_else(|e| e.into_inner());
        c.open = c.open.saturating_sub(1);
        if let Some(slugs) = self.slugs.take() {
            c.welcomed = c.welcomed.saturating_sub(1);
            for s in slugs {
                if let Some(n) = c.by_slug.get_mut(&s) {
                    *n = n.saturating_sub(1);
                    if *n == 0 {
                        c.by_slug.remove(&s);
                    }
                }
            }
        }
    }
}

/// A refusal before the upgrade: status, `{"detail"}`, maybe one header.
struct Refused(StatusCode, String, Option<(HeaderName, &'static str)>);

impl Refused {
    fn new(status: StatusCode, detail: &str, header: Option<(HeaderName, &'static str)>) -> Refused {
        Refused(status, detail.to_string(), header)
    }

    fn response(&self) -> Resp {
        let mut r = json_resp(self.0, &json!({ "detail": self.1 }));
        if let Some((k, v)) = &self.2 {
            r.headers_mut().insert(k.clone(), HeaderValue::from_static(v));
        }
        r
    }
}

fn dev1(pairs: &[auth::Pair]) -> bool {
    pairs.iter().any(|p| p.secret().starts_with("dev1."))
}

/// `GET /api/stream`: the handshake (spec H2), refusals in the spec's order,
/// logged as one ordinary request line.
pub async fn handshake(hub: Arc<Hub>, req: Request<Body>) -> Resp {
    let t0 = std::time::Instant::now();
    let slugs = auth::logged_slugs(req.headers().get("x-org-auth"));
    let door = req.extensions().get::<super::ThroughDoor>().is_some();
    let (resp, detail) = match admit(&hub, req, door).await {
        Ok(r) => (r, None),
        Err(refused) => (refused.response(), Some(refused.1)),
    };
    let status = resp.status();
    let refusal = detail.as_deref().map(|d| log::Refusal { to: None, detail: Some(d) });
    log::request_line(PATH, &slugs, status.as_u16(), t0.elapsed(), refusal);
    resp
}

async fn admit(hub: &Arc<Hub>, req: Request<Body>, door: bool) -> Result<Resp, Refused> {
    if req.method() != Method::GET {
        return Err(Refused::new(StatusCode::METHOD_NOT_ALLOWED, "Method Not Allowed", Some((header::ALLOW, "GET"))));
    }
    if !super::is_websocket(req.headers()) {
        return Err(Refused::new(StatusCode::UPGRADE_REQUIRED, "this is a WebSocket endpoint", Some((header::UPGRADE, "websocket"))));
    }
    let (mut parts, _body) = req.into_parts();
    let upgrade = match WebSocketUpgrade::from_request_parts(&mut parts, &()).await {
        Ok(u) => u,
        Err(e) => {
            return Err(Refused::new(StatusCode::BAD_REQUEST, &e.body_text(), None));
        }
    };
    let offered = parts
        .headers
        .get_all(header::SEC_WEBSOCKET_PROTOCOL)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .any(|p| p.trim() == PROTO);
    if !offered {
        return Err(Refused::new(StatusCode::BAD_REQUEST, "offer the subprotocol orgtree-hub-stream.1", None));
    }
    if !crate::web::asker(&hub.cfg, &parts.headers).may_upgrade() {
        return Err(Refused::new(StatusCode::FORBIDDEN, "this web app's address is not allowed by this hub", None));
    }
    let Some(reservation) = Reservation::take(hub) else {
        return Err(Refused::new(StatusCode::SERVICE_UNAVAILABLE, "too many streams", Some((header::RETRY_AFTER, "30"))));
    };
    let mut creds = None;
    if let Some(h) = parts.headers.get("x-org-auth") {
        let pairs = auth::pairs(Some(h));
        let dev = dev1(&pairs);
        let slugs = match super::mail::authed_pairs(hub, pairs).await {
            Ok(s) => s,
            Err(_) => Vec::new(),
        };
        if slugs.is_empty() {
            return Err(Refused::new(StatusCode::UNAUTHORIZED, "no valid org credentials", None));
        }
        if dev {
            return Err(Refused::new(StatusCode::UNAUTHORIZED, "device-signed credentials are not accepted on the stream", None));
        }
        creds = Some((h.clone(), distinct(slugs)));
    }
    let hub2 = hub.clone();
    let mut resp = upgrade
        .protocols([PROTO])
        .max_message_size(MAX_MESSAGE * 16)
        .max_frame_size(MAX_MESSAGE * 16)
        .on_upgrade(move |ws| run(hub2, ws, reservation, creds, door));
    // the 101 echoes the one subprotocol (axum does when the client offered it)
    if resp.headers().get(header::SEC_WEBSOCKET_PROTOCOL).is_none() {
        resp.headers_mut().insert(header::SEC_WEBSOCKET_PROTOCOL, HeaderValue::from_static(PROTO));
    }
    Ok(resp)
}

fn distinct(slugs: Vec<String>) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    slugs.into_iter().filter(|s| seen.insert(s.clone())).collect()
}

/// How a stream ended, for its log line.
struct End {
    code: u16,
    by: &'static str,
}

type Fut = Pin<Box<dyn Future<Output = ApiResult> + Send>>;

struct Pending {
    id: u64,
    route: &'static str,
    started: std::time::Instant,
    fut: Fut,
}

enum Verdict {
    Request(RequestFrame),
    Close(u16, &'static str),
}

struct RequestFrame {
    id: u64,
    sync: bool,
    body: Map<String, Value>,
}

/// One client frame after the welcome, checked against section 5.1.
fn read_request(text: &str, last_id: u64) -> Verdict {
    let Ok(Value::Object(mut o)) = serde_json::from_str::<Value>(text) else { return Verdict::Close(4400, "bad frame") };
    let sync = match o.get("type").and_then(Value::as_str) {
        Some("sync") => true,
        Some("poll") => false,
        _ => return Verdict::Close(4400, "unknown frame type"),
    };
    let Some(id) = o.get("id").and_then(Value::as_u64).filter(|i| (1..=MAX_ID).contains(i) && *i > last_id) else {
        return Verdict::Close(4400, "id must increase");
    };
    let Some(Value::Object(body)) = o.remove("body") else { return Verdict::Close(4400, "body must be an object") };
    Verdict::Request(RequestFrame { id, sync, body })
}

fn start(hub: &Arc<Hub>, creds: &HeaderValue, f: RequestFrame) -> Pending {
    let pairs = auth::pairs(Some(creds));
    let hub = hub.clone();
    let (route, fut): (&'static str, Fut) = if f.sync {
        ("/api/sync", Box::pin(async move { super::sync::sync_core(&hub, pairs, 25.0, f.body).await }))
    } else {
        let wait = match f.body.get("wait") {
            None | Some(Value::Null) => Ok(25.0),
            Some(Value::Number(n)) => Ok(n.as_f64().unwrap_or(0.0)),
            Some(other) => Err(match other {
                Value::String(s) => s.clone(),
                v => v.to_string(),
            }),
        };
        ("/api/poll", match wait {
            Ok(w) => Box::pin(async move { super::mail::poll_core(&hub, pairs, w).await }),
            Err(input) => Box::pin(async move { Err(ApiError::Body(StatusCode::UNPROCESSABLE_ENTITY, wire::query_error("wait", true, &input))) }),
        })
    };
    Pending { id: f.id, route, started: std::time::Instant::now(), fut }
}

/// The answer frame's status and body: what the HTTP route would have sent.
async fn answer_of(r: ApiResult) -> (u16, Value) {
    let resp = match r {
        Ok(r) => r,
        Err(e) => e.into_response(),
    };
    let status = resp.status().as_u16();
    let bytes = resp.into_body().collect().await.map(|c| c.to_bytes()).unwrap_or_default();
    let body = serde_json::from_slice::<Value>(&bytes).unwrap_or_else(|_| if status == 500 { json!({ "detail": "Internal Server Error" }) } else { Value::Null });
    (status, body)
}

fn text(v: &Value) -> Message {
    Message::Text(v.to_string().into())
}

/// Send a close frame, then give the peer a moment to answer it.
async fn close(ws: &mut WebSocket, code: u16, reason: &'static str) {
    let frame = Message::Close(Some(CloseFrame { code, reason: reason.into() }));
    if tokio::time::timeout(SHUTDOWN_GRACE, ws.send(frame)).await.is_ok() {
        let _ = tokio::time::timeout(Duration::from_millis(500), async { while let Some(Ok(_)) = ws.recv().await {} }).await;
    }
}

async fn run(hub: Arc<Hub>, mut ws: WebSocket, mut place: Reservation, creds: Option<(HeaderValue, Vec<String>)>, door: bool) {
    let started = std::time::Instant::now();
    let mut answers = 0u64;
    let mut slugs: Vec<String> = Vec::new();
    let end = session(&hub, &mut ws, &mut place, creds, door, &mut slugs, &mut answers).await;
    if place.slugs.is_some() || !slugs.is_empty() || end.code != 0 {
        log::line(&json!({
            "ts": clock::now_iso(), "stream": "close", "slugs": if place.slugs.is_some() { json!(slugs) } else { json!([]) },
            "secs": started.elapsed().as_secs(), "answers": answers, "code": end.code, "by": end.by,
        }));
    }
}

async fn session(hub: &Arc<Hub>, ws: &mut WebSocket, place: &mut Reservation, header_creds: Option<(HeaderValue, Vec<String>)>, door: bool, slugs_out: &mut Vec<String>, answers: &mut u64) -> End {
    let timers = hub.cfg.stream_timers;
    // ------------------------------------------------------------ hello
    let hello_by = Instant::now() + timers.hello;
    let hello: Map<String, Value> = loop {
        match tokio::time::timeout_at(hello_by, ws.recv()).await {
            Err(_) => {
                close(ws, 4408, "no hello").await;
                return End { code: 4408, by: "hub" };
            }
            Ok(None) | Ok(Some(Err(_))) => return End { code: 1006, by: "network" },
            Ok(Some(Ok(Message::Text(t)))) => {
                if t.len() > MAX_MESSAGE {
                    close(ws, 1009, "message too big").await;
                    return End { code: 1009, by: "hub" };
                }
                match serde_json::from_str::<Value>(t.as_str()) {
                    Ok(Value::Object(o)) if o.get("type").and_then(Value::as_str) == Some("hello") => break o,
                    _ => {
                        close(ws, 4400, "expected hello").await;
                        return End { code: 4400, by: "hub" };
                    }
                }
            }
            Ok(Some(Ok(Message::Close(f)))) => return End { code: f.map(|f| f.code).unwrap_or(1005), by: "client" },
            Ok(Some(Ok(Message::Binary(_)))) => {
                close(ws, 4400, "text frames only").await;
                return End { code: 4400, by: "hub" };
            }
            Ok(Some(Ok(_))) => continue,
        }
    };
    if !hello.get("v").and_then(Value::as_u64).is_some_and(|v| v >= 1) {
        close(ws, 4400, "bad hello").await;
        return End { code: 4400, by: "hub" };
    }
    let client = match hello.get("client") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) if s.len() <= 64 && s.bytes().all(|b| (0x20..0x7f).contains(&b)) => Some(s.clone()),
        Some(_) => {
            close(ws, 4400, "bad client").await;
            return End { code: 4400, by: "hub" };
        }
    };
    let (creds, slugs) = match (header_creds, hello.get("auth")) {
        (Some(_), Some(_)) => {
            close(ws, 4400, "send credentials once").await;
            return End { code: 4400, by: "hub" };
        }
        (Some(c), None) => c,
        (None, Some(Value::String(a))) if a.len() <= MAX_AUTH => {
            let bytes: Option<Vec<u8>> = a.chars().map(|c| u8::try_from(c as u32).ok()).collect();
            let Some(value) = bytes.and_then(|b| HeaderValue::from_bytes(&b).ok()) else {
                close(ws, 4401, "no valid org credentials").await;
                return End { code: 4401, by: "hub" };
            };
            let pairs = auth::pairs(Some(&value));
            if dev1(&pairs) {
                close(ws, 4401, "device-signed credentials are not accepted on the stream").await;
                return End { code: 4401, by: "hub" };
            }
            let slugs = super::mail::authed_pairs(hub, pairs).await.unwrap_or_default();
            if slugs.is_empty() {
                close(ws, 4401, "no valid org credentials").await;
                return End { code: 4401, by: "hub" };
            }
            (value, distinct(slugs))
        }
        _ => {
            close(ws, 4400, "credentials required").await;
            return End { code: 4400, by: "hub" };
        }
    };
    if !place.welcome(&slugs) {
        close(ws, 4429, "too many streams for this address").await;
        return End { code: 4429, by: "hub" };
    }
    *slugs_out = slugs.clone();
    let welcome = json!({
        "type": "welcome", "v": 1, "hub": hub.cfg.hub_name, "version": crate::VERSION,
        "slugs": slugs, "ping_s": timers.ping.as_secs(), "max_request": MAX_MESSAGE,
    });
    if ws.send(text(&welcome)).await.is_err() {
        return End { code: 1006, by: "network" };
    }
    log::line(&json!({ "ts": clock::now_iso(), "stream": "open", "slugs": slugs, "door": door, "client": client }));

    // ------------------------------------------------------------- requests
    let mut last_id = 0u64;
    let mut pending: Option<Pending> = None;
    let mut recent: VecDeque<Instant> = VecDeque::new();
    let mut last_rx = Instant::now();
    let mut idle_from = Instant::now();
    let mut ping = tokio::time::interval_at(Instant::now() + timers.ping, timers.ping);
    ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        let idle_at = idle_from + timers.idle;
        let heard_by = last_rx + timers.pong;
        tokio::select! {
            _ = hub.shutdown.cancelled() => {
                // a parked request answers as HTTP answers on shutdown; then 1012
                if let Some(mut p) = pending.take() {
                    if let Ok(r) = tokio::time::timeout(SHUTDOWN_GRACE, &mut p.fut).await {
                        let _ = tokio::time::timeout(SHUTDOWN_GRACE, deliver(&slugs, ws, p.id, p.route, p.started, answers, r)).await;
                    }
                }
                close(ws, 1012, "the hub is shutting down").await;
                return End { code: 1012, by: "hub" };
            }
            m = ws.recv() => {
                let Some(Ok(msg)) = m else { return End { code: 1006, by: "network" } };
                last_rx = Instant::now();
                match msg {
                    Message::Text(t) => {
                        if t.len() > MAX_MESSAGE {
                            close(ws, 1009, "message too big").await;
                            return End { code: 1009, by: "hub" };
                        }
                        match read_request(t.as_str(), last_id) {
                            Verdict::Close(code, reason) => {
                                close(ws, code, reason).await;
                                return End { code, by: "hub" };
                            }
                            Verdict::Request(f) => {
                                let now = Instant::now();
                                while recent.front().is_some_and(|t| now.duration_since(*t) > RATE_WINDOW) {
                                    recent.pop_front();
                                }
                                recent.push_back(now);
                                if recent.len() > RATE_MAX {
                                    close(ws, 4429, "too many requests").await;
                                    return End { code: 4429, by: "hub" };
                                }
                                last_id = f.id;
                                // a newer request replaces the parked one: dropped, never answered
                                pending = Some(start(hub, &creds, f));
                            }
                        }
                    }
                    Message::Binary(_) => {
                        close(ws, 4400, "text frames only").await;
                        return End { code: 4400, by: "hub" };
                    }
                    Message::Close(f) => return End { code: f.map(|f| f.code).unwrap_or(1005), by: "client" },
                    Message::Ping(_) | Message::Pong(_) => {}
                }
            }
            _ = ping.tick() => {
                if ws.send(Message::Ping(Default::default())).await.is_err() {
                    return End { code: 1006, by: "network" };
                }
            }
            _ = tokio::time::sleep_until(heard_by) => {
                // nothing, not even a pong: dropped without a close handshake
                return End { code: 1006, by: "hub" };
            }
            _ = tokio::time::sleep_until(idle_at), if pending.is_none() => {
                close(ws, 4408, "idle").await;
                return End { code: 4408, by: "hub" };
            }
            r = async { match pending.as_mut() { Some(p) => p.fut.as_mut().await, None => std::future::pending().await } } => {
                let Some(p) = pending.take() else { continue };
                match tokio::time::timeout(timers.write, deliver(&slugs, ws, p.id, p.route, p.started, answers, r)).await {
                    Ok(true) => idle_from = Instant::now(),
                    Ok(false) => return End { code: 1006, by: "network" },
                    Err(_) => {
                        close(ws, 4408, "answer not read").await;
                        return End { code: 4408, by: "hub" };
                    }
                }
            }
        }
    }
}

/// Write one answer frame and log its request line; false if the socket failed.
async fn deliver(slugs: &[String], ws: &mut WebSocket, id: u64, route: &'static str, started: std::time::Instant, answers: &mut u64, r: ApiResult) -> bool {
    if let Err(ApiError::Internal(e)) = &r {
        tracing::error!(error = %format!("{e:#}"), "request failed");
    }
    let (status, body) = answer_of(r).await;
    let detail = (400..500).contains(&status).then(|| match body.get("detail") {
        Some(Value::String(d)) => d.clone(),
        Some(other) => other.to_string(),
        None => String::new(),
    });
    let frame = json!({ "type": "answer", "id": id, "status": status, "body": body });
    if ws.send(text(&frame)).await.is_err() {
        return false;
    }
    *answers += 1;
    log::request_line_via(
        route,
        slugs,
        status,
        started.elapsed(),
        detail.as_deref().filter(|d| !d.is_empty()).map(|d| log::Refusal { to: None, detail: Some(d) }),
        Some("stream"),
    );
    true
}
