//! `bulk-cli copy plan | run | resume | runs | status | forget` (docs/db-copy.md 6.3) over the
//! `BulkDb::copy_*` facade. Owner: copy-data.
//!
//! * `plan` prints the plan id, the endpoints, the facts line, the items and the
//!   acknowledgements needed (`--sql`: every statement and the session settings).
//! * `run` plans, prints, binds to `--expect-plan`, collects acknowledgements (`--ack CODE`,
//!   `--yes`, or one question per code on a terminal), starts, waits (Ctrl+C cancels and
//!   waits; a second Ctrl+C gives up) and prints the summary.
//! * Exit codes: 0 done; 1 failed before a result existed; 2 cancelled; 3 rejected or
//!   `--expect-plan` mismatch; 4 acknowledgement / confirmation needed; 5 the job failed or
//!   ended with problems with a stored, resumable result.

use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context;
use bulk_core::types::{
    AckNeed, ConnTag, CopyEndpoint, CopyItemState, CopyOptions, CopyPlan, CopyRequest, CopyResult, CopyResumeRequest, CopyRunDetail, CopyRunInfo, CopySelection,
    CopyStatus, ItemPolicy, JobState, MysqlColumnOverride, MysqlTableOverride, NameCase, NameMapping, PlanAction, RenameKind, Severity, TableRef,
};
use bulk_core::BulkDb;

use crate::args_copy::{CopyArgs, CopyCmd, CopySpecArgs};
use crate::connect::{find_saved, password_from_env};
use crate::output::{enum_name, fmt_clock, fmt_duration_ms, human_bytes, human_count, human_rate, print_json, Align, Table};
use crate::prompts;
use crate::{EXIT_CANCELLED, EXIT_CONFIRM, EXIT_DONE, EXIT_FAILED, EXIT_REJECTED};

/// The job failed or ended with problems, with a stored, resumable result.
pub const EXIT_PROBLEMS: u8 = 5;

pub async fn run(db: &Arc<BulkDb>, a: CopyArgs, json: bool) -> anyhow::Result<ExitCode> {
    match a.cmd {
        CopyCmd::Plan(p) => {
            let req = resolved_request(db, &p.spec).await?;
            let plan = plan_with_ctrl_c(db, &req).await?;
            if json {
                print_json(&plan)?;
            } else {
                print_plan(&plan, p.sql);
            }
            Ok(ExitCode::from(if plan.blockers.is_empty() { EXIT_DONE } else { EXIT_REJECTED }))
        }
        CopyCmd::Run(r) => {
            let mut req = resolved_request(db, &r.spec).await?;
            req.options.dry_run = r.dry_run;
            req.options.confirm_production = r.confirm_production.clone();
            let plan = plan_with_ctrl_c(db, &req).await?;
            if !json {
                print_plan(&plan, false);
            }
            if !plan.blockers.is_empty() {
                eprint_blockers(&plan, json);
                return Ok(ExitCode::from(EXIT_REJECTED));
            }
            if let Some(expected) = r.expect_plan.as_deref().map(str::trim).filter(|e| !e.is_empty()) {
                if expected != plan.plan_id {
                    eprintln!("PLAN_CHANGED: the plan is {} but --expect-plan is {expected}", plan.plan_id);
                    match db.copy_recent_plan(expected) {
                        Some(old) => eprintln!("  changed items: {}", bulk_core::copy::preflight::plan_diff(&old, &plan).join(", ")),
                        None => eprintln!("  the reviewed plan {expected} is not known to this process; review the new plan with `bulk-cli copy plan`"),
                    }
                    return Ok(ExitCode::from(EXIT_REJECTED));
                }
            }
            if let Some(code) = production_gate(db, &req, r.confirm_production.as_deref()) {
                return Ok(code);
            }
            let Some(keys) = collect_acks(&plan.acks, &r.ack, r.yes)? else {
                return Ok(ExitCode::from(EXIT_CONFIRM));
            };
            // Bind the job to the reviewed plan id (the one --expect-plan pinned, which equals
            // plan.plan_id here): core compares its own re-plan against it and raises PLAN_CHANGED
            // when live state moved, which `on_warned` then refuses rather than auto-acknowledges.
            let expect_plan = r.expect_plan.as_deref().map(str::trim).filter(|e| !e.is_empty()).map(str::to_string);
            let bind = expect_plan.clone().unwrap_or_else(|| plan.plan_id.clone());
            let job = db.copy_start(req, bind, keys).await?;
            if !json {
                eprintln!("copy job {job} started{}", if r.dry_run { " (dry run)" } else { "" });
            }
            finish(db, &job, WaitOpts { progress: r.progress, yes: r.yes, acks: r.ack.clone(), expect_plan }, json).await
        }
        CopyCmd::Resume(r) => {
            let detail = db.copy_run(&r.run)?;
            let req = CopyResumeRequest {
                run_id: r.run.clone(),
                acknowledged: Vec::new(),
                partial: r.partial.into(),
                source_password: r.source_password_env.as_deref().map(password_from_env).transpose()?,
                target_password: r.target_password_env.as_deref().map(password_from_env).transpose()?,
                confirm_production: r.confirm_production.clone(),
            };
            if let Some(code) = production_gate(db, &detail.request, r.confirm_production.as_deref()) {
                return Ok(code);
            }
            let job = db.copy_resume(req).await?;
            if !json {
                eprintln!("copy job {job} resumes run {} (existing-table policy as run: {})", r.run, enum_name(&detail.request.options.existing));
            }
            finish(db, &job, WaitOpts { progress: r.progress, yes: r.yes, acks: r.ack.clone(), expect_plan: None }, json).await
        }
        CopyCmd::Runs(r) => {
            let conn_id = r.conn.as_deref().map(|c| find_saved(db, c).map(|c| c.id)).transpose()?;
            let runs = db.copy_runs(conn_id.as_deref(), r.limit)?;
            if json {
                print_json(&runs)?;
            } else {
                print_runs(&runs);
            }
            Ok(ExitCode::from(EXIT_DONE))
        }
        CopyCmd::Status(s) => {
            let detail = db.copy_run(&s.run)?;
            if json {
                print_json(&detail)?;
            } else {
                print_detail(&detail);
            }
            Ok(ExitCode::from(EXIT_DONE))
        }
        CopyCmd::Forget(s) => {
            db.copy_forget(&s.run)?;
            if json {
                print_json(&serde_json::json!({ "forgotten": s.run }))?;
            } else {
                println!("forgot copy run {}", s.run);
            }
            Ok(ExitCode::from(EXIT_DONE))
        }
    }
}

