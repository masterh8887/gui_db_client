//! SQL Server driver integration tests. They need a reachable server:
//!
//! ```text
//! BULKDB_MSSQL_URL=mssql://sa:Str0ng!Pass@localhost:1433/tempdb?encrypt=false
//! cargo test -p bulk-core --features it --test mssql_it
//! ```
//!
//! Query parameters become driver fields (`encrypt`, `trust_server_certificate`, `instance`);
//! `trust_server_certificate` defaults to `true` here because the docker images ship a
//! self-signed certificate. Every test skips (passes) when the variable is unset, and creates
//! its objects in a `bulkdb_it` schema with the process id in the names so parallel runs do
//! not collide.
#![cfg(feature = "it")]

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use arrow_array::{
    Array, BinaryArray, BooleanArray, Date32Array, Decimal128Array, Float64Array, Int64Array, RecordBatch, StringArray, Time64MicrosecondArray, TimestampMicrosecondArray,
};
use arrow_schema::{DataType, TimeUnit};
use bulk_core::catalog::Catalog;
use bulk_core::monitor::{JobHandle, Monitor, ServerSnapshot};
use bulk_core::source::mssql::MssqlDatabase;
use bulk_core::source::{Batch, ConnectParams, Query, Source, SslMode, StreamOutput};
use bulk_core::testdata::{self, Lcg};
use bulk_core::types::{SessionOpts, TableKind, TableRef};
use bulk_core::{CancellationToken, CoreError, BATCH_ROWS};
use futures::StreamExt;

const SCHEMA: &str = "bulkdb_it";

// ---------------------------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------------------------

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `mssql://user:pass@host:port/db?field=value` -> `ConnectParams`, or `None` when the env
/// variable is unset (the test then skips).
fn params(application_name: &str) -> Option<ConnectParams> {
    let url = std::env::var("BULKDB_MSSQL_URL").ok().filter(|u| !u.trim().is_empty())?;
    let rest = url.trim().split_once("://").map(|(_, r)| r).unwrap_or(url.trim());
    let (userinfo, hostpart) = rest.rsplit_once('@').map(|(u, h)| (Some(u), h)).unwrap_or((None, rest));
    let (user, password) = match userinfo {
        Some(u) => match u.split_once(':') {
            Some((user, pw)) => (Some(percent_decode(user)), Some(percent_decode(pw))),
            None => (Some(percent_decode(u)), None),
        },
        None => (None, None),
    };
    let (hostport, path_query) = hostpart.split_once('/').unwrap_or((hostpart, ""));
    let (path, query) = path_query.split_once('?').unwrap_or((path_query, ""));
    let (host, port) = match hostport.rsplit_once(':') {
        Some((h, p)) => (h.to_string(), p.parse::<u16>().unwrap_or(1433)),
        None => (hostport.to_string(), 1433),
    };
    let mut fields: BTreeMap<String, String> =
        query.split('&').filter(|kv| !kv.is_empty()).filter_map(|kv| kv.split_once('=').map(|(k, v)| (k.to_string(), percent_decode(v)))).collect();
    fields.entry("trust_server_certificate".into()).or_insert_with(|| "true".into());
    let ssl = fields.get("sslmode").map(|s| SslMode::parse(s)).unwrap_or_default();
    Some(ConnectParams {
        driver: "mssql".into(),
        host,
        port,
        database: Some(path.to_string()).filter(|d| !d.is_empty()),
        user,
        password,
        fields,
        application_name: application_name.to_string(),
        connect_timeout: Duration::from_secs(15),
        ssl,
    })
}

async fn connect(application_name: &str) -> Option<MssqlDatabase> {
    let p = params(application_name)?;
    Some(MssqlDatabase::connect(&p).await.expect("connect to BULKDB_MSSQL_URL"))
}

fn query(sql: &str) -> Query {
    Query { sql: sql.to_string(), output: StreamOutput::Arrow }
}

fn tref(name: &str) -> TableRef {
    TableRef::new(Some(SCHEMA), name)
}

fn unique(prefix: &str) -> String {
    format!("{prefix}_{}", std::process::id())
}

/// T-SQL string literal (`N'...'`) or `NULL`.
fn lit(v: Option<&str>) -> String {
    match v {
        Some(s) => format!("N'{}'", s.replace('\'', "''")),
        None => "NULL".into(),
    }
}

async fn ensure_schema(db: &MssqlDatabase) {
    db.execute_batch(&format!("IF SCHEMA_ID('{SCHEMA}') IS NULL EXEC('CREATE SCHEMA [{SCHEMA}]')")).await.unwrap();
}

