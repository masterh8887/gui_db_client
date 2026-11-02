//! MySQL / MariaDB integration tests. They need a live server: set `BULKDB_MYSQL_URL` to
//! `mysql://user:password@host:port/database` (the account must be able to create tables in
//! that database, read `information_schema.processlist` and `KILL QUERY` its own sessions) and
//! run `cargo test -p bulk-core --features it --test mysql_it`. Without the variable every test
//! prints a skip note and passes, so the file always compiles and never fails a CI box without
//! docker.
//!
//! Fixture tables are prefixed `bulkdb_it_` and recreated by every test, so the tests can run
//! in parallel against one database.
//!
//! `BULKDB_MYSQL55_URL` (e.g. `mysql://root:bulkdb@127.0.0.1:3307/bulkdb`) enables the legacy
//! suite at the end of this file: a `mysql:5.5` server started with
//! `--character-set-server=euckr --collation-server=euckr_korean_ci --old-passwords=1
//! --secure-auth=0` (see `docker/compose.yml` and the CI workflow). It covers old-password
//! accounts, EUC-KR data (also CP949 stored in latin1 columns), tabular EXPLAIN, `KILL QUERY`
//! versus `KILL`, and the `SHOW`-based catalog that MySQL 4.x gets.
#![cfg(feature = "it")]

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use arrow_array::{Array, BinaryArray, Date32Array, Decimal128Array, Int64Array, StringArray, TimestampMicrosecondArray};
use arrow_schema::{DataType, TimeUnit};
use bulk_core::catalog::mysql::generic;
use bulk_core::catalog::Catalog;
use bulk_core::encoding::Charset;
use bulk_core::monitor::{JobHandle, Monitor};
use bulk_core::source::mysql::{Flavor, MySqlDatabase};
use bulk_core::source::mysql_common::MysqlRawQuery;
use bulk_core::source::mysql_dispatch::TextPlan;
use bulk_core::source::{open, Batch, ConnectParams, Query, Source, SslMode, StreamOutput};
use bulk_core::testdata::{self, Lcg};
use bulk_core::types::{SessionOpts, TableKind, TableRef};
use bulk_core::{CancellationToken, CoreError, BATCH_ROWS};
use futures::StreamExt;
use mysql_async::prelude::Queryable;
use mysql_async::{Params, Value};

// ---------------------------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------------------------

struct Target {
    url: String,
    host: String,
    port: u16,
    user: Option<String>,
    password: Option<String>,
    database: String,
}

/// `mysql://user:password@host:port/database[?...]` from `BULKDB_MYSQL_URL`, or `None`.
fn target() -> Option<Target> {
    let url = std::env::var("BULKDB_MYSQL_URL").ok()?.trim().to_string();
    if url.is_empty() {
        return None;
    }
    Some(parse_target(&url))
}

/// Parse `mysql://user:password@host:port/database[?...]`.
fn parse_target(url: &str) -> Target {
    let url = url.to_string();
    let rest = url.strip_prefix("mysql://").or_else(|| url.strip_prefix("mariadb://")).unwrap_or(&url);
    let (creds, rest) = match rest.rfind('@') {
        Some(at) => (Some(&rest[..at]), &rest[at + 1..]),
        None => (None, rest),
    };
    let (user, password) = match creds {
        Some(c) => match c.split_once(':') {
            Some((u, p)) => (Some(u.to_string()), Some(p.to_string())),
            None => (Some(c.to_string()), None),
        },
        None => (None, None),
    };
    let (hostport, path) = rest.split_once('/').unwrap_or((rest, ""));
    let (host, port) = match hostport.rsplit_once(':') {
        Some((h, p)) => (h.to_string(), p.parse().unwrap_or(3306)),
        None => (hostport.to_string(), 3306),
    };
    let database = path.split('?').next().unwrap_or("").to_string();
    assert!(!database.is_empty(), "the MySQL test URL must name a database: mysql://user:pass@host:port/database");
    Target { url, host, port, user, password, database }
}

macro_rules! require_server {
    () => {
        match target() {
            Some(t) => t,
            None => {
                eprintln!("skipping: BULKDB_MYSQL_URL is not set");
                return;
            }
        }
    };
}

fn params(t: &Target, application_name: &str) -> ConnectParams {
    ConnectParams {
        driver: "mysql".into(),
        host: t.host.clone(),
        port: t.port,
        database: Some(t.database.clone()),
        user: t.user.clone(),
        password: t.password.clone(),
        fields: BTreeMap::new(),
        application_name: application_name.into(),
        connect_timeout: Duration::from_secs(10),
        ssl: SslMode::Prefer,
    }
}

async fn connect(t: &Target, application_name: &str) -> MySqlDatabase {
    MySqlDatabase::connect(&params(t, application_name)).await.expect("connect")
}

/// Side connection for fixtures and processlist checks (plain mysql_async, not the driver).
async fn admin(t: &Target) -> mysql_async::Conn {
    let opts = mysql_async::Opts::from_url(&t.url).expect("BULKDB_MYSQL_URL is a valid mysql:// URL");
    mysql_async::Conn::new(opts).await.expect("admin connection")
}

fn query(sql: &str) -> Query {
    Query { sql: sql.to_string(), output: StreamOutput::Arrow, max_batch_bytes: None }
}

fn table(t: &Target, name: &str) -> TableRef {
    TableRef { database: Some(t.database.clone()), schema: Some(t.database.clone()), name: name.to_string() }
}

/// `testdata::nasty_row` table: id, text, num DECIMAL(12,3), note, flag, unicode.
async fn create_nasty(conn: &mut mysql_async::Conn, name: &str, rows: u64) {
    conn.query_drop(format!("DROP TABLE IF EXISTS `{name}`")).await.unwrap();
    conn.query_drop(format!(
        "CREATE TABLE `{name}` (\
            id INT NOT NULL PRIMARY KEY, \
            text TEXT NULL, \
            num DECIMAL(12,3) NULL, \
            note VARCHAR(100) NULL, \
            flag VARCHAR(5) NOT NULL, \
            unicode VARCHAR(100) NULL, \
            INDEX ix_note (note)\
         ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COMMENT='bulkdb integration fixture'"
    ))
    .await
    .unwrap();
    const CHUNK: u64 = 500;
    let mut rng = Lcg::new(42);
    let mut i = 0u64;
    while i < rows {
        let n = CHUNK.min(rows - i);
        let tuples = std::iter::repeat_n("(?,?,?,?,?,?)", n as usize).collect::<Vec<_>>().join(",");
        let mut values: Vec<Value> = Vec::with_capacity(n as usize * 6);
        for _ in 0..n {
            let row = testdata::nasty_row(i, &mut rng);
            values.push(Value::from(i as i64));
            for v in row.into_iter().skip(1) {
                values.push(Value::from(v));
            }
            i += 1;
        }
        conn.exec_drop(format!("INSERT INTO `{name}` (id, text, num, note, flag, unicode) VALUES {tuples}"), Params::Positional(values)).await.unwrap();
    }
    conn.query_drop(format!("ANALYZE TABLE `{name}`")).await.unwrap();
}

/// Drain a stream, counting rows and checking the batch cap. Returns the batches and the
/// terminal error, if any.
async fn drain(db: &MySqlDatabase, sql: &str, cancel: CancellationToken) -> (Vec<arrow_array::RecordBatch>, u64, Option<CoreError>) {
    let out = db.stream(&query(sql), &SessionOpts::default(), cancel).await.unwrap();
    let mut stream = out.stream;
    let mut batches = Vec::new();
    let mut rows = 0u64;
    while let Some(item) = stream.next().await {
        match item {
            Ok(Batch::Arrow(b)) => {
                assert!(b.num_rows() <= BATCH_ROWS, "batch of {} rows exceeds the cap", b.num_rows());
                rows += b.num_rows() as u64;
                batches.push(b);
            }
            Ok(other) => panic!("mysql must produce Arrow batches, got {other:?}"),
            Err(e) => return (batches, rows, Some(e)),
        }
    }
    (batches, rows, None)
}

