//! Terminal output helpers shared by every subcommand: aligned tables, humanised numbers,
//! duration parsing and JSON printing of the core types. Nothing here touches the database.

use bulk_core::types::{PreflightIssue, Severity};
use serde::Serialize;

/// Column alignment for [`Table`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Align {
    Left,
    Right,
}

/// Minimal fixed-width table renderer (two-space column gap, dashed header rule). Numbers are
/// right-aligned by the caller through [`Align::Right`].
#[derive(Debug, Default)]
pub struct Table {
    headers: Vec<String>,
    aligns: Vec<Align>,
    rows: Vec<Vec<String>>,
}

impl Table {
    pub fn new(columns: &[(&str, Align)]) -> Self {
        Table { headers: columns.iter().map(|(h, _)| h.to_string()).collect(), aligns: columns.iter().map(|(_, a)| *a).collect(), rows: Vec::new() }
    }

    /// Append one row. Control characters (newlines in comments, tabs) are flattened so the
    /// table stays rectangular; missing cells are padded with empty strings.
    pub fn row(&mut self, cells: Vec<String>) {
        let mut cells: Vec<String> = cells.into_iter().map(|c| flatten(&c)).collect();
        cells.resize(self.headers.len(), String::new());
        self.rows.push(cells);
    }

    pub fn render(&self) -> String {
        let n = self.headers.len();
        let mut widths: Vec<usize> = self.headers.iter().map(|h| display_width(h)).collect();
        for r in &self.rows {
            for (i, c) in r.iter().enumerate().take(n) {
                widths[i] = widths[i].max(display_width(c));
            }
        }
        let mut out = String::new();
        push_line(&mut out, &self.headers, &widths, &self.aligns);
        let rule: Vec<String> = widths.iter().map(|w| "-".repeat(*w)).collect();
        out.push_str(rule.join("  ").trim_end());
        out.push('\n');
        for r in &self.rows {
            push_line(&mut out, r, &widths, &self.aligns);
        }
        out
    }

    /// Print to stdout; an empty table prints `(no rows)` so scripts see something happened.
    pub fn print(&self) {
        if self.rows.is_empty() {
            println!("(no rows)");
        } else {
            print!("{}", self.render());
        }
    }
}

fn flatten(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c == '\n' || c == '\r' || c == '\t' {
                ' '
            } else if c.is_control() {
                '?'
            } else {
                c
            }
        })
        .collect()
}

/// Approximate display width: one cell per char (CJK is wider on most terminals, but a
/// dependency-free CLI accepts slightly ragged columns there).
fn display_width(s: &str) -> usize {
    s.chars().count()
}

fn push_line(out: &mut String, cells: &[String], widths: &[usize], aligns: &[Align]) {
    let mut parts = Vec::with_capacity(cells.len());
    for (i, c) in cells.iter().enumerate() {
        let pad = widths.get(i).copied().unwrap_or(0).saturating_sub(display_width(c));
        let align = aligns.get(i).copied().unwrap_or(Align::Left);
        parts.push(match align {
            Align::Right => format!("{}{}", " ".repeat(pad), c),
            Align::Left => format!("{}{}", c, " ".repeat(pad)),
        });
    }
    out.push_str(parts.join("  ").trim_end());
    out.push('\n');
}

// ---------------------------------------------------------------------------------------------
// Numbers
// ---------------------------------------------------------------------------------------------

const UNITS: [&str; 6] = ["B", "KB", "MB", "GB", "TB", "PB"];

/// `1536` -> `1.5 KB`, `0` -> `0 B` (binary units, one decimal above bytes).
pub fn human_bytes(n: u64) -> String {
    human_bytes_f(n as f64)
}

