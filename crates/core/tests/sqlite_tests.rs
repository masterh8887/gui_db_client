//! SQLite driver integration tests: streaming, cancellation, probe, catalog, explain and the
//! fixed-memory guarantee. Everything runs against `testdata::sqlite_fixture` files in a
//! temp dir, no docker needed.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use arrow_array::{Array, Float64Array, Int64Array, StringArray};
use arrow_schema::DataType;
use bulk_core::catalog::Catalog;
use bulk_core::source::sqlite::SqliteDatabase;
use bulk_core::source::{Batch, ConnectParams, Query, Source, SslMode, StreamOutput};
use bulk_core::testdata::{self, Lcg};
use bulk_core::types::{SessionOpts, TableRef};
use bulk_core::{CancellationToken, CoreError, BATCH_ROWS};
use futures::StreamExt;

fn params(path: &str) -> ConnectParams {
    let mut fields = BTreeMap::new();
    fields.insert("path".to_string(), path.to_string());
    ConnectParams {
        driver: "sqlite".into(),
        host: String::new(),
        port: 0,
        database: None,
        user: None,
        password: None,
        fields,
        application_name: "bulkdb-test".into(),
        connect_timeout: Duration::from_secs(5),
        ssl: SslMode::Prefer,
    }
}

fn query(sql: &str) -> Query {
    Query { sql: sql.to_string(), output: StreamOutput::Arrow }
}

/// Fixture file in a fresh temp dir; the dir must outlive the connection.
fn fixture(wide: u64, narrow: u64, nasty: u64) -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("fixture.db");
    testdata::sqlite_fixture(&path, wide, narrow, nasty).unwrap();
    (dir, path.to_string_lossy().into_owned())
}

async fn connect(path: &str) -> SqliteDatabase {
    SqliteDatabase::connect(&params(path)).await.unwrap()
}

/// Drain a stream, counting rows and checking every batch against the row cap.
async fn drain(db: &SqliteDatabase, sql: &str) -> (Vec<arrow_array::RecordBatch>, u64) {
    let out = db.stream(&query(sql), &SessionOpts::default(), CancellationToken::new()).await.unwrap();
    let mut stream = out.stream;
    let mut batches = Vec::new();
    let mut rows = 0u64;
    while let Some(item) = stream.next().await {
        match item.unwrap() {
            Batch::Arrow(b) => {
                assert!(b.num_rows() <= BATCH_ROWS, "batch of {} rows exceeds the cap", b.num_rows());
                rows += b.num_rows() as u64;
                batches.push(b);
            }
            other => panic!("sqlite must produce Arrow batches, got {other:?}"),
        }
    }
    (batches, rows)
}

#[tokio::test]
async fn streams_200k_narrow_rows_in_capped_batches() {
    let (_dir, path) = fixture(0, 200_000, 0);
    let db = connect(&path).await;
    let out = db.stream(&query("SELECT a, b, c FROM narrow ORDER BY a"), &SessionOpts::default(), CancellationToken::new()).await.unwrap();
    let schema = out.schema.expect("schema is known after prepare");
    assert_eq!(schema.fields().len(), 3);
    for f in schema.fields() {
        assert_eq!(f.data_type(), &DataType::Int64, "column {} must be Int64", f.name());
    }
    assert_eq!(schema.field(0).name(), "a");

    let mut stream = out.stream;
    let mut total = 0u64;
    let mut batches = 0u64;
    let mut expected_next = 0i64;
    while let Some(item) = stream.next().await {
        let Batch::Arrow(b) = item.unwrap() else { panic!("expected Arrow batch") };
        assert!(b.num_rows() <= 65_536, "batch has {} rows", b.num_rows());
        assert_eq!(b.schema(), schema);
        let a = b.column(0).as_any().downcast_ref::<Int64Array>().unwrap();
        assert_eq!(a.value(0), expected_next, "rows must arrive in order with no gaps");
        expected_next += b.num_rows() as i64;
        total += b.num_rows() as u64;
        batches += 1;
    }
    assert_eq!(total, 200_000);
    assert!(batches >= 4, "200k rows at <= 65536 per batch need at least 4 batches, got {batches}");
}

