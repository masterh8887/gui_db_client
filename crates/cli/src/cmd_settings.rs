//! `settings get|set`: the CLI side of the UI's settings dialog. Values are validated with the
//! same limits the dialog enforces, then saved through `BulkDb::settings_set` (which also
//! resizes the export slots when `max_concurrent_jobs` changes).

use std::collections::BTreeSet;
use std::path::Path;
use std::process::ExitCode;

use bulk_core::error::CoreError;
use bulk_core::types::Settings;
use bulk_core::BulkDb;
use serde_json::Value;

use crate::args::{SettingsArgs, SettingsCmd};
use crate::connect::path_string;
use crate::output::{print_json, Align, Table};

/// Levels the app's log file accepts (the settings dialog offers the same list).
const LOG_LEVELS: [&str; 5] = ["trace", "debug", "info", "warn", "error"];
const MAX_JOBS: std::ops::RangeInclusive<u32> = 1..=3;
const MIN_MONITOR_INTERVAL_MS: u32 = 500;
const MIN_PROGRESS_INTERVAL_MS: u32 = 100;

/// The scalar fields of [`Settings`] (the `ui` map is addressed as `ui.<name>`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Field {
    MaxConcurrentJobs,
    DefaultOutputDir,
    CsvCrlfDefault,
    VerifyAfterExport,
    LogLevel,
    TunnelIdleSecs,
    MonitorIntervalMs,
    ProgressIntervalMs,
}

/// Display order and canonical names (snake_case; settings.toml itself stores camelCase).
const FIELDS: [(Field, &str); 8] = [
    (Field::MaxConcurrentJobs, "max_concurrent_jobs"),
    (Field::DefaultOutputDir, "default_output_dir"),
    (Field::CsvCrlfDefault, "csv_crlf_default"),
    (Field::VerifyAfterExport, "verify_after_export"),
    (Field::LogLevel, "log_level"),
    (Field::TunnelIdleSecs, "tunnel_idle_secs"),
    (Field::MonitorIntervalMs, "monitor_interval_ms"),
    (Field::ProgressIntervalMs, "progress_interval_ms"),
];

