//! bulk-cli: the core without a window.
//!
//! ```text
//! bulk-cli export  --conn prod --sql query.sql --out ./out/export.csv --work-mem 64MB --progress
//! bulk-cli export  --driver sqlite --path test.db --query "select * from t" --out out.parquet --yes
//! bulk-cli catalog tables --conn prod --schema public
//! bulk-cli catalog count --conn prod --table public.orders
//! bulk-cli probe   --conn prod
//! bulk-cli explain --conn prod --sql query.sql
//! bulk-cli check   --conn prod --query "select * from orders"
//! bulk-cli conn    list | show | add | edit | delete | test | import-dbeaver
//! bulk-cli info
//! bulk-cli drivers
//! bulk-cli jobs
//! bulk-cli history --conn prod --limit 20
//! bulk-cli settings get | set max_concurrent_jobs=2 default_output_dir=/data/exports
//! bulk-cli diagnostics --out ./support
//! ```
//!
//! `--progress` prints rows, MB/s, ETA and the process RSS once per second (the memory gate).
//! Everything goes through [`bulk_core::BulkDb`]; this crate only parses flags, prints and
//! maps job states to exit codes (0 done, 1 failed, 2 cancelled, 3 rejected, 4 needs --yes).

mod args;
mod cmd_catalog;
mod cmd_conn;
mod cmd_export;
mod cmd_history;
mod cmd_info;
mod cmd_settings;
mod connect;
mod output;
mod progress;
mod prompts;

use std::io::Write;
use std::process::ExitCode;
use std::sync::Arc;

use bulk_core::error::{CoreError, ErrorPayload};
use bulk_core::service::BulkDbOptions;
use bulk_core::BulkDb;
use clap::Parser;

use crate::args::{Cli, Cmd};

/// Exit codes (also documented in `--help`).
pub const EXIT_DONE: u8 = 0;
pub const EXIT_FAILED: u8 = 1;
pub const EXIT_CANCELLED: u8 = 2;
pub const EXIT_REJECTED: u8 = 3;
pub const EXIT_CONFIRM: u8 = 4;

/// stderr writer that runs every log line through the core's secret masking.
struct MaskedStderr;

impl Write for MaskedStderr {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let text = String::from_utf8_lossy(buf);
        std::io::stderr().write_all(bulk_core::logging::mask_secrets(&text).as_bytes())?;
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        std::io::stderr().flush()
    }
}

/// Install a `tracing` fmt layer on stderr. `--log-level` wins, then `RUST_LOG`, then
/// `warn` (`info` when an export runs with `--progress`).
fn init_logging(cli: &Cli) {
    let default = match &cli.cmd {
        Cmd::Export(e) if e.progress => "info",
        _ => "warn",
    };
    let directive = cli.log_level.clone().or_else(|| std::env::var("RUST_LOG").ok().filter(|v| !v.trim().is_empty())).unwrap_or_else(|| default.to_string());
    let filter = tracing_subscriber::EnvFilter::try_new(&directive).unwrap_or_else(|e| {
        eprintln!("warning: invalid log level '{directive}' ({e}); using '{default}'");
        tracing_subscriber::EnvFilter::new(default)
    });
    let _ = tracing_subscriber::fmt().with_env_filter(filter).with_writer(|| MaskedStderr).with_ansi(false).with_target(false).try_init();
}

/// Print an error the way the mode wants it and pick the exit code.
fn report_error(e: &anyhow::Error, json: bool) -> ExitCode {
    let core = e.downcast_ref::<CoreError>();
    let payload = match core {
        Some(c) => {
            let mut p = c.to_payload();
            let full = format!("{e:#}");
            if full != p.message {
                p.message = full;
            }
            p
        }
        None => ErrorPayload { code: "CLI".into(), message: format!("{e:#}"), detail: None },
    };
    if json {
        eprintln!("{}", serde_json::json!({ "error": payload }));
    } else {
        eprintln!("error [{}]: {}", payload.code, payload.message);
        if let Some(d) = &payload.detail {
            eprintln!("  {d}");
        }
    }
    ExitCode::from(if matches!(core, Some(CoreError::Cancelled)) { EXIT_CANCELLED } else { EXIT_FAILED })
}

/// Run a command that has no job of its own: Ctrl+C drops the future (which removes a
/// temporary connection through `ConnHandle::drop`) and exits 2.
async fn with_ctrl_c<F>(fut: F) -> anyhow::Result<ExitCode>
where
    F: std::future::Future<Output = anyhow::Result<ExitCode>>,
{
    tokio::select! {
        r = fut => r,
        _ = tokio::signal::ctrl_c() => {
            eprintln!("interrupted");
            Err(CoreError::Cancelled.into())
        }
    }
}

async fn run(cli: Cli) -> anyhow::Result<ExitCode> {
    let json = cli.json;
    let db: Arc<BulkDb> =
        BulkDb::new(BulkDbOptions { config_dir: cli.config_dir.clone(), prompts: Some(Arc::new(prompts::StdinPrompts)), credentials: None, init_logging: false }).await?;
    let result = match cli.cmd {
        Cmd::Export(a) => cmd_export::run(&db, a, json).await,
        Cmd::Catalog(a) => with_ctrl_c(cmd_catalog::run(&db, a, json)).await,
        Cmd::Probe(a) => with_ctrl_c(cmd_info::probe(&db, a, json)).await,
        Cmd::Explain(a) => with_ctrl_c(cmd_info::explain(&db, a, json)).await,
        Cmd::Check(a) => cmd_info::check(&db, a, json),
        Cmd::Info => cmd_info::info(&db, json),
        Cmd::Conn(a) => with_ctrl_c(cmd_conn::run(&db, a, json)).await,
        Cmd::Drivers(a) => cmd_info::drivers(&db, a, json),
        Cmd::Jobs => cmd_info::jobs(&db, json),
        Cmd::History(a) => cmd_history::run(&db, a, json),
        Cmd::Settings(a) => cmd_settings::run(&db, a, json),
        Cmd::Diagnostics(a) => cmd_info::diagnostics(&db, a, json),
    };
    db.shutdown().await;
    result
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(c) => c,
        Err(e) => {
            // `--help` / `--version` are not errors; usage mistakes exit 1 (2 means cancelled here).
            let _ = e.print();
            return ExitCode::from(if e.use_stderr() { EXIT_FAILED } else { EXIT_DONE });
        }
    };
    init_logging(&cli);
    let json = cli.json;
    match run(cli).await {
        Ok(code) => code,
        Err(e) => report_error(&e, json),
    }
}
