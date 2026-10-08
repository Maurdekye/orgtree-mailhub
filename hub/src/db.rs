//! PostgreSQL: the connection pool, the schema migrations, and the one place
//! every statement passes through (so verbose logging shows each one).
//!
//! The hub's tables live in their own schema, `mailhub`, so a hub can share
//! a database with other software. No statement holds a lock beyond its own
//! short transaction. Of the advisory locks below, two are startup guards
//! (one migration at a time, one hub process per database); the third
//! orders the roster's rare changes (see `ROSTER_LOCK`).

use std::time::Duration;

use anyhow::{bail, Context, Result};
use deadpool_postgres::{GenericClient, Manager, ManagerConfig, Object, Pool, RecyclingMethod, Timeouts};
use tokio_postgres::types::ToSql;
use tokio_postgres::{NoTls, Row};

use crate::config::Config;

pub const SCHEMA: &str = "mailhub";
const MIGRATE_LOCK: i64 = 0x6d61_696c_6875_6201; // "mailhub" + 1
const INSTANCE_LOCK: i64 = 0x6d61_696c_6875_6202;
/// Taken (for one short transaction) by every roster change before it draws
/// its `roster_seq`, so the roster's commit order is its seq order and a
/// directory syncing from a roster cursor never passes over a change that
/// commits late. Re-registrations that change nothing never take it.
pub const ROSTER_LOCK: i64 = 0x6d61_696c_6875_6203;

pub type Params<'a> = [&'a (dyn ToSql + Sync)];

/// The embedded migrations, in order. A database records the last one it
/// applied in `hub_meta.schema_version`.
const MIGRATIONS: &[(i64, &str, &str)] = &[
    (1, "v1 records", include_str!("../migrations/0001_v1_records.sql")),
    (2, "reply_to", include_str!("../migrations/0002_reply_to.sql")),
    (3, "sync", concat!(include_str!("../migrations/0003_sync.sql"), "\n", include_str!("../migrations/sync_backfill.sql"))),
    (4, "history", include_str!("../migrations/0004_history.sql")),
    (5, "long bodies", include_str!("../migrations/0005_long_bodies.sql")),
    (6, "transfers", include_str!("../migrations/0006_transfers.sql")),
    (7, "device keys", include_str!("../migrations/0007_device_keys.sql")),
];

/// Gives messages that have no change-log entry one (schema 3 does it for
/// the rows it finds; an import does it for the rows it adds).
const SYNC_BACKFILL: &str = include_str!("../migrations/sync_backfill.sql");

#[tracing::instrument(level = "debug", skip_all, err(level = "debug", Debug))]
pub async fn backfill_sync(c: &impl GenericClient) -> Result<(), tokio_postgres::Error> {
    tracing::debug!(target: "hub::sql", "{}", one_line(SYNC_BACKFILL));
    c.batch_execute(SYNC_BACKFILL).await
}

pub fn latest_schema() -> i64 {
    MIGRATIONS.last().map(|m| m.0).unwrap_or(0)
}

#[derive(Clone)]
pub struct Db {
    pool: Pool,
}

impl Db {
    pub fn pg_config(url: &str) -> Result<tokio_postgres::Config> {
        let mut pg: tokio_postgres::Config = url.parse().context("HUB_DATABASE_URL is not a valid PostgreSQL connection string")?;
        let options = match pg.get_options() {
            Some(o) if !o.trim().is_empty() => format!("{o} -csearch_path={SCHEMA}"),
            _ => format!("-csearch_path={SCHEMA}"),
        };
        pg.options(&options);
        if pg.get_application_name().is_none() {
            pg.application_name("orgtree-mailhub");
        }
        if pg.get_connect_timeout().is_none() {
            pg.connect_timeout(Duration::from_secs(10));
        }
        pg.keepalives(true);
        Ok(pg)
    }

    /// The connection settings a configuration names: its URL, plus the
    /// password given apart (`HUB_DATABASE_PASSWORD`) when there is one.
    pub fn config_for(cfg: &Config) -> Result<tokio_postgres::Config> {
        let mut pg = Self::pg_config(cfg.database_url())?;
        if let Some(pw) = cfg.database_password() {
            pg.password(pw);
        }
        Ok(pg)
    }