/// Plan; Ctrl+C cancels the planning (`CANCELLED`, exit 2).
async fn plan_with_ctrl_c(db: &Arc<BulkDb>, req: &CopyRequest) -> anyhow::Result<CopyPlan> {
    let token = format!("cli-{}", std::process::id());
    let fut = db.copy_plan(req, Some(token.clone()));
    tokio::pin!(fut);
    tokio::select! {
        r = &mut fut => Ok(r?),
        _ = tokio::signal::ctrl_c() => {
            eprintln!("interrupted: cancelling the plan");
            let _ = db.copy_plan_cancel(&token);
            Err(bulk_core::CoreError::Cancelled.into())
        }
    }
}

/// A production target needs `--confirm-production <its name>`; `--yes` never replaces it.
fn production_gate(db: &BulkDb, req: &CopyRequest, typed: Option<&str>) -> Option<ExitCode> {
    let tgt = db.conn_get(&req.target.conn_id).ok()?;
    if tgt.tag != Some(ConnTag::Production) {
        return None;
    }
    if typed.map(str::trim) == Some(tgt.name.trim()) {
        return None;
    }
    eprintln!("the target '{}' is tagged production: add --confirm-production \"{}\" (--yes does not replace it)", tgt.name, tgt.name);
    Some(ExitCode::from(EXIT_CONFIRM))
}

/// Keys acknowledged by `--ack CODE` (every key of that code) or `--yes`; missing ones are
/// asked for on a terminal. `None` when some stay unacknowledged.
fn collect_acks(acks: &[AckNeed], codes: &[String], yes: bool) -> anyhow::Result<Option<Vec<String>>> {
    let mut keys = Vec::new();
    let mut missing: Vec<&AckNeed> = Vec::new();
    for a in acks {
        if yes || codes.iter().any(|c| c.eq_ignore_ascii_case(&a.code) || c == &a.key) {
            keys.push(a.key.clone());
        } else {
            missing.push(a);
        }
    }
    if missing.is_empty() {
        return Ok(Some(keys));
    }
    if !prompts::is_interactive() {
        eprintln!("acknowledgements needed (stdin is not a terminal): {}", missing.iter().map(|a| a.code.as_str()).collect::<Vec<_>>().join(", "));
        eprintln!("re-run with --ack CODE for each, or --yes");
        return Ok(None);
    }
    for a in missing {
        eprintln!("{} [{}]{}", a.code, enum_name(&a.severity), if a.items.is_empty() { String::new() } else { format!(" — {} item(s):", a.items.len()) });
        for i in a.items.iter().take(20) {
            eprintln!("    {i}");
        }
        if a.items.len() > 20 {
            eprintln!("    … and {} more", a.items.len() - 20);
        }
        if !prompts::confirm(&format!("Acknowledge {}? [y/N] ", a.code))? {
            eprintln!("not acknowledged: {}", a.code);
            return Ok(None);
        }
        keys.push(a.key.clone());
    }
    Ok(Some(keys))
}

