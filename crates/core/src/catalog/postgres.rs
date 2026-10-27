//! Catalog implementation for [`PgDatabase`]: metadata straight from the system catalogs.
//!
//! * One round trip per listing call (`databases`, `schemas`, `tables`, `columns`, `indexes`):
//!   every per-row detail (sizes, comments, constraint flags) is folded into the same query.
//! * A PostgreSQL session is bound to one database; `schemas` / `tables` reject another `db`.
//! * Tables are resolved with `to_regclass`, so a `TableRef` without a schema follows the
//!   session's `search_path` and a qualified one is exact.
//! * `ddl` reconstructs `CREATE TABLE` (columns, constraints), `CREATE INDEX` for every index
//!   that is not owned by a constraint, and `COMMENT ON` statements; views get
//!   `CREATE [MATERIALIZED] VIEW ... AS <pg_get_viewdef>`.
//! * Row counts are `reltuples` estimates; `exact_count` runs `count(*)` and is cancellable.

use async_trait::async_trait;
use tokio_postgres::Row;
use tokio_util::sync::CancellationToken;
use tracing::debug;

use super::Catalog;
use crate::error::{CoreError, Result};
use crate::source::postgres::{full_message, map_pg_error, quote_ident, quote_literal, PgDatabase};
use crate::types::{ColumnInfo, DatabaseInfo, IndexInfo, SchemaInfo, TableInfo, TableKind, TableRef};

// ---------------------------------------------------------------------------------------------
// SQL
// ---------------------------------------------------------------------------------------------

/// `pg_database` listing; `{size}` is the size expression (NULL when sizes are not readable).
const DATABASES_SQL: &str = "SELECT d.datname::text, \
       ({size})::int8, \
       pg_encoding_to_char(d.encoding)::text, \
       d.datcollate::text, \
       (d.datallowconn AND has_database_privilege(d.datname, 'CONNECT')) AS accessible, \
       (d.datname = current_database()) AS is_current \
  FROM pg_database d \
 WHERE NOT d.datistemplate \
 ORDER BY is_current DESC, d.datname";

const DATABASE_SIZE_EXPR: &str = "CASE WHEN d.datallowconn AND has_database_privilege(d.datname, 'CONNECT') THEN pg_database_size(d.oid) END";

/// User schemas first (`pg_catalog` / `information_schema` last), toast / temp schemas hidden.
const SCHEMAS_SQL: &str = "SELECT n.nspname::text, \
       pg_get_userbyid(n.nspowner)::text, \
       count(c.oid)::int8, \
       (n.nspname IN ('pg_catalog', 'information_schema')) AS system \
  FROM pg_namespace n \
  LEFT JOIN pg_class c ON c.relnamespace = n.oid AND c.relkind IN ('r', 'v', 'm', 'p', 'f') \
 WHERE n.nspname !~ '^pg_(toast|temp)' \
 GROUP BY n.oid, n.nspname, n.nspowner \
 ORDER BY system, n.nspname";

/// Tables, views, materialized views, partitioned and foreign tables of one schema. A
/// partitioned parent reports the sum over its direct partitions (its own storage is empty).
const TABLES_SQL: &str = "SELECT c.relname::text, \
       c.relkind::text, \
       (CASE WHEN c.relkind = 'p' \
             THEN (SELECT sum(ch.reltuples) FROM pg_inherits i JOIN pg_class ch ON ch.oid = i.inhrelid WHERE i.inhparent = c.oid) \
             ELSE c.reltuples END)::float8, \
       (CASE WHEN c.relkind = 'p' \
             THEN (SELECT sum(pg_table_size(i.inhrelid)) FROM pg_inherits i WHERE i.inhparent = c.oid) \
             ELSE pg_table_size(c.oid) END)::int8, \
       (CASE WHEN c.relkind = 'p' \
             THEN (SELECT sum(pg_indexes_size(i.inhrelid)) FROM pg_inherits i WHERE i.inhparent = c.oid) \
             ELSE pg_indexes_size(c.oid) END)::int8, \
       (CASE WHEN c.relkind = 'p' \
             THEN (SELECT sum(pg_total_relation_size(i.inhrelid)) FROM pg_inherits i WHERE i.inhparent = c.oid) \
             ELSE pg_total_relation_size(c.oid) END)::int8, \
       obj_description(c.oid, 'pg_class')::text \
  FROM pg_class c \
  JOIN pg_namespace n ON n.oid = c.relnamespace \
 WHERE n.nspname = $1 AND c.relkind IN ('r', 'v', 'm', 'p', 'f') \
 ORDER BY c.relname";

