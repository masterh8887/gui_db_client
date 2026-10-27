//! MySQL / MariaDB driver (mysql_async 0.36, binary protocol streaming).
//!
//! * One `Conn` per [`MySqlDatabase`], kept in a `tokio::sync::Mutex<Option<Conn>>`. While an
//!   extraction streams, the connection is moved into the reader task and the slot is empty
//!   (catalog/monitor calls run on their own `bulkdb-ui` object, so they never collide).
//! * Extraction runs as a server-side prepared statement: `conn.prep(sql)` ->
//!   `conn.exec_iter(&stmt, ())` -> `result.stream::<Row>()` decoded row by row into
//!   [`ArrowBatcher`]; a batch is cut at 64K rows / 8 MB and sent through a tiny bounded
//!   channel to the pipeline. No row is ever retained. mysql_async always negotiates
//!   `CLIENT_MULTI_STATEMENTS` (it cannot be switched off), so the text protocol would execute
//!   `SELECT 1; DROP TABLE t` as two statements; `COM_STMT_PREPARE` accepts exactly one
//!   statement, which makes the server itself the last line of defence behind `sqlguard`.
//!   The extraction session is additionally `SET SESSION TRANSACTION READ ONLY`.
//! * EXPLAIN runs over the text protocol (not every server can prepare `EXPLAIN` /
//!   `ANALYZE`), so its SQL is first checked with [`has_statement_separator`] under the
//!   session's `sql_mode` lexing rules.
//! * Cancel: `KILL QUERY <connection id>` over a second short-lived connection, then the stream
//!   ends with `Cancelled` and the (half-read) connection is dropped.
//! * Session: `max_execution_time` (MySQL >= 5.7.8) / `max_statement_time` (MariaDB) and
//!   `net_read_timeout` / `net_write_timeout`.
//! * Probe: version, `innodb_buffer_pool_size` (RAM estimate x 1.33), `SHOW GRANTS` for write
//!   privileges, flavour detection (MySQL vs MariaDB).
//! * EXPLAIN: `EXPLAIN FORMAT=JSON` parsed into a [`PlanNode`] tree; `EXPLAIN ANALYZE`
//!   (MySQL 8.0.18+) / `ANALYZE FORMAT=JSON` (MariaDB) when `analyze = true`.
//!
//! mysql_async 0.36 does not send connection attributes, so `program_name` cannot be set from
//! the client. The extraction SQL is tagged with a leading `/* bulkdb-<job> */` comment instead,
//! which the monitor can find in `processlist.info` when no session id is known.
//!
//! TLS: the workspace builds mysql_async with its default features, which include no TLS
//! backend. mysql_async *panics* when TLS is requested without one, so the connect future is
//! guarded with `catch_unwind`; `SslMode::Prefer` falls back to a plain connection and
//! `Require` / `VerifyFull` report a clear `CONNECT` error until `native-tls-tls` is enabled.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::{Duration, Instant};

use arrow_schema::SchemaRef;
use async_trait::async_trait;
use futures::{FutureExt, StreamExt};
use mysql_async::consts::{ColumnFlags, ColumnType};
use mysql_async::prelude::Queryable;
use mysql_async::{Column, Conn, Opts, OptsBuilder, Params, Row, SslOpts, Value};
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio_util::sync::CancellationToken;

use super::arrow_batch::{schema_from, ArrowBatcher, Cell, ColKind};
use super::{Batch, ConnectParams, Database, Query, Source, SourceStream, SslMode};
use crate::error::{CoreError, Result};
use crate::types::{PlanNode, PlanSummary, ServerInfo, SessionOpts, TableRef};
use crate::{BATCH_BYTES, BATCH_ROWS};

/// Binary charset number: columns with this collation carry bytes, not text.
const BINARY_CHARSET: u16 = 63;
/// How long a `KILL QUERY` side connection may take (connect + kill).
const KILL_TIMEOUT: Duration = Duration::from_secs(10);
/// Depth of the internal reader -> pipeline channel (the pipeline has its own 8-deep channel).
const READER_CHANNEL_DEPTH: usize = 2;
/// Planner-only EXPLAIN limits (statement / metadata lock).
const EXPLAIN_STATEMENT_TIMEOUT_MS: u64 = 15_000;
const EXPLAIN_LOCK_TIMEOUT_S: u64 = 3;
/// Monitor ring size for the `Sending to client` ratio.
const WRITE_RING: usize = 10;
/// `ER_PARSE_ERROR`: SQL syntax error.
const ER_PARSE_ERROR: u16 = 1064;
/// `ER_UNKNOWN_SYSTEM_VARIABLE`.
const ER_UNKNOWN_SYSTEM_VARIABLE: u16 = 1193;

// ---------------------------------------------------------------------------------------------
// Server version / flavour
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flavor {
    MySql,
    MariaDb,
}

/// Parsed `VERSION()` string. MariaDB reports `5.5.5-10.6.12-MariaDB...` over the wire (the
/// `5.5.5-` prefix is a compatibility shim) and `10.6.12-MariaDB` from `VERSION()`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerVersion {
    pub flavor: Flavor,
    pub major: u16,
    pub minor: u16,
    pub patch: u16,
    pub raw: String,
}

impl ServerVersion {
    pub fn parse(raw: &str) -> ServerVersion {
        let lower = raw.to_ascii_lowercase();
        let flavor = if lower.contains("mariadb") { Flavor::MariaDb } else { Flavor::MySql };
        let mut s = raw.trim();
        if flavor == Flavor::MariaDb {
            if let Some(rest) = s.strip_prefix("5.5.5-") {
                s = rest;
            }
        }
        let mut nums = [0u16; 3];
        let mut i = 0;
        for part in s.split(|c: char| !c.is_ascii_digit()) {
            if i == 3 {
                break;
            }
            if part.is_empty() {
                // Stop at the first non-numeric token once we have started (e.g. "8.0.36-log").
                if i > 0 {
                    break;
                }
                continue;
            }
            nums[i] = part.parse().unwrap_or(0);
            i += 1;
        }
        ServerVersion { flavor, major: nums[0], minor: nums[1], patch: nums[2], raw: raw.to_string() }
    }

    pub fn at_least(&self, major: u16, minor: u16, patch: u16) -> bool {
        (self.major, self.minor, self.patch) >= (major, minor, patch)
    }

    /// `max_execution_time` exists since MySQL 5.7.8 (MariaDB uses `max_statement_time`).
    pub fn supports_max_execution_time(&self) -> bool {
        self.flavor == Flavor::MySql && self.at_least(5, 7, 8)
    }

    /// `EXPLAIN ANALYZE` exists since MySQL 8.0.18; MariaDB has `ANALYZE FORMAT=JSON`.
    pub fn supports_explain_analyze(&self) -> bool {
        match self.flavor {
            Flavor::MySql => self.at_least(8, 0, 18),
            Flavor::MariaDb => true,
        }
    }

    pub fn label(&self) -> &'static str {
        match self.flavor {
            Flavor::MySql => "MySQL",
            Flavor::MariaDb => "MariaDB",
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Database object
// ---------------------------------------------------------------------------------------------

pub struct MySqlDatabase {
    opts: Opts,
    conn: Arc<Mutex<Option<Conn>>>,
    session_id: u32,
    version: ServerVersion,
    application_name: String,
    database: Option<String>,
    user: Option<String>,
    host: String,
    port: u16,
    /// Per job ring of "server is sending to the client" samples (see `monitor/mysql.rs`).
    write_samples: std::sync::Mutex<HashMap<String, VecDeque<bool>>>,
}

impl std::fmt::Debug for MySqlDatabase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MySqlDatabase")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("database", &self.database)
            .field("session_id", &self.session_id)
            .field("version", &self.version.raw)
            .finish()
    }
}

/// Why a connect attempt failed, so `SslMode::Prefer` can decide whether to retry in clear.
enum ConnectFailure {
    /// TLS was requested and the failure is TLS related (server refused SSL, handshake error,
    /// or this build has no TLS backend).
    Tls(String),
    Other(CoreError),
}

impl ConnectFailure {
    fn into_core(self) -> CoreError {
        match self {
            ConnectFailure::Tls(m) => CoreError::Connect(format!("TLS: {m}")),
            ConnectFailure::Other(e) => e,
        }
    }
}

impl MySqlDatabase {
    /// Connect, honouring `params.ssl`, then `SET NAMES utf8mb4` and read the server version.
    pub async fn connect(params: &ConnectParams) -> Result<Self> {
        let timeout = params.connect_timeout;
        let (mut conn, opts) = match params.ssl {
            SslMode::Disable => {
                let o = build_opts(params, None);
                (connect_with_timeout(o.clone(), timeout).await.map_err(ConnectFailure::into_core)?, o)
            }
            SslMode::Prefer => {
                let tls = build_opts(params, Some(SslOpts::default().with_danger_accept_invalid_certs(true).with_danger_skip_domain_validation(true)));
                match connect_with_timeout(tls.clone(), timeout).await {
                    Ok(c) => (c, tls),
                    Err(ConnectFailure::Tls(reason)) => {
                        tracing::debug!("mysql {}:{}: TLS unavailable ({reason}); retrying without TLS", params.host, params.port);
                        let plain = build_opts(params, None);
                        (connect_with_timeout(plain.clone(), timeout).await.map_err(ConnectFailure::into_core)?, plain)
                    }
                    Err(other) => return Err(other.into_core()),
                }
            }
            SslMode::Require => {
                let o = build_opts(params, Some(SslOpts::default().with_danger_accept_invalid_certs(true).with_danger_skip_domain_validation(true)));
                (connect_with_timeout(o.clone(), timeout).await.map_err(ConnectFailure::into_core)?, o)
            }
            SslMode::VerifyFull => {
                let o = build_opts(params, Some(SslOpts::default()));
                (connect_with_timeout(o.clone(), timeout).await.map_err(ConnectFailure::into_core)?, o)
            }
        };

        conn.query_drop("SET NAMES utf8mb4").await.map_err(map_err)?;
        let raw: Option<String> = conn.query_first("SELECT VERSION()").await.map_err(map_err)?;
        let version = ServerVersion::parse(raw.as_deref().unwrap_or_default());
        let session_id = conn.id();
        tracing::debug!("mysql connected to {}:{} ({}, {}) as connection {}", params.host, params.port, version.label(), version.raw, session_id);

        Ok(MySqlDatabase {
            opts,
            conn: Arc::new(Mutex::new(Some(conn))),
            session_id,
            version,
            application_name: params.application_name.clone(),
            database: params.database.clone().filter(|d| !d.is_empty()),
            user: params.user.clone(),
            host: params.host.clone(),
            port: params.port,
            write_samples: std::sync::Mutex::new(HashMap::new()),
        })
    }

    pub fn version(&self) -> &ServerVersion {
        &self.version
    }

    pub fn flavor(&self) -> Flavor {
        self.version.flavor
    }

    /// Database named in the connection (`None` when the account connected without one).
    pub fn current_database(&self) -> Option<&str> {
        self.database.as_deref()
    }

    pub fn application_name(&self) -> &str {
        &self.application_name
    }

    /// Record one monitor sample for `job_id` and return the fraction of the last ten samples
    /// where the server was writing to the client.
    pub(crate) fn record_write_sample(&self, job_id: &str, writing: bool) -> f32 {
        let mut map = self.write_samples.lock().unwrap_or_else(|p| p.into_inner());
        let ring = map.entry(job_id.to_string()).or_default();
        ring.push_back(writing);
        while ring.len() > WRITE_RING {
            ring.pop_front();
        }
        let hits = ring.iter().filter(|w| **w).count();
        hits as f32 / ring.len() as f32
    }

    pub(crate) fn forget_write_samples(&self, job_id: &str) {
        self.write_samples.lock().unwrap_or_else(|p| p.into_inner()).remove(job_id);
    }

    // --- connection helpers ------------------------------------------------------------------