#[tokio::test]
async fn honours_fetch_batch_rows() {
    let (_dir, path) = fixture(0, 10_000, 0);
    let db = connect(&path).await;
    db.apply_session(&SessionOpts { fetch_batch_rows: Some(1000), ..Default::default() }).await.unwrap();
    let out = db.stream(&query("SELECT * FROM narrow"), &SessionOpts::default(), CancellationToken::new()).await.unwrap();
    let mut stream = out.stream;
    let mut n = 0;
    while let Some(item) = stream.next().await {
        let b = match item.unwrap() {
            Batch::Arrow(b) => b,
            _ => unreachable!(),
        };
        assert_eq!(b.num_rows(), 1000);
        n += 1;
    }
    assert_eq!(n, 10);
    // A per-call value larger than BATCH_ROWS is clamped.
    let out =
        db.stream(&query("SELECT * FROM narrow"), &SessionOpts { fetch_batch_rows: Some(1_000_000), ..Default::default() }, CancellationToken::new()).await.unwrap();
    let mut stream = out.stream;
    let mut n = 0;
    while let Some(item) = stream.next().await {
        let b = match item.unwrap() {
            Batch::Arrow(b) => b,
            _ => unreachable!(),
        };
        assert_eq!(b.num_rows(), 10_000);
        n += 1;
    }
    assert_eq!(n, 1);
}

#[tokio::test]
async fn streams_wide_rows_with_text() {
    let (_dir, path) = fixture(1_000, 0, 0);
    let db = connect(&path).await;
    let (batches, rows) = drain(&db, "SELECT * FROM wide ORDER BY id").await;
    assert_eq!(rows, 1_000);
    let schema = batches[0].schema();
    assert_eq!(schema.fields().len(), 9);
    assert_eq!(schema.field(0).data_type(), &DataType::Int64);
    for f in schema.fields().iter().skip(1) {
        assert_eq!(f.data_type(), &DataType::Utf8, "column {} must be Utf8", f.name());
    }
    assert_eq!(schema.field(1).name(), "title");
    // Row 0 is exactly what the generator produced (the fixture seeds the LCG with 42).
    let expected = testdata::wide_row(0, &mut Lcg::new(42));
    let title = batches[0].column(1).as_any().downcast_ref::<StringArray>().unwrap();
    let body = batches[0].column(2).as_any().downcast_ref::<StringArray>().unwrap();
    assert_eq!(title.value(0), expected[1]);
    assert_eq!(body.value(0), expected[2]);
}

#[tokio::test]
async fn nasty_nulls_newlines_and_unicode_survive() {
    let (_dir, path) = fixture(0, 0, 500);
    let db = connect(&path).await;
    let (batches, rows) = drain(&db, "SELECT * FROM nasty ORDER BY id").await;
    assert_eq!(rows, 500);
    assert_eq!(batches.len(), 1);
    let b = &batches[0];
    let schema = b.schema();
    assert_eq!(schema.field(0).data_type(), &DataType::Int64);
    assert_eq!(schema.field(1).data_type(), &DataType::Utf8);
    assert_eq!(schema.field(2).data_type(), &DataType::Float64, "REAL -> Float64");
    assert_eq!(schema.field(5).data_type(), &DataType::Utf8);

    let id = b.column(0).as_any().downcast_ref::<Int64Array>().unwrap();
    let text = b.column(1).as_any().downcast_ref::<StringArray>().unwrap();
    let num = b.column(2).as_any().downcast_ref::<Float64Array>().unwrap();
    let note = b.column(3).as_any().downcast_ref::<StringArray>().unwrap();
    let unicode = b.column(5).as_any().downcast_ref::<StringArray>().unwrap();

    let mut rng = Lcg::new(42);
    let mut saw_null_text = false;
    let mut saw_newline = false;
    let mut saw_unicode = false;
    for i in 0..500usize {
        let expected = testdata::nasty_row(i as u64, &mut rng);
        assert_eq!(id.value(i), i as i64);
        match &expected[1] {
            None => {
                assert!(text.is_null(i), "row {i}: text must be NULL");
                saw_null_text = true;
            }
            Some(s) => {
                assert!(!text.is_null(i));
                assert_eq!(text.value(i), s, "row {i}: text must be preserved byte for byte");
                if s.contains('\n') {
                    saw_newline = true;
                }
            }
        }
        match &expected[2] {
            None => assert!(num.is_null(i), "row {i}: num must be NULL"),
            Some(s) => assert_eq!(num.value(i), s.parse::<f64>().unwrap()),
        }
        match &expected[3] {
            None => assert!(note.is_null(i)),
            Some(s) => assert_eq!(note.value(i), s),
        }
        match &expected[5] {
            None => assert!(unicode.is_null(i)),
            Some(s) => {
                assert_eq!(unicode.value(i), s);
                if s.contains("한글") || s.contains("🚀") {
                    saw_unicode = true;
                }
            }
        }
    }
    assert!(saw_null_text && saw_newline && saw_unicode, "fixture must exercise NULLs, newlines and unicode");
}

