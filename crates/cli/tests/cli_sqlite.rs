//! End-to-end tests of the `bulk-cli` binary against a SQLite fixture: every subcommand the
//! scheduler interface relies on, run as a child process with `BULKDB_CONFIG_DIR` pointing at a
//! throw-away directory and `BULKDB_CREDENTIALS=memory` so nothing touches the OS keyring.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};

const WIDE_ROWS: u64 = 10_000;
const NARROW_ROWS: u64 = 500;
const NASTY_ROWS: u64 = 200;

/// A directory under the system temp dir, removed on drop (no `tempfile` dev-dependency).
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("bulk-cli-test-{}-{n}-{tag}", std::process::id()));
        fs::create_dir_all(&dir).expect("create temp dir");
        TempDir(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// One SQLite fixture plus an isolated config dir.
struct Fixture {
    dir: TempDir,
    db: PathBuf,
    config: PathBuf,
}

impl Fixture {
    fn new(tag: &str) -> Self {
        let dir = TempDir::new(tag);
        let db = dir.path().join("fixture.db");
        bulk_core::testdata::sqlite_fixture(&db, WIDE_ROWS, NARROW_ROWS, NASTY_ROWS).expect("build sqlite fixture");
        let config = dir.path().join("config");
        fs::create_dir_all(&config).expect("create config dir");
        Fixture { dir, db, config }
    }

    fn db(&self) -> &str {
        self.db.to_str().expect("utf-8 temp path")
    }

    fn out(&self, name: &str) -> String {
        self.dir.path().join(name).to_string_lossy().into_owned()
    }

    fn command(&self) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_bulk-cli"));
        cmd.env("BULKDB_CONFIG_DIR", &self.config).env("BULKDB_CREDENTIALS", "memory").env_remove("RUST_LOG").stdin(Stdio::null());
        cmd
    }

    /// Run the binary with `args`; stdin is closed (non-interactive).
    fn run(&self, args: &[&str]) -> Output {
        self.command().args(args).output().expect("spawn bulk-cli")
    }

    /// Run the binary with `args`, feeding `stdin` to it.
    fn run_with_stdin(&self, args: &[&str], stdin: &str) -> Output {
        let mut child = self.command().args(args).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().expect("spawn bulk-cli");
        child.stdin.take().expect("stdin").write_all(stdin.as_bytes()).expect("write stdin");
        child.wait_with_output().expect("wait for bulk-cli")
    }

    /// `export --driver sqlite --path <db> <args>`.
    fn export(&self, args: &[&str]) -> Output {
        let mut argv = vec!["export", "--driver", "sqlite", "--path", self.db()];
        argv.extend_from_slice(args);
        self.run(&argv)
    }
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

fn exit_code(o: &Output) -> i32 {
    o.status.code().unwrap_or(-1)
}

fn assert_exit(o: &Output, expected: i32) {
    assert_eq!(exit_code(o), expected, "exit code\n--- stdout\n{}\n--- stderr\n{}", stdout(o), stderr(o));
}

fn json_stdout(o: &Output) -> serde_json::Value {
    serde_json::from_str(&stdout(o)).unwrap_or_else(|e| panic!("stdout is not JSON ({e}):\n{}\n--- stderr\n{}", stdout(o), stderr(o)))
}

fn line_count(path: &str) -> usize {
    let text = fs::read_to_string(path).expect("read output");
    text.lines().filter(|l| !l.is_empty()).count()
}

fn files_named(dir: &Path, prefix: &str, suffix: &str) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = fs::read_dir(dir)
        .expect("read output dir")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            let n = p.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default();
            n.starts_with(prefix) && n.ends_with(suffix)
        })
        .collect();
    v.sort();
    v
}

// ---------------------------------------------------------------------------------------------
// export
// ---------------------------------------------------------------------------------------------

#[test]
fn export_csv_with_progress_writes_every_row() {
    let fx = Fixture::new("csv");
    let out = fx.out("wide.csv");
    let o = fx.export(&["--query", "select * from wide", "--out", &out, "--yes", "--progress"]);
    assert_exit(&o, 0);
    assert_eq!(line_count(&out), WIDE_ROWS as usize + 1, "header + {WIDE_ROWS} rows");
    let text = fs::read_to_string(&out).unwrap();
    assert!(text.starts_with("id,title,body,author,category,tags,created_at,updated_at,status"), "header: {}", text.lines().next().unwrap_or(""));
    assert!(!text.starts_with('\u{feff}'), "no BOM without --bom");
    let so = stdout(&o);
    assert!(so.starts_with("done: 10,000 rows"), "summary on stdout:\n{so}");
    assert!(so.contains("peak RSS"), "summary names the peak RSS:\n{so}");
    assert!(so.contains("wide.csv"), "summary lists the file:\n{so}");
    let se = stderr(&o);
    assert!(se.contains("preflight"), "preflight line on stderr:\n{se}");
    assert!(se.contains("started"), "--progress logs the job start on stderr (info level):\n{se}");
    // The temporary ad-hoc connection must not survive the command.
    let list = fx.run(&["conn", "list", "--json"]);
    assert_exit(&list, 0);
    assert_eq!(json_stdout(&list), serde_json::json!([]), "ad-hoc connection was removed again");
}

#[test]
fn export_parquet_writes_a_parquet_file() {
    let fx = Fixture::new("parquet");
    let out = fx.out("wide.parquet");
    let o = fx.export(&["--query", "select * from wide", "--out", &out, "--yes"]);
    assert_exit(&o, 0);
    let bytes = fs::read(&out).expect("parquet file");
    assert!(bytes.len() > 1024, "parquet file is {} bytes", bytes.len());
    assert_eq!(&bytes[..4], b"PAR1", "parquet magic at the start");
    assert_eq!(&bytes[bytes.len() - 4..], b"PAR1", "parquet magic at the end");
    assert!(stdout(&o).starts_with("done: 10,000 rows"), "{}", stdout(&o));
}