    /// Run `f` with the connection. Fails with `CONNECT` when the connection is streaming.
    async fn with_conn<T, F>(&self, f: F) -> Result<T>
    where
        F: for<'c> FnOnce(&'c mut Conn) -> futures::future::BoxFuture<'c, Result<T>> + Send,
        T: Send,
    {
        let mut guard = self.conn.lock().await;
        let conn = guard.as_mut().ok_or_else(|| CoreError::Connect("connection is busy with a running extraction".into()))?;
        f(conn).await
    }

    /// Text-protocol query returning all rows mapped through `f` (metadata only; never data).
    pub(crate) async fn query_map<T, F>(&self, sql: &str, f: F) -> Result<Vec<T>>
    where
        F: FnMut(Row) -> T + Send + 'static,
        T: Send + 'static,
    {
        let sql = sql.to_string();
        self.with_conn(move |c| Box::pin(async move { c.query_map(sql, f).await.map_err(map_err) })).await
    }

    /// Prepared query with parameters returning all rows mapped through `f` (metadata only).
    pub(crate) async fn exec_map<T, F, P>(&self, sql: &str, params: P, f: F) -> Result<Vec<T>>
    where
        F: FnMut(Row) -> T + Send + 'static,
        T: Send + 'static,
        P: Into<Params> + Send,
    {
        let sql = sql.to_string();
        let params: Params = params.into();
        self.with_conn(move |c| Box::pin(async move { c.exec_map(sql, params, f).await.map_err(map_err) })).await
    }

    /// First row of a text-protocol query.
    pub(crate) async fn query_first(&self, sql: &str) -> Result<Option<Row>> {
        let sql = sql.to_string();
        self.with_conn(move |c| Box::pin(async move { c.query_first(sql).await.map_err(map_err) })).await
    }

    /// First row of a prepared query.
    pub(crate) async fn exec_first<P>(&self, sql: &str, params: P) -> Result<Option<Row>>
    where
        P: Into<Params> + Send,
    {
        let sql = sql.to_string();
        let params: Params = params.into();
        self.with_conn(move |c| Box::pin(async move { c.exec_first(sql, params).await.map_err(map_err) })).await
    }

    /// `INVALID` unless `sql` is a single statement under this session's quoting rules. Text that
    /// is a single statement under every `sql_mode` passes without a round trip; otherwise the
    /// session's `sql_mode` decides how quotes and backslashes are lexed.
    pub(crate) async fn ensure_single_statement(&self, sql: &str) -> Result<()> {
        if !may_have_statement_separator(sql) {
            return Ok(());
        }
        let mode = match self.query_first("SELECT @@SESSION.sql_mode").await? {
            Some(row) => LexMode::from_sql_mode(&row_str(&row, 0).unwrap_or_default()),
            None => LexMode::default(),
        };
        if has_statement_separator(sql, mode) {
            return Err(CoreError::Invalid("the SQL must be a single statement: a ';' separates it into several, which MySQL would run one after the other".into()));
        }
        Ok(())
    }

    /// Statement without a result (`SET ...`, `KILL ...`).
    pub(crate) async fn query_drop(&self, sql: &str) -> Result<()> {
        let sql = sql.to_string();
        self.with_conn(move |c| Box::pin(async move { c.query_drop(sql).await.map_err(map_err) })).await
    }

    /// Cancellable `SELECT` returning one row: on cancel the statement is killed from a side
    /// connection and `Cancelled` is returned. Used by `exact_count`.
    pub(crate) async fn query_first_cancellable(&self, sql: &str, cancel: CancellationToken) -> Result<Option<Row>> {
        let sql = sql.to_string();
        let opts = self.opts.clone();
        let id = self.session_id;
        self.with_conn(move |c| {
            Box::pin(async move {
                let fut = c.query_first::<Row, _>(sql);
                tokio::pin!(fut);
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => {
                        if let Err(e) = kill_query(&opts, id).await {
                            tracing::warn!("mysql: KILL QUERY {id} failed: {e}");
                        }
                        // Let the server interrupt the statement so the connection stays clean.
                        let _ = tokio::time::timeout(KILL_TIMEOUT, &mut fut).await;
                        Err(CoreError::Cancelled)
                    }
                    r = &mut fut => r.map_err(map_err),
                }
            })
        })
        .await
    }

    // --- EXPLAIN helpers ---------------------------------------------------------------------

    /// Save, shorten and later restore the session timeouts around a planner-only EXPLAIN.
    async fn explain_with_short_timeouts(&self, explain_sql: &str) -> Result<Vec<String>> {
        let flavor = self.version.flavor;
        let supports_met = self.version.supports_max_execution_time();
        let sql = explain_sql.to_string();
        self.with_conn(move |c| {
            Box::pin(async move {
                let saved_lock: Option<u64> = c.query_first::<Row, _>("SELECT @@session.lock_wait_timeout").await.ok().flatten().and_then(|r| row_u64(&r, 0));
                let saved_stmt: Option<String> = match flavor {
                    Flavor::MariaDb => c.query_first::<Row, _>("SELECT @@session.max_statement_time").await.ok().flatten().and_then(|r| row_str(&r, 0)),
                    Flavor::MySql if supports_met => c.query_first::<Row, _>("SELECT @@session.max_execution_time").await.ok().flatten().and_then(|r| row_str(&r, 0)),
                    Flavor::MySql => None,
                };
                let _ = c.query_drop(format!("SET SESSION lock_wait_timeout = {EXPLAIN_LOCK_TIMEOUT_S}")).await;
                match flavor {
                    Flavor::MariaDb => {
                        let _ = c.query_drop(format!("SET SESSION max_statement_time = {}", EXPLAIN_STATEMENT_TIMEOUT_MS as f64 / 1000.0)).await;
                    }
                    Flavor::MySql if supports_met => {
                        let _ = c.query_drop(format!("SET SESSION max_execution_time = {EXPLAIN_STATEMENT_TIMEOUT_MS}")).await;
                    }
                    Flavor::MySql => {}
                }
                let out = c.query_map(sql, |row: Row| explain_row_text(&row)).await.map_err(map_err);
                if let Some(v) = saved_lock {
                    let _ = c.query_drop(format!("SET SESSION lock_wait_timeout = {v}")).await;
                }
                if let Some(v) = saved_stmt {
                    let stmt = match flavor {
                        Flavor::MariaDb => format!("SET SESSION max_statement_time = {v}"),
                        Flavor::MySql => format!("SET SESSION max_execution_time = {v}"),
                    };
                    let _ = c.query_drop(stmt).await;
                }
                out
            })
        })
        .await
    }
}

/// Build `Opts` for `params` with an optional TLS configuration.
fn build_opts(params: &ConnectParams, ssl: Option<SslOpts>) -> Opts {
    let builder = OptsBuilder::default()
        .ip_or_hostname(params.host.clone())
        .tcp_port(params.port)
        .user(params.user.clone())
        .pass(params.password.clone())
        .db_name(params.database.clone().filter(|d| !d.is_empty()))
        .tcp_keepalive(Some(10_000u32))
        .tcp_nodelay(true)
        .prefer_socket(false)
        .stmt_cache_size(0usize)
        .ssl_opts(ssl);
    Opts::from(builder)
}

/// `Conn::new` with a timeout and panic guard (see the module docs about TLS).
async fn connect_with_timeout(opts: Opts, timeout: Duration) -> std::result::Result<Conn, ConnectFailure> {
    let tls_requested = opts.ssl_opts().is_some();
    let host = opts.ip_or_hostname().to_string();
    let port = opts.tcp_port();
    let fut = AssertUnwindSafe(Conn::new(opts)).catch_unwind();
    match tokio::time::timeout(timeout, fut).await {
        Err(_) => Err(ConnectFailure::Other(CoreError::Connect(format!("{host}:{port}: connect timed out after {} s", timeout.as_secs())))),
        Ok(Err(panic)) => {
            let msg = panic_message(panic.as_ref());
            if tls_requested && msg.to_ascii_lowercase().contains("tls") {
                Err(ConnectFailure::Tls("TLS support is not compiled into this build of the MySQL driver".into()))
            } else {
                Err(ConnectFailure::Other(CoreError::Internal(format!("mysql driver panicked while connecting: {msg}"))))
            }
        }
        Ok(Ok(Err(e))) => {
            if tls_requested && is_tls_error(&e) {
                Err(ConnectFailure::Tls(e.to_string()))
            } else {
                Err(ConnectFailure::Other(map_err(e)))
            }
        }
        Ok(Ok(Ok(conn))) => Ok(conn),
    }
}

fn panic_message(p: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = p.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = p.downcast_ref::<String>() {
        s.clone()
    } else {
        "unknown panic".to_string()
    }
}

/// Errors that mean "TLS did not work" rather than "the server rejected us".
fn is_tls_error(e: &mysql_async::Error) -> bool {
    match e {
        mysql_async::Error::Driver(mysql_async::DriverError::NoClientSslFlagFromServer) => true,
        // A server that closes the socket after the SSL request shows up as an I/O error; a
        // handshake failure shows up as a TLS error. Both are worth a clear-text retry.
        mysql_async::Error::Io(_) => true,
        mysql_async::Error::Server(_) => false,
        other => other.to_string().to_ascii_lowercase().contains("tls"),
    }
}

/// `KILL QUERY <id>` over a fresh short-lived connection built from the same options.
pub(crate) async fn kill_query(opts: &Opts, id: u32) -> Result<()> {
    let opts = opts.clone();
    let work = async move {
        let mut side = Conn::new(opts).await.map_err(map_err)?;
        let r = side.query_drop(format!("KILL QUERY {id}")).await;
        let _ = side.disconnect().await;
        match r {
            Ok(()) => Ok(()),
            // 1094: unknown thread id -> the statement already finished.
            Err(mysql_async::Error::Server(ref s)) if s.code == 1094 => Ok(()),
            Err(e) => Err(map_err(e)),
        }
    };
    match tokio::time::timeout(KILL_TIMEOUT, work).await {
        Ok(r) => r,
        Err(_) => Err(CoreError::Timeout(format!("KILL QUERY {id} did not complete within {} s", KILL_TIMEOUT.as_secs()))),
    }
}

// ---------------------------------------------------------------------------------------------
// Error mapping
// ---------------------------------------------------------------------------------------------

/// Map a driver error onto `CoreError`. Server codes: 1317 interrupted -> `Cancelled`,
/// 3024 (`max_execution_time`) / 1969 (MariaDB `max_statement_time`) -> `Timeout`,
/// 1045 / 1044 / 1698 -> `Auth`, 2002 / 2003 / 2013 and I/O -> `Connect`, others -> `Query`.
pub fn map_err(e: mysql_async::Error) -> CoreError {
    use mysql_async::{DriverError, Error};
    match e {
        Error::Server(s) => match s.code {
            1317 => CoreError::Cancelled,
            3024 | 1969 => CoreError::Timeout(s.message),
            1045 | 1044 | 1698 => CoreError::Auth(format!("{} ({})", s.message, s.code)),
            2002 | 2003 | 2013 | 2006 => CoreError::Connect(format!("{} ({})", s.message, s.code)),
            code => CoreError::Query { code: Some(code.to_string()), msg: format!("{} (error {code}, SQLSTATE {})", s.message, s.state) },
        },
        Error::Io(io) => CoreError::Connect(io.to_string()),
        Error::Driver(d) => match d {
            DriverError::FromValue { .. } | DriverError::FromRow { .. } => CoreError::Internal(format!("mysql value conversion: {d}")),
            // Extraction is a prepared statement executed without parameters: a bare `?` or
            // `:name` in the SQL is a placeholder nobody binds.
            DriverError::StmtParamsMismatch { .. } | DriverError::MissingNamedParam { .. } | DriverError::MixedParams | DriverError::NamedParamsForPositionalQuery => {
                CoreError::Invalid(format!("the SQL contains placeholders (`?` or `:name`) but no parameters are bound: {d}"))
            }
            DriverError::ConnectionClosed | DriverError::PoolDisconnected | DriverError::PacketOutOfOrder | DriverError::UnexpectedPacket { .. } => {
                CoreError::Connect(d.to_string())
            }
            other => CoreError::Connect(other.to_string()),
        },
        Error::Url(u) => CoreError::Invalid(u.to_string()),
        Error::Other(o) => CoreError::Internal(o.to_string()),
    }
}

/// Server error code if `e` is one (used to ignore `unknown system variable` and friends).
fn server_code(e: &CoreError) -> Option<u16> {
    match e {
        CoreError::Query { code: Some(c), .. } => c.parse().ok(),
        _ => None,
    }
}

// ---------------------------------------------------------------------------------------------
// Value helpers (metadata rows)
// ---------------------------------------------------------------------------------------------

/// Render any value as text (NULL -> None).
pub(crate) fn value_str(v: Option<&Value>) -> Option<String> {
    match v? {
        Value::NULL => None,
        Value::Bytes(b) => Some(String::from_utf8_lossy(b).into_owned()),
        Value::Int(i) => Some(i.to_string()),
        Value::UInt(u) => Some(u.to_string()),
        Value::Float(f) => Some(f.to_string()),
        Value::Double(d) => Some(d.to_string()),
        Value::Date(y, m, d, h, i, s, us) => Some(format_datetime(*y, *m, *d, *h, *i, *s, *us)),
        Value::Time(neg, days, h, i, s, us) => Some(format_time(*neg, *days, *h, *i, *s, *us)),
    }
}

pub(crate) fn value_u64(v: Option<&Value>) -> Option<u64> {
    match v? {
        Value::Int(i) => u64::try_from(*i).ok(),
        Value::UInt(u) => Some(*u),
        Value::Float(f) if *f >= 0.0 => Some(*f as u64),
        Value::Double(d) if *d >= 0.0 => Some(*d as u64),
        Value::Bytes(b) => {
            let s = std::str::from_utf8(b).ok()?.trim();
            s.parse::<u64>().ok().or_else(|| s.parse::<f64>().ok().filter(|f| *f >= 0.0).map(|f| f as u64))
        }
        _ => None,
    }
}

pub(crate) fn row_str(row: &Row, i: usize) -> Option<String> {
    value_str(row.as_ref(i))
}

pub(crate) fn row_u64(row: &Row, i: usize) -> Option<u64> {
    value_u64(row.as_ref(i))
}

fn format_datetime(y: u16, m: u8, d: u8, h: u8, i: u8, s: u8, us: u32) -> String {
    if us > 0 {
        format!("{y:04}-{m:02}-{d:02} {h:02}:{i:02}:{s:02}.{us:06}")
    } else if h == 0 && i == 0 && s == 0 {
        format!("{y:04}-{m:02}-{d:02}")
    } else {
        format!("{y:04}-{m:02}-{d:02} {h:02}:{i:02}:{s:02}")
    }
}

fn format_time(neg: bool, days: u32, h: u8, i: u8, s: u8, us: u32) -> String {
    let hours = days * 24 + h as u32;
    let sign = if neg { "-" } else { "" };
    if us > 0 {
        format!("{sign}{hours:02}:{i:02}:{s:02}.{us:06}")
    } else {
        format!("{sign}{hours:02}:{i:02}:{s:02}")
    }
}

/// Format a binary-protocol `TIME` exactly like the text protocol does for a `TIME(fsp)`
/// column: `[-]HH:MM:SS` plus exactly `fsp` fractional digits (`TIME(3)` -> `-10:20:30.500`).
/// `time` is the `Value::Time` payload `(negative, days, hours, minutes, seconds, micros)`.
/// Writes into `out` (cleared first) so the per-value scratch buffer is reused.
fn format_time_fsp(out: &mut String, time: (bool, u32, u8, u8, u8, u32), fsp: u8) {
    use std::fmt::Write as _;
    let (neg, days, h, i, s, us) = time;
    out.clear();
    let hours = days * 24 + h as u32;
    let sign = if neg { "-" } else { "" };
    let _ = write!(out, "{sign}{hours:02}:{i:02}:{s:02}");
    let fsp = fsp.min(6);
    if fsp > 0 {
        // Microseconds are 6 digits; TIME(fsp) shows the leading `fsp` of them.
        let frac = us.min(999_999) / 10u32.pow(6 - fsp as u32);
        let _ = write!(out, ".{frac:0width$}", width = fsp as usize);
    }
}

/// Widen a binary-protocol `FLOAT` (an `f32`) the way the text protocol would: through its
/// shortest round-trip decimal form, so `1.1f32` becomes `1.1` and not `1.100000023841858`.
fn widen_f32(f: f32, scratch: &mut String) -> f64 {
    use std::fmt::Write as _;
    scratch.clear();
    let _ = write!(scratch, "{f}");
    scratch.parse::<f64>().unwrap_or(f as f64)
}

