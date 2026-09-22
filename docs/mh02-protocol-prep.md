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
| `native/mailhub-protocol/Cargo.lock` | The exact reviewed dependency closure, 12 packages. |
| `native/mailhub-protocol/.gitignore` | Ignores only this crate's `/target/`. |
| `native/mailhub-protocol/src/lib.rs` | Public surface and the declared boundaries. |
| `native/mailhub-protocol/src/envelope.rs` | Parsing, the envelope, the CPython coercion helpers and the injected dispatcher boundary. |
| `native/mailhub-protocol/tests/envelope_contract.rs` | The crate's own behaviour and framing checks. |
| `native/mailhub-protocol/examples/envelope_probe.rs` | Test-only pipe driver. Never a service. |
| `tests/fixtures/mh02-mcp-envelope.json` | The language-neutral profile: 168 envelope cases and 15 decode cases, a manifest for each, the pinned interpreter's integer digit limit, the source digests, the eight cards and the declared obligations with their bounded divergences. |
| `tests/mh02_mcp_contract.py` | The shared assertions plus the Python and Rust adapters. |
| `docs/mh02-protocol-prep.md` | This file. |

`tools/mh01_inventory.py` and `tests/test_mh01_inventory.py` change only to
register those ten paths and to keep unknown-file refusal working. Every path is
spelled out; there is no `native/` wildcard, and four new census controls prove
each entry is individually load-bearing and that an unregistered file under the
crate is still refused. The census denominator stays at 26 product files and the
frozen inventory is unchanged.

## How the decoder models CPython, and where it stops

`serde_json::Value` is not used to hold a decoded document, and the reason is
behavioural rather than stylistic. `Value` stores an object in a **sorted**
map; CPython's `json.loads` builds a `dict`, which preserves **insertion**
order. The source reads mappings in order in two places that decide what a
handler is actually called with —

* `str(p.get("name"))` when the name is a mapping: the dispatched tool string
  is the `repr`, and the `repr` is ordered, so `{"z":1,"a":2}` dispatches
  `{'z': 1, 'a': 2}` and never `{'a': 2, 'z': 1}`;
* `dict(p.get("arguments"))` over a list of mappings: a dict iterates its
  KEYS, so `[{"z":1,"a":2}]` forwards `{'z': 'a'}` and never `{'a': 'z'}` —
  a different key AND a different value.

so a sorted map silently changes the call. `serde_json`'s `preserve_order`
feature would fix it and is deliberately **not** used: it adds `indexmap` and
`equivalent` to the lock, and this slice may not add packages. The crate
decodes into its own `PyValue`/`PyDict` through `serde_json`'s streaming
parser instead, which yields object entries in document order and needs no new
package at all. `PyDict` applies CPython's duplicate-key rule: the first
occurrence keeps its position and the last assignment wins, so
`{"b":1,"a":2,"b":3}` is `{'b': 3, 'a': 2}`.

Four further points about the decoder — the first two are differences it
reproduces rather than declares away, and the last two are how it reports the
ones it cannot:

* **`-0`.** `serde_json` special-cases the integer token `-0` into the float
  -0.0, whose `str()` is `"-0.0"`; CPython decodes it with `int` and gets the
  `int` 0, whose `str()` is `"0"`. The two tokens denote one CPython object,
  so the decoder exchanges a complete `-0` number token for `0` outside string
  literals before parsing. `-0.0` and `-0e0` are untouched and stay negative
  zero. **Two things bound that rewrite, and both are load-bearing.**
  `decode_line` parses the ORIGINAL text first and returns its error unchanged,
  so syntactic validity is decided before any rewriting exists — malformed
  input cannot become executable whatever the scanner does. The scanner then
  independently requires a COMPLETE number token: the `-` must sit where a
  value may begin (start of document, or after `[`, `,` or `:`, ignoring
  whitespace) and `-0` must end the token (end of input, whitespace, `,`, `]`
  or `}`). Without those guards `{"x":1-0}` — which CPython's decoder rejects,
  so the source skips the line in silence — lost its minus, became the valid
  `{"x":10}`, and was dispatched to a handler as real work. `framing.minus-zero-*`
  carries that input and its neighbours at the public boundary.
