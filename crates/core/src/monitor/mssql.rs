//! Monitor implementation for `MssqlDatabase` (see monitor/mod.rs for the contract).
//!
//! Everything comes from the execution DMVs on a separate light connection:
//!
//! * `snapshot`: `sys.dm_exec_sessions` + `sys.dm_exec_requests` found by `program_name`
//!   (the job's `application_name`) or by `@@SPID`; temp usage from
//!   `sys.dm_db_session_space_usage`. The "network bound" signal is the fraction of the last
//!   [`NET_RING`] samples whose wait type was `ASYNC_NETWORK_IO` (SQL Server's "waiting for
//!   the client to read"), kept per job in `MssqlDatabase::net_waits`.
//! * `active_sessions`: requests whose SQL text (`sys.dm_exec_sql_text`) mentions one of the
//!   tables, plus the sessions that block them (`blocking_session_id`).
//! * `cancel_session`: `KILL <spid>`.
//!
//! Without `VIEW SERVER STATE` the DMVs only show the monitor's own session, so the job is
//! simply reported as not found (never an error); `sys.dm_db_session_space_usage` needs the
//! permission outright and is dropped from the query when it is denied.

use std::collections::VecDeque;

use async_trait::async_trait;
use tiberius::ToSql;

use super::{JobHandle, Monitor, ServerSnapshot};
use crate::error::{CoreError, Result};
use crate::source::mssql::{exec_batch, get_bool, get_f64, get_i64, get_string, get_u64, is_permission_error, query_rows, MssqlDatabase};
use crate::types::{SessionInfo, TableRef};

/// Samples kept per job for `client_write_ratio`.
pub const NET_RING: usize = 10;

/// Wait types that mean "the server is waiting for the client to consume rows".
const NET_WAIT_PREFIX: &str = "ASYNC_NETWORK_IO";

/// Push one sample into a job's ring and return the fraction of `true` samples.
pub fn push_ring(ring: &mut VecDeque<bool>, sample: bool) -> f32 {
    if ring.len() >= NET_RING {
        ring.pop_front();
    }
    ring.push_back(sample);
    let hits = ring.iter().filter(|b| **b).count();
    hits as f32 / ring.len() as f32
}

/// `%name%` for `LIKE ... ESCAPE '\'`: the T-SQL wildcards inside the table name are escaped
/// so `order_items` does not match `orders`-ish text through `_`, and `[` stays literal.
pub fn like_pattern(name: &str) -> String {
    let mut out = String::with_capacity(name.len() + 2);
    out.push('%');
    for ch in name.chars() {
        if matches!(ch, '%' | '_' | '[' | '\\') {
            out.push('\\');
        }
        out.push(ch);
    }
    out.push('%');
    out
}

/// Session ids are `smallint` server side; anything else is rejected before it reaches `KILL`.
pub fn parse_spid(s: &str) -> Option<i32> {
    s.trim().parse::<i32>().ok().filter(|v| *v > 0 && *v <= i16::MAX as i32)
}

/// Which of `touching` a piece of SQL text mentions (case-insensitive, schema optional).
fn tables_in_text(text: &str, touching: &[TableRef]) -> Vec<String> {
    let lower = text.to_ascii_lowercase();
    touching.iter().filter(|t| lower.contains(&t.name.to_ascii_lowercase())).map(|t| t.qualified()).collect()
}

impl MssqlDatabase {
    /// `(temp_bytes, found)` for the ring and the snapshot. `temp` toggles the
    /// `sys.dm_db_session_space_usage` join.
    fn snapshot_sql(temp: bool) -> String {
        let (temp_col, temp_join) = if temp {
            (
                "CAST(ISNULL(su.user_objects_alloc_page_count, 0) + ISNULL(su.internal_objects_alloc_page_count, 0) AS bigint) * 8192",
                "LEFT JOIN sys.dm_db_session_space_usage su ON su.session_id = s.session_id",
            )
        } else {
            ("CAST(NULL AS bigint)", "")
        };
        format!(
            "SELECT TOP 1 s.session_id, s.status AS session_status, r.status AS request_status, r.wait_type, \
                    CAST(r.wait_time AS bigint) AS wait_time, r.percent_complete, CAST(r.total_elapsed_time AS bigint) AS elapsed_ms, \
                    CAST(s.cpu_time AS bigint) AS cpu_time, CAST(r.row_count AS bigint) AS row_count, {temp_col} AS temp_bytes \
             FROM sys.dm_exec_sessions s \
             LEFT JOIN sys.dm_exec_requests r ON r.session_id = s.session_id \
             {temp_join} \
             WHERE s.session_id <> @@SPID AND (s.program_name = @P1 OR s.session_id = @P2) \
             ORDER BY CASE WHEN s.session_id = @P2 THEN 0 ELSE 1 END, r.start_time"
        )
    }
}

