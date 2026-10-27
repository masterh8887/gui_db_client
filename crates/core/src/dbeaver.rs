//! DBeaver `data-sources.json` import.
//!
//! DBeaver keeps its connections in `<workspace>/.metadata/.plugins/org.jkiss.dbeaver.core/
//! data-sources.json` (and project-level `.dbeaver/data-sources.json`). The shape we read:
//!
//! ```json
//! {
//!   "connections": {
//!     "postgres-jdbc-18c0d1f4a2b-1a2b3c4d": {
//!       "provider": "postgresql",
//!       "driver": "postgres-jdbc",
//!       "name": "Analytics (prod)",
//!       "save-password": true,
//!       "configuration": {
//!         "host": "db.example.com", "port": "5432", "database": "analytics", "user": "reader",
//!         "url": "jdbc:postgresql://db.example.com:5432/analytics",
//!         "handlers": { "ssh_tunnel": { "type": "tunnel", "enabled": true,
//!                       "properties": { "host": "bastion", "port": "22", "userName": "ec2-user",
//!                                       "authType": "PUBLIC_KEY", "keyPath": "~/.ssh/id_ed25519" } } }
//!       }
//!     }
//!   }
//! }
//! ```
//!
//! Passwords are never imported: DBeaver stores them encrypted in a separate file and BulkDB
//! keeps secrets only in the OS credential store, so the user types them once after import.
//! Unknown providers still produce a row (so the user sees what was skipped) with a note.

use std::collections::BTreeMap;
use std::path::Path;

use serde_json::Value;

use crate::error::{CoreError, Result};
use crate::types::{Connection, ImportedConnection, SessionOpts, SshAuth, SshConfig};

/// Read and parse a `data-sources.json` file.
pub fn import_file(path: &Path) -> Result<Vec<ImportedConnection>> {
    let text = std::fs::read_to_string(path).map_err(|e| CoreError::Io(format!("{}: {e}", path.display())))?;
    parse(&text)
}

/// Parse the JSON text of a `data-sources.json` file. Rows are sorted by name.
pub fn parse(json: &str) -> Result<Vec<ImportedConnection>> {
    let root: Value = serde_json::from_str(json.trim_start_matches('\u{feff}')).map_err(|e| CoreError::Invalid(format!("not a DBeaver data-sources.json: {e}")))?;
    let conns = root.get("connections").and_then(Value::as_object).ok_or_else(|| CoreError::Invalid("no \"connections\" object found in data-sources.json".into()))?;
    let folders = root.get("folders").and_then(Value::as_object);
    let mut out: Vec<ImportedConnection> = conns.iter().map(|(id, v)| convert(id, v, folders)).collect();
    out.sort_by_key(|r| r.connection.name.to_lowercase());
    Ok(out)
}

/// Map a DBeaver provider id onto a BulkDB driver id. `None` for providers BulkDB cannot open.
pub fn map_provider(provider: &str) -> Option<&'static str> {
    match provider.trim().to_ascii_lowercase().as_str() {
        "postgresql" | "postgres" | "pgsql" => Some("postgres"),
        "mysql" | "mariadb" | "mysql8" => Some("mysql"),
        "sqlserver" | "mssql" | "ms_sql" => Some("mssql"),
        "sqlite" => Some("sqlite"),
        "oracle" => Some("oracle"),
        "odbc" | "generic" => Some("odbc"),
        _ => None,
    }
}

/// Host / port / database pieces of a JDBC URL.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct JdbcParts {
    pub scheme: String,
    pub host: Option<String>,
    pub port: Option<u16>,
    pub database: Option<String>,
    /// SQL Server `instanceName=` / `host\instance`.
    pub instance: Option<String>,
    /// SQLite file path.
    pub path: Option<String>,
    /// Oracle: `true` when the URL names a SID rather than a service.
    pub oracle_sid: bool,
}

