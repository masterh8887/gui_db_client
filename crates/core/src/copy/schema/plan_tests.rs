//! Tests of `copy::plan` (kept under `schema/` with the other copy-schema files): SQLite planning
//! through `build_plan`, and the family independent assembly (actions for the four policies,
//! overrides, selection, exclude, content, resume mapping, drop dependants, truncate groups,
//! load order).

use super::*;
use crate::copy::testkit::{CannedMeta, RecordingTarget};
use crate::copy::translate::{dq, item_id};
use crate::types::TableRef;

fn row(v: &[Option<&str>]) -> Vec<Option<String>> {
    v.iter().map(|x| x.map(str::to_string)).collect()
}

fn table(cols: &[&str], rows: &[&[Option<&str>]]) -> MetaTable {
    MetaTable { columns: cols.iter().map(|c| c.to_string()).collect(), rows: rows.iter().map(|r| row(r)).collect() }
}

fn sqlite_source() -> CannedMeta {
    let master = table(
        &["type", "name", "tbl_name", "sql"],
        &[
            &[Some("table"), Some("t1"), Some("t1"), Some("CREATE TABLE t1 (id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT)")],
            &[Some("table"), Some("t2"), Some("t2"), Some("CREATE TABLE t2 (a INTEGER, b TEXT)")],
            &[Some("index"), Some("i1"), Some("t1"), Some("CREATE INDEX i1 ON t1(name)")],
            &[Some("index"), Some("sqlite_autoindex_t1_1"), Some("t1"), None],
            &[Some("view"), Some("v1"), Some("v1"), Some("CREATE VIEW v1 AS SELECT id FROM t1")],
            &[Some("view"), Some("v2"), Some("v2"), Some("CREATE VIEW v2 AS SELECT * FROM v1 JOIN t2")],
            &[Some("trigger"), Some("tr"), Some("t2"), Some("CREATE TRIGGER tr AFTER INSERT ON t2 BEGIN SELECT 1; END")],
            &[Some("table"), Some("fts"), Some("fts"), Some("CREATE VIRTUAL TABLE fts USING fts5(body)")],
            &[Some("table"), Some("fts_content"), Some("fts_content"), Some("CREATE TABLE 'fts_content'(id INTEGER PRIMARY KEY, c0)")],
        ],
    );
    let cols = ["name", "type", "notnull", "dflt_value", "pk", "hidden"];
    CannedMeta::new()
        .answer("sqlite_master", master)
        .answer(
            "pragma_table_xinfo('t1')",
            table(&cols, &[&[Some("id"), Some("INTEGER"), Some("0"), None, Some("1"), Some("0")], &[Some("name"), Some("TEXT"), Some("0"), None, Some("0"), Some("0")]]),
        )
        .answer(
            "pragma_table_xinfo('t2')",
            table(&cols, &[&[Some("a"), Some("INTEGER"), Some("0"), None, Some("0"), Some("0")], &[Some("b"), Some("TEXT"), Some("0"), None, Some("0"), Some("0")]]),
        )
}

/// A SQLite target where `t2` exists (with rows unless `empty`) and has the given columns.
fn sqlite_target(empty: bool, t2_cols: &[(&str, &str, bool, bool)]) -> RecordingTarget {
    let master = table(
        &["type", "name", "tbl_name", "sql"],
        &[&[Some("table"), Some("t2"), Some("t2"), Some("CREATE TABLE t2 (a INTEGER, b TEXT)")], &[Some("trigger"), Some("tr"), Some("t2"), Some("CREATE TRIGGER tr …")]],
    );
    let cols: Vec<Vec<Option<String>>> = t2_cols
        .iter()
        .map(|(n, t, nn, def)| row(&[Some(n), Some(t), Some(if *nn { "1" } else { "0" }), if *def { Some("0") } else { None }, Some("0"), Some("0")]))
        .collect();
    let mut t = RecordingTarget::new(TargetFacts::new(CopyFamily::Sqlite, "sqlite", "3.45.0", "main")).answer("sqlite_master", master).answer(
        "pragma_table_xinfo('t2')",
        MetaTable { columns: ["name", "type", "notnull", "dflt_value", "pk", "hidden"].iter().map(|s| s.to_string()).collect(), rows: cols },
    );
    if !empty {
        t = t.answer("SELECT 1 FROM \"t2\" LIMIT 1", table(&["1"], &[&[Some("1")]]));
    }
    t
}

fn req(family_db: &str) -> CopyRequest {
    CopyRequest {
        source: CopyEndpoint { conn_id: "src".into(), database: family_db.into(), schema: None, password: None },
        target: CopyEndpoint { conn_id: "tgt".into(), database: family_db.into(), schema: None, password: None },
        selection: CopySelection::All,
        options: CopyOptions::default(),
        name: None,
    }
}

async fn plan_with(req: &CopyRequest, src: &dyn SourceMeta, tgt: &RecordingTarget, driver: &str, version: &str, resume: Option<&ResumeState>) -> Result<PlannedCopy> {
    let sconn = Connection { driver: driver.into(), ..Default::default() };
    let tconn = Connection { driver: driver.into(), ..Default::default() };
    let server = ServerInfo { driver: driver.into(), version: version.into(), ..Default::default() };
    let cancel = CancellationToken::new();
    build_plan(PlanInput { req, source: src, source_conn: &sconn, source_server: &server, mysql_source: None, target: tgt, target_conn: &tconn, resume, cancel: &cancel })
        .await
}

fn item<'a>(p: &'a PlannedCopy, id: &str) -> &'a PlanItem {
    p.plan.items.iter().find(|i| i.id == id).unwrap_or_else(|| panic!("no item {id}: {:?}", p.plan.items.iter().map(|i| &i.id).collect::<Vec<_>>()))
}

fn codes(issues: &[CopyIssue]) -> Vec<&str> {
    issues.iter().map(|i| i.code.as_str()).collect()
}