/// Columns with type, default, comment and PK / UNIQUE / FK membership in one query.
const COLUMNS_SQL: &str = "SELECT a.attnum::int4, \
       a.attname::text, \
       format_type(a.atttypid, a.atttypmod)::text, \
       a.attnotnull, \
       pg_get_expr(d.adbin, d.adrelid)::text, \
       col_description(a.attrelid, a.attnum)::text, \
       EXISTS (SELECT 1 FROM pg_constraint p WHERE p.conrelid = a.attrelid AND p.contype = 'p' AND a.attnum = ANY (p.conkey)) AS pk, \
       EXISTS (SELECT 1 FROM pg_constraint u WHERE u.conrelid = a.attrelid AND u.contype = 'u' AND a.attnum = ANY (u.conkey)) AS uniq, \
       (SELECT fn.nspname || '.' || fc.relname || '(' || fa.attname || ')' \
          FROM pg_constraint f \
          JOIN pg_class fc ON fc.oid = f.confrelid \
          JOIN pg_namespace fn ON fn.oid = fc.relnamespace \
          JOIN pg_attribute fa ON fa.attrelid = f.confrelid \
                              AND fa.attnum = f.confkey[array_position(f.conkey, a.attnum)] \
         WHERE f.conrelid = a.attrelid AND f.contype = 'f' AND a.attnum = ANY (f.conkey) \
         ORDER BY f.conname LIMIT 1)::text AS fk \
  FROM pg_attribute a \
  LEFT JOIN pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum \
 WHERE a.attrelid = to_regclass($1::text) AND a.attnum > 0 AND NOT a.attisdropped \
 ORDER BY a.attnum";

/// Indexes with their key / INCLUDE columns in `indkey` order (expression columns rendered by
/// `pg_get_indexdef`), access method, predicate, size, scan count and full definition.
/// `{nkeys}` is `indnkeyatts` (PostgreSQL 11+) or `indnatts`.
const INDEXES_SQL: &str = "SELECT ic.relname::text, \
       i.indisunique, \
       i.indisprimary, \
       am.amname::text, \
       pg_get_expr(i.indpred, i.indrelid)::text, \
       pg_relation_size(i.indexrelid)::int8, \
       s.idx_scan::int8, \
       pg_get_indexdef(i.indexrelid)::text, \
       i.{nkeys}::int4, \
       (SELECT array_agg(coalesce(a.attname::text, pg_get_indexdef(i.indexrelid, k.ord::int4, true)) ORDER BY k.ord) \
          FROM unnest(i.indkey::int2[]) WITH ORDINALITY AS k(attnum, ord) \
          LEFT JOIN pg_attribute a ON a.attrelid = i.indrelid AND a.attnum = k.attnum) \
  FROM pg_index i \
  JOIN pg_class ic ON ic.oid = i.indexrelid \
  LEFT JOIN pg_am am ON am.oid = ic.relam \
  LEFT JOIN pg_stat_user_indexes s ON s.indexrelid = i.indexrelid \
 WHERE i.indrelid = to_regclass($1::text) \
 ORDER BY i.indisprimary DESC, ic.relname";

