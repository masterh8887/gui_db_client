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
//!    `tunnel:hostkey`, `tunnel:password`, `drivers:changed`. Every one goes to every window
//!    except the tunnel prompts, which go to the focused window only and are kept by the window
//!    registry until `prompt:resolved` (re-sent when their window closes first).
//! 4. `job:log` — every `LogEvent` from the core ring buffer (tracing + server notices).
//! 5. `copy:progress` — every 500 ms `copy_progress_snapshot()` is polled; per copy job a
//!    `CopyProgress` delta (`full: false`, the table entries whose `seq` changed, at most 64,
//!    active first) is emitted when the job's `seq` moved, and at least once a second (then
//!    with its active tables; docs/db-copy.md 6.2).
//! 6. `config_poll` — every 2 s `BulkDb::config_poll()` picks up connections and settings
//!    saved by another BulkDB process; core emits `connections:changed` / `settings:changed`.
//!
//! Core events also map `connections:changed` (null; windows whose connection was renamed are
//! retitled), `settings:changed` (`Settings`) and `prompt:resolved` (`{ promptId }`), all
//! broadcast to every window.
//!
//! Intervals are re-read from settings on every tick so `settings_set` takes effect without a
//! restart. Nothing here carries data rows: only counters, states and log lines.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bulk_core::types::{CopyItemState, CopyProgress, CopyTableProgress, CoreEvent, JobProgress, JobServer};
use bulk_core::BulkDb;
use serde::Serialize;
use tauri::{AppHandle, Emitter};
use tokio::sync::broadcast::error::RecvError;

use crate::windows;

/// Lower bound for the progress tick so a mis-set setting cannot spin the loop.
const MIN_PROGRESS_MS: u64 = 50;
/// Lower bound for the server-snapshot tick.
const MIN_SERVER_MS: u64 = 500;
/// Copy progress poll interval (docs/db-copy.md 6.2).
const COPY_PROGRESS_MS: u64 = 500;
/// Most table entries one `copy:progress` delta carries.
const COPY_DELTA_MAX_TABLES: usize = 64;
/// A copy job's progress is re-emitted at least this often.
const COPY_PROGRESS_KEEPALIVE: Duration = Duration::from_secs(1);
/// How often another process's config writes are picked up (docs/db-copy.md 1.2).
const CONFIG_POLL: Duration = Duration::from_secs(2);

/// Spawn every emitter task. Tasks live as long as the process; they hold the facade alive.
pub fn spawn(app: AppHandle, db: Arc<BulkDb>) {
    tauri::async_runtime::spawn(progress_loop(app.clone(), db.clone()));
    tauri::async_runtime::spawn(server_loop(app.clone(), db.clone()));
    tauri::async_runtime::spawn(core_event_loop(app.clone(), db.clone()));
    tauri::async_runtime::spawn(copy_progress_loop(app.clone(), db.clone()));
    tauri::async_runtime::spawn(config_poll_loop(db.clone()));
    tauri::async_runtime::spawn(log_loop(app, db));
}

fn copy_item_active(s: CopyItemState) -> bool {
    matches!(s, CopyItemState::Creating | CopyItemState::Loading | CopyItemState::Indexing | CopyItemState::Verifying)
}

/// The `copy:progress` delta from the last emitted snapshot `prev` to `next`: `None` when the
/// job's `seq` did not move; else the table entries that are new or whose `seq` changed
/// (active ones first, at most [`COPY_DELTA_MAX_TABLES`]) with `full: false`.
pub fn copy_progress_delta(prev: Option<&CopyProgress>, next: &CopyProgress) -> Option<CopyProgress> {
    if prev.is_some_and(|p| p.seq == next.seq) {
        return None;
    }
    let changed = |t: &CopyTableProgress| prev.and_then(|p| p.tables.iter().find(|o| o.item_id == t.item_id)).is_none_or(|o| o.seq != t.seq);
    let mut tables: Vec<CopyTableProgress> = next.tables.iter().filter(|t| changed(t)).cloned().collect();
    tables.sort_by_key(|t| !copy_item_active(t.state));
    tables.truncate(COPY_DELTA_MAX_TABLES);
    Some(CopyProgress { tables, full: false, ..next.clone() })
}

/// The once-a-second re-emission of a job whose `seq` did not move: the progress header and
/// its active tables (at most 64), `full: false`. The UI ignores entries whose `seq` is not
/// newer, so this only keeps a window that missed a delta from going stale.
pub fn copy_progress_keepalive(next: &CopyProgress) -> CopyProgress {
    let mut tables: Vec<CopyTableProgress> = next.tables.iter().filter(|t| copy_item_active(t.state)).cloned().collect();
    tables.truncate(COPY_DELTA_MAX_TABLES);
    CopyProgress { tables, full: false, ..next.clone() }
}

async fn copy_progress_loop(app: AppHandle, db: Arc<BulkDb>) {
    // Per job: the last snapshot seen and when the last event went out.
    let mut last: HashMap<String, (CopyProgress, Instant)> = HashMap::new();
    loop {
        tokio::time::sleep(Duration::from_millis(COPY_PROGRESS_MS)).await;
        let snapshots = db.copy_progress_snapshot();
        let mut seen: Vec<String> = Vec::with_capacity(snapshots.len());
        let now = Instant::now();
        for p in snapshots {
            seen.push(p.job_id.clone());
            let prev = last.get(&p.job_id);
            let delta = copy_progress_delta(prev.map(|(prev, _)| prev), &p)
                .or_else(|| prev.filter(|(_, at)| now.duration_since(*at) >= COPY_PROGRESS_KEEPALIVE).map(|_| copy_progress_keepalive(&p)));
            match delta {
                Some(delta) => {
                    emit(&app, "copy:progress", delta);
                    last.insert(p.job_id.clone(), (p, now));
                }
                None => {
                    if let Some(entry) = last.get_mut(&p.job_id) {
                        entry.0 = p;
                    }
                }
            }
        }
        last.retain(|id, _| seen.contains(id));
    }
}