#[tokio::test]
async fn sqlite_default_policy_skips_existing_tables() {
    let src = sqlite_source();
    let tgt = sqlite_target(false, &[("a", "INTEGER", false, false), ("b", "TEXT", false, false)]);
    let p = plan_with(&req("main"), &src, &tgt, "sqlite", "3.45.0", None).await.unwrap();
    assert!(src.queries()[0].contains("NOT LIKE 'sqlite\\_%' ESCAPE '\\'"), "escaped LIKE: {}", src.queries()[0]);

    let t1 = item(&p, "table:\"t1\"");
    assert_eq!(t1.action, PlanAction::Create);
    assert_eq!(t1.phase, CopyPhase::PreData);
    assert!(t1.has_data && !t1.exists_in_target);
    assert_eq!(
        t1.target_sql,
        vec!["CREATE TABLE t1 (id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT)".to_string(), "INSERT INTO \"t1\" (\"id\", \"name\") VALUES <rows>".to_string()]
    );
    let spec = &p.tables["table:\"t1\""];
    assert_eq!(spec.source_sql, "SELECT \"id\", \"name\" FROM \"t1\"");
    assert_eq!(spec.target, TargetTable { schema: None, name: "t1".into() });
    assert_eq!(spec.columns[0].value, ValueKind::Integer);

    // USER DECISION: an existing table is skipped by default (no DDL, no data), with the row probe.
    let t2 = item(&p, "table:\"t2\"");
    assert_eq!(t2.action, PlanAction::SkipExisting);
    assert!(t2.exists_in_target && !t2.has_data && t2.target_sql.is_empty());
    assert_eq!(t2.target_has_rows, Some(true));
    assert!(!p.tables.contains_key("table:\"t2\""));
    assert!(codes(&p.plan.warnings).contains(&"TABLES_SKIPPED_EXISTING"));
    assert_eq!(item(&p, "trigger:\"t2\".\"tr\"").action, PlanAction::SkipExisting);

    // Virtual tables and their shadow tables are unsupported, never copied twice.
    assert_eq!(item(&p, "table:\"fts\"").action, PlanAction::Unsupported);
    assert_eq!(item(&p, "table:\"fts_content\"").action, PlanAction::Unsupported);
    assert!(codes(&item(&p, "table:\"fts\"").warnings).contains(&"UNSUPPORTED_OBJECT"));

    let i1 = item(&p, "index:\"t1\".\"i1\"");
    assert_eq!((i1.action, i1.phase), (PlanAction::Create, CopyPhase::PostData));
    assert_eq!(i1.parent.as_deref(), Some("table:\"t1\""));
    assert!(!p.plan.items.iter().any(|i| i.name.starts_with("sqlite_autoindex")));

    // Views in dependency order after their tables.
    let (v1, v2) = (item(&p, "view:\"v1\""), item(&p, "view:\"v2\""));
    assert!(v1.order < v2.order && t1.order < v1.order, "{} {} {}", t1.order, v1.order, v2.order);
    assert!(v2.depends_on.contains(&"view:\"v1\"".to_string()) && v2.depends_on.contains(&"table:\"t2\"".to_string()));

    let ai = item(&p, "auto_increment:\"t1\"");
    assert_eq!((ai.action, ai.phase), (PlanAction::Run, CopyPhase::Finalize));
    assert_eq!(ai.target_sql, vec!["UPDATE sqlite_sequence SET seq = max(seq, <source value>) WHERE name = 't1'".to_string()]);
    assert_eq!(item(&p, "analyze:\"t1\"").target_sql, vec!["ANALYZE \"t1\"".to_string()]);
    assert_eq!(item(&p, "analyze:\"t2\"").action, PlanAction::SkipExisting);

    assert_eq!(p.plan.effective.load_method, LoadMethod::SqliteInsert);
    assert_eq!(p.plan.effective.table_concurrency, 1);
    assert_eq!(p.plan.totals.skip, 3, "t2, its trigger and analyze: {:?}", p.plan.totals);
    assert_eq!(p.plan.totals.tables, 4);
    assert!(p.plan.blockers.is_empty(), "{:?}", p.plan.blockers);
    // Items are sorted by phase, then order.
    let phases: Vec<(CopyPhase, u32)> = p.plan.items.iter().map(|i| (i.phase, i.order)).collect();
    let mut sorted = phases.clone();
    sorted.sort();
    assert_eq!(phases, sorted);
}

#[tokio::test]
async fn sqlite_truncate_drop_append_and_overrides() {
    let src = sqlite_source();
    let cols = [("a", "INTEGER", false, false), ("b", "TEXT", false, false)];
    let mut r = req("main");

    r.options.existing = ExistingTablePolicy::Truncate;
    let p = plan_with(&r, &src, &sqlite_target(false, &cols), "sqlite", "3.45.0", None).await.unwrap();
    let t2 = item(&p, "table:\"t2\"");
    assert_eq!((t2.action, t2.phase), (PlanAction::Truncate, CopyPhase::Data));
    assert_eq!(t2.target_sql, vec!["DELETE FROM \"t2\"".to_string(), "INSERT INTO \"t2\" (\"a\", \"b\") VALUES <rows>".to_string()]);
    assert!(codes(&t2.warnings).contains(&"DESTRUCTIVE_TRUNCATE") && codes(&t2.warnings).contains(&"TARGET_TRIGGERS"));
    assert!(p.tables.contains_key("table:\"t2\""));
    // Existing indexes kept, missing ones not created: children of a truncated table are skipped.
    assert_eq!(item(&p, "trigger:\"t2\".\"tr\"").action, PlanAction::SkipExisting);
    let parts = crate::copy::translate::item_sql_parts(t2);
    assert_eq!(parts.truncate.as_deref(), Some("DELETE FROM \"t2\""));
    assert!(parts.ddl.is_empty() && parts.load_template.is_some());

    r.options.existing = ExistingTablePolicy::DropRecreate;
    let p = plan_with(&r, &src, &sqlite_target(false, &cols), "sqlite", "3.45.0", None).await.unwrap();
    let t2 = item(&p, "table:\"t2\"");
    assert_eq!((t2.action, t2.phase), (PlanAction::DropRecreate, CopyPhase::PreData));
    assert_eq!(t2.target_sql[0], "DROP TABLE \"t2\"");
    assert_eq!(t2.target_sql[1], "CREATE TABLE t2 (a INTEGER, b TEXT)");
    assert!(codes(&t2.warnings).contains(&"DESTRUCTIVE_DROP"));
    assert_eq!(item(&p, "trigger:\"t2\".\"tr\"").action, PlanAction::Create);

    r.options.existing = ExistingTablePolicy::Append;
    let p = plan_with(&r, &src, &sqlite_target(true, &cols), "sqlite", "3.45.0", None).await.unwrap();
    let t2 = item(&p, "table:\"t2\"");
    assert_eq!(t2.action, PlanAction::Append);
    assert_eq!(t2.target_has_rows, Some(false));
    assert_eq!(t2.target_sql, vec!["INSERT INTO \"t2\" (\"a\", \"b\") VALUES <rows>".to_string()]);
    assert!(!codes(&t2.warnings).contains(&"DESTRUCTIVE_TRUNCATE"));

    // Per-item override beats the global policy.
    r.options.existing = ExistingTablePolicy::Skip;
    r.options.existing_overrides = vec![ItemPolicy { item_id: "table:\"t2\"".into(), policy: ExistingTablePolicy::Truncate }];
    let p = plan_with(&r, &src, &sqlite_target(true, &cols), "sqlite", "3.45.0", None).await.unwrap();
    assert_eq!(item(&p, "table:\"t2\"").action, PlanAction::Truncate);
    assert!(!codes(&item(&p, "table:\"t2\"").warnings).contains(&"DESTRUCTIVE_TRUNCATE"), "an empty table is not destroyed");
    assert!(!codes(&p.plan.warnings).contains(&"TABLES_SKIPPED_EXISTING"));
}

