//! Export orchestration: everything between `BulkDb::export_start` returning a job id and
//! `job:done` being emitted.
//!
//! ```text
//! pending   wait for an export slot (settings.max_concurrent_jobs)
//! preflight open "bulkdb-preflight", run every check
//!           blockers -> rejected (error code from `rejection_error`, e.g. INVALID / OOM_RISK)
//!           unacknowledged warnings -> warned, wait for job_resume / job_cancel
//! running   open "bulkdb-<job id>", apply session, stream -> bounded channel -> sink,
//!           with a "bulkdb-monitor" connection sampling the server every few seconds
//! finishing verify files on disk (and optionally re-count CSV rows), build the result
//! ```
//!
//! `job_cancel` works at any point up to the end of the pipeline: while queued, during
//! preflight, while waiting for acknowledgement, while the server plans the query and
//! mid-stream (the source cancels the server side and the sink renames files to `.partial`).
//! Calls with no server-side work are raced against the token ([`cancellable`]); driver calls
//! that take the token themselves are awaited so their own server-side cancel (PostgreSQL
//! `cancel_query`, MySQL `KILL QUERY`) runs, bounded by [`DRIVER_CANCEL_GRACE`]
//! ([`with_driver_cancel`]).
//!
//! Once the sink has finished, the files carry their final names and are complete, so a
//! cancel that arrives during verification only skips the optional row re-count (with a note)
//! and the job still ends `done`: state and on-disk outcome never disagree.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use crate::error::{CoreError, ErrorPayload, Result};
use crate::history::HistoryStart;
use crate::job::{self, JobEntry};
use crate::monitor::JobHandle;
use crate::preflight::{self, PreflightContext};
use crate::service::{BulkDb, APP_NAME_MONITOR, APP_NAME_PREFLIGHT};
use crate::sink::{self, SinkReport, SinkTarget};
use crate::source::{CsvFormat, Query, Source, StreamOutput};
use crate::sqlguard;
use crate::types::{
    Bottleneck, Connection, CoreEvent, CsvOpts, ExactCount, ExportRequest, JobDone, JobPhase, JobResult, JobServer, JobState, OutputFormat, OutputOpts, PreflightIssue,
    PreflightReport, SessionOpts, Settings, TableRef,
};
use crate::APP_NAME_PREFIX;

/// Preflight warning code raised when the first output file already exists.
const OVERWRITE_CODE: &str = "OVERWRITE";

// ---------------------------------------------------------------------------------------------
// Request resolution (pure helpers, unit tested)
// ---------------------------------------------------------------------------------------------

/// The SQL text of a request: inline, or read from `sql_file` right now. Returns the text and
/// the file path it came from.
pub fn read_sql(req: &ExportRequest) -> Result<(String, Option<String>)> {
    let inline = req.sql.as_deref().map(str::trim).filter(|s| !s.is_empty());
    let file = req.sql_file.as_deref().map(str::trim).filter(|s| !s.is_empty());
    match (inline, file) {
        (Some(_), Some(_)) => Err(CoreError::Invalid("set either `sql` or `sqlFile`, not both".into())),
        (Some(sql), None) => Ok((sql.to_string(), None)),
        (None, Some(path)) => {
            let text = std::fs::read_to_string(path).map_err(|e| CoreError::Io(format!("cannot read SQL file {path}: {e}")))?;
            let text = text.trim_start_matches('\u{feff}').trim();
            if text.is_empty() {
                return Err(CoreError::Invalid(format!("SQL file {path} is empty")));
            }
            Ok((text.to_string(), Some(path.to_string())))
        }
        (None, None) => Err(CoreError::Invalid("no SQL given: set `sql` or `sqlFile`".into())),
    }
}

/// Output directory: the request's, else the connection's default, else the global default.
pub fn resolve_output_dir(req: &ExportRequest, conn: &Connection, settings: &Settings) -> Result<PathBuf> {
    [req.output.dir.as_str(), conn.output_dir.as_deref().unwrap_or(""), settings.default_output_dir.as_deref().unwrap_or("")]
        .into_iter()
        .map(str::trim)
        .find(|s| !s.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| CoreError::Invalid("no output directory: set it in the request, on the connection or in settings".into()))
}

/// CSV options actually used: the request's, or defaults with the platform line ending.
pub fn effective_csv(opts: &OutputOpts, settings: &Settings) -> Option<CsvOpts> {
    match opts.format {
        OutputFormat::Csv => Some(opts.csv.clone().unwrap_or_else(|| CsvOpts { crlf: settings.csv_crlf_default, ..CsvOpts::default() })),
        OutputFormat::Parquet => None,
    }
}

/// Where the files go and what they are called.
pub fn build_target(req: &ExportRequest, conn: &Connection, settings: &Settings, now: chrono::DateTime<chrono::Local>) -> Result<SinkTarget> {
    let dir = resolve_output_dir(req, conn, settings)?;
    let name = req.name.as_deref().map(str::trim).filter(|s| !s.is_empty()).unwrap_or("export");
    let pattern = if req.output.name_pattern.trim().is_empty() { "{conn}_{name}_{yyyyMMdd_HHmm}" } else { req.output.name_pattern.as_str() };
    let stem = sink::naming::expand_pattern(pattern, &conn.name, name, now);
    let mut opts = req.output.clone();
    opts.dir = dir.display().to_string();
    opts.name_pattern = pattern.to_string();
    if opts.format == OutputFormat::Csv {
        let csv = effective_csv(&opts, settings).expect("csv format");
        if csv.delimiter.len() != 1 {
            return Err(CoreError::Invalid(format!("CSV delimiter must be a single byte, got {:?}", csv.delimiter)));
        }
        opts.csv = Some(csv);
    } else {
        opts.compress = None;
        if opts.parquet.is_none() {
            opts.parquet = Some(Default::default());
        }
    }
    if let Some(split) = opts.split.as_ref() {
        if split.rows == Some(0) || split.megabytes == Some(0) {
            return Err(CoreError::Invalid("split size must be greater than zero".into()));
        }
    }
    Ok(SinkTarget { dir, stem, opts })
}

