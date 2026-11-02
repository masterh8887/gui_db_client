//! Several BulkDB processes on one config directory (docs/db-copy.md section 1.2). Each process
//! registers `<config_dir>/instances/<pid>.lock`, exclusively locked for its lifetime, and
//! `<pid>.json` (`InstanceInfo`: `{pid, instanceId, kind, startedAt, version}`). The lock lives
//! in a file of its own because on Windows a locked range cannot be read.
//!
//! Liveness is the lock: a `.lock` that another process can lock is stale (its process ended
//! without cleaning up), and the scanner deletes it with its json; one it cannot lock belongs to
//! a live process. A json is only trusted together with a held lock, and `is_alive` matches the
//! `instanceId`, so a reused pid never makes an old instance look alive.
//!
//! A crash can also leave a `.lock` without its json (between locking and writing the json, or
//! between the two removals of an orderly exit): [`info`] removes such a lone lock file once it
//! is older than a registration takes and nobody holds it. A json naming this process's pid but
//! an id this process never registered belongs to a dead predecessor that had the same pid.
//!
//! A second registration inside one process (tests, two facades) uses `<pid>-<n>` file names.

use std::collections::HashSet;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use crate::error::{CoreError, Result};
use crate::types::{InstanceInfo, InstancesInfo};

/// Sub-directory of the config directory holding the registrations.
pub const INSTANCES_DIR: &str = "instances";
/// A json without its lock file is removed once it is this old (a registration in progress
/// creates the lock first, so a younger one is left alone).
const ORPHAN_JSON_AGE: Duration = Duration::from_secs(60);
/// A `.lock` without its json is removed once it is this old and unlocked (a registration in
/// progress holds its lock within microseconds of creating it and writes the json right after).
const ORPHAN_LOCK_AGE: Duration = Duration::from_secs(10);
/// How often `register` retries a lock that a scanner holds for a moment.
const REGISTER_ATTEMPTS: u32 = 40;

/// Registrations of this process: `instance_id`s, alive without any file test.
static OWN: Mutex<Option<HashSet<String>>> = Mutex::new(None);
/// Number of registrations made by this process so far (file stem `<pid>` for the first).
static SLOTS: AtomicU32 = AtomicU32::new(0);

fn own_ids<T>(f: impl FnOnce(&mut HashSet<String>) -> T) -> T {
    let mut guard = OWN.lock().unwrap_or_else(|e| e.into_inner());
    f(guard.get_or_insert_with(HashSet::new))
}

/// Keeps this process registered; dropping it unregisters (removes its files).
#[derive(Debug)]
pub struct InstanceGuard {
    instance_id: String,
    kind: String,
    /// Always `Some` while the guard lives; taken by `drop`.
    files: Option<GuardFiles>,
}

#[derive(Debug)]
struct GuardFiles {
    lock_path: PathBuf,
    json_path: PathBuf,
    /// Held open and locked for the guard's lifetime.
    lock: File,
}

impl InstanceGuard {
    /// uuid v4 of this process's registration.
    pub fn instance_id(&self) -> &str {
        &self.instance_id
    }

    /// `desktop` or `cli`.
    pub fn kind(&self) -> &str {
        &self.kind
    }
}

impl Drop for InstanceGuard {
    fn drop(&mut self) {
        own_ids(|ids| ids.remove(&self.instance_id));
        if let Some(files) = self.files.take() {
            // json first (no live json without its lock), then the lock file while still
            // holding the lock, so nobody can lock it between the unlock and the removal.
            let _ = std::fs::remove_file(&files.json_path);
            let _ = std::fs::remove_file(&files.lock_path);
            let _ = fs4::fs_std::FileExt::unlock(&files.lock);
            drop(files.lock);
        }
    }
}

fn instances_dir(config_dir: &Path) -> PathBuf {
    config_dir.join(INSTANCES_DIR)
}

/// Open (creating) and exclusively lock `path`; `None` when another process holds it.
fn lock_file(path: &Path) -> Result<Option<File>> {
    let file = std::fs::OpenOptions::new().create(true).read(true).write(true).truncate(false).open(path)?;
    // Fully qualified: std's `File` has inherent `lock*` methods of its own.
    Ok(fs4::fs_std::FileExt::try_lock_exclusive(&file)?.then_some(file))
}

