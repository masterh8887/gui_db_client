//! Query validation without a full SQL parser: strip comments and string literals, split on
//! `;`, and make sure there is exactly one statement that starts with `SELECT`, `WITH`,
//! `VALUES` or `TABLE`. DML/DDL (`INSERT`, `UPDATE`, `DELETE`, `CREATE`, `DROP`, `ALTER`,
//! `TRUNCATE`, `GRANT`, `COPY`, `SET`, ...) is rejected at this layer. `SET` belongs to the
//! session panel. Comments inside a multi-line query are fine.
//!
//! The scanner must read the query exactly as the server will, otherwise a `;` that the scanner
//! believes is inside a string or a comment can end a statement on the server (MySQL runs the
//! extraction over the text protocol with multi-statements enabled, so a hidden second
//! statement would really run). It therefore lexes per [`Dialect`]:
//!
//! * [`Dialect::Standard`] (PostgreSQL, SQL Server, SQLite, Oracle): `'...'` strings with `''`
//!   escapes (`E'...'` additionally honours `\'`), `"..."` / `` `...` `` / `[...]` identifiers,
//!   dollar quoting (`$$ ... $$`, `$tag$ ... $tag$`), `--` line comments and nested
//!   `/* ... */` block comments.
//! * [`Dialect::MySql`] (MySQL, MariaDB): `#` line comments, `--` comments only when followed by
//!   whitespace or a control character, block comments that end at the first `*/` (no nesting),
//!   backslash escapes inside `'...'` and `"..."`, no dollar quoting and no `[...]`. Executable
//!   comments (`/*! ... */`, `/*M! ... */`) are rejected: their content runs as SQL.
//! * [`Dialect::Any`] (ODBC, unknown back end): the query must pass under every rule set.
//!
//! Where the server's reading depends on settings the guard cannot see (MySQL `sql_mode`
//! `NO_BACKSLASH_ESCAPES` / `ANSI_QUOTES`, whether a lone CR ends a `--` comment), the query is
//! checked under every possible reading and must pass all of them (fail closed).
//!
//! [`check`] uses the standard rules; callers that know the driver use [`check_for`].

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::types::{PreflightIssue, Severity, TableRef};

/// What the (first) statement of a query is, by its leading keyword. On the wire (`sql_check`)
/// it is the lower-case name: `select | with | values | table | explain | empty | other`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "lowercase")]
pub enum StatementKind {
    Select,
    With,
    Values,
    Table,
    Explain,
    Empty,
    Other,
}

/// Result of the SQL guard (`sqlguard::check` / `sqlguard::check_for`); also the `sql_check`
/// IPC payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct SqlCheck {
    /// `true` when the text is exactly one read-only statement.
    pub ok: bool,
    pub kind: StatementKind,
    /// Number of statements found (0 for an empty query).
    pub statements: usize,
    /// Why the query is refused (empty when `ok`).
    pub issues: Vec<PreflightIssue>,
    /// The single statement with the trailing `;` and surrounding whitespace removed. This is
    /// the text that reaches the driver (EXPLAIN, the extraction itself).
    pub normalized: String,
}

/// Issue code: the query has no statement at all.
pub const CODE_EMPTY: &str = "SQL_EMPTY";
/// Issue code: more than one statement.
pub const CODE_MULTI: &str = "MULTI_STATEMENT";
/// Issue code: the statement is not a read-only `SELECT` / `WITH` / `VALUES` / `TABLE`.
pub const CODE_NOT_SELECT: &str = "SQL_NOT_SELECT";

/// Keywords that start a data-modifying statement (also looked for inside / after CTEs).
const DML: [&str; 4] = ["INSERT", "UPDATE", "DELETE", "MERGE"];
/// Keywords that can start the main statement after a `WITH` clause.
const CTE_MAIN: [&str; 7] = ["SELECT", "INSERT", "UPDATE", "DELETE", "MERGE", "VALUES", "TABLE"];

/// Issue code: a MySQL / MariaDB executable comment (`/*! ... */`, `/*M! ... */`).
pub const CODE_EXEC_COMMENT: &str = "SQL_EXEC_COMMENT";

// ---------------------------------------------------------------------------------------------
// Dialects
// ---------------------------------------------------------------------------------------------

/// Lexical rules of the server that will run the query (see the module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Dialect {
    /// PostgreSQL / ANSI rules; also right for SQL Server, SQLite and Oracle.
    #[default]
    Standard,
    /// MySQL and MariaDB.
    MySql,
    /// Back end unknown (ODBC): the query must pass under every rule set.
    Any,
}

impl Dialect {
    /// Dialect for a driver id (`Source::driver_id()`, `Connection.driver`). Unknown ids (ODBC,
    /// future drivers) get [`Dialect::Any`] so the check fails closed.
    pub fn from_driver(driver_id: &str) -> Dialect {
        match driver_id.trim().to_ascii_lowercase().as_str() {
            "mysql" | "mariadb" => Dialect::MySql,
            "postgres" | "postgresql" | "mssql" | "sqlite" | "oracle" => Dialect::Standard,
            _ => Dialect::Any,
        }
    }

