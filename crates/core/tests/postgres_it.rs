//! PostgreSQL integration tests against a real server (docker in CI):
//!
//! ```text
//! BULKDB_PG_URL=postgres://bulkdb:bulkdb@127.0.0.1:5432/bulkdb \
//!   cargo test -p bulk-core --features it --test postgres_it
//! ```
//!
//! Every test passes (skips) when `BULKDB_PG_URL` is unset. Each test creates its own schema
//! `bulkdb_it_<pid>_<nanos>` and drops it at the end, so tests run in parallel.
#![cfg(feature = "it")]

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use arrow_array::{Array, BooleanArray, Date32Array, Decimal128Array, Float64Array, Int64Array, StringArray, TimestampMicrosecondArray};
use arrow_schema::{DataType, TimeUnit};
use bulk_core::catalog::Catalog;
use bulk_core::error::CoreError;
use bulk_core::job::{run_pipeline, Progress};
use bulk_core::monitor::{JobHandle, Monitor};
use bulk_core::sink::{Sink, SinkReport};
use bulk_core::source::postgres::PgDatabase;
use bulk_core::source::{Batch, ConnectParams, CsvFormat, Query, Source, SslMode, StreamOutput};
use bulk_core::types::{SessionOpts, TableKind, TableRef};
use futures::StreamExt;
use tokio_util::sync::CancellationToken;

const ROWS: i64 = 100_000;

/// A query that streams for minutes. The set-returning function sits in the select list
/// (`ProjectSet`, value per call) so rows flow at once; in `FROM` the server would first
/// materialise all 200M rows into a tuplestore.
const ENDLESS: &str = "SELECT g, md5(g::text) AS h FROM (SELECT generate_series(1, 200000000) AS g) AS s";

// ---------------------------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------------------------

fn pg_url() -> Option<String> {
    match std::env::var("BULKDB_PG_URL") {
        Ok(u) if !u.trim().is_empty() => Some(u),
        _ => {
            eprintln!("BULKDB_PG_URL not set: skipping");
            None
        }
    }
}

/// `postgres://user:pass@host:port/db` -> `ConnectParams` (no percent decoding; CI URLs are
/// plain).
fn params_from_url(url: &str, application_name: &str) -> ConnectParams {
    let rest = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    let (rest, _query) = rest.split_once('?').unwrap_or((rest, ""));
    let (auth, hostdb) = match rest.rsplit_once('@') {
        Some((a, h)) => (Some(a), h),
        None => (None, rest),
    };
    let (hostport, db) = hostdb.split_once('/').unwrap_or((hostdb, ""));
    let (host, port) = match hostport.rsplit_once(':') {
        Some((h, p)) => (h, p.parse().unwrap_or(5432)),
        None => (hostport, 5432),
    };
    let (user, password) = match auth {
        Some(a) => match a.split_once(':') {
            Some((u, p)) => (Some(u.to_string()), Some(p.to_string())),
            None => (Some(a.to_string()), None),
        },
        None => (None, None),
    };
    ConnectParams {
        driver: "postgres".into(),
        host: if host.is_empty() { "127.0.0.1".into() } else { host.to_string() },
        port,
        database: Some(db.to_string()).filter(|d| !d.is_empty()),
        user,
        password,
        fields: BTreeMap::new(),
        application_name: application_name.into(),
        connect_timeout: Duration::from_secs(15),
        ssl: SslMode::Prefer,
    }
}

async fn connect(url: &str, application_name: &str) -> PgDatabase {
    PgDatabase::connect(&params_from_url(url, application_name)).await.expect("connect to BULKDB_PG_URL")
}

/// Raw client (setup, reference COPY, pg_stat_activity checks) plus a private schema.
struct Fixture {
    raw: tokio_postgres::Client,
    schema: String,
}

impl Fixture {
    async fn new(url: &str) -> Fixture {
        let (raw, conn) = tokio_postgres::connect(url, tokio_postgres::NoTls).await.expect("raw connect");
        tokio::spawn(async move {
            let _ = conn.await;
        });
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let schema = format!("bulkdb_it_{}_{}", std::process::id(), nanos % 1_000_000_000);
        raw.batch_execute(&format!("CREATE SCHEMA {schema}")).await.expect("create schema");
        Fixture { raw, schema }
    }

    /// `schema.nasty`: the testdata "nasty" shape (commas, quotes, newlines, CRLF, empty,
    /// NULL, unicode) generated server-side, analyzed so `reltuples` is meaningful.
    async fn nasty_table(&self, rows: i64) {
        let s = &self.schema;
        let sql = format!(
            "CREATE TABLE {s}.nasty (id int8 PRIMARY KEY, text text, num numeric(12,3), note text, flag boolean NOT NULL, unicode text); \
             INSERT INTO {s}.nasty \
             SELECT i, \
                    CASE i % 8 WHEN 0 THEN 'plain' WHEN 1 THEN 'has,comma' WHEN 2 THEN 'has \"quote\" inside' \
                               WHEN 3 THEN E'line1\\nline2' WHEN 4 THEN E'crlf\\r\\nline' WHEN 5 THEN '' WHEN 6 THEN NULL \
                               ELSE ' leading and trailing ' END, \
                    CASE WHEN i % 10 = 0 THEN NULL ELSE ((i * 7919) % 100000) + ((i % 1000) / 1000.0) END, \
                    CASE WHEN i % 4 = 0 THEN NULL ELSE 'note ' || (i % 1000) END, \
                    i % 2 = 0, \
                    CASE i % 5 WHEN 0 THEN '한글 텍스트' WHEN 1 THEN 'emoji 🚀🎉' WHEN 2 THEN '日本語, 中文' WHEN 3 THEN NULL ELSE 'ünïcödé' END \
               FROM generate_series(1, {rows}) AS i; \
             COMMENT ON TABLE {s}.nasty IS 'it''s nasty'; \
             COMMENT ON COLUMN {s}.nasty.num IS 'three decimals'; \
             CREATE INDEX nasty_flag_idx ON {s}.nasty (flag, lower(note)) INCLUDE (text) WHERE flag; \
             CREATE TABLE {s}.child (id int8 PRIMARY KEY, parent_id int8 REFERENCES {s}.nasty(id), tag text UNIQUE); \
             ANALYZE {s}.nasty"
        );
        self.raw.batch_execute(&sql).await.expect("create nasty table");
    }

