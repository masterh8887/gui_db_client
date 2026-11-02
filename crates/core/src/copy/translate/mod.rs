//! DDL translation per family and version (section 3): MySQL `SHOW CREATE TABLE` parsing and
//! type / charset / collation / engine rules ([`mysql`]), PG statement builders ([`pg`]),
//! SQLite verbatim DDL, and the finalize statements ([`finalize_sql`]). Owner: copy-schema.
//!
//! Shared helpers for the planner live here too: plan issues ([`issue`]), plan item ids
//! ([`item_id`]) and the finalize templates.

pub mod mysql;
pub mod mysql_reserved;
pub mod pg;

use crate::copy::types::{CopyIssue, CopyObjectKind, PlanItem};
use crate::types::Severity;

// ---------------------------------------------------------------------------------------------
// Issues
// ---------------------------------------------------------------------------------------------

/// A plan issue with an English message (the UI titles known codes itself).
pub fn issue(code: &str, severity: Severity, message: impl Into<String>) -> CopyIssue {
    CopyIssue { code: code.to_string(), severity, message: message.into(), detail: None, recommendation: None, item_id: None }
}

/// Builder methods on [`CopyIssue`].
pub trait IssueExt: Sized {
    fn detail(self, detail: impl Into<String>) -> Self;
    fn rec(self, recommendation: impl Into<String>) -> Self;
    fn on(self, item_id: &str) -> Self;
}

impl IssueExt for CopyIssue {
    fn detail(mut self, detail: impl Into<String>) -> Self {
        let d = detail.into();
        if !d.is_empty() {
            self.detail = Some(d);
        }
        self
    }

    fn rec(mut self, recommendation: impl Into<String>) -> Self {
        self.recommendation = Some(recommendation.into());
        self
    }

    fn on(mut self, item_id: &str) -> Self {
        self.item_id = Some(item_id.to_string());
        self
    }
}

pub fn info(code: &str, message: impl Into<String>) -> CopyIssue {
    issue(code, Severity::Info, message)
}

pub fn warning(code: &str, message: impl Into<String>) -> CopyIssue {
    issue(code, Severity::Warning, message)
}

pub fn danger(code: &str, message: impl Into<String>) -> CopyIssue {
    issue(code, Severity::Danger, message)
}

// ---------------------------------------------------------------------------------------------
// Plan item ids (section 2 "Plan item ids")
// ---------------------------------------------------------------------------------------------

/// Wire name of a kind (`table`, `foreign_key`, `truncate_group`, ...).
pub fn kind_name(kind: CopyObjectKind) -> &'static str {
    use CopyObjectKind::*;
    match kind {
        Database => "database",
        Schema => "schema",
        Extension => "extension",
        Collation => "collation",
        Type => "type",
        Domain => "domain",
        Sequence => "sequence",
        Function => "function",
        Table => "table",
        Partition => "partition",
        View => "view",
        MaterializedView => "materialized_view",
        Index => "index",
        Constraint => "constraint",
        ForeignKey => "foreign_key",
        Trigger => "trigger",
        Policy => "policy",
        Comment => "comment",
        Owner => "owner",
        Grant => "grant",
        TruncateGroup => "truncate_group",
        SequenceValue => "sequence_value",
        AutoIncrement => "auto_increment",
        Refresh => "refresh",
        Analyze => "analyze",
    }
}

/// `"a"."b"` (each name double-quoted, `"` doubled).
pub fn id_path(names: &[&str]) -> String {
    names.iter().map(|n| format!("\"{}\"", n.replace('"', "\"\""))).collect::<Vec<_>>().join(".")
}

/// `<kind>:<path>`.
pub fn item_id(kind: CopyObjectKind, names: &[&str]) -> String {
    format!("{}:{}", kind_name(kind), id_path(names))
}

/// `<kind>:<target item id>` for comments, owners and grants.
pub fn item_id_of(kind: CopyObjectKind, target_item: &str) -> String {
    format!("{}:{}", kind_name(kind), target_item)
}

// ---------------------------------------------------------------------------------------------
// Finalize statements (SequenceValue / AutoIncrement)
// ---------------------------------------------------------------------------------------------

