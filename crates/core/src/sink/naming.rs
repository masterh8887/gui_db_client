//! File naming: pattern expansion and extension chains.

use crate::types::{Compression, OutputFormat, OutputOpts};

/// `.csv` / `.parquet` plus `.gz` / `.zst`.
pub fn extension(opts: &OutputOpts) -> String {
    let mut ext = match opts.format {
        OutputFormat::Csv => {
            if opts.csv.as_ref().map(|c| c.delimiter == "\t").unwrap_or(false) {
                ".tsv".to_string()
            } else {
                ".csv".to_string()
            }
        }
        OutputFormat::Parquet => ".parquet".to_string(),
    };
    if opts.format == OutputFormat::Csv {
        match opts.compress {
            Some(Compression::Gzip) => ext.push_str(".gz"),
            Some(Compression::Zstd) => ext.push_str(".zst"),
            None => {}
        }
    }
    ext
}

/// `<path>.partial` (the existing extension kept), the name [`crate::sink::to_partial`]
/// renames a file to. Computes the name only; touches nothing on disk.
pub fn partial_path(path: &std::path::Path) -> std::path::PathBuf {
    let mut name = path.file_name().map(|s| s.to_os_string()).unwrap_or_default();
    name.push(".partial");
    path.with_file_name(name)
}

/// Expand `{conn}`, `{name}`, `{yyyyMMdd_HHmm}`, `{yyyyMMdd}`, `{HHmmss}` and sanitise the
/// result into a safe file stem.
pub fn expand_pattern(pattern: &str, conn: &str, name: &str, now: chrono::DateTime<chrono::Local>) -> String {
    let s = pattern
        .replace("{conn}", conn)
        .replace("{name}", name)
        .replace("{yyyyMMdd_HHmm}", &now.format("%Y%m%d_%H%M").to_string())
        .replace("{yyyyMMdd}", &now.format("%Y%m%d").to_string())
        .replace("{HHmmss}", &now.format("%H%M%S").to_string())
        .replace("{HHmm}", &now.format("%H%M").to_string());
    sanitize_stem(&s)
}

/// Replace characters that are invalid in file names on Windows/macOS.
pub fn sanitize_stem(s: &str) -> String {
    let mut out: String = s.chars().map(|c| if matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') || c.is_control() { '_' } else { c }).collect();
    out = out.trim().trim_end_matches('.').to_string();
    if out.is_empty() {
        "export".to_string()
    } else {
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn expands_and_sanitizes() {
        let now = chrono::Local.with_ymd_and_hms(2026, 10, 23, 14, 5, 9).unwrap();
        let s = expand_pattern("{conn}_{name}_{yyyyMMdd_HHmm}", "prod", "orders:v2", now);
        assert_eq!(s, "prod_orders_v2_20261023_1405");
    }

    #[test]
    fn partial_path_keeps_the_extension_chain() {
        let p = std::path::Path::new("out").join("a_part0001.csv.gz");
        assert_eq!(partial_path(&p), std::path::Path::new("out").join("a_part0001.csv.gz.partial"));
    }
}
