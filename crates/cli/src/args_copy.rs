//! `bulk-cli copy` flags (docs/db-copy.md 6.3): definitions and value parsers only. The
//! command lives in `cmd_copy`. Options start from `CopyOptions::default()`; `--help` shows
//! each default. Owner: copy-data.

use std::path::PathBuf;

use bulk_core::types::{
    ColumnTreatment, CopyContent, ExistingTablePolicy, MysqlEngineChoice, PgSourceEncoding, ResumePartial, UnmappablePolicy, VerifyMode, ZeroDatePolicy,
};
use clap::{Args, Subcommand, ValueEnum};

use crate::output::parse_duration_ms;

pub const COPY_AFTER_HELP: &str = "\
DB copy writes only into a target connection marked write-allowed
(`bulk-cli conn edit <TARGET> --write-allowed true`). Source and target must be the same family
(postgres, mysql, sqlite). Saved connections only, by name or id.

MySQL 6.x and 7.x never shipped as GA (6.0 was a cancelled alpha, 7.x is NDB Cluster
numbering); supported MySQL targets are 5.0-5.7, 8.0, 8.4 and 9.x.

Exit codes:
  0 done (no problems; verified when --verify is not none)
  1 failed before a result existed (config, connect, internal error)
  2 cancelled (Ctrl+C)
  3 rejected (blockers) or --expect-plan mismatch
  4 acknowledgement or confirmation needed
  5 the job failed or ended with problems (a stored, resumable result exists)";

#[derive(Args, Debug)]
pub struct CopyArgs {
    #[command(subcommand)]
    pub cmd: CopyCmd,
}

#[derive(Subcommand, Debug)]
#[allow(clippy::large_enum_variant)]
pub enum CopyCmd {
    /// Plan a copy and print it (plan id, facts, items, actions, issues, acknowledgements).
    Plan(CopyPlanArgs),
    /// Plan, then run the copy (asks for missing acknowledgements on a tty).
    Run(CopyRunArgs),
    /// Resume a failed, cancelled or interrupted run.
    Resume(CopyResumeArgs),
    /// List copy runs from the history database, newest first.
    Runs(CopyRunsArgs),
    /// Show one run (request and stored result).
    Status(CopyRunSel),
    /// Delete a run and its checkpoints (refused while the run is in use).
    Forget(CopyRunSel),
}

/// `skip|truncate|drop-recreate|append`.
#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExistingArg {
    Skip,
    Truncate,
    DropRecreate,
    Append,
}

impl From<ExistingArg> for ExistingTablePolicy {
    fn from(v: ExistingArg) -> Self {
        match v {
            ExistingArg::Skip => ExistingTablePolicy::Skip,
            ExistingArg::Truncate => ExistingTablePolicy::Truncate,
            ExistingArg::DropRecreate => ExistingTablePolicy::DropRecreate,
            ExistingArg::Append => ExistingTablePolicy::Append,
        }
    }
}

#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContentArg {
    Both,
    Schema,
    Data,
}

impl From<ContentArg> for CopyContent {
    fn from(v: ContentArg) -> Self {
        match v {
            ContentArg::Both => CopyContent::Both,
            ContentArg::Schema => CopyContent::SchemaOnly,
            ContentArg::Data => CopyContent::DataOnly,
        }
    }
}

#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum EngineArg {
    Auto,
    Keep,
    Innodb,
    Myisam,
}

impl From<EngineArg> for MysqlEngineChoice {
    fn from(v: EngineArg) -> Self {
        match v {
            EngineArg::Auto => MysqlEngineChoice::Auto,
            EngineArg::Keep => MysqlEngineChoice::Keep,
            EngineArg::Innodb => MysqlEngineChoice::Innodb,
            EngineArg::Myisam => MysqlEngineChoice::Myisam,
        }
    }
}

#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum EnumErrorArg {
    /// The table fails (ENUM_ERROR_VALUE).
    Fail,
    /// Load NULL, counted per column (ENUM_ERROR_NULL).
    Null,
}

