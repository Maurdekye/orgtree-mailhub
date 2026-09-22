//! Direct library behaviour and framing checks.
//!
//! These are the crate's own assertions. They are NOT the compatibility
//! contract: that lives in `tests/fixtures/mh02-mcp-envelope.json` and is run
//! against BOTH implementations by `tests/mh02_mcp_contract.py`. What this
//! file adds is the checks that are about the Rust API itself — framing,
//! helper functions, the refusal path — which the neutral profile
//! deliberately cannot express.

use mailhub_protocol::{
    decode_line, process_line, py_dumps, py_type_of, python_repr, python_str, python_strip, run,
    run_scripted, tool_names, tools, universal_lines, DecodeError, DispatchOutcome, LineOutcome,
    PyValue, Refusal, RunOutcome, ScriptedDispatcher, MAX_INT_STR_DIGITS, MAX_NESTING_DEPTH,
    PROTOCOL_VERSION, SERVER_NAME, SERVER_VERSION,
};
use serde_json::{json, Value};

/// Decode a JSON document from its TEXT, which is how every real input
/// reaches the crate. Building a value directly would skip the decoder, and
/// the decoder is where CPython's object model is reproduced.
fn py(text: &str) -> PyValue {
    decode_line(text).expect("the test document decodes")
}

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
    // The embedded cards, compared through the wire text so the check does
    // not depend on either side's in-memory value type.
    assert_eq!(
        serde_json::to_string(&frame["result"]["tools"]).expect("serialises"),
        serde_json::to_string(
            &serde_json::from_str::<Value>(&py_dumps(tools())).expect("cards are JSON")
        )
        .expect("serialises")
    );
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
    let arguments = &dispatcher.calls[0].arguments;
    assert_eq!(arguments.get("to"), Some(&PyValue::Str("peer".to_string())));
    assert_eq!(arguments.get("body"), Some(&PyValue::Str("hi".to_string())));
    // ...and in the order they arrived, which is the order CPython's dict
    // would have carried them to the handler.
    assert_eq!(
        arguments.keys().cloned().collect::<Vec<_>>(),
        vec!["to".to_string(), "body".to_string()]
    );
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
    for (text, expected) in [
        ("null", "None"),
        ("true", "True"),
        ("false", "False"),
        ("0", "0"),
        ("-1", "-1"),
        ("9007199254740993", "9007199254740993"),
        ("1.5", "1.5"),
        ("100.0", "100.0"),
        ("1e16", "1e+16"),
        ("1e15", "1000000000000000.0"),
        ("0.0001", "0.0001"),
        ("1e-5", "1e-05"),
        ("1e308", "1e+308"),
        // Two spellings CPython keeps apart, and so must this crate: `-0` is
        // the INT zero and `-0.0` is the negative float. serde_json decodes
        // both as -0.0 on its own, which would change a dispatched name.
        ("-0", "0"),
        ("-0.0", "-0.0"),
        ("-0e0", "-0.0"),
        ("\"hub_list\"", "hub_list"),
        ("\"caf\\u00e9\"", "caf\u{e9}"),
        ("[1, \"a\", null]", "[1, 'a', None]"),
        ("{}", "{}"),
        ("{\"a\": 1}", "{'a': 1}"),
        // Insertion order, not sorted order.
        ("{\"z\": 1, \"a\": 2}", "{'z': 1, 'a': 2}"),
        ("[{\"z\": 1, \"a\": 2}]", "[{'z': 1, 'a': 2}]"),
        // CPython's duplicate-key rule: the first occurrence keeps its
        // position and the last assignment wins.
        ("{\"b\": 1, \"a\": 2, \"b\": 3}", "{'b': 3, 'a': 2}"),
    ] {
        assert_eq!(python_str(&py(text)).as_deref(), Ok(expected), "{text}");
    }
}

