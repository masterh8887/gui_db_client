//! Monitor implementation for [`PgDatabase`]: `pg_stat_activity` seen from a separate, light
//! connection (`application_name = bulkdb-monitor`) every few seconds.
//!
//! * `snapshot`: finds the extraction session by its `application_name` (`bulkdb-<job id>`)
//!   or backend pid, reads state / wait event / elapsed time, the rows the server has pushed
//!   through `COPY` (`pg_stat_progress_copy`, PostgreSQL 14+) and the database's temp bytes.
//!   A per-job ring of the last [`RING_LEN`] samples yields `client_write_ratio`: the share of
//!   samples where the backend was waiting on `ClientWrite`, i.e. the network / this machine
//!   is the bottleneck, not the server.
//! * `active_sessions`: non-idle sessions whose query text mentions one of the given tables,
//!   flagged `blocking` when another session waits on a lock they hold (`pg_blocking_pids`).
//! * `cancel_session`: `pg_cancel_backend(pid)`.
//!
//! Needs PostgreSQL 9.6+ (`wait_event`, `pg_blocking_pids`); newer columns are version gated.

use std::collections::{HashMap, VecDeque};

use async_trait::async_trait;
use tokio_postgres::Row;
use tracing::debug;

use super::{JobHandle, Monitor, ServerSnapshot};
use crate::error::{CoreError, Result};
use crate::source::postgres::{map_pg_error, PgDatabase};
use crate::types::{SessionInfo, TableRef};

/// Samples kept per job for `client_write_ratio`.
pub(crate) const RING_LEN: usize = 10;

/// The wait event that means "the server is waiting for us to read".
const CLIENT_WRITE: &str = "ClientWrite";

// ---------------------------------------------------------------------------------------------
// SQL
// ---------------------------------------------------------------------------------------------

/// Session lookup for `snapshot`: `$1` application_name, `$2` backend pid (nullable). The
/// pid match wins when both are known; `{copy_join}` / `{copy_rows}` bring in
/// `pg_stat_progress_copy` on servers that have it.
pub(crate) fn snapshot_sql(server_version_num: i32) -> String {
    let (copy_join, copy_rows) =
        if server_version_num >= 140_000 { ("LEFT JOIN pg_stat_progress_copy p ON p.pid = a.pid", "p.tuples_processed::int8") } else { ("", "NULL::int8") };
    format!(
        "SELECT a.pid::int4, \
                a.state::text, \
                a.wait_event_type::text, \
                a.wait_event::text, \
                (extract(epoch FROM (now() - a.query_start)) * 1000)::int8, \
                {copy_rows}, \
                (SELECT d.temp_bytes FROM pg_stat_database d WHERE d.datname = current_database())::int8 \
           FROM pg_stat_activity a \
           {copy_join} \
          WHERE a.application_name = $1::text OR a.pid = $2::int4 \
          ORDER BY coalesce(a.pid = $2::int4, false) DESC, a.backend_start DESC \
          LIMIT 1"
    )
}

/// Non-idle sessions whose query mentions a table (`$1` = ILIKE patterns). `blocking` is set
/// for sessions that appear in `pg_blocking_pids` of a session waiting on a lock.
pub(crate) fn active_sessions_sql(server_version_num: i32) -> String {
    let backend_filter = if server_version_num >= 100_000 { "AND a.backend_type = 'client backend'" } else { "" };
    format!(
        "WITH blockers AS ( \
            SELECT DISTINCT unnest(pg_blocking_pids(w.pid)) AS pid \
              FROM pg_stat_activity w \
             WHERE w.wait_event_type = 'Lock') \
         SELECT a.pid::int4, \
                a.usename::text, \
                a.application_name::text, \
                a.state::text, \
                a.wait_event_type::text, \
                a.wait_event::text, \
                a.query::text, \
                (extract(epoch FROM (now() - coalesce(a.query_start, a.xact_start, a.backend_start))) * 1000)::int8, \
                EXISTS (SELECT 1 FROM blockers b WHERE b.pid = a.pid) AS blocking \
           FROM pg_stat_activity a \
          WHERE a.pid <> pg_backend_pid() \
            AND a.state IS NOT NULL AND a.state <> 'idle' \
            AND a.query ILIKE ANY ($1::text[]) \
            {backend_filter} \
          ORDER BY blocking DESC, 8 DESC"
    )
}

const CANCEL_SQL: &str = "SELECT pg_cancel_backend($1::int4)";

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

