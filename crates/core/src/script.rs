//! Script-mode helpers: runtime detection, install commands, env-var injection.
//!
//! All functions here are pure (no I/O except process probing) so they can be called from both
//! the Tauri shell and the CLI without pulling in any UI dependencies. The actual process
//! spawning and event streaming live in the Tauri commands layer.

use std::collections::HashMap;

use crate::types::{Connection, RuntimeInfo, ScriptRuntime};

/// Probe whether `runtime` is installed and return its version + path, or `None` if not found.
pub fn detect_runtime(runtime: ScriptRuntime) -> Option<RuntimeInfo> {
    let candidates: &[&str] = match runtime {
        ScriptRuntime::Node => &["node"],
        ScriptRuntime::Python => &["python3", "python"],
        ScriptRuntime::Bash => &["bash"],
    };
    for exe in candidates {
        if let Some(info) = probe_exe(runtime, exe) {
            return Some(info);
        }
    }
    None
}

fn probe_exe(runtime: ScriptRuntime, exe: &str) -> Option<RuntimeInfo> {
    let out = std::process::Command::new(exe).arg("--version").output().ok()?;
    if !out.status.success() && out.stdout.is_empty() && out.stderr.is_empty() {
        return None;
    }
    // Node / Python print to stdout; some tools use stderr for --version.
    let raw = if out.stdout.is_empty() { out.stderr } else { out.stdout };
    let version = String::from_utf8_lossy(&raw).trim().to_string();
    if version.is_empty() {
        return None;
    }
    let path = which_exe(exe).unwrap_or_else(|| exe.to_string());
    Some(RuntimeInfo { runtime, version, path })
}

fn which_exe(name: &str) -> Option<String> {
    #[cfg(windows)]
    {
        std::process::Command::new("where")
            .arg(name)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).lines().next().unwrap_or("").trim().to_string())
            .filter(|s| !s.is_empty())
    }
    #[cfg(not(windows))]
    {
        std::process::Command::new("which")
            .arg(name)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .filter(|s| !s.is_empty())
    }
}

/// OS-specific command + args to install `runtime` using the system package manager.
pub fn install_command(runtime: ScriptRuntime) -> Vec<String> {
    #[cfg(windows)]
    {
        let (id, _ver) = match runtime {
            ScriptRuntime::Node => ("OpenJS.NodeJS.LTS", ""),
            ScriptRuntime::Python => ("Python.Python.3", ""),
            ScriptRuntime::Bash => ("Git.Git", ""),
        };
        vec!["winget".into(), "install".into(), "--id".into(), id.into(), "-e".into(), "--accept-package-agreements".into(), "--accept-source-agreements".into()]
    }
    #[cfg(not(windows))]
    {
        let pkg = match runtime {
            ScriptRuntime::Node => "node",
            ScriptRuntime::Python => "python3",
            ScriptRuntime::Bash => "bash",
        };
        vec!["brew".into(), "install".into(), pkg.into()]
    }
}

/// Executable + args to run `script_path` with the given runtime.
pub fn runtime_args(runtime: ScriptRuntime) -> (&'static str, &'static [&'static str]) {
    match runtime {
        ScriptRuntime::Node => ("node", &[]),
        #[cfg(windows)]
        ScriptRuntime::Python => ("python", &[]),
        #[cfg(not(windows))]
        ScriptRuntime::Python => ("python3", &[]),
        ScriptRuntime::Bash => ("bash", &[]),
    }
}

/// Build a set of `BULKDB_*` environment variables that inject the connection's details into a
/// child script process. The password is passed in separately (retrieved from the keyring by the
/// caller) so this function stays pure.
pub fn connection_env(conn: &Connection, password: Option<&str>) -> HashMap<String, String> {
    let mut env: HashMap<String, String> = HashMap::new();
    env.insert("BULKDB_DRIVER".into(), conn.driver.clone());
    env.insert("BULKDB_CONN_ID".into(), conn.id.clone());
    env.insert("BULKDB_CONN_NAME".into(), conn.name.clone());
    if let Some(h) = &conn.host {
        env.insert("BULKDB_HOST".into(), h.clone());
    }
    if let Some(p) = conn.port {
        env.insert("BULKDB_PORT".into(), p.to_string());
    }
    if let Some(d) = &conn.database {
        env.insert("BULKDB_DATABASE".into(), d.clone());
    }
    if let Some(u) = &conn.user {
        env.insert("BULKDB_USER".into(), u.clone());
    }
    if let Some(pw) = password {
        env.insert("BULKDB_PASSWORD".into(), pw.to_string());
    }
    for (k, v) in &conn.fields {
        let key = format!("BULKDB_{}", k.to_ascii_uppercase().replace('-', "_"));
        env.insert(key, v.clone());
    }
    env
}
