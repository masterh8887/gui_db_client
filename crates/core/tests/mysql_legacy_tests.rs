//! Legacy MySQL (pre-4.1 protocol, old passwords, EUC-KR) against the in-process fake server
//! (`support::fake_mysql`). Owner: **legacy** (design doc section 10). Runs without docker.
//!
//! Cases (names are the contract for the CI summary):
//!
//! * `greeting_40_is_detected_as_legacy` — `peek_greeting` on `Mysql40` -> `is_pre41()`.
//! * `old_password_hash_matches_manual` — `hash_password_323(b"mypass")` = `6f8c114b58f2ce9e`.
//! * `fixture_checks_client_scramble` — the fixture's own `check_scramble_323` accepts the
//!   client's `scramble_323` and rejects a wrong password.
//! * `connect_40_with_old_auth` — `source::open` with protocol auto / legacy, right and wrong
//!   password (`AUTH`), empty password, `CONNECT_WITH_DB`, unknown database (`CONNECT`).
//! * `auto_mode_keeps_the_socket_for_40` — no `AbortedHandshake` event for a 4.0 server; a
//!   4.1 persona's detection socket gets one credential-free login (`decline`), none on the
//!   second connect (process cache), and no interrupted handshake.
//! * `legacy_refuses_41_greeting` — protocol=legacy against `Mysql41OldAuth` -> `CONNECT`,
//!   each refusal declined with a well-formed login.
//! * `fixture_counts_interrupted_handshakes` — the fixture counts a dropped socket, not a
//!   `decline`.
//! * `failing_configs_never_interrupt_a_handshake` — TLS required, invalid fields and login
//!   text the charset cannot encode: refused before connecting when possible, else declined
//!   after the greeting; `interrupted_handshakes()` stays 0.
//! * `modern_client_never_interrupts_a_handshake` — `sslmode=require` / `verify-full` on every
//!   protocol against 4.0 and 5.0 -> `CONNECT` with no TCP connection; `prefer` (default) ->
//!   one clear-text login per connect (no TLS attempt), right or wrong password; protocol
//!   modern against 4.0 -> a rejected login; `interrupted_handshakes()` stays 0.
//! * `greeting_time_host_errors_map_to_connect` — ERR 1129 / 1130 instead of the greeting ->
//!   `CONNECT` with the FLUSH HOSTS / grant hint, on every protocol setting.
//! * `old_auth_switch_41_needs_permission` — `Mysql41OldAuth` + `Mysql55PluginOldAuth`:
//!   `allow_old_password=false` -> `AUTH` containing `OLD_PASSWORD_HINT`; `true` -> connects
//!   (exercises the modern client).
//! * `euckr_text_decodes_to_utf8` — `Mysql40 { euc_kr }` rows with CP949 bytes (incl. UHC
//!   extension syllables such as "똠", NULL, empty string) -> Arrow Utf8 values.
//! * `latin1_server_with_euckr_setting` — `Mysql40 { latin1 }` + encoding=euc-kr -> decoded
//!   Korean; encoding=auto -> windows-1252 text (documented mojibake).
//! * `sql_is_sent_in_server_charset` — a Korean literal in the extraction SQL arrives as CP949
//!   bytes (`FakeServer::queries`); an emoji literal fails with `INVALID` before sending, at
//!   its line / column in the user's SQL (stream and EXPLAIN alike).
//! * `types_map_like_modern` — INT / BIGINT UNSIGNED / DECIMAL / DATE / DATETIME / 4.0
//!   compact TIMESTAMP / TIME / BLOB (binary) / TEXT / CHAR BINARY (text).
//! * `continuation_packets` — a 20 MB cell (16 MB split + tail) and an exact 0xFFFFFF payload.
//! * `batches_are_bounded` — 300_000 generated rows: batches <= 65_536 rows / 8 MB, row count,
//!   `fetch_batch_rows` honoured.
//! * `server_error_mid_query_keeps_the_session` — ERR instead of a result, then the session is
//!   reused; `;`-separated SQL is refused before sending.
//! * `cancel_kills_the_session_within_2s` — `Endless` rows, cancel -> `Cancelled` < 2 s and a
//!   `Killed { target }` event; the database reconnects for the next metadata call.
//! * `dropped_consumer_stops_a_slow_scan` — the stream (or the `stream()` future) is dropped
//!   while the server stalls before the next / first row: `KILL` within 2 s and the session
//!   slot is usable again.
//! * `cancellable_meta_query_kills_on_cancel` — `meta_query_cancellable` on an endless result.
//! * `catalog_on_40` — `SHOW DATABASES`, `SHOW TABLE STATUS` (4.0 `Type` column),
//!   `SHOW COLUMNS`, `SHOW INDEX`, `SHOW CREATE TABLE`, `exact_count` through the shared code.
//! * `explain_on_40` — tabular EXPLAIN parsed into a plan with estimated rows.
//! * `probe_and_session_on_40` — the shared probe reports the legacy protocol and text plan;
//!   `apply_session` sends only statements a 4.0 server knows.
//! * `export_euckr_server_to_utf8_and_euckr_csv` — source -> pipeline -> CSV sink from
//!   `Mysql40 { euc_kr }` to a UTF-8 CSV and to an EUC-KR CSV (byte-exact expected files).

mod support;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use arrow_array::cast::AsArray;
use arrow_array::types::{Date32Type, Decimal128Type, Float64Type, Int64Type, TimestampMicrosecondType};
use arrow_array::{Array, RecordBatch};
use arrow_schema::DataType;
use bulk_core::error::CoreError;
use bulk_core::job::{run_pipeline, Progress};
use bulk_core::sink::{self, SinkTarget};
use bulk_core::source::mysql_common::{MysqlRawQuery, OLD_PASSWORD_HINT};
use bulk_core::source::mysql_legacy::handshake::{hash_password_323, scramble_323, DECLINE_USER};
use bulk_core::source::mysql_legacy::{decline, peek_greeting, MySqlLegacyDatabase};
use bulk_core::source::{self, ConnectParams, Query, Source, SourceStream, SslMode, StreamOutput};
use bulk_core::types::{CsvEncoding, CsvOpts, OutputFormat, OutputOpts, SessionOpts, TableRef};
use bulk_core::CancellationToken;
use futures::StreamExt;
use support::fake_mysql::{check_scramble_323, flags, types, Account, Event, FakeColumn, FakeServer, Persona, Reply, RowSource, Script};

const MYPASS_HASH: [u32; 2] = [0x6f8c_114b, 0x58f2_ce9e];

fn euckr(s: &str) -> Vec<u8> {
    let (b, _, unmappable) = encoding_rs::EUC_KR.encode(s);
    assert!(!unmappable, "{s} is not representable in CP949");
    b.into_owned()
}

fn v(s: &str) -> Option<Vec<u8>> {
    Some(s.as_bytes().to_vec())
}

fn bulk_account() -> Vec<Account> {
    vec![Account { user: b"bulk".to_vec(), old_hash: Some(MYPASS_HASH) }, Account { user: b"nopass".to_vec(), old_hash: None }]
}

fn persona_40_euckr() -> Persona {
    Persona::Mysql40 { version: "4.0.27-standard-log", charset_name: "euc_kr", charset_id: 19 }
}

fn persona_40_latin1() -> Persona {
    Persona::Mysql40 { version: "4.0.18-max", charset_name: "latin1", charset_id: 8 }
}

fn params(port: u16, user: &str, password: Option<&str>, fields: &[(&str, &str)]) -> ConnectParams {
    ConnectParams {
        driver: "mysql".into(),
        host: "127.0.0.1".into(),
        port,
        database: None,
        user: Some(user.into()),
        password: password.map(Into::into),
        fields: fields.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect::<BTreeMap<_, _>>(),
        application_name: "bulkdb-test".into(),
        connect_timeout: Duration::from_secs(5),
        ssl: SslMode::Prefer,
    }
}

