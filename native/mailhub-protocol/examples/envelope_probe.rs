//! Test-only pipe driver. **Never a product service.**
//!
//! The shared Python verification runner needs a way to put the same
//! language-neutral cases through the Rust implementation. This example is
//! that way and nothing more: it reads one JSON job per line from the stdin it
//! was handed, writes one JSON result per line to its stdout, and exits when
//! stdin closes or a `quit` job arrives.
//!
//! It opens no socket, no file, no database and no process; it reads no
//! environment variable, takes no argument, registers no identity and arms no
//! listener. Every tool call it reports was answered by the scripted
//! dispatcher carried in the job itself, so no real handler can run.
//!
//! Jobs:
//!
//! ```text
//! {"op":"identify"}
//! {"op":"run","id":"<case>","input":"<text>","dispatch":[{"kind":"text","text":"…"}]}
//! {"op":"quit"}
//! ```
//!
//! Dispatch outcome kinds are `text`, `url_error` (field `reason`) and
//! `exception` (field `message`), replayed in call order.

use std::io::{self, BufRead, Write};

use mailhub_protocol::envelope::{self, DispatchOutcome, RunOutcome};
use serde_json::{Map, Value};

fn outcome_from(value: &Value) -> Result<DispatchOutcome, String> {
    let kind = value
        .get("kind")
        .and_then(Value::as_str)
        .ok_or_else(|| "a dispatch outcome needs a string `kind`".to_string())?;
    match kind {
        "text" => Ok(DispatchOutcome::Text(
            value
                .get("text")
                .and_then(Value::as_str)
                .ok_or_else(|| "`text` outcome needs a string `text`".to_string())?
                .to_string(),
        )),
        "url_error" => Ok(DispatchOutcome::UrlError {
            reason: value
                .get("reason")
                .and_then(Value::as_str)
                .ok_or_else(|| "`url_error` outcome needs a string `reason`".to_string())?
                .to_string(),
        }),
        "exception" => Ok(DispatchOutcome::Exception {
            message: value
                .get("message")
                .and_then(Value::as_str)
                .ok_or_else(|| "`exception` outcome needs a string `message`".to_string())?
                .to_string(),
        }),
        other => Err(format!("unknown dispatch outcome kind {other:?}")),
    }
}

fn identify() -> Map<String, Value> {
    let mut server = Map::new();
    server.insert(
        "name".to_string(),
        Value::String(envelope::SERVER_NAME.to_string()),
    );
    server.insert(
        "version".to_string(),
        Value::String(envelope::SERVER_VERSION.to_string()),
    );
    let mut out = Map::new();
    out.insert("op".to_string(), Value::String("identify".to_string()));
    out.insert(
        "implementation".to_string(),
        Value::String("rust/mailhub-protocol".to_string()),
    );
    out.insert(
        "source_commit".to_string(),
        Value::String(envelope::SOURCE_COMMIT.to_string()),
    );
    out.insert(
        "protocol_version".to_string(),
        Value::String(envelope::PROTOCOL_VERSION.to_string()),
    );
    out.insert("server_info".to_string(), Value::Object(server));
    // The embedded card text verbatim, so the caller can hash it against the
    // cards it re-derived from the pinned AST instead of trusting this crate.
    out.insert(
        "tools_json".to_string(),
        Value::String(envelope::TOOLS_JSON.to_string()),
    );
    out
}

fn run_job(job: &Value) -> Result<Map<String, Value>, String> {
    let input = job
        .get("input")
        .and_then(Value::as_str)
        .ok_or_else(|| "a run job needs a string `input`".to_string())?;
    let mut outcomes = Vec::new();
    if let Some(list) = job.get("dispatch") {
        let list = list
            .as_array()
            .ok_or_else(|| "`dispatch` must be an array".to_string())?;
        for entry in list {
            outcomes.push(outcome_from(entry)?);
        }
    }

    let (report, dispatcher) = envelope::run_scripted(input, outcomes);

    let calls: Vec<Value> = report
        .calls
        .iter()
        .map(|call| {
            let mut row = Map::new();
            row.insert("tool".to_string(), Value::String(call.tool.clone()));
            row.insert(
                "arguments".to_string(),
                Value::Object(call.arguments.clone()),
            );
            Value::Object(row)
        })
        .collect();

    let mut out = Map::new();
    out.insert("op".to_string(), Value::String("run".to_string()));
    if let Some(id) = job.get("id") {
        out.insert("id".to_string(), id.clone());
    }
    out.insert(
        "frames".to_string(),
        Value::Array(
            report
                .frames
                .iter()
                .map(|f| Value::String(f.clone()))
                .collect(),
        ),
    );
    out.insert("raw".to_string(), Value::String(report.raw.clone()));
    out.insert("calls".to_string(), Value::Array(calls));
    match &report.outcome {
        RunOutcome::Completed => {
            out.insert(
                "outcome".to_string(),
                Value::String("completed".to_string()),
            );
        }
        RunOutcome::Terminated { kind, message } => {
            out.insert(
                "outcome".to_string(),
                Value::String("terminated".to_string()),
            );
            let mut terminal = Map::new();
            terminal.insert("kind".to_string(), Value::String(kind.clone()));
            terminal.insert("message".to_string(), Value::String(message.clone()));
            out.insert("terminal".to_string(), Value::Object(terminal));
        }
        RunOutcome::Unrepresentable { line_index, detail } => {
            out.insert(
                "outcome".to_string(),
                Value::String("unrepresentable".to_string()),
            );
            let mut refusal = Map::new();
            refusal.insert(
                "line_index".to_string(),
                Value::Number((*line_index as u64).into()),
            );
            refusal.insert("detail".to_string(), Value::String(detail.clone()));
            out.insert("unrepresentable".to_string(), Value::Object(refusal));
        }
    }
    for (key, value) in [
        ("lines_total", report.lines_total),
        ("lines_processed", report.lines_processed),
        ("lines_unprocessed", report.lines_unprocessed),
        ("dispatch_unused", dispatcher.unused()),
        ("dispatch_overruns", dispatcher.overruns),
    ] {
        out.insert(key.to_string(), Value::Number((value as u64).into()));
    }
    Ok(out)
}

fn main() {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut out = stdout.lock();
    for line in stdin.lock().lines() {
        let line = match line {
            Ok(line) => line,
            Err(error) => {
                let _ = writeln!(out, "{{\"error\":\"stdin: {error}\"}}");
                break;
            }
        };
        if line.trim().is_empty() {
            continue;
        }
        let job: Value = match serde_json::from_str(&line) {
            Ok(job) => job,
            Err(error) => {
                let mut row = Map::new();
                row.insert(
                    "error".to_string(),
                    Value::String(format!("unreadable job: {error}")),
                );
                let _ = writeln!(
                    out,
                    "{}",
                    serde_json::to_string(&Value::Object(row)).expect("serialises")
                );
                let _ = out.flush();
                continue;
            }
        };
        let op = job.get("op").and_then(Value::as_str).unwrap_or("run");
        if op == "quit" {
            break;
        }
        let result = match op {
            "identify" => Ok(identify()),
            "run" => run_job(&job),
            other => Err(format!("unknown op {other:?}")),
        };
        let payload = match result {
            Ok(row) => Value::Object(row),
            Err(message) => {
                let mut row = Map::new();
                if let Some(id) = job.get("id") {
                    row.insert("id".to_string(), id.clone());
                }
                row.insert("error".to_string(), Value::String(message));
                Value::Object(row)
            }
        };
        let _ = writeln!(
            out,
            "{}",
            serde_json::to_string(&payload).expect("serialises")
        );
        let _ = out.flush();
    }
}