    async fn copy_reference(&self, sql: &str) -> Vec<u8> {
        let copy = format!("COPY ({sql}) TO STDOUT (FORMAT csv, HEADER true, DELIMITER ',', NULL '', QUOTE '\"', ESCAPE '\"')");
        let stream = self.raw.copy_out(copy.as_str()).await.expect("reference copy");
        futures::pin_mut!(stream);
        let mut out = Vec::new();
        while let Some(m) = stream.next().await {
            out.extend_from_slice(&m.expect("reference copy chunk"));
        }
        out
    }

    async fn session_state(&self, pid: i32) -> Option<String> {
        let rows = self.raw.query("SELECT state::text FROM pg_stat_activity WHERE pid = $1", &[&pid]).await.expect("pg_stat_activity");
        rows.first().and_then(|r| r.get::<_, Option<String>>(0))
    }

    /// Poll until the backend is idle or gone (the query was really cancelled server-side).
    async fn wait_until_idle(&self, pid: i32) -> bool {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            match self.session_state(pid).await.as_deref() {
                None | Some("idle") => return true,
                _ => tokio::time::sleep(Duration::from_millis(100)).await,
            }
        }
        false
    }

    async fn drop_schema(&self) {
        let _ = self.raw.batch_execute(&format!("DROP SCHEMA {} CASCADE", self.schema)).await;
    }

    fn table(&self, name: &str) -> TableRef {
        TableRef::new(Some(&self.schema), name)
    }
}

fn csv_query(sql: &str, fmt: CsvFormat) -> Query {
    Query { sql: sql.to_string(), output: StreamOutput::RawCsv(fmt) }
}

fn arrow_query(sql: &str) -> Query {
    Query { sql: sql.to_string(), output: StreamOutput::Arrow }
}

// ---------------------------------------------------------------------------------------------
// Source
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn csv_stream_matches_reference_copy() {
    let Some(url) = pg_url() else { return };
    let fx = Fixture::new(&url).await;
    fx.nasty_table(ROWS).await;
    let db = connect(&url, "bulkdb-it-csv").await;
    db.apply_session(&SessionOpts { work_mem: Some("16MB".into()), statement_timeout_ms: Some(120_000), ..Default::default() }).await.unwrap();

    let sql = format!("SELECT * FROM {}.nasty ORDER BY id", fx.schema);
    let reference = fx.copy_reference(&sql).await;

    let out = db.stream(&csv_query(&sql, CsvFormat::default()), &SessionOpts::default(), CancellationToken::new()).await.unwrap();
    assert!(out.schema.is_none(), "raw CSV carries no Arrow schema");
    let mut stream = out.stream;
    let mut bytes = Vec::new();
    let (mut headers, mut rows, mut batches) = (0u64, 0u64, 0u64);
    while let Some(b) = stream.next().await {
        match b.unwrap() {
            Batch::RawCsvHeader(h) => {
                headers += 1;
                assert_eq!(rows, 0, "header must come first");
                bytes.extend_from_slice(&h);
            }
            Batch::RawCsv { data, rows: n } => {
                batches += 1;
                rows += n;
                assert!(n <= bulk_core::BATCH_ROWS as u64);
                assert!(data.len() <= bulk_core::BATCH_BYTES + 64 * 1024, "batch bytes stay bounded");
                bytes.extend_from_slice(&data);
            }
            Batch::Arrow(_) => panic!("Arrow batch on the raw CSV path"),
        }
    }
    assert_eq!(headers, 1);
    assert_eq!(rows, ROWS as u64);
    assert!(batches >= 2, "100k rows must be cut into several batches (got {batches})");
    assert_eq!(bytes.len(), reference.len());
    assert!(bytes == reference, "streamed CSV bytes differ from the server's own COPY output");

    // CRLF: every batch ends with \r\n and the row count is unchanged.
    let fmt = CsvFormat { crlf: true, header: false, delimiter: b'|', null_as: "\\N".into() };
    let out = db.stream(&csv_query(&sql, fmt), &SessionOpts::default(), CancellationToken::new()).await.unwrap();
    let mut stream = out.stream;
    let mut rows = 0u64;
    while let Some(b) = stream.next().await {
        match b.unwrap() {
            Batch::RawCsv { data, rows: n } => {
                rows += n;
                assert!(data.ends_with(b"\r\n"));
                assert!(data.starts_with(b"1|") || rows > n, "{:?}", &data[..20]);
            }
            other => panic!("unexpected {other:?}"),
        }
    }
    assert_eq!(rows, ROWS as u64);
    fx.drop_schema().await;
}