/// Placeholder of the source value in finalize templates.
pub const SOURCE_VALUE: &str = "<source value>";
/// Placeholder of the target floor in finalize templates.
pub const TARGET_VALUE: &str = "<target value>";
/// Placeholder of PG's `is_called` in finalize templates.
pub const IS_CALLED: &str = "<is_called>";

/// The finalize statement of a `SequenceValue` / `AutoIncrement` item from the values read at
/// finalize (frozen). An empty string means "no statement" (the target is already at or past
/// the source value).
///
/// * PG, a sequence this run created: `SELECT pg_catalog.setval('"s"."q"', v, is_called)`.
/// * PG, the floor rule (an existing sequence, or the sequence of an append / truncate table):
///   `setval(seq, GREATEST(source, floor), true)` (LEAST for descending sequences), computed
///   here so the statement carries plain numbers.
/// * MySQL: ``ALTER TABLE `db`.`t` AUTO_INCREMENT = v`` only when `v` exceeds the target's.
/// * SQLite: `UPDATE sqlite_sequence SET seq = max(seq, v) WHERE name = 't'` when the target
///   has a row (`floor` is `Some`), else `INSERT INTO sqlite_sequence(name, seq) SELECT 't', v
///   WHERE NOT EXISTS (SELECT 1 FROM sqlite_sequence WHERE name = 't')`.
pub fn finalize_sql(item: &PlanItem, source: (i128, bool), floor: Option<i128>) -> String {
    let Some(template) = item.target_sql.first() else { return String::new() };
    let (value, is_called) = source;
    if template.starts_with("SELECT pg_catalog.setval(") {
        let Some(seq) = template.strip_prefix("SELECT pg_catalog.setval(").and_then(|r| r.split(", ").next()) else {
            return String::new();
        };
        if template.contains(TARGET_VALUE) {
            let descending = template.contains("LEAST(");
            let v = match floor {
                Some(f) if descending => value.min(f),
                Some(f) => value.max(f),
                None => value,
            };
            return format!("SELECT pg_catalog.setval({seq}, {v}, true)");
        }
        return format!("SELECT pg_catalog.setval({seq}, {value}, {is_called})");
    }
    if template.starts_with("ALTER TABLE ") {
        if floor.is_some_and(|f| f >= value) || value <= 0 {
            return String::new();
        }
        return template.replace(SOURCE_VALUE, &value.to_string());
    }
    if template.starts_with("UPDATE sqlite_sequence") || template.starts_with("INSERT INTO sqlite_sequence") {
        let name = crate::copy::translate::sqlite_name_literal(&item.target_name);
        if floor.is_some() {
            return format!("UPDATE sqlite_sequence SET seq = max(seq, {value}) WHERE name = {name}");
        }
        return format!("INSERT INTO sqlite_sequence(name, seq) SELECT {name}, {value} WHERE NOT EXISTS (SELECT 1 FROM sqlite_sequence WHERE name = {name})");
    }
    String::new()
}

/// SQLite string literal (`'` doubled).
pub fn sqlite_name_literal(name: &str) -> String {
    format!("'{}'", name.replace('\'', "''"))
}

/// SQLite / PG identifier quoting (`"` doubled).
pub fn dq(ident: &str) -> String {
    format!("\"{}\"", ident.replace('"', "\"\""))
}

/// MySQL identifier quoting (`` ` `` doubled).
pub fn bq(ident: &str) -> String {
    format!("`{}`", ident.replace('`', "``"))
}

/// `` `db`.`t` ``.
pub fn bq2(db: &str, name: &str) -> String {
    format!("{}.{}", bq(db), bq(name))
}

/// `"s"."t"`.
pub fn dq2(schema: &str, name: &str) -> String {
    format!("{}.{}", dq(schema), dq(name))
}

/// MySQL string literal for BulkDB's target sessions (backslash escapes on, `'` doubled).
pub fn my_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        match c {
            '\'' => out.push_str("''"),
            '\\' => out.push_str("\\\\"),
            '\0' => out.push_str("\\0"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\u{1a}' => out.push_str("\\Z"),
            c => out.push(c),
        }
    }
    out.push('\'');
    out
}

