//! Catalog implementation for `MssqlDatabase` (see catalog/mod.rs for the contract).
//!
//! Everything comes from the `sys.*` catalog views of the connection's database, prefixed with
//! `[db].` when another database is asked for. Sizes and row estimates come from
//! `sys.dm_db_partition_stats`; when the login lacks `VIEW DATABASE STATE` the listing falls
//! back to `sys.partitions.rows` (no sizes) instead of failing. `sql_variant` columns are
//! always `CAST` in SQL because tiberius cannot decode them.
//!
//! Cancellation of `exact_count` drops the TCP connection (tiberius exposes no attention
//! packet); the server aborts the count and the next catalog call reconnects transparently.

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use super::Catalog;
use crate::error::{CoreError, Result};
use crate::source::mssql::{get_bool, get_i64, get_string, get_u64, is_permission_error, query_rows, quote_ident, MssqlDatabase, TdsClient};
use crate::types::{ColumnInfo, DatabaseInfo, IndexInfo, SchemaInfo, TableInfo, TableKind, TableRef};

/// A column plus the extras DDL reconstruction needs.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MssqlColumn {
    pub info: ColumnInfo,
    /// `IDENTITY(seed, increment)`.
    pub identity: Option<(i64, i64)>,
    /// Computed column expression (`AS (...)`).
    pub computed: Option<String>,
    pub collation: Option<String>,
    /// `true` for alias types (`CREATE TYPE ... FROM`), rendered by name.
    pub user_defined: bool,
}

/// An index plus the extras DDL reconstruction needs.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MssqlIndex {
    pub info: IndexInfo,
    /// `sys.indexes.type_desc` verbatim (`CLUSTERED`, `NONCLUSTERED`, `CLUSTERED COLUMNSTORE`, ...).
    pub type_desc: String,
    /// One flag per key column: `true` = `DESC`.
    pub descending: Vec<bool>,
    /// Backed by a `UNIQUE` constraint (rendered as `ALTER TABLE ... ADD CONSTRAINT`).
    pub unique_constraint: bool,
    pub disabled: bool,
}

const FIXED_ROLE_SCHEMAS: &str =
    "'sys', 'INFORMATION_SCHEMA', 'db_owner', 'db_accessadmin', 'db_securityadmin', 'db_ddladmin', 'db_backupoperator', 'db_datareader', 'db_datawriter', 'db_denydatareader', 'db_denydatawriter'";

// ---------------------------------------------------------------------------------------------
// Type / DDL rendering (pure, unit-tested)
// ---------------------------------------------------------------------------------------------

/// Render a `sys.types` name with its length / precision / scale the way T-SQL spells it.
pub fn render_type(name: &str, max_length: i64, precision: i64, scale: i64) -> String {
    let lower = name.to_ascii_lowercase();
    match lower.as_str() {
        "varchar" | "char" | "varbinary" | "binary" => {
            if max_length < 0 {
                format!("{lower}(max)")
            } else {
                format!("{lower}({max_length})")
            }
        }
        "nvarchar" | "nchar" => {
            if max_length < 0 {
                format!("{lower}(max)")
            } else {
                format!("{lower}({})", max_length / 2)
            }
        }
        "decimal" | "numeric" => format!("{lower}({precision},{scale})"),
        "datetime2" | "time" | "datetimeoffset" => format!("{lower}({scale})"),
        "float" => {
            if precision == 53 || precision == 0 {
                "float".into()
            } else {
                format!("float({precision})")
            }
        }
        _ => lower,
    }
}

/// `[col] type [IDENTITY(s,i)] [NOT] NULL [DEFAULT (...)]` or `[col] AS (...)`.
pub fn render_column_ddl(c: &MssqlColumn) -> String {
    let name = quote_ident(&c.info.name);
    if let Some(expr) = &c.computed {
        return format!("{name} AS {expr}");
    }
    let mut s = format!("{name} {}", c.info.data_type);
    if let Some(coll) = &c.collation {
        if !coll.is_empty() {
            s.push_str(&format!(" COLLATE {coll}"));
        }
    }
    if let Some((seed, inc)) = c.identity {
        s.push_str(&format!(" IDENTITY({seed},{inc})"));
    }
    s.push_str(if c.info.nullable { " NULL" } else { " NOT NULL" });
    if let Some(d) = &c.info.default {
        s.push_str(&format!(" DEFAULT {d}"));
    }
    s
}

