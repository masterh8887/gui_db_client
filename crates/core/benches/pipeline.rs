//! Pipeline bench harness and CI gate (custom harness, no criterion).
//!
//! Generates the deterministic `wide` / `narrow` / `nasty` datasets from [`bulk_core::testdata`]
//! into a SQLite file, then streams each one through the real pipeline (SQLite source ->
//! bounded channel -> CSV sink in a temp dir) and measures rows/s, MB/s and the peak resident
//! set size of the process while the pipeline runs.
//!
//! ```text
//! cargo bench -p bulk-core --bench pipeline                    # all datasets, print results
//! cargo bench -p bulk-core --bench pipeline -- wide            # only `wide`
//! cargo bench -p bulk-core --bench pipeline -- --gate \
//!     --baseline bench-baseline.json --save bench-result.json  # CI gate
//! ```
//!
//! With `--gate` the process exits non-zero when
//! * the peak RSS of any run reaches `--max-rss-mb` (default 100 MB, CLAUDE.md rule 2), or
//! * a dataset's best throughput is more than `--tolerance` percent (default 5) below the
//!   baseline recorded for the same OS and row count, or
//! * the CSV sink reports a different row count than the fixture holds.
//!
//! A missing baseline file (the first run, or a fresh cache) only skips the throughput
//! comparison; the RSS and row-count checks still apply. `--save` writes this run's results in
//! the baseline format so CI can record them as the next baseline.
//!
//! Relative paths are resolved against the current directory, which `cargo bench` sets to the
//! package root (`crates/core`); CI passes absolute paths.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use bulk_core::job::{run_pipeline, Progress};
use bulk_core::sink::{self, SinkTarget};
use bulk_core::source::{self, ConnectParams, Query, SslMode, StreamOutput};
use bulk_core::types::{CsvOpts, OutputFormat, OutputOpts, SessionOpts};
use bulk_core::{platform, testdata, CancellationToken};
use serde::{Deserialize, Serialize};

/// Version of the baseline / result JSON layout.
const REPORT_VERSION: u32 = 1;
/// How often the RSS sampler looks at the process while a pipeline runs.
const RSS_SAMPLE_EVERY: Duration = Duration::from_millis(2);
const MB: f64 = 1024.0 * 1024.0;

// ---------------------------------------------------------------------------------------------
// datasets
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Dataset {
    Wide,
    Narrow,
    Nasty,
}

impl Dataset {
    const ALL: [Dataset; 3] = [Dataset::Wide, Dataset::Narrow, Dataset::Nasty];

    fn name(self) -> &'static str {
        match self {
            Dataset::Wide => "wide",
            Dataset::Narrow => "narrow",
            Dataset::Nasty => "nasty",
        }
    }

    fn parse(s: &str) -> Option<Dataset> {
        Dataset::ALL.into_iter().find(|d| d.name() == s)
    }

    /// Default fixture size: far more than `CHANNEL_DEPTH` full batches (steady-state memory),
    /// and runs of a second or more, so that the best of `--iterations` runs is stable enough
    /// for a 5% gate (sub-second runs vary by 30% on a busy machine). Still a CI-sized job:
    /// the fixture is about 1 GB on disk and takes ~10 s to build.
    fn default_rows(self) -> u64 {
        match self {
            Dataset::Wide => 3_000_000,
            Dataset::Narrow => 10_000_000,
            Dataset::Nasty => 2_000_000,
        }
    }

    fn sql(self) -> String {
        format!("SELECT * FROM {}", self.name())
    }
}

// ---------------------------------------------------------------------------------------------
// command line
// ---------------------------------------------------------------------------------------------

#[derive(Debug)]
struct Args {
    gate: bool,
    baseline: Option<PathBuf>,
    save: Option<PathBuf>,
    rows: Option<u64>,
    iterations: u32,
    max_rss_mb: f64,
    tolerance_pct: f64,
    datasets: Vec<Dataset>,
}

const USAGE: &str = "usage: pipeline [--gate] [--baseline FILE] [--save FILE] [--rows N] [--iterations N] \
[--max-rss-mb MB] [--tolerance PCT] [wide|narrow|nasty ...]";

