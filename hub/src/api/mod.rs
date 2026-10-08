//! The HTTP surface: v1's thirteen routes, dispatched on the percent-decoded
//! path the way Starlette routed them, with v1's refusal shapes:
//! `{"detail": "<text>"}` for a refusal, `{"detail": [...]}` (422) for a bad
//! query number, plain-text `Internal Server Error` (500) for a request the
//! v1 handler crashed on, `{"detail": "Not Found"}`/`"Method Not Allowed"`
//! for unknown routes, and a 307 to the slash-twin of a path that only
//! differs by its trailing slash.

pub mod files;
pub mod history;
pub mod mail;
pub mod ops;
pub mod sync;
pub mod transfers;

use std::sync::Arc;
use std::time::Instant;

use axum::body::Body;
use bytes::Bytes;
use http::{header, HeaderValue, Method, Request, Response, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Map, Value};
use tokio_util::sync::CancellationToken;

use crate::config::Config;
use crate::db::Db;
use crate::presence::Presence;
use crate::wire;

/// The read-only operator page, unchanged from v1.
pub const INDEX_HTML: &str = include_str!("../../static/index.html");

/// Largest JSON request body read (v1 read any size; a body this large is
/// refused 413 rather than buffered).
pub const MAX_JSON_BODY: usize = 32 * 1024 * 1024;

pub struct Hub {
    pub cfg: Config,
    pub db: Db,
    pub presence: Presence,
    pub shutdown: CancellationToken,
    /// resumable uploads a request is writing to right now (one at a time)
    pub writing: papaya::HashSet<String>,
}

pub type Resp = Response<Body>;

#[derive(Debug)]
pub enum ApiError {
    /// v1's `HTTPException(status, detail)`
    Http(StatusCode, String),
    /// a framework-shaped refusal whose whole body is given
    Body(StatusCode, Value),
    /// anything v1 would have crashed on: 500, plain text
    Internal(anyhow::Error),
}

pub type ApiResult<T = Resp> = Result<T, ApiError>;

pub fn refuse<T>(status: StatusCode, detail: impl Into<String>) -> ApiResult<T> {
    Err(ApiError::Http(status, detail.into()))
}

impl From<tokio_postgres::Error> for ApiError {
    fn from(e: tokio_postgres::Error) -> Self {
        ApiError::Internal(e.into())
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        ApiError::Internal(e)
    }
}

impl From<std::io::Error> for ApiError {
    fn from(e: std::io::Error) -> Self {
        ApiError::Internal(e.into())
    }
}

pub fn json(status: StatusCode, v: &Value) -> Resp {
    let mut r = Response::new(Body::from(serde_json::to_vec(v).unwrap_or_default()));
    *r.status_mut() = status;
    r.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
    r
}

pub fn ok(v: Value) -> ApiResult {
    Ok(json(StatusCode::OK, &v))
}

pub fn plain(status: StatusCode, text: &'static str) -> Resp {
    let mut r = Response::new(Body::from(text));
    *r.status_mut() = status;
    r.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain; charset=utf-8"));
    r
}

impl ApiError {
    pub fn into_response(self) -> Resp {
        match self {
            ApiError::Http(s, detail) => json(s, &serde_json::json!({ "detail": detail })),
            ApiError::Body(s, body) => json(s, &body),
            ApiError::Internal(e) => {
                tracing::error!(error = %format!("{e:#}"), "request failed");
                plain(StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error")
            }
        }
    }
}

/// One request, as the handlers see it.
pub struct Req {
    pub method: Method,
    /// percent-decoded, as ASGI's scope["path"]
    pub path: String,
    pub headers: http::HeaderMap,
    query: Vec<(String, String)>,
    raw_query: Option<String>,
    pub body: Body,
}

impl Req {
    fn new(req: Request<Body>) -> Req {
        let (parts, body) = req.into_parts();
        let query = parts.uri.query().map(|q| form_urlencoded::parse(q.as_bytes()).into_owned().collect()).unwrap_or_default();
        let raw_query = parts.uri.query().map(str::to_string);
        Req { method: parts.method, path: wire::decoded_path(parts.uri.path()), headers: parts.headers, query, raw_query, body }
    }

    /// A query parameter; repeated names take the last value (Starlette).
    pub fn query(&self, name: &str) -> Option<&str> {
        self.query.iter().rev().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }

    pub fn query_float(&self, name: &str, default: f64) -> ApiResult<f64> {
        match self.query(name) {
            None => Ok(default),
            Some(v) => wire::parse_query_float(v)
                .ok_or_else(|| ApiError::Body(StatusCode::UNPROCESSABLE_ENTITY, wire::query_error(name, true, v))),
        }
    }

