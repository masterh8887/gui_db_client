//! Script-mode helpers: runtime detection, install commands, env-var injection.
//!
//! All functions here are pure (no I/O except process probing) so they can be called from both
//! the Tauri shell and the CLI without pulling in any UI dependencies. The actual process
//! spawning and event streaming live in the Tauri commands layer.

use std::collections::HashMap;

use crate::types::{Connection, RuntimeInfo, ScriptRuntime};

/// The executable names to try for `runtime`, in order. On Windows the Python launcher `py` and a
/// real `python` come before `python3`, because `python3` is usually the Microsoft Store alias
/// (`...\WindowsApps\python3.exe`), which is not a working interpreter.
pub fn runtime_candidates(runtime: ScriptRuntime) -> &'static [&'static str] {
    match runtime {
        ScriptRuntime::Node => &["node"],
        ScriptRuntime::Python => {
            #[cfg(windows)]
            {
                &["py", "python", "python3"]
            }
            #[cfg(not(windows))]
            {
                &["python3", "python"]
            }
        }
        ScriptRuntime::Bash => &["bash"],
    }
}

/// A real `--version` line prints a dotted number. The Windows Store `python3` alias prints just
/// `Python` (to stderr, exit 9009) and a missing executable prints nothing, so both are rejected.
fn looks_like_version(s: &str) -> bool {
    s.chars().any(|c| c.is_ascii_digit())
}

/// Probe whether `runtime` is installed and return its version + path, or `None` if not found.
///
/// Tries the executable names on PATH first, then — for Python — well-known install locations, so
/// a GUI app whose PATH was captured before Python was installed still finds it.
pub fn detect_runtime(runtime: ScriptRuntime) -> Option<RuntimeInfo> {
    if let Some(info) = runtime_candidates(runtime).iter().find_map(|exe| probe_exe(runtime, exe)) {
        return Some(info);
    }
    if runtime == ScriptRuntime::Python {
        return crate::platform::python_exe_candidates().iter().find_map(|path| probe_exe(runtime, &path.to_string_lossy()));
    }
    None
}

fn probe_exe(runtime: ScriptRuntime, exe: &str) -> Option<RuntimeInfo> {
    let out = std::process::Command::new(exe).arg("--version").output().ok()?;
    // Node / Python print to stdout; some tools (and older Python) use stderr for --version.
    let raw = if !out.stdout.is_empty() { &out.stdout } else { &out.stderr };
    let version = String::from_utf8_lossy(raw).trim().to_string();
    if !looks_like_version(&version) {
        return None;
    }
    // An absolute path (a known-location probe) is already the executable to run; a bare name is
    // resolved against PATH.
    let path = if std::path::Path::new(exe).is_absolute() { exe.to_string() } else { which_exe(exe).unwrap_or_else(|| exe.to_string()) };
    Some(RuntimeInfo { runtime, version, path })
}

/// The interpreter to actually spawn: the resolved path of a detected runtime, so a script runs
/// under the same interpreter the version badge shows (never the Store alias). Falls back to the
/// first candidate name when nothing is detected, so the spawn fails with a clear error.
pub fn runtime_exe(runtime: ScriptRuntime) -> String {
    detect_runtime(runtime).map(|info| info.path).unwrap_or_else(|| runtime_candidates(runtime)[0].to_string())
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
        // winget has no evergreen "Python.Python.3"; the ids are version-specific (…3.12, …3.13).
        let (id, _ver) = match runtime {
            ScriptRuntime::Node => ("OpenJS.NodeJS.LTS", ""),
            ScriptRuntime::Python => ("Python.Python.3.13", ""),
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

/// The Python helper + launcher. The shell runs `python <this> <script.py>`: it defines the
/// global `bulkdb` (the Node helper's counterpart) and then execs the user's script as `__main__`.
pub fn python_helper_py() -> &'static str {
    include_str!("python_helper.py")
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

    /// Only a real `--version` line (with a dotted number) counts; the Windows Store `python3`
    /// alias prints just "Python" and must be rejected.
    #[test]
    fn version_lines_need_a_digit() {
        assert!(looks_like_version("Python 3.14.10"));
        assert!(looks_like_version("v26.8.1"));
        assert!(looks_like_version("GNU bash, version 5.2.21(1)-release"));
        assert!(!looks_like_version("Python")); // the Store alias's output
        assert!(!looks_like_version(""));
    }

    /// On Windows the Python launcher `py` is tried before `python3` (the Store alias).
    #[test]
    fn windows_prefers_py_over_store_alias() {
        let python = runtime_candidates(ScriptRuntime::Python);
        if cfg!(windows) {
            assert_eq!(python.first(), Some(&"py"));
            assert!(python.iter().position(|e| *e == "py").unwrap() < python.iter().position(|e| *e == "python3").unwrap());
        } else {
            assert_eq!(python.first(), Some(&"python3"));
        }
    }

    /// Whatever this machine has, a detected runtime always reports a version that looks real and
    /// an executable to run it with.
    #[test]
    fn detected_runtime_is_consistent() {
        for runtime in [ScriptRuntime::Node, ScriptRuntime::Python, ScriptRuntime::Bash] {
            if let Some(info) = detect_runtime(runtime) {
                assert!(looks_like_version(&info.version), "{runtime:?}: {:?}", info.version);
                assert!(!runtime_exe(runtime).is_empty());
            }
        }
    }

    /// The known-location fallback lists only real files, and each one probes as a valid Python —
    /// so a GUI app whose PATH predates the Python install still detects it.
    #[test]
    fn python_known_locations_are_real_interpreters() {
        for path in crate::platform::python_exe_candidates() {
            assert!(path.is_file(), "{}", path.display());
            let info = probe_exe(ScriptRuntime::Python, &path.to_string_lossy());
            assert!(info.is_some(), "known location did not probe as Python: {}", path.display());
            assert_eq!(info.unwrap().path, path.to_string_lossy());
        }
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