* **Exact integers.** CPython's integers are unbounded; a machine word is
  not, and `serde_json` converts a literal outside i64/u64 to an `f64` before
  any visitor can see it. The crate therefore enables `arbitrary_precision`, a
  serde_json-only feature that adds no package to the lock, and takes the RAW
  LEXEME instead of a converted number. Nothing is rounded on the way in, and
  `PyInt` keeps the value as canonical decimal text — an optional `-`, digits
  with no leading zero, `"0"` for zero — which is exactly CPython's own
  `str()` of the integer, so decode, id echo, `str`/`repr`, `json.dumps` and
  the driver's own report all write the same digits. **No integer transits
  `f64` and none is ever written as a quoted string**; on the wire it is a
  bare JSON number token, and the profile asserts the token text as well as
  the value, because a comparator that honours JSON's single number type
  cannot tell `100` from `1e2` or a wide integer from the float it rounds to.
  Two boundaries come with it. CPython refuses to convert an integer longer
  than `sys.get_int_max_str_digits()` decimal digits — 4300 on the pinned
  verification interpreter, both live and by default — and raises
  `ValueError`, which the source's `except ValueError: continue` turns into a
  silent skip; the crate states the same bound as `MAX_INT_STR_DIGITS` and
  skips the same lines, so 4300 digits is answered by both sides and 4301 is
  skipped by both. An interpreter configured with another limit is a
  qualification mismatch: the profile asserts the live and default values, and
  the driver reports the bound it enforces so the two are compared before any
  case is judged. And `arbitrary_precision` delivers a number as a ONE-ENTRY
  MAP under the private key `$serde_json::private::Number`, which ordinary
  JSON could spell too; three conditions have to hold together before a map is
  read as a number — the value arrives as an OWNED `String` (serde_json's
  parser never does that for a document string; `StrRead`/`SliceRead` borrow
  or copy a `&str` and `IoRead` calls `visit_str`), the map holds exactly that
  one entry, and the text is one complete JSON number token. When any of them
  fails the entry stays an ordinary key and string value and the map is
  finished as the mapping it is. A user object built to look like the marker
  is carried as a profile case and as a crate test.
* **Nesting depth.** `serde_json`'s default bound is 128 nested containers,
  which ordinary input can cross while CPython's decoder does not. The crate
  raises that bound (`unbounded_depth`, a serde_json-only feature that adds no
  package) and enforces its own stated `MAX_NESTING_DEPTH` of 256, so 140-deep
  input is answered identically on both sides. A number is a SCALAR however
  the parser delivers it, so the one-entry map that carries a wide integer
  does not consume a nesting level: the first key is read before the depth
  test, and a crate test holds that shut at exactly the bound.
* **Refusal versus silence.** `DecodeError` separates `Malformed`, which is
  skipped in silence under the source's `except ValueError: continue`, from
  `Unrepresentable`, which stops the run with a NAMED refusal because routing
  it to the skip arm would dress a divergence up as the source's own
  behaviour. Read that split precisely. `Unrepresentable` is exact: it is
  reached only where the crate KNOWS CPython would have succeeded — past the
  depth bound, and on the two `repr`/mapping-key boundaries below.
  `Malformed` is the default arm, and it is **not** a proof that CPython would
  have rejected the same text. It carries every parser failure other than the
  depth bound, and `serde_json` rejects some input CPython accepts. The three
  known ones — the bare `NaN`/`Infinity` tokens, a lone surrogate escape, and
  an overflowing literal such as `1e999` — are recorded as named obligations
  and exercised from both sides, so they are visible rather than hidden. What
  the split cannot promise is that no FURTHER such input exists; only the ones
  written down are accounted for, and finding another one means adding an
  obligation, not widening this arm. One refusal in this arm is **parity, not
  a divergence**: an integer literal longer than the pinned interpreter's
  `MAX_INT_STR_DIGITS` decimal digits, where CPython raises `ValueError` and
  the source skips the line too. Naming it as an obligation would record a
  difference that is not there.
* **Which refusal, not just that one happened.** Every `Unrepresentable`
  carries a `Refusal`: a STABLE reason name — `nesting-depth-exceeded`,
  `repr-of-non-ascii-string`, `non-string-dict-key` — plus the `fields` that
  make it specific (the bound exceeded, the string, the key type), plus prose
  that is never load-bearing. A profile case that declares a refusal must name
  the reason and the fields, and is judged against both. Binding only the line
  number and "some prose is present" meant any refusal at all satisfied a
  declared gap, including one belonging to a different obligation.