fn key_list(idx: &MssqlIndex) -> String {
    idx.info
        .columns
        .iter()
        .enumerate()
        .map(|(i, c)| format!("{} {}", quote_ident(c), if idx.descending.get(i).copied().unwrap_or(false) { "DESC" } else { "ASC" }))
        .collect::<Vec<_>>()
        .join(", ")
}

fn clustering(idx: &MssqlIndex) -> &'static str {
    if idx.type_desc.eq_ignore_ascii_case("CLUSTERED") {
        "CLUSTERED"
    } else {
        "NONCLUSTERED"
    }
}

/// `CREATE [UNIQUE] [CLUSTERED|NONCLUSTERED] INDEX ... ON ... (...) INCLUDE (...) WHERE ...`,
/// or the `ALTER TABLE ... ADD CONSTRAINT` form for primary keys and unique constraints.
pub fn render_index_ddl(table: &str, idx: &MssqlIndex) -> String {
    let name = quote_ident(&idx.info.name);
    if idx.info.primary {
        return format!("ALTER TABLE {table} ADD CONSTRAINT {name} PRIMARY KEY {} ({})", clustering(idx), key_list(idx));
    }
    if idx.unique_constraint {
        return format!("ALTER TABLE {table} ADD CONSTRAINT {name} UNIQUE {} ({})", clustering(idx), key_list(idx));
    }
    let td = idx.type_desc.to_ascii_uppercase();
    let cols = || idx.info.columns.iter().map(|c| quote_ident(c)).collect::<Vec<_>>().join(", ");
    let mut s = match td.as_str() {
        "CLUSTERED COLUMNSTORE" => format!("CREATE CLUSTERED COLUMNSTORE INDEX {name} ON {table}"),
        "NONCLUSTERED COLUMNSTORE" => format!("CREATE NONCLUSTERED COLUMNSTORE INDEX {name} ON {table} ({})", cols()),
        "XML" => format!("CREATE XML INDEX {name} ON {table} ({})", cols()),
        "SPATIAL" => format!("CREATE SPATIAL INDEX {name} ON {table} ({})", cols()),
        "NONCLUSTERED HASH" => format!("CREATE {}NONCLUSTERED HASH INDEX {name} ON {table} ({})", if idx.info.unique { "UNIQUE " } else { "" }, cols()),
        _ => format!("CREATE {}{} INDEX {name} ON {table} ({})", if idx.info.unique { "UNIQUE " } else { "" }, clustering(idx), key_list(idx)),
    };
    if !idx.info.include.is_empty() {
        s.push_str(&format!(" INCLUDE ({})", idx.info.include.iter().map(|c| quote_ident(c)).collect::<Vec<_>>().join(", ")));
    }
    if let Some(p) = idx.info.predicate.as_deref().filter(|p| !p.trim().is_empty()) {
        s.push_str(&format!(" WHERE {p}"));
    }
    s
}

/// Full `CREATE TABLE` (with the primary key inline) followed by one statement per index.
pub fn render_table_ddl(table: &str, columns: &[MssqlColumn], indexes: &[MssqlIndex]) -> String {
    let mut body: Vec<String> = columns.iter().map(|c| format!("    {}", render_column_ddl(c))).collect();
    if let Some(pk) = indexes.iter().find(|i| i.info.primary) {
        body.push(format!("    CONSTRAINT {} PRIMARY KEY {} ({})", quote_ident(&pk.info.name), clustering(pk), key_list(pk)));
    }
    let mut out = format!("CREATE TABLE {table} (\n{}\n);", body.join(",\n"));
    for idx in indexes.iter().filter(|i| !i.info.primary) {
        out.push('\n');
        out.push_str(&render_index_ddl(table, idx));
        out.push(';');
    }
    out
}

// ---------------------------------------------------------------------------------------------
// Queries
// ---------------------------------------------------------------------------------------------

impl MssqlDatabase {
    fn effective_db<'a>(&'a self, db: Option<&'a str>) -> &'a str {
        match db {
            Some(d) if !d.is_empty() => d,
            _ => &self.current_db,
        }
    }

