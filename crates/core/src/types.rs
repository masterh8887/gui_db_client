//! IPC payload and domain types. Every type here is serialised with serde (camelCase) and
//! exported to TypeScript with ts-rs (`pnpm typegen`). The TS side never hand-writes these.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::error::ErrorPayload;

// ---------------------------------------------------------------------------------------------
// Connections
// ---------------------------------------------------------------------------------------------

/// A saved connection. Never contains a password: those live in the OS credential store and
/// are addressed by the connection id (see `credentials`).
#[derive(Debug, Clone, Default, Serialize, Deserialize, TS, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Connection {
    pub id: String,
    pub name: String,
    /// Driver id from `drivers/registry.json` (`postgres`, `mysql`, `mssql`, `sqlite`, `odbc`, `oracle`).
    pub driver: String,
    #[serde(default)]
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    #[serde(default)]
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    #[serde(default)]
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub database: Option<String>,
    #[serde(default)]
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    /// Driver specific fields declared by the registry (`sslmode`, `instance`, `encrypt`,
    /// `service_name`, `path`, `driver`, `connection_string`, ...).
    #[serde(default)]
    pub fields: BTreeMap<String, String>,
    #[serde(default)]
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ssh: Option<SshConfig>,
    /// Default session settings for this connection.
    #[serde(default)]
    pub session: SessionOpts,
    #[serde(default)]
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_dir: Option<String>,
    /// `false` means "ask every time"; the password is never stored.
    #[serde(default = "default_true")]
    pub save_password: bool,
    /// Computed on load: whether a password is present in the credential store.
    #[serde(default)]
    pub has_password: bool,
    /// Computed on load: whether an SSH password / key passphrase is stored.
    #[serde(default)]
    pub has_ssh_secret: bool,
}

fn default_true() -> bool {
    true
}

/// `conn_save` argument. Secrets ride along once and go straight to the keyring.
///
/// An absent / empty secret keeps whatever is stored; the `clear_*` flags remove one stored
/// secret without touching the others (a new secret and the matching `clear_*` flag in the same
/// call contradict each other and are refused).
#[derive(Debug, Clone, Default, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct ConnectionInput {
    #[serde(flatten)]
    pub connection: Connection,
    #[serde(default)]
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
    #[serde(default)]
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ssh_password: Option<String>,
    #[serde(default)]
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ssh_key_passphrase: Option<String>,
    /// Delete the stored database password (and one typed earlier this session).
    #[serde(default)]
    #[ts(as = "Option<bool>", optional)]
    pub clear_password: bool,
    /// Delete the stored SSH password / key passphrase.
    #[serde(default)]
    #[ts(as = "Option<bool>", optional)]
    pub clear_ssh_secret: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS, Default)]
#[serde(rename_all = "lowercase")]
pub enum SshAuth {
    #[default]
    Password,
    Key,
    Agent,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SshConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    pub host: String,
    #[serde(default = "default_ssh_port")]
    pub port: u16,
    pub user: String,
    #[serde(default)]
    pub auth: SshAuth,
    /// OpenSSH private key path (`.ppk` is rejected with a conversion hint).
    #[serde(default)]
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key_path: Option<String>,
    /// `ssh -C`; on by default (3-5x for text transfers).
    #[serde(default = "default_true")]
    pub compression: bool,
}

fn default_ssh_port() -> u16 {
    22
}

/// Session settings applied before the extraction query. Drivers translate these into their own
/// `SET` statements and hide what they do not support (see registry `session_opts`).
#[derive(Debug, Clone, Default, Serialize, Deserialize, TS, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SessionOpts {
    /// PostgreSQL `work_mem`, e.g. `"64MB"`.
    #[serde(default)]
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub work_mem: Option<String>,
    /// Statement timeout in milliseconds (PG `statement_timeout`, MySQL `max_execution_time`).
    #[serde(default)]
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub statement_timeout_ms: Option<u64>,
    /// Lock timeout in milliseconds (PG `lock_timeout`, MSSQL `SET LOCK_TIMEOUT`).
    #[serde(default)]
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lock_timeout_ms: Option<u64>,
    /// PostgreSQL `max_parallel_workers_per_gather`.
    #[serde(default)]
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_parallel_workers: Option<u32>,
    /// Rows per fetch for cursor based drivers (MySQL/MSSQL/ODBC/SQLite). Default 65 536.
    #[serde(default)]
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fetch_batch_rows: Option<u32>,
}