/// Field-wise `request > connection > recommended`.
pub fn merge_session(request: &SessionOpts, connection: &SessionOpts, recommended: &SessionOpts) -> SessionOpts {
    SessionOpts {
        work_mem: request.work_mem.clone().or_else(|| connection.work_mem.clone()).or_else(|| recommended.work_mem.clone()),
        statement_timeout_ms: request.statement_timeout_ms.or(connection.statement_timeout_ms).or(recommended.statement_timeout_ms),
        lock_timeout_ms: request.lock_timeout_ms.or(connection.lock_timeout_ms).or(recommended.lock_timeout_ms),
        max_parallel_workers: request.max_parallel_workers.or(connection.max_parallel_workers).or(recommended.max_parallel_workers),
        fetch_batch_rows: request.fetch_batch_rows.or(connection.fetch_batch_rows).or(recommended.fetch_batch_rows),
    }
}

/// Job label: the request name, else the first words of the SQL.
pub fn label_for(req: &ExportRequest, sql: &str) -> String {
    if let Some(n) = req.name.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        return n.to_string();
    }
    const WORDS: usize = 8;
    const CHARS: usize = 60;
    let mut words = sql.split_whitespace();
    let head: Vec<&str> = words.by_ref().take(WORDS).collect();
    let truncated_words = words.next().is_some();
    let mut label: String = head.join(" ");
    if label.chars().count() > CHARS {
        label = label.chars().take(CHARS).collect::<String>().trim_end().to_string();
        label.push('…');
    } else if truncated_words {
        label.push('…');
    }
    if label.is_empty() {
        "export".to_string()
    } else {
        label
    }
}

/// `CsvOpts` -> the dialect a RawCsv source must produce.
pub fn csv_format(opts: &CsvOpts) -> CsvFormat {
    CsvFormat { delimiter: opts.delimiter.as_bytes().first().copied().unwrap_or(b','), header: opts.header, null_as: opts.null_as.clone(), crlf: opts.crlf }
}

/// RawCsv when the sink wants CSV and the driver can produce it, Arrow otherwise.
pub fn stream_output(source: &dyn Source, target: &SinkTarget) -> StreamOutput {
    match (target.opts.format, target.opts.csv.as_ref()) {
        (OutputFormat::Csv, Some(csv)) if source.supports_raw_csv() => StreamOutput::RawCsv(csv_format(csv)),
        _ => StreamOutput::Arrow,
    }
}

/// Whether the output can be re-read with the csv crate (CSV, not compressed).
pub fn can_recount_csv(target: &SinkTarget) -> bool {
    target.opts.format == OutputFormat::Csv && target.opts.compress.is_none()
}

/// Every file exists; returns the on-disk total and notes about discrepancies.
pub fn verify_outputs(report: &SinkReport) -> Result<(u64, Vec<String>)> {
    let mut total = 0u64;
    let mut notes = Vec::new();
    for f in &report.files {
        let md = std::fs::metadata(f).map_err(|e| CoreError::Io(format!("output file {} is missing after the run: {e}", f.display())))?;
        if !md.is_file() {
            return Err(CoreError::Io(format!("output path {} is not a file", f.display())));
        }
        total += md.len();
    }
    if total != report.bytes {
        notes.push(format!("files hold {total} bytes on disk, the writer reported {} bytes", report.bytes));
    }
    Ok((total, notes))
}

/// Count data rows of a CSV file with the csv crate (quoted newlines handled, header skipped).
pub fn count_csv_rows(path: &Path, opts: &CsvOpts) -> Result<u64> {
    Ok(count_csv_rows_until(path, opts, &CancellationToken::new())?.unwrap_or(0))
}

/// [`count_csv_rows`] that gives up when `stop` fires: `Ok(None)` means "stopped, no count".
/// The token is checked every [`RECOUNT_CHECK_EVERY`] records, so a cancelled re-count of a
/// huge file ends promptly instead of reading it to the end on a blocking thread.
pub fn count_csv_rows_until(path: &Path, opts: &CsvOpts, stop: &CancellationToken) -> Result<Option<u64>> {
    let file = std::fs::File::open(path).map_err(|e| CoreError::Io(format!("{}: {e}", path.display())))?;
    let mut rdr = csv::ReaderBuilder::new()
        .delimiter(opts.delimiter.as_bytes().first().copied().unwrap_or(b','))
        .has_headers(opts.header)
        .flexible(true)
        .from_reader(std::io::BufReader::with_capacity(1 << 20, file));
    let mut record = csv::ByteRecord::new();
    let mut n = 0u64;
    loop {
        if n.is_multiple_of(RECOUNT_CHECK_EVERY) && stop.is_cancelled() {
            return Ok(None);
        }
        if !rdr.read_byte_record(&mut record).map_err(|e| CoreError::Io(format!("{}: {e}", path.display())))? {
            return Ok(Some(n));
        }
        n += 1;
    }
}

/// How often (in records) the CSV re-count checks for cancellation.
const RECOUNT_CHECK_EVERY: u64 = 65_536;

/// Note added to the result when a cancel during finishing skipped the row re-count.
pub const RECOUNT_SKIPPED_NOTE: &str = "row-count verification skipped: cancel was requested after every file had been written";

/// Race a future against the job's cancellation token; the future is dropped as soon as the
/// token fires. Only for calls with no server-side work worth cancelling (opening a
/// connection, `SET` statements); driver calls that take the token use [`with_driver_cancel`].
pub async fn cancellable<T, F>(cancel: &CancellationToken, fut: F) -> Result<T>
where
    F: Future<Output = Result<T>>,
{
    tokio::select! {
        biased;
        _ = cancel.cancelled() => Err(CoreError::Cancelled),
        r = fut => r,
    }
}