#[tokio::test]
async fn untyped_expression_columns_follow_the_first_row() {
    let (_dir, path) = fixture(0, 10, 0);
    let db = connect(&path).await;
    let (batches, rows) = drain(&db, "SELECT a + 0.5 AS x, 'k' || a AS s, NULL AS n, a * 2 AS d, CAST('ab' AS BLOB) AS bin FROM narrow").await;
    assert_eq!(rows, 10);
    let schema = batches[0].schema();
    assert_eq!(schema.field(0).data_type(), &DataType::Float64);
    assert_eq!(schema.field(1).data_type(), &DataType::Utf8);
    assert_eq!(schema.field(2).data_type(), &DataType::Utf8, "all-NULL column defaults to Utf8");
    assert_eq!(schema.field(3).data_type(), &DataType::Int64);
    assert_eq!(schema.field(4).data_type(), &DataType::Binary);
    let n = batches[0].column(2);
    assert_eq!(n.null_count(), 10);
}

#[tokio::test]
async fn mismatched_values_become_null_without_breaking_the_schema() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mixed.db");
    {
        let c = rusqlite::Connection::open(&path).unwrap();
        c.execute_batch(
            "CREATE TABLE mixed (i INTEGER, r REAL, t TEXT);
             INSERT INTO mixed VALUES (1, 1.5, 'a');
             INSERT INTO mixed VALUES ('not a number', 'nope', 7);
             INSERT INTO mixed VALUES (3.0, 2, x'00ff');",
        )
        .unwrap();
    }
    let db = connect(&path.to_string_lossy()).await;
    let (batches, rows) = drain(&db, "SELECT i, r, t FROM mixed ORDER BY rowid").await;
    assert_eq!(rows, 3);
    let b = &batches[0];
    assert_eq!(b.schema().field(0).data_type(), &DataType::Int64);
    assert_eq!(b.schema().field(1).data_type(), &DataType::Float64);
    assert_eq!(b.schema().field(2).data_type(), &DataType::Utf8);
    let i = b.column(0).as_any().downcast_ref::<Int64Array>().unwrap();
    assert_eq!(i.value(0), 1);
    assert!(i.is_null(1), "text in an INTEGER column is written as NULL");
    assert_eq!(i.value(2), 3, "integral REAL in an INTEGER column keeps its value");
    let r = b.column(1).as_any().downcast_ref::<Float64Array>().unwrap();
    assert_eq!(r.value(0), 1.5);
    assert!(r.is_null(1));
    assert_eq!(r.value(2), 2.0);
    let t = b.column(2).as_any().downcast_ref::<StringArray>().unwrap();
    assert_eq!(t.value(0), "a");
    assert_eq!(t.value(1), "7", "integers in a TEXT column are written as text");
    assert!(!t.is_null(2));
}

#[tokio::test]
async fn rejects_bind_parameters_and_statements_without_columns() {
    let (_dir, path) = fixture(0, 10, 0);
    let db = connect(&path).await;
    let err = db.stream(&query("SELECT * FROM narrow WHERE a = ?1"), &SessionOpts::default(), CancellationToken::new()).await.err().unwrap();
    assert!(matches!(err, CoreError::Invalid(_)), "got {err:?}");
    let err = db.stream(&query("SELECT * FROM no_such_table"), &SessionOpts::default(), CancellationToken::new()).await.err().unwrap();
    assert!(matches!(err, CoreError::Query { .. }), "got {err:?}");
    // The file is opened read-only and query_only: writes never get through.
    let err = db.stream(&query("DELETE FROM narrow"), &SessionOpts::default(), CancellationToken::new()).await.err().unwrap();
    assert!(matches!(err, CoreError::Query { .. } | CoreError::Invalid(_)), "got {err:?}");
    assert_eq!(db.exact_count(&TableRef::new(None, "narrow"), CancellationToken::new()).await.unwrap(), 10);
}

