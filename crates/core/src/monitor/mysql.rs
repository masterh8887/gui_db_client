//! MySQL / MariaDB monitor, polled from a separate `bulkdb-monitor` connection (see
//! `monitor/mod.rs`), written once over [`MysqlRawQuery`] for both MySQL clients:
//!
//! * `snapshot`: the extraction session by connection id; when the job has none, by
//!   `performance_schema.session_connect_attrs` (`program_name`, when the client sent it; 5.6+)
//!   and finally by the `/* bulkdb-<job> */` tag the driver prepends to its SQL. 5.1.7+ reads
//!   `information_schema.processlist`; older servers `SHOW FULL PROCESSLIST`, filtered here.
//! * `client_write_ratio`: fraction of the last ten samples whose state was
//!   `Sending to client` (the server waits for us -> network / sink bound); the ring lives on
//!   the connection object per job ([`MysqlRawQuery::write_samples`]).
//! * `active_sessions`: non-sleeping sessions whose statement text mentions any of the tables.
//! * `cancel_session`: `KILL QUERY <id>` (5.0+) or `KILL <id>` (4.x, ends that session) from
//!   this connection.

use async_trait::async_trait;

use super::{JobHandle, Monitor, ServerSnapshot};
use crate::error::Result;
use crate::source::mysql::MySqlDatabase;
use crate::types::{SessionInfo, TableRef};

/// Longest statement text handed to the UI.
const MAX_QUERY_CHARS: usize = 4000;

/// Thread states that mean "the server is waiting on something" rather than working.
pub fn is_wait_state(state: &str) -> bool {
    let s = state.trim();
    s.starts_with("Waiting for") || s == "Sending data"
}

/// Does the process-list `info` text mention `table` (bare or `schema.table`, quoted or not)?
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

#[async_trait]
impl Monitor for MySqlDatabase {
    async fn snapshot(&self, job: &JobHandle) -> Result<ServerSnapshot> {
        generic::snapshot(self, job).await
    }

    async fn active_sessions(&self, touching: &[TableRef]) -> Result<Vec<SessionInfo>> {
        generic::active_sessions(self, touching).await
    }

    async fn cancel_session(&self, session_id: &str) -> Result<bool> {
        generic::cancel_session(self, session_id).await
    }
}

/// Version-aware monitor over [`MysqlRawQuery`], shared by both MySQL clients (owner:
/// modern; design doc section 4, "monitor"):
///
/// * 5.1.7+: `information_schema.PROCESSLIST` (by id, then `session_connect_attrs` on
///   MySQL 5.6+ / MariaDB 10.0+, then the SQL tag).
/// * 4.0 - 5.1.6: `SHOW FULL PROCESSLIST` (`Id, User, Host, db, Command, Time, State, Info`),
///   filtered client-side (the list is small; no data).
/// * `cancel_session`: `mysql_common::kill_sql` (`KILL QUERY` on 5.0+, `KILL` on 4.x).
/// * `client_write_ratio`: `MysqlRawQuery::write_samples`.
pub mod generic {
    use super::{is_wait_state, mentions_table, tag_of, truncate_chars, MAX_QUERY_CHARS};
    use crate::error::{CoreError, Result};
    use crate::monitor::{JobHandle, ServerSnapshot};
    use crate::source::mysql::{tag_like_pattern, Flavor};
    use crate::source::mysql_common::{kill_sql, quote_str, server_error_code, MetaRows, MysqlRawQuery, ServerFacts, ER_NO_SUCH_THREAD};
    use crate::types::{SessionInfo, TableRef};

    /// One process-list entry as text.
    #[derive(Debug, Clone, Default, PartialEq, Eq)]
    pub struct ProcessRow {
        pub id: String,
        pub user: Option<String>,
        pub db: Option<String>,
        pub command: Option<String>,
        pub time: Option<u64>,
        pub state: Option<String>,
        pub info: Option<String>,
    }

    impl ProcessRow {
        fn id_num(&self) -> Option<u64> {
            self.id.trim().parse().ok()
        }
    }

