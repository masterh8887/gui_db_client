//! Source catalog reading per family and version (section 3): the [`SourceMeta`] seam, the
//! family collectors (`pg`, `mysql`, `sqlite`: source catalog, target state, drafts), the
//! charset sampler and the finalize value readers. Owner: copy-schema.

pub(crate) mod mysql;
pub(crate) mod pg;
pub(crate) mod sqlite;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use crate::connect::LiveConnection;
use crate::copy::translate::{bq, dq, my_str, sqlite_name_literal, TARGET_VALUE};
use crate::copy::types::*;
use crate::error::{CoreError, Result};

/// Read-only catalog queries on the source (plan / meta session). Implementations refuse text
/// with more than one statement.
#[async_trait]
pub trait SourceMeta: Send + Sync {
    async fn query(&self, sql: &str) -> Result<MetaTable>;

    /// The first column of every row as the raw bytes the server sent (the charset sampler of
    /// 3.2; the values never leave the planner). The default answers the text's UTF-8 bytes.
    async fn query_raw(&self, sql: &str) -> Result<Vec<Option<Vec<u8>>>> {
        let t = self.query(sql).await?;
        Ok(t.rows.into_iter().map(|r| r.into_iter().next().flatten().map(String::into_bytes)).collect())
    }

    /// PostgreSQL SQL_ASCII sources (3.5): from now on read catalog values as the bytes the
    /// server stores and decode them from `enc` into UTF-8 (`None`, while `PG_SQL_ASCII_SOURCE`
    /// blocks the plan: lossy UTF-8). Invalid bytes become U+FFFD. Other readers ignore it.
    fn decode_sql_ascii(&self, _enc: Option<PgSourceEncoding>) {}
}

/// `COPY (<query>) TO STDOUT (FORMAT csv, HEADER true, ENCODING 'SQL_ASCII')` for a query
/// (`SELECT`, `WITH`, `VALUES`, `TABLE`), `None` for other statements (`SET`, `SHOW`). With
/// `ENCODING 'SQL_ASCII'` the server sends the stored bytes without converting or validating
/// them, whatever the session's `client_encoding`.
#[cfg_attr(not(feature = "postgres"), allow(dead_code))]
pub(crate) fn copy_raw_sql(sql: &str) -> Option<String> {
    let first = crate::copy::guard::leading_words(sql, 1);
    if !matches!(first.first().map(String::as_str), Some("SELECT" | "WITH" | "VALUES" | "TABLE")) {
        return None;
    }
    let body = sql.trim().trim_end_matches(';').trim_end();
    // The newline ends a trailing `--` comment before the closing parenthesis.
    Some(format!("COPY ({body}\n) TO STDOUT (FORMAT csv, HEADER true, ENCODING 'SQL_ASCII')"))
}

/// Text of raw catalog bytes: decoded from the chosen SQL_ASCII source encoding (the mapping of
/// the data path's transcoder), lossy UTF-8 without one.
#[cfg_attr(not(feature = "postgres"), allow(dead_code))]
fn decode_catalog_text(b: &[u8], enc: Option<PgSourceEncoding>) -> String {
    let codec = match enc {
        None => return String::from_utf8_lossy(b).into_owned(),
        Some(PgSourceEncoding::EucKr) => encoding_rs::EUC_KR,
        Some(PgSourceEncoding::Latin1) => encoding_rs::WINDOWS_1252,
        Some(PgSourceEncoding::Utf8) => encoding_rs::UTF_8,
    };
    codec.decode_without_bom_handling(b).0.into_owned()
}

