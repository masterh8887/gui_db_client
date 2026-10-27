//! `BulkDb`: the facade every shell talks to. Tauri commands and CLI subcommands are ten-line
//! adapters over these methods. Owns config, credentials, drivers, tunnels, jobs, history and
//! the log buffer.
//!
//! Connection handling lives in `connect` (tunnel + credentials + driver open), the export
//! state machine in `export`, catalog caching in `catalog_cache`, tunnel prompts in `prompts`
//! and the DBeaver importer in `dbeaver`; this file wires them together.

use std::collections::HashMap;
use std::future::Future;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use crate::catalog_cache::{BrowserPool, CacheKey, CatalogCache, CatalogKind, CatalogValue};
use crate::config::ConfigStore;
use crate::connect::{self, ConnectRequest, LiveConnection};
use crate::credentials::{self, CredentialStore};
use crate::drivers::DriverRegistry;
use crate::error::{CoreError, Result};
use crate::export;
use crate::history::History;
use crate::job::JobRegistry;
use crate::logging::LogBuffer;
use crate::prompts::EventPromptHandler;
use crate::sqlguard::{self, Dialect, SqlCheck};
use crate::tunnel::{PromptHandler, TunnelManager};
use crate::types::*;

pub struct BulkDbOptions {
    /// Defaults to `platform::config_dir()`.
    pub config_dir: Option<PathBuf>,
    /// How to ask the user about host keys / ssh secrets. Defaults to the event-based
    /// handler (`tunnel:hostkey` / `tunnel:password` events + `tunnel_confirm_*` calls).
    pub prompts: Option<Arc<dyn PromptHandler>>,
    /// Defaults to `credentials::default_store()`.
    pub credentials: Option<Box<dyn CredentialStore>>,
    /// Install the tracing subscriber (file + ring buffer). The CLI may prefer stderr.
    pub init_logging: bool,
}

impl Default for BulkDbOptions {
    fn default() -> Self {
        BulkDbOptions { config_dir: None, prompts: None, credentials: None, init_logging: true }
    }
}

/// `application_name` of the long-lived catalog / EXPLAIN connection per saved connection.
pub const APP_NAME_UI: &str = "bulkdb-ui";
/// `application_name` of the throw-away connection preflight uses.
pub const APP_NAME_PREFLIGHT: &str = "bulkdb-preflight";
/// `application_name` of the connection that samples the server while a job runs.
pub const APP_NAME_MONITOR: &str = "bulkdb-monitor";

/// How long [`BulkDb::shutdown`] waits for cancelled jobs to reach a terminal state.
pub const SHUTDOWN_JOB_WAIT: Duration = Duration::from_secs(5);
/// Poll interval of that wait.
const SHUTDOWN_POLL: Duration = Duration::from_millis(25);
/// Export runs kept in the history database per connection. Every run stores its full SQL
/// text, so the table is pruned to this size at startup and after every finished export.
pub const HISTORY_KEEP_PER_CONNECTION: usize = 500;

pub struct BulkDb {
    pub config: ConfigStore,
    pub creds: Box<dyn CredentialStore>,
    pub drivers: DriverRegistry,
    pub tunnels: TunnelManager,
    pub jobs: JobRegistry,
    pub history: History,
    pub logs: Arc<LogBuffer>,
    /// Event based tunnel prompts, answered through `tunnel_confirm_hostkey` /
    /// `tunnel_provide_secret`. Only wired into the tunnel manager when `opts.prompts` was
    /// `None`; otherwise the answers find no pending prompt (`NOT_FOUND`).
    pub prompts: Arc<EventPromptHandler>,
    /// Catalog results per connection; `refresh = true` bypasses it.
    pub catalog: CatalogCache,
    /// Long-lived `bulkdb-ui` connections for the catalog browser and EXPLAIN.
    pub browsers: BrowserPool,
    events: broadcast::Sender<CoreEvent>,
    /// Passwords typed this session for connections saved with `save_password = false`
    /// (memory only, dropped with the process).
    session_secrets: Mutex<HashMap<String, String>>,
    /// How many export slots the semaphore was sized for (`settings_set` resizes it).
    slots_configured: AtomicUsize,
    me: Weak<BulkDb>,
    _log_guard: Option<tracing_appender::non_blocking::WorkerGuard>,
}

impl BulkDb {
    pub async fn new(opts: BulkDbOptions) -> Result<Arc<BulkDb>> {
        let dir = opts.config_dir.unwrap_or_else(crate::platform::config_dir);
        let config = ConfigStore::open(dir.clone())?;
        let logs = Arc::new(LogBuffer::new());
        let settings = config.settings();
        let log_guard = if opts.init_logging { crate::logging::init(&crate::platform::log_dir(), &settings.log_level, logs.clone())? } else { None };
        // The event bus exists before anything that emits on it: the prompt handler feeds the
        // tunnel manager, the job registry feeds the state machine.
        let (events, _) = broadcast::channel(256);
        let prompt_events = Arc::new(EventPromptHandler::new(events.clone()));
        let prompts: Arc<dyn PromptHandler> = match opts.prompts {
            Some(custom) => custom,
            None => prompt_events.clone(),
        };
        let drivers = DriverRegistry::load(Some(&config.drivers_override_path()))?;
        let history = History::open(&config.history_path())?;
        // Best effort: a history that cannot be pruned is still usable.
        if let Err(e) = history.prune(HISTORY_KEEP_PER_CONNECTION) {
            tracing::warn!("history: cannot prune old runs: {e}");
        }
        let slots = settings.max_concurrent_jobs.clamp(1, 3) as usize;
        let tunnels = TunnelManager::new(prompts, std::time::Duration::from_secs(settings.tunnel_idle_secs as u64));
        let jobs = JobRegistry::new(events.clone(), slots);
        let creds = opts.credentials.unwrap_or_else(crate::credentials::default_store);
        let db = Arc::new_cyclic(|me| BulkDb {
            config,
            creds,
            drivers,
            tunnels,
            jobs,
            history,
            logs,
            prompts: prompt_events,
            catalog: CatalogCache::new(),
            browsers: BrowserPool::new(),
            events,
            session_secrets: Mutex::new(HashMap::new()),
            slots_configured: AtomicUsize::new(slots),
            me: me.clone(),
            _log_guard: log_guard,
        });
        db.move_connection_string_secrets();
        tracing::info!(config_dir = %dir.display(), version = env!("CARGO_PKG_VERSION"), "bulk-core ready");
        Ok(db)
    }

