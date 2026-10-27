//! PostgreSQL driver (tokio-postgres 0.7, the flagship path).
//!
//! * CSV output: `COPY (<sql>) TO STDOUT (FORMAT csv, ...)`. The server produces the final
//!   dialect; one `CopyData` message per row is coalesced into `Batch::RawCsv` chunks of
//!   `BATCH_ROWS` / `BATCH_BYTES` without decoding a single byte (`Batch::RawCsvHeader` once
//!   when a header is requested).
//! * Parquet output: `COPY (<sql>) TO STDOUT (FORMAT binary)` decoded by [`BinaryCopyDecoder`]
//!   into Arrow via `arrow_batch::ArrowBatcher`. Types without a binary decoder are cast to
//!   text server-side by wrapping the query (`SELECT ..., "col"::text AS "col" FROM (<sql>) AS
//!   _bulkdb`).
//! * Cancel: protocol-level `CancelToken::cancel_query` over the same TLS choice as the session,
//!   sent when the job's token fires *and* when a stream (or a pending statement) is dropped
//!   before the server finished it ([`CancelOnDrop`]); otherwise tokio-postgres would drain an
//!   abandoned `COPY` to its end.
//! * Session: `SET work_mem / statement_timeout / lock_timeout / max_parallel_workers_per_gather`.
//! * Probe: `server_version`, `shared_buffers` (RAM estimate x 4), `work_mem`, write privileges.
//! * EXPLAIN: `EXPLAIN (FORMAT JSON)` over the extended query protocol, which parses exactly one
//!   statement (`SELECT 1; SELECT 2` is refused by the server before anything runs), inside a
//!   transaction with `statement_timeout = 15s` and `lock_timeout = 3s` (planner-only) that
//!   always ends with `ROLLBACK`, even when the call is abandoned half way (`ExplainTxn`);
//!   `EXPLAIN ANALYZE` gets a longer statement timeout.
//! * Server NOTICE / WARNING messages are forwarded to `tracing` (they end up in the UI log).
//!
//! The catalog and monitor halves of the driver live in `catalog/postgres.rs` and
//! `monitor/postgres.rs`; they share the `Client` through `pub(crate)` fields.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use arrow_array::RecordBatch;
use async_trait::async_trait;
use bytes::{Buf, BytesMut};
use futures::stream::{self, StreamExt};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_postgres::config::SslMode as PgSslMode;
use tokio_postgres::error::DbError;
use tokio_postgres::tls::MakeTlsConnect;
use tokio_postgres::types::{FromSql, Kind, Type};
use tokio_postgres::{AsyncMessage, CancelToken, Client, Config, Connection, CopyOutStream, NoTls, Row, Socket};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use super::arrow_batch::{schema_from, ArrowBatcher, Cell, ColKind};
use super::{Batch, BoxStream, ConnectParams, CsvFormat, Database, Query, Source, SourceStream, SslMode, StreamOutput};
use crate::error::{CoreError, Result};
use crate::types::{PlanNode, PlanSummary, ServerInfo, SessionOpts, TableRef};
use crate::{BATCH_BYTES, BATCH_ROWS};

/// Statement timeout for planner-only `EXPLAIN` (spec: 15 s).
pub const EXPLAIN_TIMEOUT: &str = "15s";
/// Statement timeout for `EXPLAIN ANALYZE`, which really runs the query.
pub const EXPLAIN_ANALYZE_TIMEOUT: &str = "5min";
/// Lock timeout for every `EXPLAIN` (spec: 3 s).
pub const EXPLAIN_LOCK_TIMEOUT: &str = "3s";
/// [`EXPLAIN_TIMEOUT`] as a duration (tells EXPLAIN's own timeout from a foreign cancel).
const EXPLAIN_TIMEOUT_LIMIT: std::time::Duration = std::time::Duration::from_secs(15);
/// [`EXPLAIN_ANALYZE_TIMEOUT`] as a duration.
const EXPLAIN_ANALYZE_TIMEOUT_LIMIT: std::time::Duration = std::time::Duration::from_secs(5 * 60);

/// Days between 1970-01-01 and the PostgreSQL epoch 2000-01-01.
const PG_EPOCH_DAYS: i32 = 10_957;
/// Microseconds between 1970-01-01 and 2000-01-01.
const PG_EPOCH_MICROS: i64 = 946_684_800_000_000;
/// Binary COPY signature (11 bytes).
const BINARY_SIGNATURE: &[u8; 11] = b"PGCOPY\n\xff\r\n\0";
/// Binary COPY header: signature + flags (i32) + extension length (i32).
const BINARY_HEADER_LEN: usize = 19;

// ---------------------------------------------------------------------------------------------
// Connection
// ---------------------------------------------------------------------------------------------

/// How the session negotiated TLS; a cancel request must use the same choice.
#[derive(Clone)]
pub(crate) enum TlsChoice {
    Plain,
    Native(postgres_native_tls::MakeTlsConnector),
}

/// Protocol-level cancel for the queries of one connection (shared with running streams so a
/// stream can cancel its own `COPY` even after the `PgDatabase` has been dropped).
pub(crate) struct Canceller {
    token: CancelToken,
    tls: TlsChoice,
}

impl Canceller {
    /// Ask the server to cancel whatever this connection is running. Failures are logged, not
    /// returned: the caller ends the stream with `CoreError::Cancelled` either way.
    pub(crate) async fn cancel(&self) {
        let r = match &self.tls {
            TlsChoice::Plain => self.token.cancel_query(NoTls).await,
            TlsChoice::Native(c) => self.token.cancel_query(c.clone()).await,
        };
        match r {
            Ok(()) => debug!("sent cancel request to the server"),
            Err(e) => warn!("cancel request failed: {}", full_message(&e)),
        }
    }
}

/// Server-side cancel tied to the lifetime of one in-flight statement.
///
/// tokio-postgres keeps reading a statement's responses after the client side hangs up (a
/// dropped `CopyOutStream` or a dropped query future): the server runs the statement to the
/// end and the connection task drains every remaining byte. For an abandoned multi-GB `COPY`
/// that is hours of server work and network traffic. While armed, dropping this guard fires a
/// protocol CancelRequest in the background, which makes "drop = cancel" hold for PostgreSQL
/// the way it does for the other drivers.
///
/// Disarm it once the server has finished the statement (end of data, or a server error) so
/// that a finished statement never sends a cancel. A cancel that reaches an idle backend is
/// ignored by the server; the connections a job streams on are dedicated, so a late cancel
/// cannot hit somebody else's statement.
pub(crate) struct CancelOnDrop {
    canceller: Arc<Canceller>,
    armed: bool,
}

impl CancelOnDrop {
    pub(crate) fn new(canceller: Arc<Canceller>) -> Self {
        CancelOnDrop { canceller, armed: true }
    }

    /// The statement finished server-side; dropping the guard no longer cancels anything.
    pub(crate) fn disarm(&mut self) {
        self.armed = false;
    }

    /// Cancel now and wait until the request has been sent; the guard is disarmed afterwards.
    /// When this future is itself dropped half-way, the guard stays armed and its `Drop` sends
    /// the request instead.
    pub(crate) async fn cancel_now(&mut self) {
        self.canceller.cancel().await;
        self.armed = false;
    }
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                debug!("statement abandoned before it finished; sending a cancel request");
                let canceller = self.canceller.clone();
                handle.spawn(async move { canceller.cancel().await });
            }
            // Without a runtime the connection task is gone as well; the server ends the
            // statement as soon as it notices the closed socket.
            Err(_) => debug!("statement abandoned outside a tokio runtime; no cancel request sent"),
        }
    }
}

/// One PostgreSQL session: reads (`Source`), browses (`Catalog`) and watches (`Monitor`).
pub struct PgDatabase {
    pub(crate) client: Client,
    pub(crate) canceller: Arc<Canceller>,
    /// `pg_backend_pid()` of this session.
    pub(crate) pid: i32,
    /// `server_version_num`, e.g. `160002`.
    pub(crate) server_version_num: i32,
    /// `current_database()`; a PostgreSQL session is bound to one database.
    pub(crate) database: String,
    pub(crate) application_name: String,
    /// Serialises every statement sent on `client` (see [`PgDatabase::exclusive`]).
    ///
    /// EXPLAIN runs inside `BEGIN ... ROLLBACK` with `SET LOCAL` timeouts, and tokio-postgres
    /// executes requests in the order they were sent: without this lock, a catalog or monitor
    /// query issued concurrently on the same (shared browser) connection would run inside that
    /// transaction, under its timeouts, and fail with `25P02` when the EXPLAIN fails (or abort
    /// the EXPLAIN's transaction when it fails itself).
    pub(crate) exec_lock: tokio::sync::Mutex<()>,
    /// Per-job ring of "was the wait event ClientWrite" samples, used by the monitor.
    pub(crate) monitor_rings: Mutex<HashMap<String, VecDeque<bool>>>,
}

impl PgDatabase {
    /// Connect with the resolved parameters (tunnel already substituted, password fetched).
    ///
    /// TLS: `Disable` -> plain; `Prefer` -> TLS accepting any certificate, falling back to plain
    /// when the server refuses TLS or the handshake fails; `Require` -> TLS accepting any
    /// certificate; `VerifyFull` -> TLS with full chain + hostname verification (`sslrootcert`
    /// field adds a trusted root).
    pub async fn connect(params: &ConnectParams) -> Result<Self> {
        let mut cfg = build_config(params);
        let (client, tls) = match params.ssl {
            SslMode::Disable => {
                cfg.ssl_mode(PgSslMode::Disable);
                let client = connect_with(&cfg, NoTls).await.map_err(|e| connect_error(e, params))?;
                (client, TlsChoice::Plain)
            }
            SslMode::Prefer => {
                cfg.ssl_mode(PgSslMode::Prefer);
                let tls = make_tls(params, true)?;
                match connect_with(&cfg, tls.clone()).await {
                    Ok(client) => (client, TlsChoice::Native(tls)),
                    Err(e) if is_tls_error(&e) => {
                        warn!("TLS to {}:{} failed ({}); retrying without TLS (sslmode=prefer)", params.host, params.port, full_message(&e));
                        cfg.ssl_mode(PgSslMode::Disable);
                        let client = connect_with(&cfg, NoTls).await.map_err(|e| connect_error(e, params))?;
                        (client, TlsChoice::Plain)
                    }
                    Err(e) => return Err(connect_error(e, params)),
                }
            }
            SslMode::Require => {
                cfg.ssl_mode(PgSslMode::Require);
                let tls = make_tls(params, true)?;
                let client = connect_with(&cfg, tls.clone()).await.map_err(|e| connect_error(e, params))?;
                (client, TlsChoice::Native(tls))
            }
            SslMode::VerifyFull => {
                cfg.ssl_mode(PgSslMode::Require);
                let tls = make_tls(params, false)?;
                let client = connect_with(&cfg, tls.clone()).await.map_err(|e| connect_error(e, params))?;
                (client, TlsChoice::Native(tls))
            }
        };
        let canceller = Arc::new(Canceller { token: client.cancel_token(), tls });
        let row =
            client.query_one("SELECT pg_backend_pid()::int4, current_setting('server_version_num')::int4, current_database()::text", &[]).await.map_err(map_pg_error)?;
        let pid: i32 = row.try_get(0).map_err(map_pg_error)?;
        let server_version_num: i32 = row.try_get(1).map_err(map_pg_error)?;
        let database: String = row.try_get(2).map_err(map_pg_error)?;
        debug!("connected to {}:{} db={} pid={} server={}", params.host, params.port, database, pid, server_version_num);
        Ok(PgDatabase {
            client,
            canceller,
            pid,
            server_version_num,
            database,
            application_name: params.application_name.clone(),
            exec_lock: tokio::sync::Mutex::new(()),
            monitor_rings: Mutex::new(HashMap::new()),
        })
    }

    /// Backend pid of this session.
    pub fn backend_pid(&self) -> i32 {
        self.pid
    }

    /// `server_version_num` (e.g. `160002`), for feature gates in catalog / monitor queries.
    pub fn server_version_num(&self) -> i32 {
        self.server_version_num
    }

    /// Database this session is bound to.
    pub fn database_name(&self) -> &str {
        &self.database
    }

    /// `application_name` this session connected with (`bulkdb-<job id>`, `bulkdb-ui`, ...).
    pub fn application_name(&self) -> &str {
        &self.application_name
    }

    /// Exclusive use of this session's client until the guard is dropped.
    ///
    /// Every `Source`, `Catalog` and `Monitor` method of `PgDatabase` sends its statements
    /// while holding this guard, so nothing can be queued between EXPLAIN's `BEGIN` and its
    /// `ROLLBACK` (requests on one connection run in the order they were sent). Callers that
    /// compose several statements take the guard once and call unlocked helpers: the lock is
    /// not reentrant.
    pub(crate) async fn exclusive(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.exec_lock.lock().await
    }

    /// [`PgDatabase::exclusive`] for calls that take a cancellation token: returns `Cancelled`
    /// when `cancel` fires while another statement of this session still holds the lock.
    pub(crate) async fn exclusive_or_cancelled(&self, cancel: &CancellationToken) -> Result<tokio::sync::MutexGuard<'_, ()>> {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(CoreError::Cancelled),
            guard = self.exec_lock.lock() => Ok(guard),
        }
    }

    /// Send a protocol-level cancel for the query currently running on this session.
    pub async fn cancel_query(&self) {
        self.canceller.cancel().await;
    }

    /// Await `fut`, cancelling the server-side query and returning `Cancelled` when `cancel`
    /// fires first.
    ///
    /// The server-side query is also cancelled when this future is dropped before the server
    /// answered (an outer `select!` on the same token, a timeout), so an abandoned `COPY` or
    /// `count(*)` does not keep running.
    pub(crate) async fn with_cancel<T, F>(&self, fut: F, cancel: &CancellationToken) -> Result<T>
    where
        F: Future<Output = std::result::Result<T, tokio_postgres::Error>>,
    {
        let mut guard = CancelOnDrop::new(self.canceller.clone());
        let r = tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                guard.cancel_now().await;
                Err(CoreError::Cancelled)
            }
            r = fut => r.map_err(map_pg_error),
        };
        guard.disarm();
        r
    }
}

