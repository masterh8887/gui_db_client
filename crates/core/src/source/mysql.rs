//! MySQL / MariaDB driver over mysql_async 0.36 (protocol 4.1: MySQL 4.1+ and MariaDB). Pre-4.1
//! servers (3.23 / 4.0) are served by `mysql_legacy`; `mysql_dispatch` picks the client.
//!
//! * One `Conn` per [`MySqlDatabase`], kept in a `tokio::sync::Mutex<Slot>`. While an
//!   extraction streams, the connection is moved into the reader task (`Slot::Busy`; catalog and
//!   monitor calls run on their own `bulkdb-ui` / `bulkdb-monitor` objects, so they never
//!   collide). A connection that was killed or broke (`Slot::Gone`: a cancelled stream, a cancel
//!   on 4.1 where only `KILL <id>` exists) is re-opened lazily by the next call, with the same
//!   options and text plan.
//! * Extraction runs as a server-side prepared statement: `conn.prep(sql)` ->
//!   `conn.exec_iter(&stmt, ())` -> `result.stream::<Row>()` decoded row by row into
//!   [`ArrowBatcher`]; a batch is cut at 64K rows / 8 MB and sent through a tiny bounded
//!   channel to the pipeline. No row is ever retained. mysql_async always negotiates
//!   `CLIENT_MULTI_STATEMENTS` (it cannot be switched off), so the text protocol would execute
//!   `SELECT 1; DROP TABLE t` as two statements; `COM_STMT_PREPARE` accepts exactly one
//!   statement, which makes the server itself the last line of defence behind `sqlguard`.
//!   Where the server supports it (5.6.5+ / MariaDB 10.0+) the extraction session is also
//!   `SET SESSION TRANSACTION READ ONLY`.
//! * Text encoding (design doc section 5): connect reads `@@character_set_server`,
//!   `@@character_set_database` and `@@sql_mode`, picks a [`TextPlan`] from the `encoding`
//!   field (`mysql_dispatch::plan_text`) and runs its session statements: `SET NAMES utf8mb4`
//!   (`utf8` before 5.5.3), or for EUC-KR data the pass-through setup with
//!   `character_set_results = NULL`. SQL is encoded with the plan's charset and sent as bytes;
//!   on a pass-through connection every text column is decoded by its own collation id
//!   (`SHOW COLLATION`), which reads CP949 stored in `latin1` columns correctly.
//! * Old passwords: `allow_old_password = true` sets `secure_auth(false)`; otherwise a server
//!   asking for `mysql_old_password` yields `AUTH` with [`OLD_PASSWORD_HINT`].
//! * Probe, session settings, EXPLAIN, catalog and monitor are the version-aware shared
//!   implementations over [`MysqlRawQuery`] (`mysql_common`, `catalog::mysql::generic`,
//!   `monitor::mysql::generic`), which this driver implements over the text protocol.
//! * Cancel: `KILL QUERY <connection id>` (5.0+) or `KILL <connection id>` (4.1, ends the
//!   session) over a second short-lived connection ([`kill_sql`]), then the stream ends with
//!   `Cancelled` and the (half-read) connection is dropped. The same happens when the
//!   consumer goes away (the batch receiver is dropped after a sink failure, or `stream()`'s
//!   future is dropped): every wait on the server (prepare, execute, next row) also races
//!   `batch_tx.closed()`, so no reader task, socket or server query outlives the job.
//!
//! mysql_async 0.36 does not send connection attributes, so `program_name` cannot be set from
//! the client. The extraction SQL is tagged with a leading `/* bulkdb-<job> */` comment instead,
//! which the monitor can find in the process list when no session id is known.
//!
//! TLS: the workspace builds mysql_async with its default features, which include no TLS
//! backend ([`TLS_BACKEND`] is `false`). A TLS attempt could only end right after the server
//! greeting without a login packet, which MySQL counts toward `max_connect_errors` and which
//! finally blocks the client host (design doc section 3). So, before any socket is opened,
//! `Require` / `VerifyFull` are refused with a `CONNECT` error ([`check_tls_supported`]) and
//! `Prefer` connects in clear text straight away. The connect future stays guarded with
//! `catch_unwind` (mysql_async *panics* when TLS is requested without a backend).

use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use arrow_schema::SchemaRef;
use async_trait::async_trait;
use futures::{FutureExt, StreamExt};
use mysql_async::consts::{ColumnFlags, ColumnType};
use mysql_async::prelude::Queryable;
use mysql_async::{Column, Conn, Opts, OptsBuilder, Row, SslOpts, Value};
use tokio::sync::{mpsc, oneshot, Mutex, MutexGuard};
use tokio_util::sync::CancellationToken;

use super::arrow_batch::{schema_from, ArrowBatcher, Cell, ColKind};
use super::mysql_common::{
    self, kill_sql, server_error_code, MetaColumn, MetaRows, MysqlCaps, MysqlRawQuery, ServerFacts, WriteSamples, ER_NO_SUCH_THREAD, ER_PARSE_ERROR, META_ROW_LIMIT,
    OLD_PASSWORD_HINT,
};
use super::mysql_dispatch::{plan_text, MysqlConnOpts, TextPlan};
use super::{Batch, ConnectParams, Database, Query, Source, SourceStream, SslMode};
use crate::encoding::{decode_cell, repair_latin1_mojibake, Charset, CollationMap};
use crate::error::{CoreError, Result};
use crate::types::{PlanNode, PlanSummary, ServerInfo, SessionOpts, TableRef};
use crate::BATCH_ROWS;

