//! Target sessions (section 4): the [`TargetSession`] / [`TargetOpener`] seams (defined in
//! `copy::types`), the real opener and one writer per family. Owner: copy-data.
//!
//! Session kinds are named by their `application_name` (4.1): `bulkdb-copyplan` (planning),
//! `bulkdb-<job>-m` (the job's meta / plan session), `bulkdb-<job>-l` (lease), `bulkdb-<job>-v`
//! (checksum read-back) are read-only; only `bulkdb-<job>-t<n>` sessions write. SQLite opens a
//! missing target file only for a write session (a plan or dry run leaves no file behind).

use std::sync::{Arc, Weak};

use async_trait::async_trait;

use crate::copy::types::{CopyEndpoint, CopyFamily, ServerIdentity, TargetFacts};
use crate::error::{CoreError, Result};
use crate::service::BulkDb;
use crate::types::Connection;

pub use crate::copy::types::{TargetOpener, TargetSession, WriterConfig};

pub mod mysql;
pub mod pg;
pub mod sqlite;

/// Whether `app_name` names a write session (`…-t<n>`).
pub fn is_write_session(app_name: &str) -> bool {
    match app_name.rsplit_once("-t") {
        Some((_, n)) => !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()),
        None => false,
    }
}

/// The connection a session opens: the endpoint's database replaces the saved one (PG, MySQL).
pub fn endpoint_connection(conn: &Connection, ep: &CopyEndpoint) -> Connection {
    let mut c = conn.clone();
    if c.driver != "sqlite" && !ep.database.is_empty() {
        c.database = Some(ep.database.clone());
    }
    c
}

/// Non-secret endpoint text: `host:port` (+ ` via ssh host`) or the SQLite path.
pub fn endpoint_text(conn: &Connection) -> String {
    if conn.driver == "sqlite" {
        return sqlite::target_path(conn).unwrap_or_default();
    }
    let mut s = format!("{}:{}", conn.host.as_deref().unwrap_or("localhost"), conn.port.map(|p| p.to_string()).unwrap_or_default());
    if let Some(ssh) = &conn.ssh {
        if ssh.enabled && !ssh.host.is_empty() {
            s.push_str(&format!(" via ssh {}", ssh.host));
        }
    }
    s
}

/// The real opener: picks `PgTarget`, `MysqlTarget` or `SqliteTarget` by the connection's
/// driver. PostgreSQL and MySQL sessions go through `BulkDb::open_connection` (tunnel,
/// keyring and session passwords included).
#[derive(Clone, Default)]
pub struct DriverOpener {
    db: Weak<BulkDb>,
}

impl DriverOpener {
    pub fn new(db: Weak<BulkDb>) -> DriverOpener {
        DriverOpener { db }
    }

    fn db(&self) -> Result<Arc<BulkDb>> {
        self.db.upgrade().ok_or_else(|| CoreError::Internal("copy: the core is shutting down".into()))
    }
}

#[async_trait]
impl TargetOpener for DriverOpener {
    async fn open(&self, conn: &Connection, ep: &CopyEndpoint, app_name: &str) -> Result<Box<dyn TargetSession>> {
        let write = is_write_session(app_name);
        match CopyFamily::of_driver(&conn.driver) {
            Some(CopyFamily::Sqlite) => Ok(Box::new(sqlite::SqliteTarget::open(conn, write).await?)),
            Some(CopyFamily::Postgres) => {
                let db = self.db()?;
                Ok(Box::new(pg::PgTarget::open(&db, conn, ep, app_name, write).await?))
            }
            Some(CopyFamily::Mysql) => {
                let db = self.db()?;
                Ok(Box::new(mysql::MysqlTarget::open(&db, conn, ep, app_name, write).await?))
            }
            None => Err(CoreError::Invalid(format!("copy: driver '{}' cannot be a copy target", conn.driver))),
        }
    }
}

/// Facts shell with the endpoint filled in.
pub(crate) fn base_facts(family: CopyFamily, flavor: &str, version: &str, database: &str, conn: &Connection) -> TargetFacts {
    let mut f = TargetFacts::new(family, flavor, version, database);
    f.endpoint = endpoint_text(conn);
    f.user = conn.user.clone();
    f
}

/// Identity from the facts (see `preflight::identity`).
pub(crate) fn with_identity(mut f: TargetFacts) -> TargetFacts {
    f.identity = crate::copy::preflight::identity(&f);
    f
}

/// Placeholder identity for sessions built by `new(facts)` (tests).
pub(crate) fn identity_or_default(f: &TargetFacts) -> ServerIdentity {
    if f.identity.key.is_empty() {
        crate::copy::preflight::identity(f)
    } else {
        f.identity.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_kinds_by_name() {
        assert!(is_write_session("bulkdb-abc-t0"));
        assert!(is_write_session("bulkdb-abc-t12"));
        assert!(!is_write_session("bulkdb-abc-m"));
        assert!(!is_write_session("bulkdb-copyplan"));
        assert!(!is_write_session("bulkdb-abc-v"));
        assert!(!is_write_session("bulkdb-abc-t"));
        assert!(!is_write_session("bulkdb-abc-tx"));
    }
}
