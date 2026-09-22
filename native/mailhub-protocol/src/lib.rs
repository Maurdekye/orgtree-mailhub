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
//! decoder does not reproduce. Where the crate CAN reproduce them it does,
//! rather than declaring a gap: object entries keep CPython's insertion order
//! (see [`PyValue`]) because the source reads mappings in order, and the
//! integer token `-0` decodes to the `int` 0 exactly as CPython's does.
//!
//! The rest it declares. Where the crate cannot model an input it returns
//! [`RunOutcome::Unrepresentable`] carrying a [`Refusal`] — a STABLE reason
//! name plus the fields that make it specific, not just prose — and the
//! shared profile carries that input as a NAMED unimplemented obligation
//! bound to that same reason. It never guesses an answer, never lets an
//! unmodelled input read as a pass, and never lets one declared gap's refusal
//! stand in for another's:
//!
//! * CPython's `json` accepts the bare tokens `NaN`, `Infinity` and
//!   `-Infinity`, and integers of unbounded width. `serde_json` rejects the
//!   first three and narrows the fourth to `f64`. It also rejects a finite
//!   literal whose exponent overflows, such as `1e999`, where CPython returns
//!   `inf`.
//! * CPython's decoder accepts far deeper nesting than this crate states a
//!   bound for; input past [`MAX_NESTING_DEPTH`] is refused BY NAME rather
//!   than being allowed to look like the source's own silent skip.
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
    decode_line, process_line, py_dumps, python_repr, python_str, python_strip, run, run_scripted,
    tool_names, tools, universal_lines, Call, DecodeError, DispatchOutcome, Dispatcher,
    LineOutcome, PyDict, PyValue, Refusal, RunOutcome, RunReport, ScriptedDispatcher,
    MAX_NESTING_DEPTH, PROTOCOL_VERSION, SERVER_NAME, SERVER_VERSION, SOURCE_COMMIT, TOOLS_JSON,
};