// ---------------------------------------------------------------------------------------------
// Column typing and value decoding
// ---------------------------------------------------------------------------------------------

/// How a column's value is decoded into a [`Cell`]. Every variant accepts both encodings:
/// text-protocol bytes (`Value::Bytes`) and the typed values of the binary protocol, which is
/// what extraction uses; both decode to the same cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueParser {
    /// Signed integer text (`TINY`..`LONGLONG`, `YEAR`).
    Int,
    /// `BIT(n)`: big-endian raw bytes -> integer.
    Bit,
    Float,
    /// Decimal text scaled into an `i128` with `scale` fractional digits.
    Decimal {
        scale: i8,
    },
    /// Anything textual; typed values are formatted.
    Text,
    Binary,
    /// `YYYY-MM-DD`; zero dates become NULL.
    Date,
    /// `YYYY-MM-DD HH:MM:SS[.ffffff]`; zero datetimes become NULL.
    DateTime,
    /// `TIME(fsp)` as text, `[-]HH:MM:SS[.f..]`. A typed (binary protocol) value is printed
    /// with exactly `fsp` fractional digits, as the text protocol would; `None` (fsp unknown,
    /// e.g. an expression) prints microseconds only when they are non-zero.
    Time {
        fsp: Option<u8>,
    },
}

/// Map a result-set column onto a BulkDB column kind and its decoder.
///
/// * integers -> `Int64` (`BIGINT UNSIGNED` -> `Utf8`, it does not fit an i64)
/// * `FLOAT` / `DOUBLE` -> `Float64`
/// * `DECIMAL` -> `Decimal128` when the precision fits 38 digits, else `Utf8`
/// * `DATE` -> `Date32`, `DATETIME` / `TIMESTAMP` -> naive microsecond timestamp, `TIME` -> `Utf8`
/// * `BIT` -> `Int64`
/// * strings / blobs with the binary charset (63: `BINARY`, `VARBINARY`, `BLOB`, `GEOMETRY`)
///   -> `Binary`, every other string, `JSON`, `ENUM`, `SET` -> `Utf8`
pub fn column_kind(col: &Column) -> (ColKind, ValueParser) {
    use ColumnType::*;
    let flags = col.flags();
    let unsigned = flags.contains(ColumnFlags::UNSIGNED_FLAG);
    let binary = col.character_set() == BINARY_CHARSET;
    match col.column_type() {
        MYSQL_TYPE_TINY | MYSQL_TYPE_SHORT | MYSQL_TYPE_INT24 | MYSQL_TYPE_LONG | MYSQL_TYPE_YEAR => (ColKind::Int64, ValueParser::Int),
        MYSQL_TYPE_LONGLONG => {
            if unsigned {
                (ColKind::Utf8, ValueParser::Text)
            } else {
                (ColKind::Int64, ValueParser::Int)
            }
        }
        MYSQL_TYPE_FLOAT | MYSQL_TYPE_DOUBLE => (ColKind::Float64, ValueParser::Float),
        MYSQL_TYPE_DECIMAL | MYSQL_TYPE_NEWDECIMAL => {
            let scale = col.decimals().min(38) as i64;
            // column_length = digits + sign (signed only) + decimal point (scale > 0 only).
            let mut precision = col.column_length() as i64 - if scale > 0 { 1 } else { 0 } - if unsigned { 0 } else { 1 };
            precision = precision.max(scale + 1).max(1);
            if precision > 38 {
                (ColKind::Utf8, ValueParser::Text)
            } else {
                (ColKind::Decimal { precision: precision as u8, scale: scale as i8 }, ValueParser::Decimal { scale: scale as i8 })
            }
        }
        MYSQL_TYPE_DATE | MYSQL_TYPE_NEWDATE => (ColKind::Date, ValueParser::Date),
        MYSQL_TYPE_DATETIME | MYSQL_TYPE_TIMESTAMP | MYSQL_TYPE_DATETIME2 | MYSQL_TYPE_TIMESTAMP2 => (ColKind::Timestamp, ValueParser::DateTime),
        MYSQL_TYPE_TIME | MYSQL_TYPE_TIME2 => {
            // `decimals` is the fractional-second precision (0..=6) for a TIME column; larger
            // values (31 = "not fixed") mean the precision is unknown.
            let fsp = col.decimals();
            (ColKind::Utf8, ValueParser::Time { fsp: (fsp <= 6).then_some(fsp) })
        }
        MYSQL_TYPE_BIT => (ColKind::Int64, ValueParser::Bit),
        MYSQL_TYPE_TINY_BLOB
        | MYSQL_TYPE_MEDIUM_BLOB
        | MYSQL_TYPE_LONG_BLOB
        | MYSQL_TYPE_BLOB
        | MYSQL_TYPE_VAR_STRING
        | MYSQL_TYPE_STRING
        | MYSQL_TYPE_VARCHAR
        | MYSQL_TYPE_GEOMETRY
        | MYSQL_TYPE_VECTOR => {
            if binary {
                (ColKind::Binary, ValueParser::Binary)
            } else {
                (ColKind::Utf8, ValueParser::Text)
            }
        }
        MYSQL_TYPE_JSON | MYSQL_TYPE_ENUM | MYSQL_TYPE_SET | MYSQL_TYPE_NULL | MYSQL_TYPE_UNKNOWN | MYSQL_TYPE_TYPED_ARRAY => (ColKind::Utf8, ValueParser::Text),
    }
}

impl ValueParser {
    /// Decode one value. `scratch` backs formatted text for typed (binary protocol) values.
    pub fn cell<'a>(self, v: Option<&'a Value>, scratch: &'a mut String, col: usize) -> Result<Cell<'a>> {
        let v = match v {
            None | Some(Value::NULL) => return Ok(Cell::Null),
            Some(v) => v,
        };
        let bad = |what: &str, b: &[u8]| CoreError::Internal(format!("column {col}: '{}' is not a valid {what}", String::from_utf8_lossy(b)));
        match self {
            ValueParser::Int => match v {
                Value::Int(i) => Ok(Cell::I64(*i)),
                Value::UInt(u) => i64::try_from(*u).map(Cell::I64).map_err(|_| CoreError::Internal(format!("column {col}: {u} does not fit an i64"))),
                Value::Bytes(b) => parse_i64(b).map(Cell::I64).ok_or_else(|| bad("integer", b)),
                Value::Float(f) => Ok(Cell::I64(*f as i64)),
                Value::Double(d) => Ok(Cell::I64(*d as i64)),
                other => Err(CoreError::Internal(format!("column {col}: unexpected {other:?} for an integer column"))),
            },
            ValueParser::Bit => match v {
                Value::Int(i) => Ok(Cell::I64(*i)),
                Value::UInt(u) => i64::try_from(*u).map(Cell::I64).map_err(|_| CoreError::Internal(format!("column {col}: BIT value {u} does not fit an i64"))),
                Value::Bytes(b) => {
                    parse_bit(b).map(Cell::I64).ok_or_else(|| CoreError::Internal(format!("column {col}: BIT value of {} bytes does not fit an i64", b.len())))
                }
                other => Err(CoreError::Internal(format!("column {col}: unexpected {other:?} for a BIT column"))),
            },
            ValueParser::Float => match v {
                Value::Float(f) => Ok(Cell::F64(widen_f32(*f, scratch))),
                Value::Double(d) => Ok(Cell::F64(*d)),
                Value::Int(i) => Ok(Cell::F64(*i as f64)),
                Value::UInt(u) => Ok(Cell::F64(*u as f64)),
                Value::Bytes(b) => std::str::from_utf8(b).ok().and_then(|s| s.trim().parse::<f64>().ok()).map(Cell::F64).ok_or_else(|| bad("number", b)),
                other => Err(CoreError::Internal(format!("column {col}: unexpected {other:?} for a float column"))),
            },
            ValueParser::Decimal { scale } => match v {
                Value::Bytes(b) => parse_decimal(b, scale).map(Cell::Decimal).ok_or_else(|| bad("decimal", b)),
                Value::Int(i) => scale_int(*i as i128, scale).map(Cell::Decimal).ok_or_else(|| CoreError::Internal(format!("column {col}: decimal overflow"))),
                Value::UInt(u) => scale_int(*u as i128, scale).map(Cell::Decimal).ok_or_else(|| CoreError::Internal(format!("column {col}: decimal overflow"))),
                Value::Double(d) => {
                    scratch.clear();
                    use std::fmt::Write as _;
                    let _ = write!(scratch, "{:.*}", scale.max(0) as usize, d);
                    parse_decimal(scratch.as_bytes(), scale).map(Cell::Decimal).ok_or_else(|| CoreError::Internal(format!("column {col}: decimal overflow")))
                }
                Value::Float(f) => {
                    scratch.clear();
                    use std::fmt::Write as _;
                    let _ = write!(scratch, "{:.*}", scale.max(0) as usize, f);
                    parse_decimal(scratch.as_bytes(), scale).map(Cell::Decimal).ok_or_else(|| CoreError::Internal(format!("column {col}: decimal overflow")))
                }
                other => Err(CoreError::Internal(format!("column {col}: unexpected {other:?} for a decimal column"))),
            },
            ValueParser::Text => match v {
                Value::Bytes(b) => Ok(Cell::Bytes(b)),
                Value::Int(i) => Ok(Cell::I64(*i)),
                Value::UInt(u) => {
                    scratch.clear();
                    scratch.push_str(&u.to_string());
                    Ok(Cell::Str(scratch.as_str()))
                }
                Value::Float(f) => Ok(Cell::F64(widen_f32(*f, scratch))),
                Value::Double(d) => Ok(Cell::F64(*d)),
                Value::Date(y, m, d, h, i, s, us) => {
                    *scratch = format_datetime(*y, *m, *d, *h, *i, *s, *us);
                    Ok(Cell::Str(scratch.as_str()))
                }
                Value::Time(neg, days, h, i, s, us) => {
                    *scratch = format_time(*neg, *days, *h, *i, *s, *us);
                    Ok(Cell::Str(scratch.as_str()))
                }
                Value::NULL => Ok(Cell::Null),
            },
            ValueParser::Binary => match v {
                Value::Bytes(b) => Ok(Cell::Bytes(b)),
                other => {
                    *scratch = value_str(Some(other)).unwrap_or_default();
                    Ok(Cell::Str(scratch.as_str()))
                }
            },
            ValueParser::Date => match v {
                Value::Bytes(b) => parse_date_days(b).map(|d| d.map(Cell::Date).unwrap_or(Cell::Null)).ok_or_else(|| bad("date", b)),
                Value::Date(y, m, d, ..) => Ok(ymd_to_days(*y as i32, *m as u32, *d as u32).map(Cell::Date).unwrap_or(Cell::Null)),
                other => Err(CoreError::Internal(format!("column {col}: unexpected {other:?} for a date column"))),
            },
            ValueParser::DateTime => match v {
                Value::Bytes(b) => parse_datetime_micros(b).map(|t| t.map(Cell::Timestamp).unwrap_or(Cell::Null)).ok_or_else(|| bad("datetime", b)),
                Value::Date(y, m, d, h, i, s, us) => {
                    Ok(ymdhms_to_micros(*y as i32, *m as u32, *d as u32, *h as u32, *i as u32, *s as u32, *us).map(Cell::Timestamp).unwrap_or(Cell::Null))
                }
                other => Err(CoreError::Internal(format!("column {col}: unexpected {other:?} for a datetime column"))),
            },
            ValueParser::Time { fsp } => match v {
                Value::Bytes(b) => Ok(Cell::Bytes(b)),
                Value::Time(neg, days, h, i, s, us) => {
                    match fsp {
                        Some(fsp) => format_time_fsp(scratch, (*neg, *days, *h, *i, *s, *us), fsp),
                        None => *scratch = format_time(*neg, *days, *h, *i, *s, *us),
                    }
                    Ok(Cell::Str(scratch.as_str()))
                }
                other => {
                    *scratch = value_str(Some(other)).unwrap_or_default();
                    Ok(Cell::Str(scratch.as_str()))
                }
            },
        }
    }
}

fn parse_i64(b: &[u8]) -> Option<i64> {
    std::str::from_utf8(b).ok()?.trim().parse::<i64>().ok()
}

/// `BIT(n)` arrives as up to 8 big-endian bytes.
fn parse_bit(b: &[u8]) -> Option<i64> {
    if b.len() > 8 {
        return None;
    }
    let mut v: u64 = 0;
    for byte in b {
        v = (v << 8) | *byte as u64;
    }
    i64::try_from(v).ok()
}

fn scale_int(v: i128, scale: i8) -> Option<i128> {
    v.checked_mul(10i128.checked_pow(scale.max(0) as u32)?)
}

/// Parse a decimal string (`-123.4567`, `.5`, `12`) into an integer scaled by `10^scale`.
/// Extra fractional digits are rounded half away from zero (MySQL semantics); missing ones are
/// padded with zeros. Returns `None` on malformed input or i128 overflow.
pub fn parse_decimal(b: &[u8], scale: i8) -> Option<i128> {
    let s = std::str::from_utf8(b).ok()?.trim();
    if s.is_empty() {
        return None;
    }
    let (neg, digits) = match s.as_bytes()[0] {
        b'-' => (true, &s[1..]),
        b'+' => (false, &s[1..]),
        _ => (false, s),
    };
    let (int_part, frac_part) = match digits.find('.') {
        Some(p) => (&digits[..p], &digits[p + 1..]),
        None => (digits, ""),
    };
    if int_part.is_empty() && frac_part.is_empty() {
        return None;
    }
    if !int_part.bytes().all(|c| c.is_ascii_digit()) || !frac_part.bytes().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let scale = scale.max(0) as usize;
    let mut v: i128 = 0;
    for c in int_part.bytes() {
        v = v.checked_mul(10)?.checked_add((c - b'0') as i128)?;
    }
    let frac = frac_part.as_bytes();
    for i in 0..scale {
        let d = frac.get(i).map(|c| (c - b'0') as i128).unwrap_or(0);
        v = v.checked_mul(10)?.checked_add(d)?;
    }
    if frac.len() > scale && frac[scale] >= b'5' {
        v = v.checked_add(1)?;
    }
    Some(if neg { -v } else { v })
}

