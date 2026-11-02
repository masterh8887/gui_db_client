//! MySQL-family logic shared by both MySQL clients (design: `docs/mysql-compat.md`).
//!
//! Owner: **modern**. The version-aware SQL for probe / session / EXPLAIN (here) and for the
//! catalog (`catalog::mysql::generic`) and the monitor (`monitor::mysql::generic`) is written
//! ONCE over [`MysqlRawQuery`], which both clients implement:
//!
//! * [`super::mysql::MySqlDatabase`] — mysql_async, MySQL 4.1+ / MariaDB (protocol 4.1);
//! * [`super::mysql_legacy::MySqlLegacyDatabase`] — BulkDB's own pre-4.1 client (3.23 / 4.0).
//!
//! Only the extraction stream is client specific (binary protocol prepared statement vs. the
//! legacy text protocol). Metadata queries go through the text protocol on both clients and
//! return small, fully read [`MetaRows`] (metadata only, never table data, bounded by
//! [`META_ROW_LIMIT`]). Parameters are inlined with [`quote_str`] because 4.0 has no prepared
//! statements.
//!
//! The capability matrix ([`MysqlCaps`]) is the single source of truth for "which version can
//! do what"; the design doc's table mirrors it.

use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::time::Instant;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use super::arrow_batch::{Cell, ColKind};
use super::mysql::{
    explain_analyze_root_rows, grants_allow_write, has_statement_separator, may_have_statement_separator, parse_bit, parse_date_days, parse_datetime_micros,
    parse_decimal, parse_explain_json, parse_i64, trim_statement, ymdhms_to_micros, ExplainParsed, Flavor, LexMode, ServerVersion, ValueParser,
};
use super::mysql_dispatch::{plan_text, EncodingSetting, TextPlan};
use crate::encoding::{decode_cell, encode_sql, find_unrepresentable, repair_latin1_mojibake, sql_encoding_error, Charset, CollationMap};
use crate::error::{CoreError, Result};
use crate::types::{PlanNode, PlanSummary, ServerInfo, SessionOpts, TableRef};

/// A metadata query returning more rows than this fails with `INTERNAL` instead of growing
/// without bound (catalog listings of very large servers stay far below it).
pub const META_ROW_LIMIT: usize = 1_000_000;

/// Hint appended to the `AUTH` error when a 4.1+ server asks for the pre-4.1 password scheme
/// and `allow_old_password` is off (mysql_async `DriverError::MysqlOldPasswordDisabled`).
pub const OLD_PASSWORD_HINT: &str = "this account has a pre-4.1 (old_password) hash; enable \"Allow old passwords\" \
     (allow_old_password=true) in the connection settings, or ask the DBA to reset the password with a 4.1+ hash";

/// Hint for `ER_SERVER_IS_IN_SECURE_AUTH_MODE` (1275): a 5.6.5+ server (secure_auth on, the
/// default) refuses an account whose password still has the pre-4.1 hash.
pub const SECURE_AUTH_HINT: &str = "the server refuses pre-4.1 password hashes (secure_auth is on, the default since MySQL 5.6.5), \
     so \"Allow old passwords\" cannot help; ask the DBA to reset this account's password with a 4.1+ hash \
     (SET old_passwords = 0; SET PASSWORD FOR 'user'@'host' = PASSWORD('...'))";

/// Hint for `ER_HOST_IS_BLOCKED` (1129).
pub const HOST_BLOCKED_HINT: &str = "the server blocked this client host after too many interrupted connection attempts \
     (max_connect_errors); a DBA must run FLUSH HOSTS (or mysqladmin flush-hosts) before any client on this host can connect again";

/// Hint for `ER_HOST_NOT_PRIVILEGED` (1130).
pub const HOST_NOT_ALLOWED_HINT: &str = "no MySQL account allows connections from this client host; ask the DBA to grant access \
     from this host, or connect through an SSH tunnel from a host that is allowed";

/// `ER_UNKNOWN_SYSTEM_VARIABLE`: an optional session variable the server does not know.
pub const ER_UNKNOWN_SYSTEM_VARIABLE: u16 = 1193;
/// `ER_NO_SUCH_THREAD`: `KILL` of a session that already ended.
pub const ER_NO_SUCH_THREAD: u16 = 1094;

// ---------------------------------------------------------------------------------------------
// Capabilities
// ---------------------------------------------------------------------------------------------

/// What a server version supports. Built once per connection from the version string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MysqlCaps {
    /// 4.1+: protocol 4.1 handshake (mysql_async can talk to it).
    pub protocol_41: bool,
    /// 4.1+: per-connection charsets, `SET NAMES`, `SHOW COLLATION`, `character_set_*` vars.
    pub set_names: bool,
    /// 4.1+: `COM_STMT_PREPARE` (the modern client's single-statement guard).
    pub prepared_statements: bool,
    /// 4.1+: `@@sql_mode` is readable (4.0: lex with the defaults).
    pub sql_mode_var: bool,
    /// 5.0+: `information_schema`, views, `KILL QUERY`.
    pub information_schema: bool,
    pub kill_query: bool,
    /// 5.0.3+: `SHOW VARIABLES WHERE ...`.
    pub show_where: bool,
    /// 5.1.7+: `information_schema.PROCESSLIST` (older: `SHOW FULL PROCESSLIST`).
    pub processlist_table: bool,
    /// 5.5.3+: `lock_wait_timeout`, `utf8mb4` (MariaDB: 5.5+).
    pub lock_wait_timeout: bool,
    pub utf8mb4: bool,
    /// MySQL 5.6.5+ / MariaDB 10.0+: `SET SESSION TRANSACTION READ ONLY`.
    pub read_only_tx: bool,
    /// MySQL 5.6.5+ / MariaDB 10.1+: `EXPLAIN FORMAT=JSON` (older: tabular `EXPLAIN`).
    pub explain_json: bool,
    /// MySQL 5.7.8+: `max_execution_time` (milliseconds).
    pub max_execution_time: bool,
    /// MariaDB 10.1+: `max_statement_time` (seconds).
    pub max_statement_time: bool,
    /// MySQL 8.0.18+: `EXPLAIN ANALYZE`; MariaDB 10.1+: `ANALYZE FORMAT=JSON`.
    pub explain_analyze: bool,
    /// The server can still hold pre-4.1 password hashes (MySQL < 5.7.5; MariaDB keeps
    /// `mysql_old_password`): `allow_old_password` is meaningful.
    pub old_password_hashes: bool,
}

impl MysqlCaps {
    pub fn for_version(v: &ServerVersion) -> MysqlCaps {
        let at = |a: u16, b: u16, c: u16| v.at_least(a, b, c);
        let maria = v.flavor == Flavor::MariaDb;
        MysqlCaps {
            protocol_41: at(4, 1, 0),
            set_names: at(4, 1, 0),
            prepared_statements: at(4, 1, 0),
            sql_mode_var: at(4, 1, 0),
            information_schema: at(5, 0, 0),
            kill_query: at(5, 0, 0),
            show_where: at(5, 0, 3),
            processlist_table: at(5, 1, 7),
            lock_wait_timeout: at(5, 5, 3) || (maria && at(5, 5, 0)),
            utf8mb4: at(5, 5, 3) || (maria && at(5, 5, 0)),
            read_only_tx: if maria { at(10, 0, 0) } else { at(5, 6, 5) },
            explain_json: if maria { at(10, 1, 0) } else { at(5, 6, 5) },
            max_execution_time: !maria && at(5, 7, 8),
            max_statement_time: maria && at(10, 1, 0),
            explain_analyze: if maria { at(10, 1, 0) } else { at(8, 0, 18) },
            old_password_hashes: maria || !at(5, 7, 5),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Server facts and the raw-query trait
// ---------------------------------------------------------------------------------------------

/// Everything learnt at connect that the shared logic needs. Never contains secrets.
#[derive(Debug, Clone)]
pub struct ServerFacts {
    pub version: ServerVersion,
    pub caps: MysqlCaps,
    /// Connected with the pre-4.1 legacy client.
    pub legacy_protocol: bool,
    /// Connection (thread) id of this session.
    pub session_id: u32,
    /// 4.0: `character_set`; 4.1+: `character_set_server`.
    pub server_charset: Option<String>,
    /// 4.1+: `character_set_database` of the connected database.
    pub database_charset: Option<String>,
    /// How SQL is encoded and result text decoded on this connection.
    pub plan: TextPlan,
    /// Collation id -> charset (filled from `SHOW COLLATION` on `TextPlan::PassThrough`).
    pub collations: CollationMap,
    /// Quote / backslash lexing of the session (`@@sql_mode`; defaults on 4.0).
    pub lex: LexMode,
    pub user: Option<String>,
    pub database: Option<String>,
    /// `bulkdb-<job id>` / `bulkdb-ui`; used for the SQL tag comment.
    pub application_name: String,
    pub host: String,
    pub port: u16,
}

impl ServerFacts {
    /// Charset of one result column (`collation` = column definition charset id; `None` on
    /// the pre-4.1 protocol).
    pub fn column_charset(&self, collation: Option<u16>) -> Charset {
        self.plan.column_charset(collation, &self.collations)
    }

    /// `INVALID` when `sql` (the user's text, before any tag comment or `EXPLAIN` prefix is
    /// added) holds a character the connection's SQL charset cannot represent; the error names
    /// the character and its line / column in `sql`.
    pub fn check_sql_text(&self, sql: &str) -> Result<()> {
        let cs = self.plan.sql_charset();
        match find_unrepresentable(cs, sql) {
            Some(u) => Err(sql_encoding_error(cs, &u)),
            None => Ok(()),
        }
    }

    /// Encode UTF-8 SQL for the wire (`INVALID` on an unrepresentable character). ASCII is
    /// the same bytes in every charset BulkDB speaks, so it is sent as is.
    pub fn encode_sql<'a>(&self, sql: &'a str) -> Result<Cow<'a, [u8]>> {
        if sql.is_ascii() {
            return Ok(Cow::Borrowed(sql.as_bytes()));
        }
        encode_sql(self.plan.sql_charset(), sql)
    }

    /// Decode one metadata value: per-column charset, then the latin1 double-encoding repair
    /// when the plan asks for it (`TextPlan::repairs_metadata`) and the column is UTF-8.
    pub fn decode_meta(&self, collation: Option<u16>, bytes: &[u8]) -> String {
        let cs = self.column_charset(collation);
        let mut scratch = String::new();
        let text = decode_cell(cs, bytes, &mut scratch).text.to_string();
        if cs == Charset::Utf8 && self.plan.repairs_metadata() {
            if let Some(fixed) = repair_latin1_mojibake(&text, self.plan.sql_charset()) {
                return fixed;
            }
        }
        text
    }
}

/// One column of a [`MetaRows`] result.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MetaColumn {
    /// Column name (decoded with the connection charset).
    pub name: String,
    /// Collation id from the column definition; `None` on the pre-4.1 protocol.
    pub collation: Option<u16>,
}

