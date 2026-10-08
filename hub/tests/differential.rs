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

#[tokio::test]
#[ignore = "needs Python with the v1 hub's requirements; run with --ignored"]
async fn python_and_rust_hubs_answer_alike() {
    let pg = TestPg::start();
    let url = pg.fresh_db("hubdiff").await;
    let repo = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let python = std::env::var("HUB_TEST_PYTHON").unwrap_or_else(|_| "python".into());
    let status = tokio::process::Command::new(python)
        .arg(repo.join("tests").join("v2").join("differential.py"))
        .args(["--rust-bin", env!("CARGO_BIN_EXE_orgtree-mailhub"), "--database-url", &url])
        .args(std::env::var("HUB_DIFF_VERBOSE").ok().map(|_| "-v"))
        .status()
        .await
        .expect("run the differential driver");
    drop(pg);
    assert!(status.success(), "the hubs answered differently (see the report above)");
}