#[test]
fn export_excel_preset_is_one_file_with_bom_and_crlf() {
    let fx = Fixture::new("excel");
    let out = fx.out("wide_excel.csv");
    let o = fx.export(&["--query", "select * from wide", "--out", &out, "--excel", "--yes"]);
    assert_exit(&o, 0);
    let files = files_named(fx.dir.path(), "wide_excel", ".csv");
    assert_eq!(files.len(), 1, "one file below the 1,000,000-row split: {files:?}");
    let bytes = fs::read(&files[0]).unwrap();
    assert_eq!(&bytes[..3], b"\xEF\xBB\xBF", "UTF-8 BOM");
    let text = String::from_utf8_lossy(&bytes[3..]).into_owned();
    assert!(text.starts_with("id,title"), "header after the BOM");
    assert!(text.contains("\r\n"), "CRLF line endings");
    assert!(!text.replace("\r\n", "").contains('\n'), "no bare LF");
    assert_eq!(text.split("\r\n").filter(|l| !l.is_empty()).count(), WIDE_ROWS as usize + 1);
}

#[test]
fn export_json_reports_the_result() {
    let fx = Fixture::new("json");
    let out = fx.out("narrow.csv");
    let o = fx.run(&["--json", "export", "--driver", "sqlite", "--path", fx.db(), "--query", "select * from narrow", "--out", &out, "--yes"]);
    assert_exit(&o, 0);
    let v = json_stdout(&o);
    assert_eq!(v["state"], "done", "{v}");
    assert_eq!(v["result"]["rows"], NARROW_ROWS, "{v}");
    assert!(v["result"]["bytes"].as_u64().unwrap_or(0) > 0, "{v}");
    assert_eq!(v["result"]["files"].as_array().map(Vec::len), Some(1), "{v}");
    assert!(v["jobId"].is_string(), "{v}");
    assert!(v["preflight"].is_object(), "{v}");
    assert!(v["avgBytesPerSec"].as_f64().unwrap_or(0.0) > 0.0, "{v}");
    assert_eq!(line_count(&out), NARROW_ROWS as usize + 1);
}

#[test]
fn export_reads_the_query_from_stdin_and_splits_files() {
    let fx = Fixture::new("stdin-split");
    let out = fx.out("narrow.csv");
    let o = fx.run_with_stdin(
        &["export", "--driver", "sqlite", "--path", fx.db(), "--query", "-", "--out", &out, "--split-rows", "200", "--no-header", "--yes"],
        "select a, b from narrow order by a",
    );
    assert_exit(&o, 0);
    let parts = files_named(fx.dir.path(), "narrow_part", ".csv");
    assert_eq!(parts.len(), 3, "500 rows / 200 per file: {parts:?}");
    let total: usize = parts.iter().map(|p| line_count(p.to_str().unwrap())).sum();
    assert_eq!(total, NARROW_ROWS as usize, "no header lines with --no-header");
    let first = fs::read_to_string(&parts[0]).unwrap();
    assert!(first.starts_with("0,"), "first row of the first part: {}", first.lines().next().unwrap_or(""));
}

#[test]
fn export_csv_options_null_delimiter_and_gzip_extension() {
    let fx = Fixture::new("csv-opts");
    let out = fx.out("nasty.tsv");
    let o = fx.export(&["--query", "select id, text, note from nasty order by id", "--out", &out, "--null", "NULL", "--lf", "--yes"]);
    assert_exit(&o, 0);
    let text = fs::read_to_string(&out).unwrap();
    assert!(text.starts_with("id\ttext\tnote\n"), "tab delimiter from the .tsv extension: {:?}", text.lines().next());
    assert!(text.contains("\tNULL"), "--null text for NULL cells");
    // (`nasty` cells contain a literal CRLF inside quotes, so the --lf check uses `narrow`.)
    let lf = fx.out("narrow_lf.csv");
    let o = fx.export(&["--query", "select * from narrow", "--out", &lf, "--lf", "--yes"]);
    assert_exit(&o, 0);
    assert!(!fs::read_to_string(&lf).unwrap().contains('\r'), "--lf: no CR");
    let crlf = fx.out("narrow_crlf.csv");
    let o = fx.export(&["--query", "select * from narrow", "--out", &crlf, "--crlf", "--yes"]);
    assert_exit(&o, 0);
    assert!(fs::read_to_string(&crlf).unwrap().starts_with("a,b,c\r\n"), "--crlf line endings");

    let gz = fx.out("narrow.csv.gz");
    let o = fx.export(&["--query", "select * from narrow", "--out", &gz, "--yes"]);
    assert_exit(&o, 0);
    let bytes = fs::read(&gz).unwrap();
    assert_eq!(&bytes[..2], b"\x1f\x8b", "gzip magic from the .gz extension");
}

#[test]
fn export_existing_file_needs_acknowledgement_without_a_tty() {
    let fx = Fixture::new("confirm");
    let out = fx.out("wide.csv");
    assert_exit(&fx.export(&["--query", "select * from narrow", "--out", &out, "--yes"]), 0);
    // The file now exists: preflight warns, stdin is not a tty and --yes is missing -> exit 4.
    let o = fx.export(&["--query", "select * from narrow", "--out", &out]);
    assert_exit(&o, 4);
    assert!(stderr(&o).contains("--yes"), "hint to re-run with --yes:\n{}", stderr(&o));
    // --json still reports the state machine-readably.
    let o = fx.run(&["--json", "export", "--driver", "sqlite", "--path", fx.db(), "--query", "select * from narrow", "--out", &out]);
    assert_exit(&o, 4);
    assert_eq!(json_stdout(&o)["state"], "needs_confirmation");
    // --overwrite removes the warning.
    assert_exit(&fx.export(&["--query", "select * from narrow", "--out", &out, "--overwrite"]), 0);
}