/// Relation header for `ddl`: resolved schema / name, kind, comment, view definition, foreign
/// server. `{partkey}` is `pg_get_partkeydef(c.oid)` on PostgreSQL 10+.
const RELATION_SQL: &str = "SELECT n.nspname::text, \
       c.relname::text, \
       c.relkind::text, \
       obj_description(c.oid, 'pg_class')::text, \
       (CASE WHEN c.relkind IN ('v', 'm') THEN pg_get_viewdef(c.oid, true) END)::text, \
       (SELECT fs.srvname FROM pg_foreign_table ft JOIN pg_foreign_server fs ON fs.oid = ft.ftserver WHERE ft.ftrelid = c.oid)::text, \
       ({partkey})::text \
  FROM pg_class c \
  JOIN pg_namespace n ON n.oid = c.relnamespace \
 WHERE c.oid = to_regclass($1::text)";

/// Table constraints in DDL order: primary key, unique, foreign keys, exclusion, check.
const CONSTRAINTS_SQL: &str = "SELECT conname::text, contype::text, pg_get_constraintdef(oid, true)::text \
  FROM pg_constraint \
 WHERE conrelid = to_regclass($1::text) AND contype IN ('p', 'u', 'f', 'x', 'c') \
 ORDER BY CASE contype WHEN 'p' THEN 0 WHEN 'u' THEN 1 WHEN 'f' THEN 2 WHEN 'x' THEN 3 ELSE 4 END, conname";

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

/// `pg_class.relkind` -> [`TableKind`].
pub(crate) fn table_kind(relkind: &str) -> TableKind {
    match relkind {
        "r" => TableKind::Table,
        "v" => TableKind::View,
        "m" => TableKind::MaterializedView,
        "p" => TableKind::Partitioned,
        "f" => TableKind::Foreign,
        _ => TableKind::Other,
    }
}

/// `"schema"."name"` or `"name"` (resolved through `search_path` by `to_regclass`).
pub(crate) fn regclass_name(table: &TableRef) -> String {
    match table.schema.as_deref().filter(|s| !s.is_empty()) {
        Some(s) => format!("{}.{}", quote_ident(s), quote_ident(&table.name)),
        None => quote_ident(&table.name),
    }
}

fn opt_u64(v: Option<i64>) -> Option<u64> {
    v.and_then(|x| u64::try_from(x).ok())
}

/// `reltuples`: `-1` (never analyzed, PostgreSQL 14+) and NULL mean "unknown".
fn estimated_rows(reltuples: Option<f64>) -> Option<u64> {
    reltuples.filter(|r| r.is_finite() && *r >= 0.0).map(|r| r.round() as u64)
}

/// Resolved relation header used by [`build_table_ddl`].
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct RelationHead {
    pub schema: String,
    pub name: String,
    pub kind: TableKind,
    pub comment: Option<String>,
    /// `pg_get_viewdef` for views and materialized views.
    pub view_def: Option<String>,
    /// Foreign server name for foreign tables.
    pub foreign_server: Option<String>,
    /// `pg_get_partkeydef` for partitioned tables.
    pub partition_key: Option<String>,
}

/// One row of `pg_constraint` as `pg_get_constraintdef` renders it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ConstraintDef {
    pub name: String,
    /// `p`, `u`, `f`, `x` or `c`.
    pub contype: String,
    pub definition: String,
}

