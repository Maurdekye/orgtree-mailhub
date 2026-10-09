//! Where the relay-only door listens, in /healthz (v2.0.1): the main port
//! names it, so a client on the same machine (Hubchat linking a phone) can
//! put that address in a setup code instead of guessing; the door's own
//! /healthz never does.
//!
//!     HUB_TEST_PG=<folder> cargo test --test door_address -- --nocapture

mod support;

use std::future::IntoFuture;
use std::net::SocketAddr;

use mailhub::server;
use serde_json::{json, Value};
use support::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// GET over a real connection (HTTP/1.0, so the hub closes it when done).
async fn get(addr: SocketAddr, path: &str) -> (u16, Value) {
    let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
    s.write_all(format!("GET {path} HTTP/1.0\r\nHost: 127.0.0.1\r\n\r\n").as_bytes()).await.unwrap();
    let mut answer = Vec::new();
    s.read_to_end(&mut answer).await.unwrap();
    let answer = String::from_utf8(answer).unwrap();
    let (head, body) = answer.split_once("\r\n\r\n").unwrap();
    let code = head.split(' ').nth(1).unwrap().parse().unwrap();
    (code, serde_json::from_str(body).unwrap_or(Value::Null))
}

#[tokio::test]
async fn healthz_names_the_door_on_the_main_port_only() {
    let pg = TestPg::start();
    let url = pg.fresh_db("hubdoor").await;
    let data = std::env::temp_dir().join(format!("orgtree-hub-door-{}", token_hex16()));
    std::fs::create_dir_all(&data).unwrap();

    // --------------------------------------------------------------- no door
    let plain = hub(&url, &data, &[]).await;
    assert!(server::bind_door(&plain).await.unwrap().is_none(), "no HUB_PUBLIC, no door");
    let h = Call::new("GET", "/healthz").send(&plain).await.json();
    assert!(h["features"].as_array().unwrap().contains(&json!("door")), "{h}");
    assert!(h.get("door").is_none(), "{h}");
    println!("  ok  no door: /healthz has no door field, and its features say the hub would name one");

    // ----------------------------- a door on loopback, port 0, real listeners
    let doored = hub(&url, &data, &[("HUB_PUBLIC", "1"), ("HUB_PUBLIC_BIND", "127.0.0.1"), ("HUB_PUBLIC_PORT", "0")]).await;
    let door = server::bind_door(&doored).await.unwrap().expect("the door listens");
    let door_addr = door.local_addr().unwrap();
    assert_ne!(door_addr.port(), 0);
    let main = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let main_addr = main.local_addr().unwrap();
    tokio::spawn(axum::serve(main, server::full_app(doored.clone())).into_future());
    tokio::spawn(axum::serve(door, server::public_app(doored.clone())).into_future());

    let (code, h) = get(main_addr, "/healthz").await;
    assert_eq!(code, 200, "{h}");
    assert_eq!(h["door"], json!({ "port": door_addr.port(), "bind": "127.0.0.1" }), "{h}");
    println!("  ok  the main port names the door as it listens: 127.0.0.1, and for port 0 the port the system picked ({})", door_addr.port());

    let (code, d) = get(door_addr, "/healthz").await;
    assert_eq!(code, 200, "{d}");
    assert!(d.get("door").is_none(), "{d}");
    assert_eq!((d["ok"].clone(), d["features"].clone()), (json!(true), h["features"].clone()), "{d}");
    println!("  ok  the door's own /healthz leaves it out (same features)");

    // the harness's in-process calls take the same paths
    assert_eq!(Call::new("GET", "/healthz").send(&doored).await.json()["door"], h["door"]);
    assert!(Call::new("GET", "/healthz").public().send(&doored).await.json().get("door").is_none());

    // ------------------------------------------- a door it could not open
    let taken = hub(&url, &data, &[("HUB_PUBLIC", "1"), ("HUB_PUBLIC_BIND", "127.0.0.1"), ("HUB_PUBLIC_PORT", &door_addr.port().to_string())]).await;
    let e = server::bind_door(&taken).await.expect_err("the port is in use");
    assert!(format!("{e:#}").contains(&format!("could not listen on 127.0.0.1:{}", door_addr.port())), "{e:#}");
    assert!(Call::new("GET", "/healthz").send(&taken).await.json().get("door").is_none());
    println!("  ok  a door that could not open is never named (and the hub does not start, as before)");

    // ------------------------------------------------- a host name in the bind
    let named = hub(&url, &data, &[("HUB_PUBLIC", "1"), ("HUB_PUBLIC_BIND", "localhost"), ("HUB_PUBLIC_PORT", "0")]).await;
    let _named_door = server::bind_door(&named).await.unwrap().expect("the door listens");
    let h = Call::new("GET", "/healthz").send(&named).await.json();
    assert!(matches!(h["door"]["bind"].as_str(), Some("127.0.0.1" | "::1")), "{h}");
    println!("  ok  a host name shows as the address it resolved to ({})", h["door"]["bind"]);

    // --------------------------------- where clients reach it (v2.0.2)
    let mapped = hub(&url, &data, &[("HUB_PUBLIC", "1"), ("HUB_PUBLIC_BIND", "127.0.0.1"), ("HUB_PUBLIC_PORT", "0"), ("HUB_PUBLIC_ADVERTISE", "100.64.1.2:7378")]).await;
    let mapped_door = server::bind_door(&mapped).await.unwrap().expect("the door listens");
    let h = Call::new("GET", "/healthz").send(&mapped).await.json();
    assert_eq!(h["door"], json!({ "port": mapped_door.local_addr().unwrap().port(), "bind": "127.0.0.1", "advertise": "100.64.1.2:7378" }), "{h}");
    assert!(Call::new("GET", "/healthz").public().send(&mapped).await.json().get("door").is_none());
    let unopened = hub(&url, &data, &[("HUB_PUBLIC_ADVERTISE", "100.64.1.2:7378")]).await;
    assert!(Call::new("GET", "/healthz").send(&unopened).await.json().get("door").is_none(), "no door, no field, advertised or not");
    println!("  ok  HUB_PUBLIC_ADVERTISE shows as door.advertise beside the listener's own address (main port only; no door, no field)");

    // ------------------------------------------------------- every address
    // set directly: binding every address in a test would open a port on
    // this machine's network
    plain.door.set("0.0.0.0:7371".parse().unwrap()).unwrap();
    let h = Call::new("GET", "/healthz").send(&plain).await.json();
    assert_eq!(h["door"], json!({ "port": 7371, "bind": "0.0.0.0" }), "{h}");
    let six = hub(&url, &data, &[]).await;
    six.door.set("[::]:7378".parse().unwrap()).unwrap();
    let h = Call::new("GET", "/healthz").send(&six).await.json();
    assert_eq!(h["door"], json!({ "port": 7378, "bind": "::" }), "{h}");
    println!("  ok  every address shows as 0.0.0.0 (IPv4) or :: (IPv6)");

    let _ = std::fs::remove_dir_all(&data);
    drop(pg);
}
