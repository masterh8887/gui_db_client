//! SQLite driver (rusqlite 0.37, bundled; synchronous API driven from blocking threads).
//!
//! * The connection lives in a `Mutex<Option<rusqlite::Connection>>` because `Connection` is
//!   `!Sync`; every database call runs on `tokio::task::spawn_blocking` with a cloned `Arc`.
//! * `stream`: rows are stepped on a blocking thread, decoded into an [`ArrowBatcher`] and
//!   pushed through a bounded `mpsc::channel(2)`. The thread blocks on `blocking_send` when
//!   the consumer is slow; that is the back-pressure. The schema is known after `prepare`
//!   and is handed back through a oneshot before the first row is stepped.
//! * Cancel: a watcher task calls `InterruptHandle::interrupt()` when the token fires;
//!   `SQLITE_INTERRUPT` maps to `CoreError::Cancelled` in `error.rs`. Dropping the stream
//!   interrupts the reader as well, so no thread lingers behind an abandoned query.
//! * Probe: `sqlite_version()`, file size, page size / count, journal mode, encoding.
//! * EXPLAIN: `EXPLAIN QUERY PLAN`, rendered as the `sqlite3` shell does (text only).
//! * The catalog lives here too (`catalog/sqlite.rs` is just a pointer to this file).
//! * Also serves as the test database for the whole pipeline (no docker needed).
//!
//! Opened read-only (`SQLITE_OPEN_READ_ONLY | NO_MUTEX | URI`, `PRAGMA query_only = 1`,
//! `busy_timeout` 5 s). `:memory:` is the only path opened read-write (there is nothing to
//! protect and the tests need to fill it).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::{CStr, CString};
use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use arrow_schema::SchemaRef;
use async_trait::async_trait;
use rusqlite::types::ValueRef;
use rusqlite::{InterruptHandle, OpenFlags};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::{CancellationToken, DropGuard};

use super::arrow_batch::{schema_from, ArrowBatcher, Cell, ColKind};
use super::{Batch, BoxStream, ConnectParams, Database, Query, Source, SourceStream};
use crate::catalog::Catalog;
use crate::error::{CoreError, Result};
use crate::monitor::{JobHandle, Monitor, ServerSnapshot};
use crate::types::{ColumnInfo, DatabaseInfo, IndexInfo, PlanNode, PlanSummary, SchemaInfo, ServerInfo, SessionInfo, SessionOpts, TableInfo, TableKind, TableRef};
use crate::{BATCH_BYTES, BATCH_ROWS};

/// Batches in flight between the reader thread and the async stream. Small on purpose: the
/// job pipeline adds its own channel of `CHANNEL_DEPTH`; this one only decouples the
/// blocking thread from the runtime.
const STREAM_CHANNEL_DEPTH: usize = 2;
/// `busy_timeout` applied to every connection.
const BUSY_TIMEOUT: Duration = Duration::from_millis(5000);
/// Schema name used when a `TableRef` carries none.
const MAIN: &str = "main";

type SharedConn = Arc<Mutex<Option<rusqlite::Connection>>>;

/// One SQLite database file (or `:memory:`): source, catalog and monitor in one object.
pub struct SqliteDatabase {
    /// Path exactly as given (`:memory:`, a file path or a `file:` URI).
    path: String,
    in_memory: bool,
    /// `true` when opened with `SQLITE_OPEN_READ_ONLY`.
    read_only: bool,
    conn: SharedConn,
    interrupt: Arc<InterruptHandle>,
    /// `fetch_batch_rows` from the last `apply_session`; `0` means "not set".
    fetch_batch_rows: AtomicU32,
}

impl std::fmt::Debug for SqliteDatabase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SqliteDatabase").field("path", &self.path).field("read_only", &self.read_only).finish_non_exhaustive()
    }
}

impl SqliteDatabase {
    /// Open the database named by `params.field("path")` (or `params.database` as a fallback).
    ///
    /// `:memory:` is allowed; any other plain path must exist (`CoreError::Connect` with the
    /// path otherwise). `file:` URIs are passed to SQLite untouched.
    pub async fn connect(params: &ConnectParams) -> Result<Self> {
        let path = params
            .field("path")
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .or_else(|| params.database.as_deref().map(str::trim).filter(|s| !s.is_empty()))
            .ok_or_else(|| CoreError::Invalid("sqlite: a database file path is required (field 'path')".into()))?
            .to_string();
        let in_memory = is_memory_path(&path);
        if !in_memory && !path.starts_with("file:") && !Path::new(&path).is_file() {
            return Err(CoreError::Connect(format!("sqlite database file not found: {path}")));
        }
        let read_only = !in_memory;
        let flags = if read_only {
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX | OpenFlags::SQLITE_OPEN_URI
        } else {
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE | OpenFlags::SQLITE_OPEN_NO_MUTEX | OpenFlags::SQLITE_OPEN_URI
        };
        let open_path = path.clone();
        let conn = tokio::task::spawn_blocking(move || -> Result<rusqlite::Connection> {
            let conn =
                rusqlite::Connection::open_with_flags(&open_path, flags).map_err(|e| CoreError::Connect(format!("cannot open sqlite database {open_path}: {e}")))?;
            conn.busy_timeout(BUSY_TIMEOUT).map_err(|e| CoreError::Connect(format!("busy_timeout: {e}")))?;
            // `query_only` rejects writes even on the read-write `:memory:` connection: this
            // driver only ever reads.
            conn.pragma_update(None, "query_only", 1).map_err(|e| CoreError::Connect(format!("query_only: {e}")))?;
            // Touch the schema so that a file that is not a database fails here rather than
            // on the first catalog call.
            conn.query_row("SELECT count(*) FROM sqlite_master", [], |r| r.get::<_, i64>(0))
                .map_err(|e| CoreError::Connect(format!("{open_path} is not a readable sqlite database: {e}")))?;
            Ok(conn)
        })
        .await??;
        let interrupt = Arc::new(conn.get_interrupt_handle());
        Ok(SqliteDatabase { path, in_memory, read_only, conn: Arc::new(Mutex::new(Some(conn))), interrupt, fetch_batch_rows: AtomicU32::new(0) })
    }

    /// The path this database was opened with.
    pub fn path(&self) -> &str {
        &self.path
    }

    /// Whether the connection was opened with `SQLITE_OPEN_READ_ONLY`.
    pub fn is_read_only(&self) -> bool {
        self.read_only
    }

    /// Interrupt whatever statement is running right now (no-op when idle).
    pub fn interrupt(&self) {
        self.interrupt.interrupt();
    }

