//! The Python-isms of the v1 hub that are part of its observable contract.
//!
//! v1 is FastAPI on CPython: refusal texts interpolate `{value!r}`, request
//! fields go through `str(...)`, filenames through `os.path.basename`, query
//! numbers through pydantic, download names through Starlette's
//! Content-Disposition rule, and log lines through `json.dumps`. Clients and
//! operators see all of these, so v2 reproduces them here, in one place,
//! instead of inheriting whatever Rust's own formatting would print.

use serde_json::{Map, Value};

/// Python's `str.isspace()` for one character: Unicode White_Space plus the
/// four information separators U+001C..U+001F, which CPython also counts.
pub fn py_isspace(c: char) -> bool {
    c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c)
}

/// `str.strip()` with no arguments.
pub fn py_strip(s: &str) -> &str {
    s.trim_matches(py_isspace)
}

/// `str.split()` with no arguments: runs of whitespace separate, and empty
/// pieces never appear.
pub fn py_split(s: &str) -> impl Iterator<Item = &str> {
    s.split(py_isspace).filter(|p| !p.is_empty())
}

/// `int(text)` for configuration values: surrounding whitespace, an optional
/// sign, decimal digits with single underscores between them. `None` is the
/// ValueError.
pub fn py_int(text: &str) -> Option<i128> {
    let t = py_strip(text);
    let (neg, digits) = match t.as_bytes().first() {
        Some(b'-') => (true, &t[1..]),
        Some(b'+') => (false, &t[1..]),
        _ => (false, t),
    };
    if digits.is_empty() || digits.starts_with('_') || digits.ends_with('_') || digits.contains("__") {
        return None;
    }
    let mut v: i128 = 0;
    for c in digits.chars() {
        if c == '_' {
            continue;
        }
        let d = c.to_digit(10)? as i128;
        v = v.checked_mul(10)?.checked_add(d)?;
    }
    Some(if neg { -v } else { v })
}

/// Python truthiness of a JSON value as `json.loads` returns it.
pub fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(true),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// `body.get(key) or default`, the v1 idiom for every request field.
pub fn get_or<'a>(obj: &'a Map<String, Value>, key: &str) -> Option<&'a Value> {
    obj.get(key).filter(|v| truthy(v))
}

/// `str(value)` of a value `json.loads` produced.
pub fn py_str(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => py_repr(other),
    }
}

/// `repr(value)` of a value `json.loads` produced.
pub fn py_repr(v: &Value) -> String {
    match v {
        Value::Null => "None".into(),
        Value::Bool(true) => "True".into(),
        Value::Bool(false) => "False".into(),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                i.to_string()
            } else if let Some(u) = n.as_u64() {
                u.to_string()
            } else {
                py_float_repr(n.as_f64().unwrap_or(f64::NAN))
            }
        }
        Value::String(s) => py_repr_str(s),
        Value::Array(a) => format!("[{}]", a.iter().map(py_repr).collect::<Vec<_>>().join(", ")),
        Value::Object(o) => format!(
            "{{{}}}",
            o.iter().map(|(k, v)| format!("{}: {}", py_repr_str(k), py_repr(v))).collect::<Vec<_>>().join(", ")
        ),
    }
}

