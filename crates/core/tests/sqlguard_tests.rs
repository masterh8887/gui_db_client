//! `sqlguard` rules: exactly one read-only statement, every quoting form understood, comments
//! allowed, DML/DDL/session statements rejected with the offending keyword, data-modifying
//! CTEs rejected, and `referenced_tables` as the plan-less fallback.

use bulk_core::sqlguard::{
    check, check_for, referenced_tables, referenced_tables_for, split_statements, strip_comments, Dialect, StatementKind, CODE_EMPTY, CODE_EXEC_COMMENT, CODE_MULTI,
    CODE_NOT_SELECT,
};
use bulk_core::types::{Severity, TableRef};

fn single_code(sql: &str) -> String {
    let c = check(sql);
    assert!(!c.ok, "{sql:?} should be rejected");
    assert!(!c.issues.is_empty(), "{sql:?}: rejected without an issue");
    assert!(c.issues.iter().all(|i| i.severity == Severity::Danger), "{sql:?}: sqlguard issues are blockers");
    c.issues[0].code.clone()
}

#[test]
fn accepts_read_only_statements() {
    let cases: &[(&str, StatementKind)] = &[
        ("select 1", StatementKind::Select),
        ("SELECT * FROM t", StatementKind::Select),
        ("  \n select 1 ;  ", StatementKind::Select),
        ("(select 1) union (select 2)", StatementKind::Select),
        ("/* head */ (select 1)", StatementKind::Select),
        ("-- head\nselect 1", StatementKind::Select),
        ("with x as (select 1) select * from x", StatementKind::With),
        ("WITH RECURSIVE r AS (SELECT 1 UNION ALL SELECT n + 1 FROM r) SELECT * FROM r", StatementKind::With),
        ("with x as (select 1), y as (select 2) select * from x, y", StatementKind::With),
        ("with x as materialized (select 1) table x", StatementKind::With),
        ("values (1), (2)", StatementKind::Values),
        ("VALUES (1, 'a;b')", StatementKind::Values),
        ("table t", StatementKind::Table),
        ("TABLE public.orders;", StatementKind::Table),
        ("select a from t where b in (select c from u)", StatementKind::Select),
        ("select * from (select 1) as into_sub", StatementKind::Select),
    ];
    for (sql, kind) in cases {
        let c = check(sql);
        assert!(c.ok, "{sql:?} should pass: {:?}", c.issues);
        assert!(c.issues.is_empty());
        assert_eq!(c.kind, *kind, "{sql:?}");
        assert_eq!(c.statements, 1, "{sql:?}");
    }
}

#[test]
fn normalized_drops_trailing_semicolon_and_keeps_comments() {
    let c = check("  -- head\nselect 1 /* c */\n;  ");
    assert!(c.ok);
    assert_eq!(c.normalized, "-- head\nselect 1 /* c */");

    let c = check("select 1; -- done");
    assert!(c.ok, "{:?}", c.issues);
    assert_eq!(c.normalized, "select 1");

    let c = check("select 1;;");
    assert!(c.ok, "empty statements are dropped: {:?}", c.issues);
    assert_eq!(c.statements, 1);
    assert_eq!(c.normalized, "select 1");
}

#[test]
fn multi_line_query_with_comments_and_semicolons_in_quotes_passes() {
    let sql = "-- pull orders; all of them\n\
               SELECT o.id, 'a;b' AS tag, /* inline ; comment */ o.total\n\
               FROM public.orders o\n\
               WHERE o.note = 'it''s; fine' -- trailing; comment\n\
                 AND o.tag = $$x;y$$ AND o.other = $q$a;b$q$\n\
                 AND o.\"col;name\" = 1 AND o.`b;t` = 2 AND o.[m;s] = 3\n\
                 AND o.e = E'x\\';y';";
    let c = check(sql);
    assert!(c.ok, "{:?}", c.issues);
    assert_eq!(c.kind, StatementKind::Select);
    assert_eq!(c.statements, 1);
    assert!(c.normalized.ends_with("E'x\\';y'"), "trailing ';' removed: {:?}", c.normalized);
    assert!(c.normalized.starts_with("-- pull orders; all of them"), "comments preserved");
    assert!(c.normalized.contains("/* inline ; comment */"));
    assert_eq!(referenced_tables(sql), vec![TableRef::new(Some("public"), "orders")]);
}