fn select(sql: &str) -> Query {
    Query { sql: sql.into(), output: StreamOutput::Arrow, max_batch_bytes: None }
}

async fn collect(stream: SourceStream) -> bulk_core::error::Result<Vec<RecordBatch>> {
    let mut s = stream.stream;
    let mut out = Vec::new();
    while let Some(b) = s.next().await {
        match b? {
            source::Batch::Arrow(rb) => out.push(rb),
            other => panic!("MySQL must produce Arrow batches, got {other:?}"),
        }
    }
    Ok(out)
}

fn strings(batches: &[RecordBatch], col: usize) -> Vec<Option<String>> {
    batches.iter().flat_map(|b| b.column(col).as_string::<i32>().iter().map(|s| s.map(str::to_string)).collect::<Vec<_>>()).collect()
}

fn ints(batches: &[RecordBatch], col: usize) -> Vec<Option<i64>> {
    batches.iter().flat_map(|b| b.column(col).as_primitive::<Int64Type>().iter().collect::<Vec<_>>()).collect()
}

/// Korean rows: id, name (CP949 text), memo (TEXT = BLOB without BINARY).
fn korean_rows_script(needle: &[u8]) -> Script {
    let columns = vec![
        FakeColumn::new("id", types::LONG).flags(flags::NOT_NULL | flags::PRI_KEY),
        FakeColumn::new("name", types::VAR_STRING).length(40),
        FakeColumn::new("memo", types::BLOB).flags(flags::BLOB).length(65535),
    ];
    let rows = vec![
        vec![v("1"), Some(euckr("한글")), Some(euckr("똠방각하 뷁"))],
        vec![v("2"), Some(euckr("가나다")), None],
        vec![v("3"), Some(Vec::new()), Some(euckr("둘째 줄\n\"인용\", 쉼표"))],
    ];
    Script::new().on_contains(needle, Reply::Rows { columns, rows: RowSource::Fixed(rows) })
}

#[tokio::test]
async fn greeting_40_is_detected_as_legacy() {
    let server = FakeServer::start(persona_40_euckr(), bulk_account(), Script::new()).await;
    let (_stream, greeting) = peek_greeting("127.0.0.1", server.port(), Duration::from_secs(5)).await.unwrap();
    assert!(greeting.is_pre41());
    assert_eq!(greeting.server_version, "4.0.27-standard-log");
    assert_eq!(greeting.charset_name_40(), Some("euc_kr"));
    assert_eq!(greeting.scramble.len(), 8);
    server.shutdown().await;
}

#[test]
fn old_password_hash_matches_manual() {
    assert_eq!(hash_password_323(b"mypass"), MYPASS_HASH);
}

#[test]
fn fixture_checks_client_scramble() {
    for msg in [&b"Hk/%@x2Q"[..], b"abcdefgh", b"!~!~!~!~", b"zzzzzzzz"] {
        assert!(check_scramble_323(&scramble_323(msg, b"mypass"), msg, MYPASS_HASH));
        assert!(!check_scramble_323(&scramble_323(msg, b"mypasx"), msg, MYPASS_HASH));
    }
}

#[tokio::test]
async fn connect_40_with_old_auth() {
    let server = FakeServer::start(persona_40_euckr(), bulk_account(), Script::new()).await;
    let port = server.port();

    // protocol auto (default) and legacy: right password.
    for fields in [&[][..], &[("protocol", "legacy")][..], &[("protocol", "4.0")][..]] {
        let db = source::open(&params(port, "bulk", Some("mypass"), fields)).await.unwrap();
        assert_eq!(db.driver_id(), "mysql");
        assert!(db.session_id().is_some());
    }

    // Wrong password -> AUTH with the server's message; unknown user too.
    let err = source::open(&params(port, "bulk", Some("wrong"), &[])).await.err().unwrap();
    assert!(matches!(&err, CoreError::Auth(m) if m.contains("Access denied") && m.contains("1045")), "{err:?}");
    let err = source::open(&params(port, "nobody", Some("x"), &[("protocol", "legacy")])).await.err().unwrap();
    assert_eq!(err.code(), "AUTH");
    let err = source::open(&params(port, "bulk", None, &[])).await.err().unwrap();
    assert_eq!(err.code(), "AUTH");

    // Empty password account.
    let db = MySqlLegacyDatabase::connect(&params(port, "nopass", None, &[])).await.unwrap();
    assert!(db.version().major == 4 && db.version().minor == 0);
    assert!(db.facts().legacy_protocol);

    // CONNECT_WITH_DB: the database arrives in the login packet.
    let mut p = params(port, "bulk", Some("mypass"), &[]);
    p.database = Some("shop".into());
    let db = MySqlLegacyDatabase::connect(&p).await.unwrap();
    let rows = db.meta_query("SELECT DATABASE()").await.unwrap();
    assert_eq!(rows.text(db.facts(), 0, 0).as_deref(), Some("shop"));
    db.ping().await.unwrap();

    // Unknown database at login -> CONNECT.
    p.database = Some("missing_db".into());
    let err = MySqlLegacyDatabase::connect(&p).await.err().unwrap();
    assert!(matches!(&err, CoreError::Connect(m) if m.contains("missing_db")), "{err:?}");

    // TLS required -> CONNECT before any login attempt.
    let mut p = params(port, "bulk", Some("mypass"), &[("protocol", "legacy")]);
    p.ssl = SslMode::Require;
    let err = source::open(&p).await.err().unwrap();
    assert!(matches!(&err, CoreError::Connect(m) if m.contains("TLS")), "{err:?}");

    // Invalid field values are rejected before connecting.
    assert_eq!(source::open(&params(port, "bulk", Some("mypass"), &[("protocol", "v3")])).await.err().unwrap().code(), "INVALID");

    let events = server.events();
    assert!(!events.iter().any(|e| matches!(e, Event::AbortedHandshake { .. })), "{events:?}");
    let failed = events.iter().filter(|e| matches!(e, Event::Auth { ok: false, .. })).count();
    assert_eq!(failed, 4, "{events:?}");
    server.shutdown().await;
}

#[tokio::test]
async fn auto_mode_keeps_the_socket_for_40() {
    let server = FakeServer::start(persona_40_euckr(), bulk_account(), Script::new()).await;
    let db = source::open(&params(server.port(), "bulk", Some("mypass"), &[("protocol", "auto")])).await.unwrap();
    drop(db);
    let events = server.events();
    assert!(!events.iter().any(|e| matches!(e, Event::AbortedHandshake { .. })), "{events:?}");
    assert_eq!(events.iter().filter(|e| matches!(e, Event::Connected { .. })).count(), 1, "one TCP connection: {events:?}");
    // The plan comes from the 4.0 server-wide charset.
    assert!(server.queries().iter().any(|q| q == b"SHOW VARIABLES LIKE 'character_set'"));
    server.shutdown().await;

    // A 4.1 server: the detection socket is answered with a credential-free login (never a bare
    // close), then the process remembers host:port as modern.
    let server = FakeServer::start(Persona::Mysql41OldAuth { version: "4.1.22-standard", charset_name: "euckr" }, bulk_account(), Script::new()).await;
    // Default sslmode=prefer: without a TLS backend the modern client goes to clear text directly
    // (no TLS attempt that the server would see as an aborted handshake).
    let p = params(server.port(), "bulk", Some("mypass"), &[("allow_old_password", "true")]);
    // The modern client's own result does not matter here (old_auth_switch_41_needs_permission
    // covers it); a failed modern connect after detection keeps the cache entry.
    let _ = source::open(&p).await;
    let _ = source::open(&p).await;
    let declined = |e: &[Event]| e.iter().filter(|e| matches!(e, Event::Auth { user, ok: false, .. } if user == DECLINE_USER.as_bytes())).count();
    assert!(server.wait_for(Duration::from_secs(2), |e| declined(e) >= 1).await, "{:?}", server.events());
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(declined(&server.events()), 1, "one detection per process: {:?}", server.events());
    assert_eq!(server.interrupted_handshakes(), 0, "{:?}", server.events());
    server.shutdown().await;
}

