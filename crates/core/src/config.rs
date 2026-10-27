//! `connections.toml` and `settings.toml` in the platform config directory. No secrets ever
//! land here, so the directory can live in Git or a cloud drive.

use std::path::{Path, PathBuf};
use std::sync::RwLock;

use serde::{Deserialize, Serialize};

use crate::connect::{split_connection_string_secret, CONNECTION_STRING_FIELD};
use crate::error::{CoreError, Result};
use crate::types::{Connection, ImportedConnection, Settings};

#[derive(Debug, Default, Serialize, Deserialize)]
struct ConnectionsFile {
    #[serde(default)]
    connections: Vec<Connection>,
}

pub struct ConfigStore {
    dir: PathBuf,
    connections: RwLock<Vec<Connection>>,
    settings: RwLock<Settings>,
}

impl ConfigStore {
    pub fn open(dir: PathBuf) -> Result<Self> {
        std::fs::create_dir_all(&dir)?;
        let store = ConfigStore { dir, connections: RwLock::new(Vec::new()), settings: RwLock::new(Settings::default()) };
        store.reload()?;
        Ok(store)
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn connections_path(&self) -> PathBuf {
        self.dir.join("connections.toml")
    }

    pub fn settings_path(&self) -> PathBuf {
        self.dir.join("settings.toml")
    }

    pub fn history_path(&self) -> PathBuf {
        self.dir.join("history.sqlite")
    }

    pub fn drivers_override_path(&self) -> PathBuf {
        self.dir.join("drivers").join("registry.json")
    }

    pub fn reload(&self) -> Result<()> {
        let conns = match std::fs::read_to_string(self.connections_path()) {
            Ok(s) => toml::from_str::<ConnectionsFile>(&s)?.connections,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => vec![],
            Err(e) => return Err(e.into()),
        };
        let settings = match std::fs::read_to_string(self.settings_path()) {
            Ok(s) => toml::from_str::<Settings>(&s)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Settings::default(),
            Err(e) => return Err(e.into()),
        };
        *self.connections.write().unwrap() = conns;
        *self.settings.write().unwrap() = settings;
        Ok(())
    }

    pub fn connections(&self) -> Vec<Connection> {
        self.connections.read().unwrap().clone()
    }

    pub fn get(&self, id: &str) -> Option<Connection> {
        self.connections.read().unwrap().iter().find(|c| c.id == id).cloned()
    }

    /// Insert or replace by id; assigns an id when empty. Never writes a secret: a `password`
    /// field is dropped, and an ODBC `connection_string` that still carries a password
    /// attribute (`PWD=` / `Password=` in any case, braced values included) is refused with
    /// `INVALID` and nothing is written. `BulkDb::conn_save` moves such a password into the
    /// credential store before it gets here.
    pub fn upsert(&self, mut conn: Connection) -> Result<Connection> {
        if conn.name.trim().is_empty() {
            return Err(CoreError::Invalid("connection name is required".into()));
        }
        if conn.fields.get(CONNECTION_STRING_FIELD).is_some_and(|cs| split_connection_string_secret(cs).is_some()) {
            return Err(CoreError::Invalid(format!(
                "the connection string of '{}' contains a secret attribute (PWD=, Password=, *Token=, *Secret* ...); secrets are never written to connections.toml: \
                 remove it from the connection string and give the password as the connection's password (stored in the OS credential store), \
                 or keep other secrets in an ODBC data source (DSN)",
                conn.name
            )));
        }
        if conn.id.is_empty() {
            conn.id = uuid::Uuid::new_v4().simple().to_string()[..12].to_string();
        }
        conn.fields.remove("password");
        conn.has_password = false;
        conn.has_ssh_secret = false;
        {
            let mut all = self.connections.write().unwrap();
            if let Some(slot) = all.iter_mut().find(|c| c.id == conn.id) {
                *slot = conn.clone();
            } else {
                all.push(conn.clone());
            }
        }
        self.save_connections()?;
        Ok(conn)
    }

    pub fn delete(&self, id: &str) -> Result<()> {
        self.connections.write().unwrap().retain(|c| c.id != id);
        self.save_connections()
    }

    fn save_connections(&self) -> Result<()> {
        let file = ConnectionsFile { connections: self.connections.read().unwrap().clone() };
        let text = toml::to_string_pretty(&file)?;
        write_atomic(&self.connections_path(), text.as_bytes())
    }

    pub fn settings(&self) -> Settings {
        self.settings.read().unwrap().clone()
    }

    pub fn set_settings(&self, s: Settings) -> Result<Settings> {
        let mut s = s;
        s.max_concurrent_jobs = s.max_concurrent_jobs.clamp(1, 3);
        let text = toml::to_string_pretty(&s)?;
        write_atomic(&self.settings_path(), text.as_bytes())?;
        *self.settings.write().unwrap() = s.clone();
        Ok(s)
    }

    /// Parse DBeaver's `data-sources.json` (passwords are never imported).
    pub fn import_dbeaver(path: &Path) -> Result<Vec<ImportedConnection>> {
        let _ = path;
        Err(CoreError::Internal("dbeaver import not implemented".into()))
    }
}

/// Write via a temp file + rename so a crash never leaves a half-written config.
pub fn write_atomic(path: &Path, data: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, data)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_connections_without_password() {
        let dir = tempfile::tempdir().unwrap();
        let store = ConfigStore::open(dir.path().to_path_buf()).unwrap();
        let mut c = Connection { name: "prod".into(), driver: "postgres".into(), host: Some("db".into()), port: Some(5432), ..Default::default() };
        c.fields.insert("password".into(), "secret".into());
        let saved = store.upsert(c).unwrap();
        assert!(!saved.id.is_empty());
        let text = std::fs::read_to_string(store.connections_path()).unwrap();
        assert!(!text.contains("secret"));
        let again = ConfigStore::open(dir.path().to_path_buf()).unwrap();
        assert_eq!(again.connections().len(), 1);
        assert_eq!(again.connections()[0].name, "prod");
    }