#[test]
fn export_rejects_dml_and_bad_arguments() {
    let fx = Fixture::new("reject");
    let out = fx.out("x.csv");
    let o = fx.export(&["--query", "delete from wide", "--out", &out, "--yes"]);
    assert_ne!(exit_code(&o), 0, "DML must not run:\n{}", stdout(&o));
    assert!(exit_code(&o) == 3 || exit_code(&o) == 1, "rejected (3) or error (1), got {}", exit_code(&o));
    assert!(stdout(&o).contains("rejected") || stderr(&o).contains("rejected") || stderr(&o).contains("error"), "{}\n{}", stdout(&o), stderr(&o));
    assert!(!Path::new(&out).exists(), "no output file for a rejected query");

    // sqlite without --path is a usage error (exit 1), reported before anything runs.
    let o = fx.run(&["export", "--driver", "sqlite", "--query", "select 1", "--out", &out, "--yes"]);
    assert_exit(&o, 1);
    assert!(stderr(&o).contains("--path"), "{}", stderr(&o));

    // Unknown driver.
    let o = fx.run(&["export", "--driver", "nope", "--query", "select 1", "--out", &out, "--yes"]);
    assert_exit(&o, 1);
    assert!(stderr(&o).contains("unknown driver"), "{}", stderr(&o));

    // No output location at all.
    let o = fx.export(&["--query", "select 1", "--yes"]);
    assert_exit(&o, 1);
    assert!(stderr(&o).contains("--out"), "{}", stderr(&o));

    // clap usage errors exit 1 too, and --help exits 0.
    assert_exit(&fx.run(&["export", "--query", "select 1"]), 1);
    assert_exit(&fx.run(&["--help"]), 0);
    assert!(stdout(&fx.run(&["--help"])).contains("export"));
}

// ---------------------------------------------------------------------------------------------
// catalog
// ---------------------------------------------------------------------------------------------

#[test]
fn catalog_tables_json_lists_the_fixture_tables() {
    let fx = Fixture::new("catalog-tables");
    let o = fx.run(&["catalog", "tables", "--driver", "sqlite", "--path", fx.db(), "--json"]);
    assert_exit(&o, 0);
    let v = json_stdout(&o);
    let names: Vec<&str> = v.as_array().expect("array").iter().filter_map(|t| t["name"].as_str()).collect();
    assert!(names.contains(&"wide"), "{names:?}");
    assert!(names.contains(&"narrow"), "{names:?}");
    assert!(names.contains(&"nasty"), "{names:?}");

    let o = fx.run(&["catalog", "tables", "--driver", "sqlite", "--path", fx.db()]);
    assert_exit(&o, 0);
    let text = stdout(&o);
    assert!(text.contains("NAME") && text.contains("wide"), "{text}");
    assert!(text.contains("estimates"), "row counts are labelled as estimates: {text}");
}

#[test]
fn catalog_databases_schemas_columns_indexes_ddl() {
    let fx = Fixture::new("catalog-meta");
    let conn = ["--driver", "sqlite", "--path", fx.db()];

    let mut argv = vec!["catalog", "databases", "--json"];
    argv.extend_from_slice(&conn);
    let o = fx.run(&argv);
    assert_exit(&o, 0);
    assert!(!json_stdout(&o).as_array().expect("array").is_empty());

    let mut argv = vec!["catalog", "schemas", "--json"];
    argv.extend_from_slice(&conn);
    let o = fx.run(&argv);
    assert_exit(&o, 0);
    assert!(json_stdout(&o).is_array());

    let mut argv = vec!["catalog", "columns", "--table", "wide", "--json"];
    argv.extend_from_slice(&conn);
    let o = fx.run(&argv);
    assert_exit(&o, 0);
    let cols: Vec<String> = json_stdout(&o).as_array().expect("array").iter().filter_map(|c| c["name"].as_str().map(str::to_string)).collect();
    assert_eq!(cols, bulk_core::testdata::WIDE_COLUMNS.iter().map(|s| s.to_string()).collect::<Vec<_>>());

    let mut argv = vec!["catalog", "columns", "--table", "wide"];
    argv.extend_from_slice(&conn);
    let o = fx.run(&argv);
    assert_exit(&o, 0);
    assert!(stdout(&o).contains("title"), "{}", stdout(&o));

    let mut argv = vec!["catalog", "indexes", "--table", "nasty", "--json"];
    argv.extend_from_slice(&conn);
    let o = fx.run(&argv);
    assert_exit(&o, 0);
    let idx: Vec<String> = json_stdout(&o).as_array().expect("array").iter().filter_map(|i| i["name"].as_str().map(str::to_string)).collect();
    assert!(idx.iter().any(|n| n == "nasty_note"), "{idx:?}");

    let mut argv = vec!["catalog", "ddl", "--table", "wide"];
    argv.extend_from_slice(&conn);
    let o = fx.run(&argv);
    assert_exit(&o, 0);
    let ddl = stdout(&o).to_ascii_uppercase();
    assert!(ddl.contains("CREATE TABLE") && ddl.contains("WIDE"), "{ddl}");

    let mut argv = vec!["catalog", "columns", "--table", "a.b.c.d"];
    argv.extend_from_slice(&conn);
    assert_exit(&fx.run(&argv), 1);
}

