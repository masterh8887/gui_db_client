//! MySQL source model and planning (3.2 - 3.4, 3.7). One reader serves 4.0 through 9.x: the
//! SHOW statements below are the same on every version (and are the contract with the fake 4.0
//! server test); views, triggers and routines need 5.0+.
//!
//! | purpose | SQL |
//! |---|---|
//! | tables, engine, estimates | ``SHOW TABLE STATUS FROM `<db>` `` |
//! | definition | ``SHOW CREATE TABLE `<db>`.`<t>` `` |
//! | 4.0 facts line | `SHOW VARIABLES LIKE 'character_set'`, `SHOW VARIABLES LIKE 'timezone'` |
//! | views | ``SHOW FULL TABLES FROM `<db>` WHERE Table_type = 'VIEW'``, ``SHOW CREATE VIEW`` |
//! | triggers | ``SHOW TRIGGERS FROM `<db>` ``, ``SHOW CREATE TRIGGER`` (5.1.21+) |
//! | charset sample | ``SELECT CAST(`c` AS BINARY) FROM `<db>`.`<t>` WHERE `c` <> '' LIMIT 1000`` (4.0: ``SELECT `c` …``) |

use std::collections::{BTreeMap, BTreeSet};

use crate::copy::plan::{check_cancel, meta_u64, selected, target_table_name, Collected, Draft, FamilyOps, PlanInput, TClass, TCol, TFk, TKey, TObj, TargetState};
use crate::copy::translate::mysql::*;
use crate::copy::translate::{bq, bq2, danger, info, item_id, my_str, warning, IssueExt};
use crate::copy::types::*;
use crate::encoding::Charset;
use crate::error::{CoreError, Result};
use crate::types::Connection;

// =============================================================================================
// Source facts
// =============================================================================================

/// How the source connection decodes text (docs/mysql-compat.md §5, `TextPlan`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Decode {
    /// Pre-4.1: one charset for everything.
    Fixed(Charset),
    /// 4.1+ `SET NAMES utf8mb4`: the server converts every column from its own charset.
    ServerUtf8,
    /// 4.1+ pass-through: euckr columns as CP949, utf8 as UTF-8, latin1 (and unknown) as `legacy`.
    PassThrough(Charset),
}

/// The connection's `encoding` field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EncodingField {
    Auto,
    Utf8,
    EucKr,
    Latin1,
}

pub(crate) fn encoding_field(conn: &Connection) -> EncodingField {
    match conn.fields.get("encoding").map(|s| s.trim().to_ascii_lowercase()).as_deref() {
        Some("utf8" | "utf-8" | "utf8mb4" | "utf8mb3") => EncodingField::Utf8,
        Some("euc-kr" | "euckr" | "euc_kr" | "cp949" | "uhc" | "ksc5601") => EncodingField::EucKr,
        Some("latin1" | "windows-1252" | "cp1252") => EncodingField::Latin1,
        _ => EncodingField::Auto,
    }
}

/// What the planner knows about the source server.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SourceInfo {
    pub version: String,
    pub ver: MyVersion,
    /// Read through the pre-4.1 text protocol.
    pub legacy: bool,
    pub server_charset: Option<String>,
    pub database_charset: Option<String>,
    pub decode: Decode,
    pub encoding: EncodingField,
    /// Session statements of the connection's text plan.
    pub session: Vec<String>,
    pub user: Option<String>,
}

impl SourceInfo {
    /// From the version and the connection alone (no live facts).
    pub fn derive(version: &str, conn: &Connection, server_charset: Option<String>) -> SourceInfo {
        let ver = MyVersion::parse(version);
        let encoding = encoding_field(conn);
        let legacy = !ver.at(4, 1, 0);
        let decode = if legacy {
            Decode::Fixed(match encoding {
                EncodingField::Auto => server_charset.as_deref().and_then(Charset::from_mysql_name).filter(|c| *c != Charset::Binary).unwrap_or(Charset::Latin1),
                EncodingField::Utf8 => Charset::Utf8,
                EncodingField::EucKr => Charset::EucKr,
                EncodingField::Latin1 => Charset::Latin1,
            })
        } else {
            match encoding {
                EncodingField::Auto | EncodingField::Utf8 => Decode::ServerUtf8,
                EncodingField::EucKr => Decode::PassThrough(Charset::EucKr),
                EncodingField::Latin1 => Decode::PassThrough(Charset::Latin1),
            }
        };
        SourceInfo { version: version.to_string(), ver, legacy, server_charset, database_charset: None, decode, encoding, session: Vec::new(), user: conn.user.clone() }
    }

    #[cfg(feature = "mysql")]
    pub fn from_facts(f: &crate::source::mysql_common::ServerFacts, conn: &Connection) -> SourceInfo {
        use crate::source::mysql_dispatch::TextPlan;
        let decode = match &f.plan {
            TextPlan::ServerUtf8 { .. } => Decode::ServerUtf8,
            TextPlan::PassThrough { legacy, .. } => Decode::PassThrough(*legacy),
            TextPlan::Fixed { charset } => Decode::Fixed(*charset),
        };
        SourceInfo {
            version: f.version.raw.clone(),
            ver: MyVersion::parse(&f.version.raw),
            legacy: f.legacy_protocol || !f.version.at_least(4, 1, 0),
            server_charset: f.server_charset.clone(),
            database_charset: f.database_charset.clone(),
            decode,
            encoding: encoding_field(conn),
            session: f.plan.session_statements(),
            user: f.user.clone().or_else(|| conn.user.clone()),
        }
    }
}

#[cfg(feature = "mysql")]
fn source_info(input: &PlanInput<'_>) -> SourceInfo {
    match input.mysql_source {
        Some(f) => SourceInfo::from_facts(f, input.source_conn),
        None => SourceInfo::derive(&input.source_server.version, input.source_conn, None),
    }
}

#[cfg(not(feature = "mysql"))]
fn source_info(input: &PlanInput<'_>) -> SourceInfo {
    SourceInfo::derive(&input.source_server.version, input.source_conn, None)
}

// =============================================================================================
// Charset sampler (3.2)
// =============================================================================================

/// How a sampled column is decoded on the copy path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SampleKind {
    /// Decoded as latin1 (windows-1252).
    Latin1,
    /// Decoded as EUC-KR / CP949.
    EucKr,
    /// 4.0 `binary` attribute column, decoded with this charset.
    BinaryAttr(Charset),
    /// BLOB / TINYBLOB.
    Blob,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Finding {
    Latin1HoldsKorean,
    EucKrHoldsUtf8,
    BinaryUndecodable,
    BlobHoldsText,
}

/// Control bytes other than TAB, LF, CR: not text.
fn has_control(v: &[u8]) -> bool {
    v.iter().any(|b| *b < 0x20 && !matches!(b, 9 | 10 | 13))
}

fn is_hangul(c: char) -> bool {
    ('\u{AC00}'..='\u{D7A3}').contains(&c)
}

fn cp949_clean(v: &[u8]) -> Option<String> {
    if has_control(v) {
        return None;
    }
    encoding_rs::EUC_KR.decode_without_bom_handling_and_without_replacement(v).map(|c| c.into_owned())
}

