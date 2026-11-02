//! Process state and entry points behind the `BulkDb::copy_*` facade methods (section 6.1).
//! Owner: copy-data.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use tokio_util::sync::CancellationToken;

use super::checkpoint::{CheckpointStore, RunRow};
use super::exec::{self, CopyStart};
use super::preflight::{self, issue, FinalizeInput};
use super::target::{endpoint_connection, TargetOpener};
use super::types::*;
use super::CopyJob;
use crate::error::{CoreError, Result};
use crate::service::BulkDb;
use crate::types::{Connection, JobState, Severity};

/// Plans kept for `PLAN_CHANGED` details.
const RECENT_PLANS: usize = 16;

/// Why a run whose checkpoints are incomplete cannot be resumed.
pub const NOT_RESUMABLE: &str = "this run cannot be resumed: a checkpoint could not be stored, so what it already did is unknown; start a new copy";

/// `BulkDb.copy`: plan-token registry, recent plans for `PLAN_CHANGED`; active jobs and their
/// lease keys live on the jobs themselves (`CopyJob::keys`).
#[derive(Default)]
pub struct CopyState {
    /// `BulkDbOptions.copy_target_opener`; `None` = the real opener (`copy::target`).
    opener: Option<Arc<dyn TargetOpener>>,
    /// Cancellation tokens of running `copy_plan` calls by plan token.
    pub plan_tokens: Mutex<HashMap<String, CancellationToken>>,
    recent: Mutex<VecDeque<CopyPlan>>,
}

impl CopyState {
    pub fn new(opener: Option<Arc<dyn TargetOpener>>) -> CopyState {
        CopyState { opener, ..CopyState::default() }
    }

    /// The target opener: the test double when one was configured, else the real one.
    pub fn opener(&self, db: &BulkDb) -> Arc<dyn TargetOpener> {
        self.opener.clone().unwrap_or_else(|| Arc::new(super::target::DriverOpener::new(db.copy_weak())))
    }

    /// Whether a test double replaces the real targets (checksums are not supported there).
    pub fn has_test_opener(&self) -> bool {
        self.opener.is_some()
    }

    /// Remember a plan (the last 16 of the process).
    pub fn remember_plan(&self, plan: &CopyPlan) {
        let mut r = self.recent.lock().unwrap();
        r.retain(|p| p.plan_id != plan.plan_id);
        r.push_back(plan.clone());
        while r.len() > RECENT_PLANS {
            r.pop_front();
        }
    }

    pub fn recent_plan(&self, plan_id: &str) -> Option<CopyPlan> {
        self.recent.lock().unwrap().iter().find(|p| p.plan_id == plan_id).cloned()
    }
}

/// Removes a plan token when the plan ends.
struct TokenGuard<'a> {
    db: &'a BulkDb,
    token: Option<String>,
}

impl Drop for TokenGuard<'_> {
    fn drop(&mut self) {
        if let Some(t) = &self.token {
            self.db.copy.plan_tokens.lock().unwrap().remove(t);
        }
    }
}

fn family_of(src: &Connection, tgt: &Connection) -> std::result::Result<CopyFamily, Box<CopyIssue>> {
    match (CopyFamily::of_driver(&src.driver), CopyFamily::of_driver(&tgt.driver)) {
        (Some(a), Some(b)) if a == b => Ok(a),
        (Some(_), Some(_)) => Err(Box::new(issue(
            "FAMILY_MISMATCH",
            Severity::Danger,
            format!("the source ({}) and the target ({}) must be the same database family", src.driver, tgt.driver),
        ))),
        _ => Err(Box::new(issue("FAMILY_UNSUPPORTED", Severity::Danger, "copy supports postgres, mysql and sqlite connections"))),
    }
}

/// A plan with only blockers (a request refused before planning).
fn refused_plan(family: CopyFamily, req: &CopyRequest, src: &Connection, tgt: &Connection, blockers: Vec<CopyIssue>) -> CopyPlan {
    let mut p = super::testkit::empty_plan(family);
    p.source =
        CopyServerFacts { driver: src.driver.clone(), database: req.source.database.clone(), endpoint: super::target::endpoint_text(src), ..CopyServerFacts::default() };
    p.target =
        CopyServerFacts { driver: tgt.driver.clone(), database: req.target.database.clone(), endpoint: super::target::endpoint_text(tgt), ..CopyServerFacts::default() };
    p.blockers = blockers;
    let id = ServerIdentity::default();
    p.plan_id = preflight::plan_id(&p, &id, &id);
    p
}

/// In-use predicate of the checkpoint store for this process.
fn in_use(db: &BulkDb) -> impl Fn(&RunRow) -> bool + '_ {
    move |r: &RunRow| {
        if r.owner_instance == db.instance_id() {
            db.jobs.get(&r.last_job_id).is_some_and(|j| !j.state.lock().unwrap().is_terminal())
        } else {
            crate::instances::is_alive(db.config.dir(), &r.owner_instance)
        }
    }
}