    /// Columns with the extras needed for DDL (one query).
    pub async fn fetch_columns(&self, table: &TableRef) -> Result<Vec<MssqlColumn>> {
        let p = self.db_prefix(table.database.as_deref());
        let schema = table.schema.as_deref().filter(|s| !s.is_empty()).unwrap_or("dbo").to_string();
        let sql = format!(
            "SELECT c.column_id, c.name, t.name AS type_name, c.max_length, c.precision, c.scale, c.is_nullable, \
                    dc.definition AS default_def, CAST(ep.value AS nvarchar(4000)) AS comment, \
                    CASE WHEN EXISTS (SELECT 1 FROM {p}sys.key_constraints kc JOIN {p}sys.index_columns ic ON ic.object_id = kc.parent_object_id AND ic.index_id = kc.unique_index_id \
                                      WHERE kc.parent_object_id = c.object_id AND kc.type = 'PK' AND ic.column_id = c.column_id) THEN 1 ELSE 0 END AS is_pk, \
                    CASE WHEN EXISTS (SELECT 1 FROM {p}sys.key_constraints kc JOIN {p}sys.index_columns ic ON ic.object_id = kc.parent_object_id AND ic.index_id = kc.unique_index_id \
                                      WHERE kc.parent_object_id = c.object_id AND kc.type = 'UQ' AND ic.column_id = c.column_id \
                                        AND (SELECT COUNT(*) FROM {p}sys.index_columns ic2 WHERE ic2.object_id = kc.parent_object_id AND ic2.index_id = kc.unique_index_id) = 1) THEN 1 ELSE 0 END AS is_unique, \
                    fk.target AS fk_target, c.is_identity, CAST(idc.seed_value AS bigint) AS seed, CAST(idc.increment_value AS bigint) AS increment, \
                    cc.definition AS computed_def, c.collation_name, t.is_user_defined \
             FROM {p}sys.columns c \
             JOIN {p}sys.objects o ON o.object_id = c.object_id \
             JOIN {p}sys.schemas s ON s.schema_id = o.schema_id \
             JOIN {p}sys.types t ON t.user_type_id = c.user_type_id \
             LEFT JOIN {p}sys.default_constraints dc ON dc.parent_object_id = c.object_id AND dc.parent_column_id = c.column_id \
             LEFT JOIN {p}sys.extended_properties ep ON ep.class = 1 AND ep.major_id = c.object_id AND ep.minor_id = c.column_id AND ep.name = 'MS_Description' \
             LEFT JOIN {p}sys.identity_columns idc ON idc.object_id = c.object_id AND idc.column_id = c.column_id \
             LEFT JOIN {p}sys.computed_columns cc ON cc.object_id = c.object_id AND cc.column_id = c.column_id \
             OUTER APPLY (SELECT TOP 1 rs.name + '.' + ro.name + '(' + rc.name + ')' AS target \
                          FROM {p}sys.foreign_key_columns fkc \
                          JOIN {p}sys.objects ro ON ro.object_id = fkc.referenced_object_id \
                          JOIN {p}sys.schemas rs ON rs.schema_id = ro.schema_id \
                          JOIN {p}sys.columns rc ON rc.object_id = fkc.referenced_object_id AND rc.column_id = fkc.referenced_column_id \
                          WHERE fkc.parent_object_id = c.object_id AND fkc.parent_column_id = c.column_id \
                          ORDER BY fkc.constraint_object_id) fk \
             WHERE s.name = @P1 AND o.name = @P2 \
             ORDER BY c.column_id"
        );
        let mut guard = self.lock().await;
        let client = self.ensure_client(&mut guard).await?;
        let rows = query_rows(client, &sql, &[&schema, &table.name]).await?;
        let mut out = Vec::with_capacity(rows.len());
        for r in &rows {
            let type_name = get_string(r, 2).unwrap_or_default();
            let user_defined = get_bool(r, 17);
            let data_type = if user_defined {
                type_name.clone()
            } else {
                render_type(&type_name, get_i64(r, 3).unwrap_or(0), get_i64(r, 4).unwrap_or(0), get_i64(r, 5).unwrap_or(0))
            };
            let identity = if get_bool(r, 12) { Some((get_i64(r, 13).unwrap_or(1), get_i64(r, 14).unwrap_or(1))) } else { None };
            out.push(MssqlColumn {
                info: ColumnInfo {
                    position: get_i64(r, 0).unwrap_or(0) as u32,
                    name: get_string(r, 1).unwrap_or_default(),
                    data_type,
                    nullable: get_bool(r, 6),
                    default: get_string(r, 7),
                    primary_key: get_bool(r, 9),
                    foreign_key: get_string(r, 11),
                    unique: get_bool(r, 10),
                    comment: get_string(r, 8),
                },
                identity,
                computed: get_string(r, 15),
                collation: get_string(r, 16),
                user_defined,
            });
        }
        if out.is_empty() {
            // Distinguish "no such table" from "table without columns" (impossible).
            let exists = query_rows(
                client,
                &format!("SELECT 1 FROM {p}sys.objects o JOIN {p}sys.schemas s ON s.schema_id = o.schema_id WHERE s.name = @P1 AND o.name = @P2"),
                &[&schema, &table.name],
            )
            .await?;
            if exists.is_empty() {
                return Err(CoreError::NotFound(format!("table {}", self.qualified(table))));
            }
        }
        Ok(out)
    }

