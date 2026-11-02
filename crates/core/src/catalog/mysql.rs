//! MySQL / MariaDB catalog, version aware, written once over [`MysqlRawQuery`] for both MySQL
//! clients (design doc section 4, "catalog"):
//!
//! * 5.0+ (`information_schema`): every listing is a single query (no N+1); `indexes` adds two
//!   optional round trips (`mysql.innodb_index_stats` for sizes on 5.6+ and
//!   `SHOW CREATE TABLE` for definitions) whose failures are ignored, because both need
//!   privileges a read-only extraction account often lacks.
//! * 4.0 / 4.1 (no `information_schema`): the `SHOW` family ([`generic::show`]) —
//!   `SHOW DATABASES`, `SHOW TABLE STATUS`, `SHOW FULL COLUMNS`, `SHOW INDEX`,
//!   `SHOW CREATE TABLE` — with columns looked up by name, because their layout differs between
//!   versions (4.0 `Type` vs 4.1.2+ `Engine`, no `Comment` column before 4.1).
//!
//! MySQL has no schema level below a database: `schemas(db)` returns the database itself and
//! `tables(db, schema)` lists `schema` (falling back to `db` when `schema` is empty).
//! Parameters are inlined with `mysql_common::quote_str` (4.0 has no prepared statements) and
//! identifiers quoted with backticks. Text is decoded per the connection's text plan, so names
//! and comments stored as EUC-KR read correctly. No table data is ever read here;
//! `exact_count` is the only statement that touches rows and it is cancellable (`KILL QUERY`,
//! or `KILL` + reconnect on 4.x).

use std::collections::{BTreeMap, HashMap};

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use super::Catalog;
use crate::error::Result;
use crate::source::mysql::MySqlDatabase;
use crate::types::{ColumnInfo, DatabaseInfo, IndexInfo, SchemaInfo, TableInfo, TableKind, TableRef};

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
    backticked(line).next()
}

/// Every backtick-quoted identifier in `s`, in order (doubled backticks unescaped; an
/// unterminated one is dropped).
fn backticked(s: &str) -> impl Iterator<Item = String> + '_ {
    let mut chars = s.chars().peekable();
    std::iter::from_fn(move || {
        // Skip to the opening backtick.
        loop {
            match chars.next()? {
                '`' => break,
                _ => continue,
            }
        }
        let mut name = String::new();
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
    })
}

/// InnoDB foreign keys from `SHOW CREATE TABLE` (4.x has no `key_column_usage`):
/// `CONSTRAINT ... FOREIGN KEY (`a`, `b`) REFERENCES [`db`.]`t` (`x`, `y`)` -> column ->
/// `t(x)` (`db.t(x)` when `db` is not `schema`); several targets are joined with `, `.
pub fn foreign_keys_from_ddl(create_sql: &str, schema: &str) -> HashMap<String, String> {
    let mut out: HashMap<String, String> = HashMap::new();
    for raw in create_sql.lines() {
        let line = raw.trim();
        let upper = line.to_ascii_uppercase();
        let (Some(fk), Some(refs)) = (upper.find("FOREIGN KEY"), upper.find("REFERENCES")) else { continue };
        if refs < fk {
            continue;
        }
        let local: Vec<String> = backticked(&line[fk..refs]).collect();
        let rest = &line[refs..];
        let paren = rest.find('(').unwrap_or(rest.len());
        let target: Vec<String> = backticked(&rest[..paren]).collect();
        let remote: Vec<String> = backticked(rest[paren..].split(')').next().unwrap_or("")).collect();
        let table = match target.as_slice() {
            [t] => t.clone(),
            [db, t] if db == schema => t.clone(),
            [db, t] => format!("{db}.{t}"),
            _ => continue,
        };
        for (col, ref_col) in local.iter().zip(remote.iter()) {
            let desc = format!("{table}({ref_col})");
            out.entry(col.clone())
                .and_modify(|existing| {
                    if !existing.contains(&desc) {
                        existing.push_str(", ");
                        existing.push_str(&desc);
                    }
                })
                .or_insert(desc);
        }
    }
    out
}

/// `SHOW TABLE STATUS` comment without what InnoDB on 4.x / 5.0 appends to it
/// (`InnoDB free: 4096 kB` and `` (`a`) REFER `db/t`(`b`) `` foreign key notes).
pub fn clean_table_comment(comment: &str) -> Option<String> {
    let kept: Vec<&str> =
        comment.split(';').map(str::trim).filter(|p| !p.is_empty() && !p.starts_with("InnoDB free:") && !(p.starts_with("(`") && p.contains(") REFER `"))).collect();
    non_empty(Some(kept.join("; ")))
}

/// One column description as text (`information_schema.columns` or `SHOW FULL COLUMNS`).
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
        generic::databases(self).await
    }

    async fn schemas(&self, db: &str) -> Result<Vec<SchemaInfo>> {
        generic::schemas(self, db).await
    }

    async fn tables(&self, db: &str, schema: &str) -> Result<Vec<TableInfo>> {
        generic::tables(self, db, schema).await
    }

    async fn columns(&self, table: &TableRef) -> Result<Vec<ColumnInfo>> {
        generic::columns(self, table).await
    }

    async fn indexes(&self, table: &TableRef) -> Result<Vec<IndexInfo>> {
        generic::indexes(self, table).await
    }

    async fn ddl(&self, table: &TableRef) -> Result<String> {
        generic::ddl(self, table).await
    }

    async fn exact_count(&self, table: &TableRef, cancel: CancellationToken) -> Result<u64> {
        generic::exact_count(self, table, cancel).await
    }

    fn quote_ident(&self, ident: &str) -> String {
        quote_ident(ident)
    }

    fn quote_table(&self, table: &TableRef) -> String {
        generic::quote_table(table)
    }
}