    /// Process-list rows by column name (works for `information_schema.processlist` and
    /// `SHOW FULL PROCESSLIST` alike).
    pub fn process_rows(rows: &MetaRows, f: &ServerFacts) -> Vec<ProcessRow> {
        let get = |r: usize, n: &str| rows.col(n).and_then(|c| rows.text(f, r, c));
        (0..rows.len())
            .map(|r| ProcessRow {
                id: get(r, "Id").unwrap_or_default(),
                user: get(r, "User"),
                db: get(r, "db"),
                command: get(r, "Command"),
                time: rows.col("Time").and_then(|c| rows.u64(r, c)),
                state: get(r, "State"),
                info: get(r, "Info"),
            })
            .collect()
    }

    /// `SHOW FULL PROCESSLIST` (servers without `information_schema.processlist`).
    async fn show_processlist(q: &dyn MysqlRawQuery) -> Result<Vec<ProcessRow>> {
        let rows = q.meta_query("SHOW FULL PROCESSLIST").await?;
        Ok(process_rows(&rows, q.facts()))
    }

    const PROCESS_COLUMNS: &str = "id, user, db, command, time, state, info";

    fn first(rows: Vec<ProcessRow>) -> Option<ProcessRow> {
        rows.into_iter().next()
    }

    /// `session_connect_attrs` exists in `performance_schema` from MySQL 5.6 / MariaDB 10.0.
    fn has_connect_attrs(f: &ServerFacts) -> bool {
        let v = &f.version;
        if v.flavor == Flavor::MariaDb {
            v.at_least(10, 0, 0)
        } else {
            v.at_least(5, 6, 0)
        }
    }

    /// The process row of the job's session, located by id, connection attribute or SQL tag.
    async fn find_session(q: &dyn MysqlRawQuery, job: &JobHandle) -> Result<Option<ProcessRow>> {
        let f = q.facts();
        let id = job.session_id.as_deref().and_then(|s| s.trim().parse::<u32>().ok());
        if !f.caps.processlist_table {
            let tag = job.application_name.trim();
            let own = u64::from(f.session_id);
            let rows = show_processlist(q).await?;
            return Ok(match id {
                Some(id) => rows.into_iter().find(|p| p.id_num() == Some(u64::from(id))),
                None => rows
                    .into_iter()
                    .filter(|p| p.id_num() != Some(own) && p.info.as_deref().and_then(tag_of).is_some_and(|t| t == tag))
                    .max_by_key(|p| p.id_num().unwrap_or(0)),
            });
        }
        if let Some(id) = id {
            let rows = q.meta_query(&format!("SELECT {PROCESS_COLUMNS} FROM information_schema.processlist WHERE id = {id}")).await?;
            return Ok(first(process_rows(&rows, f)));
        }
        // No connection id: try the connection attribute, then the SQL comment tag. The
        // performance_schema lookup fails on servers where it is disabled; that is fine.
        if has_connect_attrs(f) {
            let sql = format!(
                "SELECT p.id, p.user, p.db, p.command, p.time, p.state, p.info \
                 FROM information_schema.processlist p \
                 JOIN performance_schema.session_connect_attrs a ON a.processlist_id = p.id \
                 WHERE a.attr_name = 'program_name' AND a.attr_value = {} AND p.id <> CONNECTION_ID() \
                 ORDER BY p.id DESC LIMIT 1",
                quote_str(&job.application_name, f.lex)
            );
            match q.meta_query(&sql).await {
                Ok(rows) => {
                    if let Some(row) = first(process_rows(&rows, f)) {
                        return Ok(Some(row));
                    }
                }
                Err(e) => tracing::debug!("mysql monitor: session_connect_attrs lookup failed: {e}"),
            }
        }
        let sql = format!(
            "SELECT {PROCESS_COLUMNS} FROM information_schema.processlist \
             WHERE info LIKE {} AND id <> CONNECTION_ID() ORDER BY id DESC LIMIT 1",
            quote_str(&tag_like_pattern(&job.application_name), f.lex)
        );
        let rows = q.meta_query(&sql).await?;
        Ok(first(process_rows(&rows, f)))
    }

    pub async fn snapshot(q: &dyn MysqlRawQuery, job: &JobHandle) -> Result<ServerSnapshot> {
        match find_session(q, job).await? {
            Some(p) => {
                let state = p.state.filter(|s| !s.is_empty()).or(p.command);
                let sending_to_client = state.as_deref().is_some_and(|s| s.eq_ignore_ascii_case("Sending to client"));
                let ratio = q.write_samples().record(&job.job_id, sending_to_client);
                Ok(ServerSnapshot {
                    pid: Some(p.id),
                    wait_event: state.as_deref().filter(|s| is_wait_state(s)).map(str::to_string),
                    state,
                    elapsed_ms: p.time.map(|t| t.saturating_mul(1000)),
                    server_rows: None,
                    temp_bytes: None,
                    client_write_ratio: Some(ratio),
                    found: true,
                })
            }
            None => {
                q.write_samples().forget(&job.job_id);
                Ok(ServerSnapshot { found: false, ..Default::default() })
            }
        }
    }

