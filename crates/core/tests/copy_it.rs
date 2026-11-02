//! DB copy integration tests against real servers (docs/db-copy.md section 9, owner copy-it).
//!
//! ```text
//! docker compose -f docker/compose.yml up -d --wait
//! BULKDB_PG_URL=postgres://bulkdb:bulkdb@127.0.0.1:5432/bulkdb \
//! BULKDB_PG16_URL=postgres://bulkdb:bulkdb@127.0.0.1:5433/bulkdb \
//! BULKDB_PG17_URL=postgres://bulkdb:bulkdb@127.0.0.1:5434/bulkdb \
//! BULKDB_MYSQL_URL=mysql://root:bulkdb@127.0.0.1:3306/bulkdb \
//! BULKDB_MYSQL55_URL=mysql://root:bulkdb@127.0.0.1:3307/bulkdb \
//! BULKDB_MYSQL9_URL=mysql://root:bulkdb@127.0.0.1:3308/bulkdb \
//!   cargo test -p bulk-core --features it --test copy_it
//! ```
//!
//! Pairs:
//! * `mysql:5.5` started with `--character-set-server=euckr` (EUC-KR tables, a latin1 table
//!   holding CP949 bytes, zero and invalid dates, TIMESTAMP, YEAR, DECIMAL, BLOB, BIT) into
//!   `mysql:8` and `mysql:9` (utf8mb4), with both load methods (LOAD DATA LOCAL and INSERT);
//! * `postgres:15` into `postgres:16`, `postgres:17` and a second database of the same server
//!   (identity, serial and standalone sequences, enum, domain, partitions, inheritance, views,
//!   a materialized view, functions, triggers, a deferrable FK, NOT VALID checks, comments,
//!   `citext`, RLS, a table with 200k rows);
//! * an optional real MySQL 4.0 (`BULKDB_MYSQL40_URL`; no public image exists) into `mysql:8`.
//!
//! Every test drives the public `BulkDb` facade exactly like the UI and the CLI: saved
//! connections (the target marked write-allowed by a second save), `copy_plan`, `copy_start`
//! with the plan's acknowledgement keys, `copy_status`, `job_cancel`, `copy_resume`. A test
//! whose env vars are missing prints a skip note and passes, so the file always compiles and
//! never fails a box without docker.
//!
//! **Isolation.** Every test builds its own source database (`bulkdb_it_src_<uuid>`) and copies
//! into its own target database (`bulkdb_it_<uuid>`), dropped at the end. The MySQL tests
//! change server globals (`local_infile`, `max_allowed_packet`) as test setup only (root; not
//! BulkDB behaviour), and every copy into or out of such a server holds that server's static
//! mutex below, so the copies of this binary that touch one MySQL server run one at a time.
//!
//! **Forced cancel.** Deterministic: as soon as the copy has created the biggest target table,
//! the test locks it (PG `LOCK TABLE … IN ACCESS EXCLUSIVE MODE`, MySQL `LOCK TABLES … WRITE`)
//! from its own session, so the writer blocks on its next statement on that table (load,
//! index, count). `job_cancel` must then reach `cancelled` within 3 s, and `copy_resume`
//! finishes the run in a new job of the same run.
#![cfg(all(feature = "it", feature = "postgres", feature = "mysql"))]

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bulk_core::credentials::MemoryStore;
use bulk_core::service::{BulkDb, BulkDbOptions};
use bulk_core::source::mysql_legacy::conn::LegacyConn;
use bulk_core::source::mysql_legacy::handshake::peek_greeting;
use bulk_core::source::{ConnectParams, SslMode};
use bulk_core::types::*;
use mysql_async::prelude::Queryable;
use tokio::sync::Mutex;

/// Upper bound for one copy job (the 200k-row PG table and the 131k-row MySQL table included).
const RUN_TIMEOUT: Duration = Duration::from_secs(900);
/// A cancelled copy must reach `cancelled` within this (core: `CANCEL_GRACE` = 2 s).
const CANCEL_LIMIT: Duration = Duration::from_secs(3);
const POLL: Duration = Duration::from_millis(10);

/// One mutex per MySQL server whose globals the tests change (section 9 "Isolation").
static MYSQL8_SERVER: Mutex<()> = Mutex::const_new(());
static MYSQL9_SERVER: Mutex<()> = Mutex::const_new(());
static MYSQL55_SERVER: Mutex<()> = Mutex::const_new(());

const PG_BIG_ROWS: i64 = 200_000;
/// `big` on the 5.5 source holds 2^17 rows (built by doubling; 5.5 has no generate_series).
const MY_BIG_LOG2: u32 = 17;
/// The 8.0 default, restored after (and set before) the tests that lower it.
const MY_DEFAULT_PACKET: u64 = 64 * 1024 * 1024;

// ---------------------------------------------------------------------------------------------
// URLs, names, harness
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct DbUrl {
    var: &'static str,
    user: String,
    password: String,
    host: String,
    port: u16,
    database: String,
}

/// `scheme://user:password@host:port/database[?…]` from `var` (percent-decoded), or `None`
/// with a skip note when the variable is unset or empty.
fn env_url(var: &'static str, default_port: u16) -> Option<DbUrl> {
    let raw = std::env::var(var).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
    let Some(raw) = raw else {
        eprintln!("skipping: {var} is not set");
        return None;
    };
    let rest = raw.split_once("://").map(|(_, r)| r).unwrap_or(&raw);
    let rest = rest.split('?').next().unwrap_or(rest);
    let (auth, hostdb) = match rest.rsplit_once('@') {
        Some((a, h)) => (Some(a), h),
        None => (None, rest),
    };
    let (hostport, database) = hostdb.split_once('/').unwrap_or((hostdb, ""));
    let (host, port) = match hostport.rsplit_once(':') {
        Some((h, p)) => (h, p.parse().unwrap_or(default_port)),
        None => (hostport, default_port),
    };
    let (user, password) = match auth {
        Some(a) => a.split_once(':').unwrap_or((a, "")),
        None => ("", ""),
    };
    assert!(!database.is_empty(), "{var} must name a database: scheme://user:pass@host:port/database");
    Some(DbUrl {
        var,
        user: percent_decode(user),
        password: percent_decode(password),
        host: if host.is_empty() { "127.0.0.1".into() } else { host.to_string() },
        port,
        database: percent_decode(database),
    })
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8(out).expect("URL parts are UTF-8")
}

fn unique() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

/// Upper-case hex of the UTF-8 bytes of `s` (what `HEX()` returns for a utf8mb4 column).
fn hex(s: &str) -> String {
    s.bytes().map(|b| format!("{b:02X}")).collect()
}

/// The version text from its first digit on ("PostgreSQL 16.4" -> "16.4", "5.5.62-log" stays).
fn version_num(v: &str) -> &str {
    v.trim_start_matches(|c: char| !c.is_ascii_digit())
}

fn some(s: &str) -> Option<String> {
    Some(s.to_string())
}

/// A `BulkDb` over a fresh config dir and a memory credential store.
struct Harness {
    _dir: tempfile::TempDir,
    db: Arc<BulkDb>,
}

impl Harness {
    async fn new() -> Harness {
        let dir = tempfile::tempdir().expect("temp dir");
        let db = BulkDb::new(BulkDbOptions {
            config_dir: Some(dir.path().join("config")),
            credentials: Some(Box::new(MemoryStore::default())),
            init_logging: false,
            ..Default::default()
        })
        .await
        .expect("BulkDb::new");
        Harness { _dir: dir, db }
    }

    /// Save a connection to `url` (password in the memory store). A copy target is marked
    /// write-allowed by a second save of the stored connection, the only way core accepts the
    /// flag (a new connection never starts write-allowed).
    fn save(&self, name: &str, driver: &str, url: &DbUrl, fields: &[(&str, &str)], write_allowed: bool) -> String {
        let connection = Connection {
            name: name.into(),
            driver: driver.into(),
            host: Some(url.host.clone()),
            port: Some(url.port),
            database: Some(url.database.clone()),
            user: Some(url.user.clone()).filter(|u| !u.is_empty()),
            fields: fields.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
            save_password: true,
            ..Default::default()
        };
        let password = Some(url.password.clone()).filter(|p| !p.is_empty());
        let saved = self.db.conn_save(ConnectionInput { connection, password, ..Default::default() }).expect("conn_save");
        assert_ne!(saved.write_allowed, Some(true), "a new connection never starts write-allowed");
        if write_allowed {
            let marked = self
                .db
                .conn_save(ConnectionInput { connection: Connection { write_allowed: Some(true), ..saved.clone() }, ..Default::default() })
                .expect("mark the target write-allowed");
            assert_eq!(marked.write_allowed, Some(true), "an existing connection accepts writeAllowed = true");
        }
        saved.id
    }
}

// ---------------------------------------------------------------------------------------------
// Copy driving
// ---------------------------------------------------------------------------------------------

fn request(src_conn: &str, src_db: &str, tgt_conn: &str, tgt_db: &str, selection: CopySelection, options: CopyOptions) -> CopyRequest {
    CopyRequest {
        source: CopyEndpoint { conn_id: src_conn.into(), database: src_db.into(), ..Default::default() },
        target: CopyEndpoint { conn_id: tgt_conn.into(), database: tgt_db.into(), ..Default::default() },
        selection,
        options,
        name: Some(format!("it {src_db} -> {tgt_db}")),
    }
}

/// Small writer chunks (so a cancel lands between chunks), two tables at once, and lock waits
/// long enough that only the cancel ends the writer blocked by the forced-cancel lock.
fn options(edit: impl FnOnce(&mut CopyOptions)) -> CopyOptions {
    let mut o = CopyOptions { table_concurrency: 2, batch_rows: Some(1000), batch_mb: Some(1), target_lock_timeout_s: Some(120), ..Default::default() };
    edit(&mut o);
    o
}

fn select_tables(schema: Option<&str>, names: &[&str]) -> CopySelection {
    CopySelection::Tables { tables: names.iter().map(|n| TableRef { database: None, schema: schema.map(str::to_string), name: n.to_string() }).collect() }
}

/// Plan item id of a MySQL / SQLite table (section 2 "Plan item ids").
fn my_item(table: &str) -> String {
    format!("table:\"{table}\"")
}