    /// Indexes with the extras needed for DDL (two queries: indexes, then their columns).
    pub async fn fetch_indexes(&self, table: &TableRef) -> Result<Vec<MssqlIndex>> {
        let p = self.db_prefix(table.database.as_deref());
        let schema = table.schema.as_deref().filter(|s| !s.is_empty()).unwrap_or("dbo").to_string();
        let db = self.effective_db(table.database.as_deref()).to_string();
        let with_stats = |stats: bool| {
            let ps_join = if stats {
                format!(
                    "LEFT JOIN (SELECT object_id, index_id, SUM(reserved_page_count) * 8192 AS bytes FROM {p}sys.dm_db_partition_stats GROUP BY object_id, index_id) ps \
                       ON ps.object_id = i.object_id AND ps.index_id = i.index_id \
                     LEFT JOIN (SELECT object_id, index_id, user_seeks + user_scans + user_lookups AS scans FROM sys.dm_db_index_usage_stats WHERE database_id = DB_ID(@P3)) us \
                       ON us.object_id = i.object_id AND us.index_id = i.index_id"
                )
            } else {
                String::new()
            };
            let (bytes, scans) = if stats { ("ps.bytes", "us.scans") } else { ("CAST(NULL AS bigint)", "CAST(NULL AS bigint)") };
            format!(
                "SELECT i.index_id, i.name, i.type_desc, i.is_unique, i.is_primary_key, i.is_unique_constraint, i.filter_definition, i.is_disabled, {bytes} AS bytes, {scans} AS scans \
                 FROM {p}sys.indexes i \
                 JOIN {p}sys.objects o ON o.object_id = i.object_id \
                 JOIN {p}sys.schemas s ON s.schema_id = o.schema_id \
                 {ps_join} \
                 WHERE s.name = @P1 AND o.name = @P2 AND i.index_id > 0 AND i.name IS NOT NULL \
                 ORDER BY i.index_id"
            )
        };
        let cols_sql = format!(
            "SELECT ic.index_id, c.name, ic.is_included_column, ic.key_ordinal, ic.is_descending_key \
             FROM {p}sys.index_columns ic \
             JOIN {p}sys.columns c ON c.object_id = ic.object_id AND c.column_id = ic.column_id \
             JOIN {p}sys.objects o ON o.object_id = ic.object_id \
             JOIN {p}sys.schemas s ON s.schema_id = o.schema_id \
             WHERE s.name = @P1 AND o.name = @P2 AND ic.index_id > 0 \
             ORDER BY ic.index_id, ic.is_included_column, ic.key_ordinal, ic.index_column_id"
        );
        let mut guard = self.lock().await;
        let client = self.ensure_client(&mut guard).await?;
        let rows = match query_rows(client, &with_stats(true), &[&schema, &table.name, &db]).await {
            Ok(rows) => rows,
            Err(e) if is_permission_error(&e) => {
                tracing::debug!(target: "bulkdb::mssql", "index statistics unavailable ({e}); listing without sizes");
                query_rows(client, &with_stats(false), &[&schema, &table.name]).await?
            }
            Err(e) => return Err(e),
        };
        let mut out: Vec<MssqlIndex> = Vec::with_capacity(rows.len());
        let mut ids: Vec<i64> = Vec::with_capacity(rows.len());
        for r in &rows {
            ids.push(get_i64(r, 0).unwrap_or(0));
            out.push(MssqlIndex {
                info: IndexInfo {
                    name: get_string(r, 1).unwrap_or_default(),
                    columns: Vec::new(),
                    unique: get_bool(r, 3),
                    primary: get_bool(r, 4),
                    kind: get_string(r, 2).map(|k| k.to_ascii_lowercase()),
                    predicate: get_string(r, 6),
                    include: Vec::new(),
                    size_bytes: get_u64(r, 8),
                    scans: get_u64(r, 9),
                    definition: None,
                },
                type_desc: get_string(r, 2).unwrap_or_default(),
                descending: Vec::new(),
                unique_constraint: get_bool(r, 5),
                disabled: get_bool(r, 7),
            });
        }
        if out.is_empty() {
            return Ok(out);
        }
        let col_rows = query_rows(client, &cols_sql, &[&schema, &table.name]).await?;
        for r in &col_rows {
            let index_id = get_i64(r, 0).unwrap_or(0);
            let Some(pos) = ids.iter().position(|id| *id == index_id) else { continue };
            let name = get_string(r, 1).unwrap_or_default();
            let idx = &mut out[pos];
            if get_bool(r, 2) {
                idx.info.include.push(name);
            } else {
                idx.info.columns.push(name);
                idx.descending.push(get_bool(r, 4));
            }
        }
        let qualified = self.qualified(table);
        for idx in &mut out {
            idx.info.definition = Some(render_index_ddl(&qualified, idx));
        }
        Ok(out)
    }