/// A fully read metadata result: raw cell bytes as the server sent them (`None` = NULL).
/// Decode text with [`MetaRows::text`]; never used for table data.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MetaRows {
    pub columns: Vec<MetaColumn>,
    pub rows: Vec<Vec<Option<Vec<u8>>>>,
}

impl MetaRows {
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// Index of the column called `name` (ASCII case-insensitive). `SHOW` output differs
    /// between versions (4.0 `Type` vs 4.1.2+ `Engine`), so look columns up by name.
    pub fn col(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|c| c.name.eq_ignore_ascii_case(name))
    }

    pub fn raw(&self, row: usize, col: usize) -> Option<&[u8]> {
        self.rows.get(row)?.get(col)?.as_deref()
    }

    /// Decoded text of a cell (NULL -> `None`).
    pub fn text(&self, facts: &ServerFacts, row: usize, col: usize) -> Option<String> {
        let raw = self.raw(row, col)?;
        let collation = self.columns.get(col).and_then(|c| c.collation);
        Some(facts.decode_meta(collation, raw))
    }

    pub fn text_by(&self, facts: &ServerFacts, row: usize, name: &str) -> Option<String> {
        self.text(facts, row, self.col(name)?)
    }

    /// Unsigned integer cell (ASCII digits; a float is truncated).
    pub fn u64(&self, row: usize, col: usize) -> Option<u64> {
        let s = std::str::from_utf8(self.raw(row, col)?).ok()?.trim();
        s.parse::<u64>().ok().or_else(|| s.parse::<f64>().ok().filter(|f| *f >= 0.0).map(|f| f as u64))
    }

    pub fn u64_by(&self, row: usize, name: &str) -> Option<u64> {
        self.u64(row, self.col(name)?)
    }
}

/// Per-job ring of "the server is sending to the client" monitor samples.
#[derive(Debug, Default)]
pub struct WriteSamples(std::sync::Mutex<HashMap<String, VecDeque<bool>>>);

/// Ring size of [`WriteSamples`].
pub const WRITE_RING: usize = 10;

impl WriteSamples {
    /// Record one sample for `job_id`; returns the share of the last [`WRITE_RING`] samples
    /// where the server was writing.
    pub fn record(&self, job_id: &str, writing: bool) -> f32 {
        let mut map = self.0.lock().unwrap_or_else(|p| p.into_inner());
        let ring = map.entry(job_id.to_string()).or_default();
        ring.push_back(writing);
        while ring.len() > WRITE_RING {
            ring.pop_front();
        }
        ring.iter().filter(|w| **w).count() as f32 / ring.len() as f32
    }

    pub fn forget(&self, job_id: &str) {
        self.0.lock().unwrap_or_else(|p| p.into_inner()).remove(job_id);
    }
}

/// The small surface both MySQL clients implement so that catalog / monitor / probe /
/// session / EXPLAIN logic exists once. All SQL is UTF-8 here; implementations encode it with
/// `facts().plan.sql_charset()` (after the shared code already checked it) and decode nothing:
/// [`MetaRows`] carries raw bytes plus per-column collation ids.
#[async_trait]
pub trait MysqlRawQuery: Send + Sync {
    fn facts(&self) -> &ServerFacts;

    /// Text-protocol query on the UI/extraction connection, all rows read (at most
    /// [`META_ROW_LIMIT`]). `CONNECT` error while the connection is busy streaming.
    async fn meta_query(&self, sql: &str) -> Result<MetaRows>;

    /// Statement without a result set (`SET ...`, `KILL ...`).
    async fn meta_exec(&self, sql: &str) -> Result<()>;

    /// [`MysqlRawQuery::meta_query`] that honours `cancel`: the statement is killed from a
    /// side connection ([`MysqlRawQuery::kill_from_side`]) and `Cancelled` is returned within
    /// about 2 s. Used by `exact_count`.
    async fn meta_query_cancellable(&self, sql: &str, cancel: CancellationToken) -> Result<MetaRows>;

    /// Kill `target` from a new short-lived connection with the same parameters:
    /// [`kill_sql`] picks `KILL QUERY <id>` (5.0+) or `KILL <id>` (4.x, which ends the whole
    /// session). `ER_NO_SUCH_THREAD` counts as success. Bounded by a 10 s timeout.
    async fn kill_from_side(&self, target: u32) -> Result<()>;

    /// Monitor sample ring of this connection object.
    fn write_samples(&self) -> &WriteSamples;
}

// ---------------------------------------------------------------------------------------------
// Shared helpers (pure)
// ---------------------------------------------------------------------------------------------

/// `KILL QUERY <id>` on 5.0+, `KILL <id>` on 4.x.
pub fn kill_sql(caps: &MysqlCaps, id: u32) -> String {
    if caps.kill_query {
        format!("KILL QUERY {id}")
    } else {
        format!("KILL {id}")
    }
}

/// Single-quoted SQL string literal: `'` doubled, and `\` doubled when the session lexes
/// backslash escapes (4.0 always does). Used to inline catalog parameters on both clients.
pub fn quote_str(s: &str, lex: LexMode) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        match c {
            '\'' => out.push_str("''"),
            '\\' if lex.backslash_escapes => out.push_str("\\\\"),
            '\0' if lex.backslash_escapes => out.push_str("\\0"),
            c => out.push(c),
        }
    }
    out.push('\'');
    out
}

/// Map a server error (same numbering on every version, both clients) onto `CoreError`.
/// Pre-4.1 errors carry no SQLSTATE. 1317 -> `Cancelled`, 3024 / 1969 -> `Timeout`, 1045 /
/// 1044 / 1698 -> `Auth`, 1275 (secure_auth refuses an old hash) -> `Auth` with
/// [`SECURE_AUTH_HINT`], 1129 (host blocked) -> `Connect` with [`HOST_BLOCKED_HINT`], 1130
/// (host not allowed) -> `Connect` with [`HOST_NOT_ALLOWED_HINT`], 1040 (too many
/// connections) and the client codes 2002 / 2003 / 2006 / 2013 -> `Connect`, everything else
/// -> `Query`.
pub fn map_server_error(code: u16, sqlstate: Option<&str>, message: String) -> CoreError {
    match code {
        1317 => CoreError::Cancelled,
        3024 | 1969 => CoreError::Timeout(message),
        1045 | 1044 | 1698 => CoreError::Auth(format!("{message} ({code})")),
        1275 => CoreError::Auth(format!("{message} ({code}); {SECURE_AUTH_HINT}")),
        1129 => CoreError::Connect(format!("{message} ({code}); {HOST_BLOCKED_HINT}")),
        1130 => CoreError::Connect(format!("{message} ({code}); {HOST_NOT_ALLOWED_HINT}")),
        1040 | 2002 | 2003 | 2006 | 2013 => CoreError::Connect(format!("{message} ({code})")),
        _ => {
            let msg = match sqlstate {
                Some(state) => format!("{message} (error {code}, SQLSTATE {state})"),
                None => format!("{message} (error {code})"),
            };
            CoreError::Query { code: Some(code.to_string()), msg }
        }
    }
}

/// Column metadata common to both protocols (pre-4.1 `ColumnDefinition320` has no charset).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawColumnDef {
    pub name: String,
    /// `enum_field_types` code (`MYSQL_TYPE_*`).
    pub column_type: u8,
    pub flags: u16,
    pub decimals: u8,
    /// Display length (pre-4.1: 3 bytes, so at most 0xFFFFFF).
    pub length: u32,
    /// Collation id; `None` on the pre-4.1 protocol.
    pub collation: Option<u16>,
}

/// Column kind and decoder for a column described by raw metadata (text protocol), with the
/// same mapping as [`super::mysql::column_kind`]. Without a collation (pre-4.1) a BLOB type
/// with `BINARY_FLAG` is binary and every other string type is text (`CHAR BINARY` in 4.0 is
/// a case-sensitive text column).
pub fn column_kind_raw(def: &RawColumnDef) -> (ColKind, ValueParser) {
    use mysql_async::consts::{ColumnFlags, ColumnType};
    let ty = ColumnType::try_from(def.column_type).unwrap_or(ColumnType::MYSQL_TYPE_STRING);
    let flags = ColumnFlags::from_bits_truncate(def.flags);
    let charset = match def.collation {
        Some(c) => c,
        None => {
            let blob =
                matches!(ty, ColumnType::MYSQL_TYPE_TINY_BLOB | ColumnType::MYSQL_TYPE_MEDIUM_BLOB | ColumnType::MYSQL_TYPE_LONG_BLOB | ColumnType::MYSQL_TYPE_BLOB);
            if blob && flags.contains(ColumnFlags::BINARY_FLAG) {
                63
            } else {
                8
            }
        }
    };
    let col = mysql_async::Column::new(ty).with_flags(flags).with_decimals(def.decimals).with_column_length(def.length).with_character_set(charset);
    super::mysql::column_kind(&col)
}

/// One decoded text-protocol cell plus whether text decoding replaced invalid bytes.
#[derive(Debug, Clone, Copy)]
pub struct TextCell<'a> {
    pub cell: Cell<'a>,
    pub replaced: bool,
}

