//! Managed state and the IPC error type.
//!
//! The only thing Tauri manages is an `Arc<BulkDb>` (the bulk-core facade). It is built once in
//! `setup` and torn down (`BulkDb::shutdown`, which kills ssh tunnels and cancels jobs) when the
//! main window is destroyed or the app is asked to exit.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use bulk_core::service::BulkDbOptions;
use bulk_core::{BulkDb, CoreError, ErrorPayload};
use serde::Serialize;
use tauri::{AppHandle, Manager};

/// Error returned by every `invoke` handler. Serialises as the bare `ErrorPayload` object
/// (`{ code, message, detail? }`) so the frontend sees exactly one error shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CmdError(pub ErrorPayload);

impl CmdError {
    /// An error raised by the shell itself (plugin failures, task join errors).
    pub fn new(code: &str, message: impl Into<String>, detail: Option<String>) -> Self {
        CmdError(ErrorPayload { code: code.to_string(), message: message.into(), detail })
    }

    pub fn io(message: impl Into<String>) -> Self {
        Self::new("IO", message, None)
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new("NOT_FOUND", message, None)
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::new("INTERNAL", message, None)
    }
}

impl Serialize for CmdError {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(serializer)
    }
}

impl From<CoreError> for CmdError {
    fn from(e: CoreError) -> Self {
        CmdError(e.to_payload())
    }
}

impl From<ErrorPayload> for CmdError {
    fn from(p: ErrorPayload) -> Self {
        CmdError(p)
    }
}

impl From<tauri::Error> for CmdError {
    fn from(e: tauri::Error) -> Self {
        CmdError::internal(format!("tauri: {e}"))
    }
}

impl std::fmt::Display for CmdError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.0.code, self.0.message)
    }
}

impl std::error::Error for CmdError {}

/// Result type of every command.
pub type CmdResult<T> = Result<T, CmdError>;

/// Build the core facade with its defaults (platform config dir, OS keyring, event-based
/// tunnel prompts, file + ring-buffer logging). Runs on the main thread inside `setup`, so the
/// async constructor is driven with `block_on`.
pub fn init(started: Instant) -> Result<Arc<BulkDb>, CoreError> {
    let db = tauri::async_runtime::block_on(BulkDb::new(BulkDbOptions::default()))?;
    tracing::info!(
        config_dir = %db.config.dir().display(),
        log_dir = %bulk_core::platform::log_dir().display(),
        core_ready_ms = started.elapsed().as_millis() as u64,
        "desktop shell: core initialised"
    );
    Ok(db)
}

/// Run a blocking core call (file system, sqlite, keyring, zip) on the blocking pool so the
/// async runtime that serves IPC stays responsive.
pub async fn blocking<T, F>(svc: &Arc<BulkDb>, f: F) -> CmdResult<T>
where
    F: FnOnce(&BulkDb) -> bulk_core::Result<T> + Send + 'static,
    T: Send + 'static,
{
    let db = Arc::clone(svc);
    match tauri::async_runtime::spawn_blocking(move || f(&db)).await {
        Ok(result) => result.map_err(CmdError::from),
        Err(e) => Err(CmdError::internal(format!("blocking task failed: {e}"))),
    }
}

static SHUTDOWN_DONE: AtomicBool = AtomicBool::new(false);

/// Cancel jobs and kill tunnels. Idempotent: `ExitRequested` and the main window's `Destroyed`
/// both route here and only the first call does work. Runs on the main thread (event loop
/// callback), so blocking on the async runtime is safe.
pub fn shutdown(app: &AppHandle) {
    if SHUTDOWN_DONE.swap(true, Ordering::SeqCst) {
        return;
    }
    let Some(db) = app.try_state::<Arc<BulkDb>>() else {
        return;
    };
    let db = Arc::clone(db.inner());
    let t = Instant::now();
    tauri::async_runtime::block_on(db.shutdown());
    tracing::info!(elapsed_ms = t.elapsed().as_millis() as u64, "desktop shell: core shut down");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cmd_error_serialises_as_payload_object() {
        let err: CmdError = CoreError::query_with_code("42P01", "relation \"x\" does not exist").into();
        let v = serde_json::to_value(&err).unwrap();
        assert_eq!(v["code"], "QUERY");
        assert_eq!(v["message"], "relation \"x\" does not exist");
        assert_eq!(v["detail"], "SQLSTATE 42P01");
        assert_eq!(v.as_object().unwrap().len(), 3, "exactly code/message/detail");
    }

    #[test]
    fn cmd_error_without_detail_serialises_null_detail() {
        let err: CmdError = CoreError::Cancelled.into();
        let v = serde_json::to_value(&err).unwrap();
        assert_eq!(v["code"], "CANCELLED");
        assert_eq!(v["message"], "cancelled");
        assert!(v["detail"].is_null());
    }

    #[test]
    fn shell_errors_use_core_codes() {
        let v = serde_json::to_value(CmdError::not_found("no such path")).unwrap();
        assert_eq!(v["code"], "NOT_FOUND");
        let v = serde_json::to_value(CmdError::io("explorer failed")).unwrap();
        assert_eq!(v["code"], "IO");
        assert_eq!(CmdError::internal("x").to_string(), "INTERNAL: x");
    }
}
