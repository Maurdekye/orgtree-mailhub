//! Web apps: which pages may call the hub from a browser (`HUB_WEB_ORIGINS`,
//! the runtime file's `web_origins`; Hubchat Web, Orgtree docket
//! hubchat-web-self-hostable-container-serving-hubc, spec R2.3).
//!
//! A browser only lets a page read the hub's answers when the hub names the
//! page's origin. The hub does that for the origins on its list, on `/api/*`
//! and `/healthz` only: never on the operator page, which shows all mail
//! without sign-in. With no list the hub answers byte for byte as before.
//!
//!     HUB_TEST_PG=<folder> cargo test --test web_origins -- --nocapture

mod support;

use http::HeaderMap;
use serde_json::json;
use support::*;

const CHAT: &str = "https://chat.example.com";
const OTHER: &str = "https://other.example:8443";
const EVIL: &str = "https://evil.example";

const ALLOW_METHODS: &str = "GET, POST, PATCH, DELETE";
const ALLOW_HEADERS: &str = "X-Org-Auth, Content-Type, Range, If-Range";
const EXPOSE: &str = "Content-Range, Content-Length, Content-Disposition, Accept-Ranges, ETag, Last-Modified, Date, Retry-After";

fn h<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|v| v.to_str().ok())
}

/// No header a browser reads for cross-origin access, and no `Vary`.
fn untouched(what: &str, headers: &HeaderMap) {
    for name in [
        "access-control-allow-origin",
        "access-control-allow-methods",
        "access-control-allow-headers",
        "access-control-expose-headers",
        "access-control-max-age",
        "access-control-allow-credentials",
        "vary",
    ] {
        assert!(headers.get(name).is_none(), "{what}: unexpected {name}: {:?}", headers.get(name));
    }
}

/// The answer names `origin`, says it depends on the asker, and exposes the
/// headers a client reads; never credentials (the hub uses no cookies).
fn shared_with(what: &str, headers: &HeaderMap, origin: &str) {
    assert_eq!(h(headers, "access-control-allow-origin"), Some(origin), "{what}");
    assert_eq!(h(headers, "vary"), Some("Origin"), "{what}");
    assert_eq!(h(headers, "access-control-expose-headers"), Some(EXPOSE), "{what}");
    assert!(headers.get("access-control-allow-credentials").is_none(), "{what}");
}

/// Refused for this asker: nothing a browser would accept, but the answer
/// still says it depends on who asked.
fn not_shared(what: &str, headers: &HeaderMap) {
    assert!(headers.get("access-control-allow-origin").is_none(), "{what}: {:?}", headers.get("access-control-allow-origin"));
    assert!(headers.get("access-control-allow-methods").is_none(), "{what}");
    assert!(headers.get("access-control-expose-headers").is_none(), "{what}");
    assert_eq!(h(headers, "vary"), Some("Origin"), "{what}");
}

fn preflight<'a>(path: &'a str, origin: &str, method: &str) -> Call<'a> {
    Call::new("OPTIONS", path)
        .header("origin", origin)
        .header("access-control-request-method", method)
        .header("access-control-request-headers", "content-type,x-org-auth")
}

