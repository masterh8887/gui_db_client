//! Server facts of the copy *source* (5.3 identity, replica status, versions), read on the
//! job's read-only meta session through the driver internals (the target side reads its facts
//! in `target::*::open`). Owner: copy-data.

use crate::connect::LiveConnection;
use crate::copy::types::*;
use crate::error::{CoreError, Result};
use crate::types::Connection;

/// `pg_postmaster_start_time()` as text that does not depend on the session's `TimeZone` or
/// `DateStyle` (UTC, microseconds), so two sessions of one cluster with different role or
/// connection settings give the same fallback identity (5.3). Used on both sides.
pub const PG_START_TIME_SQL: &str = "to_char(pg_postmaster_start_time() AT TIME ZONE 'UTC', 'YYYY-MM-DD HH24:MI:SS.US')";

/// One read-only statement on a source session as text rows (PG simple protocol, MySQL /
/// legacy text protocol). Not for SQLite (its identity needs no query).
pub async fn source_query(live: &LiveConnection, sql: &str) -> Result<MetaTable> {
    if crate::sqlguard::split_statements(sql).len() != 1 {
        return Err(CoreError::Internal("copy: source meta queries are single statements".into()));
    }
    let any = live.db().as_any();
    #[cfg(feature = "postgres")]
    if let Some(pg) = any.downcast_ref::<crate::source::postgres::PgDatabase>() {
        use tokio_postgres::SimpleQueryMessage;
        let msgs = pg.client.simple_query(sql).await.map_err(crate::source::postgres::map_pg_error)?;
        let mut t = MetaTable::default();
        for m in msgs {
            match m {
                SimpleQueryMessage::RowDescription(cols) => t.columns = cols.iter().map(|c| c.name().to_string()).collect(),
                SimpleQueryMessage::Row(r) => {
                    if t.columns.is_empty() {
                        t.columns = r.columns().iter().map(|c| c.name().to_string()).collect();
                    }
                    t.rows.push((0..r.len()).map(|i| r.get(i).map(str::to_string)).collect());
                }
                _ => {}
            }
        }
        return Ok(t);
    }
    #[cfg(feature = "mysql")]
    if let Some(raw) = crate::source::mysql_raw(live.db()) {
        let rows = raw.meta_query(sql).await?;
        let facts = raw.facts();
        let columns: Vec<String> = rows.columns.iter().map(|c| c.name.clone()).collect();
        let n = columns.len();
        let data = (0..rows.len()).map(|r| (0..n).map(|c| rows.text(facts, r, c)).collect()).collect();
        return Ok(MetaTable { columns, rows: data });
    }
    let _ = any;
    Err(CoreError::Internal(format!("copy: no meta queries on a {} source", live.driver_id())))
}

/// [`source_query`] raced against `cancel`: when the token fires the statement is stopped on
/// the server (PG cancel request, MySQL `KILL QUERY` through `meta_query_cancellable`) and
/// abandoned `grace` later at the latest. A long read such as an exact `COUNT(*)` must not
/// hold a cancelled job.
pub async fn source_query_cancellable(live: &LiveConnection, sql: &str, cancel: &tokio_util::sync::CancellationToken, grace: std::time::Duration) -> Result<MetaTable> {
    #[cfg(feature = "mysql")]
    if let Some(raw) = crate::source::mysql_raw(live.db()) {
        if crate::sqlguard::split_statements(sql).len() != 1 {
            return Err(CoreError::Internal("copy: source meta queries are single statements".into()));
        }
        let rows = crate::export::with_driver_cancel(cancel, grace, raw.meta_query_cancellable(sql, cancel.clone())).await?;
        if cancel.is_cancelled() {
            return Err(CoreError::Cancelled);
        }
        let facts = raw.facts();
        let columns: Vec<String> = rows.columns.iter().map(|c| c.name.clone()).collect();
        let n = columns.len();
        let data = (0..rows.len()).map(|r| (0..n).map(|c| rows.text(facts, r, c)).collect()).collect();
        return Ok(MetaTable { columns, rows: data });
    }
    let fut = source_query(live, sql);
    tokio::pin!(fut);
    tokio::select! {
        biased;
        r = &mut fut => return r,
        _ = cancel.cancelled() => {}
    }
    #[cfg(feature = "postgres")]
    if let Some(pg) = live.db().as_any().downcast_ref::<crate::source::postgres::PgDatabase>() {
        pg.cancel_query().await;
    }
    let _ = tokio::time::timeout(grace, &mut fut).await;
    Err(CoreError::Cancelled)
}