/// `processlist.command` of connection `id`, or `None` when the connection is gone.
async fn command_of(conn: &mut mysql_async::Conn, id: u32) -> Option<String> {
    let row: Option<(Option<String>, Option<String>)> = conn.exec_first("SELECT command, info FROM information_schema.processlist WHERE id = ?", (id,)).await.unwrap();
    row.map(|(cmd, _)| cmd.unwrap_or_default())
}

fn session_id_of(db: &MySqlDatabase) -> u32 {
    db.session_id().expect("mysql reports its connection id").parse().unwrap()
}

// ---------------------------------------------------------------------------------------------
// Streaming
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn streams_100k_nasty_rows_in_order() {
    let t = require_server!();
    let mut adm = admin(&t).await;
    create_nasty(&mut adm, "bulkdb_it_stream", 100_000).await;

    let db = connect(&t, "bulkdb-it-stream").await;
    db.apply_session(&SessionOpts { statement_timeout_ms: Some(600_000), ..Default::default() }).await.unwrap();
    let started = Instant::now();
    let (batches, rows, err) = drain(&db, "SELECT id, text, num, note, flag, unicode FROM bulkdb_it_stream ORDER BY id", CancellationToken::new()).await;
    assert!(err.is_none(), "stream failed: {err:?}");
    assert_eq!(rows, 100_000);
    assert!(batches.len() >= 2, "100k rows need at least two batches at {BATCH_ROWS} rows");
    eprintln!("100k nasty rows streamed in {:?} ({} batches)", started.elapsed(), batches.len());

    let schema = batches[0].schema();
    assert_eq!(schema.field(0).data_type(), &DataType::Int64);
    assert_eq!(schema.field(1).data_type(), &DataType::Utf8);
    assert_eq!(schema.field(2).data_type(), &DataType::Decimal128(12, 3));
    assert_eq!(schema.field(5).data_type(), &DataType::Utf8);

    // Every value round-trips: regenerate the deterministic rows and compare.
    let mut rng = Lcg::new(42);
    let mut i = 0u64;
    for b in &batches {
        let ids = b.column(0).as_any().downcast_ref::<Int64Array>().unwrap();
        let texts = b.column(1).as_any().downcast_ref::<StringArray>().unwrap();
        let nums = b.column(2).as_any().downcast_ref::<Decimal128Array>().unwrap();
        let unicode = b.column(5).as_any().downcast_ref::<StringArray>().unwrap();
        for r in 0..b.num_rows() {
            let expected = testdata::nasty_row(i, &mut rng);
            assert_eq!(ids.value(r), i as i64, "rows arrive in id order");
            let got_text = if texts.is_null(r) { None } else { Some(texts.value(r).to_string()) };
            assert_eq!(got_text, expected[1], "text of row {i}");
            let got_num = if nums.is_null(r) { None } else { Some(nums.value(r)) };
            let want_num = expected[2].as_ref().map(|s| s.replace('.', "").parse::<i128>().unwrap());
            assert_eq!(got_num, want_num, "num of row {i}");
            let got_unicode = if unicode.is_null(r) { None } else { Some(unicode.value(r).to_string()) };
            assert_eq!(got_unicode, expected[5], "unicode of row {i}");
            i += 1;
        }
    }
    assert_eq!(i, 100_000);
    adm.query_drop("DROP TABLE IF EXISTS `bulkdb_it_stream`").await.unwrap();
}

#[tokio::test]
async fn maps_every_column_type() {
    let t = require_server!();
    let mut adm = admin(&t).await;
    adm.query_drop("SET SESSION sql_mode = ''").await.unwrap();
    adm.query_drop("DROP TABLE IF EXISTS `bulkdb_it_types`").await.unwrap();
    adm.query_drop(
        "CREATE TABLE `bulkdb_it_types` (\
            id INT NOT NULL PRIMARY KEY, \
            c_tiny TINYINT, c_small SMALLINT, c_medium MEDIUMINT, c_int INT, c_big BIGINT, c_ubig BIGINT UNSIGNED, c_year YEAR, \
            c_float FLOAT, c_double DOUBLE, c_dec DECIMAL(10,2), c_wide DECIMAL(65,30), \
            c_date DATE, c_datetime DATETIME(6), c_ts TIMESTAMP NULL, c_time TIME(3), c_bit BIT(8), \
            c_char CHAR(5), c_varchar VARCHAR(20), c_text TEXT, \
            c_binary BINARY(3), c_varbinary VARBINARY(10), c_blob BLOB, \
            c_json JSON, c_enum ENUM('a','b'), c_set SET('x','y')\
         ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4",
    )
    .await
    .unwrap();
    adm.query_drop(
        "INSERT INTO `bulkdb_it_types` VALUES \
         (1, -1, -2, -3, 42, -9223372036854775808, 18446744073709551615, 2024, \
          1.5, 2.25, 123.45, 1.5, \
          '2024-02-29', '2024-02-29 12:34:56.123456', '2024-02-29 12:34:56', '-10:20:30.500', b'101', \
          'ab', 'hello', 'text', 'ab', X'00FF', X'DEADBEEF', \
          '{\"a\": 1}', 'b', 'x,y'), \
         (2, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, \
          NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL), \
         (3, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, '0000-00-00', '0000-00-00 00:00:00', NULL, '00:00:00', b'0', \
          '', '', '', '', '', '', NULL, 'a', '')",
    )
    .await
    .unwrap();

    let db = connect(&t, "bulkdb-it-types").await;
    let (batches, rows, err) = drain(&db, "SELECT * FROM bulkdb_it_types ORDER BY id", CancellationToken::new()).await;
    assert!(err.is_none(), "stream failed: {err:?}");
    assert_eq!(rows, 3);
    let b = &batches[0];
    let schema = b.schema();
    let ty = |name: &str| schema.field_with_name(name).unwrap().data_type().clone();
    for int in ["c_tiny", "c_small", "c_medium", "c_int", "c_big", "c_year", "c_bit"] {
        assert_eq!(ty(int), DataType::Int64, "{int}");
    }
    assert_eq!(ty("c_ubig"), DataType::Utf8, "BIGINT UNSIGNED does not fit an i64");
    assert_eq!(ty("c_float"), DataType::Float64);
    assert_eq!(ty("c_double"), DataType::Float64);
    assert_eq!(ty("c_dec"), DataType::Decimal128(10, 2));
    assert_eq!(ty("c_wide"), DataType::Utf8, "DECIMAL(65,30) is wider than Decimal128");
    assert_eq!(ty("c_date"), DataType::Date32);
    assert_eq!(ty("c_datetime"), DataType::Timestamp(TimeUnit::Microsecond, None));
    assert_eq!(ty("c_ts"), DataType::Timestamp(TimeUnit::Microsecond, None));
    assert_eq!(ty("c_time"), DataType::Utf8);
    for text in ["c_char", "c_varchar", "c_text", "c_json", "c_enum", "c_set"] {
        assert_eq!(ty(text), DataType::Utf8, "{text}");
    }
    for bin in ["c_binary", "c_varbinary", "c_blob"] {
        assert_eq!(ty(bin), DataType::Binary, "{bin}");
    }

    let col = |name: &str| b.column(schema.index_of(name).unwrap()).clone();
    let i64s = |name: &str| col(name).as_any().downcast_ref::<Int64Array>().unwrap().clone();
    let strs = |name: &str| col(name).as_any().downcast_ref::<StringArray>().unwrap().clone();
    assert_eq!(i64s("c_int").value(0), 42);
    assert_eq!(i64s("c_big").value(0), i64::MIN);
    assert_eq!(i64s("c_year").value(0), 2024);
    assert_eq!(i64s("c_bit").value(0), 5);
    assert_eq!(strs("c_ubig").value(0), "18446744073709551615");
    assert_eq!(col("c_double").as_any().downcast_ref::<arrow_array::Float64Array>().unwrap().value(0), 2.25);
    assert_eq!(col("c_dec").as_any().downcast_ref::<Decimal128Array>().unwrap().value(0), 12345);
    assert_eq!(strs("c_wide").value(0), "1.500000000000000000000000000000");
    let date = col("c_date");
    let date = date.as_any().downcast_ref::<Date32Array>().unwrap();
    assert_eq!(date.value(0), 19782, "2024-02-29 is day 19782");
    let dt = col("c_datetime");
    let dt = dt.as_any().downcast_ref::<TimestampMicrosecondArray>().unwrap();
    assert_eq!(dt.value(0), 1_709_210_096_123_456, "2024-02-29 12:34:56.123456");
    assert!(!col("c_ts").is_null(0));
    assert_eq!(strs("c_time").value(0), "-10:20:30.500");
    assert_eq!(strs("c_char").value(0), "ab");
    assert_eq!(strs("c_enum").value(0), "b");
    assert_eq!(strs("c_set").value(0), "x,y");
    assert!(strs("c_json").value(0).contains("\"a\""));
    let bin = col("c_binary");
    assert_eq!(bin.as_any().downcast_ref::<BinaryArray>().unwrap().value(0), b"ab\0", "BINARY(3) is zero padded");
    let blob = col("c_blob");
    assert_eq!(blob.as_any().downcast_ref::<BinaryArray>().unwrap().value(0), &[0xde, 0xad, 0xbe, 0xef]);

    // Row 2: every nullable column is NULL.
    for f in schema.fields().iter().skip(1) {
        assert!(col(f.name()).is_null(1), "{} must be NULL in row 2", f.name());
    }
    // Row 3: zero dates become NULL, empty strings stay empty strings.
    assert!(date.is_null(2), "0000-00-00 -> NULL");
    assert!(dt.is_null(2), "zero datetime -> NULL");
    assert_eq!(strs("c_varchar").value(2), "");
    assert!(!col("c_varchar").is_null(2));
    assert_eq!(i64s("c_bit").value(2), 0);

    adm.query_drop("DROP TABLE IF EXISTS `bulkdb_it_types`").await.unwrap();
}