fn store(db: &BulkDb) -> Result<CheckpointStore> {
    CheckpointStore::open(&db.config.history_path())
}

/// Plan a copy without creating a job (sessions `bulkdb-copyplan`).
pub async fn copy_plan(db: &BulkDb, req: &CopyRequest, plan_token: Option<String>) -> Result<CopyPlan> {
    let cancel = CancellationToken::new();
    if let Some(t) = &plan_token {
        db.copy.plan_tokens.lock().unwrap().insert(t.clone(), cancel.clone());
    }
    let _guard = TokenGuard { db, token: plan_token };
    let t0 = std::time::Instant::now();
    let src_conn = db.conn_get(&req.source.conn_id)?;
    let tgt_conn = db.conn_get(&req.target.conn_id)?;
    let family = match family_of(&src_conn, &tgt_conn) {
        Ok(f) => f,
        Err(b) => {
            let fam = CopyFamily::of_driver(&src_conn.driver).unwrap_or(CopyFamily::Sqlite);
            return Ok(refused_plan(fam, req, &src_conn, &tgt_conn, vec![*b]));
        }
    };
    db.remember_secret(&src_conn, req.source.password.clone());
    db.remember_secret(&tgt_conn, req.target.password.clone());
    let app = "bulkdb-copyplan";
    let src_live = crate::export::cancellable(&cancel, db.open_connection(&endpoint_connection(&src_conn, &req.source), req.source.password.clone(), app)).await?;
    let server = crate::export::cancellable(&cancel, src_live.probe()).await?;
    // Read only, and the catalog deparse settings (PG DateStyle & co., MySQL time_zone).
    crate::export::cancellable(&cancel, super::facts::apply_meta_session(&src_live, family, &server.version)).await?;
    let src_facts = crate::export::cancellable(&cancel, super::facts::read_source_facts(&src_live, &src_conn, &req.source.database)).await?;
    let opener = db.copy.opener(db);
    let tgt = crate::export::cancellable(&cancel, opener.open(&tgt_conn, &req.target, app)).await?;
    let mut tf = tgt.facts().clone();
    tf.identity = super::target::identity_or_default(&tf);
    let source_key = exec::endpoint_key(&src_facts.identity, &req.source.database);
    let target_key = exec::endpoint_key(&tf.identity, &req.target.database);
    let mut extra = exec::registry_issues(db, None, &source_key, &target_key);
    // Runs of other processes (checkpoint store) writing the same target.
    if let Ok(s) = store(db) {
        let pred = in_use(db);
        if let Ok(runs) = s.list(None, 200) {
            for r in runs.iter().filter(|r| r.target_identity == tf.identity.key && r.target_database.eq_ignore_ascii_case(&req.target.database)) {
                if super::checkpoint::is_active_state(r.state) && pred(r) && !extra.iter().any(|i| i.code == "TARGET_IN_USE") {
                    extra.push(issue("TARGET_IN_USE", Severity::Danger, format!("copy run {} ({}) is writing this target database", r.run_id, r.label)));
                }
            }
        }
    }
    let recommended = {
        let mut r = crate::preflight::recommended_session(&server);
        r.statement_timeout_ms = None;
        r
    };
    let session = crate::export::merge_session(&req.options.source_session, &src_conn.session, &recommended);
    if family == CopyFamily::Postgres {
        if let (Some(wm), Some(ram)) = (&session.work_mem, server.ram_bytes) {
            extra.extend(crate::preflight::check_work_mem(wm, ram).map(preflight::from_preflight));
        }
    }
    let mut planned = {
        let src_meta = super::schema::source_meta(&src_live)?;
        #[cfg(feature = "mysql")]
        let mysql_facts = crate::source::mysql_raw(src_live.db()).map(|r| r.facts().clone());
        #[cfg(not(feature = "mysql"))]
        let mysql_facts: Option<()> = None;
        let input = super::plan::PlanInput {
            req,
            source: &*src_meta,
            source_conn: &src_conn,
            source_server: &server,
            mysql_source: mysql_facts.as_ref(),
            target: &*tgt,
            target_conn: &tgt_conn,
            resume: None,
            cancel: &cancel,
        };
        crate::export::cancellable(&cancel, super::plan::build_plan(input)).await?
    };
    // The export preflight's concurrent-session check, for the tables this copy reads (5.3).
    extra.extend(crate::export::cancellable(&cancel, async { Ok(preflight::source_session_issues(src_live.monitor(), &planned.plan).await) }).await?);
    extra.extend(preflight::unknown_item_issues(&planned.plan, &req.options));
    extra.extend(preflight::mysql_enum_issues(&src_live, &planned, &req.source.database, req.options.mysql.enum_error, &cancel).await?);
    let grants = if family == CopyFamily::Mysql { tgt.query_meta("SHOW GRANTS").await.ok() } else { None };
    preflight::fill_session_sql(&mut planned.plan, &tf, Some(&src_facts), req.options.target_lock_timeout_s.unwrap_or(5), req.options.mysql.zero_dates);
    let mut fi = FinalizeInput::new(&src_conn, &tgt_conn, &src_facts.identity, &tf);
    fi.source = Some(&src_facts);
    fi.extra = extra;
    fi.grants = grants.as_ref();
    preflight::finalize(&mut planned, &fi)?;
    if db.copy.has_test_opener() && req.options.verify == VerifyMode::Checksum {
        planned.plan.blockers.push(issue("CHECKSUM_UNSUPPORTED", Severity::Danger, "checksum verification is not supported on this target"));
    }
    if cancel.is_cancelled() {
        return Err(CoreError::Cancelled);
    }
    planned.plan.elapsed_ms = t0.elapsed().as_millis() as u64;
    db.copy.remember_plan(&planned.plan);
    Ok(planned.plan)
}

