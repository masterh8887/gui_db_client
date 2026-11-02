//! SQLite target writer (4.4). Owner: copy-data.
//!
//! * Its own `rusqlite::Connection` (not `source::open`, which refuses a missing file), used on
//!   blocking threads. Read sessions open read-only without `OPEN_CREATE`; a missing file is an
//!   empty database and no file is created (plans and dry runs leave no trace). Only write
//!   sessions (`…-t<n>`) open with `OPEN_CREATE`.
//! * Loading: `BEGIN` (unless the executor already began one for a truncate), a prepared
//!   `INSERT INTO "t" ("c1", …) VALUES (?, …)` bound column-wise from the Arrow arrays, `COMMIT`
//!   every `batch_rows`; a failure rolls back only the open batch.
//! * Locks: a short busy timeout plus BulkDB's own retry loop, which watches the cancel flag and
//!   gives up after `target_lock_timeout_s` with `TARGET_LOCK_TIMEOUT`, so a writer blocked by
//!   another connection's write lock is cancellable.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use rusqlite::types::{Value, ValueRef};
use rusqlite::{ErrorCode, OpenFlags};
use tokio_util::sync::CancellationToken;

use crate::copy::data::cells::{date_text, decimal_text, time_text, Cell, Col};
use crate::copy::data::values::format_micros;
use crate::copy::guard;
use crate::copy::types::*;
use crate::copy::{copy_fail, CODE_TARGET_LOCK_TIMEOUT};
use crate::error::{CoreError, Result};
use crate::types::Connection;

const BUSY_SLICE: Duration = Duration::from_millis(50);

/// `"name"` with `"` doubled.
pub fn qi(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// The target file of a SQLite connection (`fields.path`, else `database`).
pub fn target_path(conn: &Connection) -> Option<String> {
    conn.fields
        .get("path")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| conn.database.as_deref().map(str::trim).filter(|s| !s.is_empty()).map(str::to_string))
}