Reply frames are written by the crate's own `json.dumps`-faithful encoder —
`", "`/`": "` separators, `ensure_ascii`, mappings in their own order — rather
than by `serde_json`, so an object echoed back inside `id` is never re-sorted
on the wire.

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

"By value" means JSON's values, not Python's. `json_equal()` does the
comparing, and it keeps `bool` apart from `int` at every depth: Python's
`False == 0` and `True == 1`, so a plain `==` accepted an implementation that
answered `"id": 0` where the source answers `"id": false`. The source echoes
`id` verbatim, so that is a change on the wire and reads as one. Two things
stay deliberately loose and are policy rather than oversight: an int and a
float of equal value compare equal, because JSON has one number type and the
source never distinguishes them; and object key order inside a REPLY is not
compared. Key order inside a decoded INPUT is a different matter and is a hard
requirement — it decides the dispatched name and the forwarded key/value pair,
and the `name.*` and `arguments.*` cases assert it through those values.

**How the driver process ends is part of the result.** The runner records the
child's exit status and gates on it: anything other than the expected status,
or a process that had to be killed, is a cleanup error, and cleanup errors
block collection. A driver that answered every job correctly and then died has
not passed — reading its answers and ignoring its exit status is how a run
stays green while the implementation under test is aborting. The driver carries
one test-only job, `{"op":"exit-after-input","code":N}`, so that gate is proven
against a **real** process: a control arms the failure, checks that the driver
keeps producing correct observations afterwards, and then requires the non-zero
exit to surface as a cleanup error.

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

Disagreeing is not enough on its own. Every declaration also carries a
**bounded `observed` outcome** — the same shape as a normative expectation,
judged by the same `case_errors()` — saying exactly what the Rust side really
does on that input, plus a one-line `difference`. Both halves are required:
the normative expectation must still fail, AND the observation must match the
bounded outcome exactly. Without the second half, "declared unimplemented"
degenerates into "any failure on this input counts", and an unrelated defect —
a lost result envelope, an invented JSON-RPC error member, a wrong id, a
dropped handler call, a request skipped entirely — passes as though it were the
difference the obligation names. Standing controls break each declaration in a
way it does not name and require the bound to reject it.

