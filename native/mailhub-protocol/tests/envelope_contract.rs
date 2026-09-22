//! Direct library behaviour and framing checks.
//!
//! These are the crate's own assertions. They are NOT the compatibility
//! contract: that lives in `tests/fixtures/mh02-mcp-envelope.json` and is run
//! against BOTH implementations by `tests/mh02_mcp_contract.py`. What this
//! file adds is the checks that are about the Rust API itself — framing,
//! helper functions, the refusal path — which the neutral profile
//! deliberately cannot express.

use mailhub_protocol::{
    process_line, python_repr, python_str, python_strip, run, run_scripted, tool_names, tools,
    universal_lines, DispatchOutcome, LineOutcome, RunOutcome, ScriptedDispatcher,
    PROTOCOL_VERSION, SERVER_NAME, SERVER_VERSION,
};
use serde_json::{json, Value};

fn only_frame(input: &str, outcomes: Vec<DispatchOutcome>) -> Value {
    let (report, _) = run_scripted(input, outcomes);
    assert_eq!(report.outcome, RunOutcome::Completed, "input {input:?}");
    assert_eq!(report.frames.len(), 1, "input {input:?}");
    serde_json::from_str(&report.frames[0]).expect("a frame is JSON")
}

fn silent(input: &str) {
    let (report, _) = run_scripted(input, vec![]);
    assert_eq!(report.outcome, RunOutcome::Completed, "input {input:?}");
    assert!(report.frames.is_empty(), "input {input:?} replied");
    assert_eq!(report.raw, "", "input {input:?} wrote something");
}

// ── initialize ─────────────────────────────────────────────────────────────

#[test]
fn initialize_metadata_is_exact() {
    let frame = only_frame(r#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#, vec![]);
    assert_eq!(
        frame,
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {"tools": {}},
                "serverInfo": {"name": SERVER_NAME, "version": SERVER_VERSION}
            }
        })
    );
}