/// The [`MetaTable`] of PG CSV output with a header line (`copy_raw_sql`): `"` quoting with
/// `""` escapes, records ending at LF (or CRLF) outside quotes, an unquoted empty field is
/// NULL. CSV's special bytes are ASCII and never trail bytes of EUC-KR / CP949, so the
/// records split correctly before decoding.
#[cfg_attr(not(feature = "postgres"), allow(dead_code))]
pub(crate) fn decode_raw_csv(buf: &[u8], enc: Option<PgSourceEncoding>) -> MetaTable {
    fn finish(field: &mut Vec<u8>, quoted: &mut bool) -> Option<Vec<u8>> {
        let v = std::mem::take(field);
        let q = std::mem::replace(quoted, false);
        (q || !v.is_empty()).then_some(v)
    }
    let mut records: Vec<Vec<Option<Vec<u8>>>> = Vec::new();
    let mut rec: Vec<Option<Vec<u8>>> = Vec::new();
    let mut field: Vec<u8> = Vec::new();
    let (mut quoted, mut in_quotes) = (false, false);
    let mut i = 0;
    while i < buf.len() {
        let c = buf[i];
        i += 1;
        if in_quotes {
            if c != b'"' {
                field.push(c);
            } else if buf.get(i) == Some(&b'"') {
                field.push(b'"');
                i += 1;
            } else {
                in_quotes = false;
            }
            continue;
        }
        match c {
            b'"' => {
                in_quotes = true;
                quoted = true;
            }
            b',' => rec.push(finish(&mut field, &mut quoted)),
            b'\n' => {
                rec.push(finish(&mut field, &mut quoted));
                records.push(std::mem::take(&mut rec));
            }
            b'\r' if buf.get(i) == Some(&b'\n') => {}
            _ => field.push(c),
        }
    }
    if quoted || !field.is_empty() || !rec.is_empty() {
        rec.push(finish(&mut field, &mut quoted));
        records.push(rec);
    }
    let mut it = records.into_iter();
    let columns: Vec<String> = it.next().unwrap_or_default().into_iter().map(|c| String::from_utf8_lossy(&c.unwrap_or_default()).into_owned()).collect();
    let rows = it.map(|r| r.into_iter().map(|v| v.map(|b| decode_catalog_text(&b, enc))).collect()).collect();
    MetaTable { columns, rows }
}

/// Refuse anything but one statement (4.1: every meta query passes this check).
pub(crate) fn single_statement(sql: &str) -> Result<()> {
    if crate::sqlguard::split_statements(sql).len() == 1 {
        Ok(())
    } else {
        Err(CoreError::Internal("copy: a catalog query must be exactly one statement".into()))
    }
}

/// The [`SourceMeta`] of a live source connection: PG `PgDatabase.client` with
/// `simple_query`; MySQL modern or legacy through `source::mysql_raw(db)?.meta_query`
/// (decoded with `facts().decode_meta`); SQLite its own read-only rusqlite connection.
pub fn source_meta(live: &LiveConnection) -> Result<Box<dyn SourceMeta + '_>> {
    let db = live.db();
    #[cfg(feature = "postgres")]
    if let Some(pg) = db.as_any().downcast_ref::<crate::source::postgres::PgDatabase>() {
        return Ok(Box::new(PgMeta { db: pg, sql_ascii: Mutex::new(None) }));
    }
    #[cfg(feature = "mysql")]
    if let Some(raw) = crate::source::mysql_raw(db) {
        return Ok(Box::new(MyMeta { raw }));
    }
    if let Some(s) = db.as_any().downcast_ref::<crate::source::sqlite::SqliteDatabase>() {
        return Ok(Box::new(SqliteMeta::new(s.path())));
    }
    Err(CoreError::Invalid(format!("copy: driver '{}' has no catalog reader", live.conn.driver)))
}

#[cfg(feature = "postgres")]
struct PgMeta<'a> {
    db: &'a crate::source::postgres::PgDatabase,
    /// `Some(encoding)` after [`SourceMeta::decode_sql_ascii`]: queries go through
    /// [`copy_raw_sql`] and their bytes are decoded here.
    sql_ascii: Mutex<Option<Option<PgSourceEncoding>>>,
}

