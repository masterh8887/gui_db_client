//! MySQL target writer (4.3). Owner: copy-data.
//!
//! * One dedicated `mysql_async::Conn` per session, built from the source driver's `Opts`
//!   (`db_name(None)`, all SQL fully qualified) with a [`TokenHandler`] as the global LOCAL
//!   INFILE handler; the `LiveConnection` keeps the tunnel lease and kills from a side
//!   connection (`cancel_running`).
//! * The session prelude (`SET NAMES`, `sql_mode`, `time_zone`, ...) is executed by the
//!   executor through [`TargetSession::execute`]; the writer records what it set and asserts it
//!   before every chunk (a lost `time_zone` would shift every TIMESTAMP silently).
//! * Loading per chunk (≤ 500,000 rows, ≤ `chunk_bytes`): `START TRANSACTION` on InnoDB, then
//!   `LOAD DATA LOCAL INFILE 'bulkdb-<job>-<n>-<chunk>'` fed by a depth-1 stream of encoded
//!   pieces, or a run of multi-row INSERTs; the OK info / warning count is checked before
//!   `COMMIT`, any warning or skipped row rolls the chunk back and fails the table
//!   (`DATA_WARNINGS` / `ROWS_SKIPPED`). A server that refuses LOCAL INFILE falls back to
//!   INSERT before any data is sent.
//! * DDL: `SHOW WARNINGS` after every CREATE/ALTER/DROP (`DDL_WARNING`), `SHOW CREATE TABLE`
//!   after CREATE TABLE (`DDL_SUBSTITUTED` when ENGINE or ROW_FORMAT differ).

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use tokio_util::sync::CancellationToken;

use crate::connect::LiveConnection;
use crate::copy::data::mysql_encode::{self, EncodeLimits, Encoder, SessionExpect};
use crate::copy::guard;
use crate::copy::types::*;
use crate::copy::{copy_fail, redact_values, CODE_DATA_WARNINGS, CODE_DDL_SUBSTITUTED, CODE_DDL_WARNING, CODE_ROWS_SKIPPED, CODE_TARGET_LOCK_TIMEOUT, CODE_UNMAPPABLE};
use crate::error::{CoreError, Result};
use crate::service::BulkDb;
use crate::types::Connection;

/// No piece for this long ends the chunk at the last row boundary (4.3).
const IDLE_CHUNK_END: Duration = Duration::from_secs(10);
/// Connect timeout for the dedicated writer connection (matches the source driver's default).
const WRITER_CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// The variables read into `TargetFacts.vars` (5.3).
pub const FACT_VARS: &[&str] = &[
    "version",
    "version_comment",
    "local_infile",
    "max_allowed_packet",
    "sql_mode",
    "default_storage_engine",
    "storage_engine",
    "character_set_server",
    "collation_server",
    "innodb_large_prefix",
    "innodb_file_format",
    "innodb_file_per_table",
    "innodb_strict_mode",
    "innodb_page_size",
    "innodb_default_row_format",
    "explicit_defaults_for_timestamp",
    "lower_case_table_names",
    "log_bin",
    "log_bin_trust_function_creators",
    "sql_require_primary_key",
    "restrict_fk_on_non_standard_key",
    "group_replication_transaction_size_limit",
    "group_replication_group_name",
    "wsrep_max_ws_size",
    "wsrep_max_ws_rows",
    "max_binlog_cache_size",
    "read_only",
    "server_uuid",
    "hostname",
    "port",
    "datadir",
];

// ---------------------------------------------------------------------------------------------
// LOCAL INFILE token handler
// ---------------------------------------------------------------------------------------------

#[cfg(feature = "mysql")]
pub use handler::TokenHandler;

#[cfg(feature = "mysql")]
mod handler {
    use std::sync::{Arc, Mutex};

    use futures::future::BoxFuture;
    use mysql_async::{InfileData, LocalInfileError};

    /// Serves only BulkDB's armed stream, and only to the statement that names its token; any
    /// other file name gets a stream that fails at once, so a server can never make the client
    /// read a local file.
    #[derive(Default)]
    pub struct TokenHandler {
        slot: Mutex<Option<(String, InfileData)>>,
    }

    impl TokenHandler {
        pub fn arm(&self, token: String, data: InfileData) {
            *self.slot.lock().unwrap() = Some((token, data));
        }

        pub fn disarm(&self) {
            self.slot.lock().unwrap().take();
        }