/// `repr(float)`: the shortest round-trip digits, fixed notation when the
/// decimal point falls in (-4, 16], scientific with a signed two-digit
/// exponent otherwise, and `.0` on integral values.
pub fn py_float_repr(f: f64) -> String {
    if f.is_nan() {
        return "nan".into();
    }
    if f.is_infinite() {
        return if f > 0.0 { "inf".into() } else { "-inf".into() };
    }
    if f == 0.0 {
        return if f.is_sign_negative() { "-0.0".into() } else { "0.0".into() };
    }
    // `{:e}` gives the shortest round-trip digits: "1.5e-5", "-1e16"
    let sci = format!("{:e}", f);
    let (mant, exp) = sci.split_once('e').unwrap_or((&sci, "0"));
    let exp: i32 = exp.parse().unwrap_or(0);
    let (sign, mant) = match mant.strip_prefix('-') {
        Some(m) => ("-", m),
        None => ("", mant),
    };
    let digits: String = mant.chars().filter(|c| *c != '.').collect();
    // decimal point position relative to the start of `digits`
    let decpt = exp + 1;
    if decpt <= -4 || decpt > 16 {
        let mut m = digits[..1].to_string();
        if digits.len() > 1 {
            m.push('.');
            m.push_str(&digits[1..]);
        }
        let e = decpt - 1;
        format!("{sign}{m}e{}{:02}", if e < 0 { '-' } else { '+' }, e.abs())
    } else if decpt <= 0 {
        format!("{sign}0.{}{}", "0".repeat((-decpt) as usize), digits)
    } else if decpt as usize >= digits.len() {
        format!("{sign}{}{}.0", digits, "0".repeat(decpt as usize - digits.len()))
    } else {
        format!("{sign}{}.{}", &digits[..decpt as usize], &digits[decpt as usize..])
    }
}

/// CPython's `repr(str)`.
pub fn py_repr_str(s: &str) -> String {
    let quote = if s.contains('\'') && !s.contains('"') { '"' } else { '\'' };
    let mut out = String::with_capacity(s.len() + 2);
    out.push(quote);
    for c in s.chars() {
        match c {
            _ if c == quote || c == '\\' => {
                out.push('\\');
                out.push(c);
            }
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            _ if (c as u32) < 0x20 || c as u32 == 0x7f => out.push_str(&format!("\\x{:02x}", c as u32)),
            _ if (c as u32) < 0x7f => out.push(c),
            _ if py_isprintable(c) => out.push(c),
            _ if (c as u32) <= 0xff => out.push_str(&format!("\\x{:02x}", c as u32)),
            _ if (c as u32) <= 0xffff => out.push_str(&format!("\\u{:04x}", c as u32)),
            _ => out.push_str(&format!("\\U{:08x}", c as u32)),
        }
    }
    out.push(quote);
    out
}

/// Python's `str.isprintable()` for one non-ASCII character: everything but
/// the "Other" and "Separator" categories. Exact for controls, separators,
/// format characters, private use and noncharacters; other unassigned code
/// points count as printable (CPython would escape them).
fn py_isprintable(c: char) -> bool {
    let u = c as u32;
    let nonprint: &[(u32, u32)] = &[
        (0x80, 0xa0),
        (0xad, 0xad),
        (0x600, 0x605),
        (0x61c, 0x61c),
        (0x6dd, 0x6dd),
        (0x70f, 0x70f),
        (0x890, 0x891),
        (0x8e2, 0x8e2),
        (0x1680, 0x1680),
        (0x180e, 0x180e),
        (0x2000, 0x200f),
        (0x2028, 0x202f),
        (0x205f, 0x2064),
        (0x2066, 0x206f),
        (0x3000, 0x3000),
        (0xd800, 0xf8ff),
        (0xfdd0, 0xfdef),
        (0xfeff, 0xfeff),
        (0xfff9, 0xfffb),
        (0x110bd, 0x110bd),
        (0x110cd, 0x110cd),
        (0x13430, 0x1343f),
        (0x1bca0, 0x1bca3),
        (0x1d173, 0x1d17a),
        (0xe0001, 0xe0001),
        (0xe0020, 0xe007f),
        (0xf0000, 0x10ffff),
    ];
    if (u & 0xfffe) == 0xfffe {
        return false;
    }
    !nonprint.iter().any(|(a, b)| (*a..=*b).contains(&u))
}

/// Iterating `value or []` the way a v1 handler's list comprehension does.
/// `Err` is the TypeError (the request becomes a 500).
pub fn py_iter(v: Option<&Value>) -> Result<Vec<Value>, ()> {
    let Some(v) = v.filter(|v| truthy(v)) else { return Ok(Vec::new()) };
    match v {
        Value::Array(a) => Ok(a.clone()),
        Value::String(s) => Ok(s.chars().map(|c| Value::String(c.to_string())).collect()),
        Value::Object(o) => Ok(o.keys().map(|k| Value::String(k.clone())).collect()),
        _ => Err(()),
    }
}

