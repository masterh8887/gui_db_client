//! Turn a saved [`Connection`] into a live [`Database`]: fetch the password from the credential
//! store, bring up the SSH tunnel when one is configured, substitute the tunnel's local
//! endpoint for host/port, and open the driver. The result owns the tunnel lease so the
//! tunnel stays up exactly as long as the connection object lives.
//!
//! Every failure names its step: tunnel problems are `CoreError::Tunnel`, driver login
//! failures stay `Auth` (the UI asks for the password again), everything else at connect
//! time becomes `Connect`.

use std::ops::Deref;
use std::sync::Mutex;

use crate::catalog::Catalog;
use crate::credentials::{self, CredentialStore};
use crate::error::{CoreError, Result};
use crate::monitor::Monitor;
use crate::source::{self, ConnectParams, Database, Source};
use crate::tunnel::{self, TunnelLease, TunnelManager};
use crate::types::{Connection, ServerInfo, SshAuth};

/// Steps of "test connection" / every open, in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Tunnel,
    Connect,
    Probe,
}

impl Step {
    pub fn name(self) -> &'static str {
        match self {
            Step::Tunnel => "tunnel",
            Step::Connect => "connect",
            Step::Probe => "probe",
        }
    }
}

/// Map a failure onto the step it happened in, keeping the codes the UI branches on.
pub fn step_error(step: Step, e: CoreError) -> CoreError {
    match (step, e) {
        (_, CoreError::Cancelled) => CoreError::Cancelled,
        (Step::Tunnel, CoreError::Tunnel(m)) => CoreError::Tunnel(m),
        (Step::Tunnel, e) => CoreError::Tunnel(e.to_string()),
        (Step::Connect, CoreError::Auth(m)) => CoreError::Auth(m),
        (Step::Connect, e @ CoreError::DriverMissing { .. }) => e,
        (Step::Connect, CoreError::Invalid(m)) => CoreError::Invalid(m),
        (Step::Connect, CoreError::Connect(m)) => CoreError::Connect(m),
        (Step::Connect, CoreError::Timeout(m)) => CoreError::Timeout(format!("connect: {m}")),
        (Step::Connect, e) => CoreError::Connect(e.to_string()),
        (Step::Probe, CoreError::Auth(m)) => CoreError::Auth(m),
        (Step::Probe, CoreError::Query { code, msg }) => CoreError::Query { code, msg: format!("probe: {msg}") },
        (Step::Probe, CoreError::Timeout(m)) => CoreError::Timeout(format!("probe: {m}")),
        (Step::Probe, e) => CoreError::Connect(format!("probe: {e}")),
    }
}

/// An open driver connection plus the tunnel lease that keeps its route alive.
pub struct LiveConnection {
    /// The saved connection this was opened from (never carries secrets).
    pub conn: Connection,
    pub application_name: String,
    pub tunnel_used: bool,
    /// Local endpoint actually dialled (`127.0.0.1:<port>` through a tunnel).
    pub endpoint: (String, u16),
    db: Box<dyn Database>,
    /// Dropped after `db`, releasing the tunnel lease (`Mutex` only so the type stays `Sync`).
    _lease: Mutex<Option<TunnelLease>>,
}

impl std::fmt::Debug for LiveConnection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LiveConnection")
            .field("conn_id", &self.conn.id)
            .field("driver", &self.conn.driver)
            .field("application_name", &self.application_name)
            .field("tunnel_used", &self.tunnel_used)
            .field("endpoint", &self.endpoint)
            .finish_non_exhaustive()
    }
}

impl LiveConnection {
    pub fn db(&self) -> &dyn Database {
        &*self.db
    }

    pub fn source(&self) -> &dyn Source {
        &*self.db
    }

    pub fn catalog(&self) -> &dyn Catalog {
        &*self.db
    }

    pub fn monitor(&self) -> &dyn Monitor {
        &*self.db
    }

    /// Steps that ran to open this connection (`tunnel`?, `connect`).
    pub fn steps(&self) -> Vec<String> {
        let mut v = Vec::with_capacity(3);
        if self.tunnel_used {
            v.push(Step::Tunnel.name().to_string());
        }
        v.push(Step::Connect.name().to_string());
        v
    }

