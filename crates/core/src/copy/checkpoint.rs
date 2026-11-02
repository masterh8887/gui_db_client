//! Checkpoints, run claims and resume state in `history.sqlite` (section 5.5): tables
//! `copy_runs` / `copy_items` on a separate rusqlite connection (WAL, `busy_timeout 5000`).
//! Owner: copy-data.
//!
//! * Write-ahead: the intent is written before the action (`creating` with `created_by_run =
//!   1` before CREATE, `loading` before TRUNCATE or the first chunk), the outcome after.
//! * Claims run inside `BEGIN IMMEDIATE`: a new run is inserted; a resume updates the owner
//!   only when the run is not in use (`changes() == 1`), else `RUN_IN_USE`; another in-use run
//!   on the same target identity + database is `TARGET_IN_USE`.
//! * "In use" is decided by the caller's predicate (another live instance, or a non-terminal
//!   job of this process), so the store itself stays pure SQL.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use rusqlite::{params, OptionalExtension};

use super::types::*;
use crate::error::{CoreError, Result};
use crate::history;
use crate::types::JobState;

/// Runs kept: 30 days or the newest 50, whichever keeps more (unfinished runs are never pruned).
pub const KEEP_RUNS: usize = 50;
pub const KEEP_DAYS: i64 = 30;

/// One `copy_runs` row.
#[derive(Debug, Clone, PartialEq)]
pub struct RunRow {
    pub run_id: String,
    pub label: String,
    pub source_conn_id: String,
    pub target_conn_id: String,
    pub source_database: String,
    pub target_database: String,
    pub source_identity: String,
    pub target_identity: String,
    pub request_json: String,
    pub plan_id: String,
    pub started_at: String,
    pub finished_at: Option<String>,
    pub state: JobState,
    pub last_job_id: String,
    pub owner_pid: u32,
    pub owner_instance: String,
    pub dry_run: bool,
    pub result_json: Option<String>,
}

impl RunRow {
    pub fn result(&self) -> Option<CopyResult> {
        self.result_json.as_deref().and_then(|j| serde_json::from_str(j).ok())
    }

    pub fn request(&self) -> Result<CopyRequest> {
        serde_json::from_str(&self.request_json).map_err(|e| CoreError::Internal(format!("copy run {}: stored request is unreadable: {e}", self.run_id)))
    }
}

/// Whether a run state is one of the active ones (pending .. finishing).
pub fn is_active_state(s: JobState) -> bool {
    matches!(s, JobState::Pending | JobState::Preflight | JobState::Warned | JobState::Running | JobState::Finishing)
}

pub fn state_str(s: JobState) -> String {
    serde_json::to_value(s).ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_else(|| format!("{s:?}").to_ascii_lowercase())
}

fn parse_state(s: &str) -> JobState {
    serde_json::from_value(serde_json::Value::String(s.to_string())).unwrap_or(JobState::Failed)
}

pub fn checkpoint_str(s: CheckpointState) -> &'static str {
    match s {
        CheckpointState::Pending => "pending",
        CheckpointState::Creating => "creating",
        CheckpointState::Created => "created",
        CheckpointState::Loading => "loading",
        CheckpointState::Loaded => "loaded",
        CheckpointState::Done => "done",
        CheckpointState::Failed => "failed",
        CheckpointState::Skipped => "skipped",
    }
}

fn parse_checkpoint(s: &str) -> CheckpointState {
    match s {
        "creating" => CheckpointState::Creating,
        "created" => CheckpointState::Created,
        "loading" => CheckpointState::Loading,
        "loaded" => CheckpointState::Loaded,
        "done" => CheckpointState::Done,
        "failed" => CheckpointState::Failed,
        "skipped" => CheckpointState::Skipped,
        _ => CheckpointState::Pending,
    }
}

/// The copy checkpoint store.
pub struct CheckpointStore {
    path: PathBuf,
    conn: Mutex<rusqlite::Connection>,
}