/// Best-effort JDBC URL parser for the providers BulkDB knows. Never fails: unknown shapes
/// yield an empty `JdbcParts` with only the scheme set.
pub fn parse_jdbc_url(url: &str) -> JdbcParts {
    let url = url.trim();
    let mut parts = JdbcParts::default();
    let Some(rest) = url.strip_prefix("jdbc:") else { return parts };
    let (scheme, rest) = match rest.find(':') {
        Some(i) => (&rest[..i], &rest[i + 1..]),
        None => (rest, ""),
    };
    parts.scheme = scheme.to_ascii_lowercase();
    match parts.scheme.as_str() {
        "sqlite" => {
            let p = rest.split('?').next().unwrap_or("").trim();
            if !p.is_empty() && p != ":memory:" {
                parts.path = Some(p.to_string());
            }
        }
        "oracle" => {
            // jdbc:oracle:thin:@//host:port/service | jdbc:oracle:thin:@host:port:sid | @host:port/service
            let after_at = rest.find('@').map(|i| &rest[i + 1..]).unwrap_or("");
            let after_at = after_at.trim_start_matches("//");
            if let Some((hp, svc)) = after_at.split_once('/') {
                set_host_port(&mut parts, hp);
                parts.database = non_empty(svc.split('?').next().unwrap_or(""));
            } else {
                let segs: Vec<&str> = after_at.split(':').collect();
                if segs.len() >= 3 {
                    parts.host = non_empty(segs[0]);
                    parts.port = segs[1].parse().ok();
                    parts.database = non_empty(segs[2]);
                    parts.oracle_sid = true;
                } else {
                    set_host_port(&mut parts, after_at);
                }
            }
        }
        "sqlserver" => {
            // jdbc:sqlserver://host\instance:port;databaseName=db;encrypt=true
            let body = rest.trim_start_matches("//");
            let mut segs = body.split(';');
            let hp = segs.next().unwrap_or("");
            let (hp, inst) = match hp.split_once('\\') {
                Some((h, rest)) => {
                    let (inst, port) = match rest.split_once(':') {
                        Some((i, p)) => (i, Some(p)),
                        None => (rest, None),
                    };
                    (
                        match port {
                            Some(p) => format!("{h}:{p}"),
                            None => h.to_string(),
                        },
                        non_empty(inst),
                    )
                }
                None => (hp.to_string(), None),
            };
            set_host_port(&mut parts, &hp);
            parts.instance = inst;
            for seg in segs {
                if let Some((k, v)) = seg.split_once('=') {
                    match k.trim().to_ascii_lowercase().as_str() {
                        "databasename" | "database" => parts.database = non_empty(v),
                        "instancename" => parts.instance = non_empty(v),
                        _ => {}
                    }
                }
            }
        }
        _ => {
            // jdbc:postgresql://host:port/db?x=y  |  jdbc:mysql://host:port/db
            let body = rest.trim_start_matches("//");
            let body = body.split('?').next().unwrap_or("");
            let (hp, db) = match body.split_once('/') {
                Some((hp, db)) => (hp, non_empty(db)),
                None => (body, None),
            };
            set_host_port(&mut parts, hp);
            parts.database = db;
        }
    }
    parts
}

fn set_host_port(parts: &mut JdbcParts, hp: &str) {
    let hp = hp.trim();
    if hp.is_empty() {
        return;
    }
    // Only the first host of a comma separated list is used; IPv6 literals keep their brackets.
    let hp = hp.split(',').next().unwrap_or(hp);
    if let Some(end) = hp.strip_prefix('[').and_then(|s| s.find(']')) {
        parts.host = Some(hp[1..=end].to_string());
        parts.port = hp[end + 2..].split(':').next_back().and_then(|p| p.parse().ok());
        return;
    }
    match hp.rsplit_once(':') {
        Some((h, p)) => {
            parts.host = non_empty(h);
            parts.port = p.trim().parse().ok();
        }
        None => parts.host = Some(hp.to_string()),
    }
}

fn non_empty(s: &str) -> Option<String> {
    let s = s.trim();
    if s.is_empty() {
        None
    } else {
        Some(s.to_string())
    }
}

