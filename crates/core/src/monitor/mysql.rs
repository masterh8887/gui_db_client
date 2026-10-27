//! MySQL / MariaDB monitor over `information_schema.processlist`, polled from a separate
//! `bulkdb-monitor` connection (see `monitor/mod.rs`).
//!
//! * `snapshot`: the extraction session by connection id; when the job has none, by
//!   `performance_schema.session_connect_attrs` (`program_name`, when the client sent it) and
//!   finally by the `/* bulkdb-<job> */` tag the driver prepends to its SQL.
//! * `client_write_ratio`: fraction of the last ten samples whose state was
//!   `Sending to client` (the server waits for us -> network / sink bound); the ring lives on
//!   the [`MySqlDatabase`] per job.
//! * `active_sessions`: non-sleeping sessions whose statement text mentions any of the tables.
//! * `cancel_session`: `KILL QUERY <id>` from this connection.

use async_trait::async_trait;
use mysql_async::{Params, Row, Value};

use super::{JobHandle, Monitor, ServerSnapshot};
use crate::error::{CoreError, Result};
use crate::source::mysql::{row_str, row_u64, tag_like_pattern, MySqlDatabase};
use crate::types::{SessionInfo, TableRef};

const PROCESS_BY_ID_SQL: &str = "SELECT id, state, time, info, command FROM information_schema.processlist WHERE id = ?";

const PROCESS_BY_ATTR_SQL: &str = "SELECT p.id, p.state, p.time, p.info, p.command \
     FROM information_schema.processlist p \
     JOIN performance_schema.session_connect_attrs a ON a.processlist_id = p.id \
     WHERE a.attr_name = 'program_name' AND a.attr_value = ? AND p.id <> CONNECTION_ID() \
     ORDER BY p.id DESC LIMIT 1";

const PROCESS_BY_TAG_SQL: &str = "SELECT id, state, time, info, command FROM information_schema.processlist \
     WHERE info LIKE ? AND id <> CONNECTION_ID() ORDER BY id DESC LIMIT 1";

const ACTIVE_SQL: &str = "SELECT id, user, db, command, time, state, info FROM information_schema.processlist \
     WHERE command <> 'Sleep' AND command <> 'Daemon' AND info IS NOT NULL AND id <> CONNECTION_ID()";

/// Longest statement text handed to the UI.
const MAX_QUERY_CHARS: usize = 4000;

/// Thread states that mean "the server is waiting on something" rather than working.
pub fn is_wait_state(state: &str) -> bool {
    let s = state.trim();
    s.starts_with("Waiting for") || s == "Sending data"
}

/// Does the `processlist.info` text mention `table` (bare or `schema.table`, quoted or not)?
/// Matching is case-insensitive on identifier boundaries, so `orders` does not match
/// `orders_archive`.
pub fn mentions_table(info: &str, table: &TableRef) -> bool {
    let text = info.replace('`', "").to_lowercase();
    let name = table.name.to_lowercase();
    if name.is_empty() {
        return false;
    }
    let ident = |c: char| c.is_alphanumeric() || c == '_' || c == '$';
    for (at, _) in text.match_indices(&name) {
        let before = text[..at].chars().next_back();
        let after = text[at + name.len()..].chars().next();
        if after.is_some_and(ident) {
            continue;
        }
        match before {
            None => return true,
            Some('.') => {
                // Qualified: accept any qualifier when the reference has no schema, otherwise
                // the schema must match.
                let head = &text[..at - 1];
                let qualifier: String = head.chars().rev().take_while(|c| ident(*c)).collect::<Vec<_>>().into_iter().rev().collect();
                match table.schema.as_deref().filter(|s| !s.is_empty()) {
                    Some(schema) => {
                        if qualifier == schema.to_lowercase() {
                            return true;
                        }
                    }
                    None => return true,
                }
            }
            Some(c) if ident(c) => continue,
            Some(_) => return true,
        }
    }
    false
}

/// `/* bulkdb-abc */ SELECT ...` -> `bulkdb-abc` (the application name the driver tags with).
pub fn tag_of(info: &str) -> Option<String> {
    let rest = info.trim_start().strip_prefix("/*")?;
    let end = rest.find("*/")?;
    let tag = rest[..end].trim();
    (!tag.is_empty()).then(|| tag.to_string())
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(max).collect();
        t.push('…');
        t
    }
}

fn snapshot_from(db: &MySqlDatabase, job: &JobHandle, row: &Row) -> ServerSnapshot {
    let state = row_str(row, 1).filter(|s| !s.is_empty()).or_else(|| row_str(row, 4));
    let sending_to_client = state.as_deref().is_some_and(|s| s.eq_ignore_ascii_case("Sending to client"));
    let ratio = db.record_write_sample(&job.job_id, sending_to_client);
    ServerSnapshot {
        pid: row_str(row, 0),
        wait_event: state.as_deref().filter(|s| is_wait_state(s)).map(str::to_string),
        state,
        elapsed_ms: row_u64(row, 2).map(|t| t.saturating_mul(1000)),
        server_rows: None,
        temp_bytes: None,
        client_write_ratio: Some(ratio),
        found: true,
    }
}