    /// `connections.toml` written by hand (or before `conn_save` split passwords out of ODBC
    /// connection strings) may still hold `PWD=` inside a `connection_string` field. Move each
    /// such password where `conn_save` would have put it (the credential store, or this
    /// session's memory for "ask every time" connections) and rewrite the entry without it, so
    /// it never reaches `conn_list` / `conn_get` over IPC. Entries whose password cannot be
    /// stored are left untouched (with a warning) rather than losing the password.
    ///
    /// Other secret attributes (`token=`, `KeyStoreSecret=`, `PWD2=` ...) have no place in the
    /// credential store, and `conn_save` refuses them; they are removed from the file all the
    /// same, with a warning naming the keywords (never the values) and pointing to an ODBC data
    /// source (DSN) as their home, since keeping them would put them on every `conn_list`.
    fn move_connection_string_secrets(&self) {
        for mut conn in self.config.connections() {
            let mut fields = conn.fields.clone();
            let dropped = fields.get(connect::CONNECTION_STRING_FIELD).map(|cs| connect::connection_string_unstorable_secrets(cs)).unwrap_or_default();
            let secret = take_connection_string_secret(&mut fields);
            if fields == conn.fields {
                continue;
            }
            if let Some(secret) = secret {
                if conn.save_password {
                    if let Err(e) = self.creds.set(&credentials::db_password_key(&conn.id), &secret) {
                        tracing::warn!(conn_id = %conn.id, "connection string holds a password that cannot be moved to the credential store: {e}");
                        continue;
                    }
                } else {
                    self.session_secrets.lock().unwrap().insert(conn.id.clone(), secret);
                    tracing::warn!(conn_id = %conn.id, name = %conn.name, "removed the password from the connection string of an \"ask every time\" connection; it will be asked for after a restart");
                }
            }
            conn.fields = fields;
            let (id, name) = (conn.id.clone(), conn.name.clone());
            match self.config.upsert(conn) {
                Ok(_) if dropped.is_empty() => tracing::info!(conn_id = %id, "password moved out of the saved connection string"),
                Ok(_) => tracing::warn!(
                    conn_id = %id,
                    name = %name,
                    attributes = %dropped.join(", "),
                    "removed secret attributes from the saved connection string: secrets are never kept in connections.toml; set them in an ODBC data source (DSN) and use DSN="
                ),
                Err(e) => tracing::warn!(conn_id = %id, "cannot rewrite the connection without its secrets: {e}"),
            }
        }
    }

    /// Subscribe to state / done / prompt / driver events. Progress is polled, not pushed.
    pub fn subscribe(&self) -> broadcast::Receiver<CoreEvent> {
        self.events.subscribe()
    }

    pub fn emit(&self, ev: CoreEvent) {
        let _ = self.events.send(ev);
    }

    /// Strong handle to self for background tasks.
    fn me(&self) -> Result<Arc<BulkDb>> {
        self.me.upgrade().ok_or_else(|| CoreError::Internal("service is shutting down".into()))
    }

    // ------------------------------------------------------------------ drivers

    pub fn drivers_list(&self) -> Vec<DriverInfo> {
        self.drivers.list()
    }

    pub fn drivers_rescan(&self) -> Vec<DriverInfo> {
        let list = self.drivers.rescan();
        self.emit(CoreEvent::DriversChanged(list.clone()));
        list
    }

    // ------------------------------------------------------------------ connections

    pub fn conn_list(&self) -> Vec<Connection> {
        self.config.connections().into_iter().map(|c| self.with_secret_flags(c)).collect()
    }

    pub fn conn_get(&self, id: &str) -> Result<Connection> {
        self.config.get(id).map(|c| self.with_secret_flags(c)).ok_or_else(|| CoreError::NotFound(format!("connection {id}")))
    }

    /// Fill `has_password` / `has_ssh_secret` from the credential store.
    fn with_secret_flags(&self, mut c: Connection) -> Connection {
        c.has_password = self.creds.get(&credentials::db_password_key(&c.id)).ok().flatten().is_some();
        c.has_ssh_secret = connect::ssh_secret_key(&c).and_then(|k| self.creds.get(&k).ok().flatten()).is_some();
        c
    }