// ---------------------------------------------------------------------------------------------
// Output
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS, Default)]
#[serde(rename_all = "lowercase")]
pub enum OutputFormat {
    #[default]
    Csv,
    Parquet,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CsvOpts {
    #[serde(default = "default_true")]
    pub header: bool,
    /// Single character. `"\t"` gives TSV.
    #[serde(default = "default_delimiter")]
    pub delimiter: String,
    /// Text written for NULL: `""`, `"NULL"`, `"\\N"`, ...
    #[serde(default)]
    pub null_as: String,
    #[serde(default)]
    pub bom: bool,
    /// CRLF line endings (default on Windows, off on macOS).
    #[serde(default)]
    pub crlf: bool,
}

fn default_delimiter() -> String {
    ",".into()
}

impl Default for CsvOpts {
    fn default() -> Self {
        CsvOpts { header: true, delimiter: ",".into(), null_as: String::new(), bom: false, crlf: false }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS, Default)]
#[serde(rename_all = "lowercase")]
pub enum ParquetCompression {
    #[default]
    Zstd,
    Snappy,
    None,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ParquetOpts {
    #[serde(default)]
    pub compression: ParquetCompression,
    /// Row group size in MB (default 128).
    #[serde(default = "default_row_group_mb")]
    pub row_group_mb: u32,
}

fn default_row_group_mb() -> u32 {
    128
}

impl Default for ParquetOpts {
    fn default() -> Self {
        ParquetOpts { compression: ParquetCompression::Zstd, row_group_mb: 128 }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "lowercase")]
pub enum Compression {
    Gzip,
    Zstd,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, TS, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SplitOpts {
    #[serde(default)]
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rows: Option<u64>,
    #[serde(default)]
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub megabytes: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct OutputOpts {
    pub dir: String,
    /// File name pattern without extension. Placeholders: `{conn}`, `{name}`, `{yyyyMMdd_HHmm}`,
    /// `{yyyyMMdd}`, `{HHmmss}`. Extensions (`.csv`, `.parquet`, `.gz`, `.zst`, `_part0001`) are appended.
    #[serde(default = "default_name_pattern")]
    pub name_pattern: String,
    #[serde(default)]
    pub format: OutputFormat,
    #[serde(default)]
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub csv: Option<CsvOpts>,
    #[serde(default)]
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parquet: Option<ParquetOpts>,
    #[serde(default)]
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compress: Option<Compression>,
    #[serde(default)]
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub split: Option<SplitOpts>,
    #[serde(default)]
    pub overwrite: bool,
}

fn default_name_pattern() -> String {
    "{conn}_{name}_{yyyyMMdd_HHmm}".into()
}

impl Default for OutputOpts {
    fn default() -> Self {
        OutputOpts {
            dir: String::new(),
            name_pattern: default_name_pattern(),
            format: OutputFormat::Csv,
            csv: Some(CsvOpts::default()),
            parquet: None,
            compress: None,
            split: None,
            overwrite: false,
        }
    }
}

/// `export_preflight` / `export_start` argument.
#[derive(Debug, Clone, Serialize, Deserialize, TS, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ExportRequest {
    pub conn_id: String,
    /// Inline SQL. Exactly one of `sql` / `sql_file` must be set.
    #[serde(default)]
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sql: Option<String>,
    /// Path to a `.sql` file; read at execution time.
    #[serde(default)]
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sql_file: Option<String>,
    /// Human label used for `{name}` in the file name pattern and in history.
    #[serde(default)]
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default)]
    pub session: SessionOpts,
    pub output: OutputOpts,
    /// Password for connections saved with `save_password = false`.
    #[serde(default)]
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
}

// ---------------------------------------------------------------------------------------------
// Server / probe
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize, TS, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ServerInfo {
    pub driver: String,
    pub version: String,
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub database: Option<String>,
    /// Estimated server RAM (PostgreSQL: `shared_buffers x 4`).
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ram_bytes: Option<u64>,
    /// How `ram_bytes` was derived, for the UI tooltip.
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ram_source: Option<String>,
    /// Current server defaults relevant to sessions (`work_mem`, `shared_buffers`,
    /// `statement_timeout`, `max_parallel_workers_per_gather`, ...).
    #[serde(default)]
    pub settings: BTreeMap<String, String>,
    /// `true` when the account can write (INSERT/UPDATE/DDL) somewhere obvious; a read-only
    /// account is recommended for extraction.
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub can_write: Option<bool>,
    #[serde(default)]
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ProbeResult {
    pub server: ServerInfo,
    pub tunnel_used: bool,
    pub elapsed_ms: u64,
    /// Safe defaults derived from the server (work_mem = RAM/64, statement_timeout 60 min, ...).
    pub recommended: SessionOpts,
    /// Which step failed when the probe fails is carried in the error; on success this lists the
    /// steps that ran (`tunnel`, `connect`, `probe`).
    #[serde(default)]
    pub steps: Vec<String>,
}

// ---------------------------------------------------------------------------------------------
// Catalog
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize, TS, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[serde(rename_all = "camelCase")]
pub struct TableRef {
    #[serde(default)]
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub database: Option<String>,
    #[serde(default)]
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,
    pub name: String,
}

impl TableRef {
    pub fn new(schema: Option<&str>, name: &str) -> Self {
        TableRef { database: None, schema: schema.map(|s| s.to_string()), name: name.to_string() }
    }

    /// `schema.name` or just `name`.
    pub fn qualified(&self) -> String {
        match &self.schema {
            Some(s) if !s.is_empty() => format!("{s}.{}", self.name),
            _ => self.name.clone(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, TS, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct DatabaseInfo {
    pub name: String,
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size_bytes: Option<u64>,
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub encoding: Option<String>,
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub collation: Option<String>,
    pub accessible: bool,
    pub is_current: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, TS, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SchemaInfo {
    pub name: String,
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub table_count: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS, Default)]
#[serde(rename_all = "snake_case")]
pub enum TableKind {
    #[default]
    Table,
    View,
    MaterializedView,
    Partitioned,
    Foreign,
    Other,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, TS, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TableInfo {
    pub schema: String,
    pub name: String,
    pub kind: TableKind,
    /// Statistics based estimate; the UI always labels it "estimated".
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub estimated_rows: Option<u64>,
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub table_bytes: Option<u64>,
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub index_bytes: Option<u64>,
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_bytes: Option<u64>,
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, TS, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ColumnInfo {
    pub position: u32,
    pub name: String,
    /// Full type including length / precision, e.g. `numeric(12,2)`.
    pub data_type: String,
    pub nullable: bool,
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
    pub primary_key: bool,
    /// Referenced `schema.table(column)` when the column is part of a foreign key.
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub foreign_key: Option<String>,
    pub unique: bool,
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, TS, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct IndexInfo {
    pub name: String,
    pub columns: Vec<String>,
    pub unique: bool,
    pub primary: bool,
    /// `btree`, `gin`, `hash`, `clustered`, ...
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// Partial index predicate (`WHERE ...`), shown verbatim.
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub predicate: Option<String>,
    /// `INCLUDE` columns.
    #[serde(default)]
    pub include: Vec<String>,
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size_bytes: Option<u64>,
    /// `idx_scan` where the DB supports it.
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scans: Option<u64>,
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub definition: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, TS, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ExactCount {
    pub table: TableRef,
    pub rows: u64,
    pub elapsed_ms: u64,
    /// RFC 3339 timestamp.
    pub at: String,
}

// ---------------------------------------------------------------------------------------------
// Explain / plan
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS, Default)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Info,
    #[default]
    Warning,
    Danger,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PlanWarning {
    /// `LATERAL_PATTERN`, `SEQ_SCAN_LARGE`, `SORT_SPILL`, `HASH_LARGE`, ...
    pub code: String,
    pub severity: Severity,
    pub message: String,
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// Driver-neutral plan node, parsed from EXPLAIN output where the DB gives structure.
#[derive(Debug, Clone, Default, Serialize, Deserialize, TS, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PlanNode {
    pub node_type: String,
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub relation: Option<TableRef>,
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan_rows: Option<f64>,
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan_width: Option<u32>,
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_cost: Option<f64>,
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub startup_cost: Option<f64>,
    /// Only present for EXPLAIN ANALYZE.
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actual_rows: Option<f64>,
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actual_loops: Option<f64>,
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actual_time_ms: Option<f64>,
    /// `SubPlan`, `InitPlan`, `Outer`, `Inner`, ...
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_relationship: Option<String>,
    #[serde(default)]
    pub children: Vec<PlanNode>,
    /// Anything else worth showing (`Filter`, `Index Cond`, `Sort Key`, `Hash Cond`).
    #[serde(default)]
    pub extra: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, TS, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PlanSummary {
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub estimated_rows: Option<u64>,
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub estimated_width: Option<u32>,
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_cost: Option<f64>,
    /// `estimated_rows x estimated_width` (bytes), a rough output size.
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub estimated_bytes: Option<u64>,
    pub analyzed: bool,
    pub elapsed_ms: u64,
    pub warnings: Vec<PlanWarning>,
    /// Relations referenced by the plan.
    pub tables: Vec<TableRef>,
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub root: Option<PlanNode>,
    /// Original EXPLAIN output (JSON for PostgreSQL/MySQL, XML for MSSQL, text otherwise).
    pub raw: String,
    /// `json`, `xml`, `text`.
    pub raw_format: String,
}

// ---------------------------------------------------------------------------------------------
// Preflight
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, TS, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PreflightIssue {
    /// `WORK_MEM_HIGH`, `CONCURRENT_SESSION`, `PLAN_LATERAL`, `DISK_LOW`, `OVERWRITE`,
    /// `DIR_NOT_WRITABLE`, `DISK_FULL`, `SQL_INVALID`, `MULTI_STATEMENT`, `NOT_READONLY`, ...
    pub code: String,
    pub severity: Severity,
    pub message: String,
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recommendation: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, TS, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PreflightEstimates {
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rows: Option<u64>,
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bytes: Option<u64>,
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disk_free_bytes: Option<u64>,
    /// First output file path that will be written.
    pub output_path: String,
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub files: Option<u32>,
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub work_mem: Option<String>,
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recommended_work_mem: Option<String>,
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server_ram_bytes: Option<u64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, TS, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SessionInfo {
    pub pid: String,
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub application: Option<String>,
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wait_event: Option<String>,
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub elapsed_ms: Option<u64>,
    /// Tables this session touches that overlap with the request.
    #[serde(default)]
    pub tables: Vec<String>,
    /// Set when this session holds a lock we are waiting for.
    #[serde(default)]
    pub blocking: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, TS, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PreflightReport {
    pub warnings: Vec<PreflightIssue>,
    pub blockers: Vec<PreflightIssue>,
    pub estimates: PreflightEstimates,
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan: Option<PlanSummary>,
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server: Option<ServerInfo>,
    #[serde(default)]
    pub sessions: Vec<SessionInfo>,
    pub elapsed_ms: u64,
}

// ---------------------------------------------------------------------------------------------
// Jobs
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum JobKind {
    Export,
    ExactCount,
    LocalOpen,
}

/// The nine-state machine from the spec.
///
/// ```text
/// pending -> preflight -> warned ---(ack)---> running -> finishing -> done
///                      \-> rejected          |   \-> cancelled
///                                            \-> failed
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS, Hash)]
#[serde(rename_all = "lowercase")]
pub enum JobState {
    Pending,
    Preflight,
    Warned,
    Rejected,
    Running,
    Finishing,
    Done,
    Cancelled,
    Failed,
}

impl JobState {
    pub fn is_terminal(self) -> bool {
        matches!(self, JobState::Rejected | JobState::Done | JobState::Cancelled | JobState::Failed)
    }
    pub fn is_active(self) -> bool {
        matches!(self, JobState::Pending | JobState::Preflight | JobState::Warned | JobState::Running | JobState::Finishing)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS, Default)]
#[serde(rename_all = "snake_case")]
pub enum JobPhase {
    /// Rows have not started flowing (server sorting / hashing).
    #[default]
    Preparing,
    Transferring,
    /// Server waits on `ClientWrite` more than half the time: the line is the bottleneck.
    NetworkBound,
    Finishing,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, TS, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct JobProgress {
    pub job_id: String,
    pub rows: u64,
    pub bytes_in: u64,
    pub bytes_out: u64,
    pub batches: u64,
    pub files: u32,
    pub elapsed_ms: u64,
    pub rate_rows_per_sec: f64,
    pub rate_bytes_per_sec: f64,
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub eta_sec: Option<u64>,
    pub phase: JobPhase,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "lowercase")]
pub enum Bottleneck {
    Network,
    Server,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, TS, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct JobServer {
    pub job_id: String,
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<String>,
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wait_event: Option<String>,
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server_rows: Option<u64>,
    /// Growth of temp file usage since the job started (PostgreSQL `temp_bytes`).
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temp_bytes: Option<u64>,
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bottleneck: Option<Bottleneck>,
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub elapsed_ms: Option<u64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, TS, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct JobResult {
    pub rows: u64,
    pub bytes: u64,
    pub files: Vec<String>,
    pub duration_ms: u64,
    /// Post-run verification (file sizes on disk, optional DuckDB recount).
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verified_rows: Option<u64>,
    #[serde(default)]
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct JobDone {
    pub job_id: String,
    pub state: JobState,
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<JobResult>,
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorPayload>,
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exact_count: Option<ExactCount>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct JobStateEvent {
    pub job_id: String,
    pub state: JobState,
    /// RFC 3339.
    pub at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct JobSummary {
    pub id: String,
    pub kind: JobKind,
    pub state: JobState,
    pub conn_id: String,
    pub conn_name: String,
    pub label: String,
    pub started_at: String,
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<String>,
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub progress: Option<JobProgress>,
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server: Option<JobServer>,
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request: Option<ExportRequest>,
    /// Set while `state == warned`: what the user must acknowledge.
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preflight: Option<PreflightReport>,
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<JobResult>,
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorPayload>,
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exact_count: Option<ExactCount>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct LogEvent {
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub job_id: Option<String>,
    pub level: LogLevel,
    pub message: String,
    pub at: String,
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
}

// ---------------------------------------------------------------------------------------------
// History / settings / drivers / tunnel
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, TS, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct HistoryEntry {
    pub id: i64,
    pub job_id: String,
    pub conn_id: String,
    pub started_at: String,
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<String>,
    pub state: JobState,
    pub label: String,
    pub sql: String,
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sql_file: Option<String>,
    pub session: SessionOpts,
    pub output: OutputOpts,
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rows: Option<u64>,
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bytes: Option<u64>,
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(default)]
    pub files: Vec<String>,
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    /// 1..=3 concurrent extraction jobs.
    #[serde(default = "default_max_jobs")]
    pub max_concurrent_jobs: u32,
    #[serde(default)]
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_output_dir: Option<String>,
    /// Platform default: CRLF on Windows, LF on macOS. A settings file without this key keeps
    /// the platform default (not `false`), so partial or older files do not flip line endings.
    #[serde(default = "crate::platform::csv_crlf_default")]
    pub csv_crlf_default: bool,
    #[serde(default = "default_log_level")]
    pub log_level: String,
    #[serde(default = "default_tunnel_idle")]
    pub tunnel_idle_secs: u32,
    #[serde(default = "default_monitor_interval")]
    pub monitor_interval_ms: u32,
    #[serde(default = "default_progress_interval")]
    pub progress_interval_ms: u32,
    /// Verify CSV outputs by re-reading them with the csv crate after the run (slow for huge
    /// files; off by default).
    #[serde(default)]
    pub verify_after_export: bool,
    /// Free-form UI preferences (theme, panel sizes).
    #[serde(default)]
    pub ui: BTreeMap<String, String>,
}

fn default_max_jobs() -> u32 {
    1
}
fn default_log_level() -> String {
    "info".into()
}
fn default_tunnel_idle() -> u32 {
    60
}
fn default_monitor_interval() -> u32 {
    3000
}
fn default_progress_interval() -> u32 {
    200
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            max_concurrent_jobs: 1,
            default_output_dir: None,
            csv_crlf_default: crate::platform::csv_crlf_default(),
            log_level: "info".into(),
            tunnel_idle_secs: 60,
            monitor_interval_ms: 3000,
            progress_interval_ms: 200,
            verify_after_export: false,
            ui: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "kebab-case")]
pub enum DriverTier {
    Builtin,
    VendorClient,
    Odbc,
    Plugin,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct DriverInfo {
    pub id: String,
    pub name: String,
    pub tier: DriverTier,
    pub available: bool,
    /// Why it is unavailable (missing library, wrong architecture, not compiled in).
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub install_label: Option<String>,
    /// Download page for the client, always an absolute `https://` URL (safe to open).
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub install_url: Option<String>,
    /// Plain-text install instruction (e.g. a package-manager command); never a link.
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub install_hint: Option<String>,
    pub fields: Vec<String>,
    #[serde(default)]
    pub defaults: BTreeMap<String, String>,
    pub session_opts: Vec<String>,
    /// Installed ODBC driver names (ODBC tier only).
    #[serde(default)]
    pub odbc_drivers: Vec<String>,
    /// Library that was found, for diagnostics.
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub library_path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct HostKeyPrompt {
    pub prompt_id: String,
    pub host: String,
    pub fingerprint: String,
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key_type: Option<String>,
    /// The raw ssh message, for the dialog.
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SshPasswordPrompt {
    pub prompt_id: String,
    pub host: String,
    pub user: String,
    /// `password` or `passphrase`.
    pub kind: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SqlFile {
    pub path: String,
    pub content: String,
    /// Modification time as RFC 3339, so the UI can detect changes.
    pub modified_at: String,
    pub size_bytes: u64,
}

/// Row produced by `DBeaver data-sources.json` import.
#[derive(Debug, Clone, Serialize, Deserialize, TS, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ImportedConnection {
    pub connection: Connection,
    #[serde(default)]
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct DiagnosticsBundle {
    pub path: String,
    pub included: Vec<String>,
    pub size_bytes: u64,
}

/// Static facts about the running app (`app_info`: About dialog, diagnostics header).
#[derive(Debug, Clone, Serialize, Deserialize, TS, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AppInfo {
    /// Version of the shell that asked (desktop package version, CLI crate version).
    pub version: String,
    /// `windows | macos | linux`.
    pub platform: String,
    /// `x86_64 | aarch64`.
    pub arch: String,
    /// Where `connections.toml`, `settings.toml` and `history.sqlite` live.
    pub config_dir: String,
    /// Where the daily log files are written.
    pub log_dir: String,
}

/// Events pushed from core to the shell. Progress is *not* an event: it is polled.
#[derive(Debug, Clone, Serialize, Deserialize, TS, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CoreEvent {
    JobState(JobStateEvent),
    JobDone(JobDone),
    JobLog(LogEvent),
    TunnelHostKey(HostKeyPrompt),
    TunnelPassword(SshPasswordPrompt),
    DriversChanged(Vec<DriverInfo>),
}

/// Helper: now as RFC 3339 with milliseconds.
pub fn now_rfc3339() -> String {
    chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A partial settings file must fall back to the same values as `Settings::default()`,
    /// in particular the platform CSV line ending (CRLF on Windows).
    #[test]
    fn partial_settings_keep_platform_defaults() {
        let s: Settings = toml::from_str("maxConcurrentJobs = 1").unwrap();
        assert_eq!(s.csv_crlf_default, crate::platform::csv_crlf_default());
        assert_eq!(s, Settings::default());

        let empty: Settings = serde_json::from_str("{}").unwrap();
        assert_eq!(empty.csv_crlf_default, crate::platform::csv_crlf_default());

        let explicit: Settings = toml::from_str("csvCrlfDefault = false").unwrap();
        assert!(!explicit.csv_crlf_default);
        let explicit: Settings = toml::from_str("csvCrlfDefault = true").unwrap();
        assert!(explicit.csv_crlf_default);
    }

    /// Older frontends send no `clear*` flags: they default to "keep every stored secret".
    #[test]
    fn connection_input_clear_flags_are_optional() {
        let base = r#"{"id":"c1","name":"n","driver":"sqlite","fields":{},"session":{},"savePassword":true,"hasPassword":true,"hasSshSecret":false"#;
        let plain: ConnectionInput = serde_json::from_str(&format!("{base}}}")).unwrap();
        assert!(!plain.clear_password && !plain.clear_ssh_secret);
        assert_eq!(plain.connection.id, "c1");
        let flagged: ConnectionInput = serde_json::from_str(&format!(r#"{base},"clearPassword":true,"clearSshSecret":true}}"#)).unwrap();
        assert!(flagged.clear_password && flagged.clear_ssh_secret);
        let v = serde_json::to_value(&flagged).unwrap();
        assert_eq!(v["clearPassword"], true);
        assert_eq!(v["clearSshSecret"], true);
    }

    /// The generated TypeScript declares every `#[ts(optional)]` field as `field?: T`, i.e. the
    /// key is absent when there is no value. The wire must match: a `None` is omitted, never sent
    /// as `null` (the plan view guards with `!== undefined`; a SQLite plan has no costs at all).
    #[test]
    fn optional_fields_are_omitted_not_null() {
        let leaf = PlanNode { node_type: "SCAN".into(), relation: Some(TableRef::new(None, "t")), ..Default::default() };
        let plan = PlanSummary {
            root: Some(PlanNode { node_type: "QUERY PLAN".into(), children: vec![leaf], ..Default::default() }),
            raw: "SCAN t".into(),
            raw_format: "text".into(),
            ..Default::default()
        };
        let text = serde_json::to_string(&plan).unwrap();
        assert!(!text.contains("null"), "{text}");
        assert!(!text.contains("totalCost") && !text.contains("estimatedRows") && !text.contains("schema"), "{text}");
        let back: PlanSummary = serde_json::from_str(&text).unwrap();
        assert_eq!(back, plan, "absent keys read back as None");

        let err = crate::error::ErrorPayload { code: "IO".into(), message: "m".into(), detail: None };
        assert_eq!(serde_json::to_value(&err).unwrap(), serde_json::json!({ "code": "IO", "message": "m" }));
    }

    #[test]
    fn app_info_is_camel_case() {
        let info = AppInfo { version: "1".into(), platform: "windows".into(), arch: "x86_64".into(), config_dir: "c".into(), log_dir: "l".into() };
        let v = serde_json::to_value(&info).unwrap();
        assert_eq!(v["configDir"], "c");
        assert_eq!(v["logDir"], "l");
        assert_eq!(v.as_object().unwrap().len(), 5);
    }
}
