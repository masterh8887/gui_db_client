//! `Catalog`: metadata browser. Never returns table data. Row counts are statistics-based
//! estimates unless the user explicitly asks for `exact_count`. Implementations must fetch a
//! whole schema in one query (no N+1): a 1,000-table schema lists in under two seconds.

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use crate::error::Result;
use crate::types::{ColumnInfo, DatabaseInfo, IndexInfo, SchemaInfo, TableInfo, TableRef};

#[cfg(feature = "mssql")]
pub mod mssql;
#[cfg(feature = "mysql")]
pub mod mysql;
#[cfg(feature = "postgres")]
pub mod postgres;
pub mod sqlite;

#[async_trait]
pub trait Catalog: Send + Sync {
    /// Databases visible to the account, with size where cheap.
    async fn databases(&self) -> Result<Vec<DatabaseInfo>>;

    /// Schemas of `db` (MySQL: databases are schemas; SQLite: `main`).
    async fn schemas(&self, db: &str) -> Result<Vec<SchemaInfo>>;

    /// Tables and views of a schema with estimated rows and sizes, in one round trip.
    async fn tables(&self, db: &str, schema: &str) -> Result<Vec<TableInfo>>;

    async fn columns(&self, table: &TableRef) -> Result<Vec<ColumnInfo>>;

    async fn indexes(&self, table: &TableRef) -> Result<Vec<IndexInfo>>;

    /// `CREATE TABLE` + `CREATE INDEX` statements, reconstructed where the DB has no helper.
    async fn ddl(&self, table: &TableRef) -> Result<String>;

    /// `SELECT count(*)`, cancellable. Only on explicit user request.
    async fn exact_count(&self, table: &TableRef, cancel: CancellationToken) -> Result<u64>;

    /// `SELECT "col1", "col2", ... FROM schema.table` with proper quoting for this dialect.
    fn select_all_sql(&self, table: &TableRef, columns: &[ColumnInfo]) -> String {
        let cols = if columns.is_empty() { "*".to_string() } else { columns.iter().map(|c| self.quote_ident(&c.name)).collect::<Vec<_>>().join(",\n       ") };
        format!("SELECT {cols}\nFROM {}", self.quote_table(table))
    }

    fn quote_ident(&self, ident: &str) -> String {
        format!("\"{}\"", ident.replace('"', "\"\""))
    }

    fn quote_table(&self, table: &TableRef) -> String {
        match &table.schema {
            Some(s) if !s.is_empty() => format!("{}.{}", self.quote_ident(s), self.quote_ident(&table.name)),
            _ => self.quote_ident(&table.name),
        }
    }
}