fn build_config(params: &ConnectParams) -> Config {
    let mut cfg = Config::new();
    cfg.host(&params.host);
    cfg.port(if params.port == 0 { 5432 } else { params.port });
    if let Some(u) = params.user.as_deref().filter(|s| !s.is_empty()) {
        cfg.user(u);
    }
    if let Some(p) = params.password.as_deref() {
        cfg.password(p);
    }
    if let Some(db) = params.database.as_deref().filter(|s| !s.is_empty()) {
        cfg.dbname(db);
    }
    cfg.application_name(&params.application_name);
    cfg.connect_timeout(params.connect_timeout);
    cfg.options("-c client_encoding=UTF8");
    cfg.keepalives(true);
    cfg
}

fn make_tls(params: &ConnectParams, accept_invalid: bool) -> Result<postgres_native_tls::MakeTlsConnector> {
    let mut b = native_tls::TlsConnector::builder();
    if accept_invalid {
        b.danger_accept_invalid_certs(true);
        b.danger_accept_invalid_hostnames(true);
    }
    if let Some(path) = params.field("sslrootcert") {
        let pem = std::fs::read(path).map_err(|e| CoreError::Connect(format!("cannot read sslrootcert '{path}': {e}")))?;
        let cert = native_tls::Certificate::from_pem(&pem).map_err(|e| CoreError::Connect(format!("sslrootcert '{path}' is not a PEM certificate: {e}")))?;
        b.add_root_certificate(cert);
    }
    let connector = b.build().map_err(|e| CoreError::Connect(format!("TLS setup failed: {e}")))?;
    Ok(postgres_native_tls::MakeTlsConnector::new(connector))
}

/// Connect and spawn the connection driver (which also forwards server notices).
async fn connect_with<T>(cfg: &Config, tls: T) -> std::result::Result<Client, tokio_postgres::Error>
where
    T: MakeTlsConnect<Socket>,
    T::Stream: Send + 'static,
{
    let (client, conn) = cfg.connect(tls).await?;
    spawn_connection(conn);
    Ok(client)
}

/// Drive the connection on the runtime; NOTICE / WARNING messages go to `tracing`.
fn spawn_connection<S, T>(mut conn: Connection<S, T>)
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    T: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        loop {
            match futures::future::poll_fn(|cx| conn.poll_message(cx)).await {
                Some(Ok(AsyncMessage::Notice(n))) => log_notice(&n),
                Some(Ok(_)) => {}
                Some(Err(e)) => {
                    if !e.is_closed() {
                        debug!("postgres connection ended: {}", full_message(&e));
                    }
                    break;
                }
                None => break,
            }
        }
    });
}

fn log_notice(n: &DbError) {
    let severity = n.severity();
    let mut msg = n.message().to_string();
    if let Some(d) = n.detail() {
        msg.push_str(" (");
        msg.push_str(d);
        msg.push(')');
    }
    if let Some(h) = n.hint() {
        msg.push_str(" hint: ");
        msg.push_str(h);
    }
    match severity {
        "WARNING" => warn!("server {severity}: {msg}"),
        "NOTICE" | "INFO" | "LOG" => info!("server {severity}: {msg}"),
        _ => debug!("server {severity}: {msg}"),
    }
}

fn is_tls_error(e: &tokio_postgres::Error) -> bool {
    if e.as_db_error().is_some() {
        return false;
    }
    if e.to_string().contains("TLS") {
        return true;
    }
    let mut src = std::error::Error::source(e);
    while let Some(err) = src {
        if err.downcast_ref::<native_tls::Error>().is_some() {
            return true;
        }
        src = err.source();
    }
    false
}

fn connect_error(e: tokio_postgres::Error, params: &ConnectParams) -> CoreError {
    match map_pg_error(e) {
        CoreError::Connect(m) => CoreError::Connect(format!("{}:{}: {m}", params.host, params.port)),
        other => other,
    }
}

// ---------------------------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------------------------

/// Display string plus the whole `source()` chain (tokio-postgres puts the interesting part in
/// the cause).
pub fn full_message(e: &tokio_postgres::Error) -> String {
    let mut s = e.to_string();
    let mut src = std::error::Error::source(e);
    while let Some(c) = src {
        s.push_str(": ");
        s.push_str(&c.to_string());
        src = c.source();
    }
    s
}

fn db_message(db: &DbError) -> String {
    let mut s = db.message().to_string();
    if let Some(d) = db.detail() {
        s.push_str("\nDETAIL: ");
        s.push_str(d);
    }
    if let Some(h) = db.hint() {
        s.push_str("\nHINT: ");
        s.push_str(h);
    }
    s
}

/// SQLSTATE `query_canceled`: raised both for a cancel request and for `statement_timeout`.
pub const SQLSTATE_QUERY_CANCELED: &str = "57014";

/// Map a server SQLSTATE to the `CoreError` family the UI branches on.
///
/// `57014` with "statement timeout" in the message -> Timeout; any other `57014` (a cancel
/// request this job did not send, e.g. `pg_cancel_backend` by a DBA) -> Query with the code
/// kept, because a cancel the user asked for never reaches this function: the job's own
/// cancellation token makes the stream return `Cancelled` first. `57P01/57P02/57P03` and class
/// `08` -> Connect, class `28` -> Auth, `55P03` / `40P01` (lock not available / deadlock) ->
/// Timeout, everything else -> Query.
///
/// The server translates messages (`lc_messages`), so the "statement timeout" test only works
/// for English servers; the streaming path additionally classifies a `57014` that arrives after
/// the session's `statement_timeout` has elapsed as Timeout (see `StatementClock`).
pub fn map_sqlstate(code: &str, msg: String) -> CoreError {
    match code {
        SQLSTATE_QUERY_CANCELED if msg.contains("statement timeout") => CoreError::Timeout(msg),
        "57P01" | "57P02" | "57P03" => CoreError::Connect(msg),
        "55P03" | "40P01" => CoreError::Timeout(msg),
        c if c.starts_with("08") => CoreError::Connect(msg),
        c if c.starts_with("28") => CoreError::Auth(msg),
        _ => CoreError::Query { code: Some(code.to_string()), msg },
    }
}

/// Map any tokio-postgres error. Server errors go through [`map_sqlstate`] with
/// message + detail + hint; client-side errors become Connect / Auth / Timeout / Internal.
pub fn map_pg_error(e: tokio_postgres::Error) -> CoreError {
    if let Some(db) = e.as_db_error() {
        return map_sqlstate(db.code().code(), db_message(db));
    }
    let msg = full_message(&e);
    let kind = e.to_string();
    if e.is_closed() {
        CoreError::Connect(msg)
    } else if kind.contains("authentication") {
        CoreError::Auth(msg)
    } else if kind.contains("timeout") {
        CoreError::Timeout(msg)
    } else if kind.contains("serializing")
        || kind.contains("deserializing")
        || kind.contains("column")
        || kind.contains("parameters")
        || kind.contains("rows")
        || kind.contains("parsing")
        || kind.contains("encoding")
    {
        CoreError::Internal(msg)
    } else {
        CoreError::Connect(msg)
    }
}

/// When a streaming statement started and which `statement_timeout` the session runs with.
///
/// PostgreSQL reports `statement_timeout` and a cancel request with the same SQLSTATE `57014`,
/// and only the (translated) message tells them apart. The pipeline applies `SessionOpts` to
/// the session before it streams, so a `57014` that arrives once that timeout has elapsed is a
/// timeout whatever the server's language; earlier ones stay `Query` errors.
#[derive(Debug, Clone, Copy)]
pub(crate) struct StatementClock {
    timeout: Option<std::time::Duration>,
    started: Instant,
}

impl StatementClock {
    /// Start the clock now (call it before sending the statement, so the client-side elapsed
    /// time is never shorter than the server's).
    pub(crate) fn start(opts: &SessionOpts) -> Self {
        StatementClock { timeout: opts.statement_timeout_ms.filter(|&ms| ms > 0).map(std::time::Duration::from_millis), started: Instant::now() }
    }

    /// Map a tokio-postgres error of this statement.
    pub(crate) fn map(&self, e: tokio_postgres::Error) -> CoreError {
        self.classify(map_pg_error(e), self.started.elapsed())
    }

    /// Reclassify an already mapped error of this statement.
    pub(crate) fn refine(&self, err: CoreError) -> CoreError {
        self.classify(err, self.started.elapsed())
    }

    fn classify(&self, err: CoreError, elapsed: std::time::Duration) -> CoreError {
        match (err, self.timeout) {
            (CoreError::Query { code: Some(code), msg }, Some(limit)) if code == SQLSTATE_QUERY_CANCELED && elapsed >= limit => {
                CoreError::Timeout(format!("statement_timeout ({} ms) exceeded: {msg}", limit.as_millis()))
            }
            (err, _) => err,
        }
    }
}

// ---------------------------------------------------------------------------------------------
// SQL helpers
// ---------------------------------------------------------------------------------------------

/// Double-quote an identifier (`"` doubled).
pub fn quote_ident(ident: &str) -> String {
    format!("\"{}\"", ident.replace('"', "\"\""))
}

/// Quote a string literal the way `quote_literal()` does: `'` doubled; when the value contains
/// a backslash the `E'...'` form is used with backslashes doubled, so the result is correct
/// regardless of `standard_conforming_strings`.
pub fn quote_literal(s: &str) -> String {
    if s.contains('\\') {
        format!("E'{}'", s.replace('\\', "\\\\").replace('\'', "''"))
    } else {
        format!("'{}'", s.replace('\'', "''"))
    }
}

/// Strip surrounding whitespace and trailing semicolons so the statement can be embedded in
/// `COPY (...)` / `FROM (...)`.
pub fn trim_sql(sql: &str) -> &str {
    let mut s = sql.trim();
    while let Some(t) = s.strip_suffix(';') {
        s = t.trim_end();
    }
    s
}

/// `COPY (<sql>) TO STDOUT (FORMAT csv, ...)` for the requested dialect.
pub fn copy_csv_sql(sql: &str, fmt: &CsvFormat) -> String {
    let delim = (fmt.delimiter as char).to_string();
    format!(
        "COPY (\n{sql}\n) TO STDOUT (FORMAT csv, HEADER {}, DELIMITER {}, NULL {}, QUOTE '\"', ESCAPE '\"')",
        fmt.header,
        quote_literal(&delim),
        quote_literal(&fmt.null_as)
    )
}

/// `COPY (<sql>) TO STDOUT (FORMAT binary)`.
pub fn copy_binary_sql(sql: &str) -> String {
    format!("COPY (\n{sql}\n) TO STDOUT (FORMAT binary)")
}

/// `SET` statements for the session options (values quoted as literals; `SET` takes no bind
/// parameters). Always ends with `SET client_encoding = 'UTF8'`.
pub fn session_sql(opts: &SessionOpts) -> Vec<String> {
    let mut v = Vec::new();
    if let Some(w) = opts.work_mem.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        v.push(format!("SET work_mem = {}", quote_literal(w)));
    }
    if let Some(ms) = opts.statement_timeout_ms {
        v.push(format!("SET statement_timeout = '{ms}ms'"));
    }
    if let Some(ms) = opts.lock_timeout_ms {
        v.push(format!("SET lock_timeout = '{ms}ms'"));
    }
    if let Some(n) = opts.max_parallel_workers {
        v.push(format!("SET max_parallel_workers_per_gather = '{n}'"));
    }
    v.push("SET client_encoding = 'UTF8'".to_string());
    v
}

// ---------------------------------------------------------------------------------------------
// Type planning for the binary path
// ---------------------------------------------------------------------------------------------

/// Encoding of a field in `COPY ... (FORMAT binary)` after any server-side cast.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireType {
    Bool,
    Int2,
    Int4,
    Int8,
    Oid,
    Float4,
    Float8,
    Text,
    Bytea,
    Date,
    Time,
    Timestamp,
    Numeric,
    Uuid,
}

/// How one result column is fetched and stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PgColumnPlan {
    pub name: String,
    /// Arrow column kind.
    pub kind: ColKind,
    /// Binary encoding expected on the wire.
    pub wire: WireType,
    /// `true` when the column must be cast to `text` in the wrapping query.
    pub cast: bool,
    /// PostgreSQL type name, for logs.
    pub pg_type: String,
}

/// Decode a `numeric` type modifier into `(precision, scale)`; `None` when unconstrained.
pub fn numeric_typmod(typmod: i32) -> Option<(u16, i16)> {
    if typmod < 4 {
        return None;
    }
    let t = typmod - 4;
    let precision = ((t >> 16) & 0xffff) as u16;
    // PostgreSQL 15+ stores the scale as an 11-bit two's complement value.
    let raw = (t & 0x7ff) as i16;
    let scale = if raw & 0x400 != 0 { raw - 0x800 } else { raw };
    Some((precision, scale))
}

fn unwrap_domain(ty: &Type) -> &Type {
    let mut t = ty;
    while let Kind::Domain(inner) = t.kind() {
        t = inner;
    }
    t
}