struct WaitOpts {
    progress: bool,
    yes: bool,
    acks: Vec<String>,
    /// The reviewed plan id pinned by `--expect-plan` (if any): a PLAN_CHANGED surfaced while the
    /// job waits is then refused (exit 3, 6.3), never acknowledged by `--yes` / `--ack`.
    expect_plan: Option<String>,
}

/// What `on_warned` decided about a job waiting in `warned`.
enum Warned {
    /// Acknowledgements collected; the job was resumed.
    Resumed,
    /// Acknowledgements refused (cancelled, exit 4).
    Refused,
    /// `--expect-plan` was pinned and the job's plan changed: refused, exit 3 (6.3).
    PlanChanged,
}

/// Wait for the job, print the summary (or the result as JSON) and map the exit code.
async fn finish(db: &Arc<BulkDb>, job: &str, o: WaitOpts, json: bool) -> anyhow::Result<ExitCode> {
    let started = Instant::now();
    let mut last_print = Instant::now();
    let mut peak_rss = crate::progress::rss_bytes();
    let mut interrupted = false;
    let mut confirm_refused = false;
    let mut plan_changed = false;
    let mut ctrl_c = Box::pin(tokio::signal::ctrl_c());
    let mut tick = tokio::time::interval(Duration::from_millis(250));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let status = loop {
        tokio::select! {
            _ = &mut ctrl_c => {
                if interrupted {
                    eprintln!("interrupted again: giving up");
                    return Ok(ExitCode::from(EXIT_CANCELLED));
                }
                interrupted = true;
                eprintln!("interrupted: cancelling copy job {job} (Ctrl+C again to give up waiting)");
                let _ = db.job_cancel(job);
                ctrl_c = Box::pin(tokio::signal::ctrl_c());
            }
            _ = tick.tick() => {
                let st = db.copy_status(job)?;
                if let Some(now) = crate::progress::rss_bytes() {
                    peak_rss = Some(peak_rss.map_or(now, |p| p.max(now)));
                }
                if st.state.is_terminal() {
                    break st;
                }
                if st.state == JobState::Warned && !confirm_refused && !plan_changed {
                    match on_warned(db, job, &o)? {
                        Warned::Resumed => {}
                        Warned::Refused => confirm_refused = true,
                        Warned::PlanChanged => plan_changed = true,
                    }
                }
                if o.progress && last_print.elapsed() >= Duration::from_secs(1) {
                    last_print = Instant::now();
                    print_progress(db, &st, started, peak_rss);
                }
            }
        }
    };
    if json {
        match &status.result {
            Some(r) => print_json(r)?,
            None => {
                let err = db.job_get(job).ok().and_then(|j| j.error);
                eprintln!("{}", serde_json::json!({ "error": err, "state": status.state }));
            }
        }
    } else {
        print_summary(&status);
    }
    // A pinned plan that changed is a rejection (6.3), whatever terminal state the cancel produced.
    if plan_changed {
        return Ok(ExitCode::from(EXIT_REJECTED));
    }
    Ok(ExitCode::from(exit_code(&status, confirm_refused)))
}

/// The exit code of a finished copy job (6.3).
pub fn exit_code(st: &CopyStatus, confirm_refused: bool) -> u8 {
    match st.state {
        JobState::Done => match &st.result {
            Some(r) if r.problems > 0 || r.tables.iter().any(|t| t.state == CopyItemState::Failed) => EXIT_PROBLEMS,
            _ => EXIT_DONE,
        },
        JobState::Rejected => EXIT_REJECTED,
        JobState::Cancelled if confirm_refused => EXIT_CONFIRM,
        JobState::Cancelled => EXIT_CANCELLED,
        _ => {
            if st.result.is_some() {
                EXIT_PROBLEMS
            } else {
                EXIT_FAILED
            }
        }
    }
}

/// The job waits for acknowledgements its plan at start needed (live state changed, a
/// resume): `--ack` / `--yes` / the terminal, else cancel (exit 4).
fn on_warned(db: &BulkDb, job: &str, o: &WaitOpts) -> anyhow::Result<Warned> {
    let summary = db.job_get(job)?;
    let report = summary.preflight.unwrap_or_default();
    let needs: Vec<AckNeed> = report
        .warnings
        .iter()
        .map(|w| AckNeed {
            code: w.code.split('#').next().unwrap_or(&w.code).to_string(),
            key: w.code.clone(),
            severity: w.severity,
            items: w.detail.as_deref().map(|d| d.lines().map(str::to_string).collect()).unwrap_or_default(),
        })
        .collect();
    for w in &report.warnings {
        eprintln!("{} [{}]: {}", w.code, enum_name(&w.severity), w.message);
        if let Some(d) = w.detail.as_deref().filter(|_| w.code.starts_with("PLAN_CHANGED")) {
            eprintln!("  {d}");
        }
    }
    // A plan pinned with --expect-plan that changed since review is never acknowledged (not even
    // by --yes): cancel and exit 3 (6.3), so a job nobody reviewed cannot run.
    if o.expect_plan.is_some() && needs.iter().any(|a| a.code == "PLAN_CHANGED") {
        eprintln!("the plan changed since --expect-plan pinned it; refusing to run (review the new plan with `bulk-cli copy plan`)");
        db.job_cancel(job).context("cancelling the copy job")?;
        return Ok(Warned::PlanChanged);
    }
    let keys = tokio::task::block_in_place(|| collect_acks(&needs, &o.acks, o.yes))?;
    match keys {
        Some(k) => {
            db.job_resume(job, k).context("resuming the copy job")?;
            Ok(Warned::Resumed)
        }
        None => {
            db.job_cancel(job).context("cancelling the copy job")?;
            Ok(Warned::Refused)
        }
    }
}

