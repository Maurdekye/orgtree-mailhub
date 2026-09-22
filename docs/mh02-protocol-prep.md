# MH02 protocol preparation — the MCP stdio envelope

This slice adds one isolated Rust library, `native/mailhub-protocol`, that
reproduces a single surface of the existing Python mail hub client: the MCP
request/reply envelope implemented by `serve()` in `hubtool.py`. It exists so
the later port has an executable compatibility component for that envelope
before a storage engine, a transport or any runtime authority is chosen.

**It is preparation, not MH02.** No product exists here, nothing is registered,
nothing is launched and nothing is packaged.

## Exact source

Everything below is a statement about one commit and nothing else.

| Anchor | Value |
|---|---|
| Product commit | `6477321f89d2d2e1b9313e71e940c76c35b892fb` |
| Product tree | `914a67b5cf39c48faec9fa65d509ba142daab8fd` |
| `hubtool.py` sha256 | `91eadcaadc9f4d80ea72612188bc58f60377f85352a7d11562ed4bda5192c831` (60079 bytes) |
| `serve()` | lines 1168–1202; segment sha256 `3182b5a64299afdbd44fc7c4be29bdc1f81c564e08781e004f60c35665fee3f8` |
| `TOOLS` | lines 975–1046; segment sha256 `f95af192c8df0b51e321d3cff09c4172c598031099c4628ce33bd5f2af9e7a8c` |

The same digests are pinned inside `tests/fixtures/mh02-mcp-envelope.json` and
re-derived from the live pinned blob on every run. A source change moves the
digest and stops the suite rather than quietly disagreeing with it.

## What the envelope actually does

These are the source's real distinctions. Several of them contradict a generic
reading of MCP or JSON-RPC, and the profile asserts the source, not the reading.

1. **A recognised method is always answered.** `initialize`, `tools/list` and
   `tools/call` reply even when `id` is absent or null; the reply then carries
   `"id": null`. There is no notification suppression.
2. **An unrecognised method is answered only when the id is usable**, and the
   answer is `{}` — an empty *result*. `id: 0`, `id: false` and `id: ""` are all
   answered, because none of them is `None`. A missing or null id gets no reply
   at all.
3. **Blank and unparseable lines are skipped in silence.** There is no
   parse-error reply. Silence is the contract.
4. **A line that parses but is not a JSON object is terminal.** The source calls
   `.get` on it, the `AttributeError` escapes `serve()`, and every later line is
   never processed. This is neither a reply nor a skip, and how many frames went
   unprocessed is part of the contract.
5. **Handler failures never become JSON-RPC errors.** Both `except` arms turn the
   failure into a JSON string inside `result.content[0].text`, so the envelope
   still reads as success. `urllib.error.URLError` becomes
   `{"error": "hub unreachable: <reason>"}`; anything else becomes
   `{"error": "<str(e)>"}`.
6. **Malformed params fail inside that same boundary.** `params` that is truthy
   but not a mapping, and `arguments` that `dict()` cannot consume, both produce
   CPython's own exception text in that error string rather than a refusal.
7. **Coercion is lossy on purpose.** `str(p.get("name"))` turns a missing name
   into the literal string `"None"`; `p.get("params") or {}` treats `0`, `false`,
   `""` and `[]` as absent.
8. **Line splitting is `sys.stdin`'s.** `\n`, `\r\n` and a lone `\r` all end a
   line, and `str.strip()` — not Unicode `White_Space` — decides what is blank.
   U+001C..U+001F are stripped by Python and are *not* whitespace to Rust's
   `char::trim`, so the crate carries CPython's exact 29-code-point set.

## Schema of the crate

| Path | What it is |
|---|---|
| `native/mailhub-protocol/Cargo.toml` | Standalone library crate with an empty `[workspace]`, so it cannot be absorbed into a parent workspace. No binary target. |
| `native/mailhub-protocol/Cargo.lock` | The exact reviewed dependency closure, 11 packages. |
| `native/mailhub-protocol/.gitignore` | Ignores only this crate's `/target/`. |
| `native/mailhub-protocol/src/lib.rs` | Public surface and the declared boundaries. |
| `native/mailhub-protocol/src/envelope.rs` | Parsing, the envelope, the CPython coercion helpers and the injected dispatcher boundary. |
| `native/mailhub-protocol/tests/envelope_contract.rs` | The crate's own behaviour and framing checks. |
| `native/mailhub-protocol/examples/envelope_probe.rs` | Test-only pipe driver. Never a service. |
| `tests/fixtures/mh02-mcp-envelope.json` | The language-neutral profile: 135 cases, a case manifest, the source digests, the eight cards and the declared obligations. |
| `tests/mh02_mcp_contract.py` | The shared assertions plus the Python and Rust adapters. |
| `docs/mh02-protocol-prep.md` | This file. |

