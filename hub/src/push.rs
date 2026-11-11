//! Optional push: secret subscriptions, durable coalesced wakes and bounded
//! outbound delivery.
//!
//! UnifiedPush (the apps): nothing in the payload identifies any mail; the
//! app wakes and fetches. Web Push (a browser, v2.5.1, `kind` `webpush`): a
//! browser must show something for every push, so the payload is the
//! notification itself (`crate::webpush`), encrypted to the browser, signed
//! with the key the browser registered, and sent only when there is unread
//! mail it has not announced.

use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::sync::Arc;
use std::time::Duration;

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use deadpool_postgres::GenericClient;
use futures::{stream, StreamExt};
use http::StatusCode;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use web_push_native::{p256::PublicKey, Auth, WebPushBuilder};

use crate::api::{ok, refuse, ApiResult, Hub, Req};
use crate::{auth, clock, db, webpush};

#[cfg(all(feature = "push-test", not(debug_assertions)))]
compile_error!("push-test permits local HTTP endpoints and must not be used in a release build");

const CONCURRENCY: usize = 4;
// Keep below the five-second registration cooldown. An identical registration
// can pull its lease forward; the per-host busy set also holds through finish.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(4);
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
            let url = reqwest::Url::parse(&format!("https://{entry}/")).map_err(|_| {
                anyhow::anyhow!("HUB_PUSH_ALLOW entries must be bare hosts or CIDRs")
            })?;
            if url.port().is_some() || url.host_str().is_none() {
                anyhow::bail!("HUB_PUSH_ALLOW entries must be bare hosts or CIDRs");
            }
            out.push(Allowed::Host(
                url.host_str()
                    .unwrap()
                    .trim_matches(['[', ']'])
                    .trim_end_matches('.')
                    .to_string(),
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
                        IpAddr::V6(ip) => ip
                            .to_ipv4_mapped()
                            .is_some_and(|ip| net.contains(&IpAddr::V4(ip))),
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
    /// A browser's subscription (`kind` `webpush`); None is UnifiedPush.
    web: Option<Web>,
}

/// Never derive Debug/Serialize: `vapid` signs this subscription's pushes.
struct Web {
    vapid: Vec<u8>,
    navigate: String,
    preview: bool,
}

/// tokio-postgres logs parameters at DEBUG even though our SQL wrapper does
/// not. Give the driver a redacted Debug view while preserving the wire value.
struct SecretParam<T>(T);

impl<T> std::fmt::Debug for SecretParam<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("[redacted]")
    }
}

impl<T: tokio_postgres::types::ToSql> tokio_postgres::types::ToSql for SecretParam<T> {
    fn to_sql(
        &self,
        ty: &tokio_postgres::types::Type,
        out: &mut bytes::BytesMut,
    ) -> Result<tokio_postgres::types::IsNull, Box<dyn std::error::Error + Sync + Send>> {
        self.0.to_sql(ty, out)
    }

    fn accepts(ty: &tokio_postgres::types::Type) -> bool {
        T::accepts(ty)
    }

    tokio_postgres::types::to_sql_checked!();
}

fn endpoint_url(endpoint: &str) -> Result<reqwest::Url, &'static str> {
    if endpoint.len() > 1000
        || endpoint
            .bytes()
            .any(|b| b.is_ascii_control() || b.is_ascii_whitespace())
    {
        return Err("endpoint must be an HTTPS URL of at most 1000 bytes");
    }
    let url = reqwest::Url::parse(endpoint).map_err(|_| "endpoint must be an HTTPS URL")?;
    let test_http = cfg!(feature = "push-test")
        && url.scheme() == "http"
        && url
            .host_str()
            .is_some_and(|h| matches!(h, "127.0.0.1" | "[::1]"));
    if (url.scheme() != "https" && !test_http)
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
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
            (s[0] & 0xe000) == 0x2000
                && s[0] != 0x2002
                && !(s[0] == 0x2001 && (s[1] < 0x0200 || s[1] == 0x0db8))
                && !(s[0] == 0x3fff && s[1] < 0x1000)
        }
    }
}

