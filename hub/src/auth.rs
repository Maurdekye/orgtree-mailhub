//! Address ownership: `X-Org-Auth: <slug>:<secret> [<slug>:<secret> ...]`.
//!
//! The hub stores only sha256(secret) hex. A presented secret is hashed and
//! compared with the STORED fingerprint in full, in constant time. The six
//! characters an address ends in are a display label (24 bits, collidable on
//! a laptop), never a check. Secrets ride the header only and are never
//! logged: nothing in this module derives Debug on a secret.

use deadpool_postgres::GenericClient;
use sha2::{Digest, Sha256};

use crate::db;
use crate::wire::{latin1, py_split};

/// sha256(secret) as lowercase hex: all a hub ever stores.
pub fn fingerprint(secret: &str) -> String {
    hex::encode(Sha256::digest(secret.as_bytes()))
}

/// Constant-time equality (hmac.compare_digest).
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// One presented credential. The secret is private to this module's callers
/// and not printable.
pub struct Pair {
    pub slug: String,
    secret: String,
}

impl Pair {
    pub fn secret(&self) -> &str {
        &self.secret
    }
}

/// The header value as Starlette reads it (latin-1), split like v1:
/// whitespace-separated pairs, each split at its first colon; a pair with an
/// empty slug or secret is skipped.
pub fn pairs(header: Option<&http::HeaderValue>) -> Vec<Pair> {
    let Some(h) = header else { return Vec::new() };
    let text = latin1(h.as_bytes());
    py_split(&text)
        .filter_map(|p| {
            let (slug, secret) = p.split_once(':')?;
            (!slug.is_empty() && !secret.is_empty()).then(|| Pair { slug: slug.to_string(), secret: secret.to_string() })
        })
        .collect()
}

/// The slugs whose presented secret hashes to the stored fingerprint, in
/// header order (a slug presented twice counts twice, as in v1). Unknown
/// slugs and wrong secrets simply drop out: a multiplexed call proceeds for
/// the valid ones.
pub async fn authenticate(c: &impl GenericClient, pairs: &[Pair]) -> Result<Vec<String>, tokio_postgres::Error> {
    Ok(authenticate_callers(c, pairs).await?.into_iter().map(|c| c.slug).collect())
}

/// Who a credential proved to be: the address, and the device when the call
/// was signed with that device's own key (G5).
#[derive(Clone, Debug, PartialEq)]
pub struct Caller {
    pub slug: String,
    pub device: Option<String>,
}

/// How far a device-signed call's time may be from the hub's.
pub const CALL_SKEW_MS: i64 = 10 * 60 * 1000;

/// What a device signs to make a call: `X-Org-Auth: <slug>:dev1.<device_id>.<unix ms>.<signature>`,
/// the signature (Ed25519, base64url without padding) over this text.
pub fn call_message(slug: &str, device: &str, unix_ms: i64) -> String {
    format!("orgtree-hub call v2\n{slug}\n{device}\n{unix_ms}")
}

/// A device's signed credential, parsed (device ids may hold dots, so the
/// time and signature are taken from the right).
fn device_token(secret: &str) -> Option<(&str, i64, ed25519_dalek::Signature)> {
    let rest = secret.strip_prefix("dev1.")?;
    let (head, sig) = rest.rsplit_once('.')?;
    let (device, ms) = head.rsplit_once('.')?;
    if device.is_empty() || ms.is_empty() || !ms.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let sig = b64url(sig)?;
    let sig: [u8; 64] = sig.try_into().ok()?;
    Some((device, ms.parse().ok()?, ed25519_dalek::Signature::from_bytes(&sig)))
}

/// base64url without padding (with it, too), as the keys and signatures of
/// v2's device protocol are written.
pub fn b64url(s: &str) -> Option<Vec<u8>> {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(s.trim_end_matches('=')).ok()
}

/// An Ed25519 public key as the protocol writes it (base64url, 32 bytes).
pub fn verifying_key(b64: &str) -> Option<ed25519_dalek::VerifyingKey> {
    let bytes: [u8; 32] = b64url(b64)?.try_into().ok()?;
    ed25519_dalek::VerifyingKey::from_bytes(&bytes).ok()
}

/// `signature` (base64url) by `key` (base64url) over `message`.
pub fn signed_by(key: &str, message: &str, signature: &str) -> bool {
    let (Some(key), Some(sig)) = (verifying_key(key), b64url(signature)) else { return false };
    let Ok(sig) = <[u8; 64]>::try_from(sig) else { return false };
    key.verify_strict(message.as_bytes(), &ed25519_dalek::Signature::from_bytes(&sig)).is_ok()
}

