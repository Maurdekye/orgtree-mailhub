//! Optional UnifiedPush: secret subscriptions, durable coalesced wakes and
//! bounded outbound delivery. Nothing in the payload identifies any mail.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use deadpool_postgres::GenericClient;
use futures::{stream, StreamExt};
use http::StatusCode;
use serde_json::{json, Value};
use web_push_native::{p256::PublicKey, Auth, WebPushBuilder};

use crate::api::{ok, refuse, ApiResult, Hub, Req};
use crate::{auth, clock, db};

#[cfg(all(feature = "push-test", not(debug_assertions)))]
compile_error!("push-test permits local HTTP endpoints and must not be used in a release build");

const BATCH: i64 = 16;
const CONCURRENCY: usize = 4;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(8);
const WAKE: &[u8] = b"wake";

/// Explicit operator opt-in. Hosts match exactly, never by suffix or wildcard.
#[derive(Clone, Debug)]
pub enum Allowed {
    Host(String),
    Network(ipnet::IpNet),
}

pub fn parse_allowlist(value: &str) -> anyhow::Result<Vec<Allowed>> {
    let mut out = Vec::new();
    for entry in value.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        if out.len() >= 64 {
            anyhow::bail!("HUB_PUSH_ALLOW accepts at most 64 hosts or CIDRs");
        }
        if let Ok(net) = entry.parse::<ipnet::IpNet>() {
            out.push(Allowed::Network(net));
        } else {
            // A bare host (including a literal IP), no URL/userinfo/port/path.
            if entry.len() > 253
                || entry.contains(['/', '\\', '@', '?', '#', '*'])
                || (entry.contains(':') && !(entry.starts_with('[') && entry.ends_with(']')))
                || entry.bytes().any(|b| b.is_ascii_whitespace())
            {
                anyhow::bail!("HUB_PUSH_ALLOW entries must be bare hosts or CIDRs");
            }
            let url = reqwest::Url::parse(&format!("https://{entry}/")).map_err(|_| anyhow::anyhow!("HUB_PUSH_ALLOW entries must be bare hosts or CIDRs"))?;
            if url.port().is_some() || url.host_str().is_none() {
                anyhow::bail!("HUB_PUSH_ALLOW entries must be bare hosts or CIDRs");
            }
            out.push(Allowed::Host(
                url.host_str().unwrap().trim_matches(['[', ']']).trim_end_matches('.').to_string(),
            ));
        }
    }
    Ok(out)
}

fn address_allowed(host: &str, ip: IpAddr, allow: &[Allowed]) -> bool {
    public_address(ip)
        || allow.iter().any(|entry| match entry {
            Allowed::Host(name) => name.eq_ignore_ascii_case(host.trim_end_matches('.')),
            Allowed::Network(net) => {
                net.contains(&ip)
                    || match ip {
                        IpAddr::V6(ip) => ip.to_ipv4_mapped().is_some_and(|ip| net.contains(&IpAddr::V4(ip))),
                        _ => false,
                    }
            }
        })
}

/// Never derive Debug/Serialize: the URL and auth bytes are capabilities.
struct Subscription {
    endpoint: String,
    p256dh: Vec<u8>,
    auth: Vec<u8>,
}

fn endpoint_url(endpoint: &str) -> Result<reqwest::Url, &'static str> {
    if endpoint.len() > 1000 || endpoint.bytes().any(|b| b.is_ascii_control() || b.is_ascii_whitespace()) {
        return Err("endpoint must be an HTTPS URL of at most 1000 bytes");
    }
    let url = reqwest::Url::parse(endpoint).map_err(|_| "endpoint must be an HTTPS URL")?;
    let test_http = cfg!(feature = "push-test") && url.scheme() == "http" && url.host_str().is_some_and(|h| matches!(h, "127.0.0.1" | "[::1]"));
    if (url.scheme() != "https" && !test_http) || url.host_str().is_none() || !url.username().is_empty() || url.password().is_some() || url.fragment().is_some()
    {
        return Err("endpoint must be HTTPS without user information or a fragment");
    }
    Ok(url)
}

