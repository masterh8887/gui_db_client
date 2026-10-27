//! MySQL / MariaDB catalog over `information_schema`.
//!
//! MySQL has no schema level below a database: `schemas(db)` returns the database itself and
//! `tables(db, schema)` reads `information_schema.tables WHERE table_schema = schema` (falling
//! back to `db` when `schema` is empty). Every listing is a single query (no N+1); `indexes`
//! adds two optional round trips (`mysql.innodb_index_stats` for sizes and `SHOW CREATE TABLE`
//! for definitions) whose failures are ignored, because both need privileges a read-only
//! extraction account often lacks. Identifiers are quoted with backticks. No table data is
//! ever read here; `exact_count` is the only statement that touches rows and it is cancellable
//! with `KILL QUERY` from a side connection.

use std::collections::{BTreeMap, HashMap};

use async_trait::async_trait;
use mysql_async::{Params, Row, Value};
use tokio_util::sync::CancellationToken;

use super::Catalog;
use crate::error::{CoreError, Result};
use crate::source::mysql::{row_str, row_u64, MySqlDatabase};
use crate::types::{ColumnInfo, DatabaseInfo, IndexInfo, SchemaInfo, TableInfo, TableKind, TableRef};

const DATABASES_SQL: &str = "SELECT s.schema_name, s.default_character_set_name, s.default_collation_name, t.bytes, DATABASE() \
     FROM information_schema.schemata s \
     LEFT JOIN (SELECT table_schema, SUM(data_length + index_length) AS bytes \
                FROM information_schema.tables GROUP BY table_schema) t \
       ON t.table_schema = s.schema_name \
     ORDER BY s.schema_name";

const SCHEMA_SQL: &str = "SELECT (SELECT COUNT(*) FROM information_schema.schemata WHERE schema_name = ?), \
            (SELECT COUNT(*) FROM information_schema.tables WHERE table_schema = ?)";

const TABLES_SQL: &str = "SELECT table_name, table_type, table_rows, data_length, index_length, table_comment \
     FROM information_schema.tables WHERE table_schema = ? ORDER BY table_name";

const COLUMNS_SQL: &str = "SELECT ordinal_position, column_name, column_type, is_nullable, column_default, column_key, column_comment, extra \
     FROM information_schema.columns WHERE table_schema = ? AND table_name = ? ORDER BY ordinal_position";

const FOREIGN_KEYS_SQL: &str = "SELECT column_name, referenced_table_schema, referenced_table_name, referenced_column_name \
     FROM information_schema.key_column_usage \
     WHERE table_schema = ? AND table_name = ? AND referenced_table_name IS NOT NULL \
     ORDER BY constraint_name, ordinal_position";

const INDEXES_SQL: &str = "SELECT index_name, non_unique, column_name, index_type, sub_part \
     FROM information_schema.statistics WHERE table_schema = ? AND table_name = ? \
     ORDER BY (index_name = 'PRIMARY') DESC, index_name, seq_in_index";

/// InnoDB persistent statistics: `size` is the index size in pages.
const INDEX_SIZES_SQL: &str = "SELECT index_name, stat_value * @@innodb_page_size \
     FROM mysql.innodb_index_stats WHERE database_name = ? AND table_name = ? AND stat_name = 'size'";

/// Quote an identifier with backticks (a backtick inside is doubled).
pub fn quote_ident(ident: &str) -> String {
    format!("`{}`", ident.replace('`', "``"))
}

/// `information_schema.tables.table_type` -> [`TableKind`].
pub fn table_kind(table_type: &str) -> TableKind {
    match table_type.trim().to_ascii_uppercase().as_str() {
        "BASE TABLE" | "TABLE" => TableKind::Table,
        "VIEW" | "SYSTEM VIEW" => TableKind::View,
        _ => TableKind::Other,
    }
}

/// Database a table reference points at: its `schema`, else its `database`, else the
/// connection's current database. MySQL needs one; an unqualified name on a connection without
/// a selected database is an error rather than a guess.
fn schema_of(db: &MySqlDatabase, table: &TableRef) -> Result<String> {
    table
        .schema
        .as_deref()
        .filter(|s| !s.is_empty())
        .or_else(|| table.database.as_deref().filter(|s| !s.is_empty()))
        .or_else(|| db.current_database())
        .map(str::to_string)
        .ok_or_else(|| CoreError::Invalid(format!("table '{}' is not qualified with a database and the connection has none selected", table.name)))
}

