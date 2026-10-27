//! Connection selection shared by every database command.
//!
//! * `--conn <name|id>` picks a saved connection.
//! * `--driver ...` builds an ad-hoc [`Connection`]. It is saved through the facade
//!   (`conn_save`, so the password lands in the credential store like any other connection),
//!   used for the command, and deleted again (`conn_delete`) when the [`ConnHandle`] drops,
//!   unless `--save <name>` asked to keep it.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use anyhow::{anyhow, bail, Context};
use bulk_core::error::CoreError;
use bulk_core::types::{Connection, ConnectionInput, DriverInfo, SshAuth, SshConfig};
use bulk_core::BulkDb;

use crate::args::{AdhocConn, ConnEditArgs, ConnSelect};

/// A resolved connection. Dropping a temporary (ad-hoc, unsaved) handle removes the
/// connection and its secrets from the config again.
pub struct ConnHandle {
    conn: Connection,
    password: Option<String>,
    cleanup: Option<Arc<BulkDb>>,
}

impl ConnHandle {
    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    pub fn id(&self) -> &str {
        &self.conn.id
    }

    /// Password given through `--password-env` (for saved connections that do not store one
    /// it is passed along with the request; ad-hoc connections store it for their lifetime).
    pub fn password(&self) -> Option<&str> {
        self.password.as_deref()
    }
}

impl Drop for ConnHandle {
    fn drop(&mut self) {
        if let Some(db) = self.cleanup.take() {
            if let Err(e) = db.conn_delete(&self.conn.id) {
                eprintln!("warning: could not remove the temporary connection {} ({}): {e}", self.conn.name, self.conn.id);
            } else {
                tracing::debug!("removed temporary connection {}", self.conn.id);
            }
        }
    }
}

/// Resolve `--conn` / `--driver` into a usable connection id.
pub async fn resolve(db: &Arc<BulkDb>, sel: &ConnSelect) -> anyhow::Result<ConnHandle> {
    let password = match &sel.adhoc.password_env {
        Some(var) => Some(password_from_env(var)?),
        None => None,
    };
    if let Some(key) = &sel.conn {
        let conn = find_saved(db, key)?;
        return Ok(ConnHandle { conn, password, cleanup: None });
    }
    let drivers = db.drivers_list();
    if let Some(keep) = &sel.save {
        // A second connection with the same name would make `--conn NAME` ambiguous.
        if let Some(existing) = find_by_name(&db.conn_list(), keep, None) {
            return Err(CoreError::Invalid(format!(
                "a connection named '{}' already exists ({}); use --conn '{}' instead, or change it with `bulk-cli conn edit`",
                existing.name, existing.id, existing.name
            ))
            .into());
        }
    }
    let name = sel.save.clone().unwrap_or_else(|| format!("adhoc-{}-{}", sel.adhoc.driver.as_deref().unwrap_or("db"), std::process::id()));
    let connection = build_connection(&sel.adhoc, &name, &drivers)?;
    let (ssh_password, ssh_key_passphrase) = ssh_secrets_from_env(&sel.adhoc)?;
    let saved = db
        .conn_save(ConnectionInput { connection, password: password.clone(), ssh_password, ssh_key_passphrase, ..Default::default() })
        .context("saving the ad-hoc connection")?;
    let cleanup = if sel.save.is_some() {
        eprintln!("saved connection '{}' ({})", saved.name, saved.id);
        None
    } else {
        Some(db.clone())
    };
    Ok(ConnHandle { conn: saved, password, cleanup })
}

/// Find a saved connection by id, exact name, or unique case-insensitive name.
pub fn find_saved(db: &BulkDb, key: &str) -> anyhow::Result<Connection> {
    let all = db.conn_list();
    if let Some(c) = all.iter().find(|c| c.id == key) {
        return Ok(c.clone());
    }
    let exact: Vec<&Connection> = all.iter().filter(|c| c.name == key).collect();
    match exact.len() {
        1 => return Ok(exact[0].clone()),
        n if n > 1 => bail!("{n} connections are named '{key}'; use the id (see `bulk-cli conn list`)"),
        _ => {}
    }
    let loose: Vec<&Connection> = all.iter().filter(|c| c.name.eq_ignore_ascii_case(key)).collect();
    if loose.len() == 1 {
        return Ok(loose[0].clone());
    }
    let hint = if all.is_empty() {
        " (no saved connections; use `bulk-cli conn add` or --driver)".to_string()
    } else {
        format!("; saved connections: {}", all.iter().map(|c| format!("{} [{}]", c.name, c.id)).collect::<Vec<_>>().join(", "))
    };
    Err(CoreError::NotFound(format!("connection '{key}'{hint}")).into())
}