/// Choose the Arrow kind, the wire encoding and whether a `::text` cast is needed for one
/// result column.
pub fn plan_column(name: &str, ty: &Type, typmod: i32) -> PgColumnPlan {
    let base = unwrap_domain(ty);
    let (kind, wire, cast) = match *base {
        Type::BOOL => (ColKind::Bool, WireType::Bool, false),
        Type::INT2 => (ColKind::Int64, WireType::Int2, false),
        Type::INT4 => (ColKind::Int64, WireType::Int4, false),
        Type::INT8 => (ColKind::Int64, WireType::Int8, false),
        Type::OID => (ColKind::Int64, WireType::Oid, false),
        Type::FLOAT4 => (ColKind::Float64, WireType::Float4, false),
        Type::FLOAT8 => (ColKind::Float64, WireType::Float8, false),
        Type::NUMERIC => match numeric_typmod(typmod) {
            Some((p, s)) if (1..=38).contains(&p) && s >= 0 && s <= p as i16 => (ColKind::Decimal { precision: p as u8, scale: s as i8 }, WireType::Numeric, false),
            _ => (ColKind::Utf8, WireType::Text, true),
        },
        Type::TEXT | Type::VARCHAR | Type::BPCHAR | Type::NAME => (ColKind::Utf8, WireType::Text, false),
        Type::BYTEA => (ColKind::Binary, WireType::Bytea, false),
        Type::DATE => (ColKind::Date, WireType::Date, false),
        Type::TIME => (ColKind::Time, WireType::Time, false),
        Type::TIMESTAMP => (ColKind::Timestamp, WireType::Timestamp, false),
        Type::TIMESTAMPTZ => (ColKind::TimestampTz, WireType::Timestamp, false),
        Type::UUID => (ColKind::Utf8, WireType::Uuid, false),
        // "char", unknown, json/jsonb, xml, inet/cidr/macaddr, interval, money, bit, timetz,
        // arrays, ranges, enums, composites, citext, ... : let the server render text.
        _ => (ColKind::Utf8, WireType::Text, true),
    };
    PgColumnPlan { name: name.to_string(), kind, wire, cast, pg_type: ty.name().to_string() }
}

/// Wrap `sql` so that every column marked `cast` is rendered as text. Returns `None` when no
/// cast is needed. Unique names are referenced by name; otherwise the subquery gets positional
/// aliases (`_c1`, `_c2`, ...) and the outer select aliases them back to the original names.
pub fn wrap_query(sql: &str, cols: &[PgColumnPlan]) -> Option<String> {
    if !cols.iter().any(|c| c.cast) {
        return None;
    }
    let mut seen = std::collections::HashSet::new();
    let unique = cols.iter().all(|c| !c.name.is_empty() && seen.insert(c.name.as_str()));
    let list: Vec<String> = if unique {
        cols.iter()
            .map(|c| {
                let q = quote_ident(&c.name);
                if c.cast {
                    format!("{q}::text AS {q}")
                } else {
                    q
                }
            })
            .collect()
    } else {
        cols.iter()
            .enumerate()
            .map(|(i, c)| {
                let a = format!("_c{}", i + 1);
                let q = quote_ident(&c.name);
                if c.cast {
                    format!("{a}::text AS {q}")
                } else {
                    format!("{a} AS {q}")
                }
            })
            .collect()
    };
    let select = list.join(", ");
    if unique {
        Some(format!("SELECT {select} FROM (\n{sql}\n) AS _bulkdb"))
    } else {
        let aliases: Vec<String> = (1..=cols.len()).map(|i| format!("_c{i}")).collect();
        Some(format!("SELECT {select} FROM (\n{sql}\n) AS _bulkdb({})", aliases.join(", ")))
    }
}

// ---------------------------------------------------------------------------------------------
// Binary COPY decoder
// ---------------------------------------------------------------------------------------------

const NUMERIC_POS: u16 = 0x0000;
const NUMERIC_NEG: u16 = 0x4000;
const NUMERIC_NAN: u16 = 0xC000;
const NUMERIC_PINF: u16 = 0xD000;
const NUMERIC_NINF: u16 = 0xF000;

/// A `numeric` value in its binary wire layout (base-10000 digits).
struct NumericView<'a> {
    ndigits: usize,
    weight: i32,
    sign: u16,
    dscale: u16,
    digits: &'a [u8],
}

impl NumericView<'_> {
    fn parse(data: &[u8]) -> Option<NumericView<'_>> {
        if data.len() < 8 {
            return None;
        }
        let ndigits = u16::from_be_bytes([data[0], data[1]]) as usize;
        let weight = i16::from_be_bytes([data[2], data[3]]) as i32;
        let sign = u16::from_be_bytes([data[4], data[5]]);
        let dscale = u16::from_be_bytes([data[6], data[7]]);
        let digits = data.get(8..8 + ndigits * 2)?;
        Some(NumericView { ndigits, weight, sign, dscale, digits })
    }

    fn digit(&self, i: usize) -> i64 {
        if i >= self.ndigits {
            0
        } else {
            u16::from_be_bytes([self.digits[2 * i], self.digits[2 * i + 1]]) as i64
        }
    }

    fn is_negative(&self) -> bool {
        self.sign == NUMERIC_NEG
    }

    fn is_finite(&self) -> bool {
        matches!(self.sign, NUMERIC_POS | NUMERIC_NEG)
    }

    /// Scaled integer `value x 10^scale`, rounded half away from zero when the value has more
    /// fractional digits than `scale`. `None` on overflow or when not finite.
    fn to_i128(&self, scale: i8) -> Option<i128> {
        if !self.is_finite() {
            return None;
        }
        let mut n: i128 = 0;
        for i in 0..self.ndigits {
            n = n.checked_mul(10_000)?.checked_add(self.digit(i) as i128)?;
        }
        let exp = 4 * (self.weight + 1 - self.ndigits as i32) + scale as i32;
        let v = if exp >= 0 {
            if n == 0 {
                0
            } else {
                n.checked_mul(10i128.checked_pow(u32::try_from(exp).ok()?)?)?
            }
        } else {
            let e = u32::try_from(-exp).ok()?;
            if e > 38 {
                0
            } else {
                let d = 10i128.pow(e);
                let q = n / d;
                let r = n % d;
                if r * 2 >= d {
                    q + 1
                } else {
                    q
                }
            }
        };
        Some(if self.is_negative() { -v } else { v })
    }

    /// Render the value exactly as PostgreSQL's text output does (`-123.4500`, `NaN`, ...).
    fn write_text(&self, out: &mut String) {
        use std::fmt::Write;
        out.clear();
        match self.sign {
            NUMERIC_NAN => {
                out.push_str("NaN");
                return;
            }
            NUMERIC_PINF => {
                out.push_str("Infinity");
                return;
            }
            NUMERIC_NINF => {
                out.push_str("-Infinity");
                return;
            }
            _ => {}
        }
        if self.is_negative() {
            out.push('-');
        }
        if self.weight < 0 {
            out.push('0');
        } else {
            for i in 0..=self.weight {
                let d = self.digit(i as usize);
                if i == 0 {
                    let _ = write!(out, "{d}");
                } else {
                    let _ = write!(out, "{d:04}");
                }
            }
        }
        let dscale = self.dscale as usize;
        if dscale > 0 {
            out.push('.');
            let mut frac = String::with_capacity(dscale + 4);
            let mut i = self.weight + 1;
            while frac.len() < dscale {
                let d = if i < 0 { 0 } else { self.digit(i as usize) };
                let _ = write!(frac, "{d:04}");
                i += 1;
            }
            frac.truncate(dscale);
            out.push_str(&frac);
        }
    }
}

fn rd_i16(buf: &[u8], pos: usize) -> Option<i16> {
    buf.get(pos..pos + 2).map(|b| i16::from_be_bytes([b[0], b[1]]))
}

fn rd_i32(buf: &[u8], pos: usize) -> Option<i32> {
    buf.get(pos..pos + 4).map(|b| i32::from_be_bytes([b[0], b[1], b[2], b[3]]))
}

fn field_len_error(col: usize, wire: WireType, expected: usize, got: usize) -> CoreError {
    CoreError::Internal(format!("binary COPY: column {col} ({wire:?}) expected {expected} bytes, got {got}"))
}

/// Decode one non-NULL field and push it into the batcher.
fn push_field(batcher: &mut ArrowBatcher, col: usize, wire: WireType, kind: ColKind, data: &[u8], scratch: &mut String) -> Result<()> {
    let need = |n: usize| if data.len() == n { Ok(()) } else { Err(field_len_error(col, wire, n, data.len())) };
    match wire {
        WireType::Bool => {
            need(1)?;
            batcher.push(Cell::Bool(data[0] != 0))
        }
        WireType::Int2 => {
            need(2)?;
            batcher.push(Cell::I64(i16::from_be_bytes([data[0], data[1]]) as i64))
        }
        WireType::Int4 => {
            need(4)?;
            batcher.push(Cell::I64(i32::from_be_bytes([data[0], data[1], data[2], data[3]]) as i64))
        }
        WireType::Oid => {
            need(4)?;
            batcher.push(Cell::I64(u32::from_be_bytes([data[0], data[1], data[2], data[3]]) as i64))
        }
        WireType::Int8 => {
            need(8)?;
            batcher.push(Cell::I64(i64::from_be_bytes(data.try_into().expect("8 bytes"))))
        }
        WireType::Float4 => {
            need(4)?;
            batcher.push(Cell::F64(f32::from_be_bytes([data[0], data[1], data[2], data[3]]) as f64))
        }
        WireType::Float8 => {
            need(8)?;
            batcher.push(Cell::F64(f64::from_be_bytes(data.try_into().expect("8 bytes"))))
        }
        WireType::Text => match std::str::from_utf8(data) {
            Ok(s) => batcher.push(Cell::Str(s)),
            Err(_) => batcher.push(Cell::Bytes(data)),
        },
        WireType::Bytea => batcher.push(Cell::Bytes(data)),
        WireType::Date => {
            need(4)?;
            let d = i32::from_be_bytes([data[0], data[1], data[2], data[3]]);
            match d.checked_add(PG_EPOCH_DAYS) {
                // i32::MAX / i32::MIN are +/-infinity, which Arrow cannot represent.
                Some(v) if d != i32::MAX && d != i32::MIN => batcher.push(Cell::Date(v)),
                _ => batcher.push(Cell::Null),
            }
        }
        WireType::Time => {
            need(8)?;
            batcher.push(Cell::Time(i64::from_be_bytes(data.try_into().expect("8 bytes"))))
        }
        WireType::Timestamp => {
            need(8)?;
            let t = i64::from_be_bytes(data.try_into().expect("8 bytes"));
            match t.checked_add(PG_EPOCH_MICROS) {
                Some(v) if t != i64::MAX && t != i64::MIN => batcher.push(Cell::Timestamp(v)),
                _ => batcher.push(Cell::Null),
            }
        }
        WireType::Numeric => {
            let n = NumericView::parse(data).ok_or_else(|| CoreError::Internal(format!("binary COPY: column {col}: malformed numeric ({} bytes)", data.len())))?;
            match kind {
                ColKind::Decimal { precision, scale } => {
                    if !n.is_finite() {
                        return batcher.push(Cell::Null);
                    }
                    match n.to_i128(scale) {
                        Some(v) => batcher.push(Cell::Decimal(v)),
                        None => {
                            n.write_text(scratch);
                            Err(CoreError::Query {
                                code: None,
                                msg: format!("column {col}: numeric value {scratch} does not fit decimal({precision},{scale}); cast the column to text"),
                            })
                        }
                    }
                }
                _ => {
                    n.write_text(scratch);
                    batcher.push(Cell::Str(scratch))
                }
            }
        }
        WireType::Uuid => {
            need(16)?;
            let u = uuid::Uuid::from_bytes(data.try_into().expect("16 bytes"));
            scratch.clear();
            scratch.push_str(u.hyphenated().encode_lower(&mut uuid::Uuid::encode_buffer()));
            batcher.push(Cell::Str(scratch))
        }
    }
}

/// Incremental decoder for `COPY ... TO STDOUT (FORMAT binary)`.
///
/// Bytes may arrive in any framing (one `CopyData` per row in practice, but partial tuples are
/// buffered), so [`BinaryCopyDecoder::feed`] can be called with whatever the stream yields.
/// Complete tuples are pushed into the `ArrowBatcher`; batches are cut when the batcher says
/// it is full and the remainder is flushed when the trailer (`-1` field count) arrives.
pub(crate) struct BinaryCopyDecoder {
    cols: Vec<(WireType, ColKind)>,
    buf: BytesMut,
    header_done: bool,
    has_oids: bool,
    finished: bool,
    rows: u64,
    scratch: String,
}

impl BinaryCopyDecoder {
    pub(crate) fn new(cols: Vec<(WireType, ColKind)>) -> Self {
        BinaryCopyDecoder {
            cols,
            buf: BytesMut::with_capacity(64 * 1024),
            header_done: false,
            has_oids: false,
            finished: false,
            rows: 0,
            scratch: String::with_capacity(64),
        }
    }

    /// `true` once the trailer has been seen.
    pub(crate) fn finished(&self) -> bool {
        self.finished
    }

    /// Tuples decoded so far.
    pub(crate) fn rows(&self) -> u64 {
        self.rows
    }