#[test]
fn catalog_count_runs_an_exact_count_job() {
    let fx = Fixture::new("count");
    let o = fx.run(&["catalog", "count", "--driver", "sqlite", "--path", fx.db(), "--table", "wide"]);
    assert_exit(&o, 0);
    let text = stdout(&o);
    assert!(text.contains("10,000 rows (exact"), "{text}");

    let o = fx.run(&["catalog", "count", "--driver", "sqlite", "--path", fx.db(), "--table", "narrow", "--json"]);
    assert_exit(&o, 0);
    let v = json_stdout(&o);
    assert_eq!(v["state"], "done", "{v}");
    assert_eq!(v["exactCount"]["rows"], NARROW_ROWS, "{v}");
}

/// `catalog count --cached` prints the count an earlier `catalog count` stored for a saved
/// connection (it survives the process, like the desktop's "exact (time)" label) and never
/// runs a new job; a table that was never counted exits 1.
#[test]
fn catalog_count_cached_prints_the_stored_count() {
    let fx = Fixture::new("count-cached");
    let o = fx.run(&["conn", "add", "--name", "fixture", "--driver", "sqlite", "--path", fx.db()]);
    assert_exit(&o, 0);

    // Nothing stored yet: exit 1, JSON says null.
    let o = fx.run(&["catalog", "count", "--conn", "fixture", "--table", "narrow", "--cached"]);
    assert_exit(&o, 1);
    assert!(stdout(&o).contains("no exact count stored"), "{}", stdout(&o));
    assert!(stderr(&o).contains("without --cached"), "{}", stderr(&o));
    let o = fx.run(&["catalog", "count", "--conn", "fixture", "--table", "narrow", "--cached", "--json"]);
    assert_exit(&o, 1);
    let v = json_stdout(&o);
    assert!(v["exactCount"].is_null(), "{v}");
    assert_eq!(v["cached"], true, "{v}");

    // A real count, in its own process, stores the result.
    let o = fx.run(&["catalog", "count", "--conn", "fixture", "--table", "narrow", "--json"]);
    assert_exit(&o, 0);
    let counted = json_stdout(&o)["exactCount"].clone();
    assert_eq!(counted["rows"], NARROW_ROWS, "{counted}");

    // `--cached` reads it back in a new process: same rows and timestamp, no new job.
    let o = fx.run(&["catalog", "count", "--conn", "fixture", "--table", "narrow", "--cached", "--json"]);
    assert_exit(&o, 0);
    let v = json_stdout(&o);
    assert_eq!(v["exactCount"]["rows"], NARROW_ROWS, "{v}");
    assert_eq!(v["exactCount"]["at"], counted["at"], "the stored count, not a new one: {v}");
    assert_eq!(v["exactCount"]["table"]["name"], "narrow", "{v}");
    assert!(v.get("jobId").is_none(), "no job ran: {v}");
    let o = fx.run(&["catalog", "count", "--conn", "fixture", "--table", "narrow", "--cached"]);
    assert_exit(&o, 0);
    let text = stdout(&o);
    assert!(text.contains("500 rows (exact") && text.contains("stored count"), "{text}");
    assert!(text.contains(counted["at"].as_str().expect("at is a string")), "{text}");
    assert!(!stderr(&o).contains("counting"), "no count job was started: {}", stderr(&o));

    // Per table: another table of the same connection is still uncounted.
    assert_exit(&fx.run(&["catalog", "count", "--conn", "fixture", "--table", "wide", "--cached"]), 1);
    // Ad-hoc connections keep no counts: the flag is refused up front.
    assert_exit(&fx.run(&["catalog", "count", "--driver", "sqlite", "--path", fx.db(), "--table", "narrow", "--cached"]), 1);
    // Unknown saved connection.
    assert_exit(&fx.run(&["catalog", "count", "--conn", "nope", "--table", "narrow", "--cached"]), 1);
}

// ---------------------------------------------------------------------------------------------
// probe / explain / drivers / jobs
// ---------------------------------------------------------------------------------------------

#[test]
fn probe_prints_server_facts() {
    let fx = Fixture::new("probe");
    let o = fx.run(&["probe", "--driver", "sqlite", "--path", fx.db()]);
    assert_exit(&o, 0);
    let text = stdout(&o);
    assert!(text.contains("Driver") && text.contains("sqlite"), "{text}");
    assert!(text.contains("Server") && text.contains("Elapsed"), "{text}");
    assert!(text.contains("Recommended session"), "{text}");

    let o = fx.run(&["probe", "--driver", "sqlite", "--path", fx.db(), "--json"]);
    assert_exit(&o, 0);
    let v = json_stdout(&o);
    assert_eq!(v["server"]["driver"], "sqlite", "{v}");
    assert!(v["server"]["version"].as_str().map(|s| !s.is_empty()).unwrap_or(false), "{v}");
    assert!(v["elapsedMs"].is_number(), "{v}");
    assert!(v["steps"].as_array().map(|s| !s.is_empty()).unwrap_or(false), "{v}");
}

#[test]
fn explain_prints_the_plan() {
    let fx = Fixture::new("explain");
    let o = fx.run(&["explain", "--driver", "sqlite", "--path", fx.db(), "--query", "select * from nasty where note = 'note 1'"]);
    assert_exit(&o, 0);
    let text = stdout(&o);
    assert!(text.contains("Plan"), "{text}");
    assert!(text.to_ascii_uppercase().contains("NASTY"), "{text}");

    let o = fx.run(&["explain", "--driver", "sqlite", "--path", fx.db(), "--query", "select * from wide", "--raw"]);
    assert_exit(&o, 0);
    assert!(stdout(&o).to_ascii_uppercase().contains("WIDE"), "{}", stdout(&o));

    let sql_file = fx.dir.path().join("q.sql");
    fs::write(&sql_file, "select id from wide where id < 10\n").unwrap();
    let o = fx.run(&["explain", "--driver", "sqlite", "--path", fx.db(), "--sql", sql_file.to_str().unwrap(), "--json"]);
    assert_exit(&o, 0);
    let v = json_stdout(&o);
    assert!(v["raw"].is_string(), "{v}");
    assert!(v["warnings"].is_array(), "{v}");
}

