//! Job flow of a copy (section 5.1): claim, lease, plan, preflight finalize, warned / running,
//! pre_data, data workers (with each table's post-data), remaining post_data, finalize,
//! verification, result. Owner: copy-data.
//!
//! * One failing item does not stop the others; its dependants end `skipped` with
//!   `DEPENDENCY_FAILED`. The `CopyResult` is always stored (job, checkpoint store).
//! * Every target statement passes the allow-list (`guard`) and is raced against the job's
//!   token; 300 ms after a cancel the session's `cancel_running` runs.
//! * Memory: per table one source stream -> depth-1 channel -> writer, under the job's 16 MiB
//!   in-flight budget (`data::pipeline::Budget`).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::stream::{FuturesUnordered, StreamExt};
use tokio_util::sync::CancellationToken;

use super::checkpoint::{CheckpointStore, RunRow};
use super::data::pipeline::{self, Budget, ChecksumMode, FeedOpts};
use super::guard::{self, StmtContext};
use super::hash::TableChecksum;
use super::preflight::{self, issue, FinalizeInput};
use super::target::{endpoint_connection, TargetOpener, TargetSession};
use super::types::*;
use super::*;
use crate::connect::LiveConnection;
use crate::error::{CoreError, ErrorPayload, Result};
use crate::job::JobEntry;
use crate::service::BulkDb;
use crate::source::{CsvFormat, Query, StreamOutput};
use crate::types::{Connection, JobDone, JobResult, JobState, PreflightIssue, PreflightReport, SessionOpts, Severity};

/// What a job starts from: a new request (start) or a stored run (resume).
#[derive(Debug, Clone)]
pub enum CopyStart {
    New { req: Box<CopyRequest>, plan_id: String, acknowledged: Vec<String> },
    Resume { req: CopyResumeRequest },
}

/// Grace between a cancel and `cancel_running` (5.4).
const KILL_AFTER: Duration = Duration::from_millis(300);
/// How long a cancelled statement may take to return after `cancel_running`.
const CANCEL_WAIT: Duration = Duration::from_millis(1500);
/// How long a source driver call that received the token (stream start, a count) gets for its
/// own server-side cancel: below `job::CANCEL_GRACE` (2 s), unlike export's 3 s.
const DRIVER_GRACE: Duration = Duration::from_millis(1500);

// ---------------------------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------------------------

/// Run `fut` (a statement on `session`) raced against `cancel`: 300 ms after a cancel the
/// session's `cancel_running` fires, then the statement gets 1.5 s to return.
pub async fn raced<T>(session: &dyn TargetSession, cancel: &CancellationToken, fut: impl std::future::Future<Output = Result<T>>) -> Result<T> {
    tokio::pin!(fut);
    tokio::select! {
        biased;
        r = &mut fut => return r,
        _ = cancel.cancelled() => {}
    }
    if let Ok(r) = tokio::time::timeout(KILL_AFTER, &mut fut).await {
        return r.and(Err(CoreError::Cancelled));
    }
    session.cancel_running().await;
    let _ = tokio::time::timeout(CANCEL_WAIT, &mut fut).await;
    Err(CoreError::Cancelled)
}

/// Check and execute one statement.
async fn exec_checked(session: &dyn TargetSession, family: CopyFamily, sql: &str, ctx: StmtContext, cancel: &CancellationToken) -> Result<u64> {
    guard::check_statement(family, sql, ctx)?;
    tracing::debug!(statement = %crate::logging::mask_secrets(sql), "copy target");
    raced(session, cancel, session.execute(sql)).await
}

/// Data-phase statements of a table item, split around its load template.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct ItemSql {
    /// DDL run in the item's own phase (CREATE, DROP, ALTER, SET search_path, ...).
    pub ddl: Vec<String>,
    /// Run right before the load (TRUNCATE / DELETE / DISABLE KEYS), inside the load
    /// transaction where the family has one.
    pub pre_load: Vec<String>,
    /// Run right after the load (ENABLE KEYS, ...).
    pub post_load: Vec<String>,
}

/// `ALTER TABLE … DISABLE KEYS` / `… ENABLE KEYS` (MySQL MyISAM, 3.3 DDL placement).
fn is_keys_toggle(sql: &str, which: &str) -> bool {
    let up = sql.to_ascii_uppercase();
    guard::leading_words(sql, 1).first().map(String::as_str) == Some("ALTER") && up.trim_end().trim_end_matches(';').ends_with(which)
}

/// Split the `target_sql` of an item (section 2: data items carry their load template; the
/// executor runs DDL in the item's phase and the rest around its own load statement). The
/// classification is the planner's contract, [`translate::item_sql_parts`](super::translate::item_sql_parts):
/// its load template is never executed (the writer builds the real statement), its truncate
/// runs right before the load (inside the load transaction where the family has one), its DDL
/// runs in the item's phase, except MyISAM `DISABLE KEYS` (right before the load, on the load
/// session) and `ENABLE KEYS` or anything after the template (right after the load).
pub fn split_item_sql(item: &PlanItem, has_spec: bool) -> ItemSql {
    let mut out = ItemSql::default();
    if !has_spec {
        out.ddl = item.target_sql.clone();
        return out;
    }
    let parts = super::translate::item_sql_parts(item);
    let template = parts.load_template.as_ref().and_then(|t| item.target_sql.iter().position(|s| s == t));
    for (i, s) in item.target_sql.iter().enumerate() {
        match template {
            Some(t) if i == t => {}
            Some(t) if i > t => out.post_load.push(s.clone()),
            _ if parts.truncate.as_deref() == Some(s.as_str()) || is_keys_toggle(s, "DISABLE KEYS") => out.pre_load.push(s.clone()),
            _ if is_keys_toggle(s, "ENABLE KEYS") => out.post_load.push(s.clone()),
            _ => out.ddl.push(s.clone()),
        }
    }
    out
}

/// Every statement the executor sends for the items and session of `planned`, with the context
/// it checks the statement in: `session_sql.target` as session settings; each executing item's
/// statements split by [`split_item_sql`] as that item; finalize templates built by
/// `translate::finalize_sql` from sample values (with and without the target floor). The
/// writers' own load statements are not included.
pub fn executed_statements(planned: &PlannedCopy) -> Vec<(String, StmtContext)> {
    let plan = &planned.plan;
    let mut out: Vec<(String, StmtContext)> = plan.session_sql.target.iter().map(|s| (s.clone(), StmtContext::Session)).collect();
    for item in plan.items.iter().filter(|i| executes(i.action)) {
        let ctx = StmtContext::Item { kind: item.kind, action: item.action };
        if matches!(item.kind, CopyObjectKind::SequenceValue | CopyObjectKind::AutoIncrement) {
            for floor in [None, Some(7)] {
                let s = super::translate::finalize_sql(item, (42, true), floor);
                if !s.trim().is_empty() {
                    out.push((s, ctx));
                }
            }
            continue;
        }
        let sql = split_item_sql(item, planned.tables.contains_key(&item.id));
        out.extend(sql.ddl.into_iter().chain(sql.pre_load).chain(sql.post_load).map(|s| (s, ctx)));
    }
    out
}

/// The statements of [`executed_statements`] the allow-lists of 5.2 refuse (empty = the plan
/// can run), as `"<error>"` lines.
pub fn refused_statements(planned: &PlannedCopy) -> Vec<String> {
    executed_statements(planned).into_iter().filter_map(|(s, c)| guard::check_statement(planned.plan.family, &s, c).err().map(|e| e.to_string())).collect()
}

/// A finalize template that needs the target's current value (the PG floor rule, 3.5).
fn uses_target_floor(item: &PlanItem) -> bool {
    item.target_sql.first().is_some_and(|t| t.contains(super::translate::TARGET_VALUE))
}

/// Actions that execute something.
fn executes(a: PlanAction) -> bool {
    matches!(a, PlanAction::Create | PlanAction::Truncate | PlanAction::DropRecreate | PlanAction::Append | PlanAction::Run)
}

/// Actions that load data (with a `TableDataSpec`).
fn loads(a: PlanAction) -> bool {
    matches!(a, PlanAction::Create | PlanAction::Truncate | PlanAction::DropRecreate | PlanAction::Append)
}

/// Status of an item while the job runs.
#[derive(Debug, Clone, PartialEq)]
enum St {
    Done,
    /// Not executed by design (skip_existing, unsupported, excluded, resume_done).
    NotRun,
    Failed(Option<ErrorPayload>),
    Cancelled,
    /// Skipped because a dependency failed.
    DepFailed,
}

impl St {
    fn ok(&self) -> bool {
        matches!(self, St::Done | St::NotRun)
    }

    fn item_state(&self, action: PlanAction) -> CopyItemState {
        match self {
            St::Done if action == PlanAction::ResumeDone => CopyItemState::Done,
            St::Done => CopyItemState::Done,
            St::NotRun => CopyItemState::Skipped,
            St::Failed(_) => CopyItemState::Failed,
            St::Cancelled => CopyItemState::Cancelled,
            St::DepFailed => CopyItemState::Skipped,
        }
    }
}

fn deps_of(item: &PlanItem) -> Vec<String> {
    let mut d = item.depends_on.clone();
    if let Some(p) = &item.parent {
        if !d.contains(p) {
            d.push(p.clone());
        }
    }
    d
}

enum DepState {
    Ready,
    Waiting,
    Failed,
}

fn dep_state(item: &PlanItem, known: &HashSet<String>, st: &HashMap<String, St>) -> DepState {
    let mut waiting = false;
    for d in deps_of(item) {
        if !known.contains(&d) {
            continue;
        }
        match st.get(&d) {
            None => waiting = true,
            Some(s) if s.ok() => {}
            Some(_) => return DepState::Failed,
        }
    }
    if waiting {
        DepState::Waiting
    } else {
        DepState::Ready
    }
}

/// A failure of `e` as an item status (cancellation stays a cancellation).
fn st_of(e: &CoreError) -> St {
    if matches!(e, CoreError::Cancelled) {
        St::Cancelled
    } else {
        St::Failed(Some(error_payload(e)))
    }
}

fn now_ms(t0: Instant) -> u64 {
    t0.elapsed().as_millis() as u64
}

/// MySQL `table_overrides` charsets by data item id (the override names the source table, the
/// MySQL table item's `name`). Only MySQL plans have them.
fn table_charsets(plan: &CopyPlan, my: &MysqlCopyOptions) -> BTreeMap<String, String> {
    if plan.family != CopyFamily::Mysql {
        return BTreeMap::new();
    }
    plan.items
        .iter()
        .filter(|i| i.kind == CopyObjectKind::Table && i.has_data)
        .filter_map(|i| my.table_overrides.iter().find(|o| o.table == i.name).and_then(|o| o.charset.clone()).map(|cs| (i.id.clone(), cs)))
        .collect()
}

// ---------------------------------------------------------------------------------------------
// Run context
// ---------------------------------------------------------------------------------------------

/// Everything a running copy job shares.
struct Ctx {
    db: Arc<BulkDb>,
    job: Arc<JobEntry>,
    copy: Arc<CopyJob>,
    cancel: CancellationToken,
    store: Option<CheckpointStore>,
    run_id: String,
    req: CopyRequest,
    src_conn: Connection,
    tgt_conn: Connection,
    family: CopyFamily,
    opener: Arc<dyn TargetOpener>,
    test_double: bool,
    concurrency: u8,
    session: SessionOpts,
    /// Planner + finalize output.
    planned: PlannedCopy,
    /// Item statuses.
    st: tokio::sync::Mutex<HashMap<String, St>>,
    /// Items the plan creates (write-ahead `created_by_run` flag of `creating`).
    created_by_run: HashSet<String>,
    /// Tables an earlier job of this (resumed) run created.
    created_earlier: HashSet<String>,
    /// Tables whose CREATE (or DROP + CREATE) succeeded in this job (`left_behind`).
    created_now: std::sync::Mutex<HashSet<String>>,
    /// Tables emptied in this job outside their load transaction: a committed DROP + CREATE,
    /// a MySQL TRUNCATE, a PG truncate group (`left_behind.emptied_table` when not reloaded).
    emptied_now: std::sync::Mutex<HashSet<String>>,
    budget: Budget,
    session_no: std::sync::atomic::AtomicU32,
    pg_snapshot: Option<String>,
    /// MySQL consistent snapshot: the one source session every table is read on (4.1).
    mysql_shared_source: Option<SharedSource>,
    target_facts: TargetFacts,
    source_facts: TargetFacts,
    /// Issues that arose while running (snapshot lost, checksum not applicable), added to the
    /// result's warnings once per code and item.
    run_issues: std::sync::Mutex<Vec<CopyIssue>>,
    /// A checkpoint write failed: the stored state may be stale, so the run is not resumable.
    /// `Arc` so `load_table`'s `spawn_blocking` checkpoint task can share the flag.
    checkpoint_lost: Arc<std::sync::atomic::AtomicBool>,
    /// A MySQL load left a chunk transaction open when it stopped (cancel): the server may still
    /// be rolling it back (`SERVER_ROLLBACK_PENDING`, 5.4). OR of every table's writer.
    mysql_rollback_pending: std::sync::atomic::AtomicBool,
    /// Data items whose checksum cannot be compared (PG < 12 sources with float columns).
    no_checksum: HashSet<String>,
    /// MySQL: target ENUM columns without a `''` member, by data item (the `''` such a column
    /// holds is the error value of a non-strict source).
    enum_error_columns: BTreeMap<String, Vec<String>>,
}

/// The MySQL snapshot session shared by the tables of a `table_concurrency = 1` job. It is
/// given up (taken out, closing the snapshot transaction) as soon as a table's stream did not
/// end cleanly or the connection changed: the driver re-opens a killed connection without
/// `time_zone`, READ ONLY or the snapshot, so later tables read their own session.
struct SharedSource {
    conn: tokio::sync::Mutex<Option<LiveConnection>>,
    /// The connection id the snapshot was started on.
    session_id: Option<String>,
}

/// The source settings were not applied: never read a table without them (TIMESTAMPs would
/// shift by the server's zone).
fn session_lost(sql: &str, e: &CoreError) -> CoreError {
    CoreError::Internal(format!("copy: source session lost its settings ({sql}): {e}"))
}

impl Ctx {
    /// Write an item state. `false` when the store refused it (busy, disk full, read-only):
    /// the run is then marked not resumable, and a write-ahead caller fails its item.
    fn checkpoint(&self, item_id: &str, state: CheckpointState, rows: Option<u64>, error: Option<&ErrorPayload>) -> bool {
        let Some(store) = &self.store else { return true };
        let created = matches!(state, CheckpointState::Creating) && self.created_by_run.contains(item_id);
        let err = error.map(|e| redact_values(&e.message));
        match store.set_item(&self.run_id, item_id, state, created, rows, err.as_deref()) {
            Ok(()) => true,
            Err(e) => {
                tracing::warn!(run_id = %self.run_id, item_id, "copy checkpoint: {e}");
                self.checkpoint_lost.store(true, Ordering::Relaxed);
                false
            }
        }
    }

    /// Record a run-time issue once (per code and item).
    fn note(&self, i: CopyIssue) {
        let mut v = self.run_issues.lock().unwrap();
        if !v.iter().any(|x| x.code == i.code && x.item_id == i.item_id) {
            v.push(i);
        }
    }

    /// Tables are no longer read under one snapshot (5.3 `SRC_NOT_SNAPSHOT`, after the fact).
    fn snapshot_lost(&self, why: String) {
        tracing::warn!("copy: {why}");
        let mut i = issue("SRC_NOT_SNAPSHOT", Severity::Warning, format!("tables were not all read under one snapshot: {why}"));
        i.recommendation = Some("copy from a quiet source, or a replica stopped for the copy".into());
        self.note(i);
    }

