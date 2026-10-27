//! Waiting for a job: the event loop with Ctrl+C handling, the `warned` handshake and the
//! once-per-second progress line (rows, bytes in/out, MB/s, rows/s, ETA, phase, server state
//! and the process RSS, which is the Phase 1 memory gate).

use std::time::{Duration, Instant};

use anyhow::{bail, Context};
use bulk_core::error::CoreError;
use bulk_core::types::{CoreEvent, JobDone, JobState};
use bulk_core::BulkDb;
use tokio::sync::broadcast;

use crate::output::{enum_name, fmt_clock, human_bytes, human_count, human_rate, short_count};
use crate::prompts;

/// How to wait.
#[derive(Debug, Clone, Copy, Default)]
pub struct WaitOptions {
    /// Print the progress line once per second on stderr.
    pub progress: bool,
    /// Acknowledge warnings automatically when the job stops in `warned` (`--yes`).
    pub auto_ack: bool,
}

/// What happened while waiting.
#[derive(Debug)]
pub struct WaitOutcome {
    pub done: JobDone,
    pub elapsed: Duration,
    /// Highest process RSS observed while waiting (sampled every 250 ms).
    pub peak_rss: Option<u64>,
    /// The job stopped in `warned`, nobody acknowledged, and it was cancelled (exit 4).
    pub confirm_refused: bool,
    /// Ctrl+C was pressed at least once.
    pub interrupted: bool,
}

/// Resident set size of this process in bytes.
pub fn rss_bytes() -> Option<u64> {
    memory_stats::memory_stats().map(|m| m.physical_mem as u64)
}

/// Wait until `job_id` reaches a terminal state. The receiver must have been subscribed
/// before the job was started. Ctrl+C cancels the job and keeps waiting for its `done`
/// event; a second Ctrl+C gives up (`CoreError::Cancelled`).
pub async fn wait_for_job(db: &BulkDb, events: &mut broadcast::Receiver<CoreEvent>, job_id: &str, opts: WaitOptions) -> anyhow::Result<WaitOutcome> {
    let started = Instant::now();
    let mut peak_rss = rss_bytes();
    let mut last_print = Instant::now();
    let mut interrupted = false;
    let mut confirm_refused = false;
    let mut ctrl_c = Box::pin(tokio::signal::ctrl_c());
    let mut tick = tokio::time::interval(Duration::from_millis(250));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let done = loop {
        tokio::select! {
            ev = events.recv() => match ev {
                Ok(CoreEvent::JobDone(d)) if d.job_id == job_id => break d,
                Ok(CoreEvent::JobState(s)) if s.job_id == job_id => {
                    if s.state == JobState::Warned {
                        if !on_warned(db, job_id, opts)? {
                            confirm_refused = true;
                        }
                    } else if opts.progress {
                        eprintln!("job {job_id}: {}", enum_name(&s.state));
                    }
                }
                Ok(CoreEvent::JobLog(log)) if log.job_id.as_deref() == Some(job_id) && opts.progress => {
                    eprintln!("job {job_id} [{}] {}", enum_name(&log.level), log.message);
                }
                Ok(_) => {}
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!("missed {n} core events; checking job state directly");
                    if let Some(d) = done_from_registry(db, job_id) {
                        break d;
                    }
                }
                Err(broadcast::error::RecvError::Closed) => {
                    if let Some(d) = done_from_registry(db, job_id) {
                        break d;
                    }
                    bail!("core event channel closed while job {job_id} was running");
                }
            },
            res = &mut ctrl_c => {
                res.context("listening for Ctrl+C")?;
                ctrl_c = Box::pin(tokio::signal::ctrl_c());
                if interrupted {
                    eprintln!("second interrupt: giving up on job {job_id}");
                    return Err(CoreError::Cancelled.into());
                }
                interrupted = true;
                eprintln!("interrupt: cancelling job {job_id} (press Ctrl+C again to quit without waiting)");
                if let Err(e) = db.job_cancel(job_id) {
                    tracing::warn!("cancel failed: {e}");
                }
            },
            _ = tick.tick() => {
                if let Some(now) = rss_bytes() {
                    peak_rss = Some(peak_rss.map_or(now, |p| p.max(now)));
                }
                if opts.progress && last_print.elapsed() >= Duration::from_secs(1) {
                    last_print = Instant::now();
                    print_progress(db, job_id, started, peak_rss);
                }
            }
        }
    };
    if let Some(now) = rss_bytes() {
        peak_rss = Some(peak_rss.map_or(now, |p| p.max(now)));
    }
    Ok(WaitOutcome { done, elapsed: started.elapsed(), peak_rss, confirm_refused, interrupted })
}