    pub fn new(cfg: &Config) -> Result<Db> {
        let pg = Self::config_for(cfg)?;
        let mgr = Manager::from_config(pg, NoTls, ManagerConfig { recycling_method: RecyclingMethod::Fast });
        let pool = Pool::builder(mgr)
            .max_size(cfg.pool_size)
            .runtime(deadpool_postgres::Runtime::Tokio1)
            .timeouts(Timeouts { wait: Some(Duration::from_secs(30)), create: Some(Duration::from_secs(10)), recycle: Some(Duration::from_secs(5)) })
            .build()
            .context("could not build the database pool")?;
        Ok(Db { pool })
    }

    pub async fn get(&self) -> Result<Object> {
        self.pool.get().await.context("no database connection")
    }

    /// Bring the schema up to date (idempotent; serialized by an advisory
    /// transaction lock so two starting hubs cannot both apply a migration).
    #[tracing::instrument(level = "debug", skip_all, ret(level = "debug"))]
    pub async fn migrate(&self) -> Result<i64> {
        let mut c = self.get().await?;
        let tx = c.transaction().await?;
        tx.execute("SELECT pg_advisory_xact_lock($1)", &[&MIGRATE_LOCK]).await?;
        tx.batch_execute(&format!("CREATE SCHEMA IF NOT EXISTS {SCHEMA}")).await?;
        let has_meta = tx
            .query_opt("SELECT 1 FROM pg_tables WHERE schemaname = $1 AND tablename = 'hub_meta'", &[&SCHEMA])
            .await?
            .is_some();
        let mut version: i64 = 0;
        if has_meta {
            if let Some(r) = tx.query_opt("SELECT (v #>> '{}')::bigint FROM hub_meta WHERE k = 'schema_version'", &[]).await? {
                version = r.get(0);
            }
        }
        if version > latest_schema() {
            bail!("the database schema is version {version}, newer than this hub understands ({}); use a newer hub", latest_schema());
        }
        let from = version;
        for (v, name, sql) in MIGRATIONS.iter().filter(|m| m.0 > from) {
            tx.batch_execute(sql).await.with_context(|| format!("schema migration {v} ({name}) failed"))?;
            tx.execute(
                "INSERT INTO hub_meta (k, v) VALUES ('schema_version', to_jsonb($1::bigint))
                 ON CONFLICT (k) DO UPDATE SET v = EXCLUDED.v",
                &[v],
            )
            .await?;
            tracing::info!(version = v, name, "schema migration applied");
            version = *v;
        }
        tx.commit().await?;
        Ok(version)
    }

    /// Claim this database for one hub process. Presence and long-poll
    /// wake-ups live in the process, so two hubs on one database would each
    /// see half the picture. The returned client holds the claim open.
    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn claim_instance(cfg: &Config) -> Result<tokio_postgres::Client> {
        let pg = Self::config_for(cfg)?;
        let (client, conn) = pg.connect(NoTls).await.context("could not connect to the database")?;
        tokio::spawn(async move {
            if let Err(e) = conn.await {
                tracing::warn!(error = %e, "the database connection holding the hub's instance claim closed");
            }
        });
        let got: bool = client.query_one("SELECT pg_try_advisory_lock($1)", &[&INSTANCE_LOCK]).await?.get(0);
        if !got {
            bail!("another mail hub process is already serving this database");
        }
        Ok(client)
    }
}

/// Every statement goes through these, so `HUB_LOG_VERBOSE=1` shows each
/// one (target `hub::sql`); parameters are never logged.
pub async fn query(c: &impl GenericClient, sql: &str, p: &Params<'_>) -> Result<Vec<Row>, tokio_postgres::Error> {
    tracing::debug!(target: "hub::sql", "{}", one_line(sql));
    c.query(sql, p).await
}

pub async fn query_opt(c: &impl GenericClient, sql: &str, p: &Params<'_>) -> Result<Option<Row>, tokio_postgres::Error> {
    tracing::debug!(target: "hub::sql", "{}", one_line(sql));
    c.query_opt(sql, p).await
}

pub async fn query_one(c: &impl GenericClient, sql: &str, p: &Params<'_>) -> Result<Row, tokio_postgres::Error> {
    tracing::debug!(target: "hub::sql", "{}", one_line(sql));
    c.query_one(sql, p).await
}

pub async fn execute(c: &impl GenericClient, sql: &str, p: &Params<'_>) -> Result<u64, tokio_postgres::Error> {
    tracing::debug!(target: "hub::sql", "{}", one_line(sql));
    c.execute(sql, p).await
}

fn one_line(sql: &str) -> String {
    sql.split_whitespace().collect::<Vec<_>>().join(" ")
}
