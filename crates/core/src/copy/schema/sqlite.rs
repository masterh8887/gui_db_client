//! SQLite -> SQLite (3.6): DDL verbatim from `sqlite_master`; tables (pre_data), data, indexes
//! with SQL (post_data), views, triggers; virtual tables and their shadow tables `unsupported`;
//! `sqlite_sequence` set in finalize.

use std::collections::{BTreeMap, BTreeSet};

use crate::copy::plan::{selected, Collected, Draft, FamilyOps, PlanInput, TClass, TCol, TKey, TObj, TargetState};
use crate::copy::translate::{dq, item_id, sqlite_name_literal, warning, IssueExt};
use crate::copy::types::*;
use crate::error::{CoreError, Result};

/// `sqlite_master` without SQLite's own objects. The `_` of `sqlite_` is escaped: unescaped it
/// would also hide user tables such as `sqlitex`.
pub(crate) const MASTER_SQL: &str = "SELECT type, name, tbl_name, sql FROM sqlite_master WHERE name NOT LIKE 'sqlite\\_%' ESCAPE '\\'";

/// Shadow table suffixes of FTS3/4/5, R*Tree and other virtual table modules.
const SHADOW_SUFFIXES: &[&str] = &["_content", "_data", "_idx", "_docsize", "_config", "_segments", "_segdir", "_stat", "_node", "_parent", "_rowid"];

fn columns_sql(table: &str) -> String {
    format!("SELECT name, type, \"notnull\", dflt_value, pk, hidden FROM pragma_table_xinfo({})", sqlite_name_literal(table))
}

fn value_kind(decl: &str) -> ValueKind {
    let t = decl.to_ascii_uppercase();
    if t.contains("INT") {
        ValueKind::Integer
    } else if t.contains("CHAR") || t.contains("CLOB") || t.contains("TEXT") {
        ValueKind::Text
    } else if t.contains("BLOB") {
        ValueKind::Binary
    } else if t.contains("REAL") || t.contains("FLOA") || t.contains("DOUB") {
        ValueKind::Float
    } else {
        ValueKind::Raw
    }
}

fn ops() -> FamilyOps {
    FamilyOps {
        family: CopyFamily::Sqlite,
        probe_sql: |k, _| format!("SELECT 1 FROM {} LIMIT 1", dq(&k.name)),
        count_sql: |k, n, _| format!("SELECT COUNT(*) FROM (SELECT 1 FROM {} LIMIT {n}) q", dq(&k.name)),
        drop_fk_sql: |_, _| None,
        truncate_group_sql: None,
        quote_rel: |k| dq(&k.name),
    }
}

/// Names referenced after FROM / JOIN in a view's SQL (best effort, for ordering).
fn view_refs(sql: &str) -> BTreeSet<String> {
    let toks = crate::copy::translate::mysql::lex(&sql.replace('"', "`"));
    let mut out = BTreeSet::new();
    for w in toks.windows(2) {
        if (w[0].is_word("FROM") || w[0].is_word("JOIN")) && matches!(w[1].k, crate::copy::translate::mysql::TK::Ident | crate::copy::translate::mysql::TK::Word) {
            out.insert(w[1].v.clone());
        }
    }
    out
}

async fn read_state(input: &PlanInput<'_>) -> Result<TargetState> {
    let mut st = TargetState::default();
    let m = input.target.query_meta(MASTER_SQL).await?;
    let mut triggers: BTreeMap<String, u32> = BTreeMap::new();
    for r in 0..m.len() {
        let (Some(ty), Some(name)) = (m.get(r, "type"), m.get(r, "name")) else { continue };
        let tbl = m.get(r, "tbl_name").unwrap_or(name);
        match ty {
            "table" | "view" => {
                st.objects.insert(TKey::rel(None, name), TObj { kind: ty.to_string(), ..Default::default() });
            }
            "index" => {
                st.objects.insert(TKey::of(TClass::Index, None, None, name), TObj { kind: ty.to_string(), ..Default::default() });
            }
            "trigger" => {
                st.objects.insert(TKey::of(TClass::Trigger, None, None, name), TObj { kind: ty.to_string(), ..Default::default() });
                *triggers.entry(tbl.to_string()).or_default() += 1;
            }
            _ => {}
        }
    }
    let tables: Vec<String> = st.objects.iter().filter(|(k, o)| k.class == TClass::Relation && o.kind == "table").map(|(k, _)| k.name.clone()).collect();
    for t in tables {
        let cols = input.target.query_meta(&columns_sql(&t)).await?;
        let o = st.objects.get_mut(&TKey::rel(None, &t)).expect("listed above");
        o.triggers = triggers.get(&t).copied().unwrap_or(0);
        for r in 0..cols.len() {
            if cols.get(r, "hidden").is_some_and(|h| h != "0") {
                continue;
            }
            o.columns.push(TCol {
                name: cols.get(r, "name").unwrap_or_default().to_string(),
                type_text: cols.get(r, "type").unwrap_or_default().to_string(),
                not_null: cols.get(r, "notnull") == Some("1"),
                has_default: cols.get(r, "dflt_value").is_some() || cols.get(r, "pk").is_some_and(|p| p != "0"),
            });
        }
    }
    Ok(st)
}

