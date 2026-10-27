//! Preflight: everything checked before the export starts. Warnings are shown and must be
//! acknowledged; blockers stop the job (`rejected`).
//!
//! | check              | rule                                                                     |
//! |--------------------|--------------------------------------------------------------------------|
//! | work_mem ceiling   | RAM = shared_buffers x 4; warn when work_mem x 3 > RAM x 10 %             |
//! | concurrent session | another active session reading one of the query's tables                 |
//! | plan               | Nested Loop with SubPlan/Aggregate inside -> "LATERAL pattern" warning     |
//! | result size        | plan rows x width vs free disk; blocker when disk is short                |
//! | output path        | directory writable (blocker), file exists and !overwrite (warning)        |
//! | sql                | single SELECT/CTE statement (blocker otherwise)                           |
//! | read-only          | account can write -> info                                                 |
//!
//! Issue codes produced here: `SQL_EMPTY` / `MULTI_STATEMENT` / `SQL_NOT_SELECT` /
//! `SQL_EXEC_COMMENT` (blockers, from `sqlguard::check_for` with the source's dialect),
//! `CONNECT_FAILED` (blocker), `WORK_MEM_HIGH` (danger), `WORK_MEM_INVALID`,
//! `EXPLAIN_FAILED` (info), `PLAN_LATERAL` (danger), `PLAN_SORT_SPILL` (warning),
//! `PLAN_SEQ_SCAN_LARGE` (info), `CONCURRENT_SESSION` (warning), `SESSIONS_UNKNOWN` (info),
//! `DISK_LOW` (warning), `DISK_FULL` (blocker), `DIR_NOT_WRITABLE` (blocker), `OVERWRITE`
//! (warning), `SPLIT_ADVICE` (info), `NOT_READONLY` (info).

use std::path::Path;
use std::time::Instant;

use crate::monitor::Monitor;
use crate::sink::SinkTarget;
use crate::source::Source;
use crate::sqlguard;
use crate::types::{
    ExportRequest, OutputFormat, PlanNode, PlanSummary, PlanWarning, PreflightIssue, PreflightReport, ServerInfo, SessionInfo, SessionOpts, Severity, TableRef,
};

/// PostgreSQL's default `work_mem` (4 MB), used for the spill check when the server does not
/// tell us its value.
pub const DEFAULT_WORK_MEM_BYTES: u64 = 4 * 1024 * 1024;
/// `PLAN_SEQ_SCAN_LARGE` threshold (estimated rows of one sequential scan).
pub const SEQ_SCAN_ROWS_THRESHOLD: f64 = 10_000_000.0;
/// `SPLIT_ADVICE` threshold (estimated rows of a single CSV file).
pub const SPLIT_ADVICE_ROWS: u64 = 1_000_000;
/// Headroom applied to the size estimate before comparing with free disk.
pub const DISK_HEADROOM: f64 = 1.1;

/// Inputs assembled by the service for one preflight run.
pub struct PreflightContext<'a> {
    pub request: &'a ExportRequest,
    pub sql: &'a str,
    pub source: &'a dyn Source,
    /// Separate light connection, when the driver has a monitor.
    pub monitor: Option<&'a dyn Monitor>,
    pub target: &'a SinkTarget,
    /// Session opts after defaults are applied.
    pub session: &'a SessionOpts,
}

