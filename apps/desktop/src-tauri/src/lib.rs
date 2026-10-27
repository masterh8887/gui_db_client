//! Tauri shell. Rules: no business logic here; `commands.rs` functions convert arguments to
//! core types, call `bulk_core::BulkDb` and return; `events.rs` forwards core events and emits
//! progress every 200 ms; `state.rs` owns the managed `Arc<BulkDb>` and the IPC error type.

mod commands;
mod events;
mod state;

use std::sync::Arc;
use std::time::Instant;

use tauri::{Manager, RunEvent, WindowEvent};

/// Label of the single window declared in `tauri.conf.json`.
const MAIN_WINDOW: &str = "main";

pub fn run() {
    let started = Instant::now();
    let app = tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .setup(move |app| {
            let db = state::init(started)?;
            app.manage(Arc::clone(&db));
            events::spawn(app.handle().clone(), db);
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::drivers_list,
            commands::drivers_rescan,
            commands::conn_list,
            commands::conn_get,
            commands::conn_save,
            commands::conn_delete,
            commands::conn_test,
            commands::conn_import_dbeaver,
            commands::catalog_databases,
            commands::catalog_schemas,
            commands::catalog_tables,
            commands::catalog_columns,
            commands::catalog_indexes,
            commands::catalog_ddl,
            commands::catalog_select_sql,
            commands::catalog_exact_count,
            commands::catalog_last_exact_count,
            commands::catalog_invalidate,
            commands::query_explain,
            commands::sql_check,
            commands::sql_file_read,
            commands::export_preflight,
            commands::export_start,
            commands::job_resume,
            commands::job_cancel,
            commands::job_list,
            commands::job_get,
            commands::history_list,
            commands::settings_get,
            commands::settings_set,
            commands::tunnel_confirm_hostkey,
            commands::tunnel_provide_secret,
            commands::logs_recent,
            commands::diagnostics_bundle,
            commands::app_info,
            commands::open_path,
        ])
        .build(tauri::generate_context!())
        .expect("error while building BulkDB");

    app.run(move |handle, event| match event {
        // The spec's cold-start gate is 2 s: measured from `run()` to the event loop being ready.
        RunEvent::Ready => {
            tracing::info!(startup_ms = started.elapsed().as_millis() as u64, "desktop shell: ready");
        }
        // Either path ends the session: kill tunnels and cancel jobs before the process goes.
        RunEvent::ExitRequested { .. } => state::shutdown(handle),
        RunEvent::WindowEvent { label, event: WindowEvent::Destroyed, .. } if label == MAIN_WINDOW => state::shutdown(handle),
        _ => {}
    });
}

#[cfg(test)]
mod tests {
    /// The main window's capability file, exactly as `tauri-build` embeds it.
    const DEFAULT_CAPABILITY: &str = include_str!("../capabilities/default.json");

    /// `opener:allow-open-url` without a URL scope enables the command but its allow list is
    /// empty, so tauri-plugin-opener rejects every URL with `ForbiddenUrl`. The driver
    /// "install" links (https URLs from `drivers/registry.json`) need an explicit https scope.
    #[test]
    fn open_url_permission_is_scoped_to_https() {
        let cap: serde_json::Value = serde_json::from_str(DEFAULT_CAPABILITY).expect("capability JSON parses");
        let perms = cap["permissions"].as_array().expect("permissions array");
        assert!(!perms.iter().any(|p| p.as_str() == Some("opener:allow-open-url")), "an unscoped opener:allow-open-url denies every URL");
        let scoped = perms.iter().find(|p| p["identifier"] == "opener:allow-open-url").expect("opener:allow-open-url is granted with a scope");
        let urls: Vec<&str> = scoped["allow"].as_array().expect("allow list").iter().filter_map(|e| e["url"].as_str()).collect();
        assert_eq!(urls, ["https://*"], "install links are https; nothing broader is granted");
        assert!(scoped.get("deny").is_none_or(|d| d.as_array().is_some_and(|d| d.is_empty())));
    }