#[cfg(feature = "postgres")]
#[async_trait]
impl SourceMeta for PgMeta<'_> {
    async fn query(&self, sql: &str) -> Result<MetaTable> {
        use crate::source::postgres::map_pg_error;
        use futures::StreamExt;
        use tokio_postgres::SimpleQueryMessage;
        single_statement(sql)?;
        let decode = *self.sql_ascii.lock().unwrap_or_else(|p| p.into_inner());
        let _guard = self.db.exclusive().await;
        if let (Some(enc), Some(copy)) = (decode, copy_raw_sql(sql)) {
            // SQL_ASCII: the raw bytes (catalog results are small; this is no data path).
            let stream = self.db.client.copy_out(copy.as_str()).await.map_err(map_pg_error)?;
            let mut stream = std::pin::pin!(stream);
            let mut buf: Vec<u8> = Vec::new();
            while let Some(chunk) = stream.next().await {
                buf.extend_from_slice(&chunk.map_err(map_pg_error)?);
            }
            return Ok(decode_raw_csv(&buf, enc));
        }
        let msgs = self.db.client.simple_query(sql).await.map_err(map_pg_error)?;
        let mut t = MetaTable::default();
        for m in msgs {
            match m {
                SimpleQueryMessage::RowDescription(cols) => {
                    if t.columns.is_empty() {
                        t.columns = cols.iter().map(|c| c.name().to_string()).collect();
                    }
                }
                SimpleQueryMessage::Row(r) => {
                    if t.columns.is_empty() {
                        t.columns = r.columns().iter().map(|c| c.name().to_string()).collect();
                    }
                    // Never `get` (it panics on text that is not UTF-8).
                    let row = (0..r.len()).map(|i| r.try_get(i).map(|v| v.map(str::to_string))).collect::<std::result::Result<Vec<_>, _>>().map_err(|e| {
                        CoreError::Query { code: None, msg: format!("copy: catalog text is not valid UTF-8 (a SQL_ASCII source needs pg.source_encoding): {e}") }
                    })?;
                    t.rows.push(row);
                }
                _ => {}
            }
        }
        Ok(t)
    }

    fn decode_sql_ascii(&self, enc: Option<PgSourceEncoding>) {
        *self.sql_ascii.lock().unwrap_or_else(|p| p.into_inner()) = Some(enc);
    }
}

#[cfg(feature = "mysql")]
struct MyMeta<'a> {
    raw: &'a dyn crate::source::mysql_common::MysqlRawQuery,
}

#[cfg(feature = "mysql")]
#[async_trait]
impl SourceMeta for MyMeta<'_> {
    async fn query(&self, sql: &str) -> Result<MetaTable> {
        single_statement(sql)?;
        let facts = self.raw.facts();
        facts.check_sql_text(sql)?;
        let rows = self.raw.meta_query(sql).await?;
        let width = rows.columns.len();
        Ok(MetaTable {
            columns: rows.columns.iter().map(|c| c.name.clone()).collect(),
            rows: (0..rows.len()).map(|r| (0..width).map(|c| rows.text(facts, r, c)).collect()).collect(),
        })
    }

    async fn query_raw(&self, sql: &str) -> Result<Vec<Option<Vec<u8>>>> {
        single_statement(sql)?;
        self.raw.facts().check_sql_text(sql)?;
        let rows = self.raw.meta_query(sql).await?;
        Ok(rows.rows.into_iter().map(|r| r.into_iter().next().flatten()).collect())
    }
}

fn qerr(e: rusqlite::Error) -> CoreError {
    CoreError::Query { code: None, msg: e.to_string() }
}

/// SQLite catalog reader: its own read-only connection on the source file.
struct SqliteMeta {
    path: String,
    conn: Arc<Mutex<Option<rusqlite::Connection>>>,
}

impl SqliteMeta {
    fn new(path: &str) -> SqliteMeta {
        SqliteMeta { path: path.to_string(), conn: Arc::new(Mutex::new(None)) }
    }

