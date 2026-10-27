//! `BulkDb` facade tests over a SQLite fixture: connection save/test, catalog browsing,
//! preflight, the full export job state machine (done / split / warned+resume / cancelled),
//! the exact-count job, the diagnostics bundle and the DBeaver importer.
//!
//! Everything runs through the public facade exactly as the Tauri shell and the CLI do; no
//! driver or sink internals are touched.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bulk_core::credentials::{self, MemoryStore};
use bulk_core::service::{BulkDb, BulkDbOptions};
use bulk_core::testdata::sqlite_fixture;
use bulk_core::types::*;
use bulk_core::CoreError;
use tokio::sync::broadcast;

const DONE_TIMEOUT: Duration = Duration::from_secs(120);

struct Harness {
    /// Keeps the fixture, config dir and output dir alive for the test.
    dir: tempfile::TempDir,
    db: Arc<BulkDb>,
    conn_id: String,
    out: PathBuf,
}

/// Fresh config dir + memory credential store + a saved sqlite connection over a fixture with
/// `wide` wide rows and `nasty` nasty rows.
async fn harness(wide: u64, nasty: u64) -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let fixture = dir.path().join("fixture.db");
    sqlite_fixture(&fixture, wide, 0, nasty).unwrap();
    let out = dir.path().join("out");
    std::fs::create_dir_all(&out).unwrap();
    let db = BulkDb::new(BulkDbOptions {
        config_dir: Some(dir.path().join("config")),
        credentials: Some(Box::new(MemoryStore::default())),
        init_logging: false,
        prompts: None,
    })
    .await
    .unwrap();
    let saved = db.conn_save(sqlite_input("fixture", &fixture, true, None)).unwrap();
    assert!(!saved.id.is_empty(), "conn_save assigns an id");
    Harness { dir, db, conn_id: saved.id, out }
}

fn sqlite_input(name: &str, path: &Path, save_password: bool, password: Option<&str>) -> ConnectionInput {
    let mut fields = BTreeMap::new();
    fields.insert("path".to_string(), path.display().to_string());
    ConnectionInput {
        connection: Connection { name: name.into(), driver: "sqlite".into(), fields, save_password, ..Default::default() },
        password: password.map(str::to_string),
        ..Default::default()
    }
}

/// A CSV export request whose file is `<out>/<name>.csv` (pattern `{name}`), so tests know the
/// path in advance and two exports in the same minute never collide.
fn request(conn_id: &str, name: &str, sql: &str, out: &Path, split: Option<SplitOpts>) -> ExportRequest {
    ExportRequest {
        conn_id: conn_id.into(),
        sql: Some(sql.into()),
        sql_file: None,
        name: Some(name.into()),
        session: SessionOpts::default(),
        output: OutputOpts { dir: out.display().to_string(), name_pattern: "{name}".into(), split, ..Default::default() },
        password: None,
    }
}

