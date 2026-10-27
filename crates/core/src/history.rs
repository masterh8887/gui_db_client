//! `history.sqlite`: the last runs per connection (query, settings, result, files).
//!
//! Two tables: `runs` (one row per export job, inserted at start and updated at the end) and
//! `exact_counts` (the last explicit `count(*)` per table, so the catalog can show "exact as
//! of <time>"). Session / output options and the file list are stored as JSON columns.

use std::path::Path;
use std::sync::Mutex;

use rusqlite::{params, OptionalExtension};

use crate::error::{CoreError, Result};
use crate::types::{ExactCount, HistoryEntry, JobResult, JobState, OutputOpts, SessionOpts, TableRef};

pub struct History {
    conn: Mutex<rusqlite::Connection>,
}

/// What `record_start` needs.
pub struct HistoryStart<'a> {
    pub job_id: &'a str,
    pub conn_id: &'a str,
    pub label: &'a str,
    pub sql: &'a str,
    pub sql_file: Option<&'a str>,
    pub session: &'a crate::types::SessionOpts,
    pub output: &'a crate::types::OutputOpts,
}

const SELECT_COLUMNS: &str =
    "id, job_id, conn_id, started_at, finished_at, state, label, sql, sql_file, session_json, output_json, rows, bytes, duration_ms, files_json, error";

/// Key under which a table's exact count is stored: `[db.]schema.table` (unquoted).
pub fn table_key(table: &TableRef) -> String {
    let mut parts: Vec<&str> = Vec::new();
    if let Some(db) = table.database.as_deref().filter(|s| !s.is_empty()) {
        parts.push(db);
    }
    if let Some(schema) = table.schema.as_deref().filter(|s| !s.is_empty()) {
        parts.push(schema);
    }
    parts.push(&table.name);
    parts.join(".")
}

/// Inverse of [`table_key`] (best effort: names containing `.` are split too).
fn table_from_key(key: &str) -> TableRef {
    let mut parts: Vec<&str> = key.split('.').collect();
    let name = parts.pop().unwrap_or_default().to_string();
    let schema = parts.pop().map(|s| s.to_string());
    let database = parts.pop().map(|s| s.to_string());
    TableRef { database, schema, name }
}

fn state_to_str(state: JobState) -> String {
    match serde_json::to_value(state) {
        Ok(serde_json::Value::String(s)) => s,
        _ => format!("{state:?}").to_ascii_lowercase(),
    }
}

fn state_from_str(s: &str) -> JobState {
    serde_json::from_value(serde_json::Value::String(s.to_string())).unwrap_or(JobState::Failed)
}

