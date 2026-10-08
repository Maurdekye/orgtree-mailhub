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
use crate::wire::{latin1, pg_text, py_basename, py_int, py_prefix};

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
    let meta = match tokio::fs::metadata(&path).await {
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
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/octet-stream"));
    if let Ok(v) = HeaderValue::from_str(&crate::wire::content_disposition(&name)) {
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
    let (start, len) = match single_range(req.headers.get(header::RANGE), size) {
        Range::Full => (0, size),
        Range::Part(a, b) => {
            *resp.status_mut() = StatusCode::PARTIAL_CONTENT;
            if let Ok(v) = HeaderValue::from_str(&format!("bytes {a}-{b}/{size}")) {
                resp.headers_mut().insert(header::CONTENT_RANGE, v);
            }
            (a, b - a + 1)
        }
        Range::Unsatisfiable => {
            *resp.status_mut() = StatusCode::RANGE_NOT_SATISFIABLE;
            if let Ok(v) = HeaderValue::from_str(&format!("bytes */{size}")) {
                resp.headers_mut().insert(header::CONTENT_RANGE, v);
            }
            return Ok(resp);
        }
    };
    if start > 0 {
        file.seek(std::io::SeekFrom::Start(start)).await?;
    }
    resp.headers_mut().insert(header::CONTENT_LENGTH, HeaderValue::from(len));
    let reader = tokio_util::io::ReaderStream::with_capacity(tokio::io::AsyncReadExt::take(file, len), 64 * 1024);
    *resp.body_mut() = Body::from_stream(reader);
    Ok(resp)
}

enum Range {
    Full,
    Part(u64, u64),
    Unsatisfiable,
}

/// One `bytes=a-b`, `bytes=a-` or `bytes=-n` range; anything else (several
/// ranges, other units, malformed) is served whole, as RFC 9110 allows.
fn single_range(h: Option<&HeaderValue>, size: u64) -> Range {
    let Some(spec) = h.and_then(|v| v.to_str().ok()).and_then(|v| v.trim().strip_prefix("bytes=")) else { return Range::Full };
    if spec.contains(',') {
        return Range::Full;
    }
    let Some((a, b)) = spec.trim().split_once('-') else { return Range::Full };
    let (a, b) = (a.trim(), b.trim());
    let parse = |s: &str| s.parse::<u64>().ok();
    match (a.is_empty(), b.is_empty()) {
        (true, true) => Range::Full,
        (true, false) => match parse(b) {
            Some(0) => Range::Unsatisfiable,
            Some(n) if size == 0 => {
                let _ = n;
                Range::Unsatisfiable
            }
            Some(n) => Range::Part(size.saturating_sub(n), size - 1),
            None => Range::Full,
        },
        (false, _) => {
            let Some(start) = parse(a) else { return Range::Full };
            let end = if b.is_empty() { Some(size.saturating_sub(1)) } else { parse(b) };
            let Some(end) = end else { return Range::Full };
            if start >= size {
                return Range::Unsatisfiable;
            }
            if end < start {
                return Range::Full;
            }
            Range::Part(start, end.min(size - 1))
        }
    }
}
