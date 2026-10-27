//! Metadata cache for the catalog browser plus the pool of long-lived "browser" connections.
//!
//! * [`CatalogCache`] keys results by `(connection id, kind, arguments)` so the tree does not
//!   re-query the server when the user collapses and expands a node. `refresh = true` on the
//!   facade bypasses it; deleting a connection drops its entries.
//! * [`BrowserPool`] keeps one open connection per saved connection (`application_name
//!   "bulkdb-ui"`) for catalog and EXPLAIN calls; the facade evicts it when a call fails with a
//!   connection-level error and opens a fresh one.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::connect::LiveConnection;
use crate::types::{ColumnInfo, DatabaseInfo, IndexInfo, SchemaInfo, TableInfo, TableRef};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CatalogKind {
    Databases,
    Schemas,
    Tables,
    Columns,
    Indexes,
    Ddl,
}

#[derive(Debug, Clone)]
pub enum CatalogValue {
    Databases(Vec<DatabaseInfo>),
    Schemas(Vec<SchemaInfo>),
    Tables(Vec<TableInfo>),
    Columns(Vec<ColumnInfo>),
    Indexes(Vec<IndexInfo>),
    Ddl(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CacheKey {
    pub conn_id: String,
    pub kind: CatalogKind,
    pub args: Vec<String>,
}

impl CacheKey {
    pub fn new(conn_id: &str, kind: CatalogKind, args: Vec<String>) -> Self {
        CacheKey { conn_id: conn_id.to_string(), kind, args }
    }

    /// Key for a per-table lookup (`columns`, `indexes`, `ddl`).
    pub fn table(conn_id: &str, kind: CatalogKind, table: &TableRef) -> Self {
        Self::new(conn_id, kind, table_args(table))
    }
}

/// `[database, schema, name]` with empty strings for missing parts.
pub fn table_args(table: &TableRef) -> Vec<String> {
    vec![table.database.clone().unwrap_or_default(), table.schema.clone().unwrap_or_default(), table.name.clone()]
}

#[derive(Default)]
pub struct CatalogCache {
    entries: Mutex<HashMap<CacheKey, CatalogValue>>,
}

impl CatalogCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&self, key: &CacheKey) -> Option<CatalogValue> {
        self.entries.lock().unwrap().get(key).cloned()
    }

    pub fn put(&self, key: CacheKey, value: CatalogValue) {
        self.entries.lock().unwrap().insert(key, value);
    }

    /// Drop every entry of one connection (the tree's refresh button, `conn_save`, `conn_delete`).
    pub fn invalidate(&self, conn_id: &str) {
        self.entries.lock().unwrap().retain(|k, _| k.conn_id != conn_id);
    }

    pub fn clear(&self) {
        self.entries.lock().unwrap().clear();
    }

    pub fn len(&self) -> usize {
        self.entries.lock().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// One cached browser connection per saved connection.
#[derive(Default)]
pub struct BrowserPool {
    conns: Mutex<HashMap<String, Arc<LiveConnection>>>,
}

impl BrowserPool {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&self, conn_id: &str) -> Option<Arc<LiveConnection>> {
        self.conns.lock().unwrap().get(conn_id).cloned()
    }

    /// Store a connection; the previous one (if any) is returned so the caller drops it
    /// outside the lock.
    pub fn insert(&self, conn_id: &str, live: Arc<LiveConnection>) -> Option<Arc<LiveConnection>> {
        self.conns.lock().unwrap().insert(conn_id.to_string(), live)
    }

    pub fn evict(&self, conn_id: &str) -> Option<Arc<LiveConnection>> {
        self.conns.lock().unwrap().remove(conn_id)
    }

    /// Remove everything (shutdown). Returned handles are dropped by the caller.
    pub fn clear(&self) -> Vec<Arc<LiveConnection>> {
        self.conns.lock().unwrap().drain().map(|(_, v)| v).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_keys_and_invalidation() {
        let cache = CatalogCache::new();
        let t = TableRef::new(Some("public"), "orders");
        cache.put(CacheKey::new("c1", CatalogKind::Tables, vec!["db".into(), "public".into()]), CatalogValue::Tables(vec![]));
        cache.put(CacheKey::table("c1", CatalogKind::Columns, &t), CatalogValue::Columns(vec![]));
        cache.put(CacheKey::table("c2", CatalogKind::Ddl, &t), CatalogValue::Ddl("create".into()));
        assert_eq!(cache.len(), 3);
        assert!(matches!(cache.get(&CacheKey::table("c1", CatalogKind::Columns, &t)), Some(CatalogValue::Columns(_))));
        assert!(cache.get(&CacheKey::table("c1", CatalogKind::Columns, &TableRef::new(Some("other"), "orders"))).is_none());
        cache.invalidate("c1");
        assert_eq!(cache.len(), 1);
        assert!(matches!(cache.get(&CacheKey::table("c2", CatalogKind::Ddl, &t)), Some(CatalogValue::Ddl(s)) if s == "create"));
        cache.clear();
        assert!(cache.is_empty());
    }
}