    async fn tables_query(&self, client: &mut TdsClient, p: &str, schema: &str, stats: bool) -> Result<Vec<TableInfo>> {
        let ps = if stats {
            format!(
                "SELECT object_id, \
                        SUM(CASE WHEN index_id IN (0, 1) THEN row_count ELSE 0 END) AS rows_est, \
                        SUM(CASE WHEN index_id IN (0, 1) THEN reserved_page_count ELSE 0 END) * 8192 AS table_bytes, \
                        SUM(CASE WHEN index_id > 1 THEN reserved_page_count ELSE 0 END) * 8192 AS index_bytes \
                 FROM {p}sys.dm_db_partition_stats GROUP BY object_id"
            )
        } else {
            format!(
                "SELECT object_id, SUM(CASE WHEN index_id IN (0, 1) THEN rows ELSE 0 END) AS rows_est, \
                        CAST(NULL AS bigint) AS table_bytes, CAST(NULL AS bigint) AS index_bytes \
                 FROM {p}sys.partitions GROUP BY object_id"
            )
        };
        let sql = format!(
            "SELECT o.name, o.kind, o.is_partitioned, ps.rows_est, ps.table_bytes, ps.index_bytes, CAST(ep.value AS nvarchar(4000)) AS comment \
             FROM ( \
                 SELECT t.object_id, t.name, 'T' AS kind, \
                        CASE WHEN EXISTS (SELECT 1 FROM {p}sys.partitions pt WHERE pt.object_id = t.object_id AND pt.partition_number > 1) THEN 1 ELSE 0 END AS is_partitioned \
                 FROM {p}sys.tables t JOIN {p}sys.schemas s ON s.schema_id = t.schema_id \
                 WHERE s.name = @P1 AND t.is_ms_shipped = 0 \
                 UNION ALL \
                 SELECT v.object_id, v.name, 'V', 0 \
                 FROM {p}sys.views v JOIN {p}sys.schemas s ON s.schema_id = v.schema_id \
                 WHERE s.name = @P1 AND v.is_ms_shipped = 0 \
             ) o \
             LEFT JOIN ({ps}) ps ON ps.object_id = o.object_id \
             LEFT JOIN {p}sys.extended_properties ep ON ep.class = 1 AND ep.major_id = o.object_id AND ep.minor_id = 0 AND ep.name = 'MS_Description' \
             ORDER BY o.name"
        );
        let rows = query_rows(client, &sql, &[&schema]).await?;
        Ok(rows
            .iter()
            .map(|r| {
                let is_view = get_string(r, 1).as_deref() == Some("V");
                let kind = if is_view {
                    TableKind::View
                } else if get_bool(r, 2) {
                    TableKind::Partitioned
                } else {
                    TableKind::Table
                };
                let table_bytes = get_u64(r, 4);
                let index_bytes = get_u64(r, 5);
                let total_bytes = match (table_bytes, index_bytes) {
                    (None, None) => None,
                    (a, b) => Some(a.unwrap_or(0) + b.unwrap_or(0)),
                };
                TableInfo {
                    schema: schema.to_string(),
                    name: get_string(r, 0).unwrap_or_default(),
                    kind,
                    estimated_rows: if is_view { None } else { get_u64(r, 3) },
                    table_bytes,
                    index_bytes,
                    total_bytes,
                    comment: get_string(r, 6),
                }
            })
            .collect())
    }

