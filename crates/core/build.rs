//! The only place that knows which targets ship an ODBC driver manager.
//! Windows always has odbc32.dll; macOS needs unixODBC (`brew install unixodbc`) at build
//! time, so the ODBC source is compiled in only when `BULKDB_ODBC=1` is set there.
fn main() {
    println!("cargo:rerun-if-env-changed=BULKDB_ODBC");
    println!("cargo:rustc-check-cfg=cfg(has_odbc)");
    println!("cargo:rustc-check-cfg=cfg(has_oracle)");
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let odbc_feature = std::env::var("CARGO_FEATURE_ODBC").is_ok();
    let forced = std::env::var("BULKDB_ODBC").map(|v| v == "1").unwrap_or(false);
    if odbc_feature && (target_os == "windows" || forced) {
        println!("cargo:rustc-cfg=has_odbc");
    }
    if std::env::var("CARGO_FEATURE_ORACLE").is_ok() {
        println!("cargo:rustc-cfg=has_oracle");
    }
}