/// `%text%` with `\`, `%` and `_` escaped (the default `ILIKE` escape character is `\`).
pub(crate) fn ilike_pattern(text: &str) -> String {
    let mut p = String::with_capacity(text.len() + 2);
    p.push('%');
    for c in text.chars() {
        if matches!(c, '\\' | '%' | '_') {
            p.push('\\');
        }
        p.push(c);
    }
    p.push('%');
    p
}

/// ILIKE patterns for every table: the bare name and, when a schema is known, `schema.name`.
pub(crate) fn table_patterns(touching: &[TableRef]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for t in touching {
        for text in [t.name.clone(), t.qualified()] {
            let p = ilike_pattern(&text);
            if !out.contains(&p) {
                out.push(p);
            }
        }
    }
    out
}

/// Qualified names of the tables in `touching` that `query` mentions (case-insensitive).
pub(crate) fn overlapping_tables(query: &str, touching: &[TableRef]) -> Vec<String> {
    let q = query.to_lowercase();
    touching.iter().filter(|t| q.contains(&t.name.to_lowercase())).map(TableRef::qualified).collect()
}

/// `Type:Event` when the session is waiting, `None` otherwise.
fn join_wait_event(ty: Option<String>, ev: Option<String>) -> Option<String> {
    match (ty, ev) {
        (Some(t), Some(e)) => Some(format!("{t}:{e}")),
        (None, Some(e)) => Some(e),
        _ => None,
    }
}

fn opt_u64(v: Option<i64>) -> Option<u64> {
    v.and_then(|x| u64::try_from(x).ok())
}

/// Push one sample into a job's ring and return the `ClientWrite` share so far.
pub(crate) fn ring_push(rings: &mut HashMap<String, VecDeque<bool>>, job_id: &str, client_write: bool) -> f32 {
    let ring = rings.entry(job_id.to_string()).or_insert_with(|| VecDeque::with_capacity(RING_LEN));
    if ring.len() >= RING_LEN {
        ring.pop_front();
    }
    ring.push_back(client_write);
    ring.iter().filter(|b| **b).count() as f32 / ring.len() as f32
}

fn get<'a, T: tokio_postgres::types::FromSql<'a>>(row: &'a Row, idx: usize) -> Result<T> {
    row.try_get(idx).map_err(map_pg_error)
}

fn parse_pid(session_id: &str) -> Result<i32> {
    session_id.trim().parse::<i32>().map_err(|_| CoreError::Invalid(format!("'{session_id}' is not a PostgreSQL backend pid")))
}

// ---------------------------------------------------------------------------------------------
// Monitor
// ---------------------------------------------------------------------------------------------

#[async_trait]
impl Monitor for PgDatabase {
    async fn snapshot(&self, job: &JobHandle) -> Result<ServerSnapshot> {
        let pid: Option<i32> = job.session_id.as_deref().and_then(|s| s.trim().parse().ok());
        let sql = snapshot_sql(self.server_version_num());
        // Every statement of this session goes out under its lock, so a monitor query never
        // runs inside (or aborts) an EXPLAIN transaction on the same connection.
        let rows = {
            let _guard = self.exclusive().await;
            self.client.query(sql.as_str(), &[&job.application_name, &pid]).await.map_err(map_pg_error)?
        };
        let Some(row) = rows.first() else {
            // Session gone (job finished or never visible): forget its ring.
            if let Ok(mut rings) = self.monitor_rings.lock() {
                rings.remove(&job.job_id);
            }
            return Ok(ServerSnapshot { found: false, ..Default::default() });
        };
        let wait_type: Option<String> = get(row, 2)?;
        let wait_name: Option<String> = get(row, 3)?;
        let client_write = wait_name.as_deref() == Some(CLIENT_WRITE);
        let client_write_ratio = self.monitor_rings.lock().ok().map(|mut rings| ring_push(&mut rings, &job.job_id, client_write));
        Ok(ServerSnapshot {
            pid: Some(get::<i32>(row, 0)?.to_string()),
            state: get(row, 1)?,
            wait_event: join_wait_event(wait_type, wait_name),
            elapsed_ms: opt_u64(get(row, 4)?),
            server_rows: opt_u64(get(row, 5)?),
            temp_bytes: opt_u64(get(row, 6)?),
            client_write_ratio,
            found: true,
        })
    }