/// Assemble the DDL text from the pieces the catalog queries return.
///
/// Indexes whose name matches a primary-key / unique / exclusion constraint are owned by that
/// constraint and therefore not repeated as `CREATE INDEX` (as `pg_dump` does).
pub(crate) fn build_table_ddl(head: &RelationHead, columns: &[ColumnInfo], constraints: &[ConstraintDef], indexes: &[IndexInfo]) -> String {
    let qualified = format!("{}.{}", quote_ident(&head.schema), quote_ident(&head.name));
    let mut out = String::new();
    match head.kind {
        TableKind::View | TableKind::MaterializedView => {
            let keyword = if head.kind == TableKind::View { "CREATE OR REPLACE VIEW" } else { "CREATE MATERIALIZED VIEW" };
            // pg_get_viewdef already ends with ';'.
            let body = head
                .view_def
                .as_deref()
                .map(|d| d.trim_end_matches(|c: char| c == ';' || c.is_whitespace()))
                .filter(|d| !d.is_empty())
                .unwrap_or("/* definition not available */");
            out.push_str(&format!("{keyword} {qualified} AS\n{body};\n"));
        }
        _ => {
            let keyword = if head.kind == TableKind::Foreign { "CREATE FOREIGN TABLE" } else { "CREATE TABLE" };
            out.push_str(&format!("{keyword} {qualified} (\n"));
            let mut lines: Vec<String> = columns
                .iter()
                .map(|c| {
                    let mut line = format!("    {} {}", quote_ident(&c.name), c.data_type);
                    if !c.nullable {
                        line.push_str(" NOT NULL");
                    }
                    if let Some(d) = &c.default {
                        line.push_str(" DEFAULT ");
                        line.push_str(d);
                    }
                    line
                })
                .collect();
            lines.extend(constraints.iter().map(|k| format!("    CONSTRAINT {} {}", quote_ident(&k.name), k.definition)));
            out.push_str(&lines.join(",\n"));
            out.push_str("\n)");
            if let Some(pk) = head.partition_key.as_deref().filter(|s| !s.is_empty()) {
                out.push_str(&format!(" PARTITION BY {pk}"));
            }
            if head.kind == TableKind::Foreign {
                out.push_str(&format!(" SERVER {}", head.foreign_server.as_deref().map(quote_ident).unwrap_or_else(|| "/* unknown */".into())));
            }
            out.push_str(";\n");
        }
    }
    let owned: std::collections::HashSet<&str> = constraints.iter().filter(|k| matches!(k.contype.as_str(), "p" | "u" | "x")).map(|k| k.name.as_str()).collect();
    for ix in indexes.iter().filter(|ix| !owned.contains(ix.name.as_str())) {
        if let Some(def) = &ix.definition {
            out.push_str(def.trim_end_matches(';'));
            out.push_str(";\n");
        }
    }
    if let Some(c) = &head.comment {
        out.push_str(&format!("COMMENT ON TABLE {qualified} IS {};\n", quote_literal(c)));
    }
    for c in columns.iter().filter(|c| c.comment.is_some()) {
        let comment = c.comment.as_deref().unwrap_or_default();
        out.push_str(&format!("COMMENT ON COLUMN {qualified}.{} IS {};\n", quote_ident(&c.name), quote_literal(comment)));
    }
    out
}

fn get<'a, T: tokio_postgres::types::FromSql<'a>>(row: &'a Row, idx: usize) -> Result<T> {
    row.try_get(idx).map_err(map_pg_error)
}

/// Statement helpers. None of them takes the session lock: the `Catalog` methods below hold
/// [`PgDatabase::exclusive`] for their whole body (the lock is not reentrant), so no catalog
/// statement can run inside the transaction of an EXPLAIN on the same connection.
impl PgDatabase {
    /// Reject a `db` argument that is not this session's database (PostgreSQL cannot switch).
    fn check_db(&self, db: &str) -> Result<()> {
        if db.is_empty() || db == self.database_name() {
            Ok(())
        } else {
            Err(CoreError::Invalid(format!("this connection is bound to database '{}'; reconnect with database '{db}' to browse it", self.database_name())))
        }
    }

    /// `NotFound` unless `to_regclass` resolves the table.
    async fn ensure_exists(&self, regclass: &str, table: &TableRef) -> Result<()> {
        let row = self.client.query_one("SELECT to_regclass($1::text) IS NOT NULL", &[&regclass]).await.map_err(map_pg_error)?;
        if get::<bool>(&row, 0)? {
            Ok(())
        } else {
            Err(CoreError::NotFound(format!("table {} does not exist or is not visible", table.qualified())))
        }
    }