/// Cancel a running `copy_plan` (it ends `CANCELLED` within ~2 s). Unknown tokens (already
/// finished plans) are ignored.
pub fn copy_plan_cancel(db: &BulkDb, plan_token: &str) -> Result<()> {
    if let Some(token) = db.copy.plan_tokens.lock().unwrap().get(plan_token) {
        token.cancel();
    }
    Ok(())
}

fn label_for(req: &CopyRequest, src: &Connection, tgt: &Connection) -> String {
    match req.name.as_deref().map(str::trim).filter(|n| !n.is_empty()) {
        Some(n) => n.to_string(),
        None => format!("{}.{} → {}.{}", src.name, req.source.database, tgt.name, req.target.database),
    }
}

fn job_info(run_id: &str, req: &CopyRequest, tgt: &Connection) -> CopyJobInfo {
    CopyJobInfo {
        run_id: run_id.to_string(),
        target_conn_id: tgt.id.clone(),
        target_conn_name: tgt.name.clone(),
        source_database: req.source.database.clone(),
        target_database: req.target.database.clone(),
        phase: CopyPhase::Plan,
        tables_total: 0,
        tables_done: 0,
        tables_failed: 0,
        dry_run: req.options.dry_run,
    }
}

/// Create the copy job (`JobRegistry::create_copy`) and spawn `exec::run_copy_job`; job id.
pub async fn copy_start(db: Arc<BulkDb>, req: CopyRequest, plan_id: String, acknowledged: Vec<String>) -> Result<String> {
    let src = db.conn_get(&req.source.conn_id)?;
    let tgt = db.conn_get(&req.target.conn_id)?;
    if req.source.database.trim().is_empty() || req.target.database.trim().is_empty() {
        return Err(CoreError::Invalid("copy: the source and target databases are required".into()));
    }
    let label = label_for(&req, &src, &tgt);
    db.jobs.prune(200);
    let req_for_info = req.clone();
    let tgt_for_info = tgt.clone();
    let mut copy_job: Option<Arc<CopyJob>> = None;
    let job = db.jobs.create_copy(&src.id, &src.name, &label, |id| {
        let c = Arc::new(CopyJob::new(job_info(id, &req_for_info, &tgt_for_info)));
        copy_job = Some(c.clone());
        c
    });
    let copy = copy_job.ok_or_else(|| CoreError::Internal("copy job state missing".into()))?;
    tracing::info!(job_id = %job.id, label = %label, dry_run = req.options.dry_run, "copy job created");
    let id = job.id.clone();
    tokio::spawn(exec::run_copy_job(db.clone(), job, copy, CopyStart::New { req: Box::new(req), plan_id, acknowledged }));
    Ok(id)
}

/// Resume a run (new job in the same run); job id.
pub async fn copy_resume(db: Arc<BulkDb>, req: CopyResumeRequest) -> Result<String> {
    let s = store(&db)?;
    let row = s.get(&req.run_id)?.ok_or_else(|| CoreError::NotFound(format!("copy run {}", req.run_id)))?;
    if row.dry_run {
        return Err(CoreError::Invalid("dry runs are never resumable".into()));
    }
    if row.result().is_some_and(|r| !r.resumable) {
        return Err(CoreError::Invalid(NOT_RESUMABLE.into()));
    }
    let stored = row.request()?;
    let src = db.conn_get(&row.source_conn_id)?;
    let tgt = db.conn_get(&row.target_conn_id)?;
    db.jobs.prune(200);
    let mut info_req = stored.clone();
    info_req.options.dry_run = false;
    let run_id = row.run_id.clone();
    let mut copy_job: Option<Arc<CopyJob>> = None;
    let job = db.jobs.create_copy(&src.id, &src.name, &row.label, |_| {
        let c = Arc::new(CopyJob::new(job_info(&run_id, &info_req, &tgt)));
        copy_job = Some(c.clone());
        c
    });
    let copy = copy_job.ok_or_else(|| CoreError::Internal("copy job state missing".into()))?;
    tracing::info!(job_id = %job.id, run_id = %row.run_id, "copy resume job created");
    let id = job.id.clone();
    tokio::spawn(exec::run_copy_job(db.clone(), job, copy, CopyStart::Resume { req }));
    Ok(id)
}

