//! Pre-4.1 MySQL client (MySQL 3.23 / 4.0, protocol 10 without `CLIENT_PROTOCOL_41`).
//! Owner: **legacy**. Design: `docs/mysql-compat.md` sections 3, 4 and 7.
//!
//! mysql_async only speaks the 4.1 handshake, so a 4.0 server needs this small client:
//! greeting + `HandshakeResponse320` with the 3.23 scramble ([`handshake`]), packet framing
//! with 16 MB continuation ([`packet`]), `COM_QUERY` / text result sets ([`protocol`],
//! [`conn`]) and the extraction stream ([`stream`]). No TLS, no compression, no prepared
//! statements (4.0 has none).
//!
//! [`MySqlLegacyDatabase`] implements [`MysqlRawQuery`]; probe, session settings, EXPLAIN,
//! catalog and monitor are the shared version-aware implementations in
//! [`crate::source::mysql_common`], `catalog::mysql::generic` and `monitor::mysql::generic`.
//!
//! Connect sequence: greeting (refuse 4.1+ with a `CONNECT` error naming `protocol=auto`) ->
//! auth (`charset` = the explicit encoding, else the greeting's charset, else latin1) ->
//! `SHOW VARIABLES LIKE 'character_set'` -> [`plan_text`](crate::source::mysql_dispatch::plan_text)
//! -> `USE` is implicit via `CONNECT_WITH_DB`. The password is kept in `params` only to open
//! side connections (cancel) and to reconnect; it is never logged or shown by `Debug`.
//!
//! Host-block rule: a socket is never dropped after the greeting without a login attempt
//! (MySQL counts that toward `max_connect_errors` and blocks the host until `FLUSH HOSTS`).
//! What needs no server is validated before connecting ([`precheck`]); what needs the greeting
//! (4.1+ server, TLS required, login text the server charset cannot encode) ends with
//! [`decline`]: a credential-free login whose answer is read before the socket is closed.

pub mod conn;
pub mod handshake;
pub mod packet;
pub mod protocol;
pub mod stream;

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::net::TcpStream;
use tokio::sync::{Mutex, MutexGuard};
use tokio_util::sync::CancellationToken;

pub use conn::LegacyConn;
pub use handshake::{decline, peek_greeting, Greeting};

use super::mysql::{LexMode, ServerVersion};
use super::mysql_common::{self, kill_sql, MetaRows, MysqlCaps, MysqlRawQuery, ServerFacts, WriteSamples, ER_NO_SUCH_THREAD, META_ROW_LIMIT};
use super::mysql_dispatch::{plan_text, EncodingSetting, MysqlConnOpts};
use super::{ConnectParams, Database, Query, Source, SourceStream};
use crate::catalog::Catalog;
use crate::encoding::{Charset, CollationMap};
use crate::error::{CoreError, Result};
use crate::monitor::{JobHandle, Monitor, ServerSnapshot};
use crate::types::{ColumnInfo, DatabaseInfo, IndexInfo, PlanSummary, SchemaInfo, ServerInfo, SessionInfo, SessionOpts, TableInfo, TableRef};

/// Upper bound for a side connection that sends `KILL` (design: `kill_from_side`).
const KILL_TIMEOUT: Duration = Duration::from_secs(10);

/// A connection to a pre-4.1 server: reads, browses and monitors (one TCP session; the
/// extraction moves it into the reader task, like the modern driver).
pub struct MySqlLegacyDatabase {
    pub(crate) facts: ServerFacts,
    /// Resolved parameters (incl. the password) for side connections and reconnects.
    pub(crate) params: ConnectParams,
    /// `None` while an extraction streams or after the session was killed (reconnect lazily).
    pub(crate) conn: Arc<Mutex<Option<LegacyConn>>>,
    pub(crate) write_samples: WriteSamples,
    /// An extraction holds the session: an empty `conn` slot means "busy", not "gone".
    pub(crate) streaming: Arc<AtomicBool>,
    /// Thread id of the current session (`facts.session_id` is the first one; a lazy
    /// reconnect after a kill gets a new id).
    pub(crate) current_id: Arc<AtomicU32>,
}

impl std::fmt::Debug for MySqlLegacyDatabase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MySqlLegacyDatabase")
            .field("host", &self.facts.host)
            .field("port", &self.facts.port)
            .field("database", &self.facts.database)
            .field("session_id", &self.current_id.load(Ordering::Relaxed))
            .field("version", &self.facts.version.raw)
            .finish()
    }
}

/// Charset of the login packet fixed by the `encoding` setting (`None` for `auto`: the
/// greeting decides).
fn explicit_auth_charset(encoding: EncodingSetting) -> Option<Charset> {
    match encoding {
        EncodingSetting::Auto => None,
        EncodingSetting::Utf8 => Some(Charset::Utf8),
        EncodingSetting::EucKr => Some(Charset::EucKr),
        EncodingSetting::Latin1 => Some(Charset::Latin1),
    }
}

