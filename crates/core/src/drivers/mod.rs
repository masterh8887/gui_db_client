//! Driver registry (`drivers/registry.json`) and runtime detection of vendor libraries.
//!
//! * Tier 1 (`builtin`): always available when compiled in.
//! * Tier 2 (`vendor-client`, `odbc`): try `libloading` on `requires.<os>.library` across the
//!   probe paths; failure -> `available = false` with an install hint.
//! * Tier 3 (`plugin`): reserved in the schema, not implemented in v1.
//!
//! The user may override the bundled registry with `<config dir>/drivers/registry.json`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::RwLock;

use serde::{Deserialize, Serialize};

use crate::error::{CoreError, Result};
use crate::types::{DriverInfo, DriverTier};

pub const BUNDLED_REGISTRY: &str = include_str!("../../../../drivers/registry.json");

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Requires {
    pub library: String,
    #[serde(default)]
    pub probe: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Install {
    #[serde(default)]
    pub label: Option<String>,
    #[serde(flatten)]
    pub links: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DriverDef {
    pub id: String,
    pub name: String,
    pub tier: DriverTier,
    pub source: String,
    #[serde(default)]
    pub requires: BTreeMap<String, Requires>,
    #[serde(default)]
    pub install: Option<Install>,
    #[serde(default)]
    pub fields: Vec<String>,
    #[serde(default)]
    pub defaults: BTreeMap<String, String>,
    #[serde(default)]
    pub session_opts: Vec<String>,
}

pub struct DriverRegistry {
    defs: Vec<DriverDef>,
    infos: RwLock<Vec<DriverInfo>>,
}

impl DriverRegistry {
    /// Load the bundled registry, overridden by `user_override` when it exists and parses.
    pub fn load(user_override: Option<&Path>) -> Result<Self> {
        let mut defs: Vec<DriverDef> = serde_json::from_str(BUNDLED_REGISTRY)?;
        if let Some(p) = user_override {
            if p.exists() {
                match std::fs::read_to_string(p).map_err(CoreError::from).and_then(|s| serde_json::from_str::<Vec<DriverDef>>(&s).map_err(CoreError::from)) {
                    Ok(user) => {
                        for d in user {
                            if let Some(slot) = defs.iter_mut().find(|x| x.id == d.id) {
                                *slot = d;
                            } else {
                                defs.push(d);
                            }
                        }
                    }
                    Err(e) => tracing::warn!("ignoring user driver registry {}: {e}", p.display()),
                }
            }
        }
        let reg = DriverRegistry { defs, infos: RwLock::new(Vec::new()) };
        reg.rescan();
        Ok(reg)
    }

    pub fn defs(&self) -> &[DriverDef] {
        &self.defs
    }

    pub fn def(&self, id: &str) -> Option<&DriverDef> {
        self.defs.iter().find(|d| d.id == id)
    }

    pub fn list(&self) -> Vec<DriverInfo> {
        self.infos.read().unwrap().clone()
    }

    pub fn get(&self, id: &str) -> Option<DriverInfo> {
        self.infos.read().unwrap().iter().find(|d| d.id == id).cloned()
    }

    /// Re-detect every driver (no restart). Returns the fresh list.
    pub fn rescan(&self) -> Vec<DriverInfo> {
        let infos: Vec<DriverInfo> = self.defs.iter().map(detect).collect();
        *self.infos.write().unwrap() = infos.clone();
        infos
    }
}

/// Whether a registry `install.<os>` entry is a web page the UI may open.
///
/// Only absolute `https://` URLs qualify: that is the opener capability's scope in the desktop
/// shell, so anything else handed to it would be rejected. Every other entry (for example
/// `brew install unixodbc`) is an instruction and is shown as text instead.
pub fn is_install_link(entry: &str) -> bool {
    const SCHEME: &str = "https://";
    let entry = entry.trim();
    entry.len() > SCHEME.len() && entry.get(..SCHEME.len()).is_some_and(|p| p.eq_ignore_ascii_case(SCHEME)) && !entry.chars().any(char::is_whitespace)
}

/// The `install.<os>` entry for this OS, split into (link to open, instruction to show).
fn install_step(def: &DriverDef, os: &str) -> (Option<String>, Option<String>) {
    let entry = def.install.as_ref().and_then(|i| i.links.get(os)).map(|e| e.trim()).filter(|e| !e.is_empty());
    match entry {
        Some(e) if is_install_link(e) => (Some(e.to_string()), None),
        Some(e) => (None, Some(e.to_string())),
        None => (None, None),
    }
}

/// Detect one driver on this machine.
///
/// `install_url` is only ever an `https://` link (see [`is_install_link`]); a non-link install
/// entry such as a package-manager command ends up in `install_hint`, which is plain text.
pub fn detect(def: &DriverDef) -> DriverInfo {
    let os = crate::platform::os_key();
    let (install_url, install_command) = install_step(def, os);
    let mut info = DriverInfo {
        id: def.id.clone(),
        name: def.name.clone(),
        tier: def.tier,
        available: false,
        detail: None,
        install_label: def.install.as_ref().and_then(|i| i.label.clone()),
        install_url,
        install_hint: install_command.clone(),
        fields: def.fields.clone(),
        defaults: def.defaults.clone(),
        session_opts: def.session_opts.clone(),
        odbc_drivers: vec![],
        library_path: None,
    };
    if !crate::source::is_compiled(&def.source) {
        info.detail = Some("not included in this build".into());
        return info;
    }
    match def.tier {
        DriverTier::Builtin => info.available = true,
        DriverTier::Plugin => info.detail = Some("plugins are reserved for a later version".into()),
        DriverTier::VendorClient | DriverTier::Odbc => match def.requires.get(os) {
            None => {
                // No library requirement on this OS (e.g. ODBC on Windows): available.
                info.available = true;
            }
            Some(req) => match find_library(req) {
                Ok(path) => {
                    info.available = true;
                    info.library_path = Some(path.display().to_string());
                }
                Err(detail) => {
                    info.detail = Some(detail);
                    let label = info.install_label.as_deref().unwrap_or("vendor client");
                    let mut hint = format!("{label} ({})", crate::platform::arch());
                    if let Some(cmd) = &install_command {
                        hint.push_str(": ");
                        hint.push_str(cmd);
                    }
                    info.install_hint = Some(hint);
                }
            },
        },
    }
    if def.tier == DriverTier::Odbc && info.available {
        info.odbc_drivers = odbc_driver_names();
    }
    info
}

/// Try to dlopen the required library from the probe paths, then from the default search path.
pub fn find_library(req: &Requires) -> std::result::Result<PathBuf, String> {
    let file = crate::platform::library_file_name(&req.library);
    let mut candidates: Vec<PathBuf> = Vec::new();
    for pat in &req.probe {
        for dir in crate::platform::expand_probe_path(pat) {
            // PATH-like entries expand to many dirs separated by the platform separator.
            for d in std::env::split_paths(dir.as_os_str()) {
                candidates.push(d.join(&file));
            }
        }
    }
    for d in crate::platform::default_library_dirs() {
        candidates.push(d.join(&file));
    }
    let mut last_err = String::new();
    for c in &candidates {
        if c.exists() {
            // SAFETY: loading a vendor client library only runs its initialisers; we never call
            // into it here. This is the documented detection method.
            match unsafe { libloading::Library::new(c) } {
                Ok(_) => return Ok(c.clone()),
                Err(e) => last_err = format!("{}: {e}", c.display()),
            }
        }
    }
    // Finally, let the loader search by bare name.
    match unsafe { libloading::Library::new(&file) } {
        Ok(_) => Ok(PathBuf::from(&file)),
        Err(e) => {
            if last_err.is_empty() {
                Err(format!("{file} not found ({e}); arch {}", crate::platform::arch()))
            } else {
                Err(format!("{last_err}; arch {}", crate::platform::arch()))
            }
        }
    }
}

/// Installed ODBC driver names from the driver manager (empty where unsupported).
pub fn odbc_driver_names() -> Vec<String> {
    #[cfg(has_odbc)]
    {
        crate::source::odbc::installed_drivers().unwrap_or_default()
    }
    #[cfg(not(has_odbc))]
    {
        vec![]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_registry_parses() {
        let defs: Vec<DriverDef> = serde_json::from_str(BUNDLED_REGISTRY).unwrap();
        assert!(defs.iter().any(|d| d.id == "postgres" && d.tier == DriverTier::Builtin));
        assert!(defs.iter().any(|d| d.id == "oracle" && d.tier == DriverTier::VendorClient));
    }

    /// A URL-looking install entry that is not `https://` would silently degrade to text, so
    /// the bundled registry must only use `https://` links.
    #[test]
    fn bundled_install_links_are_https() {
        let defs: Vec<DriverDef> = serde_json::from_str(BUNDLED_REGISTRY).unwrap();
        for d in &defs {
            for (os, entry) in d.install.iter().flat_map(|i| i.links.iter()) {
                if entry.contains("://") {
                    assert!(is_install_link(entry), "{} install.{os} is not an https link: {entry}", d.id);
                }
            }
        }
        let odbc = defs.iter().find(|d| d.id == "odbc").unwrap();
        assert_eq!(install_step(odbc, "macos"), (None, Some("brew install unixodbc".to_string())));
        let oracle = defs.iter().find(|d| d.id == "oracle").unwrap();
        let (url, cmd) = install_step(oracle, "macos");
        assert!(url.is_some_and(|u| u.starts_with("https://")));
        assert_eq!(cmd, None);
    }

    #[test]
    fn install_link_classification() {
        assert!(is_install_link("https://example.com/download"));
        assert!(is_install_link("  HTTPS://example.com/x  "));
        assert!(!is_install_link("https://"));
        assert!(!is_install_link("http://example.com"));
        assert!(!is_install_link("brew install unixodbc"));
        assert!(!is_install_link("https://example.com/a b"));
        assert!(!is_install_link("file:///etc/passwd"));
        assert!(!is_install_link(""));
    }

    /// A def whose `install` entry is the same on every OS, so the test is OS-independent.
    fn def_with_install(tier: DriverTier, entry: &str, requires: bool) -> DriverDef {
        let oses = ["windows", "macos", "linux"];
        let req = Requires { library: "bulkdb-test-library-that-does-not-exist".into(), probe: vec![] };
        DriverDef {
            id: "test".into(),
            name: "Test".into(),
            tier,
            // Always compiled in, so detection reaches the tier logic.
            source: "sqlite".into(),
            requires: if requires { oses.iter().map(|o| (o.to_string(), req.clone())).collect() } else { BTreeMap::new() },
            install: Some(Install { label: Some("Test Client".into()), links: oses.iter().map(|o| (o.to_string(), entry.to_string())).collect() }),
            fields: vec![],
            defaults: BTreeMap::new(),
            session_opts: vec![],
        }
    }

    #[test]
    fn detect_routes_install_commands_to_hint() {
        let info = detect(&def_with_install(DriverTier::Builtin, "brew install unixodbc", false));
        assert!(info.available);
        assert_eq!(info.install_url, None);
        assert_eq!(info.install_hint.as_deref(), Some("brew install unixodbc"));

        let info = detect(&def_with_install(DriverTier::Builtin, "https://example.com/client", false));
        assert_eq!(info.install_url.as_deref(), Some("https://example.com/client"));
        assert_eq!(info.install_hint, None);
    }

    #[test]
    fn detect_missing_library_keeps_install_command_in_hint() {
        let info = detect(&def_with_install(DriverTier::Odbc, "brew install unixodbc", true));
        assert!(!info.available);
        assert!(info.detail.is_some());
        assert_eq!(info.install_url, None);
        let hint = info.install_hint.unwrap();
        assert!(hint.starts_with("Test Client ("), "{hint}");
        assert!(hint.ends_with("): brew install unixodbc"), "{hint}");

        let info = detect(&def_with_install(DriverTier::VendorClient, "https://example.com/client", true));
        assert!(!info.available);
        assert_eq!(info.install_url.as_deref(), Some("https://example.com/client"));
        assert_eq!(info.install_hint, Some(format!("Test Client ({})", crate::platform::arch())));
    }
}