#[tokio::test]
async fn legacy_refuses_41_greeting() {
    let server = FakeServer::start(Persona::Mysql41OldAuth { version: "5.0.96-log", charset_name: "latin1" }, bulk_account(), Script::new()).await;
    for _ in 0..3 {
        let err = source::open(&params(server.port(), "bulk", Some("mypass"), &[("protocol", "legacy")])).await.err().unwrap();
        assert!(matches!(&err, CoreError::Connect(m) if m.contains("auto or modern") && m.contains("5.0.96")), "{err:?}");
    }
    // Each refusal answered the 4.1 greeting with a well-formed, credential-free login.
    let events = server.events();
    assert_eq!(events.iter().filter(|e| matches!(e, Event::Auth { user, ok: false, .. } if user == DECLINE_USER.as_bytes())).count(), 3, "{events:?}");
    assert!(!events.iter().any(|e| matches!(e, Event::Auth { user, .. } if user == b"bulk")), "the real account is never tried: {events:?}");
    assert_eq!(server.interrupted_handshakes(), 0, "{events:?}");
    server.shutdown().await;
}

/// The fixture's counter really sees a dropped socket (so the zero counts below mean something).
#[tokio::test]
async fn fixture_counts_interrupted_handshakes() {
    let server = FakeServer::start(persona_40_euckr(), bulk_account(), Script::new()).await;
    let (stream, greeting) = peek_greeting("127.0.0.1", server.port(), Duration::from_secs(5)).await.unwrap();
    assert!(greeting.is_pre41());
    drop(stream);
    assert!(server.wait_for(Duration::from_secs(2), |_| server.interrupted_handshakes() == 1).await, "{:?}", server.events());
    // `decline` on the same kind of socket is not counted, and the server saw a login attempt.
    let (stream, greeting) = peek_greeting("127.0.0.1", server.port(), Duration::from_secs(5)).await.unwrap();
    decline(stream, &greeting, Duration::from_secs(5)).await;
    assert!(server.events().iter().any(|e| matches!(e, Event::Auth { user, ok: false, .. } if user == DECLINE_USER.as_bytes())));
    assert_eq!(server.interrupted_handshakes(), 1);
    server.shutdown().await;
}

/// Wrong configurations must never leave the server an interrupted handshake: on MySQL 4.0
/// every one counts toward `max_connect_errors` (default 10) and finally blocks the client host
/// until a DBA runs FLUSH HOSTS. Checks that need no server fail before connecting; checks that
/// need the greeting answer it with a credential-free login first.
#[tokio::test]
async fn failing_configs_never_interrupt_a_handshake() {
    let server = FakeServer::start(persona_40_euckr(), bulk_account(), Script::new()).await;
    let port = server.port();
    let connections = |s: &FakeServer| s.events().iter().filter(|e| matches!(e, Event::Connected { .. })).count();

    // Detectable without a server: nothing is opened at all.
    let mut tls_legacy = params(port, "bulk", Some("mypass"), &[("protocol", "legacy")]);
    tls_legacy.ssl = SslMode::Require;
    // No client has TLS in this build, so protocol auto needs no greeting to refuse it either.
    let mut tls_auto = params(port, "bulk", Some("mypass"), &[]);
    tls_auto.ssl = SslMode::Require;
    let mut tls_modern = params(port, "bulk", Some("mypass"), &[("protocol", "modern")]);
    tls_modern.ssl = SslMode::VerifyFull;
    let before_socket: Vec<(ConnectParams, &str)> = vec![
        (tls_legacy, "CONNECT"),
        (tls_auto, "CONNECT"),
        (tls_modern, "CONNECT"),
        (params(port, "bulk", Some("mypass"), &[("allow_old_password", "maybe")]), "INVALID"),
        (params(port, "bulk", Some("mypass"), &[("encoding", "sjis")]), "INVALID"),
        (params(port, "관리자😀", Some("mypass"), &[("protocol", "legacy"), ("encoding", "euc-kr")]), "INVALID"),
        (params(port, "bulk", Some("pw😀"), &[("protocol", "legacy"), ("encoding", "latin1")]), "INVALID"),
    ];
    for (p, code) in &before_socket {
        let err = source::open(p).await.err().unwrap();
        assert_eq!(err.code(), *code, "{err:?}");
        assert!(!err.to_string().contains("pw😀"), "the password is never echoed: {err}");
    }
    assert_eq!(connections(&server), 0, "{:?}", server.events());

    // Needs the greeting (protocol auto, charset from the greeting): declined cleanly.
    let mut bad_db = params(port, "bulk", Some("mypass"), &[]);
    bad_db.database = Some("상점😀".into());
    let after_greeting: Vec<(ConnectParams, &str, &str)> = vec![
        (params(port, "관리자😀", Some("mypass"), &[]), "INVALID", "user name"),
        (params(port, "bulk", Some("pw😀"), &[]), "INVALID", "password"),
        (bad_db, "INVALID", "database name"),
    ];
    for (p, code, needle) in &after_greeting {
        let err = source::open(p).await.err().unwrap();
        assert_eq!(err.code(), *code, "{err:?}");
        assert!(err.to_string().contains(needle), "{err}");
        assert!(!err.to_string().contains("pw😀"), "the password is never echoed: {err}");
    }
    let events = server.events();
    assert_eq!(connections(&server), after_greeting.len(), "{events:?}");
    let declined = events.iter().filter(|e| matches!(e, Event::Auth { user, ok: false, .. } if user == DECLINE_USER.as_bytes())).count();
    assert_eq!(declined, after_greeting.len(), "every greeting got a login attempt: {events:?}");
    assert!(!events.iter().any(|e| matches!(e, Event::Auth { user, .. } if user == b"bulk")), "no credentials over a refused connection: {events:?}");
    assert_eq!(server.interrupted_handshakes(), 0, "{events:?}");
    server.shutdown().await;
}

