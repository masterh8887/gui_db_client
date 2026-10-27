//! `invoke` handlers — the IPC contract the frontend (`src/lib/ipc.ts`) calls.
//!
//! Every command is a thin adapter: parse arguments, call `bulk_core::BulkDb`, map the error to
//! `ErrorPayload { code, message, detail? }`. No business logic lives here and data rows never
//! cross this boundary (the catalog returns metadata only; exports write files).
//!
//! Argument names are camelCase on the wire (Tauri converts them to the snake_case Rust
//! parameters). Optional arguments may be omitted. Types are the ts-rs exports in
//! `packages/ipc-types`.
//!
//! | command                 | args (camelCase)                               | returns               |
//! |-------------------------|------------------------------------------------|-----------------------|
//! | drivers_list            | —                                              | DriverInfo[]          |
//! | drivers_rescan          | —                                              | DriverInfo[]          |
//! | conn_list               | —                                              | Connection[]          |
//! | conn_get                | connId                                         | Connection            |
//! | conn_save               | input: ConnectionInput                         | Connection            |
//! | conn_delete             | connId                                         | null                  |
//! | conn_test               | connId, password?                              | ProbeResult           |
//! | conn_import_dbeaver     | path                                           | ImportedConnection[]  |
//! | catalog_databases       | connId, refresh?                               | DatabaseInfo[]        |
//! | catalog_schemas         | connId, db, refresh?                           | SchemaInfo[]          |
//! | catalog_tables          | connId, db, schema, refresh?                   | TableInfo[]           |
//! | catalog_columns         | connId, table: TableRef                        | ColumnInfo[]          |
//! | catalog_indexes         | connId, table: TableRef                        | IndexInfo[]           |
//! | catalog_ddl             | connId, table: TableRef                        | string (DDL)          |
//! | catalog_select_sql      | connId, table: TableRef                        | string (SELECT ...)   |
//! | catalog_exact_count     | connId, table: TableRef                        | string (jobId)        |
//! | catalog_last_exact_count| connId, table: TableRef                        | ExactCount or null    |
//! | catalog_invalidate      | connId                                         | null                  |
//! | query_explain           | connId, sql, analyze?                          | PlanSummary           |
//! | sql_check               | sql, connId? (dialect of that connection)      | SqlCheck              |
//! | sql_file_read           | path                                           | SqlFile               |
//! | export_preflight        | req: ExportRequest                             | PreflightReport       |
//! | export_start            | req: ExportRequest, acknowledgedWarnings: str[] | string (jobId)       |
//! | job_resume              | jobId, acknowledgedWarnings: string[]          | null                  |
//! | job_cancel              | jobId                                          | null                  |
//! | job_list                | —                                              | JobSummary[]          |
//! | job_get                 | jobId                                          | JobSummary            |
//! | history_list            | connId?, limit? (default 200)                  | HistoryEntry[]        |
//! | settings_get            | —                                              | Settings              |
//! | settings_set            | settings: Settings                             | Settings (normalised) |
//! | tunnel_confirm_hostkey  | promptId, accept: bool                         | null                  |
//! | tunnel_provide_secret   | promptId, secret? (omit/null = cancel)         | null                  |
//! | logs_recent             | limit? (default 500)                           | LogEvent[]            |
//! | diagnostics_bundle      | outDir                                         | DiagnosticsBundle     |
//! | app_info                | —                                              | AppInfo               |
//! | open_path               | path (file: reveal in folder; dir: open it)    | null                  |
//!
//! Events emitted by the shell are documented in `events.rs`:
//! `job:progress`, `job:server`, `job:state`, `job:done`, `job:log`, `tunnel:hostkey`,
//! `tunnel:password`, `drivers:changed`.
//!
//! Errors: every command rejects with `ErrorPayload` whose `code` is one of
//! `CONNECT, AUTH, QUERY, IO, CANCELLED, TIMEOUT, DRIVER_MISSING, OOM_RISK, INVALID, TUNNEL,
//! NOT_FOUND, INTERNAL`.

