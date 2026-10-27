//! SQL Server driver (tiberius 0.12, TDS 7.3+ streaming).
//!
//! * `client.query(sql, &[])` -> `QueryStream`; every `QueryItem::Row` is decoded from its
//!   `ColumnData` cells straight into an [`ArrowBatcher`] and emitted as `Batch::Arrow` at
//!   64K rows / 8 MB. Only the first result set is exported.
//! * The TDS connection lives in a spawned task while a query streams (tiberius' stream
//!   borrows the client), feeding a 2-deep channel; the caller gets the schema as soon as the
//!   server sent the result metadata.
//! * Session: `SET LOCK_TIMEOUT <ms>`, `NOCOUNT`, `ARITHABORT`, `READ COMMITTED`. There is no
//!   server-side statement timeout, so `statement_timeout_ms` is enforced client-side around
//!   the whole stream.
//! * Cancel: tiberius does not expose the TDS attention packet, so the client is dropped; the
//!   closed socket makes the server abort the batch. Catalog/monitor calls on the same object
//!   reconnect lazily afterwards.
//! * Probe: `@@VERSION`, `sys.dm_os_sys_memory` (or `max server memory (MB)`), write permission.
//! * EXPLAIN: `SET SHOWPLAN_XML ON` (estimated) / `SET STATISTICS XML ON` (actual); the XML is
//!   parsed with regexes into [`PlanSummary`].
//!
//! Transport: tiberius speaks `futures::io`, and this workspace compiles `tokio-util` without
//! its `compat` feature, so [`TdsStream`] is a tiny adapter over `tokio::net::TcpStream`.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::panic::AssertUnwindSafe;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, OnceLock, RwLock};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use arrow_schema::SchemaRef;
use async_trait::async_trait;
use futures::stream::TryStreamExt;
use futures::FutureExt;
use regex::Regex;
use tiberius::error::Error as TdsError;
use tiberius::{AuthMethod, Client, ColumnData, ColumnType, Config, EncryptionLevel, FromSql, QueryItem, Row, ToSql};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio_util::sync::CancellationToken;

use super::arrow_batch::{days_from_date, micros_from_naive, micros_from_time, schema_from, ArrowBatcher, Cell, ColKind};
use super::{Batch, ConnectParams, Database, Query, Source, SourceStream, SslMode};
use crate::error::{CoreError, Result};
use crate::types::{PlanNode, PlanSummary, PlanWarning, ServerInfo, SessionOpts, Severity, TableRef};
use crate::{BATCH_BYTES, BATCH_ROWS};

// ---------------------------------------------------------------------------------------------
// Transport
// ---------------------------------------------------------------------------------------------

/// `futures::io` adapter over a tokio TCP stream (what `tokio_util::compat::Compat` would be,
/// had the feature been enabled in the workspace).
pub struct TdsStream(TcpStream);

impl futures::io::AsyncRead for TdsStream {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut [u8]) -> Poll<std::io::Result<usize>> {
        let mut rb = tokio::io::ReadBuf::new(buf);
        match tokio::io::AsyncRead::poll_read(Pin::new(&mut self.0), cx, &mut rb) {
            Poll::Ready(Ok(())) => Poll::Ready(Ok(rb.filled().len())),
            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl futures::io::AsyncWrite for TdsStream {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<std::io::Result<usize>> {
        tokio::io::AsyncWrite::poll_write(Pin::new(&mut self.0), cx, buf)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        tokio::io::AsyncWrite::poll_flush(Pin::new(&mut self.0), cx)
    }
    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        tokio::io::AsyncWrite::poll_shutdown(Pin::new(&mut self.0), cx)
    }
}

/// The tiberius client type used everywhere in this driver.
pub type TdsClient = Client<TdsStream>;

/// Default SQL Server TCP port.
pub const DEFAULT_PORT: u16 = 1433;

// ---------------------------------------------------------------------------------------------
// Connection
// ---------------------------------------------------------------------------------------------

/// The connection slot shared between the database object and the streaming task.
pub(crate) struct ClientSlot {
    /// `None` while a query streams (the task owns the client) or after the connection was
    /// dropped on cancel / timeout; [`MssqlDatabase::ensure_client`] reconnects lazily.
    pub(crate) client: Mutex<Option<TdsClient>>,
    /// Set while the streaming task owns the client, so other callers get "busy" instead of
    /// opening a second session behind the job's back.
    pub(crate) streaming: AtomicBool,
    /// `@@SPID` of the current connection: `None` once it was dropped, new after a reconnect.
    pub(crate) session_id: RwLock<Option<String>>,
}

pub struct MssqlDatabase {
    pub(crate) params: ConnectParams,
    pub(crate) slot: Arc<ClientSlot>,
    /// `DB_NAME()` at connect time; catalog queries prefix `[db].` only for other databases.
    pub(crate) current_db: String,
    /// Last `SET LOCK_TIMEOUT` applied in ms (-1 = infinite), restored after EXPLAIN and after
    /// a reconnect.
    pub(crate) lock_timeout_ms: AtomicI64,
    /// Per job ring of "was the wait `ASYNC_NETWORK_IO`" samples (monitor).
    pub(crate) net_waits: std::sync::Mutex<HashMap<String, VecDeque<bool>>>,
}

impl MssqlDatabase {
    /// Open a TDS connection and remember its `@@SPID` / `DB_NAME()`.
    pub async fn connect(params: &ConnectParams) -> Result<Self> {
        let mut client = connect_client(params).await?;
        let (spid, db) = fetch_session_facts(&mut client).await?;
        Ok(MssqlDatabase {
            params: params.clone(),
            slot: Arc::new(ClientSlot { client: Mutex::new(Some(client)), streaming: AtomicBool::new(false), session_id: RwLock::new(Some(spid)) }),
            current_db: db,
            lock_timeout_ms: AtomicI64::new(-1),
            net_waits: std::sync::Mutex::new(HashMap::new()),
        })
    }

    /// Lock the connection slot. Use [`Self::ensure_client`] on the guard to get the client.
    pub(crate) async fn lock(&self) -> tokio::sync::MutexGuard<'_, Option<TdsClient>> {
        self.slot.client.lock().await
    }

    /// The client inside a locked slot, reconnecting when the previous connection was dropped
    /// (cancel / timeout). Fails with `CONNECT` while an extraction streams on this object.
    pub(crate) async fn ensure_client<'g>(&self, guard: &'g mut Option<TdsClient>) -> Result<&'g mut TdsClient> {
        if guard.is_none() {
            if self.slot.streaming.load(Ordering::Acquire) {
                return Err(CoreError::Connect("the connection is busy streaming a query".into()));
            }
            tracing::debug!(target: "bulkdb::mssql", "reconnecting after the previous connection was dropped");
            let mut client = connect_client(&self.params).await?;
            let (spid, _) = fetch_session_facts(&mut client).await?;
            *self.slot.session_id.write().unwrap() = Some(spid);
            let lt = self.lock_timeout_ms.load(Ordering::Relaxed);
            if lt >= 0 {
                exec_batch(&mut client, &format!("SET LOCK_TIMEOUT {lt}")).await?;
            }
            *guard = Some(client);
        }
        Ok(guard.as_mut().expect("slot filled above"))
    }

    /// Drop the connection held in `guard` (the server aborts whatever it was running). The
    /// next call reconnects.
    pub(crate) fn drop_connection(&self, guard: &mut Option<TdsClient>) {
        if guard.take().is_some() {
            tracing::debug!(target: "bulkdb::mssql", "connection dropped; server session {:?} will be aborted", self.session_id());
        }
        *self.slot.session_id.write().unwrap() = None;
    }

    /// `[db].` when `db` names another database than the connection's, else empty.
    pub(crate) fn db_prefix(&self, db: Option<&str>) -> String {
        match db {
            Some(d) if !d.is_empty() && !d.eq_ignore_ascii_case(&self.current_db) => format!("{}.", quote_ident(d)),
            _ => String::new(),
        }
    }

    /// Fully qualified, bracket-quoted table name (`[db].[schema].[name]` where needed).
    pub(crate) fn qualified(&self, table: &TableRef) -> String {
        let schema = table.schema.as_deref().filter(|s| !s.is_empty()).unwrap_or("dbo");
        format!("{}{}.{}", self.db_prefix(table.database.as_deref()), quote_ident(schema), quote_ident(&table.name))
    }

    /// Run a batch of statements that return no rows (SET options, KILL, DDL for tests).
    /// Not used for extraction SQL.
    pub async fn execute_batch(&self, sql: &str) -> Result<()> {
        let mut guard = self.lock().await;
        let client = self.ensure_client(&mut guard).await?;
        exec_batch(client, sql).await
    }
}

