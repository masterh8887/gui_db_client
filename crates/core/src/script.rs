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

/// The Node.js helper module injected via `node --require` into every script-tab run.
/// Provides `global.bulkdb` with auto-connection and a Prisma-style query builder.
pub fn node_helper_js() -> &'static str {
    include_str!("node_helper.js")
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

/// Variable that lists the saved connections for a script run (a JSON array, see [`script_env`]).
pub const CONNECTIONS_ENV: &str = "BULKDB_CONNECTIONS";

/// Whether `script` may open `conn`: it is the tab's own connection, or the source text contains
/// its name or id literally. Secrets of every other connection stay in the credential store.
fn script_mentions(script: &str, conn: &Connection) -> bool {
    [&conn.name, &conn.id].into_iter().any(|needle| !needle.is_empty() && script.contains(needle.as_str()))
}

/// Environment of one script run: the tab's connection as flat `BULKDB_*` variables
/// ([`connection_env`]) plus [`CONNECTIONS_ENV`], a JSON array of every saved connection so a
/// script can open several by name (`bulkdb.db('name')` in the Node helper).
///
/// A connection the script may open ([`script_mentions`]) carries its details, its password and
/// `"granted": true`; the others are listed by `id`, `name` and `driver` only, so the helper can
/// tell "unknown name" from "not handed over". `password_of` is asked for granted connections only.
pub fn script_env(default: &Connection, all: &[Connection], script: &str, password_of: impl Fn(&Connection) -> Option<String>) -> HashMap<String, String> {
    let default_password = password_of(default);
    let mut env = connection_env(default, default_password.as_deref());
    let list: Vec<serde_json::Value> = all
        .iter()
        .map(|c| {
            let is_default = c.id == default.id;
            if !is_default && !script_mentions(script, c) {
                return serde_json::json!({ "id": c.id, "name": c.name, "driver": c.driver });
            }
            let password = if is_default { default_password.clone() } else { password_of(c) };
            serde_json::json!({
                "id": c.id,
                "name": c.name,
                "driver": c.driver,
                "host": c.host,
                "port": c.port,
                "database": c.database,
                "user": c.user,
                "password": password,
                "fields": c.fields,
                "ssh": c.ssh.as_ref().is_some_and(|s| s.enabled),
                "granted": true,
            })
        })
        .collect();
    env.insert(CONNECTIONS_ENV.into(), serde_json::Value::Array(list).to_string());
    env
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

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;

    fn conn(id: &str, name: &str, driver: &str) -> Connection {
        Connection { id: id.into(), name: name.into(), driver: driver.into(), host: Some(format!("{name}.internal")), port: Some(5432), ..Default::default() }
    }

    fn listed(env: &HashMap<String, String>) -> Vec<serde_json::Value> {
        serde_json::from_str(&env[CONNECTIONS_ENV]).unwrap()
    }

    /// The tab's connection and the ones the source names get their details and passwords. The
    /// rest are listed by name only, and their secrets are never even read.
    #[test]
    fn script_env_hands_over_only_the_connections_the_script_names() {
        let all = [conn("c1", "kpia1", "mysql"), conn("c2", "kpia2", "postgres"), conn("c3", "prod", "postgres")];
        let asked = RefCell::new(Vec::new());
        let env = script_env(&all[0], &all, "const b = bulkdb.db('kpia2');", |c| {
            asked.borrow_mut().push(c.id.clone());
            Some(format!("pw-{}", c.id))
        });
        assert_eq!(env["BULKDB_CONN_ID"], "c1");
        assert_eq!(env["BULKDB_PASSWORD"], "pw-c1");
        let list = listed(&env);
        assert_eq!(list.len(), 3);
        assert_eq!((&list[0]["name"], &list[0]["granted"], &list[0]["password"]), (&"kpia1".into(), &true.into(), &"pw-c1".into()));
        assert_eq!((&list[1]["name"], &list[1]["granted"], &list[1]["password"]), (&"kpia2".into(), &true.into(), &"pw-c2".into()));
        assert_eq!((&list[1]["host"], &list[1]["port"], &list[1]["ssh"]), (&"kpia2.internal".into(), &5432.into(), &false.into()));
        assert_eq!(list[2], serde_json::json!({ "id": "c3", "name": "prod", "driver": "postgres" }));
        assert_eq!(*asked.borrow(), ["c1", "c2"]);
        assert!(!env[CONNECTIONS_ENV].contains("pw-c3"));
    }

    /// An id works like a name; an empty name never matches (every text "contains" it).
    #[test]
    fn script_env_matches_ids_but_not_empty_names() {
        let all = [conn("c1", "tab", "sqlite"), conn("id-two", "second", "mysql"), conn("c3", "", "mysql")];
        let list = listed(&script_env(&all[0], &all, "bulkdb.db('id-two')", |_| None));
        assert_eq!(list[1]["granted"], true);
        assert!(list[1]["password"].is_null());
        assert!(list[2].get("granted").is_none());
    }

    /// Only an enabled tunnel is reported: the helper refuses those connections.
    #[test]
    fn script_env_reports_enabled_ssh_tunnels() {
        let ssh = |enabled: bool| serde_json::from_value(serde_json::json!({ "enabled": enabled, "host": "bastion", "user": "deploy" })).ok();
        let mut tunnelled = conn("c1", "tunnelled", "postgres");
        tunnelled.ssh = ssh(true);
        let mut direct = conn("c2", "direct", "postgres");
        direct.ssh = ssh(false);
        assert!(tunnelled.ssh.is_some() && direct.ssh.is_some());
        let all = [tunnelled, direct];
        let list = listed(&script_env(&all[0], &all, "direct", |_| None));
        assert_eq!((&list[0]["ssh"], &list[1]["ssh"]), (&true.into(), &false.into()));
    }
}
