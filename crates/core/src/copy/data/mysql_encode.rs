//! Pure MySQL encoders (4.3), shared by `MysqlTarget` and `testkit::RecordingTarget`:
//! `normalize_temporal`, the LOAD DATA field encoder, the INSERT literal encoder, the load
//! statements, the session prelude / `sql_mode` builder, the session assertion and the chunk
//! sizing from replication limits. Owner: copy-data.
//!
//! Values are encoded column-wise from the `RecordBatch` arrays (no row objects): one row's
//! fields are appended to a piece buffer, and pieces always end on a row boundary.

use std::borrow::Cow;
use std::collections::BTreeMap;

use arrow_array::RecordBatch;
use bytes::Bytes;

use crate::copy::data::cells::{canonical, date_text, decimal_text, float32_text, float_text, time_text, Cell, Col};
use crate::copy::data::values::{classify_date, fill_zero_parts, format_epoch_utc, format_micros, timestamp_min_literal, DateClass};
use crate::copy::hash::{hex_upper_into, RowHasher, TableChecksum};
use crate::copy::types::*;
use crate::copy::{copy_fail, CODE_BINARY_ATTR_UNDECODABLE, CODE_ROW_TOO_LARGE};
use crate::error::{CoreError, Result};

/// One encoded batch: the LOAD DATA payload or the INSERT statements, ending on row boundaries.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Encoded {
    /// LOAD DATA: payload pieces (each <= `piece_bytes`, a longer single row alone); INSERT:
    /// one statement per piece.
    pub pieces: Vec<Bytes>,
    /// Rows in each piece (same length as `pieces`).
    pub piece_rows: Vec<u64>,
    pub rows: u64,
    /// Characters replaced by `?` (4-byte characters on a utf8 / 3-byte target) and U+FFFD
    /// produced while decoding byte cells of text columns.
    pub unmappable: u64,
    pub value_fixes: Vec<ValueFix>,
}

/// Which load method a target uses: `MysqlLoadData` when `facts.vars["local_infile"]` is ON.
pub fn method_for(facts: &TargetFacts) -> LoadMethod {
    match facts.vars.get("local_infile").map(|v| v.to_ascii_uppercase()) {
        Some(v) if v == "ON" || v == "1" => LoadMethod::MysqlLoadData,
        _ => LoadMethod::MysqlInsert,
    }
}

/// `max_allowed_packet` of the target (64 MiB when unknown).
pub fn max_allowed_packet(facts: &TargetFacts) -> usize {
    facts.vars.get("max_allowed_packet").and_then(|v| v.trim().parse::<usize>().ok()).filter(|v| *v > 0).unwrap_or(64 << 20)
}

/// The connection charset BulkDB uses on a target: `utf8mb4` from 5.5.3, else `utf8`.
pub fn connection_charset(facts: &TargetFacts) -> &'static str {
    if facts.at_least(5, 5, 3) {
        "utf8mb4"
    } else {
        "utf8"
    }
}

/// Whether `charset` is MySQL's 3-byte UTF-8 (`utf8`, `utf8mb3`).
pub fn is_three_byte_charset(charset: &str) -> bool {
    let c = charset.trim();
    c.eq_ignore_ascii_case("utf8") || c.eq_ignore_ascii_case("utf8mb3")
}

/// Whether text must be limited to 3-byte UTF-8 (4-byte characters become `?`, 3.3
/// `UTF8MB3_ONLY`): the connection itself is utf8 (targets before 5.5.3), or the table's
/// effective target charset (`WriterConfig::charset_for`) is utf8 / utf8mb3 on any version.
pub fn three_byte_target(facts: &TargetFacts, table_charset: Option<&str>) -> bool {
    connection_charset(facts) == "utf8" || table_charset.is_some_and(is_three_byte_charset)
}

/// Size limits of the encoder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EncodeLimits {
    /// LOAD DATA piece size.
    pub piece_bytes: usize,
    /// INSERT statement size: `min(max_allowed_packet - 64 KiB, piece)`.
    pub insert_bytes: usize,
    /// Rows per INSERT statement (`batch_rows`).
    pub insert_rows: usize,
    /// Largest binary value: `max_allowed_packet - 1 KiB` (UNHEX returns NULL above it).
    pub max_binary: usize,
}

impl EncodeLimits {
    pub fn new(facts: &TargetFacts, piece_bytes: usize, batch_rows: usize) -> EncodeLimits {
        let packet = max_allowed_packet(facts);
        EncodeLimits {
            piece_bytes: piece_bytes.max(1024),
            insert_bytes: packet.saturating_sub(64 << 10).min(piece_bytes).max(1024),
            insert_rows: batch_rows.clamp(1, 65_536),
            max_binary: packet.saturating_sub(1 << 10),
        }
    }

    /// 8 MiB pieces, 50,000 rows per INSERT.
    pub fn defaults(facts: &TargetFacts) -> EncodeLimits {
        EncodeLimits::new(facts, 8 << 20, 50_000)
    }
}

/// Encode one Arrow batch for `spec` on a target with `facts` (default limits, method by
/// `local_infile`).
pub fn encode_batch(spec: &TableDataSpec, batch: &RecordBatch, facts: &TargetFacts) -> Result<Encoded> {
    encode_batch_with(spec, batch, facts, None)
}

/// [`encode_batch`] for a table whose effective target charset is `charset`.
pub fn encode_batch_with(spec: &TableDataSpec, batch: &RecordBatch, facts: &TargetFacts, charset: Option<&str>) -> Result<Encoded> {
    let mut enc = Encoder::new(spec, facts, method_for(facts), EncodeLimits::defaults(facts)).target_charset(charset);
    enc.encode(batch, None)?;
    Ok(enc.finish_batch())
}

// ---------------------------------------------------------------------------------------------
// Identifiers and statements
// ---------------------------------------------------------------------------------------------

/// `` `name` `` with backticks doubled.
pub fn qi(name: &str) -> String {
    format!("`{}`", name.replace('`', "``"))
}

/// `` `db`.`t` `` (or `` `t` ``).
pub fn qualified(t: &TargetTable) -> String {
    match &t.schema {
        Some(s) => format!("{}.{}", qi(s), qi(&t.name)),
        None => qi(&t.name),
    }
}

/// Whether `LOAD DATA` knows the `CHARACTER SET` clause (5.0.38+, 5.1.17+).
pub fn load_data_has_charset_clause(facts: &TargetFacts) -> bool {
    match (facts.major, facts.minor) {
        (5, 0) => facts.patch >= 38,
        (5, 1) => facts.patch >= 17,
        (m, _) => m >= 5,
    }
}

/// How a column travels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Out {
    Text,
    Binary,
    Bit,
    Number,
    Temporal,
}

fn out_of(c: &DataColumn) -> Out {
    match c.value {
        ValueKind::Binary => Out::Binary,
        ValueKind::Bit => Out::Bit,
        ValueKind::Integer | ValueKind::Decimal | ValueKind::Float | ValueKind::Bool => Out::Number,
        ValueKind::Date | ValueKind::DateTime | ValueKind::Timestamp | ValueKind::Time | ValueKind::Year => Out::Temporal,
        ValueKind::Text | ValueKind::Json | ValueKind::Raw => Out::Text,
    }
}

/// The LOAD DATA statement of one chunk, reading the armed stream `token`.
pub fn load_data_sql(spec: &TableDataSpec, facts: &TargetFacts, token: &str) -> String {
    let mut cols = Vec::with_capacity(spec.columns.len());
    let mut sets = Vec::new();
    for (i, c) in spec.columns.iter().enumerate() {
        match out_of(c) {
            Out::Binary => {
                cols.push(format!("@v{i}"));
                sets.push(format!("{} = UNHEX(@v{i})", qi(&c.target)));
            }
            Out::Bit => {
                cols.push(format!("@v{i}"));
                sets.push(format!("{} = CAST(@v{i} AS UNSIGNED)", qi(&c.target)));
            }
            _ => cols.push(qi(&c.target)),
        }
    }
    let charset = if load_data_has_charset_clause(facts) { format!(" CHARACTER SET {}", connection_charset(facts)) } else { String::new() };
    let mut sql = format!(
        r"LOAD DATA LOCAL INFILE '{token}' INTO TABLE {}{charset} FIELDS TERMINATED BY '\t' ESCAPED BY '\\' LINES TERMINATED BY '\n' ({})",
        qualified(&spec.target),
        cols.join(", ")
    );
    if !sets.is_empty() {
        sql.push_str(" SET ");
        sql.push_str(&sets.join(", "));
    }
    sql
}

/// `INSERT INTO `db`.`t` (`c1`, …) VALUES ` (the rows follow).
pub fn insert_prefix(spec: &TableDataSpec) -> String {
    let cols: Vec<String> = spec.columns.iter().map(|c| qi(&c.target)).collect();
    format!("INSERT INTO {} ({}) VALUES ", qualified(&spec.target), cols.join(", "))
}