use std::path::Path;
use std::sync::Arc;

use bulk_core::sqlguard::SqlCheck;
use bulk_core::types::*;
use bulk_core::BulkDb;
use tauri::{AppHandle, State};
use tauri_plugin_opener::OpenerExt;

use crate::state::{blocking, CmdError, CmdResult};

/// Default page size for `history_list`.
const DEFAULT_HISTORY_LIMIT: usize = 200;
/// Default number of lines for `logs_recent`.
const DEFAULT_LOG_LIMIT: usize = 500;

// ---------------------------------------------------------------------------------------- drivers

#[tauri::command]
pub async fn drivers_list(svc: State<'_, Arc<BulkDb>>) -> CmdResult<Vec<DriverInfo>> {
    Ok(svc.drivers_list())
}

#[tauri::command]
pub async fn drivers_rescan(svc: State<'_, Arc<BulkDb>>) -> CmdResult<Vec<DriverInfo>> {
    blocking(&svc, |db| Ok(db.drivers_rescan())).await
}

// ------------------------------------------------------------------------------------ connections

#[tauri::command]
pub async fn conn_list(svc: State<'_, Arc<BulkDb>>) -> CmdResult<Vec<Connection>> {
    Ok(svc.conn_list())
}

#[tauri::command]
pub async fn conn_get(svc: State<'_, Arc<BulkDb>>, conn_id: String) -> CmdResult<Connection> {
    blocking(&svc, move |db| db.conn_get(&conn_id)).await
}

#[tauri::command]
pub async fn conn_save(svc: State<'_, Arc<BulkDb>>, input: ConnectionInput) -> CmdResult<Connection> {
    blocking(&svc, move |db| db.conn_save(input)).await
}

#[tauri::command]
pub async fn conn_delete(svc: State<'_, Arc<BulkDb>>, conn_id: String) -> CmdResult<()> {
    blocking(&svc, move |db| db.conn_delete(&conn_id)).await
}

#[tauri::command]
pub async fn conn_test(svc: State<'_, Arc<BulkDb>>, conn_id: String, password: Option<String>) -> CmdResult<ProbeResult> {
    Ok(svc.conn_test(&conn_id, password).await?)
}

#[tauri::command]
pub async fn conn_import_dbeaver(svc: State<'_, Arc<BulkDb>>, path: String) -> CmdResult<Vec<ImportedConnection>> {
    blocking(&svc, move |db| db.conn_import_dbeaver(&path)).await
}

// ---------------------------------------------------------------------------------------- catalog

#[tauri::command]
pub async fn catalog_databases(svc: State<'_, Arc<BulkDb>>, conn_id: String, refresh: Option<bool>) -> CmdResult<Vec<DatabaseInfo>> {
    Ok(svc.catalog_databases(&conn_id, refresh.unwrap_or(false)).await?)
}

#[tauri::command]
pub async fn catalog_schemas(svc: State<'_, Arc<BulkDb>>, conn_id: String, db: String, refresh: Option<bool>) -> CmdResult<Vec<SchemaInfo>> {
    Ok(svc.catalog_schemas(&conn_id, &db, refresh.unwrap_or(false)).await?)
}

#[tauri::command]
pub async fn catalog_tables(svc: State<'_, Arc<BulkDb>>, conn_id: String, db: String, schema: String, refresh: Option<bool>) -> CmdResult<Vec<TableInfo>> {
    Ok(svc.catalog_tables(&conn_id, &db, &schema, refresh.unwrap_or(false)).await?)
}

#[tauri::command]
pub async fn catalog_columns(svc: State<'_, Arc<BulkDb>>, conn_id: String, table: TableRef) -> CmdResult<Vec<ColumnInfo>> {
    Ok(svc.catalog_columns(&conn_id, &table).await?)
}

#[tauri::command]
pub async fn catalog_indexes(svc: State<'_, Arc<BulkDb>>, conn_id: String, table: TableRef) -> CmdResult<Vec<IndexInfo>> {
    Ok(svc.catalog_indexes(&conn_id, &table).await?)
}

