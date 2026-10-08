//! Test support: a disposable PostgreSQL, a hub on it, and in-process
//! requests (the way v1's suite drove its ASGI app through httpx's
//! ASGITransport: no socket is bound).
//!
//! The database comes from one of:
//! - `HUB_TEST_DATABASE_URL`: a server where this user may CREATE DATABASE;
//!   each run creates (and drops) its own database there;
//! - `HUB_TEST_PG`: a folder holding PostgreSQL's `bin/` and an initdb'ed
//!   `template/data` (superuser `hubtest`, password in
//!   `template/password.txt`). Each run copies the template, starts its own
//!   server on a free loopback port, and stops it afterwards. On Windows the
//!   server also sits in a kill-on-close job, so a killed test run cannot
//!   leave it behind.
//!
//! Neither set is a failure, not a skip: a suite that silently does not run
//! proves nothing.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Body;
use bytes::Bytes;
use http::{HeaderMap, Method, Request, StatusCode};
use http_body_util::BodyExt;
use mailhub::{api, server, Config, Hub};
use serde_json::Value;

pub struct TestPg {
    pub admin_url: String,
    child: Option<Child>,
    run_dir: Option<PathBuf>,
    bin: Option<PathBuf>,
}

fn copy_dir(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for e in std::fs::read_dir(from)? {
        let e = e?;
        let p = e.path();
        let t = to.join(e.file_name());
        if e.file_type()?.is_dir() {
            copy_dir(&p, &t)?;
        } else {
            std::fs::copy(&p, &t)?;
        }
    }
    Ok(())
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").and_then(|l| l.local_addr()).map(|a| a.port()).unwrap_or(54329)
}

#[cfg(windows)]
fn kill_with_us(child: &Child) {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::System::JobObjects::*;
    unsafe {
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if job.is_null() {
            return;
        }
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const std::ffi::c_void,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        );
        AssignProcessToJobObject(job, child.as_raw_handle() as _);
        // the handle is deliberately never closed: it closes when this test
        // process ends, and the job then ends the server with it
    }
}

#[cfg(not(windows))]
fn kill_with_us(_child: &Child) {}

/// Is the test process that made a run folder still running? (Its server
/// lives exactly as long as it does.)
fn alive(pid: u32) -> bool {
    if pid == std::process::id() {
        return true;
    }
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
        use windows_sys::Win32::System::Threading::{GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h.is_null() {
            return false;
        }
        let mut code = 0u32;
        let ok = GetExitCodeProcess(h, &mut code) != 0;
        CloseHandle(h);
        ok && code == STILL_ACTIVE as u32
    }
    #[cfg(unix)]
    unsafe {
        libc::kill(pid as i32, 0) == 0
    }
}

impl TestPg {
    pub fn start() -> TestPg {
        if let Ok(url) = std::env::var("HUB_TEST_DATABASE_URL") {
            return TestPg { admin_url: url, child: None, run_dir: None, bin: None };
        }
        let root = PathBuf::from(std::env::var("HUB_TEST_PG").expect(
            "set HUB_TEST_DATABASE_URL (a PostgreSQL server where you may CREATE DATABASE) or HUB_TEST_PG \
             (a folder with PostgreSQL bin/ and an initdb'ed template/data): the hub suite needs a database",
        ));
        let bin = root.join("bin");
        let password = std::fs::read_to_string(root.join("template").join("password.txt")).expect("template/password.txt");
        let runs = root.join("runs");
        // runs a killed test left behind (their server died with the job);
        // a live process's runs are left alone: tests run side by side
        if let Ok(rd) = std::fs::read_dir(&runs) {
            for e in rd.flatten() {
                let pid = e.file_name().to_str().and_then(|n| n.split('-').next()?.parse::<u32>().ok());
                if pid.is_some_and(alive) {
                    continue;
                }
                let _ = std::fs::remove_dir_all(e.path());
            }
        }
        let run_dir = runs.join(format!("{}-{}", std::process::id(), uuid::Uuid::new_v4().simple()));
        copy_dir(&root.join("template").join("data"), &run_dir.join("data")).expect("copy the template cluster");
        let port = free_port();
        let log = std::fs::File::create(run_dir.join("postgres.log")).expect("postgres.log");
        let exe = if cfg!(windows) { "postgres.exe" } else { "postgres" };
        let child = Command::new(bin.join(exe))
            .arg("-D")
            .arg(run_dir.join("data"))
            .args(["-p", &port.to_string(), "-c", "listen_addresses=127.0.0.1", "-c", "fsync=off"])
            .args(["-c", "synchronous_commit=off", "-c", "full_page_writes=off", "-c", "max_connections=300"])
            .args(["-c", "unix_socket_directories="])
            .stdin(Stdio::null())
            .stdout(log.try_clone().expect("log"))
            .stderr(log)
            .spawn()
            .expect("start the test PostgreSQL server");
        kill_with_us(&child);
        let admin_url = format!("postgres://hubtest:{}@127.0.0.1:{port}/postgres", password.trim());
        TestPg { admin_url, child: Some(child), run_dir: Some(run_dir), bin: Some(bin) }
    }