/// Block until `job:done` for `job_id` arrives on the event bus.
async fn wait_done(rx: &mut broadcast::Receiver<CoreEvent>, job_id: &str) -> JobDone {
    tokio::time::timeout(DONE_TIMEOUT, async {
        loop {
            match rx.recv().await {
                Ok(CoreEvent::JobDone(d)) if d.job_id == job_id => return d,
                Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => panic!("event bus closed before job {job_id} finished"),
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("job {job_id} did not finish within {DONE_TIMEOUT:?}"))
}

/// Block until a `job:state` event moves `job_id` into `state`. Panics if the job finishes first.
async fn wait_state(rx: &mut broadcast::Receiver<CoreEvent>, job_id: &str, state: JobState) {
    tokio::time::timeout(DONE_TIMEOUT, async {
        loop {
            match rx.recv().await {
                Ok(CoreEvent::JobState(ev)) if ev.job_id == job_id && ev.state == state => return,
                Ok(CoreEvent::JobDone(d)) if d.job_id == job_id => panic!("job {job_id} finished as {:?} before reaching {state:?}", d.state),
                Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => panic!("event bus closed"),
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("job {job_id} did not reach {state:?} within {DONE_TIMEOUT:?}"))
}

fn line_count(path: &Path) -> usize {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display())).lines().count()
}

fn files_named(dir: &Path, pred: impl Fn(&str) -> bool) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> =
        std::fs::read_dir(dir).unwrap().filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| pred(&p.file_name().unwrap().to_string_lossy())).collect();
    v.sort();
    v
}

fn warning_codes(report: &PreflightReport) -> Vec<String> {
    report.warnings.iter().map(|w| w.code.clone()).collect()
}

// ---------------------------------------------------------------------------------------------
// Connections and catalog
// ---------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn save_test_and_browse_a_sqlite_connection() {
    let h = harness(1000, 500).await;
    let db = &h.db;

    let listed = db.conn_list();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, h.conn_id);
    assert_eq!(listed[0].driver, "sqlite");
    assert!(!listed[0].has_password && !listed[0].has_ssh_secret);
    assert!(db.drivers_list().iter().any(|d| d.id == "sqlite" && d.available));

    let probe = db.conn_test(&h.conn_id, None).await.unwrap();
    assert_eq!(probe.server.driver, "sqlite");
    assert!(!probe.tunnel_used);
    assert_eq!(probe.steps, vec!["connect".to_string(), "probe".to_string()]);
    assert!(matches!(db.conn_test("missing", None).await, Err(CoreError::NotFound(_))));

    let dbs = db.catalog_databases(&h.conn_id, false).await.unwrap();
    assert!(dbs.iter().any(|d| d.name == "main"), "{dbs:?}");
    let schemas = db.catalog_schemas(&h.conn_id, "main", false).await.unwrap();
    assert_eq!(schemas.len(), 1);
    let tables = db.catalog_tables(&h.conn_id, "main", "main", false).await.unwrap();
    let names: Vec<&str> = tables.iter().map(|t| t.name.as_str()).collect();
    assert!(names.contains(&"wide") && names.contains(&"nasty"), "{names:?}");
    // Second call is served from the cache (same content), refresh bypasses it.
    assert_eq!(db.catalog_tables(&h.conn_id, "main", "main", false).await.unwrap(), tables);
    assert_eq!(db.catalog_tables(&h.conn_id, "main", "main", true).await.unwrap().len(), tables.len());

    let nasty = TableRef::new(None, "nasty");
    let columns = db.catalog_columns(&h.conn_id, &nasty).await.unwrap();
    let col_names: Vec<&str> = columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(col_names, vec!["id", "text", "num", "note", "flag", "unicode"]);
    let indexes = db.catalog_indexes(&h.conn_id, &nasty).await.unwrap();
    assert!(indexes.iter().any(|i| i.name == "nasty_note"), "{indexes:?}");
    let ddl = db.catalog_ddl(&h.conn_id, &nasty).await.unwrap();
    assert!(ddl.to_ascii_lowercase().contains("create table"), "{ddl}");
    let select = db.catalog_select_sql(&h.conn_id, &nasty).await.unwrap();
    let lower = select.to_ascii_lowercase();
    assert!(lower.starts_with("select") && lower.contains("unicode") && lower.contains("nasty"), "{select}");
    // The generated query lands in the editor and is run as an extraction: it must pass the
    // guard with the connection's own dialect.
    let checked = db.sql_check(Some(&h.conn_id), &select).unwrap();
    assert!(checked.ok, "{checked:?}");

    let plan = db.query_explain(&h.conn_id, "select id, text from nasty where note is not null", false).await.unwrap();
    assert!(plan.root.is_some() || !plan.raw.is_empty());
    assert!(matches!(db.query_explain(&h.conn_id, "   ", false).await, Err(CoreError::Invalid(_))));
    assert!(db.sql_check(None, "select 1").unwrap().ok);
    assert!(!db.sql_check(None, "delete from nasty").unwrap().ok);
    assert!(db.sql_check(Some(&h.conn_id), "select 1").unwrap().ok);

    db.catalog_invalidate(&h.conn_id);
    db.conn_delete(&h.conn_id).unwrap();
    assert!(matches!(db.conn_get(&h.conn_id), Err(CoreError::NotFound(_))));
    assert!(matches!(db.conn_delete(&h.conn_id), Err(CoreError::NotFound(_))));
    assert!(db.conn_list().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn secrets_follow_save_password() {
    let h = harness(10, 0).await;
    let db = &h.db;
    let fixture = h.dir.path().join("fixture.db");

    // "Ask every time": the typed password is never stored.
    let ask = db.conn_save(sqlite_input("ask", &fixture, false, Some("typed"))).unwrap();
    assert!(!ask.has_password);
    assert!(db.creds.get(&credentials::db_password_key(&ask.id)).unwrap().is_none());
    assert!(!db.conn_get(&ask.id).unwrap().fields.contains_key("password"));

    // Saved: the store holds it, the file does not.
    let mut input = sqlite_input("keep", &fixture, true, Some("pw"));
    input.connection.fields.insert("password".into(), "leak".into());
    let keep = db.conn_save(input).unwrap();
    assert!(keep.has_password);
    assert!(!keep.fields.contains_key("password"));
    assert_eq!(db.creds.get(&credentials::db_password_key(&keep.id)).unwrap().as_deref(), Some("pw"));
    let on_disk = std::fs::read_to_string(db.config.connections_path()).unwrap();
    assert!(!on_disk.contains("pw\"") && !on_disk.contains("leak"), "{on_disk}");
    assert!(db.conn_list().iter().find(|c| c.id == keep.id).unwrap().has_password);

    // Flipping to "ask every time" drops the stored secret; deleting removes everything.
    let mut flipped = ConnectionInput { connection: keep.clone(), ..Default::default() };
    flipped.connection.save_password = false;
    assert!(!db.conn_save(flipped).unwrap().has_password);
    db.conn_delete(&keep.id).unwrap();
    assert!(db.creds.get(&credentials::db_password_key(&keep.id)).unwrap().is_none());

    assert!(matches!(
        db.conn_save(ConnectionInput { connection: Connection { name: "x".into(), driver: "db2".into(), ..Default::default() }, ..Default::default() }),
        Err(CoreError::Invalid(_))
    ));
}

/// A server connection behind an SSH tunnel (password auth) with both secrets stored.
fn tunnelled_input(name: &str) -> ConnectionInput {
    let ssh = SshConfig { enabled: true, host: "bastion".into(), port: 22, user: "deploy".into(), auth: SshAuth::Password, key_path: None, compression: true };
    ConnectionInput {
        connection: Connection {
            name: name.into(),
            driver: "postgres".into(),
            host: Some("db".into()),
            user: Some("u".into()),
            ssh: Some(ssh),
            save_password: true,
            ..Default::default()
        },
        password: Some("db-secret".into()),
        ssh_password: Some("ssh-secret".into()),
        ..Default::default()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clear_flags_delete_exactly_one_stored_secret() {
    let h = harness(1, 0).await;
    let db = &h.db;
    let saved = db.conn_save(tunnelled_input("tunnelled")).unwrap();
    assert!(saved.has_password && saved.has_ssh_secret);
    let db_key = credentials::db_password_key(&saved.id);
    let ssh_key = credentials::ssh_password_key(&saved.id);

    // An edit that sends no secret keeps both.
    let kept = db.conn_save(ConnectionInput { connection: saved.clone(), ..Default::default() }).unwrap();
    assert!(kept.has_password && kept.has_ssh_secret);

    // clear_password removes the database password only.
    let cleared = db.conn_save(ConnectionInput { connection: saved.clone(), clear_password: true, ..Default::default() }).unwrap();
    assert!(!cleared.has_password, "{cleared:?}");
    assert!(cleared.has_ssh_secret);
    assert!(db.creds.get(&db_key).unwrap().is_none());
    assert_eq!(db.creds.get(&ssh_key).unwrap().as_deref(), Some("ssh-secret"));
    let listed = db.conn_list().into_iter().find(|c| c.id == saved.id).unwrap();
    assert!(!listed.has_password && listed.has_ssh_secret);

    // clear_ssh_secret removes the SSH secret only (a new password in the same save is fine).
    let swapped = db.conn_save(ConnectionInput { connection: saved.clone(), password: Some("new-db".into()), clear_ssh_secret: true, ..Default::default() }).unwrap();
    assert!(swapped.has_password && !swapped.has_ssh_secret, "{swapped:?}");
    assert_eq!(db.creds.get(&db_key).unwrap().as_deref(), Some("new-db"));
    assert!(db.creds.get(&ssh_key).unwrap().is_none());
    assert!(!db.conn_get(&saved.id).unwrap().has_ssh_secret);

    // Clearing a secret while giving a new one for it is refused, and nothing changes.
    let both = db.conn_save(ConnectionInput { connection: saved.clone(), password: Some("x".into()), clear_password: true, ..Default::default() });
    assert!(matches!(both, Err(CoreError::Invalid(_))), "{both:?}");
    let both_ssh = db.conn_save(ConnectionInput { connection: saved.clone(), ssh_password: Some("y".into()), clear_ssh_secret: true, ..Default::default() });
    assert!(matches!(both_ssh, Err(CoreError::Invalid(_))), "{both_ssh:?}");
    assert_eq!(db.creds.get(&db_key).unwrap().as_deref(), Some("new-db"));
    // An empty secret counts as "none given", so it does not contradict the flag.
    let empty = db.conn_save(ConnectionInput { connection: saved.clone(), password: Some(String::new()), clear_password: true, ..Default::default() }).unwrap();
    assert!(!empty.has_password);

    // A password inside an ODBC connection string is a new password too.
    let odbc = db.conn_save(odbc_input("odbc-clear", "DSN=x", true, Some("pw"))).unwrap();
    let mut input = odbc_input("odbc-clear", "DSN=x;PWD=inline", true, None);
    input.connection.id = odbc.id.clone();
    input.clear_password = true;
    assert!(matches!(db.conn_save(input), Err(CoreError::Invalid(_))));
    assert_eq!(db.creds.get(&credentials::db_password_key(&odbc.id)).unwrap().as_deref(), Some("pw"));
    let on_disk = std::fs::read_to_string(db.config.connections_path()).unwrap();
    assert!(!on_disk.contains("inline"), "{on_disk}");
}

fn odbc_input(name: &str, conn_str: &str, save_password: bool, password: Option<&str>) -> ConnectionInput {
    let mut fields = BTreeMap::new();
    fields.insert("connection_string".to_string(), conn_str.to_string());
    ConnectionInput {
        connection: Connection { name: name.into(), driver: "odbc".into(), fields, save_password, ..Default::default() },
        password: password.map(str::to_string),
        ..Default::default()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn odbc_connection_string_password_never_reaches_the_file_or_ipc() {
    let h = harness(1, 0).await;
    let db = &h.db;

    // Saved password: moved to the credential store, wins over the typed one.
    let saved = db.conn_save(odbc_input("odbc", "Driver={ODBC Driver 18};Server=h,1;UID=u;PWD={s3;cr=t}}x}", true, Some("typed"))).unwrap();
    assert_eq!(saved.fields.get("connection_string").map(String::as_str), Some("Driver={ODBC Driver 18};Server=h,1;UID=u;"));
    assert!(saved.has_password);
    assert_eq!(db.creds.get(&credentials::db_password_key(&saved.id)).unwrap().as_deref(), Some("s3;cr=t}x"));
    let listed = db.conn_list().into_iter().find(|c| c.id == saved.id).unwrap();
    assert!(!listed.fields.values().any(|v| v.contains("s3;cr")), "{listed:?}");
    assert!(!db.conn_get(&saved.id).unwrap().fields.get("connection_string").unwrap().to_ascii_uppercase().contains("PWD"));

    // "Ask every time": nothing stored anywhere on disk.
    let ask = db.conn_save(odbc_input("odbc-ask", "DSN=x;password=hunter2", false, None)).unwrap();
    assert_eq!(ask.fields.get("connection_string").map(String::as_str), Some("DSN=x;"));
    assert!(!ask.has_password);
    assert!(db.creds.get(&credentials::db_password_key(&ask.id)).unwrap().is_none());

    // A string that was only a password disappears; strings without one are kept verbatim.
    let only = db.conn_save(odbc_input("odbc-only", "PWD=x", true, None)).unwrap();
    assert!(!only.fields.contains_key("connection_string"));
    let plain = db.conn_save(odbc_input("odbc-plain", " DSN=y ; Trusted_Connection=yes", true, Some("pw"))).unwrap();
    assert_eq!(plain.fields.get("connection_string").map(String::as_str), Some(" DSN=y ; Trusted_Connection=yes"));
    assert_eq!(db.creds.get(&credentials::db_password_key(&plain.id)).unwrap().as_deref(), Some("pw"));

    let on_disk = std::fs::read_to_string(db.config.connections_path()).unwrap();
    for secret in ["s3;cr", "hunter2", "PWD=x", "typed"] {
        assert!(!on_disk.contains(secret), "{secret} leaked into connections.toml:\n{on_disk}");
    }

    // Round trip through conn_list after a fresh start: nothing secret anywhere it is visible.
    let ids = [saved.id.clone(), ask.id.clone()];
    for c in db.conn_list().iter().filter(|c| ids.contains(&c.id)) {
        let json = serde_json::to_string(c).unwrap();
        assert!(!json.contains("s3;cr") && !json.contains("hunter2") && !json.to_ascii_uppercase().contains("PWD="), "{json}");
    }

    // The config store refuses a direct write of a password-carrying string (only conn_save
    // knows where the secret belongs) and leaves the file alone.
    let mut direct = saved.clone();
    direct.fields.insert("connection_string".into(), "DSN=x;Password={direct-secret}".into());
    let refused = db.config.upsert(direct);
    assert!(matches!(refused, Err(CoreError::Invalid(_))), "{refused:?}");
    assert_eq!(std::fs::read_to_string(db.config.connections_path()).unwrap(), on_disk);
    assert_eq!(db.conn_get(&saved.id).unwrap().fields.get("connection_string").map(String::as_str), Some("Driver={ODBC Driver 18};Server=h,1;UID=u;"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn odbc_secret_attributes_other_than_the_password_are_refused() {
    let h = harness(1, 0).await;
    let db = &h.db;
    let existing = db.conn_save(odbc_input("odbc-existing", "DSN=x", true, Some("kept-pw"))).unwrap();
    let on_disk = std::fs::read_to_string(db.config.connections_path()).unwrap();
    let listed_before = db.conn_list();

    for (cs, key, value) in [
        ("Driver={Databricks};Host=h;UID=token;Auth_AccessToken={dapi-123;x}", "Auth_AccessToken", "dapi-123"),
        ("Driver={ODBC Driver 18};Server=h;KeyStoreSecret=ks-value;PWD=pw-value", "KeyStoreSecret", "ks-value"),
        ("Driver={MySQL ODBC 9.1 Unicode Driver};Server=h;PWD=first;PWD2=second-factor", "PWD2", "second-factor"),
        ("Driver={SnowflakeDSIIDriver};authenticator=oauth;token=oauth-token", "token", "oauth-token"),
        ("Driver={SnowflakeDSIIDriver};priv_key_file=k.p8;priv_key_file_pwd=pk-pass", "priv_key_file_pwd", "pk-pass"),
    ] {
        // New connection and update of an existing one are refused alike, before anything changes.
        let mut update = odbc_input("odbc-existing", cs, true, None);
        update.connection.id = existing.id.clone();
        for input in [odbc_input("odbc-leaky", cs, true, Some("typed")), update] {
            let err = db.conn_save(input).unwrap_err();
            assert_eq!(err.code(), "INVALID", "{cs}");
            let msg = err.to_string();
            assert!(msg.contains(key), "the error names the keyword: {msg}");
            assert!(!msg.contains(value) && !msg.contains("pw-value") && !msg.contains("first"), "the error must not echo a secret: {msg}");
        }
        // A direct write through the config store is refused too.
        let mut direct = existing.clone();
        direct.fields.insert("connection_string".into(), cs.into());
        assert_eq!(db.config.upsert(direct).unwrap_err().code(), "INVALID", "{cs}");
    }
    // Nothing reached the file, the credential store or conn_list.
    assert_eq!(std::fs::read_to_string(db.config.connections_path()).unwrap(), on_disk);
    assert_eq!(db.conn_list(), listed_before);
    assert_eq!(db.creds.get(&credentials::db_password_key(&existing.id)).unwrap().as_deref(), Some("kept-pw"));

    // Look-alike keywords and empty secret attributes are fine: the former stay verbatim, the
    // latter are stripped (nothing is lost).
    let lookalike = db.conn_save(odbc_input("odbc-lookalike", "DSN=x;PWDPolicy=strict;TokenCacheTTL=60", true, None)).unwrap();
    assert_eq!(lookalike.fields.get("connection_string").map(String::as_str), Some("DSN=x;PWDPolicy=strict;TokenCacheTTL=60"));
    let empty = db.conn_save(odbc_input("odbc-empty", "DSN=x;token=;PWD=pw", true, None)).unwrap();
    assert_eq!(empty.fields.get("connection_string").map(String::as_str), Some("DSN=x;"));
    assert_eq!(db.creds.get(&credentials::db_password_key(&empty.id)).unwrap().as_deref(), Some("pw"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn startup_moves_connection_string_passwords_out_of_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let config_dir = dir.path().join("config");
    let open = || async {
        BulkDb::new(BulkDbOptions { config_dir: Some(config_dir.clone()), credentials: Some(Box::new(MemoryStore::default())), init_logging: false, prompts: None })
            .await
            .unwrap()
    };
    // A hand-edited / older file, written straight to disk (the config store itself refuses to
    // write such a string).
    #[derive(serde::Serialize)]
    struct ConnectionsFile {
        connections: Vec<Connection>,
    }
    let legacy = |id: &str, name: &str, conn_str: &str, save_password: bool| {
        let mut fields = BTreeMap::new();
        fields.insert("connection_string".to_string(), conn_str.to_string());
        Connection { id: id.into(), name: name.into(), driver: "odbc".into(), fields, save_password, ..Default::default() }
    };
    let (kept_id, ask_id, token_id) = ("legacy-kept".to_string(), "legacy-ask".to_string(), "legacy-token".to_string());
    std::fs::create_dir_all(&config_dir).unwrap();
    let file = ConnectionsFile {
        connections: vec![
            legacy(&kept_id, "kept", "DSN=a;PWD=old-secret", true),
            legacy(&ask_id, "ask", "DSN=b;Pwd=ask-secret", false),
            legacy(&token_id, "token", "DSN=c;Auth_AccessToken={tok-secret};PWD=c-secret;KeyStoreSecret=ks-secret", true),
        ],
    };
    std::fs::write(config_dir.join("connections.toml"), toml::to_string_pretty(&file).unwrap()).unwrap();
    assert!(std::fs::read_to_string(config_dir.join("connections.toml")).unwrap().contains("old-secret"));
    let db = open().await;
    let on_disk = std::fs::read_to_string(db.config.connections_path()).unwrap();
    for secret in ["old-secret", "ask-secret", "tok-secret", "c-secret", "ks-secret"] {
        assert!(!on_disk.contains(secret), "{secret} is still in connections.toml:\n{on_disk}");
    }
    assert_eq!(db.creds.get(&credentials::db_password_key(&kept_id)).unwrap().as_deref(), Some("old-secret"));
    assert!(db.creds.get(&credentials::db_password_key(&ask_id)).unwrap().is_none(), "ask-every-time secrets stay in memory only");
    assert_eq!(db.conn_get(&kept_id).unwrap().fields.get("connection_string").map(String::as_str), Some("DSN=a;"));
    assert_eq!(db.conn_get(&ask_id).unwrap().fields.get("connection_string").map(String::as_str), Some("DSN=b;"));
    // Secrets other than the password have no home: they are dropped from the file (and so
    // from conn_list); the password still moves to the credential store.
    assert_eq!(db.conn_get(&token_id).unwrap().fields.get("connection_string").map(String::as_str), Some("DSN=c;"));
    assert_eq!(db.creds.get(&credentials::db_password_key(&token_id)).unwrap().as_deref(), Some("c-secret"));
    let listed = serde_json::to_string(&db.conn_list()).unwrap();
    assert!(!listed.contains("tok-secret") && !listed.contains("ks-secret"), "{listed}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn explain_refuses_anything_but_a_single_select() {
    let h = harness(10, 0).await;
    let db = &h.db;
    for analyze in [false, true] {
        for sql in ["delete from wide", "select 1; delete from wide", "drop table wide", "with x as (delete from wide returning *) select * from x"] {
            let r = db.query_explain(&h.conn_id, sql, analyze).await;
            assert!(matches!(r, Err(CoreError::Invalid(_))), "{sql} (analyze={analyze}) gave {r:?}");
        }
    }
    assert!(matches!(db.query_explain(&h.conn_id, "   ", false).await, Err(CoreError::Invalid(_))));
    // A plain SELECT (trailing `;` and a comment after it included) still explains.
    let plan = db.query_explain(&h.conn_id, "select * from wide; -- all of it", false).await.unwrap();
    assert!(plan.root.is_some(), "{plan:?}");

    // Nothing was executed: the table still holds every row.
    let mut rx = db.subscribe();
    let job_id = db.catalog_exact_count(&h.conn_id, &TableRef::new(None, "wide")).await.unwrap();
    let done = wait_done(&mut rx, &job_id).await;
    assert_eq!(done.exact_count.map(|c| c.rows), Some(10), "{:?}", done.error);
}

/// `sql_check` and `query_explain` read the query with the connection's dialect: what is one
/// string literal for PostgreSQL / SQLite is a closed string, `;` and a second statement for
/// MySQL.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sql_guard_follows_the_connection_dialect() {
    let h = harness(1, 0).await;
    let db = &h.db;
    let mysql = db
        .conn_save(ConnectionInput {
            connection: Connection { name: "my".into(), driver: "mysql".into(), host: Some("mysql.invalid".into()), port: Some(1), ..Default::default() },
            ..Default::default()
        })
        .unwrap();
    let odbc = db.conn_save(odbc_input("odbc", "DSN=nowhere", true, None)).unwrap();
    let sneaky = r"select '\'' ; delete from wide -- '";

    assert!(db.sql_check(None, sneaky).unwrap().ok, "standard rules: one string literal");
    assert!(db.sql_check(Some(&h.conn_id), sneaky).unwrap().ok, "sqlite reads it like PostgreSQL");
    let my = db.sql_check(Some(&mysql.id), sneaky).unwrap();
    assert!(!my.ok && my.statements == 2, "{my:?}");
    assert_eq!(my.issues[0].code, bulk_core::sqlguard::CODE_MULTI);
    assert!(!db.sql_check(Some(&odbc.id), sneaky).unwrap().ok, "unknown back ends must pass every reading");
    // `#` ends a MySQL statement's text: a `;` behind it is commented out there only.
    let hash = "select 1 # ; delete from wide";
    assert!(db.sql_check(Some(&mysql.id), hash).unwrap().ok);
    assert!(!db.sql_check(Some(&h.conn_id), hash).unwrap().ok);
    assert!(matches!(db.sql_check(Some("missing"), "select 1"), Err(CoreError::NotFound(_))));

    // EXPLAIN refuses it for MySQL before any connection is attempted (the host does not
    // exist, so reaching the network would be a CONNECT error instead).
    for analyze in [false, true] {
        let r = db.query_explain(&mysql.id, sneaky, analyze).await;
        assert!(matches!(r, Err(CoreError::Invalid(_))), "analyze={analyze}: {r:?}");
    }
    // ...while the same text explains on SQLite, where it is a single SELECT of a string.
    assert!(db.query_explain(&h.conn_id, sneaky, false).await.is_ok());
}

// ---------------------------------------------------------------------------------------------
// Export jobs
// ---------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn preflight_then_export_wide_to_csv() {
    let h = harness(1000, 500).await;
    let db = &h.db;
    db.settings_set(Settings { verify_after_export: true, ..db.settings_get() }).unwrap();

    let req = request(&h.conn_id, "wide", "select * from wide order by id", &h.out, None);
    let report = db.export_preflight(&req).await.unwrap();
    assert!(report.blockers.is_empty(), "{:?}", report.blockers);
    assert!(report.estimates.output_path.ends_with("wide.csv"), "{}", report.estimates.output_path);

    let mut rx = db.subscribe();
    let job_id = db.export_start(req.clone(), warning_codes(&report)).await.unwrap();
    let summary = db.job_get(&job_id).unwrap();
    assert_eq!(summary.kind, JobKind::Export);
    assert_eq!(summary.label, "wide");
    assert!(summary.request.as_ref().unwrap().password.is_none());

    let done = wait_done(&mut rx, &job_id).await;
    assert_eq!(done.state, JobState::Done, "{:?}", done.error);
    let result = done.result.expect("result");
    assert_eq!(result.rows, 1000);
    assert_eq!(result.verified_rows, Some(1000), "verify_after_export re-counts the CSV");
    assert_eq!(result.files.len(), 1);
    let file = PathBuf::from(&result.files[0]);
    assert_eq!(file, h.out.join("wide.csv"));
    assert_eq!(line_count(&file), 1001, "header + 1000 rows");
    assert_eq!(std::fs::metadata(&file).unwrap().len(), result.bytes);
    assert!(result.notes.is_empty(), "{:?}", result.notes);

    let summary = db.job_get(&job_id).unwrap();
    assert_eq!(summary.state, JobState::Done);
    assert!(summary.finished_at.is_some());
    assert_eq!(summary.progress.as_ref().map(|p| p.rows), Some(1000));
    assert!(db.job_list().iter().any(|j| j.id == job_id));
    assert!(db.progress_snapshot().is_empty(), "no active job left");
    let history = db.history_list(Some(&h.conn_id), 10).unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].state, JobState::Done);
    assert_eq!(history[0].rows, Some(1000));
    assert!(matches!(db.job_cancel(&job_id), Err(CoreError::Invalid(_))), "finished jobs cannot be cancelled");
    assert!(matches!(db.job_resume(&job_id, vec![]), Err(CoreError::Invalid(_))));
}

/// An empty result is still a valid file: a header-only CSV and a zero-row Parquet file with
/// the full schema, and `job:done` reports 0 rows.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn empty_result_exports_header_only_csv_and_zero_row_parquet() {
    let h = harness(50, 0).await;
    let db = &h.db;
    let columns: Vec<String> = db.catalog_columns(&h.conn_id, &TableRef::new(None, "wide")).await.unwrap().into_iter().map(|c| c.name).collect();
    assert!(columns.len() > 3, "{columns:?}");
    let sql = "select * from wide where 0";
    let mut rx = db.subscribe();

    // CSV: exactly the header line.
    let req = request(&h.conn_id, "empty", sql, &h.out, None);
    let report = db.export_preflight(&req).await.unwrap();
    assert!(report.blockers.is_empty(), "{:?}", report.blockers);
    let job_id = db.export_start(req, warning_codes(&report)).await.unwrap();
    let done = wait_done(&mut rx, &job_id).await;
    assert_eq!(done.state, JobState::Done, "{:?}", done.error);
    let result = done.result.expect("result");
    assert_eq!(result.rows, 0);
    assert_eq!(result.files, vec![h.out.join("empty.csv").display().to_string()]);
    let text = std::fs::read_to_string(h.out.join("empty.csv")).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines, vec![columns.join(",").as_str()], "header only:\n{text}");
    assert_eq!(std::fs::metadata(h.out.join("empty.csv")).unwrap().len(), result.bytes);

    // Parquet: a readable file with the full schema and no rows.
    let mut req = request(&h.conn_id, "empty_pq", sql, &h.out, None);
    req.output.format = OutputFormat::Parquet;
    req.output.csv = None;
    req.output.parquet = Some(ParquetOpts::default());
    let report = db.export_preflight(&req).await.unwrap();
    assert!(report.blockers.is_empty(), "{:?}", report.blockers);
    let job_id = db.export_start(req, warning_codes(&report)).await.unwrap();
    let done = wait_done(&mut rx, &job_id).await;
    assert_eq!(done.state, JobState::Done, "{:?}", done.error);
    let result = done.result.expect("result");
    assert_eq!(result.rows, 0);
    let path = h.out.join("empty_pq.parquet");
    assert_eq!(result.files, vec![path.display().to_string()]);
    let builder = parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder::try_new(std::fs::File::open(&path).unwrap()).expect("valid parquet file");
    assert_eq!(builder.metadata().file_metadata().num_rows(), 0);
    let names: Vec<String> = builder.schema().fields().iter().map(|f| f.name().clone()).collect();
    assert_eq!(names, columns);
    let batches = builder.build().unwrap();
    let mut rows = 0;
    for batch in batches {
        rows += batch.unwrap().num_rows();
    }
    assert_eq!(rows, 0);
    assert_eq!(files_named(&h.out, |n| n.ends_with(".partial")), Vec::<PathBuf>::new());
    let history = db.history_list(Some(&h.conn_id), 10).unwrap();
    assert!(history.iter().all(|e| e.state == JobState::Done && e.rows == Some(0)), "{history:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn split_by_rows_yields_four_files() {
    let h = harness(1000, 0).await;
    let db = &h.db;
    let req = request(&h.conn_id, "parts", "select * from wide order by id", &h.out, Some(SplitOpts { rows: Some(300), megabytes: None }));
    let report = db.export_preflight(&req).await.unwrap();
    assert!(report.blockers.is_empty(), "{:?}", report.blockers);

    let mut rx = db.subscribe();
    let job_id = db.export_start(req, warning_codes(&report)).await.unwrap();
    let done = wait_done(&mut rx, &job_id).await;
    assert_eq!(done.state, JobState::Done, "{:?}", done.error);
    let result = done.result.unwrap();
    assert_eq!(result.rows, 1000);
    assert_eq!(result.files.len(), 4, "{:?}", result.files);
    let on_disk = files_named(&h.out, |n| n.starts_with("parts_part") && n.ends_with(".csv"));
    assert_eq!(on_disk.len(), 4, "{on_disk:?}");
    let lines: Vec<usize> = on_disk.iter().map(|p| line_count(p)).collect();
    assert_eq!(lines, vec![301, 301, 301, 101], "each part repeats the header");
    assert_eq!(on_disk.iter().map(|p| std::fs::metadata(p).unwrap().len()).sum::<u64>(), result.bytes);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn warned_job_waits_for_acknowledgement() {
    let h = harness(100, 0).await;
    let db = &h.db;
    // An existing output file with `overwrite = false` is a deterministic preflight warning.
    std::fs::write(h.out.join("warned.csv"), "stale\n").unwrap();
    let req = request(&h.conn_id, "warned", "select id, title from wide", &h.out, None);
    let report = db.export_preflight(&req).await.unwrap();
    assert!(report.blockers.is_empty(), "{:?}", report.blockers);
    assert!(warning_codes(&report).iter().any(|c| c == "OVERWRITE"), "{:?}", report.warnings);

    let mut rx = db.subscribe();
    let job_id = db.export_start(req, vec![]).await.unwrap();
    wait_state(&mut rx, &job_id, JobState::Warned).await;
    let summary = db.job_get(&job_id).unwrap();
    assert_eq!(summary.state, JobState::Warned);
    let pending = summary.preflight.expect("warned jobs expose their preflight report");
    assert_eq!(warning_codes(&pending), warning_codes(&report));
    assert!(db.progress_snapshot().iter().any(|p| p.job_id == job_id), "warned jobs are still active");

    // Partial acknowledgement is refused and the job keeps waiting.
    assert!(matches!(db.job_resume(&job_id, vec![]), Err(CoreError::Invalid(_))));
    assert_eq!(db.job_get(&job_id).unwrap().state, JobState::Warned);
    db.job_resume(&job_id, warning_codes(&pending)).unwrap();
    let done = wait_done(&mut rx, &job_id).await;
    assert_eq!(done.state, JobState::Done, "{:?}", done.error);
    assert_eq!(done.result.unwrap().rows, 100);
    assert_eq!(line_count(&h.out.join("warned.csv")), 101, "the acknowledged overwrite replaced the stale file");

    // A second warned job can be cancelled while it waits; the existing file is untouched.
    std::fs::write(h.out.join("warned2.csv"), "stale\n").unwrap();
    let req = request(&h.conn_id, "warned2", "select id from wide", &h.out, None);
    let job_id = db.export_start(req, vec![]).await.unwrap();
    wait_state(&mut rx, &job_id, JobState::Warned).await;
    db.job_cancel(&job_id).unwrap();
    let done = wait_done(&mut rx, &job_id).await;
    assert_eq!(done.state, JobState::Cancelled);
    assert_eq!(done.error.as_ref().map(|e| e.code.as_str()), Some("CANCELLED"));
    assert_eq!(std::fs::read_to_string(h.out.join("warned2.csv")).unwrap(), "stale\n", "nothing was written");
    assert!(db.history_list(Some(&h.conn_id), 10).unwrap().iter().all(|e| e.state != JobState::Cancelled), "a job cancelled before running leaves no history row");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_mid_stream_leaves_a_partial_file() {
    let h = harness(10, 0).await;
    let db = &h.db;
    let sql = "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c WHERE x < 20000000) SELECT x, x * 2 AS y, 'row ' || x AS label FROM c";
    let req = request(&h.conn_id, "slow", sql, &h.out, None);
    let report = db.export_preflight(&req).await.unwrap();
    assert!(report.blockers.is_empty(), "{:?}", report.blockers);

    let mut rx = db.subscribe();
    let job_id = db.export_start(req, warning_codes(&report)).await.unwrap();
    wait_state(&mut rx, &job_id, JobState::Running).await;

    // Let at least one batch reach the sink, then cancel.
    let t0 = Instant::now();
    loop {
        let rows = db.progress_snapshot().iter().find(|p| p.job_id == job_id).map(|p| p.rows).unwrap_or(0);
        if rows > 0 {
            break;
        }
        assert!(t0.elapsed() < Duration::from_secs(60), "no rows flowed within 60 s");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    db.job_cancel(&job_id).unwrap();
    let done = wait_done(&mut rx, &job_id).await;
    assert_eq!(done.state, JobState::Cancelled);
    assert_eq!(done.error.as_ref().map(|e| e.code.as_str()), Some("CANCELLED"));
    assert!(done.result.is_none());
    assert!(t0.elapsed() < Duration::from_secs(60), "cancellation took {:?}", t0.elapsed());

    let partial = files_named(&h.out, |n| n.ends_with(".partial"));
    assert_eq!(partial.len(), 1, "{partial:?}");
    assert!(partial[0].file_name().unwrap().to_string_lossy().starts_with("slow.csv"));
    assert!(!h.out.join("slow.csv").exists(), "the incomplete file must not look finished");
    assert_eq!(db.job_get(&job_id).unwrap().state, JobState::Cancelled);
    let history = db.history_list(Some(&h.conn_id), 10).unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].state, JobState::Cancelled);
    // The connection is usable again after the interrupted query.
    assert!(db.conn_test(&h.conn_id, None).await.is_ok());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_waits_for_running_jobs_to_leave_partial_files() {
    let h = harness(10, 0).await;
    let db = &h.db;
    let sql = "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c WHERE x < 20000000) SELECT x, x * 2 AS y, 'row ' || x AS label FROM c";
    let req = request(&h.conn_id, "closing", sql, &h.out, None);
    let report = db.export_preflight(&req).await.unwrap();
    let mut rx = db.subscribe();
    let job_id = db.export_start(req, warning_codes(&report)).await.unwrap();
    wait_state(&mut rx, &job_id, JobState::Running).await;
    let t0 = Instant::now();
    while db.progress_snapshot().iter().find(|p| p.job_id == job_id).map(|p| p.rows).unwrap_or(0) == 0 {
        assert!(t0.elapsed() < Duration::from_secs(60), "no rows flowed within 60 s");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // The shell exits the process as soon as shutdown returns: by then the job must be over
    // and its file renamed, with the history row written.
    db.shutdown().await;
    assert_eq!(db.job_get(&job_id).unwrap().state, JobState::Cancelled);
    assert!(db.jobs.active().is_empty());
    assert!(!h.out.join("closing.csv").exists(), "a truncated file must not keep its final name");
    assert_eq!(files_named(&h.out, |n| n.starts_with("closing.csv") && n.ends_with(".partial")).len(), 1);
    let history = db.history_list(Some(&h.conn_id), 10).unwrap();
    assert_eq!(history.first().map(|e| e.state), Some(JobState::Cancelled));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn export_start_rejects_bad_requests_early() {
    let h = harness(10, 0).await;
    let db = &h.db;
    let mut req = request("missing", "x", "select 1", &h.out, None);
    assert!(matches!(db.export_start(req.clone(), vec![]).await, Err(CoreError::NotFound(_))));
    req.conn_id = h.conn_id.clone();
    req.sql_file = Some("also.sql".into());
    assert!(matches!(db.export_start(req.clone(), vec![]).await, Err(CoreError::Invalid(_))));
    req.sql_file = None;
    req.sql = None;
    assert!(matches!(db.export_preflight(&req).await, Err(CoreError::Invalid(_))));
    req.sql = Some("select 1".into());
    req.output.dir = String::new();
    assert!(matches!(db.export_start(req.clone(), vec![]).await, Err(CoreError::Invalid(_))), "no output dir anywhere");
    assert!(db.job_list().is_empty(), "nothing was created");

    // DML never reaches the server: preflight blocks it, the job is rejected.
    let req = request(&h.conn_id, "dml", "delete from wide", &h.out, None);
    let report = db.export_preflight(&req).await.unwrap();
    assert!(!report.blockers.is_empty());
    let mut rx = db.subscribe();
    let job_id = db.export_start(req, vec![]).await.unwrap();
    let done = wait_done(&mut rx, &job_id).await;
    assert_eq!(done.state, JobState::Rejected);
    // The error code is a documented CoreError code; the state says "rejected by preflight".
    assert_eq!(done.error.as_ref().map(|e| e.code.as_str()), Some("INVALID"));
    assert!(done.error.as_ref().and_then(|e| e.detail.as_deref()).is_some_and(|d| d.contains("SQL_NOT_SELECT")), "{:?}", done.error);
    assert_eq!(db.job_get(&job_id).unwrap().state, JobState::Rejected);
    assert!(matches!(db.job_cancel("nope"), Err(CoreError::NotFound(_))));
}

// ---------------------------------------------------------------------------------------------
// Exact count, diagnostics, import
// ---------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exact_count_job_counts_nasty() {
    let h = harness(10, 500).await;
    let db = &h.db;
    let mut rx = db.subscribe();
    let table = TableRef::new(None, "nasty");
    let job_id = db.catalog_exact_count(&h.conn_id, &table).await.unwrap();
    assert_eq!(db.job_get(&job_id).unwrap().kind, JobKind::ExactCount);
    let done = wait_done(&mut rx, &job_id).await;
    assert_eq!(done.state, JobState::Done, "{:?}", done.error);
    let count = done.exact_count.expect("job:done carries the exact count");
    assert_eq!(count.rows, 500);
    assert_eq!(count.table, table);
    assert_eq!(db.job_get(&job_id).unwrap().exact_count.map(|c| c.rows), Some(500));
    assert_eq!(db.history.last_exact_count(&h.conn_id, &bulk_core::history::table_key(&table)).unwrap().map(|c| c.rows), Some(500));
    // The facade reads the stored count back (what the UI shows next to the estimate).
    let stored = db.catalog_last_exact_count(&h.conn_id, &table).unwrap().expect("count persisted");
    assert_eq!(stored, count, "the stored count is the one job:done carried");
    assert_eq!(db.catalog_last_exact_count(&h.conn_id, &TableRef::new(None, "wide")).unwrap(), None, "never counted");
    assert!(matches!(db.catalog_last_exact_count("no-such-conn", &table), Err(CoreError::NotFound(_))));

    assert!(matches!(db.catalog_exact_count(&h.conn_id, &TableRef::new(None, "")).await, Err(CoreError::Invalid(_))));
    let job_id = db.catalog_exact_count(&h.conn_id, &TableRef::new(None, "no_such_table")).await.unwrap();
    let done = wait_done(&mut rx, &job_id).await;
    assert_eq!(done.state, JobState::Failed);
    assert!(done.exact_count.is_none());
    assert_eq!(db.catalog_last_exact_count(&h.conn_id, &TableRef::new(None, "no_such_table")).unwrap(), None, "a failed count stores nothing");
    assert_eq!(db.catalog_last_exact_count(&h.conn_id, &table).unwrap().map(|c| c.rows), Some(500), "earlier counts survive a failed one");
}

/// The stored count survives a restart of the facade (it lives in the history database).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exact_count_survives_restart() {
    let h = harness(10, 20).await;
    let table = TableRef::new(None, "nasty");
    let mut rx = h.db.subscribe();
    let job_id = h.db.catalog_exact_count(&h.conn_id, &table).await.unwrap();
    assert_eq!(wait_done(&mut rx, &job_id).await.state, JobState::Done);
    let Harness { dir, db, conn_id, .. } = h;
    drop(db);
    let db = reopen(dir.path()).await;
    assert_eq!(db.catalog_last_exact_count(&conn_id, &table).unwrap().map(|c| c.rows), Some(20));
}

/// A second facade over the same config dir as [`harness`] (a restart of the app).
async fn reopen(dir: &Path) -> Arc<BulkDb> {
    BulkDb::new(BulkDbOptions { config_dir: Some(dir.join("config")), credentials: Some(Box::new(MemoryStore::default())), init_logging: false, prompts: None })
        .await
        .unwrap()
}

/// Record `n` finished dummy runs for `conn_id` directly in the history database.
fn seed_history(db: &BulkDb, conn_id: &str, prefix: &str, n: usize) {
    let session = SessionOpts::default();
    let output = OutputOpts::default();
    for i in 0..n {
        let job_id = format!("{prefix}-{i}");
        db.history
            .record_start(bulk_core::history::HistoryStart {
                job_id: &job_id,
                conn_id,
                label: "seed",
                sql: "select 1",
                sql_file: None,
                session: &session,
                output: &output,
            })
            .unwrap();
        db.history.record_finish(&job_id, JobState::Done, None, None).unwrap();
    }
}

/// The runs table (full SQL of every job) is bounded: pruned after every finished export and
/// again when the facade starts, keeping the newest runs per connection.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn history_is_pruned_after_exports_and_at_startup() {
    use bulk_core::service::HISTORY_KEEP_PER_CONNECTION as KEEP;
    let h = harness(10, 0).await;
    seed_history(&h.db, &h.conn_id, "old", KEEP + 5);
    seed_history(&h.db, "other-conn", "other", 3);
    assert_eq!(h.db.history.count(Some(&h.conn_id)).unwrap(), (KEEP + 5) as u64);

    let req = request(&h.conn_id, "wide", "select * from wide", &h.out, None);
    let report = h.db.export_preflight(&req).await.unwrap();
    let mut rx = h.db.subscribe();
    let job_id = h.db.export_start(req, warning_codes(&report)).await.unwrap();
    let done = wait_done(&mut rx, &job_id).await;
    assert_eq!(done.state, JobState::Done, "{:?}", done.error);
    assert_eq!(h.db.history.count(Some(&h.conn_id)).unwrap(), KEEP as u64, "pruned after the export finished");
    let newest = h.db.history_list(Some(&h.conn_id), 1).unwrap();
    assert_eq!(newest[0].job_id, job_id, "the run just finished is kept");
    assert_eq!(newest[0].state, JobState::Done);
    assert_eq!(h.db.history.count(Some("other-conn")).unwrap(), 3, "other connections keep their own runs");

    // Runs written behind the facade's back (an older version, a crash before the prune)
    // are trimmed on the next start.
    seed_history(&h.db, &h.conn_id, "late", 7);
    assert_eq!(h.db.history.count(Some(&h.conn_id)).unwrap(), (KEEP + 7) as u64);
    let Harness { dir, db, conn_id, .. } = h;
    drop(db);
    let db = reopen(dir.path()).await;
    assert_eq!(db.history.count(Some(&conn_id)).unwrap(), KEEP as u64, "pruned at startup");
    assert_eq!(db.history_list(Some(&conn_id), 1).unwrap()[0].job_id, "late-6", "newest runs survive");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn diagnostics_bundle_and_dbeaver_import() {
    let h = harness(10, 0).await;
    let db = &h.db;
    let bundle = db.diagnostics_bundle(&h.out.display().to_string()).unwrap();
    let path = Path::new(&bundle.path);
    assert!(path.exists() && path.extension().unwrap() == "zip");
    assert_eq!(std::fs::metadata(path).unwrap().len(), bundle.size_bytes);
    for name in ["settings.toml", "connections.toml", "drivers.json", "system.txt"] {
        assert!(bundle.included.iter().any(|n| n == name), "{name} missing from {:?}", bundle.included);
    }
    let mut zip = zip::ZipArchive::new(std::fs::File::open(path).unwrap()).unwrap();
    let mut system = String::new();
    std::io::Read::read_to_string(&mut zip.by_name("system.txt").unwrap(), &mut system).unwrap();
    assert!(system.contains(env!("CARGO_PKG_VERSION")) && system.contains("config dir"), "{system}");
    assert!(matches!(db.diagnostics_bundle("  "), Err(CoreError::Invalid(_))));

    let fixture = h.dir.path().join("fixture.db");
    let json = format!(
        r#"{{ "connections": {{
            "postgres-jdbc-1": {{ "provider": "postgresql", "driver": "postgres-jdbc", "name": "Analytics",
              "configuration": {{ "host": "db.example.com", "port": "5432", "database": "analytics", "user": "reader", "password": "secret" }} }},
            "sqlite-2": {{ "provider": "sqlite", "driver": "sqlite", "name": "imported fixture",
              "configuration": {{ "database": {path:?} }} }},
            "mongo-3": {{ "provider": "mongodb", "driver": "mongo", "name": "docs", "configuration": {{ "host": "m" }} }}
        }} }}"#,
        path = fixture.display().to_string()
    );
    let json_path = h.dir.path().join("data-sources.json");
    std::fs::write(&json_path, json).unwrap();
    let rows = db.conn_import_dbeaver(&json_path.display().to_string()).unwrap();
    assert_eq!(rows.len(), 3);
    let pg = rows.iter().find(|r| r.connection.name == "Analytics").unwrap();
    assert_eq!(pg.connection.driver, "postgres");
    assert!(!pg.connection.fields.contains_key("password"));
    assert!(pg.notes.iter().any(|n| n.contains("password")));
    assert!(rows.iter().find(|r| r.connection.name == "docs").unwrap().notes.iter().any(|n| n.contains("unsupported provider")));

    // An imported sqlite row is a complete, working connection.
    let imported = rows.iter().find(|r| r.connection.name == "imported fixture").unwrap().connection.clone();
    let saved = db.conn_save(ConnectionInput { connection: imported, ..Default::default() }).unwrap();
    assert!(db.conn_test(&saved.id, None).await.is_ok());
    assert_eq!(db.conn_list().len(), 2);
    assert!(matches!(db.conn_import_dbeaver("/no/such/file.json"), Err(CoreError::Io(_))));
    db.shutdown().await;
}