/// Conservative globally routable ranges; mapped IPv4 follows IPv4 rules.
fn public_address(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            let [a, b, c, _] = ip.octets();
            !(ip.is_private()
                || ip.is_loopback()
                || ip.is_link_local()
                || ip.is_broadcast()
                || ip.is_documentation()
                || a == 0
                || a >= 224
                || (a == 100 && (64..=127).contains(&b))
                || (a == 192 && b == 0 && c == 0)
                || (a == 192 && b == 88 && c == 99)
                || (a == 198 && (18..=19).contains(&b)))
        }
        IpAddr::V6(ip) => {
            if let Some(v4) = ip.to_ipv4_mapped() {
                return public_address(IpAddr::V4(v4));
            }
            let s = ip.segments();
            (s[0] & 0xe000) == 0x2000 && s[0] != 0x2002 && !(s[0] == 0x2001 && (s[1] < 0x0200 || s[1] == 0x0db8)) && !(s[0] == 0x3fff && s[1] < 0x1000)
        }
    }
}

/// Resolve immediately before connecting and pin the validated results in
/// the HTTP client. No second DNS lookup, proxies, redirects or local targets.
async fn addresses(url: &reqwest::Url, allow: &[Allowed]) -> Result<Vec<SocketAddr>, &'static str> {
    let host = url.host_str().ok_or("endpoint has no host")?.trim_matches(['[', ']']);
    let port = url.port_or_known_default().ok_or("endpoint has no port")?;
    let resolved = tokio::time::timeout(Duration::from_secs(3), tokio::net::lookup_host((host, port)))
        .await
        .map_err(|_| "endpoint DNS timed out")?
        .map_err(|_| "endpoint DNS failed")?;
    let mut out = Vec::new();
    for addr in resolved.take(32) {
        if !address_allowed(host, addr.ip(), allow) && !(cfg!(feature = "push-test") && addr.ip().is_loopback()) {
            return Err("endpoint must resolve to public addresses or be allowed by the hub operator (HUB_PUSH_ALLOW)");
        }
        out.push(addr);
    }
    if out.is_empty() {
        return Err("endpoint DNS returned no address");
    }
    Ok(out)
}

fn subscription(body: &serde_json::Map<String, Value>) -> Result<Subscription, &'static str> {
    let endpoint = body.get("endpoint").and_then(Value::as_str).ok_or("endpoint is required")?;
    endpoint_url(endpoint)?;
    let key = body
        .get("p256dh")
        .and_then(Value::as_str)
        .filter(|s| s.len() == 87)
        .and_then(|s| URL_SAFE_NO_PAD.decode(s).ok())
        .ok_or("p256dh must be a base64url P-256 public key")?;
    PublicKey::from_sec1_bytes(&key).map_err(|_| "p256dh must be a P-256 public key")?;
    let auth = body
        .get("auth")
        .and_then(Value::as_str)
        .filter(|s| s.len() == 22)
        .and_then(|s| URL_SAFE_NO_PAD.decode(s).ok())
        .filter(|v| v.len() == 16)
        .ok_or("auth must be a base64url 16-byte secret")?;
    Ok(Subscription {
        endpoint: endpoint.to_string(),
        p256dh: key,
        auth,
    })
}

/// POST /api/push {slug?, device_id, endpoint, p256dh, auth}, or
/// DELETE /api/push?slug=...&device_id=... . Shared identity credentials
/// retain their existing authority; device credentials act only for self.
/// No body, endpoint, secret or result is instrumented here.
pub async fn registration(hub: &Arc<Hub>, req: &mut Req, remove: bool) -> ApiResult {
    // A PostgreSQL constraint error can include the failing row. Do not let
    // it reach the generic request logger with capability-bearing columns.
    registration_inner(hub, req, remove).await.map_err(|e| match e {
        crate::api::ApiError::Internal(_) => crate::api::ApiError::Internal(anyhow::anyhow!("push registration storage failed")),
        other => other,
    })
}