/// Version-aware catalog over [`MysqlRawQuery`], shared by both MySQL clients (owner:
/// modern; design doc section 4, "catalog").
///
/// | operation   | 5.0+                                   | 4.0 / 4.1                                        |
/// |-------------|----------------------------------------|--------------------------------------------------|
/// | databases   | `information_schema.schemata` + sizes  | `SHOW DATABASES` (4.0: the server charset)       |
/// | schemas     | the database itself                    | same, existence via `SHOW DATABASES LIKE`        |
/// | tables      | `information_schema.tables`            | `SHOW TABLE STATUS FROM db` (`Type`/`Engine`, `Rows`, `Data_length`, `Index_length`, `Comment` minus "InnoDB free: ...") |
/// | columns     | `information_schema.columns` + FKs     | `SHOW FULL COLUMNS FROM t FROM db` (4.0: no `Comment`) + FKs from `SHOW CREATE TABLE` |
/// | indexes     | `information_schema.statistics` (+ sizes, DDL) | `SHOW INDEX FROM t FROM db` (+ DDL)      |
/// | ddl         | `SHOW CREATE TABLE`                    | `SHOW CREATE TABLE`                              |
/// | exact_count | `SELECT COUNT(*)`, cancellable         | same (`KILL <id>` ends the session on 4.x)       |
///
/// The top-level functions pick the column by `caps.information_schema`; [`show`] holds the
/// `SHOW`-based implementations, public so they can be exercised on any server.
pub mod generic {
    use std::collections::HashMap;

    use tokio_util::sync::CancellationToken;

    use super::{clean_table_comment, foreign_keys_from_ddl, index_definitions, non_empty, quote_ident, table_kind, RawColumn};
    use crate::error::{CoreError, Result};
    use crate::source::mysql::Flavor;
    use crate::source::mysql_common::{quote_str, server_error_code, MetaRows, MysqlRawQuery, ServerFacts, ER_BAD_DB_ERROR, ER_NO_SUCH_TABLE};
    use crate::types::{ColumnInfo, DatabaseInfo, IndexInfo, SchemaInfo, TableInfo, TableKind, TableRef};