    /// View definition from `sys.sql_modules`, or `None` when the object is not a view.
    async fn view_definition(&self, client: &mut TdsClient, table: &TableRef) -> Result<Option<String>> {
        let p = self.db_prefix(table.database.as_deref());
        let schema = table.schema.as_deref().filter(|s| !s.is_empty()).unwrap_or("dbo").to_string();
        let sql = format!(
            "SELECT o.type, m.definition FROM {p}sys.objects o JOIN {p}sys.schemas s ON s.schema_id = o.schema_id \
             LEFT JOIN {p}sys.sql_modules m ON m.object_id = o.object_id WHERE s.name = @P1 AND o.name = @P2"
        );
        let rows = query_rows(client, &sql, &[&schema, &table.name]).await?;
        let Some(row) = rows.first() else { return Err(CoreError::NotFound(format!("table {}", self.qualified(table)))) };
        if get_string(row, 0).map(|t| t.trim() == "V").unwrap_or(false) {
            return Ok(Some(get_string(row, 1).unwrap_or_default()));
        }
        Ok(None)
    }
}

#[async_trait]
impl Catalog for MssqlDatabase {
    async fn databases(&self) -> Result<Vec<DatabaseInfo>> {
        let mut guard = self.lock().await;
        let client = self.ensure_client(&mut guard).await?;
        let with_sizes = "SELECT d.name, d.state_desc, d.collation_name, f.size_bytes, CASE WHEN d.name = DB_NAME() THEN 1 ELSE 0 END AS is_current, HAS_DBACCESS(d.name) AS has_access \
                          FROM sys.databases d \
                          LEFT JOIN (SELECT database_id, SUM(CAST(size AS bigint)) * 8192 AS size_bytes FROM sys.master_files GROUP BY database_id) f ON f.database_id = d.database_id \
                          ORDER BY d.name";
        let without = "SELECT d.name, d.state_desc, d.collation_name, CAST(NULL AS bigint) AS size_bytes, CASE WHEN d.name = DB_NAME() THEN 1 ELSE 0 END AS is_current, HAS_DBACCESS(d.name) AS has_access \
                       FROM sys.databases d ORDER BY d.name";
        let rows = match query_rows(client, with_sizes, &[]).await {
            Ok(rows) => rows,
            Err(e) if is_permission_error(&e) => {
                tracing::debug!(target: "bulkdb::mssql", "sys.master_files unavailable ({e}); listing databases without sizes");
                query_rows(client, without, &[]).await?
            }
            Err(e) => return Err(e),
        };
        Ok(rows
            .iter()
            .map(|r| {
                let online = get_string(r, 1).map(|s| s.eq_ignore_ascii_case("ONLINE")).unwrap_or(false);
                let has_access = get_i64(r, 5).map(|v| v != 0).unwrap_or(true);
                DatabaseInfo {
                    name: get_string(r, 0).unwrap_or_default(),
                    size_bytes: get_u64(r, 3),
                    encoding: None,
                    collation: get_string(r, 2),
                    accessible: online && has_access,
                    is_current: get_bool(r, 4),
                }
            })
            .collect())
    }