/// Same as [`human_bytes`] for fractional values (rates).
pub fn human_bytes_f(v: f64) -> String {
    let v = if v.is_finite() { v.max(0.0) } else { 0.0 };
    if v < 1024.0 {
        return format!("{v:.0} B");
    }
    let mut value = v;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

/// `45.6 MB/s`.
pub fn human_rate(bytes_per_sec: f64) -> String {
    format!("{}/s", human_bytes_f(bytes_per_sec))
}

/// `1234567` -> `1,234,567`.
pub fn human_count(n: u64) -> String {
    let digits = n.to_string();
    let len = digits.len();
    let mut out = String::with_capacity(len + len / 3);
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (len - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// `512300.0` -> `512.3k`, `1.2M`, `987`.
pub fn short_count(v: f64) -> String {
    let v = if v.is_finite() { v.max(0.0) } else { 0.0 };
    if v >= 1e9 {
        format!("{:.1}G", v / 1e9)
    } else if v >= 1e6 {
        format!("{:.1}M", v / 1e6)
    } else if v >= 1e3 {
        format!("{:.1}k", v / 1e3)
    } else {
        format!("{v:.0}")
    }
}

/// `123 ms`, `12.3 s`, `1m 02s`, `1h 02m 03s`.
pub fn fmt_duration_ms(ms: u64) -> String {
    if ms < 1000 {
        return format!("{ms} ms");
    }
    let secs = ms as f64 / 1000.0;
    if secs < 60.0 {
        return format!("{secs:.1} s");
    }
    let total = ms / 1000;
    let (h, m, s) = (total / 3600, (total % 3600) / 60, total % 60);
    if h > 0 {
        format!("{h}h {m:02}m {s:02}s")
    } else {
        format!("{m}m {s:02}s")
    }
}

/// `MM:SS`, or `H:MM:SS` from one hour on. Used for elapsed / ETA in the progress line.
pub fn fmt_clock(secs: u64) -> String {
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m:02}:{s:02}")
    }
}

/// Parse `60m`, `30s`, `1500ms`, `2h`, `1.5s` or a plain number of milliseconds.
pub fn parse_duration_ms(input: &str) -> Result<u64, String> {
    let s = input.trim().to_ascii_lowercase();
    if s.is_empty() {
        return Err("empty duration".into());
    }
    let split = s.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    let value: f64 = num.parse().map_err(|_| format!("invalid duration '{input}' (examples: 60m, 30s, 1500ms, 250)"))?;
    let mult = match unit.trim() {
        "" | "ms" | "msec" => 1.0,
        "s" | "sec" | "secs" | "second" | "seconds" => 1_000.0,
        "m" | "min" | "mins" | "minute" | "minutes" => 60_000.0,
        "h" | "hr" | "hour" | "hours" => 3_600_000.0,
        "d" | "day" | "days" => 86_400_000.0,
        other => return Err(format!("unknown duration unit '{other}' in '{input}' (use ms, s, m, h or d)")),
    };
    let ms = value * mult;
    if !ms.is_finite() || ms < 0.0 || ms > u64::MAX as f64 {
        return Err(format!("duration '{input}' is out of range"));
    }
    Ok(ms.round() as u64)
}

// ---------------------------------------------------------------------------------------------
// Misc formatting
// ---------------------------------------------------------------------------------------------

/// `-` for `None`.
pub fn opt<T: ToString>(v: &Option<T>) -> String {
    v.as_ref().map(|x| x.to_string()).unwrap_or_else(|| "-".to_string())
}

/// `-` for `None`, humanised bytes otherwise.
pub fn opt_bytes(v: &Option<u64>) -> String {
    v.map(human_bytes).unwrap_or_else(|| "-".to_string())
}

/// `-` for `None`, thousands-separated otherwise.
pub fn opt_count(v: &Option<u64>) -> String {
    v.map(human_count).unwrap_or_else(|| "-".to_string())
}

pub fn yes_no(b: bool) -> &'static str {
    if b {
        "yes"
    } else {
        "no"
    }
}

/// The serde name of a unit enum value (`done`, `vendor-client`, `seq_scan`, ...), so the
/// text output uses the same spelling as the JSON output.
pub fn enum_name<T: Serialize>(v: &T) -> String {
    match serde_json::to_value(v) {
        Ok(serde_json::Value::String(s)) => s,
        Ok(other) => other.to_string(),
        Err(_) => String::from("?"),
    }
}