    /// Every reading the server may apply; the first is the reference for the normalized text.
    fn lexers(self) -> &'static [Lexer] {
        match self {
            Dialect::Standard => &[LEX_STANDARD, LEX_STANDARD_LF],
            Dialect::MySql => &[LEX_MYSQL, LEX_MYSQL_ANSI_QUOTES, LEX_MYSQL_NO_BACKSLASH],
            Dialect::Any => &[LEX_STANDARD, LEX_STANDARD_LF, LEX_MYSQL, LEX_MYSQL_ANSI_QUOTES, LEX_MYSQL_NO_BACKSLASH],
        }
    }
}

/// One concrete set of lexing rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Lexer {
    /// MySQL / MariaDB lexing: `#` comments, `-- ` needs whitespace, flat block comments,
    /// executable comments, no dollar quoting, no `[...]` identifiers, no `E'...'` prefix.
    mysql: bool,
    /// A lone `\r` ends a `--` comment as well as `\n` (PostgreSQL).
    cr_ends_line_comment: bool,
    /// Backslash escapes inside every `'...'` (MySQL without `NO_BACKSLASH_ESCAPES`).
    single_backslash: bool,
    /// `"..."` is a string with backslash escapes (MySQL without `ANSI_QUOTES` /
    /// `NO_BACKSLASH_ESCAPES`) instead of an identifier.
    double_backslash: bool,
    /// Named in issue details when this reading is the one that fails.
    label: &'static str,
}

const LEX_STANDARD: Lexer = Lexer { mysql: false, cr_ends_line_comment: true, single_backslash: false, double_backslash: false, label: "PostgreSQL rules" };
const LEX_STANDARD_LF: Lexer = Lexer {
    mysql: false,
    cr_ends_line_comment: false,
    single_backslash: false,
    double_backslash: false,
    label: "SQL Server / SQLite rules (a -- comment ends only at a line feed)",
};
const LEX_MYSQL: Lexer = Lexer { mysql: true, cr_ends_line_comment: false, single_backslash: true, double_backslash: true, label: "MySQL default sql_mode" };
const LEX_MYSQL_ANSI_QUOTES: Lexer =
    Lexer { mysql: true, cr_ends_line_comment: false, single_backslash: true, double_backslash: false, label: "MySQL sql_mode ANSI_QUOTES" };
const LEX_MYSQL_NO_BACKSLASH: Lexer =
    Lexer { mysql: true, cr_ends_line_comment: false, single_backslash: false, double_backslash: false, label: "MySQL sql_mode NO_BACKSLASH_ESCAPES" };

/// Validate an extraction query with the standard (PostgreSQL / ANSI) lexing rules.
///
/// Use [`check_for`] when the driver is known: MySQL reads quotes and comments differently.
pub fn check(sql: &str) -> SqlCheck {
    check_for(sql, Dialect::Standard)
}

/// Validate an extraction query for the server dialect that will run it.
///
/// The query is checked under every reading the dialect allows (for MySQL: default
/// `sql_mode`, `ANSI_QUOTES`, `NO_BACKSLASH_ESCAPES`) and passes only when every reading sees
/// one read-only statement. A reading that sees nothing at all to run (only whitespace and
/// plain comments, reported as [`CODE_EMPTY`]) is not held against it; a reading that finds a
/// MySQL executable comment always is, because the server runs that comment's content even
/// when nothing else in the text is code.
pub fn check_for(sql: &str, dialect: Dialect) -> SqlCheck {
    let lexers = dialect.lexers();
    let first = check_with(sql, lexers[0]);
    if !first.ok {
        return first;
    }
    for &lex in &lexers[1..] {
        let alt = check_with(sql, lex);
        if alt.ok || sees_nothing_to_run(&alt) {
            continue;
        }
        let issues = alt.issues.into_iter().map(|i| annotate(i, lex.label)).collect();
        return SqlCheck { ok: false, kind: alt.kind, statements: alt.statements, issues, normalized: first.normalized };
    }
    first
}

/// True when an alternate reading failed only because it found nothing to execute.
///
/// Such a reading means that server would run nothing, which is harmless, so it may differ
/// from the reference reading. `statements == 0` alone is not enough: [`check_with`] reports
/// a MySQL executable comment with the statement count taken after the comment was blanked,
/// so a text made only of comments and a `/*! ... */` body also counts 0 statements there,
/// yet the server runs the body as SQL.
fn sees_nothing_to_run(c: &SqlCheck) -> bool {
    c.statements == 0 && !c.issues.is_empty() && c.issues.iter().all(|i| i.code == CODE_EMPTY)
}

/// Say which reading produced an issue that the reference reading did not have.
fn annotate(mut issue: PreflightIssue, label: &str) -> PreflightIssue {
    let note = format!("Read with {label}, which quote and comment differently from the first reading.");
    issue.detail = Some(match issue.detail.take() {
        Some(d) => format!("{note} {d}"),
        None => note,
    });
    issue
}