/// What a v1 TEXT-affinity column held after binding `value`: strings as
/// they are, booleans and integers as their decimal text, null as NULL.
/// `Err` is sqlite3's refusal to bind a list or dict (a 500 in v1).
pub fn sqlite_text(v: Option<&Value>) -> Result<Option<String>, ()> {
    match v {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(Value::Bool(b)) => Ok(Some(if *b { "1" } else { "0" }.into())),
        Some(Value::Number(n)) => Ok(Some(if n.is_f64() { py_float_repr(n.as_f64().unwrap_or(0.0)) } else { n.to_string() })),
        Some(_) => Err(()),
    }
}

/// Text PostgreSQL can store: it refuses U+0000, which v1's SQLite kept.
pub fn pg_text(s: String) -> String {
    if s.contains('\0') {
        s.replace('\0', "")
    } else {
        s
    }
}

/// The first `n` characters (code points), as Python's `s[:n]`.
pub fn py_prefix(s: &str, n: usize) -> &str {
    match s.char_indices().nth(n) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

/// `os.path.basename` of the platform the hub runs on (ntpath on Windows,
/// posixpath elsewhere), as v1 applied it to an upload's declared name.
pub fn py_basename(p: &str) -> &str {
    #[cfg(windows)]
    {
        let rest = &p[nt_drive_len(p)..];
        match rest.rfind(['/', '\\']) {
            Some(i) => &rest[i + 1..],
            None => rest,
        }
    }
    #[cfg(not(windows))]
    {
        match p.rfind('/') {
            Some(i) => &p[i + 1..],
            None => p,
        }
    }
}

/// Length in bytes of `ntpath.splitdrive(p)[0]` (Python 3.10).
#[cfg_attr(not(windows), allow(dead_code))]
fn nt_drive_len(p: &str) -> usize {
    let b = p.as_bytes();
    if b.len() < 2 {
        return 0;
    }
    let sep = |x: u8| x == b'\\' || x == b'/';
    if sep(b[0]) && sep(b[1]) && !b.get(2).map(|x| sep(*x)).unwrap_or(false) {
        let Some(i1) = (2..b.len()).find(|&i| sep(b[i])) else { return 0 };
        let i2 = (i1 + 1..b.len()).find(|&i| sep(b[i]));
        return match i2 {
            Some(i) if i == i1 + 1 => 0,
            Some(i) => i,
            None => b.len(),
        };
    }
    // a drive is any first character followed by ':' (ntpath does not check
    // that it is a letter); measured in characters, not bytes
    let mut it = p.char_indices();
    match (it.next(), it.next()) {
        (Some(_), Some((i, ':'))) => i + 1,
        _ => 0,
    }
}

/// Starlette's Content-Disposition for a download named `name`:
/// `filename="name"` when `urllib.parse.quote(name)` leaves it unchanged,
/// RFC 5987 `filename*=utf-8''…` otherwise.
pub fn content_disposition(name: &str) -> String {
    use percent_encoding::{utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};
    const QUOTE: &AsciiSet = &NON_ALPHANUMERIC.remove(b'_').remove(b'.').remove(b'-').remove(b'~').remove(b'/');
    let quoted = utf8_percent_encode(name, QUOTE).to_string();
    if quoted != name {
        format!("attachment; filename*=utf-8''{quoted}")
    } else {
        format!("attachment; filename=\"{name}\"")
    }
}

/// A FastAPI/pydantic query validation failure, as v1 answers it (422 with
/// `detail` a list).
pub fn query_error(param: &str, number: bool, input: &str) -> Value {
    let (ty, msg) = if number {
        ("float_parsing", "Input should be a valid number, unable to parse string as a number")
    } else {
        ("int_parsing", "Input should be a valid integer, unable to parse string as an integer")
    };
    serde_json::json!({ "detail": [ { "type": ty, "loc": ["query", param], "msg": msg, "input": input } ] })
}

/// pydantic's float parsing of a query string (lax mode): surrounding
/// whitespace, decimal or exponent notation, `inf`/`nan` spellings.
pub fn parse_query_float(s: &str) -> Option<f64> {
    let t = s.trim();
    if t.is_empty() {
        return None;
    }
    let lower = t.to_ascii_lowercase();
    let body = lower.trim_start_matches(['+', '-']);
    if matches!(body, "inf" | "infinity" | "nan") {
        return lower.parse::<f64>().ok().or(Some(if lower.starts_with('-') { f64::NEG_INFINITY } else if body == "nan" { f64::NAN } else { f64::INFINITY }));
    }
    if !t.bytes().all(|b| b.is_ascii_digit() || matches!(b, b'+' | b'-' | b'.' | b'e' | b'E' | b'_')) {
        return None;
    }
    t.replace('_', "").parse::<f64>().ok()
}

/// pydantic's int parsing of a query string, saturated to i64 (v1 handed
/// out-of-range values to SQLite, which failed).
pub fn parse_query_int(s: &str) -> Option<i64> {
    let t = s.trim();
    let (neg, digits) = match t.as_bytes().first() {
        Some(b'-') => (true, &t[1..]),
        Some(b'+') => (false, &t[1..]),
        _ => (false, t),
    };
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let mut v: i64 = 0;
    for b in digits.bytes() {
        v = v.saturating_mul(10).saturating_add((b - b'0') as i64);
    }
    Some(if neg { -v } else { v })
}

/// `json.dumps(value)` with CPython's defaults: `", "`/`": "` separators
/// and every non-ASCII character escaped. Used for the stdout log lines,
/// which operators parse.
pub fn py_json(v: &Value) -> String {
    let mut out = String::new();
    py_json_into(v, &mut out);
    out
}

fn py_json_into(v: &Value, out: &mut String) {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => {
            if n.is_f64() {
                out.push_str(&py_float_repr(n.as_f64().unwrap_or(0.0)))
            } else {
                out.push_str(&n.to_string())
            }
        }
        Value::String(s) => py_json_str(s, out),
        Value::Array(a) => {
            out.push('[');
            for (i, x) in a.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                py_json_into(x, out);
            }
            out.push(']');
        }
        Value::Object(o) => {
            out.push('{');
            for (i, (k, x)) in o.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                py_json_str(k, out);
                out.push_str(": ");
                py_json_into(x, out);
            }
            out.push('}');
        }
    }
}

