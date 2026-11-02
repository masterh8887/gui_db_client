//! MySQL connection settings, the per-connection text-encoding plan and protocol dispatch
//! (design: `docs/mysql-compat.md`, sections 3 and 5). Owner: **shared**.
//!
//! Registry fields of the `mysql` driver (all optional, strings in `Connection.fields`):
//!
//! | field                | values                                   | default |
//! |----------------------|------------------------------------------|---------|
//! | `protocol`           | `auto` / `modern` / `legacy`             | `auto`  |
//! | `encoding`           | `auto` / `utf8` / `euc-kr` / `latin1`    | `auto`  |
//! | `allow_old_password` | `true` / `false`                         | `false` |
//!
//! [`open`] is what `source::open` calls for `driver = "mysql"`:
//!
//! * `modern` -> [`MySqlDatabase::connect`] (mysql_async, 4.1+).
//! * `legacy` -> [`MySqlLegacyDatabase::connect`] (pre-4.1 client; refuses a 4.1+ greeting).
//! * `auto` -> if this process already saw a 4.1+ greeting from `host:port`, go straight to
//!   mysql_async; otherwise open one TCP connection and read the greeting: pre-4.1 -> keep the
//!   socket and continue the legacy handshake on it; 4.1+ -> answer the greeting with a
//!   credential-free login ([`handshake::decline`]), close it, remember `host:port` as modern
//!   and connect with mysql_async.
//!
//! Rule (design doc section 3): no path drops a socket after the server greeting without a
//! login attempt. MySQL counts every such drop toward `max_connect_errors` (default 10) and
//! then blocks the client host until a DBA runs `FLUSH HOSTS`; with a wrong configuration
//! that would happen on every attempt. So what needs no server is checked before any socket
//! is opened: field values, and TLS. This build has no TLS in either client, so
//! `sslmode=require` / `verify-full` is a `CONNECT` error up front on every protocol
//! ([`check_tls_supported`] for `auto` / `modern`, `mysql_legacy::precheck` for `legacy`), and
//! `prefer` makes mysql_async connect in clear text without a TLS attempt (which would end
//! right after the greeting). Checks that need the greeting end with `decline`. A greeting
//! mysql_async cannot use (4.0 under `modern`) still gets its login packet, which the server
//! rejects.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use super::mysql::{check_tls_supported, MySqlDatabase, ServerVersion};
use super::mysql_common::MysqlCaps;
use super::mysql_legacy::{handshake, MySqlLegacyDatabase};
use super::{ConnectParams, Database};
use crate::encoding::{Charset, CollationMap};
use crate::error::{CoreError, Result};

pub const FIELD_PROTOCOL: &str = "protocol";
pub const FIELD_ENCODING: &str = "encoding";
pub const FIELD_ALLOW_OLD_PASSWORD: &str = "allow_old_password";

/// Which client speaks to the server.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ProtocolSetting {
    /// Read the greeting and choose (default).
    #[default]
    Auto,
    /// mysql_async: MySQL 4.1+ / MariaDB.
    Modern,
    /// BulkDB's pre-4.1 client: MySQL 3.23 / 4.0.
    Legacy,
}

impl ProtocolSetting {
    /// `auto` (or empty), `modern` (`4.1`, `41`, `new`), `legacy` (`4.0`, `40`, `old`,
    /// `pre41`); anything else is `INVALID`.
    pub fn parse(s: &str) -> Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "" | "auto" => Ok(ProtocolSetting::Auto),
            "modern" | "4.1" | "41" | "new" => Ok(ProtocolSetting::Modern),
            "legacy" | "4.0" | "40" | "old" | "pre41" | "pre-4.1" => Ok(ProtocolSetting::Legacy),
            other => Err(CoreError::Invalid(format!("mysql field protocol: unknown value '{other}' (use auto, modern or legacy)"))),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            ProtocolSetting::Auto => "auto",
            ProtocolSetting::Modern => "modern",
            ProtocolSetting::Legacy => "legacy",
        }
    }
}