#[test]
fn a_decoded_mapping_keeps_cpython_insertion_order_everywhere_it_is_read() {
    // The three places the source reads a mapping IN ORDER, each of which a
    // sorted map would silently change.

    // 1. `str(p.get("name"))` when the name is a mapping: the dispatched tool
    //    string is the repr, and the repr is ordered.
    let mut dispatcher = ScriptedDispatcher::new(vec![DispatchOutcome::Text("ok".to_string())]);
    run(
        "{\"id\":1,\"method\":\"tools/call\",\"params\":{\"name\":{\"z\":1,\"a\":2}}}",
        &mut dispatcher,
    );
    assert_eq!(dispatcher.calls[0].tool, "{'z': 1, 'a': 2}");

    // 2. `dict(p.get("arguments"))` over a list of mappings: a dict iterates
    //    its KEYS, so the FIRST key becomes the key and the SECOND the value.
    let mut dispatcher = ScriptedDispatcher::new(vec![DispatchOutcome::Text("ok".to_string())]);
    run(
        "{\"id\":1,\"method\":\"tools/call\",\"params\":{\"name\":\"hub_send\",\
         \"arguments\":[{\"z\":1,\"a\":2}]}}",
        &mut dispatcher,
    );
    let arguments = &dispatcher.calls[0].arguments;
    assert_eq!(arguments.len(), 1);
    assert_eq!(
        arguments.get("z"),
        Some(&PyValue::Str("a".to_string())),
        "dict([{{'z': 1, 'a': 2}}]) is {{'z': 'a'}}, not {{'a': 'z'}}"
    );

    // 3. An echoed `id` is written back out in the order it arrived.
    let (report, _) = run_scripted("{\"id\":{\"z\":1,\"a\":2},\"method\":\"nope\"}", vec![]);
    assert_eq!(
        report.frames[0],
        "{\"jsonrpc\": \"2.0\", \"id\": {\"z\": 1, \"a\": 2}, \"result\": {}}"
    );
}

#[test]
fn a_reply_frame_is_byte_identical_to_cpython_json_dumps() {
    // `json.dumps` defaults: ", " and ": " separators, ensure_ascii, and the
    // mapping's own order. Emitting through serde_json instead would sort the
    // keys and leave non-ASCII unescaped.
    let (report, _) = run_scripted("{\"id\":\"caf\\u00e9\",\"method\":\"nope\"}", vec![]);
    assert_eq!(
        report.frames[0],
        "{\"jsonrpc\": \"2.0\", \"id\": \"caf\\u00e9\", \"result\": {}}"
    );
    assert_eq!(
        py_dumps(&py("{\"z\": [1, 2.5, null, true], \"a\": {}}")),
        "{\"z\": [1, 2.5, null, true], \"a\": {}}"
    );
}

#[test]
fn nesting_past_the_stated_bound_is_refused_by_name_and_never_skipped() {
    // serde_json's own bound is 128, which ordinary input can cross while
    // CPython's decoder does not. Anything at or under the crate's stated
    // bound decodes; anything past it is an explicit refusal, NOT the silent
    // skip that `except ValueError: continue` would otherwise disguise it as.
    let deep = |n: usize| format!("{}{}", "[".repeat(n), "]".repeat(n));
    assert!(
        decode_line(&deep(140)).is_ok(),
        "140 is well within CPython"
    );
    assert!(decode_line(&deep(MAX_NESTING_DEPTH)).is_ok());
    match decode_line(&deep(MAX_NESTING_DEPTH + 1)) {
        Err(DecodeError::Unrepresentable(refusal)) => {
            assert_eq!(refusal.reason, Refusal::NESTING_DEPTH_EXCEEDED);
            assert_eq!(
                refusal.fields.get("limit"),
                Some(&PyValue::int(MAX_NESTING_DEPTH)),
                "the bound that was exceeded is part of the refusal, not just its prose"
            );
        }
        other => panic!("expected a named depth refusal, got {other:?}"),
    }
    assert!(matches!(
        decode_line("{\"a\":}"),
        Err(DecodeError::Malformed(_))
    ));

    // And the difference is visible at the envelope: a 140-deep argument is
    // answered like any other, while one past the bound stops the run with a
    // named refusal instead of pretending the source stayed silent.
    let call = |argument: &str| {
        format!(
            "{{\"id\":1,\"method\":\"tools/call\",\"params\":{{\"name\":\"hub_send\",\
             \"arguments\":[[\"k\",{argument}]]}}}}"
        )
    };
    let (report, _) = run_scripted(&call(&deep(140)), vec![DispatchOutcome::Text("ok".into())]);
    assert_eq!(report.outcome, RunOutcome::Completed);
    assert_eq!(report.frames.len(), 1);

    let (report, _) = run_scripted(
        &call(&deep(MAX_NESTING_DEPTH + 1)),
        vec![DispatchOutcome::Text("ok".into())],
    );
    assert!(
        matches!(report.outcome, RunOutcome::Unrepresentable { .. }),
        "observed {:?}",
        report.outcome
    );
    assert!(report.frames.is_empty());
}