    /// Save the connection (no secrets in the file) and route the secrets to the credential
    /// store: stored when `save_password` is on, removed when it is off.
    ///
    /// A `PWD=` / `PASSWORD=` attribute inside an ODBC `connection_string` field is a secret
    /// too: it is cut out of the string and treated exactly like the password field (it wins
    /// over a typed password, as it did at connect time). The ODBC driver appends `PWD=` from
    /// the resolved password when it connects. Any other secret attribute (`PWD2=`,
    /// `KeyStoreSecret=`, `token=` ..., see `connect::is_connection_string_secret_key`) has no
    /// place in the credential store, so a connection carrying one is refused with `INVALID`
    /// (naming the keyword, never the value) and nothing is saved.
    ///
    /// Absent secrets keep what is stored. `clear_password` deletes exactly the stored database
    /// password (and a password typed for this connection earlier in the session);
    /// `clear_ssh_secret` deletes the stored SSH password and key passphrase. Asking to clear a
    /// secret while giving a new one for it is `INVALID`.
    pub fn conn_save(&self, input: ConnectionInput) -> Result<Connection> {
        let ConnectionInput { mut connection, password, ssh_password, ssh_key_passphrase, clear_password, clear_ssh_secret } = input;
        refuse_unstorable_secrets(&connection)?;
        connection.fields.remove("password");
        let present = |s: Option<String>| s.filter(|v| !v.is_empty());
        let mut password = present(password);
        let ssh_password = present(ssh_password);
        let ssh_key_passphrase = present(ssh_key_passphrase);
        let moved_secret = match take_connection_string_secret(&mut connection.fields) {
            Some(secret) => {
                password = Some(secret);
                true
            }
            None => false,
        };
        if clear_password && password.is_some() {
            let source = if moved_secret { "the connection string carries a password" } else { "a new password was given" };
            return Err(CoreError::Invalid(format!("cannot clear the stored password: {source} in the same save")));
        }
        if clear_ssh_secret && (ssh_password.is_some() || ssh_key_passphrase.is_some()) {
            return Err(CoreError::Invalid("cannot clear the stored SSH secret: a new SSH password or key passphrase was given in the same save".into()));
        }
        connection.driver = connection.driver.trim().to_string();
        if self.drivers.def(&connection.driver).is_none() {
            return Err(CoreError::Invalid(format!("unknown driver '{}'", connection.driver)));
        }
        if let Some(ssh) = connection.ssh.as_ref() {
            if ssh.host.trim().is_empty() && ssh.enabled {
                return Err(CoreError::Invalid("ssh tunnel needs a bastion host".into()));
            }
            if ssh.auth == SshAuth::Key {
                if let Some(k) = ssh.key_path.as_deref().filter(|k| !k.trim().is_empty()) {
                    crate::tunnel::validate_key_path(k)?;
                }
            }
        }
        let saved = self.config.upsert(connection)?;
        let id = saved.id.clone();
        let db_key = credentials::db_password_key(&id);
        let ssh_pw_key = credentials::ssh_password_key(&id);
        let ssh_pp_key = credentials::ssh_passphrase_key(&id);
        if moved_secret {
            tracing::info!(conn_id = %id, "password moved out of the connection string");
        }
        if saved.save_password {
            if let Some(p) = password {
                self.creds.set(&db_key, &p)?;
                // The credential store is authoritative now; a password typed earlier this
                // session would otherwise shadow the new one.
                self.session_secrets.lock().unwrap().remove(&id);
            }
            if let Some(p) = ssh_password {
                self.creds.set(&ssh_pw_key, &p)?;
            }
            if let Some(p) = ssh_key_passphrase {
                self.creds.set(&ssh_pp_key, &p)?;
            }
        } else {
            for key in [&db_key, &ssh_pw_key, &ssh_pp_key] {
                self.creds.delete(key)?;
            }
            // "Ask every time" with a password that came inside the connection string: keep it
            // for this process only, so the connection still works until the app restarts.
            if let Some(p) = password.filter(|_| moved_secret) {
                self.session_secrets.lock().unwrap().insert(id.clone(), p);
            }
        }
        // Drop the ssh secret of an auth mode that is no longer configured.
        match saved.ssh.as_ref().map(|s| s.auth) {
            Some(SshAuth::Password) => self.creds.delete(&ssh_pp_key)?,
            Some(SshAuth::Key) => self.creds.delete(&ssh_pw_key)?,
            Some(SshAuth::Agent) | None => {
                self.creds.delete(&ssh_pw_key)?;
                self.creds.delete(&ssh_pp_key)?;
            }
        }
        if clear_password {
            self.creds.delete(&db_key)?;
            self.session_secrets.lock().unwrap().remove(&id);
            tracing::info!(conn_id = %id, "stored password cleared");
        }
        if clear_ssh_secret {
            self.creds.delete(&ssh_pw_key)?;
            self.creds.delete(&ssh_pp_key)?;
            tracing::info!(conn_id = %id, "stored ssh secret cleared");
        }
        // Host / port / credentials may have changed: forget the cached browser connection.
        self.catalog.invalidate(&id);
        drop(self.browsers.evict(&id));
        tracing::info!(conn_id = %id, name = %saved.name, driver = %saved.driver, "connection saved");
        Ok(self.with_secret_flags(saved))
    }

    pub fn conn_delete(&self, id: &str) -> Result<()> {
        if self.config.get(id).is_none() {
            return Err(CoreError::NotFound(format!("connection {id}")));
        }
        self.config.delete(id)?;
        for key in [credentials::db_password_key(id), credentials::ssh_password_key(id), credentials::ssh_passphrase_key(id)] {
            if let Err(e) = self.creds.delete(&key) {
                tracing::warn!(conn_id = %id, "cannot remove stored secret: {e}");
            }
        }
        self.session_secrets.lock().unwrap().remove(id);
        self.catalog.invalidate(id);
        drop(self.browsers.evict(id));
        tracing::info!(conn_id = %id, "connection deleted");
        Ok(())
    }

    pub fn conn_import_dbeaver(&self, path: &str) -> Result<Vec<ImportedConnection>> {
        crate::dbeaver::import_file(Path::new(path))
    }

    /// Remember a password typed for an "ask every time" connection for this process.
    fn remember_secret(&self, conn: &Connection, password: Option<String>) {
        if let Some(p) = password.filter(|p| !p.is_empty()) {
            if !conn.save_password {
                self.session_secrets.lock().unwrap().insert(conn.id.clone(), p);
            }
        }
    }

    /// Open a driver connection for `conn` (tunnel included). `password` overrides the stored
    /// one; without it, passwords typed earlier this session are used for "ask every time"
    /// connections.
    pub async fn open_connection(&self, conn: &Connection, password: Option<String>, application_name: &str) -> Result<LiveConnection> {
        let password = password.or_else(|| self.session_secrets.lock().unwrap().get(&conn.id).cloned());
        let default_port = self.drivers.get(&conn.driver).and_then(|d| d.defaults.get("port").and_then(|p| p.parse().ok()));
        let req = ConnectRequest { conn, password, application_name, default_port };
        connect::open_connection(req, &*self.creds, &self.tunnels).await
    }

    /// Tunnel -> connect -> probe. Errors name the failing step.
    pub async fn conn_test(&self, id: &str, password: Option<String>) -> Result<ProbeResult> {
        let conn = self.conn_get(id)?;
        let t0 = Instant::now();
        let live = self.open_connection(&conn, password.clone(), APP_NAME_UI).await?;
        let server = live.probe().await?;
        self.remember_secret(&conn, password);
        let mut steps = live.steps();
        steps.push(connect::Step::Probe.name().to_string());
        let elapsed_ms = t0.elapsed().as_millis() as u64;
        tracing::info!(conn_id = %id, driver = %server.driver, version = %server.version, tunnel = live.tunnel_used, elapsed_ms, "connection test ok");
        let recommended = crate::preflight::recommended_session(&server);
        let tunnel_used = live.tunnel_used;
        // The freshly tested connection becomes the catalog browser's connection.
        drop(self.browsers.insert(id, Arc::new(live)));
        Ok(ProbeResult { server, tunnel_used, elapsed_ms, recommended, steps })
    }

    // ------------------------------------------------------------------ catalog

    /// The cached `bulkdb-ui` connection, opened on first use.
    async fn browser(&self, conn_id: &str) -> Result<Arc<LiveConnection>> {
        if let Some(live) = self.browsers.get(conn_id) {
            return Ok(live);
        }
        let conn = self.conn_get(conn_id)?;
        let live = Arc::new(self.open_connection(&conn, None, APP_NAME_UI).await?);
        drop(self.browsers.insert(conn_id, live.clone()));
        Ok(live)
    }

