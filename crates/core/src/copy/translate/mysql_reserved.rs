//! MySQL reserved words per version (from the MySQL "Keywords and Reserved Words" pages of the
//! 4.1, 5.0-5.7, 8.0, 8.4 and 9.x manuals), for the `RESERVED_WORD` notice (3.3). BulkDB always
//! quotes identifiers, so these words never break its own SQL; the notice is for the
//! applications that will use the copied tables.

/// Reserved in every version BulkDB copies from or to (3.23 / 4.0 base list).
const BASE: &[&str] = &[
    "ADD",
    "ALL",
    "ALTER",
    "ANALYZE",
    "AND",
    "AS",
    "ASC",
    "BEFORE",
    "BETWEEN",
    "BIGINT",
    "BINARY",
    "BLOB",
    "BOTH",
    "BY",
    "CASCADE",
    "CASE",
    "CHANGE",
    "CHAR",
    "CHARACTER",
    "CHECK",
    "COLLATE",
    "COLUMN",
    "CONSTRAINT",
    "CONVERT",
    "CREATE",
    "CROSS",
    "CURRENT_DATE",
    "CURRENT_TIME",
    "CURRENT_TIMESTAMP",
    "CURRENT_USER",
    "DATABASE",
    "DATABASES",
    "DAY_HOUR",
    "DAY_MICROSECOND",
    "DAY_MINUTE",
    "DAY_SECOND",
    "DEC",
    "DECIMAL",
    "DEFAULT",
    "DELAYED",
    "DELETE",
    "DESC",
    "DESCRIBE",
    "DISTINCT",
    "DISTINCTROW",
    "DIV",
    "DOUBLE",
    "DROP",
    "DUAL",
    "ELSE",
    "ENCLOSED",
    "ESCAPED",
    "EXISTS",
    "EXPLAIN",
    "FALSE",
    "FLOAT",
    "FLOAT4",
    "FLOAT8",
    "FOR",
    "FORCE",
    "FOREIGN",
    "FROM",
    "FULLTEXT",
    "GRANT",
    "GROUP",
    "HAVING",
    "HIGH_PRIORITY",
    "HOUR_MICROSECOND",
    "HOUR_MINUTE",
    "HOUR_SECOND",
    "IF",
    "IGNORE",
    "IN",
    "INDEX",
    "INFILE",
    "INNER",
    "INSERT",
    "INT",
    "INT1",
    "INT2",
    "INT3",
    "INT4",
    "INT8",
    "INTEGER",
    "INTERVAL",
    "INTO",
    "IS",
    "JOIN",
    "KEY",
    "KEYS",
    "KILL",
    "LEADING",
    "LEFT",
    "LIKE",
    "LIMIT",
    "LINES",
    "LOAD",
    "LOCALTIME",
    "LOCALTIMESTAMP",
    "LOCK",
    "LONG",
    "LONGBLOB",
    "LONGTEXT",
    "LOW_PRIORITY",
    "MATCH",
    "MEDIUMBLOB",
    "MEDIUMINT",
    "MEDIUMTEXT",
    "MIDDLEINT",
    "MINUTE_MICROSECOND",
    "MINUTE_SECOND",
    "MOD",
    "NATURAL",
    "NOT",
    "NO_WRITE_TO_BINLOG",
    "NULL",
    "NUMERIC",
    "ON",
    "OPTIMIZE",
    "OPTION",
    "OPTIONALLY",
    "OR",
    "ORDER",
    "OUTER",
    "OUTFILE",
    "PRECISION",
    "PRIMARY",
    "PROCEDURE",
    "PURGE",
    "READ",
    "REAL",
    "REFERENCES",
    "REGEXP",
    "RENAME",
    "REPLACE",
    "REQUIRE",
    "RESTRICT",
    "REVOKE",
    "RIGHT",
    "RLIKE",
    "SECOND_MICROSECOND",
    "SELECT",
    "SEPARATOR",
    "SET",
    "SHOW",
    "SMALLINT",
    "SPATIAL",
    "SQL_BIG_RESULT",
    "SQL_CALC_FOUND_ROWS",
    "SQL_SMALL_RESULT",
    "SSL",
    "STARTING",
    "STRAIGHT_JOIN",
    "TABLE",
    "TERMINATED",
    "THEN",
    "TINYBLOB",
    "TINYINT",
    "TINYTEXT",
    "TO",
    "TRAILING",
    "TRUE",
    "UNION",
    "UNIQUE",
    "UNLOCK",
    "UNSIGNED",
    "UPDATE",
    "USAGE",
    "USE",
    "USING",
    "UTC_DATE",
    "UTC_TIME",
    "UTC_TIMESTAMP",
    "VALUES",
    "VARBINARY",
    "VARCHAR",
    "VARCHARACTER",
    "VARYING",
    "WHEN",
    "WHERE",
    "WITH",
    "WRITE",
    "XOR",
    "YEAR_MONTH",
    "ZEROFILL",
];

