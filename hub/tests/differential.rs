//! The v1 Python hub and this binary, side by side (tests/v2/differential.py
//! drives both over real loopback sockets and compares every answer). This
//! file only provides the throwaway database and runs the driver.
//!
//! Needs Python with fastapi, uvicorn and httpx (the reference hub's own
//! requirements), so it runs only when asked:
//!
//!     HUB_TEST_PG=<folder> cargo test --test differential -- --ignored --nocapture

mod support;

use support::TestPg;

async fn drive(mode: &str) {
    run_script("differential.py", &["--mode", mode]).await;
}

async fn run_script(script: &str, extra: &[&str]) {
    let pg = TestPg::start();
    let url = pg.fresh_db("hubdiff").await;
    let repo = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let python = std::env::var("HUB_TEST_PYTHON").unwrap_or_else(|_| "python".into());
    let status = tokio::process::Command::new(python)
        .arg(repo.join("tests").join("v2").join(script))
        .args(["--rust-bin", env!("CARGO_BIN_EXE_orgtree-mailhub"), "--database-url", &url])
        .args(extra)
        .args(std::env::var("HUB_DIFF_VERBOSE").ok().filter(|_| script == "differential.py").map(|_| "-v"))
        .status()
        .await
        .expect("run the driver");
    drop(pg);
    assert!(status.success(), "the hubs answered differently (see the report above)");
}

/// hubtool.py, the real chat client (CLI, listener, MCP server), against
/// each hub in a throwaway HOME; transcripts compared.
#[tokio::test]
#[ignore = "needs Python with the v1 hub's requirements; run with --ignored"]
async fn hubtool_works_against_both_hubs() {
    run_script("client_interop.py", &[]).await;
}

/// The same requests to both hubs, fresh.
#[tokio::test]
#[ignore = "needs Python with the v1 hub's requirements; run with --ignored"]
async fn python_and_rust_hubs_answer_alike() {
    drive("protocol").await;
}

/// A store the v1 hub built, imported by v2 at startup, read back from both.
#[tokio::test]
#[ignore = "needs Python with the v1 hub's requirements; run with --ignored"]
async fn an_imported_v1_store_reads_back_the_same() {
    drive("import").await;
}