fn ymd_to_days(y: i32, m: u32, d: u32) -> Option<i32> {
    if y == 0 && m == 0 && d == 0 {
        return None;
    }
    let date = chrono::NaiveDate::from_ymd_opt(y, m, d)?;
    Some(super::arrow_batch::days_from_date(date))
}

fn ymdhms_to_micros(y: i32, m: u32, d: u32, h: u32, i: u32, s: u32, us: u32) -> Option<i64> {
    if y == 0 && m == 0 && d == 0 {
        return None;
    }
    let date = chrono::NaiveDate::from_ymd_opt(y, m, d)?;
    let time = chrono::NaiveTime::from_hms_micro_opt(h, i, s, us)?;
    Some(super::arrow_batch::micros_from_naive(date.and_time(time)))
}

fn split_ymd(b: &[u8]) -> Option<(i32, u32, u32)> {
    if b.len() < 10 || b[4] != b'-' || b[7] != b'-' {
        return None;
    }
    let num = |s: &[u8]| -> Option<u32> {
        if s.is_empty() || !s.iter().all(|c| c.is_ascii_digit()) {
            return None;
        }
        std::str::from_utf8(s).ok()?.parse().ok()
    };
    Some((num(&b[0..4])? as i32, num(&b[5..7])?, num(&b[8..10])?))
}

/// `YYYY-MM-DD` -> days since epoch. `Ok(None)` for the zero date (and dates that do not exist,
/// such as `2024-02-30` or `2024-00-00`, which MySQL stores under `ALLOW_INVALID_DATES`).
/// `None` when the text is not a date at all.
pub fn parse_date_days(b: &[u8]) -> Option<Option<i32>> {
    let s = trim_ascii(b);
    if s.len() != 10 {
        return None;
    }
    let (y, m, d) = split_ymd(s)?;
    if y == 0 && m == 0 && d == 0 {
        return Some(None);
    }
    Some(ymd_to_days(y, m, d))
}

/// `YYYY-MM-DD HH:MM:SS[.ffffff]` (or a bare date) -> microseconds since epoch, naive.
/// `Ok(None)` for zero datetimes; `None` when malformed.
pub fn parse_datetime_micros(b: &[u8]) -> Option<Option<i64>> {
    let s = trim_ascii(b);
    if s.len() < 10 {
        return None;
    }
    let (y, m, d) = split_ymd(s)?;
    let (mut h, mut mi, mut sec, mut us) = (0u32, 0u32, 0u32, 0u32);
    if s.len() > 10 {
        let rest = &s[10..];
        if rest.len() < 9 || (rest[0] != b' ' && rest[0] != b'T') || rest[3] != b':' || rest[6] != b':' {
            return None;
        }
        let num = |x: &[u8]| -> Option<u32> {
            if !x.iter().all(|c| c.is_ascii_digit()) {
                return None;
            }
            std::str::from_utf8(x).ok()?.parse().ok()
        };
        h = num(&rest[1..3])?;
        mi = num(&rest[4..6])?;
        sec = num(&rest[7..9])?;
        if rest.len() > 9 {
            if rest[9] != b'.' {
                return None;
            }
            let frac = &rest[10..];
            if frac.is_empty() || frac.len() > 9 || !frac.iter().all(|c| c.is_ascii_digit()) {
                return None;
            }
            let mut micros = 0u32;
            for i in 0..6 {
                micros = micros * 10 + frac.get(i).map(|c| (c - b'0') as u32).unwrap_or(0);
            }
            us = micros;
        }
    }
    if y == 0 && m == 0 && d == 0 {
        return Some(None);
    }
    Some(ymdhms_to_micros(y, m, d, h, mi, sec, us))
}

fn trim_ascii(b: &[u8]) -> &[u8] {
    let start = b.iter().position(|c| !c.is_ascii_whitespace()).unwrap_or(b.len());
    let end = b.iter().rposition(|c| !c.is_ascii_whitespace()).map(|p| p + 1).unwrap_or(start);
    &b[start..end.max(start)]
}

// ---------------------------------------------------------------------------------------------
// SQL helpers
// ---------------------------------------------------------------------------------------------

/// Remove surrounding whitespace and trailing semicolons. A terminator is not part of a
/// statement: `COM_STMT_PREPARE` (extraction) rejects it as a syntax error, and wrapping
/// `SELECT ...;` in `EXPLAIN` would leave an empty second statement behind. This does not make
/// multi-statement text safe; see [`has_statement_separator`] and the module docs.
pub fn trim_statement(sql: &str) -> &str {
    sql.trim().trim_end_matches(|c: char| c == ';' || c.is_whitespace())
}

/// The `sql_mode` switches that change how MySQL tokenises quotes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LexMode {
    /// Backslash escapes the next character inside string literals (`NO_BACKSLASH_ESCAPES`
    /// off, the server default).
    pub backslash_escapes: bool,
    /// `"` quotes identifiers instead of strings (`ANSI_QUOTES` on).
    pub ansi_quotes: bool,
}

impl LexMode {
    /// Lexing rules for a session `sql_mode` value (comma-separated, case-insensitive).
    pub fn from_sql_mode(sql_mode: &str) -> Self {
        let has = |flag: &str| sql_mode.split(',').any(|m| m.trim().eq_ignore_ascii_case(flag));
        // `ANSI` is a combination mode that includes ANSI_QUOTES.
        LexMode { backslash_escapes: !has("NO_BACKSLASH_ESCAPES"), ansi_quotes: has("ANSI_QUOTES") || has("ANSI") }
    }
}

impl Default for LexMode {
    /// The server default: backslash escapes on, `"` quotes strings.
    fn default() -> Self {
        LexMode { backslash_escapes: true, ansi_quotes: false }
    }
}

/// `true` when `sql` contains a statement separator: a `;` outside string literals, quoted
/// identifiers and comments, lexed the way MySQL / MariaDB do under `mode`.
///
/// * `'...'` and (without `ANSI_QUOTES`) `"..."` are strings: a doubled quote, and with
///   backslash escapes `\x`, do not end them.
/// * `` `...` `` and (with `ANSI_QUOTES`) `"..."` are identifiers: only a doubled quote escapes.
/// * `# ...` and `-- ...` (the dashes followed by whitespace, a control character or the end)
///   run to the end of the line; `/* ... */` does not nest.
/// * Executable comments `/*! ... */`, `/*!50700 ... */` and MariaDB's `/*M! ... */` are code to
///   the server, so their content is scanned as code (conservative: a `;` there counts even when
///   the version gate would skip it).
///
/// Run it on [`trim_statement`] output: a trailing terminator is a separator too.
pub fn has_statement_separator(sql: &str, mode: LexMode) -> bool {
    let b = sql.as_bytes();
    let n = b.len();
    let mut i = 0;
    // Inside an executable comment: its closing `*/` is skipped like whitespace.
    let mut in_exec_comment = false;
    while i < n {
        match b[i] {
            b';' => return true,
            q @ (b'\'' | b'"' | b'`') => {
                let backslash = mode.backslash_escapes && (q == b'\'' || (q == b'"' && !mode.ansi_quotes));
                i += 1;
                while i < n {
                    if backslash && b[i] == b'\\' {
                        i += 2;
                        continue;
                    }
                    if b[i] == q {
                        if i + 1 < n && b[i + 1] == q {
                            i += 2;
                            continue;
                        }
                        break;
                    }
                    i += 1;
                }
                // Past the closing quote (or the end of an unterminated literal).
                i += 1;
            }
            b'#' => i = skip_line(b, i),
            b'-' if b.get(i + 1) == Some(&b'-') && b.get(i + 2).is_none_or(|c| c.is_ascii_whitespace() || c.is_ascii_control()) => i = skip_line(b, i),
            b'/' if b.get(i + 1) == Some(&b'*') => {
                let exec = match b.get(i + 2) {
                    Some(b'!') => Some(3),
                    Some(b'M') if b.get(i + 3) == Some(&b'!') => Some(4),
                    _ => None,
                };
                match exec {
                    // Executable comment (a nested one is scanned as code too: conservative).
                    Some(opener) => {
                        in_exec_comment = true;
                        i += opener;
                    }
                    None => {
                        // Ordinary comment (also inside an executable one, where the server
                        // treats it as a comment as well): skip to the first `*/`.
                        i += 2;
                        while i < n && !(b[i] == b'*' && b.get(i + 1) == Some(&b'/')) {
                            i += 1;
                        }
                        i += 2;
                    }
                }
            }
            b'*' if in_exec_comment && b.get(i + 1) == Some(&b'/') => {
                in_exec_comment = false;
                i += 2;
            }
            _ => i += 1,
        }
    }
    false
}

/// [`has_statement_separator`] under every quoting mode: `false` means `sql` is a single
/// statement whatever the session's `sql_mode` is. Cheap pre-check before asking the server
/// for its `sql_mode`.
pub fn may_have_statement_separator(sql: &str) -> bool {
    [false, true]
        .into_iter()
        .any(|backslash_escapes| [false, true].into_iter().any(|ansi_quotes| has_statement_separator(sql, LexMode { backslash_escapes, ansi_quotes })))
}

/// Index just past the end of the line that starts a comment at `i`.
fn skip_line(b: &[u8], i: usize) -> usize {
    b[i..].iter().position(|&c| c == b'\n').map(|p| i + p + 1).unwrap_or(b.len())
}

/// Prefix the extraction SQL with `/* bulkdb-<job> */` so a monitor can find the session in
/// `processlist.info` without connection attributes.
pub fn tag_sql(application_name: &str, sql: &str) -> String {
    let tag = application_name.replace("*/", "").replace(['\r', '\n'], " ");
    format!("/* {} */ {}", tag.trim(), trim_statement(sql))
}

/// `LIKE` pattern that matches `tag_sql` output for `application_name`.
pub fn tag_like_pattern(application_name: &str) -> String {
    let tag = application_name.replace("*/", "");
    let escaped = tag.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_");
    format!("%/* {} */%", escaped.trim())
}

// ---------------------------------------------------------------------------------------------
// Source
// ---------------------------------------------------------------------------------------------

/// Everything the reader task needs besides the connection.
struct StreamCtx {
    opts: Opts,
    session_id: u32,
    slot: Arc<Mutex<Option<Conn>>>,
    cancel: CancellationToken,
    max_rows: usize,
}

enum Step {
    Row(Row),
    End,
    Fail(mysql_async::Error),
    Cancel,
}

/// Reader task body: runs the query, streams rows into batches and hands the connection back
/// to the slot when the result set was fully drained.
async fn stream_task(mut conn: Conn, sql: String, ctx: StreamCtx, schema_tx: oneshot::Sender<Result<SchemaRef>>, batch_tx: mpsc::Sender<Result<Batch>>) {
    let mut schema_tx = Some(schema_tx);
    let mut clean = true;
    let outcome = run_query(&mut conn, &sql, &ctx, &mut schema_tx, &batch_tx, &mut clean).await;
    match outcome {
        Ok(()) if clean => {
            *ctx.slot.lock().await = Some(conn);
        }
        Ok(()) => {
            // Every row was delivered but the connection could not be tidied up (closing the
            // prepared statement failed): do not hand a broken connection back.
            drop(conn);
        }
        Err(e) => {
            // Settle the connection before reporting, so a caller that reacts to the error by
            // using this database again (EXPLAIN, another stream) finds it back in its slot.
            if clean {
                // The server answered with an error before any row: the connection is usable.
                *ctx.slot.lock().await = Some(conn);
            } else {
                // Half-read result set: closing the socket makes the server abort the statement.
                drop(conn);
            }
            if let Some(tx) = schema_tx.take() {
                let _ = tx.send(Err(e));
            } else {
                let _ = batch_tx.send(Err(e)).await;
            }
        }
    }
}

async fn cancelled(ctx: &StreamCtx) -> CoreError {
    if let Err(e) = kill_query(&ctx.opts, ctx.session_id).await {
        tracing::warn!("mysql: KILL QUERY {} failed: {e}", ctx.session_id);
    }
    CoreError::Cancelled
}

/// Send a batch unless the job is cancelled or the pipeline went away.
async fn deliver(ctx: &StreamCtx, tx: &mpsc::Sender<Result<Batch>>, batch: Batch) -> Result<()> {
    tokio::select! {
        biased;
        _ = ctx.cancel.cancelled() => Err(cancelled(ctx).await),
        r = tx.send(Ok(batch)) => match r {
            Ok(()) => Ok(()),
            Err(_) => Err(cancelled(ctx).await),
        },
    }
}

/// Error for a statement the server refused to prepare. A syntax error on text that contains a
/// statement separator is reported as `INVALID` with a clear reason instead of the server's
/// "error near ..." message.
fn prepare_error(e: mysql_async::Error, sql: &str) -> CoreError {
    let err = map_err(e);
    if server_code(&err) == Some(ER_PARSE_ERROR) && may_have_statement_separator(sql) {
        return CoreError::Invalid(format!("the extraction SQL must be a single SELECT statement; MySQL refused to prepare it: {err}"));
    }
    err
}

/// Prepare `sql` and stream its result, then close the statement so none is left behind on the
/// server (`stmt_cache_size` is 0, so mysql_async never closes it on its own).
///
/// Prepared statements are the multi-statement guard: `COM_STMT_PREPARE` accepts exactly one
/// statement, whereas the text protocol runs every `;`-separated statement because mysql_async
/// always negotiates `CLIENT_MULTI_STATEMENTS`.
async fn run_query(
    conn: &mut Conn,
    sql: &str,
    ctx: &StreamCtx,
    schema_tx: &mut Option<oneshot::Sender<Result<SchemaRef>>>,
    batch_tx: &mpsc::Sender<Result<Batch>>,
    clean: &mut bool,
) -> Result<()> {
    *clean = true;
    let stmt = {
        let fut = conn.prep(sql);
        tokio::pin!(fut);
        tokio::select! {
            biased;
            _ = ctx.cancel.cancelled() => {
                *clean = false;
                return Err(cancelled(ctx).await);
            }
            r = &mut fut => r.map_err(|e| prepare_error(e, sql))?,
        }
    };
    let outcome = stream_prepared(conn, &stmt, ctx, schema_tx, batch_tx, clean).await;
    if *clean {
        // COM_STMT_CLOSE has no reply, so this only fails when the socket is gone.
        if let Err(e) = conn.close(stmt).await {
            tracing::warn!("mysql: closing the extraction statement failed: {e}");
            *clean = false;
        }
    }
    // A half-read connection is dropped by the caller; the server frees its statements then.
    outcome
}