#[derive(Clone, Copy)]
enum ResolveError {
    Busy,
    Refused,
}

struct AddressLookup(Arc<papaya::HashSet<String>>, String);

impl Drop for AddressLookup {
    fn drop(&mut self) {
        self.0.pin().remove(&self.1);
    }
}

/// Resolve immediately before connecting and pin the validated results in
/// the HTTP client. No second DNS lookup, proxies, redirects or local targets.
async fn addresses(
    url: &reqwest::Url,
    allow: &[Allowed],
    dns: &Arc<tokio::sync::Semaphore>,
    registration: Option<(Arc<papaya::HashSet<String>>, String)>,
) -> Result<Vec<SocketAddr>, ResolveError> {
    let host = url
        .host_str()
        .ok_or(ResolveError::Refused)?
        .trim_matches(['[', ']']);
    let port = url.port_or_known_default().ok_or(ResolveError::Refused)?;
    let resolved: Vec<SocketAddr> = if let Ok(ip) = host.parse::<IpAddr>() {
        vec![SocketAddr::new(ip, port)]
    } else {
        let address = if let Some((set, slug)) = registration {
            if !set.pin().insert(slug.clone()) {
                return Err(ResolveError::Busy);
            }
            Some(AddressLookup(set, slug))
        } else {
            None
        };
        let permit = dns
            .clone()
            .try_acquire_owned()
            .map_err(|_| ResolveError::Busy)?;
        let name = host.to_string();
        let lookup = tokio::task::spawn_blocking(move || {
            // Dropping the timeout future does not cancel getaddrinfo. Keep
            // its permit inside the blocking task until the OS call returns.
            let _permit = permit;
            let _address = address;
            (name.as_str(), port)
                .to_socket_addrs()
                .map(|v| v.take(32).collect::<Vec<_>>())
        });
        tokio::time::timeout(Duration::from_secs(3), lookup)
            .await
            .map_err(|_| ResolveError::Refused)?
            .map_err(|_| ResolveError::Refused)?
            .map_err(|_| ResolveError::Refused)?
    };
    let mut out = Vec::new();
    for addr in resolved {
        if !(address_allowed(host, addr.ip(), allow)
            || cfg!(feature = "push-test") && addr.ip().is_loopback())
        {
            return Err(ResolveError::Refused);
        }
        out.push(addr);
    }
    if out.is_empty() {
        return Err(ResolveError::Refused);
    }
    Ok(out)
}

fn subscription(body: &serde_json::Map<String, Value>) -> Result<Subscription, &'static str> {
    let endpoint = body
        .get("endpoint")
        .and_then(Value::as_str)
        .ok_or("endpoint is required")?;
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
    // No `kind`, or `unifiedpush`: as before, whatever else the body holds.
    let web = match body.get("kind") {
        None => None,
        Some(Value::String(k)) if k == "unifiedpush" => None,
        Some(Value::String(k)) if k == "webpush" => Some(Web {
            vapid: webpush::vapid_key(body.get("vapid").and_then(Value::as_str).ok_or("vapid is required for kind webpush")?)?,
            navigate: webpush::navigate(
                body.get("navigate").and_then(Value::as_str).ok_or("navigate is required for kind webpush")?,
                cfg!(feature = "push-test"),
            )?,
            preview: match body.get("preview") {
                None => true,
                Some(Value::Bool(on)) => *on,
                Some(_) => return Err("preview must be true or false"),
            },
        }),
        Some(_) => return Err("kind must be unifiedpush or webpush"),
    };
    Ok(Subscription {
        endpoint: endpoint.to_string(),
        p256dh: key,
        auth,
        web,
    })
}