fn data_dir(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("orgtree-hub-web-{tag}-{}", token_hex16()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[tokio::test]
async fn without_a_list_nothing_changes() {
    let pg = TestPg::start();
    let url = pg.fresh_db("hubweb0").await;
    let data = data_dir("none");
    for extra in [&[][..], &[("HUB_WEB_ORIGINS", "   ")][..]] {
        let hub = hub(&url, &data, extra).await;
        let r = Call::new("GET", "/healthz").header("origin", CHAT).send(&hub).await;
        assert_eq!(r.code(), 200);
        untouched("healthz", &r.headers);
        // a preflight is what it always was: a method this path does not take
        let r = preflight("/api/sync", CHAT, "POST").send(&hub).await;
        assert_eq!(r.code(), 405, "{}", r.text());
        assert_eq!(h(&r.headers, "allow"), Some("POST"));
        assert_eq!(r.json(), json!({ "detail": "Method Not Allowed" }));
        untouched("preflight", &r.headers);
        let r = Call::new("POST", "/api/sync").header("origin", CHAT).json(json!({ "device_id": "d1", "wait": 0 })).send(&hub).await;
        assert_eq!(r.code(), 401, "{}", r.text());
        untouched("a refusal", &r.headers);
    }
    println!("  ok  no list (unset or blank): no header is added and OPTIONS is still 405");
    let _ = std::fs::remove_dir_all(&data);
}

#[tokio::test]
async fn a_listed_origin_may_ask_and_read() {
    let pg = TestPg::start();
    let url = pg.fresh_db("hubweb1").await;
    let data = data_dir("list");
    // spaces, an upper-case scheme and host, a default port and a closing
    // slash are all the same origin as a browser writes it
    let hub = hub(&url, &data, &[("HUB_WEB_ORIGINS", &format!(" HTTPS://Chat.Example.com:443/ , {OTHER}"))]).await;

    // ------------------------------------------------------- the preflight
    for (path, method, public) in [
        ("/api/sync", "POST", false),
        ("/api/uploads/abc?offset=0", "PATCH", false),
        ("/api/messages/some-id", "DELETE", false),
        ("/healthz", "GET", false),
        ("/api/sync", "POST", true),
    ] {
        let mut call = preflight(path, CHAT, method);
        if public {
            call = call.public();
        }
        let r = call.send(&hub).await;
        assert_eq!(r.code(), 204, "{path}: {}", r.text());
        assert!(r.body.is_empty(), "{path}: a preflight answer has no body");
        assert_eq!(h(&r.headers, "access-control-allow-origin"), Some(CHAT), "{path}");
        assert_eq!(h(&r.headers, "access-control-allow-methods"), Some(ALLOW_METHODS), "{path}");
        assert_eq!(h(&r.headers, "access-control-allow-headers"), Some(ALLOW_HEADERS), "{path}");
        assert_eq!(h(&r.headers, "access-control-max-age"), Some("86400"), "{path}");
        assert_eq!(h(&r.headers, "vary"), Some("Origin"), "{path}");
        assert!(r.headers.get("access-control-allow-credentials").is_none(), "{path}");
    }
    let r = preflight("/api/sync", OTHER, "POST").send(&hub).await;
    assert_eq!(r.code(), 204);
    assert_eq!(h(&r.headers, "access-control-allow-origin"), Some(OTHER), "each asker is named itself");
    println!("  ok  a listed origin's preflight: 204, the methods and headers, a day's cache, on the main port and the door");

    // ----------------------------------------------- answers, refusals too
    let me = ("me.web.aaaaaa".to_string(), token_hex16());
    let auth = pair(&me.0, &me.1);
    let r = Call::new("POST", "/api/register")
        .auth(auth.clone())
        .header("origin", CHAT)
        .json(json!({ "slug": me.0, "org_name": "Me", "username": "w", "kind": "person" }))
        .send(&hub)
        .await;
    assert_eq!(r.code(), 200, "{}", r.text());
    shared_with("register", &r.headers, CHAT);
    let r = Call::new("POST", "/api/sync").auth(auth.clone()).header("origin", CHAT).json(json!({ "device_id": "d1", "wait": 0 })).public().send(&hub).await;
    assert_eq!(r.code(), 200, "{}", r.text());
    shared_with("sync through the door", &r.headers, CHAT);
    let r = Call::new("GET", "/healthz").header("origin", OTHER).send(&hub).await;
    assert_eq!(r.code(), 200);
    shared_with("healthz", &r.headers, OTHER);
    let r = Call::new("POST", "/api/sync").header("origin", CHAT).json(json!({ "device_id": "d1", "wait": 0 })).send(&hub).await;
    assert_eq!(r.code(), 401, "{}", r.text());
    shared_with("a 401", &r.headers, CHAT);
    let r = Call::new("GET", "/api/no-such-route").header("origin", CHAT).send(&hub).await;
    assert_eq!(r.code(), 404);
    shared_with("a 404 under /api/", &r.headers, CHAT);
    let r = Call::new("GET", "/api/sync").header("origin", CHAT).send(&hub).await;
    assert_eq!(r.code(), 405);
    assert_eq!(h(&r.headers, "allow"), Some("POST"));
    shared_with("a 405", &r.headers, CHAT);
    // OPTIONS that is not a preflight (no Access-Control-Request-Method)
    let r = Call::new("OPTIONS", "/api/sync").header("origin", CHAT).send(&hub).await;
    assert_eq!(r.code(), 405, "{}", r.text());
    shared_with("a bare OPTIONS", &r.headers, CHAT);
    println!("  ok  a listed origin reads answers and refusals alike");

    // --------------------------------------- a client that is not a browser
    let plain = support::hub(&url, &data, &[]).await;
    let with_list = Call::new("GET", "/healthz").send(&hub).await;
    let without = Call::new("GET", "/healthz").send(&plain).await;
    untouched("no Origin header", &with_list.headers);
    assert_eq!(
        with_list.headers.keys().map(|k| k.as_str().to_string()).collect::<Vec<_>>(),
        without.headers.keys().map(|k| k.as_str().to_string()).collect::<Vec<_>>(),
        "a request without Origin gets the headers it always got"
    );
    println!("  ok  a request without an Origin header is answered exactly as before");

    // ------------------------------------------------------ everybody else
    let r = Call::new("GET", "/healthz").header("origin", EVIL).send(&hub).await;
    assert_eq!(r.code(), 200);
    not_shared("another origin", &r.headers);
    let r = preflight("/api/sync", EVIL, "POST").send(&hub).await;
    assert_eq!(r.code(), 405, "{}", r.text());
    assert_eq!(h(&r.headers, "allow"), Some("POST"));
    not_shared("another origin's preflight", &r.headers);
    for near in ["http://chat.example.com", "https://chat.example.com:8443", "https://chat.example.com.evil.example", "https://sub.chat.example.com", "null"] {
        let r = Call::new("GET", "/healthz").header("origin", near).send(&hub).await;
        not_shared(near, &r.headers);
    }
    println!("  ok  an origin not on the list (another scheme, port or host; null) is told nothing");

    // ------------------------------------------------- the operator page
    for path in ["/", "/ui/data", "/ui/messages"] {
        let r = Call::new("GET", path).header("origin", CHAT).send(&hub).await;
        assert_eq!(r.code(), 200, "{path}");
        untouched(path, &r.headers);
        let r = preflight(path, CHAT, "GET").send(&hub).await;
        assert_eq!(r.code(), 405, "{path}: {}", r.text());
        untouched(path, &r.headers);
    }
    println!("  ok  the operator page and its data are never shared, even with a listed origin");

    // -------------------------------------------- a WebSocket handshake
    let r = Call::new("GET", "/api/sync").header("origin", CHAT).header("upgrade", "websocket").send(&hub).await;
    assert_eq!(r.code(), 403);
    assert!(r.body.is_empty());
    untouched("a WebSocket handshake", &r.headers);
    println!("  ok  a WebSocket handshake is refused as before");
    let _ = std::fs::remove_dir_all(&data);
}

#[tokio::test]
async fn any_origin_names_the_asker_and_still_spares_the_operator_page() {
    let pg = TestPg::start();
    let url = pg.fresh_db("hubweb2").await;
    let data = data_dir("any");
    let hub = hub(&url, &data, &[("HUB_WEB_ORIGINS", "*")]).await;
    for origin in [CHAT, EVIL, "http://localhost:5173"] {
        let r = Call::new("GET", "/healthz").header("origin", origin).send(&hub).await;
        shared_with(origin, &r.headers, origin);
        let r = preflight("/api/send", origin, "POST").send(&hub).await;
        assert_eq!(r.code(), 204, "{origin}");
        assert_eq!(h(&r.headers, "access-control-allow-origin"), Some(origin), "the asker, never a literal *");
    }
    // a page without an origin of its own (a sandboxed frame, a file)
    let r = Call::new("GET", "/healthz").header("origin", "null").send(&hub).await;
    not_shared("null", &r.headers);
    let r = preflight("/api/send", "null", "POST").send(&hub).await;
    assert_eq!(r.code(), 405);
    for path in ["/", "/ui/data", "/ui/messages"] {
        let r = Call::new("GET", path).header("origin", EVIL).send(&hub).await;
        assert_eq!(r.code(), 200, "{path}");
        untouched(path, &r.headers);
    }
    println!("  ok  *: every origin is named itself, null is not, the operator page is not shared");
    let _ = std::fs::remove_dir_all(&data);
}

#[tokio::test]
async fn the_runtime_file_wins_and_changes_without_a_restart() {
    let pg = TestPg::start();
    let url = pg.fresh_db("hubweb3").await;
    let data = data_dir("runtime");
    let file = data.join("runtime.json");
    let write = |v: serde_json::Value| std::fs::write(&file, serde_json::to_vec(&v).unwrap()).unwrap();
    let hub = hub(&url, &data, &[("HUB_WEB_ORIGINS", CHAT), ("HUB_RUNTIME_CONFIG_FILE", file.to_str().unwrap())]).await;
    let allowed = |origin: &'static str| {
        let hub = hub.clone();
        async move {
            // a route that never reads the runtime file itself (/healthz reads its upload limit)
            let r = Call::new("GET", "/api/roster").header("origin", origin).send(&hub).await;
            assert_eq!(r.code(), 401, "{}", r.text());
            h(&r.headers, "access-control-allow-origin").map(str::to_string) == Some(origin.to_string())
        }
    };

    // no file yet: the environment's list
    assert!(allowed(CHAT).await);
    assert!(!allowed(OTHER).await);
    // a file without the key: still the environment's list
    write(json!({ "max_attachment_bytes": 1048576 }));
    assert!(allowed(CHAT).await);
    assert!(!allowed(OTHER).await);
    // the file's list replaces the environment's
    write(json!({ "max_attachment_bytes": 1048576, "web_origins": [OTHER] }));
    assert!(allowed(OTHER).await);
    assert!(!allowed(CHAT).await, "the file's list wins over HUB_WEB_ORIGINS");
    // and changes at once
    write(json!({ "max_attachment_bytes": 1048576, "web_origins": [" HTTPS://Chat.Example.com/ ", EVIL] }));
    assert!(allowed(CHAT).await);
    assert!(allowed(EVIL).await);
    assert!(!allowed(OTHER).await);
    write(json!({ "max_attachment_bytes": 1048576, "web_origins": ["*"] }));
    assert!(allowed(OTHER).await);
    // an empty list allows nobody, and still wins
    write(json!({ "max_attachment_bytes": 1048576, "web_origins": [] }));
    assert!(!allowed(CHAT).await);
    let r = Call::new("GET", "/healthz").header("origin", CHAT).send(&hub).await;
    untouched("an empty list is no list", &r.headers);
    // anything that is not a list of origins allows nobody
    for bad in [json!("https://chat.example.com"), json!([CHAT, 7]), json!([CHAT, "ftp://files.example"]), json!([CHAT, "https://chat.example.com/app"]), json!(true)] {
        write(json!({ "max_attachment_bytes": 1048576, "web_origins": bad }));
        assert!(!allowed(CHAT).await, "{bad}");
        let r = preflight("/api/sync", CHAT, "POST").send(&hub).await;
        assert_eq!(r.code(), 405, "{bad}");
    }
    // a file that is not JSON: nobody (never wider than asked)
    std::fs::write(&file, b"{ not json").unwrap();
    assert!(!allowed(CHAT).await);
    // back to a good file
    write(json!({ "max_attachment_bytes": 1048576, "web_origins": [CHAT] }));
    assert!(allowed(CHAT).await);
    println!("  ok  the runtime file's web_origins wins, is read at each request, and anything invalid allows nobody");
    let _ = std::fs::remove_dir_all(&data);
}

#[test]
fn a_bad_list_refuses_to_start() {
    let vars = |v: &str| -> std::collections::HashMap<String, String> {
        [("HUB_DATABASE_URL", "postgres://x@localhost/y"), ("HUB_WEB_ORIGINS", v)].iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    };
    for bad in [
        "chat.example.com",                  // no scheme
        "ftp://files.example",               // not a web page's scheme
        "https://chat.example.com/app",      // a path
        "https://chat.example.com?x=1",      // a query
        "https://chat.example.com#top",      // a fragment
        "https://user:pw@chat.example.com",  // credentials
        "https://",                          // no host
        "null",
        "https://a.example https://b.example", // a space is not a separator
    ] {
        let err = mailhub::Config::from_vars(&vars(bad)).err().unwrap_or_else(|| panic!("{bad:?} must refuse to start"));
        assert!(format!("{err:#}").contains("HUB_WEB_ORIGINS"), "{bad:?}: {err:#}");
    }
    let many = (0..65).map(|i| format!("https://h{i}.example")).collect::<Vec<_>>().join(",");
    assert!(mailhub::Config::from_vars(&vars(&many)).is_err(), "at most 64 origins");
    for good in ["", " ", "*", "https://chat.example.com", "http://localhost:5173, https://chat.example.com:8443 ,", "https://chat.example.com, *"] {
        assert!(mailhub::Config::from_vars(&vars(good)).is_ok(), "{good:?}");
    }
    println!("  ok  HUB_WEB_ORIGINS: origins only (scheme, host, optional port), at most 64, or *");
}