#[test]
fn rejects_multiple_statements() {
    let c = check("select 1; select 2");
    assert!(!c.ok);
    assert_eq!(c.statements, 2);
    assert_eq!(c.kind, StatementKind::Select, "kind of the first statement");
    assert_eq!(c.issues.len(), 1);
    assert_eq!(c.issues[0].code, CODE_MULTI);
    assert!(c.issues[0].message.contains("2 statements"), "{}", c.issues[0].message);
    assert_eq!(c.issues[0].severity, Severity::Danger);

    let c = check("begin; select 1; commit");
    assert_eq!(c.statements, 3);
    assert_eq!(c.issues[0].code, CODE_MULTI);
    assert!(c.issues[0].detail.as_deref().unwrap_or_default().contains("begin, select, commit"), "{:?}", c.issues[0].detail);

    assert_eq!(single_code("select 1;\n-- comment\nselect 2;"), CODE_MULTI);
    assert_eq!(single_code("set work_mem = '64MB'; select 1"), CODE_MULTI);
}

#[test]
fn rejects_empty_queries() {
    for sql in ["", "   \n\t ", "-- only a comment", "/* block */", ";", "; ;", "/* a */ -- b\n;"] {
        let c = check(sql);
        assert!(!c.ok, "{sql:?}");
        assert_eq!(c.kind, StatementKind::Empty, "{sql:?}");
        assert_eq!(c.statements, 0, "{sql:?}");
        assert_eq!(c.issues.len(), 1, "{sql:?}");
        assert_eq!(c.issues[0].code, CODE_EMPTY, "{sql:?}");
    }
}

#[test]
fn rejects_every_non_select_keyword_naming_it() {
    let cases: &[(&str, &str)] = &[
        ("explain select 1", "EXPLAIN"),
        ("EXPLAIN (ANALYZE, FORMAT JSON) SELECT * FROM t", "EXPLAIN"),
        ("set work_mem = '64MB'", "SET"),
        ("SET SESSION CHARACTERISTICS AS TRANSACTION READ ONLY", "SET"),
        ("insert into t values (1)", "INSERT"),
        ("Insert Into t Select * From u", "INSERT"),
        ("update t set a = 1", "UPDATE"),
        ("delete from t where 1 = 1", "DELETE"),
        ("merge into t using u on t.id = u.id when matched then update set a = 1", "MERGE"),
        ("create table t (a int)", "CREATE"),
        ("CREATE TABLE AS SELECT * FROM t", "CREATE"),
        ("drop table t", "DROP"),
        ("alter table t add column b int", "ALTER"),
        ("truncate t", "TRUNCATE"),
        ("grant select on t to bob", "GRANT"),
        ("revoke select on t from bob", "REVOKE"),
        ("copy t to stdout", "COPY"),
        ("COPY (SELECT 1) TO '/tmp/x.csv'", "COPY"),
        ("call refresh_stats()", "CALL"),
        ("do $$ begin raise notice 'a;b'; end $$", "DO"),
        ("vacuum analyze t", "VACUUM"),
        ("analyze t", "ANALYZE"),
        ("begin", "BEGIN"),
        ("BEGIN TRANSACTION", "BEGIN"),
        ("commit", "COMMIT"),
        ("rollback", "ROLLBACK"),
        ("show tables", "SHOW"),
        ("exec sp_who", "EXEC"),
        ("use otherdb", "USE"),
        ("lock table t in access exclusive mode", "LOCK"),
        ("frobnicate the database", "FROBNICATE"),
        ("(insert into t values (1))", "INSERT"),
        ("/* c */ -- d\n DELETE FROM t", "DELETE"),
    ];
    for (sql, keyword) in cases {
        let c = check(sql);
        assert!(!c.ok, "{sql:?} must be rejected");
        assert_eq!(c.statements, 1, "{sql:?}: quoted ';' must not split");
        assert_eq!(c.issues.len(), 1, "{sql:?}: {:?}", c.issues);
        assert_eq!(c.issues[0].code, CODE_NOT_SELECT, "{sql:?}");
        assert!(c.issues[0].message.contains(keyword), "{sql:?}: message {:?} should name {keyword}", c.issues[0].message);
        assert_eq!(c.issues[0].severity, Severity::Danger);
        let expected_kind = if *keyword == "EXPLAIN" { StatementKind::Explain } else { StatementKind::Other };
        assert_eq!(c.kind, expected_kind, "{sql:?}");
    }
}

