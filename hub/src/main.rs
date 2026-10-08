//! `orgtree-mailhub` — the mail hub's one binary.
//!
//!     orgtree-mailhub [serve]                 run the hub (configured by HUB_* variables)
//!     orgtree-mailhub migrate                 bring the database schema up to date and exit
//!     orgtree-mailhub import-sqlite [PATH] [--merge]
//!                                             import a v1 store (default: $HUB_DATA/hub.sqlite3)
//!     orgtree-mailhub remove-address SLUG...   take addresses off the roster (as their own unregister)
//!     orgtree-mailhub healthcheck             exit 0 when the local hub answers /healthz
//!     orgtree-mailhub version

use std::path::PathBuf;
use std::process::ExitCode;

use mailhub::{import, server, Config, VERSION};

const USAGE: &str =
    "usage: orgtree-mailhub [serve | migrate | import-sqlite [PATH] [--merge] | remove-address SLUG... | healthcheck | version]";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().cloned().unwrap_or_else(|| "serve".into());
    let cmd = cmd.as_str();
    if matches!(cmd, "version" | "--version" | "-V") {
        println!("orgtree-mailhub {VERSION}");
        return ExitCode::SUCCESS;
    }
    if matches!(cmd, "help" | "--help" | "-h") {
        println!("{USAGE}");
        return ExitCode::SUCCESS;
    }
    let rt = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("orgtree-mailhub: could not start the runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    if cmd == "healthcheck" {
        let port = std::env::var("HUB_PORT").ok().and_then(|p| mailhub::wire::py_int(&p)).and_then(|p| u16::try_from(p).ok());
        let ok = rt.block_on(server::healthcheck(port.unwrap_or(mailhub::config::DEFAULT_PORT)));
        return if ok { ExitCode::SUCCESS } else { ExitCode::FAILURE };
    }
    let cfg = match Config::from_env() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("orgtree-mailhub: {e:#}");
            return ExitCode::FAILURE;
        }
    };
    let result = rt.block_on(async {
        match cmd {
            "serve" => server::serve(cfg).await,
            "migrate" => {
                mailhub::log::init_tracing(cfg.verbose);
                let db = mailhub::db::Db::new(&cfg)?;
                let v = db.migrate().await?;
                println!("schema version {v}");
                Ok(())
            }
            "import-sqlite" => {
                mailhub::log::init_tracing(cfg.verbose);
                let rest: Vec<&String> = args.iter().skip(1).collect();
                let merge = rest.iter().any(|a| a.as_str() == "--merge");
                let path = rest.iter().find(|a| !a.starts_with("--")).map(|a| PathBuf::from(a.as_str()));
                let path = match path {
                    Some(p) if p.is_dir() => p.join("hub.sqlite3"),
                    Some(p) => p,
                    None => cfg.sqlite_path(),
                };
                let db = mailhub::db::Db::new(&cfg)?;
                db.migrate().await?;
                let mode = if merge { import::Mode::Merge } else { import::Mode::Empty };
                let report = import::import(&db, &path, &cfg.blob_dir(), mode).await?;
                println!("{}", serde_json::to_string_pretty(&report.json)?);
                Ok(())
            }
            "remove-address" => {
                mailhub::log::init_tracing(cfg.verbose);
                let slugs: Vec<String> = args.iter().skip(1).cloned().collect();
                if slugs.is_empty() {
                    anyhow::bail!("name the addresses to remove\n{USAGE}");
                }
                let db = mailhub::db::Db::new(&cfg)?;
                db.migrate().await?;
                let gone = mailhub::api::sync::remove_addresses(&db, &slugs).await?;
                for s in &slugs {
                    println!("{s}: {}", if gone.contains(s) { "removed" } else { "not registered" });
                }
                Ok(())
            }
            other => Err(anyhow::anyhow!("unknown command {other:?}\n{USAGE}")),
        }
    });
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("orgtree-mailhub: {e:#}");
            ExitCode::FAILURE
        }
    }
}