const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS copy_runs (run_id TEXT PRIMARY KEY, label TEXT NOT NULL, source_conn_id TEXT NOT NULL,
               target_conn_id TEXT NOT NULL, source_database TEXT NOT NULL, target_database TEXT NOT NULL,
               source_identity TEXT NOT NULL, target_identity TEXT NOT NULL,
               request_json TEXT NOT NULL, plan_id TEXT NOT NULL,
               started_at TEXT NOT NULL, finished_at TEXT, state TEXT NOT NULL, last_job_id TEXT NOT NULL,
               owner_pid INTEGER NOT NULL, owner_instance TEXT NOT NULL, dry_run INTEGER NOT NULL DEFAULT 0, result_json TEXT);
             CREATE TABLE IF NOT EXISTS copy_items (run_id TEXT NOT NULL, item_id TEXT NOT NULL, state TEXT NOT NULL,
               created_by_run INTEGER NOT NULL DEFAULT 0, rows_written INTEGER NOT NULL DEFAULT 0, updated_at TEXT NOT NULL,
               error TEXT, PRIMARY KEY (run_id, item_id));
             CREATE INDEX IF NOT EXISTS copy_runs_started ON copy_runs(started_at DESC);";

const RUN_COLS: &str ="run_id, label, source_conn_id, target_conn_id, source_database, target_database, source_identity, target_identity, request_json, plan_id, started_at, finished_at, state, last_job_id, owner_pid, owner_instance, dry_run, result_json";

fn row_to_run(r: &rusqlite::Row<'_>) -> rusqlite::Result<RunRow> {
    Ok(RunRow {
        run_id: r.get(0)?,
        label: r.get(1)?,
        source_conn_id: r.get(2)?,
        target_conn_id: r.get(3)?,
        source_database: r.get(4)?,
        target_database: r.get(5)?,
        source_identity: r.get(6)?,
        target_identity: r.get(7)?,
        request_json: r.get(8)?,
        plan_id: r.get(9)?,
        started_at: r.get(10)?,
        finished_at: r.get(11)?,
        state: parse_state(&r.get::<_, String>(12)?),
        last_job_id: r.get(13)?,
        owner_pid: r.get::<_, i64>(14)? as u32,
        owner_instance: r.get(15)?,
        dry_run: r.get::<_, i64>(16)? != 0,
        result_json: r.get(17)?,
    })
}