/// Classify sampled raw values of one column (pure; values never leave the planner).
pub(crate) fn classify(kind: SampleKind, values: &[Vec<u8>]) -> Option<Finding> {
    let high: Vec<&Vec<u8>> = values.iter().filter(|v| v.iter().any(|b| *b >= 0x80)).collect();
    match kind {
        SampleKind::Latin1 | SampleKind::Blob => {
            if high.is_empty() {
                return None;
            }
            let mut clean = 0usize;
            let mut hangul = 0usize;
            for v in &high {
                if let Some(s) = cp949_clean(v) {
                    clean += 1;
                    if s.chars().any(is_hangul) {
                        hangul += 1;
                    }
                }
            }
            (clean * 10 >= high.len() * 9 && hangul >= 1 && hangul * 2 >= clean).then_some(if kind == SampleKind::Blob {
                Finding::BlobHoldsText
            } else {
                Finding::Latin1HoldsKorean
            })
        }
        SampleKind::EucKr => {
            if high.is_empty() {
                return None;
            }
            let utf = high.iter().filter(|v| std::str::from_utf8(v).is_ok_and(|s| s.chars().any(is_hangul))).count();
            (utf * 10 >= high.len() * 9).then_some(Finding::EucKrHoldsUtf8)
        }
        SampleKind::BinaryAttr(cs) => {
            let bad = values.iter().any(|v| {
                let control = has_control(v);
                let undecodable = match cs {
                    Charset::EucKr => cp949_clean(v).is_none(),
                    Charset::Utf8 => std::str::from_utf8(v).is_err(),
                    Charset::Latin1 | Charset::Binary => false,
                };
                control || undecodable
            });
            bad.then_some(Finding::BinaryUndecodable)
        }
    }
}

/// Which columns the sampler reads (3.2 "Charset sample").
pub(crate) fn sample_kind(decode: Decode, legacy: bool, col: &MyColumn, table_cs: &str) -> Option<SampleKind> {
    let ty = col.ty.as_str();
    if matches!(ty, "tinyblob" | "blob") {
        return Some(SampleKind::Blob);
    }
    let text = matches!(ty, "char" | "varchar" | "tinytext" | "text" | "mediumtext" | "longtext" | "enum" | "set");
    if !text {
        return None;
    }
    if col.binary_attr && legacy {
        return Some(SampleKind::BinaryAttr(match decode {
            Decode::Fixed(c) => c,
            _ => Charset::Latin1,
        }));
    }
    let col_cs = col.charset.as_deref().map(norm_cs).or_else(|| col.collation.as_deref().map(coll_charset)).unwrap_or_else(|| norm_cs(table_cs));
    match decode {
        Decode::Fixed(Charset::Latin1) => Some(SampleKind::Latin1),
        Decode::Fixed(Charset::EucKr) => Some(SampleKind::EucKr),
        Decode::Fixed(_) => None,
        Decode::ServerUtf8 | Decode::PassThrough(Charset::Latin1) if col_cs == "latin1" => Some(SampleKind::Latin1),
        Decode::PassThrough(Charset::Latin1) | Decode::ServerUtf8 | Decode::PassThrough(Charset::EucKr) if col_cs == "euckr" => Some(SampleKind::EucKr),
        _ => None,
    }
}

/// The sample SQL of one column.
pub(crate) fn sample_sql(legacy: bool, db: &str, table: &str, column: &str) -> String {
    let c = bq(column);
    if legacy {
        format!("SELECT {c} FROM {} WHERE {c} <> '' LIMIT 1000", bq2(db, table))
    } else {
        format!("SELECT CAST({c} AS BINARY) FROM {} WHERE {c} <> '' LIMIT 1000", bq2(db, table))
    }
}

const SAMPLE_TABLES: usize = 20;
const SAMPLE_COLUMNS: usize = 200;
const SAMPLE_BYTES: usize = 4 << 20;

/// The issue of a finding on table `t`, column `c`. Item blockers go to `blockers`.
fn finding_issue(f: Finding, column: &str, encoding: EncodingField, warnings: &mut Vec<CopyIssue>, blockers: &mut Vec<CopyIssue>) {
    match f {
        Finding::Latin1HoldsKorean => {
            let i = danger("SRC_LATIN1_KOREAN", format!("latin1 column `{column}` holds Korean (CP949) bytes")).rec("set the source connection's encoding to euc-kr");
            if encoding == EncodingField::Auto {
                blockers.push(i);
            } else {
                warnings.push(i);
            }
        }
        Finding::EucKrHoldsUtf8 => {
            let i = danger("SRC_EUCKR_HOLDS_UTF8", format!("euckr column `{column}` holds UTF-8 bytes")).rec("encoding utf8 on 4.0, or exclude the column's table");
            if encoding == EncodingField::Auto {
                blockers.push(i);
            } else {
                warnings.push(i);
            }
        }
        Finding::BinaryUndecodable => blockers.push(
            danger("BINARY_ATTR_UNDECODABLE", format!("binary-attribute column `{column}` holds bytes that are not text"))
                .rec(format!("mysql.columns treat_as: binary for `{column}` (varbinary / binary, loaded as bytes)")),
        ),
        Finding::BlobHoldsText => {
            warnings.push(info("BLOB_HOLDS_TEXT", format!("BLOB column `{column}` looks like Korean text")).rec(format!("mysql.columns treat_as: text for `{column}`")))
        }
    }
}

// =============================================================================================
// Target state
// =============================================================================================

fn ops() -> FamilyOps {
    FamilyOps {
        family: CopyFamily::Mysql,
        probe_sql: |k, _| format!("SELECT 1 FROM {} LIMIT 1", bq2(k.schema.as_deref().unwrap_or(""), &k.name)),
        count_sql: |k, n, _| format!("SELECT COUNT(*) FROM (SELECT 1 FROM {} LIMIT {n}) q", bq2(k.schema.as_deref().unwrap_or(""), &k.name)),
        drop_fk_sql: |k, name| Some(format!("ALTER TABLE {} DROP FOREIGN KEY {}", bq2(k.schema.as_deref().unwrap_or(""), &k.name), bq(name))),
        truncate_group_sql: None,
        quote_rel: |k| bq2(k.schema.as_deref().unwrap_or(""), &k.name),
    }
}

fn norm_name(s: &str, lower: bool) -> String {
    if lower {
        s.to_lowercase()
    } else {
        s.to_string()
    }
}