/// Build the tiberius `Config` from connection parameters. Public for unit tests.
pub fn build_config(params: &ConnectParams) -> Result<Config> {
    let port = if params.port == 0 { DEFAULT_PORT } else { params.port };
    if let Some(instance) = params.field("instance") {
        if params.port == 0 {
            return Err(CoreError::Connect(format!(
                "named instance '{instance}' needs its TCP port: SQL Browser lookup is not compiled in. Find the port in SQL Server Configuration Manager (TCP/IP -> IP Addresses -> IPAll) and set it on the connection."
            )));
        }
    }
    let user = params.user.as_deref().map(str::trim).unwrap_or("");
    let mut cfg = if user.is_empty() {
        if crate::platform::os_key() != "windows" {
            return Err(CoreError::Auth("a user name is required: Windows integrated authentication is only available on Windows".into()));
        }
        // tiberius gates `AuthMethod::Integrated` behind `cfg(windows)`; parsing a minimal
        // ADO string yields it without a platform branch in core.
        Config::from_ado_string("IntegratedSecurity=true").map_err(map_err)?
    } else {
        let mut c = Config::new();
        c.authentication(AuthMethod::sql_server(user, params.password.as_deref().unwrap_or("")));
        c
    };
    cfg.host(&params.host);
    cfg.port(port);
    if let Some(db) = params.database.as_deref().filter(|d| !d.is_empty()) {
        cfg.database(db);
    }
    cfg.application_name(&params.application_name);
    let encrypt = match params.field("encrypt") {
        Some(_) => params.field_bool("encrypt", true),
        None => params.ssl != SslMode::Disable,
    };
    cfg.encryption(if encrypt { EncryptionLevel::Required } else { EncryptionLevel::Off });
    if params.ssl != SslMode::VerifyFull && params.field_bool("trust_server_certificate", false) {
        cfg.trust_cert();
    }
    Ok(cfg)
}

/// TCP connect + TDS login, following one Azure routing redirect.
pub(crate) async fn connect_client(params: &ConnectParams) -> Result<TdsClient> {
    let base = build_config(params)?;
    let mut host = params.host.clone();
    let mut port = if params.port == 0 { DEFAULT_PORT } else { params.port };
    for attempt in 0..2 {
        let tcp = tokio::time::timeout(params.connect_timeout, TcpStream::connect((host.as_str(), port)))
            .await
            .map_err(|_| CoreError::Connect(format!("timed out after {:?} connecting to {host}:{port}", params.connect_timeout)))?
            .map_err(|e| CoreError::Connect(format!("cannot reach {host}:{port}: {e}")))?;
        tcp.set_nodelay(true)?;
        let mut cfg = base.clone();
        cfg.host(&host);
        cfg.port(port);
        match tokio::time::timeout(params.connect_timeout, Client::connect(cfg, TdsStream(tcp))).await {
            Err(_) => return Err(CoreError::Connect(format!("login to {host}:{port} timed out after {:?}", params.connect_timeout))),
            Ok(Ok(client)) => return Ok(client),
            Ok(Err(TdsError::Routing { host: h, port: p })) if attempt == 0 => {
                tracing::debug!(target: "bulkdb::mssql", "server redirected the connection to {h}:{p}");
                host = h;
                port = p;
            }
            Ok(Err(e)) => return Err(map_err(e)),
        }
    }
    Err(CoreError::Connect("too many routing redirects".into()))
}

async fn fetch_session_facts(client: &mut TdsClient) -> Result<(String, String)> {
    let rows = query_rows(client, "SELECT @@SPID AS spid, DB_NAME() AS db", &[]).await?;
    let row = rows.first().ok_or_else(|| CoreError::Internal("SELECT @@SPID returned no row".into()))?;
    let spid = get_i64(row, 0).ok_or_else(|| CoreError::Internal("@@SPID is null".into()))?;
    Ok((spid.to_string(), get_string(row, 1).unwrap_or_default()))
}

// ---------------------------------------------------------------------------------------------
// Small query helpers shared with catalog / monitor
// ---------------------------------------------------------------------------------------------

/// Run a parameterised query and collect the (small, metadata) first result set.
pub(crate) async fn query_rows(client: &mut TdsClient, sql: &str, params: &[&dyn ToSql]) -> Result<Vec<Row>> {
    let stream = client.query(sql, params).await.map_err(map_err)?;
    stream.into_first_result().await.map_err(map_err)
}

/// Run a plain SQL batch (session `SET` options, `KILL`) and drain its (row-less) response.
pub(crate) async fn exec_batch(client: &mut TdsClient, sql: &str) -> Result<()> {
    let stream = client.simple_query(sql).await.map_err(map_err)?;
    stream.into_results().await.map_err(map_err)?;
    Ok(())
}

/// Whether a `CoreError` is a SQL Server permission error (297 / 300), so that optional
/// catalog statistics can fall back instead of failing the listing.
pub(crate) fn is_permission_error(e: &CoreError) -> bool {
    matches!(e, CoreError::Query { code: Some(c), .. } if c == "297" || c == "300")
}

pub(crate) fn cell_at(row: &Row, i: usize) -> Option<&ColumnData<'static>> {
    row.cells().nth(i).map(|(_, d)| d)
}

/// Text of a cell: strings, XML, GUIDs and numbers are all rendered; `None` for NULL.
pub(crate) fn get_string(row: &Row, i: usize) -> Option<String> {
    match cell_at(row, i)? {
        ColumnData::String(Some(s)) => Some(s.to_string()),
        ColumnData::Xml(Some(x)) => Some(x.as_ref().to_string()),
        ColumnData::Guid(Some(g)) => Some(format_guid(g)),
        ColumnData::Numeric(Some(n)) => Some(format_numeric(n.value(), n.scale())),
        ColumnData::Bit(Some(b)) => Some(b.to_string()),
        ColumnData::U8(Some(v)) => Some(v.to_string()),
        ColumnData::I16(Some(v)) => Some(v.to_string()),
        ColumnData::I32(Some(v)) => Some(v.to_string()),
        ColumnData::I64(Some(v)) => Some(v.to_string()),
        ColumnData::F32(Some(v)) => Some(v.to_string()),
        ColumnData::F64(Some(v)) => Some(v.to_string()),
        _ => None,
    }
}

/// Integer value of a cell whatever its integer width (also accepts bit / numeric / float).
pub(crate) fn get_i64(row: &Row, i: usize) -> Option<i64> {
    match cell_at(row, i)? {
        ColumnData::U8(Some(v)) => Some(*v as i64),
        ColumnData::I16(Some(v)) => Some(*v as i64),
        ColumnData::I32(Some(v)) => Some(*v as i64),
        ColumnData::I64(Some(v)) => Some(*v),
        ColumnData::Bit(Some(b)) => Some(*b as i64),
        ColumnData::F32(Some(v)) => Some(*v as i64),
        ColumnData::F64(Some(v)) => Some(*v as i64),
        ColumnData::Numeric(Some(n)) => Some((n.value() / 10i128.pow(n.scale() as u32)) as i64),
        ColumnData::String(Some(s)) => s.trim().parse().ok(),
        _ => None,
    }
}