#[tokio::test]
async fn sqlite_append_mismatch_and_skipped_differs() {
    let src = sqlite_source();
    let mut r = req("main");
    r.options.existing = ExistingTablePolicy::Append;
    // Target t2 lacks `b` and has a NOT NULL column without default the source does not have.
    let p = plan_with(&r, &src, &sqlite_target(false, &[("a", "INTEGER", false, false), ("c", "TEXT", true, false)]), "sqlite", "3.45.0", None).await.unwrap();
    let b = p.plan.blockers.iter().find(|b| b.code == "APPEND_COLUMNS_MISMATCH").expect("item blocker");
    assert_eq!(b.item_id.as_deref(), Some("table:\"t2\""));
    assert!(b.detail.as_deref().unwrap().contains("b") && b.detail.as_deref().unwrap().contains("c"));

    r.options.existing = ExistingTablePolicy::Skip;
    let p = plan_with(&r, &src, &sqlite_target(false, &[("a", "INTEGER", false, false), ("zz", "TEXT", false, false)]), "sqlite", "3.45.0", None).await.unwrap();
    assert!(codes(&item(&p, "table:\"t2\"").warnings).contains(&"SKIPPED_DIFFERS"));
    assert!(p.plan.blockers.is_empty());
}

#[tokio::test]
async fn selection_exclude_and_content() {
    let src = sqlite_source();
    let cols = [("a", "INTEGER", false, false), ("b", "TEXT", false, false)];
    let mut r = req("main");
    r.selection = CopySelection::Tables { tables: vec![TableRef::new(None, "t1")] };
    let p = plan_with(&r, &src, &sqlite_target(false, &cols), "sqlite", "3.45.0", None).await.unwrap();
    assert!(!p.plan.items.iter().any(|i| i.id == "table:\"t2\""), "unselected tables leave the plan");
    let v2 = item(&p, "view:\"v2\"");
    assert_eq!(v2.action, PlanAction::Excluded);
    assert!(codes(&v2.warnings).contains(&"DEPENDENCY_NOT_SELECTED"));
    assert_eq!(item(&p, "view:\"v1\"").action, PlanAction::Create);
    assert_eq!(item(&p, "trigger:\"t2\".\"tr\"").action, PlanAction::Excluded);

    let mut r = req("main");
    r.options.exclude = vec!["table:\"t1\"".into()];
    let p = plan_with(&r, &src, &sqlite_target(false, &cols), "sqlite", "3.45.0", None).await.unwrap();
    for id in ["table:\"t1\"", "index:\"t1\".\"i1\"", "view:\"v1\"", "view:\"v2\"", "auto_increment:\"t1\""] {
        assert_eq!(item(&p, id).action, PlanAction::Excluded, "{id}");
        assert!(item(&p, id).target_sql.is_empty());
    }
    assert!(!p.tables.contains_key("table:\"t1\""));
    assert_eq!(p.plan.totals.excluded, 6, "t1, i1, v1, v2, auto_increment, analyze");

    let mut r = req("main");
    r.options.content = CopyContent::SchemaOnly;
    let p = plan_with(&r, &src, &sqlite_target(false, &cols), "sqlite", "3.45.0", None).await.unwrap();
    assert!(p.tables.is_empty());
    assert_eq!(item(&p, "table:\"t1\"").target_sql.len(), 1);
    assert!(!p.plan.items.iter().any(|i| matches!(i.kind, CopyObjectKind::AutoIncrement | CopyObjectKind::Analyze)));

    let mut r = req("main");
    r.options.content = CopyContent::DataOnly;
    let p = plan_with(&r, &src, &sqlite_target(false, &cols), "sqlite", "3.45.0", None).await.unwrap();
    assert!(p.plan.blockers.iter().any(|b| b.code == "TABLE_MISSING" && b.item_id.as_deref() == Some("table:\"t1\"")));
    assert!(!p.plan.items.iter().any(|i| matches!(i.kind, CopyObjectKind::View | CopyObjectKind::Index | CopyObjectKind::Trigger)));
    assert!(item(&p, "table:\"t1\"").target_sql.iter().all(|s| !s.starts_with("CREATE")));
    let mut r2 = r.clone();
    r2.options.exclude = vec!["table:\"t1\"".into()];
    let p = plan_with(&r2, &src, &sqlite_target(false, &cols), "sqlite", "3.45.0", None).await.unwrap();
    assert!(codes(&p.plan.warnings).contains(&"DATA_ONLY_ALL_SKIPPED"));

    let mut r = req("main");
    r.options.objects.sequences = false;
    let p = plan_with(&r, &src, &sqlite_target(false, &cols), "sqlite", "3.45.0", None).await.unwrap();
    assert!(codes(&p.plan.warnings).contains(&"SEQUENCES_NOT_SET"));
    assert!(!p.plan.items.iter().any(|i| i.kind == CopyObjectKind::AutoIncrement));

    let mut r = req("main");
    r.options.rename = vec![NameMapping { kind: RenameKind::Table, schema: None, from: "t1".into(), to: "x".into() }];
    assert!(matches!(plan_with(&r, &src, &sqlite_target(false, &cols), "sqlite", "3.45.0", None).await, Err(CoreError::Invalid(_))));
}

