//! MySQL DDL translation (3.2 - 3.4): a token-aware `SHOW CREATE TABLE` parser that serves every
//! version from 3.23 / 4.0 to 9.x ([`parse_create_table`]), the per-target-version translation
//! of one table ([`translate_table`]: types, charsets, collations, engines, zero dates, keys,
//! row limits, per-column source reads), the view / trigger rewriting (database qualifiers,
//! DEFINER) and the zero-date policy resolution. The session prelude and the load statements
//! are copy-data's (`copy::data::mysql_encode`), so the plan shows exactly what runs.
//!
//! Everything here is pure: the planner (`copy::schema::mysql`) feeds it catalog text and
//! facts, and attaches the returned issues to plan items.

use std::collections::{BTreeMap, BTreeSet};

use crate::copy::types::*;

use super::mysql_reserved::is_reserved;
use super::{bq, bq2, danger, info, my_str, warning, IssueExt};

// =============================================================================================
// Versions and target capabilities
// =============================================================================================

/// A MySQL-family server version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MyVersion {
    pub maria: bool,
    pub major: u16,
    pub minor: u16,
    pub patch: u16,
}

impl MyVersion {
    /// `"8.0.36-log"`, `"5.7.44-ndb-7.6.29"`, `"4.0.27-standard"`, `"5.5.5-10.6.12-MariaDB"`.
    pub fn parse(raw: &str) -> MyVersion {
        let maria = raw.to_ascii_lowercase().contains("mariadb");
        let s = raw.trim();
        let s = if maria { s.strip_prefix("5.5.5-").unwrap_or(s) } else { s };
        let mut nums = s.split(|c: char| !c.is_ascii_digit()).filter(|p| !p.is_empty()).map(|p| p.parse::<u16>().unwrap_or(0));
        MyVersion { maria, major: nums.next().unwrap_or(0), minor: nums.next().unwrap_or(0), patch: nums.next().unwrap_or(0) }
    }

    pub fn of_facts(f: &TargetFacts) -> MyVersion {
        let mut v = MyVersion::parse(&f.version);
        v.maria = v.maria || f.flavor.eq_ignore_ascii_case("mariadb");
        if v.major == 0 {
            v.major = f.major;
            v.minor = f.minor;
            v.patch = f.patch;
        }
        v
    }

    pub fn at(&self, a: u16, b: u16, c: u16) -> bool {
        (self.major, self.minor, self.patch) >= (a, b, c)
    }

    pub fn label(&self) -> String {
        format!("{}{}.{}.{}", if self.maria { "MariaDB " } else { "" }, self.major, self.minor, self.patch)
    }
}

/// Source side of a MySQL copy, as far as translation needs it.
#[derive(Debug, Clone, PartialEq)]
pub struct MySourceCtx {
    pub ver: MyVersion,
    /// Source database.
    pub db: String,
    /// Read through the pre-4.1 text protocol (legacy client): FLOAT / DOUBLE arrive as text.
    pub text_protocol: bool,
    /// 4.0: `character_set` (`euc_kr`, `latin1`, ...); 4.1+: `character_set_server`.
    pub server_charset: Option<String>,
}

impl MySourceCtx {
    /// No per-session time zone before 4.1.3: TIMESTAMPs are read as epochs.
    pub fn epoch_timestamps(&self) -> bool {
        !self.ver.at(4, 1, 3)
    }

    /// 4.0 (and 3.23): the first TIMESTAMP column is auto-set.
    pub fn first_timestamp_rule(&self) -> bool {
        !self.ver.at(4, 1, 0)
    }
}

/// Target side of a MySQL copy and the resolved options.
#[derive(Debug, Clone, PartialEq)]
pub struct MyTargetCtx {
    pub ver: MyVersion,
    pub vars: BTreeMap<String, String>,
    /// Target database.
    pub db: String,
    /// Effective charset (3.3).
    pub charset: String,
    /// The user's collation (`"legacy"` resolved); `None` = the charset's default.
    pub user_collation: Option<String>,
    pub engine: MysqlEngineChoice,
    /// Resolved zero-date policy (never `Auto`).
    pub zero: ZeroDatePolicy,
    pub indexes_after_data: bool,
    pub fix_row_size: bool,
}

impl MyTargetCtx {
    fn my(&self, a: u16, b: u16, c: u16) -> bool {
        !self.ver.maria && self.ver.at(a, b, c)
    }
    fn maria(&self, a: u16, b: u16, c: u16) -> bool {
        self.ver.maria && self.ver.at(a, b, c)
    }
    pub fn var(&self, name: &str) -> Option<&str> {
        self.vars.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
    pub fn var_on(&self, name: &str) -> Option<bool> {
        self.var(name).map(|v| matches!(v.trim().to_ascii_uppercase().as_str(), "ON" | "1" | "TRUE" | "YES"))
    }
    pub fn utf8mb4(&self) -> bool {
        self.ver.at(5, 5, 3) || self.maria(5, 5, 0)
    }
    pub fn fsp(&self) -> bool {
        self.my(5, 6, 4) || self.maria(5, 3, 0)
    }
    pub fn datetime_now_default(&self) -> bool {
        self.my(5, 6, 5) || self.maria(10, 0, 1)
    }
    pub fn expr_default(&self) -> bool {
        self.my(8, 0, 13) || self.maria(10, 2, 1)
    }
    pub fn checks(&self) -> bool {
        self.my(8, 0, 16) || self.maria(10, 2, 1)
    }
    pub fn functional_keys(&self) -> bool {
        self.my(8, 0, 13)
    }
    pub fn invisible_index(&self) -> bool {
        self.my(8, 0, 0)
    }
    pub fn invisible_column(&self) -> bool {
        self.my(8, 0, 23) || self.maria(10, 3, 3)
    }
    pub fn generated(&self) -> bool {
        self.my(5, 7, 6) || self.maria(10, 2, 0)
    }
    pub fn json(&self) -> bool {
        self.my(5, 7, 8) || self.maria(10, 2, 7)
    }
    pub fn srid(&self) -> bool {
        self.my(8, 0, 0)
    }
    pub fn drops_display_width(&self) -> bool {
        self.my(8, 0, 17)
    }
    pub fn innodb_default(&self) -> bool {
        self.ver.at(5, 5, 0)
    }
    pub fn fulltext_innodb(&self) -> bool {
        self.my(5, 6, 0) || self.maria(10, 0, 5)
    }
    pub fn spatial_innodb(&self) -> bool {
        self.my(5, 7, 0) || self.maria(10, 2, 2)
    }
    pub fn partitions(&self) -> bool {
        self.ver.at(5, 1, 0)
    }
    pub fn uca0900(&self) -> bool {
        self.my(8, 0, 0)
    }
    pub fn unicode520(&self) -> bool {
        self.my(5, 6, 0) || self.maria(10, 0, 0)
    }
    pub fn index_comment(&self) -> bool {
        self.ver.at(5, 5, 3)
    }
    pub fn fulltext_parser(&self) -> bool {
        self.my(5, 7, 6)
    }
    /// The 8.0.17 deprecations (display widths, ZEROFILL, UNSIGNED on FLOAT/DOUBLE/DECIMAL, FLOAT(M,D)).
    pub fn deprecated_syntax(&self) -> bool {
        self.my(8, 0, 17)
    }
    /// InnoDB's default row format is DYNAMIC (5.7.9+, MariaDB 10.2.2+).
    pub fn dynamic_default(&self) -> bool {
        self.my(5, 7, 9) || self.maria(10, 2, 2)
    }
    /// `ROW_FORMAT=DYNAMIC` really gives DYNAMIC rows (5.7.7+, or 5.5/5.6 with Barracuda and
    /// file_per_table; otherwise the server silently uses COMPACT).
    pub fn dynamic_ok(&self) -> bool {
        if self.dynamic_default() || self.my(5, 7, 7) {
            return true;
        }
        let barracuda = self.var("innodb_file_format").is_some_and(|v| v.eq_ignore_ascii_case("barracuda"));
        barracuda && self.var_on("innodb_file_per_table").unwrap_or(false)
    }
    /// 3072-byte key parts with DYNAMIC rows.
    pub fn large_prefix(&self) -> bool {
        self.my(5, 7, 7) || self.maria(10, 2, 2) || self.var_on("innodb_large_prefix").unwrap_or(false)
    }
    pub fn page_size(&self) -> u64 {
        self.var("innodb_page_size").and_then(|v| v.trim().parse::<u64>().ok()).filter(|v| *v >= 4096).unwrap_or(16_384)
    }
    pub fn restrict_fk_non_standard(&self) -> bool {
        self.var_on("restrict_fk_on_non_standard_key").unwrap_or(self.my(8, 4, 0))
    }
    pub fn require_pk(&self) -> bool {
        self.var_on("sql_require_primary_key").unwrap_or(false)
    }
    pub fn lower_case_names(&self) -> bool {
        self.var("lower_case_table_names").is_some_and(|v| v.trim() != "0" && !v.trim().is_empty())
    }
    pub fn inplace_fk(&self) -> bool {
        self.ver.at(5, 6, 0)
    }
    /// `SET NAMES` charset of BulkDB's target sessions.
    pub fn names(&self) -> &'static str {
        if self.utf8mb4() {
            "utf8mb4"
        } else {
            "utf8"
        }
    }

    /// The default collation of `cs` on this target (3.3).
    pub fn default_collation(&self, cs: &str) -> Option<String> {
        let cs = norm_cs(cs);
        Some(match cs.as_str() {
            "utf8mb4" if self.uca0900() => "utf8mb4_0900_ai_ci".to_string(),
            "utf8mb4" => "utf8mb4_general_ci".to_string(),
            "utf8" | "utf8mb3" => format!("{cs}_general_ci"),
            "latin1" => "latin1_swedish_ci".to_string(),
            "euckr" => "euckr_korean_ci".to_string(),
            "ascii" | "ucs2" | "utf16" | "utf32" | "cp1250" | "cp1251" | "cp1256" | "cp1257" | "greek" | "hebrew" | "koi8r" | "latin2" | "latin5" | "latin7" | "gbk"
            | "big5" | "sjis" | "ujis" | "cp932" | "gb2312" | "tis620" => format!("{cs}_general_ci"),
            _ => return None,
        })
    }

    /// The `legacy` preset (PAD SPACE, 3.3).
    pub fn legacy_collation(&self, cs: &str) -> String {
        let cs = norm_cs(cs);
        if self.unicode520() {
            format!("{cs}_unicode_520_ci")
        } else {
            format!("{cs}_general_ci")
        }
    }
}

/// Global `sql_mode` default per version when the variable could not be read.
pub fn default_global_sql_mode(v: &MyVersion) -> &'static str {
    if v.maria {
        if v.at(10, 2, 4) {
            "STRICT_TRANS_TABLES,ERROR_FOR_DIVISION_BY_ZERO,NO_AUTO_CREATE_USER,NO_ENGINE_SUBSTITUTION"
        } else {
            ""
        }
    } else if v.at(8, 0, 0) {
        "ONLY_FULL_GROUP_BY,STRICT_TRANS_TABLES,NO_ZERO_IN_DATE,NO_ZERO_DATE,ERROR_FOR_DIVISION_BY_ZERO,NO_ENGINE_SUBSTITUTION"
    } else if v.at(5, 7, 5) {
        "ONLY_FULL_GROUP_BY,STRICT_TRANS_TABLES,NO_ZERO_IN_DATE,NO_ZERO_DATE,ERROR_FOR_DIVISION_BY_ZERO,NO_AUTO_CREATE_USER,NO_ENGINE_SUBSTITUTION"
    } else if v.at(5, 6, 6) {
        "NO_ENGINE_SUBSTITUTION"
    } else {
        ""
    }
}

fn modes(s: &str) -> Vec<String> {
    s.split(',').map(|m| m.trim().to_ascii_uppercase()).filter(|m| !m.is_empty()).collect()
}

/// `zero_dates: auto` (3.4): `null` when the target's global sql_mode has `NO_ZERO_DATE`, or
/// `NO_ZERO_IN_DATE` with a `STRICT_*` mode; otherwise `keep`.
pub fn resolve_zero_dates(policy: ZeroDatePolicy, global_sql_mode: &str) -> ZeroDatePolicy {
    if policy != ZeroDatePolicy::Auto {
        return policy;
    }
    let m = modes(global_sql_mode);
    let has = |x: &str| m.iter().any(|v| v == x);
    let strict = has("STRICT_TRANS_TABLES") || has("STRICT_ALL_TABLES") || has("TRADITIONAL");
    if has("NO_ZERO_DATE") || has("TRADITIONAL") || (has("NO_ZERO_IN_DATE") && strict) {
        ZeroDatePolicy::Null
    } else {
        ZeroDatePolicy::Keep
    }
}

/// sql_mode flags removed in 8.0 (a trigger's stored mode must not carry them).
const MODES_REMOVED_8_0: &[&str] =
    &["NO_AUTO_CREATE_USER", "DB2", "MAXDB", "MSSQL", "MYSQL323", "MYSQL40", "ORACLE", "POSTGRESQL", "NO_FIELD_OPTIONS", "NO_KEY_OPTIONS", "NO_TABLE_OPTIONS"];

/// A trigger's stored sql_mode minus the modes the target does not know.
pub fn trigger_sql_mode(mode: &str, t: &MyTargetCtx) -> String {
    modes(mode).into_iter().filter(|m| !(t.my(8, 0, 0) && MODES_REMOVED_8_0.contains(&m.as_str()))).collect::<Vec<_>>().join(",")
}

// =============================================================================================
// Lexer
// =============================================================================================

/// Token kinds of the MySQL lexer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TK {
    /// `` `name` `` (value unescaped).
    Ident,
    Word,
    /// `'…'` / `"…"` (value unescaped).
    Str,
    Num,
    Punct,
    /// `b'0101'`, `x'ff'`, `N'…'`, `_utf8mb4'…'` (value = raw text).
    Prefixed,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Tok {
    pub k: TK,
    pub v: String,
    /// Byte range in the lexed text.
    pub s: usize,
    pub e: usize,
}

impl Tok {
    pub fn is_word(&self, w: &str) -> bool {
        self.k == TK::Word && self.v.eq_ignore_ascii_case(w)
    }
    pub fn is_p(&self, c: char) -> bool {
        self.k == TK::Punct && self.v.len() == c.len_utf8() && self.v.starts_with(c)
    }
}

