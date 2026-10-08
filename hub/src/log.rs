//! Logging.
//!
//! stdout carries v1's structured lines, byte for byte the way v1 printed
//! them with `json.dumps` (operators parse `docker logs`): one per request
//! (`ts`, `path`, `slugs`, `status`, `ms` — slugs, never secrets), one at
//! startup and one per retention sweep that removed something. A writer
//! thread does the printing, so a request never waits on the console.
//!
//! stderr carries `tracing`: warnings and errors always; with
//! `HUB_LOG_VERBOSE=1` also every instrumented function's call (with its
//! arguments) and return, and every SQL statement (target `hub::sql`, never
//! its parameters).

use std::sync::mpsc;
use std::sync::OnceLock;
use std::time::Duration;

use serde_json::{json, Value};

use crate::clock;
use crate::wire::{py_json, py_round};

static STDOUT: OnceLock<mpsc::Sender<String>> = OnceLock::new();

/// Start the stdout writer. Until this runs (in tests) the lines are
/// dropped, as v1's suite silenced them.
pub fn init_stdout() {
    STDOUT.get_or_init(|| {
        let (tx, rx) = mpsc::channel::<String>();
        std::thread::Builder::new()
            .name("hub-stdout".into())
            .spawn(move || {
                use std::io::Write;
                let out = std::io::stdout();
                for line in rx {
                    let mut o = out.lock();
                    let _ = writeln!(o, "{line}");
                    let _ = o.flush();
                }
            })
            .ok();
        tx
    });
}

pub fn line(v: &Value) {
    if let Some(tx) = STDOUT.get() {
        let _ = tx.send(py_json(v));
    }
}

pub fn request_line(path: &str, slugs: &[String], status: u16, elapsed: Duration) {
    if STDOUT.get().is_none() {
        return;
    }
    line(&json!({
        "ts": clock::now_iso(),
        "path": path,
        "slugs": slugs,
        "status": status,
        "ms": py_round(elapsed.as_secs_f64() * 1000.0),
    }));
}

pub fn init_tracing(verbose: bool) {
    use tracing_subscriber::fmt::format::FmtSpan;
    let filter = if verbose { "info,mailhub=debug,hub::sql=debug" } else { "warn,mailhub=info" };
    let env = tracing_subscriber::EnvFilter::try_from_env("HUB_LOG_FILTER").unwrap_or_else(|_| filter.into());
    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(env)
        .with_span_events(if verbose { FmtSpan::NEW } else { FmtSpan::NONE })
        .with_target(true)
        .try_init();
}
