//! PostgreSQL target writer (4.2). Owner: copy-data.
//!
//! * One `LiveConnection` per session (tunnel lease included); statements go through the
//!   downcast `PgDatabase.client` with `simple_query` after the allow-list check (one statement).
//! * Loading: `BEGIN` (unless the executor already began the transaction for a `TRUNCATE`) ->
//!   `COPY "s"."t" ("c1", …) FROM STDIN (FORMAT csv, HEADER false, DELIMITER ',', NULL '',
//!   QUOTE '"', ESCAPE '"')` via `client.copy_in`, the raw CSV bytes of the source's `COPY TO
//!   STDOUT` passed in unchanged -> `COMMIT`. Failure or cancel drops the sink (CopyFail) and
//!   rolls back: a table load is one transaction.
//! * `cancel_running` sends a protocol cancel request.

use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use bytes::Bytes;
use futures::SinkExt;
use tokio_util::sync::CancellationToken;

use crate::connect::LiveConnection;
use crate::copy::guard;
use crate::copy::types::*;
use crate::copy::{copy_fail, CODE_TARGET_LOCK_TIMEOUT};
use crate::error::{CoreError, Result};
use crate::service::BulkDb;
use crate::types::Connection;

/// `"name"` with `"` doubled.
pub fn qi(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// `"s"."t"` (or `"t"`).
pub fn qualified(t: &TargetTable) -> String {
    match &t.schema {
        Some(s) => format!("{}.{}", qi(s), qi(&t.name)),
        None => qi(&t.name),
    }
}

/// The load statement of a table (4.2).
pub fn copy_in_sql(spec: &TableDataSpec) -> String {
    let cols: Vec<String> = spec.columns.iter().map(|c| qi(&c.target)).collect();
    format!("COPY {} ({}) FROM STDIN (FORMAT csv, HEADER false, DELIMITER ',', NULL '', QUOTE '\"', ESCAPE '\"')", qualified(&spec.target), cols.join(", "))
}

/// The session settings of PG write sessions (4.1 / 4.2), in order.
pub fn write_session_sql(lock_timeout_s: u32) -> Vec<String> {
    vec![
        "SET statement_timeout = 0".to_string(),
        "SET idle_in_transaction_session_timeout = 0".to_string(),
        format!("SET lock_timeout = '{}s'", lock_timeout_s.clamp(1, 600)),
        "SET DateStyle = ISO, YMD".to_string(),
        "SET IntervalStyle = postgres".to_string(),
        "SET extra_float_digits = 3".to_string(),
        "SET bytea_output = hex".to_string(),
        "SET TimeZone = 'UTC'".to_string(),
        "SET client_encoding = 'UTF8'".to_string(),
    ]
}

/// The session settings of PG source data sessions (4.1), `client_encoding` per the SQL_ASCII
/// transcoding case of 3.5.
pub fn source_session_sql(sql_ascii: bool) -> Vec<String> {
    vec![
        "SET SESSION CHARACTERISTICS AS TRANSACTION READ ONLY".to_string(),
        "SET row_security = off".to_string(),
        "SET DateStyle = ISO, YMD".to_string(),
        "SET IntervalStyle = postgres".to_string(),
        "SET extra_float_digits = 3".to_string(),
        "SET bytea_output = hex".to_string(),
        "SET TimeZone = 'UTC'".to_string(),
        format!("SET client_encoding = '{}'", if sql_ascii { "SQL_ASCII" } else { "UTF8" }),
    ]
}

#[cfg(feature = "postgres")]
fn map_err(e: tokio_postgres::Error) -> CoreError {
    let lock = e.code().map(|c| c.code() == "55P03").unwrap_or(false);
    let mapped = crate::source::postgres::map_pg_error(e);
    if lock {
        copy_fail(CODE_TARGET_LOCK_TIMEOUT, format!("the target table is in use; try again later ({mapped})"))
    } else {
        mapped
    }
}

/// PostgreSQL copy target session.
pub struct PgTarget {
    facts: TargetFacts,
    live: Option<LiveConnection>,
    write: bool,
    in_tx: AtomicBool,
}

impl PgTarget {
    /// A session without a connection (tests): every statement fails.
    pub fn new(facts: TargetFacts) -> PgTarget {
        PgTarget { facts, live: None, write: false, in_tx: AtomicBool::new(false) }
    }

    /// Open a session on `ep.database` of `conn`. Read sessions run `SET SESSION
    /// CHARACTERISTICS AS TRANSACTION READ ONLY`.
    #[cfg(feature = "postgres")]
    pub async fn open(db: &BulkDb, conn: &Connection, ep: &CopyEndpoint, app_name: &str, write: bool) -> Result<PgTarget> {
        let c = super::endpoint_connection(conn, ep);
        let live = db.open_connection(&c, ep.password.clone(), app_name).await?;
        let mut t =
            PgTarget { facts: super::base_facts(CopyFamily::Postgres, "postgres", "", &ep.database, conn), live: Some(live), write, in_tx: AtomicBool::new(false) };
        if !write {
            t.simple("SET SESSION CHARACTERISTICS AS TRANSACTION READ ONLY").await?;
        }
        t.facts = t.read_facts(conn, &ep.database).await?;
        Ok(t)
    }

    #[cfg(not(feature = "postgres"))]
    pub async fn open(_db: &BulkDb, conn: &Connection, _ep: &CopyEndpoint, _app_name: &str, _write: bool) -> Result<PgTarget> {
        Err(CoreError::DriverMissing { id: conn.driver.clone(), detail: "postgres is not compiled into this build".into() })
    }

    #[cfg(feature = "postgres")]
    fn pg(&self) -> Result<&crate::source::postgres::PgDatabase> {
        let live = self.live.as_ref().ok_or_else(|| CoreError::Internal("copy: postgres target is not connected".into()))?;
        live.db().as_any().downcast_ref::<crate::source::postgres::PgDatabase>().ok_or_else(|| CoreError::Internal("copy: not a postgres connection".into()))
    }

    /// Run one statement with the simple protocol; (rows of the last result, affected rows).
    #[cfg(feature = "postgres")]
    async fn simple(&self, sql: &str) -> Result<(MetaTable, u64)> {
        use tokio_postgres::SimpleQueryMessage;
        let msgs = self.pg()?.client.simple_query(sql).await.map_err(map_err)?;
        let mut table = MetaTable::default();
        let mut affected = 0;
        for m in msgs {
            match m {
                SimpleQueryMessage::RowDescription(cols) => {
                    table.columns = cols.iter().map(|c| c.name().to_string()).collect();
                }
                SimpleQueryMessage::Row(r) => {
                    if table.columns.is_empty() {
                        table.columns = r.columns().iter().map(|c| c.name().to_string()).collect();
                    }
                    table.rows.push((0..r.len()).map(|i| r.get(i).map(str::to_string)).collect());
                }
                SimpleQueryMessage::CommandComplete(n) => affected = n,
                _ => {}
            }
        }
        Ok((table, affected))
    }

    #[cfg(not(feature = "postgres"))]
    async fn simple(&self, _sql: &str) -> Result<(MetaTable, u64)> {
        Err(CoreError::DriverMissing { id: "postgres".into(), detail: "postgres is not compiled into this build".into() })
    }

    /// Target facts (5.3); unreadable values are absent.
    async fn read_facts(&self, conn: &Connection, database: &str) -> Result<TargetFacts> {
        let (m, _) = self
            .simple(&format!(
                "SELECT current_setting('server_version') AS server_version, current_setting('server_version_num') AS server_version_num, \
                 current_setting('server_encoding') AS server_encoding, current_user AS usr, current_setting('lc_monetary') AS lc_monetary, \
                 current_setting('port') AS port, pg_is_in_recovery()::text AS pg_is_in_recovery, {} AS start_time, \
                 (SELECT rolsuper::text FROM pg_roles WHERE rolname = current_user) AS is_superuser, current_database() AS db",
                crate::copy::facts::PG_START_TIME_SQL
            ))
            .await?;
        let version = m.get(0, "server_version").unwrap_or("").to_string();
        let mut f = super::base_facts(CopyFamily::Postgres, "postgres", &version, database, conn);
        if let Some(n) = m.get(0, "server_version_num").and_then(|v| v.parse::<u32>().ok()) {
            f.major = (n / 10_000) as u16;
            f.minor = if n >= 100_000 { (n % 10_000) as u16 } else { ((n / 100) % 100) as u16 };
            f.patch = if n >= 100_000 { 0 } else { (n % 100) as u16 };
        }
        for (k, col) in [
            ("server_version_num", "server_version_num"),
            ("server_encoding", "server_encoding"),
            ("lc_monetary", "lc_monetary"),
            ("port", "port"),
            ("pg_is_in_recovery", "pg_is_in_recovery"),
            ("start_time", "start_time"),
            ("is_superuser", "is_superuser"),
        ] {
            if let Some(v) = m.get(0, col) {
                f.vars.insert(k.to_string(), v.to_string());
            }
        }
        f.user = m.get(0, "usr").map(str::to_string);
        let coll = match self.simple("SELECT datcollate, datctype, datlocprovider::text AS datlocprovider FROM pg_database WHERE datname = current_database()").await {
            Ok((m, _)) => m,
            Err(_) => self.simple("SELECT datcollate, datctype FROM pg_database WHERE datname = current_database()").await.map(|r| r.0).unwrap_or_default(),
        };
        for k in ["datcollate", "datctype", "datlocprovider"] {
            if let Some(v) = coll.get(0, k) {
                f.vars.insert(k.to_string(), v.to_string());
            }
        }
        if let Ok((m, _)) = self.simple("SELECT system_identifier::text AS system_identifier FROM pg_control_system()").await {
            if let Some(v) = m.get(0, "system_identifier") {
                f.vars.insert("system_identifier".into(), v.to_string());
            }
        }
        Ok(super::with_identity(f))
    }
}

#[async_trait]
impl TargetSession for PgTarget {
    fn facts(&self) -> &TargetFacts {
        &self.facts
    }

    async fn query_meta(&self, sql: &str) -> Result<MetaTable> {
        guard::check_read(CopyFamily::Postgres, sql)?;
        Ok(self.simple(sql).await?.0)
    }

    async fn execute(&self, sql: &str) -> Result<u64> {
        guard::check_always_rejected(sql)?;
        if !self.write {
            // The lease and read-only probes are reads; nothing else runs on a read session.
            return Err(CoreError::Internal("copy: write on a read-only postgres target session".into()));
        }
        let first = guard::leading_words(sql, 1).into_iter().next().unwrap_or_default();
        let r = self.simple(sql).await;
        match (first.as_str(), &r) {
            ("BEGIN" | "START", Ok(_)) => self.in_tx.store(true, Ordering::Relaxed),
            ("COMMIT" | "END" | "ROLLBACK", _) if !sql.to_ascii_uppercase().contains("SAVEPOINT") => self.in_tx.store(false, Ordering::Relaxed),
            _ => {}
        }
        Ok(r?.1)
    }

    #[cfg(feature = "postgres")]
    async fn load(&self, spec: &TableDataSpec, input: LoadInput, cancel: CancellationToken) -> Result<LoadOutcome> {
        if !self.write {
            return Err(CoreError::Internal("copy: load on a read-only postgres target session".into()));
        }
        let LoadInput::RawCsv(mut rx) = input else {
            return Err(CoreError::Invalid("postgres targets load the raw CSV of the source's COPY".into()));
        };
        let sql = copy_in_sql(spec);
        guard::check_statement(CopyFamily::Postgres, &sql, guard::StmtContext::Load)?;
        let pg = self.pg()?;
        if !self.in_tx.load(Ordering::Relaxed) {
            pg.client.batch_execute("BEGIN").await.map_err(map_err)?;
            self.in_tx.store(true, Ordering::Relaxed);
        }
        let run = async {
            let sink = pg.client.copy_in::<str, Bytes>(sql.as_str()).await.map_err(map_err)?;
            futures::pin_mut!(sink);
            let mut sent = 0u64;
            loop {
                let next = tokio::select! {
                    biased;
                    _ = cancel.cancelled() => return Err(CoreError::Cancelled),
                    n = rx.recv() => n,
                };
                let Some(b) = next else { break };
                sent += b.rows;
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => return Err(CoreError::Cancelled),
                    r = sink.send(b.item) => r.map_err(map_err)?,
                }
                drop(b.permit);
            }
            if cancel.is_cancelled() {
                // The reader stopped early: never commit a partial table.
                return Err(CoreError::Cancelled);
            }
            let rows = sink.as_mut().finish().await.map_err(map_err)?;
            let _ = sent;
            Ok(rows)
        };
        match run.await {
            Ok(rows) => {
                pg.client.batch_execute("COMMIT").await.map_err(map_err)?;
                self.in_tx.store(false, Ordering::Relaxed);
                Ok(LoadOutcome { rows, method: Some(LoadMethod::PgCopyCsv), ..LoadOutcome::default() })
            }
            Err(e) => {
                let _ = pg.client.batch_execute("ROLLBACK").await;
                self.in_tx.store(false, Ordering::Relaxed);
                Err(e)
            }
        }
    }

    #[cfg(not(feature = "postgres"))]
    async fn load(&self, _spec: &TableDataSpec, _input: LoadInput, _cancel: CancellationToken) -> Result<LoadOutcome> {
        Err(CoreError::DriverMissing { id: "postgres".into(), detail: "postgres is not compiled into this build".into() })
    }

    async fn count_rows(&self, t: &TargetTable) -> Result<u64> {
        let name = qualified(t);
        let schema = t.schema.clone().unwrap_or_else(|| "public".into());
        let kind = self
            .query_meta(&format!(
                "SELECT c.relkind::text AS relkind FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace WHERE n.nspname = {} AND c.relname = {}",
                crate::copy::preflight::pg_literal(&schema),
                crate::copy::preflight::pg_literal(&t.name)
            ))
            .await?;
        let only = if kind.get(0, "relkind") == Some("p") { "" } else { "ONLY " };
        let m = self.query_meta(&format!("SELECT count(*) AS n FROM {only}{name}")).await?;
        m.get(0, "n").and_then(|v| v.parse().ok()).ok_or_else(|| CoreError::Internal("copy: count returned no row".into()))
    }

    async fn cancel_running(&self) {
        #[cfg(feature = "postgres")]
        if let Ok(pg) = self.pg() {
            pg.canceller.cancel().await;
        }
    }

    /// A table load is one transaction: nothing is committed until it ends.
    fn committed_rows(&self) -> Option<u64> {
        Some(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statements() {
        let spec = TableDataSpec {
            item_id: "table:\"app\".\"t\"".into(),
            source_sql: String::new(),
            verify_sql: String::new(),
            raw_csv: true,
            transcode: None,
            target: TargetTable { schema: Some("app".into()), name: "o\"rders".into() },
            columns: vec![DataColumn {
                source: "id".into(),
                target: "id".into(),
                value: ValueKind::Integer,
                read: SourceRead::Plain,
                zero_date: ZeroDateAction::NotTemporal,
                invalid_date: ZeroDateAction::NotTemporal,
                binary_attr: false,
                max_bytes: None,
            }],
            est_rows: None,
            first_timestamp_auto: false,
            fk_checks: false,
        };
        assert_eq!(copy_in_sql(&spec), "COPY \"app\".\"o\"\"rders\" (\"id\") FROM STDIN (FORMAT csv, HEADER false, DELIMITER ',', NULL '', QUOTE '\"', ESCAPE '\"')");
        for s in write_session_sql(5).iter().chain(source_session_sql(false).iter()) {
            guard::check_statement(CopyFamily::Postgres, s, guard::StmtContext::Session).unwrap();
        }
        assert!(write_session_sql(5).contains(&"SET lock_timeout = '5s'".to_string()));
        assert!(source_session_sql(true).last().unwrap().contains("SQL_ASCII"));
    }
}