    fn writer_cfg(&self, session_no: u32) -> WriterConfig {
        let o = &self.req.options;
        let c = self.concurrency.max(1) as u32;
        let piece_mb = o.batch_mb.unwrap_or(8).clamp(1, 16).min((8 / c).max(1));
        let (chunk_bytes, chunk_rows) = super::data::mysql_encode::chunk_limits(&self.target_facts);
        WriterConfig {
            job_id: self.job.id.clone(),
            session_no,
            batch_rows: o.batch_rows.unwrap_or(50_000).clamp(1, 65_536),
            piece_bytes: (piece_mb as usize) << 20,
            chunk_bytes: self.planned.plan.effective.chunk_bytes.unwrap_or(chunk_bytes).min(chunk_bytes),
            chunk_rows,
            lock_timeout_s: o.target_lock_timeout_s.unwrap_or(5).clamp(1, 600),
            fail_on_unmappable: o.on_unmappable == UnmappablePolicy::Fail,
            charset: self.planned.plan.effective.charset.clone(),
            table_charsets: table_charsets(&self.planned.plan, &o.mysql),
            enum_error_columns: self.enum_error_columns.clone(),
            enum_error_null: o.mysql.enum_error == Some(EnumErrorPolicy::Null),
            // The MySQL writer hashes the source as it encodes, so the reader builds no second
            // encoder (4.1 memory budget). PG/SQLite still hash in the reader.
            source_checksum: self.family == CopyFamily::Mysql && o.verify == VerifyMode::Checksum && !self.test_double,
        }
    }

    /// Open a write session `t<n>` with the session settings applied.
    async fn write_session(&self, n: u32) -> Result<Box<dyn TargetSession>> {
        let app = format!("{}{}-t{n}", crate::APP_NAME_PREFIX, self.job.id);
        let s = cancellable(&self.cancel, self.opener.open(&self.tgt_conn, &self.req.target, &app)).await?;
        s.configure(&self.writer_cfg(n));
        for sql in &self.planned.plan.session_sql.target {
            exec_checked(&*s, self.family, sql, StmtContext::Session, &self.cancel).await?;
        }
        Ok(s)
    }

    fn next_session_no(&self) -> u32 {
        self.session_no.fetch_add(1, Ordering::Relaxed)
    }

    async fn set_st(&self, id: &str, s: St) {
        let state = self.planned.plan.items.iter().find(|i| i.id == id).map(|i| s.item_state(i.action)).unwrap_or(CopyItemState::Done);
        self.copy.item_finished(state);
        self.st.lock().await.insert(id.to_string(), s);
    }

    /// Open a source data session (`s<n>`) with the export session settings, the 4.1 source
    /// settings and no inherited statement / idle timeouts. A setting that fails is an error:
    /// a MySQL session without `time_zone = '+00:00'` would shift every TIMESTAMP.
    async fn source_session(&self, n: u32) -> Result<LiveConnection> {
        let app = format!("{}{}-s{n}", crate::APP_NAME_PREFIX, self.job.id);
        let conn = endpoint_connection(&self.src_conn, &self.req.source);
        let live = cancellable(&self.cancel, self.db.open_connection(&conn, self.req.source.password.clone(), &app)).await?;
        cancellable(&self.cancel, live.apply_session(&self.session)).await?;
        let timeouts = super::facts::source_timeout_sql(&self.source_facts, self.session.statement_timeout_ms.is_some());
        let settings: Vec<String> = match self.family {
            CopyFamily::Postgres => {
                let sql_ascii = self.planned.tables.values().any(|t| t.transcode.is_some());
                super::target::pg::source_session_sql(sql_ascii)
            }
            CopyFamily::Mysql => self.planned.plan.session_sql.source.clone(),
            CopyFamily::Sqlite => Vec::new(),
        };
        for sql in settings.iter().chain(timeouts.iter()) {
            match cancellable(&self.cancel, super::facts::source_exec(&live, sql)).await {
                Ok(()) => {}
                Err(CoreError::Cancelled) => return Err(CoreError::Cancelled),
                Err(e) => return Err(session_lost(sql, &e)),
            }
        }
        if let (CopyFamily::Postgres, Some(snap)) = (self.family, &self.pg_snapshot) {
            cancellable(&self.cancel, super::facts::source_exec(&live, "BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY")).await?;
            cancellable(&self.cancel, super::facts::source_exec(&live, &format!("SET TRANSACTION SNAPSHOT {}", preflight::pg_literal(snap)))).await?;
        }
        Ok(live)
    }
}

/// The data items of `planned` whose source table has a column printed through the float
/// output functions: `real` / `double precision`, geometric types, arrays and domains of them,
/// and (conservatively) composite and range types. One catalog query on the meta session.
async fn pg_float_tables(src_live: &LiveConnection, planned: &PlannedCopy) -> Result<HashSet<String>> {
    let m = super::facts::source_query(
        src_live,
        "SELECT DISTINCT n.nspname AS s, c.relname AS t FROM pg_catalog.pg_attribute a \
         JOIN pg_catalog.pg_class c ON c.oid = a.attrelid JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
         JOIN pg_catalog.pg_type t0 ON t0.oid = a.atttypid \
         JOIN pg_catalog.pg_type t1 ON t1.oid = CASE WHEN t0.typtype = 'd' THEN t0.typbasetype ELSE t0.oid END \
         LEFT JOIN pg_catalog.pg_type t2 ON t2.oid = t1.typelem AND t1.typcategory = 'A' \
         WHERE a.attnum > 0 AND NOT a.attisdropped AND c.relkind IN ('r', 'p', 'm', 'f') \
         AND n.nspname NOT IN ('pg_catalog', 'information_schema') \
         AND (t1.typname IN ('float4', 'float8') OR t1.typcategory = 'G' OR t1.typtype IN ('c', 'r', 'm') \
              OR t2.typname IN ('float4', 'float8') OR t2.typcategory = 'G' OR t2.typtype IN ('c', 'r', 'm', 'd'))",
    )
    .await?;
    let found: HashSet<(String, String)> = (0..m.len()).filter_map(|r| Some((m.get(r, "s")?.to_string(), m.get(r, "t")?.to_string()))).collect();
    Ok(planned
        .plan
        .items
        .iter()
        .filter(|i| planned.tables.contains_key(&i.id) && found.contains(&(i.schema.clone().unwrap_or_else(|| "public".into()), i.name.clone())))
        .map(|i| i.id.clone())
        .collect())
}

/// Race a future without server-side work against the token.
async fn cancellable<T>(cancel: &CancellationToken, fut: impl std::future::Future<Output = Result<T>>) -> Result<T> {
    crate::export::cancellable(cancel, fut).await
}

// ---------------------------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------------------------

/// How a job ended before or after it produced a result.
enum End {
    /// A result exists (done, done with problems, failed items, cancelled mid-run).
    Result(CopyResult, Option<CoreError>),
    Rejected(Vec<CopyIssue>),
    Error(CoreError),
}

/// Run one copy job to a terminal state. Spawned by `api::copy_start` / `api::copy_resume`.
pub async fn run_copy_job(db: Arc<BulkDb>, job: Arc<JobEntry>, copy: Arc<CopyJob>, start: CopyStart) -> Result<CopyResult> {
    let run_id = copy.run_id();
    let t0 = Instant::now();
    let mut store = CheckpointStore::open(&db.config.history_path()).map_err(|e| tracing::warn!("copy: checkpoint store unavailable: {e}")).ok();
    let end = run_inner(&db, &job, &copy, start, &mut store, t0).await;
    let target_conn = copy.info().target_conn_id;
    db.catalog_invalidate(&target_conn);
    let out = match end {
        End::Result(result, err) => {
            copy.set_result(result.clone());
            copy.set_phase(CopyPhase::Verify);
            let state = match &err {
                None => JobState::Done,
                Some(CoreError::Cancelled) => JobState::Cancelled,
                Some(_) => JobState::Failed,
            };
            if let Some(s) = &store {
                if let Err(e) = s.finish_run(&run_id, state, Some(&result)) {
                    tracing::warn!(run_id = %run_id, "copy checkpoint: {e}");
                }
            }
            let rows: u64 = result.tables.iter().map(|t| t.rows_written).sum();
            let bytes = job.progress.bytes_out.load(Ordering::Relaxed);
            let mut notes = Vec::new();
            if result.skipped_existing > 0 {
                notes.push(format!("{}개 테이블은 이미 있어 데이터를 복사하지 않았습니다", result.skipped_existing));
            }
            if result.problems > 0 {
                notes.push(format!("{} problem(s) need attention", result.problems));
            }
            tracing::info!(job_id = %job.id, run_id = %run_id, state = ?state, rows, problems = result.problems, "copy finished");
            match err {
                None => db.jobs.finish(&job, Ok(JobResult { rows, bytes, files: Vec::new(), duration_ms: result.duration_ms, verified_rows: None, notes })),
                Some(e) => db.jobs.finish(&job, Err(e)),
            }
            Ok(result)
        }
        End::Rejected(blockers) => {
            let codes: Vec<String> = blockers.iter().map(|b| b.code.clone()).collect();
            let payload = ErrorPayload {
                code: "INVALID".into(),
                message: format!("the copy plan has blockers: {}", codes.join(", ")),
                detail: Some(blockers.iter().map(|b| format!("{}: {}", b.code, b.message)).collect::<Vec<_>>().join("\n")),
            };
            if let Some(s) = &store {
                let _ = s.finish_run(&run_id, JobState::Rejected, None);
            }
            *job.error.lock().unwrap() = Some(payload.clone());
            if let Err(e) = db.jobs.transition(&job, JobState::Rejected) {
                tracing::error!(job_id = %job.id, "cannot mark copy job rejected: {e}");
            }
            let _ = db.jobs.events().send(crate::types::CoreEvent::JobDone(JobDone {
                job_id: job.id.clone(),
                state: JobState::Rejected,
                result: None,
                error: Some(payload),
                exact_count: None,
            }));
            tracing::warn!(job_id = %job.id, codes = ?codes, "copy rejected");
            Err(CoreError::Invalid(format!("copy rejected: {}", codes.join(", "))))
        }
        End::Error(e) => {
            let state = if matches!(e, CoreError::Cancelled) { JobState::Cancelled } else { JobState::Failed };
            if let Some(s) = &store {
                let _ = s.finish_run(&run_id, state, copy.result().as_ref());
            }
            if state == JobState::Failed {
                tracing::error!(job_id = %job.id, code = e.code(), "copy failed: {}", redact_values(&e.to_string()));
            }
            let ret = match &e {
                CoreError::Cancelled => CoreError::Cancelled,
                other => CoreError::Internal(redact_values(&other.to_string())),
            };
            db.jobs.finish(&job, Err(e));
            Err(ret)
        }
    };
    out
}

fn transition(db: &BulkDb, job: &JobEntry, store: &Option<CheckpointStore>, run_id: &str, to: JobState) -> Result<()> {
    db.jobs.transition(job, to)?;
    if let Some(s) = store {
        let _ = s.set_run_state(run_id, to);
    }
    Ok(())
}

fn sanitized(req: &CopyRequest) -> CopyRequest {
    let mut r = req.clone();
    r.source.password = None;
    r.target.password = None;
    r.options.dry_run = false;
    r.options.confirm_production = None;
    r
}

fn in_use_pred(db: &BulkDb) -> impl Fn(&RunRow) -> bool + '_ {
    move |r: &RunRow| {
        if r.owner_instance == db.instance_id() {
            db.jobs.get(&r.last_job_id).is_some_and(|j| !j.state.lock().unwrap().is_terminal())
        } else {
            crate::instances::is_alive(db.config.dir(), &r.owner_instance)
        }
    }
}

fn blocker_from_claim(e: &CoreError) -> Option<CopyIssue> {
    let msg = e.to_string();
    for code in ["RUN_IN_USE", "TARGET_IN_USE"] {
        if let Some(i) = msg.find(code) {
            let text = msg[i + code.len()..].trim_start_matches(':').trim().to_string();
            return Some(issue(code, Severity::Danger, text));
        }
    }
    None
}

/// The lease statement of a family (5.1).
pub fn lease_sql(family: CopyFamily, database: &str) -> Option<String> {
    match family {
        CopyFamily::Postgres => Some("SELECT pg_try_advisory_lock(hashtext('bulkdb-copy')) AS locked".into()),
        CopyFamily::Mysql => Some(format!("SELECT GET_LOCK('bulkdb-copy:{}', 0) AS locked", super::hash::sha256_hex_prefix(database.as_bytes(), 32))),
        CopyFamily::Sqlite => None,
    }
}

