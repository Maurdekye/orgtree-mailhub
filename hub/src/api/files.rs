//! Attachments: a streamed upload owned by its uploader, bound to one
//! message by a send, downloadable by the uploader and by that message's
//! recipient. The bytes never sit in memory whole: an upload goes to disk
//! chunk by chunk, a download streams from the file.

use std::sync::Arc;

use axum::body::Body;
use futures::StreamExt;
use http::{header, HeaderValue, Response, StatusCode};
use serde_json::json;
use tokio::io::{AsyncSeekExt, AsyncWriteExt};

use super::mail::{authed, mark_seen};
use super::{ok, refuse, ApiError, ApiResult, Hub, Req};
use crate::blobs::{self, UploadFiles};
use crate::clock;
use crate::db;
use crate::wire::{latin1, pg_text, py_basename, py_int, py_prefix, py_strip};

#[tracing::instrument(level = "debug", skip_all, ret(level = "debug"), err(level = "debug", Debug))]
pub async fn upload(hub: &Arc<Hub>, req: &mut Req) -> ApiResult {
    let name = req.query_str("name", "file");
    let slugs = authed(hub, req).await?;
    if slugs.is_empty() {
        return refuse(StatusCode::UNAUTHORIZED, "no valid org credentials");
    }
    let owner = slugs[0].clone();
    // the limit is read ONCE per upload: a change applies to later uploads
    let Ok(limit) = blobs::attachment_limit(&hub.cfg) else {
        return refuse(StatusCode::SERVICE_UNAVAILABLE, "attachment limit configuration is invalid");
    };
    let too_large = format!("attachment exceeds hub limit of {limit} bytes");
    if let Some(len) = req.headers.get(header::CONTENT_LENGTH) {
        match py_int(&latin1(len.as_bytes())) {
            Some(declared) if declared >= 0 => {
                if declared > limit as i128 {
                    return refuse(StatusCode::PAYLOAD_TOO_LARGE, too_large);
                }
            }
            _ => return refuse(StatusCode::BAD_REQUEST, "invalid Content-Length"),
        }
    }
    let dir = hub.cfg.blob_dir();
    tokio::fs::create_dir_all(&dir).await?;
    let aid = uuid::Uuid::new_v4().simple().to_string();
    let mut files = UploadFiles { partial: dir.join(format!("{aid}.part")), final_path: dir.join(&aid), committed: false };
    let mut out = tokio::fs::OpenOptions::new().write(true).create_new(true).open(&files.partial).await?;
    let mut size: u64 = 0;
    let mut stream = std::mem::take(&mut req.body).into_data_stream();
    while let Some(chunk) = stream.next().await {
        // a client that hangs up mid-upload: the guard removes the partial
        let chunk = chunk.map_err(|e| ApiError::Http(StatusCode::BAD_REQUEST, format!("upload interrupted: {e}")))?;
        size += chunk.len() as u64;
        if size > limit {
            return refuse(StatusCode::PAYLOAD_TOO_LARGE, too_large);
        }
        out.write_all(&chunk).await?;
    }
    out.flush().await?;
    drop(out);
    tokio::fs::rename(&files.partial, &files.final_path).await?;
    let declared = py_prefix(py_basename(&name), 255);
    let name = pg_text(if declared.is_empty() { "file".to_string() } else { declared.to_string() });
    let c = hub.db.get().await?;
    db::execute(
        &c,
        "INSERT INTO attachments (id, owner_slug, name, bytes, created_at) VALUES ($1, $2, $3, $4, $5)",
        &[&aid, &owner, &name, &(size as i64), &clock::now()],
    )
    .await?;
    files.committed = true;
    mark_seen(hub, &c, std::slice::from_ref(&owner)).await?;
    ok(json!({ "id": aid, "bytes": size }))
}