pub fn copy_status(db: &BulkDb, job_id: &str) -> Result<CopyStatus> {
    let job = db.jobs.get(job_id).ok_or_else(|| CoreError::NotFound(format!("job {job_id}")))?;
    let copy = job.copy.as_ref().ok_or_else(|| CoreError::Invalid(format!("job {job_id} is not a copy job")))?;
    let state = *job.state.lock().unwrap();
    Ok(CopyStatus { job_id: job_id.to_string(), run_id: copy.run_id(), state, plan: copy.plan(), progress: copy.progress(job_id), result: copy.result() })
}

fn run_info(db: &BulkDb, s: &CheckpointStore, r: &RunRow) -> CopyRunInfo {
    let pred = in_use(db);
    let in_use = super::checkpoint::is_active_state(r.state) && pred(r);
    // A run left active by a process that is gone was interrupted.
    let state = if super::checkpoint::is_active_state(r.state) && !in_use { JobState::Failed } else { r.state };
    let (total, done, failed) = s.table_counts(&r.run_id).unwrap_or((0, 0, 0));
    let (total, done, failed) = match r.result() {
        Some(res) if total == 0 => (
            res.tables.len() as u32,
            res.tables.iter().filter(|t| matches!(t.state, CopyItemState::Done | CopyItemState::Skipped | CopyItemState::Planned)).count() as u32,
            res.tables.iter().filter(|t| t.state == CopyItemState::Failed).count() as u32,
        ),
        _ => (total, done, failed),
    };
    CopyRunInfo {
        run_id: r.run_id.clone(),
        label: r.label.clone(),
        source_conn_id: r.source_conn_id.clone(),
        target_conn_id: r.target_conn_id.clone(),
        source_database: r.source_database.clone(),
        target_database: r.target_database.clone(),
        started_at: r.started_at.clone(),
        finished_at: r.finished_at.clone(),
        state,
        last_job_id: r.last_job_id.clone(),
        tables_total: total,
        tables_done: done,
        tables_failed: failed,
        // A result that says otherwise had a checkpoint write fail: the stored item states may
        // be stale, so the run is not resumable. A run that finished `done` is resumable only
        // when its stored result has problems (the result view offers the same, 5.1): a clean
        // done run has nothing left to do.
        resumable: !r.dry_run
            && !in_use
            && r.result().is_none_or(|res| res.resumable)
            && (matches!(state, JobState::Failed | JobState::Cancelled | JobState::Rejected)
                || (state == JobState::Done && r.result().is_some_and(|res| res.problems > 0))),
        dry_run: r.dry_run,
        in_use,
    }
}

/// Runs whose source or target is `conn_id` (all when `None`), newest first.
pub fn copy_runs(db: &BulkDb, conn_id: Option<&str>, limit: usize) -> Result<Vec<CopyRunInfo>> {
    let s = store(db)?;
    if let Err(e) = s.prune() {
        tracing::warn!("copy runs: cannot prune: {e}");
    }
    let rows = s.list(conn_id, limit.max(1))?;
    Ok(rows.iter().map(|r| run_info(db, &s, r)).collect())
}

pub fn copy_run(db: &BulkDb, run_id: &str) -> Result<CopyRunDetail> {
    let s = store(db)?;
    let r = s.get(run_id)?.ok_or_else(|| CoreError::NotFound(format!("copy run {run_id}")))?;
    let mut request = r.request()?;
    request.source.password = None;
    request.target.password = None;
    request.options.dry_run = false;
    request.options.confirm_production = None;
    Ok(CopyRunDetail { info: run_info(db, &s, &r), request, result: r.result() })
}

/// Delete a run and its checkpoints; refuses an in-use run.
pub fn copy_forget(db: &BulkDb, run_id: &str) -> Result<()> {
    let s = store(db)?;
    let pred = in_use(db);
    s.forget(run_id, &pred)
}

/// Full progress per active copy job (the shell polls this every 500 ms).
pub fn copy_progress_snapshot(db: &BulkDb) -> Vec<CopyProgress> {
    db.jobs.active().iter().filter_map(|j| j.copy.as_ref().map(|c| c.progress(&j.id))).collect()
}