    async fn schemas(&self, db: &str) -> Result<Vec<SchemaInfo>> {
        let p = self.db_prefix(Some(db));
        let sql = format!(
            "SELECT s.name, pr.name AS owner, \
                    (SELECT COUNT_BIG(*) FROM {p}sys.tables t WHERE t.schema_id = s.schema_id AND t.is_ms_shipped = 0) \
                  + (SELECT COUNT_BIG(*) FROM {p}sys.views v WHERE v.schema_id = s.schema_id AND v.is_ms_shipped = 0) AS table_count \
             FROM {p}sys.schemas s \
             LEFT JOIN {p}sys.database_principals pr ON pr.principal_id = s.principal_id \
             WHERE s.name NOT IN ({FIXED_ROLE_SCHEMAS}) \
             ORDER BY s.name"
        );
        let mut guard = self.lock().await;
        let client = self.ensure_client(&mut guard).await?;
        let rows = query_rows(client, &sql, &[]).await?;
        Ok(rows.iter().map(|r| SchemaInfo { name: get_string(r, 0).unwrap_or_default(), owner: get_string(r, 1), table_count: get_u64(r, 2) }).collect())
    }

    async fn tables(&self, db: &str, schema: &str) -> Result<Vec<TableInfo>> {
        let p = self.db_prefix(Some(db));
        let mut guard = self.lock().await;
        let client = self.ensure_client(&mut guard).await?;
        match self.tables_query(client, &p, schema, true).await {
            Ok(t) => Ok(t),
            Err(e) if is_permission_error(&e) => {
                tracing::debug!(target: "bulkdb::mssql", "sys.dm_db_partition_stats unavailable ({e}); listing tables without sizes");
                self.tables_query(client, &p, schema, false).await
            }
            Err(e) => Err(e),
        }
    }

    async fn columns(&self, table: &TableRef) -> Result<Vec<ColumnInfo>> {
        Ok(self.fetch_columns(table).await?.into_iter().map(|c| c.info).collect())
    }

    async fn indexes(&self, table: &TableRef) -> Result<Vec<IndexInfo>> {
        Ok(self.fetch_indexes(table).await?.into_iter().map(|i| i.info).collect())
    }

    async fn ddl(&self, table: &TableRef) -> Result<String> {
        {
            let mut guard = self.lock().await;
            let client = self.ensure_client(&mut guard).await?;
            if let Some(def) = self.view_definition(client, table).await? {
                return Ok(def);
            }
        }
        let columns = self.fetch_columns(table).await?;
        let indexes = self.fetch_indexes(table).await?;
        Ok(render_table_ddl(&self.qualified(table), &columns, &indexes))
    }