/// How long a driver call that was handed the job's token gets to finish its own server-side
/// cancel (PostgreSQL `cancel_query`, MySQL `KILL QUERY`) after the token fires.
pub const DRIVER_CANCEL_GRACE: Duration = Duration::from_secs(3);

/// Await a driver call that received `cancel` itself (`Source::stream`, `Catalog::exact_count`).
///
/// Unlike [`cancellable`] the future is not dropped when the token fires: the driver's own
/// cancel branch must run so the statement is stopped on the server. Only when the driver has
/// not returned `grace` after the token fired is the call abandoned. Once the token has fired,
/// any driver error is reported as `Cancelled` (the server's "canceling statement due to user
/// request" is the expected outcome, not a failure); a success that raced the cancel is
/// returned as is, and the caller decides what to do with it.
pub async fn with_driver_cancel<T, F>(cancel: &CancellationToken, grace: Duration, fut: F) -> Result<T>
where
    F: Future<Output = Result<T>>,
{
    tokio::pin!(fut);
    tokio::select! {
        biased;
        r = &mut fut => return r,
        _ = cancel.cancelled() => {}
    }
    match tokio::time::timeout(grace, &mut fut).await {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(e)) => {
            if !matches!(e, CoreError::Cancelled) {
                tracing::debug!(code = e.code(), "driver call ended after cancellation: {e}");
            }
            Err(CoreError::Cancelled)
        }
        Err(_) => {
            tracing::warn!("driver did not acknowledge cancellation within {grace:?}; abandoning the call");
            Err(CoreError::Cancelled)
        }
    }
}