#[test]
fn drivers_json_lists_sqlite_as_available() {
    let fx = Fixture::new("drivers");
    let o = fx.run(&["drivers", "--json"]);
    assert_exit(&o, 0);
    let v = json_stdout(&o);
    let sqlite = v.as_array().expect("array").iter().find(|d| d["id"] == "sqlite").expect("sqlite driver listed");
    assert_eq!(sqlite["available"], true, "{sqlite}");
    assert_eq!(sqlite["tier"], "builtin", "{sqlite}");
    for id in ["postgres", "mysql", "mssql"] {
        assert!(v.as_array().unwrap().iter().any(|d| d["id"] == id), "{id} listed");
    }

    let o = fx.run(&["drivers"]);
    assert_exit(&o, 0);
    let text = stdout(&o);
    assert!(text.contains("ID") && text.contains("TIER") && text.contains("sqlite"), "{text}");
}

#[test]
fn jobs_lists_nothing_in_a_fresh_process() {
    let fx = Fixture::new("jobs");
    let o = fx.run(&["jobs", "--json"]);
    assert_exit(&o, 0);
    assert_eq!(json_stdout(&o), serde_json::json!([]));
    let o = fx.run(&["jobs"]);
    assert_exit(&o, 0);
    assert!(stdout(&o).contains("(no rows)"), "{}", stdout(&o));
}

// ---------------------------------------------------------------------------------------------
// conn
// ---------------------------------------------------------------------------------------------

#[test]
fn conn_add_list_test_delete_and_save() {
    let fx = Fixture::new("conn");
    let o = fx.run(&["conn", "add", "--name", "fixture", "--driver", "sqlite", "--path", fx.db(), "--json"]);
    assert_exit(&o, 0);
    let saved = json_stdout(&o);
    assert_eq!(saved["name"], "fixture", "{saved}");
    assert_eq!(saved["driver"], "sqlite", "{saved}");
    let id = saved["id"].as_str().expect("id").to_string();
    assert!(!id.is_empty());

    let o = fx.run(&["conn", "list"]);
    assert_exit(&o, 0);
    assert!(stdout(&o).contains("fixture") && stdout(&o).contains(&id), "{}", stdout(&o));

    // The saved connection works by name and by id.
    let out = fx.out("by_name.csv");
    let o = fx.run(&["export", "--conn", "fixture", "--query", "select * from narrow", "--out", &out, "--yes"]);
    assert_exit(&o, 0);
    assert_eq!(line_count(&out), NARROW_ROWS as usize + 1);
    let o = fx.run(&["conn", "test", &id, "--json"]);
    assert_exit(&o, 0);
    assert_eq!(json_stdout(&o)["server"]["driver"], "sqlite");

    // --save keeps an ad-hoc connection.
    let o = fx.run(&["probe", "--driver", "sqlite", "--path", fx.db(), "--save", "kept"]);
    assert_exit(&o, 0);
    let o = fx.run(&["conn", "list", "--json"]);
    assert_exit(&o, 0);
    let names: Vec<String> = json_stdout(&o).as_array().unwrap().iter().filter_map(|c| c["name"].as_str().map(str::to_string)).collect();
    assert_eq!(names.len(), 2, "{names:?}");
    assert!(names.contains(&"kept".to_string()), "{names:?}");

    // Unknown name, and delete without --yes on a non-tty is refused.
    let o = fx.run(&["conn", "test", "missing"]);
    assert_exit(&o, 1);
    assert!(stderr(&o).contains("NOT_FOUND"), "{}", stderr(&o));
    let o = fx.run(&["conn", "delete", "kept"]);
    assert_exit(&o, 1);

    let o = fx.run(&["conn", "delete", "kept", "--yes"]);
    assert_exit(&o, 0);
    let o = fx.run(&["conn", "delete", "fixture", "--yes", "--json"]);
    assert_exit(&o, 0);
    assert_eq!(json_stdout(&o)["deleted"], id);
    let o = fx.run(&["conn", "list", "--json"]);
    assert_eq!(json_stdout(&o), serde_json::json!([]));
}

#[test]
fn conn_import_dbeaver_lists_without_saving() {
    let fx = Fixture::new("dbeaver");
    let path = fx.dir.path().join("data-sources.json");
    fs::write(
        &path,
        r#"{"connections":{"postgres-jdbc-1":{"provider":"postgresql","driver":"postgres-jdbc","name":"pg prod","configuration":{"host":"db.example.org","port":"5432","database":"app","user":"reader","url":"jdbc:postgresql://db.example.org:5432/app"}}}}"#,
    )
    .unwrap();
    let o = fx.run(&["conn", "import-dbeaver", path.to_str().unwrap(), "--json"]);
    assert_exit(&o, 0);
    let v = json_stdout(&o);
    let imported = v["imported"].as_array().expect("imported array");
    assert_eq!(imported.len(), 1, "{v}");
    assert_eq!(imported[0]["connection"]["name"], "pg prod", "{v}");
    assert_eq!(imported[0]["connection"]["host"], "db.example.org", "{v}");
    assert_eq!(v["saved"].as_array().map(Vec::len), Some(0), "not saved without --save: {v}");
    let o = fx.run(&["conn", "list", "--json"]);
    assert_eq!(json_stdout(&o), serde_json::json!([]));

    // Saving twice does not create a second "pg prod" (that would make --conn ambiguous).
    let o = fx.run(&["conn", "import-dbeaver", path.to_str().unwrap(), "--save", "--json"]);
    assert_exit(&o, 0);
    assert_eq!(json_stdout(&o)["saved"].as_array().map(Vec::len), Some(1), "{}", stdout(&o));
    let o = fx.run(&["conn", "import-dbeaver", path.to_str().unwrap(), "--save", "--json"]);
    assert_exit(&o, 0);
    let v = json_stdout(&o);
    assert_eq!(v["saved"].as_array().map(Vec::len), Some(0), "{v}");
    assert_eq!(v["skipped"], serde_json::json!(["pg prod"]), "{v}");
    let o = fx.run(&["conn", "list", "--json"]);
    assert_eq!(json_stdout(&o).as_array().map(Vec::len), Some(1));
}