/// Run every check. Never fails: problems become blockers.
pub async fn run(ctx: PreflightContext<'_>) -> PreflightReport {
    let started = Instant::now();
    let mut report = PreflightReport::default();
    let opts = &ctx.target.opts;

    // --- SQL ---------------------------------------------------------------------------------
    // Lexed with the rules of the server that will run it: MySQL reads quotes and comments
    // differently, and a `;` hidden from a standard-SQL scanner would split the statement there.
    let dialect = sqlguard::Dialect::from_driver(ctx.source.driver_id());
    let check = sqlguard::check_for(ctx.sql, dialect);
    report.blockers.extend(check.issues.iter().cloned());
    let sql_ok = check.ok;
    let sql = check.normalized.clone();

    // --- probe -------------------------------------------------------------------------------
    let server: Option<ServerInfo> = match ctx.source.probe().await {
        Ok(s) => Some(s),
        Err(e) => {
            report.blockers.push(PreflightIssue {
                code: "CONNECT_FAILED".into(),
                severity: Severity::Danger,
                message: format!("Could not probe the server: {e}"),
                detail: Some(e.code().to_string()),
                recommendation: Some("Check the connection settings and try 'Test connection'.".into()),
            });
            None
        }
    };

    // --- work_mem ----------------------------------------------------------------------------
    let ram = server.as_ref().and_then(|s| s.ram_bytes);
    let work_mem = ctx.session.work_mem.clone().or_else(|| server.as_ref().and_then(|s| s.settings.get("work_mem").cloned()));
    report.estimates.work_mem = work_mem.clone();
    report.estimates.server_ram_bytes = ram;
    if let Some(ram) = ram {
        report.estimates.recommended_work_mem = Some(recommended_work_mem(ram));
        if let Some(wm) = work_mem.as_deref() {
            if let Some(issue) = check_work_mem(wm, ram) {
                report.warnings.push(issue);
            }
        }
    }
    let work_mem_bytes = work_mem.as_deref().and_then(parse_mem).unwrap_or(DEFAULT_WORK_MEM_BYTES);

    // --- explain -----------------------------------------------------------------------------
    let plan: Option<PlanSummary> = if sql_ok {
        match ctx.source.explain(&sql, false).await {
            Ok(p) => p,
            Err(e) => {
                report.warnings.push(PreflightIssue {
                    code: "EXPLAIN_FAILED".into(),
                    severity: Severity::Info,
                    message: format!("EXPLAIN failed, so no size estimate or plan warnings are available: {e}"),
                    detail: Some(e.code().to_string()),
                    recommendation: None,
                });
                None
            }
        }
    } else {
        None
    };
    if let Some(plan) = &plan {
        let mut codes: Vec<String> = Vec::new();
        let mut plan_warnings: Vec<PlanWarning> = Vec::new();
        if let Some(root) = &plan.root {
            plan_warnings.extend(analyze_plan_with(root, work_mem_bytes));
        }
        // Keep driver-provided warnings whose codes we did not produce ourselves.
        for w in &plan.warnings {
            if !plan_warnings.iter().any(|p| p.code == w.code) {
                plan_warnings.push(w.clone());
            }
        }
        for w in plan_warnings {
            if codes.contains(&w.code) && w.severity != Severity::Danger {
                continue;
            }
            codes.push(w.code.clone());
            report.warnings.push(plan_warning_to_issue(&w));
        }
        let rows = plan.estimated_rows.or_else(|| plan.root.as_ref().and_then(|r| r.plan_rows).map(f64_to_u64));
        let width = plan.estimated_width.or_else(|| plan.root.as_ref().and_then(|r| r.plan_width));
        report.estimates.rows = rows;
        report.estimates.bytes = plan.estimated_bytes.or_else(|| match (rows, width) {
            (Some(r), Some(w)) => Some(r.saturating_mul(w as u64)),
            _ => None,
        });
    }

    // --- concurrent sessions -----------------------------------------------------------------
    if sql_ok {
        if let Some(monitor) = ctx.monitor {
            let mut tables: Vec<TableRef> = plan.as_ref().map(|p| p.tables.clone()).unwrap_or_default();
            if tables.is_empty() {
                tables = sqlguard::referenced_tables_for(&sql, dialect);
            }
            if !tables.is_empty() {
                match monitor.active_sessions(&tables).await {
                    Ok(sessions) if !sessions.is_empty() => {
                        report.warnings.push(concurrent_session_issue(&sessions, &tables));
                        report.sessions = sessions;
                    }
                    Ok(_) => {}
                    Err(e) => report.warnings.push(PreflightIssue {
                        code: "SESSIONS_UNKNOWN".into(),
                        severity: Severity::Info,
                        message: format!("Could not list concurrent sessions: {e}"),
                        detail: Some(e.code().to_string()),
                        recommendation: None,
                    }),
                }
            }
        }
    }

    // --- output directory --------------------------------------------------------------------
    let dir = &ctx.target.dir;
    if let Err(msg) = check_dir_writable(dir) {
        report.blockers.push(PreflightIssue {
            code: "DIR_NOT_WRITABLE".into(),
            severity: Severity::Danger,
            message: format!("Output directory {} is not writable: {msg}", dir.display()),
            detail: None,
            recommendation: Some("Create the directory or choose another output folder.".into()),
        });
    }

    // --- disk --------------------------------------------------------------------------------
    let free = disk_free(dir);
    report.estimates.disk_free_bytes = free;
    if let (Some(free), Some(bytes)) = (free, report.estimates.bytes) {
        let need = (bytes as f64 * DISK_HEADROOM) as u64;
        if free < need {
            report.blockers.push(PreflightIssue {
                code: "DISK_FULL".into(),
                severity: Severity::Danger,
                message: format!("Not enough disk space: about {} needed, {} free on {}.", format_size(need), format_size(free), dir.display()),
                detail: Some(format!("estimate {} x {DISK_HEADROOM:.1} headroom", format_size(bytes))),
                recommendation: Some("Free up space, choose another drive, or enable compression / Parquet.".into()),
            });
        } else if free < need.saturating_mul(2) {
            report.warnings.push(PreflightIssue {
                code: "DISK_LOW".into(),
                severity: Severity::Warning,
                message: format!("Disk space is tight: about {} needed, {} free on {}.", format_size(need), format_size(free), dir.display()),
                detail: Some(format!("estimate {} x {DISK_HEADROOM:.1} headroom; less than twice that is free", format_size(bytes))),
                recommendation: Some("Consider compression or Parquet, or another drive.".into()),
            });
        }
    }

    // --- first file / overwrite --------------------------------------------------------------
    let split = opts.split.as_ref().filter(|s| s.rows.is_some() || s.megabytes.is_some());
    let first = ctx.target.path_for(if split.is_some() { 1 } else { 0 });
    report.estimates.output_path = first.to_string_lossy().to_string();
    if first.exists() && !opts.overwrite {
        report.warnings.push(PreflightIssue {
            code: "OVERWRITE".into(),
            severity: Severity::Warning,
            message: format!("{} already exists and will be overwritten.", first.display()),
            detail: None,
            recommendation: Some("Acknowledge to overwrite, or change the file name pattern.".into()),
        });
    }

    // --- file count estimate -----------------------------------------------------------------
    report.estimates.files = match (split, report.estimates.rows, report.estimates.bytes) {
        (None, _, _) => Some(1),
        (Some(s), Some(rows), _) if s.rows.unwrap_or(0) > 0 => Some(rows.div_ceil(s.rows.unwrap_or(1)).max(1).min(u32::MAX as u64) as u32),
        (Some(s), _, Some(bytes)) if s.rows.is_none() && s.megabytes.unwrap_or(0) > 0 => {
            Some(bytes.div_ceil(s.megabytes.unwrap_or(1).saturating_mul(1024 * 1024)).max(1).min(u32::MAX as u64) as u32)
        }
        _ => None,
    };

    // --- split advice ------------------------------------------------------------------------
    if let Some(rows) = report.estimates.rows {
        if rows > SPLIT_ADVICE_ROWS && opts.format == OutputFormat::Csv && split.is_none() {
            report.warnings.push(PreflightIssue {
                code: "SPLIT_ADVICE".into(),
                severity: Severity::Info,
                message: format!("About {} rows will go into one CSV file; Excel opens at most 1,048,576 rows.", fmt_num(rows)),
                detail: None,
                recommendation: Some("Enable splitting (1,000,000 rows per file) if the file is meant for Excel, or use Parquet.".into()),
            });
        }
    }

    // --- read-only ---------------------------------------------------------------------------
    if let Some(s) = &server {
        if s.can_write == Some(true) {
            report.warnings.push(PreflightIssue {
                code: "NOT_READONLY".into(),
                severity: Severity::Info,
                message: format!("Account {} can modify data; a read-only account is recommended for extraction.", s.user.as_deref().unwrap_or("(unknown)")),
                detail: None,
                recommendation: Some("Create a read-only role for BulkDB.".into()),
            });
        }
    }

    report.server = server;
    report.plan = plan;
    report.elapsed_ms = started.elapsed().as_millis() as u64;
    report
}

