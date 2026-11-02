//! `connections.toml` and `settings.toml` in the platform config directory. No secrets ever
//! land here, so the directory can live in Git or a cloud drive.
//!
//! Several BulkDB processes share these files (docs/db-copy.md 1.2). Every mutation runs under
//! an advisory lock on `<config_dir>/.config.lock` (`fs4`, polled every 25 ms for up to 5 s):
//! inside it the file is re-read, only this call's delta is applied (connections by id; the
//! settings object as a whole, last writer wins), the result is written atomically and its
//! stamp (mtime + length) recorded. [`ConfigStore::reload_if_changed`] compares the stamps to
//! pick up what another process wrote.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime};

use serde::{Deserialize, Serialize};

use crate::connect::{split_connection_string_secret, CONNECTION_STRING_FIELD};
use crate::error::{CoreError, Result};
use crate::types::{Connection, ImportedConnection, Settings};

/// Advisory lock file of config writes (never holds data).
pub const CONFIG_LOCK_FILE: &str = ".config.lock";
/// How long a write waits for another process's write to finish.
pub const CONFIG_LOCK_WAIT: Duration = Duration::from_secs(5);
/// Poll interval while waiting for the config lock.
const CONFIG_LOCK_POLL: Duration = Duration::from_millis(25);
/// Message of the `IO` error when the lock stays taken.
pub const CONFIG_LOCK_BUSY: &str = "다른 BulkDB가 설정을 저장하는 중입니다 (another BulkDB is saving the configuration)";

#[derive(Debug, Default, Serialize, Deserialize)]
struct ConnectionsFile {
    #[serde(default)]
    connections: Vec<Connection>,
}

/// What identifies one version of a config file: modification time and length (`None`: the
/// file does not exist).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Stamp {
    modified: Option<SystemTime>,
    len: u64,
}

