//! # bulk-core
//!
//! Everything BulkDB does lives here: drivers (`source`, `catalog`, `monitor`), file writers
//! (`sink`), the streaming pipeline (`job`), preflight checks, SSH tunnels, credentials,
//! configuration, history and logging. The Tauri shell and `bulk-cli` are thin adapters over
//! [`service::BulkDb`].
//!
//! Design rules (from the spec, enforced in review):
//! 1. Rows are never materialised as objects. Bytes flow `Source -> bounded channel -> Sink`.
//! 2. Memory is fixed regardless of result size (channel of 8 batches, batch <= 8 MB).
//! 3. No dependency on Tauri here. No `tauri::` types anywhere in this crate.
//! 4. Drivers are three tiers; see `drivers`.
//! 5. `#[cfg(target_os = ...)]` appears only inside `platform/`.

pub mod catalog;
pub mod catalog_cache;
pub mod config;
pub mod connect;
pub mod credentials;
pub mod dbeaver;
pub mod drivers;
pub mod error;
pub mod export;
pub mod history;
pub mod job;
pub mod logging;
pub mod monitor;
pub mod platform;
pub mod preflight;
pub mod prompts;
pub mod service;
pub mod sink;
pub mod source;
pub mod sqlguard;
pub mod testdata;
pub mod tunnel;
pub mod types;

pub use error::{CoreError, ErrorPayload, Result};
pub use service::BulkDb;
pub use tokio_util::sync::CancellationToken;

/// Application name prefix used for server-side sessions (`bulkdb-<job id>`), so that
/// monitors can find the extraction session in `pg_stat_activity` and friends.
pub const APP_NAME_PREFIX: &str = "bulkdb-";

/// Bounded channel depth between reader and writer. 8 batches x 8 MB = 64 MB worst case.
pub const CHANNEL_DEPTH: usize = 8;
/// A batch is closed when it reaches this many rows ...
pub const BATCH_ROWS: usize = 65_536;
/// ... or this many bytes, whichever comes first.
pub const BATCH_BYTES: usize = 8 * 1024 * 1024;

#[cfg(all(test, feature = "ts-export"))]
mod ts_export;