pub(crate) fn get_u64(row: &Row, i: usize) -> Option<u64> {
    get_i64(row, i).map(|v| v.max(0) as u64)
}

pub(crate) fn get_bool(row: &Row, i: usize) -> bool {
    get_i64(row, i).unwrap_or(0) != 0
}

pub(crate) fn get_f64(row: &Row, i: usize) -> Option<f64> {
    match cell_at(row, i)? {
        ColumnData::F32(Some(v)) => Some(*v as f64),
        ColumnData::F64(Some(v)) => Some(*v),
        _ => get_i64(row, i).map(|v| v as f64),
    }
}

/// `[name]` with `]` doubled, the T-SQL quoting rule.
pub fn quote_ident(ident: &str) -> String {
    format!("[{}]", ident.replace(']', "]]"))
}

/// GUIDs as SQL Server prints them: upper-case, hyphenated.
pub fn format_guid(g: &tiberius::Uuid) -> String {
    g.hyphenated().to_string().to_ascii_uppercase()
}

/// Exact decimal text for a scaled integer (tiberius' own `Display` mangles negatives).
pub fn format_numeric(value: i128, scale: u8) -> String {
    let sign = if value < 0 { "-" } else { "" };
    let abs = value.unsigned_abs();
    if scale == 0 {
        return format!("{sign}{abs}");
    }
    let pow = 10u128.pow(scale as u32);
    format!("{sign}{}.{:0width$}", abs / pow, abs % pow, width = scale as usize)
}

// ---------------------------------------------------------------------------------------------
// Error mapping
// ---------------------------------------------------------------------------------------------

/// Map a tiberius error onto `CoreError`.
pub fn map_err(e: TdsError) -> CoreError {
    match e {
        TdsError::Server(t) => map_server_error(t.code(), t.message()),
        TdsError::Io { kind, message } => CoreError::Connect(format!("{message} ({kind:?})")),
        TdsError::Tls(m) => CoreError::Connect(format!("TLS handshake failed: {m}")),
        TdsError::Routing { host, port } => CoreError::Connect(format!("server redirected the connection to {host}:{port}")),
        TdsError::Utf8 | TdsError::Utf16 => CoreError::Internal("tiberius: invalid text encoding received from the server".into()),
        other => CoreError::Internal(format!("tiberius: {other}")),
    }
}

/// Map a SQL Server error number (`sys.messages`) onto `CoreError`.
pub fn map_server_error(code: u32, message: &str) -> CoreError {
    match code {
        // Login failed / account disabled / locked / password expired / untrusted domain.
        18456 | 18452 | 18470 | 18486 | 18487 | 18488 | 15151 => CoreError::Auth(message.to_string()),
        1222 => CoreError::Timeout(format!("lock request time out period exceeded: {message}")),
        1205 => CoreError::Timeout(format!("chosen as deadlock victim: {message}")),
        // Cannot open database / default database / network path / connection refused / forcibly closed.
        4060 | 4064 | 53 | 2 | 64 | 233 | 10053 | 10054 | 10060 | 10061 | 17142 | 17187 => CoreError::Connect(message.to_string()),
        _ => CoreError::Query { code: Some(code.to_string()), msg: message.to_string() },
    }
}

// ---------------------------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------------------------

/// Map a TDS column type onto the logical column kinds BulkDB writes.
pub fn col_kind(ty: ColumnType) -> ColKind {
    use ColumnType::*;
    match ty {
        Bit | Bitn => ColKind::Bool,
        Int1 | Int2 | Int4 | Int8 | Intn => ColKind::Int64,
        Float4 | Float8 | Floatn => ColKind::Float64,
        Money | Money4 => ColKind::Decimal { precision: 19, scale: 4 },
        // Precision/scale are not in the column metadata; exact text keeps every digit.
        Decimaln | Numericn => ColKind::Utf8,
        Datetime | Datetime2 | Datetimen | Datetime4 => ColKind::Timestamp,
        DatetimeOffsetn => ColKind::TimestampTz,
        Daten => ColKind::Date,
        Timen => ColKind::Time,
        BigVarChar | BigChar | NVarchar | NChar | Text | NText | Xml | Udt | Guid | Null => ColKind::Utf8,
        BigVarBin | BigBinary | Image | SSVariant => ColKind::Binary,
    }
}

/// Column types tiberius cannot decode (it panics on them): the user must `CAST` them.
fn unsupported_hint(ty: ColumnType) -> Option<&'static str> {
    match ty {
        ColumnType::SSVariant => Some("sql_variant: CAST it to nvarchar(max) or varbinary(max) in the query"),
        ColumnType::Udt => Some("CLR user-defined type (hierarchyid/geometry/geography): CAST it to nvarchar(max) or varbinary(max) in the query"),
        _ => None,
    }
}

/// Round a legacy `datetime` value to the millisecond.
///
/// `datetime` is stored in 1/300 s ticks, so `'05.123'` is really 05.1233333 s. SQL Server,
/// sqlcmd, bcp and SSMS all render it at its documented millisecond accuracy (`.123`, `.997`),
/// and an export should show the same value rather than `.123333`. Ticks are k/300 s, i.e.
/// k·3.33 ms, so a value never sits exactly between two milliseconds. `datetime2` and
/// `smalldatetime` are exact and are not rounded.
pub(crate) fn round_legacy_datetime_micros(micros: i64) -> i64 {
    (micros + 500).div_euclid(1000) * 1000
}

/// Push one decoded cell into the batcher, converting to the column's kind.
fn push_cell(b: &mut ArrowBatcher, data: &ColumnData<'static>, col: usize) -> Result<()> {
    let kind = b.kinds()[col];
    match data {
        ColumnData::U8(v) => b.push(v.map(|x| Cell::I64(x as i64)).unwrap_or(Cell::Null)),
        ColumnData::I16(v) => b.push(v.map(|x| Cell::I64(x as i64)).unwrap_or(Cell::Null)),
        ColumnData::I32(v) => b.push(v.map(|x| Cell::I64(x as i64)).unwrap_or(Cell::Null)),
        ColumnData::I64(v) => b.push(v.map(Cell::I64).unwrap_or(Cell::Null)),
        ColumnData::F32(v) => b.push(v.map(|x| Cell::F64(x as f64)).unwrap_or(Cell::Null)),
        ColumnData::F64(v) => match (v, kind) {
            // money / smallmoney arrive as f64 already divided by 10^4.
            (Some(x), ColKind::Decimal { scale, .. }) => b.push(Cell::Decimal(scale_f64(*x, scale))),
            (Some(x), _) => b.push(Cell::F64(*x)),
            (None, _) => b.push(Cell::Null),
        },
        ColumnData::Bit(v) => b.push(v.map(Cell::Bool).unwrap_or(Cell::Null)),
        ColumnData::String(v) => b.push(v.as_deref().map(Cell::Str).unwrap_or(Cell::Null)),
        ColumnData::Xml(v) => b.push(v.as_deref().map(|x| Cell::Str(x.as_ref())).unwrap_or(Cell::Null)),
        ColumnData::Binary(v) => b.push(v.as_deref().map(Cell::Bytes).unwrap_or(Cell::Null)),
        ColumnData::Guid(v) => match v {
            Some(g) => b.push(Cell::Str(&format_guid(g))),
            None => b.push(Cell::Null),
        },
        ColumnData::Numeric(v) => match v {
            Some(n) => b.push(Cell::Str(&format_numeric(n.value(), n.scale()))),
            None => b.push(Cell::Null),
        },
        ColumnData::DateTime(_) => {
            let v = chrono::NaiveDateTime::from_sql(data).map_err(map_err)?;
            b.push(v.map(|t| Cell::Timestamp(round_legacy_datetime_micros(micros_from_naive(t)))).unwrap_or(Cell::Null))
        }
        ColumnData::SmallDateTime(_) | ColumnData::DateTime2(_) => {
            let v = chrono::NaiveDateTime::from_sql(data).map_err(map_err)?;
            b.push(v.map(|t| Cell::Timestamp(micros_from_naive(t))).unwrap_or(Cell::Null))
        }
        ColumnData::DateTimeOffset(_) => {
            let v = chrono::DateTime::<chrono::Utc>::from_sql(data).map_err(map_err)?;
            b.push(v.map(|t| Cell::Timestamp(t.timestamp_micros())).unwrap_or(Cell::Null))
        }
        ColumnData::Date(_) => {
            let v = chrono::NaiveDate::from_sql(data).map_err(map_err)?;
            b.push(v.map(|d| Cell::Date(days_from_date(d))).unwrap_or(Cell::Null))
        }
        ColumnData::Time(_) => {
            let v = chrono::NaiveTime::from_sql(data).map_err(map_err)?;
            b.push(v.map(|t| Cell::Time(micros_from_time(t))).unwrap_or(Cell::Null))
        }
    }
}