// ---------------------------------------------------------------------------------------------
// conn edit / duplicate names
// ---------------------------------------------------------------------------------------------

#[test]
fn conn_edit_keeps_the_id_and_add_refuses_duplicates() {
    let fx = Fixture::new("conn-edit");
    let o = fx.run(&["conn", "add", "--name", "fixture", "--driver", "sqlite", "--path", fx.db(), "--json"]);
    assert_exit(&o, 0);
    let id = json_stdout(&o)["id"].as_str().expect("id").to_string();

    // Re-adding the same name (in any case) would make `--conn fixture` ambiguous: refused.
    let o = fx.run(&["conn", "add", "--name", "Fixture", "--driver", "sqlite", "--path", fx.db()]);
    assert_exit(&o, 1);
    assert!(stderr(&o).contains("INVALID") && stderr(&o).contains("conn edit"), "{}", stderr(&o));
    // So is keeping an ad-hoc connection under a taken name.
    let o = fx.run(&["probe", "--driver", "sqlite", "--path", fx.db(), "--save", "fixture"]);
    assert_exit(&o, 1);
    assert!(stderr(&o).contains("already exists"), "{}", stderr(&o));
    let o = fx.run(&["conn", "list", "--json"]);
    assert_eq!(json_stdout(&o).as_array().map(Vec::len), Some(1), "{}", stdout(&o));

    // Edit in place: same id, only the given fields change.
    let out_dir = fx.out("conn-out");
    let o = fx.run(&["conn", "edit", "fixture", "--name", "renamed", "--output-dir", &out_dir, "--json"]);
    assert_exit(&o, 0);
    let edited = json_stdout(&o);
    assert_eq!(edited["id"], id.as_str(), "{edited}");
    assert_eq!(edited["name"], "renamed", "{edited}");
    assert_eq!(edited["driver"], "sqlite", "{edited}");
    assert!(edited["outputDir"].as_str().unwrap_or_default().ends_with("conn-out"), "{edited}");
    assert!(edited["fields"]["path"].as_str().unwrap_or_default().ends_with("fixture.db"), "{edited}");
    let o = fx.run(&["conn", "edit", &id]);
    assert_exit(&o, 0);
    assert!(stdout(&o).contains("no changes"), "{}", stdout(&o));
    let o = fx.run(&["conn", "edit", "missing", "--host", "x"]);
    assert_exit(&o, 1);
    assert!(stderr(&o).contains("NOT_FOUND"), "{}", stderr(&o));

    // Another connection cannot be renamed onto that name.
    let o = fx.run(&["conn", "add", "--name", "other", "--driver", "sqlite", "--path", fx.db()]);
    assert_exit(&o, 0);
    let o = fx.run(&["conn", "edit", "other", "--name", "RENAMED"]);
    assert_exit(&o, 1);
    assert!(stderr(&o).contains("already exists"), "{}", stderr(&o));

    // --replace overwrites the definition but keeps the id; the connection still works.
    let o = fx.run(&["conn", "add", "--name", "renamed", "--driver", "sqlite", "--path", fx.db(), "--replace", "--json"]);
    assert_exit(&o, 0);
    let replaced = json_stdout(&o);
    assert_eq!(replaced["id"], id.as_str(), "{replaced}");
    assert!(replaced.get("outputDir").map(serde_json::Value::is_null).unwrap_or(true), "replace drops what the flags do not set: {replaced}");
    let out = fx.out("after_edit.csv");
    let o = fx.run(&["export", "--conn", "renamed", "--query", "select * from narrow", "--out", &out, "--yes"]);
    assert_exit(&o, 0);
    assert_eq!(line_count(&out), NARROW_ROWS as usize + 1);
}

#[test]
fn export_debug_log_never_prints_the_password() {
    let fx = Fixture::new("pw-log");
    let o = fx.run(&["conn", "add", "--name", "fixture", "--driver", "sqlite", "--path", fx.db()]);
    assert_exit(&o, 0);
    let out = fx.out("pw.csv");
    // A double quote in the password used to end the log masking's match early.
    let secret = "top\"se\"cret-tail";
    let o = fx
        .command()
        .env("BULK_TEST_PW", secret)
        .args(["export", "--conn", "fixture", "--password-env", "BULK_TEST_PW", "--query", "select 1 as x", "--out", &out, "--yes", "--log-level", "debug"])
        .output()
        .expect("spawn bulk-cli");
    assert_exit(&o, 0);
    let err = stderr(&o);
    assert!(err.contains("export request"), "the debug line is still logged: {err}");
    assert!(!err.contains("cret-tail") && !err.contains("top\\\"se"), "password leaked to stderr: {err}");
}

// ---------------------------------------------------------------------------------------------
// history / settings / diagnostics
// ---------------------------------------------------------------------------------------------