A declaration whose bounded outcome is a **refusal** carries one more thing:
the refusal's stable `reason` and its `fields`. Without them the bound says
only that something was refused on that line, and an entirely unrelated
refusal — a different obligation's, or an invented transport-policy rejection
— satisfies it while the whole suite stays green. Five standing controls hold
that shut: three replace the observed reason or its fields (with an unrelated
one, with another obligation's, and with the wrong stated bound), and two
strip `reason` and `fields` out of the DECLARATION and require the real
observation to stop being accepted by it.

| Obligation | What Rust cannot do |
|---|---|
| ~~`numeric-domain.integer-wider-than-64-bits`~~ **CLOSED** | CPython integers are unbounded and `serde_json` USED TO narrow anything outside i64/u64 to f64, so a 23-digit id lost its exact value. It no longer does: see **Exact integers** above. The obligation keeps its id and the case that carried it keeps its own, and both now assert the source's answer. A stale `still divergent` declaration on a case that passes is a failure, and so is a `closed` row that names no case asserting the behaviour, names a case that does not exist, or is still declared by one. |
| `numeric-domain.bare-non-standard-tokens` | CPython's `json` accepts bare `NaN`/`Infinity`/`-Infinity` and returns a float, which ends the source's loop. `serde_json` rejects them, so Rust skips the line and keeps going. |
| `string-domain.lone-surrogate-escape` | CPython decodes a lone `\ud800` into an unpaired surrogate and returns a `str`, ending the loop. `serde_json` rejects it; a Rust `String` cannot hold one. |
| `repr-domain.non-ascii-inside-a-container` | `repr()` of a non-ASCII string follows CPython's printability table. The crate refuses with reason `repr-of-non-ascii-string` and the string itself, instead of guessing. `str()` of a bare string tool name — the reachable case — is exact. |
| `key-domain.non-string-dictionary-key` | `dict([[1,2]])` is `{1: 2}`. A JSON object key must be a string, so the crate refuses with reason `non-string-dict-key` and the offending key's type, rather than coercing it to `"1"`. Exercised by the crate's own test, because a language-neutral case cannot state the expectation. |
| `numeric-domain.overflowing-float-literal` | CPython decodes a finite literal whose exponent overflows, such as `1e999`, to the float `inf`, which ends the source's loop. `serde_json` reports it as out of range, so Rust skips the line. |
| `depth-domain.nesting-past-the-stated-bound` | CPython's decoder accepts nesting far deeper than this crate states a bound for. Past `MAX_NESTING_DEPTH` the crate refuses the line with reason `nesting-depth-exceeded` and the bound it exceeded, rather than letting a decoder error read as the source's silent skip. 140 deep is answered identically by both; 257 deep is the declared refusal. |

Six obligations remain open and unchanged by this slice. The seventh is
closed above; nothing else moved, and both MH01 conversion blockers stay open.

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

Two dependencies are declared: `serde_json 1.0.150` and `serde_core 1.0.228`
(both MIT OR Apache-2.0). Naming `serde_core` **adds no package to the lock** —
`serde_json` already depends on it, so the lock gains one dependency edge and
no new `[[package]]` entry. It is named directly for the one custom
`DeserializeSeed` that keeps object entries in document order. The `serde`
facade and its derive macros are not used; this crate derives nothing.

The closure is unchanged at twelve packages, which is the whole of
`Cargo.lock`: `mailhub-protocol` itself, `serde_json 1.0.150`, `itoa 1.0.18`,
`memchr 2.8.3`, `serde 1.0.228`, `serde_core 1.0.228`, `serde_derive 1.0.228`,
`zmij 1.0.21`, `proc-macro2 1.0.106`, `quote 1.0.46`, `syn 2.0.118` and
`unicode-ident 1.0.24`. (The round-3 revision of this file said eleven and
listed ten; the lock has always held twelve, and the count is corrected here
rather than left to be rediscovered.) There is no SQL, HTTP, async or process dependency,
and none is needed. The lock was generated and every check runs with
`--offline --locked`.

Feature selection, stated rather than defaulted, and **the lock is byte
identical either way** — every feature below is serde_json-only and adds no
package. `unbounded_depth` is ON, because the library's 128-container default
is shallower than CPython's and the crate enforces its own stated bound
instead. `arbitrary_precision` is ON, because it is the only way to see a wide
integer's exact digits without hand-writing a JSON parser: it hands over the
raw lexeme rather than a converted number. `float_roundtrip` is OFF and not
needed — float lexemes are parsed with Rust's own correctly-rounded `f64`
parser, which agrees with CPython's `float()` on every finite literal.
`preserve_order` is unused, because it would add `indexmap` and `equivalent`
to the lock; the crate decodes into its own insertion-ordered `PyValue`
instead.

Numeric-domain behaviour of the chosen decoder, recorded: every integer the
pinned interpreter accepts is exact, at any width — 2⁵³±1, the i64 and u64
boundaries, past 2¹²⁷, 100 digits and the interpreter's own 4300-digit bound —
and two integers that share one `f64` stay two integers; an integer literal
with more than 4300 decimal digits is skipped, which is what CPython's
`ValueError` makes the source do as well; `NaN`, `Infinity` and `-Infinity`
are rejected as input, and so is a finite literal whose exponent overflows,
such as `1e999`, where CPython returns `inf`; the integer token `-0` is the
`int` 0 the way CPython decodes it, while `-0.0` and `-0e0` stay negative
zero; an exponent or fraction spelling is a `float` and stays one; float
output differs from CPython only in formatting, which value comparison
absorbs.

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

The profile runs 433 tests: 168 envelope cases and 15 decode cases on each of
the two targets, plus the registry, control, obligation and cleanup checks. An
ENVELOPE case puts a whole stdin text through `serve()` and judges the reply
stream; a DECODE case puts ONE line through the decoder alone and judges what
CPython says the value IS — its type name, `str()`, `repr()`, `json.dumps()`,
its digit count, and the value that actually travelled down the pipe. Decode
cases carry only input the two sides agree on; every declared divergence stays
in the obligation machinery above.

A missing Rust driver is an **environment blocker** that fails the run. It is
never a skip and never a pass: `--target python` alone, an absent driver and a
driver that does not exist all leave the whole Rust half of the profile red
rather than green. A driver that runs but exits non-zero is blocked the same
way.

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