fn positional<const N: usize>(values: [Value; N]) -> Params {
    Params::Positional(values.to_vec())
}

fn non_empty(s: Option<String>) -> Option<String> {
    s.filter(|s| !s.trim().is_empty())
}

/// `SHOW CREATE TABLE` output -> index name -> its `KEY ...` line (without the trailing comma).
/// `PRIMARY KEY (...)` is stored under `PRIMARY`; foreign keys and columns are skipped.
pub fn index_definitions(create_sql: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for raw in create_sql.lines() {
        let line = raw.trim().trim_end_matches(',').trim();
        let upper = line.to_ascii_uppercase();
        if upper.starts_with("PRIMARY KEY") {
            out.insert("PRIMARY".to_string(), line.to_string());
            continue;
        }
        let is_key =
            ["UNIQUE KEY ", "UNIQUE INDEX ", "FULLTEXT KEY ", "FULLTEXT INDEX ", "SPATIAL KEY ", "SPATIAL INDEX ", "KEY ", "INDEX "].iter().any(|p| upper.starts_with(p));
        if !is_key {
            continue;
        }
        if let Some(name) = first_backticked(line) {
            out.insert(name, line.to_string());
        }
    }
    out
}

/// The first backtick-quoted identifier in `line` (doubled backticks unescaped).
fn first_backticked(line: &str) -> Option<String> {
    let start = line.find('`')? + 1;
    let rest = &line[start..];
    let mut name = String::new();
    let mut chars = rest.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '`' {
            if chars.peek() == Some(&'`') {
                chars.next();
                name.push('`');
            } else {
                return Some(name);
            }
        } else {
            name.push(c);
        }
    }
    None
}

/// One `information_schema.columns` row as text (the `COLUMNS_SQL` column order).
#[derive(Debug, Default)]
struct RawColumn {
    position: Option<u64>,
    name: Option<String>,
    column_type: Option<String>,
    is_nullable: Option<String>,
    default: Option<String>,
    key: Option<String>,
    comment: Option<String>,
    extra: Option<String>,
}

impl RawColumn {
    fn from_row(r: &Row) -> RawColumn {
        RawColumn {
            position: row_u64(r, 0),
            name: row_str(r, 1),
            column_type: row_str(r, 2),
            is_nullable: row_str(r, 3),
            default: row_str(r, 4),
            key: row_str(r, 5),
            comment: row_str(r, 6),
            extra: row_str(r, 7),
        }
    }

    /// Fold the row into a [`ColumnInfo`]. `extra` (`auto_increment`, `DEFAULT_GENERATED`,
    /// `on update CURRENT_TIMESTAMP`) is folded into `default` so the UI shows it.
    fn into_info(self, foreign_keys: &HashMap<String, String>) -> ColumnInfo {
        let name = self.name.unwrap_or_default();
        let key = self.key.unwrap_or_default().to_ascii_uppercase();
        let extra = self.extra.unwrap_or_default();
        let extra_upper = extra.to_ascii_uppercase();
        let mut default = self.default;
        if extra_upper.contains("AUTO_INCREMENT") {
            default = Some(match default {
                Some(d) if !d.is_empty() => format!("{d} AUTO_INCREMENT"),
                _ => "AUTO_INCREMENT".to_string(),
            });
        } else if let Some(d) = default.as_ref().filter(|_| extra_upper.contains("DEFAULT_GENERATED")) {
            // MySQL 8 expression defaults: show them as expressions, not literals.
            default = Some(format!("({d})"));
        }
        if let Some(update) = extra_upper.find("ON UPDATE") {
            let clause = extra[update..].trim();
            default = Some(match default {
                Some(d) if !d.is_empty() => format!("{d} {clause}"),
                _ => clause.to_string(),
            });
        }
        ColumnInfo {
            position: self.position.unwrap_or(0) as u32,
            data_type: self.column_type.unwrap_or_default(),
            nullable: self.is_nullable.map(|v| v.eq_ignore_ascii_case("YES")).unwrap_or(true),
            default,
            primary_key: key == "PRI",
            foreign_key: foreign_keys.get(&name).cloned(),
            unique: key == "PRI" || key == "UNI",
            comment: non_empty(self.comment),
            name,
        }
    }
}

