//! `bulk-cli copy` and `conn --write-allowed` end to end on SQLite files (docs/db-copy.md 6.3,
//! owner copy-data): the binary runs as a child process with `BULKDB_CONFIG_DIR` in a temp dir
//! and `BULKDB_CREDENTIALS=memory`, stdin closed (non-interactive).
//!
//! Exit codes: 0 done, 3 rejected / `--expect-plan` mismatch, 4 acknowledgement or
//! confirmation needed, 5 failed or done with problems with a stored result.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

struct Fx {
    _dir: tempfile::TempDir,
    root: PathBuf,
    src: PathBuf,
    tgt: PathBuf,
    config: PathBuf,
}

impl Fx {
    fn new() -> Fx {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let src = root.join("src.db");
        let tgt = root.join("tgt.db");
        rusqlite_like::open(&src);
        let config = root.join("config");
        fs::create_dir_all(&config).unwrap();
        let fx = Fx { _dir: dir, root, src, tgt, config };
        fx.ok(&["conn", "add", "--name", "src", "--driver", "sqlite", "--path", &fx.p(&fx.src)]);
        fx.ok(&["conn", "add", "--name", "tgt", "--driver", "sqlite", "--path", &fx.p(&fx.tgt), "--write-allowed", "true"]);
        fx
    }

    fn p(&self, p: &Path) -> String {
        p.to_string_lossy().into_owned()
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_bulk-cli"))
            .env("BULKDB_CONFIG_DIR", &self.config)
            .env("BULKDB_CREDENTIALS", "memory")
            .env_remove("RUST_LOG")
            .stdin(Stdio::null())
            .args(args)
            .output()
            .expect("spawn bulk-cli")
    }

    fn ok(&self, args: &[&str]) -> Output {
        let o = self.run(args);
        expect(&o, 0);
        o
    }

    fn copy(&self, sub: &str, extra: &[&str]) -> Output {
        let mut args = vec!["copy", sub, "--source", "src", "--source-db", "main", "--target", "tgt", "--target-db", "main"];
        args.extend_from_slice(extra);
        self.run(&args)
    }

    fn sql(&self, path: &Path, sql: &str) {
        rusqlite_like::exec(path, sql);
    }

    fn count(&self, path: &Path, table: &str) -> i64 {
        rusqlite_like::count(path, table)
    }
}

/// Tiny SQLite helpers for the fixtures.
mod rusqlite_like {
    use std::path::Path;

    pub fn open(path: &Path) {
        exec(
            path,
            "CREATE TABLE customers (id INTEGER PRIMARY KEY, name TEXT);
             INSERT INTO customers VALUES (1, '홍길동'), (2, NULL), (3, '');
             CREATE TABLE orders (id INTEGER PRIMARY KEY, customer_id INTEGER, amount INTEGER);
             WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM n WHERE x < 500) INSERT INTO orders SELECT x, x % 3 + 1, x FROM n;",
        );
    }

    pub fn exec(path: &Path, sql: &str) {
        rusqlite::Connection::open(path).unwrap().execute_batch(sql).expect("sqlite exec");
    }

    pub fn count(path: &Path, table: &str) -> i64 {
        rusqlite::Connection::open(path).unwrap().query_row(&format!("SELECT count(*) FROM \"{table}\""), [], |r| r.get(0)).expect("sqlite count")
    }
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

fn expect(o: &Output, code: i32) {
    assert_eq!(o.status.code().unwrap_or(-1), code, "exit code\n--- stdout\n{}\n--- stderr\n{}", stdout(o), stderr(o));
}