#[tracing::instrument(level = "debug", skip_all, ret(level = "debug"), err(level = "debug", Debug))]
pub async fn download(hub: &Arc<Hub>, req: &mut Req) -> ApiResult {
    let aid = req.path.strip_prefix("/api/attachments/").unwrap_or("").to_string();
    let slugs = authed(hub, req).await?;
    if slugs.is_empty() {
        return refuse(StatusCode::UNAUTHORIZED, "no valid org credentials");
    }
    let c = hub.db.get().await?;
    let row = if aid.contains('\0') {
        None
    } else {
        db::query_opt(&c, "SELECT owner_slug, name, message_id FROM attachments WHERE id = $1", &[&aid]).await?
    };
    let Some(row) = row else { return refuse(StatusCode::NOT_FOUND, "no such attachment") };
    let owner: String = row.get(0);
    let name: String = row.get(1);
    let message: Option<String> = row.get(2);
    let mut allowed = slugs.contains(&owner);
    if !allowed {
        if let Some(mid) = message.filter(|m| !m.is_empty()) {
            if let Some(m) = db::query_opt(&c, "SELECT to_slug FROM messages WHERE id = $1", &[&mid]).await? {
                allowed = slugs.contains(&m.get::<_, String>(0));
            }
        }
    }
    drop(c);
    if !allowed {
        return refuse(StatusCode::FORBIDDEN, "not yours");
    }
    let Some(path) = blobs::blob_path(&hub.cfg.blob_dir(), &aid) else { return refuse(StatusCode::GONE, "blob expired") };
    serve_file(RangeAsk::of(req), &path, Some(&name), "application/octet-stream").await
}

/// The request's Range and If-Range, read before the response is built (the
/// request itself is not held across the file's reads).
struct RangeAsk {
    range: Option<String>,
    if_range: Option<String>,
}

impl RangeAsk {
    fn of(req: &Req) -> RangeAsk {
        RangeAsk {
            range: req.headers.get(header::RANGE).map(|v| latin1(v.as_bytes())),
            if_range: req.headers.get(header::IF_RANGE).map(|v| latin1(v.as_bytes())),
        }
    }
}

/// G6: `GET /api/messages/{id}/body[?slug=]` — a message's whole body, for
/// its sender or recipient while their copy exists: from the row, or
/// streamed from its file (with ranges, so a long one can be resumed).
#[tracing::instrument(level = "debug", skip(hub, req), ret(level = "debug"), err(level = "debug", Debug))]
pub async fn message_body(hub: &Arc<Hub>, req: &mut Req, id: &str) -> ApiResult {
    let me = super::mail::caller(hub, req).await?;
    let ranges = RangeAsk::of(req);
    let c = hub.db.get().await?;
    let row = if id.contains('\0') {
        None
    } else {
        db::query_opt(
            &c,
            "SELECT body, body_part FROM messages
              WHERE id = $1 AND ((from_slug = $2 AND sender_deleted_at IS NULL) OR (to_slug = $2 AND recipient_deleted_at IS NULL))",
            &[&id, &me],
        )
        .await?
    };
    let Some(row) = row else { return refuse(StatusCode::NOT_FOUND, "no such message") };
    mark_seen(hub, &c, std::slice::from_ref(&me)).await?;
    drop(c);
    const TEXT: &str = "text/plain; charset=utf-8";
    match row.get::<_, Option<String>>(1) {
        None => {
            let mut r = Response::new(Body::from(row.get::<_, String>(0)));
            r.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static(TEXT));
            Ok(r)
        }
        Some(part) => {
            let Some(path) = blobs::blob_path(&hub.cfg.blob_dir(), &part) else { return refuse(StatusCode::GONE, "blob expired") };
            serve_file(ranges, &path, None, TEXT).await
        }
    }
}