/// A held target lease.
pub enum Lease {
    Session(#[allow(dead_code)] Box<dyn TargetSession>),
    File(#[allow(dead_code)] std::fs::File),
    None,
}

/// Take the target lease (5.1). `Ok(None)` when another writer holds it.
pub async fn take_lease(opener: &dyn TargetOpener, conn: &Connection, ep: &CopyEndpoint, family: CopyFamily, app: &str) -> Result<Option<Lease>> {
    match family {
        CopyFamily::Sqlite => {
            let Some(path) = super::target::sqlite::target_path(conn) else {
                return Ok(Some(Lease::None));
            };
            // Next to the canonical file, so every spelling of it (symlink, `file:` URI, letter
            // case) takes the same lease.
            let mut lock_path = super::target::sqlite::canonical_path(&path).into_os_string();
            lock_path.push(".bulkdb-lock");
            let f = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(&lock_path)?;
            match fs4::fs_std::FileExt::try_lock_exclusive(&f) {
                Ok(true) => Ok(Some(Lease::File(f))),
                Ok(false) => Ok(None),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(None),
                Err(e) => Err(e.into()),
            }
        }
        _ => {
            let s = opener.open(conn, ep, app).await?;
            let sql = lease_sql(family, &ep.database).unwrap_or_default();
            guard::check_statement(family, sql.trim_end_matches(" AS locked"), StmtContext::Lease)?;
            let m = s.query_meta(&sql).await?;
            // An empty answer comes only from test doubles: treat it as granted.
            let ok = match m.rows.first().and_then(|r| r.first()).cloned().flatten() {
                None if m.is_empty() => true,
                None => false,
                Some(v) => matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "t" | "true"),
            };
            Ok(if ok { Some(Lease::Session(s)) } else { None })
        }
    }
}

/// `<identity key>/<database>`: the lease registry key of an endpoint.
pub fn endpoint_key(identity: &ServerIdentity, database: &str) -> String {
    format!("{}/{}", identity.key, database.to_ascii_lowercase())
}

/// Other active copy jobs of this process writing `target_key` / targeting `source_key`.
pub fn registry_issues(db: &BulkDb, own_job: Option<&str>, source_key: &str, target_key: &str) -> Vec<CopyIssue> {
    let mut out = Vec::new();
    for j in db.jobs.active() {
        if Some(j.id.as_str()) == own_job {
            continue;
        }
        let Some(c) = &j.copy else { continue };
        let (_, t) = c.keys();
        if t.as_deref() == Some(target_key) {
            out.push(issue("TARGET_IN_USE", Severity::Danger, format!("copy job {} ({}) is writing this target database", j.id, j.label)));
        }
        if t.as_deref() == Some(source_key) {
            out.push(issue("SOURCE_BEING_WRITTEN", Severity::Warning, format!("copy job {} ({}) is writing into this job's source database", j.id, j.label)));
        }
    }
    out
}

async fn run_inner(db: &Arc<BulkDb>, job: &Arc<JobEntry>, copy: &Arc<CopyJob>, start: CopyStart, store: &mut Option<CheckpointStore>, t0: Instant) -> End {
    let cancel = job.cancel.clone();
    let run_id = copy.run_id();
    // -- pending: one export slot while running ----------------------------------------------
    let slots = db.jobs.export_slots.clone();
    let _slot = tokio::select! {
        biased;
        _ = cancel.cancelled() => return End::Error(CoreError::Cancelled),
        p = slots.acquire_owned() => match p { Ok(p) => p, Err(_) => return End::Error(CoreError::Internal("export slots closed".into())) },
    };
    if let Err(e) = transition(db, job, store, &run_id, JobState::Preflight) {
        return End::Error(e);
    }
    copy.set_phase(CopyPhase::Plan);

    // -- request ------------------------------------------------------------------------------
    let (req, expected_plan, mut acknowledged, resume_row, partial) = match start {
        CopyStart::New { req, plan_id, acknowledged } => (*req, Some(plan_id), acknowledged, None, ResumePartial::Fail),
        CopyStart::Resume { req: r } => {
            let Some(s) = store.as_ref() else { return End::Error(CoreError::Internal("copy: the checkpoint store is unavailable".into())) };
            let pred = in_use_pred(db);
            let row = match s.claim_resume(&r.run_id, std::process::id(), db.instance_id(), &job.id, &pred) {
                Ok(row) => row,
                Err(e) => return blocker_from_claim(&e).map(|b| End::Rejected(vec![b])).unwrap_or(End::Error(e)),
            };
            if row.dry_run {
                return End::Error(CoreError::Invalid("dry runs are never resumable".into()));
            }
            if row.result().is_some_and(|r| !r.resumable) {
                return End::Error(CoreError::Invalid(super::api::NOT_RESUMABLE.into()));
            }
            let mut stored = match row.request() {
                Ok(q) => q,
                Err(e) => return End::Error(e),
            };
            stored.source.password = r.source_password.clone();
            stored.target.password = r.target_password.clone();
            stored.options.confirm_production = r.confirm_production.clone();
            stored.options.dry_run = false;
            (stored, None, r.acknowledged.clone(), Some(row), r.partial)
        }
    };
    let src_conn = match db.conn_get(&req.source.conn_id) {
        Ok(c) => c,
        Err(e) => return End::Error(e),
    };
    let tgt_conn = match db.conn_get(&req.target.conn_id) {
        Ok(c) => c,
        Err(e) => return End::Error(e),
    };
    let family = match (CopyFamily::of_driver(&src_conn.driver), CopyFamily::of_driver(&tgt_conn.driver)) {
        (Some(a), Some(b)) if a == b => a,
        (Some(_), Some(_)) => {
            return End::Rejected(vec![issue(
                "FAMILY_MISMATCH",
                Severity::Danger,
                format!("the source ({}) and the target ({}) must be the same database family", src_conn.driver, tgt_conn.driver),
            )])
        }
        _ => return End::Rejected(vec![issue("FAMILY_UNSUPPORTED", Severity::Danger, "copy supports postgres, mysql and sqlite connections")]),
    };
    db.remember_secret(&src_conn, req.source.password.clone());
    db.remember_secret(&tgt_conn, req.target.password.clone());
    let dry_run = req.options.dry_run;
    let opener = db.copy.opener(db);
    let test_double = db.copy.has_test_opener();
    let concurrency = if family == CopyFamily::Sqlite { 1 } else { req.options.table_concurrency.clamp(1, 4) };

    // -- endpoints ----------------------------------------------------------------------------
    let app_m = format!("{}{}-m", crate::APP_NAME_PREFIX, job.id);
    let src_live = match cancellable(&cancel, db.open_connection(&endpoint_connection(&src_conn, &req.source), req.source.password.clone(), &app_m)).await {
        Ok(l) => l,
        Err(e) => return End::Error(e),
    };
    let server = match cancellable(&cancel, src_live.probe()).await {
        Ok(s) => s,
        Err(e) => return End::Error(e),
    };
    // Read only, and the catalog deparse settings (PG DateStyle & co., MySQL time_zone): the
    // meta session reads the DDL the target replays.
    if let Err(e) = cancellable(&cancel, super::facts::apply_meta_session(&src_live, family, &server.version)).await {
        return End::Error(e);
    }
    let src_facts = match cancellable(&cancel, super::facts::read_source_facts(&src_live, &src_conn, &req.source.database)).await {
        Ok(f) => f,
        Err(e) => return End::Error(e),
    };
    let session = {
        let mut recommended = crate::preflight::recommended_session(&server);
        recommended.statement_timeout_ms = None;
        crate::export::merge_session(&req.options.source_session, &src_conn.session, &recommended)
    };
    // The PG meta session holds the exported snapshot idle in its transaction while the workers
    // stream: no idle-in-transaction or statement timeout may end it (pg_dump does the same).
    if family == CopyFamily::Postgres {
        for sql in super::facts::source_timeout_sql(&src_facts, session.statement_timeout_ms.is_some()) {
            if let Err(e) = cancellable(&cancel, super::facts::source_exec(&src_live, &sql)).await {
                return End::Error(e);
            }
        }
    }
    let tgt_meta = match cancellable(&cancel, opener.open(&tgt_conn, &req.target, &app_m)).await {
        Ok(s) => s,
        Err(e) => return End::Error(e),
    };
    let target_facts = tgt_meta.facts().clone();
    let target_identity = super::target::identity_or_default(&target_facts);
    let source_key = endpoint_key(&src_facts.identity, &req.source.database);
    let target_key = endpoint_key(&target_identity, &req.target.database);
    let mut extra = registry_issues(db, Some(&job.id), &source_key, &target_key);
    copy.set_keys(Some(source_key.clone()), Some(target_key.clone()));

    // -- lease ----------------------------------------------------------------------------------
    let app_l = format!("{}{}-l", crate::APP_NAME_PREFIX, job.id);
    let _lease = match cancellable(&cancel, take_lease(&*opener, &tgt_conn, &req.target, family, &app_l)).await {
        Ok(Some(l)) => Some(l),
        Ok(None) => {
            extra.push(issue("TARGET_IN_USE", Severity::Danger, "another BulkDB copy holds the lease of this target database"));
            None
        }
        Err(e) => return End::Error(e),
    };

    // -- claim ----------------------------------------------------------------------------------
    if resume_row.is_none() {
        if let Some(s) = store.as_ref() {
            let row = RunRow {
                run_id: run_id.clone(),
                label: job.label.clone(),
                source_conn_id: src_conn.id.clone(),
                target_conn_id: tgt_conn.id.clone(),
                source_database: req.source.database.clone(),
                target_database: req.target.database.clone(),
                source_identity: src_facts.identity.key.clone(),
                target_identity: target_identity.key.clone(),
                request_json: serde_json::to_string(&sanitized(&req)).unwrap_or_default(),
                plan_id: expected_plan.clone().unwrap_or_default(),
                started_at: job.started_at.clone(),
                finished_at: None,
                state: JobState::Preflight,
                last_job_id: job.id.clone(),
                owner_pid: std::process::id(),
                owner_instance: db.instance_id().to_string(),
                dry_run,
                result_json: None,
            };
            let pred = in_use_pred(db);
            if let Err(e) = s.claim_new(&row, &pred) {
                match blocker_from_claim(&e) {
                    Some(b) => extra.push(b),
                    None => return End::Error(e),
                }
            }
        }
    }
    if let Some(row) = &resume_row {
        if row.target_identity != target_identity.key || !row.target_database.eq_ignore_ascii_case(&req.target.database) {
            extra.push(issue("RESUME_TARGET_CHANGED_SERVER", Severity::Danger, "the target connection points at another server or database than when the run started"));
        }
        if row.source_identity != src_facts.identity.key {
            extra.push(issue("RESUME_SOURCE_CHANGED_SERVER", Severity::Warning, "the source server is not the one the run started from"));
        }
    }
    let resume = match (&resume_row, store.as_ref()) {
        (Some(row), Some(s)) => match s.items(&row.run_id) {
            Ok(items) => Some(ResumeState { run_id: row.run_id.clone(), items, partial }),
            Err(e) => return End::Error(e),
        },
        _ => None,
    };

    // -- plan -------------------------------------------------------------------------------------
    if family == CopyFamily::Postgres {
        if let (Some(wm), Some(ram)) = (&session.work_mem, server.ram_bytes) {
            extra.extend(crate::preflight::check_work_mem(wm, ram).map(preflight::from_preflight));
        }
    }
    let planned = {
        let src_meta = match super::schema::source_meta(&src_live) {
            Ok(m) => m,
            Err(e) => return End::Error(e),
        };
        #[cfg(feature = "mysql")]
        let mysql_facts = crate::source::mysql_raw(src_live.db()).map(|r| r.facts().clone());
        #[cfg(not(feature = "mysql"))]
        let mysql_facts: Option<()> = None;
        let input = super::plan::PlanInput {
            req: &req,
            source: &*src_meta,
            source_conn: &src_conn,
            source_server: &server,
            mysql_source: mysql_facts.as_ref(),
            target: &*tgt_meta,
            target_conn: &tgt_conn,
            resume: resume.as_ref(),
            cancel: &cancel,
        };
        match super::plan::build_plan(input).await {
            Ok(p) => p,
            Err(e) => return End::Error(e),
        }
    };
    let mut planned = planned;
    // The export preflight's concurrent-session check, for the tables this copy reads (5.3).
    match cancellable(&cancel, async { Ok(preflight::source_session_issues(src_live.monitor(), &planned.plan).await) }).await {
        Ok(issues) => extra.extend(issues),
        Err(e) => return End::Error(e),
    }
    extra.extend(preflight::unknown_item_issues(&planned.plan, &req.options));
    match preflight::mysql_enum_issues(&src_live, &planned, &req.source.database, req.options.mysql.enum_error, &cancel).await {
        Ok(issues) => extra.extend(issues),
        Err(e) => return End::Error(e),
    }
    let grants = if family == CopyFamily::Mysql { tgt_meta.query_meta("SHOW GRANTS").await.ok() } else { None };
    let mut tf = target_facts.clone();
    tf.identity = target_identity.clone();
    preflight::fill_session_sql(&mut planned.plan, &tf, Some(&src_facts), req.options.target_lock_timeout_s.unwrap_or(5), req.options.mysql.zero_dates);
    {
        let mut fi = FinalizeInput::new(&src_conn, &tgt_conn, &src_facts.identity, &tf);
        fi.at_start = true;
        fi.confirm_production = req.options.confirm_production.as_deref();
        fi.source = Some(&src_facts);
        fi.extra = extra;
        fi.grants = grants.as_ref();
        if let Err(e) = preflight::finalize(&mut planned, &fi) {
            return End::Error(e);
        }
    }
    if let Some(row) = &resume_row {
        planned.plan.resume_of = Some(row.run_id.clone());
    }
    if test_double && req.options.verify == VerifyMode::Checksum {
        planned.plan.blockers.push(issue("CHECKSUM_UNSUPPORTED", Severity::Danger, "checksum verification is not supported on this target"));
    }
    let loads_list: Vec<(String, Option<u64>)> = planned
        .plan
        .items
        .iter()
        .filter(|i| {
            (planned.tables.contains_key(&i.id) && (loads(i.action) || i.action == PlanAction::ResumeDone))
                || (i.action == PlanAction::SkipExisting && i.kind == CopyObjectKind::Table)
        })
        .map(|i| (i.id.clone(), i.est_rows))
        .collect();
    let items_total = planned.plan.items.iter().filter(|i| executes(i.action)).count() as u32;
    copy.set_plan(&planned.plan, &loads_list, items_total);
    {
        let mut info = copy.info();
        info.dry_run = dry_run;
        copy.set_info(info);
    }
    if let Some(s) = store.as_ref() {
        if resume_row.is_none() {
            let _ = s.set_run_plan(&run_id, &src_facts.identity.key, &target_identity.key, &planned.plan.plan_id);
        }
    }
    if !planned.plan.blockers.is_empty() {
        return End::Rejected(planned.plan.blockers.clone());
    }

    // -- acknowledgements ---------------------------------------------------------------------
    if let Some(expected) = &expected_plan {
        if !expected.is_empty() && *expected != planned.plan.plan_id {
            let detail = match db.copy.recent_plan(expected) {
                Some(old) => {
                    let ids = preflight::plan_diff(&old, &planned.plan);
                    if ids.is_empty() {
                        "the issues or the effective settings changed".to_string()
                    } else {
                        format!("changed items: {}", ids.join(", "))
                    }
                }
                None => format!("the reviewed plan {expected} is not known to this process; review the new plan"),
            };
            let mut i = issue("PLAN_CHANGED", Severity::Warning, "the plan changed since it was reviewed");
            i.detail = Some(detail);
            planned.plan.warnings.push(i);
            planned.plan.acks.push(AckNeed { code: "PLAN_CHANGED".into(), key: "PLAN_CHANGED".into(), severity: Severity::Warning, items: Vec::new() });
            copy.set_plan(&planned.plan, &loads_list, items_total);
        }
    }
    let missing: Vec<AckNeed> = planned.plan.acks.iter().filter(|a| !acknowledged.contains(&a.key)).cloned().collect();
    if !missing.is_empty() {
        let (tx, rx) = tokio::sync::oneshot::channel::<Vec<String>>();
        *job.resume.lock().unwrap() = Some(tx);
        let report = PreflightReport {
            warnings: missing
                .iter()
                .map(|a| {
                    let msg = planned
                        .plan
                        .warnings
                        .iter()
                        .chain(planned.plan.items.iter().flat_map(|i| i.warnings.iter()))
                        .find(|w| w.code == a.code)
                        .map(|w| w.message.clone())
                        .unwrap_or_else(|| a.code.clone());
                    PreflightIssue {
                        code: a.key.clone(),
                        severity: a.severity,
                        message: msg,
                        detail: if a.items.is_empty() { None } else { Some(a.items.join("\n")) },
                        recommendation: None,
                    }
                })
                .collect(),
            ..PreflightReport::default()
        };
        *job.preflight.lock().unwrap() = Some(report);
        if let Err(e) = transition(db, job, store, &run_id, JobState::Warned) {
            return End::Error(e);
        }
        tracing::info!(job_id = %job.id, keys = ?missing.iter().map(|a| &a.key).collect::<Vec<_>>(), "copy waits for acknowledgements");
        let keys = tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                job.resume.lock().unwrap().take();
                return End::Error(CoreError::Cancelled);
            }
            r = rx => match r { Ok(k) => k, Err(_) => return End::Error(CoreError::Cancelled) },
        };
        acknowledged.extend(keys);
        // The job may have waited in `warned` for a long time. The write-permission and
        // production checks ran before the wait, against a snapshot of the target connection.
        // Re-read it now and reject if, in the meantime, someone cleared 쓰기 허용, pointed the
        // connection at another server or database, or tagged it production: an acknowledged
        // plan must not write a target the user no longer authorised.
        let fresh_tgt = match db.conn_get(&req.target.conn_id) {
            Ok(c) => c,
            Err(e) => return End::Error(e),
        };
        let mut late: Vec<CopyIssue> = Vec::new();
        if crate::config::endpoint_changed(&tgt_conn, &fresh_tgt) {
            late.push(issue(
                "RESUME_TARGET_CHANGED_SERVER",
                Severity::Danger,
                "the target connection now points at another server or database than when the plan was acknowledged",
            ));
        } else if fresh_tgt.write_allowed != Some(true) {
            late.push(issue("TARGET_NOT_WRITABLE", Severity::Danger, format!("the target connection {} is no longer allowed to be written to", fresh_tgt.name)));
        }
        if fresh_tgt.tag == Some(crate::types::ConnTag::Production) && tgt_conn.tag != Some(crate::types::ConnTag::Production) {
            late.push(issue(
                "PRODUCTION_CONFIRM",
                Severity::Danger,
                format!("the target was tagged production after review: type its name ({}) to start", fresh_tgt.name),
            ));
        }
        if !late.is_empty() {
            return End::Rejected(late);
        }
    }

    // -- running --------------------------------------------------------------------------------
    if let Err(e) = transition(db, job, store, &run_id, JobState::Running) {
        return End::Error(e);
    }
    job.progress.set_phase(crate::types::JobPhase::Transferring);
    let expected_rows: u64 = planned.plan.items.iter().filter(|i| planned.tables.contains_key(&i.id) && loads(i.action)).filter_map(|i| i.est_rows).sum();
    job.progress.expected_rows.store(expected_rows, Ordering::Relaxed);
    let created_by_run: HashSet<String> = planned
        .plan
        .items
        .iter()
        .filter(|i| matches!(i.action, PlanAction::Create | PlanAction::DropRecreate) || resume.as_ref().is_some_and(|r| r.items.get(&i.id).is_some_and(|x| x.1)))
        .map(|i| i.id.clone())
        .collect();
    let created_earlier: HashSet<String> = resume.as_ref().map(|r| r.items.iter().filter(|(_, v)| v.1).map(|(k, _)| k.clone()).collect()).unwrap_or_default();

    // PG consistent snapshot: exported by the meta session, imported by each worker.
    let mut pg_snapshot = None;
    let mut no_snapshot: Option<String> = None;
    if family == CopyFamily::Postgres && planned.plan.effective.consistent_snapshot && !dry_run {
        let r = cancellable(&cancel, async {
            super::facts::source_exec(&src_live, "BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY").await?;
            super::facts::source_query(&src_live, "SELECT pg_export_snapshot() AS s").await
        })
        .await;
        match r.map(|m| m.get(0, "s").map(str::to_string)) {
            Ok(Some(s)) => pg_snapshot = Some(s),
            Ok(None) => no_snapshot = Some("the source exported no snapshot".into()),
            Err(CoreError::Cancelled) => return End::Error(CoreError::Cancelled),
            Err(e) => {
                let _ = super::facts::source_exec(&src_live, "ROLLBACK").await;
                no_snapshot = Some(format!("the source could not export a snapshot ({})", redact_values(&e.to_string())));
            }
        }
    }
    let ctx = Ctx {
        db: db.clone(),
        job: job.clone(),
        copy: copy.clone(),
        cancel: cancel.clone(),
        store: store.take(),
        run_id: run_id.clone(),
        req,
        src_conn,
        tgt_conn,
        family,
        opener,
        test_double,
        concurrency,
        session,
        planned,
        st: tokio::sync::Mutex::new(HashMap::new()),
        created_by_run,
        created_earlier,
        created_now: std::sync::Mutex::new(HashSet::new()),
        emptied_now: std::sync::Mutex::new(HashSet::new()),
        budget: Budget::new(concurrency),
        session_no: std::sync::atomic::AtomicU32::new(1),
        pg_snapshot,
        mysql_shared_source: None,
        target_facts: tf,
        source_facts: src_facts,
        run_issues: std::sync::Mutex::new(Vec::new()),
        checkpoint_lost: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        mysql_rollback_pending: std::sync::atomic::AtomicBool::new(false),
        no_checksum: HashSet::new(),
        enum_error_columns: BTreeMap::new(),
    };
    let mut ctx = ctx;
    if family == CopyFamily::Mysql {
        ctx.enum_error_columns = preflight::mysql_enum_error_columns(&ctx.planned);
    }
    if family == CopyFamily::Postgres && ctx.req.options.verify == VerifyMode::Checksum && !dry_run && !ctx.source_facts.at_least(12, 0, 0) {
        // PG 10 / 11 print floats with extra_float_digits = 3 as DBL_DIG + 3 digits, PG 12+
        // the shortest exact form: the raw CSV of such columns never compares equal.
        let ids = cancellable(&cancel, pg_float_tables(&src_live, &ctx.planned)).await;
        ctx.no_checksum = match ids {
            Ok(ids) => ids,
            Err(CoreError::Cancelled) => {
                *store = ctx.store.take();
                return End::Error(CoreError::Cancelled);
            }
            Err(e) => {
                tracing::warn!("copy: cannot read the float columns of the source ({e}); no checksums");
                ctx.planned.tables.keys().cloned().collect()
            }
        };
        let mut skipped: Vec<&String> = ctx.no_checksum.iter().collect();
        skipped.sort();
        for id in skipped {
            let mut x = issue(
                CODE_CHECKSUM_NOT_APPLICABLE,
                Severity::Info,
                "the source (PostgreSQL before 12) prints floating-point values differently from the target: this table is verified by row count only",
            );
            x.item_id = Some(id.clone());
            ctx.note(x);
        }
    }
    if family == CopyFamily::Mysql && ctx.planned.plan.effective.consistent_snapshot && !dry_run {
        if concurrency != 1 {
            no_snapshot = Some("a consistent snapshot needs table concurrency 1".into());
        } else {
            match ctx.source_session(0).await {
                Ok(live) => {
                    // A consistent read needs REPEATABLE READ (mysqldump --single-transaction):
                    // a source configured READ-COMMITTED would read each table at its own time.
                    let r = cancellable(&cancel, async {
                        super::facts::source_exec(&live, "SET SESSION TRANSACTION ISOLATION LEVEL REPEATABLE READ").await?;
                        super::facts::source_exec(&live, "START TRANSACTION WITH CONSISTENT SNAPSHOT").await
                    })
                    .await;
                    match r {
                        Ok(()) => ctx.mysql_shared_source = Some(SharedSource { session_id: live.session_id(), conn: tokio::sync::Mutex::new(Some(live)) }),
                        Err(CoreError::Cancelled) => {
                            *store = ctx.store.take();
                            return End::Error(CoreError::Cancelled);
                        }
                        Err(e) => no_snapshot = Some(format!("the source could not start a consistent snapshot ({})", redact_values(&e.to_string()))),
                    }
                }
                Err(e) => {
                    *store = ctx.store.take();
                    return End::Error(e);
                }
            }
        }
    }
    if let Some(why) = no_snapshot {
        ctx.planned.plan.effective.consistent_snapshot = false;
        copy.set_plan(&ctx.planned.plan, &loads_list, items_total);
        ctx.snapshot_lost(why);
    }
    let result = if dry_run { dry_run_items(&ctx, &src_live).await } else { execute(&ctx, &src_live).await };
    *store = ctx.store.take();
    drop(_lease);
    match result {
        Ok(mut r) => {
            r.duration_ms = now_ms(t0);
            let failed = r.tables.iter().any(|t| t.state == CopyItemState::Failed) || r.objects.iter().any(|o| o.state == CopyItemState::Failed);
            let cancelled = cancel.is_cancelled() || r.tables.iter().any(|t| t.state == CopyItemState::Cancelled);
            let err = if cancelled {
                Some(CoreError::Cancelled)
            } else if failed {
                let n = r.tables.iter().filter(|t| t.state == CopyItemState::Failed).count() + r.objects.iter().filter(|o| o.state == CopyItemState::Failed).count();
                Some(CoreError::query(format!("{n} item(s) failed; the run can be resumed")))
            } else {
                None
            };
            End::Result(r, err)
        }
        Err(e) => End::Error(e),
    }
}