#[tokio::test]
async fn cancel_interrupts_a_running_query_within_two_seconds() {
    let (_dir, path) = fixture(0, 1, 0);
    let db = connect(&path).await;
    let cancel = CancellationToken::new();
    let sql = "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM c WHERE x < 50000000) SELECT x FROM c";
    let out = db.stream(&query(sql), &SessionOpts::default(), cancel.clone()).await.unwrap();
    assert_eq!(out.schema.unwrap().field(0).data_type(), &DataType::Int64);
    let mut stream = out.stream;
    let first = stream.next().await.unwrap().unwrap();
    assert_eq!(first.rows(), BATCH_ROWS as u64);

    cancel.cancel();
    let started = Instant::now();
    let mut outcome = None;
    while let Some(item) = stream.next().await {
        if let Err(e) = item {
            outcome = Some(e);
            break;
        }
    }
    let elapsed = started.elapsed();
    assert!(matches!(outcome, Some(CoreError::Cancelled)), "got {outcome:?}");
    assert!(elapsed < Duration::from_secs(2), "cancellation took {elapsed:?}");
    assert!(stream.next().await.is_none(), "stream ends after the error");

    // The reader thread must have released the connection (interrupted, not still
    // stepping 50M rows): a probe on the same connection returns promptly.
    let probe = tokio::time::timeout(Duration::from_secs(2), db.probe()).await;
    assert!(probe.is_ok(), "the connection stayed busy after cancellation");
    probe.unwrap().unwrap();
}

#[tokio::test]
async fn dropping_the_stream_interrupts_the_reader() {
    let (_dir, path) = fixture(0, 1, 0);
    let db = connect(&path).await;
    let sql = "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM c WHERE x < 50000000) SELECT sum(x) FROM c";
    // An aggregate yields no row until the whole CTE ran; dropping the future / stream
    // must not leave the connection busy for the rest of that time.
    let started = Instant::now();
    let q = query(sql);
    let opts = SessionOpts::default();
    let fut = db.stream(&q, &opts, CancellationToken::new());
    let out = tokio::time::timeout(Duration::from_millis(300), fut).await;
    drop(out);
    let probe = tokio::time::timeout(Duration::from_secs(2), db.probe()).await;
    assert!(probe.is_ok(), "connection still busy {:?} after the stream was dropped", started.elapsed());
    probe.unwrap().unwrap();
}

#[tokio::test]
async fn probe_reports_file_facts() {
    let (_dir, path) = fixture(0, 100, 0);
    let db = connect(&path).await;
    assert_eq!(db.driver_id(), "sqlite");
    assert!(db.session_id().is_none());
    assert!(!db.supports_raw_csv());
    let info = db.probe().await.unwrap();
    assert_eq!(info.driver, "sqlite");
    assert!(info.version.starts_with('3'), "sqlite_version() = {}", info.version);
    assert_eq!(info.database.as_deref(), Some(path.as_str()));
    assert!(info.ram_bytes.is_none());
    assert_eq!(info.can_write, Some(false));
    let size: u64 = info.settings["file_size_bytes"].parse().unwrap();
    assert!(size > 0);
    assert_eq!(size, std::fs::metadata(&path).unwrap().len());
    let page_size: u64 = info.settings["page_size"].parse().unwrap();
    let page_count: u64 = info.settings["page_count"].parse().unwrap();
    assert!(page_size >= 512 && page_count >= 1);
    assert!(!info.settings["journal_mode"].is_empty());
    assert_eq!(info.settings["encoding"], "UTF-8");
    assert_eq!(info.settings["read_only"], "true");
    assert_eq!(info.settings["sqlite_version"], info.version);
}