/// Replace the markers of versioned comments (`/*!50100 ` … `*/`) by spaces of the same byte
/// length, so their content is parsed like any other text and byte offsets stay valid.
pub fn unversion_comments(src: &str) -> String {
    let b = src.as_bytes();
    let mut out = b.to_vec();
    let mut i = 0usize;
    let mut quote: Option<u8> = None;
    let mut open = 0usize;
    while i < b.len() {
        let c = b[i];
        if let Some(q) = quote {
            if c == b'\\' && q != b'`' {
                i += 2;
                continue;
            }
            if c == q {
                if b.get(i + 1) == Some(&q) {
                    i += 2;
                    continue;
                }
                quote = None;
            }
            i += 1;
            continue;
        }
        match c {
            b'\'' | b'"' | b'`' => {
                quote = Some(c);
                i += 1;
            }
            b'/' if b.get(i + 1) == Some(&b'*') && b.get(i + 2) == Some(&b'!') => {
                let mut j = i + 3;
                while j < b.len() && b[j].is_ascii_digit() {
                    j += 1;
                }
                for x in &mut out[i..j] {
                    *x = b' ';
                }
                open += 1;
                i = j;
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                i = match src[i + 2..].find("*/") {
                    Some(p) => i + 2 + p + 2,
                    None => b.len(),
                };
            }
            b'*' if open > 0 && b.get(i + 1) == Some(&b'/') => {
                out[i] = b' ';
                out[i + 1] = b' ';
                open -= 1;
                i += 2;
            }
            _ => i += 1,
        }
    }
    String::from_utf8(out).unwrap_or_else(|_| src.to_string())
}

fn is_word_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '$' || !c.is_ascii()
}

/// Tokenize MySQL text (DDL or a view / trigger statement). Comments are skipped.
pub fn lex(src: &str) -> Vec<Tok> {
    let chars: Vec<(usize, char)> = src.char_indices().collect();
    let n = chars.len();
    let end_of = |i: usize| if i < n { chars[i].0 } else { src.len() };
    let mut out = Vec::new();
    let mut i = 0usize;
    let read_quoted = |start: usize, q: char| -> (String, usize) {
        // Returns (unescaped value, index after the closing quote).
        let mut v = String::new();
        let mut j = start + 1;
        while j < n {
            let c = chars[j].1;
            if c == '\\' && q != '`' && j + 1 < n {
                let e = chars[j + 1].1;
                match e {
                    '0' => v.push('\0'),
                    'n' => v.push('\n'),
                    'r' => v.push('\r'),
                    't' => v.push('\t'),
                    'b' => v.push('\u{8}'),
                    'Z' => v.push('\u{1a}'),
                    '%' | '_' => {
                        v.push('\\');
                        v.push(e);
                    }
                    other => v.push(other),
                }
                j += 2;
                continue;
            }
            if c == q {
                if j + 1 < n && chars[j + 1].1 == q {
                    v.push(q);
                    j += 2;
                    continue;
                }
                return (v, j + 1);
            }
            v.push(c);
            j += 1;
        }
        (v, n)
    };
    while i < n {
        let (pos, c) = chars[i];
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        if c == '/' && i + 1 < n && chars[i + 1].1 == '*' {
            let mut j = i + 2;
            while j + 1 < n && !(chars[j].1 == '*' && chars[j + 1].1 == '/') {
                j += 1;
            }
            i = (j + 2).min(n);
            continue;
        }
        if c == '#' || (c == '-' && i + 2 < n && chars[i + 1].1 == '-' && chars[i + 2].1.is_whitespace()) {
            while i < n && chars[i].1 != '\n' {
                i += 1;
            }
            continue;
        }
        if c == '`' || c == '\'' || c == '"' {
            let (v, j) = read_quoted(i, c);
            let k = if c == '`' { TK::Ident } else { TK::Str };
            out.push(Tok { k, v, s: pos, e: end_of(j) });
            i = j;
            continue;
        }
        if c.is_ascii_digit() || (c == '.' && i + 1 < n && chars[i + 1].1.is_ascii_digit()) {
            let mut j = i;
            if c == '0' && i + 1 < n && (chars[i + 1].1 == 'x' || chars[i + 1].1 == 'X') {
                j += 2;
                while j < n && chars[j].1.is_ascii_hexdigit() {
                    j += 1;
                }
            } else {
                while j < n && (chars[j].1.is_ascii_digit() || chars[j].1 == '.') {
                    j += 1;
                }
                if j < n && (chars[j].1 == 'e' || chars[j].1 == 'E') {
                    let mut k = j + 1;
                    if k < n && (chars[k].1 == '+' || chars[k].1 == '-') {
                        k += 1;
                    }
                    if k < n && chars[k].1.is_ascii_digit() {
                        while k < n && chars[k].1.is_ascii_digit() {
                            k += 1;
                        }
                        j = k;
                    }
                }
            }
            // A number glued to letters is a word (`1abc` names are legal).
            if j < n && is_word_char(chars[j].1) && !chars[j].1.is_ascii_digit() {
                while j < n && is_word_char(chars[j].1) {
                    j += 1;
                }
                out.push(Tok { k: TK::Word, v: src[pos..end_of(j)].to_string(), s: pos, e: end_of(j) });
            } else {
                out.push(Tok { k: TK::Num, v: src[pos..end_of(j)].to_string(), s: pos, e: end_of(j) });
            }
            i = j;
            continue;
        }
        if is_word_char(c) {
            let mut j = i;
            while j < n && is_word_char(chars[j].1) {
                j += 1;
            }
            let w = &src[pos..end_of(j)];
            let prefix = matches!(w, "b" | "B" | "x" | "X" | "n" | "N") || (w.starts_with('_') && w.len() > 1);
            if prefix && j < n && chars[j].1 == '\'' {
                let (_, k) = read_quoted(j, '\'');
                out.push(Tok { k: TK::Prefixed, v: src[pos..end_of(k)].to_string(), s: pos, e: end_of(k) });
                i = k;
                continue;
            }
            out.push(Tok { k: TK::Word, v: w.to_string(), s: pos, e: end_of(j) });
            i = j;
            continue;
        }
        out.push(Tok { k: TK::Punct, v: c.to_string(), s: pos, e: end_of(i + 1) });
        i += 1;
    }
    out
}

/// Index of the `)` matching the `(` at `open` (or `toks.len()` when unbalanced).
fn matching(toks: &[Tok], open: usize) -> usize {
    let mut depth = 0i32;
    for (i, t) in toks.iter().enumerate().skip(open) {
        if t.is_p('(') {
            depth += 1;
        } else if t.is_p(')') {
            depth -= 1;
            if depth == 0 {
                return i;
            }
        }
    }
    toks.len()
}

/// Split at depth-0 commas.
fn split_commas(toks: &[Tok]) -> Vec<&[Tok]> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut start = 0usize;
    for (i, t) in toks.iter().enumerate() {
        if t.is_p('(') {
            depth += 1;
        } else if t.is_p(')') {
            depth -= 1;
        } else if t.is_p(',') && depth == 0 {
            out.push(&toks[start..i]);
            start = i + 1;
        }
    }
    if start < toks.len() {
        out.push(&toks[start..]);
    }
    out
}

/// Verbatim text of a token run.
fn text_of<'a>(src: &'a str, toks: &[Tok]) -> &'a str {
    match (toks.first(), toks.last()) {
        (Some(a), Some(b)) => &src[a.s..b.e],
        _ => "",
    }
}

// =============================================================================================
// SHOW CREATE TABLE model and parser
// =============================================================================================

#[derive(Debug, Clone, PartialEq)]
pub enum MyDefault {
    Null,
    /// A quoted literal (decoded).
    Literal(String),
    /// A bare number (`-1`, `0.5`).
    Number(String),
    /// `CURRENT_TIMESTAMP[(fsp)]` (also `now()`, `localtimestamp`).
    CurrentTimestamp(Option<String>),
    /// `b'…'`, `x'…'`, `0x…`, `TRUE`, introducer literals: verbatim.
    Raw(String),
    /// `(expr)` (8.0.13+) or a MariaDB function default: verbatim, parentheses included.
    Expr(String),
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct MyColumn {
    pub name: String,
    /// Lower-case base type (`varchar`, `int`, `enum`, ...).
    pub ty: String,
    /// Numeric arguments (`["20"]`, `["8", "2"]`).
    pub args: Vec<String>,
    /// ENUM / SET values (decoded).
    pub values: Vec<String>,
    pub unsigned: bool,
    pub zerofill: bool,
    /// 4.0 `binary` attribute (`varchar(20) binary`).
    pub binary_attr: bool,
    pub charset: Option<String>,
    pub collation: Option<String>,
    pub not_null: bool,
    pub default: Option<MyDefault>,
    pub auto_increment: bool,
    /// `ON UPDATE CURRENT_TIMESTAMP[(fsp)]`: the fsp text, `Some("")` without one.
    pub on_update: Option<String>,
    pub comment: Option<String>,
    /// `GENERATED ALWAYS AS (expr) VIRTUAL|STORED`: (expr with parentheses, stored).
    pub generated: Option<(String, bool)>,
    pub invisible: bool,
    pub srid: Option<String>,
    /// Recognised attributes that are dropped (`COLUMN_FORMAT`, `STORAGE`, ...).
    pub dropped: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MyKeyKind {
    Primary,
    Unique,
    Index,
    Fulltext,
    Spatial,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct MyKeyPart {
    pub column: Option<String>,
    pub prefix: Option<u32>,
    pub desc: bool,
    /// Functional key part `(expr)` (8.0.13+), parentheses included.
    pub expr: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MyKey {
    pub kind: MyKeyKind,
    pub name: Option<String>,
    pub parts: Vec<MyKeyPart>,
    pub using: Option<String>,
    pub comment: Option<String>,
    pub invisible: bool,
    pub parser: Option<String>,
    pub key_block_size: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MyCheck {
    pub name: Option<String>,
    /// `(expr)` verbatim.
    pub expr: String,
    pub enforced: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MyFk {
    pub name: Option<String>,
    pub columns: Vec<String>,
    pub ref_db: Option<String>,
    pub ref_table: String,
    pub ref_columns: Vec<String>,
    pub on_delete: Option<String>,
    pub on_update: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MyOption {
    /// Normalised upper-case name: `ENGINE`, `TYPE`, `CHARSET`, `COLLATE`, `COMMENT`, ...
    pub name: String,
    /// Decoded value.
    pub value: String,
}

/// Parsed `SHOW CREATE TABLE` output.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct MyTable {
    pub name: String,
    pub columns: Vec<MyColumn>,
    pub keys: Vec<MyKey>,
    pub checks: Vec<MyCheck>,
    pub fks: Vec<MyFk>,
    pub options: Vec<MyOption>,
    /// `PARTITION BY …` verbatim (also from inside `/*!50100 … */`).
    pub partition: Option<String>,
    /// Clauses the parser does not recognise (`UNPARSED_CLAUSE`).
    pub unparsed: Vec<String>,
}

impl MyTable {
    pub fn option(&self, name: &str) -> Option<&str> {
        self.options.iter().find(|o| o.name.eq_ignore_ascii_case(name)).map(|o| o.value.as_str())
    }

    /// `ENGINE=` / 4.0 `TYPE=`.
    pub fn engine(&self) -> Option<&str> {
        self.option("ENGINE").or_else(|| self.option("TYPE"))
    }

    pub fn column(&self, name: &str) -> Option<&MyColumn> {
        self.columns.iter().find(|c| c.name.eq_ignore_ascii_case(name))
    }

    /// The PRIMARY KEY's columns.
    pub fn pk_columns(&self) -> Vec<String> {
        self.keys.iter().filter(|k| k.kind == MyKeyKind::Primary).flat_map(|k| k.parts.iter().filter_map(|p| p.column.clone())).collect()
    }
}

/// A cursor over tokens.
struct Cur<'a> {
    t: &'a [Tok],
    i: usize,
}

impl<'a> Cur<'a> {
    fn new(t: &'a [Tok]) -> Self {
        Cur { t, i: 0 }
    }
    fn peek(&self) -> Option<&'a Tok> {
        self.t.get(self.i)
    }
    fn peek_at(&self, k: usize) -> Option<&'a Tok> {
        self.t.get(self.i + k)
    }
    fn done(&self) -> bool {
        self.i >= self.t.len()
    }
    fn word(&mut self, w: &str) -> bool {
        if self.peek().is_some_and(|t| t.is_word(w)) {
            self.i += 1;
            true
        } else {
            false
        }
    }
    fn at_word(&self, w: &str) -> bool {
        self.peek().is_some_and(|t| t.is_word(w))
    }
    fn punct(&mut self, c: char) -> bool {
        if self.peek().is_some_and(|t| t.is_p(c)) {
            self.i += 1;
            true
        } else {
            false
        }
    }
    fn at_punct(&self, c: char) -> bool {
        self.peek().is_some_and(|t| t.is_p(c))
    }
    /// A name: identifier, word or string.
    fn name(&mut self) -> Option<String> {
        let t = self.peek()?;
        match t.k {
            TK::Ident | TK::Word | TK::Str => {
                self.i += 1;
                Some(t.v.clone())
            }
            _ => None,
        }
    }
    /// At `(`: the tokens strictly inside the group; advances past `)`.
    fn group(&mut self) -> Option<&'a [Tok]> {
        if !self.at_punct('(') {
            return None;
        }
        let close = matching(self.t, self.i);
        let inner = &self.t[self.i + 1..close.min(self.t.len())];
        self.i = (close + 1).min(self.t.len());
        Some(inner)
    }
    /// At `(`: the whole group including the parentheses; advances past `)`.
    fn group_full(&mut self) -> Option<&'a [Tok]> {
        if !self.at_punct('(') {
            return None;
        }
        let close = matching(self.t, self.i).min(self.t.len() - 1);
        let all = &self.t[self.i..=close];
        self.i = close + 1;
        Some(all)
    }
    fn rest(&self) -> &'a [Tok] {
        &self.t[self.i.min(self.t.len())..]
    }
}

/// Parse `SHOW CREATE TABLE` output of any version (3.4 "Parsing").
pub fn parse_create_table(text: &str) -> MyTable {
    let src = unversion_comments(text);
    let toks = lex(&src);
    let mut t = MyTable::default();
    let mut c = Cur::new(&toks);
    if !c.word("CREATE") {
        t.unparsed.push(short(text));
        return t;
    }
    c.word("TEMPORARY");
    if !c.word("TABLE") {
        t.unparsed.push(short(text));
        return t;
    }
    if c.word("IF") {
        c.word("NOT");
        c.word("EXISTS");
    }
    t.name = c.name().unwrap_or_default();
    if c.punct('.') {
        t.name = c.name().unwrap_or_default();
    }
    let Some(body) = c.group() else {
        t.unparsed.push(short(text));
        return t;
    };
    for def in split_commas(body) {
        parse_def(def, &src, &mut t);
    }
    parse_table_options(c.rest(), &src, &mut t);
    t
}

fn short(s: &str) -> String {
    let s = s.trim();
    if s.chars().count() > 120 {
        format!("{}…", s.chars().take(120).collect::<String>())
    } else {
        s.to_string()
    }
}