#[tauri::command]
pub async fn catalog_ddl(svc: State<'_, Arc<BulkDb>>, conn_id: String, table: TableRef) -> CmdResult<String> {
    Ok(svc.catalog_ddl(&conn_id, &table).await?)
}

#[tauri::command]
pub async fn catalog_select_sql(svc: State<'_, Arc<BulkDb>>, conn_id: String, table: TableRef) -> CmdResult<String> {
    Ok(svc.catalog_select_sql(&conn_id, &table).await?)
}

/// Starts a cancellable `count(*)` job; the result arrives as `job:done` with `exactCount`.
#[tauri::command]
pub async fn catalog_exact_count(svc: State<'_, Arc<BulkDb>>, conn_id: String, table: TableRef) -> CmdResult<String> {
    Ok(svc.catalog_exact_count(&conn_id, &table).await?)
}

/// The last finished exact count of `table` on this connection, read back from the history
/// database so a count the user paid for survives restarts; `null` when it was never counted.
#[tauri::command]
pub async fn catalog_last_exact_count(svc: State<'_, Arc<BulkDb>>, conn_id: String, table: TableRef) -> CmdResult<Option<ExactCount>> {
    blocking(&svc, move |db| db.catalog_last_exact_count(&conn_id, &table)).await
}

/// Drop the cached metadata of one connection (the tree's connection-level refresh).
#[tauri::command]
pub async fn catalog_invalidate(svc: State<'_, Arc<BulkDb>>, conn_id: String) -> CmdResult<()> {
    svc.catalog_invalidate(&conn_id);
    Ok(())
}

// ------------------------------------------------------------------------------------------ query

#[tauri::command]
pub async fn query_explain(svc: State<'_, Arc<BulkDb>>, conn_id: String, sql: String, analyze: Option<bool>) -> CmdResult<PlanSummary> {
    Ok(svc.query_explain(&conn_id, &sql, analyze.unwrap_or(false)).await?)
}

/// `connId` picks the SQL dialect (MySQL quoting / comments, ...); without it the standard
/// rules apply. An unknown `connId` is `NOT_FOUND`.
#[tauri::command]
pub async fn sql_check(svc: State<'_, Arc<BulkDb>>, sql: String, conn_id: Option<String>) -> CmdResult<SqlCheck> {
    Ok(svc.sql_check(conn_id.as_deref(), &sql)?)
}

#[tauri::command]
pub async fn sql_file_read(svc: State<'_, Arc<BulkDb>>, path: String) -> CmdResult<SqlFile> {
    blocking(&svc, move |db| db.sql_file_read(&path)).await
}

// ----------------------------------------------------------------------------------------- export

#[tauri::command]
pub async fn export_preflight(svc: State<'_, Arc<BulkDb>>, req: ExportRequest) -> CmdResult<PreflightReport> {
    Ok(svc.export_preflight(&req).await?)
}

#[tauri::command]
pub async fn export_start(svc: State<'_, Arc<BulkDb>>, req: ExportRequest, acknowledged_warnings: Vec<String>) -> CmdResult<String> {
    Ok(svc.export_start(req, acknowledged_warnings).await?)
}

// ------------------------------------------------------------------------------------------- jobs

#[tauri::command]
pub async fn job_resume(svc: State<'_, Arc<BulkDb>>, job_id: String, acknowledged_warnings: Vec<String>) -> CmdResult<()> {
    Ok(svc.job_resume(&job_id, acknowledged_warnings)?)
}

#[tauri::command]
pub async fn job_cancel(svc: State<'_, Arc<BulkDb>>, job_id: String) -> CmdResult<()> {
    Ok(svc.job_cancel(&job_id)?)
}

#[tauri::command]
pub async fn job_list(svc: State<'_, Arc<BulkDb>>) -> CmdResult<Vec<JobSummary>> {
    Ok(svc.job_list())
}