#[test]
fn plan_run_and_exit_codes() {
    let fx = Fx::new();
    // Plan (text and JSON).
    let o = fx.copy("plan", &["--sql"]);
    expect(&o, 0);
    let text = stdout(&o);
    assert!(text.starts_with("plan "), "{text}");
    assert!(text.contains("INSERT INTO \"orders\""), "--sql prints the statements: {text}");
    let o = fx.copy("plan", &["--json"]);
    expect(&o, 0);
    let plan: serde_json::Value = serde_json::from_slice(&o.stdout).expect("one JSON document");
    let plan_id = plan["planId"].as_str().unwrap().to_string();
    assert_eq!(plan_id.len(), 32);

    // --expect-plan mismatch: exit 3, nothing written.
    let o = fx.copy("run", &["--expect-plan", "0000", "--yes"]);
    expect(&o, 3);
    assert!(stderr(&o).contains("PLAN_CHANGED"), "{}", stderr(&o));
    assert!(!fx.tgt.exists());

    // Run bound to the reviewed plan: exit 0.
    let o = fx.copy("run", &["--expect-plan", &plan_id, "--yes", "--progress"]);
    expect(&o, 0);
    assert_eq!(fx.count(&fx.tgt, "orders"), 500);
    assert_eq!(fx.count(&fx.tgt, "customers"), 3);
    assert!(stdout(&o).contains("copy done"), "{}", stdout(&o));

    // Every table exists now: without option flags the plan only skips them.
    let o = fx.copy("plan", &["--json"]);
    expect(&o, 0);
    let plan: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
    let tables: Vec<&serde_json::Value> = plan["items"].as_array().unwrap().iter().filter(|i| i["kind"] == "table").collect();
    assert!(!tables.is_empty());
    assert!(tables.iter().all(|i| i["action"] == "skip_existing"), "{tables:?}");
    // The skip needs its acknowledgement: without a terminal and without --ack: exit 4.
    let o = fx.copy("run", &[]);
    expect(&o, 4);
    assert!(stderr(&o).contains("TABLES_SKIPPED_EXISTING"), "{}", stderr(&o));
    // --ack CODE acknowledges it.
    let o = fx.copy("run", &["--ack", "TABLES_SKIPPED_EXISTING", "--json"]);
    expect(&o, 0);
    let result: serde_json::Value = serde_json::from_slice(&o.stdout).expect("CopyResult JSON");
    assert_eq!(result["skippedExisting"], 2);

    // A table name or item id that matches nothing is refused before anything runs (it would
    // otherwise be ignored and the table copied under the global policy).
    for args in [
        &["--existing", "truncate", "--exclude-table", "nope", "--yes"][..],
        &["--existing", "truncate", "--existing-for", "main.orders=skip", "--yes"][..],
        &["--existing", "truncate", "--exclude", "table:\"nope\"", "--yes"][..],
    ] {
        let o = fx.copy("run", args);
        expect(&o, 1);
        assert!(stderr(&o).contains("in the plan") || stderr(&o).contains("no schema") || stderr(&o).contains("no plan item"), "{}", stderr(&o));
    }
    assert_eq!(fx.count(&fx.tgt, "orders"), 500, "nothing was truncated");
    // A known name resolves to the plan's item id.
    let o = fx.copy("plan", &["--existing", "truncate", "--existing-for", "orders=skip", "--json"]);
    expect(&o, 0);
    let plan: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
    let orders = plan["items"].as_array().unwrap().iter().find(|i| i["kind"] == "table" && i["name"] == "orders").unwrap();
    assert_eq!(orders["action"], "skip_existing", "{orders}");

    // A failing table (a target trigger deletes rows during an append): exit 5 with a result.
    fx.sql(&fx.tgt, "DELETE FROM orders; CREATE TRIGGER drop_even AFTER INSERT ON orders BEGIN DELETE FROM orders WHERE id = NEW.id AND NEW.id % 2 = 0; END;");
    let o = fx.copy("run", &["--existing", "append", "--exclude-table", "customers", "--yes"]);
    expect(&o, 5);
    assert!(stdout(&o).contains("VERIFY_MISMATCH"), "{}", stdout(&o));
    assert!(stdout(&o).contains("copy resume --run"), "{}", stdout(&o));

    // Runs, status, forget.
    let o = fx.run(&["copy", "runs", "--json"]);
    expect(&o, 0);
    let runs: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
    let runs = runs.as_array().unwrap();
    assert!(runs.len() >= 3);
    let failed = runs.iter().find(|r| r["state"] == "failed").expect("the failed run is listed");
    assert_eq!(failed["resumable"], true);
    let id = failed["runId"].as_str().unwrap().to_string();
    let o = fx.run(&["copy", "status", "--run", &id, "--json"]);
    expect(&o, 0);
    let detail: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
    assert!(detail["result"]["tables"].is_array());
    fx.ok(&["copy", "forget", "--run", &id]);
    let o = fx.run(&["copy", "status", "--run", &id]);
    assert_ne!(o.status.code(), Some(0));
}

#[test]
fn write_permission_and_refusals() {
    let fx = Fx::new();
    // `conn list` / `conn show` print the flag.
    let o = fx.ok(&["conn", "show", "tgt", "--json"]);
    let c: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(c["writeAllowed"], true);
    assert!(stdout(&fx.ok(&["conn", "list"])).contains("WRITE"));
    // Forbid it: the plan has a blocker, `run` exits 3 and writes nothing.
    fx.ok(&["conn", "edit", "tgt", "--write-allowed", "false"]);
    let o = fx.copy("run", &["--yes"]);
    expect(&o, 3);
    assert!(stdout(&o).contains("TARGET_NOT_WRITABLE"), "{}", stdout(&o));
    assert!(!fx.tgt.exists());
    // Production: --write-allowed true needs --yes without a terminal.
    fx.ok(&["conn", "edit", "tgt", "--tag", "production"]);
    let o = fx.run(&["conn", "edit", "tgt", "--write-allowed", "true"]);
    expect(&o, 4);
    fx.ok(&["conn", "edit", "tgt", "--write-allowed", "true", "--yes"]);
    // A production target needs --confirm-production; --yes does not replace it.
    let o = fx.copy("run", &["--yes"]);
    expect(&o, 4);
    assert!(stderr(&o).contains("--confirm-production"), "{}", stderr(&o));
    let o = fx.copy("run", &["--yes", "--confirm-production", "tgt"]);
    expect(&o, 0);
    // Pointing the connection elsewhere clears the permission.
    let other = fx.root.join("other.db");
    fx.ok(&["conn", "edit", "tgt", "--path", &fx.p(&other)]);
    let o = fx.ok(&["conn", "show", "tgt", "--json"]);
    let c: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
    assert!(c.get("writeAllowed").is_none() || c["writeAllowed"] == false, "{c}");
    // Source == target: SAME_DATABASE.
    let o = fx.run(&["copy", "plan", "--source", "src", "--source-db", "main", "--target", "src", "--target-db", "main"]);
    expect(&o, 3);
    assert!(stdout(&o).contains("SAME_DATABASE"), "{}", stdout(&o));
    // Dry run: nothing written.
    fx.ok(&["conn", "edit", "tgt", "--write-allowed", "true", "--yes"]);
    let o = fx.copy("run", &["--yes", "--dry-run", "--confirm-production", "tgt"]);
    expect(&o, 0);
    assert!(stdout(&o).contains("dry run"), "{}", stdout(&o));
    assert!(!other.exists(), "a dry run creates no target file");
}
