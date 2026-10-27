//! Event emitters (core -> webview). Four background tasks are spawned in `setup`:
//!
//! 1. `job:progress` — every `settings.progress_interval_ms` (200 ms) the active jobs' progress
//!    counters are polled; a `JobProgress` is emitted whenever anything the job card renders
//!    moved since the last emission for that job: the counters, the phase, the rates, the ETA, or
//!    the elapsed time at whole-second resolution. The elapsed second alone guarantees at least
//!    one event per second per active job, so the timer keeps running and a stale MB/s / ETA is
//!    replaced while the server sorts before the first row (`preparing`) or the transfer stalls;
//!    that is the same cadence the CLI prints at.
//! 2. `job:server` — every `settings.monitor_interval_ms` (3 s) each `JobServer` snapshot is
//!    emitted when it differs from the last emitted one.
//! 3. core events — `CoreEvent` is mapped 1:1 to `job:state`, `job:done`, `job:log`,
//!    `tunnel:hostkey`, `tunnel:password`, `drivers:changed`.
//! 4. `job:log` — every `LogEvent` from the core ring buffer (tracing + server notices).
//!
//! Intervals are re-read from settings on every tick so `settings_set` takes effect without a
//! restart. Nothing here carries data rows: only counters, states and log lines.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use bulk_core::types::{CoreEvent, JobProgress, JobServer};
use bulk_core::BulkDb;
use serde::Serialize;
use tauri::{AppHandle, Emitter};
use tokio::sync::broadcast::error::RecvError;

/// Lower bound for the progress tick so a mis-set setting cannot spin the loop.
const MIN_PROGRESS_MS: u64 = 50;
/// Lower bound for the server-snapshot tick.
const MIN_SERVER_MS: u64 = 500;

/// Spawn every emitter task. Tasks live as long as the process; they hold the facade alive.
pub fn spawn(app: AppHandle, db: Arc<BulkDb>) {
    tauri::async_runtime::spawn(progress_loop(app.clone(), db.clone()));
    tauri::async_runtime::spawn(server_loop(app.clone(), db.clone()));
    tauri::async_runtime::spawn(core_event_loop(app.clone(), db.clone()));
    tauri::async_runtime::spawn(log_loop(app, db));
}

/// `true` when anything the job card renders differs between two snapshots of the same job.
///
/// Counted as a change: rows, bytes in/out, batches, files, phase, ETA, either rate, and the
/// elapsed time when it crosses a whole second (the card shows seconds, so sub-second movement
/// would only cost events without changing the display). Because the elapsed second advances
/// while a job waits on the server or stalls, an active job is re-emitted at least once a second
/// even when no counter moves; otherwise the UI would freeze its timer and keep the last
/// non-zero rate and ETA exactly when the user is deciding whether to cancel.
pub fn progress_changed(prev: &JobProgress, next: &JobProgress) -> bool {
    prev.rows != next.rows
        || prev.bytes_in != next.bytes_in
        || prev.bytes_out != next.bytes_out
        || prev.batches != next.batches
        || prev.files != next.files
        || prev.phase != next.phase
        || prev.eta_sec != next.eta_sec
        || prev.rate_rows_per_sec != next.rate_rows_per_sec
        || prev.rate_bytes_per_sec != next.rate_bytes_per_sec
        || prev.elapsed_ms / 1000 != next.elapsed_ms / 1000
}

fn emit<S: Serialize + Clone>(app: &AppHandle, name: &str, payload: S) {
    if let Err(e) = app.emit(name, payload) {
        tracing::debug!(event = name, error = %e, "event emit failed (window gone?)");
    }
}

async fn progress_loop(app: AppHandle, db: Arc<BulkDb>) {
    let mut last: HashMap<String, JobProgress> = HashMap::new();
    loop {
        let ms = u64::from(db.settings_get().progress_interval_ms).max(MIN_PROGRESS_MS);
        tokio::time::sleep(Duration::from_millis(ms)).await;
        let snapshots = db.progress_snapshot();
        let mut seen: Vec<&str> = Vec::with_capacity(snapshots.len());
        let mut emitted: Vec<JobProgress> = Vec::new();
        for p in &snapshots {
            seen.push(p.job_id.as_str());
            let changed = last.get(&p.job_id).is_none_or(|prev| progress_changed(prev, p));
            if changed {
                emit(&app, "job:progress", p.clone());
                emitted.push(p.clone());
            }
        }
        // Forget jobs that are no longer active so their ids can never suppress a fresh emission.
        last.retain(|id, _| seen.contains(&id.as_str()));
        for p in emitted {
            last.insert(p.job_id.clone(), p);
        }
    }
}

async fn server_loop(app: AppHandle, db: Arc<BulkDb>) {
    let mut last: HashMap<String, JobServer> = HashMap::new();
    loop {
        let ms = u64::from(db.settings_get().monitor_interval_ms).max(MIN_SERVER_MS);
        tokio::time::sleep(Duration::from_millis(ms)).await;
        let snapshots = db.server_snapshot();
        let mut seen: Vec<&str> = Vec::with_capacity(snapshots.len());
        let mut emitted: Vec<JobServer> = Vec::new();
        for s in &snapshots {
            seen.push(s.job_id.as_str());
            if last.get(&s.job_id) != Some(s) {
                emit(&app, "job:server", s.clone());
                emitted.push(s.clone());
            }
        }
        last.retain(|id, _| seen.contains(&id.as_str()));
        for s in emitted {
            last.insert(s.job_id.clone(), s);
        }
    }
}