pub(crate) async fn collect(input: &PlanInput<'_>) -> Result<Collected> {
    let req = input.req;
    let opts = &req.options;
    if !opts.rename.is_empty() || opts.name_case == NameCase::Lower || req.target.schema.is_some() {
        return Err(CoreError::Invalid("copy: SQLite copies keep the source names (renames are not supported)".into()));
    }
    let master = input.source.query(MASTER_SQL).await?;
    struct Obj {
        ty: String,
        name: String,
        tbl: String,
        sql: Option<String>,
    }
    let objs: Vec<Obj> = (0..master.len())
        .filter_map(|r| {
            Some(Obj {
                ty: master.get(r, "type")?.to_string(),
                name: master.get(r, "name")?.to_string(),
                tbl: master.get(r, "tbl_name").unwrap_or_default().to_string(),
                sql: master.get(r, "sql").map(str::to_string),
            })
        })
        .collect();
    let vtabs: BTreeSet<String> = objs
        .iter()
        .filter(|o| o.ty == "table" && o.sql.as_deref().is_some_and(|s| s.trim_start().to_ascii_uppercase().starts_with("CREATE VIRTUAL TABLE")))
        .map(|o| o.name.clone())
        .collect();
    let is_shadow = |name: &str| vtabs.iter().any(|vt| SHADOW_SUFFIXES.iter().any(|s| name == format!("{vt}{s}")));
    let table_names: BTreeSet<String> = objs.iter().filter(|o| o.ty == "table").map(|o| o.name.clone()).collect();
    let view_names: BTreeSet<String> = objs.iter().filter(|o| o.ty == "view").map(|o| o.name.clone()).collect();

    let mut drafts: Vec<Draft> = Vec::new();
    let tid = |t: &str| item_id(CopyObjectKind::Table, &[t]);
    for o in objs.iter().filter(|o| o.ty == "table") {
        let id = tid(&o.name);
        let mut d = Draft::new(CopyObjectKind::Table, id.clone(), &o.name, &o.name, CopyPhase::PreData);
        d.selected = selected(req, None, &o.name);
        d.exists_key = Some(TKey::rel(None, &o.name));
        d.source_ddl = o.sql.clone();
        if vtabs.contains(&o.name) || is_shadow(&o.name) {
            d.unsupported = true;
            d.warnings.push(warning("UNSUPPORTED_OBJECT", "virtual tables and their shadow tables are not copied").detail(o.name.clone()));
            drafts.push(d);
            continue;
        }
        if !d.selected {
            drafts.push(d);
            continue;
        }
        let sql = o.sql.clone().unwrap_or_default();
        d.create = vec![sql.clone()];
        d.drop = vec![format!("DROP TABLE {}", dq(&o.name))];
        let cols = input.source.query(&columns_sql(&o.name)).await?;
        let mut columns = Vec::new();
        for r in 0..cols.len() {
            if cols.get(r, "hidden").is_some_and(|h| h != "0") {
                continue;
            }
            let name = cols.get(r, "name").unwrap_or_default().to_string();
            columns.push(DataColumn {
                source: name.clone(),
                target: name,
                value: value_kind(cols.get(r, "type").unwrap_or_default()),
                read: SourceRead::Plain,
                zero_date: ZeroDateAction::NotTemporal,
                invalid_date: ZeroDateAction::NotTemporal,
                binary_attr: false,
                max_bytes: None,
            });
        }
        let list = columns.iter().map(|c| dq(&c.source)).collect::<Vec<_>>().join(", ");
        d.data = Some(TableDataSpec {
            item_id: id.clone(),
            source_sql: format!("SELECT {list} FROM {}", dq(&o.name)),
            verify_sql: format!("SELECT {list} FROM {}", dq(&o.name)),
            raw_csv: false,
            transcode: None,
            target: TargetTable { schema: None, name: o.name.clone() },
            columns,
            est_rows: None,
            first_timestamp_auto: false,
            fk_checks: false,
        });
        d.truncate = Some(format!("DELETE FROM {}", dq(&o.name)));
        d.load_template = Some(format!("INSERT INTO {} ({list}) VALUES <rows>", dq(&o.name)));
        drafts.push(d);
        if sql.to_ascii_uppercase().contains("AUTOINCREMENT") {
            let mut a = Draft::new(CopyObjectKind::AutoIncrement, item_id(CopyObjectKind::AutoIncrement, &[&o.name]), &o.name, &o.name, CopyPhase::Finalize);
            a.parent = Some(id.clone());
            a.depends_on = vec![id.clone()];
            let tpl = format!("UPDATE sqlite_sequence SET seq = max(seq, <source value>) WHERE name = {}", sqlite_name_literal(&o.name));
            a.seq_templates = Some((tpl.clone(), tpl));
            drafts.push(a);
        }
        if opts.objects.analyze {
            let mut a = Draft::new(CopyObjectKind::Analyze, item_id(CopyObjectKind::Analyze, &[&o.name]), &o.name, &o.name, CopyPhase::Finalize);
            a.parent = Some(id.clone());
            a.depends_on = vec![id.clone()];
            a.create = vec![format!("ANALYZE {}", dq(&o.name))];
            drafts.push(a);
        }
    }
    for o in objs.iter().filter(|o| o.ty == "index") {
        let Some(sql) = o.sql.clone() else { continue };
        if vtabs.contains(&o.tbl) || is_shadow(&o.tbl) {
            continue;
        }
        let parent = tid(&o.tbl);
        let mut d = Draft::new(
            CopyObjectKind::Index,
            item_id(CopyObjectKind::Index, &[&o.tbl, &o.name]),
            &o.name,
            &o.name,
            if opts.indexes_after_data { CopyPhase::PostData } else { CopyPhase::PreData },
        );
        d.parent = Some(parent.clone());
        d.depends_on = vec![parent];
        d.create = vec![sql];
        d.drop = vec![format!("DROP INDEX {}", dq(&o.name))];
        d.exists_key = Some(TKey::of(TClass::Index, None, None, &o.name));
        drafts.push(d);
    }
    if opts.objects.views {
        for o in objs.iter().filter(|o| o.ty == "view") {
            let sql = o.sql.clone().unwrap_or_default();
            let mut d = Draft::new(CopyObjectKind::View, item_id(CopyObjectKind::View, &[&o.name]), &o.name, &o.name, CopyPhase::PreData);
            for r in view_refs(&sql) {
                if table_names.contains(&r) {
                    d.depends_on.push(tid(&r));
                } else if view_names.contains(&r) && r != o.name {
                    d.depends_on.push(item_id(CopyObjectKind::View, &[&r]));
                }
            }
            d.create = vec![sql];
            d.drop = vec![format!("DROP VIEW {}", dq(&o.name))];
            d.exists_key = Some(TKey::rel(None, &o.name));
            drafts.push(d);
        }
    }
    if opts.objects.triggers {
        for o in objs.iter().filter(|o| o.ty == "trigger") {
            let parent = if table_names.contains(&o.tbl) { tid(&o.tbl) } else { item_id(CopyObjectKind::View, &[&o.tbl]) };
            let mut d = Draft::new(CopyObjectKind::Trigger, item_id(CopyObjectKind::Trigger, &[&o.tbl, &o.name]), &o.name, &o.name, CopyPhase::PostData);
            d.parent = Some(parent.clone());
            d.depends_on = vec![parent];
            d.create = vec![o.sql.clone().unwrap_or_default()];
            d.drop = vec![format!("DROP TRIGGER {}", dq(&o.name))];
            d.exists_key = Some(TKey::of(TClass::Trigger, None, None, &o.name));
            drafts.push(d);
        }
    }

    let target = read_state(input).await?;
    let tf = input.target.facts();
    let warnings = Vec::new();
    Ok(Collected {
        drafts,
        warnings,
        blockers: Vec::new(),
        source_facts: CopyServerFacts {
            driver: "sqlite".into(),
            version: input.source_server.version.clone(),
            database: "main".into(),
            endpoint: crate::copy::target::endpoint_text(input.source_conn),
            charset: Some("UTF-8".into()),
            ..Default::default()
        },
        target_facts: CopyServerFacts {
            driver: "sqlite".into(),
            version: tf.version.clone(),
            database: "main".into(),
            endpoint: if tf.endpoint.is_empty() { crate::copy::target::endpoint_text(input.target_conn) } else { tf.endpoint.clone() },
            charset: Some("UTF-8".into()),
            ..Default::default()
        },
        effective: CopyEffective {
            charset: None,
            collation: None,
            engine: None,
            zero_dates: None,
            time_zone: None,
            load_method: LoadMethod::SqliteInsert,
            table_concurrency: 1,
            chunk_bytes: None,
            consistent_snapshot: false,
            source_sessions: 1,
            target_sessions: 2,
        },
        session_sql: SessionSql::default(),
        target,
        ops: ops(),
        target_db_missing: false,
    })
}