#[test]
fn rejects_data_modifying_ctes() {
    let cases: &[(&str, &str)] = &[
        ("with x as (select 1) insert into t select * from x", "INSERT"),
        ("WITH x AS (SELECT 1) UPDATE t SET a = 1 FROM x", "UPDATE"),
        ("with x as (select 1) delete from t using x", "DELETE"),
        ("with x as (select 1) merge into t using x on 1 = 1 when matched then delete", "MERGE"),
        ("with d as (delete from t returning *) select * from d", "DELETE"),
        ("with u as (update t set a = 1 returning id) select count(*) from u", "UPDATE"),
        ("with i as (insert into t values (1) returning id), s as (select 1) select * from i, s", "INSERT"),
        ("with d as materialized (delete from t returning *) select 1", "DELETE"),
    ];
    for (sql, keyword) in cases {
        let c = check(sql);
        assert!(!c.ok, "{sql:?} must be rejected");
        assert_eq!(c.kind, StatementKind::With, "{sql:?}");
        assert!(c.issues.iter().all(|i| i.code == CODE_NOT_SELECT), "{sql:?}: {:?}", c.issues);
        assert!(c.issues.iter().any(|i| i.message.contains(keyword)), "{sql:?}: {:?}", c.issues);
    }
    // A CTE body that merely mentions a DML keyword inside a string or a nested select is fine.
    let c = check("with x as (select 'delete' as op, (select max(id) from t) as m) select * from x");
    assert!(c.ok, "{:?}", c.issues);
}

#[test]
fn rejects_select_into() {
    for sql in ["select * into new_t from t", "SELECT a INTO TEMP tmp FROM t", "select 1 into @v"] {
        assert_eq!(single_code(sql), CODE_NOT_SELECT, "{sql:?}");
        assert!(check(sql).issues[0].message.contains("INTO"), "{sql:?}");
    }
    // INTO inside a subquery is not a top-level INTO (and would fail on the server anyway).
    assert!(check("select * from t where a in (select b from u)").ok);
}

#[test]
fn strip_comments_handles_every_form() {
    let sql = "select 1 -- c\n, 2 /* b /* nested */ end */, '--not' , \"/*id*/\" from t";
    assert_eq!(strip_comments(sql), "select 1 \n, 2  , '--not' , \"/*id*/\" from t");
    assert_eq!(strip_comments("-- only"), "");
    assert_eq!(strip_comments("a/*unterminated"), "a ");
    assert_eq!(strip_comments("select $$ -- not a comment $$ from t"), "select $$ -- not a comment $$ from t");
    assert_eq!(strip_comments("select `--x`, [/*y*/] from t"), "select `--x`, [/*y*/] from t");
}

#[test]
fn split_statements_respects_every_quoting_form() {
    let sql = "select 'a;b'; select \"c;d\"; select `e;f`; select [g;h]; select $$i;j$$; select $t$k;l$t$; select E'x\\';y'; select 'it''s;'";
    let parts = split_statements(sql);
    assert_eq!(parts.len(), 8, "{parts:?}");
    assert_eq!(parts[0], "select 'a;b'");
    assert_eq!(parts[6], "select E'x\\';y'");
    assert_eq!(parts[7], "select 'it''s;'");

    // Comments stay with their statement (the `;` inside them does not split).
    assert_eq!(split_statements("-- a;b\nselect 1 /* ; */"), vec!["-- a;b\nselect 1 /* ; */".to_string()]);
    assert_eq!(split_statements("select 1;\n\n;select 2;"), vec!["select 1".to_string(), "select 2".to_string()]);
    assert!(split_statements(" ; -- x\n /* y */ ").is_empty());
    // Dollar quoting must not trigger on positional parameters.
    assert_eq!(split_statements("select $1; select $2").len(), 2);
    // Nested block comments: the inner `*/` does not end the comment.
    assert_eq!(split_statements("select 1 /* a /* b */ ; */; select 2").len(), 2);
}