/// POST /api/push {slug?, device_id, endpoint, p256dh, auth} and for a
/// browser {kind: "webpush", vapid, navigate, preview?} besides, or
/// DELETE /api/push?slug=...&device_id=... . Shared identity credentials
/// retain their existing authority; device credentials act only for self.
/// No body, endpoint, secret or result is instrumented here.
pub async fn registration(hub: &Arc<Hub>, req: &mut Req, remove: bool) -> ApiResult {
    // A PostgreSQL constraint error can include the failing row. Do not let
    // it reach the generic request logger with capability-bearing columns.
    registration_inner(hub, req, remove)
        .await
        .map_err(|e| match e {
            crate::api::ApiError::Internal(_) => {
                crate::api::ApiError::Internal(anyhow::anyhow!("push registration storage failed"))
            }
            other => other,
        })
}

async fn registration_inner(hub: &Arc<Hub>, req: &mut Req, remove: bool) -> ApiResult {
    let body = if remove {
        serde_json::Map::new()
    } else {
        req.json_object_strict().await?
    };
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
    .filter(|s| {
        !s.is_empty()
            && s.len() <= crate::api::sync::DEVICE_ID_MAX
            && s.bytes().all(|b| (0x21..=0x7e).contains(&b))
    })
    .ok_or_else(|| {
        crate::api::ApiError::Http(
            StatusCode::UNPROCESSABLE_ENTITY,
            "device_id is required (1-64 printable ASCII characters)".into(),
        )
    })?
    .to_string();
    let pairs = auth::pairs(req.auth_header());
    let c = hub.db.get().await?;
    let callers = auth::authenticate_callers(&c, &pairs).await?;
    if callers.is_empty() {
        return refuse(StatusCode::UNAUTHORIZED, "no valid org credentials");
    }
    let slugs: Vec<String> = callers.iter().map(|c| c.slug.clone()).collect();
    let slug = crate::api::mail::one_address(&slugs, asked.as_ref())?;
    drop(c); // DNS must never monopolize a database connection.
    let sub = if remove {
        None
    } else {
        let sub = match subscription(&body) {
            Ok(s) => s,
            Err(e) => return refuse(StatusCode::UNPROCESSABLE_ENTITY, e),
        };
        if let Err(e) = addresses(
            &endpoint_url(&sub.endpoint).expect("validated endpoint"),
            &hub.cfg.push_allow,
            &hub.push_dns,
            Some((hub.push_dns_addresses.clone(), slug.clone())),
        )
        .await
        {
            let (status, message) = match e {
                ResolveError::Busy => (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "push resolver busy; retry later",
                ),
                ResolveError::Refused => (
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "endpoint cannot be reached under this hub's endpoint policy (HUB_PUSH_ALLOW)",
                ),
            };
            return refuse(status, message);
        }
        Some(sub)
    };
    // Same identity row lock as key rotation/enrolment: credentials cannot
    // be revoked between the final authorization and this write.
    let mut c = hub.db.get().await?;
    let tx = c.transaction().await?;
    if db::query_opt(
        &tx,
        "SELECT slug FROM identities WHERE slug = $1 FOR UPDATE",
        &[&slug],
    )
    .await?
    .is_none()
    {
        return refuse(StatusCode::UNAUTHORIZED, "no valid org credentials");
    }
    let callers = auth::authenticate_callers(&tx, &pairs).await?;
    if !callers.iter().any(|c| {
        c.slug == slug && (c.device.is_none() || c.device.as_deref() == Some(device.as_str()))
    }) {
        return refuse(
            StatusCode::UNAUTHORIZED,
            "credentials must authorize this device",
        );
    }
    if remove {
        db::execute(
            &tx,
            "DELETE FROM device_push WHERE slug = $1 AND device_id = $2",
            &[&slug, &device],
        )
        .await?;
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
            // A browser's row starts at the newest message there is: mail
            // from before the registration is not announced. The same
            // subscription registered again keeps what it has announced.
            "INSERT INTO device_push (slug, device_id, registration, endpoint, p256dh, auth, next_attempt, destination,
                                      kind, vapid, navigate, preview, notified_until)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12,
                     CASE WHEN $9 = 'webpush' THEN (SELECT COALESCE(max(n), 0) FROM messages) END)
             ON CONFLICT (slug, device_id) DO UPDATE SET
               registration = CASE WHEN (device_push.endpoint, device_push.p256dh, device_push.auth, device_push.kind, device_push.vapid)
                 IS DISTINCT FROM (EXCLUDED.endpoint, EXCLUDED.p256dh, EXCLUDED.auth, EXCLUDED.kind, EXCLUDED.vapid)
                 THEN EXCLUDED.registration ELSE device_push.registration END,
               next_attempt = CASE WHEN device_push.next_attempt IS NOT NULL OR
                 (device_push.endpoint, device_push.p256dh, device_push.auth, device_push.kind, device_push.vapid)
                 IS DISTINCT FROM (EXCLUDED.endpoint, EXCLUDED.p256dh, EXCLUDED.auth, EXCLUDED.kind, EXCLUDED.vapid)
                 THEN GREATEST(EXCLUDED.next_attempt, device_push.last_attempt + interval '5 seconds') ELSE NULL END,
               notified_until = CASE WHEN EXCLUDED.kind = 'webpush' AND device_push.kind = 'webpush'
                 THEN device_push.notified_until ELSE EXCLUDED.notified_until END,
               endpoint = EXCLUDED.endpoint, p256dh = EXCLUDED.p256dh, auth = EXCLUDED.auth,
               kind = EXCLUDED.kind, vapid = EXCLUDED.vapid, navigate = EXCLUDED.navigate, preview = EXCLUDED.preview,
               destination = EXCLUDED.destination, attempts = 0",
            &[
                &slug,
                &device,
                &uuid::Uuid::new_v4().to_string(),
                &SecretParam(&s.endpoint),
                &SecretParam(&s.p256dh),
                &SecretParam(&s.auth),
                &clock::now(),
                &hex::encode(Sha256::digest(endpoint_url(&s.endpoint).expect("validated endpoint").host_str().unwrap().trim_end_matches('.').as_bytes())),
                &(if s.web.is_some() { "webpush" } else { "unifiedpush" }),
                &SecretParam(s.web.as_ref().map(|w| &w.vapid)),
                &s.web.as_ref().map(|w| &w.navigate),
                &s.web.as_ref().is_none_or(|w| w.preview),
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
    queue_at(tx, slug, clock::now()).await
}