/// A file as Starlette's FileResponse served it: ETag, Last-Modified,
/// Accept-Ranges, and Range (one range, several as multipart/byteranges,
/// 400/416 refusals), streamed, never read whole.
async fn serve_file(ranges: RangeAsk, path: &std::path::Path, name: Option<&str>, ctype: &'static str) -> ApiResult {
    let meta = match tokio::fs::metadata(path).await {
        Ok(m) if m.is_file() => m,
        _ => return refuse(StatusCode::GONE, "blob expired"),
    };
    let size = meta.len();
    let mut file = match tokio::fs::File::open(&path).await {
        Ok(f) => f,
        Err(_) => return refuse(StatusCode::GONE, "blob expired"),
    };
    let modified: Option<chrono::DateTime<chrono::Utc>> = meta.modified().ok().map(Into::into);
    let etag = {
        use sha2::Digest;
        let stamp = modified.map(|m| m.timestamp_nanos_opt().unwrap_or(0)).unwrap_or(0);
        format!("\"{}\"", &hex::encode(sha2::Sha256::digest(format!("{stamp}-{size}")))[..32])
    };
    let mut resp = Response::new(Body::empty());
    let h = resp.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static(ctype));
    if let Some(Ok(v)) = name.map(|n| HeaderValue::from_str(&crate::wire::content_disposition(n))) {
        h.insert(header::CONTENT_DISPOSITION, v);
    }
    h.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    if let Some(m) = modified {
        if let Ok(v) = HeaderValue::from_str(&m.format("%a, %d %b %Y %H:%M:%S GMT").to_string()) {
            h.insert(header::LAST_MODIFIED, v);
        }
    }
    if let Ok(v) = HeaderValue::from_str(&etag) {
        h.insert(header::ETAG, v);
    }
    let RangeAsk { range, if_range } = ranges;
    let last_modified = resp.headers().get(header::LAST_MODIFIED).map(|v| latin1(v.as_bytes()));
    let use_range = match (&range, &if_range) {
        (None, _) => false,
        (Some(_), None) => true,
        (Some(_), Some(ir)) => Some(ir) == last_modified.as_ref() || *ir == etag,
    };
    let ranges = match range.filter(|_| use_range) {
        None => None,
        Some(r) => match parse_ranges(&r, size) {
            Ok(r) => Some(r),
            Err(RangeError::Malformed(text)) => return Ok(super::plain(StatusCode::BAD_REQUEST, text)),
            Err(RangeError::NotSatisfiable) => {
                let mut r = super::plain(StatusCode::RANGE_NOT_SATISFIABLE, "");
                if let Ok(v) = HeaderValue::from_str(&format!("bytes */{size}")) {
                    r.headers_mut().insert(header::CONTENT_RANGE, v);
                }
                r.headers_mut().insert(header::CONTENT_LENGTH, HeaderValue::from(0u64));
                return Ok(r);
            }
        },
    };
    match ranges.as_deref() {
        None => {
            resp.headers_mut().insert(header::CONTENT_LENGTH, HeaderValue::from(size));
            *resp.body_mut() = Body::from_stream(tokio_util::io::ReaderStream::with_capacity(file, CHUNK));
        }
        Some([(start, end)]) => {
            *resp.status_mut() = StatusCode::PARTIAL_CONTENT;
            if let Ok(v) = HeaderValue::from_str(&format!("bytes {start}-{}/{size}", end - 1)) {
                resp.headers_mut().insert(header::CONTENT_RANGE, v);
            }
            resp.headers_mut().insert(header::CONTENT_LENGTH, HeaderValue::from(end - start));
            file.seek(std::io::SeekFrom::Start(*start)).await?;
            let part = tokio::io::AsyncReadExt::take(file, end - start);
            *resp.body_mut() = Body::from_stream(tokio_util::io::ReaderStream::with_capacity(part, CHUNK));
        }
        Some(many) => {
            // Starlette draws 13 random bytes as hex for the boundary
            let boundary = uuid::Uuid::new_v4().simple().to_string()[..26].to_string();
            let fixed = 49 + boundary.len() as u64 + ctype.len() as u64 + size.to_string().len() as u64;
            let length: u64 = many
                .iter()
                .map(|(s, e)| s.to_string().len() as u64 + (e - 1).to_string().len() as u64 + fixed + (e - s))
                .sum::<u64>()
                + 4
                + boundary.len() as u64;
            *resp.status_mut() = StatusCode::PARTIAL_CONTENT;
            if let Ok(v) = HeaderValue::from_str(&format!("multipart/byteranges; boundary={boundary}")) {
                resp.headers_mut().insert(header::CONTENT_TYPE, v);
            }
            resp.headers_mut().insert(header::CONTENT_LENGTH, HeaderValue::from(length));
            *resp.body_mut() = Body::from_stream(multipart(file, many.to_vec(), boundary, ctype, size));
        }
    }
    Ok(resp)
}

const CHUNK: usize = 64 * 1024;