    /// Append `data`, decode every complete tuple into `batcher` and return the batches cut.
    pub(crate) fn feed(&mut self, data: &[u8], batcher: &mut ArrowBatcher) -> Result<Vec<RecordBatch>> {
        let mut out = Vec::new();
        if self.finished {
            return Ok(out);
        }
        self.buf.extend_from_slice(data);
        let mut pos = 0usize;
        if !self.header_done {
            if self.buf.len() < BINARY_HEADER_LEN {
                return Ok(out);
            }
            if &self.buf[..BINARY_SIGNATURE.len()] != BINARY_SIGNATURE {
                return Err(CoreError::Internal("binary COPY: bad signature (not a PGCOPY stream)".into()));
            }
            let flags = rd_i32(&self.buf, 11).unwrap_or(0);
            let ext = rd_i32(&self.buf, 15).unwrap_or(0);
            let ext = usize::try_from(ext).map_err(|_| CoreError::Internal("binary COPY: negative header extension length".into()))?;
            if self.buf.len() < BINARY_HEADER_LEN + ext {
                return Ok(out);
            }
            self.has_oids = flags & (1 << 16) != 0;
            self.header_done = true;
            pos = BINARY_HEADER_LEN + ext;
        }
        let Self { cols, buf, scratch, .. } = self;
        let oid_skip = if self.has_oids { 4 } else { 0 };
        while let Some(nfields) = rd_i16(buf, pos) {
            if nfields == -1 {
                self.finished = true;
                pos += 2;
                if let Some(b) = batcher.flush()? {
                    out.push(b);
                }
                break;
            }
            if nfields < 0 || nfields as usize != cols.len() {
                return Err(CoreError::Internal(format!("binary COPY: tuple has {nfields} fields, expected {}", cols.len())));
            }
            // First pass: is the whole tuple buffered?
            let mut p = pos + 2 + oid_skip;
            let mut complete = buf.len() >= p;
            if complete {
                for _ in 0..nfields {
                    let Some(len) = rd_i32(buf, p) else {
                        complete = false;
                        break;
                    };
                    p += 4;
                    if len > 0 {
                        let len = len as usize;
                        if buf.len() < p + len {
                            complete = false;
                            break;
                        }
                        p += len;
                    } else if len < -1 {
                        return Err(CoreError::Internal(format!("binary COPY: invalid field length {len}")));
                    }
                }
            }
            if !complete {
                break;
            }
            // Second pass: decode.
            let mut q = pos + 2 + oid_skip;
            for (col, (wire, kind)) in cols.iter().enumerate() {
                let len = rd_i32(buf, q).unwrap_or(-1);
                q += 4;
                if len == -1 {
                    batcher.push(Cell::Null)?;
                } else {
                    let len = len as usize;
                    push_field(batcher, col, *wire, *kind, &buf[q..q + len], scratch)?;
                    q += len;
                }
            }
            self.rows += 1;
            if batcher.end_row()? {
                if let Some(b) = batcher.flush()? {
                    out.push(b);
                }
            }
            pos = p;
        }
        buf.advance(pos);
        Ok(out)
    }
}

// ---------------------------------------------------------------------------------------------
// Streams
// ---------------------------------------------------------------------------------------------

/// Append one COPY CSV message to `buf`, converting the trailing `\n` to `\r\n` when asked.
fn append_csv_line(buf: &mut BytesMut, msg: &[u8], crlf: bool) {
    if crlf {
        if let Some(body) = msg.strip_suffix(b"\n") {
            if body.ends_with(b"\r") {
                buf.extend_from_slice(msg);
            } else {
                buf.extend_from_slice(body);
                buf.extend_from_slice(b"\r\n");
            }
            return;
        }
    }
    buf.extend_from_slice(msg);
}

/// Coalesces COPY CSV messages (one row each) into `Batch::RawCsv` chunks of at most
/// `BATCH_ROWS` rows and `BATCH_BYTES` bytes.
///
/// The byte limit is checked *before* a row is appended, so the buffer never outgrows the
/// `BATCH_BYTES` it was allocated with: no regrowth to 16 MB (which `freeze()` would keep for
/// every queued batch) and no extra 8 MB copy per batch. Only a single row larger than
/// `BATCH_BYTES` ends up in a batch of its own that exceeds the limit.
struct CsvBatcher {
    buf: BytesMut,
    rows: u64,
    total_rows: u64,
    crlf: bool,
}

impl CsvBatcher {
    fn new(crlf: bool) -> Self {
        CsvBatcher { buf: BytesMut::with_capacity(BATCH_BYTES), rows: 0, total_rows: 0, crlf }
    }

    /// Add one row; returns a batch when one is complete.
    fn push(&mut self, msg: &[u8]) -> Option<Batch> {
        // `append_csv_line` adds at most one byte (`\n` -> `\r\n`).
        let grow = msg.len() + usize::from(self.crlf);
        if self.rows > 0 && self.buf.len() + grow > BATCH_BYTES {
            // The row does not fit: ship what we have and start the next batch with it. A row
            // that alone exceeds the limit is shipped by the next push (or `finish`) on its own.
            let full = self.take();
            append_csv_line(&mut self.buf, msg, self.crlf);
            self.rows = 1;
            return Some(full);
        }
        append_csv_line(&mut self.buf, msg, self.crlf);
        self.rows += 1;
        if self.rows >= BATCH_ROWS as u64 || self.buf.len() >= BATCH_BYTES {
            return Some(self.take());
        }
        None
    }

    /// The partial batch left at end of data, if any.
    fn finish(&mut self) -> Option<Batch> {
        (self.rows > 0).then(|| self.take())
    }

    fn take(&mut self) -> Batch {
        let data = std::mem::replace(&mut self.buf, BytesMut::with_capacity(BATCH_BYTES)).freeze();
        let rows = self.rows;
        self.total_rows += rows;
        self.rows = 0;
        Batch::RawCsv { data, rows }
    }
}

struct CsvState {
    copy: Pin<Box<CopyOutStream>>,
    batcher: CsvBatcher,
    expect_header: bool,
    cancel: CancellationToken,
    /// Armed until the server has finished the `COPY`: dropping the stream earlier (job cancel,
    /// sink failure) sends a CancelRequest instead of letting the server run to the end.
    server: CancelOnDrop,
    clock: StatementClock,
    /// The stream has yielded its last item.
    done: bool,
}

fn csv_stream(state: CsvState) -> BoxStream<'static, Result<Batch>> {
    Box::pin(stream::unfold(state, |mut st| async move {
        if st.done {
            return None;
        }
        loop {
            let next = tokio::select! {
                biased;
                _ = st.cancel.cancelled() => {
                    st.done = true;
                    st.server.cancel_now().await;
                    return Some((Err(CoreError::Cancelled), st));
                }
                m = st.copy.next() => m,
            };
            match next {
                Some(Ok(msg)) => {
                    if st.expect_header {
                        st.expect_header = false;
                        let mut h = BytesMut::with_capacity(msg.len() + 1);
                        append_csv_line(&mut h, &msg, st.batcher.crlf);
                        return Some((Ok(Batch::RawCsvHeader(h.freeze())), st));
                    }
                    if let Some(b) = st.batcher.push(&msg) {
                        return Some((Ok(b), st));
                    }
                }
                Some(Err(e)) => {
                    st.done = true;
                    if e.as_db_error().is_some() {
                        // The server reported the error, so it has already ended the COPY.
                        st.server.disarm();
                    }
                    return Some((Err(st.clock.map(e)), st));
                }
                None => {
                    st.done = true;
                    st.server.disarm();
                    let last = st.batcher.finish();
                    debug!("COPY csv finished: {} rows", st.batcher.total_rows);
                    return last.map(|b| (Ok(b), st));
                }
            }
        }
    }))
}

struct ArrowState {
    copy: Pin<Box<CopyOutStream>>,
    decoder: BinaryCopyDecoder,
    batcher: ArrowBatcher,
    pending: VecDeque<RecordBatch>,
    cancel: CancellationToken,
    /// Armed until the server has finished the `COPY` (see [`CsvState::server`]).
    server: CancelOnDrop,
    clock: StatementClock,
    /// No more data will be read; `pending` may still hold batches.
    done: bool,
}

fn arrow_stream(state: ArrowState) -> BoxStream<'static, Result<Batch>> {
    Box::pin(stream::unfold(state, |mut st| async move {
        loop {
            if let Some(b) = st.pending.pop_front() {
                return Some((Ok(Batch::Arrow(b)), st));
            }
            if st.done {
                return None;
            }
            let next = tokio::select! {
                biased;
                _ = st.cancel.cancelled() => {
                    st.done = true;
                    st.server.cancel_now().await;
                    return Some((Err(CoreError::Cancelled), st));
                }
                m = st.copy.next() => m,
            };
            match next {
                Some(Ok(msg)) => match st.decoder.feed(&msg, &mut st.batcher) {
                    Ok(batches) => st.pending.extend(batches),
                    Err(e) => {
                        // The server is still sending: stop it before reporting the error.
                        st.done = true;
                        st.server.cancel_now().await;
                        return Some((Err(e), st));
                    }
                },
                Some(Err(e)) => {
                    st.done = true;
                    if e.as_db_error().is_some() {
                        st.server.disarm();
                    }
                    return Some((Err(st.clock.map(e)), st));
                }
                None => {
                    st.done = true;
                    st.server.disarm();
                    if !st.decoder.finished() {
                        return Some((Err(CoreError::Internal("binary COPY ended before its trailer".into())), st));
                    }
                    match st.batcher.flush() {
                        Ok(Some(b)) => st.pending.push_back(b),
                        Ok(None) => {}
                        Err(e) => return Some((Err(e), st)),
                    }
                    debug!("COPY binary finished: {} rows", st.decoder.rows());
                }
            }
        }
    }))
}

// ---------------------------------------------------------------------------------------------
// Probe
// ---------------------------------------------------------------------------------------------

/// Settings read by `probe` (via `current_setting(name, true)`), in result-column order.
const PROBE_SETTINGS: [&str; 15] = [
    "server_version",
    "shared_buffers",
    "work_mem",
    "max_parallel_workers_per_gather",
    "statement_timeout",
    "lock_timeout",
    "max_connections",
    "is_superuser",
    "maintenance_work_mem",
    "effective_cache_size",
    "temp_file_limit",
    "server_encoding",
    "TimeZone",
    "max_parallel_workers",
    "idle_in_transaction_session_timeout",
];

fn probe_sql() -> String {
    let settings: Vec<String> = PROBE_SETTINGS.iter().map(|s| format!("current_setting('{s}', true)::text")).collect();
    format!("SELECT {}, current_user::text, current_database()::text, version()::text", settings.join(", "))
}

const CAN_WRITE_EXPR: &str = "coalesce((SELECT bool_or(has_table_privilege(t.oid, 'INSERT')) \
     FROM (SELECT c.oid FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
           WHERE c.relkind = 'r' AND n.nspname NOT IN ('pg_catalog', 'information_schema') LIMIT 1000) t), false) \
     OR has_database_privilege(current_database(), 'CREATE')";

// ---------------------------------------------------------------------------------------------
// EXPLAIN
// ---------------------------------------------------------------------------------------------

/// Plan node properties copied into `PlanNode::extra` (as strings) when present.
const PLAN_EXTRA_KEYS: &[&str] = &[
    "Filter",
    "Index Cond",
    "Hash Cond",
    "Merge Cond",
    "Recheck Cond",
    "Join Filter",
    "One-Time Filter",
    "TID Cond",
    "Order By",
    "Sort Key",
    "Group Key",
    "Join Type",
    "Index Name",
    "Subplan Name",
    "Strategy",
    "Partial Mode",
    "CTE Name",
    "Function Name",
    "Scan Direction",
    "Parallel Aware",
    "Workers Planned",
    "Workers Launched",
    "Sort Method",
    "Sort Space Used",
    "Sort Space Type",
    "Hash Buckets",
    "Peak Memory Usage",
    "Rows Removed by Filter",
    "Rows Removed by Index Recheck",
    "Rows Removed by Join Filter",
    "Heap Fetches",
];