const KEY_WORDS: &[&str] = &["PRIMARY", "UNIQUE", "KEY", "INDEX", "FULLTEXT", "SPATIAL", "CONSTRAINT", "FOREIGN", "CHECK"];

fn parse_def(def: &[Tok], src: &str, t: &mut MyTable) {
    let Some(first) = def.first() else { return };
    let is_key = first.k == TK::Word && KEY_WORDS.iter().any(|w| first.is_word(w));
    if !is_key {
        parse_column(def, src, t);
        return;
    }
    let mut c = Cur::new(def);
    let mut cname: Option<String> = None;
    if c.word("CONSTRAINT") {
        if let Some(n) = c.peek() {
            let kw = n.k == TK::Word && ["PRIMARY", "UNIQUE", "FOREIGN", "CHECK"].iter().any(|w| n.is_word(w));
            if !kw {
                cname = c.name();
            }
        }
    }
    if c.word("PRIMARY") {
        c.word("KEY");
        parse_key(MyKeyKind::Primary, None, &mut c, src, t);
    } else if c.word("UNIQUE") {
        let _ = c.word("KEY") || c.word("INDEX");
        parse_key(MyKeyKind::Unique, cname, &mut c, src, t);
    } else if c.word("KEY") || c.word("INDEX") {
        parse_key(MyKeyKind::Index, None, &mut c, src, t);
    } else if c.word("FULLTEXT") {
        let _ = c.word("KEY") || c.word("INDEX");
        parse_key(MyKeyKind::Fulltext, None, &mut c, src, t);
    } else if c.word("SPATIAL") {
        let _ = c.word("KEY") || c.word("INDEX");
        parse_key(MyKeyKind::Spatial, None, &mut c, src, t);
    } else if c.word("FOREIGN") {
        c.word("KEY");
        parse_fk(cname, &mut c, src, t);
    } else if c.word("CHECK") {
        match c.group_full() {
            Some(g) => {
                let expr = text_of(src, g).to_string();
                let mut enforced = true;
                if c.word("NOT") && c.word("ENFORCED") {
                    enforced = false;
                } else {
                    c.word("ENFORCED");
                }
                if !c.done() {
                    t.unparsed.push(short(text_of(src, def)));
                }
                t.checks.push(MyCheck { name: cname, expr, enforced });
            }
            None => t.unparsed.push(short(text_of(src, def))),
        }
    } else {
        t.unparsed.push(short(text_of(src, def)));
    }
}

fn parse_key(kind: MyKeyKind, cname: Option<String>, c: &mut Cur<'_>, src: &str, t: &mut MyTable) {
    let mut key = MyKey { kind, name: None, parts: Vec::new(), using: None, comment: None, invisible: false, parser: None, key_block_size: None };
    if !c.at_punct('(') && !c.at_word("USING") {
        key.name = c.name();
    }
    if key.name.is_none() {
        key.name = cname;
    }
    if c.word("USING") {
        key.using = c.name().map(|s| s.to_ascii_uppercase());
    }
    let Some(parts) = c.group() else {
        t.unparsed.push(short(text_of(src, c.t)));
        return;
    };
    for p in split_commas(parts) {
        let mut pc = Cur::new(p);
        let mut part = MyKeyPart::default();
        if pc.at_punct('(') {
            part.expr = pc.group_full().map(|g| text_of(src, g).to_string());
        } else {
            part.column = pc.name();
            if let Some(g) = pc.group() {
                part.prefix = g.first().and_then(|x| x.v.parse().ok());
            }
        }
        if pc.word("DESC") {
            part.desc = true;
        } else {
            pc.word("ASC");
        }
        if !pc.done() {
            t.unparsed.push(short(text_of(src, p)));
        }
        key.parts.push(part);
    }
    while !c.done() {
        if c.word("USING") {
            key.using = c.name().map(|s| s.to_ascii_uppercase());
        } else if c.word("KEY_BLOCK_SIZE") {
            c.punct('=');
            key.key_block_size = c.name().or_else(|| c.peek().map(|x| x.v.clone()));
            if key.key_block_size.is_some() && c.peek().is_some_and(|x| x.k == TK::Num) {
                c.i += 1;
            }
        } else if c.word("WITH") {
            c.word("PARSER");
            key.parser = c.name();
        } else if c.word("COMMENT") {
            key.comment = c.name();
        } else if c.word("INVISIBLE") {
            key.invisible = true;
        } else if c.word("VISIBLE") {
        } else {
            t.unparsed.push(short(text_of(src, c.rest())));
            break;
        }
    }
    t.keys.push(key);
}

fn ident_list(g: &[Tok]) -> Vec<String> {
    split_commas(g).iter().filter_map(|p| p.first().map(|x| x.v.clone())).collect()
}

fn parse_fk(cname: Option<String>, c: &mut Cur<'_>, src: &str, t: &mut MyTable) {
    let mut name = cname;
    if !c.at_punct('(') {
        let n = c.name();
        if name.is_none() {
            name = n;
        }
    }
    let Some(cols) = c.group() else {
        t.unparsed.push(short(text_of(src, c.t)));
        return;
    };
    if !c.word("REFERENCES") {
        t.unparsed.push(short(text_of(src, c.t)));
        return;
    }
    let mut ref_db = None;
    let mut ref_table = c.name().unwrap_or_default();
    if c.punct('.') {
        ref_db = Some(ref_table);
        ref_table = c.name().unwrap_or_default();
    }
    let ref_columns = c.group().map(ident_list).unwrap_or_default();
    let mut fk = MyFk { name, columns: ident_list(cols), ref_db, ref_table, ref_columns, on_delete: None, on_update: None };
    while !c.done() {
        if c.word("MATCH") {
            c.name();
        } else if c.word("ON") {
            let delete = c.word("DELETE");
            if !delete {
                c.word("UPDATE");
            }
            let action = if c.word("SET") {
                format!("SET {}", c.name().unwrap_or_default().to_ascii_uppercase())
            } else if c.word("NO") {
                c.word("ACTION");
                "NO ACTION".to_string()
            } else {
                c.name().unwrap_or_default().to_ascii_uppercase()
            };
            if delete {
                fk.on_delete = Some(action);
            } else {
                fk.on_update = Some(action);
            }
        } else {
            t.unparsed.push(short(text_of(src, c.rest())));
            break;
        }
    }
    t.fks.push(fk);
}

const TYPE_WORDS: &[&str] = &[
    "tinyint",
    "smallint",
    "mediumint",
    "int",
    "integer",
    "bigint",
    "bool",
    "boolean",
    "bit",
    "decimal",
    "numeric",
    "dec",
    "fixed",
    "float",
    "double",
    "real",
    "char",
    "varchar",
    "binary",
    "varbinary",
    "tinytext",
    "text",
    "mediumtext",
    "longtext",
    "tinyblob",
    "blob",
    "mediumblob",
    "longblob",
    "enum",
    "set",
    "date",
    "time",
    "datetime",
    "timestamp",
    "year",
    "json",
    "geometry",
    "point",
    "linestring",
    "polygon",
    "multipoint",
    "multilinestring",
    "multipolygon",
    "geometrycollection",
    "geomcollection",
    "serial",
];

fn parse_column(def: &[Tok], src: &str, t: &mut MyTable) {
    let mut c = Cur::new(def);
    let name = c.name().unwrap_or_default();
    let mut col = MyColumn { name, ..Default::default() };
    let Some(ty) = c.peek().filter(|x| x.k == TK::Word) else {
        t.unparsed.push(short(text_of(src, def)));
        return;
    };
    c.i += 1;
    col.ty = ty.v.to_ascii_lowercase();
    if col.ty == "double" {
        c.word("PRECISION");
    }
    if col.ty == "long" {
        if c.word("VARCHAR") {
            col.ty = "mediumtext".into();
        } else if c.word("VARBINARY") {
            col.ty = "mediumblob".into();
        } else {
            col.ty = "mediumtext".into();
        }
    }
    if col.ty == "national" {
        col.ty = c.name().unwrap_or_default().to_ascii_lowercase();
    }
    if col.ty == "character" {
        col.ty = if c.word("VARYING") { "varchar".into() } else { "char".into() };
    }
    if let Some(g) = c.group() {
        for a in split_commas(g) {
            match a.first() {
                Some(x) if x.k == TK::Str => col.values.push(x.v.clone()),
                Some(_) => col.args.push(text_of(src, a).trim().to_string()),
                None => {}
            }
        }
    }
    while !c.done() {
        if c.word("UNSIGNED") {
            col.unsigned = true;
        } else if c.word("SIGNED") {
        } else if c.word("ZEROFILL") {
            col.zerofill = true;
        } else if c.word("BINARY") {
            col.binary_attr = true;
        } else if c.word("ASCII") {
            col.charset = Some("latin1".into());
        } else if c.word("UNICODE") {
            col.charset = Some("ucs2".into());
        } else if c.word("CHARACTER") {
            c.word("SET");
            col.charset = c.name();
        } else if c.word("CHARSET") {
            col.charset = c.name();
        } else if c.word("COLLATE") {
            col.collation = c.name();
        } else if c.word("NOT") {
            if c.word("NULL") {
                col.not_null = true;
            } else if c.word("SECONDARY") {
                col.dropped.push("NOT SECONDARY".into());
            } else {
                t.unparsed.push(format!("column `{}`: {}", col.name, short(text_of(src, c.rest()))));
                break;
            }
        } else if c.word("NULL") {
            col.not_null = false;
        } else if c.word("DEFAULT") {
            match parse_default(&mut c, src) {
                Some(d) => col.default = Some(d),
                None => {
                    t.unparsed.push(format!("column `{}`: {}", col.name, short(text_of(src, c.rest()))));
                    break;
                }
            }
        } else if c.word("AUTO_INCREMENT") {
            col.auto_increment = true;
        } else if c.at_word("ON") && c.peek_at(1).is_some_and(|x| x.is_word("UPDATE")) {
            c.i += 2;
            let w = c.name().unwrap_or_default().to_ascii_lowercase();
            if !["current_timestamp", "now", "localtimestamp", "localtime"].contains(&w.as_str()) {
                t.unparsed.push(format!("column `{}`: ON UPDATE {w}", col.name));
                break;
            }
            let fsp = c.group().map(|g| text_of(src, g).trim().to_string()).unwrap_or_default();
            col.on_update = Some(fsp);
        } else if c.word("COMMENT") {
            col.comment = c.name();
        } else if c.word("GENERATED") {
            c.word("ALWAYS");
            if !c.word("AS") {
                t.unparsed.push(format!("column `{}`: {}", col.name, short(text_of(src, c.rest()))));
                break;
            }
            let expr = c.group_full().map(|g| text_of(src, g).to_string()).unwrap_or_default();
            col.generated = Some((expr, false));
        } else if c.at_word("AS") && c.peek_at(1).is_some_and(|x| x.is_p('(')) {
            c.i += 1;
            let expr = c.group_full().map(|g| text_of(src, g).to_string()).unwrap_or_default();
            col.generated = Some((expr, false));
        } else if c.word("VIRTUAL") {
        } else if c.word("STORED") || c.word("PERSISTENT") {
            if let Some(g) = col.generated.as_mut() {
                g.1 = true;
            }
        } else if c.word("INVISIBLE") {
            col.invisible = true;
        } else if c.word("VISIBLE") {
        } else if c.word("SRID") {
            col.srid = c.peek().map(|x| x.v.clone());
            c.i += 1;
        } else if c.word("COLUMN_FORMAT") || c.word("STORAGE") {
            let v = c.name().unwrap_or_default();
            col.dropped.push(v);
        } else if c.word("ENGINE_ATTRIBUTE") || c.word("SECONDARY_ENGINE_ATTRIBUTE") {
            c.punct('=');
            c.name();
            col.dropped.push("ENGINE_ATTRIBUTE".into());
        } else if c.word("PRIMARY") {
            c.word("KEY");
            t.keys.push(MyKey {
                kind: MyKeyKind::Primary,
                name: None,
                parts: vec![MyKeyPart { column: Some(col.name.clone()), ..Default::default() }],
                using: None,
                comment: None,
                invisible: false,
                parser: None,
                key_block_size: None,
            });
        } else if c.word("UNIQUE") {
            c.word("KEY");
            t.keys.push(MyKey {
                kind: MyKeyKind::Unique,
                name: Some(col.name.clone()),
                parts: vec![MyKeyPart { column: Some(col.name.clone()), ..Default::default() }],
                using: None,
                comment: None,
                invisible: false,
                parser: None,
                key_block_size: None,
            });
        } else if c.word("CHECK") {
            let expr = c.group_full().map(|g| text_of(src, g).to_string()).unwrap_or_default();
            t.checks.push(MyCheck { name: None, expr, enforced: true });
        } else {
            t.unparsed.push(format!("column `{}`: {}", col.name, short(text_of(src, c.rest()))));
            break;
        }
    }
    if !TYPE_WORDS.contains(&col.ty.as_str()) {
        t.unparsed.push(format!("column `{}`: unknown type {}", col.name, col.ty));
    }
    t.columns.push(col);
}

fn parse_default(c: &mut Cur<'_>, src: &str) -> Option<MyDefault> {
    let tok = c.peek()?;
    match tok.k {
        TK::Str => {
            c.i += 1;
            Some(MyDefault::Literal(tok.v.clone()))
        }
        TK::Num => {
            c.i += 1;
            Some(MyDefault::Number(tok.v.clone()))
        }
        TK::Prefixed => {
            c.i += 1;
            Some(MyDefault::Raw(tok.v.clone()))
        }
        TK::Punct if tok.is_p('-') || tok.is_p('+') => {
            let n = c.peek_at(1).filter(|x| x.k == TK::Num)?;
            c.i += 2;
            Some(MyDefault::Number(format!("{}{}", if tok.is_p('-') { "-" } else { "" }, n.v)))
        }
        TK::Punct if tok.is_p('(') => {
            let g = c.group_full()?;
            Some(MyDefault::Expr(text_of(src, g).to_string()))
        }
        TK::Word => {
            c.i += 1;
            let w = tok.v.to_ascii_lowercase();
            match w.as_str() {
                "null" => Some(MyDefault::Null),
                "current_timestamp" | "now" | "localtimestamp" | "localtime" => {
                    let fsp = c.group().map(|g| text_of(src, g).trim().to_string()).filter(|s| !s.is_empty());
                    Some(MyDefault::CurrentTimestamp(fsp))
                }
                _ if c.at_punct('(') => {
                    let g = c.group_full()?;
                    Some(MyDefault::Expr(format!("({}{})", tok.v, text_of(src, g))))
                }
                _ => Some(MyDefault::Raw(tok.v.clone())),
            }
        }
        _ => None,
    }
}