    /// The app config, exactly as `tauri-build` / `generate_context!` read it.
    const TAURI_CONF: &str = include_str!("../tauri.conf.json");
    /// The shell's manifest (dependency list), for the updater consistency check.
    const CARGO_TOML: &str = include_str!("../Cargo.toml");

    fn tauri_conf() -> serde_json::Value {
        serde_json::from_str(TAURI_CONF).expect("tauri.conf.json parses")
    }

    /// Sources of one CSP directive (`None` when the directive is absent).
    fn csp_directive<'a>(csp: &'a str, name: &str) -> Option<Vec<&'a str>> {
        csp.split(';').map(str::trim).find_map(|d| {
            let mut parts = d.split_whitespace();
            (parts.next() == Some(name)).then(|| parts.collect())
        })
    }

    /// Tauri 2 sends every invoke and event over its IPC custom protocol (`ipc://localhost` on
    /// macOS, `http://ipc.localhost` on Windows/WebView2). A CSP without those origins in
    /// `connect-src` makes the first invoke fail, after which Tauri logs "IPC custom protocol
    /// failed" and falls back to the slower postMessage channel for the rest of the session.
    /// `'self'` must stay too: an explicit `connect-src` no longer inherits `default-src`.
    #[test]
    fn csp_allows_the_ipc_custom_protocol() {
        let conf = tauri_conf();
        let security = &conf["app"]["security"];
        let csp = security["csp"].as_str().expect("app.security.csp is a policy string");
        // `devCsp` replaces `csp` in dev builds when present, so it must allow IPC as well.
        let policies = std::iter::once(("csp", csp)).chain(security["devCsp"].as_str().map(|p| ("devCsp", p)));
        for (key, policy) in policies {
            let connect = csp_directive(policy, "connect-src").unwrap_or_else(|| panic!("{key} has an explicit connect-src"));
            for required in ["'self'", "ipc:", "http://ipc.localhost"] {
                assert!(connect.contains(&required), "{key} connect-src lacks {required}: {connect:?}");
            }
            for broad in ["*", "http:", "https:", "data:", "blob:"] {
                assert!(!connect.contains(&broad), "{key} connect-src must not allow {broad}: {connect:?}");
            }
            assert_eq!(csp_directive(policy, "default-src"), Some(vec!["'self'"]), "{key} default-src stays 'self'");
        }
    }

    /// Updater artifacts are all-or-nothing: `bundle.createUpdaterArtifacts` makes `tauri build`
    /// sign the installers with `TAURI_SIGNING_PRIVATE_KEY`, which needs the matching public key
    /// in `plugins.updater.pubkey`, and the signatures are only worth shipping when the app
    /// registers tauri-plugin-updater to verify them. Half of that setup either fails the build
    /// or publishes signatures (and a release manifest) that no client ever reads.
    #[test]
    fn updater_artifacts_are_all_or_nothing() {
        let conf = tauri_conf();
        let artifacts = match &conf["bundle"]["createUpdaterArtifacts"] {
            serde_json::Value::Null => false,
            serde_json::Value::Bool(enabled) => *enabled,
            serde_json::Value::String(mode) => {
                assert_eq!(mode, "v1Compatible", "the only string value Tauri accepts");
                true
            }
            other => panic!("bundle.createUpdaterArtifacts must be a bool or \"v1Compatible\", got {other}"),
        };
        let pubkey = conf["plugins"]["updater"]["pubkey"].as_str().is_some_and(|key| !key.trim().is_empty());
        let plugin_dependency = CARGO_TOML.lines().map(str::trim_start).any(|line| line.starts_with("tauri-plugin-updater"));
        assert_eq!(pubkey, artifacts, "plugins.updater.pubkey is set exactly when bundle.createUpdaterArtifacts is enabled");
        assert_eq!(plugin_dependency, artifacts, "tauri-plugin-updater is a dependency exactly when bundle.createUpdaterArtifacts is enabled");
    }
}