#[test]
fn referenced_tables_handles_schemas_quotes_joins_and_subqueries() {
    let sql = "select o.id, i.qty\n\
               from public.orders o, app.dbo.three t3\n\
               join \"Sales\".\"Items\" i on o.id = i.order_id\n\
               left outer join [dbo].[Users] as u on u.id = o.user_id\n\
               inner join `db`.`t` z on z.k = o.k\n\
               where exists (select 1 from sub.q where q.id = o.id)";
    let tables = referenced_tables(sql);
    let expected = [
        TableRef::new(Some("public"), "orders"),
        TableRef::new(Some("Sales"), "Items"),
        TableRef::new(Some("dbo"), "Users"),
        TableRef::new(Some("db"), "t"),
        TableRef { database: Some("app".into()), schema: Some("dbo".into()), name: "three".into() },
        TableRef::new(Some("sub"), "q"),
    ];
    for e in &expected {
        assert!(tables.contains(e), "missing {e:?} in {tables:?}");
    }
    for bogus in ["o", "i", "u", "z", "t3", "exists", "where", "as", "on"] {
        assert!(!tables.iter().any(|t| t.name.eq_ignore_ascii_case(bogus)), "alias/keyword {bogus} captured: {tables:?}");
    }
}

#[test]
fn referenced_tables_skips_subqueries_functions_and_dedups() {
    // Derived table: the "(" after FROM is skipped, the JOIN after it is still read.
    assert_eq!(referenced_tables("select * from (select 1 from inner_t) sq join t2 on 1 = 1"), vec![TableRef::new(None, "t2")]);
    // Table-valued functions are not tables.
    assert!(referenced_tables("select * from generate_series(1, 10) g").is_empty());
    // EXTRACT(... FROM ...) / SUBSTRING(... FROM ...) are not FROM clauses.
    assert_eq!(referenced_tables("select extract(year from created_at), substring(x from 2) from t"), vec![TableRef::new(None, "t")]);
    // Same table twice: one entry. Comma lists: both.
    assert_eq!(referenced_tables("select * from t a join t b on a.id = b.id"), vec![TableRef::new(None, "t")]);
    assert_eq!(referenced_tables("select * from a, b where a.x = b.x"), vec![TableRef::new(None, "a"), TableRef::new(None, "b")]);
    // Comments in the way do not matter.
    assert_eq!(referenced_tables("select * from /* c */ s.t -- x\n where 1 = 1"), vec![TableRef::new(Some("s"), "t")]);
    // No FROM at all.
    assert!(referenced_tables("select 1").is_empty());
    assert!(referenced_tables("values (1)").is_empty());
}

// ---------------------------------------------------------------------------------------------
// Dialects: the scanner must read quotes and comments the way the server does
// ---------------------------------------------------------------------------------------------

/// First issue code of a rejected query under `dialect`.
fn code_for(sql: &str, dialect: Dialect) -> String {
    let c = check_for(sql, dialect);
    assert!(!c.ok, "{sql:?} should be rejected under {dialect:?}");
    assert!(c.issues.iter().all(|i| i.severity == Severity::Danger), "{sql:?}: sqlguard issues are blockers");
    c.issues[0].code.clone()
}