fn print_progress(db: &BulkDb, st: &CopyStatus, started: Instant, peak_rss: Option<u64>) {
    let p = db.progress_snapshot().into_iter().find(|p| p.job_id == st.job_id);
    let active =
        st.progress.tables.iter().find(|t| matches!(t.state, CopyItemState::Loading | CopyItemState::Creating | CopyItemState::Indexing | CopyItemState::Verifying));
    let mut line =
        format!("[{}] {} | tables {}/{}", fmt_clock(started.elapsed().as_secs()), enum_name(&st.progress.phase), st.progress.tables_done, st.progress.tables_total);
    if let Some(p) = p {
        line.push_str(&format!(" | rows {} | {}", human_count(p.rows), human_rate(p.rate_bytes_per_sec)));
        if let Some(eta) = p.eta_sec {
            line.push_str(&format!(" | ETA {}", fmt_clock(eta)));
        }
    }
    if let Some(t) = active {
        line.push_str(&format!(" | {} {}", t.item_id, enum_name(&t.state)));
    }
    if let Some(rss) = peak_rss {
        line.push_str(&format!(" | RSS {}", human_bytes(rss)));
    }
    eprintln!("{line}");
}

fn facts_line(plan: &CopyPlan) -> String {
    let e = &plan.effective;
    let mut parts = vec![format!("load {}", enum_name(&e.load_method)), format!("concurrency {}", e.table_concurrency)];
    parts.push(format!("sessions source {} / target {}", e.source_sessions, e.target_sessions));
    parts.push(format!("snapshot {}", if e.consistent_snapshot { "yes" } else { "no" }));
    if let Some(c) = &e.charset {
        parts.push(format!("charset {c}"));
    }
    if let Some(c) = &e.collation {
        parts.push(format!("collation {c}"));
    }
    if let Some(en) = &e.engine {
        parts.push(format!("engine {en}"));
    }
    if let Some(z) = &e.zero_dates {
        parts.push(format!("zero dates {}", enum_name(z)));
    }
    if let Some(b) = e.chunk_bytes {
        parts.push(format!("chunk {}", human_bytes(b)));
    }
    parts.join(" · ")
}