/// Register this process (`kind`: `desktop`, `cli`): lock `<pid>.lock` first, then write
/// `<pid>.json`. If a scanner deleted the lock file between its creation and the lock (it saw
/// an unlocked file and took it for stale), the file is recreated and locked again.
pub fn register(config_dir: &Path, kind: &str) -> Result<InstanceGuard> {
    let dir = instances_dir(config_dir);
    std::fs::create_dir_all(&dir)?;
    let pid = std::process::id();
    let slot = SLOTS.fetch_add(1, Ordering::SeqCst);
    let stem = if slot == 0 { pid.to_string() } else { format!("{pid}-{slot}") };
    let lock_path = dir.join(format!("{stem}.lock"));
    let json_path = dir.join(format!("{stem}.json"));

    // No live process shares this pid, so a held `<stem>.lock` can only be a scanner looking at
    // a stale file of a dead predecessor with the same pid for a moment (it removes the file
    // while holding the lock): wait for it. A lock on a file deleted under us is retried too.
    let mut lock = None;
    let mut last_err = None;
    for _ in 0..REGISTER_ATTEMPTS {
        match lock_file(&lock_path) {
            Ok(Some(file)) if lock_path.exists() => {
                lock = Some(file);
                break;
            }
            Ok(Some(file)) => {
                // Deleted under us: our lock is on an unlinked file. Try again with a fresh one.
                let _ = fs4::fs_std::FileExt::unlock(&file);
            }
            Ok(None) => last_err = Some(CoreError::Io(format!("instance registration {} is locked by another process", lock_path.display()))),
            // Windows: a file deleted while another handle is open cannot be reopened until
            // that handle closes.
            Err(e) => last_err = Some(e),
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    let lock = lock.ok_or_else(|| last_err.unwrap_or_else(|| CoreError::Io(format!("cannot keep {} registered", lock_path.display()))))?;

    let instance_id = new_instance_id();
    let info = InstanceInfo {
        pid,
        instance_id: instance_id.clone(),
        kind: kind.to_string(),
        started_at: crate::types::now_rfc3339(),
        version: env!("CARGO_PKG_VERSION").to_string(),
    };
    crate::config::write_atomic(&json_path, serde_json::to_string_pretty(&info)?.as_bytes())?;
    own_ids(|ids| ids.insert(instance_id.clone()));
    Ok(InstanceGuard { instance_id, kind: kind.to_string(), files: Some(GuardFiles { lock_path, json_path, lock }) })
}

/// One registration found on disk.
struct Found {
    json_path: PathBuf,
    lock_path: PathBuf,
}

/// Every `<stem>.json` in the directory with its lock path.
fn registrations(dir: &Path) -> Vec<Found> {
    let Ok(entries) = std::fs::read_dir(dir) else { return vec![] };
    entries
        .flatten()
        .filter_map(|e| {
            let path = e.path();
            (path.extension().and_then(|x| x.to_str()) == Some("json")).then(|| Found { lock_path: path.with_extension("lock"), json_path: path })
        })
        .collect()
}

/// Liveness of a registration: `Some(true)` live (its lock is held elsewhere), `Some(false)`
/// stale (files removed here), `None` undecided (no lock file: a registration in progress, or
/// an orphan json, removed once old).
fn check(found: &Found) -> Option<bool> {
    let file = match std::fs::OpenOptions::new().read(true).write(true).open(&found.lock_path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let old = std::fs::metadata(&found.json_path)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|m| SystemTime::now().duration_since(m).ok())
                .is_some_and(|age| age > ORPHAN_JSON_AGE);
            if old {
                let _ = std::fs::remove_file(&found.json_path);
            }
            return None;
        }
        Err(_) => return None,
    };
    match fs4::fs_std::FileExt::try_lock_exclusive(&file) {
        Ok(false) => Some(true),
        Ok(true) => {
            // Nobody holds it: the process is gone. Remove its files (json first, so no live
            // json is ever left without its lock), the lock file while still holding the lock.
            let _ = std::fs::remove_file(&found.json_path);
            let _ = std::fs::remove_file(&found.lock_path);
            let _ = fs4::fs_std::FileExt::unlock(&file);
            Some(false)
        }
        Err(_) => None,
    }
}