/// `queue`, but a wake not already due goes no earlier than `at` (v2.4.0: a
/// message waiting for its translation holds it up to `translate::HOLD`).
pub async fn queue_at(tx: &impl GenericClient, slug: &str, at: chrono::DateTime<chrono::Utc>) -> Result<(), tokio_postgres::Error> {
    db::execute(
        tx,
        "UPDATE device_push SET pending = pending + 1,
           next_attempt = COALESCE(next_attempt, GREATEST($2::timestamptz, last_sent + interval '5 seconds')),
           attempts = CASE WHEN next_attempt IS NULL THEN 0 ELSE attempts END WHERE slug = $1",
        &[&slug, &at],
    )
    .await?;
    Ok(())
}

/// A held wake need wait no longer (its translation is in, or will not
/// come soon): due now, or as soon as the five-second spacing allows.
pub async fn hurry(c: &impl GenericClient, slug: &str) -> Result<(), tokio_postgres::Error> {
    db::execute(
        c,
        "UPDATE device_push SET next_attempt = LEAST(next_attempt, GREATEST($2::timestamptz, last_sent + interval '5 seconds'))
          WHERE slug = $1 AND next_attempt IS NOT NULL",
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
    destination: String,
    subscription: Subscription,
    /// A browser's row: the newest message it has announced (`messages.n`).
    notified_until: Option<i64>,
}

/// Lease a small batch, then release the DB connection before any HTTP.
async fn claim(hub: &Hub, slots: usize, busy: &[String]) -> anyhow::Result<Vec<Wake>> {
    let c = hub.db.get().await?;
    let rows = db::query(
        &c,
        "WITH candidates AS (SELECT DISTINCT ON (p.destination) p.slug, p.device_id, p.next_attempt
           FROM device_push p JOIN devices d USING (slug, device_id)
           WHERE p.next_attempt <= $1 AND d.revoked_at IS NULL
             AND (d.active_until IS NULL OR d.active_until <= $1) AND NOT (p.destination = ANY($3))
             -- an app decides for itself whether another device is in use; a
             -- browser cannot (it is not running), so its push is held here
             AND (p.kind <> 'webpush' OR NOT EXISTS (SELECT 1 FROM devices o
                    WHERE o.slug = p.slug AND o.revoked_at IS NULL AND o.active_until > $1))
           ORDER BY p.destination, p.next_attempt, p.slug, p.device_id),
         due AS (SELECT p.slug, p.device_id FROM device_push p JOIN candidates c USING (slug, device_id)
           ORDER BY c.next_attempt, p.slug, p.device_id LIMIT $2 FOR UPDATE OF p SKIP LOCKED)
         UPDATE device_push p SET next_attempt = $1::timestamptz + interval '60 seconds', last_attempt = $1
           FROM due WHERE p.slug = due.slug AND p.device_id = due.device_id
         RETURNING p.slug, p.device_id, p.registration, p.pending, p.attempts, p.endpoint, p.p256dh, p.auth, p.destination,
                   p.vapid, p.navigate, p.preview, p.notified_until",
        &[&clock::now(), &(slots as i64), &busy],
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
            destination: r.get(8),
            subscription: Subscription {
                endpoint: r.get(5),
                p256dh: r.get(6),
                auth: r.get(7),
                web: match (r.get::<_, Option<Vec<u8>>>(9), r.get::<_, Option<String>>(10)) {
                    (Some(vapid), Some(navigate)) => Some(Web { vapid, navigate, preview: r.get(11) }),
                    _ => None,
                },
            },
            notified_until: r.get(12),
        })
        .collect())
}