/// Each query is one statement to a PostgreSQL-style scanner but several to MySQL: the `;`
/// that standard rules hide in a string or comment is code to the MySQL server.
const MYSQL_ONLY_MULTI: [&str; 7] = [
    // `#` starts a line comment in MySQL, so the quote after it opens nothing.
    "select 1 # '\n; delete from t; -- '",
    // MySQL block comments do not nest: the first `*/` ends the comment.
    "select 1 /* /* */ ; delete from t; /* */",
    // Backslash escapes the quote inside '...' (MySQL default sql_mode).
    r"select 'a\', ' ; delete from t; -- '",
    // ... and inside "..." (a string in MySQL's default sql_mode).
    r#"select "a\", " ; delete from t; -- ""#,
    // `--` without a following space is two minus signs in MySQL.
    "select 1--1; delete from t",
    // No dollar quoting in MySQL (`$a$` is an identifier).
    "select $a$, 1; delete from t; select $a$",
    // No [bracket] identifiers in MySQL.
    "select 1 as [x; delete from t;]",
];

#[test]
fn mysql_lexing_exposes_hidden_statements() {
    for sql in MYSQL_ONLY_MULTI {
        let std = check_for(sql, Dialect::Standard);
        assert!(std.ok, "{sql:?} is a single SELECT under standard rules: {:?}", std.issues);
        assert_eq!(code_for(sql, Dialect::MySql), CODE_MULTI, "{sql:?}");
        // An unknown back end (ODBC) must assume the strictest reading.
        assert_eq!(code_for(sql, Dialect::Any), CODE_MULTI, "{sql:?}");
        assert!(check_for(sql, Dialect::MySql).statements >= 2, "{sql:?}");
    }
}

