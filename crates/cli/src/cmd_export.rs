//! `export`: preflight -> acknowledge -> `export_start` -> wait for `job:done`, with the
//! progress line and the summary. Also the pure [`build_request`] mapping from flags to
//! [`ExportRequest`], which is unit tested.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use anyhow::{anyhow, bail, Context};
use bulk_core::types::{
    Compression, Connection, CsvOpts, ExportRequest, JobState, OutputFormat, OutputOpts, ParquetOpts, PreflightReport, SessionOpts, Settings, SplitOpts,
};
use bulk_core::BulkDb;

use crate::args::ExportArgs;
use crate::connect::{self, path_string};
use crate::output::{enum_name, eprint_issues, fmt_duration_ms, human_bytes, human_count, human_rate, opt_bytes, opt_count, print_json};
use crate::progress::{wait_for_job, WaitOptions, WaitOutcome};
use crate::prompts;
use crate::{EXIT_CANCELLED, EXIT_CONFIRM, EXIT_DONE, EXIT_FAILED, EXIT_REJECTED};

/// Where the query text comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SqlSource {
    /// Inline text (`--query`, or `--query -` read from stdin).
    Inline(String),
    /// `--sql FILE`: the core reads it at execution time.
    File(String),
}

/// Resolve `--sql` / `--query` (reading stdin for `-`).
pub fn read_sql_source(sql: Option<&Path>, query: Option<&str>) -> anyhow::Result<SqlSource> {
    match (sql, query) {
        (Some(path), None) => {
            if !path.is_file() {
                bail!("SQL file not found: {}", path.display());
            }
            Ok(SqlSource::File(path_string(path)))
        }
        (None, Some("-")) => {
            let text = std::io::read_to_string(std::io::stdin()).context("reading the query from stdin")?;
            if text.trim().is_empty() {
                bail!("no query on stdin");
            }
            Ok(SqlSource::Inline(text))
        }
        (None, Some(q)) => {
            if q.trim().is_empty() {
                bail!("--query is empty");
            }
            Ok(SqlSource::Inline(q.to_string()))
        }
        _ => bail!("pass exactly one of --sql FILE or --query SQL"),
    }
}

/// The query text, reading `--sql` files through the core (`explain` needs the text).
pub fn sql_text(db: &BulkDb, src: &SqlSource) -> anyhow::Result<String> {
    match src {
        SqlSource::Inline(s) => Ok(s.clone()),
        SqlSource::File(p) => Ok(db.sql_file_read(p).with_context(|| format!("reading {p}"))?.content),
    }
}

/// What an `--out PATH` says about the destination.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutTarget {
    pub dir: String,
    pub stem: String,
    pub format: Option<OutputFormat>,
    pub compress: Option<Compression>,
    pub tsv: bool,
}

/// Split `--out` into directory, stem and the format / compression implied by the extension
/// chain (`.csv`, `.tsv`, `.parquet`, then `.gz` / `.zst`). Unknown extensions stay in the stem.
pub fn parse_out_path(out: &Path) -> OutTarget {
    let file = out.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default();
    let parent = out.parent().filter(|p| !p.as_os_str().is_empty()).map(|p| p.to_path_buf()).unwrap_or_else(|| PathBuf::from("."));
    let mut stem = file.clone();
    let mut compress = None;
    let lower = stem.to_ascii_lowercase();
    if lower.ends_with(".gz") {
        compress = Some(Compression::Gzip);
        stem.truncate(stem.len() - 3);
    } else if lower.ends_with(".zst") {
        compress = Some(Compression::Zstd);
        stem.truncate(stem.len() - 4);
    }
    let lower = stem.to_ascii_lowercase();
    let (format, tsv) = if lower.ends_with(".csv") {
        stem.truncate(stem.len() - 4);
        (Some(OutputFormat::Csv), false)
    } else if lower.ends_with(".tsv") {
        stem.truncate(stem.len() - 4);
        (Some(OutputFormat::Csv), true)
    } else if lower.ends_with(".parquet") {
        stem.truncate(stem.len() - 8);
        (Some(OutputFormat::Parquet), false)
    } else {
        (None, false)
    };
    if stem.is_empty() {
        stem = file;
    }
    OutTarget { dir: path_string(&parent), stem, format, compress, tsv }
}

fn parse_delimiter(s: &str) -> anyhow::Result<String> {
    let d = match s {
        "\\t" | "tab" | "TAB" => "\t".to_string(),
        other => other.to_string(),
    };
    if d.chars().count() != 1 {
        bail!("--delimiter must be a single character, `\\t` or `tab` (got '{s}')");
    }
    Ok(d)
}