/// What a browser's push says now: the unread mail it has not announced,
/// as one notification, and the newest of it. None: nothing to say (read
/// on another device meanwhile, deleted, or the person's status is Do not
/// disturb), and then nothing is sent: a browser's push cannot be silent.
/// The connection is released before any HTTP.
async fn notification(hub: &Hub, wake: &Wake, web: &Web) -> anyhow::Result<Option<(String, i64)>> {
    let c = hub.db.get().await?;
    let dnd = db::query_opt(&c, "SELECT 1 FROM identities WHERE slug = $1 AND status = 'dnd'", &[&wake.slug]).await?.is_some();
    if dnd {
        return Ok(None);
    }
    let rows = db::query(
        &c,
        "SELECT m.n, m.from_slug, COALESCE(NULLIF(i.org_name, ''), i.username, ''), COALESCE(t.body, m.body), m.attachments
           FROM messages m
           LEFT JOIN identities i ON i.slug = m.from_slug
           LEFT JOIN message_translations t ON t.message_n = m.n AND t.status = 'translated'
          WHERE m.to_slug = $1 AND m.from_slug <> $1 AND m.n > $2
            AND m.read_at IS NULL AND m.recipient_deleted_at IS NULL
          ORDER BY m.n DESC LIMIT 50",
        &[&wake.slug, &wake.notified_until.unwrap_or(0)],
    )
    .await?;
    let Some(newest) = rows.first().map(|r| r.get::<_, i64>(0)) else {
        return Ok(None);
    };
    let unread: i64 = db::query_one(
        &c,
        "SELECT count(*) FROM (SELECT 1 FROM messages WHERE to_slug = $1 AND from_slug <> $1
            AND read_at IS NULL AND recipient_deleted_at IS NULL LIMIT 1000) unread",
        &[&wake.slug],
    )
    .await?
    .get(0);
    let notes: Vec<webpush::Note> = rows
        .iter()
        .map(|r| webpush::Note {
            from: r.get(1),
            name: r.get(2),
            text: r.get(3),
            files: r
                .get::<_, Value>(4)
                .as_array()
                .map(|files| files.iter().filter_map(|f| f.get("name").and_then(Value::as_str).map(str::to_string)).collect())
                .unwrap_or_default(),
        })
        .collect();
    Ok(Some((webpush::payload(&notes, &web.navigate, web.preview, unread), newest)))
}

