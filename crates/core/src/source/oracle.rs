//! Oracle driver (`oracle` crate, ODPI-C, Instant Client loaded at runtime). Tier 2.
//! Compiled with `--features oracle` (`has_oracle` cfg).
//!
//! * Streams rows with prefetch into `arrow_batch::ArrowBatcher`.
//! * Catalog: `ALL_TABLES` (`NUM_ROWS`), `ALL_TAB_COLUMNS`, `ALL_INDEXES`; monitor `v$session`.
//! * Cancel: `Connection::break_execution`.

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use super::{ConnectParams, Database, Query, Source, SourceStream};
use crate::catalog::Catalog;
use crate::error::{CoreError, Result};
use crate::monitor::{JobHandle, Monitor, ServerSnapshot};
use crate::types::{ColumnInfo, DatabaseInfo, IndexInfo, PlanSummary, SchemaInfo, ServerInfo, SessionInfo, SessionOpts, TableInfo, TableRef};

pub struct OracleDatabase {
    _params: ConnectParams,
}

impl OracleDatabase {
    pub async fn connect(params: &ConnectParams) -> Result<Self> {
        let _ = params;
        Err(CoreError::Internal("oracle driver not implemented yet".into()))
    }
}

#[async_trait]
impl Source for OracleDatabase {
    async fn probe(&self) -> Result<ServerInfo> {
        Err(CoreError::Internal("not implemented".into()))
    }
    async fn apply_session(&self, _opts: &SessionOpts) -> Result<()> {
        Ok(())
    }
    async fn stream(&self, _q: &Query, _opts: &SessionOpts, _cancel: CancellationToken) -> Result<SourceStream> {
        Err(CoreError::Internal("not implemented".into()))
    }
    async fn explain(&self, _sql: &str, _analyze: bool) -> Result<Option<PlanSummary>> {
        Ok(None)
    }
    fn session_id(&self) -> Option<String> {
        None
    }
    fn driver_id(&self) -> &'static str {
        "oracle"
    }
}

#[async_trait]
impl Catalog for OracleDatabase {
    async fn databases(&self) -> Result<Vec<DatabaseInfo>> {
        Err(CoreError::Internal("not implemented".into()))
    }
    async fn schemas(&self, _db: &str) -> Result<Vec<SchemaInfo>> {
        Err(CoreError::Internal("not implemented".into()))
    }
    async fn tables(&self, _db: &str, _schema: &str) -> Result<Vec<TableInfo>> {
        Err(CoreError::Internal("not implemented".into()))
    }
    async fn columns(&self, _table: &TableRef) -> Result<Vec<ColumnInfo>> {
        Err(CoreError::Internal("not implemented".into()))
    }
    async fn indexes(&self, _table: &TableRef) -> Result<Vec<IndexInfo>> {
        Ok(vec![])
    }
    async fn ddl(&self, _table: &TableRef) -> Result<String> {
        Err(CoreError::Internal("not implemented".into()))
    }
    async fn exact_count(&self, _table: &TableRef, _cancel: CancellationToken) -> Result<u64> {
        Err(CoreError::Internal("not implemented".into()))
    }
}

#[async_trait]
impl Monitor for OracleDatabase {
    async fn snapshot(&self, _job: &JobHandle) -> Result<ServerSnapshot> {
        Ok(ServerSnapshot { found: false, ..Default::default() })
    }
    async fn active_sessions(&self, _touching: &[TableRef]) -> Result<Vec<SessionInfo>> {
        Ok(vec![])
    }
}

impl Database for OracleDatabase {}