    pub fn query_int(&self, name: &str, default: i64) -> ApiResult<i64> {
        match self.query(name) {
            None => Ok(default),
            Some(v) => wire::parse_query_int(v)
                .ok_or_else(|| ApiError::Body(StatusCode::UNPROCESSABLE_ENTITY, wire::query_error(name, false, v))),
        }
    }

    pub fn query_str(&self, name: &str, default: &str) -> String {
        self.query(name).unwrap_or(default).to_string()
    }

    pub fn auth_header(&self) -> Option<&HeaderValue> {
        self.headers.get("x-org-auth")
    }

    /// `await request.json()` where the handler then calls `.get` on it:
    /// anything but a JSON object is the 500 v1 answered.
    pub async fn json_object(&mut self) -> ApiResult<Map<String, Value>> {
        let body = std::mem::take(&mut self.body);
        let bytes = read_limited(body, MAX_JSON_BODY).await?;
        match serde_json::from_slice::<Value>(&bytes) {
            Ok(Value::Object(o)) => Ok(o),
            Ok(_) => Err(ApiError::Internal(anyhow::anyhow!("request body is JSON but not an object"))),
            Err(e) => Err(ApiError::Internal(anyhow::anyhow!("request body is not JSON: {e}"))),
        }
    }

    /// The body of a v2 route (no v1 behaviour to keep): anything but a
    /// JSON object is a 400 that says so.
    pub async fn json_object_strict(&mut self) -> ApiResult<Map<String, Value>> {
        let body = std::mem::take(&mut self.body);
        let bytes = read_limited(body, MAX_JSON_BODY).await?;
        match serde_json::from_slice::<Value>(&bytes) {
            Ok(Value::Object(o)) => Ok(o),
            _ => refuse(StatusCode::BAD_REQUEST, "the request body must be a JSON object"),
        }
    }
}

async fn read_limited(body: Body, limit: usize) -> ApiResult<Bytes> {
    let limited = http_body_util::Limited::new(body, limit);
    match limited.collect().await {
        Ok(c) => Ok(c.to_bytes()),
        Err(e) if e.is::<http_body_util::LengthLimitError>() => refuse(StatusCode::PAYLOAD_TOO_LARGE, "request body too large"),
        Err(e) => Err(ApiError::Internal(anyhow::anyhow!("could not read the request body: {e}"))),
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Route {
    Register,
    Unregister,
    Poll,
    Ack,
    Send,
    Receipts,
    Upload,
    Download,
    Roster,
    Profile,
    Sync,
    Devices,
    Conversations,
    Directory,
    History,
    DeleteMessage,
    MessageBody,
    DeleteConversation,
    UploadStart,
    UploadStatus,
    UploadAppend,
    UploadCancel,
    LinkPut,
    LinkTake,
    LinkCancel,
    Health,
    Index,
    UiData,
    UiMessages,
}

/// Starlette's route table: `Some(Ok(route))` on a full match,
/// `Some(Err(allowed))` when only the method differs, `None` otherwise.
fn route(method: &Method, path: &str) -> Option<Result<Route, &'static str>> {
    use Route::*;
    // the one path with several methods: an upload in progress
    if let Some(id) = path.strip_prefix("/api/uploads/") {
        if id.is_empty() || id.contains('/') {
            return None;
        }
        return Some(if *method == Method::GET {
            Ok(UploadStatus)
        } else if *method == Method::PATCH {
            Ok(UploadAppend)
        } else if *method == Method::DELETE {
            Ok(UploadCancel)
        } else {
            Err("GET, PATCH, DELETE")
        });
    }
    let (r, m) = match path {
        "/api/register" => (Register, Method::POST),
        "/api/unregister" => (Unregister, Method::POST),
        "/api/poll" => (Poll, Method::POST),
        "/api/ack" => (Ack, Method::POST),
        "/api/send" => (Send, Method::POST),
        "/api/receipts" => (Receipts, Method::POST),
        "/api/attachments" => (Upload, Method::POST),
        "/api/roster" => (Roster, Method::GET),
        "/api/profile" => (Profile, Method::POST),
        "/api/sync" => (Sync, Method::POST),
        "/api/devices" => (Devices, Method::GET),
        "/api/conversations" => (Conversations, Method::GET),
        "/api/directory" => (Directory, Method::GET),
        "/api/history" => (History, Method::GET),
        "/api/uploads" => (UploadStart, Method::POST),
        "/api/link/put" => (LinkPut, Method::POST),
        "/api/link/take" => (LinkTake, Method::POST),
        "/api/link/cancel" => (LinkCancel, Method::POST),
        "/healthz" => (Health, Method::GET),
        "/" => (Index, Method::GET),
        "/ui/data" => (UiData, Method::GET),
        "/ui/messages" => (UiMessages, Method::GET),
        p => {
            if let Some(aid) = p.strip_prefix("/api/attachments/") {
                if aid.is_empty() || aid.contains('/') {
                    return None;
                }
                (Download, Method::GET)
            } else if let Some(id) = p.strip_prefix("/api/messages/") {
                // a message id is the client's: everything after the prefix
                // (G6: `.../body` read with GET is the message's whole body)
                if id.is_empty() {
                    return None;
                }
                match id.strip_suffix("/body") {
                    Some(mid) if !mid.is_empty() && *method == Method::GET => (MessageBody, Method::GET),
                    _ => (DeleteMessage, Method::DELETE),
                }
            } else if let Some(with) = p.strip_prefix("/api/conversations/") {
                if with.is_empty() || with.contains('/') {
                    return None;
                }
                (DeleteConversation, Method::DELETE)
            } else {
                return None;
            }
        }
    };
    let allow = if m == Method::GET {
        "GET"
    } else if m == Method::DELETE {
        "DELETE"
    } else {
        "POST"
    };
    Some(if *method == m { Ok(r) } else { Err(allow) })
}

/// The full app (port 7370): API, health and the operator UI.
#[tracing::instrument(level = "debug", skip_all, fields(method = %req.method(), path = %req.uri().path()))]
pub async fn dispatch(hub: Arc<Hub>, req: Request<Body>) -> Resp {
    if is_websocket(req.headers()) {
        return websocket_refused();
    }
    let t0 = Instant::now();
    let slugs = crate::auth::logged_slugs(req.headers().get("x-org-auth"));
    let mut req = Req::new(req);
    let path = req.path.clone();
    let resp = match route(&req.method, &path) {
        Some(Ok(r)) => handle(&hub, r, &mut req).await.unwrap_or_else(ApiError::into_response),
        Some(Err(allow)) => {
            let mut r = json(StatusCode::METHOD_NOT_ALLOWED, &serde_json::json!({ "detail": "Method Not Allowed" }));
            r.headers_mut().insert(header::ALLOW, HeaderValue::from_static(allow));
            r
        }
        None => slash_redirect(&req).unwrap_or_else(|| json(StatusCode::NOT_FOUND, &serde_json::json!({ "detail": "Not Found" }))),
    };
    crate::log::request_line(&path, &slugs, resp.status().as_u16(), t0.elapsed());
    resp
}

/// The FR-10 public listener (port 7371): a route split, not a tunnel.
/// `/api/*` and `/healthz` pass to the app; every other path is a plain-text
/// 404 that never reaches it (the operator UI is an unauthenticated view of
/// all mail).
#[tracing::instrument(level = "debug", skip_all, fields(listener = "public"))]
pub async fn dispatch_public(hub: Arc<Hub>, req: Request<Body>) -> Resp {
    if is_websocket(req.headers()) {
        return websocket_refused();
    }
    let path = wire::decoded_path(req.uri().path());
    if !(path.starts_with("/api/") || path == "/healthz") {
        let mut r = Response::new(Body::from("not found"));
        *r.status_mut() = StatusCode::NOT_FOUND;
        r.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain"));
        return r;
    }
    dispatch(hub, req).await
}

/// A WebSocket handshake, on either listener. v1 had no WebSocket route and
/// its public wrapper closed any WebSocket scope, so the server refused the
/// handshake — 403, plain text, before the app's request log ever saw it.
fn is_websocket(h: &http::HeaderMap) -> bool {
    h.get(header::UPGRADE).and_then(|v| v.to_str().ok()).map(|v| v.eq_ignore_ascii_case("websocket")).unwrap_or(false)
}

fn websocket_refused() -> Resp {
    let mut r = Response::new(Body::empty());
    *r.status_mut() = StatusCode::FORBIDDEN;
    r.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain"));
    r
}

/// Starlette's `redirect_slashes`: a path that matches no route but whose
/// slash-twin matches one (in any method) is redirected there, 307.
fn slash_redirect(req: &Req) -> Option<Resp> {
    if req.path == "/" {
        return None;
    }
    let twin = if req.path.ends_with('/') { req.path.trim_end_matches('/').to_string() } else { format!("{}/", req.path) };
    let _matched = route(&Method::GET, &twin).or_else(|| route(&Method::POST, &twin))?;
    let host = req.headers.get(header::HOST).map(|h| wire::latin1(h.as_bytes())).unwrap_or_else(|| "localhost".into());
    // RedirectResponse quotes the URL with this safe set
    const SAFE: &percent_encoding::AsciiSet = &percent_encoding::NON_ALPHANUMERIC
        .remove(b'-').remove(b'.').remove(b'_').remove(b'~').remove(b':').remove(b'/').remove(b'%').remove(b'#')
        .remove(b'?').remove(b'=').remove(b'@').remove(b'[').remove(b']').remove(b'!').remove(b'$').remove(b'&')
        .remove(b'\'').remove(b'(').remove(b')').remove(b'*').remove(b'+').remove(b',').remove(b';');
    let mut url = format!("http://{host}{twin}");
    if let Some(q) = req.raw_query.as_deref().filter(|q| !q.is_empty()) {
        url.push('?');
        url.push_str(q);
    }
    let url = percent_encoding::utf8_percent_encode(&url, SAFE).to_string();
    let mut r = Response::new(Body::empty());
    *r.status_mut() = StatusCode::TEMPORARY_REDIRECT;
    if let Ok(v) = HeaderValue::from_str(&url) {
        r.headers_mut().insert(header::LOCATION, v);
    }
    Some(r)
}

async fn handle(hub: &Arc<Hub>, r: Route, req: &mut Req) -> ApiResult {
    match r {
        Route::Register => mail::register(hub, req).await,
        Route::Unregister => mail::unregister(hub, req).await,
        Route::Poll => mail::poll(hub, req).await,
        Route::Ack => mail::ack(hub, req).await,
        Route::Send => mail::send(hub, req).await,
        Route::Receipts => mail::receipts(hub, req).await,
        Route::Roster => mail::roster_route(hub, req).await,
        Route::Profile => mail::profile(hub, req).await,
        Route::Sync => sync::sync(hub, req).await,
        Route::Devices => sync::devices(hub, req).await,
        Route::Conversations => history::conversations(hub, req).await,
        Route::Directory => mail::directory(hub, req).await,
        Route::History => history::history(hub, req).await,
        Route::DeleteMessage => {
            let id = req.path.strip_prefix("/api/messages/").unwrap_or_default().to_string();
            history::delete_message(hub, req, &id).await
        }
        Route::MessageBody => {
            let id = req.path.strip_prefix("/api/messages/").and_then(|p| p.strip_suffix("/body")).unwrap_or_default().to_string();
            files::message_body(hub, req, &id).await
        }
        Route::UploadStart => transfers::start(hub, req).await,
        Route::UploadStatus | Route::UploadAppend | Route::UploadCancel => {
            let id = req.path.strip_prefix("/api/uploads/").unwrap_or_default().to_string();
            match r {
                Route::UploadStatus => transfers::status(hub, req, &id).await,
                Route::UploadAppend => transfers::append(hub, req, &id).await,
                _ => transfers::cancel(hub, req, &id).await,
            }
        }
        Route::LinkPut => transfers::link_put(hub, req).await,
        Route::LinkTake => transfers::link_take(hub, req).await,
        Route::LinkCancel => transfers::link_cancel(hub, req).await,
        Route::DeleteConversation => {
            let with = req.path.strip_prefix("/api/conversations/").unwrap_or_default().to_string();
            history::delete_conversation(hub, req, &with).await
        }
        Route::Upload => files::upload(hub, req).await,
        Route::Download => files::download(hub, req).await,
        Route::Health => ops::healthz(hub).await,
        Route::Index => Ok(ops::index()),
        Route::UiData => ops::ui_data(hub).await,
        Route::UiMessages => ops::ui_messages(hub, req).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_like_starlette() {
        assert_eq!(route(&Method::POST, "/api/poll"), Some(Ok(Route::Poll)));
        assert_eq!(route(&Method::GET, "/api/poll"), Some(Err("POST")));
        assert_eq!(route(&Method::GET, "/api/attachments/abc"), Some(Ok(Route::Download)));
        assert_eq!(route(&Method::GET, "/api/attachments/"), None);
        assert_eq!(route(&Method::GET, "/api/attachments/a/b"), None);
        assert_eq!(route(&Method::GET, "/api/../ui/messages"), None);
        assert_eq!(route(&Method::HEAD, "/healthz"), Some(Err("GET")));
    }
}