/// `Ok(())` when `dir` exists, is a directory and a temp file can be created in it.
fn check_dir_writable(dir: &Path) -> std::result::Result<(), String> {
    let meta = std::fs::metadata(dir).map_err(|e| if e.kind() == std::io::ErrorKind::NotFound { "directory does not exist".to_string() } else { e.to_string() })?;
    if !meta.is_dir() {
        return Err("path is not a directory".into());
    }
    let probe = dir.join(format!(".bulkdb-write-test-{}", uuid::Uuid::new_v4().simple()));
    std::fs::File::create(&probe).map_err(|e| e.to_string())?;
    let _ = std::fs::remove_file(&probe);
    Ok(())
}

fn concurrent_session_issue(sessions: &[SessionInfo], tables: &[TableRef]) -> PreflightIssue {
    let names: Vec<String> = tables.iter().map(TableRef::qualified).collect();
    let lines: Vec<String> = sessions
        .iter()
        .map(|s| {
            let mut line = format!("pid {}", s.pid);
            if let Some(u) = &s.user {
                line.push_str(&format!(" user {u}"));
            }
            if let Some(ms) = s.elapsed_ms {
                line.push_str(&format!(" running {}", format_duration(ms)));
            }
            if let Some(st) = &s.state {
                line.push_str(&format!(" [{st}]"));
            }
            if s.blocking {
                line.push_str(" (holds a lock)");
            }
            if !s.tables.is_empty() {
                line.push_str(&format!(" on {}", s.tables.join(", ")));
            }
            line
        })
        .collect();
    PreflightIssue {
        code: "CONCURRENT_SESSION".into(),
        severity: Severity::Warning,
        message: format!("{} other session{} currently reading {}.", sessions.len(), if sessions.len() == 1 { " is" } else { "s are" }, names.join(", ")),
        detail: Some(lines.join("\n")),
        recommendation: Some("Running two heavy reads on the same table doubles the I/O; wait for the other session or proceed if the server is idle.".into()),
    }
}

fn plan_warning_to_issue(w: &PlanWarning) -> PreflightIssue {
    let recommendation = match w.code.as_str() {
        "PLAN_LATERAL" => Some("Rewrite the correlated subquery as a JOIN (or materialise it in a CTE); the inner plan runs once per outer row."),
        "PLAN_SORT_SPILL" => Some("Raise work_mem for this session or add an index matching the ORDER BY / GROUP BY."),
        "PLAN_SEQ_SCAN_LARGE" => Some("Expected for a full extraction; make sure the WHERE clause is what you want."),
        _ => None,
    };
    PreflightIssue {
        code: w.code.clone(),
        severity: w.severity,
        message: w.message.clone(),
        detail: w.detail.clone(),
        recommendation: recommendation.map(|s| s.to_string()),
    }
}

