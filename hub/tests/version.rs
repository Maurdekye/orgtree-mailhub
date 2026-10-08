//! The hub reports its version to every client (user request 8 October):
//! each answer that names the hub (`name`) also carries `version`, so a
//! client sees it on the paths it already uses — registration, poll, sync,
//! roster, directory, the operator page's data and /healthz.
//!
//!     HUB_TEST_PG=<folder> cargo test --test version -- --nocapture

mod support;

use serde_json::{json, Value};
use support::*;

#[tokio::test]
async fn every_answer_that_names_the_hub_gives_its_version() {
    let pg = TestPg::start();
    let data = std::env::temp_dir().join(format!("orgtree-hub-version-{}", token_hex16()));
    std::fs::create_dir_all(&data).unwrap();
    let url = pg.fresh_db("hubversion").await;
    let hub = hub(&url, &data, &[]).await;
    let me = ("me.version.aaaaaa".to_string(), token_hex16());
    let auth = pair(&me.0, &me.1);
    let want = json!(mailhub::VERSION);
    assert_eq!(want, json!(env!("CARGO_PKG_VERSION")));

    let check = |what: &str, v: &Value| {
        assert_eq!(v["version"], want, "{what}: {v}");
        assert!(v.get("name").is_some(), "{what} names the hub: {v}");
        println!("  ok  {what} carries version {}", v["version"]);
    };
    let r = Call::new("POST", "/api/register")
        .auth(auth.clone())
        .json(json!({ "slug": me.0, "org_name": "Me", "username": "v", "kind": "person" }))
        .send(&hub)
        .await;
    assert_eq!(r.code(), 200, "{}", r.text());
    check("register", &r.json());
    check("poll", &Call::new("POST", "/api/poll?wait=0").auth(auth.clone()).send(&hub).await.json());
    check("sync", &Call::new("POST", "/api/sync").auth(auth.clone()).json(json!({ "device_id": "d1", "wait": 0 })).send(&hub).await.json());
    check("roster", &Call::new("GET", "/api/roster").auth(auth.clone()).send(&hub).await.json());
    check("directory", &Call::new("GET", "/api/directory").auth(auth.clone()).send(&hub).await.json());
    check("operator page data", &Call::new("GET", "/ui/data").send(&hub).await.json());
    check("healthz", &Call::new("GET", "/healthz").send(&hub).await.json());
    let page = Call::new("GET", "/").send(&hub).await.text();
    assert!(page.contains("d.version"), "the operator page shows the version");
    let _ = std::fs::remove_dir_all(&data);
}