/// Map the flags onto an [`ExportRequest`]. Pure: no I/O, no database.
/// JSON of a request for the debug log, with the password removed before serialising: the
/// log masking cannot be relied on for secrets (a JSON-escaped quote ends its match early).
fn loggable_request(req: &ExportRequest) -> String {
    let mut shown = req.clone();
    if shown.password.is_some() {
        shown.password = Some("***".into());
    }
    serde_json::to_string(&shown).unwrap_or_default()
}

pub fn build_request(args: &ExportArgs, conn: &Connection, settings: &Settings, sql: &SqlSource, password: Option<String>) -> anyhow::Result<ExportRequest> {
    let default_pattern = OutputOpts::default().name_pattern;
    let (dir, name_pattern, label, hinted_format, hinted_compress, tsv) = match (&args.out, &args.dir) {
        (Some(out), _) => {
            let t = parse_out_path(out);
            let label = args.name.clone().unwrap_or_else(|| t.stem.clone());
            (t.dir, t.stem, label, t.format, t.compress, t.tsv)
        }
        (None, Some(dir)) => (path_string(dir), args.pattern.clone().unwrap_or(default_pattern), args.name.clone().unwrap_or_else(|| "export".into()), None, None, false),
        (None, None) => {
            let dir = conn
                .output_dir
                .clone()
                .filter(|d| !d.is_empty())
                .or_else(|| settings.default_output_dir.clone().filter(|d| !d.is_empty()))
                .ok_or_else(|| anyhow!("no output location: pass --out FILE or --dir DIR (or set an output directory on the connection)"))?;
            (dir, args.pattern.clone().unwrap_or(default_pattern), args.name.clone().unwrap_or_else(|| "export".into()), None, None, false)
        }
    };

    let format: OutputFormat = args.format.map(Into::into).or(hinted_format).unwrap_or(OutputFormat::Csv);
    if args.excel && format != OutputFormat::Csv {
        bail!("--excel is a CSV preset; it does not apply to parquet");
    }
    let compress = match args.compress {
        Some(c) => c.to_core(),
        None => hinted_compress,
    };
    if format == OutputFormat::Parquet && compress.is_some() {
        bail!("--compress / .gz / .zst apply to CSV only; parquet uses --parquet-compression");
    }

    let csv = if format == OutputFormat::Csv {
        let delimiter = match &args.delimiter {
            Some(d) => parse_delimiter(d)?,
            None if tsv => "\t".to_string(),
            None => ",".to_string(),
        };
        let crlf = if args.crlf {
            true
        } else if args.lf {
            false
        } else {
            args.excel || settings.csv_crlf_default
        };
        Some(CsvOpts { header: !args.no_header, delimiter, null_as: args.null.clone().unwrap_or_default(), bom: args.bom || args.excel, crlf })
    } else {
        None
    };
    let parquet = if format == OutputFormat::Parquet {
        Some(ParquetOpts { compression: args.parquet_compression.map(Into::into).unwrap_or_default(), row_group_mb: args.row_group_mb.unwrap_or(128) })
    } else {
        None
    };

    let split_rows = args.split_rows.or(if args.excel { Some(1_000_000) } else { None });
    let split = if split_rows.is_some() || args.split_mb.is_some() { Some(SplitOpts { rows: split_rows, megabytes: args.split_mb }) } else { None };

    let base = &conn.session;
    let session = SessionOpts {
        work_mem: args.work_mem.clone().or_else(|| base.work_mem.clone()),
        statement_timeout_ms: args.statement_timeout.or(base.statement_timeout_ms),
        lock_timeout_ms: args.lock_timeout.or(base.lock_timeout_ms),
        max_parallel_workers: args.parallel_workers.or(base.max_parallel_workers),
        fetch_batch_rows: args.fetch_batch.or(base.fetch_batch_rows),
    };

    let (sql_text, sql_file) = match sql {
        SqlSource::Inline(s) => (Some(s.clone()), None),
        SqlSource::File(p) => (None, Some(p.clone())),
    };

    Ok(ExportRequest {
        conn_id: conn.id.clone(),
        sql: sql_text,
        sql_file,
        name: Some(label),
        session,
        output: OutputOpts { dir, name_pattern, format, csv, parquet, compress, split, overwrite: args.overwrite },
        password,
    })
}

