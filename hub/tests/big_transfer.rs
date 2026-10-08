//! One whole 1 GiB transfer, measured, through this binary over real
//! sockets (tests/v2/big_transfer.py drives it): uploaded in one request and
//! resumably in pieces with a cut, sent, downloaded and resumed, 1 GiB + 1
//! refused, and the hub's peak memory read from the OS. Needs Python with
//! httpx and about 2 GiB of disk beside the test database:
//!
//!     HUB_TEST_PG=<folder> cargo test --test big_transfer -- --ignored --nocapture

mod support;

use support::TestPg;

#[tokio::test]
#[ignore = "moves 5 GiB through the hub; run with --ignored"]
async fn one_gib_end_to_end() {
    let pg = TestPg::start();
    let url = pg.fresh_db("hubbig").await;
    // beside the test database (HUB_TEST_PG), not in the system temp folder
    let root = std::env::var("HUB_TEST_PG").map(std::path::PathBuf::from).unwrap_or_else(|_| std::env::temp_dir());
    let data = root.join(format!("big-transfer-{}", std::process::id()));
    let repo = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let python = std::env::var("HUB_TEST_PYTHON").unwrap_or_else(|_| "python".into());
    let bin = std::env::var("HUB_BIG_BIN").unwrap_or_else(|_| env!("CARGO_BIN_EXE_orgtree-mailhub").to_string());
    let status = tokio::process::Command::new(python)
        .arg(repo.join("tests").join("v2").join("big_transfer.py"))
        .args(["--rust-bin", &bin, "--database-url", &url, "--data"])
        .arg(&data)
        .status()
        .await
        .expect("run the driver");
    let _ = std::fs::remove_dir_all(&data);
    drop(pg);
    assert!(status.success(), "the 1 GiB transfer failed (see above)");
}
