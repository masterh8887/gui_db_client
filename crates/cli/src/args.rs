//! Command-line surface of bulk-cli (clap derive): definitions and value parsers only. The
//! commands themselves live in the `cmd_*` modules.

use std::path::PathBuf;

use bulk_core::types::{Compression, OutputFormat, ParquetCompression, SshAuth};
use clap::{ArgGroup, Args, Parser, Subcommand, ValueEnum};

use crate::output::parse_duration_ms;

const AFTER_HELP: &str = "\
Connections:
  Every database command takes either --conn <NAME|ID> (a saved connection, see `conn list`)
  or an ad-hoc connection: --driver <ID> with --host/--port/--database/--user, --path (sqlite)
  and --field KEY=VALUE for driver specific settings (sslmode=require, service_name=ORCL,
  connection_string=...). Passwords are never flags: --password-env VAR names an environment
  variable. An ad-hoc connection is saved through the core for the duration of the command and
  deleted again when the command finishes (its password goes to the credential store and is
  removed with it); pass --save <NAME> to keep it as a saved connection.

Exit codes:
  0 done   1 failed / error   2 cancelled (Ctrl+C)   3 rejected by preflight (or by `check`)
  4 preflight warnings need acknowledgement (re-run with --yes)

Environment:
  BULKDB_CONFIG_DIR     config directory (same as --config-dir)
  BULKDB_CREDENTIALS    `memory` keeps secrets in memory instead of the OS keyring (tests, CI)
  RUST_LOG              log filter used when --log-level is not given";

/// BulkDB headless extractor: the core without a window.
#[derive(Parser, Debug)]
#[command(name = "bulk-cli", version, about = "BulkDB headless extractor: stream query results to CSV / Parquet", after_long_help = AFTER_HELP)]
pub struct Cli {
    /// Config directory (default: the app's platform config dir).
    #[arg(long, global = true, env = "BULKDB_CONFIG_DIR", value_name = "DIR")]
    pub config_dir: Option<PathBuf>,
    /// Print JSON (the core's IPC types) instead of tables.
    #[arg(long, global = true)]
    pub json: bool,
    /// Log filter for stderr: error, warn, info, debug, trace or a tracing directive
    /// (default: warn; info when --progress is given).
    #[arg(long, global = true, value_name = "LEVEL")]
    pub log_level: Option<String>,
    #[command(subcommand)]
    pub cmd: Cmd,
}

#[derive(Subcommand, Debug)]
#[allow(clippy::large_enum_variant)]
pub enum Cmd {
    /// Run an extraction: preflight, then stream the result to files.
    Export(ExportArgs),
    /// Browse metadata (databases, schemas, tables, columns, indexes, DDL, exact counts).
    Catalog(CatalogArgs),
    /// Test a connection and print server facts and recommended session settings.
    Probe(ProbeArgs),
    /// Show the execution plan and its warnings without running the extraction.
    Explain(ExplainArgs),
    /// Check a query with the extraction SQL guard, without contacting a server: exit 0 when
    /// it is a single read-only statement, 3 when an extraction would reject it.
    Check(CheckArgs),
    /// Print the version, platform and the config / log directories.
    Info,
    /// Manage saved connections.
    Conn(ConnArgs),
    /// List drivers and their availability on this machine.
    Drivers(DriversArgs),
    /// List the jobs of this process (debugging; `history` lists past runs).
    Jobs,
    /// List past extraction runs from the history database, newest first.
    History(HistoryArgs),
    /// Show or change the application settings (settings.toml).
    Settings(SettingsArgs),
    /// Write a support bundle (logs, settings, connection definitions without secrets, driver
    /// detection) to a zip file.
    Diagnostics(DiagnosticsArgs),
}

// ---------------------------------------------------------------------------------------------
// Connection selection
// ---------------------------------------------------------------------------------------------

/// `--conn <name|id>` or an ad-hoc `--driver ...` connection (exactly one of the two).
#[derive(Args, Debug, Clone)]
#[command(group(ArgGroup::new("conn_select").required(true).args(["conn", "driver"])))]
pub struct ConnSelect {
    /// Saved connection, by name or id (see `conn list`).
    #[arg(long, value_name = "NAME|ID", conflicts_with = "driver", help_heading = "Connection")]
    pub conn: Option<String>,
    #[command(flatten)]
    pub adhoc: AdhocConn,
    /// Keep the ad-hoc connection as a saved connection under this name (--driver only).
    // `conflicts_with` rather than `requires = "driver"`: clap waives a missing `requires`
    // target when it conflicts with a present arg (`--conn` excludes `--driver`), so the
    // requirement would never fire; the conflict is always enforced.
    #[arg(long, value_name = "NAME", conflicts_with = "conn", help_heading = "Connection")]
    pub save: Option<String>,
}