/// The first saved connection other than `except_id` whose name equals `name`, ignoring case
/// and surrounding blanks: the same names `find_saved` would consider a match, so a new
/// connection with that name would make `--conn NAME` ambiguous.
pub fn find_by_name<'a>(all: &'a [Connection], name: &str, except_id: Option<&str>) -> Option<&'a Connection> {
    let wanted = name.trim();
    all.iter().find(|c| Some(c.id.as_str()) != except_id && c.name.trim().eq_ignore_ascii_case(wanted))
}

/// Look a driver id up in the registry, warning on stderr when it is not usable here.
fn lookup_driver<'a>(drivers: &'a [DriverInfo], driver: &str) -> anyhow::Result<&'a DriverInfo> {
    let info = drivers
        .iter()
        .find(|d| d.id == driver)
        .ok_or_else(|| anyhow!("unknown driver '{driver}'; known drivers: {}", drivers.iter().map(|d| d.id.as_str()).collect::<Vec<_>>().join(", ")))?;
    if !info.available {
        eprintln!(
            "warning: driver '{}' is not available on this machine: {}{}",
            info.id,
            info.detail.as_deref().unwrap_or("not detected"),
            info.install_hint.as_deref().map(|h| format!(" (install: {h})")).unwrap_or_default()
        );
    }
    Ok(info)
}

/// The registry's default port of a driver.
fn default_port(info: &DriverInfo) -> Option<u16> {
    info.defaults.get("port").and_then(|p| p.parse::<u16>().ok())
}

/// Merge `--field KEY=VALUE` flags into `fields` (a `password` field is refused).
fn merge_fields(fields: &mut BTreeMap<String, String>, adhoc: &AdhocConn) -> anyhow::Result<()> {
    for (k, v) in &adhoc.fields {
        if k.eq_ignore_ascii_case("password") {
            bail!("passwords are never passed as fields; use --password-env VAR");
        }
        fields.insert(k.clone(), v.clone());
    }
    if let Some(p) = &adhoc.path {
        fields.insert("path".into(), path_string(p));
    }
    Ok(())
}

fn check_sqlite_path(driver: &str, fields: &BTreeMap<String, String>) -> anyhow::Result<()> {
    if driver == "sqlite" && !fields.get("path").map(|p| !p.is_empty()).unwrap_or(false) {
        bail!("--path FILE is required for sqlite");
    }
    Ok(())
}

/// Build a [`Connection`] from ad-hoc flags. Validates the driver id against the registry,
/// fills the driver's default port and checks the SSH settings; nothing is saved here.
pub fn build_connection(adhoc: &AdhocConn, name: &str, drivers: &[DriverInfo]) -> anyhow::Result<Connection> {
    let driver = adhoc.driver.as_deref().filter(|d| !d.trim().is_empty()).ok_or_else(|| anyhow!("--driver is required (or --conn for a saved connection)"))?;
    let info = lookup_driver(drivers, driver)?;

    let mut fields: BTreeMap<String, String> = BTreeMap::new();
    merge_fields(&mut fields, adhoc)?;
    check_sqlite_path(driver, &fields)?;

    let port = adhoc.port.or_else(|| default_port(info));

    let ssh = match &adhoc.ssh_host {
        Some(host) => {
            let user = adhoc.ssh_user.clone().filter(|u| !u.is_empty()).ok_or_else(|| anyhow!("--ssh-user is required with --ssh-host"))?;
            let auth = adhoc.ssh_auth.map(Into::into).unwrap_or_default();
            let key_path = adhoc.ssh_key.as_deref().map(path_string);
            if let Some(k) = &key_path {
                bulk_core::tunnel::validate_key_path(k)?;
            }
            if auth == bulk_core::types::SshAuth::Key && key_path.is_none() {
                bail!("--ssh-key FILE is required with --ssh-auth key");
            }
            Some(SshConfig { enabled: true, host: host.clone(), port: adhoc.ssh_port.unwrap_or(22), user, auth, key_path, compression: !adhoc.no_ssh_compression })
        }
        None => {
            if adhoc.ssh_user.is_some() || adhoc.ssh_key.is_some() || adhoc.ssh_port.is_some() || adhoc.ssh_auth.is_some() {
                bail!("--ssh-host is required for the other --ssh-* flags");
            }
            None
        }
    };

    Ok(Connection {
        id: String::new(),
        name: name.to_string(),
        driver: driver.to_string(),
        host: adhoc.host.clone(),
        port,
        database: adhoc.database.clone(),
        user: adhoc.user.clone(),
        fields,
        ssh,
        session: Default::default(),
        output_dir: adhoc.output_dir.as_deref().map(path_string),
        save_password: true,
        has_password: false,
        has_ssh_secret: false,
    })
}