/// The modern client (mysql_async) must not drop a socket after the greeting either. This build
/// has no TLS backend, so a TLS attempt always ends right after the greeting: `require` /
/// `verify-full` are refused before any socket on every protocol (also `modern` against 4.0),
/// and `prefer` connects in clear text without trying TLS first.
#[tokio::test]
async fn modern_client_never_interrupts_a_handshake() {
    let connections = |s: &FakeServer| s.events().iter().filter(|e| matches!(e, Event::Connected { .. })).count();
    let persona_50 = || Persona::Mysql41OldAuth { version: "5.0.96-log", charset_name: "latin1" };
    for persona in [persona_40_euckr(), persona_50()] {
        let server = FakeServer::start(persona.clone(), bulk_account(), Script::new()).await;
        for protocol in ["auto", "modern", "legacy"] {
            for ssl in [SslMode::Require, SslMode::VerifyFull] {
                for _ in 0..2 {
                    let mut p = params(server.port(), "bulk", Some("mypass"), &[("protocol", protocol), ("allow_old_password", "true")]);
                    p.ssl = ssl;
                    let err = source::open(&p).await.err().unwrap();
                    assert!(matches!(&err, CoreError::Connect(m) if m.contains("TLS")), "{persona:?} {protocol} {ssl:?}: {err:?}");
                }
            }
        }
        assert_eq!(connections(&server), 0, "refused before connecting: {persona:?}: {:?}", server.events());
        server.shutdown().await;
    }

    // sslmode=prefer (the default) against a 4.1+ server without CLIENT_SSL: every connect is one
    // clear-text login, right or wrong password, and nothing is counted as interrupted.
    let server = FakeServer::start(persona_50(), bulk_account(), Script::new()).await;
    for protocol in ["modern", "auto", "modern", "auto"] {
        let p = params(server.port(), "bulk", Some("mypass"), &[("protocol", protocol), ("allow_old_password", "true")]);
        assert_eq!(p.ssl, SslMode::Prefer);
        let db = source::open(&p).await;
        assert!(db.is_ok(), "{protocol}: {:?}", db.err());
    }
    let wrong = params(server.port(), "bulk", Some("wrong"), &[("protocol", "modern"), ("allow_old_password", "true")]);
    assert_eq!(source::open(&wrong).await.err().unwrap().code(), "AUTH");
    tokio::time::sleep(Duration::from_millis(100)).await;
    let events = server.events();
    // 5 connects + 1 detection socket (auto, first time only).
    assert_eq!(connections(&server), 6, "no TLS attempt before the clear-text login: {events:?}");
    assert_eq!(server.interrupted_handshakes(), 0, "{events:?}");
    server.shutdown().await;

    // protocol=modern against 4.0 without TLS: mysql_async answers the greeting with its own
    // login, which the server rejects; a login attempt, not an interrupted handshake.
    let server = FakeServer::start(persona_40_euckr(), bulk_account(), Script::new()).await;
    for ssl in [SslMode::Prefer, SslMode::Disable] {
        let mut p = params(server.port(), "bulk", Some("mypass"), &[("protocol", "modern")]);
        p.ssl = ssl;
        assert!(source::open(&p).await.is_err(), "{ssl:?}");
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    let events = server.events();
    assert_eq!(connections(&server), 2, "{events:?}");
    assert_eq!(server.interrupted_handshakes(), 0, "{events:?}");
    server.shutdown().await;
}

/// Errors sent instead of the greeting are connection problems with a hint, not query errors.
#[tokio::test]
async fn greeting_time_host_errors_map_to_connect() {
    let server = FakeServer::start(persona_40_euckr(), bulk_account(), Script::new()).await;
    server.refuse_connections(1129, "Host '127.0.0.1' is blocked because of many connection errors.  Unblock with 'mysqladmin flush-hosts'");
    for protocol in ["auto", "legacy", "modern"] {
        let mut p = params(server.port(), "bulk", Some("mypass"), &[("protocol", protocol)]);
        p.ssl = SslMode::Disable;
        let err = source::open(&p).await.err().unwrap();
        assert!(matches!(&err, CoreError::Connect(m) if m.contains("(1129)") && m.contains("FLUSH HOSTS")), "{protocol}: {err:?}");
    }
    server.refuse_connections(1130, "Host '127.0.0.1' is not allowed to connect to this MySQL server");
    for protocol in ["auto", "modern"] {
        let mut p = params(server.port(), "bulk", Some("mypass"), &[("protocol", protocol)]);
        p.ssl = SslMode::Disable;
        let err = source::open(&p).await.err().unwrap();
        assert!(matches!(&err, CoreError::Connect(m) if m.contains("(1130)") && m.contains("grant access")), "{protocol}: {err:?}");
    }
    server.shutdown().await;
}

#[tokio::test]
async fn old_auth_switch_41_needs_permission() {
    let personas =
        [Persona::Mysql41OldAuth { version: "4.1.22-standard", charset_name: "euckr" }, Persona::Mysql55PluginOldAuth { version: "5.5.62", charset_name: "euckr" }];
    for persona in personas {
        let server = FakeServer::start(persona.clone(), bulk_account(), Script::new()).await;
        let denied = source::open(&params(server.port(), "bulk", Some("mypass"), &[("protocol", "modern")])).await.err().unwrap();
        assert!(matches!(&denied, CoreError::Auth(m) if m.contains(OLD_PASSWORD_HINT)), "{persona:?}: {denied:?}");
        let db = source::open(&params(server.port(), "bulk", Some("mypass"), &[("protocol", "modern"), ("allow_old_password", "true")])).await;
        assert!(db.is_ok(), "{persona:?}: {:?}", db.err());
        assert!(server.events().iter().any(|e| matches!(e, Event::Auth { ok: true, .. })), "{persona:?}: {:?}", server.events());
        server.shutdown().await;
    }
}

#[tokio::test]
async fn euckr_text_decodes_to_utf8() {
    let server = FakeServer::start(persona_40_euckr(), bulk_account(), korean_rows_script(b"FROM kr")).await;
    let db = source::open(&params(server.port(), "bulk", Some("mypass"), &[])).await.unwrap();
    let stream = db.stream(&select("SELECT id, name, memo FROM kr"), &SessionOpts::default(), CancellationToken::new()).await.unwrap();
    let schema = stream.schema.clone().unwrap();
    assert_eq!(schema.fields().iter().map(|f| f.name().as_str()).collect::<Vec<_>>(), ["id", "name", "memo"]);
    assert_eq!(schema.field(0).data_type(), &DataType::Int64);
    assert_eq!(schema.field(1).data_type(), &DataType::Utf8);
    assert_eq!(schema.field(2).data_type(), &DataType::Utf8);
    let batches = collect(stream).await.unwrap();
    assert_eq!(ints(&batches, 0), [Some(1), Some(2), Some(3)]);
    assert_eq!(strings(&batches, 1), [Some("한글".into()), Some("가나다".into()), Some(String::new())]);
    assert_eq!(strings(&batches, 2), [Some("똠방각하 뷁".into()), None, Some("둘째 줄\n\"인용\", 쉼표".into())]);
    // The session went back to its slot: metadata works on the same TCP connection.
    let legacy = MySqlLegacyDatabase::connect(&params(server.port(), "bulk", Some("mypass"), &[])).await.unwrap();
    assert_eq!(legacy.facts().plan.sql_charset(), bulk_core::encoding::Charset::EucKr);
    server.shutdown().await;
}

#[tokio::test]
async fn latin1_server_with_euckr_setting() {
    // A Korean legacy database: CP949 bytes stored on a latin1 4.0 server.
    let server = FakeServer::start(persona_40_latin1(), bulk_account(), korean_rows_script(b"FROM kr")).await;
    let db = source::open(&params(server.port(), "bulk", Some("mypass"), &[("encoding", "euc-kr")])).await.unwrap();
    let batches = collect(db.stream(&select("SELECT id, name, memo FROM kr"), &SessionOpts::default(), CancellationToken::new()).await.unwrap()).await.unwrap();
    assert_eq!(strings(&batches, 1)[0].as_deref(), Some("한글"));
    assert_eq!(strings(&batches, 2)[0].as_deref(), Some("똠방각하 뷁"));

    // encoding=auto reads latin1 as windows-1252: the documented mojibake.
    let db = source::open(&params(server.port(), "bulk", Some("mypass"), &[])).await.unwrap();
    let batches = collect(db.stream(&select("SELECT id, name, memo FROM kr"), &SessionOpts::default(), CancellationToken::new()).await.unwrap()).await.unwrap();
    assert_eq!(strings(&batches, 1)[0].as_deref(), Some("ÇÑ±Û"));
    server.shutdown().await;
}

#[tokio::test]
async fn sql_is_sent_in_server_charset() {
    let server = FakeServer::start(persona_40_euckr(), bulk_account(), korean_rows_script(b"FROM kr")).await;
    let db = source::open(&params(server.port(), "bulk", Some("mypass"), &[])).await.unwrap();
    let sql = "SELECT id, name, memo FROM kr WHERE name = '한글'";
    collect(db.stream(&select(sql), &SessionOpts::default(), CancellationToken::new()).await.unwrap()).await.unwrap();
    let sent = server.queries().last().cloned().unwrap();
    let cp949 = euckr("'한글'");
    assert!(sent.windows(cp949.len()).any(|w| w == cp949.as_slice()), "SQL must arrive as CP949: {sent:x?}");
    assert!(!sent.windows("한글".len()).any(|w| w == "한글".as_bytes()), "no UTF-8 on the wire");
    assert!(sent.starts_with(b"/* bulkdb-test */ SELECT"), "tagged: {}", String::from_utf8_lossy(&sent));

    // An emoji cannot be represented in CP949: INVALID before anything is sent.
    let before = server.queries().len();
    let err = db.stream(&select("SELECT id FROM kr WHERE name = '😀'"), &SessionOpts::default(), CancellationToken::new()).await.err().unwrap();
    assert_eq!(err.code(), "INVALID", "{err:?}");
    // Position in the user's SQL, not shifted by the `/* bulkdb-test */ ` tag (column 51).
    assert!(err.to_string().contains("line 1, column 33"), "{err}");
    let err = db.stream(&select("SELECT id\nFROM kr\nWHERE name = '😀'"), &SessionOpts::default(), CancellationToken::new()).await.err().unwrap();
    assert!(err.to_string().contains("line 3, column 15"), "{err}");
    // EXPLAIN reports the same position, not shifted by its `EXPLAIN ` prefix.
    let err = db.explain("SELECT id FROM kr WHERE name = '😀'", false).await.err().unwrap();
    assert!(matches!(&err, CoreError::Invalid(m) if m.contains("line 1, column 33")), "{err:?}");
    assert_eq!(server.queries().len(), before);
    // The connection tells preflight which charset SQL must fit (SQL_ENCODING blocker).
    assert_eq!(db.sql_charset(), Some(bulk_core::encoding::Charset::EucKr));
    // The session is still usable afterwards.
    collect(db.stream(&select("SELECT id, name, memo FROM kr"), &SessionOpts::default(), CancellationToken::new()).await.unwrap()).await.unwrap();
    server.shutdown().await;
}

#[tokio::test]
async fn types_map_like_modern() {
    let columns = vec![
        FakeColumn::new("i", types::LONG),
        FakeColumn::new("u", types::LONGLONG).flags(flags::UNSIGNED).length(20),
        FakeColumn::new("d", types::DECIMAL).decimals(2).length(12),
        FakeColumn::new("dt", types::DATE).length(10),
        FakeColumn::new("ts", types::DATETIME).length(19),
        FakeColumn::new("ts40", types::TIMESTAMP).flags(flags::TIMESTAMP | flags::UNSIGNED | flags::ZEROFILL | flags::BINARY).length(14),
        FakeColumn::new("tm", types::TIME).length(8),
        FakeColumn::new("bin", types::BLOB).flags(flags::BLOB | flags::BINARY).length(65535),
        FakeColumn::new("txt", types::BLOB).flags(flags::BLOB).length(65535),
        FakeColumn::new("cb", types::STRING).flags(flags::BINARY).length(10),
        FakeColumn::new("f", types::DOUBLE).decimals(31).length(22),
        FakeColumn::new("y", types::YEAR).flags(flags::UNSIGNED | flags::ZEROFILL).length(4),
    ];
    let rows = vec![
        vec![
            v("-7"),
            v("18446744073709551615"),
            v("-12345.67"),
            v("2003-12-31"),
            v("2003-12-31 23:59:59"),
            v("20031231235959"),
            v("-838:59:59"),
            Some(vec![0, 159, 146, 150, 255]),
            Some(euckr("메모")),
            Some(euckr("가")),
            v("3.5"),
            v("2004"),
        ],
        vec![v("0"), v("0"), v("0.50"), v("0000-00-00"), v("0000-00-00 00:00:00"), v("00000000000000"), v("00:00:00"), Some(vec![]), None, None, None, None],
    ];
    let script = Script::new().on_contains(b"FROM typed", Reply::Rows { columns, rows: RowSource::Fixed(rows) });
    let server = FakeServer::start(persona_40_euckr(), bulk_account(), script).await;
    let db = source::open(&params(server.port(), "bulk", Some("mypass"), &[])).await.unwrap();
    let stream = db.stream(&select("SELECT * FROM typed"), &SessionOpts::default(), CancellationToken::new()).await.unwrap();
    let schema = stream.schema.clone().unwrap();
    let types_of: Vec<DataType> = schema.fields().iter().map(|f| f.data_type().clone()).collect();
    assert_eq!(
        types_of,
        [
            DataType::Int64,
            DataType::Utf8,
            DataType::Decimal128(10, 2),
            DataType::Date32,
            DataType::Timestamp(arrow_schema::TimeUnit::Microsecond, None),
            DataType::Timestamp(arrow_schema::TimeUnit::Microsecond, None),
            DataType::Utf8,
            DataType::Binary,
            DataType::Utf8,
            DataType::Utf8,
            DataType::Float64,
            DataType::Int64,
        ]
    );
    let batches = collect(stream).await.unwrap();
    assert_eq!(batches.len(), 1);
    let b = &batches[0];
    assert_eq!(ints(&batches, 0), [Some(-7), Some(0)]);
    assert_eq!(strings(&batches, 1)[0].as_deref(), Some("18446744073709551615"));
    let d = b.column(2).as_primitive::<Decimal128Type>();
    assert_eq!((d.value(0), d.value(1)), (-1_234_567, 50));
    let dt = b.column(3).as_primitive::<Date32Type>();
    assert_eq!(dt.value(0), 12_417); // 2003-12-31
    assert!(dt.is_null(1), "zero date -> NULL");
    let ts = b.column(4).as_primitive::<TimestampMicrosecondType>();
    assert_eq!(ts.value(0), 1_072_915_199_000_000);
    assert!(ts.is_null(1), "zero datetime -> NULL");
    let ts40 = b.column(5).as_primitive::<TimestampMicrosecondType>();
    assert_eq!(ts40.value(0), 1_072_915_199_000_000, "4.0 compact TIMESTAMP");
    assert!(ts40.is_null(1), "zero TIMESTAMP -> NULL");
    assert_eq!(strings(&batches, 6), [Some("-838:59:59".into()), Some("00:00:00".into())]);
    let bin = b.column(7).as_binary::<i32>();
    assert_eq!(bin.value(0), &[0, 159, 146, 150, 255]);
    assert_eq!(bin.value(1), b"");
    assert_eq!(strings(&batches, 8), [Some("메모".into()), None]);
    assert_eq!(strings(&batches, 9), [Some("가".into()), None], "CHAR BINARY is text in 4.0");
    let f = b.column(10).as_primitive::<Float64Type>();
    assert_eq!(f.value(0), 3.5);
    assert!(f.is_null(1));
    assert_eq!(ints(&batches, 11), [Some(2004), None]);
    server.shutdown().await;
}

#[tokio::test]
async fn continuation_packets() {
    const BIG: usize = 20 * 1024 * 1024;
    // id "1" (2 bytes) + lenenc prefix (0xFD + 3 bytes) + cell = exactly one full packet,
    // followed by an empty packet.
    const EXACT: usize = 0xFF_FFFF - 6;
    let gen = Arc::new(|i: u64| -> Vec<Option<Vec<u8>>> {
        let len = if i == 0 { BIG } else { EXACT };
        let mut cell: Vec<u8> = (0..len).map(|j| (j % 251) as u8).collect();
        cell[len - 1] = 0xAB;
        vec![Some(i.to_string().into_bytes()), Some(cell)]
    });
    let columns = vec![FakeColumn::new("id", types::LONG), FakeColumn::new("payload", types::BLOB).flags(flags::BLOB | flags::BINARY).length(0xFF_FFFF)];
    let script = Script::new().on_contains(b"FROM big", Reply::Rows { columns, rows: RowSource::Generated { count: 2, gen } });
    let server = FakeServer::start(persona_40_euckr(), bulk_account(), script).await;
    let db = source::open(&params(server.port(), "bulk", Some("mypass"), &[])).await.unwrap();
    let batches = collect(db.stream(&select("SELECT id, payload FROM big"), &SessionOpts::default(), CancellationToken::new()).await.unwrap()).await.unwrap();
    let mut lens = Vec::new();
    for b in &batches {
        let col = b.column(1).as_binary::<i32>();
        for i in 0..col.len() {
            let cell = col.value(i);
            assert_eq!(cell[1], 1);
            assert_eq!(cell[250], 250);
            assert_eq!(*cell.last().unwrap(), 0xAB);
            lens.push(cell.len());
        }
    }
    assert_eq!(lens, [BIG, EXACT]);
    assert_eq!(ints(&batches, 0), [Some(0), Some(1)]);
    server.shutdown().await;
}

#[tokio::test]
async fn batches_are_bounded() {
    const ROWS: u64 = 300_000;
    let gen = Arc::new(|i: u64| -> Vec<Option<Vec<u8>>> { vec![Some(i.to_string().into_bytes()), Some(format!("row-{i:08}").into_bytes())] });
    let columns = vec![FakeColumn::new("id", types::LONGLONG), FakeColumn::new("label", types::VAR_STRING).length(20)];
    let script = Script::new().on_contains(b"FROM many", Reply::Rows { columns, rows: RowSource::Generated { count: ROWS, gen } });
    let server = FakeServer::start(persona_40_euckr(), bulk_account(), script).await;
    let db = source::open(&params(server.port(), "bulk", Some("mypass"), &[])).await.unwrap();

    let batches = collect(db.stream(&select("SELECT id, label FROM many"), &SessionOpts::default(), CancellationToken::new()).await.unwrap()).await.unwrap();
    let total: usize = batches.iter().map(|b| b.num_rows()).sum();
    assert_eq!(total as u64, ROWS);
    assert!(batches.iter().all(|b| b.num_rows() <= bulk_core::BATCH_ROWS), "{:?}", batches.iter().map(|b| b.num_rows()).collect::<Vec<_>>());
    assert!(batches.iter().all(|b| b.get_array_memory_size() <= 2 * bulk_core::BATCH_BYTES));
    assert_eq!(batches[0].num_rows(), bulk_core::BATCH_ROWS);
    let ids = ints(&batches, 0);
    assert_eq!((ids[0], ids[ROWS as usize - 1]), (Some(0), Some(ROWS as i64 - 1)));

    // fetch_batch_rows is honoured, and the session is reused (still one TCP connection).
    let opts = SessionOpts { fetch_batch_rows: Some(10_000), ..SessionOpts::default() };
    let batches = collect(db.stream(&select("SELECT id, label FROM many"), &opts, CancellationToken::new()).await.unwrap()).await.unwrap();
    assert_eq!(batches.len(), 30);
    assert!(batches.iter().all(|b| b.num_rows() == 10_000));
    assert_eq!(server.events().iter().filter(|e| matches!(e, Event::Connected { .. })).count(), 1);
    server.shutdown().await;
}

#[tokio::test]
async fn server_error_mid_query_keeps_the_session() {
    let msg = euckr("테이블 'shop.nope' 는 존재하지 않습니다");
    let script = korean_rows_script(b"FROM kr").on_contains(b"FROM nope", Reply::Err { code: 1146, message: msg });
    let server = FakeServer::start(persona_40_euckr(), bulk_account(), script).await;
    let db = source::open(&params(server.port(), "bulk", Some("mypass"), &[])).await.unwrap();
    let err = db.stream(&select("SELECT * FROM nope"), &SessionOpts::default(), CancellationToken::new()).await.err().unwrap();
    assert!(matches!(&err, CoreError::Query { code: Some(c), msg } if c == "1146" && msg.contains("존재하지 않습니다")), "{err:?}");
    // `;` outside literals: refused before sending (4.0 runs no batches anyway).
    let before = server.queries().len();
    let err = db.stream(&select("SELECT 1; DROP TABLE kr"), &SessionOpts::default(), CancellationToken::new()).await.err().unwrap();
    assert_eq!(err.code(), "INVALID");
    assert_eq!(server.queries().len(), before);
    // The same session serves the next extraction.
    let batches = collect(db.stream(&select("SELECT id, name, memo FROM kr"), &SessionOpts::default(), CancellationToken::new()).await.unwrap()).await.unwrap();
    assert_eq!(batches[0].num_rows(), 3);
    assert_eq!(server.events().iter().filter(|e| matches!(e, Event::Connected { .. })).count(), 1);
    server.shutdown().await;
}

#[tokio::test]
async fn cancel_kills_the_session_within_2s() {
    let gen = Arc::new(|i: u64| -> Vec<Option<Vec<u8>>> { vec![Some(i.to_string().into_bytes()), Some(vec![b'x'; 200])] });
    let columns = vec![FakeColumn::new("id", types::LONGLONG), FakeColumn::new("pad", types::VAR_STRING).length(200)];
    let script = Script::new().on_contains(b"FROM endless", Reply::Rows { columns, rows: RowSource::Endless { gen } });
    let server = FakeServer::start(persona_40_euckr(), bulk_account(), script).await;
    let db = MySqlLegacyDatabase::connect(&params(server.port(), "bulk", Some("mypass"), &[])).await.unwrap();
    let victim: u32 = db.session_id().unwrap().parse().unwrap();

    let cancel = CancellationToken::new();
    let stream = db.stream(&select("SELECT id, pad FROM endless"), &SessionOpts::default(), cancel.clone()).await.unwrap();
    let mut s = stream.stream;
    let first = s.next().await.unwrap().unwrap();
    assert!(first.rows() > 0);
    // Busy while streaming.
    assert_eq!(db.meta_query("SELECT VERSION()").await.err().unwrap().code(), "CONNECT");

    let started = Instant::now();
    cancel.cancel();
    let mut outcome = None;
    while let Some(item) = tokio::time::timeout(Duration::from_secs(3), s.next()).await.expect("stream must end after cancel") {
        if let Err(e) = item {
            outcome = Some(e);
            break;
        }
    }
    let elapsed = started.elapsed();
    assert!(matches!(outcome, Some(CoreError::Cancelled)), "{outcome:?}");
    assert!(elapsed < Duration::from_secs(2), "cancel took {elapsed:?}");
    assert!(
        server.wait_for(Duration::from_secs(5), |e| e.contains(&Event::Killed { target: victim })).await,
        "KILL {victim} from a side connection: {:?}",
        server.events()
    );
    assert!(server.events().contains(&Event::Disconnected { id: victim }));

    // The next metadata call reconnects with a new thread id.
    let rows = db.meta_query("SELECT VERSION()").await.unwrap();
    assert_eq!(rows.text(db.facts(), 0, 0).as_deref(), Some("4.0.27-standard-log"));
    let now: u32 = db.session_id().unwrap().parse().unwrap();
    assert_ne!(now, victim);
    let kill_sql = format!("KILL {victim}").into_bytes();
    assert!(server.queries().contains(&kill_sql), "4.x has no KILL QUERY");
    server.shutdown().await;
}

/// The consumer goes away (sink failure, job torn down) while the server is slow to send the
/// next row: the reader task must not wait for that row. It drops the socket and kills the
/// server thread like a cancel, and the session slot is free again at once.
#[tokio::test]
async fn dropped_consumer_stops_a_slow_scan() {
    let gen: support::fake_mysql::RowGen = Arc::new(|i: u64| -> Vec<Option<Vec<u8>>> { vec![Some(i.to_string().into_bytes())] });
    let columns = vec![FakeColumn::new("id", types::LONGLONG)];
    // 4 rows, then a 60 s pause before the next one (a slow scan between rows).
    let script = Script::new()
        .on_contains(b"FROM slow", Reply::Rows { columns: columns.clone(), rows: RowSource::Stalled { rows: 4, gen: gen.clone(), millis: 60_000 } })
        // A sort that keeps the server silent for 60 s before the first row.
        .on_contains(b"FROM sorted", Reply::Stall { millis: 60_000, then: Box::new(Reply::Rows { columns, rows: RowSource::Fixed(vec![vec![v("1")]]) }) });
    let server = FakeServer::start(persona_40_euckr(), bulk_account(), script).await;
    let db = MySqlLegacyDatabase::connect(&params(server.port(), "bulk", Some("mypass"), &[])).await.unwrap();

    // 1) Mid-result: the stream is dropped after the first batch.
    let victim: u32 = db.session_id().unwrap().parse().unwrap();
    let opts = SessionOpts { fetch_batch_rows: Some(2), ..Default::default() };
    let stream = db.stream(&select("SELECT id FROM slow"), &opts, CancellationToken::new()).await.unwrap();
    let mut s = stream.stream;
    assert_eq!(s.next().await.unwrap().unwrap().rows(), 2);
    drop(s);
    assert!(
        server.wait_for(Duration::from_secs(2), |e| e.contains(&Event::Killed { target: victim }) && e.contains(&Event::Disconnected { id: victim })).await,
        "KILL {victim} soon after the consumer left: {:?}",
        server.events()
    );
    // The reader task released the session: the next call reconnects instead of "busy".
    let rows = tokio::time::timeout(Duration::from_secs(2), db.meta_query("SELECT VERSION()")).await.expect("not stuck").unwrap();
    assert_eq!(rows.text(db.facts(), 0, 0).as_deref(), Some("4.0.27-standard-log"));

    // 2) Before the first row: the caller gives up on `stream()` itself.
    let victim: u32 = db.session_id().unwrap().parse().unwrap();
    let started = tokio::time::timeout(Duration::from_millis(300), db.stream(&select("SELECT id FROM sorted"), &SessionOpts::default(), CancellationToken::new())).await;
    assert!(started.is_err(), "the server is still sorting");
    assert!(
        server.wait_for(Duration::from_secs(2), |e| e.contains(&Event::Killed { target: victim }) && e.contains(&Event::Disconnected { id: victim })).await,
        "KILL {victim} after stream() was dropped: {:?}",
        server.events()
    );
    let rows = tokio::time::timeout(Duration::from_secs(2), db.meta_query("SELECT VERSION()")).await.expect("not stuck").unwrap();
    assert_eq!(rows.rows.len(), 1);
    server.shutdown().await;
}

#[tokio::test]
async fn cancellable_meta_query_kills_on_cancel() {
    // A COUNT(*) that takes 60 s before the server sends anything.
    let count = Reply::Rows { columns: vec![FakeColumn::new("COUNT(*)", types::LONGLONG)], rows: RowSource::Fixed(vec![vec![v("3")]]) };
    let script = Script::new().on_contains(b"COUNT(*)", Reply::Stall { millis: 60_000, then: Box::new(count) });
    let server = FakeServer::start(persona_40_euckr(), bulk_account(), script).await;
    let db = MySqlLegacyDatabase::connect(&params(server.port(), "bulk", Some("mypass"), &[])).await.unwrap();
    let victim: u32 = db.session_id().unwrap().parse().unwrap();
    let cancel = CancellationToken::new();
    let c2 = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(200)).await;
        c2.cancel();
    });
    let started = Instant::now();
    let err = db.meta_query_cancellable("SELECT COUNT(*) FROM big", cancel).await.err().unwrap();
    assert!(matches!(err, CoreError::Cancelled), "{err:?}");
    assert!(started.elapsed() < Duration::from_secs(2));
    assert!(server.wait_for(Duration::from_secs(5), |e| e.contains(&Event::Killed { target: victim })).await, "{:?}", server.events());
    db.ping().await.unwrap();
    server.shutdown().await;
}