    /// Close the connection. Interrupts a running statement first and waits for it to
    /// release the connection. Every later call fails with `CoreError::Connect`.
    pub async fn close(&self) -> Result<()> {
        self.interrupt.interrupt();
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || -> Result<()> {
            let mut guard = lock_conn(&conn);
            match guard.take() {
                Some(c) => c.close().map_err(|(_, e)| CoreError::Io(format!("sqlite close: {e}"))),
                None => Ok(()),
            }
        })
        .await?
    }

    /// Run `f` with the connection on a blocking thread.
    async fn with_conn<T, F>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&rusqlite::Connection) -> Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let guard = lock_conn(&conn);
            let c = guard.as_ref().ok_or_else(closed)?;
            f(c)
        })
        .await?
    }

    /// Spawn a watcher that interrupts the connection when `token` is cancelled. The
    /// returned guard stops the watcher when dropped; drop it before releasing the
    /// connection so a late interrupt can never hit somebody else's statement.
    fn interrupt_on_cancel(&self, token: CancellationToken) -> DropGuard {
        let done = CancellationToken::new();
        let guard = done.clone().drop_guard();
        let interrupt = self.interrupt.clone();
        tokio::spawn(async move {
            tokio::select! {
                biased;
                _ = done.cancelled() => {}
                _ = token.cancelled() => interrupt.interrupt(),
            }
        });
        guard
    }

    /// Rows per batch: the call's `fetch_batch_rows`, else the session's, else `BATCH_ROWS`;
    /// never above `BATCH_ROWS` (rule 2).
    fn batch_rows(&self, opts: &SessionOpts) -> usize {
        opts.fetch_batch_rows
            .filter(|n| *n > 0)
            .or_else(|| Some(self.fetch_batch_rows.load(Ordering::Relaxed)).filter(|n| *n > 0))
            .map(|n| n as usize)
            .unwrap_or(BATCH_ROWS)
            .clamp(1, BATCH_ROWS)
    }
}

fn is_memory_path(path: &str) -> bool {
    path == ":memory:" || path.starts_with("file::memory:") || (path.starts_with("file:") && path.contains("mode=memory"))
}

fn closed() -> CoreError {
    CoreError::Connect("sqlite connection is closed".into())
}

/// Lock the connection, recovering from poisoning (a panic elsewhere does not invalidate
/// the SQLite handle itself).
fn lock_conn(conn: &SharedConn) -> MutexGuard<'_, Option<rusqlite::Connection>> {
    conn.lock().unwrap_or_else(|p| p.into_inner())
}

/// Double-quote an identifier for SQLite.
fn quote(ident: &str) -> String {
    format!("\"{}\"", ident.replace('"', "\"\""))
}

/// Schema a `TableRef` points at: `schema`, else `database`, else `main`.
fn schema_of(table: &TableRef) -> String {
    table.schema.as_deref().filter(|s| !s.is_empty()).or_else(|| table.database.as_deref().filter(|s| !s.is_empty())).unwrap_or(MAIN).to_string()
}

fn qualified(schema: &str, name: &str) -> String {
    format!("{}.{}", quote(schema), quote(name))
}

// ---------------------------------------------------------------------------------------------
// Source
// ---------------------------------------------------------------------------------------------

#[async_trait]
impl Source for SqliteDatabase {
    async fn probe(&self) -> Result<ServerInfo> {
        let path = self.path.clone();
        let in_memory = self.in_memory;
        let read_only = self.read_only;
        self.with_conn(move |c| {
            let version: String = c.query_row("SELECT sqlite_version()", [], |r| r.get(0))?;
            let page_size: i64 = c.pragma_query_value(None, "page_size", |r| r.get(0))?;
            let page_count: i64 = c.pragma_query_value(None, "page_count", |r| r.get(0))?;
            let journal_mode: String = c.pragma_query_value(None, "journal_mode", |r| r.get(0))?;
            let encoding: String = c.pragma_query_value(None, "encoding", |r| r.get(0))?;
            let file_size = if in_memory { None } else { std::fs::metadata(&path).ok().map(|m| m.len()) };

            let mut settings = BTreeMap::new();
            settings.insert("sqlite_version".to_string(), version.clone());
            settings.insert("path".to_string(), path.clone());
            if let Some(size) = file_size {
                settings.insert("file_size_bytes".to_string(), size.to_string());
            }
            settings.insert("page_size".to_string(), page_size.to_string());
            settings.insert("page_count".to_string(), page_count.to_string());
            settings.insert("journal_mode".to_string(), journal_mode);
            settings.insert("encoding".to_string(), encoding);
            settings.insert("read_only".to_string(), read_only.to_string());
            settings.insert("query_only".to_string(), "true".to_string());

            let mut notes = vec!["local database file: there is no server session to monitor".to_string()];
            if in_memory {
                notes.push("in-memory database: contents vanish when the connection closes".to_string());
            }
            Ok(ServerInfo {
                driver: "sqlite".into(),
                version,
                user: None,
                database: Some(path),
                ram_bytes: None,
                ram_source: None,
                settings,
                can_write: Some(!read_only),
                notes,
            })
        })
        .await
    }

    /// Only `fetch_batch_rows` matters for SQLite; everything else is a server-side notion.
    async fn apply_session(&self, opts: &SessionOpts) -> Result<()> {
        self.fetch_batch_rows.store(opts.fetch_batch_rows.unwrap_or(0), Ordering::Relaxed);
        Ok(())
    }

    /// Always yields `Batch::Arrow`: `q.output` is not consulted because this driver reports
    /// `supports_raw_csv() == false`, so the pipeline never asks it for raw CSV bytes.
    async fn stream(&self, q: &Query, opts: &SessionOpts, cancel: CancellationToken) -> Result<SourceStream> {
        let sql = q.sql.trim().trim_end_matches(';').trim().to_string();
        if sql.is_empty() {
            return Err(CoreError::Invalid("empty query".into()));
        }
        let max_rows = self.batch_rows(opts);
        // `abort` fires on cancel *and* when the stream is dropped, so an abandoned query is
        // interrupted too.
        let abort = cancel.child_token();
        let drop_guard = abort.clone().drop_guard();
        let watcher = self.interrupt_on_cancel(abort.clone());
        let (schema_tx, schema_rx) = oneshot::channel::<Result<SchemaRef>>();
        let (tx, rx) = mpsc::channel::<Result<Batch>>(STREAM_CHANNEL_DEPTH);
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || read_rows(conn, sql, max_rows, abort, Reply { schema_tx: Some(schema_tx), tx }, watcher));
        let schema = match schema_rx.await {
            Ok(Ok(schema)) => schema,
            Ok(Err(e)) => return Err(e),
            Err(_) => return Err(CoreError::Internal("sqlite reader thread exited before sending the schema".into())),
        };
        Ok(SourceStream { schema: Some(schema), stream: batch_stream(rx, cancel, drop_guard) })
    }

    /// `EXPLAIN QUERY PLAN`. SQLite has no `EXPLAIN ANALYZE`; `analyze` is ignored and the
    /// result is always planner-only (`analyzed = false`).
    async fn explain(&self, sql: &str, _analyze: bool) -> Result<Option<PlanSummary>> {
        let started = Instant::now();
        let body = sql.trim().trim_end_matches(';').trim().to_string();
        if body.is_empty() {
            return Err(CoreError::Invalid("empty query".into()));
        }
        let rows = self
            .with_conn(move |c| {
                let mut stmt = c.prepare(&format!("EXPLAIN QUERY PLAN {body}"))?;
                let mut rows = stmt.query([])?;
                let mut out = Vec::new();
                while let Some(r) = rows.next()? {
                    out.push(EqpRow { id: r.get(0)?, parent: r.get(1)?, detail: r.get::<_, String>(3)? });
                }
                Ok(out)
            })
            .await?;
        let (root, tables, raw) = build_plan(&rows);
        Ok(Some(PlanSummary {
            estimated_rows: None,
            estimated_width: None,
            total_cost: None,
            estimated_bytes: None,
            analyzed: false,
            elapsed_ms: started.elapsed().as_millis() as u64,
            warnings: Vec::new(),
            tables,
            root: Some(root),
            raw,
            raw_format: "text".into(),
        }))
    }

    fn session_id(&self) -> Option<String> {
        None
    }

    fn driver_id(&self) -> &'static str {
        "sqlite"
    }
}