/// `Some(value)` for a non-blank flag value, `None` for `""` (clears the setting).
fn non_blank(v: &str) -> Option<String> {
    let t = v.trim();
    (!t.is_empty()).then(|| t.to_string())
}

/// Apply `conn edit` flags to a saved connection. Only the given flags change anything; the
/// id, session defaults and every other field are kept, so `conn_save` updates the
/// connection in place. Nothing is saved here and secrets are handled by the caller.
pub fn apply_edit(existing: &Connection, a: &ConnEditArgs, drivers: &[DriverInfo]) -> anyhow::Result<Connection> {
    let ad = &a.adhoc;
    let mut c = existing.clone();
    if let Some(name) = &a.name {
        c.name = non_blank(name).ok_or_else(|| anyhow!("--name must not be empty"))?;
    }
    if let Some(driver) = &ad.driver {
        let driver = driver.trim();
        if driver.is_empty() {
            bail!("--driver must not be empty");
        }
        let info = lookup_driver(drivers, driver)?;
        if driver != existing.driver {
            // A port that was just the old driver's default follows the new driver's default.
            let old_default = drivers.iter().find(|d| d.id == existing.driver).and_then(default_port);
            if ad.port.is_none() && (c.port.is_none() || c.port == old_default) {
                c.port = default_port(info);
            }
            c.driver = driver.to_string();
        }
    }
    if let Some(h) = &ad.host {
        c.host = non_blank(h);
    }
    if let Some(p) = ad.port {
        c.port = Some(p);
    }
    if let Some(d) = &ad.database {
        c.database = non_blank(d);
    }
    if let Some(u) = &ad.user {
        c.user = non_blank(u);
    }
    for key in &a.unset_fields {
        c.fields.remove(key.trim());
    }
    merge_fields(&mut c.fields, ad)?;
    check_sqlite_path(&c.driver, &c.fields)?;
    if a.clear_output_dir {
        c.output_dir = None;
    } else if let Some(dir) = &ad.output_dir {
        c.output_dir = Some(path_string(dir));
    }
    if a.save_password {
        c.save_password = true;
    } else if a.no_save_password {
        c.save_password = false;
    }
    c.ssh = edit_ssh(existing.ssh.as_ref(), a)?;
    Ok(c)
}