#[tokio::test]
async fn connect_errors_name_the_missing_file() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("nope.db");
    let err = SqliteDatabase::connect(&params(&missing.to_string_lossy())).await.err().unwrap();
    match err {
        CoreError::Connect(msg) => assert!(msg.contains("nope.db"), "message must carry the path: {msg}"),
        other => panic!("expected Connect, got {other:?}"),
    }
    // Not a database file at all.
    let junk = dir.path().join("junk.db");
    std::fs::write(&junk, b"definitely not sqlite, just some bytes padded out to a page.....").unwrap();
    let err = SqliteDatabase::connect(&params(&junk.to_string_lossy())).await.err().unwrap();
    assert!(matches!(err, CoreError::Connect(_)), "got {err:?}");
    // `database` is accepted as a fallback for `path`.
    let (_d, path) = fixture(0, 1, 0);
    let mut p = params("");
    p.fields.clear();
    p.database = Some(path);
    SqliteDatabase::connect(&p).await.unwrap();
    // `:memory:` is always allowed and reports no file size.
    let mem = SqliteDatabase::connect(&params(":memory:")).await.unwrap();
    let info = mem.probe().await.unwrap();
    assert!(!info.settings.contains_key("file_size_bytes"));
    let (_, rows) = drain(&mem, "SELECT 1 AS x UNION ALL SELECT 2").await;
    assert_eq!(rows, 2);
}

#[tokio::test]
async fn catalog_lists_tables_columns_indexes_and_ddl() {
    let (_dir, path) = fixture(50, 100, 200);
    let db = connect(&path).await;

    let dbs = db.databases().await.unwrap();
    let main = dbs.iter().find(|d| d.name == "main").expect("main database");
    assert!(main.is_current && main.accessible);
    assert!(main.size_bytes.unwrap() > 0);
    assert_eq!(main.encoding.as_deref(), Some("UTF-8"));

    let schemas = db.schemas("main").await.unwrap();
    assert_eq!(schemas.len(), 1);
    assert_eq!(schemas[0].name, "main");
    assert_eq!(schemas[0].table_count, Some(3));
    assert!(matches!(db.schemas("nope").await, Err(CoreError::NotFound(_))));

    let tables = db.tables("main", "main").await.unwrap();
    let by_name: BTreeMap<&str, _> = tables.iter().map(|t| (t.name.as_str(), t)).collect();
    assert_eq!(by_name.len(), 3, "{tables:?}");
    assert_eq!(by_name["wide"].estimated_rows, Some(50));
    assert_eq!(by_name["narrow"].estimated_rows, Some(100));
    assert_eq!(by_name["nasty"].estimated_rows, Some(200));
    for t in &tables {
        assert_eq!(t.schema, "main");
        assert_eq!(t.kind, bulk_core::types::TableKind::Table);
        // dbstat is compiled into the bundled SQLite: every table has a size.
        assert!(t.table_bytes.unwrap() > 0, "{t:?}");
        assert_eq!(t.total_bytes, Some(t.table_bytes.unwrap() + t.index_bytes.unwrap()));
    }
    assert!(by_name["nasty"].index_bytes.unwrap() > 0, "nasty has a (partial) index");

    let nasty = TableRef::new(Some("main"), "nasty");
    let cols = db.columns(&nasty).await.unwrap();
    assert_eq!(cols.len(), 6);
    let names: Vec<&str> = cols.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, testdata::NASTY_COLUMNS.to_vec());
    assert_eq!(cols[0].position, 1);
    assert!(cols[0].primary_key && cols[0].unique);
    assert_eq!(cols[0].data_type, "INTEGER");
    assert_eq!(cols[2].data_type, "REAL");
    assert!(!cols[1].primary_key && cols[1].nullable && !cols[1].unique);
    assert!(cols.iter().all(|c| c.foreign_key.is_none()));
    assert!(matches!(db.columns(&TableRef::new(None, "missing")).await, Err(CoreError::NotFound(_))));

    let idx = db.indexes(&nasty).await.unwrap();
    let partial = idx.iter().find(|i| i.name == "nasty_note").expect("partial index listed");
    assert_eq!(partial.columns, vec!["note".to_string()]);
    assert_eq!(partial.predicate.as_deref(), Some("note IS NOT NULL"));
    assert!(!partial.unique && !partial.primary);
    assert_eq!(partial.kind.as_deref(), Some("btree"));
    assert!(partial.definition.as_deref().unwrap().starts_with("CREATE INDEX"));
    assert!(partial.size_bytes.unwrap() > 0);

    let ddl = db.ddl(&nasty).await.unwrap();
    assert!(ddl.starts_with("CREATE TABLE nasty"), "{ddl}");
    assert!(ddl.contains("CREATE INDEX nasty_note"), "{ddl}");
    assert!(ddl.lines().all(|l| l.ends_with(';')));
    assert!(matches!(db.ddl(&TableRef::new(None, "missing")).await, Err(CoreError::NotFound(_))));

    assert_eq!(db.exact_count(&nasty, CancellationToken::new()).await.unwrap(), 200);
    assert_eq!(db.exact_count(&TableRef::new(None, "wide"), CancellationToken::new()).await.unwrap(), 50);
    let cancelled = CancellationToken::new();
    cancelled.cancel();
    assert!(matches!(db.exact_count(&nasty, cancelled).await, Err(CoreError::Cancelled)));

    assert_eq!(db.quote_ident("a\"b"), "\"a\"\"b\"");
    assert_eq!(db.select_all_sql(&nasty, &cols[..2]), "SELECT \"id\",\n       \"text\"\nFROM \"main\".\"nasty\"");
}