/// Fields of an ad-hoc connection (also the fields of `conn add`).
#[derive(Args, Debug, Clone, Default)]
pub struct AdhocConn {
    /// Driver id: postgres, mysql, mssql, sqlite, oracle, odbc (see `drivers`).
    #[arg(long, value_name = "ID", help_heading = "Ad-hoc connection")]
    pub driver: Option<String>,
    #[arg(long, value_name = "HOST", help_heading = "Ad-hoc connection")]
    pub host: Option<String>,
    /// Port (default: the driver's default port).
    #[arg(long, value_name = "PORT", help_heading = "Ad-hoc connection")]
    pub port: Option<u16>,
    #[arg(long, value_name = "NAME", help_heading = "Ad-hoc connection")]
    pub database: Option<String>,
    #[arg(long, value_name = "USER", help_heading = "Ad-hoc connection")]
    pub user: Option<String>,
    /// Environment variable that holds the password (never a flag). Also used with --conn
    /// for connections that do not store their password.
    #[arg(long, value_name = "VAR", help_heading = "Ad-hoc connection")]
    pub password_env: Option<String>,
    /// SQLite database file.
    #[arg(long, value_name = "FILE", help_heading = "Ad-hoc connection")]
    pub path: Option<PathBuf>,
    /// Driver specific field, repeatable: sslmode=require, instance=SQLEXPRESS,
    /// service_name=ORCL, driver="ODBC Driver 18 for SQL Server", connection_string=...
    #[arg(long = "field", value_name = "KEY=VALUE", value_parser = parse_key_val, help_heading = "Ad-hoc connection")]
    pub fields: Vec<(String, String)>,
    /// SSH bastion host (enables the tunnel).
    #[arg(long, value_name = "HOST", help_heading = "SSH tunnel")]
    pub ssh_host: Option<String>,
    #[arg(long, value_name = "PORT", help_heading = "SSH tunnel")]
    pub ssh_port: Option<u16>,
    #[arg(long, value_name = "USER", help_heading = "SSH tunnel")]
    pub ssh_user: Option<String>,
    /// How to authenticate to the bastion (default: password).
    #[arg(long, value_enum, value_name = "METHOD", help_heading = "SSH tunnel")]
    pub ssh_auth: Option<SshAuthArg>,
    /// OpenSSH private key (`.ppk` is rejected with a conversion hint).
    #[arg(long, value_name = "FILE", help_heading = "SSH tunnel")]
    pub ssh_key: Option<PathBuf>,
    /// Environment variable holding the ssh password (`conn add`, non-interactive).
    #[arg(long, value_name = "VAR", help_heading = "SSH tunnel")]
    pub ssh_password_env: Option<String>,
    /// Environment variable holding the key passphrase (`conn add`, non-interactive).
    #[arg(long, value_name = "VAR", help_heading = "SSH tunnel")]
    pub ssh_passphrase_env: Option<String>,
    /// Disable `ssh -C` compression for this tunnel.
    #[arg(long, help_heading = "SSH tunnel")]
    pub no_ssh_compression: bool,
    /// Default output directory stored with the connection.
    #[arg(long, value_name = "DIR", help_heading = "Ad-hoc connection")]
    pub output_dir: Option<PathBuf>,
}

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum SshAuthArg {
    Password,
    Key,
    Agent,
}

impl From<SshAuthArg> for SshAuth {
    fn from(a: SshAuthArg) -> Self {
        match a {
            SshAuthArg::Password => SshAuth::Password,
            SshAuthArg::Key => SshAuth::Key,
            SshAuthArg::Agent => SshAuth::Agent,
        }
    }
}

// ---------------------------------------------------------------------------------------------
// export
// ---------------------------------------------------------------------------------------------

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormatArg {
    Csv,
    Parquet,
}

impl From<FormatArg> for OutputFormat {
    fn from(f: FormatArg) -> Self {
        match f {
            FormatArg::Csv => OutputFormat::Csv,
            FormatArg::Parquet => OutputFormat::Parquet,
        }
    }
}

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompressArg {
    Gzip,
    Zstd,
    /// No compression (overrides a `.gz` / `.zst` --out extension).
    #[value(name = "none")]
    NoCompression,
}

impl CompressArg {
    pub fn to_core(self) -> Option<Compression> {
        match self {
            CompressArg::Gzip => Some(Compression::Gzip),
            CompressArg::Zstd => Some(Compression::Zstd),
            CompressArg::NoCompression => None,
        }
    }
}

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParquetCompressionArg {
    Zstd,
    Snappy,
    #[value(name = "none")]
    NoCompression,
}

impl From<ParquetCompressionArg> for ParquetCompression {
    fn from(c: ParquetCompressionArg) -> Self {
        match c {
            ParquetCompressionArg::Zstd => ParquetCompression::Zstd,
            ParquetCompressionArg::Snappy => ParquetCompression::Snappy,
            ParquetCompressionArg::NoCompression => ParquetCompression::None,
        }
    }
}