#[tokio::test]
async fn cancel_kills_the_server_query() {
    let t = require_server!();
    let mut adm = admin(&t).await;
    create_nasty(&mut adm, "bulkdb_it_cancel", 3_000).await;

    let db = connect(&t, "bulkdb-it-cancel").await;
    let id = session_id_of(&db);
    let cancel = CancellationToken::new();
    // 27 billion rows: never finishes on its own.
    let sql = "SELECT a.id, b.id, c.note FROM bulkdb_it_cancel a, bulkdb_it_cancel b, bulkdb_it_cancel c";
    let out = db.stream(&query(sql), &SessionOpts::default(), cancel.clone()).await.unwrap();
    let mut stream = out.stream;
    let first = stream.next().await.expect("a first batch arrives");
    assert!(matches!(first, Ok(Batch::Arrow(_))), "first item is a batch: {first:?}");

    let started = Instant::now();
    cancel.cancel();
    let mut terminal = None;
    while let Some(item) = stream.next().await {
        if let Err(e) = item {
            terminal = Some(e);
            break;
        }
    }
    assert!(matches!(terminal, Some(CoreError::Cancelled)), "stream must end with Cancelled, got {terminal:?}");
    assert!(started.elapsed() < Duration::from_secs(10), "cancel took {:?}", started.elapsed());

    // The server side is gone too: the connection disappears or goes idle within a few seconds.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match command_of(&mut adm, id).await {
            None => break,
            Some(cmd) if cmd == "Sleep" => break,
            Some(cmd) => {
                assert!(Instant::now() < deadline, "connection {id} still '{cmd}' 10 s after cancel");
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        }
    }
    adm.query_drop("DROP TABLE IF EXISTS `bulkdb_it_cancel`").await.unwrap();
}

/// Wait until connection `id` no longer runs a statement (gone or `Sleep`), at most `secs`.
async fn wait_not_running(adm: &mut mysql_async::Conn, id: u32, secs: u64, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        match command_of(adm, id).await {
            None => return,
            Some(cmd) if cmd == "Sleep" => return,
            Some(cmd) => {
                assert!(Instant::now() < deadline, "connection {id} still '{cmd}' {secs} s after {what}");
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        }
    }
}

/// The consumer goes away while the server is slow to produce the next (or the first) row: the
/// reader task must kill the query instead of waiting for that row.
#[tokio::test]
async fn dropped_consumer_kills_a_slow_query() {
    let t = require_server!();
    let mut adm = admin(&t).await;
    create_nasty(&mut adm, "bulkdb_it_dropped", 3_000).await;

    // Mid-result: rows flow in primary-key order (`text` is in no index), then the server
    // sleeps 60 s on row 2900.
    let db = connect(&t, "bulkdb-it-dropped").await;
    let id = session_id_of(&db);
    let opts = SessionOpts { fetch_batch_rows: Some(100), ..Default::default() };
    let sql = "SELECT id, text, SLEEP(IF(id = 2900, 60, 0)) AS s FROM bulkdb_it_dropped";
    let out = db.stream(&query(sql), &opts, CancellationToken::new()).await.unwrap();
    let mut stream = out.stream;
    assert!(matches!(stream.next().await, Some(Ok(Batch::Arrow(_)))));
    drop(stream);
    wait_not_running(&mut adm, id, 10, "the stream was dropped").await;

    // Before the first row: the caller gives up on `stream()` while the server is busy.
    let db = connect(&t, "bulkdb-it-dropped-2").await;
    let id = session_id_of(&db);
    let started = tokio::time::timeout(Duration::from_millis(500), db.stream(&query("SELECT SLEEP(60)"), &SessionOpts::default(), CancellationToken::new())).await;
    assert!(started.is_err(), "the server is still sleeping");
    wait_not_running(&mut adm, id, 10, "stream() was dropped").await;
    adm.query_drop("DROP TABLE IF EXISTS `bulkdb_it_dropped`").await.unwrap();
}

/// mysql_async always negotiates CLIENT_MULTI_STATEMENTS, so the driver must never let text
/// after a `;` run: extraction is a prepared statement (one statement only) and EXPLAIN is
/// checked lexically. The extraction session is read only on top of that.
#[tokio::test]
async fn refuses_multi_statements_and_runs_read_only() {
    let t = require_server!();
    let mut adm = admin(&t).await;
    adm.query_drop("DROP TABLE IF EXISTS `bulkdb_it_multi`").await.unwrap();
    adm.query_drop("DROP TABLE IF EXISTS `bulkdb_it_multi_ro`").await.unwrap();
    adm.query_drop("CREATE TABLE `bulkdb_it_multi_ro` (x INT) ENGINE=InnoDB").await.unwrap();

    let db = connect(&t, "bulkdb-it-multi").await;
    db.apply_session(&SessionOpts::default()).await.unwrap();

    // Extraction: the server refuses to prepare two statements, nothing after `;` runs.
    let multi = "SELECT 1; CREATE TABLE bulkdb_it_multi (x INT)";
    match db.stream(&query(multi), &SessionOpts::default(), CancellationToken::new()).await {
        Err(CoreError::Invalid(msg)) => assert!(msg.contains("single"), "{msg}"),
        Err(CoreError::Query { .. }) => {}
        Err(other) => panic!("unexpected error for a multi-statement extraction: {other:?}"),
        Ok(_) => panic!("a multi-statement extraction must be refused"),
    }
    // EXPLAIN (text protocol) refuses it before it reaches the server.
    match db.explain(multi, false).await {
        Err(CoreError::Invalid(_)) => {}
        other => panic!("EXPLAIN of two statements must be INVALID, got {other:?}"),
    }
    let created: Option<String> = adm
        .exec_first("SELECT CAST(table_name AS CHAR) FROM information_schema.tables WHERE table_schema = ? AND table_name = 'bulkdb_it_multi'", (t.database.clone(),))
        .await
        .unwrap();
    assert!(created.is_none(), "the statement after ';' must not have run");

    // A refused prepare leaves the connection usable, and quoted semicolons are data.
    let (batches, rows, err) = drain(&db, "SELECT ';' AS semi", CancellationToken::new()).await;
    assert!(err.is_none(), "stream failed: {err:?}");
    assert_eq!(rows, 1);
    let semi = batches[0].column(0).as_any().downcast_ref::<StringArray>().unwrap().value(0).to_string();
    assert_eq!(semi, ";");
    assert!(db.explain("SELECT ';' AS semi", false).await.unwrap().is_some());

    // The session is read only: a write that reaches the server is refused (1792).
    let (batches, _, err) = drain(&db, "SELECT CAST(@@SESSION.transaction_read_only AS CHAR)", CancellationToken::new()).await;
    let flag = match err {
        None => batches[0].column(0).as_any().downcast_ref::<StringArray>().unwrap().value(0).to_string(),
        // Servers before MySQL 5.7.20 / MariaDB 11.1 call it tx_read_only.
        Some(_) => {
            let (batches, _, err) = drain(&db, "SELECT CAST(@@SESSION.tx_read_only AS CHAR)", CancellationToken::new()).await;
            assert!(err.is_none(), "neither transaction_read_only nor tx_read_only is readable: {err:?}");
            batches[0].column(0).as_any().downcast_ref::<StringArray>().unwrap().value(0).to_string()
        }
    };
    assert_eq!(flag, "1", "apply_session makes the session read only");
    match db.stream(&query("INSERT INTO bulkdb_it_multi_ro VALUES (1)"), &SessionOpts::default(), CancellationToken::new()).await {
        Err(CoreError::Query { code, .. }) => assert_eq!(code.as_deref(), Some("1792"), "read-only transaction error"),
        Err(other) => panic!("unexpected error for a write in a read-only session: {other:?}"),
        Ok(_) => panic!("a write must fail in a read-only session"),
    }
    let count: Option<u64> = adm.query_first("SELECT COUNT(*) FROM bulkdb_it_multi_ro").await.unwrap();
    assert_eq!(count, Some(0));

    adm.query_drop("DROP TABLE IF EXISTS `bulkdb_it_multi`").await.unwrap();
    adm.query_drop("DROP TABLE IF EXISTS `bulkdb_it_multi_ro`").await.unwrap();
}