/// Charset for the login packet: the explicit `encoding` setting, else the greeting's.
fn auth_charset(encoding: EncodingSetting, greeting: &Greeting) -> Charset {
    explicit_auth_charset(encoding).unwrap_or_else(|| greeting.auth_charset())
}

/// Checks that need no server, run before a legacy socket is opened (design doc section 3: a
/// socket must never be dropped after the greeting without a login attempt): field values
/// (`INVALID`), a TLS requirement (`CONNECT`) and, when the `encoding` setting fixes the login
/// charset, whether the user name / password / database can be encoded (`INVALID`). What
/// needs the greeting is checked in [`LegacyConn::handshake`], which then declines cleanly.
pub(crate) fn precheck(params: &ConnectParams) -> Result<MysqlConnOpts> {
    let opts = MysqlConnOpts::from_params(params)?;
    conn::check_tls(params)?;
    if let Some(cs) = explicit_auth_charset(opts.encoding) {
        conn::encode_login(params, cs)?;
    }
    Ok(opts)
}

fn refuse_41(params: &ConnectParams, greeting: &Greeting) -> CoreError {
    CoreError::Connect(format!(
        "{}:{} runs MySQL {} with the 4.1+ protocol; set the connection protocol to auto or modern",
        params.host, params.port, greeting.server_version
    ))
}

/// Open one more authenticated session with the parameters and text plan of an existing
/// connection (lazy reconnect, `KILL` side connection).
pub(crate) async fn open_session(params: &ConnectParams, facts: &ServerFacts) -> Result<LegacyConn> {
    let opts = precheck(params)?;
    let (stream, greeting) = peek_greeting(&params.host, params.port, params.connect_timeout).await?;
    if !greeting.is_pre41() {
        // The server was replaced by a 4.1+ one: answer its greeting before leaving.
        decline(stream, &greeting, params.connect_timeout).await;
        return Err(refuse_41(params, &greeting));
    }
    let cs = auth_charset(opts.encoding, &greeting);
    let mut conn = LegacyConn::handshake(stream, greeting, params, cs).await?;
    conn.set_charset(facts.plan.sql_charset());
    for stmt in facts.plan.session_statements() {
        conn.exec(stmt.as_bytes()).await?;
    }
    Ok(conn)
}

/// `KILL <target>` (4.x; `KILL QUERY` would need 5.0) from a new short-lived session.
/// `ER_NO_SUCH_THREAD` (the session already ended) counts as success. Bounded by 10 s.
pub(crate) async fn kill_session(params: &ConnectParams, facts: &ServerFacts, target: u32) -> Result<()> {
    let work = async {
        let mut side = open_session(params, facts).await?;
        let sql = kill_sql(&facts.caps, target);
        let r = side.exec(sql.as_bytes()).await;
        side.quit().await;
        match r {
            Ok(_) => Ok(()),
            Err(CoreError::Query { code: Some(code), .. }) if code == ER_NO_SUCH_THREAD.to_string() => Ok(()),
            Err(e) => Err(e),
        }
    };
    match tokio::time::timeout(KILL_TIMEOUT, work).await {
        Ok(r) => r,
        Err(_) => Err(CoreError::Timeout(format!("KILL {target} did not finish within {} s", KILL_TIMEOUT.as_secs()))),
    }
}

/// Kill `target` in the background (cancel paths must not wait for it); failures are logged.
pub(crate) fn spawn_kill(params: ConnectParams, facts: ServerFacts, target: u32) {
    tokio::spawn(async move {
        if let Err(e) = kill_session(&params, &facts, target).await {
            tracing::warn!("mysql {}:{}: KILL {target} failed: {e}", facts.host, facts.port);
        }
    });
}

/// Drop a connection that is not idle (half-read result, broken socket): closing the socket
/// aborts the statement server-side and the next use reconnects.
fn settle(slot: &mut Option<LegacyConn>) {
    if slot.as_ref().is_some_and(|c| !c.is_idle()) {
        *slot = None;
    }
}

impl MySqlLegacyDatabase {
    /// Connect with `protocol = legacy`: read the greeting, refuse a 4.1+ server
    /// (`CONNECT`: "server speaks the 4.1 protocol; use protocol auto or modern").
    pub async fn connect(params: &ConnectParams) -> Result<Self> {
        // Everything that needs no server is refused before a socket is opened.
        precheck(params)?;
        let (stream, greeting) = peek_greeting(&params.host, params.port, params.connect_timeout).await?;
        Self::connect_with_greeting(params, stream, greeting).await
    }

