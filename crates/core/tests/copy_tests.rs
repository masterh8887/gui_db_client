//! DB copy end to end without docker (docs/db-copy.md section 9, owner copy-data): SQLite ->
//! SQLite through the public `BulkDb::copy_*` facade, and the in-process fake MySQL 4.0 EUC-KR
//! server as a source into a `RecordingTarget` (`BulkDbOptions.copy_target_opener`).
//!
//! Cases:
//! * `sqlite_full_copy_schema_and_data` — tables, index, view; NULLs, Korean text, empty
//!   strings, blobs with NUL bytes; counts verified; the source file is unchanged.
//! * `default_skip_existing_table` — the default policy skips an existing table untouched and
//!   `TABLES_SKIPPED_EXISTING` needs its acknowledgement (the job waits in `warned`).
//! * `existing_policies_truncate_drop_recreate_append_and_overrides`.
//! * `cancel_mid_table_then_resume` — the target's write lock held by a second connection
//!   blocks the writer; cancel within 3 s; resume reloads only the interrupted table.
//! * `verification_mismatch_is_reported` — a target trigger deletes rows during an append.
//! * `append_checksum_compares_only_the_added_rows` — checksum of an append into a non-empty
//!   table (target sum after − before).
//! * `cancel_lists_every_recreated_table_left_behind` — drop_recreate tables that never
//!   loaded are `created_table` + `emptied_table`.
//! * `dry_run_writes_nothing` — byte-identical existing target, no file for a missing one.
//! * `refusals` — not write-allowed, the same database, production confirmation.
//! * `target_in_use_across_two_cores` — two `BulkDb` on one config dir.
//! * `memory_stays_bounded_for_a_large_table` — wide rows, peak RSS < 100 MB.
//! * `fake_mysql40_euckr_into_recording_target` — translated DDL, prelude on every session,
//!   UTF-8 LOAD payload / INSERT literals, zero and invalid dates, value fixes.
//! * `fake_mysql40_latin1_holding_cp949_is_blocked` — `SRC_LATIN1_KOREAN`.
//! * `mysql_target_insert_path_against_a_fake_55_server` — the real `MysqlTarget` (facts,
//!   prelude, session assertion, chunk transaction, INSERT literals, count) on the fake server.
//! * `mysql_target_reports_committed_rows_after_a_failed_chunk` — the real `MysqlTarget`: one
//!   chunk committed, the next rolled back; `take_failed_outcome` reports the committed rows.
//! * `failed_mysql_table_reports_committed_rows_as_partial` — `left_behind.partial_rows` is the
//!   writer's committed count, not the rows sent.

mod support;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bulk_core::credentials::MemoryStore;
use bulk_core::service::{BulkDb, BulkDbOptions};
use bulk_core::types::*;

const DONE_TIMEOUT: Duration = Duration::from_secs(120);

// ---------------------------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------------------------

struct H {
    dir: tempfile::TempDir,
    db: Arc<BulkDb>,
    src_id: String,
    tgt_id: String,
    src: PathBuf,
    tgt: PathBuf,
}

fn sqlite_conn(name: &str, path: &Path) -> Connection {
    let mut fields = BTreeMap::new();
    fields.insert("path".to_string(), path.display().to_string());
    Connection { name: name.into(), driver: "sqlite".into(), fields, ..Default::default() }
}

async fn core(config: &Path, opener: Option<Arc<dyn TargetOpener>>) -> Arc<BulkDb> {
    BulkDb::new(BulkDbOptions {
        config_dir: Some(config.to_path_buf()),
        credentials: Some(Box::new(MemoryStore::default())),
        init_logging: false,
        prompts: None,
        copy_target_opener: opener,
        ..Default::default()
    })
    .await
    .unwrap()
}

/// Save `conn` and mark it write-allowed (only an existing id can become write-allowed).
fn save_writable(db: &BulkDb, conn: Connection) -> Connection {
    let saved = db.conn_save(ConnectionInput { connection: conn, ..Default::default() }).unwrap();
    let mut again = saved.clone();
    again.write_allowed = Some(true);
    let w = db.conn_save(ConnectionInput { connection: again, ..Default::default() }).unwrap();
    assert_eq!(w.write_allowed, Some(true), "write_allowed is set on an existing id");
    w
}