// ---------------------------------------------------------------------------------------------
// Probe, session, explain
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn probe_session_and_explain() {
    let t = require_server!();
    let mut adm = admin(&t).await;
    create_nasty(&mut adm, "bulkdb_it_probe", 1_000).await;

    let db = connect(&t, "bulkdb-it-probe").await;
    let info = db.probe().await.unwrap();
    assert_eq!(info.driver, "mysql");
    assert!(!info.version.is_empty());
    assert_eq!(info.database.as_deref(), Some(t.database.as_str()));
    assert!(info.user.is_some());
    assert_eq!(info.can_write, Some(true), "the fixture account creates tables, so it can write");
    if info.settings.contains_key("innodb_buffer_pool_size") {
        assert!(info.ram_bytes.is_some());
        assert_eq!(info.ram_source.as_deref(), Some("innodb_buffer_pool_size x 1.33"));
    }
    assert!(info.notes.iter().any(|n| n.starts_with("MySQL") || n.starts_with("MariaDB")));
    assert_eq!(info.settings.get("protocol").map(String::as_str), Some("modern"));
    assert!(info.settings.get("text_plan").is_some_and(|p| p.contains("SET NAMES")), "{:?}", info.settings.get("text_plan"));
    // protocol = auto reaches a 4.1+ server through the greeting check; legacy refuses it.
    let mut auto = params(&t, "bulkdb-it-auto");
    auto.fields.insert("protocol".into(), "auto".into());
    let via_auto = open(&auto).await.expect("protocol=auto connects to a 4.1+ server");
    assert_eq!(via_auto.probe().await.unwrap().settings.get("protocol").map(String::as_str), Some("modern"));
    let mut legacy = params(&t, "bulkdb-it-legacy");
    legacy.fields.insert("protocol".into(), "legacy".into());
    assert!(matches!(open(&legacy).await.err(), Some(CoreError::Connect(_))), "protocol=legacy refuses a 4.1+ server");

    db.apply_session(&SessionOpts { statement_timeout_ms: Some(120_000), work_mem: Some("64MB".into()), lock_timeout_ms: Some(5_000), ..Default::default() })
        .await
        .unwrap();

    let plan = db.explain("SELECT id, note FROM bulkdb_it_probe WHERE id < 10 ORDER BY note", false).await.unwrap().expect("a plan");
    assert_eq!(plan.raw_format, "json");
    assert!(!plan.analyzed);
    assert!(plan.root.is_some());
    assert!(plan.tables.iter().any(|r| r.name == "bulkdb_it_probe"), "tables: {:?}", plan.tables);
    assert!(plan.estimated_rows.is_some());

    match db.explain("SELECT COUNT(*) FROM bulkdb_it_probe", true).await {
        Ok(Some(analyzed)) => {
            assert!(analyzed.analyzed);
            assert!(!analyzed.raw.is_empty());
        }
        Err(CoreError::Invalid(msg)) => eprintln!("EXPLAIN ANALYZE unsupported here: {msg}"),
        other => panic!("unexpected EXPLAIN ANALYZE result: {other:?}"),
    }

    // The statement timeout is honoured and surfaces as `CoreError::Timeout`. A lone
    // `SELECT SLEEP(n)` cannot prove that: MySQL documents that an interrupted lone SLEEP()
    // returns 1 without an error. A SLEEP evaluated per row of the 1 000-row fixture (5 ms each,
    // 5 s in all) is interrupted mid-scan instead, and the statement then fails with ER 3024
    // (MySQL `max_execution_time`) / 1969 (MariaDB `max_statement_time`). COUNT(*) keeps any row
    // from being streamed before the error arrives.
    db.apply_session(&SessionOpts { statement_timeout_ms: Some(1_000), ..Default::default() }).await.unwrap();
    let version = db.version();
    let timeout_supported = match version.flavor {
        Flavor::MySql => version.supports_max_execution_time(),
        // `max_statement_time` arrived in MariaDB 10.1.1.
        Flavor::MariaDb => version.at_least(10, 1, 1),
    };
    let started = Instant::now();
    let (_, _, err) = drain(&db, "SELECT COUNT(*) FROM bulkdb_it_probe WHERE SLEEP(0.005) = 0", CancellationToken::new()).await;
    let elapsed = started.elapsed();
    match err {
        Some(CoreError::Timeout(_)) => assert!(elapsed < Duration::from_secs(4), "the 1 s statement timeout fired only after {elapsed:?}"),
        // MySQL < 5.7.8 and MariaDB < 10.1.1 have no statement timeout; the driver ignores it.
        None if !timeout_supported => eprintln!("{} has no statement timeout; the 5 s scan ran to completion", version.raw),
        other => panic!("expected CoreError::Timeout from a 1 s statement timeout on {}, got {other:?} after {elapsed:?}", version.raw),
    }
    adm.query_drop("DROP TABLE IF EXISTS `bulkdb_it_probe`").await.unwrap();
}