/// Transition with a log line carrying the job id (the log panel filters on it).
pub fn transition(bulk: &BulkDb, job: &JobEntry, to: JobState) -> Result<()> {
    let from = bulk.jobs.transition(job, to)?;
    tracing::info!(job_id = %job.id, from = ?from, to = ?to, "job state {from:?} -> {to:?}");
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Preflight
// ---------------------------------------------------------------------------------------------

/// A request resolved against the saved connection and settings (no server contact yet).
pub struct PreparedExport {
    pub conn: Connection,
    pub sql: String,
    pub sql_file: Option<String>,
    pub target: SinkTarget,
    /// Request options over the connection's defaults (server recommendations come later).
    pub session: SessionOpts,
    pub label: String,
}

pub fn prepare(bulk: &BulkDb, req: &ExportRequest) -> Result<PreparedExport> {
    let conn = bulk.conn_get(&req.conn_id)?;
    let (sql, sql_file) = read_sql(req)?;
    let settings = bulk.settings_get();
    let target = build_target(req, &conn, &settings, chrono::Local::now())?;
    let session = merge_session(&req.session, &conn.session, &SessionOpts::default());
    let label = label_for(req, &sql);
    Ok(PreparedExport { conn, sql, sql_file, target, session, label })
}

/// Open a fresh `bulkdb-preflight` connection and run every check.
pub async fn preflight(bulk: &BulkDb, req: &ExportRequest, prep: &PreparedExport, cancel: &CancellationToken) -> Result<PreflightReport> {
    let live = cancellable(cancel, bulk.open_connection(&prep.conn, req.password.clone(), APP_NAME_PREFLIGHT)).await?;
    let ctx = PreflightContext { request: req, sql: &prep.sql, source: live.source(), monitor: Some(live.monitor()), target: &prep.target, session: &prep.session };
    let report = tokio::select! {
        biased;
        _ = cancel.cancelled() => return Err(CoreError::Cancelled),
        r = preflight::run(ctx) => r,
    };
    tracing::info!(
        conn_id = %prep.conn.id,
        blockers = report.blockers.len(),
        warnings = report.warnings.len(),
        estimated_rows = ?report.estimates.rows,
        elapsed_ms = report.elapsed_ms,
        "preflight finished"
    );
    Ok(report)
}

// ---------------------------------------------------------------------------------------------
// Export job
// ---------------------------------------------------------------------------------------------

enum Outcome {
    Done(JobResult),
    Rejected(PreflightIssue),
}

#[derive(Default)]
struct RunCtx {
    /// `history.record_start` was called (so `record_finish` must follow).
    started: bool,
}

/// Body of the background task spawned by `BulkDb::export_start`.
pub async fn run_export_job(bulk: Arc<BulkDb>, job: Arc<JobEntry>, req: ExportRequest, acknowledged: Vec<String>) {
    let mut ctx = RunCtx::default();
    match run_export_inner(&bulk, &job, &req, acknowledged, &mut ctx).await {
        Ok(Outcome::Done(result)) => {
            tracing::info!(job_id = %job.id, rows = result.rows, bytes = result.bytes, files = result.files.len(), duration_ms = result.duration_ms, "export done");
            if ctx.started {
                bulk.history_finish(&job.id, JobState::Done, Some(&result), None);
            }
            bulk.jobs.finish(&job, Ok(result));
        }
        Ok(Outcome::Rejected(issue)) => {
            tracing::warn!(job_id = %job.id, code = %issue.code, "export rejected by preflight: {}", issue.message);
            finish_rejected(&bulk, &job, &issue);
        }
        Err(e) => {
            let state = if matches!(e, CoreError::Cancelled) { JobState::Cancelled } else { JobState::Failed };
            if state == JobState::Cancelled {
                tracing::info!(job_id = %job.id, "export cancelled");
            } else {
                tracing::error!(job_id = %job.id, code = e.code(), "export failed: {e}");
            }
            if ctx.started {
                bulk.history_finish(&job.id, state, None, Some(&e.to_string()));
            }
            bulk.jobs.finish(&job, Err(e));
        }
    }
}

/// The statement an export hands to the driver: the SQL guard's normalized single statement,
/// lexed with the rules of `driver_id`'s server (the same check preflight ran). Comments before
/// the statement stay; the trailing `;` and anything after it (a trailing comment) are gone, so
/// drivers that wrap the text (`COPY (...) TO STDOUT`) or send it over a multi-statement
/// protocol (MySQL) see exactly one statement. `Err` is the guard's first issue.
pub fn extraction_statement(sql: &str, driver_id: &str) -> std::result::Result<String, PreflightIssue> {
    let check = sqlguard::check_for(sql, sqlguard::Dialect::from_driver(driver_id));
    if check.ok {
        return Ok(check.normalized);
    }
    Err(check.issues.into_iter().next().unwrap_or_else(|| PreflightIssue {
        code: sqlguard::CODE_NOT_SELECT.into(),
        severity: crate::types::Severity::Danger,
        message: "only a single read-only SELECT statement can be extracted".into(),
        detail: None,
        recommendation: None,
    }))
}

/// The `CoreError` a preflight blocker stands for, so a rejected job's error code is one of
/// the documented `CoreError::code()` values. `JobState::Rejected` stays the signal that
/// preflight (not the run) stopped the job.
pub fn rejection_error(issue: &PreflightIssue) -> CoreError {
    let msg = issue.message.clone();
    let code = issue.code.as_str();
    if code.contains("MEM") || code.contains("OOM") {
        CoreError::OomRisk(msg)
    } else {
        match code {
            "DISK_FULL" | "DIR_NOT_WRITABLE" => CoreError::Io(msg),
            "CONNECT_FAILED" => CoreError::Connect(msg),
            "EXPLAIN_FAILED" => CoreError::query(msg),
            _ => CoreError::Invalid(msg),
        }
    }
}

/// Payload of a rejected job: code from [`rejection_error`], the blocker's message as is, and
/// the blocker code plus its detail (or recommendation) as detail.
pub fn rejection_payload(issue: &PreflightIssue) -> ErrorPayload {
    let detail = match issue.detail.as_deref().or(issue.recommendation.as_deref()) {
        Some(d) => format!("preflight blocker {}: {d}", issue.code),
        None => format!("preflight blocker {}", issue.code),
    };
    ErrorPayload { code: rejection_error(issue).code().to_string(), message: issue.message.clone(), detail: Some(detail) }
}

/// Rejected is not a `finish(Err)`: the registry would call it `failed`. Store the payload,
/// transition and emit `job:done` by hand.
pub fn finish_rejected(bulk: &BulkDb, job: &JobEntry, issue: &PreflightIssue) {
    let payload = rejection_payload(issue);
    *job.error.lock().unwrap() = Some(payload.clone());
    if let Err(e) = transition(bulk, job, JobState::Rejected) {
        tracing::error!(job_id = %job.id, "cannot mark job rejected: {e}");
    }
    let _ =
        bulk.jobs.events().send(CoreEvent::JobDone(JobDone { job_id: job.id.clone(), state: JobState::Rejected, result: None, error: Some(payload), exact_count: None }));
}

async fn run_export_inner(bulk: &Arc<BulkDb>, job: &Arc<JobEntry>, req: &ExportRequest, acknowledged: Vec<String>, ctx: &mut RunCtx) -> Result<Outcome> {
    let cancel = job.cancel.clone();

    // -- pending: wait for a slot ---------------------------------------------------------
    let slots = bulk.jobs.export_slots.clone();
    if slots.available_permits() == 0 {
        tracing::info!(job_id = %job.id, "queued: waiting for an export slot");
    }
    let _permit = tokio::select! {
        biased;
        _ = cancel.cancelled() => return Err(CoreError::Cancelled),
        p = slots.acquire_owned() => p.map_err(|_| CoreError::Internal("export slots closed".into()))?,
    };

    // -- preflight ------------------------------------------------------------------------
    transition(bulk, job, JobState::Preflight)?;
    let mut prep = prepare(bulk, req)?;
    if let Err(e) = std::fs::create_dir_all(&prep.target.dir) {
        tracing::warn!(job_id = %job.id, dir = %prep.target.dir.display(), "cannot create output directory: {e}");
    }
    let report = preflight(bulk, req, &prep, &cancel).await?;
    *job.preflight.lock().unwrap() = Some(report.clone());
    if let Some(blocker) = report.blockers.first() {
        return Ok(Outcome::Rejected(blocker.clone()));
    }
    // Preflight passed the guard; the driver gets exactly the statement the guard approved.
    let statement = match extraction_statement(&prep.sql, &prep.conn.driver) {
        Ok(s) => s,
        Err(issue) => return Ok(Outcome::Rejected(issue)),
    };
    let mut acknowledged = acknowledged;
    let pending: Vec<String> = report.warnings.iter().filter(|w| !acknowledged.contains(&w.code)).map(|w| w.code.clone()).collect();
    if !pending.is_empty() {
        let (tx, rx) = oneshot::channel::<Vec<String>>();
        *job.resume.lock().unwrap() = Some(tx);
        transition(bulk, job, JobState::Warned)?;
        tracing::info!(job_id = %job.id, codes = ?pending, "waiting for {} warning(s) to be acknowledged", pending.len());
        let codes = tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                job.resume.lock().unwrap().take();
                return Err(CoreError::Cancelled);
            }
            r = rx => r.map_err(|_| CoreError::Cancelled)?,
        };
        acknowledged.extend(codes);
        tracing::info!(job_id = %job.id, codes = ?acknowledged, "warnings acknowledged");
    }
    // Preflight says "acknowledge to overwrite": an acknowledged OVERWRITE warning turns the
    // option on, otherwise the sink would refuse the existing file.
    if !prep.target.opts.overwrite && report.warnings.iter().any(|w| w.code == OVERWRITE_CODE) && acknowledged.iter().any(|c| c == OVERWRITE_CODE) {
        tracing::info!(job_id = %job.id, "overwrite acknowledged: existing output files will be replaced");
        prep.target.opts.overwrite = true;
    }

    // -- running --------------------------------------------------------------------------
    transition(bulk, job, JobState::Running)?;
    ctx.started = true;
    let run_started = Instant::now();
    let recommended = report.server.as_ref().map(preflight::recommended_session).unwrap_or_default();
    let session = merge_session(&req.session, &prep.conn.session, &recommended);
    if let Err(e) = bulk.history.record_start(HistoryStart {
        job_id: &job.id,
        conn_id: &prep.conn.id,
        label: &job.label,
        sql: &prep.sql,
        sql_file: prep.sql_file.as_deref(),
        session: &session,
        output: &prep.target.opts,
    }) {
        tracing::warn!(job_id = %job.id, "history: {e}");
    }
    let app_name = format!("{APP_NAME_PREFIX}{}", job.id);
    let live = cancellable(&cancel, bulk.open_connection(&prep.conn, req.password.clone(), &app_name)).await?;
    tracing::info!(job_id = %job.id, driver = live.driver_id(), session = ?session, "session opened");
    cancellable(&cancel, live.apply_session(&session)).await?;
    let output = stream_output(live.source(), &prep.target);
    let raw_csv = matches!(output, StreamOutput::RawCsv(_));
    let query = Query { sql: statement, output };
    tracing::info!(job_id = %job.id, raw_csv, target = %prep.target.path_for(0).display(), "starting query");
    // The driver got the token: awaited (not raced) so a cancel while the server is still
    // planning or sorting reaches the server through the driver's own cancel path.
    let stream = with_driver_cancel(&cancel, DRIVER_CANCEL_GRACE, live.stream(&query, &session, cancel.clone())).await?;
    if cancel.is_cancelled() {
        // The query started just as the cancel arrived: dropping the stream stops it server
        // side, and no output file has been created yet.
        drop(stream);
        return Err(CoreError::Cancelled);
    }
    let sink = sink::build(&prep.target)?;
    if let Some(rows) = report.estimates.rows.or_else(|| report.plan.as_ref().and_then(|p| p.estimated_rows)) {
        job.progress.expected_rows.store(rows, Ordering::Relaxed);
    }
    let monitor = spawn_monitor(bulk, job, &prep.conn, req.password.clone(), live.session_id());
    let pipeline = job::run_pipeline(stream, sink, job.progress.clone(), cancel.clone()).await;
    monitor.stop().await;
    let written = pipeline?;
    tracing::info!(job_id = %job.id, rows = written.rows, bytes = written.bytes, files = written.files.len(), "pipeline finished");

    // -- finishing ------------------------------------------------------------------------
    // The sink has finished: every file is complete and carries its final name. From here on
    // a cancel cannot undo the export, so it only skips the optional re-count and the job
    // ends `done` (see the module docs).
    transition(bulk, job, JobState::Finishing)?;
    job.progress.set_phase(JobPhase::Finishing);
    drop(live);
    let (bytes_on_disk, mut notes) = verify_outputs(&written)?;
    let mut verified_rows = None;
    if bulk.settings_get().verify_after_export && can_recount_csv(&prep.target) {
        let counted = if cancel.is_cancelled() {
            None
        } else {
            let csv = prep.target.opts.csv.clone().unwrap_or_default();
            let files = written.files.clone();
            let stop = cancel.clone();
            tokio::task::spawn_blocking(move || {
                let mut n = 0u64;
                for f in &files {
                    match count_csv_rows_until(f, &csv, &stop)? {
                        Some(c) => n += c,
                        None => return Ok(None),
                    }
                }
                Ok::<Option<u64>, CoreError>(Some(n))
            })
            .await??
        };
        match counted {
            Some(counted) => {
                if counted != written.rows {
                    notes.push(format!("verification re-read {counted} rows, the writer reported {}", written.rows));
                    tracing::warn!(job_id = %job.id, counted, reported = written.rows, "row count mismatch after export");
                } else {
                    tracing::info!(job_id = %job.id, counted, "verified row count");
                }
                verified_rows = Some(counted);
            }
            None => notes.push(RECOUNT_SKIPPED_NOTE.to_string()),
        }
    }
    if cancel.is_cancelled() {
        tracing::info!(job_id = %job.id, "cancel requested after the files were complete; finishing as done");
    }
    Ok(Outcome::Done(JobResult {
        rows: written.rows,
        bytes: bytes_on_disk,
        files: written.files.iter().map(|p| p.display().to_string()).collect(),
        duration_ms: run_started.elapsed().as_millis() as u64,
        verified_rows,
        notes,
    }))
}

