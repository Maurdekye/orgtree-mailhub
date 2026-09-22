//! The pinned mailhub MCP stdio envelope, reproduced against one exact source.
//!
//! The semantic reference is `hubtool.py`'s `serve()` function and its literal
//! `TOOLS` declaration at product commit
//! `6477321f89d2d2e1b9313e71e940c76c35b892fb`. Everything in this file is a
//! statement about that source, not about MCP in general: where the source and
//! a generic MCP reading disagree, the source wins and the disagreement is
//! written down rather than normalised away.
//!
//! What this module does NOT do, by construction: no transport, no socket, no
//! file, no database, no account, no credential, no clock, no identity, no
//! PID, no listener, no registration, no process. It takes a `&str` of already
//! received text and an injected [`Dispatcher`], and returns frames.
//!
//! The four behaviours that a generic MCP implementation gets wrong here:
//!
//! 1. A recognised method is answered even when `id` is absent or null — the
//!    reply then carries `"id": null`. There is no notification suppression
//!    for `initialize`, `tools/list` or `tools/call`.
//! 2. An UNRECOGNISED method is answered only when `id` is neither missing nor
//!    null, and the answer is `{}` — an empty result, never a JSON-RPC error
//!    member. `id: 0` and `id: false` are answered; they are not `None`.
//! 3. A blank line or a line that fails to parse as JSON is skipped in
//!    silence. There is no parse-error reply.
//! 4. A line that PARSES but is not a JSON object is terminal: the source
//!    calls `.get` on it, the `AttributeError` escapes `serve()`, and every
//!    later line is never processed. This is the one outcome that is neither
//!    a reply nor a skip.
//!
//! Handler failures never become JSON-RPC errors either. They are turned into
//! a JSON string INSIDE `result.content[0].text`, so the envelope still reads
//! as success.
//!
//! # Why this module does not decode into `serde_json::Value`
//!
//! `serde_json::Value` stores an object in a sorted map, and CPython's
//! `json.loads` builds a `dict`, which preserves INSERTION order. That is not
//! a formatting difference: the source calls `str()` on a tool name and
//! `dict()` on an argument list, and both of those read the mapping in order,
//! so sorting changes the tool that is dispatched and the key/value pair that
//! is forwarded. Enabling serde_json's `preserve_order` feature would fix it
//! but pulls in a dependency this slice is not permitted to add, so the module
//! decodes into its own [`PyValue`] instead — still through serde_json's
//! parser, which yields object entries in document order, with no new
//! dependency at all. [`PyDict`] then applies CPython's duplicate-key rule:
//! the FIRST occurrence keeps its position and the LAST assignment wins.

use std::fmt;
use std::sync::OnceLock;

use serde_core::de::{DeserializeSeed, Deserializer, MapAccess, SeqAccess, Visitor};

/// Product commit whose `serve()` and `TOOLS` this module reproduces.
pub const SOURCE_COMMIT: &str = "6477321f89d2d2e1b9313e71e940c76c35b892fb";

/// `initialize` metadata, verbatim from the pinned source.
pub const PROTOCOL_VERSION: &str = "2024-11-05";
/// `serverInfo.name`, verbatim from the pinned source.
pub const SERVER_NAME: &str = "mailhub";
/// `serverInfo.version`, verbatim from the pinned source.
pub const SERVER_VERSION: &str = "1.0";

/// How many nested containers this module decodes.
///
/// CPython's decoder is bounded by its own recursion limit and reaches far
/// deeper than anything an MCP frame plausibly carries; serde_json's default
/// bound is 128, which is shallow enough that ordinary input crosses it. This
/// module raises serde_json's bound and imposes its own, stated one instead,
/// and input beyond it is REFUSED by name — never silently dropped, which is
/// what a decoder error would otherwise become under the source's
/// `except ValueError: continue`.
pub const MAX_NESTING_DEPTH: usize = 256;

/// How many decimal DIGITS an integer literal may carry.
///
/// CPython's integers are unbounded, but converting one to or from a decimal
/// STRING is not: since 3.11 the interpreter refuses a conversion longer than
/// `sys.get_int_max_str_digits()` and raises `ValueError`. The pinned
/// verification interpreter, CPython 3.13.15, reports both the live and the
/// default limit as 4300, and `json.loads` of a 4301-digit literal raises
/// there — which the source's `except ValueError: continue` turns into a
/// SILENT SKIP.
///
/// So this bound is PARITY, not a divergence, and it is deliberately the same
/// number rather than a smaller invented cap: at 4300 digits both sides decode
/// the integer exactly, and at 4301 both sides skip the line. A decoder that
/// accepted 4301 digits would answer a request the pinned source refuses to
/// answer, which is a protocol change in the direction that is hardest to see.
///
/// An interpreter configured with a different limit is an explicit
/// QUALIFICATION MISMATCH: the shared profile asserts the oracle's live value
/// before it compares anything, and fails rather than quietly re-deriving the
/// expectation from whatever interpreter happens to be running.
pub const MAX_INT_STR_DIGITS: usize = 4300;

/// The eight tool cards, in the source's declaration order, byte-derived from
/// the pinned `TOOLS` literal by `ast.literal_eval` and re-serialised as JSON.
/// Order is contractual: `tools/list` returns this list as it stands.
pub const TOOLS_JSON: &str = r##"[
  {
    "name": "hub_register",
    "description": "Join the mail hub as THIS SESSION. Choose a UNIQUE, semantically appropriate `name` that reflects this session's own context / directive / purpose (e.g. 'orgtree-redteam', 'terrain-pipeline') — every session has its own identity, and the name is the key. REMEMBER the name you chose: registering with it again later resumes the SAME address (<name>.<user>.<fingerprint>); a different name is a different identity. Immutable once minted. Returns your address and the roster — ⚠ if the result carries `resumed` and YOU did not register this name earlier, another session owns it: pick a different name.",
    "inputSchema": {
      "type": "object",
      "properties": {
        "name": {
          "type": "string",
          "description": "this session's self-chosen identity name — unique, purpose-describing, reused on every later register"
        }
      }
    }
  },
  {
    "name": "hub_list",
    "description": "Everyone on your hubs — orgs and chats — with kind, presence and last_seen. On a multi-hub identity the rosters are MERGED, one row per slug, each row's `hubs` naming where it lives.",
    "inputSchema": {
      "type": "object",
      "properties": {}
    }
  },
  {
    "name": "hub_send",
    "description": "Send mail to any hub client (org or chat) by its slug from hub_list. On a multi-hub identity the hub is resolved by roster (several hold the target → the local one wins; none → refused naming the hubs searched — never guessed).",
    "inputSchema": {
      "type": "object",
      "properties": {
        "to": {
          "type": "string"
        },
        "body": {
          "type": "string"
        }
      },
      "required": [
        "to",
        "body"
      ]
    }
  },
  {
    "name": "hub_fetch",
    "description": "Download a mail attachment by its id (the listener and hub_read surface ids beside filenames). Saves into `dir` (default: the current directory) and returns the written path.",
    "inputSchema": {
      "type": "object",
      "properties": {
        "id": {
          "type": "string"
        },
        "dir": {
          "type": "string"
        }
      },
      "required": [
        "id"
      ]
    }
  },
  {
    "name": "hub_unregister",
    "description": "The polite exit: remove this identity's row from every hub on its list (queued mail for you ages out on the hub's retention). Your local identity is KEPT — registering again later resumes the identical address.",
    "inputSchema": {
      "type": "object",
      "properties": {}
    }
  },
  {
    "name": "hub_read",
    "description": "Fetch (and consume) any mail waiting for you right now — across ALL your hubs; each message carries `hub` so you know where to reply (acks go back to the hub each message came from automatically).",
    "inputSchema": {
      "type": "object",
      "properties": {}
    }
  },
  {
    "name": "hub_wait",
    "description": "Wait up to `timeout` seconds (max 55) for new mail — polls every hub on your list (the window is split across them); each message carries `hub`; empty result on timeout.",
    "inputSchema": {
      "type": "object",
      "properties": {
        "timeout": {
          "type": "number"
        }
      }
    }
  },
  {
    "name": "hub_hubs",
    "description": "This identity's mailserver list. No args = show it. `add` joins another hub (registers there immediately; same address everywhere — the fingerprint derives from your uid, not the hub). `remove` drops one (its polling stops; the others' cursors are untouched). Addresses accept bare host / host:port (http + :7370 assumed).",
    "inputSchema": {
      "type": "object",
      "properties": {
        "add": {
          "type": "string"
        },
        "remove": {
          "type": "string"
        }
      }
    }
  }
]"##;

// ──────────────────────────────────────────────────── CPython's object model