/// `export` arguments.
#[derive(Args, Debug, Clone)]
#[command(group(ArgGroup::new("sql_source").required(true).args(["sql", "query"])))]
pub struct ExportArgs {
    #[command(flatten)]
    pub target: ConnSelect,

    /// SQL file (the core reads it at execution time and records the path in history).
    #[arg(long, value_name = "FILE", help_heading = "Query")]
    pub sql: Option<PathBuf>,
    /// Inline SQL, or `-` to read the query from stdin.
    #[arg(long, value_name = "SQL", help_heading = "Query")]
    pub query: Option<String>,
    /// Label used for `{name}` in file names and in history (default: the --out stem).
    #[arg(long, value_name = "LABEL", help_heading = "Output")]
    pub name: Option<String>,

    /// Output file: directory, stem and extension chain set --dir, the name and the format
    /// (.csv, .tsv, .parquet; .gz / .zst add compression).
    #[arg(long, value_name = "PATH", conflicts_with_all = ["dir", "pattern"], help_heading = "Output")]
    pub out: Option<PathBuf>,
    /// Output directory, used with --name / --pattern (default: the connection's output dir).
    #[arg(long, value_name = "DIR", help_heading = "Output")]
    pub dir: Option<PathBuf>,
    /// File name pattern without extension, with --dir. Placeholders: {conn}, {name},
    /// {yyyyMMdd_HHmm}, {yyyyMMdd}, {HHmmss} (default: {conn}_{name}_{yyyyMMdd_HHmm}).
    #[arg(long, value_name = "PATTERN", help_heading = "Output")]
    pub pattern: Option<String>,
    /// Output format (default: from the --out extension, else csv).
    #[arg(long, value_enum, value_name = "FORMAT", help_heading = "Output")]
    pub format: Option<FormatArg>,
    /// Overwrite existing output files instead of failing preflight.
    #[arg(long, help_heading = "Output")]
    pub overwrite: bool,

    /// Field delimiter: a single character, `\t` or `tab` (default: `,`; `\t` for .tsv).
    #[arg(long, value_name = "CHAR", help_heading = "CSV")]
    pub delimiter: Option<String>,
    /// Do not write the header line.
    #[arg(long, help_heading = "CSV")]
    pub no_header: bool,
    /// Text written for NULL (default: empty).
    #[arg(long, value_name = "TEXT", help_heading = "CSV")]
    pub null: Option<String>,
    /// Write a UTF-8 byte order mark (Excel).
    #[arg(long, help_heading = "CSV")]
    pub bom: bool,
    /// CRLF line endings (default: the platform setting; CRLF on Windows).
    #[arg(long, conflicts_with = "lf", help_heading = "CSV")]
    pub crlf: bool,
    /// LF line endings.
    #[arg(long, help_heading = "CSV")]
    pub lf: bool,
    /// Compress the CSV stream (default: from the --out extension).
    #[arg(long, value_enum, value_name = "CODEC", help_heading = "CSV")]
    pub compress: Option<CompressArg>,
    /// Excel preset: split every 1,000,000 rows, BOM, CRLF.
    #[arg(long, help_heading = "CSV")]
    pub excel: bool,

    /// Parquet codec (default: zstd).
    #[arg(long, value_enum, value_name = "CODEC", help_heading = "Parquet")]
    pub parquet_compression: Option<ParquetCompressionArg>,
    /// Parquet row group size in MB (default: 128).
    #[arg(long, value_name = "MB", help_heading = "Parquet")]
    pub row_group_mb: Option<u32>,

    /// Start a new file every N rows.
    #[arg(long, value_name = "N", help_heading = "Splitting")]
    pub split_rows: Option<u64>,
    /// Start a new file every N megabytes.
    #[arg(long, value_name = "N", help_heading = "Splitting")]
    pub split_mb: Option<u64>,

    /// PostgreSQL work_mem for the session, e.g. 64MB.
    #[arg(long, value_name = "SIZE", help_heading = "Session")]
    pub work_mem: Option<String>,
    /// Statement timeout: 60m, 30s, 1500ms or plain milliseconds.
    #[arg(long, value_name = "DURATION", value_parser = parse_duration_arg, help_heading = "Session")]
    pub statement_timeout: Option<u64>,
    /// Lock timeout: 10s, 500ms, ...
    #[arg(long, value_name = "DURATION", value_parser = parse_duration_arg, help_heading = "Session")]
    pub lock_timeout: Option<u64>,
    /// PostgreSQL max_parallel_workers_per_gather.
    #[arg(long, value_name = "N", help_heading = "Session")]
    pub parallel_workers: Option<u32>,
    /// Rows per fetch for cursor based drivers (default: 65536).
    #[arg(long, value_name = "N", help_heading = "Session")]
    pub fetch_batch: Option<u32>,