/// Remove `.lock` files that have no json, are older than [`ORPHAN_LOCK_AGE`] and that nobody
/// holds: left by a process that crashed between locking and writing its json, or between the
/// two removals of its exit. A held one (a live process mid-registration) is kept. Returns the
/// number removed.
fn sweep_lone_locks(dir: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else { return 0 };
    let mut removed = 0;
    for path in entries.flatten().map(|e| e.path()) {
        if path.extension().and_then(|x| x.to_str()) != Some("lock") || path.with_extension("json").exists() {
            continue;
        }
        let old =
            std::fs::metadata(&path).and_then(|m| m.modified()).ok().and_then(|m| SystemTime::now().duration_since(m).ok()).is_some_and(|age| age > ORPHAN_LOCK_AGE);
        if !old {
            continue;
        }
        let Ok(file) = std::fs::OpenOptions::new().read(true).write(true).open(&path) else { continue };
        if fs4::fs_std::FileExt::try_lock_exclusive(&file).unwrap_or(false) {
            // Re-check under the lock: a registration that locked and wrote its json meanwhile
            // would have made the lock unavailable, so a json now means "leave it".
            if !path.with_extension("json").exists() && std::fs::remove_file(&path).is_ok() {
                removed += 1;
            }
            let _ = fs4::fs_std::FileExt::unlock(&file);
        }
    }
    removed
}

fn read_info(path: &Path) -> Option<InstanceInfo> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

/// The other live instances (own pid skipped; stale files removed), oldest first.
pub fn info(config_dir: &Path) -> Result<InstancesInfo> {
    let self_pid = std::process::id();
    let mut others = Vec::new();
    let dir = instances_dir(config_dir);
    sweep_lone_locks(&dir);
    for found in registrations(&dir) {
        let Some(info) = read_info(&found.json_path) else { continue };
        if info.pid == self_pid {
            continue;
        }
        if check(&found) == Some(true) {
            others.push(info);
        }
    }
    others.sort_by(|a, b| a.started_at.cmp(&b.started_at).then(a.pid.cmp(&b.pid)));
    Ok(InstancesInfo { self_pid, others })
}

/// Whether the instance `instance_id` is alive: one of this process's registrations, or a json
/// naming exactly this id whose lock is held by another process. A reused pid with another id
/// is not alive, and neither is an id whose json names this process's own pid without being one
/// of its registrations (a dead predecessor with the same pid). A stale registration found on
/// the way is removed.
pub fn is_alive(config_dir: &Path, instance_id: &str) -> bool {
    if instance_id.is_empty() {
        return false;
    }
    if own_ids(|ids| ids.contains(instance_id)) {
        return true;
    }
    let self_pid = std::process::id();
    registrations(&instances_dir(config_dir))
        .into_iter()
        .find_map(|f| read_info(&f.json_path).filter(|i| i.instance_id == instance_id).map(|i| (f, i.pid)))
        .is_some_and(|(f, pid)| pid != self_pid && check(&f) == Some(true))
}