/// Execute the prepared extraction statement and stream its single result set into batches.
async fn stream_prepared(
    conn: &mut Conn,
    stmt: &mysql_async::Statement,
    ctx: &StreamCtx,
    schema_tx: &mut Option<oneshot::Sender<Result<SchemaRef>>>,
    batch_tx: &mpsc::Sender<Result<Batch>>,
    clean: &mut bool,
) -> Result<()> {
    *clean = true;
    let mut result = {
        let fut = conn.exec_iter(stmt, ());
        tokio::pin!(fut);
        tokio::select! {
            biased;
            _ = ctx.cancel.cancelled() => {
                *clean = false;
                return Err(cancelled(ctx).await);
            }
            r = &mut fut => r.map_err(map_err)?,
        }
    };

    let columns = result.columns().unwrap_or_else(|| Arc::from(Vec::<Column>::new()));
    let mut named: Vec<(String, ColKind)> = Vec::with_capacity(columns.len());
    let mut kinds: Vec<ColKind> = Vec::with_capacity(columns.len());
    let mut parsers: Vec<ValueParser> = Vec::with_capacity(columns.len());
    for c in columns.iter() {
        let (kind, parser) = column_kind(c);
        named.push((c.name_str().into_owned(), kind));
        kinds.push(kind);
        parsers.push(parser);
    }
    let schema = schema_from(&named);
    if let Some(tx) = schema_tx.take() {
        let _ = tx.send(Ok(schema.clone()));
    }
    let mut batcher = ArrowBatcher::with_limits(schema, kinds, ctx.max_rows, BATCH_BYTES);
    let mut scratch = String::new();

    *clean = false;
    {
        let rows = match result.stream::<Row>().await.map_err(map_err)? {
            Some(s) => s,
            None => {
                *clean = true;
                return Ok(());
            }
        };
        let mut rows = std::pin::pin!(rows);
        loop {
            let step = tokio::select! {
                biased;
                _ = ctx.cancel.cancelled() => Step::Cancel,
                n = rows.next() => match n {
                    Some(Ok(r)) => Step::Row(r),
                    Some(Err(e)) => Step::Fail(e),
                    None => Step::End,
                },
            };
            match step {
                Step::Cancel => return Err(cancelled(ctx).await),
                Step::Fail(e) => return Err(map_err(e)),
                Step::End => break,
                Step::Row(row) => {
                    for (i, p) in parsers.iter().enumerate() {
                        let cell = p.cell(row.as_ref(i), &mut scratch, i)?;
                        batcher.push(cell)?;
                    }
                    if batcher.end_row()? {
                        if let Some(b) = batcher.flush()? {
                            deliver(ctx, batch_tx, Batch::Arrow(b)).await?;
                        }
                    }
                }
            }
        }
    }
    if let Some(b) = batcher.flush()? {
        deliver(ctx, batch_tx, Batch::Arrow(b)).await?;
    }
    // Read the final OK packet (and any further result set, which a single prepared SELECT
    // never has) so the connection is idle again.
    result.drop_result().await.map_err(map_err)?;
    *clean = true;
    Ok(())
}

#[async_trait]
impl Source for MySqlDatabase {
    async fn probe(&self) -> Result<ServerInfo> {
        let head = self.query_first("SELECT VERSION(), CURRENT_USER(), DATABASE()").await?;
        let (version_raw, user, database) = match head {
            Some(r) => (row_str(&r, 0).unwrap_or_default(), row_str(&r, 1), row_str(&r, 2)),
            None => (self.version.raw.clone(), None, None),
        };
        let version = ServerVersion::parse(&version_raw);

        let vars_sql = "SHOW VARIABLES WHERE Variable_name IN ('innodb_buffer_pool_size','max_execution_time','max_statement_time',\
             'net_read_timeout','net_write_timeout','max_allowed_packet','wait_timeout','version_comment','sql_mode',\
             'character_set_server','read_only','transaction_isolation','tx_isolation','innodb_page_size')";
        let mut settings = BTreeMap::new();
        for (k, v) in self.query_map(vars_sql, |r: Row| (row_str(&r, 0).unwrap_or_default(), row_str(&r, 1).unwrap_or_default())).await? {
            settings.insert(k, v);
        }

        let pool = settings.get("innodb_buffer_pool_size").and_then(|v| v.parse::<u64>().ok());
        let ram_bytes = pool.map(|p| p / 3 * 4 + (p % 3) * 4 / 3);
        let ram_source = pool.map(|_| "innodb_buffer_pool_size x 1.33".to_string());

        let mut notes = Vec::new();
        let can_write = match self.query_map("SHOW GRANTS", |r: Row| row_str(&r, 0).unwrap_or_default()).await {
            Ok(lines) => Some(grants_allow_write(&lines)),
            Err(e) => {
                notes.push(format!("SHOW GRANTS failed: {e}"));
                None
            }
        };
        notes.push(format!("{} {}.{}.{}", version.label(), version.major, version.minor, version.patch));
        if ram_bytes.is_none() {
            notes.push("innodb_buffer_pool_size not visible; server RAM unknown".into());
        }

        Ok(ServerInfo {
            driver: "mysql".into(),
            version: version_raw,
            user: user.or_else(|| self.user.clone()),
            database: database.or_else(|| self.database.clone()),
            ram_bytes,
            ram_source,
            settings,
            can_write,
            notes,
        })
    }

    async fn apply_session(&self, opts: &SessionOpts) -> Result<()> {
        // Every later transaction of this (extraction) session is read only, so even a
        // statement that slipped past sqlguard cannot change data. MySQL 5.6.5+ / MariaDB 10.0+
        // know the syntax; an older server answers with a parse error and keeps going without
        // it (the prepared-statement guard in `stream` still applies).
        match self.query_drop("SET SESSION TRANSACTION READ ONLY").await {
            Ok(()) => {}
            Err(e) if server_code(&e) == Some(ER_PARSE_ERROR) => {
                tracing::warn!("mysql {}: SET SESSION TRANSACTION READ ONLY is not supported; the extraction session stays read-write", self.version.raw)
            }
            Err(e) => return Err(e),
        }
        let mut stmts: Vec<String> = Vec::new();
        if let Some(ms) = opts.statement_timeout_ms {
            match self.version.flavor {
                Flavor::MariaDb => stmts.push(format!("SET SESSION max_statement_time = {}", ms as f64 / 1000.0)),
                Flavor::MySql if self.version.supports_max_execution_time() => stmts.push(format!("SET SESSION max_execution_time = {ms}")),
                Flavor::MySql => tracing::debug!("mysql {}: max_execution_time unsupported; statement timeout ignored", self.version.raw),
            }
        }
        let net = std::cmp::max(600, opts.statement_timeout_ms.unwrap_or(0) / 1000);
        stmts.push(format!("SET SESSION net_read_timeout = {net}"));
        stmts.push(format!("SET SESSION net_write_timeout = {net}"));
        for s in stmts {
            match self.query_drop(&s).await {
                Ok(()) => {}
                // Unknown system variable (older server / fork) -> unsupported, ignore.
                Err(e) if server_code(&e) == Some(ER_UNKNOWN_SYSTEM_VARIABLE) => tracing::debug!("mysql: ignoring unsupported session option: {s}"),
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    async fn stream(&self, q: &Query, opts: &SessionOpts, cancel: CancellationToken) -> Result<SourceStream> {
        // `Batch::RawCsv` is PostgreSQL only; every output comes out of this driver as Arrow.
        let conn = self.conn.lock().await.take().ok_or_else(|| CoreError::Connect("connection is busy with a running extraction".into()))?;
        let sql = tag_sql(&self.application_name, &q.sql);
        let max_rows = opts.fetch_batch_rows.map(|n| (n as usize).clamp(1, BATCH_ROWS)).unwrap_or(BATCH_ROWS);
        let ctx = StreamCtx { opts: self.opts.clone(), session_id: self.session_id, slot: self.conn.clone(), cancel, max_rows };
        let (schema_tx, schema_rx) = oneshot::channel();
        let (batch_tx, batch_rx) = mpsc::channel::<Result<Batch>>(READER_CHANNEL_DEPTH);
        tokio::spawn(stream_task(conn, sql, ctx, schema_tx, batch_tx));
        let schema = match schema_rx.await {
            Ok(r) => r?,
            Err(_) => return Err(CoreError::Internal("mysql reader task ended before reporting the result schema".into())),
        };
        let stream = futures::stream::unfold(batch_rx, |mut rx| async move { rx.recv().await.map(|b| (b, rx)) });
        Ok(SourceStream { schema: Some(schema), stream: Box::pin(stream) })
    }

    async fn explain(&self, sql: &str, analyze: bool) -> Result<Option<PlanSummary>> {
        let started = Instant::now();
        let body = trim_statement(sql);
        // EXPLAIN goes over the text protocol, where mysql_async's always-on multi-statement
        // support would run anything after a `;` for real (and EXPLAIN ANALYZE executes the
        // query itself). Refuse such text before it reaches the server.
        self.ensure_single_statement(body).await?;
        if analyze {
            if !self.version.supports_explain_analyze() {
                return Err(CoreError::Invalid(format!("EXPLAIN ANALYZE needs MySQL 8.0.18 or newer (server is {})", self.version.raw)));
            }
            return match self.version.flavor {
                Flavor::MySql => {
                    let lines = self.query_map(&format!("EXPLAIN ANALYZE {body}"), |r: Row| explain_row_text(&r)).await?;
                    let raw = lines.join("\n");
                    let estimated_rows = explain_analyze_root_rows(&raw);
                    Ok(Some(PlanSummary {
                        estimated_rows,
                        estimated_width: None,
                        total_cost: None,
                        estimated_bytes: None,
                        analyzed: true,
                        elapsed_ms: started.elapsed().as_millis() as u64,
                        warnings: vec![],
                        tables: vec![],
                        root: None,
                        raw,
                        raw_format: "text".into(),
                    }))
                }
                Flavor::MariaDb => {
                    let lines = self.query_map(&format!("ANALYZE FORMAT=JSON {body}"), |r: Row| explain_row_text(&r)).await?;
                    let raw = lines.join("\n");
                    let parsed = parse_explain_json(&raw)?;
                    Ok(Some(parsed.into_summary(raw, true, started.elapsed().as_millis() as u64)))
                }
            };
        }
        let lines = self.explain_with_short_timeouts(&format!("EXPLAIN FORMAT=JSON {body}")).await?;
        let raw = lines.join("\n");
        let parsed = parse_explain_json(&raw)?;
        Ok(Some(parsed.into_summary(raw, false, started.elapsed().as_millis() as u64)))
    }

    fn session_id(&self) -> Option<String> {
        Some(self.session_id.to_string())
    }

    fn driver_id(&self) -> &'static str {
        "mysql"
    }
}

impl Database for MySqlDatabase {}

/// The one text column of an EXPLAIN row (`EXPLAIN` / `Create Table` style outputs).
fn explain_row_text(row: &Row) -> String {
    row_str(row, 0).unwrap_or_default()
}

/// `SHOW GRANTS` lines -> can this account modify data or schema anywhere?
pub fn grants_allow_write(lines: &[String]) -> bool {
    const WRITE: [&str; 7] = ["ALL PRIVILEGES", "ALL", "INSERT", "UPDATE", "DELETE", "CREATE", "DROP"];
    lines.iter().any(|line| {
        let upper = line.to_ascii_uppercase();
        let Some(rest) = upper.strip_prefix("GRANT ") else { return false };
        let privs = match rest.find(" ON ") {
            Some(p) => &rest[..p],
            None => return false,
        };
        privs.split(',').map(|p| p.trim()).any(|p| {
            // "INSERT (col1, col2)" column privileges still count.
            let name = p.split('(').next().unwrap_or("").trim();
            WRITE.contains(&name) || name.starts_with("ALTER")
        })
    })
}

/// Top-line actual row count of `EXPLAIN ANALYZE` text (`... (actual time=.. rows=N loops=1)`).
fn explain_analyze_root_rows(raw: &str) -> Option<u64> {
    let first = raw.lines().next()?;
    let actual = first.find("actual time=")?;
    let tail = &first[actual..];
    let rows_at = tail.find("rows=")?;
    let num: String = tail[rows_at + 5..].chars().take_while(|c| c.is_ascii_digit()).collect();
    num.parse().ok()
}

// ---------------------------------------------------------------------------------------------
// EXPLAIN FORMAT=JSON parsing
// ---------------------------------------------------------------------------------------------

/// What [`parse_explain_json`] extracts from an `EXPLAIN FORMAT=JSON` document.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ExplainParsed {
    pub root: Option<PlanNode>,
    pub estimated_rows: Option<u64>,
    pub total_cost: Option<f64>,
    pub tables: Vec<TableRef>,
}

impl ExplainParsed {
    fn into_summary(self, raw: String, analyzed: bool, elapsed_ms: u64) -> PlanSummary {
        PlanSummary {
            estimated_rows: self.estimated_rows,
            estimated_width: None,
            total_cost: self.total_cost,
            estimated_bytes: None,
            analyzed,
            elapsed_ms,
            warnings: vec![],
            tables: self.tables,
            root: self.root,
            raw,
            raw_format: "json".into(),
        }
    }
}

/// Parse MySQL (and MariaDB) `EXPLAIN FORMAT=JSON` output into a [`PlanNode`] tree.
///
/// `query_block` / `ordering_operation` / `grouping_operation` / `duplicates_removal` /
/// `windowing` / `buffer_result` / `union_result` / `materialized_from_subquery` wrappers become
/// nodes named after the key; `nested_loop` arrays are flattened into the parent's children;
/// every `*subqueries` array becomes `subquery` nodes with `parent_relationship = SubPlan`;
/// `table` objects become nodes whose `node_type` is the access type (`ALL`, `ref`, `range`, ...).
pub fn parse_explain_json(raw: &str) -> Result<ExplainParsed> {
    let doc: serde_json::Value = serde_json::from_str(raw.trim()).map_err(|e| CoreError::query(format!("EXPLAIN returned invalid JSON: {e}")))?;
    let obj = doc.as_object().ok_or_else(|| CoreError::query("EXPLAIN JSON is not an object"))?;
    let mut tables: Vec<TableRef> = Vec::new();
    let root = match obj.get("query_block").and_then(|v| v.as_object()) {
        Some(qb) => walk_block("query_block", qb, &mut tables),
        None => walk_block("plan", obj, &mut tables),
    };
    let total_cost = root.total_cost;
    let estimated_rows = output_rows(&root).map(|r| r.max(0.0).round() as u64);
    Ok(ExplainParsed { root: Some(root), estimated_rows, total_cost, tables })
}

fn json_num(v: &serde_json::Value) -> Option<f64> {
    match v {
        serde_json::Value::Number(n) => n.as_f64(),
        serde_json::Value::String(s) => s.trim().parse::<f64>().ok(),
        _ => None,
    }
}

fn json_scalar_text(v: &serde_json::Value) -> Option<String> {
    match v {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        serde_json::Value::Bool(b) => Some(b.to_string()),
        serde_json::Value::Null => None,
        serde_json::Value::Array(items) => {
            let parts: Vec<String> = items.iter().filter_map(json_scalar_text).collect();
            if parts.is_empty() {
                None
            } else {
                Some(parts.join(", "))
            }
        }
        serde_json::Value::Object(_) => None,
    }
}

fn truncate(s: String, max: usize) -> String {
    if s.chars().count() <= max {
        s
    } else {
        let mut t: String = s.chars().take(max).collect();
        t.push('…');
        t
    }
}

/// Keys of a `table` object that are shown in `extra`.
const TABLE_EXTRA_KEYS: [&str; 14] = [
    "rows_examined_per_scan",
    "filtered",
    "key",
    "used_key_parts",
    "possible_keys",
    "key_length",
    "ref",
    "attached_condition",
    "index_condition",
    "using_index",
    "using_index_for_group_by",
    "partitions",
    "using_MRR",
    "message",
];

fn is_table_object(obj: &serde_json::Map<String, serde_json::Value>) -> bool {
    obj.contains_key("table_name") || obj.contains_key("access_type")
}

fn walk_table(obj: &serde_json::Map<String, serde_json::Value>, tables: &mut Vec<TableRef>) -> PlanNode {
    let mut node = PlanNode { node_type: obj.get("access_type").and_then(|v| v.as_str()).unwrap_or("table").to_string(), ..Default::default() };
    if let Some(name) = obj.get("table_name").and_then(|v| v.as_str()) {
        let r = TableRef { database: None, schema: None, name: name.to_string() };
        if !name.starts_with('<') && !tables.contains(&r) {
            tables.push(r.clone());
        }
        node.relation = Some(r);
    }
    node.plan_rows = obj.get("rows_produced_per_join").and_then(json_num).or_else(|| obj.get("rows").and_then(json_num));
    if let Some(cost) = obj.get("cost_info").and_then(|v| v.as_object()) {
        node.total_cost = cost.get("prefix_cost").and_then(json_num).or_else(|| {
            let read = cost.get("read_cost").and_then(json_num);
            let eval = cost.get("eval_cost").and_then(json_num);
            match (read, eval) {
                (Some(r), Some(e)) => Some(r + e),
                (Some(r), None) => Some(r),
                (None, e) => e,
            }
        });
    }
    node.actual_rows = obj.get("r_rows").and_then(json_num);
    node.actual_loops = obj.get("r_loops").and_then(json_num);
    node.actual_time_ms = obj.get("r_total_time_ms").and_then(json_num);
    for key in TABLE_EXTRA_KEYS {
        if let Some(text) = obj.get(key).and_then(json_scalar_text) {
            node.extra.insert(key.to_string(), truncate(text, 500));
        }
    }
    for (k, v) in obj {
        match v {
            serde_json::Value::Object(sub) if k == "materialized_from_subquery" => {
                let mut child = walk_block(k, sub, tables);
                child.parent_relationship = Some("SubPlan".into());
                node.children.push(child);
            }
            serde_json::Value::Array(items) if k.ends_with("subqueries") => {
                for item in items.iter().filter_map(|i| i.as_object()) {
                    node.children.push(walk_subquery(item, tables));
                }
            }
            _ => {}
        }
    }
    node
}

fn walk_subquery(item: &serde_json::Map<String, serde_json::Value>, tables: &mut Vec<TableRef>) -> PlanNode {
    let mut sub = PlanNode { node_type: "subquery".into(), parent_relationship: Some("SubPlan".into()), ..Default::default() };
    for flag in ["dependent", "cacheable", "using_temporary_table"] {
        if let Some(text) = item.get(flag).and_then(json_scalar_text) {
            sub.extra.insert(flag.to_string(), text);
        }
    }
    if let Some(qb) = item.get("query_block").and_then(|v| v.as_object()) {
        let child = walk_block("query_block", qb, tables);
        sub.total_cost = child.total_cost;
        sub.plan_rows = output_rows(&child);
        sub.children.push(child);
    } else if is_table_object(item) {
        sub.children.push(walk_table(item, tables));
    } else {
        sub.children.push(walk_block("subquery", item, tables));
    }
    sub
}

fn walk_block(name: &str, obj: &serde_json::Map<String, serde_json::Value>, tables: &mut Vec<TableRef>) -> PlanNode {
    if is_table_object(obj) {
        return walk_table(obj, tables);
    }
    let mut node = PlanNode { node_type: name.to_string(), ..Default::default() };
    for (k, v) in obj {
        match (k.as_str(), v) {
            ("table", serde_json::Value::Object(t)) => node.children.push(walk_table(t, tables)),
            ("nested_loop", serde_json::Value::Array(items)) => {
                for item in items.iter().filter_map(|i| i.as_object()) {
                    for (k2, v2) in item {
                        if let serde_json::Value::Object(sub) = v2 {
                            node.children.push(walk_block(k2, sub, tables));
                        }
                    }
                }
            }
            ("cost_info", serde_json::Value::Object(cost)) => {
                node.total_cost =
                    cost.get("query_cost").and_then(json_num).or_else(|| cost.get("prefix_cost").and_then(json_num)).or_else(|| cost.get("sort_cost").and_then(json_num));
            }
            ("query_specifications", serde_json::Value::Array(items)) => {
                for item in items.iter().filter_map(|i| i.as_object()) {
                    match item.get("query_block").and_then(|v| v.as_object()) {
                        Some(qb) => {
                            let mut child = walk_block("query_block", qb, tables);
                            if let Some(text) = item.get("dependent").and_then(json_scalar_text) {
                                child.extra.insert("dependent".into(), text);
                            }
                            node.children.push(child);
                        }
                        None => node.children.push(walk_block("query_specification", item, tables)),
                    }
                }
            }
            (key, serde_json::Value::Array(items)) if key.ends_with("subqueries") => {
                for item in items.iter().filter_map(|i| i.as_object()) {
                    node.children.push(walk_subquery(item, tables));
                }
            }
            ("r_rows", v) => node.actual_rows = json_num(v),
            ("r_loops", v) => node.actual_loops = json_num(v),
            ("r_total_time_ms", v) => node.actual_time_ms = json_num(v),
            (_, serde_json::Value::Object(sub)) => node.children.push(walk_block(k, sub, tables)),
            (_, serde_json::Value::Array(items)) => {
                if items.iter().all(|i| !i.is_object()) {
                    if let Some(text) = json_scalar_text(v) {
                        node.extra.insert(k.clone(), truncate(text, 500));
                    }
                } else {
                    for item in items.iter().filter_map(|i| i.as_object()) {
                        node.children.push(walk_block(k, item, tables));
                    }
                }
            }
            (_, scalar) => {
                if let Some(text) = json_scalar_text(scalar) {
                    node.extra.insert(k.clone(), truncate(text, 500));
                }
            }
        }
    }
    node
}

/// Rows a node produces: its own estimate, else the estimate of its last non-subquery child
/// (the last table of a nested loop is the join output).
fn output_rows(node: &PlanNode) -> Option<f64> {
    if let Some(r) = node.plan_rows {
        return Some(r);
    }
    node.children.iter().rev().filter(|c| c.parent_relationship.as_deref() != Some("SubPlan")).find_map(output_rows)
}

// ---------------------------------------------------------------------------------------------
// Tests (no server needed)
// ---------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use mysql_async::ServerError;

