//! Web apps: which pages a browser lets call this hub (Hubchat Web).
//!
//! A browser sends a page's request to the hub whatever the hub thinks of
//! the page; what it enforces is that the page may only READ the answer
//! when the hub names the page's origin. So this module answers one
//! question: is this `Origin` on the hub owner's list? The list is
//! `HUB_WEB_ORIGINS`, or the runtime file's `web_origins`, which wins and is
//! read at each use (the embedding host changes it without a restart).
//!
//! Two things rest on that answer:
//! - CORS, on `/api/*` and `/healthz` only (`api::dispatch`): the preflight
//!   answer, and the headers on every answer to a listed origin;
//! - a WebSocket upgrade, which a browser never preflights: the hub itself
//!   has to refuse an upgrade whose `Origin` is not listed
//!   (`Asker::may_upgrade`). Every upgrade is refused today; an endpoint
//!   that accepts one must ask here first.
//!
//! Never the operator page (`/`, `/ui/data`, `/ui/messages`): it shows all
//! mail without sign-in, and naming a web page there would publish every
//! message to that page. With no list the hub adds nothing and answers byte
//! for byte as it did before this module.

use http::{header, HeaderMap, HeaderValue, Method, Response, StatusCode};
use serde_json::Value;

use crate::config::Config;

/// The methods a listed page may use: every method an `/api/*` route takes.
pub const ALLOW_METHODS: &str = "GET, POST, PATCH, DELETE";
/// The request headers a listed page may send beyond the browser's own:
/// the credentials, a JSON or file body's type, and a resumed download's.
pub const ALLOW_HEADERS: &str = "X-Org-Auth, Content-Type, Range, If-Range";
/// The answer headers a listed page may read beyond the browser's defaults:
/// what a download, a resumed download and a retry need.
pub const EXPOSE_HEADERS: &str = "Content-Range, Content-Length, Content-Disposition, Accept-Ranges, ETag, Last-Modified, Date, Retry-After";
/// How long a browser may remember a preflight answer, in seconds (browsers
/// cap it lower themselves).
pub const MAX_AGE: &str = "86400";
/// More entries than this is refused: a list this long is a mistake.
pub const MAX_ORIGINS: usize = 64;

/// The hub owner's list.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub enum Origins {
    /// No list: no web page may call, and nothing is added to any answer.
    #[default]
    None,
    /// `*`: any web page (for a hub that is public anyway). Each asker is
    /// still named itself, never as `*`, and `null` is never allowed.
    Any,
    /// Exact origins, as a browser writes them: lower-case scheme and host,
    /// no default port, nothing after the port.
    List(Vec<String>),
}

/// One entry as a browser writes that origin, or None when it is not an
/// origin a web page can have: `scheme://host[:port]`, http or https, with
/// no credentials, path, query or fragment (a closing `/` is fine).
fn origin(entry: &str) -> Option<String> {
    let entry = entry.trim();
    if entry.is_empty() || entry.bytes().any(|b| b.is_ascii_whitespace() || b.is_ascii_control()) {
        return None;
    }
    let url = reqwest::Url::parse(entry).ok()?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none_or(str::is_empty)
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.path() != "/"
    {
        return None;
    }
    Some(url.origin().ascii_serialization())
}

/// A list from its entries; an empty entry is skipped, `*` anywhere means
/// any origin. `Err` names the entry that is not an origin.
pub fn list<'a>(entries: impl IntoIterator<Item = &'a str>) -> Result<Origins, String> {
    let mut out: Vec<String> = Vec::new();
    let mut any = false;
    for entry in entries {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        if entry == "*" {
            any = true;
            continue;
        }
        let o = origin(entry).ok_or_else(|| entry.to_string())?;
        if !out.contains(&o) {
            if out.len() >= MAX_ORIGINS {
                return Err(format!("more than {MAX_ORIGINS} origins"));
            }
            out.push(o);
        }
    }
    Ok(if any {
        Origins::Any
    } else if out.is_empty() {
        Origins::None
    } else {
        Origins::List(out)
    })
}