impl Args {
    fn parse(mut it: impl Iterator<Item = String>) -> Result<Args> {
        let mut args = Args { gate: false, baseline: None, save: None, rows: None, iterations: 5, max_rss_mb: 100.0, tolerance_pct: 5.0, datasets: Vec::new() };
        while let Some(arg) = it.next() {
            let mut value = |flag: &str| it.next().ok_or_else(|| anyhow!("{flag} needs a value\n{USAGE}"));
            match arg.as_str() {
                // `cargo bench` always passes `--bench` to harness = false targets.
                "--bench" => {}
                "--gate" => args.gate = true,
                "--baseline" => args.baseline = Some(PathBuf::from(value("--baseline")?)),
                "--save" => args.save = Some(PathBuf::from(value("--save")?)),
                "--rows" => args.rows = Some(parse_num(&value("--rows")?, "--rows")?),
                "--iterations" => args.iterations = parse_num(&value("--iterations")?, "--iterations")?,
                "--max-rss-mb" => args.max_rss_mb = parse_num(&value("--max-rss-mb")?, "--max-rss-mb")?,
                "--tolerance" => args.tolerance_pct = parse_num(&value("--tolerance")?, "--tolerance")?,
                "-h" | "--help" => bail!("{USAGE}"),
                flag if flag.starts_with('-') => bail!("unknown option {flag}\n{USAGE}"),
                name => {
                    let d = Dataset::parse(name).ok_or_else(|| anyhow!("unknown dataset {name:?} (expected wide, narrow or nasty)\n{USAGE}"))?;
                    if !args.datasets.contains(&d) {
                        args.datasets.push(d);
                    }
                }
            }
        }
        if args.datasets.is_empty() {
            args.datasets = Dataset::ALL.to_vec();
        }
        args.datasets.sort();
        if args.iterations == 0 {
            bail!("--iterations must be at least 1");
        }
        if args.rows == Some(0) {
            bail!("--rows must be at least 1");
        }
        if args.max_rss_mb.is_nan() || args.max_rss_mb <= 0.0 || !(0.0..100.0).contains(&args.tolerance_pct) {
            bail!("--max-rss-mb must be > 0 and --tolerance must be in 0..100");
        }
        Ok(args)
    }

    fn rows_for(&self, d: Dataset) -> u64 {
        self.rows.unwrap_or_else(|| d.default_rows())
    }
}

fn parse_num<T: std::str::FromStr>(s: &str, flag: &str) -> Result<T> {
    s.parse().map_err(|_| anyhow!("{flag}: {s:?} is not a valid number"))
}

// ---------------------------------------------------------------------------------------------
// report format (also the baseline format)
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BenchReport {
    version: u32,
    os: String,
    arch: String,
    datasets: BTreeMap<String, DatasetResult>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DatasetResult {
    rows: u64,
    /// CSV bytes written by the fastest run.
    bytes: u64,
    /// Wall time of the fastest run, seconds.
    best_secs: f64,
    rows_per_sec: f64,
    mb_per_sec: f64,
    /// Highest RSS seen across all runs of this dataset.
    peak_rss_mb: f64,
}

fn read_baseline(path: &Path) -> Result<Option<BenchReport>> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("reading baseline {}", path.display())),
    };
    let report: BenchReport = serde_json::from_str(&text).with_context(|| format!("parsing baseline {}", path.display()))?;
    if report.version != REPORT_VERSION {
        println!("baseline {} has version {} (expected {REPORT_VERSION}); throughput not compared", path.display(), report.version);
        return Ok(None);
    }
    Ok(Some(report))
}

// ---------------------------------------------------------------------------------------------
// peak RSS sampling
// ---------------------------------------------------------------------------------------------

/// Current resident set size of this process in bytes, `None` where unsupported.
fn current_rss() -> Option<u64> {
    memory_stats::memory_stats().map(|m| m.physical_mem as u64)
}

/// Samples the process RSS on a background thread until [`RssSampler::stop`].
struct RssSampler {
    stop: Arc<AtomicBool>,
    peak: Arc<AtomicU64>,
    handle: JoinHandle<()>,
}

impl RssSampler {
    fn start() -> Result<RssSampler> {
        let first = current_rss().ok_or_else(|| anyhow!("this platform does not report the process RSS (memory-stats)"))?;
        let stop = Arc::new(AtomicBool::new(false));
        let peak = Arc::new(AtomicU64::new(first));
        let handle = std::thread::Builder::new()
            .name("rss-sampler".into())
            .spawn({
                let (stop, peak) = (stop.clone(), peak.clone());
                move || {
                    while !stop.load(Ordering::Relaxed) {
                        if let Some(rss) = current_rss() {
                            peak.fetch_max(rss, Ordering::Relaxed);
                        }
                        std::thread::sleep(RSS_SAMPLE_EVERY);
                    }
                }
            })
            .context("spawning the RSS sampler thread")?;
        Ok(RssSampler { stop, peak, handle })
    }

    /// Stop sampling and return the peak RSS in bytes (including one final sample).
    fn stop(self) -> u64 {
        self.stop.store(true, Ordering::Relaxed);
        let _ = self.handle.join();
        if let Some(rss) = current_rss() {
            self.peak.fetch_max(rss, Ordering::Relaxed);
        }
        self.peak.load(Ordering::Relaxed)
    }
}