#[async_trait]
impl Catalog for MySqlDatabase {
    async fn databases(&self) -> Result<Vec<DatabaseInfo>> {
        let rows = self.query_map(DATABASES_SQL, |r: Row| (row_str(&r, 0).unwrap_or_default(), row_str(&r, 1), row_str(&r, 2), row_u64(&r, 3), row_str(&r, 4))).await?;
        Ok(rows
            .into_iter()
            .map(|(name, charset, collation, bytes, current)| DatabaseInfo {
                is_current: current.as_deref() == Some(name.as_str()),
                name,
                size_bytes: bytes,
                encoding: non_empty(charset),
                collation: non_empty(collation),
                accessible: true,
            })
            .collect())
    }

    async fn schemas(&self, db: &str) -> Result<Vec<SchemaInfo>> {
        let row = self.exec_first(SCHEMA_SQL, positional([Value::from(db), Value::from(db)])).await?;
        let (present, tables) = match row {
            Some(r) => (row_u64(&r, 0).unwrap_or(0), row_u64(&r, 1)),
            None => (0, None),
        };
        if present == 0 {
            return Err(CoreError::NotFound(format!("database '{db}' does not exist or is not visible to this account")));
        }
        Ok(vec![SchemaInfo { name: db.to_string(), owner: None, table_count: tables }])
    }

    async fn tables(&self, db: &str, schema: &str) -> Result<Vec<TableInfo>> {
        let schema = if schema.is_empty() { db } else { schema };
        let owner = schema.to_string();
        let rows = self
            .exec_map(TABLES_SQL, positional([Value::from(schema)]), move |r: Row| {
                let kind = table_kind(&row_str(&r, 1).unwrap_or_default());
                let table_bytes = row_u64(&r, 3);
                let index_bytes = row_u64(&r, 4);
                let total_bytes = match (table_bytes, index_bytes) {
                    (None, None) => None,
                    (a, b) => Some(a.unwrap_or(0) + b.unwrap_or(0)),
                };
                TableInfo {
                    schema: owner.clone(),
                    name: row_str(&r, 0).unwrap_or_default(),
                    kind,
                    estimated_rows: if kind == TableKind::View { None } else { row_u64(&r, 2) },
                    table_bytes,
                    index_bytes,
                    total_bytes,
                    comment: non_empty(row_str(&r, 5)),
                }
            })
            .await?;
        Ok(rows)
    }

    async fn columns(&self, table: &TableRef) -> Result<Vec<ColumnInfo>> {
        let schema = schema_of(self, table)?;
        let fk_rows = match self
            .exec_map(FOREIGN_KEYS_SQL, positional([Value::from(schema.as_str()), Value::from(table.name.as_str())]), |r: Row| {
                (row_str(&r, 0).unwrap_or_default(), row_str(&r, 1), row_str(&r, 2).unwrap_or_default(), row_str(&r, 3).unwrap_or_default())
            })
            .await
        {
            Ok(rows) => rows,
            Err(e) => {
                tracing::debug!("mysql: foreign keys of {schema}.{} not readable: {e}", table.name);
                Vec::new()
            }
        };
        let mut foreign_keys: HashMap<String, String> = HashMap::new();
        for (column, ref_schema, ref_table, ref_column) in fk_rows {
            let target = match ref_schema.filter(|s| !s.is_empty() && *s != schema) {
                Some(s) => format!("{s}.{ref_table}({ref_column})"),
                None => format!("{ref_table}({ref_column})"),
            };
            foreign_keys
                .entry(column)
                .and_modify(|existing| {
                    if !existing.contains(&target) {
                        existing.push_str(", ");
                        existing.push_str(&target);
                    }
                })
                .or_insert(target);
        }

        let columns = self
            .exec_map(COLUMNS_SQL, positional([Value::from(schema.as_str()), Value::from(table.name.as_str())]), move |r: Row| {
                RawColumn::from_row(&r).into_info(&foreign_keys)
            })
            .await?;
        if columns.is_empty() {
            return Err(CoreError::NotFound(format!("table {schema}.{} does not exist or is not visible to this account", table.name)));
        }
        Ok(columns)
    }

