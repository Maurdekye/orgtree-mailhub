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

/// Test support: while armed, every stdout line is also kept in memory, so
/// a test can read what the hub logged (in tests the lines are otherwise
/// dropped, as v1's suite silenced them). Never armed by the server.
static CAPTURE: OnceLock<std::sync::Mutex<Vec<String>>> = OnceLock::new();

/// Arm the line capture and forget any lines captured before.
pub fn arm_capture() {
    let c = CAPTURE.get_or_init(|| std::sync::Mutex::new(Vec::new()));
    if let Ok(mut g) = c.lock() {
        g.clear();
    }
}

/// The lines captured since `arm_capture` (empty while never armed).
pub fn captured_lines() -> Vec<String> {
    CAPTURE.get().and_then(|c| c.lock().ok().map(|g| g.clone())).unwrap_or_default()
}

pub fn line(v: &Value) {
    if let Some(c) = CAPTURE.get() {
        if let Ok(mut g) = c.lock() {
            g.push(py_json(v));
        }
    }
    if let Some(tx) = STDOUT.get() {
        let _ = tx.send(py_json(v));
    }
}

/// Why a request was refused, carried to its log line only: the recipient
/// a refused send named (`to`) and the detail the sender was told. Never a
/// message body, an attachment's contents or a credential.
pub struct Refusal<'a> {
    pub to: Option<&'a str>,
    pub detail: Option<&'a str>,
}

pub fn request_line(path: &str, slugs: &[String], status: u16, elapsed: Duration, refusal: Option<Refusal<'_>>) {
    let mut v = json!({
        "ts": clock::now_iso(),
        "path": path,
        "slugs": slugs,
        "status": status,
        "ms": py_round(elapsed.as_secs_f64() * 1000.0),
    });
    if let Some(r) = refusal {
        if let Some(to) = r.to {
            v["to"] = json!(to);
        }
        if let Some(detail) = r.detail {
            v["detail"] = json!(detail);
        }
    }
    line(&v);
}

pub fn init_tracing(verbose: bool) {
    use tracing_subscriber::fmt::format::FmtSpan;
    let filter = if verbose { "info,mailhub=debug,hub::sql=debug" } else { "warn,mailhub=info" };
    let env = tracing_subscriber::EnvFilter::try_from_env("HUB_LOG_FILTER").unwrap_or_else(|_| filter.into());
    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        // colour only on a terminal: `docker logs` and a host's log file get plain text
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
        .with_env_filter(env)
        .with_span_events(if verbose { FmtSpan::NEW } else { FmtSpan::NONE })
        .with_target(true)
        .try_init();
}