/// `HUB_WEB_ORIGINS`: origins separated by commas, or `*`. A value that is
/// not that refuses to start: a typo must not silently lock every page out
/// (or let a wrong one in).
pub fn from_env(value: &str) -> anyhow::Result<Origins> {
    list(value.split(',')).map_err(|bad| {
        anyhow::anyhow!(
            "HUB_WEB_ORIGINS must list the web addresses that may call this hub, separated by commas, such as \
             https://chat.example.com or https://pc.example.ts.net:8443 (scheme, host and optional port, nothing \
             after it; at most {MAX_ORIGINS}), or be * for any; not {bad:?}"
        )
    })
}

/// The list as it stands now: the runtime file's `web_origins` (an array of
/// origin strings, `["*"]` for any) when the file has one, the startup list
/// otherwise. A file that cannot be read as JSON, or a `web_origins` that is
/// not such an array, allows nobody: never wider than the owner asked.
pub fn origins(cfg: &Config) -> Origins {
    let Some(path) = &cfg.runtime_config_file else { return cfg.web_origins.clone() };
    let text = match std::fs::read(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return cfg.web_origins.clone(),
        Err(_) => return Origins::None,
    };
    let Ok(Value::Object(file)) = serde_json::from_slice::<Value>(&text) else { return Origins::None };
    match file.get("web_origins") {
        None | Some(Value::Null) => cfg.web_origins.clone(),
        Some(Value::Array(entries)) => {
            let Some(strings) = entries.iter().map(Value::as_str).collect::<Option<Vec<&str>>>() else { return Origins::None };
            list(strings).unwrap_or(Origins::None)
        }
        Some(_) => Origins::None,
    }
}

/// Who is asking, by the request's `Origin` header.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Asker {
    /// No `Origin` header: not a page in a browser (every native client).
    NotAPage,
    /// A page, and the hub has no list.
    NoList,
    /// A page on the list: its origin exactly as it sent it.
    Listed(HeaderValue),
    /// A page that is not on the list.
    NotListed,
}

impl Asker {
    /// May this asker open a WebSocket? A browser sends `Origin` with every
    /// upgrade and applies no CORS to it, so the hub is the only guard: a
    /// page must be on the list; a client that sends no `Origin` is not a
    /// page.
    pub fn may_upgrade(&self) -> bool {
        matches!(self, Asker::NotAPage | Asker::Listed(_))
    }
}

/// Read the asker from the request's headers and the list as it stands now.
pub fn asker(cfg: &Config, headers: &HeaderMap) -> Asker {
    let mut sent = headers.get_all(header::ORIGIN).iter();
    let Some(value) = sent.next() else { return Asker::NotAPage };
    let list = origins(cfg);
    if list == Origins::None {
        return Asker::NoList;
    }
    // two Origin headers, or one that is not text, is no browser's request
    let (Ok(text), None) = (value.to_str(), sent.next()) else { return Asker::NotListed };
    let text = text.trim().to_ascii_lowercase();
    // a page without an origin of its own (a sandboxed frame, a local file)
    // says "null": that names nobody, so it is never allowed, with `*` too
    if text.is_empty() || text == "null" {
        return Asker::NotListed;
    }
    let listed = match &list {
        Origins::Any => true,
        Origins::List(entries) => entries.iter().any(|e| *e == text),
        Origins::None => false,
    };
    if listed {
        Asker::Listed(value.clone())
    } else {
        Asker::NotListed
    }
}

/// The paths a web page may be let into: the API and the health answer,
/// the same set the relay-only door serves. Never the operator page.
pub fn shared_path(path: &str) -> bool {
    path.starts_with("/api/") || path == "/healthz"
}

/// What the hub owes this request beyond its ordinary answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Cors {
    /// Nothing: not a page, no list, or a path that is never shared.
    Untouched,
    /// A listed page's preflight: answered here, the route never runs.
    Preflight(HeaderValue),
    /// A listed page: its ordinary answer, readable by it.
    Share(HeaderValue),
    /// A page that is not listed: its ordinary answer, unreadable by it,
    /// marked as depending on who asked.
    Withhold,
}