/// One decoded JSON document, with CPython's object model where it differs
/// from serde_json's.
///
/// Two differences are load-bearing and neither is cosmetic:
///
/// * a mapping keeps INSERTION order, because the source reads mappings in
///   order (`str(dict)` and `dict(iterable)`), so order decides which tool is
///   dispatched and which key/value pair is forwarded;
/// * `int` and `float` are distinct, because `str()` of them differs and the
///   source's tool name is `str(p.get("name"))`.
#[derive(Clone, Debug, PartialEq)]
pub enum PyValue {
    /// JSON `null`, CPython `None`.
    None,
    /// JSON `true`/`false`, CPython `bool`.
    Bool(bool),
    /// A JSON integer literal, CPython `int`, held EXACTLY — see [`PyInt`].
    Int(PyInt),
    /// A JSON number written with a fraction or an exponent, CPython `float`.
    Float(f64),
    /// CPython `str`.
    Str(String),
    /// CPython `list`.
    List(Vec<PyValue>),
    /// CPython `dict`, in insertion order.
    Dict(PyDict),
}

/// A CPython `int`, held exactly.
///
/// CPython's integers are unbounded and a machine word is not, so the value is
/// kept as its CANONICAL decimal text: an optional `-`, then decimal digits
/// with no leading zero, and `"0"` for zero. That is exactly the spelling
/// CPython's own `str()` of the integer produces, so [`py_dumps`] and
/// [`python_repr`] write the text straight out and cannot round, truncate or
/// re-spell the value on the way. Because the form is canonical, two `PyInt`s
/// are equal exactly when the integers they denote are.
///
/// Two things this type exists to make impossible:
///
/// * no integer ever transits `f64`. `12345678901234567890123` and
///   `12345678901234567890124` are distinct here, where the nearest `f64` is
///   one value for both;
/// * no integer is ever written to the wire as a QUOTED string. The decimal
///   text is an internal representation; on the wire it is a bare JSON number
///   token, which is what the pinned source emits.
///
/// The width a literal may have on the way IN is bounded by
/// [`MAX_INT_STR_DIGITS`], which mirrors the pinned interpreter rather than
/// this crate's own convenience.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct PyInt {
    decimal: String,
}

impl PyInt {
    /// The canonical decimal text — CPython's `str()` of the integer.
    pub fn as_decimal(&self) -> &str {
        &self.decimal
    }

    /// Whether the value is zero. `-0` canonicalises to `0`, exactly as
    /// CPython's decoder gives one `int` for both spellings.
    pub fn is_zero(&self) -> bool {
        self.decimal == "0"
    }

    /// Whether the value is negative.
    pub fn is_negative(&self) -> bool {
        self.decimal.starts_with('-')
    }

    /// How many decimal digits the value has, ignoring the sign. This is the
    /// count CPython's `sys.get_int_max_str_digits()` limit is measured in.
    pub fn digits(&self) -> usize {
        self.decimal.len() - usize::from(self.is_negative())
    }

    /// The value as an `i128`, or `None` when it does not fit.
    ///
    /// Nothing in the envelope needs this — the envelope only ever echoes,
    /// prints and forwards integers, and all three are exact on the text. It
    /// exists for a caller that genuinely wants a machine integer, such as the
    /// test-only driver reading its own exit code out of a job.
    pub fn to_i128(&self) -> Option<i128> {
        self.decimal.parse().ok()
    }

    /// Build from text that is ALREADY canonical. Private on purpose: every
    /// public route in is either a machine integer or [`number_from_token`],
    /// and both canonicalise.
    fn from_canonical(decimal: String) -> Self {
        PyInt { decimal }
    }
}

impl fmt::Display for PyInt {
    fn fmt(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str(&self.decimal)
    }
}

macro_rules! py_int_from_machine_integer {
    ($($type:ty),* $(,)?) => {
        $(
            impl From<$type> for PyInt {
                /// Rust's own decimal formatting of a machine integer is
                /// already canonical: no leading zero, `-` for negatives and
                /// `0` for zero.
                fn from(value: $type) -> PyInt {
                    PyInt { decimal: value.to_string() }
                }
            }
        )*
    };
}

py_int_from_machine_integer!(i8, i16, i32, i64, i128, isize, u8, u16, u32, u64, u128, usize);

/// An insertion-ordered string-keyed mapping with CPython's duplicate-key
/// rule: assigning to an existing key REPLACES its value and LEAVES its
/// position, so `{"b":1,"a":2,"b":3}` decodes as `{'b': 3, 'a': 2}` — not as
/// `{'a': 2, 'b': 3}`, which is what a sorted map would give.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PyDict {
    entries: Vec<(String, PyValue)>,
}

impl PyDict {
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    /// `d[key] = value`, with CPython's first-position/last-value rule.
    pub fn insert(&mut self, key: String, value: PyValue) {
        match self.entries.iter_mut().find(|(k, _)| *k == key) {
            Some(slot) => slot.1 = value,
            None => self.entries.push((key, value)),
        }
    }

    pub fn get(&self, key: &str) -> Option<&PyValue> {
        self.entries.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The entries in insertion order.
    pub fn iter(&self) -> impl Iterator<Item = (&String, &PyValue)> {
        self.entries.iter().map(|(k, v)| (k, v))
    }

    /// The keys in insertion order — what CPython iterates a dict as.
    pub fn keys(&self) -> impl Iterator<Item = &String> {
        self.entries.iter().map(|(k, _)| k)
    }
}

impl PyValue {
    /// A CPython `int` from a machine integer.
    pub fn int<T: Into<PyInt>>(value: T) -> PyValue {
        PyValue::Int(value.into())
    }

    fn as_str(&self) -> Option<&str> {
        match self {
            PyValue::Str(s) => Some(s.as_str()),
            _ => None,
        }
    }

    fn is_none(&self) -> bool {
        matches!(self, PyValue::None)
    }
}

// ───────────────────────────────────────────────────────────────── decoding

/// Why this module declined to model an input.
///
/// `reason` is a STABLE machine-readable name and `detail` is prose. The
/// separation is the point: prose can be reworded, translated or made more
/// helpful without anything noticing, so an expectation that binds only to
/// "something was refused" is satisfied by any refusal whatsoever — including
/// one belonging to a completely different obligation. `reason` is what an
/// expectation binds to, and `fields` carries the few values that make the
/// refusal specific (the bound that was exceeded, the type that could not be
/// a key) so the expectation can be exact rather than approximate.
#[derive(Clone, Debug, PartialEq)]
pub struct Refusal {
    /// One of the `Refusal::` constants below. Never free text.
    pub reason: &'static str,
    /// The values that make this refusal specific, in a stable order.
    pub fields: PyDict,
    /// Human prose. Never load-bearing for an expectation.
    pub detail: String,
}

impl Refusal {
    /// The input nests deeper than [`MAX_NESTING_DEPTH`]. Field: `limit`.
    pub const NESTING_DEPTH_EXCEEDED: &'static str = "nesting-depth-exceeded";
    /// `repr()` of a non-ASCII string, whose escaping follows CPython's
    /// printability table. Field: `text`.
    pub const REPR_OF_NON_ASCII_STRING: &'static str = "repr-of-non-ascii-string";
    /// A Python mapping key that a JSON object cannot carry. Field:
    /// `key_type`.
    pub const NON_STRING_DICT_KEY: &'static str = "non-string-dict-key";
    /// A defect guard, not a modelling boundary: the integer `-0` rewrite
    /// turned a line that parsed into one that does not. No field.
    pub const REWRITE_BROKE_A_VALID_LINE: &'static str = "rewrite-broke-a-valid-line";

