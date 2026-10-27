//! Generic ODBC driver tests that need only the Windows driver manager (`odbc32.dll`): no DSN,
//! no database server. They cover the driver / data-source listings, the connection string
//! builder and the error path of a refused login. Windows only: the `odbc` module is compiled
//! only where build.rs finds a driver manager (`has_odbc`), which is every Windows build.
#![cfg(all(windows, has_odbc))]

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use bulk_core::source::odbc::{build_connection_string, connection_string_keys, data_sources, escape_odbc_value, installed_drivers, OdbcDatabase};
use bulk_core::source::{ConnectParams, SslMode};
use bulk_core::CoreError;

fn params(fields: &[(&str, &str)], host: &str, port: u16, user: Option<&str>, password: Option<&str>) -> ConnectParams {
    ConnectParams {
        driver: "odbc".into(),
        host: host.into(),
        port,
        database: Some("sales".into()),
        user: user.map(str::to_string),
        password: password.map(str::to_string),
        fields: fields.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect::<BTreeMap<_, _>>(),
        application_name: "bulkdb-test".into(),
        connect_timeout: Duration::from_secs(5),
        ssl: SslMode::Prefer,
    }
}

/// The SQL Server driver to use for the connect test: the newest "ODBC Driver NN for SQL Server"
/// when one is installed, else the "SQL Server" driver that ships with Windows itself.
fn sql_server_driver(drivers: &[String]) -> Option<String> {
    let mut modern: Vec<&String> = drivers.iter().filter(|d| d.starts_with("ODBC Driver") && d.ends_with("for SQL Server")).collect();
    modern.sort();
    modern.last().map(|d| (*d).clone()).or_else(|| drivers.iter().find(|d| d.as_str() == "SQL Server").cloned())
}

// ---------------------------------------------------------------------------------------------
// Driver manager listings
// ---------------------------------------------------------------------------------------------

#[test]
fn installed_drivers_lists_the_stock_windows_drivers() {
    let drivers = installed_drivers().expect("SQLDrivers must succeed on Windows");
    println!("installed ODBC drivers ({}):", drivers.len());
    for d in &drivers {
        println!("  {d}");
    }
    assert!(!drivers.is_empty(), "a Windows installation always registers at least the 'SQL Server' driver");
    assert!(
        drivers.iter().any(|d| d.contains("SQL Server") || d.contains("Microsoft Access Driver")),
        "expected a stock Windows driver ('SQL Server' or 'Microsoft Access Driver') in {drivers:?}"
    );
    assert!(drivers.iter().all(|d| !d.trim().is_empty()), "driver descriptions must not be blank");
}

#[test]
fn installed_drivers_is_stable_across_calls() {
    // The environment is process-wide: a second listing (possibly from another thread) must
    // return the same drivers and must not re-initialise the driver manager.
    let first = installed_drivers().unwrap();
    let second = std::thread::spawn(installed_drivers).join().unwrap().unwrap();
    assert_eq!(first, second);
}

#[test]
fn data_sources_succeeds_without_any_dsn() {
    let sources = data_sources().expect("SQLDataSources must succeed even when no DSN is configured");
    println!("configured ODBC data sources ({}):", sources.len());
    for (name, driver) in &sources {
        println!("  {name} -> {driver}");
        assert!(!name.is_empty(), "a DSN always has a name");
    }
}

// ---------------------------------------------------------------------------------------------
// Connection string builder
// ---------------------------------------------------------------------------------------------

#[test]
fn escaping_adds_braces_only_when_needed() {
    assert_eq!(escape_odbc_value("plain"), "plain");
    assert_eq!(escape_odbc_value("has space"), "has space");
    assert_eq!(escape_odbc_value("semi;colon"), "{semi;colon}");
    assert_eq!(escape_odbc_value("eq=ual"), "{eq=ual}");
    assert_eq!(escape_odbc_value("close}brace"), "{close}}brace}");
    assert_eq!(escape_odbc_value("{braced}"), "{{braced}}}");
    assert_eq!(escape_odbc_value(""), "{}");
}

#[test]
fn builds_driver_server_database_and_credentials() {
    let p = params(&[("driver", "ODBC Driver 18 for SQL Server")], "db.example.com", 1433, Some("sa"), Some("p;w}d"));
    assert_eq!(build_connection_string(&p).unwrap(), "Driver={ODBC Driver 18 for SQL Server};Server=db.example.com,1433;Database=sales;UID=sa;PWD={p;w}}d};");
    // The driver name is always braced, even when it needs no escaping.
    let p = params(&[("driver", "SQL Server")], "localhost", 0, None, None);
    assert_eq!(build_connection_string(&p).unwrap(), "Driver={SQL Server};Server=localhost;Database=sales;");
}