        /// The armed stream for `file_name`, or a failing stream.
        pub fn take(&self, file_name: &[u8]) -> InfileData {
            let mut slot = self.slot.lock().unwrap();
            match slot.take() {
                Some((token, data)) if token.as_bytes() == file_name => data,
                other => {
                    *slot = other;
                    let name = String::from_utf8_lossy(file_name).into_owned();
                    Box::pin(futures::stream::once(async move {
                        Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied, format!("BulkDB serves no LOCAL INFILE named '{name}'")))
                    }))
                }
            }
        }
    }

    /// The `GlobalHandler` wrapper (the writer keeps its own `Arc` to arm it).
    pub struct HandlerRef(pub Arc<TokenHandler>);

    impl mysql_async::prelude::GlobalHandler for HandlerRef {
        fn handle(&self, file_name: &[u8]) -> BoxFuture<'static, Result<InfileData, LocalInfileError>> {
            let data = self.0.take(file_name);
            Box::pin(async move { Ok(data) })
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Session
// ---------------------------------------------------------------------------------------------

/// MySQL copy target session.
pub struct MysqlTarget {
    facts: TargetFacts,
    #[cfg_attr(not(feature = "mysql"), allow(dead_code))]
    live: Option<LiveConnection>,
    #[cfg(feature = "mysql")]
    conn: tokio::sync::Mutex<Option<mysql_async::Conn>>,
    #[cfg(feature = "mysql")]
    handler: Arc<TokenHandler>,
    conn_id: AtomicU32,
    write: bool,
    expect: Mutex<SessionExpect>,
    cfg: Mutex<Option<WriterConfig>>,
    chunk_no: AtomicU64,
    in_tx: AtomicBool,
    cancelled: AtomicBool,
    /// What the current / last failed load committed (`take_failed_outcome`).
    failed: Mutex<Option<LoadOutcome>>,
    /// Whether the table of the current load is transactional (`committed_rows`).
    load_transactional: AtomicBool,
    /// Whether a chunk transaction is open (set after `START TRANSACTION`, cleared after its
    /// `COMMIT`): still true after a failed / cancelled / dropped load means the server may be
    /// rolling the last chunk back (`server_rollback_pending`, 5.4).
    chunk_tx_open: AtomicBool,
}

impl MysqlTarget {
    /// A session without a connection (tests): every statement fails.
    pub fn new(facts: TargetFacts) -> MysqlTarget {
        MysqlTarget {
            facts,
            live: None,
            #[cfg(feature = "mysql")]
            conn: tokio::sync::Mutex::new(None),
            #[cfg(feature = "mysql")]
            handler: Arc::new(TokenHandler::default()),
            conn_id: AtomicU32::new(0),
            write: false,
            expect: Mutex::new(SessionExpect::default()),
            cfg: Mutex::new(None),
            chunk_no: AtomicU64::new(0),
            in_tx: AtomicBool::new(false),
            cancelled: AtomicBool::new(false),
            failed: Mutex::new(None),
            load_transactional: AtomicBool::new(true),
            chunk_tx_open: AtomicBool::new(false),
        }
    }

    #[cfg(not(feature = "mysql"))]
    pub async fn open(_db: &BulkDb, conn: &Connection, _ep: &CopyEndpoint, _app_name: &str, _write: bool) -> Result<MysqlTarget> {
        Err(CoreError::DriverMissing { id: conn.driver.clone(), detail: "mysql is not compiled into this build".into() })
    }

    /// Open a writer / reader session on the server of `conn` (no default database).
    #[cfg(feature = "mysql")]
    pub async fn open(db: &BulkDb, conn: &Connection, ep: &CopyEndpoint, app_name: &str, write: bool) -> Result<MysqlTarget> {
        use mysql_async::OptsBuilder;
        let mut c = conn.clone();
        c.database = None;
        let live = db.open_connection(&c, ep.password.clone(), app_name).await?;
        let mdb = live.db().as_any().downcast_ref::<crate::source::mysql::MySqlDatabase>().ok_or_else(|| {
            CoreError::Invalid("copy: the target server speaks the pre-4.1 protocol; MySQL 4.x cannot be a copy target (TARGET_VERSION_UNSUPPORTED)".into())
        })?;
        let handler = Arc::new(TokenHandler::default());
        let opts = OptsBuilder::from_opts(mdb.opts().clone()).db_name(None::<String>).local_infile_handler(Some(handler::HandlerRef(handler.clone())));
        // The dedicated writer connection, like the source driver, must not hang forever on a
        // server that accepts the TCP connection but never completes the handshake.
        let host = conn.host.clone().unwrap_or_default();
        let my = match tokio::time::timeout(WRITER_CONNECT_TIMEOUT, mysql_async::Conn::new(opts)).await {
            Ok(r) => r.map_err(crate::source::mysql::map_err)?,
            Err(_) => return Err(CoreError::Connect(format!("{host}: the copy writer connection timed out after {} s", WRITER_CONNECT_TIMEOUT.as_secs()))),
        };
        let id = my.id();
        let version = {
            use crate::source::mysql_common::MysqlRawQuery;
            mdb.facts().version.raw.clone()
        };
        let mut t = MysqlTarget {
            facts: super::base_facts(CopyFamily::Mysql, "mysql", "", &ep.database, conn),
            live: Some(live),
            conn: tokio::sync::Mutex::new(Some(my)),
            handler,
            conn_id: AtomicU32::new(id),
            write,
            expect: Mutex::new(SessionExpect::default()),
            cfg: Mutex::new(None),
            chunk_no: AtomicU64::new(0),
            in_tx: AtomicBool::new(false),
            cancelled: AtomicBool::new(false),
            failed: Mutex::new(None),
            load_transactional: AtomicBool::new(true),
            chunk_tx_open: AtomicBool::new(false),
        };
        t.facts = t.read_facts(conn, &ep.database, &version).await?;
        if !write && t.facts.at_least(5, 6, 5) && t.facts.flavor == "mysql" {
            t.raw_exec("SET SESSION TRANSACTION READ ONLY").await?;
        }
        Ok(t)
    }

    #[cfg(feature = "mysql")]
    async fn raw_meta(&self, sql: &str) -> Result<MetaTable> {
        let mut g = self.conn.lock().await;
        let c = g.as_mut().ok_or_else(|| CoreError::Internal("copy: mysql target is not connected".into()))?;
        meta_on(c, sql).await
    }

    #[cfg(feature = "mysql")]
    async fn raw_exec(&self, sql: &str) -> Result<()> {
        use mysql_async::prelude::Queryable;
        let mut g = self.conn.lock().await;
        let c = g.as_mut().ok_or_else(|| CoreError::Internal("copy: mysql target is not connected".into()))?;
        c.query_drop(sql).await.map_err(map_err)
    }

    #[cfg(feature = "mysql")]
    async fn read_facts(&self, conn: &Connection, database: &str, version: &str) -> Result<TargetFacts> {
        let flavor = if version.to_ascii_lowercase().contains("mariadb") { "mariadb" } else { "mysql" };
        let mut f = super::base_facts(CopyFamily::Mysql, flavor, version, database, conn);
        if let Ok(v) = self.raw_meta("SELECT CURRENT_USER() AS u, UNIX_TIMESTAMP() AS now").await {
            f.user = v.get(0, "u").map(str::to_string).or(f.user);
            if let Some(now) = v.get(0, "now") {
                f.vars.insert("now".into(), now.to_string());
            }
        }
        let list = FACT_VARS.iter().map(|n| format!("'{n}'")).collect::<Vec<_>>().join(", ");
        if let Ok(m) = self.raw_meta(&format!("SHOW GLOBAL VARIABLES WHERE Variable_name IN ({list})")).await {
            for r in 0..m.len() {
                if let (Some(k), Some(val)) = (m.get(r, "Variable_name"), m.get(r, "Value")) {
                    f.vars.insert(k.to_ascii_lowercase(), val.to_string());
                }
            }
        }
        if !f.vars.contains_key("default_storage_engine") {
            if let Some(e) = f.vars.get("storage_engine").cloned() {
                f.vars.insert("default_storage_engine".into(), e);
            }
        }
        if let Ok(m) = self.raw_meta("SHOW GLOBAL STATUS LIKE 'Uptime'").await {
            if let Some(u) = m.get(0, "Value") {
                f.vars.insert("uptime".into(), u.to_string());
            }
        }
        Ok(super::with_identity(f))
    }

    fn writer_cfg(&self) -> WriterConfig {
        self.cfg.lock().unwrap().clone().unwrap_or(WriterConfig {
            job_id: "copy".into(),
            session_no: 0,
            batch_rows: 50_000,
            piece_bytes: 8 << 20,
            chunk_bytes: mysql_encode::chunk_limits(&self.facts).0,
            chunk_rows: mysql_encode::chunk_limits(&self.facts).1,
            lock_timeout_s: 5,
            fail_on_unmappable: false,
            charset: None,
            table_charsets: Default::default(),
            enum_error_columns: Default::default(),
            enum_error_null: false,
            source_checksum: false,
        })
    }
}

/// Map a driver error; lock wait timeouts become `TARGET_LOCK_TIMEOUT`.
#[cfg(feature = "mysql")]
fn map_err(e: mysql_async::Error) -> CoreError {
    let lock = matches!(&e, mysql_async::Error::Server(s) if s.code == 1205);
    let mapped = crate::source::mysql::map_err(e);
    if lock {
        copy_fail(CODE_TARGET_LOCK_TIMEOUT, format!("the target table is in use; try again later ({})", redact_values(&mapped.to_string())))
    } else {
        mapped
    }
}

/// Server codes meaning "LOCAL INFILE is not allowed here".
#[cfg(feature = "mysql")]
fn infile_refused(e: &mysql_async::Error) -> bool {
    match e {
        mysql_async::Error::Server(s) => matches!(s.code, 1148 | 3948 | 2068 | 1229),
        mysql_async::Error::Other(_) => true,
        _ => false,
    }
}

#[cfg(feature = "mysql")]
fn cell_text(v: &mysql_async::Value) -> Option<String> {
    use mysql_async::Value;
    match v {
        Value::NULL => None,
        Value::Bytes(b) => Some(String::from_utf8_lossy(b).into_owned()),
        Value::Int(i) => Some(i.to_string()),
        Value::UInt(u) => Some(u.to_string()),
        Value::Float(f) => Some(f.to_string()),
        Value::Double(d) => Some(d.to_string()),
        other => Some(other.as_sql(true).trim_matches('\'').to_string()),
    }
}

#[cfg(feature = "mysql")]
async fn meta_on(c: &mut mysql_async::Conn, sql: &str) -> Result<MetaTable> {
    use mysql_async::prelude::Queryable;
    let mut result = c.query_iter(sql).await.map_err(map_err)?;
    let columns: Vec<String> = result.columns_ref().iter().map(|c| c.name_str().into_owned()).collect();
    let rows: Vec<mysql_async::Row> = result.collect().await.map_err(map_err)?;
    drop(result);
    let rows = rows.iter().map(|r| (0..r.len()).map(|i| r.as_ref(i).and_then(cell_text)).collect()).collect();
    Ok(MetaTable { columns, rows })
}

/// `SHOW WARNINGS LIMIT 10` as redacted samples plus the raw (level, code) list.
#[cfg(feature = "mysql")]
async fn warnings_on(c: &mut mysql_async::Conn) -> (Vec<String>, Vec<(String, u32)>) {
    match meta_on(c, "SHOW WARNINGS LIMIT 10").await {
        Ok(m) => {
            let mut samples = Vec::new();
            let mut codes = Vec::new();
            for r in 0..m.len() {
                let level = m.get(r, "Level").unwrap_or("").to_string();
                let code: u32 = m.get(r, "Code").and_then(|v| v.parse().ok()).unwrap_or(0);
                samples.push(redact_values(&format!("{level} {code}: {}", m.get(r, "Message").unwrap_or(""))));
                codes.push((level, code));
            }
            (samples, codes)
        }
        Err(e) => (vec![format!("SHOW WARNINGS failed: {}", redact_values(&e.to_string()))], Vec::new()),
    }
}

/// The table named by `CREATE TABLE [IF NOT EXISTS] `db`.`t``.
pub fn created_table(sql: &str) -> Option<String> {
    let re = regex::Regex::new(r"(?is)^\s*CREATE\s+(?:TEMPORARY\s+)?TABLE\s+(?:IF\s+NOT\s+EXISTS\s+)?((?:`(?:[^`]|``)+`\s*\.\s*)?`(?:[^`]|``)+`)").ok()?;
    re.captures(sql).map(|c| c[1].to_string())
}

/// `ENGINE=` / `ROW_FORMAT=` of a table definition (upper case).
pub fn table_option(ddl: &str, name: &str) -> Option<String> {
    let re = regex::Regex::new(&format!(r"(?i)\b{name}\s*=\s*([A-Za-z0-9_]+)")).ok()?;
    re.captures_iter(ddl).last().map(|c| c[1].to_ascii_uppercase())
}

#[async_trait]
impl TargetSession for MysqlTarget {
    fn facts(&self) -> &TargetFacts {
        &self.facts
    }

    #[cfg(feature = "mysql")]
    async fn query_meta(&self, sql: &str) -> Result<MetaTable> {
        guard::check_read(CopyFamily::Mysql, sql)?;
        self.raw_meta(sql).await
    }

    #[cfg(not(feature = "mysql"))]
    async fn query_meta(&self, _sql: &str) -> Result<MetaTable> {
        Err(CoreError::DriverMissing { id: "mysql".into(), detail: "mysql is not compiled into this build".into() })
    }

    #[cfg(feature = "mysql")]
    async fn execute(&self, sql: &str) -> Result<u64> {
        use mysql_async::prelude::Queryable;
        guard::check_always_rejected(sql)?;
        if !self.write {
            return Err(CoreError::Internal("copy: write on a read-only mysql target session".into()));
        }
        let words = guard::leading_words(sql, 2);
        let first = words.first().map(String::as_str).unwrap_or("");
        let mut g = self.conn.lock().await;
        let c = g.as_mut().ok_or_else(|| CoreError::Internal("copy: mysql target is not connected".into()))?;
        c.query_drop(sql).await.map_err(map_err)?;
        let affected = c.affected_rows();
        match first {
            "SET" => self.expect.lock().unwrap().observe(sql),
            "BEGIN" | "START" => self.in_tx.store(true, Ordering::Relaxed),
            "COMMIT" | "ROLLBACK" => self.in_tx.store(false, Ordering::Relaxed),
            _ => {}
        }
        if matches!(first, "CREATE" | "ALTER" | "DROP" | "TRUNCATE") && c.get_warnings() > 0 {
            let (samples, codes) = warnings_on(c).await;
            let failing: Vec<String> = samples
                .iter()
                .zip(codes.iter())
                .filter(|(_, (level, code))| !(mysql_encode::ddl_warning_is_note(*code) || level.eq_ignore_ascii_case("Note")))
                .map(|(s, _)| s.clone())
                .collect();
            if failing.is_empty() {
                tracing::info!(notes = ?samples, "copy: DDL notes");
            } else {
                return Err(copy_fail(CODE_DDL_WARNING, format!("the server changed or warned about the DDL: {}", failing.join("; "))));
            }
        }
        if first == "CREATE" && words.get(1).map(String::as_str) == Some("TABLE") {
            if let Some(name) = created_table(sql) {
                let m = meta_on(c, &format!("SHOW CREATE TABLE {name}")).await?;
                let ddl = m.rows.first().and_then(|r| r.get(1)).cloned().flatten().unwrap_or_default();
                for opt in ["ENGINE", "ROW_FORMAT"] {
                    if let Some(want) = table_option(sql, opt) {
                        let got = table_option(&ddl, opt);
                        // A row format the server reports only when it differs from the default
                        // is absent from SHOW CREATE TABLE when it equals the default.
                        let differs = match (&got, opt) {
                            (Some(g), _) => *g != want,
                            (None, "ROW_FORMAT") => false,
                            (None, _) => true,
                        };
                        if differs {
                            return Err(copy_fail(
                                CODE_DDL_SUBSTITUTED,
                                format!("{name}: {opt} is {} on the target, the plan asked for {want}", got.unwrap_or_else(|| "unknown".into())),
                            ));
                        }
                    }
                }
            }
        }
        Ok(affected)
    }

    #[cfg(not(feature = "mysql"))]
    async fn execute(&self, _sql: &str) -> Result<u64> {
        Err(CoreError::DriverMissing { id: "mysql".into(), detail: "mysql is not compiled into this build".into() })
    }

    #[cfg(feature = "mysql")]
    async fn load(&self, spec: &TableDataSpec, input: LoadInput, cancel: CancellationToken) -> Result<LoadOutcome> {
        use mysql_async::prelude::Queryable;
        if !self.write {
            return Err(CoreError::Internal("copy: load on a read-only mysql target session".into()));
        }
        let LoadInput::Arrow(mut rx) = input else {
            return Err(CoreError::Invalid("mysql targets load Arrow batches".into()));
        };
        *self.failed.lock().unwrap() = None;
        let cfg = self.writer_cfg();
        let facts = &self.facts;
        let limits = EncodeLimits::new(facts, cfg.piece_bytes, cfg.batch_rows as usize);
        let (chunk_bytes, chunk_rows) = (cfg.chunk_bytes.max(1 << 20), cfg.chunk_rows.max(1));
        let mut method = mysql_encode::method_for(facts);
        let mut g = self.conn.lock().await;
        let c = g.as_mut().ok_or_else(|| CoreError::Internal("copy: mysql target is not connected".into()))?;
        let schema = spec.target.schema.clone().unwrap_or_else(|| facts.database.clone());
        let lex = crate::source::mysql::LexMode { backslash_escapes: true, ansi_quotes: false };
        let eng = meta_on(
            c,
            &format!(
                "SELECT ENGINE FROM information_schema.TABLES WHERE TABLE_SCHEMA = {} AND TABLE_NAME = {}",
                crate::source::mysql_common::quote_str(&schema, lex),
                crate::source::mysql_common::quote_str(&spec.target.name, lex)
            ),
        )
        .await?;
        let transactional =
            eng.get(0, "ENGINE").is_none_or(|e| e.eq_ignore_ascii_case("InnoDB") || e.eq_ignore_ascii_case("ndbcluster") || e.eq_ignore_ascii_case("TokuDB"));
        if !mysql_encode::load_data_has_charset_clause(facts) {
            c.query_drop(format!("SET SESSION character_set_database = {}", mysql_encode::connection_charset(facts))).await.map_err(map_err)?;
        }
        // Probe LOCAL INFILE with an empty stream before any data is consumed.
        if method == LoadMethod::MysqlLoadData {
            let token = format!("bulkdb-{}-{}-probe", cfg.job_id, cfg.session_no);
            self.handler.arm(token.clone(), Box::pin(futures::stream::empty()));
            let probe = c.query_drop(mysql_encode::load_data_sql(spec, facts, &token)).await;
            self.handler.disarm();
            if let Err(e) = probe {
                if infile_refused(&e) {
                    tracing::info!(table = %spec.target.name, "copy: the server refuses LOAD DATA LOCAL INFILE; using INSERT batches");
                    method = LoadMethod::MysqlInsert;
                } else {
                    return Err(map_err(e));
                }
            }
        }
        self.load_transactional.store(transactional, Ordering::Relaxed);
        self.chunk_tx_open.store(false, Ordering::Relaxed);
        let mut enc = Encoder::new(spec, facts, method, limits)
            .target_charset(cfg.charset_for(&spec.item_id))
            .enum_errors(cfg.enum_error_columns_for(&spec.item_id), cfg.enum_error_null);
        let mut out = LoadOutcome { method: Some(method), ..LoadOutcome::default() };
        // Nothing committed yet; kept current after every COMMIT, so even a load whose future is
        // dropped (cancel after the grace period) reports what stays in the target.
        *self.failed.lock().unwrap() = Some(out.clone());
        let mut pending: VecDeque<(Bytes, u64)> = VecDeque::new();
        let mut input_done = false;
        let expect = self.expect.lock().unwrap().clone();
        let fail_unmappable = cfg.fail_on_unmappable;
        // The source-side checksum (5.6), hashed as the writer encodes each batch it already has
        // to encode, so the reader needs no second encoder.
        let mut source_sum = cfg.source_checksum.then(crate::copy::hash::TableChecksum::default);

        // Pull one batch from the input into `pending` (false at the end of the input).
        macro_rules! pull {
            ($wait:expr) => {{
                let next = match $wait {
                    Some(d) => {
                        tokio::select! {
                            biased;
                            _ = cancel.cancelled() => return Err(CoreError::Cancelled),
                            r = tokio::time::timeout(d, rx.recv()) => match r { Ok(n) => Some(n), Err(_) => None },
                        }
                    }
                    None => {
                        tokio::select! {
                            biased;
                            _ = cancel.cancelled() => return Err(CoreError::Cancelled),
                            n = rx.recv() => Some(n),
                        }
                    }
                };
                match next {
                    None => Pulled::Idle,
                    Some(None) => Pulled::End,
                    Some(Some(b)) => {
                        enc.encode(&b.item, source_sum.as_mut())?;
                        drop(b);
                        let e = enc.finish_batch();
                        out.unmappable += e.unmappable;
                        if fail_unmappable && out.unmappable > 0 {
                            return Err(copy_fail(
                                CODE_UNMAPPABLE,
                                format!("{} characters of {} cannot be stored in the target character set", out.unmappable, spec.target.name),
                            ));
                        }
                        mysql_encode::merge_fixes(&mut out.value_fixes, &e.value_fixes);
                        for (p, r) in e.pieces.into_iter().zip(e.piece_rows) {
                            pending.push_back((p, r));
                        }
                        Pulled::Batch
                    }
                }
            }};
        }

        let result: Result<()> = async {
            loop {
                if pending.is_empty() {
                    if input_done {
                        break;
                    }
                    if let Pulled::End = pull!(None::<Duration>) {
                        input_done = true;
                        continue;
                    }
                    if pending.is_empty() {
                        continue;
                    }
                }
                if cancel.is_cancelled() {
                    return Err(CoreError::Cancelled);
                }
                // Session assertion before every chunk.
                let row = meta_on(c, SessionExpect::QUERY).await?;
                let col = |i: usize| row.rows.first().and_then(|r| r.get(i)).cloned().flatten();
                expect.check(col(0).as_deref(), col(1).as_deref(), col(2).as_deref())?;
                if transactional {
                    c.query_drop("START TRANSACTION").await.map_err(map_err)?;
                    self.chunk_tx_open.store(true, Ordering::Relaxed);
                }
                let mut chunk_rows_sent = 0u64;
                let mut chunk_bytes_sent = 0u64;
                let chunk_result: Result<()> = async {
                    match method {
                        LoadMethod::MysqlLoadData => {
                            let n = self.chunk_no.fetch_add(1, Ordering::Relaxed);
                            let token = format!("bulkdb-{}-{}-{n}", cfg.job_id, cfg.session_no);
                            let sql = mysql_encode::load_data_sql(spec, facts, &token);
                            guard::check_statement(CopyFamily::Mysql, &sql, guard::StmtContext::Load)?;
                            let (ptx, prx) = tokio::sync::mpsc::channel::<Bytes>(1);
                            let stream = futures::stream::unfold(prx, |mut rx| async move { rx.recv().await.map(|b| (Ok::<Bytes, std::io::Error>(b), rx)) });
                            self.handler.arm(token, Box::pin(stream));
                            let feeder = async {
                                let ptx = ptx;
                                loop {
                                    if pending.is_empty() {
                                        if input_done {
                                            return Ok::<(), CoreError>(());
                                        }
                                        let wait = if chunk_rows_sent > 0 { Some(IDLE_CHUNK_END) } else { None };
                                        match pull!(wait) {
                                            Pulled::End => input_done = true,
                                            Pulled::Idle => return Ok(()),
                                            Pulled::Batch => {}
                                        }
                                        continue;
                                    }
                                    let (_, rows) = pending.front().cloned().unwrap_or_default();
                                    let len = pending.front().map(|p| p.0.len() as u64).unwrap_or(0);
                                    if chunk_rows_sent > 0 && (chunk_rows_sent + rows > chunk_rows || chunk_bytes_sent + len > chunk_bytes) {
                                        return Ok(());
                                    }
                                    let (piece, rows) = pending.pop_front().expect("piece");
                                    tokio::select! {
                                        biased;
                                        _ = cancel.cancelled() => return Err(CoreError::Cancelled),
                                        r = ptx.send(piece) => if r.is_err() { return Ok(()) },
                                    }
                                    chunk_rows_sent += rows;
                                    chunk_bytes_sent += len;
                                }
                            };
                            // Both run until done; a statement that ends first (an error before
                            // the server asked for the file) drops the armed stream, which
                            // unblocks a feeder waiting on the full channel.
                            let (load_res, fed_res) = {
                                let load = c.query_drop(sql);
                                tokio::pin!(load);
                                tokio::pin!(feeder);
                                let (mut load_res, mut fed_res) = (None, None);
                                while load_res.is_none() || fed_res.is_none() {
                                    tokio::select! {
                                        r = &mut load, if load_res.is_none() => {
                                            load_res = Some(r);
                                            self.handler.disarm();
                                        }
                                        r = &mut feeder, if fed_res.is_none() => fed_res = Some(r),
                                    }
                                }
                                (load_res, fed_res)
                            };
                            self.handler.disarm();
                            let load = load_res.expect("load result");
                            match (load, fed_res.expect("feeder result")) {
                                (Err(e), _) => return Err(map_err(e)),
                                (Ok(()), fed) => fed?,
                            }
                            let (records, skipped, warnings) = mysql_encode::parse_load_info(&c.info());
                            let _ = records;
                            if skipped + warnings > 0 {
                                out.warnings += warnings;
                                out.skipped += skipped;
                                let (samples, _) = warnings_on(c).await;
                                out.samples.extend(samples.iter().cloned());
                                let code = if skipped > 0 { CODE_ROWS_SKIPPED } else { CODE_DATA_WARNINGS };
                                return Err(copy_fail(
                                    code,
                                    format!("{} warnings, {} skipped rows while loading {}: {}", warnings, skipped, spec.target.name, samples.join("; ")),
                                ));
                            }
                            Ok(())
                        }
                        _ => loop {
                            if pending.is_empty() {
                                if input_done {
                                    return Ok(());
                                }
                                let wait = if chunk_rows_sent > 0 { Some(IDLE_CHUNK_END) } else { None };
                                match pull!(wait) {
                                    Pulled::End => input_done = true,
                                    Pulled::Idle => return Ok(()),
                                    Pulled::Batch => {}
                                }
                                continue;
                            }
                            let rows = pending.front().map(|p| p.1).unwrap_or(0);
                            let len = pending.front().map(|p| p.0.len() as u64).unwrap_or(0);
                            if chunk_rows_sent > 0 && (chunk_rows_sent + rows > chunk_rows || chunk_bytes_sent + len > chunk_bytes) {
                                return Ok(());
                            }
                            let (stmt, rows) = pending.pop_front().expect("statement");
                            let text = std::str::from_utf8(&stmt).map_err(|e| CoreError::Internal(format!("copy: INSERT is not UTF-8: {e}")))?;
                            tokio::select! {
                                biased;
                                _ = cancel.cancelled() => return Err(CoreError::Cancelled),
                                r = c.query_drop(text) => r.map_err(map_err)?,
                            }
                            if c.get_warnings() > 0 {
                                let w = c.get_warnings() as u64;
                                out.warnings += w;
                                let (samples, _) = warnings_on(c).await;
                                out.samples.extend(samples.iter().cloned());
                                return Err(copy_fail(CODE_DATA_WARNINGS, format!("{w} warnings while inserting into {}: {}", spec.target.name, samples.join("; "))));
                            }
                            chunk_rows_sent += rows;
                            chunk_bytes_sent += len;
                        },
                    }
                }
                .await;
                match chunk_result {
                    Ok(()) => {
                        if cancel.is_cancelled() {
                            if transactional {
                                let _ = c.query_drop("ROLLBACK").await;
                            }
                            return Err(CoreError::Cancelled);
                        }
                        if transactional {
                            c.query_drop("COMMIT").await.map_err(map_err)?;
                            self.chunk_tx_open.store(false, Ordering::Relaxed);
                        }
                        out.rows += chunk_rows_sent;
                        if let Some(f) = self.failed.lock().unwrap().as_mut() {
                            f.rows = out.rows;
                        }
                    }
                    Err(e) => {
                        if transactional {
                            let _ = c.query_drop("ROLLBACK").await;
                        }
                        return Err(e);
                    }
                }
            }
            Ok(())
        }
        .await;
        out.samples.truncate(10);
        match result {
            Ok(()) => {
                out.source_checksum = source_sum;
                *self.failed.lock().unwrap() = None;
                Ok(out)
            }
            Err(e) => {
                // `out.rows` counts whole committed chunks only (a rolled-back or interrupted
                // chunk never adds to it); a MyISAM table may also keep part of the last one.
                *self.failed.lock().unwrap() = Some(out);
                Err(e)
            }
        }
    }

    #[cfg(not(feature = "mysql"))]
    async fn load(&self, _spec: &TableDataSpec, _input: LoadInput, _cancel: CancellationToken) -> Result<LoadOutcome> {
        Err(CoreError::DriverMissing { id: "mysql".into(), detail: "mysql is not compiled into this build".into() })
    }

    async fn count_rows(&self, t: &TargetTable) -> Result<u64> {
        let m = self.query_meta(&format!("SELECT COUNT(*) AS n FROM {}", mysql_encode::qualified(t))).await?;
        m.get(0, "n").and_then(|v| v.parse().ok()).ok_or_else(|| CoreError::Internal("copy: count returned no row".into()))
    }

    async fn cancel_running(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
        #[cfg(feature = "mysql")]
        if let Some(live) = &self.live {
            if let Some(raw) = crate::source::mysql_raw(live.db()) {
                let id = self.conn_id.load(Ordering::Relaxed);
                if id != 0 {
                    if let Err(e) = raw.kill_from_side(id).await {
                        tracing::warn!("copy: KILL QUERY {id} failed: {e}");
                    }
                }
            }
        }
    }

    fn configure(&self, cfg: &WriterConfig) {
        *self.cfg.lock().unwrap() = Some(cfg.clone());
    }

    fn take_failed_outcome(&self) -> Option<LoadOutcome> {
        self.failed.lock().unwrap().take()
    }

    /// Whole committed chunks of a transactional table; `None` for MyISAM & co., where every
    /// row sent is final.
    fn committed_rows(&self) -> Option<u64> {
        if !self.load_transactional.load(Ordering::Relaxed) {
            return None;
        }
        Some(self.failed.lock().unwrap().as_ref().map(|f| f.rows).unwrap_or(0))
    }

    /// A chunk transaction left open by a load that did not finish cleanly (cancel, error, or a
    /// dropped future): the server may still be rolling it back.
    fn server_rollback_pending(&self) -> bool {
        self.chunk_tx_open.load(Ordering::Relaxed)
    }
}

/// What one pull from the input produced.
#[allow(dead_code)]
enum Pulled {
    Batch,
    End,
    Idle,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "mysql")]
    #[tokio::test]
    async fn token_handler_serves_only_its_token() {
        use futures::StreamExt;
        let h = TokenHandler::default();
        h.arm("bulkdb-j-1-0".into(), Box::pin(futures::stream::iter(vec![Ok(Bytes::from_static(b"1\n"))])));
        let mut other = h.take(b"/etc/passwd");
        assert!(other.next().await.unwrap().is_err());
        let mut mine = h.take(b"bulkdb-j-1-0");
        assert_eq!(mine.next().await.unwrap().unwrap(), Bytes::from_static(b"1\n"));
        // Taken once: a second request for the same token fails too.
        let mut again = h.take(b"bulkdb-j-1-0");
        assert!(again.next().await.unwrap().is_err());
    }

    #[test]
    fn ddl_helpers() {
        assert_eq!(created_table("CREATE TABLE `db`.`t` (id int) ENGINE=InnoDB").as_deref(), Some("`db`.`t`"));
        assert_eq!(created_table("create table if not exists `a``b` (x int)").as_deref(), Some("`a``b`"));
        assert_eq!(table_option("CREATE TABLE t (x int) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 ROW_FORMAT=DYNAMIC", "ENGINE").as_deref(), Some("INNODB"));
        assert_eq!(table_option("... ROW_FORMAT=dynamic", "ROW_FORMAT").as_deref(), Some("DYNAMIC"));
        assert_eq!(table_option("CREATE TABLE t (x int)", "ENGINE"), None);
    }
}
