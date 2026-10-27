//! `Source`: read a query result as a stream of batches without materialising rows.
//!
//! One file per driver (`postgres.rs`, `mysql.rs`, ...). Each driver exposes a `XxxDatabase`
//! struct implementing [`Source`], [`crate::catalog::Catalog`] and [`crate::monitor::Monitor`],
//! and a `connect(&ConnectParams)` constructor. [`open`] dispatches on the driver id.

use std::collections::BTreeMap;
use std::pin::Pin;
use std::time::Duration;

use arrow_array::RecordBatch;
use arrow_schema::SchemaRef;
use async_trait::async_trait;
use bytes::Bytes;
use futures::Stream;
use tokio_util::sync::CancellationToken;

use crate::catalog::Catalog;
use crate::error::{CoreError, Result};
use crate::monitor::Monitor;
use crate::types::{Connection, PlanSummary, ServerInfo, SessionOpts};

pub mod arrow_batch;
#[cfg(feature = "mssql")]
pub mod mssql;
#[cfg(feature = "mysql")]
pub mod mysql;
#[cfg(has_odbc)]
pub mod odbc;
#[cfg(has_oracle)]
pub mod oracle;
#[cfg(feature = "postgres")]
pub mod postgres;
pub mod sqlite;

pub type BoxStream<'a, T> = Pin<Box<dyn Stream<Item = T> + Send + 'a>>;

/// One unit flowing through the bounded channel.
#[derive(Debug)]
pub enum Batch {
    /// CSV header line, newline terminated, exactly as the sink should write it. Sent at most
    /// once and always first. Only on the PostgreSQL COPY path; Arrow batches carry a schema.
    RawCsvHeader(Bytes),
    /// Whole CSV rows produced by the server (PostgreSQL `COPY ... TO STDOUT (FORMAT csv)`),
    /// already in the requested dialect (delimiter, NULL text, line ending). `rows` counts the
    /// complete rows in `data`. Goes to disk without decoding.
    RawCsv { data: Bytes, rows: u64 },
    /// Column-oriented batch produced by MySQL / MSSQL / SQLite / ODBC / Oracle, and by
    /// PostgreSQL when the output is Parquet (COPY binary decoded).
    Arrow(RecordBatch),
}

impl Batch {
    pub fn rows(&self) -> u64 {
        match self {
            Batch::RawCsvHeader(_) => 0,
            Batch::RawCsv { rows, .. } => *rows,
            Batch::Arrow(b) => b.num_rows() as u64,
        }
    }

    /// Approximate bytes carried (what the channel bound is measured in).
    pub fn bytes(&self) -> u64 {
        match self {
            Batch::RawCsvHeader(b) => b.len() as u64,
            Batch::RawCsv { data, .. } => data.len() as u64,
            Batch::Arrow(b) => b.get_array_memory_size() as u64,
        }
    }
}

/// CSV dialect the source must produce on the RawCsv path (mirrors `CsvOpts`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CsvFormat {
    pub delimiter: u8,
    pub header: bool,
    pub null_as: String,
    pub crlf: bool,
}

impl Default for CsvFormat {
    fn default() -> Self {
        CsvFormat { delimiter: b',', header: true, null_as: String::new(), crlf: false }
    }
}

/// What the sink wants from the stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamOutput {
    /// Server-side CSV bytes (PostgreSQL only). Drivers that cannot produce raw CSV fall back
    /// to `Arrow` automatically; sinks accept both.
    RawCsv(CsvFormat),
    Arrow,
}

#[derive(Debug, Clone)]
pub struct Query {
    pub sql: String,
    pub output: StreamOutput,
}

/// Result of [`Source::stream`].
pub struct SourceStream {
    /// Known up front for Arrow producers (and for PostgreSQL via a prepared statement);
    /// `None` when the first batch carries it.
    pub schema: Option<SchemaRef>,
    pub stream: BoxStream<'static, Result<Batch>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SslMode {
    Disable,
    #[default]
    Prefer,
    Require,
    VerifyFull,
}

impl SslMode {
    pub fn parse(s: &str) -> SslMode {
        match s.trim().to_ascii_lowercase().as_str() {
            "disable" | "off" | "false" | "no" => SslMode::Disable,
            "require" | "required" | "on" | "true" | "yes" => SslMode::Require,
            "verify-full" | "verify_full" | "verify-ca" | "verifyfull" => SslMode::VerifyFull,
            _ => SslMode::Prefer,
        }
    }
}