    async fn indexes(&self, table: &TableRef) -> Result<Vec<IndexInfo>> {
        let schema = schema_of(self, table)?;
        let params = || positional([Value::from(schema.as_str()), Value::from(table.name.as_str())]);
        let rows = self
            .exec_map(INDEXES_SQL, params(), |r: Row| (row_str(&r, 0).unwrap_or_default(), row_u64(&r, 1).unwrap_or(1), row_str(&r, 2), row_str(&r, 3), row_u64(&r, 4)))
            .await?;

        let mut indexes: Vec<IndexInfo> = Vec::new();
        let mut slot: HashMap<String, usize> = HashMap::new();
        for (name, non_unique, column, index_type, sub_part) in rows {
            let column = match (column, sub_part) {
                (Some(c), Some(n)) => format!("{c}({n})"),
                (Some(c), None) => c,
                // Functional index parts have no column name.
                (None, _) => "(expression)".to_string(),
            };
            let i = match slot.get(&name) {
                Some(i) => *i,
                None => {
                    indexes.push(IndexInfo {
                        primary: name == "PRIMARY",
                        unique: non_unique == 0,
                        kind: non_empty(index_type).map(|t| t.to_ascii_lowercase()),
                        name: name.clone(),
                        ..Default::default()
                    });
                    slot.insert(name, indexes.len() - 1);
                    indexes.len() - 1
                }
            };
            indexes[i].columns.push(column);
        }
        if indexes.is_empty() {
            return Ok(indexes);
        }

        // Sizes from InnoDB persistent statistics; needs SELECT on `mysql.*`, so failures are
        // silently ignored.
        match self.exec_map(INDEX_SIZES_SQL, params(), |r: Row| (row_str(&r, 0).unwrap_or_default(), row_u64(&r, 1))).await {
            Ok(sizes) => {
                for (name, bytes) in sizes {
                    if let Some(i) = slot.get(&name) {
                        indexes[*i].size_bytes = bytes;
                    }
                }
            }
            Err(e) => tracing::debug!("mysql: innodb_index_stats of {schema}.{} not readable: {e}", table.name),
        }

        // Definitions from SHOW CREATE TABLE (one round trip; fails for views and without
        // SHOW VIEW / SELECT privileges, which is fine).
        match self.query_first(&format!("SHOW CREATE TABLE {}.{}", quote_ident(&schema), quote_ident(&table.name))).await {
            Ok(Some(row)) => {
                let defs = index_definitions(&row_str(&row, 1).unwrap_or_default());
                for (name, def) in defs {
                    if let Some(i) = slot.get(&name) {
                        indexes[*i].definition = Some(def);
                    }
                }
            }
            Ok(None) => {}
            Err(e) => tracing::debug!("mysql: SHOW CREATE TABLE {schema}.{} failed: {e}", table.name),
        }
        Ok(indexes)
    }

    async fn ddl(&self, table: &TableRef) -> Result<String> {
        let schema = schema_of(self, table)?;
        let row = self.query_first(&format!("SHOW CREATE TABLE {}.{}", quote_ident(&schema), quote_ident(&table.name))).await?;
        row.and_then(|r| row_str(&r, 1))
            .filter(|s| !s.is_empty())
            .ok_or_else(|| CoreError::NotFound(format!("table {schema}.{} does not exist or is not visible to this account", table.name)))
    }

    async fn exact_count(&self, table: &TableRef, cancel: CancellationToken) -> Result<u64> {
        let sql = format!("SELECT COUNT(*) FROM {}", self.quote_table(table));
        let row = self.query_first_cancellable(&sql, cancel).await?;
        row.and_then(|r| row_u64(&r, 0)).ok_or_else(|| CoreError::Internal(format!("COUNT(*) of {} returned no row", table.qualified())))
    }

    fn quote_ident(&self, ident: &str) -> String {
        quote_ident(ident)
    }