/// The SSH block after `conn edit`: removed with `--no-ssh`, created from `--ssh-host` /
/// `--ssh-user` when there was none, otherwise the existing block with the given flags applied.
fn edit_ssh(existing: Option<&SshConfig>, a: &ConnEditArgs) -> anyhow::Result<Option<SshConfig>> {
    let ad = &a.adhoc;
    if a.no_ssh {
        return Ok(None);
    }
    let touched = ad.ssh_host.is_some()
        || ad.ssh_port.is_some()
        || ad.ssh_user.is_some()
        || ad.ssh_auth.is_some()
        || ad.ssh_key.is_some()
        || ad.no_ssh_compression
        || a.ssh_compression;
    if !touched {
        return Ok(existing.cloned());
    }
    let mut ssh = match existing {
        Some(s) => s.clone(),
        None => {
            let host = ad.ssh_host.as_deref().and_then(non_blank).ok_or_else(|| anyhow!("--ssh-host is required to add an SSH tunnel"))?;
            let user = ad.ssh_user.as_deref().and_then(non_blank).ok_or_else(|| anyhow!("--ssh-user is required with --ssh-host"))?;
            SshConfig { enabled: true, host, port: 22, user, auth: SshAuth::default(), key_path: None, compression: true }
        }
    };
    // Changing tunnel settings means the tunnel is wanted.
    ssh.enabled = true;
    if let Some(h) = &ad.ssh_host {
        ssh.host = non_blank(h).ok_or_else(|| anyhow!("--ssh-host must not be empty (use --no-ssh to remove the tunnel)"))?;
    }
    if let Some(p) = ad.ssh_port {
        ssh.port = p;
    }
    if let Some(u) = &ad.ssh_user {
        ssh.user = non_blank(u).ok_or_else(|| anyhow!("--ssh-user must not be empty"))?;
    }
    if let Some(auth) = ad.ssh_auth {
        ssh.auth = auth.into();
    }
    if let Some(k) = &ad.ssh_key {
        let k = path_string(k);
        bulk_core::tunnel::validate_key_path(&k)?;
        ssh.key_path = Some(k);
    }
    if ad.no_ssh_compression {
        ssh.compression = false;
    } else if a.ssh_compression {
        ssh.compression = true;
    }
    if ssh.auth == SshAuth::Key && ssh.key_path.as_deref().map(|k| k.trim().is_empty()).unwrap_or(true) {
        bail!("--ssh-key FILE is required with --ssh-auth key");
    }
    Ok(Some(ssh))
}

/// Whether an edit changed what the stored SSH secret belongs to (new tunnel, other bastion,
/// user, auth mode or key), so the old secret no longer applies.
pub fn ssh_identity_changed(old: Option<&SshConfig>, new: &SshConfig) -> bool {
    match old {
        None => true,
        Some(o) => !o.enabled || o.host != new.host || o.user != new.user || o.auth != new.auth || o.key_path != new.key_path,
    }
}

/// Read a secret from the named environment variable (unset is an error; empty is allowed).
pub fn password_from_env(var: &str) -> anyhow::Result<String> {
    match std::env::var(var) {
        Ok(v) => Ok(v),
        Err(std::env::VarError::NotPresent) => bail!("environment variable {var} (--password-env) is not set"),
        Err(e) => bail!("environment variable {var}: {e}"),
    }
}

/// `(ssh password, key passphrase)` from `--ssh-password-env` / `--ssh-passphrase-env`.
pub fn ssh_secrets_from_env(adhoc: &AdhocConn) -> anyhow::Result<(Option<String>, Option<String>)> {
    let pw = adhoc.ssh_password_env.as_deref().map(password_from_env).transpose()?;
    let pp = adhoc.ssh_passphrase_env.as_deref().map(password_from_env).transpose()?;
    Ok((pw, pp))
}