    /// A fresh, empty database on this server; returns its URL.
    pub async fn fresh_db(&self, name: &str) -> String {
        let deadline = Instant::now() + Duration::from_secs(60);
        let (client, conn) = loop {
            match tokio_postgres::connect(&self.admin_url, tokio_postgres::NoTls).await {
                Ok(c) => break c,
                Err(e) if Instant::now() < deadline => {
                    let _ = e;
                    tokio::time::sleep(Duration::from_millis(250)).await;
                }
                Err(e) => panic!("the test database server never accepted connections: {e}"),
            }
        };
        tokio::spawn(async move {
            let _ = conn.await;
        });
        let db = format!("{name}_{}", &uuid::Uuid::new_v4().simple().to_string()[..12]);
        client.batch_execute(&format!("CREATE DATABASE {db}")).await.expect("create the test database");
        replace_db(&self.admin_url, &db)
    }
}

/// `url` with its database name replaced.
pub fn replace_db(url: &str, db: &str) -> String {
    match url.rfind('/') {
        Some(i) => {
            let (head, tail) = url.split_at(i + 1);
            let query = tail.find('?').map(|q| &tail[q..]).unwrap_or("");
            format!("{head}{db}{query}")
        }
        None => url.to_string(),
    }
}

impl Drop for TestPg {
    fn drop(&mut self) {
        if let (Some(mut child), Some(bin), Some(dir)) = (self.child.take(), self.bin.take(), self.run_dir.take()) {
            let ctl = if cfg!(windows) { "pg_ctl.exe" } else { "pg_ctl" };
            let _ = Command::new(bin.join(ctl))
                .args(["stop", "-m", "immediate", "-D"])
                .arg(dir.join("data"))
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            let _ = child.kill();
            let _ = child.wait();
            for _ in 0..20 {
                if std::fs::remove_dir_all(&dir).is_ok() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(250));
            }
        }
    }
}

/// A hub on `url` with its data in `data`, configured like v1's suite
/// (HUB_NAME=test-hub, 30 days) plus `extra` variables.
pub async fn hub(url: &str, data: &Path, extra: &[(&str, &str)]) -> Arc<Hub> {
    let mut vars: std::collections::HashMap<String, String> = [
        ("HUB_DATABASE_URL", url),
        ("HUB_DATA", data.to_str().unwrap()),
        ("HUB_NAME", "test-hub"),
        ("HUB_RETENTION_DAYS", "30"),
        ("HUB_DB_POOL", "16"),
    ]
    .iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect();
    for (k, v) in extra {
        vars.insert(k.to_string(), v.to_string());
    }
    let cfg = Config::from_vars(&vars).expect("test config");
    server::prepare(cfg).await.expect("prepare the hub")
}

/// A direct connection for assertions, with the hub's search path.
pub async fn sql_client(url: &str) -> tokio_postgres::Client {
    let pg = mailhub::db::Db::pg_config(url).unwrap();
    let (client, conn) = pg.connect(tokio_postgres::NoTls).await.expect("connect for assertions");
    tokio::spawn(async move {
        let _ = conn.await;
    });
    client
}

pub struct Resp {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Bytes,
}

impl Resp {
    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    pub fn code(&self) -> u16 {
        self.status.as_u16()
    }
}

pub struct Call<'a> {
    pub method: Method,
    pub path: &'a str,
    pub auth: Option<String>,
    pub json: Option<Value>,
    pub content: Option<Body>,
    pub headers: Vec<(&'static str, String)>,
    pub public: bool,
}

impl<'a> Call<'a> {
    pub fn new(method: &str, path: &'a str) -> Call<'a> {
        Call {
            method: method.parse().unwrap(),
            path,
            auth: None,
            json: None,
            content: None,
            headers: Vec::new(),
            public: false,
        }
    }
    pub fn auth(mut self, a: impl Into<String>) -> Self {
        let a = a.into();
        if !a.is_empty() {
            self.auth = Some(a);
        }
        self
    }
    pub fn json(mut self, v: Value) -> Self {
        self.json = Some(v);
        self
    }
    pub fn content(mut self, b: impl Into<Body>) -> Self {
        self.content = Some(b.into());
        self
    }
    pub fn header(mut self, k: &'static str, v: impl Into<String>) -> Self {
        self.headers.push((k, v.into()));
        self
    }
    pub fn public(mut self) -> Self {
        self.public = true;
        self
    }

    pub async fn send(self, hub: &Arc<Hub>) -> Resp {
        let uri = http::Uri::builder().path_and_query(self.path).build().expect("test uri");
        let mut b = Request::builder().method(self.method).uri(uri).header("host", "hub");
        if let Some(a) = &self.auth {
            b = b.header("x-org-auth", a.as_str());
        }
        for (k, v) in &self.headers {
            b = b.header(*k, v.as_str());
        }
        let body = match (self.json, self.content) {
            (Some(j), _) => {
                b = b.header("content-type", "application/json");
                Body::from(serde_json::to_vec(&j).unwrap())
            }
            (None, Some(c)) => c,
            (None, None) => Body::empty(),
        };
        let req = b.body(body).unwrap();
        let resp = if self.public { api::dispatch_public(hub.clone(), req).await } else { api::dispatch(hub.clone(), req).await };
        let (parts, body) = resp.into_parts();
        let body = body.collect().await.map(|c| c.to_bytes()).unwrap_or_default();
        Resp { status: parts.status, headers: parts.headers, body }
    }
}

pub fn pair(slug: &str, secret: &str) -> String {
    format!("{slug}:{secret}")
}

/// 32 random hex characters (v1 used secrets.token_hex(16)).
pub fn token_hex16() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

/// 6 random hex characters (secrets.token_hex(3)).
pub fn token_hex3() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..6].to_string()
}

pub fn sha256_hex(s: &str) -> String {
    mailhub::auth::fingerprint(s)
}
