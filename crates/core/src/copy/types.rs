//! DB copy types (docs/db-copy.md section 2). Frozen after the skeleton (W0): only the owner
//! **copy-data** may *add* optional fields here, and reports each so the shared owner runs
//! `pnpm typegen`.
//!
//! * IPC types derive `Serialize, Deserialize, TS` (camelCase structs, snake_case enums) and are
//!   exported by `ts_export.rs`; `crate::types` re-exports everything here.
//! * Option structs carry struct-level `#[serde(default)]` with a manual `Default` holding the
//!   documented defaults; TS sends whole objects (`DEFAULT_COPY_OPTIONS`).
//! * The internal contract types at the bottom (not IPC) are where copy-schema and copy-data
//!   meet.

use std::collections::BTreeMap;

use arrow_array::RecordBatch;
use async_trait::async_trait;
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use ts_rs::TS;

use super::hash::TableChecksum;

use crate::error::{ErrorPayload, Result};
use crate::types::{Connection, JobState, SessionOpts, Severity, TableRef};

// =============================================================================================
// Request
// =============================================================================================

/// Database family of a copy. Source and target must be the same family.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum CopyFamily {
    Postgres,
    Mysql,
    Sqlite,
}

impl CopyFamily {
    /// The family of a driver id (`postgres`, `mysql`, `sqlite`), `None` for every other driver.
    pub fn of_driver(driver: &str) -> Option<CopyFamily> {
        match driver {
            "postgres" => Some(CopyFamily::Postgres),
            "mysql" => Some(CopyFamily::Mysql),
            "sqlite" => Some(CopyFamily::Sqlite),
            _ => None,
        }
    }
}

/// One side of a copy: a saved connection plus the database (and PG schema) to use.
#[derive(Debug, Clone, Default, Serialize, Deserialize, TS, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CopyEndpoint {
    pub conn_id: String,
    /// MySQL database; PG database (the session opens it: `conn.database` overridden); SQLite `main`.
    pub database: String,
    /// PG only. Source: restrict to one schema (absent = every user schema). Target: only with a
    /// source schema; that schema is written into this one (a rename into another database).
    #[serde(default)]
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,
    /// "Ask every time" connections; never stored, never in history.
    #[serde(default)]
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
}

/// What to copy. TS `{kind:"all"} | {kind:"tables", tables: TableRef[]}`.
/// `TableRef.schema`: PG schema (required), absent for MySQL/SQLite; `TableRef.database` is ignored.
#[derive(Debug, Clone, Serialize, Deserialize, TS, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CopySelection {
    All,
    Tables { tables: Vec<TableRef> },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS, Default)]
#[serde(rename_all = "snake_case")]
pub enum CopyContent {
    #[default]
    Both,
    SchemaOnly,
    DataOnly,
}

/// What happens to a table that already exists in the target. Default **skip** (user decision).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS, Default)]
#[serde(rename_all = "snake_case")]
pub enum ExistingTablePolicy {
    #[default]
    Skip,
    Truncate,
    DropRecreate,
    Append,
}

/// Per existing table policy (section 3.7).
#[derive(Debug, Clone, Serialize, Deserialize, TS, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ItemPolicy {
    pub item_id: String,
    pub policy: ExistingTablePolicy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS, Default)]
#[serde(rename_all = "snake_case")]
pub enum VerifyMode {
    None,
    #[default]
    RowCount,
    Checksum,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS, Default)]
#[serde(rename_all = "snake_case")]
pub enum ZeroDatePolicy {
    #[default]
    Auto,
    Null,
    Keep,
    Min,
}

/// What a MySQL ENUM error value (`''`, index 0, stored by a non-strict source for invalid
/// input) becomes: the strict target refuses it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS, Default)]
#[serde(rename_all = "snake_case")]
pub enum EnumErrorPolicy {
    /// The table fails with `ENUM_ERROR_VALUE` (default).
    #[default]
    Fail,
    /// NULL, counted per column as the value fix `ENUM_ERROR_NULL` (a NOT NULL column still
    /// fails).
    Null,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS, Default)]
#[serde(rename_all = "snake_case")]
pub enum UnmappablePolicy {
    /// Replace and count; the table ends "done with problems".
    #[default]
    Replace,
    Fail,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS, Default)]