/// `SHOW ...` answers exactly as a 4.0.27 server prints them.
fn catalog_script() -> Script {
    let text = |n: &str| FakeColumn::new(n, types::VAR_STRING).length(64);
    let num = |n: &str| FakeColumn::new(n, types::LONGLONG).length(21);
    let status_cols = vec![
        text("Name"),
        text("Type"),
        text("Row_format"),
        num("Rows"),
        num("Avg_row_length"),
        num("Data_length"),
        num("Max_data_length"),
        num("Index_length"),
        num("Data_free"),
        num("Auto_increment"),
        FakeColumn::new("Create_time", types::DATETIME).length(19),
        FakeColumn::new("Update_time", types::DATETIME).length(19),
        FakeColumn::new("Check_time", types::DATETIME).length(19),
        text("Create_options"),
        text("Comment"),
    ];
    let status_rows = vec![
        vec![
            v("kr"),
            v("MyISAM"),
            v("Dynamic"),
            v("3"),
            v("40"),
            v("120"),
            v("4294967295"),
            v("2048"),
            v("0"),
            v("4"),
            v("2005-01-01 00:00:00"),
            v("2005-01-02 00:00:00"),
            None,
            v(""),
            Some(euckr("고객 목록")),
        ],
        vec![v("orders"), v("InnoDB"), v("Fixed"), v("1000"), v("100"), v("100000"), None, v("16384"), v("0"), None, None, None, None, v(""), v("InnoDB free: 4096 kB")],
    ];
    let column_cols = vec![text("Field"), text("Type"), text("Null"), text("Key"), text("Default"), text("Extra"), text("Privileges")];
    let column_rows = vec![
        vec![v("id"), v("int(11)"), v(""), v("PRI"), None, v("auto_increment"), v("select,insert,update,references")],
        vec![v("name"), v("varchar(40)"), v("YES"), v(""), None, v(""), v("select,insert,update,references")],
        vec![v("memo"), v("text"), v("YES"), v(""), None, v(""), v("select,insert,update,references")],
    ];
    let index_cols = vec![
        text("Table"),
        num("Non_unique"),
        text("Key_name"),
        num("Seq_in_index"),
        text("Column_name"),
        text("Collation"),
        num("Cardinality"),
        num("Sub_part"),
        text("Packed"),
        text("Null"),
        text("Index_type"),
        text("Comment"),
    ];
    let index_rows = vec![vec![v("kr"), v("0"), v("PRIMARY"), v("1"), v("id"), v("A"), v("3"), None, None, v(""), v("BTREE"), v("")]];
    let ddl = format!(
        "CREATE TABLE `kr` (\n  `id` int(11) NOT NULL auto_increment,\n  `name` varchar(40) default NULL,\n  PRIMARY KEY  (`id`)\n) TYPE=MyISAM COMMENT='{}'",
        "고객 목록"
    );
    Script::new()
        .on_contains(b"SHOW DATABASES", Reply::Rows { columns: vec![text("Database")], rows: RowSource::Fixed(vec![vec![v("mysql")], vec![v("shop")], vec![v("test")]]) })
        .on_contains(b"SHOW TABLE STATUS", Reply::Rows { columns: status_cols, rows: RowSource::Fixed(status_rows) })
        .on_contains(b"COLUMNS FROM", Reply::Rows { columns: column_cols, rows: RowSource::Fixed(column_rows) })
        .on_contains(b"SHOW INDEX", Reply::Rows { columns: index_cols, rows: RowSource::Fixed(index_rows) })
        .on_contains(
            b"SHOW CREATE TABLE",
            Reply::Rows { columns: vec![text("Table"), text("Create Table")], rows: RowSource::Fixed(vec![vec![v("kr"), Some(euckr(&ddl))]]) },
        )
        .on_contains(b"COUNT(*)", Reply::Rows { columns: vec![num("COUNT(*)")], rows: RowSource::Fixed(vec![vec![v("3")]]) })
}