    /// `probe()` with the error attributed to the probe step.
    pub async fn probe(&self) -> Result<ServerInfo> {
        self.db.probe().await.map_err(|e| step_error(Step::Probe, e))
    }
}

impl Deref for LiveConnection {
    type Target = dyn Database;
    fn deref(&self) -> &Self::Target {
        &*self.db
    }
}

impl Drop for LiveConnection {
    fn drop(&mut self) {
        tracing::debug!(conn_id = %self.conn.id, application_name = %self.application_name, "connection closed");
    }
}

/// What [`open_connection`] needs besides the stores.
pub struct ConnectRequest<'a> {
    pub conn: &'a Connection,
    /// Password typed for this call (connections saved with `save_password = false`, or a
    /// value the user is testing before saving). `None` falls back to the credential store.
    pub password: Option<String>,
    /// `bulkdb-ui`, `bulkdb-preflight`, `bulkdb-monitor`, `bulkdb-<job id>`.
    pub application_name: &'a str,
    /// Port used when the connection has none (the driver's registry default).
    pub default_port: Option<u16>,
}

/// Well-known default ports, used when the registry gives none.
pub fn builtin_default_port(driver: &str) -> Option<u16> {
    match driver {
        "postgres" => Some(5432),
        "mysql" => Some(3306),
        "mssql" => Some(1433),
        "oracle" => Some(1521),
        _ => None,
    }
}

/// Which credential-store key holds the ssh secret of a connection, if any.
pub fn ssh_secret_key(conn: &Connection) -> Option<String> {
    let ssh = conn.ssh.as_ref()?;
    match ssh.auth {
        SshAuth::Password => Some(credentials::ssh_password_key(&conn.id)),
        SshAuth::Key => Some(credentials::ssh_passphrase_key(&conn.id)),
        SshAuth::Agent => None,
    }
}

/// Driver field holding a complete ODBC-style connection string (`key=value;...`), used verbatim
/// by the ODBC driver.
pub const CONNECTION_STRING_FIELD: &str = "connection_string";

/// Connection-string keywords (upper-cased) whose value is the database password. The ODBC
/// driver appends `PWD=` from the connection's password at connect time, so `conn_save` can
/// move these into the credential store as the connection's password.
const CONNECTION_STRING_PASSWORD_KEYS: [&str; 2] = ["PWD", "PASSWORD"];

/// Keyword endings (upper-cased, separators and trailing digits removed) that mark a secret
/// value: `PWD`, `PWD1`..`PWD3` (MySQL multi-factor), `priv_key_file_pwd`, `SSLPassword`,
/// `Auth_AccessToken`, `token`, `api_key`, `private_key` ... Matching the *end* of the keyword
/// (like `logging::mask_secrets` does) keeps look-alikes such as `PWDPolicy` or `TokenCacheTTL`
/// out.
const SECRET_KEY_SUFFIXES: [&str; 7] = ["PWD", "PASSWORD", "PASSWD", "PASSPHRASE", "TOKEN", "APIKEY", "PRIVATEKEY"];

/// Whether an ODBC connection-string keyword names a secret value.
///
/// The keyword is compared upper-cased with everything but ASCII letters and digits removed
/// and trailing digits dropped: it is a secret when it ends with `PWD`, `PASSWORD`, `PASSWD`,
/// `PASSPHRASE`, `TOKEN`, `APIKEY` or `PRIVATEKEY`, or contains `SECRET` anywhere (`KeyStoreSecret`, `client_secret`,
/// `SecretAccessKey`). This is the same family of names `logging::mask_secrets` masks.
pub fn is_connection_string_secret_key(key: &str) -> bool {
    let mut norm: String = key.chars().filter(char::is_ascii_alphanumeric).map(|c| c.to_ascii_uppercase()).collect();
    let without_digits = norm.trim_end_matches(|c: char| c.is_ascii_digit()).len();
    norm.truncate(without_digits);
    !norm.is_empty() && (norm.contains("SECRET") || SECRET_KEY_SUFFIXES.iter().any(|s| norm.ends_with(s)))
}

/// One `key=value` attribute of a connection string.
struct ConnStrAttr<'a> {
    /// The attribute as written (trimmed, without its terminating `;`).
    text: &'a str,
    /// Keyword as written (trimmed), for messages.
    raw_key: &'a str,
    /// Keyword, trimmed and upper-cased.
    key: String,
    /// Decoded value (`{...}` braces removed, `}}` unescaped); `None` when there is no `=`.
    value: Option<String>,
}