    /// Run `f` on the browser connection; on a connection-level failure drop it and retry once
    /// on a fresh one.
    async fn with_browser<T, F, Fut>(&self, conn_id: &str, f: F) -> Result<T>
    where
        F: Fn(Arc<LiveConnection>) -> Fut,
        Fut: Future<Output = Result<T>>,
    {
        let live = self.browser(conn_id).await?;
        match f(live.clone()).await {
            Ok(v) => Ok(v),
            Err(e) if matches!(e, CoreError::Connect(_) | CoreError::Io(_) | CoreError::Tunnel(_) | CoreError::Internal(_)) => {
                tracing::warn!(conn_id = %conn_id, "browser connection failed ({e}); reopening");
                drop(self.browsers.evict(conn_id));
                drop(live);
                let live = self.browser(conn_id).await?;
                f(live).await
            }
            Err(e) => Err(e),
        }
    }

    pub async fn catalog_databases(&self, conn_id: &str, refresh: bool) -> Result<Vec<DatabaseInfo>> {
        let key = CacheKey::new(conn_id, CatalogKind::Databases, vec![]);
        if !refresh {
            if let Some(CatalogValue::Databases(v)) = self.catalog.get(&key) {
                return Ok(v);
            }
        }
        let v = self.with_browser(conn_id, |live| async move { live.databases().await }).await?;
        self.catalog.put(key, CatalogValue::Databases(v.clone()));
        Ok(v)
    }

    pub async fn catalog_schemas(&self, conn_id: &str, db: &str, refresh: bool) -> Result<Vec<SchemaInfo>> {
        let key = CacheKey::new(conn_id, CatalogKind::Schemas, vec![db.to_string()]);
        if !refresh {
            if let Some(CatalogValue::Schemas(v)) = self.catalog.get(&key) {
                return Ok(v);
            }
        }
        let v = self.with_browser(conn_id, |live| async move { live.schemas(db).await }).await?;
        self.catalog.put(key, CatalogValue::Schemas(v.clone()));
        Ok(v)
    }

    pub async fn catalog_tables(&self, conn_id: &str, db: &str, schema: &str, refresh: bool) -> Result<Vec<TableInfo>> {
        let key = CacheKey::new(conn_id, CatalogKind::Tables, vec![db.to_string(), schema.to_string()]);
        if !refresh {
            if let Some(CatalogValue::Tables(v)) = self.catalog.get(&key) {
                return Ok(v);
            }
        }
        let t0 = Instant::now();
        let v = self.with_browser(conn_id, |live| async move { live.tables(db, schema).await }).await?;
        tracing::debug!(conn_id = %conn_id, schema = %schema, tables = v.len(), elapsed_ms = t0.elapsed().as_millis() as u64, "tables listed");
        self.catalog.put(key, CatalogValue::Tables(v.clone()));
        Ok(v)
    }

    pub async fn catalog_columns(&self, conn_id: &str, table: &TableRef) -> Result<Vec<ColumnInfo>> {
        let key = CacheKey::table(conn_id, CatalogKind::Columns, table);
        if let Some(CatalogValue::Columns(v)) = self.catalog.get(&key) {
            return Ok(v);
        }
        let v = self.with_browser(conn_id, |live| async move { live.columns(table).await }).await?;
        self.catalog.put(key, CatalogValue::Columns(v.clone()));
        Ok(v)
    }

    pub async fn catalog_indexes(&self, conn_id: &str, table: &TableRef) -> Result<Vec<IndexInfo>> {
        let key = CacheKey::table(conn_id, CatalogKind::Indexes, table);
        if let Some(CatalogValue::Indexes(v)) = self.catalog.get(&key) {
            return Ok(v);
        }
        let v = self.with_browser(conn_id, |live| async move { live.indexes(table).await }).await?;
        self.catalog.put(key, CatalogValue::Indexes(v.clone()));
        Ok(v)
    }

    pub async fn catalog_ddl(&self, conn_id: &str, table: &TableRef) -> Result<String> {
        let key = CacheKey::table(conn_id, CatalogKind::Ddl, table);
        if let Some(CatalogValue::Ddl(v)) = self.catalog.get(&key) {
            return Ok(v);
        }
        let v = self.with_browser(conn_id, |live| async move { live.ddl(table).await }).await?;
        self.catalog.put(key, CatalogValue::Ddl(v.clone()));
        Ok(v)
    }

    /// Drop the cached metadata of one connection (the "refresh" button on the tree root).
    pub fn catalog_invalidate(&self, conn_id: &str) {
        self.catalog.invalidate(conn_id);
    }

    /// `SELECT col, ... FROM table` for the "make full extraction query" action.
    pub async fn catalog_select_sql(&self, conn_id: &str, table: &TableRef) -> Result<String> {
        let columns = self.catalog_columns(conn_id, table).await?;
        let live = self.browser(conn_id).await?;
        Ok(live.select_all_sql(table, &columns))
    }

    /// Starts a cancellable `count(*)` job; result arrives as `job:done` with `exact_count`.
    pub async fn catalog_exact_count(&self, conn_id: &str, table: &TableRef) -> Result<String> {
        let conn = self.conn_get(conn_id)?;
        if table.name.trim().is_empty() {
            return Err(CoreError::Invalid("table name is required".into()));
        }
        let me = self.me()?;
        let job = self.jobs.create(JobKind::ExactCount, &conn.id, &conn.name, &format!("count {}", table.qualified()), None);
        tracing::info!(job_id = %job.id, conn_id = %conn_id, table = %table.qualified(), "exact count job created");
        tokio::spawn(export::run_exact_count_job(me, job.clone(), table.clone()));
        Ok(job.id.clone())
    }

    /// The last exact count of `table` that finished on this connection (persisted in the
    /// history database by [`BulkDb::catalog_exact_count`]), so a count the user paid for is
    /// still shown next to the estimate after the job left the registry or the app restarted.
    /// `None` when the table was never counted. Unknown connection: `NOT_FOUND`.
    pub fn catalog_last_exact_count(&self, conn_id: &str, table: &TableRef) -> Result<Option<ExactCount>> {
        let conn = self.conn_get(conn_id)?;
        let stored = self.history.last_exact_count(&conn.id, &crate::history::table_key(table))?;
        // The stored key is a flat `[db.]schema.table` string whose parse is lossy (dotted
        // names, a database without a schema); the caller's table is the one the key matched.
        Ok(stored.map(|c| ExactCount { table: table.clone(), ..c }))
    }

    // ------------------------------------------------------------------ query