    /// Continue on a socket whose pre-4.1 greeting was already read (dispatcher, `auto`). A
    /// 4.1+ greeting or invalid fields still get a credential-free login ([`decline`]) before
    /// the error, so the server never records an interrupted handshake.
    pub async fn connect_with_greeting(params: &ConnectParams, stream: TcpStream, greeting: Greeting) -> Result<Self> {
        let checked = if greeting.is_pre41() { MysqlConnOpts::from_params(params) } else { Err(refuse_41(params, &greeting)) };
        let opts = match checked {
            Ok(opts) => opts,
            Err(e) => {
                decline(stream, &greeting, params.connect_timeout).await;
                return Err(e);
            }
        };
        let version = greeting.version();
        let session_id = greeting.connection_id;
        let greeting_charset = greeting.charset_name_40();
        let cs = auth_charset(opts.encoding, &greeting);
        let mut conn = LegacyConn::handshake(stream, greeting, params, cs).await?;

        // 4.0 has one server-wide charset (`character_set`, e.g. `euc_kr`, `latin1`).
        let server_charset = match conn.query_meta(b"SHOW VARIABLES LIKE 'character_set'", 16).await {
            Ok(rows) => rows.col("Value").or(Some(1)).and_then(|c| rows.raw(0, c)).map(|b| String::from_utf8_lossy(b).trim().to_string()),
            Err(e) if !conn.is_idle() => return Err(e),
            Err(e) => {
                tracing::debug!("mysql {}:{}: SHOW VARIABLES LIKE 'character_set' failed: {e}", params.host, params.port);
                None
            }
        }
        .filter(|s| !s.is_empty())
        .or_else(|| greeting_charset.map(str::to_string));

        let (plan, note) = plan_text(opts.encoding, &version, true, server_charset.as_deref(), None);
        if let Some(note) = &note {
            tracing::info!("mysql {}:{}: {note}", params.host, params.port);
        }
        conn.set_charset(plan.sql_charset());
        for stmt in plan.session_statements() {
            conn.exec(stmt.as_bytes()).await?;
        }
        tracing::debug!("mysql {}:{}: connected with the pre-4.1 client to {} (thread {session_id}, {})", params.host, params.port, version.raw, plan.describe());
        let facts = ServerFacts {
            caps: MysqlCaps::for_version(&version),
            version,
            legacy_protocol: true,
            session_id,
            server_charset,
            database_charset: None,
            plan,
            collations: CollationMap::new(),
            lex: LexMode::default(),
            user: params.user.clone(),
            database: params.database.clone().filter(|d| !d.is_empty()),
            application_name: params.application_name.clone(),
            host: params.host.clone(),
            port: params.port,
        };
        Ok(MySqlLegacyDatabase {
            facts,
            params: params.clone(),
            conn: Arc::new(Mutex::new(Some(conn))),
            write_samples: WriteSamples::default(),
            streaming: Arc::new(AtomicBool::new(false)),
            current_id: Arc::new(AtomicU32::new(session_id)),
        })
    }

    pub fn version(&self) -> &ServerVersion {
        &self.facts.version
    }

    /// Lock the session slot, reconnecting when the session was killed or dropped. `CONNECT`
    /// while an extraction holds the session.
    async fn lock_session(&self) -> Result<MutexGuard<'_, Option<LegacyConn>>> {
        let mut slot = self.conn.lock().await;
        if slot.is_none() {
            if self.streaming.load(Ordering::SeqCst) {
                return Err(CoreError::Connect("connection is busy with a running extraction".into()));
            }
            let conn = open_session(&self.params, &self.facts).await?;
            tracing::debug!("mysql {}:{}: reconnected (thread {})", self.facts.host, self.facts.port, conn.connection_id());
            self.current_id.store(conn.connection_id(), Ordering::SeqCst);
            *slot = Some(conn);
        }
        Ok(slot)
    }

    /// Take the session for an extraction (the reader task owns it until the result set is
    /// read to the end).
    pub(crate) async fn take_for_stream(&self) -> Result<LegacyConn> {
        let mut slot = self.lock_session().await?;
        let conn = slot.take().ok_or_else(|| CoreError::Internal("mysql session slot is empty".into()))?;
        self.streaming.store(true, Ordering::SeqCst);
        Ok(conn)
    }

    /// `COM_PING` on the session (reconnects first when it was killed).
    pub async fn ping(&self) -> Result<()> {
        let mut slot = self.lock_session().await?;
        let r = match slot.as_mut() {
            Some(c) => c.ping().await,
            None => Err(CoreError::Internal("mysql session slot is empty".into())),
        };
        settle(&mut slot);
        r
    }
}

#[async_trait]
impl MysqlRawQuery for MySqlLegacyDatabase {
    fn facts(&self) -> &ServerFacts {
        &self.facts
    }