/// [`check`] for one concrete set of lexing rules.
fn check_with(sql: &str, lex: Lexer) -> SqlCheck {
    let statements = split_with(sql, lex);
    let normalized = statements.first().map(|s| normalize(s)).unwrap_or_else(|| sql.trim().to_string());

    if lex.mysql && scan(sql, lex).iter().any(|s| s.region == Region::ExecComment) {
        let kind = statements.first().map(|s| classify(&first_keyword(s, lex).unwrap_or_default())).unwrap_or(StatementKind::Empty);
        return SqlCheck {
            ok: false,
            kind,
            statements: statements.len(),
            issues: vec![issue(
                CODE_EXEC_COMMENT,
                "The query contains a MySQL executable comment (/*! ... */ or /*M! ... */); the server runs its content as SQL, so it is not allowed in an extraction query.",
                None,
                Some("Remove the executable comment, or write its content as plain SQL. Optimizer hints (/*+ ... */) are fine."),
            )],
            normalized,
        };
    }

    if statements.is_empty() {
        return SqlCheck {
            ok: false,
            kind: StatementKind::Empty,
            statements: 0,
            issues: vec![issue(CODE_EMPTY, "The query is empty.", None, Some("Enter a single SELECT statement."))],
            normalized,
        };
    }

    if statements.len() > 1 {
        let kinds: Vec<String> = statements.iter().map(|s| first_keyword(s, lex).unwrap_or_else(|| "?".into())).collect();
        let kind = classify(&kinds[0]);
        return SqlCheck {
            ok: false,
            kind,
            statements: statements.len(),
            issues: vec![issue(
                CODE_MULTI,
                format!("The query contains {} statements; only a single SELECT can be extracted.", statements.len()),
                Some(format!("Statements start with: {}", kinds.join(", "))),
                Some("Remove the extra statements. A ';' inside a string literal or a comment is fine."),
            )],
            normalized,
        };
    }

    let stmt = &statements[0];
    let keyword = first_keyword(stmt, lex).unwrap_or_default();
    let kind = classify(&keyword);
    let mut issues = Vec::new();

    match kind {
        StatementKind::Select | StatementKind::With | StatementKind::Values | StatementKind::Table => {
            let toks = tokenize(&strip_with(stmt, lex), lex);
            if kind == StatementKind::With {
                let cte = analyze_cte(&toks);
                if let Some(kw) = cte.modifying_body {
                    issues.push(issue(
                        CODE_NOT_SELECT,
                        format!("The WITH clause contains a {kw} (data-modifying CTE); only read-only queries can be extracted."),
                        None,
                        Some("Remove the data-modifying CTE or run it in a database client."),
                    ));
                }
                if let Some(kw) = cte.main.as_deref().filter(|k| DML.contains(k)) {
                    issues.push(issue(
                        CODE_NOT_SELECT,
                        format!("The WITH query ends in {kw} (data-modifying CTE); only SELECT / VALUES / TABLE can follow a WITH clause."),
                        None,
                        Some("BulkDB only reads data; run this statement in a database client."),
                    ));
                }
            }
            if issues.is_empty() && has_select_into(&toks) {
                issues.push(issue(
                    CODE_NOT_SELECT,
                    "SELECT ... INTO writes to a table or a file on the server; only a plain SELECT can be extracted.",
                    None,
                    Some("Remove the INTO clause; BulkDB writes the result to a local file."),
                ));
            }
        }
        StatementKind::Explain => issues.push(issue(
            CODE_NOT_SELECT,
            "The statement starts with EXPLAIN; only SELECT / WITH / VALUES / TABLE can be extracted.",
            None,
            Some("Use the Explain button to see the plan, and extract the query itself."),
        )),
        StatementKind::Other => {
            let kw = keyword.to_ascii_uppercase();
            let recommendation = match kw.as_str() {
                "SET" | "RESET" => "Session settings (work_mem, timeouts, parallel workers) belong in the session panel.",
                "BEGIN" | "START" | "COMMIT" | "ROLLBACK" | "END" => "BulkDB runs the query in its own read-only session; transaction control is not needed.",
                _ => "BulkDB only reads data; run this statement in a database client.",
            };
            issues.push(issue(
                CODE_NOT_SELECT,
                format!("The statement starts with {kw}; only SELECT / WITH / VALUES / TABLE can be extracted."),
                None,
                Some(recommendation),
            ));
        }
        StatementKind::Empty => issues.push(issue(CODE_EMPTY, "The query is empty.", None, Some("Enter a single SELECT statement."))),
    }

    SqlCheck { ok: issues.is_empty(), kind, statements: 1, issues, normalized }
}

fn issue(code: &str, message: impl Into<String>, detail: Option<String>, recommendation: Option<&str>) -> PreflightIssue {
    PreflightIssue { code: code.to_string(), severity: Severity::Danger, message: message.into(), detail, recommendation: recommendation.map(|s| s.to_string()) }
}

/// Trim, drop one trailing `;` (and whitespace before it), keep comments.
fn normalize(stmt: &str) -> String {
    let s = stmt.trim();
    let s = s.strip_suffix(';').map(str::trim_end).unwrap_or(s);
    s.to_string()
}

fn classify(keyword: &str) -> StatementKind {
    match keyword.to_ascii_uppercase().as_str() {
        "" => StatementKind::Empty,
        "SELECT" => StatementKind::Select,
        "WITH" => StatementKind::With,
        "VALUES" => StatementKind::Values,
        "TABLE" => StatementKind::Table,
        "EXPLAIN" => StatementKind::Explain,
        _ => StatementKind::Other,
    }
}

/// First word of a statement (comments removed, leading parentheses skipped), original case.
fn first_keyword(stmt: &str, lex: Lexer) -> Option<String> {
    let toks = tokenize(&strip_with(stmt, lex), lex);
    toks.iter().find(|t| !matches!(t, Tok::Punct(b'('))).map(|t| match t {
        Tok::Word(w) => w.clone(),
        Tok::Num(n) => n.clone(),
        Tok::Quoted(q) => format!("\"{q}\""),
        Tok::Str => "'...'".into(),
        Tok::Punct(c) => (*c as char).to_string(),
    })
}