    async fn relation_head(&self, regclass: &str, table: &TableRef) -> Result<RelationHead> {
        let partkey = if self.server_version_num() >= 100_000 { "CASE WHEN c.relkind = 'p' THEN pg_get_partkeydef(c.oid) END" } else { "NULL" };
        let sql = RELATION_SQL.replace("{partkey}", partkey);
        let rows = self.client.query(sql.as_str(), &[&regclass]).await.map_err(map_pg_error)?;
        let Some(row) = rows.first() else {
            return Err(CoreError::NotFound(format!("table {} does not exist or is not visible", table.qualified())));
        };
        Ok(RelationHead {
            schema: get(row, 0)?,
            name: get(row, 1)?,
            kind: table_kind(&get::<String>(row, 2)?),
            comment: get(row, 3)?,
            view_def: get(row, 4)?,
            foreign_server: get(row, 5)?,
            partition_key: get(row, 6)?,
        })
    }

    async fn constraints(&self, regclass: &str) -> Result<Vec<ConstraintDef>> {
        let rows = self.client.query(CONSTRAINTS_SQL, &[&regclass]).await.map_err(map_pg_error)?;
        rows.iter().map(|r| Ok(ConstraintDef { name: get(r, 0)?, contype: get(r, 1)?, definition: get(r, 2)? })).collect()
    }

    /// Body of [`Catalog::columns`]; the caller holds the session lock.
    async fn columns_unlocked(&self, table: &TableRef) -> Result<Vec<ColumnInfo>> {
        let regclass = regclass_name(table);
        let rows = self.client.query(COLUMNS_SQL, &[&regclass]).await.map_err(map_pg_error)?;
        if rows.is_empty() {
            // Zero-column tables are legal; tell "missing" apart from "empty".
            self.ensure_exists(&regclass, table).await?;
        }
        rows.iter()
            .map(|r| {
                Ok(ColumnInfo {
                    position: u32::try_from(get::<i32>(r, 0)?).unwrap_or(0),
                    name: get(r, 1)?,
                    data_type: get(r, 2)?,
                    nullable: !get::<bool>(r, 3)?,
                    default: get(r, 4)?,
                    comment: get(r, 5)?,
                    primary_key: get(r, 6)?,
                    unique: get(r, 7)?,
                    foreign_key: get(r, 8)?,
                })
            })
            .collect()
    }

    /// Body of [`Catalog::indexes`]; the caller holds the session lock.
    async fn indexes_unlocked(&self, table: &TableRef) -> Result<Vec<IndexInfo>> {
        let regclass = regclass_name(table);
        let nkeys = if self.server_version_num() >= 110_000 { "indnkeyatts" } else { "indnatts" };
        let sql = INDEXES_SQL.replace("{nkeys}", nkeys);
        let rows = self.client.query(sql.as_str(), &[&regclass]).await.map_err(map_pg_error)?;
        if rows.is_empty() {
            self.ensure_exists(&regclass, table).await?;
        }
        rows.iter()
            .map(|r| {
                let nkeys = usize::try_from(get::<i32>(r, 8)?).unwrap_or(0);
                let mut all: Vec<String> = get::<Option<Vec<String>>>(r, 9)?.unwrap_or_default();
                let include = if all.len() > nkeys { all.split_off(nkeys) } else { Vec::new() };
                Ok(IndexInfo {
                    name: get(r, 0)?,
                    columns: all,
                    unique: get(r, 1)?,
                    primary: get(r, 2)?,
                    kind: get(r, 3)?,
                    predicate: get(r, 4)?,
                    include,
                    size_bytes: opt_u64(get(r, 5)?),
                    scans: opt_u64(get(r, 6)?),
                    definition: get(r, 7)?,
                })
            })
            .collect()
    }
}