/// The plan as text (6.3).
pub fn print_plan(plan: &CopyPlan, sql: bool) {
    println!("plan {} ({})", plan.plan_id, enum_name(&plan.family));
    let side = |label: &str, f: &bulk_core::types::CopyServerFacts| {
        let mut s = format!("{label:<7} {} {} · {} · {}", f.driver, f.version, f.database, f.endpoint);
        if let Some(c) = &f.charset {
            s.push_str(&format!(" · {c}"));
        }
        println!("{s}");
        for n in &f.notes {
            println!("        {n}");
        }
    };
    side("source", &plan.source);
    side("target", &plan.target);
    println!("facts   {}", facts_line(plan));
    if plan.family == bulk_core::types::CopyFamily::Mysql {
        println!("        MySQL 6.x and 7.x never shipped as GA (6.0 was a cancelled alpha, 7.x is NDB Cluster numbering)");
    }
    if let Some(r) = &plan.resume_of {
        println!("resume  of run {r}");
    }
    let mut t = Table::new(&[
        ("ORDER", Align::Right),
        ("PHASE", Align::Left),
        ("KIND", Align::Left),
        ("NAME", Align::Left),
        ("TARGET", Align::Left),
        ("ACTION", Align::Left),
        ("EST ROWS", Align::Right),
        ("ISSUES", Align::Left),
    ]);
    for i in &plan.items {
        let mut action = enum_name(&i.action);
        if i.exists_in_target && matches!(i.action, PlanAction::SkipExisting | PlanAction::Truncate | PlanAction::DropRecreate | PlanAction::Append) {
            match i.target_has_rows {
                Some(true) => action.push_str(" (exists · rows)"),
                Some(false) => action.push_str(" (exists · empty)"),
                None => action.push_str(" (exists)"),
            }
        }
        let name = match &i.schema {
            Some(s) => format!("{s}.{}", i.name),
            None => i.name.clone(),
        };
        let target = match &i.target_schema {
            Some(s) => format!("{s}.{}", i.target_name),
            None => i.target_name.clone(),
        };
        t.row(vec![
            i.order.to_string(),
            enum_name(&i.phase),
            enum_name(&i.kind),
            name,
            target,
            action,
            i.est_rows.map(human_count).unwrap_or_else(|| "-".into()),
            i.warnings.iter().map(|w| w.code.clone()).collect::<Vec<_>>().join(","),
        ]);
    }
    t.print();
    let t = &plan.totals;
    println!(
        "totals  {} tables, {} objects: create {}, skip {}, truncate {}, drop+recreate {}, append {}, unsupported {}, excluded {}{}",
        t.tables,
        t.objects,
        t.create,
        t.skip,
        t.truncate,
        t.drop_recreate,
        t.append,
        t.unsupported,
        t.excluded,
        t.est_rows.map(|r| format!(", about {} rows", human_count(r))).unwrap_or_default()
    );
    for w in &plan.warnings {
        println!("{:<7} {} [{}]: {}{}", "issue", w.code, enum_name(&w.severity), w.message, w.item_id.as_deref().map(|i| format!(" ({i})")).unwrap_or_default());
    }
    for b in &plan.blockers {
        println!("{:<7} {} [{}]: {}{}", "BLOCKER", b.code, enum_name(&b.severity), b.message, b.item_id.as_deref().map(|i| format!(" ({i})")).unwrap_or_default());
    }
    if !plan.acks.is_empty() {
        println!("acknowledgements needed (--ack CODE or --yes):");
        for a in &plan.acks {
            println!("  {} [{}]{}", a.code, enum_name(&a.severity), if a.items.is_empty() { String::new() } else { format!(" — {} item(s)", a.items.len()) });
        }
    }
    if sql {
        println!("-- session (source)");
        for s in &plan.session_sql.source {
            println!("{s};");
        }
        println!("-- session (target)");
        for s in &plan.session_sql.target {
            println!("{s};");
        }
        for i in &plan.items {
            if i.target_sql.is_empty() {
                continue;
            }
            println!("-- {} ({})", i.id, enum_name(&i.action));
            for s in &i.target_sql {
                println!("{s};");
            }
        }
    }
}

fn eprint_blockers(plan: &CopyPlan, json: bool) {
    if json {
        eprintln!("{}", serde_json::json!({ "error": { "code": "INVALID", "message": "the copy plan has blockers" }, "blockers": plan.blockers }));
    } else {
        eprintln!("rejected: the plan has {} blocker(s): {}", plan.blockers.len(), plan.blockers.iter().map(|b| b.code.as_str()).collect::<Vec<_>>().join(", "));
    }
}

/// The end of a run as text (6.3).
pub fn print_summary(st: &CopyStatus) {
    let Some(r) = &st.result else {
        println!("copy {}: no result", enum_name(&st.state));
        if let Some(p) = &st.plan {
            for b in &p.blockers {
                println!("  blocker {}: {}", b.code, b.message);
            }
        }
        return;
    };
    print_result(st.state, r);
}

fn print_result(state: JobState, r: &CopyResult) {
    let rows: u64 = r.tables.iter().map(|t| t.rows_written).sum();
    let outcome = match state {
        JobState::Done if r.problems > 0 => "done with problems".to_string(),
        s => enum_name(&s),
    };
    println!(
        "copy {outcome}{}: {} table(s), {} rows in {}",
        if r.dry_run { " (dry run)" } else { "" },
        r.tables.len(),
        human_count(rows),
        fmt_duration_ms(r.duration_ms)
    );
    let mut t = Table::new(&[
        ("TABLE", Align::Left),
        ("STATE", Align::Left),
        ("READ", Align::Right),
        ("WRITTEN", Align::Right),
        ("VERIFY", Align::Left),
        ("PROBLEMS", Align::Left),
    ]);
    for x in &r.tables {
        t.row(vec![x.target_name.clone(), enum_name(&x.state), human_count(x.rows_read), human_count(x.rows_written), enum_name(&x.verify), x.problems.join(",")]);
    }
    t.print();
    for x in &r.tables {
        if let Some(e) = &x.error {
            println!("  {} failed [{}]: {}", x.target_name, e.code, e.message);
        }
        for s in &x.warning_samples {
            println!("  {}: {s}", x.target_name);
        }
        for f in &x.value_fixes {
            println!("  {}.{}: {} x{}", x.target_name, f.column, f.code, f.count);
        }
    }
    for o in r.objects.iter().filter(|o| o.state == CopyItemState::Failed) {
        println!("  {} failed: {}", o.item_id, o.error.as_ref().map(|e| e.message.as_str()).unwrap_or(""));
    }
    if r.skipped_existing > 0 {
        println!("{}개 테이블은 이미 있어 데이터를 복사하지 않았습니다 ({} table(s) already existed; their data was not copied)", r.skipped_existing, r.skipped_existing);
    }
    println!("existing-table policy: {}", enum_name(&r.existing));
    for l in &r.left_behind {
        println!("  left behind: {} {}{}", l.item_id, enum_name(&l.kind), l.rows.map(|n| format!(" ({n} rows)")).unwrap_or_default());
    }
    for w in r.warnings.iter().filter(|w| w.severity != Severity::Info) {
        println!("  warning {}: {}", w.code, w.message);
    }
    if r.verified {
        println!("verified");
    }
    // A done run with problems is resumable too (5.1), so the hint and `run_info` agree.
    if r.resumable && (state != JobState::Done || r.problems > 0) {
        println!("resume with: bulk-cli copy resume --run {}", r.run_id);
    }
}