    /// Acknowledge every preflight warning (otherwise: ask on the tty, or exit 4).
    #[arg(long, short = 'y', help_heading = "Run")]
    pub yes: bool,
    /// Print rows, bytes, MB/s, ETA, phase, server state and process RSS once per second.
    #[arg(long, help_heading = "Run")]
    pub progress: bool,
}

// ---------------------------------------------------------------------------------------------
// catalog
// ---------------------------------------------------------------------------------------------

#[derive(Args, Debug)]
pub struct CatalogArgs {
    #[command(subcommand)]
    pub cmd: CatalogCmd,
}

#[derive(Subcommand, Debug)]
pub enum CatalogCmd {
    /// List databases.
    Databases(CatalogCommon),
    /// List schemas of a database (--db, default: the connection's database).
    Schemas(CatalogCommon),
    /// List tables with estimated rows and sizes.
    Tables(CatalogTablesArgs),
    /// List columns of a table.
    Columns(CatalogTableArgs),
    /// List indexes of a table.
    Indexes(CatalogTableArgs),
    /// Print the DDL of a table.
    Ddl(CatalogTableArgs),
    /// Run an exact `count(*)` as a cancellable job (--cached: print the last stored count).
    Count(CatalogCountArgs),
}

#[derive(Args, Debug, Clone)]
pub struct CatalogCommon {
    #[command(flatten)]
    pub target: ConnSelect,
    /// Database (default: the connection's database).
    #[arg(long, value_name = "NAME", help_heading = "Catalog")]
    pub db: Option<String>,
    /// Bypass the metadata cache.
    #[arg(long, help_heading = "Catalog")]
    pub refresh: bool,
}

#[derive(Args, Debug, Clone)]
pub struct CatalogTablesArgs {
    #[command(flatten)]
    pub common: CatalogCommon,
    /// Schema (default: the driver's default schema).
    #[arg(long, value_name = "NAME", help_heading = "Catalog")]
    pub schema: Option<String>,
}

#[derive(Args, Debug, Clone)]
pub struct CatalogTableArgs {
    #[command(flatten)]
    pub common: CatalogCommon,
    /// Table as NAME, SCHEMA.NAME or DB.SCHEMA.NAME.
    #[arg(long, value_name = "[SCHEMA.]NAME", help_heading = "Catalog")]
    pub table: String,
    /// Schema, when the table name itself is not qualified.
    #[arg(long, value_name = "NAME", help_heading = "Catalog")]
    pub schema: Option<String>,
}

#[derive(Args, Debug, Clone)]
pub struct CatalogCountArgs {
    #[command(flatten)]
    pub table: CatalogTableArgs,
    /// Print the last exact count stored for this table (with when it was taken) instead of
    /// running a new `count(*)`. Never contacts the server. Needs a saved connection (--conn):
    /// counts are kept per saved connection, an ad-hoc --driver connection has none. Exits 1
    /// when the table was never counted, so `count --cached ... || count ...` falls back.
    #[arg(long, conflicts_with = "driver", help_heading = "Catalog")]
    pub cached: bool,
}

// ---------------------------------------------------------------------------------------------
// probe / explain / drivers
// ---------------------------------------------------------------------------------------------

#[derive(Args, Debug)]
pub struct ProbeArgs {
    #[command(flatten)]
    pub target: ConnSelect,
}

#[derive(Args, Debug)]
#[command(group(ArgGroup::new("sql_source").required(true).args(["sql", "query"])))]
pub struct ExplainArgs {
    #[command(flatten)]
    pub target: ConnSelect,
    /// SQL file.
    #[arg(long, value_name = "FILE", help_heading = "Query")]
    pub sql: Option<PathBuf>,
    /// Inline SQL, or `-` to read it from stdin.
    #[arg(long, value_name = "SQL", help_heading = "Query")]
    pub query: Option<String>,
    /// EXPLAIN ANALYZE: really runs the query.
    #[arg(long)]
    pub analyze: bool,
    /// Print the raw EXPLAIN output instead of the parsed tree.
    #[arg(long)]
    pub raw: bool,
}

#[derive(Args, Debug)]
#[command(group(ArgGroup::new("sql_source").required(true).args(["sql", "query"])))]
pub struct CheckArgs {
    /// Read the query with this saved connection's SQL dialect (MySQL quotes and comments
    /// differ; ODBC must pass every reading), as export and explain will. Without it the
    /// standard (PostgreSQL / ANSI) rules apply.
    #[arg(long, value_name = "NAME|ID")]
    pub conn: Option<String>,
    /// SQL file.
    #[arg(long, value_name = "FILE", help_heading = "Query")]
    pub sql: Option<PathBuf>,
    /// Inline SQL, or `-` to read it from stdin.
    #[arg(long, value_name = "SQL", help_heading = "Query")]
    pub query: Option<String>,
}