#[tokio::test]
async fn arrow_stream_decodes_every_supported_type() {
    let Some(url) = pg_url() else { return };
    let fx = Fixture::new(&url).await;
    let s = &fx.schema;
    fx.raw
        .batch_execute(&format!(
            "CREATE TABLE {s}.types (b boolean, i2 int2, i4 int4, i8 int8, f4 float4, f8 float8, \
                 n numeric(12,3), nu numeric, t text, vc varchar(10), bp char(3), by bytea, d date, tm time, \
                 ts timestamp, tz timestamptz, u uuid, j json, jb jsonb, arr int4[], iv interval, nm name); \
             INSERT INTO {s}.types VALUES \
               (true, -3, 42, -7, 1.5, 2.5, -123.450, 1234567890.123456789, 'héllo', 'vc', 'ab', '\\xdead', \
                '2024-02-29', '01:00:00', '2024-02-29 12:34:56', '2024-02-29 12:34:56+00', \
                '123e4567-e89b-12d3-a456-426614174000', '{{\"a\": 1}}', '{{\"b\": [1, 2]}}', '{{1,2,3}}', '1 day 02:00:00', 'nm'), \
               (NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL)"
        ))
        .await
        .expect("create types table");
    let db = connect(&url, "bulkdb-it-arrow").await;
    db.apply_session(&SessionOpts::default()).await.unwrap();
    let out = db.stream(&arrow_query(&format!("SELECT * FROM {s}.types ORDER BY b NULLS LAST")), &SessionOpts::default(), CancellationToken::new()).await.unwrap();
    let schema = out.schema.expect("Arrow path knows the schema up front");
    let expect: Vec<(&str, DataType)> = vec![
        ("b", DataType::Boolean),
        ("i2", DataType::Int64),
        ("i4", DataType::Int64),
        ("i8", DataType::Int64),
        ("f4", DataType::Float64),
        ("f8", DataType::Float64),
        ("n", DataType::Decimal128(12, 3)),
        ("nu", DataType::Utf8),
        ("t", DataType::Utf8),
        ("vc", DataType::Utf8),
        ("bp", DataType::Utf8),
        ("by", DataType::Binary),
        ("d", DataType::Date32),
        ("tm", DataType::Time64(TimeUnit::Microsecond)),
        ("ts", DataType::Timestamp(TimeUnit::Microsecond, None)),
        ("tz", DataType::Timestamp(TimeUnit::Microsecond, Some("UTC".into()))),
        ("u", DataType::Utf8),
        ("j", DataType::Utf8),
        ("jb", DataType::Utf8),
        ("arr", DataType::Utf8),
        ("iv", DataType::Utf8),
        ("nm", DataType::Utf8),
    ];
    assert_eq!(schema.fields().len(), expect.len());
    for (f, (name, ty)) in schema.fields().iter().zip(&expect) {
        assert_eq!(f.name(), name);
        assert_eq!(f.data_type(), ty, "column {name}");
    }
    let mut stream = out.stream;
    let mut batches = Vec::new();
    while let Some(b) = stream.next().await {
        match b.unwrap() {
            Batch::Arrow(rb) => batches.push(rb),
            other => panic!("unexpected {other:?}"),
        }
    }
    assert_eq!(batches.len(), 1);
    let rb = &batches[0];
    assert_eq!(rb.num_rows(), 2);
    let col = |i: usize| rb.column(i);
    assert!(col(0).as_any().downcast_ref::<BooleanArray>().unwrap().value(0));
    assert_eq!(col(1).as_any().downcast_ref::<Int64Array>().unwrap().value(0), -3);
    assert_eq!(col(2).as_any().downcast_ref::<Int64Array>().unwrap().value(0), 42);
    assert_eq!(col(3).as_any().downcast_ref::<Int64Array>().unwrap().value(0), -7);
    assert_eq!(col(4).as_any().downcast_ref::<Float64Array>().unwrap().value(0), 1.5);
    assert_eq!(col(5).as_any().downcast_ref::<Float64Array>().unwrap().value(0), 2.5);
    assert_eq!(col(6).as_any().downcast_ref::<Decimal128Array>().unwrap().value(0), -123_450);
    let strs = |i: usize| col(i).as_any().downcast_ref::<StringArray>().unwrap().value(0).to_string();
    assert_eq!(strs(7), "1234567890.123456789");
    assert_eq!(strs(8), "héllo");
    assert_eq!(strs(9), "vc");
    assert_eq!(strs(10), "ab ");
    assert_eq!(col(11).as_any().downcast_ref::<arrow_array::BinaryArray>().unwrap().value(0), &[0xde, 0xad]);
    assert_eq!(col(12).as_any().downcast_ref::<Date32Array>().unwrap().value(0), 19_782); // 2024-02-29
    assert_eq!(col(13).as_any().downcast_ref::<arrow_array::Time64MicrosecondArray>().unwrap().value(0), 3_600_000_000);
    assert_eq!(col(14).as_any().downcast_ref::<TimestampMicrosecondArray>().unwrap().value(0), 1_709_210_096_000_000);
    assert_eq!(col(15).as_any().downcast_ref::<TimestampMicrosecondArray>().unwrap().value(0), 1_709_210_096_000_000);
    assert_eq!(strs(16), "123e4567-e89b-12d3-a456-426614174000");
    assert_eq!(strs(17), "{\"a\": 1}");
    assert_eq!(strs(18), "{\"b\": [1, 2]}");
    assert_eq!(strs(19), "{1,2,3}");
    assert_eq!(strs(20), "1 day 02:00:00");
    assert_eq!(strs(21), "nm");
    for i in 0..expect.len() {
        assert!(col(i).is_null(1), "column {i} must be NULL in the second row");
    }

    // Duplicate column names fall back to positional aliases.
    let out = db.stream(&arrow_query("SELECT 1 AS x, '{\"k\":true}'::jsonb AS x"), &SessionOpts::default(), CancellationToken::new()).await.unwrap();
    let names: Vec<String> = out.schema.unwrap().fields().iter().map(|f| f.name().clone()).collect();
    assert_eq!(names, vec!["x", "x"]);
    let mut stream = out.stream;
    let rb = match stream.next().await.unwrap().unwrap() {
        Batch::Arrow(rb) => rb,
        other => panic!("unexpected {other:?}"),
    };
    assert_eq!(rb.column(1).as_any().downcast_ref::<StringArray>().unwrap().value(0), "{\"k\": true}");
    fx.drop_schema().await;
}

#[tokio::test]
async fn cancel_mid_stream_ends_the_server_session() {
    let Some(url) = pg_url() else { return };
    let fx = Fixture::new(&url).await;
    let db = connect(&url, "bulkdb-it-cancel").await;
    let pid = db.backend_pid();
    for output in [StreamOutput::RawCsv(CsvFormat::default()), StreamOutput::Arrow] {
        let cancel = CancellationToken::new();
        let q = Query { sql: ENDLESS.into(), output };
        let out = db.stream(&q, &SessionOpts::default(), cancel.clone()).await.unwrap();
        let mut stream = out.stream;
        let first = stream.next().await.expect("first batch").expect("first batch ok");
        assert!(matches!(first, Batch::RawCsvHeader(_) | Batch::RawCsv { .. } | Batch::Arrow(_)));
        cancel.cancel();
        let mut saw_cancelled = false;
        for _ in 0..1000 {
            match stream.next().await {
                Some(Err(CoreError::Cancelled)) => {
                    saw_cancelled = true;
                    break;
                }
                Some(Err(e)) => panic!("unexpected error after cancel: {e}"),
                Some(Ok(_)) => continue,
                None => break,
            }
        }
        assert!(saw_cancelled, "stream must end with Cancelled");
        assert!(stream.next().await.is_none(), "stream is fused after the error");
        assert!(fx.wait_until_idle(pid).await, "backend {pid} is still running the query after cancel");
    }
    // The connection is still usable afterwards.
    let info = db.probe().await.unwrap();
    assert_eq!(info.driver, "postgres");
    assert!(info.ram_bytes.is_some());
    assert_eq!(info.ram_source.as_deref(), Some("shared_buffers x 4"));
    assert!(info.settings.contains_key("work_mem"));
    fx.drop_schema().await;
}

