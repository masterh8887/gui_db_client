//! Pipeline tests: `job::run_pipeline` with an in-memory fake stream and a counting fake sink.
//! No database and no files: the stream is 300k `narrow` rows cut into 64K-row Arrow batches
//! by `ArrowBatcher` (the production batch size), and the sink only counts what it is given.
//!
//! Covered: counters on success, cancel mid-stream (and while the source is stuck), the
//! source getting one more poll after cancel (so drivers can cancel on the server), a stream
//! error, a sink error (the reader must stop although the stream is endless), the channel
//! bound (never more than `CHANNEL_DEPTH` batches queued), and empty results still producing
//! a header-only CSV / zero-row Parquet file (real SQLite source and real sinks).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use arrow_array::RecordBatch;
use bulk_core::error::{CoreError, Result};
use bulk_core::job::{run_pipeline, Progress, CANCEL_GRACE};
use bulk_core::sink::{self, Sink, SinkReport, SinkTarget};
use bulk_core::source::arrow_batch::{schema_from, ArrowBatcher, Cell, ColKind};
use bulk_core::source::sqlite::SqliteDatabase;
use bulk_core::source::{Batch, ConnectParams, Query, Source, SourceStream, SslMode, StreamOutput};
use bulk_core::testdata::{self, Lcg, NARROW_COLUMNS};
use bulk_core::types::{CsvOpts, JobPhase, OutputFormat, OutputOpts, SessionOpts, SplitOpts};
use bulk_core::{CancellationToken, BATCH_ROWS, CHANNEL_DEPTH};
use futures::{stream, StreamExt};

const ROWS: u64 = 300_000;

// ---------------------------------------------------------------------------------------------
// fake source
// ---------------------------------------------------------------------------------------------

fn narrow_batcher() -> ArrowBatcher {
    let cols: Vec<(String, ColKind)> = NARROW_COLUMNS.iter().map(|n| (n.to_string(), ColKind::Int64)).collect();
    ArrowBatcher::new(schema_from(&cols), vec![ColKind::Int64; NARROW_COLUMNS.len()])
}

/// `rows` deterministic narrow rows as Arrow batches cut at the production limits
/// (`BATCH_ROWS` / `BATCH_BYTES`): 300k rows give 4 full batches and one of 37,856 rows.
fn narrow_batches(rows: u64) -> Vec<RecordBatch> {
    let mut b = narrow_batcher();
    let mut rng = Lcg::new(42);
    let mut out = Vec::new();
    for i in 0..rows {
        for v in testdata::narrow_row(i, &mut rng) {
            b.push(Cell::I64(v)).unwrap();
        }
        if b.end_row().unwrap() {
            out.push(b.flush().unwrap().unwrap());
        }
    }
    if let Some(last) = b.flush().unwrap() {
        out.push(last);
    }
    out
}

/// One small batch, built on demand (for endless streams).
fn small_batch(first_row: u64, rows: u64) -> RecordBatch {
    let mut b = narrow_batcher();
    let mut rng = Lcg::new(first_row);
    for i in first_row..first_row + rows {
        for v in testdata::narrow_row(i, &mut rng) {
            b.push(Cell::I64(v)).unwrap();
        }
        b.end_row().unwrap();
    }
    b.flush().unwrap().unwrap()
}

fn stream_of(items: Vec<Result<Batch>>) -> SourceStream {
    SourceStream { schema: Some(narrow_batcher().schema()), stream: stream::iter(items).boxed() }
}

/// What an endless fake source reports about itself.
#[derive(Default)]
struct SourceState {
    /// Batches handed to the pipeline so far.
    produced: AtomicU64,
    /// Highest `produced - written` observed when a batch was produced.
    max_in_flight: AtomicU64,
    /// The stream was polled after `cancel` fired (where a real driver sends its server-side
    /// cancel: `cancel_query`, `KILL QUERY`, ...).
    polled_after_cancel: AtomicBool,
}

