//! Generic ODBC driver (odbc-api 13, block cursors). Tier 2: the user installs the vendor driver.
//! Compiled only where a driver manager exists at build time (`has_odbc` cfg from build.rs).
//!
//! * `odbc_api::Connection` is `!Send`, so every connection lives on its own std thread, a tiny
//!   actor that owns the connection and executes [`Cmd`]s received over a channel. The async
//!   [`Source`] / [`Catalog`] methods send a command and await a oneshot reply.
//! * Rows are fetched with a block cursor (`fetch_batch_rows` rows per `SQLFetch`, default
//!   [`DEFAULT_FETCH_ROWS`]) into a columnar ODBC buffer and converted column by column into
//!   Arrow batches through [`ArrowBatcher`]; batches travel through a small bounded channel to the
//!   async stream. No row objects, fixed memory: the bound ODBC buffer is hard-capped at
//!   [`FETCH_BUFFER_BUDGET`] (one batch's byte budget, [`crate::BATCH_BYTES`]) by shrinking the
//!   rows-per-fetch when columns are wide, down to a single row; if even one row would not fit,
//!   the widest text / binary cells are narrowed first (see `fit_row_to_budget`).
//! * Text is bound as UTF-16 (`SQL_C_WCHAR`) so that non-ASCII data survives on Windows, where
//!   the narrow "system encoding" is the ANSI code page and cannot represent all of Unicode.
//! * Known ODBC limitation: block cursors cannot stream individual cells, so unbounded text /
//!   binary columns (`TEXT`, `VARCHAR(MAX)`, `BLOB`, ...) are bound with a fixed per-cell size
//!   ([`MAX_UNBOUNDED_TEXT`] / [`MAX_UNBOUNDED_BINARY`]). Longer values are truncated by the
//!   driver; the indicators are checked after every fetch and a `tracing::warn!` is emitted once
//!   per query when that happens.
//! * Cancellation: the cancel token is checked between fetches and `SQLCancel` is issued on the
//!   running statement from the async side, which is the ODBC way to interrupt a blocking
//!   `SQLExecDirect` / `SQLFetch`. Either path surfaces as [`CoreError::Cancelled`].
//! * Catalog via `SQLTables` / `SQLColumns` (one call per listing); no sizes, no row estimates,
//!   no primary keys (`SQLPrimaryKeys` is not exposed by odbc-api 13), no indexes, no monitor.
//! * The driver list for the UI comes from [`installed_drivers`] (`drivers::odbc_driver_names`).

use std::borrow::Cow;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use arrow_schema::SchemaRef;
use async_trait::async_trait;
use odbc_api::buffers::{AnySlice, BinColumnView, BufferDesc, ColumnarAnyBuffer, NullableSlice, TextColumnView};
use odbc_api::handles::{AsStatementRef, Statement as _};
use odbc_api::sys::{Date as OdbcDate, HStmt, SqlReturn, Time as OdbcTime, Timestamp as OdbcTimestamp};
use odbc_api::{Bit, ColumnDescription, Connection, ConnectionOptions, Cursor, DataType, Error as OdbcError, Preallocated, ResultSetMetadata, RowSetBuffer};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use super::arrow_batch::{days_from_date, micros_from_naive, micros_from_time, schema_from, ArrowBatcher, Cell, ColKind};
use super::{Batch, ConnectParams, Database, Query, Source, SourceStream};
use crate::catalog::Catalog;
use crate::error::{CoreError, Result};
use crate::monitor::{JobHandle, Monitor, ServerSnapshot};
use crate::types::{ColumnInfo, DatabaseInfo, IndexInfo, PlanSummary, SchemaInfo, ServerInfo, SessionInfo, SessionOpts, TableInfo, TableKind, TableRef};
use crate::{BATCH_BYTES, BATCH_ROWS};

// ---------------------------------------------------------------------------------------------
// Tunables
// ---------------------------------------------------------------------------------------------

/// Rows per `SQLFetch` when the session does not set `fetch_batch_rows`.
pub const DEFAULT_FETCH_ROWS: usize = 8192;
/// Per-cell capacity (UTF-16 code units, i.e. 8 KiB) for text columns whose size the driver does
/// not report (`TEXT`, `VARCHAR(MAX)`, `CLOB`, ...). Longer values are truncated (see module docs).
pub const MAX_UNBOUNDED_TEXT: usize = 4096;
/// Per-cell cap (UTF-16 code units) for text columns with a declared size, so that a
/// `VARCHAR(2147483647)` does not allocate gigabytes.
pub const MAX_BOUNDED_TEXT: usize = 32_768;
/// Per-cell capacity in bytes for binary columns without a declared size.
pub const MAX_UNBOUNDED_BINARY: usize = 4096;
/// Per-cell cap in bytes for binary columns with a declared size.
pub const MAX_BOUNDED_BINARY: usize = 65_536;
/// Hard upper bound for the memory bound to one block cursor: one fetch never holds more than
/// one Arrow batch's byte budget ([`BATCH_BYTES`]), so the ODBC buffer adds at most one batch to
/// the pipeline's fixed footprint. Rows per fetch are reduced until `rows x bytes_per_row` fits
/// (down to one row); when a single row is wider than the budget, the text / binary cells are
/// narrowed until it fits (see `fit_row_to_budget`).
pub const FETCH_BUFFER_BUDGET: usize = BATCH_BYTES;
// Compile-time guard: one fetch never holds more than one batch's byte budget.
const _: () = assert!(FETCH_BUFFER_BUDGET <= BATCH_BYTES);
/// Narrowing cells to fit one row into [`FETCH_BUFFER_BUDGET`] never goes below this many bytes
/// of value capacity per text / binary cell (256 UTF-16 code units). Only a result with tens of
/// thousands of text columns could still exceed the budget at this floor.
const MIN_CELL_BYTES: usize = 512;
/// Arrow batches in flight between the actor thread and the async stream (each <= 8 MB).
const STREAM_DEPTH: usize = 2;
/// Per-cell capacity for catalog text (names, remarks, defaults) and probe results.
const META_TEXT_LEN: usize = 1024;
/// Rows per fetch for catalog listings.
const META_BATCH_ROWS: usize = 512;
/// Extra time the async side waits for the login beyond the driver's own login timeout, in case
/// a driver ignores `SQL_ATTR_LOGIN_TIMEOUT`.
const CONNECT_GRACE: Duration = Duration::from_secs(10);

// ---------------------------------------------------------------------------------------------
// Environment and driver manager queries
// ---------------------------------------------------------------------------------------------

/// The process-wide ODBC environment.
///
/// odbc-api keeps exactly one `Environment` in a `std::sync::OnceLock` behind
/// `odbc_api::environment()`; `Environment::new()` is a safe function in odbc-api 13 but the ODBC
/// spec allows only one environment per process and `SQLAllocHandle(SQL_HANDLE_ENV)` is not
/// guaranteed to be re-entrant in every driver manager, so first-time initialisation is
/// serialised here and we never construct a second environment ourselves.
fn env() -> Result<&'static odbc_api::Environment> {
    static INIT: Mutex<()> = Mutex::new(());
    let _guard = INIT.lock().unwrap_or_else(|p| p.into_inner());
    odbc_api::environment().map_err(|e| match e {
        OdbcError::FailedAllocatingEnvironment | OdbcError::FailedSettingConnectionPooling | OdbcError::UnsupportedOdbcApiVersion(_) => {
            CoreError::DriverMissing { id: "odbc".into(), detail: e.to_string() }
        }
        other => odbc_err(other, Phase::Connect),
    })
}

/// Names (descriptions) of the ODBC drivers registered with the driver manager, e.g.
/// `"ODBC Driver 18 for SQL Server"`. Used by `drivers::odbc_driver_names`.
pub fn installed_drivers() -> Result<Vec<String>> {
    let drivers = env()?.drivers().map_err(|e| odbc_err(e, Phase::Connect))?;
    Ok(drivers.into_iter().map(|d| d.description).collect())
}

/// User and system data sources as `(name, driver)` pairs.
pub fn data_sources() -> Result<Vec<(String, String)>> {
    let sources = env()?.data_sources().map_err(|e| odbc_err(e, Phase::Connect))?;
    Ok(sources.into_iter().map(|d| (d.server_name, d.driver)).collect())
}

// ---------------------------------------------------------------------------------------------
// Connection strings
// ---------------------------------------------------------------------------------------------

/// Quote a connection string attribute value with braces when it contains characters that would
/// otherwise break the `key=value;` syntax (`;`, `{`, `}`, `=`, `+`, or surrounding blanks).
/// A `}` inside a braced value is doubled, as the ODBC specification requires.
pub fn escape_odbc_value(value: &str) -> Cow<'_, str> {
    let needs_braces = value.is_empty() || value.contains([';', '{', '}', '=', '+']) || value.starts_with(char::is_whitespace) || value.ends_with(char::is_whitespace);
    if needs_braces {
        Cow::Owned(format!("{{{}}}", value.replace('}', "}}")))
    } else {
        Cow::Borrowed(value)
    }
}

/// Always wrap a value in braces (doubling any `}`), the conventional spelling for `Driver=`,
/// whose values routinely contain blanks and parentheses (`Driver={PostgreSQL Unicode(x64)};`).
fn brace_value(value: &str) -> String {
    format!("{{{}}}", value.replace('}', "}}"))
}

/// Attribute keys present in a connection string, upper-cased and trimmed. Braced values (which
/// may contain `;` and `=`) are skipped correctly.
pub fn connection_string_keys(conn_str: &str) -> Vec<String> {
    let mut keys = Vec::new();
    let mut chars = conn_str.chars().peekable();
    loop {
        // Skip separators and blanks before a key.
        while matches!(chars.peek(), Some(c) if *c == ';' || c.is_whitespace()) {
            chars.next();
        }
        let mut key = String::new();
        let mut saw_eq = false;
        for c in chars.by_ref() {
            if c == '=' {
                saw_eq = true;
                break;
            }
            if c == ';' {
                break;
            }
            key.push(c);
        }
        if key.is_empty() && !saw_eq && chars.peek().is_none() {
            break;
        }
        if !key.trim().is_empty() {
            keys.push(key.trim().to_ascii_uppercase());
        }
        if !saw_eq {
            if chars.peek().is_none() {
                break;
            }
            continue;
        }
        // Value: braced (with `}}` escapes) or up to the next `;`.
        while matches!(chars.peek(), Some(c) if c.is_whitespace()) {
            chars.next();
        }
        if chars.peek() == Some(&'{') {
            chars.next();
            loop {
                match chars.next() {
                    None => break,
                    Some('}') => {
                        if chars.peek() == Some(&'}') {
                            chars.next();
                        } else {
                            break;
                        }
                    }
                    Some(_) => {}
                }
            }
            // Consume up to and including the terminating `;`.
            for c in chars.by_ref() {
                if c == ';' {
                    break;
                }
            }
        } else {
            for c in chars.by_ref() {
                if c == ';' {
                    break;
                }
            }
        }
        if chars.peek().is_none() {
            break;
        }
    }
    keys
}