// ---------------------------------------------------------------------------------------------
// Execution
// ---------------------------------------------------------------------------------------------

/// Run every item of a real (non dry-run) job.
async fn execute(ctx: &Ctx, src_live: &LiveConnection) -> Result<CopyResult> {
    let plan = &ctx.planned.plan;
    let known: HashSet<String> = plan.items.iter().map(|i| i.id.clone()).collect();
    // Items that run nothing.
    for i in &plan.items {
        if !executes(i.action) {
            let s = if i.action == PlanAction::ResumeDone { St::Done } else { St::NotRun };
            ctx.st.lock().await.insert(i.id.clone(), s);
            if i.action == PlanAction::SkipExisting {
                ctx.checkpoint(&i.id, CheckpointState::Skipped, None, None);
                ctx.copy.update_table(&i.id, |p| p.state = CopyItemState::Skipped);
            } else if i.action == PlanAction::ResumeDone {
                ctx.copy.update_table(&i.id, |p| p.state = CopyItemState::Done);
            }
        }
    }
    let mut results: BTreeMap<String, CopyTableResult> = BTreeMap::new();
    let ddl = ctx.write_session(0).await?;

    // -- pre_data ---------------------------------------------------------------------------
    ctx.copy.set_phase(CopyPhase::PreData);
    let mut pre: Vec<&PlanItem> = plan.items.iter().filter(|i| i.phase == CopyPhase::PreData && executes(i.action)).collect();
    pre.sort_by_key(|i| i.order);
    for item in pre {
        let has_spec = ctx.planned.tables.contains_key(&item.id);
        let sql = split_item_sql(item, has_spec);
        run_ddl_item(ctx, &*ddl, &known, item, &sql.ddl, has_spec).await;
        if ctx.cancel.is_cancelled() {
            break;
        }
    }
    // Close the DDL session for the data phase: the data workers each open their own write
    // session, so holding this idle one open would push the peak target sessions to 3 + c, one
    // more than the plan reports (2 + c). It is reopened for post-data and finalize below.
    drop(ddl);

    // -- data (+ per-table post_data) ------------------------------------------------------------
    ctx.copy.set_phase(CopyPhase::Data);
    let data_items: Vec<&PlanItem> =
        plan.items.iter().filter(|i| executes(i.action) && (i.phase == CopyPhase::Data || (ctx.planned.tables.contains_key(&i.id) && loads(i.action)))).collect();
    let data_ids: HashSet<&str> = data_items.iter().map(|i| i.id.as_str()).collect();
    let mut attached: HashMap<&str, Vec<&PlanItem>> = HashMap::new();
    let mut rest_post: Vec<&PlanItem> = Vec::new();
    for i in plan.items.iter().filter(|i| i.phase == CopyPhase::PostData && executes(i.action)) {
        match &i.parent {
            Some(p) if i.kind != CopyObjectKind::ForeignKey && data_ids.contains(p.as_str()) && ctx.planned.tables.contains_key(p) => {
                attached.entry(p.as_str()).or_default().push(i)
            }
            _ => rest_post.push(i),
        }
    }
    for v in attached.values_mut() {
        v.sort_by_key(|i| i.order);
    }
    let mut waiting: Vec<&PlanItem> = data_items.clone();
    waiting.sort_by_key(|i| i.order);
    let mut running = FuturesUnordered::new();
    while !waiting.is_empty() || !running.is_empty() {
        if !ctx.cancel.is_cancelled() {
            let st = ctx.st.lock().await.clone();
            let mut i = 0;
            while i < waiting.len() && running.len() < ctx.concurrency as usize {
                // Failed in pre_data (its CREATE): never loaded.
                if let Some(s) = st.get(&waiting[i].id) {
                    let item = waiting.remove(i);
                    if ctx.planned.tables.contains_key(&item.id) {
                        let mut r = skipped_result(item, Vec::new());
                        r.state = s.item_state(item.action);
                        if let St::Failed(p) = s {
                            r.error = p.clone();
                        }
                        results.insert(item.id.clone(), r);
                    }
                    for p in attached.remove(item.id.as_str()).unwrap_or_default() {
                        ctx.set_st(&p.id, St::DepFailed).await;
                    }
                    continue;
                }
                match dep_state(waiting[i], &known, &st) {
                    DepState::Ready => {
                        let item = waiting.remove(i);
                        let posts = attached.remove(item.id.as_str()).unwrap_or_default();
                        running.push(run_data_item(ctx, &known, item, posts));
                    }
                    DepState::Failed => {
                        let item = waiting.remove(i);
                        skip_dependants(ctx, item, attached.remove(item.id.as_str()).unwrap_or_default(), &mut results).await;
                    }
                    DepState::Waiting => i += 1,
                }
            }
        }
        if running.is_empty() {
            // Nothing can start: cancelled, or waiting on items outside the data phase.
            for item in waiting.drain(..) {
                if ctx.cancel.is_cancelled() {
                    ctx.set_st(&item.id, St::Cancelled).await;
                    ctx.copy.update_table(&item.id, |p| p.state = CopyItemState::Cancelled);
                } else {
                    skip_dependants(ctx, item, attached.remove(item.id.as_str()).unwrap_or_default(), &mut results).await;
                }
            }
            break;
        }
        if let Some((id, Some(r))) = running.next().await {
            results.insert(id, r);
        }
    }
    // Attached post-data items whose table never ran.
    for (_, posts) in attached.drain() {
        for p in posts {
            if !ctx.st.lock().await.contains_key(&p.id) {
                rest_post.push(p);
            }
        }
    }
    // The last read has ended: end the snapshots now, not after post-data, so they hold back
    // vacuum / purge on the source no longer than needed. The PG meta session stays open
    // (outside a transaction) for finalize.
    if ctx.pg_snapshot.is_some() {
        let _ = cancellable(&ctx.cancel, super::facts::source_exec(src_live, "COMMIT")).await;
    }
    if let Some(shared) = &ctx.mysql_shared_source {
        if let Some(live) = shared.conn.lock().await.take() {
            let _ = cancellable(&ctx.cancel, super::facts::source_exec(&live, "COMMIT")).await;
        }
    }

    // -- remaining post_data (FKs after every table) -------------------------------------------
    ctx.copy.set_phase(CopyPhase::PostData);
    // Post-data and finalize only run when the job was not cancelled (a cancel leaves their items
    // for `build_result` to mark). When it runs, reopen the DDL session closed for the data phase;
    // as with the initial open a genuine failure ends the job, and the committed data is resumable.
    if !ctx.cancel.is_cancelled() {
        let ddl = ctx.write_session(0).await?;
        rest_post.sort_by_key(|i| (i.kind == CopyObjectKind::Trigger, i.order));
        for item in rest_post {
            if ctx.cancel.is_cancelled() {
                ctx.set_st(&item.id, St::Cancelled).await;
                continue;
            }
            run_ddl_item(ctx, &*ddl, &known, item, &item.target_sql, false).await;
        }

        // -- finalize -----------------------------------------------------------------------------
        ctx.copy.set_phase(CopyPhase::Finalize);
        let mut fin: Vec<&PlanItem> = plan.items.iter().filter(|i| i.phase == CopyPhase::Finalize && executes(i.action)).collect();
        fin.sort_by_key(|i| i.order);
        let seq_items: Vec<&PlanItem> = fin.iter().copied().filter(|i| matches!(i.kind, CopyObjectKind::SequenceValue | CopyObjectKind::AutoIncrement)).collect();
        let mut values: BTreeMap<String, (i128, bool)> = BTreeMap::new();
        if !seq_items.is_empty() && !ctx.cancel.is_cancelled() {
            let st = ctx.st.lock().await.clone();
            let ready: Vec<&PlanItem> = seq_items.iter().copied().filter(|i| matches!(dep_state(i, &known, &st), DepState::Ready)).collect();
            if !ready.is_empty() {
                match super::schema::source_meta(src_live) {
                    Ok(meta) => match cancellable(&ctx.cancel, super::schema::read_finalize_values(&*meta, &ready)).await {
                        Ok(v) => values = v,
                        Err(e) => tracing::warn!("copy: cannot read finalize values: {e}"),
                    },
                    Err(e) => tracing::warn!("copy: no source meta session for finalize: {e}"),
                }
            }
        }
        for item in fin {
            if ctx.cancel.is_cancelled() {
                ctx.set_st(&item.id, St::Cancelled).await;
                continue;
            }
            if matches!(item.kind, CopyObjectKind::SequenceValue | CopyObjectKind::AutoIncrement) {
                let st = ctx.st.lock().await.clone();
                if !matches!(dep_state(item, &known, &st), DepState::Ready) {
                    ctx.set_st(&item.id, St::DepFailed).await;
                    continue;
                }
                let Some(v) = values.get(&item.id).copied() else {
                    ctx.set_st(&item.id, St::Failed(Some(ErrorPayload { code: "QUERY".into(), message: "the source value could not be read".into(), detail: None })))
                        .await;
                    continue;
                };
                // The floor rule (3.5: a sequence never moves backwards) needs the target's value;
                // when it cannot be read the sequence is left as is, never set to the source value.
                let floor = match raced(&*ddl, &ctx.cancel, super::schema::read_target_floor(&*ddl, item)).await {
                    Ok(f) => f,
                    Err(CoreError::Cancelled) => {
                        ctx.set_st(&item.id, St::Cancelled).await;
                        continue;
                    }
                    Err(e) if uses_target_floor(item) => {
                        tracing::warn!(item = %item.id, "copy: target floor unreadable: {e}");
                        let p = error_payload(&copy_fail(
                            CODE_SEQUENCE_FLOOR_UNREADABLE,
                            format!("the target's current value could not be read, so the sequence was not set ({})", redact_values(&e.to_string())),
                        ));
                        ctx.checkpoint(&item.id, CheckpointState::Failed, None, Some(&p));
                        ctx.set_st(&item.id, St::Failed(Some(p))).await;
                        continue;
                    }
                    Err(e) => {
                        tracing::warn!(item = %item.id, "copy: target floor unreadable: {e}");
                        None
                    }
                };
                let sql = super::translate::finalize_sql(item, v, floor);
                let stmts: Vec<String> = if sql.trim().is_empty() { Vec::new() } else { vec![sql] };
                run_ddl_item(ctx, &*ddl, &known, item, &stmts, false).await;
            } else {
                run_ddl_item(ctx, &*ddl, &known, item, &item.target_sql, false).await;
            }
        }
        drop(ddl);
    }

    // -- verification -------------------------------------------------------------------------
    ctx.copy.set_phase(CopyPhase::Verify);
    let _ = transition(&ctx.db, &ctx.job, &ctx.store, &ctx.run_id, JobState::Finishing);
    let mut item_problems: HashMap<String, Vec<String>> = HashMap::new();
    if ctx.family == CopyFamily::Mysql && ctx.req.options.verify != VerifyMode::None && !ctx.cancel.is_cancelled() {
        if let Ok(probe) = cancellable(&ctx.cancel, ctx.opener.open(&ctx.tgt_conn, &ctx.req.target, &format!("{}{}-v", crate::APP_NAME_PREFIX, ctx.job.id))).await {
            let st = ctx.st.lock().await.clone();
            for item in plan.items.iter().filter(|i| i.kind == CopyObjectKind::ForeignKey && st.get(&i.id) == Some(&St::Done)) {
                for sql in &item.target_sql {
                    for fk in verify::mysql_foreign_keys(sql) {
                        let q = verify::orphan_probe_sql(&fk);
                        if guard::check_read(CopyFamily::Mysql, &q).is_err() {
                            continue;
                        }
                        if let Ok(m) = raced(&*probe, &ctx.cancel, probe.query_meta(&q)).await {
                            if !m.is_empty() {
                                item_problems.entry(item.id.clone()).or_default().push(CODE_FK_ORPHANS.to_string());
                            }
                        }
                    }
                }
            }
        }
    }
    // Skipped existing tables (one target probe session for all of them).
    let mut skip_probe: Option<Box<dyn TargetSession>> = None;
    for item in plan.items.iter().filter(|i| i.action == PlanAction::SkipExisting && (i.kind == CopyObjectKind::Table || i.has_data)) {
        let mut problems = Vec::new();
        if item.warnings.iter().any(|w| w.code == CODE_SKIPPED_DIFFERS) {
            problems.push(CODE_SKIPPED_DIFFERS.to_string());
        }
        let mut target_count = None;
        if ctx.req.options.verify_skipped && !ctx.cancel.is_cancelled() {
            if skip_probe.is_none() {
                let app = format!("{}{}-v", crate::APP_NAME_PREFIX, ctx.job.id);
                skip_probe = cancellable(&ctx.cancel, ctx.opener.open(&ctx.tgt_conn, &ctx.req.target, &app)).await.ok();
            }
            let counted = match &skip_probe {
                Some(p) => count_skipped(ctx, src_live, &**p, item).await,
                None => None,
            };
            if let Some(n) = counted {
                target_count = Some(n.1);
                if n.0 != n.1 && !problems.iter().any(|p| p == CODE_SKIPPED_DIFFERS) {
                    problems.push(CODE_SKIPPED_DIFFERS.to_string());
                }
            }
        }
        results.insert(
            item.id.clone(),
            CopyTableResult {
                item_id: item.id.clone(),
                target_name: item.target_name.clone(),
                state: CopyItemState::Skipped,
                rows_read: 0,
                rows_written: 0,
                target_count,
                verify: VerifyOutcome::NotRun,
                checksum_source: None,
                checksum_target: None,
                unmappable_chars: 0,
                data_warnings: 0,
                warning_samples: Vec::new(),
                value_fixes: Vec::new(),
                problems,
                duration_ms: 0,
                method: None,
                error: None,
            },
        );
    }
    Ok(build_result(ctx, results, item_problems).await)
}