const KNOWN_TABLE_OPTIONS: &[&str] = &[
    "ENGINE",
    "TYPE",
    "AUTO_INCREMENT",
    "CHARSET",
    "COLLATE",
    "COMMENT",
    "ROW_FORMAT",
    "KEY_BLOCK_SIZE",
    "COMPRESSION",
    "STATS_PERSISTENT",
    "STATS_AUTO_RECALC",
    "STATS_SAMPLE_PAGES",
    "MAX_ROWS",
    "MIN_ROWS",
    "AVG_ROW_LENGTH",
    "PACK_KEYS",
    "DELAY_KEY_WRITE",
    "CHECKSUM",
    "TABLE_CHECKSUM",
    "RAID_TYPE",
    "RAID_CHUNKS",
    "RAID_CHUNKSIZE",
    "UNION",
    "INSERT_METHOD",
    "CONNECTION",
    "ENCRYPTION",
    "TABLESPACE",
    "STORAGE",
    "DATA DIRECTORY",
    "INDEX DIRECTORY",
    "PAGE_CHECKSUM",
    "TRANSACTIONAL",
    "PAGE_COMPRESSED",
    "PAGE_COMPRESSION_LEVEL",
    "AUTOEXTEND_SIZE",
    "SECONDARY_ENGINE",
    "SECONDARY_ENGINE_ATTRIBUTE",
    "ENGINE_ATTRIBUTE",
];

fn parse_table_options(toks: &[Tok], src: &str, t: &mut MyTable) {
    let mut c = Cur::new(toks);
    while !c.done() {
        if c.punct(',') || c.punct(';') {
            continue;
        }
        if c.word("DEFAULT") {
            continue;
        }
        if c.at_word("PARTITION") {
            let start = c.peek().map(|x| x.s).unwrap_or(src.len());
            t.partition = Some(src[start..].trim().to_string());
            return;
        }
        let Some(tok) = c.peek().filter(|x| x.k == TK::Word) else {
            t.unparsed.push(short(text_of(src, c.rest())));
            return;
        };
        c.i += 1;
        let mut name = tok.v.to_ascii_uppercase();
        if name == "CHARACTER" {
            c.word("SET");
            name = "CHARSET".into();
        } else if (name == "DATA" || name == "INDEX") && c.word("DIRECTORY") {
            name = format!("{name} DIRECTORY");
        }
        if !KNOWN_TABLE_OPTIONS.contains(&name.as_str()) {
            t.unparsed.push(short(text_of(src, &toks[c.i - 1..])));
            return;
        }
        c.punct('=');
        let value = if c.at_punct('(') {
            c.group_full().map(|g| text_of(src, g).to_string()).unwrap_or_default()
        } else {
            match c.peek() {
                Some(v) if v.k != TK::Punct => {
                    c.i += 1;
                    v.v.clone()
                }
                _ => String::new(),
            }
        };
        t.options.push(MyOption { name, value });
    }
}

// =============================================================================================
// Types and sizes
// =============================================================================================

/// Normalised charset name (`euc_kr` -> `euckr`, `utf8mb3` -> `utf8`).
pub fn norm_cs(cs: &str) -> String {
    match cs.trim().to_ascii_lowercase().as_str() {
        "euc_kr" | "euc-kr" => "euckr".into(),
        "utf8mb3" | "utf-8" => "utf8".into(),
        other => other.to_string(),
    }
}

/// Bytes per character of a charset (maximum).
pub fn cs_bpc(cs: &str) -> u64 {
    match norm_cs(cs).as_str() {
        "utf8mb4" | "utf16" | "utf16le" | "utf32" | "gb18030" => 4,
        "utf8" | "ujis" | "eucjpms" => 3,
        "euckr" | "big5" | "gbk" | "gb2312" | "sjis" | "cp932" | "ucs2" | "cp949" => 2,
        _ => 1,
    }
}

/// The charset part of a collation name.
pub fn coll_charset(coll: &str) -> String {
    let c = coll.to_ascii_lowercase();
    if c == "binary" {
        return "binary".into();
    }
    c.split_once('_').map(|(a, _)| norm_cs(a)).unwrap_or(c)
}

/// What a collation compares like (the part after the charset): `general_ci`, `0900_ai_ci`, `bin`.
fn coll_family(coll: &str) -> String {
    let c = coll.to_ascii_lowercase();
    c.split_once('_').map(|(_, r)| r.to_string()).unwrap_or(c)
}

fn is_no_pad(coll: &str) -> bool {
    let c = coll.to_ascii_lowercase();
    c.contains("_0900_") || c.contains("nopad")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Class {
    Int(u64),
    Bit,
    Decimal,
    Float,
    Double,
    Char,
    Varchar,
    Binary,
    Varbinary,
    /// 0 tiny, 1 text, 2 medium, 3 long
    Text(u8),
    Blob(u8),
    Enum,
    Set,
    Date,
    Time,
    DateTime,
    Timestamp,
    Year,
    Json,
    Geometry,
    Unknown,
}

fn class_of(ty: &str) -> Class {
    match ty {
        "tinyint" | "bool" | "boolean" => Class::Int(1),
        "smallint" => Class::Int(2),
        "mediumint" => Class::Int(3),
        "int" | "integer" => Class::Int(4),
        "bigint" | "serial" => Class::Int(8),
        "bit" => Class::Bit,
        "decimal" | "numeric" | "dec" | "fixed" => Class::Decimal,
        "float" => Class::Float,
        "double" | "real" => Class::Double,
        "char" => Class::Char,
        "varchar" => Class::Varchar,
        "binary" => Class::Binary,
        "varbinary" => Class::Varbinary,
        "tinytext" => Class::Text(0),
        "text" => Class::Text(1),
        "mediumtext" => Class::Text(2),
        "longtext" => Class::Text(3),
        "tinyblob" => Class::Blob(0),
        "blob" => Class::Blob(1),
        "mediumblob" => Class::Blob(2),
        "longblob" => Class::Blob(3),
        "enum" => Class::Enum,
        "set" => Class::Set,
        "date" => Class::Date,
        "time" => Class::Time,
        "datetime" => Class::DateTime,
        "timestamp" => Class::Timestamp,
        "year" => Class::Year,
        "json" => Class::Json,
        "geometry" | "point" | "linestring" | "polygon" | "multipoint" | "multilinestring" | "multipolygon" | "geometrycollection" | "geomcollection" => Class::Geometry,
        _ => Class::Unknown,
    }
}

const TEXT_NAMES: [&str; 4] = ["tinytext", "text", "mediumtext", "longtext"];
const BLOB_NAMES: [&str; 4] = ["tinyblob", "blob", "mediumblob", "longblob"];
const LOB_BYTES: [u64; 4] = [255, 65_535, 16_777_215, 4_294_967_295];

fn is_temporal(c: Class) -> bool {
    matches!(c, Class::Date | Class::DateTime | Class::Timestamp | Class::Year)
}

fn is_text(c: Class) -> bool {
    matches!(c, Class::Char | Class::Varchar | Class::Text(_) | Class::Enum | Class::Set)
}

fn decimal_bytes(m: u64, d: u64) -> u64 {
    const LEFT: [u64; 10] = [0, 1, 1, 2, 2, 3, 3, 4, 4, 4];
    let part = |digits: u64| (digits / 9) * 4 + LEFT[(digits % 9) as usize];
    part(m.saturating_sub(d)) + part(d)
}

fn fsp_bytes(fsp: u64) -> u64 {
    fsp.div_ceil(2)
}

/// Target size of one column: (max bytes, variable length, LOB pointer bytes for the 65,535 rule).
#[derive(Debug, Clone, Copy)]
struct Size {
    max: u64,
    var: bool,
    lob: Option<u64>,
}

// =============================================================================================
// Translation of one table
// =============================================================================================

/// Per-table overrides (3.3 `table_overrides`, `columns`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MyTableOverrides {
    pub charset: Option<String>,
    pub collation: Option<String>,
    /// column -> treatment
    pub columns: BTreeMap<String, ColumnTreatment>,
    /// TIMESTAMP DEFAULT literals of sources < 4.1.3, converted to UTC by the planner.
    pub timestamp_defaults_utc: BTreeMap<String, String>,
}

/// A secondary key added after the data (one combined `ALTER TABLE` per table, 3.3).
#[derive(Debug, Clone, PartialEq)]
pub struct MyPostKey {
    pub name: String,
    pub clause: String,
    pub unique: bool,
    /// FULLTEXT on InnoDB: InnoDB builds one FULLTEXT index per ALTER.
    pub separate: bool,
    pub warnings: Vec<CopyIssue>,
}

/// A foreign key, translated (`ADD CONSTRAINT …`).
#[derive(Debug, Clone, PartialEq)]
pub struct MyFkOut {
    pub name: String,
    pub clause: String,
    pub columns: Vec<String>,
    /// Source database of the parent (`None` = the copied database).
    pub ref_db: Option<String>,
    /// Source parent table.
    pub ref_table: String,
    pub ref_columns: Vec<String>,
}

/// The translation of one table for one target.
#[derive(Debug, Clone, PartialEq)]
pub struct MyTableOut {
    /// Source engine (normalised) and the chosen target engine.
    pub source_engine: String,
    pub engine: String,
    /// MERGE / FEDERATED: the table is `unsupported`.
    pub unsupported: Option<CopyIssue>,
    pub create_sql: String,
    /// MyISAM with non-unique keys and data: `DISABLE KEYS` before the load, `ENABLE KEYS` after.
    pub disable_keys: bool,
    pub post_keys: Vec<MyPostKey>,
    pub fks: Vec<MyFkOut>,
    pub columns: Vec<DataColumn>,
    /// The data SELECT (3.2).
    pub select_sql: String,
    /// The target read-back for checksum verification.
    pub verify_sql: String,
    pub first_timestamp_auto: bool,
    pub auto_increment: Option<String>,
    pub charset: String,
    pub collation: Option<String>,
    /// PRIMARY / UNIQUE keys over plain columns (target names), for 8.4's FK rule.
    pub unique_keys: Vec<Vec<String>>,
    pub has_text_lob: bool,
    pub warnings: Vec<CopyIssue>,
    pub blockers: Vec<CopyIssue>,
    /// `SHOW CREATE TABLE` with FEDERATED `CONNECTION` values redacted.
    pub source_ddl: String,
}

/// FEDERATED `CONNECTION='mysql://user:pass@host/db/t'` (and any other CONNECTION) redacted.
pub fn redact_connection(ddl: &str) -> String {
    let re = regex::Regex::new(r"(?i)(CONNECTION\s*=\s*)'(?:[^'\\]|\\.|'')*'").expect("static regex");
    re.replace_all(ddl, "$1'…'").into_owned()
}

fn norm_engine(e: &str) -> String {
    match e.trim().to_ascii_uppercase().as_str() {
        "INNODB" | "INNOBASE" => "InnoDB".into(),
        "MYISAM" => "MyISAM".into(),
        "HEAP" | "MEMORY" => "MEMORY".into(),
        "MRG_MYISAM" | "MERGE" | "MRG_ISAM" => "MRG_MYISAM".into(),
        "ISAM" => "ISAM".into(),
        "BDB" | "BERKELEYDB" => "BDB".into(),
        "NDB" | "NDBCLUSTER" => "ndbcluster".into(),
        "FEDERATED" => "FEDERATED".into(),
        "ARCHIVE" => "ARCHIVE".into(),
        "CSV" => "CSV".into(),
        "BLACKHOLE" => "BLACKHOLE".into(),
        "ARIA" => "Aria".into(),
        "" => "MyISAM".into(),
        other => other.to_string(),
    }
}

fn is_zero_temporal(v: &str) -> bool {
    let digits: String = v.chars().filter(|c| c.is_ascii_digit()).collect();
    !digits.is_empty() && digits.chars().all(|c| c == '0')
}

/// A literal with a zero month or day (`'2020-00-15'`), not all zero.
fn partial_zero_fix(v: &str) -> Option<String> {
    let (date, rest) = v.split_at(v.find(' ').unwrap_or(v.len()));
    let mut parts: Vec<String> = date.split('-').map(str::to_string).collect();
    if parts.len() != 3 || is_zero_temporal(v) {
        return None;
    }
    let mut changed = false;
    for p in parts.iter_mut().skip(1) {
        if p.chars().all(|c| c == '0') {
            *p = "01".into();
            changed = true;
        }
    }
    changed.then(|| format!("{}{}", parts.join("-"), rest))
}

fn min_literal(c: Class) -> String {
    match c {
        Class::Date => "'1000-01-01'".into(),
        Class::DateTime => "'1000-01-01 00:00:00'".into(),
        Class::Timestamp => format!("'{}'", crate::copy::data::values::timestamp_min_literal("+00:00")),
        Class::Year => "1901".into(),
        _ => "NULL".into(),
    }
}

fn zero_literal(c: Class) -> String {
    match c {
        Class::Date => "'0000-00-00'".into(),
        Class::DateTime | Class::Timestamp => "'0000-00-00 00:00:00'".into(),
        Class::Year => "0000".into(),
        _ => "NULL".into(),
    }
}

/// Fold for ENUM duplicate detection under a case/width/accent-insensitive collation.
fn ci_fold(s: &str, no_pad: bool) -> String {
    let mapped: String = s
        .chars()
        .map(|c| match c {
            '\u{FF01}'..='\u{FF5E}' => char::from_u32(c as u32 - 0xFEE0).unwrap_or(c),
            '\u{3000}' => ' ',
            'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' | 'À' | 'Á' | 'Â' | 'Ã' | 'Ä' | 'Å' => 'a',
            'è' | 'é' | 'ê' | 'ë' | 'È' | 'É' | 'Ê' | 'Ë' => 'e',
            'ì' | 'í' | 'î' | 'ï' | 'Ì' | 'Í' | 'Î' | 'Ï' => 'i',
            'ò' | 'ó' | 'ô' | 'õ' | 'ö' | 'Ò' | 'Ó' | 'Ô' | 'Õ' | 'Ö' => 'o',
            'ù' | 'ú' | 'û' | 'ü' | 'Ù' | 'Ú' | 'Û' | 'Ü' => 'u',
            'ñ' | 'Ñ' => 'n',
            'ç' | 'Ç' => 'c',
            other => other,
        })
        .flat_map(char::to_lowercase)
        .collect();
    if no_pad {
        mapped
    } else {
        mapped.trim_end_matches(' ').to_string()
    }
}