fn py_json_str(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            ' '..='~' => out.push(c),
            _ => {
                let mut buf = [0u16; 2];
                for unit in c.encode_utf16(&mut buf) {
                    out.push_str(&format!("\\u{:04x}", unit));
                }
            }
        }
    }
    out.push('"');
}

/// Python's `round(x)`: half to even.
pub fn py_round(x: f64) -> i64 {
    let r = x.round();
    if (x - x.trunc()).abs() == 0.5 && (r as i64) % 2 != 0 {
        (r - x.signum()) as i64
    } else {
        r as i64
    }
}

/// An ASGI server's `scope["path"]`: the request path percent-decoded as
/// UTF-8 (invalid sequences replaced), which is what v1 routed on, logged
/// and filtered on.
pub fn decoded_path(raw: &str) -> String {
    if !raw.contains('%') {
        return raw.to_string();
    }
    percent_encoding::percent_decode_str(raw).decode_utf8_lossy().into_owned()
}

/// A header value read the way Starlette reads it: latin-1.
pub fn latin1(bytes: &[u8]) -> String {
    bytes.iter().map(|&b| b as char).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn repr_matches_cpython() {
        assert_eq!(py_repr_str("zz.nobody.ffffff"), "'zz.nobody.ffffff'");
        assert_eq!(py_repr_str("it's"), "\"it's\"");
        assert_eq!(py_repr_str("both ' and \""), "'both \\' and \"'");
        assert_eq!(py_repr_str("a\\b\n\t\x01\x7f"), "'a\\\\b\\n\\t\\x01\\x7f'");
        assert_eq!(py_repr_str("é\u{a0}\u{2028}😀\u{e000}"), "'é\\xa0\\u2028😀\\ue000'");
        assert_eq!(py_repr(&json!({"id": "x", "n": [1, true, null]})), "{'id': 'x', 'n': [1, True, None]}");
    }

    #[test]
    fn float_repr_matches_cpython() {
        for (f, s) in [
            (1.5, "1.5"),
            (100.0, "100.0"),
            (1e16, "1e+16"),
            (1e15, "1000000000000000.0"),
            (0.0001, "0.0001"),
            (0.00001, "1e-05"),
            (1.5e-5, "1.5e-05"),
            (-2.5e20, "-2.5e+20"),
            (0.1, "0.1"),
            (123456.789, "123456.789"),
        ] {
            assert_eq!(py_float_repr(f), s, "{f}");
        }
    }

    #[test]
    fn ints_and_strip() {
        assert_eq!(py_int(" 30 "), Some(30));
        assert_eq!(py_int("+1_000"), Some(1000));
        assert_eq!(py_int("-5"), Some(-5));
        assert_eq!(py_int("1__0"), None);
        assert_eq!(py_int("_1"), None);
        assert_eq!(py_int("30.0"), None);
        assert_eq!(py_int(""), None);
        assert_eq!(py_strip("\u{1c} a \u{a0}"), "a");
    }

    #[test]
    fn iteration_and_text() {
        assert_eq!(py_iter(Some(&json!("ab"))).unwrap(), vec![json!("a"), json!("b")]);
        assert_eq!(py_iter(Some(&json!(0))).unwrap(), Vec::<Value>::new());
        assert!(py_iter(Some(&json!(5))).is_err());
        assert!(py_iter(Some(&json!(true))).is_err());
        assert_eq!(py_str(&json!(true)), "True");
        assert_eq!(sqlite_text(Some(&json!(true))).unwrap().as_deref(), Some("1"));
        assert!(sqlite_text(Some(&json!([1]))).is_err());
    }

    #[test]
    fn basename_like_python() {
        assert_eq!(py_basename("../../etc/passwd"), "passwd");
        assert_eq!(py_basename("a/b/"), "");
        assert_eq!(py_basename(".."), "..");
        #[cfg(windows)]
        {
            assert_eq!(py_basename("..\\..\\win.ini"), "win.ini");
            assert_eq!(py_basename("C:notes.txt"), "notes.txt");
            assert_eq!(py_basename("\\\\server\\share"), "");
            assert_eq!(py_basename("\\\\server\\share\\x.txt"), "x.txt");
        }
    }

    #[test]
    fn disposition_like_starlette() {
        assert_eq!(content_disposition("notes.txt"), "attachment; filename=\"notes.txt\"");
        assert_eq!(content_disposition("my file.txt"), "attachment; filename*=utf-8''my%20file.txt");
        assert_eq!(content_disposition("é.md"), "attachment; filename*=utf-8''%C3%A9.md");
    }

    #[test]
    fn log_json_like_python() {
        let v = json!({"ts": "t", "path": "/é", "slugs": ["a"], "status": 200, "ms": 3});
        assert_eq!(py_json(&v), "{\"ts\": \"t\", \"path\": \"/\\u00e9\", \"slugs\": [\"a\"], \"status\": 200, \"ms\": 3}");
        assert_eq!(py_round(2.5), 2);
        assert_eq!(py_round(3.5), 4);
        assert_eq!(py_round(2.4), 2);
    }

    #[test]
    fn query_numbers() {
        assert_eq!(parse_query_float("25"), Some(25.0));
        assert_eq!(parse_query_float(" 1e1 "), Some(10.0));
        assert!(parse_query_float("inf").unwrap().is_infinite());
        assert_eq!(parse_query_float("abc"), None);
        assert_eq!(parse_query_float(""), None);
        assert_eq!(parse_query_int("5"), Some(5));
        assert_eq!(parse_query_int("5.0"), None);
        assert_eq!(parse_query_int("-0"), Some(0));
    }
}