/// Event name for a core event; the payload is the variant's inner value.
pub fn core_event_name(ev: &CoreEvent) -> &'static str {
    match ev {
        CoreEvent::JobState(_) => "job:state",
        CoreEvent::JobDone(_) => "job:done",
        CoreEvent::JobLog(_) => "job:log",
        CoreEvent::TunnelHostKey(_) => "tunnel:hostkey",
        CoreEvent::TunnelPassword(_) => "tunnel:password",
        CoreEvent::DriversChanged(_) => "drivers:changed",
    }
}

fn forward(app: &AppHandle, ev: CoreEvent) {
    let name = core_event_name(&ev);
    match ev {
        CoreEvent::JobState(p) => emit(app, name, p),
        CoreEvent::JobDone(p) => emit(app, name, p),
        CoreEvent::JobLog(p) => emit(app, name, p),
        CoreEvent::TunnelHostKey(p) => emit(app, name, p),
        CoreEvent::TunnelPassword(p) => emit(app, name, p),
        CoreEvent::DriversChanged(p) => emit(app, name, p),
    }
}

async fn core_event_loop(app: AppHandle, db: Arc<BulkDb>) {
    let mut rx = db.subscribe();
    loop {
        match rx.recv().await {
            Ok(ev) => forward(&app, ev),
            Err(RecvError::Lagged(n)) => {
                tracing::warn!(skipped = n, "core event stream lagged; the UI should re-fetch job_list");
            }
            Err(RecvError::Closed) => break,
        }
    }
}

async fn log_loop(app: AppHandle, db: Arc<BulkDb>) {
    let mut rx = db.logs.subscribe();
    loop {
        match rx.recv().await {
            Ok(ev) => emit(&app, "job:log", ev),
            Err(RecvError::Lagged(n)) => {
                tracing::debug!(skipped = n, "log event stream lagged");
            }
            Err(RecvError::Closed) => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bulk_core::types::{JobPhase, JobState, JobStateEvent};

    fn transferring() -> JobProgress {
        JobProgress {
            job_id: "j".into(),
            rows: 10,
            bytes_in: 90,
            bytes_out: 100,
            batches: 1,
            files: 1,
            elapsed_ms: 1000,
            rate_rows_per_sec: 1.0,
            rate_bytes_per_sec: 10.0,
            eta_sec: Some(5),
            phase: JobPhase::Transferring,
        }
    }

    #[test]
    fn progress_changed_detects_counter_and_phase_moves() {
        let a = transferring();
        assert!(!progress_changed(&a, &a.clone()));
        assert!(progress_changed(&a, &JobProgress { rows: 11, ..a.clone() }));
        assert!(progress_changed(&a, &JobProgress { bytes_in: 91, ..a.clone() }));
        assert!(progress_changed(&a, &JobProgress { bytes_out: 101, ..a.clone() }));
        assert!(progress_changed(&a, &JobProgress { batches: 2, ..a.clone() }));
        assert!(progress_changed(&a, &JobProgress { files: 2, ..a.clone() }));
        assert!(progress_changed(&a, &JobProgress { phase: JobPhase::Finishing, ..a.clone() }));
    }

    #[test]
    fn progress_changed_detects_rate_and_eta_moves_while_counters_stall() {
        // A stall: counters frozen while the sampled rates decay and the ETA changes or vanishes.
        let a = transferring();
        assert!(progress_changed(&a, &JobProgress { rate_bytes_per_sec: 0.0, ..a.clone() }));
        assert!(progress_changed(&a, &JobProgress { rate_rows_per_sec: 0.0, ..a.clone() }));
        assert!(progress_changed(&a, &JobProgress { eta_sec: None, ..a.clone() }));
        assert!(progress_changed(&a, &JobProgress { eta_sec: Some(6), ..a.clone() }));
    }

    #[test]
    fn progress_changed_ticks_the_elapsed_clock_once_per_second() {
        // Preparing: the server sorts before the first row, so only the clock moves.
        let a = JobProgress { job_id: "j".into(), phase: JobPhase::Preparing, elapsed_ms: 1000, ..Default::default() };
        assert!(!progress_changed(&a, &JobProgress { elapsed_ms: 1999, ..a.clone() }), "sub-second movement is not re-emitted");
        assert!(progress_changed(&a, &JobProgress { elapsed_ms: 2000, ..a.clone() }), "a new elapsed second is re-emitted");
        assert!(progress_changed(&a, &JobProgress { elapsed_ms: 125_400, ..a.clone() }));
    }

    #[test]
    fn core_event_names_match_the_frontend_contract() {
        let ev = CoreEvent::JobState(JobStateEvent { job_id: "j".into(), state: JobState::Running, at: String::new() });
        assert_eq!(core_event_name(&ev), "job:state");
        assert_eq!(core_event_name(&CoreEvent::DriversChanged(vec![])), "drivers:changed");
    }
}