// ---------------------------------------------------------------------------------------------
// Streaming (blocking side)
// ---------------------------------------------------------------------------------------------

/// Where the reader thread reports to: the schema oneshot until it is used, the batch
/// channel afterwards.
struct Reply {
    schema_tx: Option<oneshot::Sender<Result<SchemaRef>>>,
    tx: mpsc::Sender<Result<Batch>>,
}

impl Reply {
    /// `false` when the consumer is gone.
    fn schema(&mut self, schema: SchemaRef) -> bool {
        match self.schema_tx.take() {
            Some(tx) => tx.send(Ok(schema)).is_ok(),
            None => false,
        }
    }

    /// Blocks while the channel is full (back-pressure). `false` when the consumer is gone.
    fn batch(&self, batch: arrow_array::RecordBatch) -> bool {
        self.tx.blocking_send(Ok(Batch::Arrow(batch))).is_ok()
    }

    fn fail(&mut self, e: CoreError) {
        match self.schema_tx.take() {
            Some(tx) => {
                let _ = tx.send(Err(e));
            }
            None => {
                let _ = self.tx.blocking_send(Err(e));
            }
        }
    }
}

/// Entry point of the reader thread: lock the connection, run the query, report the outcome.
fn read_rows(conn: SharedConn, sql: String, max_rows: usize, abort: CancellationToken, mut reply: Reply, watcher: DropGuard) {
    let guard = lock_conn(&conn);
    // Declared after `guard` so it is dropped first: the interrupt watcher stops before the
    // connection is released to the next caller.
    let _watcher = watcher;
    let outcome = match guard.as_ref() {
        None => Err(closed()),
        Some(c) => run_query(c, &sql, max_rows, &abort, &mut reply),
    };
    if let Err(e) = outcome {
        reply.fail(e);
    }
}

/// Prepare, derive the schema, step every row into the batcher and ship batches.
fn run_query(conn: &rusqlite::Connection, sql: &str, max_rows: usize, abort: &CancellationToken, reply: &mut Reply) -> Result<()> {
    if abort.is_cancelled() {
        return Err(CoreError::Cancelled);
    }
    let declared = declared_types(conn, sql);
    let mut stmt = conn.prepare(sql)?;
    let params = stmt.parameter_count();
    if params > 0 {
        return Err(CoreError::Invalid(format!("query has {params} bind parameter(s); extraction SQL must not use placeholders")));
    }
    let names: Vec<String> = stmt.column_names().into_iter().map(String::from).collect();
    if names.is_empty() {
        return Err(CoreError::Invalid("statement returns no columns".into()));
    }
    if abort.is_cancelled() {
        return Err(CoreError::Cancelled);
    }
    let mut rows = stmt.raw_query();
    // The first row decides the kind of every column without a declared type (and widens
    // declared numeric columns whose first value is not numeric).
    let first = rows.next()?;
    let kinds: Vec<ColKind> = (0..names.len())
        .map(|i| {
            let decl = declared.get(i).and_then(|d| d.as_deref());
            let value = first.and_then(|row| row.get_ref(i).ok());
            column_kind(decl, value)
        })
        .collect();
    let cols: Vec<(String, ColKind)> = names.iter().cloned().zip(kinds.iter().copied()).collect();
    let schema = schema_from(&cols);
    if !reply.schema(schema.clone()) {
        return Ok(());
    }
    let mut batcher = ArrowBatcher::with_limits(schema, kinds.clone(), max_rows, BATCH_BYTES);
    let warned = vec![false; kinds.len()];
    let mut codec = RowCodec { kinds, names, warned };

    if let Some(row) = first {
        codec.push_row(&mut batcher, row)?;
        if batcher.end_row()? && !ship(&mut batcher, abort, reply)? {
            return Ok(());
        }
    }
    while let Some(row) = rows.next()? {
        codec.push_row(&mut batcher, row)?;
        if batcher.end_row()? && !ship(&mut batcher, abort, reply)? {
            return Ok(());
        }
    }
    ship(&mut batcher, abort, reply)?;
    Ok(())
}

/// Flush the batcher and send the batch. `Ok(false)` when the consumer went away.
fn ship(batcher: &mut ArrowBatcher, abort: &CancellationToken, reply: &Reply) -> Result<bool> {
    if abort.is_cancelled() {
        return Err(CoreError::Cancelled);
    }
    match batcher.flush()? {
        Some(batch) => Ok(reply.batch(batch)),
        None => Ok(true),
    }
}

/// Declared types of the result columns of `sql` (`sqlite3_column_decltype`), `None` for
/// expression columns. Empty when the statement does not compile; `Connection::prepare`
/// reports that error properly afterwards.
///
/// rusqlite exposes this only behind its `column_decltype` feature, so the statement is
/// compiled once more through the raw C API just to read the declarations.
fn declared_types(conn: &rusqlite::Connection, sql: &str) -> Vec<Option<String>> {
    use rusqlite::ffi;
    let Ok(c_sql) = CString::new(sql) else {
        return Vec::new();
    };
    let mut stmt: *mut ffi::sqlite3_stmt = std::ptr::null_mut();
    // SAFETY: the connection handle is live (the caller holds the connection mutex for the
    // whole call), `c_sql` outlives the prepare call, and the statement is finalised before
    // returning, so no pointer escapes this function.
    unsafe {
        let rc = ffi::sqlite3_prepare_v2(conn.handle(), c_sql.as_ptr(), -1, &mut stmt, std::ptr::null_mut());
        if rc != ffi::SQLITE_OK || stmt.is_null() {
            if !stmt.is_null() {
                ffi::sqlite3_finalize(stmt);
            }
            return Vec::new();
        }
        let n = ffi::sqlite3_column_count(stmt).max(0) as usize;
        let out = (0..n)
            .map(|i| {
                let p = ffi::sqlite3_column_decltype(stmt, i as std::os::raw::c_int);
                if p.is_null() {
                    None
                } else {
                    Some(CStr::from_ptr(p).to_string_lossy().into_owned())
                }
            })
            .collect();
        ffi::sqlite3_finalize(stmt);
        out
    }
}