// ---------------------------------------------------------------------------------------------
// Catalog
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn catalog_matches_information_schema() {
    let t = require_server!();
    let mut adm = admin(&t).await;
    adm.query_drop("DROP VIEW IF EXISTS `bulkdb_it_cat_v`").await.unwrap();
    adm.query_drop("DROP TABLE IF EXISTS `bulkdb_it_cat_t`").await.unwrap();
    adm.query_drop("DROP TABLE IF EXISTS `bulkdb_it_cat_p`").await.unwrap();
    adm.query_drop("CREATE TABLE `bulkdb_it_cat_p` (id INT NOT NULL PRIMARY KEY, label VARCHAR(20)) ENGINE=InnoDB").await.unwrap();
    adm.query_drop(
        "CREATE TABLE `bulkdb_it_cat_t` (\
            id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, \
            parent_id INT NOT NULL COMMENT 'owner', \
            code VARCHAR(10) NOT NULL, \
            amount DECIMAL(12,2) NULL DEFAULT '0.00', \
            created DATETIME NOT NULL, \
            UNIQUE KEY uq_code (code), \
            KEY ix_parent_code (parent_id, code), \
            CONSTRAINT fk_cat_parent FOREIGN KEY (parent_id) REFERENCES `bulkdb_it_cat_p` (id)\
         ) ENGINE=InnoDB COMMENT='catalog fixture'",
    )
    .await
    .unwrap();
    adm.query_drop("INSERT INTO `bulkdb_it_cat_p` VALUES (1, 'one'), (2, 'two')").await.unwrap();
    let values: Vec<String> = (0..1000).map(|i| format!("({}, 'c{i}', {i}.5, '2024-01-01 00:00:00')", 1 + i % 2)).collect();
    adm.query_drop(format!("INSERT INTO `bulkdb_it_cat_t` (parent_id, code, amount, created) VALUES {}", values.join(","))).await.unwrap();
    adm.query_drop("ANALYZE TABLE `bulkdb_it_cat_t`").await.unwrap();
    adm.query_drop("CREATE VIEW `bulkdb_it_cat_v` AS SELECT id, code FROM `bulkdb_it_cat_t`").await.unwrap();

    let db = connect(&t, "bulkdb-it-catalog").await;

    let dbs = db.databases().await.unwrap();
    let mine = dbs.iter().find(|d| d.name == t.database).expect("the connected database is listed");
    assert!(mine.is_current);
    assert!(mine.accessible);
    assert!(mine.encoding.is_some());
    assert!(mine.size_bytes.unwrap_or(0) > 0, "database size comes from information_schema.tables");
    assert_eq!(dbs.iter().filter(|d| d.is_current).count(), 1);

    let schemas = db.schemas(&t.database).await.unwrap();
    assert_eq!(schemas.len(), 1);
    assert_eq!(schemas[0].name, t.database);
    assert!(schemas[0].table_count.unwrap_or(0) >= 3);
    assert!(matches!(db.schemas("bulkdb_no_such_db_xyz").await, Err(CoreError::NotFound(_))));

    let tables = db.tables(&t.database, &t.database).await.unwrap();
    let names: Vec<&str> = tables.iter().map(|x| x.name.as_str()).collect();
    let mut sorted = names.clone();
    sorted.sort_unstable();
    assert_eq!(names, sorted, "tables are ordered by name");
    let tt = tables.iter().find(|x| x.name == "bulkdb_it_cat_t").unwrap();
    assert_eq!(tt.kind, TableKind::Table);
    assert_eq!(tt.schema, t.database);
    assert!(tt.estimated_rows.is_some(), "InnoDB reports table_rows after ANALYZE");
    assert!(tt.table_bytes.unwrap_or(0) > 0);
    assert!(tt.total_bytes.unwrap_or(0) >= tt.table_bytes.unwrap_or(0));
    assert_eq!(tt.comment.as_deref(), Some("catalog fixture"));
    let tv = tables.iter().find(|x| x.name == "bulkdb_it_cat_v").unwrap();
    assert_eq!(tv.kind, TableKind::View);
    assert_eq!(tv.estimated_rows, None);
    // Same list as information_schema sees it.
    let expected_count: u64 = adm.exec_first("SELECT COUNT(*) FROM information_schema.tables WHERE table_schema = ?", (t.database.as_str(),)).await.unwrap().unwrap();
    assert_eq!(tables.len() as u64, expected_count);
    assert_eq!(db.tables(&t.database, "").await.unwrap().len(), tables.len(), "an empty schema falls back to the database");

    let tref = table(&t, "bulkdb_it_cat_t");
    let columns = db.columns(&tref).await.unwrap();
    let expected_names: Vec<String> = adm
        .exec(
            "SELECT column_name FROM information_schema.columns WHERE table_schema = ? AND table_name = ? ORDER BY ordinal_position",
            (t.database.as_str(), "bulkdb_it_cat_t"),
        )
        .await
        .unwrap();
    assert_eq!(columns.iter().map(|c| c.name.clone()).collect::<Vec<_>>(), expected_names);
    assert_eq!(columns[0].position, 1);
    assert!(columns[0].primary_key && columns[0].unique && !columns[0].nullable);
    assert!(columns[0].default.as_deref().unwrap_or("").contains("AUTO_INCREMENT"));
    assert!(columns[0].data_type.to_lowercase().starts_with("int"));
    let parent = columns.iter().find(|c| c.name == "parent_id").unwrap();
    assert_eq!(parent.foreign_key.as_deref(), Some("bulkdb_it_cat_p(id)"));
    assert_eq!(parent.comment.as_deref(), Some("owner"));
    let code = columns.iter().find(|c| c.name == "code").unwrap();
    assert!(code.unique && !code.primary_key);
    assert_eq!(code.data_type.to_lowercase(), "varchar(10)");
    let amount = columns.iter().find(|c| c.name == "amount").unwrap();
    assert!(amount.nullable);
    // MySQL reports `0.00`, MariaDB 10.2.7+ reports the literal quoted (`'0.00'`).
    assert!(amount.default.as_deref().unwrap_or("").contains("0.00"), "{:?}", amount.default);
    assert_eq!(amount.data_type.to_lowercase(), "decimal(12,2)");
    assert!(matches!(db.columns(&table(&t, "bulkdb_it_missing")).await, Err(CoreError::NotFound(_))));

    let indexes = db.indexes(&tref).await.unwrap();
    let pk = indexes.iter().find(|i| i.name == "PRIMARY").expect("PRIMARY index");
    assert!(pk.primary && pk.unique);
    assert_eq!(pk.columns, vec!["id"]);
    assert_eq!(pk.kind.as_deref(), Some("btree"));
    assert_eq!(indexes[0].name, "PRIMARY", "PRIMARY is listed first");
    let uq = indexes.iter().find(|i| i.name == "uq_code").unwrap();
    assert!(uq.unique && !uq.primary);
    assert_eq!(uq.columns, vec!["code"]);
    assert!(uq.definition.as_deref().unwrap_or("").contains("UNIQUE KEY"));
    let ix = indexes.iter().find(|i| i.name == "ix_parent_code").unwrap();
    assert!(!ix.unique);
    assert_eq!(ix.columns, vec!["parent_id", "code"]);
    assert!(ix.definition.as_deref().unwrap_or("").starts_with("KEY `ix_parent_code`"));
    assert!(ix.scans.is_none());
    if let Some(size) = ix.size_bytes {
        assert!(size > 0, "index size is pages x page size");
    }

    let ddl = db.ddl(&tref).await.unwrap();
    assert!(ddl.starts_with("CREATE TABLE"));
    assert!(ddl.contains("fk_cat_parent"));
    let view_ddl = db.ddl(&table(&t, "bulkdb_it_cat_v")).await.unwrap();
    assert!(view_ddl.to_uppercase().contains("VIEW"));
    assert!(matches!(db.ddl(&table(&t, "bulkdb_it_missing")).await, Err(CoreError::Query { .. }) | Err(CoreError::NotFound(_))));

    assert_eq!(db.exact_count(&tref, CancellationToken::new()).await.unwrap(), 1000);
    assert_eq!(db.exact_count(&table(&t, "bulkdb_it_cat_v"), CancellationToken::new()).await.unwrap(), 1000);
    assert!(db.exact_count(&table(&t, "bulkdb_it_missing"), CancellationToken::new()).await.is_err());

    let sql = db.select_all_sql(&tref, &columns);
    assert!(sql.starts_with("SELECT `id`,"), "{sql}");
    assert!(sql.ends_with(&format!("FROM `{}`.`bulkdb_it_cat_t`", t.database)), "{sql}");
    assert_eq!(db.quote_ident("a`b"), "`a``b`");
    // The generated statement is valid for the server.
    let (_, rows, err) = drain(&db, &sql, CancellationToken::new()).await;
    assert!(err.is_none(), "{err:?}");
    assert_eq!(rows, 1000);

    adm.query_drop("DROP VIEW IF EXISTS `bulkdb_it_cat_v`").await.unwrap();
    adm.query_drop("DROP TABLE IF EXISTS `bulkdb_it_cat_t`").await.unwrap();
    adm.query_drop("DROP TABLE IF EXISTS `bulkdb_it_cat_p`").await.unwrap();
}