#[test]
fn dsn_only_strings_carry_no_server() {
    let p = params(&[("dsn", "Warehouse")], "ignored.example.com", 1433, Some("u"), Some("p"));
    let s = build_connection_string(&p).unwrap();
    assert_eq!(s, "DSN=Warehouse;Database=sales;UID=u;PWD=p;");
    assert!(!s.contains("Server="));
}

#[test]
fn uid_and_pwd_are_appended_only_when_missing() {
    // Neither present: both appended.
    let p = params(&[("connection_string", "Driver={SQL Server};Server=h")], "h", 0, Some("u"), Some("p"));
    assert_eq!(build_connection_string(&p).unwrap(), "Driver={SQL Server};Server=h;UID=u;PWD=p;");
    // Both present (any case, braced values): nothing appended, string used verbatim.
    let p = params(&[("connection_string", "DSN=x;Uid=already;Pwd={s;s};")], "h", 0, Some("u"), Some("p"));
    assert_eq!(build_connection_string(&p).unwrap(), "DSN=x;Uid=already;Pwd={s;s};");
    // Only UID present: PWD appended, UID untouched.
    let p = params(&[("connection_string", "DSN=x;UID=already")], "h", 0, Some("u"), Some("p"));
    assert_eq!(build_connection_string(&p).unwrap(), "DSN=x;UID=already;PWD=p;");
    // No credentials in params: nothing appended.
    let p = params(&[("connection_string", "DSN=x")], "h", 0, None, None);
    assert_eq!(build_connection_string(&p).unwrap(), "DSN=x;");
    // Empty credentials count as absent.
    let p = params(&[("connection_string", "DSN=x")], "h", 0, Some(""), Some(""));
    assert_eq!(build_connection_string(&p).unwrap(), "DSN=x;");
}

#[test]
fn key_scanner_skips_braced_values() {
    assert_eq!(connection_string_keys("Driver={a;b=c};PWD={x}}y};Server=h"), vec!["DRIVER", "PWD", "SERVER"]);
}

#[test]
fn missing_driver_dsn_and_connection_string_is_invalid() {
    let p = params(&[], "h", 0, Some("u"), Some("p"));
    assert!(matches!(build_connection_string(&p), Err(CoreError::Invalid(_))));
}

// ---------------------------------------------------------------------------------------------
// Connecting (failure paths only: no server is required)
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn connect_without_driver_or_dsn_is_invalid() {
    let p = params(&[], "127.0.0.1", 1, None, None);
    match OdbcDatabase::connect(&p).await {
        Err(CoreError::Invalid(_)) => {}
        Err(other) => panic!("expected Invalid, got {other:?}"),
        Ok(_) => panic!("connected without a driver, DSN or connection string"),
    }
}

#[tokio::test]
async fn connect_with_unknown_driver_is_a_connect_error() {
    // IM002 "Data source name not found and no default driver specified" from the driver manager.
    let p = params(&[("driver", "BulkDB Nonexistent Driver 0.0")], "127.0.0.1", 1, Some("u"), Some("p"));
    let started = Instant::now();
    match OdbcDatabase::connect(&p).await {
        Err(CoreError::Connect(msg)) => {
            println!("unknown driver -> Connect: {msg}");
            assert!(msg.contains("IM002") || msg.to_ascii_lowercase().contains("driver"), "unexpected message: {msg}");
        }
        Err(other) => panic!("expected Connect, got {other:?}"),
        Ok(_) => panic!("connected with a driver that does not exist"),
    }
    assert!(started.elapsed() < Duration::from_secs(5), "the driver manager rejects an unknown driver immediately");
}

#[tokio::test]
async fn connect_to_a_refused_port_is_a_connect_error_within_the_login_timeout() {
    let drivers = installed_drivers().unwrap();
    let Some(driver) = sql_server_driver(&drivers) else {
        println!("no SQL Server ODBC driver installed; skipping");
        return;
    };
    println!("connecting with '{driver}' to 127.0.0.1,1 (expected to be refused)");
    // Encrypt=no keeps ODBC Driver 18 from negotiating TLS before it reports the refused socket.
    let conn_str = format!("Driver={{{driver}}};Server=127.0.0.1,1;Encrypt=no;");
    let p = params(&[("driver", driver.as_str()), ("connection_string", &conn_str)], "127.0.0.1", 1, Some("sa"), Some("nope"));
    let started = Instant::now();
    let result = OdbcDatabase::connect(&p).await;
    let elapsed = started.elapsed();
    match result {
        Err(CoreError::Connect(msg)) => println!("refused -> Connect after {elapsed:?}: {msg}"),
        Err(other) => panic!("expected Connect, got {other:?} after {elapsed:?}"),
        Ok(_) => panic!("connected to 127.0.0.1,1 after {elapsed:?}; nothing should listen there"),
    }
    // Login timeout (5 s) plus the async side's grace period: the actor must not hang.
    assert!(elapsed < Duration::from_secs(20), "connect took {elapsed:?}");
}
