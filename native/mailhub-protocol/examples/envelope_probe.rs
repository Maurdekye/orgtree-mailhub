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
//! {"op":"decode","id":"<case>","text":"<one line>"}
//! {"op":"exit-after-input","code":42}
//! {"op":"quit"}
//! ```
//!
//! Dispatch outcome kinds are `text`, `url_error` (field `reason`) and
//! `exception` (field `message`), replayed in call order.
//!
//! `decode` exists for the exact-integer work and reports one decoded line
//! WITHOUT running the envelope at all: the CPython type name, `str()`,
//! `repr()` and `json.dumps()` of the value. It is how the profile observes a
//! value's TYPE and its exact output LEXEME rather than only the round-tripped
//! number a tolerant comparator would accept — an implementation that answered
//! a wide integer as a float, or as a quoted digit string, matches on value
//! and fails here. It decodes text handed to it on this same pipe and reaches
//! nothing else.
//!
//! `exit-after-input` exists for ONE test: the runner's own gate on how this
//! process ends. It acknowledges the job, keeps answering normally, and then
//! returns the requested status from `main` once stdin closes — so the suite
//! can prove, against a real process rather than a hand-written receipt, that
//! a driver which emits perfectly good observations and THEN fails cannot be
//! collected as a success. It is reachable only from a job on the driver's own
//! stdin and changes no envelope behaviour. It calls no function in
//! `std::process`: the only name it takes from there is the `ExitCode` TYPE
//! that `main` returns, which is the ordinary way a Rust program reports its
//! own status — it starts nothing and terminates nothing.

use std::io::{self, BufRead, Write};
use std::process::ExitCode;

use mailhub_protocol::envelope::{self, DecodeError, DispatchOutcome, PyDict, PyValue, RunOutcome};

fn string_field(job: &PyDict, field: &str) -> Option<String> {
    match job.get(field) {
        Some(PyValue::Str(s)) => Some(s.clone()),
        _ => None,
    }
}

fn outcome_from(value: &PyValue) -> Result<DispatchOutcome, String> {
    let PyValue::Dict(fields) = value else {
        return Err("a dispatch outcome must be a JSON object".to_string());
    };
    let kind = string_field(fields, "kind")
        .ok_or_else(|| "a dispatch outcome needs a string `kind`".to_string())?;
    match kind.as_str() {
        "text" => Ok(DispatchOutcome::Text(
            string_field(fields, "text")
                .ok_or_else(|| "`text` outcome needs a string `text`".to_string())?,
        )),
        "url_error" => Ok(DispatchOutcome::UrlError {
            reason: string_field(fields, "reason")
                .ok_or_else(|| "`url_error` outcome needs a string `reason`".to_string())?,
        }),
        "exception" => Ok(DispatchOutcome::Exception {
            message: string_field(fields, "message")
                .ok_or_else(|| "`exception` outcome needs a string `message`".to_string())?,
        }),
        other => Err(format!("unknown dispatch outcome kind {other:?}")),
    }
}

fn text_value(value: &str) -> PyValue {
    PyValue::Str(value.to_string())
}

fn count(value: usize) -> PyValue {
    PyValue::int(value)
}

fn identify() -> PyDict {
    let mut server = PyDict::new();
    server.insert("name".to_string(), text_value(envelope::SERVER_NAME));
    server.insert("version".to_string(), text_value(envelope::SERVER_VERSION));
    let mut out = PyDict::new();
    out.insert("op".to_string(), text_value("identify"));
    out.insert(
        "implementation".to_string(),
        text_value("rust/mailhub-protocol"),
    );
    out.insert(
        "source_commit".to_string(),
        text_value(envelope::SOURCE_COMMIT),
    );
    out.insert(
        "protocol_version".to_string(),
        text_value(envelope::PROTOCOL_VERSION),
    );
    out.insert("server_info".to_string(), PyValue::Dict(server));
    out.insert(
        "max_nesting_depth".to_string(),
        count(envelope::MAX_NESTING_DEPTH),
    );
    // The decimal-digit bound this build enforces. The profile compares it
    // against the ORACLE interpreter's live `sys.get_int_max_str_digits()` and
    // fails the run when they differ, rather than re-deriving the expectation
    // from whichever interpreter happens to be present.
    out.insert(
        "max_int_str_digits".to_string(),
        count(envelope::MAX_INT_STR_DIGITS),
    );
    // The embedded card text verbatim, so the caller can hash it against the
    // cards it re-derived from the pinned AST instead of trusting this crate.
    out.insert("tools_json".to_string(), text_value(envelope::TOOLS_JSON));
    out
}

