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
    if pairs.is_empty() {
        return Ok(Vec::new());
    }
    let mut wanted: Vec<&str> = pairs.iter().map(|p| p.slug.as_str()).collect();
    wanted.sort_unstable();
    wanted.dedup();
    let rows = db::query(c, "SELECT slug, fingerprint FROM identities WHERE slug = ANY($1)", &[&wanted]).await?;
    let stored: std::collections::HashMap<String, String> = rows.iter().map(|r| (r.get(0), r.get(1))).collect();
    Ok(pairs
        .iter()
        .filter(|p| stored.get(&p.slug).map(|fp| ct_eq(fp.as_bytes(), fingerprint(&p.secret).as_bytes())).unwrap_or(false))
        .map(|p| p.slug.clone())
        .collect())
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