/// Map a declared type to a column kind following SQLite's affinity rules (in their order):
/// `INT` -> Int64; `CHAR`/`CLOB`/`TEXT` -> Utf8; `BLOB` -> Binary; `REAL`/`FLOA`/`DOUB` ->
/// Float64; anything else (`NUMERIC`, `DECIMAL`, `BOOLEAN`, `DATE`, ...) -> Utf8. An empty
/// declaration counts as missing.
fn kind_from_decltype(decl: &str) -> Option<ColKind> {
    let d = decl.trim().to_ascii_uppercase();
    if d.is_empty() {
        return None;
    }
    Some(if d.contains("INT") {
        ColKind::Int64
    } else if d.contains("CHAR") || d.contains("CLOB") || d.contains("TEXT") {
        ColKind::Utf8
    } else if d.contains("BLOB") {
        ColKind::Binary
    } else if d.contains("REAL") || d.contains("FLOA") || d.contains("DOUB") {
        ColKind::Float64
    } else {
        ColKind::Utf8
    })
}

/// Kind of a result column from its declaration and the first row's value.
fn column_kind(decl: Option<&str>, first: Option<ValueRef<'_>>) -> ColKind {
    match decl.and_then(kind_from_decltype) {
        Some(kind) => widen(kind, first),
        None => match first {
            Some(ValueRef::Integer(_)) => ColKind::Int64,
            Some(ValueRef::Real(_)) => ColKind::Float64,
            Some(ValueRef::Blob(_)) => ColKind::Binary,
            _ => ColKind::Utf8,
        },
    }
}

/// A declared numeric column whose first value cannot be represented as declared is widened
/// up front (Int64 -> Float64 for fractions, anything -> Utf8 for text) instead of
/// producing NULLs later.
fn widen(kind: ColKind, first: Option<ValueRef<'_>>) -> ColKind {
    match (kind, first) {
        (ColKind::Int64, Some(ValueRef::Real(f))) if !is_integral(f) => ColKind::Float64,
        (ColKind::Int64, Some(ValueRef::Text(t))) if parse_i64(t).is_none() => {
            if parse_f64(t).is_some() {
                ColKind::Float64
            } else {
                ColKind::Utf8
            }
        }
        (ColKind::Float64, Some(ValueRef::Text(t))) if parse_f64(t).is_none() => ColKind::Utf8,
        (ColKind::Int64 | ColKind::Float64, Some(ValueRef::Blob(_))) => ColKind::Utf8,
        _ => kind,
    }
}

fn is_integral(f: f64) -> bool {
    f.is_finite() && f.fract() == 0.0 && f.abs() < 9.2e18
}

fn parse_i64(t: &[u8]) -> Option<i64> {
    std::str::from_utf8(t).ok()?.trim().parse().ok()
}

fn parse_f64(t: &[u8]) -> Option<f64> {
    std::str::from_utf8(t).ok()?.trim().parse().ok()
}

/// Converts SQLite values into cells of the fixed schema, warning once per column when a
/// value cannot be represented and is written as NULL.
struct RowCodec {
    kinds: Vec<ColKind>,
    names: Vec<String>,
    warned: Vec<bool>,
}

impl RowCodec {
    fn push_row(&mut self, batcher: &mut ArrowBatcher, row: &rusqlite::Row<'_>) -> Result<()> {
        for col in 0..self.kinds.len() {
            let value = row.get_ref(col)?;
            self.push_value(batcher, col, value)?;
        }
        Ok(())
    }

    fn push_value(&mut self, batcher: &mut ArrowBatcher, col: usize, value: ValueRef<'_>) -> Result<()> {
        let kind = self.kinds[col];
        let cell = match (kind, value) {
            (_, ValueRef::Null) => Cell::Null,
            (ColKind::Int64, ValueRef::Integer(i)) => Cell::I64(i),
            (ColKind::Int64, ValueRef::Real(f)) if is_integral(f) => Cell::I64(f as i64),
            (ColKind::Int64, ValueRef::Real(_)) => self.lost(col, "a fractional REAL"),
            (ColKind::Int64, ValueRef::Text(t)) => match parse_i64(t) {
                Some(i) => Cell::I64(i),
                None => self.lost(col, "non-integer TEXT"),
            },
            (ColKind::Int64, ValueRef::Blob(_)) => self.lost(col, "a BLOB"),
            (ColKind::Float64, ValueRef::Integer(i)) => Cell::F64(i as f64),
            (ColKind::Float64, ValueRef::Real(f)) => Cell::F64(f),
            (ColKind::Float64, ValueRef::Text(t)) => match parse_f64(t) {
                Some(f) => Cell::F64(f),
                None => self.lost(col, "non-numeric TEXT"),
            },
            (ColKind::Float64, ValueRef::Blob(_)) => self.lost(col, "a BLOB"),
            (ColKind::Utf8, ValueRef::Integer(i)) => Cell::I64(i),
            (ColKind::Utf8, ValueRef::Real(f)) => Cell::F64(f),
            (ColKind::Utf8, ValueRef::Text(t)) => match std::str::from_utf8(t) {
                Ok(s) => Cell::Str(s),
                // The batcher converts bytes into a Utf8 column lossily.
                Err(_) => Cell::Bytes(t),
            },
            (ColKind::Utf8, ValueRef::Blob(b)) => Cell::Bytes(b),
            (ColKind::Binary, ValueRef::Blob(b)) | (ColKind::Binary, ValueRef::Text(b)) => Cell::Bytes(b),
            (ColKind::Binary, ValueRef::Integer(i)) => return batcher.push(Cell::Bytes(i.to_string().as_bytes())),
            (ColKind::Binary, ValueRef::Real(f)) => return batcher.push(Cell::Bytes(f.to_string().as_bytes())),
            (other, _) => return Err(CoreError::Internal(format!("sqlite driver never produces {other:?} columns"))),
        };
        batcher.push(cell)
    }

    /// Record (once per column) that a value was dropped, and return the NULL cell.
    fn lost(&mut self, col: usize, what: &str) -> Cell<'static> {
        if !self.warned[col] {
            self.warned[col] = true;
            tracing::warn!(
                column = %self.names[col],
                kind = ?self.kinds[col],
                "sqlite: {what} found in a {:?} column; such values are written as NULL",
                self.kinds[col]
            );
        }
        Cell::Null
    }
}

// ---------------------------------------------------------------------------------------------
// Streaming (async side)
// ---------------------------------------------------------------------------------------------