#[tokio::test]
async fn catalog_on_40() {
    let server = FakeServer::start(persona_40_euckr(), bulk_account(), catalog_script()).await;
    let mut p = params(server.port(), "bulk", Some("mypass"), &[]);
    p.database = Some("shop".into());
    let db = source::open(&p).await.unwrap();
    let dbs = db.databases().await.unwrap();
    assert!(dbs.iter().any(|d| d.name == "shop"), "{dbs:?}");
    let tables = db.tables("shop", "shop").await.unwrap();
    let kr = tables.iter().find(|t| t.name == "kr").unwrap_or_else(|| panic!("{tables:?}"));
    assert_eq!(kr.comment.as_deref(), Some("고객 목록"));
    let table = TableRef { database: Some("shop".into()), schema: Some("shop".into()), name: "kr".into() };
    let cols = db.columns(&table).await.unwrap();
    assert_eq!(cols.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["id", "name", "memo"]);
    let idx = db.indexes(&table).await.unwrap();
    assert!(idx.iter().any(|i| i.name == "PRIMARY"), "{idx:?}");
    let ddl = db.ddl(&table).await.unwrap();
    assert!(ddl.contains("CREATE TABLE `kr`") && ddl.contains("고객 목록"), "{ddl}");
    assert_eq!(db.exact_count(&table, CancellationToken::new()).await.unwrap(), 3);
    // Every catalog query went to the 4.0 server through the SHOW family.
    assert!(!server.queries().iter().any(|q| String::from_utf8_lossy(q).to_ascii_lowercase().contains("information_schema")));
    server.shutdown().await;
}