/// Exact counts of a skipped table on both sides (`verify_skipped`), both raced against the
/// job's token (the source count with the driver's own server-side cancel).
async fn count_skipped(ctx: &Ctx, src_live: &LiveConnection, probe: &dyn TargetSession, item: &PlanItem) -> Option<(u64, u64)> {
    let target = TargetTable {
        schema: item.target_schema.clone().or_else(|| if ctx.family == CopyFamily::Mysql { Some(ctx.req.target.database.clone()) } else { None }),
        name: item.target_name.clone(),
    };
    let t = raced(probe, &ctx.cancel, probe.count_rows(&target)).await.ok()?;
    let src_sql = match ctx.family {
        CopyFamily::Postgres => {
            format!("SELECT count(*) AS n FROM ONLY {}.{}", super::target::pg::qi(item.schema.as_deref().unwrap_or("public")), super::target::pg::qi(&item.name))
        }
        CopyFamily::Mysql => {
            format!("SELECT COUNT(*) AS n FROM {}.{}", super::data::mysql_encode::qi(&ctx.req.source.database), super::data::mysql_encode::qi(&item.name))
        }
        CopyFamily::Sqlite => {
            let meta = super::schema::source_meta(src_live).ok()?;
            let m = cancellable(&ctx.cancel, meta.query(&format!("SELECT count(*) AS n FROM {}", super::target::sqlite::qi(&item.name)))).await.ok()?;
            return Some((m.get(0, "n")?.parse().ok()?, t));
        }
    };
    let m = super::facts::source_query_cancellable(src_live, &src_sql, &ctx.cancel, DRIVER_GRACE).await.ok()?;
    Some((m.get(0, "n")?.parse().ok()?, t))
}

/// Mark a data item and its attached post-data items skipped (`DEPENDENCY_FAILED`).
///
/// No checkpoint is written: the item keeps the state it had (none, `created`, or an earlier
/// job's), so a resume runs it again. `skipped` means `skip_existing` only, which a resume
/// treats as finished.
async fn skip_dependants(ctx: &Ctx, item: &PlanItem, posts: Vec<&PlanItem>, results: &mut BTreeMap<String, CopyTableResult>) {
    ctx.set_st(&item.id, St::DepFailed).await;
    ctx.copy.update_table(&item.id, |p| p.state = CopyItemState::Skipped);
    if ctx.planned.tables.contains_key(&item.id) {
        results.insert(item.id.clone(), skipped_result(item, vec![CODE_DEPENDENCY_FAILED.to_string()]));
    }
    for p in posts {
        ctx.set_st(&p.id, St::DepFailed).await;
    }
}

fn skipped_result(item: &PlanItem, problems: Vec<String>) -> CopyTableResult {
    CopyTableResult {
        item_id: item.id.clone(),
        target_name: item.target_name.clone(),
        state: CopyItemState::Skipped,
        rows_read: 0,
        rows_written: 0,
        target_count: None,
        verify: VerifyOutcome::NotRun,
        checksum_source: None,
        checksum_target: None,
        unmappable_chars: 0,
        data_warnings: 0,
        warning_samples: Vec::new(),
        value_fixes: Vec::new(),
        problems,
        duration_ms: 0,
        method: None,
        error: None,
    }
}

/// Execute the DDL statements of one item on `session` (write-ahead checkpoints, deps).
async fn run_ddl_item(ctx: &Ctx, session: &dyn TargetSession, known: &HashSet<String>, item: &PlanItem, stmts: &[String], is_table: bool) {
    if ctx.st.lock().await.contains_key(&item.id) {
        return;
    }
    {
        let mut st = ctx.st.lock().await.clone();
        if item.phase == CopyPhase::PreData {
            // Pre-data DDL only needs its dependencies to exist: tables that are created (or
            // already exist) count, unless they failed.
            for t in ctx.planned.plan.items.iter().filter(|t| t.kind == CopyObjectKind::Table) {
                st.entry(t.id.clone()).or_insert(St::Done);
            }
        }
        match dep_state(item, known, &st) {
            DepState::Ready => {}
            _ => {
                // No checkpoint (see `skip_dependants`): a resume runs the item again.
                ctx.set_st(&item.id, St::DepFailed).await;
                ctx.copy.update_table(&item.id, |p| p.state = CopyItemState::Skipped);
                return;
            }
        }
    }
    if stmts.is_empty() && is_table {
        // Nothing to create (truncate / append tables): the data phase does the rest.
        return;
    }
    let fail = |e: CoreError| async move {
        tracing::warn!(item = %item.id, code = e.code(), "copy item failed: {}", redact_values(&e.to_string()));
        let s = st_of(&e);
        if let St::Failed(p) = &s {
            ctx.checkpoint(&item.id, CheckpointState::Failed, None, p.as_ref());
        }
        if is_table {
            let p = error_payload(&e);
            ctx.copy.update_table(&item.id, |t| {
                t.state = if matches!(e, CoreError::Cancelled) { CopyItemState::Cancelled } else { CopyItemState::Failed };
                t.error = Some(p);
            });
        }
        ctx.set_st(&item.id, s).await;
    };
    // Write-ahead: without the stored intent a resume could not know what this job did.
    if !ctx.checkpoint(&item.id, CheckpointState::Creating, None, None) {
        fail(copy_fail(CODE_CHECKPOINT_FAILED, "the run's checkpoint could not be stored; the item was not run")).await;
        return;
    }
    if is_table {
        ctx.copy.update_table(&item.id, |p| p.state = CopyItemState::Creating);
    }
    for sql in stmts {
        if let Err(e) = exec_checked(session, ctx.family, sql, StmtContext::Item { kind: item.kind, action: item.action }, &ctx.cancel).await {
            fail(e).await;
            // A multi-statement item (a MySQL trigger with its own sql_mode and collation) that
            // stopped half-way leaves its session settings behind: restore the prelude before
            // the next item runs on this session.
            restore_session(ctx, session).await;
            return;
        }
    }
    note_effects(ctx, item, is_table && !stmts.is_empty());
    if is_table {
        ctx.checkpoint(&item.id, CheckpointState::Created, None, None);
        ctx.copy.update_table(&item.id, |p| p.state = CopyItemState::Pending);
    } else {
        ctx.checkpoint(&item.id, CheckpointState::Done, None, None);
        ctx.set_st(&item.id, St::Done).await;
    }
}

/// Re-run the target session prelude after a failed item (see `run_ddl_item`).
async fn restore_session(ctx: &Ctx, session: &dyn TargetSession) {
    if ctx.cancel.is_cancelled() {
        return;
    }
    for sql in &ctx.planned.plan.session_sql.target {
        if let Err(e) = exec_checked(session, ctx.family, sql, StmtContext::Session, &ctx.cancel).await {
            tracing::warn!("copy: the session settings could not be restored after a failed item ({sql}): {e}");
            return;
        }
    }
}

/// What a successful DDL item did to tables, for `left_behind`: a table CREATE (or DROP +
/// CREATE) ran; a truncate group emptied its member tables.
fn note_effects(ctx: &Ctx, item: &PlanItem, created_table: bool) {
    if created_table && matches!(item.action, PlanAction::Create | PlanAction::DropRecreate) {
        ctx.created_now.lock().unwrap().insert(item.id.clone());
        if item.action == PlanAction::DropRecreate {
            ctx.emptied_now.lock().unwrap().insert(item.id.clone());
        }
    }
    if item.kind == CopyObjectKind::TruncateGroup {
        let mut e = ctx.emptied_now.lock().unwrap();
        for t in ctx.planned.plan.items.iter().filter(|t| t.kind == CopyObjectKind::Table && t.depends_on.contains(&item.id)) {
            e.insert(t.id.clone());
        }
    }
}

/// Load one table (plus its post-data items) on a worker. Returns the table result.
async fn run_data_item<'a>(ctx: &'a Ctx, known: &'a HashSet<String>, item: &'a PlanItem, posts: Vec<&'a PlanItem>) -> (String, Option<CopyTableResult>) {
    let t0 = Instant::now();
    let Some(spec) = ctx.planned.tables.get(&item.id) else {
        // A data-phase item without data (TruncateGroup, ...): statements only.
        match ctx.write_session(ctx.next_session_no()).await {
            Ok(s) => run_ddl_item(ctx, &*s, known, item, &item.target_sql, false).await,
            Err(e) => ctx.set_st(&item.id, st_of(&e)).await,
        }
        return (item.id.clone(), None);
    };
    let n = ctx.next_session_no();
    let sql = split_item_sql(item, true);
    let mut res = CopyTableResult {
        item_id: item.id.clone(),
        target_name: item.target_name.clone(),
        state: CopyItemState::Loading,
        rows_read: 0,
        rows_written: 0,
        target_count: None,
        verify: VerifyOutcome::NotRun,
        checksum_source: None,
        checksum_target: None,
        unmappable_chars: 0,
        data_warnings: 0,
        warning_samples: Vec::new(),
        value_fixes: Vec::new(),
        problems: Vec::new(),
        duration_ms: 0,
        method: None,
        error: None,
    };
    // Rows the target keeps if the load does not finish (checkpointed for the resume probe).
    let mut cp_rows: Option<u64> = None;
    let outcome = load_table(ctx, item, spec, &sql, n, &mut res, &mut cp_rows).await;
    let mut skipped_rows = 0;
    match outcome {
        Ok(skipped) => skipped_rows = skipped,
        Err(e) => {
            let payload = error_payload(&e);
            if let Some(code) = fail_code(&e) {
                if !res.problems.iter().any(|p| p == code) {
                    res.problems.push(code.to_string());
                }
            }
            res.state = if matches!(e, CoreError::Cancelled) { CopyItemState::Cancelled } else { CopyItemState::Failed };
            res.error = if matches!(e, CoreError::Cancelled) { None } else { Some(payload) };
        }
    }
    if res.state != CopyItemState::Failed && res.state != CopyItemState::Cancelled {
        let (state, problems) = verify::table_verdict(&res, skipped_rows, ctx.req.options.on_unmappable);
        res.state = state;
        res.problems = problems;
        if res.state == CopyItemState::Loading || res.state == CopyItemState::Verifying {
            res.state = CopyItemState::Done;
        }
    }
    res.duration_ms = now_ms(t0);
    let st = match res.state {
        CopyItemState::Done => St::Done,
        CopyItemState::Cancelled => St::Cancelled,
        _ => St::Failed(res.error.clone()),
    };
    let cp = match &st {
        St::Done => CheckpointState::Done,
        _ => CheckpointState::Failed,
    };
    // Post-data of this table, on a fresh session (the load session may be broken).
    if st == St::Done && !posts.is_empty() {
        ctx.set_st(&item.id, St::Done).await;
        match ctx.write_session(ctx.next_session_no()).await {
            Ok(s) => {
                ctx.copy.update_table(&item.id, |p| p.state = CopyItemState::Indexing);
                for p in &posts {
                    if ctx.cancel.is_cancelled() {
                        ctx.set_st(&p.id, St::Cancelled).await;
                        continue;
                    }
                    run_ddl_item(ctx, &*s, known, p, &p.target_sql, false).await;
                }
            }
            Err(e) => {
                for p in &posts {
                    ctx.set_st(&p.id, st_of(&e)).await;
                }
            }
        }
    } else {
        ctx.set_st(&item.id, st.clone()).await;
        for p in &posts {
            ctx.set_st(&p.id, if ctx.cancel.is_cancelled() { St::Cancelled } else { St::DepFailed }).await;
        }
    }
    // Committed rows only (5.5): a resume compares the target's count with this figure to tell
    // this run's rows from an application's.
    let rows = if cp == CheckpointState::Done { Some(res.rows_written) } else { cp_rows };
    ctx.checkpoint(&item.id, cp, rows, res.error.as_ref());
    let final_state = res.state;
    let err = res.error.clone();
    let method = res.method;
    ctx.copy.update_table(&item.id, |p| {
        p.state = final_state;
        p.error = err;
        p.method = method;
    });
    (item.id.clone(), Some(res))
}