/// PG string literal: `'` doubled, `E'...'` with doubled backslashes when needed.
pub fn pg_str(s: &str) -> String {
    if s.contains('\\') {
        format!("E'{}'", s.replace('\\', "\\\\").replace('\'', "''"))
    } else {
        format!("'{}'", s.replace('\'', "''"))
    }
}

/// Classification of an item's `target_sql` for the executor: the DDL part (run in the item's
/// phase), the truncate statement (run right before the load, inside the load transaction on
/// PG) and the load template (never executed as is; the data path builds the real statement).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ItemSqlParts {
    pub ddl: Vec<String>,
    pub truncate: Option<String>,
    pub load_template: Option<String>,
}

/// Whether a statement is a data load template (`COPY … FROM STDIN …`, `LOAD DATA LOCAL INFILE
/// '<stream>' …`, `INSERT INTO … VALUES <rows>`).
pub fn is_load_template(sql: &str) -> bool {
    (sql.starts_with("COPY ") && sql.contains(" FROM STDIN"))
        || sql.starts_with("LOAD DATA LOCAL INFILE '<stream>'")
        || (sql.starts_with("INSERT INTO ") && sql.ends_with("<rows>"))
}

/// Whether a statement empties a table before its load (`TRUNCATE …`, SQLite `DELETE FROM "t"`).
pub fn is_truncate(sql: &str) -> bool {
    sql.starts_with("TRUNCATE ") || (sql.starts_with("DELETE FROM ") && !sql.contains(" WHERE "))
}

