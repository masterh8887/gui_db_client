//! The only module allowed to contain `#[cfg(target_os = ...)]`. Everything platform specific
//! (paths, ssh binary, vendor library search, default line endings) is answered here.

use std::path::PathBuf;

#[cfg(target_os = "windows")]
mod windows;
#[cfg(target_os = "windows")]
use windows as imp;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
use macos as imp;

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
mod linux;
#[cfg(not(any(target_os = "windows", target_os = "macos")))]
use linux as imp;

/// `windows`, `macos` or `linux` — the key used in `drivers/registry.json`.
pub fn os_key() -> &'static str {
    imp::OS_KEY
}

/// `x86_64` or `aarch64`, for driver architecture diagnostics.
pub fn arch() -> &'static str {
    std::env::consts::ARCH
}

/// `%APPDATA%\BulkDB`, `~/Library/Application Support/BulkDB`, `~/.config/BulkDB`.
/// Overridable with `BULKDB_CONFIG_DIR` (tests, portable installs).
pub fn config_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("BULKDB_CONFIG_DIR") {
        if !dir.is_empty() {
            return PathBuf::from(dir);
        }
    }
    imp::config_dir()
}

pub fn log_dir() -> PathBuf {
    config_dir().join("logs")
}

/// Path of the OS `ssh` binary.
pub fn ssh_path() -> PathBuf {
    imp::ssh_path()
}

/// Default CSV line ending: CRLF on Windows, LF elsewhere.
pub fn csv_crlf_default() -> bool {
    imp::CSV_CRLF_DEFAULT
}

/// Shared-library file name for a vendor client, e.g. `oci.dll` / `libclntsh.dylib`.
pub fn library_file_name(base: &str) -> String {
    imp::library_file_name(base)
}

/// Expand `%VAR%` / `$VAR` / `~` in a registry probe path and glob `*`.
pub fn expand_probe_path(pattern: &str) -> Vec<PathBuf> {
    let expanded = expand_env(pattern);
    if expanded.contains('*') {
        match glob::glob(&expanded) {
            Ok(paths) => paths.filter_map(|p| p.ok()).collect(),
            Err(_) => vec![],
        }
    } else {
        vec![PathBuf::from(expanded)]
    }
}

fn expand_env(input: &str) -> String {
    let mut out = input.to_string();
    if let Some(home) = dirs::home_dir() {
        if out.starts_with('~') {
            out = format!("{}{}", home.display(), &out[1..]);
        }
    }
    // %VAR%
    let re = regex::Regex::new(r"%([A-Za-z0-9_]+)%").unwrap();
    out = re.replace_all(&out, |caps: &regex::Captures| std::env::var(&caps[1]).unwrap_or_default()).to_string();
    // $VAR / ${VAR}
    let re2 = regex::Regex::new(r"\$\{?([A-Za-z0-9_]+)\}?").unwrap();
    out = re2.replace_all(&out, |caps: &regex::Captures| std::env::var(&caps[1]).unwrap_or_default()).to_string();
    out
}

/// Whether a private key file is readable by other users (warn in the UI).
pub fn key_file_permissions_too_open(path: &std::path::Path) -> bool {
    imp::key_file_permissions_too_open(path)
}

/// Directories where the ODBC driver manager / vendor libraries are usually found.
pub fn default_library_dirs() -> Vec<PathBuf> {
    imp::default_library_dirs()
}

/// The command that starts another BulkDB process from the running executable `exe`
/// (docs/db-copy.md section 1.2): `exe` itself with no arguments, on every OS.
///
/// macOS included: `exe` is the bundle's `Contents/MacOS/<name>` binary, and running it directly
/// starts a separate process that **inherits this process's environment** (`BULKDB_CONFIG_DIR`
/// included). `/usr/bin/open -n <bundle>` would instead let LaunchServices start the sibling with
/// launchd's environment, so a BulkDB launched with a non-default `BULKDB_CONFIG_DIR` (portable
/// and test setups) would spawn its sibling on the default config directory, and the two would
/// share no connections, settings, history, run claims or instance registry.
pub fn instance_command(exe: &std::path::Path) -> (PathBuf, Vec<String>) {
    instance_command_for(exe)
}

/// [`instance_command`]'s pure core (unit-tested). Every OS execs the binary directly; see
/// [`instance_command`] for why macOS no longer goes through `open -n`.
fn instance_command_for(exe: &std::path::Path) -> (PathBuf, Vec<String>) {
    (exe.to_path_buf(), Vec::new())
}

/// Start another BulkDB process, detached from this one (it outlives it and shares nothing but
/// the files of section 1.2). The environment is inherited, `BULKDB_CONFIG_DIR` included (every
/// OS execs the binary directly; see [`instance_command`]).
pub fn spawn_instance() -> crate::error::Result<()> {
    let exe = std::env::current_exe()?;
    let (program, args) = instance_command(&exe);
    let mut cmd = std::process::Command::new(&program);
    cmd.args(&args).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    imp::detach(&mut cmd);
    let mut child = cmd.spawn().map_err(|e| crate::error::CoreError::Io(format!("cannot start {}: {e}", program.display())))?;
    tracing::info!(program = %program.display(), pid = child.id(), "started another BulkDB instance");
    // Reap the child when it exits (`open -n` returns at once) so no zombie is left behind.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn instance_command_execs_the_binary_directly() {
        // Every OS runs the executable itself with no arguments, so the sibling inherits this
        // process's environment (BULKDB_CONFIG_DIR included). On macOS that is the bundle's own
        // `Contents/MacOS/<name>` binary: deliberately not `/usr/bin/open -n <bundle>`, which
        // would drop the environment through LaunchServices.
        let mac = Path::new("/Applications/BulkDB.app/Contents/MacOS/BulkDB");
        assert_eq!(instance_command_for(mac), (mac.to_path_buf(), vec![]));
        let dev = Path::new("/Users/me/bulkdb/target/debug/bulkdb-desktop");
        assert_eq!(instance_command_for(dev), (dev.to_path_buf(), vec![]));
        let win = Path::new(r"C:\Program Files\BulkDB\BulkDB.exe");
        assert_eq!(instance_command_for(win), (win.to_path_buf(), vec![]));
        assert_eq!(instance_command_for(Path::new("/usr/bin/bulkdb")), (PathBuf::from("/usr/bin/bulkdb"), vec![]));
        // The public entry point answers through the same rule.
        assert_eq!(instance_command(win), instance_command_for(win));
    }
}