    fn col(ty: ColumnType) -> Column {
        Column::new(ty).with_name(b"c")
    }

    #[test]
    fn version_parsing() {
        let v = ServerVersion::parse("8.0.36");
        assert_eq!((v.flavor, v.major, v.minor, v.patch), (Flavor::MySql, 8, 0, 36));
        assert!(v.supports_max_execution_time());
        assert!(v.supports_explain_analyze());
        let v = ServerVersion::parse("5.7.44-log");
        assert!(v.supports_max_execution_time());
        assert!(!v.supports_explain_analyze());
        let v = ServerVersion::parse("5.6.51");
        assert!(!v.supports_max_execution_time());
        let v = ServerVersion::parse("5.5.5-10.6.12-MariaDB-1:10.6.12+maria~ubu2004");
        assert_eq!((v.flavor, v.major, v.minor, v.patch), (Flavor::MariaDb, 10, 6, 12));
        assert!(!v.supports_max_execution_time());
        let v = ServerVersion::parse("10.11.6-MariaDB");
        assert_eq!((v.major, v.minor, v.patch), (10, 11, 6));
        let v = ServerVersion::parse("8.0.18-percona");
        assert_eq!(v.flavor, Flavor::MySql);
        assert!(v.supports_explain_analyze());
    }

    #[test]
    fn column_type_mapping() {
        assert_eq!(column_kind(&col(ColumnType::MYSQL_TYPE_TINY)), (ColKind::Int64, ValueParser::Int));
        assert_eq!(column_kind(&col(ColumnType::MYSQL_TYPE_SHORT)).0, ColKind::Int64);
        assert_eq!(column_kind(&col(ColumnType::MYSQL_TYPE_INT24)).0, ColKind::Int64);
        assert_eq!(column_kind(&col(ColumnType::MYSQL_TYPE_LONG)).0, ColKind::Int64);
        assert_eq!(column_kind(&col(ColumnType::MYSQL_TYPE_YEAR)).0, ColKind::Int64);
        assert_eq!(column_kind(&col(ColumnType::MYSQL_TYPE_LONGLONG)).0, ColKind::Int64);
        let ubig = col(ColumnType::MYSQL_TYPE_LONGLONG).with_flags(ColumnFlags::UNSIGNED_FLAG);
        assert_eq!(column_kind(&ubig), (ColKind::Utf8, ValueParser::Text));
        let uint = col(ColumnType::MYSQL_TYPE_LONG).with_flags(ColumnFlags::UNSIGNED_FLAG);
        assert_eq!(column_kind(&uint).0, ColKind::Int64);
        assert_eq!(column_kind(&col(ColumnType::MYSQL_TYPE_FLOAT)), (ColKind::Float64, ValueParser::Float));
        assert_eq!(column_kind(&col(ColumnType::MYSQL_TYPE_DOUBLE)).0, ColKind::Float64);

        // DECIMAL(10,2) signed: column_length = 12.
        let dec = col(ColumnType::MYSQL_TYPE_NEWDECIMAL).with_column_length(12).with_decimals(2);
        assert_eq!(column_kind(&dec), (ColKind::Decimal { precision: 10, scale: 2 }, ValueParser::Decimal { scale: 2 }));
        // DECIMAL(10,2) UNSIGNED: column_length = 11.
        let dec = col(ColumnType::MYSQL_TYPE_NEWDECIMAL).with_column_length(11).with_decimals(2).with_flags(ColumnFlags::UNSIGNED_FLAG);
        assert_eq!(column_kind(&dec).0, ColKind::Decimal { precision: 10, scale: 2 });
        // DECIMAL(10,0) signed: column_length = 11.
        let dec = col(ColumnType::MYSQL_TYPE_DECIMAL).with_column_length(11).with_decimals(0);
        assert_eq!(column_kind(&dec).0, ColKind::Decimal { precision: 10, scale: 0 });
        // DECIMAL(65,30): too wide for Decimal128 -> text.
        let dec = col(ColumnType::MYSQL_TYPE_NEWDECIMAL).with_column_length(67).with_decimals(30);
        assert_eq!(column_kind(&dec), (ColKind::Utf8, ValueParser::Text));
        // Precision never drops below scale + 1.
        let dec = col(ColumnType::MYSQL_TYPE_NEWDECIMAL).with_column_length(3).with_decimals(5);
        assert_eq!(column_kind(&dec).0, ColKind::Decimal { precision: 6, scale: 5 });

        assert_eq!(column_kind(&col(ColumnType::MYSQL_TYPE_DATE)), (ColKind::Date, ValueParser::Date));
        assert_eq!(column_kind(&col(ColumnType::MYSQL_TYPE_DATETIME)), (ColKind::Timestamp, ValueParser::DateTime));
        assert_eq!(column_kind(&col(ColumnType::MYSQL_TYPE_TIMESTAMP)).0, ColKind::Timestamp);
        assert_eq!(column_kind(&col(ColumnType::MYSQL_TYPE_TIME)), (ColKind::Utf8, ValueParser::Time { fsp: Some(0) }));
        assert_eq!(column_kind(&col(ColumnType::MYSQL_TYPE_TIME2).with_decimals(3)), (ColKind::Utf8, ValueParser::Time { fsp: Some(3) }));
        assert_eq!(column_kind(&col(ColumnType::MYSQL_TYPE_TIME).with_decimals(31)), (ColKind::Utf8, ValueParser::Time { fsp: None }));
        assert_eq!(column_kind(&col(ColumnType::MYSQL_TYPE_BIT)), (ColKind::Int64, ValueParser::Bit));

        let text_blob = col(ColumnType::MYSQL_TYPE_BLOB).with_character_set(255);
        assert_eq!(column_kind(&text_blob), (ColKind::Utf8, ValueParser::Text));
        let bin_blob = col(ColumnType::MYSQL_TYPE_BLOB).with_character_set(63).with_flags(ColumnFlags::BINARY_FLAG | ColumnFlags::BLOB_FLAG);
        assert_eq!(column_kind(&bin_blob), (ColKind::Binary, ValueParser::Binary));
        let long_blob = col(ColumnType::MYSQL_TYPE_LONG_BLOB).with_character_set(63);
        assert_eq!(column_kind(&long_blob).0, ColKind::Binary);
        let varbinary = col(ColumnType::MYSQL_TYPE_VAR_STRING).with_character_set(63);
        assert_eq!(column_kind(&varbinary).0, ColKind::Binary);
        // utf8mb4_bin sets BINARY_FLAG but is still text (charset != 63).
        let bin_collation = col(ColumnType::MYSQL_TYPE_VAR_STRING).with_character_set(46).with_flags(ColumnFlags::BINARY_FLAG);
        assert_eq!(column_kind(&bin_collation).0, ColKind::Utf8);
        assert_eq!(column_kind(&col(ColumnType::MYSQL_TYPE_STRING).with_character_set(45)).0, ColKind::Utf8);
        assert_eq!(column_kind(&col(ColumnType::MYSQL_TYPE_VARCHAR).with_character_set(45)).0, ColKind::Utf8);
        assert_eq!(column_kind(&col(ColumnType::MYSQL_TYPE_JSON).with_character_set(63)).0, ColKind::Utf8);
        assert_eq!(column_kind(&col(ColumnType::MYSQL_TYPE_ENUM).with_character_set(45)).0, ColKind::Utf8);
        assert_eq!(column_kind(&col(ColumnType::MYSQL_TYPE_SET).with_character_set(45)).0, ColKind::Utf8);
        assert_eq!(column_kind(&col(ColumnType::MYSQL_TYPE_GEOMETRY).with_character_set(63)).0, ColKind::Binary);
        assert_eq!(column_kind(&col(ColumnType::MYSQL_TYPE_NULL)).0, ColKind::Utf8);
    }

