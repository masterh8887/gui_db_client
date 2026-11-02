//! Public test doubles of the copy seams (section 2 "Traits and seams"). The API listed as
//! frozen below may only be extended (owner copy-data):
//!
//! * `RecordingTarget::new(facts)`, `.answer(sql_substring, MetaTable)`, `.statements() ->
//!   Vec<(session_no, String)>`, `.payloads() -> Vec<(item_id, Bytes)>`.
//! * `CannedMeta::new()`, `.answer(sql_substring, MetaTable)`.
//!
//! `RecordingTarget` is a [`TargetSession`] (session 0) and a [`TargetOpener`] whose every
//! `open` returns another session (1, 2, ...) recording into the same log, so it can be handed
//! to `BulkDbOptions.copy_target_opener`. Unmatched `query_meta` calls answer an empty
//! [`MetaTable`]; `count_rows` returns the rows it recorded for that table. Arrow input is
//! encoded through the same pure `copy::data::mysql_encode` as `MysqlTarget` (MySQL facts only),
//! so recorded payloads are what a server would receive. Checksum verification is not
//! supported on it (`verify: checksum` -> `INVALID`, enforced by the executor).

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use bytes::{Bytes, BytesMut};
use tokio_util::sync::CancellationToken;

use super::data::mysql_encode;
use super::schema::SourceMeta;
use super::types::*;
use crate::error::{CoreError, Result};
use crate::types::Connection;

/// First answer (in registration order) whose substring occurs in `sql`; else an empty table.
fn lookup(answers: &[(String, MetaTable)], sql: &str) -> MetaTable {
    answers.iter().find(|(needle, _)| sql.contains(needle.as_str())).map(|(_, t)| t.clone()).unwrap_or_default()
}

struct Recorder {
    facts: TargetFacts,
    answers: Mutex<Vec<(String, MetaTable)>>,
    statements: Mutex<Vec<(u32, String)>>,
    queries: Mutex<Vec<(u32, String)>>,
    payloads: Mutex<Vec<(String, Bytes)>>,
    rows: Mutex<BTreeMap<TargetTable, u64>>,
    next_session: AtomicU32,
}

/// Recording copy target (see the module docs).
#[derive(Clone)]
pub struct RecordingTarget {
    rec: Arc<Recorder>,
    session_no: u32,
    /// This session's writer settings (`configure`), so payloads follow the table charset.
    cfg: Arc<Mutex<Option<WriterConfig>>>,
}

impl RecordingTarget {
    pub fn new(facts: TargetFacts) -> RecordingTarget {
        RecordingTarget {
            rec: Arc::new(Recorder {
                facts,
                answers: Mutex::new(Vec::new()),
                statements: Mutex::new(Vec::new()),
                queries: Mutex::new(Vec::new()),
                payloads: Mutex::new(Vec::new()),
                rows: Mutex::new(BTreeMap::new()),
                next_session: AtomicU32::new(1),
            }),
            session_no: 0,
            cfg: Arc::new(Mutex::new(None)),
        }
    }

    /// Answer `query_meta` calls whose SQL contains `sql_substring` with `table`.
    pub fn answer(self, sql_substring: &str, table: MetaTable) -> RecordingTarget {
        self.rec.answers.lock().unwrap().push((sql_substring.to_string(), table));
        self
    }

    /// Every `execute` of every session, in order: (session number, statement).
    pub fn statements(&self) -> Vec<(u32, String)> {
        self.rec.statements.lock().unwrap().clone()
    }

    /// Every `query_meta` of every session, in order: (session number, SQL). (Extension.)
    pub fn queries(&self) -> Vec<(u32, String)> {
        self.rec.queries.lock().unwrap().clone()
    }

    /// Every load payload: (item id, bytes). RawCsv input: one payload per load; Arrow input:
    /// one per encoded piece (a LOAD DATA payload or one INSERT statement).
    pub fn payloads(&self) -> Vec<(String, Bytes)> {
        self.rec.payloads.lock().unwrap().clone()
    }

    /// This session's number (0 for the target itself, 1.. for opened sessions). (Extension.)
    pub fn session_no(&self) -> u32 {
        self.session_no
    }

    fn add_rows(&self, t: &TargetTable, n: u64) {
        *self.rec.rows.lock().unwrap().entry(t.clone()).or_default() += n;
    }
}

#[async_trait]
impl TargetSession for RecordingTarget {
    fn facts(&self) -> &TargetFacts {
        &self.rec.facts
    }