// ---------------------------------------------------------------------------------------------
// Scanner: classify every byte as code / comment / string / quoted identifier
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Region {
    Code,
    LineComment,
    BlockComment,
    /// MySQL / MariaDB executable comment (`/*! ... */`, `/*M! ... */`): the server runs its
    /// content. Scanned like a block comment; [`check_for`] rejects any query that has one.
    ExecComment,
    /// String literal, including dollar-quoted strings.
    Str,
    /// Quoted identifier (`"x"`, `` `x` ``, `[x]`).
    Ident,
}

#[derive(Debug, Clone, Copy)]
struct Span {
    start: usize,
    end: usize,
    region: Region,
}

fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b >= 0x80
}

fn is_ident_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_' || b >= 0x80
}

/// Split `sql` into spans following the rules of `lex`. Spans cover the whole input, in order,
/// without gaps. An unterminated string or comment runs to the end of the input.
fn scan(sql: &str, lex: Lexer) -> Vec<Span> {
    let b = sql.as_bytes();
    let n = b.len();
    let mut spans: Vec<Span> = Vec::new();
    let mut i = 0;
    let mut code_start = 0;

    fn push_code(spans: &mut Vec<Span>, from: usize, to: usize) {
        if to > from {
            spans.push(Span { start: from, end: to, region: Region::Code });
        }
    }

    while i < n {
        let c = b[i];
        let next = b.get(i + 1).copied();

        // Line comments. `--` always starts one in standard SQL; MySQL needs whitespace or a
        // control character (or the end of input) after it, so `1--1` is `1 - -1` there.
        // MySQL also has `#`. Standard comments end at LF or CR, MySQL ones only at LF.
        let line_comment = if c == b'-' && next == Some(b'-') { !lex.mysql || b.get(i + 2).is_none_or(|&x| x <= b' ' || x == 0x7f) } else { lex.mysql && c == b'#' };
        if line_comment {
            push_code(&mut spans, code_start, i);
            let start = i;
            while i < n && b[i] != b'\n' && !(lex.cr_ends_line_comment && b[i] == b'\r') {
                i += 1;
            }
            spans.push(Span { start, end: i, region: Region::LineComment });
            code_start = i;
            continue;
        }

        // /* block comment */: nested in standard SQL, flat (first `*/` ends it) in MySQL.
        if c == b'/' && next == Some(b'*') {
            push_code(&mut spans, code_start, i);
            let start = i;
            let region = if lex.mysql && is_exec_comment_body(&b[i + 2..]) { Region::ExecComment } else { Region::BlockComment };
            i = if lex.mysql { flat_comment_end(b, i + 2) } else { nested_comment_end(b, i + 2) };
            spans.push(Span { start, end: i, region });
            code_start = i;
            continue;
        }

        // '...' string with '' escapes. Backslash escapes: every string in MySQL (unless
        // NO_BACKSLASH_ESCAPES), only E'...' strings in standard SQL.
        if c == b'\'' {
            let backslash = lex.single_backslash || (!lex.mysql && i > 0 && matches!(b[i - 1], b'E' | b'e') && (i < 2 || !is_ident_byte(b[i - 2])));
            push_code(&mut spans, code_start, i);
            let start = i;
            i = quoted_end(b, i, b'\'', backslash);
            spans.push(Span { start, end: i, region: Region::Str });
            code_start = i;
            continue;
        }

        // "..." identifier, or a backslash-escaped string in MySQL's default sql_mode;
        // `...` identifier. Doubled delimiters escape in both.
        if c == b'"' || c == b'`' {
            let as_string = c == b'"' && lex.double_backslash;
            push_code(&mut spans, code_start, i);
            let start = i;
            i = quoted_end(b, i, c, as_string);
            spans.push(Span { start, end: i, region: if as_string { Region::Str } else { Region::Ident } });
            code_start = i;
            continue;
        }

        // [...] identifiers (SQL Server), ]] escape. Not a quote in MySQL.
        if c == b'[' && !lex.mysql {
            push_code(&mut spans, code_start, i);
            let start = i;
            i = quoted_end(b, i, b']', false);
            spans.push(Span { start, end: i, region: Region::Ident });
            code_start = i;
            continue;
        }

        // $tag$ ... $tag$ (PostgreSQL); not when glued to an identifier (`x$1`). MySQL has no
        // dollar quoting (`$a$` is an identifier there).
        if c == b'$' && !lex.mysql && (i == 0 || !is_ident_byte(b[i - 1])) {
            if let Some(tag_len) = dollar_tag_len(&b[i..]) {
                push_code(&mut spans, code_start, i);
                let start = i;
                let delim = &b[i..i + tag_len];
                let body_start = i + tag_len;
                let end = match find_bytes(&b[body_start..], delim) {
                    Some(pos) => body_start + pos + tag_len,
                    None => n,
                };
                i = end;
                spans.push(Span { start, end, region: Region::Str });
                code_start = i;
                continue;
            }
        }
        i += 1;
    }
    push_code(&mut spans, code_start, n);
    spans
}

