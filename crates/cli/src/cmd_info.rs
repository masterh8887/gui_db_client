//! `probe`, `explain`, `check`, `info`, `drivers`, `jobs` and `diagnostics`: commands that print
//! what the core knows about a server, a query plan, a query's shape, this installation, this
//! machine's drivers and this process's jobs, or package it as a support bundle.

use std::process::ExitCode;
use std::sync::Arc;

use anyhow::Context;
use bulk_core::sqlguard::SqlCheck;
use bulk_core::types::{AppInfo, Connection, DiagnosticsBundle, DriverInfo, JobSummary, PlanNode, PlanSummary, PlanWarning, ProbeResult};
use bulk_core::BulkDb;

use crate::args::{CheckArgs, DiagnosticsArgs, DriversArgs, ExplainArgs, ProbeArgs};
use crate::cmd_export::{read_sql_source, sql_text};
use crate::connect;
use crate::output::{enum_name, fmt_duration_ms, human_bytes, human_count, kv, opt, opt_count, print_json, severity_tag, yes_no, Align, Table};
use crate::{EXIT_DONE, EXIT_REJECTED};

// ---------------------------------------------------------------------------------------------
// probe
// ---------------------------------------------------------------------------------------------

pub async fn probe(db: &Arc<BulkDb>, args: ProbeArgs, json: bool) -> anyhow::Result<ExitCode> {
    let handle = connect::resolve(db, &args.target).await?;
    let result = db.conn_test(handle.id(), handle.password().map(str::to_string)).await?;
    print_probe(handle.conn(), &result, json)?;
    Ok(ExitCode::SUCCESS)
}