// ---------------------------------------------------------------------------------------------
// Catalog
// ---------------------------------------------------------------------------------------------

#[async_trait]
impl Catalog for PgDatabase {
    async fn databases(&self) -> Result<Vec<DatabaseInfo>> {
        let with_sizes = DATABASES_SQL.replace("{size}", DATABASE_SIZE_EXPR);
        // Both attempts under one guard: the failing size query runs in autocommit, never
        // inside (and never aborting) an EXPLAIN transaction on this connection.
        let _guard = self.exclusive().await;
        let rows = match self.client.query(with_sizes.as_str(), &[]).await {
            Ok(rows) => rows,
            Err(e) => {
                // Hosted services sometimes grant CONNECT but refuse pg_database_size.
                debug!("database sizes unavailable, listing without them: {}", full_message(&e));
                let without = DATABASES_SQL.replace("{size}", "NULL");
                self.client.query(without.as_str(), &[]).await.map_err(map_pg_error)?
            }
        };
        rows.iter()
            .map(|r| {
                Ok(DatabaseInfo {
                    name: get(r, 0)?,
                    size_bytes: opt_u64(get(r, 1)?),
                    encoding: get(r, 2)?,
                    collation: get(r, 3)?,
                    accessible: get(r, 4)?,
                    is_current: get(r, 5)?,
                })
            })
            .collect()
    }

    async fn schemas(&self, db: &str) -> Result<Vec<SchemaInfo>> {
        self.check_db(db)?;
        let _guard = self.exclusive().await;
        let rows = self.client.query(SCHEMAS_SQL, &[]).await.map_err(map_pg_error)?;
        rows.iter().map(|r| Ok(SchemaInfo { name: get(r, 0)?, owner: get(r, 1)?, table_count: opt_u64(get(r, 2)?) })).collect()
    }

    async fn tables(&self, db: &str, schema: &str) -> Result<Vec<TableInfo>> {
        self.check_db(db)?;
        let _guard = self.exclusive().await;
        let rows = self.client.query(TABLES_SQL, &[&schema]).await.map_err(map_pg_error)?;
        rows.iter()
            .map(|r| {
                let kind = table_kind(&get::<String>(r, 1)?);
                // Views and foreign tables have no local storage or statistics worth showing.
                let has_storage = !matches!(kind, TableKind::View | TableKind::Foreign);
                Ok(TableInfo {
                    schema: schema.to_string(),
                    name: get(r, 0)?,
                    kind,
                    estimated_rows: estimated_rows(get(r, 2)?).filter(|_| has_storage),
                    table_bytes: opt_u64(get(r, 3)?).filter(|_| has_storage),
                    index_bytes: opt_u64(get(r, 4)?).filter(|_| has_storage),
                    total_bytes: opt_u64(get(r, 5)?).filter(|_| has_storage),
                    comment: get(r, 6)?,
                })
            })
            .collect()
    }

    async fn columns(&self, table: &TableRef) -> Result<Vec<ColumnInfo>> {
        let _guard = self.exclusive().await;
        self.columns_unlocked(table).await
    }

    async fn indexes(&self, table: &TableRef) -> Result<Vec<IndexInfo>> {
        let _guard = self.exclusive().await;
        self.indexes_unlocked(table).await
    }

    async fn ddl(&self, table: &TableRef) -> Result<String> {
        let regclass = regclass_name(table);
        // One guard for all statements: the DDL is read without an EXPLAIN in between.
        let _guard = self.exclusive().await;
        let head = self.relation_head(&regclass, table).await?;
        let resolved = TableRef { database: None, schema: Some(head.schema.clone()), name: head.name.clone() };
        let columns = self.columns_unlocked(&resolved).await?;
        let (constraints, indexes) = match head.kind {
            TableKind::View => (Vec::new(), Vec::new()),
            TableKind::MaterializedView => (Vec::new(), self.indexes_unlocked(&resolved).await?),
            _ => (self.constraints(&regclass).await?, self.indexes_unlocked(&resolved).await?),
        };
        Ok(build_table_ddl(&head, &columns, &constraints, &indexes))
    }