impl CheckpointStore {
    /// Open (and create the tables of) the store at `config.history_path()`, through the
    /// multi-process safe [`history::open_shared`] (`busy_timeout` before WAL), with the schema
    /// setup retried while another process holds the database (spec 1.2).
    pub fn open(path: &Path) -> Result<CheckpointStore> {
        let conn = history::open_shared(path)?;
        history::retry_busy(|| conn.execute_batch(SCHEMA))?;
        Ok(CheckpointStore { path: path.to_path_buf(), conn: Mutex::new(conn) })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Run `f` inside `BEGIN IMMEDIATE` (committed on `Ok`, rolled back on `Err`).
    fn immediate<T>(&self, f: impl FnOnce(&rusqlite::Connection) -> Result<T>) -> Result<T> {
        let conn = self.conn.lock().unwrap();
        conn.execute_batch("BEGIN IMMEDIATE")?;
        match f(&conn) {
            Ok(v) => {
                conn.execute_batch("COMMIT")?;
                Ok(v)
            }
            Err(e) => {
                let _ = conn.execute_batch("ROLLBACK");
                Err(e)
            }
        }
    }

    fn target_busy(conn: &rusqlite::Connection, target_identity: &str, target_database: &str, except: &str, in_use: &dyn Fn(&RunRow) -> bool) -> Result<Option<RunRow>> {
        let mut stmt = conn.prepare(&format!("SELECT {RUN_COLS} FROM copy_runs WHERE target_identity = ?1 AND target_database = ?2 AND run_id <> ?3"))?;
        let rows = stmt.query_map(params![target_identity, target_database, except], row_to_run)?;
        for r in rows {
            let r = r?;
            if is_active_state(r.state) && in_use(&r) {
                return Ok(Some(r));
            }
        }
        Ok(None)
    }

    /// Insert a new run (start). `TARGET_IN_USE` (`INVALID`) when another in-use run writes the
    /// same target database.
    pub fn claim_new(&self, run: &RunRow, in_use: &dyn Fn(&RunRow) -> bool) -> Result<()> {
        self.immediate(|conn| {
            if let Some(other) = Self::target_busy(conn, &run.target_identity, &run.target_database, &run.run_id, in_use)? {
                return Err(CoreError::Invalid(format!("TARGET_IN_USE: copy run {} ({}) is writing this target database", other.run_id, other.label)));
            }
            conn.execute(
                &format!("INSERT INTO copy_runs ({RUN_COLS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)"),
                params![
                    run.run_id,
                    run.label,
                    run.source_conn_id,
                    run.target_conn_id,
                    run.source_database,
                    run.target_database,
                    run.source_identity,
                    run.target_identity,
                    run.request_json,
                    run.plan_id,
                    run.started_at,
                    run.finished_at,
                    state_str(run.state),
                    run.last_job_id,
                    run.owner_pid as i64,
                    run.owner_instance,
                    run.dry_run as i64,
                    run.result_json
                ],
            )?;
            Ok(())
        })
    }

    /// Take over a run for a resume: `RUN_IN_USE` when it is in use, `TARGET_IN_USE` when
    /// another in-use run writes its target. Returns the updated row.
    pub fn claim_resume(&self, run_id: &str, owner_pid: u32, owner_instance: &str, job_id: &str, in_use: &dyn Fn(&RunRow) -> bool) -> Result<RunRow> {
        self.immediate(|conn| {
            let row = conn
                .query_row(&format!("SELECT {RUN_COLS} FROM copy_runs WHERE run_id = ?1"), params![run_id], row_to_run)
                .optional()?
                .ok_or_else(|| CoreError::NotFound(format!("copy run {run_id}")))?;
            if is_active_state(row.state) && in_use(&row) {
                return Err(CoreError::Invalid(format!("RUN_IN_USE: copy run {run_id} is in use by job {}", row.last_job_id)));
            }
            if let Some(other) = Self::target_busy(conn, &row.target_identity, &row.target_database, run_id, in_use)? {
                return Err(CoreError::Invalid(format!("TARGET_IN_USE: copy run {} ({}) is writing this target database", other.run_id, other.label)));
            }
            let n = conn.execute(
                "UPDATE copy_runs SET owner_pid = ?1, owner_instance = ?2, last_job_id = ?3, state = 'pending', finished_at = NULL WHERE run_id = ?4",
                params![owner_pid as i64, owner_instance, job_id, run_id],
            )?;
            if n != 1 {
                return Err(CoreError::Invalid(format!("RUN_IN_USE: copy run {run_id} could not be claimed")));
            }
            let mut row = row;
            row.owner_pid = owner_pid;
            row.owner_instance = owner_instance.to_string();
            row.last_job_id = job_id.to_string();
            row.state = JobState::Pending;
            row.finished_at = None;
            Ok(row)
        })
    }

    pub fn set_run_state(&self, run_id: &str, state: JobState) -> Result<()> {
        self.conn.lock().unwrap().execute("UPDATE copy_runs SET state = ?1 WHERE run_id = ?2", params![state_str(state), run_id])?;
        Ok(())
    }

    /// Store the identities and plan id once the job has planned (first start).
    pub fn set_run_plan(&self, run_id: &str, source_identity: &str, target_identity: &str, plan_id: &str) -> Result<()> {
        self.conn.lock().unwrap().execute(
            "UPDATE copy_runs SET source_identity = ?1, target_identity = ?2, plan_id = ?3 WHERE run_id = ?4",
            params![source_identity, target_identity, plan_id, run_id],
        )?;
        Ok(())
    }

    pub fn finish_run(&self, run_id: &str, state: JobState, result: Option<&CopyResult>) -> Result<()> {
        let json = result.map(serde_json::to_string).transpose()?;
        self.conn.lock().unwrap().execute(
            "UPDATE copy_runs SET state = ?1, finished_at = ?2, result_json = COALESCE(?3, result_json) WHERE run_id = ?4",
            params![state_str(state), crate::types::now_rfc3339(), json, run_id],
        )?;
        Ok(())
    }

    /// Write-ahead item state. `created_by_run` only ever turns on; `rows_written` is replaced
    /// when given.
    pub fn set_item(&self, run_id: &str, item_id: &str, state: CheckpointState, created_by_run: bool, rows_written: Option<u64>, error: Option<&str>) -> Result<()> {
        self.conn.lock().unwrap().execute(
            "INSERT INTO copy_items (run_id, item_id, state, created_by_run, rows_written, updated_at, error) VALUES (?1, ?2, ?3, ?4, COALESCE(?5, 0), ?6, ?7)
             ON CONFLICT(run_id, item_id) DO UPDATE SET state = excluded.state, created_by_run = MAX(copy_items.created_by_run, excluded.created_by_run),
               rows_written = COALESCE(?5, copy_items.rows_written), updated_at = excluded.updated_at, error = excluded.error",
            params![run_id, item_id, checkpoint_str(state), created_by_run as i64, rows_written.map(|r| r as i64), crate::types::now_rfc3339(), error],
        )?;
        Ok(())
    }

    /// Every item of a run: id -> (state, created_by_run, rows_written).
    pub fn items(&self, run_id: &str) -> Result<BTreeMap<String, (CheckpointState, bool, u64)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT item_id, state, created_by_run, rows_written FROM copy_items WHERE run_id = ?1")?;
        let rows = stmt.query_map(params![run_id], |r| {
            Ok((r.get::<_, String>(0)?, parse_checkpoint(&r.get::<_, String>(1)?), r.get::<_, i64>(2)? != 0, r.get::<_, i64>(3)? as u64))
        })?;
        let mut out = BTreeMap::new();
        for r in rows {
            let (id, st, created, n) = r?;
            out.insert(id, (st, created, n));
        }
        Ok(out)
    }