#[derive(Debug, Clone, PartialEq, Eq)]
enum Key {
    Field(Field, &'static str),
    Ui(String),
}

impl Key {
    fn name(&self) -> String {
        match self {
            Key::Field(_, name) => (*name).to_string(),
            Key::Ui(k) => format!("ui.{k}"),
        }
    }
}

fn invalid(msg: String) -> anyhow::Error {
    CoreError::Invalid(msg).into()
}

/// `max_concurrent_jobs`, `max-concurrent-jobs` and `maxConcurrentJobs` all name the same
/// field; `ui.<name>` names a UI preference (the name is case sensitive).
fn parse_key(raw: &str) -> anyhow::Result<Key> {
    let raw = raw.trim();
    if let Some(rest) = raw.strip_prefix("ui.") {
        let name = rest.trim();
        if name.is_empty() {
            return Err(invalid("ui.<name> needs a name".into()));
        }
        return Ok(Key::Ui(name.to_string()));
    }
    let squashed: String = raw.chars().filter(|c| *c != '_' && *c != '-').flat_map(char::to_lowercase).collect();
    FIELDS
        .iter()
        .find(|(_, name)| name.replace('_', "") == squashed)
        .map(|(f, name)| Key::Field(*f, name))
        .ok_or_else(|| invalid(format!("unknown setting '{raw}'; known keys: {}, ui.<name>", FIELDS.iter().map(|(_, n)| *n).collect::<Vec<_>>().join(", "))))
}

/// Current value of a key (`null` for an unset optional value).
fn get(s: &Settings, key: &Key) -> Value {
    match key {
        Key::Field(f, _) => match f {
            Field::MaxConcurrentJobs => s.max_concurrent_jobs.into(),
            Field::DefaultOutputDir => s.default_output_dir.clone().map(Value::String).unwrap_or(Value::Null),
            Field::CsvCrlfDefault => s.csv_crlf_default.into(),
            Field::VerifyAfterExport => s.verify_after_export.into(),
            Field::LogLevel => s.log_level.clone().into(),
            Field::TunnelIdleSecs => s.tunnel_idle_secs.into(),
            Field::MonitorIntervalMs => s.monitor_interval_ms.into(),
            Field::ProgressIntervalMs => s.progress_interval_ms.into(),
        },
        Key::Ui(k) => s.ui.get(k).cloned().map(Value::String).unwrap_or(Value::Null),
    }
}

fn parse_u32(key: &Key, v: &str) -> anyhow::Result<u32> {
    v.parse::<u32>().map_err(|_| invalid(format!("{}: expected a whole number, got '{v}'", key.name())))
}

fn parse_bool(key: &Key, v: &str) -> anyhow::Result<bool> {
    match v.to_ascii_lowercase().as_str() {
        "true" | "yes" | "on" | "1" => Ok(true),
        "false" | "no" | "off" | "0" => Ok(false),
        _ => Err(invalid(format!("{}: expected true or false, got '{v}'", key.name()))),
    }
}

/// Validate `value` for `key` and store it in `s`.
fn set(s: &mut Settings, key: &Key, value: &str) -> anyhow::Result<()> {
    let v = value.trim();
    match key {
        Key::Field(f, name) => match f {
            Field::MaxConcurrentJobs => {
                let n = parse_u32(key, v)?;
                if !MAX_JOBS.contains(&n) {
                    return Err(invalid(format!("{name} must be {}..{}, got {n}", MAX_JOBS.start(), MAX_JOBS.end())));
                }
                s.max_concurrent_jobs = n;
            }
            Field::DefaultOutputDir => s.default_output_dir = (!v.is_empty()).then(|| path_string(Path::new(v))),
            Field::CsvCrlfDefault => s.csv_crlf_default = parse_bool(key, v)?,
            Field::VerifyAfterExport => s.verify_after_export = parse_bool(key, v)?,
            Field::LogLevel => {
                let level = v.to_ascii_lowercase();
                if !LOG_LEVELS.contains(&level.as_str()) {
                    return Err(invalid(format!("{name} must be one of {}, got '{v}'", LOG_LEVELS.join(", "))));
                }
                s.log_level = level;
            }
            Field::TunnelIdleSecs => s.tunnel_idle_secs = parse_u32(key, v)?,
            Field::MonitorIntervalMs => s.monitor_interval_ms = at_least(key, parse_u32(key, v)?, MIN_MONITOR_INTERVAL_MS)?,
            Field::ProgressIntervalMs => s.progress_interval_ms = at_least(key, parse_u32(key, v)?, MIN_PROGRESS_INTERVAL_MS)?,
        },
        Key::Ui(k) => {
            if value.is_empty() {
                s.ui.remove(k);
            } else {
                s.ui.insert(k.clone(), value.to_string());
            }
        }
    }
    Ok(())
}

fn at_least(key: &Key, n: u32, min: u32) -> anyhow::Result<u32> {
    if n < min {
        return Err(invalid(format!("{} must be at least {min}, got {n}", key.name())));
    }
    Ok(n)
}

/// Every key in display order: the fields, then the `ui.*` entries.
fn keys(s: &Settings) -> Vec<Key> {
    FIELDS.iter().map(|(f, n)| Key::Field(*f, n)).chain(s.ui.keys().map(|k| Key::Ui(k.clone()))).collect()
}

/// `(key, before, after)` for every key whose value differs.
fn diff(before: &Settings, after: &Settings) -> Vec<(String, Value, Value)> {
    let ui: BTreeSet<&String> = before.ui.keys().chain(after.ui.keys()).collect();
    FIELDS
        .iter()
        .map(|(f, n)| Key::Field(*f, n))
        .chain(ui.into_iter().map(|k| Key::Ui(k.clone())))
        .filter_map(|k| {
            let (b, a) = (get(before, &k), get(after, &k));
            (b != a).then(|| (k.name(), b, a))
        })
        .collect()
}

/// Plain text of a value; `unset` stands for `null` in tables and change lists.
fn shown(v: &Value) -> String {
    match v {
        Value::Null => "(unset)".into(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

pub fn run(db: &BulkDb, args: SettingsArgs, json: bool) -> anyhow::Result<ExitCode> {
    match args.cmd {
        SettingsCmd::Get { key: None } => {
            let s = db.settings_get();
            if json {
                print_json(&s)?;
            } else {
                let mut t = Table::new(&[("KEY", Align::Left), ("VALUE", Align::Left)]);
                for k in keys(&s) {
                    t.row(vec![k.name(), shown(&get(&s, &k))]);
                }
                t.print();
            }
        }
        SettingsCmd::Get { key: Some(raw) } => {
            let key = parse_key(&raw)?;
            let value = get(&db.settings_get(), &key);
            if json {
                let mut m = serde_json::Map::new();
                m.insert(key.name(), value);
                print_json(&m)?;
            } else {
                // Bare value for scripts: an unset value prints an empty line.
                println!("{}", if value.is_null() { String::new() } else { shown(&value) });
            }
        }
        SettingsCmd::Set { pairs } => {
            let before = db.settings_get();
            let mut next = before.clone();
            for (k, v) in &pairs {
                let key = parse_key(k)?;
                set(&mut next, &key, v)?;
            }
            let changes = diff(&before, &next);
            let saved = if changes.is_empty() { before } else { db.settings_set(next)? };
            if json {
                print_json(&saved)?;
            } else if changes.is_empty() {
                println!("settings unchanged");
            } else {
                for (k, b, a) in &changes {
                    println!("{k}: {} -> {}", shown(b), shown(a));
                }
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_in_any_spelling() {
        for raw in ["max_concurrent_jobs", "max-concurrent-jobs", "maxConcurrentJobs", " MAX_CONCURRENT_JOBS "] {
            assert_eq!(parse_key(raw).unwrap(), Key::Field(Field::MaxConcurrentJobs, "max_concurrent_jobs"), "{raw}");
        }
        assert_eq!(parse_key("ui.theme").unwrap(), Key::Ui("theme".into()));
        assert!(parse_key("ui.").is_err());
        let err = parse_key("colour").unwrap_err().to_string();
        assert!(err.contains("unknown setting") && err.contains("log_level"), "{err}");
    }

    #[test]
    fn values_are_validated_like_the_dialog() {
        let mut s = Settings::default();
        let k = |raw: &str| parse_key(raw).unwrap();
        set(&mut s, &k("max_concurrent_jobs"), "2").unwrap();
        assert_eq!(s.max_concurrent_jobs, 2);
        assert!(set(&mut s, &k("max_concurrent_jobs"), "4").is_err());
        assert!(set(&mut s, &k("max_concurrent_jobs"), "0").is_err());
        assert!(set(&mut s, &k("max_concurrent_jobs"), "two").is_err());
        set(&mut s, &k("verify_after_export"), "yes").unwrap();
        assert!(s.verify_after_export);
        set(&mut s, &k("csv-crlf-default"), "FALSE").unwrap();
        assert!(!s.csv_crlf_default);
        assert!(set(&mut s, &k("csv_crlf_default"), "maybe").is_err());
        set(&mut s, &k("log_level"), "DEBUG").unwrap();
        assert_eq!(s.log_level, "debug");
        assert!(set(&mut s, &k("log_level"), "verbose").is_err());
        assert!(set(&mut s, &k("monitor_interval_ms"), "499").is_err());
        set(&mut s, &k("monitor_interval_ms"), "1000").unwrap();
        assert!(set(&mut s, &k("progress_interval_ms"), "50").is_err());
        set(&mut s, &k("tunnel_idle_secs"), "0").unwrap();
        assert_eq!(s.tunnel_idle_secs, 0);

        set(&mut s, &k("default_output_dir"), "exports").unwrap();
        let dir = s.default_output_dir.clone().expect("dir set");
        assert!(Path::new(&dir).is_absolute() && dir.ends_with("exports"), "{dir}");
        set(&mut s, &k("default_output_dir"), "").unwrap();
        assert!(s.default_output_dir.is_none());

        set(&mut s, &k("ui.theme"), "dark").unwrap();
        assert_eq!(s.ui.get("theme").map(String::as_str), Some("dark"));
        set(&mut s, &k("ui.theme"), "").unwrap();
        assert!(s.ui.is_empty());
    }

    #[test]
    fn diff_lists_changed_keys_only() {
        let before = Settings::default();
        let mut after = before.clone();
        after.max_concurrent_jobs = 3;
        after.ui.insert("theme".into(), "dark".into());
        let d = diff(&before, &after);
        assert_eq!(d, vec![("max_concurrent_jobs".to_string(), Value::from(1), Value::from(3)), ("ui.theme".to_string(), Value::Null, Value::from("dark"))]);
        assert!(diff(&before, &before).is_empty());
        assert_eq!(shown(&Value::Null), "(unset)");
        assert_eq!(keys(&after).len(), FIELDS.len() + 1);
    }
}