fn stamp_of(path: &Path) -> Result<Option<Stamp>> {
    match std::fs::metadata(path) {
        Ok(m) => Ok(Some(Stamp { modified: m.modified().ok(), len: m.len() })),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

#[derive(Debug, Default)]
struct Stamps {
    connections: Option<Stamp>,
    settings: Option<Stamp>,
}

pub struct ConfigStore {
    dir: PathBuf,
    connections: RwLock<Vec<Connection>>,
    settings: RwLock<Settings>,
    /// Stamps of the files as this process last read or wrote them.
    stamps: Mutex<Stamps>,
    /// Changes by other processes that a locked write folded into the cache before
    /// `reload_if_changed` saw them; its next call reports them.
    pending: Mutex<ConfigChange>,
    /// Serialises this process's writes and reloads (the file lock serialises processes).
    io: Mutex<()>,
}

impl ConfigStore {
    pub fn open(dir: PathBuf) -> Result<Self> {
        std::fs::create_dir_all(&dir)?;
        let store = ConfigStore {
            dir,
            connections: RwLock::new(Vec::new()),
            settings: RwLock::new(Settings::default()),
            stamps: Mutex::new(Stamps::default()),
            pending: Mutex::new(ConfigChange::default()),
            io: Mutex::new(()),
        };
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

    fn lock_path(&self) -> PathBuf {
        self.dir.join(CONFIG_LOCK_FILE)
    }

    /// `connections.toml` as on disk now, with the stamp taken before reading (a write racing
    /// the read then shows up as a changed stamp on the next poll, never as a missed change).
    fn read_connections_file(&self) -> Result<(Vec<Connection>, Option<Stamp>)> {
        let path = self.connections_path();
        let stamp = stamp_of(&path)?;
        let conns = match std::fs::read_to_string(&path) {
            Ok(s) => toml::from_str::<ConnectionsFile>(&s)?.connections,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => vec![],
            Err(e) => return Err(e.into()),
        };
        Ok((conns, stamp))
    }

    fn read_settings_file(&self) -> Result<(Settings, Option<Stamp>)> {
        let path = self.settings_path();
        let stamp = stamp_of(&path)?;
        let settings = match std::fs::read_to_string(&path) {
            Ok(s) => toml::from_str::<Settings>(&s)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Settings::default(),
            Err(e) => return Err(e.into()),
        };
        Ok((settings, stamp))
    }

    pub fn reload(&self) -> Result<()> {
        let _io = self.io.lock().unwrap();
        let (conns, conn_stamp) = self.read_connections_file()?;
        let (settings, settings_stamp) = self.read_settings_file()?;
        *self.connections.write().unwrap() = conns;
        *self.settings.write().unwrap() = settings;
        *self.stamps.lock().unwrap() = Stamps { connections: conn_stamp, settings: settings_stamp };
        *self.pending.lock().unwrap() = ConfigChange::default();
        Ok(())
    }

    pub fn connections(&self) -> Vec<Connection> {
        self.connections.read().unwrap().clone()
    }

    pub fn get(&self, id: &str) -> Option<Connection> {
        self.connections.read().unwrap().iter().find(|c| c.id == id).cloned()
    }

    /// Run `f` holding this process's write mutex and the cross-process advisory lock on
    /// `.config.lock` (polled every 25 ms for up to [`CONFIG_LOCK_WAIT`], then `IO`).
    fn with_lock<T>(&self, f: impl FnOnce() -> Result<T>) -> Result<T> {
        let _io = self.io.lock().unwrap();
        let file = std::fs::OpenOptions::new().create(true).read(true).write(true).truncate(false).open(self.lock_path())?;
        let deadline = Instant::now() + CONFIG_LOCK_WAIT;
        // Fully qualified: std's `File` has inherent `lock*` methods of its own.
        while !fs4::fs_std::FileExt::try_lock_exclusive(&file)? {
            if Instant::now() >= deadline {
                return Err(CoreError::Io(CONFIG_LOCK_BUSY.into()));
            }
            std::thread::sleep(CONFIG_LOCK_POLL);
        }
        let out = f();
        if let Err(e) = fs4::fs_std::FileExt::unlock(&file) {
            tracing::debug!("config lock: unlock failed (released when the file closes): {e}");
        }
        out
    }

    /// Inside the lock: re-read `connections.toml`, let `apply` change it, write it back and
    /// update the cache. Ids another process changed since this process last looked (other
    /// than the ones `apply` touched) are queued for `reload_if_changed`.
    fn mutate_connections<T>(&self, apply: impl FnOnce(&mut Vec<Connection>) -> Result<(T, Vec<String>)>) -> Result<T> {
        self.with_lock(|| {
            let (mut all, disk_stamp) = self.read_connections_file()?;
            let known_stamp = self.stamps.lock().unwrap().connections;
            let foreign = if disk_stamp != known_stamp { changed_ids(&self.connections.read().unwrap(), &all) } else { Vec::new() };
            let (out, touched) = apply(&mut all)?;
            let text = toml::to_string_pretty(&ConnectionsFile { connections: all.clone() })?;
            write_atomic(&self.connections_path(), text.as_bytes())?;
            let stamp = stamp_of(&self.connections_path())?;
            *self.connections.write().unwrap() = all;
            self.stamps.lock().unwrap().connections = stamp;
            let foreign: Vec<String> = foreign.into_iter().filter(|id| !touched.contains(id)).collect();
            if !foreign.is_empty() {
                self.pending.lock().unwrap().add_connections(foreign);
            }
            Ok(out)
        })
    }

    /// Insert or replace by id; assigns an id when empty. Never writes a secret: a `password`
    /// field is dropped, and an ODBC `connection_string` that still carries a password
    /// attribute (`PWD=` / `Password=` in any case, braced values included) is refused with
    /// `INVALID` and nothing is written. `BulkDb::conn_save` moves such a password into the
    /// credential store before it gets here.
    ///
    /// Runs under the config lock against the re-read file, so a save never drops another
    /// process's connections, and `write_allowed` is resolved against the value stored on disk
    /// (a stale process can neither revive nor drop the flag).
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
        self.mutate_connections(|all| {
            conn.write_allowed = resolve_write_allowed(all.iter().find(|c| c.id == conn.id), &conn);
            if let Some(slot) = all.iter_mut().find(|c| c.id == conn.id) {
                *slot = conn.clone();
            } else {
                all.push(conn.clone());
            }
            let id = conn.id.clone();
            Ok((conn, vec![id]))
        })
    }

    pub fn delete(&self, id: &str) -> Result<()> {
        self.mutate_connections(|all| {
            all.retain(|c| c.id != id);
            Ok(((), vec![id.to_string()]))
        })
    }

    pub fn settings(&self) -> Settings {
        self.settings.read().unwrap().clone()
    }

    /// Persist the settings as a whole (last writer wins across processes), under the lock.
    pub fn set_settings(&self, s: Settings) -> Result<Settings> {
        let mut s = s;
        s.max_concurrent_jobs = s.max_concurrent_jobs.clamp(1, 3);
        let text = toml::to_string_pretty(&s)?;
        self.with_lock(|| {
            write_atomic(&self.settings_path(), text.as_bytes())?;
            let stamp = stamp_of(&self.settings_path())?;
            *self.settings.write().unwrap() = s.clone();
            self.stamps.lock().unwrap().settings = stamp;
            // This write supersedes whatever another process wrote before it.
            self.pending.lock().unwrap().settings = false;
            Ok(s)
        })
    }

    /// Parse DBeaver's `data-sources.json` (passwords are never imported).
    pub fn import_dbeaver(path: &Path) -> Result<Vec<ImportedConnection>> {
        let _ = path;
        Err(CoreError::Internal("dbeaver import not implemented".into()))
    }

    /// Reload the files when another process changed them (stamps: mtime + len) and say what
    /// changed (docs/db-copy.md section 1.2): the ids of connections added, changed or removed,
    /// and whether the settings differ. Changes that a locked write of this process already
    /// folded into the cache are reported here too; this process's own writes never are.
    pub fn reload_if_changed(&self) -> Result<ConfigChange> {
        let _io = self.io.lock().unwrap();
        // Drain the ids a locked write already folded into the cache. The reads below are
        // fallible (a Windows sharing violation while another process renames the file over it,
        // an unparsable hand-edited settings.toml), and on any error `change` must go back into
        // `pending`: a stamp this call already advanced would otherwise hide those ids forever,
        // so the other windows' catalog/browser caches would never be dropped. Report them again
        // on a later poll instead of losing them after one failed read.
        let mut change = std::mem::take(&mut *self.pending.lock().unwrap());
        match self.apply_file_changes(&mut change) {
            Ok(()) => Ok(change),
            Err(e) => {
                let mut pending = self.pending.lock().unwrap();
                pending.add_connections(std::mem::take(&mut change.connections));
                pending.settings |= change.settings;
                Err(e)
            }
        }
    }

    /// Reload whichever file changed, folding the ids / settings flag into `change`. Mutates the
    /// cache and stamps only for a file it read in full, so a failure leaves `change` holding
    /// everything not yet reported (the caller restores it into `pending`).
    fn apply_file_changes(&self, change: &mut ConfigChange) -> Result<()> {
        let (known_conns, known_settings) = {
            let s = self.stamps.lock().unwrap();
            (s.connections, s.settings)
        };
        if stamp_of(&self.connections_path())? != known_conns {
            let (conns, stamp) = self.read_connections_file()?;
            let ids = changed_ids(&self.connections.read().unwrap(), &conns);
            *self.connections.write().unwrap() = conns;
            self.stamps.lock().unwrap().connections = stamp;
            change.add_connections(ids);
        }
        if stamp_of(&self.settings_path())? != known_settings {
            let (settings, stamp) = self.read_settings_file()?;
            let differs = *self.settings.read().unwrap() != settings;
            *self.settings.write().unwrap() = settings;
            self.stamps.lock().unwrap().settings = stamp;
            change.settings |= differs;
        }
        Ok(())
    }
}

/// Ids added, removed or changed between two connection lists, sorted.
fn changed_ids(old: &[Connection], new: &[Connection]) -> Vec<String> {
    let mut ids = BTreeSet::new();
    for c in new {
        if old.iter().find(|o| o.id == c.id) != Some(c) {
            ids.insert(c.id.clone());
        }
    }
    for o in old {
        if !new.iter().any(|c| c.id == o.id) {
            ids.insert(o.id.clone());
        }
    }
    ids.into_iter().collect()
}

/// What [`ConfigStore::reload_if_changed`] found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConfigChange {
    /// Ids of connections changed or removed.
    pub connections: Vec<String>,
    pub settings: bool,
}

impl ConfigChange {
    pub fn is_empty(&self) -> bool {
        self.connections.is_empty() && !self.settings
    }

    /// Merge `ids` into `connections` (sorted, unique).
    fn add_connections(&mut self, ids: impl IntoIterator<Item = String>) {
        let mut all: BTreeSet<String> = std::mem::take(&mut self.connections).into_iter().collect();
        all.extend(ids);
        self.connections = all.into_iter().collect();
    }
}

/// `Connection.write_allowed` after a save of `incoming` over `stored` (docs/db-copy.md
/// section 2): stored only as `Some(true)`; a new connection never starts write-allowed;
/// absent keeps the stored value unless the endpoint changed; `false` clears; `true` sets only
/// on an existing id. Owner windows re-applies this under the config lock (W1).
pub fn resolve_write_allowed(stored: Option<&Connection>, incoming: &Connection) -> Option<bool> {
    let stored = stored?;
    match incoming.write_allowed {
        Some(true) => Some(true),
        Some(false) => None,
        None if endpoint_changed(stored, incoming) => None,
        None => stored.write_allowed.filter(|w| *w),
    }
}

/// Whether a save points the connection at another server or database: driver, host, port,
/// path, database, connection string or SSH endpoint (host, port, user).
pub fn endpoint_changed(a: &Connection, b: &Connection) -> bool {
    let field = |c: &Connection, k: &str| c.fields.get(k).cloned().unwrap_or_default();
    let ssh = |c: &Connection| c.ssh.as_ref().filter(|s| s.enabled).map(|s| (s.host.clone(), s.port, s.user.clone()));
    a.driver != b.driver
        || a.host != b.host
        || a.port != b.port
        || a.database != b.database
        || field(a, "path") != field(b, "path")
        || field(a, CONNECTION_STRING_FIELD) != field(b, CONNECTION_STRING_FIELD)
        || ssh(a) != ssh(b)
}

/// Write via a temp file + rename so a crash never leaves a half-written config.
/// The temp file is per process (`<name>.<pid>.tmp`), and the rename is retried briefly when
/// Windows reports a sharing violation (a virus scanner or indexer holding the target open).
pub fn write_atomic(path: &Path, data: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let tmp = path.with_file_name(format!("{name}.{}.tmp", std::process::id()));
    std::fs::write(&tmp, data)?;
    let mut attempt = 0;
    loop {
        match std::fs::rename(&tmp, path) {
            Ok(()) => return Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied && attempt < 20 => {
                attempt += 1;
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(e) => {
                let _ = std::fs::remove_file(&tmp);
                return Err(e.into());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ConnTag;

    #[test]
    fn write_allowed_rules_in_upsert() {
        let dir = tempfile::tempdir().unwrap();
        let store = ConfigStore::open(dir.path().to_path_buf()).unwrap();
        // A new connection never starts write-allowed.
        let c = Connection { name: "tgt".into(), driver: "mysql".into(), host: Some("db".into()), write_allowed: Some(true), ..Default::default() };
        let saved = store.upsert(c).unwrap();
        assert_eq!(saved.write_allowed, None);
        // `true` on an existing id sets it; absent keeps it.
        let saved = store.upsert(Connection { write_allowed: Some(true), ..saved }).unwrap();
        assert_eq!(saved.write_allowed, Some(true));
        let saved = store.upsert(Connection { write_allowed: None, name: "tgt2".into(), ..saved }).unwrap();
        assert_eq!(saved.write_allowed, Some(true));
        // An endpoint edit clears it unless the same save sends `true`.
        let moved = store.upsert(Connection { write_allowed: None, host: Some("other".into()), ..saved.clone() }).unwrap();
        assert_eq!(moved.write_allowed, None);
        let kept = store.upsert(Connection { write_allowed: Some(true), ..moved }).unwrap();
        let kept = store.upsert(Connection { write_allowed: Some(true), port: Some(3307), ..kept }).unwrap();
        assert_eq!(kept.write_allowed, Some(true));
        // `false` clears; nothing but `true` is ever stored.
        let cleared = store.upsert(Connection { write_allowed: Some(false), ..kept }).unwrap();
        assert_eq!(cleared.write_allowed, None);
        let text = std::fs::read_to_string(store.connections_path()).unwrap();
        assert!(!text.contains("writeAllowed") && !text.contains("write_allowed"), "{text}");
    }

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

    /// The environment tag survives connections.toml; files written before the tag existed
    /// (no `tag` key) load as untagged, and an untagged connection writes no `tag` key.
    #[test]
    fn tag_roundtrips_and_old_files_load_untagged() {
        let dir = tempfile::tempdir().unwrap();
        let store = ConfigStore::open(dir.path().to_path_buf()).unwrap();
        let prod = store.upsert(Connection { name: "prod".into(), driver: "postgres".into(), tag: Some(ConnTag::Production), ..Default::default() }).unwrap();
        let plain = store.upsert(Connection { name: "plain".into(), driver: "sqlite".into(), ..Default::default() }).unwrap();
        let text = std::fs::read_to_string(store.connections_path()).unwrap();
        assert_eq!(text.matches("tag = ").count(), 1, "{text}");
        assert!(text.contains("tag = \"production\""), "{text}");

        let again = ConfigStore::open(dir.path().to_path_buf()).unwrap();
        assert_eq!(again.get(&prod.id).unwrap().tag, Some(ConnTag::Production));
        assert_eq!(again.get(&plain.id).unwrap().tag, None);
        assert_eq!(again.connections(), store.connections());

        // Clearing the tag removes the key from the file.
        let cleared = store.upsert(Connection { tag: None, ..prod.clone() }).unwrap();
        assert_eq!(cleared.tag, None);
        assert!(!std::fs::read_to_string(store.connections_path()).unwrap().contains("tag = "));

        // A file from before the tag existed.
        let old = "[[connections]]\nid = \"a1\"\nname = \"legacy\"\ndriver = \"sqlite\"\nsavePassword = true\n\n[connections.fields]\npath = \"x.db\"\n";
        std::fs::write(store.connections_path(), old).unwrap();
        let legacy = ConfigStore::open(dir.path().to_path_buf()).unwrap();
        let c = legacy.get("a1").unwrap();
        assert_eq!((c.name.as_str(), c.tag), ("legacy", None));
        assert_eq!(c.fields.get("path").map(String::as_str), Some("x.db"));

        // Every tag spelling written by hand reads back; a misspelt one is a config error.
        for tag in ConnTag::ALL {
            std::fs::write(store.connections_path(), format!("[[connections]]\nid = \"a1\"\nname = \"n\"\ndriver = \"sqlite\"\ntag = \"{tag}\"\n")).unwrap();
            assert_eq!(ConfigStore::open(dir.path().to_path_buf()).unwrap().get("a1").unwrap().tag, Some(tag));
        }
        std::fs::write(store.connections_path(), "[[connections]]\nid = \"a1\"\nname = \"n\"\ndriver = \"sqlite\"\ntag = \"prod\"\n").unwrap();
        assert!(ConfigStore::open(dir.path().to_path_buf()).is_err());
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

    // ------------------------------------------------------------ several processes (1.2)

    fn named(name: &str) -> Connection {
        Connection { name: name.into(), driver: "sqlite".into(), ..Default::default() }
    }

    fn names(store: &ConfigStore) -> Vec<String> {
        let mut n: Vec<String> = store.connections().into_iter().map(|c| c.name).collect();
        n.sort();
        n
    }

    /// Two stores on one directory (two processes) saving at the same time: every save
    /// survives, because each one re-reads the file under the lock and applies only its delta.
    #[test]
    fn two_writers_never_lose_an_update() {
        let dir = tempfile::tempdir().unwrap();
        let a = std::sync::Arc::new(ConfigStore::open(dir.path().to_path_buf()).unwrap());
        let b = std::sync::Arc::new(ConfigStore::open(dir.path().to_path_buf()).unwrap());
        let writers: Vec<_> = [("a", a.clone()), ("b", b.clone())]
            .into_iter()
            .map(|(who, store)| {
                std::thread::spawn(move || {
                    for i in 0..15 {
                        store.upsert(named(&format!("{who}{i:02}"))).unwrap();
                    }
                })
            })
            .collect();
        for w in writers {
            w.join().unwrap();
        }
        let disk = ConfigStore::open(dir.path().to_path_buf()).unwrap();
        assert_eq!(disk.connections().len(), 30, "{:?}", names(&disk));
        // Each store picks up the other's saves.
        a.reload_if_changed().unwrap();
        b.reload_if_changed().unwrap();
        assert_eq!(names(&a), names(&disk));
        assert_eq!(names(&b), names(&disk));
    }

    /// Child half of [`two_processes_never_lose_an_update`]: a no-op unless the parent test
    /// set `BULKDB_CONFIG_CHILD_DIR`.
    #[test]
    fn config_child_writer() {
        let Ok(dir) = std::env::var("BULKDB_CONFIG_CHILD_DIR") else { return };
        let who = std::env::var("BULKDB_CONFIG_CHILD_NAME").unwrap();
        let store = ConfigStore::open(PathBuf::from(dir)).unwrap();
        for i in 0..10 {
            store.upsert(named(&format!("{who}{i:02}"))).unwrap();
        }
    }

    /// The same with real processes: two children of this test binary and this process write
    /// into one directory at once.
    #[test]
    fn two_processes_never_lose_an_update() {
        let dir = tempfile::tempdir().unwrap();
        let exe = std::env::current_exe().unwrap();
        let children: Vec<_> = ["p", "q"]
            .into_iter()
            .map(|who| {
                std::process::Command::new(&exe)
                    .args(["--exact", "config::tests::config_child_writer", "--test-threads", "1", "--quiet"])
                    .env("BULKDB_CONFIG_CHILD_DIR", dir.path())
                    .env("BULKDB_CONFIG_CHILD_NAME", who)
                    .stdout(std::process::Stdio::null())
                    .spawn()
                    .unwrap()
            })
            .collect();
        let store = ConfigStore::open(dir.path().to_path_buf()).unwrap();
        for i in 0..10 {
            store.upsert(named(&format!("me{i:02}"))).unwrap();
        }
        for mut c in children {
            assert!(c.wait().unwrap().success());
        }
        let disk = ConfigStore::open(dir.path().to_path_buf()).unwrap();
        assert_eq!(disk.connections().len(), 30, "{:?}", names(&disk));
        let change = store.reload_if_changed().unwrap();
        assert_eq!(change.connections.len(), 20, "the children's 20 connections are reported: {change:?}");
        assert_eq!(names(&store), names(&disk));
    }

    #[test]
    fn reload_if_changed_reports_changed_and_removed_ids_but_never_own_writes() {
        let dir = tempfile::tempdir().unwrap();
        let a = ConfigStore::open(dir.path().to_path_buf()).unwrap();
        let x = a.upsert(named("x")).unwrap();
        let y = a.upsert(named("y")).unwrap();
        let b = ConfigStore::open(dir.path().to_path_buf()).unwrap();
        assert!(a.reload_if_changed().unwrap().is_empty(), "own writes are not changes");
        assert!(b.reload_if_changed().unwrap().is_empty());

        a.upsert(Connection { name: "x2".into(), ..x.clone() }).unwrap();
        a.delete(&y.id).unwrap();
        let z = a.upsert(named("z")).unwrap();
        let mut want = vec![x.id.clone(), y.id.clone(), z.id.clone()];
        want.sort();
        let change = b.reload_if_changed().unwrap();
        assert_eq!(change, ConfigChange { connections: want, settings: false });
        assert_eq!(b.get(&x.id).unwrap().name, "x2");
        assert!(b.get(&y.id).is_none());
        assert!(b.reload_if_changed().unwrap().is_empty(), "reported once");

        // Settings: a changed object is reported, an identical rewrite is not.
        let mut s = a.settings();
        s.max_concurrent_jobs = 3;
        a.set_settings(s.clone()).unwrap();
        let change = b.reload_if_changed().unwrap();
        assert!(change.settings && change.connections.is_empty(), "{change:?}");
        assert_eq!(b.settings().max_concurrent_jobs, 3);
        a.set_settings(s).unwrap();
        assert!(!b.reload_if_changed().unwrap().settings);
        assert!(a.reload_if_changed().unwrap().is_empty());
    }

    /// A locked write folds another process's changes into the cache; the next poll still
    /// reports them (so their catalog caches are dropped), but not the write's own id.
    #[test]
    fn a_locked_write_queues_the_other_process_changes() {
        let dir = tempfile::tempdir().unwrap();
        let a = ConfigStore::open(dir.path().to_path_buf()).unwrap();
        let b = ConfigStore::open(dir.path().to_path_buf()).unwrap();
        let x = a.upsert(named("x")).unwrap();
        let y = b.upsert(named("y")).unwrap();
        assert_eq!(names(&b), ["x", "y"], "b's save kept a's connection");
        assert_eq!(b.reload_if_changed().unwrap().connections, vec![x.id]);
        assert_eq!(a.reload_if_changed().unwrap().connections, vec![y.id]);
    }

    /// A read that fails mid-poll must not drop the ids a locked write already folded in: they
    /// are queued back and reported on the next (successful) poll, so the other windows still
    /// drop their catalog/browser caches for the foreign change.
    #[test]
    fn reload_if_changed_keeps_pending_when_a_read_fails() {
        let dir = tempfile::tempdir().unwrap();
        let a = ConfigStore::open(dir.path().to_path_buf()).unwrap();
        let b = ConfigStore::open(dir.path().to_path_buf()).unwrap();
        // a adds x; b's own locked write folds x into b's cache and queues its id for b's poll.
        let x = a.upsert(named("x")).unwrap();
        b.upsert(named("y")).unwrap();

        // A hand-edited settings.toml that does not parse makes the settings read fail. b's
        // connections are current (b just wrote them), so only the settings branch runs and errors.
        std::fs::write(b.settings_path(), "this is not = valid = toml").unwrap();
        assert!(b.reload_if_changed().is_err(), "the unparsable settings file fails the poll");

        // Fix the file; the queued foreign id is still reported, not lost after the failed read.
        std::fs::write(b.settings_path(), "maxConcurrentJobs = 2\n").unwrap();
        let change = b.reload_if_changed().unwrap();
        assert_eq!(change.connections, vec![x.id], "x is still reported: {change:?}");
        assert!(change.settings, "the now-valid settings change is reported too");
        assert_eq!(b.settings().max_concurrent_jobs, 2);
        assert!(b.reload_if_changed().unwrap().is_empty(), "reported once");
    }

    /// `write_allowed` is resolved against the file under the lock: a process with a stale
    /// cache can neither revive nor drop the flag by saving an unrelated edit.
    #[test]
    fn a_stale_process_cannot_revive_or_drop_write_allowed() {
        let dir = tempfile::tempdir().unwrap();
        let a = ConfigStore::open(dir.path().to_path_buf()).unwrap();
        let c = a.upsert(Connection { host: Some("h".into()), ..named("tgt") }).unwrap();
        let b = ConfigStore::open(dir.path().to_path_buf()).unwrap();
        let stale = b.get(&c.id).unwrap();
        assert_eq!(stale.write_allowed, None);

        // a marks it write-allowed; b (stale) renames it: the flag stays.
        a.upsert(Connection { write_allowed: Some(true), ..c.clone() }).unwrap();
        let renamed = b.upsert(Connection { name: "tgt-renamed".into(), write_allowed: None, ..stale.clone() }).unwrap();
        assert_eq!(renamed.write_allowed, Some(true));
        assert_eq!(ConfigStore::open(dir.path().to_path_buf()).unwrap().get(&c.id).unwrap().write_allowed, Some(true));

        // a clears it; b, still believing it set, saves again: it stays cleared.
        a.reload_if_changed().unwrap();
        a.upsert(Connection { write_allowed: Some(false), ..a.get(&c.id).unwrap() }).unwrap();
        let again = b.upsert(Connection { name: "tgt-again".into(), write_allowed: None, ..renamed }).unwrap();
        assert_eq!(again.write_allowed, None);
        assert_eq!(ConfigStore::open(dir.path().to_path_buf()).unwrap().get(&c.id).unwrap().write_allowed, None);
    }

    /// A write waits for another holder of `.config.lock` and proceeds once it is released.
    #[test]
    fn a_write_waits_for_the_lock() {
        let dir = tempfile::tempdir().unwrap();
        let store = ConfigStore::open(dir.path().to_path_buf()).unwrap();
        let holder = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(dir.path().join(CONFIG_LOCK_FILE)).unwrap();
        assert!(fs4::fs_std::FileExt::try_lock_exclusive(&holder).unwrap());
        let release = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            fs4::fs_std::FileExt::unlock(&holder).unwrap();
        });
        let t = Instant::now();
        store.upsert(named("after")).unwrap();
        assert!(t.elapsed() >= Duration::from_millis(250), "the write waited: {:?}", t.elapsed());
        release.join().unwrap();
        assert_eq!(names(&store), ["after"]);
    }

    /// A lock held for longer than [`CONFIG_LOCK_WAIT`] fails the write with `IO` and changes
    /// nothing.
    #[test]
    fn a_write_gives_up_after_the_lock_wait() {
        let dir = tempfile::tempdir().unwrap();
        let store = ConfigStore::open(dir.path().to_path_buf()).unwrap();
        let holder = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(dir.path().join(CONFIG_LOCK_FILE)).unwrap();
        assert!(fs4::fs_std::FileExt::try_lock_exclusive(&holder).unwrap());
        let t = Instant::now();
        let err = store.set_settings(Settings::default()).unwrap_err();
        assert_eq!(err.code(), "IO");
        assert!(err.to_string().contains("다른 BulkDB가 설정을 저장하는 중입니다"), "{err}");
        assert!(t.elapsed() >= CONFIG_LOCK_WAIT, "{:?}", t.elapsed());
        assert!(!store.settings_path().exists());
        fs4::fs_std::FileExt::unlock(&holder).unwrap();
    }

    #[test]
    fn write_atomic_leaves_no_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.toml");
        write_atomic(&path, b"one").unwrap();
        write_atomic(&path, b"two").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "two");
        let entries: Vec<_> = std::fs::read_dir(dir.path()).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(entries, [std::ffi::OsString::from("x.toml")]);
    }
}