// ---------------------------------------------------------------------------------------------
// Server monitor
// ---------------------------------------------------------------------------------------------

/// Handle of the monitor loop; `stop` ends it and waits briefly for it to exit.
pub struct MonitorTask {
    stop: CancellationToken,
    handle: tokio::task::JoinHandle<()>,
}

impl MonitorTask {
    pub async fn stop(self) {
        self.stop.cancel();
        if tokio::time::timeout(Duration::from_secs(5), self.handle).await.is_err() {
            tracing::debug!("monitor loop did not stop within 5 s");
        }
    }
}

fn spawn_monitor(bulk: &Arc<BulkDb>, job: &Arc<JobEntry>, conn: &Connection, password: Option<String>, session_id: Option<String>) -> MonitorTask {
    let stop = CancellationToken::new();
    let interval = Duration::from_millis(bulk.settings_get().monitor_interval_ms.max(250) as u64);
    let handle = tokio::spawn(monitor_loop(bulk.clone(), job.clone(), conn.clone(), password, session_id, interval, stop.clone()));
    MonitorTask { stop, handle }
}

/// `true` when the loop must stop.
async fn pause(stop: &CancellationToken, cancel: &CancellationToken, d: Duration) -> bool {
    tokio::select! {
        biased;
        _ = stop.cancelled() => true,
        _ = cancel.cancelled() => true,
        _ = tokio::time::sleep(d) => false,
    }
}