/// After a failed or cancelled load: the rows the writer **committed** (whole MySQL chunks)
/// become the table's `rows_written`, with the warnings it read, so `left_behind.partial_rows`
/// never reports rows that were only sent. Without a report nothing was committed.
fn take_committed(tgt: &dyn TargetSession, res: &mut CopyTableResult) {
    let Some(p) = tgt.take_failed_outcome() else { return };
    res.rows_written = p.rows;
    res.data_warnings = res.data_warnings.max(p.warnings);
    if res.warning_samples.is_empty() {
        res.warning_samples = p.samples.iter().take(10).map(|s| redact_values(s)).collect();
    }
    if res.value_fixes.is_empty() {
        res.value_fixes = p.value_fixes;
    }
    res.unmappable_chars = res.unmappable_chars.max(p.unmappable);
    res.method = res.method.or(p.method);
}

/// The load of one table: DDL (data-phase items), pre-load statements, source stream ->
/// writer, post-load statements, count / checksum verification. `Ok(skipped rows)`.
async fn load_table(ctx: &Ctx, item: &PlanItem, spec: &TableDataSpec, sql: &ItemSql, n: u32, res: &mut CopyTableResult, cp_rows: &mut Option<u64>) -> Result<u64> {
    let cancel = &ctx.cancel;
    let tgt = ctx.write_session(n).await?;
    if item.phase == CopyPhase::Data {
        for s in &sql.ddl {
            exec_checked(&*tgt, ctx.family, s, StmtContext::Item { kind: item.kind, action: item.action }, cancel).await?;
        }
        note_effects(ctx, item, !sql.ddl.is_empty());
    }
    let before = if item.action == PlanAction::Append && ctx.req.options.verify != VerifyMode::None {
        Some(raced(&*tgt, cancel, tgt.count_rows(&spec.target)).await?)
    } else {
        None
    };
    let checksum_on = ctx.req.options.verify == VerifyMode::Checksum && !ctx.test_double && !ctx.no_checksum.contains(&item.id);
    // An append compares only the rows it added: the checksum is a wrapping sum, so the
    // target's sum before the load is subtracted from the sum after it.
    let before_sum = if checksum_on && item.action == PlanAction::Append {
        match target_checksum(ctx, spec).await {
            Err(CoreError::Cancelled) => return Err(CoreError::Cancelled),
            r => Some(r),
        }
    } else {
        None
    };
    if !ctx.checkpoint(&item.id, CheckpointState::Loading, Some(0), None) {
        return Err(copy_fail(CODE_CHECKPOINT_FAILED, "the run's checkpoint could not be stored; the table was not loaded"));
    }
    ctx.copy.update_table(&item.id, |p| p.state = CopyItemState::Loading);
    if !sql.pre_load.is_empty() && matches!(ctx.family, CopyFamily::Postgres | CopyFamily::Sqlite) {
        // Atomic with the load: the writer continues this transaction.
        exec_checked(&*tgt, ctx.family, "BEGIN", StmtContext::Session, cancel).await?;
    }
    // Pre-load statements (MySQL TRUNCATE) run while foreign_key_checks is still 0: InnoDB
    // refuses to truncate a table that a foreign key references while the checks are on.
    for s in &sql.pre_load {
        exec_checked(&*tgt, ctx.family, s, StmtContext::Item { kind: item.kind, action: item.action }, cancel).await?;
        if ctx.family == CopyFamily::Mysql && matches!(guard::leading_words(s, 1).first().map(String::as_str), Some("TRUNCATE" | "DELETE")) {
            // MySQL commits it on its own: the table stays emptied if the load fails.
            ctx.emptied_now.lock().unwrap().insert(item.id.clone());
        }
    }
    if spec.fk_checks && ctx.family == CopyFamily::Mysql {
        exec_checked(&*tgt, ctx.family, "SET SESSION foreign_key_checks = 1", StmtContext::Session, cancel).await?;
    }

    // Source stream.
    let check = crate::sqlguard::check_for(&spec.source_sql, crate::sqlguard::Dialect::from_driver(&ctx.src_conn.driver));
    if !check.ok {
        return Err(CoreError::Internal(format!("copy: the generated source query of {} is not a single read-only SELECT", item.id)));
    }
    let output = if spec.raw_csv { StreamOutput::RawCsv(CsvFormat { delimiter: b',', header: false, null_as: String::new(), crlf: false }) } else { StreamOutput::Arrow };
    let query = Query { sql: check.normalized.clone(), output, max_batch_bytes: Some((crate::BATCH_BYTES / ctx.concurrency.max(1) as usize).max(1 << 20)) };
    let table_cancel = cancel.child_token();
    // The shared MySQL snapshot session while it is intact, else a session of this table.
    let mut shared_guard = match &ctx.mysql_shared_source {
        Some(s) => Some(s.conn.lock().await),
        None => None,
    };
    if shared_guard.as_ref().is_some_and(|g| g.is_none()) {
        shared_guard = None;
        ctx.snapshot_lost("the snapshot session was given up after a table that did not finish; the tables after it are read at their own time".into());
    }
    let own_source;
    let src: &LiveConnection = match shared_guard.as_ref().and_then(|g| g.as_ref()) {
        Some(l) => l,
        None => {
            own_source = ctx.source_session(n).await?;
            &own_source
        }
    };
    let checksum = if checksum_on {
        match ctx.family {
            CopyFamily::Postgres => Some(ChecksumMode::RawCsv),
            // The MySQL writer hashes the source itself (`LoadOutcome::source_checksum`), so the
            // reader does not build a second encoder.
            CopyFamily::Mysql => None,
            CopyFamily::Sqlite => Some(ChecksumMode::Canonical),
        }
    } else {
        None
    };
    let sent = std::sync::atomic::AtomicU64::new(0);
    let tgt_ref: &dyn TargetSession = &*tgt;
    // For the hot-path (once-per-second) checkpoint inside `on_batch`: extract owned copies of
    // what `spawn_blocking` needs so the async worker is never blocked by rusqlite (items#13).
    // JoinHandles are collected and drained before the final `loaded` write so a slow
    // spawn_blocking (e.g. hit by rusqlite busy_timeout) cannot overwrite `loaded` with `loading`.
    let cp_path = ctx.store.as_ref().map(|s| s.path().to_path_buf());
    let cp_run_id = ctx.run_id.clone();
    let cp_item_id_bg = item.id.clone();
    let cp_lost = Arc::clone(&ctx.checkpoint_lost);
    let cp_handles: std::sync::Mutex<Vec<tokio::task::JoinHandle<()>>> = Default::default();
    let streamed: Result<(Result<pipeline::ReadOutcome>, Result<LoadOutcome>)> = async {
        let stream = crate::export::with_driver_cancel(cancel, DRIVER_GRACE, src.stream(&query, &ctx.session, table_cancel.clone())).await?;
        let (feed, input) = pipeline::channel(spec.raw_csv);
        let copy = ctx.copy.clone();
        let job = ctx.job.clone();
        let item_id = item.id.clone();
        let mut last_cp = Instant::now();
        let reader = pipeline::feed(
            stream,
            feed,
            FeedOpts { spec, budget: &ctx.budget, checksum, cancel: table_cancel.clone(), fail_on_replaced: ctx.req.options.on_unmappable == UnmappablePolicy::Fail },
            |rows, bytes| {
                copy.add_rows(&item_id, rows, rows, bytes);
                let total = sent.fetch_add(rows, Ordering::Relaxed) + rows;
                // What the target keeps so far (5.5): the writer's committed rows; rows sent
                // only for a non-transactional (MyISAM) table, where each one is final.
                // The write is offloaded to `spawn_blocking` so rusqlite's busy_timeout (up to
                // 5 s) never stalls the async worker, the writer, or cancel detection (items#13).
                if last_cp.elapsed() >= Duration::from_secs(1) {
                    last_cp = Instant::now();
                    let rows_now = tgt_ref.committed_rows().unwrap_or(total);
                    if let Some(ref path) = cp_path {
                        let path = path.clone();
                        let run_id = cp_run_id.clone();
                        let iid = cp_item_id_bg.clone();
                        let lost = Arc::clone(&cp_lost);
                        let handle = tokio::task::spawn_blocking(move || match CheckpointStore::open(&path) {
                            Ok(s) => {
                                if let Err(e) = s.set_item(&run_id, &iid, CheckpointState::Loading, false, Some(rows_now), None) {
                                    tracing::warn!(run_id = %run_id, item_id = %iid, "copy checkpoint: {e}");
                                    lost.store(true, Ordering::Relaxed);
                                }
                            }
                            Err(e) => {
                                tracing::warn!(run_id = %run_id, item_id = %iid, "copy checkpoint: cannot open store: {e}");
                                lost.store(true, Ordering::Relaxed);
                            }
                        });
                        cp_handles.lock().unwrap().push(handle);
                    }
                }
                job.progress.add_rows(rows);
                job.progress.bytes_in.fetch_add(bytes, Ordering::Relaxed);
                job.progress.bytes_out.fetch_add(bytes, Ordering::Relaxed);
                job.progress.batches.fetch_add(1, Ordering::Relaxed);
            },
        );
        let reader = async {
            let r = reader.await;
            if r.is_err() {
                // Tell the writer the input ended abnormally before the channel closes.
                table_cancel.cancel();
            }
            r
        };
        let writer = tgt_ref.load(spec, input, table_cancel.clone());
        let both = async { tokio::join!(reader, writer) };
        tokio::pin!(both);
        let r = tokio::select! {
            biased;
            r = &mut both => r,
            _ = cancel.cancelled() => {
                table_cancel.cancel();
                match tokio::time::timeout(KILL_AFTER, &mut both).await {
                    Ok(r) => r,
                    Err(_) => {
                        tgt_ref.cancel_running().await;
                        let _ = tokio::time::timeout(CANCEL_WAIT, &mut both).await;
                        return Err(CoreError::Cancelled);
                    }
                }
            }
        };
        Ok(r)
    }
    .await;
    let clean = matches!(&streamed, Ok((Ok(_), Ok(_))));
    if ctx.pg_snapshot.is_some() && shared_guard.is_none() {
        let _ = cancellable(cancel, super::facts::source_exec(src, "COMMIT")).await;
    }
    // A stream that did not end cleanly had its query killed, and the driver re-opens such a
    // connection without the snapshot or the session settings: give the shared session up.
    if let (Some(g), Some(shared)) = (shared_guard.as_mut(), &ctx.mysql_shared_source) {
        let same = g.as_ref().map(|l| l.session_id()) == Some(shared.session_id.clone());
        if !clean || !same {
            drop(g.take());
        }
    }
    drop(shared_guard);
    // After a failure: only what the writer committed stays in the target (left_behind) and is
    // checkpointed.
    let record = |res: &mut CopyTableResult, cp_rows: &mut Option<u64>| {
        // A chunk transaction still open when the load stopped: the server may be rolling it
        // back (5.4). Only a load that did not finish cleanly reaches here.
        if tgt.server_rollback_pending() {
            ctx.mysql_rollback_pending.store(true, Ordering::Relaxed);
        }
        let committed = tgt.committed_rows();
        take_committed(&*tgt, res);
        *cp_rows = Some(match committed {
            Some(_) => res.rows_written,
            None => sent.load(Ordering::Relaxed).max(res.rows_written),
        });
    };
    let cancel_or = |e: CoreError| if cancel.is_cancelled() && !matches!(e, CoreError::Cancelled) { CoreError::Cancelled } else { e };
    let (read, written) = match streamed {
        Ok(p) => p,
        Err(e) => {
            record(res, cp_rows);
            return Err(cancel_or(e));
        }
    };
    let read = match read {
        Ok(r) => r,
        Err(e) => {
            record(res, cp_rows);
            return Err(cancel_or(e));
        }
    };
    let out = match written {
        Ok(o) => o,
        Err(e) => {
            record(res, cp_rows);
            res.rows_read = read.rows;
            return Err(cancel_or(e));
        }
    };
    res.rows_read = read.rows;
    res.rows_written = out.rows;
    res.method = out.method;
    res.data_warnings = out.warnings;
    res.warning_samples = out.samples.iter().take(10).map(|s| redact_values(s)).collect();
    res.value_fixes = out.value_fixes.clone();
    res.unmappable_chars = out.unmappable + read.replaced;
    *cp_rows = Some(out.rows);
    ctx.emptied_now.lock().unwrap().remove(&item.id);
    // Drain any in-flight hot-path `Loading` spawn_blocking writes before recording `Loaded`,
    // so a slow spawn_blocking (e.g. rusqlite busy_timeout) cannot overwrite `loaded` with
    // `loading` after this point (items#13).
    let drained: Vec<_> = cp_handles.lock().unwrap().drain(..).collect();
    for h in drained {
        let _ = h.await;
    }
    ctx.checkpoint(&item.id, CheckpointState::Loaded, Some(out.rows), None);
    ctx.copy.update_table(&item.id, |p| {
        p.rows_read = read.rows;
        p.rows_written = out.rows;
        p.method = out.method;
    });
    if out.rows != read.rows && out.method.is_some() {
        tracing::debug!(item = %item.id, read = read.rows, written = out.rows, "copy: writer and reader counts differ");
    }
    for s in &sql.post_load {
        exec_checked(&*tgt, ctx.family, s, StmtContext::Item { kind: item.kind, action: item.action }, cancel).await?;
    }
    if spec.fk_checks && ctx.family == CopyFamily::Mysql {
        exec_checked(&*tgt, ctx.family, "SET SESSION foreign_key_checks = 0", StmtContext::Session, cancel).await?;
    }

    // Verification (5.6).
    if ctx.req.options.verify != VerifyMode::None {
        ctx.copy.update_table(&item.id, |p| p.state = CopyItemState::Verifying);
        let after = raced(&*tgt, cancel, tgt.count_rows(&spec.target)).await?;
        let count = match before {
            Some(b) => after.saturating_sub(b),
            None => after,
        };
        res.target_count = Some(count);
        res.verify = verify::table_outcome(read.rows, Some(count), out.warnings + out.skipped, res.unmappable_chars);
        if checksum_on {
            // MySQL: the writer hashed the source as it encoded; others: the reader did.
            let src_checksum = match ctx.family {
                CopyFamily::Mysql => out.source_checksum,
                _ => read.checksum,
            };
            if let Some(src_sum) = src_checksum {
                res.checksum_source = Some(src_sum.to_hex());
                let after = target_checksum(ctx, spec).await;
                let target = match (after, before_sum) {
                    (Ok(t), None) => Ok(t),
                    (Ok(t), Some(Ok(b))) => Ok(t.minus(b)),
                    (Err(e), _) | (_, Some(Err(e))) => Err(e),
                };
                match target {
                    Ok(t) => {
                        res.checksum_target = Some(t.to_hex());
                        if t != src_sum && res.verify == VerifyOutcome::Match {
                            res.verify = VerifyOutcome::Mismatch;
                        }
                    }
                    Err(e) => {
                        tracing::warn!(item = %item.id, "copy: checksum read-back failed: {e}");
                        res.verify = VerifyOutcome::Error;
                    }
                }
            }
        }
    } else {
        res.verify = verify::table_outcome(read.rows, None, out.warnings + out.skipped, res.unmappable_chars);
    }
    res.state = CopyItemState::Done;
    Ok(out.skipped)
}