// ---------------------------------------------------------------------------------------------
// Monitor
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn monitor_finds_and_cancels_the_session() {
    let t = require_server!();
    let mut adm = admin(&t).await;
    create_nasty(&mut adm, "bulkdb_it_mon", 3_000).await;

    let job = connect(&t, "bulkdb-itjob").await;
    let monitor = connect(&t, "bulkdb-monitor").await;
    let id = session_id_of(&job);
    let cancel = CancellationToken::new();
    let sql = "SELECT a.id, b.id, c.note FROM bulkdb_it_mon a, bulkdb_it_mon b, bulkdb_it_mon c";
    let out = job.stream(&query(sql), &SessionOpts::default(), cancel.clone()).await.unwrap();
    let mut stream = out.stream;
    assert!(matches!(stream.next().await, Some(Ok(Batch::Arrow(_)))));

    let handle = JobHandle { job_id: "itjob".into(), application_name: "bulkdb-itjob".into(), session_id: Some(id.to_string()), temp_bytes_at_start: None };
    let snap = monitor.snapshot(&handle).await.unwrap();
    assert!(snap.found, "session {id} must be visible in the processlist");
    assert_eq!(snap.pid.as_deref(), Some(id.to_string().as_str()));
    assert!(snap.state.is_some());
    assert!(snap.elapsed_ms.is_some());
    let ratio = snap.client_write_ratio.expect("ratio is tracked per job");
    assert!((0.0..=1.0).contains(&ratio));

    // Without a connection id the SQL tag locates the session.
    let by_tag = JobHandle { session_id: None, ..handle.clone() };
    let snap = monitor.snapshot(&by_tag).await.unwrap();
    assert!(snap.found, "the /* bulkdb-itjob */ tag must locate the session");
    assert_eq!(snap.pid.as_deref(), Some(id.to_string().as_str()));

    let touching = [table(&t, "bulkdb_it_mon")];
    let sessions = monitor.active_sessions(&touching).await.unwrap();
    let mine = sessions.iter().find(|s| s.pid == id.to_string()).expect("the extraction session touches bulkdb_it_mon");
    assert_eq!(mine.application.as_deref(), Some("bulkdb-itjob"));
    assert!(mine.tables.iter().any(|x| x.ends_with("bulkdb_it_mon")));
    assert!(mine.query.as_deref().unwrap_or("").contains("bulkdb_it_mon"));
    assert!(mine.user.is_some());
    assert!(monitor.active_sessions(&[table(&t, "bulkdb_it_nowhere")]).await.unwrap().iter().all(|s| s.pid != id.to_string()));
    assert!(monitor.active_sessions(&[]).await.unwrap().is_empty());

    // Cancel from the monitor side: the extraction ends with Cancelled (server error 1317).
    assert!(monitor.cancel_session(&id.to_string()).await.unwrap());
    let started = Instant::now();
    let mut terminal = None;
    while let Some(item) = stream.next().await {
        if let Err(e) = item {
            terminal = Some(e);
            break;
        }
    }
    assert!(matches!(terminal, Some(CoreError::Cancelled)), "got {terminal:?}");
    assert!(started.elapsed() < Duration::from_secs(10));

    // Unknown ids are reported as "nothing to cancel", not as errors.
    assert!(!monitor.cancel_session("4294967295").await.unwrap());
    assert!(matches!(monitor.cancel_session("abc").await, Err(CoreError::Invalid(_))));

    let gone = JobHandle { session_id: Some("4294967295".into()), ..handle };
    assert!(!monitor.snapshot(&gone).await.unwrap().found);
    adm.query_drop("DROP TABLE IF EXISTS `bulkdb_it_mon`").await.unwrap();
}

// ---------------------------------------------------------------------------------------------
// Legacy MySQL 5.5 (BULKDB_MYSQL55_URL): EUC-KR server, pre-4.1 password hashes
// ---------------------------------------------------------------------------------------------
//
// The CI container runs `mysql:5.5 --character-set-server=euckr --collation-server=euckr_korean_ci
// --old-passwords=1 --secure-auth=0`, so the fixture database defaults to euckr and root has a
// pre-4.1 hash: every connection of this suite sets `allow_old_password=true` unless a test is
// about that toggle. Fixture objects are prefixed `bulkdb_it55_`.

/// `BULKDB_MYSQL55_URL` parsed like `BULKDB_MYSQL_URL`, or `None`.
fn target55() -> Option<Target> {
    let url = std::env::var("BULKDB_MYSQL55_URL").ok()?.trim().to_string();
    if url.is_empty() {
        return None;
    }
    Some(parse_target(&url))
}

macro_rules! require_server55 {
    () => {
        match target55() {
            Some(t) => t,
            None => {
                eprintln!("skipping: BULKDB_MYSQL55_URL is not set");
                return;
            }
        }
    };
}

/// Connection parameters for the 5.5 server with extra `--field`s (old passwords allowed by
/// default because root has a pre-4.1 hash).
fn params55(t: &Target, application_name: &str, fields: &[(&str, &str)]) -> ConnectParams {
    let mut p = params(t, application_name);
    p.fields.insert("allow_old_password".into(), "true".into());
    for (k, v) in fields {
        p.fields.insert(k.to_string(), v.to_string());
    }
    p
}

async fn connect55(t: &Target, application_name: &str, fields: &[(&str, &str)]) -> MySqlDatabase {
    MySqlDatabase::connect(&params55(t, application_name, fields)).await.expect("connect to mysql:5.5")
}

/// Admin connection to the 5.5 server (`secure_auth(false)`: root has an old hash).
async fn admin55(t: &Target) -> mysql_async::Conn {
    let opts = mysql_async::Opts::from_url(&t.url).expect("BULKDB_MYSQL55_URL is a valid mysql:// URL");
    mysql_async::Conn::new(mysql_async::OptsBuilder::from_opts(opts).secure_auth(false)).await.expect("admin connection to mysql:5.5")
}

/// Every string cell of column `col` over all batches.
fn strings(batches: &[arrow_array::RecordBatch], col: usize) -> Vec<String> {
    let mut out = Vec::new();
    for b in batches {
        let a = b.column(col).as_any().downcast_ref::<StringArray>().expect("a string column");
        out.extend((0..a.len()).map(|i| a.value(i).to_string()));
    }
    out
}

const HANGUL_CP949: &str = "X'C7D1B1DB'"; // "한글" in CP949 / EUC-KR

#[tokio::test]
async fn mysql55_old_password_account_needs_the_toggle() {
    let t = require_server55!();
    let mut adm = admin55(&t).await;
    for (user, old) in [("bulkdb_it55_old", 1), ("bulkdb_it55_new", 0)] {
        let _ = adm.query_drop(format!("DROP USER '{user}'@'%'")).await;
        adm.query_drop(format!("SET SESSION old_passwords = {old}")).await.unwrap();
        adm.query_drop(format!("CREATE USER '{user}'@'%' IDENTIFIED BY 'pw-{user}'")).await.unwrap();
        adm.query_drop(format!("GRANT SELECT ON `{}`.* TO '{user}'@'%'", t.database)).await.unwrap();
    }
    let len: Option<u64> = adm.query_first("SELECT LENGTH(Password) FROM mysql.user WHERE User = 'bulkdb_it55_old'").await.unwrap();
    assert_eq!(len, Some(16), "old_passwords = 1 stores a pre-4.1 hash");

    let as_user = |user: &str, fields: &[(&str, &str)]| {
        let mut p = params(&t, "bulkdb-it55-auth");
        p.user = Some(user.to_string());
        p.password = Some(format!("pw-{user}"));
        for (k, v) in fields {
            p.fields.insert(k.to_string(), v.to_string());
        }
        p
    };
    // protocol = auto: the greeting is 4.1+, so mysql_async connects; the old hash needs the toggle.
    match open(&as_user("bulkdb_it55_old", &[("protocol", "auto")])).await {
        Err(CoreError::Auth(msg)) => assert!(msg.contains("allow_old_password"), "the error names the toggle: {msg}"),
        Err(other) => panic!("expected AUTH for an old-password account without the toggle, got {other:?}"),
        Ok(_) => panic!("an old-password account must not log in without allow_old_password"),
    }
    let db = open(&as_user("bulkdb_it55_old", &[("protocol", "auto"), ("allow_old_password", "true")])).await.expect("old-password login with the toggle");
    let info = db.probe().await.unwrap();
    assert_eq!(info.settings.get("protocol").map(String::as_str), Some("modern"));
    assert!(info.version.starts_with("5.5"));
    // A 4.1-hash account never needs the toggle.
    open(&as_user("bulkdb_it55_new", &[("protocol", "modern")])).await.expect("new-hash login");

    for user in ["bulkdb_it55_old", "bulkdb_it55_new"] {
        let _ = adm.query_drop(format!("DROP USER '{user}'@'%'")).await;
    }
}