/// Walk a plan tree for dangerous patterns, assuming PostgreSQL's default `work_mem` (4 MB)
/// for the spill check. See [`analyze_plan_with`].
pub fn analyze_plan(root: &PlanNode) -> Vec<PlanWarning> {
    analyze_plan_with(root, DEFAULT_WORK_MEM_BYTES)
}

/// Walk a plan tree for dangerous patterns.
///
/// * `PLAN_LATERAL` (danger): a Nested Loop whose inner side contains a SubPlan / InitPlan /
///   Aggregate / Materialize, or that has a child with `parent_relationship = SubPlan`. The
///   inner plan is re-executed once per outer row; detail is `<outer rows> x <inner cost>`.
/// * `PLAN_SORT_SPILL` (warning): a Sort / Hash node whose `rows x width` exceeds `work_mem`.
/// * `PLAN_SEQ_SCAN_LARGE` (info): a sequential scan over more than 10 M rows.
pub fn analyze_plan_with(root: &PlanNode, work_mem_bytes: u64) -> Vec<PlanWarning> {
    let mut out = Vec::new();
    walk(root, work_mem_bytes, &mut out);
    out
}

fn walk(node: &PlanNode, work_mem: u64, out: &mut Vec<PlanWarning>) {
    let nt = node.node_type.as_str();

    if nt.contains("Nested Loop") {
        if let Some((outer, inner)) = nested_loop_sides(node) {
            let subplan_child = node.children.iter().find(|c| rel_is(c, "SubPlan"));
            let lateral_inner = inner.filter(|i| rel_is(i, "SubPlan") || rel_is(i, "InitPlan") || subtree_any(i, is_lateral_marker));
            if let Some(hot) = lateral_inner.or(subplan_child) {
                let outer_rows = outer.and_then(|o| o.plan_rows).unwrap_or(0.0);
                let inner_cost = hot.total_cost.unwrap_or(0.0);
                let what = subtree_first(hot, is_lateral_marker).map(|n| n.node_type.clone()).unwrap_or_else(|| hot.node_type.clone());
                out.push(PlanWarning {
                    code: "PLAN_LATERAL".into(),
                    severity: Severity::Danger,
                    message: format!(
                        "Nested Loop re-runs a {what} for every outer row (LATERAL / correlated subquery pattern): about {} iterations.",
                        fmt_num(f64_to_u64(outer_rows))
                    ),
                    detail: Some(format!("{} x {:.0}", fmt_num(f64_to_u64(outer_rows)), inner_cost)),
                });
            }
        }
    }

    if is_sort_or_hash(nt) {
        if let (Some(rows), Some(width)) = (node.plan_rows, node.plan_width) {
            let bytes = rows.max(0.0) * width as f64;
            if bytes > work_mem as f64 {
                out.push(PlanWarning {
                    code: "PLAN_SORT_SPILL".into(),
                    severity: Severity::Warning,
                    message: format!(
                        "{nt} over about {} rows x {width} B (~{}) exceeds work_mem {}; it will spill to temp files on the server.",
                        fmt_num(f64_to_u64(rows)),
                        format_size(bytes as u64),
                        format_mem(work_mem)
                    ),
                    detail: node.extra.get("Sort Key").or_else(|| node.extra.get("Hash Cond")).or_else(|| node.extra.get("Group Key")).cloned(),
                });
            }
        }
    }

    if is_seq_scan(nt) {
        if let Some(rows) = node.plan_rows {
            if rows > SEQ_SCAN_ROWS_THRESHOLD {
                let rel = node.relation.as_ref().map(TableRef::qualified).or_else(|| node.alias.clone()).unwrap_or_else(|| "a table".into());
                out.push(PlanWarning {
                    code: "PLAN_SEQ_SCAN_LARGE".into(),
                    severity: Severity::Info,
                    message: format!("Sequential scan of {rel}: about {} rows.", fmt_num(f64_to_u64(rows))),
                    detail: node.extra.get("Filter").cloned(),
                });
            }
        }
    }

    for c in &node.children {
        walk(c, work_mem, out);
    }
}

/// `(outer, inner)` children of a join node: by `parent_relationship` when the planner labels
/// them (PostgreSQL), otherwise first / second child.
fn nested_loop_sides(node: &PlanNode) -> Option<(Option<&PlanNode>, Option<&PlanNode>)> {
    if node.children.is_empty() {
        return None;
    }
    let outer = node.children.iter().find(|c| rel_is(c, "Outer"));
    let inner = node.children.iter().find(|c| rel_is(c, "Inner"));
    if outer.is_some() || inner.is_some() {
        return Some((outer, inner));
    }
    let plain: Vec<&PlanNode> = node.children.iter().filter(|c| !rel_is(c, "SubPlan") && !rel_is(c, "InitPlan")).collect();
    Some((plain.first().copied(), plain.get(1).copied()))
}

fn rel_is(node: &PlanNode, rel: &str) -> bool {
    node.parent_relationship.as_deref().map(|r| r.eq_ignore_ascii_case(rel)).unwrap_or(false)
}

fn is_lateral_marker(node: &PlanNode) -> bool {
    let nt = node.node_type.as_str();
    rel_is(node, "SubPlan") || rel_is(node, "InitPlan") || nt.contains("Aggregate") || nt == "Materialize"
}