    #[test]
    fn decimal_scaling() {
        assert_eq!(parse_decimal(b"123.45", 2), Some(12345));
        assert_eq!(parse_decimal(b"-123.45", 2), Some(-12345));
        assert_eq!(parse_decimal(b"123", 2), Some(12300));
        assert_eq!(parse_decimal(b"0.5", 3), Some(500));
        assert_eq!(parse_decimal(b".5", 1), Some(5));
        assert_eq!(parse_decimal(b"-0.01", 2), Some(-1));
        assert_eq!(parse_decimal(b"1.005", 2), Some(101), "round half away from zero");
        assert_eq!(parse_decimal(b"-1.005", 2), Some(-101));
        assert_eq!(parse_decimal(b"1.004", 2), Some(100));
        assert_eq!(parse_decimal(b"9.999", 2), Some(1000));
        assert_eq!(parse_decimal(b"42", 0), Some(42));
        assert_eq!(parse_decimal(b"42.7", 0), Some(43));
        assert_eq!(parse_decimal(b"+7.25", 2), Some(725));
        assert_eq!(parse_decimal(b"99999999999999999999999999999999999999", 0), Some(99_999_999_999_999_999_999_999_999_999_999_999_999i128));
        assert_eq!(parse_decimal(b"", 2), None);
        assert_eq!(parse_decimal(b"abc", 2), None);
        assert_eq!(parse_decimal(b"1.2.3", 2), None);
        assert_eq!(parse_decimal(b".", 2), None);
        // i128 overflow is reported, not wrapped.
        assert_eq!(parse_decimal(b"999999999999999999999999999999999999999", 2), None);
    }

    #[test]
    fn datetime_parsing() {
        let d = parse_date_days(b"1970-01-02").unwrap().unwrap();
        assert_eq!(d, 1);
        assert_eq!(parse_date_days(b"1969-12-31"), Some(Some(-1)));
        assert_eq!(parse_date_days(b"0000-00-00"), Some(None), "zero date -> NULL");
        assert_eq!(parse_date_days(b"2024-02-30"), Some(None), "impossible date -> NULL");
        assert_eq!(parse_date_days(b"2024-1-1"), None);
        assert_eq!(parse_date_days(b"hello"), None);

        assert_eq!(parse_datetime_micros(b"1970-01-01 00:00:00"), Some(Some(0)));
        assert_eq!(parse_datetime_micros(b"1970-01-01 00:00:01.5"), Some(Some(1_500_000)));
        assert_eq!(parse_datetime_micros(b"1970-01-01 00:00:00.000123"), Some(Some(123)));
        assert_eq!(parse_datetime_micros(b"1970-01-01 00:00:00.123456789"), Some(Some(123_456)), "nanos are truncated");
        assert_eq!(parse_datetime_micros(b"1970-01-02"), Some(Some(86_400_000_000)));
        // 946 684 800 s (2000-01-01) + 12 h 34 m 56 s (45 296 s).
        assert_eq!(parse_datetime_micros(b"2000-01-01T12:34:56"), Some(Some(946_730_096_000_000)));
        assert_eq!(parse_datetime_micros(b"0000-00-00 00:00:00"), Some(None));
        assert_eq!(parse_datetime_micros(b"0000-00-00 00:00:00.000000"), Some(None));
        assert_eq!(parse_datetime_micros(b"1969-12-31 23:59:59.999999"), Some(Some(-1)));
        assert_eq!(parse_datetime_micros(b"2024-01-01 25:00:00"), Some(None), "invalid time -> NULL");
        assert_eq!(parse_datetime_micros(b"2024-01-01 12:00"), None);
        assert_eq!(parse_datetime_micros(b"2024-01-01 12:00:00."), None);
        assert_eq!(parse_datetime_micros(b"garbage"), None);

        // Through the parser: typed values and text agree.
        let mut scratch = String::new();
        let text = Value::Bytes(b"2001-02-03 04:05:06.000007".to_vec());
        let typed = Value::Date(2001, 2, 3, 4, 5, 6, 7);
        let a = match ValueParser::DateTime.cell(Some(&text), &mut scratch, 0).unwrap() {
            Cell::Timestamp(x) => x,
            other => panic!("unexpected {other:?}"),
        };
        let b = match ValueParser::DateTime.cell(Some(&typed), &mut scratch, 0).unwrap() {
            Cell::Timestamp(y) => y,
            other => panic!("unexpected {other:?}"),
        };
        assert_eq!(a, b);
        let zero = Value::Bytes(b"0000-00-00".to_vec());
        assert!(matches!(ValueParser::Date.cell(Some(&zero), &mut scratch, 0).unwrap(), Cell::Null));
    }

    #[test]
    fn value_decoding() {
        let mut scratch = String::new();
        assert!(matches!(ValueParser::Int.cell(Some(&Value::Bytes(b"-42".to_vec())), &mut scratch, 0).unwrap(), Cell::I64(-42)));
        assert!(matches!(ValueParser::Int.cell(Some(&Value::NULL), &mut scratch, 0).unwrap(), Cell::Null));
        assert!(matches!(ValueParser::Int.cell(None, &mut scratch, 0).unwrap(), Cell::Null));
        assert!(ValueParser::Int.cell(Some(&Value::Bytes(b"x".to_vec())), &mut scratch, 0).is_err());
        assert!(matches!(ValueParser::Bit.cell(Some(&Value::Bytes(vec![0x01, 0x02])), &mut scratch, 0).unwrap(), Cell::I64(258)));
        assert!(matches!(ValueParser::Bit.cell(Some(&Value::Bytes(vec![])), &mut scratch, 0).unwrap(), Cell::I64(0)));
        assert!(ValueParser::Bit.cell(Some(&Value::Bytes(vec![0xff; 8])), &mut scratch, 0).is_err());
        match ValueParser::Float.cell(Some(&Value::Bytes(b"1.5e3".to_vec())), &mut scratch, 0).unwrap() {
            Cell::F64(f) => assert_eq!(f, 1500.0),
            other => panic!("unexpected {other:?}"),
        }
        assert!(matches!(ValueParser::Decimal { scale: 2 }.cell(Some(&Value::Bytes(b"-1.5".to_vec())), &mut scratch, 0).unwrap(), Cell::Decimal(-150)));
        assert!(matches!(ValueParser::Decimal { scale: 2 }.cell(Some(&Value::Int(3)), &mut scratch, 0).unwrap(), Cell::Decimal(300)));
        assert!(matches!(ValueParser::Text.cell(Some(&Value::Bytes(b"hi".to_vec())), &mut scratch, 0).unwrap(), Cell::Bytes(b"hi")));
        match ValueParser::Text.cell(Some(&Value::Time(true, 1, 2, 3, 4, 0)), &mut scratch, 0).unwrap() {
            Cell::Str(s) => assert_eq!(s, "-26:03:04"),
            other => panic!("unexpected {other:?}"),
        }
        match ValueParser::Text.cell(Some(&Value::UInt(u64::MAX)), &mut scratch, 0).unwrap() {
            Cell::Str(s) => assert_eq!(s, "18446744073709551615"),
            other => panic!("unexpected {other:?}"),
        }
        assert!(matches!(ValueParser::Binary.cell(Some(&Value::Bytes(vec![0, 255])), &mut scratch, 0).unwrap(), Cell::Bytes(&[0, 255])));
    }

    /// Extraction uses the binary protocol: its typed values must decode to what the text
    /// protocol's bytes decode to.
    #[test]
    fn binary_protocol_values_match_text() {
        let mut scratch = String::new();
        // FLOAT arrives as an f32; widening must not invent digits (text protocol: "1.1").
        match ValueParser::Float.cell(Some(&Value::Float(1.1)), &mut scratch, 0).unwrap() {
            Cell::F64(f) => assert_eq!(f, 1.1),
            other => panic!("unexpected {other:?}"),
        }
        match ValueParser::Float.cell(Some(&Value::Float(-3.4e38)), &mut scratch, 0).unwrap() {
            Cell::F64(f) => assert_eq!(f, -3.4e38),
            other => panic!("unexpected {other:?}"),
        }
        match ValueParser::Float.cell(Some(&Value::Float(1.5e-7)), &mut scratch, 0).unwrap() {
            Cell::F64(f) => assert_eq!(f, 1.5e-7),
            other => panic!("unexpected {other:?}"),
        }
        // TIME(3): exactly three fractional digits, like the text protocol.
        let time = |fsp: Option<u8>, v: Value| -> String {
            let mut s = String::new();
            match (ValueParser::Time { fsp }).cell(Some(&v), &mut s, 0).unwrap() {
                Cell::Str(x) => x.to_string(),
                Cell::Bytes(b) => String::from_utf8(b.to_vec()).unwrap(),
                other => panic!("unexpected {other:?}"),
            }
        };
        assert_eq!(time(Some(3), Value::Time(true, 0, 10, 20, 30, 500_000)), "-10:20:30.500");
        assert_eq!(time(Some(6), Value::Time(false, 0, 0, 0, 1, 0)), "00:00:01.000000");
        assert_eq!(time(Some(0), Value::Time(false, 34, 22, 59, 59, 0)), "838:59:59");
        assert_eq!(time(Some(2), Value::Time(false, 0, 1, 2, 3, 999_999)), "01:02:03.99");
        assert_eq!(time(None, Value::Time(false, 0, 1, 2, 3, 7)), "01:02:03.000007");
        assert_eq!(time(None, Value::Time(true, 1, 2, 3, 4, 0)), "-26:03:04");
        // Text-protocol bytes pass through untouched.
        assert_eq!(time(Some(3), Value::Bytes(b"-10:20:30.500".to_vec())), "-10:20:30.500");
        assert!(matches!((ValueParser::Time { fsp: Some(0) }).cell(Some(&Value::NULL), &mut scratch, 0).unwrap(), Cell::Null));
        // Zero dates: the binary protocol sends all-zero components.
        assert!(matches!(ValueParser::Date.cell(Some(&Value::Date(0, 0, 0, 0, 0, 0, 0)), &mut scratch, 0).unwrap(), Cell::Null));
        assert!(matches!(ValueParser::DateTime.cell(Some(&Value::Date(0, 0, 0, 0, 0, 0, 0)), &mut scratch, 0).unwrap(), Cell::Null));
        // YEAR / small integers / unsigned INT arrive typed.
        assert!(matches!(ValueParser::Int.cell(Some(&Value::UInt(2024)), &mut scratch, 0).unwrap(), Cell::I64(2024)));
        assert!(matches!(ValueParser::Int.cell(Some(&Value::Int(-3)), &mut scratch, 0).unwrap(), Cell::I64(-3)));
    }