#[tokio::test]
async fn mysql55_legacy_protocol_is_refused() {
    let t = require_server55!();
    match open(&params55(&t, "bulkdb-it55-legacy", &[("protocol", "legacy")])).await {
        Err(CoreError::Connect(msg)) => assert!(msg.contains("auto") || msg.contains("modern"), "{msg}"),
        Err(other) => panic!("protocol=legacy against 5.5 must be CONNECT, got {other:?}"),
        Ok(_) => panic!("protocol=legacy must refuse a 4.1+ server"),
    }
    match open(&params55(&t, "bulkdb-it55-legacy", &[("protocol", "v3")])).await {
        Err(CoreError::Invalid(_)) => {}
        other => panic!("an unknown protocol value is INVALID, got {:?}", other.err()),
    }
}

#[tokio::test]
async fn mysql55_euckr_and_latin1_tables_read_as_korean() {
    let t = require_server55!();
    let mut adm = admin55(&t).await;
    adm.query_drop("DROP TABLE IF EXISTS bulkdb_it55_euckr").await.unwrap();
    adm.query_drop("DROP TABLE IF EXISTS bulkdb_it55_latin1").await.unwrap();
    adm.query_drop("CREATE TABLE bulkdb_it55_euckr (id INT PRIMARY KEY, name VARCHAR(20)) DEFAULT CHARSET=euckr").await.unwrap();
    adm.query_drop("CREATE TABLE bulkdb_it55_latin1 (id INT PRIMARY KEY, name VARCHAR(20)) DEFAULT CHARSET=latin1").await.unwrap();
    // The legacy setup: CP949 bytes in a latin1 column, as an old application wrote them.
    adm.query_drop(format!("INSERT INTO bulkdb_it55_euckr VALUES (1, _euckr {HANGUL_CP949}), (2, 'abc')")).await.unwrap();
    adm.query_drop(format!("INSERT INTO bulkdb_it55_latin1 VALUES (1, _latin1 {HANGUL_CP949}), (2, 'abc')")).await.unwrap();

    // encoding = euc-kr: no server-side conversion, each column decoded by its collation.
    let db = connect55(&t, "bulkdb-it55-euc", &[("encoding", "euc-kr")]).await;
    assert_eq!(db.text_plan(), &TextPlan::PassThrough { client: "euckr", legacy: Charset::EucKr }, "the fixture database defaults to euckr");
    for table in ["bulkdb_it55_euckr", "bulkdb_it55_latin1"] {
        let (batches, rows, err) = drain(&db, &format!("SELECT name FROM {table} ORDER BY id"), CancellationToken::new()).await;
        assert!(err.is_none(), "{table}: {err:?}");
        assert_eq!(rows, 2);
        assert_eq!(strings(&batches, 0), ["한글", "abc"], "{table} under encoding=euc-kr");
    }
    // A Korean literal and alias are encoded as EUC-KR on the way in.
    let (batches, rows, err) = drain(&db, "SELECT id AS 번호, name FROM bulkdb_it55_euckr WHERE name = '한글'", CancellationToken::new()).await;
    assert!(err.is_none(), "{err:?}");
    assert_eq!(rows, 1);
    assert_eq!(batches[0].schema().field(0).name(), "번호");
    // A character EUC-KR cannot represent is refused before it reaches the server.
    match db.stream(&query("SELECT '😀' AS x"), &SessionOpts::default(), CancellationToken::new()).await {
        Err(CoreError::Invalid(_)) => {}
        Err(other) => panic!("an unrepresentable literal must be INVALID, got {other:?}"),
        Ok(_) => panic!("an unrepresentable literal must be refused"),
    }

    // encoding = auto: the server converts; the euckr column is right, the latin1 one is the
    // familiar mojibake (and the probe suggests euc-kr when the default charset is latin1).
    let auto = connect55(&t, "bulkdb-it55-auto", &[]).await;
    assert_eq!(auto.text_plan(), &TextPlan::ServerUtf8 { names: "utf8mb4" });
    let (batches, _, err) = drain(&auto, "SELECT name FROM bulkdb_it55_euckr ORDER BY id", CancellationToken::new()).await;
    assert!(err.is_none(), "{err:?}");
    assert_eq!(strings(&batches, 0), ["한글", "abc"]);
    let (batches, _, err) = drain(&auto, "SELECT name FROM bulkdb_it55_latin1 ORDER BY id", CancellationToken::new()).await;
    assert!(err.is_none(), "{err:?}");
    assert_eq!(strings(&batches, 0), ["ÇÑ±Û", "abc"]);

    adm.query_drop("DROP TABLE IF EXISTS bulkdb_it55_euckr").await.unwrap();
    adm.query_drop("DROP TABLE IF EXISTS bulkdb_it55_latin1").await.unwrap();
}

/// A latin1 database (the classic Korean 4.x / 5.x setup): the client charset is latin1, so a
/// Korean literal compares byte-for-byte with the CP949 stored in latin1 columns, and names /
/// comments double-encoded in the data dictionary are repaired.
#[tokio::test]
async fn mysql55_latin1_database_holding_cp949() {
    let t = require_server55!();
    let mut adm = admin55(&t).await;
    adm.query_drop("DROP DATABASE IF EXISTS bulkdb_it55_l1").await.unwrap();
    adm.query_drop("CREATE DATABASE bulkdb_it55_l1 DEFAULT CHARACTER SET latin1").await.unwrap();
    // Sent through a latin1 connection as raw CP949 bytes, exactly like the legacy application.
    adm.query_drop("SET NAMES latin1").await.unwrap();
    let mut ddl = b"CREATE TABLE bulkdb_it55_l1.member (id INT PRIMARY KEY, name VARCHAR(20)) DEFAULT CHARSET=latin1 COMMENT='".to_vec();
    ddl.extend_from_slice(&[0xC7, 0xD1, 0xB1, 0xDB]);
    ddl.push(b'\'');
    adm.query_drop(ddl.as_slice()).await.unwrap();
    adm.query_drop(format!("INSERT INTO bulkdb_it55_l1.member VALUES (1, {HANGUL_CP949}), (2, 'abc')")).await.unwrap();

    let mut p = params55(&t, "bulkdb-it55-l1", &[("encoding", "euc-kr")]);
    p.database = Some("bulkdb_it55_l1".into());
    let db = MySqlDatabase::connect(&p).await.expect("connect to the latin1 database");
    assert_eq!(db.text_plan(), &TextPlan::PassThrough { client: "latin1", legacy: Charset::EucKr });
    let (batches, rows, err) = drain(&db, "SELECT name FROM member WHERE name = '한글'", CancellationToken::new()).await;
    assert!(err.is_none(), "{err:?}");
    assert_eq!(rows, 1);
    assert_eq!(strings(&batches, 0), ["한글"]);

    let tables = db.tables("bulkdb_it55_l1", "bulkdb_it55_l1").await.unwrap();
    let member = tables.iter().find(|x| x.name == "member").expect("member is listed");
    assert_eq!(member.comment.as_deref(), Some("한글"), "the double-encoded comment is repaired");
    let probe = db.probe().await.unwrap();
    assert!(probe.settings.get("text_plan").is_some_and(|p| p.contains("latin1")), "{:?}", probe.settings.get("text_plan"));

    adm.query_drop("DROP DATABASE IF EXISTS bulkdb_it55_l1").await.unwrap();
}