/// `x * 10^scale` rounded to the nearest integer (money -> Decimal(19,4)).
pub fn scale_f64(x: f64, scale: i8) -> i128 {
    (x * 10f64.powi(scale as i32)).round() as i128
}

fn push_row(b: &mut ArrowBatcher, row: &Row) -> Result<()> {
    for (i, (_, data)) in row.cells().enumerate() {
        push_cell(b, data, i)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Source
// ---------------------------------------------------------------------------------------------

/// Drive one query on `client`, sending Arrow batches into `tx`. The schema (or the error
/// that prevented it) is reported once through `ready`.
async fn pump(
    client: &mut TdsClient,
    sql: &str,
    tx: &mpsc::Sender<Result<Batch>>,
    ready: &mut Option<oneshot::Sender<Result<SchemaRef>>>,
    max_rows: usize,
) -> Result<()> {
    let mut stream = client.query(sql, &[]).await.map_err(map_err)?;
    let mut batcher: Option<ArrowBatcher> = None;
    let mut rows: u64 = 0;
    while let Some(item) = stream.try_next().await.map_err(map_err)? {
        match item {
            QueryItem::Metadata(meta) => {
                if let Some(b) = batcher.as_mut() {
                    tracing::warn!(target: "bulkdb::mssql", "the query produced a second result set; only the first one is exported");
                    // Flush what the first result set left in the batcher, then abandon the
                    // rest of the response with the connection (see caller).
                    if let Some(batch) = b.flush()? {
                        tx.send(Ok(Batch::Arrow(batch))).await.map_err(|_| CoreError::Cancelled)?;
                    }
                    return Err(CoreError::Internal("second result set".into()));
                }
                for c in meta.columns() {
                    if let Some(hint) = unsupported_hint(c.column_type()) {
                        return Err(CoreError::query(format!("column '{}' has an unsupported type — {hint}", c.name())));
                    }
                }
                let cols: Vec<(String, ColKind)> = meta.columns().iter().map(|c| (c.name().to_string(), col_kind(c.column_type()))).collect();
                let schema = schema_from(&cols);
                let kinds = cols.iter().map(|(_, k)| *k).collect();
                batcher = Some(ArrowBatcher::with_limits(schema.clone(), kinds, max_rows, BATCH_BYTES));
                if let Some(r) = ready.take() {
                    let _ = r.send(Ok(schema));
                }
            }
            QueryItem::Row(row) => {
                let b = batcher.as_mut().ok_or_else(|| CoreError::Internal("row before result metadata".into()))?;
                push_row(b, &row)?;
                rows += 1;
                if b.end_row()? {
                    if let Some(batch) = b.flush()? {
                        tx.send(Ok(Batch::Arrow(batch))).await.map_err(|_| CoreError::Cancelled)?;
                    }
                }
            }
        }
    }
    match batcher.as_mut() {
        Some(b) => {
            if let Some(batch) = b.flush()? {
                tx.send(Ok(Batch::Arrow(batch))).await.map_err(|_| CoreError::Cancelled)?;
            }
        }
        None => return Err(CoreError::query("the statement returned no result set")),
    }
    tracing::debug!(target: "bulkdb::mssql", rows, "result set drained");
    Ok(())
}

/// Internal marker for "the connection task hit the second result set": the client is dropped
/// (the server stops sending) but the export itself is complete and successful.
fn is_second_result_set(e: &CoreError) -> bool {
    matches!(e, CoreError::Internal(m) if m == "second result set")
}

#[async_trait]
impl Source for MssqlDatabase {
    async fn probe(&self) -> Result<ServerInfo> {
        let mut guard = self.lock().await;
        let client = self.ensure_client(&mut guard).await?;
        const PROBE: &str = "SELECT CAST(@@VERSION AS nvarchar(max)) AS version, \
            CAST(SERVERPROPERTY('ProductVersion') AS nvarchar(128)) AS product_version, \
            SUSER_SNAME() AS login_name, DB_NAME() AS db, \
            CASE WHEN HAS_PERMS_BY_NAME(DB_NAME(), 'DATABASE', 'INSERT') = 1 OR IS_MEMBER('db_datawriter') = 1 OR IS_SRVROLEMEMBER('sysadmin') = 1 THEN 1 ELSE 0 END AS can_write, \
            CAST(SERVERPROPERTY('Edition') AS nvarchar(128)) AS edition, @@LOCK_TIMEOUT AS lock_timeout";
        let rows = query_rows(client, PROBE, &[]).await?;
        let row = rows.first().ok_or_else(|| CoreError::Internal("probe query returned no row".into()))?;
        let full_version = get_string(row, 0).unwrap_or_default();
        let version = full_version.lines().next().unwrap_or("").trim().to_string();
        let product_version = get_string(row, 1);
        let user = get_string(row, 2);
        let database = get_string(row, 3);
        let can_write = get_bool(row, 4);
        let edition = get_string(row, 5);
        let lock_timeout = get_i64(row, 6);

        let mut settings = BTreeMap::new();
        let mut notes = Vec::new();
        if let Some(v) = &product_version {
            settings.insert("product_version".into(), v.clone());
        }
        if let Some(e) = &edition {
            settings.insert("edition".into(), e.clone());
        }
        if let Some(l) = lock_timeout {
            settings.insert("lock_timeout".into(), l.to_string());
        }

        // Memory: physical RAM when `VIEW SERVER STATE` is granted, else the configured cap.
        let mut ram_bytes = None;
        let mut ram_source = None;
        match query_rows(client, "SELECT CAST(physical_memory_kb AS bigint) FROM sys.dm_os_sys_memory", &[]).await {
            Ok(rows) => {
                if let Some(kb) = rows.first().and_then(|r| get_u64(r, 0)) {
                    ram_bytes = Some(kb * 1024);
                    ram_source = Some("sys.dm_os_sys_memory.physical_memory_kb".to_string());
                }
            }
            Err(e) => tracing::debug!(target: "bulkdb::mssql", "sys.dm_os_sys_memory not readable: {e}"),
        }
        match query_rows(
            client,
            "SELECT name, CAST(value_in_use AS bigint) FROM sys.configurations WHERE name IN ('max server memory (MB)', 'max degree of parallelism', 'cost threshold for parallelism')",
            &[],
        )
        .await
        {
            Ok(rows) => {
                for r in &rows {
                    let (Some(name), Some(v)) = (get_string(r, 0), get_i64(r, 1)) else { continue };
                    settings.insert(name.clone(), v.to_string());
                    if name == "max server memory (MB)" && ram_bytes.is_none() && v > 0 && v != 2_147_483_647 {
                        ram_bytes = Some(v as u64 * 1024 * 1024);
                        ram_source = Some("sys.configurations 'max server memory (MB)'".to_string());
                    }
                }
            }
            Err(e) => tracing::debug!(target: "bulkdb::mssql", "sys.configurations not readable: {e}"),
        }
        if ram_bytes.is_none() {
            notes.push("Server memory is unknown (no VIEW SERVER STATE permission and no max server memory cap).".into());
        }
        if can_write {
            notes.push(format!("The login can write to {}; a read-only login is recommended for extraction.", database.as_deref().unwrap_or("the database")));
        }
        Ok(ServerInfo { driver: "mssql".into(), version, user, database, ram_bytes, ram_source, settings, can_write: Some(can_write), notes })
    }

    async fn apply_session(&self, opts: &SessionOpts) -> Result<()> {
        let mut sql = String::from("SET NOCOUNT ON; SET ARITHABORT ON; SET TRANSACTION ISOLATION LEVEL READ COMMITTED;");
        if let Some(ms) = opts.lock_timeout_ms {
            let ms = ms.min(i32::MAX as u64) as i64;
            sql.push_str(&format!(" SET LOCK_TIMEOUT {ms};"));
            self.lock_timeout_ms.store(ms, Ordering::Relaxed);
        }
        let mut guard = self.lock().await;
        let client = self.ensure_client(&mut guard).await?;
        exec_batch(client, &sql).await
    }

    async fn stream(&self, q: &Query, opts: &SessionOpts, cancel: CancellationToken) -> Result<SourceStream> {
        let mut client = {
            let mut guard = self.lock().await;
            self.ensure_client(&mut guard).await?;
            guard.take().expect("ensured above")
        };
        self.slot.streaming.store(true, Ordering::Release);
        let slot = self.slot.clone();
        let sql = q.sql.clone();
        let max_rows = opts.fetch_batch_rows.map(|n| n as usize).filter(|n| *n > 0).unwrap_or(BATCH_ROWS).min(BATCH_ROWS);
        let timeout = opts.statement_timeout_ms.filter(|ms| *ms > 0).map(Duration::from_millis);
        let (tx, rx) = mpsc::channel::<Result<Batch>>(2);
        let (ready_tx, ready_rx) = oneshot::channel::<Result<SchemaRef>>();

        tokio::spawn(async move {
            let mut ready = Some(ready_tx);
            let outcome: Result<()> = {
                let work = AssertUnwindSafe(pump(&mut client, &sql, &tx, &mut ready, max_rows)).catch_unwind();
                let deadline = async {
                    match timeout {
                        Some(d) => tokio::time::sleep(d).await,
                        None => std::future::pending::<()>().await,
                    }
                };
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => Err(CoreError::Cancelled),
                    _ = deadline => Err(CoreError::Timeout(format!("statement timeout of {} ms exceeded", timeout.map(|d| d.as_millis()).unwrap_or(0)))),
                    r = work => match r {
                        Ok(r) => r,
                        Err(panic) => Err(CoreError::Internal(format!("mssql driver panicked while decoding rows: {}", panic_message(&panic)))),
                    },
                }
            };
            // Decide what happens to the connection. A cleanly drained stream leaves the
            // session reusable; anything else is dropped so the server aborts the batch.
            let reusable = match &outcome {
                Ok(()) => true,
                Err(CoreError::Query { .. }) | Err(CoreError::Auth(_)) => true,
                Err(_) => false,
            };
            if reusable {
                *slot.client.lock().await = Some(client);
            } else {
                drop(client);
                *slot.session_id.write().unwrap() = None;
            }
            slot.streaming.store(false, Ordering::Release);
            match outcome {
                Ok(()) => {}
                Err(e) if is_second_result_set(&e) => {}
                Err(e) => {
                    if let Some(r) = ready.take() {
                        let _ = r.send(Err(e));
                    } else {
                        let _ = tx.send(Err(e)).await;
                    }
                }
            }
        });

        let schema = match ready_rx.await {
            Ok(Ok(schema)) => schema,
            Ok(Err(e)) => return Err(e),
            Err(_) => return Err(CoreError::Cancelled),
        };
        let stream = futures::stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|item| (item, rx)) });
        Ok(SourceStream { schema: Some(schema), stream: Box::pin(stream) })
    }

    async fn explain(&self, sql: &str, analyze: bool) -> Result<Option<PlanSummary>> {
        let started = Instant::now();
        let mode = if analyze { "STATISTICS XML" } else { "SHOWPLAN_XML" };
        let mut guard = self.lock().await;
        let client = self.ensure_client(&mut guard).await?;
        exec_batch(client, "SET LOCK_TIMEOUT 3000").await?;
        exec_batch(client, &format!("SET {mode} ON")).await?;
        let work = fetch_plan_xml(client, sql);
        let result = if analyze {
            work.await
        } else {
            match tokio::time::timeout(Duration::from_secs(15), work).await {
                Ok(r) => r,
                Err(_) => Err(CoreError::Timeout("EXPLAIN took longer than 15 s".into())),
            }
        };
        // Always leave showplan mode. After a timeout the response is half read, so the
        // connection is dropped instead (the next call reconnects with fresh session state).
        if matches!(result, Err(CoreError::Timeout(_))) {
            self.drop_connection(&mut guard);
        } else {
            let off = exec_batch(client, &format!("SET {mode} OFF")).await;
            let restore = exec_batch(client, &format!("SET LOCK_TIMEOUT {}", self.lock_timeout_ms.load(Ordering::Relaxed))).await;
            if let Err(e) = off.and(restore) {
                tracing::warn!(target: "bulkdb::mssql", "could not restore session options after EXPLAIN: {e}");
                self.drop_connection(&mut guard);
            }
        }
        let (xml, rows_seen) = result?;
        let mut plan = parse_showplan(&xml, analyze);
        plan.elapsed_ms = started.elapsed().as_millis() as u64;
        if analyze && plan.estimated_rows.is_none() {
            plan.estimated_rows = Some(rows_seen);
        }
        Ok(Some(plan))
    }

    fn session_id(&self) -> Option<String> {
        self.slot.session_id.read().unwrap().clone()
    }

    fn driver_id(&self) -> &'static str {
        "mssql"
    }
}