pub fn severity_tag(s: Severity) -> &'static str {
    match s {
        Severity::Info => "INFO",
        Severity::Warning => "WARN",
        Severity::Danger => "DANGER",
    }
}

/// One preflight / plan issue as an indented block.
pub fn format_issue(i: &PreflightIssue) -> String {
    let mut s = format!("  [{}] {}: {}", severity_tag(i.severity), i.code, i.message);
    if let Some(d) = i.detail.as_deref().filter(|d| !d.trim().is_empty()) {
        s.push_str(&format!("\n           {}", d.trim().replace('\n', "\n           ")));
    }
    if let Some(r) = i.recommendation.as_deref().filter(|r| !r.trim().is_empty()) {
        s.push_str(&format!("\n           -> {}", r.trim()));
    }
    s
}

/// Print a titled issue list to stderr (nothing when the list is empty).
pub fn eprint_issues(title: &str, issues: &[PreflightIssue]) {
    if issues.is_empty() {
        return;
    }
    eprintln!("{title}:");
    for i in issues {
        eprintln!("{}", format_issue(i));
    }
}

/// Pretty JSON of any core type to stdout.
pub fn print_json<T: Serialize>(v: &T) -> anyhow::Result<()> {
    println!("{}", serde_json::to_string_pretty(v)?);
    Ok(())
}

/// `label        value` line for the probe / explain summaries.
pub fn kv(label: &str, value: impl AsRef<str>) {
    println!("{label:<18}{}", value.as_ref());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations() {
        assert_eq!(parse_duration_ms("60m").unwrap(), 3_600_000);
        assert_eq!(parse_duration_ms("30s").unwrap(), 30_000);
        assert_eq!(parse_duration_ms("1500ms").unwrap(), 1_500);
        assert_eq!(parse_duration_ms("250").unwrap(), 250);
        assert_eq!(parse_duration_ms("1.5s").unwrap(), 1_500);
        assert_eq!(parse_duration_ms(" 2h ").unwrap(), 7_200_000);
        assert!(parse_duration_ms("").is_err());
        assert!(parse_duration_ms("abc").is_err());
        assert!(parse_duration_ms("10parsecs").is_err());
    }

    #[test]
    fn bytes_and_counts() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(1023), "1023 B");
        assert_eq!(human_bytes(1536), "1.5 KB");
        assert_eq!(human_bytes(8 * 1024 * 1024), "8.0 MB");
        assert_eq!(human_rate(45.6 * 1024.0 * 1024.0), "45.6 MB/s");
        assert_eq!(human_count(0), "0");
        assert_eq!(human_count(999), "999");
        assert_eq!(human_count(1_000), "1,000");
        assert_eq!(human_count(1_234_567), "1,234,567");
        assert_eq!(short_count(987.0), "987");
        assert_eq!(short_count(512_300.0), "512.3k");
        assert_eq!(short_count(1_200_000.0), "1.2M");
    }

    #[test]
    fn durations_formatted() {
        assert_eq!(fmt_duration_ms(123), "123 ms");
        assert_eq!(fmt_duration_ms(12_340), "12.3 s");
        assert_eq!(fmt_duration_ms(62_000), "1m 02s");
        assert_eq!(fmt_duration_ms(3_723_000), "1h 02m 03s");
        assert_eq!(fmt_clock(62), "01:02");
        assert_eq!(fmt_clock(3_723), "1:02:03");
    }

    #[test]
    fn table_alignment() {
        let mut t = Table::new(&[("NAME", Align::Left), ("ROWS", Align::Right)]);
        t.row(vec!["wide".into(), "10,000".into()]);
        t.row(vec!["n\narrow".into(), "7".into()]);
        let out = t.render();
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines[0], "NAME       ROWS");
        assert_eq!(lines[1], "-------  ------");
        assert_eq!(lines[2], "wide     10,000");
        assert_eq!(lines[3], "n arrow       7");
    }

    #[test]
    fn enum_names_match_serde() {
        use bulk_core::types::{DriverTier, JobState};
        assert_eq!(enum_name(&JobState::Done), "done");
        assert_eq!(enum_name(&DriverTier::VendorClient), "vendor-client");
    }
}