// ---------------------------------------------------------------------------------------------
// Session prelude, sql_mode, assertion, chunks
// ---------------------------------------------------------------------------------------------

/// The components of the combination modes BulkDB has to edit.
fn expand_mode(flag: &str) -> Vec<&str> {
    match flag {
        "ANSI" => vec!["REAL_AS_FLOAT", "PIPES_AS_CONCAT", "ANSI_QUOTES", "IGNORE_SPACE", "ONLY_FULL_GROUP_BY"],
        "TRADITIONAL" => vec!["STRICT_TRANS_TABLES", "STRICT_ALL_TABLES", "NO_ZERO_IN_DATE", "NO_ZERO_DATE", "ERROR_FOR_DIVISION_BY_ZERO", "NO_ENGINE_SUBSTITUTION"],
        other => vec![other],
    }
}

/// The session `sql_mode` of a target writer (4.3): the target's global mode − `ANSI_QUOTES` −
/// `NO_BACKSLASH_ESCAPES` + `STRICT_ALL_TABLES` + `NO_ENGINE_SUBSTITUTION` +
/// `NO_AUTO_VALUE_ON_ZERO`; with `keep_zero_dates` also − `NO_ZERO_DATE` − `NO_ZERO_IN_DATE` +
/// `ALLOW_INVALID_DATES`.
pub fn target_sql_mode(global: &str, keep_zero_dates: bool) -> String {
    let mut flags: Vec<String> = Vec::new();
    for raw in global.split(',').map(|f| f.trim().to_ascii_uppercase()).filter(|f| !f.is_empty()) {
        for f in expand_mode(&raw) {
            if !flags.iter().any(|x| x == f) {
                flags.push(f.to_string());
            }
        }
    }
    let mut remove = vec!["ANSI_QUOTES", "NO_BACKSLASH_ESCAPES"];
    let mut add = vec!["STRICT_ALL_TABLES", "NO_ENGINE_SUBSTITUTION", "NO_AUTO_VALUE_ON_ZERO"];
    if keep_zero_dates {
        remove.extend(["NO_ZERO_DATE", "NO_ZERO_IN_DATE"]);
        add.push("ALLOW_INVALID_DATES");
    }
    flags.retain(|f| !remove.contains(&f.as_str()));
    for a in add {
        if !flags.iter().any(|f| f == a) {
            flags.push(a.to_string());
        }
    }
    flags.join(",")
}

/// `zero_dates: auto` resolution (3.4): `null` when the target's global `sql_mode` has
/// `NO_ZERO_DATE`, or `NO_ZERO_IN_DATE` with a `STRICT_*` mode; otherwise `keep`.
pub fn resolve_zero_dates(policy: ZeroDatePolicy, global_sql_mode: &str) -> ZeroDatePolicy {
    if policy != ZeroDatePolicy::Auto {
        return policy;
    }
    let flags: Vec<String> =
        global_sql_mode.split(',').flat_map(|f| expand_mode(&f.trim().to_ascii_uppercase()).into_iter().map(str::to_string).collect::<Vec<_>>()).collect();
    let has = |f: &str| flags.iter().any(|x| x == f);
    if has("NO_ZERO_DATE") || (has("NO_ZERO_IN_DATE") && (has("STRICT_TRANS_TABLES") || has("STRICT_ALL_TABLES"))) {
        ZeroDatePolicy::Null
    } else {
        ZeroDatePolicy::Keep
    }
}

/// The session prelude of every MySQL target writer session (4.3), in order. `zero_dates` is
/// the effective policy (never `Auto`).
pub fn session_prelude(facts: &TargetFacts, zero_dates: ZeroDatePolicy, lock_timeout_s: u32) -> Vec<String> {
    let global = facts.vars.get("sql_mode").map(String::as_str).unwrap_or("");
    let mode = target_sql_mode(global, zero_dates == ZeroDatePolicy::Keep);
    let mut out = vec![
        format!("SET NAMES {}", connection_charset(facts)),
        format!("SET SESSION sql_mode = '{mode}'"),
        "SET SESSION time_zone = '+00:00'".to_string(),
        "SET SESSION foreign_key_checks = 0".to_string(),
    ];
    if facts.at_least(5, 5, 3) {
        out.push(format!("SET SESSION lock_wait_timeout = {}", lock_timeout_s.clamp(1, 600)));
    }
    out.push("SET SESSION net_read_timeout = 600, net_write_timeout = 600, wait_timeout = 28800".to_string());
    out
}

/// What the session assertion expects, collected from the session `SET` statements a writer
/// executed (time zone, sql_mode, `SET NAMES` charset).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionExpect {
    pub time_zone: Option<String>,
    pub sql_mode: Option<String>,
    pub charset: Option<String>,
}

impl SessionExpect {
    /// Record a session statement (others are ignored).
    pub fn observe(&mut self, sql: &str) {
        let t = sql.trim();
        let up = t.to_ascii_uppercase();
        let quoted = |s: &str| s.split('\'').nth(1).map(str::to_string);
        if let Some(rest) = up.strip_prefix("SET NAMES ") {
            self.charset = Some(rest.split_whitespace().next().unwrap_or("").trim_matches('\'').to_ascii_lowercase());
        } else if up.starts_with("SET SESSION SQL_MODE") || up.starts_with("SET SQL_MODE") {
            self.sql_mode = quoted(t);
        } else if up.starts_with("SET SESSION TIME_ZONE") || up.starts_with("SET TIME_ZONE") {
            self.time_zone = quoted(t);
        }
    }

    /// The assertion query (4.3).
    pub const QUERY: &'static str = "SELECT @@session.time_zone, @@session.sql_mode, @@session.character_set_client";

    /// Compare the assertion row; `Err(INTERNAL "target session lost its settings")` on a
    /// difference. `sql_mode` is compared as a set (the server reorders flags).
    pub fn check(&self, time_zone: Option<&str>, sql_mode: Option<&str>, charset: Option<&str>) -> Result<()> {
        let set = |s: &str| {
            let mut v: Vec<String> = s.split(',').map(|f| f.trim().to_ascii_uppercase()).filter(|f| !f.is_empty()).collect();
            v.sort();
            v
        };
        let tz_ok = self.time_zone.as_deref().is_none_or(|e| time_zone.is_some_and(|t| t == e || (e == "+00:00" && t.eq_ignore_ascii_case("UTC"))));
        let mode_ok = self.sql_mode.as_deref().is_none_or(|e| sql_mode.is_some_and(|m| set(m) == set(e)));
        let cs_ok = self.charset.as_deref().is_none_or(|e| charset.is_some_and(|c| c.eq_ignore_ascii_case(e) || (e == "utf8" && c.eq_ignore_ascii_case("utf8mb3"))));
        if tz_ok && mode_ok && cs_ok {
            Ok(())
        } else {
            Err(CoreError::Internal(format!("target session lost its settings (time_zone {:?}, sql_mode {:?}, character_set_client {:?})", time_zone, sql_mode, charset)))
        }
    }
}

/// Default chunk limits (4.3).
pub const CHUNK_BYTES: u64 = 64 << 20;
pub const CHUNK_ROWS: u64 = 500_000;

/// Chunk size from the replication limits (4.3): 64 MiB, capped at half of the smallest of
/// `group_replication_transaction_size_limit`, `wsrep_max_ws_size`, `max_binlog_cache_size`
/// (when set); rows 500,000, capped at half of `wsrep_max_ws_rows` (when set).
pub fn chunk_limits(facts: &TargetFacts) -> (u64, u64) {
    let num = |k: &str| facts.vars.get(k).and_then(|v| v.trim().parse::<u64>().ok()).filter(|v| *v > 0);
    let mut bytes = CHUNK_BYTES;
    for k in ["group_replication_transaction_size_limit", "wsrep_max_ws_size", "max_binlog_cache_size"] {
        if let Some(v) = num(k) {
            bytes = bytes.min(v / 2);
        }
    }
    let rows = match num("wsrep_max_ws_rows") {
        Some(v) => CHUNK_ROWS.min((v / 2).max(1)),
        None => CHUNK_ROWS,
    };
    (bytes.max(1 << 20), rows)
}

/// Parse the OK info of a LOAD statement (`Records: N  Deleted: 0  Skipped: S  Warnings: W`)
/// into (records, skipped, warnings).
pub fn parse_load_info(info: &str) -> (u64, u64, u64) {
    let get = |key: &str| -> u64 {
        info.find(key).map(|i| info[i + key.len()..].trim_start().split(|c: char| !c.is_ascii_digit()).next().unwrap_or("").parse().unwrap_or(0)).unwrap_or(0)
    };
    (get("Records:"), get("Skipped:"), get("Warnings:"))
}

/// `SHOW WARNINGS` codes after DDL that only annotate the item (deprecations, duplicate index).
pub fn ddl_warning_is_note(code: u32) -> bool {
    matches!(code, 1287 | 1681 | 3719 | 3778 | 1831)
}