#[derive(Clone, Copy)]
enum Delivery {
    Accepted,
    Gone,
    Retry,
    Deferred,
    /// A browser's push with nothing to say: not sent, and the wake is done.
    Nothing,
}

/// `notification`: a browser's push, the payload it carries.
async fn deliver(s: &Subscription, hub: &Hub, notification: Option<&str>) -> Delivery {
    let request = async {
        let url = endpoint_url(&s.endpoint).ok()?;
        let addresses =
            match addresses(&url, &hub.cfg.push_allow, &hub.push_delivery_dns, None).await {
                Ok(addresses) => addresses,
                Err(ResolveError::Busy) => return Some(Delivery::Deferred),
                Err(ResolveError::Refused) => return None,
            };
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
        let encrypted = WebPushBuilder::new(
            s.endpoint.parse().ok()?,
            public,
            Auth::clone_from_slice(&s.auth),
        )
        .with_valid_duration(Duration::from_secs(86400))
        .build(notification.map_or(WAKE, str::as_bytes))
        .ok()?;
        let (parts, body) = encrypted.into_parts();
        // No hub credentials or identifying headers; message content only
        // inside a browser's encrypted notification.
        let request = client.post(url.clone()).headers(parts.headers);
        let request = match (&s.web, notification) {
            // Signed with the subscription's own key, as its push service
            // requires. No Topic: a push that replaced an undelivered one
            // would lose what that one announced.
            (Some(web), Some(_)) => request
                .header(http::header::AUTHORIZATION, webpush::authorization(&url, &web.vapid, &hub.cfg.push_contact, clock::now().timestamp())?)
                .header("Urgency", "high"),
            (None, None) => request.header("Topic", "hubchat-wake"),
            _ => return None,
        };
        let response = request.body(body).send().await.ok()?;
        // Response bodies are deliberately never read or logged.
        Some(match response.status().as_u16() {
            200..=299 => Delivery::Accepted,
            404 | 410 => Delivery::Gone,
            _ => Delivery::Retry,
        })
    };
    tokio::time::timeout(REQUEST_TIMEOUT, request)
        .await
        .ok()
        .flatten()
        .unwrap_or(Delivery::Retry)
}

/// One claimed wake, from what it says to its record.
async fn handle(hub: &Hub, wake: &Wake) -> anyhow::Result<()> {
    let (result, announced) = match &wake.subscription.web {
        None => (deliver(&wake.subscription, hub, None).await, None),
        Some(web) => match notification(hub, wake, web).await? {
            None => (Delivery::Nothing, None),
            Some((payload, newest)) => (deliver(&wake.subscription, hub, Some(&payload)).await, Some(newest)),
        },
    };
    finish(hub, wake, result, announced).await
}