    pub async fn active_sessions(q: &dyn MysqlRawQuery, touching: &[TableRef]) -> Result<Vec<SessionInfo>> {
        if touching.is_empty() {
            return Ok(vec![]);
        }
        let f = q.facts();
        let rows = if f.caps.processlist_table {
            let sql = format!(
                "SELECT {PROCESS_COLUMNS} FROM information_schema.processlist \
                 WHERE command <> 'Sleep' AND command <> 'Daemon' AND info IS NOT NULL AND id <> CONNECTION_ID()"
            );
            process_rows(&q.meta_query(&sql).await?, f)
        } else {
            let own = u64::from(f.session_id);
            show_processlist(q).await?.into_iter().filter(|p| !matches!(p.command.as_deref(), Some("Sleep" | "Daemon")) && p.id_num() != Some(own)).collect()
        };
        let mut out = Vec::new();
        for p in rows {
            let Some(info) = p.info.filter(|i| !i.trim().is_empty()) else { continue };
            let tables: Vec<String> = touching.iter().filter(|t| mentions_table(&info, t)).map(TableRef::qualified).collect();
            if tables.is_empty() {
                continue;
            }
            let state = p.state.filter(|s| !s.is_empty()).or(p.command);
            out.push(SessionInfo {
                pid: p.id,
                user: match (p.user, p.db) {
                    (Some(u), Some(d)) if !d.is_empty() => Some(format!("{u}@{d}")),
                    (u, _) => u,
                },
                application: tag_of(&info),
                wait_event: state.as_deref().filter(|s| is_wait_state(s)).map(str::to_string),
                state,
                query: Some(truncate_chars(&info, MAX_QUERY_CHARS)),
                elapsed_ms: p.time.map(|t| t.saturating_mul(1000)),
                tables,
                blocking: false,
            });
        }
        Ok(out)
    }