// ---------------------------------------------------------------------------------------------
// Temporal normalisation (3.4)
// ---------------------------------------------------------------------------------------------

/// A temporal value as the reader delivered it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TemporalIn<'a> {
    Null,
    Text(&'a str),
    /// `UNIX_TIMESTAMP(c)` (sources < 4.1.3); 0 is the zero value.
    Epoch(i64),
    /// `c + 0` for YEAR; 0 is the zero year.
    Number(i64),
}

/// Value-fix codes.
pub const FIX_ZERO_NULL: &str = "ZERO_DATE_NULL";
pub const FIX_ZERO_MIN: &str = "ZERO_DATE_MIN";
pub const FIX_INVALID: &str = "INVALID_DATE";

/// Result of [`normalize_temporal`]: the text to write (`None` = NULL) and the value-fix code
/// when the value was replaced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Normalised {
    pub value: Option<String>,
    pub fix: Option<&'static str>,
}

fn min_literal(kind: ValueKind) -> String {
    match kind {
        ValueKind::Date => "1000-01-01".to_string(),
        ValueKind::Timestamp => timestamp_min_literal("+00:00"),
        ValueKind::Year => "1901".to_string(),
        _ => "1000-01-01 00:00:00".to_string(),
    }
}

/// Normalise one temporal value of `col` (3.4): epochs to UTC text, compact 4.0 TIMESTAMP
/// forms, YEAR numbers, calendar validation and the zero-date / invalid-date actions.
/// `first_ts_auto`: this is the auto-set first TIMESTAMP of a 4.0 table, whose zero values
/// load as the minimum under every policy except `keep`.
pub fn normalize_temporal(input: TemporalIn<'_>, col: &DataColumn, first_ts_auto: bool) -> Normalised {
    let kind = col.value;
    let keep = |s: String| Normalised { value: Some(s), fix: None };
    let zero_action = if first_ts_auto && kind == ValueKind::Timestamp && col.zero_date != ZeroDateAction::Keep { ZeroDateAction::Min } else { col.zero_date };
    let on_zero = |text: Option<&str>, partial: bool| -> Normalised {
        match zero_action {
            ZeroDateAction::Null => Normalised { value: None, fix: Some(FIX_ZERO_NULL) },
            ZeroDateAction::Min => {
                let v = match (partial, text) {
                    (true, Some(t)) if kind != ValueKind::Year => fill_zero_parts(t).unwrap_or_else(|| min_literal(kind)),
                    _ => min_literal(kind),
                };
                Normalised { value: Some(v), fix: Some(FIX_ZERO_MIN) }
            }
            ZeroDateAction::Keep | ZeroDateAction::NotTemporal => Normalised {
                value: Some(match (kind, text) {
                    // Four digits: LOAD DATA passes every field as a string, and MySQL stores a
                    // 1- or 2-character '0' / '00' as 2000; '0000' (and a numeric 0000 on the
                    // INSERT path) stores the zero year.
                    (ValueKind::Year, _) => "0000".to_string(),
                    (_, Some(t)) => t.to_string(),
                    (ValueKind::Date, None) => "0000-00-00".to_string(),
                    _ => "0000-00-00 00:00:00".to_string(),
                }),
                fix: None,
            },
        }
    };
    match input {
        TemporalIn::Null => Normalised { value: None, fix: None },
        TemporalIn::Number(n) | TemporalIn::Epoch(n) if kind == ValueKind::Year => {
            if n == 0 {
                on_zero(None, false)
            } else {
                keep(n.to_string())
            }
        }
        TemporalIn::Epoch(secs) | TemporalIn::Number(secs) => {
            if secs == 0 {
                on_zero(None, false)
            } else {
                match format_epoch_utc(secs) {
                    Some(t) if kind == ValueKind::Date => keep(t[..10].to_string()),
                    Some(t) => keep(t),
                    None => keep(secs.to_string()),
                }
            }
        }
        TemporalIn::Text(t) => {
            let t = t.trim();
            match kind {
                ValueKind::Time => keep(t.to_string()),
                ValueKind::Year => match t.parse::<i64>() {
                    Ok(0) => on_zero(None, false),
                    _ => keep(t.to_string()),
                },
                _ => {
                    // 4.0 compact TIMESTAMP(M) forms (only reached by the DATE_FORMAT fallback
                    // or a reader that bypassed it).
                    if kind == ValueKind::Timestamp && !t.is_empty() && t.bytes().all(|b| b.is_ascii_digit()) {
                        if let Some(parsed) = compact_timestamp(t) {
                            return match parsed {
                                None => on_zero(None, false),
                                Some(micros) => keep(format_micros(micros).unwrap_or_else(|| t.to_string())),
                            };
                        }
                    }
                    match classify_date(t) {
                        DateClass::Valid | DateClass::Unparsable => keep(t.to_string()),
                        DateClass::Zero => on_zero(Some(t), false),
                        DateClass::PartialZero => on_zero(Some(t), true),
                        DateClass::Invalid => {
                            let action = match (col.invalid_date, kind) {
                                // A TIMESTAMP cannot hold an impossible date.
                                (ZeroDateAction::Keep | ZeroDateAction::NotTemporal, ValueKind::Timestamp) => ZeroDateAction::Min,
                                (a, _) => a,
                            };
                            match action {
                                ZeroDateAction::Null => Normalised { value: None, fix: Some(FIX_INVALID) },
                                ZeroDateAction::Min => Normalised { value: Some(min_literal(kind)), fix: Some(FIX_INVALID) },
                                ZeroDateAction::Keep | ZeroDateAction::NotTemporal => keep(t.to_string()),
                            }
                        }
                    }
                }
            }
        }
    }
}

/// 4.0 compact `TIMESTAMP(M)` text (`20040601103045`, `040601`): `Some(None)` for the zero value,
/// `Some(Some(micros))` otherwise, `None` when it is not such a value.
#[cfg(feature = "mysql")]
fn compact_timestamp(t: &str) -> Option<Option<i64>> {
    crate::source::mysql_common::parse_compact_timestamp(t.as_bytes())
}

#[cfg(not(feature = "mysql"))]
fn compact_timestamp(_t: &str) -> Option<Option<i64>> {
    None
}

/// Merge `more` into `acc` (sum per column and code).
pub fn merge_fixes(acc: &mut Vec<ValueFix>, more: &[ValueFix]) {
    for f in more {
        match acc.iter_mut().find(|a| a.column == f.column && a.code == f.code) {
            Some(a) => a.count += f.count,
            None => acc.push(f.clone()),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Encoder
// ---------------------------------------------------------------------------------------------

/// LOAD DATA field escaping (4.3): `\` `\\`, TAB `\t`, LF `\n`, CR `\r`, NUL `\0`, 0x1A `\Z`.
pub fn escape_field_into(s: &[u8], out: &mut Vec<u8>) {
    for &b in s {
        match b {
            b'\\' => out.extend_from_slice(b"\\\\"),
            b'\t' => out.extend_from_slice(b"\\t"),
            b'\n' => out.extend_from_slice(b"\\n"),
            b'\r' => out.extend_from_slice(b"\\r"),
            0 => out.extend_from_slice(b"\\0"),
            0x1A => out.extend_from_slice(b"\\Z"),
            _ => out.push(b),
        }
    }
}

/// Single-quoted literal for the INSERT path (backslash escapes on, as BulkDB's session
/// `sql_mode` never has `NO_BACKSLASH_ESCAPES`; the connection is utf8/utf8mb4).
pub fn quote_literal_into(s: &str, out: &mut Vec<u8>) {
    out.push(b'\'');
    for &b in s.as_bytes() {
        match b {
            b'\'' => out.extend_from_slice(b"''"),
            b'\\' => out.extend_from_slice(b"\\\\"),
            0 => out.extend_from_slice(b"\\0"),
            0x1A => out.extend_from_slice(b"\\Z"),
            _ => out.push(b),
        }
    }
    out.push(b'\'');
}

/// `true` for a plain numeric literal (`-12`, `3.50`, `1e300`), safe to send bare.
pub fn is_number_literal(s: &str) -> bool {
    let b = s.as_bytes();
    let mut i = 0;
    if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
        i += 1;
    }
    let start = i;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
    }
    let mut digits = i - start;
    if i < b.len() && b[i] == b'.' {
        i += 1;
        let f = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        digits += i - f;
    }
    if digits == 0 {
        return false;
    }
    if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
        i += 1;
        if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
            i += 1;
        }
        let e = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        if i == e {
            return false;
        }
    }
    i == b.len()
}