async fn registration_inner(hub: &Arc<Hub>, req: &mut Req, remove: bool) -> ApiResult {
    let body = if remove { serde_json::Map::new() } else { req.json_object_strict().await? };
    let asked = if remove {
        req.query("slug").map(|s| json!(s))
    } else {
        body.get("slug").cloned()
    };
    let device = if remove {
        req.query("device_id")
    } else {
        body.get("device_id").and_then(Value::as_str)
    }
    .filter(|s| !s.is_empty() && s.len() <= 64 && s.bytes().all(|b| (0x21..=0x7e).contains(&b)))
    .ok_or_else(|| {
        crate::api::ApiError::Http(
            StatusCode::UNPROCESSABLE_ENTITY,
            "device_id is required (1-64 printable ASCII characters)".into(),
        )
    })?
    .to_string();
    let pairs = auth::pairs(req.auth_header());
    let mut c = hub.db.get().await?;
    let callers = auth::authenticate_callers(&c, &pairs).await?;
    if callers.is_empty() {
        return refuse(StatusCode::UNAUTHORIZED, "no valid org credentials");
    }
    let slugs: Vec<String> = callers.iter().map(|c| c.slug.clone()).collect();
    let slug = crate::api::mail::one_address(&slugs, asked.as_ref())?;
    let sub = if remove {
        None
    } else {
        let sub = match subscription(&body) {
            Ok(s) => s,
            Err(e) => return refuse(StatusCode::UNPROCESSABLE_ENTITY, e),
        };
        if let Err(e) = addresses(&endpoint_url(&sub.endpoint).expect("validated endpoint"), &hub.cfg.push_allow).await {
            return refuse(StatusCode::UNPROCESSABLE_ENTITY, e);
        }
        Some(sub)
    };
    // Same identity row lock as key rotation/enrolment: credentials cannot
    // be revoked between the final authorization and this write.
    let tx = c.transaction().await?;
    if db::query_opt(&tx, "SELECT slug FROM identities WHERE slug = $1 FOR UPDATE", &[&slug])
        .await?
        .is_none()
    {
        return refuse(StatusCode::UNAUTHORIZED, "no valid org credentials");
    }
    let callers = auth::authenticate_callers(&tx, &pairs).await?;
    if !callers
        .iter()
        .any(|c| c.slug == slug && (c.device.is_none() || c.device.as_deref() == Some(device.as_str())))
    {
        return refuse(StatusCode::UNAUTHORIZED, "credentials must authorize this device");
    }
    if remove {
        db::execute(&tx, "DELETE FROM device_push WHERE slug = $1 AND device_id = $2", &[&slug, &device]).await?;
    } else if let Some(s) = sub {
        if db::query_opt(
            &tx,
            "SELECT device_id FROM devices WHERE slug = $1 AND device_id = $2 AND revoked_at IS NULL FOR UPDATE",
            &[&slug, &device],
        )
        .await?
        .is_none()
        {
            return refuse(
                StatusCode::CONFLICT,
                "sync this device before registering push; signed-out devices cannot register",
            );
        }
        db::execute(
            &tx,
            "INSERT INTO device_push (slug, device_id, registration, endpoint, p256dh, auth, next_attempt)
             VALUES ($1, $2, $3, $4, $5, $6, $7)
             ON CONFLICT (slug, device_id) DO UPDATE SET registration = EXCLUDED.registration,
               endpoint = EXCLUDED.endpoint, p256dh = EXCLUDED.p256dh, auth = EXCLUDED.auth,
               pending = 1, next_attempt = EXCLUDED.next_attempt, attempts = 0, last_sent = NULL
             WHERE (device_push.endpoint, device_push.p256dh, device_push.auth)
                 IS DISTINCT FROM (EXCLUDED.endpoint, EXCLUDED.p256dh, EXCLUDED.auth)",
            &[
                &slug,
                &device,
                &uuid::Uuid::new_v4().to_string(),
                &s.endpoint,
                &s.p256dh,
                &s.auth,
                &clock::now(),
            ],
        )
        .await?;
    }
    tx.commit().await?;
    ok(json!({"registered": !remove}))
}