impl Database for MssqlDatabase {}

fn panic_message(p: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = p.downcast_ref::<&str>() {
        s.to_string()
    } else if let Some(s) = p.downcast_ref::<String>() {
        s.clone()
    } else {
        "unknown panic".into()
    }
}

/// Run `sql` in showplan / statistics mode and return the last `<ShowPlanXML>` document plus
/// the number of ordinary rows that were streamed past (STATISTICS XML really runs the query).
async fn fetch_plan_xml(client: &mut TdsClient, sql: &str) -> Result<(String, u64)> {
    let mut stream = client.simple_query(sql).await.map_err(map_err)?;
    let mut xml: Option<String> = None;
    let mut rows: u64 = 0;
    while let Some(item) = stream.try_next().await.map_err(map_err)? {
        if let QueryItem::Row(row) = item {
            let plan = if row.len() == 1 { get_string(&row, 0).filter(|s| s.trim_start().starts_with("<ShowPlanXML")) } else { None };
            match plan {
                Some(p) => xml = Some(p),
                None => rows += 1,
            }
        }
    }
    match xml {
        Some(x) => Ok((x, rows)),
        None => Err(CoreError::query("the server returned no XML showplan for this statement")),
    }
}

// ---------------------------------------------------------------------------------------------
// Showplan XML parsing (regex based; no XML crate in the workspace)
// ---------------------------------------------------------------------------------------------