/// One rendered field.
enum Field<'a> {
    Null,
    /// Text that needs escaping / quoting.
    Text(Cow<'a, str>),
    /// A number sent bare.
    Bare(Cow<'a, str>),
    /// Raw bytes (hex on the wire).
    Hex(&'a [u8]),
}

/// Stateful encoder of one table (counts value fixes and unmappable characters across batches).
pub struct Encoder<'s> {
    spec: &'s TableDataSpec,
    method: LoadMethod,
    limits: EncodeLimits,
    three_byte: bool,
    prefix: String,
    first_ts: Option<usize>,
    fixes: BTreeMap<(usize, &'static str), u64>,
    unmappable: u64,
    rows: u64,
    pieces: Vec<Bytes>,
    piece_rows: Vec<u64>,
    /// Per column: an ENUM without a `''` member (its `''` is the error value).
    enum_error: Vec<bool>,
    /// `enum_error: null`.
    enum_error_null: bool,
}

impl<'s> Encoder<'s> {
    pub fn new(spec: &'s TableDataSpec, facts: &TargetFacts, method: LoadMethod, limits: EncodeLimits) -> Encoder<'s> {
        let first_ts = if spec.first_timestamp_auto { spec.columns.iter().position(|c| c.value == ValueKind::Timestamp) } else { None };
        Encoder {
            spec,
            method,
            limits,
            three_byte: three_byte_target(facts, None),
            prefix: insert_prefix(spec),
            first_ts,
            fixes: BTreeMap::new(),
            unmappable: 0,
            rows: 0,
            pieces: Vec::new(),
            piece_rows: Vec::new(),
            enum_error: Vec::new(),
            enum_error_null: false,
        }
    }

    /// The table's effective target charset (`WriterConfig::charset_for`): utf8 / utf8mb3
    /// limits text to 3-byte UTF-8 even on a utf8mb4 connection.
    pub fn target_charset(mut self, charset: Option<&str>) -> Self {
        self.three_byte |= charset.is_some_and(is_three_byte_charset);
        self
    }

    /// The ENUM columns (target names) whose `''` is the error value of a non-strict source
    /// (`WriterConfig::enum_error_columns_for`), and whether it loads as NULL (`enum_error:
    /// null`) instead of failing the table.
    pub fn enum_errors(mut self, columns: &[String], null: bool) -> Self {
        self.enum_error = self.spec.columns.iter().map(|c| columns.iter().any(|x| x == &c.target)).collect();
        self.enum_error_null = null;
        self
    }

    pub fn method(&self) -> LoadMethod {
        self.method
    }

    /// Render one cell of column `ci`.
    fn field<'c>(&mut self, ci: usize, col: &DataColumn, cell: Cell<'c>, scratch: &'c mut String) -> Result<Field<'c>> {
        let empty = matches!(&cell, Cell::Str(s) if s.is_empty()) || matches!(&cell, Cell::Bytes(b) if b.is_empty());
        if empty && self.enum_error.get(ci).copied().unwrap_or(false) {
            if self.enum_error_null {
                *self.fixes.entry((ci, crate::copy::FIX_ENUM_ERROR_NULL)).or_default() += 1;
                return Ok(Field::Null);
            }
            return Err(copy_fail(
                crate::copy::CODE_ENUM_ERROR_VALUE,
                format!(
                    "column {} holds the ENUM error value '' (invalid input a non-strict source server kept), which the strict target refuses; load it as NULL (enum_error: null) or fix the source",
                    col.target
                ),
            ));
        }
        Ok(match out_of(col) {
            _ if matches!(cell, Cell::Null) => Field::Null,
            Out::Binary => match cell {
                Cell::Bytes(b) => {
                    if b.len() > self.limits.max_binary {
                        return Err(copy_fail(
                            CODE_ROW_TOO_LARGE,
                            format!("a value of column {} has {} bytes, more than the target's max_allowed_packet allows; raise max_allowed_packet", col.target, b.len()),
                        ));
                    }
                    Field::Hex(b)
                }
                Cell::Str(s) => Field::Hex(s.as_bytes()),
                other => {
                    *scratch = String::from_utf8_lossy(&canonical(&other).unwrap_or_default()).into_owned();
                    Field::Hex(scratch.as_bytes())
                }
            },
            Out::Bit => {
                let v: u64 = match cell {
                    Cell::Int(i) => i as u64,
                    Cell::UInt(u) => u,
                    Cell::Bool(b) => b as u64,
                    Cell::Bytes(b) => b.iter().fold(0u64, |acc, x| (acc << 8) | *x as u64),
                    Cell::Str(s) => s.trim().parse::<u64>().unwrap_or_else(|_| s.bytes().fold(0u64, |acc, x| (acc << 8) | x as u64)),
                    _ => 0,
                };
                *scratch = v.to_string();
                Field::Bare(Cow::Borrowed(scratch.as_str()))
            }
            // A 4.0 FLOAT read widened (`c + 0e0`): the double carries ~14 digits of the f32
            // (0.1f arrives as 0.10000000149012); write the f32's shortest text, which is what
            // the FLOAT column stores and reads back.
            Out::Number if col.read == SourceRead::FloatWiden && matches!(cell, Cell::Float(_) | Cell::Str(_)) => {
                let v = match cell {
                    Cell::Float(f) => Some(f),
                    Cell::Str(s) => s.trim().parse::<f64>().ok(),
                    _ => None,
                };
                match (v, cell) {
                    (Some(f), _) if f.is_finite() => Field::Bare(Cow::Owned(float32_text(f))),
                    (_, Cell::Str(s)) if is_number_literal(s.trim()) => Field::Bare(Cow::Borrowed(s.trim())),
                    (_, Cell::Str(s)) => Field::Text(Cow::Borrowed(s)),
                    (_, other) => Field::Text(Cow::Owned(String::from_utf8_lossy(&canonical(&other).unwrap_or_default()).into_owned())),
                }
            }
            Out::Number => match cell {
                Cell::Int(i) => Field::Bare(Cow::Owned(i.to_string())),
                Cell::UInt(u) => Field::Bare(Cow::Owned(u.to_string())),
                Cell::Float(f) => Field::Bare(Cow::Owned(float_text(f))),
                Cell::Bool(b) => Field::Bare(Cow::Borrowed(if b { "1" } else { "0" })),
                Cell::Decimal(v, s) => Field::Bare(Cow::Owned(decimal_text(v, s))),
                Cell::Str(s) if is_number_literal(s.trim()) => Field::Bare(Cow::Borrowed(s.trim())),
                Cell::Str(s) => Field::Text(Cow::Borrowed(s)),
                other => Field::Text(Cow::Owned(String::from_utf8_lossy(&canonical(&other).unwrap_or_default()).into_owned())),
            },
            Out::Temporal => {
                let input = match cell {
                    Cell::Str(s) => TemporalIn::Text(s),
                    Cell::Int(i) if col.read == SourceRead::UnixTimestamp => TemporalIn::Epoch(i),
                    Cell::Int(i) => TemporalIn::Number(i),
                    Cell::UInt(u) if col.read == SourceRead::UnixTimestamp => TemporalIn::Epoch(u as i64),
                    Cell::UInt(u) => TemporalIn::Number(u as i64),
                    Cell::Decimal(v, s) if s <= 0 => TemporalIn::Number(v as i64),
                    Cell::Decimal(v, s) => {
                        *scratch = decimal_text(v, s);
                        let whole = scratch.split('.').next().unwrap_or("0").parse::<i64>().unwrap_or(0);
                        if col.read == SourceRead::UnixTimestamp {
                            TemporalIn::Epoch(whole)
                        } else {
                            TemporalIn::Number(whole)
                        }
                    }
                    Cell::Float(f) => TemporalIn::Number(f as i64),
                    Cell::Date(d) => {
                        *scratch = date_text(d);
                        TemporalIn::Text(scratch.as_str())
                    }
                    Cell::Time(t) => {
                        *scratch = time_text(t);
                        TemporalIn::Text(scratch.as_str())
                    }
                    Cell::Timestamp(t) => {
                        *scratch = format_micros(t).unwrap_or_default();
                        TemporalIn::Text(scratch.as_str())
                    }
                    Cell::Bytes(b) => {
                        *scratch = String::from_utf8_lossy(b).into_owned();
                        TemporalIn::Text(scratch.as_str())
                    }
                    Cell::Other(o) => {
                        *scratch = o;
                        TemporalIn::Text(scratch.as_str())
                    }
                    Cell::Bool(b) => TemporalIn::Number(b as i64),
                    Cell::Null => TemporalIn::Null,
                };
                let n = normalize_temporal(input, col, self.first_ts == Some(ci));
                if let Some(code) = n.fix {
                    *self.fixes.entry((ci, code)).or_default() += 1;
                }
                match n.value {
                    None => Field::Null,
                    Some(v) if col.value == ValueKind::Year && is_number_literal(&v) => Field::Bare(Cow::Owned(v)),
                    Some(v) => Field::Text(Cow::Owned(v)),
                }
            }
            Out::Text => {
                let text: Cow<'c, str> = match cell {
                    Cell::Str(s) => Cow::Borrowed(s),
                    Cell::Bytes(b) => {
                        let s = String::from_utf8_lossy(b);
                        if let Cow::Owned(o) = &s {
                            self.unmappable += o.chars().filter(|c| *c == '\u{FFFD}').count() as u64;
                        }
                        Cow::Owned(s.into_owned())
                    }
                    other => Cow::Owned(String::from_utf8_lossy(&canonical(&other).unwrap_or_default()).into_owned()),
                };
                if col.binary_attr && text.contains('\u{FFFD}') {
                    return Err(copy_fail(
                        CODE_BINARY_ATTR_UNDECODABLE,
                        format!("column {} holds bytes that are not text in the source character set; copy it as binary (treat_as: binary)", col.target),
                    ));
                }
                if self.three_byte && text.chars().any(|c| c as u32 > 0xFFFF) {
                    let mut n = 0u64;
                    let replaced: String = text
                        .chars()
                        .map(|c| {
                            if c as u32 > 0xFFFF {
                                n += 1;
                                '?'
                            } else {
                                c
                            }
                        })
                        .collect();
                    self.unmappable += n;
                    Field::Text(Cow::Owned(replaced))
                } else {
                    Field::Text(text)
                }
            }
        })
    }

    /// Encode `batch` (columns in `spec.columns` order). Hashes each row's written form into
    /// `checksum` when given.
    pub fn encode(&mut self, batch: &RecordBatch, mut checksum: Option<&mut TableChecksum>) -> Result<()> {
        if batch.num_columns() != self.spec.columns.len() {
            return Err(CoreError::Internal(format!(
                "copy: {} has {} source columns but the plan lists {}",
                self.spec.item_id,
                batch.num_columns(),
                self.spec.columns.len()
            )));
        }
        let cols: Vec<Col<'_>> = batch.columns().iter().map(Col::new).collect::<Result<_>>()?;
        let spec = self.spec;
        let mut piece: Vec<u8> = Vec::new();
        let mut piece_rows = 0usize;
        let mut row_buf: Vec<u8> = Vec::with_capacity(256);
        let load_data = self.method == LoadMethod::MysqlLoadData;
        let mut scratch = String::new();
        for row in 0..batch.num_rows() {
            row_buf.clear();
            let mut hasher = checksum.as_ref().map(|_| RowHasher::default());
            if !load_data {
                row_buf.push(b'(');
            }
            for (ci, col) in spec.columns.iter().enumerate() {
                if ci > 0 {
                    row_buf.push(if load_data { b'\t' } else { b',' });
                }
                let cell = cols[ci].cell(row);
                let field = self.field(ci, col, cell, &mut scratch)?;
                if let Some(h) = hasher.as_mut() {
                    match &field {
                        Field::Null => h.cell(None),
                        // YEAR is read back as a number (`y + 0`): the zero year written as
                        // `0000` hashes as `0`.
                        Field::Bare(t) if col.value == ValueKind::Year => match t.trim().parse::<i64>() {
                            Ok(n) => h.cell(Some(n.to_string().as_bytes())),
                            Err(_) => h.cell(Some(t.as_bytes())),
                        },
                        Field::Text(t) | Field::Bare(t) => h.cell(Some(t.as_bytes())),
                        Field::Hex(b) => h.cell(Some(crate::copy::hash::hex(b).as_bytes())),
                    }
                }
                match (field, load_data) {
                    (Field::Null, true) => row_buf.extend_from_slice(b"\\N"),
                    (Field::Null, false) => row_buf.extend_from_slice(b"NULL"),
                    (Field::Text(t), true) => escape_field_into(t.as_bytes(), &mut row_buf),
                    (Field::Text(t), false) => quote_literal_into(&t, &mut row_buf),
                    (Field::Bare(t), _) => row_buf.extend_from_slice(t.as_bytes()),
                    (Field::Hex(b), true) => hex_upper_into(b, &mut row_buf),
                    (Field::Hex(b), false) => {
                        row_buf.extend_from_slice(b"X'");
                        hex_upper_into(b, &mut row_buf);
                        row_buf.push(b'\'');
                    }
                }
            }
            if let (Some(h), Some(sum)) = (hasher, checksum.as_deref_mut()) {
                sum.add_row(h.finish());
            }
            if load_data {
                row_buf.push(b'\n');
                piece.extend_from_slice(&row_buf);
                piece_rows += 1;
                if piece.len() >= self.limits.piece_bytes {
                    self.pieces.push(Bytes::from(std::mem::take(&mut piece)));
                    self.piece_rows.push(piece_rows as u64);
                    piece_rows = 0;
                }
            } else {
                row_buf.push(b')');
                if self.prefix.len() + row_buf.len() > self.limits.insert_bytes {
                    return Err(copy_fail(
                        CODE_ROW_TOO_LARGE,
                        format!(
                            "a row of {} needs a {} byte INSERT, more than the target's max_allowed_packet allows; raise max_allowed_packet",
                            spec.target.name,
                            self.prefix.len() + row_buf.len()
                        ),
                    ));
                }
                if piece_rows > 0 && (piece.len() + 1 + row_buf.len() > self.limits.insert_bytes || piece_rows >= self.limits.insert_rows) {
                    self.pieces.push(Bytes::from(std::mem::take(&mut piece)));
                    self.piece_rows.push(piece_rows as u64);
                    piece_rows = 0;
                }
                if piece_rows == 0 {
                    piece.extend_from_slice(self.prefix.as_bytes());
                } else {
                    piece.push(b',');
                }
                piece.extend_from_slice(&row_buf);
                piece_rows += 1;
            }
            self.rows += 1;
        }
        if !piece.is_empty() {
            self.pieces.push(Bytes::from(piece));
            self.piece_rows.push(piece_rows as u64);
        }
        Ok(())
    }

    /// Take the pieces and counters accumulated since the last call.
    pub fn finish_batch(&mut self) -> Encoded {
        let value_fixes = std::mem::take(&mut self.fixes)
            .into_iter()
            .map(|((ci, code), count)| ValueFix { column: self.spec.columns.get(ci).map(|c| c.target.clone()).unwrap_or_default(), code: code.to_string(), count })
            .collect();
        Encoded {
            pieces: std::mem::take(&mut self.pieces),
            piece_rows: std::mem::take(&mut self.piece_rows),
            rows: std::mem::take(&mut self.rows),
            unmappable: std::mem::take(&mut self.unmappable),
            value_fixes,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use arrow_array::{ArrayRef, BinaryArray, Decimal128Array, Float64Array, Int64Array, StringArray};

    fn col(name: &str, value: ValueKind) -> DataColumn {
        let temporal = matches!(value, ValueKind::Date | ValueKind::DateTime | ValueKind::Timestamp | ValueKind::Year);
        DataColumn {
            source: name.into(),
            target: name.into(),
            value,
            read: SourceRead::Plain,
            zero_date: if temporal { ZeroDateAction::Null } else { ZeroDateAction::NotTemporal },
            invalid_date: if temporal { ZeroDateAction::Null } else { ZeroDateAction::NotTemporal },
            binary_attr: false,
            max_bytes: None,
        }
    }

    fn spec(columns: Vec<DataColumn>) -> TableDataSpec {
        TableDataSpec {
            item_id: "table:\"t\"".into(),
            source_sql: "SELECT 1".into(),
            verify_sql: String::new(),
            raw_csv: false,
            transcode: None,
            target: TargetTable { schema: Some("db".into()), name: "t".into() },
            columns,
            est_rows: None,
            first_timestamp_auto: false,
            fk_checks: false,
        }
    }

    fn facts(local_infile: bool) -> TargetFacts {
        let mut f = TargetFacts::new(CopyFamily::Mysql, "mysql", "8.0.36", "db");
        f.vars.insert("local_infile".into(), if local_infile { "ON" } else { "OFF" }.into());
        f.vars.insert("max_allowed_packet".into(), "67108864".into());
        f
    }

    fn batch(cols: Vec<ArrayRef>) -> RecordBatch {
        let fields: Vec<arrow_schema::Field> = cols.iter().enumerate().map(|(i, a)| arrow_schema::Field::new(format!("c{i}"), a.data_type().clone(), true)).collect();
        RecordBatch::try_new(Arc::new(arrow_schema::Schema::new(fields)), cols).unwrap()
    }

    #[test]
    fn load_data_field_escaping() {
        let mut out = Vec::new();
        escape_field_into(b"a\\b\tc\nd\re\0f\x1Ag", &mut out);
        assert_eq!(out, b"a\\\\b\\tc\\nd\\re\\0f\\Zg");
        let s = spec(vec![col("id", ValueKind::Integer), col("name", ValueKind::Text), col("data", ValueKind::Binary), col("flags", ValueKind::Bit)]);
        let b = batch(vec![
            Arc::new(Int64Array::from(vec![Some(1), Some(2), None])),
            Arc::new(StringArray::from(vec![Some("가나\t다"), Some(""), None])),
            Arc::new(BinaryArray::from(vec![Some(&[0x00, 0xff][..]), Some(&[][..]), None])),
            Arc::new(Int64Array::from(vec![Some(-1), Some(5), None])),
        ]);
        let enc = encode_batch(&s, &b, &facts(true)).unwrap();
        assert_eq!(enc.rows, 3);
        assert_eq!(enc.pieces.len(), 1);
        let text = String::from_utf8(enc.pieces[0].to_vec()).unwrap();
        assert_eq!(text, "1\t가나\\t다\t00FF\t18446744073709551615\n2\t\t\t5\n\\N\t\\N\t\\N\t\\N\n");
        let sql = load_data_sql(&s, &facts(true), "bulkdb-j-1-0");
        assert_eq!(
            sql,
            r"LOAD DATA LOCAL INFILE 'bulkdb-j-1-0' INTO TABLE `db`.`t` CHARACTER SET utf8mb4 FIELDS TERMINATED BY '\t' ESCAPED BY '\\' LINES TERMINATED BY '\n' (`id`, `name`, @v2, @v3) SET `data` = UNHEX(@v2), `flags` = CAST(@v3 AS UNSIGNED)"
        );
        let old = TargetFacts::new(CopyFamily::Mysql, "mysql", "5.0.37", "db");
        assert!(!load_data_sql(&s, &old, "x").contains("CHARACTER SET"));
        assert!(load_data_sql(&s, &TargetFacts::new(CopyFamily::Mysql, "mysql", "5.1.17", "db"), "x").contains("CHARACTER SET utf8 "));
    }

    #[test]
    fn insert_literals_and_packet_splitting() {
        let s = spec(vec![
            col("id", ValueKind::Integer),
            col("name", ValueKind::Text),
            col("price", ValueKind::Decimal),
            col("ratio", ValueKind::Float),
            col("b", ValueKind::Binary),
        ]);
        let b = batch(vec![
            Arc::new(Int64Array::from(vec![Some(1), Some(2)])),
            Arc::new(StringArray::from(vec![Some("it's \\ 한글"), None])),
            Arc::new(Decimal128Array::from(vec![Some(12345), Some(-5)]).with_precision_and_scale(9, 2).unwrap()),
            Arc::new(Float64Array::from(vec![Some(0.1), Some(1e300)])),
            Arc::new(BinaryArray::from(vec![Some(&[0xab][..]), None])),
        ]);
        let f = facts(false);
        let enc = encode_batch(&s, &b, &f).unwrap();
        assert_eq!(enc.pieces.len(), 1);
        assert_eq!(
            String::from_utf8(enc.pieces[0].to_vec()).unwrap(),
            "INSERT INTO `db`.`t` (`id`, `name`, `price`, `ratio`, `b`) VALUES (1,'it''s \\\\ 한글',123.45,0.1,X'AB'),(2,NULL,-0.05,1e300,NULL)"
        );
        // Row limit splits statements on row boundaries.
        let mut e = Encoder::new(&s, &f, LoadMethod::MysqlInsert, EncodeLimits::new(&f, 8 << 20, 1));
        e.encode(&b, None).unwrap();
        let out = e.finish_batch();
        assert_eq!(out.pieces.len(), 2);
        assert!(out.pieces.iter().all(|p| p.starts_with(b"INSERT INTO `db`.`t`") && p.ends_with(b")")));
        // A single row above the packet budget fails before sending.
        let mut small = f.clone();
        small.vars.insert("max_allowed_packet".into(), "70000".into());
        let big = batch(vec![
            Arc::new(Int64Array::from(vec![Some(1)])),
            Arc::new(StringArray::from(vec![Some("x".repeat(10_000))])),
            Arc::new(Decimal128Array::from(vec![Some(1)]).with_precision_and_scale(9, 2).unwrap()),
            Arc::new(Float64Array::from(vec![Some(1.0)])),
            Arc::new(BinaryArray::from(vec![None::<&[u8]>])),
        ]);
        let err = encode_batch(&s, &big, &small).unwrap_err();
        assert!(err.to_string().contains("max_allowed_packet"), "{err}");
        assert_eq!(crate::copy::fail_code(&err), Some(CODE_ROW_TOO_LARGE));
    }

    #[test]
    fn load_data_pieces_end_on_row_boundaries() {
        let s = spec(vec![col("name", ValueKind::Text)]);
        let f = facts(true);
        let values: Vec<String> = (0..100).map(|i| format!("row-{i:03}-{}", "y".repeat(30))).collect();
        let b = batch(vec![Arc::new(StringArray::from(values.iter().map(|v| Some(v.as_str())).collect::<Vec<_>>()))]);
        let mut e = Encoder::new(&s, &f, LoadMethod::MysqlLoadData, EncodeLimits { piece_bytes: 1024, insert_bytes: 1024, insert_rows: 10, max_binary: 1 << 20 });
        e.encode(&b, None).unwrap();
        let out = e.finish_batch();
        assert!(out.pieces.len() > 1);
        for p in &out.pieces {
            assert!(p.ends_with(b"\n"));
            assert!(p.len() < 1024 + 50);
        }
        let joined: Vec<u8> = out.pieces.iter().flat_map(|p| p.to_vec()).collect();
        assert_eq!(joined.iter().filter(|b| **b == b'\n').count(), 100);
    }

    #[test]
    fn four_byte_characters_on_three_byte_targets() {
        let s = spec(vec![col("name", ValueKind::Text)]);
        let b = batch(vec![Arc::new(StringArray::from(vec![Some("a😀b😀")]))]);
        let mut old = TargetFacts::new(CopyFamily::Mysql, "mysql", "5.5.2", "db");
        old.vars.insert("local_infile".into(), "ON".into());
        let enc = encode_batch(&s, &b, &old).unwrap();
        assert_eq!(enc.unmappable, 2);
        assert_eq!(&enc.pieces[0][..], b"a?b?\n");
        let enc = encode_batch(&s, &b, &facts(true)).unwrap();
        assert_eq!(enc.unmappable, 0);
        assert_eq!(&enc.pieces[0][..], "a😀b😀\n".as_bytes());
        // A user-chosen (or per-table) utf8 / utf8mb3 charset on a utf8mb4-capable target:
        // replaced and counted on every version and both load methods.
        for version in ["5.5.3", "5.7.44", "8.0.36", "8.4.3", "9.1.0"] {
            for infile in ["ON", "OFF"] {
                let mut f = TargetFacts::new(CopyFamily::Mysql, "mysql", version, "db");
                f.vars.insert("local_infile".into(), infile.into());
                assert_eq!(connection_charset(&f), "utf8mb4");
                for cs in ["utf8", "utf8mb3", "UTF8MB3"] {
                    assert!(three_byte_target(&f, Some(cs)), "{version} {cs}");
                    let enc = encode_batch_with(&s, &b, &f, Some(cs)).unwrap();
                    assert_eq!(enc.unmappable, 2, "{version} {infile} {cs}");
                    let text = String::from_utf8(enc.pieces.concat()).unwrap();
                    assert!(text.contains("a?b?") && !text.contains('😀'), "{version} {infile} {cs}: {text}");
                }
                let enc = encode_batch_with(&s, &b, &f, Some("utf8mb4")).unwrap();
                assert_eq!(enc.unmappable, 0, "{version} utf8mb4 keeps them");
                assert!(!three_byte_target(&f, None));
            }
        }
        // BMP text (Korean) is never touched.
        let ko = batch(vec![Arc::new(StringArray::from(vec![Some("한글")]))]);
        let enc = encode_batch_with(&s, &ko, &facts(true), Some("utf8")).unwrap();
        assert_eq!((enc.unmappable, &enc.pieces[0][..]), (0, "한글\n".as_bytes()));
    }

    #[test]
    fn writer_config_charset_per_table() {
        let cfg = WriterConfig {
            job_id: "j".into(),
            session_no: 1,
            batch_rows: 1,
            piece_bytes: 1,
            chunk_bytes: 1,
            chunk_rows: 1,
            lock_timeout_s: 5,
            fail_on_unmappable: false,
            charset: Some("utf8mb4".into()),
            table_charsets: [("table:\"legacy\"".to_string(), "utf8".to_string())].into_iter().collect(),
            enum_error_columns: [("table:\"legacy\"".to_string(), vec!["state".to_string()])].into_iter().collect(),
            enum_error_null: false,
            source_checksum: false,
        };
        assert_eq!(cfg.enum_error_columns_for("table:\"legacy\""), ["state".to_string()]);
        assert!(cfg.enum_error_columns_for("table:\"other\"").is_empty());
        assert_eq!(cfg.charset_for("table:\"legacy\""), Some("utf8"));
        assert_eq!(cfg.charset_for("table:\"other\""), Some("utf8mb4"));
        let none = WriterConfig { charset: None, table_charsets: Default::default(), ..cfg };
        assert_eq!(none.charset_for("table:\"legacy\""), None);
    }

    #[test]
    fn binary_attribute_with_replacement_fails() {
        let mut c = col("code", ValueKind::Text);
        c.binary_attr = true;
        let s = spec(vec![c]);
        let b = batch(vec![Arc::new(StringArray::from(vec![Some("ab\u{FFFD}")]))]);
        let err = encode_batch(&s, &b, &facts(true)).unwrap_err();
        assert_eq!(crate::copy::fail_code(&err), Some(CODE_BINARY_ATTR_UNDECODABLE));
    }

    #[test]
    fn temporal_normalisation_per_policy() {
        let mut d = col("d", ValueKind::Date);
        let n = |c: &DataColumn, v: TemporalIn<'_>| normalize_temporal(v, c, false);
        // null policy
        assert_eq!(n(&d, TemporalIn::Text("0000-00-00")), Normalised { value: None, fix: Some(FIX_ZERO_NULL) });
        assert_eq!(n(&d, TemporalIn::Text("2004-02-31")), Normalised { value: None, fix: Some(FIX_INVALID) });
        assert_eq!(n(&d, TemporalIn::Text("2004-04-31")), Normalised { value: None, fix: Some(FIX_INVALID) });
        assert_eq!(n(&d, TemporalIn::Text("2020-00-15")), Normalised { value: None, fix: Some(FIX_ZERO_NULL) });
        assert_eq!(n(&d, TemporalIn::Text("2004-02-29")).value.as_deref(), Some("2004-02-29"));
        // min policy
        d.zero_date = ZeroDateAction::Min;
        d.invalid_date = ZeroDateAction::Min;
        assert_eq!(n(&d, TemporalIn::Text("0000-00-00")), Normalised { value: Some("1000-01-01".into()), fix: Some(FIX_ZERO_MIN) });
        assert_eq!(n(&d, TemporalIn::Text("2020-05-00")).value.as_deref(), Some("2020-05-01"));
        assert_eq!(n(&d, TemporalIn::Text("2020-00-00")).value.as_deref(), Some("2020-01-01"));
        assert_eq!(n(&d, TemporalIn::Text("2004-02-31")), Normalised { value: Some("1000-01-01".into()), fix: Some(FIX_INVALID) });
        // keep policy
        d.zero_date = ZeroDateAction::Keep;
        d.invalid_date = ZeroDateAction::Keep;
        assert_eq!(n(&d, TemporalIn::Text("0000-00-00")), Normalised { value: Some("0000-00-00".into()), fix: None });
        assert_eq!(n(&d, TemporalIn::Text("2004-02-31")), Normalised { value: Some("2004-02-31".into()), fix: None });
        // TIMESTAMP under keep cannot hold an invalid date: minimum.
        let mut ts = col("ts", ValueKind::Timestamp);
        ts.zero_date = ZeroDateAction::Keep;
        ts.invalid_date = ZeroDateAction::Keep;
        assert_eq!(n(&ts, TemporalIn::Text("2004-02-31 10:00:00")), Normalised { value: Some("1970-01-01 00:00:01".into()), fix: Some(FIX_INVALID) });
        // Epochs (UNIX_TIMESTAMP reads of 4.0 TIMESTAMP(N)).
        ts.read = SourceRead::UnixTimestamp;
        ts.zero_date = ZeroDateAction::Null;
        assert_eq!(n(&ts, TemporalIn::Epoch(1_086_085_845)).value.as_deref(), Some("2004-06-01 10:30:45"));
        assert_eq!(n(&ts, TemporalIn::Epoch(0)), Normalised { value: None, fix: Some(FIX_ZERO_NULL) });
        // The first auto-set TIMESTAMP of a 4.0 table: zero -> minimum even under null.
        assert_eq!(normalize_temporal(TemporalIn::Epoch(0), &ts, true), Normalised { value: Some("1970-01-01 00:00:01".into()), fix: Some(FIX_ZERO_MIN) });
        // Compact 4.0 forms (fallback).
        let ts2 = col("ts2", ValueKind::Timestamp);
        assert_eq!(n(&ts2, TemporalIn::Text("20040601103045")).value.as_deref(), Some("2004-06-01 10:30:45"));
        assert_eq!(n(&ts2, TemporalIn::Text("040601")).value.as_deref(), Some("2004-06-01 00:00:00"));
        assert_eq!(n(&ts2, TemporalIn::Text("00000000000000")), Normalised { value: None, fix: Some(FIX_ZERO_NULL) });
        // YEAR numbers.
        let mut y = col("y", ValueKind::Year);
        assert_eq!(n(&y, TemporalIn::Number(0)), Normalised { value: None, fix: Some(FIX_ZERO_NULL) });
        assert_eq!(n(&y, TemporalIn::Number(2004)).value.as_deref(), Some("2004"));
        y.zero_date = ZeroDateAction::Min;
        assert_eq!(n(&y, TemporalIn::Number(0)).value.as_deref(), Some("1901"));
        y.zero_date = ZeroDateAction::Keep;
        assert_eq!(n(&y, TemporalIn::Number(0)).value.as_deref(), Some("0000"), "never '0': LOAD DATA stores that as 2000");
        assert_eq!(n(&y, TemporalIn::Text("0000")).value.as_deref(), Some("0000"));
    }

    /// A zero YEAR under `keep` reaches LOAD DATA as `0000` (a 1- or 2-character '0' would be
    /// stored as 2000) and INSERT as the number 0000; the checksum hashes the number the target
    /// reads back (`y + 0` = 0).
    #[test]
    fn zero_year_under_keep_in_both_load_paths() {
        let mut y = col("y", ValueKind::Year);
        y.read = SourceRead::YearNumber;
        y.zero_date = ZeroDateAction::Keep;
        let s = spec(vec![col("id", ValueKind::Integer), y]);
        let b = batch(vec![Arc::new(Int64Array::from(vec![Some(1), Some(2)])), Arc::new(Int64Array::from(vec![Some(0), Some(2004)]))]);
        let enc = encode_batch(&s, &b, &facts(true)).unwrap();
        assert_eq!(String::from_utf8(enc.pieces[0].to_vec()).unwrap(), "1\t0000\n2\t2004\n");
        let enc = encode_batch(&s, &b, &facts(false)).unwrap();
        assert_eq!(String::from_utf8(enc.pieces[0].to_vec()).unwrap(), "INSERT INTO `db`.`t` (`id`, `y`) VALUES (1,0000),(2,2004)");
        // The checksum equals the one of the read-back (`(y + 0)`: integers 0 and 2004).
        let f = facts(true);
        let mut e = Encoder::new(&s, &f, LoadMethod::MysqlLoadData, EncodeLimits::defaults(&f));
        let mut sum = TableChecksum::default();
        e.encode(&b, Some(&mut sum)).unwrap();
        let mut back = TableChecksum::default();
        crate::copy::data::cells::checksum_batch(&b, &mut back).unwrap();
        assert_eq!(sum, back);
    }

    /// A 4.0 FLOAT read widened (`c + 0e0`, about 14 significant digits) is written and hashed
    /// as the shortest text of the nearest f32, which is what the FLOAT column stores and reads
    /// back.
    #[test]
    fn widened_float_is_written_as_its_f32() {
        let mut c = col("f", ValueKind::Float);
        c.read = SourceRead::FloatWiden;
        let s = spec(vec![c.clone()]);
        let b = batch(vec![Arc::new(Float64Array::from(vec![Some(0.10000000149012), Some(1.1000000238419), Some(-2.5), Some(3.4028234663853e38), None])) as ArrayRef]);
        let enc = encode_batch(&s, &b, &facts(true)).unwrap();
        assert_eq!(String::from_utf8(enc.pieces[0].to_vec()).unwrap(), "0.1\n1.1\n-2.5\n3.4028235e38\n\\N\n");
        let text = batch(vec![Arc::new(StringArray::from(vec![Some("0.10000000149012")])) as ArrayRef]);
        let enc = encode_batch(&s, &text, &facts(false)).unwrap();
        assert_eq!(String::from_utf8(enc.pieces[0].to_vec()).unwrap(), "INSERT INTO `db`.`t` (`f`) VALUES (0.1)");
        // The checksum equals the target's read-back of the stored f32 (widened like the driver).
        let f = facts(true);
        let mut e = Encoder::new(&s, &f, LoadMethod::MysqlLoadData, EncodeLimits::defaults(&f));
        let mut sum = TableChecksum::default();
        e.encode(&b, Some(&mut sum)).unwrap();
        let stored: Vec<Option<f64>> =
            vec![Some(0.1f32), Some(1.1f32), Some(-2.5f32), Some(3.4028235e38f32), None].into_iter().map(|v| v.map(|x| format!("{x}").parse::<f64>().unwrap())).collect();
        let mut back = TableChecksum::default();
        crate::copy::data::cells::checksum_batch(&batch(vec![Arc::new(Float64Array::from(stored)) as ArrayRef]), &mut back).unwrap();
        assert_eq!(sum, back);
        // A FLOAT(M,D) / DOUBLE column (plain read) keeps the double.
        let s = spec(vec![col("d", ValueKind::Float)]);
        let enc = encode_batch(&s, &b, &facts(true)).unwrap();
        assert!(String::from_utf8(enc.pieces[0].to_vec()).unwrap().starts_with("0.10000000149012\n"));
    }

    /// The ENUM error value `''`: refused with `ENUM_ERROR_VALUE` by default, NULL (counted)
    /// under `enum_error: null`; a real `''` member and other columns are untouched.
    #[test]
    fn enum_error_value_policy() {
        let s = spec(vec![col("state", ValueKind::Text), col("note", ValueKind::Text)]);
        let b = batch(vec![Arc::new(StringArray::from(vec![Some("Y"), Some("")])) as ArrayRef, Arc::new(StringArray::from(vec![Some(""), Some("")])) as ArrayRef]);
        let f = facts(true);
        let cols = vec!["state".to_string()];
        let mut e = Encoder::new(&s, &f, LoadMethod::MysqlLoadData, EncodeLimits::defaults(&f)).enum_errors(&cols, false);
        let err = e.encode(&b, None).unwrap_err();
        assert_eq!(crate::copy::fail_code(&err), Some(crate::copy::CODE_ENUM_ERROR_VALUE));
        assert!(err.to_string().contains("state"), "{err}");
        let mut e = Encoder::new(&s, &f, LoadMethod::MysqlLoadData, EncodeLimits::defaults(&f)).enum_errors(&cols, true);
        e.encode(&b, None).unwrap();
        let out = e.finish_batch();
        assert_eq!(String::from_utf8(out.pieces[0].to_vec()).unwrap(), "Y\t\n\\N\t\n");
        assert_eq!(out.value_fixes, vec![ValueFix { column: "state".into(), code: crate::copy::FIX_ENUM_ERROR_NULL.into(), count: 1 }]);
        // Without ENUM columns nothing changes.
        assert!(encode_batch(&s, &b, &f).is_ok());
    }

    #[test]
    fn value_fixes_are_counted_per_column() {
        let s = spec(vec![col("d", ValueKind::Date)]);
        let b = batch(vec![Arc::new(StringArray::from(vec![Some("0000-00-00"), Some("2004-02-31"), Some("2004-04-31"), Some("2004-01-01"), None]))]);
        let enc = encode_batch(&s, &b, &facts(true)).unwrap();
        assert_eq!(&enc.pieces[0][..], b"\\N\n\\N\n\\N\n2004-01-01\n\\N\n");
        let mut fixes = enc.value_fixes.clone();
        fixes.sort_by(|a, b| a.code.cmp(&b.code));
        assert_eq!(
            fixes,
            vec![ValueFix { column: "d".into(), code: FIX_INVALID.into(), count: 2 }, ValueFix { column: "d".into(), code: FIX_ZERO_NULL.into(), count: 1 }]
        );
        let mut acc = Vec::new();
        merge_fixes(&mut acc, &fixes);
        merge_fixes(&mut acc, &fixes);
        assert_eq!(acc.iter().map(|f| f.count).sum::<u64>(), 6);
    }

    #[test]
    fn sql_mode_and_prelude() {
        let strict = "ONLY_FULL_GROUP_BY,STRICT_TRANS_TABLES,NO_ZERO_IN_DATE,NO_ZERO_DATE,ERROR_FOR_DIVISION_BY_ZERO,NO_ENGINE_SUBSTITUTION";
        assert_eq!(
            target_sql_mode(strict, false),
            "ONLY_FULL_GROUP_BY,STRICT_TRANS_TABLES,NO_ZERO_IN_DATE,NO_ZERO_DATE,ERROR_FOR_DIVISION_BY_ZERO,NO_ENGINE_SUBSTITUTION,STRICT_ALL_TABLES,NO_AUTO_VALUE_ON_ZERO"
        );
        assert_eq!(
            target_sql_mode(strict, true),
            "ONLY_FULL_GROUP_BY,STRICT_TRANS_TABLES,ERROR_FOR_DIVISION_BY_ZERO,NO_ENGINE_SUBSTITUTION,STRICT_ALL_TABLES,NO_AUTO_VALUE_ON_ZERO,ALLOW_INVALID_DATES"
        );
        assert_eq!(target_sql_mode("ANSI_QUOTES,NO_BACKSLASH_ESCAPES", false), "STRICT_ALL_TABLES,NO_ENGINE_SUBSTITUTION,NO_AUTO_VALUE_ON_ZERO");
        assert!(!target_sql_mode("ANSI", false).contains("ANSI_QUOTES"));
        assert!(!target_sql_mode("TRADITIONAL", true).contains("NO_ZERO_DATE"));
        assert_eq!(target_sql_mode("", false), "STRICT_ALL_TABLES,NO_ENGINE_SUBSTITUTION,NO_AUTO_VALUE_ON_ZERO");
        assert_eq!(resolve_zero_dates(ZeroDatePolicy::Auto, strict), ZeroDatePolicy::Null);
        assert_eq!(resolve_zero_dates(ZeroDatePolicy::Auto, "NO_ZERO_IN_DATE"), ZeroDatePolicy::Keep);
        assert_eq!(resolve_zero_dates(ZeroDatePolicy::Auto, ""), ZeroDatePolicy::Keep);
        assert_eq!(resolve_zero_dates(ZeroDatePolicy::Min, strict), ZeroDatePolicy::Min);

        let mut f = facts(true);
        f.vars.insert("sql_mode".into(), strict.into());
        let p = session_prelude(&f, ZeroDatePolicy::Null, 5);
        assert_eq!(p[0], "SET NAMES utf8mb4");
        assert!(p[1].starts_with("SET SESSION sql_mode = 'ONLY_FULL_GROUP_BY,") && p[1].contains("STRICT_ALL_TABLES"));
        assert_eq!(p[2], "SET SESSION time_zone = '+00:00'");
        assert_eq!(p[3], "SET SESSION foreign_key_checks = 0");
        assert_eq!(p[4], "SET SESSION lock_wait_timeout = 5");
        let old = TargetFacts::new(CopyFamily::Mysql, "mysql", "5.1.73", "db");
        let p = session_prelude(&old, ZeroDatePolicy::Keep, 5);
        assert_eq!(p[0], "SET NAMES utf8");
        assert!(p[1].contains("ALLOW_INVALID_DATES"));
        assert!(!p.iter().any(|s| s.contains("lock_wait_timeout")));
        for s in &p {
            crate::copy::guard::check_statement(CopyFamily::Mysql, s, crate::copy::guard::StmtContext::Session).unwrap();
        }
    }

    #[test]
    fn session_assertion() {
        let mut e = SessionExpect::default();
        for s in session_prelude(&facts(true), ZeroDatePolicy::Null, 5) {
            e.observe(&s);
        }
        assert_eq!(e.charset.as_deref(), Some("utf8mb4"));
        assert_eq!(e.time_zone.as_deref(), Some("+00:00"));
        let mode = e.sql_mode.clone().unwrap();
        let reordered: Vec<&str> = mode.split(',').rev().collect();
        assert!(e.check(Some("+00:00"), Some(&reordered.join(",")), Some("utf8mb4")).is_ok());
        assert!(e.check(Some("SYSTEM"), Some(&mode), Some("utf8mb4")).is_err(), "a lost time_zone is an error");
        assert!(e.check(Some("+00:00"), Some(""), Some("utf8mb4")).is_err());
        assert!(e.check(Some("+00:00"), Some(&mode), Some("latin1")).is_err());
        assert!(SessionExpect::default().check(None, None, None).is_ok());
    }

    #[test]
    fn chunk_sizing_from_replication_limits() {
        let mut f = facts(true);
        assert_eq!(chunk_limits(&f), (64 << 20, 500_000));
        f.vars.insert("group_replication_transaction_size_limit".into(), "150000000".into());
        assert_eq!(chunk_limits(&f).0, 64 << 20);
        f.vars.insert("wsrep_max_ws_size".into(), "20000000".into());
        f.vars.insert("wsrep_max_ws_rows".into(), "100000".into());
        assert_eq!(chunk_limits(&f), (10_000_000, 50_000));
        f.vars.insert("max_binlog_cache_size".into(), "18446744073709547520".into());
        assert_eq!(chunk_limits(&f).0, 10_000_000);
        assert_eq!(parse_load_info("Records: 3  Deleted: 0  Skipped: 1  Warnings: 2"), (3, 1, 2));
        assert_eq!(parse_load_info(""), (0, 0, 0));
        assert!(ddl_warning_is_note(1287) && !ddl_warning_is_note(1265));
    }

    #[test]
    fn number_literals() {
        for ok in ["1", "-12", "3.50", ".5", "1e300", "1.5E-7", "+4"] {
            assert!(is_number_literal(ok), "{ok}");
        }
        for bad in ["", "-", "1e", "1;DROP", "0x10", "1 2", "abc", "."] {
            assert!(!is_number_literal(bad), "{bad}");
        }
    }
}