#[derive(Args, Debug)]
pub struct DriversArgs {
    /// Re-detect vendor libraries before listing.
    #[arg(long)]
    pub rescan: bool,
}

// ---------------------------------------------------------------------------------------------
// history / settings / diagnostics
// ---------------------------------------------------------------------------------------------

#[derive(Args, Debug)]
pub struct HistoryArgs {
    /// Only runs of this connection: a saved name or id, or the id of a connection that no
    /// longer exists (ad-hoc runs, deleted connections).
    #[arg(long, value_name = "NAME|ID")]
    pub conn: Option<String>,
    /// Maximum number of runs to list.
    #[arg(long, value_name = "N", default_value_t = 50, value_parser = clap::value_parser!(u32).range(1..))]
    pub limit: u32,
}

const SETTINGS_KEYS_HELP: &str = "\
Keys (snake_case, kebab-case or camelCase):
  max_concurrent_jobs   1..3 concurrent extraction jobs
  default_output_dir    output directory when neither the connection nor --dir / --out sets
                        one (an empty value clears it)
  csv_crlf_default      CRLF line endings by default: true / false
  verify_after_export   re-read CSV outputs after a run: true / false
  log_level             level of the app's log file: trace, debug, info, warn, error
                        (bulk-cli itself logs to stderr per --log-level)
  tunnel_idle_secs      close idle SSH tunnels after N seconds (applies from the next start)
  monitor_interval_ms   server monitor interval in ms, at least 500
  progress_interval_ms  progress event interval in ms, at least 100
  ui.<name>             free-form UI preference (an empty value removes it)

Example:
  bulk-cli settings set max_concurrent_jobs=2 default_output_dir=/data/exports verify_after_export=true";

#[derive(Args, Debug)]
pub struct SettingsArgs {
    #[command(subcommand)]
    pub cmd: SettingsCmd,
}

#[derive(Subcommand, Debug)]
pub enum SettingsCmd {
    /// Print every setting, or the value of one key.
    #[command(after_long_help = SETTINGS_KEYS_HELP)]
    Get {
        /// Setting key; omit it to print all settings.
        #[arg(value_name = "KEY")]
        key: Option<String>,
    },
    /// Change settings. Every pair is validated first, then all are saved together.
    #[command(after_long_help = SETTINGS_KEYS_HELP)]
    Set {
        /// One or more KEY=VALUE pairs.
        #[arg(value_name = "KEY=VALUE", required = true, num_args = 1.., value_parser = parse_key_val)]
        pairs: Vec<(String, String)>,
    },
}

#[derive(Args, Debug)]
pub struct DiagnosticsArgs {
    /// Directory for the zip file (created when missing).
    #[arg(long, value_name = "DIR", default_value = ".")]
    pub out: PathBuf,
}

// ---------------------------------------------------------------------------------------------
// conn
// ---------------------------------------------------------------------------------------------

#[derive(Args, Debug)]
pub struct ConnArgs {
    #[command(subcommand)]
    pub cmd: ConnCmd,
}

#[derive(Subcommand, Debug)]
pub enum ConnCmd {
    /// List saved connections.
    List,
    /// Show one saved connection (never a secret: only whether one is stored).
    Show {
        /// Connection name or id.
        #[arg(value_name = "NAME|ID")]
        conn: String,
    },
    /// Save a new connection (password from --password-env or prompted; stored in the keyring).
    Add(Box<ConnAddArgs>),
    /// Change a saved connection in place. Only the given flags change; the id stays, so the
    /// connection's history stays attached to it.
    Edit(Box<ConnEditArgs>),
    /// Delete a saved connection and its stored secrets.
    Delete {
        /// Connection name or id.
        #[arg(value_name = "NAME|ID")]
        conn: String,
        /// Do not ask for confirmation.
        #[arg(long, short = 'y')]
        yes: bool,
    },
    /// Test a saved connection (tunnel -> connect -> probe).
    Test {
        /// Connection name or id.
        #[arg(value_name = "NAME|ID")]
        conn: String,
        /// Environment variable holding the password when it is not stored.
        #[arg(long, value_name = "VAR")]
        password_env: Option<String>,
    },
    /// Import connections from DBeaver's data-sources.json (passwords are never imported).
    ImportDbeaver {
        /// Path to data-sources.json.
        path: PathBuf,
        /// Save the imported connections instead of only listing them.
        #[arg(long)]
        save: bool,
    },
}