/// Decode one text-protocol value (`None` = SQL NULL) for `parser`; text is decoded with
/// `charset` (the column's charset from [`ServerFacts::column_charset`]). MySQL 4.0
/// `TIMESTAMP` values come without separators (`YYYYMMDDHHMMSS`, `YYMMDD`, ...); they are
/// accepted for `DateTime` columns ([`parse_compact_timestamp`]).
pub fn text_cell<'a>(parser: ValueParser, raw: Option<&'a [u8]>, charset: Charset, scratch: &'a mut String, col: usize) -> Result<TextCell<'a>> {
    let Some(b) = raw else { return Ok(TextCell { cell: Cell::Null, replaced: false }) };
    let bad = |what: &str| CoreError::Internal(format!("column {col}: '{}' is not a valid {what}", String::from_utf8_lossy(b)));
    let plain = |cell: Cell<'a>| Ok(TextCell { cell, replaced: false });
    match parser {
        ValueParser::Int => parse_i64(b).map(Cell::I64).map_or_else(|| Err(bad("integer")), plain),
        ValueParser::Bit => parse_bit(b).map(Cell::I64).map_or_else(|| Err(bad("BIT value")), plain),
        ValueParser::Float => std::str::from_utf8(b).ok().and_then(|s| s.trim().parse::<f64>().ok()).map(Cell::F64).map_or_else(|| Err(bad("number")), plain),
        ValueParser::Decimal { scale } => parse_decimal(b, scale).map(Cell::Decimal).map_or_else(|| Err(bad("decimal")), plain),
        ValueParser::Binary => plain(Cell::Bytes(b)),
        ValueParser::Time { .. } => plain(Cell::Bytes(b)),
        ValueParser::Date => match parse_date_days(b) {
            Some(d) => plain(d.map(Cell::Date).unwrap_or(Cell::Null)),
            None => Err(bad("date")),
        },
        ValueParser::DateTime => match parse_datetime_micros(b).or_else(|| parse_compact_timestamp(b)) {
            Some(t) => plain(t.map(Cell::Timestamp).unwrap_or(Cell::Null)),
            None => Err(bad("datetime")),
        },
        ValueParser::Text => {
            let d = decode_cell(charset, b, scratch);
            Ok(TextCell { cell: Cell::Str(d.text), replaced: d.replaced })
        }
    }
}

/// MySQL 4.0 `TIMESTAMP(M)` display formats (digits only): 14 `YYYYMMDDHHMMSS`,
/// 12 `YYMMDDHHMMSS`, 10 `YYMMDDHHMM`, 8 `YYYYMMDD`, 6 `YYMMDD`, 4 `YYMM`, 2 `YY`. Two-digit
/// years: 70-99 -> 19xx, 00-69 -> 20xx. All zeros -> `Some(None)` (zero timestamp = NULL).
/// `None` when `b` is not such a value.
pub fn parse_compact_timestamp(b: &[u8]) -> Option<Option<i64>> {
    if b.is_empty() || !b.iter().all(|c| c.is_ascii_digit()) {
        return None;
    }
    if b.iter().all(|c| *c == b'0') {
        return Some(None);
    }
    let num = |s: &[u8]| -> u32 { s.iter().fold(0u32, |acc, c| acc * 10 + (c - b'0') as u32) };
    let yy = |s: &[u8]| -> i32 {
        let y = num(s) as i32;
        if y >= 70 {
            1900 + y
        } else {
            2000 + y
        }
    };
    let (y, rest) = match b.len() {
        14 | 8 => (num(&b[..4]) as i32, &b[4..]),
        12 | 10 | 6 | 4 | 2 => (yy(&b[..2]), &b[2..]),
        _ => return None,
    };
    let part = |i: usize, default: u32| -> u32 { rest.get(i * 2..i * 2 + 2).map(num).unwrap_or(default) };
    let (m, d, h, i, s) = (part(0, 1), part(1, 1), part(2, 0), part(3, 0), part(4, 0));
    Some(ymdhms_to_micros(y, m.max(1), d.max(1), h, i, s, 0))
}

// ---------------------------------------------------------------------------------------------
// Version-aware operations (owner: modern)
// ---------------------------------------------------------------------------------------------

/// Planner-only EXPLAIN limits: statement timeout (where the server has one) and metadata
/// lock wait (where `lock_wait_timeout` exists).
pub const EXPLAIN_STATEMENT_TIMEOUT_MS: u64 = 15_000;
pub const EXPLAIN_LOCK_TIMEOUT_S: u64 = 3;
/// `ER_PARSE_ERROR`: SQL syntax error (also: an optional statement an old server lacks).
pub const ER_PARSE_ERROR: u16 = 1064;
/// `ER_NO_SUCH_TABLE`: catalog lookup of a table that does not exist.
pub const ER_NO_SUCH_TABLE: u16 = 1146;
/// `ER_BAD_DB_ERROR`: unknown database.
pub const ER_BAD_DB_ERROR: u16 = 1049;

/// Server error number carried by a `CoreError::Query` (from either client).
pub fn server_error_code(e: &CoreError) -> Option<u16> {
    match e {
        CoreError::Query { code: Some(c), .. } => c.parse().ok(),
        _ => None,
    }
}

/// `INVALID` unless `sql` is a single statement under the session's quoting rules. Text that
/// is a single statement under every `sql_mode` passes without looking at `lex`.
pub fn ensure_single_statement(sql: &str, lex: LexMode) -> Result<()> {
    if may_have_statement_separator(sql) && has_statement_separator(sql, lex) {
        return Err(CoreError::Invalid("the SQL must be a single statement: a ';' separates it into several, which MySQL would run one after the other".into()));
    }
    Ok(())
}

/// Variables the probe reports on 5.0.3+ (one `SHOW VARIABLES WHERE`); each appears only when
/// the server has it.
const PROBE_VARIABLES: &[&str] = &[
    "innodb_buffer_pool_size",
    "key_buffer_size",
    "max_execution_time",
    "max_statement_time",
    "net_read_timeout",
    "net_write_timeout",
    "max_allowed_packet",
    "wait_timeout",
    "version_comment",
    "sql_mode",
    "character_set_server",
    "character_set_database",
    "read_only",
    "transaction_isolation",
    "tx_isolation",
    "innodb_page_size",
    "have_innodb",
];

/// Variables asked for one at a time (`SHOW VARIABLES LIKE`) on servers without
/// `SHOW VARIABLES WHERE` (4.0 - 5.0.2). `character_set` is the 4.0 name, the
/// `character_set_*` pair the 4.1+ one; only the names the version knows are asked for.
const PROBE_VARIABLES_LEGACY: &[&str] = &[
    "version_comment",
    "innodb_buffer_pool_size",
    "have_innodb",
    "key_buffer_size",
    "max_allowed_packet",
    "net_read_timeout",
    "net_write_timeout",
    "wait_timeout",
    "sql_mode",
    "character_set",
    "character_set_server",
    "character_set_database",
    "tx_isolation",
    "read_only",
];

/// The variables [`probe`] asks for on this server, and whether it can ask in one statement.
pub fn probe_variables(caps: &MysqlCaps) -> (bool, Vec<&'static str>) {
    if caps.show_where {
        return (true, PROBE_VARIABLES.to_vec());
    }
    let names = PROBE_VARIABLES_LEGACY
        .iter()
        .copied()
        .filter(|n| match *n {
            "character_set" => !caps.set_names,
            "character_set_server" | "character_set_database" => caps.set_names,
            _ => true,
        })
        .collect();
    (false, names)
}

/// `SHOW VARIABLES` for [`probe_variables`] -> name -> value.
async fn read_variables(q: &dyn MysqlRawQuery) -> Result<BTreeMap<String, String>> {
    let f = q.facts();
    let mut out = BTreeMap::new();
    let put = |out: &mut BTreeMap<String, String>, rows: &MetaRows, only: Option<&str>| {
        for r in 0..rows.len() {
            let Some(name) = rows.text(f, r, 0) else { continue };
            if only.is_some_and(|o| !name.eq_ignore_ascii_case(o)) {
                continue;
            }
            out.insert(name, rows.text(f, r, 1).unwrap_or_default());
        }
    };
    match probe_variables(&f.caps) {
        (true, names) => {
            let list = names.iter().map(|v| quote_str(v, f.lex)).collect::<Vec<_>>().join(",");
            let rows = q.meta_query(&format!("SHOW VARIABLES WHERE Variable_name IN ({list})")).await?;
            put(&mut out, &rows, None);
        }
        (false, names) => {
            for name in names {
                match q.meta_query(&format!("SHOW VARIABLES LIKE {}", quote_str(name, f.lex))).await {
                    Ok(rows) => put(&mut out, &rows, Some(name)),
                    Err(e @ CoreError::Connect(_)) => return Err(e),
                    Err(e) => tracing::debug!("mysql probe: SHOW VARIABLES LIKE '{name}' failed: {e}"),
                }
            }
        }
    }
    Ok(out)
}

/// `SHOW GRANTS` lines of the current account. 4.1.2+ knows the bare form; older servers need
/// the account spelled out (`CURRENT_USER()` = `user@host`).
async fn read_grants(q: &dyn MysqlRawQuery, current_user: Option<&str>) -> Result<Vec<String>> {
    let f = q.facts();
    let sql = if f.version.at_least(4, 1, 2) {
        "SHOW GRANTS".to_string()
    } else {
        let (user, host) =
            current_user.and_then(|u| u.rsplit_once('@')).ok_or_else(|| CoreError::Invalid("the account name is unknown, so its grants cannot be listed".into()))?;
        format!("SHOW GRANTS FOR {}@{}", quote_str(user, f.lex), quote_str(host, f.lex))
    };
    let rows = q.meta_query(&sql).await?;
    Ok((0..rows.len()).filter_map(|r| rows.text(f, r, 0)).collect())
}