/// Translate one parsed table for the target (3.3, 3.4). `target_name` is the target table
/// name (renames applied); `renamed` maps source table -> target table for FK parents.
pub fn translate_table(t: &MyTable, target_name: &str, src: &MySourceCtx, tgt: &MyTargetCtx, ov: &MyTableOverrides, renamed: &BTreeMap<String, String>) -> MyTableOut {
    let mut warnings: Vec<CopyIssue> = Vec::new();
    let mut blockers: Vec<CopyIssue> = Vec::new();
    let mut dropped_opts: Vec<String> = Vec::new();

    // ---- engine
    let source_engine = norm_engine(t.engine().unwrap_or("MyISAM"));
    let mut unsupported = None;
    if source_engine == "MRG_MYISAM" || source_engine == "FEDERATED" {
        unsupported = Some(warning("UNSUPPORTED_OBJECT", format!("{source_engine} tables are not copied")).detail(format!("table `{}`", t.name)));
    }
    let default_engine = if tgt.innodb_default() { "InnoDB" } else { "MyISAM" };
    let mut engine = match tgt.engine {
        MysqlEngineChoice::Innodb => "InnoDB".to_string(),
        MysqlEngineChoice::Myisam => "MyISAM".to_string(),
        MysqlEngineChoice::Auto if tgt.innodb_default() => "InnoDB".to_string(),
        MysqlEngineChoice::Auto | MysqlEngineChoice::Keep => match source_engine.as_str() {
            "ISAM" | "BDB" => default_engine.to_string(),
            other => other.to_string(),
        },
    };
    let has_fulltext = t.keys.iter().any(|k| k.kind == MyKeyKind::Fulltext);
    let has_spatial = t.keys.iter().any(|k| k.kind == MyKeyKind::Spatial);
    if engine == "InnoDB" && ((has_fulltext && !tgt.fulltext_innodb()) || (has_spatial && !tgt.spatial_innodb())) {
        engine = "MyISAM".into();
        warnings.push(warning("ENGINE_KEPT_MYISAM", "the table keeps MyISAM: InnoDB on this target has no FULLTEXT (before 5.6) or SPATIAL (before 5.7) indexes"));
    }
    // AUTO_INCREMENT column and its key.
    let ai_col = t.columns.iter().find(|c| c.auto_increment).map(|c| c.name.clone());
    let ai_leads = |k: &MyKey| k.parts.first().and_then(|p| p.column.as_deref()).is_some_and(|c| Some(c) == ai_col.as_deref());
    let grouped_ai = ai_col.is_some() && !t.keys.iter().any(|k| ai_leads(k) && k.kind != MyKeyKind::Fulltext);
    if grouped_ai && engine != "MyISAM" && (source_engine == "MyISAM" || source_engine == "BDB") {
        engine = "MyISAM".into();
        warnings.push(warning("AUTOINC_GROUPED", "AUTO_INCREMENT on a non-leading key column (per-group counters) has no InnoDB equivalent: the table keeps MyISAM"));
    }
    if engine != source_engine && unsupported.is_none() {
        warnings.push(info("ENGINE_CHANGED", format!("engine {source_engine} becomes {engine}")));
    }
    let innodb = engine == "InnoDB";
    let myisam = engine == "MyISAM";

    // ---- charset / collation
    let cs = norm_cs(ov.charset.as_deref().unwrap_or(&tgt.charset));
    let bpc = cs_bpc(&cs);
    let eff_default = ov.collation.clone().or_else(|| tgt.user_collation.clone()).or_else(|| tgt.default_collation(&cs));
    let src_table_cs = t
        .option("CHARSET")
        .map(norm_cs)
        .or_else(|| t.option("COLLATE").map(coll_charset))
        .or_else(|| src.server_charset.as_deref().map(norm_cs))
        .unwrap_or_else(|| "latin1".into());
    let src_table_coll = t.option("COLLATE").map(str::to_string);
    let map_coll = |src_coll: Option<&str>, binary_attr: bool, warn: &mut Vec<CopyIssue>| -> Option<String> {
        if cs == "binary" {
            return None;
        }
        let utf = cs == "utf8mb4" || cs == "utf8";
        if binary_attr || src_coll.is_some_and(|c| c.to_ascii_lowercase().ends_with("_bin")) {
            return Some(format!("{cs}_bin"));
        }
        if let Some(c) = src_coll.map(|c| c.to_ascii_lowercase()) {
            let ccs = coll_charset(&c);
            let fam = coll_family(&c);
            if ccs == "utf8" || ccs == "utf8mb4" {
                if !utf {
                    return eff_default.clone();
                }
                if fam.starts_with("0900_") {
                    if cs == "utf8mb4" && tgt.uca0900() {
                        return Some(c);
                    }
                    warn.push(
                        warning("DOWNGRADE", format!("collation {c} does not exist on this target"))
                            .detail("a utf8mb4_0900 collation becomes unicode_520_ci (unicode_ci before 5.6)"),
                    );
                    return Some(if tgt.unicode520() { format!("{cs}_unicode_520_ci") } else { format!("{cs}_unicode_ci") });
                }
                if fam.starts_with("unicode_520") && !tgt.unicode520() {
                    return Some(format!("{cs}_unicode_ci"));
                }
                return Some(format!("{cs}_{fam}"));
            }
            if fam.ends_with("_cs") {
                return Some(if cs == "utf8mb4" && tgt.uca0900() { "utf8mb4_0900_as_cs".to_string() } else { format!("{cs}_bin") });
            }
        }
        eff_default.clone()
    };
    let mut scratch = Vec::new();
    let table_coll = map_coll(src_table_coll.as_deref(), false, &mut scratch);
    warnings.append(&mut scratch);

    // ---- keys facts
    let pk_cols: BTreeSet<String> = t.pk_columns().into_iter().map(|c| c.to_ascii_lowercase()).collect();
    let unique_cols: BTreeSet<String> =
        t.keys.iter().filter(|k| k.kind == MyKeyKind::Unique).flat_map(|k| k.parts.iter().filter_map(|p| p.column.as_ref().map(|c| c.to_ascii_lowercase()))).collect();
    let key_cols: BTreeSet<String> = t.keys.iter().flat_map(|k| k.parts.iter().filter_map(|p| p.column.as_ref().map(|c| c.to_ascii_lowercase()))).collect();

    // ---- columns
    struct ColOut {
        name: String,
        class: Class,
        type_sql: String,
        chars: Option<u64>,
        text_bpc: u64,
        size: Size,
        nullable: bool,
        generated: bool,
        coll_changed: bool,
        target_coll: Option<String>,
    }
    let mut cols: Vec<ColOut> = Vec::new();
    let mut data_cols: Vec<DataColumn> = Vec::new();
    let mut coll_changed_cols: Vec<String> = Vec::new();
    let mut pad_note = false;
    let mut zero_cols: Vec<String> = Vec::new();
    let mut zero_in_key: Vec<String> = Vec::new();
    let mut text_promoted: Vec<String> = Vec::new();
    let mut varchar_to_text: Vec<String> = Vec::new();
    let mut decimal_widened: Vec<String> = Vec::new();
    let mut deprecated: Vec<String> = Vec::new();
    let mut default_dropped: Vec<String> = Vec::new();
    let mut float_text: Vec<String> = Vec::new();
    let mut year2: Vec<String> = Vec::new();
    let mut col_dropped_attrs: Vec<String> = Vec::new();
    let mut first_ts_seen = false;
    let mut first_timestamp_auto = false;
    let mut has_timestamp = false;
    let mut has_text_lob = false;
    let zero = tgt.zero;

    for col in &t.columns {
        let lname = col.name.to_ascii_lowercase();
        let mut class = class_of(&col.ty);
        let treat = ov.columns.iter().find(|(k, _)| k.eq_ignore_ascii_case(&col.name)).map(|(_, v)| *v);
        let n: u64 = col.args.first().and_then(|a| a.parse().ok()).unwrap_or(1);
        // Source text charset of this column (sizing).
        let col_src_cs = col.charset.as_deref().map(norm_cs).or_else(|| col.collation.as_deref().map(coll_charset)).unwrap_or_else(|| src_table_cs.clone());
        let src_bpc = cs_bpc(&col_src_cs);
        let charset_binary = col_src_cs == "binary" && is_text(class);
        // Binary treatment: CHARACTER SET binary, or treat_as binary.
        if charset_binary || (treat == Some(ColumnTreatment::Binary) && is_text(class)) {
            class = match class {
                // A CHARACTER SET binary CHAR is a BINARY(n) on the source too. treat_as binary
                // keeps the bytes the source CHAR returns (trailing spaces stripped): BINARY(n)
                // would pad them with 0x00 and change the value, VARBINARY(n) stores them as is.
                Class::Char if charset_binary => Class::Binary,
                Class::Char => Class::Varbinary,
                Class::Varchar => Class::Varbinary,
                Class::Text(k) => Class::Blob(k),
                other => other,
            };
        } else if treat == Some(ColumnTreatment::Text) {
            class = match class {
                Class::Binary => Class::Char,
                Class::Varbinary => Class::Varchar,
                Class::Blob(k) => Class::Text(k),
                other => other,
            };
        }
        let mut type_sql: String;
        let mut chars = None;
        let mut size = Size { max: 0, var: false, lob: None };
        let width_kept = |w: &str| -> bool { !tgt.drops_display_width() || col.zerofill || (col.ty == "tinyint" && w == "1") || col.ty == "bool" || col.ty == "boolean" };
        match class {
            Class::Int(bytes) => {
                let base = if col.ty == "integer" {
                    "int"
                } else if col.ty == "bool" || col.ty == "boolean" {
                    "tinyint"
                } else if col.ty == "serial" {
                    "bigint"
                } else {
                    col.ty.as_str()
                };
                type_sql = base.to_string();
                let w = if col.ty == "bool" || col.ty == "boolean" { Some("1".to_string()) } else { col.args.first().cloned() };
                if let Some(w) = w {
                    if width_kept(&w) {
                        type_sql = format!("{base}({w})");
                    }
                }
                size.max = bytes;
            }
            Class::Bit => {
                type_sql = format!("bit({n})");
                size.max = n.div_ceil(8).max(1);
            }
            Class::Decimal => {
                let m: u64 = col.args.first().and_then(|a| a.parse().ok()).unwrap_or(10);
                let d: u64 = col.args.get(1).and_then(|a| a.parse().ok()).unwrap_or(0);
                let (mut m2, mut d2) = (m, d);
                if !src.ver.at(5, 0, 3) {
                    m2 = (m + 1).min(65);
                    d2 = d.min(30);
                    decimal_widened.push(col.name.clone());
                    if m + 1 > 65 || d > 30 {
                        warnings.push(warning("DECIMAL_PRECISION", format!("column `{}` decimal({m},{d}) exceeds the target's DECIMAL(65,30)", col.name)));
                    }
                }
                type_sql = format!("decimal({m2},{d2})");
                size.max = decimal_bytes(m2, d2);
            }
            Class::Float | Class::Double => {
                let base = if class == Class::Float { "float" } else { "double" };
                type_sql = if col.args.is_empty() { base.to_string() } else { format!("{base}({})", col.args.join(",")) };
                size.max = if class == Class::Float && col.args.first().and_then(|a| a.parse::<u64>().ok()).unwrap_or(0) <= 24 { 4 } else { 8 };
            }
            Class::Char => {
                type_sql = format!("char({n})");
                chars = Some(n);
                size = Size { max: n * bpc, var: bpc > 1, lob: None };
            }
            Class::Varchar => {
                if n * bpc > 65_535 {
                    type_sql = "mediumtext".into();
                    class = Class::Text(2);
                    varchar_to_text.push(col.name.clone());
                    size = Size { max: LOB_BYTES[2], var: true, lob: Some(11) };
                } else {
                    type_sql = format!("varchar({n})");
                    chars = Some(n);
                    size = Size { max: n * bpc, var: true, lob: None };
                }
            }
            Class::Binary => {
                type_sql = format!("binary({n})");
                size.max = n;
            }
            Class::Varbinary => {
                type_sql = format!("varbinary({n})");
                size = Size { max: n, var: true, lob: None };
            }
            Class::Text(k) => {
                let mut k2 = k;
                if bpc > src_bpc && k < 3 {
                    k2 = k + 1;
                    text_promoted.push(format!("{} {} -> {}", col.name, TEXT_NAMES[k as usize], TEXT_NAMES[k2 as usize]));
                }
                class = Class::Text(k2);
                type_sql = TEXT_NAMES[k2 as usize].to_string();
                size = Size { max: LOB_BYTES[k2 as usize], var: true, lob: Some(9 + k2 as u64) };
                has_text_lob = true;
            }
            Class::Blob(k) => {
                type_sql = BLOB_NAMES[k as usize].to_string();
                size = Size { max: LOB_BYTES[k as usize], var: true, lob: Some(9 + k as u64) };
            }
            Class::Enum | Class::Set => {
                let kw = if class == Class::Enum { "enum" } else { "set" };
                type_sql = format!("{kw}({})", col.values.iter().map(|v| my_str(v)).collect::<Vec<_>>().join(","));
                size.max = if class == Class::Enum {
                    if col.values.len() > 255 {
                        2
                    } else {
                        1
                    }
                } else {
                    match col.values.len() {
                        0..=8 => 1,
                        9..=16 => 2,
                        17..=24 => 3,
                        25..=32 => 4,
                        _ => 8,
                    }
                };
            }
            Class::Date => {
                type_sql = "date".into();
                size.max = 3;
            }
            Class::Year => {
                if col.args.first().is_some_and(|a| a == "2") {
                    year2.push(col.name.clone());
                }
                type_sql = "year".into();
                size.max = 1;
            }
            Class::Time | Class::DateTime | Class::Timestamp => {
                let base = match class {
                    Class::Time => "time",
                    Class::DateTime => "datetime",
                    _ => "timestamp",
                };
                // Before 5.6.4 an argument of TIMESTAMP is a display width, never fsp.
                let fsp: u64 = if src.ver.at(5, 6, 4) || (src.ver.maria && src.ver.at(5, 3, 0)) { col.args.first().and_then(|a| a.parse().ok()).unwrap_or(0) } else { 0 };
                if fsp > 0 && !tgt.fsp() {
                    warnings.push(warning("DOWNGRADE", format!("column `{}`: fractional seconds are dropped (the target has no {base}(fsp))", col.name)));
                }
                type_sql = if fsp > 0 && tgt.fsp() { format!("{base}({fsp})") } else { base.to_string() };
                let f = if tgt.fsp() { fsp_bytes(fsp) } else { 0 };
                size.max = match class {
                    Class::Time => 3 + f,
                    Class::DateTime => {
                        if tgt.fsp() {
                            5 + f
                        } else {
                            8
                        }
                    }
                    _ => 4 + f,
                };
            }
            Class::Json => {
                if tgt.json() {
                    type_sql = "json".into();
                } else {
                    type_sql = "longtext".into();
                    warnings.push(warning("DOWNGRADE", format!("column `{}`: json becomes longtext (no JSON type before 5.7.8)", col.name)));
                }
                size = Size { max: LOB_BYTES[3], var: true, lob: Some(12) };
            }
            Class::Geometry => {
                type_sql = col.ty.clone();
                size = Size { max: LOB_BYTES[3], var: true, lob: Some(12) };
            }
            Class::Unknown => {
                type_sql = if col.args.is_empty() { col.ty.clone() } else { format!("{}({})", col.ty, col.args.join(",")) };
                blockers.push(danger("UNSUPPORTED_COLUMN", format!("column `{}` has type {} that BulkDB does not translate", col.name, col.ty)));
            }
        }
        if matches!(class, Class::Int(_) | Class::Decimal | Class::Float | Class::Double) {
            if col.unsigned {
                type_sql.push_str(" unsigned");
            }
            if col.zerofill {
                type_sql.push_str(" zerofill");
            }
            if tgt.deprecated_syntax()
                && (col.zerofill
                    || (col.unsigned && matches!(class, Class::Decimal | Class::Float | Class::Double))
                    || (matches!(class, Class::Float | Class::Double) && !col.args.is_empty()))
            {
                deprecated.push(col.name.clone());
            }
        }
        if matches!(class, Class::Float) && col.args.is_empty() && src.text_protocol {
            // read widened (3.2)
        }
        if class == Class::Double && src.text_protocol {
            float_text.push(col.name.clone());
        }
        // Collation of text columns.
        let mut target_coll = None;
        let mut coll_changed = false;
        if is_text(class) {
            let src_coll = col.collation.clone().or_else(|| if col.charset.is_some() { None } else { src_table_coll.clone() });
            let mut w = Vec::new();
            target_coll = map_coll(src_coll.as_deref(), col.binary_attr, &mut w);
            warnings.append(&mut w);
            if let Some(tc) = &target_coll {
                let src_fam = src_coll.as_deref().map(coll_family).unwrap_or_else(|| if col.binary_attr { "bin".into() } else { format!("{col_src_cs} default") });
                let tgt_fam = coll_family(tc);
                if src_fam != tgt_fam {
                    coll_changed = true;
                    coll_changed_cols.push(col.name.clone());
                    if is_no_pad(tc) && !src_coll.as_deref().is_some_and(is_no_pad) {
                        pad_note = true;
                    }
                }
            }
            // ENUM / SET duplicates under the new collation.
            if matches!(class, Class::Enum | Class::Set) {
                let tc = target_coll.clone().or_else(|| table_coll.clone()).unwrap_or_default();
                if tc.ends_with("_ci") {
                    let mut seen = BTreeSet::new();
                    for v in &col.values {
                        if !seen.insert(ci_fold(v, is_no_pad(&tc))) {
                            blockers.push(
                                danger("ENUM_DUPLICATE", format!("column `{}`: values of the {} collide under {tc}", col.name, col.ty))
                                    .rec("use a _bin collation for this table (mysql.table_overrides)"),
                            );
                            break;
                        }
                    }
                }
            }
        }
        if matches!(class, Class::Text(_) | Class::Blob(_) | Class::Json) {
            has_text_lob = true;
        }

        // ---- zero dates, nullability, defaults
        let temporal = is_temporal(class);
        let in_pk = pk_cols.contains(&lname);
        let first_auto = class == Class::Timestamp && src.first_timestamp_rule() && !first_ts_seen;
        if class == Class::Timestamp {
            has_timestamp = true;
            if src.first_timestamp_rule() && !first_ts_seen {
                first_timestamp_auto = true;
            }
            first_ts_seen = true;
        }
        let zero_action = if !temporal {
            ZeroDateAction::NotTemporal
        } else if first_auto {
            if zero == ZeroDatePolicy::Keep {
                ZeroDateAction::Keep
            } else {
                ZeroDateAction::Min
            }
        } else {
            match zero {
                ZeroDatePolicy::Null if in_pk => ZeroDateAction::Min,
                ZeroDatePolicy::Null => ZeroDateAction::Null,
                ZeroDatePolicy::Keep | ZeroDatePolicy::Auto => ZeroDateAction::Keep,
                ZeroDatePolicy::Min => ZeroDateAction::Min,
            }
        };
        let src_not_null = col.not_null || (class == Class::Timestamp && src.first_timestamp_rule());
        let nullable = if zero_action == ZeroDateAction::Null { true } else { !src_not_null && !in_pk };
        let invalid_action = match (class, zero_action) {
            (Class::Year, _) => ZeroDateAction::Keep,
            (c, _) if !is_temporal(c) => ZeroDateAction::NotTemporal,
            (_, ZeroDateAction::Null) => ZeroDateAction::Null,
            (_, ZeroDateAction::Min) => ZeroDateAction::Min,
            (Class::Timestamp, _) => {
                if nullable {
                    ZeroDateAction::Null
                } else {
                    ZeroDateAction::Min
                }
            }
            _ => ZeroDateAction::Keep,
        };
        if temporal {
            zero_cols.push(col.name.clone());
            if zero == ZeroDatePolicy::Null && (in_pk || unique_cols.contains(&lname)) {
                zero_in_key.push(col.name.clone());
            }
        }
        let mut def_sql = String::new();
        let mut default = col.default.clone();
        if class == Class::Timestamp {
            if let Some(utc) = ov.timestamp_defaults_utc.get(&col.name) {
                default = Some(MyDefault::Literal(utc.clone()));
            }
        }
        let gen = col.generated.is_some();
        if !gen {
            if first_auto {
                def_sql = " DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP".into();
            } else {
                let zero_default = |c: Class| -> String {
                    match zero_action {
                        ZeroDateAction::Null => " DEFAULT NULL".into(),
                        ZeroDateAction::Min => format!(" DEFAULT {}", min_literal(c)),
                        _ => format!(" DEFAULT {}", zero_literal(c)),
                    }
                };
                match &default {
                    Some(MyDefault::Null) => def_sql = " DEFAULT NULL".into(),
                    Some(MyDefault::Literal(v)) if temporal && is_zero_temporal(v) => def_sql = zero_default(class),
                    Some(MyDefault::Number(v)) if class == Class::Year && is_zero_temporal(v) => def_sql = zero_default(class),
                    Some(MyDefault::Literal(v)) if temporal && partial_zero_fix(v).is_some() => {
                        def_sql = match zero_action {
                            ZeroDateAction::Null => " DEFAULT NULL".into(),
                            ZeroDateAction::Min => format!(" DEFAULT {}", my_str(&partial_zero_fix(v).unwrap_or_default())),
                            _ => format!(" DEFAULT {}", my_str(v)),
                        }
                    }
                    Some(MyDefault::Literal(v)) => {
                        if matches!(class, Class::Text(_) | Class::Blob(_) | Class::Json | Class::Geometry) {
                            if tgt.expr_default() {
                                def_sql = format!(" DEFAULT ({})", my_str(v));
                            } else {
                                default_dropped.push(col.name.clone());
                            }
                        } else {
                            def_sql = format!(" DEFAULT {}", my_str(v));
                        }
                    }
                    Some(MyDefault::Number(v)) => def_sql = format!(" DEFAULT {v}"),
                    Some(MyDefault::Raw(v)) => def_sql = format!(" DEFAULT {v}"),
                    Some(MyDefault::CurrentTimestamp(fsp)) => {
                        if class == Class::DateTime && !tgt.datetime_now_default() {
                            default_dropped.push(col.name.clone());
                        } else {
                            let f = fsp.as_deref().filter(|_| tgt.fsp()).map(|f| format!("({f})")).unwrap_or_default();
                            def_sql = format!(" DEFAULT CURRENT_TIMESTAMP{f}");
                        }
                    }
                    Some(MyDefault::Expr(e)) => {
                        if tgt.expr_default() {
                            def_sql = format!(" DEFAULT {e}");
                        } else {
                            warnings.push(warning("DOWNGRADE", format!("column `{}`: expression default {e} is dropped (needs 8.0.13+)", col.name)));
                        }
                    }
                    None => {
                        if class == Class::Timestamp {
                            def_sql = if nullable { " DEFAULT NULL".into() } else { zero_default(class) };
                        } else if src.first_timestamp_rule() && temporal && !nullable && !in_pk {
                            // 4.0 NOT NULL temporal columns default to the zero value.
                            def_sql = zero_default(class);
                        }
                    }
                }
                if zero_action == ZeroDateAction::Null && def_sql.is_empty() && temporal {
                    // A nullable column without a default: implicit NULL.
                }
                if let Some(fsp) = &col.on_update {
                    if class == Class::DateTime && !tgt.datetime_now_default() {
                        default_dropped.push(format!("{} (ON UPDATE)", col.name));
                    } else {
                        let f = Some(fsp.as_str()).filter(|f| !f.is_empty() && tgt.fsp()).map(|f| format!("({f})")).unwrap_or_default();
                        def_sql.push_str(&format!(" ON UPDATE CURRENT_TIMESTAMP{f}"));
                    }
                }
            }
        }
        // ---- generated / invisible / srid
        if gen && !tgt.generated() {
            blockers.push(danger("UNSUPPORTED_COLUMN", format!("generated column `{}` needs MySQL 5.7.6+", col.name)));
        }
        if col.invisible && !tgt.invisible_column() {
            blockers.push(danger("UNSUPPORTED_COLUMN", format!("invisible column `{}` needs MySQL 8.0.23+", col.name)));
        }
        if !col.dropped.is_empty() {
            col_dropped_attrs.push(format!("{} ({})", col.name, col.dropped.join(", ")));
        }
        let mut sql = format!("  {} {}", bq(&col.name), type_sql);
        if let Some(tc) = &target_coll {
            if Some(tc) != table_coll.as_ref() {
                sql.push_str(&format!(" COLLATE {tc}"));
            }
        }
        if let Some((expr, stored)) = &col.generated {
            sql.push_str(&format!(" GENERATED ALWAYS AS {expr} {}", if *stored { "STORED" } else { "VIRTUAL" }));
        }
        let explicit_null = class == Class::Timestamp || gen;
        if !nullable {
            sql.push_str(" NOT NULL");
        } else if explicit_null {
            sql.push_str(" NULL");
        }
        sql.push_str(&def_sql);
        if col.auto_increment {
            sql.push_str(" AUTO_INCREMENT");
        }
        if col.invisible && tgt.invisible_column() {
            sql.push_str(" INVISIBLE");
        }
        if let Some(c) = &col.comment {
            sql.push_str(&format!(" COMMENT {}", my_str(c)));
        }
        if let Some(srid) = &col.srid {
            if tgt.srid() {
                sql.push_str(&format!(" SRID {srid}"));
            } else {
                warnings.push(warning("DOWNGRADE", format!("column `{}`: SRID {srid} is dropped (needs 8.0)", col.name)));
            }
        }
        if !gen {
            let value = match class {
                Class::Int(_) => ValueKind::Integer,
                Class::Bit => ValueKind::Bit,
                Class::Decimal => ValueKind::Decimal,
                Class::Float | Class::Double => ValueKind::Float,
                Class::Char | Class::Varchar | Class::Text(_) | Class::Enum | Class::Set => ValueKind::Text,
                Class::Binary | Class::Varbinary | Class::Blob(_) | Class::Geometry => ValueKind::Binary,
                Class::Date => ValueKind::Date,
                Class::DateTime => ValueKind::DateTime,
                Class::Timestamp => ValueKind::Timestamp,
                Class::Time => ValueKind::Time,
                Class::Year => ValueKind::Year,
                Class::Json => ValueKind::Json,
                Class::Unknown => ValueKind::Raw,
            };
            let orig = class_of(&col.ty);
            let read = match orig {
                Class::Date | Class::DateTime | Class::Time => {
                    if src.ver.at(4, 0, 2) {
                        SourceRead::CastChar
                    } else {
                        SourceRead::Concat
                    }
                }
                Class::Timestamp => {
                    if src.epoch_timestamps() {
                        SourceRead::UnixTimestamp
                    } else {
                        SourceRead::CastChar
                    }
                }
                Class::Year => SourceRead::YearNumber,
                Class::Float if col.args.is_empty() && src.text_protocol => SourceRead::FloatWiden,
                _ => SourceRead::Plain,
            };
            let max_bytes = match class {
                Class::Char | Class::Varchar | Class::Binary | Class::Varbinary | Class::Text(_) | Class::Blob(_) => Some(size.max),
                _ => None,
            };
            data_cols.push(DataColumn {
                source: col.name.clone(),
                target: col.name.clone(),
                value,
                read,
                zero_date: zero_action,
                invalid_date: invalid_action,
                binary_attr: col.binary_attr && value == ValueKind::Text,
                max_bytes,
            });
        }
        cols.push(ColOut { name: col.name.clone(), class, type_sql: sql, chars, text_bpc: bpc, size, nullable, generated: gen, coll_changed, target_coll });
    }

    // ---- issues per table (columns)
    if !coll_changed_cols.is_empty() {
        let mut iss =
            info("COLLATION_CHANGED", "text columns get a collation with different comparison rules").detail(format!("columns: {}", coll_changed_cols.join(", ")));
        if pad_note {
            iss = iss.rec("trailing spaces now matter (NO PAD): `WHERE c = 'abc '` and uniqueness behave differently; the `legacy` collation preset keeps PAD SPACE");
        }
        warnings.push(iss);
    }
    if !zero_cols.is_empty() {
        let (code, msg) = match zero {
            ZeroDatePolicy::Null => ("ZERO_DATE_NULL", "zero and invalid dates become NULL; NOT NULL date columns become nullable"),
            ZeroDatePolicy::Min => ("ZERO_DATE_MIN", "zero dates become the minimum value ('1000-01-01', TIMESTAMP '1970-01-01 00:00:01', YEAR 1901)"),
            _ => ("ZERO_DATE_KEEP", "zero dates are kept: applications on strict servers fail to write such rows"),
        };
        warnings.push(warning(code, msg).detail(format!("columns: {}", zero_cols.join(", "))));
    }
    if !zero_in_key.is_empty() {
        warnings.push(
            warning("ZERO_DATE_IN_KEY", "temporal key columns: PRIMARY KEY columns use the minimum value; NULLs in UNIQUE keys never collide, so duplicates may load")
                .detail(format!("columns: {}", zero_in_key.join(", ")))
                .rec("zero_dates: min"),
        );
    }
    if has_timestamp {
        warnings.push(info("TIMESTAMP_SEMANTICS", "TIMESTAMP values are copied as UTC instants under time_zone '+00:00'"));
    }
    if first_timestamp_auto {
        warnings.push(info(
            "TIMESTAMP_SEMANTICS",
            "the first TIMESTAMP column of a 4.0 table was auto-set; it becomes DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP",
        ));
    }
    if !text_promoted.is_empty() {
        warnings.push(info("TEXT_PROMOTED", "TEXT columns move one class up because the target needs more bytes per character").detail(text_promoted.join(", ")));
    }
    if !varchar_to_text.is_empty() {
        warnings.push(
            warning("VARCHAR_TO_TEXT", "VARCHAR columns too wide for the target charset become MEDIUMTEXT").detail(format!("columns: {}", varchar_to_text.join(", "))),
        );
    }
    if !decimal_widened.is_empty() {
        warnings
            .push(info("DECIMAL_WIDENED", "DECIMAL columns from a source before 5.0.3 get one more digit").detail(format!("columns: {}", decimal_widened.join(", "))));
    }
    if !year2.is_empty() {
        warnings.push(info("YEAR2_WIDENED", "YEAR(2) becomes YEAR (4 digits)").detail(format!("columns: {}", year2.join(", "))));
    }
    if !deprecated.is_empty() {
        warnings.push(
            info("DEPRECATED_SYNTAX", "kept syntax that is deprecated on this target (ZEROFILL, UNSIGNED or (M,D) on floating point)")
                .detail(format!("columns: {}", deprecated.join(", "))),
        );
    }
    if !default_dropped.is_empty() {
        warnings.push(warning("DEFAULT_DROPPED", "defaults the target cannot hold are dropped").detail(format!("columns: {}", default_dropped.join(", "))));
    }
    if !float_text.is_empty() {
        warnings.push(
            warning("FLOAT_TEXT_PRECISION", "values beyond 15 significant digits are rounded (the 4.0 text protocol prints at most 15)")
                .detail(format!("columns: {}", float_text.join(", "))),
        );
    }
    if !col_dropped_attrs.is_empty() {
        dropped_opts.push(format!("column attributes: {}", col_dropped_attrs.join("; ")));
    }
    if tgt.require_pk() && t.pk_columns().is_empty() {
        blockers.push(danger("TGT_REQUIRES_PK", "the target requires a PRIMARY KEY (sql_require_primary_key=ON) and this table has none").rec("exclude the table"));
    }
    // Reserved words (for the applications' sake).
    let reserved_new = |w: &str| is_reserved(w, tgt.ver.major, tgt.ver.minor, tgt.ver.patch) && !is_reserved(w, src.ver.major, src.ver.minor, src.ver.patch);
    let mut reserved: Vec<String> = Vec::new();
    if reserved_new(target_name) {
        reserved.push(format!("table {target_name}"));
    }
    for c in &t.columns {
        if reserved_new(&c.name) {
            reserved.push(format!("column {}", c.name));
        }
    }
    if !reserved.is_empty() {
        warnings.push(info("RESERVED_WORD", "names that are reserved words on the target must be quoted by applications").detail(reserved.join(", ")));
    }

    // ---- row format and key limits
    let src_dynamic = t.option("ROW_FORMAT").is_some_and(|r| matches!(r.to_ascii_uppercase().as_str(), "DYNAMIC" | "COMPRESSED"));
    let mut dynamic = innodb && tgt.dynamic_ok() && (tgt.dynamic_default() || src_dynamic);
    fn part_bytes(cols: &[ColOut], p: &MyKeyPart) -> (u64, bool, u64) {
        // (bytes, is string, bpc)
        let Some(c) = p.column.as_deref().and_then(|n| cols.iter().find(|c| c.name.eq_ignore_ascii_case(n))) else { return (0, false, 1) };
        match c.class {
            Class::Char | Class::Varchar | Class::Text(_) => {
                let chars = p.prefix.map(u64::from).or(c.chars).unwrap_or(c.size.max / c.text_bpc.max(1));
                (chars * c.text_bpc, true, c.text_bpc)
            }
            Class::Binary | Class::Varbinary | Class::Blob(_) => (p.prefix.map(u64::from).unwrap_or(c.size.max), true, 1),
            _ => (c.size.max, false, 1),
        }
    }
    let limits = |dynamic: bool| -> (u64, u64) {
        if innodb {
            (if dynamic && tgt.large_prefix() { 3072 } else { 767 }, 3072)
        } else if myisam {
            (1000, 1000)
        } else {
            (3072, 3072)
        }
    };
    let needs_large = t.keys.iter().filter(|k| !matches!(k.kind, MyKeyKind::Fulltext | MyKeyKind::Spatial)).any(|k| k.parts.iter().any(|p| part_bytes(&cols, p).0 > 767));
    if innodb && needs_large && !dynamic && tgt.dynamic_ok() && tgt.large_prefix() {
        dynamic = true;
        warnings.push(info("ROW_FORMAT_DYNAMIC", "the table gets ROW_FORMAT=DYNAMIC so its long keys need no prefix"));
    }
    let (part_limit, key_limit) = limits(dynamic);

    // ---- row size limits
    let lob_ptr_total = |cols: &[ColOut]| -> u64 {
        let nullable = cols.iter().filter(|c| c.nullable).count() as u64;
        let mut sum = nullable.div_ceil(8);
        for c in cols.iter().filter(|c| !c.generated || true) {
            sum += match c.size.lob {
                Some(p) => p,
                None if c.size.var => c.size.max + if c.size.max > 255 { 2 } else { 1 },
                None => c.size.max,
            };
        }
        sum
    };
    let inrow = |cols: &[ColOut], dynamic: bool| -> u64 {
        let ext = if dynamic { 40 } else { 768 + 20 };
        cols.iter()
            .filter(|c| !(c.generated && !c.type_sql.contains(" STORED")))
            .map(|c| if !c.size.var || c.size.max <= 255 { c.size.max } else { c.size.max.min(ext) })
            .sum()
    };
    let mut row_fixed: Vec<String> = Vec::new();
    if lob_ptr_total(&cols) > 65_535 && tgt.fix_row_size {
        let mut order: Vec<usize> = (0..cols.len()).filter(|&i| cols[i].class == Class::Varchar && !key_cols.contains(&cols[i].name.to_ascii_lowercase())).collect();
        order.sort_by_key(|&i| std::cmp::Reverse(cols[i].size.max));
        for i in order {
            if lob_ptr_total(&cols) <= 65_535 {
                break;
            }
            let c = &mut cols[i];
            let (name, k) = if c.size.max <= 65_535 { ("text", 1u8) } else { ("mediumtext", 2u8) };
            c.type_sql = c.type_sql.replacen(&format!(" varchar({})", c.chars.unwrap_or(0)), &format!(" {name}"), 1);
            c.class = Class::Text(k);
            c.size = Size { max: LOB_BYTES[k as usize], var: true, lob: Some(9 + k as u64) };
            row_fixed.push(format!("{} -> {name}", c.name));
            if let Some(dc) = data_cols.iter_mut().find(|d| d.source == c.name) {
                dc.max_bytes = Some(LOB_BYTES[k as usize]);
            }
        }
    }
    if !row_fixed.is_empty() {
        warnings.push(warning("ROW_SIZE_FIXED", "the widest VARCHAR columns become TEXT so the row fits MySQL's 65,535-byte limit").detail(row_fixed.join(", ")));
    }
    if lob_ptr_total(&cols) > 65_535 {
        blockers.push(
            danger("ROW_SIZE_LIMIT", format!("the row needs {} bytes, over MySQL's 65,535-byte limit for non-TEXT columns", lob_ptr_total(&cols)))
                .rec("a 5.7+ target, engine myisam, or exclude the table"),
        );
    }
    if innodb {
        let limit = tgt.page_size() / 2 - 66;
        if inrow(&cols, dynamic) > limit && !dynamic && tgt.dynamic_ok() && tgt.fix_row_size {
            dynamic = true;
            warnings.push(info("ROW_FORMAT_DYNAMIC", "the table gets ROW_FORMAT=DYNAMIC so its row fits InnoDB's in-row limit"));
        }
        let need = inrow(&cols, dynamic);
        if need > limit {
            blockers.push(
                danger("INNODB_ROW_SIZE", format!("the row needs up to {need} bytes in-row, over InnoDB's {limit}-byte limit"))
                    .rec("a 5.7+ target, engine myisam, or exclude the table"),
            );
        }
    }

    // ---- keys
    let mut inline_keys: Vec<String> = Vec::new();
    let mut post_keys: Vec<MyPostKey> = Vec::new();
    let mut unique_keys: Vec<Vec<String>> = Vec::new();
    let mut ai_key_done = false;
    let keys_inline_all = !innodb || !tgt.indexes_after_data;
    let mut has_nonunique = false;
    let mut key_dropped: Vec<String> = Vec::new();
    for k in &t.keys {
        let mut kw: Vec<CopyIssue> = Vec::new();
        if k.parts.iter().any(|p| p.expr.is_some()) && !tgt.functional_keys() {
            warnings.push(warning("DOWNGRADE", format!("functional key {} is dropped (needs 8.0.13+)", k.name.as_deref().unwrap_or("?"))));
            continue;
        }
        let unique = matches!(k.kind, MyKeyKind::Primary | MyKeyKind::Unique);
        if unique && k.parts.iter().all(|p| p.column.is_some() && p.prefix.is_none()) {
            unique_keys.push(k.parts.iter().filter_map(|p| p.column.clone()).collect());
        }
        // Prefixes for the target's key limits.
        let mut prefixes: Vec<Option<u32>> = k.parts.iter().map(|p| p.prefix).collect();
        let mut prefixed: Vec<String> = Vec::new();
        if !matches!(k.kind, MyKeyKind::Fulltext | MyKeyKind::Spatial) {
            let mut bytes: Vec<(u64, bool, u64)> = k.parts.iter().map(|p| part_bytes(&cols, p)).collect();
            for (i, p) in k.parts.iter().enumerate() {
                let (b, is_str, bpcx) = bytes[i];
                if is_str && b > part_limit {
                    let np = (part_limit / bpcx) as u32;
                    prefixes[i] = Some(np);
                    bytes[i].0 = np as u64 * bpcx;
                    prefixed.push(format!("{}({np})", p.column.as_deref().unwrap_or("?")));
                }
            }
            let mut guard = 0;
            while bytes.iter().map(|b| b.0).sum::<u64>() > key_limit && guard < 64 {
                guard += 1;
                let total: u64 = bytes.iter().map(|b| b.0).sum();
                let Some((i, _)) = bytes.iter().enumerate().filter(|(_, b)| b.1 && b.0 > b.2).max_by_key(|(_, b)| b.0) else { break };
                let excess = total - key_limit;
                let nb = bytes[i].0.saturating_sub(excess).max(bytes[i].2);
                let np = (nb / bytes[i].2) as u32;
                prefixes[i] = Some(np);
                bytes[i].0 = np as u64 * bytes[i].2;
                prefixed.push(format!("{}({np})", k.parts[i].column.as_deref().unwrap_or("?")));
            }
        }
        let kname = k.name.clone().unwrap_or_else(|| if k.kind == MyKeyKind::Primary { "PRIMARY".into() } else { "?".into() });
        if !prefixed.is_empty() {
            if unique {
                kw.push(
                    danger("UNIQUE_PREFIX", format!("key {kname} needs a prefix on this target, which changes what counts as a duplicate"))
                        .detail(prefixed.join(", "))
                        .rec("a target with DYNAMIC rows (5.7.7+), a shorter column, or charset utf8 for this table (mysql.table_overrides)"),
                );
            } else {
                kw.push(warning("INDEX_PREFIX", format!("key {kname} gets a prefix to fit the target's key length limit")).detail(prefixed.join(", ")));
            }
        }
        // Collation risk on unique keys over changed text columns.
        if unique {
            let risky: Vec<&str> = k
                .parts
                .iter()
                .filter_map(|p| p.column.as_deref())
                .filter(|c| cols.iter().any(|x| x.name.eq_ignore_ascii_case(c) && x.coll_changed && x.target_coll.as_deref().is_some_and(|tc| tc.ends_with("_ci"))))
                .collect();
            if !risky.is_empty() {
                kw.push(
                    warning(
                        "COLLATION_UNIQUE_RISK",
                        format!("key {kname}: values that differ only by width, case, accent or trailing spaces collide under the new collation"),
                    )
                    .detail(format!("columns: {}", risky.join(", ")))
                    .rec("utf8mb4_bin for this table (mysql.table_overrides)"),
                );
            }
        }
        let parts_sql: Vec<String> = k
            .parts
            .iter()
            .zip(&prefixes)
            .map(|(p, pre)| {
                let mut s = match (&p.expr, &p.column) {
                    (Some(e), _) => e.clone(),
                    (None, Some(c)) => bq(c),
                    _ => String::new(),
                };
                if let Some(n) = pre {
                    s.push_str(&format!("({n})"));
                }
                if p.desc {
                    s.push_str(" DESC");
                }
                s
            })
            .collect();
        let mut def = match k.kind {
            MyKeyKind::Primary => "PRIMARY KEY".to_string(),
            MyKeyKind::Unique => format!("UNIQUE KEY {}", bq(&kname)),
            MyKeyKind::Index => format!("KEY {}", bq(&kname)),
            MyKeyKind::Fulltext => format!("FULLTEXT KEY {}", bq(&kname)),
            MyKeyKind::Spatial => format!("SPATIAL KEY {}", bq(&kname)),
        };
        def.push_str(&format!(" ({})", parts_sql.join(",")));
        if let Some(u) = &k.using {
            if u == "BTREE" || u == "HASH" {
                def.push_str(&format!(" USING {u}"));
            }
        }
        if let Some(p) = &k.parser {
            if tgt.fulltext_parser() {
                def.push_str(&format!(" WITH PARSER {}", bq(p)));
            } else {
                kw.push(warning("DOWNGRADE", format!("key {kname}: WITH PARSER {p} is dropped (needs 5.7.6+)")));
            }
        }
        if let Some(c) = &k.comment {
            if tgt.index_comment() {
                def.push_str(&format!(" COMMENT {}", my_str(c)));
            } else {
                key_dropped.push(format!("{kname} COMMENT"));
            }
        }
        if k.invisible {
            if tgt.invisible_index() {
                def.push_str(" INVISIBLE");
            } else {
                kw.push(warning("DOWNGRADE", format!("key {kname} becomes visible (invisible indexes need 8.0)")));
            }
        }
        if k.key_block_size.is_some() {
            key_dropped.push(format!("{kname} KEY_BLOCK_SIZE"));
        }
        let is_ai_key = !ai_key_done && ai_col.is_some() && ai_leads(k);
        if is_ai_key {
            ai_key_done = true;
        }
        if k.kind == MyKeyKind::Primary || is_ai_key || keys_inline_all {
            inline_keys.push(format!("  {def}"));
            warnings.extend(kw);
            if !unique && k.kind != MyKeyKind::Primary {
                has_nonunique = true;
            }
        } else {
            post_keys.push(MyPostKey { name: kname, clause: format!("ADD {def}"), unique, separate: k.kind == MyKeyKind::Fulltext, warnings: kw });
        }
    }
    if !key_dropped.is_empty() {
        dropped_opts.push(format!("key options: {}", key_dropped.join(", ")));
    }

    // ---- checks
    let mut check_sql: Vec<String> = Vec::new();
    for ch in &t.checks {
        if tgt.checks() {
            let name = ch.name.as_ref().map(|n| format!("CONSTRAINT {} ", bq(n))).unwrap_or_default();
            let enf = if ch.enforced { "" } else { " NOT ENFORCED" };
            check_sql.push(format!("  {name}CHECK {}{enf}", ch.expr));
        } else {
            warnings
                .push(warning("DOWNGRADE", format!("CHECK constraint {} is dropped (the target ignores CHECK before 8.0.16)", ch.name.as_deref().unwrap_or(&ch.expr))));
        }
    }

    // ---- FKs
    let mut fks: Vec<MyFkOut> = Vec::new();
    for (i, fk) in t.fks.iter().enumerate() {
        let name = fk.name.clone().unwrap_or_else(|| format!("{}_ibfk_{}", target_name, i + 1));
        let same_db = fk.ref_db.as_deref().is_none_or(|d| d == src.db);
        let parent = if same_db {
            bq2(&tgt.db, renamed.get(&fk.ref_table).map(String::as_str).unwrap_or(&fk.ref_table))
        } else {
            bq2(fk.ref_db.as_deref().unwrap_or(""), &fk.ref_table)
        };
        let mut clause = format!(
            "ADD CONSTRAINT {} FOREIGN KEY ({}) REFERENCES {} ({})",
            bq(&name),
            fk.columns.iter().map(|c| bq(c)).collect::<Vec<_>>().join(", "),
            parent,
            fk.ref_columns.iter().map(|c| bq(c)).collect::<Vec<_>>().join(", ")
        );
        if let Some(a) = &fk.on_delete {
            clause.push_str(&format!(" ON DELETE {a}"));
        }
        if let Some(a) = &fk.on_update {
            clause.push_str(&format!(" ON UPDATE {a}"));
        }
        fks.push(MyFkOut {
            name,
            clause,
            columns: fk.columns.clone(),
            ref_db: if same_db { None } else { fk.ref_db.clone() },
            ref_table: fk.ref_table.clone(),
            ref_columns: fk.ref_columns.clone(),
        });
    }

    // ---- table options
    for o in &t.options {
        let n = o.name.as_str();
        let dropped = match n {
            "ENGINE" | "CHARSET" | "COLLATE" | "COMMENT" | "AUTO_INCREMENT" | "ROW_FORMAT" => false,
            "TYPE" => false,
            "PACK_KEYS" | "DELAY_KEY_WRITE" | "CHECKSUM" | "TABLE_CHECKSUM" => engine != source_engine || !myisam,
            _ => true,
        };
        if dropped {
            dropped_opts.push(format!("{n}={}", if n == "CONNECTION" { "'…'" } else { o.value.as_str() }));
        }
    }
    if t.option("TYPE").is_some() {
        dropped_opts.push("TYPE= (written as ENGINE=)".into());
    }
    if !dropped_opts.is_empty() {
        warnings.push(info("TABLE_OPTION_DROPPED", "table options the target does not need are dropped").detail(dropped_opts.join("; ")));
    }
    // Partitioning.
    let mut partition_sql = None;
    if let Some(p) = &t.partition {
        let engine_ok = !tgt.my(8, 0, 0) || engine == "InnoDB" || engine == "ndbcluster";
        if !tgt.partitions() || !engine_ok {
            blockers.push(danger("PARTITION_UNSUPPORTED", format!("the target cannot partition a {engine} table")).rec("engine innodb, or exclude the table"));
        } else {
            let re = regex::Regex::new(r"(?i)(ENGINE\s*=\s*)[A-Za-z_]+").expect("static regex");
            partition_sql = Some(re.replace_all(p, format!("${{1}}{engine}").as_str()).into_owned());
        }
    }

    // ---- CREATE TABLE text
    let mut body: Vec<String> = cols.iter().map(|c| c.type_sql.clone()).collect();
    body.extend(inline_keys);
    body.extend(check_sql);
    let mut create = format!("CREATE TABLE {} (\n{}\n) ENGINE={engine}", bq2(&tgt.db, target_name), body.join(",\n"));
    if cs != "binary" {
        create.push_str(&format!(" DEFAULT CHARSET={cs}"));
        if let Some(c) = &table_coll {
            create.push_str(&format!(" COLLATE={c}"));
        }
    }
    if innodb && dynamic {
        create.push_str(" ROW_FORMAT=DYNAMIC");
    } else if myisam && engine == source_engine {
        if let Some(rf) = t.option("ROW_FORMAT") {
            create.push_str(&format!(" ROW_FORMAT={}", rf.to_ascii_uppercase()));
        }
    }
    if myisam && engine == source_engine {
        for o in ["PACK_KEYS", "DELAY_KEY_WRITE", "CHECKSUM"] {
            if let Some(v) = t.option(o) {
                create.push_str(&format!(" {o}={v}"));
            }
        }
    }
    if let Some(c) = t.option("COMMENT") {
        create.push_str(&format!(" COMMENT={}", my_str(c)));
    }
    if let Some(p) = partition_sql {
        create.push('\n');
        create.push_str(&p);
    }
    for u in &t.unparsed {
        blockers.push(danger("UNPARSED_CLAUSE", "a clause of SHOW CREATE TABLE is not recognised").detail(u.clone()).rec("exclude the table"));
    }

    // ---- data SELECT / verify
    let sel: Vec<String> = data_cols
        .iter()
        .map(|d| {
            let c = bq(&d.source);
            match d.read {
                SourceRead::Plain => c,
                SourceRead::CastChar => format!("CAST({c} AS CHAR) AS {c}"),
                SourceRead::Concat => format!("CONCAT({c}) AS {c}"),
                SourceRead::UnixTimestamp => format!("UNIX_TIMESTAMP({c}) AS {c}"),
                SourceRead::DateFormat => format!("DATE_FORMAT({c},'%Y-%m-%d %H:%i:%s') AS {c}"),
                SourceRead::YearNumber => format!("({c} + 0) AS {c}"),
                SourceRead::FloatWiden => format!("({c} + 0e0) AS {c}"),
            }
        })
        .collect();
    let ver: Vec<String> = data_cols
        .iter()
        .map(|d| {
            let c = bq(&d.target);
            match d.value {
                ValueKind::Date | ValueKind::DateTime | ValueKind::Timestamp | ValueKind::Time => format!("CAST({c} AS CHAR) AS {c}"),
                ValueKind::Year => format!("({c} + 0) AS {c}"),
                // The writer hashes BIT as the unsigned value it loads; read it back unsigned too
                // (BIGINT UNSIGNED arrives as decimal text), or BIT(64) >= 2^63 would mismatch.
                ValueKind::Bit => format!("CAST({c} AS UNSIGNED) AS {c}"),
                _ => c,
            }
        })
        .collect();
    let select_sql = format!("SELECT {} FROM {}", sel.join(", "), bq2(&src.db, &t.name));
    let verify_sql = format!("SELECT {} FROM {}", ver.join(", "), bq2(&tgt.db, target_name));

    MyTableOut {
        source_engine,
        engine,
        unsupported,
        create_sql: create,
        disable_keys: myisam && has_nonunique,
        post_keys,
        fks,
        columns: data_cols,
        select_sql,
        verify_sql,
        first_timestamp_auto,
        auto_increment: ai_col,
        charset: cs,
        collation: table_coll,
        unique_keys,
        has_text_lob,
        warnings,
        blockers,
        source_ddl: String::new(),
    }
}