    pub async fn query_explain(&self, conn_id: &str, sql: &str, analyze: bool) -> Result<PlanSummary> {
        let conn = self.conn_get(conn_id)?;
        let sql = sql.trim();
        if sql.is_empty() {
            return Err(CoreError::Invalid("nothing to explain".into()));
        }
        // EXPLAIN ANALYZE, MariaDB's ANALYZE and SQL Server's STATISTICS XML execute the
        // statement, and some drivers send the text through a multi-statement protocol: the
        // extraction guard applies here too, lexed the way this connection's server reads it
        // (MySQL quotes and comments differ), and only its single normalized statement reaches
        // the server.
        let dialect = Dialect::from_driver(&conn.driver);
        let check = sqlguard::check_for(sql, dialect);
        if !check.ok {
            tracing::info!(conn_id = %conn_id, dialect = ?dialect, kind = ?check.kind, statements = check.statements, "explain refused by the SQL guard");
            return Err(CoreError::Invalid(guard_message(&check)));
        }
        let sql = check.normalized.as_str();
        let t0 = Instant::now();
        let outcome = self.with_browser(conn_id, |live| async move { live.explain(sql, analyze).await }).await;
        match outcome {
            Ok(Some(mut plan)) => {
                if let Some(root) = plan.root.as_ref() {
                    for w in crate::preflight::analyze_plan(root) {
                        if !plan.warnings.iter().any(|x| x.code == w.code && x.message == w.message) {
                            plan.warnings.push(w);
                        }
                    }
                }
                if plan.estimated_bytes.is_none() {
                    if let (Some(rows), Some(width)) = (plan.estimated_rows, plan.estimated_width) {
                        plan.estimated_bytes = Some(rows.saturating_mul(width as u64));
                    }
                }
                if plan.elapsed_ms == 0 {
                    plan.elapsed_ms = t0.elapsed().as_millis() as u64;
                }
                plan.analyzed = plan.analyzed || analyze;
                tracing::info!(conn_id = %conn_id, analyze, rows = ?plan.estimated_rows, warnings = plan.warnings.len(), elapsed_ms = plan.elapsed_ms, "explain done");
                Ok(plan)
            }
            Ok(None) => Err(CoreError::Invalid(format!("the {} driver does not provide execution plans", conn.driver))),
            Err(e) if is_lock_timeout(&e) => {
                let tables = sqlguard::referenced_tables_for(sql, dialect);
                let sessions = match self.browser(conn_id).await {
                    Ok(live) => live.monitor().active_sessions(&tables).await.unwrap_or_default(),
                    Err(_) => vec![],
                };
                tracing::warn!(conn_id = %conn_id, blocking = sessions.iter().filter(|s| s.blocking).count(), "explain hit a lock/statement timeout");
                Err(with_lock_detail(e, &sessions))
            }
            Err(e) => Err(e),
        }
    }

    /// Run the extraction SQL guard. With a connection id the query is lexed the way that
    /// connection's server reads it (`Dialect::from_driver`: MySQL comment / quote rules, every
    /// reading for ODBC), exactly as preflight and EXPLAIN will; without one the standard
    /// (PostgreSQL / ANSI) rules apply. An unknown connection id is `NOT_FOUND`.
    pub fn sql_check(&self, conn_id: Option<&str>, sql: &str) -> Result<SqlCheck> {
        let dialect = match conn_id {
            Some(id) => Dialect::from_driver(&self.conn_get(id)?.driver),
            None => Dialect::Standard,
        };
        Ok(sqlguard::check_for(sql, dialect))
    }

    pub fn sql_file_read(&self, path: &str) -> Result<SqlFile> {
        let meta = std::fs::metadata(path)?;
        let content = std::fs::read_to_string(path)?;
        let modified: chrono::DateTime<chrono::Local> = meta.modified()?.into();
        Ok(SqlFile { path: path.to_string(), content, modified_at: modified.to_rfc3339_opts(chrono::SecondsFormat::Millis, true), size_bytes: meta.len() })
    }

    // ------------------------------------------------------------------ export / jobs

    pub async fn export_preflight(&self, req: &ExportRequest) -> Result<PreflightReport> {
        let prep = export::prepare(self, req)?;
        self.remember_secret(&prep.conn, req.password.clone());
        export::preflight(self, req, &prep, &CancellationToken::new()).await
    }

    /// Create the job and run it in the background. Returns the job id immediately. When
    /// preflight produces warnings that are not in `acknowledged`, the job waits in `warned`
    /// until `job_resume` or `job_cancel`.
    pub async fn export_start(&self, req: ExportRequest, acknowledged: Vec<String>) -> Result<String> {
        // Fail fast on what needs no server: unknown connection, missing SQL, no output dir.
        let conn = self.conn_get(&req.conn_id)?;
        let (sql, _) = export::read_sql(&req)?;
        export::resolve_output_dir(&req, &conn, &self.settings_get())?;
        let label = export::label_for(&req, &sql);
        self.remember_secret(&conn, req.password.clone());
        let me = self.me()?;
        self.jobs.prune(200);
        // The stored request is what `job_get` returns over IPC: never with a password.
        let mut stored = req.clone();
        stored.password = None;
        let job = self.jobs.create(JobKind::Export, &conn.id, &conn.name, &label, Some(stored));
        tracing::info!(job_id = %job.id, conn_id = %conn.id, label = %label, format = ?req.output.format, "export job created");
        tokio::spawn(export::run_export_job(me, job.clone(), req, acknowledged));
        Ok(job.id.clone())
    }

    /// Let a `warned` job continue. Every warning code of its preflight report must be in
    /// `acknowledged`, otherwise `INVALID` and the job keeps waiting.
    pub fn job_resume(&self, job_id: &str, acknowledged: Vec<String>) -> Result<()> {
        let job = self.jobs.get(job_id).ok_or_else(|| CoreError::NotFound(format!("job {job_id}")))?;
        let state = *job.state.lock().unwrap();
        if state != JobState::Warned {
            return Err(CoreError::Invalid(format!("job {job_id} is {state:?}, not waiting for acknowledgement")));
        }
        let missing: Vec<String> = job
            .preflight
            .lock()
            .unwrap()
            .as_ref()
            .map(|r| r.warnings.iter().filter(|w| !acknowledged.contains(&w.code)).map(|w| w.code.clone()).collect())
            .unwrap_or_default();
        if !missing.is_empty() {
            return Err(CoreError::Invalid(format!("acknowledge every warning before resuming: {}", missing.join(", "))));
        }
        let tx = job.resume.lock().unwrap().take().ok_or_else(|| CoreError::Invalid(format!("job {job_id} is not waiting for acknowledgement")))?;
        tracing::info!(job_id = %job_id, codes = ?acknowledged, "job resumed");
        tx.send(acknowledged).map_err(|_| CoreError::Internal(format!("job {job_id} stopped waiting")))
    }

