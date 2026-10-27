use std::path::{Path, PathBuf};

pub const OS_KEY: &str = "windows";
pub const CSV_CRLF_DEFAULT: bool = true;

pub fn config_dir() -> PathBuf {
    dirs::config_dir().unwrap_or_else(|| PathBuf::from(".")).join("BulkDB")
}

pub fn ssh_path() -> PathBuf {
    let system_root = std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".into());
    let builtin = PathBuf::from(system_root).join("System32").join("OpenSSH").join("ssh.exe");
    if builtin.exists() {
        return builtin;
    }
    PathBuf::from("ssh.exe")
}

pub fn library_file_name(base: &str) -> String {
    if base.to_ascii_lowercase().ends_with(".dll") {
        base.to_string()
    } else {
        format!("{base}.dll")
    }
}

pub fn key_file_permissions_too_open(_path: &Path) -> bool {
    // OpenSSH for Windows checks ACLs itself and refuses the key; we cannot do better cheaply.
    false
}

pub fn default_library_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Ok(p) = std::env::var("PATH") {
        dirs.extend(std::env::split_paths(&p));
    }
    dirs
}