/// Split a table item's statements (see [`ItemSqlParts`]). Items without data have only DDL.
pub fn item_sql_parts(item: &PlanItem) -> ItemSqlParts {
    let mut out = ItemSqlParts::default();
    for s in &item.target_sql {
        if is_load_template(s) {
            out.load_template = Some(s.clone());
        } else if item.kind == CopyObjectKind::Table && is_truncate(s) {
            out.truncate = Some(s.clone());
        } else {
            out.ddl.push(s.clone());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::copy::types::*;

    fn item(kind: CopyObjectKind, target_name: &str, sql: &[&str]) -> PlanItem {
        PlanItem {
            id: item_id(kind, &[target_name]),
            kind,
            parent: None,
            schema: None,
            name: target_name.into(),
            target_schema: None,
            target_name: target_name.into(),
            action: PlanAction::Run,
            phase: CopyPhase::Finalize,
            order: 0,
            exists_in_target: false,
            target_has_rows: None,
            target_est_rows: None,
            has_data: false,
            est_rows: None,
            est_bytes: None,
            source_ddl: None,
            target_sql: sql.iter().map(|s| s.to_string()).collect(),
            warnings: vec![],
            depends_on: vec![],
        }
    }

    #[test]
    fn ids_quote_and_double() {
        assert_eq!(item_id(CopyObjectKind::Table, &["app", "orders"]), "table:\"app\".\"orders\"");
        assert_eq!(item_id(CopyObjectKind::ForeignKey, &["app", "orders", "orders_customer_fk"]), "foreign_key:\"app\".\"orders\".\"orders_customer_fk\"");
        assert_eq!(item_id(CopyObjectKind::Table, &["we\"ird"]), "table:\"we\"\"ird\"");
        assert_eq!(format!("{}(integer, text)", item_id(CopyObjectKind::Function, &["app", "f"])), "function:\"app\".\"f\"(integer, text)");
        assert_eq!(item_id_of(CopyObjectKind::Comment, "table:\"app\".\"orders\""), "comment:table:\"app\".\"orders\"");
        assert_eq!(item_id(CopyObjectKind::TruncateGroup, &["app", "customers"]), "truncate_group:\"app\".\"customers\"");
        assert_eq!(item_id(CopyObjectKind::Database, &["shop"]), "database:\"shop\"");
    }

    #[test]
    fn finalize_pg_created_and_floor() {
        let created = item(CopyObjectKind::SequenceValue, "q", &["SELECT pg_catalog.setval('\"s\".\"q\"', <source value>, <is_called>)"]);
        assert_eq!(finalize_sql(&created, (42, true), None), "SELECT pg_catalog.setval('\"s\".\"q\"', 42, true)");
        assert_eq!(finalize_sql(&created, (1, false), None), "SELECT pg_catalog.setval('\"s\".\"q\"', 1, false)");
        let floor = item(CopyObjectKind::SequenceValue, "q", &["SELECT pg_catalog.setval('\"s\".\"q\"', GREATEST(<source value>, <target value>), true)"]);
        // Never moves backwards: the target is ahead.
        assert_eq!(finalize_sql(&floor, (10, true), Some(500)), "SELECT pg_catalog.setval('\"s\".\"q\"', 500, true)");
        assert_eq!(finalize_sql(&floor, (900, true), Some(500)), "SELECT pg_catalog.setval('\"s\".\"q\"', 900, true)");
        assert_eq!(finalize_sql(&floor, (900, true), None), "SELECT pg_catalog.setval('\"s\".\"q\"', 900, true)");
        let desc = item(CopyObjectKind::SequenceValue, "q", &["SELECT pg_catalog.setval('\"s\".\"q\"', LEAST(<source value>, <target value>), true)"]);
        assert_eq!(finalize_sql(&desc, (-10, true), Some(-50)), "SELECT pg_catalog.setval('\"s\".\"q\"', -50, true)");
    }

    #[test]
    fn finalize_mysql_only_raises() {
        let it = item(CopyObjectKind::AutoIncrement, "t", &["ALTER TABLE `db`.`t` AUTO_INCREMENT = <source value>"]);
        assert_eq!(finalize_sql(&it, (100, true), Some(5)), "ALTER TABLE `db`.`t` AUTO_INCREMENT = 100");
        assert_eq!(finalize_sql(&it, (100, true), Some(100)), "");
        assert_eq!(finalize_sql(&it, (100, true), Some(101)), "");
        assert_eq!(finalize_sql(&it, (100, true), None), "ALTER TABLE `db`.`t` AUTO_INCREMENT = 100");
    }

    #[test]
    fn finalize_sqlite_update_or_insert() {
        let it = item(CopyObjectKind::AutoIncrement, "it's", &["UPDATE sqlite_sequence SET seq = max(seq, <source value>) WHERE name = 'it''s'"]);
        assert_eq!(finalize_sql(&it, (7, true), Some(3)), "UPDATE sqlite_sequence SET seq = max(seq, 7) WHERE name = 'it''s'");
        assert_eq!(
            finalize_sql(&it, (7, true), None),
            "INSERT INTO sqlite_sequence(name, seq) SELECT 'it''s', 7 WHERE NOT EXISTS (SELECT 1 FROM sqlite_sequence WHERE name = 'it''s')"
        );
        assert_eq!(finalize_sql(&item(CopyObjectKind::AutoIncrement, "t", &[]), (7, true), None), "");
    }

    #[test]
    fn item_parts_split_ddl_truncate_and_template() {
        let mut it =
            item(CopyObjectKind::Table, "t", &["SET search_path = \"s\", pg_catalog", "TRUNCATE ONLY \"s\".\"t\"", "COPY \"s\".\"t\" (\"a\") FROM STDIN (FORMAT csv)"]);
        it.kind = CopyObjectKind::Table;
        let p = item_sql_parts(&it);
        assert_eq!(p.ddl, vec!["SET search_path = \"s\", pg_catalog".to_string()]);
        assert_eq!(p.truncate.as_deref(), Some("TRUNCATE ONLY \"s\".\"t\""));
        assert!(p.load_template.unwrap().starts_with("COPY"));
        assert!(is_load_template("LOAD DATA LOCAL INFILE '<stream>' INTO TABLE `d`.`t`"));
        assert!(is_load_template("INSERT INTO \"t\" (\"a\") VALUES <rows>"));
        assert!(is_truncate("DELETE FROM \"t\""));
        assert_eq!(my_str("it's \\ ok"), "'it''s \\\\ ok'");
        assert_eq!(pg_str("a'b"), "'a''b'");
        assert_eq!(pg_str("a\\b"), "E'a\\\\b'");
    }
}