#[test]
fn history_lists_runs_after_the_process_exits() {
    let fx = Fixture::new("history");
    let o = fx.run(&["conn", "add", "--name", "fixture", "--driver", "sqlite", "--path", fx.db(), "--json"]);
    assert_exit(&o, 0);
    let id = json_stdout(&o)["id"].as_str().expect("id").to_string();
    let out = fx.out("hist.csv");
    let o = fx.run(&["export", "--conn", "fixture", "--query", "select * from narrow", "--out", &out, "--name", "nightly", "--yes"]);
    assert_exit(&o, 0);
    let adhoc_out = fx.out("adhoc.csv");
    let o = fx.export(&["--query", "select * from nasty", "--out", &adhoc_out, "--yes"]);
    assert_exit(&o, 0);

    // A fresh process sees both runs, newest first (`jobs` would be empty here).
    let o = fx.run(&["history", "--json"]);
    assert_exit(&o, 0);
    let all = json_stdout(&o);
    let runs = all.as_array().expect("array");
    assert_eq!(runs.len(), 2, "{all}");
    assert_eq!(runs[1]["label"], "nightly", "{all}");
    assert_eq!(runs[1]["state"], "done", "{all}");
    assert_eq!(runs[1]["rows"], NARROW_ROWS, "{all}");
    assert_eq!(runs[1]["connId"], id.as_str(), "{all}");

    let o = fx.run(&["history", "--conn", "fixture", "--json"]);
    assert_exit(&o, 0);
    let mine = json_stdout(&o);
    assert_eq!(mine.as_array().map(Vec::len), Some(1), "{mine}");
    let o = fx.run(&["history", "--limit", "1", "--json"]);
    assert_eq!(json_stdout(&o).as_array().map(Vec::len), Some(1));
    let o = fx.run(&["history", "--conn", "no-such-conn", "--json"]);
    assert_exit(&o, 0);
    assert_eq!(json_stdout(&o), serde_json::json!([]));

    let o = fx.run(&["history"]);
    assert_exit(&o, 0);
    let text = stdout(&o);
    assert!(text.contains("STARTED") && text.contains("nightly") && text.contains("fixture") && text.contains("500"), "{text}");
}

#[test]
fn settings_get_and_set_round_trip() {
    let fx = Fixture::new("settings");
    let out_dir = fx.out("default-out");
    let dir_pair = format!("default_output_dir={out_dir}");
    let o = fx.run(&["settings", "set", "max_concurrent_jobs=2", "verify-after-export=true", &dir_pair, "--json"]);
    assert_exit(&o, 0);
    let saved = json_stdout(&o);
    assert_eq!(saved["maxConcurrentJobs"], 2, "{saved}");
    assert_eq!(saved["verifyAfterExport"], true, "{saved}");

    // Persisted: a new process reads it back.
    let o = fx.run(&["settings", "get", "maxConcurrentJobs"]);
    assert_exit(&o, 0);
    assert_eq!(stdout(&o).trim(), "2");
    let o = fx.run(&["settings", "get"]);
    assert_exit(&o, 0);
    assert!(stdout(&o).contains("verify_after_export") && stdout(&o).contains("default-out"), "{}", stdout(&o));
    assert!(fs::read_to_string(fx.config.join("settings.toml")).expect("settings.toml").contains("maxConcurrentJobs = 2"), "settings.toml uses the camelCase IPC names");

    // The default output dir (which must exist, like any output dir) is used by an export
    // without --out / --dir.
    fs::create_dir_all(&out_dir).expect("create default output dir");
    let o = fx.export(&["--query", "select * from narrow", "--name", "via_settings", "--yes"]);
    assert_exit(&o, 0);
    assert_eq!(files_named(Path::new(&out_dir), "", ".csv").len(), 1, "{}", stderr(&o));

    // Validation: nothing is saved when one pair is bad.
    let bad: [&[&str]; 4] = [&["max_concurrent_jobs=1", "max_concurrent_jobs=9"], &["log_level=loud"], &["colour=blue"], &["monitor_interval_ms=10"]];
    for pairs in bad {
        let mut argv = vec!["settings", "set"];
        argv.extend_from_slice(pairs);
        let o = fx.run(&argv);
        assert_exit(&o, 1);
        assert!(stderr(&o).contains("INVALID"), "{pairs:?}: {}", stderr(&o));
    }
    let o = fx.run(&["settings", "get", "max_concurrent_jobs"]);
    assert_eq!(stdout(&o).trim(), "2");

    let o = fx.run(&["settings", "set", "default_output_dir=", "max_concurrent_jobs=2"]);
    assert_exit(&o, 0);
    assert!(stdout(&o).contains("default_output_dir:") && stdout(&o).contains("(unset)"), "{}", stdout(&o));
    let o = fx.run(&["settings", "set", "max_concurrent_jobs=2"]);
    assert_exit(&o, 0);
    assert!(stdout(&o).contains("unchanged"), "{}", stdout(&o));
}

#[test]
fn diagnostics_writes_a_zip_bundle() {
    let fx = Fixture::new("diag");
    let dir = fx.out("support");
    let o = fx.run(&["diagnostics", "--out", &dir, "--json"]);
    assert_exit(&o, 0);
    let b = json_stdout(&o);
    let path = PathBuf::from(b["path"].as_str().expect("path"));
    assert!(path.starts_with(&dir) && path.extension().map(|e| e == "zip").unwrap_or(false), "{b}");
    let size = fs::metadata(&path).expect("zip exists").len();
    assert_eq!(b["sizeBytes"], size, "{b}");
    let included: Vec<&str> = b["included"].as_array().expect("included").iter().filter_map(|v| v.as_str()).collect();
    assert!(included.contains(&"settings.toml") && included.contains(&"drivers.json") && included.contains(&"system.txt"), "{included:?}");
}

// ---------------------------------------------------------------------------------------------
// check / info / conn show / clear-secret flags
// ---------------------------------------------------------------------------------------------