fn print_runs(runs: &[CopyRunInfo]) {
    let mut t = Table::new(&[
        ("RUN", Align::Left),
        ("LABEL", Align::Left),
        ("STATE", Align::Left),
        ("TABLES", Align::Right),
        ("STARTED", Align::Left),
        ("RESUMABLE", Align::Left),
        ("IN USE", Align::Left),
    ]);
    for r in runs {
        t.row(vec![
            r.run_id.clone(),
            r.label.clone(),
            format!("{}{}", enum_name(&r.state), if r.dry_run { " (dry run)" } else { "" }),
            format!("{}/{} ({} failed)", r.tables_done, r.tables_total, r.tables_failed),
            r.started_at.clone(),
            if r.resumable { "yes".into() } else { "-".into() },
            if r.in_use { "yes".into() } else { "-".into() },
        ]);
    }
    t.print();
}

fn print_detail(d: &CopyRunDetail) {
    print_runs(std::slice::from_ref(&d.info));
    println!(
        "source {}.{} -> target {}.{} · existing {}",
        d.request.source.conn_id,
        d.request.source.database,
        d.request.target.conn_id,
        d.request.target.database,
        enum_name(&d.request.options.existing)
    );
    if let Some(r) = &d.result {
        print_result(d.info.state, r);
    }
}

/// `[schema.]table` -> `TableRef` (schema only when given).
fn table_ref(s: &str) -> TableRef {
    match s.split_once('.') {
        Some((schema, name)) => TableRef::new(Some(schema), name),
        None => TableRef::new(None, s),
    }
}