    async fn meta_query(&self, sql: &str) -> Result<MetaRows> {
        let bytes = conn::encode_text(self.facts.plan.sql_charset(), sql)?;
        let mut slot = self.lock_session().await?;
        let r = match slot.as_mut() {
            Some(c) => c.query_meta(&bytes, META_ROW_LIMIT).await,
            None => Err(CoreError::Internal("mysql session slot is empty".into())),
        };
        settle(&mut slot);
        r
    }

    async fn meta_exec(&self, sql: &str) -> Result<()> {
        let bytes = conn::encode_text(self.facts.plan.sql_charset(), sql)?;
        let mut slot = self.lock_session().await?;
        let r = match slot.as_mut() {
            Some(c) => c.exec(&bytes).await.map(|_| ()),
            None => Err(CoreError::Internal("mysql session slot is empty".into())),
        };
        settle(&mut slot);
        r
    }

    async fn meta_query_cancellable(&self, sql: &str, cancel: CancellationToken) -> Result<MetaRows> {
        let bytes = conn::encode_text(self.facts.plan.sql_charset(), sql)?;
        let mut slot = self.lock_session().await?;
        let Some(conn) = slot.as_mut() else { return Err(CoreError::Internal("mysql session slot is empty".into())) };
        let id = conn.connection_id();
        let r = tokio::select! {
            biased;
            _ = cancel.cancelled() => None,
            r = conn.query_meta(&bytes, META_ROW_LIMIT) => Some(r),
        };
        match r {
            Some(r) => {
                settle(&mut slot);
                r
            }
            None => {
                // 4.x has no KILL QUERY: the statement dies with its session. Close our end
                // now and kill the thread from a side connection; the next use reconnects.
                *slot = None;
                drop(slot);
                spawn_kill(self.params.clone(), self.facts.clone(), id);
                Err(CoreError::Cancelled)
            }
        }
    }

    async fn kill_from_side(&self, target: u32) -> Result<()> {
        kill_session(&self.params, &self.facts, target).await
    }

    fn write_samples(&self) -> &WriteSamples {
        &self.write_samples
    }
}

#[async_trait]
impl Source for MySqlLegacyDatabase {
    async fn probe(&self) -> Result<ServerInfo> {
        mysql_common::probe(self).await
    }

    async fn apply_session(&self, opts: &SessionOpts) -> Result<()> {
        mysql_common::apply_session(self, opts).await
    }

    async fn stream(&self, q: &Query, opts: &SessionOpts, cancel: CancellationToken) -> Result<SourceStream> {
        stream::start(self, q, opts, cancel).await
    }

    async fn explain(&self, sql: &str, analyze: bool) -> Result<Option<PlanSummary>> {
        mysql_common::explain(self, sql, analyze).await
    }

    fn session_id(&self) -> Option<String> {
        Some(self.current_id.load(Ordering::SeqCst).to_string())
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

#[async_trait]
impl Catalog for MySqlLegacyDatabase {
    async fn databases(&self) -> Result<Vec<DatabaseInfo>> {
        crate::catalog::mysql::generic::databases(self).await
    }

    async fn schemas(&self, db: &str) -> Result<Vec<SchemaInfo>> {
        crate::catalog::mysql::generic::schemas(self, db).await
    }

    async fn tables(&self, db: &str, schema: &str) -> Result<Vec<TableInfo>> {
        crate::catalog::mysql::generic::tables(self, db, schema).await
    }

    async fn columns(&self, table: &TableRef) -> Result<Vec<ColumnInfo>> {
        crate::catalog::mysql::generic::columns(self, table).await
    }

    async fn indexes(&self, table: &TableRef) -> Result<Vec<IndexInfo>> {
        crate::catalog::mysql::generic::indexes(self, table).await
    }

    async fn ddl(&self, table: &TableRef) -> Result<String> {
        crate::catalog::mysql::generic::ddl(self, table).await
    }

    async fn exact_count(&self, table: &TableRef, cancel: CancellationToken) -> Result<u64> {
        crate::catalog::mysql::generic::exact_count(self, table, cancel).await
    }

    fn quote_ident(&self, ident: &str) -> String {
        crate::catalog::mysql::quote_ident(ident)
    }

    fn quote_table(&self, table: &TableRef) -> String {
        crate::catalog::mysql::generic::quote_table(table)
    }
}

#[async_trait]
impl Monitor for MySqlLegacyDatabase {
    async fn snapshot(&self, job: &JobHandle) -> Result<ServerSnapshot> {
        crate::monitor::mysql::generic::snapshot(self, job).await
    }

    async fn active_sessions(&self, touching: &[TableRef]) -> Result<Vec<SessionInfo>> {
        crate::monitor::mysql::generic::active_sessions(self, touching).await
    }

    async fn cancel_session(&self, session_id: &str) -> Result<bool> {
        crate::monitor::mysql::generic::cancel_session(self, session_id).await
    }
}

impl Database for MySqlLegacyDatabase {}