/// Print the preflight estimates and issues to stderr.
fn eprint_preflight(report: &PreflightReport) {
    let e = &report.estimates;
    let mut parts: Vec<String> = Vec::new();
    if let Some(r) = e.rows {
        parts.push(format!("~{} rows", human_count(r)));
    }
    if let Some(b) = e.bytes {
        parts.push(format!("~{}", human_bytes(b)));
    }
    if let Some(f) = e.files {
        parts.push(format!("{f} file(s)"));
    }
    if let Some(d) = e.disk_free_bytes {
        parts.push(format!("disk free {}", human_bytes(d)));
    }
    if let Some(w) = &e.work_mem {
        parts.push(format!("work_mem {w}"));
    }
    eprintln!(
        "preflight ({}): {} -> {}",
        fmt_duration_ms(report.elapsed_ms),
        if parts.is_empty() { "no estimates".to_string() } else { parts.join(", ") },
        e.output_path
    );
    for s in report.sessions.iter().filter(|s| s.blocking) {
        eprintln!("  blocking session {} ({}) on {}", s.pid, s.user.as_deref().unwrap_or("?"), s.tables.join(", "));
    }
    eprint_issues("Blockers", &report.blockers);
    eprint_issues("Warnings", &report.warnings);
}

/// Exit code for a finished job.
fn exit_for(outcome: &WaitOutcome) -> ExitCode {
    if outcome.confirm_refused {
        return ExitCode::from(EXIT_CONFIRM);
    }
    ExitCode::from(match outcome.done.state {
        JobState::Done => EXIT_DONE,
        JobState::Cancelled => EXIT_CANCELLED,
        JobState::Rejected => EXIT_REJECTED,
        _ => EXIT_FAILED,
    })
}

pub async fn run(db: &Arc<BulkDb>, args: ExportArgs, json: bool) -> anyhow::Result<ExitCode> {
    let sql = read_sql_source(args.sql.as_deref(), args.query.as_deref())?;
    let handle = connect::resolve(db, &args.target).await?;
    let settings = db.settings_get();
    let req = build_request(&args, handle.conn(), &settings, &sql, handle.password().map(str::to_string))?;
    tracing::debug!("export request: {}", loggable_request(&req));

    // Subscribe before starting so no state / done event can be missed.
    let mut events = db.subscribe();

    let report = db.export_preflight(&req).await.context("preflight")?;
    if !json {
        eprint_preflight(&report);
    }
    if !report.blockers.is_empty() {
        if json {
            print_json(&serde_json::json!({ "state": "rejected", "preflight": report }))?;
        } else {
            eprintln!("rejected: {} blocker(s)", report.blockers.len());
        }
        return Ok(ExitCode::from(EXIT_REJECTED));
    }
    let acknowledged: Vec<String> = report.warnings.iter().map(|w| w.code.clone()).collect();
    if !acknowledged.is_empty() && !args.yes {
        if json {
            eprint_issues("Warnings", &report.warnings);
        }
        let accepted = if prompts::is_interactive() {
            prompts::confirm(&format!("Proceed despite {} warning(s)? [y/N] ", acknowledged.len()))?
        } else {
            eprintln!("preflight warnings need acknowledgement and stdin is not a terminal: re-run with --yes");
            false
        };
        if !accepted {
            if json {
                print_json(&serde_json::json!({ "state": "needs_confirmation", "preflight": report }))?;
            }
            return Ok(ExitCode::from(EXIT_CONFIRM));
        }
    }

    let job_id = db.export_start(req, acknowledged).await.context("starting the export")?;
    tracing::info!("job {job_id} started");
    let outcome = wait_for_job(db, &mut events, &job_id, WaitOptions { progress: args.progress, auto_ack: args.yes }).await?;

    let elapsed_ms = outcome.elapsed.as_millis() as u64;
    let rows_so_far = db.job_get(&job_id).ok().and_then(|j| j.progress).map(|p| p.rows);
    if json {
        let avg = outcome.done.result.as_ref().map(|r| r.bytes as f64 / (r.duration_ms.max(1) as f64 / 1000.0));
        print_json(&serde_json::json!({
            "jobId": job_id,
            "state": outcome.done.state,
            "result": outcome.done.result,
            "error": outcome.done.error,
            "preflight": report,
            "elapsedMs": elapsed_ms,
            "avgBytesPerSec": avg,
            "peakRssBytes": outcome.peak_rss,
            "interrupted": outcome.interrupted,
            "rows": rows_so_far,
        }))?;
    } else {
        print_summary(&outcome, rows_so_far);
    }
    drop(handle);
    Ok(exit_for(&outcome))
}