fn is_sort_or_hash(nt: &str) -> bool {
    matches!(nt, "Sort" | "Incremental Sort" | "Hash" | "HashAggregate" | "Hash Aggregate" | "GroupAggregate" | "Group Aggregate")
}

fn is_seq_scan(nt: &str) -> bool {
    nt.contains("Seq Scan") || nt == "Table Scan" || nt == "Full Table Scan"
}

fn subtree_any(node: &PlanNode, pred: fn(&PlanNode) -> bool) -> bool {
    pred(node) || node.children.iter().any(|c| subtree_any(c, pred))
}

fn subtree_first(node: &PlanNode, pred: fn(&PlanNode) -> bool) -> Option<&PlanNode> {
    if pred(node) {
        return Some(node);
    }
    node.children.iter().find_map(|c| subtree_first(c, pred))
}

/// `work_mem x 3 > RAM x 10 %` check. Returns the warning and the recommended value.
///
/// A value that cannot be parsed yields a `WORK_MEM_INVALID` warning (the `SET` would fail on
/// the server anyway).
pub fn check_work_mem(work_mem: &str, ram_bytes: u64) -> Option<PreflightIssue> {
    let Some(wm) = parse_mem(work_mem) else {
        return Some(PreflightIssue {
            code: "WORK_MEM_INVALID".into(),
            severity: Severity::Warning,
            message: format!("work_mem '{work_mem}' is not a valid memory size."),
            detail: None,
            recommendation: Some(format!("Use a value like {}.", recommended_work_mem(ram_bytes))),
        });
    };
    if ram_bytes == 0 {
        return None;
    }
    let budget = ram_bytes / 10;
    let triple = wm.saturating_mul(3);
    if triple > budget {
        let rec = recommended_work_mem(ram_bytes);
        Some(PreflightIssue {
            code: "WORK_MEM_HIGH".into(),
            severity: Severity::Danger,
            message: format!("work_mem {} is high for a server with about {} of RAM.", format_mem(wm), format_size(ram_bytes)),
            detail: Some(format!(
                "work_mem x 3 ({}) exceeds 10 % of RAM ({}); every sort and hash in the plan may allocate that much and push the server into swap or OOM.",
                format_size(triple),
                format_size(budget)
            )),
            recommendation: Some(format!("Use {rec} (RAM / 64) for this session.")),
        })
    } else {
        None
    }
}

/// RAM / 64, rounded down to a power-of-two-ish MB value, min 4MB (4 GB -> 64MB).
pub fn recommended_work_mem(ram_bytes: u64) -> String {
    let mb = (ram_bytes / 64 / (1024 * 1024)).max(4);
    format!("{mb}MB")
}

/// Parse PostgreSQL style memory strings (`64MB`, `512kB`, `1GB`, `8192` = kB) into bytes.
pub fn parse_mem(s: &str) -> Option<u64> {
    let s = s.trim();
    let idx = s.find(|c: char| !c.is_ascii_digit() && c != '.').unwrap_or(s.len());
    let (num, unit) = s.split_at(idx);
    let n: f64 = num.trim().parse().ok()?;
    let mult: f64 = match unit.trim().to_ascii_lowercase().as_str() {
        "" | "kb" => 1024.0,
        "b" => 1.0,
        "mb" => 1024.0 * 1024.0,
        "gb" => 1024.0 * 1024.0 * 1024.0,
        "tb" => 1024.0 * 1024.0 * 1024.0 * 1024.0,
        _ => return None,
    };
    Some((n * mult) as u64)
}

pub fn format_mem(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;
    if bytes >= GB && bytes.is_multiple_of(GB) {
        format!("{}GB", bytes / GB)
    } else if bytes >= MB {
        format!("{}MB", bytes / MB)
    } else {
        format!("{}kB", bytes / KB)
    }
}

/// Human readable size with one decimal (`1.5 GB`), for messages.
pub fn format_size(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    let b = bytes as f64;
    if b >= KB * KB * KB * KB {
        format!("{:.1} TB", b / (KB * KB * KB * KB))
    } else if b >= KB * KB * KB {
        format!("{:.1} GB", b / (KB * KB * KB))
    } else if b >= KB * KB {
        format!("{:.1} MB", b / (KB * KB))
    } else if b >= KB {
        format!("{:.1} kB", b / KB)
    } else {
        format!("{bytes} B")
    }
}