#[test]
fn mysql_checks_every_sql_mode_reading() {
    // One statement with backslash escapes, two under NO_BACKSLASH_ESCAPES.
    let sql = r"select 'a\'; delete from t; -- '";
    let c = check_for(sql, Dialect::MySql);
    assert!(!c.ok);
    assert_eq!(c.issues[0].code, CODE_MULTI);
    let detail = c.issues[0].detail.as_deref().unwrap_or_default();
    assert!(detail.contains("NO_BACKSLASH_ESCAPES"), "the failing reading is named: {detail}");

    // INTO hidden in a string with backslash escapes, visible without them.
    assert_eq!(code_for(r"select 'a\' into outfile '/tmp/f' -- '", Dialect::MySql), CODE_NOT_SELECT);

    // A double-quoted string under the default mode, an identifier under ANSI_QUOTES.
    assert_eq!(code_for(r#"select "a\"; delete from t; -- ""#, Dialect::MySql), CODE_MULTI);
}

#[test]
fn mysql_executable_comments_are_rejected() {
    for sql in ["select /*!40001 SQL_NO_CACHE */ * from t", "select 1 /*! ; delete from t */", "select 1 /*M!100100 , 2 */", "/*!50000 select 1 */"] {
        assert_eq!(code_for(sql, Dialect::MySql), CODE_EXEC_COMMENT, "{sql:?}");
        assert!(!check_for(sql, Dialect::Any).ok, "{sql:?}");
    }
    // A plain comment (or an optimizer hint) is fine; under standard rules `/*!` is just a comment.
    assert!(check_for("select /*+ MAX_EXECUTION_TIME(1000) */ * from t", Dialect::MySql).ok);
    assert!(check_for("select /*!40001 SQL_NO_CACHE */ * from t", Dialect::Standard).ok);
}

/// Every MySQL reading sees only comments plus an executable comment, while both standard
/// readings see a single SELECT: nested `/* /* */ ... */` hides the `#` from standard rules,
/// but MySQL ends the comment at the first `*/` and reads the rest of the line as a `#`
/// comment that swallows the SELECT. A MySQL server behind ODBC would run the body of the
/// executable comment, so the unknown-back-end check must fail.
#[test]
fn any_dialect_rejects_exec_comment_behind_mysql_only_comments() {
    for sql in [
        "/* /* */ # */ select 1\n/*!50000 delete from t */",
        "/* /* */ # */ select 1\n/*M!100100 drop table t */",
        "/* /* */ # */ select 1\r\n/*! update t set a = 1 */ # trailing",
    ] {
        let std = check_for(sql, Dialect::Standard);
        assert!(std.ok, "{sql:?} is a single SELECT under standard rules: {:?}", std.issues);
        assert_eq!(std.statements, 1, "{sql:?}");

        let my = check_for(sql, Dialect::MySql);
        assert_eq!(my.issues[0].code, CODE_EXEC_COMMENT, "{sql:?}");
        assert_eq!(my.statements, 0, "the MySQL reading sees no plain statement in {sql:?}");

        let any = check_for(sql, Dialect::Any);
        assert!(!any.ok, "{sql:?} must fail for an unknown back end");
        assert_eq!(any.issues[0].code, CODE_EXEC_COMMENT, "{sql:?}");
        let detail = any.issues[0].detail.as_deref().unwrap_or_default();
        assert!(detail.contains("MySQL"), "the failing reading is named: {detail}");
    }
}

/// A reading that sees nothing to run (only plain comments) is still not held against a
/// query the reference reading accepts.
#[test]
fn any_dialect_ignores_readings_that_see_only_plain_comments() {
    let sql = "/* /* */ # */ select 1";
    assert!(check_for(sql, Dialect::Standard).ok);
    assert_eq!(code_for(sql, Dialect::MySql), CODE_EMPTY);
    let any = check_for(sql, Dialect::Any);
    assert!(any.ok, "{:?}", any.issues);
    assert_eq!(any.statements, 1);
}

#[test]
fn mysql_accepts_ordinary_queries() {
    let cases = [
        "select * from t # trailing comment",
        "select * from t -- comment ; still comment\nwhere a = 1",
        r"select 'it\'s' from t",
        r"select * from t where path like 'C:\dir;x%'",
        "select \"a;b\", 'c;d', `e;f` from t;",
        "select a - -1, b-1 from t",
        "select /* one; */ 1 /* two; */",
        "with x as (select 1 as a) select a from x",
    ];
    for sql in cases {
        let c = check_for(sql, Dialect::MySql);
        assert!(c.ok, "{sql:?} should pass under MySQL rules: {:?}", c.issues);
        assert_eq!(c.statements, 1, "{sql:?}");
    }
    assert_eq!(check_for("select * from t # x;", Dialect::MySql).normalized, "select * from t # x");
    // DML is still DML.
    assert_eq!(code_for("delete from t # x", Dialect::MySql), CODE_NOT_SELECT);
    assert_eq!(code_for("# only a comment", Dialect::MySql), CODE_EMPTY);
}

#[test]
fn standard_rules_still_accept_postgres_syntax() {
    for sql in ["select data #>> '{a,b}' from t", "select $$a;b$$, $q$c;d$q$", r"select E'it\'s;' from t", "select 1 /* outer /* inner; */ still; */"] {
        let c = check(sql);
        assert!(c.ok, "{sql:?}: {:?}", c.issues);
        assert!(check_for(sql, Dialect::Standard).ok, "{sql:?}");
    }
    // Dollar quoting means nothing to MySQL, so the same text splits there.
    assert_eq!(code_for("select $$a;b$$", Dialect::MySql), CODE_MULTI);
}

#[test]
fn standard_checks_both_line_comment_endings() {
    // PostgreSQL ends a `--` comment at CR; SQL Server / SQLite only at LF. Under the LF-only
    // reading the quote is part of the comment and the `;` after the newline is code.
    let sql = "select 1 -- x\r'\n; delete from t; -- '";
    let c = check(sql);
    assert!(!c.ok);
    assert_eq!(c.issues[0].code, CODE_MULTI);
    assert!(c.issues[0].detail.as_deref().unwrap_or_default().contains("line feed"), "{:?}", c.issues[0].detail);
    // CRLF files are unaffected.
    assert!(check("-- don't\r\nselect 1\r\n").ok);
}

#[test]
fn referenced_tables_follow_the_dialect() {
    let sql = "select * from orders # main table\n, items";
    assert_eq!(referenced_tables_for(sql, Dialect::MySql), vec![TableRef::new(None, "orders"), TableRef::new(None, "items")]);
    // Standard rules read `#` as an operator and stop the FROM list there.
    assert_eq!(referenced_tables_for(sql, Dialect::Standard), vec![TableRef::new(None, "orders")]);
    assert_eq!(referenced_tables(sql), referenced_tables_for(sql, Dialect::Standard));
}