/// How the host/port/database are spelled for the common driver families.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AddressStyle {
    /// `Server=host,port;Database=db;` (SQL Server, and the default for unknown drivers).
    SqlServer,
    /// `Server=host;Port=port;Database=db;` (MySQL / MariaDB Connector/ODBC).
    ServerPort,
    /// `Servername=host;Port=port;Database=db;` (psqlODBC).
    Postgres,
    /// `DBQ=host:port/service;` (Oracle Instant Client ODBC).
    Oracle,
}

fn address_style(driver: &str) -> AddressStyle {
    let d = driver.to_ascii_lowercase();
    if d.contains("mysql") || d.contains("maria") {
        AddressStyle::ServerPort
    } else if d.contains("postgres") {
        AddressStyle::Postgres
    } else if d.contains("oracle") {
        AddressStyle::Oracle
    } else {
        AddressStyle::SqlServer
    }
}

/// Build the ODBC connection string for `params`.
///
/// * `connection_string` field present: used verbatim (trimmed, `;`-terminated).
/// * Otherwise `Driver={<driver>};` (always braced) and/or `DSN=<dsn>;` from the `driver` / `dsn`
///   fields, then `Server=host[,port];Database=db;` spelled for the driver family (see
///   [`AddressStyle`]). With a DSN the server address comes from the DSN and is not repeated.
/// * `UID=` / `PWD=` are appended from `user` / `password` only when the string does not already
///   carry them; values are brace-escaped with [`escape_odbc_value`].
pub fn build_connection_string(params: &ConnectParams) -> Result<String> {
    let mut s = match params.field("connection_string") {
        Some(cs) => {
            let mut cs = cs.trim().to_string();
            if !cs.ends_with(';') {
                cs.push(';');
            }
            cs
        }
        None => {
            let driver = params.field("driver");
            let dsn = params.field("dsn");
            if driver.is_none() && dsn.is_none() {
                return Err(CoreError::Invalid("odbc: set an ODBC driver name, a DSN or a full connection string".into()));
            }
            let mut cs = String::new();
            if let Some(d) = driver {
                cs.push_str(&format!("Driver={};", brace_value(d.trim())));
            }
            if let Some(dsn) = dsn {
                cs.push_str(&format!("DSN={};", escape_odbc_value(dsn)));
            }
            let style = address_style(driver.unwrap_or(""));
            let host = params.host.trim();
            let db = params.database.as_deref().map(str::trim).filter(|d| !d.is_empty());
            if dsn.is_none() && !host.is_empty() {
                match style {
                    AddressStyle::SqlServer => {
                        if params.port > 0 {
                            cs.push_str(&format!("Server={},{};", escape_odbc_value(host), params.port));
                        } else {
                            cs.push_str(&format!("Server={};", escape_odbc_value(host)));
                        }
                    }
                    AddressStyle::ServerPort | AddressStyle::Postgres => {
                        let key = if style == AddressStyle::Postgres { "Servername" } else { "Server" };
                        cs.push_str(&format!("{key}={};", escape_odbc_value(host)));
                        if params.port > 0 {
                            cs.push_str(&format!("Port={};", params.port));
                        }
                    }
                    AddressStyle::Oracle => {
                        let mut dbq = host.to_string();
                        if params.port > 0 {
                            dbq.push_str(&format!(":{}", params.port));
                        }
                        if let Some(service) = db {
                            dbq.push('/');
                            dbq.push_str(service);
                        }
                        cs.push_str(&format!("DBQ={};", escape_odbc_value(&dbq)));
                    }
                }
            }
            if style != AddressStyle::Oracle || dsn.is_some() {
                if let Some(db) = db {
                    cs.push_str(&format!("Database={};", escape_odbc_value(db)));
                }
            }
            cs
        }
    };
    let keys = connection_string_keys(&s);
    if let Some(user) = params.user.as_deref().filter(|u| !u.is_empty()) {
        if !keys.iter().any(|k| k == "UID") {
            s.push_str(&format!("UID={};", escape_odbc_value(user)));
        }
    }
    if let Some(pw) = params.password.as_deref().filter(|p| !p.is_empty()) {
        if !keys.iter().any(|k| k == "PWD") {
            s.push_str(&format!("PWD={};", escape_odbc_value(pw)));
        }
    }
    Ok(s)
}

// ---------------------------------------------------------------------------------------------
// Error mapping
// ---------------------------------------------------------------------------------------------

/// Which operation produced an ODBC error; decides the default `CoreError` variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Connect,
    Query,
}

fn record_message(record: &odbc_api::handles::Record) -> String {
    odbc_api::handles::slice_to_cow_utf8(&record.message).trim().to_string()
}

/// Map an SQLSTATE + message to a `CoreError`.
///
/// * `28xxx` (or an authentication message during connect) -> `Auth`
/// * `08xxx` (connection exceptions) and `IMxxx` (driver manager: DSN/driver not found) -> `Connect`
/// * `HYT00` / `HYT01` -> `Timeout`, `HY008` -> `Cancelled`
/// * anything else: `Connect` while connecting, otherwise `Query { code: SQLSTATE }`.
fn classify_sqlstate(state: &str, message: &str, phase: Phase) -> CoreError {
    let lower = message.to_ascii_lowercase();
    let auth_text = lower.contains("login failed")
        || lower.contains("access denied")
        || lower.contains("authentication failed")
        || lower.contains("invalid username")
        || lower.contains("invalid password")
        || lower.contains("logon denied");
    match state {
        s if s.starts_with("28") => CoreError::Auth(format!("[{state}] {message}")),
        "HYT00" | "HYT01" => CoreError::Timeout(format!("[{state}] {message}")),
        "HY008" => CoreError::Cancelled,
        s if s.starts_with("08") || s.starts_with("IM") => {
            if phase == Phase::Connect && auth_text {
                CoreError::Auth(format!("[{state}] {message}"))
            } else {
                CoreError::Connect(format!("[{state}] {message}"))
            }
        }
        _ => match phase {
            Phase::Connect if auth_text => CoreError::Auth(format!("[{state}] {message}")),
            Phase::Connect => CoreError::Connect(format!("[{state}] {message}")),
            Phase::Query => CoreError::query_with_code(state, message),
        },
    }
}

/// Convert an odbc-api error into a `CoreError`, keeping the SQLSTATE and diagnostic text.
fn odbc_err(e: OdbcError, phase: Phase) -> CoreError {
    match e {
        OdbcError::Diagnostics { record, function } => {
            let msg = record_message(&record);
            let msg = if msg.is_empty() { format!("{function} failed") } else { msg };
            classify_sqlstate(record.state.as_str(), &msg, phase)
        }
        OdbcError::UnsupportedOdbcApiVersion(record)
        | OdbcError::UnableToRepresentNull(record)
        | OdbcError::OracleOdbcDriverDoesNotSupport64Bit(record)
        | OdbcError::InvalidRowArraySize { record, .. } => {
            let msg = record_message(&record);
            classify_sqlstate(record.state.as_str(), &msg, phase)
        }
        OdbcError::NoDiagnostics { function } => match phase {
            Phase::Connect => CoreError::Connect(format!("{function} failed without a diagnostic record")),
            Phase::Query => CoreError::query(format!("{function} failed without a diagnostic record")),
        },
        OdbcError::AbortedConnectionStringCompletion => CoreError::Connect("connection string completion aborted".into()),
        OdbcError::FailedReadingInput(io) => CoreError::Io(io.to_string()),
        OdbcError::FailedAllocatingEnvironment | OdbcError::FailedSettingConnectionPooling => CoreError::DriverMissing { id: "odbc".into(), detail: e.to_string() },
        OdbcError::TooLargeColumnBufferSize { buffer_index, num_elements, element_size } => CoreError::Internal(format!(
            "odbc: cannot allocate the fetch buffer for column {} ({num_elements} rows x {element_size} bytes); lower fetch_batch_rows",
            buffer_index + 1
        )),
        OdbcError::TooLargeValueForBuffer { indicator, buffer_index } => CoreError::query(format!(
            "odbc: value in column {} is too large for the fetch buffer ({} bytes)",
            buffer_index + 1,
            indicator.map(|n| n.to_string()).unwrap_or_else(|| "unknown".into())
        )),
    }
}

/// Error from a fetch/execute while a cancel may be in flight: the token wins over the driver's
/// own (often generic) diagnostic.
fn query_err(e: OdbcError, cancel: &CancellationToken) -> CoreError {
    if cancel.is_cancelled() {
        return CoreError::Cancelled;
    }
    odbc_err(e, Phase::Query)
}

fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "unknown panic".to_string()
    }
}

// ---------------------------------------------------------------------------------------------
// Cancellation: SQLCancel from another thread
// ---------------------------------------------------------------------------------------------

/// A raw statement handle that may be passed to the cancelling task.
///
/// ODBC explicitly allows `SQLCancel` to be called from a different thread than the one running
/// a function on the statement; the driver manager serialises access per connection.
struct StmtHandle(HStmt);
// SAFETY: the handle is only ever used for `SQLCancel`, which the ODBC specification defines as
// callable from another thread while the statement is busy; the owning thread clears the slot
// (under the same mutex) before freeing the statement, so the pointer never dangles.
unsafe impl Send for StmtHandle {}

/// Shared between the actor (which arms it with the statement in use) and the async watcher
/// (which fires `SQLCancel` when the job's token is cancelled).
#[derive(Default)]
struct CancelSlot {
    stmt: Mutex<Option<StmtHandle>>,
    fired: AtomicBool,
}

impl CancelSlot {
    fn arm(&self, h: HStmt) {
        let mut slot = self.stmt.lock().unwrap_or_else(|p| p.into_inner());
        *slot = Some(StmtHandle(h));
    }

    fn disarm(&self) {
        let mut slot = self.stmt.lock().unwrap_or_else(|p| p.into_inner());
        *slot = None;
    }

    /// Issue `SQLCancel` on the armed statement, if any.
    fn fire(&self) {
        self.fired.store(true, Ordering::SeqCst);
        let slot = self.stmt.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(h) = slot.as_ref() {
            // SAFETY: `h.0` is a statement handle allocated by odbc-api that is still owned by the
            // actor thread (it clears the slot before freeing it, under this mutex).
            let ret = unsafe { odbc_api::sys::SQLCancel(h.0) };
            if ret != SqlReturn::SUCCESS && ret != SqlReturn::SUCCESS_WITH_INFO {
                tracing::debug!("odbc: SQLCancel returned {:?}", ret.0);
            }
        }
    }
}

/// RAII guard: arms the slot on creation, disarms on drop (also on error / panic paths).
struct Armed<'a>(&'a CancelSlot);

impl<'a> Armed<'a> {
    fn new(slot: &'a CancelSlot, h: HStmt) -> Self {
        slot.arm(h);
        Armed(slot)
    }
}

impl Drop for Armed<'_> {
    fn drop(&mut self) {
        self.0.disarm();
    }
}