fn json_scalar_string(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(a) => a.iter().map(json_scalar_string).collect::<Vec<_>>().join(", "),
        serde_json::Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn parse_plan_node(v: &serde_json::Value) -> PlanNode {
    let s = |k: &str| v.get(k).and_then(|x| x.as_str()).map(str::to_string);
    let f = |k: &str| v.get(k).and_then(|x| x.as_f64());
    let relation = s("Relation Name").map(|name| TableRef { database: None, schema: s("Schema"), name });
    let children = v.get("Plans").and_then(|p| p.as_array()).map(|a| a.iter().map(parse_plan_node).collect()).unwrap_or_default();
    let mut extra = BTreeMap::new();
    for k in PLAN_EXTRA_KEYS {
        if let Some(x) = v.get(*k) {
            extra.insert((*k).to_string(), json_scalar_string(x));
        }
    }
    PlanNode {
        node_type: s("Node Type").unwrap_or_else(|| "?".into()),
        relation,
        alias: s("Alias"),
        plan_rows: f("Plan Rows"),
        plan_width: f("Plan Width").map(|w| w.max(0.0) as u32),
        total_cost: f("Total Cost"),
        startup_cost: f("Startup Cost"),
        actual_rows: f("Actual Rows"),
        actual_loops: f("Actual Loops"),
        actual_time_ms: f("Actual Total Time"),
        parent_relationship: s("Parent Relationship"),
        children,
        extra,
    }
}

fn collect_tables(node: &PlanNode, out: &mut Vec<TableRef>) {
    if let Some(r) = &node.relation {
        if !out.contains(r) {
            out.push(r.clone());
        }
    }
    for c in &node.children {
        collect_tables(c, out);
    }
}

/// Parse `EXPLAIN (FORMAT JSON)` output (`[{"Plan": {...}, ...}]`) into a `PlanSummary`.
/// `warnings` is left empty: preflight adds them.
pub fn parse_plan_json(raw: &str, analyzed: bool, elapsed_ms: u64) -> Result<PlanSummary> {
    let v: serde_json::Value = serde_json::from_str(raw).map_err(|e| CoreError::Internal(format!("EXPLAIN output is not JSON: {e}")))?;
    let top = v.as_array().and_then(|a| a.first()).ok_or_else(|| CoreError::Internal("EXPLAIN output is not a JSON array".into()))?;
    let plan = top.get("Plan").ok_or_else(|| CoreError::Internal("EXPLAIN output has no \"Plan\"".into()))?;
    let root = parse_plan_node(plan);
    let mut tables = Vec::new();
    collect_tables(&root, &mut tables);
    let estimated_rows = root.plan_rows.map(|r| r.max(0.0).round() as u64);
    let estimated_width = root.plan_width;
    let estimated_bytes = match (estimated_rows, estimated_width) {
        (Some(r), Some(w)) => Some(r.saturating_mul(w as u64)),
        _ => None,
    };
    Ok(PlanSummary {
        estimated_rows,
        estimated_width,
        total_cost: root.total_cost,
        estimated_bytes,
        analyzed,
        elapsed_ms,
        warnings: Vec::new(),
        tables,
        root: Some(root),
        raw: serde_json::to_string_pretty(&v).unwrap_or_else(|_| raw.to_string()),
        raw_format: "json".into(),
    })
}

/// `EXPLAIN (FORMAT JSON[, ANALYZE, BUFFERS]) <sql>` for an already trimmed statement.
///
/// The driver sends it with the extended query protocol (a single Parse message), where the
/// server parses exactly one statement: text such as `SELECT 1; SELECT 2` is rejected with
/// SQLSTATE `42601` before anything runs, instead of every statement being executed as the
/// simple query protocol would.
pub fn explain_sql(sql: &str, analyze: bool) -> String {
    let options = if analyze { "FORMAT JSON, ANALYZE, BUFFERS" } else { "FORMAT JSON" };
    format!("EXPLAIN ({options}) {sql}")
}

/// The statements that open EXPLAIN's transaction: `BEGIN` plus `SET LOCAL` statement and lock
/// timeouts ([`EXPLAIN_TIMEOUT`] or [`EXPLAIN_ANALYZE_TIMEOUT`], [`EXPLAIN_LOCK_TIMEOUT`]).
/// `SET LOCAL` ends with the transaction, so the session's own timeouts are back after the
/// `ROLLBACK`. Constant text only: user SQL never goes through this simple-protocol batch.
pub fn explain_begin_sql(analyze: bool) -> String {
    let stmt_timeout = if analyze { EXPLAIN_ANALYZE_TIMEOUT } else { EXPLAIN_TIMEOUT };
    format!("BEGIN; SET LOCAL statement_timeout = '{stmt_timeout}'; SET LOCAL lock_timeout = '{EXPLAIN_LOCK_TIMEOUT}'")
}

/// One `QUERY PLAN` value of `EXPLAIN (FORMAT JSON)`, as JSON text.
///
/// PostgreSQL types the column `json`, whose binary wire format is the JSON text itself;
/// `String` does not accept `json`, hence this wrapper. `jsonb` (version byte 1 + text) and
/// `text` / `varchar` (PostgreSQL-compatible servers that report the plan as text) are accepted
/// as well.
#[derive(Debug, PartialEq, Eq)]
struct PlanText(String);

impl<'a> FromSql<'a> for PlanText {
    fn from_sql(ty: &Type, raw: &'a [u8]) -> std::result::Result<Self, Box<dyn std::error::Error + Sync + Send>> {
        let body = if *ty == Type::JSONB {
            match raw.split_first() {
                Some((&1, rest)) => rest,
                Some((version, _)) => return Err(format!("unsupported jsonb encoding version {version}").into()),
                None => return Err("empty jsonb value".into()),
            }
        } else {
            raw
        };
        Ok(PlanText(std::str::from_utf8(body)?.to_owned()))
    }

    fn accepts(ty: &Type) -> bool {
        matches!(*ty, Type::JSON | Type::JSONB | Type::TEXT | Type::VARCHAR)
    }
}

/// The plan text of EXPLAIN's result rows. `FORMAT JSON` returns exactly one row; should a
/// server split it over several, they are joined with newlines (like the text format).
fn plan_text(rows: &[Row]) -> Result<String> {
    if rows.is_empty() {
        return Err(CoreError::Internal("EXPLAIN returned no plan".into()));
    }
    let mut out = String::new();
    for (i, row) in rows.iter().enumerate() {
        let PlanText(text) = row.try_get(0).map_err(|e| CoreError::Internal(format!("unreadable EXPLAIN output: {}", full_message(&e))))?;
        if i > 0 {
            out.push('\n');
        }
        out.push_str(&text);
    }
    Ok(out)
}

/// The short transaction an EXPLAIN runs in: [`explain_begin_sql`], the EXPLAIN, `ROLLBACK`.
/// The rollback also undoes whatever `EXPLAIN ANALYZE` executed.
///
/// When the `explain` future is dropped half way (a caller's timeout or `select!`), `Drop`
/// queues the `ROLLBACK` behind the statement still in flight, so the session never stays
/// "idle in transaction" with EXPLAIN's `SET LOCAL` timeouts applying to whatever this
/// connection runs next (requests run in the order they were sent).
struct ExplainTxn<'a> {
    client: &'a Client,
    /// `BEGIN` may have been sent and no `ROLLBACK` has been sent yet.
    open: bool,
}

impl<'a> ExplainTxn<'a> {
    /// Send `BEGIN; SET LOCAL ...`. The guard is armed before the request goes out, so it
    /// covers a drop while `BEGIN` is in flight. On failure the transaction (possibly opened
    /// and then aborted by a failing `SET LOCAL`) is rolled back before the error is returned.
    async fn begin(client: &'a Client, analyze: bool) -> Result<ExplainTxn<'a>> {
        let txn = ExplainTxn { client, open: true };
        match client.batch_execute(&explain_begin_sql(analyze)).await {
            Ok(()) => Ok(txn),
            Err(e) => {
                let err = map_pg_error(e);
                if let Err(rb) = txn.rollback().await {
                    debug!("ROLLBACK after a failed EXPLAIN BEGIN failed too: {rb}");
                }
                Err(err)
            }
        }
    }

    /// End the transaction. `open` is cleared before the request is sent: the first poll of
    /// the `ROLLBACK` future happens in the same poll of the caller, so a later drop cannot
    /// leave the transaction open nor queue a second `ROLLBACK`.
    async fn rollback(mut self) -> Result<()> {
        self.open = false;
        self.client.batch_execute("ROLLBACK").await.map_err(map_pg_error)
    }
}

impl Drop for ExplainTxn<'_> {
    fn drop(&mut self) {
        if !self.open {
            return;
        }
        debug!("EXPLAIN abandoned inside its transaction; queueing ROLLBACK");
        // tokio-postgres sends a request on the first poll of its future and keeps reading the
        // responses of a request nobody waits for any more, so one poll is enough to queue the
        // ROLLBACK right behind the statement in flight. A closed connection ends the
        // transaction server-side by itself.
        let _ = futures::FutureExt::now_or_never(self.client.batch_execute("ROLLBACK"));
    }
}

/// Map an error of the EXPLAIN statement itself; `elapsed` is the time since just before it
/// was sent. See [`explain_timeout_error`]; everything else goes through [`map_pg_error`] (a
/// multi-statement text is `Query` with code `42601`, a foreign cancel `Query` with `57014`).
fn map_explain_error(e: tokio_postgres::Error, analyze: bool, elapsed: std::time::Duration) -> CoreError {
    if let Some(db) = e.as_db_error() {
        if let Some(err) = explain_timeout_error(db.code().code(), db.message(), analyze, elapsed) {
            return err;
        }
    }
    map_pg_error(e)
}