/// RAM estimate: `innodb_buffer_pool_size` x 1.33 (InnoDB present), else `key_buffer_size` x 4
/// (MyISAM-only 4.x servers, where the manual recommends a quarter of the RAM for it).
pub fn ram_estimate(settings: &BTreeMap<String, String>) -> (Option<u64>, Option<String>) {
    let num = |k: &str| settings.get(k).and_then(|v| v.trim().parse::<u64>().ok()).filter(|v| *v > 0);
    let innodb_on = settings.get("have_innodb").is_none_or(|h| !(h.eq_ignore_ascii_case("NO") || h.eq_ignore_ascii_case("DISABLED")));
    if let Some(p) = num("innodb_buffer_pool_size").filter(|_| innodb_on) {
        return (Some(p / 3 * 4 + (p % 3) * 4 / 3), Some("innodb_buffer_pool_size x 1.33".to_string()));
    }
    match num("key_buffer_size") {
        Some(k) => (Some(k.saturating_mul(4)), Some("key_buffer_size x 4".to_string())),
        None => (None, None),
    }
}

/// The `plan_text` note for this connection, when the plan is the one `auto` would pick
/// (latin1 hint, unsupported 4.0 charset).
fn plan_note(f: &ServerFacts) -> Option<String> {
    let (auto_plan, note) = plan_text(EncodingSetting::Auto, &f.version, f.legacy_protocol, f.server_charset.as_deref(), f.database_charset.as_deref());
    if auto_plan == f.plan {
        note
    } else {
        None
    }
}

/// Probe (design doc section 4, "probe"): version, user, database, charsets
/// (`settings["character_set_server"]`, `settings["text_plan"]`, `settings["protocol"]`),
/// RAM estimate ([`ram_estimate`]), `SHOW GRANTS` -> `can_write`, notes (legacy protocol, latin1
/// hint, no server-side statement timeout). 4.x: `SHOW VARIABLES LIKE '...'` per variable;
/// 5.0.3+: one `SHOW VARIABLES WHERE`.
pub async fn probe(q: &dyn MysqlRawQuery) -> Result<ServerInfo> {
    let f = q.facts();
    let caps = f.caps;
    // CURRENT_USER() exists since 4.0.6; USER() is the fallback for older servers.
    let head = match q.meta_query("SELECT VERSION(), CURRENT_USER(), DATABASE()").await {
        Ok(rows) => rows,
        Err(e) if server_error_code(&e).is_some() && !caps.protocol_41 => q.meta_query("SELECT VERSION(), USER(), DATABASE()").await?,
        Err(e) => return Err(e),
    };
    let version_raw = head.text(f, 0, 0).filter(|v| !v.is_empty()).unwrap_or_else(|| f.version.raw.clone());
    let user = head.text(f, 0, 1).filter(|u| !u.is_empty());
    let database = head.text(f, 0, 2).filter(|d| !d.is_empty());
    let version = ServerVersion::parse(&version_raw);

    let mut settings = read_variables(q).await?;
    let (ram_bytes, ram_source) = ram_estimate(&settings);

    let mut notes = Vec::new();
    let can_write = match read_grants(q, user.as_deref()).await {
        Ok(lines) => Some(grants_allow_write(&lines)),
        Err(e @ CoreError::Connect(_)) => return Err(e),
        Err(e) => {
            notes.push(format!("SHOW GRANTS failed: {e}"));
            None
        }
    };
    notes.push(format!("{} {}.{}.{}", version.label(), version.major, version.minor, version.patch));
    if f.legacy_protocol {
        notes.push("pre-4.1 server: connected with the BulkDB legacy MySQL client (no TLS, no prepared statements)".into());
    }
    if let Some(note) = plan_note(f) {
        notes.push(note);
    }
    if !caps.max_execution_time && !caps.max_statement_time {
        notes.push("this server version has no statement timeout (MySQL 5.7.8+ / MariaDB 10.1+ have one): a time limit cannot be enforced server-side".into());
    }
    if ram_bytes.is_none() {
        notes.push("neither innodb_buffer_pool_size nor key_buffer_size is visible; server RAM unknown".into());
    }

    settings.insert("protocol".into(), if f.legacy_protocol { "legacy (pre-4.1)".into() } else { "modern".into() });
    settings.insert("text_plan".into(), f.plan.describe());
    if let Some(cs) = &f.server_charset {
        settings.entry("character_set_server".into()).or_insert_with(|| cs.clone());
    }
    if let Some(cs) = &f.database_charset {
        settings.entry("character_set_database".into()).or_insert_with(|| cs.clone());
    }

    Ok(ServerInfo {
        driver: "mysql".into(),
        version: version_raw,
        user: user.or_else(|| f.user.clone()),
        database: database.or_else(|| f.database.clone()),
        ram_bytes,
        ram_source,
        settings,
        can_write,
        notes,
    })
}

/// `SET` statements for an extraction session, in order (design doc section 4, "session"):
/// `SET SESSION TRANSACTION READ ONLY` (caps.read_only_tx), the statement timeout
/// (`max_execution_time` / `max_statement_time` when supported, else none),
/// `net_read_timeout` / `net_write_timeout` (all versions).
pub fn session_statements(facts: &ServerFacts, opts: &SessionOpts) -> Vec<String> {
    let caps = facts.caps;
    let mut out = Vec::new();
    // Every later transaction of this (extraction) session is read only, so even a statement
    // that slipped past sqlguard cannot change data.
    if caps.read_only_tx {
        out.push("SET SESSION TRANSACTION READ ONLY".to_string());
    }
    if let Some(ms) = opts.statement_timeout_ms {
        if caps.max_execution_time {
            out.push(format!("SET SESSION max_execution_time = {ms}"));
        } else if caps.max_statement_time {
            out.push(format!("SET SESSION max_statement_time = {}", ms as f64 / 1000.0));
        }
    }
    let net = std::cmp::max(600, opts.statement_timeout_ms.unwrap_or(0) / 1000);
    out.push(format!("SET SESSION net_read_timeout = {net}"));
    out.push(format!("SET SESSION net_write_timeout = {net}"));
    out
}