#[test]
fn a_recognised_method_answers_even_with_no_id() {
    // The distinction a generic MCP implementation loses: this is a
    // notification by JSON-RPC's reading, and the source answers it anyway.
    let frame = only_frame(r#"{"jsonrpc":"2.0","method":"initialize"}"#, vec![]);
    assert_eq!(frame["id"], Value::Null);
    assert_eq!(frame["result"]["protocolVersion"], PROTOCOL_VERSION);
}

// ── tools/list ─────────────────────────────────────────────────────────────

#[test]
fn every_tool_card_is_listed_in_source_order() {
    let frame = only_frame(r#"{"id":"a","method":"tools/list"}"#, vec![]);
    let listed = frame["result"]["tools"].as_array().expect("an array");
    assert_eq!(listed.len(), 8);
    assert_eq!(
        tool_names(),
        vec![
            "hub_register",
            "hub_list",
            "hub_send",
            "hub_fetch",
            "hub_unregister",
            "hub_read",
            "hub_wait",
            "hub_hubs",
        ]
    );
    assert_eq!(&frame["result"]["tools"], tools());
    for card in listed {
        assert!(card.get("name").is_some());
        assert!(card.get("description").is_some());
        assert_eq!(card["inputSchema"]["type"], "object");
    }
}

// ── tools/call ─────────────────────────────────────────────────────────────

#[test]
fn a_tool_call_forwards_its_name_and_arguments() {
    let mut dispatcher = ScriptedDispatcher::new(vec![DispatchOutcome::Text("{}".to_string())]);
    let report = run(
        "{\"id\":7,\"method\":\"tools/call\",\"params\":{\"name\":\"hub_send\",\
           \"arguments\":{\"to\":\"peer\",\"body\":\"hi\"}}}\n",
        &mut dispatcher,
    );
    assert_eq!(report.outcome, RunOutcome::Completed);
    assert_eq!(dispatcher.calls.len(), 1);
    assert_eq!(dispatcher.calls[0].tool, "hub_send");
    assert_eq!(dispatcher.calls[0].arguments["to"], "peer");
    assert_eq!(dispatcher.calls[0].arguments["body"], "hi");
    assert_eq!(dispatcher.overruns, 0);
}

#[test]
fn a_handler_failure_stays_inside_a_successful_result() {
    // The recorded conversion blocker, asserted: there is no `error` member.
    for outcome in [
        DispatchOutcome::UrlError {
            reason: "timed out".to_string(),
        },
        DispatchOutcome::Exception {
            message: "not registered".to_string(),
        },
    ] {
        let frame = only_frame(
            r#"{"id":1,"method":"tools/call","params":{"name":"hub_list"}}"#,
            vec![outcome],
        );
        assert!(frame.get("error").is_none(), "{frame}");
        assert_eq!(frame["result"]["content"][0]["type"], "text");
    }
}

#[test]
fn the_unreachable_hub_text_is_the_sources_own_sentence() {
    let frame = only_frame(
        r#"{"id":1,"method":"tools/call","params":{"name":"hub_list"}}"#,
        vec![DispatchOutcome::UrlError {
            reason: "[Errno 61] Connection refused".to_string(),
        }],
    );
    assert_eq!(
        frame["result"]["content"][0]["text"],
        r#"{"error": "hub unreachable: [Errno 61] Connection refused"}"#
    );
}

#[test]
fn a_missing_name_is_forwarded_as_the_string_none() {
    // `str(p.get("name"))` on a missing key is `str(None)`, which is "None" —
    // not an empty string and not a refusal.
    let mut dispatcher = ScriptedDispatcher::new(vec![DispatchOutcome::Text("x".to_string())]);
    run(
        r#"{"id":1,"method":"tools/call","params":{}}"#,
        &mut dispatcher,
    );
    assert_eq!(dispatcher.calls[0].tool, "None");
    assert!(dispatcher.calls[0].arguments.is_empty());
}

#[test]
fn malformed_params_fail_inside_the_handler_boundary() {
    // Each of these raises where the source has a `try`, so each is an error
    // STRING in a successful envelope rather than a dropped frame.
    for (params, text) in [
        (
            "\"oops\"",
            r#"{"error": "'str' object has no attribute 'get'"}"#,
        ),
        (
            r#"{"name":"hub_list","arguments":"ab"}"#,
            r#"{"error": "dictionary update sequence element #0 has length 1; 2 is required"}"#,
        ),
        (
            r#"{"name":"hub_list","arguments":[1,2]}"#,
            r#"{"error": "cannot convert dictionary update sequence element #0 to a sequence"}"#,
        ),
        (
            r#"{"name":"hub_list","arguments":5}"#,
            r#"{"error": "'int' object is not iterable"}"#,
        ),
    ] {
        let line = format!(r#"{{"id":1,"method":"tools/call","params":{params}}}"#);
        let frame = only_frame(&line, vec![]);
        assert_eq!(frame["result"]["content"][0]["text"], text, "{line}");
    }
}

// ── unknown methods and ids ────────────────────────────────────────────────

#[test]
fn an_unknown_method_with_an_id_gets_an_empty_result() {
    for id in ["0", "false", "\"\"", "\"x\"", "-1"] {
        let line = format!(r#"{{"id":{id},"method":"resources/list"}}"#);
        let frame = only_frame(&line, vec![]);
        assert_eq!(frame["result"], json!({}), "{line}");
        assert!(frame.get("error").is_none(), "{line}");
    }
}

#[test]
fn an_unknown_method_without_a_usable_id_is_silent() {
    silent(r#"{"method":"resources/list"}"#);
    silent(r#"{"id":null,"method":"resources/list"}"#);
    silent(r#"{}"#);
    silent(r#"{"method":5,"id":null}"#);
}

// ── framing ────────────────────────────────────────────────────────────────

#[test]
fn blank_and_unparseable_lines_are_skipped_without_a_reply() {
    silent("\n\n   \n");
    silent("not json\n");
    silent("{\n");
    silent("{\"method\":\"initialize\",}\n");
}

#[test]
fn a_final_line_with_no_newline_is_still_processed() {
    let (report, _) = run_scripted(r#"{"id":1,"method":"initialize"}"#, vec![]);
    assert_eq!(report.lines_total, 1);
    assert_eq!(report.frames.len(), 1);
    assert!(report.raw.ends_with('\n'), "every frame is newline framed");
}

#[test]
fn every_frame_is_exactly_one_line_and_raw_is_their_concatenation() {
    let (report, _) = run_scripted(
        "{\"id\":1,\"method\":\"initialize\"}\n{\"id\":2,\"method\":\"tools/list\"}\n",
        vec![],
    );
    assert_eq!(report.frames.len(), 2);
    for frame in &report.frames {
        assert!(!frame.contains('\n'), "a frame must not contain a newline");
    }
    let expected: String = report
        .frames
        .iter()
        .map(|f| format!("{f}\n"))
        .collect::<Vec<_>>()
        .join("");
    assert_eq!(report.raw, expected);
}

#[test]
fn replies_keep_their_request_order() {
    let (report, _) = run_scripted(
        "{\"id\":\"a\",\"method\":\"initialize\"}\n\n{\"id\":\"b\",\"method\":\"tools/list\"}\n\
         bad json\n{\"id\":\"c\",\"method\":\"nope\"}\n",
        vec![],
    );
    let ids: Vec<Value> = report
        .frames
        .iter()
        .map(|f| serde_json::from_str::<Value>(f).expect("JSON")["id"].clone())
        .collect();
    assert_eq!(ids, vec![json!("a"), json!("b"), json!("c")]);
    assert_eq!(report.lines_total, 5);
    assert_eq!(report.lines_unprocessed, 0);
}

// ── the terminal outcome ───────────────────────────────────────────────────

#[test]
fn parsed_non_object_json_ends_the_loop_before_a_reply() {
    for (line, type_name) in [
        ("5", "int"),
        ("1.5", "float"),
        ("\"text\"", "str"),
        ("[1,2]", "list"),
        ("true", "bool"),
        ("null", "NoneType"),
    ] {
        let input = format!("{line}\n{{\"id\":9,\"method\":\"initialize\"}}\n");
        let (report, _) = run_scripted(&input, vec![]);
        assert_eq!(
            report.outcome,
            RunOutcome::Terminated {
                kind: "AttributeError".to_string(),
                message: format!("'{type_name}' object has no attribute 'get'"),
            },
            "{input:?}"
        );
        assert!(report.frames.is_empty(), "{input:?} replied before dying");
        assert_eq!(report.lines_total, 2);
        assert_eq!(
            report.lines_unprocessed, 1,
            "the frame after a terminal outcome must not be processed"
        );
    }
}

#[test]
fn work_done_before_the_terminal_line_is_kept() {
    let (report, _) = run_scripted(
        "{\"id\":1,\"method\":\"initialize\"}\n\"boom\"\n{\"id\":2,\"method\":\"tools/list\"}\n",
        vec![],
    );
    assert_eq!(report.frames.len(), 1);
    assert_eq!(report.lines_unprocessed, 1);
    assert!(matches!(report.outcome, RunOutcome::Terminated { .. }));
}

// ── helpers ────────────────────────────────────────────────────────────────

#[test]
fn python_strip_follows_cpythons_whitespace_set_not_rusts() {
    // U+001C..U+001F are whitespace to `str.strip()` and are NOT in Rust's
    // `char::is_whitespace`, so `str::trim` would leave them in place and the
    // JSON parse would fail where the source succeeds.
    for pad in ["\u{1c}", "\u{1d}", "\u{1e}", "\u{1f}"] {
        assert_eq!(python_strip(&format!("{pad}x{pad}")), "x", "{pad:?}");
        assert_ne!(format!("{pad}x{pad}").trim(), "x", "{pad:?}");
    }
    for pad in [" ", "\t", "\r", "\u{a0}", "\u{2028}", "\u{3000}", "\u{85}"] {
        assert_eq!(python_strip(&format!("{pad}x{pad}")), "x", "{pad:?}");
    }
    // Not whitespace to Python: a zero-width space must survive.
    assert_eq!(python_strip("\u{200b}x"), "\u{200b}x");
}

#[test]
fn a_separator_padded_frame_still_parses() {
    let frame = only_frame("\u{1c}{\"id\":1,\"method\":\"initialize\"}\u{1c}\n", vec![]);
    assert_eq!(frame["id"], 1);
}

#[test]
fn universal_newlines_split_the_way_stdin_does() {
    assert_eq!(universal_lines("a\rb\nc"), vec!["a", "b", "c"]);
    assert_eq!(universal_lines("a\r\nb\r\n"), vec!["a", "b"]);
    assert_eq!(universal_lines(""), Vec::<&str>::new());
    assert_eq!(universal_lines("\n"), vec![""]);
    assert_eq!(universal_lines("a"), vec!["a"]);
}

#[test]
fn python_str_matches_cpython_for_the_values_json_can_carry() {
    for (value, expected) in [
        (json!(null), "None"),
        (json!(true), "True"),
        (json!(false), "False"),
        (json!(0), "0"),
        (json!(-1), "-1"),
        (json!(9007199254740993i64), "9007199254740993"),
        (json!(1.5), "1.5"),
        (json!(100.0), "100.0"),
        (json!(1e16), "1e+16"),
        (json!(1e15), "1000000000000000.0"),
        (json!(0.0001), "0.0001"),
        (json!(1e-5), "1e-05"),
        (json!(1e308), "1e+308"),
        (json!(-0.0), "-0.0"),
        (json!("hub_list"), "hub_list"),
        (json!("caf\u{e9}"), "caf\u{e9}"),
        (json!([1, "a", null]), "[1, 'a', None]"),
        (json!({}), "{}"),
        (json!({"a": 1}), "{'a': 1}"),
    ] {
        assert_eq!(python_str(&value).as_deref(), Ok(expected), "{value}");
    }
}

#[test]
fn repr_of_a_non_ascii_string_is_refused_rather_than_guessed() {
    // Inside a container the source would call repr(), whose escaping depends
    // on CPython's printability table. Refusing is the declared obligation.
    assert!(python_repr(&json!(["caf\u{e9}"])).is_err());
    // But str() of that same string, which is what a tool name actually goes
    // through, is fully supported.
    assert_eq!(python_str(&json!("caf\u{e9}")).unwrap(), "caf\u{e9}");
}

#[test]
fn an_unmodellable_input_is_refused_and_never_answered() {
    // A non-string dictionary key is legal Python and cannot be a JSON object
    // key. The crate says so instead of coercing it into something plausible.
    let mut dispatcher = ScriptedDispatcher::new(vec![]);
    let report = run(
        r#"{"id":1,"method":"tools/call","params":{"name":"hub_list","arguments":[[1,2]]}}"#,
        &mut dispatcher,
    );
    match report.outcome {
        RunOutcome::Unrepresentable { line_index, detail } => {
            assert_eq!(line_index, 0);
            assert!(detail.contains("int"), "{detail}");
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
    assert!(report.frames.is_empty());
    assert!(
        dispatcher.calls.is_empty(),
        "no handler may run on a refusal"
    );
}

#[test]
fn process_line_reports_each_outcome_kind_on_its_own() {
    let mut dispatcher = ScriptedDispatcher::new(vec![]);
    assert_eq!(
        process_line("  ", &mut dispatcher),
        Ok(LineOutcome::Skipped)
    );
    assert_eq!(
        process_line("nonsense", &mut dispatcher),
        Ok(LineOutcome::Skipped)
    );
    assert_eq!(
        process_line(r#"{"method":"nope"}"#, &mut dispatcher),
        Ok(LineOutcome::Silent)
    );
    assert!(matches!(
        process_line(r#"{"id":1,"method":"initialize"}"#, &mut dispatcher),
        Ok(LineOutcome::Reply(_))
    ));
    assert!(matches!(
        process_line("[]", &mut dispatcher),
        Ok(LineOutcome::Terminal { .. })
    ));
    assert!(dispatcher.calls.is_empty());
}

// ── isolation ──────────────────────────────────────────────────────────────

#[test]
fn no_tool_name_can_reach_a_real_handler() {
    // All eight names, each answered only by the injected dispatcher. Nothing
    // registers, opens a store or contacts a hub, because there is no code
    // path from this crate to one.
    for name in tool_names() {
        let mut dispatcher =
            ScriptedDispatcher::new(vec![DispatchOutcome::Text(format!("ok:{name}"))]);
        let line = format!(
            r#"{{"id":1,"method":"tools/call","params":{{"name":"{name}","arguments":{{}}}}}}"#
        );
        let report = run(&line, &mut dispatcher);
        assert_eq!(report.outcome, RunOutcome::Completed);
        assert_eq!(dispatcher.calls.len(), 1);
        assert_eq!(dispatcher.calls[0].tool, name);
        assert_eq!(dispatcher.overruns, 0);
        let frame: Value = serde_json::from_str(&report.frames[0]).expect("JSON");
        assert_eq!(frame["result"]["content"][0]["text"], format!("ok:{name}"));
    }
}