/// Spawn the task that translates the job's `CancellationToken` into `SQLCancel`. It ends as soon
/// as the actor finishes the command (`done` sender dropped) or the token fires.
fn spawn_cancel_watcher(slot: Arc<CancelSlot>, cancel: CancellationToken, done: oneshot::Receiver<()>) {
    tokio::spawn(async move {
        tokio::select! {
            _ = cancel.cancelled() => slot.fire(),
            _ = done => {}
        }
    });
}

// ---------------------------------------------------------------------------------------------
// Column planning: ODBC type -> (Arrow kind, bound C buffer)
// ---------------------------------------------------------------------------------------------

/// Which C buffer a column is bound to and how its values are converted into [`Cell`]s.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Bind {
    /// UTF-16 text -> `Utf8`.
    WText {
        max_len: usize,
    },
    /// Narrow (ASCII) text parsed as a scaled integer -> `Decimal`.
    DecimalText {
        max_len: usize,
        scale: u8,
    },
    /// Narrow text `HH:MM:SS[.fff]` parsed into microseconds -> `Time` (keeps fractional seconds
    /// that `SQL_TIME_STRUCT` would drop).
    TimeText {
        max_len: usize,
    },
    Binary {
        max_len: usize,
    },
    I8,
    U8,
    I16,
    I32,
    I64,
    F64,
    Date,
    Time,
    Timestamp,
    Bit,
}

impl Bind {
    fn desc(self) -> BufferDesc {
        match self {
            Bind::WText { max_len } => BufferDesc::WText { max_str_len: max_len },
            Bind::DecimalText { max_len, .. } | Bind::TimeText { max_len } => BufferDesc::Text { max_str_len: max_len },
            Bind::Binary { max_len } => BufferDesc::Binary { length: max_len },
            Bind::I8 => BufferDesc::I8 { nullable: true },
            Bind::U8 => BufferDesc::U8 { nullable: true },
            Bind::I16 => BufferDesc::I16 { nullable: true },
            Bind::I32 => BufferDesc::I32 { nullable: true },
            Bind::I64 => BufferDesc::I64 { nullable: true },
            Bind::F64 => BufferDesc::F64 { nullable: true },
            Bind::Date => BufferDesc::Date { nullable: true },
            Bind::Time => BufferDesc::Time { nullable: true },
            Bind::Timestamp => BufferDesc::Timestamp { nullable: true },
            Bind::Bit => BufferDesc::Bit { nullable: true },
        }
    }
}

#[derive(Debug, Clone)]
struct ColPlan {
    name: String,
    kind: ColKind,
    bind: Bind,
}

fn text_len(declared: Option<std::num::NonZeroUsize>) -> usize {
    declared.map(|l| l.get().min(MAX_BOUNDED_TEXT)).unwrap_or(MAX_UNBOUNDED_TEXT)
}

fn binary_len(declared: Option<std::num::NonZeroUsize>) -> usize {
    declared.map(|l| l.get().min(MAX_BOUNDED_BINARY)).unwrap_or(MAX_UNBOUNDED_BINARY)
}

/// Decide the Arrow kind and the C buffer for one result column.
///
/// * integers bind at their native width (widened to `Int64`); unsigned columns one width up,
///   unsigned `BIGINT` as `DECIMAL(20,0)` text
/// * `DECIMAL`/`NUMERIC` with precision <= 38 bind as text and become `Decimal128`; wider or odd
///   precisions fall back to `Utf8`
/// * `TIME` with fractional seconds binds as text to keep the fraction
/// * everything character-like, `Unknown` and driver-specific `Other` types become `Utf8`
///   (`display_size` sizes the buffer for the latter)
fn plan_column(name: String, dt: DataType, unsigned: bool, display_size: Option<usize>) -> ColPlan {
    let (kind, bind) = match dt {
        DataType::Integer => (ColKind::Int64, if unsigned { Bind::I64 } else { Bind::I32 }),
        DataType::SmallInt => (ColKind::Int64, if unsigned { Bind::I32 } else { Bind::I16 }),
        DataType::TinyInt => (ColKind::Int64, if unsigned { Bind::U8 } else { Bind::I8 }),
        DataType::BigInt => {
            if unsigned {
                (ColKind::Decimal { precision: 20, scale: 0 }, Bind::DecimalText { max_len: 24, scale: 0 })
            } else {
                (ColKind::Int64, Bind::I64)
            }
        }
        DataType::Real | DataType::Float { .. } | DataType::Double => (ColKind::Float64, Bind::F64),
        DataType::Numeric { precision, scale } | DataType::Decimal { precision, scale } => {
            if (1..=38).contains(&precision) && scale >= 0 && (scale as usize) <= precision {
                (ColKind::Decimal { precision: precision as u8, scale: scale as i8 }, Bind::DecimalText { max_len: precision + 3, scale: scale as u8 })
            } else {
                let max_len = display_size.map(|d| d.min(MAX_BOUNDED_TEXT)).unwrap_or(precision.max(32) + 8);
                (ColKind::Utf8, Bind::WText { max_len })
            }
        }
        DataType::Date => (ColKind::Date, Bind::Date),
        DataType::Time { precision } if precision <= 0 => (ColKind::Time, Bind::Time),
        DataType::Time { precision } => (ColKind::Time, Bind::TimeText { max_len: 8 + 1 + (precision.min(9) as usize) + 4 }),
        DataType::Timestamp { .. } => (ColKind::Timestamp, Bind::Timestamp),
        DataType::Bit => (ColKind::Bool, Bind::Bit),
        DataType::Binary { length } | DataType::Varbinary { length } | DataType::LongVarbinary { length } => {
            (ColKind::Binary, Bind::Binary { max_len: binary_len(length) })
        }
        DataType::Char { length }
        | DataType::Varchar { length }
        | DataType::WChar { length }
        | DataType::WVarchar { length }
        | DataType::LongVarchar { length }
        | DataType::WLongVarchar { length } => (ColKind::Utf8, Bind::WText { max_len: text_len(length) }),
        DataType::Unknown | DataType::Other { .. } => {
            let max_len = display_size.map(|d| d.clamp(1, MAX_BOUNDED_TEXT)).unwrap_or(MAX_UNBOUNDED_TEXT);
            (ColKind::Utf8, Bind::WText { max_len })
        }
    };
    ColPlan { name, kind, bind }
}

/// Bytes one row occupies in a block-cursor buffer built from `descs` (values + indicators).
fn row_bytes(descs: &[BufferDesc]) -> usize {
    descs.iter().map(|d| d.bytes_per_row()).sum()
}

/// Value capacity in bytes of a variable-length cell (`None` for fixed-size and narrow-text
/// buffers, which are never narrowed: the latter only carry decimals and times, at most ~45 bytes).
fn cell_value_bytes(desc: &BufferDesc) -> Option<usize> {
    match *desc {
        BufferDesc::WText { max_str_len } => Some(max_str_len * 2),
        BufferDesc::Binary { length } => Some(length),
        _ => None,
    }
}

/// `desc` with its value capacity limited to `cap` bytes (UTF-16 text: `cap / 2` code units).
fn capped(desc: BufferDesc, cap: usize) -> BufferDesc {
    match desc {
        BufferDesc::WText { max_str_len } => BufferDesc::WText { max_str_len: max_str_len.min((cap / 2).max(1)) },
        BufferDesc::Binary { length } => BufferDesc::Binary { length: length.min(cap.max(1)) },
        other => other,
    }
}