#[tokio::test]
async fn resume_mapping() {
    let src = sqlite_source();
    let cols = [("a", "INTEGER", false, false), ("b", "TEXT", false, false)];
    // t1 now exists in the target too (created by the run).
    let tgt = RecordingTarget::new(TargetFacts::new(CopyFamily::Sqlite, "sqlite", "3.45.0", "main"))
        .answer(
            "sqlite_master",
            table(
                &["type", "name", "tbl_name", "sql"],
                &[
                    &[Some("table"), Some("t1"), Some("t1"), Some("…")],
                    &[Some("table"), Some("t2"), Some("t2"), Some("…")],
                    &[Some("index"), Some("i1"), Some("t1"), Some("…")],
                ],
            ),
        )
        .answer("SELECT 1 FROM \"t2\" LIMIT 1", table(&["1"], &[&[Some("1")]]))
        .answer("FROM (SELECT 1 FROM \"t1\" LIMIT 51) q", table(&["COUNT(*)"], &[&[Some("50")]]))
        .answer("FROM (SELECT 1 FROM \"t2\" LIMIT 11) q", table(&["COUNT(*)"], &[&[Some("99")]]))
        .answer("SELECT 1 FROM \"t1\" LIMIT 1", table(&["1"], &[&[Some("1")]]));
    let mut rs = ResumeState { run_id: "run1".into(), items: BTreeMap::new(), partial: ResumePartial::Fail };
    rs.items.insert("table:\"t1\"".into(), (CheckpointState::Loading, true, 50));
    rs.items.insert("table:\"t2\"".into(), (CheckpointState::Loading, false, 10));
    let r = req("main");
    let p = plan_with(&r, &src, &tgt, "sqlite", "3.45.0", Some(&rs)).await.unwrap();
    assert_eq!(p.plan.resume_of.as_deref(), Some("run1"));
    let t1 = item(&p, "table:\"t1\"");
    assert_eq!(t1.action, PlanAction::Truncate);
    assert!(codes(&t1.warnings).contains(&"RESUME_RELOAD"), "{:?}", t1.warnings);
    assert!(t1.target_sql.iter().all(|s| !s.starts_with("CREATE")));
    // The index of the reloaded table already exists: done.
    assert_eq!(item(&p, "index:\"t1\".\"i1\"").action, PlanAction::ResumeDone);
    // An interrupted append into a pre-existing table blocks unless `partial` says otherwise.
    assert!(p.plan.blockers.iter().any(|b| b.code == "RESUME_APPEND_PARTIAL" && b.item_id.as_deref() == Some("table:\"t2\"")));

    rs.partial = ResumePartial::Truncate;
    let p = plan_with(&r, &src, &tgt, "sqlite", "3.45.0", Some(&rs)).await.unwrap();
    let t2 = item(&p, "table:\"t2\"");
    assert_eq!(t2.action, PlanAction::Truncate);
    assert!(codes(&t2.warnings).contains(&"DESTRUCTIVE_TRUNCATE"));
    rs.partial = ResumePartial::Skip;
    let p = plan_with(&r, &src, &tgt, "sqlite", "3.45.0", Some(&rs)).await.unwrap();
    assert_eq!(item(&p, "table:\"t2\"").action, PlanAction::ResumeDone);

    // More rows than this run wrote: an application wrote meanwhile.
    rs.items.insert("table:\"t1\"".into(), (CheckpointState::Failed, true, 60));
    let tgt2 = tgt.clone().answer("FROM (SELECT 1 FROM \"t1\" LIMIT 61) q", table(&["COUNT(*)"], &[&[Some("61")]]));
    let p = plan_with(&r, &src, &tgt2, "sqlite", "3.45.0", Some(&rs)).await.unwrap();
    assert!(codes(&item(&p, "table:\"t1\"").warnings).contains(&"DESTRUCTIVE_TRUNCATE"));

    // creating + exists: the CREATE (and its follow-ups) may be incomplete and nothing was built
    // on the run's own table: drop and create it again; creating + missing: create; done:
    // resume_done.
    let mut rs = ResumeState { run_id: "run1".into(), items: BTreeMap::new(), partial: ResumePartial::Fail };
    rs.items.insert("table:\"t1\"".into(), (CheckpointState::Creating, true, 0));
    rs.items.insert("table:\"t2\"".into(), (CheckpointState::Done, false, 3));
    let p = plan_with(&r, &src, &tgt, "sqlite", "3.45.0", Some(&rs)).await.unwrap();
    let t1 = item(&p, "table:\"t1\"");
    assert_eq!(t1.action, PlanAction::DropRecreate);
    assert_eq!(t1.target_sql[..2], ["DROP TABLE \"t1\"".to_string(), "CREATE TABLE t1 (id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT)".to_string()]);
    assert_eq!(item(&p, "table:\"t2\"").action, PlanAction::ResumeDone);
    assert!(codes(&item(&p, "table:\"t2\"").warnings).contains(&"RESUME_DONE"));
    let p = plan_with(&r, &src, &sqlite_target(false, &cols), "sqlite", "3.45.0", Some(&rs)).await.unwrap();
    assert_eq!(item(&p, "table:\"t1\"").action, PlanAction::Create);
    // An earlier job built on it (its view is done): the CREATE had completed, truncate + load.
    rs.items.insert("view:\"v1\"".into(), (CheckpointState::Done, true, 0));
    let p = plan_with(&r, &src, &tgt, "sqlite", "3.45.0", Some(&rs)).await.unwrap();
    let t1 = item(&p, "table:\"t1\"");
    assert_eq!(t1.action, PlanAction::Truncate);
    assert!(t1.target_sql.iter().all(|s| !s.starts_with("CREATE")));

    // Loaded: only its post-data items, when missing.
    let mut rs = ResumeState { run_id: "run1".into(), items: BTreeMap::new(), partial: ResumePartial::Fail };
    rs.items.insert("table:\"t1\"".into(), (CheckpointState::Loaded, true, 5));
    let tgt3 = RecordingTarget::new(TargetFacts::new(CopyFamily::Sqlite, "sqlite", "3.45.0", "main"))
        .answer("sqlite_master", table(&["type", "name", "tbl_name", "sql"], &[&[Some("table"), Some("t1"), Some("t1"), Some("…")]]));
    let p = plan_with(&r, &src, &tgt3, "sqlite", "3.45.0", Some(&rs)).await.unwrap();
    assert_eq!(item(&p, "table:\"t1\"").action, PlanAction::ResumeDone);
    assert!(!p.tables.contains_key("table:\"t1\""));
    assert_eq!(item(&p, "index:\"t1\".\"i1\"").action, PlanAction::Create);
    // ... and its finalize items, which an earlier job never ran (never checkpointed).
    assert_eq!(item(&p, "auto_increment:\"t1\"").action, PlanAction::Run);
    assert_eq!(item(&p, "analyze:\"t1\"").action, PlanAction::Run);
}

/// The target of the resume tests below: t1 (with i1) and t2 exist, both hold rows.
fn resume_target() -> RecordingTarget {
    RecordingTarget::new(TargetFacts::new(CopyFamily::Sqlite, "sqlite", "3.45.0", "main"))
        .answer(
            "sqlite_master",
            table(
                &["type", "name", "tbl_name", "sql"],
                &[
                    &[Some("table"), Some("t1"), Some("t1"), Some("…")],
                    &[Some("table"), Some("t2"), Some("t2"), Some("…")],
                    &[Some("index"), Some("i1"), Some("t1"), Some("…")],
                ],
            ),
        )
        .answer("SELECT 1 FROM \"t1\" LIMIT 1", table(&["1"], &[&[Some("1")]]))
        .answer("SELECT 1 FROM \"t2\" LIMIT 1", table(&["1"], &[&[Some("1")]]))
        .answer("FROM (SELECT 1 FROM \"t1\" LIMIT", table(&["COUNT(*)"], &[&[Some("5")]]))
}

fn resume_state(items: &[(&str, CheckpointState, bool, u64)]) -> ResumeState {
    ResumeState { run_id: "run1".into(), items: items.iter().map(|(id, s, c, n)| (id.to_string(), (*s, *c, *n))).collect(), partial: ResumePartial::Fail }
}

