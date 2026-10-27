use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

pub const OS_KEY: &str = "macos";
pub const CSV_CRLF_DEFAULT: bool = false;

pub fn config_dir() -> PathBuf {
    dirs::config_dir().unwrap_or_else(|| PathBuf::from(".")).join("BulkDB")
}

pub fn ssh_path() -> PathBuf {
    let builtin = PathBuf::from("/usr/bin/ssh");
    if builtin.exists() {
        return builtin;
    }
    PathBuf::from("ssh")
}

pub fn library_file_name(base: &str) -> String {
    if base.ends_with(".dylib") {
        base.to_string()
    } else if base.starts_with("lib") {
        format!("{base}.dylib")
    } else {
        format!("lib{base}.dylib")
    }
}

pub fn key_file_permissions_too_open(path: &Path) -> bool {
    match std::fs::metadata(path) {
        Ok(m) => m.permissions().mode() & 0o077 != 0,
        Err(_) => false,
    }
}

pub fn default_library_dirs() -> Vec<PathBuf> {
    let mut dirs = vec![PathBuf::from("/opt/homebrew/lib"), PathBuf::from("/usr/local/lib"), PathBuf::from("/usr/lib")];
    if let Ok(p) = std::env::var("DYLD_LIBRARY_PATH") {
        dirs.extend(std::env::split_paths(&p));
    }
    dirs
}