// =============================================================================================
// Views and triggers
// =============================================================================================

/// Rebuild `src` from tokens, replacing some token runs.
fn splice(src: &str, edits: &mut [(usize, usize, String)]) -> String {
    edits.sort_by_key(|e| e.0);
    let mut out = String::with_capacity(src.len());
    let mut pos = 0usize;
    for (s, e, r) in edits.iter() {
        if *s < pos {
            continue;
        }
        out.push_str(&src[pos..*s]);
        out.push_str(r);
        pos = *e;
    }
    out.push_str(&src[pos..]);
    out
}

/// Every backquoted identifier equal to `src_db` in qualifier position (followed by `.`)
/// becomes `tgt_db`; string literals and comments are left alone (3.3 "Database qualifiers").
pub fn rewrite_db_qualifiers(sql: &str, src_db: &str, tgt_db: &str) -> String {
    let toks = lex(sql);
    let mut edits = Vec::new();
    for (i, t) in toks.iter().enumerate() {
        if t.k == TK::Ident && t.v == src_db && toks.get(i + 1).is_some_and(|n| n.is_p('.') && n.s == t.e) {
            edits.push((t.s, t.e, bq(tgt_db)));
        }
    }
    splice(sql, &mut edits)
}

/// Tables / views referenced as `` `db`.`name` `` (MySQL stores view bodies fully qualified).
pub fn qualified_refs(sql: &str, db: &str) -> BTreeSet<String> {
    let toks = lex(sql);
    let mut out = BTreeSet::new();
    for w in toks.windows(3) {
        if w[0].k == TK::Ident && w[0].v == db && w[1].is_p('.') && w[2].k == TK::Ident {
            // `db`.`t`.`col` is a column reference of table t: still a reference to t.
            out.insert(w[2].v.clone());
        }
    }
    out
}