#[tokio::test]
async fn resume_runs_finalize_items_of_finished_tables() {
    // Default policy skip: every table the run created exists now. A finished table's sequence
    // value / AUTO_INCREMENT and ANALYZE still run unless they finished themselves (the
    // SEQUENCES_NOT_SET hazard otherwise: the next INSERT collides with copied keys).
    let src = sqlite_source();
    let r = req("main");
    for state in [CheckpointState::Done, CheckpointState::Loaded] {
        let rs = resume_state(&[("table:\"t1\"", state, true, 5), ("index:\"t1\".\"i1\"", CheckpointState::Done, true, 0)]);
        let p = plan_with(&r, &src, &resume_target(), "sqlite", "3.45.0", Some(&rs)).await.unwrap();
        assert_eq!(item(&p, "table:\"t1\"").action, PlanAction::ResumeDone, "{state:?}");
        assert_eq!(item(&p, "index:\"t1\".\"i1\"").action, PlanAction::ResumeDone);
        let ai = item(&p, "auto_increment:\"t1\"");
        assert_eq!(ai.action, PlanAction::Run, "{state:?}");
        assert!(ai.target_sql[0].starts_with("UPDATE sqlite_sequence SET seq = max(seq,"), "{:?}", ai.target_sql);
        assert_eq!(item(&p, "analyze:\"t1\"").action, PlanAction::Run);
        // t2 was not touched by the run: still skipped, its finalize items too.
        assert_eq!(item(&p, "table:\"t2\"").action, PlanAction::SkipExisting);
        assert_eq!(item(&p, "analyze:\"t2\"").action, PlanAction::SkipExisting);
    }
    // A finalize item that finished in the earlier job is not run again.
    let rs = resume_state(&[("table:\"t1\"", CheckpointState::Done, true, 5), ("auto_increment:\"t1\"", CheckpointState::Done, false, 0)]);
    let p = plan_with(&r, &src, &resume_target(), "sqlite", "3.45.0", Some(&rs)).await.unwrap();
    assert_eq!(item(&p, "auto_increment:\"t1\"").action, PlanAction::ResumeDone);
    assert_eq!(item(&p, "analyze:\"t1\"").action, PlanAction::Run);
    // A table the earlier job skipped as existing keeps its finalize items skipped.
    let rs = resume_state(&[("table:\"t2\"", CheckpointState::Skipped, false, 0)]);
    let p = plan_with(&r, &src, &resume_target(), "sqlite", "3.45.0", Some(&rs)).await.unwrap();
    assert_eq!(item(&p, "table:\"t2\"").action, PlanAction::ResumeDone);
    assert_eq!(item(&p, "analyze:\"t2\"").action, PlanAction::SkipExisting);
}

#[tokio::test]
async fn resume_reappends_an_interrupted_append_that_committed_nothing() {
    // An append into a pre-existing table (not created by the run) that was interrupted with 0
    // committed rows: a rolled-back single-transaction PG load, or no committed MySQL/SQLite
    // chunk. The checkpoint records committed rows, so a plain re-append is exact and the user is
    // not forced into the destructive truncate/skip resume choice.
    let src = sqlite_source();
    let r = req("main");
    for state in [CheckpointState::Loading, CheckpointState::Failed] {
        let rs = resume_state(&[("table:\"t2\"", state, false, 0)]);
        let p = plan_with(&r, &src, &resume_target(), "sqlite", "3.45.0", Some(&rs)).await.unwrap();
        let t2 = item(&p, "table:\"t2\"");
        assert_eq!(t2.action, PlanAction::Append, "{state:?}");
        assert!(codes(&t2.warnings).contains(&"RESUME_APPEND_EMPTY"), "{state:?}: {:?}", t2.warnings);
        assert!(!p.plan.blockers.iter().any(|b| b.code == "RESUME_APPEND_PARTIAL"), "{state:?}: {:?}", p.plan.blockers);
    }
    // A non-zero committed count means rows are really in the table: the explicit partial choice
    // still applies (default `fail`: block).
    let rs = resume_state(&[("table:\"t2\"", CheckpointState::Failed, false, 7)]);
    let p = plan_with(&r, &src, &resume_target(), "sqlite", "3.45.0", Some(&rs)).await.unwrap();
    assert!(p.plan.blockers.iter().any(|b| b.code == "RESUME_APPEND_PARTIAL" && b.item_id.as_deref() == Some("table:\"t2\"")), "{:?}", p.plan.blockers);
}

#[tokio::test]
async fn resume_rebuilds_a_table_whose_create_failed() {
    // pre_data failure (e.g. DDL_SUBSTITUTED, SET COMPRESSION lz4): `failed` before any row and
    // nothing built on it. The run's own table is dropped and created again, its children too.
    let src = sqlite_source();
    let r = req("main");
    // t1 exists and is empty (no answer to its row probe); t2 is gone.
    let tgt = RecordingTarget::new(TargetFacts::new(CopyFamily::Sqlite, "sqlite", "3.45.0", "main"))
        .answer(
            "sqlite_master",
            table(&["type", "name", "tbl_name", "sql"], &[&[Some("table"), Some("t1"), Some("t1"), Some("…")], &[Some("index"), Some("i1"), Some("t1"), Some("…")]]),
        )
        .answer("FROM (SELECT 1 FROM \"t1\" LIMIT", table(&["COUNT(*)"], &[&[Some("0")]]));
    let rs = resume_state(&[("table:\"t1\"", CheckpointState::Failed, true, 0)]);
    let p = plan_with(&r, &src, &tgt, "sqlite", "3.45.0", Some(&rs)).await.unwrap();
    let t1 = item(&p, "table:\"t1\"");
    assert_eq!(t1.action, PlanAction::DropRecreate);
    assert_eq!(t1.target_sql[0], "DROP TABLE \"t1\"");
    assert!(t1.target_sql.iter().any(|s| s.starts_with("CREATE TABLE t1")), "{:?}", t1.target_sql);
    assert!(codes(&t1.warnings).contains(&"RESUME_RELOAD"));
    assert!(!codes(&t1.warnings).contains(&"DESTRUCTIVE_DROP"), "the run's own empty table");
    assert!(p.tables.contains_key("table:\"t1\""));
    // The dropped table takes its index along: created again, whatever its checkpoint says.
    assert_eq!(item(&p, "index:\"t1\".\"i1\"").action, PlanAction::Create);
    assert_eq!(item(&p, "auto_increment:\"t1\"").action, PlanAction::Run);
    assert!(p.plan.blockers.is_empty(), "{:?}", p.plan.blockers);

    // Rows were written before the failure: it failed in data, truncate + reload as before.
    let rs = resume_state(&[("table:\"t1\"", CheckpointState::Failed, true, 4)]);
    let p = plan_with(&r, &src, &tgt, "sqlite", "3.45.0", Some(&rs)).await.unwrap();
    let t1 = item(&p, "table:\"t1\"");
    assert_eq!(t1.action, PlanAction::Truncate);
    assert!(codes(&t1.warnings).contains(&"RESUME_RELOAD"));
    // A dependency-skipped table the run created and something was built on: truncate + load,
    // never resume_done (it was never loaded).
    let rs = resume_state(&[("table:\"t1\"", CheckpointState::Skipped, true, 0), ("view:\"v1\"", CheckpointState::Done, true, 0)]);
    let p = plan_with(&r, &src, &tgt, "sqlite", "3.45.0", Some(&rs)).await.unwrap();
    assert_eq!(item(&p, "table:\"t1\"").action, PlanAction::Truncate);
    assert!(p.tables.contains_key("table:\"t1\""));
}