/// Decide it before the route runs (a preflight never reaches a route).
pub fn cors(cfg: &Config, method: &Method, path: &str, headers: &HeaderMap) -> Cors {
    if !shared_path(path) || !headers.contains_key(header::ORIGIN) {
        return Cors::Untouched;
    }
    match asker(cfg, headers) {
        Asker::NotAPage | Asker::NoList => Cors::Untouched,
        Asker::NotListed => Cors::Withhold,
        Asker::Listed(origin) => {
            if *method == Method::OPTIONS && headers.contains_key(header::ACCESS_CONTROL_REQUEST_METHOD) {
                Cors::Preflight(origin)
            } else {
                Cors::Share(origin)
            }
        }
    }
}

/// The answer to a listed page's preflight: 204, no body. The hub uses no
/// cookies, so it never allows credentials.
pub fn preflight<B: Default>(origin: &HeaderValue) -> Response<B> {
    let mut r = Response::new(B::default());
    *r.status_mut() = StatusCode::NO_CONTENT;
    let h = r.headers_mut();
    h.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, origin.clone());
    h.insert(header::ACCESS_CONTROL_ALLOW_METHODS, HeaderValue::from_static(ALLOW_METHODS));
    h.insert(header::ACCESS_CONTROL_ALLOW_HEADERS, HeaderValue::from_static(ALLOW_HEADERS));
    h.insert(header::ACCESS_CONTROL_MAX_AGE, HeaderValue::from_static(MAX_AGE));
    h.insert(header::VARY, HeaderValue::from_static("Origin"));
    r
}