/// Index just past the closing `close` of a quoted token opened at `open`, or the end of input
/// when it is unterminated. A doubled `close` is an escaped delimiter; with `backslash`, a `\`
/// escapes the byte after it.
fn quoted_end(b: &[u8], open: usize, close: u8, backslash: bool) -> usize {
    let n = b.len();
    let mut i = open + 1;
    while i < n {
        if backslash && b[i] == b'\\' {
            i += 2;
            continue;
        }
        if b[i] == close {
            if b.get(i + 1) == Some(&close) {
                i += 2;
                continue;
            }
            return i + 1;
        }
        i += 1;
    }
    n
}

/// End of a standard (nesting) block comment whose body starts at `from`.
fn nested_comment_end(b: &[u8], from: usize) -> usize {
    let n = b.len();
    let mut i = from;
    let mut depth = 1usize;
    while i < n && depth > 0 {
        if b[i] == b'/' && b.get(i + 1) == Some(&b'*') {
            depth += 1;
            i += 2;
        } else if b[i] == b'*' && b.get(i + 1) == Some(&b'/') {
            depth -= 1;
            i += 2;
        } else {
            i += 1;
        }
    }
    i.min(n)
}

/// End of a MySQL block comment whose body starts at `from`: the first `*/` closes it.
fn flat_comment_end(b: &[u8], from: usize) -> usize {
    match find_bytes(&b[from..], b"*/") {
        Some(pos) => from + pos + 2,
        None => b.len(),
    }
}

/// Whether a MySQL block comment body (the bytes after `/*`) makes it an executable comment:
/// `/*!` (MySQL, MariaDB) or `/*M!` (MariaDB).
fn is_exec_comment_body(body: &[u8]) -> bool {
    matches!(body, [b'!', ..] | [b'M' | b'm', b'!', ..])
}

/// Length of a dollar-quote opener (`$$` = 2, `$tag$` = tag + 2) at the start of `b`, if any.
fn dollar_tag_len(b: &[u8]) -> Option<usize> {
    if b.first() != Some(&b'$') {
        return None;
    }
    let mut j = 1;
    if j < b.len() && is_ident_start(b[j]) {
        while j < b.len() && is_ident_byte(b[j]) {
            j += 1;
        }
    }
    (b.get(j) == Some(&b'$')).then_some(j + 1)
}

fn find_bytes(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Remove `--` line comments and `/* */` block comments (string literals preserved), using
/// the standard rules. A block comment becomes a single space so adjacent tokens stay
/// separate; a line comment is removed and its terminating newline kept.
pub fn strip_comments(sql: &str) -> String {
    strip_with(sql, LEX_STANDARD)
}

/// [`strip_comments`] for one set of lexing rules.
fn strip_with(sql: &str, lex: Lexer) -> String {
    let mut out = String::with_capacity(sql.len());
    for span in scan(sql, lex) {
        match span.region {
            Region::LineComment => {}
            Region::BlockComment | Region::ExecComment => out.push(' '),
            _ => out.push_str(&sql[span.start..span.end]),
        }
    }
    out
}

/// Split on `;` outside strings / comments / dollar-quotes, using the standard rules. Empty
/// statements are dropped (a piece that holds nothing but whitespace and comments counts as
/// empty). Each returned statement is trimmed and keeps its comments; the `;` is not included.
pub fn split_statements(sql: &str) -> Vec<String> {
    split_with(sql, LEX_STANDARD)
}

/// [`split_statements`] for one set of lexing rules.
fn split_with(sql: &str, lex: Lexer) -> Vec<String> {
    let bytes = sql.as_bytes();
    let mut pieces = Vec::new();
    let mut piece_start = 0;
    for span in scan(sql, lex) {
        if span.region != Region::Code {
            continue;
        }
        for i in span.start..span.end {
            if bytes[i] == b';' {
                pieces.push(&sql[piece_start..i]);
                piece_start = i + 1;
            }
        }
    }
    pieces.push(&sql[piece_start..]);
    pieces.into_iter().map(str::trim).filter(|p| !strip_with(p, lex).trim().is_empty()).map(|p| p.to_string()).collect()
}

// ---------------------------------------------------------------------------------------------
// Tokens (over comment-stripped SQL)
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
enum Tok {
    /// Unquoted word: keyword or identifier, original case.
    Word(String),
    /// Quoted identifier, delimiters removed and doubled delimiters unescaped.
    Quoted(String),
    /// String literal (content irrelevant here).
    Str,
    /// Numeric literal.
    Num(String),
    /// Any other single byte (`(`, `)`, `,`, `.`, `;`, operators).
    Punct(u8),
}

impl Tok {
    fn is_word(&self, kw: &str) -> bool {
        matches!(self, Tok::Word(w) if w.eq_ignore_ascii_case(kw))
    }

    fn word_upper(&self) -> Option<String> {
        match self {
            Tok::Word(w) => Some(w.to_ascii_uppercase()),
            _ => None,
        }
    }
}

fn unquote(raw: &str) -> String {
    let b = raw.as_bytes();
    if b.is_empty() {
        return String::new();
    }
    let (open, close) = match b[0] {
        b'[' => ("[", "]"),
        b'`' => ("`", "`"),
        _ => ("\"", "\""),
    };
    let inner = raw.strip_prefix(open).unwrap_or(raw);
    let inner = inner.strip_suffix(close).unwrap_or(inner);
    inner.replace(&format!("{close}{close}"), close)
}

fn tokenize(sql: &str, lex: Lexer) -> Vec<Tok> {
    let mut toks = Vec::new();
    let bytes = sql.as_bytes();
    for span in scan(sql, lex) {
        match span.region {
            Region::LineComment | Region::BlockComment | Region::ExecComment => {}
            Region::Str => toks.push(Tok::Str),
            Region::Ident => toks.push(Tok::Quoted(unquote(&sql[span.start..span.end]))),
            Region::Code => {
                let mut i = span.start;
                while i < span.end {
                    let c = bytes[i];
                    if c.is_ascii_whitespace() {
                        i += 1;
                    } else if is_ident_start(c) {
                        let s = i;
                        while i < span.end && (is_ident_byte(bytes[i]) || bytes[i] == b'$') {
                            i += 1;
                        }
                        toks.push(Tok::Word(sql[s..i].to_string()));
                    } else if c.is_ascii_digit() {
                        let s = i;
                        while i < span.end && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_' || bytes[i] == b'.') {
                            i += 1;
                        }
                        toks.push(Tok::Num(sql[s..i].to_string()));
                    } else {
                        toks.push(Tok::Punct(c));
                        i += 1;
                    }
                }
            }
        }
    }
    toks
}