impl History {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = rusqlite::Connection::open(path)?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             CREATE TABLE IF NOT EXISTS runs (
               id INTEGER PRIMARY KEY AUTOINCREMENT,
               job_id TEXT NOT NULL UNIQUE,
               conn_id TEXT NOT NULL,
               started_at TEXT NOT NULL,
               finished_at TEXT,
               state TEXT NOT NULL,
               label TEXT NOT NULL,
               sql TEXT NOT NULL,
               sql_file TEXT,
               session_json TEXT NOT NULL,
               output_json TEXT NOT NULL,
               rows INTEGER,
               bytes INTEGER,
               duration_ms INTEGER,
               files_json TEXT NOT NULL DEFAULT '[]',
               error TEXT
             );
             CREATE INDEX IF NOT EXISTS runs_conn ON runs(conn_id, started_at DESC);
             CREATE TABLE IF NOT EXISTS exact_counts (
               conn_id TEXT NOT NULL,
               table_key TEXT NOT NULL,
               rows INTEGER NOT NULL,
               elapsed_ms INTEGER NOT NULL,
               at TEXT NOT NULL,
               PRIMARY KEY (conn_id, table_key)
             );",
        )?;
        Ok(History { conn: Mutex::new(conn) })
    }

    pub fn in_memory() -> Result<Self> {
        Self::open(Path::new(":memory:"))
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, rusqlite::Connection> {
        // A poisoned lock only means another thread panicked mid-query; the connection is
        // still usable.
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Insert a run in state `running` with `started_at = now`. Returns the row id.
    /// Recording the same `job_id` twice is an `Invalid` error.
    pub fn record_start(&self, s: HistoryStart<'_>) -> Result<i64> {
        let session_json = serde_json::to_string(s.session)?;
        let output_json = serde_json::to_string(s.output)?;
        let conn = self.lock();
        conn.execute(
            "INSERT INTO runs (job_id, conn_id, started_at, state, label, sql, sql_file, session_json, output_json)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![s.job_id, s.conn_id, crate::types::now_rfc3339(), state_to_str(JobState::Running), s.label, s.sql, s.sql_file, session_json, output_json],
        )
        .map_err(|e| match e {
            rusqlite::Error::SqliteFailure(f, _) if f.code == rusqlite::ErrorCode::ConstraintViolation => {
                CoreError::Invalid(format!("history already has a run for job {}", s.job_id))
            }
            other => CoreError::from(other),
        })?;
        Ok(conn.last_insert_rowid())
    }

    /// Store the outcome of a run. `finished_at` is set to now; rows / bytes / duration / files
    /// come from `result` when there is one. A job that was never recorded is a no-op.
    pub fn record_finish(&self, job_id: &str, state: JobState, result: Option<&JobResult>, error: Option<&str>) -> Result<()> {
        let files_json = serde_json::to_string(&result.map(|r| r.files.clone()).unwrap_or_default())?;
        let conn = self.lock();
        conn.execute(
            "UPDATE runs SET state = ?2, finished_at = ?3, rows = ?4, bytes = ?5, duration_ms = ?6, files_json = ?7, error = ?8
             WHERE job_id = ?1",
            params![
                job_id,
                state_to_str(state),
                crate::types::now_rfc3339(),
                result.map(|r| r.rows as i64),
                result.map(|r| r.bytes as i64),
                result.map(|r| r.duration_ms as i64),
                files_json,
                error,
            ],
        )?;
        Ok(())
    }

    /// Most recent runs, newest first. `conn_id = None` lists every connection.
    pub fn list(&self, conn_id: Option<&str>, limit: usize) -> Result<Vec<HistoryEntry>> {
        let conn = self.lock();
        let limit = limit.min(i64::MAX as usize) as i64;
        let mut out = Vec::new();
        match conn_id {
            Some(cid) => {
                let mut st = conn.prepare_cached(&format!("SELECT {SELECT_COLUMNS} FROM runs WHERE conn_id = ?1 ORDER BY started_at DESC, id DESC LIMIT ?2"))?;
                let rows = st.query_map(params![cid, limit], row_to_entry)?;
                for r in rows {
                    out.push(r?);
                }
            }
            None => {
                let mut st = conn.prepare_cached(&format!("SELECT {SELECT_COLUMNS} FROM runs ORDER BY started_at DESC, id DESC LIMIT ?1"))?;
                let rows = st.query_map(params![limit], row_to_entry)?;
                for r in rows {
                    out.push(r?);
                }
            }
        }
        Ok(out)
    }

    /// Upsert the last exact count of `count.table` for `conn_id`.
    pub fn record_exact_count(&self, conn_id: &str, count: &ExactCount) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO exact_counts (conn_id, table_key, rows, elapsed_ms, at) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(conn_id, table_key) DO UPDATE SET rows = excluded.rows, elapsed_ms = excluded.elapsed_ms, at = excluded.at",
            params![conn_id, table_key(&count.table), count.rows as i64, count.elapsed_ms as i64, count.at],
        )?;
        Ok(())
    }

    /// Last exact count for a table (`table_key` as produced by [`table_key`]).
    pub fn last_exact_count(&self, conn_id: &str, table_key: &str) -> Result<Option<ExactCount>> {
        let conn = self.lock();
        let row = conn
            .query_row("SELECT table_key, rows, elapsed_ms, at FROM exact_counts WHERE conn_id = ?1 AND table_key = ?2", params![conn_id, table_key], |r| {
                Ok(ExactCount {
                    table: table_from_key(&r.get::<_, String>(0)?),
                    rows: r.get::<_, i64>(1)?.max(0) as u64,
                    elapsed_ms: r.get::<_, i64>(2)?.max(0) as u64,
                    at: r.get(3)?,
                })
            })
            .optional()?;
        Ok(row)
    }

    /// Keep only the newest `keep` runs per connection.
    pub fn prune(&self, keep: usize) -> Result<()> {
        let conn = self.lock();
        let keep = keep.min(i64::MAX as usize) as i64;
        conn.execute(
            "DELETE FROM runs WHERE id NOT IN (
               SELECT r2.id FROM runs r2 WHERE r2.conn_id = runs.conn_id ORDER BY r2.started_at DESC, r2.id DESC LIMIT ?1
             )",
            params![keep],
        )?;
        Ok(())
    }

    /// Number of recorded runs (diagnostics / tests).
    pub fn count(&self, conn_id: Option<&str>) -> Result<u64> {
        let conn = self.lock();
        let n: i64 = match conn_id {
            Some(cid) => conn.query_row("SELECT count(*) FROM runs WHERE conn_id = ?1", params![cid], |r| r.get(0))?,
            None => conn.query_row("SELECT count(*) FROM runs", [], |r| r.get(0))?,
        };
        Ok(n.max(0) as u64)
    }
}