impl From<EnumErrorArg> for bulk_core::types::EnumErrorPolicy {
    fn from(v: EnumErrorArg) -> Self {
        match v {
            EnumErrorArg::Fail => bulk_core::types::EnumErrorPolicy::Fail,
            EnumErrorArg::Null => bulk_core::types::EnumErrorPolicy::Null,
        }
    }
}

#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum ZeroDatesArg {
    Auto,
    Null,
    Keep,
    Min,
}

impl From<ZeroDatesArg> for ZeroDatePolicy {
    fn from(v: ZeroDatesArg) -> Self {
        match v {
            ZeroDatesArg::Auto => ZeroDatePolicy::Auto,
            ZeroDatesArg::Null => ZeroDatePolicy::Null,
            ZeroDatesArg::Keep => ZeroDatePolicy::Keep,
            ZeroDatesArg::Min => ZeroDatePolicy::Min,
        }
    }
}

#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum VerifyArg {
    None,
    Count,
    Checksum,
}

impl From<VerifyArg> for VerifyMode {
    fn from(v: VerifyArg) -> Self {
        match v {
            VerifyArg::None => VerifyMode::None,
            VerifyArg::Count => VerifyMode::RowCount,
            VerifyArg::Checksum => VerifyMode::Checksum,
        }
    }
}

#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnmappableArg {
    Replace,
    Fail,
}

impl From<UnmappableArg> for UnmappablePolicy {
    fn from(v: UnmappableArg) -> Self {
        match v {
            UnmappableArg::Replace => UnmappablePolicy::Replace,
            UnmappableArg::Fail => UnmappablePolicy::Fail,
        }
    }
}

#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum PgEncodingArg {
    #[value(name = "euc-kr")]
    EucKr,
    Latin1,
    Utf8,
}

impl From<PgEncodingArg> for PgSourceEncoding {
    fn from(v: PgEncodingArg) -> Self {
        match v {
            PgEncodingArg::EucKr => PgSourceEncoding::EucKr,
            PgEncodingArg::Latin1 => PgSourceEncoding::Latin1,
            PgEncodingArg::Utf8 => PgSourceEncoding::Utf8,
        }
    }
}

#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum PartialArg {
    Fail,
    Truncate,
    Skip,
}

impl From<PartialArg> for ResumePartial {
    fn from(v: PartialArg) -> Self {
        match v {
            PartialArg::Fail => ResumePartial::Fail,
            PartialArg::Truncate => ResumePartial::Truncate,
            PartialArg::Skip => ResumePartial::Skip,
        }
    }
}

/// `t.c=binary|text` of `--column-as`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnAs {
    pub table: String,
    pub column: String,
    pub treat_as: ColumnTreatment,
}

/// `KEY=VALUE` (split at the first `=`; both sides non-empty).
pub fn parse_pair(s: &str) -> Result<(String, String), String> {
    match s.split_once('=') {
        Some((k, v)) if !k.trim().is_empty() && !v.trim().is_empty() => Ok((k.trim().to_string(), v.trim().to_string())),
        _ => Err(format!("expected KEY=VALUE, got '{s}'")),
    }
}

/// `[s.]t=<policy>` of `--existing-for`.
pub fn parse_existing_for(s: &str) -> Result<(String, ExistingTablePolicy), String> {
    let (t, p) = parse_pair(s)?;
    let policy = ExistingArg::from_str(&p, true).map_err(|_| format!("unknown policy '{p}' (skip, truncate, drop-recreate, append)"))?;
    Ok((t, policy.into()))
}

/// `t.c=binary|text` of `--column-as`.
pub fn parse_column_as(s: &str) -> Result<ColumnAs, String> {
    let (tc, kind) = parse_pair(s)?;
    let (table, column) = tc.rsplit_once('.').filter(|(t, c)| !t.is_empty() && !c.is_empty()).ok_or_else(|| format!("expected TABLE.COLUMN=binary|text, got '{s}'"))?;
    let treat_as = match kind.to_ascii_lowercase().as_str() {
        "binary" => ColumnTreatment::Binary,
        "text" => ColumnTreatment::Text,
        other => return Err(format!("unknown column treatment '{other}' (binary, text)")),
    };
    Ok(ColumnAs { table: table.to_string(), column: column.to_string(), treat_as })
}