/// Accepts and discards every batch (the pipeline counts rows itself).
struct NullSink;

/// Fails the first write, like a full disk.
struct DiskFullSink;

impl Sink for NullSink {
    fn write(&mut self, _batch: Batch) -> bulk_core::Result<()> {
        Ok(())
    }
    fn finish(self: Box<Self>) -> bulk_core::Result<SinkReport> {
        Ok(SinkReport::default())
    }
    fn abort(self: Box<Self>) -> Vec<PathBuf> {
        Vec::new()
    }
    fn bytes_out(&self) -> u64 {
        0
    }
    fn files_so_far(&self) -> u32 {
        0
    }
}

impl Sink for DiskFullSink {
    fn write(&mut self, _batch: Batch) -> bulk_core::Result<()> {
        Err(CoreError::Io("No space left on device (test)".into()))
    }
    fn finish(self: Box<Self>) -> bulk_core::Result<SinkReport> {
        Ok(SinkReport::default())
    }
    fn abort(self: Box<Self>) -> Vec<PathBuf> {
        Vec::new()
    }
    fn bytes_out(&self) -> u64 {
        0
    }
    fn files_so_far(&self) -> u32 {
        0
    }
}

/// Dropping a stream mid-COPY without polling it again must cancel the query server-side;
/// tokio-postgres alone would keep draining the COPY to its end.
#[tokio::test]
async fn dropping_the_stream_cancels_the_server_query() {
    let Some(url) = pg_url() else { return };
    let fx = Fixture::new(&url).await;
    let db = connect(&url, "bulkdb-it-drop").await;
    let pid = db.backend_pid();
    for output in [StreamOutput::RawCsv(CsvFormat::default()), StreamOutput::Arrow] {
        let out = db.stream(&Query { sql: ENDLESS.into(), output }, &SessionOpts::default(), CancellationToken::new()).await.unwrap();
        let mut stream = out.stream;
        stream.next().await.expect("first batch").expect("first batch ok");
        drop(stream);
        assert!(fx.wait_until_idle(pid).await, "backend {pid} still runs the COPY after its stream was dropped");
    }
    // The session survives and serves the next statement.
    assert_eq!(db.probe().await.unwrap().driver, "postgres");
    fx.drop_schema().await;
}