/// A never-ending stream of 1,024-row batches, each built lazily when polled. `written` is
/// the sink's batch counter, used to measure how many batches are in flight. Like the real
/// drivers it checks `cancel` first on every poll: once cancelled it records that it was
/// polled and answers `Err(Cancelled)` without producing data.
fn endless_stream(source: Arc<SourceState>, written: Arc<SinkState>, cancel: CancellationToken) -> SourceStream {
    const PER_BATCH: u64 = 1024;
    let s = stream::unfold(0u64, move |i| {
        let source = source.clone();
        let written = written.clone();
        let cancel = cancel.clone();
        async move {
            if cancel.is_cancelled() {
                source.polled_after_cancel.store(true, Ordering::SeqCst);
                return Some((Err(CoreError::Cancelled), i));
            }
            let batch = small_batch(i * PER_BATCH, PER_BATCH);
            let produced = source.produced.fetch_add(1, Ordering::SeqCst) + 1;
            let in_flight = produced.saturating_sub(written.batches.load(Ordering::SeqCst));
            source.max_in_flight.fetch_max(in_flight, Ordering::SeqCst);
            Some((Ok(Batch::Arrow(batch)), i + 1))
        }
    });
    SourceStream { schema: Some(narrow_batcher().schema()), stream: s.boxed() }
}

// ---------------------------------------------------------------------------------------------
// fake sink
// ---------------------------------------------------------------------------------------------

#[derive(Default)]
struct SinkState {
    rows: AtomicU64,
    batches: AtomicU64,
    bytes: AtomicU64,
    finished: AtomicBool,
    aborted: AtomicBool,
}

/// Counts batches; optionally fails the n-th write and/or sleeps per batch (a slow disk).
struct CountingSink {
    state: Arc<SinkState>,
    fail_at: Option<u64>,
    delay: Duration,
}

impl CountingSink {
    fn new(state: Arc<SinkState>) -> Box<Self> {
        Box::new(CountingSink { state, fail_at: None, delay: Duration::ZERO })
    }

    fn slow(state: Arc<SinkState>, delay: Duration) -> Box<Self> {
        Box::new(CountingSink { state, fail_at: None, delay })
    }

    fn failing_at(state: Arc<SinkState>, nth: u64) -> Box<Self> {
        Box::new(CountingSink { state, fail_at: Some(nth), delay: Duration::ZERO })
    }
}

