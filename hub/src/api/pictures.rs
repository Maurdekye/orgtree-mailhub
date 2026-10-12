//! Bounded identity pictures, independent of message attachment custody.
//! Never instrument record/body arguments or return values: they contain images.
use std::{io::Cursor, sync::Arc};

use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use http::{Method, StatusCode};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use super::{mail, ok, read_limited, refuse, sync, ApiResult, Hub, Req};
use crate::{auth, db};

pub const MAX_BYTES: usize = 256 * 1024;
const MAX_BODY: usize = MAX_BYTES * 4 / 3 + 4096;
const PREFIX: &str = "profile_picture:";

/// Public provenance, not the raw authentication fingerprint. Stable across
/// hubs/reconnects for the same identity; different secrets cannot share it.
pub fn owner_token(slug: &str, fingerprint: &str) -> String {
    hex::encode(Sha256::digest(format!("orgtree-picture-owner-v1\0{slug}\0{fingerprint}")))
}

pub fn valid_revision(s: &str) -> bool {
    s.len() == 53 && s.as_bytes()[20] == b'-'
        && s.as_bytes()[..20].iter().all(u8::is_ascii_digit)
        && s[..20].parse::<u64>().is_ok_and(|n| n > 0)
        && s.as_bytes()[21..].iter().all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(c))
}

fn invalid<T>() -> ApiResult<T> {
    refuse(StatusCode::UNPROCESSABLE_ENTITY, "invalid profile picture; use a bounded static normalized PNG")
}

/// Verify a client-normalized image without retaining arbitrary metadata/chunks.
pub fn validate(v: &Value) -> ApiResult<Value> {
    let Some(revision) = v["revision"].as_str().filter(|s| valid_revision(s)) else { return invalid(); };
    if v.get("data").is_some_and(Value::is_null) {
        return Ok(json!({"revision":revision,"sha256":null,"width":0,"height":0,"bytes":0,"data":null}));
    }
    let Some(data) = v["data"].as_str().filter(|s| s.len() <= (MAX_BYTES + 2) / 3 * 4) else { return invalid(); };
    let Ok(bytes) = B64.decode(data) else { return invalid(); };
    if bytes.len() > MAX_BYTES || !bytes.starts_with(b"\x89PNG\r\n\x1a\n") { return invalid(); }
    let mut at = 8usize;
    let mut ended = false;
    while at + 12 <= bytes.len() {
        let len = u32::from_be_bytes(bytes[at..at+4].try_into().unwrap()) as usize;
        let Some(end) = at.checked_add(12).and_then(|n| n.checked_add(len)).filter(|n| *n <= bytes.len()) else { return invalid(); };
        let kind = &bytes[at+4..at+8];
        if !matches!(kind, b"IHDR" | b"IDAT" | b"IEND") { return invalid(); }
        at = end;
        if kind == b"IEND" { ended = true; break; }
    }
    if !ended || at != bytes.len() { return invalid(); }
    let mut decoder = png::Decoder::new(Cursor::new(&bytes));
    decoder.set_limits(png::Limits { bytes: 2 * 1024 * 1024 });
    let Ok(mut reader) = decoder.read_info() else { return invalid(); };
    let info = reader.info();
    let (w, h) = (info.width, info.height);
    if w == 0 || w != h || w > 512 || info.bit_depth != png::BitDepth::Eight
        || !matches!(info.color_type, png::ColorType::Rgb | png::ColorType::Rgba) { return invalid(); }
    let mut pixels = vec![0; reader.output_buffer_size()];
    if reader.next_frame(&mut pixels).is_err() { return invalid(); }
    let hash = hex::encode(Sha256::digest(&bytes));
    if v["sha256"].as_str() != Some(hash.as_str()) || v["bytes"].as_u64() != Some(bytes.len() as u64)
        || v["width"].as_u64() != Some(w as u64) || v["height"].as_u64() != Some(h as u64) { return invalid(); }
    Ok(json!({"revision":revision,"sha256":hash,"width":w,"height":h,"bytes":bytes.len(),"data":B64.encode(bytes)}))
}

/// One current bounded record per registered identity; no unbounded blob history.
#[tracing::instrument(level = "debug", skip_all)]
pub async fn picture(hub: &Arc<Hub>, req: &mut Req) -> ApiResult {
    let pairs = auth::pairs(req.auth_header());
    let mut c = hub.db.get().await?;
    let slugs = auth::authenticate(&c, &pairs).await?;
    if slugs.is_empty() { return refuse(StatusCode::UNAUTHORIZED, "no valid org credentials"); }
    if req.method == Method::GET {
        let slug = req.query("slug").unwrap_or("");
        let row = db::query_opt(&c,
            "SELECT p.v, i.fingerprint FROM identities i LEFT JOIN hub_meta p ON p.k = 'profile_picture:' || i.slug || ':' || i.fingerprint WHERE i.slug=$1 AND i.kind IN ('person','org')",
            &[&slug]).await?;
        let Some(row) = row else { return refuse(StatusCode::NOT_FOUND, "profile picture identity not found"); };
        return ok(json!({"picture":row.get::<_,Option<Value>>(0),"picture_owner":owner_token(slug,&row.get::<_,String>(1))}));
    }
    let bytes = read_limited(std::mem::take(&mut req.body), MAX_BODY).await?;
    let Ok(body) = serde_json::from_slice::<Value>(&bytes) else { return invalid(); };
    let slug = mail::one_address(&slugs, body.get("slug"))?;
    let incoming = validate(&body["picture"])?;
    let tx = c.transaction().await?;
    sync::roster_lock(&tx).await?;
    // Revalidate after the roster lock: unregister/re-register must not let an
    // old credential mutate the new identity that occupies the same address.
    if !auth::authenticate(&tx, &pairs).await?.contains(&slug) {
        return refuse(StatusCode::UNAUTHORIZED, "no valid org credentials");
    }
    let row = db::query_opt(&tx, "SELECT kind, fingerprint FROM identities WHERE slug=$1", &[&slug]).await?;
    let Some(row) = row else { return refuse(StatusCode::UNAUTHORIZED, "no valid org credentials"); };
    if !matches!(row.get::<_,String>(0).as_str(), "person" | "org") {
        return refuse(StatusCode::FORBIDDEN, "direct sessions do not support profile pictures");
    }
    let key = format!("{PREFIX}{slug}:{}", row.get::<_,String>(1));
    let old = db::query_opt(&tx, "SELECT v FROM hub_meta WHERE k=$1", &[&key]).await?.map(|r| r.get::<_,Value>(0));
    let changed = match &old {
        Some(old) if old["revision"].as_str() == incoming["revision"].as_str() => {
            if *old != incoming { return refuse(StatusCode::CONFLICT, "picture revision already has different content"); }
            false
        }
        Some(old) => old["revision"].as_str() < incoming["revision"].as_str(),
        None => true,
    };
    if changed {
        db::execute(&tx, "INSERT INTO hub_meta(k,v) VALUES($1,$2) ON CONFLICT(k) DO UPDATE SET v=excluded.v", &[&key,&incoming]).await?;
        db::execute(&tx, "UPDATE identities SET roster_seq=nextval('roster_seq') WHERE slug=$1", &[&slug]).await?;
    }
    tx.commit().await?;
    if changed { hub.presence.roster_changed(); }
    ok(json!({"changed":changed,"picture_owner":owner_token(&slug,&row.get::<_,String>(1)),"picture":if changed { incoming } else { old.unwrap_or(incoming) }}))
}