/// `announced`: the newest message a browser's accepted push told of.
async fn finish(hub: &Hub, wake: &Wake, result: Delivery, announced: Option<i64>) -> anyhow::Result<()> {
    let c = hub.db.get().await?;
    let now = clock::now();
    match result {
        Delivery::Deferred => {
            let retry_at = now + chrono::Duration::seconds(5);
            db::execute(&c,
                "UPDATE device_push SET next_attempt = $4 WHERE slug = $1 AND device_id = $2 AND registration = $3",
                &[&wake.slug, &wake.device, &wake.registration, &retry_at]).await?;
        }
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
                "UPDATE device_push SET last_sent = $5, attempts = 0, notified_until = COALESCE($6, notified_until),
                   next_attempt = CASE WHEN pending = $4 THEN NULL ELSE $5::timestamptz + interval '5 seconds' END
                 WHERE slug = $1 AND device_id = $2 AND registration = $3",
                &[&wake.slug, &wake.device, &wake.registration, &wake.pending, &now, &announced],
            )
            .await?;
        }
        Delivery::Nothing => {
            // Mail that came meanwhile bumped `pending`: look again then.
            db::execute(
                &c,
                "UPDATE device_push SET attempts = 0,
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
    let wakes = claim(hub, CONCURRENCY, &[]).await?;
    let count = wakes.len();
    let results = stream::iter(wakes)
        .map(|wake| async move { handle(hub, &wake).await })
        .buffer_unordered(CONCURRENCY)
        .collect::<Vec<_>>()
        .await;
    for result in results {
        result?;
    }
    Ok(count)
}

pub async fn run(hub: Arc<Hub>) {
    // Tasks keep making progress even while another claim waits for the DB.
    // Dropping the JoinSet aborts all outstanding requests on shutdown.
    let mut pending = tokio::task::JoinSet::new();
    let mut busy = std::collections::HashMap::new();
    loop {
        if hub.shutdown.is_cancelled() {
            break;
        }
        if pending.len() < CONCURRENCY {
            let destinations: Vec<String> = busy.values().cloned().collect();
            let claimed = tokio::select! {
                _ = hub.shutdown.cancelled() => break,
                result = claim(&hub, CONCURRENCY - pending.len(), &destinations) => result,
            };
            match claimed {
                Ok(wakes) => {
                    for wake in wakes {
                        let destination = wake.destination.clone();
                        let hub = hub.clone();
                        let task = pending.spawn(async move { handle(&hub, &wake).await });
                        busy.insert(task.id(), destination);
                    }
                }
                Err(_) => tracing::warn!("UnifiedPush claim failed; durable wakes will retry"),
            }
        }
        tokio::select! {
            _ = hub.shutdown.cancelled() => break,
            Some(completed) = pending.join_next_with_id(), if !pending.is_empty() => {
                let (id, failed) = match completed {
                    Ok((id, result)) => (id, result.is_err()),
                    Err(error) => (error.id(), true),
                };
                busy.remove(&id);
                if failed {
                    // Never format DB/HTTP errors: some contain capabilities.
                    tracing::warn!("UnifiedPush completion failed; durable wakes will retry");
                }
            },
            _ = tokio::time::sleep(Duration::from_secs(1)) => {},
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn driver_parameters_hide_capabilities_but_preserve_wire_values() {
        use tokio_postgres::types::{ToSql, Type};
        let endpoint = SecretParam("https://push.example/capability-secret".to_string());
        let auth = SecretParam(vec![9u8; 16]);
        let parameters: &[&(dyn ToSql + Sync)] = &[&endpoint, &auth];
        assert_eq!(format!("{parameters:?}"), "[[redacted], [redacted]]");
        let mut wire = bytes::BytesMut::new();
        endpoint.to_sql_checked(&Type::TEXT, &mut wire).unwrap();
        assert_eq!(&wire[..], endpoint.0.as_bytes());
        wire.clear();
        auth.to_sql_checked(&Type::BYTEA, &mut wire).unwrap();
        assert_eq!(&wire[..], &auth.0);
    }

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
        assert!(address_allowed(
            "push.example",
            "192.168.1.9".parse().unwrap(),
            &allow
        ));
        assert!(address_allowed(
            "other.example",
            "100.64.1.2".parse().unwrap(),
            &allow
        ));
        assert!(address_allowed(
            "other.example",
            "fd00::1".parse().unwrap(),
            &allow
        ));
        assert!(!address_allowed(
            "evil.push.example",
            "192.168.1.9".parse().unwrap(),
            &allow
        ));
        assert!(!address_allowed(
            "other.example",
            "169.254.169.254".parse().unwrap(),
            &allow
        ));
        for s in [
            "https://example.com",
            "*.example.com",
            "host:1234",
            "x@y",
            "10.0.0.0/99",
        ] {
            assert!(parse_allowlist(s).is_err(), "{s}");
        }
    }
}