struct PlanRegex {
    stmt: Regex,
    token: Regex,
    attr: Regex,
}

fn plan_regex() -> &'static PlanRegex {
    static RE: OnceLock<PlanRegex> = OnceLock::new();
    RE.get_or_init(|| PlanRegex {
        stmt: Regex::new(r"<StmtSimple\b([^>]*)>").unwrap(),
        token: Regex::new(r"<RelOp\b([^>]*)>|(</RelOp>)|<Object\b([^>]*)>|<RunTimeCountersPerThread\b([^>]*)>|<Warnings\b([^>]*)>|<SpillToTempDb\b([^>]*)>").unwrap(),
        attr: Regex::new(r#"([A-Za-z_][A-Za-z0-9_:.-]*)="([^"]*)""#).unwrap(),
    })
}

fn parse_attrs(s: &str) -> BTreeMap<String, String> {
    plan_regex().attr.captures_iter(s).map(|c| (c[1].to_string(), unescape_xml(&c[2]))).collect()
}

fn unescape_xml(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    s.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&apos;", "'").replace("&#xA;", "\n").replace("&#xD;", "\r").replace("&amp;", "&")
}

fn strip_brackets(s: &str) -> String {
    let t = s.trim();
    let t = t.strip_prefix('[').unwrap_or(t);
    let t = t.strip_suffix(']').unwrap_or(t);
    t.replace("]]", "]")
}

fn attr_f64(a: &BTreeMap<String, String>, key: &str) -> Option<f64> {
    a.get(key).and_then(|v| v.trim().parse::<f64>().ok())
}

/// Parse a `<ShowPlanXML>` document into a [`PlanSummary`]. Public for tests and for any
/// caller that already holds the XML.
pub fn parse_showplan(xml: &str, analyzed: bool) -> PlanSummary {
    let re = plan_regex();
    let mut stack: Vec<PlanNode> = Vec::new();
    let mut roots: Vec<PlanNode> = Vec::new();
    let mut tables: BTreeSet<TableRef> = BTreeSet::new();
    let mut warnings: Vec<PlanWarning> = Vec::new();

    for cap in re.token.captures_iter(xml) {
        if let Some(a) = cap.get(1) {
            let attrs = parse_attrs(a.as_str());
            let physical = attrs.get("PhysicalOp").cloned();
            let logical = attrs.get("LogicalOp").cloned();
            let mut extra = BTreeMap::new();
            for key in ["LogicalOp", "NodeId", "Parallel", "EstimateIO", "EstimateCPU", "EstimatedExecutionMode", "TableCardinality"] {
                if let Some(v) = attrs.get(key) {
                    if key != "LogicalOp" || Some(v) != physical.as_ref() {
                        extra.insert(key.to_string(), v.clone());
                    }
                }
            }
            stack.push(PlanNode {
                node_type: physical.or(logical).unwrap_or_else(|| "RelOp".into()),
                relation: None,
                alias: None,
                plan_rows: attr_f64(&attrs, "EstimateRows"),
                plan_width: attr_f64(&attrs, "AvgRowSize").map(|w| w.max(0.0) as u32),
                total_cost: attr_f64(&attrs, "EstimatedTotalSubtreeCost"),
                startup_cost: None,
                actual_rows: None,
                actual_loops: None,
                actual_time_ms: None,
                parent_relationship: None,
                children: Vec::new(),
                extra,
            });
        } else if cap.get(2).is_some() {
            if let Some(node) = stack.pop() {
                match stack.last_mut() {
                    Some(parent) => parent.children.push(node),
                    None => roots.push(node),
                }
            }
        } else if let Some(a) = cap.get(3) {
            let attrs = parse_attrs(a.as_str());
            let name = attrs.get("Table").map(|t| strip_brackets(t)).unwrap_or_default();
            if name.is_empty() {
                continue;
            }
            let r = TableRef {
                database: attrs.get("Database").map(|d| strip_brackets(d)).filter(|d| !d.is_empty()),
                schema: attrs.get("Schema").map(|s| strip_brackets(s)).filter(|s| !s.is_empty()),
                name,
            };
            if let Some(top) = stack.last_mut() {
                if top.relation.is_none() {
                    top.relation = Some(r.clone());
                    top.alias = attrs.get("Alias").map(|s| strip_brackets(s)).filter(|s| !s.is_empty());
                }
                if let Some(ix) = attrs.get("Index").map(|s| strip_brackets(s)).filter(|s| !s.is_empty()) {
                    top.extra.entry("Index".into()).or_insert(ix);
                }
            }
            tables.insert(r);
        } else if let Some(a) = cap.get(4) {
            let attrs = parse_attrs(a.as_str());
            if let Some(top) = stack.last_mut() {
                if let Some(v) = attr_f64(&attrs, "ActualRows") {
                    top.actual_rows = Some(top.actual_rows.unwrap_or(0.0) + v);
                }
                if let Some(v) = attr_f64(&attrs, "ActualExecutions") {
                    top.actual_loops = Some(top.actual_loops.unwrap_or(0.0).max(v));
                }
                if let Some(v) = attr_f64(&attrs, "ActualElapsedms") {
                    top.actual_time_ms = Some(top.actual_time_ms.unwrap_or(0.0).max(v));
                }
            }
        } else if let Some(a) = cap.get(5) {
            let attrs = parse_attrs(a.as_str());
            if attrs.get("NoJoinPredicate").map(|v| v == "true").unwrap_or(false) {
                let at = stack.last().map(|n| n.node_type.clone()).unwrap_or_default();
                warnings.push(PlanWarning {
                    code: "NO_JOIN_PREDICATE".into(),
                    severity: Severity::Danger,
                    message: "A join has no predicate (cartesian product); the result may be far larger than expected.".into(),
                    detail: Some(format!("operator: {at}")),
                });
            }
        } else if cap.get(6).is_some() {
            let at = stack.last().map(|n| n.node_type.clone()).unwrap_or_default();
            warnings.push(PlanWarning {
                code: "SORT_SPILL".into(),
                severity: Severity::Warning,
                message: format!("{at} spilled to tempdb: the server needed more memory than it was granted."),
                detail: None,
            });
        }
    }
    while let Some(node) = stack.pop() {
        match stack.last_mut() {
            Some(parent) => parent.children.push(node),
            None => roots.push(node),
        }
    }
    let root = match roots.len() {
        0 => None,
        1 => roots.pop(),
        _ => Some(PlanNode { node_type: "Batch".into(), children: roots, ..Default::default() }),
    };

    let stmt = re.stmt.captures(xml).map(|c| parse_attrs(&c[1])).unwrap_or_default();
    let estimated_rows = attr_f64(&stmt, "StatementEstRows").or(root.as_ref().and_then(|r| r.plan_rows)).map(|r| r.round().max(0.0) as u64);
    let total_cost = attr_f64(&stmt, "StatementSubTreeCost").or(root.as_ref().and_then(|r| r.total_cost));
    let estimated_width = root.as_ref().and_then(|r| r.plan_width);
    let estimated_bytes = match (estimated_rows, estimated_width) {
        (Some(r), Some(w)) => Some(r.saturating_mul(w as u64)),
        _ => None,
    };
    if let Some(r) = &root {
        heuristics(r, &mut warnings);
    }
    warnings.dedup_by(|a, b| a.code == b.code && a.message == b.message);
    PlanSummary {
        estimated_rows,
        estimated_width,
        total_cost,
        estimated_bytes,
        analyzed,
        elapsed_ms: 0,
        warnings,
        tables: tables.into_iter().collect(),
        root,
        raw: xml.to_string(),
        raw_format: "xml".into(),
    }
}