/// Every credential the header proves, in header order: an address's shared
/// v1 secret (while the address keeps it on), or a call its enrolled,
/// signed-in device signed with its own key within the last ten minutes.
#[tracing::instrument(level = "debug", skip_all, fields(slugs = ?pairs.iter().map(|p| p.slug.as_str()).collect::<Vec<_>>()), ret(level = "debug"))]
pub async fn authenticate_callers(c: &impl GenericClient, pairs: &[Pair]) -> Result<Vec<Caller>, tokio_postgres::Error> {
    if pairs.is_empty() {
        return Ok(Vec::new());
    }
    let mut wanted: Vec<&str> = pairs.iter().map(|p| p.slug.as_str()).collect();
    wanted.sort_unstable();
    wanted.dedup();
    let rows = db::query(c, "SELECT slug, fingerprint, shared_key_enabled FROM identities WHERE slug = ANY($1)", &[&wanted]).await?;
    let stored: std::collections::HashMap<String, (String, bool)> = rows.iter().map(|r| (r.get(0), (r.get(1), r.get(2)))).collect();
    let tokens: Vec<Option<(&str, i64, ed25519_dalek::Signature)>> = pairs.iter().map(|p| device_token(&p.secret)).collect();
    let (mut tslugs, mut tdevices) = (Vec::new(), Vec::new());
    for (p, t) in pairs.iter().zip(&tokens) {
        if let Some((device, _, _)) = t {
            tslugs.push(p.slug.as_str());
            tdevices.push(*device);
        }
    }
    let keys: std::collections::HashMap<(String, String), String> = if tslugs.is_empty() {
        Default::default()
    } else {
        db::query(
            c,
            "SELECT d.slug, d.device_id, d.public_key FROM devices d
               JOIN unnest($1::text[], $2::text[]) AS w(slug, device_id) ON d.slug = w.slug AND d.device_id = w.device_id
              WHERE d.public_key IS NOT NULL AND d.revoked_at IS NULL",
            &[&tslugs, &tdevices],
        )
        .await?
        .iter()
        .map(|r| ((r.get(0), r.get(1)), r.get(2)))
        .collect()
    };
    let now = crate::clock::now().timestamp_millis();
    let mut out = Vec::new();
    for (p, t) in pairs.iter().zip(&tokens) {
        let Some((fp, shared)) = stored.get(&p.slug) else { continue };
        if *shared && ct_eq(fp.as_bytes(), fingerprint(&p.secret).as_bytes()) {
            out.push(Caller { slug: p.slug.clone(), device: None });
            continue;
        }
        let Some((device, ms, sig)) = t else { continue };
        let Some(key) = keys.get(&(p.slug.clone(), device.to_string())).and_then(|k| verifying_key(k)) else { continue };
        if (now - ms).abs() <= CALL_SKEW_MS && key.verify_strict(call_message(&p.slug, device, *ms).as_bytes(), sig).is_ok() {
            out.push(Caller { slug: p.slug.clone(), device: Some(device.to_string()) });
        }
    }
    Ok(out)
}

/// The secret `/api/register` reads for `slug`: v1 scanned every
/// whitespace-separated piece (colon or not) and kept the LAST one naming
/// this slug, even an empty one.
pub fn register_secret(header: Option<&http::HeaderValue>, slug: &str) -> String {
    let Some(h) = header else { return String::new() };
    let text = latin1(h.as_bytes());
    let mut secret = String::new();
    for p in py_split(&text) {
        let (s, sec) = p.split_once(':').unwrap_or((p, ""));
        if s == slug {
            secret = sec.to_string();
        }
    }
    secret
}

/// The slugs a request presented, for the request log line (never the
/// secrets): every whitespace-separated piece that has a colon.
pub fn logged_slugs(header: Option<&http::HeaderValue>) -> Vec<String> {
    let Some(h) = header else { return Vec::new() };
    let text = latin1(h.as_bytes());
    py_split(&text).filter(|p| p.contains(':')).map(|p| p.split(':').next().unwrap_or("").to_string()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_is_sha256_hex() {
        assert_eq!(fingerprint("abc"), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
    }

    #[test]
    fn pairs_parse_like_v1() {
        let h = http::HeaderValue::from_static("a.b.c:s1  x:y:z \t:nosecret noslug: bare");
        let p = pairs(Some(&h));
        let got: Vec<(&str, &str)> = p.iter().map(|p| (p.slug.as_str(), p.secret())).collect();
        assert_eq!(got, vec![("a.b.c", "s1"), ("x", "y:z")]);
        assert_eq!(logged_slugs(Some(&h)), vec!["a.b.c", "x", "", "noslug"]);
    }

    #[test]
    fn register_reads_the_last_pair_for_its_slug() {
        let h = http::HeaderValue::from_static("a:one b:two a:three");
        assert_eq!(register_secret(Some(&h), "a"), "three");
        let h = http::HeaderValue::from_static("a:one a:");
        assert_eq!(register_secret(Some(&h), "a"), "");
        let h = http::HeaderValue::from_static("a:one a");
        assert_eq!(register_secret(Some(&h), "a"), "");
        assert_eq!(register_secret(None, "a"), "");
    }

    #[test]
    fn constant_time_compare() {
        assert!(ct_eq(b"abc", b"abc"));
        assert!(!ct_eq(b"abc", b"abd"));
        assert!(!ct_eq(b"abc", b"ab"));
    }
}