    pub async fn cancel_session(q: &dyn MysqlRawQuery, session_id: &str) -> Result<bool> {
        let id: u32 = session_id.trim().parse().map_err(|_| CoreError::Invalid(format!("'{session_id}' is not a MySQL connection id")))?;
        match q.meta_exec(&kill_sql(&q.facts().caps, id)).await {
            Ok(()) => Ok(true),
            // Unknown thread id -> nothing to cancel.
            Err(e) if server_error_code(&e) == Some(ER_NO_SUCH_THREAD) => Ok(false),
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

    mod versions {
        use crate::encoding::Charset;
        use crate::error::CoreError;
        use crate::monitor::mysql::generic;
        use crate::monitor::JobHandle;
        use crate::source::mysql_common::test_support::{facts, text_rows, utf8, Canned};
        use crate::source::mysql_common::ER_NO_SUCH_THREAD;
        use crate::source::mysql_dispatch::TextPlan;
        use crate::types::TableRef;

        const PROCESSLIST: [&str; 8] = ["Id", "User", "Host", "db", "Command", "Time", "State", "Info"];

        fn job(session_id: Option<&str>) -> JobHandle {
            JobHandle { job_id: "j1".into(), application_name: "bulkdb-j1".into(), session_id: session_id.map(str::to_string), temp_bytes_at_start: None }
        }

        fn legacy() -> Canned {
            Canned::new(facts("4.0.27", TextPlan::Fixed { charset: Charset::EucKr })).on(
                "SHOW FULL PROCESSLIST",
                text_rows(
                    &PROCESSLIST,
                    &[
                        &["7", "app", "h:1", "shop", "Query", "0", "\\N", "SHOW FULL PROCESSLIST"],
                        &["12", "app", "h:2", "shop", "Query", "35", "Sending data", "/* bulkdb-j1 */ SELECT * FROM orders"],
                        &["13", "app", "h:3", "shop", "Sleep", "100", "", "\\N"],
                        &["14", "bob", "h:4", "\\N", "Query", "3", "Locked", "UPDATE orders SET x = 1"],
                    ],
                ),
            )
        }

        #[tokio::test]
        async fn show_processlist_on_4x() {
            let q = legacy();
            let s = generic::snapshot(&q, &job(Some("12"))).await.unwrap();
            assert!(s.found);
            assert_eq!(s.pid.as_deref(), Some("12"));
            assert_eq!(s.state.as_deref(), Some("Sending data"));
            assert_eq!(s.wait_event.as_deref(), Some("Sending data"));
            assert_eq!(s.elapsed_ms, Some(35_000));
            assert_eq!(s.client_write_ratio, Some(0.0));
            // Without an id the SQL tag finds it; an unknown id is "not found".
            assert_eq!(generic::snapshot(&q, &job(None)).await.unwrap().pid.as_deref(), Some("12"));
            assert!(!generic::snapshot(&q, &job(Some("99"))).await.unwrap().found);
            assert!(q.log().iter().all(|s| s == "SHOW FULL PROCESSLIST"), "4.0 has no information_schema: {:?}", q.log());

            let sessions = generic::active_sessions(&q, &[TableRef::new(Some("shop"), "orders")]).await.unwrap();
            assert_eq!(sessions.iter().map(|s| s.pid.as_str()).collect::<Vec<_>>(), ["12", "14"]);
            assert_eq!(sessions[0].application.as_deref(), Some("bulkdb-j1"));
            assert_eq!(sessions[0].user.as_deref(), Some("app@shop"));
            assert_eq!(sessions[1].user.as_deref(), Some("bob"));
            assert_eq!(sessions[1].state.as_deref(), Some("Locked"));
        }

        #[tokio::test]
        async fn cancel_uses_kill_or_kill_query_by_version() {
            let q = Canned::new(facts("4.1.22", utf8()));
            assert!(generic::cancel_session(&q, "12").await.unwrap());
            assert_eq!(q.log(), ["KILL 12"]);
            let q = Canned::new(facts("5.5.62", utf8())).fail("KILL QUERY 13", ER_NO_SUCH_THREAD);
            assert!(generic::cancel_session(&q, "12").await.unwrap());
            assert!(!generic::cancel_session(&q, "13").await.unwrap(), "an unknown id is nothing to cancel");
            assert_eq!(q.log(), ["KILL QUERY 12", "KILL QUERY 13"]);
            assert!(matches!(generic::cancel_session(&q, "abc").await, Err(CoreError::Invalid(_))));
        }

        #[tokio::test]
        async fn information_schema_processlist_on_5_1_7_plus() {
            let cols = ["id", "user", "db", "command", "time", "state", "info"];
            let q = Canned::new(facts("8.0.36", utf8()))
                .on("WHERE id = 12", text_rows(&cols, &[&["12", "app", "shop", "Query", "4", "Sending to client", "/* bulkdb-j1 */ SELECT 1"]]))
                .on("info LIKE", text_rows(&cols, &[&["12", "app", "shop", "Query", "4", "executing", "/* bulkdb-j1 */ SELECT 1"]]));
            let s = generic::snapshot(&q, &job(Some("12"))).await.unwrap();
            assert_eq!(s.client_write_ratio, Some(1.0));
            assert_eq!(s.state.as_deref(), Some("Sending to client"));
            let s = generic::snapshot(&q, &job(None)).await.unwrap();
            assert!(s.found);
            let log = q.log();
            assert!(log[0].ends_with("FROM information_schema.processlist WHERE id = 12"), "{log:?}");
            assert!(log[1].contains("session_connect_attrs") && log[1].contains("a.attr_value = 'bulkdb-j1'"));
            assert!(log[2].contains("info LIKE '%/* bulkdb-j1 */%'"), "{log:?}");
            // 5.5: no session_connect_attrs; 5.1.6: SHOW FULL PROCESSLIST.
            let q = Canned::new(facts("5.5.62", utf8()));
            generic::snapshot(&q, &job(None)).await.unwrap();
            assert!(q.log().iter().all(|s| !s.contains("session_connect_attrs")));
            let q = Canned::new(facts("5.1.6", utf8()));
            generic::snapshot(&q, &job(Some("1"))).await.unwrap();
            assert_eq!(q.log(), ["SHOW FULL PROCESSLIST"]);
        }
    }
}