/// Operator-level warnings the UI shows before an extraction starts.
fn heuristics(node: &PlanNode, out: &mut Vec<PlanWarning>) {
    const BIG: f64 = 1_000_000.0;
    let rows = node.plan_rows.unwrap_or(0.0);
    let rel = node.relation.as_ref().map(|r| r.qualified()).unwrap_or_else(|| "?".into());
    match node.node_type.as_str() {
        "Table Scan" | "Clustered Index Scan" | "Index Scan" if rows >= BIG => out.push(PlanWarning {
            code: "SEQ_SCAN_LARGE".into(),
            severity: Severity::Info,
            message: format!("Full scan of {rel} (~{} rows).", fmt_rows(rows)),
            detail: Some("Expected for a whole-table extraction; add a WHERE clause if you need less.".into()),
        }),
        "Sort" if rows >= BIG => out.push(PlanWarning {
            code: "SORT_LARGE".into(),
            severity: Severity::Warning,
            message: format!("Sorting ~{} rows on the server needs a large memory grant and may spill to tempdb.", fmt_rows(rows)),
            detail: Some("Drop ORDER BY unless the file order matters.".into()),
        }),
        "Hash Match" if rows >= BIG => out.push(PlanWarning {
            code: "HASH_LARGE".into(),
            severity: Severity::Warning,
            message: format!("Hash join / aggregate over ~{} rows needs a large memory grant.", fmt_rows(rows)),
            detail: None,
        }),
        _ => {}
    }
    for c in &node.children {
        heuristics(c, out);
    }
}

fn fmt_rows(r: f64) -> String {
    if r >= 1e9 {
        format!("{:.1}G", r / 1e9)
    } else if r >= 1e6 {
        format!("{:.1}M", r / 1e6)
    } else if r >= 1e3 {
        format!("{:.0}K", r / 1e3)
    } else {
        format!("{r:.0}")
    }
}