#[test]
fn check_reads_the_query_with_the_connection_dialect() {
    let fx = Fixture::new("check");
    let o = fx.run(&["check", "--query", "select * from wide; -- all rows"]);
    assert_exit(&o, 0);
    assert!(stdout(&o).contains("ok") && stdout(&o).contains("standard SQL"), "{}", stdout(&o));

    // Refused queries exit 3 (what an extraction would be: rejected) and say why.
    let o = fx.run(&["check", "--query", "delete from wide", "--json"]);
    assert_exit(&o, 3);
    let v = json_stdout(&o);
    assert_eq!(v["ok"], false, "{v}");
    assert_eq!(v["kind"], "other", "{v}");
    assert_eq!(v["issues"][0]["code"], "SQL_NOT_SELECT", "{v}");
    let o = fx.run(&["check", "--query", "select 1; select 2"]);
    assert_exit(&o, 3);
    assert!(stdout(&o).contains("MULTI_STATEMENT"), "{}", stdout(&o));

    // A saved MySQL connection reads `#` as a comment, so the `;` behind it is not a split.
    let o = fx.run(&["conn", "add", "--name", "my", "--driver", "mysql", "--host", "mysql.invalid", "--user", "u"]);
    assert_exit(&o, 0);
    let hash = "select 1 # ; delete from wide";
    assert_exit(&fx.run(&["check", "--query", hash]), 3);
    let o = fx.run(&["check", "--conn", "my", "--query", hash, "--json"]);
    assert_exit(&o, 0);
    assert_eq!(json_stdout(&o)["statements"], 1);
    // ...and its backslash escapes make this one string literal two statements.
    let sneaky = r"select '\'' ; delete from wide -- '";
    assert_exit(&fx.run(&["check", "--query", sneaky]), 0);
    let o = fx.run(&["check", "--conn", "my", "--query", sneaky]);
    assert_exit(&o, 3);
    assert!(stdout(&o).contains("mysql") && stdout(&o).contains("MULTI_STATEMENT"), "{}", stdout(&o));

    // --sql FILE, and an unknown connection.
    let sql_file = fx.dir.path().join("q.sql");
    fs::write(&sql_file, "select id from wide\n").unwrap();
    assert_exit(&fx.run(&["check", "--sql", sql_file.to_str().unwrap()]), 0);
    let o = fx.run(&["check", "--conn", "missing", "--query", "select 1"]);
    assert_exit(&o, 1);
    assert!(stderr(&o).contains("NOT_FOUND"), "{}", stderr(&o));
}

#[test]
fn info_prints_version_platform_and_dirs() {
    let fx = Fixture::new("info");
    let o = fx.run(&["info", "--json"]);
    assert_exit(&o, 0);
    let v = json_stdout(&o);
    assert_eq!(v["version"], env!("CARGO_PKG_VERSION"), "{v}");
    assert!(["windows", "macos", "linux"].contains(&v["platform"].as_str().unwrap_or_default()), "{v}");
    assert!(v["arch"].as_str().is_some_and(|a| !a.is_empty()), "{v}");
    assert_eq!(PathBuf::from(v["configDir"].as_str().expect("configDir")), fx.config, "{v}");
    assert!(v["logDir"].as_str().is_some_and(|d| !d.is_empty()), "{v}");
    let o = fx.run(&["info"]);
    assert_exit(&o, 0);
    let text = stdout(&o);
    assert!(text.contains("Version") && text.contains("Config dir") && text.contains("Log dir"), "{text}");
}

#[test]
fn conn_show_and_clear_secret_flags() {
    let fx = Fixture::new("conn-show");
    let o = fx.run(&["conn", "add", "--name", "fixture", "--driver", "sqlite", "--path", fx.db(), "--json"]);
    assert_exit(&o, 0);
    let id = json_stdout(&o)["id"].as_str().expect("id").to_string();

    let o = fx.run(&["conn", "show", "fixture", "--json"]);
    assert_exit(&o, 0);
    let v = json_stdout(&o);
    assert_eq!(v["id"], id.as_str(), "{v}");
    assert_eq!(v["driver"], "sqlite", "{v}");
    assert_eq!(v["hasPassword"], false, "{v}");
    assert_eq!(v["hasSshSecret"], false, "{v}");
    let o = fx.run(&["conn", "show", &id]);
    assert_exit(&o, 0);
    let text = stdout(&o);
    assert!(text.contains(&id) && text.contains("Password") && text.contains("path"), "{text}");
    let o = fx.run(&["conn", "show", "missing"]);
    assert_exit(&o, 1);
    assert!(stderr(&o).contains("NOT_FOUND"), "{}", stderr(&o));

    // A clear request is a change of its own (not "no changes"), even with nothing stored.
    let o = fx.run(&["conn", "edit", "fixture", "--clear-password", "--clear-ssh-secret"]);
    assert_exit(&o, 0);
    let text = stdout(&o);
    assert!(text.contains("updated connection") && text.contains("no stored password to remove") && text.contains("no stored SSH secret to remove"), "{text}");
    let o = fx.run(&["conn", "edit", "fixture", "--clear-password", "--json"]);
    assert_exit(&o, 0);
    assert_eq!(json_stdout(&o)["hasPassword"], false);
    // Clearing and replacing the same secret at once is refused by the argument parser.
    assert_exit(&fx.run(&["conn", "edit", "fixture", "--clear-password", "--password-env", "PW"]), 1);
    assert_exit(&fx.run(&["conn", "edit", "fixture", "--clear-ssh-secret", "--ssh-password-env", "PW"]), 1);
}

#[test]
fn catalog_table_listings_accept_refresh() {
    let fx = Fixture::new("catalog-refresh");
    for sub in ["columns", "indexes", "ddl"] {
        let o = fx.run(&["catalog", sub, "--driver", "sqlite", "--path", fx.db(), "--table", "nasty", "--refresh", "--json"]);
        assert_exit(&o, 0);
        assert!(!stdout(&o).trim().is_empty(), "{sub}");
    }
}