#[tokio::test]
async fn probe_and_session_on_40() {
    let server = FakeServer::start(persona_40_euckr(), bulk_account(), Script::new()).await;
    let db = source::open(&params(server.port(), "bulk", Some("mypass"), &[])).await.unwrap();
    let info = db.probe().await.unwrap();
    assert_eq!(info.driver, "mysql");
    assert_eq!(info.version, "4.0.27-standard-log");
    assert_eq!(info.settings.get("protocol").map(String::as_str), Some("legacy (pre-4.1)"));
    assert!(info.settings.get("text_plan").is_some_and(|p| p.contains("EUC-KR")), "{:?}", info.settings);
    let opts = SessionOpts { statement_timeout_ms: Some(60_000), ..SessionOpts::default() };
    db.apply_session(&opts).await.unwrap();
    let sent: Vec<String> = server.queries().iter().map(|q| String::from_utf8_lossy(q).to_ascii_uppercase()).collect();
    assert!(!sent.iter().any(|q| q.contains("READ ONLY") || q.contains("MAX_EXECUTION_TIME") || q.contains("INFORMATION_SCHEMA")), "{sent:?}");
    // Still one session: probe and SET ran on the connection the dispatcher opened.
    assert_eq!(server.events().iter().filter(|e| matches!(e, Event::Connected { .. })).count(), 1);
    server.shutdown().await;
}