`tools/mh01_inventory.py` and `tests/test_mh01_inventory.py` change only to
register those ten paths and to keep unknown-file refusal working. Every path is
spelled out; there is no `native/` wildcard, and four new census controls prove
each entry is individually load-bearing and that an unregistered file under the
crate is still refused. The census denominator stays at 26 product files and the
frozen inventory is unchanged.

## The oracle

`tests/mh02_mcp_contract.py` never imports `hubtool`. It reads the pinned blob
out of Git, extracts the exact `serve` function and the exact `TOOLS` literal
from the AST, checks their digests, and compiles that one function into a
namespace holding only:

* `TOOLS` — the literal, evaluated with `ast.literal_eval`;
* `cast` — a function that returns its second argument;
* `dispatch` — a fake that records its arguments and replays scripted outcomes;
* `json` — the real module;
* `sys` — in-memory streams, with universal-newline decoding so `\r` behaves as
  it does on a real stdin;
* `urllib` — a namespace carrying nothing but `error.URLError`.

Builtins are restricted to `str`, `dict`, `Exception` and `ValueError`: there is
no `open`, no `__import__`, no `eval`. The module's main guard, CLI, identity
helpers and listener are never compiled, so there is no code path from the test
to a profile, a store or a socket. While the extracted function runs, an audit
hook refuses **every** audited operation, and a control proves the hook fires.

Compilation uses the source's own `from __future__ import annotations` flag.
Without it the inner `def reply(id_: Any, …)` would evaluate an annotation the
namespace deliberately does not hold; the suite asserts which names are
annotation-only so that dependency stays visible.

Reading Git and launching the Rust driver are explicit harness operations. They
happen outside the guarded region and are not product effects.

## The profile and the drivers

The profile is the contract; the two adapters are drivers for it. Both produce
the same observation shape — frames, raw stream, recorded handler calls,
outcome, terminal cause, line counts — and one `case_errors()` function judges
both. Implementation selection sits outside the assertions.

Reply envelopes are compared **by value**, so object key order and the outer
`json.dumps` spacing are not treated as product requirements. Strings inside a
result, including `result.content[0].text`, compare exactly, because that is
where the source's own sentences live. Framing is asserted separately: one JSON
document per line, one newline per frame, and the raw stream equal to their
concatenation.

Coverage: all three methods; the eight tool names, each forwarded to a synthetic
handler; string, integer, zero, negative, false, null, missing, object, list,
float and Unicode ids; unknown methods with and without a usable id; blank,
invalid, truncated and non-object JSON; params and arguments coercion in every
falsy and malformed shape; all three handler outcomes; multiple frames, no
trailing newline, CRLF, lone CR, Unicode payloads, escaped line breaks and the
exotic whitespace classes; and the terminal outcome with its unprocessed-frame
count.

## Declared obligations

Some pinned-source behaviour depends on CPython specifics a JSON decoder does not
reproduce. Each is DECLARED in the profile, each runs on both implementations,
and the Rust side is required to **disagree**: an obligation that quietly starts
passing is a stale declaration and fails the run exactly as a regression would.
None of them is skipped, and none is absorbed by a narrower profile.

| Obligation | What Rust cannot do |
|---|---|
| `numeric-domain.integer-wider-than-64-bits` | CPython integers are unbounded; `serde_json` narrows anything outside i64/u64 to f64, so a 23-digit id loses its exact value. |
| `numeric-domain.bare-non-standard-tokens` | CPython's `json` accepts bare `NaN`/`Infinity`/`-Infinity` and returns a float, which ends the source's loop. `serde_json` rejects them, so Rust skips the line and keeps going. |
| `string-domain.lone-surrogate-escape` | CPython decodes a lone `\ud800` into an unpaired surrogate and returns a `str`, ending the loop. `serde_json` rejects it; a Rust `String` cannot hold one. |
| `repr-domain.non-ascii-inside-a-container` | `repr()` of a non-ASCII string follows CPython's printability table. The crate reports `unrepresentable` instead of guessing. `str()` of a bare string tool name — the reachable case — is exact. |
| `key-domain.non-string-dictionary-key` | `dict([[1,2]])` is `{1: 2}`. A JSON object key must be a string, so the crate refuses rather than coercing it to `"1"`. Exercised by the crate's own test, because a language-neutral case cannot state the expectation. |

