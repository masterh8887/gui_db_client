//! Deterministic datasets shared by unit tests, integration tests and the bench harness.
//!
//! * `wide`  — 9 text columns, ~200 B per row (the 2026-10-23 `content` extraction shape).
//! * `narrow` — 3 integer columns (memory stays flat regardless of row count).
//! * `nasty` — commas, quotes, newlines, unicode, NULLs mixed in (CSV correctness).
//!
//! Generators are seeded LCGs, so every platform produces the same bytes.

/// Tiny deterministic PRNG (64-bit LCG), good enough for test data.
#[derive(Clone)]
pub struct Lcg(u64);

impl Lcg {
    pub fn new(seed: u64) -> Self {
        Lcg(seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407))
    }
    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        self.0 >> 11
    }
    pub fn below(&mut self, n: u64) -> u64 {
        if n == 0 {
            0
        } else {
            self.next_u64() % n
        }
    }
}

pub const WIDE_COLUMNS: [&str; 9] = ["id", "title", "body", "author", "category", "tags", "created_at", "updated_at", "status"];
pub const NARROW_COLUMNS: [&str; 3] = ["a", "b", "c"];
pub const NASTY_COLUMNS: [&str; 6] = ["id", "text", "num", "note", "flag", "unicode"];

const WORDS: [&str; 16] =
    ["lorem", "ipsum", "dolor", "sit", "amet", "consectetur", "adipiscing", "elit", "sed", "do", "eiusmod", "tempor", "incididunt", "ut", "labore", "magna"];

/// One `wide` row: 9 strings, roughly 200 bytes.
pub fn wide_row(i: u64, rng: &mut Lcg) -> Vec<String> {
    let words = |rng: &mut Lcg, n: usize| (0..n).map(|_| WORDS[rng.below(16) as usize]).collect::<Vec<_>>().join(" ");
    vec![
        i.to_string(),
        words(rng, 4),
        words(rng, 18),
        format!("user{}", rng.below(5000)),
        ["news", "blog", "music", "video"][rng.below(4) as usize].to_string(),
        format!("t{},t{}", rng.below(100), rng.below(100)),
        format!("2026-{:02}-{:02} {:02}:{:02}:{:02}", 1 + rng.below(12), 1 + rng.below(28), rng.below(24), rng.below(60), rng.below(60)),
        format!("2026-{:02}-{:02} {:02}:{:02}:{:02}", 1 + rng.below(12), 1 + rng.below(28), rng.below(24), rng.below(60), rng.below(60)),
        ["active", "deleted", "draft"][rng.below(3) as usize].to_string(),
    ]
}

/// One `narrow` row: 3 integers.
pub fn narrow_row(i: u64, rng: &mut Lcg) -> [i64; 3] {
    [i as i64, rng.below(1_000_000) as i64, (rng.next_u64() as i64) % 1000]
}

/// One `nasty` row. `None` is SQL NULL. Column types: int, text, float-as-text, text, bool-as-text, text.
pub fn nasty_row(i: u64, rng: &mut Lcg) -> Vec<Option<String>> {
    let text = match rng.below(8) {
        0 => Some("plain".to_string()),
        1 => Some("has,comma".to_string()),
        2 => Some("has \"quote\" inside".to_string()),
        3 => Some("line1\nline2".to_string()),
        4 => Some("crlf\r\nline".to_string()),
        5 => Some(String::new()),
        6 => None,
        _ => Some(" leading and trailing ".to_string()),
    };
    let unicode = match rng.below(5) {
        0 => Some("한글 텍스트".to_string()),
        1 => Some("emoji 🚀🎉".to_string()),
        2 => Some("日本語, 中文".to_string()),
        3 => None,
        _ => Some("ünïcödé".to_string()),
    };
    vec![
        Some(i.to_string()),
        text,
        if rng.below(10) == 0 { None } else { Some(format!("{}.{:03}", rng.below(100000), rng.below(1000))) },
        if rng.below(4) == 0 { None } else { Some(format!("note {}", rng.below(1000))) },
        Some(if rng.below(2) == 0 { "true".into() } else { "false".into() }),
        unicode,
    ]
}

/// Create a SQLite database at `path` with `wide`, `narrow` and `nasty` tables of the given
/// sizes (0 skips a table). Returns the number of rows written per table.
pub fn sqlite_fixture(path: &std::path::Path, wide: u64, narrow: u64, nasty: u64) -> rusqlite::Result<(u64, u64, u64)> {
    let mut conn = rusqlite::Connection::open(path)?;
    conn.execute_batch(
        "PRAGMA journal_mode=OFF; PRAGMA synchronous=OFF;
         CREATE TABLE IF NOT EXISTS wide (id INTEGER PRIMARY KEY, title TEXT, body TEXT, author TEXT, category TEXT, tags TEXT, created_at TEXT, updated_at TEXT, status TEXT);
         CREATE TABLE IF NOT EXISTS narrow (a INTEGER PRIMARY KEY, b INTEGER, c INTEGER);
         CREATE TABLE IF NOT EXISTS nasty (id INTEGER PRIMARY KEY, text TEXT, num REAL, note TEXT, flag TEXT, unicode TEXT);
         CREATE INDEX IF NOT EXISTS nasty_note ON nasty(note) WHERE note IS NOT NULL;",
    )?;
    let mut rng = Lcg::new(42);
    let tx = conn.transaction()?;
    {
        let mut st = tx.prepare("INSERT INTO wide VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)")?;
        for i in 0..wide {
            let r = wide_row(i, &mut rng);
            st.execute(rusqlite::params![r[0].parse::<i64>().unwrap(), r[1], r[2], r[3], r[4], r[5], r[6], r[7], r[8]])?;
        }
        let mut st = tx.prepare("INSERT INTO narrow VALUES (?1,?2,?3)")?;
        for i in 0..narrow {
            let r = narrow_row(i, &mut rng);
            st.execute(rusqlite::params![r[0], r[1], r[2]])?;
        }
        let mut st = tx.prepare("INSERT INTO nasty VALUES (?1,?2,?3,?4,?5,?6)")?;
        for i in 0..nasty {
            let r = nasty_row(i, &mut rng);
            let num: Option<f64> = r[2].as_ref().map(|s| s.parse().unwrap());
            st.execute(rusqlite::params![i as i64, r[1], num, r[3], r[4], r[5]])?;
        }
    }
    tx.commit()?;
    Ok((wide, narrow, nasty))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic() {
        let mut a = Lcg::new(1);
        let mut b = Lcg::new(1);
        assert_eq!(wide_row(1, &mut a), wide_row(1, &mut b));
        let mut c = Lcg::new(7);
        let rows: Vec<_> = (0..200).map(|i| nasty_row(i, &mut c)).collect();
        assert!(rows.iter().any(|r| r[1].as_deref() == Some("line1\nline2")));
        assert!(rows.iter().any(|r| r[1].is_none()));
    }

    #[test]
    fn fixture_builds() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("t.db");
        sqlite_fixture(&p, 10, 10, 10).unwrap();
        let c = rusqlite::Connection::open(&p).unwrap();
        let n: i64 = c.query_row("select count(*) from nasty", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 10);
    }
}