#[derive(Args, Debug)]
pub struct ConnAddArgs {
    /// Display name of the connection.
    #[arg(long, value_name = "NAME")]
    pub name: String,
    #[command(flatten)]
    pub adhoc: AdhocConn,
    /// Ask for the password every time instead of storing it.
    #[arg(long)]
    pub no_save_password: bool,
    /// When a connection with this name exists, replace its definition with these flags
    /// (keeping its id, history and session defaults) instead of failing.
    #[arg(long)]
    pub replace: bool,
}

/// `conn edit`: every flag is optional and only what is given changes.
#[derive(Args, Debug)]
pub struct ConnEditArgs {
    /// Connection name or id.
    #[arg(value_name = "NAME|ID")]
    pub conn: String,
    /// New display name.
    #[arg(long, value_name = "NAME")]
    pub name: Option<String>,
    /// Driver, host, port, database, user, fields, SSH and output dir to change. An empty
    /// value (`--host ""`) clears host, database or user.
    #[command(flatten)]
    pub adhoc: AdhocConn,
    /// Remove a driver specific field, repeatable.
    #[arg(long = "unset-field", value_name = "KEY", help_heading = "Edit")]
    pub unset_fields: Vec<String>,
    /// Remove the connection's default output directory.
    #[arg(long, conflicts_with = "output_dir", help_heading = "Edit")]
    pub clear_output_dir: bool,
    /// Remove the SSH tunnel (and its stored secrets).
    #[arg(
        long,
        conflicts_with_all = ["ssh_host", "ssh_port", "ssh_user", "ssh_auth", "ssh_key", "ssh_password_env", "ssh_passphrase_env", "no_ssh_compression", "ssh_compression"],
        help_heading = "Edit"
    )]
    pub no_ssh: bool,
    /// Turn `ssh -C` compression back on.
    #[arg(long, conflicts_with = "no_ssh_compression", help_heading = "Edit")]
    pub ssh_compression: bool,
    /// Store the password in the credential store (prompted for on a terminal unless
    /// --password-env is given).
    #[arg(long, conflicts_with = "no_save_password", help_heading = "Edit")]
    pub save_password: bool,
    /// Ask for the password every time; removes the stored secrets.
    #[arg(long, conflicts_with_all = ["password_env", "ssh_password_env", "ssh_passphrase_env"], help_heading = "Edit")]
    pub no_save_password: bool,
    /// Prompt for a new password on the terminal (instead of --password-env).
    #[arg(long, conflicts_with_all = ["password_env", "no_save_password"], help_heading = "Edit")]
    pub ask_password: bool,
    /// Delete the stored database password; the SSH secret stays.
    #[arg(long, conflicts_with_all = ["password_env", "ask_password"], help_heading = "Edit")]
    pub clear_password: bool,
    /// Delete the stored SSH password / key passphrase; the database password stays.
    #[arg(long, conflicts_with_all = ["ssh_password_env", "ssh_passphrase_env"], help_heading = "Edit")]
    pub clear_ssh_secret: bool,
}

// ---------------------------------------------------------------------------------------------
// value parsers
// ---------------------------------------------------------------------------------------------

/// `KEY=VALUE` for `--field`.
pub fn parse_key_val(s: &str) -> Result<(String, String), String> {
    let (k, v) = s.split_once('=').ok_or_else(|| format!("expected KEY=VALUE, got '{s}'"))?;
    let k = k.trim();
    if k.is_empty() {
        return Err(format!("empty key in '{s}'"));
    }
    Ok((k.to_string(), v.to_string()))
}