/// Read the target table back through `verify_sql` on a read session and hash it like the
/// writer side (5.6).
async fn target_checksum(ctx: &Ctx, spec: &TableDataSpec) -> Result<TableChecksum> {
    let app = format!("{}{}-v", crate::APP_NAME_PREFIX, ctx.job.id);
    let mut conn = endpoint_connection(&ctx.tgt_conn, &ctx.req.target);
    if ctx.family == CopyFamily::Mysql {
        conn.database = None;
    }
    let live = cancellable(&ctx.cancel, ctx.db.open_connection(&conn, ctx.req.target.password.clone(), &app)).await?;
    let settings: Vec<String> = match ctx.family {
        CopyFamily::Postgres => super::target::pg::source_session_sql(false),
        CopyFamily::Mysql => vec![format!("SET NAMES {}", super::data::mysql_encode::connection_charset(&ctx.target_facts)), "SET time_zone = '+00:00'".to_string()],
        CopyFamily::Sqlite => Vec::new(),
    };
    for s in &settings {
        cancellable(&ctx.cancel, super::facts::source_exec(&live, s)).await?;
    }
    let raw = ctx.family == CopyFamily::Postgres;
    let output = if raw { StreamOutput::RawCsv(CsvFormat { delimiter: b',', header: false, null_as: String::new(), crlf: false }) } else { StreamOutput::Arrow };
    let check = crate::sqlguard::check_for(&spec.verify_sql, crate::sqlguard::Dialect::from_driver(&ctx.tgt_conn.driver));
    if !check.ok {
        return Err(CoreError::Internal("copy: the verify query is not a single read-only SELECT".into()));
    }
    // Split the batch budget by concurrency like the load reader (4.1): several tables can be in
    // read-back at once (building plus queued), so a fixed BATCH_BYTES/2 each overshoots the budget.
    let q = Query { sql: check.normalized, output, max_batch_bytes: Some((crate::BATCH_BYTES / ctx.concurrency.max(1) as usize).max(1 << 20)) };
    let mut stream = crate::export::with_driver_cancel(&ctx.cancel, DRIVER_GRACE, live.stream(&q, &SessionOpts::default(), ctx.cancel.clone())).await?;
    let mut sum = TableChecksum::default();
    let mut raw_hasher = pipeline::CsvRecordHasher::default();
    loop {
        let next = tokio::select! {
            biased;
            _ = ctx.cancel.cancelled() => return Err(CoreError::Cancelled),
            n = stream.stream.next() => n,
        };
        let Some(b) = next else { break };
        match b? {
            crate::source::Batch::RawCsv { data, .. } => raw_hasher.push(&data),
            crate::source::Batch::Arrow(b) => super::data::cells::checksum_batch(&b, &mut sum)?,
            crate::source::Batch::RawCsvHeader(_) => {}
        }
    }
    Ok(if raw { raw_hasher.finish() } else { sum })
}

/// Assemble the result from the item statuses.
async fn build_result(ctx: &Ctx, mut tables: BTreeMap<String, CopyTableResult>, item_problems: HashMap<String, Vec<String>>) -> CopyResult {
    let plan = &ctx.planned.plan;
    let st = ctx.st.lock().await.clone();
    let mut objects = Vec::new();
    let mut left_behind = Vec::new();
    let mut extra_problems = 0u32;
    // Tables that never got their turn (cancelled while waiting, skipped for a failed
    // dependency): a result row each.
    for item in plan.items.iter().filter(|i| ctx.planned.tables.contains_key(&i.id) && loads(i.action)) {
        if !tables.contains_key(&item.id) {
            let s = st.get(&item.id).cloned().unwrap_or(St::Cancelled);
            let mut r = skipped_result(item, Vec::new());
            r.state = s.item_state(item.action);
            if let St::Failed(p) = &s {
                r.error = p.clone();
            }
            tables.insert(item.id.clone(), r);
        }
    }
    for item in &plan.items {
        let is_table = tables.contains_key(&item.id);
        let s = st.get(&item.id).cloned().unwrap_or(if ctx.cancel.is_cancelled() { St::Cancelled } else { St::NotRun });
        if !is_table {
            let error = match &s {
                St::Failed(p) => p.clone(),
                _ => None,
            };
            if item_problems.contains_key(&item.id) {
                extra_problems += 1;
            }
            if executes(item.action) || item.action == PlanAction::ResumeDone {
                objects.push(CopyItemResult { item_id: item.id.clone(), state: s.item_state(item.action), error });
            }
        }
        if !s.ok() && executes(item.action) {
            match item.kind {
                CopyObjectKind::SequenceValue | CopyObjectKind::AutoIncrement => {
                    left_behind.push(LeftBehind { item_id: item.id.clone(), kind: LeftBehindKind::SequenceNotSet, rows: None })
                }
                CopyObjectKind::Index | CopyObjectKind::Constraint | CopyObjectKind::ForeignKey | CopyObjectKind::Trigger | CopyObjectKind::Policy => {
                    left_behind.push(LeftBehind { item_id: item.id.clone(), kind: LeftBehindKind::MissingPostData, rows: None })
                }
                _ => {}
            }
        }
    }
    // What unfinished tables leave in the target, from what this job actually did (and, for
    // `created_table`, what an earlier job of the run created): never from the plan's intent.
    {
        let created_now = ctx.created_now.lock().unwrap();
        let emptied_now = ctx.emptied_now.lock().unwrap();
        for item in plan.items.iter().filter(|i| loads(i.action)) {
            let Some(t) = tables.get(&item.id) else { continue };
            let s = st.get(&item.id).cloned().unwrap_or(if ctx.cancel.is_cancelled() { St::Cancelled } else { St::NotRun });
            if s.ok() && t.state != CopyItemState::Cancelled {
                continue;
            }
            if created_now.contains(&item.id) || ctx.created_earlier.contains(&item.id) {
                left_behind.push(LeftBehind { item_id: item.id.clone(), kind: LeftBehindKind::CreatedTable, rows: None });
            }
            if emptied_now.contains(&item.id) && item.target_has_rows != Some(false) {
                left_behind.push(LeftBehind { item_id: item.id.clone(), kind: LeftBehindKind::EmptiedTable, rows: None });
            }
            // Rows that stay in the target of an unfinished table: `rows_written` is what was
            // committed (MySQL/SQLite whole chunks; for PG, the single COPY either committed the
            // whole table — so it failed later, in post-data, verification or a mismatch — or
            // rolled back and left `rows_written` at 0). Report it for every family.
            if t.rows_written > 0 {
                left_behind.push(LeftBehind { item_id: item.id.clone(), kind: LeftBehindKind::PartialRows, rows: Some(t.rows_written) });
            }
        }
    }
    let mut warnings = plan.warnings.clone();
    for i in ctx.run_issues.lock().unwrap().iter() {
        if !warnings.iter().any(|w| w.code == i.code && w.item_id == i.item_id) {
            warnings.push(i.clone());
        }
    }
    // Only when a chunk transaction was actually open at cancel (5.4), not on every cancelled
    // MySQL job: a cancel between chunks leaves nothing to roll back.
    if ctx.cancel.is_cancelled() && ctx.family == CopyFamily::Mysql && ctx.mysql_rollback_pending.load(Ordering::Relaxed) {
        warnings.push(issue(CODE_SERVER_ROLLBACK_PENDING, Severity::Info, "the server may still be rolling back the last open chunk"));
    }
    let tables: Vec<CopyTableResult> = plan.items.iter().filter_map(|i| tables.get(&i.id).cloned()).collect();
    let (problems, verified) = verify::summarize(&tables, ctx.req.options.verify, extra_problems);
    CopyResult {
        run_id: ctx.run_id.clone(),
        dry_run: false,
        existing: ctx.req.options.existing,
        skipped_existing: plan.items.iter().filter(|i| i.action == PlanAction::SkipExisting && (i.kind == CopyObjectKind::Table || i.has_data)).count() as u32,
        tables,
        objects,
        warnings,
        left_behind,
        verified,
        problems,
        // Resuming needs the checkpoints of every item: none without the store, and none once
        // a write failed (the stored state may be stale).
        resumable: ctx.store.is_some() && !ctx.checkpoint_lost.load(Ordering::Relaxed),
        duration_ms: 0,
    }
}

// ---------------------------------------------------------------------------------------------
// Dry run (5.1)
// ---------------------------------------------------------------------------------------------

/// `SELECT * FROM (<sql>) q WHERE 1=0`; MySQL 4.0 (no derived tables): `<sql> LIMIT 0`.
pub fn dry_run_sql(sql: &str, mysql_40: bool) -> String {
    let s = sql.trim().trim_end_matches(';').trim_end();
    if mysql_40 {
        format!("{s} LIMIT 0")
    } else {
        format!("SELECT * FROM ({s}) q WHERE 1=0")
    }
}