/// Wrap the receiver as a `Stream`. Yields `Err(Cancelled)` as soon as the token fires (even
/// when batches are still buffered) and ends after the first error. Holds `guard` so that
/// dropping the stream interrupts the reader thread.
fn batch_stream(rx: mpsc::Receiver<Result<Batch>>, cancel: CancellationToken, guard: DropGuard) -> BoxStream<'static, Result<Batch>> {
    struct State {
        rx: mpsc::Receiver<Result<Batch>>,
        cancel: CancellationToken,
        finished: bool,
        _guard: DropGuard,
    }
    let state = State { rx, cancel, finished: false, _guard: guard };
    Box::pin(futures::stream::unfold(state, |mut st| async move {
        if st.finished {
            return None;
        }
        let item = tokio::select! {
            biased;
            _ = st.cancel.cancelled() => {
                st.rx.close();
                Some(Err(CoreError::Cancelled))
            }
            item = st.rx.recv() => item,
        };
        match item {
            // Channel closed: the reader thread finished (or died); the state is dropped.
            None => None,
            Some(Ok(batch)) => Some((Ok(batch), st)),
            Some(Err(e)) => {
                st.finished = true;
                Some((Err(e), st))
            }
        }
    }))
}

// ---------------------------------------------------------------------------------------------
// EXPLAIN QUERY PLAN
// ---------------------------------------------------------------------------------------------

/// One `EXPLAIN QUERY PLAN` row (`id`, `parent`, `detail`; `notused` is skipped).
#[derive(Debug, Clone)]
struct EqpRow {
    id: i64,
    parent: i64,
    detail: String,
}

/// Node types SQLite prints, longest first so that `SETUP SCAN` wins over `SCAN`.
const PLAN_NODE_TYPES: &[&str] = &[
    "CORRELATED SCALAR SUBQUERY",
    "CORRELATED LIST SUBQUERY",
    "USE TEMP B-TREE",
    "SCALAR SUBQUERY",
    "LIST SUBQUERY",
    "COMPOUND QUERY",
    "MULTI-INDEX OR",
    "BLOOM FILTER ON",
    "RECURSIVE STEP",
    "SETUP SCAN",
    "CO-ROUTINE",
    "MATERIALIZE",
    "UNION ALL",
    "INTERSECT",
    "SEARCH",
    "EXCEPT",
    "SCAN",
    "UNION",
    "MERGE",
    "INDEX",
    "RIGHT",
    "LEFT",
];

/// Parsed `detail`: node type, scanned relation and alias, plus the index used if any.
#[derive(Debug, Default, PartialEq, Eq)]
struct DetailParts {
    node_type: String,
    relation: Option<String>,
    alias: Option<String>,
    index: Option<String>,
    covering: bool,
}

fn parse_detail(detail: &str) -> DetailParts {
    let detail = detail.trim();
    let node_type = PLAN_NODE_TYPES
        .iter()
        .find(|p| detail.starts_with(*p) && detail[p.len()..].chars().next().is_none_or(|c| !c.is_ascii_alphanumeric()))
        .map(|p| p.to_string())
        .unwrap_or_else(|| {
            // Unknown line: the leading run of upper-case words (at most three).
            let words: Vec<&str> = detail.split_whitespace().take_while(|w| w.chars().all(|c| c.is_ascii_uppercase() || c == '-')).take(3).collect();
            if words.is_empty() {
                detail.split_whitespace().next().unwrap_or("").to_string()
            } else {
                words.join(" ")
            }
        });
    let mut parts = DetailParts { node_type: node_type.clone(), ..Default::default() };
    if node_type == "SCAN" || node_type == "SEARCH" {
        let rest = detail[node_type.len()..].trim_start();
        let rest = rest.strip_prefix("TABLE ").unwrap_or(rest);
        let tokens: Vec<&str> = rest.split_whitespace().collect();
        if let Some(first) = tokens.first() {
            if !matches!(*first, "SUBQUERY" | "CONSTANT" | "VIEW") {
                parts.relation = Some(first.trim_matches(|c| c == '"' || c == '`' || c == '[' || c == ']').to_string());
                if tokens.get(1) == Some(&"AS") {
                    parts.alias = tokens.get(2).map(|a| a.to_string());
                }
            }
        }
        if let Some(pos) = tokens.iter().position(|t| *t == "INDEX") {
            parts.index = tokens.get(pos + 1).map(|i| i.to_string());
            parts.covering = pos >= 1 && tokens[pos - 1] == "COVERING";
        }
    }
    parts
}

/// Turn the EQP rows into a `PlanNode` tree (synthetic `QUERY PLAN` root, like the shell),
/// the list of scanned relations and the indented text.
fn build_plan(rows: &[EqpRow]) -> (PlanNode, Vec<TableRef>, String) {
    let mut children: HashMap<i64, Vec<usize>> = HashMap::new();
    let ids: HashSet<i64> = rows.iter().map(|r| r.id).collect();
    for (i, r) in rows.iter().enumerate() {
        // Orphans (parent id unknown) hang off the root instead of disappearing.
        let parent = if r.parent != 0 && ids.contains(&r.parent) && r.parent != r.id { r.parent } else { 0 };
        children.entry(parent).or_default().push(i);
    }
    let mut tables: Vec<TableRef> = Vec::new();
    let mut raw = String::from("QUERY PLAN");
    let mut visited = HashSet::new();
    let kids = build_children(rows, &children, 0, "", &mut raw, &mut tables, &mut visited, 0);
    let root = PlanNode { node_type: "QUERY PLAN".into(), children: kids, ..Default::default() };
    (root, tables, raw)
}