/// Every 2 s: connections and settings another BulkDB process saved (blocking file I/O on the
/// blocking pool). An error (a hand-edited file that does not parse) is logged once until it
/// changes.
async fn config_poll_loop(db: Arc<BulkDb>) {
    let mut last_error: Option<String> = None;
    loop {
        tokio::time::sleep(CONFIG_POLL).await;
        let db = db.clone();
        match tauri::async_runtime::spawn_blocking(move || db.config_poll()).await {
            Ok(Ok(())) => last_error = None,
            Ok(Err(e)) => {
                let text = e.to_string();
                if last_error.as_deref() != Some(text.as_str()) {
                    tracing::warn!("config poll: {text}");
                    last_error = Some(text);
                }
            }
            Err(e) => tracing::debug!("config poll task failed: {e}"),
        }
    }
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
        CoreEvent::ConnectionsChanged => "connections:changed",
        CoreEvent::SettingsChanged(_) => "settings:changed",
        CoreEvent::PromptResolved(_) => "prompt:resolved",
    }
}

fn forward(app: &AppHandle, db: &BulkDb, ev: CoreEvent) {
    let name = core_event_name(&ev);
    match ev {
        CoreEvent::JobState(p) => emit(app, name, p),
        CoreEvent::JobDone(p) => {
            windows::job_done(app, &p.job_id);
            emit(app, name, p)
        }
        CoreEvent::JobLog(p) => emit(app, name, p),
        // Prompts go to one window (the focused one) and are kept until resolved.
        CoreEvent::TunnelHostKey(p) => windows::route_prompt(app, windows::PromptPayload::HostKey(p)),
        CoreEvent::TunnelPassword(p) => windows::route_prompt(app, windows::PromptPayload::Password(p)),
        CoreEvent::DriversChanged(p) => emit(app, name, p),
        CoreEvent::ConnectionsChanged => {
            emit(app, name, ());
            windows::connections_changed(app, db);
        }
        CoreEvent::SettingsChanged(p) => emit(app, name, p),
        CoreEvent::PromptResolved(p) => {
            windows::prompt_resolved(app, &p.prompt_id);
            emit(app, name, p)
        }
    }
}

async fn core_event_loop(app: AppHandle, db: Arc<BulkDb>) {
    let mut rx = db.subscribe();
    loop {
        match rx.recv().await {
            Ok(ev) => forward(&app, &db, ev),
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
        assert_eq!(core_event_name(&CoreEvent::ConnectionsChanged), "connections:changed");
        let resolved = CoreEvent::PromptResolved(bulk_core::types::PromptResolved { prompt_id: "p".into() });
        assert_eq!(core_event_name(&resolved), "prompt:resolved");
    }

    fn table(item: &str, seq: u64, state: CopyItemState) -> CopyTableProgress {
        CopyTableProgress {
            seq,
            item_id: item.into(),
            state,
            rows_read: 0,
            rows_written: 0,
            bytes: 0,
            est_rows: None,
            rate_rows_per_sec: 0.0,
            elapsed_ms: None,
            method: None,
            error: None,
        }
    }

    fn copy_progress(seq: u64, tables: Vec<CopyTableProgress>) -> CopyProgress {
        CopyProgress {
            job_id: "j".into(),
            run_id: "j".into(),
            seq,
            phase: bulk_core::types::CopyPhase::Data,
            items_total: 2,
            items_done: 0,
            items_failed: 0,
            items_skipped: 0,
            tables_total: 2,
            tables_done: 0,
            tables,
            full: true,
        }
    }

    #[test]
    fn copy_progress_delta_carries_only_changed_tables() {
        let a = copy_progress(1, vec![table("t:a", 1, CopyItemState::Pending), table("t:b", 1, CopyItemState::Pending)]);
        let first = copy_progress_delta(None, &a).expect("the first snapshot is emitted");
        assert!(!first.full);
        assert_eq!(first.tables.len(), 2);
        assert!(copy_progress_delta(Some(&a), &a).is_none(), "nothing moved");
        let b = copy_progress(2, vec![table("t:a", 1, CopyItemState::Pending), table("t:b", 2, CopyItemState::Loading)]);
        let delta = copy_progress_delta(Some(&a), &b).expect("seq moved");
        assert_eq!(delta.tables.iter().map(|t| t.item_id.as_str()).collect::<Vec<_>>(), ["t:b"]);
    }

    /// Once a second without a change: the header and the active tables only.
    #[test]
    fn copy_progress_keepalive_carries_the_active_tables() {
        let p = copy_progress(7, vec![table("t:a", 3, CopyItemState::Done), table("t:b", 5, CopyItemState::Loading)]);
        let k = copy_progress_keepalive(&p);
        assert_eq!((k.seq, k.full), (7, false));
        assert_eq!(k.tables.iter().map(|t| t.item_id.as_str()).collect::<Vec<_>>(), ["t:b"]);
    }
}
