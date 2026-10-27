//! JobRunner: `Source.stream() -> mpsc::channel(8) -> Sink.write()` plus the job registry and
//! the nine-state machine.
//!
//! * The reader is a tokio task pulling the source stream and sending into a bounded channel.
//! * The writer is a blocking thread (`spawn_blocking`) draining the channel into the sink.
//! * Back-pressure comes from the channel; the channel depth (8) x batch size (8 MB) is the
//!   memory bound.
//! * One `CancellationToken` is shared by reader, writer and the source's server-side cancel.
//!   On cancel the reader polls the source once more (bounded by [`CANCEL_GRACE`]) so the driver
//!   can send its server-side cancel, then the sink is aborted and its files renamed to `.partial`.
//! * An empty result still produces a valid file: when the source announced a schema and no
//!   batch arrived, the writer hands the sink one zero-row batch of that schema before `finish`
//!   (CSV header / Parquet schema).
//! * Progress is a set of atomic counters read by whoever wants them (Tauri every 200 ms,
//!   the CLI every second). Core pushes no progress events.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use arrow_array::RecordBatch;
use arrow_schema::SchemaRef;

use futures::StreamExt;
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;

use crate::error::{CoreError, ErrorPayload, Result};
use crate::sink::{Sink, SinkReport};
use crate::source::{Batch, BoxStream, SourceStream};
use crate::types::{
    CoreEvent, ExactCount, ExportRequest, JobDone, JobKind, JobPhase, JobProgress, JobResult, JobServer, JobState, JobStateEvent, JobSummary, PreflightReport,
};
use crate::CHANNEL_DEPTH;

/// How long the reader keeps polling a cancelled source so its driver can cancel the query on
/// the server (PostgreSQL `cancel_query`, MySQL `KILL QUERY`, MSSQL attention, ...). Drivers
/// check the token first when polled, so this normally takes one round trip; a source that does
/// not answer within this time is dropped anyway.
pub const CANCEL_GRACE: Duration = Duration::from_secs(2);

// ---------------------------------------------------------------------------------------------
// Progress
// ---------------------------------------------------------------------------------------------

/// Lock-free progress counters shared by the pipeline and its observers.
pub struct Progress {
    pub rows: AtomicU64,
    pub bytes_in: AtomicU64,
    pub bytes_out: AtomicU64,
    pub batches: AtomicU64,
    pub files: AtomicU32,
    phase: AtomicU8,
    started: Instant,
    /// Ring of (elapsed_ms, rows, bytes_out) samples for the 30 s window rate / ETA.
    samples: Mutex<std::collections::VecDeque<(u64, u64, u64)>>,
    /// Expected total rows (from the plan), for ETA.
    pub expected_rows: AtomicU64,
}

impl Default for Progress {
    fn default() -> Self {
        Self::new()
    }
}

impl Progress {
    pub fn new() -> Self {
        Progress {
            rows: AtomicU64::new(0),
            bytes_in: AtomicU64::new(0),
            bytes_out: AtomicU64::new(0),
            batches: AtomicU64::new(0),
            files: AtomicU32::new(0),
            phase: AtomicU8::new(JobPhase::Preparing as u8),
            started: Instant::now(),
            samples: Mutex::new(std::collections::VecDeque::with_capacity(256)),
            expected_rows: AtomicU64::new(0),
        }
    }

    pub fn elapsed_ms(&self) -> u64 {
        self.started.elapsed().as_millis() as u64
    }

    pub fn set_phase(&self, phase: JobPhase) {
        self.phase.store(phase as u8, Ordering::Relaxed);
    }

    pub fn phase(&self) -> JobPhase {
        match self.phase.load(Ordering::Relaxed) {
            0 => JobPhase::Preparing,
            1 => JobPhase::Transferring,
            2 => JobPhase::NetworkBound,
            _ => JobPhase::Finishing,
        }
    }