    pub fn job_cancel(&self, job_id: &str) -> Result<()> {
        let job = self.jobs.get(job_id).ok_or_else(|| CoreError::NotFound(format!("job {job_id}")))?;
        let state = *job.state.lock().unwrap();
        if state.is_terminal() {
            return Err(CoreError::Invalid(format!("job {job_id} already finished ({state:?})")));
        }
        tracing::info!(job_id = %job_id, state = ?state, "cancel requested");
        job.cancel.cancel();
        Ok(())
    }

    pub fn job_list(&self) -> Vec<JobSummary> {
        self.jobs.list().iter().map(|j| j.summary()).collect()
    }

    pub fn job_get(&self, job_id: &str) -> Result<JobSummary> {
        self.jobs.get(job_id).map(|j| j.summary()).ok_or_else(|| CoreError::NotFound(format!("job {job_id}")))
    }

    /// Progress of every active job (the shell emits these every 200 ms).
    pub fn progress_snapshot(&self) -> Vec<JobProgress> {
        self.jobs
            .active()
            .iter()
            .map(|j| {
                j.progress.sample();
                j.progress.snapshot(&j.id)
            })
            .collect()
    }

    /// Latest server-side snapshot of every active job (the shell emits these every 3-5 s).
    /// The monitor loop that fills them runs inside the job task.
    pub fn server_snapshot(&self) -> Vec<JobServer> {
        self.jobs.active().iter().filter_map(|j| j.server.lock().unwrap().clone()).collect()
    }

    // ------------------------------------------------------------------ history / settings

    pub fn history_list(&self, conn_id: Option<&str>, limit: usize) -> Result<Vec<HistoryEntry>> {
        self.history.list(conn_id, limit)
    }

    /// Record the end of an export run, then prune the history back to
    /// [`HISTORY_KEEP_PER_CONNECTION`] runs per connection. Both steps are best effort: a
    /// history failure is logged and never changes the job's outcome.
    pub(crate) fn history_finish(&self, job_id: &str, state: JobState, result: Option<&JobResult>, error: Option<&str>) {
        if let Err(e) = self.history.record_finish(job_id, state, result, error) {
            tracing::warn!(job_id = %job_id, "history: {e}");
        }
        if let Err(e) = self.history.prune(HISTORY_KEEP_PER_CONNECTION) {
            tracing::warn!(job_id = %job_id, "history: cannot prune old runs: {e}");
        }
    }

    pub fn settings_get(&self) -> Settings {
        self.config.settings()
    }

    /// Persist settings. A changed `max_concurrent_jobs` resizes the export slots: extra
    /// permits are added at once, surplus ones are retired as running jobs release them.
    pub fn settings_set(&self, s: Settings) -> Result<Settings> {
        let saved = self.config.set_settings(s)?;
        let wanted = saved.max_concurrent_jobs.clamp(1, 3) as usize;
        let current = self.slots_configured.swap(wanted, Ordering::SeqCst);
        if wanted > current {
            self.jobs.export_slots.add_permits(wanted - current);
        } else if wanted < current {
            let slots = self.jobs.export_slots.clone();
            let retire = (current - wanted) as u32;
            tokio::spawn(async move {
                if let Ok(permit) = slots.acquire_many_owned(retire).await {
                    permit.forget();
                }
            });
        }
        Ok(saved)
    }

    // ------------------------------------------------------------------ prompts / misc

    /// Answer a `tunnel:hostkey` prompt.
    pub fn tunnel_confirm_hostkey(&self, prompt_id: &str, accept: bool) -> Result<()> {
        self.prompts.confirm_hostkey(prompt_id, accept)
    }

    /// Answer a `tunnel:password` prompt (`None` = cancel).
    pub fn tunnel_provide_secret(&self, prompt_id: &str, secret: Option<String>) -> Result<()> {
        self.prompts.provide_secret(prompt_id, secret)
    }

    pub fn logs_recent(&self, n: usize) -> Vec<LogEvent> {
        self.logs.recent(n)
    }

    /// Static facts about this installation. `version` is the calling shell's own version
    /// (the desktop package version, the CLI crate version); everything else comes from core.
    pub fn app_info(&self, version: &str) -> AppInfo {
        AppInfo {
            version: version.to_string(),
            platform: crate::platform::os_key().to_string(),
            arch: crate::platform::arch().to_string(),
            config_dir: self.config.dir().display().to_string(),
            log_dir: crate::platform::log_dir().display().to_string(),
        }
    }

    /// Zip logs + settings (no secrets) + driver detection into `out_dir`.
    pub fn diagnostics_bundle(&self, out_dir: &str) -> Result<DiagnosticsBundle> {
        use zip::write::SimpleFileOptions;
        let out_dir = Path::new(out_dir.trim());
        if out_dir.as_os_str().is_empty() {
            return Err(CoreError::Invalid("output directory is required".into()));
        }
        std::fs::create_dir_all(out_dir)?;
        let path = out_dir.join(format!("bulkdb-diagnostics-{}.zip", chrono::Local::now().format("%Y%m%d-%H%M%S")));
        let zip_err = |e: zip::result::ZipError| CoreError::Io(format!("zip: {e}"));
        let file = std::fs::File::create(&path)?;
        let mut zip = zip::ZipWriter::new(std::io::BufWriter::new(file));
        let opts = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated).unix_permissions(0o644);
        let mut included: Vec<String> = Vec::new();

        let add_text = |zip: &mut zip::ZipWriter<std::io::BufWriter<std::fs::File>>, name: &str, text: &str, included: &mut Vec<String>| -> Result<()> {
            zip.start_file(name, opts).map_err(zip_err)?;
            zip.write_all(text.as_bytes())?;
            included.push(name.to_string());
            Ok(())
        };