/// The `CopyRequest` the flags describe, starting from `CopyOptions::default()`.
pub fn build_request(db: &BulkDb, a: &CopySpecArgs) -> anyhow::Result<CopyRequest> {
    let source = find_saved(db, &a.source)?;
    let target = find_saved(db, &a.target)?;

    let mut tables: Vec<TableRef> = a.tables.iter().filter(|t| !t.trim().is_empty()).map(|t| table_ref(t.trim())).collect();
    if let Some(file) = &a.tables_file {
        let text = std::fs::read_to_string(file).with_context(|| format!("cannot read {}", file.display()))?;
        tables.extend(text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')).map(table_ref));
    }
    let selection = if tables.is_empty() { CopySelection::All } else { CopySelection::Tables { tables } };

    // `--exclude-table` / `--existing-for` name tables; `resolved_request` maps them to plan
    // item ids (the planner matches ids exactly).
    let mut o = CopyOptions { content: a.content.into(), existing: a.existing.into(), ..CopyOptions::default() };
    o.exclude = a.exclude.clone();
    o.name_case = if a.lowercase_names { NameCase::Lower } else { NameCase::Keep };
    o.rename = a.rename_schema.iter().map(|(from, to)| NameMapping { kind: RenameKind::Schema, schema: None, from: from.clone(), to: to.clone() }).collect();
    o.rename.extend(a.rename_table.iter().map(|(from, to)| {
        let t = table_ref(from);
        NameMapping { kind: RenameKind::Table, schema: t.schema, from: t.name, to: to.clone() }
    }));
    o.indexes_after_data = !a.indexes_with_tables;
    o.table_concurrency = a.concurrency;
    o.batch_rows = a.batch_rows;
    o.batch_mb = a.batch_mb;
    o.target_lock_timeout_s = a.target_lock_timeout;
    o.verify = a.verify.into();
    o.verify_skipped = a.verify_skipped;
    o.on_unmappable = a.on_unmappable.into();
    o.objects.views = !a.no_views;
    o.objects.routines = !a.no_routines;
    o.objects.triggers = !a.no_triggers;
    o.objects.sequences = !a.no_sequences;
    o.objects.types = !a.no_types;
    o.objects.extensions = !a.no_extensions;
    o.objects.comments = !a.no_comments;
    o.objects.owners = a.owners;
    o.objects.grants = a.grants;
    o.objects.analyze = !a.no_analyze;
    o.objects.refresh_matviews = !a.no_refresh;
    o.source_session.work_mem = a.work_mem.clone();
    o.source_session.lock_timeout_ms = a.lock_timeout;
    o.source_session.fetch_batch_rows = a.fetch_batch;
    o.mysql.charset = a.charset.clone();
    o.mysql.collation = a.collation.clone();
    o.mysql.engine = a.engine.into();
    o.mysql.zero_dates = a.zero_dates.into();
    o.mysql.time_zone = a.time_zone.clone();
    o.mysql.fix_row_size = !a.no_fix_row_size;
    o.mysql.keep_definer = a.keep_definer;
    o.mysql.enum_error = a.enum_error.map(Into::into);
    for (table, cs) in &a.table_charset {
        o.mysql.table_overrides.push(MysqlTableOverride { table: table.clone(), charset: Some(cs.clone()), collation: None });
    }
    for (table, coll) in &a.table_collation {
        match o.mysql.table_overrides.iter_mut().find(|t| &t.table == table) {
            Some(t) => t.collation = Some(coll.clone()),
            None => o.mysql.table_overrides.push(MysqlTableOverride { table: table.clone(), charset: None, collation: Some(coll.clone()) }),
        }
    }
    o.mysql.columns = a.column_as.iter().map(|c| MysqlColumnOverride { table: c.table.clone(), column: c.column.clone(), treat_as: c.treat_as }).collect();
    o.pg.collation_fallback = a.pg_collation_fallback;
    o.pg.source_encoding = a.pg_source_encoding.map(Into::into);

    Ok(CopyRequest {
        source: CopyEndpoint {
            conn_id: source.id,
            database: a.source_db.clone(),
            schema: a.source_schema.clone(),
            password: a.source_password_env.as_deref().map(password_from_env).transpose()?,
        },
        target: CopyEndpoint {
            conn_id: target.id,
            database: a.target_db.clone(),
            schema: a.target_schema.clone(),
            password: a.target_password_env.as_deref().map(password_from_env).transpose()?,
        },
        selection,
        options: o,
        name: a.name.clone(),
    })
}

/// [`build_request`] with `--exclude-table` / `--existing-for` resolved to the plan's table
/// item ids, and `--exclude` ids checked against the plan: one plan without them first. An
/// unknown or ambiguous name is an error before anything starts (the planner matches ids
/// exactly and would ignore it, so a table meant to be left out or protected would be
/// copied, truncated or replaced).
pub async fn resolved_request(db: &Arc<BulkDb>, a: &CopySpecArgs) -> anyhow::Result<CopyRequest> {
    let mut req = build_request(db, a)?;
    if a.exclude.is_empty() && a.exclude_table.is_empty() && a.existing_for.is_empty() {
        return Ok(req);
    }
    let plan = plan_with_ctrl_c(db, &req).await?;
    for id in &a.exclude {
        if !plan.items.iter().any(|i| &i.id == id) {
            anyhow::bail!("--exclude {id}: no plan item has this id (see `bulk-cli copy plan`)");
        }
    }
    for t in &a.exclude_table {
        let id = resolve_table(&plan, &req.source.database, t).map_err(|e| anyhow::anyhow!("--exclude-table {t}: {e}"))?;
        if !req.options.exclude.contains(&id) {
            req.options.exclude.push(id);
        }
    }
    for (t, policy) in &a.existing_for {
        let id = resolve_table(&plan, &req.source.database, t).map_err(|e| anyhow::anyhow!("--existing-for {t}: {e}"))?;
        req.options.existing_overrides.retain(|o| o.item_id != id);
        req.options.existing_overrides.push(ItemPolicy { item_id: id, policy: *policy });
    }
    Ok(req)
}

/// The id of the plan's table item named `[schema.]table`. PostgreSQL: the schema narrows the
/// match and is needed when the name is in several schemas. MySQL and SQLite tables have no
/// schema (a MySQL prefix equal to the source database is accepted). Exact names first, then
/// a unique case-insensitive match.
pub fn resolve_table(plan: &CopyPlan, source_database: &str, spec: &str) -> Result<String, String> {
    let tables: Vec<&bulk_core::types::PlanItem> = plan.items.iter().filter(|i| i.kind == bulk_core::types::CopyObjectKind::Table).collect();
    let pick = |name: &str, schema: Option<&str>| -> Result<Option<String>, String> {
        for exact in [true, false] {
            let eq = |a: &str, b: &str| if exact { a == b } else { a.eq_ignore_ascii_case(b) };
            let found: Vec<&&bulk_core::types::PlanItem> =
                tables.iter().filter(|i| eq(&i.name, name) && schema.is_none_or(|s| i.schema.as_deref().is_some_and(|x| eq(x, s)))).collect();
            match found.len() {
                0 => continue,
                1 => return Ok(Some(found[0].id.clone())),
                _ => {
                    let names: Vec<String> = found.iter().map(|i| format!("{}.{}", i.schema.as_deref().unwrap_or(""), i.name)).collect();
                    return Err(format!("the name is ambiguous ({}); give the schema as schema.table", names.join(", ")));
                }
            }
        }
        Ok(None)
    };
    if plan.family == bulk_core::types::CopyFamily::Postgres {
        let t = table_ref(spec);
        return pick(&t.name, t.schema.as_deref())?.ok_or_else(|| format!("no table {spec} in the plan"));
    }
    // MySQL / SQLite: no schema; a name may itself contain a dot.
    if let Some(id) = pick(spec, None)? {
        return Ok(id);
    }
    let t = table_ref(spec);
    match t.schema.as_deref() {
        Some(db) if plan.family == bulk_core::types::CopyFamily::Mysql && db.eq_ignore_ascii_case(source_database) => {
            pick(&t.name, None)?.ok_or_else(|| format!("no table {} in the plan", t.name))
        }
        Some(db) => Err(format!("{} tables have no schema; '{db}' is not the source database: write the table name only", enum_name(&plan.family))),
        None => Err(format!("no table {spec} in the plan")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bulk_core::types::{CopyFamily, CopyObjectKind, PlanItem};

    fn table(schema: Option<&str>, name: &str, id: &str) -> PlanItem {
        PlanItem {
            id: id.into(),
            kind: CopyObjectKind::Table,
            parent: None,
            schema: schema.map(str::to_string),
            name: name.into(),
            target_schema: schema.map(str::to_string),
            target_name: name.into(),
            action: PlanAction::Create,
            phase: bulk_core::types::CopyPhase::PreData,
            order: 0,
            exists_in_target: false,
            target_has_rows: None,
            target_est_rows: None,
            has_data: true,
            est_rows: None,
            est_bytes: None,
            source_ddl: None,
            target_sql: vec![],
            warnings: vec![],
            depends_on: vec![],
        }
    }

    fn plan(family: CopyFamily, items: Vec<PlanItem>) -> CopyPlan {
        let mut p = bulk_core::copy::testkit::empty_plan(family);
        p.items = items;
        p
    }

    /// PostgreSQL ids carry the schema: a bare name resolves when it is unique, needs the
    /// schema when it is not, and an unknown name is an error (never silently ignored).
    #[test]
    fn pg_table_names_resolve_to_schema_qualified_ids() {
        let p = plan(
            CopyFamily::Postgres,
            vec![
                table(Some("app"), "orders", "table:\"app\".\"orders\""),
                table(Some("app"), "items", "table:\"app\".\"items\""),
                table(Some("archive"), "items", "table:\"archive\".\"items\""),
            ],
        );
        assert_eq!(resolve_table(&p, "shop", "orders").unwrap(), "table:\"app\".\"orders\"");
        assert_eq!(resolve_table(&p, "shop", "app.orders").unwrap(), "table:\"app\".\"orders\"");
        assert_eq!(resolve_table(&p, "shop", "ORDERS").unwrap(), "table:\"app\".\"orders\"", "unique case-insensitive match");
        assert_eq!(resolve_table(&p, "shop", "archive.items").unwrap(), "table:\"archive\".\"items\"");
        assert!(resolve_table(&p, "shop", "items").unwrap_err().contains("ambiguous"));
        assert!(resolve_table(&p, "shop", "public.orders").unwrap_err().contains("no table"));
        assert!(resolve_table(&p, "shop", "customers").unwrap_err().contains("no table"));
    }

    /// MySQL / SQLite ids have no schema: a prefix naming the source database is dropped,
    /// any other prefix is refused.
    #[test]
    fn mysql_and_sqlite_table_names() {
        let p = plan(CopyFamily::Mysql, vec![table(None, "orders", "table:\"orders\""), table(None, "a.b", "table:\"a.b\"")]);
        assert_eq!(resolve_table(&p, "shop", "orders").unwrap(), "table:\"orders\"");
        assert_eq!(resolve_table(&p, "shop", "shop.orders").unwrap(), "table:\"orders\"");
        assert_eq!(resolve_table(&p, "shop", "a.b").unwrap(), "table:\"a.b\"", "a dotted name");
        assert!(resolve_table(&p, "shop", "other.orders").unwrap_err().contains("no schema"));
        assert!(resolve_table(&p, "shop", "nope").is_err());
        let s = plan(CopyFamily::Sqlite, vec![table(None, "customers", "table:\"customers\"")]);
        assert_eq!(resolve_table(&s, "main", "customers").unwrap(), "table:\"customers\"");
        assert!(resolve_table(&s, "main", "main.customers").is_err());
    }
}