impl Sink for CountingSink {
    fn write(&mut self, batch: Batch) -> Result<()> {
        let n = self.state.batches.load(Ordering::SeqCst) + 1;
        if self.fail_at == Some(n) {
            return Err(CoreError::Io("disk full (fake)".into()));
        }
        if !self.delay.is_zero() {
            std::thread::sleep(self.delay);
        }
        self.state.rows.fetch_add(batch.rows(), Ordering::SeqCst);
        self.state.bytes.fetch_add(batch.bytes(), Ordering::SeqCst);
        self.state.batches.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    fn finish(self: Box<Self>) -> Result<SinkReport> {
        self.state.finished.store(true, Ordering::SeqCst);
        Ok(SinkReport { rows: self.state.rows.load(Ordering::SeqCst), bytes: self.state.bytes.load(Ordering::SeqCst), files: vec![PathBuf::from("fake.csv")] })
    }

    fn abort(self: Box<Self>) -> Vec<PathBuf> {
        self.state.aborted.store(true, Ordering::SeqCst);
        vec![PathBuf::from("fake.csv.partial")]
    }

    fn bytes_out(&self) -> u64 {
        self.state.bytes.load(Ordering::SeqCst)
    }

    fn files_so_far(&self) -> u32 {
        u32::from(self.state.batches.load(Ordering::SeqCst) > 0)
    }
}

/// Poll until the sink has written at least `batches` batches (bounded wait).
async fn wait_for_batches(state: &SinkState, batches: u64) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while state.batches.load(Ordering::SeqCst) < batches {
        assert!(tokio::time::Instant::now() < deadline, "sink never reached {batches} batches");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

// ---------------------------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn streams_300k_rows_in_64k_batches() {
    let batches = narrow_batches(ROWS);
    assert_eq!(batches.len(), 5, "300k rows at 64K per batch");
    assert_eq!(batches[0].num_rows(), BATCH_ROWS);
    assert_eq!(batches.iter().map(|b| b.num_rows() as u64).sum::<u64>(), ROWS);
    let expected_bytes_in: u64 = batches.iter().map(|b| b.get_array_memory_size() as u64).sum();

    let stream = stream_of(batches.into_iter().map(|b| Ok(Batch::Arrow(b))).collect());
    let state = Arc::new(SinkState::default());
    let progress = Arc::new(Progress::new());
    let report = run_pipeline(stream, CountingSink::new(state.clone()), progress.clone(), CancellationToken::new()).await.expect("pipeline succeeds");

    assert_eq!(report.rows, ROWS);
    assert_eq!(report.bytes, expected_bytes_in);
    assert_eq!(report.files, vec![PathBuf::from("fake.csv")]);
    assert!(state.finished.load(Ordering::SeqCst));
    assert!(!state.aborted.load(Ordering::SeqCst));
    assert_eq!(state.batches.load(Ordering::SeqCst), 5);

    assert_eq!(progress.rows.load(Ordering::SeqCst), ROWS);
    assert_eq!(progress.batches.load(Ordering::SeqCst), 5);
    assert_eq!(progress.bytes_in.load(Ordering::SeqCst), expected_bytes_in);
    assert_eq!(progress.bytes_out.load(Ordering::SeqCst), report.bytes);
    assert_eq!(progress.files.load(Ordering::SeqCst), 1);
    assert_eq!(progress.phase(), JobPhase::Finishing);
}

#[tokio::test]
async fn empty_stream_finishes_with_zero_rows() {
    let state = Arc::new(SinkState::default());
    let progress = Arc::new(Progress::new());
    let report = run_pipeline(stream_of(Vec::new()), CountingSink::new(state.clone()), progress.clone(), CancellationToken::new()).await.unwrap();
    assert_eq!(report.rows, 0);
    assert!(state.finished.load(Ordering::SeqCst));
    // The source announced a schema: the sink got exactly one zero-row batch carrying it (so it
    // can write a header / Parquet schema). It is not a data batch for the progress counters.
    assert_eq!(state.batches.load(Ordering::SeqCst), 1);
    assert_eq!(state.rows.load(Ordering::SeqCst), 0);
    assert_eq!(progress.batches.load(Ordering::SeqCst), 0);
    assert_eq!(progress.rows.load(Ordering::SeqCst), 0);
    assert_eq!(progress.phase(), JobPhase::Finishing);
}

#[tokio::test]
async fn empty_stream_without_schema_writes_no_batch() {
    let state = Arc::new(SinkState::default());
    let stream = SourceStream { schema: None, stream: stream::iter(Vec::<Result<Batch>>::new()).boxed() };
    let report = run_pipeline(stream, CountingSink::new(state.clone()), Arc::new(Progress::new()), CancellationToken::new()).await.unwrap();
    assert_eq!(report.rows, 0);
    assert!(state.finished.load(Ordering::SeqCst));
    assert_eq!(state.batches.load(Ordering::SeqCst), 0, "nothing to describe the columns with");
}

#[tokio::test]
async fn schema_batch_is_not_added_when_data_arrived() {
    let state = Arc::new(SinkState::default());
    let stream = stream_of(vec![Ok(Batch::Arrow(small_batch(0, 10)))]);
    let report = run_pipeline(stream, CountingSink::new(state.clone()), Arc::new(Progress::new()), CancellationToken::new()).await.unwrap();
    assert_eq!(report.rows, 10);
    assert_eq!(state.batches.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_mid_stream_returns_cancelled_and_aborts() {
    let source = Arc::new(SourceState::default());
    let state = Arc::new(SinkState::default());
    let progress = Arc::new(Progress::new());
    let cancel = CancellationToken::new();
    let stream = endless_stream(source.clone(), state.clone(), cancel.clone());
    let task = tokio::spawn(run_pipeline(stream, CountingSink::slow(state.clone(), Duration::from_millis(2)), progress.clone(), cancel.clone()));

    wait_for_batches(&state, 3).await;
    assert_eq!(progress.phase(), JobPhase::Transferring);
    cancel.cancel();

    let outcome = tokio::time::timeout(Duration::from_secs(10), task).await.expect("pipeline stops after cancel").unwrap();
    assert!(matches!(outcome, Err(CoreError::Cancelled)), "{outcome:?}");
    assert!(state.aborted.load(Ordering::SeqCst), "abort() must be called on cancel");
    assert!(!state.finished.load(Ordering::SeqCst), "finish() must not be called on cancel");
    assert_eq!(progress.rows.load(Ordering::SeqCst), state.rows.load(Ordering::SeqCst));
    assert_eq!(progress.batches.load(Ordering::SeqCst), state.batches.load(Ordering::SeqCst));
    // The reader stopped promptly: at most the channel plus one batch in each hand.
    let produced = source.produced.load(Ordering::SeqCst);
    let written = state.batches.load(Ordering::SeqCst);
    assert!(produced <= written + CHANNEL_DEPTH as u64 + 2, "produced {produced}, written {written}");
    // ... but it polled the source once more so the driver could cancel on the server.
    assert!(source.polled_after_cancel.load(Ordering::SeqCst), "the source must see the cancel before it is dropped");
}

/// The source is waiting for the server (no batch ready) when the user cancels. The reader must
/// not just drop it: the driver only sends its server-side cancel (PostgreSQL `cancel_query`)
/// from inside the stream, so the stream has to be polled after the token fired.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_polls_a_waiting_source_so_it_can_cancel_on_the_server() {
    let cancel = CancellationToken::new();
    let server_cancel_sent = Arc::new(AtomicBool::new(false));
    let s = stream::unfold((), {
        let cancel = cancel.clone();
        let sent = server_cancel_sent.clone();
        move |()| {
            let cancel = cancel.clone();
            let sent = sent.clone();
            async move {
                // "The server is still running the query": nothing until the token fires.
                cancel.cancelled().await;
                // Where a real driver runs `canceller.cancel_query()` / `KILL QUERY`.
                sent.store(true, Ordering::SeqCst);
                Some((Err(CoreError::Cancelled), ()))
            }
        }
    });
    let stream = SourceStream { schema: Some(narrow_batcher().schema()), stream: s.boxed() };
    let state = Arc::new(SinkState::default());
    let task = tokio::spawn(run_pipeline(stream, CountingSink::new(state.clone()), Arc::new(Progress::new()), cancel.clone()));

    tokio::time::sleep(Duration::from_millis(50)).await;
    let t0 = Instant::now();
    cancel.cancel();
    let outcome = tokio::time::timeout(Duration::from_secs(10), task).await.expect("pipeline stops after cancel").unwrap();

    assert!(matches!(outcome, Err(CoreError::Cancelled)), "{outcome:?}");
    assert!(server_cancel_sent.load(Ordering::SeqCst), "the source was dropped without seeing the cancel");
    assert!(t0.elapsed() < CANCEL_GRACE, "a source that answers the cancel at once must not cost the grace period ({:?})", t0.elapsed());
    assert!(state.aborted.load(Ordering::SeqCst));
    assert!(!state.finished.load(Ordering::SeqCst));
    assert_eq!(state.batches.load(Ordering::SeqCst), 0, "no schema batch on cancel");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_while_source_is_pending() {
    let stream = SourceStream { schema: None, stream: stream::pending().boxed() };
    let state = Arc::new(SinkState::default());
    let progress = Arc::new(Progress::new());
    let cancel = CancellationToken::new();
    let task = tokio::spawn(run_pipeline(stream, CountingSink::new(state.clone()), progress.clone(), cancel.clone()));

    tokio::time::sleep(Duration::from_millis(50)).await;
    cancel.cancel();

    // A source that never answers is given at most CANCEL_GRACE, then dropped.
    let outcome = tokio::time::timeout(CANCEL_GRACE + Duration::from_secs(8), task).await.expect("reader must leave a pending source on cancel").unwrap();
    assert!(matches!(outcome, Err(CoreError::Cancelled)), "{outcome:?}");
    assert!(state.aborted.load(Ordering::SeqCst));
    assert!(!state.finished.load(Ordering::SeqCst));
    assert_eq!(progress.rows.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn stream_error_propagates_and_aborts() {
    let good: Vec<RecordBatch> = (0..2).map(|i| small_batch(i * 100, 100)).collect();
    let mut items: Vec<Result<Batch>> = good.into_iter().map(|b| Ok(Batch::Arrow(b))).collect();
    items.push(Err(CoreError::query("relation \"orders\" vanished")));
    items.push(Ok(Batch::Arrow(small_batch(1000, 100))));

    let state = Arc::new(SinkState::default());
    let progress = Arc::new(Progress::new());
    let outcome = run_pipeline(stream_of(items), CountingSink::new(state.clone()), progress.clone(), CancellationToken::new()).await;

    let err = outcome.expect_err("stream error must propagate");
    assert!(matches!(err, CoreError::Query { .. }), "{err:?}");
    assert_eq!(err.code(), "QUERY");
    assert!(err.to_string().contains("vanished"));
    assert!(state.aborted.load(Ordering::SeqCst), "abort() must be called on a stream error");
    assert!(!state.finished.load(Ordering::SeqCst));
    assert_eq!(state.batches.load(Ordering::SeqCst), 2, "batches before the error are written");
    assert_eq!(progress.rows.load(Ordering::SeqCst), 200);
    assert_eq!(progress.batches.load(Ordering::SeqCst), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sink_error_stops_the_reader() {
    let source = Arc::new(SourceState::default());
    let state = Arc::new(SinkState::default());
    let progress = Arc::new(Progress::new());
    let stream = endless_stream(source.clone(), state.clone(), CancellationToken::new());

    // The stream never ends: the pipeline only returns if the reader notices the closed channel.
    let outcome =
        tokio::time::timeout(Duration::from_secs(10), run_pipeline(stream, CountingSink::failing_at(state.clone(), 3), progress.clone(), CancellationToken::new()))
            .await
            .expect("reader must stop when the writer fails");

    let err = outcome.expect_err("sink error must propagate");
    assert!(matches!(err, CoreError::Io(_)), "{err:?}");
    assert_eq!(err.code(), "IO");
    assert!(state.aborted.load(Ordering::SeqCst), "abort() must be called on a sink error");
    assert!(!state.finished.load(Ordering::SeqCst));
    assert_eq!(state.batches.load(Ordering::SeqCst), 2);
    assert_eq!(progress.batches.load(Ordering::SeqCst), 2);
    let produced = source.produced.load(Ordering::SeqCst);
    assert!(produced <= 2 + CHANNEL_DEPTH as u64 + 2, "reader kept producing after the sink failed: {produced}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn never_more_than_channel_depth_batches_in_flight() {
    let source = Arc::new(SourceState::default());
    let state = Arc::new(SinkState::default());
    let progress = Arc::new(Progress::new());
    let cancel = CancellationToken::new();
    let stream = endless_stream(source.clone(), state.clone(), cancel.clone());
    // A slow sink so the reader is always ahead and the channel fills up.
    let task = tokio::spawn(run_pipeline(stream, CountingSink::slow(state.clone(), Duration::from_millis(5)), progress, cancel.clone()));

    wait_for_batches(&state, 20).await;
    cancel.cancel();
    let outcome = tokio::time::timeout(Duration::from_secs(10), task).await.unwrap().unwrap();
    assert!(matches!(outcome, Err(CoreError::Cancelled)));

    let max_in_flight = source.max_in_flight.load(Ordering::SeqCst);
    // Channel (CHANNEL_DEPTH) + the batch the reader is sending + the batch the writer holds.
    assert!(max_in_flight <= CHANNEL_DEPTH as u64 + 2, "in flight peaked at {max_in_flight}");
    assert!(max_in_flight >= CHANNEL_DEPTH as u64, "the channel never filled ({max_in_flight}); the test is not exercising back-pressure");
}

// ---------------------------------------------------------------------------------------------
// empty results through a real source and real sinks
// ---------------------------------------------------------------------------------------------

fn sqlite_params(path: &Path) -> ConnectParams {
    let mut fields = BTreeMap::new();
    fields.insert("path".to_string(), path.to_string_lossy().into_owned());
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

/// Run `sql` against a small SQLite fixture into the sink described by `opts`.
async fn export_sqlite(dir: &Path, sql: &str, opts: OutputOpts) -> SinkReport {
    let db_path = dir.join("fixture.db");
    testdata::sqlite_fixture(&db_path, 0, 100, 0).unwrap();
    let db = SqliteDatabase::connect(&sqlite_params(&db_path)).await.unwrap();
    let query = Query { sql: sql.to_string(), output: StreamOutput::Arrow };
    let cancel = CancellationToken::new();
    let stream = db.stream(&query, &SessionOpts::default(), cancel.clone()).await.unwrap();
    assert!(stream.schema.is_some(), "the SQLite source announces its schema up front");
    let out_dir = dir.join("out");
    std::fs::create_dir_all(&out_dir).unwrap();
    let target = SinkTarget { dir: out_dir, stem: "empty".into(), opts };
    let sink = sink::build(&target).unwrap();
    run_pipeline(stream, sink, Arc::new(Progress::new()), cancel).await.expect("an empty result is a successful export")
}

fn csv_output(split: Option<SplitOpts>) -> OutputOpts {
    OutputOpts {
        format: OutputFormat::Csv,
        csv: Some(CsvOpts { header: true, delimiter: ",".into(), null_as: String::new(), bom: false, crlf: false }),
        split,
        ..OutputOpts::default()
    }
}

#[tokio::test]
async fn empty_sqlite_result_writes_a_header_only_csv() {
    let dir = tempfile::tempdir().unwrap();
    let report = export_sqlite(dir.path(), "select * from narrow where 0", csv_output(None)).await;
    assert_eq!(report.rows, 0);
    assert_eq!(report.files.len(), 1, "{:?}", report.files);
    let text = std::fs::read_to_string(&report.files[0]).unwrap();
    assert_eq!(text, "a,b,c\n", "header only");
}

#[tokio::test]
async fn empty_sqlite_result_with_split_writes_one_header_only_part() {
    let dir = tempfile::tempdir().unwrap();
    let report = export_sqlite(dir.path(), "select a, b from narrow where 0", csv_output(Some(SplitOpts { rows: Some(10), megabytes: None }))).await;
    assert_eq!(report.rows, 0);
    assert_eq!(report.files.len(), 1, "{:?}", report.files);
    assert_eq!(std::fs::read_to_string(&report.files[0]).unwrap(), "a,b\n");
}

#[tokio::test]
async fn empty_sqlite_result_writes_a_zero_row_parquet_file() {
    use parquet::file::reader::{FileReader, SerializedFileReader};

    let dir = tempfile::tempdir().unwrap();
    let opts = OutputOpts { format: OutputFormat::Parquet, ..OutputOpts::default() };
    let report = export_sqlite(dir.path(), "select * from narrow where 0", opts).await;
    assert_eq!(report.rows, 0);
    assert_eq!(report.files.len(), 1, "an empty result must still produce a Parquet file: {:?}", report.files);
    let reader = SerializedFileReader::new(std::fs::File::open(&report.files[0]).unwrap()).expect("a valid Parquet file");
    let meta = reader.metadata().file_metadata();
    assert_eq!(meta.num_rows(), 0);
    let columns: Vec<String> = meta.schema_descr().columns().iter().map(|c| c.name().to_string()).collect();
    assert_eq!(columns, vec!["a", "b", "c"]);
}