    pub fn get(&self, run_id: &str) -> Result<Option<RunRow>> {
        Ok(self.conn.lock().unwrap().query_row(&format!("SELECT {RUN_COLS} FROM copy_runs WHERE run_id = ?1"), params![run_id], row_to_run).optional()?)
    }

    /// Runs whose source or target is `conn_id` (all when `None`), newest first.
    pub fn list(&self, conn_id: Option<&str>, limit: usize) -> Result<Vec<RunRow>> {
        let conn = self.conn.lock().unwrap();
        let limit = limit.clamp(1, 10_000) as i64;
        let mut out = Vec::new();
        match conn_id {
            Some(id) => {
                let mut stmt =
                    conn.prepare(&format!("SELECT {RUN_COLS} FROM copy_runs WHERE source_conn_id = ?1 OR target_conn_id = ?1 ORDER BY started_at DESC LIMIT ?2"))?;
                for r in stmt.query_map(params![id, limit], row_to_run)? {
                    out.push(r?);
                }
            }
            None => {
                let mut stmt = conn.prepare(&format!("SELECT {RUN_COLS} FROM copy_runs ORDER BY started_at DESC LIMIT ?1"))?;
                for r in stmt.query_map(params![limit], row_to_run)? {
                    out.push(r?);
                }
            }
        }
        Ok(out)
    }