fn print_summary(outcome: &WaitOutcome, rows_so_far: Option<u64>) {
    let peak = outcome.peak_rss.map(human_bytes).unwrap_or_else(|| "n/a".into());
    match (&outcome.done.state, &outcome.done.result, &outcome.done.error) {
        (JobState::Done, Some(r), _) => {
            let avg = human_rate(r.bytes as f64 / (r.duration_ms.max(1) as f64 / 1000.0));
            println!(
                "done: {} rows, {}, {} file(s), {} ({}), peak RSS {}",
                human_count(r.rows),
                human_bytes(r.bytes),
                r.files.len(),
                fmt_duration_ms(r.duration_ms),
                avg,
                peak
            );
            for f in &r.files {
                println!("  {f}");
            }
            if let Some(v) = r.verified_rows {
                println!("  verified rows: {}", human_count(v));
            }
            for n in &r.notes {
                println!("  note: {n}");
            }
        }
        (JobState::Cancelled, _, _) => {
            println!(
                "cancelled after {} ({} rows written so far; partial files are renamed .partial), peak RSS {}",
                fmt_duration_ms(outcome.elapsed.as_millis() as u64),
                opt_count(&rows_so_far),
                peak
            );
        }
        (state, _, Some(e)) => {
            println!("{} [{}]: {}", enum_name(state), e.code, e.message);
            if let Some(d) = &e.detail {
                println!("  {d}");
            }
            println!("  after {}, {} rows, peak RSS {}", fmt_duration_ms(outcome.elapsed.as_millis() as u64), opt_count(&rows_so_far), peak);
        }
        (state, _, None) => {
            println!(
                "{} after {} ({} rows, {} bytes), peak RSS {}",
                enum_name(state),
                fmt_duration_ms(outcome.elapsed.as_millis() as u64),
                opt_count(&rows_so_far),
                opt_bytes(&None),
                peak
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::args::{Cli, Cmd};
    use clap::Parser;

    fn export_args(extra: &[&str]) -> ExportArgs {
        let mut argv = vec!["bulk-cli", "export", "--driver", "sqlite", "--path", "t.db"];
        argv.extend_from_slice(extra);
        match Cli::try_parse_from(argv).expect("parse").cmd {
            Cmd::Export(e) => e,
            _ => unreachable!(),
        }
    }

    #[test]
    fn debug_log_never_carries_the_password() {
        let req = ExportRequest {
            conn_id: "c1".into(),
            sql: Some("select 1".into()),
            sql_file: None,
            name: Some("t".into()),
            session: Default::default(),
            output: Default::default(),
            password: Some(r#"pa"ss\word-secret"#.into()),
        };
        let text = loggable_request(&req);
        assert!(!text.contains("secret") && !text.contains(r#"pa\"ss"#), "{text}");
        assert!(text.contains(r#""password":"***""#) && text.contains(r#""connId":"c1""#), "{text}");
        assert_eq!(req.password.as_deref(), Some(r#"pa"ss\word-secret"#), "the request itself is untouched");
        let none = loggable_request(&ExportRequest { password: None, ..req });
        // `None` fields are omitted on the wire (the generated TS types declare them optional).
        assert!(!none.contains("password") && none.contains(r#""connId":"c1""#), "{none}");
    }

    fn conn() -> Connection {
        Connection { id: "c1".into(), name: "test".into(), driver: "sqlite".into(), ..Default::default() }
    }

    fn settings() -> Settings {
        Settings { csv_crlf_default: false, ..Settings::default() }
    }

    #[test]
    fn out_path_extension_chain() {
        let t = parse_out_path(Path::new("out/wide.csv.gz"));
        assert_eq!(t.stem, "wide");
        assert_eq!(t.format, Some(OutputFormat::Csv));
        assert_eq!(t.compress, Some(Compression::Gzip));
        assert!(t.dir.ends_with("out"));
        let t = parse_out_path(Path::new("data.TSV"));
        assert_eq!((t.stem.as_str(), t.tsv, t.format), ("data", true, Some(OutputFormat::Csv)));
        let t = parse_out_path(Path::new("x.parquet"));
        assert_eq!(t.format, Some(OutputFormat::Parquet));
        let t = parse_out_path(Path::new("plain"));
        assert_eq!((t.stem.as_str(), t.format), ("plain", None));
        let t = parse_out_path(Path::new("a.b.zst"));
        assert_eq!((t.stem.as_str(), t.compress), ("a.b", Some(Compression::Zstd)));
    }

    #[test]
    fn request_from_out_csv_gz() {
        let args = export_args(&["--query", "select 1", "--out", "o/wide.csv.gz", "--null", "NULL", "--no-header", "--statement-timeout", "30s"]);
        let req = build_request(&args, &conn(), &settings(), &SqlSource::Inline("select 1".into()), None).unwrap();
        assert_eq!(req.output.name_pattern, "wide");
        assert_eq!(req.name.as_deref(), Some("wide"));
        assert_eq!(req.output.format, OutputFormat::Csv);
        assert_eq!(req.output.compress, Some(Compression::Gzip));
        let csv = req.output.csv.unwrap();
        assert!(!csv.header);
        assert_eq!(csv.null_as, "NULL");
        assert_eq!(csv.delimiter, ",");
        assert!(!csv.crlf);
        assert!(req.output.parquet.is_none());
        assert_eq!(req.session.statement_timeout_ms, Some(30_000));
        assert_eq!(req.sql.as_deref(), Some("select 1"));
        assert!(req.sql_file.is_none());
    }

    #[test]
    fn excel_preset_and_overrides() {
        let args = export_args(&["--query", "select 1", "--out", "o/x.csv", "--excel"]);
        let req = build_request(&args, &conn(), &settings(), &SqlSource::Inline("select 1".into()), None).unwrap();
        let csv = req.output.csv.clone().unwrap();
        assert!(csv.bom && csv.crlf);
        assert_eq!(req.output.split, Some(SplitOpts { rows: Some(1_000_000), megabytes: None }));
        let args = export_args(&["--query", "select 1", "--out", "o/x.csv", "--excel", "--lf", "--split-rows", "500"]);
        let req = build_request(&args, &conn(), &settings(), &SqlSource::Inline("select 1".into()), None).unwrap();
        assert!(!req.output.csv.unwrap().crlf);
        assert_eq!(req.output.split.unwrap().rows, Some(500));
        let args = export_args(&["--query", "select 1", "--out", "o/x.parquet", "--excel"]);
        assert!(build_request(&args, &conn(), &settings(), &SqlSource::Inline("select 1".into()), None).is_err());
    }

    #[test]
    fn parquet_and_tsv() {
        let args = export_args(&["--query", "select 1", "--out", "o/x.parquet", "--parquet-compression", "snappy"]);
        let req = build_request(&args, &conn(), &settings(), &SqlSource::Inline("select 1".into()), None).unwrap();
        assert_eq!(req.output.format, OutputFormat::Parquet);
        assert!(req.output.csv.is_none());
        assert_eq!(req.output.parquet.unwrap().compression, bulk_core::types::ParquetCompression::Snappy);
        let args = export_args(&["--query", "select 1", "--out", "o/x.parquet", "--compress", "gzip"]);
        assert!(build_request(&args, &conn(), &settings(), &SqlSource::Inline("select 1".into()), None).is_err());
        let args = export_args(&["--query", "select 1", "--out", "o/x.tsv"]);
        let req = build_request(&args, &conn(), &settings(), &SqlSource::Inline("select 1".into()), None).unwrap();
        assert_eq!(req.output.csv.unwrap().delimiter, "\t");
        let args = export_args(&["--query", "select 1", "--out", "o/x.csv", "--delimiter", "tab"]);
        let req = build_request(&args, &conn(), &settings(), &SqlSource::Inline("select 1".into()), None).unwrap();
        assert_eq!(req.output.csv.unwrap().delimiter, "\t");
        let args = export_args(&["--query", "select 1", "--out", "o/x.csv", "--delimiter", "ab"]);
        assert!(build_request(&args, &conn(), &settings(), &SqlSource::Inline("select 1".into()), None).is_err());
    }

    #[test]
    fn dir_pattern_and_fallbacks() {
        let args = export_args(&["--sql", "q.sql", "--dir", "outdir", "--name", "orders", "--pattern", "{name}_{yyyyMMdd}"]);
        let req = build_request(&args, &conn(), &settings(), &SqlSource::File("q.sql".into()), None).unwrap();
        assert!(req.output.dir.ends_with("outdir"));
        assert_eq!(req.output.name_pattern, "{name}_{yyyyMMdd}");
        assert_eq!(req.name.as_deref(), Some("orders"));
        assert_eq!(req.sql_file.as_deref(), Some("q.sql"));
        let args = export_args(&["--query", "select 1"]);
        assert!(build_request(&args, &conn(), &settings(), &SqlSource::Inline("select 1".into()), None).is_err());
        let mut c = conn();
        c.output_dir = Some("C:/exports".into());
        c.session.work_mem = Some("32MB".into());
        let req = build_request(&args, &c, &settings(), &SqlSource::Inline("select 1".into()), None).unwrap();
        assert_eq!(req.output.dir, "C:/exports");
        assert_eq!(req.output.name_pattern, OutputOpts::default().name_pattern);
        assert_eq!(req.session.work_mem.as_deref(), Some("32MB"));
    }
}