/// Split an ODBC connection string into attributes. Follows the same grammar as the ODBC
/// driver's own parser: `;` separates attributes, a value starting with `{` runs to the matching
/// `}` (a doubled `}}` is a literal brace) and may contain `;` and `=`. All delimiters are ASCII,
/// so byte offsets always fall on UTF-8 boundaries.
fn connection_string_attrs(s: &str) -> Vec<ConnStrAttr<'_>> {
    let b = s.as_bytes();
    let n = b.len();
    let mut attrs = Vec::new();
    let mut i = 0;
    while i < n {
        // Separators and blanks before a key.
        while i < n && (b[i] == b';' || b[i].is_ascii_whitespace()) {
            i += 1;
        }
        if i >= n {
            break;
        }
        let start = i;
        while i < n && b[i] != b'=' && b[i] != b';' {
            i += 1;
        }
        let raw_key = s[start..i].trim();
        let key = raw_key.to_ascii_uppercase();
        if i >= n || b[i] == b';' {
            // A bare word without `=`: keep it as written.
            attrs.push(ConnStrAttr { text: s[start..i].trim(), raw_key, key, value: None });
            i += 1;
            continue;
        }
        i += 1; // past '='
        while i < n && b[i].is_ascii_whitespace() {
            i += 1;
        }
        let value = if i < n && b[i] == b'{' {
            i += 1;
            let mut v = String::new();
            let mut seg = i;
            while i < n {
                if b[i] == b'}' {
                    v.push_str(&s[seg..i]);
                    if i + 1 < n && b[i + 1] == b'}' {
                        v.push('}');
                        i += 2;
                        seg = i;
                        continue;
                    }
                    break;
                }
                i += 1;
            }
            if i >= n {
                // Unterminated brace: the rest of the string is the value.
                v.push_str(&s[seg..]);
            }
            // Anything between the closing brace and the next `;` is ignored, as ODBC does.
            while i < n && b[i] != b';' {
                i += 1;
            }
            v
        } else {
            let vstart = i;
            while i < n && b[i] != b';' {
                i += 1;
            }
            s[vstart..i].trim().to_string()
        };
        attrs.push(ConnStrAttr { text: s[start..i].trim(), raw_key, key, value: Some(value) });
        i += 1; // past ';'
    }
    attrs
}

/// Remove every secret attribute (any keyword [`is_connection_string_secret_key`] accepts:
/// `PWD=`, `Password=`, `KeyStoreSecret=`, `Auth_AccessToken=` ..., any case, braced or not)
/// from an ODBC connection string.
///
/// Returns `None` when the string carries no secret attribute (leave it untouched). Otherwise
/// returns the string without those attributes (each remaining attribute exactly as written,
/// `;`-terminated) and the decoded value of the first password attribute (`PWD=` /
/// `PASSWORD=`; `None` when there is none or it is empty). The ODBC driver appends `PWD=` again
/// from the connection's password at connect time, so that value belongs in the credential
/// store. Other secret attributes have no such home: callers that must not lose them check
/// [`connection_string_unstorable_secrets`] first (`conn_save` refuses them). Either way no
/// secret ever reaches `connections.toml` or an IPC payload, which is why `ConfigStore::upsert`
/// refuses any string for which this returns `Some`.
pub fn split_connection_string_secret(conn_str: &str) -> Option<(String, Option<String>)> {
    let attrs = connection_string_attrs(conn_str);
    let is_secret = |a: &ConnStrAttr<'_>| is_connection_string_secret_key(&a.key);
    if !attrs.iter().any(is_secret) {
        return None;
    }
    let mut cleaned = String::with_capacity(conn_str.len());
    let mut first: Option<Option<String>> = None;
    for a in attrs {
        if !is_secret(&a) {
            cleaned.push_str(a.text);
            cleaned.push(';');
        } else if first.is_none() && CONNECTION_STRING_PASSWORD_KEYS.contains(&a.key.as_str()) {
            first = Some(a.value);
        }
    }
    Some((cleaned, first.flatten().filter(|v| !v.is_empty())))
}