/// Run a session statement on a source session (PG `batch_execute`, MySQL `meta_exec`).
pub async fn source_exec(live: &LiveConnection, sql: &str) -> Result<()> {
    if crate::sqlguard::split_statements(sql).len() != 1 {
        return Err(CoreError::Internal("copy: source session statements are single statements".into()));
    }
    let any = live.db().as_any();
    #[cfg(feature = "postgres")]
    if let Some(pg) = any.downcast_ref::<crate::source::postgres::PgDatabase>() {
        return pg.client.batch_execute(sql).await.map_err(crate::source::postgres::map_pg_error);
    }
    #[cfg(feature = "mysql")]
    if let Some(raw) = crate::source::mysql_raw(live.db()) {
        return raw.meta_exec(sql).await;
    }
    let _ = any;
    Ok(())
}

/// Settings of a source plan / meta session (4.1), run before any catalog read.
///
/// * PG: read only, `row_security = off`, and pg_dump's deparse settings (`DateStyle = ISO, YMD`,
///   `IntervalStyle = postgres`, `extra_float_digits = 3`, `TimeZone = 'UTC'`): `pg_get_expr`,
///   `pg_get_constraintdef`, `pg_get_viewdef` and partition bounds print constants through the
///   type output functions, which follow the session settings (postgresql.conf, `ALTER
///   DATABASE | ROLE … SET`); the target's DDL sessions read them back under the same settings.
/// * MySQL: read only (5.6.5+, not MariaDB) and `time_zone = '+00:00'` (4.1.3+): `SHOW CREATE
///   TABLE` prints a TIMESTAMP column's constant DEFAULT (a UTC instant) in the session zone, and
///   every target DDL statement runs under '+00:00' (mysqldump `--tz-utc`).
pub fn meta_session_sql(family: CopyFamily, mysql_version: Option<(u16, u16, u16)>, server_version: &str) -> Vec<String> {
    match family {
        CopyFamily::Postgres => vec![
            "SET SESSION CHARACTERISTICS AS TRANSACTION READ ONLY".to_string(),
            "SET row_security = off".to_string(),
            "SET DateStyle = ISO, YMD".to_string(),
            "SET IntervalStyle = postgres".to_string(),
            "SET extra_float_digits = 3".to_string(),
            "SET TimeZone = 'UTC'".to_string(),
        ],
        CopyFamily::Mysql => {
            let v = mysql_version.unwrap_or((0, 0, 0));
            let mut out = Vec::new();
            if v >= (5, 6, 5) && !server_version.to_ascii_lowercase().contains("mariadb") {
                out.push("SET SESSION TRANSACTION READ ONLY".to_string());
            }
            if v >= (4, 1, 3) {
                out.push("SET time_zone = '+00:00'".to_string());
            }
            out
        }
        CopyFamily::Sqlite => Vec::new(),
    }
}

/// Apply [`meta_session_sql`] on a source plan / meta session.
pub async fn apply_meta_session(live: &LiveConnection, family: CopyFamily, server_version: &str) -> Result<()> {
    for sql in meta_session_sql(family, mysql_version(live), server_version) {
        source_exec(live, &sql).await?;
    }
    Ok(())
}

/// Timeouts a long source session must not inherit from the server, role or database (4.1:
/// data sessions run without a statement timeout; pg_dump does the same), given the source
/// facts and whether the merged export session sets its own statement timeout:
/// * PG: `idle_in_transaction_session_timeout = 0` (9.6+; the snapshot-holding meta session sits
///   idle in its transaction while the workers stream), `statement_timeout = 0`.
/// * MySQL 5.7.8+: `max_execution_time = 0`; MariaDB 10.1.1+: `max_statement_time = 0`.
pub fn source_timeout_sql(facts: &TargetFacts, has_statement_timeout: bool) -> Vec<String> {
    let mut out = Vec::new();
    match facts.family {
        CopyFamily::Postgres => {
            if facts.at_least(9, 6, 0) {
                out.push("SET idle_in_transaction_session_timeout = 0".to_string());
            }
            if !has_statement_timeout {
                out.push("SET statement_timeout = 0".to_string());
            }
        }
        CopyFamily::Mysql if !has_statement_timeout => {
            if facts.flavor == "mariadb" {
                if facts.at_least(10, 1, 1) {
                    out.push("SET SESSION max_statement_time = 0".to_string());
                }
            } else if facts.at_least(5, 7, 8) {
                out.push("SET SESSION max_execution_time = 0".to_string());
            }
        }
        _ => {}
    }
    out
}

/// The MySQL source version as (major, minor, patch), when the source is MySQL.
pub fn mysql_version(live: &LiveConnection) -> Option<(u16, u16, u16)> {
    #[cfg(feature = "mysql")]
    if let Some(raw) = crate::source::mysql_raw(live.db()) {
        let v = &raw.facts().version;
        return Some((v.major, v.minor, v.patch));
    }
    let _ = live;
    None
}