// ---------------------------------------------------------------------------------------------
// Statement shape
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Default)]
struct CteShape {
    /// Keyword of the statement after the CTE list (`SELECT`, `INSERT`, ...), if found.
    main: Option<String>,
    /// First data-modifying keyword found at the start of a CTE body.
    modifying_body: Option<String>,
}

/// Inspect a `WITH ...` statement: which keyword starts the main statement and whether any CTE
/// body is data-modifying (`WITH d AS (DELETE ... RETURNING *) SELECT ...`).
fn analyze_cte(toks: &[Tok]) -> CteShape {
    let mut shape = CteShape::default();
    let mut depth = 0usize;
    let mut prev_depth0: Option<String> = None;
    let mut i = 0;
    while i < toks.len() {
        match &toks[i] {
            Tok::Punct(b'(') => {
                depth += 1;
                if depth == 1 && matches!(prev_depth0.as_deref(), Some("AS") | Some("MATERIALIZED")) {
                    // First word inside the CTE body (skipping nested opening parens).
                    let mut j = i + 1;
                    while let Some(Tok::Punct(b'(')) = toks.get(j) {
                        j += 1;
                    }
                    if let Some(kw) = toks.get(j).and_then(Tok::word_upper) {
                        if DML.contains(&kw.as_str()) && shape.modifying_body.is_none() {
                            shape.modifying_body = Some(kw);
                        }
                    }
                }
            }
            Tok::Punct(b')') => depth = depth.saturating_sub(1),
            Tok::Word(w) if depth == 0 => {
                let up = w.to_ascii_uppercase();
                if i > 0 && CTE_MAIN.contains(&up.as_str()) {
                    shape.main = Some(up);
                    return shape;
                }
                prev_depth0 = Some(up);
            }
            _ => {}
        }
        i += 1;
    }
    shape
}

/// `SELECT ... INTO` at the top level of the statement (PostgreSQL / MSSQL create a table,
/// MySQL writes a server-side file or a variable). Not a read-only query.
fn has_select_into(toks: &[Tok]) -> bool {
    let mut depth = 0usize;
    let mut in_select = false;
    for t in toks {
        match t {
            Tok::Punct(b'(') => depth += 1,
            Tok::Punct(b')') => depth = depth.saturating_sub(1),
            Tok::Word(_) if depth == 0 => {
                if t.is_word("SELECT") {
                    in_select = true;
                } else if in_select && t.is_word("INTO") {
                    return true;
                }
            }
            _ => {}
        }
    }
    false
}

// ---------------------------------------------------------------------------------------------
// Referenced tables
// ---------------------------------------------------------------------------------------------

/// Words that cannot be a table alias (so the FROM list ends there).
const NOT_ALIAS: [&str; 48] = [
    "WHERE",
    "ON",
    "USING",
    "JOIN",
    "LEFT",
    "RIGHT",
    "INNER",
    "OUTER",
    "FULL",
    "CROSS",
    "NATURAL",
    "GROUP",
    "ORDER",
    "HAVING",
    "LIMIT",
    "OFFSET",
    "FETCH",
    "UNION",
    "EXCEPT",
    "INTERSECT",
    "WINDOW",
    "FOR",
    "WITH",
    "SET",
    "RETURNING",
    "TABLESAMPLE",
    "STRAIGHT_JOIN",
    "LATERAL",
    "INTO",
    "QUALIFY",
    "SAMPLE",
    "PIVOT",
    "UNPIVOT",
    "OPTION",
    "START",
    "CONNECT",
    "MINUS",
    "END",
    "THEN",
    "ELSE",
    "WHEN",
    "AND",
    "OR",
    "NOT",
    "IS",
    "IN",
    "AS",
    "VALUES",
];

/// Functions whose argument list uses `FROM` for something that is not a table.
const FROM_FUNCTIONS: [&str; 6] = ["EXTRACT", "SUBSTRING", "TRIM", "POSITION", "OVERLAY", "XMLELEMENT"];