/// Report one decoded line: its CPython type and its three exact renderings.
fn decode_job(job: &PyDict) -> Result<PyDict, String> {
    let text = string_field(job, "text")
        .ok_or_else(|| "a decode job needs a string `text`".to_string())?;
    let mut out = PyDict::new();
    out.insert("op".to_string(), text_value("decode"));
    if let Some(id) = job.get("id") {
        out.insert("id".to_string(), id.clone());
    }
    match envelope::decode_line(&text) {
        Err(DecodeError::Malformed(message)) => {
            out.insert("status".to_string(), text_value("malformed"));
            out.insert("detail".to_string(), text_value(&message));
        }
        Err(DecodeError::Unrepresentable(refusal)) => {
            out.insert("status".to_string(), text_value("unrepresentable"));
            out.insert("reason".to_string(), text_value(refusal.reason));
            out.insert("fields".to_string(), PyValue::Dict(refusal.fields.clone()));
            out.insert("detail".to_string(), text_value(&refusal.detail));
        }
        Ok(value) => {
            out.insert("status".to_string(), text_value("ok"));
            out.insert("type".to_string(), text_value(envelope::py_type_of(&value)));
            // The value itself, re-encoded. A wide integer travels as a bare
            // JSON number token, never as a quoted string, so the caller's own
            // decoder sees the same integer this one did.
            out.insert("value".to_string(), value.clone());
            out.insert("dumps".to_string(), text_value(&envelope::py_dumps(&value)));
            if let PyValue::Int(integer) = &value {
                out.insert("int_digits".to_string(), count(integer.digits()));
            }
            match envelope::python_str(&value) {
                Ok(rendered) => {
                    out.insert("str".to_string(), text_value(&rendered));
                }
                Err(refusal) => {
                    out.insert("str_refusal".to_string(), text_value(refusal.reason));
                }
            }
            match envelope::python_repr(&value) {
                Ok(rendered) => {
                    out.insert("repr".to_string(), text_value(&rendered));
                }
                Err(refusal) => {
                    out.insert("repr_refusal".to_string(), text_value(refusal.reason));
                }
            }
        }
    }
    Ok(out)
}

fn run_job(job: &PyDict) -> Result<PyDict, String> {
    let input =
        string_field(job, "input").ok_or_else(|| "a run job needs a string `input`".to_string())?;
    let mut outcomes = Vec::new();
    match job.get("dispatch") {
        None | Some(PyValue::None) => {}
        Some(PyValue::List(list)) => {
            for entry in list {
                outcomes.push(outcome_from(entry)?);
            }
        }
        Some(_) => return Err("`dispatch` must be an array".to_string()),
    }

    let (report, dispatcher) = envelope::run_scripted(&input, outcomes);

    let calls: Vec<PyValue> = report
        .calls
        .iter()
        .map(|call| {
            let mut row = PyDict::new();
            row.insert("tool".to_string(), text_value(&call.tool));
            // Insertion order, not sorted: the order a handler was handed its
            // arguments in is part of what this driver is reporting.
            row.insert(
                "arguments".to_string(),
                PyValue::Dict(call.arguments.clone()),
            );
            PyValue::Dict(row)
        })
        .collect();

    let mut out = PyDict::new();
    out.insert("op".to_string(), text_value("run"));
    if let Some(id) = job.get("id") {
        out.insert("id".to_string(), id.clone());
    }
    out.insert(
        "frames".to_string(),
        PyValue::List(report.frames.iter().map(|f| text_value(f)).collect()),
    );
    out.insert("raw".to_string(), text_value(&report.raw));
    out.insert("calls".to_string(), PyValue::List(calls));
    match &report.outcome {
        RunOutcome::Completed => {
            out.insert("outcome".to_string(), text_value("completed"));
        }
        RunOutcome::Terminated { kind, message } => {
            out.insert("outcome".to_string(), text_value("terminated"));
            let mut terminal = PyDict::new();
            terminal.insert("kind".to_string(), text_value(kind));
            terminal.insert("message".to_string(), text_value(message));
            out.insert("terminal".to_string(), PyValue::Dict(terminal));
        }
        RunOutcome::Unrepresentable {
            line_index,
            refusal,
        } => {
            out.insert("outcome".to_string(), text_value("unrepresentable"));
            let mut row = PyDict::new();
            row.insert("line_index".to_string(), count(*line_index));
            // `reason` and `fields` are the machine-readable half, and they
            // are what an expectation binds to. `detail` is prose and is
            // reported so a human reading a failure can see what happened.
            row.insert("reason".to_string(), text_value(refusal.reason));
            row.insert("fields".to_string(), PyValue::Dict(refusal.fields.clone()));
            row.insert("detail".to_string(), text_value(&refusal.detail));
            out.insert("unrepresentable".to_string(), PyValue::Dict(row));
        }
    }
    for (key, value) in [
        ("lines_total", report.lines_total),
        ("lines_processed", report.lines_processed),
        ("lines_unprocessed", report.lines_unprocessed),
        ("dispatch_unused", dispatcher.unused()),
        ("dispatch_overruns", dispatcher.overruns),
    ] {
        out.insert(key.to_string(), count(value));
    }
    Ok(out)
}