fn parse_duration_arg(s: &str) -> Result<u64, String> {
    parse_duration_ms(s)
}

/// Endpoints, selection and options shared by `copy plan` and `copy run`.
#[derive(Args, Debug)]
pub struct CopySpecArgs {
    /// Source connection (saved; name or id).
    #[arg(long, value_name = "CONN", help_heading = "Endpoints")]
    pub source: String,
    /// Source database (MySQL database, PostgreSQL database, SQLite `main`).
    #[arg(long, value_name = "DB", help_heading = "Endpoints")]
    pub source_db: String,
    /// PostgreSQL: copy only this source schema (default: every user schema).
    #[arg(long, value_name = "SCHEMA", help_heading = "Endpoints")]
    pub source_schema: Option<String>,
    /// Target connection (saved, marked write-allowed; name or id).
    #[arg(long, value_name = "CONN", help_heading = "Endpoints")]
    pub target: String,
    /// Target database (a missing MySQL database is created; a PostgreSQL one must exist).
    #[arg(long, value_name = "DB", help_heading = "Endpoints")]
    pub target_db: String,
    /// PostgreSQL: write the source schema into this target schema (needs --source-schema).
    #[arg(long, value_name = "SCHEMA", help_heading = "Endpoints")]
    pub target_schema: Option<String>,
    /// Name of the run (default: "<src>.<db> -> <tgt>.<db>").
    #[arg(long, value_name = "NAME", help_heading = "Endpoints")]
    pub name: Option<String>,
    /// Read the source password from this environment variable ("ask every time" connections).
    #[arg(long, value_name = "VAR", help_heading = "Endpoints")]
    pub source_password_env: Option<String>,
    /// Read the target password from this environment variable.
    #[arg(long, value_name = "VAR", help_heading = "Endpoints")]
    pub target_password_env: Option<String>,

    /// Copy only these tables (`schema.table` or `table`), comma separated.
    #[arg(long, value_name = "TABLES", value_delimiter = ',', help_heading = "Selection")]
    pub tables: Vec<String>,
    /// Copy only the tables listed in this file (one `schema.table` or `table` per line).
    #[arg(long, value_name = "FILE", help_heading = "Selection")]
    pub tables_file: Option<PathBuf>,
    /// Leave out a plan item (and its dependants) by item id (repeatable).
    #[arg(long, value_name = "ITEM_ID", help_heading = "Selection")]
    pub exclude: Vec<String>,
    /// Leave out a table by name, `[schema.]table` (repeatable). Resolved against the plan:
    /// an unknown name, or a PostgreSQL name in several schemas without its schema, is an error.
    #[arg(long, value_name = "TABLE", help_heading = "Selection")]
    pub exclude_table: Vec<String>,
    /// What to copy.
    #[arg(long, value_enum, default_value = "both", help_heading = "Selection")]
    pub content: ContentArg,

    /// Tables that already exist in the target.
    #[arg(long, value_enum, default_value = "skip", help_heading = "Existing tables")]
    pub existing: ExistingArg,
    /// Policy for one existing table, `[schema.]table=<policy>` (repeatable). Resolved like
    /// `--exclude-table`.
    #[arg(long, value_name = "TABLE=POLICY", value_parser = parse_existing_for, help_heading = "Existing tables")]
    pub existing_for: Vec<(String, ExistingTablePolicy)>,