#[test]
fn a_minus_before_a_zero_is_only_rewritten_when_it_is_a_whole_number_token() {
    // THE REGRESSION THIS EXISTS FOR. The `-0` rewrite used to fire on any
    // `-0` outside a string, so `{"x":1-0}` — which CPython's decoder REJECTS,
    // making the source skip the line in silence — lost its minus, became the
    // perfectly valid `{"x":10}`, and was dispatched to a handler as work the
    // source would never have done.
    for malformed in [
        "{\"x\":1-0}",
        "{\"x\":1.2-0}",
        "{\"x\":1e2-0}",
        "[1]-0",
        "{\"a\":1 -0}",
        "[1,2-0]",
        "-0-0",
    ] {
        assert!(
            matches!(decode_line(malformed), Err(DecodeError::Malformed(_))),
            "{malformed} is not valid JSON and must stay invalid, not be repaired \
             into something dispatchable"
        );
    }

    // And nothing that IS a whole `-0` token lost its meaning in the fixing.
    for (text, expected) in [
        ("-0", "0"),
        ("  -0  ", "0"),
        ("[-0]", "[0]"),
        ("[-0,1]", "[0, 1]"),
        ("{\"a\":-0}", "{'a': 0}"),
        ("[ -0 ]", "[0]"),
        // Untouched spellings, all still the negative float.
        ("-0.0", "-0.0"),
        ("-0e0", "-0.0"),
        // An exponent sign is not a value position, so `1e-0` is left exactly
        // as it stands — and decodes to the same 1.0 it always did.
        ("1e-0", "1.0"),
        // A `-0` inside a string is data.
        ("\"x-0y\"", "x-0y"),
    ] {
        assert_eq!(python_str(&py(text)).as_deref(), Ok(expected), "{text}");
    }

    // The whole envelope, not just the decoder: the malformed line is skipped
    // in silence and the prepared handler outcome is never consumed.
    let mut dispatcher = ScriptedDispatcher::new(vec![DispatchOutcome::Text("ok".to_string())]);
    let report = run(
        "{\"id\":1,\"method\":\"tools/call\",\"params\":{\"name\":\"hub_send\",\
         \"arguments\":{\"x\":1-0}}}",
        &mut dispatcher,
    );
    assert_eq!(report.outcome, RunOutcome::Completed);
    assert!(report.frames.is_empty(), "a skipped line answers nothing");
    assert!(
        dispatcher.calls.is_empty(),
        "no handler may run on a line the source would have skipped"
    );
    assert_eq!(dispatcher.unused(), 1);
}