// ---------------------------------------------------------------------------------------------
// Tests (no database)
// ---------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn params(port: u16, fields: &[(&str, &str)]) -> ConnectParams {
        ConnectParams {
            driver: "mssql".into(),
            host: "db.example.com".into(),
            port,
            database: Some("shop".into()),
            user: Some("reader".into()),
            password: Some("pw".into()),
            fields: fields.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
            application_name: "bulkdb-test".into(),
            connect_timeout: Duration::from_secs(1),
            ssl: SslMode::Prefer,
        }
    }

    /// Legacy `datetime` arrives as days since 1900 + 1/300 s ticks; the export shows it at its
    /// millisecond accuracy, like SQL Server itself. `datetime2` keeps every digit.
    #[test]
    fn legacy_datetime_is_rounded_to_the_millisecond() {
        use arrow_array::{Array, TimestampMicrosecondArray};
        let day = |y, m, d| (chrono::NaiveDate::from_ymd_opt(y, m, d).unwrap() - chrono::NaiveDate::from_ymd_opt(1900, 1, 1).unwrap()).num_days() as i32;
        let base = chrono::NaiveDate::from_ymd_opt(2024, 1, 2).unwrap().and_hms_opt(3, 4, 5).unwrap().and_utc().timestamp_micros();
        let secs = (3 * 3600 + 4 * 60 + 5) * 300;
        let decode = |data: ColumnData<'static>| {
            let kinds = vec![ColKind::Timestamp];
            let mut b = ArrowBatcher::new(schema_from(&[("t".to_string(), ColKind::Timestamp)]), kinds);
            push_cell(&mut b, &data, 0).unwrap();
            b.end_row().unwrap();
            let batch = b.flush().unwrap().unwrap();
            batch.column(0).as_any().downcast_ref::<TimestampMicrosecondArray>().unwrap().value(0)
        };
        let dt = |days, ticks| ColumnData::DateTime(Some(tiberius::time::DateTime::new(days, ticks)));

        // '05.123' is stored as 37 ticks = 0.1233333 s.
        assert_eq!(decode(dt(day(2024, 1, 2), secs + 37)), base + 123_000);
        // '05.997' (299 ticks = 0.9966667 s) and a whole second stay what SQL Server shows.
        assert_eq!(decode(dt(day(2024, 1, 2), secs + 299)), base + 997_000);
        assert_eq!(decode(dt(day(2024, 1, 2), secs)), base);
        // Before 1970 (negative epoch micros) rounds the same way: 1753-01-01 00:00:00.003.
        let early = chrono::NaiveDate::from_ymd_opt(1753, 1, 1).unwrap().and_hms_opt(0, 0, 0).unwrap().and_utc().timestamp_micros();
        assert_eq!(decode(dt(day(1753, 1, 1), 1)), early + 3_000);

        assert_eq!(round_legacy_datetime_micros(-1_876_667), -1_877_000);
        assert_eq!(round_legacy_datetime_micros(123_333), 123_000);
    }

    #[test]
    fn column_type_mapping() {
        use ColumnType::*;
        assert_eq!(col_kind(Bit), ColKind::Bool);
        assert_eq!(col_kind(Bitn), ColKind::Bool);
        for t in [Int1, Int2, Int4, Int8, Intn] {
            assert_eq!(col_kind(t), ColKind::Int64);
        }
        for t in [Float4, Float8, Floatn] {
            assert_eq!(col_kind(t), ColKind::Float64);
        }
        assert_eq!(col_kind(Money), ColKind::Decimal { precision: 19, scale: 4 });
        assert_eq!(col_kind(Money4), ColKind::Decimal { precision: 19, scale: 4 });
        assert_eq!(col_kind(Decimaln), ColKind::Utf8);
        assert_eq!(col_kind(Numericn), ColKind::Utf8);
        for t in [Datetime, Datetime2, Datetimen, Datetime4] {
            assert_eq!(col_kind(t), ColKind::Timestamp);
        }
        assert_eq!(col_kind(DatetimeOffsetn), ColKind::TimestampTz);
        assert_eq!(col_kind(Daten), ColKind::Date);
        assert_eq!(col_kind(Timen), ColKind::Time);
        for t in [BigVarChar, BigChar, NVarchar, NChar, Text, NText, Xml, Udt, Guid, Null] {
            assert_eq!(col_kind(t), ColKind::Utf8);
        }
        for t in [BigVarBin, BigBinary, Image, SSVariant] {
            assert_eq!(col_kind(t), ColKind::Binary);
        }
        assert!(unsupported_hint(SSVariant).is_some());
        assert!(unsupported_hint(Udt).is_some());
        assert!(unsupported_hint(NVarchar).is_none());
    }

    #[test]
    fn numeric_and_money_formatting() {
        assert_eq!(format_numeric(12345, 2), "123.45");
        assert_eq!(format_numeric(-12345, 2), "-123.45");
        assert_eq!(format_numeric(-5, 3), "-0.005");
        assert_eq!(format_numeric(7, 0), "7");
        assert_eq!(format_numeric(0, 4), "0.0000");
        assert_eq!(scale_f64(12.3456, 4), 123456);
        assert_eq!(scale_f64(-0.0001, 4), -1);
    }

    #[test]
    fn error_code_mapping() {
        assert!(matches!(map_server_error(18456, "Login failed for user 'x'."), CoreError::Auth(_)));
        assert!(matches!(map_server_error(1222, "Lock request time out period exceeded."), CoreError::Timeout(_)));
        assert!(matches!(map_server_error(1205, "deadlock victim"), CoreError::Timeout(_)));
        assert!(matches!(map_server_error(4060, "Cannot open database"), CoreError::Connect(_)));
        assert!(matches!(map_server_error(53, "network path"), CoreError::Connect(_)));
        assert!(matches!(map_server_error(10061, "refused"), CoreError::Connect(_)));
        match map_server_error(208, "Invalid object name 'nope'.") {
            CoreError::Query { code, msg } => {
                assert_eq!(code.as_deref(), Some("208"));
                assert_eq!(msg, "Invalid object name 'nope'.");
            }
            other => panic!("unexpected {other:?}"),
        }
        let io = map_err(TdsError::Io { kind: std::io::ErrorKind::ConnectionReset, message: "reset".into() });
        assert!(matches!(io, CoreError::Connect(_)));
        assert!(matches!(map_err(TdsError::Tls("bad cert".into())), CoreError::Connect(_)));
        assert!(matches!(map_err(TdsError::Protocol("x".into())), CoreError::Internal(_)));
    }

    #[test]
    fn identifier_quoting_escapes_brackets() {
        assert_eq!(quote_ident("plain"), "[plain]");
        assert_eq!(quote_ident("we]ird"), "[we]]ird]");
        assert_eq!(quote_ident("[x]"), "[[x]]]");
        assert_eq!(strip_brackets("[dbo]"), "dbo");
        assert_eq!(strip_brackets("[a]]b]"), "a]b");
    }

    #[test]
    fn config_defaults_and_named_instance() {
        let cfg = build_config(&params(0, &[])).unwrap();
        assert_eq!(cfg.get_addr(), "db.example.com:1433");
        let cfg = build_config(&params(1500, &[("encrypt", "false"), ("trust_server_certificate", "true")])).unwrap();
        assert_eq!(cfg.get_addr(), "db.example.com:1500");
        let err = build_config(&params(0, &[("instance", "SQLEXPRESS")])).unwrap_err();
        assert!(matches!(err, CoreError::Connect(ref m) if m.contains("SQLEXPRESS") && m.contains("port")), "{err}");
        assert!(build_config(&params(1434, &[("instance", "SQLEXPRESS")])).is_ok());
    }

    const PLAN: &str = r#"<ShowPlanXML xmlns="http://schemas.microsoft.com/sqlserver/2004/07/showplan" Version="1.564" Build="15.0.2000.5"><BatchSequence><Batch><Statements><StmtSimple StatementCompId="1" StatementEstRows="250000" StatementId="1" StatementOptmLevel="FULL" StatementSubTreeCost="12.5" StatementText="SELECT o.id, c.name FROM dbo.orders o JOIN sales.customers c ON c.id = o.customer_id WHERE o.qty &gt; 1" StatementType="SELECT"><QueryPlan CachedPlanSize="32"><RelOp AvgRowSize="47" EstimateCPU="1.0" EstimateIO="0.5" EstimateRows="250000" LogicalOp="Inner Join" NodeId="0" Parallel="false" PhysicalOp="Hash Match" EstimatedTotalSubtreeCost="12.5"><OutputList/><Hash><RelOp AvgRowSize="20" EstimateRows="1000" LogicalOp="Clustered Index Scan" NodeId="1" PhysicalOp="Clustered Index Scan" EstimatedTotalSubtreeCost="0.1"><IndexScan Ordered="false"><Object Database="[shop]" Schema="[sales]" Table="[customers]" Index="[PK_customers]" Alias="[c]" IndexKind="Clustered"/></IndexScan></RelOp><RelOp AvgRowSize="30" EstimateRows="2500000" LogicalOp="Table Scan" NodeId="2" PhysicalOp="Table Scan" EstimatedTotalSubtreeCost="2.0"><TableScan Ordered="false"><Object Database="[shop]" Schema="[dbo]" Table="[orders]" Alias="[o]"/></TableScan></RelOp></Hash></RelOp></QueryPlan></StmtSimple></Statements></Batch></BatchSequence></ShowPlanXML>"#;

    #[test]
    fn parses_showplan_xml() {
        let p = parse_showplan(PLAN, false);
        assert_eq!(p.estimated_rows, Some(250_000));
        assert_eq!(p.total_cost, Some(12.5));
        assert_eq!(p.estimated_width, Some(47));
        assert_eq!(p.estimated_bytes, Some(250_000 * 47));
        assert!(!p.analyzed);
        assert_eq!(p.raw_format, "xml");
        assert_eq!(p.raw, PLAN);
        let names: Vec<String> = p.tables.iter().map(|t| t.qualified()).collect();
        assert_eq!(names, vec!["dbo.orders", "sales.customers"]);
        assert_eq!(p.tables[0].database.as_deref(), Some("shop"));
        let root = p.root.expect("root");
        assert_eq!(root.node_type, "Hash Match");
        assert_eq!(root.extra.get("LogicalOp").map(String::as_str), Some("Inner Join"));
        assert_eq!(root.children.len(), 2);
        let scan = &root.children[0];
        assert_eq!(scan.node_type, "Clustered Index Scan");
        assert_eq!(scan.relation.as_ref().map(|r| r.qualified()), Some("sales.customers".into()));
        assert_eq!(scan.alias.as_deref(), Some("c"));
        assert_eq!(scan.extra.get("Index").map(String::as_str), Some("PK_customers"));
        assert_eq!(scan.plan_rows, Some(1000.0));
        let ts = &root.children[1];
        assert_eq!(ts.node_type, "Table Scan");
        assert_eq!(ts.relation.as_ref().map(|r| r.qualified()), Some("dbo.orders".into()));
        assert!(p.warnings.iter().any(|w| w.code == "SEQ_SCAN_LARGE"), "{:?}", p.warnings);
    }

    #[test]
    fn parses_actual_counters_and_warnings() {
        let xml = r#"<ShowPlanXML><StmtSimple StatementEstRows="10" StatementSubTreeCost="0.01"><QueryPlan><RelOp PhysicalOp="Nested Loops" LogicalOp="Inner Join" EstimateRows="10" AvgRowSize="9"><Warnings NoJoinPredicate="true"/><RunTimeInformation><RunTimeCountersPerThread Thread="1" ActualRows="7" ActualExecutions="1" ActualElapsedms="3"/><RunTimeCountersPerThread Thread="0" ActualRows="5" ActualExecutions="1" ActualElapsedms="4"/></RunTimeInformation><NestedLoops><RelOp PhysicalOp="Sort" EstimateRows="10"><Warnings><SpillToTempDb SpillLevel="1"/></Warnings><Sort><RelOp PhysicalOp="Table Scan" EstimateRows="10"><TableScan><Object Schema="[dbo]" Table="[t]"/></TableScan></RelOp></Sort></RelOp></NestedLoops></RelOp></QueryPlan></StmtSimple></ShowPlanXML>"#;
        let p = parse_showplan(xml, true);
        assert!(p.analyzed);
        let root = p.root.expect("root");
        assert_eq!(root.actual_rows, Some(12.0));
        assert_eq!(root.actual_loops, Some(1.0));
        assert_eq!(root.actual_time_ms, Some(4.0));
        assert_eq!(root.children.len(), 1);
        assert_eq!(root.children[0].node_type, "Sort");
        assert_eq!(root.children[0].children[0].relation.as_ref().map(|r| r.qualified()), Some("dbo.t".into()));
        assert!(p.warnings.iter().any(|w| w.code == "NO_JOIN_PREDICATE" && w.severity == Severity::Danger));
        assert!(p.warnings.iter().any(|w| w.code == "SORT_SPILL"));
    }

    #[test]
    fn parses_empty_or_garbage_plan() {
        let p = parse_showplan("<nothing/>", false);
        assert!(p.root.is_none());
        assert!(p.tables.is_empty());
        assert_eq!(p.estimated_rows, None);
    }

    #[test]
    fn xml_unescape() {
        assert_eq!(unescape_xml("a &lt; b &amp;&amp; c &quot;d&quot;"), "a < b && c \"d\"");
        assert_eq!(unescape_xml("plain"), "plain");
    }
}