#[async_trait]
impl Monitor for MssqlDatabase {
    async fn snapshot(&self, job: &JobHandle) -> Result<ServerSnapshot> {
        // -1 never matches a session id, so an unknown id falls back to program_name alone.
        let spid: i32 = job.session_id.as_deref().and_then(parse_spid).unwrap_or(-1);
        let app = job.application_name.clone();
        let params: [&dyn ToSql; 2] = [&app, &spid];

        let mut guard = self.lock().await;
        let client = self.ensure_client(&mut guard).await?;
        let rows = match query_rows(client, &Self::snapshot_sql(true), &params).await {
            Ok(rows) => rows,
            Err(e) if is_permission_error(&e) => {
                tracing::debug!(target: "bulkdb::mssql", "sys.dm_db_session_space_usage not readable ({e}); monitoring without temp usage");
                query_rows(client, &Self::snapshot_sql(false), &params).await?
            }
            Err(e) => return Err(e),
        };
        drop(guard);

        let Some(row) = rows.first() else {
            // The job's session is gone (or invisible): forget its ring.
            if let Ok(mut rings) = self.net_waits.lock() {
                rings.remove(&job.job_id);
            }
            return Ok(ServerSnapshot { found: false, ..Default::default() });
        };

        let pid = get_i64(row, 0).map(|v| v.to_string());
        let request_status = get_string(row, 2).map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
        let session_status = get_string(row, 1).map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
        let wait_type = get_string(row, 3).map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
        let wait_ms = get_u64(row, 4);
        let percent = get_f64(row, 5).filter(|p| *p > 0.0);
        let elapsed_ms = get_u64(row, 6);
        let cpu_ms = get_u64(row, 7);
        let server_rows = get_u64(row, 8);
        let temp_bytes = get_u64(row, 9);

        // `running` / `runnable` / `suspended` while a request exists, else the session's own
        // `sleeping`; `percent_complete` is only reported for operations that track it.
        let state = request_status.or(session_status).map(|s| match percent {
            Some(p) => format!("{s} {p:.1}%"),
            None => s,
        });
        let net_wait = wait_type.as_deref().map(|w| w.starts_with(NET_WAIT_PREFIX)).unwrap_or(false);
        let client_write_ratio = match self.net_waits.lock() {
            Ok(mut rings) => Some(push_ring(rings.entry(job.job_id.clone()).or_default(), net_wait)),
            Err(_) => None,
        };
        tracing::trace!(
            target: "bulkdb::mssql",
            job = %job.job_id, spid = ?pid, state = ?state, wait = ?wait_type, wait_ms = ?wait_ms, cpu_ms = ?cpu_ms, rows = ?server_rows,
            "monitor sample"
        );
        Ok(ServerSnapshot { pid, state, wait_event: wait_type, elapsed_ms, server_rows, temp_bytes, client_write_ratio, found: true })
    }

    async fn active_sessions(&self, touching: &[TableRef]) -> Result<Vec<SessionInfo>> {
        if touching.is_empty() {
            return Ok(Vec::new());
        }
        let patterns: Vec<String> = touching.iter().map(|t| like_pattern(&t.name)).collect();
        let matches = |alias: &str| (1..=patterns.len()).map(|i| format!("{alias}.text LIKE @P{i} ESCAPE '\\'")).collect::<Vec<_>>().join(" OR ");
        let sql = format!(
            "SELECT s.session_id, s.login_name, s.program_name, ISNULL(r.status, s.status) AS status, r.wait_type, \
                    CAST(r.total_elapsed_time AS bigint) AS elapsed_ms, LEFT(t.text, 8000) AS sql_text, \
                    CASE WHEN EXISTS (SELECT 1 FROM sys.dm_exec_requests b WHERE b.blocking_session_id = s.session_id) THEN 1 ELSE 0 END AS is_blocker \
             FROM sys.dm_exec_sessions s \
             LEFT JOIN sys.dm_exec_requests r ON r.session_id = s.session_id \
             OUTER APPLY (SELECT TOP 1 c.most_recent_sql_handle FROM sys.dm_exec_connections c WHERE c.session_id = s.session_id) c \
             OUTER APPLY sys.dm_exec_sql_text(ISNULL(r.sql_handle, c.most_recent_sql_handle)) t \
             WHERE s.is_user_process = 1 AND s.session_id <> @@SPID \
               AND ((r.session_id IS NOT NULL AND ({touch})) \
                    OR EXISTS (SELECT 1 FROM sys.dm_exec_requests b CROSS APPLY sys.dm_exec_sql_text(b.sql_handle) bt \
                               WHERE b.blocking_session_id = s.session_id AND ({blocked}))) \
             ORDER BY s.session_id",
            touch = matches("t"),
            blocked = matches("bt"),
        );
        let params: Vec<&dyn ToSql> = patterns.iter().map(|p| p as &dyn ToSql).collect();
        let mut guard = self.lock().await;
        let client = self.ensure_client(&mut guard).await?;
        let rows = query_rows(client, &sql, &params).await?;
        drop(guard);
        Ok(rows
            .iter()
            .map(|r| {
                let query = get_string(r, 6).map(|q| q.trim().to_string()).filter(|q| !q.is_empty());
                let tables = query.as_deref().map(|q| tables_in_text(q, touching)).unwrap_or_default();
                SessionInfo {
                    pid: get_i64(r, 0).unwrap_or(0).to_string(),
                    user: get_string(r, 1),
                    application: get_string(r, 2).map(|s| s.trim().to_string()).filter(|s| !s.is_empty()),
                    state: get_string(r, 3).map(|s| s.trim().to_string()),
                    wait_event: get_string(r, 4).map(|s| s.trim().to_string()).filter(|s| !s.is_empty()),
                    query,
                    elapsed_ms: get_u64(r, 5),
                    tables,
                    blocking: get_bool(r, 7),
                }
            })
            .collect())
    }