/// Best-effort list of tables referenced after `FROM` / `JOIN` (used only as a fallback when
/// the database gives no plan with relation names). Subqueries (`FROM (SELECT ...)`),
/// table-valued functions (`FROM generate_series(...)`) and `EXTRACT(... FROM ...)` are
/// skipped; `schema.table` / `db.schema.table` and every quoting form are understood.
///
/// Uses the standard lexing rules; see [`referenced_tables_for`].
pub fn referenced_tables(sql: &str) -> Vec<TableRef> {
    referenced_tables_for(sql, Dialect::Standard)
}

/// [`referenced_tables`] with the lexing rules of `dialect` (MySQL `#` comments, backslash
/// escapes, ...). For dialects with several readings the first (default) one is used.
pub fn referenced_tables_for(sql: &str, dialect: Dialect) -> Vec<TableRef> {
    let lex = dialect.lexers()[0];
    let toks = tokenize(&strip_with(sql, lex), lex);
    let mut out: Vec<TableRef> = Vec::new();
    let mut seen: BTreeSet<TableRef> = BTreeSet::new();
    // One entry per open paren: `true` when it is the argument list of a FROM_FUNCTION.
    let mut parens: Vec<bool> = Vec::new();
    let mut i = 0;
    while i < toks.len() {
        match &toks[i] {
            Tok::Punct(b'(') => {
                let fn_ctx = i > 0 && FROM_FUNCTIONS.iter().any(|f| toks[i - 1].is_word(f));
                parens.push(fn_ctx);
                i += 1;
            }
            Tok::Punct(b')') => {
                parens.pop();
                i += 1;
            }
            t if (t.is_word("FROM") || t.is_word("JOIN")) && !parens.last().copied().unwrap_or(false) => {
                i = collect_from_list(&toks, i + 1, &mut out, &mut seen);
            }
            _ => i += 1,
        }
    }
    out
}

/// Parse `name [alias] [, name [alias]]...` starting at `start`; returns where parsing stopped.
fn collect_from_list(toks: &[Tok], start: usize, out: &mut Vec<TableRef>, seen: &mut BTreeSet<TableRef>) -> usize {
    let mut j = start;
    loop {
        while matches!(toks.get(j), Some(t) if t.is_word("ONLY") || t.is_word("LATERAL")) {
            j += 1;
        }
        match toks.get(j) {
            Some(Tok::Punct(b'(')) => {
                // Subquery or parenthesised join: skip it entirely.
                j = skip_parens(toks, j);
            }
            Some(Tok::Word(_)) | Some(Tok::Quoted(_)) => {
                let mut parts: Vec<String> = Vec::new();
                loop {
                    match toks.get(j) {
                        Some(Tok::Word(w)) => parts.push(w.clone()),
                        Some(Tok::Quoted(q)) => parts.push(q.clone()),
                        _ => break,
                    }
                    j += 1;
                    if matches!(toks.get(j), Some(Tok::Punct(b'.'))) {
                        j += 1;
                    } else {
                        break;
                    }
                }
                if matches!(toks.get(j), Some(Tok::Punct(b'('))) {
                    // Table-valued function: not a table.
                    j = skip_parens(toks, j);
                } else if !parts.is_empty() {
                    let t = table_ref_from_parts(parts);
                    if seen.insert(t.clone()) {
                        out.push(t);
                    }
                }
            }
            _ => return j,
        }
        // Optional alias: `AS x`, `x`, `x (col, ...)`.
        if matches!(toks.get(j), Some(t) if t.is_word("AS")) {
            j += 1;
            if matches!(toks.get(j), Some(Tok::Word(_)) | Some(Tok::Quoted(_))) {
                j += 1;
            }
        } else {
            match toks.get(j) {
                Some(Tok::Word(w)) if !NOT_ALIAS.contains(&w.to_ascii_uppercase().as_str()) => j += 1,
                Some(Tok::Quoted(_)) => j += 1,
                _ => {}
            }
        }
        if matches!(toks.get(j), Some(Tok::Punct(b'('))) {
            j = skip_parens(toks, j);
        }
        if matches!(toks.get(j), Some(Tok::Punct(b','))) {
            j += 1;
            continue;
        }
        return j;
    }
}

/// Index just past the `)` matching the `(` at `open`.
fn skip_parens(toks: &[Tok], open: usize) -> usize {
    let mut depth = 0usize;
    let mut j = open;
    while j < toks.len() {
        match toks[j] {
            Tok::Punct(b'(') => depth += 1,
            Tok::Punct(b')') => {
                depth -= 1;
                if depth == 0 {
                    return j + 1;
                }
            }
            _ => {}
        }
        j += 1;
    }
    j
}