/// The `Timeout` errors EXPLAIN's own `SET LOCAL` limits produce, or `None` for any other
/// server error:
///
/// * `55P03`: no lock within [`EXPLAIN_LOCK_TIMEOUT`].
/// * `57014`: EXPLAIN's statement timeout ([`EXPLAIN_TIMEOUT`] / [`EXPLAIN_ANALYZE_TIMEOUT`]),
///   recognised by the message ("statement timeout", English servers) or by `elapsed` having
///   reached that limit (any `lc_messages`). `elapsed` is measured client-side from just
///   before the statement was sent, so it is never shorter than the server's clock. An earlier
///   `57014` with another message is a cancel request this call did not send (a DBA's
///   `pg_cancel_backend`): `None`, so it stays `Query` with code `57014` and is not reported
///   as a timeout (the service would then look for lock holders that have nothing to do with
///   it).
fn explain_timeout_error(code: &str, msg: &str, analyze: bool, elapsed: std::time::Duration) -> Option<CoreError> {
    match code {
        "55P03" => Some(CoreError::Timeout(format!("lock wait exceeded {EXPLAIN_LOCK_TIMEOUT} while planning: {msg}"))),
        SQLSTATE_QUERY_CANCELED => {
            let (what, text, limit) =
                if analyze { ("EXPLAIN ANALYZE", EXPLAIN_ANALYZE_TIMEOUT, EXPLAIN_ANALYZE_TIMEOUT_LIMIT) } else { ("EXPLAIN", EXPLAIN_TIMEOUT, EXPLAIN_TIMEOUT_LIMIT) };
            (msg.contains("statement timeout") || elapsed >= limit).then(|| CoreError::Timeout(format!("{what} exceeded the {text} statement timeout")))
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------------------------
// Source
// ---------------------------------------------------------------------------------------------

#[async_trait]
impl Source for PgDatabase {
    async fn probe(&self) -> Result<ServerInfo> {
        let _guard = self.exclusive().await;
        let sql = probe_sql();
        let row = self.client.query_one(sql.as_str(), &[]).await.map_err(map_pg_error)?;
        let n = PROBE_SETTINGS.len();
        let mut settings = BTreeMap::new();
        for (i, name) in PROBE_SETTINGS.iter().enumerate() {
            if let Some(v) = row.try_get::<_, Option<String>>(i).map_err(map_pg_error)? {
                settings.insert((*name).to_string(), v);
            }
        }
        let user: Option<String> = row.try_get(n).map_err(map_pg_error)?;
        let database: Option<String> = row.try_get(n + 1).map_err(map_pg_error)?;
        let version_full: Option<String> = row.try_get(n + 2).map_err(map_pg_error)?;
        settings.insert("server_version_num".into(), self.server_version_num.to_string());
        if let Some(v) = &version_full {
            settings.insert("version".into(), v.clone());
        }

        let ssl_expr = if self.server_version_num >= 90500 { "(SELECT s.ssl FROM pg_stat_ssl s WHERE s.pid = pg_backend_pid())" } else { "NULL::bool" };
        let write_sql = format!("SELECT ({CAN_WRITE_EXPR}) AS can_write, {ssl_expr} AS ssl");
        let (can_write, ssl) = match self.client.query_one(write_sql.as_str(), &[]).await {
            Ok(r) => (r.try_get::<_, Option<bool>>(0).unwrap_or(None), r.try_get::<_, Option<bool>>(1).unwrap_or(None)),
            Err(e) => {
                debug!("privilege probe failed: {}", full_message(&e));
                (None, None)
            }
        };

        let ram_bytes = settings.get("shared_buffers").and_then(|s| crate::preflight::parse_mem(s)).map(|b| b.saturating_mul(4));
        let mut notes = Vec::new();
        if settings.get("is_superuser").is_some_and(|v| v == "on") {
            notes.push("Connected as a superuser; a read-only account is recommended for extraction.".to_string());
        } else if can_write == Some(true) {
            notes.push("The account can write to this database; a read-only account is recommended for extraction.".to_string());
        }
        match ssl {
            Some(true) => notes.push("Connection is TLS encrypted.".to_string()),
            Some(false) => notes.push("Connection is not encrypted.".to_string()),
            None => {}
        }
        Ok(ServerInfo {
            driver: "postgres".into(),
            version: settings.get("server_version").cloned().unwrap_or_else(|| self.server_version_num.to_string()),
            user,
            database,
            ram_bytes,
            ram_source: ram_bytes.map(|_| "shared_buffers x 4".to_string()),
            settings,
            can_write,
            notes,
        })
    }

    async fn apply_session(&self, opts: &SessionOpts) -> Result<()> {
        let stmts = session_sql(opts);
        let sql = stmts.join("; ");
        debug!("applying session settings: {sql}");
        let _guard = self.exclusive().await;
        self.client.batch_execute(&sql).await.map_err(map_pg_error)
    }

    /// `opts` must be the options applied with `apply_session`: its `statement_timeout_ms` tells
    /// a statement timeout apart from a foreign cancel request (both are SQLSTATE `57014`).
    async fn stream(&self, q: &Query, opts: &SessionOpts, cancel: CancellationToken) -> Result<SourceStream> {
        let sql = trim_sql(&q.sql);
        if sql.is_empty() {
            return Err(CoreError::Invalid("empty query".into()));
        }
        // Held until the server has started the COPY: anything this session sends later queues
        // behind the running COPY, and the COPY itself never goes out inside another call's
        // EXPLAIN transaction.
        let _guard = self.exclusive_or_cancelled(&cancel).await?;
        match &q.output {
            StreamOutput::RawCsv(fmt) => {
                let copy_sql = copy_csv_sql(sql, fmt);
                debug!("starting COPY csv on pid {}", self.pid);
                let clock = StatementClock::start(opts);
                let copy = self.with_cancel(self.client.copy_out(copy_sql.as_str()), &cancel).await.map_err(|e| clock.refine(e))?;
                let state = CsvState {
                    copy: Box::pin(copy),
                    batcher: CsvBatcher::new(fmt.crlf),
                    expect_header: fmt.header,
                    cancel,
                    server: CancelOnDrop::new(self.canceller.clone()),
                    clock,
                    done: false,
                };
                Ok(SourceStream { schema: None, stream: csv_stream(state) })
            }
            StreamOutput::Arrow => {
                let clock = StatementClock::start(opts);
                let stmt = self.with_cancel(self.client.prepare(sql), &cancel).await.map_err(|e| clock.refine(e))?;
                let plans: Vec<PgColumnPlan> = stmt.columns().iter().map(|c| plan_column(c.name(), c.type_(), c.type_modifier())).collect();
                drop(stmt);
                if plans.is_empty() {
                    return Err(CoreError::Invalid("the query returns no columns".into()));
                }
                let wrapped = wrap_query(sql, &plans);
                let copy_sql = copy_binary_sql(wrapped.as_deref().unwrap_or(sql));
                debug!("starting COPY binary on pid {} ({} columns, {} cast to text)", self.pid, plans.len(), plans.iter().filter(|p| p.cast).count());
                let clock = StatementClock::start(opts);
                let copy = self.with_cancel(self.client.copy_out(copy_sql.as_str()), &cancel).await.map_err(|e| clock.refine(e))?;
                let cols: Vec<(String, ColKind)> = plans.iter().map(|p| (p.name.clone(), p.kind)).collect();
                let schema = schema_from(&cols);
                let kinds: Vec<ColKind> = plans.iter().map(|p| p.kind).collect();
                let batcher = ArrowBatcher::new(schema.clone(), kinds);
                let decoder = BinaryCopyDecoder::new(plans.iter().map(|p| (p.wire, p.kind)).collect());
                let state = ArrowState {
                    copy: Box::pin(copy),
                    decoder,
                    batcher,
                    pending: VecDeque::new(),
                    cancel,
                    server: CancelOnDrop::new(self.canceller.clone()),
                    clock,
                    done: false,
                };
                Ok(SourceStream { schema: Some(schema), stream: arrow_stream(state) })
            }
        }
    }

    async fn explain(&self, sql: &str, analyze: bool) -> Result<Option<PlanSummary>> {
        let sql = trim_sql(sql);
        if sql.is_empty() {
            return Err(CoreError::Invalid("empty query".into()));
        }
        // Held until after the ROLLBACK (or, when this future is dropped, until `ExplainTxn`'s
        // drop has queued it: locals drop in reverse order), so no other statement of this
        // session can run inside EXPLAIN's transaction.
        let _guard = self.exclusive().await;
        let start = Instant::now();
        let txn = ExplainTxn::begin(&self.client, analyze).await?;
        // Extended protocol, one round trip (Parse/Bind/Describe/Execute/Sync on the unnamed
        // statement): the server accepts exactly one statement and refuses `a; b` at Parse,
        // before executing anything. Never `simple_query`, which runs every statement.
        let explain = explain_sql(sql, analyze);
        debug!("explain on pid {} (analyze: {analyze})", self.pid);
        let sent = Instant::now();
        let result = self.client.query_typed(explain.as_str(), &[]).await;
        let statement_elapsed = sent.elapsed();
        let rollback = txn.rollback().await;
        let rows = result.map_err(|e| map_explain_error(e, analyze, statement_elapsed))?;
        rollback?;
        let raw = plan_text(&rows)?;
        let elapsed_ms = start.elapsed().as_millis() as u64;
        parse_plan_json(&raw, analyze, elapsed_ms).map(Some)
    }

    fn session_id(&self) -> Option<String> {
        Some(self.pid.to_string())
    }

    fn driver_id(&self) -> &'static str {
        "postgres"
    }

    fn supports_raw_csv(&self) -> bool {
        true
    }
}

impl Database for PgDatabase {}

// ---------------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use arrow_array::{Array, BinaryArray, BooleanArray, Date32Array, Decimal128Array, Float64Array, Int64Array, StringArray, TimestampMicrosecondArray};

    // ----------------------------------------------------------------------- byte builders

    fn header(flags: i32, ext: &[u8]) -> Vec<u8> {
        let mut v = BINARY_SIGNATURE.to_vec();
        v.extend_from_slice(&flags.to_be_bytes());
        v.extend_from_slice(&(ext.len() as i32).to_be_bytes());
        v.extend_from_slice(ext);
        v
    }

    fn tuple(fields: &[Option<Vec<u8>>]) -> Vec<u8> {
        let mut v = (fields.len() as i16).to_be_bytes().to_vec();
        for f in fields {
            match f {
                None => v.extend_from_slice(&(-1i32).to_be_bytes()),
                Some(b) => {
                    v.extend_from_slice(&(b.len() as i32).to_be_bytes());
                    v.extend_from_slice(b);
                }
            }
        }
        v
    }

    fn trailer() -> Vec<u8> {
        (-1i16).to_be_bytes().to_vec()
    }

    fn numeric_bytes(sign: u16, weight: i16, dscale: u16, digits: &[u16]) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&(digits.len() as u16).to_be_bytes());
        v.extend_from_slice(&weight.to_be_bytes());
        v.extend_from_slice(&sign.to_be_bytes());
        v.extend_from_slice(&dscale.to_be_bytes());
        for d in digits {
            v.extend_from_slice(&d.to_be_bytes());
        }
        v
    }

    fn batcher_for(cols: &[(WireType, ColKind)]) -> ArrowBatcher {
        let named: Vec<(String, ColKind)> = cols.iter().enumerate().map(|(i, (_, k))| (format!("c{i}"), *k)).collect();
        ArrowBatcher::new(schema_from(&named), cols.iter().map(|(_, k)| *k).collect())
    }

    // ----------------------------------------------------------------------- decoder

    #[test]
    fn decodes_every_wire_type_with_nulls() {
        let cols = vec![
            (WireType::Int4, ColKind::Int64),
            (WireType::Text, ColKind::Utf8),
            (WireType::Numeric, ColKind::Decimal { precision: 10, scale: 2 }),
            (WireType::Timestamp, ColKind::Timestamp),
            (WireType::Bool, ColKind::Bool),
            (WireType::Date, ColKind::Date),
            (WireType::Float8, ColKind::Float64),
            (WireType::Bytea, ColKind::Binary),
            (WireType::Uuid, ColKind::Utf8),
            (WireType::Int8, ColKind::Int64),
            (WireType::Int2, ColKind::Int64),
            (WireType::Oid, ColKind::Int64),
            (WireType::Float4, ColKind::Float64),
            (WireType::Time, ColKind::Time),
        ];
        let mut batcher = batcher_for(&cols);
        let mut dec = BinaryCopyDecoder::new(cols);

        // 2024-02-29 12:34:56 UTC: micros since 1970.
        let unix_micros: i64 = 1_709_210_096_000_000;
        let pg_micros = unix_micros - PG_EPOCH_MICROS;
        let uuid = uuid::Uuid::parse_str("123e4567-e89b-12d3-a456-426614174000").unwrap();

        let mut bytes = header(0, &[]);
        bytes.extend(tuple(&[
            Some(42i32.to_be_bytes().to_vec()),
            Some(b"h\xc3\xa9llo".to_vec()),
            Some(numeric_bytes(NUMERIC_NEG, 0, 2, &[123, 4500])), // -123.45
            Some(pg_micros.to_be_bytes().to_vec()),
            Some(vec![1]),
            Some(0i32.to_be_bytes().to_vec()), // 2000-01-01
            Some(2.5f64.to_be_bytes().to_vec()),
            Some(vec![0xde, 0xad]),
            Some(uuid.as_bytes().to_vec()),
            Some((-7i64).to_be_bytes().to_vec()),
            Some((-3i16).to_be_bytes().to_vec()),
            Some(4_000_000_000u32.to_be_bytes().to_vec()),
            Some(1.5f32.to_be_bytes().to_vec()),
            Some(3_600_000_000i64.to_be_bytes().to_vec()),
        ]));
        bytes.extend(tuple(&[None, None, None, None, None, None, None, None, None, None, None, None, None, None]));
        bytes.extend(tuple(&[
            Some(1i32.to_be_bytes().to_vec()),
            Some(b"".to_vec()),
            Some(numeric_bytes(NUMERIC_POS, 1, 0, &[1, 2])), // 10002
            Some(i64::MAX.to_be_bytes().to_vec()),           // infinity -> null
            Some(vec![0]),
            Some((-10_957i32).to_be_bytes().to_vec()), // 1970-01-01
            Some(f64::NAN.to_be_bytes().to_vec()),
            Some(vec![]),
            Some(uuid.as_bytes().to_vec()),
            Some(0i64.to_be_bytes().to_vec()),
            Some(0i16.to_be_bytes().to_vec()),
            Some(0u32.to_be_bytes().to_vec()),
            Some(0f32.to_be_bytes().to_vec()),
            Some(0i64.to_be_bytes().to_vec()),
        ]));
        bytes.extend(trailer());

        let out = dec.feed(&bytes, &mut batcher).unwrap();
        assert!(dec.finished());
        assert_eq!(dec.rows(), 3);
        assert_eq!(out.len(), 1);
        let b = &out[0];
        assert_eq!(b.num_rows(), 3);

        let ints = b.column(0).as_any().downcast_ref::<Int64Array>().unwrap();
        assert_eq!(ints.value(0), 42);
        assert!(ints.is_null(1));
        let texts = b.column(1).as_any().downcast_ref::<StringArray>().unwrap();
        assert_eq!(texts.value(0), "héllo");
        assert!(texts.is_null(1));
        assert_eq!(texts.value(2), "");
        let decs = b.column(2).as_any().downcast_ref::<Decimal128Array>().unwrap();
        assert_eq!(decs.value(0), -12345);
        assert!(decs.is_null(1));
        assert_eq!(decs.value(2), 1_000_200);
        let ts = b.column(3).as_any().downcast_ref::<TimestampMicrosecondArray>().unwrap();
        assert_eq!(ts.value(0), unix_micros);
        assert!(ts.is_null(1));
        assert!(ts.is_null(2), "infinity timestamps become NULL");
        let bools = b.column(4).as_any().downcast_ref::<BooleanArray>().unwrap();
        assert!(bools.value(0));
        assert!(!bools.value(2));
        let dates = b.column(5).as_any().downcast_ref::<Date32Array>().unwrap();
        assert_eq!(dates.value(0), 10_957);
        assert_eq!(dates.value(2), 0);
        let floats = b.column(6).as_any().downcast_ref::<Float64Array>().unwrap();
        assert_eq!(floats.value(0), 2.5);
        assert!(floats.value(2).is_nan());
        let bins = b.column(7).as_any().downcast_ref::<BinaryArray>().unwrap();
        assert_eq!(bins.value(0), &[0xde, 0xad]);
        assert_eq!(bins.value(2), b"");
        let uuids = b.column(8).as_any().downcast_ref::<StringArray>().unwrap();
        assert_eq!(uuids.value(0), "123e4567-e89b-12d3-a456-426614174000");
        let i8s = b.column(9).as_any().downcast_ref::<Int64Array>().unwrap();
        assert_eq!(i8s.value(0), -7);
        let i2s = b.column(10).as_any().downcast_ref::<Int64Array>().unwrap();
        assert_eq!(i2s.value(0), -3);
        let oids = b.column(11).as_any().downcast_ref::<Int64Array>().unwrap();
        assert_eq!(oids.value(0), 4_000_000_000);
        let f4s = b.column(12).as_any().downcast_ref::<Float64Array>().unwrap();
        assert_eq!(f4s.value(0), 1.5);
        let times = b.column(13).as_any().downcast_ref::<arrow_array::Time64MicrosecondArray>().unwrap();
        assert_eq!(times.value(0), 3_600_000_000);
    }

    #[test]
    fn decoder_handles_arbitrary_framing_and_header_extension() {
        let cols = vec![(WireType::Int4, ColKind::Int64), (WireType::Text, ColKind::Utf8)];
        let mut batcher = batcher_for(&cols);
        let mut dec = BinaryCopyDecoder::new(cols);
        let mut bytes = header(0, b"ext!");
        for i in 0..100i32 {
            bytes.extend(tuple(&[Some(i.to_be_bytes().to_vec()), Some(format!("row{i}").into_bytes())]));
        }
        bytes.extend(trailer());
        let mut batches = Vec::new();
        // Feed one byte at a time: every tuple must be reassembled across calls.
        for chunk in bytes.chunks(1) {
            batches.extend(dec.feed(chunk, &mut batcher).unwrap());
        }
        assert!(dec.finished());
        assert_eq!(dec.rows(), 100);
        let total: usize = batches.iter().map(|b| b.num_rows()).sum();
        assert_eq!(total, 100);
        let last = batches.last().unwrap();
        let texts = last.column(1).as_any().downcast_ref::<StringArray>().unwrap();
        assert_eq!(texts.value(last.num_rows() - 1), "row99");
        // Bytes after the trailer are ignored.
        assert!(dec.feed(b"garbage", &mut batcher).unwrap().is_empty());
    }

    #[test]
    fn decoder_cuts_batches_when_batcher_is_full() {
        let cols = vec![(WireType::Int2, ColKind::Int64)];
        let named = vec![("x".to_string(), ColKind::Int64)];
        let mut batcher = ArrowBatcher::with_limits(schema_from(&named), vec![ColKind::Int64], 3, usize::MAX);
        let mut dec = BinaryCopyDecoder::new(cols);
        let mut bytes = header(0, &[]);
        for i in 0..7i16 {
            bytes.extend(tuple(&[Some(i.to_be_bytes().to_vec())]));
        }
        bytes.extend(trailer());
        let out = dec.feed(&bytes, &mut batcher).unwrap();
        let sizes: Vec<usize> = out.iter().map(|b| b.num_rows()).collect();
        assert_eq!(sizes, vec![3, 3, 1]);
    }

    #[test]
    fn decoder_rejects_bad_signature_and_field_count() {
        let cols = vec![(WireType::Int4, ColKind::Int64)];
        let mut batcher = batcher_for(&cols);
        let mut dec = BinaryCopyDecoder::new(cols.clone());
        let mut bad = header(0, &[]);
        bad[0] = b'X';
        assert!(dec.feed(&bad, &mut batcher).is_err());

        let mut dec = BinaryCopyDecoder::new(cols);
        let mut bytes = header(0, &[]);
        bytes.extend(tuple(&[Some(1i32.to_be_bytes().to_vec()), Some(2i32.to_be_bytes().to_vec())]));
        let err = dec.feed(&bytes, &mut batcher).unwrap_err();
        assert!(err.to_string().contains("expected 1"), "{err}");
    }

    #[test]
    fn numeric_special_values_and_text_rendering() {
        // Decimal column: NaN -> NULL.
        let cols = vec![(WireType::Numeric, ColKind::Decimal { precision: 5, scale: 1 })];
        let mut batcher = batcher_for(&cols);
        let mut dec = BinaryCopyDecoder::new(cols);
        let mut bytes = header(0, &[]);
        bytes.extend(tuple(&[Some(numeric_bytes(NUMERIC_NAN, 0, 0, &[]))]));
        bytes.extend(tuple(&[Some(numeric_bytes(NUMERIC_POS, -1, 3, &[500]))])); // 0.05 -> 0.1 (half away from zero)
        bytes.extend(tuple(&[Some(numeric_bytes(NUMERIC_NEG, -1, 2, &[1200]))])); // -0.12 -> -0.1
        bytes.extend(trailer());
        let out = dec.feed(&bytes, &mut batcher).unwrap();
        let d = out[0].column(0).as_any().downcast_ref::<Decimal128Array>().unwrap();
        assert!(d.is_null(0));
        assert_eq!(d.value(1), 1);
        assert_eq!(d.value(2), -1);

        // Utf8 column fed numeric bytes: rendered as text like the server does.
        let cols = vec![(WireType::Numeric, ColKind::Utf8)];
        let mut batcher = batcher_for(&cols);
        let mut dec = BinaryCopyDecoder::new(cols);
        let mut bytes = header(0, &[]);
        bytes.extend(tuple(&[Some(numeric_bytes(NUMERIC_NEG, 0, 2, &[123, 4500]))]));
        bytes.extend(tuple(&[Some(numeric_bytes(NUMERIC_POS, -2, 6, &[4200]))])); // 0.0000 4200 -> 0.000042
        bytes.extend(tuple(&[Some(numeric_bytes(NUMERIC_POS, 2, 0, &[1]))]));
        bytes.extend(tuple(&[Some(numeric_bytes(NUMERIC_POS, 0, 0, &[]))]));
        bytes.extend(tuple(&[Some(numeric_bytes(NUMERIC_NAN, 0, 0, &[]))]));
        bytes.extend(tuple(&[Some(numeric_bytes(NUMERIC_NINF, 0, 0, &[]))]));
        bytes.extend(tuple(&[Some(numeric_bytes(NUMERIC_POS, 0, 4, &[1, 5000]))]));
        bytes.extend(trailer());
        let out = dec.feed(&bytes, &mut batcher).unwrap();
        let s = out[0].column(0).as_any().downcast_ref::<StringArray>().unwrap();
        assert_eq!(s.value(0), "-123.45");
        assert_eq!(s.value(1), "0.000042");
        assert_eq!(s.value(2), "100000000");
        assert_eq!(s.value(3), "0");
        assert_eq!(s.value(4), "NaN");
        assert_eq!(s.value(5), "-Infinity");
        assert_eq!(s.value(6), "1.5000");
    }

    #[test]
    fn numeric_overflow_is_an_error_for_decimal_columns() {
        let cols = vec![(WireType::Numeric, ColKind::Decimal { precision: 38, scale: 0 })];
        let mut batcher = batcher_for(&cols);
        let mut dec = BinaryCopyDecoder::new(cols);
        let mut bytes = header(0, &[]);
        // 10^44: digit 1 with weight 11 -> 1 followed by 44 zeros.
        bytes.extend(tuple(&[Some(numeric_bytes(NUMERIC_POS, 11, 0, &[1]))]));
        bytes.extend(trailer());
        let err = dec.feed(&bytes, &mut batcher).unwrap_err();
        assert!(err.to_string().contains("does not fit"), "{err}");
    }

    // ----------------------------------------------------------------------- SQL helpers

    #[test]
    fn copy_csv_sql_escapes_options() {
        let fmt = CsvFormat { delimiter: b'\t', header: true, null_as: "N'ULL".into(), crlf: false };
        let sql = copy_csv_sql("select 1;", &fmt);
        assert_eq!(sql, "COPY (\nselect 1;\n) TO STDOUT (FORMAT csv, HEADER true, DELIMITER '\t', NULL 'N''ULL', QUOTE '\"', ESCAPE '\"')");
        let fmt = CsvFormat { delimiter: b'|', header: false, null_as: "\\N".into(), crlf: true };
        let sql = copy_csv_sql("select 1", &fmt);
        assert!(sql.contains("HEADER false"));
        assert!(sql.contains("DELIMITER '|'"));
        assert!(sql.contains("NULL E'\\\\N'"), "{sql}");
        assert_eq!(copy_binary_sql("select 1"), "COPY (\nselect 1\n) TO STDOUT (FORMAT binary)");
    }

    #[test]
    fn quoting_helpers() {
        assert_eq!(quote_ident("a\"b"), "\"a\"\"b\"");
        assert_eq!(quote_literal("it's"), "'it''s'");
        assert_eq!(quote_literal("a\\b'c"), "E'a\\\\b''c'");
        assert_eq!(trim_sql("  select 1 ; ;\n"), "select 1");
        assert_eq!(trim_sql("select 1"), "select 1");
    }

    #[test]
    fn session_sql_quotes_values() {
        let opts = SessionOpts {
            work_mem: Some("64MB'; DROP TABLE x; --".into()),
            statement_timeout_ms: Some(60_000),
            lock_timeout_ms: Some(5_000),
            max_parallel_workers: Some(2),
            fetch_batch_rows: Some(1000),
        };
        let v = session_sql(&opts);
        assert_eq!(
            v,
            vec![
                "SET work_mem = '64MB''; DROP TABLE x; --'",
                "SET statement_timeout = '60000ms'",
                "SET lock_timeout = '5000ms'",
                "SET max_parallel_workers_per_gather = '2'",
                "SET client_encoding = 'UTF8'",
            ]
        );
        assert_eq!(session_sql(&SessionOpts::default()), vec!["SET client_encoding = 'UTF8'"]);
    }

    #[test]
    fn sqlstate_mapping() {
        // 57014 is never a user cancel here (the job token wins first): a statement timeout is
        // TIMEOUT, a foreign cancel request (DBA) is a QUERY failure that keeps its code.
        assert!(matches!(map_sqlstate("57014", "canceling statement due to statement timeout".into()), CoreError::Timeout(_)));
        match map_sqlstate("57014", "canceling statement due to user request".into()) {
            CoreError::Query { code, msg } => {
                assert_eq!(code.as_deref(), Some("57014"));
                assert!(msg.contains("user request"));
            }
            other => panic!("unexpected {other:?}"),
        }
        assert!(matches!(map_sqlstate("57P01", "x".into()), CoreError::Connect(_)));
        assert!(matches!(map_sqlstate("08006", "x".into()), CoreError::Connect(_)));
        assert!(matches!(map_sqlstate("28P01", "x".into()), CoreError::Auth(_)));
        assert!(matches!(map_sqlstate("55P03", "x".into()), CoreError::Timeout(_)));
        assert!(matches!(map_sqlstate("40P01", "x".into()), CoreError::Timeout(_)));
        match map_sqlstate("42P01", "relation \"x\" does not exist".into()) {
            CoreError::Query { code, msg } => {
                assert_eq!(code.as_deref(), Some("42P01"));
                assert!(msg.contains("does not exist"));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn statement_clock_turns_late_query_canceled_into_timeout() {
        use std::time::Duration;
        let canceled = || CoreError::Query { code: Some(SQLSTATE_QUERY_CANCELED.into()), msg: "Anweisung storniert".into() };
        let clock = StatementClock { timeout: Some(Duration::from_millis(500)), started: Instant::now() };
        // Translated message, timeout elapsed -> TIMEOUT.
        match clock.classify(canceled(), Duration::from_millis(501)) {
            CoreError::Timeout(m) => assert!(m.contains("500 ms") && m.contains("storniert"), "{m}"),
            other => panic!("unexpected {other:?}"),
        }
        // Before the timeout: a foreign cancel, stays QUERY 57014.
        assert!(matches!(clock.classify(canceled(), Duration::from_millis(100)), CoreError::Query { code: Some(c), .. } if c == "57014"));
        // Other errors are untouched.
        assert!(matches!(clock.classify(CoreError::Connect("x".into()), Duration::from_secs(9)), CoreError::Connect(_)));
        assert!(matches!(clock.classify(CoreError::Query { code: Some("42P01".into()), msg: "x".into() }, Duration::from_secs(9)), CoreError::Query { .. }));
        // No timeout configured (None or 0) -> never TIMEOUT by elapsed time.
        for opts in [SessionOpts::default(), SessionOpts { statement_timeout_ms: Some(0), ..Default::default() }] {
            let clock = StatementClock::start(&opts);
            assert!(matches!(clock.classify(canceled(), Duration::from_secs(3600)), CoreError::Query { .. }));
        }
        let clock = StatementClock::start(&SessionOpts { statement_timeout_ms: Some(1), ..Default::default() });
        std::thread::sleep(Duration::from_millis(5));
        assert!(matches!(clock.refine(canceled()), CoreError::Timeout(_)));
    }

    #[test]
    fn explain_errors_tell_their_own_timeouts_from_a_foreign_cancel() {
        use std::time::Duration;
        let timeout_msg = "canceling statement due to statement timeout";
        let user_msg = "canceling statement due to user request";
        let is_timeout = |e: Option<CoreError>, needle: &str| matches!(e, Some(CoreError::Timeout(ref m)) if m.contains(needle));

        // Lock wait (EXPLAIN's lock_timeout) is always a timeout.
        assert!(is_timeout(explain_timeout_error("55P03", "could not obtain lock", false, Duration::ZERO), "3s"));
        // English message: a timeout whatever the elapsed time.
        assert!(is_timeout(explain_timeout_error("57014", timeout_msg, false, Duration::from_secs(1)), "EXPLAIN exceeded the 15s"));
        assert!(is_timeout(explain_timeout_error("57014", timeout_msg, true, Duration::from_secs(1)), "EXPLAIN ANALYZE exceeded the 5min"));
        // A DBA's pg_cancel_backend before the limit: not ours, falls through to map_pg_error.
        assert!(explain_timeout_error("57014", user_msg, false, Duration::from_secs(2)).is_none());
        assert!(explain_timeout_error("57014", user_msg, true, Duration::from_secs(60)).is_none());
        assert!(matches!(map_sqlstate("57014", user_msg.into()), CoreError::Query { code: Some(ref c), .. } if c == "57014"));
        // Translated message: decided by the elapsed time against the SET LOCAL limit.
        let translated = "Anweisung wird storniert";
        assert!(explain_timeout_error("57014", translated, false, Duration::from_millis(14_999)).is_none());
        assert!(is_timeout(explain_timeout_error("57014", translated, false, EXPLAIN_TIMEOUT_LIMIT), "15s"));
        assert!(explain_timeout_error("57014", translated, true, EXPLAIN_TIMEOUT_LIMIT).is_none());
        assert!(is_timeout(explain_timeout_error("57014", translated, true, EXPLAIN_ANALYZE_TIMEOUT_LIMIT), "5min"));
        // Everything else is left to map_pg_error.
        for code in ["42601", "42P01", "25P02", "40P01"] {
            assert!(explain_timeout_error(code, "x", true, Duration::from_secs(3600)).is_none(), "{code}");
        }
    }

    #[test]
    fn explain_timeout_limits_match_their_set_local_text() {
        use std::time::Duration;
        fn pg_duration(text: &str) -> Duration {
            if let Some(min) = text.strip_suffix("min") {
                Duration::from_secs(min.parse::<u64>().expect("minutes") * 60)
            } else if let Some(s) = text.strip_suffix('s') {
                Duration::from_secs(s.parse().expect("seconds"))
            } else {
                panic!("unexpected PostgreSQL duration {text:?}")
            }
        }
        assert_eq!(pg_duration(EXPLAIN_TIMEOUT), EXPLAIN_TIMEOUT_LIMIT);
        assert_eq!(pg_duration(EXPLAIN_ANALYZE_TIMEOUT), EXPLAIN_ANALYZE_TIMEOUT_LIMIT);
    }

    // ----------------------------------------------------------------------- CSV batching

    /// Feed `n` copies of `line` and check every batch against the limits; returns
    /// (batches, rows, bytes).
    fn feed_csv(b: &mut CsvBatcher, line: &[u8], n: usize, cap: usize) -> (usize, u64, usize) {
        let (mut batches, mut rows, mut bytes) = (0usize, 0u64, 0usize);
        let mut check = |batch: Batch| match batch {
            Batch::RawCsv { data, rows: r } => {
                assert!(data.len() <= BATCH_BYTES, "batch of {} bytes exceeds BATCH_BYTES", data.len());
                assert!(r <= BATCH_ROWS as u64);
                batches += 1;
                rows += r;
                bytes += data.len();
            }
            other => panic!("unexpected {other:?}"),
        };
        for _ in 0..n {
            if let Some(batch) = b.push(line) {
                check(batch);
            }
            assert!(b.buf.capacity() <= cap, "batch buffer regrew to {} bytes", b.buf.capacity());
        }
        if let Some(batch) = b.finish() {
            check(batch);
        }
        (batches, rows, bytes)
    }

    #[test]
    fn csv_batcher_cuts_before_the_byte_limit_without_regrowing() {
        // 200-byte rows: the byte limit (8 MB) is reached long before 64K rows.
        let mut line = vec![b'a'; 199];
        line.push(b'\n');
        for crlf in [false, true] {
            let mut b = CsvBatcher::new(crlf);
            let cap = b.buf.capacity();
            let n = 100_000;
            let (batches, rows, bytes) = feed_csv(&mut b, &line, n, cap);
            let out_len = line.len() + usize::from(crlf);
            assert_eq!(rows, n as u64);
            assert_eq!(bytes, n * out_len);
            // Every batch but the last is as full as whole rows allow.
            assert_eq!(batches, n.div_ceil(BATCH_BYTES / out_len));
            assert_eq!(b.total_rows, n as u64);
        }
        // Rows that tile 8 MB exactly fill a batch to the byte.
        let line = [b'b'; 256];
        let mut b = CsvBatcher::new(false);
        let rows_per_batch = BATCH_BYTES / line.len();
        let first = (0..rows_per_batch).find_map(|_| b.push(&line)).expect("a full batch");
        assert!(matches!(first, Batch::RawCsv { ref data, rows } if data.len() == BATCH_BYTES && rows == rows_per_batch as u64));
        assert!(b.finish().is_none());
    }

    #[test]
    fn csv_batcher_cuts_on_the_row_limit() {
        let mut b = CsvBatcher::new(false);
        let cap = b.buf.capacity();
        let (batches, rows, _) = feed_csv(&mut b, b"1\n", BATCH_ROWS + 10, cap);
        assert_eq!((batches, rows), (2, BATCH_ROWS as u64 + 10));
    }

    #[test]
    fn csv_batcher_gives_an_oversized_row_its_own_batch() {
        let big = vec![b'x'; BATCH_BYTES + 10];
        let rows_of = |b: Option<Batch>| match b {
            Some(Batch::RawCsv { data, rows }) => (rows, data.len()),
            other => panic!("unexpected {other:?}"),
        };
        let mut b = CsvBatcher::new(false);
        assert!(b.push(b"a\n").is_none());
        assert_eq!(rows_of(b.push(&big)), (1, 2), "the small row is shipped before the big one");
        assert_eq!(rows_of(b.push(b"c\n")), (1, BATCH_BYTES + 10), "the big row travels alone");
        assert_eq!(rows_of(b.finish()), (1, 2));
        // A big first row is cut immediately.
        let mut b = CsvBatcher::new(false);
        assert_eq!(rows_of(b.push(&big)), (1, BATCH_BYTES + 10));
        assert!(b.finish().is_none());
        assert_eq!(b.buf.capacity(), BytesMut::with_capacity(BATCH_BYTES).capacity(), "the next batch starts with a fresh 8 MB buffer");
    }

    // ----------------------------------------------------------------------- type planning

    #[test]
    fn numeric_typmod_decoding() {
        assert_eq!(numeric_typmod(-1), None);
        assert_eq!(numeric_typmod(((12 << 16) | 2) + 4), Some((12, 2)));
        assert_eq!(numeric_typmod((5 << 16) + 4), Some((5, 0)));
        // PostgreSQL 15 negative scale: numeric(5,-2) stores scale as 11-bit two's complement.
        assert_eq!(numeric_typmod(((5 << 16) | (0x800 - 2)) + 4), Some((5, -2)));
    }

    #[test]
    fn type_to_colkind_mapping() {
        let p = plan_column("b", &Type::BOOL, -1);
        assert_eq!((p.kind, p.wire, p.cast), (ColKind::Bool, WireType::Bool, false));
        for t in [Type::INT2, Type::INT4, Type::INT8, Type::OID] {
            let p = plan_column("i", &t, -1);
            assert_eq!(p.kind, ColKind::Int64);
            assert!(!p.cast);
        }
        assert_eq!(plan_column("f", &Type::FLOAT4, -1).wire, WireType::Float4);
        assert_eq!(plan_column("f", &Type::FLOAT8, -1).kind, ColKind::Float64);
        let n = plan_column("n", &Type::NUMERIC, ((12 << 16) | 2) + 4);
        assert_eq!(n.kind, ColKind::Decimal { precision: 12, scale: 2 });
        assert_eq!(n.wire, WireType::Numeric);
        assert!(!n.cast);
        let n = plan_column("n", &Type::NUMERIC, -1);
        assert_eq!((n.kind, n.wire, n.cast), (ColKind::Utf8, WireType::Text, true));
        let n = plan_column("n", &Type::NUMERIC, ((40 << 16) | 2) + 4);
        assert!(n.cast, "precision > 38 falls back to text");
        for t in [Type::TEXT, Type::VARCHAR, Type::BPCHAR, Type::NAME] {
            let p = plan_column("s", &t, -1);
            assert_eq!((p.kind, p.wire, p.cast), (ColKind::Utf8, WireType::Text, false));
        }
        assert_eq!(plan_column("b", &Type::BYTEA, -1).kind, ColKind::Binary);
        assert_eq!(plan_column("d", &Type::DATE, -1).kind, ColKind::Date);
        assert_eq!(plan_column("t", &Type::TIME, -1).kind, ColKind::Time);
        assert_eq!(plan_column("t", &Type::TIMESTAMP, -1).kind, ColKind::Timestamp);
        assert_eq!(plan_column("t", &Type::TIMESTAMPTZ, -1).kind, ColKind::TimestampTz);
        let u = plan_column("u", &Type::UUID, -1);
        assert_eq!((u.kind, u.wire, u.cast), (ColKind::Utf8, WireType::Uuid, false));
        for t in [
            Type::JSON,
            Type::JSONB,
            Type::XML,
            Type::INET,
            Type::CIDR,
            Type::MACADDR,
            Type::INTERVAL,
            Type::INT4_ARRAY,
            Type::CHAR,
            Type::UNKNOWN,
            Type::MONEY,
            Type::TIMETZ,
        ] {
            let p = plan_column("x", &t, -1);
            assert_eq!((p.kind, p.wire, p.cast), (ColKind::Utf8, WireType::Text, true), "{}", t.name());
        }
        // Domains unwrap to their base type; enums and citext go through text.
        let dom = Type::new("positive_int".into(), 70000, Kind::Domain(Type::INT4), "public".into());
        assert_eq!(plan_column("d", &dom, -1).wire, WireType::Int4);
        let en = Type::new("mood".into(), 70001, Kind::Enum(vec!["sad".into(), "ok".into()]), "public".into());
        assert!(plan_column("e", &en, -1).cast);
        let citext = Type::new("citext".into(), 70002, Kind::Simple, "public".into());
        let p = plan_column("c", &citext, -1);
        assert_eq!((p.kind, p.cast), (ColKind::Utf8, true));
    }

    #[test]
    fn wrapped_query_construction() {
        let cols = vec![plan_column("id", &Type::INT4, -1), plan_column("name", &Type::TEXT, -1)];
        assert_eq!(wrap_query("select 1", &cols), None);

        let cols = vec![plan_column("id", &Type::INT4, -1), plan_column("doc", &Type::JSONB, -1), plan_column("we\"ird", &Type::INET, -1)];
        let w = wrap_query("select * from t", &cols).unwrap();
        assert_eq!(w, "SELECT \"id\", \"doc\"::text AS \"doc\", \"we\"\"ird\"::text AS \"we\"\"ird\" FROM (\nselect * from t\n) AS _bulkdb");

        let cols = vec![plan_column("x", &Type::INT4, -1), plan_column("x", &Type::JSONB, -1)];
        let w = wrap_query("select 1 x, '{}'::jsonb x", &cols).unwrap();
        assert_eq!(w, "SELECT _c1 AS \"x\", _c2::text AS \"x\" FROM (\nselect 1 x, '{}'::jsonb x\n) AS _bulkdb(_c1, _c2)");
    }

    // ----------------------------------------------------------------------- CSV framing

    #[test]
    fn csv_line_endings() {
        let mut b = BytesMut::new();
        append_csv_line(&mut b, b"a,b\n", true);
        append_csv_line(&mut b, b"c,\"x\ny\"\n", true);
        append_csv_line(&mut b, b"already\r\n", true);
        append_csv_line(&mut b, b"e,f\n", false);
        assert_eq!(&b[..], b"a,b\r\nc,\"x\ny\"\r\nalready\r\ne,f\n");
    }

    // ----------------------------------------------------------------------- plan parsing

    const NESTED_LOOP_PLAN: &str = r#"[
      {
        "Plan": {
          "Node Type": "Nested Loop",
          "Parallel Aware": false,
          "Join Type": "Inner",
          "Startup Cost": 0.29,
          "Total Cost": 12345.67,
          "Plan Rows": 1500000,
          "Plan Width": 120,
          "Plans": [
            {
              "Node Type": "Seq Scan",
              "Parent Relationship": "Outer",
              "Parallel Aware": false,
              "Relation Name": "orders",
              "Schema": "public",
              "Alias": "o",
              "Startup Cost": 0.00,
              "Total Cost": 100.00,
              "Plan Rows": 1500000,
              "Plan Width": 100,
              "Filter": "(o.status = 'open'::text)"
            },
            {
              "Node Type": "Aggregate",
              "Strategy": "Plain",
              "Partial Mode": "Simple",
              "Parent Relationship": "SubPlan",
              "Subplan Name": "SubPlan 1",
              "Startup Cost": 8.30,
              "Total Cost": 8.31,
              "Plan Rows": 1,
              "Plan Width": 8,
              "Plans": [
                {
                  "Node Type": "Index Scan",
                  "Parent Relationship": "Outer",
                  "Scan Direction": "Forward",
                  "Index Name": "items_order_idx",
                  "Relation Name": "items",
                  "Schema": "public",
                  "Alias": "i",
                  "Startup Cost": 0.29,
                  "Total Cost": 8.30,
                  "Plan Rows": 1,
                  "Plan Width": 8,
                  "Index Cond": "(i.order_id = o.id)",
                  "Sort Key": ["i.created_at DESC", "i.id"]
                }
              ]
            }
          ]
        },
        "Planning Time": 0.5
      }
    ]"#;

    #[test]
    fn parses_nested_loop_with_subplan() {
        let s = parse_plan_json(NESTED_LOOP_PLAN, false, 7).unwrap();
        assert_eq!(s.estimated_rows, Some(1_500_000));
        assert_eq!(s.estimated_width, Some(120));
        assert_eq!(s.estimated_bytes, Some(180_000_000));
        assert_eq!(s.total_cost, Some(12345.67));
        assert_eq!(s.raw_format, "json");
        assert!(!s.analyzed);
        assert_eq!(s.elapsed_ms, 7);
        assert!(s.warnings.is_empty());
        assert_eq!(s.tables, vec![TableRef::new(Some("public"), "orders"), TableRef::new(Some("public"), "items")]);
        let root = s.root.unwrap();
        assert_eq!(root.node_type, "Nested Loop");
        assert_eq!(root.extra.get("Join Type").map(String::as_str), Some("Inner"));
        assert_eq!(root.children.len(), 2);
        let scan = &root.children[0];
        assert_eq!(scan.parent_relationship.as_deref(), Some("Outer"));
        assert_eq!(scan.alias.as_deref(), Some("o"));
        assert_eq!(scan.extra.get("Filter").map(String::as_str), Some("(o.status = 'open'::text)"));
        let sub = &root.children[1];
        assert_eq!(sub.node_type, "Aggregate");
        assert_eq!(sub.parent_relationship.as_deref(), Some("SubPlan"));
        assert_eq!(sub.extra.get("Subplan Name").map(String::as_str), Some("SubPlan 1"));
        let idx = &sub.children[0];
        assert_eq!(idx.extra.get("Index Name").map(String::as_str), Some("items_order_idx"));
        assert_eq!(idx.extra.get("Sort Key").map(String::as_str), Some("i.created_at DESC, i.id"));
        assert!(idx.actual_rows.is_none());
        assert!(s.raw.contains("\"Node Type\": \"Nested Loop\""));
    }

    #[test]
    fn parses_analyze_fields_and_rejects_garbage() {
        let raw = r#"[{"Plan":{"Node Type":"Seq Scan","Relation Name":"t","Plan Rows":3,"Plan Width":4,"Total Cost":1.5,"Actual Rows":3,"Actual Loops":1,"Actual Total Time":0.02}}]"#;
        let s = parse_plan_json(raw, true, 1).unwrap();
        let root = s.root.unwrap();
        assert_eq!(root.actual_rows, Some(3.0));
        assert_eq!(root.actual_loops, Some(1.0));
        assert_eq!(root.actual_time_ms, Some(0.02));
        assert_eq!(root.relation, Some(TableRef::new(None, "t")));
        assert!(parse_plan_json("not json", false, 0).is_err());
        assert!(parse_plan_json("[]", false, 0).is_err());
        assert!(parse_plan_json("[{\"x\":1}]", false, 0).is_err());
    }

    #[test]
    fn explain_statement_and_transaction_text() {
        assert_eq!(explain_sql("SELECT 1", false), "EXPLAIN (FORMAT JSON) SELECT 1");
        assert_eq!(explain_sql("SELECT ';' AS semi", true), "EXPLAIN (FORMAT JSON, ANALYZE, BUFFERS) SELECT ';' AS semi");
        assert_eq!(explain_begin_sql(false), "BEGIN; SET LOCAL statement_timeout = '15s'; SET LOCAL lock_timeout = '3s'");
        assert_eq!(explain_begin_sql(true), "BEGIN; SET LOCAL statement_timeout = '5min'; SET LOCAL lock_timeout = '3s'");
    }

    #[test]
    fn plan_text_reads_json_jsonb_and_text_columns() {
        let plan: &[u8] = br#"[{"Plan": {"Node Type": "Result", "Plan Rows": 1, "Plan Width": 4, "Total Cost": 0.01}}]"#;
        let expected = PlanText(String::from_utf8(plan.to_vec()).unwrap());
        // `json` is what PostgreSQL sends for FORMAT JSON; its binary format is the text.
        for ty in [Type::JSON, Type::TEXT, Type::VARCHAR] {
            assert!(<PlanText as FromSql>::accepts(&ty), "{ty:?}");
            assert_eq!(PlanText::from_sql(&ty, plan).unwrap(), expected, "{ty:?}");
        }
        // `jsonb` binary format: version byte 1, then the text.
        assert!(<PlanText as FromSql>::accepts(&Type::JSONB));
        let jsonb = [&[1u8][..], plan].concat();
        assert_eq!(PlanText::from_sql(&Type::JSONB, &jsonb).unwrap(), expected);
        let summary = parse_plan_json(&PlanText::from_sql(&Type::JSON, plan).unwrap().0, false, 3).unwrap();
        assert_eq!(summary.estimated_rows, Some(1));
        assert_eq!(summary.root.unwrap().node_type, "Result");

        assert!(PlanText::from_sql(&Type::JSONB, b"\x02[]").is_err(), "unknown jsonb version");
        assert!(PlanText::from_sql(&Type::JSONB, b"").is_err(), "empty jsonb");
        assert!(PlanText::from_sql(&Type::JSON, b"[\"\xff\xfe\"]").is_err(), "invalid UTF-8");
        for ty in [Type::INT4, Type::BYTEA, Type::XML, Type::JSON_ARRAY] {
            assert!(!<PlanText as FromSql>::accepts(&ty), "{ty:?}");
        }
    }
}