    /// `KILL <spid>` (needs `ALTER ANY CONNECTION`). `Ok(true)` when the server accepted it.
    async fn cancel_session(&self, session_id: &str) -> Result<bool> {
        let spid = parse_spid(session_id).ok_or_else(|| CoreError::Invalid(format!("'{session_id}' is not a SQL Server session id")))?;
        let mut guard = self.lock().await;
        let client = self.ensure_client(&mut guard).await?;
        exec_batch(client, &format!("KILL {spid}")).await?;
        tracing::info!(target: "bulkdb::mssql", spid, "server session killed");
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_keeps_last_ten_samples() {
        let mut ring = VecDeque::new();
        assert_eq!(push_ring(&mut ring, true), 1.0);
        assert_eq!(push_ring(&mut ring, false), 0.5);
        for _ in 0..20 {
            push_ring(&mut ring, false);
        }
        assert_eq!(ring.len(), NET_RING);
        assert_eq!(push_ring(&mut ring, false), 0.0);
        for _ in 0..6 {
            push_ring(&mut ring, true);
        }
        assert!((push_ring(&mut ring, true) - 0.7).abs() < 1e-6);
    }

    #[test]
    fn like_pattern_escapes_wildcards() {
        assert_eq!(like_pattern("orders"), "%orders%");
        assert_eq!(like_pattern("order_items"), "%order\\_items%");
        assert_eq!(like_pattern("100%"), "%100\\%%");
        assert_eq!(like_pattern("a[b]"), "%a\\[b]%");
        assert_eq!(like_pattern("back\\slash"), "%back\\\\slash%");
    }

    #[test]
    fn spid_parsing() {
        assert_eq!(parse_spid("57"), Some(57));
        assert_eq!(parse_spid(" 57 "), Some(57));
        assert_eq!(parse_spid("0"), None);
        assert_eq!(parse_spid("-3"), None);
        assert_eq!(parse_spid("40000"), None);
        assert_eq!(parse_spid("57; DROP TABLE x"), None);
        assert_eq!(parse_spid(""), None);
    }

    #[test]
    fn tables_mentioned_in_text() {
        let touching = vec![TableRef::new(Some("dbo"), "orders"), TableRef::new(Some("sales"), "customers"), TableRef::new(None, "nothing")];
        let found = tables_in_text("SELECT * FROM dbo.Orders o JOIN [sales].[Customers] c ON c.id = o.cid", &touching);
        assert_eq!(found, vec!["dbo.orders", "sales.customers"]);
        assert!(tables_in_text("WAITFOR DELAY '00:00:01'", &touching).is_empty());
    }

    #[test]
    fn snapshot_sql_variants() {
        let with = MssqlDatabase::snapshot_sql(true);
        assert!(with.contains("sys.dm_db_session_space_usage"));
        assert!(with.contains("* 8192"));
        let without = MssqlDatabase::snapshot_sql(false);
        assert!(!without.contains("dm_db_session_space_usage"));
        assert!(without.contains("CAST(NULL AS bigint) AS temp_bytes"));
        for sql in [&with, &without] {
            assert!(sql.contains("s.program_name = @P1"));
            assert!(sql.contains("s.session_id = @P2"));
        }
    }
}