#[tokio::test]
async fn resume_truncate_of_a_populated_table_needs_an_acknowledgement() {
    // Stored policy truncate: tables the cancelled job never reached (no checkpoint, or
    // pending) and created tables without the count probe still hold rows the reload deletes.
    let src = sqlite_source();
    let mut r = req("main");
    r.options.existing = ExistingTablePolicy::Truncate;
    for t2 in [None, Some(CheckpointState::Pending)] {
        let mut items = vec![("table:\"t1\"", CheckpointState::Done, true, 5)];
        if let Some(s) = t2 {
            items.push(("table:\"t2\"", s, false, 0));
        }
        let p = plan_with(&r, &src, &resume_target(), "sqlite", "3.45.0", Some(&resume_state(&items))).await.unwrap();
        let t2i = item(&p, "table:\"t2\"");
        assert_eq!(t2i.action, PlanAction::Truncate, "{t2:?}");
        assert!(codes(&t2i.warnings).contains(&"DESTRUCTIVE_TRUNCATE"), "{t2:?}: {:?}", t2i.warnings);
    }
    // `created` (the run's own table, nothing loaded yet) holding rows: someone else wrote them.
    let rs = resume_state(&[("table:\"t1\"", CheckpointState::Created, true, 0)]);
    let p = plan_with(&r, &src, &resume_target(), "sqlite", "3.45.0", Some(&rs)).await.unwrap();
    let t1 = item(&p, "table:\"t1\"");
    assert_eq!(t1.action, PlanAction::Truncate);
    assert!(codes(&t1.warnings).contains(&"DESTRUCTIVE_TRUNCATE"));
    // A table the mapping already probed is explained once (RESUME_RELOAD, the run's own rows).
    let rs = resume_state(&[("table:\"t1\"", CheckpointState::Loading, true, 5)]);
    let p = plan_with(&r, &src, &resume_target(), "sqlite", "3.45.0", Some(&rs)).await.unwrap();
    let t1 = item(&p, "table:\"t1\"");
    assert!(codes(&t1.warnings).contains(&"RESUME_RELOAD") && !codes(&t1.warnings).contains(&"DESTRUCTIVE_TRUNCATE"), "{:?}", t1.warnings);
    // A dependency-skipped load into a pre-existing table never ran: the fresh truncate stands.
    let rs = resume_state(&[("table:\"t2\"", CheckpointState::Skipped, false, 0)]);
    let p = plan_with(&r, &src, &resume_target(), "sqlite", "3.45.0", Some(&rs)).await.unwrap();
    let t2 = item(&p, "table:\"t2\"");
    assert_eq!(t2.action, PlanAction::Truncate);
    assert!(codes(&t2.warnings).contains(&"DESTRUCTIVE_TRUNCATE"));
}

#[tokio::test]
async fn resume_applies_comments_and_grants_of_objects_the_run_created() {
    // PG-like assembly: a table the run created and finished, plus its comment and sequence
    // value that a cancel in finalize never ran; and a pre-existing table loaded by policy.
    let mk = |table: &str, kind: CopyObjectKind, sql: &str| {
        let tid = item_id(CopyObjectKind::Table, &["app", table]);
        let mut d = Draft::new(kind, item_id(kind, &["app", table]), table, table, CopyPhase::Finalize);
        d.parent = Some(tid.clone());
        d.depends_on = vec![tid];
        d.create = vec![sql.to_string()];
        d
    };
    let mut drafts = vec![tdraft("orders"), tdraft("legacy")];
    for t in ["orders", "legacy"] {
        drafts.push(mk(t, CopyObjectKind::Comment, &format!("COMMENT ON TABLE \"{t}\" IS 'x'")));
        let mut sv = mk(t, CopyObjectKind::SequenceValue, "");
        sv.seq_templates = Some((
            "SELECT pg_catalog.setval('\"s\"', <source value>, <is_called>)".into(),
            "SELECT pg_catalog.setval('\"s\"', GREATEST(<source value>, <target value>), true)".into(),
        ));
        drafts.push(sv);
    }
    let mut r = req("app");
    r.options.existing_overrides = vec![ItemPolicy { item_id: "table:\"app\".\"legacy\"".into(), policy: ExistingTablePolicy::Truncate }];
    let rs = resume_state(&[("table:\"app\".\"orders\"", CheckpointState::Done, true, 10), ("table:\"app\".\"legacy\"", CheckpointState::Done, false, 10)]);
    let src = CannedMeta::new();
    let conn = Connection { driver: "postgres".into(), ..Default::default() };
    let server = ServerInfo::default();
    let cancel = CancellationToken::new();
    let tgt = pg_target();
    let input = PlanInput {
        req: &r,
        source: &src,
        source_conn: &conn,
        source_server: &server,
        mysql_source: None,
        target: &tgt,
        target_conn: &conn,
        resume: Some(&rs),
        cancel: &cancel,
    };
    let p = assemble(&input, CopyFamily::Postgres, collected(drafts, existing(&["orders", "legacy"]))).await.unwrap();
    assert_eq!(item(&p, "table:\"app\".\"orders\"").action, PlanAction::ResumeDone);
    assert_eq!(item(&p, &item_id(CopyObjectKind::Comment, &["app", "orders"])).action, PlanAction::Run);
    let sv = item(&p, &item_id(CopyObjectKind::SequenceValue, &["app", "orders"]));
    assert_eq!(sv.action, PlanAction::Run);
    assert!(sv.target_sql[0].contains("GREATEST"), "the floor rule: {:?}", sv.target_sql);
    // The pre-existing table is not the run's: its comment stays skipped, the sequence value runs.
    assert_eq!(item(&p, "table:\"app\".\"legacy\"").action, PlanAction::ResumeDone);
    assert_eq!(item(&p, &item_id(CopyObjectKind::Comment, &["app", "legacy"])).action, PlanAction::SkipExisting);
    assert_eq!(item(&p, &item_id(CopyObjectKind::SequenceValue, &["app", "legacy"])).action, PlanAction::Run);
}

// ---------------------------------------------------------------------------------------------
// Generic assembly over hand-made drafts (PG-like ops)
// ---------------------------------------------------------------------------------------------

fn pg_ops() -> FamilyOps {
    FamilyOps {
        family: CopyFamily::Postgres,
        probe_sql: |k, _| format!("SELECT EXISTS (SELECT 1 FROM ONLY {}.{}) AS e", dq(k.schema.as_deref().unwrap_or("")), dq(&k.name)),
        count_sql: |k, n, _| format!("SELECT COUNT(*) FROM (SELECT 1 FROM {} LIMIT {n}) q", dq(&k.name)),
        drop_fk_sql: |k, name| Some(format!("ALTER TABLE {} DROP CONSTRAINT {}", dq(&k.name), dq(name))),
        truncate_group_sql: Some(|keys| {
            format!("TRUNCATE {}", keys.iter().map(|(k, p)| format!("{}{}", if *p { "" } else { "ONLY " }, dq(&k.name))).collect::<Vec<_>>().join(", "))
        }),
        quote_rel: |k| dq(&k.name),
    }
}