    fn quote_table(&self, table: &TableRef) -> String {
        let schema = table.schema.as_deref().filter(|s| !s.is_empty()).or_else(|| table.database.as_deref().filter(|s| !s.is_empty()));
        match schema {
            Some(s) => format!("{}.{}", quote_ident(s), quote_ident(&table.name)),
            None => quote_ident(&table.name),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backtick_quoting() {
        assert_eq!(quote_ident("orders"), "`orders`");
        assert_eq!(quote_ident("we`ird"), "`we``ird`");
        assert_eq!(quote_ident("with space"), "`with space`");
    }

    #[test]
    fn table_kinds() {
        assert_eq!(table_kind("BASE TABLE"), TableKind::Table);
        assert_eq!(table_kind("base table"), TableKind::Table);
        assert_eq!(table_kind("VIEW"), TableKind::View);
        assert_eq!(table_kind("SYSTEM VIEW"), TableKind::View);
        assert_eq!(table_kind("SEQUENCE"), TableKind::Other);
        assert_eq!(table_kind(""), TableKind::Other);
    }

    #[test]
    fn index_definitions_from_show_create_table() {
        let ddl = "CREATE TABLE `t` (\n\
                   \x20 `id` int NOT NULL AUTO_INCREMENT,\n\
                   \x20 `key` varchar(20) DEFAULT NULL,\n\
                   \x20 `body` text,\n\
                   \x20 PRIMARY KEY (`id`),\n\
                   \x20 UNIQUE KEY `uq_key` (`key`),\n\
                   \x20 KEY `ix_multi` (`key`,`id`) USING BTREE,\n\
                   \x20 FULLTEXT KEY `ft_body` (`body`),\n\
                   \x20 SPATIAL KEY `sp` (`geom`),\n\
                   \x20 KEY `we``ird` (`id`),\n\
                   \x20 CONSTRAINT `fk_parent` FOREIGN KEY (`id`) REFERENCES `p` (`id`)\n\
                   ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4";
        let defs = index_definitions(ddl);
        assert_eq!(defs.get("PRIMARY").map(String::as_str), Some("PRIMARY KEY (`id`)"));
        assert_eq!(defs.get("uq_key").map(String::as_str), Some("UNIQUE KEY `uq_key` (`key`)"));
        assert_eq!(defs.get("ix_multi").map(String::as_str), Some("KEY `ix_multi` (`key`,`id`) USING BTREE"));
        assert_eq!(defs.get("ft_body").map(String::as_str), Some("FULLTEXT KEY `ft_body` (`body`)"));
        assert_eq!(defs.get("sp").map(String::as_str), Some("SPATIAL KEY `sp` (`geom`)"));
        assert!(defs.contains_key("we`ird"), "doubled backticks are unescaped");
        assert!(!defs.contains_key("fk_parent"), "foreign keys are not indexes");
        assert!(!defs.contains_key("key"), "a column named `key` is not an index");
        assert_eq!(defs.len(), 6);
        assert!(index_definitions("CREATE VIEW v AS SELECT 1").is_empty());
    }

    #[test]
    fn first_backticked_identifier() {
        assert_eq!(first_backticked("KEY `a` (`b`)").as_deref(), Some("a"));
        assert_eq!(first_backticked("KEY `a``b` (`c`)").as_deref(), Some("a`b"));
        assert_eq!(first_backticked("PRIMARY KEY (id)"), None);
        assert_eq!(first_backticked("KEY `unterminated"), None);
    }

    #[test]
    fn column_rows_are_decoded() {
        let fks: HashMap<String, String> = HashMap::from([("parent_id".to_string(), "shop.parents(id)".to_string())]);
        let s = |v: &str| Some(v.to_string());
        let raw = |position: u64, name: &str, ty: &str, nullable: &str, default: Option<String>, key: &str, comment: &str, extra: &str| RawColumn {
            position: Some(position),
            name: s(name),
            column_type: s(ty),
            is_nullable: s(nullable),
            default,
            key: s(key),
            comment: s(comment),
            extra: s(extra),
        };

        let id = raw(1, "id", "int unsigned", "NO", None, "PRI", "", "auto_increment").into_info(&fks);
        assert_eq!(id.position, 1);
        assert_eq!(id.name, "id");
        assert_eq!(id.data_type, "int unsigned");
        assert!(!id.nullable);
        assert!(id.primary_key && id.unique);
        assert_eq!(id.default.as_deref(), Some("AUTO_INCREMENT"));
        assert_eq!(id.foreign_key, None);
        assert_eq!(id.comment, None);

        let parent = raw(2, "parent_id", "int", "YES", s("0"), "MUL", "owner", "").into_info(&fks);
        assert!(parent.nullable);
        assert!(!parent.primary_key && !parent.unique);
        assert_eq!(parent.default.as_deref(), Some("0"));
        assert_eq!(parent.foreign_key.as_deref(), Some("shop.parents(id)"));
        assert_eq!(parent.comment.as_deref(), Some("owner"));

        let code = raw(3, "code", "varchar(10)", "NO", None, "UNI", "", "").into_info(&fks);
        assert!(code.unique && !code.primary_key);

        let ts = raw(4, "updated", "timestamp", "NO", s("CURRENT_TIMESTAMP"), "", "", "DEFAULT_GENERATED on update CURRENT_TIMESTAMP").into_info(&fks);
        assert_eq!(ts.default.as_deref(), Some("(CURRENT_TIMESTAMP) on update CURRENT_TIMESTAMP"));

        let missing = RawColumn::default().into_info(&fks);
        assert_eq!(missing.position, 0);
        assert!(missing.nullable);
        assert_eq!(missing.default, None);
    }
}