/// (state, target database exists, a database named like the source exists on the target server)
async fn read_state(input: &PlanInput<'_>, tdb: &str, src_db: &str, lower: bool, want_views: bool) -> Result<(TargetState, bool, bool)> {
    let mut st = TargetState::default();
    let dbs = input.target.query_meta(&format!("SELECT SCHEMA_NAME FROM information_schema.SCHEMATA WHERE SCHEMA_NAME IN ({}, {})", my_str(tdb), my_str(src_db))).await?;
    let names: Vec<&str> = (0..dbs.len()).filter_map(|r| dbs.get(r, "SCHEMA_NAME")).collect();
    let exists = names.iter().any(|n| *n == tdb || (lower && n.eq_ignore_ascii_case(tdb)));
    let src_named = names.contains(&src_db);
    if !exists {
        return Ok((st, false, src_named));
    }
    let lit = my_str(tdb);
    let tables = input.target.query_meta(&format!("SELECT TABLE_NAME, TABLE_TYPE, ENGINE, TABLE_ROWS FROM information_schema.TABLES WHERE TABLE_SCHEMA = {lit}")).await?;
    for r in 0..tables.len() {
        let Some(name) = tables.get(r, "TABLE_NAME") else { continue };
        st.objects.insert(
            TKey::rel(Some(tdb), &norm_name(name, lower)),
            TObj { kind: tables.get(r, "TABLE_TYPE").unwrap_or("BASE TABLE").to_string(), est_rows: meta_u64(tables.get(r, "TABLE_ROWS")), ..Default::default() },
        );
    }
    let cols = input
        .target
        .query_meta(&format!("SELECT TABLE_NAME, COLUMN_NAME, COLUMN_TYPE, IS_NULLABLE, COLUMN_DEFAULT, EXTRA FROM information_schema.COLUMNS WHERE TABLE_SCHEMA = {lit} ORDER BY TABLE_NAME, ORDINAL_POSITION"))
        .await?;
    for r in 0..cols.len() {
        let (Some(t), Some(c)) = (cols.get(r, "TABLE_NAME"), cols.get(r, "COLUMN_NAME")) else { continue };
        if let Some(o) = st.objects.get_mut(&TKey::rel(Some(tdb), &norm_name(t, lower))) {
            let extra = cols.get(r, "EXTRA").unwrap_or_default().to_ascii_lowercase();
            if extra.contains("generated") {
                continue;
            }
            o.columns.push(TCol {
                name: c.to_string(),
                type_text: cols.get(r, "COLUMN_TYPE").unwrap_or_default().to_string(),
                not_null: cols.get(r, "IS_NULLABLE") == Some("NO"),
                has_default: cols.get(r, "COLUMN_DEFAULT").is_some() || extra.contains("auto_increment"),
            });
        }
    }
    let trg = input.target.query_meta(&format!("SELECT TRIGGER_NAME, EVENT_OBJECT_TABLE FROM information_schema.TRIGGERS WHERE TRIGGER_SCHEMA = {lit}")).await?;
    for r in 0..trg.len() {
        let (Some(n), Some(t)) = (trg.get(r, "TRIGGER_NAME"), trg.get(r, "EVENT_OBJECT_TABLE")) else { continue };
        st.objects.insert(TKey::of(TClass::Trigger, Some(tdb), None, n), TObj { kind: "trigger".into(), ..Default::default() });
        if let Some(o) = st.objects.get_mut(&TKey::rel(Some(tdb), &norm_name(t, lower))) {
            o.triggers += 1;
        }
    }
    let fks = input
        .target
        .query_meta(&format!(
            "SELECT DISTINCT TABLE_SCHEMA, TABLE_NAME, CONSTRAINT_NAME, REFERENCED_TABLE_SCHEMA, REFERENCED_TABLE_NAME FROM information_schema.KEY_COLUMN_USAGE \
             WHERE REFERENCED_TABLE_NAME IS NOT NULL AND (TABLE_SCHEMA = {lit} OR REFERENCED_TABLE_SCHEMA = {lit})"
        ))
        .await?;
    for r in 0..fks.len() {
        let (Some(cs), Some(ct), Some(n), Some(ps), Some(pt)) = (
            fks.get(r, "TABLE_SCHEMA"),
            fks.get(r, "TABLE_NAME"),
            fks.get(r, "CONSTRAINT_NAME"),
            fks.get(r, "REFERENCED_TABLE_SCHEMA"),
            fks.get(r, "REFERENCED_TABLE_NAME"),
        ) else {
            continue;
        };
        st.fks.push(TFk { child: TKey::rel(Some(cs), &norm_name(ct, lower)), name: n.to_string(), parent: TKey::rel(Some(ps), &norm_name(pt, lower)) });
    }
    if want_views {
        let views = input
            .target
            // `INSTR`, not `LIKE`: a literal substring search needs no escape character, so it
            // works on the plan read session whatever the target's global sql_mode is. `LIKE`
            // with backslash-escaped `_`/`%` silently matches nothing under NO_BACKSLASH_ESCAPES.
            .query_meta(&format!("SELECT TABLE_SCHEMA, TABLE_NAME, VIEW_DEFINITION FROM information_schema.VIEWS WHERE INSTR(VIEW_DEFINITION, {}) > 0", my_str(&bq(tdb))))
            .await?;
        for r in 0..views.len() {
            let (Some(s), Some(v), Some(def)) = (views.get(r, "TABLE_SCHEMA"), views.get(r, "TABLE_NAME"), views.get(r, "VIEW_DEFINITION")) else { continue };
            for t in qualified_refs(def, tdb) {
                st.view_deps.push((TKey::rel(Some(s), &norm_name(v, lower)), TKey::rel(Some(tdb), &norm_name(&t, lower))));
            }
        }
    }
    Ok((st, true, src_named))
}

// =============================================================================================
// Collector
// =============================================================================================

struct SrcTable {
    name: String,
    engine: String,
    rows: Option<u64>,
    bytes: Option<u64>,
}

struct Translated {
    src: SrcTable,
    target_name: String,
    id: String,
    table: MyTable,
    out: MyTableOut,
    ddl: String,
    ov: MyTableOverrides,
}

/// A MySQL stored procedure, function or event (`what`: `procedure`, `function`, `event`) as an
/// `unsupported` plan item with `MYSQL_ROUTINES_NOT_COPIED` (3.3: never copied). Kind
/// `function` for all three; the id appends the kind word, since the three namespaces are
/// separate (`function:"cleanup"(procedure)`).
fn routine_item(src_db: &str, tdb: &str, what: &str, name: &str) -> Draft {
    let id = format!("{}({what})", item_id(CopyObjectKind::Function, &[name]));
    let mut d = Draft::new(CopyObjectKind::Function, id, name, name, CopyPhase::PreData);
    d.schema = Some(src_db.to_string());
    d.target_schema = Some(tdb.to_string());
    d.unsupported = true;
    d.warnings.push(
        warning("MYSQL_ROUTINES_NOT_COPIED", format!("the {what} {name} is not copied: stored procedures, functions and events are not supported"))
            .rec("create it on the target by hand after the copy"),
    );
    d
}