/// The primaries' UUIDs of every replication channel of a `SHOW REPLICA STATUS` / `SHOW SLAVE
/// STATUS` answer (one row per channel on a multi-source replica), comma-joined; `None` when
/// no channel names one.
pub fn replica_source_uuids(m: &MetaTable) -> Option<String> {
    let mut out: Vec<String> = Vec::new();
    for r in 0..m.len() {
        if let Some(u) = m.get(r, "Source_UUID").or_else(|| m.get(r, "Master_UUID")).map(str::trim).filter(|u| !u.is_empty()) {
            if !out.iter().any(|x| x.eq_ignore_ascii_case(u)) {
                out.push(u.to_string());
            }
        }
    }
    if out.is_empty() {
        None
    } else {
        Some(out.join(","))
    }
}

/// Facts of the source endpoint (identity variables, version, replica status). Unreadable
/// values are absent; a failing replica status read sets `replica_status = unreadable`.
pub async fn read_source_facts(live: &LiveConnection, conn: &Connection, database: &str) -> Result<TargetFacts> {
    let family = CopyFamily::of_driver(&conn.driver).ok_or_else(|| CoreError::Invalid(format!("driver '{}' cannot be a copy source", conn.driver)))?;
    match family {
        CopyFamily::Sqlite => {
            let path = crate::copy::target::sqlite::target_path(conn).unwrap_or_default();
            let mut f = crate::copy::target::base_facts(family, "sqlite", rusqlite::version(), "main", conn);
            f.endpoint = path;
            Ok(crate::copy::target::with_identity(f))
        }
        CopyFamily::Postgres => {
            let m = source_query(
                live,
                &format!(
                    "SELECT current_setting('server_version') AS server_version, current_setting('server_encoding') AS server_encoding, \
                     current_setting('port') AS port, pg_is_in_recovery()::text AS pg_is_in_recovery, {PG_START_TIME_SQL} AS start_time, current_user AS usr"
                ),
            )
            .await?;
            let mut f = crate::copy::target::base_facts(family, "postgres", m.get(0, "server_version").unwrap_or(""), database, conn);
            for k in ["server_encoding", "port", "pg_is_in_recovery", "start_time"] {
                if let Some(v) = m.get(0, k) {
                    f.vars.insert(k.into(), v.into());
                }
            }
            f.user = m.get(0, "usr").map(str::to_string);
            if let Ok(s) = source_query(live, "SELECT system_identifier::text AS system_identifier FROM pg_control_system()").await {
                if let Some(v) = s.get(0, "system_identifier") {
                    f.vars.insert("system_identifier".into(), v.into());
                }
            }
            Ok(crate::copy::target::with_identity(f))
        }
        CopyFamily::Mysql => {
            let (ma, mi, pa) = mysql_version(live).unwrap_or((0, 0, 0));
            let mut f = crate::copy::target::base_facts(family, "mysql", &format!("{ma}.{mi}.{pa}"), database, conn);
            let var = |m: &MetaTable| m.get(0, "Value").map(str::to_string);
            for name in ["datadir", "read_only", "port", "hostname", "server_uuid", "group_replication_group_name", "version", "character_set", "character_set_server"] {
                if let Ok(m) = source_query(live, &format!("SHOW VARIABLES LIKE '{name}'")).await {
                    if let Some(v) = var(&m) {
                        f.vars.insert(name.into(), v);
                    }
                }
            }
            if let Some(v) = f.vars.get("version").cloned() {
                if v.to_ascii_lowercase().contains("mariadb") {
                    f.flavor = "mariadb".into();
                }
                f.version = v;
            }
            if let Ok(m) = source_query(live, "SHOW STATUS LIKE 'Uptime'").await {
                if let Some(v) = var(&m) {
                    f.vars.insert("uptime".into(), v);
                }
            }
            if let Ok(m) = source_query(live, "SELECT UNIX_TIMESTAMP() AS now").await {
                if let Some(v) = m.get(0, "now") {
                    f.vars.insert("now".into(), v.into());
                }
            }
            // Replica status (5.3): 8.0.22+ SHOW REPLICA STATUS, else SHOW SLAVE STATUS.
            let replica = if (ma, mi, pa) >= (8, 0, 22) { source_query(live, "SHOW REPLICA STATUS").await } else { source_query(live, "SHOW SLAVE STATUS").await };
            match replica {
                Ok(m) => {
                    if let Some(list) = replica_source_uuids(&m) {
                        f.vars.insert("replica_source_uuid".into(), list);
                    }
                }
                Err(_) => {
                    f.vars.insert("replica_status".into(), "unreadable".into());
                }
            }
            Ok(crate::copy::target::with_identity(f))
        }
    }
}