        // Newest two log files (daily rotation), streamed.
        let log_dir = crate::platform::log_dir();
        for p in newest_files(&log_dir, 2) {
            let name = format!("logs/{}", p.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default());
            match std::fs::File::open(&p) {
                Ok(mut f) => {
                    zip.start_file(&name, opts).map_err(zip_err)?;
                    std::io::copy(&mut f, &mut zip)?;
                    included.push(name);
                }
                Err(e) => tracing::warn!(path = %p.display(), "diagnostics: cannot read log file: {e}"),
            }
        }
        // The in-memory ring (covers runs where file logging is off), already masked.
        let recent = self.logs.recent(crate::logging::RING_CAPACITY);
        if !recent.is_empty() {
            let text: String = recent
                .iter()
                .map(|e| {
                    format!(
                        "{} {:<5} {}{}\n",
                        e.at,
                        format!("{:?}", e.level).to_uppercase(),
                        e.job_id.as_deref().map(|j| format!("[{j}] ")).unwrap_or_default(),
                        e.message
                    )
                })
                .collect();
            add_text(&mut zip, "logs/recent.txt", &text, &mut included)?;
        }
        // settings.toml as on disk (or the effective defaults).
        let settings_text = match std::fs::read_to_string(self.config.settings_path()) {
            Ok(t) => t,
            Err(_) => toml::to_string_pretty(&self.config.settings())?,
        };
        add_text(&mut zip, "settings.toml", &crate::logging::mask_secrets(&settings_text), &mut included)?;
        // connections.toml with any secret-looking key redacted (there should be none).
        if let Ok(t) = std::fs::read_to_string(self.config.connections_path()) {
            add_text(&mut zip, "connections.toml", &redact_config(&t), &mut included)?;
        }
        add_text(&mut zip, "drivers.json", &serde_json::to_string_pretty(&self.drivers.list())?, &mut included)?;
        let system = format!(
            "BulkDB {}\nos: {} ({})\narch: {}\nconfig dir: {}\nlog dir: {}\ncredential store: {}\nactive jobs: {}\ngenerated: {}\n",
            env!("CARGO_PKG_VERSION"),
            crate::platform::os_key(),
            std::env::consts::OS,
            crate::platform::arch(),
            self.config.dir().display(),
            log_dir.display(),
            if std::env::var("BULKDB_CREDENTIALS").map(|v| v == "memory").unwrap_or(false) { "memory" } else { "os keyring" },
            self.jobs.active().len(),
            now_rfc3339(),
        );
        add_text(&mut zip, "system.txt", &system, &mut included)?;
        let mut inner = zip.finish().map_err(zip_err)?;
        inner.flush()?;
        drop(inner);
        let size_bytes = std::fs::metadata(&path)?.len();
        tracing::info!(path = %path.display(), size_bytes, "diagnostics bundle written");
        Ok(DiagnosticsBundle { path: path.display().to_string(), included, size_bytes })
    }

    /// Cancel every job, wait (at most [`SHUTDOWN_JOB_WAIT`]) for them to wind down, then close
    /// the browser connections and the tunnels.
    ///
    /// Waiting matters: the shell exits the process right after this returns, and a job that
    /// is still writing would leave a truncated file under its final name instead of
    /// `<name>.partial`, with no history row. Tunnels go last so the cancellation reaches the
    /// server instead of turning into connection errors.
    pub async fn shutdown(&self) {
        let deadline = Instant::now() + SHUTDOWN_JOB_WAIT;
        let mut announced = false;
        loop {
            let active = self.jobs.active();
            if active.is_empty() {
                break;
            }
            if !announced {
                tracing::info!(jobs = active.len(), "shutdown: cancelling active jobs");
                announced = true;
            }
            // Jobs created while shutting down are cancelled too.
            for j in &active {
                j.cancel.cancel();
            }
            if Instant::now() >= deadline {
                let ids: Vec<&str> = active.iter().map(|j| j.id.as_str()).collect();
                tracing::warn!(jobs = ?ids, "shutdown: jobs still running after {SHUTDOWN_JOB_WAIT:?}; exiting anyway");
                break;
            }
            tokio::time::sleep(SHUTDOWN_POLL).await;
        }
        drop(self.browsers.clear());
        self.catalog.clear();
        self.tunnels.shutdown().await;
    }
}

/// `INVALID` when the ODBC `connection_string` field carries a secret attribute other than the
/// password (see `connect::connection_string_unstorable_secrets`): there is nowhere to keep it,
/// and it must never reach `connections.toml` or `conn_list`. The message names the keywords
/// only, never their values.
fn refuse_unstorable_secrets(conn: &Connection) -> Result<()> {
    let Some(cs) = conn.fields.get(connect::CONNECTION_STRING_FIELD) else {
        return Ok(());
    };
    let keys = connect::connection_string_unstorable_secrets(cs);
    if keys.is_empty() {
        return Ok(());
    }
    let (noun, pronoun) = if keys.len() == 1 { ("attribute", "it") } else { ("attributes", "them") };
    Err(CoreError::Invalid(format!(
        "the connection string of '{}' contains the secret {noun} {}: secrets are never written to connections.toml, and only PWD= / Password= \
         can be kept (as the connection's password, in the OS credential store); set {pronoun} in an ODBC data source (DSN) and use DSN=, or remove {pronoun}",
        conn.name,
        keys.join(", ")
    )))
}

/// Cut every secret attribute out of the `connection_string` field, in place (see
/// `connect::split_connection_string_secret`). Returns the password (`PWD=` / `PASSWORD=`)
/// when one was present and non-empty; other secret attributes are dropped, so callers that
/// must not lose them check `refuse_unstorable_secrets` first. A string that held nothing but
/// secrets is removed from the fields altogether.
fn take_connection_string_secret(fields: &mut std::collections::BTreeMap<String, String>) -> Option<String> {
    let (cleaned, secret) = fields.get(connect::CONNECTION_STRING_FIELD).and_then(|cs| connect::split_connection_string_secret(cs))?;
    if cleaned.is_empty() {
        fields.remove(connect::CONNECTION_STRING_FIELD);
    } else {
        fields.insert(connect::CONNECTION_STRING_FIELD.to_string(), cleaned);
    }
    secret
}

/// The message of an SQL guard rejection: every issue, in order.
fn guard_message(check: &SqlCheck) -> String {
    let msg = check.issues.iter().map(|i| i.message.as_str()).collect::<Vec<_>>().join(" ");
    if msg.is_empty() {
        "only a single read-only SELECT statement can be explained".to_string()
    } else {
        msg
    }
}

/// Newest `n` regular files of `dir` by modification time (empty when the dir is missing).
fn newest_files(dir: &Path, n: usize) -> Vec<PathBuf> {
    let Ok(rd) = std::fs::read_dir(dir) else { return vec![] };
    let mut files: Vec<(std::time::SystemTime, PathBuf)> = rd
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let md = e.metadata().ok()?;
            if md.is_file() {
                Some((md.modified().ok()?, e.path()))
            } else {
                None
            }
        })
        .collect();
    files.sort_by_key(|f| std::cmp::Reverse(f.0));
    files.into_iter().take(n).map(|(_, p)| p).collect()
}