fn row_to_entry(r: &rusqlite::Row<'_>) -> rusqlite::Result<HistoryEntry> {
    let session_json: String = r.get(9)?;
    let output_json: String = r.get(10)?;
    let files_json: String = r.get(14)?;
    let state: String = r.get(5)?;
    Ok(HistoryEntry {
        id: r.get(0)?,
        job_id: r.get(1)?,
        conn_id: r.get(2)?,
        started_at: r.get(3)?,
        finished_at: r.get(4)?,
        state: state_from_str(&state),
        label: r.get(6)?,
        sql: r.get(7)?,
        sql_file: r.get(8)?,
        session: serde_json::from_str::<SessionOpts>(&session_json).unwrap_or_default(),
        output: serde_json::from_str::<OutputOpts>(&output_json).unwrap_or_default(),
        rows: r.get::<_, Option<i64>>(11)?.map(|v| v.max(0) as u64),
        bytes: r.get::<_, Option<i64>>(12)?.map(|v| v.max(0) as u64),
        duration_ms: r.get::<_, Option<i64>>(13)?.map(|v| v.max(0) as u64),
        files: serde_json::from_str::<Vec<String>>(&files_json).unwrap_or_default(),
        error: r.get(15)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{OutputFormat, SplitOpts};

    fn start<'a>(job_id: &'a str, conn_id: &'a str, session: &'a SessionOpts, output: &'a OutputOpts) -> HistoryStart<'a> {
        HistoryStart { job_id, conn_id, label: "orders", sql: "select * from orders", sql_file: None, session, output }
    }

    #[test]
    fn crud_round_trip() {
        let h = History::in_memory().unwrap();
        let session = SessionOpts { work_mem: Some("64MB".into()), statement_timeout_ms: Some(1000), ..Default::default() };
        let output = OutputOpts { dir: "C:/out".into(), format: OutputFormat::Parquet, split: Some(SplitOpts { rows: Some(10), megabytes: None }), ..Default::default() };

        let id = h.record_start(start("j1", "c1", &session, &output)).unwrap();
        assert!(id > 0);
        assert!(matches!(h.record_start(start("j1", "c1", &session, &output)), Err(CoreError::Invalid(_))));

        let listed = h.list(Some("c1"), 10).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].state, JobState::Running);
        assert_eq!(listed[0].session, session);
        assert_eq!(listed[0].output, output);
        assert!(listed[0].finished_at.is_none());
        assert_eq!(listed[0].sql, "select * from orders");

        let result = JobResult { rows: 1234, bytes: 56789, files: vec!["a.parquet".into(), "b.parquet".into()], duration_ms: 42, verified_rows: None, notes: vec![] };
        h.record_finish("j1", JobState::Done, Some(&result), None).unwrap();
        let e = &h.list(Some("c1"), 10).unwrap()[0];
        assert_eq!(e.state, JobState::Done);
        assert_eq!(e.rows, Some(1234));
        assert_eq!(e.bytes, Some(56789));
        assert_eq!(e.duration_ms, Some(42));
        assert_eq!(e.files, vec!["a.parquet", "b.parquet"]);
        assert!(e.finished_at.is_some());
        assert_eq!(e.error, None);

        h.record_start(start("j2", "c1", &session, &output)).unwrap();
        h.record_finish("j2", JobState::Failed, None, Some("boom")).unwrap();
        let e = h.list(Some("c1"), 10).unwrap();
        assert_eq!(e.len(), 2);
        assert_eq!(e[0].job_id, "j2", "newest first");
        assert_eq!(e[0].state, JobState::Failed);
        assert_eq!(e[0].error.as_deref(), Some("boom"));
        assert_eq!(e[0].rows, None);
        assert!(e[0].files.is_empty());

        // Unknown job: no-op.
        h.record_finish("nope", JobState::Done, None, None).unwrap();
        assert_eq!(h.count(None).unwrap(), 2);
    }

    #[test]
    fn list_filters_and_limits() {
        let h = History::in_memory().unwrap();
        let session = SessionOpts::default();
        let output = OutputOpts::default();
        for i in 0..5 {
            h.record_start(start(&format!("a{i}"), "A", &session, &output)).unwrap();
            h.record_start(start(&format!("b{i}"), "B", &session, &output)).unwrap();
        }
        assert_eq!(h.list(None, 100).unwrap().len(), 10);
        assert_eq!(h.list(None, 3).unwrap().len(), 3);
        let b = h.list(Some("B"), 100).unwrap();
        assert_eq!(b.len(), 5);
        assert!(b.iter().all(|e| e.conn_id == "B"));
        assert_eq!(b[0].job_id, "b4");
        assert!(h.list(Some("C"), 100).unwrap().is_empty());
    }

    #[test]
    fn exact_counts_upsert() {
        let h = History::in_memory().unwrap();
        let table = TableRef::new(Some("public"), "orders");
        let key = table_key(&table);
        assert_eq!(key, "public.orders");
        assert_eq!(h.last_exact_count("c1", &key).unwrap(), None);

        h.record_exact_count("c1", &ExactCount { table: table.clone(), rows: 10, elapsed_ms: 5, at: "2026-10-23T00:00:00.000Z".into() }).unwrap();
        h.record_exact_count("c1", &ExactCount { table: table.clone(), rows: 20, elapsed_ms: 7, at: "2026-10-24T00:00:00.000Z".into() }).unwrap();
        let c = h.last_exact_count("c1", &key).unwrap().unwrap();
        assert_eq!(c.rows, 20);
        assert_eq!(c.elapsed_ms, 7);
        assert_eq!(c.at, "2026-10-24T00:00:00.000Z");
        assert_eq!(c.table, table);
        assert_eq!(h.last_exact_count("c2", &key).unwrap(), None);

        let with_db = TableRef { database: Some("app".into()), schema: Some("dbo".into()), name: "t".into() };
        assert_eq!(table_key(&with_db), "app.dbo.t");
        assert_eq!(table_from_key("app.dbo.t"), with_db);
        assert_eq!(table_from_key("t"), TableRef::new(None, "t"));
    }

    #[test]
    fn prune_keeps_newest_per_connection() {
        let h = History::in_memory().unwrap();
        let session = SessionOpts::default();
        let output = OutputOpts::default();
        for i in 0..6 {
            h.record_start(start(&format!("a{i}"), "A", &session, &output)).unwrap();
        }
        for i in 0..2 {
            h.record_start(start(&format!("b{i}"), "B", &session, &output)).unwrap();
        }
        h.prune(3).unwrap();
        let a = h.list(Some("A"), 100).unwrap();
        assert_eq!(a.iter().map(|e| e.job_id.as_str()).collect::<Vec<_>>(), vec!["a5", "a4", "a3"]);
        assert_eq!(h.count(Some("B")).unwrap(), 2);
    }

    #[test]
    fn opens_on_disk_and_reopens() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.sqlite");
        {
            let h = History::open(&path).unwrap();
            h.record_start(start("j", "c", &SessionOpts::default(), &OutputOpts::default())).unwrap();
        }
        let h = History::open(&path).unwrap();
        assert_eq!(h.count(None).unwrap(), 1);
    }
}