#[tokio::test]
async fn explain_on_40() {
    let text = |n: &str| FakeColumn::new(n, types::VAR_STRING).length(64);
    let columns = vec![
        text("table"),
        text("type"),
        text("possible_keys"),
        text("key"),
        FakeColumn::new("key_len", types::LONGLONG),
        text("ref"),
        FakeColumn::new("rows", types::LONGLONG),
        text("Extra"),
    ];
    let rows = vec![
        vec![v("o"), v("ALL"), None, None, None, None, v("1000"), v("Using where")],
        vec![v("k"), v("eq_ref"), v("PRIMARY"), v("PRIMARY"), v("4"), v("o.kr_id"), v("1"), v("")],
    ];
    let script = Script::new().on_contains(b"EXPLAIN", Reply::Rows { columns, rows: RowSource::Fixed(rows) });
    let server = FakeServer::start(persona_40_euckr(), bulk_account(), script).await;
    let db = source::open(&params(server.port(), "bulk", Some("mypass"), &[])).await.unwrap();
    let plan = db.explain("SELECT * FROM orders o JOIN kr k ON k.id = o.kr_id", false).await.unwrap().expect("a plan");
    assert_eq!(plan.estimated_rows, Some(1000));
    assert!(server.queries().iter().any(|q| q.starts_with(b"EXPLAIN SELECT")), "tabular EXPLAIN without FORMAT=JSON");
    assert_eq!(db.explain("SELECT 1", true).await.err().unwrap().code(), "INVALID", "no EXPLAIN ANALYZE on 4.0");
    server.shutdown().await;
}

fn csv_opts(encoding: Option<CsvEncoding>) -> OutputOpts {
    OutputOpts {
        format: OutputFormat::Csv,
        csv: Some(CsvOpts { header: true, delimiter: ",".into(), null_as: String::new(), bom: false, crlf: false, encoding }),
        ..OutputOpts::default()
    }
}

#[tokio::test]
async fn export_euckr_server_to_utf8_and_euckr_csv() {
    let columns = vec![
        FakeColumn::new("id", types::LONG),
        FakeColumn::new(euckr("이름"), types::VAR_STRING).length(40),
        FakeColumn::new("memo", types::BLOB).flags(flags::BLOB).length(65535),
    ];
    let rows = vec![vec![v("1"), Some(euckr("한글")), Some(euckr("똠방각하, \"인용\""))], vec![v("2"), Some(euckr("가나다")), None]];
    let script = Script::new().on_contains(b"FROM kr", Reply::Rows { columns, rows: RowSource::Fixed(rows) });
    let server = FakeServer::start(persona_40_euckr(), bulk_account(), script).await;
    let dir = tempfile::tempdir().unwrap();
    let expected_utf8 = "id,이름,memo\n1,한글,\"똠방각하, \"\"인용\"\"\"\n2,가나다,\n";

    for (stem, encoding, expected) in [("utf8", None, expected_utf8.as_bytes().to_vec()), ("euckr", Some(CsvEncoding::EucKr), euckr(expected_utf8))] {
        let db = source::open(&params(server.port(), "bulk", Some("mypass"), &[])).await.unwrap();
        let cancel = CancellationToken::new();
        let stream = db.stream(&select("SELECT id, name, memo FROM kr"), &SessionOpts::default(), cancel.clone()).await.unwrap();
        let target = SinkTarget { dir: dir.path().to_path_buf(), stem: stem.into(), opts: csv_opts(encoding) };
        let sink = sink::build(&target).unwrap();
        let report = run_pipeline(stream, sink, Arc::new(Progress::new()), cancel).await.unwrap();
        assert_eq!(report.rows, 2);
        assert_eq!(report.unmappable_chars, 0);
        let written = std::fs::read(&report.files[0]).unwrap();
        assert_eq!(written, expected, "{stem}: {}", String::from_utf8_lossy(&written));
    }
    server.shutdown().await;
}