    async fn run(&self, sql: &str) -> Result<(Vec<String>, Vec<Vec<Option<Vec<u8>>>>, Vec<Vec<Option<String>>>)> {
        single_statement(sql)?;
        let path = self.path.clone();
        let conn = self.conn.clone();
        let sql = sql.to_string();
        tokio::task::spawn_blocking(move || -> Result<_> {
            let mut guard = conn.lock().unwrap_or_else(|p| p.into_inner());
            if guard.is_none() {
                use rusqlite::OpenFlags;
                let c = rusqlite::Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX | OpenFlags::SQLITE_OPEN_URI)
                    .map_err(|e| CoreError::Connect(format!("cannot open sqlite database {path}: {e}")))?;
                let _ = c.busy_timeout(std::time::Duration::from_secs(5));
                *guard = Some(c);
            }
            let c = guard.as_ref().expect("opened above");
            let mut st = c.prepare(&sql).map_err(qerr)?;
            let cols: Vec<String> = st.column_names().iter().map(|s| s.to_string()).collect();
            let width = cols.len();
            let mut raw = Vec::new();
            let mut text = Vec::new();
            let mut rows = st.query([]).map_err(qerr)?;
            while let Some(r) = rows.next().map_err(qerr)? {
                let mut rr = Vec::with_capacity(width);
                let mut tt = Vec::with_capacity(width);
                for i in 0..width {
                    use rusqlite::types::ValueRef;
                    let v = r.get_ref(i).map_err(qerr)?;
                    let (b, t) = match v {
                        ValueRef::Null => (None, None),
                        ValueRef::Integer(n) => (Some(n.to_string().into_bytes()), Some(n.to_string())),
                        ValueRef::Real(f) => (Some(f.to_string().into_bytes()), Some(f.to_string())),
                        ValueRef::Text(s) => (Some(s.to_vec()), Some(String::from_utf8_lossy(s).into_owned())),
                        ValueRef::Blob(s) => (Some(s.to_vec()), Some(s.iter().map(|b| format!("{b:02x}")).collect())),
                    };
                    rr.push(b);
                    tt.push(t);
                }
                raw.push(rr);
                text.push(tt);
            }
            Ok((cols, raw, text))
        })
        .await
        .map_err(|e| CoreError::Internal(format!("copy: sqlite catalog task failed: {e}")))?
    }
}

#[async_trait]
impl SourceMeta for SqliteMeta {
    async fn query(&self, sql: &str) -> Result<MetaTable> {
        let (columns, _, rows) = self.run(sql).await?;
        Ok(MetaTable { columns, rows })
    }

    async fn query_raw(&self, sql: &str) -> Result<Vec<Option<Vec<u8>>>> {
        let (_, raw, _) = self.run(sql).await?;
        Ok(raw.into_iter().map(|r| r.into_iter().next().flatten()).collect())
    }
}

// ---------------------------------------------------------------------------------------------
// Finalize values (frozen API)
// ---------------------------------------------------------------------------------------------

fn parse_i128(v: Option<&str>) -> Option<i128> {
    let s = v?.trim();
    s.parse::<i128>().ok().or_else(|| s.parse::<f64>().ok().map(|f| f as i128))
}

/// `LIKE` pattern of an exact name (`\`, `_`, `%` escaped), as a MySQL string literal.
pub(crate) fn mysql_like_exact(name: &str) -> String {
    let mut p = String::with_capacity(name.len());
    for c in name.chars() {
        if matches!(c, '\\' | '_' | '%') {
            p.push('\\');
        }
        p.push(c);
    }
    my_str(&p)
}

/// The template of a finalize item (its first statement).
fn template(item: &PlanItem) -> &str {
    item.target_sql.first().map(String::as_str).unwrap_or("")
}

/// The regclass literal (`'"s"."q"'`) of a PG setval template.
fn setval_literal(t: &str) -> Option<&str> {
    t.strip_prefix("SELECT pg_catalog.setval(").and_then(|r| r.split(", ").next())
}

/// Unquote a PG string literal (`'…'`, `''` -> `'`).
fn unquote_pg(lit: &str) -> String {
    lit.trim().trim_start_matches('E').trim_matches('\'').replace("''", "'")
}