/// Shared by `probe` and `conn test`.
pub fn print_probe(conn: &Connection, r: &ProbeResult, json: bool) -> anyhow::Result<()> {
    if json {
        return print_json(r);
    }
    let s = &r.server;
    kv("Connection", format!("{} ({})", conn.name, conn.id));
    kv("Driver", &s.driver);
    kv("Server", &s.version);
    kv("User", opt(&s.user));
    kv("Database", opt(&s.database));
    kv(
        "Server RAM",
        match (s.ram_bytes, &s.ram_source) {
            (Some(b), Some(src)) => format!("{} (estimated: {src})", human_bytes(b)),
            (Some(b), None) => format!("{} (estimated)", human_bytes(b)),
            _ => "unknown".to_string(),
        },
    );
    kv(
        "Can write",
        match s.can_write {
            Some(true) => "yes  (warning: a read-only account is recommended for extraction)".to_string(),
            Some(false) => "no".to_string(),
            None => "unknown".to_string(),
        },
    );
    kv("Tunnel", yes_no(r.tunnel_used));
    kv("Elapsed", format!("{}{}", fmt_duration_ms(r.elapsed_ms), if r.steps.is_empty() { String::new() } else { format!("  [{}]", r.steps.join(" -> ")) }));
    if !s.settings.is_empty() {
        println!("Server settings");
        for (k, v) in &s.settings {
            println!("  {k:<36}{v}");
        }
    }
    println!("Recommended session");
    let rec = &r.recommended;
    let mut any = false;
    if let Some(w) = &rec.work_mem {
        println!("  {:<36}{w}", "work_mem");
        any = true;
    }
    if let Some(t) = rec.statement_timeout_ms {
        println!("  {:<36}{}", "statement_timeout", fmt_duration_ms(t));
        any = true;
    }
    if let Some(t) = rec.lock_timeout_ms {
        println!("  {:<36}{}", "lock_timeout", fmt_duration_ms(t));
        any = true;
    }
    if let Some(p) = rec.max_parallel_workers {
        println!("  {:<36}{p}", "max_parallel_workers_per_gather");
        any = true;
    }
    if let Some(f) = rec.fetch_batch_rows {
        println!("  {:<36}{}", "fetch_batch_rows", human_count(f as u64));
        any = true;
    }
    if !any {
        println!("  (driver defaults)");
    }
    if !s.notes.is_empty() {
        println!("Notes");
        for n in &s.notes {
            println!("  - {n}");
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// explain
// ---------------------------------------------------------------------------------------------

pub async fn explain(db: &Arc<BulkDb>, args: ExplainArgs, json: bool) -> anyhow::Result<ExitCode> {
    let src = read_sql_source(args.sql.as_deref(), args.query.as_deref())?;
    let sql = sql_text(db, &src)?;
    let handle = connect::resolve(db, &args.target).await?;
    let plan = db.query_explain(handle.id(), &sql, args.analyze).await.context("explain")?;
    if json {
        print_json(&plan)?;
    } else if args.raw {
        println!("{}", plan.raw.trim_end());
    } else {
        print_plan(&plan);
    }
    Ok(ExitCode::SUCCESS)
}

pub fn print_plan(p: &PlanSummary) {
    kv("Estimated rows", opt_count(&p.estimated_rows));
    kv("Estimated width", p.estimated_width.map(|w| format!("{w} B/row")).unwrap_or_else(|| "-".into()));
    kv("Estimated bytes", p.estimated_bytes.map(human_bytes).unwrap_or_else(|| "-".into()));
    kv("Total cost", p.total_cost.map(|c| format!("{c:.2}")).unwrap_or_else(|| "-".into()));
    kv("Analyzed", yes_no(p.analyzed));
    kv("Elapsed", fmt_duration_ms(p.elapsed_ms));
    if !p.warnings.is_empty() {
        println!("Warnings");
        for w in &p.warnings {
            print_plan_warning(w);
        }
    }
    if !p.tables.is_empty() {
        println!("Tables");
        println!("  {}", p.tables.iter().map(|t| t.qualified()).collect::<Vec<_>>().join(", "));
    }
    match &p.root {
        Some(root) => {
            println!("Plan");
            print_node(root, 1, p.analyzed);
        }
        None if !p.raw.trim().is_empty() => {
            println!("Plan ({})", p.raw_format);
            for line in p.raw.trim_end().lines() {
                println!("  {line}");
            }
        }
        None => println!("Plan: not available for this driver"),
    }
}

fn print_plan_warning(w: &PlanWarning) {
    println!("  [{}] {}: {}", severity_tag(w.severity), w.code, w.message);
    if let Some(d) = w.detail.as_deref().filter(|d| !d.trim().is_empty()) {
        println!("           {}", d.trim().replace('\n', "\n           "));
    }
}

fn print_node(n: &PlanNode, depth: usize, analyzed: bool) {
    let indent = "  ".repeat(depth);
    let mut head = n.node_type.clone();
    if let Some(r) = &n.relation {
        head.push_str(&format!(" on {}", r.qualified()));
        if let Some(a) = n.alias.as_deref().filter(|a| *a != r.name) {
            head.push_str(&format!(" {a}"));
        }
    }
    if let Some(rel) = &n.parent_relationship {
        head = format!("{rel}: {head}");
    }
    let mut facts: Vec<String> = Vec::new();
    if let Some(r) = n.plan_rows {
        facts.push(format!("rows={}", human_count(r.round().max(0.0) as u64)));
    }
    if let Some(w) = n.plan_width {
        facts.push(format!("width={w}"));
    }
    match (n.startup_cost, n.total_cost) {
        (Some(s), Some(t)) => facts.push(format!("cost={s:.2}..{t:.2}")),
        (None, Some(t)) => facts.push(format!("cost={t:.2}")),
        _ => {}
    }
    if analyzed {
        if let Some(a) = n.actual_rows {
            facts.push(format!("actual_rows={}", human_count(a.round().max(0.0) as u64)));
        }
        if let Some(l) = n.actual_loops {
            facts.push(format!("loops={l:.0}"));
        }
        if let Some(t) = n.actual_time_ms {
            facts.push(format!("time={t:.1}ms"));
        }
    }
    if facts.is_empty() {
        println!("{indent}-> {head}");
    } else {
        println!("{indent}-> {head}  ({})", facts.join(" "));
    }
    for (k, v) in &n.extra {
        println!("{indent}     {k}: {v}");
    }
    for c in &n.children {
        print_node(c, depth + 1, analyzed);
    }
}

// ---------------------------------------------------------------------------------------------
// check / info
// ---------------------------------------------------------------------------------------------

/// `check`: the extraction SQL guard (`BulkDb::sql_check`), with `--conn`'s dialect when given.
/// Exit 0 when the query is a single read-only statement, 3 (rejected) otherwise.
pub fn check(db: &BulkDb, args: CheckArgs, json: bool) -> anyhow::Result<ExitCode> {
    let src = read_sql_source(args.sql.as_deref(), args.query.as_deref())?;
    let sql = sql_text(db, &src)?;
    let conn = args.conn.as_deref().map(|key| connect::find_saved(db, key)).transpose()?;
    let result = db.sql_check(conn.as_ref().map(|c| c.id.as_str()), &sql)?;
    if json {
        print_json(&result)?;
    } else {
        print_check(conn.as_ref(), &result);
    }
    Ok(ExitCode::from(if result.ok { EXIT_DONE } else { EXIT_REJECTED }))
}

fn print_check(conn: Option<&Connection>, r: &SqlCheck) {
    kv("Dialect", conn.map(|c| format!("{} (connection '{}')", c.driver, c.name)).unwrap_or_else(|| "standard SQL".into()));
    kv("Statements", r.statements.to_string());
    kv("Kind", enum_name(&r.kind));
    if r.ok {
        kv("Result", "ok: a single read-only statement");
        return;
    }
    kv("Result", "rejected");
    println!("Issues");
    for i in &r.issues {
        println!("  [{}] {}: {}", severity_tag(i.severity), i.code, i.message);
        for extra in [i.detail.as_deref(), i.recommendation.as_deref()].into_iter().flatten().filter(|s| !s.trim().is_empty()) {
            println!("           {}", extra.trim());
        }
    }
}

/// `info`: version, platform and where config and logs live (`BulkDb::app_info`).
pub fn info(db: &BulkDb, json: bool) -> anyhow::Result<ExitCode> {
    let info: AppInfo = db.app_info(env!("CARGO_PKG_VERSION"));
    if json {
        print_json(&info)?;
    } else {
        kv("Version", &info.version);
        kv("Platform", format!("{} ({})", info.platform, info.arch));
        kv("Config dir", &info.config_dir);
        kv("Log dir", &info.log_dir);
    }
    Ok(ExitCode::SUCCESS)
}

// ---------------------------------------------------------------------------------------------
// drivers / jobs
// ---------------------------------------------------------------------------------------------

pub fn drivers(db: &BulkDb, args: DriversArgs, json: bool) -> anyhow::Result<ExitCode> {
    let list: Vec<DriverInfo> = if args.rescan { db.drivers_rescan() } else { db.drivers_list() };
    if json {
        print_json(&list)?;
        return Ok(ExitCode::SUCCESS);
    }
    let mut t = Table::new(&[
        ("ID", Align::Left),
        ("NAME", Align::Left),
        ("TIER", Align::Left),
        ("AVAILABLE", Align::Left),
        ("DETAIL", Align::Left),
        ("ODBC DRIVERS", Align::Left),
    ]);
    for d in &list {
        let detail = if d.available {
            d.library_path.clone().unwrap_or_default()
        } else {
            let mut s = d.detail.clone().unwrap_or_else(|| "not detected".into());
            if let Some(h) = &d.install_hint {
                s.push_str(&format!(" (install: {h})"));
            }
            if let Some(u) = &d.install_url {
                s.push_str(&format!(" {u}"));
            }
            s
        };
        t.row(vec![d.id.clone(), d.name.clone(), enum_name(&d.tier), yes_no(d.available).into(), detail, d.odbc_drivers.join(", ")]);
    }
    t.print();
    Ok(ExitCode::SUCCESS)
}

pub fn jobs(db: &BulkDb, json: bool) -> anyhow::Result<ExitCode> {
    let list: Vec<JobSummary> = db.job_list();
    if json {
        print_json(&list)?;
        return Ok(ExitCode::SUCCESS);
    }
    let mut t = Table::new(&[
        ("ID", Align::Left),
        ("KIND", Align::Left),
        ("STATE", Align::Left),
        ("CONN", Align::Left),
        ("LABEL", Align::Left),
        ("STARTED", Align::Left),
        ("FINISHED", Align::Left),
        ("ROWS", Align::Right),
        ("ERROR", Align::Left),
    ]);
    for j in &list {
        t.row(vec![
            j.id.clone(),
            enum_name(&j.kind),
            enum_name(&j.state),
            j.conn_name.clone(),
            j.label.clone(),
            j.started_at.clone(),
            opt(&j.finished_at),
            j.result.as_ref().map(|r| human_count(r.rows)).or_else(|| j.progress.as_ref().map(|p| human_count(p.rows))).unwrap_or_else(|| "-".into()),
            j.error.as_ref().map(|e| format!("{}: {}", e.code, e.message)).unwrap_or_default(),
        ]);
    }
    t.print();
    Ok(ExitCode::SUCCESS)
}

// ---------------------------------------------------------------------------------------------
// diagnostics
// ---------------------------------------------------------------------------------------------

/// Write the support bundle (`BulkDb::diagnostics_bundle`) and report where it went.
pub fn diagnostics(db: &BulkDb, args: DiagnosticsArgs, json: bool) -> anyhow::Result<ExitCode> {
    let dir = connect::path_string(&args.out);
    let bundle: DiagnosticsBundle = db.diagnostics_bundle(&dir).context("writing the diagnostics bundle")?;
    if json {
        print_json(&bundle)?;
        return Ok(ExitCode::SUCCESS);
    }
    println!("wrote {} ({})", bundle.path, human_bytes(bundle.size_bytes));
    for name in &bundle.included {
        println!("  {name}");
    }
    eprintln!("note: the bundle holds logs, settings and connection definitions (hosts, users) but no passwords or data; review it before sharing");
    Ok(ExitCode::SUCCESS)
}
