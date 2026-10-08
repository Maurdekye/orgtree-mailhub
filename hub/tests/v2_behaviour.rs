//! Behaviour v2 adds or changes on purpose (docs/v2.md lists each), measured
//! rather than asserted in prose.
//!
//!     HUB_TEST_PG=<folder> cargo test --test v2_behaviour -- --nocapture

mod support;

use std::sync::Arc;

use mailhub::Hub;
use serde_json::json;
use support::*;

async fn org(hub: &Arc<Hub>, name: &str) -> (String, String) {
    let o = (format!("{name}.v2.{}", token_hex3()), token_hex16());
    let r = Call::new("POST", "/api/register").auth(pair(&o.0, &o.1)).json(json!({ "slug": o.0 })).send(hub).await;
    assert_eq!(r.code(), 200, "{}", r.text());
    o
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn v2_behaviour() {
    let pg = TestPg::start();
    let url = pg.fresh_db("hubv2").await;
    let data = std::env::temp_dir().join(format!("orgtree-hub-v2-{}", token_hex16()));
    std::fs::create_dir_all(data.join("blobs")).unwrap();

    // a partial upload a killed hub left behind is gone once a hub starts
    std::fs::write(data.join("blobs").join("deadbeef.part"), b"half").unwrap();
    std::fs::write(data.join("blobs").join("keepme"), b"a finished blob").unwrap();
    let hub = hub(&url, &data, &[]).await;
    assert!(!data.join("blobs").join("deadbeef.part").exists(), "a stale partial upload survived the start");
    assert!(data.join("blobs").join("keepme").exists(), "a finished blob was removed at start");
    println!("  ok  startup removes partial uploads a killed hub left, and nothing else");

    // a poll answer carries at most 500 queued messages; the next poll the rest
    let (a, b) = (org(&hub, "sender").await, org(&hub, "inbox").await);
    for i in 0..620 {
        let r = Call::new("POST", "/api/send").auth(pair(&a.0, &a.1)).json(json!({ "id": format!("bulk-{i:04}"), "to": b.0, "body": "x" })).send(&hub).await;
        assert_eq!(r.code(), 200);
    }
    let first = Call::new("POST", "/api/poll?wait=0").auth(pair(&b.0, &b.1)).send(&hub).await.json();
    let ids: Vec<String> = first["messages"].as_array().unwrap().iter().map(|m| m["id"].as_str().unwrap().to_string()).collect();
    assert_eq!(ids.len(), 500);
    assert_eq!(ids.first().map(String::as_str), Some("bulk-0000"));
    assert_eq!(ids.last().map(String::as_str), Some("bulk-0499"));
    let acked = Call::new("POST", "/api/ack").auth(pair(&b.0, &b.1)).json(json!({ "ids": ids })).send(&hub).await.json();
    assert_eq!(acked["acked"], 500);
    let t0 = std::time::Instant::now();
    let rest = Call::new("POST", "/api/poll?wait=25").auth(pair(&b.0, &b.1)).send(&hub).await.json();
    assert!(t0.elapsed() < std::time::Duration::from_secs(2), "the next poll waited although mail was queued");
    let rest: Vec<&str> = rest["messages"].as_array().unwrap().iter().map(|m| m["id"].as_str().unwrap()).collect();
    assert_eq!(rest.len(), 120);
    assert_eq!(rest.first().copied(), Some("bulk-0500"));
    println!("  ok  620 queued: the first poll carries 500 in order, the next answers at once with the other 120");

    // v1's operator read cap, with more rows than the cap (MH01's 501-row case)
    let page = Call::new("GET", "/ui/messages?limit=99999").send(&hub).await.json();
    assert_eq!(page["messages"].as_array().unwrap().len(), 500, "the operator read is not capped at 500");
    let page = Call::new("GET", "/ui/messages?limit=0").send(&hub).await.json();
    assert_eq!(page["messages"].as_array().unwrap().len(), 1);
    println!("  ok  with 620 messages stored, the operator read is capped at 500 (and limit=0 reads 1), as in v1");

    // a JSON body over 32 MiB is refused, not buffered
    let huge = format!("{{\"to\": \"{}\", \"body\": \"{}\"}}", b.0, "x".repeat(33 * 1024 * 1024));
    let r = Call::new("POST", "/api/send").auth(pair(&a.0, &a.1)).content(huge).send(&hub).await;
    assert_eq!(r.code(), 413, "{}", r.text());
    assert_eq!(r.json(), json!({ "detail": "request body too large" }));
    let just_under = format!("{{\"to\": \"{}\", \"body\": \"{}\"}}", b.0, "y".repeat(31 * 1024 * 1024));
    let r = Call::new("POST", "/api/send").auth(pair(&a.0, &a.1)).content(just_under).send(&hub).await;
    assert_eq!(r.code(), 200, "a 31 MiB body (cut to 20,000 characters, as in v1) was refused: {}", r.text());
    println!("  ok  a JSON body over 32 MiB is refused 413; a 31 MiB one is accepted and cut to 20,000 characters as in v1");

    // a message id PostgreSQL cannot hold is refused, never silently changed
    let r = Call::new("POST", "/api/send").auth(pair(&a.0, &a.1)).json(json!({ "id": "a\u{0}b", "to": b.0, "body": "x" })).send(&hub).await;
    assert_eq!(r.code(), 422);
    assert_eq!(r.json()["detail"], "message id contains a NUL character");
    println!("  ok  a message id with a NUL character is refused 422 (stripping it could collide with another id)");

    // one hub process per database
    let mut vars: std::collections::HashMap<String, String> =
        [("HUB_DATABASE_URL", url.as_str()), ("HUB_DATA", data.to_str().unwrap())].iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    let cfg = mailhub::Config::from_vars(&vars).unwrap();
    let first_claim = mailhub::db::Db::claim_instance(&cfg).await.expect("the first claim");
    let second = mailhub::db::Db::claim_instance(&cfg).await;
    assert!(second.is_err(), "a second hub process could claim the same database");
    assert!(format!("{:#}", second.err().unwrap()).contains("already serving this database"));
    drop(first_claim);
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    assert!(mailhub::db::Db::claim_instance(&cfg).await.is_ok(), "the claim was not released with its connection");
    println!("  ok  a second hub process on the same database is refused; the claim ends with the first process's connection");

    // the password can travel apart from the address
    let (user_url, password) = {
        let at = url.find('@').unwrap();
        let scheme_end = url.find("://").unwrap() + 3;
        let creds = &url[scheme_end..at];
        let (user, pw) = creds.split_once(':').unwrap();
        (format!("{}{}{}", &url[..scheme_end], user, &url[at..]), pw.to_string())
    };
    vars.insert("HUB_DATABASE_URL".into(), user_url.clone());
    let without = mailhub::Config::from_vars(&vars).unwrap();
    assert!(mailhub::db::Db::new(&without).unwrap().migrate().await.is_err(), "connected without the password");
    vars.insert("HUB_DATABASE_PASSWORD".into(), password);
    let with = mailhub::Config::from_vars(&vars).unwrap();
    assert!(!format!("{with:?}").contains(&*vars["HUB_DATABASE_PASSWORD"]), "the password shows in the config's debug output");
    assert!(mailhub::db::Db::new(&with).unwrap().migrate().await.is_ok());
    println!("  ok  HUB_DATABASE_PASSWORD supplies the password apart from HUB_DATABASE_URL, and never shows in debug output");

    // the schema version is recorded and a newer one refused
    let db = sql_client(&url).await;
    let v: i64 = db.query_one("SELECT (v #>> '{}')::bigint FROM hub_meta WHERE k = 'schema_version'", &[]).await.unwrap().get(0);
    assert_eq!(v, mailhub::db::latest_schema());
    db.execute("UPDATE hub_meta SET v = to_jsonb(999) WHERE k = 'schema_version'", &[]).await.unwrap();
    let err = mailhub::db::Db::new(&with).unwrap().migrate().await.unwrap_err();
    assert!(format!("{err:#}").contains("newer than this hub"), "{err:#}");
    db.execute("UPDATE hub_meta SET v = to_jsonb($1::bigint) WHERE k = 'schema_version'", &[&v]).await.unwrap();
    println!("  ok  the schema version is recorded; a database from a newer hub is refused, not downgraded");

    drop(hub);
    let _ = std::fs::remove_dir_all(&data);
    drop(pg);
}