async fn monitor_loop(
    bulk: Arc<BulkDb>,
    job: Arc<JobEntry>,
    conn: Connection,
    password: Option<String>,
    session_id: Option<String>,
    interval: Duration,
    stop: CancellationToken,
) {
    // Give the extraction session a moment to show up server-side.
    if pause(&stop, &job.cancel, interval.min(Duration::from_secs(1))).await {
        return;
    }
    // Opening the connection and sampling are read-only and may be dropped at any moment, so
    // both are raced against stop / cancel: a stuck monitor never delays the job's finish
    // (and with it `BulkDb::shutdown`).
    let opened = tokio::select! {
        biased;
        _ = stop.cancelled() => return,
        _ = job.cancel.cancelled() => return,
        r = bulk.open_connection(&conn, password, APP_NAME_MONITOR) => r,
    };
    let live = match opened {
        Ok(l) => l,
        Err(e) => {
            tracing::warn!(job_id = %job.id, "server monitor unavailable: {e}");
            return;
        }
    };
    let mut handle = JobHandle { job_id: job.id.clone(), application_name: format!("{APP_NAME_PREFIX}{}", job.id), session_id, temp_bytes_at_start: None };
    loop {
        let sampled = tokio::select! {
            biased;
            _ = stop.cancelled() => return,
            _ = job.cancel.cancelled() => return,
            r = live.monitor().snapshot(&handle) => r,
        };
        match sampled {
            Ok(snap) if snap.found => {
                if handle.temp_bytes_at_start.is_none() {
                    handle.temp_bytes_at_start = snap.temp_bytes;
                }
                let bottleneck = snap.client_write_ratio.map(|r| if r > 0.5 { Bottleneck::Network } else { Bottleneck::Server });
                let phase = job.progress.phase();
                match bottleneck {
                    Some(Bottleneck::Network) if phase != JobPhase::Finishing => job.progress.set_phase(JobPhase::NetworkBound),
                    Some(Bottleneck::Server) if phase == JobPhase::NetworkBound => job.progress.set_phase(JobPhase::Transferring),
                    _ => {}
                }
                let temp_bytes = match (snap.temp_bytes, handle.temp_bytes_at_start) {
                    (Some(now), Some(start)) => Some(now.saturating_sub(start)),
                    (now, _) => now,
                };
                tracing::debug!(job_id = %job.id, pid = ?snap.pid, state = ?snap.state, wait = ?snap.wait_event, server_rows = ?snap.server_rows, "server snapshot");
                *job.server.lock().unwrap() = Some(JobServer {
                    job_id: job.id.clone(),
                    pid: snap.pid,
                    state: snap.state,
                    wait_event: snap.wait_event,
                    server_rows: snap.server_rows,
                    temp_bytes,
                    bottleneck,
                    elapsed_ms: snap.elapsed_ms,
                });
            }
            Ok(_) => tracing::debug!(job_id = %job.id, "extraction session not visible to the monitor"),
            Err(e) => tracing::debug!(job_id = %job.id, "monitor snapshot failed: {e}"),
        }
        if pause(&stop, &job.cancel, interval).await {
            return;
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Exact count job
// ---------------------------------------------------------------------------------------------

/// Body of the task spawned by `BulkDb::catalog_exact_count`.
pub async fn run_exact_count_job(bulk: Arc<BulkDb>, job: Arc<JobEntry>, table: TableRef) {
    match exact_count_inner(&bulk, &job, &table).await {
        Ok(count) => {
            tracing::info!(job_id = %job.id, table = %table.qualified(), rows = count.rows, elapsed_ms = count.elapsed_ms, "exact count done");
            *job.exact_count.lock().unwrap() = Some(count.clone());
            if let Err(e) = bulk.history.record_exact_count(&job.conn_id, &count) {
                tracing::warn!(job_id = %job.id, "history: {e}");
            }
            bulk.jobs.finish(&job, Ok(JobResult { rows: count.rows, bytes: 0, files: vec![], duration_ms: count.elapsed_ms, verified_rows: None, notes: vec![] }));
        }
        Err(e) => {
            if matches!(e, CoreError::Cancelled) {
                tracing::info!(job_id = %job.id, "exact count cancelled");
            } else {
                tracing::error!(job_id = %job.id, "exact count failed: {e}");
            }
            bulk.jobs.finish(&job, Err(e));
        }
    }
}

async fn exact_count_inner(bulk: &Arc<BulkDb>, job: &Arc<JobEntry>, table: &TableRef) -> Result<ExactCount> {
    let cancel = job.cancel.clone();
    transition(bulk, job, JobState::Running)?;
    let conn = bulk.conn_get(&job.conn_id)?;
    let app_name = format!("{APP_NAME_PREFIX}{}", job.id);
    let live = cancellable(&cancel, bulk.open_connection(&conn, None, &app_name)).await?;
    let t0 = Instant::now();
    // Awaited, not raced: the driver cancels the `count(*)` on the server itself. A count that
    // completed just as the cancel arrived is still reported.
    let rows = with_driver_cancel(&cancel, DRIVER_CANCEL_GRACE, live.exact_count(table, cancel.clone())).await?;
    Ok(ExactCount { table: table.clone(), rows, elapsed_ms: t0.elapsed().as_millis() as u64, at: crate::types::now_rfc3339() })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::SplitOpts;
    use chrono::TimeZone;
    use std::io::Write;

    fn req() -> ExportRequest {
        ExportRequest {
            conn_id: "c".into(),
            sql: Some("select * from t".into()),
            sql_file: None,
            name: None,
            session: SessionOpts::default(),
            output: OutputOpts { dir: "C:/out".into(), ..Default::default() },
            password: None,
        }
    }

    #[test]
    fn sql_source_rules() {
        let mut r = req();
        assert_eq!(read_sql(&r).unwrap(), ("select * from t".to_string(), None));
        r.sql_file = Some("x.sql".into());
        assert!(matches!(read_sql(&r), Err(CoreError::Invalid(_))));
        r.sql = None;
        r.sql_file = None;
        assert!(matches!(read_sql(&r), Err(CoreError::Invalid(_))));
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("q.sql");
        std::fs::write(&p, "\u{feff}  select 1  \n").unwrap();
        r.sql_file = Some(p.display().to_string());
        let (sql, file) = read_sql(&r).unwrap();
        assert_eq!(sql, "select 1");
        assert_eq!(file.as_deref(), Some(p.display().to_string().as_str()));
        std::fs::write(&p, "   ").unwrap();
        assert!(matches!(read_sql(&r), Err(CoreError::Invalid(_))));
    }

    #[test]
    fn output_dir_precedence_and_target() {
        let mut r = req();
        let mut conn = Connection { id: "c".into(), name: "prod db".into(), driver: "postgres".into(), ..Default::default() };
        let mut settings = Settings::default();
        assert_eq!(resolve_output_dir(&r, &conn, &settings).unwrap(), PathBuf::from("C:/out"));
        r.output.dir = String::new();
        assert!(resolve_output_dir(&r, &conn, &settings).is_err());
        settings.default_output_dir = Some("D:/default".into());
        assert_eq!(resolve_output_dir(&r, &conn, &settings).unwrap(), PathBuf::from("D:/default"));
        conn.output_dir = Some("E:/conn".into());
        assert_eq!(resolve_output_dir(&r, &conn, &settings).unwrap(), PathBuf::from("E:/conn"));

        let now = chrono::Local.with_ymd_and_hms(2026, 10, 26, 9, 30, 0).unwrap();
        r.name = Some("orders".into());
        settings.csv_crlf_default = true;
        let t = build_target(&r, &conn, &settings, now).unwrap();
        assert_eq!(t.stem, "prod db_orders_20261026_0930");
        assert_eq!(t.dir, PathBuf::from("E:/conn"));
        assert_eq!(t.opts.dir, "E:/conn");
        assert_eq!(t.path_for(0).file_name().unwrap().to_string_lossy(), "prod db_orders_20261026_0930.csv");
        // CsvOpts::default() is used by OutputOpts::default(), so crlf stays as given (false).
        assert!(!t.opts.csv.as_ref().unwrap().crlf);
        r.output.csv = None;
        let t = build_target(&r, &conn, &settings, now).unwrap();
        assert!(t.opts.csv.as_ref().unwrap().crlf, "platform default applied when the request has no csv opts");

        r.output.csv = Some(CsvOpts { delimiter: "ab".into(), ..Default::default() });
        assert!(matches!(build_target(&r, &conn, &settings, now), Err(CoreError::Invalid(_))));
        r.output.csv = None;
        r.output.split = Some(SplitOpts { rows: Some(0), megabytes: None });
        assert!(matches!(build_target(&r, &conn, &settings, now), Err(CoreError::Invalid(_))));
        r.output.split = None;
        r.output.format = OutputFormat::Parquet;
        r.output.compress = Some(crate::types::Compression::Gzip);
        let t = build_target(&r, &conn, &settings, now).unwrap();
        assert!(t.opts.compress.is_none(), "parquet is never externally compressed");
        assert!(t.opts.parquet.is_some());
        assert_eq!(t.extension(), ".parquet");
    }

    #[test]
    fn session_merge_and_label() {
        let request = SessionOpts { work_mem: Some("32MB".into()), ..Default::default() };
        let connection = SessionOpts { work_mem: Some("16MB".into()), statement_timeout_ms: Some(10), ..Default::default() };
        let recommended = SessionOpts { work_mem: Some("64MB".into()), statement_timeout_ms: Some(20), lock_timeout_ms: Some(5), ..Default::default() };
        let m = merge_session(&request, &connection, &recommended);
        assert_eq!(m.work_mem.as_deref(), Some("32MB"));
        assert_eq!(m.statement_timeout_ms, Some(10));
        assert_eq!(m.lock_timeout_ms, Some(5));
        assert_eq!(m.max_parallel_workers, None);

        let mut r = req();
        assert_eq!(label_for(&r, "select  a,\n b from t where x = 1 and y = 2 order by a"), "select a, b from t where x =…");
        assert_eq!(label_for(&r, "select 1"), "select 1");
        assert_eq!(label_for(&r, ""), "export");
        r.name = Some("  orders  ".into());
        assert_eq!(label_for(&r, "select 1"), "orders");
    }

    #[test]
    fn csv_recount_handles_quoted_newlines() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("x.csv");
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(b"id;text\r\n1;\"line1\nline2\"\r\n2;plain\r\n3;\"has;semi\"\r\n").unwrap();
        drop(f);
        let opts = CsvOpts { delimiter: ";".into(), header: true, ..Default::default() };
        assert_eq!(count_csv_rows(&p, &opts).unwrap(), 3);
        let opts = CsvOpts { delimiter: ";".into(), header: false, ..Default::default() };
        assert_eq!(count_csv_rows(&p, &opts).unwrap(), 4);
        std::fs::write(&p, "id,text\n").unwrap();
        assert_eq!(count_csv_rows(&p, &CsvOpts::default()).unwrap(), 0);
    }

    #[test]
    fn verify_reports_missing_and_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a.csv");
        std::fs::write(&p, b"12345").unwrap();
        let ok = SinkReport { rows: 1, bytes: 5, files: vec![p.clone()] };
        assert_eq!(verify_outputs(&ok).unwrap(), (5, vec![]));
        let off = SinkReport { rows: 1, bytes: 9, files: vec![p.clone()] };
        let (bytes, notes) = verify_outputs(&off).unwrap();
        assert_eq!(bytes, 5);
        assert_eq!(notes.len(), 1);
        let missing = SinkReport { rows: 1, bytes: 5, files: vec![dir.path().join("nope.csv")] };
        assert!(matches!(verify_outputs(&missing), Err(CoreError::Io(_))));
    }

    #[tokio::test]
    async fn cancellable_prefers_cancel() {
        let c = CancellationToken::new();
        c.cancel();
        let r: Result<()> = cancellable(&c, async { Ok(()) }).await;
        assert!(matches!(r, Err(CoreError::Cancelled)));
        let c = CancellationToken::new();
        assert_eq!(cancellable(&c, async { Ok(5) }).await.unwrap(), 5);
    }

    #[tokio::test]
    async fn driver_cancel_lets_the_driver_run_its_cancel_branch() {
        use std::sync::atomic::AtomicBool;
        // A driver that reacts to the token by "sending a server-side cancel" (takes a while)
        // and then returns its own error: the branch runs to completion, the result is Cancelled.
        let c = CancellationToken::new();
        let server_cancelled = Arc::new(AtomicBool::new(false));
        let driver = {
            let (c, flag) = (c.clone(), server_cancelled.clone());
            async move {
                c.cancelled().await;
                tokio::time::sleep(Duration::from_millis(100)).await;
                flag.store(true, Ordering::SeqCst);
                Err::<u64, _>(CoreError::query_with_code("57014", "canceling statement due to user request"))
            }
        };
        let canceller = {
            let c = c.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(20)).await;
                c.cancel();
            })
        };
        let r = with_driver_cancel(&c, Duration::from_secs(5), driver).await;
        canceller.await.unwrap();
        assert!(matches!(r, Err(CoreError::Cancelled)), "{r:?}");
        assert!(server_cancelled.load(Ordering::SeqCst), "the driver's cancel branch was pre-empted");

        // A success that races the cancel is returned; no cancel means plain pass-through.
        let c = CancellationToken::new();
        c.cancel();
        assert_eq!(with_driver_cancel(&c, Duration::from_secs(1), async { Ok(7) }).await.unwrap(), 7);
        let c = CancellationToken::new();
        assert!(matches!(with_driver_cancel(&c, Duration::from_secs(1), async { Err::<(), _>(CoreError::query("boom")) }).await, Err(CoreError::Query { .. })));

        // A driver that ignores the token is abandoned after the grace period.
        let c = CancellationToken::new();
        c.cancel();
        let t0 = Instant::now();
        let r = with_driver_cancel(&c, Duration::from_millis(50), std::future::pending::<Result<()>>()).await;
        assert!(matches!(r, Err(CoreError::Cancelled)));
        assert!(t0.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn rejections_use_documented_error_codes() {
        let issue = |code: &str, detail: Option<&str>, rec: Option<&str>| PreflightIssue {
            code: code.into(),
            severity: crate::types::Severity::Danger,
            message: format!("{code} happened"),
            detail: detail.map(str::to_string),
            recommendation: rec.map(str::to_string),
        };
        assert_eq!(rejection_error(&issue("WORK_MEM_HIGH", None, None)).code(), "OOM_RISK");
        assert_eq!(rejection_error(&issue("DISK_FULL", None, None)).code(), "IO");
        assert_eq!(rejection_error(&issue("DIR_NOT_WRITABLE", None, None)).code(), "IO");
        assert_eq!(rejection_error(&issue("CONNECT_FAILED", None, None)).code(), "CONNECT");
        assert_eq!(rejection_error(&issue("EXPLAIN_FAILED", None, None)).code(), "QUERY");
        assert_eq!(rejection_error(&issue(crate::sqlguard::CODE_NOT_SELECT, None, None)).code(), "INVALID");
        assert_eq!(rejection_error(&issue(crate::sqlguard::CODE_MULTI, None, None)).code(), "INVALID");

        let p = rejection_payload(&issue("SQL_NOT_SELECT", None, Some("use a SELECT")));
        assert_eq!(p.code, "INVALID");
        assert_eq!(p.message, "SQL_NOT_SELECT happened");
        assert_eq!(p.detail.as_deref(), Some("preflight blocker SQL_NOT_SELECT: use a SELECT"));
        let p = rejection_payload(&issue("DISK_FULL", Some("estimate 1 GB"), Some("free space")));
        assert_eq!(p.detail.as_deref(), Some("preflight blocker DISK_FULL: estimate 1 GB"));
        assert_eq!(rejection_payload(&issue("X", None, None)).detail.as_deref(), Some("preflight blocker X"));
    }

    #[test]
    fn extraction_statement_is_the_guarded_single_statement() {
        assert_eq!(extraction_statement("  select 1 ;  -- done\n", "postgres").unwrap(), "select 1");
        assert_eq!(extraction_statement("-- head\nselect 1;", "sqlite").unwrap(), "-- head\nselect 1");
        assert_eq!(extraction_statement("select 1", "odbc").unwrap(), "select 1");
        assert_eq!(extraction_statement("delete from t", "postgres").unwrap_err().code, crate::sqlguard::CODE_NOT_SELECT);
        assert_eq!(extraction_statement("select 1; select 2", "sqlite").unwrap_err().code, crate::sqlguard::CODE_MULTI);
        // MySQL reads `\'` as an escaped quote: what the standard rules see as one string
        // literal is a closed string, a `;` and a second statement on a MySQL server, and the
        // MySQL dialect catches it.
        let sneaky = r"select '\'' ; delete from t -- '";
        assert!(extraction_statement(sneaky, "postgres").is_ok());
        assert_eq!(extraction_statement(sneaky, "mysql").unwrap_err().code, crate::sqlguard::CODE_MULTI);
        assert!(extraction_statement(sneaky, "odbc").is_err(), "unknown back ends must pass every reading");
        assert_eq!(extraction_statement("/*!50000 select 1 */", "mysql").unwrap_err().code, crate::sqlguard::CODE_EXEC_COMMENT);
    }

    #[test]
    fn csv_recount_stops_on_cancel() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("x.csv");
        std::fs::write(&p, "a\n1\n2\n").unwrap();
        let stop = CancellationToken::new();
        assert_eq!(count_csv_rows_until(&p, &CsvOpts::default(), &stop).unwrap(), Some(2));
        stop.cancel();
        assert_eq!(count_csv_rows_until(&p, &CsvOpts::default(), &stop).unwrap(), None);
    }
}