    /// `SELECT COUNT_BIG(*)`. Cancelling drops the connection (the server aborts the scan);
    /// the next call on this object reconnects.
    async fn exact_count(&self, table: &TableRef, cancel: CancellationToken) -> Result<u64> {
        let sql = format!("SELECT COUNT_BIG(*) FROM {}", self.qualified(table));
        let mut guard = self.lock().await;
        self.ensure_client(&mut guard).await?;
        let mut client = guard.take().expect("ensured above");
        let count = async {
            let rows = query_rows(&mut client, &sql, &[]).await?;
            rows.first().and_then(|r| get_u64(r, 0)).ok_or_else(|| CoreError::Internal("COUNT_BIG returned no row".into()))
        };
        let result = tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(CoreError::Cancelled),
            r = count => r,
        };
        if matches!(result, Err(CoreError::Cancelled)) {
            drop(client);
            self.drop_connection(&mut guard);
        } else {
            *guard = Some(client);
        }
        result
    }

    fn quote_ident(&self, ident: &str) -> String {
        quote_ident(ident)
    }

    fn quote_table(&self, table: &TableRef) -> String {
        self.qualified(table)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_types() {
        assert_eq!(render_type("varchar", 50, 0, 0), "varchar(50)");
        assert_eq!(render_type("nvarchar", -1, 0, 0), "nvarchar(max)");
        assert_eq!(render_type("nvarchar", 100, 0, 0), "nvarchar(50)");
        assert_eq!(render_type("nchar", 20, 0, 0), "nchar(10)");
        assert_eq!(render_type("decimal", 9, 12, 2), "decimal(12,2)");
        assert_eq!(render_type("numeric", 17, 38, 10), "numeric(38,10)");
        assert_eq!(render_type("datetime2", 8, 27, 7), "datetime2(7)");
        assert_eq!(render_type("time", 5, 16, 3), "time(3)");
        assert_eq!(render_type("varbinary", -1, 0, 0), "varbinary(max)");
        assert_eq!(render_type("binary", 16, 0, 0), "binary(16)");
        assert_eq!(render_type("float", 8, 53, 0), "float");
        assert_eq!(render_type("real", 4, 24, 0), "real");
        assert_eq!(render_type("int", 4, 10, 0), "int");
        assert_eq!(render_type("uniqueidentifier", 16, 0, 0), "uniqueidentifier");
    }

    fn col(name: &str, ty: &str, nullable: bool) -> MssqlColumn {
        MssqlColumn { info: ColumnInfo { name: name.into(), data_type: ty.into(), nullable, ..Default::default() }, ..Default::default() }
    }

    #[test]
    fn renders_table_and_index_ddl() {
        let mut id = col("id", "int", false);
        id.identity = Some((1, 1));
        id.info.primary_key = true;
        let mut name = col("name", "nvarchar(50)", false);
        name.info.default = Some("('n/a')".into());
        let mut total = col("total", "money", true);
        total.computed = Some("([qty]*[price])".into());
        let pk = MssqlIndex {
            info: IndexInfo { name: "PK_t".into(), columns: vec!["id".into()], unique: true, primary: true, kind: Some("clustered".into()), ..Default::default() },
            type_desc: "CLUSTERED".into(),
            descending: vec![false],
            ..Default::default()
        };
        let ix = MssqlIndex {
            info: IndexInfo {
                name: "ix_name".into(),
                columns: vec!["name".into(), "id".into()],
                unique: true,
                include: vec!["total".into()],
                predicate: Some("([name] IS NOT NULL)".into()),
                kind: Some("nonclustered".into()),
                ..Default::default()
            },
            type_desc: "NONCLUSTERED".into(),
            descending: vec![false, true],
            ..Default::default()
        };
        let ddl = render_table_ddl("[dbo].[t]", &[id, name, total], &[pk.clone(), ix.clone()]);
        let expected = "CREATE TABLE [dbo].[t] (\n    [id] int IDENTITY(1,1) NOT NULL,\n    [name] nvarchar(50) NOT NULL DEFAULT ('n/a'),\n    [total] AS ([qty]*[price]),\n    CONSTRAINT [PK_t] PRIMARY KEY CLUSTERED ([id] ASC)\n);\nCREATE UNIQUE NONCLUSTERED INDEX [ix_name] ON [dbo].[t] ([name] ASC, [id] DESC) INCLUDE ([total]) WHERE ([name] IS NOT NULL);";
        assert_eq!(ddl, expected);
        assert_eq!(render_index_ddl("[dbo].[t]", &pk), "ALTER TABLE [dbo].[t] ADD CONSTRAINT [PK_t] PRIMARY KEY CLUSTERED ([id] ASC)");
        let uq = MssqlIndex {
            unique_constraint: true,
            type_desc: "NONCLUSTERED".into(),
            info: IndexInfo { name: "UQ_x".into(), columns: vec!["a".into()], unique: true, ..Default::default() },
            ..Default::default()
        };
        assert_eq!(render_index_ddl("[s].[t]", &uq), "ALTER TABLE [s].[t] ADD CONSTRAINT [UQ_x] UNIQUE NONCLUSTERED ([a] ASC)");
        let cs = MssqlIndex { type_desc: "CLUSTERED COLUMNSTORE".into(), info: IndexInfo { name: "cci".into(), ..Default::default() }, ..Default::default() };
        assert_eq!(render_index_ddl("[s].[t]", &cs), "CREATE CLUSTERED COLUMNSTORE INDEX [cci] ON [s].[t]");
    }

    #[test]
    fn quoting_in_ddl_escapes_brackets() {
        let c = col("we]ird", "int", true);
        assert_eq!(render_column_ddl(&c), "[we]]ird] int NULL");
    }
}