    fn odbc(name: &str, conn_str: &str) -> Connection {
        let mut c = Connection { name: name.into(), driver: "odbc".into(), save_password: true, ..Default::default() };
        c.fields.insert(CONNECTION_STRING_FIELD.into(), conn_str.into());
        c
    }

    /// A direct caller (not `conn_save`) gets INVALID instead of a password on disk, whatever
    /// the spelling of the attribute.
    #[test]
    fn upsert_refuses_a_password_inside_the_connection_string() {
        let dir = tempfile::tempdir().unwrap();
        let store = ConfigStore::open(dir.path().to_path_buf()).unwrap();
        let kept = store.upsert(odbc("plain", "Driver={ODBC Driver 18};Server=h;UID=u;Trusted_Connection=yes")).unwrap();
        let before = std::fs::read_to_string(store.connections_path()).unwrap();

        for cs in [
            "DSN=x;PWD=hunter2",
            "DSN=x;pwd=hunter2;",
            "Driver={x};Password=hunter2",
            "Driver={x};PASSWORD = hunter2",
            "Driver={x};Pwd={hunter2;with}}braces};UID=u",
            "PWD=",
        ] {
            let err = store.upsert(odbc("leaky", cs)).unwrap_err();
            assert_eq!(err.code(), "INVALID", "{cs}");
            assert!(!err.to_string().contains("hunter2"), "the error must not echo the secret: {err}");
            // Updating an existing connection with a secret is refused the same way.
            let mut update = odbc("plain", cs);
            update.id = kept.id.clone();
            assert_eq!(store.upsert(update).unwrap_err().code(), "INVALID", "{cs}");
        }
        // Nothing was written or changed in memory.
        assert_eq!(std::fs::read_to_string(store.connections_path()).unwrap(), before);
        assert_eq!(store.connections(), vec![kept.clone()]);
        assert!(!before.contains("hunter2"));

        // Attribute names that merely contain "pwd" are not passwords.
        let ok = store.upsert(odbc("lookalike", "DSN=x;PWDPolicy=strict;OldPwdHint=no")).unwrap();
        assert_eq!(ok.fields.get(CONNECTION_STRING_FIELD).map(String::as_str), Some("DSN=x;PWDPolicy=strict;OldPwdHint=no"));
    }
}