/// Binary charset number: columns with this collation carry bytes, not text.
const BINARY_CHARSET: u16 = 63;
/// How long a `KILL [QUERY]` side connection may take (connect + kill).
const KILL_TIMEOUT: Duration = Duration::from_secs(10);
/// Depth of the internal reader -> pipeline channel (the pipeline has its own 8-deep channel).
const READER_CHANNEL_DEPTH: usize = 2;
/// Client-side packet limit (the server's own `max_allowed_packet` caps what it sends). Set in
/// `Opts` together with `wait_timeout`, so mysql_async skips its post-connect
/// `SELECT @@max_allowed_packet, @@wait_timeout` round trip.
const CLIENT_MAX_ALLOWED_PACKET: usize = 1 << 30;
const CLIENT_WAIT_TIMEOUT_S: usize = 28_800;
/// First statement after the handshake (4.1+): version, default charsets and quoting rules.
const CONNECT_FACTS_SQL: &[u8] = b"SELECT VERSION(), @@character_set_server, @@character_set_database, @@sql_mode";

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

/// Where the connection of a [`MySqlDatabase`] is.
enum Slot {
    Idle(Conn),
    /// Moved into a running extraction.
    Busy,
    /// Killed or broken: the next call opens a new one.
    Gone,
}

pub struct MySqlDatabase {
    /// Options of the successful connect (TLS choice, `secure_auth`), reused for side
    /// connections and reconnects. Holds the password; never logged.
    opts: Opts,
    connect_timeout: Duration,
    conn: Arc<Mutex<Slot>>,
    /// Live connection id (a re-opened connection gets a new one).
    session_id: Arc<AtomicU32>,
    /// Per job ring of "server is sending to the client" samples (see `monitor/mysql.rs`).
    write_samples: WriteSamples,
    /// What the shared version-aware logic needs (`mysql_common::ServerFacts`).
    facts: ServerFacts,
}

impl std::fmt::Debug for MySqlDatabase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MySqlDatabase")
            .field("host", &self.facts.host)
            .field("port", &self.facts.port)
            .field("database", &self.facts.database)
            .field("session_id", &self.session_id.load(Ordering::Relaxed))
            .field("version", &self.facts.version.raw)
            .field("text_plan", &self.facts.plan)
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

const BUSY: &str = "connection is busy with a running extraction";

fn conn_mut(slot: &mut Slot) -> Result<&mut Conn> {
    match slot {
        Slot::Idle(c) => Ok(c),
        _ => Err(CoreError::Connect(BUSY.into())),
    }
}

impl MySqlDatabase {
    /// Connect (honouring `params.ssl` and `allow_old_password`), read the server version,
    /// default charsets and `sql_mode`, choose the [`TextPlan`] from the `encoding` field and run
    /// its session statements (design doc section 4, "connect").
    pub async fn connect(params: &ConnectParams) -> Result<Self> {
        let settings = MysqlConnOpts::from_params(params)?;
        let timeout = params.connect_timeout;
        let old = settings.allow_old_password;
        // Nothing below may open a socket that is certain to fail right after the greeting
        // (design doc section 3).
        check_tls_supported(params)?;
        let tls = match params.ssl {
            SslMode::Disable => None,
            // Without a TLS backend the TLS attempt would be dropped after the greeting before
            // the clear-text retry: go to clear text directly.
            SslMode::Prefer if !TLS_BACKEND => None,
            SslMode::Prefer | SslMode::Require => Some(SslOpts::default().with_danger_accept_invalid_certs(true).with_danger_skip_domain_validation(true)),
            SslMode::VerifyFull => Some(SslOpts::default()),
        };
        let (mut conn, opts) = match tls {
            None => {
                let o = build_opts(params, None, old);
                (connect_with_timeout(o.clone(), timeout).await.map_err(ConnectFailure::into_core)?, o)
            }
            Some(ssl) => {
                let o = build_opts(params, Some(ssl), old);
                match connect_with_timeout(o.clone(), timeout).await {
                    Ok(c) => (c, o),
                    Err(ConnectFailure::Tls(reason)) if params.ssl == SslMode::Prefer => {
                        tracing::debug!("mysql {}:{}: TLS unavailable ({reason}); retrying without TLS", params.host, params.port);
                        let plain = build_opts(params, None, old);
                        (connect_with_timeout(plain.clone(), timeout).await.map_err(ConnectFailure::into_core)?, plain)
                    }
                    Err(other) => return Err(other.into_core()),
                }
            }
        };

        // Before any SET NAMES the results use the handshake collation (utf8 / utf8mb4); these
        // values are ASCII on every server.
        let head = query_meta(&mut conn, CONNECT_FACTS_SQL).await?;
        let cell = |i: usize| head.raw(0, i).map(|b| String::from_utf8_lossy(b).trim().to_string()).filter(|s| !s.is_empty());
        let version = ServerVersion::parse(&cell(0).unwrap_or_default());
        let server_charset = cell(1);
        let database_charset = cell(2);
        let lex = cell(3).map(|m| LexMode::from_sql_mode(&m)).unwrap_or_default();
        let (plan, note) = plan_text(settings.encoding, &version, false, server_charset.as_deref(), database_charset.as_deref());
        if let Some(note) = &note {
            tracing::info!("mysql {}:{}: {note}", params.host, params.port);
        }
        for stmt in plan.session_statements() {
            conn.query_drop(stmt).await.map_err(map_err)?;
        }
        let mut collations = CollationMap::builtin();
        if matches!(plan, TextPlan::PassThrough { .. }) {
            match query_meta(&mut conn, b"SHOW COLLATION").await {
                Ok(rows) => fill_collations(&mut collations, &rows),
                Err(e) => tracing::warn!("mysql {}:{}: SHOW COLLATION failed ({e}); using the builtin collation table", params.host, params.port),
            }
        }
        let session_id = conn.id();
        tracing::debug!(
            "mysql connected to {}:{} ({} {}, charset {} / {}) as connection {session_id}; {}",
            params.host,
            params.port,
            version.label(),
            version.raw,
            server_charset.as_deref().unwrap_or("?"),
            database_charset.as_deref().unwrap_or("?"),
            plan.describe()
        );

        let facts = ServerFacts {
            caps: MysqlCaps::for_version(&version),
            version,
            legacy_protocol: false,
            session_id,
            server_charset,
            database_charset,
            plan,
            collations,
            lex,
            user: params.user.clone(),
            database: params.database.clone().filter(|d| !d.is_empty()),
            application_name: params.application_name.clone(),
            host: params.host.clone(),
            port: params.port,
        };
        Ok(MySqlDatabase {
            opts,
            connect_timeout: timeout,
            conn: Arc::new(Mutex::new(Slot::Idle(conn))),
            session_id: Arc::new(AtomicU32::new(session_id)),
            write_samples: WriteSamples::default(),
            facts,
        })
    }