    fn new(reason: &'static str, fields: PyDict, detail: String) -> Self {
        Refusal {
            reason,
            fields,
            detail,
        }
    }
}

/// One-entry [`PyDict`], for a refusal that has a single relevant value.
fn one_field(key: &str, value: PyValue) -> PyDict {
    let mut fields = PyDict::new();
    fields.insert(key.to_string(), value);
    fields
}

/// Why a line did not decode.
#[derive(Clone, Debug, PartialEq)]
pub enum DecodeError {
    /// The parser rejected the input, and this module treats that as the
    /// source's own `except ValueError: continue`, so the line is skipped in
    /// silence.
    ///
    /// Read this arm precisely. It is NOT a proof that CPython's `json` would
    /// have rejected the same text: it carries every parser failure other
    /// than the depth bound below, and serde_json rejects some input CPython
    /// accepts. The three known ones — the bare tokens `NaN`/`Infinity`, a
    /// lone surrogate escape, and a literal whose exponent overflows such as
    /// `1e999` — are recorded as NAMED obligations in the shared profile and
    /// exercised from both sides, so they are visible rather than hidden.
    /// What this arm cannot do is promise that no FURTHER such input exists;
    /// only the ones written down are accounted for.
    ///
    /// It also carries one refusal that is PARITY rather than a divergence: an
    /// integer literal with more than [`MAX_INT_STR_DIGITS`] decimal digits.
    /// CPython's `int()` raises `ValueError` there, the source's
    /// `except ValueError: continue` skips the line, and this arm IS that
    /// skip. Naming it as a refusal would be wrong — it would make the two
    /// implementations look different on an input they answer identically.
    Malformed(String),
    /// CPython's `json` would have SUCCEEDED and this module declines to
    /// model the result. It must never reach the skip arm, because that would
    /// dress a divergence up as the source's own behaviour.
    Unrepresentable(Refusal),
}

/// `json.loads(text)` over the subset this module reproduces exactly.
///
/// Syntactic validity is decided by the ORIGINAL text, before the `-0`
/// rewrite below is even considered. That ordering is a guarantee rather than
/// an implementation detail: a line CPython's decoder would have rejected
/// cannot become a line this module decodes and acts on, whatever the
/// rewriting scanner does or gets wrong. The scanner carries its own guards
/// as well, and the two are deliberately independent of each other.
pub fn decode_line(text: &str) -> Result<PyValue, DecodeError> {
    let value = parse_json(text)?;
    let rewritten = match negative_zero_integer_rewrite(text) {
        None => return Ok(value),
        Some(rewritten) => rewritten,
    };
    // The rewrite exchanges one complete JSON number token for another inside
    // a document that has just parsed, so this parse cannot fail. If it ever
    // does, the scanner is wrong, and the answer has to be a NAMED refusal —
    // never the silent skip a `Malformed` would turn into.
    parse_json(&rewritten).map_err(|_| {
        DecodeError::Unrepresentable(Refusal::new(
            Refusal::REWRITE_BROKE_A_VALID_LINE,
            PyDict::new(),
            "the integer `-0` rewrite turned a line that parsed into one that does not; \
             that is a defect in this module, and it is refused by name rather than \
             skipped in silence"
                .to_string(),
        ))
    })
}

/// One parse of one exact string. No rewriting and no fallback.
fn parse_json(text: &str) -> Result<PyValue, DecodeError> {
    let too_deep = std::cell::Cell::new(false);
    let mut de = serde_json::Deserializer::from_str(text);
    // serde_json's own bound is 128 nested containers, which ordinary input
    // can cross; the bound this module answers for is MAX_NESTING_DEPTH, and
    // `PyValueSeed` enforces it, so the parser never recurses past it either.
    de.disable_recursion_limit();
    let seed = PyValueSeed {
        depth: 0,
        too_deep: &too_deep,
    };
    let decoded = seed
        .deserialize(&mut de)
        .and_then(|value| de.end().map(|()| value));
    match decoded {
        Ok(value) => Ok(value),
        Err(error) if too_deep.get() => Err(DecodeError::Unrepresentable(Refusal::new(
            Refusal::NESTING_DEPTH_EXCEEDED,
            one_field("limit", PyValue::int(MAX_NESTING_DEPTH)),
            format!(
                "the input nests more than {MAX_NESTING_DEPTH} containers deep; CPython's \
                 decoder accepts it and this module states a shallower bound rather than \
                 letting the difference pass as a parse failure ({error})"
            ),
        ))),
        Err(error) => Err(DecodeError::Malformed(error.to_string())),
    }
}

/// Exchange the complete JSON number token `-0` for `0`, outside strings.
///
/// CPython decodes `-0` with `int`, giving the `int` 0 — the very same object
/// `0` decodes to. serde_json special-cases the token and yields the `f64`
/// -0.0, whose `str()` is `-0.0`, which would change a dispatched tool name.
/// The two tokens denote one CPython object, so exchanging them is exact
/// rather than a coercion. `-0.0`, `-0e0` and every other float spelling are
/// left alone and still decode to -0.0, which is what CPython gives them too.
///
/// TWO GUARDS, and both are load-bearing.
///
/// * The `-` must sit where a JSON VALUE may begin: at the start of the
///   document, or after `[`, `,` or `:`, ignoring whitespace. Anywhere else a
///   `-` belongs to something already in progress — the exponent sign in
///   `1e-0`, or simply malformed text like `1-0` — and editing there strips a
///   byte out of the middle of a token.
/// * `-0` must also END the token: end of input, whitespace, `,`, `]` or `}`.
///   A digit, a `.`, an `e` or anything else after the zero means this was
///   never the token `-0` in the first place.
///
/// Without them, `{"x":1-0}` — which CPython's decoder REJECTS, so the source
/// skips the line in silence — became the perfectly valid `{"x":10}` and was
/// dispatched to a handler as real work. With them, the only thing ever
/// exchanged is one complete number token for another, which cannot change
/// whether the document parses at all.
///
/// `None` means there is nothing to do and the caller keeps the original text.
fn negative_zero_integer_rewrite(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    if !bytes.windows(2).any(|w| w == b"-0") {
        return None;
    }
    let is_space = |byte: u8| matches!(byte, b' ' | b'\t' | b'\n' | b'\r');
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut index = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    // The last non-whitespace byte seen OUTSIDE a string. `None` is the start
    // of the document.
    let mut previous: Option<u8> = None;
    let mut rewrote = false;
    while index < bytes.len() {
        let byte = bytes[index];
        if in_string {
            out.push(byte);
            index += 1;
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
                previous = Some(b'"');
            }
            continue;
        }
        if byte == b'"' {
            in_string = true;
            out.push(byte);
            index += 1;
            continue;
        }
        let starts_a_value = matches!(previous, None | Some(b'[') | Some(b',') | Some(b':'));
        let ends_the_token = match bytes.get(index + 2) {
            None => true,
            Some(&next) => is_space(next) || matches!(next, b',' | b']' | b'}'),
        };
        if byte == b'-' && bytes.get(index + 1) == Some(&b'0') && starts_a_value && ends_the_token {
            out.push(b'0');
            index += 2;
            previous = Some(b'0');
            rewrote = true;
            continue;
        }
        out.push(byte);
        index += 1;
        if !is_space(byte) {
            previous = Some(byte);
        }
    }
    if !rewrote {
        return None;
    }
    // Only an ASCII `-` was ever removed, so the result is still valid UTF-8.
    Some(String::from_utf8(out).expect("only an ASCII byte was dropped"))
}
/// The key serde_json hands a NUMBER through when `arbitrary_precision` is on.
///
/// That feature is how this crate sees a wide integer's exact digits. With it,
/// a literal that fits `i64`/`u64` still arrives at `visit_i64`/`visit_u64`
/// unchanged, and everything else — every integer wider than a machine word,
/// and every float — arrives as a ONE-ENTRY MAP under this key whose value is
/// the raw lexeme the parser just scanned. Nothing is rounded on the way,
/// because nothing has been converted yet.
///
/// A real JSON document may of course carry an object with that very key, and
/// it must not be mistaken for a number. THREE things have to hold together
/// before a map is read as one, and the first is the load-bearing one:
///
/// 1. the value arrives through `visit_string`, i.e. as an OWNED `String`.
///    serde_json's own parser never calls `visit_string` for a document
///    string: `StrRead` and `SliceRead` hand out a borrowed or copied `&str`
///    through `visit_borrowed_str`/`visit_str`, and `IoRead` calls
///    `visit_str`. Only the private `String` deserializer that carries the
///    number payload hands over ownership;
/// 2. the map holds exactly that one entry;
/// 3. the text is one complete JSON number token.
///
/// If any of them fails the entry is kept as an ordinary key with an ordinary
/// string value and the map is finished as the mapping it is — nothing is
/// lost, because the raw text is still in hand either way. The profile
/// exercises that path directly with a user object built to look like the
/// marker.
const SERDE_JSON_NUMBER_TOKEN: &str = "$serde_json::private::Number";

/// What one lexed JSON number token denotes.
enum NumberFromToken {
    /// A CPython `int`, exactly.
    Int(PyInt),
    /// A CPython `float`.
    Float(f64),
    /// A well-formed integer literal with more than [`MAX_INT_STR_DIGITS`]
    /// decimal digits. CPython raises `ValueError` there and the source skips
    /// the line, so this must reach the caller as an ordinary parse failure —
    /// the silent skip — and never as a value or as a named refusal.
    TooManyDigits(usize),
    /// A finite literal whose exponent overflows `f64`, such as `1e999`.
    /// CPython returns `inf`; this crate declines, and that difference is a
    /// declared obligation of its own rather than something to close here.
    OutOfRange,
    /// Not a JSON number token at all.
    NotANumber,
}

/// Is `token` one complete JSON number literal, and does it carry a fraction
/// or an exponent?
///
/// `Some(false)` is an integer, `Some(true)` a float, `None` not a number.
///
/// This is a VALIDATOR for text the parser has already lexed, not a second
/// parser: it reads no document, consumes no input and decides nothing about
/// syntax. It exists so a string arriving through the private number map can
/// be told apart from an ordinary object that happens to use the same key,
/// and so this crate does not depend on serde_json's internal spelling of the
/// lexeme it hands back.
fn classify_json_number(token: &str) -> Option<bool> {
    let bytes = token.as_bytes();
    let mut index = 0usize;
    let digit = |byte: Option<&u8>| matches!(byte, Some(b'0'..=b'9'));
    if bytes.first() == Some(&b'-') {
        index += 1;
    }
    match bytes.get(index) {
        // JSON allows exactly one leading zero, and only on its own.
        Some(b'0') => {
            index += 1;
            if digit(bytes.get(index)) {
                return None;
            }
        }
        Some(b'1'..=b'9') => {
            while digit(bytes.get(index)) {
                index += 1;
            }
        }
        _ => return None,
    }
    let mut is_float = false;
    if bytes.get(index) == Some(&b'.') {
        index += 1;
        let start = index;
        while digit(bytes.get(index)) {
            index += 1;
        }
        if index == start {
            return None;
        }
        is_float = true;
    }
    if matches!(bytes.get(index), Some(b'e') | Some(b'E')) {
        index += 1;
        if matches!(bytes.get(index), Some(b'+') | Some(b'-')) {
            index += 1;
        }
        let start = index;
        while digit(bytes.get(index)) {
            index += 1;
        }
        if index == start {
            return None;
        }
        is_float = true;
    }
    if index != bytes.len() {
        return None;
    }
    Some(is_float)
}