/// Fully resolved connection parameters: after tunnel substitution, with the password fetched.
#[derive(Debug, Clone)]
pub struct ConnectParams {
    pub driver: String,
    pub host: String,
    pub port: u16,
    pub database: Option<String>,
    pub user: Option<String>,
    pub password: Option<String>,
    pub fields: BTreeMap<String, String>,
    /// `bulkdb-<job id>` for extraction sessions, `bulkdb-ui` for catalog/probe/monitor.
    pub application_name: String,
    pub connect_timeout: Duration,
    pub ssl: SslMode,
}

impl ConnectParams {
    pub fn from_connection(conn: &Connection, password: Option<String>, host_override: Option<(String, u16)>, application_name: impl Into<String>) -> Self {
        let (host, port) = match host_override {
            Some((h, p)) => (h, p),
            None => (conn.host.clone().unwrap_or_else(|| "localhost".into()), conn.port.unwrap_or(0)),
        };
        let ssl = conn.fields.get("sslmode").map(|s| SslMode::parse(s)).unwrap_or_default();
        ConnectParams {
            driver: conn.driver.clone(),
            host,
            port,
            database: conn.database.clone(),
            user: conn.user.clone(),
            password,
            fields: conn.fields.clone(),
            application_name: application_name.into(),
            connect_timeout: Duration::from_secs(15),
            ssl,
        }
    }

    pub fn field(&self, key: &str) -> Option<&str> {
        self.fields.get(key).map(|s| s.as_str()).filter(|s| !s.is_empty())
    }

    pub fn field_bool(&self, key: &str, default: bool) -> bool {
        match self.field(key) {
            Some(v) => matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"),
            None => default,
        }
    }
}

#[async_trait]
pub trait Source: Send + Sync {
    /// Connection check + server facts (version, memory settings). Used by "test connection"
    /// and by preflight.
    async fn probe(&self) -> Result<ServerInfo>;

    /// Apply session settings (`SET work_mem` etc.) on this connection. Unsupported keys are
    /// ignored silently; the registry tells the UI which keys the driver understands.
    async fn apply_session(&self, opts: &SessionOpts) -> Result<()>;

    /// Execute the query and return the batch stream. Returns as soon as the server accepted
    /// the query; rows flow while the stream is polled. Cancellation must cancel the server
    /// side session (protocol level cancel for PostgreSQL, `KILL QUERY` for MySQL, ...).
    async fn stream(&self, q: &Query, opts: &SessionOpts, cancel: CancellationToken) -> Result<SourceStream>;

    /// Execution plan if the database has one. `analyze = true` really runs the query.
    /// Planner-only EXPLAIN must run with a short statement/lock timeout (15 s / 3 s).
    async fn explain(&self, sql: &str, analyze: bool) -> Result<Option<PlanSummary>>;

    /// Server-side identifier of this session (PostgreSQL backend pid, MySQL connection id,
    /// MSSQL session id) so a monitor can find it.
    fn session_id(&self) -> Option<String>;

    /// Driver id (`postgres`, ...).
    fn driver_id(&self) -> &'static str;

    /// Whether this driver can produce `Batch::RawCsv` (PostgreSQL only).
    fn supports_raw_csv(&self) -> bool {
        false
    }
}

/// A driver connection: one object that reads, browses and monitors.
pub trait Database: Source + Catalog + Monitor + Send + Sync {}

/// Open a connection with the right driver. Tier 2 drivers return `DriverMissing` when the
/// vendor library is not loadable.
pub async fn open(params: &ConnectParams) -> Result<Box<dyn Database>> {
    match params.driver.as_str() {
        #[cfg(feature = "postgres")]
        "postgres" => Ok(Box::new(postgres::PgDatabase::connect(params).await?)),
        #[cfg(feature = "mysql")]
        "mysql" => Ok(Box::new(mysql::MySqlDatabase::connect(params).await?)),
        #[cfg(feature = "mssql")]
        "mssql" => Ok(Box::new(mssql::MssqlDatabase::connect(params).await?)),
        "sqlite" => Ok(Box::new(sqlite::SqliteDatabase::connect(params).await?)),
        #[cfg(has_odbc)]
        "odbc" => Ok(Box::new(odbc::OdbcDatabase::connect(params).await?)),
        #[cfg(has_oracle)]
        "oracle" => Ok(Box::new(oracle::OracleDatabase::connect(params).await?)),
        other => Err(CoreError::DriverMissing { id: other.to_string(), detail: "driver is not compiled into this build".into() }),
    }
}

/// Whether a driver id is compiled into this build (independent of vendor libraries).
pub fn is_compiled(driver: &str) -> bool {
    match driver {
        "postgres" => cfg!(feature = "postgres"),
        "mysql" => cfg!(feature = "mysql"),
        "mssql" => cfg!(feature = "mssql"),
        "sqlite" => true,
        "odbc" => cfg!(has_odbc),
        "oracle" => cfg!(has_oracle),
        _ => false,
    }
}