/// Keywords (as written, first spelling of each, in order) of the secret attributes in an ODBC
/// connection string that hold a value but cannot become the connection's password: everything
/// [`is_connection_string_secret_key`] accepts except `PWD` / `PASSWORD` themselves
/// (`PWD2=`, `KeyStoreSecret=`, `token=`, `priv_key_file_pwd=` ...). There is nowhere safe to
/// keep their values (the credential store holds one database password per connection), so a
/// connection that carries one is refused by `conn_save`; such secrets belong in an ODBC data
/// source (DSN) instead. Bare keywords and empty values are not listed: stripping them loses
/// nothing. Only keyword names are returned, never values.
pub fn connection_string_unstorable_secrets(conn_str: &str) -> Vec<String> {
    let mut keys: Vec<String> = Vec::new();
    for a in connection_string_attrs(conn_str) {
        if a.value.as_deref().is_some_and(|v| !v.is_empty())
            && is_connection_string_secret_key(&a.key)
            && !CONNECTION_STRING_PASSWORD_KEYS.contains(&a.key.as_str())
            && !keys.iter().any(|k| k.eq_ignore_ascii_case(a.raw_key))
        {
            keys.push(a.raw_key.to_string());
        }
    }
    keys
}

/// Resolve the database password: the explicit value, else whatever the credential store holds
/// for this connection, else none (the driver decides whether that is an error). "Ask every
/// time" connections (`save_password = false`) have no stored entry because `conn_save`
/// removes it, so the lookup is harmless for them.
pub fn resolve_password(conn: &Connection, explicit: Option<String>, creds: &dyn CredentialStore) -> Result<Option<String>> {
    match explicit {
        Some(p) => Ok(Some(p)),
        None => creds.get(&credentials::db_password_key(&conn.id)).map_err(|e| CoreError::Internal(format!("cannot read the stored password for '{}': {e}", conn.name))),
    }
}