/// Run [`session_statements`]; `ER_UNKNOWN_SYSTEM_VARIABLE` and parse errors (a fork or an
/// old server without the option) are logged and ignored.
pub async fn apply_session(q: &dyn MysqlRawQuery, opts: &SessionOpts) -> Result<()> {
    let f = q.facts();
    if opts.statement_timeout_ms.is_some() && !f.caps.max_execution_time && !f.caps.max_statement_time {
        tracing::debug!("mysql {}: no statement timeout on this version; the requested timeout is not enforced server-side", f.version.raw);
    }
    if !f.caps.read_only_tx {
        tracing::debug!("mysql {}: SET SESSION TRANSACTION READ ONLY is not supported; the extraction session stays read-write", f.version.raw);
    }
    for s in session_statements(f, opts) {
        match q.meta_exec(&s).await {
            Ok(()) => {}
            Err(e) if matches!(server_error_code(&e), Some(ER_UNKNOWN_SYSTEM_VARIABLE | ER_PARSE_ERROR)) => {
                tracing::warn!("mysql {}: ignoring unsupported session option `{s}`: {e}", f.version.raw)
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// MySQL before 5.6.3 (MariaDB before 5.3) runs subqueries while it explains a statement:
/// derived tables are materialized and constant subqueries evaluated, so an EXPLAIN can cost
/// as much as the query itself.
pub fn explain_runs_subqueries(v: &ServerVersion) -> bool {
    match v.flavor {
        Flavor::MySql => !v.at_least(5, 6, 3),
        Flavor::MariaDb => !v.at_least(5, 3, 0),
    }
}

/// Whether `sql` contains `( SELECT` anywhere (conservative: a string literal counts too).
pub fn has_subquery(sql: &str) -> bool {
    let b = sql.as_bytes();
    let ident = |c: &u8| c.is_ascii_alphanumeric() || *c == b'_' || *c == b'$';
    for (i, _) in b.iter().enumerate().filter(|(_, c)| **c == b'(') {
        let mut j = i + 1;
        while j < b.len() && b[j].is_ascii_whitespace() {
            j += 1;
        }
        if b.get(j..j + 6).is_some_and(|w| w.eq_ignore_ascii_case(b"select")) && !b.get(j + 6).is_some_and(ident) {
            return true;
        }
    }
    false
}

/// How [`explain`] asks for a plan on a server version.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExplainMode {
    /// `EXPLAIN FORMAT=JSON` (MySQL 5.6.5+ / MariaDB 10.1+).
    Json,
    /// Tabular `EXPLAIN` parsed by [`parse_explain_tabular`].
    Tabular,
}

impl ExplainMode {
    pub fn for_caps(caps: &MysqlCaps) -> ExplainMode {
        if caps.explain_json {
            ExplainMode::Json
        } else {
            ExplainMode::Tabular
        }
    }
}

/// Run a planner-only statement with short metadata-lock / statement timeouts where the
/// server has those variables, restoring the session values afterwards.
async fn with_short_timeouts(q: &dyn MysqlRawQuery, sql: &str) -> Result<MetaRows> {
    let f = q.facts();
    let caps = f.caps;
    let mut vars: Vec<&str> = Vec::new();
    if caps.lock_wait_timeout {
        vars.push("lock_wait_timeout");
    }
    if caps.max_execution_time {
        vars.push("max_execution_time");
    } else if caps.max_statement_time {
        vars.push("max_statement_time");
    }
    if vars.is_empty() {
        return q.meta_query(sql).await;
    }
    let select = vars.iter().map(|v| format!("@@session.{v}")).collect::<Vec<_>>().join(", ");
    // Saved values are re-validated as numbers before they go back into SQL.
    let saved: Vec<Option<String>> = match q.meta_query(&format!("SELECT {select}")).await {
        Ok(rows) => (0..vars.len()).map(|i| rows.text(f, 0, i).filter(|v| v.trim().parse::<f64>().is_ok())).collect(),
        Err(e) => {
            tracing::debug!("mysql explain: reading the session timeouts failed: {e}");
            vec![None; vars.len()]
        }
    };
    let short = |v: &str| match v {
        "lock_wait_timeout" => format!("SESSION lock_wait_timeout = {EXPLAIN_LOCK_TIMEOUT_S}"),
        "max_execution_time" => format!("SESSION max_execution_time = {EXPLAIN_STATEMENT_TIMEOUT_MS}"),
        _ => format!("SESSION max_statement_time = {}", EXPLAIN_STATEMENT_TIMEOUT_MS as f64 / 1000.0),
    };
    let set = vars.iter().map(|v| short(v)).collect::<Vec<_>>().join(", ");
    if let Err(e) = q.meta_exec(&format!("SET {set}")).await {
        tracing::debug!("mysql explain: shortening the session timeouts failed: {e}");
    }
    let out = q.meta_query(sql).await;
    let restore: Vec<String> = vars.iter().zip(&saved).filter_map(|(v, s)| s.as_ref().map(|s| format!("SESSION {v} = {}", s.trim()))).collect();
    if !restore.is_empty() {
        if let Err(e) = q.meta_exec(&format!("SET {}", restore.join(", "))).await {
            tracing::debug!("mysql explain: restoring the session timeouts failed: {e}");
        }
    }
    out
}

fn plan_summary(parsed: ExplainParsed, raw: String, raw_format: &str, analyzed: bool, started: Instant) -> PlanSummary {
    PlanSummary {
        estimated_rows: parsed.estimated_rows,
        estimated_width: None,
        total_cost: parsed.total_cost,
        estimated_bytes: None,
        analyzed,
        elapsed_ms: started.elapsed().as_millis() as u64,
        warnings: vec![],
        tables: parsed.tables,
        root: parsed.root,
        raw,
        raw_format: raw_format.into(),
    }
}

/// First column of every row, one line each (`EXPLAIN FORMAT=JSON`, `EXPLAIN ANALYZE`).
fn first_column_text(rows: &MetaRows, f: &ServerFacts) -> String {
    (0..rows.len()).filter_map(|r| rows.text(f, r, 0)).collect::<Vec<_>>().join("\n")
}

/// EXPLAIN (design doc section 4, "explain"): single-statement check under `facts.lex`,
/// then `EXPLAIN FORMAT=JSON` (caps.explain_json) or tabular `EXPLAIN` parsed by
/// [`parse_explain_tabular`], both with short lock / statement timeouts where the variables
/// exist; `analyze = true` needs caps.explain_analyze, else `INVALID`. On servers that run
/// subqueries while explaining ([`explain_runs_subqueries`]) a statement with a subquery is
/// not explained at all (`INVALID`), so preflight never runs the heavy part twice.
pub async fn explain(q: &dyn MysqlRawQuery, sql: &str, analyze: bool) -> Result<Option<PlanSummary>> {
    let started = Instant::now();
    let f = q.facts();
    // Before the `EXPLAIN` prefix is added, so an unrepresentable character is reported at its
    // line / column in the caller's SQL.
    f.check_sql_text(sql)?;
    let body = trim_statement(sql);
    // EXPLAIN goes over the text protocol, where everything after a `;` would run for real
    // (and EXPLAIN ANALYZE executes the query itself).
    ensure_single_statement(body, f.lex)?;
    if analyze {
        if !f.caps.explain_analyze {
            return Err(CoreError::Invalid(format!("EXPLAIN ANALYZE needs MySQL 8.0.18+ or MariaDB 10.1+ (server is {})", f.version.raw)));
        }
        return match f.version.flavor {
            Flavor::MySql => {
                let rows = q.meta_query(&format!("EXPLAIN ANALYZE {body}")).await?;
                let raw = first_column_text(&rows, f);
                let parsed = ExplainParsed { estimated_rows: explain_analyze_root_rows(&raw), ..Default::default() };
                Ok(Some(plan_summary(parsed, raw, "text", true, started)))
            }
            Flavor::MariaDb => {
                let rows = q.meta_query(&format!("ANALYZE FORMAT=JSON {body}")).await?;
                let raw = first_column_text(&rows, f);
                Ok(Some(plan_summary(parse_explain_json(&raw)?, raw, "json", true, started)))
            }
        };
    }
    if explain_runs_subqueries(&f.version) && has_subquery(body) {
        return Err(CoreError::Invalid(format!(
            "EXPLAIN skipped: {} {} executes subqueries while explaining a statement, which would run them a second time on the server",
            f.version.label(),
            f.version.raw
        )));
    }
    match ExplainMode::for_caps(&f.caps) {
        ExplainMode::Json => {
            let rows = with_short_timeouts(q, &format!("EXPLAIN FORMAT=JSON {body}")).await?;
            let raw = first_column_text(&rows, f);
            Ok(Some(plan_summary(parse_explain_json(&raw)?, raw, "json", false, started)))
        }
        ExplainMode::Tabular => {
            let rows = with_short_timeouts(q, &format!("EXPLAIN {body}")).await?;
            let parsed = parse_explain_tabular(&rows, f);
            Ok(Some(plan_summary(parsed, render_tabular(&rows, f), "text", false, started)))
        }
    }
}

/// Tabular EXPLAIN as text: a header line and one line per row, tab separated, NULL as `NULL`.
pub fn render_tabular(rows: &MetaRows, f: &ServerFacts) -> String {
    let mut out = rows.columns.iter().map(|c| c.name.as_str()).collect::<Vec<_>>().join("\t");
    for r in 0..rows.len() {
        out.push('\n');
        let cells: Vec<String> = (0..rows.columns.len()).map(|c| rows.text(f, r, c).unwrap_or_else(|| "NULL".into())).collect();
        out.push_str(&cells.join("\t"));
    }
    out
}

fn clip(s: String, max: usize) -> String {
    if s.chars().count() <= max {
        s
    } else {
        let mut t: String = s.chars().take(max).collect();
        t.push('…');
        t
    }
}

/// Tabular `EXPLAIN` (4.0: `table,type,possible_keys,key,key_len,ref,rows,Extra`; 4.1-5.6:
/// plus `id,select_type` and optional `partitions,filtered`) -> plan tree, estimated rows
/// (product of `rows` along the join; `filtered` applied when present) and tables. Columns
/// are looked up by name.
///
/// The tree mirrors the JSON one: a `query_block` root whose children are the table nodes of
/// the first select (node type = access type: `ALL`, `ref`, `range`, ...), then one node per
/// further select (`derived`, `subquery`, `union`, ...; `parent_relationship = SubPlan`).
/// For a `UNION` the estimate is the sum of its selects; "Impossible WHERE" estimates 0.
pub fn parse_explain_tabular(rows: &MetaRows, facts: &ServerFacts) -> super::mysql::ExplainParsed {
    struct Block {
        id: Option<String>,
        select_type: String,
        children: Vec<PlanNode>,
        product: Option<f64>,
        impossible: bool,
    }
    let c = |name: &str| rows.col(name);
    let c_id = c("id");
    let c_sel = c("select_type");
    let c_table = c("table");
    let c_type = c("type");
    let c_rows = c("rows");
    let c_filtered = c("filtered");
    let c_extra = c("Extra");
    let extras = [
        ("select_type", c_sel),
        ("possible_keys", c("possible_keys")),
        ("key", c("key")),
        ("key_length", c("key_len")),
        ("ref", c("ref")),
        ("rows_examined_per_scan", c_rows),
        ("filtered", c_filtered),
        ("partitions", c("partitions")),
        ("extra", c_extra),
    ];

    let mut blocks: Vec<Block> = Vec::new();
    let mut tables: Vec<TableRef> = Vec::new();
    for r in 0..rows.len() {
        let text = |col: Option<usize>| col.and_then(|i| rows.text(facts, r, i)).map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
        let id = text(c_id);
        let select_type = text(c_sel).unwrap_or_else(|| "SIMPLE".to_string());
        let examined = text(c_rows).and_then(|s| s.parse::<f64>().ok());
        let filtered = text(c_filtered).and_then(|s| s.parse::<f64>().ok());
        let impossible = text(c_extra).is_some_and(|e| e.contains("Impossible") || e.contains("no matching row"));

        let mut node = PlanNode { node_type: text(c_type).unwrap_or_else(|| "table".to_string()), ..Default::default() };
        if let Some(name) = text(c_table) {
            let tr = TableRef { database: None, schema: None, name };
            if !tr.name.starts_with('<') && !tables.contains(&tr) {
                tables.push(tr.clone());
            }
            node.relation = Some(tr);
        }
        node.plan_rows = examined.map(|n| n.max(0.0) * filtered.unwrap_or(100.0).clamp(0.0, 100.0) / 100.0);
        for (key, col) in extras {
            if let Some(v) = text(col) {
                node.extra.insert(key.to_string(), clip(v, 500));
            }
        }

        // Rows of one select share an id; 4.0 has no id column (one select); a NULL id is the
        // `UNION RESULT` row, a block of its own.
        let slot = match (&id, c_id) {
            (Some(id), _) => blocks.iter().position(|b| b.id.as_deref() == Some(id.as_str())),
            (None, None) => (!blocks.is_empty()).then_some(0),
            (None, Some(_)) => None,
        };
        let i = match slot {
            Some(i) => i,
            None => {
                blocks.push(Block { id: id.clone(), select_type, children: Vec::new(), product: None, impossible: false });
                blocks.len() - 1
            }
        };
        let b = &mut blocks[i];
        if let Some(n) = node.plan_rows {
            b.product = Some(b.product.unwrap_or(1.0) * n);
        }
        b.impossible |= impossible;
        b.children.push(node);
    }

    let mut iter = blocks.into_iter();
    let Some(first) = iter.next() else { return super::mysql::ExplainParsed::default() };
    let rows_of = |b: &Block| if b.impossible { Some(0.0) } else { b.product };
    let first_rows = rows_of(&first);
    let mut union_rows = first_rows;
    let mut has_union = false;
    let mut root = PlanNode { node_type: "query_block".into(), plan_rows: first_rows, ..Default::default() };
    root.extra.insert("select_type".into(), first.select_type.clone());
    root.children = first.children;
    for b in iter {
        let rows = rows_of(&b);
        if b.select_type.eq_ignore_ascii_case("UNION") {
            has_union = true;
            union_rows = match (union_rows, rows) {
                (Some(a), Some(r)) => Some(a + r),
                (a, r) => a.or(r),
            };
        }
        let mut node = PlanNode { node_type: b.select_type.to_ascii_lowercase(), plan_rows: rows, parent_relationship: Some("SubPlan".into()), ..Default::default() };
        node.children = b.children;
        if let Some(id) = b.id {
            node.extra.insert("id".into(), id);
        }
        root.children.push(node);
    }
    let estimate = if has_union { union_rows } else { first_rows };
    root.plan_rows = estimate;
    super::mysql::ExplainParsed { root: Some(root), estimated_rows: estimate.map(|r| r.max(0.0).round() as u64), total_cost: None, tables }
}

/// Test double of [`MysqlRawQuery`] with canned replies, shared by the version-matrix tests
/// here and in `catalog::mysql` / `monitor::mysql`.
#[cfg(test)]
pub(crate) mod test_support {
    use std::sync::Mutex;

    use super::*;

    /// Facts for a server reporting `version` (pre-4.1 versions get the legacy protocol).
    pub(crate) fn facts(version: &str, plan: TextPlan) -> ServerFacts {
        let version = ServerVersion::parse(version);
        let caps = MysqlCaps::for_version(&version);
        ServerFacts {
            legacy_protocol: !caps.protocol_41,
            caps,
            version,
            session_id: 7,
            server_charset: None,
            database_charset: None,
            plan,
            collations: CollationMap::builtin(),
            lex: LexMode::default(),
            user: Some("app".into()),
            database: Some("shop".into()),
            application_name: "bulkdb-test".into(),
            host: "db.test".into(),
            port: 3306,
        }
    }

    pub(crate) fn utf8() -> TextPlan {
        TextPlan::ServerUtf8 { names: "utf8mb4" }
    }

    /// A result whose columns carry no collation id (as on the pre-4.1 protocol).
    pub(crate) fn rows(columns: &[&str], data: Vec<Vec<Option<&[u8]>>>) -> MetaRows {
        MetaRows {
            columns: columns.iter().map(|c| MetaColumn { name: c.to_string(), collation: None }).collect(),
            rows: data.into_iter().map(|r| r.into_iter().map(|c| c.map(<[u8]>::to_vec)).collect()).collect(),
        }
    }

    /// A result of text cells; `"\\N"` is NULL.
    pub(crate) fn text_rows(columns: &[&str], data: &[&[&str]]) -> MetaRows {
        rows(columns, data.iter().map(|r| r.iter().map(|c| (*c != "\\N").then_some(c.as_bytes())).collect()).collect())
    }

    /// Answers each statement with the reply of the first rule whose needle it contains
    /// (unmatched statements get an empty result) and records every statement.
    pub(crate) struct Canned {
        pub facts: ServerFacts,
        replies: Vec<(String, std::result::Result<MetaRows, u16>)>,
        log: Mutex<Vec<String>>,
        samples: WriteSamples,
    }

    impl Canned {
        pub(crate) fn new(facts: ServerFacts) -> Self {
            Canned { facts, replies: Vec::new(), log: Mutex::new(Vec::new()), samples: WriteSamples::default() }
        }

        pub(crate) fn on(mut self, needle: &str, rows: MetaRows) -> Self {
            self.replies.push((needle.to_string(), Ok(rows)));
            self
        }

        /// Statements containing `needle` fail with server error `code`.
        pub(crate) fn fail(mut self, needle: &str, code: u16) -> Self {
            self.replies.push((needle.to_string(), Err(code)));
            self
        }

        pub(crate) fn log(&self) -> Vec<String> {
            self.log.lock().unwrap().clone()
        }

        fn answer(&self, sql: &str) -> Result<MetaRows> {
            self.log.lock().unwrap().push(sql.to_string());
            match self.replies.iter().find(|(needle, _)| sql.contains(needle.as_str())) {
                Some((_, Ok(rows))) => Ok(rows.clone()),
                Some((_, Err(code))) => Err(map_server_error(*code, None, format!("canned error for `{sql}`"))),
                None => Ok(MetaRows::default()),
            }
        }
    }

    #[async_trait]
    impl MysqlRawQuery for Canned {
        fn facts(&self) -> &ServerFacts {
            &self.facts
        }

        async fn meta_query(&self, sql: &str) -> Result<MetaRows> {
            self.answer(sql)
        }

        async fn meta_exec(&self, sql: &str) -> Result<()> {
            self.answer(sql).map(|_| ())
        }

        async fn meta_query_cancellable(&self, sql: &str, _cancel: CancellationToken) -> Result<MetaRows> {
            self.answer(sql)
        }

        async fn kill_from_side(&self, target: u32) -> Result<()> {
            self.answer(&kill_sql(&self.facts.caps, target)).map(|_| ())
        }

        fn write_samples(&self) -> &WriteSamples {
            &self.samples
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{facts, rows, text_rows, utf8, Canned};
    use super::*;

    #[test]
    fn caps_by_version() {
        let v40 = MysqlCaps::for_version(&ServerVersion::parse("4.0.27-standard-log"));
        assert!(!v40.protocol_41 && !v40.information_schema && !v40.kill_query && !v40.utf8mb4);
        let v55 = MysqlCaps::for_version(&ServerVersion::parse("5.5.62"));
        assert!(v55.protocol_41 && v55.information_schema && v55.utf8mb4 && !v55.explain_json && !v55.read_only_tx && v55.old_password_hashes);
        let v80 = MysqlCaps::for_version(&ServerVersion::parse("8.0.36"));
        assert!(v80.explain_json && v80.max_execution_time && v80.explain_analyze && !v80.old_password_hashes);
        let maria = MysqlCaps::for_version(&ServerVersion::parse("5.5.5-10.6.12-MariaDB"));
        assert!(maria.max_statement_time && maria.explain_json && !maria.max_execution_time);
    }

    #[test]
    fn literals_and_kill() {
        assert_eq!(quote_str("a'b\\c", LexMode::default()), "'a''b\\\\c'");
        assert_eq!(quote_str("a\\b", LexMode { backslash_escapes: false, ansi_quotes: false }), "'a\\b'");
        let v40 = MysqlCaps::for_version(&ServerVersion::parse("4.0.27"));
        assert_eq!(kill_sql(&v40, 7), "KILL 7");
        let v41 = MysqlCaps::for_version(&ServerVersion::parse("4.1.22"));
        assert_eq!(kill_sql(&v41, 7), "KILL 7", "4.1 has no KILL QUERY either");
        let v50 = MysqlCaps::for_version(&ServerVersion::parse("5.0.96"));
        assert_eq!(kill_sql(&v50, 7), "KILL QUERY 7");
    }

    #[test]
    fn compact_timestamps() {
        let full = parse_compact_timestamp(b"20031231235959").unwrap().unwrap();
        assert_eq!(Some(full), parse_datetime_micros(b"2003-12-31 23:59:59").unwrap());
        assert_eq!(parse_compact_timestamp(b"00000000000000"), Some(None));
        assert_eq!(parse_compact_timestamp(b"031231"), Some(parse_datetime_micros(b"2003-12-31 00:00:00").unwrap()));
        assert_eq!(parse_compact_timestamp(b"2003-12-31"), None);
    }

    #[test]
    fn legacy_column_kinds() {
        let def = |ty: u8, flags: u16| RawColumnDef { name: "c".into(), column_type: ty, flags, decimals: 0, length: 10, collation: None };
        // 252 = BLOB; 128 = BINARY_FLAG; 253 = VAR_STRING.
        assert_eq!(column_kind_raw(&def(252, 128)).0, ColKind::Binary);
        assert_eq!(column_kind_raw(&def(252, 0)).0, ColKind::Utf8);
        assert_eq!(column_kind_raw(&def(253, 128)).0, ColKind::Utf8);
        assert_eq!(column_kind_raw(&def(3, 0)).0, ColKind::Int64);
    }

    #[test]
    fn session_statements_by_version() {
        let opts = SessionOpts { statement_timeout_ms: Some(120_000), ..Default::default() };
        let s = |v: &str| session_statements(&facts(v, utf8()), &opts);
        let net = ["SET SESSION net_read_timeout = 600", "SET SESSION net_write_timeout = 600"];
        assert_eq!(s("8.0.36"), ["SET SESSION TRANSACTION READ ONLY", "SET SESSION max_execution_time = 120000", net[0], net[1]]);
        assert_eq!(s("5.7.8"), ["SET SESSION TRANSACTION READ ONLY", "SET SESSION max_execution_time = 120000", net[0], net[1]]);
        assert_eq!(s("5.7.7"), ["SET SESSION TRANSACTION READ ONLY", net[0], net[1]], "no statement timeout before 5.7.8");
        assert_eq!(s("5.6.40"), ["SET SESSION TRANSACTION READ ONLY", net[0], net[1]]);
        assert_eq!(s("5.5.62"), net, "5.5: no READ ONLY session, no statement timeout");
        assert_eq!(s("4.1.22"), net);
        assert_eq!(s("4.0.27"), net);
        assert_eq!(s("5.5.5-10.6.12-MariaDB"), ["SET SESSION TRANSACTION READ ONLY", "SET SESSION max_statement_time = 120", net[0], net[1]]);
        let long = SessionOpts { statement_timeout_ms: Some(3_600_000), ..Default::default() };
        assert_eq!(session_statements(&facts("4.0.27", utf8()), &long), ["SET SESSION net_read_timeout = 3600", "SET SESSION net_write_timeout = 3600"]);
    }

    #[tokio::test]
    async fn apply_session_ignores_unknown_variables() {
        let q = Canned::new(facts("8.0.36", utf8())).fail("max_execution_time", ER_UNKNOWN_SYSTEM_VARIABLE).fail("READ ONLY", ER_PARSE_ERROR);
        apply_session(&q, &SessionOpts { statement_timeout_ms: Some(5_000), ..Default::default() }).await.unwrap();
        assert_eq!(q.log().len(), 4);
        let q = Canned::new(facts("8.0.36", utf8())).fail("net_read_timeout", 1227);
        assert!(matches!(apply_session(&q, &SessionOpts::default()).await, Err(CoreError::Query { .. })));
    }

    #[test]
    fn probe_variables_by_version() {
        let caps = |v: &str| MysqlCaps::for_version(&ServerVersion::parse(v));
        let (one, names) = probe_variables(&caps("4.0.27"));
        assert!(!one && names.contains(&"character_set") && !names.contains(&"character_set_server") && names.contains(&"key_buffer_size"));
        let (one, names) = probe_variables(&caps("4.1.22"));
        assert!(!one && !names.contains(&"character_set") && names.contains(&"character_set_server"));
        let (one, names) = probe_variables(&caps("5.0.3"));
        assert!(one && names.contains(&"innodb_buffer_pool_size") && names.contains(&"character_set_database"));
    }

    #[test]
    fn ram_estimates() {
        let m = |pairs: &[(&str, &str)]| pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect::<BTreeMap<_, _>>();
        assert_eq!(ram_estimate(&m(&[("innodb_buffer_pool_size", "3000")])), (Some(4000), Some("innodb_buffer_pool_size x 1.33".into())));
        let myisam = m(&[("innodb_buffer_pool_size", "8388608"), ("have_innodb", "DISABLED"), ("key_buffer_size", "1000")]);
        assert_eq!(ram_estimate(&myisam), (Some(4000), Some("key_buffer_size x 4".into())));
        assert_eq!(ram_estimate(&m(&[])), (None, None));
    }

    #[tokio::test]
    async fn probe_on_mysql_40_asks_variables_one_by_one() {
        let mut f = facts("4.0.27-standard", TextPlan::Fixed { charset: Charset::EucKr });
        f.server_charset = Some("euc_kr".into());
        let q = Canned::new(f)
            .on("SELECT VERSION()", text_rows(&["VERSION()", "CURRENT_USER()", "DATABASE()"], &[&["4.0.27-standard", "app@%", "shop"]]))
            .on("LIKE 'key_buffer_size'", text_rows(&["Variable_name", "Value"], &[&["key_buffer_size", "268435456"]]))
            .on("LIKE 'character_set'", text_rows(&["Variable_name", "Value"], &[&["character_set", "euc_kr"]]))
            .on("SHOW GRANTS FOR 'app'@'%'", text_rows(&["Grants for app@%"], &[&["GRANT SELECT ON shop.* TO 'app'@'%'"]]));
        let info = probe(&q).await.unwrap();
        assert_eq!(info.version, "4.0.27-standard");
        assert_eq!(info.user.as_deref(), Some("app@%"));
        assert_eq!(info.ram_bytes, Some(4 * 268_435_456));
        assert_eq!(info.ram_source.as_deref(), Some("key_buffer_size x 4"));
        assert_eq!(info.can_write, Some(false));
        assert_eq!(info.settings.get("character_set").map(String::as_str), Some("euc_kr"));
        assert_eq!(info.settings.get("protocol").map(String::as_str), Some("legacy (pre-4.1)"));
        assert!(info.settings.get("text_plan").is_some_and(|p| p.contains("EUC-KR")));
        assert!(info.notes.iter().any(|n| n.starts_with("MySQL 4.0.27")));
        assert!(info.notes.iter().any(|n| n.contains("no statement timeout")));
        let log = q.log();
        assert!(log.iter().all(|s| !s.contains("WHERE")), "4.0 has no SHOW VARIABLES WHERE: {log:?}");
        assert!(log.iter().any(|s| s == "SHOW VARIABLES LIKE 'character_set'"));
        assert!(!log.iter().any(|s| s.contains("character_set_server")));
    }

    #[tokio::test]
    async fn probe_on_mysql_8_uses_one_variables_query() {
        let q = Canned::new(facts("8.0.36", utf8()))
            .on("SELECT VERSION()", text_rows(&["VERSION()", "CURRENT_USER()", "DATABASE()"], &[&["8.0.36", "root@%", "shop"]]))
            .on("SHOW VARIABLES WHERE", text_rows(&["Variable_name", "Value"], &[&["innodb_buffer_pool_size", "134217728"], &["max_execution_time", "0"]]))
            .on("SHOW GRANTS", text_rows(&["Grants for root@%"], &[&["GRANT ALL PRIVILEGES ON *.* TO `root`@`%`"]]));
        let info = probe(&q).await.unwrap();
        assert_eq!(info.can_write, Some(true));
        assert_eq!(info.ram_source.as_deref(), Some("innodb_buffer_pool_size x 1.33"));
        assert_eq!(info.settings.get("protocol").map(String::as_str), Some("modern"));
        assert!(!info.notes.iter().any(|n| n.contains("no statement timeout")));
        assert_eq!(q.log().iter().filter(|s| s.starts_with("SHOW VARIABLES")).count(), 1);
    }

    #[tokio::test]
    async fn explain_mode_by_version() {
        let json = r#"{"query_block":{"select_id":1,"table":{"table_name":"t","access_type":"ALL","rows_examined_per_scan":10,"rows_produced_per_join":10}}}"#;
        let q = Canned::new(facts("8.0.36", utf8()))
            .on("SELECT @@session", text_rows(&["a", "b"], &[&["50", "0"]]))
            .on("EXPLAIN FORMAT=JSON", text_rows(&["EXPLAIN"], &[&[json]]));
        let plan = explain(&q, "SELECT * FROM t;", false).await.unwrap().unwrap();
        assert_eq!(plan.raw_format, "json");
        assert_eq!(plan.estimated_rows, Some(10));
        assert_eq!(
            q.log(),
            [
                "SELECT @@session.lock_wait_timeout, @@session.max_execution_time",
                "SET SESSION lock_wait_timeout = 3, SESSION max_execution_time = 15000",
                "EXPLAIN FORMAT=JSON SELECT * FROM t",
                "SET SESSION lock_wait_timeout = 50, SESSION max_execution_time = 0",
            ]
        );

        let q = Canned::new(facts("5.5.62", utf8()))
            .on("SELECT @@session", text_rows(&["a"], &[&["31536000"]]))
            .on("EXPLAIN SELECT", text_rows(&["id", "select_type", "table", "type", "rows", "Extra"], &[&["1", "SIMPLE", "t", "ALL", "42", ""]]));
        let plan = explain(&q, "SELECT * FROM t", false).await.unwrap().unwrap();
        assert_eq!(plan.raw_format, "text");
        assert_eq!(plan.estimated_rows, Some(42));
        assert!(plan.raw.starts_with("id\tselect_type\ttable"));
        assert_eq!(
            q.log(),
            ["SELECT @@session.lock_wait_timeout", "SET SESSION lock_wait_timeout = 3", "EXPLAIN SELECT * FROM t", "SET SESSION lock_wait_timeout = 31536000"]
        );

        let q = Canned::new(facts("4.0.27", TextPlan::Fixed { charset: Charset::Latin1 }));
        assert!(explain(&q, "SELECT * FROM t", false).await.unwrap().is_some());
        assert_eq!(q.log(), ["EXPLAIN SELECT * FROM t"], "4.0: no timeout variables to shorten");

        let maria = Canned::new(facts("5.5.5-10.6.12-MariaDB", utf8())).on("EXPLAIN FORMAT=JSON", text_rows(&["EXPLAIN"], &[&[json]]));
        explain(&maria, "SELECT * FROM t", false).await.unwrap();
        assert!(maria.log().contains(&"SET SESSION lock_wait_timeout = 3, SESSION max_statement_time = 15".to_string()));
    }

    #[tokio::test]
    async fn explain_refusals() {
        // Old servers run subqueries to explain them: no EXPLAIN at all, nothing sent.
        let q = Canned::new(facts("5.1.73", utf8()));
        assert!(matches!(explain(&q, "SELECT * FROM (SELECT id FROM big) d", false).await, Err(CoreError::Invalid(_))));
        assert!(q.log().is_empty());
        assert!(matches!(explain(&q, "SELECT 1", true).await, Err(CoreError::Invalid(_))), "no EXPLAIN ANALYZE before 8.0.18");
        assert!(matches!(explain(&q, "SELECT 1; DROP TABLE t", false).await, Err(CoreError::Invalid(_))));
        assert!(q.log().is_empty());
        // 5.6.3+ explains subqueries without running them (5.6.4: still tabular).
        let q =
            Canned::new(facts("5.6.4", utf8())).on("EXPLAIN SELECT", text_rows(&["id", "select_type", "table", "type", "rows"], &[&["1", "PRIMARY", "t", "ALL", "1"]]));
        assert!(explain(&q, "SELECT * FROM (SELECT id FROM big) d", false).await.unwrap().is_some());
    }

    #[test]
    fn subquery_detection() {
        assert!(has_subquery("SELECT * FROM (SELECT 1) d"));
        assert!(has_subquery("select * from t where id in (\n  select id from u)"));
        assert!(!has_subquery("SELECT COUNT(*) FROM t WHERE (a = 1)"));
        assert!(!has_subquery("SELECT f(selected) FROM t"));
        assert!(explain_runs_subqueries(&ServerVersion::parse("5.5.62")));
        assert!(!explain_runs_subqueries(&ServerVersion::parse("5.6.3")));
        assert!(!explain_runs_subqueries(&ServerVersion::parse("5.5.68-MariaDB")));
        assert_eq!(ExplainMode::for_caps(&MysqlCaps::for_version(&ServerVersion::parse("5.6.5"))), ExplainMode::Json);
        assert_eq!(ExplainMode::for_caps(&MysqlCaps::for_version(&ServerVersion::parse("5.6.4"))), ExplainMode::Tabular);
    }

    #[test]
    fn tabular_explain_mysql_40_columns() {
        let f = facts("4.0.27", TextPlan::Fixed { charset: Charset::EucKr });
        // 4.0: no id / select_type; a table named in CP949 ("한글" = C7D1 B1DB).
        let r = rows(
            &["table", "type", "possible_keys", "key", "key_len", "ref", "rows", "Extra"],
            vec![
                vec![Some(&[0xC7, 0xD1, 0xB1, 0xDB][..]), Some(b"ALL"), None, None, None, None, Some(b"1000"), Some(b"Using where")],
                vec![Some(b"customers"), Some(b"eq_ref"), Some(b"PRIMARY"), Some(b"PRIMARY"), Some(b"4"), Some(b"o.customer_id"), Some(b"1"), Some(b"")],
            ],
        );
        let p = parse_explain_tabular(&r, &f);
        assert_eq!(p.estimated_rows, Some(1000));
        assert_eq!(p.tables.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(), ["한글", "customers"]);
        let root = p.root.unwrap();
        assert_eq!(root.node_type, "query_block");
        assert_eq!(root.children.len(), 2);
        assert_eq!(root.children[0].node_type, "ALL");
        assert_eq!(root.children[0].extra.get("extra").map(String::as_str), Some("Using where"));
        assert_eq!(root.children[1].node_type, "eq_ref");
        assert_eq!(root.children[1].extra.get("key").map(String::as_str), Some("PRIMARY"));
        assert_eq!(root.children[1].extra.get("key_length").map(String::as_str), Some("4"));
    }

    #[test]
    fn tabular_explain_mysql_5x_columns() {
        let f = facts("5.5.62", utf8());
        let cols = ["id", "select_type", "table", "partitions", "type", "possible_keys", "key", "key_len", "ref", "rows", "filtered", "Extra"];
        let r = text_rows(
            &cols,
            &[
                &["1", "PRIMARY", "<derived2>", "\\N", "ALL", "\\N", "\\N", "\\N", "\\N", "50", "100.00", ""],
                &["1", "PRIMARY", "t1", "\\N", "ref", "ix_a", "ix_a", "5", "d.a", "4", "50.00", "Using where"],
                &["2", "DERIVED", "t2", "\\N", "ALL", "\\N", "\\N", "\\N", "\\N", "500", "100.00", ""],
                &["3", "UNION", "t3", "\\N", "ALL", "\\N", "\\N", "\\N", "\\N", "10", "100.00", ""],
                &["\\N", "UNION RESULT", "<union1,3>", "\\N", "ALL", "\\N", "\\N", "\\N", "\\N", "\\N", "\\N", ""],
            ],
        );
        let p = parse_explain_tabular(&r, &f);
        // Join of the first select (50 x 4 x 50 %) plus the UNION branch.
        assert_eq!(p.estimated_rows, Some(110));
        assert_eq!(p.tables.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(), ["t1", "t2", "t3"]);
        let root = p.root.unwrap();
        assert_eq!(root.extra.get("select_type").map(String::as_str), Some("PRIMARY"));
        let kinds: Vec<&str> = root.children.iter().map(|c| c.node_type.as_str()).collect();
        assert_eq!(kinds, ["ALL", "ref", "derived", "union", "union result"]);
        assert_eq!(root.children[1].plan_rows, Some(2.0));
        assert_eq!(root.children[2].parent_relationship.as_deref(), Some("SubPlan"));
        assert_eq!(root.children[2].children[0].relation.as_ref().map(|t| t.name.as_str()), Some("t2"));

        // 5.1 without partitions / filtered, and an impossible WHERE.
        let simple = text_rows(&["id", "select_type", "table", "type", "rows", "Extra"], &[&["1", "SIMPLE", "t", "range", "120", "Using where"]]);
        assert_eq!(parse_explain_tabular(&simple, &f).estimated_rows, Some(120));
        let none = text_rows(&["id", "select_type", "table", "type", "rows", "Extra"], &[&["1", "SIMPLE", "\\N", "\\N", "\\N", "Impossible WHERE"]]);
        assert_eq!(parse_explain_tabular(&none, &f).estimated_rows, Some(0));
        assert_eq!(parse_explain_tabular(&MetaRows::default(), &f), ExplainParsed::default());
    }

    #[test]
    fn meta_text_is_decoded_per_plan() {
        let euc = facts("4.0.27", TextPlan::Fixed { charset: Charset::EucKr });
        let r = rows(&["Name"], vec![vec![Some(&[0xC7, 0xD1, 0xB1, 0xDB][..])]]);
        assert_eq!(r.text(&euc, 0, 0).as_deref(), Some("한글"));
        let latin = facts("4.0.27", TextPlan::Fixed { charset: Charset::Latin1 });
        assert_eq!(r.text(&latin, 0, 0).as_deref(), Some("ÇÑ±Û"));
        // ASCII SQL needs no encoder on any plan.
        assert_eq!(euc.encode_sql("SELECT 1").unwrap().as_ref(), b"SELECT 1");

        // Pass-through over a latin1 client: utf8 metadata columns get the mojibake repair,
        // latin1 columns are CP949, SQL is encoded as CP949 and refused when it cannot be.
        let pass = facts("5.5.62", TextPlan::PassThrough { client: "latin1", legacy: Charset::EucKr });
        let meta = MetaRows {
            columns: vec![MetaColumn { name: "comment".into(), collation: Some(33) }, MetaColumn { name: "data".into(), collation: Some(8) }],
            rows: vec![vec![Some("ÇÑ±Û".as_bytes().to_vec()), Some(vec![0xC7, 0xD1, 0xB1, 0xDB])]],
        };
        assert_eq!(meta.text(&pass, 0, 0).as_deref(), Some("한글"), "dictionary mojibake is repaired");
        assert_eq!(meta.text(&pass, 0, 1).as_deref(), Some("한글"), "latin1 column bytes are CP949");
        assert_eq!(meta.text(&facts("5.5.62", utf8()), 0, 0).as_deref(), Some("ÇÑ±Û"), "no repair on a UTF-8 plan");
        assert_eq!(pass.encode_sql("SELECT '한글'").unwrap().as_ref(), b"SELECT '\xC7\xD1\xB1\xDB'");
        assert!(matches!(pass.encode_sql("SELECT '😀'"), Err(CoreError::Invalid(_))));
        assert!(pass.check_sql_text("SELECT '한글'").is_ok());
        let err = pass.check_sql_text("SELECT 1,\n  '😀'").unwrap_err();
        assert!(matches!(&err, CoreError::Invalid(m) if m.contains("line 2, column 4")), "{err:?}");
        assert!(facts("8.0.36", utf8()).check_sql_text("SELECT '😀'").is_ok(), "UTF-8 plans represent everything");
        assert_eq!(r.u64(0, 0), None);
        assert_eq!(text_rows(&["n"], &[&["42"]]).u64_by(0, "N"), Some(42));
    }

    #[tokio::test]
    async fn explain_reports_positions_in_the_callers_sql() {
        let q = Canned::new(facts("4.0.27", TextPlan::Fixed { charset: Charset::EucKr }));
        let err = explain(&q, "SELECT '😀'", false).await.unwrap_err();
        // Column 9 of the caller's text, not 17 of `EXPLAIN SELECT ...`.
        assert!(matches!(&err, CoreError::Invalid(m) if m.contains("line 1, column 9")), "{err:?}");
        assert!(q.log().is_empty(), "nothing is sent");
    }

    #[test]
    fn connection_level_server_errors_carry_hints() {
        let e = map_server_error(1275, Some("HY000"), "Server is running in --secure-auth mode, but 'u'@'h' has a password in the old format".into());
        assert!(matches!(&e, CoreError::Auth(m) if m.contains("(1275)") && m.contains(SECURE_AUTH_HINT)), "{e:?}");
        let e = map_server_error(1129, None, "Host '10.0.0.7' is blocked because of many connection errors".into());
        assert!(matches!(&e, CoreError::Connect(m) if m.contains("FLUSH HOSTS") && m.contains("mysqladmin flush-hosts") && m.contains("(1129)")), "{e:?}");
        let e = map_server_error(1130, None, "Host '10.0.0.7' is not allowed to connect to this MySQL server".into());
        assert!(matches!(&e, CoreError::Connect(m) if m.contains(HOST_NOT_ALLOWED_HINT)), "{e:?}");
        assert_eq!(map_server_error(1045, None, "Access denied".into()).code(), "AUTH");
        assert_eq!(map_server_error(1146, Some("42S02"), "no table".into()).code(), "QUERY");
    }

    /// The dispatcher's decision from the first packet: pre-4.1 greeting -> legacy client and a
    /// `Fixed` text plan from the 4.0 charset; 4.1+ greeting -> mysql_async.
    #[test]
    fn dispatch_decisions_from_greeting_bytes() {
        use crate::source::mysql_dispatch::{plan_text, EncodingSetting};
        use crate::source::mysql_legacy::handshake::parse_greeting;

        fn greeting(version: &str, caps_lo: u16, charset: u8, tail: &[u8]) -> Vec<u8> {
            let mut p = vec![10u8];
            p.extend_from_slice(version.as_bytes());
            p.push(0);
            p.extend_from_slice(&42u32.to_le_bytes());
            p.extend_from_slice(b"abcdefgh");
            p.push(0);
            p.extend_from_slice(&caps_lo.to_le_bytes());
            p.push(charset);
            p.extend_from_slice(&2u16.to_le_bytes());
            p.extend_from_slice(tail);
            p
        }
        // MySQL 4.0: LONG_PASSWORD | LONG_FLAG | CONNECT_WITH_DB | COMPRESS | TRANSACTIONS, euc_kr (19).
        let g40 = match parse_greeting(&greeting("4.0.27-standard", 0x202D, 19, &[0u8; 13])) {
            Ok(g) => g,
            Err(CoreError::Internal(m)) if m.starts_with("not implemented") => return eprintln!("skipping: {m}"),
            Err(e) => panic!("4.0 greeting: {e}"),
        };
        assert!(g40.is_pre41());
        assert!(!MysqlCaps::for_version(&g40.version()).protocol_41);
        let (plan, note) = plan_text(EncodingSetting::Auto, &g40.version(), true, g40.charset_name_40(), None);
        assert_eq!(plan, TextPlan::Fixed { charset: Charset::EucKr });
        assert!(note.is_none());

        // MySQL 5.5 (protocol 4.1 + secure connection + plugin auth).
        let mut tail = Vec::new();
        tail.extend_from_slice(&0x000Fu16.to_le_bytes()); // upper capabilities (PLUGIN_AUTH ...)
        tail.push(21);
        tail.extend_from_slice(&[0u8; 10]);
        tail.extend_from_slice(b"ijklmnopqrst\0");
        tail.extend_from_slice(b"mysql_native_password\0");
        let g55 = parse_greeting(&greeting("5.5.62", 0xF7FF, 33, &tail)).expect("5.5 greeting");
        assert!(!g55.is_pre41());
        let caps = MysqlCaps::for_version(&g55.version());
        assert!(caps.protocol_41 && caps.kill_query && !caps.explain_json);
    }
}