/// `DEFINER=user@host` removed; returns the new text and the definer (user, host).
pub fn strip_definer(sql: &str) -> (String, Option<(String, String)>) {
    let toks = lex(sql);
    for (i, t) in toks.iter().enumerate() {
        if t.is_word("DEFINER") && toks.get(i + 1).is_some_and(|x| x.is_p('=')) {
            let user = toks.get(i + 2).map(|x| x.v.clone()).unwrap_or_default();
            let (host, end) = if toks.get(i + 3).is_some_and(|x| x.is_p('@')) {
                (toks.get(i + 4).map(|x| x.v.clone()).unwrap_or_default(), toks.get(i + 4).map(|x| x.e).unwrap_or(t.e))
            } else {
                (String::new(), toks.get(i + 2).map(|x| x.e).unwrap_or(t.e))
            };
            let mut e = end;
            let b = sql.as_bytes();
            while e < b.len() && b[e] == b' ' {
                e += 1;
            }
            let out = format!("{}{}", &sql[..t.s], &sql[e..]);
            return (out, Some((user, host)));
        }
    }
    (sql.to_string(), None)
}

/// `CREATE … VIEW `v` AS …` for the target: the view name qualified with the target database,
/// source database qualifiers rewritten, DEFINER removed unless `keep_definer`.
pub fn view_create_sql(create_view: &str, src_db: &str, tgt_db: &str, target_name: &str, keep_definer: bool) -> (String, Option<(String, String)>) {
    let (mut sql, definer) = strip_definer(create_view);
    if keep_definer {
        sql = create_view.to_string();
    }
    let toks = lex(&sql);
    let mut edits = Vec::new();
    if let Some(i) = toks.iter().position(|t| t.is_word("VIEW")) {
        let start = toks.get(i + 1).map(|t| t.s);
        let mut j = i + 1;
        if toks.get(j + 1).is_some_and(|t| t.is_p('.')) {
            j += 2;
        }
        if let (Some(s), Some(e)) = (start, toks.get(j).map(|t| t.e)) {
            edits.push((s, e, bq2(tgt_db, target_name)));
        }
    }
    let renamed = splice(&sql, &mut edits);
    (rewrite_db_qualifiers(&renamed, src_db, tgt_db), definer)
}