/// Plan item id of a PostgreSQL table.
fn pg_item(schema: &str, table: &str) -> String {
    format!("table:\"{schema}\".\"{table}\"")
}

fn ack_keys(plan: &CopyPlan) -> Vec<String> {
    plan.acks.iter().map(|a| a.key.clone()).collect()
}

/// Codes of every issue of a plan (plan warnings, blockers and item warnings).
fn issue_codes(plan: &CopyPlan) -> BTreeSet<String> {
    plan.warnings.iter().chain(&plan.blockers).chain(plan.items.iter().flat_map(|i| &i.warnings)).map(|i| i.code.clone()).collect()
}

fn issues_text(issues: &[CopyIssue]) -> String {
    issues.iter().map(|i| format!("\n  {} [{}] {}", i.code, i.item_id.as_deref().unwrap_or("-"), i.message)).collect()
}

/// Everything useful about a job for an assertion message.
fn describe(db: &BulkDb, st: &CopyStatus) -> String {
    let mut s = format!("job {} (run {}) is {:?}", st.job_id, st.run_id, st.state);
    if let Ok(job) = db.job_get(&st.job_id) {
        if let Some(e) = job.error {
            s.push_str(&format!("\n  job error: {e:?}"));
        }
    }
    if let Some(plan) = &st.plan {
        if !plan.blockers.is_empty() {
            s.push_str(&format!("\n  blockers:{}", issues_text(&plan.blockers)));
        }
    }
    if let Some(r) = &st.result {
        for t in r.tables.iter().filter(|t| t.state != CopyItemState::Done || !t.problems.is_empty() || t.error.is_some()) {
            s.push_str(&format!(
                "\n  table {} {:?} verify {:?} problems {:?} samples {:?} error {:?}",
                t.item_id, t.state, t.verify, t.problems, t.warning_samples, t.error
            ));
        }
        for o in r.objects.iter().filter(|o| o.state == CopyItemState::Failed) {
            s.push_str(&format!("\n  object {} {:?} error {:?}", o.item_id, o.state, o.error));
        }
    }
    s
}

fn assert_done(db: &BulkDb, st: &CopyStatus) {
    assert_eq!(st.state, JobState::Done, "{}", describe(db, st));
}

/// `copy_plan`, asserting that nothing blocks it.
async fn plan_ok(db: &BulkDb, req: &CopyRequest) -> CopyPlan {
    let plan = db.copy_plan(req, None).await.unwrap_or_else(|e| panic!("copy_plan: {e:?}"));
    assert!(plan.blockers.is_empty(), "unexpected blockers:{}", issues_text(&plan.blockers));
    plan
}

/// `copy_start` with every acknowledgement the reviewed plan asks for.
async fn start(db: &BulkDb, req: &CopyRequest, plan: &CopyPlan) -> String {
    db.copy_start(req.clone(), plan.plan_id.clone(), ack_keys(plan)).await.unwrap_or_else(|e| panic!("copy_start: {e:?}"))
}

/// Keys a warned copy job waits for: its re-planned `acks`, plus the codes of its preflight
/// report (what the generic `job_resume` compares).
fn warned_keys(db: &BulkDb, job: &str, st: &CopyStatus) -> Vec<String> {
    let mut keys: Vec<String> = st.plan.iter().flat_map(ack_keys).collect();
    if let Some(report) = db.job_get(job).ok().and_then(|j| j.preflight) {
        keys.extend(report.warnings.iter().map(|w| w.code.clone()));
    }
    keys.sort();
    keys.dedup();
    keys
}

/// Wait until the job is terminal or warned.
async fn settle(db: &BulkDb, job: &str) -> CopyStatus {
    let deadline = Instant::now() + RUN_TIMEOUT;
    loop {
        let st = db.copy_status(job).unwrap_or_else(|e| panic!("copy_status {job}: {e:?}"));
        if st.state.is_terminal() || st.state == JobState::Warned {
            return st;
        }
        assert!(Instant::now() < deadline, "copy job did not finish within {RUN_TIMEOUT:?}: {}", describe(db, &st));
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Run a job to its end. A warned job (a resume's new acknowledgements) is acknowledged with
/// what it asks for; `PLAN_CHANGED` after a fresh `copy_plan` would mean an unstable plan id.
async fn drive(db: &BulkDb, job: &str) -> CopyStatus {
    for _ in 0..3 {
        let st = settle(db, job).await;
        if st.state != JobState::Warned {
            return st;
        }
        if let Some(plan) = &st.plan {
            assert!(!issue_codes(plan).contains("PLAN_CHANGED") || plan.resume_of.is_some(), "the plan changed between copy_plan and copy_start: {}", describe(db, &st));
        }
        db.job_resume(job, warned_keys(db, job, &st)).unwrap_or_else(|e| panic!("job_resume: {e:?}\n{}", describe(db, &st)));
    }
    panic!("copy job {job} kept asking for acknowledgements")
}

/// Plan, start with the plan's acknowledgements, run to the end.
async fn copy_to_end(db: &BulkDb, req: &CopyRequest) -> (CopyPlan, CopyStatus) {
    let plan = plan_ok(db, req).await;
    let job = start(db, req, &plan).await;
    let st = drive(db, &job).await;
    (plan, st)
}

fn table_result<'a>(st: &'a CopyStatus, item_id: &str) -> &'a CopyTableResult {
    let r = st.result.as_ref().unwrap_or_else(|| panic!("job {} has no result", st.job_id));
    r.tables.iter().find(|t| t.item_id == item_id).unwrap_or_else(|| panic!("no table result {item_id} in {:?}", r.tables.iter().map(|t| &t.item_id).collect::<Vec<_>>()))
}

fn plan_item<'a>(plan: &'a CopyPlan, item_id: &str) -> &'a PlanItem {
    plan.items.iter().find(|i| i.id == item_id).unwrap_or_else(|| panic!("no plan item {item_id}"))
}

/// The table names `code` as a problem or in its error.
fn mentions(t: &CopyTableResult, code: &str) -> bool {
    t.problems.iter().any(|p| p == code)
        || t.error.as_ref().is_some_and(|e| e.code == code || e.message.contains(code) || e.detail.as_deref().is_some_and(|d| d.contains(code)))
}

fn fix_count(t: &CopyTableResult, column: &str, code: &str) -> u64 {
    t.value_fixes.iter().filter(|f| f.column == column && f.code == code).map(|f| f.count).sum()
}