// ---------------------------------------------------------------------------------------------
// one measured run
// ---------------------------------------------------------------------------------------------

struct RunStats {
    rows: u64,
    bytes: u64,
    secs: f64,
    peak_rss: u64,
}

fn sqlite_params(db: &Path) -> ConnectParams {
    ConnectParams {
        driver: "sqlite".into(),
        host: String::new(),
        port: 0,
        database: None,
        user: None,
        password: None,
        fields: BTreeMap::from([("path".to_string(), db.to_string_lossy().into_owned())]),
        application_name: format!("{}bench", bulk_core::APP_NAME_PREFIX),
        connect_timeout: Duration::from_secs(15),
        ssl: SslMode::Disable,
    }
}

/// Stream one dataset from `db` into a fresh CSV file under `out_dir`, measuring wall time and
/// peak RSS. The output file is deleted afterwards so repeated runs do not fill the disk.
async fn run_once(db: &Path, dataset: Dataset, out_dir: &Path, iteration: u32) -> Result<RunStats> {
    let database = source::open(&sqlite_params(db)).await.context("opening the SQLite fixture")?;
    let opts = OutputOpts {
        dir: out_dir.to_string_lossy().into_owned(),
        format: OutputFormat::Csv,
        csv: Some(CsvOpts { crlf: false, ..CsvOpts::default() }),
        overwrite: true,
        ..OutputOpts::default()
    };
    let target = SinkTarget { dir: out_dir.to_path_buf(), stem: format!("{}_{iteration}", dataset.name()), opts };
    let sink = sink::build(&target).context("building the CSV sink")?;
    let query = Query { sql: dataset.sql(), output: StreamOutput::Arrow };
    let cancel = CancellationToken::new();

    let sampler = RssSampler::start()?;
    let started = Instant::now();
    let outcome = async {
        let stream = database.stream(&query, &SessionOpts::default(), cancel.clone()).await?;
        run_pipeline(stream, sink, Arc::new(Progress::new()), cancel).await
    }
    .await;
    let secs = started.elapsed().as_secs_f64();
    let peak_rss = sampler.stop();
    drop(database);

    let report = outcome.map_err(|e| anyhow!("pipeline failed for {}: {e}", dataset.name()))?;
    for file in &report.files {
        std::fs::remove_file(file).with_context(|| format!("removing bench output {}", file.display()))?;
    }
    Ok(RunStats { rows: report.rows, bytes: report.bytes, secs, peak_rss })
}

/// Run `dataset` `iterations` times; keep the fastest run and the highest peak RSS.
async fn bench_dataset(db: &Path, dataset: Dataset, expected_rows: u64, iterations: u32, out_dir: &Path) -> Result<DatasetResult> {
    let mut best: Option<RunStats> = None;
    let mut peak_rss = 0u64;
    for i in 1..=iterations {
        let run = run_once(db, dataset, out_dir, i).await?;
        if run.rows != expected_rows {
            bail!("{}: the sink wrote {} rows, the fixture holds {expected_rows}", dataset.name(), run.rows);
        }
        println!(
            "  {:<6} run {i}/{iterations}: {:>9} rows in {:>7.3} s  {:>11.0} rows/s  {:>7.1} MB/s  peak RSS {:>6.1} MB",
            dataset.name(),
            run.rows,
            run.secs,
            run.rows as f64 / run.secs,
            run.bytes as f64 / MB / run.secs,
            run.peak_rss as f64 / MB
        );
        peak_rss = peak_rss.max(run.peak_rss);
        if best.as_ref().is_none_or(|b| run.secs < b.secs) {
            best = Some(run);
        }
    }
    let best = best.ok_or_else(|| anyhow!("no runs for {}", dataset.name()))?;
    let secs = best.secs.max(f64::EPSILON);
    Ok(DatasetResult {
        rows: best.rows,
        bytes: best.bytes,
        best_secs: best.secs,
        rows_per_sec: best.rows as f64 / secs,
        mb_per_sec: best.bytes as f64 / MB / secs,
        peak_rss_mb: peak_rss as f64 / MB,
    })
}

// ---------------------------------------------------------------------------------------------
// gate
// ---------------------------------------------------------------------------------------------