fn str_field(obj: Option<&serde_json::Map<String, Value>>, key: &str) -> Option<String> {
    let v = obj?.get(key)?;
    match v {
        Value::String(s) => non_empty(s),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

fn port_field(obj: Option<&serde_json::Map<String, Value>>, key: &str) -> Option<u16> {
    str_field(obj, key).and_then(|s| s.trim().parse().ok())
}

/// Stable 12-hex-char id derived from the DBeaver id, so re-importing updates instead of
/// duplicating (FNV-1a 64, truncated).
pub fn stable_id(dbeaver_id: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in dbeaver_id.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{h:016x}")[..12].to_string()
}

fn convert(id: &str, v: &Value, folders: Option<&serde_json::Map<String, Value>>) -> ImportedConnection {
    let mut notes = Vec::new();
    let obj = v.as_object();
    let name = str_field(obj, "name").unwrap_or_else(|| id.to_string());
    let provider = str_field(obj, "provider").unwrap_or_default();
    let dbeaver_driver = str_field(obj, "driver").unwrap_or_default();
    let cfg = obj.and_then(|o| o.get("configuration")).and_then(Value::as_object);
    let url = str_field(cfg, "url").map(|u| parse_jdbc_url(&u)).unwrap_or_default();

    let mut conn = Connection {
        id: stable_id(id),
        name,
        driver: String::new(),
        host: str_field(cfg, "host").or(url.host.clone()),
        port: port_field(cfg, "port").or(url.port),
        database: str_field(cfg, "database").or(url.database.clone()),
        user: str_field(cfg, "user"),
        fields: BTreeMap::new(),
        ssh: None,
        session: SessionOpts::default(),
        output_dir: None,
        save_password: true,
        has_password: false,
        has_ssh_secret: false,
    };

    match map_provider(&provider) {
        Some("sqlite") => {
            conn.driver = "sqlite".into();
            let path = str_field(cfg, "database").or(url.path.clone());
            match path {
                Some(p) => {
                    conn.fields.insert("path".into(), p);
                }
                None => notes.push("sqlite file path not found in the DBeaver configuration".into()),
            }
            conn.host = None;
            conn.port = None;
            conn.database = None;
        }
        Some("oracle") => {
            conn.driver = "oracle".into();
            let sid_mode = obj
                .and_then(|o| o.get("configuration"))
                .and_then(|c| c.get("provider-properties"))
                .and_then(|p| p.get("@dbeaver-sid-service@"))
                .and_then(Value::as_str)
                .map(|s| s.eq_ignore_ascii_case("SID"))
                .unwrap_or(url.oracle_sid);
            if let Some(db) = conn.database.take() {
                conn.fields.insert("service_name".into(), db);
            }
            if sid_mode {
                notes.push("DBeaver connects by SID; BulkDB uses a service name (check `service_name`)".into());
            }
        }
        Some("mssql") => {
            conn.driver = "mssql".into();
            if let Some(inst) = url.instance.clone() {
                conn.fields.insert("instance".into(), inst);
            }
            if let Some(h) = conn.host.clone() {
                if let Some((h, inst)) = h.split_once('\\') {
                    conn.host = Some(h.to_string());
                    conn.fields.entry("instance".into()).or_insert_with(|| inst.to_string());
                }
            }
        }
        Some("odbc") => {
            conn.driver = "odbc".into();
            notes.push("ODBC connections need the driver name and connection string entered by hand".into());
        }
        Some(d) => conn.driver = d.into(),
        None => {
            conn.driver = provider.clone();
            notes.push(format!("unsupported provider '{provider}' (DBeaver driver '{dbeaver_driver}')"));
        }
    }

    // SSL hints (DBeaver keeps them as provider properties or an "ssl" handler).
    if let Some(handlers) = cfg.and_then(|c| c.get("handlers")).and_then(Value::as_object) {
        if handlers.get("ssl").and_then(|h| h.get("enabled")).and_then(Value::as_bool).unwrap_or(false) {
            match conn.driver.as_str() {
                "postgres" | "mysql" => {
                    conn.fields.insert("sslmode".into(), "require".into());
                }
                "mssql" => {
                    conn.fields.insert("encrypt".into(), "true".into());
                }
                _ => {}
            }
        }
        if let Some(ssh) = handlers.get("ssh_tunnel").and_then(Value::as_object) {
            let enabled = ssh.get("enabled").and_then(Value::as_bool).unwrap_or(false);
            let props = ssh.get("properties").and_then(Value::as_object);
            let host = str_field(props, "host");
            match (enabled, host) {
                (true, Some(host)) => {
                    let auth = match str_field(props, "authType").unwrap_or_default().to_ascii_uppercase().as_str() {
                        "PUBLIC_KEY" => SshAuth::Key,
                        "AGENT" => SshAuth::Agent,
                        _ => SshAuth::Password,
                    };
                    let key_path = str_field(props, "keyPath");
                    if auth == SshAuth::Key {
                        match &key_path {
                            Some(p) if p.to_ascii_lowercase().ends_with(".ppk") => notes.push("ssh key is a PuTTY .ppk file; convert it to OpenSSH format".into()),
                            Some(_) => {}
                            None => notes.push("ssh key path missing in the DBeaver tunnel settings".into()),
                        }
                    }
                    conn.ssh = Some(SshConfig {
                        enabled: true,
                        host,
                        port: port_field(props, "port").unwrap_or(22),
                        user: str_field(props, "userName").or_else(|| str_field(props, "user")).unwrap_or_default(),
                        auth,
                        key_path,
                        compression: true,
                    });
                    if auth != SshAuth::Agent {
                        notes.push("ssh password / passphrase not imported".into());
                    }
                }
                (true, None) => notes.push("ssh tunnel enabled in DBeaver but no bastion host was found".into()),
                _ => {}
            }
        }
    }

    // Passwords never travel: say so when DBeaver had one.
    let had_password = cfg.map(|c| c.contains_key("password")).unwrap_or(false) || obj.and_then(|o| o.get("save-password")).and_then(Value::as_bool).unwrap_or(false);
    if had_password && conn.user.is_some() {
        notes.push("database password not imported; enter it once in BulkDB".into());
    }

    if let Some(folder) = str_field(obj, "folder") {
        let label = folders.and_then(|f| f.get(&folder)).and_then(|f| f.get("name")).and_then(Value::as_str).unwrap_or(folder.as_str());
        notes.push(format!("DBeaver folder: {label}"));
    }

    ImportedConnection { connection: conn, notes }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
      "folders": { "prod": { "name": "Production" } },
      "connections": {
        "postgres-jdbc-1": {
          "provider": "postgresql", "driver": "postgres-jdbc", "name": "Analytics",
          "save-password": true, "folder": "prod",
          "configuration": { "host": "db.example.com", "port": "5432", "database": "analytics", "user": "reader",
            "url": "jdbc:postgresql://db.example.com:5432/analytics", "password": "nope",
            "handlers": { "ssh_tunnel": { "type": "tunnel", "enabled": true,
              "properties": { "host": "bastion", "port": "2222", "userName": "ec2-user", "authType": "PUBLIC_KEY", "keyPath": "/home/me/.ssh/id_ed25519" } } } }
        },
        "mysql-2": { "provider": "mariadb", "driver": "mariadb", "name": "shop",
          "configuration": { "url": "jdbc:mysql://shop-db:3307/shop?useSSL=false", "user": "root" } },
        "sqlite-3": { "provider": "sqlite", "driver": "sqlite", "name": "local",
          "configuration": { "url": "jdbc:sqlite:C:\\data\\test.db" } },
        "mssql-4": { "provider": "sqlserver", "driver": "mssql", "name": "erp",
          "configuration": { "url": "jdbc:sqlserver://erp\\SQL2019:1433;databaseName=ERP;encrypt=true", "user": "sa" } },
        "oracle-5": { "provider": "oracle", "driver": "oracle_thin", "name": "legacy",
          "configuration": { "url": "jdbc:oracle:thin:@legacy-db:1521:ORCL", "user": "system",
            "provider-properties": { "@dbeaver-sid-service@": "SID" } } },
        "mongo-6": { "provider": "mongodb", "driver": "mongo", "name": "docs", "configuration": { "host": "m" } }
      }
    }"#;

    #[test]
    fn imports_every_provider() {
        let rows = parse(SAMPLE).unwrap();
        assert_eq!(rows.len(), 6);
        let by_name = |n: &str| rows.iter().find(|r| r.connection.name == n).unwrap();

        let pg = by_name("Analytics");
        assert_eq!(pg.connection.driver, "postgres");
        assert_eq!(pg.connection.host.as_deref(), Some("db.example.com"));
        assert_eq!(pg.connection.port, Some(5432));
        assert_eq!(pg.connection.database.as_deref(), Some("analytics"));
        assert_eq!(pg.connection.user.as_deref(), Some("reader"));
        assert!(!pg.connection.fields.contains_key("password"));
        let ssh = pg.connection.ssh.as_ref().unwrap();
        assert_eq!(ssh.host, "bastion");
        assert_eq!(ssh.port, 2222);
        assert_eq!(ssh.auth, SshAuth::Key);
        assert!(pg.notes.iter().any(|n| n.contains("password not imported")));
        assert!(pg.notes.iter().any(|n| n.contains("Production")));
        assert_eq!(pg.connection.id, stable_id("postgres-jdbc-1"));

        let my = by_name("shop");
        assert_eq!(my.connection.driver, "mysql");
        assert_eq!(my.connection.host.as_deref(), Some("shop-db"));
        assert_eq!(my.connection.port, Some(3307));
        assert_eq!(my.connection.database.as_deref(), Some("shop"));

        let lite = by_name("local");
        assert_eq!(lite.connection.driver, "sqlite");
        assert_eq!(lite.connection.fields.get("path").map(String::as_str), Some("C:\\data\\test.db"));
        assert!(lite.connection.host.is_none());

        let ms = by_name("erp");
        assert_eq!(ms.connection.driver, "mssql");
        assert_eq!(ms.connection.host.as_deref(), Some("erp"));
        assert_eq!(ms.connection.port, Some(1433));
        assert_eq!(ms.connection.database.as_deref(), Some("ERP"));
        assert_eq!(ms.connection.fields.get("instance").map(String::as_str), Some("SQL2019"));

        let ora = by_name("legacy");
        assert_eq!(ora.connection.driver, "oracle");
        assert_eq!(ora.connection.fields.get("service_name").map(String::as_str), Some("ORCL"));
        assert!(ora.notes.iter().any(|n| n.contains("SID")));

        let mongo = by_name("docs");
        assert_eq!(mongo.connection.driver, "mongodb");
        assert!(mongo.notes.iter().any(|n| n.contains("unsupported provider")));
    }

    #[test]
    fn jdbc_urls() {
        let p = parse_jdbc_url("jdbc:postgresql://[::1]:5433/db?ssl=true");
        assert_eq!(p.host.as_deref(), Some("::1"));
        assert_eq!(p.port, Some(5433));
        assert_eq!(p.database.as_deref(), Some("db"));
        let o = parse_jdbc_url("jdbc:oracle:thin:@//host:1521/svc");
        assert_eq!(o.host.as_deref(), Some("host"));
        assert_eq!(o.database.as_deref(), Some("svc"));
        assert!(!o.oracle_sid);
        let s = parse_jdbc_url("jdbc:sqlserver://h;databaseName=x;instanceName=I");
        assert_eq!(s.instance.as_deref(), Some("I"));
        assert_eq!(s.database.as_deref(), Some("x"));
        assert!(parse_jdbc_url("not a url").host.is_none());
    }

    #[test]
    fn rejects_other_json() {
        assert!(matches!(parse("{}"), Err(CoreError::Invalid(_))));
        assert!(matches!(parse("nope"), Err(CoreError::Invalid(_))));
    }
}