#[serde(rename_all = "snake_case")]
pub enum MysqlEngineChoice {
    #[default]
    Auto,
    Keep,
    Innodb,
    Myisam,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum ColumnTreatment {
    Binary,
    Text,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum PgSourceEncoding {
    EucKr,
    Latin1,
    Utf8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS, Default)]
#[serde(rename_all = "snake_case")]
pub enum NameCase {
    #[default]
    Keep,
    Lower,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum RenameKind {
    Schema,
    Table,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct NameMapping {
    pub kind: RenameKind,
    /// `Table` in PG: its source schema.
    #[serde(default)]
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,
    pub from: String,
    pub to: String,
}

/// Which non-table objects are copied. Defaults: all true except `owners` and `grants`.
#[derive(Debug, Clone, Serialize, Deserialize, TS, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct CopyObjectFlags {
    pub views: bool,
    /// PG functions + procedures.
    pub routines: bool,
    pub triggers: bool,
    /// Only the `SequenceValue` / `AutoIncrement` items (3.7).
    pub sequences: bool,
    /// PG.
    pub types: bool,
    /// PG.
    pub extensions: bool,
    pub comments: bool,
    /// false, PG.
    pub owners: bool,
    /// false, PG.
    pub grants: bool,
    pub analyze: bool,
    /// PG.
    pub refresh_matviews: bool,
}

impl Default for CopyObjectFlags {
    fn default() -> Self {
        CopyObjectFlags {
            views: true,
            routines: true,
            triggers: true,
            sequences: true,
            types: true,
            extensions: true,
            comments: true,
            owners: false,
            grants: false,
            analyze: true,
            refresh_matviews: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, TS, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct MysqlTableOverride {
    /// Source table.
    pub table: String,
    #[serde(default)]
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub charset: Option<String>,
    #[serde(default)]
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub collation: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct MysqlColumnOverride {
    pub table: String,
    pub column: String,
    pub treat_as: ColumnTreatment,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct MysqlCopyOptions {
    /// Absent = version default (3.3).
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub charset: Option<String>,
    /// Absent = the charset's default on that target; `"legacy"` = PAD SPACE preset (3.3).
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub collation: Option<String>,
    pub engine: MysqlEngineChoice,
    pub zero_dates: ZeroDatePolicy,
    /// `"+09:00"`; used only by the DATE_FORMAT fallback of 3.2.
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub time_zone: Option<String>,
    /// true (3.3 row limits).
    pub fix_row_size: bool,
    /// false (3.3 views and triggers).
    pub keep_definer: bool,
    pub table_overrides: Vec<MysqlTableOverride>,
    pub columns: Vec<MysqlColumnOverride>,
    /// ENUM error values (`''` in a column without a `''` member). Absent = `fail`.
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enum_error: Option<EnumErrorPolicy>,
}

impl Default for MysqlCopyOptions {
    fn default() -> Self {
        MysqlCopyOptions {
            charset: None,
            collation: None,
            engine: MysqlEngineChoice::Auto,
            zero_dates: ZeroDatePolicy::Auto,
            time_zone: None,
            fix_row_size: true,
            keep_definer: false,
            table_overrides: Vec::new(),
            columns: Vec::new(),
            enum_error: None,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, TS, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct PgCopyOptions {
    /// false: missing collation = item blocker; true: database default + warning.
    pub collation_fallback: bool,
    /// SQL_ASCII sources only (3.5).
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_encoding: Option<PgSourceEncoding>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct CopyOptions {
    pub content: CopyContent,
    pub existing: ExistingTablePolicy,
    pub existing_overrides: Vec<ItemPolicy>,
    pub objects: CopyObjectFlags,
    pub name_case: NameCase,
    pub rename: Vec<NameMapping>,
    /// Plan item ids left out (their dependants too); action `excluded`.
    pub exclude: Vec<String>,
    /// true.
    pub indexes_after_data: bool,
    /// 1, clamped 1..=4; SQLite targets: 1.
    pub table_concurrency: u8,
    /// Writer chunk rows, default 50_000, clamped 1..=65_536.
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub batch_rows: Option<u32>,
    /// Writer piece MiB, default 8, clamped 1..=16 (effective size 4.1).
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub batch_mb: Option<u32>,
    /// Default 5, clamped 1..=600 (4.1).
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_lock_timeout_s: Option<u32>,
    pub verify: VerifyMode,
    /// false; true: exact counts of skip_existing tables on both sides (5.6).
    pub verify_skipped: bool,
    pub on_unmappable: UnmappablePolicy,
    /// Start-time field: never changes a plan, not hashed.
    pub dry_run: bool,
    /// Start-time field; must equal the target connection's name when tagged production.
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confirm_production: Option<String>,
    /// Merged like export (request > connection > recommended); copy default: no statement timeout.
    pub source_session: SessionOpts,
    pub mysql: MysqlCopyOptions,
    pub pg: PgCopyOptions,
}

impl Default for CopyOptions {
    fn default() -> Self {
        CopyOptions {
            content: CopyContent::Both,
            existing: ExistingTablePolicy::Skip,
            existing_overrides: Vec::new(),
            objects: CopyObjectFlags::default(),
            name_case: NameCase::Keep,
            rename: Vec::new(),
            exclude: Vec::new(),
            indexes_after_data: true,
            table_concurrency: 1,
            batch_rows: None,
            batch_mb: None,
            target_lock_timeout_s: None,
            verify: VerifyMode::RowCount,
            verify_skipped: false,
            on_unmappable: UnmappablePolicy::Replace,
            dry_run: false,
            confirm_production: None,
            source_session: SessionOpts::default(),
            mysql: MysqlCopyOptions::default(),
            pg: PgCopyOptions::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, TS, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CopyRequest {
    pub source: CopyEndpoint,
    pub target: CopyEndpoint,
    pub selection: CopySelection,
    #[serde(default)]
    pub options: CopyOptions,
    #[serde(default)]
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, TS, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CopyResumeRequest {
    pub run_id: String,
    /// Ack keys.
    #[serde(default)]
    pub acknowledged: Vec<String>,
    #[serde(default)]
    pub partial: ResumePartial,
    #[serde(default)]
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_password: Option<String>,
    #[serde(default)]
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_password: Option<String>,
    #[serde(default)]
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confirm_production: Option<String>,
}

// =============================================================================================
// Plan
// =============================================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum CopyObjectKind {
    Database,
    Schema,
    Extension,
    Collation,
    Type,
    Domain,
    Sequence,
    Function,
    Table,
    Partition,
    View,
    MaterializedView,
    Index,
    Constraint,
    ForeignKey,
    Trigger,
    Policy,
    Comment,
    Owner,
    Grant,
    TruncateGroup,
    SequenceValue,
    AutoIncrement,
    Refresh,
    Analyze,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum PlanAction {
    Create,
    SkipExisting,
    Truncate,
    DropRecreate,
    Append,
    Run,
    Unsupported,
    Excluded,
    ResumeDone,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum CopyPhase {
    Plan,
    PreData,
    Data,
    PostData,
    Finalize,
    Verify,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum LoadMethod {
    PgCopyCsv,
    MysqlLoadData,
    MysqlInsert,
    SqliteInsert,
}

/// A plan issue. `message` is English; the UI titles known codes through `copy.issue.<CODE>`.
#[derive(Debug, Clone, Serialize, Deserialize, TS, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CopyIssue {
    pub code: String,
    pub severity: Severity,
    pub message: String,
    #[serde(default)]
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(default)]
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recommendation: Option<String>,
    #[serde(default)]
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub item_id: Option<String>,
}

/// One acknowledgement the start needs (section 2 "Acknowledgement").
#[derive(Debug, Clone, Serialize, Deserialize, TS, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AckNeed {
    pub code: String,
    pub key: String,
    pub severity: Severity,
    #[serde(default)]
    pub items: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, TS, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CopyServerFacts {
    pub driver: String,
    pub version: String,
    pub database: String,
    /// Non-secret: `"host:port"` (+ `" via ssh host"`) or the SQLite path.
    pub endpoint: String,
    #[serde(default)]
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub charset: Option<String>,
    #[serde(default)]
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub collation: Option<String>,
    #[serde(default)]
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    #[serde(default)]
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CopyEffective {
    #[serde(default)]
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub charset: Option<String>,
    #[serde(default)]
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub collation: Option<String>,
    #[serde(default)]
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub engine: Option<String>,
    /// Never `Auto`.
    #[serde(default)]
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub zero_dates: Option<ZeroDatePolicy>,
    #[serde(default)]
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub time_zone: Option<String>,
    pub load_method: LoadMethod,
    pub table_concurrency: u8,
    /// MySQL (4.3).
    #[serde(default)]
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chunk_bytes: Option<u64>,
    pub consistent_snapshot: bool,
    pub source_sessions: u8,
    pub target_sessions: u8,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PlanItem {
    /// Stable across re-plans; checkpoint key; `<kind>:<path>` (section 2 "Plan item ids").
    pub id: String,
    pub kind: CopyObjectKind,
    /// index/constraint/trigger/... -> its table item id.
    #[serde(default)]
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    #[serde(default)]
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,
    pub name: String,
    #[serde(default)]
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_schema: Option<String>,
    pub target_name: String,
    pub action: PlanAction,
    pub phase: CopyPhase,
    /// Execution order inside the phase.
    pub order: u32,
    pub exists_in_target: bool,
    /// Exact probe (3.7); absent when not probed.
    #[serde(default)]
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_has_rows: Option<bool>,
    #[serde(default)]
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_est_rows: Option<u64>,
    pub has_data: bool,
    #[serde(default)]
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub est_rows: Option<u64>,
    #[serde(default)]
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub est_bytes: Option<u64>,
    /// FEDERATED CONNECTION values redacted.
    #[serde(default)]
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_ddl: Option<String>,
    /// Every statement BulkDB runs for this item; empty for skip/unsupported/excluded. Data
    /// items: the load template, data elided. SequenceValue / AutoIncrement: a template with
    /// `<source value>` / `<target value>` placeholders, filled at finalize (5.1).
    #[serde(default)]
    pub target_sql: Vec<String>,
    #[serde(default)]
    pub warnings: Vec<CopyIssue>,
    #[serde(default)]
    pub depends_on: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, TS, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CopyTotals {
    pub tables: u32,
    pub objects: u32,
    pub create: u32,
    pub skip: u32,
    pub truncate: u32,
    pub drop_recreate: u32,
    pub append: u32,
    pub unsupported: u32,
    pub excluded: u32,
    #[serde(default)]
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub est_rows: Option<u64>,
    #[serde(default)]
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub est_bytes: Option<u64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, TS, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SessionSql {
    #[serde(default)]
    pub source: Vec<String>,
    #[serde(default)]
    pub target: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CopyPlan {
    /// Section 2 "Plan validity".
    pub plan_id: String,
    pub family: CopyFamily,
    pub source: CopyServerFacts,
    pub target: CopyServerFacts,
    pub effective: CopyEffective,
    pub items: Vec<PlanItem>,
    pub warnings: Vec<CopyIssue>,
    pub blockers: Vec<CopyIssue>,
    #[serde(default)]
    pub acks: Vec<AckNeed>,
    pub session_sql: SessionSql,
    pub totals: CopyTotals,
    #[serde(default)]
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resume_of: Option<String>,
    pub elapsed_ms: u64,
}

// =============================================================================================
// Progress / result
// =============================================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum CopyItemState {
    Pending,
    Creating,
    Loading,
    Indexing,
    Verifying,
    Done,
    Skipped,
    Failed,
    Cancelled,
    Planned,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum VerifyOutcome {
    NotRun,
    Match,
    Mismatch,
    /// Counts equal, values changed (5.6).
    Lossy,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS, Default)]
#[serde(rename_all = "snake_case")]
pub enum ResumePartial {
    #[default]
    Fail,
    Truncate,
    Skip,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ValueFix {
    pub column: String,
    /// `ZERO_DATE_NULL`, `ZERO_DATE_MIN`, `INVALID_DATE`, ...
    pub code: String,
    pub count: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CopyTableProgress {
    pub seq: u64,
    pub item_id: String,
    pub state: CopyItemState,
    pub rows_read: u64,
    pub rows_written: u64,
    pub bytes: u64,
    #[serde(default)]
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub est_rows: Option<u64>,
    pub rate_rows_per_sec: f64,
    #[serde(default)]
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub elapsed_ms: Option<u64>,
    #[serde(default)]
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub method: Option<LoadMethod>,
    #[serde(default)]
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorPayload>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CopyProgress {
    pub job_id: String,
    pub run_id: String,
    /// Monotonic per job.
    pub seq: u64,
    pub phase: CopyPhase,
    pub items_total: u32,
    pub items_done: u32,
    pub items_failed: u32,
    pub items_skipped: u32,
    pub tables_total: u32,
    pub tables_done: u32,
    #[serde(default)]
    pub tables: Vec<CopyTableProgress>,
    /// true: `tables` is the complete list; false (event delta): only entries changed since the
    /// last event.
    pub full: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CopyTableResult {
    pub item_id: String,
    pub target_name: String,
    pub state: CopyItemState,
    pub rows_read: u64,
    pub rows_written: u64,
    #[serde(default)]
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_count: Option<u64>,
    pub verify: VerifyOutcome,
    #[serde(default)]
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checksum_source: Option<String>,
    #[serde(default)]
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checksum_target: Option<String>,
    pub unmappable_chars: u64,
    pub data_warnings: u64,
    /// <= 10, redacted (5.2).
    #[serde(default)]
    pub warning_samples: Vec<String>,
    #[serde(default)]
    pub value_fixes: Vec<ValueFix>,
    /// Codes, 5.6.
    #[serde(default)]
    pub problems: Vec<String>,
    pub duration_ms: u64,
    #[serde(default)]
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub method: Option<LoadMethod>,
    #[serde(default)]
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorPayload>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CopyItemResult {
    pub item_id: String,
    pub state: CopyItemState,
    #[serde(default)]
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorPayload>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum LeftBehindKind {
    CreatedTable,
    PartialRows,
    EmptiedTable,
    MissingPostData,
    SequenceNotSet,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct LeftBehind {
    pub item_id: String,
    pub kind: LeftBehindKind,
    #[serde(default)]
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rows: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CopyResult {
    pub run_id: String,
    pub dry_run: bool,
    /// As run.
    pub existing: ExistingTablePolicy,
    pub tables: Vec<CopyTableResult>,
    pub objects: Vec<CopyItemResult>,
    pub warnings: Vec<CopyIssue>,
    pub left_behind: Vec<LeftBehind>,
    pub verified: bool,
    pub problems: u32,
    pub skipped_existing: u32,
    pub resumable: bool,
    pub duration_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CopyStatus {
    pub job_id: String,
    pub run_id: String,
    pub state: JobState,
    /// Absent until the job has planned; `progress.tables` is empty and phase is `plan` until then.
    #[serde(default)]
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan: Option<CopyPlan>,
    /// Full.
    pub progress: CopyProgress,
    #[serde(default)]
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<CopyResult>,
}

/// `JobSummary.copy` for copy jobs.
#[derive(Debug, Clone, Serialize, Deserialize, TS, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CopyJobInfo {
    pub run_id: String,
    pub target_conn_id: String,
    pub target_conn_name: String,
    pub source_database: String,
    pub target_database: String,
    pub phase: CopyPhase,
    pub tables_total: u32,
    pub tables_done: u32,
    pub tables_failed: u32,
    pub dry_run: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CopyRunInfo {
    pub run_id: String,
    pub label: String,
    pub source_conn_id: String,
    pub target_conn_id: String,
    pub source_database: String,
    pub target_database: String,
    pub started_at: String,
    #[serde(default)]
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<String>,
    pub state: JobState,
    pub last_job_id: String,
    pub tables_total: u32,
    pub tables_done: u32,
    pub tables_failed: u32,
    pub resumable: bool,
    pub dry_run: bool,
    pub in_use: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CopyRunDetail {
    pub info: CopyRunInfo,
    /// No passwords, no start-time fields.
    pub request: CopyRequest,
    #[serde(default)]
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<CopyResult>,
}

// =============================================================================================
// Internal contract types (not IPC). copy-schema and copy-data meet here.
// =============================================================================================

/// The plan plus what the data path needs per table item.
#[derive(Debug, Clone, PartialEq)]
pub struct PlannedCopy {
    pub plan: CopyPlan,
    /// By item id.
    pub tables: BTreeMap<String, TableDataSpec>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TableDataSpec {
    pub item_id: String,
    /// One SELECT, BulkDB-generated (3.2 / 4.2).
    pub source_sql: String,
    /// Target-side read-back for checksum: target names, same column order and casts (5.6).
    pub verify_sql: String,
    /// PG->PG.
    pub raw_csv: bool,
    /// PG SQL_ASCII source (3.5).
    pub transcode: Option<PgSourceEncoding>,
    pub target: TargetTable,
    pub columns: Vec<DataColumn>,
    pub est_rows: Option<u64>,
    /// 4.0 source: first TIMESTAMP column is auto-set (3.3).
    pub first_timestamp_auto: bool,
    /// MySQL: load with foreign_key_checks = 1 (3.7).
    pub fk_checks: bool,
}

/// A target table, unquoted names.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TargetTable {
    /// MySQL: database; PG: schema; SQLite: None.
    pub schema: Option<String>,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DataColumn {
    pub source: String,
    pub target: String,
    pub value: ValueKind,
    pub read: SourceRead,
    pub zero_date: ZeroDateAction,
    pub invalid_date: ZeroDateAction,
    pub binary_attr: bool,
    pub max_bytes: Option<u64>,
}

/// How a source column is read (3.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceRead {
    Plain,
    CastChar,
    Concat,
    UnixTimestamp,
    DateFormat,
    YearNumber,
    FloatWiden,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ValueKind {
    Text,
    Binary,
    Bit,
    Integer,
    Decimal,
    Float,
    Bool,
    Date,
    DateTime,
    Timestamp,
    Time,
    Year,
    Json,
    Raw,
}

/// Min: DATE/DATETIME '1000-01-01[ 00:00:00]', TIMESTAMP epoch 1, YEAR 1901.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ZeroDateAction {
    NotTemporal,
    Null,
    Keep,
    Min,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerIdentity {
    pub key: String,
    pub system_identifier: Option<String>,
    pub server_uuid: Option<String>,
    pub replica_of: Option<String>,
    pub group_name: Option<String>,
    pub in_recovery: bool,
    pub read_only: bool,
    /// MySQL < 5.6 hostname:port match only.
    pub weak: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TargetFacts {
    pub family: CopyFamily,
    /// `mysql | mariadb | postgres | sqlite`.
    pub flavor: String,
    pub version: String,
    pub major: u16,
    pub minor: u16,
    pub patch: u16,
    pub database: String,
    pub user: Option<String>,
    pub vars: BTreeMap<String, String>,
    pub identity: ServerIdentity,
    pub endpoint: String,
}

impl TargetFacts {
    /// Facts with the version numbers parsed from `version` (`"8.0.36"`, `"5.7.44-ndb-7.6.29"`,
    /// `"16.2"`); everything else empty. Convenient for tests and test doubles.
    pub fn new(family: CopyFamily, flavor: &str, version: &str, database: &str) -> TargetFacts {
        let mut nums = version.split(|c: char| !c.is_ascii_digit()).filter(|s| !s.is_empty()).map(|s| s.parse::<u16>().unwrap_or(0));
        let (major, minor, patch) = (nums.next().unwrap_or(0), nums.next().unwrap_or(0), nums.next().unwrap_or(0));
        TargetFacts {
            family,
            flavor: flavor.to_string(),
            version: version.to_string(),
            major,
            minor,
            patch,
            database: database.to_string(),
            user: None,
            vars: BTreeMap::new(),
            identity: ServerIdentity::default(),
            endpoint: String::new(),
        }
    }

    /// `(major, minor, patch) >= (a, b, c)`.
    pub fn at_least(&self, a: u16, b: u16, c: u16) -> bool {
        (self.major, self.minor, self.patch) >= (a, b, c)
    }
}

/// Rows of a catalog / meta query, text values (NULL = `None`). Never data rows of a table
/// (the charset sample of 3.2 stays inside the planner).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MetaTable {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Option<String>>>,
}

impl MetaTable {
    /// Build from string slices (tests and canned answers).
    pub fn from_rows(columns: &[&str], rows: &[&[Option<&str>]]) -> MetaTable {
        MetaTable { columns: columns.iter().map(|c| c.to_string()).collect(), rows: rows.iter().map(|r| r.iter().map(|v| v.map(str::to_string)).collect()).collect() }
    }

    /// Value of column `name` (ASCII case-insensitive) in row `row`; `None` for NULL, a missing
    /// column or a missing row.
    pub fn get(&self, row: usize, name: &str) -> Option<&str> {
        let col = self.columns.iter().position(|c| c.eq_ignore_ascii_case(name))?;
        self.rows.get(row)?.get(col)?.as_deref()
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }
}

/// Write-ahead checkpoint states of `copy_items.state` (5.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckpointState {
    Pending,
    Creating,
    Created,
    Loading,
    Loaded,
    Done,
    Failed,
    Skipped,
}

/// What a resumed plan starts from (5.5).
#[derive(Debug, Clone, PartialEq)]
pub struct ResumeState {
    pub run_id: String,
    /// item id -> (state, created_by_run, rows_written).
    pub items: BTreeMap<String, (CheckpointState, bool, u64)>,
    pub partial: ResumePartial,
}

/// An item travelling reader -> writer with its share of the 16 MiB in-flight budget (4.1).
/// The writer drops `permit` after encoding or forwarding the item.
#[derive(Debug)]
pub struct Budgeted<T> {
    pub item: T,
    pub rows: u64,
    pub permit: tokio::sync::OwnedSemaphorePermit,
}

/// Input of [`TargetSession::load`]; channels of depth 1.
#[derive(Debug)]
pub enum LoadInput {
    RawCsv(mpsc::Receiver<Budgeted<Bytes>>),
    Arrow(mpsc::Receiver<Budgeted<RecordBatch>>),
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct LoadOutcome {
    pub rows: u64,
    pub warnings: u64,
    pub skipped: u64,
    pub samples: Vec<String>,
    pub unmappable: u64,
    pub value_fixes: Vec<ValueFix>,
    pub method: Option<LoadMethod>,
    /// The source-side checksum (5.6) computed by the writer while it encodes, when
    /// `WriterConfig::source_checksum` is set (MySQL: the writer already encodes every batch, so
    /// hashing it there avoids a second encoder in the reader). `None` otherwise.
    pub source_checksum: Option<TableChecksum>,
}

/// Writer settings of one job, handed to every write session before it loads (copy-data;
/// internal, not IPC). Sessions that do not need them ignore [`TargetSession::configure`].
#[derive(Debug, Clone, PartialEq)]
pub struct WriterConfig {
    pub job_id: String,
    /// 0 = the DDL session, 1.. = data workers.
    pub session_no: u32,
    /// Writer chunk rows (`batch_rows`, default 50,000; SQLite commits, INSERT statements).
    pub batch_rows: u32,
    /// Writer piece bytes (`min(batch_mb, 8 / c)` MiB).
    pub piece_bytes: usize,
    /// MySQL chunk limits (4.3).
    pub chunk_bytes: u64,
    pub chunk_rows: u64,
    /// `target_lock_timeout_s`.
    pub lock_timeout_s: u32,
    /// `on_unmappable: fail`.
    pub fail_on_unmappable: bool,
    /// MySQL: the job's effective target charset (`CopyEffective.charset`). A utf8 / utf8mb3
    /// charset limits text to 3-byte UTF-8: 4-byte characters become `?` and are counted
    /// (3.3 `UTF8MB3_ONLY`), on every target version.
    pub charset: Option<String>,
    /// MySQL: per-table charsets of `mysql.table_overrides`, by data item id.
    pub table_charsets: BTreeMap<String, String>,
    /// MySQL: target ENUM columns without a `''` member, by data item id. Their `''` is the
    /// error value (index 0) a non-strict source stored for invalid input, which the strict
    /// writer cannot load.
    pub enum_error_columns: BTreeMap<String, Vec<String>>,
    /// `mysql.enum_error: null`: such values load as NULL (counted as `ENUM_ERROR_NULL`)
    /// instead of failing the table with `ENUM_ERROR_VALUE`.
    pub enum_error_null: bool,
    /// MySQL only: the writer computes the source-side checksum while it encodes each batch
    /// (5.6), reported back in `LoadOutcome::source_checksum`, so the reader needs no second
    /// encoder. Set when the job verifies by checksum.
    pub source_checksum: bool,
}

impl WriterConfig {
    /// The effective target charset of the table loaded by `item_id`.
    pub fn charset_for(&self, item_id: &str) -> Option<&str> {
        self.table_charsets.get(item_id).or(self.charset.as_ref()).map(String::as_str)
    }

    /// The ENUM columns of `item_id` whose `''` is the error value.
    pub fn enum_error_columns_for(&self, item_id: &str) -> &[String] {
        self.enum_error_columns.get(item_id).map(Vec::as_slice).unwrap_or(&[])
    }
}

/// One session on the copy target (owner copy-data; section 2 "Traits and seams").
#[async_trait]
pub trait TargetSession: Send + Sync {
    fn facts(&self) -> &TargetFacts;
    /// Reads only (5.2).
    async fn query_meta(&self, sql: &str) -> Result<MetaTable>;
    /// One generated statement; affected rows.
    async fn execute(&self, sql: &str) -> Result<u64>;
    async fn load(&self, spec: &TableDataSpec, input: LoadInput, cancel: CancellationToken) -> Result<LoadOutcome>;
    async fn count_rows(&self, t: &TargetTable) -> Result<u64>;
    /// PG cancel request / MySQL KILL QUERY from a side connection / SQLite interrupt.
    async fn cancel_running(&self);

    /// Job settings for the writer (chunk sizes, lock timeout). Added by copy-data; the default
    /// ignores them, so test doubles need not implement it.
    fn configure(&self, _cfg: &WriterConfig) {}

    /// After a `load` that failed or was cancelled (also when its future was dropped): what it
    /// had **committed** before that, so `left_behind.partial_rows` reports rows that stay in the
    /// target, never rows that were only sent (MySQL: rows of whole committed chunks, plus the
    /// warnings and samples read). Taken once. Default `None`: nothing committed (PG and SQLite
    /// roll the open transaction back, and resume reloads the table).
    fn take_failed_outcome(&self) -> Option<LoadOutcome> {
        None
    }

    /// While a `load` runs: the rows it has **committed** so far (MySQL whole chunks of a
    /// transactional table, SQLite whole batches, PostgreSQL 0 until its single commit), what
    /// the periodic `loading` checkpoint stores (5.5). `None`: every row sent is final (a
    /// MyISAM table) or the session cannot tell; the executor then stores the rows sent.
    fn committed_rows(&self) -> Option<u64> {
        None
    }

    /// After a `load` ended (failed, cancelled, or its future dropped): whether a chunk
    /// transaction was still open, so the server may be rolling it back (MySQL `SERVER_ROLLBACK_
    /// PENDING`, 5.4). `false` by default and whenever the last chunk committed cleanly.
    fn server_rollback_pending(&self) -> bool {
        false
    }
}

/// Opens target sessions. `BulkDbOptions.copy_target_opener` substitutes a test double.
#[async_trait]
pub trait TargetOpener: Send + Sync {
    async fn open(&self, conn: &Connection, ep: &CopyEndpoint, app_name: &str) -> Result<Box<dyn TargetSession>>;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The UI starts every wizard from `components/copy/copy-options-default.json`
    /// (`DEFAULT_COPY_OPTIONS`); it must equal the serde defaults exactly.
    #[test]
    fn copy_options_defaults_equal_the_ui_json() {
        let json = include_str!("../../../../apps/desktop/src/components/copy/copy-options-default.json");
        let ui: serde_json::Value = serde_json::from_str(json).unwrap();
        assert_eq!(serde_json::to_value(CopyOptions::default()).unwrap(), ui);
        let parsed: CopyOptions = serde_json::from_value(ui).unwrap();
        assert_eq!(parsed, CopyOptions::default());
        // Partial objects fall back to the same defaults (struct-level `#[serde(default)]`).
        let partial: CopyOptions = serde_json::from_str(r#"{"existing":"truncate","mysql":{"engine":"keep"}}"#).unwrap();
        assert_eq!(partial.existing, ExistingTablePolicy::Truncate);
        assert!(partial.mysql.fix_row_size && partial.objects.views && !partial.objects.owners);
        assert_eq!(partial.mysql.engine, MysqlEngineChoice::Keep);
    }

    #[test]
    fn selection_and_request_wire_shape() {
        let all: CopySelection = serde_json::from_str(r#"{"kind":"all"}"#).unwrap();
        assert_eq!(all, CopySelection::All);
        let t: CopySelection = serde_json::from_str(r#"{"kind":"tables","tables":[{"schema":"app","name":"orders"}]}"#).unwrap();
        assert_eq!(t, CopySelection::Tables { tables: vec![TableRef::new(Some("app"), "orders")] });
        let req: CopyRequest =
            serde_json::from_str(r#"{"source":{"connId":"a","database":"shop"},"target":{"connId":"b","database":"shop2"},"selection":{"kind":"all"}}"#).unwrap();
        assert_eq!(req.options, CopyOptions::default());
        let v = serde_json::to_value(&req).unwrap();
        assert!(v["source"].get("password").is_none() && v.get("name").is_none(), "absent optionals are not serialised: {v}");
        assert_eq!(serde_json::to_value(PgSourceEncoding::EucKr).unwrap(), "euc_kr");
        assert_eq!(serde_json::to_value(ExistingTablePolicy::DropRecreate).unwrap(), "drop_recreate");
    }

    #[test]
    fn meta_table_get_by_name() {
        let m = MetaTable::from_rows(&["Name", "Engine"], &[&[Some("t"), None]]);
        assert_eq!(m.get(0, "name"), Some("t"));
        assert_eq!(m.get(0, "Engine"), None);
        assert_eq!(m.get(1, "Name"), None);
        assert_eq!(m.get(0, "missing"), None);
        let f = TargetFacts::new(CopyFamily::Mysql, "mysql", "5.7.44-ndb-7.6.29", "db");
        assert_eq!((f.major, f.minor, f.patch), (5, 7, 44));
        assert!(f.at_least(5, 7, 0) && !f.at_least(8, 0, 0));
    }
}