/// Compare this run against the limits and the baseline; returns the failures.
fn gate(args: &Args, current: &BenchReport, baseline: Option<&BenchReport>) -> Vec<String> {
    let mut failures = Vec::new();
    let baseline = baseline.filter(|b| {
        let same = b.os == current.os && b.arch == current.arch;
        if !same {
            println!("baseline was recorded on {}/{}, this is {}/{}: throughput not compared", b.os, b.arch, current.os, current.arch);
        }
        same
    });
    for (name, r) in &current.datasets {
        if r.peak_rss_mb >= args.max_rss_mb {
            failures.push(format!("{name}: peak RSS {:.1} MB >= {:.0} MB limit", r.peak_rss_mb, args.max_rss_mb));
        }
        let Some(base) = baseline.and_then(|b| b.datasets.get(name)) else {
            println!("{name}: no baseline, throughput not compared");
            continue;
        };
        if base.rows != r.rows {
            println!("{name}: baseline has {} rows, this run {}: throughput not compared", base.rows, r.rows);
            continue;
        }
        let change_pct = (r.rows_per_sec / base.rows_per_sec - 1.0) * 100.0;
        println!("{name}: {:.0} rows/s vs baseline {:.0} rows/s ({change_pct:+.1}%)", r.rows_per_sec, base.rows_per_sec);
        if change_pct < -args.tolerance_pct {
            failures.push(format!(
                "{name}: throughput {:.0} rows/s is {:.1}% below the baseline {:.0} rows/s (limit {:.1}%)",
                r.rows_per_sec, -change_pct, base.rows_per_sec, args.tolerance_pct
            ));
        }
    }
    failures
}

// ---------------------------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------------------------

fn run(args: &Args) -> Result<Vec<String>> {
    let baseline = load_baseline(args)?;
    let work = tempfile::Builder::new().prefix("bulkdb-bench-").tempdir().context("creating the bench temp dir")?;
    let db = work.path().join("fixture.db");
    let out_dir = work.path().join("out");
    std::fs::create_dir_all(&out_dir).with_context(|| format!("creating {}", out_dir.display()))?;

    let rows_of = |d: Dataset| {
        if args.datasets.contains(&d) {
            args.rows_for(d)
        } else {
            0
        }
    };
    let started = Instant::now();
    testdata::sqlite_fixture(&db, rows_of(Dataset::Wide), rows_of(Dataset::Narrow), rows_of(Dataset::Nasty)).context("building the SQLite fixture")?;
    println!(
        "fixture: {} in {:.1} s ({:.0} MB)",
        args.datasets.iter().map(|d| format!("{} {}", d.name(), args.rows_for(*d))).collect::<Vec<_>>().join(", "),
        started.elapsed().as_secs_f64(),
        std::fs::metadata(&db).map(|m| m.len() as f64 / MB).unwrap_or(0.0)
    );

    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build().context("starting the tokio runtime")?;
    let mut report = BenchReport { version: REPORT_VERSION, os: platform::os_key().to_string(), arch: platform::arch().to_string(), datasets: BTreeMap::new() };
    for &d in &args.datasets {
        let result = runtime.block_on(bench_dataset(&db, d, args.rows_for(d), args.iterations, &out_dir))?;
        println!("{:<6} best: {:.0} rows/s, {:.1} MB/s, peak RSS {:.1} MB", d.name(), result.rows_per_sec, result.mb_per_sec, result.peak_rss_mb);
        report.datasets.insert(d.name().to_string(), result);
    }
    drop(runtime);

    if let Some(path) = &args.save {
        let json = serde_json::to_string_pretty(&report).context("serialising the bench report")?;
        std::fs::write(path, json + "\n").with_context(|| format!("writing {}", path.display()))?;
        println!("results saved to {}", path.display());
    }

    if !args.gate {
        return Ok(Vec::new());
    }
    Ok(gate(args, &report, baseline.as_ref()))
}

/// Load the baseline for `--gate` (before anything runs, so a malformed file fails fast and
/// `--save` may safely point at the same path).
fn load_baseline(args: &Args) -> Result<Option<BenchReport>> {
    if !args.gate {
        return Ok(None);
    }
    let Some(path) = &args.baseline else {
        println!("no --baseline given: throughput not compared");
        return Ok(None);
    };
    let baseline = read_baseline(path)?;
    if baseline.is_none() {
        println!("no usable baseline at {}: throughput not compared", path.display());
    }
    Ok(baseline)
}

fn main() -> ExitCode {
    let args = match Args::parse(std::env::args().skip(1)) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(2);
        }
    };
    match run(&args) {
        Ok(failures) if failures.is_empty() => {
            if args.gate {
                println!("bench gate: PASS");
            }
            ExitCode::SUCCESS
        }
        Ok(failures) => {
            for f in &failures {
                eprintln!("bench gate: FAIL: {f}");
            }
            ExitCode::FAILURE
        }
        Err(e) => {
            eprintln!("bench harness error: {e:#}");
            ExitCode::from(2)
        }
    }
}