    async fn active_sessions(&self, touching: &[TableRef]) -> Result<Vec<SessionInfo>> {
        let patterns = table_patterns(touching);
        if patterns.is_empty() {
            return Ok(Vec::new());
        }
        let sql = active_sessions_sql(self.server_version_num());
        let rows = {
            let _guard = self.exclusive().await;
            self.client.query(sql.as_str(), &[&patterns]).await.map_err(map_pg_error)?
        };
        rows.iter()
            .map(|r| {
                let query: Option<String> = get(r, 6)?;
                let tables = query.as_deref().map(|q| overlapping_tables(q, touching)).unwrap_or_default();
                Ok(SessionInfo {
                    pid: get::<i32>(r, 0)?.to_string(),
                    user: get(r, 1)?,
                    application: get(r, 2)?,
                    state: get(r, 3)?,
                    wait_event: join_wait_event(get(r, 4)?, get(r, 5)?),
                    query,
                    elapsed_ms: opt_u64(get(r, 7)?),
                    tables,
                    blocking: get(r, 8)?,
                })
            })
            .collect()
    }

    async fn cancel_session(&self, session_id: &str) -> Result<bool> {
        let pid = parse_pid(session_id)?;
        debug!("pg_cancel_backend({pid})");
        let row = {
            let _guard = self.exclusive().await;
            self.client.query_one(CANCEL_SQL, &[&pid]).await.map_err(map_pg_error)?
        };
        Ok(get::<Option<bool>>(&row, 0)?.unwrap_or(false))
    }
}

// ---------------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ilike_patterns_escape_wildcards() {
        assert_eq!(ilike_pattern("orders"), "%orders%");
        assert_eq!(ilike_pattern("order_items"), "%order\\_items%");
        assert_eq!(ilike_pattern("100%"), "%100\\%%");
        assert_eq!(ilike_pattern("a\\b"), "%a\\\\b%");
    }

    #[test]
    fn table_patterns_cover_bare_and_qualified_names() {
        let t = vec![TableRef::new(Some("sales"), "orders"), TableRef::new(None, "orders"), TableRef::new(Some("sales"), "items")];
        assert_eq!(table_patterns(&t), vec!["%orders%", "%sales.orders%", "%items%", "%sales.items%"]);
        assert!(table_patterns(&[]).is_empty());
    }

    #[test]
    fn overlapping_tables_are_case_insensitive_and_qualified() {
        let t = vec![TableRef::new(Some("sales"), "orders"), TableRef::new(Some("sales"), "items"), TableRef::new(None, "customers")];
        assert_eq!(overlapping_tables("SELECT * FROM Sales.ORDERS o JOIN customers c ON ...", &t), vec!["sales.orders", "customers"]);
        assert!(overlapping_tables("select 1", &t).is_empty());
    }

    #[test]
    fn wait_events_join_as_type_colon_event() {
        assert_eq!(join_wait_event(Some("Client".into()), Some("ClientWrite".into())), Some("Client:ClientWrite".into()));
        assert_eq!(join_wait_event(None, Some("ClientWrite".into())), Some("ClientWrite".into()));
        assert_eq!(join_wait_event(Some("Lock".into()), None), None);
        assert_eq!(join_wait_event(None, None), None);
    }

    #[test]
    fn ring_keeps_last_ten_samples_per_job() {
        let mut rings = HashMap::new();
        assert_eq!(ring_push(&mut rings, "a", true), 1.0);
        assert_eq!(ring_push(&mut rings, "a", false), 0.5);
        assert_eq!(ring_push(&mut rings, "b", false), 0.0);
        for _ in 0..20 {
            ring_push(&mut rings, "a", true);
        }
        assert_eq!(rings["a"].len(), RING_LEN);
        assert_eq!(ring_push(&mut rings, "a", true), 1.0);
        for _ in 0..5 {
            ring_push(&mut rings, "a", false);
        }
        assert_eq!(ring_push(&mut rings, "a", false), 0.4);
        assert_eq!(rings["b"].len(), 1);
    }

    #[test]
    fn sql_is_version_gated() {
        let v16 = snapshot_sql(160_000);
        assert!(v16.contains("pg_stat_progress_copy") && v16.contains("p.tuples_processed::int8"));
        let v13 = snapshot_sql(130_000);
        assert!(!v13.contains("pg_stat_progress_copy") && v13.contains("NULL::int8"));
        assert!(v13.contains("a.application_name = $1::text OR a.pid = $2::int4"));

        assert!(active_sessions_sql(160_000).contains("a.backend_type = 'client backend'"));
        assert!(!active_sessions_sql(90_600).contains("backend_type"));
        assert!(active_sessions_sql(160_000).contains("pg_blocking_pids"));
    }

    #[test]
    fn pid_parsing() {
        assert_eq!(parse_pid(" 4242 ").unwrap(), 4242);
        assert!(matches!(parse_pid("abc"), Err(CoreError::Invalid(_))));
        assert_eq!(opt_u64(Some(-1)), None);
    }
}
