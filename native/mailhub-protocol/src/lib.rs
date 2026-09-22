//! `mailhub-protocol` — MH02 protocol preparation, and nothing else.
//!
//! # What this crate is
//!
//! An isolated library that reproduces ONE surface of the existing Python mail
//! hub client: the MCP stdio request/reply envelope implemented by `serve()`
//! in `hubtool.py`, pinned to product commit
//! `6477321f89d2d2e1b9313e71e940c76c35b892fb`. It exists so the later port has
//! an executable compatibility component for that envelope before any storage
//! engine, transport or runtime authority is chosen.
//!
//! # What this crate is NOT
//!
//! It is not a mail hub, not an MCP server and not a product component. It
//! performs **no** transport, filesystem, database, account, credential,
//! clock, identity, PID, listener, registration, launcher or packaging work,
//! and it has no binary target. Every tool call is answered by an injected
//! [`Dispatcher`]; no real handler is reachable from here, so none of the
//! eight tools can register an identity, open a store or contact a hub. The
//! crate ships one example, `envelope_probe`, which is a test-only driver for
//! the shared verification runner and is never a service.
//!
//! The MCP conversion blocker recorded by the MH01 source census stays open:
//! this crate exercises the envelope with fake handlers, not the eight tools'
//! actual identity, SQL, transport, retry or shutdown behaviour.
//!
//! # Boundaries this crate declares rather than hides
//!
//! Some pinned-source behaviour depends on CPython specifics that a JSON
//! decoder does not reproduce. Where that happens the crate returns
//! [`RunOutcome::Unrepresentable`] with a reason, and the shared profile
//! carries the input as a NAMED unimplemented obligation. It never guesses an
//! answer and never lets an unmodelled input read as a pass:
//!
//! * CPython's `json` accepts the bare tokens `NaN`, `Infinity` and
//!   `-Infinity`, and integers of unbounded width. `serde_json` rejects the
//!   first three and narrows the fourth to `f64`.
//! * CPython dictionary keys may be non-strings; a JSON object's may not.
//! * `repr()` of a non-ASCII string follows CPython's printability table,
//!   which this crate does not carry. Only `repr()` is bounded — `str()` of a
//!   string, which is what the source actually calls on a tool name, handles
//!   the full Unicode range.
//!
//! # Example
//!
//! ```
//! use mailhub_protocol::{run_scripted, DispatchOutcome, RunOutcome};
//!
//! let (report, dispatcher) = run_scripted(
//!     "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/call\",\
//!        \"params\":{\"name\":\"hub_list\",\"arguments\":{}}}\n",
//!     vec![DispatchOutcome::Text("[]".to_string())],
//! );
//! assert_eq!(report.outcome, RunOutcome::Completed);
//! assert_eq!(report.frames.len(), 1);
//! assert_eq!(dispatcher.calls[0].tool, "hub_list");
//! ```

#![forbid(unsafe_code)]

pub mod envelope;

pub use envelope::{
    process_line, python_repr, python_str, python_strip, run, run_scripted, tool_names, tools,
    universal_lines, Call, DispatchOutcome, Dispatcher, LineOutcome, RunOutcome, RunReport,
    ScriptedDispatcher, PROTOCOL_VERSION, SERVER_NAME, SERVER_VERSION, SOURCE_COMMIT, TOOLS_JSON,
};
