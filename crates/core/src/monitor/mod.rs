//! `Monitor`: server-side view of a running job, read over a separate light connection every
//! 3-5 seconds. Implementations live next to their driver (`monitor/postgres.rs`, ...).

use async_trait::async_trait;

use crate::error::Result;
use crate::types::{SessionInfo, TableRef};

#[cfg(feature = "mssql")]
pub mod mssql;
#[cfg(feature = "mysql")]
pub mod mysql;
#[cfg(feature = "postgres")]
pub mod postgres;

/// What a monitor needs to find the job's session.
#[derive(Debug, Clone)]
pub struct JobHandle {
    pub job_id: String,
    /// `bulkdb-<job id>`; the driver sets it as `application_name` / `program_name`.
    pub application_name: String,
    /// Backend pid / connection id when the source reported one.
    pub session_id: Option<String>,
    /// Temp usage of the session when the job started, so `temp_bytes` can be a delta.
    pub temp_bytes_at_start: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ServerSnapshot {
    pub pid: Option<String>,
    pub state: Option<String>,
    pub wait_event: Option<String>,
    pub elapsed_ms: Option<u64>,
    /// Rows the server reports having sent (PostgreSQL `pg_stat_progress_copy.tuples_processed`).
    pub server_rows: Option<u64>,
    /// Current temp bytes (absolute).
    pub temp_bytes: Option<u64>,
    /// Fraction of recent samples where the wait event was `ClientWrite` (0..1), when the
    /// driver can tell. The job runner turns > 0.5 into `Bottleneck::Network`.
    pub client_write_ratio: Option<f32>,
    /// Session was not found (finished, or application_name not visible).
    pub found: bool,
}

#[async_trait]
pub trait Monitor: Send + Sync {
    async fn snapshot(&self, job: &JobHandle) -> Result<ServerSnapshot>;

    /// Active sessions that touch any of `touching` (schema-qualified where known). Used by
    /// preflight ("another session is reading the same table") and by the EXPLAIN lock-wait
    /// explanation ("pid 123 holds a lock on orders").
    async fn active_sessions(&self, touching: &[TableRef]) -> Result<Vec<SessionInfo>>;

    /// Cancel the server session identified by `session_id` from this (separate) connection.
    /// Used when the source cannot cancel itself. Default: unsupported.
    async fn cancel_session(&self, _session_id: &str) -> Result<bool> {
        Ok(false)
    }
}