/// Runtime values of the `SequenceValue` / `AutoIncrement` items at finalize (frozen):
/// item id -> (value, is_called). Items without a value (no AUTO_INCREMENT, no
/// `sqlite_sequence` row) are absent.
pub async fn read_finalize_values(src: &dyn SourceMeta, items: &[&PlanItem]) -> Result<BTreeMap<String, (i128, bool)>> {
    let mut out = BTreeMap::new();
    let mut stats_expiry_set = false;
    for item in items {
        let t = template(item);
        match item.kind {
            CopyObjectKind::SequenceValue if t.starts_with("SELECT pg_catalog.setval(") => {
                let schema = item.schema.clone().unwrap_or_else(|| "public".into());
                let m = src.query(&format!("SELECT last_value, is_called FROM {}.{}", dq(&schema), dq(&item.name))).await?;
                if let Some(v) = parse_i128(m.get(0, "last_value")) {
                    let called = crate::copy::plan::meta_bool(m.get(0, "is_called")).unwrap_or(true);
                    out.insert(item.id.clone(), (v, called));
                }
            }
            CopyObjectKind::AutoIncrement if t.starts_with("ALTER TABLE ") => {
                let db = item.schema.clone().unwrap_or_default();
                if !stats_expiry_set {
                    stats_expiry_set = true;
                    // 8.0+ caches TABLE STATUS for up to 24 h; older servers do not know the variable.
                    let _ = src.query("SET SESSION information_schema_stats_expiry = 0").await;
                }
                let m = src.query(&format!("SHOW TABLE STATUS FROM {} LIKE {}", bq(&db), mysql_like_exact(&item.name))).await?;
                for r in 0..m.len() {
                    if m.get(r, "Name") == Some(item.name.as_str()) {
                        if let Some(v) = parse_i128(m.get(r, "Auto_increment")) {
                            out.insert(item.id.clone(), (v, true));
                        }
                    }
                }
            }
            CopyObjectKind::AutoIncrement if t.contains("sqlite_sequence") => {
                let m = src.query(&format!("SELECT seq FROM sqlite_sequence WHERE name = {}", sqlite_name_literal(&item.name))).await?;
                if let Some(v) = parse_i128(m.rows.first().and_then(|r| r.first()).and_then(|v| v.as_deref())) {
                    out.insert(item.id.clone(), (v, true));
                }
            }
            _ => {}
        }
    }
    Ok(out)
}