#[tauri::command]
pub async fn job_get(svc: State<'_, Arc<BulkDb>>, job_id: String) -> CmdResult<JobSummary> {
    Ok(svc.job_get(&job_id)?)
}

// ------------------------------------------------------------------------------- history / settings

#[tauri::command]
pub async fn history_list(svc: State<'_, Arc<BulkDb>>, conn_id: Option<String>, limit: Option<usize>) -> CmdResult<Vec<HistoryEntry>> {
    let limit = limit.unwrap_or(DEFAULT_HISTORY_LIMIT);
    blocking(&svc, move |db| db.history_list(conn_id.as_deref(), limit)).await
}

#[tauri::command]
pub async fn settings_get(svc: State<'_, Arc<BulkDb>>) -> CmdResult<Settings> {
    Ok(svc.settings_get())
}

#[tauri::command]
pub async fn settings_set(svc: State<'_, Arc<BulkDb>>, settings: Settings) -> CmdResult<Settings> {
    blocking(&svc, move |db| db.settings_set(settings)).await
}

// ----------------------------------------------------------------------------------------- tunnel

#[tauri::command]
pub async fn tunnel_confirm_hostkey(svc: State<'_, Arc<BulkDb>>, prompt_id: String, accept: bool) -> CmdResult<()> {
    Ok(svc.tunnel_confirm_hostkey(&prompt_id, accept)?)
}

/// `secret: null` / omitted cancels the tunnel attempt.
#[tauri::command]
pub async fn tunnel_provide_secret(svc: State<'_, Arc<BulkDb>>, prompt_id: String, secret: Option<String>) -> CmdResult<()> {
    Ok(svc.tunnel_provide_secret(&prompt_id, secret)?)
}

// ------------------------------------------------------------------------------- logs / diagnostics

#[tauri::command]
pub async fn logs_recent(svc: State<'_, Arc<BulkDb>>, limit: Option<usize>) -> CmdResult<Vec<LogEvent>> {
    Ok(svc.logs_recent(limit.unwrap_or(DEFAULT_LOG_LIMIT)))
}

#[tauri::command]
pub async fn diagnostics_bundle(svc: State<'_, Arc<BulkDb>>, out_dir: String) -> CmdResult<DiagnosticsBundle> {
    blocking(&svc, move |db| db.diagnostics_bundle(&out_dir)).await
}

// -------------------------------------------------------------------------------------------- app

#[tauri::command]
pub async fn app_info(app: AppHandle, svc: State<'_, Arc<BulkDb>>) -> CmdResult<AppInfo> {
    Ok(svc.app_info(&app.package_info().version.to_string()))
}