    pub fn add_rows(&self, n: u64) {
        if n > 0 {
            self.rows.fetch_add(n, Ordering::Relaxed);
            if self.phase() == JobPhase::Preparing {
                self.set_phase(JobPhase::Transferring);
            }
        }
    }

    /// Record a sample for rate/ETA; call from the observer at its own interval.
    pub fn sample(&self) {
        let now = self.elapsed_ms();
        let rows = self.rows.load(Ordering::Relaxed);
        let bytes = self.bytes_out.load(Ordering::Relaxed);
        let mut s = self.samples.lock().unwrap();
        s.push_back((now, rows, bytes));
        while let Some(front) = s.front() {
            if now.saturating_sub(front.0) > 30_000 && s.len() > 2 {
                s.pop_front();
            } else {
                break;
            }
        }
    }

    /// Snapshot for IPC. Rate is computed over the last ~30 s of samples.
    ///
    /// Counters are not guaranteed to be monotonic: the Parquet sink publishes an *estimate* of
    /// its output size that can shrink once a row group is encoded and compressed. A window
    /// whose newest sample is below its oldest therefore reports a rate of 0 for that counter
    /// (saturating subtraction) instead of underflowing, which would panic in debug builds and
    /// report an absurd ~1.8e19 B/s in release builds.
    pub fn snapshot(&self, job_id: &str) -> JobProgress {
        let elapsed = self.elapsed_ms();
        let rows = self.rows.load(Ordering::Relaxed);
        let bytes_out = self.bytes_out.load(Ordering::Relaxed);
        let (rate_rows, rate_bytes) = {
            let s = self.samples.lock().unwrap();
            match (s.front(), s.back()) {
                (Some(a), Some(b)) if b.0 > a.0 => {
                    let dt = (b.0 - a.0) as f64 / 1000.0;
                    (b.1.saturating_sub(a.1) as f64 / dt, b.2.saturating_sub(a.2) as f64 / dt)
                }
                _ => {
                    let dt = (elapsed.max(1)) as f64 / 1000.0;
                    (rows as f64 / dt, bytes_out as f64 / dt)
                }
            }
        };
        let expected = self.expected_rows.load(Ordering::Relaxed);
        let eta_sec =
            if expected > rows && rate_rows > 0.0 && self.phase() != JobPhase::Preparing { Some(((expected - rows) as f64 / rate_rows).round() as u64) } else { None };
        JobProgress {
            job_id: job_id.to_string(),
            rows,
            bytes_in: self.bytes_in.load(Ordering::Relaxed),
            bytes_out,
            batches: self.batches.load(Ordering::Relaxed),
            files: self.files.load(Ordering::Relaxed),
            elapsed_ms: elapsed,
            rate_rows_per_sec: rate_rows,
            rate_bytes_per_sec: rate_bytes,
            eta_sec,
            phase: self.phase(),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Pipeline
// ---------------------------------------------------------------------------------------------

/// Run one extraction: drain `stream` into `sink` through a bounded channel.
///
/// * A tokio task (the *reader*) polls the source stream and sends every item into a
///   `mpsc::channel(CHANNEL_DEPTH)`. At most `CHANNEL_DEPTH` batches are queued, plus the one
///   the reader just pulled and the one the writer is currently writing.
/// * A blocking thread (the *writer*) drains the channel into the sink and bumps `progress`.
/// * Cancellation: as soon as `cancel` fires the reader stops forwarding batches and polls the
///   source one last time (at most [`CANCEL_GRACE`]): drivers send their server-side cancel from
///   inside the stream when they see the token, and a stream that is never polled again would
///   leave the query running on the server. The writer notices (either the token or the closed
///   channel), aborts the sink and returns [`CoreError::Cancelled`].
/// * Empty results: when `stream.schema` is known and the stream ends without a single batch,
///   the writer gives the sink one zero-row batch of that schema, so CSV gets its header and
///   Parquet a valid zero-row file instead of nothing.
/// * A stream error is forwarded through the channel; the writer aborts the sink and returns it.
/// * A sink error makes the writer drop the receiver; the reader sees the closed channel and
///   exits, which drops the source stream (and with it the driver's server-side session).
///
/// Returns the sink report on success. On cancellation returns `CoreError::Cancelled` after
/// aborting the sink (files renamed to `.partial`); on any other error the sink is aborted too.
pub async fn run_pipeline(stream: SourceStream, sink: Box<dyn Sink>, progress: Arc<Progress>, cancel: CancellationToken) -> Result<SinkReport> {
    let (tx, rx) = mpsc::channel::<Result<Batch>>(CHANNEL_DEPTH);
    // Fired only when the reader dies without saying why (a panic inside the stream). The
    // writer must not finish the sink in that case: the files would look complete but are not.
    let abnormal = CancellationToken::new();

    let SourceStream { schema, stream } = stream;
    let reader = tokio::spawn(reader_task(stream, tx, cancel.clone(), abnormal.clone()));
    let writer = tokio::task::spawn_blocking({
        let progress = progress.clone();
        let cancel = cancel.clone();
        let abnormal = abnormal.clone();
        move || writer_loop(rx, sink, schema, &progress, &cancel, &abnormal)
    });

    let outcome = match writer.await {
        Ok(r) => r,
        Err(join) => Err(CoreError::from(join)),
    };
    // The writer is done, so its receiver is gone: the reader exits on `tx.closed()` even when
    // the source is still waiting for the server. Awaiting it guarantees the source stream has
    // been dropped (server-side cancel) before the caller sees the outcome.
    let _ = reader.await;
    outcome
}

/// Why the reader task stopped. Diagnostics only: the writer learns everything it needs from
/// the channel and the cancellation token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReaderExit {
    /// The stream returned `None`.
    Eof,
    /// `cancel` fired.
    Cancelled,
    /// A stream error was forwarded to the writer.
    ErrorSent,
    /// The writer dropped its receiver (sink failure).
    WriterGone,
}

/// Cancels `abnormal` unless the reader returned through one of its explicit exits.
struct ReaderGuard {
    abnormal: CancellationToken,
    clean: bool,
}

impl Drop for ReaderGuard {
    fn drop(&mut self) {
        if !self.clean {
            self.abnormal.cancel();
        }
    }
}

async fn reader_task(
    mut source: BoxStream<'static, Result<Batch>>,
    tx: mpsc::Sender<Result<Batch>>,
    cancel: CancellationToken,
    abnormal: CancellationToken,
) -> ReaderExit {
    let mut guard = ReaderGuard { abnormal, clean: false };
    let exit = loop {
        let item = tokio::select! {
            biased;
            _ = cancel.cancelled() => break ReaderExit::Cancelled,
            _ = tx.closed() => break ReaderExit::WriterGone,
            item = source.next() => item,
        };
        let Some(item) = item else { break ReaderExit::Eof };
        let is_err = item.is_err();
        let sent = tokio::select! {
            biased;
            _ = cancel.cancelled() => break ReaderExit::Cancelled,
            r = tx.send(item) => r.is_ok(),
        };
        if !sent {
            break ReaderExit::WriterGone;
        }
        if is_err {
            break ReaderExit::ErrorSent;
        }
    };
    if exit == ReaderExit::Cancelled {
        cancel_grace_poll(&mut source).await;
    }
    guard.clean = true;
    drop(source);
    tracing::debug!(?exit, "pipeline reader finished");
    exit
}

/// Give a cancelled source one bounded poll so its driver sees the token and cancels the query
/// on the server before the stream (and the connection) is dropped. Whatever the poll yields is
/// discarded: the writer is already on its way out with [`CoreError::Cancelled`].
///
/// Only called when the stream has not ended (`ReaderExit::Cancelled` is never reached after a
/// `None`), so polling it again is allowed.
async fn cancel_grace_poll(source: &mut BoxStream<'static, Result<Batch>>) {
    match tokio::time::timeout(CANCEL_GRACE, source.next()).await {
        Ok(Some(Err(e))) => tracing::debug!(error = %e, "source acknowledged cancel"),
        Ok(Some(Ok(_))) => tracing::debug!("source returned data after cancel; discarded"),
        Ok(None) => tracing::debug!("source ended after cancel"),
        Err(_) => tracing::warn!(grace_ms = CANCEL_GRACE.as_millis() as u64, "source did not react to cancel in time; dropping it"),
    }
}

/// The writer side: runs on a blocking thread, owns the sink.
///
/// `schema` is the schema the source announced up front; it is only used when the stream ends
/// without a single batch (see [`run_pipeline`]).
fn writer_loop(
    mut rx: mpsc::Receiver<Result<Batch>>,
    mut sink: Box<dyn Sink>,
    schema: Option<SchemaRef>,
    progress: &Progress,
    cancel: &CancellationToken,
    abnormal: &CancellationToken,
) -> Result<SinkReport> {
    let mut wrote_any = false;
    loop {
        if cancel.is_cancelled() {
            sink.abort();
            return Err(CoreError::Cancelled);
        }
        match rx.blocking_recv() {
            Some(Ok(batch)) => {
                let rows = batch.rows();
                let bytes = batch.bytes();
                if let Err(e) = sink.write(batch) {
                    tracing::warn!(error = %e, "sink write failed; aborting");
                    sink.abort();
                    return Err(e);
                }
                wrote_any = true;
                progress.add_rows(rows);
                progress.bytes_in.fetch_add(bytes, Ordering::Relaxed);
                progress.batches.fetch_add(1, Ordering::Relaxed);
                progress.bytes_out.store(sink.bytes_out(), Ordering::Relaxed);
                progress.files.store(sink.files_so_far(), Ordering::Relaxed);
            }
            Some(Err(e)) => {
                tracing::warn!(error = %e, "source stream failed; aborting");
                sink.abort();
                return Err(e);
            }
            None => {
                // The reader is gone. Either the stream ended, or it stopped for a reason the
                // tokens tell us about.
                if cancel.is_cancelled() {
                    sink.abort();
                    return Err(CoreError::Cancelled);
                }
                if abnormal.is_cancelled() {
                    sink.abort();
                    return Err(CoreError::Internal("source stream ended abnormally".into()));
                }
                if !wrote_any {
                    if let Some(schema) = schema {
                        // Empty result: let the sink open its file from the schema (CSV
                        // header, Parquet schema) so the output is a valid empty file.
                        if let Err(e) = sink.write(Batch::Arrow(RecordBatch::new_empty(schema))) {
                            tracing::warn!(error = %e, "sink rejected the empty-result schema batch; aborting");
                            sink.abort();
                            return Err(e);
                        }
                        progress.bytes_out.store(sink.bytes_out(), Ordering::Relaxed);
                        progress.files.store(sink.files_so_far(), Ordering::Relaxed);
                    }
                }
                progress.set_phase(JobPhase::Finishing);
                let report = sink.finish()?;
                progress.bytes_out.store(report.bytes, Ordering::Relaxed);
                progress.files.store(report.files.len() as u32, Ordering::Relaxed);
                return Ok(report);
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------------------------

/// One job known to the registry. Everything observable by the UI lives here.
pub struct JobEntry {
    pub id: String,
    pub kind: JobKind,
    pub conn_id: String,
    pub conn_name: String,
    pub label: String,
    pub started_at: String,
    pub finished_at: Mutex<Option<String>>,
    pub state: Mutex<JobState>,
    pub progress: Arc<Progress>,
    pub cancel: CancellationToken,
    pub request: Option<ExportRequest>,
    pub preflight: Mutex<Option<PreflightReport>>,
    pub server: Mutex<Option<JobServer>>,
    pub result: Mutex<Option<JobResult>>,
    pub error: Mutex<Option<ErrorPayload>>,
    pub exact_count: Mutex<Option<ExactCount>>,
    /// Resolved by `job_resume` when the job waits in `Warned`.
    pub resume: Mutex<Option<tokio::sync::oneshot::Sender<Vec<String>>>>,
}

impl JobEntry {
    pub fn summary(&self) -> JobSummary {
        let state = *self.state.lock().unwrap();
        JobSummary {
            id: self.id.clone(),
            kind: self.kind,
            state,
            conn_id: self.conn_id.clone(),
            conn_name: self.conn_name.clone(),
            label: self.label.clone(),
            started_at: self.started_at.clone(),
            finished_at: self.finished_at.lock().unwrap().clone(),
            progress: if state.is_active() || state.is_terminal() { Some(self.progress.snapshot(&self.id)) } else { None },
            server: self.server.lock().unwrap().clone(),
            request: self.request.clone(),
            preflight: if state == JobState::Warned { self.preflight.lock().unwrap().clone() } else { None },
            result: self.result.lock().unwrap().clone(),
            error: self.error.lock().unwrap().clone(),
            exact_count: self.exact_count.lock().unwrap().clone(),
        }
    }
}

/// Jobs by id plus their creation order. Kept behind one lock so no method can take two locks
/// in different orders (an earlier two-lock layout deadlocked `prune` against `list`).
#[derive(Default)]
struct JobTable {
    jobs: HashMap<String, Arc<JobEntry>>,
    /// Ids, oldest first.
    order: Vec<String>,
}

/// Registry of all jobs of this process plus the event bus.
pub struct JobRegistry {
    table: RwLock<JobTable>,
    events: broadcast::Sender<CoreEvent>,
    /// Limits concurrent *running* export jobs (settings.max_concurrent_jobs).
    pub export_slots: Arc<tokio::sync::Semaphore>,
}

impl JobRegistry {
    pub fn new(events: broadcast::Sender<CoreEvent>, max_concurrent: usize) -> Self {
        JobRegistry { table: RwLock::new(JobTable::default()), events, export_slots: Arc::new(tokio::sync::Semaphore::new(max_concurrent.clamp(1, 3))) }
    }

    pub fn events(&self) -> &broadcast::Sender<CoreEvent> {
        &self.events
    }

    pub fn create(&self, kind: JobKind, conn_id: &str, conn_name: &str, label: &str, request: Option<ExportRequest>) -> Arc<JobEntry> {
        let id = uuid::Uuid::new_v4().simple().to_string()[..12].to_string();
        let entry = Arc::new(JobEntry {
            id: id.clone(),
            kind,
            conn_id: conn_id.to_string(),
            conn_name: conn_name.to_string(),
            label: label.to_string(),
            started_at: crate::types::now_rfc3339(),
            finished_at: Mutex::new(None),
            state: Mutex::new(JobState::Pending),
            progress: Arc::new(Progress::new()),
            cancel: CancellationToken::new(),
            request,
            preflight: Mutex::new(None),
            server: Mutex::new(None),
            result: Mutex::new(None),
            error: Mutex::new(None),
            exact_count: Mutex::new(None),
            resume: Mutex::new(None),
        });
        {
            let mut table = self.table.write().unwrap();
            table.jobs.insert(id.clone(), entry.clone());
            table.order.push(id);
        }
        self.emit_state(&entry);
        entry
    }

    pub fn get(&self, id: &str) -> Option<Arc<JobEntry>> {
        self.table.read().unwrap().jobs.get(id).cloned()
    }

    /// All jobs, oldest first.
    pub fn list(&self) -> Vec<Arc<JobEntry>> {
        let table = self.table.read().unwrap();
        table.order.iter().filter_map(|id| table.jobs.get(id).cloned()).collect()
    }

    pub fn active(&self) -> Vec<Arc<JobEntry>> {
        self.list().into_iter().filter(|j| j.state.lock().unwrap().is_active()).collect()
    }

    /// Transition with validation; emits `job:state`. Returns the previous state.
    pub fn transition(&self, job: &JobEntry, to: JobState) -> Result<JobState> {
        let mut st = job.state.lock().unwrap();
        let from = *st;
        if !valid_transition(from, to) {
            return Err(CoreError::Invalid(format!("job {} cannot go from {from:?} to {to:?}", job.id)));
        }
        *st = to;
        drop(st);
        if to.is_terminal() {
            *job.finished_at.lock().unwrap() = Some(crate::types::now_rfc3339());
            job.progress.set_phase(JobPhase::Finishing);
        }
        self.emit_state(job);
        Ok(from)
    }

    fn emit_state(&self, job: &JobEntry) {
        let state = *job.state.lock().unwrap();
        let _ = self.events.send(CoreEvent::JobState(JobStateEvent { job_id: job.id.clone(), state, at: crate::types::now_rfc3339() }));
    }

    /// Finish a job: store the outcome, transition and emit `job:done`.
    pub fn finish(&self, job: &JobEntry, outcome: std::result::Result<JobResult, CoreError>) {
        let state = match &outcome {
            Ok(_) => JobState::Done,
            Err(CoreError::Cancelled) => JobState::Cancelled,
            Err(_) => JobState::Failed,
        };
        match outcome {
            Ok(r) => *job.result.lock().unwrap() = Some(r),
            Err(e) => *job.error.lock().unwrap() = Some(e.to_payload()),
        }
        let _ = self.transition(job, state);
        let _ = self.events.send(CoreEvent::JobDone(JobDone {
            job_id: job.id.clone(),
            state,
            result: job.result.lock().unwrap().clone(),
            error: job.error.lock().unwrap().clone(),
            exact_count: job.exact_count.lock().unwrap().clone(),
        }));
    }

    /// Drop terminal jobs older than `keep` entries (keeps the monitor list short).
    ///
    /// Takes only the table lock (plus each job's own state mutex, which no code holds while
    /// waiting for the table lock).
    pub fn prune(&self, keep: usize) {
        let mut guard = self.table.write().unwrap();
        let table = &mut *guard;
        let terminal: Vec<String> = table.order.iter().filter(|id| table.jobs.get(*id).map(|j| j.state.lock().unwrap().is_terminal()).unwrap_or(true)).cloned().collect();
        if terminal.len() > keep {
            let drop_ids: HashSet<&String> = terminal[..terminal.len() - keep].iter().collect();
            table.jobs.retain(|id, _| !drop_ids.contains(id));
            table.order.retain(|id| !drop_ids.contains(id));
        }
    }
}

/// The nine-state machine's allowed edges.
pub fn valid_transition(from: JobState, to: JobState) -> bool {
    use JobState::*;
    matches!(
        (from, to),
        (Pending, Preflight)
            | (Pending, Running)
            | (Pending, Rejected)
            | (Pending, Cancelled)
            | (Pending, Failed)
            | (Preflight, Warned)
            | (Preflight, Rejected)
            | (Preflight, Running)
            | (Preflight, Cancelled)
            | (Preflight, Failed)
            | (Warned, Running)
            | (Warned, Cancelled)
            | (Warned, Rejected)
            | (Running, Finishing)
            | (Running, Cancelled)
            | (Running, Failed)
            | (Running, Done)
            | (Finishing, Done)
            | (Finishing, Failed)
            | (Finishing, Cancelled)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_machine_edges() {
        assert!(valid_transition(JobState::Pending, JobState::Preflight));
        assert!(valid_transition(JobState::Warned, JobState::Running));
        assert!(!valid_transition(JobState::Done, JobState::Running));
        assert!(!valid_transition(JobState::Rejected, JobState::Running));
        assert!(JobState::Rejected.is_terminal());
        assert!(JobState::Warned.is_active());
    }

    fn registry() -> JobRegistry {
        JobRegistry::new(broadcast::channel(1024).0, 2)
    }

    #[test]
    fn prune_drops_oldest_terminal_jobs_and_keeps_order() {
        let reg = registry();
        let ids: Vec<String> = (0..5).map(|i| reg.create(JobKind::Export, "c", "conn", &format!("job {i}"), None).id.clone()).collect();
        // Jobs 0, 1, 3 finish; 2 and 4 stay active.
        for i in [0, 1, 3] {
            reg.transition(&reg.get(&ids[i]).unwrap(), JobState::Cancelled).unwrap();
        }
        reg.prune(1);
        let left: Vec<String> = reg.list().iter().map(|j| j.id.clone()).collect();
        assert_eq!(left, vec![ids[2].clone(), ids[3].clone(), ids[4].clone()], "oldest terminal jobs pruned, order kept");
        assert!(reg.get(&ids[0]).is_none());
        assert_eq!(reg.active().len(), 2);
    }

    /// `prune` (export start) racing `list`/`active` (the 200 ms progress tick) must never
    /// deadlock: both now take the single table lock.
    #[test]
    fn prune_and_list_do_not_deadlock() {
        let reg = Arc::new(registry());
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let mut threads = Vec::new();
        for t in 0..4 {
            let reg = reg.clone();
            let done_tx = done_tx.clone();
            threads.push(std::thread::spawn(move || {
                for i in 0..2_000 {
                    if t % 2 == 0 {
                        let job = reg.create(JobKind::Export, "c", "conn", "x", None);
                        if i % 2 == 0 {
                            let _ = reg.transition(&job, JobState::Cancelled);
                        }
                        reg.prune(10);
                    } else {
                        let _ = reg.active();
                        let _ = reg.list();
                    }
                }
                done_tx.send(()).unwrap();
            }));
        }
        for _ in 0..4 {
            done_rx.recv_timeout(std::time::Duration::from_secs(60)).expect("registry deadlocked");
        }
        for t in threads {
            t.join().unwrap();
        }
    }

    #[test]
    fn progress_phase_flips_on_first_rows() {
        let p = Progress::new();
        assert_eq!(p.phase(), JobPhase::Preparing);
        p.add_rows(10);
        assert_eq!(p.phase(), JobPhase::Transferring);
        let s = p.snapshot("j");
        assert_eq!(s.rows, 10);
    }

    /// A counter that goes down inside the rate window (the Parquet sink's byte estimate shrinks
    /// after compression) must give a zero rate for that counter, not an underflow panic (debug)
    /// or a ~1.8e19 B/s rate (release). The other counter's rate is unaffected.
    #[test]
    fn progress_rate_survives_a_decreasing_counter() {
        let p = Progress::new();
        p.add_rows(1_000);
        {
            let mut s = p.samples.lock().unwrap();
            s.push_back((0, 0, 4_000_000));
            s.push_back((2_000, 1_000, 1_500_000));
        }
        let snap = p.snapshot("j");
        assert_eq!(snap.rate_bytes_per_sec, 0.0, "a shrinking byte estimate is no throughput");
        assert!((snap.rate_rows_per_sec - 500.0).abs() < 1e-9, "rows/s over 2 s: {}", snap.rate_rows_per_sec);

        // Rows going backwards (never expected, but the counters are public) is clamped too.
        p.expected_rows.store(10_000, Ordering::Relaxed);
        {
            let mut s = p.samples.lock().unwrap();
            s.clear();
            s.push_back((0, 5_000, 0));
            s.push_back((1_000, 4_000, 2_048));
        }
        let snap = p.snapshot("j");
        assert_eq!(snap.rate_rows_per_sec, 0.0);
        assert!((snap.rate_bytes_per_sec - 2_048.0).abs() < 1e-9, "bytes/s over 1 s: {}", snap.rate_bytes_per_sec);
        assert!(snap.eta_sec.is_none(), "no ETA without a positive row rate");

        // Through the public API: `sample` after the byte estimate was revised downwards.
        let p = Progress::new();
        p.bytes_out.store(10_000, Ordering::Relaxed);
        p.sample();
        std::thread::sleep(std::time::Duration::from_millis(5));
        p.bytes_out.store(1, Ordering::Relaxed);
        p.sample();
        let snap = p.snapshot("j");
        assert!(snap.rate_bytes_per_sec.is_finite() && snap.rate_bytes_per_sec >= 0.0, "{}", snap.rate_bytes_per_sec);
    }
}