/// Called in the new-mail transaction, before mailbox head locks. One row
/// per subscription bounds storage even when a distributor is unavailable.
pub async fn queue(tx: &impl GenericClient, slug: &str) -> Result<(), tokio_postgres::Error> {
    db::execute(
        tx,
        "UPDATE device_push SET pending = pending + 1,
           next_attempt = COALESCE(next_attempt, GREATEST($2::timestamptz, last_sent + interval '5 seconds')),
           attempts = CASE WHEN next_attempt IS NULL THEN 0 ELSE attempts END WHERE slug = $1",
        &[&slug, &clock::now()],
    )
    .await?;
    Ok(())
}

struct Wake {
    slug: String,
    device: String,
    registration: String,
    pending: i64,
    attempts: i32,
    subscription: Subscription,
}

/// Lease a small batch, then release the DB connection before any HTTP.
async fn claim(hub: &Hub) -> anyhow::Result<Vec<Wake>> {
    let c = hub.db.get().await?;
    let rows = db::query(
        &c,
        "WITH due AS (SELECT p.slug, p.device_id FROM device_push p
           JOIN devices d USING (slug, device_id) WHERE p.next_attempt <= $1 AND d.revoked_at IS NULL
           ORDER BY p.next_attempt, p.slug, p.device_id LIMIT $2 FOR UPDATE OF p SKIP LOCKED)
         UPDATE device_push p SET next_attempt = $1::timestamptz + interval '60 seconds'
           FROM due WHERE p.slug = due.slug AND p.device_id = due.device_id
         RETURNING p.slug, p.device_id, p.registration, p.pending, p.attempts, p.endpoint, p.p256dh, p.auth",
        &[&clock::now(), &BATCH],
    )
    .await?;
    Ok(rows
        .iter()
        .map(|r| Wake {
            slug: r.get(0),
            device: r.get(1),
            registration: r.get(2),
            pending: r.get(3),
            attempts: r.get(4),
            subscription: Subscription {
                endpoint: r.get(5),
                p256dh: r.get(6),
                auth: r.get(7),
            },
        })
        .collect())
}

#[derive(Clone, Copy)]
enum Delivery {
    Accepted,
    Gone,
    Retry,
}

async fn deliver(s: &Subscription, allow: &[Allowed]) -> Delivery {
    let request = async {
        let url = endpoint_url(&s.endpoint).ok()?;
        let addresses = addresses(&url, allow).await.ok()?;
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(3))
            .timeout(REQUEST_TIMEOUT)
            .resolve_to_addrs(url.host_str()?.trim_matches(['[', ']']), &addresses)
            .build()
            .ok()?;
        let public = PublicKey::from_sec1_bytes(&s.p256dh).ok()?;
        if s.auth.len() != 16 {
            return None;
        }
        let encrypted = WebPushBuilder::new(s.endpoint.parse().ok()?, public, Auth::clone_from_slice(&s.auth))
            .with_valid_duration(Duration::from_secs(86400))
            .build(WAKE)
            .ok()?;
        let (parts, body) = encrypted.into_parts();
        // No hub credentials, identifying headers or message content.
        let response = client
            .post(url)
            .headers(parts.headers)
            .header("Topic", "hubchat-wake")
            .body(body)
            .send()
            .await
            .ok()?;
        // Response bodies are deliberately never read or logged.
        Some(match response.status().as_u16() {
            200..=299 => Delivery::Accepted,
            404 | 410 => Delivery::Gone,
            _ => Delivery::Retry,
        })
    };
    tokio::time::timeout(REQUEST_TIMEOUT, request).await.ok().flatten().unwrap_or(Delivery::Retry)
}