/// Make sure a single row fits into [`FETCH_BUFFER_BUDGET`] by narrowing the widest text /
/// binary cells to a common capacity, the largest one for which the row fits (never below
/// [`MIN_CELL_BYTES`]). Columns already narrower than that capacity, and fixed-size columns, keep
/// their buffers. Returns the applied per-cell capacity in bytes, or `None` when the row already
/// fit and nothing changed. Values longer than the narrowed cell are truncated by the driver and
/// reported by the per-fetch truncation check, like any other over-long value.
fn fit_row_to_budget(descs: &mut [BufferDesc]) -> Option<usize> {
    if row_bytes(descs) <= FETCH_BUFFER_BUDGET {
        return None;
    }
    let widest = descs.iter().filter_map(cell_value_bytes).max()?;
    if widest <= MIN_CELL_BYTES {
        return None;
    }
    let bytes_at = |cap: usize| descs.iter().map(|d| capped(*d, cap).bytes_per_row()).sum::<usize>();
    // `bytes_at` grows monotonically with `cap`: binary search the largest cap that still fits.
    // Invariant: `lo` is acceptable (fits, or is the floor), every cap > `hi` does not fit.
    let (mut lo, mut hi) = (MIN_CELL_BYTES, widest);
    while lo < hi {
        let mid = lo + (hi - lo).div_ceil(2);
        if bytes_at(mid) <= FETCH_BUFFER_BUDGET {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    for d in descs.iter_mut() {
        *d = capped(*d, lo);
    }
    Some(lo)
}

/// Rows per fetch so that the bound buffer stays within [`FETCH_BUFFER_BUDGET`]. This is a hard
/// cap: when the budget allows fewer rows than requested, fewer rows are fetched, down to one row
/// per `SQLFetch` (callers narrow over-wide rows first with [`fit_row_to_budget`]).
fn rows_for_budget(requested: usize, descs: &[BufferDesc]) -> usize {
    let fit = (FETCH_BUFFER_BUDGET / row_bytes(descs).max(1)).max(1);
    requested.clamp(1, BATCH_ROWS).min(fit)
}

// ---------------------------------------------------------------------------------------------
// Value conversion
// ---------------------------------------------------------------------------------------------

/// Typed view over one bound column of the current row set.
enum View<'a> {
    WText(TextColumnView<'a, u16>),
    Text(TextColumnView<'a, u8>),
    Bin(BinColumnView<'a>),
    I8(NullableSlice<'a, i8>),
    U8(NullableSlice<'a, u8>),
    I16(NullableSlice<'a, i16>),
    I32(NullableSlice<'a, i32>),
    I64(NullableSlice<'a, i64>),
    F64(NullableSlice<'a, f64>),
    Date(NullableSlice<'a, OdbcDate>),
    Time(NullableSlice<'a, OdbcTime>),
    Ts(NullableSlice<'a, OdbcTimestamp>),
    Bit(NullableSlice<'a, Bit>),
}

fn view_of(slice: AnySlice<'_>, col: usize) -> Result<View<'_>> {
    Ok(match slice {
        AnySlice::WText(v) => View::WText(v),
        AnySlice::Text(v) => View::Text(v),
        AnySlice::Binary(v) => View::Bin(v),
        AnySlice::NullableI8(v) => View::I8(v),
        AnySlice::NullableU8(v) => View::U8(v),
        AnySlice::NullableI16(v) => View::I16(v),
        AnySlice::NullableI32(v) => View::I32(v),
        AnySlice::NullableI64(v) => View::I64(v),
        AnySlice::NullableF64(v) => View::F64(v),
        AnySlice::NullableDate(v) => View::Date(v),
        AnySlice::NullableTime(v) => View::Time(v),
        AnySlice::NullableTimestamp(v) => View::Ts(v),
        AnySlice::NullableBit(v) => View::Bit(v),
        _ => return Err(CoreError::Internal(format!("odbc: column {}: unexpected buffer variant", col + 1))),
    })
}

/// Per-query "warn once" flags.
#[derive(Default)]
struct Warnings {
    truncated: bool,
    invalid_datetime: bool,
}

fn date_days(d: &OdbcDate) -> Option<i32> {
    chrono::NaiveDate::from_ymd_opt(d.year as i32, d.month as u32, d.day as u32).map(days_from_date)
}

fn time_micros(t: &OdbcTime) -> i64 {
    (t.hour as i64 * 3600 + t.minute as i64 * 60 + t.second as i64) * 1_000_000
}

fn timestamp_micros(ts: &OdbcTimestamp) -> Option<i64> {
    chrono::NaiveDate::from_ymd_opt(ts.year as i32, ts.month as u32, ts.day as u32)
        .and_then(|d| d.and_hms_nano_opt(ts.hour as u32, ts.minute as u32, ts.second as u32, ts.fraction))
        .map(micros_from_naive)
}

/// Parse `HH:MM:SS[.fraction]` (what drivers emit for `TIME(p)` bound as text) into microseconds.
fn parse_time_text(text: &str) -> Option<i64> {
    let t = text.trim();
    chrono::NaiveTime::parse_from_str(t, "%H:%M:%S%.f").or_else(|_| chrono::NaiveTime::parse_from_str(t, "%H:%M:%S")).ok().map(micros_from_time)
}

fn decode_utf16_into(units: &[u16], out: &mut String) {
    out.clear();
    out.extend(char::decode_utf16(units.iter().copied()).map(|r| r.unwrap_or(char::REPLACEMENT_CHARACTER)));
}

impl View<'_> {
    /// The cell at `row`. Text cells borrow `scratch`, which the caller owns across rows.
    fn cell<'s>(&'s self, plan: &ColPlan, col: usize, row: usize, scratch: &'s mut String, warn: &mut Warnings) -> Result<Cell<'s>> {
        let mut invalid_datetime = || {
            if !warn.invalid_datetime {
                warn.invalid_datetime = true;
                tracing::warn!("odbc: column {} ({}): invalid date/time value replaced with NULL", col + 1, plan.name);
            }
            Cell::Null
        };
        Ok(match self {
            View::WText(v) => match v.get(row) {
                None => Cell::Null,
                Some(units) => {
                    decode_utf16_into(units, scratch);
                    Cell::Str(scratch)
                }
            },
            View::Text(v) => match v.get(row) {
                None => Cell::Null,
                Some(bytes) => match plan.bind {
                    Bind::DecimalText { scale, .. } => Cell::Decimal(odbc_api::decimal_text_to_i128(bytes, scale as usize)),
                    Bind::TimeText { .. } => {
                        let s = String::from_utf8_lossy(bytes);
                        match parse_time_text(&s) {
                            Some(us) => Cell::Time(us),
                            None => return Err(CoreError::Internal(format!("odbc: column {} ({}): cannot parse time '{s}'", col + 1, plan.name))),
                        }
                    }
                    _ => {
                        scratch.clear();
                        scratch.push_str(&String::from_utf8_lossy(bytes));
                        Cell::Str(scratch)
                    }
                },
            },
            View::Bin(v) => v.get(row).map(Cell::Bytes).unwrap_or(Cell::Null),
            View::I8(v) => v.get(row).map(|x| Cell::I64(*x as i64)).unwrap_or(Cell::Null),
            View::U8(v) => v.get(row).map(|x| Cell::I64(*x as i64)).unwrap_or(Cell::Null),
            View::I16(v) => v.get(row).map(|x| Cell::I64(*x as i64)).unwrap_or(Cell::Null),
            View::I32(v) => v.get(row).map(|x| Cell::I64(*x as i64)).unwrap_or(Cell::Null),
            View::I64(v) => v.get(row).map(|x| Cell::I64(*x)).unwrap_or(Cell::Null),
            View::F64(v) => v.get(row).map(|x| Cell::F64(*x)).unwrap_or(Cell::Null),
            View::Date(v) => match v.get(row) {
                None => Cell::Null,
                Some(d) => date_days(d).map(Cell::Date).unwrap_or_else(&mut invalid_datetime),
            },
            View::Time(v) => v.get(row).map(|t| Cell::Time(time_micros(t))).unwrap_or(Cell::Null),
            View::Ts(v) => match v.get(row) {
                None => Cell::Null,
                Some(ts) => timestamp_micros(ts).map(Cell::Timestamp).unwrap_or_else(&mut invalid_datetime),
            },
            View::Bit(v) => v.get(row).map(|b| Cell::Bool(b.as_bool())).unwrap_or(Cell::Null),
        })
    }
}

// ---------------------------------------------------------------------------------------------
// Actor: the thread that owns the connection
// ---------------------------------------------------------------------------------------------

/// One `SQLTables` row.
#[derive(Debug, Clone)]
struct TableRow {
    schema: Option<String>,
    name: String,
    kind: Option<String>,
    remarks: Option<String>,
}

/// One `SQLColumns` row (the subset BulkDB renders).
#[derive(Debug, Clone)]
struct ColumnRow {
    schema: Option<String>,
    table: String,
    name: String,
    sql_type: Option<i32>,
    type_name: String,
    size: Option<i64>,
    digits: Option<i32>,
    nullable: Option<bool>,
    remarks: Option<String>,
    default: Option<String>,
    position: Option<u32>,
}

#[derive(Debug, Clone, Default)]
struct ProbeInfo {
    dbms: String,
    version: String,
    user: Option<String>,
    catalog: String,
}

/// Reply channels of a streaming query. The schema goes first (once), then batches.
struct QueryReply {
    schema_tx: Option<oneshot::Sender<Result<Option<SchemaRef>>>>,
    tx: mpsc::Sender<Result<Batch>>,
}

impl QueryReply {
    fn schema(&mut self, r: Result<Option<SchemaRef>>) {
        if let Some(t) = self.schema_tx.take() {
            let _ = t.send(r);
        }
    }

    /// Route an error to whichever side is still listening.
    fn fail(&mut self, e: CoreError) {
        if self.schema_tx.is_some() {
            self.schema(Err(e));
        } else {
            let _ = self.tx.blocking_send(Err(e));
        }
    }
}

enum Cmd {
    Query {
        sql: String,
        batch_rows: usize,
        cancel: CancellationToken,
        slot: Arc<CancelSlot>,
        /// Dropped when the query is finished; ends the cancel watcher.
        done: oneshot::Sender<()>,
        reply: QueryReply,
    },
    Tables {
        catalog: String,
        schema: String,
        table_type: String,
        tx: oneshot::Sender<Result<Vec<TableRow>>>,
    },
    Columns {
        catalog: String,
        schema: String,
        table: String,
        tx: oneshot::Sender<Result<Vec<ColumnRow>>>,
    },
    Probe {
        tx: oneshot::Sender<Result<ProbeInfo>>,
    },
    Count {
        sql: String,
        cancel: CancellationToken,
        slot: Arc<CancelSlot>,
        done: oneshot::Sender<()>,
        tx: oneshot::Sender<Result<u64>>,
    },
    Close,
}

/// What the actor reports back once the login succeeded.
#[derive(Debug, Clone, Default)]
struct ConnInfo {
    catalog: String,
    dbms: String,
}

/// Body of the connection thread: connect, then serve commands until `Close` or until every
/// sender is gone. The connection is dropped (disconnected) when the thread ends.
fn actor_main(conn_str: String, login_timeout_sec: u32, ready: oneshot::Sender<Result<ConnInfo>>, mut rx: mpsc::UnboundedReceiver<Cmd>) {
    let env = match env() {
        Ok(e) => e,
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };
    let options = ConnectionOptions { login_timeout_sec: Some(login_timeout_sec), packet_size: None };
    let conn = match env.connect_with_connection_string(&conn_str, options) {
        Ok(c) => c,
        Err(e) => {
            let _ = ready.send(Err(odbc_err(e, Phase::Connect)));
            return;
        }
    };
    // Never keep the credentials around longer than the login.
    drop(conn_str);
    let info = ConnInfo {
        catalog: conn.current_catalog().unwrap_or_default().trim().to_string(),
        dbms: conn.database_management_system_name().unwrap_or_default().trim().to_string(),
    };
    if ready.send(Ok(info)).is_err() {
        // The caller gave up (timeout); disconnect right away.
        return;
    }
    while let Some(cmd) = rx.blocking_recv() {
        match cmd {
            Cmd::Close => break,
            Cmd::Query { sql, batch_rows, cancel, slot, done, mut reply } => {
                let outcome = catch_unwind(AssertUnwindSafe(|| run_stream(&conn, &sql, batch_rows, &cancel, &slot, &mut reply)));
                match outcome {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => reply.fail(e),
                    Err(p) => reply.fail(CoreError::Internal(format!("odbc driver panicked: {}", panic_message(p)))),
                }
                drop(done);
            }
            Cmd::Tables { catalog, schema, table_type, tx } => {
                let r = catch_unwind(AssertUnwindSafe(|| run_tables(&conn, &catalog, &schema, &table_type)))
                    .unwrap_or_else(|p| Err(CoreError::Internal(format!("odbc driver panicked: {}", panic_message(p)))));
                let _ = tx.send(r);
            }
            Cmd::Columns { catalog, schema, table, tx } => {
                let r = catch_unwind(AssertUnwindSafe(|| run_columns(&conn, &catalog, &schema, &table)))
                    .unwrap_or_else(|p| Err(CoreError::Internal(format!("odbc driver panicked: {}", panic_message(p)))));
                let _ = tx.send(r);
            }
            Cmd::Probe { tx } => {
                let r = catch_unwind(AssertUnwindSafe(|| run_probe(&conn)))
                    .unwrap_or_else(|p| Err(CoreError::Internal(format!("odbc driver panicked: {}", panic_message(p)))));
                let _ = tx.send(r);
            }
            Cmd::Count { sql, cancel, slot, done, tx } => {
                let r = catch_unwind(AssertUnwindSafe(|| run_count(&conn, &sql, &cancel, &slot)))
                    .unwrap_or_else(|p| Err(CoreError::Internal(format!("odbc driver panicked: {}", panic_message(p)))));
                drop(done);
                let _ = tx.send(r);
            }
        }
    }
    drop(conn);
}

/// Execute `sql` on a fresh statement and stream the result set as Arrow batches.
fn run_stream(conn: &Connection<'_>, sql: &str, batch_rows: usize, cancel: &CancellationToken, slot: &CancelSlot, reply: &mut QueryReply) -> Result<()> {
    if cancel.is_cancelled() {
        return Err(CoreError::Cancelled);
    }
    let mut stmt: Preallocated<_> = conn.preallocate().map_err(|e| odbc_err(e, Phase::Query))?;
    let hstmt = stmt.as_stmt_ref().as_sys();
    let _armed = Armed::new(slot, hstmt);

    let cursor = match stmt.execute(sql, ()).map_err(|e| query_err(e, cancel))? {
        Some(c) => c,
        None => {
            // No result set (DDL/DML or a driver that reports no columns): nothing to stream.
            reply.schema(Ok(None));
            return Ok(());
        }
    };
    if cancel.is_cancelled() {
        return Err(CoreError::Cancelled);
    }
    let mut cursor = cursor;
    let plans = describe_columns(&mut cursor)?;
    let cols: Vec<(String, ColKind)> = plans.iter().map(|p| (p.name.clone(), p.kind)).collect();
    let schema = schema_from(&cols);
    let kinds: Vec<ColKind> = plans.iter().map(|p| p.kind).collect();
    let mut descs: Vec<BufferDesc> = plans.iter().map(|p| p.bind.desc()).collect();
    if let Some(cap) = fit_row_to_budget(&mut descs) {
        tracing::warn!(
            "odbc: one row of {} columns exceeds the {} MiB fetch buffer budget; text/binary cells are limited to {cap} bytes and longer values will be truncated",
            plans.len(),
            FETCH_BUFFER_BUDGET / (1024 * 1024)
        );
    }
    let rows = rows_for_budget(batch_rows, &descs);
    tracing::debug!("odbc: {} columns, {} rows per fetch ({} bytes per row)", plans.len(), rows, row_bytes(&descs));
    let buffer = ColumnarAnyBuffer::try_from_descs(rows, descs.iter().copied()).map_err(|e| odbc_err(e, Phase::Query))?;
    let mut block = cursor.bind_buffer(buffer).map_err(|e| odbc_err(e, Phase::Query))?;

    // The schema is known: let `stream()` return while rows start flowing.
    reply.schema(Ok(Some(schema.clone())));

    let mut batcher = ArrowBatcher::new(schema, kinds);
    let mut scratch = String::new();
    let mut warn = Warnings::default();
    let ncols = plans.len();
    loop {
        if cancel.is_cancelled() {
            return Err(CoreError::Cancelled);
        }
        let Some(rowset) = block.fetch().map_err(|e| query_err(e, cancel))? else { break };
        if !warn.truncated {
            if let Some(t) = rowset.find_truncation() {
                warn.truncated = true;
                let name = plans.get(t.buffer_index).map(|p| p.name.as_str()).unwrap_or("?");
                tracing::warn!(
                    "odbc: values in column {} ({name}) exceed the fetch buffer and were truncated (known ODBC block-cursor limitation; {} bytes needed)",
                    t.buffer_index + 1,
                    t.indicator.map(|n| n.to_string()).unwrap_or_else(|| "unknown".into())
                );
            }
        }
        let views = (0..ncols).map(|c| view_of(rowset.column(c), c)).collect::<Result<Vec<_>>>()?;
        for row in 0..rowset.num_rows() {
            for (c, view) in views.iter().enumerate() {
                let cell = view.cell(&plans[c], c, row, &mut scratch, &mut warn)?;
                batcher.push(cell)?;
            }
            if batcher.end_row()? {
                if let Some(b) = batcher.flush()? {
                    if reply.tx.blocking_send(Ok(Batch::Arrow(b))).is_err() {
                        // The consumer went away (pipeline aborted): stop fetching quietly.
                        return Ok(());
                    }
                }
            }
        }
    }
    if let Some(b) = batcher.flush()? {
        let _ = reply.tx.blocking_send(Ok(Batch::Arrow(b)));
    }
    Ok(())
}

/// Describe every result column with `SQLDescribeCol` (one call per column, no panics on odd
/// driver values) and plan its binding.
fn describe_columns(cursor: &mut impl ResultSetMetadata) -> Result<Vec<ColPlan>> {
    let n = cursor.num_result_cols().map_err(|e| odbc_err(e, Phase::Query))?;
    let n = u16::try_from(n.max(0)).unwrap_or(u16::MAX);
    let mut plans = Vec::with_capacity(n as usize);
    let mut desc = ColumnDescription::default();
    for i in 1..=n {
        cursor.describe_col(i, &mut desc).map_err(|e| odbc_err(e, Phase::Query))?;
        let name = desc.name_to_string().unwrap_or_else(|_| format!("column{i}"));
        let name = if name.is_empty() { format!("column{i}") } else { name };
        let dt = desc.data_type;
        let unsigned = match dt {
            DataType::Integer | DataType::SmallInt | DataType::TinyInt | DataType::BigInt => cursor.column_is_unsigned(i).unwrap_or(false),
            _ => false,
        };
        let display_size = match dt {
            DataType::Unknown | DataType::Other { .. } | DataType::Numeric { .. } | DataType::Decimal { .. } => {
                cursor.col_display_size(i).ok().flatten().map(|d| d.get())
            }
            _ => None,
        };
        plans.push(plan_column(name, dt, unsigned, display_size));
    }
    Ok(plans)
}

/// Fetch selected columns of a (small, metadata) result set as UTF-16 text. Columns beyond the
/// result set width (older drivers return fewer `SQLColumns` columns) come back as `None`.
fn read_text_rows(mut cursor: impl Cursor, cols: &[u16], max_len: usize) -> Result<Vec<Vec<Option<String>>>> {
    let ncols = cursor.num_result_cols().map_err(|e| odbc_err(e, Phase::Query))?.max(0) as u16;
    let bound: Vec<u16> = cols.iter().copied().filter(|c| *c >= 1 && *c <= ncols).collect();
    let buffer = ColumnarAnyBuffer::from_descs_and_indices(META_BATCH_ROWS, bound.iter().map(|c| (*c, BufferDesc::WText { max_str_len: max_len })));
    let mut block = cursor.bind_buffer(buffer).map_err(|e| odbc_err(e, Phase::Query))?;
    let mut rows: Vec<Vec<Option<String>>> = Vec::new();
    while let Some(rs) = block.fetch().map_err(|e| odbc_err(e, Phase::Query))? {
        for r in 0..rs.num_rows() {
            let mut row = Vec::with_capacity(cols.len());
            for want in cols {
                let value = bound.iter().position(|b| b == want).and_then(|idx| match rs.column(idx) {
                    AnySlice::WText(v) => v.get(r).map(String::from_utf16_lossy),
                    _ => None,
                });
                row.push(value);
            }
            rows.push(row);
        }
    }
    Ok(rows)
}

fn opt_text(v: &Option<String>) -> Option<String> {
    v.as_deref().map(str::trim).filter(|s| !s.is_empty()).map(str::to_string)
}

fn run_tables(conn: &Connection<'_>, catalog: &str, schema: &str, table_type: &str) -> Result<Vec<TableRow>> {
    let cursor = conn.tables(catalog, schema, "", table_type).map_err(|e| odbc_err(e, Phase::Query))?;
    // TABLE_CAT, TABLE_SCHEM, TABLE_NAME, TABLE_TYPE, REMARKS
    let rows = read_text_rows(cursor, &[2, 3, 4, 5], META_TEXT_LEN)?;
    Ok(rows
        .into_iter()
        .filter_map(|r| {
            let name = opt_text(&r[1])?;
            Some(TableRow { schema: opt_text(&r[0]), name, kind: opt_text(&r[2]), remarks: opt_text(&r[3]) })
        })
        .collect())
}

fn run_columns(conn: &Connection<'_>, catalog: &str, schema: &str, table: &str) -> Result<Vec<ColumnRow>> {
    let cursor = conn.columns(catalog, schema, table, "").map_err(|e| odbc_err(e, Phase::Query))?;
    // 2 TABLE_SCHEM, 3 TABLE_NAME, 4 COLUMN_NAME, 5 DATA_TYPE, 6 TYPE_NAME, 7 COLUMN_SIZE,
    // 9 DECIMAL_DIGITS, 11 NULLABLE, 12 REMARKS, 13 COLUMN_DEF, 17 ORDINAL_POSITION, 18 IS_NULLABLE
    let rows = read_text_rows(cursor, &[2, 3, 4, 5, 6, 7, 9, 11, 12, 13, 17, 18], META_TEXT_LEN)?;
    Ok(rows
        .into_iter()
        .filter_map(|r| {
            let name = opt_text(&r[2])?;
            let nullable = match opt_text(&r[7]).as_deref() {
                Some("0") => Some(false),
                Some("1") => Some(true),
                _ => match opt_text(&r[11]).as_deref().map(str::to_ascii_uppercase).as_deref() {
                    Some("NO") => Some(false),
                    Some("YES") => Some(true),
                    _ => None,
                },
            };
            Some(ColumnRow {
                schema: opt_text(&r[0]),
                table: opt_text(&r[1]).unwrap_or_default(),
                name,
                sql_type: opt_text(&r[3]).and_then(|s| s.parse().ok()),
                type_name: opt_text(&r[4]).unwrap_or_else(|| "unknown".into()),
                size: opt_text(&r[5]).and_then(|s| s.parse().ok()),
                digits: opt_text(&r[6]).and_then(|s| s.parse().ok()),
                nullable,
                remarks: opt_text(&r[8]),
                default: opt_text(&r[9]),
                position: opt_text(&r[10]).and_then(|s| s.parse().ok()),
            })
        })
        .collect())
}

/// DBMS specific statements returning `(version, user)`; tried in order until one succeeds.
/// odbc-api 13 does not expose `SQLGetInfo(SQL_DBMS_VER / SQL_USER_NAME)` on a live connection,
/// so the version is read with a query where the DBMS family is recognised.
fn probe_queries(dbms: &str) -> &'static [&'static str] {
    let d = dbms.to_ascii_lowercase();
    if d.contains("sql server") || d.contains("azure") {
        &["SELECT CAST(SERVERPROPERTY('ProductVersion') AS NVARCHAR(128)), SUSER_SNAME()"]
    } else if d.contains("postgres") {
        &["SELECT current_setting('server_version'), current_user"]
    } else if d.contains("mysql") || d.contains("maria") {
        &["SELECT VERSION(), CURRENT_USER()"]
    } else if d.contains("oracle") {
        &["SELECT (SELECT banner FROM v$version WHERE ROWNUM = 1), USER FROM dual", "SELECT NULL, USER FROM dual"]
    } else if d.contains("sqlite") {
        &["SELECT sqlite_version(), NULL"]
    } else if d.contains("db2") {
        &["SELECT service_level, CURRENT USER FROM TABLE(SYSPROC.ENV_GET_INST_INFO()) AS t", "SELECT NULL, CURRENT USER FROM sysibm.sysdummy1"]
    } else if d.contains("snowflake") {
        &["SELECT CURRENT_VERSION(), CURRENT_USER()"]
    } else if d.contains("access") || d.contains("excel") || d.contains("text") {
        &[]
    } else {
        &["SELECT version()", "SELECT @@version"]
    }
}

fn run_probe(conn: &Connection<'_>) -> Result<ProbeInfo> {
    let dbms = conn.database_management_system_name().map_err(|e| odbc_err(e, Phase::Query))?.trim().to_string();
    let catalog = conn.current_catalog().unwrap_or_default().trim().to_string();
    let mut info = ProbeInfo { dbms: dbms.clone(), version: String::new(), user: None, catalog };
    for sql in probe_queries(&dbms) {
        let cursor = match conn.execute(sql, (), None) {
            Ok(Some(c)) => c,
            Ok(None) => continue,
            Err(e) => {
                tracing::debug!("odbc probe query failed ({sql}): {e}");
                continue;
            }
        };
        match read_text_rows(cursor, &[1, 2], META_TEXT_LEN) {
            Ok(rows) => {
                if let Some(row) = rows.first() {
                    info.version = opt_text(&row[0]).unwrap_or_default();
                    info.user = opt_text(&row[1]);
                    break;
                }
            }
            Err(e) => tracing::debug!("odbc probe query failed ({sql}): {e}"),
        }
    }
    Ok(info)
}

fn run_count(conn: &Connection<'_>, sql: &str, cancel: &CancellationToken, slot: &CancelSlot) -> Result<u64> {
    if cancel.is_cancelled() {
        return Err(CoreError::Cancelled);
    }
    let mut stmt = conn.preallocate().map_err(|e| odbc_err(e, Phase::Query))?;
    let hstmt = stmt.as_stmt_ref().as_sys();
    let _armed = Armed::new(slot, hstmt);
    let cursor = stmt.execute(sql, ()).map_err(|e| query_err(e, cancel))?.ok_or_else(|| CoreError::query("count query returned no result set"))?;
    let rows = read_text_rows(cursor, &[1], 64).map_err(|e| if cancel.is_cancelled() { CoreError::Cancelled } else { e })?;
    let text = rows.first().and_then(|r| opt_text(&r[0])).ok_or_else(|| CoreError::query("count query returned no row"))?;
    text.trim().parse::<u64>().map_err(|_| CoreError::query(format!("count query returned '{text}'")))
}

// ---------------------------------------------------------------------------------------------
// Catalog rendering helpers
// ---------------------------------------------------------------------------------------------

/// Render `TYPE_NAME` with `COLUMN_SIZE` / `DECIMAL_DIGITS` the way DDL would spell it.
fn render_type(type_name: &str, sql_type: Option<i32>, size: Option<i64>, digits: Option<i32>) -> String {
    let name = type_name.trim();
    if name.contains('(') || name.is_empty() {
        return if name.is_empty() { "unknown".into() } else { name.to_string() };
    }
    const CHAR_TYPES: &[i32] = &[1, 12, -8, -9, -2, -3]; // CHAR, VARCHAR, WCHAR, WVARCHAR, BINARY, VARBINARY
    const DECIMAL_TYPES: &[i32] = &[2, 3]; // NUMERIC, DECIMAL
    match (sql_type, size) {
        (Some(t), Some(n)) if CHAR_TYPES.contains(&t) && n > 0 => {
            if n >= i32::MAX as i64 {
                format!("{name}(max)")
            } else {
                format!("{name}({n})")
            }
        }
        (Some(t), Some(p)) if DECIMAL_TYPES.contains(&t) && p > 0 => format!("{name}({p},{})", digits.unwrap_or(0).max(0)),
        _ => name.to_string(),
    }
}

fn table_kind(type_name: Option<&str>) -> TableKind {
    match type_name.map(|t| t.trim().to_ascii_uppercase()).as_deref() {
        Some("TABLE") | Some("BASE TABLE") => TableKind::Table,
        Some("VIEW") => TableKind::View,
        Some("MATERIALIZED VIEW") => TableKind::MaterializedView,
        Some("FOREIGN TABLE") => TableKind::Foreign,
        Some("PARTITIONED TABLE") => TableKind::Partitioned,
        Some(t) if t.contains("VIEW") => TableKind::View,
        Some(t) if t.contains("TABLE") => TableKind::Table,
        _ => TableKind::Other,
    }
}

// ---------------------------------------------------------------------------------------------
// The database object
// ---------------------------------------------------------------------------------------------

/// Display name for a connection whose driver reports no current catalog (SQLite, flat files).
const NO_CATALOG: &str = "(default)";

/// An ODBC connection: the async handle to the actor thread.
pub struct OdbcDatabase {
    tx: mpsc::UnboundedSender<Cmd>,
    /// Current catalog as reported by the driver after login (may be empty).
    catalog: String,
    /// `SQLGetInfo(SQL_DBMS_NAME)`.
    dbms: String,
    user: Option<String>,
    driver_name: Option<String>,
}

impl OdbcDatabase {
    /// Connect on a dedicated thread. Fails with `Invalid` when neither a driver, a DSN nor a
    /// connection string is given, `Connect` / `Auth` / `Timeout` from the driver's diagnostics.
    pub async fn connect(params: &ConnectParams) -> Result<Self> {
        let conn_str = build_connection_string(params)?;
        let login_timeout = params.connect_timeout.as_secs().clamp(1, u32::MAX as u64) as u32;
        let (ready_tx, ready_rx) = oneshot::channel();
        let (tx, rx) = mpsc::unbounded_channel();
        let name = format!("odbc-{}", params.application_name);
        thread::Builder::new()
            .name(name)
            .spawn(move || actor_main(conn_str, login_timeout, ready_tx, rx))
            .map_err(|e| CoreError::Internal(format!("odbc: cannot spawn connection thread: {e}")))?;
        let wait = params.connect_timeout + CONNECT_GRACE;
        let info = match tokio::time::timeout(wait, ready_rx).await {
            Ok(Ok(r)) => r?,
            Ok(Err(_)) => return Err(CoreError::Connect("odbc connection thread exited before the login completed".into())),
            Err(_) => return Err(CoreError::Timeout(format!("odbc login did not complete within {} s", wait.as_secs()))),
        };
        Ok(OdbcDatabase {
            tx,
            catalog: info.catalog,
            dbms: info.dbms,
            user: params.user.clone().filter(|u| !u.is_empty()),
            driver_name: params.field("driver").map(str::to_string),
        })
    }

    /// `SQLGetInfo(SQL_DBMS_NAME)` of the connected server, e.g. `"Microsoft SQL Server"`.
    pub fn dbms_name(&self) -> &str {
        &self.dbms
    }

    /// Catalog the connection is attached to, as the driver reports it (may be empty).
    pub fn current_catalog(&self) -> &str {
        &self.catalog
    }

    fn send(&self, cmd: Cmd) -> Result<()> {
        self.tx.send(cmd).map_err(|_| CoreError::Connect("odbc connection is closed".into()))
    }

    async fn call<T>(&self, make: impl FnOnce(oneshot::Sender<Result<T>>) -> Cmd) -> Result<T> {
        let (tx, rx) = oneshot::channel();
        self.send(make(tx))?;
        rx.await.map_err(|_| CoreError::Internal("odbc connection thread dropped the reply".into()))?
    }

    fn catalog_display(&self) -> String {
        if self.catalog.is_empty() {
            NO_CATALOG.to_string()
        } else {
            self.catalog.clone()
        }
    }

    /// Translate a database name coming back from the UI into the `SQLTables` catalog argument.
    fn catalog_arg(&self, db: &str) -> String {
        if db.is_empty() || db == NO_CATALOG {
            String::new()
        } else {
            db.to_string()
        }
    }

    fn catalog_of(&self, table: &TableRef) -> String {
        match table.database.as_deref().filter(|d| !d.is_empty()) {
            Some(db) => self.catalog_arg(db),
            None => self.catalog.clone(),
        }
    }

    fn is_mysql(&self) -> bool {
        let d = self.dbms.to_ascii_lowercase();
        d.contains("mysql") || d.contains("maria")
    }

    async fn column_rows(&self, table: &TableRef) -> Result<Vec<ColumnRow>> {
        let catalog = self.catalog_of(table);
        let schema = table.schema.clone().unwrap_or_default();
        let name = table.name.clone();
        let rows = self.call(|tx| Cmd::Columns { catalog, schema: schema.clone(), table: name.clone(), tx }).await?;
        // `SQLColumns` arguments are search patterns (`_` matches any character): keep only the
        // exact table (and schema, when one was given).
        let mut rows: Vec<ColumnRow> = rows.into_iter().filter(|r| r.table == table.name && (schema.is_empty() || r.schema.as_deref().unwrap_or("") == schema)).collect();
        rows.sort_by_key(|r| r.position.unwrap_or(u32::MAX));
        Ok(rows)
    }
}

impl Drop for OdbcDatabase {
    fn drop(&mut self) {
        let _ = self.tx.send(Cmd::Close);
    }
}

impl std::fmt::Debug for OdbcDatabase {
    /// Never prints credentials or the connection string.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OdbcDatabase").field("dbms", &self.dbms).field("catalog", &self.catalog).field("driver", &self.driver_name).field("user", &self.user).finish()
    }
}

#[async_trait]
impl Source for OdbcDatabase {
    async fn probe(&self) -> Result<ServerInfo> {
        let p = self.call(|tx| Cmd::Probe { tx }).await?;
        let version = match (p.dbms.is_empty(), p.version.is_empty()) {
            (false, false) => format!("{} {}", p.dbms, p.version),
            (false, true) => p.dbms.clone(),
            (true, _) => p.version.clone(),
        };
        let mut settings = std::collections::BTreeMap::new();
        if !p.dbms.is_empty() {
            settings.insert("dbms_name".to_string(), p.dbms.clone());
        }
        if let Some(d) = &self.driver_name {
            settings.insert("odbc_driver".to_string(), d.clone());
        }
        let mut notes = vec!["ODBC: server memory, session monitoring and execution plans are not available through the generic driver".to_string()];
        if p.version.is_empty() {
            notes.push("the server version could not be determined for this DBMS".to_string());
        }
        Ok(ServerInfo {
            driver: "odbc".into(),
            version,
            user: p.user.or_else(|| self.user.clone()),
            database: Some(if p.catalog.is_empty() { self.catalog_display() } else { p.catalog }),
            ram_bytes: None,
            ram_source: None,
            settings,
            can_write: None,
            notes,
        })
    }

    /// ODBC has no portable session settings; only `fetch_batch_rows` applies (read in `stream`).
    async fn apply_session(&self, _opts: &SessionOpts) -> Result<()> {
        Ok(())
    }

    async fn stream(&self, q: &Query, opts: &SessionOpts, cancel: CancellationToken) -> Result<SourceStream> {
        let batch_rows = opts.fetch_batch_rows.map(|n| n as usize).unwrap_or(DEFAULT_FETCH_ROWS).clamp(1, BATCH_ROWS);
        let (schema_tx, schema_rx) = oneshot::channel();
        let (tx, rx) = mpsc::channel::<Result<Batch>>(STREAM_DEPTH);
        let slot = Arc::new(CancelSlot::default());
        let (done_tx, done_rx) = oneshot::channel();
        self.send(Cmd::Query {
            sql: q.sql.clone(),
            batch_rows,
            cancel: cancel.clone(),
            slot: slot.clone(),
            done: done_tx,
            reply: QueryReply { schema_tx: Some(schema_tx), tx },
        })?;
        spawn_cancel_watcher(slot, cancel, done_rx);
        let schema = schema_rx.await.map_err(|_| CoreError::Internal("odbc connection thread dropped the query".into()))??;
        let stream = futures::stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|item| (item, rx)) });
        Ok(SourceStream { schema, stream: Box::pin(stream) })
    }

    /// ODBC has no portable EXPLAIN.
    async fn explain(&self, _sql: &str, _analyze: bool) -> Result<Option<PlanSummary>> {
        Ok(None)
    }

    fn session_id(&self) -> Option<String> {
        None
    }

    fn driver_id(&self) -> &'static str {
        "odbc"
    }
}

#[async_trait]
impl Catalog for OdbcDatabase {
    async fn databases(&self) -> Result<Vec<DatabaseInfo>> {
        Ok(vec![DatabaseInfo { name: self.catalog_display(), accessible: true, is_current: true, ..Default::default() }])
    }

    async fn schemas(&self, db: &str) -> Result<Vec<SchemaInfo>> {
        let catalog = self.catalog_arg(db);
        let rows = self.call(|tx| Cmd::Tables { catalog, schema: String::new(), table_type: String::new(), tx }).await?;
        let mut counts: std::collections::BTreeMap<String, u64> = std::collections::BTreeMap::new();
        for r in rows {
            *counts.entry(r.schema.unwrap_or_default()).or_insert(0) += 1;
        }
        Ok(counts.into_iter().map(|(name, n)| SchemaInfo { name, owner: None, table_count: Some(n) }).collect())
    }

    async fn tables(&self, db: &str, schema: &str) -> Result<Vec<TableInfo>> {
        let catalog = self.catalog_arg(db);
        let schema_arg = schema.to_string();
        let rows = self.call(|tx| Cmd::Tables { catalog, schema: schema_arg, table_type: "TABLE,VIEW".into(), tx }).await?;
        let mut out: Vec<TableInfo> = rows
            .into_iter()
            .filter(|r| schema.is_empty() || r.schema.as_deref().unwrap_or("") == schema)
            .map(|r| TableInfo {
                schema: r.schema.unwrap_or_default(),
                name: r.name,
                kind: table_kind(r.kind.as_deref()),
                estimated_rows: None,
                table_bytes: None,
                index_bytes: None,
                total_bytes: None,
                comment: r.remarks,
            })
            .collect();
        out.sort_by(|a, b| a.schema.cmp(&b.schema).then_with(|| a.name.cmp(&b.name)));
        Ok(out)
    }

    async fn columns(&self, table: &TableRef) -> Result<Vec<ColumnInfo>> {
        let rows = self.column_rows(table).await?;
        Ok(rows
            .into_iter()
            .enumerate()
            .map(|(i, r)| ColumnInfo {
                position: r.position.unwrap_or(i as u32 + 1),
                name: r.name,
                data_type: render_type(&r.type_name, r.sql_type, r.size, r.digits),
                nullable: r.nullable.unwrap_or(true),
                default: r.default,
                // SQLPrimaryKeys / SQLStatistics are not exposed by odbc-api 13.
                primary_key: false,
                foreign_key: None,
                unique: false,
                comment: r.remarks,
            })
            .collect())
    }

    async fn indexes(&self, _table: &TableRef) -> Result<Vec<IndexInfo>> {
        Ok(vec![])
    }

    /// `CREATE TABLE` reconstructed from `SQLColumns` (generic SQL; no constraints or indexes).
    async fn ddl(&self, table: &TableRef) -> Result<String> {
        let cols = self.columns(table).await?;
        if cols.is_empty() {
            return Err(CoreError::NotFound(format!("table {} has no columns or does not exist", table.qualified())));
        }
        let mut lines = Vec::with_capacity(cols.len());
        for c in &cols {
            let mut line = format!("  {} {}", self.quote_ident(&c.name), c.data_type);
            if !c.nullable {
                line.push_str(" NOT NULL");
            }
            if let Some(d) = &c.default {
                line.push_str(" DEFAULT ");
                line.push_str(d);
            }
            lines.push(line);
        }
        Ok(format!(
            "-- reconstructed from ODBC SQLColumns; keys, constraints and indexes are not available\nCREATE TABLE {} (\n{}\n);\n",
            self.quote_table(table),
            lines.join(",\n")
        ))
    }

    async fn exact_count(&self, table: &TableRef, cancel: CancellationToken) -> Result<u64> {
        let sql = format!("SELECT COUNT(*) FROM {}", self.quote_table(table));
        let slot = Arc::new(CancelSlot::default());
        let (done_tx, done_rx) = oneshot::channel();
        spawn_cancel_watcher(slot.clone(), cancel.clone(), done_rx);
        self.call(|tx| Cmd::Count { sql, cancel, slot, done: done_tx, tx }).await
    }

    /// Double quotes (SQL standard) everywhere except MySQL / MariaDB, which need backticks
    /// unless `ANSI_QUOTES` is on.
    fn quote_ident(&self, ident: &str) -> String {
        if self.is_mysql() {
            format!("`{}`", ident.replace('`', "``"))
        } else {
            format!("\"{}\"", ident.replace('"', "\"\""))
        }
    }
}

#[async_trait]
impl Monitor for OdbcDatabase {
    async fn snapshot(&self, _job: &JobHandle) -> Result<ServerSnapshot> {
        Ok(ServerSnapshot { found: false, ..Default::default() })
    }

    async fn active_sessions(&self, _touching: &[TableRef]) -> Result<Vec<SessionInfo>> {
        Ok(vec![])
    }
}

impl Database for OdbcDatabase {}

// ---------------------------------------------------------------------------------------------
// Tests that need no driver manager
// ---------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::num::NonZeroUsize;

    use super::*;
    use crate::source::SslMode;

    fn params(fields: &[(&str, &str)], user: Option<&str>, password: Option<&str>) -> ConnectParams {
        ConnectParams {
            driver: "odbc".into(),
            host: "db.example.com".into(),
            port: 1433,
            database: Some("sales".into()),
            user: user.map(str::to_string),
            password: password.map(str::to_string),
            fields: fields.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect::<BTreeMap<_, _>>(),
            application_name: "bulkdb-test".into(),
            connect_timeout: Duration::from_secs(15),
            ssl: SslMode::Prefer,
        }
    }

    #[test]
    fn escapes_values_with_braces_only_when_needed() {
        assert_eq!(escape_odbc_value("plain"), "plain");
        assert_eq!(escape_odbc_value("a;b"), "{a;b}");
        assert_eq!(escape_odbc_value("a}b"), "{a}}b}");
        assert_eq!(escape_odbc_value("p=w"), "{p=w}");
        assert_eq!(escape_odbc_value("x+y"), "{x+y}");
        assert_eq!(escape_odbc_value(" lead"), "{ lead}");
        assert_eq!(escape_odbc_value(""), "{}");
    }

    #[test]
    fn parses_keys_honouring_braces() {
        let keys = connection_string_keys("Driver={ODBC Driver 18 for SQL Server};Server=h,1;PWD={a;b=c}}};uid=x");
        assert_eq!(keys, vec!["DRIVER", "SERVER", "PWD", "UID"]);
        assert!(connection_string_keys("").is_empty());
        assert_eq!(connection_string_keys("  ;;DSN=foo;;"), vec!["DSN"]);
    }

    #[test]
    fn builds_sql_server_style_string() {
        let p = params(&[("driver", "ODBC Driver 18 for SQL Server")], Some("sa"), Some("p;w}d"));
        let s = build_connection_string(&p).unwrap();
        assert_eq!(s, "Driver={ODBC Driver 18 for SQL Server};Server=db.example.com,1433;Database=sales;UID=sa;PWD={p;w}}d};");
    }

    #[test]
    fn builds_mysql_postgres_oracle_styles() {
        let mut p = params(&[("driver", "MySQL ODBC 9.0 Unicode Driver")], None, None);
        p.port = 3306;
        assert_eq!(build_connection_string(&p).unwrap(), "Driver={MySQL ODBC 9.0 Unicode Driver};Server=db.example.com;Port=3306;Database=sales;");
        let mut p = params(&[("driver", "PostgreSQL Unicode(x64)")], None, None);
        p.port = 5432;
        assert_eq!(build_connection_string(&p).unwrap(), "Driver={PostgreSQL Unicode(x64)};Servername=db.example.com;Port=5432;Database=sales;");
        let mut p = params(&[("driver", "Oracle in instantclient_21_3")], None, None);
        p.port = 1521;
        assert_eq!(build_connection_string(&p).unwrap(), "Driver={Oracle in instantclient_21_3};DBQ=db.example.com:1521/sales;");
    }

    #[test]
    fn dsn_omits_server_and_zero_port_omits_port() {
        let p = params(&[("dsn", "MyDsn")], Some("u"), None);
        assert_eq!(build_connection_string(&p).unwrap(), "DSN=MyDsn;Database=sales;UID=u;");
        let mut p = params(&[("driver", "SQL Server")], None, None);
        p.port = 0;
        p.database = None;
        assert_eq!(build_connection_string(&p).unwrap(), "Driver={SQL Server};Server=db.example.com;");
    }

    #[test]
    fn driver_name_is_always_braced_and_closing_braces_doubled() {
        let mut p = params(&[("driver", " Odd}Driver ")], None, None);
        p.port = 0;
        p.database = None;
        assert_eq!(build_connection_string(&p).unwrap(), "Driver={Odd}}Driver};Server=db.example.com;");
        assert_eq!(connection_string_keys("Driver={Odd}}Driver};Server=h;"), vec!["DRIVER", "SERVER"]);
    }

    #[test]
    fn connection_string_field_wins_and_credentials_are_not_duplicated() {
        let p = params(&[("connection_string", "DSN=x;Uid=already;Pwd={s;s}")], Some("u"), Some("p"));
        assert_eq!(build_connection_string(&p).unwrap(), "DSN=x;Uid=already;Pwd={s;s};");
        let p = params(&[("connection_string", "Driver={SQL Server};Server=h")], Some("u"), Some("p"));
        assert_eq!(build_connection_string(&p).unwrap(), "Driver={SQL Server};Server=h;UID=u;PWD=p;");
    }

    #[test]
    fn missing_driver_dsn_and_string_is_invalid() {
        let p = params(&[], Some("u"), Some("p"));
        assert!(matches!(build_connection_string(&p), Err(CoreError::Invalid(_))));
    }

    #[test]
    fn classifies_sqlstates() {
        assert!(matches!(classify_sqlstate("28000", "Login failed for user 'x'", Phase::Connect), CoreError::Auth(_)));
        assert!(matches!(classify_sqlstate("08001", "TCP Provider: refused", Phase::Connect), CoreError::Connect(_)));
        assert!(matches!(classify_sqlstate("08001", "FATAL: password authentication failed", Phase::Connect), CoreError::Auth(_)));
        assert!(matches!(classify_sqlstate("IM002", "Data source name not found", Phase::Connect), CoreError::Connect(_)));
        assert!(matches!(classify_sqlstate("HYT00", "Login timeout expired", Phase::Connect), CoreError::Timeout(_)));
        assert!(matches!(classify_sqlstate("HY008", "Operation canceled", Phase::Query), CoreError::Cancelled));
        assert!(matches!(classify_sqlstate("42S02", "Invalid object name", Phase::Query), CoreError::Query { code: Some(c), .. } if c == "42S02"));
        assert!(matches!(classify_sqlstate("08S01", "Communication link failure", Phase::Query), CoreError::Connect(_)));
    }

    #[test]
    fn plans_types() {
        let p = plan_column("i".into(), DataType::Integer, false, None);
        assert_eq!((p.kind, p.bind), (ColKind::Int64, Bind::I32));
        let p = plan_column("u".into(), DataType::BigInt, true, None);
        assert_eq!(p.kind, ColKind::Decimal { precision: 20, scale: 0 });
        let p = plan_column("d".into(), DataType::Decimal { precision: 12, scale: 2 }, false, None);
        assert_eq!(p.kind, ColKind::Decimal { precision: 12, scale: 2 });
        assert_eq!(p.bind, Bind::DecimalText { max_len: 15, scale: 2 });
        let p = plan_column("big".into(), DataType::Numeric { precision: 60, scale: 10 }, false, Some(70));
        assert_eq!(p.kind, ColKind::Utf8);
        let p = plan_column("t".into(), DataType::Time { precision: 6 }, false, None);
        assert_eq!((p.kind, p.bind), (ColKind::Time, Bind::TimeText { max_len: 19 }));
        let p = plan_column("t0".into(), DataType::Time { precision: 0 }, false, None);
        assert_eq!(p.bind, Bind::Time);
        let p = plan_column("txt".into(), DataType::LongVarchar { length: NonZeroUsize::new(i32::MAX as usize) }, false, None);
        assert_eq!(p.bind, Bind::WText { max_len: MAX_BOUNDED_TEXT });
        let p = plan_column("max".into(), DataType::Varchar { length: None }, false, None);
        assert_eq!(p.bind, Bind::WText { max_len: MAX_UNBOUNDED_TEXT });
        let p = plan_column("bin".into(), DataType::Varbinary { length: NonZeroUsize::new(16) }, false, None);
        assert_eq!((p.kind, p.bind), (ColKind::Binary, Bind::Binary { max_len: 16 }));
        let p = plan_column("guid".into(), DataType::Other { data_type: odbc_api::sys::SqlDataType(-11), column_size: None, decimal_digits: 0 }, false, Some(36));
        assert_eq!((p.kind, p.bind), (ColKind::Utf8, Bind::WText { max_len: 36 }));
        let p = plan_column("b".into(), DataType::Bit, false, None);
        assert_eq!((p.kind, p.bind), (ColKind::Bool, Bind::Bit));
    }

    #[test]
    fn rows_per_fetch_respects_budget() {
        let wide = vec![BufferDesc::WText { max_str_len: MAX_BOUNDED_TEXT }; 8];
        let rows = rows_for_budget(65_536, &wide);
        assert!(rows >= 1);
        assert!(rows * row_bytes(&wide) <= FETCH_BUFFER_BUDGET);

        // The review example: 30 x NVARCHAR(255) is 15,600 bytes per row, so the default request
        // of 8192 rows (~128 MB) is cut to what fits into the budget.
        let nvarchar = vec![BufferDesc::WText { max_str_len: text_len(NonZeroUsize::new(255)) }; 30];
        assert_eq!(row_bytes(&nvarchar), 30 * 520);
        let rows = rows_for_budget(DEFAULT_FETCH_ROWS, &nvarchar);
        assert_eq!(rows, FETCH_BUFFER_BUDGET / (30 * 520));
        assert!(rows * row_bytes(&nvarchar) <= FETCH_BUFFER_BUDGET);

        // The budget is a hard cap: very wide rows go down to one row per fetch instead of being
        // floored at a minimum row count that would overshoot it.
        let very_wide = vec![BufferDesc::WText { max_str_len: MAX_BOUNDED_TEXT }; 100];
        assert!(row_bytes(&very_wide) <= FETCH_BUFFER_BUDGET);
        assert!(2 * row_bytes(&very_wide) > FETCH_BUFFER_BUDGET);
        assert_eq!(rows_for_budget(DEFAULT_FETCH_ROWS, &very_wide), 1);

        // An explicit small request is honoured, an oversized one is clamped to BATCH_ROWS.
        let narrow = vec![BufferDesc::I64 { nullable: true }; 3];
        assert_eq!(rows_for_budget(8192, &narrow), 8192);
        assert_eq!(rows_for_budget(1, &narrow), 1);
        assert_eq!(rows_for_budget(0, &narrow), 1);
        assert_eq!(rows_for_budget(1 << 20, &narrow), BATCH_ROWS);
        assert!(BATCH_ROWS * row_bytes(&narrow) <= FETCH_BUFFER_BUDGET);
    }

    #[test]
    fn over_wide_row_is_narrowed_to_fit_budget() {
        // A row that already fits is left untouched.
        let mut fits = vec![BufferDesc::WText { max_str_len: MAX_BOUNDED_TEXT }; 100];
        let before = fits.clone();
        assert_eq!(fit_row_to_budget(&mut fits), None);
        assert_eq!(fits, before);

        // 200 x WText(32768) is ~13 MB for a single row: the text/binary cells are narrowed to a
        // common capacity so that one row fits; fixed-size and narrow-text buffers keep theirs.
        let mut descs = vec![BufferDesc::WText { max_str_len: MAX_BOUNDED_TEXT }; 200];
        descs.push(BufferDesc::Binary { length: MAX_BOUNDED_BINARY });
        descs.push(BufferDesc::WText { max_str_len: 10 });
        descs.push(BufferDesc::I64 { nullable: true });
        descs.push(BufferDesc::Text { max_str_len: 41 });
        assert!(row_bytes(&descs) > FETCH_BUFFER_BUDGET);
        let cap = fit_row_to_budget(&mut descs).expect("narrowed");
        assert!((MIN_CELL_BYTES..MAX_BOUNDED_TEXT * 2).contains(&cap));
        assert!(row_bytes(&descs) <= FETCH_BUFFER_BUDGET);
        // `cap` is the largest capacity that fits: one more byte per cell would overshoot.
        let wider: Vec<BufferDesc> = descs
            .iter()
            .map(|d| match *d {
                BufferDesc::WText { max_str_len } if max_str_len > 10 => BufferDesc::WText { max_str_len: max_str_len + 1 },
                BufferDesc::Binary { length } => BufferDesc::Binary { length: length + 2 },
                other => other,
            })
            .collect();
        assert!(row_bytes(&wider) > FETCH_BUFFER_BUDGET);
        assert_eq!(descs[0], BufferDesc::WText { max_str_len: cap / 2 });
        assert_eq!(descs[200], BufferDesc::Binary { length: cap });
        assert_eq!(descs[201], BufferDesc::WText { max_str_len: 10 });
        assert_eq!(descs[202], BufferDesc::I64 { nullable: true });
        assert_eq!(descs[203], BufferDesc::Text { max_str_len: 41 });
        assert_eq!(rows_for_budget(DEFAULT_FETCH_ROWS, &descs), 1);

        // Absurdly many text columns: narrowing stops at the floor (and the fetch is one row).
        let mut huge = vec![BufferDesc::WText { max_str_len: MAX_UNBOUNDED_TEXT }; 20_000];
        assert_eq!(fit_row_to_budget(&mut huge), Some(MIN_CELL_BYTES));
        assert!(huge.iter().all(|d| *d == BufferDesc::WText { max_str_len: MIN_CELL_BYTES / 2 }));
        assert_eq!(rows_for_budget(DEFAULT_FETCH_ROWS, &huge), 1);

        // Only fixed-size columns: nothing can be narrowed.
        let mut ints = vec![BufferDesc::I64 { nullable: true }; 600_000];
        assert_eq!(fit_row_to_budget(&mut ints), None);
    }

    #[test]
    fn converts_odbc_dates_times() {
        let d = OdbcDate { year: 2024, month: 2, day: 29 };
        assert_eq!(date_days(&d), Some(days_from_date(chrono::NaiveDate::from_ymd_opt(2024, 2, 29).unwrap())));
        assert_eq!(date_days(&OdbcDate { year: 0, month: 0, day: 0 }), None);
        let t = OdbcTime { hour: 1, minute: 2, second: 3 };
        assert_eq!(time_micros(&t), 3_723_000_000);
        let ts = OdbcTimestamp { year: 1970, month: 1, day: 1, hour: 0, minute: 0, second: 1, fraction: 500_000_000 };
        assert_eq!(timestamp_micros(&ts), Some(1_500_000));
        assert_eq!(parse_time_text("12:34:56.789"), Some((12_i64 * 3600 + 34 * 60 + 56) * 1_000_000 + 789_000));
        assert_eq!(parse_time_text("12:34:56"), Some((12_i64 * 3600 + 34 * 60 + 56) * 1_000_000));
        assert_eq!(parse_time_text("garbage"), None);
    }

    #[test]
    fn decodes_utf16() {
        let mut s = String::new();
        let units: Vec<u16> = "한글 text".encode_utf16().collect();
        decode_utf16_into(&units, &mut s);
        assert_eq!(s, "한글 text");
        decode_utf16_into(&[0xD800], &mut s);
        assert_eq!(s, "\u{FFFD}");
    }

    #[test]
    fn renders_types_and_kinds() {
        assert_eq!(render_type("varchar", Some(12), Some(50), None), "varchar(50)");
        assert_eq!(render_type("varchar", Some(12), Some(i32::MAX as i64), None), "varchar(max)");
        assert_eq!(render_type("numeric", Some(2), Some(12), Some(2)), "numeric(12,2)");
        assert_eq!(render_type("int", Some(4), Some(10), Some(0)), "int");
        assert_eq!(render_type("datetime2", Some(93), Some(27), Some(7)), "datetime2");
        assert_eq!(render_type("", None, None, None), "unknown");
        assert_eq!(table_kind(Some("TABLE")), TableKind::Table);
        assert_eq!(table_kind(Some("VIEW")), TableKind::View);
        assert_eq!(table_kind(Some("SYSTEM TABLE")), TableKind::Table);
        assert_eq!(table_kind(Some("SYNONYM")), TableKind::Other);
        assert_eq!(table_kind(None), TableKind::Other);
    }

    #[test]
    fn picks_probe_queries_by_dbms() {
        assert!(probe_queries("Microsoft SQL Server")[0].contains("SERVERPROPERTY"));
        assert!(probe_queries("PostgreSQL")[0].contains("server_version"));
        assert!(probe_queries("MariaDB")[0].contains("VERSION()"));
        assert!(probe_queries("ACCESS").is_empty());
        assert_eq!(probe_queries("Something Else").len(), 2);
    }
}