/// The real job path: `run_pipeline` drops the stream on cancel (without polling it again) and
/// on a sink failure (no cancel token involved). Both must stop the server-side COPY.
#[tokio::test]
async fn pipeline_cancel_and_sink_failure_stop_the_server_query() {
    let Some(url) = pg_url() else { return };
    let fx = Fixture::new(&url).await;
    let db = connect(&url, "bulkdb-it-pipeline").await;
    let pid = db.backend_pid();
    for output in [StreamOutput::RawCsv(CsvFormat::default()), StreamOutput::Arrow] {
        let q = Query { sql: ENDLESS.into(), output };

        // Cancel through the job token once rows are flowing.
        let cancel = CancellationToken::new();
        let out = db.stream(&q, &SessionOpts::default(), cancel.clone()).await.unwrap();
        let progress = Arc::new(Progress::new());
        let run = tokio::spawn(run_pipeline(out, Box::new(NullSink), progress.clone(), cancel.clone()));
        let deadline = Instant::now() + Duration::from_secs(30);
        while progress.rows.load(Ordering::Relaxed) == 0 {
            assert!(Instant::now() < deadline, "no rows reached the sink");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        cancel.cancel();
        let outcome = run.await.expect("pipeline task");
        assert!(matches!(outcome, Err(CoreError::Cancelled)), "{outcome:?}");
        assert!(fx.wait_until_idle(pid).await, "backend {pid} still runs the COPY after the job was cancelled");

        // Disk full: the writer gives up, the reader drops the stream, nobody cancels a token.
        let out = db.stream(&q, &SessionOpts::default(), CancellationToken::new()).await.unwrap();
        let outcome = run_pipeline(out, Box::new(DiskFullSink), Arc::new(Progress::new()), CancellationToken::new()).await;
        assert!(matches!(outcome, Err(CoreError::Io(_))), "{outcome:?}");
        assert!(fx.wait_until_idle(pid).await, "backend {pid} still runs the COPY after the sink failed");
    }
    assert_eq!(db.probe().await.unwrap().driver, "postgres");
    fx.drop_schema().await;
}

/// `statement_timeout` raises SQLSTATE 57014 like a cancel request; it must end the job as
/// TIMEOUT, never as CANCELLED (which the UI and CLI report as a user cancel).
#[tokio::test]
async fn statement_timeout_fails_as_timeout_not_cancelled() {
    let Some(url) = pg_url() else { return };
    let fx = Fixture::new(&url).await;
    let db = connect(&url, "bulkdb-it-timeout").await;
    let pid = db.backend_pid();
    let opts = SessionOpts { statement_timeout_ms: Some(1_000), ..Default::default() };
    db.apply_session(&opts).await.unwrap();
    for output in [StreamOutput::RawCsv(CsvFormat::default()), StreamOutput::Arrow] {
        let out = db.stream(&Query { sql: ENDLESS.into(), output }, &opts, CancellationToken::new()).await.unwrap();
        let mut stream = out.stream;
        let mut terminal = None;
        while let Some(item) = stream.next().await {
            if let Err(e) = item {
                terminal = Some(e);
                break;
            }
        }
        assert!(matches!(terminal, Some(CoreError::Timeout(_))), "expected TIMEOUT, got {terminal:?}");
        assert!(stream.next().await.is_none(), "stream is fused after the error");
        assert!(fx.wait_until_idle(pid).await);
    }
    fx.drop_schema().await;
}

/// Drain `q` and return its terminal error: from `stream()` itself (the server failed before
/// the COPY started) or from the stream (it failed afterwards). `None` when it succeeded.
async fn terminal_error(db: &PgDatabase, q: &Query, opts: &SessionOpts) -> Option<CoreError> {
    let out = match db.stream(q, opts, CancellationToken::new()).await {
        Ok(out) => out,
        Err(e) => return Some(e),
    };
    let mut stream = out.stream;
    while let Some(item) = stream.next().await {
        if let Err(e) = item {
            assert!(stream.next().await.is_none(), "stream is fused after the error");
            return Some(e);
        }
    }
    None
}

/// A statement that sleeps past `statement_timeout` before producing a single row (unlike the
/// streaming case above, nothing has been sent when the timer fires) also ends as TIMEOUT,
/// not CANCELLED, on both output paths, from the source and through the job pipeline.
#[tokio::test]
async fn statement_timeout_on_a_silent_statement_is_timeout() {
    let Some(url) = pg_url() else { return };
    let fx = Fixture::new(&url).await;
    let db = connect(&url, "bulkdb-it-sleep-timeout").await;
    let pid = db.backend_pid();
    let opts = SessionOpts { statement_timeout_ms: Some(500), ..Default::default() };
    db.apply_session(&opts).await.unwrap();
    for output in [StreamOutput::RawCsv(CsvFormat::default()), StreamOutput::Arrow] {
        let q = Query { sql: "select pg_sleep(5)".into(), output };

        let started = Instant::now();
        let err = terminal_error(&db, &q, &opts).await;
        let elapsed = started.elapsed();
        assert!(matches!(err, Some(CoreError::Timeout(_))), "{:?}: expected TIMEOUT, got {err:?}", q.output);
        assert_eq!(err.as_ref().map(CoreError::code), Some("TIMEOUT"));
        assert!(elapsed >= Duration::from_millis(500), "failed before the timeout could fire ({elapsed:?})");
        assert!(elapsed < Duration::from_secs(4), "the 500 ms timeout did not stop pg_sleep(5) ({elapsed:?})");
        assert!(fx.wait_until_idle(pid).await, "backend {pid} still busy after the timeout");

        // The job path reports the same outcome.
        let outcome = match db.stream(&q, &opts, CancellationToken::new()).await {
            Ok(out) => run_pipeline(out, Box::new(NullSink), Arc::new(Progress::new()), CancellationToken::new()).await,
            Err(e) => Err(e),
        };
        assert!(matches!(outcome, Err(CoreError::Timeout(_))), "{:?}: expected TIMEOUT, got {outcome:?}", q.output);
        assert!(fx.wait_until_idle(pid).await);
    }
    // The session survives the timeouts.
    assert_eq!(db.probe().await.unwrap().driver, "postgres");
    fx.drop_schema().await;
}

/// One CSV cell of a one-row, one-column query run on `db` itself (no header, no quoting
/// needed for the setting values read here).
async fn scalar(db: &PgDatabase, sql: &str) -> String {
    let fmt = CsvFormat { header: false, ..CsvFormat::default() };
    let out = db.stream(&csv_query(sql, fmt), &SessionOpts::default(), CancellationToken::new()).await.expect("scalar query");
    let mut stream = out.stream;
    let mut bytes = Vec::new();
    while let Some(b) = stream.next().await {
        match b.expect("scalar batch") {
            Batch::RawCsv { data, .. } => bytes.extend_from_slice(&data),
            other => panic!("unexpected {other:?}"),
        }
    }
    String::from_utf8(bytes).expect("utf-8").trim_end().to_string()
}

/// EXPLAIN goes through the extended protocol: the server parses exactly one statement, so a
/// text with a second statement is refused before anything runs. Under the simple protocol
/// the extra statements would execute, including a `COMMIT` that ends EXPLAIN's transaction
/// (so the rollback no longer undoes what follows) and a `nextval` no rollback undoes.
#[tokio::test]
async fn explain_refuses_multiple_statements_without_executing_them() {
    let Some(url) = pg_url() else { return };
    let fx = Fixture::new(&url).await;
    let s = fx.schema.clone();
    fx.raw.batch_execute(&format!("CREATE SEQUENCE {s}.explain_seq")).await.expect("create sequence");
    let db = connect(&url, "bulkdb-it-explain-multi").await;
    let pid = db.backend_pid();
    let attempts = [
        "select 1; select 2".to_string(),
        format!("SELECT 1; COMMIT; CREATE TABLE {s}.explain_pwned (x int)"),
        format!("SELECT 1; SELECT nextval('{s}.explain_seq')"),
        format!("SELECT nextval('{s}.explain_seq'); SELECT 1"),
    ];
    for analyze in [false, true] {
        for sql in &attempts {
            match db.explain(sql, analyze).await {
                Err(CoreError::Query { code: Some(ref c), .. }) if c == "42601" => {}
                other => panic!("EXPLAIN (analyze {analyze}) of {sql:?} must be refused with 42601, got {other:?}"),
            }
        }
    }
    let row = fx.raw.query_one(&format!("SELECT to_regclass('{s}.explain_pwned') IS NULL, is_called FROM {s}.explain_seq"), &[]).await.expect("check side effects");
    assert!(row.get::<_, bool>(0), "a statement after the first one ran: the table exists");
    assert!(!row.get::<_, bool>(1), "nextval() ran: the sequence advanced");

    // The refused EXPLAIN left no transaction behind, and single statements still work,
    // including a trailing semicolon and a semicolon inside a literal.
    assert!(fx.wait_until_idle(pid).await, "session left in a transaction");
    assert!(db.explain("select 1;", false).await.unwrap().is_some());
    let plan = db.explain("SELECT ';' AS semi", true).await.unwrap().expect("plan");
    assert!(plan.analyzed);
    assert_eq!(plan.root.and_then(|r| r.actual_rows), Some(1.0));
    fx.drop_schema().await;
}

/// The SET LOCAL timeouts are scoped to EXPLAIN's transaction: a lock wait ends as TIMEOUT
/// (55P03) after `lock_timeout`, and neither a finished nor an abandoned EXPLAIN leaves the
/// session in a transaction or with EXPLAIN's timeouts.
#[tokio::test]
async fn explain_lock_wait_times_out_and_never_leaks_its_transaction() {
    let Some(url) = pg_url() else { return };
    let fx = Fixture::new(&url).await;
    fx.nasty_table(100).await;
    let db = connect(&url, "bulkdb-it-explain-lock").await;
    let pid = db.backend_pid();
    db.apply_session(&SessionOpts { statement_timeout_ms: Some(120_000), lock_timeout_ms: Some(60_000), ..Default::default() }).await.unwrap();
    let settings = "SELECT current_setting('statement_timeout') || '/' || current_setting('lock_timeout')";
    let session_settings = scalar(&db, settings).await;
    assert_eq!(session_settings, "2min/1min");
    let sql = format!("SELECT * FROM {}.nasty", fx.schema);

    // Another session holds an ACCESS EXCLUSIVE lock: planning waits, lock_timeout = 3s fires.
    fx.raw.batch_execute(&format!("BEGIN; LOCK TABLE {}.nasty IN ACCESS EXCLUSIVE MODE", fx.schema)).await.expect("take lock");
    let started = Instant::now();
    let err = db.explain(&sql, false).await.unwrap_err();
    let elapsed = started.elapsed();
    assert!(matches!(err, CoreError::Timeout(_)), "expected TIMEOUT, got {err:?}");
    assert!(elapsed >= Duration::from_millis(2_500) && elapsed < Duration::from_secs(15), "lock_timeout is 3s, waited {elapsed:?}");
    assert!(fx.wait_until_idle(pid).await, "session left in a transaction after the lock timeout");
    assert_eq!(scalar(&db, settings).await, session_settings, "EXPLAIN's SET LOCAL leaked into the session");

    // Abandon an EXPLAIN while it waits on the lock: the queued ROLLBACK still ends its
    // transaction once the statement finishes.
    let abandoned = tokio::time::timeout(Duration::from_millis(500), db.explain(&sql, false)).await;
    assert!(abandoned.is_err(), "EXPLAIN cannot finish while the table is locked");
    fx.raw.batch_execute("ROLLBACK").await.expect("release lock");
    assert!(fx.wait_until_idle(pid).await, "abandoned EXPLAIN left the session in a transaction");
    assert_eq!(scalar(&db, settings).await, session_settings, "abandoned EXPLAIN's SET LOCAL leaked into the session");

    // Successful EXPLAINs end their transaction as well.
    assert!(db.explain(&sql, false).await.unwrap().is_some());
    assert!(db.explain(&sql, true).await.unwrap().is_some());
    assert_eq!(scalar(&db, settings).await, session_settings);
    assert!(fx.wait_until_idle(pid).await);
    fx.drop_schema().await;
}

#[tokio::test]
async fn explain_parses_plan_and_analyze() {
    let Some(url) = pg_url() else { return };
    let fx = Fixture::new(&url).await;
    fx.nasty_table(1_000).await;
    let db = connect(&url, "bulkdb-it-explain").await;
    let sql = format!("SELECT n.*, (SELECT count(*) FROM {s}.child c WHERE c.parent_id = n.id) FROM {s}.nasty n WHERE n.id < 10", s = fx.schema);
    let plan = db.explain(&sql, false).await.unwrap().expect("postgres has plans");
    assert_eq!(plan.raw_format, "json");
    assert!(!plan.analyzed);
    assert!(plan.estimated_rows.is_some() && plan.estimated_width.is_some() && plan.total_cost.is_some());
    assert!(plan.tables.contains(&fx.table("nasty")), "{:?}", plan.tables);
    assert!(plan.tables.contains(&fx.table("child")), "{:?}", plan.tables);
    let root = plan.root.expect("root node");
    assert!(!root.node_type.is_empty());
    assert!(root.actual_rows.is_none());
    assert!(plan.raw.contains("\"Node Type\""));

    let analyzed = db.explain(&sql, true).await.unwrap().unwrap();
    assert!(analyzed.analyzed);
    let root = analyzed.root.unwrap();
    assert_eq!(root.actual_rows, Some(9.0));
    assert!(root.actual_time_ms.is_some());

    // Errors keep the connection usable (the transaction is rolled back).
    let err = db.explain("SELECT * FROM no_such_table_xyz", false).await.unwrap_err();
    assert!(matches!(err, CoreError::Query { code: Some(ref c), .. } if c == "42P01"), "{err:?}");
    assert!(db.explain("SELECT 1", false).await.unwrap().is_some());
    fx.drop_schema().await;
}

/// The UI sends catalog, monitor and EXPLAIN requests concurrently over one cached browser
/// connection. Requests issued while an EXPLAIN's transaction is open must wait for its
/// ROLLBACK instead of running inside it: there they would see EXPLAIN's `SET LOCAL` timeouts
/// and fail with `25P02` once the EXPLAIN fails (here: a 3 s lock wait).
#[tokio::test]
async fn concurrent_calls_never_run_inside_an_explain_transaction() {
    let Some(url) = pg_url() else { return };
    let fx = Fixture::new(&url).await;
    fx.nasty_table(100).await;
    let db = connect(&url, "bulkdb-it-explain-shared").await;
    let pid = db.backend_pid();
    let settings = "SELECT current_setting('statement_timeout') || '/' || current_setting('lock_timeout')";
    let session_settings = scalar(&db, settings).await;
    let sql = format!("SELECT * FROM {}.nasty", fx.schema);

    fx.raw.batch_execute(&format!("BEGIN; LOCK TABLE {}.nasty IN ACCESS EXCLUSIVE MODE", fx.schema)).await.expect("take lock");
    let explain = db.explain(&sql, false);
    let browse = async {
        // Let the EXPLAIN open its transaction and block on the lock first. Only calls that do
        // not open the locked relation (they would wait for it in autocommit too).
        tokio::time::sleep(Duration::from_millis(500)).await;
        let schemas = db.schemas("").await;
        let databases = db.databases().await;
        let sessions = db.active_sessions(&[fx.table("nasty")]).await;
        let seen = scalar(&db, settings).await;
        (schemas, databases, sessions, seen)
    };
    let (explain, (schemas, databases, sessions, seen)) = tokio::join!(explain, browse);
    assert!(matches!(explain, Err(CoreError::Timeout(_))), "EXPLAIN must hit its 3s lock timeout: {explain:?}");
    let schemas = schemas.expect("schemas() issued during the EXPLAIN");
    assert!(schemas.iter().any(|s| s.name == fx.schema), "{schemas:?}");
    let databases = databases.expect("databases() issued during the EXPLAIN");
    assert!(databases.iter().any(|d| d.is_current), "{databases:?}");
    let sessions = sessions.expect("active_sessions() issued during the EXPLAIN");
    assert!(sessions.iter().any(|s| s.query.as_deref().is_some_and(|q| q.contains("LOCK TABLE"))), "the lock holder (idle in transaction) is listed: {sessions:?}");
    assert_eq!(seen, session_settings, "a statement ran inside EXPLAIN's transaction (saw its SET LOCAL timeouts)");

    fx.raw.batch_execute("ROLLBACK").await.expect("release lock");
    assert!(fx.wait_until_idle(pid).await, "session left in a transaction");
    assert!(db.explain(&sql, false).await.unwrap().is_some());
    fx.drop_schema().await;
}

/// A DBA's `pg_cancel_backend` stops a running EXPLAIN ANALYZE with SQLSTATE 57014, the same
/// code as EXPLAIN's own statement timeout. Long before that timeout it is a QUERY error that
/// keeps its code, not a TIMEOUT.
#[tokio::test]
async fn explain_cancelled_by_another_session_is_a_query_error_not_a_timeout() {
    let Some(url) = pg_url() else { return };
    let fx = Fixture::new(&url).await;
    let db = connect(&url, "bulkdb-it-explain-dba-cancel").await;
    let pid = db.backend_pid();
    let explain = db.explain("SELECT pg_sleep(60)", true);
    let dba = async {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let rows =
                fx.raw.query("SELECT 1 FROM pg_stat_activity WHERE pid = $1 AND state = 'active' AND query ILIKE 'EXPLAIN%'", &[&pid]).await.expect("pg_stat_activity");
            if !rows.is_empty() {
                break;
            }
            assert!(Instant::now() < deadline, "EXPLAIN ANALYZE never became active");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let row = fx.raw.query_one("SELECT pg_cancel_backend($1)", &[&pid]).await.expect("pg_cancel_backend");
        assert!(row.get::<_, bool>(0), "pg_cancel_backend refused");
    };
    let started = Instant::now();
    let (result, ()) = tokio::join!(explain, dba);
    match result {
        Err(CoreError::Query { code: Some(ref c), .. }) if c == "57014" => {}
        other => panic!("a foreign cancel must be QUERY 57014, got {other:?}"),
    }
    assert!(started.elapsed() < Duration::from_secs(30), "the cancel did not stop the EXPLAIN ANALYZE");
    assert!(fx.wait_until_idle(pid).await, "session left in a transaction");
    assert!(db.explain("SELECT 1", false).await.unwrap().is_some());
    fx.drop_schema().await;
}

// ---------------------------------------------------------------------------------------------
// Catalog
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn catalog_matches_information_schema() {
    let Some(url) = pg_url() else { return };
    let fx = Fixture::new(&url).await;
    fx.nasty_table(ROWS).await;
    let db = connect(&url, "bulkdb-it-catalog").await;
    let s = fx.schema.clone();

    let dbs = db.databases().await.unwrap();
    let current = dbs.iter().find(|d| d.is_current).expect("current database listed");
    assert_eq!(current.name, db.database_name());
    assert!(current.accessible);
    assert!(current.size_bytes.unwrap_or(0) > 0);
    assert!(current.encoding.is_some());

    let schemas = db.schemas("").await.unwrap();
    let mine = schemas.iter().find(|x| x.name == s).expect("own schema listed");
    assert_eq!(mine.table_count, Some(2));
    assert!(mine.owner.is_some());
    let pos = |n: &str| schemas.iter().position(|x| x.name == n).unwrap();
    assert!(pos(&s) < pos("pg_catalog") && pos(&s) < pos("information_schema"), "user schemas first");
    assert!(!schemas.iter().any(|x| x.name.starts_with("pg_toast")));
    assert!(matches!(db.schemas("some_other_db").await, Err(CoreError::Invalid(_))));

    let tables = db.tables("", &s).await.unwrap();
    assert_eq!(tables.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(), vec!["child", "nasty"]);
    let nasty = &tables[1];
    assert_eq!(nasty.kind, TableKind::Table);
    assert_eq!(nasty.schema, s);
    assert_eq!(nasty.comment.as_deref(), Some("it's nasty"));
    let est = nasty.estimated_rows.expect("analyzed table has reltuples");
    assert!((est as i64 - ROWS).abs() < ROWS / 10, "estimate {est} vs {ROWS}");
    assert!(nasty.table_bytes.unwrap() > 0 && nasty.index_bytes.unwrap() > 0);
    assert!(nasty.total_bytes.unwrap() >= nasty.table_bytes.unwrap() + nasty.index_bytes.unwrap());

    // Columns vs information_schema.columns (names, order, nullability).
    let cols = db.columns(&fx.table("nasty")).await.unwrap();
    let reference = fx
        .raw
        .query("SELECT column_name::text, ordinal_position::int4, is_nullable::text FROM information_schema.columns WHERE table_schema = $1 AND table_name = 'nasty' ORDER BY ordinal_position", &[&s])
        .await
        .unwrap();
    assert_eq!(cols.len(), reference.len());
    for (c, r) in cols.iter().zip(&reference) {
        assert_eq!(c.name, r.get::<_, String>(0));
        assert_eq!(c.position as i32, r.get::<_, i32>(1));
        assert_eq!(c.nullable, r.get::<_, String>(2) == "YES", "{}", c.name);
    }
    let by_name = |n: &str| cols.iter().find(|c| c.name == n).unwrap();
    assert!(by_name("id").primary_key && !by_name("id").nullable && by_name("id").data_type == "bigint");
    assert_eq!(by_name("num").data_type, "numeric(12,3)");
    assert_eq!(by_name("num").comment.as_deref(), Some("three decimals"));
    assert!(!by_name("flag").nullable && !by_name("flag").primary_key);
    let child = db.columns(&fx.table("child")).await.unwrap();
    let parent = child.iter().find(|c| c.name == "parent_id").unwrap();
    assert_eq!(parent.foreign_key.as_deref(), Some(format!("{s}.nasty(id)").as_str()));
    assert!(child.iter().find(|c| c.name == "tag").unwrap().unique);
    assert!(matches!(db.columns(&fx.table("missing")).await, Err(CoreError::NotFound(_))));
    // Unqualified names resolve through search_path.
    assert!(!db.columns(&TableRef::new(None, "pg_class")).await.unwrap().is_empty());

    let idx = db.indexes(&fx.table("nasty")).await.unwrap();
    let pkey = idx.iter().find(|i| i.primary).expect("primary key index");
    assert_eq!(pkey.name, "nasty_pkey");
    assert!(pkey.unique);
    assert_eq!(pkey.columns, vec!["id"]);
    assert_eq!(pkey.kind.as_deref(), Some("btree"));
    assert!(pkey.definition.as_deref().unwrap().starts_with("CREATE UNIQUE INDEX nasty_pkey"));
    assert!(pkey.size_bytes.unwrap() > 0);
    let flag = idx.iter().find(|i| i.name == "nasty_flag_idx").expect("partial index");
    assert_eq!(flag.columns, vec!["flag", "lower(note)"]);
    assert_eq!(flag.include, vec!["text"]);
    assert_eq!(flag.predicate.as_deref(), Some("flag"));
    assert!(!flag.unique && !flag.primary);
    assert!(flag.scans.is_some());

    let ddl = db.ddl(&fx.table("nasty")).await.unwrap();
    assert!(ddl.starts_with(&format!("CREATE TABLE \"{s}\".\"nasty\" (\n    \"id\" bigint NOT NULL,\n")), "{ddl}");
    assert!(ddl.contains("CONSTRAINT \"nasty_pkey\" PRIMARY KEY (id)"), "{ddl}");
    assert!(ddl.contains("CREATE INDEX nasty_flag_idx ON"), "{ddl}");
    assert!(!ddl.contains("CREATE UNIQUE INDEX nasty_pkey"), "constraint-owned index is not repeated: {ddl}");
    assert!(ddl.contains(&format!("COMMENT ON TABLE \"{s}\".\"nasty\" IS 'it''s nasty';")), "{ddl}");
    assert!(ddl.contains(&format!("COMMENT ON COLUMN \"{s}\".\"nasty\".\"num\" IS 'three decimals';")), "{ddl}");
    let child_ddl = db.ddl(&fx.table("child")).await.unwrap();
    assert!(child_ddl.contains(&format!("FOREIGN KEY (parent_id) REFERENCES {s}.nasty(id)")), "{child_ddl}");
    assert!(child_ddl.contains("UNIQUE (tag)"), "{child_ddl}");

    assert_eq!(db.exact_count(&fx.table("nasty"), CancellationToken::new()).await.unwrap(), ROWS as u64);
    let cancelled = CancellationToken::new();
    cancelled.cancel();
    assert!(matches!(db.exact_count(&fx.table("nasty"), cancelled).await, Err(CoreError::Cancelled)));

    let select = db.select_all_sql(&fx.table("nasty"), &cols);
    assert!(select.starts_with("SELECT \"id\",") && select.ends_with(&format!("FROM \"{s}\".\"nasty\"")));
    fx.drop_schema().await;
}

// ---------------------------------------------------------------------------------------------
// Monitor
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn monitor_finds_the_session_by_application_name() {
    let Some(url) = pg_url() else { return };
    let fx = Fixture::new(&url).await;
    fx.nasty_table(10_000).await;
    let app = format!("bulkdb-it-job-{}", std::process::id());
    let job = connect(&url, &app).await;
    let monitor = connect(&url, "bulkdb-monitor").await;
    assert_eq!(job.application_name(), app);
    let handle = JobHandle { job_id: "it".into(), application_name: app.clone(), session_id: job.session_id(), temp_bytes_at_start: None };

    // Nothing running yet: still found (idle), because the session exists.
    let idle = monitor.snapshot(&handle).await.unwrap();
    assert!(idle.found);
    assert_eq!(idle.pid.as_deref(), Some(job.backend_pid().to_string().as_str()));

    // A slow COPY that is never polled: the server ends up waiting on ClientWrite.
    let sql = format!("SELECT n.id, pg_sleep(0.002) FROM {}.nasty n", fx.schema);
    let cancel = CancellationToken::new();
    let out = job.stream(&csv_query(&sql, CsvFormat::default()), &SessionOpts::default(), cancel.clone()).await.unwrap();
    let mut stream = out.stream;
    tokio::time::sleep(Duration::from_millis(300)).await;

    let snap = monitor.snapshot(&handle).await.unwrap();
    assert!(snap.found);
    assert_eq!(snap.pid.as_deref(), Some(job.backend_pid().to_string().as_str()));
    assert_eq!(snap.state.as_deref(), Some("active"));
    assert!(snap.elapsed_ms.is_some());
    assert!(snap.temp_bytes.is_some());
    assert!(snap.client_write_ratio.is_some());
    if monitor.server_version_num() >= 140_000 {
        assert!(snap.server_rows.is_some(), "pg_stat_progress_copy row expected on PostgreSQL 14+");
    }

    let sessions = monitor.active_sessions(&[fx.table("nasty")]).await.unwrap();
    let mine = sessions.iter().find(|x| x.pid == job.backend_pid().to_string()).expect("job session touches nasty");
    assert_eq!(mine.application.as_deref(), Some(app.as_str()));
    assert_eq!(mine.tables, vec![format!("{}.nasty", fx.schema)]);
    assert!(mine.query.as_deref().unwrap().contains("pg_sleep"));
    assert!(!mine.blocking);
    assert!(monitor.active_sessions(&[]).await.unwrap().is_empty());

    // Cancel from the monitor connection: the stream ends with Cancelled.
    assert!(monitor.cancel_session(&job.backend_pid().to_string()).await.unwrap());
    let mut saw_cancelled = false;
    for _ in 0..1000 {
        match stream.next().await {
            Some(Err(CoreError::Cancelled)) => {
                saw_cancelled = true;
                break;
            }
            Some(Err(e)) => panic!("unexpected error: {e}"),
            Some(Ok(_)) => continue,
            None => break,
        }
    }
    assert!(saw_cancelled, "pg_cancel_backend must surface as Cancelled");
    assert!(fx.wait_until_idle(job.backend_pid()).await);

    // Unknown pid / bad id.
    assert!(!monitor.cancel_session("2147483646").await.unwrap());
    assert!(matches!(monitor.cancel_session("nope").await, Err(CoreError::Invalid(_))));

    drop(job);
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut gone = monitor.snapshot(&handle).await.unwrap();
    while gone.found && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(100)).await;
        gone = monitor.snapshot(&handle).await.unwrap();
    }
    assert!(!gone.found, "closed session must not be found");
    assert_eq!(gone, bulk_core::monitor::ServerSnapshot::default());
    fx.drop_schema().await;
}