fn tdraft(name: &str) -> Draft {
    let id = item_id(CopyObjectKind::Table, &["app", name]);
    let mut d = Draft::new(CopyObjectKind::Table, id.clone(), name, name, CopyPhase::PreData);
    d.schema = Some("app".into());
    d.target_schema = Some("app".into());
    d.exists_key = Some(TKey::rel(Some("app"), name));
    d.create = vec![format!("CREATE TABLE {}", dq(name))];
    d.drop = vec![format!("DROP TABLE {} RESTRICT", dq(name))];
    d.truncate = Some(format!("TRUNCATE ONLY {}", dq(name)));
    d.load_template = Some(format!("COPY {} (\"a\") FROM STDIN (FORMAT csv)", dq(name)));
    d.data = Some(TableDataSpec {
        item_id: id,
        source_sql: "SELECT 1".into(),
        verify_sql: String::new(),
        raw_csv: true,
        transcode: None,
        target: TargetTable { schema: Some("app".into()), name: name.into() },
        columns: vec![],
        est_rows: Some(10),
        first_timestamp_auto: false,
        fk_checks: false,
    });
    d.est_rows = Some(10);
    d
}

fn collected(drafts: Vec<Draft>, target: TargetState) -> Collected {
    Collected {
        drafts,
        warnings: vec![],
        blockers: vec![],
        source_facts: CopyServerFacts::default(),
        target_facts: CopyServerFacts::default(),
        effective: CopyEffective {
            charset: None,
            collation: None,
            engine: None,
            zero_dates: None,
            time_zone: None,
            load_method: LoadMethod::PgCopyCsv,
            table_concurrency: 1,
            chunk_bytes: None,
            consistent_snapshot: true,
            source_sessions: 1,
            target_sessions: 2,
        },
        session_sql: SessionSql::default(),
        target,
        ops: pg_ops(),
        target_db_missing: false,
    }
}

async fn assemble_with(req: &CopyRequest, tgt: &RecordingTarget, c: Collected) -> PlannedCopy {
    let src = CannedMeta::new();
    let conn = Connection { driver: "postgres".into(), ..Default::default() };
    let server = ServerInfo::default();
    let cancel = CancellationToken::new();
    let input =
        PlanInput { req, source: &src, source_conn: &conn, source_server: &server, mysql_source: None, target: tgt, target_conn: &conn, resume: None, cancel: &cancel };
    assemble(&input, CopyFamily::Postgres, c).await.unwrap()
}

fn existing(names: &[&str]) -> TargetState {
    let mut st = TargetState::default();
    for n in names {
        st.objects.insert(TKey::rel(Some("app"), n), TObj { kind: "r".into(), est_rows: Some(0), ..Default::default() });
    }
    st
}

fn pg_target() -> RecordingTarget {
    RecordingTarget::new(TargetFacts::new(CopyFamily::Postgres, "postgres", "16.2", "app")).answer("SELECT EXISTS", table(&["e"], &[&[Some("t")]]))
}

#[tokio::test]
async fn truncate_groups_follow_target_foreign_keys() {
    let mut r = req("app");
    r.options.existing = ExistingTablePolicy::Truncate;
    let mut st = existing(&["customers", "orders", "items", "audit"]);
    st.fks.push(TFk { child: TKey::rel(Some("app"), "orders"), name: "orders_customer_fk".into(), parent: TKey::rel(Some("app"), "customers") });
    st.fks.push(TFk { child: TKey::rel(Some("app"), "items"), name: "items_order_fk".into(), parent: TKey::rel(Some("app"), "orders") });
    let drafts = vec![tdraft("customers"), tdraft("orders"), tdraft("items"), tdraft("audit")];
    let p = assemble_with(&r, &pg_target(), collected(drafts, st)).await;
    let g = item(&p, "truncate_group:\"app\".\"customers\"");
    assert_eq!((g.kind, g.phase, g.order, g.action), (CopyObjectKind::TruncateGroup, CopyPhase::Data, 0, PlanAction::Run));
    assert_eq!(g.target_sql, vec!["TRUNCATE ONLY \"customers\", ONLY \"items\", ONLY \"orders\"".to_string()]);
    assert!(codes(&g.warnings).contains(&"TRUNCATE_GROUPED"));
    let orders = item(&p, "table:\"app\".\"orders\"");
    assert!(orders.depends_on.contains(&g.id));
    assert!(!orders.target_sql.iter().any(|s| s.starts_with("TRUNCATE")), "the group empties it: {:?}", orders.target_sql);
    // Existing tables load parents first (PG enforces existing FKs during COPY).
    assert!(orders.depends_on.contains(&"table:\"app\".\"customers\"".to_string()));
    assert!(item(&p, "table:\"app\".\"items\"").depends_on.contains(&"table:\"app\".\"orders\"".to_string()));
    // A table outside any group truncates inside its own load transaction.
    let audit = item(&p, "table:\"app\".\"audit\"");
    assert_eq!(audit.target_sql[0], "TRUNCATE ONLY \"audit\"");
    assert!(codes(&audit.warnings).contains(&"TARGET_LOCKED_DURING_LOAD") && codes(&audit.warnings).contains(&"DESTRUCTIVE_TRUNCATE"));

    // A referencing table that is not truncated blocks the truncate.
    let mut st = existing(&["customers", "orders"]);
    st.fks.push(TFk { child: TKey::rel(Some("app"), "orders"), name: "orders_customer_fk".into(), parent: TKey::rel(Some("app"), "customers") });
    r.options.existing_overrides = vec![ItemPolicy { item_id: "table:\"app\".\"orders\"".into(), policy: ExistingTablePolicy::Skip }];
    let p = assemble_with(&r, &pg_target(), collected(vec![tdraft("customers"), tdraft("orders")], st)).await;
    assert!(p.plan.blockers.iter().any(|b| b.code == "TRUNCATE_FK_REFERENCED" && b.item_id.as_deref() == Some("table:\"app\".\"customers\"")));
}