/// Arm the deliberate abnormal exit. See the module note: this is the runner's
/// own negative control and touches nothing the envelope does.
fn exit_after_input(job: &PyDict) -> Result<(PyDict, u8), String> {
    let code = match job.get("code").and_then(|value| match value {
        PyValue::Int(integer) => integer.to_i128(),
        _ => None,
    }) {
        Some(value) if (0..=255).contains(&value) => value as u8,
        _ => return Err("`exit-after-input` needs an integer `code` in 0..=255".to_string()),
    };
    let mut out = PyDict::new();
    out.insert("op".to_string(), text_value("exit-after-input"));
    out.insert("code".to_string(), count(usize::from(code)));
    Ok((out, code))
}

fn emit<W: Write>(out: &mut W, payload: &PyValue) {
    let _ = writeln!(out, "{}", envelope::py_dumps(payload));
    let _ = out.flush();
}

fn main() -> ExitCode {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut out = stdout.lock();
    let mut exit_code: u8 = 0;
    for line in stdin.lock().lines() {
        let line = match line {
            Ok(line) => line,
            Err(error) => {
                let mut row = PyDict::new();
                row.insert("error".to_string(), text_value(&format!("stdin: {error}")));
                emit(&mut out, &PyValue::Dict(row));
                break;
            }
        };
        if line.trim().is_empty() {
            continue;
        }
        let job = match envelope::decode_line(&line) {
            Ok(PyValue::Dict(job)) => job,
            Ok(_) => {
                let mut row = PyDict::new();
                row.insert(
                    "error".to_string(),
                    text_value("a job must be a JSON object"),
                );
                emit(&mut out, &PyValue::Dict(row));
                continue;
            }
            Err(error) => {
                // A JOB that does not decode is a harness fault, not a
                // protocol observation, so both arms read the same way here.
                let why = match &error {
                    DecodeError::Malformed(message) => message.clone(),
                    DecodeError::Unrepresentable(refusal) => {
                        format!("{}: {}", refusal.reason, refusal.detail)
                    }
                };
                let mut row = PyDict::new();
                row.insert(
                    "error".to_string(),
                    text_value(&format!("unreadable job: {why}")),
                );
                emit(&mut out, &PyValue::Dict(row));
                continue;
            }
        };
        let op = string_field(&job, "op").unwrap_or_else(|| "run".to_string());
        if op == "quit" {
            break;
        }
        let result = match op.as_str() {
            "identify" => Ok(identify()),
            "run" => run_job(&job),
            "decode" => decode_job(&job),
            "exit-after-input" => exit_after_input(&job).map(|(row, code)| {
                exit_code = code;
                row
            }),
            other => Err(format!("unknown op {other:?}")),
        };
        let payload = match result {
            Ok(row) => row,
            Err(message) => {
                let mut row = PyDict::new();
                if let Some(id) = job.get("id") {
                    row.insert("id".to_string(), id.clone());
                }
                row.insert("error".to_string(), text_value(&message));
                row
            }
        };
        emit(&mut out, &PyValue::Dict(payload));
    }
    ExitCode::from(exit_code)
}