    async fn exact_count(&self, table: &TableRef, cancel: CancellationToken) -> Result<u64> {
        let sql = format!("SELECT count(*)::int8 FROM {}", self.quote_table(table));
        // Keeps the count out of an EXPLAIN transaction (and its SET LOCAL timeouts) on this
        // connection; a cancel while another statement holds the session ends the wait.
        let _guard = self.exclusive_or_cancelled(&cancel).await?;
        debug!("exact count: {sql}");
        let row = self.with_cancel(self.client.query_one(sql.as_str(), &[]), &cancel).await?;
        Ok(opt_u64(Some(get::<i64>(&row, 0)?)).unwrap_or(0))
    }
}

// ---------------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn col(position: u32, name: &str, ty: &str, nullable: bool, default: Option<&str>, comment: Option<&str>) -> ColumnInfo {
        ColumnInfo {
            position,
            name: name.into(),
            data_type: ty.into(),
            nullable,
            default: default.map(str::to_string),
            primary_key: false,
            foreign_key: None,
            unique: false,
            comment: comment.map(str::to_string),
        }
    }

    #[test]
    fn relkind_mapping_and_regclass_names() {
        assert_eq!(table_kind("r"), TableKind::Table);
        assert_eq!(table_kind("v"), TableKind::View);
        assert_eq!(table_kind("m"), TableKind::MaterializedView);
        assert_eq!(table_kind("p"), TableKind::Partitioned);
        assert_eq!(table_kind("f"), TableKind::Foreign);
        assert_eq!(table_kind("S"), TableKind::Other);
        assert_eq!(regclass_name(&TableRef::new(Some("Sales"), "or\"ders")), "\"Sales\".\"or\"\"ders\"");
        assert_eq!(regclass_name(&TableRef::new(None, "orders")), "\"orders\"");
        assert_eq!(regclass_name(&TableRef::new(Some(""), "orders")), "\"orders\"");
    }

    #[test]
    fn estimates_and_sizes() {
        assert_eq!(estimated_rows(Some(-1.0)), None);
        assert_eq!(estimated_rows(None), None);
        assert_eq!(estimated_rows(Some(0.0)), Some(0));
        assert_eq!(estimated_rows(Some(1234.6)), Some(1235));
        assert_eq!(opt_u64(Some(-5)), None);
        assert_eq!(opt_u64(Some(5)), Some(5));
        assert_eq!(opt_u64(None), None);
    }

    #[test]
    fn sql_templates_fill_in() {
        assert!(DATABASES_SQL.replace("{size}", DATABASE_SIZE_EXPR).contains("pg_database_size(d.oid)"));
        assert!(!DATABASES_SQL.replace("{size}", "NULL").contains("{size}"));
        assert!(INDEXES_SQL.replace("{nkeys}", "indnkeyatts").contains("i.indnkeyatts::int4"));
        assert!(!RELATION_SQL.replace("{partkey}", "NULL").contains("{partkey}"));
    }

    #[test]
    fn table_ddl_has_columns_constraints_indexes_and_comments() {
        let head = RelationHead { schema: "public".into(), name: "orders".into(), kind: TableKind::Table, comment: Some("it's orders".into()), ..Default::default() };
        let columns = vec![
            col(1, "id", "bigint", false, Some("nextval('orders_id_seq'::regclass)"), None),
            col(2, "customer_id", "integer", true, None, Some("FK to customers")),
            col(3, "amount", "numeric(12,2)", false, Some("0"), None),
        ];
        let constraints = vec![
            ConstraintDef { name: "orders_pkey".into(), contype: "p".into(), definition: "PRIMARY KEY (id)".into() },
            ConstraintDef { name: "orders_customer_id_fkey".into(), contype: "f".into(), definition: "FOREIGN KEY (customer_id) REFERENCES customers(id)".into() },
        ];
        let indexes = vec![
            IndexInfo {
                name: "orders_pkey".into(),
                columns: vec!["id".into()],
                unique: true,
                primary: true,
                definition: Some("CREATE UNIQUE INDEX orders_pkey ON public.orders USING btree (id)".into()),
                ..Default::default()
            },
            IndexInfo {
                name: "orders_customer_idx".into(),
                columns: vec!["customer_id".into()],
                definition: Some("CREATE INDEX orders_customer_idx ON public.orders USING btree (customer_id) WHERE (amount > (0)::numeric)".into()),
                ..Default::default()
            },
        ];
        let ddl = build_table_ddl(&head, &columns, &constraints, &indexes);
        let expected = [
            "CREATE TABLE \"public\".\"orders\" (",
            "    \"id\" bigint NOT NULL DEFAULT nextval('orders_id_seq'::regclass),",
            "    \"customer_id\" integer,",
            "    \"amount\" numeric(12,2) NOT NULL DEFAULT 0,",
            "    CONSTRAINT \"orders_pkey\" PRIMARY KEY (id),",
            "    CONSTRAINT \"orders_customer_id_fkey\" FOREIGN KEY (customer_id) REFERENCES customers(id)",
            ");",
            "CREATE INDEX orders_customer_idx ON public.orders USING btree (customer_id) WHERE (amount > (0)::numeric);",
            "COMMENT ON TABLE \"public\".\"orders\" IS 'it''s orders';",
            "COMMENT ON COLUMN \"public\".\"orders\".\"customer_id\" IS 'FK to customers';",
            "",
        ]
        .join("\n");
        assert_eq!(ddl, expected);
    }

    #[test]
    fn ddl_variants_for_views_partitions_and_foreign_tables() {
        let view = RelationHead { schema: "s".into(), name: "v".into(), kind: TableKind::View, view_def: Some(" SELECT 1;".into()), ..Default::default() };
        assert_eq!(build_table_ddl(&view, &[], &[], &[]), "CREATE OR REPLACE VIEW \"s\".\"v\" AS\n SELECT 1;\n");

        let mv = RelationHead { schema: "s".into(), name: "mv".into(), kind: TableKind::MaterializedView, view_def: Some("SELECT 1".into()), ..Default::default() };
        let idx = IndexInfo { name: "mv_idx".into(), definition: Some("CREATE INDEX mv_idx ON s.mv USING btree (x);".into()), ..Default::default() };
        let ddl = build_table_ddl(&mv, &[], &[], &[idx]);
        assert!(ddl.starts_with("CREATE MATERIALIZED VIEW \"s\".\"mv\" AS\nSELECT 1;\n"));
        assert!(ddl.ends_with("CREATE INDEX mv_idx ON s.mv USING btree (x);\n"));

        let part = RelationHead {
            schema: "s".into(),
            name: "events".into(),
            kind: TableKind::Partitioned,
            partition_key: Some("RANGE (created_at)".into()),
            ..Default::default()
        };
        let ddl = build_table_ddl(&part, &[col(1, "created_at", "timestamp without time zone", false, None, None)], &[], &[]);
        assert_eq!(ddl, "CREATE TABLE \"s\".\"events\" (\n    \"created_at\" timestamp without time zone NOT NULL\n) PARTITION BY RANGE (created_at);\n");

        let foreign = RelationHead { schema: "s".into(), name: "remote".into(), kind: TableKind::Foreign, foreign_server: Some("srv".into()), ..Default::default() };
        let ddl = build_table_ddl(&foreign, &[col(1, "id", "integer", true, None, None)], &[], &[]);
        assert_eq!(ddl, "CREATE FOREIGN TABLE \"s\".\"remote\" (\n    \"id\" integer\n) SERVER \"srv\";\n");
    }
}