#[async_trait]
impl Monitor for MySqlDatabase {
    async fn snapshot(&self, job: &JobHandle) -> Result<ServerSnapshot> {
        let mut row: Option<Row> = None;
        if let Some(id) = job.session_id.as_deref().and_then(|s| s.trim().parse::<u32>().ok()) {
            row = self.exec_first(PROCESS_BY_ID_SQL, Params::Positional(vec![Value::from(id)])).await?;
        } else {
            // No connection id: try the connection attribute, then the SQL comment tag. The
            // performance_schema lookup fails on servers where it is disabled; that is fine.
            match self.exec_first(PROCESS_BY_ATTR_SQL, Params::Positional(vec![Value::from(job.application_name.as_str())])).await {
                Ok(found) => row = found,
                Err(e) => tracing::debug!("mysql monitor: session_connect_attrs lookup failed: {e}"),
            }
            if row.is_none() {
                row = self.exec_first(PROCESS_BY_TAG_SQL, Params::Positional(vec![Value::from(tag_like_pattern(&job.application_name))])).await?;
            }
        }
        match row {
            Some(r) => Ok(snapshot_from(self, job, &r)),
            None => {
                self.forget_write_samples(&job.job_id);
                Ok(ServerSnapshot { found: false, ..Default::default() })
            }
        }
    }

    async fn active_sessions(&self, touching: &[TableRef]) -> Result<Vec<SessionInfo>> {
        if touching.is_empty() {
            return Ok(vec![]);
        }
        let rows = self
            .query_map(ACTIVE_SQL, |r: Row| {
                (row_str(&r, 0).unwrap_or_default(), row_str(&r, 1), row_str(&r, 2), row_str(&r, 3), row_u64(&r, 4), row_str(&r, 5), row_str(&r, 6))
            })
            .await?;
        let mut out = Vec::new();
        for (pid, user, db, command, time, state, info) in rows {
            let Some(info) = info.filter(|i| !i.trim().is_empty()) else { continue };
            let tables: Vec<String> = touching.iter().filter(|t| mentions_table(&info, t)).map(TableRef::qualified).collect();
            if tables.is_empty() {
                continue;
            }
            let state = state.filter(|s| !s.is_empty()).or(command);
            out.push(SessionInfo {
                pid,
                user: match (user, db) {
                    (Some(u), Some(d)) if !d.is_empty() => Some(format!("{u}@{d}")),
                    (u, _) => u,
                },
                application: tag_of(&info),
                wait_event: state.as_deref().filter(|s| is_wait_state(s)).map(str::to_string),
                state,
                query: Some(truncate_chars(&info, MAX_QUERY_CHARS)),
                elapsed_ms: time.map(|t| t.saturating_mul(1000)),
                tables,
                blocking: false,
            });
        }
        Ok(out)
    }

    async fn cancel_session(&self, session_id: &str) -> Result<bool> {
        let id: u32 = session_id.trim().parse().map_err(|_| CoreError::Invalid(format!("'{session_id}' is not a MySQL connection id")))?;
        match self.query_drop(&format!("KILL QUERY {id}")).await {
            Ok(()) => Ok(true),
            // 1094: unknown thread id -> nothing to cancel.
            Err(CoreError::Query { code: Some(c), .. }) if c == "1094" => Ok(false),
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wait_states() {
        assert!(is_wait_state("Waiting for table metadata lock"));
        assert!(is_wait_state("Sending data"));
        assert!(!is_wait_state("Sending to client"));
        assert!(!is_wait_state("executing"));
        assert!(!is_wait_state(""));
    }

    #[test]
    fn table_mentions() {
        let bare = TableRef::new(None, "orders");
        assert!(mentions_table("SELECT * FROM orders WHERE id = 1", &bare));
        assert!(mentions_table("select * from `shop`.`orders`", &bare));
        assert!(mentions_table("SELECT * FROM ORDERS", &bare));
        assert!(mentions_table("/* bulkdb-x */ select o.id from orders o", &bare));
        assert!(!mentions_table("SELECT * FROM orders_archive", &bare));
        assert!(!mentions_table("SELECT * FROM old_orders", &bare));
        assert!(mentions_table("SELECT * FROM old_orders, orders", &bare));
        assert!(!mentions_table("", &bare));

        let qualified = TableRef::new(Some("shop"), "orders");
        assert!(mentions_table("select * from shop.orders", &qualified));
        assert!(mentions_table("select * from `shop`.`orders`", &qualified));
        assert!(mentions_table("select * from orders", &qualified), "bare names may be the same table via USE");
        assert!(!mentions_table("select * from other.orders", &qualified));
        assert!(!mentions_table("select * from other.orders join orders_x", &qualified));
        assert!(mentions_table("select * from other.orders join orders", &qualified));
    }

    #[test]
    fn sql_tag_extraction() {
        assert_eq!(tag_of("/* bulkdb-abc */ select 1").as_deref(), Some("bulkdb-abc"));
        assert_eq!(tag_of("  /*bulkdb-abc*/select 1").as_deref(), Some("bulkdb-abc"));
        assert_eq!(tag_of("select 1 /* bulkdb-abc */"), None);
        assert_eq!(tag_of("/* */ select 1"), None);
        assert_eq!(tag_of("/* unterminated"), None);
    }

    #[test]
    fn query_text_is_truncated() {
        assert_eq!(truncate_chars("abc", 5), "abc");
        assert_eq!(truncate_chars("abcdef", 3), "abc…");
        assert_eq!(truncate_chars("한글텍스트", 2), "한글…");
    }
}
