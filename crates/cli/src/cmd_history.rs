//! `history`: past extraction runs from the core's history database (the CLI's view of the
//! UI's history panel). `jobs` only knows the jobs of the running process; this survives it.

use std::collections::HashMap;
use std::process::ExitCode;

use bulk_core::error::CoreError;
use bulk_core::types::HistoryEntry;
use bulk_core::BulkDb;

use crate::args::HistoryArgs;
use crate::connect;
use crate::output::{enum_name, fmt_duration_ms, opt_bytes, opt_count, print_json, Align, Table};

/// Longest error text shown in the table (the full text is in `--json`).
const ERROR_WIDTH: usize = 100;

pub fn run(db: &BulkDb, args: HistoryArgs, json: bool) -> anyhow::Result<ExitCode> {
    let conn_id = match &args.conn {
        None => None,
        Some(key) => Some(match connect::find_saved(db, key) {
            Ok(c) => c.id,
            // Runs outlive their connection (ad-hoc runs delete theirs when they finish), so
            // an unknown key is taken as a raw connection id.
            Err(e) if matches!(e.downcast_ref::<CoreError>(), Some(CoreError::NotFound(_))) => {
                eprintln!("note: no saved connection '{key}'; listing runs recorded for connection id '{key}'");
                key.clone()
            }
            Err(e) => return Err(e),
        }),
    };
    let entries = db.history_list(conn_id.as_deref(), args.limit as usize)?;
    if json {
        print_json(&entries)?;
        return Ok(ExitCode::SUCCESS);
    }
    let names: HashMap<String, String> = db.conn_list().into_iter().map(|c| (c.id, c.name)).collect();
    let mut t = Table::new(&[
        ("ID", Align::Right),
        ("STARTED", Align::Left),
        ("STATE", Align::Left),
        ("CONNECTION", Align::Left),
        ("LABEL", Align::Left),
        ("ROWS", Align::Right),
        ("SIZE", Align::Right),
        ("DURATION", Align::Right),
        ("OUTPUT", Align::Left),
        ("ERROR", Align::Left),
    ]);
    for e in &entries {
        t.row(row(e, &names));
    }
    t.print();
    Ok(ExitCode::SUCCESS)
}

/// One table row; connections that no longer exist show their id.
fn row(e: &HistoryEntry, names: &HashMap<String, String>) -> Vec<String> {
    vec![
        e.id.to_string(),
        local_time(&e.started_at),
        enum_name(&e.state),
        names.get(&e.conn_id).cloned().unwrap_or_else(|| e.conn_id.clone()),
        e.label.clone(),
        opt_count(&e.rows),
        opt_bytes(&e.bytes),
        e.duration_ms.map(fmt_duration_ms).unwrap_or_else(|| "-".into()),
        output_cell(&e.files),
        e.error.as_deref().map(|m| truncate(m, ERROR_WIDTH)).unwrap_or_default(),
    ]
}

/// RFC 3339 timestamp as local `YYYY-MM-DD HH:MM:SS` (unparseable text is shown as is).
fn local_time(ts: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(ts).map(|d| d.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M:%S").to_string()).unwrap_or_else(|_| ts.to_string())
}

/// First output file, plus how many more were written (split runs).
fn output_cell(files: &[String]) -> String {
    match files {
        [] => "-".into(),
        [one] => one.clone(),
        [first, rest @ ..] => format!("{first} (+{} more)", rest.len()),
    }
}

/// At most `max` characters, with `...` when shortened.
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(3)).collect();
    out.push_str("...");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cells() {
        assert_eq!(output_cell(&[]), "-");
        assert_eq!(output_cell(&["a.csv".into()]), "a.csv");
        assert_eq!(output_cell(&["a_0001.csv".into(), "a_0002.csv".into(), "a_0003.csv".into()]), "a_0001.csv (+2 more)");
        assert_eq!(truncate("short", 10), "short");
        assert_eq!(truncate("0123456789abc", 10), "0123456...");
        assert_eq!(truncate("가나다라마바사아자차카", 5), "가나...");
        assert_eq!(local_time("not a time"), "not a time");
        let shown = local_time("2026-10-26T01:02:03Z");
        assert_eq!(shown.len(), "2026-10-26 01:02:03".len(), "{shown}");
        assert!(shown.starts_with("2026-10-2"), "{shown}");
    }
}