fn table_ref_from_parts(mut parts: Vec<String>) -> TableRef {
    let name = parts.pop().unwrap_or_default();
    let schema = parts.pop();
    let database = parts.pop();
    TableRef { database, schema, name }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scanner_keeps_semicolons_inside_quotes() {
        let sql = "select 'a;b', \"c;d\", `e;f`, [g;h], $$i;j$$, $t$k;l$t$ from t -- ;\n/* ; /* ; */ */;";
        assert_eq!(split_statements(sql).len(), 1);
    }

    #[test]
    fn dollar_tag_detection() {
        assert_eq!(dollar_tag_len(b"$$x"), Some(2));
        assert_eq!(dollar_tag_len(b"$tag$x"), Some(5));
        assert_eq!(dollar_tag_len(b"$1"), None);
        assert_eq!(dollar_tag_len(b"$ 1"), None);
    }

    #[test]
    fn tokenizer_unquotes() {
        let toks = tokenize("select \"a\"\"b\", [c]]d], `e``f` from x", LEX_STANDARD);
        assert_eq!(toks[1], Tok::Quoted("a\"b".into()));
        assert_eq!(toks[3], Tok::Quoted("c]d".into()));
        assert_eq!(toks[5], Tok::Quoted("e`f".into()));
    }

    fn regions(sql: &str, lex: Lexer) -> Vec<(Region, &str)> {
        scan(sql, lex).into_iter().map(|s| (s.region, &sql[s.start..s.end])).collect()
    }

    #[test]
    fn mysql_scanner_rules() {
        // `#` comment, flat block comment, `-- ` needs whitespace.
        assert_eq!(regions("a # x;\nb", LEX_MYSQL)[1], (Region::LineComment, "# x;"));
        assert_eq!(regions("/* /* */ ;", LEX_MYSQL)[0], (Region::BlockComment, "/* /* */"));
        assert_eq!(regions("1--1;", LEX_MYSQL), vec![(Region::Code, "1--1;")]);
        assert_eq!(regions("1-- x", LEX_MYSQL)[1], (Region::LineComment, "-- x"));
        assert_eq!(regions("1--", LEX_MYSQL)[1], (Region::LineComment, "--"));
        // Backslash escapes in both quote kinds by default; `"` is an identifier with ANSI_QUOTES.
        assert_eq!(regions(r"'a\';b'", LEX_MYSQL), vec![(Region::Str, r"'a\';b'")]);
        assert_eq!(regions(r#""a\";b""#, LEX_MYSQL), vec![(Region::Str, r#""a\";b""#)]);
        assert_eq!(regions(r#""a\";b""#, LEX_MYSQL_ANSI_QUOTES)[0], (Region::Ident, r#""a\""#));
        assert_eq!(regions(r"'a\';b'", LEX_MYSQL_NO_BACKSLASH)[0], (Region::Str, r"'a\'"));
        // No dollar quoting, no [identifiers], no E'' prefix.
        assert_eq!(regions("$a$;$a$", LEX_MYSQL), vec![(Region::Code, "$a$;$a$")]);
        assert_eq!(regions("[a;b]", LEX_MYSQL), vec![(Region::Code, "[a;b]")]);
        // Executable comments are recognised.
        assert_eq!(regions("/*!50000 x */", LEX_MYSQL)[0].0, Region::ExecComment);
        assert_eq!(regions("/*M!100100 x */", LEX_MYSQL)[0].0, Region::ExecComment);
        assert_eq!(regions("/*+ hint */", LEX_MYSQL)[0].0, Region::BlockComment);
    }

    #[test]
    fn standard_line_comment_end() {
        assert_eq!(regions("a -- x\r;b", LEX_STANDARD)[1], (Region::LineComment, "-- x"));
        assert_eq!(regions("a -- x\r;b", LEX_STANDARD_LF)[1], (Region::LineComment, "-- x\r;b"));
        assert_eq!(regions("a #b", LEX_STANDARD), vec![(Region::Code, "a #b")]);
    }

    #[test]
    fn dialect_from_driver() {
        assert_eq!(Dialect::from_driver("mysql"), Dialect::MySql);
        assert_eq!(Dialect::from_driver("MariaDB"), Dialect::MySql);
        assert_eq!(Dialect::from_driver("postgres"), Dialect::Standard);
        assert_eq!(Dialect::from_driver("mssql"), Dialect::Standard);
        assert_eq!(Dialect::from_driver("sqlite"), Dialect::Standard);
        assert_eq!(Dialect::from_driver("oracle"), Dialect::Standard);
        assert_eq!(Dialect::from_driver("odbc"), Dialect::Any);
        assert_eq!(Dialect::from_driver("something-new"), Dialect::Any);
    }

    /// The `sql_check` wire shape the frontend relies on: flat camelCase fields and a
    /// lower-case statement kind.
    #[test]
    fn sql_check_serialises_flat_camel_case() {
        let v = serde_json::to_value(check("select 1;")).unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(v["kind"], "select");
        assert_eq!(v["statements"], 1);
        assert_eq!(v["normalized"], "select 1");
        assert!(v["issues"].as_array().unwrap().is_empty());
        assert_eq!(v.as_object().unwrap().len(), 5, "{v}");

        let dml = serde_json::to_value(check("delete from t")).unwrap();
        assert_eq!(dml["ok"], false);
        assert_eq!(dml["kind"], "other");
        assert_eq!(dml["issues"][0]["code"], CODE_NOT_SELECT);
        assert_eq!(serde_json::to_value(check("")).unwrap()["kind"], "empty");

        let kinds = [
            (StatementKind::Select, "select"),
            (StatementKind::With, "with"),
            (StatementKind::Values, "values"),
            (StatementKind::Table, "table"),
            (StatementKind::Explain, "explain"),
            (StatementKind::Empty, "empty"),
            (StatementKind::Other, "other"),
        ];
        for (kind, name) in kinds {
            assert_eq!(serde_json::to_value(kind).unwrap(), name);
            assert_eq!(serde_json::from_value::<StatementKind>(name.into()).unwrap(), kind);
        }
        let back: SqlCheck = serde_json::from_value(dml).unwrap();
        assert_eq!(back, check("delete from t"));
    }
}