#[allow(clippy::too_many_arguments)]
fn build_children(
    rows: &[EqpRow],
    children: &HashMap<i64, Vec<usize>>,
    parent: i64,
    prefix: &str,
    raw: &mut String,
    tables: &mut Vec<TableRef>,
    visited: &mut HashSet<i64>,
    depth: usize,
) -> Vec<PlanNode> {
    let Some(kids) = children.get(&parent) else {
        return Vec::new();
    };
    if depth > 64 {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(kids.len());
    for (n, &idx) in kids.iter().enumerate() {
        let row = &rows[idx];
        if !visited.insert(row.id) {
            continue;
        }
        let last = n + 1 == kids.len();
        raw.push('\n');
        raw.push_str(prefix);
        raw.push_str(if last { "`--" } else { "|--" });
        raw.push_str(&row.detail);
        let parts = parse_detail(&row.detail);
        let relation = parts.relation.as_deref().map(|name| TableRef::new(None, name));
        if let Some(rel) = &relation {
            if !tables.contains(rel) {
                tables.push(rel.clone());
            }
        }
        let mut extra = BTreeMap::new();
        extra.insert("detail".to_string(), row.detail.clone());
        if let Some(index) = &parts.index {
            extra.insert("index".to_string(), index.clone());
            if parts.covering {
                extra.insert("covering".to_string(), "true".to_string());
            }
        }
        let next_prefix = format!("{prefix}{}", if last { "   " } else { "|  " });
        let node_children = build_children(rows, children, row.id, &next_prefix, raw, tables, visited, depth + 1);
        out.push(PlanNode { node_type: parts.node_type, relation, alias: parts.alias, children: node_children, extra, ..Default::default() });
    }
    out
}

// ---------------------------------------------------------------------------------------------
// Catalog
// ---------------------------------------------------------------------------------------------

/// `name NOT LIKE 'sqlite_%'` with the underscore escaped (internal tables only).
const NOT_INTERNAL: &str = "name NOT LIKE 'sqlite\\_%' ESCAPE '\\'";

/// Per-btree sizes from the `dbstat` virtual table (`name -> bytes`), `None` when the
/// extension is not available in this build or the query fails.
fn dbstat_sizes(c: &rusqlite::Connection, schema: &str) -> Option<HashMap<String, u64>> {
    let mut stmt = match c.prepare("SELECT name, sum(pgsize) FROM dbstat WHERE schema = ?1 GROUP BY name") {
        Ok(s) => s,
        Err(e) => {
            tracing::debug!("sqlite: dbstat unavailable: {e}");
            return None;
        }
    };
    let mut rows = stmt.query([schema]).ok()?;
    let mut map = HashMap::new();
    loop {
        match rows.next() {
            Ok(Some(r)) => {
                if let (Ok(name), Ok(size)) = (r.get::<_, String>(0), r.get::<_, i64>(1)) {
                    map.insert(name, size.max(0) as u64);
                }
            }
            Ok(None) => break,
            Err(e) => {
                tracing::debug!("sqlite: dbstat failed: {e}");
                return None;
            }
        }
    }
    Some(map)
}

/// Size of one btree via `dbstat` (walks only that btree), `None` when unavailable.
fn dbstat_size(c: &rusqlite::Connection, schema: &str, name: &str) -> Option<u64> {
    c.query_row("SELECT sum(pgsize) FROM dbstat WHERE schema = ?1 AND name = ?2", [schema, name], |r| r.get::<_, Option<i64>>(0)).ok().flatten().map(|n| n.max(0) as u64)
}

/// Names of the databases in `PRAGMA database_list`.
fn database_names(c: &rusqlite::Connection) -> Result<Vec<String>> {
    let mut names = Vec::new();
    c.pragma_query(None, "database_list", |r| {
        names.push(r.get::<_, String>(1)?);
        Ok(())
    })?;
    Ok(names)
}

/// `WHERE` predicate of a `CREATE INDEX` statement (text after the last top-level ` WHERE `).
fn partial_predicate(create_sql: &str) -> Option<String> {
    let upper = create_sql.to_ascii_uppercase();
    // The column list is parenthesised; the predicate follows the closing parenthesis.
    let close = upper.rfind(')')?;
    let tail = &upper[close + 1..];
    let pos = tail.find("WHERE")?;
    let start = close + 1 + pos + "WHERE".len();
    let pred = create_sql[start..].trim().trim_end_matches(';').trim();
    if pred.is_empty() {
        None
    } else {
        Some(pred.to_string())
    }
}

#[async_trait]
impl Catalog for SqliteDatabase {
    async fn databases(&self) -> Result<Vec<DatabaseInfo>> {
        self.with_conn(|c| {
            let mut list: Vec<(String, Option<String>)> = Vec::new();
            c.pragma_query(None, "database_list", |r| {
                list.push((r.get(1)?, r.get(2)?));
                Ok(())
            })?;
            let mut out = Vec::with_capacity(list.len());
            for (name, file) in list {
                let size_bytes = file.as_deref().filter(|f| !f.is_empty()).and_then(|f| std::fs::metadata(f).ok()).map(|m| m.len());
                let encoding = c.pragma_query_value(Some(&name), "encoding", |r| r.get::<_, String>(0)).ok();
                out.push(DatabaseInfo { is_current: name == MAIN, name, size_bytes, encoding, collation: None, accessible: true });
            }
            Ok(out)
        })
        .await
    }

    /// SQLite has one schema per attached database, named like the database (`main`).
    async fn schemas(&self, db: &str) -> Result<Vec<SchemaInfo>> {
        let db = if db.trim().is_empty() { MAIN.to_string() } else { db.trim().to_string() };
        self.with_conn(move |c| {
            if !database_names(c)?.iter().any(|n| n.eq_ignore_ascii_case(&db)) {
                return Err(CoreError::NotFound(format!("sqlite database '{db}' is not attached")));
            }
            let count: i64 =
                c.query_row(&format!("SELECT count(*) FROM {}.sqlite_master WHERE type IN ('table','view') AND {NOT_INTERNAL}", quote(&db)), [], |r| r.get(0))?;
            Ok(vec![SchemaInfo { name: db, owner: None, table_count: Some(count.max(0) as u64) }])
        })
        .await
    }

    /// Tables and views with an immediate `count(*)` (cheap for a local file) and sizes from
    /// `dbstat`, all in one blocking call.
    async fn tables(&self, db: &str, schema: &str) -> Result<Vec<TableInfo>> {
        let schema = [schema, db, MAIN].iter().map(|s| s.trim()).find(|s| !s.is_empty()).unwrap_or(MAIN).to_string();
        self.with_conn(move |c| {
            let master = format!("{}.sqlite_master", quote(&schema));
            let mut objects: Vec<(String, String)> = Vec::new();
            {
                let mut stmt = c.prepare(&format!("SELECT name, type FROM {master} WHERE type IN ('table','view') AND {NOT_INTERNAL} ORDER BY name"))?;
                let mut rows = stmt.query([])?;
                while let Some(r) = rows.next()? {
                    objects.push((r.get(0)?, r.get(1)?));
                }
            }
            let mut index_owner: Vec<(String, String)> = Vec::new();
            {
                let mut stmt = c.prepare(&format!("SELECT name, tbl_name FROM {master} WHERE type = 'index'"))?;
                let mut rows = stmt.query([])?;
                while let Some(r) = rows.next()? {
                    index_owner.push((r.get(0)?, r.get(1)?));
                }
            }
            let sizes = dbstat_sizes(c, &schema);
            let mut out = Vec::with_capacity(objects.len());
            for (name, ty) in objects {
                let kind = if ty == "view" { TableKind::View } else { TableKind::Table };
                let estimated_rows = match c.query_row(&format!("SELECT count(*) FROM {}", qualified(&schema, &name)), [], |r| r.get::<_, i64>(0)) {
                    Ok(n) => Some(n.max(0) as u64),
                    Err(e) => {
                        tracing::debug!("sqlite: count(*) on {name} failed: {e}");
                        None
                    }
                };
                let (table_bytes, index_bytes) = match &sizes {
                    Some(map) if kind == TableKind::Table => {
                        let t = map.get(&name).copied().unwrap_or(0);
                        let i = index_owner.iter().filter(|(_, tbl)| *tbl == name).map(|(idx, _)| map.get(idx).copied().unwrap_or(0)).sum::<u64>();
                        (Some(t), Some(i))
                    }
                    _ => (None, None),
                };
                let total_bytes = match (table_bytes, index_bytes) {
                    (Some(t), Some(i)) => Some(t + i),
                    _ => None,
                };
                out.push(TableInfo { schema: schema.clone(), name, kind, estimated_rows, table_bytes, index_bytes, total_bytes, comment: None });
            }
            Ok(out)
        })
        .await
    }

    async fn columns(&self, table: &TableRef) -> Result<Vec<ColumnInfo>> {
        let schema = schema_of(table);
        let name = table.name.clone();
        self.with_conn(move |c| {
            let mut cols: Vec<ColumnInfo> = Vec::new();
            c.pragma(Some(&schema), "table_xinfo", &name, |r| {
                let cid: i64 = r.get(0)?;
                let notnull: i64 = r.get(3)?;
                let pk: i64 = r.get(5)?;
                cols.push(ColumnInfo {
                    position: (cid.max(0) as u32) + 1,
                    name: r.get(1)?,
                    data_type: r.get::<_, Option<String>>(2)?.unwrap_or_default(),
                    nullable: notnull == 0,
                    default: r.get(4)?,
                    primary_key: pk > 0,
                    foreign_key: None,
                    unique: false,
                    comment: None,
                });
                Ok(())
            })?;
            if cols.is_empty() {
                return Err(CoreError::NotFound(format!("table {} does not exist", qualified(&schema, &name))));
            }
            // Foreign keys: from-column -> "table(to)".
            let mut fks: HashMap<String, String> = HashMap::new();
            c.pragma(Some(&schema), "foreign_key_list", &name, |r| {
                let target: String = r.get(2)?;
                let from: String = r.get(3)?;
                let to: Option<String> = r.get(4)?;
                fks.insert(
                    from,
                    match to {
                        Some(t) => format!("{target}({t})"),
                        None => target,
                    },
                );
                Ok(())
            })?;
            // Uniqueness: single-column unique indexes (constraints or CREATE UNIQUE INDEX).
            let mut unique_indexes: Vec<String> = Vec::new();
            c.pragma(Some(&schema), "index_list", &name, |r| {
                let idx: String = r.get(1)?;
                let unique: i64 = r.get(2)?;
                let origin: String = r.get(3)?;
                let partial: i64 = r.get::<_, Option<i64>>(4)?.unwrap_or(0);
                if unique == 1 && partial == 0 && matches!(origin.as_str(), "u" | "pk" | "c") {
                    unique_indexes.push(idx);
                }
                Ok(())
            })?;
            let mut unique_cols: HashSet<String> = HashSet::new();
            for idx in unique_indexes {
                let mut key_cols: Vec<Option<String>> = Vec::new();
                c.pragma(Some(&schema), "index_info", &idx, |r| {
                    key_cols.push(r.get(2)?);
                    Ok(())
                })?;
                if let [Some(col)] = key_cols.as_slice() {
                    unique_cols.insert(col.clone());
                }
            }
            // A single primary key column (rowid alias included) is unique by definition.
            if cols.iter().filter(|c| c.primary_key).count() == 1 {
                if let Some(pk) = cols.iter().find(|c| c.primary_key) {
                    unique_cols.insert(pk.name.clone());
                }
            }
            for col in &mut cols {
                col.foreign_key = fks.get(&col.name).cloned();
                col.unique = unique_cols.contains(&col.name);
            }
            Ok(cols)
        })
        .await
    }

    async fn indexes(&self, table: &TableRef) -> Result<Vec<IndexInfo>> {
        let schema = schema_of(table);
        let name = table.name.clone();
        self.with_conn(move |c| {
            let mut list: Vec<(String, bool, String, bool)> = Vec::new();
            c.pragma(Some(&schema), "index_list", &name, |r| {
                let unique: i64 = r.get(2)?;
                let partial: i64 = r.get::<_, Option<i64>>(4)?.unwrap_or(0);
                list.push((r.get(1)?, unique == 1, r.get(3)?, partial == 1));
                Ok(())
            })?;
            let master = format!("{}.sqlite_master", quote(&schema));
            let mut sql_stmt = c.prepare(&format!("SELECT sql FROM {master} WHERE type = 'index' AND name = ?1"))?;
            let mut out = Vec::with_capacity(list.len());
            for (idx, unique, origin, partial) in list {
                let mut columns: Vec<(i64, String)> = Vec::new();
                c.pragma(Some(&schema), "index_xinfo", &idx, |r| {
                    let seqno: i64 = r.get(0)?;
                    let cid: i64 = r.get(1)?;
                    let key: i64 = r.get::<_, Option<i64>>(5)?.unwrap_or(1);
                    if key == 1 {
                        let col: Option<String> = r.get(2)?;
                        let col = col.unwrap_or_else(|| if cid == -1 { "rowid".to_string() } else { "<expr>".to_string() });
                        columns.push((seqno, col));
                    }
                    Ok(())
                })?;
                columns.sort_by_key(|(seq, _)| *seq);
                let definition: Option<String> = sql_stmt.query_row([&idx], |r| r.get::<_, Option<String>>(0)).ok().flatten();
                let predicate = if partial { definition.as_deref().and_then(partial_predicate) } else { None };
                out.push(IndexInfo {
                    name: idx.clone(),
                    columns: columns.into_iter().map(|(_, c)| c).collect(),
                    unique,
                    primary: origin == "pk",
                    kind: Some("btree".into()),
                    predicate,
                    include: Vec::new(),
                    size_bytes: dbstat_size(c, &schema, &idx),
                    scans: None,
                    definition,
                });
            }
            Ok(out)
        })
        .await
    }

    /// `CREATE TABLE ...;` followed by one `CREATE INDEX ...;` per line (auto-indexes have no SQL).
    async fn ddl(&self, table: &TableRef) -> Result<String> {
        let schema = schema_of(table);
        let name = table.name.clone();
        self.with_conn(move |c| {
            let master = format!("{}.sqlite_master", quote(&schema));
            let table_sql: Option<String> =
                c.query_row(&format!("SELECT sql FROM {master} WHERE type IN ('table','view') AND name = ?1"), [&name], |r| r.get(0)).map_err(|e| match e {
                    rusqlite::Error::QueryReturnedNoRows => CoreError::NotFound(format!("table {} does not exist", qualified(&schema, &name))),
                    other => CoreError::from(other),
                })?;
            let mut lines = Vec::new();
            if let Some(sql) = table_sql {
                lines.push(format!("{};", sql.trim().trim_end_matches(';')));
            }
            let mut stmt = c.prepare(&format!("SELECT sql FROM {master} WHERE type = 'index' AND tbl_name = ?1 AND sql IS NOT NULL ORDER BY name"))?;
            let mut rows = stmt.query([&name])?;
            while let Some(r) = rows.next()? {
                let sql: String = r.get(0)?;
                lines.push(format!("{};", sql.trim().trim_end_matches(';')));
            }
            Ok(lines.join("\n"))
        })
        .await
    }

    async fn exact_count(&self, table: &TableRef, cancel: CancellationToken) -> Result<u64> {
        let sql = format!("SELECT count(*) FROM {}", qualified(&schema_of(table), &table.name));
        let abort = cancel.child_token();
        let watcher = self.interrupt_on_cancel(abort.clone());
        self.with_conn(move |c| {
            let _watcher = watcher;
            if abort.is_cancelled() {
                return Err(CoreError::Cancelled);
            }
            let n: i64 = c.query_row(&sql, [], |r| r.get(0))?;
            Ok(n.max(0) as u64)
        })
        .await
    }
}

// ---------------------------------------------------------------------------------------------
// Monitor
// ---------------------------------------------------------------------------------------------

/// A local file has no server sessions: the snapshot is never found and nobody else is
/// reading the tables through this process.
#[async_trait]
impl Monitor for SqliteDatabase {
    async fn snapshot(&self, _job: &JobHandle) -> Result<ServerSnapshot> {
        Ok(ServerSnapshot { found: false, ..Default::default() })
    }

    async fn active_sessions(&self, _touching: &[TableRef]) -> Result<Vec<SessionInfo>> {
        Ok(Vec::new())
    }
}

impl Database for SqliteDatabase {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decltype_mapping_follows_affinity_rules() {
        assert_eq!(kind_from_decltype("INTEGER"), Some(ColKind::Int64));
        assert_eq!(kind_from_decltype("bigint"), Some(ColKind::Int64));
        assert_eq!(kind_from_decltype("FLOATING POINT"), Some(ColKind::Int64)); // SQLite quirk
        assert_eq!(kind_from_decltype("REAL"), Some(ColKind::Float64));
        assert_eq!(kind_from_decltype("DOUBLE PRECISION"), Some(ColKind::Float64));
        assert_eq!(kind_from_decltype("BLOB"), Some(ColKind::Binary));
        assert_eq!(kind_from_decltype("VARCHAR(20)"), Some(ColKind::Utf8));
        assert_eq!(kind_from_decltype("NUMERIC(12,2)"), Some(ColKind::Utf8));
        assert_eq!(kind_from_decltype("DECIMAL"), Some(ColKind::Utf8));
        assert_eq!(kind_from_decltype("BOOLEAN"), Some(ColKind::Utf8));
        assert_eq!(kind_from_decltype(""), None);
        assert_eq!(kind_from_decltype("  "), None);
    }

    #[test]
    fn first_row_decides_untyped_columns() {
        assert_eq!(column_kind(None, Some(ValueRef::Integer(1))), ColKind::Int64);
        assert_eq!(column_kind(None, Some(ValueRef::Real(1.5))), ColKind::Float64);
        assert_eq!(column_kind(None, Some(ValueRef::Text(b"x"))), ColKind::Utf8);
        assert_eq!(column_kind(None, Some(ValueRef::Blob(b"x"))), ColKind::Binary);
        assert_eq!(column_kind(None, Some(ValueRef::Null)), ColKind::Utf8);
        assert_eq!(column_kind(None, None), ColKind::Utf8);
        // Declared types win, but are widened when the first value cannot be represented.
        assert_eq!(column_kind(Some("INTEGER"), Some(ValueRef::Text(b"abc"))), ColKind::Utf8);
        assert_eq!(column_kind(Some("INTEGER"), Some(ValueRef::Text(b"12"))), ColKind::Int64);
        assert_eq!(column_kind(Some("INTEGER"), Some(ValueRef::Real(2.5))), ColKind::Float64);
        assert_eq!(column_kind(Some("INTEGER"), Some(ValueRef::Real(2.0))), ColKind::Int64);
        assert_eq!(column_kind(Some("INTEGER"), Some(ValueRef::Null)), ColKind::Int64);
        assert_eq!(column_kind(Some("REAL"), Some(ValueRef::Text(b"n/a"))), ColKind::Utf8);
    }

    #[test]
    fn parses_plan_details() {
        let p = parse_detail("SCAN narrow");
        assert_eq!(p.node_type, "SCAN");
        assert_eq!(p.relation.as_deref(), Some("narrow"));
        let p = parse_detail("SEARCH nasty USING INDEX nasty_note (note=?)");
        assert_eq!(p.node_type, "SEARCH");
        assert_eq!(p.relation.as_deref(), Some("nasty"));
        assert_eq!(p.index.as_deref(), Some("nasty_note"));
        assert!(!p.covering);
        let p = parse_detail("SCAN TABLE t AS u USING COVERING INDEX i");
        assert_eq!(p.relation.as_deref(), Some("t"));
        assert_eq!(p.alias.as_deref(), Some("u"));
        assert!(p.covering);
        let p = parse_detail("USE TEMP B-TREE FOR ORDER BY");
        assert_eq!(p.node_type, "USE TEMP B-TREE");
        assert_eq!(p.relation, None);
        let p = parse_detail("SCAN SUBQUERY 1");
        assert_eq!(p.relation, None);
        assert_eq!(parse_detail("SETUP SCAN c").node_type, "SETUP SCAN");
        assert_eq!(parse_detail("CO-ROUTINE c").node_type, "CO-ROUTINE");
        assert_eq!(parse_detail("SCANNING something").node_type, "SCANNING");
    }

    #[test]
    fn builds_plan_tree_and_text() {
        let rows = vec![
            EqpRow { id: 2, parent: 0, detail: "CO-ROUTINE c".into() },
            EqpRow { id: 5, parent: 2, detail: "SETUP SCAN c".into() },
            EqpRow { id: 9, parent: 2, detail: "RECURSIVE STEP".into() },
            EqpRow { id: 12, parent: 9, detail: "SCAN c".into() },
            EqpRow { id: 20, parent: 0, detail: "SCAN c".into() },
        ];
        let (root, tables, raw) = build_plan(&rows);
        assert_eq!(root.node_type, "QUERY PLAN");
        assert_eq!(root.children.len(), 2);
        assert_eq!(root.children[0].children.len(), 2);
        assert_eq!(root.children[0].children[1].children[0].node_type, "SCAN");
        assert_eq!(tables, vec![TableRef::new(None, "c")]);
        assert_eq!(raw, "QUERY PLAN\n|--CO-ROUTINE c\n|  |--SETUP SCAN c\n|  `--RECURSIVE STEP\n|     `--SCAN c\n`--SCAN c");
    }

    #[test]
    fn extracts_partial_predicate() {
        assert_eq!(partial_predicate("CREATE INDEX nasty_note ON nasty(note) WHERE note IS NOT NULL").as_deref(), Some("note IS NOT NULL"));
        assert_eq!(partial_predicate("CREATE INDEX i ON t(a, b)"), None);
        assert_eq!(partial_predicate("CREATE INDEX i ON t(\"where\") where a > 1;").as_deref(), Some("a > 1"));
    }

    #[test]
    fn memory_paths() {
        assert!(is_memory_path(":memory:"));
        assert!(is_memory_path("file::memory:?cache=shared"));
        assert!(is_memory_path("file:x?mode=memory"));
        assert!(!is_memory_path("C:\\data\\x.db"));
    }

    #[test]
    fn schema_resolution() {
        assert_eq!(schema_of(&TableRef::new(None, "t")), "main");
        assert_eq!(schema_of(&TableRef::new(Some("aux"), "t")), "aux");
        let t = TableRef { database: Some("other".into()), schema: None, name: "t".into() };
        assert_eq!(schema_of(&t), "other");
        assert_eq!(qualified("main", "we\"ird"), "\"main\".\"we\"\"ird\"");
    }
}