#[tokio::test]
async fn columns_report_foreign_keys_and_unique_constraints() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("fk.db");
    {
        let c = rusqlite::Connection::open(&path).unwrap();
        c.execute_batch(
            "CREATE TABLE parent (id INTEGER PRIMARY KEY, code TEXT UNIQUE NOT NULL);
             CREATE TABLE child (id INTEGER PRIMARY KEY, parent_id INTEGER REFERENCES parent(id), email TEXT, note TEXT DEFAULT 'x');
             CREATE UNIQUE INDEX child_email ON child(email);
             CREATE VIEW v AS SELECT id FROM child;",
        )
        .unwrap();
    }
    let db = connect(&path.to_string_lossy()).await;
    let cols = db.columns(&TableRef::new(None, "child")).await.unwrap();
    let parent_id = cols.iter().find(|c| c.name == "parent_id").unwrap();
    assert_eq!(parent_id.foreign_key.as_deref(), Some("parent(id)"));
    let email = cols.iter().find(|c| c.name == "email").unwrap();
    assert!(email.unique && !email.primary_key);
    let note = cols.iter().find(|c| c.name == "note").unwrap();
    assert_eq!(note.default.as_deref(), Some("'x'"));
    let pcols = db.columns(&TableRef::new(None, "parent")).await.unwrap();
    let code = pcols.iter().find(|c| c.name == "code").unwrap();
    assert!(code.unique && !code.nullable);

    let idx = db.indexes(&TableRef::new(None, "child")).await.unwrap();
    let email_idx = idx.iter().find(|i| i.name == "child_email").unwrap();
    assert!(email_idx.unique && !email_idx.primary && email_idx.predicate.is_none());
    let pidx = db.indexes(&TableRef::new(None, "parent")).await.unwrap();
    let auto = pidx.iter().find(|i| i.name.starts_with("sqlite_autoindex")).expect("UNIQUE constraint index");
    assert!(auto.unique && auto.definition.is_none());

    let tables = db.tables("", "").await.unwrap();
    let view = tables.iter().find(|t| t.name == "v").unwrap();
    assert_eq!(view.kind, bulk_core::types::TableKind::View);
    assert_eq!(view.estimated_rows, Some(0));
    assert!(view.table_bytes.is_none());
    assert!(db.ddl(&TableRef::new(None, "v")).await.unwrap().starts_with("CREATE VIEW v"));
}