/// Redact TOML lines whose key looks like a secret; mask everything else with the log masker.
fn redact_config(text: &str) -> String {
    let secret_key = regex::Regex::new(r"(?i)^\s*[A-Za-z0-9_.\-]*(password|passwd|passphrase|secret|token|pwd)[A-Za-z0-9_.\-]*\s*=").expect("static regex");
    let mut out = String::with_capacity(text.len());
    for line in text.lines() {
        if secret_key.is_match(line) {
            let key = line.split('=').next().unwrap_or(line).trim_end();
            out.push_str(key);
            out.push_str(" = \"***\"\n");
        } else {
            out.push_str(&crate::logging::mask_secrets(line));
            out.push('\n');
        }
    }
    out
}

/// Lock / statement timeout errors from EXPLAIN that deserve the "who holds the lock" detail.
fn is_lock_timeout(e: &CoreError) -> bool {
    match e {
        CoreError::Timeout(_) => true,
        CoreError::Query { code, msg } => {
            let m = msg.to_ascii_lowercase();
            matches!(code.as_deref(), Some("55P03") | Some("57014") | Some("1205") | Some("3024") | Some("1222"))
                || (m.contains("lock") && m.contains("timeout"))
                || m.contains("lock wait timeout")
                || m.contains("canceling statement due to")
                || m.contains("lock request time out")
        }
        _ => false,
    }
}

fn format_elapsed(ms: Option<u64>) -> String {
    match ms {
        Some(ms) => {
            let s = ms / 1000;
            format!("{:02}:{:02}:{:02}", s / 3600, (s / 60) % 60, s % 60)
        }
        None => "--:--:--".to_string(),
    }
}

/// Append "waiting for lock held by pid 123 (user, 00:04:12): <query>" lines to the error.
fn with_lock_detail(e: CoreError, sessions: &[SessionInfo]) -> CoreError {
    let blocking: Vec<&SessionInfo> = sessions.iter().filter(|s| s.blocking).collect();
    let shown: Vec<&SessionInfo> = if blocking.is_empty() { sessions.iter().collect() } else { blocking };
    if shown.is_empty() {
        return e;
    }
    let lines: Vec<String> = shown
        .iter()
        .take(5)
        .map(|s| {
            let query: String = s.query.as_deref().unwrap_or("").split_whitespace().collect::<Vec<_>>().join(" ");
            let query: String = if query.chars().count() > 200 { query.chars().take(200).collect::<String>() + "…" } else { query };
            format!("waiting for lock held by pid {} ({}, {}): {}", s.pid, s.user.as_deref().unwrap_or("?"), format_elapsed(s.elapsed_ms), query)
        })
        .collect();
    let detail = lines.join("; ");
    match e {
        CoreError::Timeout(m) => CoreError::Timeout(format!("{m}; {detail}")),
        CoreError::Query { code, msg } => CoreError::Query { code, msg: format!("{msg}; {detail}") },
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_secret_keys_only() {
        let text = "[[connections]]\nname = \"x\"\npassword = \"p\"\nssh_passphrase = \"q\"\nhost = \"h\"\n";
        let out = redact_config(text);
        assert!(out.contains("password = \"***\""));
        assert!(out.contains("ssh_passphrase = \"***\""));
        assert!(out.contains("host = \"h\""));
        assert!(!out.contains("\"p\""));
    }

    #[test]
    fn lock_detail_is_appended() {
        let s = SessionInfo {
            pid: "123".into(),
            user: Some("app".into()),
            elapsed_ms: Some(252_000),
            query: Some("UPDATE   orders\nSET x = 1".into()),
            blocking: true,
            ..Default::default()
        };
        let e = with_lock_detail(CoreError::Timeout("lock timeout".into()), &[s]);
        assert_eq!(e.code(), "TIMEOUT");
        assert!(e.to_string().contains("waiting for lock held by pid 123 (app, 00:04:12): UPDATE orders SET x = 1"), "{e}");
        assert!(is_lock_timeout(&CoreError::query_with_code("55P03", "could not obtain lock")));
        assert!(is_lock_timeout(&CoreError::query("Lock wait timeout exceeded; try restarting transaction")));
        assert!(!is_lock_timeout(&CoreError::query("syntax error")));
        let unchanged = with_lock_detail(CoreError::query("x"), &[]);
        assert_eq!(unchanged.to_string(), "x");
    }

    /// `clear_password` also forgets a password typed for the connection this session, so a
    /// cleared connection really asks again.
    #[tokio::test]
    async fn clear_password_forgets_the_session_password_too() {
        let dir = tempfile::tempdir().unwrap();
        let db = BulkDb::new(BulkDbOptions {
            config_dir: Some(dir.path().to_path_buf()),
            prompts: None,
            credentials: Some(Box::new(crate::credentials::MemoryStore::default())),
            init_logging: false,
        })
        .await
        .unwrap();
        let mut fields = std::collections::BTreeMap::new();
        fields.insert("path".to_string(), dir.path().join("x.db").display().to_string());
        let conn = Connection { name: "ask".into(), driver: "sqlite".into(), fields, save_password: false, ..Default::default() };
        let saved = db.conn_save(ConnectionInput { connection: conn, ..Default::default() }).unwrap();
        db.remember_secret(&saved, Some("typed".into()));
        let remembered = |db: &BulkDb| db.session_secrets.lock().unwrap().contains_key(&saved.id);
        assert!(remembered(&db));
        // An ordinary edit keeps it ...
        db.conn_save(ConnectionInput { connection: saved.clone(), ..Default::default() }).unwrap();
        assert!(remembered(&db));
        // ... clearing the password drops it.
        db.conn_save(ConnectionInput { connection: saved.clone(), clear_password: true, ..Default::default() }).unwrap();
        assert!(!remembered(&db));
    }

    #[test]
    fn newest_files_orders_by_mtime() {
        let dir = tempfile::tempdir().unwrap();
        for (i, n) in ["a.log", "b.log", "c.log"].iter().enumerate() {
            let p = dir.path().join(n);
            std::fs::write(&p, n).unwrap();
            let t = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000 + i as u64 * 60);
            std::fs::File::options().write(true).open(&p).unwrap().set_modified(t).unwrap();
        }
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        let newest = newest_files(dir.path(), 2);
        let names: Vec<String> = newest.iter().map(|p| p.file_name().unwrap().to_string_lossy().to_string()).collect();
        assert_eq!(names, vec!["c.log", "b.log"]);
        assert!(newest_files(&dir.path().join("missing"), 2).is_empty());
    }
}