/// `1234567` -> `1,234,567`.
pub fn fmt_num(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn format_duration(ms: u64) -> String {
    let s = ms / 1000;
    if s >= 3600 {
        format!("{}h {}m", s / 3600, (s % 3600) / 60)
    } else if s >= 60 {
        format!("{}m {}s", s / 60, s % 60)
    } else {
        format!("{s}s")
    }
}

fn f64_to_u64(v: f64) -> u64 {
    if v.is_finite() && v > 0.0 {
        v.round() as u64
    } else {
        0
    }
}

/// Free bytes on the volume holding `path`.
pub fn disk_free(path: &Path) -> Option<u64> {
    fs4::available_space(path).ok()
}

/// Default session values derived from a probe (RAM/64, 60 min, 5 s).
pub fn recommended_session(server: &ServerInfo) -> SessionOpts {
    SessionOpts {
        work_mem: server.ram_bytes.map(recommended_work_mem),
        statement_timeout_ms: Some(60 * 60 * 1000),
        lock_timeout_ms: Some(5_000),
        max_parallel_workers: None,
        fetch_batch_rows: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::{CoreError, Result};
    use crate::monitor::Monitor;
    use crate::source::{Query, Source, SourceStream};
    use crate::types::{OutputOpts, SplitOpts};
    use async_trait::async_trait;
    use std::collections::BTreeMap;
    use tokio_util::sync::CancellationToken;

    const GB: u64 = 1024 * 1024 * 1024;

    #[test]
    fn work_mem_rule() {
        let w = check_work_mem("512MB", 4 * GB).expect("512MB on 4GB must warn");
        assert_eq!(w.code, "WORK_MEM_HIGH");
        assert_eq!(w.severity, Severity::Danger);
        assert!(w.recommendation.as_deref().unwrap().contains("64MB"), "{:?}", w.recommendation);
        assert!(check_work_mem("64MB", 4 * GB).is_none());
        assert!(check_work_mem("128MB", 4 * GB).is_none()); // 384 MB < 409.6 MB
        assert_eq!(check_work_mem("lots", 4 * GB).unwrap().code, "WORK_MEM_INVALID");
        assert!(check_work_mem("512MB", 0).is_none());
    }

    #[test]
    fn mem_parsing_and_formatting() {
        assert_eq!(parse_mem("64MB"), Some(64 * 1024 * 1024));
        assert_eq!(parse_mem("512kB"), Some(512 * 1024));
        assert_eq!(parse_mem("1GB"), Some(GB));
        assert_eq!(parse_mem("8192"), Some(8192 * 1024));
        assert_eq!(parse_mem(" 1.5 GB "), Some(GB + GB / 2));
        assert_eq!(parse_mem("16 B"), Some(16));
        assert_eq!(parse_mem("1TB"), Some(1024 * GB));
        assert_eq!(parse_mem("abc"), None);
        assert_eq!(parse_mem("10 parsecs"), None);
        assert_eq!(format_mem(64 * 1024 * 1024), "64MB");
        assert_eq!(format_mem(4 * GB), "4GB");
        assert_eq!(format_mem(1536 * 1024 * 1024), "1536MB");
        assert_eq!(format_mem(512 * 1024), "512kB");
        assert_eq!(recommended_work_mem(4 * GB), "64MB");
        assert_eq!(recommended_work_mem(0), "4MB");
        assert_eq!(format_size(1536 * 1024 * 1024), "1.5 GB");
        assert_eq!(fmt_num(1_234_567), "1,234,567");
        assert_eq!(fmt_num(12), "12");
    }

    fn node(node_type: &str, rows: f64, width: u32, cost: f64, rel: Option<&str>, children: Vec<PlanNode>) -> PlanNode {
        PlanNode {
            node_type: node_type.into(),
            plan_rows: Some(rows),
            plan_width: Some(width),
            total_cost: Some(cost),
            parent_relationship: rel.map(|s| s.to_string()),
            children,
            ..PlanNode::default()
        }
    }

    #[test]
    fn lateral_pattern_detected() {
        // Nested Loop: outer Seq Scan (50k rows), inner Aggregate with a SubPlan.
        let inner = node("Aggregate", 1.0, 8, 123.0, Some("Inner"), vec![node("Index Scan", 10.0, 40, 100.0, Some("Outer"), vec![])]);
        let outer = node("Seq Scan", 50_000.0, 100, 1000.0, Some("Outer"), vec![]);
        let root = node("Nested Loop", 50_000.0, 108, 6_000_000.0, None, vec![outer, inner]);
        let w = analyze_plan(&root);
        let lat = w.iter().find(|w| w.code == "PLAN_LATERAL").expect("lateral warning");
        assert_eq!(lat.severity, Severity::Danger);
        assert_eq!(lat.detail.as_deref(), Some("50,000 x 123"));

        // SubPlan child under the loop, no labelled sides.
        let root = node(
            "Nested Loop Left Join",
            10.0,
            8,
            10.0,
            None,
            vec![node("Seq Scan", 10.0, 8, 1.0, None, vec![]), node("Seq Scan", 10.0, 8, 1.0, None, vec![]), node("Limit", 1.0, 8, 9.0, Some("SubPlan"), vec![])],
        );
        assert!(analyze_plan(&root).iter().any(|w| w.code == "PLAN_LATERAL"));

        // A plain nested loop over an index scan is fine.
        let root =
            node("Nested Loop", 10.0, 8, 10.0, None, vec![node("Seq Scan", 10.0, 8, 1.0, Some("Outer"), vec![]), node("Index Scan", 1.0, 8, 1.0, Some("Inner"), vec![])]);
        assert!(analyze_plan(&root).is_empty());
    }

    #[test]
    fn sort_spill_and_seq_scan() {
        let sort = node("Sort", 1_000_000.0, 100, 10.0, None, vec![node("Seq Scan", 20_000_000.0, 100, 1.0, None, vec![])]);
        let w = analyze_plan_with(&sort, 64 * 1024 * 1024);
        assert!(w.iter().any(|w| w.code == "PLAN_SORT_SPILL" && w.severity == Severity::Warning), "{w:?}");
        assert!(w.iter().any(|w| w.code == "PLAN_SEQ_SCAN_LARGE" && w.severity == Severity::Info), "{w:?}");
        // Fits in work_mem: no spill warning.
        let w = analyze_plan_with(&sort, 1024 * 1024 * 1024);
        assert!(!w.iter().any(|w| w.code == "PLAN_SORT_SPILL"));
        // Small scan: nothing.
        let small = node("Seq Scan", 1000.0, 10, 1.0, None, vec![]);
        assert!(analyze_plan(&small).is_empty());
    }

    // --- end-to-end run() with fakes ---------------------------------------------------------

    struct FakeSource {
        /// What `driver_id()` reports (picks the sqlguard dialect).
        driver: &'static str,
        probe: Result<ServerInfo>,
        explain: Result<Option<PlanSummary>>,
    }

    #[async_trait]
    impl Source for FakeSource {
        async fn probe(&self) -> Result<ServerInfo> {
            self.probe.clone()
        }
        async fn apply_session(&self, _opts: &SessionOpts) -> Result<()> {
            Ok(())
        }
        async fn stream(&self, _q: &Query, _opts: &SessionOpts, _cancel: CancellationToken) -> Result<SourceStream> {
            Err(CoreError::internal("not used"))
        }
        async fn explain(&self, _sql: &str, _analyze: bool) -> Result<Option<PlanSummary>> {
            self.explain.clone()
        }
        fn session_id(&self) -> Option<String> {
            None
        }
        fn driver_id(&self) -> &'static str {
            self.driver
        }
    }

    struct FakeMonitor {
        sessions: Vec<SessionInfo>,
    }

    #[async_trait]
    impl Monitor for FakeMonitor {
        async fn snapshot(&self, _job: &crate::monitor::JobHandle) -> Result<crate::monitor::ServerSnapshot> {
            Ok(Default::default())
        }
        async fn active_sessions(&self, _touching: &[TableRef]) -> Result<Vec<SessionInfo>> {
            Ok(self.sessions.clone())
        }
    }

    fn server(ram: u64, work_mem: &str, can_write: Option<bool>) -> ServerInfo {
        let mut settings = BTreeMap::new();
        settings.insert("work_mem".to_string(), work_mem.to_string());
        ServerInfo { driver: "fake".into(), version: "1".into(), user: Some("app".into()), ram_bytes: Some(ram), settings, can_write, ..Default::default() }
    }

    fn plan(rows: f64, width: u32) -> PlanSummary {
        let root = node("Seq Scan", rows, width, 100.0, None, vec![]);
        PlanSummary {
            estimated_rows: Some(rows as u64),
            estimated_width: Some(width),
            estimated_bytes: Some((rows as u64).saturating_mul(width as u64)),
            tables: vec![TableRef::new(Some("public"), "orders")],
            root: Some(root),
            raw_format: "json".into(),
            ..Default::default()
        }
    }

    fn request(dir: &Path, split: Option<SplitOpts>, overwrite: bool) -> (ExportRequest, SinkTarget) {
        let output = OutputOpts { dir: dir.to_string_lossy().to_string(), split, overwrite, ..OutputOpts::default() };
        let req = ExportRequest {
            conn_id: "c1".into(),
            sql: Some("select 1".into()),
            sql_file: None,
            name: None,
            session: SessionOpts::default(),
            output: output.clone(),
            password: None,
        };
        let target = SinkTarget { dir: dir.to_path_buf(), stem: "out".into(), opts: output };
        (req, target)
    }

    fn codes(issues: &[PreflightIssue]) -> Vec<&str> {
        issues.iter().map(|i| i.code.as_str()).collect()
    }

    #[tokio::test]
    async fn run_collects_every_issue() {
        let dir = tempfile::tempdir().unwrap();
        let (req, target) = request(dir.path(), None, false);
        // The first output file already exists.
        std::fs::write(target.path_for(0), b"old").unwrap();
        let src = FakeSource { driver: "fake", probe: Ok(server(4 * GB, "512MB", Some(true))), explain: Ok(Some(plan(20_000_000.0, 10))) };
        let mon = FakeMonitor { sessions: vec![SessionInfo { pid: "42".into(), user: Some("bob".into()), elapsed_ms: Some(65_000), ..Default::default() }] };
        let session = SessionOpts { work_mem: Some("512MB".into()), ..Default::default() };
        let report =
            run(PreflightContext { request: &req, sql: "select * from public.orders;", source: &src, monitor: Some(&mon), target: &target, session: &session }).await;

        assert!(report.blockers.is_empty(), "{:?}", report.blockers);
        let w = codes(&report.warnings);
        for expected in ["WORK_MEM_HIGH", "PLAN_SEQ_SCAN_LARGE", "CONCURRENT_SESSION", "OVERWRITE", "SPLIT_ADVICE", "NOT_READONLY"] {
            assert!(w.contains(&expected), "missing {expected} in {w:?}");
        }
        assert_eq!(report.estimates.rows, Some(20_000_000));
        assert_eq!(report.estimates.bytes, Some(200_000_000));
        assert_eq!(report.estimates.files, Some(1));
        assert_eq!(report.estimates.recommended_work_mem.as_deref(), Some("64MB"));
        assert_eq!(report.estimates.work_mem.as_deref(), Some("512MB"));
        assert_eq!(report.estimates.output_path, target.path_for(0).to_string_lossy());
        assert_eq!(report.sessions.len(), 1);
        let cs = report.warnings.iter().find(|i| i.code == "CONCURRENT_SESSION").unwrap();
        assert!(cs.detail.as_deref().unwrap().contains("pid 42 user bob running 1m 5s"), "{:?}", cs.detail);
        assert!(report.server.is_some());
        assert!(report.plan.is_some());
    }

    #[tokio::test]
    async fn run_turns_failures_into_issues() {
        let missing = std::env::temp_dir().join(format!("bulkdb-preflight-missing-{}", uuid::Uuid::new_v4().simple()));
        let (req, target) = request(&missing, Some(SplitOpts { rows: Some(1_000_000), megabytes: None }), true);
        let src = FakeSource { driver: "fake", probe: Err(CoreError::Connect("refused".into())), explain: Err(CoreError::query("no explain")) };
        let session = SessionOpts::default();
        let report = run(PreflightContext { request: &req, sql: "delete from t", source: &src, monitor: None, target: &target, session: &session }).await;
        let b = codes(&report.blockers);
        assert!(b.contains(&"SQL_NOT_SELECT"), "{b:?}");
        assert!(b.contains(&"CONNECT_FAILED"), "{b:?}");
        assert!(b.contains(&"DIR_NOT_WRITABLE"), "{b:?}");
        // Invalid SQL: EXPLAIN is skipped, so no EXPLAIN_FAILED here.
        assert!(!codes(&report.warnings).contains(&"EXPLAIN_FAILED"));
        assert_eq!(report.estimates.output_path, target.path_for(1).to_string_lossy());
        assert!(report.server.is_none());
    }

    #[tokio::test]
    async fn explain_failure_is_only_info() {
        let dir = tempfile::tempdir().unwrap();
        let (req, target) = request(dir.path(), None, false);
        let src = FakeSource { driver: "fake", probe: Ok(server(4 * GB, "64MB", Some(false))), explain: Err(CoreError::query("no explain")) };
        let session = SessionOpts::default();
        let report = run(PreflightContext { request: &req, sql: "select 1", source: &src, monitor: None, target: &target, session: &session }).await;
        assert!(report.blockers.is_empty(), "{:?}", report.blockers);
        let ef = report.warnings.iter().find(|i| i.code == "EXPLAIN_FAILED").expect("info issue");
        assert_eq!(ef.severity, Severity::Info);
        assert!(!codes(&report.warnings).contains(&"WORK_MEM_HIGH"));
        assert!(!codes(&report.warnings).contains(&"NOT_READONLY"));
        assert_eq!(report.estimates.rows, None);
        assert!(report.estimates.disk_free_bytes.is_some());
    }

    /// The SQL check follows the source's dialect: MySQL block comments do not nest, so the
    /// `;` that PostgreSQL rules hide inside a nested comment splits the query on MySQL.
    #[tokio::test]
    async fn sql_check_uses_the_source_dialect() {
        let dir = tempfile::tempdir().unwrap();
        let (req, target) = request(dir.path(), None, false);
        let session = SessionOpts::default();
        let sql = "select 1 /* /* */ ; delete from t; /* */";
        for (driver, blocked) in [("postgres", false), ("mysql", true), ("odbc", true)] {
            let src = FakeSource { driver, probe: Ok(server(4 * GB, "64MB", Some(false))), explain: Ok(None) };
            let report = run(PreflightContext { request: &req, sql, source: &src, monitor: None, target: &target, session: &session }).await;
            let b = codes(&report.blockers);
            assert_eq!(b.contains(&"MULTI_STATEMENT"), blocked, "{driver}: {b:?}");
        }
    }

    #[tokio::test]
    async fn disk_full_is_a_blocker() {
        let dir = tempfile::tempdir().unwrap();
        let (req, target) = request(dir.path(), Some(SplitOpts { rows: None, megabytes: Some(1) }), false);
        // 10^15 rows x 1000 B (an exabyte) cannot fit on any disk.
        let src = FakeSource { driver: "fake", probe: Ok(server(4 * GB, "64MB", None)), explain: Ok(Some(plan(1.0e15, 1000))) };
        let session = SessionOpts::default();
        let report = run(PreflightContext { request: &req, sql: "select 1", source: &src, monitor: None, target: &target, session: &session }).await;
        assert!(codes(&report.blockers).contains(&"DISK_FULL"), "{:?}", report.blockers);
        assert!(report.estimates.files.unwrap() > 1);
    }
}