#[tokio::test]
async fn explain_returns_a_text_plan() {
    let (_dir, path) = fixture(0, 100, 50);
    let db = connect(&path).await;
    let plan = db.explain("SELECT * FROM narrow WHERE a = 5", false).await.unwrap().unwrap();
    assert_eq!(plan.raw_format, "text");
    assert!(plan.raw.starts_with("QUERY PLAN\n"), "{}", plan.raw);
    assert!(plan.raw.contains("SEARCH narrow"), "{}", plan.raw);
    assert!(plan.estimated_rows.is_none() && !plan.analyzed);
    let root = plan.root.unwrap();
    assert_eq!(root.node_type, "QUERY PLAN");
    assert_eq!(root.children[0].node_type, "SEARCH");
    assert_eq!(root.children[0].relation.as_ref().map(|r| r.name.as_str()), Some("narrow"));
    assert_eq!(root.children[0].extra["detail"], root.children[0].extra["detail"].trim());
    assert_eq!(plan.tables, vec![TableRef::new(None, "narrow")]);

    let plan = db.explain("SELECT n.id, w.b FROM nasty n JOIN narrow w ON w.a = n.id WHERE n.note IS NOT NULL ORDER BY n.text;", true).await.unwrap().unwrap();
    assert!(plan.raw.lines().count() >= 3, "{}", plan.raw);
    assert!(!plan.analyzed, "SQLite has no EXPLAIN ANALYZE");
    assert!(plan.tables.len() >= 2, "{:?}", plan.tables);

    let err = db.explain("SELECT * FROM missing_table", false).await.err().unwrap();
    assert!(matches!(err, CoreError::Query { .. }), "got {err:?}");
}

/// Fast 5M-row narrow table: one recursive-CTE insert instead of 5M prepared executes.
fn big_narrow_fixture(path: &std::path::Path, rows: i64) {
    let c = rusqlite::Connection::open(path).unwrap();
    c.execute_batch("PRAGMA journal_mode=OFF; PRAGMA synchronous=OFF; CREATE TABLE narrow (a INTEGER PRIMARY KEY, b INTEGER, c INTEGER);").unwrap();
    c.execute(
        "INSERT INTO narrow(a, b, c) WITH RECURSIVE s(x) AS (SELECT 0 UNION ALL SELECT x + 1 FROM s WHERE x < ?1 - 1) \
         SELECT x, (x * 7919) % 1000000, (x * 104729) % 1000 FROM s",
        [rows],
    )
    .unwrap();
}

#[tokio::test]
async fn memory_stays_flat_over_5m_rows() {
    const ROWS: i64 = 5_000_000;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("big.db");
    let t0 = Instant::now();
    big_narrow_fixture(&path, ROWS);
    let gen_secs = t0.elapsed().as_secs_f64();
    let file_mb = std::fs::metadata(&path).unwrap().len() / (1024 * 1024);
    println!("fixture: {ROWS} rows, {file_mb} MB, generated in {gen_secs:.1} s");

    let db = Arc::new(connect(&path.to_string_lossy()).await);
    let before = memory_stats::memory_stats().map(|m| m.physical_mem).unwrap_or(0);
    let t1 = Instant::now();
    let out = db.stream(&query("SELECT a, b, c FROM narrow"), &SessionOpts::default(), CancellationToken::new()).await.unwrap();
    let mut stream = out.stream;
    let mut rows = 0u64;
    let mut batches = 0u64;
    let mut peak = before;
    while let Some(item) = stream.next().await {
        // A fake consumer: count and drop.
        let b = item.unwrap();
        rows += b.rows();
        batches += 1;
        drop(b);
        if batches.is_multiple_of(8) {
            if let Some(m) = memory_stats::memory_stats() {
                peak = peak.max(m.physical_mem);
            }
        }
    }
    let after = memory_stats::memory_stats().map(|m| m.physical_mem).unwrap_or(0);
    peak = peak.max(after);
    let secs = t1.elapsed().as_secs_f64();
    println!(
        "streamed {rows} rows in {batches} batches in {secs:.1} s ({:.0} rows/s); RSS before {:.1} MB, peak {:.1} MB, after {:.1} MB",
        rows as f64 / secs.max(0.001),
        before as f64 / 1048576.0,
        peak as f64 / 1048576.0,
        after as f64 / 1048576.0
    );
    assert_eq!(rows, ROWS as u64);
    assert!(peak < 150 * 1024 * 1024, "RSS peaked at {} MB, the pipeline must stay under 150 MB", peak / 1048576);
}