/// The target's current floor for a `SequenceValue` / `AutoIncrement` item (frozen): PG
/// `GREATEST(last_value when is_called, max(owning column))` (LEAST / min for descending
/// sequences) for floor-rule items, `None` for sequences this run created; MySQL the table's
/// next AUTO_INCREMENT; SQLite the `sqlite_sequence` value (`None` without a row).
pub async fn read_target_floor(tgt: &dyn TargetSession, item: &PlanItem) -> Result<Option<i128>> {
    let t = template(item);
    if t.starts_with("SELECT pg_catalog.setval(") {
        if !t.contains(TARGET_VALUE) {
            return Ok(None);
        }
        let Some(lit) = setval_literal(t) else { return Ok(None) };
        let descending = t.contains("LEAST(");
        let seq = unquote_pg(lit);
        let mut floor: Option<i128> = None;
        let m = tgt.query_meta(&format!("SELECT last_value, is_called FROM {seq}")).await?;
        if crate::copy::plan::meta_bool(m.get(0, "is_called")) == Some(true) {
            floor = parse_i128(m.get(0, "last_value"));
        }
        let owner = tgt
            .query_meta(&format!(
                "SELECT n.nspname AS schema, c.relname AS tbl, a.attname AS col FROM pg_catalog.pg_depend d \
                 JOIN pg_catalog.pg_class c ON c.oid = d.refobjid JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
                 JOIN pg_catalog.pg_attribute a ON a.attrelid = d.refobjid AND a.attnum = d.refobjsubid \
                 WHERE d.classid = 'pg_catalog.pg_class'::regclass AND d.objid = {lit}::regclass AND d.deptype IN ('a', 'i')"
            ))
            .await?;
        if let (Some(s), Some(tb), Some(c)) = (owner.get(0, "schema"), owner.get(0, "tbl"), owner.get(0, "col")) {
            let agg = if descending { "min" } else { "max" };
            let m = tgt.query_meta(&format!("SELECT {agg}({}) AS v FROM {}.{}", dq(c), dq(s), dq(tb))).await?;
            if let Some(v) = parse_i128(m.get(0, "v")) {
                floor = Some(match floor {
                    Some(f) if descending => f.min(v),
                    Some(f) => f.max(v),
                    None => v,
                });
            }
        }
        return Ok(floor);
    }
    if t.starts_with("ALTER TABLE ") {
        let db = item.target_schema.clone().unwrap_or_default();
        let m = tgt.query_meta(&format!("SHOW TABLE STATUS FROM {} LIKE {}", bq(&db), mysql_like_exact(&item.target_name))).await?;
        for r in 0..m.len() {
            if m.get(r, "Name") == Some(item.target_name.as_str()) {
                return Ok(parse_i128(m.get(r, "Auto_increment")));
            }
        }
        return Ok(None);
    }
    if t.contains("sqlite_sequence") {
        let m = tgt.query_meta(&format!("SELECT seq FROM sqlite_sequence WHERE name = {}", sqlite_name_literal(&item.target_name))).await?;
        return Ok(parse_i128(m.rows.first().and_then(|r| r.first()).and_then(|v| v.as_deref())));
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::copy::testkit::{CannedMeta, RecordingTarget};
    use crate::copy::translate::item_id;

    fn fin(kind: CopyObjectKind, schema: Option<&str>, name: &str, tschema: Option<&str>, tname: &str, tpl: &str) -> PlanItem {
        PlanItem {
            id: item_id(kind, &[name]),
            kind,
            parent: None,
            schema: schema.map(str::to_string),
            name: name.into(),
            target_schema: tschema.map(str::to_string),
            target_name: tname.into(),
            action: PlanAction::Run,
            phase: CopyPhase::Finalize,
            order: 0,
            exists_in_target: false,
            target_has_rows: None,
            target_est_rows: None,
            has_data: false,
            est_rows: None,
            est_bytes: None,
            source_ddl: None,
            target_sql: vec![tpl.into()],
            warnings: vec![],
            depends_on: vec![],
        }
    }

    #[test]
    fn sql_ascii_catalog_reads_raw_bytes() {
        // Queries go through COPY ... ENCODING 'SQL_ASCII'; other statements stay as they are.
        assert_eq!(
            copy_raw_sql("/* bulkdb:pg:relations */ SELECT c.relname AS name FROM pg_class c -- tail\n;").as_deref(),
            Some("COPY (/* bulkdb:pg:relations */ SELECT c.relname AS name FROM pg_class c -- tail\n) TO STDOUT (FORMAT csv, HEADER true, ENCODING 'SQL_ASCII')")
        );
        assert!(copy_raw_sql("WITH x AS (SELECT 1) SELECT * FROM x").is_some());
        assert_eq!(copy_raw_sql("SET search_path = \"app\", pg_catalog"), None);
        assert_eq!(copy_raw_sql("SHOW server_encoding"), None);
        // EUC-KR bytes of 주문 (orders) and 한글, a quoted comma / quote / newline, NULL vs "".
        let mut csv: Vec<u8> = b"name,comment,def\r\n".to_vec();
        csv.extend_from_slice(b"orders,\"");
        csv.extend_from_slice(&[0xC1, 0xD6, 0xB9, 0xAE]);
        csv.extend_from_slice(b", \"\"x\"\"\nnext\",\n\"\",");
        csv.extend_from_slice(&[0xC7, 0xD1, 0xB1, 0xDB]);
        csv.extend_from_slice(b",\"\"\n");
        let t = decode_raw_csv(&csv, Some(PgSourceEncoding::EucKr));
        assert_eq!(t.columns, vec!["name", "comment", "def"]);
        assert_eq!(t.rows.len(), 2, "{:?}", t.rows);
        assert_eq!(t.rows[0], vec![Some("orders".to_string()), Some("주문, \"x\"\nnext".to_string()), None]);
        assert_eq!(t.rows[1], vec![Some(String::new()), Some("한글".to_string()), Some(String::new())]);
        // Without a chosen encoding: lossy UTF-8, never a panic; an empty result keeps its columns.
        let t = decode_raw_csv(&csv, None);
        assert!(t.rows[0][1].as_deref().unwrap().contains('\u{FFFD}'));
        let t = decode_raw_csv(b"a,b\n", None);
        assert_eq!((t.columns.len(), t.rows.len()), (2, 0));
        // A single NULL column: an empty line.
        assert_eq!(decode_raw_csv(b"a\n\n", None).rows, vec![vec![None]]);
    }

    #[test]
    fn like_patterns_escape_wildcards() {
        assert_eq!(mysql_like_exact("orders"), "'orders'");
        assert_eq!(mysql_like_exact("a_b%c"), "'a\\\\_b\\\\%c'");
        assert!(single_statement("SELECT 1; SELECT 2").is_err());
        assert!(single_statement("SHOW TABLE STATUS FROM `db`").is_ok());
    }

    #[tokio::test]
    async fn finalize_values_per_family() {
        let src = CannedMeta::new()
            .answer("FROM \"app\".\"orders_id_seq\"", MetaTable::from_rows(&["last_value", "is_called"], &[&[Some("42"), Some("t")]]))
            .answer("SHOW TABLE STATUS FROM `shop` LIKE 'orders'", MetaTable::from_rows(&["Name", "Auto_increment"], &[&[Some("orders"), Some("1001")]]))
            .answer("FROM sqlite_sequence", MetaTable::from_rows(&["seq"], &[&[Some("7")]]));
        let pg = fin(
            CopyObjectKind::SequenceValue,
            Some("app"),
            "orders_id_seq",
            Some("app"),
            "orders_id_seq",
            "SELECT pg_catalog.setval('\"app\".\"orders_id_seq\"', <source value>, <is_called>)",
        );
        let my = fin(CopyObjectKind::AutoIncrement, Some("shop"), "orders", Some("shop2"), "orders", "ALTER TABLE `shop2`.`orders` AUTO_INCREMENT = <source value>");
        let lite = fin(CopyObjectKind::AutoIncrement, None, "t", None, "t", "UPDATE sqlite_sequence SET seq = max(seq, <source value>) WHERE name = 't'");
        let v = read_finalize_values(&src, &[&pg, &my, &lite]).await.unwrap();
        assert_eq!(v[&pg.id], (42, true));
        assert_eq!(v[&my.id], (1001, true));
        assert_eq!(v[&lite.id], (7, true));
        assert!(src.queries().iter().any(|q| q == "SHOW TABLE STATUS FROM `shop` LIKE 'orders'"));
    }

    #[tokio::test]
    async fn target_floor_reads_sequence_and_owning_column() {
        let tgt = RecordingTarget::new(TargetFacts::new(CopyFamily::Postgres, "postgres", "16.2", "app"))
            .answer("SELECT last_value, is_called FROM \"app\".\"q\"", MetaTable::from_rows(&["last_value", "is_called"], &[&[Some("10"), Some("t")]]))
            .answer("pg_depend", MetaTable::from_rows(&["schema", "tbl", "col"], &[&[Some("app"), Some("t"), Some("id")]]))
            .answer("SELECT max(\"id\")", MetaTable::from_rows(&["v"], &[&[Some("500")]]));
        let floor = fin(
            CopyObjectKind::SequenceValue,
            Some("app"),
            "q",
            Some("app"),
            "q",
            "SELECT pg_catalog.setval('\"app\".\"q\"', GREATEST(<source value>, <target value>), true)",
        );
        assert_eq!(read_target_floor(&tgt, &floor).await.unwrap(), Some(500));
        let created = fin(CopyObjectKind::SequenceValue, Some("app"), "q", Some("app"), "q", "SELECT pg_catalog.setval('\"app\".\"q\"', <source value>, <is_called>)");
        assert_eq!(read_target_floor(&tgt, &created).await.unwrap(), None);
        let my = RecordingTarget::new(TargetFacts::new(CopyFamily::Mysql, "mysql", "8.0.36", "shop2"))
            .answer("SHOW TABLE STATUS FROM `shop2` LIKE 'orders'", MetaTable::from_rows(&["Name", "Auto_increment"], &[&[Some("orders"), Some("90")]]));
        let item = fin(CopyObjectKind::AutoIncrement, Some("shop"), "orders", Some("shop2"), "orders", "ALTER TABLE `shop2`.`orders` AUTO_INCREMENT = <source value>");
        assert_eq!(read_target_floor(&my, &item).await.unwrap(), Some(90));
        let lite = RecordingTarget::new(TargetFacts::new(CopyFamily::Sqlite, "sqlite", "3.45.0", "main"));
        let item = fin(CopyObjectKind::AutoIncrement, None, "t", None, "t", "UPDATE sqlite_sequence SET seq = max(seq, <source value>) WHERE name = 't'");
        assert_eq!(read_target_floor(&lite, &item).await.unwrap(), None);
    }
}
