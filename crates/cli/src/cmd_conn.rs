//! `conn`: list / add / edit / delete / test / import-dbeaver for saved connections.

use std::process::ExitCode;
use std::sync::Arc;

use anyhow::{bail, Context};
use bulk_core::error::CoreError;
use bulk_core::types::{Connection, ConnectionInput, DriverInfo, SshAuth, SshConfig};
use bulk_core::BulkDb;

use crate::args::{AdhocConn, ConnAddArgs, ConnArgs, ConnCmd, ConnEditArgs};
use crate::cmd_info::print_probe;
use crate::connect::{self, apply_edit, build_connection, find_by_name, password_from_env, ssh_identity_changed, ssh_secrets_from_env};
use crate::output::{enum_name, kv, opt, print_json, yes_no, Align, Table};
use crate::prompts;

pub async fn run(db: &Arc<BulkDb>, args: ConnArgs, json: bool) -> anyhow::Result<ExitCode> {
    match args.cmd {
        ConnCmd::List => {
            let list = db.conn_list();
            if json {
                print_json(&list)?;
            } else {
                print_conn_table(&list);
            }
        }
        ConnCmd::Show { conn } => {
            let found = connect::find_saved(db, &conn)?;
            let c = db.conn_get(&found.id)?;
            if json {
                print_json(&c)?;
            } else {
                print_conn_details(&c);
            }
        }
        ConnCmd::Add(a) => add(db, &a, json)?,
        ConnCmd::Edit(a) => edit(db, &a, json)?,
        ConnCmd::Delete { conn, yes } => {
            let c = connect::find_saved(db, &conn)?;
            if !yes && !json {
                if !prompts::is_interactive() {
                    anyhow::bail!("refusing to delete '{}' without --yes (stdin is not a terminal)", c.name);
                }
                if !prompts::confirm(&format!("Delete connection '{}' ({}) and its stored secrets? [y/N] ", c.name, c.id))? {
                    println!("not deleted");
                    return Ok(ExitCode::SUCCESS);
                }
            }
            db.conn_delete(&c.id).context("deleting the connection")?;
            if json {
                print_json(&serde_json::json!({ "deleted": c.id, "name": c.name }))?;
            } else {
                println!("deleted connection '{}' ({})", c.name, c.id);
            }
        }
        ConnCmd::Test { conn, password_env } => {
            let c = connect::find_saved(db, &conn)?;
            let password = password_env.as_deref().map(password_from_env).transpose()?;
            let result = db.conn_test(&c.id, password).await?;
            print_probe(&c, &result, json)?;
        }
        ConnCmd::ImportDbeaver { path, save } => {
            let imported = db.conn_import_dbeaver(&connect::path_string(&path)).context("reading data-sources.json")?;
            let mut saved_ids: Vec<Connection> = Vec::new();
            let mut skipped: Vec<String> = Vec::new();
            if save {
                // Names already taken (saved before, or earlier in this file) are skipped: a
                // duplicate name would make `--conn NAME` ambiguous.
                let mut known = db.conn_list();
                for ic in &imported {
                    if find_by_name(&known, &ic.connection.name, None).is_some() {
                        skipped.push(ic.connection.name.clone());
                        continue;
                    }
                    let mut c = ic.connection.clone();
                    c.id.clear();
                    let s = db.conn_save(ConnectionInput { connection: c, ..Default::default() }).with_context(|| format!("saving '{}'", ic.connection.name))?;
                    known.push(s.clone());
                    saved_ids.push(s);
                }
            }
            if json {
                print_json(&serde_json::json!({ "imported": imported, "saved": saved_ids, "skipped": skipped }))?;
            } else {
                let mut t = Table::new(&[
                    ("NAME", Align::Left),
                    ("DRIVER", Align::Left),
                    ("HOST", Align::Left),
                    ("PORT", Align::Right),
                    ("DATABASE", Align::Left),
                    ("USER", Align::Left),
                    ("SSH", Align::Left),
                    ("NOTES", Align::Left),
                ]);
                for ic in &imported {
                    let c = &ic.connection;
                    t.row(vec![c.name.clone(), c.driver.clone(), opt(&c.host), opt(&c.port), opt(&c.database), opt(&c.user), ssh_label(c), ic.notes.join("; ")]);
                }
                t.print();
                if save {
                    println!(
                        "saved {} connection(s); passwords are never imported: store one with `bulk-cli conn edit <ID> --password-env VAR` or pass --password-env at run time",
                        saved_ids.len()
                    );
                    if !skipped.is_empty() {
                        eprintln!("skipped {} connection(s) whose name is already saved: {}", skipped.len(), skipped.join(", "));
                    }
                } else if !imported.is_empty() {
                    println!("({} connection(s); re-run with --save to keep them)", imported.len());
                }
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// `conn add`: refuses a name that is already saved unless `--replace` asks to overwrite
/// that connection (same id, so its history and session defaults stay).
fn add(db: &BulkDb, a: &ConnAddArgs, json: bool) -> anyhow::Result<()> {
    let name = a.name.trim();
    if name.is_empty() {
        bail!("--name must not be empty");
    }
    let drivers = db.drivers_list();
    let mut conn = build_connection(&a.adhoc, name, &drivers)?;
    let all = db.conn_list();
    let replaced = match find_by_name(&all, name, None) {
        None => None,
        Some(existing) if a.replace => {
            let same = all.iter().filter(|c| c.name.trim().eq_ignore_ascii_case(name)).count();
            if same > 1 {
                return Err(CoreError::Invalid(format!(
                    "{same} connections are named '{name}'; change one by id with `bulk-cli conn edit <ID>` (see `bulk-cli conn list`)"
                ))
                .into());
            }
            conn.id = existing.id.clone();
            conn.session = existing.session.clone();
            Some(existing.clone())
        }
        Some(existing) => {
            return Err(CoreError::Invalid(format!(
                "a connection named '{}' already exists ({}); change it with `bulk-cli conn edit {}` or re-run with --replace",
                existing.name, existing.id, existing.id
            ))
            .into());
        }
    };
    conn.save_password = !a.no_save_password;
    let keeps_password = replaced.as_ref().map(|c| c.save_password && c.has_password).unwrap_or(false);
    let password = if conn.save_password { db_password(&a.adhoc, &conn, &drivers, keeps_password)? } else { None };
    // Secrets of a connection that asks every time are never stored, so do not ask for them.
    let (ssh_password, ssh_key_passphrase) = if conn.save_password { ssh_secrets(&a.adhoc, &conn)? } else { (None, None) };
    let saved = db.conn_save(ConnectionInput { connection: conn, password, ssh_password, ssh_key_passphrase, ..Default::default() }).context("saving the connection")?;
    if json {
        print_json(&saved)?;
    } else {
        let verb = if replaced.is_some() { "replaced" } else { "saved" };
        println!("{verb} connection '{}' ({}) for driver {}", saved.name, saved.id, saved.driver);
        note_missing_password(&saved, &drivers);
    }
    Ok(())
}

/// `conn edit`: apply the given flags to a saved connection and save it under the same id.
fn edit(db: &BulkDb, a: &ConnEditArgs, json: bool) -> anyhow::Result<()> {
    let existing = connect::find_saved(db, &a.conn)?;
    let drivers = db.drivers_list();
    let conn = apply_edit(&existing, a, &drivers)?;
    if conn.name != existing.name {
        if let Some(other) = find_by_name(&db.conn_list(), &conn.name, Some(&existing.id)) {
            return Err(CoreError::Invalid(format!("a connection named '{}' already exists ({}); pick another --name", other.name, other.id)).into());
        }
    }

    let interactive = prompts::is_interactive();
    let refuse_unstored = || -> anyhow::Result<()> {
        Err(CoreError::Invalid(format!("'{}' asks for its secrets every time and stores none; add --save-password to store them", conn.name)).into())
    };

    // Database password: only when one is given, asked for, or newly needed.
    let password = if let Some(var) = &a.adhoc.password_env {
        if !conn.save_password {
            refuse_unstored()?;
        }
        Some(password_from_env(var)?)
    } else if a.ask_password {
        if !conn.save_password {
            refuse_unstored()?;
        }
        if !interactive {
            bail!("--ask-password needs a terminal; use --password-env VAR");
        }
        let pw = prompts::prompt_secret(&format!("New password for {} (empty = keep the stored one): ", user_at_host(&conn)))?;
        (!pw.is_empty()).then_some(pw)
    } else if conn.save_password && !existing.save_password && !a.clear_password && driver_needs_password(&drivers, &conn.driver) && interactive {
        // Switched from "ask every time" to storing the password.
        let pw = prompts::prompt_secret(&format!("Password for {} (empty = none): ", user_at_host(&conn)))?;
        (!pw.is_empty()).then_some(pw)
    } else {
        None
    };

    // SSH secret: from the env flags, or prompted for when the tunnel identity changed.
    let (mut ssh_password, mut ssh_key_passphrase) = ssh_secrets_from_env(&a.adhoc)?;
    let tunnel = conn.ssh.as_ref().filter(|s| s.enabled);
    if ssh_password.is_some() || ssh_key_passphrase.is_some() {
        if tunnel.is_none() {
            bail!("--ssh-password-env / --ssh-passphrase-env need an SSH tunnel (--ssh-host)");
        }
        if !conn.save_password {
            refuse_unstored()?;
        }
    } else if let Some(ssh) = tunnel {
        if conn.save_password && !a.clear_ssh_secret && (ssh_identity_changed(existing.ssh.as_ref(), ssh) || (!existing.save_password && interactive)) {
            if interactive {
                (ssh_password, ssh_key_passphrase) = prompt_ssh_secrets(ssh, None, None)?;
            } else if existing.has_ssh_secret && ssh.auth != SshAuth::Agent && existing.ssh.as_ref().map(|o| o.auth) == Some(ssh.auth) {
                eprintln!("note: the stored SSH secret was kept; pass --ssh-password-env / --ssh-passphrase-env if the changed tunnel needs another one");
            }
        }
    }

    let clears = a.clear_password || a.clear_ssh_secret;
    if conn == existing && password.is_none() && ssh_password.is_none() && ssh_key_passphrase.is_none() && !clears {
        if json {
            print_json(&existing)?;
        } else {
            println!("no changes to connection '{}' ({})", existing.name, existing.id);
        }
        return Ok(());
    }

    let input = ConnectionInput { connection: conn, password, ssh_password, ssh_key_passphrase, clear_password: a.clear_password, clear_ssh_secret: a.clear_ssh_secret };
    let saved = db.conn_save(input).context("saving the connection")?;
    if json {
        print_json(&saved)?;
    } else {
        println!("updated connection '{}' ({}) for driver {}", saved.name, saved.id, saved.driver);
        if a.clear_password {
            println!("{}", if existing.has_password { "removed the stored password" } else { "no stored password to remove" });
        }
        if a.clear_ssh_secret {
            println!("{}", if existing.has_ssh_secret { "removed the stored SSH secret" } else { "no stored SSH secret to remove" });
        }
        note_missing_password(&saved, &drivers);
    }
    Ok(())
}

/// Hint for a connection that should store a password but has none.
fn note_missing_password(saved: &Connection, drivers: &[DriverInfo]) {
    if !saved.has_password && saved.save_password && driver_needs_password(drivers, &saved.driver) {
        eprintln!("note: no password stored; pass --password-env at run time or store one with `bulk-cli conn edit {} --password-env VAR`", saved.id);
    }
}

fn print_conn_table(list: &[Connection]) {
    let mut t = Table::new(&[
        ("ID", Align::Left),
        ("NAME", Align::Left),
        ("DRIVER", Align::Left),
        ("HOST", Align::Left),
        ("PORT", Align::Right),
        ("DATABASE", Align::Left),
        ("USER", Align::Left),
        ("SSH", Align::Left),
        ("PASSWORD", Align::Left),
        ("OUTPUT DIR", Align::Left),
    ]);
    for c in list {
        let host = c.host.clone().or_else(|| c.fields.get("path").cloned()).unwrap_or_else(|| "-".into());
        let password = if !c.save_password { "ask".to_string() } else { yes_no(c.has_password).to_string() };
        t.row(vec![c.id.clone(), c.name.clone(), c.driver.clone(), host, opt(&c.port), opt(&c.database), opt(&c.user), ssh_label(c), password, opt(&c.output_dir)]);
    }
    t.print();
}

/// `conn show`: one connection as label / value lines (secrets only as stored / not stored).
fn print_conn_details(c: &Connection) {
    kv("ID", &c.id);
    kv("Name", &c.name);
    kv("Driver", &c.driver);
    kv("Host", opt(&c.host));
    kv("Port", opt(&c.port));
    kv("Database", opt(&c.database));
    kv("User", opt(&c.user));
    for (k, v) in &c.fields {
        kv(&format!("  {k}"), v);
    }
    kv("SSH tunnel", ssh_label(c));
    if let Some(s) = c.ssh.as_ref().filter(|s| s.enabled) {
        kv("  auth", enum_name(&s.auth));
        if let Some(key) = &s.key_path {
            kv("  key", key);
        }
        kv("  compression", yes_no(s.compression));
    }
    kv(
        "Password",
        if c.save_password {
            if c.has_password {
                "stored"
            } else {
                "not stored"
            }
        } else {
            "ask every time"
        },
    );
    kv("SSH secret", if c.has_ssh_secret { "stored" } else { "not stored" });
    kv("Output dir", opt(&c.output_dir));
}

fn ssh_label(c: &Connection) -> String {
    match &c.ssh {
        Some(s) if s.enabled => format!("{}@{}:{}", s.user, s.host, s.port),
        _ => "-".into(),
    }
}

fn user_at_host(conn: &Connection) -> String {
    format!("{}@{}", conn.user.as_deref().unwrap_or("?"), conn.host.as_deref().unwrap_or("?"))
}

fn driver_needs_password(drivers: &[DriverInfo], driver: &str) -> bool {
    drivers.iter().find(|d| d.id == driver).map(|d| d.fields.iter().any(|f| f == "password")).unwrap_or(true)
}

/// `--password-env`, else a tty prompt when the driver uses passwords, else none.
/// `keeps_existing`: a password is already stored and an empty answer keeps it.
fn db_password(adhoc: &AdhocConn, conn: &Connection, drivers: &[DriverInfo], keeps_existing: bool) -> anyhow::Result<Option<String>> {
    if let Some(var) = &adhoc.password_env {
        return Ok(Some(password_from_env(var)?));
    }
    if !driver_needs_password(drivers, &conn.driver) {
        return Ok(None);
    }
    if !prompts::is_interactive() {
        if !keeps_existing {
            eprintln!("note: no --password-env and stdin is not a terminal; the connection is saved without a password");
        }
        return Ok(None);
    }
    let empty = if keeps_existing { "keep the stored one" } else { "none" };
    let pw = prompts::prompt_secret(&format!("Password for {} (empty = {empty}): ", user_at_host(conn)))?;
    Ok(if pw.is_empty() { None } else { Some(pw) })
}

/// SSH password / key passphrase from env vars, else prompted on the tty.
fn ssh_secrets(adhoc: &AdhocConn, conn: &Connection) -> anyhow::Result<(Option<String>, Option<String>)> {
    let (pw, pp) = ssh_secrets_from_env(adhoc)?;
    match &conn.ssh {
        Some(ssh) if prompts::is_interactive() => prompt_ssh_secrets(ssh, pw, pp),
        _ => Ok((pw, pp)),
    }
}

/// Ask on the tty for the secret the tunnel's auth mode needs, unless it is already given.
fn prompt_ssh_secrets(ssh: &SshConfig, mut pw: Option<String>, mut pp: Option<String>) -> anyhow::Result<(Option<String>, Option<String>)> {
    match ssh.auth {
        SshAuth::Password if pw.is_none() => {
            let s = prompts::prompt_secret(&format!("SSH password for {}@{} (empty = ask every time): ", ssh.user, ssh.host))?;
            pw = if s.is_empty() { None } else { Some(s) };
        }
        SshAuth::Key if pp.is_none() => {
            let s = prompts::prompt_secret(&format!("Passphrase for {} (empty = none): ", ssh.key_path.as_deref().unwrap_or("the key")))?;
            pp = if s.is_empty() { None } else { Some(s) };
        }
        _ => {}
    }
    Ok((pw, pp))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::args::{Cli, Cmd};
    use bulk_core::credentials::{self, MemoryStore};
    use bulk_core::service::BulkDbOptions;
    use clap::Parser;

    fn edit_args(argv: &[&str]) -> ConnEditArgs {
        let mut full = vec!["bulk-cli", "conn", "edit"];
        full.extend_from_slice(argv);
        match Cli::try_parse_from(full).expect("valid conn edit").cmd {
            Cmd::Conn(ConnArgs { cmd: ConnCmd::Edit(e) }) => *e,
            _ => panic!("expected conn edit"),
        }
    }

    /// `--clear-password` / `--clear-ssh-secret` each delete exactly one stored secret (run in
    /// process: the memory credential store does not outlive a `bulk-cli` process).
    #[tokio::test]
    async fn edit_clears_exactly_the_requested_secret() {
        let dir = std::env::temp_dir().join(format!("bulk-cli-unit-{}-clear-secrets", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = BulkDb::new(BulkDbOptions { config_dir: Some(dir.clone()), prompts: None, credentials: Some(Box::new(MemoryStore::default())), init_logging: false })
            .await
            .unwrap();
        let ssh = SshConfig { enabled: true, host: "bastion".into(), port: 22, user: "deploy".into(), auth: SshAuth::Password, key_path: None, compression: true };
        let conn = Connection { name: "pg".into(), driver: "postgres".into(), host: Some("db".into()), ssh: Some(ssh), save_password: true, ..Default::default() };
        let saved = db.conn_save(ConnectionInput { connection: conn, password: Some("pw".into()), ssh_password: Some("sp".into()), ..Default::default() }).unwrap();
        assert!(saved.has_password && saved.has_ssh_secret);
        let db_key = credentials::db_password_key(&saved.id);
        let ssh_key = credentials::ssh_password_key(&saved.id);

        edit(&db, &edit_args(&["pg", "--clear-password"]), true).unwrap();
        let c = db.conn_get(&saved.id).unwrap();
        assert!(!c.has_password && c.has_ssh_secret, "{c:?}");
        assert!(db.creds.get(&db_key).unwrap().is_none());
        assert_eq!(db.creds.get(&ssh_key).unwrap().as_deref(), Some("sp"));

        edit(&db, &edit_args(&["pg", "--clear-ssh-secret"]), true).unwrap();
        let c = db.conn_get(&saved.id).unwrap();
        assert!(!c.has_password && !c.has_ssh_secret, "{c:?}");
        assert!(db.creds.get(&ssh_key).unwrap().is_none());
        assert_eq!(c.id, saved.id, "the id (and its history) stays");

        db.shutdown().await;
        drop(db);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