/// The job stopped in `warned`: show the warnings and acknowledge (`--yes` or the tty), or
/// cancel. Returns `false` when it was cancelled for lack of acknowledgement.
fn on_warned(db: &BulkDb, job_id: &str, opts: WaitOptions) -> anyhow::Result<bool> {
    let summary = db.job_get(job_id)?;
    let report = summary.preflight.unwrap_or_default();
    crate::output::eprint_issues("Preflight warnings", &report.warnings);
    let codes: Vec<String> = report.warnings.iter().map(|w| w.code.clone()).collect();
    let accepted = if opts.auto_ack {
        true
    } else if prompts::is_interactive() {
        tokio::task::block_in_place(|| prompts::confirm(&format!("Proceed despite {} warning(s)? [y/N] ", codes.len())))?
    } else {
        eprintln!("warnings need acknowledgement and stdin is not a terminal: re-run with --yes");
        false
    };
    if accepted {
        db.job_resume(job_id, codes).context("resuming the job")?;
        Ok(true)
    } else {
        db.job_cancel(job_id).context("cancelling the job")?;
        Ok(false)
    }
}

/// Synthesize the `done` event from the registry when events were missed.
fn done_from_registry(db: &BulkDb, job_id: &str) -> Option<JobDone> {
    let s = db.job_get(job_id).ok()?;
    if !s.state.is_terminal() {
        return None;
    }
    Some(JobDone { job_id: s.id, state: s.state, result: s.result, error: s.error, exact_count: s.exact_count })
}

/// One progress line on stderr.
fn print_progress(db: &BulkDb, job_id: &str, started: Instant, peak_rss: Option<u64>) {
    let progress = db.progress_snapshot().into_iter().find(|p| p.job_id == job_id);
    let server = db.server_snapshot().into_iter().find(|s| s.job_id == job_id);
    let elapsed = fmt_clock(started.elapsed().as_secs());
    let mut line = format!("[{elapsed}]");
    match progress {
        Some(p) => {
            line.push_str(&format!(
                " rows {} ({} rows/s) | in {} | out {} ({}) | files {}",
                human_count(p.rows),
                short_count(p.rate_rows_per_sec),
                human_bytes(p.bytes_in),
                human_bytes(p.bytes_out),
                human_rate(p.rate_bytes_per_sec),
                p.files
            ));
            line.push_str(&format!(" | eta {}", p.eta_sec.map(fmt_clock).unwrap_or_else(|| "--:--".into())));
            line.push_str(&format!(" | {}", enum_name(&p.phase)));
        }
        None => line.push_str(" waiting for the job"),
    }
    if let Some(s) = server {
        let mut parts: Vec<String> = Vec::new();
        if let Some(st) = &s.state {
            parts.push(st.clone());
        }
        if let Some(w) = &s.wait_event {
            parts.push(w.clone());
        }
        if let Some(b) = s.bottleneck {
            parts.push(format!("bottleneck={}", enum_name(&b)));
        }
        if let Some(t) = s.temp_bytes.filter(|t| *t > 0) {
            parts.push(format!("temp {}", human_bytes(t)));
        }
        if !parts.is_empty() {
            line.push_str(&format!(" | server {}", parts.join("/")));
        }
    }
    match (rss_bytes(), peak_rss) {
        (Some(now), Some(peak)) => line.push_str(&format!(" | rss {} (peak {})", human_bytes(now), human_bytes(peak))),
        (Some(now), None) => line.push_str(&format!(" | rss {}", human_bytes(now))),
        _ => {}
    }
    eprintln!("{line}");
}