    #[test]
    fn statement_separators() {
        let d = LexMode::default();
        let ansi = LexMode { backslash_escapes: true, ansi_quotes: true };
        let nbe = LexMode { backslash_escapes: false, ansi_quotes: false };
        // Plain single statements.
        assert!(!has_statement_separator("select 1", d));
        assert!(!has_statement_separator(trim_statement("select 1 ;  ; "), d));
        assert!(!has_statement_separator("with t as (select 1 a) select a from t", d));
        // Separators.
        assert!(has_statement_separator("select 1; drop table t", d));
        assert!(has_statement_separator("select 1;", d));
        assert!(has_statement_separator("select 1 /* c */; delete from t", d));
        // Quoted semicolons are data.
        assert!(!has_statement_separator("select ';' , \";\", `a;b` from t", d));
        assert!(!has_statement_separator("select 'it''s; fine'", d));
        assert!(!has_statement_separator("select `a``;b`", d));
        // Backslash escapes depend on NO_BACKSLASH_ESCAPES.
        let tricky = r"select 'a\'; drop table t; -- '";
        assert!(!has_statement_separator(tricky, d), "with backslash escapes the whole tail is one string");
        assert!(has_statement_separator(tricky, nbe), "without them the string ends at \\' and ';' is code");
        // ANSI_QUOTES: "..." is an identifier, backslash does not escape inside it.
        let dq = "select \"a\\\"; drop table t; -- \"";
        assert!(!has_statement_separator(dq, d));
        assert!(has_statement_separator(dq, ansi));
        // Comments.
        assert!(!has_statement_separator("select 1 -- ; drop\n", d));
        assert!(!has_statement_separator("select 1 # ; drop", d));
        assert!(!has_statement_separator("select 1 /* ; */", d));
        assert!(!has_statement_separator("select 1 --", d));
        assert!(has_statement_separator("select 1 --; drop table t", d), "`--` without a following space is two minus signs");
        assert!(has_statement_separator("select 1 -- c\n; drop table t", d));
        // Executable comments are code to the server.
        assert!(has_statement_separator("select 1 /*! ; drop table t */", d));
        assert!(has_statement_separator("select 1 /*!50700 ; drop table t */", d));
        assert!(has_statement_separator("select 1 /*M! ; drop table t */", d));
        assert!(!has_statement_separator("select 1 /*! + 1 */", d));
        assert!(!has_statement_separator("select 1 /*+ BKA(t) */", d), "optimizer hints are comments");
        // `*/` closes an executable comment as a unit: the `*` after it is not a comment start.
        assert!(has_statement_separator("select /*!1*/*/ ; drop table t", d));
        assert!(!has_statement_separator("select 2*/*c;*/3", d));
        // Unterminated literal / comment never panics.
        assert!(!has_statement_separator("select 'abc", d));
        assert!(!has_statement_separator("select 'abc\\", d));
        assert!(!has_statement_separator("select /* abc", d));

        // The mode-free pre-check is the union of every mode.
        assert!(may_have_statement_separator(tricky));
        assert!(may_have_statement_separator(dq));
        assert!(!may_have_statement_separator("select ';' from t where a = 'x'"));

        assert_eq!(LexMode::from_sql_mode(""), d);
        assert_eq!(LexMode::from_sql_mode("STRICT_TRANS_TABLES,NO_BACKSLASH_ESCAPES"), nbe);
        assert_eq!(LexMode::from_sql_mode("ansi_quotes"), ansi);
        assert_eq!(LexMode::from_sql_mode("REAL_AS_FLOAT,PIPES_AS_CONCAT,ANSI_QUOTES,IGNORE_SPACE,ONLY_FULL_GROUP_BY,ANSI"), ansi);
    }

    #[test]
    fn multi_statement_prepare_errors_are_invalid() {
        let parse = mysql_async::Error::Server(ServerError { code: 1064, message: "syntax error near 'drop table t'".into(), state: "42000".into() });
        assert!(matches!(prepare_error(parse, "/* bulkdb-x */ select 1; drop table t"), CoreError::Invalid(_)));
        let parse = mysql_async::Error::Server(ServerError { code: 1064, message: "syntax error".into(), state: "42000".into() });
        assert!(matches!(prepare_error(parse, "selec 1"), CoreError::Query { .. }), "a plain typo stays a QUERY error");
        let missing = mysql_async::Error::Server(ServerError { code: 1146, message: "no such table".into(), state: "42S02".into() });
        assert!(matches!(prepare_error(missing, "select 1; x"), CoreError::Query { .. }));
    }

    #[test]
    fn error_code_mapping() {
        let server = |code: u16| mysql_async::Error::Server(ServerError { code, message: format!("error {code}"), state: "HY000".into() });
        assert!(matches!(map_err(server(1317)), CoreError::Cancelled));
        assert!(matches!(map_err(server(3024)), CoreError::Timeout(_)));
        assert!(matches!(map_err(server(1969)), CoreError::Timeout(_)));
        assert!(matches!(map_err(server(1045)), CoreError::Auth(_)));
        assert!(matches!(map_err(server(1044)), CoreError::Auth(_)));
        assert!(matches!(map_err(server(2002)), CoreError::Connect(_)));
        assert!(matches!(map_err(server(2003)), CoreError::Connect(_)));
        assert!(matches!(map_err(server(2013)), CoreError::Connect(_)));
        match map_err(server(1064)) {
            CoreError::Query { code, msg } => {
                assert_eq!(code.as_deref(), Some("1064"));
                assert!(msg.contains("error 1064"));
            }
            other => panic!("unexpected {other:?}"),
        }
        let io = mysql_async::Error::Io(std::io::Error::new(std::io::ErrorKind::ConnectionReset, "reset").into());
        assert!(matches!(map_err(io), CoreError::Connect(_)));
        let closed = mysql_async::Error::Driver(mysql_async::DriverError::ConnectionClosed);
        assert!(matches!(map_err(closed), CoreError::Connect(_)));
        let unbound = mysql_async::Error::Driver(mysql_async::DriverError::StmtParamsMismatch { required: 1, supplied: 0 });
        assert!(matches!(map_err(unbound), CoreError::Invalid(_)));
        assert_eq!(server_code(&map_err(server(1193))), Some(1193));
        assert!(is_tls_error(&mysql_async::Error::Driver(mysql_async::DriverError::NoClientSslFlagFromServer)));
        assert!(!is_tls_error(&server(1045)));
    }

    #[test]
    fn grants_detection() {
        let ro = vec!["GRANT USAGE ON *.* TO `r`@`%`".to_string(), "GRANT SELECT ON `shop`.* TO `r`@`%`".to_string()];
        assert!(!grants_allow_write(&ro));
        let rw = vec!["GRANT SELECT, INSERT, UPDATE ON `shop`.* TO `w`@`%`".to_string()];
        assert!(grants_allow_write(&rw));
        let all = vec!["GRANT ALL PRIVILEGES ON *.* TO `root`@`localhost` WITH GRANT OPTION".to_string()];
        assert!(grants_allow_write(&all));
        let col = vec!["GRANT SELECT, UPDATE (price) ON `shop`.`items` TO `u`@`%`".to_string()];
        assert!(grants_allow_write(&col));
        let role = vec!["GRANT `app_ro`@`%` TO `u`@`%`".to_string()];
        assert!(!grants_allow_write(&role));
        assert!(!grants_allow_write(&[]));
    }

    #[test]
    fn sql_tagging() {
        assert_eq!(tag_sql("bulkdb-abc", "select 1;\n"), "/* bulkdb-abc */ select 1");
        assert_eq!(tag_sql("evil */ drop", "select 1"), "/* evil  drop */ select 1");
        assert_eq!(tag_like_pattern("bulkdb-a_b"), "%/* bulkdb-a\\_b */%");
        assert_eq!(trim_statement("  select 1 ; ;  "), "select 1");
    }

    #[test]
    fn explain_analyze_rows() {
        let raw = "-> Nested loop inner join  (cost=1.70 rows=3) (actual time=0.044..0.064 rows=3 loops=1)\n    -> Table scan on t1";
        assert_eq!(explain_analyze_root_rows(raw), Some(3));
        assert_eq!(explain_analyze_root_rows("nothing"), None);
    }

    const EXPLAIN_FIXTURE: &str = r#"{
  "query_block": {
    "select_id": 1,
    "cost_info": { "query_cost": "1234.56" },
    "ordering_operation": {
      "using_filesort": true,
      "nested_loop": [
        {
          "table": {
            "table_name": "orders",
            "access_type": "ALL",
            "rows_examined_per_scan": 100000,
            "rows_produced_per_join": 33333,
            "filtered": "33.33",
            "cost_info": { "read_cost": "900.00", "eval_cost": "33.33", "prefix_cost": "933.33", "data_read_per_join": "5M" },
            "used_columns": ["id", "customer_id"],
            "attached_condition": "(`shop`.`orders`.`status` = 'open')"
          }
        },
        {
          "table": {
            "table_name": "customers",
            "access_type": "eq_ref",
            "possible_keys": ["PRIMARY"],
            "key": "PRIMARY",
            "used_key_parts": ["id"],
            "key_length": "4",
            "ref": ["shop.orders.customer_id"],
            "rows_examined_per_scan": 1,
            "rows_produced_per_join": 33333,
            "filtered": "100.00",
            "cost_info": { "read_cost": "250.00", "eval_cost": "3.33", "prefix_cost": "1234.56", "data_read_per_join": "2M" }
          }
        }
      ],
      "select_list_subqueries": [
        {
          "dependent": true,
          "cacheable": false,
          "query_block": {
            "select_id": 2,
            "cost_info": { "query_cost": "0.35" },
            "table": {
              "table_name": "order_items",
              "access_type": "ref",
              "key": "order_id",
              "rows_examined_per_scan": 4,
              "rows_produced_per_join": 4,
              "filtered": "100.00"
            }
          }
        }
      ]
    }
  }
}"#;

    #[test]
    fn explain_json_parsing() {
        let parsed = parse_explain_json(EXPLAIN_FIXTURE).unwrap();
        assert_eq!(parsed.total_cost, Some(1234.56));
        assert_eq!(parsed.estimated_rows, Some(33333));
        let names: Vec<&str> = parsed.tables.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["orders", "customers", "order_items"]);

        let root = parsed.root.unwrap();
        assert_eq!(root.node_type, "query_block");
        assert_eq!(root.children.len(), 1);
        let ordering = &root.children[0];
        assert_eq!(ordering.node_type, "ordering_operation");
        assert_eq!(ordering.extra.get("using_filesort").map(String::as_str), Some("true"));
        // nested_loop flattened + the subquery node.
        assert_eq!(ordering.children.len(), 3);
        let scan = &ordering.children[0];
        assert_eq!(scan.node_type, "ALL");
        assert_eq!(scan.relation.as_ref().unwrap().name, "orders");
        assert_eq!(scan.plan_rows, Some(33333.0));
        assert_eq!(scan.total_cost, Some(933.33));
        assert_eq!(scan.extra.get("rows_examined_per_scan").map(String::as_str), Some("100000"));
        assert_eq!(scan.extra.get("filtered").map(String::as_str), Some("33.33"));
        assert!(scan.extra.get("attached_condition").unwrap().contains("status"));
        assert!(!scan.extra.contains_key("used_columns"));
        let lookup = &ordering.children[1];
        assert_eq!(lookup.node_type, "eq_ref");
        assert_eq!(lookup.extra.get("key").map(String::as_str), Some("PRIMARY"));
        assert_eq!(lookup.extra.get("used_key_parts").map(String::as_str), Some("id"));
        let sub = &ordering.children[2];
        assert_eq!(sub.node_type, "subquery");
        assert_eq!(sub.parent_relationship.as_deref(), Some("SubPlan"));
        assert_eq!(sub.extra.get("dependent").map(String::as_str), Some("true"));
        assert_eq!(sub.children.len(), 1);
        assert_eq!(sub.children[0].node_type, "query_block");
        assert_eq!(sub.children[0].total_cost, Some(0.35));
        assert_eq!(sub.children[0].children[0].node_type, "ref");
        assert_eq!(sub.children[0].children[0].relation.as_ref().unwrap().name, "order_items");
    }

    #[test]
    fn explain_json_single_table_and_materialized() {
        let raw = r#"{"query_block":{"select_id":1,"cost_info":{"query_cost":"2.50"},
            "table":{"table_name":"<derived2>","access_type":"ALL","rows_examined_per_scan":10,"rows_produced_per_join":10,"filtered":"100.00",
              "materialized_from_subquery":{"using_temporary_table":true,"dependent":false,"cacheable":true,
                "query_block":{"select_id":2,"cost_info":{"query_cost":"1.25"},
                  "table":{"table_name":"t","access_type":"range","key":"ix","rows_examined_per_scan":10,"rows_produced_per_join":10,"filtered":"100.00"}}}}}}"#;
        let parsed = parse_explain_json(raw).unwrap();
        assert_eq!(parsed.estimated_rows, Some(10));
        assert_eq!(parsed.total_cost, Some(2.5));
        assert_eq!(parsed.tables.len(), 1, "derived tables are not real tables");
        assert_eq!(parsed.tables[0].name, "t");
        let root = parsed.root.unwrap();
        let derived = &root.children[0];
        assert_eq!(derived.node_type, "ALL");
        assert_eq!(derived.children[0].node_type, "materialized_from_subquery");
        assert_eq!(derived.children[0].parent_relationship.as_deref(), Some("SubPlan"));
        assert_eq!(derived.children[0].children[0].children[0].node_type, "range");

        // MariaDB shape: "rows" instead of rows_produced_per_join, ANALYZE counters.
        let maria = r#"{"query_block":{"select_id":1,"r_loops":1,"r_total_time_ms":12.5,
            "table":{"table_name":"t","access_type":"ALL","rows":500,"r_rows":480,"r_loops":1,"filtered":100,"r_filtered":96}}}"#;
        let parsed = parse_explain_json(maria).unwrap();
        assert_eq!(parsed.estimated_rows, Some(500));
        let root = parsed.root.unwrap();
        assert_eq!(root.actual_time_ms, Some(12.5));
        assert_eq!(root.children[0].actual_rows, Some(480.0));

        assert!(parse_explain_json("not json").is_err());
        assert!(parse_explain_json("[1,2]").is_err());
    }

    #[test]
    fn write_sample_ring() {
        let db_samples: std::sync::Mutex<HashMap<String, VecDeque<bool>>> = std::sync::Mutex::new(HashMap::new());
        // Same arithmetic as `record_write_sample`, exercised without a connection.
        let mut map = db_samples.lock().unwrap();
        let ring = map.entry("j".into()).or_default();
        for i in 0..15 {
            ring.push_back(i % 3 == 0);
            while ring.len() > WRITE_RING {
                ring.pop_front();
            }
        }
        assert_eq!(ring.len(), WRITE_RING);
    }

    #[test]
    fn value_helpers() {
        assert_eq!(value_u64(Some(&Value::Bytes(b"123".to_vec()))), Some(123));
        assert_eq!(value_u64(Some(&Value::Bytes(b"123.0".to_vec()))), Some(123));
        assert_eq!(value_u64(Some(&Value::Int(-1))), None);
        assert_eq!(value_u64(Some(&Value::UInt(7))), Some(7));
        assert_eq!(value_u64(Some(&Value::NULL)), None);
        assert_eq!(value_u64(None), None);
        assert_eq!(value_str(Some(&Value::Date(2024, 5, 6, 0, 0, 0, 0))).as_deref(), Some("2024-05-06"));
        assert_eq!(value_str(Some(&Value::Date(2024, 5, 6, 7, 8, 9, 10))).as_deref(), Some("2024-05-06 07:08:09.000010"));
        assert_eq!(value_str(Some(&Value::Bytes(vec![0xff, b'a']))).as_deref(), Some("\u{fffd}a"));
    }
}