    async fn query_meta(&self, sql: &str) -> Result<MetaTable> {
        self.rec.queries.lock().unwrap().push((self.session_no, sql.to_string()));
        Ok(lookup(&self.rec.answers.lock().unwrap(), sql))
    }

    async fn execute(&self, sql: &str) -> Result<u64> {
        self.rec.statements.lock().unwrap().push((self.session_no, sql.to_string()));
        Ok(0)
    }

    async fn load(&self, spec: &TableDataSpec, input: LoadInput, cancel: CancellationToken) -> Result<LoadOutcome> {
        let mut out = LoadOutcome::default();
        match input {
            LoadInput::RawCsv(mut rx) => {
                let mut buf = BytesMut::new();
                loop {
                    let next = tokio::select! {
                        biased;
                        _ = cancel.cancelled() => return Err(CoreError::Cancelled),
                        n = rx.recv() => n,
                    };
                    let Some(b) = next else { break };
                    buf.extend_from_slice(&b.item);
                    out.rows += b.rows;
                    drop(b.permit);
                }
                self.rec.payloads.lock().unwrap().push((spec.item_id.clone(), buf.freeze()));
                out.method = Some(match self.rec.facts.family {
                    CopyFamily::Postgres => LoadMethod::PgCopyCsv,
                    CopyFamily::Mysql => mysql_encode::method_for(&self.rec.facts),
                    CopyFamily::Sqlite => LoadMethod::SqliteInsert,
                });
            }
            LoadInput::Arrow(mut rx) => {
                if self.rec.facts.family != CopyFamily::Mysql {
                    return Err(CoreError::Invalid("RecordingTarget encodes Arrow input for MySQL targets only".into()));
                }
                loop {
                    let next = tokio::select! {
                        biased;
                        _ = cancel.cancelled() => return Err(CoreError::Cancelled),
                        n = rx.recv() => n,
                    };
                    let Some(b) = next else { break };
                    let charset = self.cfg.lock().unwrap().as_ref().and_then(|c| c.charset_for(&spec.item_id).map(str::to_string));
                    let enc = mysql_encode::encode_batch_with(spec, &b.item, &self.rec.facts, charset.as_deref())?;
                    drop(b.permit);
                    out.rows += enc.rows;
                    out.unmappable += enc.unmappable;
                    out.value_fixes.extend(enc.value_fixes);
                    let mut payloads = self.rec.payloads.lock().unwrap();
                    payloads.extend(enc.pieces.into_iter().map(|p| (spec.item_id.clone(), p)));
                }
                out.method = Some(mysql_encode::method_for(&self.rec.facts));
            }
        }
        self.add_rows(&spec.target, out.rows);
        Ok(out)
    }

    async fn count_rows(&self, t: &TargetTable) -> Result<u64> {
        Ok(self.rec.rows.lock().unwrap().get(t).copied().unwrap_or(0))
    }

    async fn cancel_running(&self) {}

    fn configure(&self, cfg: &WriterConfig) {
        *self.cfg.lock().unwrap() = Some(cfg.clone());
    }
}

#[async_trait]
impl TargetOpener for RecordingTarget {
    async fn open(&self, _conn: &Connection, _ep: &CopyEndpoint, _app_name: &str) -> Result<Box<dyn TargetSession>> {
        let no = self.rec.next_session.fetch_add(1, Ordering::Relaxed);
        Ok(Box::new(RecordingTarget { rec: self.rec.clone(), session_no: no, cfg: Arc::new(Mutex::new(None)) }))
    }
}

/// Canned source catalog answers for planner tests.
#[derive(Clone, Default)]
pub struct CannedMeta {
    answers: Arc<Mutex<Vec<(String, MetaTable)>>>,
    queries: Arc<Mutex<Vec<String>>>,
}

impl CannedMeta {
    pub fn new() -> CannedMeta {
        CannedMeta::default()
    }

    /// Answer queries whose SQL contains `sql_substring` with `table` (first match wins).
    pub fn answer(self, sql_substring: &str, table: MetaTable) -> CannedMeta {
        self.answers.lock().unwrap().push((sql_substring.to_string(), table));
        self
    }

    /// Every query asked so far, in order. (Extension.)
    pub fn queries(&self) -> Vec<String> {
        self.queries.lock().unwrap().clone()
    }
}