/// `CREATE … TRIGGER `t` <timing> <event> ON `tbl` FOR EACH ROW body` for the target: trigger
/// and table qualified with the target database, DEFINER removed unless `keep_definer`. The body
/// is not rewritten. Returns (statement, definer, body references the source database).
pub fn trigger_create_sql(create: &str, src_db: &str, tgt_db: &str, target_table: &str, keep_definer: bool) -> (String, Option<(String, String)>, bool) {
    let (stripped, definer) = strip_definer(create);
    let sql = if keep_definer { create.to_string() } else { stripped };
    let toks = lex(&sql);
    let mut edits = Vec::new();
    let mut body_start = sql.len();
    if let Some(i) = toks.iter().position(|t| t.is_word("TRIGGER")) {
        let mut j = i + 1;
        let s = toks.get(j).map(|t| t.s);
        if toks.get(j + 1).is_some_and(|t| t.is_p('.')) {
            j += 2;
        }
        let name = toks.get(j).map(|t| t.v.clone()).unwrap_or_default();
        if let (Some(s), Some(e)) = (s, toks.get(j).map(|t| t.e)) {
            edits.push((s, e, bq2(tgt_db, &name)));
        }
        if let Some(k) = toks.iter().skip(j).position(|t| t.is_word("ON")).map(|k| k + j) {
            let mut m = k + 1;
            let s2 = toks.get(m).map(|t| t.s);
            if toks.get(m + 1).is_some_and(|t| t.is_p('.')) {
                m += 2;
            }
            if let (Some(s2), Some(e2)) = (s2, toks.get(m).map(|t| t.e)) {
                edits.push((s2, e2, bq2(tgt_db, target_table)));
            }
            if let Some(r) = toks.iter().skip(m).position(|t| t.is_word("ROW")).map(|r| r + m) {
                body_start = toks.get(r + 1).map(|t| t.s).unwrap_or(sql.len());
            }
        }
    }
    let body_refs = lex(&sql[body_start.min(sql.len())..]).iter().any(|t| (t.k == TK::Ident || t.k == TK::Word) && t.v.eq_ignore_ascii_case(src_db));
    (splice(&sql, &mut edits), definer, body_refs)
}

/// The 5.0.10 - 5.1.20 statement built from `SHOW TRIGGERS` columns.
pub fn trigger_from_columns(name: &str, timing: &str, event: &str, table: &str, statement: &str) -> String {
    format!("CREATE TRIGGER {} {} {} ON {} FOR EACH ROW {}", bq(name), timing.to_ascii_uppercase(), event.to_ascii_uppercase(), bq(table), statement)
}

/// Order views so that every view comes after the views it references (by name); cycles keep
/// their input order. `deps[i]` are names referenced by view `names[i]`.
pub fn order_views(names: &[String], deps: &[BTreeSet<String>]) -> Vec<usize> {
    let n = names.len();
    let mut done = vec![false; n];
    let mut out = Vec::with_capacity(n);
    loop {
        let mut progress = false;
        for i in 0..n {
            if done[i] {
                continue;
            }
            let ready = deps[i].iter().all(|d| names.iter().enumerate().all(|(j, nm)| nm != d || done[j] || j == i));
            if ready {
                done[i] = true;
                out.push(i);
                progress = true;
            }
        }
        if !progress {
            break;
        }
    }
    out.extend((0..n).filter(|i| !done[*i]));
    out
}

#[cfg(test)]
mod tests;