/// The file path a SQLite connection string names: a `file:` URI (which the SQLite source
/// driver accepts) loses its scheme, authority, `?query` and `#fragment` and is
/// percent-decoded (`file:///C:/x.db` -> `C:/x.db`); a plain path is returned as is.
pub fn file_path_of(path: &str) -> String {
    let Some(rest) = path.strip_prefix("file:") else { return path.to_string() };
    let rest = rest.split(['?', '#']).next().unwrap_or("");
    // `file://host/path` / `file:///path`: drop the authority (SQLite accepts only empty or
    // `localhost`).
    let rest = match rest.strip_prefix("//") {
        Some(r) => match r.find('/') {
            Some(i) => &r[i..],
            None => "",
        },
        None => rest,
    };
    // `/C:/dir/x.db` (a Windows drive in a URI) -> `C:/dir/x.db`.
    let b = rest.as_bytes();
    let rest = if b.len() >= 3 && b[0] == b'/' && b[1].is_ascii_alphabetic() && b[2] == b':' { &rest[1..] } else { rest };
    let rb = rest.as_bytes();
    let mut out = Vec::with_capacity(rb.len());
    let mut i = 0;
    while i < rb.len() {
        if rb[i] == b'%' && i + 2 < rb.len() {
            if let Some(v) = std::str::from_utf8(&rb[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(rb[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The canonical path of a SQLite target or source file: symlinks resolved and, on
/// case-insensitive volumes, the name's case as stored (`fs::canonicalize` asks the file
/// system for the final path). A file that does not exist yet keeps its name as written
/// under its canonical parent directory.
pub fn canonical_path(path: &str) -> PathBuf {
    let file = file_path_of(path);
    let p = Path::new(&file);
    if let Ok(c) = std::fs::canonicalize(p) {
        return c;
    }
    let parent = p.parent().filter(|d| !d.as_os_str().is_empty()).map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("."));
    let parent = std::fs::canonicalize(&parent).unwrap_or(parent);
    let name = p.file_name().map(|n| n.to_os_string()).unwrap_or_default();
    parent.join(name)
}

/// Identity key of a SQLite file (5.3): its [`canonical_path`]; the file may not exist yet.
pub fn identity_key(path: &str) -> String {
    canonical_path(path).to_string_lossy().into_owned()
}

struct Inner {
    conn: Option<rusqlite::Connection>,
}

/// SQLite copy target session.
pub struct SqliteTarget {
    facts: TargetFacts,
    path: Option<String>,
    write: bool,
    inner: Arc<Mutex<Inner>>,
    interrupt: Option<Arc<rusqlite::InterruptHandle>>,
    cancelled: Arc<AtomicBool>,
    in_tx: Arc<AtomicBool>,
    cfg: Mutex<Option<WriterConfig>>,
    /// Rows of the current load committed so far (whole batches); reset when a load succeeds,
    /// so after a failure it is what stays in the target (`take_failed_outcome`).
    committed: Arc<AtomicU64>,
}

impl SqliteTarget {
    /// A session without a file: an empty, read-only database (tests).
    pub fn new(facts: TargetFacts) -> SqliteTarget {
        SqliteTarget {
            facts,
            path: None,
            write: false,
            inner: Arc::new(Mutex::new(Inner { conn: None })),
            interrupt: None,
            cancelled: Arc::new(AtomicBool::new(false)),
            in_tx: Arc::new(AtomicBool::new(false)),
            cfg: Mutex::new(None),
            committed: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Open the target file of `conn` (read-only unless `write`).
    pub async fn open(conn: &Connection, write: bool) -> Result<SqliteTarget> {
        let path = target_path(conn).ok_or_else(|| CoreError::Invalid("sqlite: a database file path is required (field 'path')".into()))?;
        let p = path.clone();
        let (db, version) = tokio::task::spawn_blocking(move || -> Result<(Option<rusqlite::Connection>, String)> {
            let exists = Path::new(&p).is_file();
            let db = if write {
                let c = rusqlite::Connection::open_with_flags(&p, OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE | OpenFlags::SQLITE_OPEN_NO_MUTEX)
                    .map_err(|e| CoreError::Connect(format!("cannot open sqlite target {p}: {e}")))?;
                Some(c)
            } else if exists {
                let c = rusqlite::Connection::open_with_flags(&p, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)
                    .map_err(|e| CoreError::Connect(format!("cannot open sqlite target {p}: {e}")))?;
                Some(c)
            } else {
                None
            };
            if let Some(c) = &db {
                c.busy_timeout(BUSY_SLICE).map_err(|e| CoreError::Connect(format!("busy_timeout: {e}")))?;
                if !write {
                    c.pragma_update(None, "query_only", 1).map_err(|e| CoreError::Connect(format!("query_only: {e}")))?;
                }
                c.query_row("SELECT count(*) FROM sqlite_master", [], |r| r.get::<_, i64>(0))
                    .map_err(|e| CoreError::Connect(format!("{p} is not a readable sqlite database: {e}")))?;
            }
            let version = rusqlite::version().to_string();
            Ok((db, version))
        })
        .await
        .map_err(|e| CoreError::Internal(format!("sqlite task: {e}")))??;
        let interrupt = db.as_ref().map(|c| Arc::new(c.get_interrupt_handle()));
        let mut facts = super::base_facts(CopyFamily::Sqlite, "sqlite", &version, "main", conn);
        facts.endpoint = path.clone();
        facts.vars.insert("sqlite_version".into(), version);
        let dir = Path::new(&path).parent().filter(|d| !d.as_os_str().is_empty()).map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("."));
        if let Ok(free) = fs4::available_space(&dir) {
            facts.vars.insert("free_bytes".into(), free.to_string());
        }
        facts.vars.insert("exists".into(), Path::new(&path).is_file().to_string());
        let facts = super::with_identity(facts);
        Ok(SqliteTarget {
            facts,
            path: Some(path),
            write,
            inner: Arc::new(Mutex::new(Inner { conn: db })),
            interrupt,
            cancelled: Arc::new(AtomicBool::new(false)),
            in_tx: Arc::new(AtomicBool::new(false)),
            cfg: Mutex::new(None),
            committed: Arc::new(AtomicU64::new(0)),
        })
    }

    pub fn path(&self) -> Option<&str> {
        self.path.as_deref()
    }

    fn lock_timeout(&self) -> Duration {
        Duration::from_secs(self.cfg.lock().unwrap().as_ref().map(|c| c.lock_timeout_s).unwrap_or(5).clamp(1, 600) as u64)
    }

    fn batch_rows(&self) -> u64 {
        self.cfg.lock().unwrap().as_ref().map(|c| c.batch_rows).unwrap_or(50_000).clamp(1, 65_536) as u64
    }

    async fn blocking<T: Send + 'static>(&self, f: impl FnOnce(&mut Inner) -> Result<T> + Send + 'static) -> Result<T> {
        let inner = self.inner.clone();
        tokio::task::spawn_blocking(move || {
            let mut g = inner.lock().unwrap();
            f(&mut g)
        })
        .await
        .map_err(|e| CoreError::Internal(format!("sqlite task: {e}")))?
    }
}

fn is_busy(e: &rusqlite::Error) -> bool {
    matches!(e, rusqlite::Error::SqliteFailure(f, _) if matches!(f.code, ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked))
}

/// Run `f` until it stops failing with SQLITE_BUSY, the cancel flag is set, or `timeout`.
fn retry_busy<T>(stop: &dyn Fn() -> bool, timeout: Duration, mut f: impl FnMut() -> rusqlite::Result<T>) -> Result<T> {
    let deadline = Instant::now() + timeout;
    loop {
        if stop() {
            return Err(CoreError::Cancelled);
        }
        match f() {
            Ok(v) => return Ok(v),
            Err(e) if is_busy(&e) => {
                if Instant::now() >= deadline {
                    return Err(copy_fail(CODE_TARGET_LOCK_TIMEOUT, "the target table is in use; try again later (database is locked)"));
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(e) => {
                if stop() {
                    return Err(CoreError::Cancelled);
                }
                return Err(e.into());
            }
        }
    }
}

fn value_text(v: ValueRef<'_>) -> Option<String> {
    match v {
        ValueRef::Null => None,
        ValueRef::Integer(i) => Some(i.to_string()),
        ValueRef::Real(f) => Some(f.to_string()),
        ValueRef::Text(t) => Some(String::from_utf8_lossy(t).into_owned()),
        ValueRef::Blob(b) => Some(crate::copy::hash::hex(b)),
    }
}

fn query_meta_blocking(conn: &rusqlite::Connection, sql: &str) -> rusqlite::Result<MetaTable> {
    let mut stmt = conn.prepare(sql)?;
    let columns: Vec<String> = stmt.column_names().iter().map(|s| s.to_string()).collect();
    let n = columns.len();
    let mut rows = stmt.query([])?;
    let mut out = MetaTable { columns, rows: Vec::new() };
    while let Some(r) = rows.next()? {
        let mut row = Vec::with_capacity(n);
        for i in 0..n {
            row.push(value_text(r.get_ref(i)?));
        }
        out.rows.push(row);
        if out.rows.len() >= 100_000 {
            break;
        }
    }
    Ok(out)
}

fn bind_cell(stmt: &mut rusqlite::Statement<'_>, idx: usize, cell: Cell<'_>) -> rusqlite::Result<()> {
    match cell {
        Cell::Null => stmt.raw_bind_parameter(idx, Value::Null),
        Cell::Int(i) => stmt.raw_bind_parameter(idx, i),
        Cell::UInt(u) => match i64::try_from(u) {
            Ok(i) => stmt.raw_bind_parameter(idx, i),
            Err(_) => stmt.raw_bind_parameter(idx, u.to_string()),
        },
        Cell::Float(f) => stmt.raw_bind_parameter(idx, f),
        Cell::Bool(b) => stmt.raw_bind_parameter(idx, b as i64),
        Cell::Decimal(v, s) => stmt.raw_bind_parameter(idx, decimal_text(v, s)),
        Cell::Str(s) => stmt.raw_bind_parameter(idx, s),
        Cell::Bytes(b) => stmt.raw_bind_parameter(idx, b),
        Cell::Date(d) => stmt.raw_bind_parameter(idx, date_text(d)),
        Cell::Time(t) => stmt.raw_bind_parameter(idx, time_text(t)),
        Cell::Timestamp(t) => stmt.raw_bind_parameter(idx, format_micros(t).unwrap_or_default()),
        Cell::Other(s) => stmt.raw_bind_parameter(idx, s),
    }
}

#[async_trait]
impl TargetSession for SqliteTarget {
    fn facts(&self) -> &TargetFacts {
        &self.facts
    }

    async fn query_meta(&self, sql: &str) -> Result<MetaTable> {
        guard::check_read(CopyFamily::Sqlite, sql)?;
        let sql = sql.to_string();
        let cancelled = self.cancelled.clone();
        let timeout = self.lock_timeout();
        self.blocking(move |inner| match inner.conn.as_ref() {
            Some(c) => retry_busy(&|| cancelled.load(Ordering::Relaxed), timeout, || query_meta_blocking(c, &sql)),
            // Missing file: an empty database.
            None => Ok(MetaTable::default()),
        })
        .await
    }

    async fn execute(&self, sql: &str) -> Result<u64> {
        guard::check_always_rejected(sql)?;
        if !self.write {
            return Err(CoreError::Internal("copy: write on a read-only sqlite target session".into()));
        }
        let words = guard::leading_words(sql, 1);
        let first = words.first().map(String::as_str).unwrap_or("");
        let sql = sql.to_string();
        let cancelled = self.cancelled.clone();
        let in_tx = self.in_tx.clone();
        let timeout = self.lock_timeout();
        let first = first.to_string();
        self.blocking(move |inner| {
            let c = inner.conn.as_ref().ok_or_else(|| CoreError::Internal("sqlite target is not open".into()))?;
            let n = retry_busy(&|| cancelled.load(Ordering::Relaxed), timeout, || c.execute(&sql, []))?;
            match first.as_str() {
                "BEGIN" => in_tx.store(true, Ordering::Relaxed),
                "COMMIT" | "END" | "ROLLBACK" => in_tx.store(false, Ordering::Relaxed),
                _ => {}
            }
            Ok(n as u64)
        })
        .await
    }

    async fn load(&self, spec: &TableDataSpec, input: LoadInput, cancel: CancellationToken) -> Result<LoadOutcome> {
        if !self.write {
            return Err(CoreError::Internal("copy: load on a read-only sqlite target session".into()));
        }
        let LoadInput::Arrow(mut rx) = input else {
            return Err(CoreError::Invalid("sqlite targets load Arrow batches".into()));
        };
        let cols: Vec<String> = spec.columns.iter().map(|c| qi(&c.target)).collect();
        let params: Vec<String> = (1..=cols.len()).map(|i| format!("?{i}")).collect();
        let table = match &spec.target.schema {
            Some(s) if s != "main" => format!("{}.{}", qi(s), qi(&spec.target.name)),
            _ => qi(&spec.target.name),
        };
        let insert = format!("INSERT INTO {table} ({}) VALUES ({})", cols.join(", "), params.join(", "));
        guard::check_statement(CopyFamily::Sqlite, &insert, guard::StmtContext::Load)?;
        let ncols = spec.columns.len();
        let batch_rows = self.batch_rows();
        let timeout = self.lock_timeout();
        let cancelled = self.cancelled.clone();
        let in_tx = self.in_tx.clone();
        let inner = self.inner.clone();
        let committed = self.committed.clone();
        committed.store(0, Ordering::Relaxed);
        let item_cancel = cancel.clone();
        let load_stop = Arc::new(AtomicBool::new(false));
        let task_stop = load_stop.clone();
        let task = tokio::task::spawn_blocking(move || -> Result<u64> {
            let stop = || cancelled.load(Ordering::Relaxed) || task_stop.load(Ordering::Relaxed) || item_cancel.is_cancelled();
            let g = inner.lock().unwrap();
            let c = g.conn.as_ref().ok_or_else(|| CoreError::Internal("sqlite target is not open".into()))?;
            let mut run = || -> Result<u64> {
                if !in_tx.load(Ordering::Relaxed) {
                    retry_busy(&stop, timeout, || c.execute_batch("BEGIN"))?;
                    in_tx.store(true, Ordering::Relaxed);
                }
                let mut stmt = c.prepare(&insert)?;
                let mut rows = 0u64;
                let mut open_rows = 0u64;
                while let Some(b) = rx.blocking_recv() {
                    if b.item.num_columns() != ncols {
                        return Err(CoreError::Internal(format!("copy: batch has {} columns, the plan lists {ncols}", b.item.num_columns())));
                    }
                    let arrays: Vec<Col<'_>> = b.item.columns().iter().map(Col::new).collect::<Result<_>>()?;
                    for r in 0..b.item.num_rows() {
                        for (j, a) in arrays.iter().enumerate() {
                            bind_cell(&mut stmt, j + 1, a.cell(r))?;
                        }
                        retry_busy(&stop, timeout, || stmt.raw_execute())?;
                        rows += 1;
                        open_rows += 1;
                        if open_rows >= batch_rows {
                            retry_busy(&stop, timeout, || c.execute_batch("COMMIT"))?;
                            committed.fetch_add(open_rows, Ordering::Relaxed);
                            retry_busy(&stop, timeout, || c.execute_batch("BEGIN"))?;
                            open_rows = 0;
                        }
                    }
                    drop(arrays);
                    drop(b);
                    if stop() {
                        return Err(CoreError::Cancelled);
                    }
                }
                // The input ended: a cancelled token means the reader stopped early.
                if stop() {
                    return Err(CoreError::Cancelled);
                }
                drop(stmt);
                retry_busy(&stop, timeout, || c.execute_batch("COMMIT"))?;
                in_tx.store(false, Ordering::Relaxed);
                committed.store(0, Ordering::Relaxed);
                Ok(rows)
            };
            let r = run();
            if r.is_err() && in_tx.load(Ordering::Relaxed) {
                let _ = c.execute_batch("ROLLBACK");
                in_tx.store(false, Ordering::Relaxed);
            }
            r
        });
        tokio::pin!(task);
        let joined = tokio::select! {
            biased;
            r = &mut task => r,
            _ = cancel.cancelled() => {
                load_stop.store(true, Ordering::Relaxed);
                if let Some(i) = &self.interrupt {
                    i.interrupt();
                }
                (&mut task).await
            }
        };
        let rows = joined.map_err(|e| CoreError::Internal(format!("sqlite task: {e}")))??;
        Ok(LoadOutcome { rows, method: Some(LoadMethod::SqliteInsert), ..LoadOutcome::default() })
    }

    async fn count_rows(&self, t: &TargetTable) -> Result<u64> {
        let table = match &t.schema {
            Some(s) if s != "main" => format!("{}.{}", qi(s), qi(&t.name)),
            _ => qi(&t.name),
        };
        let m = self.query_meta(&format!("SELECT count(*) AS n FROM {table}")).await?;
        m.rows
            .first()
            .and_then(|r| r.first())
            .and_then(|v| v.as_deref())
            .and_then(|v| v.parse().ok())
            .ok_or_else(|| CoreError::Internal("copy: count returned no row".into()))
    }

    async fn cancel_running(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
        if let Some(i) = &self.interrupt {
            i.interrupt();
        }
    }

    fn configure(&self, cfg: &WriterConfig) {
        *self.cfg.lock().unwrap() = Some(cfg.clone());
    }

    /// The whole batches a failed or cancelled load committed (a failure rolls back only the
    /// open batch).
    fn take_failed_outcome(&self) -> Option<LoadOutcome> {
        let rows = self.committed.swap(0, Ordering::Relaxed);
        (rows > 0).then(|| LoadOutcome { rows, method: Some(LoadMethod::SqliteInsert), ..LoadOutcome::default() })
    }

    /// Whole batches committed by the current load.
    fn committed_rows(&self) -> Option<u64> {
        Some(self.committed.load(Ordering::Relaxed))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc as StdArc;

    use arrow_array::{ArrayRef, BinaryArray, Float64Array, Int64Array, RecordBatch, StringArray};
    use tokio::sync::{mpsc, Semaphore};

    fn conn_for(path: &Path) -> Connection {
        let mut c = Connection { driver: "sqlite".into(), ..Connection::default() };
        c.fields.insert("path".into(), path.to_string_lossy().into_owned());
        c
    }

    fn spec() -> TableDataSpec {
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
        TableDataSpec {
            item_id: "table:\"t\"".into(),
            source_sql: "SELECT 1".into(),
            verify_sql: String::new(),
            raw_csv: false,
            transcode: None,
            target: TargetTable { schema: None, name: "t".into() },
            columns: vec![col("id", ValueKind::Integer), col("name", ValueKind::Text), col("f", ValueKind::Float), col("b", ValueKind::Binary)],
            est_rows: None,
            first_timestamp_auto: false,
            fk_checks: false,
        }
    }

    #[tokio::test]
    async fn missing_file_reads_as_empty_and_is_not_created() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("none.db");
        let t = SqliteTarget::open(&conn_for(&path), false).await.unwrap();
        assert!(t.query_meta("SELECT name FROM sqlite_master").await.unwrap().is_empty());
        assert!(t.execute("CREATE TABLE x (a int)").await.is_err());
        assert!(!path.exists());
        assert!(!t.facts().identity.key.is_empty());
    }

    #[tokio::test]
    async fn loads_batches_with_nulls_korean_and_blobs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.db");
        let t = SqliteTarget::open(&conn_for(&path), true).await.unwrap();
        t.configure(&WriterConfig {
            job_id: "j".into(),
            session_no: 1,
            batch_rows: 2,
            piece_bytes: 1 << 20,
            chunk_bytes: 1 << 20,
            chunk_rows: 10,
            lock_timeout_s: 5,
            fail_on_unmappable: false,
            charset: None,
            table_charsets: Default::default(),
            enum_error_columns: Default::default(),
            enum_error_null: false,
            source_checksum: false,
        });
        t.execute("CREATE TABLE \"t\" (id INTEGER, name TEXT, f REAL, b BLOB)").await.unwrap();
        let budget = StdArc::new(Semaphore::new(100));
        let (tx, rx) = mpsc::channel(1);
        let cols: Vec<ArrayRef> = vec![
            StdArc::new(Int64Array::from(vec![Some(1), Some(2), None])),
            StdArc::new(StringArray::from(vec![Some("가나다"), None, Some("")])),
            StdArc::new(Float64Array::from(vec![Some(1.5), None, Some(-0.25)])),
            StdArc::new(BinaryArray::from(vec![Some(&[0u8, 1, 2][..]), None, Some(&[][..])])),
        ];
        let fields: Vec<arrow_schema::Field> = cols.iter().enumerate().map(|(i, a)| arrow_schema::Field::new(format!("c{i}"), a.data_type().clone(), true)).collect();
        let batch = RecordBatch::try_new(StdArc::new(arrow_schema::Schema::new(fields)), cols).unwrap();
        let send = async move {
            let permit = budget.acquire_many_owned(1).await.unwrap();
            tx.send(Budgeted { item: batch, rows: 3, permit }).await.unwrap();
        };
        let s = spec();
        let (out, ()) = tokio::join!(t.load(&s, LoadInput::Arrow(rx), CancellationToken::new()), send);
        assert_eq!(out.unwrap().rows, 3);
        assert_eq!(t.count_rows(&s.target).await.unwrap(), 3);
        let m = t.query_meta("SELECT id, name, f, hex(b) AS b FROM \"t\" ORDER BY rowid").await.unwrap();
        assert_eq!(m.get(0, "name"), Some("가나다"));
        assert_eq!(m.get(1, "name"), None);
        assert_eq!(m.get(2, "name"), Some(""));
        assert_eq!(m.get(0, "b"), Some("000102"));
        assert_eq!(m.get(2, "id"), None);
    }

    /// A load that fails in its second batch keeps the first (committed) one, and
    /// `take_failed_outcome` reports exactly those rows, once; a successful load reports none.
    #[tokio::test]
    async fn a_failed_load_reports_its_committed_batches() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.db");
        let t = SqliteTarget::open(&conn_for(&path), true).await.unwrap();
        t.configure(&WriterConfig {
            job_id: "j".into(),
            session_no: 1,
            batch_rows: 2,
            piece_bytes: 1 << 20,
            chunk_bytes: 1 << 20,
            chunk_rows: 10,
            lock_timeout_s: 5,
            fail_on_unmappable: false,
            charset: None,
            table_charsets: Default::default(),
            enum_error_columns: Default::default(),
            enum_error_null: false,
            source_checksum: false,
        });
        t.execute("CREATE TABLE \"t\" (id INTEGER NOT NULL, name TEXT, f REAL, b BLOB)").await.unwrap();
        let load = |ids: Vec<Option<i64>>| {
            let budget = StdArc::new(Semaphore::new(100));
            let (tx, rx) = mpsc::channel(1);
            let n = ids.len();
            let cols: Vec<ArrayRef> = vec![
                StdArc::new(Int64Array::from(ids)),
                StdArc::new(StringArray::from(vec![Some("x"); n])),
                StdArc::new(Float64Array::from(vec![Some(1.0); n])),
                StdArc::new(BinaryArray::from(vec![Some(&[1u8][..]); n])),
            ];
            let fields: Vec<arrow_schema::Field> = cols.iter().enumerate().map(|(i, a)| arrow_schema::Field::new(format!("c{i}"), a.data_type().clone(), true)).collect();
            let batch = RecordBatch::try_new(StdArc::new(arrow_schema::Schema::new(fields)), cols).unwrap();
            let send = async move {
                let permit = budget.acquire_many_owned(1).await.unwrap();
                let _ = tx.send(Budgeted { item: batch, rows: n as u64, permit }).await;
            };
            (rx, send)
        };
        let s = spec();
        // Rows 1-2 commit; row 4 (NULL into NOT NULL) fails the open batch of rows 3-4.
        let (rx, send) = load(vec![Some(1), Some(2), Some(3), None, Some(5)]);
        let (out, ()) = tokio::join!(t.load(&s, LoadInput::Arrow(rx), CancellationToken::new()), send);
        assert!(out.is_err());
        assert_eq!(t.count_rows(&s.target).await.unwrap(), 2, "the open batch was rolled back");
        let partial = t.take_failed_outcome().expect("committed rows reported");
        assert_eq!((partial.rows, partial.method), (2, Some(LoadMethod::SqliteInsert)));
        assert!(t.take_failed_outcome().is_none(), "taken once");
        // A successful load leaves nothing to report.
        let (rx, send) = load(vec![Some(6), Some(7), Some(8)]);
        let (out, ()) = tokio::join!(t.load(&s, LoadInput::Arrow(rx), CancellationToken::new()), send);
        assert_eq!(out.unwrap().rows, 3);
        assert!(t.take_failed_outcome().is_none());
    }

    #[tokio::test]
    async fn a_reader_that_stops_early_rolls_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.db");
        let t = SqliteTarget::open(&conn_for(&path), true).await.unwrap();
        t.execute("CREATE TABLE \"t\" (id INTEGER, name TEXT, f REAL, b BLOB)").await.unwrap();
        let (tx, rx) = mpsc::channel::<Budgeted<RecordBatch>>(1);
        let cancel = CancellationToken::new();
        cancel.cancel();
        drop(tx);
        let err = t.load(&spec(), LoadInput::Arrow(rx), cancel).await.unwrap_err();
        assert!(matches!(err, CoreError::Cancelled));
        assert_eq!(t.count_rows(&TargetTable { schema: None, name: "t".into() }).await.unwrap(), 0);
    }

    #[test]
    fn identity_keys_survive_missing_files() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("x.db");
        let k1 = identity_key(&a.to_string_lossy());
        std::fs::write(&a, b"").unwrap();
        assert_eq!(k1, identity_key(&a.to_string_lossy()));
        assert_ne!(k1, identity_key(&dir.path().join("y.db").to_string_lossy()));
    }

    /// Other spellings of one file share its key: `file:` URIs (with a query), `.` segments,
    /// and another letter case on a case-insensitive volume.
    #[test]
    fn identity_keys_resolve_other_spellings() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("Case Name.db");
        std::fs::write(&a, b"").unwrap();
        let plain = identity_key(&a.to_string_lossy());
        let slashed = a.to_string_lossy().replace('\\', "/");
        let uri = if slashed.starts_with('/') { format!("file://{slashed}?mode=ro") } else { format!("file:///{slashed}?mode=ro") };
        assert_eq!(identity_key(&uri), plain, "{uri}");
        assert_eq!(identity_key(&format!("file:{}", slashed.replace(' ', "%20"))), plain);
        assert_eq!(identity_key(&dir.path().join(".").join("Case Name.db").to_string_lossy()), plain);
        let lower = dir.path().join("case name.db");
        if lower.exists() {
            // Case-insensitive volume (Windows, macOS default): the same file.
            assert_eq!(identity_key(&lower.to_string_lossy()), plain);
        } else {
            assert_ne!(identity_key(&lower.to_string_lossy()), plain);
        }
        assert_eq!(file_path_of("file:///C:/data/x.db?mode=ro#f"), "C:/data/x.db");
        assert_eq!(file_path_of("file://localhost/data/x%2Fy.db"), "/data/x/y.db");
        assert_eq!(file_path_of("/plain/x.db"), "/plain/x.db");
    }
}