async fn drop_table(db: &MssqlDatabase, name: &str) {
    let _ = db.execute_batch(&format!("DROP TABLE IF EXISTS [{SCHEMA}].[{name}]")).await;
}

/// Drain a stream into batches, checking every batch against the row cap.
async fn drain(db: &MssqlDatabase, sql: &str, opts: &SessionOpts) -> Vec<RecordBatch> {
    let out = db.stream(&query(sql), opts, CancellationToken::new()).await.unwrap();
    let cap = opts.fetch_batch_rows.map(|n| n as usize).unwrap_or(BATCH_ROWS);
    let mut stream = out.stream;
    let mut batches = Vec::new();
    while let Some(item) = stream.next().await {
        match item.unwrap() {
            Batch::Arrow(b) => {
                assert!(b.num_rows() <= cap, "batch of {} rows exceeds the cap {cap}", b.num_rows());
                batches.push(b);
            }
            other => panic!("unexpected batch {other:?}"),
        }
    }
    batches
}

/// `connection_id` of the object's current physical connection. Unlike the SPID, which SQL
/// Server hands to the next login as soon as it is free, it is unique per connection.
/// (Reading `sys.dm_exec_connections` needs VIEW SERVER STATE; CI connects as `sa`.)
async fn connection_guid(db: &MssqlDatabase) -> String {
    let sql = "SELECT CONVERT(nvarchar(36), connection_id) AS cid FROM sys.dm_exec_connections WHERE session_id = @@SPID";
    let batches = drain(db, sql, &SessionOpts::default()).await;
    let b = batches.iter().find(|b| b.num_rows() > 0).expect("a row for the own connection");
    str_col(b, 0).value(0).to_string()
}

fn str_col(b: &RecordBatch, i: usize) -> &StringArray {
    b.column(i).as_any().downcast_ref::<StringArray>().unwrap_or_else(|| panic!("column {i} is {:?}, not Utf8", b.schema().field(i).data_type()))
}

fn i64_col(b: &RecordBatch, i: usize) -> &Int64Array {
    b.column(i).as_any().downcast_ref::<Int64Array>().unwrap_or_else(|| panic!("column {i} is {:?}, not Int64", b.schema().field(i).data_type()))
}