#[test]
fn repr_of_a_non_ascii_string_is_refused_rather_than_guessed() {
    // Inside a container the source would call repr(), whose escaping depends
    // on CPython's printability table. Refusing is the declared obligation.
    let refusal = python_repr(&py("[\"caf\\u00e9\"]")).expect_err("this must refuse");
    assert_eq!(refusal.reason, Refusal::REPR_OF_NON_ASCII_STRING);
    assert_eq!(
        refusal.fields.get("text"),
        Some(&PyValue::Str("caf\u{e9}".to_string()))
    );
    // But str() of that same string, which is what a tool name actually goes
    // through, is fully supported.
    assert_eq!(python_str(&py("\"caf\\u00e9\"")).unwrap(), "caf\u{e9}");
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
        RunOutcome::Unrepresentable {
            line_index,
            refusal,
        } => {
            assert_eq!(line_index, 0);
            // The REASON is the part an expectation binds to, so it is the
            // part this test pins. Prose stays prose.
            assert_eq!(refusal.reason, Refusal::NON_STRING_DICT_KEY);
            assert_eq!(
                refusal.fields.get("key_type"),
                Some(&PyValue::Str("int".to_string()))
            );
            assert!(refusal.detail.contains("int"), "{}", refusal.detail);
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

// ── exact integers ─────────────────────────────────────────────────────────
// These are the crate-side half of the wide-integer work. The neutral profile
// asserts the same behaviour against the pinned Python source; what is here is
// what the profile cannot express — the frame TEXT, the refusal arm a skipped
// line took, and the helper functions on their own.

/// The 23-digit id from the profile, and the integer one above it. The two
/// share a single `f64`, which is the whole point of using them.
const WIDE: &str = "12345678901234567890123";
const WIDE_NEXT: &str = "12345678901234567890124";

fn initialize(raw_id: &str) -> String {
    format!("{{\"id\":{raw_id},\"method\":\"initialize\"}}\n")
}

fn tool_call(raw_name: &str, raw_arguments: &str) -> String {
    format!(
        "{{\"id\":1,\"method\":\"tools/call\",\
         \"params\":{{\"name\":{raw_name},\"arguments\":{raw_arguments}}}}}\n"
    )
}

#[test]
fn a_wide_id_is_echoed_as_the_exact_decimal_token() {
    let (report, _) = run_scripted(&initialize(WIDE), vec![]);
    assert_eq!(report.frames.len(), 1);
    let frame = &report.frames[0];
    // The TEXT, not the parsed value. A comparator that honours JSON's single
    // number type cannot tell an integer from the float it rounds to, so the
    // assertion that matters is about the token on the wire.
    assert!(
        frame.contains(&format!("\"id\": {WIDE},")),
        "the frame does not carry the exact token: {frame}"
    );
    // ...and specifically NOT as a quoted digit string, and NOT as a float.
    assert!(!frame.contains(&format!("\"{WIDE}\"")), "{frame}");
    assert!(!frame.contains("e+"), "{frame}");
    assert!(!frame.contains("1.2345678901234568"), "{frame}");
}

#[test]
fn two_ids_that_share_one_f64_stay_two_different_ids() {
    let input = format!("{}{}", initialize(WIDE), initialize(WIDE_NEXT));
    let (report, _) = run_scripted(&input, vec![]);
    assert_eq!(report.frames.len(), 2);
    assert!(report.frames[0].contains(WIDE), "{}", report.frames[0]);
    assert!(report.frames[1].contains(WIDE_NEXT), "{}", report.frames[1]);
    assert_ne!(report.frames[0], report.frames[1]);
    // The f64 they would both collapse to.
    let collapsed: f64 = WIDE.parse().expect("a float parse");
    assert_eq!(collapsed, WIDE_NEXT.parse::<f64>().expect("a float parse"));
}

#[test]
fn a_wide_integer_survives_every_argument_position() {
    let arguments =
        format!("{{\"flat\":{WIDE},\"list\":[[{WIDE_NEXT}]],\"map\":{{\"k\":-{WIDE}}}}}");
    let (_, dispatcher) = run_scripted(
        &tool_call("\"hub_send\"", &arguments),
        vec![DispatchOutcome::Text("ok".to_string())],
    );
    let forwarded = &dispatcher.calls[0].arguments;
    assert_eq!(forwarded.get("flat"), Some(&py(WIDE)));
    assert_eq!(
        forwarded.get("list"),
        Some(&PyValue::List(vec![PyValue::List(vec![py(WIDE_NEXT)])]))
    );
    let PyValue::Dict(inner) = forwarded.get("map").expect("the nested mapping") else {
        panic!("the nested argument is a mapping")
    };
    assert_eq!(inner.get("k"), Some(&py(&format!("-{WIDE}"))));
    // Every one of them is an `int`, not a float that happens to compare
    // equal: `py_type_of` is the check a value comparison cannot make.
    assert_eq!(py_type_of(forwarded.get("flat").expect("flat")), "int");
}

#[test]
fn a_wide_integer_tool_name_keeps_every_digit_through_str_and_repr() {
    // `str()` of a bare integer name.
    let (_, dispatcher) = run_scripted(
        &tool_call(WIDE, "{}"),
        vec![DispatchOutcome::Text("ok".to_string())],
    );
    assert_eq!(dispatcher.calls[0].tool, WIDE);
    // `repr()` of a container holding one.
    let (_, dispatcher) = run_scripted(
        &tool_call(&format!("[{WIDE},{{\"k\":-{WIDE_NEXT}}}]"), "{}"),
        vec![DispatchOutcome::Text("ok".to_string())],
    );
    assert_eq!(
        dispatcher.calls[0].tool,
        format!("[{WIDE}, {{'k': -{WIDE_NEXT}}}]")
    );
}

#[test]
fn an_integer_and_its_float_spelling_are_different_tool_names() {
    for (raw, expected, type_name) in [
        ("9007199254740993", "9007199254740993", "int"),
        ("9007199254740993.0", "9007199254740992.0", "float"),
        ("1e2", "100.0", "float"),
        ("100", "100", "int"),
        ("-0", "0", "int"),
        ("-0.0", "-0.0", "float"),
    ] {
        let (_, dispatcher) = run_scripted(
            &tool_call(raw, "{}"),
            vec![DispatchOutcome::Text("ok".to_string())],
        );
        assert_eq!(dispatcher.calls[0].tool, expected, "input {raw}");
        assert_eq!(py_type_of(&py(raw)), type_name, "input {raw}");
    }
}

#[test]
fn the_interpreter_digit_bound_is_a_silent_skip_and_not_a_refusal() {
    let at_bound = format!("1{}", "0".repeat(MAX_INT_STR_DIGITS - 1));
    let past_bound = format!("1{}", "0".repeat(MAX_INT_STR_DIGITS));

    // At the bound both sides answer.
    let (report, _) = run_scripted(&initialize(&at_bound), vec![]);
    assert_eq!(report.outcome, RunOutcome::Completed);
    assert_eq!(report.frames.len(), 1);
    assert!(report.frames[0].contains(&at_bound));

    // One digit further CPython raises ValueError, the source's
    // `except ValueError: continue` skips the line, and so does this. It must
    // NOT become a named refusal: that would record a difference that is not
    // there, and would stop the following request from being answered.
    let input = format!("{}{}", initialize(&past_bound), initialize("1"));
    let (report, _) = run_scripted(&input, vec![]);
    assert_eq!(report.outcome, RunOutcome::Completed);
    assert_eq!(report.frames.len(), 1);
    assert!(report.frames[0].contains("\"id\": 1,"));
    assert_eq!(report.lines_unprocessed, 0);
    assert!(matches!(
        decode_line(&past_bound),
        Err(DecodeError::Malformed(_))
    ));
}

#[test]
fn an_overflowing_exponent_is_still_the_declared_divergence() {
    // Reading the raw lexeme would make `1e999` saturate to an infinity the
    // way CPython's does. It deliberately does not: that obligation is not
    // this slice's to close, and it is asserted here so it cannot drift shut
    // by accident.
    for raw in ["1e999", "-1e999", "1.5e400"] {
        assert!(
            matches!(decode_line(raw), Err(DecodeError::Malformed(_))),
            "{raw}"
        );
        let input = format!("{}{}", initialize(raw), initialize("1"));
        let (report, _) = run_scripted(&input, vec![]);
        assert_eq!(report.frames.len(), 1, "{raw}");
        assert_eq!(report.outcome, RunOutcome::Completed, "{raw}");
    }
    // The bare non-standard tokens are untouched too.
    for raw in ["NaN", "Infinity", "-Infinity"] {
        assert!(
            matches!(decode_line(raw), Err(DecodeError::Malformed(_))),
            "{raw}"
        );
    }
}

#[test]
fn a_private_parser_marker_object_is_ordinary_data_end_to_end() {
    let marker = "{\"$serde_json::private::Number\":\"123\"}";
    // As an argument value.
    let (_, dispatcher) = run_scripted(
        &tool_call("\"hub_read\"", marker),
        vec![DispatchOutcome::Text("ok".to_string())],
    );
    assert_eq!(dispatcher.calls[0].arguments.len(), 1);
    assert_eq!(
        dispatcher.calls[0]
            .arguments
            .get("$serde_json::private::Number"),
        Some(&PyValue::Str("123".to_string()))
    );
    // And as the tool name, where `str()` of a mapping is its `repr()`.
    let (_, dispatcher) = run_scripted(
        &tool_call(marker, "{}"),
        vec![DispatchOutcome::Text("ok".to_string())],
    );
    assert_eq!(
        dispatcher.calls[0].tool,
        "{'$serde_json::private::Number': '123'}"
    );
}

#[test]
fn malformed_number_spellings_never_become_dispatchable() {
    // Leading zeros, bad signs, missing separators, embedded digits and the
    // landed `1-0` family. Every one of them is a line CPython's decoder
    // rejects, so the source skips it in silence and no handler runs.
    for raw in [
        "01",
        "-01",
        "+1",
        "1.",
        ".5",
        "1_000",
        "--1",
        "0x10",
        "1 2",
        "1-0",
        "1.2-0",
        "1e2-0",
        "1e",
        "1e+",
        "12345678901234567890123-0",
    ] {
        assert!(
            matches!(decode_line(raw), Err(DecodeError::Malformed(_))),
            "{raw} decoded"
        );
        let input = format!("{raw}\n{}", initialize("1"));
        let (report, dispatcher) = run_scripted(&input, vec![]);
        assert_eq!(report.frames.len(), 1, "{raw}");
        assert!(dispatcher.calls.is_empty(), "{raw} reached a handler");
    }
    // Digits INSIDE a string stay a string, which is what a quoted-digit
    // answer would look like.
    let quoted = format!("\"{WIDE}\"");
    assert_eq!(py_type_of(&py(&quoted)), "str");
    assert_eq!(python_str(&py(&quoted)).expect("a str"), WIDE);
    assert_eq!(
        python_repr(&py(&quoted)).expect("a repr"),
        format!("'{WIDE}'")
    );
}

#[test]
fn py_dumps_writes_a_wide_integer_as_a_bare_token() {
    for raw in [
        WIDE,
        WIDE_NEXT,
        "-12345678901234567890123",
        "170141183460469231731687303715884105728",
        "-170141183460469231731687303715884105728",
        "9223372036854775807",
        "-9223372036854775808",
        "18446744073709551615",
        "18446744073709551616",
        "0",
    ] {
        let value = py(raw);
        assert_eq!(py_type_of(&value), "int", "{raw}");
        assert_eq!(py_dumps(&value), raw, "{raw}");
        assert_eq!(python_str(&value).expect("a str"), raw, "{raw}");
        assert_eq!(python_repr(&value).expect("a repr"), raw, "{raw}");
        // And it round-trips: the text this crate wrote decodes to the same
        // value, which a quoted or rounded answer would not.
        assert_eq!(decode_line(&py_dumps(&value)), Ok(value), "{raw}");
    }
}