    pub fn version(&self) -> &ServerVersion {
        &self.facts.version
    }

    pub fn flavor(&self) -> Flavor {
        self.facts.version.flavor
    }

    /// Database named in the connection (`None` when the account connected without one).
    pub fn current_database(&self) -> Option<&str> {
        self.facts.database.as_deref()
    }

    pub fn application_name(&self) -> &str {
        &self.facts.application_name
    }

    /// How this connection encodes SQL and decodes text.
    pub fn text_plan(&self) -> &TextPlan {
        &self.facts.plan
    }

    /// Connection options of this database (DB copy opens its own writer connections from them).
    #[allow(dead_code)] // used by copy::target::mysql (owner copy-data, W1)
    pub(crate) fn opts(&self) -> &Opts {
        &self.opts
    }

    // --- connection helpers ------------------------------------------------------------------

    /// Lock the connection slot, re-opening a killed or broken connection first. `CONNECT`
    /// while an extraction holds the connection.
    async fn idle_conn(&self) -> Result<MutexGuard<'_, Slot>> {
        let mut guard = self.conn.lock().await;
        match &*guard {
            Slot::Idle(_) => {}
            Slot::Busy => return Err(CoreError::Connect(BUSY.into())),
            Slot::Gone => *guard = Slot::Idle(self.reopen().await?),
        }
        Ok(guard)
    }

    /// A new connection with the same options and text plan (after a kill or a broken socket).
    async fn reopen(&self) -> Result<Conn> {
        let mut conn = connect_with_timeout(self.opts.clone(), self.connect_timeout).await.map_err(ConnectFailure::into_core)?;
        for stmt in self.facts.plan.session_statements() {
            conn.query_drop(stmt).await.map_err(map_err)?;
        }
        self.session_id.store(conn.id(), Ordering::Relaxed);
        tracing::debug!("mysql {}:{}: reconnected as connection {}", self.facts.host, self.facts.port, conn.id());
        Ok(conn)
    }
}

/// `SHOW COLLATION` (`Collation, Charset, Id, ...`) -> collation id -> charset.
fn fill_collations(map: &mut CollationMap, rows: &MetaRows) {
    let (Some(c_charset), Some(c_id)) = (rows.col("Charset"), rows.col("Id")) else { return };
    for r in 0..rows.len() {
        let id = rows.u64(r, c_id).and_then(|v| u16::try_from(v).ok());
        let name = rows.raw(r, c_charset).map(String::from_utf8_lossy);
        if let (Some(id), Some(name)) = (id, name) {
            map.insert_named(id, &name);
        }
    }
}

/// Text-protocol query with every row read as raw bytes plus the column collation ids
/// (metadata only, never table data; at most [`META_ROW_LIMIT`] rows). Further result sets,
/// which a single statement never has, are drained.
async fn query_meta(conn: &mut Conn, sql: &[u8]) -> Result<MetaRows> {
    let result = conn.query_iter(sql).await.map_err(map_err)?;
    let columns: Vec<MetaColumn> = result.columns_ref().iter().map(|c| MetaColumn { name: c.name_str().into_owned(), collation: Some(c.character_set()) }).collect();
    let mut rows: Vec<Vec<Option<Vec<u8>>>> = Vec::new();
    let mut overflow = false;
    result
        .for_each_and_drop(|mut row| {
            if rows.len() >= META_ROW_LIMIT {
                overflow = true;
                return;
            }
            let cells = (0..row.len())
                .map(|i| match row.take::<Value, usize>(i) {
                    Some(Value::Bytes(b)) => Some(b),
                    Some(Value::NULL) | None => None,
                    Some(other) => value_str(Some(&other)).map(String::into_bytes),
                })
                .collect();
            rows.push(cells);
        })
        .await
        .map_err(map_err)?;
    if overflow {
        return Err(CoreError::Internal(format!("a metadata query returned more than {META_ROW_LIMIT} rows")));
    }
    Ok(MetaRows { columns, rows })
}