    /// MySQL target charset (default: utf8mb4 on 5.5.3+, else utf8).
    #[arg(long, value_name = "CHARSET", help_heading = "MySQL")]
    pub charset: Option<String>,
    /// MySQL target collation, or `legacy` (PAD SPACE, compatible with old sorting).
    #[arg(long, value_name = "COLLATION", help_heading = "MySQL")]
    pub collation: Option<String>,
    /// Charset of one table, `table=charset` (repeatable).
    #[arg(long, value_name = "TABLE=CHARSET", value_parser = parse_pair, help_heading = "MySQL")]
    pub table_charset: Vec<(String, String)>,
    /// Collation of one table, `table=collation` (repeatable).
    #[arg(long, value_name = "TABLE=COLLATION", value_parser = parse_pair, help_heading = "MySQL")]
    pub table_collation: Vec<(String, String)>,
    /// Treat a column as bytes or text, `table.column=binary|text` (repeatable).
    #[arg(long, value_name = "T.C=KIND", value_parser = parse_column_as, help_heading = "MySQL")]
    pub column_as: Vec<ColumnAs>,
    /// Storage engine of created tables.
    #[arg(long, value_enum, default_value = "auto", help_heading = "MySQL")]
    pub engine: EngineArg,
    /// Zero dates ('0000-00-00'): auto = NULL on strict targets, else keep.
    #[arg(long, value_enum, default_value = "auto", help_heading = "MySQL")]
    pub zero_dates: ZeroDatesArg,
    /// ENUM error values ('' a non-strict source stored for invalid input), which a strict
    /// target refuses: fail the table (default) or load NULL.
    #[arg(long, value_enum, help_heading = "MySQL")]
    pub enum_error: Option<EnumErrorArg>,
    /// Source time zone for the DATE_FORMAT fallback only, e.g. +09:00.
    #[arg(long, value_name = "OFFSET", help_heading = "MySQL")]
    pub time_zone: Option<String>,
    /// Do not adjust tables that exceed the row size limits (they become blockers).
    #[arg(long, help_heading = "MySQL")]
    pub no_fix_row_size: bool,
    /// Keep DEFINER= of views and triggers for accounts that exist on the target.
    #[arg(long, help_heading = "MySQL")]
    pub keep_definer: bool,

    /// PostgreSQL: use the database default for collations missing on the target (warning).
    #[arg(long, help_heading = "PostgreSQL")]
    pub pg_collation_fallback: bool,
    /// PostgreSQL SQL_ASCII source: what the stored bytes are.
    #[arg(long, value_enum, help_heading = "PostgreSQL")]
    pub pg_source_encoding: Option<PgEncodingArg>,

    /// Rename a schema, `from=to` (repeatable).
    #[arg(long, value_name = "FROM=TO", value_parser = parse_pair, help_heading = "Names")]
    pub rename_schema: Vec<(String, String)>,
    /// Rename a table, `[schema.]from=to` (repeatable).
    #[arg(long, value_name = "FROM=TO", value_parser = parse_pair, help_heading = "Names")]
    pub rename_table: Vec<(String, String)>,
    /// Lower-case target table names.
    #[arg(long, help_heading = "Names")]
    pub lowercase_names: bool,

    /// Create indexes and constraints with the tables instead of after the data.
    #[arg(long, help_heading = "Loading")]
    pub indexes_with_tables: bool,
    /// Tables copied at once (1-4; SQLite targets: 1).
    #[arg(long, value_name = "N", default_value_t = 1, value_parser = clap::value_parser!(u8).range(1..=4), help_heading = "Loading")]
    pub concurrency: u8,
    /// Rows per writer chunk (default 50000).
    #[arg(long, value_name = "N", help_heading = "Loading")]
    pub batch_rows: Option<u32>,
    /// MiB per writer piece (default 8).
    #[arg(long, value_name = "N", help_heading = "Loading")]
    pub batch_mb: Option<u32>,
    /// Seconds a target statement waits for a lock (default 5).
    #[arg(long, value_name = "SECONDS", help_heading = "Loading")]
    pub target_lock_timeout: Option<u32>,

    /// Verification after the load (checksum reads the target once more).
    #[arg(long, value_enum, default_value = "count", help_heading = "Verification")]
    pub verify: VerifyArg,
    /// Also compare exact row counts of skipped existing tables.
    #[arg(long, help_heading = "Verification")]
    pub verify_skipped: bool,
    /// Characters the target cannot hold: replace and report, or fail the table.
    #[arg(long, value_enum, default_value = "replace", help_heading = "Verification")]
    pub on_unmappable: UnmappableArg,