/// Absolute, lossless-as-possible string form of a path argument.
pub fn path_string(p: &Path) -> String {
    std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf()).to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use bulk_core::types::DriverTier;

    fn drivers() -> Vec<DriverInfo> {
        vec![
            DriverInfo {
                id: "postgres".into(),
                name: "PostgreSQL".into(),
                tier: DriverTier::Builtin,
                available: true,
                detail: None,
                install_label: None,
                install_url: None,
                install_hint: None,
                fields: vec!["host".into(), "password".into()],
                defaults: [("port".to_string(), "5432".to_string())].into_iter().collect(),
                session_opts: vec![],
                odbc_drivers: vec![],
                library_path: None,
            },
            DriverInfo {
                id: "sqlite".into(),
                name: "SQLite".into(),
                tier: DriverTier::Builtin,
                available: true,
                detail: None,
                install_label: None,
                install_url: None,
                install_hint: None,
                fields: vec!["path".into()],
                defaults: BTreeMap::new(),
                session_opts: vec![],
                odbc_drivers: vec![],
                library_path: None,
            },
        ]
    }

    #[test]
    fn default_port_and_fields() {
        let adhoc = AdhocConn {
            driver: Some("postgres".into()),
            host: Some("db".into()),
            user: Some("app".into()),
            fields: vec![("sslmode".into(), "require".into())],
            ..Default::default()
        };
        let c = build_connection(&adhoc, "x", &drivers()).unwrap();
        assert_eq!(c.port, Some(5432));
        assert_eq!(c.fields.get("sslmode").map(String::as_str), Some("require"));
        assert!(c.ssh.is_none());
    }

    #[test]
    fn sqlite_needs_path_and_password_field_is_refused() {
        let adhoc = AdhocConn { driver: Some("sqlite".into()), ..Default::default() };
        assert!(build_connection(&adhoc, "x", &drivers()).is_err());
        let adhoc = AdhocConn { driver: Some("postgres".into()), fields: vec![("password".into(), "x".into())], ..Default::default() };
        assert!(build_connection(&adhoc, "x", &drivers()).unwrap_err().to_string().contains("--password-env"));
        assert!(build_connection(&AdhocConn { driver: Some("nope".into()), ..Default::default() }, "x", &drivers()).is_err());
    }

    #[test]
    fn ssh_block() {
        let adhoc = AdhocConn {
            driver: Some("postgres".into()),
            ssh_host: Some("bastion".into()),
            ssh_user: Some("me".into()),
            ssh_auth: Some(crate::args::SshAuthArg::Key),
            ssh_key: Some("id_ed25519".into()),
            ..Default::default()
        };
        let c = build_connection(&adhoc, "x", &drivers()).unwrap();
        let ssh = c.ssh.unwrap();
        assert_eq!(ssh.port, 22);
        assert!(ssh.compression);
        assert_eq!(ssh.auth, bulk_core::types::SshAuth::Key);
        let bad = AdhocConn { ssh_key: Some("k.ppk".into()), ..adhoc.clone() };
        assert!(build_connection(&bad, "x", &drivers()).is_err());
        let no_user = AdhocConn { ssh_user: None, ..adhoc };
        assert!(build_connection(&no_user, "x", &drivers()).is_err());
    }

    /// Parse `conn edit target <flags>` the way the binary does.
    fn edit_args(flags: &[&str]) -> ConnEditArgs {
        use clap::Parser;
        let mut argv = vec!["bulk-cli", "conn", "edit", "target"];
        argv.extend_from_slice(flags);
        match crate::args::Cli::try_parse_from(argv).expect("valid flags").cmd {
            crate::args::Cmd::Conn(crate::args::ConnArgs { cmd: crate::args::ConnCmd::Edit(e) }) => *e,
            other => panic!("expected conn edit, got {other:?}"),
        }
    }

    fn saved_pg() -> Connection {
        let adhoc = AdhocConn {
            driver: Some("postgres".into()),
            host: Some("db1".into()),
            database: Some("app".into()),
            user: Some("reader".into()),
            fields: vec![("sslmode".into(), "require".into())],
            ssh_host: Some("bastion".into()),
            ssh_user: Some("me".into()),
            ..Default::default()
        };
        let mut c = build_connection(&adhoc, "prod", &drivers()).unwrap();
        c.id = "abc123".into();
        c.session.work_mem = Some("64MB".into());
        c.has_password = true;
        c
    }

    #[test]
    fn edit_changes_only_the_given_flags() {
        let old = saved_pg();
        let new = apply_edit(&old, &edit_args(&["--host", "db2", "--port", "6432", "--field", "application_name=x"]), &drivers()).unwrap();
        assert_eq!(new.id, "abc123", "id is kept so history stays attached");
        assert_eq!(new.host.as_deref(), Some("db2"));
        assert_eq!(new.port, Some(6432));
        assert_eq!(new.database, old.database);
        assert_eq!(new.user, old.user);
        assert_eq!(new.session, old.session);
        assert_eq!(new.ssh, old.ssh);
        assert_eq!(new.fields.get("sslmode").map(String::as_str), Some("require"));
        assert_eq!(new.fields.get("application_name").map(String::as_str), Some("x"));
        assert!(new.save_password && new.has_password);

        // No flags: identical connection.
        assert_eq!(apply_edit(&old, &edit_args(&[]), &drivers()).unwrap(), old);

        // Clearing, renaming, unsetting.
        let new = apply_edit(&old, &edit_args(&["--name", " staging ", "--database", "", "--unset-field", "sslmode", "--no-save-password"]), &drivers()).unwrap();
        assert_eq!(new.name, "staging");
        assert_eq!(new.database, None);
        assert!(!new.fields.contains_key("sslmode"));
        assert!(!new.save_password);
        assert!(apply_edit(&old, &edit_args(&["--name", ""]), &drivers()).is_err());
        assert!(apply_edit(&old, &edit_args(&["--field", "password=x"]), &drivers()).is_err());
    }

    #[test]
    fn edit_ssh_block() {
        let old = saved_pg();
        // Change one tunnel setting, keep the rest.
        let new = apply_edit(&old, &edit_args(&["--ssh-port", "2222", "--no-ssh-compression"]), &drivers()).unwrap();
        let ssh = new.ssh.as_ref().unwrap();
        assert_eq!((ssh.host.as_str(), ssh.user.as_str(), ssh.port, ssh.compression), ("bastion", "me", 2222, false));
        assert!(!ssh_identity_changed(old.ssh.as_ref(), ssh), "port / compression keep the stored secret");
        let back = apply_edit(&new, &edit_args(&["--ssh-compression"]), &drivers()).unwrap();
        assert!(back.ssh.unwrap().compression);
        // Remove it.
        assert!(apply_edit(&old, &edit_args(&["--no-ssh"]), &drivers()).unwrap().ssh.is_none());
        // Key auth needs a key; agent auth does not.
        assert!(apply_edit(&old, &edit_args(&["--ssh-auth", "key"]), &drivers()).is_err());
        let agent = apply_edit(&old, &edit_args(&["--ssh-auth", "agent"]), &drivers()).unwrap();
        assert!(ssh_identity_changed(old.ssh.as_ref(), agent.ssh.as_ref().unwrap()));
        // A new tunnel needs host and user.
        let mut plain = old.clone();
        plain.ssh = None;
        assert!(apply_edit(&plain, &edit_args(&["--ssh-port", "22"]), &drivers()).is_err());
        assert!(apply_edit(&plain, &edit_args(&["--ssh-host", "b"]), &drivers()).is_err());
        let added = apply_edit(&plain, &edit_args(&["--ssh-host", "b", "--ssh-user", "u"]), &drivers()).unwrap();
        assert_eq!(added.ssh.as_ref().map(|s| (s.host.as_str(), s.port, s.enabled)), Some(("b", 22, true)));
    }

    #[test]
    fn edit_driver_switch_moves_a_default_port() {
        let mut drivers = drivers();
        drivers[1].defaults.insert("port".into(), "1".into());
        let old = saved_pg();
        let new = apply_edit(&old, &edit_args(&["--driver", "sqlite", "--path", "x.db"]), &drivers).unwrap();
        assert_eq!(new.driver, "sqlite");
        assert_eq!(new.port, Some(1), "5432 was postgres' default");
        assert!(apply_edit(&old, &edit_args(&["--driver", "sqlite"]), &drivers).is_err(), "sqlite needs a path");
        assert!(apply_edit(&old, &edit_args(&["--driver", "nope"]), &drivers).is_err());
        let custom = Connection { port: Some(6000), ..old };
        assert_eq!(apply_edit(&custom, &edit_args(&["--driver", "sqlite", "--path", "x.db"]), &drivers).unwrap().port, Some(6000));
    }

    #[test]
    fn names_clash_ignoring_case() {
        let a = Connection { id: "1".into(), name: "Prod".into(), ..Default::default() };
        let b = Connection { id: "2".into(), name: "dev".into(), ..Default::default() };
        let all = vec![a, b];
        assert_eq!(find_by_name(&all, " prod ", None).map(|c| c.id.as_str()), Some("1"));
        assert!(find_by_name(&all, "prod", Some("1")).is_none(), "renaming a connection to its own name is fine");
        assert!(find_by_name(&all, "qa", None).is_none());
    }
}