`arbitrary_precision` is deliberately **not** enabled on `serde_json`: it would
change the numeric domain this slice is characterising rather than record it.

## Toolchain and dependencies

Recorded for this bounded trial only. This is not a programme-wide toolchain
adoption, and nothing here was installed or updated.

| Component | Exact identity |
|---|---|
| Toolchain | `nightly-x86_64-pc-windows-msvc` |
| rustc | `1.94.0-nightly (f52090008 2025-12-10)` |
| cargo | `1.94.0-nightly (2c283a9a5 2025-12-04)` |
| rustfmt | `1.8.0-nightly (f520900083 2025-12-10)` |
| clippy | `0.1.93 (f520900083 2025-12-10)` |
| Python | CPython 3.13.15, the bundled engine runtime |

The only declared dependency is `serde_json 1.0.150` (MIT OR Apache-2.0), with
default features. Its closure is `itoa 1.0.18`, `memchr 2.8.3`, `serde 1.0.228`,
`serde_core 1.0.228`, `serde_derive 1.0.228`, `zmij 1.0.21`, `proc-macro2
1.0.106`, `quote 1.0.46`, `syn 2.0.118` and `unicode-ident 1.0.24` — eleven
packages including the crate itself. There is no SQL, HTTP, async or process
dependency, and none is needed. The lock was generated and every check runs with
`--offline --locked`.

Numeric-domain behaviour of the chosen decoder, recorded: integers inside i64 or
u64 are exact (including 2⁵³+1, which a naive f64 implementation loses);
anything wider becomes f64; `NaN`, `Infinity` and `-Infinity` are rejected as
input; float output differs from CPython only in formatting, which value
comparison absorbs.

## Commands

```text
cargo fmt --manifest-path native/mailhub-protocol/Cargo.toml --check
cargo clippy --offline --locked --manifest-path native/mailhub-protocol/Cargo.toml --all-targets -- -D warnings
cargo test --offline --locked --manifest-path native/mailhub-protocol/Cargo.toml
cargo build --offline --locked --manifest-path native/mailhub-protocol/Cargo.toml --example envelope_probe
python -B tools/mh01_inventory.py
python -B tests/test_mh01_inventory.py
python -B tests/mh01_contract.py
python -B tests/mh02_mcp_contract.py --target both --rust-driver <built envelope_probe>
```

A missing Rust driver is an **environment blocker** that fails the run. It is
never a skip and never a pass: `--target python` alone, an absent driver and a
driver that does not exist all leave 139 tests red rather than green.

## Cleanup

The Python harness creates no temporary directory — it imports neither
`tempfile` nor `shutil`, and a control asserts that against its own source. The
Rust driver is the only process it owns. `DriverSession` bounds each run in time
and in output, closes both pipes and stdin, waits, kills only if the wait
expires, and proves the process is gone; because the teardown lives in
`__exit__`, it runs on success, on an assertion failure and on a timeout alike,
and there is one control for each of those three.

## What stays blocked

* **The MCP conversion blocker recorded by the MH01 census is still open.** Every
  handler here is synthetic. This slice says nothing about the eight tools'
  actual identity, SQL, transport, retry or shutdown behaviour; it narrows the
  blocker to the envelope and no further.
* **Listener behaviour is untouched**: PID reuse, lock acquisition and release,
  unlink failure, early exit, reconnect and multi-hub operation remain
  unexercised. The MH01 review's nonblocking correction also still stands — the
  documented empty-list `hubs0[0]` example is unreachable because `_hubs`
  supplies a fallback, and it is not a demonstrated fault.
* **Storage is not selected.** Mailhub storage mapping, source authority,
  original-key replay versus legacy global-id duplicate behaviour and the
  `/api/roster` parent-probe mismatch all still need recorded dispositions.
  Nothing here chooses PostgreSQL or SQLite for the independent product.
* **Full MH02 needs the delivered P01–P10 assets and phase clearance**, a
  production implementation, complete public/client coverage and native
  concurrency, crash and recovery qualification. This library supplies one
  reusable input.
* **MH03 and R10 stay separate.** Nothing here is a Rust parent/product pair, a
  queue, a receipt or a migration, and a Rust test driver does not begin to
  satisfy the Python-free packaging gate.