async fn dry_run_items(ctx: &Ctx, src_live: &LiveConnection) -> Result<CopyResult> {
    let plan = &ctx.planned.plan;
    let mysql_40 = super::facts::mysql_version(src_live).is_some_and(|v| v < (4, 1, 0));
    let mut objects = Vec::new();
    let mut tables = Vec::new();
    ctx.copy.set_phase(CopyPhase::Data);
    // Source SELECTs and the type mapping.
    for item in plan.items.iter().filter(|i| ctx.planned.tables.contains_key(&i.id) && loads(i.action)) {
        if ctx.cancel.is_cancelled() {
            break;
        }
        let spec = &ctx.planned.tables[&item.id];
        let sql = dry_run_sql(&spec.source_sql, mysql_40);
        let check = crate::sqlguard::check_for(&sql, crate::sqlguard::Dialect::from_driver(&ctx.src_conn.driver));
        let r: Result<()> = async {
            if !check.ok {
                return Err(CoreError::Internal(format!("copy: the generated source query of {} is not a single read-only SELECT", item.id)));
            }
            let q = Query::new(check.normalized.clone(), StreamOutput::Arrow);
            let mut s = crate::export::with_driver_cancel(&ctx.cancel, DRIVER_GRACE, src_live.stream(&q, &ctx.session, ctx.cancel.clone())).await?;
            while let Some(b) = s.stream.next().await {
                b?;
            }
            Ok(())
        }
        .await;
        let (state, error) = match r {
            Ok(()) => (CopyItemState::Planned, None),
            Err(e) => (if matches!(e, CoreError::Cancelled) { CopyItemState::Cancelled } else { CopyItemState::Failed }, Some(error_payload(&e))),
        };
        ctx.copy.update_table(&item.id, |p| {
            p.state = state;
            p.error = error.clone();
        });
        let mut t = skipped_result(item, Vec::new());
        t.state = state;
        t.error = error;
        tables.push(t);
    }
    // PG: DDL in one rolled-back transaction, a savepoint per item.
    if ctx.family == CopyFamily::Postgres && !ctx.cancel.is_cancelled() {
        ctx.copy.set_phase(CopyPhase::PreData);
        let s = ctx.write_session(0).await?;
        exec_checked(&*s, ctx.family, "BEGIN", StmtContext::DryRun, &ctx.cancel).await?;
        // Only DDL of objects this run creates from scratch: a child of a table that already
        // exists (drop_recreate is never dropped in a dry run; skip, truncate, append) would
        // run against the live, populated table — a CREATE INDEX scan, an FK validation, a
        // PARTITION OF — holding its locks until the final ROLLBACK.
        let existing_tables: HashSet<&str> = plan
            .items
            .iter()
            .filter(|t| matches!(t.kind, CopyObjectKind::Table | CopyObjectKind::Partition) && t.action != PlanAction::Create)
            .map(|t| t.id.as_str())
            .collect();
        let mut items: Vec<&PlanItem> = Vec::new();
        for i in plan.items.iter().filter(|i| matches!(i.phase, CopyPhase::PreData | CopyPhase::PostData) && matches!(i.action, PlanAction::Create | PlanAction::Run)) {
            if deps_of(i).iter().any(|d| existing_tables.contains(d.as_str())) {
                let mut x = issue("DRY_RUN_NOT_CHECKED", Severity::Info, "not tried in the dry run: it changes a table that already exists in the target");
                x.item_id = Some(i.id.clone());
                ctx.note(x);
                continue;
            }
            items.push(i);
        }
        items.sort_by_key(|i| (i.phase, i.order));
        for (n, item) in items.into_iter().enumerate() {
            let has_spec = ctx.planned.tables.contains_key(&item.id);
            let sql = split_item_sql(item, has_spec);
            let stmts: Vec<&String> = sql.ddl.iter().filter(|q| guard::check_statement(ctx.family, q, StmtContext::DryRun).is_ok()).collect();
            if stmts.is_empty() {
                continue;
            }
            let sp = format!("SAVEPOINT bulkdb_{n}");
            exec_checked(&*s, ctx.family, &sp, StmtContext::DryRun, &ctx.cancel).await?;
            let mut err = None;
            for q in stmts {
                if let Err(e) = exec_checked(&*s, ctx.family, q, StmtContext::DryRun, &ctx.cancel).await {
                    err = Some(e);
                    break;
                }
            }
            match err {
                None => {
                    exec_checked(&*s, ctx.family, &format!("RELEASE SAVEPOINT bulkdb_{n}"), StmtContext::DryRun, &ctx.cancel).await?;
                    if !has_spec {
                        objects.push(CopyItemResult { item_id: item.id.clone(), state: CopyItemState::Planned, error: None });
                    }
                }
                Some(e) => {
                    if matches!(e, CoreError::Cancelled) {
                        return Err(e);
                    }
                    exec_checked(&*s, ctx.family, &format!("ROLLBACK TO SAVEPOINT bulkdb_{n}"), StmtContext::DryRun, &ctx.cancel).await?;
                    if has_spec {
                        if let Some(t) = tables.iter_mut().find(|t| t.item_id == item.id) {
                            t.state = CopyItemState::Failed;
                            t.error = Some(error_payload(&e));
                        }
                    } else {
                        objects.push(CopyItemResult { item_id: item.id.clone(), state: CopyItemState::Failed, error: Some(error_payload(&e)) });
                    }
                }
            }
        }
        exec_checked(&*s, ctx.family, "ROLLBACK", StmtContext::DryRun, &ctx.cancel).await?;
    }
    for item in plan.items.iter().filter(|i| executes(i.action) && !ctx.planned.tables.contains_key(&i.id)) {
        if !objects.iter().any(|o: &CopyItemResult| o.item_id == item.id) {
            objects.push(CopyItemResult { item_id: item.id.clone(), state: CopyItemState::Planned, error: None });
        }
    }
    let (problems, _) = verify::summarize(&tables, VerifyMode::None, 0);
    Ok(CopyResult {
        run_id: ctx.run_id.clone(),
        dry_run: true,
        existing: ctx.req.options.existing,
        skipped_existing: plan.items.iter().filter(|i| i.action == PlanAction::SkipExisting && (i.kind == CopyObjectKind::Table || i.has_data)).count() as u32,
        tables,
        objects,
        warnings: plan.warnings.iter().chain(ctx.run_issues.lock().unwrap().iter()).cloned().collect(),
        left_behind: Vec::new(),
        verified: false,
        problems,
        resumable: false,
        duration_ms: 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(sql: &[&str], phase: CopyPhase) -> PlanItem {
        PlanItem {
            id: "table:\"t\"".into(),
            kind: CopyObjectKind::Table,
            parent: None,
            schema: None,
            name: "t".into(),
            target_schema: None,
            target_name: "t".into(),
            action: PlanAction::Truncate,
            phase,
            order: 0,
            exists_in_target: true,
            target_has_rows: None,
            target_est_rows: None,
            has_data: true,
            est_rows: None,
            est_bytes: None,
            source_ddl: None,
            target_sql: sql.iter().map(|s| s.to_string()).collect(),
            warnings: vec![],
            depends_on: vec![],
        }
    }

    #[test]
    fn item_sql_split_around_the_load_template() {
        let i = item(
            &[
                "SET search_path = \"app\", pg_catalog",
                "CREATE TABLE \"app\".\"t\" (id int)",
                "TRUNCATE ONLY \"app\".\"t\"",
                "COPY \"app\".\"t\" (\"id\") FROM STDIN (FORMAT csv)",
                "ANALYZE \"app\".\"t\"",
            ],
            CopyPhase::PreData,
        );
        let s = split_item_sql(&i, true);
        assert_eq!(s.ddl.len(), 2);
        assert_eq!(s.pre_load, vec!["TRUNCATE ONLY \"app\".\"t\"".to_string()]);
        assert_eq!(s.post_load, vec!["ANALYZE \"app\".\"t\"".to_string()]);
        let m = item(
            &[
                "CREATE TABLE `db`.`t` (id int) ENGINE=MyISAM",
                "ALTER TABLE `db`.`t` DISABLE KEYS",
                "LOAD DATA LOCAL INFILE '<stream>' INTO TABLE `db`.`t`",
                "ALTER TABLE `db`.`t` ENABLE KEYS",
            ],
            CopyPhase::PreData,
        );
        let s = split_item_sql(&m, true);
        assert_eq!(s.ddl.len(), 1);
        assert_eq!(s.pre_load.len(), 1);
        assert_eq!(s.post_load, vec!["ALTER TABLE `db`.`t` ENABLE KEYS".to_string()]);
        let q = item(&["DELETE FROM \"t\"", "ALTER TABLE `db`.`t` ENABLE KEYS"], CopyPhase::Data);
        let s = split_item_sql(&q, true);
        assert_eq!(s.pre_load, vec!["DELETE FROM \"t\"".to_string()]);
        assert_eq!(s.post_load.len(), 1);
        let s = split_item_sql(&q, false);
        assert_eq!(s.ddl.len(), 2);
    }

    fn planned(family: CopyFamily, items: Vec<PlanItem>, session: &[&str]) -> PlannedCopy {
        let mut p = super::super::testkit::empty_plan(family);
        p.session_sql.target = session.iter().map(|s| s.to_string()).collect();
        let tables = items
            .iter()
            .filter(|i| i.has_data && i.kind == CopyObjectKind::Table)
            .map(|i| {
                let spec = TableDataSpec {
                    item_id: i.id.clone(),
                    source_sql: String::new(),
                    verify_sql: String::new(),
                    raw_csv: family == CopyFamily::Postgres,
                    transcode: None,
                    target: TargetTable { schema: None, name: i.target_name.clone() },
                    columns: vec![],
                    est_rows: None,
                    first_timestamp_auto: false,
                    fk_checks: false,
                };
                (i.id.clone(), spec)
            })
            .collect();
        p.items = items;
        PlannedCopy { plan: p, tables }
    }

    fn obj(kind: CopyObjectKind, action: PlanAction, name: &str, sql: &[&str]) -> PlanItem {
        let mut i = item(sql, CopyPhase::PreData);
        i.id = format!("{kind:?}:{name}");
        i.kind = kind;
        i.action = action;
        i.name = name.into();
        i.target_name = name.into();
        i.has_data = false;
        i
    }

    /// Every statement shape the translators emit passes the executor's allow-lists (5.2) in the
    /// context the executor checks it, after the split of `translate::item_sql_parts`.
    #[test]
    fn allow_lists_accept_every_translator_shape() {
        use CopyObjectKind as K;
        use PlanAction as A;
        // SQLite: drops of drop_recreate items, truncate, inlined sqlite_sequence finalize.
        let mut t = item(&["DROP TABLE \"t\"", "CREATE TABLE t (id INTEGER PRIMARY KEY AUTOINCREMENT)", "INSERT INTO \"t\" (\"id\") VALUES <rows>"], CopyPhase::PreData);
        t.action = A::DropRecreate;
        let mut tr = item(&["DELETE FROM \"t2\"", "INSERT INTO \"t2\" (\"a\") VALUES <rows>"], CopyPhase::Data);
        tr.id = "table:\"t2\"".into();
        tr.target_name = "t2".into();
        let sqlite = planned(
            CopyFamily::Sqlite,
            vec![
                t,
                tr,
                obj(K::Index, A::DropRecreate, "i", &["DROP INDEX \"i\"", "CREATE INDEX i ON t(id)"]),
                obj(K::View, A::DropRecreate, "v", &["DROP VIEW \"v\"", "CREATE VIEW v AS SELECT id FROM t"]),
                obj(K::Trigger, A::DropRecreate, "tg", &["DROP TRIGGER \"tg\"", "CREATE TRIGGER tg AFTER INSERT ON t BEGIN SELECT 1; END"]),
                obj(K::AutoIncrement, A::Run, "it's", &["UPDATE sqlite_sequence SET seq = max(seq, <source value>) WHERE name = 'it''s'"]),
                obj(K::Analyze, A::Run, "t", &["ANALYZE \"t\""]),
            ],
            &[],
        );
        let stmts: Vec<String> = executed_statements(&sqlite).into_iter().map(|(s, _)| s).collect();
        for want in [
            "DROP TABLE",
            "DROP INDEX",
            "DROP VIEW",
            "DROP TRIGGER",
            "DELETE FROM \"t2\"",
            "UPDATE sqlite_sequence SET seq = max(seq, 42)",
            "INSERT INTO sqlite_sequence(name, seq) SELECT 'it''s', 42",
        ] {
            assert!(stmts.iter().any(|s| s.starts_with(want)), "{want} not executed: {stmts:#?}");
        }
        assert!(!stmts.iter().any(|s| s.ends_with("<rows>")), "load templates never run: {stmts:#?}");
        assert_eq!(refused_statements(&sqlite), Vec::<String>::new());

        // MySQL: MyISAM DISABLE / ENABLE KEYS, trigger items with their sql_mode, collation and
        // the prelude restore, the database item, drops, FK drops, AUTO_INCREMENT.
        let mut m = item(
            &[
                "DROP TABLE `db`.`t`",
                "CREATE TABLE `db`.`t` (`id` int NOT NULL) ENGINE=MyISAM DEFAULT CHARSET=utf8mb4",
                "ALTER TABLE `db`.`t` DISABLE KEYS",
                "LOAD DATA LOCAL INFILE '<stream>' INTO TABLE `db`.`t` CHARACTER SET utf8mb4 (`id`)",
            ],
            CopyPhase::PreData,
        );
        m.action = A::DropRecreate;
        let prelude = [
            "SET NAMES utf8mb4",
            "SET SESSION sql_mode = 'STRICT_ALL_TABLES,NO_ENGINE_SUBSTITUTION,NO_AUTO_VALUE_ON_ZERO'",
            "SET SESSION time_zone = '+00:00'",
            "SET SESSION foreign_key_checks = 0",
            "SET SESSION lock_wait_timeout = 5",
            "SET SESSION net_read_timeout = 600, net_write_timeout = 600, wait_timeout = 28800",
        ];
        let mysql = planned(
            CopyFamily::Mysql,
            vec![
                obj(K::Database, A::Create, "db", &["CREATE DATABASE `db` DEFAULT CHARACTER SET utf8mb4 COLLATE utf8mb4_0900_ai_ci"]),
                m,
                obj(K::Index, A::Create, "(enable keys)", &["ALTER TABLE `db`.`t` ENABLE KEYS"]),
                obj(K::Index, A::Create, "k", &["ALTER TABLE `db`.`t` ADD KEY `k` (`id`), ADD UNIQUE KEY `u` (`id`)"]),
                obj(
                    K::ForeignKey,
                    A::DropRecreate,
                    "fk",
                    &["ALTER TABLE `db`.`c` DROP FOREIGN KEY `fk`", "ALTER TABLE `db`.`c` ADD CONSTRAINT `fk` FOREIGN KEY (`p`) REFERENCES `db`.`t` (`id`)"],
                ),
                obj(K::View, A::DropRecreate, "v", &["DROP VIEW `db`.`v`", "CREATE ALGORITHM=UNDEFINED SQL SECURITY DEFINER VIEW `db`.`v` AS select 1 AS `a`"]),
                obj(
                    K::Trigger,
                    A::DropRecreate,
                    "tg",
                    &[
                        "DROP TRIGGER `db`.`tg`",
                        "SET SESSION sql_mode = 'STRICT_TRANS_TABLES'",
                        "SET SESSION collation_connection = 'utf8mb4_bin'",
                        "CREATE TRIGGER `db`.`tg` BEFORE INSERT ON `db`.`t` FOR EACH ROW SET NEW.id = NEW.id + 1",
                        "SET SESSION sql_mode = 'STRICT_ALL_TABLES,NO_ENGINE_SUBSTITUTION,NO_AUTO_VALUE_ON_ZERO'",
                        "SET NAMES utf8mb4",
                    ],
                ),
                obj(K::AutoIncrement, A::Run, "t", &["ALTER TABLE `db`.`t` AUTO_INCREMENT = <source value>"]),
                obj(K::Analyze, A::Run, "t", &["ANALYZE TABLE `db`.`t`"]),
            ],
            &prelude,
        );
        let split = split_item_sql(&mysql.plan.items[1], true);
        assert_eq!(split.ddl, vec!["DROP TABLE `db`.`t`".to_string(), "CREATE TABLE `db`.`t` (`id` int NOT NULL) ENGINE=MyISAM DEFAULT CHARSET=utf8mb4".to_string()]);
        assert_eq!(split.pre_load, vec!["ALTER TABLE `db`.`t` DISABLE KEYS".to_string()], "DISABLE KEYS runs on the load session");
        assert!(split.post_load.is_empty());
        let stmts: Vec<String> = executed_statements(&mysql).into_iter().map(|(s, _)| s).collect();
        assert!(stmts.iter().any(|s| s == "ALTER TABLE `db`.`t` AUTO_INCREMENT = 42"));
        assert_eq!(refused_statements(&mysql), Vec::<String>::new());
        // The trigger's session statements end with the prelude values the session assertion
        // expects before the next chunk.
        let mut expect = crate::copy::data::mysql_encode::SessionExpect::default();
        for s in prelude.iter().copied().chain(mysql.plan.items[6].target_sql.iter().map(String::as_str)) {
            if s.starts_with("SET") {
                expect.observe(s);
            }
        }
        expect.check(Some("+00:00"), Some("NO_AUTO_VALUE_ON_ZERO,STRICT_ALL_TABLES,NO_ENGINE_SUBSTITUTION"), Some("utf8mb4")).unwrap();
        assert!(expect.check(Some("+00:00"), Some("STRICT_TRANS_TABLES"), Some("utf8mb4")).is_err(), "a trigger's own sql_mode left behind is caught");

        // PostgreSQL: SET search_path / check_function_bodies per item, drops with RESTRICT
        // (policies too), truncate groups, setval in both forms, comments, grants, refresh.
        let mut p = item(
            &[
                "SET search_path = \"app\", pg_catalog",
                "DROP TABLE \"app\".\"t\" RESTRICT",
                "CREATE TABLE \"app\".\"t\" (id integer)",
                "COPY \"app\".\"t\" (\"id\") FROM STDIN (FORMAT csv, HEADER false, DELIMITER ',', NULL '', QUOTE '\"', ESCAPE '\"')",
            ],
            CopyPhase::PreData,
        );
        p.action = A::DropRecreate;
        let pg = planned(
            CopyFamily::Postgres,
            vec![
                p,
                obj(
                    K::Function,
                    A::DropRecreate,
                    "f",
                    &[
                        "SET search_path = \"app\", pg_catalog",
                        "SET check_function_bodies = off",
                        "DROP FUNCTION \"app\".\"f\"(integer) RESTRICT",
                        "CREATE OR REPLACE FUNCTION \"app\".\"f\"(integer) RETURNS integer LANGUAGE sql AS $$ select 1 $$",
                    ],
                ),
                obj(
                    K::Policy,
                    A::DropRecreate,
                    "pol",
                    &["SET search_path = \"app\", pg_catalog", "DROP POLICY \"pol\" ON \"app\".\"t\" RESTRICT", "CREATE POLICY pol ON t USING (true)"],
                ),
                obj(
                    K::Trigger,
                    A::DropRecreate,
                    "tg",
                    &[
                        "DROP TRIGGER \"tg\" ON \"app\".\"t\" RESTRICT",
                        "CREATE TRIGGER tg AFTER INSERT ON t FOR EACH ROW EXECUTE FUNCTION f()",
                        "ALTER TABLE \"app\".\"t\" DISABLE TRIGGER \"tg\"",
                    ],
                ),
                obj(K::TruncateGroup, A::Run, "g", &["TRUNCATE ONLY \"app\".\"a\", ONLY \"app\".\"b\""]),
                obj(K::SequenceValue, A::Run, "q", &["SELECT pg_catalog.setval('\"app\".\"q\"', <source value>, <is_called>)"]),
                obj(K::SequenceValue, A::Run, "q2", &["SELECT pg_catalog.setval('\"app\".\"q2\"', GREATEST(<source value>, <target value>), true)"]),
                obj(K::Comment, A::Run, "c", &["COMMENT ON TABLE \"app\".\"t\" IS '설명'"]),
                obj(K::Grant, A::Run, "gr", &["GRANT SELECT ON TABLE \"app\".\"t\" TO \"reader\""]),
                obj(K::Owner, A::Run, "o", &["ALTER TABLE \"app\".\"t\" OWNER TO \"app_owner\""]),
                obj(K::Refresh, A::Run, "mv", &["REFRESH MATERIALIZED VIEW \"app\".\"mv\""]),
                obj(K::Analyze, A::Run, "t", &["ANALYZE \"app\".\"t\""]),
            ],
            &[
                "SET statement_timeout = 0",
                "SET idle_in_transaction_session_timeout = 0",
                "SET lock_timeout = '5s'",
                "SET DateStyle = ISO, YMD",
                "SET check_function_bodies = off",
            ],
        );
        let split = split_item_sql(&pg.plan.items[0], true);
        assert_eq!(split.ddl.len(), 3, "{split:?}");
        assert!(split.pre_load.is_empty() && split.post_load.is_empty());
        assert_eq!(refused_statements(&pg), Vec::<String>::new());
        // A drop without RESTRICT is refused (the old DROP POLICY shape).
        let bad = planned(CopyFamily::Postgres, vec![obj(K::Policy, A::DropRecreate, "pol", &["DROP POLICY \"pol\" ON \"app\".\"t\""])], &[]);
        assert_eq!(refused_statements(&bad).len(), 1);
    }

    #[test]
    fn dry_run_and_lease_statements() {
        assert_eq!(dry_run_sql("SELECT a FROM t;", false), "SELECT * FROM (SELECT a FROM t) q WHERE 1=0");
        assert_eq!(dry_run_sql("SELECT a FROM t", true), "SELECT a FROM t LIMIT 0");
        let my = lease_sql(CopyFamily::Mysql, "shop").unwrap();
        assert!(my.starts_with("SELECT GET_LOCK('bulkdb-copy:") && my.contains("', 0)"));
        let name = my.split('\'').nth(1).unwrap();
        assert!(name.len() <= 64, "MySQL lock names are limited to 64 characters: {name}");
        assert_eq!(lease_sql(CopyFamily::Postgres, "x").unwrap(), "SELECT pg_try_advisory_lock(hashtext('bulkdb-copy')) AS locked");
        assert!(lease_sql(CopyFamily::Sqlite, "main").is_none());
        for f in [CopyFamily::Postgres, CopyFamily::Mysql] {
            guard::check_read(f, &lease_sql(f, "db").unwrap()).unwrap();
        }
    }

    #[tokio::test]
    async fn sqlite_lease_is_exclusive() {
        let dir = tempfile::tempdir().unwrap();
        let mut conn = Connection { driver: "sqlite".into(), ..Connection::default() };
        conn.fields.insert("path".into(), dir.path().join("t.db").to_string_lossy().into_owned());
        let opener = super::super::testkit::RecordingTarget::new(TargetFacts::new(CopyFamily::Sqlite, "sqlite", "3", "main"));
        let ep = CopyEndpoint { conn_id: "x".into(), database: "main".into(), schema: None, password: None };
        let a = take_lease(&opener, &conn, &ep, CopyFamily::Sqlite, "l").await.unwrap();
        assert!(a.is_some());
        let b = take_lease(&opener, &conn, &ep, CopyFamily::Sqlite, "l").await.unwrap();
        assert!(b.is_none(), "a second lease on the same file is refused");
        drop(a);
        assert!(take_lease(&opener, &conn, &ep, CopyFamily::Sqlite, "l").await.unwrap().is_some());
    }

    #[tokio::test]
    async fn raced_cancels_a_blocked_statement_within_two_seconds() {
        let t = super::super::testkit::RecordingTarget::new(TargetFacts::new(CopyFamily::Postgres, "postgres", "16", "db"));
        let cancel = CancellationToken::new();
        let c2 = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            c2.cancel();
        });
        let t0 = Instant::now();
        let r: Result<()> = raced(&t, &cancel, async {
            tokio::time::sleep(Duration::from_secs(30)).await;
            Ok(())
        })
        .await;
        assert!(matches!(r, Err(CoreError::Cancelled)));
        assert!(t0.elapsed() < Duration::from_secs(2), "{:?}", t0.elapsed());
    }
}