/// Whether mysql_async is built with a TLS backend (its `native-tls-tls` / `rustls-tls`
/// feature). The workspace enables neither; set this to `true` together with that feature.
pub const TLS_BACKEND: bool = false;

/// Refuse `sslmode=require` / `verify-full` before a socket is opened when this build cannot do
/// TLS: such a connect would always fail right after the server greeting without a login
/// packet, and MySQL counts each of those toward `max_connect_errors` for the client host
/// (design doc section 3). `mysql_dispatch::open` also runs it for `protocol=auto`, since the
/// pre-4.1 client never has TLS either.
pub fn check_tls_supported(params: &ConnectParams) -> Result<()> {
    if !TLS_BACKEND && matches!(params.ssl, SslMode::Require | SslMode::VerifyFull) {
        return Err(CoreError::Connect(
            "TLS is not available: this build of the MySQL driver has no TLS backend (and the pre-4.1 client none at all); set sslmode to prefer or disable, or use an SSH tunnel"
                .into(),
        ));
    }
    Ok(())
}

/// Build `Opts` for `params` with an optional TLS configuration. `allow_old_password` turns
/// mysql_async's `secure_auth` off, so a 4.1 - 5.7.4 account with a pre-4.1 hash can log in.
fn build_opts(params: &ConnectParams, ssl: Option<SslOpts>, allow_old_password: bool) -> Opts {
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
        .secure_auth(!allow_old_password)
        .max_allowed_packet(Some(CLIENT_MAX_ALLOWED_PACKET))
        .wait_timeout(Some(CLIENT_WAIT_TIMEOUT_S))
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

/// [`kill_sql`] (`KILL QUERY <id>` on 5.0+, `KILL <id>` on 4.1) over a fresh short-lived
/// connection built from the same options. `ER_NO_SUCH_THREAD` (already finished) is success.
pub(crate) async fn kill_with(opts: &Opts, caps: &MysqlCaps, id: u32) -> Result<()> {
    let opts = opts.clone();
    let sql = kill_sql(caps, id);
    let label = sql.clone();
    let work = async move {
        let mut side = Conn::new(opts).await.map_err(map_err)?;
        let r = side.query_drop(sql).await;
        let _ = side.disconnect().await;
        match r {
            Ok(()) => Ok(()),
            Err(mysql_async::Error::Server(ref s)) if s.code == ER_NO_SUCH_THREAD => Ok(()),
            Err(e) => Err(map_err(e)),
        }
    };
    match tokio::time::timeout(KILL_TIMEOUT, work).await {
        Ok(r) => r,
        Err(_) => Err(CoreError::Timeout(format!("{label} did not complete within {} s", KILL_TIMEOUT.as_secs()))),
    }
}

// ---------------------------------------------------------------------------------------------
// Error mapping
// ---------------------------------------------------------------------------------------------

/// Map a driver error onto `CoreError`. Server codes go through
/// [`mysql_common::map_server_error`], the same table as the pre-4.1 client: 1317 interrupted
/// -> `Cancelled`, 3024 (`max_execution_time`) / 1969 (MariaDB `max_statement_time`) ->
/// `Timeout`, 1045 / 1044 / 1698 and 1275 (secure_auth refuses an old hash) -> `Auth`, 1129
/// (host blocked, also when sent instead of the greeting) / 1130 / 1040 / 2002 / 2003 / 2006
/// / 2013 -> `Connect` (1129 / 1130 / 1275 with a hint), others -> `Query`. I/O -> `Connect`.
/// A server that asks for `mysql_old_password` while `allow_old_password` is off -> `Auth`
/// with [`OLD_PASSWORD_HINT`].
pub fn map_err(e: mysql_async::Error) -> CoreError {
    use mysql_async::{DriverError, Error};
    match e {
        // An ERR sent instead of the greeting carries no SQLSTATE.
        Error::Server(s) => mysql_common::map_server_error(s.code, Some(s.state.as_str()).filter(|st| !st.is_empty()), s.message),
        Error::Io(io) => CoreError::Connect(io.to_string()),
        Error::Driver(d) => match d {
            DriverError::FromValue { .. } | DriverError::FromRow { .. } => CoreError::Internal(format!("mysql value conversion: {d}")),
            // Extraction is a prepared statement executed without parameters: a bare `?` or
            // `:name` in the SQL is a placeholder nobody binds.
            DriverError::StmtParamsMismatch { .. } | DriverError::MissingNamedParam { .. } | DriverError::MixedParams | DriverError::NamedParamsForPositionalQuery => {
                CoreError::Invalid(format!("the SQL contains placeholders (`?` or `:name`) but no parameters are bound: {d}"))
            }
            DriverError::MysqlOldPasswordDisabled => CoreError::Auth(OLD_PASSWORD_HINT.to_string()),
            DriverError::ConnectionClosed | DriverError::PoolDisconnected | DriverError::PacketOutOfOrder | DriverError::UnexpectedPacket { .. } => {
                CoreError::Connect(d.to_string())
            }
            other => CoreError::Connect(other.to_string()),
        },
        Error::Url(u) => CoreError::Invalid(u.to_string()),
        Error::Other(o) => CoreError::Internal(o.to_string()),
    }
}

// ---------------------------------------------------------------------------------------------
// Value helpers
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
/// * `BIT` -> `Int64` (`BIT(64)` values above `i64::MAX` keep their bits: read them as `u64`)
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
                // BIT(64) can exceed i64::MAX: keep the 64 bits (consumers reinterpret as u64).
                Value::UInt(u) => Ok(Cell::I64(*u as i64)),
                Value::Bytes(b) => {
                    parse_bit(b).map(Cell::I64).ok_or_else(|| CoreError::Internal(format!("column {col}: BIT value of {} bytes is longer than 64 bits", b.len())))
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

pub(crate) fn parse_i64(b: &[u8]) -> Option<i64> {
    std::str::from_utf8(b).ok()?.trim().parse::<i64>().ok()
}

/// `BIT(n)` arrives as up to 8 big-endian bytes. The 64 bits are kept as they are: a `BIT(64)`
/// value with the top bit set becomes a negative `i64` whose bits are the unsigned value (the
/// Arrow column is `Int64`; consumers that need the BIT value read it as `u64`). `None` only for
/// more than 8 bytes.
pub(crate) fn parse_bit(b: &[u8]) -> Option<i64> {
    if b.len() > 8 {
        return None;
    }
    let mut v: u64 = 0;
    for byte in b {
        v = (v << 8) | *byte as u64;
    }
    Some(v as i64)
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

pub(crate) fn ymd_to_days(y: i32, m: u32, d: u32) -> Option<i32> {
    if y == 0 && m == 0 && d == 0 {
        return None;
    }
    let date = chrono::NaiveDate::from_ymd_opt(y, m, d)?;
    Some(super::arrow_batch::days_from_date(date))
}

pub(crate) fn ymdhms_to_micros(y: i32, m: u32, d: u32, h: u32, i: u32, s: u32, us: u32) -> Option<i64> {
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

/// How result text is decoded (from the connection's [`ServerFacts`]).
struct TextDecode {
    plan: TextPlan,
    collations: CollationMap,
}

impl TextDecode {
    /// Charset to decode a text column with when it is not UTF-8 already (`None`: the bytes are
    /// UTF-8 or binary and go to the batcher untouched).
    fn transcode(&self, col: &Column, parser: ValueParser) -> Option<Charset> {
        if parser != ValueParser::Text {
            return None;
        }
        match self.plan.column_charset(Some(col.character_set()), &self.collations) {
            cs @ (Charset::EucKr | Charset::Latin1) => Some(cs),
            Charset::Utf8 | Charset::Binary => None,
        }
    }

    /// Column name as shown in the output: metadata is UTF-8 (the data dictionary), with the
    /// latin1 double-encoding repaired on a `PassThrough { latin1, EUC-KR }` connection.
    fn column_name(&self, col: &Column) -> String {
        let name = col.name_str().into_owned();
        if self.plan.repairs_metadata() {
            return repair_latin1_mojibake(&name, self.plan.sql_charset()).unwrap_or(name);
        }
        name
    }
}

/// Everything the reader task needs besides the connection.
struct StreamCtx {
    opts: Opts,
    caps: MysqlCaps,
    session_id: u32,
    slot: Arc<Mutex<Slot>>,
    cancel: CancellationToken,
    max_rows: usize,
    /// Byte limit of one batch (`Query::batch_bytes`).
    max_bytes: usize,
    /// U+FFFD produced by decoding (`SourceStream.replaced`).
    replaced: Arc<AtomicU64>,
    text: TextDecode,
    /// The SQL as the user wrote it (UTF-8), for error messages.
    sql_text: String,
}

enum Step {
    Row(Row),
    End,
    Fail(mysql_async::Error),
    /// The job was cancelled or the consumer dropped the stream.
    Cancel,
}

/// Reader task body: runs the query, streams rows into batches and hands the connection back
/// to the slot when the result set was fully drained (a half-read one is dropped and the slot
/// marked `Gone`, so the next call reconnects).
async fn stream_task(mut conn: Conn, sql: Vec<u8>, ctx: StreamCtx, schema_tx: oneshot::Sender<Result<SchemaRef>>, batch_tx: mpsc::Sender<Result<Batch>>) {
    let mut schema_tx = Some(schema_tx);
    let mut clean = true;
    let outcome = run_query(&mut conn, &sql, &ctx, &mut schema_tx, &batch_tx, &mut clean).await;
    match outcome {
        Ok(()) if clean => {
            *ctx.slot.lock().await = Slot::Idle(conn);
        }
        Ok(()) => {
            // Every row was delivered but the connection could not be tidied up (closing the
            // prepared statement failed): do not hand a broken connection back.
            drop(conn);
            *ctx.slot.lock().await = Slot::Gone;
        }
        Err(e) => {
            // Settle the connection before reporting, so a caller that reacts to the error by
            // using this database again (EXPLAIN, another stream) finds it back in its slot.
            if clean {
                // The server answered with an error before any row: the connection is usable.
                *ctx.slot.lock().await = Slot::Idle(conn);
            } else {
                // Half-read result set: closing the socket makes the server abort the statement.
                drop(conn);
                *ctx.slot.lock().await = Slot::Gone;
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
    if let Err(e) = kill_with(&ctx.opts, &ctx.caps, ctx.session_id).await {
        tracing::warn!("mysql: {} failed: {e}", kill_sql(&ctx.caps, ctx.session_id));
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
    if server_error_code(&err) == Some(ER_PARSE_ERROR) && may_have_statement_separator(sql) {
        return CoreError::Invalid(format!("the extraction SQL must be a single SELECT statement; MySQL refused to prepare it: {err}"));
    }
    err
}

/// Prepare `sql` (already encoded for the connection) and stream its result, then close the
/// statement so none is left behind on the server (`stmt_cache_size` is 0, so mysql_async never
/// closes it on its own).
///
/// Prepared statements are the multi-statement guard: `COM_STMT_PREPARE` accepts exactly one
/// statement, whereas the text protocol runs every `;`-separated statement because mysql_async
/// always negotiates `CLIENT_MULTI_STATEMENTS`.
async fn run_query(
    conn: &mut Conn,
    sql: &[u8],
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
            // The caller gave up on `stream()` (its future was dropped): stop like a cancel.
            _ = batch_tx.closed() => {
                *clean = false;
                return Err(cancelled(ctx).await);
            }
            r = &mut fut => r.map_err(|e| prepare_error(e, &ctx.sql_text))?,
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
/// On a pass-through connection text columns in EUC-KR / latin1 are decoded per column; bytes
/// invalid in their charset become U+FFFD and are counted (logged once, as a count only).
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
            // A slow first row (server-side sort) while the consumer went away.
            _ = batch_tx.closed() => {
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
    let mut transcode: Vec<Option<Charset>> = Vec::with_capacity(columns.len());
    for c in columns.iter() {
        let (kind, parser) = column_kind(c);
        named.push((ctx.text.column_name(c), kind));
        kinds.push(kind);
        parsers.push(parser);
        transcode.push(ctx.text.transcode(c, parser));
    }
    let schema = schema_from(&named);
    if let Some(tx) = schema_tx.take() {
        let _ = tx.send(Ok(schema.clone()));
    }
    let mut batcher = ArrowBatcher::with_limits(schema, kinds, ctx.max_rows, ctx.max_bytes);
    let mut scratch = String::new();
    let mut replaced: u64 = 0;

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
                // The pipeline dropped the stream (sink failure, job torn down) while the server
                // is slow to produce the next row: do not wait for it, kill the query now.
                _ = batch_tx.closed() => Step::Cancel,
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
                        let v = row.as_ref(i);
                        let cell = match (transcode[i], v) {
                            (Some(cs), Some(Value::Bytes(b))) => {
                                let d = decode_cell(cs, b, &mut scratch);
                                replaced += u64::from(d.replaced);
                                if d.replaced {
                                    ctx.replaced.fetch_add(d.text.matches('\u{FFFD}').count() as u64, Ordering::Relaxed);
                                }
                                Cell::Str(d.text)
                            }
                            _ => p.cell(v, &mut scratch, i)?,
                        };
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
    if replaced > 0 {
        tracing::warn!("mysql: {replaced} text values held bytes that are invalid in their character set; they were decoded with U+FFFD");
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
        mysql_common::probe(self).await
    }

    async fn apply_session(&self, opts: &SessionOpts) -> Result<()> {
        mysql_common::apply_session(self, opts).await
    }

    async fn stream(&self, q: &Query, opts: &SessionOpts, cancel: CancellationToken) -> Result<SourceStream> {
        // `Batch::RawCsv` is PostgreSQL only; every output comes out of this driver as Arrow.
        // Checked before tagging, so an unrepresentable character is reported at its line /
        // column in the user's SQL rather than in `/* tag */ SQL`.
        self.facts.check_sql_text(&q.sql)?;
        let tagged = tag_sql(&self.facts.application_name, &q.sql);
        let sql = self.facts.encode_sql(&tagged)?.into_owned();
        let conn = {
            let mut guard = self.idle_conn().await?;
            match std::mem::replace(&mut *guard, Slot::Busy) {
                Slot::Idle(c) => c,
                other => {
                    *guard = other;
                    return Err(CoreError::Connect(BUSY.into()));
                }
            }
        };
        let max_rows = opts.fetch_batch_rows.map(|n| (n as usize).clamp(1, BATCH_ROWS)).unwrap_or(BATCH_ROWS);
        let replaced = Arc::new(AtomicU64::new(0));
        let ctx = StreamCtx {
            opts: self.opts.clone(),
            caps: self.facts.caps,
            session_id: self.session_id.load(Ordering::Relaxed),
            slot: self.conn.clone(),
            cancel,
            max_rows,
            max_bytes: q.batch_bytes(),
            replaced: replaced.clone(),
            text: TextDecode { plan: self.facts.plan.clone(), collations: self.facts.collations.clone() },
            sql_text: q.sql.clone(),
        };
        let (schema_tx, schema_rx) = oneshot::channel();
        let (batch_tx, batch_rx) = mpsc::channel::<Result<Batch>>(READER_CHANNEL_DEPTH);
        tokio::spawn(stream_task(conn, sql, ctx, schema_tx, batch_tx));
        let schema = match schema_rx.await {
            Ok(r) => r?,
            Err(_) => return Err(CoreError::Internal("mysql reader task ended before reporting the result schema".into())),
        };
        let stream = futures::stream::unfold(batch_rx, |mut rx| async move { rx.recv().await.map(|b| (b, rx)) });
        // Only a non-UTF-8 text plan decodes anything (and can produce U+FFFD).
        let replaced = (!matches!(self.facts.plan, TextPlan::ServerUtf8 { .. })).then_some(replaced);
        Ok(SourceStream { schema: Some(schema), stream: Box::pin(stream), replaced })
    }

    async fn explain(&self, sql: &str, analyze: bool) -> Result<Option<PlanSummary>> {
        mysql_common::explain(self, sql, analyze).await
    }

    fn session_id(&self) -> Option<String> {
        Some(self.session_id.load(Ordering::Relaxed).to_string())
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn driver_id(&self) -> &'static str {
        "mysql"
    }

    fn sql_charset(&self) -> Option<Charset> {
        Some(self.facts.plan.sql_charset()).filter(|cs| !cs.is_utf8_like())
    }
}

impl Database for MySqlDatabase {}

/// The shared-logic surface (design doc section 4) over the text protocol: SQL encoded by
/// `facts.encode_sql` and sent as bytes, rows read as raw bytes with the column collation ids.
#[async_trait]
impl MysqlRawQuery for MySqlDatabase {
    fn facts(&self) -> &ServerFacts {
        &self.facts
    }

    async fn meta_query(&self, sql: &str) -> Result<MetaRows> {
        let bytes = self.facts.encode_sql(sql)?;
        let mut guard = self.idle_conn().await?;
        let out = query_meta(conn_mut(&mut guard)?, &bytes).await;
        if matches!(out, Err(CoreError::Connect(_))) {
            *guard = Slot::Gone;
        }
        out
    }

    async fn meta_exec(&self, sql: &str) -> Result<()> {
        let bytes = self.facts.encode_sql(sql)?;
        let mut guard = self.idle_conn().await?;
        let out = conn_mut(&mut guard)?.query_drop(bytes.as_ref()).await.map_err(map_err);
        if matches!(out, Err(CoreError::Connect(_))) {
            *guard = Slot::Gone;
        }
        out
    }

    async fn meta_query_cancellable(&self, sql: &str, cancel: CancellationToken) -> Result<MetaRows> {
        let bytes = self.facts.encode_sql(sql)?.into_owned();
        let mut guard = self.idle_conn().await?;
        let id = self.session_id.load(Ordering::Relaxed);
        let (out, lost) = {
            let conn = conn_mut(&mut guard)?;
            let fut = query_meta(conn, &bytes);
            tokio::pin!(fut);
            tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    if let Err(e) = self.kill_from_side(id).await {
                        tracing::warn!("mysql: {} failed: {e}", kill_sql(&self.facts.caps, id));
                    }
                    if self.facts.caps.kill_query {
                        // KILL QUERY interrupts the statement: read its error so the connection
                        // stays clean and usable.
                        let settled = tokio::time::timeout(KILL_TIMEOUT, &mut fut).await.is_ok();
                        (Err(CoreError::Cancelled), !settled)
                    } else {
                        // 4.1: KILL ended the whole session; the next call reconnects.
                        (Err(CoreError::Cancelled), true)
                    }
                }
                r = &mut fut => {
                    let lost = matches!(r, Err(CoreError::Connect(_)));
                    (r, lost)
                }
            }
        };
        if lost {
            *guard = Slot::Gone;
        }
        out
    }

    async fn kill_from_side(&self, target: u32) -> Result<()> {
        kill_with(&self.opts, &self.facts.caps, target).await
    }

    fn write_samples(&self) -> &WriteSamples {
        &self.write_samples
    }
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
pub(crate) fn explain_analyze_root_rows(raw: &str) -> Option<u64> {
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
        // BIT(64) with the top bit set: the bits survive (consumers reinterpret them as u64).
        assert!(matches!(ValueParser::Bit.cell(Some(&Value::Bytes(vec![0xff; 8])), &mut scratch, 0).unwrap(), Cell::I64(-1)));
        assert!(matches!(ValueParser::Bit.cell(Some(&Value::Bytes(vec![0x80, 0, 0, 0, 0, 0, 0, 0])), &mut scratch, 0).unwrap(), Cell::I64(i64::MIN)));
        assert!(matches!(ValueParser::Bit.cell(Some(&Value::UInt(u64::MAX)), &mut scratch, 0).unwrap(), Cell::I64(-1)));
        assert!(ValueParser::Bit.cell(Some(&Value::Bytes(vec![0xff; 9])), &mut scratch, 0).is_err());
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
        // 5.6.5+ with secure_auth refusing an old hash; greeting-time host errors (no SQLSTATE).
        assert!(matches!(map_err(server(1275)), CoreError::Auth(m) if m.contains(mysql_common::SECURE_AUTH_HINT)));
        let greeting_err = |code: u16| mysql_async::Error::Server(ServerError { code, message: format!("Host 'x' error {code}"), state: String::new() });
        assert!(matches!(map_err(greeting_err(1129)), CoreError::Connect(m) if m.contains("FLUSH HOSTS") && m.contains("mysqladmin flush-hosts")));
        assert!(matches!(map_err(greeting_err(1130)), CoreError::Connect(m) if m.contains(mysql_common::HOST_NOT_ALLOWED_HINT)));
        assert!(matches!(map_err(greeting_err(1105)), CoreError::Query { msg, .. } if !msg.contains("SQLSTATE")));
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
        assert_eq!(server_error_code(&map_err(server(1193))), Some(1193));
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

    /// Pass-through connections decode text columns by their own collation; everything else is
    /// left to the server (UTF-8) or is binary.
    #[test]
    fn pass_through_columns_are_transcoded_by_collation() {
        let pass = TextDecode { plan: TextPlan::PassThrough { client: "latin1", legacy: Charset::EucKr }, collations: CollationMap::builtin() };
        let text = |charset: u16| col(ColumnType::MYSQL_TYPE_VAR_STRING).with_character_set(charset);
        assert_eq!(pass.transcode(&text(8), ValueParser::Text), Some(Charset::EucKr), "CP949 stored in a latin1 column");
        assert_eq!(pass.transcode(&text(19), ValueParser::Text), Some(Charset::EucKr));
        assert_eq!(pass.transcode(&text(33), ValueParser::Text), None, "utf8 columns are UTF-8 already");
        assert_eq!(pass.transcode(&text(63), ValueParser::Text), None, "binary is never transcoded");
        assert_eq!(pass.transcode(&text(8), ValueParser::Int), None, "only text parsers");
        let blob = col(ColumnType::MYSQL_TYPE_BLOB).with_character_set(63);
        assert_eq!(column_kind(&blob).0, ColKind::Binary);
        // The data dictionary holds a latin1 client's Korean as windows-1252 characters.
        let named = col(ColumnType::MYSQL_TYPE_VAR_STRING).with_name("ÇÑ±Û".as_bytes());
        assert_eq!(pass.column_name(&named), "한글");

        let utf8 = TextDecode { plan: TextPlan::ServerUtf8 { names: "utf8mb4" }, collations: CollationMap::builtin() };
        assert_eq!(utf8.transcode(&text(45), ValueParser::Text), None);
        assert_eq!(utf8.column_name(&named), "ÇÑ±Û", "only the latin1 pass-through plan repairs names");
        let euckr_client = TextDecode { plan: TextPlan::PassThrough { client: "euckr", legacy: Charset::EucKr }, collations: CollationMap::builtin() };
        assert_eq!(euckr_client.column_name(&named), "ÇÑ±Û");
    }

    #[test]
    fn old_password_refusal_names_the_toggle() {
        match map_err(mysql_async::Error::Driver(mysql_async::DriverError::MysqlOldPasswordDisabled)) {
            CoreError::Auth(msg) => assert!(msg.contains("allow_old_password"), "{msg}"),
            other => panic!("expected AUTH, got {other:?}"),
        }
    }

    #[test]
    fn opts_follow_the_old_password_toggle() {
        let params = ConnectParams {
            driver: "mysql".into(),
            host: "db.test".into(),
            port: 3306,
            database: Some("shop".into()),
            user: Some("app".into()),
            password: Some("secret".into()),
            fields: Default::default(),
            application_name: "bulkdb-test".into(),
            connect_timeout: Duration::from_secs(5),
            ssl: SslMode::Disable,
        };
        assert!(build_opts(&params, None, false).secure_auth());
        let old = build_opts(&params, None, true);
        assert!(!old.secure_auth());
        // Set in Opts so mysql_async skips its post-connect SELECT of both variables.
        assert_eq!(old.max_allowed_packet(), Some(CLIENT_MAX_ALLOWED_PACKET));
        assert_eq!(old.wait_timeout(), Some(CLIENT_WAIT_TIMEOUT_S));

        // No TLS backend: require / verify-full are refused before any socket, prefer is fine.
        for (ssl, ok) in [(SslMode::Disable, true), (SslMode::Prefer, true), (SslMode::Require, TLS_BACKEND), (SslMode::VerifyFull, TLS_BACKEND)] {
            let r = check_tls_supported(&ConnectParams { ssl, ..params.clone() });
            assert_eq!(r.is_ok(), ok, "{ssl:?}: {r:?}");
            assert!(r.is_ok() || matches!(&r, Err(CoreError::Connect(m)) if m.contains("sslmode")), "{r:?}");
        }
    }

    #[test]
    fn write_sample_ring() {
        // `record_write_sample` delegates to `WriteSamples`, exercised without a connection.
        let samples = WriteSamples::default();
        let mut last = 0.0;
        for i in 0..15 {
            last = samples.record("j", i % 3 == 0);
        }
        // Last ten samples: i = 5..=14 -> writing at 6, 9, 12 -> 3 / 10.
        assert!((last - 0.3).abs() < 1e-6);
        samples.forget("j");
        assert_eq!(samples.record("j", true), 1.0);
    }

    #[test]
    fn value_helpers() {
        assert_eq!(value_str(Some(&Value::Date(2024, 5, 6, 0, 0, 0, 0))).as_deref(), Some("2024-05-06"));
        assert_eq!(value_str(Some(&Value::Date(2024, 5, 6, 7, 8, 9, 10))).as_deref(), Some("2024-05-06 07:08:09.000010"));
        assert_eq!(value_str(Some(&Value::Bytes(vec![0xff, b'a']))).as_deref(), Some("\u{fffd}a"));
    }
}