impl Cors {
    /// Mark an ordinary answer, a refusal as much as a success: a page that
    /// could not read a 401 could not tell it from a hub that is down.
    pub fn mark(&self, headers: &mut HeaderMap) {
        match self {
            Cors::Untouched | Cors::Preflight(_) => {}
            Cors::Withhold => {
                headers.append(header::VARY, HeaderValue::from_static("Origin"));
            }
            Cors::Share(origin) => {
                headers.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, origin.clone());
                headers.insert(header::ACCESS_CONTROL_EXPOSE_HEADERS, HeaderValue::from_static(EXPOSE_HEADERS));
                headers.append(header::VARY, HeaderValue::from_static("Origin"));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn cfg(origins: &str, runtime: Option<&std::path::Path>) -> Config {
        let mut vars: HashMap<String, String> =
            [("HUB_DATABASE_URL", "postgres://x@localhost/y"), ("HUB_WEB_ORIGINS", origins)].iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        if let Some(p) = runtime {
            vars.insert("HUB_RUNTIME_CONFIG_FILE".into(), p.to_str().unwrap().into());
        }
        Config::from_vars(&vars).unwrap()
    }

    fn headers(origins: &[&str]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for o in origins {
            h.append(header::ORIGIN, HeaderValue::from_str(o).unwrap());
        }
        h
    }

    #[test]
    fn an_origin_is_written_as_a_browser_writes_it() {
        for (entry, want) in [
            ("https://chat.example.com", "https://chat.example.com"),
            (" HTTPS://Chat.Example.COM:443/ ", "https://chat.example.com"),
            ("http://Localhost:80", "http://localhost"),
            ("http://localhost:5173/", "http://localhost:5173"),
            ("https://pc.example.ts.net:8443", "https://pc.example.ts.net:8443"),
            ("http://[::1]:8080", "http://[::1]:8080"),
            ("https://bücher.example", "https://xn--bcher-kva.example"),
        ] {
            assert_eq!(origin(entry).as_deref(), Some(want), "{entry:?}");
        }
        for bad in [
            "", "*", "null", "chat.example.com", "//chat.example.com", "ftp://files.example", "wss://chat.example.com",
            "https://", "https://chat.example.com/app", "https://chat.example.com/?x=1", "https://chat.example.com#top",
            "https://user@chat.example.com", "https://user:pw@chat.example.com", "https://a.example https://b.example",
            "https://a.example\thttps://b.example", "file:///C:/app/index.html",
        ] {
            assert_eq!(origin(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn a_list_from_its_entries() {
        assert_eq!(list([]), Ok(Origins::None));
        assert_eq!(list(["", "  "]), Ok(Origins::None));
        assert_eq!(list(["*"]), Ok(Origins::Any));
        assert_eq!(list(["https://a.example", " * "]), Ok(Origins::Any));
        assert_eq!(
            list(["https://a.example", "", "HTTPS://A.example/", "http://b.example:8080"]),
            Ok(Origins::List(vec!["https://a.example".into(), "http://b.example:8080".into()])),
            "an origin written twice counts once"
        );
        assert_eq!(list(["https://a.example", "b.example"]), Err("b.example".into()));
        let many: Vec<String> = (0..=MAX_ORIGINS).map(|i| format!("https://h{i}.example")).collect();
        assert!(list(many.iter().map(String::as_str)).is_err());
        assert!(list(many[..MAX_ORIGINS].iter().map(String::as_str)).is_ok());
        assert!(from_env("https://a.example,b.example").unwrap_err().to_string().contains("HUB_WEB_ORIGINS"));
        assert_eq!(from_env("").unwrap(), Origins::None);
    }

    #[test]
    fn who_is_asking() {
        let none = cfg("", None);
        let some = cfg("https://chat.example.com, http://localhost:5173", None);
        let any = cfg("*", None);
        // not a page, whatever the list
        for c in [&none, &some, &any] {
            assert_eq!(asker(c, &headers(&[])), Asker::NotAPage);
            assert!(asker(c, &headers(&[])).may_upgrade(), "a native client may always ask to upgrade");
        }
        // no list: no page
        assert_eq!(asker(&none, &headers(&["https://chat.example.com"])), Asker::NoList);
        assert!(!asker(&none, &headers(&["https://chat.example.com"])).may_upgrade());
        // a list
        let listed = asker(&some, &headers(&["https://chat.example.com"]));
        assert_eq!(listed, Asker::Listed(HeaderValue::from_static("https://chat.example.com")));
        assert!(listed.may_upgrade());
        assert!(matches!(asker(&some, &headers(&["http://localhost:5173"])), Asker::Listed(_)));
        assert!(matches!(asker(&some, &headers(&["HTTPS://CHAT.EXAMPLE.COM"])), Asker::Listed(_)), "compared without case");
        for other in ["https://evil.example", "http://chat.example.com", "https://chat.example.com:444", "https://chat.example.com.evil.example", "null", "", "https://chat.example.com/"] {
            let a = asker(&some, &headers(&[other]));
            assert_eq!(a, Asker::NotListed, "{other:?}");
            assert!(!a.may_upgrade(), "{other:?}");
        }
        assert_eq!(asker(&some, &headers(&["https://chat.example.com", "https://chat.example.com"])), Asker::NotListed, "two Origin headers");
        // any: everybody but null, each named as it wrote itself
        assert_eq!(asker(&any, &headers(&["https://evil.example"])), Asker::Listed(HeaderValue::from_static("https://evil.example")));
        assert_eq!(asker(&any, &headers(&["null"])), Asker::NotListed);
        assert_eq!(asker(&any, &headers(&["NULL"])), Asker::NotListed);
    }

    #[test]
    fn what_a_request_is_owed() {
        let some = cfg("https://chat.example.com", None);
        let o = HeaderValue::from_static("https://chat.example.com");
        let mut pre = headers(&["https://chat.example.com"]);
        pre.insert(header::ACCESS_CONTROL_REQUEST_METHOD, HeaderValue::from_static("POST"));
        assert_eq!(cors(&some, &Method::OPTIONS, "/api/sync", &pre), Cors::Preflight(o.clone()));
        assert_eq!(cors(&some, &Method::OPTIONS, "/healthz", &pre), Cors::Preflight(o.clone()));
        assert_eq!(cors(&some, &Method::OPTIONS, "/api/sync", &headers(&["https://chat.example.com"])), Cors::Share(o.clone()), "OPTIONS alone is no preflight");
        assert_eq!(cors(&some, &Method::POST, "/api/sync", &pre), Cors::Share(o.clone()));
        assert_eq!(cors(&some, &Method::POST, "/api/sync", &headers(&["https://evil.example"])), Cors::Withhold);
        assert_eq!(cors(&some, &Method::POST, "/api/sync", &headers(&[])), Cors::Untouched);
        for never in ["/", "/ui/data", "/ui/messages", "/api", "/healthz/", "/favicon.ico"] {
            assert_eq!(cors(&some, &Method::GET, never, &pre), Cors::Untouched, "{never}");
            assert_eq!(cors(&cfg("*", None), &Method::OPTIONS, never, &pre), Cors::Untouched, "{never}");
        }
        assert_eq!(cors(&cfg("", None), &Method::OPTIONS, "/api/sync", &pre), Cors::Untouched);

        let r: Response<()> = preflight(&o);
        assert_eq!(r.status(), StatusCode::NO_CONTENT);
        assert_eq!(r.headers().get(header::ACCESS_CONTROL_ALLOW_ORIGIN), Some(&o));
        assert_eq!(r.headers().get(header::ACCESS_CONTROL_ALLOW_METHODS).unwrap(), ALLOW_METHODS);
        assert_eq!(r.headers().get(header::ACCESS_CONTROL_ALLOW_HEADERS).unwrap(), ALLOW_HEADERS);
        assert_eq!(r.headers().get(header::ACCESS_CONTROL_MAX_AGE).unwrap(), MAX_AGE);
        assert!(r.headers().get(header::ACCESS_CONTROL_ALLOW_CREDENTIALS).is_none());

        let mut h = HeaderMap::new();
        Cors::Untouched.mark(&mut h);
        assert!(h.is_empty());
        Cors::Withhold.mark(&mut h);
        assert_eq!(h.len(), 1);
        assert_eq!(h.get(header::VARY).unwrap(), "Origin");
        let mut h = HeaderMap::new();
        h.insert(header::VARY, HeaderValue::from_static("Accept-Encoding"));
        Cors::Share(o.clone()).mark(&mut h);
        assert_eq!(h.get(header::ACCESS_CONTROL_ALLOW_ORIGIN), Some(&o));
        assert_eq!(h.get(header::ACCESS_CONTROL_EXPOSE_HEADERS).unwrap(), EXPOSE_HEADERS);
        assert_eq!(h.get_all(header::VARY).iter().count(), 2, "an answer's own Vary is kept");
    }

    #[test]
    fn the_runtime_file_wins() {
        let dir = std::env::temp_dir().join(format!("orgtree-hub-web-unit-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("runtime.json");
        let c = cfg("https://env.example", Some(&file));
        let env = Origins::List(vec!["https://env.example".into()]);
        assert_eq!(origins(&c), env, "no file: the startup list");
        for (text, want) in [
            (r#"{"max_attachment_bytes": 5}"#, env.clone()),
            (r#"{"web_origins": null}"#, env.clone()),
            (r#"{"web_origins": ["https://file.example"]}"#, Origins::List(vec!["https://file.example".into()])),
            (r#"{"web_origins": ["*"]}"#, Origins::Any),
            (r#"{"web_origins": []}"#, Origins::None),
            (r#"{"web_origins": "https://file.example"}"#, Origins::None),
            (r#"{"web_origins": ["https://file.example", 7]}"#, Origins::None),
            (r#"{"web_origins": ["https://file.example/path"]}"#, Origins::None),
            (r#"["https://file.example"]"#, Origins::None),
            ("{ not json", Origins::None),
        ] {
            std::fs::write(&file, text).unwrap();
            assert_eq!(origins(&c), want, "{text}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