/// Tunnel (if configured) -> connect. See the module docs for the error mapping.
pub async fn open_connection(req: ConnectRequest<'_>, creds: &dyn CredentialStore, tunnels: &TunnelManager) -> Result<LiveConnection> {
    let conn = req.conn;
    if conn.driver.trim().is_empty() {
        return Err(CoreError::Invalid(format!("connection '{}' has no driver", conn.name)));
    }
    if !source::is_compiled(&conn.driver) {
        return Err(CoreError::DriverMissing { id: conn.driver.clone(), detail: "driver is not compiled into this build".into() });
    }
    let password = resolve_password(conn, req.password, creds)?;
    let target_host = conn.host.clone().filter(|h| !h.trim().is_empty()).unwrap_or_else(|| "localhost".into());
    let target_port = conn.port.filter(|p| *p != 0).or(req.default_port).or_else(|| builtin_default_port(&conn.driver));

    let mut lease: Option<TunnelLease> = None;
    let mut tunnel_used = false;
    if let Some(ssh) = conn.ssh.as_ref().filter(|s| s.enabled) {
        let port = target_port.ok_or_else(|| CoreError::Tunnel(format!("connection '{}' needs a database port to tunnel to", conn.name)))?;
        if ssh.auth == SshAuth::Key {
            if let Some(key) = ssh.key_path.as_deref().filter(|k| !k.trim().is_empty()) {
                tunnel::validate_key_path(key).map_err(|e| step_error(Step::Tunnel, e))?;
            }
        }
        let secret = match ssh_secret_key(conn) {
            Some(key) => creds.get(&key).map_err(|e| CoreError::Tunnel(format!("cannot read the stored ssh secret: {e}")))?,
            None => None,
        };
        tracing::info!(conn_id = %conn.id, bastion = %ssh.host, target = %format!("{target_host}:{port}"), "acquiring ssh tunnel");
        let l = tunnels.acquire(ssh, &target_host, port, secret).await.map_err(|e| step_error(Step::Tunnel, e))?;
        tracing::info!(conn_id = %conn.id, local_port = l.local_port, "tunnel ready");
        lease = Some(l);
        tunnel_used = true;
    }

    let endpoint = match &lease {
        Some(l) => l.local(),
        None => (target_host.clone(), target_port.unwrap_or(0)),
    };
    let params = ConnectParams::from_connection(conn, password, Some(endpoint.clone()), req.application_name);
    tracing::debug!(conn_id = %conn.id, driver = %conn.driver, application_name = %req.application_name, "opening connection");
    let db = source::open(&params).await.map_err(|e| step_error(Step::Connect, e))?;
    Ok(LiveConnection { conn: conn.clone(), application_name: req.application_name.to_string(), tunnel_used, endpoint, db, _lease: Mutex::new(lease) })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::credentials::MemoryStore;
    use crate::types::SshConfig;
    use std::time::Duration;

    #[test]
    fn errors_keep_meaningful_codes() {
        assert_eq!(step_error(Step::Tunnel, CoreError::Io("x".into())).code(), "TUNNEL");
        assert_eq!(step_error(Step::Connect, CoreError::Auth("x".into())).code(), "AUTH");
        assert_eq!(step_error(Step::Connect, CoreError::Io("x".into())).code(), "CONNECT");
        assert_eq!(step_error(Step::Connect, CoreError::DriverMissing { id: "oracle".into(), detail: "d".into() }).code(), "DRIVER_MISSING");
        assert_eq!(step_error(Step::Probe, CoreError::query("x")).code(), "QUERY");
        assert!(step_error(Step::Probe, CoreError::query("x")).to_string().contains("probe:"));
        assert_eq!(step_error(Step::Probe, CoreError::Cancelled).code(), "CANCELLED");
    }

    #[test]
    fn password_resolution() {
        let creds = MemoryStore::default();
        let conn = Connection { id: "c1".into(), name: "n".into(), driver: "postgres".into(), save_password: true, ..Default::default() };
        assert_eq!(resolve_password(&conn, None, &creds).unwrap(), None, "nothing stored yet");
        creds.set(&credentials::db_password_key("c1"), "stored").unwrap();
        assert_eq!(resolve_password(&conn, Some("typed".into()), &creds).unwrap().as_deref(), Some("typed"));
        assert_eq!(resolve_password(&conn, None, &creds).unwrap().as_deref(), Some("stored"));
        // "Ask every time": conn_save removes the stored entry, so nothing comes back.
        creds.delete(&credentials::db_password_key("c1")).unwrap();
        let conn = Connection { save_password: false, ..conn };
        assert_eq!(resolve_password(&conn, None, &creds).unwrap(), None);
    }

    #[test]
    fn ssh_secret_keys_follow_auth() {
        let mut conn = Connection { id: "c1".into(), ..Default::default() };
        assert_eq!(ssh_secret_key(&conn), None);
        let mut ssh = SshConfig { enabled: true, host: "b".into(), port: 22, user: "u".into(), auth: SshAuth::Password, key_path: None, compression: true };
        conn.ssh = Some(ssh.clone());
        assert_eq!(ssh_secret_key(&conn).as_deref(), Some("c1/ssh"));
        ssh.auth = SshAuth::Key;
        conn.ssh = Some(ssh.clone());
        assert_eq!(ssh_secret_key(&conn).as_deref(), Some("c1/ssh-passphrase"));
        ssh.auth = SshAuth::Agent;
        conn.ssh = Some(ssh);
        assert_eq!(ssh_secret_key(&conn), None);
    }

    #[test]
    fn connection_string_password_is_split_out() {
        let split = split_connection_string_secret;
        let some = |rest: &str, pw: Option<&str>| Some((rest.to_string(), pw.map(str::to_string)));
        assert_eq!(split("DSN=x;UID=u;PWD=secret"), some("DSN=x;UID=u;", Some("secret")));
        // Braced values may hold `;`, `=` and doubled braces; other attributes stay as written.
        assert_eq!(
            split("Driver={ODBC Driver 18 for SQL Server};Server=h,1;PWD={a;b=c}}};uid=x"),
            some("Driver={ODBC Driver 18 for SQL Server};Server=h,1;uid=x;", Some("a;b=c}"))
        );
        assert_eq!(split("dsn=foo; password = p w ;"), some("dsn=foo;", Some("p w")));
        assert_eq!(split("DSN=데이터;Pwd=비밀번호"), some("DSN=데이터;", Some("비밀번호")));
        // Every password attribute goes, the first one's value wins.
        assert_eq!(split("PWD=one;DSN=a;PASSWORD=two"), some("DSN=a;", Some("one")));
        // Empty values are stripped but yield no secret; a bare keyword is stripped too.
        assert_eq!(split("  ;;PWD=;DSN=a"), some("DSN=a;", None));
        assert_eq!(split("PWD;DSN=a"), some("DSN=a;", None));
        assert_eq!(split("PWD={unterminated;DSN=a"), some("", Some("unterminated;DSN=a")));
        // No password attribute: untouched (`PWD=` inside a braced value is not a keyword).
        assert_eq!(split("Driver={x;PWD=y};Server=s"), None);
        assert_eq!(split("DSN=foo"), None);
        assert_eq!(split(""), None);
    }

    #[test]
    fn secret_attribute_keys() {
        for key in [
            "PWD",
            "pwd",
            "Password",
            "PWD1",
            "pwd3",
            "SSLPassword",
            "priv_key_file_pwd",
            "KeyStoreSecret",
            "client_secret",
            "SecretAccessKey",
            "token",
            "Auth_AccessToken",
            "OAuth-Refresh-Token",
            "api_key",
            "APIKey",
            "private_key",
            "passphrase",
            "Passwd",
        ] {
            assert!(is_connection_string_secret_key(key), "{key} is a secret");
        }
        for key in ["DSN", "UID", "Driver", "PWDPolicy", "OldPwdHint", "TokenCacheTTL", "Trusted_Connection", "SSLKey", "priv_key_file", "Authentication", "", "123"] {
            assert!(!is_connection_string_secret_key(key), "{key} is not a secret");
        }
    }

    #[test]
    fn every_secret_attribute_is_split_out_but_only_pwd_is_the_password() {
        let split = split_connection_string_secret;
        let some = |rest: &str, pw: Option<&str>| Some((rest.to_string(), pw.map(str::to_string)));
        // Non-password secrets are cut out too (so ConfigStore::upsert refuses them) but never
        // become the password.
        assert_eq!(split("Driver={x};KeyStoreSecret={k;s}}x};UID=u"), some("Driver={x};UID=u;", None));
        assert_eq!(split("token=t1;DSN=a;PWD=p;PWD2=p2"), some("DSN=a;", Some("p")));
        assert_eq!(split("DSN=a;PWDPolicy=strict"), None);

        assert!(connection_string_unstorable_secrets("DSN=a;PWD=p;Password=q;PWDPolicy=x").is_empty());
        assert!(connection_string_unstorable_secrets("Driver={x;token=y}").is_empty(), "inside a braced value");
        assert_eq!(
            connection_string_unstorable_secrets("DSN=a;PWD=p;pwd2={2nd};KeyStoreSecret=s;PWD2=again;Auth_AccessToken = t"),
            vec!["pwd2".to_string(), "KeyStoreSecret".into(), "Auth_AccessToken".into()]
        );
        // Bare keywords and empty values hold nothing to lose: split strips them, nothing refuses.
        assert!(connection_string_unstorable_secrets("DSN=a;token;client_secret=;PWD2={}").is_empty());
        assert_eq!(split("DSN=a;token;client_secret=;PWD2={}"), some("DSN=a;", None));
    }

    #[tokio::test]
    async fn unknown_driver_is_driver_missing_and_ppk_is_tunnel_error() {
        let creds = MemoryStore::default();
        let tunnels = TunnelManager::new(std::sync::Arc::new(tunnel::DenyAllPrompts), Duration::from_secs(1));
        let conn = Connection { id: "c".into(), name: "n".into(), driver: "db2".into(), ..Default::default() };
        let req = ConnectRequest { conn: &conn, password: None, application_name: "bulkdb-ui", default_port: None };
        assert!(matches!(open_connection(req, &creds, &tunnels).await, Err(CoreError::DriverMissing { .. })));

        let conn = Connection {
            id: "c".into(),
            name: "n".into(),
            driver: "postgres".into(),
            host: Some("db".into()),
            ssh: Some(SshConfig { enabled: true, host: "b".into(), port: 22, user: "u".into(), auth: SshAuth::Key, key_path: Some("k.ppk".into()), compression: true }),
            ..Default::default()
        };
        let req = ConnectRequest { conn: &conn, password: None, application_name: "bulkdb-ui", default_port: None };
        let err = open_connection(req, &creds, &tunnels).await.unwrap_err();
        assert!(matches!(err, CoreError::Tunnel(ref m) if m.contains("PuTTY")), "{err}");
    }
}