fn make_source(path: &Path, big_rows: u64) {
    let c = rusqlite::Connection::open(path).unwrap();
    c.execute_batch(
        "CREATE TABLE customers (id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT NOT NULL, note TEXT, photo BLOB, score REAL);
         CREATE INDEX customers_name ON customers(name);
         CREATE TABLE orders (id INTEGER PRIMARY KEY, customer_id INTEGER REFERENCES customers(id), amount INTEGER, memo TEXT);
         CREATE VIEW big_orders AS SELECT * FROM orders WHERE amount > 100;
         INSERT INTO customers (name, note, photo, score) VALUES ('홍길동', '서울 / 강남구', x'00FF0001', 1.5);
         INSERT INTO customers (name, note, photo, score) VALUES ('김철수', NULL, NULL, NULL);
         INSERT INTO customers (name, note, photo, score) VALUES ('', '', x'', -0.25);
         INSERT INTO customers (name, note, photo, score) VALUES ('tab	and
newline \\ \"quote\" ''single''', '똠방각하 😀', x'5c4e0a', 1e300);",
    )
    .unwrap();
    c.execute_batch(&format!(
        "WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM n WHERE x < {big_rows})
         INSERT INTO orders (id, customer_id, amount, memo) SELECT x, (x % 4) + 1, x * 7 % 1000, '주문 ' || x FROM n"
    ))
    .unwrap();
}

async fn harness(big_rows: u64) -> H {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("source.db");
    let tgt = dir.path().join("target.db");
    make_source(&src, big_rows);
    let db = core(&dir.path().join("config"), None).await;
    let s = db.conn_save(ConnectionInput { connection: sqlite_conn("src", &src), ..Default::default() }).unwrap();
    let t = save_writable(&db, sqlite_conn("tgt", &tgt));
    H { dir, db, src_id: s.id, tgt_id: t.id, src, tgt }
}

fn request(h: &H) -> CopyRequest {
    CopyRequest {
        source: CopyEndpoint { conn_id: h.src_id.clone(), database: "main".into(), schema: None, password: None },
        target: CopyEndpoint { conn_id: h.tgt_id.clone(), database: "main".into(), schema: None, password: None },
        selection: CopySelection::All,
        options: CopyOptions::default(),
        name: None,
    }
}

fn keys(plan: &CopyPlan) -> Vec<String> {
    plan.acks.iter().map(|a| a.key.clone()).collect()
}

async fn wait_terminal(db: &BulkDb, job: &str) -> CopyStatus {
    let t0 = Instant::now();
    loop {
        let s = db.copy_status(job).unwrap();
        if s.state.is_terminal() {
            return s;
        }
        assert!(t0.elapsed() < DONE_TIMEOUT, "copy job {job} did not finish: {:?}", s.state);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn wait_state(db: &BulkDb, job: &str, want: JobState) -> CopyStatus {
    let t0 = Instant::now();
    loop {
        let s = db.copy_status(job).unwrap();
        if s.state == want || s.state.is_terminal() {
            return s;
        }
        assert!(t0.elapsed() < DONE_TIMEOUT, "copy job {job} never reached {want:?}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Plan, acknowledge everything, start, wait.
async fn run(db: &BulkDb, req: &CopyRequest) -> (CopyPlan, CopyStatus) {
    let plan = db.copy_plan(req, None).await.unwrap();
    assert!(plan.blockers.is_empty(), "unexpected blockers: {:?}", plan.blockers);
    let job = db.copy_start(req.clone(), plan.plan_id.clone(), keys(&plan)).await.unwrap();
    let st = wait_terminal(db, &job).await;
    (plan, st)
}

fn query(path: &Path, sql: &str) -> Vec<Vec<String>> {
    let c = rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let mut stmt = c.prepare(sql).unwrap();
    let n = stmt.column_count();
    let rows = stmt
        .query_map([], |r| {
            Ok((0..n)
                .map(|i| match r.get_ref(i).unwrap() {
                    rusqlite::types::ValueRef::Null => "NULL".to_string(),
                    rusqlite::types::ValueRef::Integer(v) => v.to_string(),
                    rusqlite::types::ValueRef::Real(v) => format!("{v:?}"),
                    rusqlite::types::ValueRef::Text(t) => String::from_utf8_lossy(t).into_owned(),
                    rusqlite::types::ValueRef::Blob(b) => format!("x'{}'", b.iter().map(|x| format!("{x:02x}")).collect::<String>()),
                })
                .collect())
        })
        .unwrap();
    rows.map(|r| r.unwrap()).collect()
}

fn hash_file(p: &Path) -> Vec<u8> {
    std::fs::read(p).unwrap()
}

fn table_result<'a>(r: &'a CopyResult, name: &str) -> &'a CopyTableResult {
    r.tables.iter().find(|t| t.target_name == name).unwrap_or_else(|| panic!("no result for {name}: {:?}", r.tables.iter().map(|t| &t.target_name).collect::<Vec<_>>()))
}

// ---------------------------------------------------------------------------------------------
// SQLite -> SQLite
// ---------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sqlite_full_copy_schema_and_data() {
    let h = harness(2_500).await;
    let before = hash_file(&h.src);
    let (plan, st) = run(&h.db, &request(&h)).await;
    assert_eq!(st.state, JobState::Done, "{:?}", st.result);
    let r = st.result.clone().unwrap();
    assert!(!r.dry_run && r.resumable);
    assert_eq!(table_result(&r, "customers").rows_written, 4);
    assert_eq!(table_result(&r, "orders").rows_written, 2_500);
    for t in &r.tables {
        assert_eq!(t.verify, VerifyOutcome::Match, "{}", t.target_name);
        assert_eq!(t.target_count, Some(t.rows_read));
        assert_eq!(t.method, Some(LoadMethod::SqliteInsert));
    }
    assert!(r.verified, "count verification: {r:?}");
    assert_eq!(r.problems, 0);
    // Values survive exactly.
    let src = query(&h.src, "SELECT id, name, note, photo, score FROM customers ORDER BY id");
    let tgt = query(&h.tgt, "SELECT id, name, note, photo, score FROM customers ORDER BY id");
    assert_eq!(src, tgt);
    assert_eq!(tgt[0][1], "홍길동");
    assert_eq!(tgt[1][2], "NULL");
    assert_eq!(tgt[2][1], "");
    assert_eq!(tgt[0][3], "x'00ff0001'");
    assert_eq!(query(&h.src, "SELECT sum(amount), count(*) FROM orders"), query(&h.tgt, "SELECT sum(amount), count(*) FROM orders"));
    // Schema objects.
    let objects = query(&h.tgt, "SELECT type, name FROM sqlite_master WHERE name NOT LIKE 'sqlite\\_%' ESCAPE '\\' ORDER BY type, name");
    let names: Vec<String> = objects.iter().map(|r| format!("{}:{}", r[0], r[1])).collect();
    for want in ["table:customers", "table:orders", "index:customers_name", "view:big_orders"] {
        assert!(names.contains(&want.to_string()), "{want} missing in {names:?}");
    }
    // The AUTOINCREMENT counter follows the source.
    assert_eq!(query(&h.tgt, "SELECT seq FROM sqlite_sequence WHERE name = 'customers'"), vec![vec!["4".to_string()]]);
    // Progress and the stored run.
    assert_eq!(st.progress.tables_total, st.progress.tables_done);
    assert!(st.plan.is_some());
    assert_eq!(plan.plan_id, st.plan.as_ref().unwrap().plan_id);
    let runs = h.db.copy_runs(Some(&h.tgt_id), 10).unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].state, JobState::Done);
    assert!(!runs[0].in_use);
    assert!(!runs[0].resumable, "a clean `done` run has nothing left to resume");
    let detail = h.db.copy_run(&runs[0].run_id).unwrap();
    assert!(detail.result.is_some());
    assert!(detail.request.source.password.is_none());
    assert_eq!(hash_file(&h.src), before, "the source file is never written");
    // A finished run can be forgotten.
    h.db.copy_forget(&runs[0].run_id).unwrap();
    assert!(h.db.copy_runs(None, 10).unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn default_skip_existing_table() {
    let h = harness(100).await;
    {
        let c = rusqlite::Connection::open(&h.tgt).unwrap();
        c.execute_batch("CREATE TABLE orders (id INTEGER PRIMARY KEY, customer_id INTEGER, amount INTEGER, memo TEXT); INSERT INTO orders VALUES (1, 1, 1, 'keep me');")
            .unwrap();
    }
    let req = request(&h);
    let plan = h.db.copy_plan(&req, None).await.unwrap();
    let orders = plan.items.iter().find(|i| i.kind == CopyObjectKind::Table && i.name == "orders").unwrap();
    assert_eq!(orders.action, PlanAction::SkipExisting, "the default policy is skip");
    assert_eq!(orders.target_has_rows, Some(true));
    let ack = plan.acks.iter().find(|a| a.code == "TABLES_SKIPPED_EXISTING").expect("TABLES_SKIPPED_EXISTING needs an acknowledgement");
    // Without the acknowledgement the job waits in `warned`.
    let job = h.db.copy_start(req.clone(), plan.plan_id.clone(), Vec::new()).await.unwrap();
    let st = wait_state(&h.db, &job, JobState::Warned).await;
    assert_eq!(st.state, JobState::Warned);
    let summary = h.db.job_get(&job).unwrap();
    assert!(summary.preflight.unwrap().warnings.iter().any(|w| w.code == ack.key));
    h.db.job_resume(&job, keys(&plan)).unwrap();
    let st = wait_terminal(&h.db, &job).await;
    assert_eq!(st.state, JobState::Done, "{:?}", st.result);
    let r = st.result.unwrap();
    assert_eq!(r.skipped_existing, 1);
    assert_eq!(table_result(&r, "orders").state, CopyItemState::Skipped);
    assert_eq!(query(&h.tgt, "SELECT count(*), max(memo) FROM orders"), vec![vec!["1".to_string(), "keep me".to_string()]], "skipped table untouched");
    assert_eq!(query(&h.tgt, "SELECT count(*) FROM customers"), vec![vec!["4".to_string()]]);
}

/// A job that waits in `warned` is re-checked against the target connection when it is resumed:
/// if 쓰기 허용 was cleared in the meantime, the acknowledged plan is rejected (TARGET_NOT_WRITABLE),
/// never written. (Finding: a warned job resumed without re-checking write permission.)
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn warned_job_rechecks_write_permission_before_running() {
    let h = harness(50).await;
    {
        let c = rusqlite::Connection::open(&h.tgt).unwrap();
        c.execute_batch("CREATE TABLE orders (id INTEGER PRIMARY KEY, customer_id INTEGER, amount INTEGER, memo TEXT); INSERT INTO orders VALUES (1, 1, 1, 'keep me');")
            .unwrap();
    }
    let req = request(&h);
    let plan = h.db.copy_plan(&req, None).await.unwrap();
    assert!(plan.acks.iter().any(|a| a.code == "TABLES_SKIPPED_EXISTING"), "the default skip policy waits for an acknowledgement");
    let job = h.db.copy_start(req.clone(), plan.plan_id.clone(), Vec::new()).await.unwrap();
    wait_state(&h.db, &job, JobState::Warned).await;
    // Someone clears 쓰기 허용 on the target while the job waits (Welcome row menu, `conn edit`, …).
    let stored = h.db.conn_get(&h.tgt_id).unwrap();
    let cleared = h.db.conn_save(ConnectionInput { connection: Connection { write_allowed: Some(false), ..stored }, ..Default::default() }).unwrap();
    assert_eq!(cleared.write_allowed, None, "write permission is cleared");
    // Acknowledge and resume: the job must reject, not write.
    h.db.job_resume(&job, keys(&plan)).unwrap();
    let st = wait_terminal(&h.db, &job).await;
    assert_eq!(st.state, JobState::Rejected, "{:?}", st.result);
    let blockers = st.plan.as_ref().map(|p| p.blockers.clone()).unwrap_or_default();
    assert!(
        blockers.iter().any(|b| b.code == "TARGET_NOT_WRITABLE")
            || h.db.job_get(&job).ok().and_then(|j| j.error).is_some_and(|e| e.detail.unwrap_or_default().contains("TARGET_NOT_WRITABLE")),
        "rejected for TARGET_NOT_WRITABLE: {blockers:?}"
    );
    // The skipped/existing target row is untouched (nothing was loaded).
    assert_eq!(query(&h.tgt, "SELECT count(*), max(memo) FROM orders"), vec![vec!["1".to_string(), "keep me".to_string()]]);
    assert!(query(&h.tgt, "SELECT name FROM sqlite_master WHERE type='table' AND name='customers'").is_empty(), "no new table was created");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn existing_policies_truncate_drop_recreate_append_and_overrides() {
    let h = harness(50).await;
    let seed = |sql: &str| rusqlite::Connection::open(&h.tgt).unwrap().execute_batch(sql).unwrap();
    seed("CREATE TABLE orders (id INTEGER PRIMARY KEY, customer_id INTEGER, amount INTEGER, memo TEXT); INSERT INTO orders VALUES (1000, 1, 1, 'old');
          CREATE TABLE customers (id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT NOT NULL, note TEXT, photo BLOB, score REAL, extra TEXT); INSERT INTO customers (id, name) VALUES (99, 'old');");
    // truncate everything.
    let mut req = request(&h);
    req.options.existing = ExistingTablePolicy::Truncate;
    let (plan, st) = run(&h.db, &req).await;
    assert!(
        plan.items.iter().filter(|i| i.kind == CopyObjectKind::Table).all(|i| i.action == PlanAction::Truncate),
        "{:?}",
        plan.items.iter().map(|i| (&i.id, i.action)).collect::<Vec<_>>()
    );
    assert_eq!(st.state, JobState::Done, "{:?}", st.result);
    assert_eq!(query(&h.tgt, "SELECT count(*) FROM orders"), vec![vec!["50".to_string()]]);
    assert_eq!(query(&h.tgt, "SELECT count(*) FROM customers WHERE name = 'old'"), vec![vec!["0".to_string()]]);
    // append into orders (keys differ): rows add up; drop_recreate customers via an override.
    seed("DELETE FROM orders WHERE id > 10; UPDATE orders SET id = id + 5000;");
    let mut req = request(&h);
    req.options.existing = ExistingTablePolicy::Append;
    let customers_id = query(&h.tgt, "SELECT 1").len(); // keep the connection warm
    let _ = customers_id;
    let plan = h.db.copy_plan(&req, None).await.unwrap();
    let cust = plan.items.iter().find(|i| i.kind == CopyObjectKind::Table && i.name == "customers").unwrap().id.clone();
    req.options.existing_overrides = vec![ItemPolicy { item_id: cust.clone(), policy: ExistingTablePolicy::DropRecreate }];
    let (plan, st) = run(&h.db, &req).await;
    assert_eq!(plan.items.iter().find(|i| i.id == cust).unwrap().action, PlanAction::DropRecreate);
    assert_eq!(plan.items.iter().find(|i| i.kind == CopyObjectKind::Table && i.name == "orders").unwrap().action, PlanAction::Append);
    assert_eq!(st.state, JobState::Done, "{:?}", st.result);
    assert_eq!(query(&h.tgt, "SELECT count(*) FROM orders"), vec![vec!["60".to_string()]], "10 kept + 50 appended");
    let r = st.result.unwrap();
    assert_eq!(table_result(&r, "orders").target_count, Some(50), "append verifies the difference");
    // drop_recreate: the extra column is gone, the data is the source's.
    let cols = query(&h.tgt, "SELECT name FROM pragma_table_info('customers')");
    assert!(!cols.iter().any(|c| c[0] == "extra"), "{cols:?}");
    assert_eq!(query(&h.tgt, "SELECT count(*) FROM customers"), vec![vec!["4".to_string()]]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancel_mid_table_then_resume() {
    let h = harness(200_000).await;
    let mut req = request(&h);
    req.options.batch_rows = Some(1000);
    let plan = h.db.copy_plan(&req, None).await.unwrap();
    let job = h.db.copy_start(req.clone(), plan.plan_id.clone(), keys(&plan)).await.unwrap();
    // Wait until the big table has rows written.
    let t0 = Instant::now();
    loop {
        let s = h.db.copy_status(&job).unwrap();
        if s.progress.tables.iter().any(|t| t.item_id.contains("orders") && t.rows_written > 0) {
            break;
        }
        assert!(!s.state.is_terminal(), "finished before it could be interrupted: {:?}", s.result);
        assert!(t0.elapsed() < Duration::from_secs(60));
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    // Hold the target's write lock so the writer blocks between two batches.
    let tgt = h.tgt.clone();
    let (locked_tx, locked_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let holder = std::thread::spawn(move || {
        let c = rusqlite::Connection::open(&tgt).unwrap();
        c.busy_timeout(Duration::from_millis(0)).unwrap();
        let t0 = Instant::now();
        loop {
            match c.execute_batch("BEGIN IMMEDIATE") {
                Ok(()) => break,
                Err(_) if t0.elapsed() < Duration::from_secs(30) => std::thread::yield_now(),
                Err(e) => panic!("cannot take the write lock: {e}"),
            }
        }
        locked_tx.send(()).unwrap();
        release_rx.recv().unwrap();
        c.execute_batch("ROLLBACK").unwrap();
    });
    locked_rx.recv_timeout(Duration::from_secs(40)).expect("write lock taken");
    let t_cancel = Instant::now();
    h.db.job_cancel(&job).unwrap();
    let st = wait_terminal(&h.db, &job).await;
    assert_eq!(st.state, JobState::Cancelled, "{:?}", st.result);
    assert!(t_cancel.elapsed() < Duration::from_secs(3), "cancelled in {:?}", t_cancel.elapsed());
    release_tx.send(()).unwrap();
    holder.join().unwrap();
    let r = st.result.expect("a cancelled run stores its result");
    assert!(r.resumable);
    // What stays in the target: when the cancel caught the big table mid-load, its whole
    // committed batches, reported as partial rows (the open batch was rolled back).
    let orders = r.tables.iter().find(|t| t.item_id.contains("orders")).expect("orders result");
    let in_target: u64 = query(&h.tgt, "SELECT count(*) FROM orders")[0][0].parse().unwrap();
    let partial = r.left_behind.iter().find(|l| l.kind == LeftBehindKind::PartialRows && l.item_id == orders.item_id);
    if orders.state == CopyItemState::Cancelled {
        assert!(in_target.is_multiple_of(1000), "{in_target}");
        assert_eq!(orders.rows_written, in_target, "{orders:?}");
        assert_eq!(partial.map(|l| l.rows), (in_target > 0).then_some(Some(in_target)), "{:?}", r.left_behind);
    } else {
        assert!(partial.is_none(), "a finished table leaves no partial rows: {:?}", r.left_behind);
    }
    let runs = h.db.copy_runs(None, 10).unwrap();
    assert!(runs[0].resumable, "{:?}", runs[0]);
    // Resume: only the interrupted table is reloaded.
    let resume = CopyResumeRequest { run_id: st.run_id.clone(), ..Default::default() };
    let job2 = h.db.copy_resume(resume.clone()).await.unwrap();
    let mut st2 = wait_state(&h.db, &job2, JobState::Warned).await;
    if st2.state == JobState::Warned {
        let acks: Vec<String> = st2.plan.as_ref().unwrap().acks.iter().map(|a| a.key.clone()).collect();
        h.db.job_resume(&job2, acks).unwrap();
        st2 = wait_terminal(&h.db, &job2).await;
    }
    assert_eq!(st2.state, JobState::Done, "{:?} / {:?}", st2.result, st2.plan.as_ref().map(|p| &p.blockers));
    let p2 = st2.plan.unwrap();
    assert_eq!(p2.resume_of.as_deref(), Some(st.run_id.as_str()));
    let cust = p2.items.iter().find(|i| i.kind == CopyObjectKind::Table && i.name == "customers").unwrap();
    assert_eq!(cust.action, PlanAction::ResumeDone, "the finished table is not copied again");
    assert_eq!(query(&h.tgt, "SELECT count(*), sum(amount) FROM orders"), query(&h.src, "SELECT count(*), sum(amount) FROM orders"));
    assert_eq!(st2.run_id, st.run_id, "a resume is a new job in the same run");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn verification_mismatch_is_reported() {
    let h = harness(100).await;
    rusqlite::Connection::open(&h.tgt)
        .unwrap()
        .execute_batch(
            "CREATE TABLE orders (id INTEGER PRIMARY KEY, customer_id INTEGER, amount INTEGER, memo TEXT);
             CREATE TRIGGER drop_even AFTER INSERT ON orders BEGIN DELETE FROM orders WHERE id = NEW.id AND NEW.id % 2 = 0; END;",
        )
        .unwrap();
    let mut req = request(&h);
    req.options.existing = ExistingTablePolicy::Append;
    let (_, st) = run(&h.db, &req).await;
    assert_eq!(st.state, JobState::Failed);
    let r = st.result.unwrap();
    let t = table_result(&r, "orders");
    assert_eq!(t.verify, VerifyOutcome::Mismatch);
    assert_eq!(t.state, CopyItemState::Failed);
    assert!(t.problems.iter().any(|p| p == "VERIFY_MISMATCH"), "{:?}", t.problems);
    assert_eq!(t.target_count, Some(50));
    assert!(!r.verified);
    assert_eq!(table_result(&r, "customers").verify, VerifyOutcome::Match, "other tables still finish");
}

/// An append into a table that already holds rows: the checksum compares the rows this copy
/// added (target sum after − before), not the whole table.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn append_checksum_compares_only_the_added_rows() {
    let h = harness(50).await;
    rusqlite::Connection::open(&h.tgt)
        .unwrap()
        .execute_batch(
            "CREATE TABLE orders (id INTEGER PRIMARY KEY, customer_id INTEGER, amount INTEGER, memo TEXT);
             INSERT INTO orders VALUES (9001, 1, 5, 'already here'), (9002, 2, 6, NULL);",
        )
        .unwrap();
    let mut req = request(&h);
    req.options.existing = ExistingTablePolicy::Append;
    req.options.verify = VerifyMode::Checksum;
    let (_, st) = run(&h.db, &req).await;
    assert_eq!(st.state, JobState::Done, "{:?}", st.result);
    let r = st.result.unwrap();
    let t = table_result(&r, "orders");
    assert_eq!(t.verify, VerifyOutcome::Match, "{t:?}");
    assert_eq!(t.checksum_source, t.checksum_target);
    assert!(t.checksum_target.as_deref().is_some_and(|c| c.starts_with("50:")), "{t:?}");
    assert_eq!(t.target_count, Some(50));
    assert_eq!(query(&h.tgt, "SELECT count(*) FROM orders"), vec![vec!["52".to_string()]]);
    assert!(r.verified, "{r:?}");
}

/// Cancelled while the first of several drop_recreate tables loads: pre-data already dropped
/// and re-created every table, so each one that never finished is listed in `left_behind`
/// (created and emptied), not only the one that was loading.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancel_lists_every_recreated_table_left_behind() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("source.db");
    let tgt = dir.path().join("target.db");
    rusqlite::Connection::open(&src)
        .unwrap()
        .execute_batch(
            "CREATE TABLE a_big (id INTEGER PRIMARY KEY, memo TEXT);
             CREATE TABLE b_small (id INTEGER PRIMARY KEY, memo TEXT);
             CREATE TABLE c_small (id INTEGER PRIMARY KEY, memo TEXT);
             WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM n WHERE x < 200000) INSERT INTO a_big SELECT x, 'row ' || x FROM n;
             INSERT INTO b_small VALUES (1, 'b'); INSERT INTO c_small VALUES (1, 'c');",
        )
        .unwrap();
    rusqlite::Connection::open(&tgt)
        .unwrap()
        .execute_batch(
            "CREATE TABLE a_big (id INTEGER PRIMARY KEY, memo TEXT); INSERT INTO a_big VALUES (1, 'old');
             CREATE TABLE b_small (id INTEGER PRIMARY KEY, memo TEXT); INSERT INTO b_small VALUES (1, 'old');
             CREATE TABLE c_small (id INTEGER PRIMARY KEY, memo TEXT); INSERT INTO c_small VALUES (1, 'old');",
        )
        .unwrap();
    let db = core(&dir.path().join("config"), None).await;
    let s = db.conn_save(ConnectionInput { connection: sqlite_conn("src", &src), ..Default::default() }).unwrap();
    let t = save_writable(&db, sqlite_conn("tgt", &tgt));
    let mut req = CopyRequest {
        source: CopyEndpoint { conn_id: s.id, database: "main".into(), schema: None, password: None },
        target: CopyEndpoint { conn_id: t.id, database: "main".into(), schema: None, password: None },
        selection: CopySelection::All,
        options: CopyOptions::default(),
        name: None,
    };
    req.options.existing = ExistingTablePolicy::DropRecreate;
    req.options.batch_rows = Some(1000);
    let plan = db.copy_plan(&req, None).await.unwrap();
    assert!(plan.blockers.is_empty(), "{:?}", plan.blockers);
    let order = |name: &str| plan.items.iter().find(|i| i.kind == CopyObjectKind::Table && i.name == name).map(|i| (i.phase, i.order)).unwrap();
    let job = db.copy_start(req.clone(), plan.plan_id.clone(), keys(&plan)).await.unwrap();
    // Cancel once the first table to load has rows written.
    let first = ["a_big", "b_small", "c_small"].into_iter().min_by_key(|n| order(n)).unwrap();
    let t0 = Instant::now();
    loop {
        let s = db.copy_status(&job).unwrap();
        if s.progress.tables.iter().any(|t| t.item_id.contains(first) && t.rows_written > 0) {
            break;
        }
        assert!(!s.state.is_terminal(), "finished before it could be interrupted: {:?}", s.result);
        assert!(t0.elapsed() < Duration::from_secs(60));
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    db.job_cancel(&job).unwrap();
    let st = wait_terminal(&db, &job).await;
    assert_eq!(st.state, JobState::Cancelled, "{:?}", st.result);
    let r = st.result.expect("a cancelled run stores its result");
    let unfinished: Vec<&CopyTableResult> = r.tables.iter().filter(|t| t.state != CopyItemState::Done).collect();
    assert!(unfinished.len() >= 2, "the cancel left later tables unloaded: {:?}", r.tables);
    for t in &unfinished {
        for kind in [LeftBehindKind::CreatedTable, LeftBehindKind::EmptiedTable] {
            assert!(r.left_behind.iter().any(|l| l.item_id == t.item_id && l.kind == kind), "{} {kind:?} missing: {:?}", t.item_id, r.left_behind);
        }
    }
    // Finished tables leave nothing behind.
    for t in r.tables.iter().filter(|t| t.state == CopyItemState::Done) {
        assert!(!r.left_behind.iter().any(|l| l.item_id == t.item_id), "{:?}", r.left_behind);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dry_run_writes_nothing() {
    let h = harness(10).await;
    // Missing target file: nothing is created.
    let mut req = request(&h);
    req.options.dry_run = true;
    let (_, st) = run(&h.db, &req).await;
    assert_eq!(st.state, JobState::Done, "{:?}", st.result);
    let r = st.result.unwrap();
    assert!(r.dry_run && !r.resumable);
    assert!(r.tables.iter().all(|t| t.state == CopyItemState::Planned), "{:?}", r.tables);
    assert!(!h.tgt.exists(), "a dry run creates no target file");
    // Existing target: byte-identical afterwards.
    rusqlite::Connection::open(&h.tgt).unwrap().execute_batch("CREATE TABLE other (x INTEGER); INSERT INTO other VALUES (1);").unwrap();
    let before = hash_file(&h.tgt);
    let (_, st) = run(&h.db, &req).await;
    assert_eq!(st.state, JobState::Done);
    assert_eq!(hash_file(&h.tgt), before);
    let runs = h.db.copy_runs(None, 10).unwrap();
    assert!(runs.iter().all(|r| r.dry_run && !r.resumable));
    assert!(h.db.copy_resume(CopyResumeRequest { run_id: runs[0].run_id.clone(), ..Default::default() }).await.is_err(), "dry runs are never resumable");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn refusals() {
    let h = harness(10).await;
    // Not write-allowed.
    let plain = h.db.conn_save(ConnectionInput { connection: sqlite_conn("plain", &h.dir.path().join("plain.db")), ..Default::default() }).unwrap();
    let mut req = request(&h);
    req.target.conn_id = plain.id.clone();
    let plan = h.db.copy_plan(&req, None).await.unwrap();
    assert!(plan.blockers.iter().any(|b| b.code == "TARGET_NOT_WRITABLE"), "{:?}", plan.blockers);
    let job = h.db.copy_start(req, plan.plan_id.clone(), keys(&plan)).await.unwrap();
    let st = wait_terminal(&h.db, &job).await;
    assert_eq!(st.state, JobState::Rejected);
    assert!(!h.dir.path().join("plain.db").exists());
    // Source == target (same file through another connection, and the same connection).
    let same = save_writable(&h.db, sqlite_conn("same", &h.src));
    for tgt in [same.id.clone(), h.src_id.clone()] {
        let mut req = request(&h);
        req.target.conn_id = tgt;
        let plan = h.db.copy_plan(&req, None).await.unwrap();
        assert!(plan.blockers.iter().any(|b| b.code == "SAME_DATABASE"), "{:?}", plan.blockers);
    }
    // Production: the name must be typed at start; --yes never replaces it.
    let mut prod = h.db.conn_get(&h.tgt_id).unwrap();
    prod.tag = Some(ConnTag::Production);
    h.db.conn_save(ConnectionInput { connection: prod, ..Default::default() }).unwrap();
    let req = request(&h);
    let plan = h.db.copy_plan(&req, None).await.unwrap();
    assert!(plan.warnings.iter().any(|w| w.code == "PRODUCTION_CONFIRM" && w.severity == Severity::Info));
    assert!(plan.blockers.is_empty());
    assert!(!plan.acks.iter().any(|a| a.code == "PRODUCTION_CONFIRM"));
    let job = h.db.copy_start(req.clone(), plan.plan_id.clone(), keys(&plan)).await.unwrap();
    assert_eq!(wait_terminal(&h.db, &job).await.state, JobState::Rejected, "no name typed");
    let mut ok = req.clone();
    ok.options.confirm_production = Some("tgt".into());
    let job = h.db.copy_start(ok, plan.plan_id.clone(), keys(&plan)).await.unwrap();
    let st = wait_terminal(&h.db, &job).await;
    assert_eq!(st.state, JobState::Done, "{:?}", st.result);
    assert_eq!(st.plan.unwrap().plan_id, plan.plan_id, "start-time fields never change the plan id");
    // Different families.
    let pg =
        h.db.conn_save(ConnectionInput {
            connection: Connection { name: "pg".into(), driver: "postgres".into(), host: Some("127.0.0.1".into()), port: Some(1), ..Default::default() },
            ..Default::default()
        })
        .unwrap();
    let mut req = request(&h);
    req.target.conn_id = pg.id;
    let plan = h.db.copy_plan(&req, None).await.unwrap();
    assert!(plan.blockers.iter().any(|b| b.code == "FAMILY_MISMATCH"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn target_in_use_across_two_cores() {
    let h = harness(300_000).await;
    let other = core(&h.dir.path().join("config"), None).await;
    let mut req = request(&h);
    req.options.batch_rows = Some(1000);
    let plan = h.db.copy_plan(&req, None).await.unwrap();
    let job = h.db.copy_start(req.clone(), plan.plan_id.clone(), keys(&plan)).await.unwrap();
    wait_state(&h.db, &job, JobState::Running).await;
    let p2 = other.copy_plan(&req, None).await.unwrap();
    let job2 = other.copy_start(req.clone(), p2.plan_id.clone(), keys(&p2)).await.unwrap();
    let st2 = wait_terminal(&other, &job2).await;
    assert_eq!(st2.state, JobState::Rejected);
    assert!(st2.plan.unwrap().blockers.iter().any(|b| b.code == "TARGET_IN_USE"));
    h.db.job_cancel(&job).ok();
    wait_terminal(&h.db, &job).await;
}

/// RSS is per process and the other tests of this binary run in parallel threads, so the
/// measurement runs in a child process of this test binary (`BULKDB_COPY_RSS_CHILD=1`).
#[test]
fn memory_stays_bounded_for_a_large_table() {
    if std::env::var_os("BULKDB_COPY_RSS_CHILD").is_some() {
        tokio::runtime::Builder::new_multi_thread().worker_threads(4).enable_all().build().unwrap().block_on(rss_child());
        return;
    }
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "memory_stays_bounded_for_a_large_table", "--nocapture", "--test-threads", "1"])
        .env("BULKDB_COPY_RSS_CHILD", "1")
        .output()
        .expect("spawn the test binary");
    assert!(out.status.success(), "child failed:\n{}\n{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(String::from_utf8_lossy(&out.stdout).contains("1 passed"), "the child ran the test");
}

async fn rss_child() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("wide.db");
    {
        let c = rusqlite::Connection::open(&src).unwrap();
        c.execute_batch(
            "CREATE TABLE wide (id INTEGER PRIMARY KEY, a TEXT, b TEXT, c TEXT, d BLOB);
             WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM n WHERE x < 200000)
             INSERT INTO wide SELECT x, printf('%.200c', 'a'), printf('%.200c', 'b'), '가나다라마바사 ' || x, randomblob(200) FROM n;",
        )
        .unwrap();
    }
    let db = core(&dir.path().join("config"), None).await;
    let s = db.conn_save(ConnectionInput { connection: sqlite_conn("wide", &src), ..Default::default() }).unwrap();
    let t = save_writable(&db, sqlite_conn("out", &dir.path().join("out.db")));
    let mut req = CopyRequest {
        source: CopyEndpoint { conn_id: s.id, database: "main".into(), schema: None, password: None },
        target: CopyEndpoint { conn_id: t.id, database: "main".into(), schema: None, password: None },
        selection: CopySelection::All,
        options: CopyOptions::default(),
        name: Some("wide".into()),
    };
    req.options.table_concurrency = 4;
    let plan = db.copy_plan(&req, None).await.unwrap();
    assert_eq!(plan.effective.table_concurrency, 1, "SQLite targets copy one table at a time");
    let job = db.copy_start(req, plan.plan_id.clone(), keys(&plan)).await.unwrap();
    let base = memory_stats::memory_stats().map(|m| m.physical_mem as u64).unwrap_or(0);
    let mut peak = base;
    let t0 = Instant::now();
    loop {
        if let Some(m) = memory_stats::memory_stats() {
            peak = peak.max(m.physical_mem as u64);
        }
        let st = db.copy_status(&job).unwrap();
        if st.state.is_terminal() {
            assert_eq!(st.state, JobState::Done, "{:?}", st.result);
            assert_eq!(st.result.unwrap().tables[0].rows_written, 200_000);
            break;
        }
        assert!(t0.elapsed() < DONE_TIMEOUT);
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(peak < 100 << 20, "peak RSS {} MB (start {} MB)", peak >> 20, base >> 20);
}

// ---------------------------------------------------------------------------------------------
// Fake MySQL 4.0 EUC-KR -> RecordingTarget
// ---------------------------------------------------------------------------------------------

mod fake40 {
    use super::support::fake_mysql::{types, Account, FakeColumn, FakeServer, Persona, Reply, RowSource, Script};
    use super::*;
    use bulk_core::copy::testkit::RecordingTarget;

    fn cp949(s: &str) -> Vec<u8> {
        encoding_rs::EUC_KR.encode(s).0.into_owned()
    }

    fn text_col(name: &str) -> FakeColumn {
        FakeColumn::new(name, types::VAR_STRING)
    }

    /// `SHOW CREATE TABLE` of the 4.0 table (CP949 bytes on the wire).
    fn create_table() -> String {
        "CREATE TABLE `items` (\n  `id` int(11) NOT NULL auto_increment,\n  `name` varchar(20) default NULL,\n  `kind` enum('가','나') default '가',\n  `created` timestamp(14) NOT NULL,\n  `stamp8` timestamp(8) NOT NULL,\n  `stamp6` timestamp(6) NOT NULL,\n  `stamp12` timestamp(12) NOT NULL,\n  `d` date default NULL,\n  `price` decimal(8,2) default NULL,\n  `ratio` float default NULL,\n  `data` blob,\n  PRIMARY KEY  (`id`)\n) TYPE=MyISAM".to_string()
    }

    pub async fn server(charset_name: &'static str, charset_id: u8) -> FakeServer {
        let status_cols = vec![text_col("Name"), text_col("Type"), text_col("Rows"), text_col("Data_length"), text_col("Index_length"), text_col("Auto_increment")];
        let status_row: Vec<Option<Vec<u8>>> =
            vec![Some(b"items".to_vec()), Some(b"MyISAM".to_vec()), Some(b"4".to_vec()), Some(b"400".to_vec()), Some(b"100".to_vec()), Some(b"5".to_vec())];
        let data_cols = vec![
            FakeColumn::new("id", types::LONG),
            text_col("name"),
            text_col("kind"),
            FakeColumn::new("created", types::LONGLONG),
            FakeColumn::new("stamp8", types::LONGLONG),
            FakeColumn::new("stamp6", types::LONGLONG),
            FakeColumn::new("stamp12", types::LONGLONG),
            text_col("d"),
            FakeColumn::new("price", types::DECIMAL).decimals(2).length(10),
            FakeColumn::new("ratio", types::DOUBLE),
            FakeColumn::new("data", types::BLOB).flags(super::support::fake_mysql::flags::BINARY | super::support::fake_mysql::flags::BLOB),
        ];
        let row = |id: &str,
                   name: &str,
                   kind: &str,
                   created: &str,
                   s8: &str,
                   s6: &str,
                   s12: &str,
                   d: Option<&str>,
                   price: &str,
                   ratio: &str,
                   data: Option<&[u8]>|
         -> Vec<Option<Vec<u8>>> {
            vec![
                Some(id.as_bytes().to_vec()),
                Some(cp949(name)),
                Some(cp949(kind)),
                Some(created.as_bytes().to_vec()),
                Some(s8.as_bytes().to_vec()),
                Some(s6.as_bytes().to_vec()),
                Some(s12.as_bytes().to_vec()),
                d.map(|v| v.as_bytes().to_vec()),
                Some(price.as_bytes().to_vec()),
                Some(ratio.as_bytes().to_vec()),
                data.map(|b| b.to_vec()),
            ]
        };
        let rows = vec![
            row("1", "홍길동", "가", "1086085845", "1086048000", "1086048000", "1086085800", Some("2004-06-01"), "123.45", "0.5", Some(&[0x00, 0xff, 0x10])),
            row("2", "똠방각하", "나", "0", "0", "0", "0", Some("0000-00-00"), "-5.00", "1.25", None),
            row("3", "", "가", "1086085845", "1086048000", "1086048000", "1086085800", Some("2004-02-31"), "0.00", "0", Some(&[])),
            row("4", "김", "나", "1086085845", "1086048000", "1086048000", "1086085800", Some("2004-04-31"), "1.00", "2", Some(b"x")),
        ];
        let script = Script::new()
            .on_contains(b"SHOW TABLE STATUS", Reply::Rows { columns: status_cols.clone(), rows: RowSource::Fixed(vec![status_row.clone()]) })
            .on_contains(
                b"SHOW CREATE TABLE",
                Reply::Rows {
                    columns: vec![text_col("Table"), text_col("Create Table")],
                    rows: RowSource::Fixed(vec![vec![Some(b"items".to_vec()), Some(cp949(&create_table()))]]),
                },
            )
            .on_contains(
                b"LIMIT 1000",
                Reply::Rows { columns: vec![text_col("name")], rows: RowSource::Fixed(vec![vec![Some(cp949("홍길동"))], vec![Some(cp949("김"))]]) },
            )
            .on_contains(b"FROM `shop`.`items`", Reply::Rows { columns: data_cols, rows: RowSource::Fixed(rows) });
        FakeServer::start(Persona::Mysql40 { version: "4.0.27-standard", charset_name, charset_id }, vec![Account { user: b"app".to_vec(), old_hash: None }], script)
            .await
    }

    pub fn facts(local_infile: bool) -> TargetFacts {
        let mut f = TargetFacts::new(CopyFamily::Mysql, "mysql", "8.0.36", "shop");
        f.endpoint = "target:3306".into();
        for (k, v) in [
            ("local_infile", if local_infile { "ON" } else { "OFF" }),
            ("max_allowed_packet", "67108864"),
            ("sql_mode", "ONLY_FULL_GROUP_BY,STRICT_TRANS_TABLES,NO_ZERO_IN_DATE,NO_ZERO_DATE,ERROR_FOR_DIVISION_BY_ZERO,NO_ENGINE_SUBSTITUTION"),
            ("default_storage_engine", "InnoDB"),
            ("character_set_server", "utf8mb4"),
            ("collation_server", "utf8mb4_0900_ai_ci"),
            ("innodb_file_per_table", "ON"),
            ("innodb_default_row_format", "dynamic"),
            ("lower_case_table_names", "0"),
            ("server_uuid", "11111111-2222-3333-4444-555555555555"),
        ] {
            f.vars.insert(k.into(), v.into());
        }
        f
    }

    pub fn target(local_infile: bool) -> RecordingTarget {
        RecordingTarget::new(facts(local_infile)).answer(
            "SHOW CHARACTER SET",
            MetaTable::from_rows(
                &["Charset", "Default collation", "Maxlen"],
                &[&[Some("utf8mb4"), Some("utf8mb4_0900_ai_ci"), Some("4")], &[Some("utf8mb3"), Some("utf8mb3_general_ci"), Some("3")]],
            ),
        )
    }
}

async fn fake_case(local_infile: bool) -> (bulk_core::copy::testkit::RecordingTarget, CopyStatus, CopyPlan) {
    let rec = fake40::target(local_infile);
    let (st, plan) = fake_run(Arc::new(rec.clone())).await;
    (rec, st, plan)
}

/// The fake 4.0 EUC-KR source copied (all tables, zero dates `null`) through `opener`.
async fn fake_run(opener: Arc<dyn TargetOpener>) -> (CopyStatus, CopyPlan) {
    let (_db, _dir, st, plan) = fake_run_keep(opener).await;
    (st, plan)
}

/// As [`fake_run`], but returns the live `BulkDb` (and its config dir) so a test can inspect
/// `copy_runs` / resume after the job finished.
async fn fake_run_keep(opener: Arc<dyn TargetOpener>) -> (Arc<BulkDb>, tempfile::TempDir, CopyStatus, CopyPlan) {
    let server = fake40::server("euc_kr", 19).await;
    let dir = tempfile::tempdir().unwrap();
    let db = core(&dir.path().join("config"), Some(opener)).await;
    let src = db
        .conn_save(ConnectionInput {
            connection: Connection {
                name: "legacy".into(),
                driver: "mysql".into(),
                host: Some("127.0.0.1".into()),
                port: Some(server.port()),
                database: Some("shop".into()),
                user: Some("app".into()),
                ..Default::default()
            },
            password: None,
            ..Default::default()
        })
        .unwrap();
    let tgt = save_writable(
        &db,
        Connection { name: "new".into(), driver: "mysql".into(), host: Some("10.0.0.9".into()), port: Some(3306), user: Some("copy".into()), ..Default::default() },
    );
    let mut req = CopyRequest {
        source: CopyEndpoint { conn_id: src.id.clone(), database: "shop".into(), schema: None, password: None },
        target: CopyEndpoint { conn_id: tgt.id.clone(), database: "shop".into(), schema: None, password: None },
        selection: CopySelection::All,
        options: CopyOptions::default(),
        name: None,
    };
    req.options.mysql.zero_dates = ZeroDatePolicy::Null;
    let plan = db.copy_plan(&req, None).await.unwrap();
    assert!(plan.blockers.is_empty(), "blockers: {:?}", plan.blockers);
    let job = db.copy_start(req, plan.plan_id.clone(), keys(&plan)).await.unwrap();
    let st = wait_terminal(&db, &job).await;
    drop(server);
    (db, dir, st, plan)
}

/// A MySQL writer that commits one chunk (1 row), then fails the next one: delegates every
/// other call to a `RecordingTarget` session.
mod partial {
    use super::*;
    use bulk_core::copy::testkit::RecordingTarget;
    use bulk_core::error::{CoreError, Result};
    use bulk_core::CancellationToken;
    use std::sync::Mutex;

    pub struct Opener(pub RecordingTarget);

    struct Session {
        inner: Box<dyn TargetSession>,
        failed: Mutex<Option<LoadOutcome>>,
    }

    #[async_trait::async_trait]
    impl TargetOpener for Opener {
        async fn open(&self, conn: &Connection, ep: &CopyEndpoint, app_name: &str) -> Result<Box<dyn TargetSession>> {
            Ok(Box::new(Session { inner: self.0.open(conn, ep, app_name).await?, failed: Mutex::new(None) }))
        }
    }

    #[async_trait::async_trait]
    impl TargetSession for Session {
        fn facts(&self) -> &TargetFacts {
            self.inner.facts()
        }
        async fn query_meta(&self, sql: &str) -> Result<MetaTable> {
            self.inner.query_meta(sql).await
        }
        async fn execute(&self, sql: &str) -> Result<u64> {
            self.inner.execute(sql).await
        }
        async fn load(&self, _spec: &TableDataSpec, input: LoadInput, _cancel: CancellationToken) -> Result<LoadOutcome> {
            let LoadInput::Arrow(mut rx) = input else { return Err(CoreError::Invalid("arrow input expected".into())) };
            let mut sent = 0;
            while let Some(b) = rx.recv().await {
                sent += b.rows;
            }
            assert!(sent > 1, "the reader sent every row");
            *self.failed.lock().unwrap() = Some(LoadOutcome {
                rows: 1,
                warnings: 1,
                samples: vec!["Data truncated for column 'name' at row 1: 'secret value'".into()],
                method: Some(LoadMethod::MysqlInsert),
                ..LoadOutcome::default()
            });
            Err(CoreError::Query { code: Some("1406".into()), msg: format!("{sent} rows sent; the second chunk failed") })
        }
        async fn count_rows(&self, t: &TargetTable) -> Result<u64> {
            self.inner.count_rows(t).await
        }
        async fn cancel_running(&self) {
            self.inner.cancel_running().await
        }
        fn configure(&self, cfg: &WriterConfig) {
            self.inner.configure(cfg)
        }
        fn take_failed_outcome(&self) -> Option<LoadOutcome> {
            self.failed.lock().unwrap().take()
        }
    }
}

/// A target opener that tracks which write sessions (`-t0` DDL, `-t1`.. workers) are open at
/// once, to prove the DDL session is closed for the data phase (peak target sessions = 2 + c,
/// not 3 + c). Delegates every call to a `RecordingTarget`.
mod session_count {
    use super::*;
    use bulk_core::copy::testkit::RecordingTarget;
    use bulk_core::error::Result;
    use bulk_core::CancellationToken;
    use std::collections::HashSet;
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct State {
        open: HashSet<String>,
        ddl_overlapped_worker: bool,
    }

    #[derive(Clone)]
    pub struct Opener {
        inner: RecordingTarget,
        state: Arc<Mutex<State>>,
    }

    impl Opener {
        pub fn new(inner: RecordingTarget) -> Opener {
            Opener { inner, state: Arc::new(Mutex::new(State::default())) }
        }
        /// Whether the DDL session (`t0`) was ever open while a data worker (`t1`..) was.
        pub fn ddl_overlapped_worker(&self) -> bool {
            self.state.lock().unwrap().ddl_overlapped_worker
        }
    }

    struct Sess {
        inner: Box<dyn TargetSession>,
        suffix: String,
        state: Arc<Mutex<State>>,
    }

    impl Drop for Sess {
        fn drop(&mut self) {
            self.state.lock().unwrap().open.remove(&self.suffix);
        }
    }

    #[async_trait::async_trait]
    impl TargetOpener for Opener {
        async fn open(&self, conn: &Connection, ep: &CopyEndpoint, app_name: &str) -> Result<Box<dyn TargetSession>> {
            let inner = self.inner.open(conn, ep, app_name).await?;
            let suffix = app_name.rsplit('-').next().unwrap_or("").to_string();
            {
                let mut s = self.state.lock().unwrap();
                s.open.insert(suffix.clone());
                let ddl = s.open.contains("t0");
                let worker = s.open.iter().any(|x| x.starts_with('t') && x != "t0");
                if ddl && worker {
                    s.ddl_overlapped_worker = true;
                }
            }
            Ok(Box::new(Sess { inner, suffix, state: self.state.clone() }))
        }
    }

    #[async_trait::async_trait]
    impl TargetSession for Sess {
        fn facts(&self) -> &TargetFacts {
            self.inner.facts()
        }
        async fn query_meta(&self, sql: &str) -> Result<MetaTable> {
            self.inner.query_meta(sql).await
        }
        async fn execute(&self, sql: &str) -> Result<u64> {
            self.inner.execute(sql).await
        }
        async fn load(&self, spec: &TableDataSpec, input: LoadInput, cancel: CancellationToken) -> Result<LoadOutcome> {
            self.inner.load(spec, input, cancel).await
        }
        async fn count_rows(&self, t: &TargetTable) -> Result<u64> {
            self.inner.count_rows(t).await
        }
        async fn cancel_running(&self) {
            self.inner.cancel_running().await
        }
        fn configure(&self, cfg: &WriterConfig) {
            self.inner.configure(cfg)
        }
    }
}

/// The DDL write session (`t0`) is closed for the data phase, so it is never open at the same
/// time as a data worker (`t1`..): the executor's peak target sessions match the plan's 2 + c,
/// not 3 + c. (Finding: the executor held `ddl` open through the data phase.)
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ddl_session_is_closed_during_the_data_phase() {
    let opener = session_count::Opener::new(fake40::target(false));
    let (_db, _dir, st, _plan) = fake_run_keep(Arc::new(opener.clone())).await;
    assert_eq!(st.state, JobState::Done, "{:?}", st.result);
    assert!(!opener.ddl_overlapped_worker(), "the DDL session (t0) must be closed while data workers (t1..) run");
}

/// A run that finished `done` but with problems (invalid dates were fixed) is resumable in
/// `copy_runs`, just as the result view and the CLI hint offer (5.1). A clean `done` run is not.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn done_with_problems_run_is_resumable() {
    let (db, _dir, st, _plan) = fake_run_keep(Arc::new(fake40::target(false))).await;
    assert_eq!(st.state, JobState::Done, "done with problems: {:?}", st.result);
    assert!(st.result.as_ref().unwrap().problems > 0, "the run finished with problems: {:?}", st.result);
    let runs = db.copy_runs(None, 10).unwrap();
    assert_eq!(runs[0].state, JobState::Done);
    assert!(runs[0].resumable, "a done-with-problems run is resumable in copy_runs: {:?}", runs[0]);
    // copy_resume does not refuse it (the stored result is resumable); the spawned job is left to
    // the runtime, we only assert the resumability gate accepts the run.
    assert!(db.copy_resume(CopyResumeRequest { run_id: runs[0].run_id.clone(), ..Default::default() }).await.is_ok(), "copy_resume accepts a done-with-problems run");
}

/// `left_behind.partial_rows` after a MySQL table failure reports the rows the writer
/// committed (whole chunks), not the rows the reader sent to it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failed_mysql_table_reports_committed_rows_as_partial() {
    let (st, _plan) = fake_run(Arc::new(partial::Opener(fake40::target(false)))).await;
    assert_eq!(st.state, JobState::Failed, "{:?}", st.result);
    let r = st.result.expect("result");
    let t = &r.tables[0];
    assert_eq!(t.state, CopyItemState::Failed);
    assert_eq!(t.rows_written, 1, "committed rows, not the 4 sent: {t:?}");
    assert_eq!(t.data_warnings, 1);
    assert!(t.warning_samples.iter().all(|s| !s.contains("secret value")), "samples are redacted: {:?}", t.warning_samples);
    let partial: Vec<&LeftBehind> = r.left_behind.iter().filter(|l| l.kind == LeftBehindKind::PartialRows).collect();
    assert_eq!(partial.len(), 1, "{:?}", r.left_behind);
    assert_eq!((partial[0].item_id.as_str(), partial[0].rows), (t.item_id.as_str(), Some(1)));
    assert!(r.left_behind.iter().any(|l| l.kind == LeftBehindKind::CreatedTable && l.item_id == t.item_id));
    assert!(r.resumable);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fake_mysql40_euckr_into_recording_target() {
    for local_infile in [false, true] {
        let (rec, st, plan) = fake_case(local_infile).await;
        let r = st.result.clone().unwrap_or_else(|| panic!("no result: {:?}", st));
        let statements = rec.statements();
        let ddl: Vec<&String> = statements.iter().map(|(_, s)| s).filter(|s| s.starts_with("CREATE TABLE")).collect();
        assert_eq!(ddl.len(), 1, "{statements:#?}");
        let create = ddl[0];
        for want in ["utf8mb4", "utf8mb4_0900_ai_ci", "ENGINE=InnoDB", "decimal(9,2)", "enum('가','나')"] {
            assert!(create.contains(want), "{want} missing in {create}");
        }
        assert!(create.contains("DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP"), "{create}");
        // Every write session carries the prelude.
        let sessions: std::collections::BTreeSet<u32> = statements.iter().filter(|(_, s)| s.starts_with("CREATE") || s.starts_with("ALTER")).map(|(n, _)| *n).collect();
        let loads: std::collections::BTreeSet<u32> = statements.iter().map(|(n, _)| *n).collect();
        for n in sessions.iter().chain(loads.iter()) {
            let mine: Vec<&String> = statements.iter().filter(|(m, _)| m == n).map(|(_, s)| s).collect();
            assert!(mine.iter().any(|s| *s == "SET NAMES utf8mb4"), "session {n}: {mine:?}");
            assert!(mine.iter().any(|s| s.starts_with("SET SESSION sql_mode = '") && s.contains("STRICT_ALL_TABLES")), "session {n}");
            assert!(mine.iter().any(|s| *s == "SET SESSION time_zone = '+00:00'"), "session {n}");
        }
        let payload: Vec<u8> = rec.payloads().iter().flat_map(|(_, b)| b.to_vec()).collect();
        let text = String::from_utf8(payload).expect("the payload is UTF-8");
        assert!(text.contains("홍길동") && text.contains("똠방각하"), "{text}");
        let t = &r.tables[0];
        assert_eq!(t.rows_written, 4, "{t:?}");
        assert_eq!(t.method, Some(if local_infile { LoadMethod::MysqlLoadData } else { LoadMethod::MysqlInsert }));
        if local_infile {
            assert!(text.contains("2004-06-01 10:30:45"), "the first TIMESTAMP as UTC text: {text}");
            assert!(text.contains("1970-01-01 00:00:01"), "zero first TIMESTAMP -> minimum: {text}");
            assert!(text.contains("00FF10"), "hex blob: {text}");
            assert!(text.contains("\\N"), "NULLs: {text}");
        } else {
            assert!(text.contains("X'00FF10'"), "{text}");
            assert!(text.contains("'2004-06-01 10:30:45'"), "{text}");
        }
        let fixes: BTreeMap<String, u64> = t.value_fixes.iter().map(|f| (format!("{}:{}", f.column, f.code), f.count)).collect();
        assert_eq!(fixes.get("d:INVALID_DATE"), Some(&2), "{fixes:?}");
        assert_eq!(fixes.get("d:ZERO_DATE_NULL"), Some(&1), "{fixes:?}");
        assert_eq!(fixes.get("created:ZERO_DATE_MIN"), Some(&1), "{fixes:?}");
        assert!(t.problems.iter().any(|p| p == "INVALID_DATE"), "{:?}", t.problems);
        assert_eq!(st.state, JobState::Done, "done with problems: {:?}", st.result);
        assert!(r.problems > 0 && !r.verified);
        assert!(plan.source.version.starts_with("4.0"), "{:?}", plan.source);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fake_mysql40_latin1_holding_cp949_is_blocked() {
    let server = fake40::server("latin1", 8).await;
    let dir = tempfile::tempdir().unwrap();
    let rec = fake40::target(true);
    let db = core(&dir.path().join("config"), Some(Arc::new(rec.clone()))).await;
    let src = db
        .conn_save(ConnectionInput {
            connection: Connection {
                name: "legacy".into(),
                driver: "mysql".into(),
                host: Some("127.0.0.1".into()),
                port: Some(server.port()),
                database: Some("shop".into()),
                user: Some("app".into()),
                ..Default::default()
            },
            password: None,
            ..Default::default()
        })
        .unwrap();
    let tgt = save_writable(&db, Connection { name: "new".into(), driver: "mysql".into(), host: Some("10.0.0.9".into()), port: Some(3306), ..Default::default() });
    let req = CopyRequest {
        source: CopyEndpoint { conn_id: src.id, database: "shop".into(), schema: None, password: None },
        target: CopyEndpoint { conn_id: tgt.id, database: "shop".into(), schema: None, password: None },
        selection: CopySelection::All,
        options: CopyOptions::default(),
        name: None,
    };
    let plan = db.copy_plan(&req, None).await.unwrap();
    assert!(plan.blockers.iter().any(|b| b.code == "SRC_LATIN1_KOREAN"), "{:?}", plan.blockers);
    assert!(rec.statements().is_empty(), "planning writes nothing");
}

/// The real `MysqlTarget` (mysql_async writer) on its INSERT path against the fake 5.5 server:
/// facts, prelude, session assertion, chunk transaction, literals, count.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mysql_target_insert_path_against_a_fake_55_server() {
    use arrow_array::{ArrayRef, BinaryArray, Int64Array, RecordBatch, StringArray};
    use bulk_core::copy::data::mysql_encode;
    use support::fake_mysql::{types, Account, FakeColumn, FakeServer, Persona, Reply, RowSource, Script};

    let mode = mysql_encode::target_sql_mode("STRICT_TRANS_TABLES", false);
    let text = |n: &str| FakeColumn::new(n, types::VAR_STRING);
    let one = |cols: &[&str], vals: &[&str]| Reply::Rows {
        columns: cols.iter().map(|c| text(c)).collect(),
        rows: RowSource::Fixed(vec![vals.iter().map(|v| Some(v.as_bytes().to_vec())).collect()]),
    };
    let script = Script::new()
        .on_contains(
            b"SHOW GLOBAL VARIABLES",
            Reply::Rows {
                columns: vec![text("Variable_name"), text("Value")],
                rows: RowSource::Fixed(vec![
                    vec![Some(b"local_infile".to_vec()), Some(b"OFF".to_vec())],
                    vec![Some(b"sql_mode".to_vec()), Some(b"STRICT_TRANS_TABLES".to_vec())],
                    vec![Some(b"max_allowed_packet".to_vec()), Some(b"16777216".to_vec())],
                ]),
            },
        )
        .on_contains(b"@@session.time_zone", one(&["tz", "mode", "cs"], &["+00:00", &mode, "utf8mb4"]))
        .on_contains(b"information_schema.TABLES", one(&["ENGINE"], &["InnoDB"]))
        .on_contains(b"SELECT COUNT(*) AS n FROM `shop`.`t`", one(&["n"], &["2"]))
        .on_contains(b"INSERT INTO `shop`.`t`", Reply::Ok { affected_rows: 2 })
        .on_exact(b"START TRANSACTION", Reply::Ok { affected_rows: 0 })
        .on_exact(b"COMMIT", Reply::Ok { affected_rows: 0 })
        .on_exact(b"ROLLBACK", Reply::Ok { affected_rows: 0 });
    let server =
        FakeServer::start(Persona::Mysql55PluginOldAuth { version: "5.5.62", charset_name: "utf8" }, vec![Account { user: b"nopass".to_vec(), old_hash: None }], script)
            .await;
    let dir = tempfile::tempdir().unwrap();
    let db = core(&dir.path().join("config"), None).await;
    let mut fields = BTreeMap::new();
    fields.insert("protocol".to_string(), "modern".to_string());
    fields.insert("allow_old_password".to_string(), "true".to_string());
    let conn = Connection {
        id: "t".into(),
        name: "fake55".into(),
        driver: "mysql".into(),
        host: Some("127.0.0.1".into()),
        port: Some(server.port()),
        user: Some("nopass".into()),
        fields,
        ..Default::default()
    };
    let ep = CopyEndpoint { conn_id: "t".into(), database: "shop".into(), schema: None, password: None };
    let opener = bulk_core::copy::target::DriverOpener::new(Arc::downgrade(&db));
    let s = opener.open(&conn, &ep, "bulkdb-j-t1").await.expect("open the writer session");
    assert_eq!((s.facts().major, s.facts().minor), (5, 5));
    assert_eq!(s.facts().vars.get("local_infile").map(String::as_str), Some("OFF"));
    for sql in mysql_encode::session_prelude(s.facts(), ZeroDatePolicy::Null, 5) {
        s.execute(&sql).await.unwrap();
    }
    let col = |n: &str, v: ValueKind| DataColumn {
        source: n.into(),
        target: n.into(),
        value: v,
        read: SourceRead::Plain,
        zero_date: ZeroDateAction::NotTemporal,
        invalid_date: ZeroDateAction::NotTemporal,
        binary_attr: false,
        max_bytes: None,
    };
    let spec = TableDataSpec {
        item_id: "table:\"t\"".into(),
        source_sql: String::new(),
        verify_sql: String::new(),
        raw_csv: false,
        transcode: None,
        target: TargetTable { schema: Some("shop".into()), name: "t".into() },
        columns: vec![col("id", ValueKind::Integer), col("name", ValueKind::Text), col("b", ValueKind::Binary)],
        est_rows: None,
        first_timestamp_auto: false,
        fk_checks: false,
    };
    let arrays: Vec<ArrayRef> = vec![
        Arc::new(Int64Array::from(vec![Some(1), Some(2)])),
        Arc::new(StringArray::from(vec![Some("가나"), None])),
        Arc::new(BinaryArray::from(vec![Some(&[0x00, 0xff][..]), None])),
    ];
    let fields: Vec<arrow_schema::Field> = arrays.iter().enumerate().map(|(i, a)| arrow_schema::Field::new(format!("c{i}"), a.data_type().clone(), true)).collect();
    let batch = RecordBatch::try_new(Arc::new(arrow_schema::Schema::new(fields)), arrays).unwrap();
    let budget = Arc::new(tokio::sync::Semaphore::new(16));
    let (tx, rx) = tokio::sync::mpsc::channel(1);
    let send = async move {
        let permit = budget.acquire_many_owned(1).await.unwrap();
        tx.send(Budgeted { item: batch, rows: 2, permit }).await.unwrap();
    };
    let (out, ()) = tokio::join!(s.load(&spec, LoadInput::Arrow(rx), bulk_core::CancellationToken::new()), send);
    let out = out.expect("load");
    assert_eq!((out.rows, out.method), (2, Some(LoadMethod::MysqlInsert)));
    assert!(!s.server_rollback_pending(), "a clean load commits its last chunk: nothing to roll back");
    assert_eq!(s.count_rows(&spec.target).await.unwrap(), 2);
    let queries: Vec<String> = server.queries().iter().map(|q| String::from_utf8_lossy(q).into_owned()).collect();
    let insert = queries.iter().find(|q| q.contains("INSERT INTO `shop`.`t`")).expect("the INSERT reached the server");
    assert!(insert.contains("(1,'가나',X'00FF'),(2,NULL,NULL)"), "{insert}");
    let pos = |needle: &str| queries.iter().position(|q| q.contains(needle)).unwrap_or_else(|| panic!("{needle} not sent: {queries:#?}"));
    assert!(pos("@@session.time_zone") < pos("INSERT INTO"), "the session assertion runs before the chunk");
    assert!(pos("START TRANSACTION") < pos("INSERT INTO") && pos("INSERT INTO") < pos("COMMIT"));
    drop(s);
    server.shutdown().await;
}

/// The real `MysqlTarget` on the fake 5.5 server: chunks of 2 rows; the first chunk commits, the
/// second INSERT fails and is rolled back. `take_failed_outcome` reports the 2 committed rows
/// (whole chunks), not the 4 sent; it is taken once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mysql_target_reports_committed_rows_after_a_failed_chunk() {
    use arrow_array::{ArrayRef, Int64Array, RecordBatch, StringArray};
    use bulk_core::copy::data::mysql_encode;
    use support::fake_mysql::{types, Account, FakeColumn, FakeServer, Persona, Reply, RowSource, Script};

    let mode = mysql_encode::target_sql_mode("STRICT_TRANS_TABLES", false);
    let text = |n: &str| FakeColumn::new(n, types::VAR_STRING);
    let one = |cols: &[&str], vals: &[&str]| Reply::Rows {
        columns: cols.iter().map(|c| text(c)).collect(),
        rows: RowSource::Fixed(vec![vals.iter().map(|v| Some(v.as_bytes().to_vec())).collect()]),
    };
    let script = Script::new()
        .on_contains(
            b"SHOW GLOBAL VARIABLES",
            Reply::Rows {
                columns: vec![text("Variable_name"), text("Value")],
                rows: RowSource::Fixed(vec![
                    vec![Some(b"local_infile".to_vec()), Some(b"OFF".to_vec())],
                    vec![Some(b"sql_mode".to_vec()), Some(b"STRICT_TRANS_TABLES".to_vec())],
                    vec![Some(b"max_allowed_packet".to_vec()), Some(b"16777216".to_vec())],
                ]),
            },
        )
        .on_contains(b"@@session.time_zone", one(&["tz", "mode", "cs"], &["+00:00", &mode, "utf8mb4"]))
        .on_contains(b"information_schema.TABLES", one(&["ENGINE"], &["InnoDB"]))
        // The second chunk (rows 3 and 4) violates a constraint.
        .on_contains(b"(3,", Reply::Err { code: 1406, message: b"Data too long for column 'name' at row 1".to_vec() })
        .on_contains(b"INSERT INTO `shop`.`t`", Reply::Ok { affected_rows: 2 })
        .on_exact(b"START TRANSACTION", Reply::Ok { affected_rows: 0 })
        .on_exact(b"COMMIT", Reply::Ok { affected_rows: 0 })
        .on_exact(b"ROLLBACK", Reply::Ok { affected_rows: 0 });
    let server =
        FakeServer::start(Persona::Mysql55PluginOldAuth { version: "5.5.62", charset_name: "utf8" }, vec![Account { user: b"nopass".to_vec(), old_hash: None }], script)
            .await;
    let dir = tempfile::tempdir().unwrap();
    let db = core(&dir.path().join("config"), None).await;
    let mut fields = BTreeMap::new();
    fields.insert("protocol".to_string(), "modern".to_string());
    fields.insert("allow_old_password".to_string(), "true".to_string());
    let conn = Connection {
        id: "t".into(),
        name: "fake55".into(),
        driver: "mysql".into(),
        host: Some("127.0.0.1".into()),
        port: Some(server.port()),
        user: Some("nopass".into()),
        fields,
        ..Default::default()
    };
    let ep = CopyEndpoint { conn_id: "t".into(), database: "shop".into(), schema: None, password: None };
    let opener = bulk_core::copy::target::DriverOpener::new(Arc::downgrade(&db));
    let s = opener.open(&conn, &ep, "bulkdb-j-t1").await.expect("open the writer session");
    for sql in mysql_encode::session_prelude(s.facts(), ZeroDatePolicy::Null, 5) {
        s.execute(&sql).await.unwrap();
    }
    s.configure(&WriterConfig {
        job_id: "j".into(),
        session_no: 1,
        batch_rows: 2,
        piece_bytes: 1 << 20,
        chunk_bytes: 64 << 20,
        chunk_rows: 2,
        lock_timeout_s: 5,
        fail_on_unmappable: false,
        charset: Some("utf8mb4".into()),
        table_charsets: BTreeMap::new(),
        enum_error_columns: BTreeMap::new(),
        enum_error_null: false,
        source_checksum: false,
    });
    let col = |n: &str, v: ValueKind| DataColumn {
        source: n.into(),
        target: n.into(),
        value: v,
        read: SourceRead::Plain,
        zero_date: ZeroDateAction::NotTemporal,
        invalid_date: ZeroDateAction::NotTemporal,
        binary_attr: false,
        max_bytes: None,
    };
    let spec = TableDataSpec {
        item_id: "table:\"t\"".into(),
        source_sql: String::new(),
        verify_sql: String::new(),
        raw_csv: false,
        transcode: None,
        target: TargetTable { schema: Some("shop".into()), name: "t".into() },
        columns: vec![col("id", ValueKind::Integer), col("name", ValueKind::Text)],
        est_rows: None,
        first_timestamp_auto: false,
        fk_checks: false,
    };
    let arrays: Vec<ArrayRef> = vec![Arc::new(Int64Array::from(vec![1, 2, 3, 4])), Arc::new(StringArray::from(vec!["가", "나", "다", "라"]))];
    let fields: Vec<arrow_schema::Field> = arrays.iter().enumerate().map(|(i, a)| arrow_schema::Field::new(format!("c{i}"), a.data_type().clone(), true)).collect();
    let batch = RecordBatch::try_new(Arc::new(arrow_schema::Schema::new(fields)), arrays).unwrap();
    let budget = Arc::new(tokio::sync::Semaphore::new(16));
    let (tx, rx) = tokio::sync::mpsc::channel(1);
    let send = async move {
        let permit = budget.acquire_many_owned(1).await.unwrap();
        let _ = tx.send(Budgeted { item: batch, rows: 4, permit }).await;
    };
    let (out, ()) = tokio::join!(s.load(&spec, LoadInput::Arrow(rx), bulk_core::CancellationToken::new()), send);
    assert!(out.is_err(), "the second chunk fails the table: {out:?}");
    let partial = s.take_failed_outcome().expect("a failed load reports what it committed");
    assert_eq!(partial.rows, 2, "whole committed chunks only: {partial:?}");
    assert_eq!(partial.method, Some(LoadMethod::MysqlInsert));
    assert!(s.take_failed_outcome().is_none(), "taken once");
    // The second chunk's transaction was rolled back and never committed: a rollback is pending
    // (drives SERVER_ROLLBACK_PENDING, 5.4), unlike a cancel that happens between chunks.
    assert!(s.server_rollback_pending(), "a rolled-back open chunk leaves a pending rollback");
    let queries: Vec<String> = server.queries().iter().map(|q| String::from_utf8_lossy(q).into_owned()).collect();
    let pos = |needle: &str| queries.iter().position(|q| q.contains(needle)).unwrap_or_else(|| panic!("{needle} not sent: {queries:#?}"));
    assert!(pos("(1,") < pos("COMMIT") && pos("COMMIT") < pos("(3,") && pos("(3,") < pos("ROLLBACK"), "{queries:#?}");
    drop(s);
    server.shutdown().await;
}