    /// Table counts of a run from its items: (total, done, failed).
    pub fn table_counts(&self, run_id: &str) -> Result<(u32, u32, u32)> {
        let conn = self.conn.lock().unwrap();
        let (total, done, failed): (i64, i64, i64) = conn.query_row(
            "SELECT COUNT(*), COALESCE(SUM(state IN ('done', 'loaded', 'skipped')), 0), COALESCE(SUM(state = 'failed'), 0) FROM copy_items WHERE run_id = ?1 AND item_id LIKE 'table:%'",
            params![run_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        Ok((total as u32, done as u32, failed as u32))
    }

    /// Delete a run and its items; refused (`INVALID`) while it is in use.
    pub fn forget(&self, run_id: &str, in_use: &dyn Fn(&RunRow) -> bool) -> Result<()> {
        self.immediate(|conn| {
            let row = conn
                .query_row(&format!("SELECT {RUN_COLS} FROM copy_runs WHERE run_id = ?1"), params![run_id], row_to_run)
                .optional()?
                .ok_or_else(|| CoreError::NotFound(format!("copy run {run_id}")))?;
            if is_active_state(row.state) && in_use(&row) {
                return Err(CoreError::Invalid(format!("RUN_IN_USE: copy run {run_id} is in use; cancel its job first")));
            }
            conn.execute("DELETE FROM copy_items WHERE run_id = ?1", params![run_id])?;
            conn.execute("DELETE FROM copy_runs WHERE run_id = ?1", params![run_id])?;
            Ok(())
        })
    }

    /// Keep runs of the last 30 days or the newest 50, whichever keeps more; unfinished runs
    /// stay.
    pub fn prune(&self) -> Result<usize> {
        let cutoff = (chrono::Local::now() - chrono::Duration::days(KEEP_DAYS)).to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        self.immediate(|conn| {
            let ids: Vec<String> = {
                let mut stmt = conn.prepare(
                    "SELECT run_id FROM copy_runs WHERE finished_at IS NOT NULL AND started_at < ?1
                       AND run_id NOT IN (SELECT run_id FROM copy_runs ORDER BY started_at DESC LIMIT ?2)",
                )?;
                let rows = stmt.query_map(params![cutoff, KEEP_RUNS as i64], |r| r.get::<_, String>(0))?;
                rows.collect::<rusqlite::Result<Vec<_>>>()?
            };
            for id in &ids {
                conn.execute("DELETE FROM copy_items WHERE run_id = ?1", params![id])?;
                conn.execute("DELETE FROM copy_runs WHERE run_id = ?1", params![id])?;
            }
            Ok(ids.len())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(id: &str, target: &str) -> RunRow {
        RunRow {
            run_id: id.into(),
            label: format!("run {id}"),
            source_conn_id: "s".into(),
            target_conn_id: "t".into(),
            source_database: "a".into(),
            target_database: "b".into(),
            source_identity: "src".into(),
            target_identity: target.into(),
            request_json: "{}".into(),
            plan_id: "p".into(),
            started_at: crate::types::now_rfc3339(),
            finished_at: None,
            state: JobState::Pending,
            last_job_id: id.into(),
            owner_pid: 1,
            owner_instance: "inst-a".into(),
            dry_run: false,
            result_json: None,
        }
    }

    #[test]
    fn opens_through_the_shared_helper_with_busy_timeout_and_wal() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.sqlite");
        // The export history and the checkpoint store share one file.
        let _h = crate::history::History::open(&path).unwrap();
        let s = CheckpointStore::open(&path).unwrap();
        let c = s.conn.lock().unwrap();
        let ms: i64 = c.query_row("PRAGMA busy_timeout", [], |r| r.get(0)).unwrap();
        assert_eq!(ms, crate::history::BUSY_TIMEOUT.as_millis() as i64);
        let mode: String = c.query_row("PRAGMA journal_mode", [], |r| r.get(0)).unwrap();
        assert_eq!(mode.to_ascii_lowercase(), "wal");
    }

    #[test]
    fn schema_setup_waits_for_another_writer() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.sqlite");
        // Another connection (another process in production) holds the write lock for a while.
        let holder = crate::history::open_shared(&path).unwrap();
        holder.execute_batch("CREATE TABLE IF NOT EXISTS other (x); BEGIN IMMEDIATE; INSERT INTO other VALUES (1);").unwrap();
        let p = path.clone();
        let opener = std::thread::spawn(move || CheckpointStore::open(&p).map(|_| ()));
        std::thread::sleep(std::time::Duration::from_millis(300));
        holder.execute_batch("COMMIT").unwrap();
        opener.join().unwrap().expect("schema setup waits instead of failing with SQLITE_BUSY");
    }

    #[test]
    fn crud_with_two_connections_and_write_ahead_states() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.sqlite");
        let a = CheckpointStore::open(&path).unwrap();
        let b = CheckpointStore::open(&path).unwrap();
        a.claim_new(&run("r1", "tgt"), &|_| false).unwrap();
        a.set_item("r1", "table:\"t\"", CheckpointState::Creating, true, None, None).unwrap();
        b.set_item("r1", "table:\"t\"", CheckpointState::Loading, false, Some(0), None).unwrap();
        a.set_item("r1", "table:\"t\"", CheckpointState::Loaded, false, Some(42), None).unwrap();
        b.set_item("r1", "index:\"t\".\"i\"", CheckpointState::Failed, false, None, Some("boom")).unwrap();
        let items = b.items("r1").unwrap();
        assert_eq!(items["table:\"t\""], (CheckpointState::Loaded, true, 42), "created_by_run stays on");
        assert_eq!(items["index:\"t\".\"i\""].0, CheckpointState::Failed);
        assert_eq!(a.table_counts("r1").unwrap(), (1, 1, 0));
        a.finish_run("r1", JobState::Failed, None).unwrap();
        let r = b.get("r1").unwrap().unwrap();
        assert_eq!(r.state, JobState::Failed);
        assert!(r.finished_at.is_some());
        assert_eq!(b.list(Some("t"), 10).unwrap().len(), 1);
        assert_eq!(b.list(Some("zzz"), 10).unwrap().len(), 0);
    }

    #[test]
    fn claims_own_process_other_instance_and_concurrent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.sqlite");
        let s = CheckpointStore::open(&path).unwrap();
        s.claim_new(&run("r1", "tgt"), &|_| false).unwrap();
        s.set_run_state("r1", JobState::Running).unwrap();
        // Same target while r1 is in use: TARGET_IN_USE.
        let err = s.claim_new(&run("r2", "tgt"), &|r| r.run_id == "r1").unwrap_err();
        assert!(err.to_string().contains("TARGET_IN_USE"), "{err}");
        // Another live instance owns r1: a resume is refused.
        let err = s.claim_resume("r1", 2, "inst-b", "j2", &|r| r.owner_instance == "inst-a").unwrap_err();
        assert!(err.to_string().contains("RUN_IN_USE"), "{err}");
        // Own process after a failure: the old job is terminal, so not in use -> claimed.
        s.finish_run("r1", JobState::Failed, None).unwrap();
        let row = s.claim_resume("r1", 1, "inst-a", "j3", &|_| false).unwrap();
        assert_eq!((row.last_job_id.as_str(), row.state), ("j3", JobState::Pending));
        // Two concurrent claims on two connections: exactly one wins.
        s.finish_run("r1", JobState::Failed, None).unwrap();
        let p1 = path.clone();
        let p2 = path.clone();
        let t1 = std::thread::spawn(move || {
            let st = CheckpointStore::open(&p1).unwrap();
            st.claim_resume("r1", 10, "x", "jx", &|r| r.last_job_id != "j3")
        });
        let t2 = std::thread::spawn(move || {
            let st = CheckpointStore::open(&p2).unwrap();
            st.claim_resume("r1", 11, "y", "jy", &|r| r.last_job_id != "j3")
        });
        let (a, b) = (t1.join().unwrap(), t2.join().unwrap());
        assert!(a.is_ok() ^ b.is_ok(), "exactly one claim wins: {a:?} / {b:?}");
    }

    #[test]
    fn forget_refuses_in_use_runs() {
        let dir = tempfile::tempdir().unwrap();
        let s = CheckpointStore::open(&dir.path().join("h.sqlite")).unwrap();
        s.claim_new(&run("r1", "tgt"), &|_| false).unwrap();
        s.set_item("r1", "table:\"t\"", CheckpointState::Done, true, Some(1), None).unwrap();
        assert!(s.forget("r1", &|_| true).unwrap_err().to_string().contains("RUN_IN_USE"));
        s.forget("r1", &|_| false).unwrap();
        assert!(s.get("r1").unwrap().is_none());
        assert!(s.items("r1").unwrap().is_empty());
        assert!(matches!(s.forget("r1", &|_| false), Err(CoreError::NotFound(_))));
        assert_eq!(s.prune().unwrap(), 0);
    }
}