enum RangeError {
    Malformed(&'static str),
    NotSatisfiable,
}

/// Starlette's `FileResponse._parse_range_header` (what v1's downloads
/// answered): half-open byte ranges, merged when several overlap.
fn parse_ranges(header: &str, size: u64) -> Result<Vec<(u64, u64)>, RangeError> {
    let Some((units, spec)) = header.split_once('=') else { return Err(RangeError::Malformed("Malformed range header.")) };
    if py_strip(units).to_lowercase() != "bytes" {
        return Err(RangeError::Malformed("Only support bytes range"));
    }
    let size_i = size as i128;
    let mut ranges: Vec<(i128, i128)> = Vec::new();
    for part in spec.split(',') {
        let part = py_strip(part);
        if part.is_empty() || part == "-" {
            continue;
        }
        let Some((s, e)) = part.split_once('-') else { continue };
        let (s, e) = (py_strip(s), py_strip(e));
        let start = if !s.is_empty() {
            match py_int(s) {
                Some(v) => v,
                None => continue,
            }
        } else {
            match py_int(e) {
                Some(v) => size_i - v,
                None => continue,
            }
        };
        let end = if !s.is_empty() && !e.is_empty() {
            match py_int(e) {
                Some(v) if v < size_i => v + 1,
                Some(_) => size_i,
                None => continue,
            }
        } else {
            size_i
        };
        ranges.push((start, end));
    }
    if ranges.is_empty() {
        return Err(RangeError::Malformed("Range header: range must be requested"));
    }
    if ranges.iter().any(|(s, _)| !(0 <= *s && *s < size_i)) {
        return Err(RangeError::NotSatisfiable);
    }
    if ranges.iter().any(|(s, e)| s > e) {
        return Err(RangeError::Malformed("Range header: start must be less than end"));
    }
    let mut ranges: Vec<(u64, u64)> = ranges.into_iter().map(|(s, e)| (s as u64, e as u64)).collect();
    if ranges.len() == 1 {
        return Ok(ranges);
    }
    ranges.sort();
    let mut merged: Vec<(u64, u64)> = vec![ranges[0]];
    for (s, e) in ranges.into_iter().skip(1) {
        let last = merged.last_mut().expect("at least one range");
        if s <= last.1 {
            last.1 = last.1.max(e);
        } else {
            merged.push((s, e));
        }
    }
    Ok(merged)
}

/// The multipart/byteranges body Starlette writes for several ranges, read
/// from the file a chunk at a time.
fn multipart(
    file: tokio::fs::File,
    ranges: Vec<(u64, u64)>,
    boundary: String,
    ctype: &'static str,
    size: u64,
) -> impl futures::Stream<Item = std::io::Result<bytes::Bytes>> + Send {
    struct State {
        file: tokio::fs::File,
        ranges: Vec<(u64, u64)>,
        i: usize,
        pos: u64,
        in_body: bool,
        done: bool,
    }
    let start = State { file, ranges, i: 0, pos: 0, in_body: false, done: false };
    futures::stream::unfold(start, move |mut st| {
        let boundary = boundary.clone();
        async move {
            use tokio::io::AsyncReadExt;
            if st.done {
                return None;
            }
            if st.i == st.ranges.len() {
                st.done = true;
                return Some((Ok(bytes::Bytes::from(format!("--{boundary}--"))), st));
            }
            let (s, e) = st.ranges[st.i];
            if !st.in_body {
                if let Err(err) = st.file.seek(std::io::SeekFrom::Start(s)).await {
                    st.done = true;
                    return Some((Err(err), st));
                }
                st.pos = s;
                st.in_body = true;
                let head = format!("--{boundary}\r\nContent-Type: {ctype}\r\nContent-Range: bytes {s}-{}/{size}\r\n\r\n", e - 1);
                return Some((Ok(bytes::Bytes::from(head)), st));
            }
            if st.pos < e {
                let mut buf = vec![0u8; ((e - st.pos) as usize).min(CHUNK)];
                return match st.file.read(&mut buf).await {
                    Ok(0) => {
                        st.done = true;
                        Some((Err(std::io::ErrorKind::UnexpectedEof.into()), st))
                    }
                    Ok(n) => {
                        buf.truncate(n);
                        st.pos += n as u64;
                        Some((Ok(bytes::Bytes::from(buf)), st))
                    }
                    Err(err) => {
                        st.done = true;
                        Some((Err(err), st))
                    }
                };
            }
            st.i += 1;
            st.in_body = false;
            Some((Ok(bytes::Bytes::from_static(b"\r\n")), st))
        }
    })
}