/// Show a path in the OS file manager: a file is revealed (selected) inside its folder, a
/// directory is opened. Used for "open output folder" after an export.
#[tauri::command]
pub async fn open_path(app: AppHandle, path: String) -> CmdResult<()> {
    let p = Path::new(path.trim());
    if path.trim().is_empty() || !p.exists() {
        return Err(CmdError::not_found(format!("path does not exist: {}", p.display())));
    }
    let result = if p.is_dir() { app.opener().open_path(p.display().to_string(), None::<&str>) } else { app.opener().reveal_item_in_dir(p) };
    result.map_err(|e| CmdError::io(format!("open path: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    use bulk_core::credentials::MemoryStore;
    use bulk_core::service::BulkDbOptions;

    /// A facade over a throw-away config dir and an in-memory credential store.
    async fn service() -> (tempfile_dir::TempDir, Arc<BulkDb>) {
        let dir = tempfile_dir::TempDir::new();
        let db = BulkDb::new(BulkDbOptions {
            config_dir: Some(dir.path().to_path_buf()),
            prompts: None,
            credentials: Some(Box::new(MemoryStore::default())),
            init_logging: false,
        })
        .await
        .expect("core starts");
        (dir, db)
    }

    /// `sql_check` returns core's `SqlCheck`; its JSON must stay the shape the frontend was
    /// built against (flat camelCase fields, lower-case kind).
    #[test]
    fn sql_check_wire_shape_is_unchanged() {
        let v = serde_json::to_value(bulk_core::sqlguard::check("select 1;")).unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(v["kind"], "select");
        assert_eq!(v["statements"], 1);
        assert_eq!(v["normalized"], "select 1");
        assert!(v["issues"].as_array().unwrap().is_empty());
        let keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(keys.len(), 5, "{keys:?}");

        let dml = serde_json::to_value(bulk_core::sqlguard::check("delete from t")).unwrap();
        assert_eq!(dml["ok"], false);
        assert_eq!(dml["kind"], "other");
        assert_eq!(dml["issues"][0]["code"], bulk_core::sqlguard::CODE_NOT_SELECT);
    }

    #[tokio::test]
    async fn sql_check_uses_the_connection_dialect() {
        let (_dir, db) = service().await;
        let mysql = db
            .conn_save(ConnectionInput {
                connection: Connection { name: "my".into(), driver: "mysql".into(), host: Some("h".into()), ..Default::default() },
                ..Default::default()
            })
            .unwrap();
        // One string literal for PostgreSQL; a closed string plus a second statement for MySQL.
        let sql = r"select '\'' ; delete from t -- '";
        assert!(db.sql_check(None, sql).unwrap().ok);
        let checked = db.sql_check(Some(&mysql.id), sql).unwrap();
        assert!(!checked.ok, "{checked:?}");
        assert_eq!(checked.issues[0].code, bulk_core::sqlguard::CODE_MULTI);
        let missing = db.sql_check(Some("nope"), "select 1").unwrap_err();
        assert_eq!(CmdError::from(missing).0.code, "NOT_FOUND");
    }

    #[tokio::test]
    async fn app_info_comes_from_core() {
        let (dir, db) = service().await;
        let info = db.app_info("9.9.9");
        let v = serde_json::to_value(&info).unwrap();
        assert_eq!(v["version"], "9.9.9");
        assert_eq!(v["platform"], bulk_core::platform::os_key());
        assert_eq!(v["arch"], bulk_core::platform::arch());
        assert_eq!(v["configDir"], dir.path().display().to_string());
        assert!(v["logDir"].as_str().is_some_and(|s| !s.is_empty()));
        assert_eq!(v.as_object().unwrap().len(), 5);
    }

    /// The facade calls behind `conn_get` and `catalog_invalidate`.
    #[tokio::test]
    async fn conn_get_and_catalog_invalidate() {
        let (_dir, db) = service().await;
        let saved = db
            .conn_save(ConnectionInput {
                connection: Connection { name: "pg".into(), driver: "postgres".into(), host: Some("h".into()), save_password: true, ..Default::default() },
                password: Some("pw".into()),
                ..Default::default()
            })
            .unwrap();
        let got = db.conn_get(&saved.id).unwrap();
        assert_eq!(got, saved);
        assert!(got.has_password);
        let err: CmdError = db.conn_get("missing").unwrap_err().into();
        assert_eq!(err.0.code, "NOT_FOUND");
        // Invalidating an uncached (or unknown) connection is a harmless no-op.
        db.catalog_invalidate(&saved.id);
        db.catalog_invalidate("missing");
    }

    /// The facade call behind `catalog_last_exact_count`: `null` on the wire for a table that was
    /// never counted, `NOT_FOUND` for an unknown connection, camelCase fields for a stored count.
    #[tokio::test]
    async fn catalog_last_exact_count_reads_history() {
        let (_dir, db) = service().await;
        let saved = db
            .conn_save(ConnectionInput {
                connection: Connection { name: "pg".into(), driver: "postgres".into(), host: Some("h".into()), ..Default::default() },
                ..Default::default()
            })
            .unwrap();
        let table = TableRef::new(Some("public"), "t");
        let never = db.catalog_last_exact_count(&saved.id, &table).unwrap();
        assert_eq!(never, None);
        assert!(serde_json::to_value(never).unwrap().is_null(), "a never-counted table is null on the wire");
        let err: CmdError = db.catalog_last_exact_count("missing", &table).unwrap_err().into();
        assert_eq!(err.0.code, "NOT_FOUND");

        let stored = ExactCount { table: table.clone(), rows: 7, elapsed_ms: 3, at: "2026-10-27T00:00:00Z".into() };
        let v = serde_json::to_value(Some(stored)).unwrap();
        assert_eq!(v["rows"], 7);
        assert_eq!(v["elapsedMs"], 3);
        assert_eq!(v["at"], "2026-10-27T00:00:00Z");
        assert_eq!(v["table"]["schema"], "public");
        assert_eq!(v["table"]["name"], "t");
    }

    /// End to end over a SQLite fixture: the count a `catalog_exact_count` job delivered with
    /// `job:done` is exactly what `catalog_last_exact_count` returns afterwards (also for a
    /// facade restarted on the same config dir), and its wire shape is what `TableTab` reads.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn catalog_last_exact_count_returns_the_finished_count() {
        let (dir, db) = service().await;
        let fixture = dir.path().join("fixture.db");
        bulk_core::testdata::sqlite_fixture(&fixture, 3, 7, 0).expect("sqlite fixture");
        let mut fields = std::collections::BTreeMap::new();
        fields.insert("path".to_string(), fixture.display().to_string());
        let saved = db
            .conn_save(ConnectionInput { connection: Connection { name: "fx".into(), driver: "sqlite".into(), fields, ..Default::default() }, ..Default::default() })
            .unwrap();
        let table = TableRef::new(None, "narrow");
        assert_eq!(db.catalog_last_exact_count(&saved.id, &table).unwrap(), None);

        let mut events = db.subscribe();
        let job_id = db.catalog_exact_count(&saved.id, &table).await.unwrap();
        let done = tokio::time::timeout(std::time::Duration::from_secs(60), async {
            loop {
                match events.recv().await {
                    Ok(CoreEvent::JobDone(d)) if d.job_id == job_id => return d,
                    Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(e) => panic!("event bus closed: {e}"),
                }
            }
        })
        .await
        .expect("job:done within 60 s");
        assert_eq!(done.state, JobState::Done, "{:?}", done.error);
        let delivered = done.exact_count.expect("job:done carries the count");

        let stored = db.catalog_last_exact_count(&saved.id, &table).unwrap().expect("count persisted");
        assert_eq!(stored, delivered);
        let v = serde_json::to_value(Some(&stored)).unwrap();
        assert_eq!(v["rows"], 7);
        assert_eq!(v["table"]["name"], "narrow");
        assert!(v["at"].as_str().is_some_and(|at| !at.is_empty()), "{v}");
        assert!(v["elapsedMs"].is_u64(), "{v}");
        assert_eq!(db.catalog_last_exact_count(&saved.id, &TableRef::new(None, "wide")).unwrap(), None, "other tables stay uncounted");

        // The count survives a restart of the shell (it lives in the history database).
        drop(db);
        let reopened = BulkDb::new(BulkDbOptions {
            config_dir: Some(dir.path().to_path_buf()),
            prompts: None,
            credentials: Some(Box::new(MemoryStore::default())),
            init_logging: false,
        })
        .await
        .expect("core restarts");
        assert_eq!(reopened.catalog_last_exact_count(&saved.id, &table).unwrap(), Some(stored));
    }

    /// Minimal temp dir (the shell has no `tempfile` dev-dependency).
    mod tempfile_dir {
        use std::path::{Path, PathBuf};
        use std::sync::atomic::{AtomicUsize, Ordering};

        pub struct TempDir(PathBuf);

        impl TempDir {
            pub fn new() -> Self {
                static N: AtomicUsize = AtomicUsize::new(0);
                let p = std::env::temp_dir().join(format!("bulkdb-desktop-test-{}-{}", std::process::id(), N.fetch_add(1, Ordering::SeqCst)));
                std::fs::create_dir_all(&p).expect("create temp dir");
                TempDir(p)
            }

            pub fn path(&self) -> &Path {
                &self.0
            }
        }

        impl Drop for TempDir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }
}