/// A fresh uuid v4 instance id.
pub fn new_instance_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_json(dir: &Path, stem: &str, info: &InstanceInfo) {
        std::fs::write(dir.join(format!("{stem}.json")), serde_json::to_string(info).unwrap()).unwrap();
    }

    fn foreign(pid: u32, id: &str) -> InstanceInfo {
        InstanceInfo { pid, instance_id: id.into(), kind: "cli".into(), started_at: "2026-10-27T10:00:00.000+09:00".into(), version: "0.1.0".into() }
    }

    #[test]
    fn register_locks_before_writing_and_cleans_up_on_drop() {
        let cfg = tempfile::tempdir().unwrap();
        let guard = register(cfg.path(), "desktop").unwrap();
        let dir = instances_dir(cfg.path());
        let jsons: Vec<_> = registrations(&dir);
        assert_eq!(jsons.len(), 1);
        let info = read_info(&jsons[0].json_path).unwrap();
        assert_eq!((info.pid, info.kind.as_str(), info.instance_id.as_str()), (std::process::id(), "desktop", guard.instance_id()));
        assert_eq!(info.version, env!("CARGO_PKG_VERSION"));
        // The lock is held: another handle cannot take it, so a scan sees it as live.
        assert_eq!(check(&jsons[0]), Some(true));
        assert!(jsons[0].lock_path.exists() && jsons[0].json_path.exists());
        // Own pid is skipped by `info`, but the own id is alive.
        assert!(info_of(cfg.path()).is_empty());
        assert!(is_alive(cfg.path(), guard.instance_id()));
        let id = guard.instance_id().to_string();
        drop(guard);
        assert!(registrations(&dir).is_empty(), "json removed");
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0, "lock removed");
        assert!(!is_alive(cfg.path(), &id));
    }

    fn info_of(cfg: &Path) -> Vec<InstanceInfo> {
        info(cfg).unwrap().others
    }

    /// A process that died without cleaning up leaves an unlocked lock file: the scan removes
    /// both files and does not list it.
    #[test]
    fn stale_registrations_are_removed() {
        let cfg = tempfile::tempdir().unwrap();
        let dir = instances_dir(cfg.path());
        std::fs::create_dir_all(&dir).unwrap();
        write_json(&dir, "999991", &foreign(999_991, "dead-id"));
        std::fs::write(dir.join("999991.lock"), b"").unwrap();
        assert!(info_of(cfg.path()).is_empty());
        assert!(!dir.join("999991.json").exists() && !dir.join("999991.lock").exists());
        assert!(!is_alive(cfg.path(), "dead-id"));

        // A fresh json without its lock (registration in progress) is left alone.
        write_json(&dir, "999992", &foreign(999_992, "young"));
        assert!(info_of(cfg.path()).is_empty());
        assert!(dir.join("999992.json").exists());
    }

    /// Another live process: its lock is held (simulated by a second handle held here, which
    /// conflicts like another process would), its json is listed and its id is alive; a json
    /// naming another id under the same pid (pid reuse) is not.
    #[test]
    fn live_instances_are_listed_and_matched_by_instance_id() {
        let cfg = tempfile::tempdir().unwrap();
        let dir = instances_dir(cfg.path());
        std::fs::create_dir_all(&dir).unwrap();
        write_json(&dir, "999993", &foreign(999_993, "live-id"));
        let held = lock_file(&dir.join("999993.lock")).unwrap().expect("lock taken");
        let others = info_of(cfg.path());
        assert_eq!(others.iter().map(|i| (i.pid, i.instance_id.as_str())).collect::<Vec<_>>(), [(999_993, "live-id")]);
        assert!(is_alive(cfg.path(), "live-id"));
        assert!(!is_alive(cfg.path(), "another-id"), "same pid, other instance id");
        assert!(!is_alive(cfg.path(), ""));
        drop(held);
        // The holder is gone: stale now.
        assert!(!is_alive(cfg.path(), "live-id"));
        assert!(info_of(cfg.path()).is_empty());
    }

    fn age(path: &Path, by: Duration) {
        let f = std::fs::OpenOptions::new().write(true).open(path).unwrap();
        f.set_modified(SystemTime::now() - by).unwrap();
    }

    /// A crash between locking and writing the json (or between the two removals of an exit)
    /// leaves a lone `.lock`: removed once old and unlocked; a young one or a held one stays.
    #[test]
    fn lone_lock_files_are_swept_when_old_and_unlocked() {
        let cfg = tempfile::tempdir().unwrap();
        let dir = instances_dir(cfg.path());
        std::fs::create_dir_all(&dir).unwrap();
        let stale = dir.join("999994.lock");
        let young = dir.join("999995.lock");
        let held_path = dir.join("999996.lock");
        for p in [&stale, &young, &held_path] {
            std::fs::write(p, b"").unwrap();
        }
        age(&stale, Duration::from_secs(120));
        age(&held_path, Duration::from_secs(120));
        let held = lock_file(&held_path).unwrap().expect("lock taken");
        assert!(info_of(cfg.path()).is_empty());
        assert!(!stale.exists(), "old, unlocked lone lock removed");
        assert!(young.exists(), "a registration may be in progress");
        assert!(held_path.exists(), "held by a live process");
        drop(held);
        // A lock with its json is never swept as lone (the json decides through `check`).
        let paired = dir.join("999997.lock");
        std::fs::write(&paired, b"").unwrap();
        age(&paired, Duration::from_secs(120));
        write_json(&dir, "999997", &foreign(999_997, "paired"));
        assert_eq!(sweep_lone_locks(&dir), 1, "only the formerly held lone lock");
        assert!(paired.exists());
    }

    /// A json naming this process's own pid but an id it never registered is a dead predecessor
    /// that had the same pid: not alive, even while some lock of this process is held.
    #[test]
    fn own_pid_with_unknown_id_is_not_alive() {
        let cfg = tempfile::tempdir().unwrap();
        let dir = instances_dir(cfg.path());
        std::fs::create_dir_all(&dir).unwrap();
        let stem = format!("{}-old", std::process::id());
        write_json(&dir, &stem, &foreign(std::process::id(), "predecessor"));
        let _held = lock_file(&dir.join(format!("{stem}.lock"))).unwrap().expect("lock taken");
        assert!(!is_alive(cfg.path(), "predecessor"));
    }

    const CHILD_ENV: &str = "BULKDB_INSTANCES_CHILD_DIR";

    /// Not a test on its own: the body of the child process of
    /// `another_process_is_alive_until_it_crashes`. Without the env var it does nothing.
    #[test]
    fn child_process_helper() {
        let Some(cfg) = std::env::var_os(CHILD_ENV) else { return };
        let cfg = PathBuf::from(cfg);
        let guard = register(&cfg, "cli").unwrap();
        std::fs::write(cfg.join("child-ready.tmp"), guard.instance_id()).unwrap();
        std::fs::rename(cfg.join("child-ready.tmp"), cfg.join("child-ready")).unwrap();
        // Wait to be killed (a crash: the guard never drops); give up after a while.
        let deadline = std::time::Instant::now() + Duration::from_secs(60);
        while std::time::Instant::now() < deadline && !cfg.join("child-stop").exists() {
            std::thread::sleep(Duration::from_millis(50));
        }
        std::mem::forget(guard);
    }

    /// A real second process: listed and alive while it runs; killed (no cleanup), its lock is
    /// released by the OS, so it is no longer alive and its files are removed by the scan.
    #[test]
    fn another_process_is_alive_until_it_crashes() {
        let cfg = tempfile::tempdir().unwrap();
        let exe = std::env::current_exe().unwrap();
        let mut child = std::process::Command::new(exe)
            .args(["instances::tests::child_process_helper", "--exact", "--test-threads=1", "--nocapture"])
            .env(CHILD_ENV, cfg.path())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let ready = cfg.path().join("child-ready");
        let deadline = std::time::Instant::now() + Duration::from_secs(60);
        while !ready.exists() {
            assert!(std::time::Instant::now() < deadline, "child never registered");
            std::thread::sleep(Duration::from_millis(25));
        }
        let child_id = std::fs::read_to_string(&ready).unwrap();
        let others = info_of(cfg.path());
        assert_eq!(others.iter().map(|i| (i.pid, i.instance_id.as_str(), i.kind.as_str())).collect::<Vec<_>>(), [(child.id(), child_id.as_str(), "cli")]);
        assert!(is_alive(cfg.path(), &child_id));
        assert!(!is_alive(cfg.path(), "some-other-id"));

        child.kill().unwrap();
        child.wait().unwrap();
        // The OS releases the lock of a killed process (on Windows possibly a moment later).
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while is_alive(cfg.path(), &child_id) {
            assert!(std::time::Instant::now() < deadline, "a killed process stays alive");
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(info_of(cfg.path()).is_empty());
        assert!(registrations(&instances_dir(cfg.path())).is_empty(), "stale json removed");
        assert!(!instances_dir(cfg.path()).join(format!("{}.lock", child.id())).exists(), "stale lock removed");
    }

    /// The other registrations of a process (a second facade) use their own files; `info`
    /// skips the own pid either way.
    #[test]
    fn two_registrations_in_one_process() {
        let cfg = tempfile::tempdir().unwrap();
        let a = register(cfg.path(), "desktop").unwrap();
        let b = register(cfg.path(), "cli").unwrap();
        assert_ne!(a.instance_id(), b.instance_id());
        assert_eq!(registrations(&instances_dir(cfg.path())).len(), 2);
        assert!(info_of(cfg.path()).is_empty());
        assert!(is_alive(cfg.path(), a.instance_id()) && is_alive(cfg.path(), b.instance_id()));
        assert_eq!(b.kind(), "cli");
    }
}