/// Turn one lexed JSON number token into the CPython value it denotes.
fn number_from_token(token: &str) -> NumberFromToken {
    let Some(is_float) = classify_json_number(token) else {
        return NumberFromToken::NotANumber;
    };
    if is_float {
        // Rust's `f64` parser is correctly rounded, as CPython's `float()` is,
        // so the two agree on every finite literal. An overflowing exponent
        // saturates to an infinity here; CPython returns that infinity and
        // this crate declines it, which is the standing obligation.
        return match token.parse::<f64>() {
            Ok(value) if value.is_finite() => NumberFromToken::Float(value),
            _ => NumberFromToken::OutOfRange,
        };
    }
    let digits = token.len() - usize::from(token.starts_with('-'));
    if digits > MAX_INT_STR_DIGITS {
        return NumberFromToken::TooManyDigits(digits);
    }
    // JSON forbids a leading zero, so `-0` is the one spelling that is not
    // already canonical — and CPython decodes it to the `int` 0.
    let canonical = if token == "-0" { "0" } else { token };
    NumberFromToken::Int(PyInt::from_canonical(canonical.to_string()))
}

/// One value that may turn out to be serde_json's private number payload.
enum MaybeNumber {
    /// An OWNED string. serde_json's JSON parser never produces one for a
    /// document string, so this came from the private number deserializer.
    OwnedText(String),
    /// An ordinary decoded value.
    Value(PyValue),
}

/// The seed used for the value under a key that spells
/// [`SERDE_JSON_NUMBER_TOKEN`]. Everything except an owned string is decoded
/// exactly as any other value would be, one nesting level down, so an ordinary
/// object using that key is still bounded by [`MAX_NESTING_DEPTH`].
struct MaybeNumberSeed<'a> {
    inner: PyValueSeed<'a>,
}

impl<'de> DeserializeSeed<'de> for MaybeNumberSeed<'_> {
    type Value = MaybeNumber;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<MaybeNumber, D::Error> {
        deserializer.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for MaybeNumberSeed<'_> {
    type Value = MaybeNumber;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("a JSON value, or serde_json's private number payload")
    }

    /// THE discriminator. See [`SERDE_JSON_NUMBER_TOKEN`].
    fn visit_string<E: serde_core::de::Error>(self, value: String) -> Result<MaybeNumber, E> {
        Ok(MaybeNumber::OwnedText(value))
    }

    fn visit_unit<E: serde_core::de::Error>(self) -> Result<MaybeNumber, E> {
        self.inner.visit_unit().map(MaybeNumber::Value)
    }

    fn visit_none<E: serde_core::de::Error>(self) -> Result<MaybeNumber, E> {
        self.inner.visit_none().map(MaybeNumber::Value)
    }

    fn visit_bool<E: serde_core::de::Error>(self, value: bool) -> Result<MaybeNumber, E> {
        self.inner.visit_bool(value).map(MaybeNumber::Value)
    }

    fn visit_i64<E: serde_core::de::Error>(self, value: i64) -> Result<MaybeNumber, E> {
        self.inner.visit_i64(value).map(MaybeNumber::Value)
    }

    fn visit_u64<E: serde_core::de::Error>(self, value: u64) -> Result<MaybeNumber, E> {
        self.inner.visit_u64(value).map(MaybeNumber::Value)
    }

    fn visit_f64<E: serde_core::de::Error>(self, value: f64) -> Result<MaybeNumber, E> {
        self.inner.visit_f64(value).map(MaybeNumber::Value)
    }

    fn visit_str<E: serde_core::de::Error>(self, value: &str) -> Result<MaybeNumber, E> {
        self.inner.visit_str(value).map(MaybeNumber::Value)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, access: A) -> Result<MaybeNumber, A::Error> {
        self.inner.visit_seq(access).map(MaybeNumber::Value)
    }

    fn visit_map<A: MapAccess<'de>>(self, access: A) -> Result<MaybeNumber, A::Error> {
        self.inner.visit_map(access).map(MaybeNumber::Value)
    }
}

#[derive(Clone, Copy)]
struct PyValueSeed<'a> {
    depth: usize,
    too_deep: &'a std::cell::Cell<bool>,
}

impl<'a> PyValueSeed<'a> {
    fn inner(&self) -> PyValueSeed<'a> {
        PyValueSeed {
            depth: self.depth + 1,
            too_deep: self.too_deep,
        }
    }

    fn too_deep<E: serde_core::de::Error>(&self) -> E {
        self.too_deep.set(true);
        E::custom(format!("more than {MAX_NESTING_DEPTH} nested containers"))
    }
}

impl<'de> DeserializeSeed<'de> for PyValueSeed<'_> {
    type Value = PyValue;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<PyValue, D::Error> {
        deserializer.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for PyValueSeed<'_> {
    type Value = PyValue;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("a JSON document")
    }

    fn visit_unit<E>(self) -> Result<PyValue, E> {
        Ok(PyValue::None)
    }

    fn visit_none<E>(self) -> Result<PyValue, E> {
        Ok(PyValue::None)
    }

    fn visit_bool<E>(self, value: bool) -> Result<PyValue, E> {
        Ok(PyValue::Bool(value))
    }

    fn visit_i64<E>(self, value: i64) -> Result<PyValue, E> {
        Ok(PyValue::int(value))
    }

    fn visit_u64<E>(self, value: u64) -> Result<PyValue, E> {
        Ok(PyValue::int(value))
    }

    fn visit_f64<E>(self, value: f64) -> Result<PyValue, E> {
        Ok(PyValue::Float(value))
    }

    fn visit_str<E>(self, value: &str) -> Result<PyValue, E> {
        Ok(PyValue::Str(value.to_string()))
    }

    fn visit_string<E>(self, value: String) -> Result<PyValue, E> {
        Ok(PyValue::Str(value))
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut access: A) -> Result<PyValue, A::Error> {
        if self.depth >= MAX_NESTING_DEPTH {
            return Err(self.too_deep());
        }
        let mut items = Vec::new();
        while let Some(item) = access.next_element_seed(self.inner())? {
            items.push(item);
        }
        Ok(PyValue::List(items))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut access: A) -> Result<PyValue, A::Error> {
        // The FIRST key is read BEFORE the depth test, and the ordering is
        // load-bearing. With serde_json's `arbitrary_precision` a NUMBER
        // arrives as a one-entry map (see [`SERDE_JSON_NUMBER_TOKEN`]), and a
        // number is a SCALAR: charging it a nesting level would refuse a wide
        // integer sitting at the bound as though it were a container, which is
        // a depth-bound answer given to an input that never reached the bound.
        let first_key = access.next_key::<String>()?;
        let mut first_value: Option<PyValue> = None;
        let mut second_key: Option<String> = None;

        if first_key.as_deref() == Some(SERDE_JSON_NUMBER_TOKEN) {
            // The seed sits one level down, so a genuine container under this
            // key is still bounded exactly as it would be under any other.
            match access.next_value_seed(MaybeNumberSeed {
                inner: self.inner(),
            })? {
                MaybeNumber::Value(value) => first_value = Some(value),
                MaybeNumber::OwnedText(raw) => {
                    second_key = access.next_key::<String>()?;
                    if second_key.is_none() {
                        match number_from_token(&raw) {
                            NumberFromToken::Int(value) => return Ok(PyValue::Int(value)),
                            NumberFromToken::Float(value) => return Ok(PyValue::Float(value)),
                            NumberFromToken::TooManyDigits(digits) => {
                                return Err(serde_core::de::Error::custom(format!(
                                    "integer literal has {digits} decimal digits; the pinned \
                                     CPython decoder raises ValueError past \
                                     {MAX_INT_STR_DIGITS}, and the source skips such a line"
                                )))
                            }
                            NumberFromToken::OutOfRange => {
                                return Err(serde_core::de::Error::custom(format!(
                                    "number out of range: the literal {raw} has no finite \
                                     f64 value"
                                )))
                            }
                            // Not the parser's payload after all: an ordinary
                            // object whose first value is an owned string.
                            NumberFromToken::NotANumber => {}
                        }
                    }
                    first_value = Some(PyValue::Str(raw));
                }
            }
        }

        // Everything from here down is an ordinary mapping, and a mapping
        // answers for its own nesting level.
        if self.depth >= MAX_NESTING_DEPTH {
            return Err(self.too_deep());
        }
        let mut dict = PyDict::new();
        if let Some(key) = first_key {
            let value = match first_value {
                Some(value) => value,
                None => access.next_value_seed(self.inner())?,
            };
            dict.insert(key, value);
        }
        if let Some(key) = second_key {
            let value = access.next_value_seed(self.inner())?;
            dict.insert(key, value);
        }
        while let Some(key) = access.next_key::<String>()? {
            let value = access.next_value_seed(self.inner())?;
            dict.insert(key, value);
        }
        Ok(PyValue::Dict(dict))
    }
}

