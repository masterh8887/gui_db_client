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