#[tokio::test]
async fn mysql55_probe_session_and_tabular_explain() {
    let t = require_server55!();
    let mut adm = admin55(&t).await;
    create_nasty(&mut adm, "bulkdb_it55_probe", 1_000).await;

    let db = connect55(&t, "bulkdb-it55-probe", &[]).await;
    assert!(!db.facts().caps.explain_json && !db.facts().caps.read_only_tx && db.facts().caps.kill_query);
    let info = db.probe().await.unwrap();
    assert_eq!(info.settings.get("protocol").map(String::as_str), Some("modern"));
    assert_eq!(info.settings.get("character_set_server").map(String::as_str), Some("euckr"));
    assert!(info.settings.get("text_plan").is_some_and(|p| p.contains("utf8mb4")));
    assert!(info.notes.iter().any(|n| n.contains("no statement timeout")), "{:?}", info.notes);
    assert_eq!(info.can_write, Some(true));

    // No READ ONLY session and no statement timeout on 5.5: both are skipped, not errors.
    db.apply_session(&SessionOpts { statement_timeout_ms: Some(60_000), ..Default::default() }).await.unwrap();

    let plan = db.explain("SELECT id, note FROM bulkdb_it55_probe WHERE id < 10 ORDER BY note", false).await.unwrap().expect("a plan");
    assert_eq!(plan.raw_format, "text");
    assert!(plan.raw.starts_with("id\tselect_type\ttable"), "{}", plan.raw);
    assert!(plan.root.is_some());
    assert!(plan.tables.iter().any(|r| r.name == "bulkdb_it55_probe"), "{:?}", plan.tables);
    assert!(plan.estimated_rows.is_some());
    // 5.5 would run a derived table to explain it: refused instead.
    assert!(matches!(db.explain("SELECT * FROM (SELECT id FROM bulkdb_it55_probe) d", false).await, Err(CoreError::Invalid(_))));
    assert!(matches!(db.explain("SELECT 1", true).await, Err(CoreError::Invalid(_))));
    adm.query_drop("DROP TABLE IF EXISTS bulkdb_it55_probe").await.unwrap();
}

/// 5.5 has `KILL QUERY`: a cancelled count leaves the session alive. A `KILL <id>` (what 4.x
/// cancels do) ends it, and the next call reconnects on its own.
#[tokio::test]
async fn mysql55_kill_query_keeps_the_session_and_kill_reconnects() {
    let t = require_server55!();
    let mut adm = admin55(&t).await;
    create_nasty(&mut adm, "bulkdb_it55_kill", 3_000).await;
    adm.query_drop("DROP VIEW IF EXISTS bulkdb_it55_slow").await.unwrap();
    adm.query_drop("CREATE VIEW bulkdb_it55_slow AS SELECT a.id FROM bulkdb_it55_kill a, bulkdb_it55_kill b, bulkdb_it55_kill c").await.unwrap();

    let db = connect55(&t, "bulkdb-it55-kill", &[]).await;
    let id = session_id_of(&db);
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(500)).await;
        trigger.cancel();
    });
    let started = Instant::now();
    let r = db.exact_count(&table(&t, "bulkdb_it55_slow"), cancel).await;
    assert!(matches!(r, Err(CoreError::Cancelled)), "got {r:?}");
    assert!(started.elapsed() < Duration::from_secs(5), "cancel took {:?}", started.elapsed());
    // KILL QUERY: same session, still usable.
    assert_eq!(db.exact_count(&table(&t, "bulkdb_it55_kill"), CancellationToken::new()).await.unwrap(), 3_000);
    assert_eq!(session_id_of(&db), id);

    // A whole-session KILL from outside: the next metadata call reconnects transparently.
    adm.query_drop(format!("KILL {id}")).await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    let first = db.tables(&t.database, &t.database).await;
    let tables = match first {
        Ok(tables) => tables,
        // The call that discovers the dead socket fails with CONNECT; the next one reconnects.
        Err(CoreError::Connect(_)) => db.tables(&t.database, &t.database).await.expect("reconnected"),
        Err(other) => panic!("unexpected error after KILL: {other:?}"),
    };
    assert!(tables.iter().any(|x| x.name == "bulkdb_it55_kill"));
    assert_ne!(session_id_of(&db), id, "a new connection after KILL");

    // Streaming cancel ends with Cancelled via KILL QUERY.
    let cancel = CancellationToken::new();
    let out = db.stream(&query("SELECT a.id, b.id FROM bulkdb_it55_kill a, bulkdb_it55_kill b"), &SessionOpts::default(), cancel.clone()).await.unwrap();
    let mut stream = out.stream;
    assert!(matches!(stream.next().await, Some(Ok(Batch::Arrow(_)))));
    cancel.cancel();
    let mut terminal = None;
    while let Some(item) = stream.next().await {
        if let Err(e) = item {
            terminal = Some(e);
            break;
        }
    }
    assert!(matches!(terminal, Some(CoreError::Cancelled)), "got {terminal:?}");

    adm.query_drop("DROP VIEW IF EXISTS bulkdb_it55_slow").await.unwrap();
    adm.query_drop("DROP TABLE IF EXISTS bulkdb_it55_kill").await.unwrap();
}

/// The `SHOW`-based catalog (what 4.0 / 4.1 get) agrees with `information_schema` on 5.5.
#[tokio::test]
async fn mysql55_show_catalog_matches_information_schema() {
    let t = require_server55!();
    let mut adm = admin55(&t).await;
    adm.query_drop("DROP TABLE IF EXISTS bulkdb_it55_cat").await.unwrap();
    adm.query_drop(
        "CREATE TABLE bulkdb_it55_cat (\
            id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, \
            code VARCHAR(10) NOT NULL COMMENT 'business key', \
            label VARCHAR(40) NULL DEFAULT 'none', \
            UNIQUE KEY uq_code (code), \
            KEY ix_label (label(8))\
         ) ENGINE=InnoDB COMMENT='catalog fixture'",
    )
    .await
    .unwrap();
    adm.query_drop("INSERT INTO bulkdb_it55_cat (code, label) VALUES ('a', 'x'), ('b', NULL)").await.unwrap();

    let db = connect55(&t, "bulkdb-it55-cat", &[]).await;
    let tref = table(&t, "bulkdb_it55_cat");

    let is_tables = generic::tables(&db, &t.database, &t.database).await.unwrap();
    let show_tables = generic::show::tables(&db, &t.database).await.unwrap();
    // Other tests create and drop tables in parallel: compare the fixture, not whole listings.
    let listed = is_tables.iter().find(|x| x.name == "bulkdb_it55_cat").expect("information_schema lists the fixture");
    let shown = show_tables.iter().find(|x| x.name == "bulkdb_it55_cat").expect("SHOW TABLE STATUS lists the fixture");
    assert_eq!((shown.kind, &shown.comment), (listed.kind, &listed.comment));
    assert_eq!(shown.kind, TableKind::Table);
    assert_eq!(shown.comment.as_deref(), Some("catalog fixture"), "InnoDB notes are stripped");
    assert!(shown.estimated_rows.is_some() && shown.table_bytes.is_some());

    let is_cols = generic::columns(&db, &tref).await.unwrap();
    let show_cols = generic::show::columns(&db, &tref).await.unwrap();
    let sig = |c: &bulk_core::types::ColumnInfo| (c.position, c.name.clone(), c.data_type.to_lowercase(), c.nullable, c.primary_key, c.unique, c.comment.clone());
    assert_eq!(show_cols.iter().map(sig).collect::<Vec<_>>(), is_cols.iter().map(sig).collect::<Vec<_>>());
    assert_eq!(show_cols[1].comment.as_deref(), Some("business key"));

    let is_ix = generic::indexes(&db, &tref).await.unwrap();
    let show_ix = generic::show::indexes(&db, &tref).await.unwrap();
    let isig = |i: &bulk_core::types::IndexInfo| (i.name.clone(), i.primary, i.unique, i.columns.clone(), i.kind.clone(), i.definition.clone());
    assert_eq!(show_ix.iter().map(isig).collect::<Vec<_>>(), is_ix.iter().map(isig).collect::<Vec<_>>());
    assert_eq!(show_ix[0].name, "PRIMARY");
    assert!(show_ix.iter().any(|i| i.columns == ["label(8)"]));

    let schemas = generic::show::schemas(&db, &t.database).await.unwrap();
    assert!(schemas[0].table_count.unwrap_or(0) >= 1);
    assert!(matches!(generic::show::schemas(&db, "bulkdb_no_such_db_xyz").await, Err(CoreError::NotFound(_))));
    assert!(matches!(generic::show::columns(&db, &table(&t, "bulkdb_it55_missing")).await, Err(CoreError::NotFound(_))));
    let dbs = generic::show::databases(&db).await.unwrap();
    assert!(dbs.iter().any(|d| d.name == t.database && d.is_current));

    adm.query_drop("DROP TABLE IF EXISTS bulkdb_it55_cat").await.unwrap();
}