// ───────────────────────────────────────────────────────────────── the cards

fn tools_value() -> &'static PyValue {
    static TOOLS: OnceLock<PyValue> = OnceLock::new();
    TOOLS.get_or_init(|| decode_line(TOOLS_JSON).expect("embedded tool cards must be valid JSON"))
}

/// The eight tool cards as a decoded list, in the source's declaration order.
pub fn tools() -> &'static PyValue {
    tools_value()
}

/// The tool names in declaration order.
pub fn tool_names() -> Vec<&'static str> {
    let PyValue::List(cards) = tools_value() else {
        unreachable!("the embedded tool cards are a JSON array")
    };
    cards
        .iter()
        .map(|card| match card {
            PyValue::Dict(fields) => fields
                .get("name")
                .and_then(PyValue::as_str)
                .expect("every card names a tool"),
            _ => unreachable!("every tool card is a JSON object"),
        })
        .collect()
}

// ───────────────────────────────────────────────────────── the injected edge

/// What the injected dispatcher did. The source's `dispatch` returns a string
/// and may raise; those are the only three shapes `serve()` distinguishes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DispatchOutcome {
    /// `dispatch` returned this exact string.
    Text(String),
    /// `dispatch` raised `urllib.error.URLError`; the field is `e.reason`
    /// stringified, which is what the source interpolates.
    UrlError { reason: String },
    /// `dispatch` raised any other `Exception`; the field is `str(e)`.
    Exception { message: String },
}

/// One recorded call into the injected dispatcher.
#[derive(Clone, Debug, PartialEq)]
pub struct Call {
    /// The forwarded tool name, after the source's `str(...)` coercion.
    pub tool: String,
    /// The forwarded arguments, after the source's `dict(... or {})`
    /// coercion, in the order CPython would have built them.
    pub arguments: PyDict,
}

/// The synthetic handler boundary. No real handler is ever reachable from this
/// crate: the only way a tool call produces anything is through this trait.
pub trait Dispatcher {
    fn dispatch(&mut self, tool: &str, arguments: &PyDict) -> DispatchOutcome;
}

/// A dispatcher that replays a queue of prepared outcomes and records what it
/// was asked. Test-only; it holds no handle to anything outside itself.
#[derive(Debug, Default)]
pub struct ScriptedDispatcher {
    queue: std::collections::VecDeque<DispatchOutcome>,
    /// Every call the run made, in order.
    pub calls: Vec<Call>,
    /// Calls that arrived after the queue was empty — a harness fault, never
    /// a product behaviour, so it is counted rather than papered over.
    pub overruns: usize,
}

impl ScriptedDispatcher {
    pub fn new<I: IntoIterator<Item = DispatchOutcome>>(outcomes: I) -> Self {
        Self {
            queue: outcomes.into_iter().collect(),
            calls: Vec::new(),
            overruns: 0,
        }
    }

    /// Outcomes that were prepared and never consumed.
    pub fn unused(&self) -> usize {
        self.queue.len()
    }
}

impl Dispatcher for ScriptedDispatcher {
    fn dispatch(&mut self, tool: &str, arguments: &PyDict) -> DispatchOutcome {
        self.calls.push(Call {
            tool: tool.to_string(),
            arguments: arguments.clone(),
        });
        match self.queue.pop_front() {
            Some(outcome) => outcome,
            None => {
                self.overruns += 1;
                DispatchOutcome::Exception {
                    message: "scripted dispatcher exhausted".to_string(),
                }
            }
        }
    }
}

// ─────────────────────────────────────────────────────────────── the outcomes

/// What one input line produced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LineOutcome {
    /// Blank after stripping, or not parseable as JSON: `continue`, no reply.
    Skipped,
    /// Parsed, recognised as an unanswerable frame: no reply, loop continues.
    Silent,
    /// One reply frame, without its terminating newline.
    Reply(String),
    /// `serve()` raised and the loop ended here.
    Terminal { kind: String, message: String },
}

/// How a whole run ended.
#[derive(Clone, Debug, PartialEq)]
pub enum RunOutcome {
    /// Every line was consumed and `serve()` returned normally.
    Completed,
    /// `serve()` raised; `lines_unprocessed` of the input never ran.
    Terminated { kind: String, message: String },
    /// This implementation cannot model the pinned source for that input, and
    /// says so instead of guessing. A declared obligation, never a pass.
    ///
    /// The whole [`Refusal`] travels, not just its prose, so an expectation
    /// can bind to WHICH refusal happened rather than to the bare fact that
    /// one did.
    Unrepresentable { line_index: usize, refusal: Refusal },
}

/// The full record of one run.
#[derive(Clone, Debug, PartialEq)]
pub struct RunReport {
    /// Reply frames in emission order, each without its newline.
    pub frames: Vec<String>,
    /// Exactly what would have been written to the stream: every frame
    /// followed by one `\n`, and nothing else.
    pub raw: String,
    /// Calls that reached the injected dispatcher, in order.
    pub calls: Vec<Call>,
    pub outcome: RunOutcome,
    pub lines_total: usize,
    pub lines_processed: usize,
    pub lines_unprocessed: usize,
}

// ──────────────────────────────────────────────────── Python string handling

/// Every code point for which CPython 3.13's `str.isspace()` is true, which is
/// exactly the set `str.strip()` removes. Enumerated from the pinned test
/// runtime (CPython 3.13.15) rather than assumed: Rust's `char::is_whitespace`
/// follows the Unicode `White_Space` property and therefore does NOT include
/// U+001C..U+001F, which Python strips.
const PY_WHITESPACE: [char; 29] = [
    '\u{9}', '\u{a}', '\u{b}', '\u{c}', '\u{d}', '\u{1c}', '\u{1d}', '\u{1e}', '\u{1f}', '\u{20}',
    '\u{85}', '\u{a0}', '\u{1680}', '\u{2000}', '\u{2001}', '\u{2002}', '\u{2003}', '\u{2004}',
    '\u{2005}', '\u{2006}', '\u{2007}', '\u{2008}', '\u{2009}', '\u{200a}', '\u{2028}', '\u{2029}',
    '\u{202f}', '\u{205f}', '\u{3000}',
];

fn is_py_space(c: char) -> bool {
    PY_WHITESPACE.contains(&c)
}

/// `str.strip()` with no argument, as CPython 3.13 performs it.
pub fn python_strip(s: &str) -> &str {
    s.trim_matches(is_py_space)
}

/// Split text the way an iterated `sys.stdin` does: `\n`, `\r\n` and a lone
/// `\r` all end a line (universal newlines), and a final fragment with no
/// terminator is still a line. The terminator itself is not returned, because
/// the source strips it away immediately.
pub fn universal_lines(input: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let bytes = input.as_bytes();
    let mut start = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'\n' => {
                out.push(&input[start..i]);
                i += 1;
                start = i;
            }
            b'\r' => {
                out.push(&input[start..i]);
                i += if i + 1 < bytes.len() && bytes[i + 1] == b'\n' {
                    2
                } else {
                    1
                };
                start = i;
            }
            _ => i += 1,
        }
    }
    if start < bytes.len() {
        out.push(&input[start..]);
    }
    out
}

/// `json.dumps(value)`, with CPython's defaults: `", "`/`": "` separators,
/// `ensure_ascii=True`, and mappings written in their own order rather than
/// re-sorted.
pub fn py_dumps(value: &PyValue) -> String {
    let mut out = String::new();
    py_dumps_into(value, &mut out);
    out
}

fn py_dumps_into(value: &PyValue, out: &mut String) {
    match value {
        PyValue::None => out.push_str("null"),
        PyValue::Bool(true) => out.push_str("true"),
        PyValue::Bool(false) => out.push_str("false"),
        PyValue::Int(i) => out.push_str(i.as_decimal()),
        PyValue::Float(f) => out.push_str(&py_dumps_float(*f)),
        PyValue::Str(s) => py_json_string(s, out),
        PyValue::List(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push_str(", ");
                }
                py_dumps_into(item, out);
            }
            out.push(']');
        }
        PyValue::Dict(map) => {
            out.push('{');
            for (index, (key, item)) in map.iter().enumerate() {
                if index > 0 {
                    out.push_str(", ");
                }
                py_json_string(key, out);
                out.push_str(": ");
                py_dumps_into(item, out);
            }
            out.push('}');
        }
    }
}

/// `json.dumps` of a float: `repr()`, except that the three non-finite values
/// are written as the bare tokens CPython emits by default.
fn py_dumps_float(x: f64) -> String {
    if x.is_nan() {
        return "NaN".to_string();
    }
    if x.is_infinite() {
        return if x < 0.0 { "-Infinity" } else { "Infinity" }.to_string();
    }
    python_float_repr(x)
}