/// Poll `monitor.snapshot(handle)` until `pred` accepts it, failing after `timeout`.
async fn wait_for_snapshot(monitor: &MssqlDatabase, handle: &JobHandle, what: &str, timeout: Duration, pred: impl Fn(&ServerSnapshot) -> bool) -> ServerSnapshot {
    let started = Instant::now();
    loop {
        let snap = monitor.snapshot(handle).await.unwrap();
        if pred(&snap) {
            return snap;
        }
        assert!(started.elapsed() < timeout, "timed out waiting for {what}: {snap:?}");
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

// ---------------------------------------------------------------------------------------------
// Streaming
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn streams_100k_rows_in_capped_batches() {
    let Some(db) = connect("bulkdb-it-100k").await else { return };
    db.apply_session(&SessionOpts { lock_timeout_ms: Some(5000), ..Default::default() }).await.unwrap();
    // 100 000 generated rows, no table needed.
    let sql = "SELECT TOP (100000) CAST(ROW_NUMBER() OVER (ORDER BY (SELECT NULL)) AS bigint) AS n, \
                      CAST(a.object_id AS int) AS oid, a.name AS obj, CAST(b.object_id % 2 AS bit) AS flag \
               FROM sys.all_objects a CROSS JOIN sys.all_objects b";
    let opts = SessionOpts { fetch_batch_rows: Some(10_000), ..Default::default() };
    let batches = drain(&db, sql, &opts).await;
    let rows: usize = batches.iter().map(|b| b.num_rows()).sum();
    assert_eq!(rows, 100_000);
    assert!(batches.len() >= 10, "{} batches", batches.len());
    let schema = batches[0].schema();
    assert_eq!(schema.field(0).data_type(), &DataType::Int64);
    assert_eq!(schema.field(1).data_type(), &DataType::Int64);
    assert_eq!(schema.field(2).data_type(), &DataType::Utf8);
    assert_eq!(schema.field(3).data_type(), &DataType::Boolean);
    let sum: i64 = batches.iter().map(|b| i64_col(b, 0).iter().map(|v| v.unwrap()).sum::<i64>()).sum();
    assert_eq!(sum, 100_000 * 100_001 / 2);
    // The default cap applies without fetch_batch_rows.
    let batches = drain(&db, sql, &SessionOpts::default()).await;
    assert_eq!(batches.iter().map(|b| b.num_rows()).sum::<usize>(), 100_000);
    assert!(batches.len() <= 3, "{} batches at the 64K cap", batches.len());
}

#[tokio::test]
async fn nasty_values_round_trip() {
    let Some(db) = connect("bulkdb-it-nasty").await else { return };
    ensure_schema(&db).await;
    let table = unique("nasty");
    drop_table(&db, &table).await;
    db.execute_batch(&format!(
        "CREATE TABLE [{SCHEMA}].[{table}] (id int NOT NULL PRIMARY KEY, [text] nvarchar(max) NULL, num float NULL, note nvarchar(200) NULL, flag bit NULL, [unicode] nvarchar(100) NULL)"
    ))
    .await
    .unwrap();
    let mut rng = Lcg::new(7);
    let rows: Vec<Vec<Option<String>>> = (0..2000).map(|i| testdata::nasty_row(i, &mut rng)).collect();
    for chunk in rows.chunks(250) {
        let values: Vec<String> = chunk
            .iter()
            .map(|r| {
                format!(
                    "({}, {}, {}, {}, {}, {})",
                    r[0].as_deref().unwrap(),
                    lit(r[1].as_deref()),
                    r[2].as_deref().unwrap_or("NULL"),
                    lit(r[3].as_deref()),
                    match r[4].as_deref() {
                        Some("true") => "1",
                        Some("false") => "0",
                        _ => "NULL",
                    },
                    lit(r[5].as_deref()),
                )
            })
            .collect();
        db.execute_batch(&format!("INSERT INTO [{SCHEMA}].[{table}] (id, [text], num, note, flag, [unicode]) VALUES {}", values.join(", "))).await.unwrap();
    }
    let batches = drain(&db, &format!("SELECT id, [text], num, note, flag, [unicode] FROM [{SCHEMA}].[{table}] ORDER BY id"), &SessionOpts::default()).await;
    assert_eq!(batches.iter().map(|b| b.num_rows()).sum::<usize>(), rows.len());
    let mut seen = 0usize;
    for b in &batches {
        let ids = i64_col(b, 0);
        let texts = str_col(b, 1);
        let nums = b.column(2).as_any().downcast_ref::<Float64Array>().unwrap();
        let notes = str_col(b, 3);
        let flags = b.column(4).as_any().downcast_ref::<BooleanArray>().unwrap();
        let unicode = str_col(b, 5);
        for i in 0..b.num_rows() {
            let want = &rows[seen];
            assert_eq!(ids.value(i).to_string(), want[0].as_deref().unwrap());
            assert_eq!(texts.is_null(i), want[1].is_none(), "row {seen} text null");
            if let Some(t) = &want[1] {
                assert_eq!(texts.value(i), t, "row {seen} text");
            }
            match &want[2] {
                Some(n) => assert!((nums.value(i) - n.parse::<f64>().unwrap()).abs() < 1e-6, "row {seen} num"),
                None => assert!(nums.is_null(i), "row {seen} num null"),
            }
            assert_eq!(notes.is_null(i), want[3].is_none());
            if let Some(n) = &want[3] {
                assert_eq!(notes.value(i), n);
            }
            assert_eq!(flags.value(i), want[4].as_deref() == Some("true"));
            assert_eq!(unicode.is_null(i), want[5].is_none());
            if let Some(u) = &want[5] {
                assert_eq!(unicode.value(i), u, "row {seen} unicode");
            }
            seen += 1;
        }
    }
    drop_table(&db, &table).await;
}

#[tokio::test]
async fn decodes_all_column_types() {
    let Some(db) = connect("bulkdb-it-types").await else { return };
    let sql = "SELECT CAST(1 AS bit) AS b, CAST(255 AS tinyint) AS ti, CAST(-32768 AS smallint) AS si, CAST(2147483647 AS int) AS i, \
                      CAST(9223372036854775807 AS bigint) AS bi, CAST(1.5 AS real) AS r, CAST(2.25 AS float) AS f, \
                      CAST(12.3456 AS money) AS m, CAST(-1.5 AS smallmoney) AS sm, CAST(-123.45 AS decimal(10,2)) AS d, CAST(12345678901234567890 AS numeric(38,0)) AS n, \
                      CAST('2024-01-02T03:04:05.123' AS datetime) AS dt, CAST('2024-01-02T03:04:05.1234567' AS datetime2) AS dt2, \
                      CAST('2024-01-02T03:04:00' AS smalldatetime) AS sdt, CAST('2024-01-02T03:04:05+02:00' AS datetimeoffset) AS dto, \
                      CAST('2024-01-02' AS date) AS d8, CAST('03:04:05.123456' AS time) AS t, \
                      CAST('abc' AS varchar(10)) AS vc, CAST('abc' AS char(5)) AS c, N'한글' AS nvc, CAST(N'x' AS nchar(2)) AS nc, \
                      CAST('legacy' AS text) AS tx, CAST(N'legacy n' AS ntext) AS ntx, CAST('<a><b/></a>' AS xml) AS x, \
                      CAST('6f9619ff-8b86-d011-b42d-00c04fc964ff' AS uniqueidentifier) AS g, \
                      CAST(0x0102 AS varbinary(10)) AS vb, CAST(0x01 AS binary(2)) AS bn, CAST(0x03 AS image) AS img, \
                      CAST(NULL AS int) AS ni, CAST(NULL AS nvarchar(10)) AS ns, CAST(NULL AS datetime2) AS ndt";
    let batches = drain(&db, sql, &SessionOpts::default()).await;
    assert_eq!(batches.len(), 1);
    let b = &batches[0];
    assert_eq!(b.num_rows(), 1);
    let schema = b.schema();
    let ty = |name: &str| schema.field_with_name(name).unwrap().data_type().clone();
    let col = |name: &str| b.column(schema.index_of(name).unwrap());

    assert_eq!(ty("b"), DataType::Boolean);
    assert!(col("b").as_any().downcast_ref::<BooleanArray>().unwrap().value(0));
    for (name, want) in [("ti", 255i64), ("si", -32768), ("i", 2147483647), ("bi", i64::MAX)] {
        assert_eq!(ty(name), DataType::Int64, "{name}");
        assert_eq!(col(name).as_any().downcast_ref::<Int64Array>().unwrap().value(0), want, "{name}");
    }
    assert_eq!(ty("r"), DataType::Float64);
    assert_eq!(col("r").as_any().downcast_ref::<Float64Array>().unwrap().value(0), 1.5);
    assert_eq!(col("f").as_any().downcast_ref::<Float64Array>().unwrap().value(0), 2.25);
    assert_eq!(ty("m"), DataType::Decimal128(19, 4));
    assert_eq!(col("m").as_any().downcast_ref::<Decimal128Array>().unwrap().value(0), 123_456);
    assert_eq!(col("sm").as_any().downcast_ref::<Decimal128Array>().unwrap().value(0), -15_000);
    assert_eq!(ty("d"), DataType::Utf8);
    assert_eq!(col("d").as_any().downcast_ref::<StringArray>().unwrap().value(0), "-123.45");
    assert_eq!(col("n").as_any().downcast_ref::<StringArray>().unwrap().value(0), "12345678901234567890");

    let micros = |name: &str| col(name).as_any().downcast_ref::<TimestampMicrosecondArray>().unwrap().value(0);
    let base = chrono::NaiveDate::from_ymd_opt(2024, 1, 2).unwrap().and_hms_opt(3, 4, 5).unwrap().and_utc().timestamp_micros();
    assert_eq!(ty("dt"), DataType::Timestamp(TimeUnit::Microsecond, None));
    assert_eq!(micros("dt"), base + 123_000);
    assert_eq!(micros("dt2"), base + 123_456);
    assert_eq!(micros("sdt"), base - 5_000_000);
    assert_eq!(ty("dto"), DataType::Timestamp(TimeUnit::Microsecond, Some("UTC".into())));
    assert_eq!(micros("dto"), base - 2 * 3600 * 1_000_000, "datetimeoffset is converted to UTC");
    assert_eq!(ty("d8"), DataType::Date32);
    assert_eq!(col("d8").as_any().downcast_ref::<Date32Array>().unwrap().value(0), 19_724);
    assert_eq!(ty("t"), DataType::Time64(TimeUnit::Microsecond));
    assert_eq!(col("t").as_any().downcast_ref::<Time64MicrosecondArray>().unwrap().value(0), ((3 * 3600 + 4 * 60 + 5) * 1_000_000) + 123_456);

    let text = |name: &str| col(name).as_any().downcast_ref::<StringArray>().unwrap().value(0).to_string();
    assert_eq!(text("vc"), "abc");
    assert_eq!(text("c"), "abc  ");
    assert_eq!(text("nvc"), "한글");
    assert_eq!(text("nc"), "x ");
    assert_eq!(text("tx"), "legacy");
    assert_eq!(text("ntx"), "legacy n");
    assert!(text("x").contains("<b"), "{}", text("x"));
    assert_eq!(text("g"), "6F9619FF-8B86-D011-B42D-00C04FC964FF");
    let bytes = |name: &str| col(name).as_any().downcast_ref::<BinaryArray>().unwrap().value(0).to_vec();
    assert_eq!(ty("vb"), DataType::Binary);
    assert_eq!(bytes("vb"), vec![1, 2]);
    assert_eq!(bytes("bn"), vec![1, 0]);
    assert_eq!(bytes("img"), vec![3]);
    for name in ["ni", "ns", "ndt"] {
        assert!(col(name).is_null(0), "{name} should be NULL");
    }
}

#[tokio::test]
async fn second_result_set_is_ignored_and_errors_are_mapped() {
    let Some(db) = connect("bulkdb-it-multi").await else { return };
    let batches = drain(&db, "SELECT 1 AS a UNION ALL SELECT 2; SELECT 'ignored' AS b", &SessionOpts::default()).await;
    assert_eq!(batches.iter().map(|b| b.num_rows()).sum::<usize>(), 2);
    assert_eq!(batches[0].schema().field(0).name(), "a");
    // The connection was dropped after the first result set; the next call reconnects.
    let err = db
        .stream(&query("SELECT * FROM [bulkdb_it].[does_not_exist_anywhere]"), &SessionOpts::default(), CancellationToken::new())
        .await
        .err()
        .expect("missing table fails");
    match err {
        CoreError::Query { code, .. } => assert_eq!(code.as_deref(), Some("208")),
        other => panic!("unexpected {other:?}"),
    }
    assert!(db.session_id().is_some());
    // Statement timeout is enforced client-side.
    let opts = SessionOpts { statement_timeout_ms: Some(500), ..Default::default() };
    let out = db
        .stream(
            &query("SELECT a.object_id FROM sys.all_objects a CROSS JOIN sys.all_objects b CROSS JOIN sys.all_objects c CROSS JOIN sys.all_objects d"),
            &opts,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let mut stream = out.stream;
    let mut saw_timeout = false;
    while let Some(item) = stream.next().await {
        if let Err(CoreError::Timeout(_)) = item {
            saw_timeout = true;
            break;
        }
    }
    assert!(saw_timeout, "statement timeout should end the stream with TIMEOUT");
}

// ---------------------------------------------------------------------------------------------
// Cancellation and monitoring
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn cancel_closes_the_server_session() {
    let app = unique("bulkdb-it-cancel");
    let Some(db) = connect(&app).await else { return };
    let Some(monitor) = connect("bulkdb-it-monitor").await else { return };
    let spid = db.session_id().expect("spid after connect");
    let connection_before = connection_guid(&db).await;
    let handle = JobHandle { job_id: "job-cancel".into(), application_name: app.clone(), session_id: Some(spid.clone()), temp_bytes_at_start: None };

    let cancel = CancellationToken::new();
    let big = "SELECT a.object_id, b.object_id, c.object_id, a.name FROM sys.all_objects a CROSS JOIN sys.all_objects b CROSS JOIN sys.all_objects c";
    let out = db.stream(&query(big), &SessionOpts::default(), cancel.clone()).await.unwrap();
    let mut stream = out.stream;
    let first = stream.next().await.expect("first batch").unwrap();
    assert!(first.rows() > 0);

    // The job is visible by program_name while it streams (and by its spid).
    let snap = monitor.snapshot(&handle).await.unwrap();
    assert!(snap.found, "job session should be found by program_name");
    assert_eq!(snap.pid.as_deref(), Some(spid.as_str()));
    assert!(snap.state.is_some());

    cancel.cancel();
    let mut ended_with_cancel = false;
    let deadline = Instant::now() + Duration::from_secs(20);
    while let Some(item) = stream.next().await {
        if let Err(CoreError::Cancelled) = item {
            ended_with_cancel = true;
            break;
        }
        assert!(Instant::now() < deadline, "stream did not stop after cancel");
    }
    assert!(ended_with_cancel, "stream must end with CANCELLED");
    assert!(db.session_id().is_none(), "the dropped connection forgets its spid");

    // The server notices the closed socket and the session disappears.
    wait_for_snapshot(&monitor, &handle, "the cancelled session to disappear", Duration::from_secs(30), |s| !s.found).await;

    // The same object reconnects transparently for the next call.
    let info = db.probe().await.unwrap();
    assert_eq!(info.driver, "mssql");
    assert!(db.session_id().is_some(), "the reconnected object knows its new spid");
    // The SPID may well be the same number again (SQL Server reuses a freed SPID at once);
    // the physical connection must be a new one.
    assert_ne!(connection_guid(&db).await, connection_before, "calls after cancel must run on a new connection");
}

#[tokio::test]
async fn monitor_finds_job_by_program_name() {
    let app = unique("bulkdb-it-job");
    let Some(db) = connect(&app).await else { return };
    let Some(monitor) = connect("bulkdb-it-monitor2").await else { return };
    let spid = db.session_id().unwrap();
    // No spid given: program_name alone must do.
    let handle = JobHandle { job_id: "job-monitor".into(), application_name: app.clone(), session_id: None, temp_bytes_at_start: None };

    let cancel = CancellationToken::new();
    let sql = "SELECT a.object_id, b.object_id, c.name FROM sys.all_objects a CROSS JOIN sys.all_objects b CROSS JOIN sys.all_objects c";
    let out = db.stream(&query(sql), &SessionOpts::default(), cancel.clone()).await.unwrap();
    let mut stream = out.stream;
    let _ = stream.next().await.expect("first batch").unwrap();

    // Not reading the stream any further: the server soon waits on ASYNC_NETWORK_IO.
    wait_for_snapshot(&monitor, &handle, "a network wait on the job session", Duration::from_secs(20), |s| {
        s.found && s.wait_event.as_deref().map(|w| w.starts_with("ASYNC_NETWORK_IO")).unwrap_or(false)
    })
    .await;
    let snap = monitor.snapshot(&handle).await.unwrap();
    assert_eq!(snap.pid.as_deref(), Some(spid.as_str()));
    assert!(snap.elapsed_ms.is_some());
    assert!(snap.client_write_ratio.unwrap_or(0.0) > 0.0);

    // active_sessions sees the job through its SQL text and the table it touches.
    let sessions = monitor.active_sessions(&[TableRef::new(Some("sys"), "all_objects")]).await.unwrap();
    let mine = sessions.iter().find(|s| s.pid == spid).expect("job session listed by table name");
    assert_eq!(mine.application.as_deref(), Some(app.as_str()));
    assert!(mine.tables.contains(&"sys.all_objects".to_string()));
    assert!(mine.query.as_deref().unwrap_or("").contains("CROSS JOIN"));
    assert!(monitor.active_sessions(&[TableRef::new(None, "no_such_table_xyz")]).await.unwrap().iter().all(|s| s.pid != spid));

    // KILL from the monitor connection ends the job's stream with an error (needs ALTER ANY CONNECTION).
    match monitor.cancel_session(&spid).await {
        Ok(true) => {
            let mut errored = false;
            while let Some(item) = stream.next().await {
                if item.is_err() {
                    errored = true;
                    break;
                }
            }
            assert!(errored, "killed session must surface an error on the stream");
            wait_for_snapshot(&monitor, &handle, "the killed session to disappear", Duration::from_secs(30), |s| !s.found).await;
        }
        Ok(false) => panic!("cancel_session is implemented for mssql"),
        Err(CoreError::Query { code, msg }) => {
            eprintln!("KILL not permitted for this login ({code:?}: {msg}); cancelling client-side instead");
            cancel.cancel();
            while let Some(item) = stream.next().await {
                if item.is_err() {
                    break;
                }
            }
        }
        Err(other) => panic!("unexpected {other:?}"),
    }
    assert!(matches!(monitor.cancel_session("not-a-spid").await, Err(CoreError::Invalid(_))));
}

// ---------------------------------------------------------------------------------------------
// Probe, catalog and EXPLAIN
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn probe_reports_server_facts() {
    let Some(db) = connect("bulkdb-it-probe").await else { return };
    let info = db.probe().await.unwrap();
    assert_eq!(info.driver, "mssql");
    assert!(info.version.contains("SQL Server"), "{}", info.version);
    assert!(info.user.is_some());
    assert!(info.database.is_some());
    assert!(info.settings.contains_key("product_version"));
    assert!(info.can_write.is_some());
    if let Some(ram) = info.ram_bytes {
        assert!(ram > 64 * 1024 * 1024, "{ram}");
        assert!(info.ram_source.is_some());
    }
}

#[tokio::test]
async fn catalog_matches_information_schema() {
    let Some(db) = connect("bulkdb-it-catalog").await else { return };
    ensure_schema(&db).await;
    let parent = unique("cat_parent");
    let table = unique("cat_child");
    drop_table(&db, &table).await;
    drop_table(&db, &parent).await;
    db.execute_batch(&format!(
        "CREATE TABLE [{SCHEMA}].[{parent}] (id int NOT NULL CONSTRAINT [PK_{parent}] PRIMARY KEY, label nvarchar(20) NULL); \
         CREATE TABLE [{SCHEMA}].[{table}] ( \
             id int IDENTITY(1,1) NOT NULL CONSTRAINT [PK_{table}] PRIMARY KEY CLUSTERED, \
             parent_id int NOT NULL CONSTRAINT [FK_{table}_parent] FOREIGN KEY REFERENCES [{SCHEMA}].[{parent}](id), \
             code varchar(50) NOT NULL CONSTRAINT [UQ_{table}_code] UNIQUE, \
             title nvarchar(max) NULL, \
             amount decimal(12,2) NOT NULL CONSTRAINT [DF_{table}_amount] DEFAULT ((0)), \
             created datetime2(3) NOT NULL DEFAULT (sysutcdatetime()), \
             payload varbinary(max) NULL, \
             [we]]ird] bit NULL); \
         CREATE NONCLUSTERED INDEX [IX_{table}_parent] ON [{SCHEMA}].[{table}] (parent_id ASC, created DESC) INCLUDE (amount) WHERE ([title] IS NOT NULL); \
         EXEC sys.sp_addextendedproperty @name = N'MS_Description', @value = N'child rows', @level0type = N'SCHEMA', @level0name = N'{SCHEMA}', @level1type = N'TABLE', @level1name = N'{table}'; \
         EXEC sys.sp_addextendedproperty @name = N'MS_Description', @value = N'the code', @level0type = N'SCHEMA', @level0name = N'{SCHEMA}', @level1type = N'TABLE', @level1name = N'{table}', @level2type = N'COLUMN', @level2name = N'code'; \
         INSERT INTO [{SCHEMA}].[{parent}] (id, label) VALUES (1, N'one'), (2, N'two'); \
         INSERT INTO [{SCHEMA}].[{table}] (parent_id, code, title, amount) VALUES (1, 'a', N't', 1.5), (1, 'b', NULL, 2), (2, 'c', N'u', 3);"
    ))
    .await
    .unwrap();
    let t = tref(&table);

    // databases / schemas
    let dbs = db.databases().await.unwrap();
    let current = dbs.iter().find(|d| d.is_current).expect("current database listed");
    assert!(current.accessible);
    let schemas = db.schemas(&current.name).await.unwrap();
    assert!(schemas.iter().all(|s| s.name != "sys" && s.name != "INFORMATION_SCHEMA" && !s.name.starts_with("db_")));
    let mine = schemas.iter().find(|s| s.name == SCHEMA).expect("bulkdb_it schema listed");
    assert!(mine.table_count.unwrap_or(0) >= 2);

    // tables (with estimates) and a view
    let tables = db.tables(&current.name, SCHEMA).await.unwrap();
    let ti = tables.iter().find(|x| x.name == table).expect("table listed");
    assert_eq!(ti.kind, TableKind::Table);
    assert_eq!(ti.schema, SCHEMA);
    assert_eq!(ti.comment.as_deref(), Some("child rows"));
    if let Some(est) = ti.estimated_rows {
        assert!(est <= 3, "estimate {est} for 3 rows");
    }

    // columns vs INFORMATION_SCHEMA
    let cols = db.columns(&t).await.unwrap();
    let is_sql = format!(
        "SELECT COLUMN_NAME, DATA_TYPE, IS_NULLABLE, CAST(ORDINAL_POSITION AS int) FROM INFORMATION_SCHEMA.COLUMNS WHERE TABLE_SCHEMA = '{SCHEMA}' AND TABLE_NAME = '{table}' ORDER BY ORDINAL_POSITION"
    );
    let batches = drain(&db, &is_sql, &SessionOpts::default()).await;
    let expected: Vec<(String, String, bool, i64)> = batches
        .iter()
        .flat_map(|b| {
            let names = str_col(b, 0);
            let types = str_col(b, 1);
            let nulls = str_col(b, 2);
            let pos = i64_col(b, 3);
            (0..b.num_rows()).map(move |i| (names.value(i).to_string(), types.value(i).to_string(), nulls.value(i) == "YES", pos.value(i))).collect::<Vec<_>>()
        })
        .collect();
    assert_eq!(cols.len(), expected.len());
    for (c, (name, data_type, nullable, pos)) in cols.iter().zip(&expected) {
        assert_eq!(&c.name, name);
        assert_eq!(c.nullable, *nullable, "{name}");
        assert_eq!(c.position as i64, *pos, "{name}");
        assert!(c.data_type.starts_with(data_type.as_str()), "{name}: {} vs {data_type}", c.data_type);
    }
    let by_name = |n: &str| cols.iter().find(|c| c.name == n).unwrap_or_else(|| panic!("column {n}"));
    assert!(by_name("id").primary_key);
    assert_eq!(by_name("code").data_type, "varchar(50)");
    assert!(by_name("code").unique);
    assert_eq!(by_name("code").comment.as_deref(), Some("the code"));
    assert_eq!(by_name("title").data_type, "nvarchar(max)");
    assert_eq!(by_name("amount").data_type, "decimal(12,2)");
    assert_eq!(by_name("amount").default.as_deref(), Some("((0))"));
    assert_eq!(by_name("created").data_type, "datetime2(3)");
    assert_eq!(by_name("payload").data_type, "varbinary(max)");
    assert_eq!(by_name("parent_id").foreign_key.as_deref(), Some(format!("{SCHEMA}.{parent}(id)").as_str()));
    assert_eq!(by_name("we]ird").data_type, "bit");

    // indexes
    let idx = db.indexes(&t).await.unwrap();
    let pk = idx.iter().find(|i| i.primary).expect("primary key index");
    assert_eq!(pk.name, format!("PK_{table}"));
    assert_eq!(pk.columns, vec!["id"]);
    assert_eq!(pk.kind.as_deref(), Some("clustered"));
    let uq = idx.iter().find(|i| i.name == format!("UQ_{table}_code")).expect("unique constraint index");
    assert!(uq.unique);
    let ix = idx.iter().find(|i| i.name == format!("IX_{table}_parent")).expect("filtered index");
    assert_eq!(ix.columns, vec!["parent_id", "created"]);
    assert_eq!(ix.include, vec!["amount"]);
    assert!(ix.predicate.as_deref().unwrap_or("").contains("IS NOT NULL"));
    assert!(!ix.unique);
    let def = ix.definition.as_deref().unwrap();
    assert!(def.contains("INCLUDE ([amount])") && def.contains("[created] DESC"), "{def}");

    // ddl, exact count, quoting
    let ddl = db.ddl(&t).await.unwrap();
    assert!(ddl.starts_with(&format!("CREATE TABLE [{SCHEMA}].[{table}] (")), "{ddl}");
    assert!(ddl.contains("[id] int IDENTITY(1,1) NOT NULL"), "{ddl}");
    assert!(ddl.contains(&format!("CONSTRAINT [PK_{table}] PRIMARY KEY CLUSTERED ([id] ASC)")), "{ddl}");
    assert!(ddl.contains(&format!("CREATE NONCLUSTERED INDEX [IX_{table}_parent]")), "{ddl}");
    assert!(ddl.contains("[we]]ird] bit NULL"), "{ddl}");
    assert_eq!(db.exact_count(&t, CancellationToken::new()).await.unwrap(), 3);
    assert!(matches!(db.exact_count(&tref("no_such_table_xyz"), CancellationToken::new()).await, Err(CoreError::Query { .. })));
    assert_eq!(Catalog::quote_ident(&db, "we]ird"), "[we]]ird]");
    let select = db.select_all_sql(&t, &cols);
    assert!(select.starts_with("SELECT [id],"), "{select}");
    assert!(select.ends_with(&format!("FROM [{SCHEMA}].[{table}]")), "{select}");
    assert!(select.contains("[we]]ird]"));

    // a view comes back with its definition
    let view = unique("cat_view");
    db.execute_batch(&format!("CREATE VIEW [{SCHEMA}].[{view}] AS SELECT id, code FROM [{SCHEMA}].[{table}]")).await.unwrap();
    let tables = db.tables(&current.name, SCHEMA).await.unwrap();
    assert_eq!(tables.iter().find(|x| x.name == view).map(|x| x.kind), Some(TableKind::View));
    let vdef = db.ddl(&tref(&view)).await.unwrap();
    assert!(vdef.contains("CREATE VIEW"), "{vdef}");
    assert!(matches!(db.columns(&tref("no_such_table_xyz")).await, Err(CoreError::NotFound(_))));

    // EXPLAIN (estimated and actual)
    let sql = format!("SELECT c.id, p.label FROM [{SCHEMA}].[{table}] c JOIN [{SCHEMA}].[{parent}] p ON p.id = c.parent_id");
    let plan = db.explain(&sql, false).await.unwrap().expect("plan");
    assert_eq!(plan.raw_format, "xml");
    assert!(plan.raw.starts_with("<ShowPlanXML"));
    assert!(!plan.analyzed);
    assert!(plan.root.is_some());
    let names: Vec<String> = plan.tables.iter().map(|t| t.qualified()).collect();
    assert!(names.contains(&format!("{SCHEMA}.{table}")) && names.contains(&format!("{SCHEMA}.{parent}")), "{names:?}");
    let actual = db.explain(&sql, true).await.unwrap().expect("actual plan");
    assert!(actual.analyzed);
    assert!(actual.root.as_ref().and_then(|r| r.actual_rows).is_some(), "actual counters parsed");
    // Session options survive EXPLAIN: the connection still works and streams.
    assert_eq!(drain(&db, &sql, &SessionOpts::default()).await.iter().map(|b| b.num_rows()).sum::<usize>(), 3);

    db.execute_batch(&format!("DROP VIEW [{SCHEMA}].[{view}]")).await.unwrap();
    drop_table(&db, &table).await;
    drop_table(&db, &parent).await;
}