async fn finish(hub: &Hub, wake: &Wake, result: Delivery) -> anyhow::Result<()> {
    let c = hub.db.get().await?;
    let now = clock::now();
    match result {
        Delivery::Gone => {
            db::execute(
                &c,
                "DELETE FROM device_push WHERE slug = $1 AND device_id = $2 AND registration = $3",
                &[&wake.slug, &wake.device, &wake.registration],
            )
            .await?;
        }
        Delivery::Accepted => {
            db::execute(
                &c,
                "UPDATE device_push SET last_sent = $5, attempts = 0,
                   next_attempt = CASE WHEN pending = $4 THEN NULL ELSE $5::timestamptz + interval '5 seconds' END
                 WHERE slug = $1 AND device_id = $2 AND registration = $3",
                &[&wake.slug, &wake.device, &wake.registration, &wake.pending, &now],
            )
            .await?;
        }
        Delivery::Retry => {
            let attempts = (wake.attempts + 1).min(10);
            let retry_at = now + chrono::Duration::seconds((5i64 << attempts).min(3600));
            db::execute(
                &c,
                "UPDATE device_push SET attempts = $4, next_attempt = $5 WHERE slug = $1 AND device_id = $2 AND registration = $3",
                &[&wake.slug, &wake.device, &wake.registration, &attempts, &retry_at],
            )
            .await?;
        }
    }
    Ok(())
}

/// One bounded iteration, also used by the isolated integration proof.
pub async fn dispatch_due(hub: &Arc<Hub>) -> anyhow::Result<usize> {
    let wakes = claim(hub).await?;
    let count = wakes.len();
    let results = stream::iter(wakes)
        .map(|wake| async move {
            let result = deliver(&wake.subscription, &hub.cfg.push_allow).await;
            finish(hub, &wake, result).await
        })
        .buffer_unordered(CONCURRENCY)
        .collect::<Vec<_>>()
        .await;
    for result in results {
        result?;
    }
    Ok(count)
}

pub async fn run(hub: Arc<Hub>) {
    loop {
        tokio::select! {
            _ = hub.shutdown.cancelled() => break,
            result = dispatch_due(&hub) => if result.is_err() {
                // DB/HTTP errors are not formatted: some contain capabilities.
                tracing::warn!("UnifiedPush dispatch failed; durable wakes will retry");
            },
        }
        tokio::select! {
            _ = hub.shutdown.cancelled() => break,
            _ = tokio::time::sleep(Duration::from_secs(1)) => {},
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_safety() {
        for s in [
            "http://example.com/x",
            "https://user:secret@example.com/x",
            "https://example.com/x#secret",
            "file:///x",
            "https://example.com/\nx",
        ] {
            assert!(endpoint_url(s).is_err());
        }
        for s in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.1.1",
            "192.168.1.1",
            "100.64.1.1",
            "169.254.169.254",
            "0.0.0.0",
            "224.0.0.1",
            "198.18.0.1",
            "::1",
            "::ffff:127.0.0.1",
            "fc00::1",
            "fe80::1",
            "2002:7f00:1::",
            "2001:db8::1",
        ] {
            assert!(!public_address(s.parse().unwrap()), "{s}");
        }
        for s in ["1.1.1.1", "8.8.8.8", "2606:4700:4700::1111"] {
            assert!(public_address(s.parse().unwrap()));
        }
        let allow = parse_allowlist("push.example,100.64.0.0/10,fd00::/8").unwrap();
        assert!(address_allowed("push.example", "192.168.1.9".parse().unwrap(), &allow));
        assert!(address_allowed("other.example", "100.64.1.2".parse().unwrap(), &allow));
        assert!(address_allowed("other.example", "fd00::1".parse().unwrap(), &allow));
        assert!(!address_allowed("evil.push.example", "192.168.1.9".parse().unwrap(), &allow));
        assert!(!address_allowed("other.example", "169.254.169.254".parse().unwrap(), &allow));
        for s in ["https://example.com", "*.example.com", "host:1234", "x@y", "10.0.0.0/99"] {
            assert!(parse_allowlist(s).is_err(), "{s}");
        }
    }
}