#[tokio::test]
async fn drop_recreate_lists_dependants_and_orders_drops() {
    let mut r = req("app");
    r.options.existing = ExistingTablePolicy::DropRecreate;
    let mut st = existing(&["customers", "orders", "other"]);
    st.objects.insert(TKey::rel(Some("app"), "v_orders"), TObj { kind: "v".into(), ..Default::default() });
    st.objects.insert(TKey::rel(Some("app"), "v_ext"), TObj { kind: "v".into(), ..Default::default() });
    st.fks.push(TFk { child: TKey::rel(Some("app"), "orders"), name: "orders_customer_fk".into(), parent: TKey::rel(Some("app"), "customers") });
    st.fks.push(TFk { child: TKey::rel(Some("app"), "other"), name: "other_fk".into(), parent: TKey::rel(Some("app"), "orders") });
    st.view_deps.push((TKey::rel(Some("app"), "v_orders"), TKey::rel(Some("app"), "orders")));
    let mut view = Draft::new(CopyObjectKind::View, item_id(CopyObjectKind::View, &["app", "v_orders"]), "v_orders", "v_orders", CopyPhase::PreData);
    view.exists_key = Some(TKey::rel(Some("app"), "v_orders"));
    view.create = vec!["CREATE VIEW \"v_orders\" AS SELECT 1".into()];
    view.drop = vec!["DROP VIEW \"v_orders\" RESTRICT".into()];
    view.depends_on = vec!["table:\"app\".\"orders\"".into()];
    let drafts = vec![tdraft("customers"), tdraft("orders"), view];
    let p = assemble_with(&r, &pg_target(), collected(drafts, st.clone())).await;
    // `other` (not in the plan) references orders: the drop is blocked.
    let b = p.plan.blockers.iter().find(|b| b.code == "DROP_HAS_DEPENDANTS").expect("blocker");
    assert_eq!(b.item_id.as_deref(), Some("table:\"app\".\"orders\""));
    assert!(b.detail.as_deref().unwrap().contains("other_fk"));
    // Dependants that are drop_recreate items are dropped first, inside the first item that needs it.
    let customers = item(&p, "table:\"app\".\"customers\"");
    let orders = item(&p, "table:\"app\".\"orders\"");
    let all: Vec<&String> = p.plan.items.iter().filter(|i| i.phase == CopyPhase::PreData).flat_map(|i| i.target_sql.iter()).collect();
    let pos = |s: &str| all.iter().position(|x| x.as_str() == s).unwrap_or_else(|| panic!("{s} not in {all:?}"));
    assert!(pos("DROP VIEW \"v_orders\" RESTRICT") < pos("DROP TABLE \"orders\" RESTRICT"));
    assert!(pos("DROP TABLE \"orders\" RESTRICT") < pos("CREATE VIEW \"v_orders\" AS SELECT 1"));
    // The FK orders -> customers goes before customers is dropped when orders is dropped later.
    if customers.order < orders.order {
        assert!(pos("ALTER TABLE \"orders\" DROP CONSTRAINT \"orders_customer_fk\"") < pos("DROP TABLE \"customers\" RESTRICT"));
    }
    assert_eq!(item(&p, "view:\"app\".\"v_orders\"").target_sql, vec!["CREATE VIEW \"v_orders\" AS SELECT 1".to_string()]);
    assert!(codes(&customers.warnings).contains(&"DESTRUCTIVE_DROP"));
}

#[tokio::test]
async fn foreign_key_to_a_skipped_table_and_name_collision() {
    let r = req("app");
    let st = existing(&["customers"]);
    let mut fk = Draft::new(CopyObjectKind::ForeignKey, item_id(CopyObjectKind::ForeignKey, &["app", "orders", "fk"]), "fk", "fk", CopyPhase::PostData);
    fk.parent = Some("table:\"app\".\"orders\"".into());
    fk.depends_on = vec!["table:\"app\".\"orders\"".into(), "table:\"app\".\"customers\"".into()];
    fk.fk_parent = Some("table:\"app\".\"customers\"".into());
    fk.create = vec!["ALTER TABLE ONLY \"orders\" ADD CONSTRAINT \"fk\" FOREIGN KEY (a) REFERENCES customers(a)".into()];
    let mut dup = tdraft("Orders");
    dup.target_name = "orders".into();
    dup.id = item_id(CopyObjectKind::Table, &["app", "Orders"]);
    let mut c = collected(vec![tdraft("customers"), tdraft("orders"), fk, dup], st);
    c.ops.family = CopyFamily::Postgres;
    let p = assemble_with(&r, &pg_target(), c).await;
    let fk = item(&p, "foreign_key:\"app\".\"orders\".\"fk\"");
    assert_eq!(fk.action, PlanAction::Create);
    assert!(codes(&fk.warnings).contains(&"FK_TO_SKIPPED"));
    assert!(p.plan.blockers.iter().any(|b| b.code == "NAME_COLLISION"));
    assert!(fk.order > 0 || p.plan.items.iter().filter(|i| i.phase == CopyPhase::PostData).count() == 1);
}

/// Every statement of real SQLite plans (all four policies, a target holding every object)
/// passes the executor's allow-lists in the context the executor checks it (5.2): drops of
/// tables and views (indexes and triggers go with their dropped table; their own DROP shapes are
/// covered by `exec::tests::allow_lists_accept_every_translator_shape`), `DELETE FROM`
/// truncates, trigger bodies with `;`, the inlined `sqlite_sequence` finalize statements.
#[tokio::test]
async fn sqlite_plans_pass_the_executor_allow_lists() {
    let src = sqlite_source();
    let cols = ["name", "type", "notnull", "dflt_value", "pk", "hidden"];
    let full_target = || {
        let master = table(
            &["type", "name", "tbl_name", "sql"],
            &[
                &[Some("table"), Some("t1"), Some("t1"), Some("CREATE TABLE t1 (id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT)")],
                &[Some("table"), Some("t2"), Some("t2"), Some("CREATE TABLE t2 (a INTEGER, b TEXT)")],
                &[Some("index"), Some("i1"), Some("t1"), Some("CREATE INDEX i1 ON t1(name)")],
                &[Some("view"), Some("v1"), Some("v1"), Some("CREATE VIEW v1 AS SELECT id FROM t1")],
                &[Some("view"), Some("v2"), Some("v2"), Some("CREATE VIEW v2 AS SELECT * FROM v1 JOIN t2")],
                &[Some("trigger"), Some("tr"), Some("t2"), Some("CREATE TRIGGER tr AFTER INSERT ON t2 BEGIN SELECT 1; END")],
            ],
        );
        RecordingTarget::new(TargetFacts::new(CopyFamily::Sqlite, "sqlite", "3.45.0", "main"))
            .answer("sqlite_master", master)
            .answer(
                "pragma_table_xinfo('t1')",
                table(
                    &cols,
                    &[&[Some("id"), Some("INTEGER"), Some("0"), None, Some("1"), Some("0")], &[Some("name"), Some("TEXT"), Some("0"), None, Some("0"), Some("0")]],
                ),
            )
            .answer(
                "pragma_table_xinfo('t2')",
                table(&cols, &[&[Some("a"), Some("INTEGER"), Some("0"), None, Some("0"), Some("0")], &[Some("b"), Some("TEXT"), Some("0"), None, Some("0"), Some("0")]]),
            )
    };
    let mut seen: Vec<String> = Vec::new();
    for policy in [ExistingTablePolicy::Skip, ExistingTablePolicy::Truncate, ExistingTablePolicy::DropRecreate, ExistingTablePolicy::Append] {
        for tgt in [full_target(), RecordingTarget::new(TargetFacts::new(CopyFamily::Sqlite, "sqlite", "3.45.0", "main"))] {
            let mut r = req("main");
            r.options.existing = policy;
            let p = plan_with(&r, &src, &tgt, "sqlite", "3.45.0", None).await.unwrap();
            assert_eq!(crate::copy::exec::refused_statements(&p), Vec::<String>::new(), "{policy:?}");
            seen.extend(crate::copy::exec::executed_statements(&p).into_iter().map(|(s, _)| s));
        }
    }
    for want in [
        "DROP TABLE \"t1\"",
        "DROP VIEW \"v1\"",
        "DELETE FROM \"t2\"",
        "CREATE TRIGGER tr AFTER INSERT ON t2 BEGIN SELECT 1; END",
        "UPDATE sqlite_sequence SET seq = max(seq, 42) WHERE name = 't1'",
        "INSERT INTO sqlite_sequence(name, seq) SELECT 't1', 42 WHERE NOT EXISTS",
    ] {
        assert!(seen.iter().any(|s| s.starts_with(want)), "{want} never planned: {seen:#?}");
    }
}