/// New in 5.0 (stored programs, triggers, views).
const V5_0: &[&str] = &[
    "ASENSITIVE",
    "CALL",
    "CONDITION",
    "CONNECTION",
    "CONTINUE",
    "CURSOR",
    "DECLARE",
    "DETERMINISTIC",
    "EACH",
    "ELSEIF",
    "EXIT",
    "FETCH",
    "GOTO",
    "INOUT",
    "INSENSITIVE",
    "ITERATE",
    "LABEL",
    "LEAVE",
    "LOOP",
    "MODIFIES",
    "OUT",
    "READS",
    "RELEASE",
    "REPEAT",
    "RETURN",
    "SCHEMA",
    "SCHEMAS",
    "SENSITIVE",
    "SPECIFIC",
    "SQL",
    "SQLEXCEPTION",
    "SQLSTATE",
    "SQLWARNING",
    "TRIGGER",
    "UNDO",
    "UPGRADE",
    "WHILE",
];
/// New in 5.1.
const V5_1: &[&str] = &["ACCESSIBLE", "LINEAR", "MASTER_SSL_VERIFY_SERVER_CERT", "RANGE", "READ_ONLY", "READ_WRITE"];
/// New in 5.5.
const V5_5: &[&str] = &["GENERAL", "IGNORE_SERVER_IDS", "MASTER_HEARTBEAT_PERIOD", "MAXVALUE", "RESIGNAL", "SIGNAL", "SLOW"];
/// New in 5.6.
const V5_6: &[&str] = &["GET", "IO_AFTER_GTIDS", "IO_BEFORE_GTIDS", "MASTER_BIND", "PARTITION", "SQL_AFTER_GTIDS", "SQL_BEFORE_GTIDS"];
/// New in 5.7.
const V5_7: &[&str] = &["GENERATED", "OPTIMIZER_COSTS", "STORED", "VIRTUAL"];
/// New in 8.0 (GA list).
const V8_0: &[&str] = &[
    "CUBE",
    "CUME_DIST",
    "DENSE_RANK",
    "EMPTY",
    "EXCEPT",
    "FIRST_VALUE",
    "FUNCTION",
    "GROUPING",
    "GROUPS",
    "JSON_TABLE",
    "LAG",
    "LAST_VALUE",
    "LATERAL",
    "LEAD",
    "NTH_VALUE",
    "NTILE",
    "OF",
    "OVER",
    "PERCENT_RANK",
    "RANK",
    "RECURSIVE",
    "ROW",
    "ROWS",
    "ROW_NUMBER",
    "SYSTEM",
    "WINDOW",
];
/// New in 8.0.17.
const V8_0_17: &[&str] = &["ARRAY", "MEMBER"];
/// New in 8.0.31.
const V8_0_31: &[&str] = &["INTERSECT"];
/// New in 8.4.
const V8_4: &[&str] = &["MANUAL", "PARALLEL", "QUALIFY", "TABLESAMPLE"];
/// Removed in 8.0 (reserved before).
const REMOVED_8_0: &[&str] = &["ANALYSE", "DES_KEY_FILE", "PARSE_GCOL_EXPR", "REDOFILE", "SQL_CACHE"];
/// Reserved from 5.0 to 5.7 only (see `REMOVED_8_0`); `ANALYSE` came with 5.0's PROCEDURE ANALYSE.
const V5_0_TO_5_7: &[&str] = &["ANALYSE", "DES_KEY_FILE", "REDOFILE", "SQL_CACHE"];

/// Whether `word` is a reserved word on MySQL `(major, minor, patch)`.
pub fn is_reserved(word: &str, major: u16, minor: u16, patch: u16) -> bool {
    let w = word.to_ascii_uppercase();
    let at = |a: u16, b: u16, c: u16| (major, minor, patch) >= (a, b, c);
    let has = |list: &[&str]| list.iter().any(|x| *x == w);
    if has(BASE) {
        return true;
    }
    if at(5, 0, 0) && has(V5_0) {
        return true;
    }
    if at(5, 0, 0) && !at(8, 0, 0) && has(V5_0_TO_5_7) {
        return true;
    }
    if at(8, 0, 0) && has(REMOVED_8_0) {
        return false;
    }
    (at(5, 1, 0) && has(V5_1))
        || (at(5, 5, 0) && has(V5_5))
        || (at(5, 6, 0) && has(V5_6))
        || (at(5, 7, 0) && has(V5_7))
        || (at(8, 0, 0) && has(V8_0))
        || (at(8, 0, 17) && has(V8_0_17))
        || (at(8, 0, 31) && has(V8_0_31))
        || (at(8, 4, 0) && has(V8_4))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reserved_by_version() {
        assert!(is_reserved("select", 4, 0, 27));
        assert!(!is_reserved("condition", 4, 0, 27));
        assert!(is_reserved("CONDITION", 5, 0, 96));
        assert!(!is_reserved("range", 5, 0, 96) && is_reserved("range", 5, 1, 73));
        assert!(!is_reserved("rank", 5, 7, 44) && is_reserved("rank", 8, 0, 36));
        assert!(!is_reserved("member", 8, 0, 16) && is_reserved("member", 8, 0, 17));
        assert!(!is_reserved("intersect", 8, 0, 30) && is_reserved("intersect", 8, 0, 31));
        assert!(!is_reserved("qualify", 8, 0, 36) && is_reserved("qualify", 8, 4, 0) && is_reserved("qualify", 9, 1, 0));
        assert!(is_reserved("sql_cache", 5, 7, 44) && !is_reserved("sql_cache", 8, 0, 0));
        assert!(!is_reserved("orders", 9, 0, 0));
    }
}