pub(crate) async fn collect(input: &PlanInput<'_>) -> Result<Collected> {
    let req = input.req;
    let opts = &req.options;
    let my = &opts.mysql;
    let src_db = req.source.database.clone();
    let tdb = req.target.database.clone();
    let tf = input.target.facts();
    let tver = MyVersion::of_facts(tf);
    let mut si = source_info(input);
    let mut warnings: Vec<CopyIssue> = Vec::new();
    let mut blockers: Vec<CopyIssue> = Vec::new();
    let mut src_notes: Vec<String> = Vec::new();
    if src_db.is_empty() || tdb.is_empty() {
        return Err(CoreError::Invalid("copy: MySQL copies need a source and a target database".into()));
    }

    // ---- 4.0 facts line (the contract SQL of the fake 4.0 server)
    if !si.ver.at(4, 1, 0) {
        if let Ok(m) = input.source.query("SHOW VARIABLES LIKE 'character_set'").await {
            if let Some(v) = m.get(0, "Value") {
                if si.server_charset.is_none() {
                    si = SourceInfo { server_charset: Some(v.to_string()), ..SourceInfo::derive(&si.version, input.source_conn, Some(v.to_string())) }.merge_session(&si);
                }
            }
        }
        if let Ok(m) = input.source.query("SHOW VARIABLES LIKE 'timezone'").await {
            if let Some(v) = m.get(0, "Value") {
                src_notes.push(format!("time zone {v}"));
            }
        }
    }
    check_cancel(input.cancel)?;

    // ---- versions
    if !tver.at(5, 0, 0) {
        blockers.push(danger(
            "TARGET_VERSION_UNSUPPORTED",
            format!("MySQL {} cannot be a target: targets must be 5.0 or newer (the legacy client never writes)", tver.label()),
        ));
    }
    if tver.maria || si.ver.maria {
        warnings.push(warning("MARIADB", "MariaDB is copied with the MySQL 5.7 rules"));
    }
    if tver.major == 6 || si.ver.major == 6 {
        warnings.push(warning("UNTESTED_VERSION", "MySQL 6.0 was a cancelled alpha; the 5.x rules apply"));
    }
    if !tver.maria && !si.ver.maria && (si.ver.major, si.ver.minor) > (tver.major, tver.minor) {
        warnings.push(danger("DOWNGRADE", format!("the source ({}) is newer than the target ({})", si.ver.label(), tver.label())));
    }
    if !si.ver.at(4, 1, 0) && si.encoding == EncodingField::Auto {
        let ok = si.server_charset.as_deref().and_then(Charset::from_mysql_name).is_some_and(|c| c != Charset::Binary);
        if !ok {
            blockers.push(
                danger("CHARSET_UNSUPPORTED", format!("the 4.0 server character set '{}' would be read as latin1", si.server_charset.as_deref().unwrap_or("unknown")))
                    .rec("set the source connection's encoding (euc-kr, utf8 or latin1) explicitly"),
            );
        }
    }

    // ---- effective charset, collation, zero dates
    let utf8mb4 = tver.at(5, 5, 3) || (tver.maria && tver.at(5, 5, 0));
    let charset = my.charset.as_deref().map(norm_cs).unwrap_or_else(|| if utf8mb4 { "utf8mb4".into() } else { "utf8".into() });
    let global_mode = tf.vars.get("sql_mode").cloned().unwrap_or_else(|| default_global_sql_mode(&tver).to_string());
    let zero = resolve_zero_dates(my.zero_dates, &global_mode);
    let mut tctx = MyTargetCtx {
        ver: tver,
        vars: tf.vars.clone(),
        db: tdb.clone(),
        charset: charset.clone(),
        user_collation: None,
        engine: my.engine,
        zero,
        indexes_after_data: opts.indexes_after_data,
        fix_row_size: my.fix_row_size,
    };
    tctx.user_collation = my.collation.as_deref().map(|c| if c.eq_ignore_ascii_case("legacy") { tctx.legacy_collation(&charset) } else { c.to_string() });
    // Every user-supplied charset / collation name (global and per-table overrides) is a bare
    // identifier the target knows. An unchecked name is interpolated straight into CREATE
    // DATABASE / CREATE TABLE, so a value with spaces or extra clauses would otherwise become
    // part of the DDL; the single-statement guard stops a second statement but not that. The
    // `legacy` collation preset is resolved by BulkDB and never sent as a name.
    {
        let valid = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
        let charsets: BTreeSet<&str> = std::iter::once(my.charset.as_deref()).chain(my.table_overrides.iter().map(|o| o.charset.as_deref())).flatten().collect();
        let collations: BTreeSet<&str> = std::iter::once(my.collation.as_deref())
            .chain(my.table_overrides.iter().map(|o| o.collation.as_deref()))
            .flatten()
            .filter(|c| !c.eq_ignore_ascii_case("legacy"))
            .collect();
        if !charsets.is_empty() {
            let m = input.target.query_meta("SHOW CHARACTER SET").await?;
            let known: BTreeSet<String> = (0..m.len()).filter_map(|r| m.get(r, "Charset").map(norm_cs)).collect();
            for cs in charsets {
                if !valid(&norm_cs(cs)) {
                    blockers.push(danger("CHARSET_UNSUPPORTED", format!("'{cs}' is not a valid character set name")));
                } else if !known.is_empty() && !known.contains(&norm_cs(cs)) {
                    blockers.push(danger("CHARSET_UNSUPPORTED", format!("the target has no character set '{cs}'")));
                }
            }
        }
        if !collations.is_empty() {
            let m = input.target.query_meta("SHOW COLLATION").await?;
            let known: BTreeSet<String> = (0..m.len()).filter_map(|r| m.get(r, "Collation").map(|c| c.to_ascii_lowercase())).collect();
            for coll in collations {
                if !valid(coll) {
                    blockers.push(danger("COLLATION_UNSUPPORTED", format!("'{coll}' is not a valid collation name")));
                } else if !known.is_empty() && !known.contains(&coll.to_ascii_lowercase()) {
                    blockers.push(danger("COLLATION_UNSUPPORTED", format!("the target has no collation '{coll}'")));
                }
            }
        }
    }
    let lower = tctx.lower_case_names();

    // ---- source tables and views
    let status = input.source.query(&format!("SHOW TABLE STATUS FROM {}", bq(&src_db))).await?;
    let mut view_names: BTreeSet<String> = BTreeSet::new();
    if si.ver.at(5, 0, 0) {
        let v = input.source.query(&format!("SHOW FULL TABLES FROM {} WHERE Table_type = 'VIEW'", bq(&src_db))).await?;
        for r in &v.rows {
            if let Some(Some(n)) = r.first() {
                view_names.insert(n.clone());
            }
        }
    }
    let mut src_tables: Vec<SrcTable> = Vec::new();
    for r in 0..status.len() {
        let Some(name) = status.get(r, "Name") else { continue };
        let engine = status.get(r, "Engine").or_else(|| status.get(r, "Type"));
        if view_names.contains(name) || (engine.is_none() && status.get(r, "Comment").is_some_and(|c| c.eq_ignore_ascii_case("VIEW"))) {
            view_names.insert(name.to_string());
            continue;
        }
        let bytes = match (meta_u64(status.get(r, "Data_length")), meta_u64(status.get(r, "Index_length"))) {
            (Some(a), Some(b)) => Some(a + b),
            (a, b) => a.or(b),
        };
        src_tables.push(SrcTable { name: name.to_string(), engine: engine.unwrap_or("MyISAM").to_string(), rows: meta_u64(status.get(r, "Rows")), bytes });
    }
    let renamed: BTreeMap<String, String> = src_tables.iter().map(|t| (t.name.clone(), target_table_name(opts, None, &t.name, lower))).collect();
    let srcctx =
        MySourceCtx { ver: si.ver, db: src_db.clone(), text_protocol: si.legacy, server_charset: si.server_charset.clone().or_else(|| si.database_charset.clone()) };

    // ---- per table: SHOW CREATE TABLE, parse, translate
    let mut drafts: Vec<Draft> = Vec::new();
    let mut translated: Vec<Translated> = Vec::new();
    let tid = |n: &str| item_id(CopyObjectKind::Table, &[n]);
    for t in src_tables {
        check_cancel(input.cancel)?;
        let target_name = renamed.get(&t.name).cloned().unwrap_or_else(|| t.name.clone());
        let id = tid(&t.name);
        if !selected(req, None, &t.name) {
            let mut d = Draft::new(CopyObjectKind::Table, id, &t.name, &target_name, CopyPhase::PreData);
            d.selected = false;
            drafts.push(d);
            continue;
        }
        let m = input.source.query(&format!("SHOW CREATE TABLE {}", bq2(&src_db, &t.name))).await?;
        let ddl = m.get(0, "Create Table").map(str::to_string).or_else(|| m.rows.first().and_then(|r| r.get(1)).cloned().flatten()).unwrap_or_default();
        let table = parse_create_table(&ddl);
        let mut ov = MyTableOverrides::default();
        if let Some(o) = my.table_overrides.iter().find(|o| o.table == t.name) {
            ov.charset = o.charset.clone();
            ov.collation = o.collation.clone();
        }
        for c in my.columns.iter().filter(|c| c.table == t.name) {
            ov.columns.insert(c.column.clone(), c.treat_as);
        }
        // TIMESTAMP DEFAULT literals before 4.1.3 are local wall time: converted on the source.
        if srcctx.epoch_timestamps() {
            for c in table.columns.iter().filter(|c| c.ty == "timestamp") {
                if let Some(MyDefault::Literal(v)) = &c.default {
                    if v.chars().any(|ch| ch.is_ascii_digit() && ch != '0') {
                        if let Ok(m) = input.source.query(&format!("SELECT UNIX_TIMESTAMP({})", my_str(v))).await {
                            let epoch = m.rows.first().and_then(|r| r.first()).and_then(|v| v.as_deref()).and_then(|s| s.trim().parse::<i64>().ok());
                            if let Some(utc) = epoch.and_then(crate::copy::data::values::format_epoch_utc) {
                                ov.timestamp_defaults_utc.insert(c.name.clone(), utc);
                            }
                        }
                    }
                }
            }
        }
        let out = translate_table(&table, &target_name, &srcctx, &tctx, &ov, &renamed);
        translated.push(Translated { src: t, target_name, id, table, out, ddl, ov });
    }

    // ---- a 3-byte utf8 target cannot hold 4-byte characters of a UTF-8 source
    let utf8_decode = matches!(si.decode, Decode::Fixed(Charset::Utf8));
    let holds_4byte = |t: &Translated| {
        utf8_decode
            || t.table.option("CHARSET").map(norm_cs).as_deref() == Some("utf8mb4")
            || t.table.columns.iter().any(|c| c.charset.as_deref().map(norm_cs).as_deref() == Some("utf8mb4"))
    };
    let utf8mb4_source = utf8_decode || translated.iter().any(holds_4byte);
    if (charset == "utf8" || charset == "utf8mb3") && utf8mb4_source {
        warnings.push(warning("UTF8MB3_ONLY", "the target charset holds only 3-byte UTF-8: 4-byte characters (emoji, rare CJK) are replaced by '?' and counted"));
    } else {
        // A table override to utf8 / utf8mb3 on a utf8mb4 copy: the same rule for that table.
        for t in translated.iter_mut() {
            if t.ov.charset.as_deref().is_some_and(crate::copy::data::mysql_encode::is_three_byte_charset) && holds_4byte(t) {
                t.out.warnings.push(warning(
                    "UTF8MB3_ONLY",
                    "this table's target charset holds only 3-byte UTF-8: 4-byte characters (emoji, rare CJK) are replaced by '?' and counted",
                ));
            }
        }
    }

    // ---- charset sampler
    let mut sample_w: BTreeMap<String, Vec<CopyIssue>> = BTreeMap::new();
    let mut sample_b: BTreeMap<String, Vec<CopyIssue>> = BTreeMap::new();
    {
        let mut order: Vec<usize> = (0..translated.len()).filter(|&i| translated[i].out.unsupported.is_none()).collect();
        order.sort_by_key(|&i| std::cmp::Reverse(translated[i].src.bytes.unwrap_or(0)));
        let mut cols_left = SAMPLE_COLUMNS;
        let mut bytes_left = SAMPLE_BYTES;
        'tables: for &i in order.iter().take(SAMPLE_TABLES) {
            let tr = &translated[i];
            let table_cs = tr
                .table
                .option("CHARSET")
                .map(str::to_string)
                .or_else(|| si.database_charset.clone())
                .or_else(|| si.server_charset.clone())
                .unwrap_or_else(|| "latin1".into());
            for col in &tr.table.columns {
                if tr.ov.columns.contains_key(&col.name) || col.generated.is_some() {
                    continue;
                }
                let Some(kind) = sample_kind(si.decode, si.legacy, col, &table_cs) else { continue };
                if cols_left == 0 || bytes_left == 0 {
                    break 'tables;
                }
                cols_left -= 1;
                check_cancel(input.cancel)?;
                let values = match input.source.query_raw(&sample_sql(si.legacy, &src_db, &tr.src.name, &col.name)).await {
                    Ok(v) => v,
                    Err(CoreError::Cancelled) => return Err(CoreError::Cancelled),
                    Err(_) => continue,
                };
                let mut vals: Vec<Vec<u8>> = Vec::new();
                for v in values.into_iter().flatten() {
                    if v.len() > bytes_left {
                        bytes_left = 0;
                        break;
                    }
                    bytes_left -= v.len();
                    vals.push(v);
                }
                if let Some(f) = classify(kind, &vals) {
                    let w = sample_w.entry(tr.id.clone()).or_default();
                    let b = sample_b.entry(tr.id.clone()).or_default();
                    finding_issue(f, &col.name, si.encoding, w, b);
                }
            }
        }
    }

    // ---- target state
    let want_views = opts.existing == ExistingTablePolicy::DropRecreate || opts.existing_overrides.iter().any(|o| o.policy == ExistingTablePolicy::DropRecreate);
    let (target, db_exists, src_named_on_target) = read_state(input, &tdb, &src_db, lower, want_views).await?;
    let method = crate::copy::data::mysql_encode::method_for(tf);

    // ---- database item
    if !db_exists {
        let coll = tctx.user_collation.clone().or_else(|| tctx.default_collation(&charset));
        let mut d = Draft::new(CopyObjectKind::Database, item_id(CopyObjectKind::Database, &[&tdb]), &tdb, &tdb, CopyPhase::PreData);
        d.create = vec![format!("CREATE DATABASE {} DEFAULT CHARACTER SET {charset}{}", bq(&tdb), coll.map(|c| format!(" COLLATE {c}")).unwrap_or_default())];
        d.warnings.push(info("DATABASE_CREATED", format!("the target database {tdb} does not exist and is created")));
        drafts.push(d);
    }

    // ---- table drafts
    let engines: BTreeMap<String, String> = translated.iter().map(|t| (t.src.name.clone(), t.out.engine.clone())).collect();
    let uniques: BTreeMap<String, Vec<Vec<String>>> = translated.iter().map(|t| (t.src.name.clone(), t.out.unique_keys.clone())).collect();
    let all_table_ids: Vec<String> = translated.iter().map(|t| t.id.clone()).collect();
    let mut any_ai_innodb = false;
    let mut rebuild_tables: Vec<String> = Vec::new();
    let lock_timeout = opts.target_lock_timeout_s.unwrap_or(5).clamp(1, 600);
    for tr in &translated {
        let out = &tr.out;
        let mut d = Draft::new(CopyObjectKind::Table, tr.id.clone(), &tr.src.name, &tr.target_name, CopyPhase::PreData);
        d.schema = Some(src_db.clone());
        d.target_schema = Some(tdb.clone());
        d.exists_key = Some(TKey::rel(Some(&tdb), &norm_name(&tr.target_name, lower)));
        d.source_ddl = Some(redact_connection(&tr.ddl));
        d.est_rows = tr.src.rows;
        d.est_bytes = tr.src.bytes;
        d.warnings = out.warnings.clone();
        d.blockers = out.blockers.clone();
        d.warnings.extend(sample_w.remove(&tr.id).unwrap_or_default());
        d.blockers.extend(sample_b.remove(&tr.id).unwrap_or_default());
        if let Some(u) = &out.unsupported {
            d.unsupported = true;
            d.warnings.push(u.clone());
            drafts.push(d);
            continue;
        }
        d.create = vec![out.create_sql.clone()];
        let target_q = bq2(&tdb, &tr.target_name);
        if out.disable_keys && opts.content != CopyContent::SchemaOnly {
            d.create.push(format!("ALTER TABLE {target_q} DISABLE KEYS"));
        }
        d.drop = vec![format!("DROP TABLE {target_q}")];
        d.truncate = Some(format!("TRUNCATE TABLE {target_q}"));
        let spec = TableDataSpec {
            item_id: tr.id.clone(),
            source_sql: out.select_sql.clone(),
            verify_sql: out.verify_sql.clone(),
            raw_csv: false,
            transcode: None,
            target: TargetTable { schema: Some(tdb.clone()), name: tr.target_name.clone() },
            columns: out.columns.clone(),
            est_rows: tr.src.rows,
            first_timestamp_auto: out.first_timestamp_auto,
            fk_checks: false,
        };
        d.load_template = Some(if method == LoadMethod::MysqlLoadData {
            crate::copy::data::mysql_encode::load_data_sql(&spec, tf, "<stream>")
        } else {
            format!("{}<rows>", crate::copy::data::mysql_encode::insert_prefix(&spec))
        });
        d.data = Some(spec);
        drafts.push(d);

        // Secondary keys after the data (one combined ALTER per table), MyISAM ENABLE KEYS.
        let fk_merge = !tctx.inplace_fk() && !out.fks.is_empty();
        if fk_merge {
            rebuild_tables.push(format!("{} ({} bytes)", tr.target_name, tr.src.bytes.unwrap_or(0)));
        }
        for k in &out.post_keys {
            let mut kd = Draft::new(CopyObjectKind::Index, item_id(CopyObjectKind::Index, &[&tr.src.name, &k.name]), &k.name, &k.name, CopyPhase::PostData);
            kd.schema = Some(src_db.clone());
            kd.target_schema = Some(tdb.clone());
            kd.parent = Some(tr.id.clone());
            kd.depends_on = vec![tr.id.clone()];
            kd.warnings = k.warnings.clone();
            let g = format!("{target_q}|{}", if fk_merge { "all" } else { "keys" });
            kd.group = Some((g, k.clause.clone(), k.separate));
            drafts.push(kd);
        }
        if out.disable_keys && opts.content != CopyContent::SchemaOnly {
            let mut kd = Draft::new(
                CopyObjectKind::Index,
                item_id(CopyObjectKind::Index, &[&tr.src.name, "(enable keys)"]),
                "(enable keys)",
                "(enable keys)",
                CopyPhase::PostData,
            );
            kd.schema = Some(src_db.clone());
            kd.target_schema = Some(tdb.clone());
            kd.parent = Some(tr.id.clone());
            kd.depends_on = vec![tr.id.clone()];
            kd.create = vec![format!("ALTER TABLE {target_q} ENABLE KEYS")];
            drafts.push(kd);
        }
        // Foreign keys: one combined ALTER per child table after every load.
        for fk in &out.fks {
            let mut fd = Draft::new(CopyObjectKind::ForeignKey, item_id(CopyObjectKind::ForeignKey, &[&tr.src.name, &fk.name]), &fk.name, &fk.name, CopyPhase::PostData);
            fd.schema = Some(src_db.clone());
            fd.target_schema = Some(tdb.clone());
            fd.parent = Some(tr.id.clone());
            fd.depends_on = vec![tr.id.clone()];
            if fk.ref_db.is_none() {
                let pid = tid(&fk.ref_table);
                if pid != tr.id {
                    fd.depends_on.push(pid.clone());
                }
                fd.fk_parent = Some(pid);
            }
            fd.source_ddl = Some(fk.clause.clone());
            let parent_engine = if fk.ref_db.is_none() { engines.get(&fk.ref_table).cloned() } else { None };
            if out.engine != "InnoDB" || parent_engine.as_deref().is_some_and(|e| e != "InnoDB") {
                fd.unsupported = true;
                fd.warnings.push(warning(
                    "FK_ON_MYISAM",
                    "MySQL parses and ignores foreign keys on MyISAM (and an InnoDB child cannot reference a MyISAM parent): the key is not created",
                ));
            } else if tctx.restrict_fk_non_standard() && fk.ref_db.is_none() {
                let refs: Vec<String> = fk.ref_columns.iter().map(|c| c.to_lowercase()).collect();
                let standard =
                    uniques.get(&fk.ref_table).is_some_and(|keys| keys.iter().any(|k| k.len() == refs.len() && k.iter().all(|c| refs.contains(&c.to_lowercase()))));
                if !standard {
                    fd.unsupported = true;
                    fd.warnings.push(warning(
                        "FK_NON_STANDARD_KEY",
                        "the target refuses foreign keys whose parent columns are not a full PRIMARY or UNIQUE key (restrict_fk_on_non_standard_key)",
                    ));
                }
            }
            if fk.ref_db.is_some() {
                fd.warnings.push(info("FK_OTHER_DATABASE", format!("the foreign key references database {}, which is not copied", fk.ref_db.as_deref().unwrap_or(""))));
            }
            if !fd.unsupported {
                let g = format!("{target_q}|{}", if fk_merge { "all" } else { "fks" });
                fd.group = Some((g, fk.clause.clone(), false));
            }
            drafts.push(fd);
        }
        // Finalize: AUTO_INCREMENT, ANALYZE.
        if out.auto_increment.is_some() {
            if out.engine == "InnoDB" && !tver.at(8, 0, 0) {
                any_ai_innodb = true;
            }
            let mut a =
                Draft::new(CopyObjectKind::AutoIncrement, item_id(CopyObjectKind::AutoIncrement, &[&tr.src.name]), &tr.src.name, &tr.target_name, CopyPhase::Finalize);
            a.schema = Some(src_db.clone());
            a.target_schema = Some(tdb.clone());
            a.parent = Some(tr.id.clone());
            a.depends_on = vec![tr.id.clone()];
            let tpl = format!("ALTER TABLE {target_q} AUTO_INCREMENT = <source value>");
            a.seq_templates = Some((tpl.clone(), tpl));
            drafts.push(a);
        }
        if opts.objects.analyze {
            let mut a = Draft::new(CopyObjectKind::Analyze, item_id(CopyObjectKind::Analyze, &[&tr.src.name]), &tr.src.name, &tr.target_name, CopyPhase::Finalize);
            a.schema = Some(src_db.clone());
            a.target_schema = Some(tdb.clone());
            a.parent = Some(tr.id.clone());
            a.depends_on = vec![tr.id.clone()];
            a.create = vec![format!("ANALYZE TABLE {target_q}")];
            drafts.push(a);
        }
    }
    let _ = &all_table_ids;
    if any_ai_innodb {
        warnings.push(info("AUTOINC_NOT_PERSISTED", "InnoDB before 8.0 resets AUTO_INCREMENT to max + 1 on restart, so copied gaps are lost after a restart"));
    }
    if !rebuild_tables.is_empty() {
        warnings.push(
            info("FK_VALIDATE_REBUILD", "the target rebuilds a table on every ALTER (before 5.6): keys and foreign keys go into one ALTER per table after all loads")
                .detail(rebuild_tables.join(", ")),
        );
    }
    let renamed_src: BTreeSet<&String> = renamed.iter().filter(|(a, b)| a != b).map(|(a, _)| a).collect();

    // ---- views
    let keep_definer = my.keep_definer;
    let mut definer_ok: BTreeMap<(String, String), bool> = BTreeMap::new();
    if opts.objects.views && si.ver.at(5, 0, 0) {
        for v in &view_names {
            check_cancel(input.cancel)?;
            let target_name = target_table_name(opts, None, v, lower);
            let id = item_id(CopyObjectKind::View, &[v]);
            let m = input.source.query(&format!("SHOW CREATE VIEW {}", bq2(&src_db, v))).await?;
            let create = m.get(0, "Create View").map(str::to_string).unwrap_or_default();
            let refs = qualified_refs(&create, &src_db);
            let (stripped, definer) = view_create_sql(&create, &src_db, &tdb, &target_name, false);
            let mut d = Draft::new(CopyObjectKind::View, id, v, &target_name, CopyPhase::PreData);
            d.schema = Some(src_db.clone());
            d.target_schema = Some(tdb.clone());
            d.exists_key = Some(TKey::rel(Some(&tdb), &norm_name(&target_name, lower)));
            d.source_ddl = Some(create.clone());
            d.drop = vec![format!("DROP VIEW {}", bq2(&tdb, &target_name))];
            for r in &refs {
                if r == v {
                    continue;
                }
                if view_names.contains(r) {
                    d.depends_on.push(item_id(CopyObjectKind::View, &[r]));
                } else if renamed.contains_key(r) {
                    d.depends_on.push(tid(r));
                }
            }
            if refs.iter().any(|r| renamed_src.contains(r)) {
                d.warnings.push(
                    warning("RENAMED_REFS", "the view references renamed tables by their old names; it is not rewritten").rec("fix the view by hand after the copy"),
                );
            }
            let sql = match (&definer, keep_definer) {
                (Some(def), true) => {
                    let ok = match definer_ok.get(def) {
                        Some(ok) => *ok,
                        None => {
                            let ok = definer_exists(input, def).await?;
                            definer_ok.insert(def.clone(), ok);
                            ok
                        }
                    };
                    if ok {
                        view_create_sql(&create, &src_db, &tdb, &target_name, true).0
                    } else {
                        d.warnings.push(definer_warning(def, false));
                        stripped
                    }
                }
                (Some(def), false) => {
                    d.warnings.push(definer_warning(def, true));
                    stripped
                }
                (None, _) => stripped,
            };
            d.create = vec![sql];
            drafts.push(d);
        }
    }

    // ---- triggers (5.0.10+)
    let mut any_trigger = false;
    if opts.objects.triggers && si.ver.at(5, 0, 10) {
        let m = input.source.query(&format!("SHOW TRIGGERS FROM {}", bq(&src_db))).await?;
        let global = tf.vars.get("sql_mode").map(String::as_str).unwrap_or("");
        let restore_mode = crate::copy::data::mysql_encode::target_sql_mode(global, zero == ZeroDatePolicy::Keep);
        let names = crate::copy::data::mysql_encode::connection_charset(tf);
        for r in 0..m.len() {
            check_cancel(input.cancel)?;
            let (Some(name), Some(table)) = (m.get(r, "Trigger"), m.get(r, "Table")) else { continue };
            let table_target = renamed.get(table).cloned().unwrap_or_else(|| table.to_string());
            let parent = tid(table);
            let mut d = Draft::new(CopyObjectKind::Trigger, item_id(CopyObjectKind::Trigger, &[table, name]), name, name, CopyPhase::PostData);
            d.schema = Some(src_db.clone());
            d.target_schema = Some(tdb.clone());
            d.parent = Some(parent.clone());
            d.depends_on = vec![parent];
            d.exists_key = Some(TKey::of(TClass::Trigger, Some(&tdb), None, name));
            d.drop = vec![format!("DROP TRIGGER {}", bq2(&tdb, name))];
            let (create, mode, coll) = if si.ver.at(5, 1, 21) {
                let c = input.source.query(&format!("SHOW CREATE TRIGGER {}", bq2(&src_db, name))).await?;
                (c.get(0, "SQL Original Statement").map(str::to_string), c.get(0, "sql_mode").map(str::to_string), c.get(0, "collation_connection").map(str::to_string))
            } else {
                match (m.get(r, "Timing"), m.get(r, "Event"), m.get(r, "Statement")) {
                    (Some(ti), Some(ev), Some(st)) => (Some(trigger_from_columns(name, ti, ev, table, st)), m.get(r, "sql_mode").map(str::to_string), None),
                    _ => (None, None, None),
                }
            };
            let Some(create) = create else {
                d.unsupported = true;
                d.warnings.push(warning("TRIGGER_UNREADABLE", "the trigger's statement cannot be read on this source version"));
                drafts.push(d);
                continue;
            };
            any_trigger = true;
            d.source_ddl = Some(create.clone());
            let (mut sql, definer, body_refs) = trigger_create_sql(&create, &src_db, &tdb, &table_target, false);
            if let Some(def) = &definer {
                let ok = if keep_definer {
                    match definer_ok.get(def) {
                        Some(ok) => *ok,
                        None => {
                            let ok = definer_exists(input, def).await?;
                            definer_ok.insert(def.clone(), ok);
                            ok
                        }
                    }
                } else {
                    false
                };
                if ok {
                    sql = trigger_create_sql(&create, &src_db, &tdb, &table_target, true).0;
                } else {
                    d.warnings.push(definer_warning(def, !keep_definer));
                }
            }
            if body_refs {
                let i = danger("DB_QUALIFIED_REFS", format!("the trigger body names the source database {src_db}; it is not rewritten"))
                    .rec("exclude the trigger or fix it by hand after the copy");
                if src_named_on_target {
                    d.blockers.push(i);
                } else {
                    let mut w = i;
                    w.severity = crate::types::Severity::Warning;
                    d.warnings.push(w);
                }
            }
            if renamed_src.iter().any(|t| lex(&create).iter().any(|tok| (tok.k == TK::Ident || tok.k == TK::Word) && &&tok.v == t)) {
                d.warnings.push(warning("RENAMED_REFS", "the trigger references renamed tables by their old names; it is not rewritten"));
            }
            let mut stmts = Vec::new();
            if let Some(mode) = &mode {
                stmts.push(format!("SET SESSION sql_mode = {}", my_str(&trigger_sql_mode(mode, &tctx))));
            }
            if coll.as_deref().is_some_and(|c| c.to_ascii_lowercase().ends_with("_bin")) {
                stmts.push(format!("SET SESSION collation_connection = '{names}_bin'"));
            }
            stmts.push(sql);
            if mode.is_some() {
                stmts.push(format!("SET SESSION sql_mode = {}", my_str(&restore_mode)));
            }
            if coll.is_some() {
                stmts.push(format!("SET NAMES {names}"));
            }
            d.create = stmts;
            drafts.push(d);
        }
    }
    if any_trigger
        && tf.vars.get("log_bin").is_some_and(|v| v.eq_ignore_ascii_case("ON") || v == "1")
        && tf.vars.get("log_bin_trust_function_creators").is_none_or(|v| v.eq_ignore_ascii_case("OFF") || v == "0")
    {
        warnings.push(warning("TGT_BINLOG_TRIGGERS", "the target writes a binary log and does not trust function creators: CREATE TRIGGER may need the SUPER privilege"));
    }

    // ---- routines and events (5.0+): listed one by one as `unsupported` items, never copied.
    // Copying the whole database (selection `all`) is the only case where they were asked for.
    if si.ver.at(5, 0, 0) && matches!(req.selection, CopySelection::All) {
        // (kind word, name): procedures, functions and events have separate namespaces.
        let mut found: Vec<(String, String)> = Vec::new();
        if let Ok(m) = input.source.query(&format!("SELECT ROUTINE_TYPE, ROUTINE_NAME FROM information_schema.ROUTINES WHERE ROUTINE_SCHEMA = {}", my_str(&src_db))).await
        {
            for r in 0..m.len() {
                if let Some(name) = m.get(r, "ROUTINE_NAME") {
                    found.push((m.get(r, "ROUTINE_TYPE").unwrap_or("ROUTINE").to_ascii_lowercase(), name.to_string()));
                }
            }
        }
        if si.ver.at(5, 1, 6) {
            if let Ok(m) = input.source.query(&format!("SELECT EVENT_NAME FROM information_schema.EVENTS WHERE EVENT_SCHEMA = {}", my_str(&src_db))).await {
                for r in 0..m.len() {
                    if let Some(name) = m.get(r, "EVENT_NAME") {
                        found.push(("event".to_string(), name.to_string()));
                    }
                }
            }
        }
        for (what, name) in found {
            drafts.push(routine_item(&src_db, &tdb, &what, &name));
        }
    }

    // ---- facts, effective, session SQL
    let selected_engines: Vec<&str> = translated.iter().map(|t| t.src.engine.as_str()).collect();
    let c = crate::copy::plan::concurrency(opts, CopyFamily::Mysql);
    let consistent = c == 1 && si.ver.at(4, 1, 8) && !selected_engines.is_empty() && selected_engines.iter().all(|e| e.eq_ignore_ascii_case("InnoDB"));
    let mut source_session = si.session.clone();
    if (!si.ver.maria && si.ver.at(5, 6, 5)) || (si.ver.maria && si.ver.at(10, 0, 0)) {
        source_session.push("SET SESSION TRANSACTION READ ONLY".into());
    }
    if si.ver.at(4, 1, 3) {
        source_session.push("SET time_zone = '+00:00'".into());
    }
    let mut tnotes =
        vec!["MySQL 6.x and 7.x were never released as GA (6.0 was a cancelled alpha, 7.x is NDB Cluster numbering): supported targets are 5.0-5.7, 8.0, 8.4 and 9.x"
            .to_string()];
    if tf.version.contains("-ndb-") {
        tnotes.push(format!("NDB Cluster reports MySQL {}: that version's rules apply", tver.label()));
    }
    if si.version.contains("-ndb-") {
        src_notes.push(format!("NDB Cluster reports MySQL {}: that version's rules apply", si.ver.label()));
    }
    let engine_label = match my.engine {
        MysqlEngineChoice::Auto if tctx.innodb_default() => "InnoDB".to_string(),
        MysqlEngineChoice::Auto | MysqlEngineChoice::Keep => "source engine".to_string(),
        MysqlEngineChoice::Innodb => "InnoDB".to_string(),
        MysqlEngineChoice::Myisam => "MyISAM".to_string(),
    };
    let collation = tctx.user_collation.clone().or_else(|| tctx.default_collation(&charset));
    let (chunk_bytes, _) = crate::copy::data::mysql_encode::chunk_limits(tf);
    Ok(Collected {
        drafts,
        warnings,
        blockers,
        source_facts: CopyServerFacts {
            driver: "mysql".into(),
            version: si.version.clone(),
            database: src_db.clone(),
            endpoint: crate::copy::target::endpoint_text(input.source_conn),
            charset: si.database_charset.clone().or_else(|| si.server_charset.clone()),
            collation: None,
            user: si.user.clone(),
            notes: src_notes,
        },
        target_facts: CopyServerFacts {
            driver: "mysql".into(),
            version: tf.version.clone(),
            database: tdb.clone(),
            endpoint: if tf.endpoint.is_empty() { crate::copy::target::endpoint_text(input.target_conn) } else { tf.endpoint.clone() },
            charset: tf.vars.get("character_set_server").cloned(),
            collation: tf.vars.get("collation_server").cloned(),
            user: tf.user.clone(),
            notes: tnotes,
        },
        effective: CopyEffective {
            charset: Some(charset.clone()),
            collation,
            engine: Some(engine_label),
            zero_dates: Some(zero),
            time_zone: Some("+00:00".into()),
            load_method: method,
            table_concurrency: c,
            chunk_bytes: Some(chunk_bytes),
            consistent_snapshot: consistent,
            source_sessions: 1,
            target_sessions: 2,
        },
        session_sql: SessionSql { source: source_session, target: crate::copy::data::mysql_encode::session_prelude(tf, zero, lock_timeout) },
        target,
        ops: ops(),
        target_db_missing: !db_exists,
    })
}