#[async_trait]
impl SourceMeta for CannedMeta {
    async fn query(&self, sql: &str) -> Result<MetaTable> {
        self.queries.lock().unwrap().push(sql.to_string());
        Ok(lookup(&self.answers.lock().unwrap(), sql))
    }
}

/// An empty plan of `family` (no items, no issues): a starting point for tests and for the
/// plan of a request that is refused before planning (extension).
pub fn empty_plan(family: CopyFamily) -> CopyPlan {
    CopyPlan {
        plan_id: String::new(),
        family,
        source: CopyServerFacts::default(),
        target: CopyServerFacts::default(),
        effective: CopyEffective {
            charset: None,
            collation: None,
            engine: None,
            zero_dates: None,
            time_zone: None,
            load_method: match family {
                CopyFamily::Postgres => LoadMethod::PgCopyCsv,
                CopyFamily::Mysql => LoadMethod::MysqlInsert,
                CopyFamily::Sqlite => LoadMethod::SqliteInsert,
            },
            table_concurrency: 1,
            chunk_bytes: None,
            consistent_snapshot: false,
            source_sessions: 1,
            target_sessions: 2,
        },
        items: Vec::new(),
        warnings: Vec::new(),
        blockers: Vec::new(),
        acks: Vec::new(),
        session_sql: SessionSql::default(),
        totals: CopyTotals::default(),
        resume_of: None,
        elapsed_ms: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tokio::sync::{mpsc, Semaphore};

    fn spec(item: &str) -> TableDataSpec {
        TableDataSpec {
            item_id: item.into(),
            source_sql: "SELECT 1".into(),
            verify_sql: String::new(),
            raw_csv: true,
            transcode: None,
            target: TargetTable { schema: Some("public".into()), name: "t".into() },
            columns: vec![],
            est_rows: None,
            first_timestamp_auto: false,
            fk_checks: false,
        }
    }

    #[tokio::test]
    async fn recording_target_records_sessions_answers_and_payloads() {
        let facts = TargetFacts::new(CopyFamily::Postgres, "postgres", "16.2", "app");
        assert_eq!((facts.major, facts.minor, facts.patch), (16, 2, 0));
        let t = RecordingTarget::new(facts).answer("pg_class", MetaTable::from_rows(&["relname"], &[&[Some("t")]]));
        let s1 = t.open(&Connection::default(), &CopyEndpoint::default(), "bulkdb-x-t1").await.unwrap();
        s1.execute("CREATE TABLE \"t\" (a int)").await.unwrap();
        let m = s1.query_meta("SELECT relname FROM pg_class").await.unwrap();
        assert_eq!(m.get(0, "RELNAME"), Some("t"));
        assert!(t.query_meta("SELECT 42").await.unwrap().is_empty());

        let budget = Arc::new(Semaphore::new(4));
        let (tx, rx) = mpsc::channel(1);
        let send = async move {
            for chunk in ["1,a\n", "2,b\n"] {
                let permit = budget.clone().acquire_many_owned(1).await.unwrap();
                tx.send(Budgeted { item: Bytes::from(chunk), rows: 1, permit }).await.unwrap();
            }
        };
        let sp = spec("table:\"public\".\"t\"");
        let (out, ()) = tokio::join!(s1.load(&sp, LoadInput::RawCsv(rx), CancellationToken::new()), send);
        let out = out.unwrap();
        assert_eq!(out.rows, 2);
        assert_eq!(out.method, Some(LoadMethod::PgCopyCsv));
        assert_eq!(t.statements(), vec![(1, "CREATE TABLE \"t\" (a int)".to_string())]);
        assert_eq!(t.payloads(), vec![(sp.item_id.clone(), Bytes::from("1,a\n2,b\n"))]);
        assert_eq!(t.count_rows(&sp.target).await.unwrap(), 2);
    }

    #[tokio::test]
    async fn canned_meta_answers_first_match() {
        let m = CannedMeta::new()
            .answer("SHOW TABLE STATUS", MetaTable::from_rows(&["Name", "Type"], &[&[Some("t"), Some("MyISAM")]]))
            .answer("SHOW", MetaTable::from_rows(&["x"], &[]));
        let r = m.query("SHOW TABLE STATUS FROM `db`").await.unwrap();
        assert_eq!(r.get(0, "type"), Some("MyISAM"));
        assert!(m.query("SHOW CREATE TABLE `db`.`t`").await.unwrap().is_empty());
        assert_eq!(m.queries().len(), 2);
    }
}