    /// Which family of statements the catalog uses on a server.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum CatalogMode {
        /// `information_schema` (5.0+).
        InformationSchema,
        /// The `SHOW` family (4.0 / 4.1).
        Show,
    }

    impl CatalogMode {
        pub fn for_facts(facts: &ServerFacts) -> CatalogMode {
            if facts.caps.information_schema {
                CatalogMode::InformationSchema
            } else {
                CatalogMode::Show
            }
        }
    }

    fn mode(q: &dyn MysqlRawQuery) -> CatalogMode {
        CatalogMode::for_facts(q.facts())
    }

    /// Database a table reference points at: its `schema`, else its `database`, else the
    /// connection's current database. MySQL needs one; an unqualified name on a connection
    /// without a selected database is an error rather than a guess.
    pub fn schema_of(facts: &ServerFacts, table: &TableRef) -> Result<String> {
        table
            .schema
            .as_deref()
            .filter(|s| !s.is_empty())
            .or_else(|| table.database.as_deref().filter(|s| !s.is_empty()))
            .or(facts.database.as_deref())
            .map(str::to_string)
            .ok_or_else(|| CoreError::Invalid(format!("table '{}' is not qualified with a database and the connection has none selected", table.name)))
    }

    fn lit(q: &dyn MysqlRawQuery, s: &str) -> String {
        quote_str(s, q.facts().lex)
    }

    /// `LIKE` pattern matching `s` literally (`\`, `%`, `_` escaped with the default `\`).
    fn like_literal(s: &str) -> String {
        s.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_")
    }

    fn missing_table(schema: &str, table: &str) -> CoreError {
        CoreError::NotFound(format!("table {schema}.{table} does not exist or is not visible to this account"))
    }

    /// Text of every row's cell in column `col` (by name).
    fn texts(rows: &MetaRows, f: &ServerFacts, col: usize) -> Vec<Option<String>> {
        (0..rows.len()).map(|r| rows.text(f, r, col)).collect()
    }

    /// `SHOW CREATE TABLE` text (second column); `None` when the server returned no row.
    pub async fn create_table_sql(q: &dyn MysqlRawQuery, schema: &str, table: &str) -> Result<Option<String>> {
        let rows = q.meta_query(&format!("SHOW CREATE TABLE {}.{}", quote_ident(schema), quote_ident(table))).await?;
        Ok(rows.text(q.facts(), 0, 1).filter(|s| !s.is_empty()))
    }

    pub async fn databases(q: &dyn MysqlRawQuery) -> Result<Vec<DatabaseInfo>> {
        match mode(q) {
            CatalogMode::Show => show::databases(q).await,
            CatalogMode::InformationSchema => {
                let f = q.facts();
                let rows = q
                    .meta_query(
                        "SELECT s.schema_name, s.default_character_set_name, s.default_collation_name, t.bytes, DATABASE() \
                         FROM information_schema.schemata s \
                         LEFT JOIN (SELECT table_schema, SUM(data_length + index_length) AS bytes \
                                    FROM information_schema.tables GROUP BY table_schema) t \
                           ON t.table_schema = s.schema_name \
                         ORDER BY s.schema_name",
                    )
                    .await?;
                Ok((0..rows.len())
                    .map(|r| {
                        let name = rows.text(f, r, 0).unwrap_or_default();
                        DatabaseInfo {
                            is_current: rows.text(f, r, 4).as_deref() == Some(name.as_str()),
                            name,
                            size_bytes: rows.u64(r, 3),
                            encoding: non_empty(rows.text(f, r, 1)),
                            collation: non_empty(rows.text(f, r, 2)),
                            accessible: true,
                        }
                    })
                    .collect())
            }
        }
    }

    pub async fn schemas(q: &dyn MysqlRawQuery, db: &str) -> Result<Vec<SchemaInfo>> {
        if mode(q) == CatalogMode::Show {
            return show::schemas(q, db).await;
        }
        let d = lit(q, db);
        let rows = q
            .meta_query(&format!(
                "SELECT (SELECT COUNT(*) FROM information_schema.schemata WHERE schema_name = {d}), \
                        (SELECT COUNT(*) FROM information_schema.tables WHERE table_schema = {d})"
            ))
            .await?;
        if rows.u64(0, 0).unwrap_or(0) == 0 {
            return Err(CoreError::NotFound(format!("database '{db}' does not exist or is not visible to this account")));
        }
        Ok(vec![SchemaInfo { name: db.to_string(), owner: None, table_count: rows.u64(0, 1) }])
    }

    pub async fn tables(q: &dyn MysqlRawQuery, db: &str, schema: &str) -> Result<Vec<TableInfo>> {
        let schema = if schema.is_empty() { db } else { schema };
        if mode(q) == CatalogMode::Show {
            return show::tables(q, schema).await;
        }
        let f = q.facts();
        let rows = q
            .meta_query(&format!(
                "SELECT table_name, table_type, table_rows, data_length, index_length, table_comment \
                 FROM information_schema.tables WHERE table_schema = {} ORDER BY table_name",
                lit(q, schema)
            ))
            .await?;
        Ok((0..rows.len())
            .map(|r| {
                let kind = table_kind(&rows.text(f, r, 1).unwrap_or_default());
                let table_bytes = rows.u64(r, 3);
                let index_bytes = rows.u64(r, 4);
                let total_bytes = match (table_bytes, index_bytes) {
                    (None, None) => None,
                    (a, b) => Some(a.unwrap_or(0) + b.unwrap_or(0)),
                };
                TableInfo {
                    schema: schema.to_string(),
                    name: rows.text(f, r, 0).unwrap_or_default(),
                    kind,
                    estimated_rows: if kind == TableKind::View { None } else { rows.u64(r, 2) },
                    table_bytes,
                    index_bytes,
                    total_bytes,
                    comment: non_empty(rows.text(f, r, 5)),
                }
            })
            .collect())
    }

    /// `information_schema.key_column_usage` foreign keys of a table: column -> `t(c)` targets
    /// (failures are ignored: the view needs privileges and is slow on some 5.0 servers).
    async fn foreign_keys_is(q: &dyn MysqlRawQuery, schema: &str, table: &str) -> HashMap<String, String> {
        let f = q.facts();
        let sql = format!(
            "SELECT column_name, referenced_table_schema, referenced_table_name, referenced_column_name \
             FROM information_schema.key_column_usage \
             WHERE table_schema = {} AND table_name = {} AND referenced_table_name IS NOT NULL \
             ORDER BY constraint_name, ordinal_position",
            lit(q, schema),
            lit(q, table)
        );
        let rows = match q.meta_query(&sql).await {
            Ok(rows) => rows,
            Err(e) => {
                tracing::debug!("mysql: foreign keys of {schema}.{table} not readable: {e}");
                return HashMap::new();
            }
        };
        let mut out: HashMap<String, String> = HashMap::new();
        for r in 0..rows.len() {
            let column = rows.text(f, r, 0).unwrap_or_default();
            let ref_table = rows.text(f, r, 2).unwrap_or_default();
            let ref_column = rows.text(f, r, 3).unwrap_or_default();
            let target = match rows.text(f, r, 1).filter(|s| !s.is_empty() && s != schema) {
                Some(s) => format!("{s}.{ref_table}({ref_column})"),
                None => format!("{ref_table}({ref_column})"),
            };
            out.entry(column)
                .and_modify(|existing| {
                    if !existing.contains(&target) {
                        existing.push_str(", ");
                        existing.push_str(&target);
                    }
                })
                .or_insert(target);
        }
        out
    }

    pub async fn columns(q: &dyn MysqlRawQuery, table: &TableRef) -> Result<Vec<ColumnInfo>> {
        if mode(q) == CatalogMode::Show {
            return show::columns(q, table).await;
        }
        let f = q.facts();
        let schema = schema_of(f, table)?;
        let foreign_keys = foreign_keys_is(q, &schema, &table.name).await;
        let rows = q
            .meta_query(&format!(
                "SELECT ordinal_position, column_name, column_type, is_nullable, column_default, column_key, column_comment, extra \
                 FROM information_schema.columns WHERE table_schema = {} AND table_name = {} ORDER BY ordinal_position",
                lit(q, &schema),
                lit(q, &table.name)
            ))
            .await?;
        let columns: Vec<ColumnInfo> = (0..rows.len())
            .map(|r| {
                RawColumn {
                    position: rows.u64(r, 0),
                    name: rows.text(f, r, 1),
                    column_type: rows.text(f, r, 2),
                    is_nullable: rows.text(f, r, 3),
                    default: rows.text(f, r, 4),
                    key: rows.text(f, r, 5),
                    comment: rows.text(f, r, 6),
                    extra: rows.text(f, r, 7),
                }
                .into_info(&foreign_keys)
            })
            .collect();
        if columns.is_empty() {
            return Err(missing_table(&schema, &table.name));
        }
        Ok(columns)
    }

    /// Group index rows (in server order) into indexes, `PRIMARY` first.
    fn group_indexes(parts: Vec<(String, bool, String, Option<String>)>) -> (Vec<IndexInfo>, HashMap<String, usize>) {
        let mut indexes: Vec<IndexInfo> = Vec::new();
        let mut slot: HashMap<String, usize> = HashMap::new();
        for (name, unique, column, kind) in parts {
            let i = match slot.get(&name) {
                Some(i) => *i,
                None => {
                    indexes.push(IndexInfo {
                        primary: name == "PRIMARY",
                        unique,
                        kind: non_empty(kind).map(|t| t.to_ascii_lowercase()),
                        name: name.clone(),
                        ..Default::default()
                    });
                    slot.insert(name, indexes.len() - 1);
                    indexes.len() - 1
                }
            };
            indexes[i].columns.push(column);
        }
        // PRIMARY first, then by name (the `information_schema` order; `SHOW INDEX` lists
        // unique keys first).
        indexes.sort_by_cached_key(|ix| (!ix.primary, ix.name.to_uppercase()));
        let slot = indexes.iter().enumerate().map(|(i, ix)| (ix.name.clone(), i)).collect();
        (indexes, slot)
    }

    /// Attach `SHOW CREATE TABLE` definitions (failures ignored: views, missing privileges).
    async fn attach_definitions(q: &dyn MysqlRawQuery, schema: &str, table: &str, indexes: &mut [IndexInfo], slot: &HashMap<String, usize>) {
        match create_table_sql(q, schema, table).await {
            Ok(Some(ddl)) => {
                for (name, def) in index_definitions(&ddl) {
                    if let Some(i) = slot.get(&name) {
                        indexes[*i].definition = Some(def);
                    }
                }
            }
            Ok(None) => {}
            Err(e) => tracing::debug!("mysql: SHOW CREATE TABLE {schema}.{table} failed: {e}"),
        }
    }

    pub async fn indexes(q: &dyn MysqlRawQuery, table: &TableRef) -> Result<Vec<IndexInfo>> {
        if mode(q) == CatalogMode::Show {
            return show::indexes(q, table).await;
        }
        let f = q.facts();
        let schema = schema_of(f, table)?;
        let (s, t) = (lit(q, &schema), lit(q, &table.name));
        let rows = q
            .meta_query(&format!(
                "SELECT index_name, non_unique, column_name, index_type, sub_part \
                 FROM information_schema.statistics WHERE table_schema = {s} AND table_name = {t} \
                 ORDER BY (index_name = 'PRIMARY') DESC, index_name, seq_in_index"
            ))
            .await?;
        let parts = (0..rows.len())
            .map(|r| {
                let column = match (rows.text(f, r, 2), rows.u64(r, 4)) {
                    (Some(c), Some(n)) => format!("{c}({n})"),
                    (Some(c), None) => c,
                    // Functional index parts have no column name.
                    (None, _) => "(expression)".to_string(),
                };
                (rows.text(f, r, 0).unwrap_or_default(), rows.u64(r, 1).unwrap_or(1) == 0, column, rows.text(f, r, 3))
            })
            .collect();
        let (mut indexes, slot) = group_indexes(parts);
        if indexes.is_empty() {
            return Ok(indexes);
        }

        // Sizes from InnoDB persistent statistics (MySQL 5.6+ / MariaDB 10.0+); needs SELECT on
        // `mysql.*`, so failures are silently ignored.
        let v = &f.version;
        let stats = if v.flavor == Flavor::MariaDb { v.at_least(10, 0, 0) } else { v.at_least(5, 6, 0) };
        if stats {
            let sql = format!(
                "SELECT index_name, stat_value * @@innodb_page_size \
                 FROM mysql.innodb_index_stats WHERE database_name = {s} AND table_name = {t} AND stat_name = 'size'"
            );
            match q.meta_query(&sql).await {
                Ok(sizes) => {
                    for r in 0..sizes.len() {
                        if let Some(i) = sizes.text(f, r, 0).and_then(|n| slot.get(&n).copied()) {
                            indexes[i].size_bytes = sizes.u64(r, 1);
                        }
                    }
                }
                Err(e) => tracing::debug!("mysql: innodb_index_stats of {schema}.{} not readable: {e}", table.name),
            }
        }
        attach_definitions(q, &schema, &table.name, &mut indexes, &slot).await;
        Ok(indexes)
    }

    pub async fn ddl(q: &dyn MysqlRawQuery, table: &TableRef) -> Result<String> {
        let schema = schema_of(q.facts(), table)?;
        match create_table_sql(q, &schema, &table.name).await {
            Ok(Some(ddl)) => Ok(ddl),
            Ok(None) => Err(missing_table(&schema, &table.name)),
            Err(e) if server_error_code(&e) == Some(ER_NO_SUCH_TABLE) => Err(missing_table(&schema, &table.name)),
            Err(e) => Err(e),
        }
    }

    pub async fn exact_count(q: &dyn MysqlRawQuery, table: &TableRef, cancel: CancellationToken) -> Result<u64> {
        let rows = q.meta_query_cancellable(&format!("SELECT COUNT(*) FROM {}", quote_table(table)), cancel).await?;
        rows.u64(0, 0).ok_or_else(|| CoreError::Internal(format!("COUNT(*) of {} returned no row", table.qualified())))
    }

    /// `` `db`.`table` `` (schema, else database, else the bare name).
    pub fn quote_table(table: &TableRef) -> String {
        let schema = table.schema.as_deref().filter(|s| !s.is_empty()).or_else(|| table.database.as_deref().filter(|s| !s.is_empty()));
        match schema {
            Some(s) => format!("{}.{}", quote_ident(s), quote_ident(&table.name)),
            None => quote_ident(&table.name),
        }
    }

    /// The `SHOW`-based catalog of MySQL 4.0 / 4.1 (works on every later version too).
    pub mod show {
        use super::*;

        fn not_found_as(e: CoreError, what: impl FnOnce() -> CoreError) -> CoreError {
            match server_error_code(&e) {
                Some(ER_NO_SUCH_TABLE | ER_BAD_DB_ERROR) => what(),
                _ => e,
            }
        }

        /// `SHOW DATABASES`. 4.0 has one server-wide charset, reported as every database's
        /// encoding; 4.1 per-database charsets would cost a statement per database.
        pub async fn databases(q: &dyn MysqlRawQuery) -> Result<Vec<DatabaseInfo>> {
            let f = q.facts();
            let rows = q.meta_query("SHOW DATABASES").await?;
            let encoding = if f.caps.set_names { None } else { f.server_charset.clone() };
            Ok(texts(&rows, f, 0)
                .into_iter()
                .flatten()
                .map(|name| DatabaseInfo {
                    is_current: f.database.as_deref().is_some_and(|d| d.eq_ignore_ascii_case(&name)),
                    name,
                    size_bytes: None,
                    encoding: encoding.clone(),
                    collation: None,
                    accessible: true,
                })
                .collect())
        }

        /// Existence via `SHOW DATABASES LIKE`, table count via `SHOW TABLES FROM`.
        pub async fn schemas(q: &dyn MysqlRawQuery, db: &str) -> Result<Vec<SchemaInfo>> {
            let f = q.facts();
            let rows = q.meta_query(&format!("SHOW DATABASES LIKE {}", lit(q, &like_literal(db)))).await?;
            if !texts(&rows, f, 0).into_iter().flatten().any(|n| n.eq_ignore_ascii_case(db)) {
                return Err(CoreError::NotFound(format!("database '{db}' does not exist or is not visible to this account")));
            }
            let table_count = match q.meta_query(&format!("SHOW TABLES FROM {}", quote_ident(db))).await {
                Ok(t) => Some(t.len() as u64),
                Err(e) => {
                    tracing::debug!("mysql: SHOW TABLES FROM {db} failed: {e}");
                    None
                }
            };
            Ok(vec![SchemaInfo { name: db.to_string(), owner: None, table_count }])
        }

        /// `SHOW TABLE STATUS FROM db`: `Name`, `Engine` (4.1.2+) / `Type` (4.0), `Rows`,
        /// `Data_length`, `Index_length`, `Comment`. On 5.0 a view has `Comment = 'VIEW'` and no
        /// engine. Sorted by name, case-insensitively (uppercase, like `utf8_general_ci`), as `ORDER BY table_name` does.
        pub async fn tables(q: &dyn MysqlRawQuery, schema: &str) -> Result<Vec<TableInfo>> {
            let f = q.facts();
            let rows = q
                .meta_query(&format!("SHOW TABLE STATUS FROM {}", quote_ident(schema)))
                .await
                .map_err(|e| not_found_as(e, || CoreError::NotFound(format!("database '{schema}' does not exist or is not visible to this account"))))?;
            let col = |n: &str| rows.col(n);
            let (c_name, c_engine, c_rows, c_data, c_index, c_comment) =
                (col("Name"), col("Engine").or_else(|| col("Type")), col("Rows"), col("Data_length"), col("Index_length"), col("Comment"));
            let mut out: Vec<TableInfo> = (0..rows.len())
                .filter_map(|r| {
                    let name = rows.text(f, r, c_name?)?;
                    let comment = c_comment.and_then(|c| rows.text(f, r, c)).unwrap_or_default();
                    let engine = c_engine.and_then(|c| rows.text(f, r, c));
                    let kind = if engine.is_none() && comment.trim().eq_ignore_ascii_case("VIEW") { TableKind::View } else { TableKind::Table };
                    let table_bytes = c_data.and_then(|c| rows.u64(r, c));
                    let index_bytes = c_index.and_then(|c| rows.u64(r, c));
                    let total_bytes = match (table_bytes, index_bytes) {
                        (None, None) => None,
                        (a, b) => Some(a.unwrap_or(0) + b.unwrap_or(0)),
                    };
                    Some(TableInfo {
                        schema: schema.to_string(),
                        name,
                        kind,
                        estimated_rows: if kind == TableKind::View { None } else { c_rows.and_then(|c| rows.u64(r, c)) },
                        table_bytes,
                        index_bytes,
                        total_bytes,
                        comment: if kind == TableKind::View { None } else { clean_table_comment(&comment) },
                    })
                })
                .collect();
            out.sort_by_cached_key(|t| t.name.to_uppercase());
            Ok(out)
        }

        /// `SHOW FULL COLUMNS FROM t FROM db` (`Field`, `Type`, `Null`, `Key`, `Default`,
        /// `Extra`, `Comment` from 4.1) plus InnoDB foreign keys from `SHOW CREATE TABLE`.
        pub async fn columns(q: &dyn MysqlRawQuery, table: &TableRef) -> Result<Vec<ColumnInfo>> {
            let f = q.facts();
            let schema = schema_of(f, table)?;
            let rows = q
                .meta_query(&format!("SHOW FULL COLUMNS FROM {} FROM {}", quote_ident(&table.name), quote_ident(&schema)))
                .await
                .map_err(|e| not_found_as(e, || missing_table(&schema, &table.name)))?;
            let foreign_keys = match create_table_sql(q, &schema, &table.name).await {
                Ok(Some(ddl)) => foreign_keys_from_ddl(&ddl, &schema),
                Ok(None) => HashMap::new(),
                Err(e) => {
                    tracing::debug!("mysql: SHOW CREATE TABLE {schema}.{} failed: {e}", table.name);
                    HashMap::new()
                }
            };
            let get = |r: usize, name: &str| rows.col(name).and_then(|c| rows.text(f, r, c));
            let columns: Vec<ColumnInfo> = (0..rows.len())
                .map(|r| {
                    RawColumn {
                        position: Some(r as u64 + 1),
                        name: get(r, "Field"),
                        column_type: get(r, "Type"),
                        // 4.0 reports `YES` or an empty string.
                        is_nullable: Some(get(r, "Null").unwrap_or_default()),
                        default: get(r, "Default"),
                        key: get(r, "Key"),
                        comment: get(r, "Comment"),
                        extra: get(r, "Extra"),
                    }
                    .into_info(&foreign_keys)
                })
                .collect();
            if columns.is_empty() {
                return Err(missing_table(&schema, &table.name));
            }
            Ok(columns)
        }

        /// `SHOW INDEX FROM t FROM db` (`Key_name`, `Non_unique`, `Column_name`, `Sub_part`,
        /// `Index_type` from 4.0.2) plus definitions from `SHOW CREATE TABLE`.
        pub async fn indexes(q: &dyn MysqlRawQuery, table: &TableRef) -> Result<Vec<IndexInfo>> {
            let f = q.facts();
            let schema = schema_of(f, table)?;
            let rows = q
                .meta_query(&format!("SHOW INDEX FROM {} FROM {}", quote_ident(&table.name), quote_ident(&schema)))
                .await
                .map_err(|e| not_found_as(e, || missing_table(&schema, &table.name)))?;
            let get = |r: usize, name: &str| rows.col(name).and_then(|c| rows.text(f, r, c));
            let parts = (0..rows.len())
                .filter_map(|r| {
                    let name = get(r, "Key_name")?;
                    let column = match (get(r, "Column_name"), rows.col("Sub_part").and_then(|c| rows.u64(r, c))) {
                        (Some(c), Some(n)) => format!("{c}({n})"),
                        (Some(c), None) => c,
                        (None, _) => "(expression)".to_string(),
                    };
                    let unique = rows.col("Non_unique").and_then(|c| rows.u64(r, c)) == Some(0);
                    Some((name, unique, column, get(r, "Index_type")))
                })
                .collect();
            let (mut indexes, slot) = group_indexes(parts);
            if !indexes.is_empty() {
                attach_definitions(q, &schema, &table.name, &mut indexes, &slot).await;
            }
            Ok(indexes)
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

    #[test]
    fn foreign_keys_from_create_table() {
        let ddl = "CREATE TABLE `orders` (\n\
                   \x20 `id` int(11) NOT NULL default '0',\n\
                   \x20 `customer_id` int(11) default NULL,\n\
                   \x20 `a` int, `b` int,\n\
                   \x20 PRIMARY KEY  (`id`),\n\
                   \x20 CONSTRAINT `0_12` FOREIGN KEY (`customer_id`) REFERENCES `customers` (`id`),\n\
                   \x20 CONSTRAINT `fk2` FOREIGN KEY (`a`, `b`) REFERENCES `other`.`pairs` (`x`, `y`) ON DELETE CASCADE,\n\
                   \x20 CONSTRAINT `fk3` FOREIGN KEY (`customer_id`) REFERENCES `shop`.`archive` (`cid`)\n\
                   ) TYPE=InnoDB";
        let fks = foreign_keys_from_ddl(ddl, "shop");
        assert_eq!(fks.get("customer_id").map(String::as_str), Some("customers(id), archive(cid)"));
        assert_eq!(fks.get("a").map(String::as_str), Some("other.pairs(x)"));
        assert_eq!(fks.get("b").map(String::as_str), Some("other.pairs(y)"));
        assert_eq!(fks.len(), 3);
        assert!(foreign_keys_from_ddl("CREATE TABLE t (id int) TYPE=MyISAM", "shop").is_empty());
    }

    #[test]
    fn innodb_notes_are_stripped_from_comments() {
        assert_eq!(clean_table_comment("InnoDB free: 4096 kB").as_deref(), None);
        assert_eq!(clean_table_comment("customers; InnoDB free: 4096 kB").as_deref(), Some("customers"));
        assert_eq!(clean_table_comment("InnoDB free: 4096 kB; (`customer_id`) REFER `shop/customers`(`id`)").as_deref(), None);
        assert_eq!(clean_table_comment("a; b").as_deref(), Some("a; b"));
        assert_eq!(clean_table_comment("").as_deref(), None);
    }

    mod versions {
        use crate::catalog::mysql::generic::{self, show, CatalogMode};
        use crate::encoding::Charset;
        use crate::error::CoreError;
        use crate::source::mysql_common::test_support::{facts, text_rows, utf8, Canned};
        use crate::source::mysql_common::{MetaColumn, MetaRows, ER_NO_SUCH_TABLE};
        use crate::source::mysql_dispatch::TextPlan;
        use crate::types::{TableKind, TableRef};
        use tokio_util::sync::CancellationToken;

        fn cp949(s: &str) -> Vec<u8> {
            let (bytes, _, unmappable) = encoding_rs::EUC_KR.encode(s);
            assert!(!unmappable);
            bytes.into_owned()
        }

        /// A `SHOW` result whose cells are CP949 bytes (`None` = NULL), as a 4.0 server sends it.
        fn euc_rows(columns: &[&str], data: &[&[Option<&str>]]) -> MetaRows {
            MetaRows {
                columns: columns.iter().map(|c| MetaColumn { name: c.to_string(), collation: None }).collect(),
                rows: data.iter().map(|r| r.iter().map(|c| c.map(cp949)).collect()).collect(),
            }
        }

        fn mysql40() -> crate::source::mysql_common::ServerFacts {
            let mut f = facts("4.0.27-standard", TextPlan::Fixed { charset: Charset::EucKr });
            f.server_charset = Some("euc_kr".into());
            f
        }

        #[test]
        fn mode_by_version() {
            assert_eq!(CatalogMode::for_facts(&facts("4.0.27", utf8())), CatalogMode::Show);
            assert_eq!(CatalogMode::for_facts(&facts("4.1.22", utf8())), CatalogMode::Show);
            assert_eq!(CatalogMode::for_facts(&facts("5.0.96", utf8())), CatalogMode::InformationSchema);
            assert_eq!(CatalogMode::for_facts(&facts("8.0.36", utf8())), CatalogMode::InformationSchema);
        }

        #[test]
        fn known_cp949_vector() {
            assert_eq!(cp949("한글"), [0xC7, 0xD1, 0xB1, 0xDB]);
        }

        #[tokio::test]
        async fn show_catalog_decodes_euc_kr_names_and_comments() {
            let q = Canned::new(mysql40())
                .on(
                    "SHOW TABLE STATUS FROM `shop`",
                    euc_rows(
                        &["Name", "Type", "Row_format", "Rows", "Avg_row_length", "Data_length", "Max_data_length", "Index_length", "Comment"],
                        &[
                            &[Some("주문"), Some("InnoDB"), Some("Dynamic"), Some("1200"), Some("80"), Some("98304"), None, Some("16384"), Some("주문 목록; InnoDB free: 4096 kB")],
                            &[Some("고객"), Some("MyISAM"), Some("Fixed"), Some("12"), Some("20"), Some("240"), Some("4294967295"), Some("1024"), Some("")],
                        ],
                    ),
                )
                .on(
                    "SHOW FULL COLUMNS FROM `고객` FROM `shop`",
                    euc_rows(
                        &["Field", "Type", "Null", "Key", "Default", "Extra", "Privileges"],
                        &[
                            &[Some("번호"), Some("int(11)"), Some(""), Some("PRI"), None, Some("auto_increment"), Some("select")],
                            &[Some("이름"), Some("varchar(20)"), Some("YES"), Some("MUL"), Some("무명"), Some(""), Some("select")],
                        ],
                    ),
                )
                .on(
                    "SHOW INDEX FROM `고객` FROM `shop`",
                    euc_rows(
                        &["Table", "Non_unique", "Key_name", "Seq_in_index", "Column_name", "Collation", "Cardinality", "Sub_part", "Packed", "Comment"],
                        &[
                            &[Some("고객"), Some("1"), Some("ix_이름"), Some("1"), Some("이름"), Some("A"), None, Some("4"), None, Some("")],
                            &[Some("고객"), Some("0"), Some("PRIMARY"), Some("1"), Some("번호"), Some("A"), Some("12"), None, None, Some("")],
                        ],
                    ),
                )
                .on(
                    "SHOW CREATE TABLE `shop`.`고객`",
                    euc_rows(
                        &["Table", "Create Table"],
                        &[&[Some("고객"), Some("CREATE TABLE `고객` (\n  `번호` int(11) NOT NULL auto_increment,\n  `이름` varchar(20) default '무명',\n  PRIMARY KEY  (`번호`),\n  KEY `ix_이름` (`이름`(4))\n) TYPE=MyISAM")]],
                    ),
                )
                .on("SHOW DATABASES", euc_rows(&["Database"], &[&[Some("mysql")], &[Some("shop")]]));

            let tables = generic::tables(&q, "shop", "").await.unwrap();
            assert_eq!(tables.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(), ["고객", "주문"], "sorted by name");
            let order = &tables[1];
            assert_eq!(order.kind, TableKind::Table);
            assert_eq!(order.estimated_rows, Some(1200));
            assert_eq!((order.table_bytes, order.index_bytes, order.total_bytes), (Some(98304), Some(16384), Some(114688)));
            assert_eq!(order.comment.as_deref(), Some("주문 목록"));
            assert_eq!(tables[0].comment, None);

            let t = TableRef::new(Some("shop"), "고객");
            let cols = generic::columns(&q, &t).await.unwrap();
            assert_eq!(cols.len(), 2);
            assert_eq!((cols[0].position, cols[0].name.as_str(), cols[0].nullable, cols[0].primary_key), (1, "번호", false, true));
            assert_eq!(cols[0].default.as_deref(), Some("AUTO_INCREMENT"));
            assert_eq!((cols[1].name.as_str(), cols[1].nullable, cols[1].default.as_deref()), ("이름", true, Some("무명")));
            assert_eq!(cols[1].comment, None, "4.0 has no column comments");

            let ix = generic::indexes(&q, &t).await.unwrap();
            assert_eq!(ix.iter().map(|i| i.name.as_str()).collect::<Vec<_>>(), ["PRIMARY", "ix_이름"]);
            assert!(ix[0].primary && ix[0].unique);
            assert_eq!(ix[1].columns, ["이름(4)"]);
            assert!(!ix[1].unique);
            assert_eq!(ix[1].kind, None, "no Index_type column before 4.0.2");
            assert_eq!(ix[1].definition.as_deref(), Some("KEY `ix_이름` (`이름`(4))"));
            assert!(generic::ddl(&q, &t).await.unwrap().starts_with("CREATE TABLE `고객`"));

            let dbs = generic::databases(&q).await.unwrap();
            assert_eq!(dbs.len(), 2);
            assert!(dbs[1].is_current && !dbs[0].is_current);
            assert_eq!(dbs[1].encoding.as_deref(), Some("euc_kr"), "4.0: one server-wide charset");

            let log = q.log();
            assert!(log.iter().all(|s| !s.contains("information_schema")), "{log:?}");
        }

        #[tokio::test]
        async fn show_schemas_and_missing_objects() {
            let q = Canned::new(mysql40())
                .on("SHOW DATABASES LIKE 'my\\\\_db'", text_rows(&["Database (my\\_db)"], &[&["my_db"]]))
                .on("SHOW TABLES FROM `my_db`", text_rows(&["Tables_in_my_db"], &[&["a"], &["b"]]))
                .fail("SHOW FULL COLUMNS FROM `gone`", ER_NO_SUCH_TABLE)
                .fail("SHOW CREATE TABLE `shop`.`gone`", ER_NO_SUCH_TABLE);
            let s = generic::schemas(&q, "my_db").await.unwrap();
            assert_eq!((s[0].name.as_str(), s[0].table_count), ("my_db", Some(2)));
            assert!(matches!(generic::schemas(&q, "nope").await, Err(CoreError::NotFound(_))));
            let gone = TableRef::new(Some("shop"), "gone");
            assert!(matches!(generic::columns(&q, &gone).await, Err(CoreError::NotFound(_))));
            assert!(matches!(generic::ddl(&q, &gone).await, Err(CoreError::NotFound(_))));
            assert!(matches!(generic::columns(&q, &TableRef { database: None, schema: None, name: "x".into() }).await, Ok(_) | Err(CoreError::NotFound(_))));
        }

        #[tokio::test]
        async fn information_schema_on_5x_with_inline_literals() {
            let q = Canned::new(facts("5.5.62", utf8()))
                .on(
                    "FROM information_schema.tables WHERE",
                    text_rows(
                        &["table_name", "table_type", "table_rows", "data_length", "index_length", "table_comment"],
                        &[&["t", "BASE TABLE", "5", "16384", "0", ""], &["v", "VIEW", "\\N", "\\N", "\\N", "VIEW"]],
                    ),
                )
                .on("SELECT COUNT(*) FROM `shop`.`t`", text_rows(&["COUNT(*)"], &[&["5"]]))
                .on(
                    "information_schema.statistics",
                    text_rows(&["index_name", "non_unique", "column_name", "index_type", "sub_part"], &[&["PRIMARY", "0", "id", "BTREE", "\\N"]]),
                );
            let tables = generic::tables(&q, "shop", "shop").await.unwrap();
            assert_eq!(tables.len(), 2);
            assert_eq!(tables[1].kind, TableKind::View);
            assert_eq!(tables[1].estimated_rows, None);
            let t = TableRef::new(Some("shop"), "t");
            assert_eq!(generic::exact_count(&q, &t, CancellationToken::new()).await.unwrap(), 5);
            let log = q.log();
            assert!(log[0].contains("WHERE table_schema = 'shop' ORDER BY table_name"), "{log:?}");
            // No innodb_index_stats before 5.6.
            generic::indexes(&q, &t).await.unwrap();
            assert!(q.log().iter().all(|s| !s.contains("innodb_index_stats")));
            // A quote in a name is escaped, not interpreted.
            generic::tables(&q, "x'; DROP TABLE t; --", "").await.unwrap();
            assert!(q.log().last().unwrap().contains("table_schema = 'x''; DROP TABLE t; --'"));
            // The SHOW family is usable on 5.x too.
            show::tables(&q, "shop").await.unwrap();
            assert_eq!(q.log().last().map(String::as_str), Some("SHOW TABLE STATUS FROM `shop`"));
        }
    }
}