    #[arg(long, help_heading = "Objects")]
    pub no_views: bool,
    #[arg(long, help_heading = "Objects")]
    pub no_routines: bool,
    #[arg(long, help_heading = "Objects")]
    pub no_triggers: bool,
    /// Do not set sequence / AUTO_INCREMENT values on the target.
    #[arg(long, help_heading = "Objects")]
    pub no_sequences: bool,
    #[arg(long, help_heading = "Objects")]
    pub no_types: bool,
    #[arg(long, help_heading = "Objects")]
    pub no_extensions: bool,
    #[arg(long, help_heading = "Objects")]
    pub no_comments: bool,
    /// PostgreSQL: copy owners (ALTER ... OWNER TO).
    #[arg(long, help_heading = "Objects")]
    pub owners: bool,
    /// PostgreSQL: copy grants.
    #[arg(long, help_heading = "Objects")]
    pub grants: bool,
    #[arg(long, help_heading = "Objects")]
    pub no_analyze: bool,
    /// PostgreSQL: do not refresh materialized views.
    #[arg(long, help_heading = "Objects")]
    pub no_refresh: bool,

    /// PostgreSQL work_mem of the source sessions, e.g. 64MB.
    #[arg(long, value_name = "SIZE", help_heading = "Source session")]
    pub work_mem: Option<String>,
    /// Lock timeout of the source sessions: 10s, 500ms, ...
    #[arg(long, value_name = "DURATION", value_parser = parse_duration_arg, help_heading = "Source session")]
    pub lock_timeout: Option<u64>,
    /// Rows per fetch for cursor based drivers.
    #[arg(long, value_name = "N", help_heading = "Source session")]
    pub fetch_batch: Option<u32>,
}

#[derive(Args, Debug)]
#[command(after_long_help = COPY_AFTER_HELP)]
pub struct CopyPlanArgs {
    #[command(flatten)]
    pub spec: CopySpecArgs,
    /// Also print every target statement and the session settings of both sides.
    #[arg(long)]
    pub sql: bool,
}

#[derive(Args, Debug)]
#[command(after_long_help = COPY_AFTER_HELP)]
pub struct CopyRunArgs {
    #[command(flatten)]
    pub spec: CopySpecArgs,
    /// Refuse to run when the plan id differs from this reviewed plan (exit 3).
    #[arg(long, value_name = "PLAN_ID")]
    pub expect_plan: Option<String>,
    /// Acknowledge every warning of this code (repeatable).
    #[arg(long, value_name = "CODE")]
    pub ack: Vec<String>,
    /// Acknowledge every warning (never replaces --confirm-production).
    #[arg(long, short = 'y')]
    pub yes: bool,
    /// Print one progress line per second to stderr.
    #[arg(long)]
    pub progress: bool,
    /// Check the plan without copying (PG: DDL executed and rolled back).
    #[arg(long)]
    pub dry_run: bool,
    /// The target connection's name, required when it is tagged production.
    #[arg(long, value_name = "TARGET NAME")]
    pub confirm_production: Option<String>,
}

#[derive(Args, Debug)]
#[command(after_long_help = COPY_AFTER_HELP)]
pub struct CopyResumeArgs {
    #[arg(long, value_name = "RUN_ID")]
    pub run: String,
    /// Tables appended to partially: fail (default), truncate and reload, or skip.
    #[arg(long, value_enum, default_value = "fail")]
    pub partial: PartialArg,
    #[arg(long, value_name = "CODE")]
    pub ack: Vec<String>,
    #[arg(long, short = 'y')]
    pub yes: bool,
    #[arg(long)]
    pub progress: bool,
    #[arg(long, value_name = "NAME")]
    pub confirm_production: Option<String>,
    #[arg(long, value_name = "VAR")]
    pub source_password_env: Option<String>,
    #[arg(long, value_name = "VAR")]
    pub target_password_env: Option<String>,
}

#[derive(Args, Debug)]
pub struct CopyRunsArgs {
    /// Only runs whose source or target is this connection.
    #[arg(long, value_name = "CONN")]
    pub conn: Option<String>,
    #[arg(long, value_name = "N", default_value_t = 50)]
    pub limit: usize,
}

#[derive(Args, Debug)]
pub struct CopyRunSel {
    #[arg(long, value_name = "RUN_ID")]
    pub run: String,
}
