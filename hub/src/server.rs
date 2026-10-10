//! Startup and the listeners: the full app on `HUB_BIND:HUB_PORT` and, when
//! `HUB_PUBLIC` is set, the API-only public listener on
//! `HUB_PUBLIC_BIND:HUB_PUBLIC_PORT` (7371), exactly as `mailhub.serve` did.

use std::future::IntoFuture;
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::body::Body;
use axum::extract::{Request, State};
use axum::Router;
use serde_json::json;
use tokio_util::sync::CancellationToken;

use crate::api::{self, Hub};
use crate::config::Config;
use crate::db::Db;
use crate::presence::Presence;
use crate::{blobs, clock, import, log, sweep};

/// Everything up to (not including) the listeners: database, schema, the
/// one-time v1 import, leftover partial uploads.
#[tracing::instrument(level = "debug", skip_all)]
pub async fn prepare(cfg: Config) -> Result<Arc<Hub>> {
    std::fs::create_dir_all(cfg.blob_dir()).with_context(|| format!("could not create {}", cfg.blob_dir().display()))?;
    let db = Db::new(&cfg)?;
    let version = db.migrate().await.context("could not prepare the database")?;
    tracing::info!(version, "database schema ready");
    if let Some(report) = import::auto_import(&cfg, &db).await.context("importing the v1 store failed")? {
        log::line(&json!({ "ts": clock::now_iso(), "import_sqlite": report.json }));
    }
    let removed = blobs::clean_partials(&cfg.blob_dir());
    if removed > 0 {
        tracing::info!(removed, "removed partial uploads left by a previous run");
    }
    Ok(Arc::new(Hub { cfg, db, presence: Presence::default(), shutdown: CancellationToken::new(), writing: papaya::HashSet::new(), door: std::sync::OnceLock::new(), push_dns: Arc::new(tokio::sync::Semaphore::new(8)), push_delivery_dns: Arc::new(tokio::sync::Semaphore::new(4)), push_dns_addresses: Arc::new(papaya::HashSet::new()) }))
}

/// The relay-only door's listener, when `HUB_PUBLIC` asks for one. Where it
/// actually listens is kept for /healthz, which names it on the main port.
pub async fn bind_door(hub: &Hub) -> Result<Option<tokio::net::TcpListener>> {
    if !hub.cfg.public {
        return Ok(None);
    }
    let listener = tokio::net::TcpListener::bind((hub.cfg.public_bind.as_str(), hub.cfg.public_port))
        .await
        .with_context(|| format!("could not listen on {}:{}", hub.cfg.public_bind, hub.cfg.public_port))?;
    let _ = hub.door.set(listener.local_addr()?);
    Ok(Some(listener))
}

async fn full(State(hub): State<Arc<Hub>>, req: Request<Body>) -> api::Resp {
    api::dispatch(hub, req).await
}

async fn public(State(hub): State<Arc<Hub>>, req: Request<Body>) -> api::Resp {
    api::dispatch_public(hub, req).await
}

pub fn full_app(hub: Arc<Hub>) -> Router {
    Router::new().fallback(full).with_state(hub)
}

pub fn public_app(hub: Arc<Hub>) -> Router {
    Router::new().fallback(public).with_state(hub)
}

pub async fn serve(cfg: Config) -> Result<()> {
    log::init_stdout();
    log::init_tracing(cfg.verbose);
    // presence and wake-ups live in this process: one hub per database
    let _claim = Db::claim_instance(&cfg).await?;
    let hub = prepare(cfg).await?;
    let full_listener = tokio::net::TcpListener::bind((hub.cfg.bind.as_str(), hub.cfg.port))
        .await
        .with_context(|| format!("could not listen on {}:{}", hub.cfg.bind, hub.cfg.port))?;
    let public_listener = bind_door(&hub).await?;
    log::line(&json!({ "ts": clock::now_iso(), "hub": hub.cfg.hub_name, "retention_days": hub.cfg.retention_days }));
    tracing::info!(bind = %hub.cfg.bind, port = hub.cfg.port, public = hub.cfg.public, "mail hub listening");
    tokio::spawn(sweep::sweep_loop(hub.clone()));
    tokio::spawn(api::sync::presence_loop(hub.clone()));
    let push_task = tokio::spawn(crate::push::run(hub.clone()));
    {
        let hub = hub.clone();
        tokio::spawn(async move {
            shutdown_signal().await;
            tracing::info!("shutting down");
            hub.shutdown.cancel();
            hub.presence.wake_all();
        });
    }
    let stop = hub.shutdown.clone();
    let full = axum::serve(full_listener, full_app(hub.clone())).with_graceful_shutdown(stop.clone().cancelled_owned());
    let result = match public_listener {
        Some(pl) => {
            let public = axum::serve(pl, public_app(hub.clone())).with_graceful_shutdown(stop.cancelled_owned());
            let (a, b) = tokio::join!(full.into_future(), public.into_future());
            a.and(b)
        }
        None => full.await,
    };
    hub.shutdown.cancel();
    let _ = push_task.await;
    result.map_err(Into::into)
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).ok();
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = async { match term.as_mut() { Some(t) => { t.recv().await; } None => std::future::pending().await } } => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

/// `orgtree-mailhub healthcheck`: the container healthcheck (v1 ran a second
/// Python for it). GET /healthz on the local full listener; exit 0 on 200.
pub async fn healthcheck(port: u16) -> bool {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let go = async {
        let mut s = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.ok()?;
        s.write_all(b"GET /healthz HTTP/1.0\r\nHost: 127.0.0.1\r\n\r\n").await.ok()?;
        let mut buf = vec![0u8; 64];
        let n = s.read(&mut buf).await.ok()?;
        let head = String::from_utf8_lossy(&buf[..n]).to_string();
        Some(head.starts_with("HTTP/1.1 200") || head.starts_with("HTTP/1.0 200"))
    };
    matches!(tokio::time::timeout(std::time::Duration::from_secs(3), go).await, Ok(Some(true)))
}