fn parse_duration_arg(s: &str) -> Result<u64, String> {
    parse_duration_ms(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn export_requires_a_connection_and_a_query() {
        assert!(Cli::try_parse_from(["bulk-cli", "export", "--query", "select 1", "--out", "x.csv"]).is_err());
        assert!(Cli::try_parse_from(["bulk-cli", "export", "--driver", "sqlite", "--out", "x.csv"]).is_err());
        assert!(Cli::try_parse_from(["bulk-cli", "export", "--conn", "a", "--driver", "sqlite", "--query", "select 1"]).is_err());
        let ok = Cli::try_parse_from(["bulk-cli", "export", "--driver", "sqlite", "--path", "t.db", "--query", "select 1", "--out", "x.csv"]).unwrap();
        match ok.cmd {
            Cmd::Export(e) => {
                assert_eq!(e.target.adhoc.driver.as_deref(), Some("sqlite"));
                assert!(e.sql.is_none());
            }
            _ => panic!("expected export"),
        }
    }

    #[test]
    fn durations_and_fields_parse() {
        let cli = Cli::try_parse_from([
            "bulk-cli",
            "export",
            "--conn",
            "prod",
            "--sql",
            "q.sql",
            "--dir",
            "out",
            "--statement-timeout",
            "60m",
            "--lock-timeout",
            "500ms",
            "--field",
            "sslmode=require",
            "--field",
            "connection_string=a=b;c=d",
        ])
        .unwrap();
        let Cmd::Export(e) = cli.cmd else { panic!() };
        assert_eq!(e.statement_timeout, Some(3_600_000));
        assert_eq!(e.lock_timeout, Some(500));
        assert_eq!(e.target.adhoc.fields, vec![("sslmode".to_string(), "require".to_string()), ("connection_string".to_string(), "a=b;c=d".to_string())]);
        assert!(Cli::try_parse_from(["bulk-cli", "export", "--conn", "p", "--query", "x", "--statement-timeout", "soon"]).is_err());
    }

    #[test]
    fn catalog_table_subcommands() {
        let cli = Cli::try_parse_from(["bulk-cli", "catalog", "columns", "--conn", "prod", "--table", "public.orders", "--json"]).unwrap();
        assert!(cli.json);
        match cli.cmd {
            Cmd::Catalog(c) => match c.cmd {
                CatalogCmd::Columns(t) => assert_eq!(t.table, "public.orders"),
                _ => panic!(),
            },
            _ => panic!(),
        }
    }

    #[test]
    fn catalog_count_cached_flag() {
        let cli = Cli::try_parse_from(["bulk-cli", "catalog", "count", "--conn", "prod", "--table", "public.orders", "--cached"]).unwrap();
        let Cmd::Catalog(CatalogArgs { cmd: CatalogCmd::Count(c) }) = cli.cmd else { panic!("expected catalog count") };
        assert!(c.cached);
        assert_eq!((c.table.table.as_str(), c.table.common.target.conn.as_deref()), ("public.orders", Some("prod")));
        let Cmd::Catalog(CatalogArgs { cmd: CatalogCmd::Count(c) }) =
            Cli::try_parse_from(["bulk-cli", "catalog", "count", "--conn", "prod", "--table", "t"]).unwrap().cmd
        else {
            panic!()
        };
        assert!(!c.cached, "a plain count runs a new job");
        // Stored counts belong to saved connections; an ad-hoc one never has any.
        assert!(Cli::try_parse_from(["bulk-cli", "catalog", "count", "--driver", "sqlite", "--path", "a.db", "--table", "t", "--cached"]).is_err());
        // Only `count` knows the flag.
        assert!(Cli::try_parse_from(["bulk-cli", "catalog", "columns", "--conn", "prod", "--table", "t", "--cached"]).is_err());
    }

    #[test]
    fn save_requires_driver() {
        assert!(Cli::try_parse_from(["bulk-cli", "probe", "--conn", "prod", "--save", "x"]).is_err());
        assert!(Cli::try_parse_from(["bulk-cli", "probe", "--driver", "sqlite", "--path", "a.db", "--save", "x"]).is_ok());
    }

    #[test]
    fn command_definition_is_consistent() {
        // Catches conflicts_with / requires that name an arg id which does not exist.
        <Cli as clap::CommandFactory>::command().debug_assert();
    }

    #[test]
    fn history_settings_and_diagnostics_parse() {
        let cli = Cli::try_parse_from(["bulk-cli", "history", "--conn", "prod", "--limit", "5"]).unwrap();
        let Cmd::History(h) = cli.cmd else { panic!("expected history") };
        assert_eq!((h.conn.as_deref(), h.limit), (Some("prod"), 5));
        let Cmd::History(h) = Cli::try_parse_from(["bulk-cli", "history"]).unwrap().cmd else { panic!() };
        assert_eq!((h.conn, h.limit), (None, 50));
        assert!(Cli::try_parse_from(["bulk-cli", "history", "--limit", "0"]).is_err());

        let cli = Cli::try_parse_from(["bulk-cli", "settings", "set", "max_concurrent_jobs=2", "default_output_dir="]).unwrap();
        let Cmd::Settings(SettingsArgs { cmd: SettingsCmd::Set { pairs } }) = cli.cmd else { panic!("expected settings set") };
        assert_eq!(pairs, vec![("max_concurrent_jobs".to_string(), "2".to_string()), ("default_output_dir".to_string(), String::new())]);
        assert!(Cli::try_parse_from(["bulk-cli", "settings", "set"]).is_err());
        assert!(Cli::try_parse_from(["bulk-cli", "settings", "set", "novalue"]).is_err());
        let Cmd::Settings(SettingsArgs { cmd: SettingsCmd::Get { key } }) = Cli::try_parse_from(["bulk-cli", "settings", "get"]).unwrap().cmd else { panic!() };
        assert!(key.is_none());

        let Cmd::Diagnostics(d) = Cli::try_parse_from(["bulk-cli", "diagnostics"]).unwrap().cmd else { panic!() };
        assert_eq!(d.out, PathBuf::from("."));
    }

    #[test]
    fn conn_edit_flags() {
        let cli = Cli::try_parse_from(["bulk-cli", "conn", "edit", "prod", "--host", "db2", "--password-env", "PW", "--unset-field", "sslmode", "--no-ssh"]).unwrap();
        let Cmd::Conn(ConnArgs { cmd: ConnCmd::Edit(e) }) = cli.cmd else { panic!("expected conn edit") };
        assert_eq!(e.conn, "prod");
        assert_eq!(e.adhoc.host.as_deref(), Some("db2"));
        assert_eq!(e.adhoc.password_env.as_deref(), Some("PW"));
        assert_eq!(e.unset_fields, vec!["sslmode".to_string()]);
        assert!(e.no_ssh);
        // Contradictory flags are refused by clap.
        assert!(Cli::try_parse_from(["bulk-cli", "conn", "edit", "prod", "--no-ssh", "--ssh-host", "b"]).is_err());
        assert!(Cli::try_parse_from(["bulk-cli", "conn", "edit", "prod", "--save-password", "--no-save-password"]).is_err());
        assert!(Cli::try_parse_from(["bulk-cli", "conn", "edit", "prod", "--ask-password", "--password-env", "PW"]).is_err());
        assert!(Cli::try_parse_from(["bulk-cli", "conn", "edit", "prod", "--clear-output-dir", "--output-dir", "x"]).is_err());
        assert!(Cli::try_parse_from(["bulk-cli", "conn", "edit"]).is_err());
        let cli = Cli::try_parse_from(["bulk-cli", "conn", "add", "--name", "p", "--driver", "sqlite", "--path", "a.db", "--replace"]).unwrap();
        let Cmd::Conn(ConnArgs { cmd: ConnCmd::Add(a) }) = cli.cmd else { panic!() };
        assert!(a.replace);
    }

    #[test]
    fn conn_edit_clear_secret_flags() {
        let cli = Cli::try_parse_from(["bulk-cli", "conn", "edit", "prod", "--clear-password", "--clear-ssh-secret"]).unwrap();
        let Cmd::Conn(ConnArgs { cmd: ConnCmd::Edit(e) }) = cli.cmd else { panic!("expected conn edit") };
        assert!(e.clear_password && e.clear_ssh_secret);
        let Cmd::Conn(ConnArgs { cmd: ConnCmd::Edit(e) }) = Cli::try_parse_from(["bulk-cli", "conn", "edit", "prod"]).unwrap().cmd else { panic!() };
        assert!(!e.clear_password && !e.clear_ssh_secret);
        // Clearing a secret and giving a new one for it at the same time is refused.
        assert!(Cli::try_parse_from(["bulk-cli", "conn", "edit", "prod", "--clear-password", "--password-env", "PW"]).is_err());
        assert!(Cli::try_parse_from(["bulk-cli", "conn", "edit", "prod", "--clear-password", "--ask-password"]).is_err());
        assert!(Cli::try_parse_from(["bulk-cli", "conn", "edit", "prod", "--clear-ssh-secret", "--ssh-password-env", "S"]).is_err());
        assert!(Cli::try_parse_from(["bulk-cli", "conn", "edit", "prod", "--clear-ssh-secret", "--ssh-passphrase-env", "S"]).is_err());
        // The other secret can still be replaced in the same edit.
        assert!(Cli::try_parse_from(["bulk-cli", "conn", "edit", "prod", "--clear-password", "--ssh-password-env", "S"]).is_ok());
        assert!(Cli::try_parse_from(["bulk-cli", "conn", "edit", "prod", "--clear-ssh-secret", "--password-env", "PW"]).is_ok());
    }

    #[test]
    fn check_info_and_conn_show_parse() {
        let Cmd::Check(c) = Cli::try_parse_from(["bulk-cli", "check", "--query", "select 1", "--conn", "prod"]).unwrap().cmd else { panic!("expected check") };
        assert_eq!((c.query.as_deref(), c.conn.as_deref()), (Some("select 1"), Some("prod")));
        let Cmd::Check(c) = Cli::try_parse_from(["bulk-cli", "check", "--sql", "q.sql"]).unwrap().cmd else { panic!() };
        assert_eq!(c.sql, Some(PathBuf::from("q.sql")));
        assert!(c.conn.is_none());
        assert!(Cli::try_parse_from(["bulk-cli", "check"]).is_err(), "a query is required");
        assert!(Cli::try_parse_from(["bulk-cli", "check", "--sql", "q.sql", "--query", "x"]).is_err());
        assert!(matches!(Cli::try_parse_from(["bulk-cli", "info", "--json"]).unwrap().cmd, Cmd::Info));
        let Cmd::Conn(ConnArgs { cmd: ConnCmd::Show { conn } }) = Cli::try_parse_from(["bulk-cli", "conn", "show", "prod"]).unwrap().cmd else {
            panic!("expected conn show")
        };
        assert_eq!(conn, "prod");
        assert!(Cli::try_parse_from(["bulk-cli", "conn", "show"]).is_err());
    }
}