/// Every table this job loaded was verified as a match (row count or checksum).
fn assert_loaded_tables_match(db: &BulkDb, st: &CopyStatus) {
    let r = st.result.as_ref().unwrap_or_else(|| panic!("no result: {}", describe(db, st)));
    for t in r.tables.iter().filter(|t| t.rows_written > 0) {
        assert_eq!(t.verify, VerifyOutcome::Match, "table {} was loaded but not verified: {}", t.item_id, describe(db, st));
        assert_eq!(t.rows_read, t.rows_written, "table {}", t.item_id);
        if t.checksum_source.is_some() || t.checksum_target.is_some() {
            assert_eq!(t.checksum_source, t.checksum_target, "table {}", t.item_id);
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Forced cancel + resume
// ---------------------------------------------------------------------------------------------

/// A session on the target that locks one table so the copy's writer blocks on it.
enum Blocker {
    Pg { client: tokio_postgres::Client, table: String },
    My { conn: mysql_async::Conn, db: String, table: String },
}

impl Blocker {
    async fn table_exists(&mut self) -> bool {
        match self {
            Blocker::Pg { client, table } => client.query_one("SELECT to_regclass($1) IS NOT NULL", &[&table.as_str()]).await.expect("to_regclass").get(0),
            Blocker::My { conn, db, table } => {
                let n: Option<u64> = conn
                    .exec_first("SELECT COUNT(*) FROM information_schema.TABLES WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ?", (db.as_str(), table.as_str()))
                    .await
                    .expect("information_schema.TABLES");
                n.unwrap_or(0) > 0
            }
        }
    }

    /// Waits for whatever the writer holds on the table right now (the current chunk or
    /// COPY), then holds the lock until [`Blocker::unlock`].
    async fn lock(&mut self) {
        match self {
            Blocker::Pg { client, table } => {
                client.batch_execute(&format!("BEGIN; SET LOCAL lock_timeout = '300s'; LOCK TABLE {table} IN ACCESS EXCLUSIVE MODE")).await.expect("LOCK TABLE");
            }
            Blocker::My { conn, db, table } => {
                conn.query_drop("SET SESSION lock_wait_timeout = 300").await.expect("lock_wait_timeout");
                conn.query_drop(format!("LOCK TABLES `{db}`.`{table}` WRITE")).await.expect("LOCK TABLES");
            }
        }
    }

    async fn unlock(&mut self) {
        match self {
            Blocker::Pg { client, .. } => client.batch_execute("ROLLBACK").await.expect("ROLLBACK"),
            Blocker::My { conn, .. } => conn.query_drop("UNLOCK TABLES").await.expect("UNLOCK TABLES"),
        }
    }
}

/// Start the copy, block its writer on `blocker`'s table, cancel (must be `cancelled` within
/// 3 s), release the lock, then `copy_resume` the run to `done`. Returns the reviewed plan and
/// the resumed job's final status.
async fn copy_with_forced_cancel(db: &BulkDb, req: &CopyRequest, blocker: &mut Blocker) -> (CopyPlan, CopyStatus) {
    let plan = plan_ok(db, req).await;
    let job = start(db, req, &plan).await;

    let deadline = Instant::now() + RUN_TIMEOUT;
    loop {
        let st = db.copy_status(&job).unwrap_or_else(|e| panic!("copy_status: {e:?}"));
        assert!(!st.state.is_terminal(), "the copy ended before it created the table to block: {}", describe(db, &st));
        if st.state == JobState::Warned {
            db.job_resume(&job, warned_keys(db, &job, &st)).unwrap_or_else(|e| panic!("job_resume: {e:?}"));
        }
        if blocker.table_exists().await {
            break;
        }
        assert!(Instant::now() < deadline, "the copy never created the table to block: {}", describe(db, &st));
        tokio::time::sleep(POLL).await;
    }
    blocker.lock().await;
    // Every remaining step on that table (load, indexes, ANALYZE, verification) now waits.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let st = db.copy_status(&job).expect("copy_status");
    assert!(!st.state.is_terminal(), "the copy finished before the forced cancel: {}", describe(db, &st));

    let t0 = Instant::now();
    db.job_cancel(&job).expect("job_cancel");
    let cancelled = loop {
        let st = db.copy_status(&job).expect("copy_status");
        if st.state.is_terminal() {
            break st;
        }
        assert!(t0.elapsed() < CANCEL_LIMIT, "a writer blocked on a lock was not cancelled within {CANCEL_LIMIT:?}: {}", describe(db, &st));
        tokio::time::sleep(POLL).await;
    };
    let took = t0.elapsed();
    blocker.unlock().await;
    assert_eq!(cancelled.state, JobState::Cancelled, "{}", describe(db, &cancelled));
    assert!(took < CANCEL_LIMIT, "cancel took {took:?}");
    let result = cancelled.result.as_ref().unwrap_or_else(|| panic!("a cancelled copy stores its result: {}", describe(db, &cancelled)));
    assert!(result.resumable, "a cancelled (non-dry) run is resumable");
    assert!(!result.dry_run);

    let run_id = cancelled.run_id.clone();
    let resumed = db.copy_resume(CopyResumeRequest { run_id: run_id.clone(), ..Default::default() }).await.unwrap_or_else(|e| panic!("copy_resume: {e:?}"));
    assert_ne!(resumed, job, "a resume is a new job");
    let st = drive(db, &resumed).await;
    assert_done(db, &st);
    assert_eq!(st.run_id, run_id, "a resume continues the same run");
    assert_eq!(st.plan.as_ref().and_then(|p| p.resume_of.as_deref()), Some(run_id.as_str()), "the resumed plan names its run");
    let runs = db.copy_runs(None, 50).expect("copy_runs");
    let run = runs.iter().find(|r| r.run_id == run_id).unwrap_or_else(|| panic!("run {run_id} not listed"));
    assert_eq!(run.last_job_id, resumed);
    assert!(!run.in_use, "a finished run is not in use");
    (plan, st)
}

// ---------------------------------------------------------------------------------------------
// MySQL helpers
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
enum MyTarget {
    Mysql8,
    Mysql9,
}

impl MyTarget {
    fn var(self) -> &'static str {
        match self {
            MyTarget::Mysql8 => "BULKDB_MYSQL_URL",
            MyTarget::Mysql9 => "BULKDB_MYSQL9_URL",
        }
    }

    fn server(self) -> &'static Mutex<()> {
        match self {
            MyTarget::Mysql8 => &MYSQL8_SERVER,
            MyTarget::Mysql9 => &MYSQL9_SERVER,
        }
    }

    /// The 5.5 source and this target, or `None` (skip) when either is not configured.
    fn servers(self) -> Option<(DbUrl, DbUrl)> {
        let src = env_url("BULKDB_MYSQL55_URL", 3306);
        let tgt = env_url(self.var(), 3306);
        Some((src?, tgt?))
    }
}

/// Admin session (fixtures, globals, checks; plain mysql_async, not BulkDB). `old_password`:
/// the 5.5 root account has a pre-4.1 hash.
async fn my_admin(url: &DbUrl, db: Option<&str>, old_password: bool) -> mysql_async::Conn {
    let opts = mysql_async::OptsBuilder::default()
        .ip_or_hostname(url.host.clone())
        .tcp_port(url.port)
        .user(Some(url.user.clone()))
        .pass(Some(url.password.clone()).filter(|p| !p.is_empty()))
        .db_name(db.map(str::to_string))
        .prefer_socket(false)
        .secure_auth(!old_password);
    mysql_async::Conn::new(opts).await.unwrap_or_else(|e| panic!("admin connection to {}: {e}", url.var))
}

async fn my_exec(conn: &mut mysql_async::Conn, statements: &[String]) {
    for sql in statements {
        conn.query_drop(sql.as_str()).await.unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
}

async fn my_global(conn: &mut mysql_async::Conn, name: &str) -> String {
    let v: Option<String> = conn.query_first(format!("SELECT CAST(@@global.{name} AS CHAR)")).await.unwrap_or_else(|e| panic!("@@global.{name}: {e}"));
    v.unwrap_or_default()
}

async fn my_count(conn: &mut mysql_async::Conn, db: &str, table: &str) -> u64 {
    let n: Option<u64> = conn.query_first(format!("SELECT COUNT(*) FROM `{db}`.`{table}`")).await.unwrap_or_else(|e| panic!("count {db}.{table}: {e}"));
    n.expect("COUNT(*) returns a row")
}

async fn my_drop_database(url: &DbUrl, db: &str, old_password: bool) {
    let mut conn = my_admin(url, None, old_password).await;
    conn.query_drop(format!("DROP DATABASE IF EXISTS `{db}`")).await.unwrap_or_else(|e| panic!("drop {db}: {e}"));
    let _ = conn.disconnect().await;
}

/// The 5.5 source: an euckr database with
/// * `member`: Korean text, ENUM, TEXT, TIMESTAMP, zero and invalid DATE/DATETIME, zero
///   YEAR, DECIMAL, BLOB, BIT; AUTO_INCREMENT moved to 1000;
/// * `legacy_l1`: a latin1 table holding CP949 bytes, as old applications wrote them;
/// * `big`: `2^big_log2` rows of Korean text (0 = no such table).
async fn mysql55_source(src: &DbUrl, db: &str, big_log2: u32) {
    let mut adm = my_admin(src, None, true).await;
    my_exec(
        &mut adm,
        &[
            // UTF-8 statement text; the server converts it into each column's charset.
            "SET NAMES utf8".into(),
            "SET SESSION sql_mode = 'ALLOW_INVALID_DATES'".into(),
            "SET time_zone = '+00:00'".into(),
            format!("DROP DATABASE IF EXISTS `{db}`"),
            format!("CREATE DATABASE `{db}` DEFAULT CHARACTER SET euckr COLLATE euckr_korean_ci"),
            format!(
                "CREATE TABLE `{db}`.`member` (\
                   `id` INT NOT NULL AUTO_INCREMENT, \
                   `name` VARCHAR(20) NOT NULL, \
                   `kind` ENUM('가','나') NULL, \
                   `memo` TEXT NULL, \
                   `ts` TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP, \
                   `d` DATE NULL, \
                   `dt` DATETIME NULL, \
                   `y` YEAR(4) NULL, \
                   `price` DECIMAL(8,2) NULL, \
                   `data` BLOB NULL, \
                   `flags` BIT(8) NULL, \
                   PRIMARY KEY (`id`), \
                   KEY `name_idx` (`name`)\
                 ) ENGINE=InnoDB DEFAULT CHARSET=euckr COMMENT='회원 명부'"
            ),
            format!(
                "INSERT INTO `{db}`.`member` (id, name, kind, memo, ts, d, dt, y, price, data, flags) VALUES \
                 (1, '홍길동', '가', '한글 메모 텍스트', '2024-03-01 12:34:56', '2024-02-29', '2024-02-29 23:59:59', 2024, 12345.67, X'00FF10C7D1', b'10100101'), \
                 (2, '김철수', '나', NULL, '2001-09-09 01:46:40', '0000-00-00', '0000-00-00 00:00:00', 0, -0.50, NULL, b'00000000'), \
                 (3, 'abc', NULL, '', '2010-01-01 00:00:00', '2004-02-31', '2004-04-31 10:00:00', 1999, NULL, X'', NULL)"
            ),
            format!("ALTER TABLE `{db}`.`member` AUTO_INCREMENT = 1000"),
            format!("CREATE TABLE `{db}`.`legacy_l1` (`id` INT NOT NULL PRIMARY KEY, `name` VARCHAR(20) NULL) ENGINE=InnoDB DEFAULT CHARSET=latin1"),
            // "한글" and "한글 한글" in CP949, stored byte for byte in latin1 columns.
            format!("INSERT INTO `{db}`.`legacy_l1` VALUES (1, _latin1 X'C7D1B1DB'), (2, 'abc'), (3, _latin1 X'C7D1B1DB20C7D1B1DB')"),
        ],
    )
    .await;
    if big_log2 > 0 {
        my_exec(
            &mut adm,
            &[
                format!(
                    "CREATE TABLE `{db}`.`big` (`id` INT NOT NULL AUTO_INCREMENT PRIMARY KEY, `k` INT NOT NULL, `name` VARCHAR(40) NOT NULL, \
                     `note` VARCHAR(200) NULL, KEY `k_idx` (`k`)) ENGINE=InnoDB DEFAULT CHARSET=euckr"
                ),
                format!("INSERT INTO `{db}`.`big` (k, name, note) VALUES (1, '대용량 한글 행', REPEAT('가나다라', 40))"),
            ],
        )
        .await;
        for i in 0..big_log2 {
            let n = 1u64 << i;
            my_exec(&mut adm, &[format!("INSERT INTO `{db}`.`big` (k, name, note) SELECT k + {n}, name, note FROM `{db}`.`big`")]).await;
        }
        assert_eq!(my_count(&mut adm, db, "big").await, 1 << big_log2);
    }
    let _ = adm.disconnect().await;
}

/// Saved connections: the 5.5 source read with `encoding=euc-kr` (CP949 in latin1 columns
/// decodes correctly) and the write-allowed target.
fn mysql_connections(h: &Harness, src: &DbUrl, tgt: &DbUrl) -> (String, String) {
    let source = h.save("mysql55 euckr", "mysql", src, &[("allow_old_password", "true"), ("encoding", "euc-kr")], false);
    let target = h.save("mysql target", "mysql", tgt, &[], true);
    (source, target)
}

type MemberRow = (i64, String, Option<String>, Option<String>, Option<String>, Option<i64>, Option<String>, Option<String>, Option<u64>);

fn expected_member(policy: ZeroDatePolicy) -> Vec<MemberRow> {
    let keep = policy == ZeroDatePolicy::Keep;
    let kept = |v: &str| if keep { some(v) } else { None };
    vec![
        (1, hex("홍길동"), some("가"), some("2024-02-29"), some("2024-02-29 23:59:59"), Some(2024), some("12345.67"), some("00FF10C7D1"), Some(165)),
        (2, hex("김철수"), some("나"), kept("0000-00-00"), kept("0000-00-00 00:00:00"), if keep { Some(0) } else { None }, some("-0.50"), None, Some(0)),
        // '2004-02-31' / '2004-04-31' are impossible dates: kept under `keep` (ALLOW_INVALID_DATES), else NULL.
        (3, hex("abc"), None, kept("2004-02-31"), kept("2004-04-31 10:00:00"), Some(1999), None, some(""), None),
    ]
}

/// `member` and `legacy_l1` on the target: UTF-8 bytes, ENUM, dates per policy, TIMESTAMP
/// instants equal to the source's, YEAR, DECIMAL, BLOB, BIT.
async fn check_member(src_adm: &mut mysql_async::Conn, src_db: &str, tgt: &mut mysql_async::Conn, tgt_db: &str, policy: ZeroDatePolicy, with_l1: bool) {
    let sql = |db: &str| {
        format!(
            "SELECT id, HEX(name), kind, CAST(d AS CHAR), CAST(dt AS CHAR), CAST(y AS SIGNED), CAST(price AS CHAR), HEX(data), CAST(flags AS UNSIGNED) \
             FROM `{db}`.`member` ORDER BY id"
        )
    };
    let rows: Vec<MemberRow> = tgt.query(sql(tgt_db)).await.expect("read target member");
    assert_eq!(rows, expected_member(policy), "member on the target (zero dates {policy:?})");

    let epochs = |db: &str| format!("SELECT id, CAST(UNIX_TIMESTAMP(ts) AS SIGNED) FROM `{db}`.`member` ORDER BY id");
    let src_epochs: Vec<(i64, i64)> = src_adm.query(epochs(src_db)).await.expect("source epochs");
    let tgt_epochs: Vec<(i64, i64)> = tgt.query(epochs(tgt_db)).await.expect("target epochs");
    assert_eq!(src_epochs.len(), 3);
    assert_eq!(tgt_epochs, src_epochs, "TIMESTAMP instants survive the copy (UTC sessions on both sides)");

    if with_l1 {
        let l1: Vec<(i64, Option<String>)> = tgt.query(format!("SELECT id, HEX(name) FROM `{tgt_db}`.`legacy_l1` ORDER BY id")).await.expect("read target legacy_l1");
        assert_eq!(l1, vec![(1, some(&hex("한글"))), (2, some(&hex("abc"))), (3, some(&hex("한글 한글")))], "CP949 held in latin1 arrives as UTF-8 Korean");
    }
}

/// Target DDL: utf8mb4 + the 8.0/9.x default collation, InnoDB, the ENUM in UTF-8, the
/// AUTO_INCREMENT carried over, and the database created with utf8mb4.
async fn check_mysql_ddl(tgt: &mut mysql_async::Conn, tgt_db: &str) {
    let (_, ddl): (String, String) = tgt.query_first(format!("SHOW CREATE TABLE `{tgt_db}`.`member`")).await.expect("SHOW CREATE TABLE").expect("a row");
    assert!(ddl.contains("utf8mb4"), "{ddl}");
    assert!(ddl.contains("ENGINE=InnoDB"), "{ddl}");
    assert!(ddl.contains("enum('가','나')"), "{ddl}");
    // 8.0+ caches information_schema statistics for 24 h by default.
    let _ = tgt.query_drop("SET SESSION information_schema_stats_expiry = 0").await;
    for table in ["member", "legacy_l1"] {
        let row: Option<(String, String)> = tgt
            .exec_first("SELECT ENGINE, TABLE_COLLATION FROM information_schema.TABLES WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ?", (tgt_db, table))
            .await
            .expect("information_schema.TABLES");
        assert_eq!(row, Some(("InnoDB".to_string(), "utf8mb4_0900_ai_ci".to_string())), "{table}");
    }
    let charset: Option<String> = tgt
        .exec_first("SELECT CHARACTER_SET_NAME FROM information_schema.COLUMNS WHERE TABLE_SCHEMA = ? AND TABLE_NAME = 'member' AND COLUMN_NAME = 'name'", (tgt_db,))
        .await
        .expect("information_schema.COLUMNS");
    assert_eq!(charset.as_deref(), Some("utf8mb4"));
    let auto: Option<Option<u64>> =
        tgt.exec_first("SELECT AUTO_INCREMENT FROM information_schema.TABLES WHERE TABLE_SCHEMA = ? AND TABLE_NAME = 'member'", (tgt_db,)).await.expect("AUTO_INCREMENT");
    assert_eq!(auto.flatten(), Some(1000), "the AUTO_INCREMENT item carries the source's next value");
    let db_charset: Option<String> =
        tgt.exec_first("SELECT DEFAULT_CHARACTER_SET_NAME FROM information_schema.SCHEMATA WHERE SCHEMA_NAME = ?", (tgt_db,)).await.expect("SCHEMATA");
    assert_eq!(db_charset.as_deref(), Some("utf8mb4"), "the missing target database is created with utf8mb4");
}

/// Set the target's `local_infile` (test setup; the server lock must be held).
async fn set_local_infile(adm: &mut mysql_async::Conn, on: bool) {
    my_exec(adm, &[format!("SET GLOBAL local_infile = {}", u8::from(on))]).await;
}

// ---------------------------------------------------------------------------------------------
// MySQL 5.5 euckr -> 8.0 / 9.x
// ---------------------------------------------------------------------------------------------

/// The main MySQL path: charset sampling, LOAD DATA with a forced cancel and resume, the
/// INSERT path, and both zero-date policies.
async fn mysql55_copy(t: MyTarget) {
    let Some((src, tgt)) = t.servers() else { return };
    let _server = t.server().lock().await;
    let id = unique();
    let src_db = format!("bulkdb_it_src_{id}");
    let (db_a, db_b, db_c) = (format!("bulkdb_it_{id}_a"), format!("bulkdb_it_{id}_b"), format!("bulkdb_it_{id}_c"));
    mysql55_source(&src, &src_db, MY_BIG_LOG2).await;
    let mut tgt_adm = my_admin(&tgt, None, false).await;
    let infile_before = my_global(&mut tgt_adm, "local_infile").await;
    my_exec(&mut tgt_adm, &[format!("SET GLOBAL max_allowed_packet = {MY_DEFAULT_PACKET}")]).await;

    let h = Harness::new().await;
    let (source, target) = mysql_connections(&h, &src, &tgt);

    // 1. encoding = auto: the server's latin1 -> utf8 conversion would double-encode the CP949
    //    bytes; the charset sample catches it before anything is written.
    let auto_source = h.save("mysql55 auto", "mysql", &src, &[("allow_old_password", "true")], false);
    let req = request(&auto_source, &src_db, &target, &db_a, CopySelection::All, options(|_| {}));
    let plan = h.db.copy_plan(&req, None).await.unwrap_or_else(|e| panic!("copy_plan: {e:?}"));
    let blocker = plan.blockers.iter().find(|b| b.code == "SRC_LATIN1_KOREAN");
    assert!(blocker.is_some(), "a latin1 column holding CP949 read with encoding=auto must block:{}", issues_text(&plan.blockers));

    // 2. LOAD DATA LOCAL, zero dates -> NULL, every table; cancelled while blocked, resumed.
    set_local_infile(&mut tgt_adm, true).await;
    let req = request(&source, &src_db, &target, &db_a, CopySelection::All, options(|o| o.mysql.zero_dates = ZeroDatePolicy::Null));
    let mut lock = Blocker::My { conn: my_admin(&tgt, None, false).await, db: db_a.clone(), table: "big".into() };
    let (plan, st) = copy_with_forced_cancel(&h.db, &req, &mut lock).await;
    assert_eq!(plan.family, CopyFamily::Mysql);
    assert!(version_num(&plan.source.version).starts_with("5.5."), "{}", plan.source.version);
    assert_eq!(plan.effective.load_method, LoadMethod::MysqlLoadData);
    assert_eq!(plan.effective.charset.as_deref(), Some("utf8mb4"));
    assert_eq!(plan.effective.collation.as_deref(), Some("utf8mb4_0900_ai_ci"));
    assert_eq!(plan.effective.engine.as_deref().map(str::to_ascii_lowercase).as_deref(), Some("innodb"));
    assert_eq!(plan.effective.zero_dates, Some(ZeroDatePolicy::Null));
    assert!(plan.items.iter().any(|i| i.kind == CopyObjectKind::Database && i.action == PlanAction::Create), "the missing target database is created");
    assert!(st.result.as_ref().is_some_and(|r| !r.dry_run));
    let mut src_adm = my_admin(&src, Some(&src_db), true).await;
    for table in ["member", "legacy_l1", "big"] {
        assert_eq!(my_count(&mut tgt_adm, &db_a, table).await, my_count(&mut src_adm, &src_db, table).await, "row count of {table}");
    }
    check_member(&mut src_adm, &src_db, &mut tgt_adm, &db_a, ZeroDatePolicy::Null, true).await;
    check_mysql_ddl(&mut tgt_adm, &db_a).await;
    let big = |db: &str| format!("SELECT COUNT(*), CAST(SUM(k) AS SIGNED) FROM `{db}`.`big`");
    let src_big: Option<(u64, i64)> = src_adm.query_first(big(&src_db)).await.expect("source big");
    let tgt_big: Option<(u64, i64)> = tgt_adm.query_first(big(&db_a)).await.expect("target big");
    assert_eq!(tgt_big, src_big, "big: count and key sum");
    let sample: Option<(String, u64)> = tgt_adm.query_first(format!("SELECT HEX(name), CHAR_LENGTH(note) FROM `{db_a}`.`big` WHERE k = 777")).await.expect("big sample");
    assert_eq!(sample, Some((hex("대용량 한글 행"), 160)));

    // 3. INSERT path (local_infile OFF), zero dates -> NULL: a clean run of the small tables
    //    reports the replacements per column and the invalid dates as a problem.
    set_local_infile(&mut tgt_adm, false).await;
    let req = request(&source, &src_db, &target, &db_b, select_tables(None, &["member", "legacy_l1"]), options(|o| o.mysql.zero_dates = ZeroDatePolicy::Null));
    let (plan, st) = copy_to_end(&h.db, &req).await;
    assert_eq!(plan.effective.load_method, LoadMethod::MysqlInsert);
    assert_done(&h.db, &st);
    let member = table_result(&st, &my_item("member"));
    assert_eq!(member.state, CopyItemState::Done, "{}", describe(&h.db, &st));
    assert_eq!(member.method, Some(LoadMethod::MysqlInsert));
    assert_eq!(member.rows_written, 3);
    assert_eq!(fix_count(member, "d", "INVALID_DATE"), 1, "{:?}", member.value_fixes);
    assert_eq!(fix_count(member, "dt", "INVALID_DATE"), 1, "{:?}", member.value_fixes);
    assert_eq!(fix_count(member, "d", "ZERO_DATE_NULL"), 1, "{:?}", member.value_fixes);
    assert_eq!(fix_count(member, "dt", "ZERO_DATE_NULL"), 1, "{:?}", member.value_fixes);
    assert!(member.problems.iter().any(|p| p == "INVALID_DATE"), "invalid dates are a problem: {:?}", member.problems);
    let result = st.result.as_ref().expect("result");
    assert!(result.problems > 0, "done with problems");
    assert!(!result.verified, "a run with problems is not 'verified'");
    let l1 = table_result(&st, &my_item("legacy_l1"));
    assert_eq!((l1.state, l1.verify, l1.unmappable_chars), (CopyItemState::Done, VerifyOutcome::Match, 0));
    check_member(&mut src_adm, &src_db, &mut tgt_adm, &db_b, ZeroDatePolicy::Null, true).await;

    // 4. LOAD DATA, zero dates kept (the target session allows them), invalid dates kept.
    set_local_infile(&mut tgt_adm, true).await;
    let req = request(&source, &src_db, &target, &db_c, select_tables(None, &["member"]), options(|o| o.mysql.zero_dates = ZeroDatePolicy::Keep));
    let (plan, st) = copy_to_end(&h.db, &req).await;
    assert_eq!(plan.effective.load_method, LoadMethod::MysqlLoadData);
    assert_eq!(plan.effective.zero_dates, Some(ZeroDatePolicy::Keep));
    assert!(issue_codes(&plan).contains("ZERO_DATE_KEEP"), "{:?}", issue_codes(&plan));
    assert_done(&h.db, &st);
    let member = table_result(&st, &my_item("member"));
    assert_eq!(member.method, Some(LoadMethod::MysqlLoadData));
    assert_eq!(member.data_warnings, 0, "{:?}", member.warning_samples);
    check_member(&mut src_adm, &src_db, &mut tgt_adm, &db_c, ZeroDatePolicy::Keep, false).await;

    h.db.shutdown().await;
    let restore = if infile_before == "1" || infile_before.eq_ignore_ascii_case("on") { "1" } else { "0" };
    my_exec(&mut tgt_adm, &[format!("SET GLOBAL local_infile = {restore}")]).await;
    let _ = src_adm.disconnect().await;
    let _ = tgt_adm.disconnect().await;
    for db in [&db_a, &db_b, &db_c] {
        my_drop_database(&tgt, db, false).await;
    }
    my_drop_database(&src, &src_db, true).await;
}

/// A 5 MB BLOB against a 4 MB target `max_allowed_packet` fails the table with
/// `ROW_TOO_LARGE` on both load methods (LOAD DATA would otherwise store NULL from UNHEX).
async fn mysql55_row_too_large(t: MyTarget) {
    let Some((src, tgt)) = t.servers() else { return };
    let _server = t.server().lock().await;
    let _source_server = MYSQL55_SERVER.lock().await;
    let id = unique();
    let src_db = format!("bulkdb_it_src_{id}");
    let mut src_adm = my_admin(&src, None, true).await;
    let packet55 = my_global(&mut src_adm, "max_allowed_packet").await;
    // The source must be able to send (and the fixture to build) the 5 MB value.
    my_exec(&mut src_adm, &["SET GLOBAL max_allowed_packet = 16777216".into()]).await;
    let _ = src_adm.disconnect().await;
    let mut src_adm = my_admin(&src, None, true).await;
    my_exec(
        &mut src_adm,
        &[
            format!("DROP DATABASE IF EXISTS `{src_db}`"),
            format!("CREATE DATABASE `{src_db}` DEFAULT CHARACTER SET euckr"),
            format!("CREATE TABLE `{src_db}`.`blobs` (`id` INT NOT NULL PRIMARY KEY, `data` LONGBLOB NULL) ENGINE=InnoDB"),
            format!("INSERT INTO `{src_db}`.`blobs` VALUES (1, REPEAT('x', 5242880)), (2, 'small')"),
        ],
    )
    .await;
    let len: Option<Option<u64>> = src_adm.query_first(format!("SELECT LENGTH(data) FROM `{src_db}`.`blobs` WHERE id = 1")).await.expect("blob length");
    assert_eq!(len.flatten(), Some(5_242_880), "the fixture holds the 5 MB value (REPEAT returns NULL above max_allowed_packet)");

    let mut tgt_adm = my_admin(&tgt, None, false).await;
    let infile_before = my_global(&mut tgt_adm, "local_infile").await;
    my_exec(&mut tgt_adm, &["SET GLOBAL max_allowed_packet = 4194304".into()]).await;
    let h = Harness::new().await;
    let (source, target) = mysql_connections(&h, &src, &tgt);
    let mut targets = Vec::new();
    for (n, load_data) in [(1, true), (2, false)] {
        set_local_infile(&mut tgt_adm, load_data).await;
        let tgt_db = format!("bulkdb_it_{id}_{n}");
        targets.push(tgt_db.clone());
        let req = request(&source, &src_db, &target, &tgt_db, CopySelection::All, options(|_| {}));
        let (plan, st) = copy_to_end(&h.db, &req).await;
        let method = if load_data { LoadMethod::MysqlLoadData } else { LoadMethod::MysqlInsert };
        assert_eq!(plan.effective.load_method, method);
        assert_eq!(st.state, JobState::Failed, "{}", describe(&h.db, &st));
        let blobs = table_result(&st, &my_item("blobs"));
        assert_eq!(blobs.state, CopyItemState::Failed, "{}", describe(&h.db, &st));
        assert!(mentions(blobs, "ROW_TOO_LARGE"), "{method:?}: {}", describe(&h.db, &st));
        assert_ne!(blobs.verify, VerifyOutcome::Match);
    }

    h.db.shutdown().await;
    let restore = if infile_before == "1" || infile_before.eq_ignore_ascii_case("on") { "1" } else { "0" };
    my_exec(&mut tgt_adm, &[format!("SET GLOBAL max_allowed_packet = {MY_DEFAULT_PACKET}"), format!("SET GLOBAL local_infile = {restore}")]).await;
    my_exec(&mut src_adm, &[format!("SET GLOBAL max_allowed_packet = {}", packet55.parse::<u64>().unwrap_or(1_048_576))]).await;
    let _ = src_adm.disconnect().await;
    let _ = tgt_adm.disconnect().await;
    for db in &targets {
        my_drop_database(&tgt, db, false).await;
    }
    my_drop_database(&src, &src_db, true).await;
}

/// A value the target truncates (append into an existing narrower column) is a LOAD DATA
/// warning: the chunk is rolled back and the table fails with `DATA_WARNINGS`, `lossy`.
async fn mysql55_data_warnings(t: MyTarget) {
    let Some((src, tgt)) = t.servers() else { return };
    let _server = t.server().lock().await;
    let id = unique();
    let src_db = format!("bulkdb_it_src_{id}");
    let tgt_db = format!("bulkdb_it_{id}");
    let mut src_adm = my_admin(&src, None, true).await;
    my_exec(
        &mut src_adm,
        &[
            "SET NAMES utf8".into(),
            format!("DROP DATABASE IF EXISTS `{src_db}`"),
            format!("CREATE DATABASE `{src_db}` DEFAULT CHARACTER SET euckr"),
            format!("CREATE TABLE `{src_db}`.`narrow` (`id` INT NOT NULL PRIMARY KEY, `name` VARCHAR(20) NOT NULL) ENGINE=InnoDB DEFAULT CHARSET=euckr"),
            format!("INSERT INTO `{src_db}`.`narrow` VALUES (1, '가나다라마바'), (2, '가')"),
        ],
    )
    .await;
    let mut tgt_adm = my_admin(&tgt, None, false).await;
    let infile_before = my_global(&mut tgt_adm, "local_infile").await;
    my_exec(
        &mut tgt_adm,
        &[
            format!("SET GLOBAL max_allowed_packet = {MY_DEFAULT_PACKET}"),
            "SET GLOBAL local_infile = 1".into(),
            format!("CREATE DATABASE `{tgt_db}` DEFAULT CHARACTER SET utf8mb4"),
            format!("CREATE TABLE `{tgt_db}`.`narrow` (`id` INT NOT NULL PRIMARY KEY, `name` VARCHAR(3) NOT NULL) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4"),
        ],
    )
    .await;

    let h = Harness::new().await;
    let (source, target) = mysql_connections(&h, &src, &tgt);
    let req = request(&source, &src_db, &target, &tgt_db, CopySelection::All, options(|o| o.existing = ExistingTablePolicy::Append));
    let (plan, st) = copy_to_end(&h.db, &req).await;
    let item = plan_item(&plan, &my_item("narrow"));
    assert_eq!((item.action, item.exists_in_target, item.target_has_rows), (PlanAction::Append, true, Some(false)));
    assert_eq!(plan.effective.load_method, LoadMethod::MysqlLoadData);
    assert_eq!(st.state, JobState::Failed, "{}", describe(&h.db, &st));
    let narrow = table_result(&st, &my_item("narrow"));
    assert_eq!(narrow.state, CopyItemState::Failed);
    assert!(mentions(narrow, "DATA_WARNINGS"), "{}", describe(&h.db, &st));
    assert!(narrow.data_warnings > 0);
    assert_eq!(narrow.verify, VerifyOutcome::Lossy, "never 'match' with altered values");
    assert_eq!(my_count(&mut tgt_adm, &tgt_db, "narrow").await, 0, "the InnoDB chunk with the warning is rolled back");

    h.db.shutdown().await;
    let restore = if infile_before == "1" || infile_before.eq_ignore_ascii_case("on") { "1" } else { "0" };
    my_exec(&mut tgt_adm, &[format!("SET GLOBAL local_infile = {restore}")]).await;
    let _ = src_adm.disconnect().await;
    let _ = tgt_adm.disconnect().await;
    my_drop_database(&tgt, &tgt_db, false).await;
    my_drop_database(&src, &src_db, true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mysql55_euckr_to_mysql8_utf8mb4() {
    mysql55_copy(MyTarget::Mysql8).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mysql55_euckr_to_mysql9_utf8mb4() {
    mysql55_copy(MyTarget::Mysql9).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mysql55_to_mysql8_blob_over_max_allowed_packet_is_row_too_large() {
    mysql55_row_too_large(MyTarget::Mysql8).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mysql55_to_mysql9_blob_over_max_allowed_packet_is_row_too_large() {
    mysql55_row_too_large(MyTarget::Mysql9).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mysql55_to_mysql8_truncating_value_fails_with_data_warnings() {
    mysql55_data_warnings(MyTarget::Mysql8).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mysql55_to_mysql9_truncating_value_fails_with_data_warnings() {
    mysql55_data_warnings(MyTarget::Mysql9).await;
}

// ---------------------------------------------------------------------------------------------
// MySQL 4.0 (optional real server) -> 8.0
// ---------------------------------------------------------------------------------------------

/// Admin session to the 4.0 server through BulkDB's pre-4.1 client (mysql_async cannot talk
/// to it); used for the fixture only.
async fn legacy_admin(url: &DbUrl) -> LegacyConn {
    let params = ConnectParams {
        driver: "mysql".into(),
        host: url.host.clone(),
        port: url.port,
        database: None,
        user: Some(url.user.clone()),
        password: Some(url.password.clone()).filter(|p| !p.is_empty()),
        fields: BTreeMap::new(),
        application_name: "bulkdb-it40-admin".into(),
        connect_timeout: Duration::from_secs(15),
        ssl: SslMode::Prefer,
    };
    let (stream, greeting) = peek_greeting(&url.host, url.port, params.connect_timeout).await.expect("MySQL 4.0 greeting");
    assert!(greeting.is_pre41(), "{} must point at a pre-4.1 server", url.var);
    let charset = greeting.auth_charset();
    LegacyConn::handshake(stream, greeting, &params, charset).await.expect("legacy login")
}

/// `stamps` on the target: id, the four TIMESTAMP epochs, YEAR, FLOAT as DOUBLE, HEX(name).
type StampRow = (i64, i64, i64, i64, i64, Option<i64>, Option<f64>, Option<String>);

fn meta_text(v: &Option<Vec<u8>>) -> String {
    String::from_utf8_lossy(v.as_deref().unwrap_or_default()).into_owned()
}

/// Pins the 4.0 per-column reads of section 3.2 against real output: `UNIX_TIMESTAMP(c)` for
/// `timestamp(8)` / `(6)` / `(12)` (their text drops the time of day), `c + 0e0` for FLOAT
/// (4.0 prints about 6 digits) and `c + 0` for YEAR(2) (prints '00' for 2000).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mysql40_source_reads_are_pinned_against_a_real_server() {
    let (Some(src), Some(tgt)) = (env_url("BULKDB_MYSQL40_URL", 3306), env_url("BULKDB_MYSQL_URL", 3306)) else { return };
    let _server = MYSQL8_SERVER.lock().await;
    let id = unique();
    let src_db = format!("bulkdb_it40_{}", &id[..16]);
    let tgt_db = format!("bulkdb_it_{id}");
    let mut legacy = legacy_admin(&src).await;
    for sql in [
        format!("DROP DATABASE IF EXISTS `{src_db}`"),
        format!("CREATE DATABASE `{src_db}`"),
        format!(
            "CREATE TABLE `{src_db}`.`stamps` (`id` int(11) NOT NULL auto_increment, `t14` timestamp(14) NOT NULL, `t8` timestamp(8) NOT NULL, \
             `t6` timestamp(6) NOT NULL, `t12` timestamp(12) NOT NULL, `f` float default NULL, `y2` year(2) default NULL, \
             `name` varchar(20) default NULL, PRIMARY KEY (`id`)) TYPE=MyISAM"
        ),
        format!(
            "INSERT INTO `{src_db}`.`stamps` VALUES \
             (1, '2004-03-05 12:34:56', '2004-03-05 12:34:56', '2004-03-05 12:34:56', '2004-03-05 12:34:56', 0.1234567, '2000', 0xC7D1B1DB), \
             (2, '1999-12-31 23:59:58', '2003-07-15 08:09:10', '2002-02-02 02:02:02', '2001-01-01 13:14:15', 1234.5678, '1999', 'abc')"
        ),
    ] {
        legacy.exec(sql.as_bytes()).await.unwrap_or_else(|e| panic!("{sql}: {e:?}"));
    }
    let source = legacy
        .query_meta(
            format!("SELECT id, UNIX_TIMESTAMP(t14), UNIX_TIMESTAMP(t8), UNIX_TIMESTAMP(t6), UNIX_TIMESTAMP(t12), t8 FROM `{src_db}`.`stamps` ORDER BY id").as_bytes(),
            10,
        )
        .await
        .expect("source epochs");
    let src_epochs: Vec<Vec<i64>> = source.rows.iter().map(|r| r[..5].iter().map(|v| meta_text(v).parse().expect("an epoch")).collect()).collect();
    assert_eq!(src_epochs.len(), 2);
    assert_eq!(meta_text(&source.rows[0][5]).len(), 8, "timestamp(8) prints only YYYYMMDD on 4.0, which is why it is read as UNIX_TIMESTAMP");

    let h = Harness::new().await;
    let source_conn = h.save("mysql40", "mysql", &src, &[("protocol", "auto"), ("encoding", "euc-kr")], false);
    let target = h.save("mysql8 target", "mysql", &tgt, &[], true);
    let req = request(&source_conn, &src_db, &target, &tgt_db, CopySelection::All, options(|_| {}));
    let (plan, st) = copy_to_end(&h.db, &req).await;
    assert!(version_num(&plan.source.version).starts_with("4.0."), "{}", plan.source.version);
    assert_done(&h.db, &st);
    assert_loaded_tables_match(&h.db, &st);

    let mut tgt_adm = my_admin(&tgt, None, false).await;
    my_exec(&mut tgt_adm, &["SET time_zone = '+00:00'".into()]).await;
    let rows: Vec<StampRow> = tgt_adm
        .query(format!(
            "SELECT id, CAST(UNIX_TIMESTAMP(t14) AS SIGNED), CAST(UNIX_TIMESTAMP(t8) AS SIGNED), CAST(UNIX_TIMESTAMP(t6) AS SIGNED), \
             CAST(UNIX_TIMESTAMP(t12) AS SIGNED), CAST(y2 AS SIGNED), f + 0e0, HEX(name) FROM `{tgt_db}`.`stamps` ORDER BY id"
        ))
        .await
        .expect("target stamps");
    assert_eq!(rows.len(), 2);
    for (row, src) in rows.iter().zip(&src_epochs) {
        assert_eq!(vec![row.0, row.1, row.2, row.3, row.4], *src, "the full instants of every TIMESTAMP width survive");
    }
    assert_eq!(rows.iter().map(|r| r.5).collect::<Vec<_>>(), [Some(2000), Some(1999)], "YEAR(2) read as c + 0");
    let f = rows[0].6.expect("float value");
    assert!((f - f64::from(0.1234567f32)).abs() < 1e-9, "FLOAT widened (c + 0e0) keeps the stored value: {f}");
    assert_eq!(rows[0].7, some(&hex("한글")));
    assert_eq!(rows[1].7, some(&hex("abc")));

    h.db.shutdown().await;
    legacy.exec(format!("DROP DATABASE IF EXISTS `{src_db}`").as_bytes()).await.expect("drop the 4.0 fixture");
    legacy.quit().await;
    let _ = tgt_adm.disconnect().await;
    my_drop_database(&tgt, &tgt_db, false).await;
}

// ---------------------------------------------------------------------------------------------
// PostgreSQL helpers
// ---------------------------------------------------------------------------------------------

async fn pg_client(url: &DbUrl, db: &str) -> tokio_postgres::Client {
    let mut cfg = tokio_postgres::Config::new();
    cfg.host(&url.host).port(url.port).user(&url.user).dbname(db).application_name("bulkdb-it-admin");
    if !url.password.is_empty() {
        cfg.password(&url.password);
    }
    let (client, conn) = cfg.connect(tokio_postgres::NoTls).await.unwrap_or_else(|e| panic!("connect to {} database {db}: {e}", url.var));
    tokio::spawn(async move {
        let _ = conn.await;
    });
    client
}

async fn pg_create_database(url: &DbUrl, db: &str) {
    let admin = pg_client(url, &url.database).await;
    // Two round trips: a multi-statement query string is one implicit transaction block, which
    // DROP / CREATE DATABASE refuse.
    admin.batch_execute(&format!("DROP DATABASE IF EXISTS {db} WITH (FORCE)")).await.unwrap_or_else(|e| panic!("drop database {db}: {e}"));
    admin.batch_execute(&format!("CREATE DATABASE {db}")).await.unwrap_or_else(|e| panic!("create database {db}: {e}"));
}

async fn pg_drop_database(url: &DbUrl, db: &str) {
    let admin = pg_client(url, &url.database).await;
    admin.batch_execute(&format!("DROP DATABASE IF EXISTS {db} WITH (FORCE)")).await.unwrap_or_else(|e| panic!("drop database {db}: {e}"));
}

/// One text value (`NULL` -> `None`).
async fn pg_text(c: &tokio_postgres::Client, sql: &str) -> Option<String> {
    c.query_one(&format!("SELECT ({sql})::text"), &[]).await.unwrap_or_else(|e| panic!("{sql}: {e}")).get(0)
}

/// The PG15 source database: every object kind the copy must carry, Korean text everywhere,
/// and `app.big` with `big_rows` rows.
async fn pg_source(url: &DbUrl, db: &str, big_rows: i64) {
    pg_create_database(url, db).await;
    let c = pg_client(url, db).await;
    let sql = format!(
        r#"
CREATE EXTENSION IF NOT EXISTS citext;
CREATE SCHEMA app;
CREATE TYPE app.mood AS ENUM ('행복', '보통', '슬픔');
CREATE DOMAIN app.posint AS integer CHECK (VALUE > 0);
CREATE SEQUENCE app.ticket_seq START 100 INCREMENT 7;
CREATE TABLE app.customers (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    name text NOT NULL,
    email citext UNIQUE,
    mood app.mood NOT NULL DEFAULT '보통',
    score app.posint
);
COMMENT ON TABLE app.customers IS '고객 테이블';
COMMENT ON COLUMN app.customers.name IS '이름';
INSERT INTO app.customers (name, email, mood, score) VALUES
    ('홍길동', 'Hong@Example.com', '행복', 10), ('김철수', 'kim@example.com', '슬픔', 20), ('이영희', NULL, '보통', 2000000);
-- A NOT VALID domain check over a violating legacy row (2000000).
ALTER DOMAIN app.posint ADD CONSTRAINT posint_small CHECK (VALUE < 1000000) NOT VALID;
-- Used by an expression index and has no search_path: PG17_SEARCH_PATH on 17+ targets.
CREATE FUNCTION app.norm_name(t text) RETURNS text LANGUAGE sql IMMUTABLE AS $$ SELECT lower(t) $$;
CREATE INDEX customers_norm_idx ON app.customers (app.norm_name(name));
CREATE TABLE app.orders (
    id serial PRIMARY KEY,
    customer_id bigint NOT NULL REFERENCES app.customers (id) DEFERRABLE INITIALLY DEFERRED,
    amount numeric(12,2) NOT NULL,
    note text
);
INSERT INTO app.orders (customer_id, amount, note) SELECT 1 + i % 3, i * 1.25, '주문 ' || i FROM generate_series(1, 500) AS i;
INSERT INTO app.orders (customer_id, amount, note) VALUES (1, -5.00, '환불');
-- A NOT VALID check over a violating row: created after the load on the target.
ALTER TABLE app.orders ADD CONSTRAINT amount_nonneg CHECK (amount >= 0) NOT VALID;
-- Must not fire during the load (triggers come last); the second one stays disabled.
CREATE FUNCTION app.mark_note() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN NEW.note := 'fired'; RETURN NEW; END $$;
CREATE TRIGGER orders_mark BEFORE INSERT ON app.orders FOR EACH ROW EXECUTE FUNCTION app.mark_note();
CREATE TRIGGER orders_mark_disabled BEFORE UPDATE ON app.orders FOR EACH ROW EXECUTE FUNCTION app.mark_note();
ALTER TABLE app.orders DISABLE TRIGGER orders_mark_disabled;
CREATE TABLE app.tickets (id serial PRIMARY KEY, label text NOT NULL);
INSERT INTO app.tickets (label) SELECT '표 ' || i FROM generate_series(1, 50) AS i;
CREATE TABLE app.events (id int NOT NULL, at date NOT NULL, payload text) PARTITION BY RANGE (at);
CREATE TABLE app.events_2024 PARTITION OF app.events FOR VALUES FROM ('2024-01-01') TO ('2025-01-01');
CREATE TABLE app.events_2025 PARTITION OF app.events FOR VALUES FROM ('2025-01-01') TO ('2026-01-01');
INSERT INTO app.events SELECT i, DATE '2024-06-01' + i % 400, '이벤트 ' || i FROM generate_series(1, 1000) AS i;
CREATE TABLE app.animals (id int PRIMARY KEY, name text NOT NULL);
CREATE TABLE app.dogs (breed text) INHERITS (app.animals);
INSERT INTO app.animals VALUES (1, '고양이'), (2, '토끼');
INSERT INTO app.dogs (id, name, breed) VALUES (10, '바둑이', '진돗개');
CREATE VIEW app.v_customers WITH (security_invoker = true) AS SELECT id, name, mood FROM app.customers;
CREATE MATERIALIZED VIEW app.mv_order_totals AS SELECT customer_id, count(*) AS n, sum(amount) AS total FROM app.orders GROUP BY customer_id;
CREATE TABLE app.secrets (id int PRIMARY KEY, owner_name text NOT NULL, secret text);
ALTER TABLE app.secrets ENABLE ROW LEVEL SECURITY;
CREATE POLICY secrets_own ON app.secrets USING (owner_name = current_user);
INSERT INTO app.secrets VALUES (1, 'nobody', '비밀'), (2, 'somebody', '공개');
CREATE TABLE app.big (id bigint PRIMARY KEY, k int NOT NULL, txt text, created timestamptz NOT NULL);
INSERT INTO app.big
    SELECT i, i % 1000, '대용량 데이터 ' || i || ' ' || repeat('가', 20), TIMESTAMPTZ '2024-01-01 00:00:00+00' + i * INTERVAL '1 second'
    FROM generate_series(1, {big_rows}) AS i;
CREATE INDEX big_k_idx ON app.big (k);
SELECT setval('app.ticket_seq', 4242);
ANALYZE;
"#
    );
    c.batch_execute(&sql).await.unwrap_or_else(|e| panic!("PG source fixture: {e}"));
}

/// Saved connections: the PG source (read) and the write-allowed target.
fn pg_connections(h: &Harness, src: &DbUrl, tgt: &DbUrl) -> (String, String) {
    (h.save("pg15 source", "postgres", src, &[], false), h.save("pg target", "postgres", tgt, &[], true))
}

/// `last_value:is_called` of the sequence behind `table.column` (identity or serial).
async fn pg_seq(c: &tokio_postgres::Client, table: &str, column: &str) -> String {
    let seq = pg_text(c, &format!("pg_get_serial_sequence('{table}', '{column}')")).await.unwrap_or_else(|| panic!("no sequence behind {table}.{column}"));
    pg_text(c, &format!("SELECT last_value || ':' || is_called FROM {seq}")).await.expect("sequence state")
}

/// Compare the copied database with the source: counts, Korean values, sequences and every
/// object kind of the fixture.
async fn check_pg_copy(src: &tokio_postgres::Client, tgt: &tokio_postgres::Client) {
    // (label, query, expected target value; None = only "equal to the source and not NULL")
    let checks: &[(&str, &str, Option<&str>)] = &[
        ("customers", "SELECT string_agg(id || ':' || name || ':' || coalesce(email::text, '-') || ':' || mood || ':' || coalesce(score::text, '-'), ',' ORDER BY id) FROM app.customers", Some("1:홍길동:Hong@Example.com:행복:10,2:김철수:kim@example.com:슬픔:20,3:이영희:-:보통:2000000")),
        ("enum labels", "SELECT string_agg(enumlabel, ',' ORDER BY enumsortorder) FROM pg_enum WHERE enumtypid = 'app.mood'::regtype", Some("행복,보통,슬픔")),
        ("orders", "SELECT count(*) || ':' || md5(string_agg(id || '|' || customer_id || '|' || amount || '|' || coalesce(note, ''), ',' ORDER BY id)) FROM app.orders", None),
        ("refund row", "SELECT note || ':' || amount FROM app.orders WHERE amount < 0", Some("환불:-5.00")),
        ("tickets", "SELECT count(*) || ':' || string_agg(label, ',' ORDER BY id) FROM app.tickets", None),
        ("events", "SELECT count(*) || ':' || sum(id) || ':' || md5(string_agg(payload || at, ',' ORDER BY id)) FROM app.events", None),
        ("event partitions", "SELECT string_agg(c.relname, ',' ORDER BY c.relname) FROM pg_inherits i JOIN pg_class c ON c.oid = i.inhrelid WHERE i.inhparent = 'app.events'::regclass", Some("events_2024,events_2025")),
        ("animals only", "SELECT string_agg(id || ':' || name, ',' ORDER BY id) FROM ONLY app.animals", Some("1:고양이,2:토끼")),
        ("dogs", "SELECT string_agg(id || ':' || name || ':' || breed, ',' ORDER BY id) FROM app.dogs", Some("10:바둑이:진돗개")),
        ("inheritance", "SELECT count(*) FROM pg_inherits WHERE inhrelid = 'app.dogs'::regclass AND inhparent = 'app.animals'::regclass", Some("1")),
        ("secrets", "SELECT string_agg(id || ':' || owner_name || ':' || secret, ',' ORDER BY id) FROM app.secrets", Some("1:nobody:비밀,2:somebody:공개")),
        ("rls", "SELECT relrowsecurity || ':' || (SELECT string_agg(polname, ',') FROM pg_policy WHERE polrelid = 'app.secrets'::regclass) FROM pg_class WHERE oid = 'app.secrets'::regclass", Some("true:secrets_own")),
        ("big", "SELECT count(*) || ':' || md5(string_agg(id || '|' || k || '|' || txt || '|' || extract(epoch FROM created)::bigint, ',' ORDER BY id)) FROM app.big", None),
        ("big sample", "SELECT txt || ':' || char_length(txt) FROM app.big WHERE id = 1234", Some("대용량 데이터 1234 가가가가가가가가가가가가가가가가가가가가:33")),
        ("view", "SELECT count(*) || ':' || array_to_string((SELECT reloptions FROM pg_class WHERE oid = 'app.v_customers'::regclass), ',') FROM app.v_customers", Some("3:security_invoker=true")),
        ("matview", "SELECT string_agg(customer_id || ':' || n || ':' || total, ',' ORDER BY customer_id) FROM app.mv_order_totals", None),
        ("functions", "SELECT string_agg(proname, ',' ORDER BY proname) FROM pg_proc WHERE pronamespace = 'app'::regnamespace", Some("mark_note,norm_name")),
        ("triggers", "SELECT string_agg(tgname || '=' || tgenabled, ',' ORDER BY tgname) FROM pg_trigger WHERE tgrelid = 'app.orders'::regclass AND NOT tgisinternal", Some("orders_mark=O,orders_mark_disabled=D")),
        ("trigger did not fire during the load", "SELECT count(*) FROM app.orders WHERE note = 'fired'", Some("0")),
        ("deferrable fk", "SELECT string_agg(condeferrable || '/' || condeferred, ',') FROM pg_constraint WHERE conrelid = 'app.orders'::regclass AND contype = 'f'", Some("true/true")),
        ("not valid check", "SELECT convalidated FROM pg_constraint WHERE conrelid = 'app.orders'::regclass AND conname = 'amount_nonneg'", Some("false")),
        ("domain checks", "SELECT string_agg(pg_get_constraintdef(oid), ',' ORDER BY convalidated DESC) FROM pg_constraint WHERE contypid = 'app.posint'::regtype", None),
        ("domain check validity", "SELECT string_agg(convalidated::text, ',' ORDER BY convalidated DESC) FROM pg_constraint WHERE contypid = 'app.posint'::regtype", Some("true,false")),
        ("comments", "SELECT obj_description('app.customers'::regclass, 'pg_class') || '/' || col_description('app.customers'::regclass, 2)", Some("고객 테이블/이름")),
        ("citext", "SELECT (SELECT extname FROM pg_extension WHERE extname = 'citext') || ':' || (SELECT count(*) FROM app.customers WHERE email = 'hong@example.com')", Some("citext:1")),
        ("indexes", "SELECT string_agg(indexname, ',' ORDER BY indexname) FROM pg_indexes WHERE schemaname = 'app' AND indexname IN ('big_k_idx', 'customers_norm_idx')", Some("big_k_idx,customers_norm_idx")),
        ("standalone sequence", "SELECT last_value || ':' || is_called FROM app.ticket_seq", Some("4242:true")),
    ];
    for (label, sql, expected) in checks {
        let want = pg_text(src, sql).await;
        let got = pg_text(tgt, sql).await;
        assert!(got.is_some(), "{label}: missing on the target ({sql})");
        assert_eq!(got, want, "{label}: target differs from the source");
        if let Some(e) = expected {
            assert_eq!(got.as_deref(), Some(*e), "{label}");
        }
    }
    for (table, column, expected) in [("app.customers", "id", "3:true"), ("app.orders", "id", "501:true"), ("app.tickets", "id", "50:true")] {
        let want = pg_seq(src, table, column).await;
        assert_eq!(want, expected, "source fixture sequence of {table}");
        assert_eq!(pg_seq(tgt, table, column).await, want, "sequence of {table}.{column} carries the source's value");
    }
}

/// PG15 -> another PostgreSQL server: everything through a forced cancel and a resume, then
/// the whole database compared with the source.
async fn pg15_copy_to(target_var: &'static str, target_major: u32) {
    let (Some(src), Some(tgt)) = (env_url("BULKDB_PG_URL", 5432), env_url(target_var, 5432)) else { return };
    let id = unique();
    let (src_db, tgt_db) = (format!("bulkdb_it_src_{id}"), format!("bulkdb_it_{id}"));
    pg_source(&src, &src_db, PG_BIG_ROWS).await;
    pg_create_database(&tgt, &tgt_db).await;

    let h = Harness::new().await;
    let (source, target) = pg_connections(&h, &src, &tgt);
    let req = request(&source, &src_db, &target, &tgt_db, CopySelection::All, options(|o| o.verify = VerifyMode::Checksum));
    let mut lock = Blocker::Pg { client: pg_client(&tgt, &tgt_db).await, table: "app.big".into() };
    let (plan, st) = copy_with_forced_cancel(&h.db, &req, &mut lock).await;
    assert_eq!(plan.family, CopyFamily::Postgres);
    assert_eq!(plan.effective.load_method, LoadMethod::PgCopyCsv);
    assert!(version_num(&plan.source.version).starts_with("15"), "{}", plan.source.version);
    assert!(version_num(&plan.target.version).starts_with(&target_major.to_string()), "{}", plan.target.version);
    assert_eq!(plan_item(&plan, &pg_item("app", "big")).action, PlanAction::Create);
    assert!(plan.items.iter().filter(|i| i.kind == CopyObjectKind::Table).all(|i| i.action == PlanAction::Create), "an empty target database creates every table");
    let codes = issue_codes(&plan);
    assert_eq!(codes.contains("PG17_SEARCH_PATH"), target_major >= 17, "PG17_SEARCH_PATH exactly on 17+ targets: {codes:?}");
    assert!(!codes.contains("SRC_RLS"), "a superuser reads RLS tables completely");
    assert_loaded_tables_match(&h.db, &st);

    let (src_c, tgt_c) = (pg_client(&src, &src_db).await, pg_client(&tgt, &tgt_db).await);
    check_pg_copy(&src_c, &tgt_c).await;

    h.db.shutdown().await;
    drop((src_c, tgt_c, lock));
    pg_drop_database(&tgt, &tgt_db).await;
    pg_drop_database(&src, &src_db).await;
}

// ---------------------------------------------------------------------------------------------
// PostgreSQL 15 -> 16 / 17 / 15
// ---------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pg15_to_pg16_full_database_with_cancel_and_resume() {
    pg15_copy_to("BULKDB_PG16_URL", 16).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pg15_to_pg17_full_database_with_cancel_and_resume() {
    pg15_copy_to("BULKDB_PG17_URL", 17).await;
}

/// PG15 into a second database of the same server, plus: an RLS table read by a non-owner is
/// `SRC_RLS`; a dry run with the truncate policy changes nothing (rows, sequence); an append
/// into a table whose sequence is ahead leaves the sequence ahead.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pg15_to_pg15_second_database_dry_run_append_and_rls() {
    let Some(pg) = env_url("BULKDB_PG_URL", 5432) else { return };
    let id = unique();
    let (src_db, tgt_db) = (format!("bulkdb_it_src_{id}"), format!("bulkdb_it_{id}"));
    let reader = format!("bulkdb_it_r_{}", &id[..12]);
    pg_source(&pg, &src_db, 20_000).await;
    pg_create_database(&pg, &tgt_db).await;
    let admin = pg_client(&pg, &pg.database).await;
    admin.batch_execute(&format!("DROP ROLE IF EXISTS {reader}; CREATE ROLE {reader} LOGIN PASSWORD 'reader'")).await.expect("create reader role");
    let src_c = pg_client(&pg, &src_db).await;
    src_c
        .batch_execute(&format!(
            "GRANT USAGE ON SCHEMA app TO {reader}; GRANT SELECT ON ALL TABLES IN SCHEMA app TO {reader}; GRANT SELECT ON ALL SEQUENCES IN SCHEMA app TO {reader}"
        ))
        .await
        .expect("grant the reader");

    let h = Harness::new().await;
    let (source, target) = pg_connections(&h, &pg, &pg);

    // 1. A non-owner without BYPASSRLS would read only the rows the policies allow.
    let reader_url = DbUrl { user: reader.clone(), password: "reader".into(), ..pg.clone() };
    let reader_conn = h.save("pg15 reader", "postgres", &reader_url, &[], false);
    let req = request(&reader_conn, &src_db, &target, &tgt_db, CopySelection::All, options(|_| {}));
    let plan = h.db.copy_plan(&req, None).await.unwrap_or_else(|e| panic!("copy_plan as the reader: {e:?}"));
    let rls = plan.blockers.iter().find(|b| b.code == "SRC_RLS").unwrap_or_else(|| panic!("SRC_RLS expected:{}", issues_text(&plan.blockers)));
    let secrets = pg_item("app", "secrets");
    let names_secrets = |t: &str| t.contains("secrets");
    assert!(
        rls.item_id.as_deref() == Some(secrets.as_str()) || names_secrets(&rls.message) || rls.detail.as_deref().is_some_and(names_secrets),
        "SRC_RLS names the RLS table: {rls:?}"
    );

    // 2. The whole database, through a forced cancel and a resume.
    let req = request(&source, &src_db, &target, &tgt_db, CopySelection::All, options(|o| o.verify = VerifyMode::Checksum));
    let mut lock = Blocker::Pg { client: pg_client(&pg, &tgt_db).await, table: "app.big".into() };
    let (plan, st) = copy_with_forced_cancel(&h.db, &req, &mut lock).await;
    assert!(!issue_codes(&plan).contains("SAME_DATABASE"));
    assert_loaded_tables_match(&h.db, &st);
    let tgt_c = pg_client(&pg, &tgt_db).await;
    check_pg_copy(&src_c, &tgt_c).await;

    // 3. Dry run, truncate policy, into the existing serial table: nothing changes.
    tgt_c.batch_execute("SELECT setval(pg_get_serial_sequence('app.tickets', 'id'), 777, true)").await.expect("setval");
    let tickets = select_tables(Some("app"), &["tickets"]);
    let req = request(
        &source,
        &src_db,
        &target,
        &tgt_db,
        tickets.clone(),
        options(|o| {
            o.existing = ExistingTablePolicy::Truncate;
            o.dry_run = true;
        }),
    );
    let (plan, st) = copy_to_end(&h.db, &req).await;
    let item = plan_item(&plan, &pg_item("app", "tickets"));
    assert_eq!((item.action, item.target_has_rows), (PlanAction::Truncate, Some(true)));
    assert!(issue_codes(&plan).contains("DESTRUCTIVE_TRUNCATE"), "{:?}", issue_codes(&plan));
    assert_done(&h.db, &st);
    let result = st.result.as_ref().expect("dry-run result");
    assert!(result.dry_run && !result.resumable, "a dry run says so and is never resumable");
    assert!(result.tables.iter().all(|t| t.rows_written == 0), "a dry run writes no rows");
    assert_eq!(pg_seq(&tgt_c, "app.tickets", "id").await, "777:true", "a dry run never runs setval");
    assert_eq!(pg_text(&tgt_c, "SELECT count(*) FROM app.tickets").await.as_deref(), Some("50"));

    // 4. Append into the emptied table whose sequence is ahead: the sequence never moves back.
    tgt_c.batch_execute("TRUNCATE app.tickets; SELECT setval(pg_get_serial_sequence('app.tickets', 'id'), 100000, true)").await.expect("prepare append");
    let req = request(&source, &src_db, &target, &tgt_db, tickets, options(|o| o.existing = ExistingTablePolicy::Append));
    let (plan, st) = copy_to_end(&h.db, &req).await;
    assert_eq!(plan_item(&plan, &pg_item("app", "tickets")).action, PlanAction::Append);
    assert_done(&h.db, &st);
    assert_eq!(table_result(&st, &pg_item("app", "tickets")).rows_written, 50);
    assert_eq!(pg_text(&tgt_c, "SELECT count(*) || ':' || max(id) FROM app.tickets").await.as_deref(), Some("50:50"));
    assert_eq!(pg_seq(&tgt_c, "app.tickets", "id").await, "100000:true", "an append leaves a sequence that is ahead where it is");

    h.db.shutdown().await;
    drop((src_c, tgt_c, lock));
    pg_drop_database(&pg, &tgt_db).await;
    pg_drop_database(&pg, &src_db).await;
    admin.batch_execute(&format!("DROP ROLE IF EXISTS {reader}")).await.expect("drop reader role");
}