/// `json.dumps(value)` for a one-key error object, matching CPython's
/// defaults: `", "`/`": "` separators and `ensure_ascii=True`.
fn py_dumps_error(message: &str) -> String {
    let mut out = String::from("{\"error\": ");
    py_json_string(message, &mut out);
    out.push('}');
    out
}

/// CPython `json`'s ASCII string encoder.
fn py_json_string(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 || (c as u32) > 0x7e => {
                let cp = c as u32;
                if cp > 0xffff {
                    let v = cp - 0x10000;
                    out.push_str(&format!("\\u{:04x}", 0xd800 + (v >> 10)));
                    out.push_str(&format!("\\u{:04x}", 0xdc00 + (v & 0x3ff)));
                } else {
                    out.push_str(&format!("\\u{:04x}", cp));
                }
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// CPython's `repr()` of a finite float, which is also its `str()`.
fn python_float_repr(x: f64) -> String {
    if x.is_nan() {
        return "nan".to_string();
    }
    if x.is_infinite() {
        return if x < 0.0 { "-inf" } else { "inf" }.to_string();
    }
    // `{:e}` is Rust's shortest round-tripping form, which is the same digit
    // string CPython's repr uses; only the layout rules differ.
    let formatted = format!("{:e}", x);
    let (mantissa, exponent) = formatted
        .split_once('e')
        .expect("Rust's LowerExp always emits an exponent");
    let exponent: i32 = exponent.parse().expect("Rust emits a decimal exponent");
    let negative = mantissa.starts_with('-');
    let digits: String = mantissa.chars().filter(|c| c.is_ascii_digit()).collect();
    // value == 0.<digits> * 10^decpt
    let decpt = exponent + 1;
    let mut body = String::new();
    if decpt <= -4 || decpt > 16 {
        body.push_str(&digits[..1]);
        if digits.len() > 1 {
            body.push('.');
            body.push_str(&digits[1..]);
        }
        let e = decpt - 1;
        body.push_str(&format!("e{}{:02}", if e < 0 { '-' } else { '+' }, e.abs()));
    } else if decpt <= 0 {
        body.push_str("0.");
        for _ in 0..(-decpt) {
            body.push('0');
        }
        body.push_str(&digits);
    } else if decpt as usize >= digits.len() {
        body.push_str(&digits);
        for _ in 0..(decpt as usize - digits.len()) {
            body.push('0');
        }
        body.push_str(".0");
    } else {
        body.push_str(&digits[..decpt as usize]);
        body.push('.');
        body.push_str(&digits[decpt as usize..]);
    }
    if negative {
        format!("-{body}")
    } else {
        body
    }
}

/// `str(value)` for a value that came out of `json.loads`.
///
/// `Err` means this crate will not guess: the source would produce a CPython
/// `repr` this implementation does not reproduce, and the caller must surface
/// that as an unimplemented obligation instead of inventing an answer.
pub fn python_str(value: &PyValue) -> Result<String, Refusal> {
    match value {
        PyValue::Str(s) => Ok(s.clone()),
        other => python_repr(other),
    }
}

/// `repr(value)` for a value that came out of `json.loads`, over the subset
/// this crate reproduces exactly.
pub fn python_repr(value: &PyValue) -> Result<String, Refusal> {
    match value {
        PyValue::None => Ok("None".to_string()),
        PyValue::Bool(true) => Ok("True".to_string()),
        PyValue::Bool(false) => Ok("False".to_string()),
        PyValue::Int(i) => Ok(i.as_decimal().to_string()),
        PyValue::Float(f) => Ok(python_float_repr(*f)),
        PyValue::Str(s) => python_string_repr(s),
        PyValue::List(items) => {
            let mut parts = Vec::with_capacity(items.len());
            for item in items {
                parts.push(python_repr(item)?);
            }
            Ok(format!("[{}]", parts.join(", ")))
        }
        PyValue::Dict(map) => {
            if map.is_empty() {
                return Ok("{}".to_string());
            }
            let mut parts = Vec::with_capacity(map.len());
            for (key, item) in map.iter() {
                parts.push(format!(
                    "{}: {}",
                    python_string_repr(key)?,
                    python_repr(item)?
                ));
            }
            Ok(format!("{{{}}}", parts.join(", ")))
        }
    }
}

/// CPython's `repr()` of a str, restricted to ASCII.
///
/// Deliberately refuses non-ASCII rather than approximating it: Python decides
/// whether to escape a character by its Unicode category, and this crate does
/// not carry a category table. A non-ASCII string is still fully supported
/// wherever `str()` is what the source calls — only `repr()`, reached solely
/// through a container, is bounded here.
fn python_string_repr(s: &str) -> Result<String, Refusal> {
    if !s.is_ascii() {
        return Err(Refusal::new(
            Refusal::REPR_OF_NON_ASCII_STRING,
            one_field("text", PyValue::Str(s.to_string())),
            format!(
                "repr() of the non-ASCII string {s:?} depends on CPython's \
                 printability table, which this crate does not reproduce"
            ),
        ));
    }
    let quote = if s.contains('\'') && !s.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::new();
    out.push(quote);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c if (c as u32) < 0x20 || (c as u32) == 0x7f => {
                out.push_str(&format!("\\x{:02x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push(quote);
    Ok(out)
}

/// Why `dict(...)` did not produce a mapping.
#[derive(Clone, Debug, PartialEq)]
enum DictFailure {
    /// A CPython exception the source's `except Exception` arm catches. The
    /// field is `str(e)`, which is the whole of what reaches the wire.
    Caught(String),
    /// An input this crate declines to model rather than guess at.
    Unrepresentable(Refusal),
}

/// Python truthiness for a value that came out of `json.loads`.
fn python_truthy(value: &PyValue) -> bool {
    match value {
        PyValue::None => false,
        PyValue::Bool(b) => *b,
        PyValue::Int(i) => !i.is_zero(),
        // NaN is truthy in Python; only a zero magnitude is false, and -0.0
        // compares equal to 0.0.
        PyValue::Float(f) => f.is_nan() || *f != 0.0,
        PyValue::Str(s) => !s.is_empty(),
        PyValue::List(items) => !items.is_empty(),
        PyValue::Dict(map) => !map.is_empty(),
    }
}

/// CPython's `type(value).__name__` for a value that came out of
/// `json.loads`.
///
/// Public because the TYPE is half of what exact-integer work has to observe:
/// an implementation that answers a wide integer as a `float` of the same
/// magnitude matches on value under any comparator that treats JSON's single
/// number type as one type, and differs here.
pub fn py_type_of(value: &PyValue) -> &'static str {
    py_type_name(value)
}

fn py_type_name(value: &PyValue) -> &'static str {
    match value {
        PyValue::None => "NoneType",
        PyValue::Bool(_) => "bool",
        PyValue::Int(_) => "int",
        PyValue::Float(_) => "float",
        PyValue::Str(_) => "str",
        PyValue::List(_) => "list",
        PyValue::Dict(_) => "dict",
    }
}

/// `dict(value)` over the subset this crate reproduces exactly.
fn python_dict(value: &PyValue) -> Result<PyDict, DictFailure> {
    match value {
        PyValue::Dict(map) => Ok(map.clone()),
        PyValue::Str(_) | PyValue::List(_) => {
            let elements: Vec<PyValue> = match value {
                PyValue::Str(s) => s.chars().map(|c| PyValue::Str(c.to_string())).collect(),
                PyValue::List(items) => items.clone(),
                _ => unreachable!(),
            };
            let mut out = PyDict::new();
            for (index, element) in elements.iter().enumerate() {
                let pair: Vec<PyValue> = match element {
                    PyValue::Str(s) => s.chars().map(|c| PyValue::Str(c.to_string())).collect(),
                    PyValue::List(items) => items.clone(),
                    // A dict iterates over its KEYS, in insertion order, so
                    // `dict([{"z": 1, "a": 2}])` really does yield
                    // `{'z': 'a'}` — the FIRST key is the key and the SECOND
                    // is the value.
                    PyValue::Dict(map) => map.keys().map(|k| PyValue::Str(k.clone())).collect(),
                    // A number, a bool or None is not a sequence at all.
                    _ => {
                        // TypeError
                        return Err(DictFailure::Caught(format!(
                            "cannot convert dictionary update sequence element #{index} \
                             to a sequence"
                        )));
                    }
                };
                if pair.len() != 2 {
                    // ValueError
                    return Err(DictFailure::Caught(format!(
                        "dictionary update sequence element #{index} has length {}; \
                         2 is required",
                        pair.len()
                    )));
                }
                match &pair[0] {
                    PyValue::Str(key) => {
                        out.insert(key.clone(), pair[1].clone());
                    }
                    PyValue::List(_) | PyValue::Dict(_) => {
                        // TypeError
                        return Err(DictFailure::Caught(format!(
                            "unhashable type: '{}'",
                            py_type_name(&pair[0])
                        )));
                    }
                    other => {
                        return Err(DictFailure::Unrepresentable(Refusal::new(
                            Refusal::NON_STRING_DICT_KEY,
                            one_field("key_type", PyValue::Str(py_type_name(other).to_string())),
                            format!(
                                "a {} dictionary key is legal in Python but cannot be \
                                 carried by a JSON object, so this crate refuses it \
                                 rather than coercing it",
                                py_type_name(other)
                            ),
                        )))
                    }
                }
            }
            Ok(out)
        }
        // TypeError
        other => Err(DictFailure::Caught(format!(
            "'{}' object is not iterable",
            py_type_name(other)
        ))),
    }
}

// ────────────────────────────────────────────────────────────── the envelope

fn reply_frame(id: &PyValue, result: PyValue) -> String {
    // `json.dumps({"jsonrpc": "2.0", "id": id_, "result": result})`, written
    // by this module's own CPython-faithful encoder rather than by
    // serde_json, so that an object echoed back inside `id` keeps the key
    // order the source would have written.
    let mut envelope = PyDict::new();
    envelope.insert("jsonrpc".to_string(), PyValue::Str("2.0".to_string()));
    envelope.insert("id".to_string(), id.clone());
    envelope.insert("result".to_string(), result);
    py_dumps(&PyValue::Dict(envelope))
}

fn initialize_result() -> PyValue {
    let mut server_info = PyDict::new();
    server_info.insert("name".to_string(), PyValue::Str(SERVER_NAME.to_string()));
    server_info.insert(
        "version".to_string(),
        PyValue::Str(SERVER_VERSION.to_string()),
    );
    let mut capabilities = PyDict::new();
    capabilities.insert("tools".to_string(), PyValue::Dict(PyDict::new()));
    let mut result = PyDict::new();
    result.insert(
        "protocolVersion".to_string(),
        PyValue::Str(PROTOCOL_VERSION.to_string()),
    );
    result.insert("capabilities".to_string(), PyValue::Dict(capabilities));
    result.insert("serverInfo".to_string(), PyValue::Dict(server_info));
    PyValue::Dict(result)
}

fn content_result(text: String) -> PyValue {
    let mut item = PyDict::new();
    item.insert("type".to_string(), PyValue::Str("text".to_string()));
    item.insert("text".to_string(), PyValue::Str(text));
    let mut result = PyDict::new();
    result.insert(
        "content".to_string(),
        PyValue::List(vec![PyValue::Dict(item)]),
    );
    PyValue::Dict(result)
}

/// Process one already-received line exactly as the pinned `serve()` loop
/// would. `Err` is an input this crate declines to model.
pub fn process_line<D: Dispatcher + ?Sized>(
    line: &str,
    dispatcher: &mut D,
) -> Result<LineOutcome, Refusal> {
    let stripped = python_strip(line);
    if stripped.is_empty() {
        return Ok(LineOutcome::Skipped);
    }
    let message: PyValue = match decode_line(stripped) {
        Ok(value) => value,
        // `except ValueError: continue`. Note the standing obligation: CPython
        // also accepts the bare tokens NaN/Infinity/-Infinity and integers
        // wider than 64 bits, which serde_json rejects or narrows. The profile
        // carries those as declared unimplemented obligations rather than
        // letting this arm absorb them.
        Err(DecodeError::Malformed(_)) => return Ok(LineOutcome::Skipped),
        // A document CPython WOULD have decoded. Refusing it out loud is the
        // whole point of the distinction: routing it to the skip arm would
        // make a divergence look like the source's own silence.
        Err(DecodeError::Unrepresentable(refusal)) => return Err(refusal),
    };
    let object = match &message {
        PyValue::Dict(map) => map,
        other => {
            // `msg.get(...)` on a non-dict raises AttributeError, and nothing
            // in serve() catches it: the loop ends here.
            return Ok(LineOutcome::Terminal {
                kind: "AttributeError".to_string(),
                message: format!("'{}' object has no attribute 'get'", py_type_name(other)),
            });
        }
    };
    let method = object.get("method");
    let id = object.get("id").cloned().unwrap_or(PyValue::None);
    let method_name = method.and_then(PyValue::as_str);

    match method_name {
        Some("initialize") => Ok(LineOutcome::Reply(reply_frame(&id, initialize_result()))),
        Some("tools/list") => {
            let mut result = PyDict::new();
            result.insert("tools".to_string(), tools_value().clone());
            Ok(LineOutcome::Reply(reply_frame(&id, PyValue::Dict(result))))
        }
        Some("tools/call") => {
            let params = object.get("params").cloned().unwrap_or(PyValue::None);
            let params = if python_truthy(&params) {
                params
            } else {
                PyValue::Dict(PyDict::new())
            };
            let text = call_text(&params, dispatcher)?;
            Ok(LineOutcome::Reply(reply_frame(&id, content_result(text))))
        }
        // Every other method, including a non-string one: answered with an
        // empty result only when the id is neither missing nor null.
        _ => {
            if id.is_none() {
                Ok(LineOutcome::Silent)
            } else {
                Ok(LineOutcome::Reply(reply_frame(
                    &id,
                    PyValue::Dict(PyDict::new()),
                )))
            }
        }
    }
}

/// The body of the source's `try:` around `dispatch`, including the two
/// `except` arms. `Err` is an input this crate declines to model.
fn call_text<D: Dispatcher + ?Sized>(
    params: &PyValue,
    dispatcher: &mut D,
) -> Result<String, Refusal> {
    // `p.get("name")` — an AttributeError here IS inside the try, so it
    // becomes an error string rather than ending the loop.
    let params = match params {
        PyValue::Dict(map) => map,
        other => {
            return Ok(py_dumps_error(&format!(
                "'{}' object has no attribute 'get'",
                py_type_name(other)
            )))
        }
    };
    let name = params.get("name").cloned().unwrap_or(PyValue::None);
    let tool = python_str(&name)?;
    let raw_arguments = params.get("arguments").cloned().unwrap_or(PyValue::None);
    let arguments = if python_truthy(&raw_arguments) {
        match python_dict(&raw_arguments) {
            Ok(map) => map,
            Err(DictFailure::Caught(message)) => return Ok(py_dumps_error(&message)),
            Err(DictFailure::Unrepresentable(refusal)) => return Err(refusal),
        }
    } else {
        PyDict::new()
    };
    Ok(match dispatcher.dispatch(&tool, &arguments) {
        DispatchOutcome::Text(text) => text,
        DispatchOutcome::UrlError { reason } => {
            py_dumps_error(&format!("hub unreachable: {reason}"))
        }
        DispatchOutcome::Exception { message } => py_dumps_error(&message),
    })
}

/// Run a whole stream of already-received text through the envelope.
pub fn run<D: Dispatcher + ?Sized>(input: &str, dispatcher: &mut D) -> RunReport {
    let lines = universal_lines(input);
    let lines_total = lines.len();
    let mut frames: Vec<String> = Vec::new();
    let mut raw = String::new();
    let mut outcome = RunOutcome::Completed;
    let mut processed = 0usize;

    for (index, line) in lines.iter().enumerate() {
        match process_line(line, dispatcher) {
            Ok(LineOutcome::Skipped) | Ok(LineOutcome::Silent) => processed += 1,
            Ok(LineOutcome::Reply(frame)) => {
                processed += 1;
                raw.push_str(&frame);
                raw.push('\n');
                frames.push(frame);
            }
            Ok(LineOutcome::Terminal { kind, message }) => {
                processed += 1;
                outcome = RunOutcome::Terminated { kind, message };
                break;
            }
            Err(refusal) => {
                outcome = RunOutcome::Unrepresentable {
                    line_index: index,
                    refusal,
                };
                break;
            }
        }
    }

    RunReport {
        frames,
        raw,
        calls: Vec::new(),
        outcome,
        lines_total,
        lines_processed: processed,
        lines_unprocessed: lines_total - processed,
    }
}

/// Run a stream against a [`ScriptedDispatcher`] and fold its recorded calls
/// into the report. This is the entry point the test driver uses.
pub fn run_scripted(
    input: &str,
    outcomes: Vec<DispatchOutcome>,
) -> (RunReport, ScriptedDispatcher) {
    let mut dispatcher = ScriptedDispatcher::new(outcomes);
    let mut report = run(input, &mut dispatcher);
    report.calls = dispatcher.calls.clone();
    (report, dispatcher)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `-0` scanner, tested DIRECTLY rather than through the decoder.
    ///
    /// Going through `decode_line` cannot isolate this: the decoder parses the
    /// original text first, so a scanner that fires where it should not is
    /// masked by that earlier guard and the two would test as one. They are
    /// meant to be independent, and this is where each of the scanner's own
    /// two conditions is held to account on its own.
    #[test]
    fn the_rewrite_fires_only_on_a_complete_negative_zero_token() {
        // Fires: a whole `-0` token, in every position a value may begin.
        for (text, rewritten) in [
            ("-0", "0"),
            ("  -0  ", "  0  "),
            ("[-0]", "[0]"),
            ("[-0,1]", "[0,1]"),
            ("[1,-0]", "[1,0]"),
            ("{\"a\":-0}", "{\"a\":0}"),
            ("[ -0 ]", "[ 0 ]"),
            ("[-0,-0]", "[0,0]"),
        ] {
            assert_eq!(
                negative_zero_integer_rewrite(text).as_deref(),
                Some(rewritten),
                "{text}"
            );
        }

        // Does not fire because the `-` does not begin a value. Every one of
        // these is MALFORMED, and rewriting would hand the parser a valid
        // document the source's own decoder would have thrown away —
        // `{"x":1-0}` became `{"x":10}` and was dispatched to a handler.
        for text in [
            "{\"x\":1-0}",
            "{\"x\":1.2-0}",
            "{\"x\":1e2-0}",
            "[1]-0",
            "{\"a\":1 -0}",
            "[1,2-0]",
            "-0-0",
            "[--0]",
            "{}-0",
            "true-0",
        ] {
            assert_eq!(negative_zero_integer_rewrite(text), None, "{text}");
        }

        // Does not fire because `-0` is not where the token ENDS. These are
        // valid documents whose value would change: `-0.0` is the negative
        // float and `0.0` is not.
        for text in ["-0.0", "-0e0", "-0E0", "[-0.5]", "-00", "-0123"] {
            assert_eq!(negative_zero_integer_rewrite(text), None, "{text}");
        }

        // A `-0` inside a string is data, and an exponent sign is not a value
        // position, so neither is touched.
        for text in ["\"x-0y\"", "{\"a-0\":1}", "1e-0", "[1e-0]", "\"[-0]\""] {
            assert_eq!(negative_zero_integer_rewrite(text), None, "{text}");
        }
    }

    /// The decoder's own guard, independent of the scanner above.
    #[test]
    fn syntactic_validity_is_decided_by_the_original_text() {
        // Whatever the scanner does, a document that does not parse as it
        // stands never reaches the rewritten parse at all.
        for text in ["{\"x\":1-0}", "[1]-0", "{\"a\":}", "{"] {
            assert!(
                matches!(decode_line(text), Err(DecodeError::Malformed(_))),
                "{text}"
            );
        }
        // And one that does parse keeps the rewrite's result.
        assert_eq!(decode_line("-0"), Ok(PyValue::int(0)));
        assert_eq!(decode_line("-0.0"), Ok(PyValue::Float(-0.0)));
    }

    /// The lexeme validator, on its own. It is not a parser and must not
    /// accept anything JSON does not spell as one complete number.
    #[test]
    fn a_number_lexeme_is_classified_exactly() {
        for integer in [
            "0",
            "-0",
            "7",
            "-7",
            "9223372036854775807",
            "-9223372036854775808",
            "18446744073709551615",
            "12345678901234567890123",
        ] {
            assert_eq!(classify_json_number(integer), Some(false), "{integer}");
        }
        for float in [
            "0.0", "-0.0", "1.5", "1e2", "1E2", "1e+2", "1e-2", "-1.5e-9",
        ] {
            assert_eq!(classify_json_number(float), Some(true), "{float}");
        }
        // Not one complete JSON number: leading zeros, bad signs, missing
        // separators, trailing text, CPython's own non-standard tokens, and
        // the empty string.
        for other in [
            "",
            "-",
            "+1",
            "01",
            "-01",
            "1.",
            ".5",
            "1e",
            "1e+",
            "1-0",
            "1 2",
            "1,",
            "0x10",
            "NaN",
            "Infinity",
            "-Infinity",
            "nan",
            "1_000",
            " 1",
            "1 ",
            "--1",
            "1.2.3",
            "1e2e3",
            "$serde_json::private::Number",
        ] {
            assert_eq!(classify_json_number(other), None, "{other:?}");
        }
    }

    /// The digit bound is measured on DIGITS, not on the length of the
    /// literal, so a sign never costs a digit.
    #[test]
    fn the_decimal_digit_bound_ignores_the_sign() {
        let at_bound = "9".repeat(MAX_INT_STR_DIGITS);
        let past_bound = "9".repeat(MAX_INT_STR_DIGITS + 1);
        for sign in ["", "-"] {
            let accepted = format!("{sign}{at_bound}");
            match number_from_token(&accepted) {
                NumberFromToken::Int(value) => {
                    assert_eq!(value.digits(), MAX_INT_STR_DIGITS);
                    assert_eq!(value.as_decimal(), accepted);
                }
                _ => panic!("{MAX_INT_STR_DIGITS} digits must decode"),
            }
            let refused = format!("{sign}{past_bound}");
            assert!(
                matches!(
                    number_from_token(&refused),
                    NumberFromToken::TooManyDigits(count) if count == MAX_INT_STR_DIGITS + 1
                ),
                "{} digits must be refused",
                MAX_INT_STR_DIGITS + 1
            );
            // And it reaches a caller as the source's own silent skip, never
            // as a named refusal.
            assert!(matches!(
                decode_line(&refused),
                Err(DecodeError::Malformed(_))
            ));
        }
    }

    /// An overflowing exponent stays a declared divergence: it must not
    /// quietly start saturating to an infinity now that the lexeme is in hand.
    #[test]
    fn an_overflowing_float_literal_is_still_out_of_range() {
        for text in ["1e999", "-1e999", "1.5e400", "1e309"] {
            assert!(
                matches!(number_from_token(text), NumberFromToken::OutOfRange),
                "{text}"
            );
            assert!(
                matches!(decode_line(text), Err(DecodeError::Malformed(_))),
                "{text}"
            );
        }
        // Underflow is NOT overflow: CPython gives 0.0 and so does this.
        assert_eq!(decode_line("1e-999"), Ok(PyValue::Float(0.0)));
        assert_eq!(decode_line("-1e-999"), Ok(PyValue::Float(-0.0)));
    }

    /// The canonical form is what makes equality mean equality.
    #[test]
    fn an_integer_is_canonical_however_it_was_built() {
        assert_eq!(PyValue::int(0), decode_line("-0").unwrap());
        assert_eq!(PyValue::int(0), decode_line("0").unwrap());
        assert_eq!(PyValue::int(-1i64), decode_line("-1").unwrap());
        assert_eq!(
            PyValue::int(u64::MAX),
            decode_line("18446744073709551615").unwrap()
        );
        let wide = decode_line("12345678901234567890123").unwrap();
        assert_eq!(wide, PyValue::int(12345678901234567890123i128));
        assert_ne!(wide, decode_line("12345678901234567890124").unwrap());
        // ... and the two really do share one f64.
        let approximate = 12345678901234567890123f64;
        assert_eq!(approximate, 12345678901234567890124f64);
    }

    /// An ordinary object that spells the parser's private key is an ordinary
    /// object. This is the control for the whole `arbitrary_precision` design.
    #[test]
    fn a_user_object_is_never_mistaken_for_the_private_number_payload() {
        let marker = SERDE_JSON_NUMBER_TOKEN;
        // One entry, and its value even LOOKS like a number lexeme.
        let one = format!("{{{:?}: \"123\"}}", marker);
        let PyValue::Dict(fields) = decode_line(&one).unwrap() else {
            panic!("a JSON object decodes to a dict")
        };
        assert_eq!(fields.len(), 1);
        assert_eq!(fields.get(marker), Some(&PyValue::Str("123".to_string())));
        // Two entries, the marker first.
        let two = format!("{{{:?}: \"123\", \"b\": 1}}", marker);
        let PyValue::Dict(fields) = decode_line(&two).unwrap() else {
            panic!("a JSON object decodes to a dict")
        };
        assert_eq!(fields.len(), 2);
        assert_eq!(fields.get(marker), Some(&PyValue::Str("123".to_string())));
        assert_eq!(fields.get("b"), Some(&PyValue::int(1)));
        assert_eq!(
            fields.keys().cloned().collect::<Vec<_>>(),
            vec![marker.to_string(), "b".to_string()]
        );
        // A non-string value under the marker key is not even ambiguous.
        let nested = format!("{{{:?}: {{\"deep\": [1]}}}}", marker);
        assert!(matches!(decode_line(&nested), Ok(PyValue::Dict(_))));
    }

    /// A number is a SCALAR however serde_json delivers it, so it must not
    /// consume a nesting level. Without this, a wide integer at the bound
    /// would be refused as if the input nested one container deeper.
    #[test]
    fn a_wide_number_at_the_depth_bound_is_not_a_container() {
        let open = "[".repeat(MAX_NESTING_DEPTH);
        let close = "]".repeat(MAX_NESTING_DEPTH);
        // A machine-width integer at the bound is accepted today; a wide one
        // and a float have to be accepted on exactly the same terms.
        for scalar in ["1", "12345678901234567890123", "1.5"] {
            let text = format!("{open}{scalar}{close}");
            assert!(decode_line(&text).is_ok(), "{scalar} at the bound");
        }
        // One container deeper is still refused BY NAME.
        let deeper = format!("[{open}1{close}]");
        assert!(matches!(
            decode_line(&deeper),
            Err(DecodeError::Unrepresentable(Refusal {
                reason: Refusal::NESTING_DEPTH_EXCEEDED,
                ..
            }))
        ));
    }
}