/// What the bytes stored on the server are, as the user declares it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EncodingSetting {
    /// 4.1+: the server converts to UTF-8; 4.0: decode with the server's `character_set`.
    #[default]
    Auto,
    /// Data is UTF-8 (4.1+: same as auto; 4.0: decode bytes as UTF-8).
    Utf8,
    /// Data is EUC-KR / CP949, wherever it is stored (also in `latin1` columns).
    EucKr,
    /// Data is latin1 (windows-1252).
    Latin1,
}

impl EncodingSetting {
    /// `auto` (or empty), `utf8` (`utf-8`, `utf8mb4`), `euc-kr` (`euckr`, `euc_kr`, `cp949`,
    /// `ksc5601`), `latin1` (`windows-1252`, `cp1252`); anything else is `INVALID`.
    pub fn parse(s: &str) -> Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "" | "auto" => Ok(EncodingSetting::Auto),
            "utf8" | "utf-8" | "utf8mb4" | "utf8mb3" => Ok(EncodingSetting::Utf8),
            "euc-kr" | "euckr" | "euc_kr" | "cp949" | "uhc" | "ksc5601" => Ok(EncodingSetting::EucKr),
            "latin1" | "windows-1252" | "cp1252" => Ok(EncodingSetting::Latin1),
            other => Err(CoreError::Invalid(format!("mysql field encoding: unknown value '{other}' (use auto, utf8, euc-kr or latin1)"))),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            EncodingSetting::Auto => "auto",
            EncodingSetting::Utf8 => "utf8",
            EncodingSetting::EucKr => "euc-kr",
            EncodingSetting::Latin1 => "latin1",
        }
    }
}

/// The MySQL-specific connection fields, parsed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MysqlConnOpts {
    pub protocol: ProtocolSetting,
    pub encoding: EncodingSetting,
    /// Let mysql_async answer a 4.1+ server's switch to `mysql_old_password`
    /// (`OptsBuilder::secure_auth(false)`). Pre-4.1 servers always use the old scheme.
    pub allow_old_password: bool,
}

impl MysqlConnOpts {
    /// Parse the three fields; any unknown value is `INVALID` naming the field (never a silent
    /// default: a mistyped `allow_old_password` would otherwise turn into an `AUTH` failure).
    pub fn from_params(p: &ConnectParams) -> Result<Self> {
        Ok(MysqlConnOpts {
            protocol: ProtocolSetting::parse(p.field(FIELD_PROTOCOL).unwrap_or(""))?,
            encoding: EncodingSetting::parse(p.field(FIELD_ENCODING).unwrap_or(""))?,
            allow_old_password: parse_bool_field(FIELD_ALLOW_OLD_PASSWORD, p.field(FIELD_ALLOW_OLD_PASSWORD), false)?,
        })
    }
}

/// A boolean connection field: `true` / `false`, `1` / `0`, `yes` / `no`, `on` / `off`
/// (case-insensitive, surrounding blanks ignored); absent or empty -> `default`; anything else
/// is `INVALID` naming the field.
pub fn parse_bool_field(name: &str, value: Option<&str>, default: bool) -> Result<bool> {
    let Some(raw) = value else { return Ok(default) };
    match raw.trim().to_ascii_lowercase().as_str() {
        "" => Ok(default),
        "true" | "1" | "yes" | "on" => Ok(true),
        "false" | "0" | "no" | "off" => Ok(false),
        _ => Err(CoreError::Invalid(format!("mysql field {name}: unknown value '{}' (use true or false)", raw.trim()))),
    }
}