impl SourceInfo {
    /// Keep the live session statements and user of `live` when re-deriving.
    fn merge_session(mut self, live: &SourceInfo) -> SourceInfo {
        if !live.session.is_empty() {
            self.session = live.session.clone();
        }
        if live.user.is_some() {
            self.user = live.user.clone();
        }
        self
    }
}

fn definer_warning(def: &(String, String), stripped: bool) -> CopyIssue {
    let who = format!("`{}`@`{}`", def.0, def.1);
    if stripped {
        warning("DEFINER_CHANGED", format!("DEFINER={who} is removed: the object runs with the copy account's privileges"))
            .rec("mysql.keep_definer keeps DEFINER for accounts that exist on the target")
    } else {
        warning("DEFINER_CHANGED", format!("the account {who} does not exist on the target: DEFINER is removed"))
    }
}

async fn definer_exists(input: &PlanInput<'_>, def: &(String, String)) -> Result<bool> {
    match input.target.query_meta(&format!("SELECT 1 FROM mysql.user WHERE User = {} AND Host = {}", my_str(&def.0), my_str(&def.1))).await {
        Ok(m) => Ok(!m.is_empty()),
        Err(CoreError::Cancelled) => Err(CoreError::Cancelled),
        Err(_) => Ok(false),
    }
}

#[cfg(test)]
mod tests;