/// How one connection encodes SQL and decodes result text. Decided once, right after the
/// handshake, by [`plan_text`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TextPlan {
    /// 4.1+: `SET NAMES <names>` (`utf8mb4` on 5.5.3+, else `utf8`); SQL is sent as UTF-8
    /// and every text column arrives as UTF-8 (the server converts).
    ServerUtf8 { names: &'static str },
    /// 4.1+: `SET character_set_client = <client>, character_set_connection = <client>,
    /// character_set_results = NULL`. SQL is encoded with `legacy`; the server converts
    /// nothing on the way out, each column is decoded by its own collation id: utf8 /
    /// utf8mb4 -> UTF-8, binary -> bytes, euckr -> CP949, everything else (latin1, ascii,
    /// unknown) -> `legacy`. `client` is `latin1` when the database default charset is latin1
    /// (literals then compare byte-for-byte with CP949 stored in latin1 columns), else `euckr`.
    PassThrough { client: &'static str, legacy: Charset },
    /// Pre-4.1: one server-wide charset and no conversion at all; SQL is encoded and every
    /// text column decoded with `charset`.
    Fixed { charset: Charset },
}

impl TextPlan {
    /// Charset the SQL text is encoded with before it is sent.
    pub fn sql_charset(&self) -> Charset {
        match self {
            TextPlan::ServerUtf8 { .. } => Charset::Utf8,
            TextPlan::PassThrough { legacy, .. } => *legacy,
            TextPlan::Fixed { charset } => *charset,
        }
    }

    /// Statements to run right after connecting (and after every reconnect).
    pub fn session_statements(&self) -> Vec<String> {
        match self {
            TextPlan::ServerUtf8 { names } => vec![format!("SET NAMES {names}")],
            TextPlan::PassThrough { client, .. } => {
                vec![format!("SET character_set_client = {client}, character_set_connection = {client}, character_set_results = NULL")]
            }
            TextPlan::Fixed { .. } => vec![],
        }
    }

    /// Charset of a result column. `collation` is the column definition's charset id (`None`
    /// on the pre-4.1 protocol). Binary string columns are routed to `Cell::Bytes` by the
    /// column kind before this matters.
    pub fn column_charset(&self, collation: Option<u16>, map: &CollationMap) -> Charset {
        match self {
            TextPlan::ServerUtf8 { .. } => match collation {
                Some(63) => Charset::Binary,
                _ => Charset::Utf8,
            },
            TextPlan::PassThrough { legacy, .. } => match collation.and_then(|id| map.get(id)) {
                Some(Charset::Binary) => Charset::Binary,
                Some(Charset::Utf8) => Charset::Utf8,
                Some(Charset::EucKr) => Charset::EucKr,
                Some(Charset::Latin1) | None => *legacy,
            },
            TextPlan::Fixed { charset } => *charset,
        }
    }

    /// Whether metadata from UTF-8 columns gets the latin1 double-encoding repair
    /// (`encoding::repair_latin1_mojibake`): EUC-KR data over a latin1 client charset.
    pub fn repairs_metadata(&self) -> bool {
        matches!(self, TextPlan::PassThrough { client: "latin1", legacy: Charset::EucKr })
    }

    /// One-line description for probe notes / `settings["text_plan"]`.
    pub fn describe(&self) -> String {
        match self {
            TextPlan::ServerUtf8 { names } => format!("server converts text to UTF-8 (SET NAMES {names})"),
            TextPlan::PassThrough { client, legacy } => {
                format!("no server-side conversion (character_set_results = NULL, client charset {client}); non-UTF-8 text decoded as {}", legacy.label())
            }
            TextPlan::Fixed { charset } => format!("pre-4.1 server: text decoded as {}", charset.label()),
        }
    }
}

/// Decide the [`TextPlan`] (design doc section 5). Returns the plan and an optional note for
/// the probe result (shown under "test connection").
///
/// * pre-4.1: `auto` -> the server's `character_set` (`euc_kr` -> EUC-KR, `latin1` ->
///   latin1; anything else -> latin1 plus a note asking for an explicit encoding); `utf8` /
///   `euc-kr` / `latin1` -> that charset.
/// * 4.1+: `auto` / `utf8` -> `ServerUtf8` (`utf8mb4` from 5.5.3, else `utf8`); with `auto`
///   and a latin1 database / server default, a note suggests `euc-kr` if Korean text looks
///   broken. `euc-kr` -> `PassThrough { client: latin1 if the database (else server) default
///   charset is latin1, else euckr; legacy: EUC-KR }`. `latin1` -> `PassThrough { latin1,
///   latin1 }`.
pub fn plan_text(
    enc: EncodingSetting,
    version: &ServerVersion,
    legacy_protocol: bool,
    server_charset: Option<&str>,
    database_charset: Option<&str>,
) -> (TextPlan, Option<String>) {
    if legacy_protocol || !MysqlCaps::for_version(version).set_names {
        return match enc {
            EncodingSetting::Auto => match server_charset.and_then(Charset::from_mysql_name).filter(|c| *c != Charset::Binary) {
                Some(charset) => (TextPlan::Fixed { charset }, None),
                None => (
                    TextPlan::Fixed { charset: Charset::Latin1 },
                    Some(format!(
                        "server character set '{}' is not supported; text is read as latin1. Choose encoding euc-kr, utf8 or latin1 explicitly",
                        server_charset.unwrap_or("unknown")
                    )),
                ),
            },
            EncodingSetting::Utf8 => (TextPlan::Fixed { charset: Charset::Utf8 }, None),
            EncodingSetting::EucKr => (TextPlan::Fixed { charset: Charset::EucKr }, None),
            EncodingSetting::Latin1 => (TextPlan::Fixed { charset: Charset::Latin1 }, None),
        };
    }
    let default_cs = database_charset.or(server_charset).and_then(Charset::from_mysql_name);
    match enc {
        EncodingSetting::Auto | EncodingSetting::Utf8 => {
            let names = if MysqlCaps::for_version(version).utf8mb4 { "utf8mb4" } else { "utf8" };
            let note = (enc == EncodingSetting::Auto && default_cs == Some(Charset::Latin1))
                .then(|| "default character set is latin1: if Korean text looks broken, set the connection encoding to euc-kr".to_string());
            (TextPlan::ServerUtf8 { names }, note)
        }
        EncodingSetting::EucKr => {
            let client = if default_cs == Some(Charset::Latin1) { "latin1" } else { "euckr" };
            (TextPlan::PassThrough { client, legacy: Charset::EucKr }, None)
        }
        EncodingSetting::Latin1 => (TextPlan::PassThrough { client: "latin1", legacy: Charset::Latin1 }, None),
    }
}

// ---------------------------------------------------------------------------------------------
// Protocol detection cache (process lifetime)
// ---------------------------------------------------------------------------------------------

fn modern_hosts() -> &'static Mutex<HashMap<(String, u16), ()>> {
    static CACHE: OnceLock<Mutex<HashMap<(String, u16), ()>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// `host:port` greeted with the 4.1+ protocol earlier in this process.
pub fn is_known_modern(host: &str, port: u16) -> bool {
    modern_hosts().lock().unwrap_or_else(|p| p.into_inner()).contains_key(&(host.to_ascii_lowercase(), port))
}

pub fn remember_modern(host: &str, port: u16) {
    modern_hosts().lock().unwrap_or_else(|p| p.into_inner()).insert((host.to_ascii_lowercase(), port), ());
}

pub fn forget_protocol(host: &str, port: u16) {
    modern_hosts().lock().unwrap_or_else(|p| p.into_inner()).remove(&(host.to_ascii_lowercase(), port));
}

/// Open a MySQL-family connection with the client the `protocol` field asks for (see the
/// module docs). Field values are validated first (`INVALID`).
pub async fn open(params: &ConnectParams) -> Result<Box<dyn Database>> {
    let opts = MysqlConnOpts::from_params(params)?;
    match opts.protocol {
        ProtocolSetting::Modern => Ok(Box::new(MySqlDatabase::connect(params).await?)),
        ProtocolSetting::Legacy => Ok(Box::new(MySqlLegacyDatabase::connect(params).await?)),
        ProtocolSetting::Auto => {
            // Neither client can do TLS in this build: refuse sslmode=require / verify-full
            // before the detection socket instead of declining a greeting for nothing.
            check_tls_supported(params)?;
            if is_known_modern(&params.host, params.port) {
                return match MySqlDatabase::connect(params).await {
                    Ok(db) => Ok(Box::new(db)),
                    Err(e) => {
                        // The server may have been replaced; detect again next time.
                        forget_protocol(&params.host, params.port);
                        Err(e)
                    }
                };
            }
            let (stream, greeting) = handshake::peek_greeting(&params.host, params.port, params.connect_timeout).await?;
            if greeting.is_pre41() {
                tracing::debug!("mysql {}:{}: pre-4.1 greeting ({}); using the legacy client", params.host, params.port, greeting.server_version);
                Ok(Box::new(MySqlLegacyDatabase::connect_with_greeting(params, stream, greeting).await?))
            } else {
                // mysql_async cannot take over this socket. Answer the greeting with a
                // credential-free login before closing it: a bare close would count toward the
                // server's max_connect_errors, and when the real login below fails (wrong
                // password, ...) nothing would reset that counter.
                handshake::decline(stream, &greeting, params.connect_timeout).await;
                remember_modern(&params.host, params.port);
                Ok(Box::new(MySqlDatabase::connect(params).await?))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> ServerVersion {
        ServerVersion::parse(s)
    }

    #[test]
    fn settings_parse() {
        assert_eq!(ProtocolSetting::parse("").unwrap(), ProtocolSetting::Auto);
        assert_eq!(ProtocolSetting::parse("Legacy").unwrap(), ProtocolSetting::Legacy);
        assert!(ProtocolSetting::parse("v3").is_err());
        assert_eq!(EncodingSetting::parse("EUC-KR").unwrap(), EncodingSetting::EucKr);
        assert_eq!(EncodingSetting::parse("cp949").unwrap(), EncodingSetting::EucKr);
        assert!(EncodingSetting::parse("sjis").is_err());
    }

    fn with_fields(fields: &[(&str, &str)]) -> ConnectParams {
        ConnectParams {
            driver: "mysql".into(),
            host: "db".into(),
            port: 3306,
            database: None,
            user: None,
            password: None,
            fields: fields.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
            application_name: "bulkdb-test".into(),
            connect_timeout: std::time::Duration::from_secs(1),
            ssl: crate::source::SslMode::Disable,
        }
    }

    #[test]
    fn conn_opts_reject_unknown_values() {
        assert_eq!(MysqlConnOpts::from_params(&with_fields(&[])).unwrap(), MysqlConnOpts::default());
        for (v, want) in [("true", true), ("YES", true), (" on ", true), ("1", true), ("false", false), ("0", false), ("No", false), ("off", false), ("", false)] {
            let o = MysqlConnOpts::from_params(&with_fields(&[(FIELD_ALLOW_OLD_PASSWORD, v)])).unwrap();
            assert_eq!(o.allow_old_password, want, "{v:?}");
        }
        for bad in ["ture", "2", "y", "enabled"] {
            let err = MysqlConnOpts::from_params(&with_fields(&[(FIELD_ALLOW_OLD_PASSWORD, bad)])).unwrap_err();
            assert!(matches!(&err, CoreError::Invalid(m) if m.contains("allow_old_password") && m.contains(bad)), "{bad}: {err:?}");
        }
        let err = MysqlConnOpts::from_params(&with_fields(&[(FIELD_PROTOCOL, "v3")])).unwrap_err();
        assert!(matches!(&err, CoreError::Invalid(m) if m.contains("protocol")), "{err:?}");
        let err = MysqlConnOpts::from_params(&with_fields(&[(FIELD_ENCODING, "sjis")])).unwrap_err();
        assert!(matches!(&err, CoreError::Invalid(m) if m.contains("encoding")), "{err:?}");
    }

    #[test]
    fn plans_for_legacy_servers() {
        let v40 = v("4.0.27-standard");
        assert_eq!(plan_text(EncodingSetting::Auto, &v40, true, Some("euc_kr"), None).0, TextPlan::Fixed { charset: Charset::EucKr });
        let (plan, note) = plan_text(EncodingSetting::Auto, &v40, true, Some("sjis"), None);
        assert_eq!(plan, TextPlan::Fixed { charset: Charset::Latin1 });
        assert!(note.is_some());
        assert_eq!(plan_text(EncodingSetting::EucKr, &v40, true, Some("latin1"), None).0, TextPlan::Fixed { charset: Charset::EucKr });
        assert!(plan_text(EncodingSetting::EucKr, &v40, true, None, None).0.session_statements().is_empty());
    }

    #[test]
    fn plans_for_modern_servers() {
        let (p, note) = plan_text(EncodingSetting::Auto, &v("8.0.36"), false, Some("utf8mb4"), Some("utf8mb4"));
        assert_eq!(p, TextPlan::ServerUtf8 { names: "utf8mb4" });
        assert!(note.is_none());
        assert_eq!(plan_text(EncodingSetting::Auto, &v("5.1.73"), false, None, None).0, TextPlan::ServerUtf8 { names: "utf8" });
        let (p, note) = plan_text(EncodingSetting::Auto, &v("5.5.62"), false, Some("latin1"), Some("latin1"));
        assert_eq!(p, TextPlan::ServerUtf8 { names: "utf8mb4" });
        assert!(note.is_some());
        let p = plan_text(EncodingSetting::EucKr, &v("5.0.96"), false, Some("latin1"), Some("latin1")).0;
        assert_eq!(p, TextPlan::PassThrough { client: "latin1", legacy: Charset::EucKr });
        assert!(p.repairs_metadata());
        assert_eq!(p.sql_charset(), Charset::EucKr);
        assert_eq!(p.session_statements(), vec!["SET character_set_client = latin1, character_set_connection = latin1, character_set_results = NULL".to_string()]);
        let p = plan_text(EncodingSetting::EucKr, &v("5.5.62"), false, Some("euckr"), None).0;
        assert_eq!(p, TextPlan::PassThrough { client: "euckr", legacy: Charset::EucKr });
        assert!(!p.repairs_metadata());
    }

    #[test]
    fn pass_through_column_charsets() {
        let map = CollationMap::builtin();
        let p = TextPlan::PassThrough { client: "latin1", legacy: Charset::EucKr };
        assert_eq!(p.column_charset(Some(8), &map), Charset::EucKr); // latin1 column holding CP949
        assert_eq!(p.column_charset(Some(19), &map), Charset::EucKr);
        assert_eq!(p.column_charset(Some(33), &map), Charset::Utf8);
        assert_eq!(p.column_charset(Some(63), &map), Charset::Binary);
        assert_eq!(p.column_charset(Some(999), &map), Charset::EucKr);
        let s = TextPlan::ServerUtf8 { names: "utf8mb4" };
        assert_eq!(s.column_charset(Some(8), &map), Charset::Utf8);
        assert_eq!(s.column_charset(Some(63), &map), Charset::Binary);
    }

    #[test]
    fn modern_cache() {
        assert!(!is_known_modern("db.example.test", 3399));
        remember_modern("DB.example.test", 3399);
        assert!(is_known_modern("db.example.test", 3399));
        forget_protocol("db.example.test", 3399);
        assert!(!is_known_modern("db.example.test", 3399));
    }
}
