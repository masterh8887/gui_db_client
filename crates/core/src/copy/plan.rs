//! Plan building (section 3.7): every item (incl. the `Database` item for a missing MySQL
//! database and `TruncateGroup` items), translation issues, the per-family catalog checks,
//! action derivation (selection, overrides, exclude, resume mapping). Owner: copy-schema.
//!
//! [`build_plan`] runs in two stages:
//!
//! 1. A family collector (`copy::schema::{pg, mysql, sqlite}`) reads the source catalog, the
//!    target state, translates the DDL and returns [`Draft`]s (one per plan item, actions not
//!    decided yet) plus the facts, the effective settings and the session SQL.
//! 2. [`assemble`] (family independent) decides the actions (existing-table policy with SKIP as
//!    the default, overrides, `content`, selection, `exclude`, resume mapping), probes row
//!    presence, lists drop dependants, builds `TruncateGroup` items, orders the items and
//!    composes every item's `target_sql`.
//!
//! **Item statements** (contract with the executor; see `translate::item_sql_parts`):
//! * A `table` item holds its DDL (drops of drop_recreate, `CREATE TABLE`, follow-ups such as
//!   `ALTER SEQUENCE … OWNED BY` or MyISAM `DISABLE KEYS`), then the truncate statement (action
//!   `truncate`, unless a `truncate_group` item empties it), then the load template (data
//!   elided) when `has_data`. Its `phase` is `pre_data` when it runs DDL (`create`,
//!   `drop_recreate`), else `data`; the load itself always runs in the data phase from
//!   `PlannedCopy.tables`.
//! * MySQL secondary keys of a table go into one combined `ALTER TABLE … ADD …, ADD …` (FKs: one
//!   per child table). The combined statement sits on the first item of the group; the other
//!   items of the group have no statements and depend on it.
//! * PG items that run DDL start with `SET search_path = "<target schema>", pg_catalog`.
//! * `sequence_value` / `auto_increment` items hold a template; the executor builds the real
//!   statement with `translate::finalize_sql` from `schema::read_finalize_values` /
//!   `schema::read_target_floor`.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::time::Instant;

use tokio_util::sync::CancellationToken;

use super::schema::SourceMeta;
use super::translate::{info, warning, IssueExt};
use super::types::*;
use crate::error::{CoreError, Result};
use crate::types::{Connection, ServerInfo, Severity};

/// MySQL facts of the source connection (version, caps, text plan).
#[cfg(feature = "mysql")]
pub type MysqlSourceFacts = crate::source::mysql_common::ServerFacts;
/// Without the `mysql` feature there are no MySQL facts.
#[cfg(not(feature = "mysql"))]
pub type MysqlSourceFacts = ();

pub struct PlanInput<'a> {
    pub req: &'a CopyRequest,
    pub source: &'a dyn SourceMeta,
    pub source_conn: &'a Connection,
    pub source_server: &'a ServerInfo,
    pub mysql_source: Option<&'a MysqlSourceFacts>,
    pub target: &'a dyn TargetSession,
    pub target_conn: &'a Connection,
    pub resume: Option<&'a ResumeState>,
    pub cancel: &'a CancellationToken,
}

/// Build the plan. `copy::preflight::finalize` then adds the 5.3 issues, `acks` and `plan_id`.
pub async fn build_plan(input: PlanInput<'_>) -> Result<PlannedCopy> {
    let started = Instant::now();
    let sf = CopyFamily::of_driver(&input.source_conn.driver);
    let tf = CopyFamily::of_driver(&input.target_conn.driver);
    let family = match (sf, tf) {
        (Some(a), Some(b)) if a == b => a,
        (Some(a), _) if input.source_conn.driver != input.target_conn.driver => {
            return Ok(rejected(&input, a, "FAMILY_MISMATCH", "source and target must be the same database family"));
        }
        _ => {
            return Err(CoreError::Invalid(format!(
                "copy: only postgres, mysql and sqlite connections can be copied ({} -> {})",
                input.source_conn.driver, input.target_conn.driver
            )))
        }
    };
    check_cancel(input.cancel)?;
    let collected = match family {
        CopyFamily::Sqlite => super::schema::sqlite::collect(&input).await?,
        CopyFamily::Mysql => super::schema::mysql::collect(&input).await?,
        CopyFamily::Postgres => super::schema::pg::collect(&input).await?,
    };
    check_cancel(input.cancel)?;
    let mut planned = assemble(&input, family, collected).await?;
    planned.plan.elapsed_ms = started.elapsed().as_millis() as u64;
    Ok(planned)
}

pub(crate) fn check_cancel(cancel: &CancellationToken) -> Result<()> {
    if cancel.is_cancelled() {
        Err(CoreError::Cancelled)
    } else {
        Ok(())
    }
}

fn rejected(input: &PlanInput<'_>, family: CopyFamily, code: &str, msg: &str) -> PlannedCopy {
    let mut b = super::translate::danger(code, msg);
    b.detail = Some(format!("{} -> {}", input.source_conn.driver, input.target_conn.driver));
    PlannedCopy {
        plan: CopyPlan {
            plan_id: String::new(),
            family,
            source: CopyServerFacts {
                driver: input.source_conn.driver.clone(),
                version: input.source_server.version.clone(),
                database: input.req.source.database.clone(),
                ..Default::default()
            },
            target: CopyServerFacts { driver: input.target_conn.driver.clone(), database: input.req.target.database.clone(), ..Default::default() },
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
            items: vec![],
            warnings: vec![],
            blockers: vec![b],
            acks: vec![],
            session_sql: SessionSql::default(),
            totals: CopyTotals::default(),
            resume_of: input.resume.map(|r| r.run_id.clone()),
            elapsed_ms: 0,
        },
        tables: BTreeMap::new(),
    }
}

// =============================================================================================
// Drafts and target state (the collectors' output)
// =============================================================================================

/// What a target object is looked up as.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum TClass {
    /// Tables, views, materialized views (one namespace).
    Relation,
    Type,
    Function,
    Sequence,
    Index,
    Constraint,
    Trigger,
    Policy,
    Collation,
    Extension,
    Schema,
}

/// A target object key. `table` is set for per-table objects (constraints, triggers, policies;
/// MySQL / SQLite indexes).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct TKey {
    pub class: TClass,
    pub schema: Option<String>,
    pub table: Option<String>,
    pub name: String,
}

impl TKey {
    pub fn rel(schema: Option<&str>, name: &str) -> TKey {
        TKey { class: TClass::Relation, schema: schema.map(str::to_string), table: None, name: name.to_string() }
    }
    pub fn of(class: TClass, schema: Option<&str>, table: Option<&str>, name: &str) -> TKey {
        TKey { class, schema: schema.map(str::to_string), table: table.map(str::to_string), name: name.to_string() }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct TCol {
    pub name: String,
    pub type_text: String,
    pub not_null: bool,
    pub has_default: bool,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct TObj {
    /// Catalog kind (`r`, `v`, `BASE TABLE`, `table`, ...), informational.
    pub kind: String,
    pub est_rows: Option<u64>,
    pub columns: Vec<TCol>,
    pub triggers: u32,
    pub can_insert: Option<bool>,
    pub can_truncate: Option<bool>,
    pub is_owner: Option<bool>,
    /// PG partitioned table (`TRUNCATE` without `ONLY`).
    pub partitioned: bool,
}

/// A target foreign key (child -> parent relations).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct TFk {
    pub child: TKey,
    pub name: String,
    pub parent: TKey,
}

/// What exists in the target (read by the collector through `TargetSession::query_meta`).
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct TargetState {
    pub objects: BTreeMap<TKey, TObj>,
    pub fks: Vec<TFk>,
    /// (dependant view, referenced relation).
    pub view_deps: Vec<(TKey, TKey)>,
}

/// A plan item before its action is decided.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Draft {
    pub kind: CopyObjectKind,
    pub id: String,
    pub parent: Option<String>,
    pub schema: Option<String>,
    pub name: String,
    pub target_schema: Option<String>,
    pub target_name: String,
    pub phase: CopyPhase,
    /// Statements that create the object (table: CREATE TABLE and follow-ups).
    pub create: Vec<String>,
    /// Statements that drop the existing object (drop_recreate).
    pub drop: Vec<String>,
    /// MySQL / PG: drop a foreign key of this table (`ALTER TABLE … DROP …`), by FK name.
    pub source_ddl: Option<String>,
    pub warnings: Vec<CopyIssue>,
    /// Item blockers (the item id is filled in by `assemble`).
    pub blockers: Vec<CopyIssue>,
    pub depends_on: Vec<String>,
    /// Table items: what the data path needs.
    pub data: Option<TableDataSpec>,
    /// Table items: the truncate statement.
    pub truncate: Option<String>,
    /// Table items: the load template (data elided).
    pub load_template: Option<String>,
    pub est_rows: Option<u64>,
    pub est_bytes: Option<u64>,
    pub unsupported: bool,
    /// How to look the object up in the target (`None`: never exists, e.g. comments).
    pub exists_key: Option<TKey>,
    /// `false` for tables outside a `tables` selection (dropped from the plan).
    pub selected: bool,
    /// Included only when an included item depends on it (selection `tables`).
    pub support: bool,
    /// PG extension configuration tables: `append` whatever the policy.
    pub always_append: bool,
    /// MySQL combined ALTER: (group key, clause, separate statement).
    pub group: Option<(String, String, bool)>,
    /// Finalize templates: (created by this run, floor rule).
    pub seq_templates: Option<(String, String)>,
    /// FK items: the parent table's item id.
    pub fk_parent: Option<String>,
}

impl Draft {
    pub fn new(kind: CopyObjectKind, id: String, name: &str, target_name: &str, phase: CopyPhase) -> Draft {
        Draft {
            kind,
            id,
            parent: None,
            schema: None,
            name: name.to_string(),
            target_schema: None,
            target_name: target_name.to_string(),
            phase,
            create: Vec::new(),
            drop: Vec::new(),
            source_ddl: None,
            warnings: Vec::new(),
            blockers: Vec::new(),
            depends_on: Vec::new(),
            data: None,
            truncate: None,
            load_template: None,
            est_rows: None,
            est_bytes: None,
            unsupported: false,
            exists_key: None,
            selected: true,
            support: false,
            always_append: false,
            group: None,
            seq_templates: None,
            fk_parent: None,
        }
    }
}

/// Builds `TRUNCATE ONLY a, ONLY b` from (table, partitioned) pairs.
pub(crate) type TruncateGroupSql = fn(&[(TKey, bool)]) -> String;

/// Family specifics `assemble` needs.
#[derive(Debug, Clone)]
pub(crate) struct FamilyOps {
    pub family: CopyFamily,
    /// Row presence probe of a target table (one row when it has rows; PG answers a boolean).
    pub probe_sql: fn(&TKey, bool) -> String,
    /// `SELECT COUNT(*) FROM (SELECT 1 FROM t LIMIT n) q` (resume).
    pub count_sql: fn(&TKey, u64, bool) -> String,
    /// `ALTER TABLE child DROP FOREIGN KEY|CONSTRAINT fk`.
    pub drop_fk_sql: fn(&TKey, &str) -> Option<String>,
    /// `TRUNCATE` of a group (PG): `TRUNCATE ONLY a, ONLY b`.
    pub truncate_group_sql: Option<TruncateGroupSql>,
    /// DROP statement of a target object that is not in the plan (only views on PG / MySQL
    /// are dropped this way when they are drop_recreate items themselves).
    pub quote_rel: fn(&TKey) -> String,
}

/// A collector's result.
#[derive(Debug, Clone)]
pub(crate) struct Collected {
    pub drafts: Vec<Draft>,
    pub warnings: Vec<CopyIssue>,
    pub blockers: Vec<CopyIssue>,
    pub source_facts: CopyServerFacts,
    pub target_facts: CopyServerFacts,
    pub effective: CopyEffective,
    pub session_sql: SessionSql,
    pub target: TargetState,
    pub ops: FamilyOps,
    /// A missing target database (MySQL): every object is new.
    pub target_db_missing: bool,
}

// =============================================================================================
// Shared helpers for collectors
// =============================================================================================

/// The target name of a table: `rename` (kind table), then `name_case: lower` / a target that
/// folds names (`lower`).
pub(crate) fn target_table_name(opts: &CopyOptions, schema: Option<&str>, name: &str, lower: bool) -> String {
    let mut out = name.to_string();
    for m in &opts.rename {
        if m.kind == RenameKind::Table && m.from == name && (m.schema.is_none() || m.schema.as_deref() == schema) {
            out = m.to.clone();
        }
    }
    if lower || opts.name_case == NameCase::Lower {
        out = out.to_lowercase();
    }
    out
}

/// The target schema of a PG schema: the endpoint's target schema, else `rename` (kind schema).
pub(crate) fn target_schema_name(req: &CopyRequest, schema: &str) -> String {
    if let (Some(s), Some(t)) = (&req.source.schema, &req.target.schema) {
        if s == schema {
            return t.clone();
        }
    }
    for m in &req.options.rename {
        if m.kind == RenameKind::Schema && m.from == schema {
            return m.to.clone();
        }
    }
    schema.to_string()
}

/// Whether a table is in the request's selection (`TableRef.schema` matched for PG only).
pub(crate) fn selected(req: &CopyRequest, schema: Option<&str>, name: &str) -> bool {
    match &req.selection {
        CopySelection::All => true,
        CopySelection::Tables { tables } => tables.iter().any(|t| t.name == name && (schema.is_none() || t.schema.as_deref().is_none_or(|s| Some(s) == schema))),
    }
}

/// Clamped table concurrency (1..=4; SQLite 1).
pub(crate) fn concurrency(opts: &CopyOptions, family: CopyFamily) -> u8 {
    if family == CopyFamily::Sqlite {
        1
    } else {
        opts.table_concurrency.clamp(1, 4)
    }
}

/// Session totals (4.1): source 1 + c (+1 checksum); target 2 + c (+1 checksum).
pub(crate) fn sessions(opts: &CopyOptions, c: u8) -> (u8, u8) {
    let v = u8::from(opts.verify == VerifyMode::Checksum);
    (1 + c + v, 2 + c + v)
}

pub(crate) fn meta_bool(v: Option<&str>) -> Option<bool> {
    v.map(|s| matches!(s.trim().to_ascii_lowercase().as_str(), "t" | "true" | "1" | "yes" | "on"))
}

pub(crate) fn meta_u64(v: Option<&str>) -> Option<u64> {
    v.and_then(|s| {
        let s = s.trim();
        s.parse::<u64>().ok().or_else(|| s.parse::<f64>().ok().filter(|f| *f >= 0.0).map(|f| f as u64))
    })
}

// =============================================================================================
// Assembly
// =============================================================================================

/// Codes that only matter when the item's DDL runs.
const DDL_ONLY: &[&str] = &[
    "UNPARSED_CLAUSE",
    "UNSUPPORTED_COLUMN",
    "PARTITION_UNSUPPORTED",
    "ROW_SIZE_LIMIT",
    "INNODB_ROW_SIZE",
    "ENUM_DUPLICATE",
    "TGT_REQUIRES_PK",
    "COLLATION_CHANGED",
    "COLLATION_UNIQUE_RISK",
    "TEXT_PROMOTED",
    "VARCHAR_TO_TEXT",
    "DECIMAL_WIDENED",
    "DECIMAL_PRECISION",
    "YEAR2_WIDENED",
    "DEPRECATED_SYNTAX",
    "DEFAULT_DROPPED",
    "TABLE_OPTION_DROPPED",
    "ENGINE_CHANGED",
    "ENGINE_KEPT_MYISAM",
    "AUTOINC_GROUPED",
    "ROW_SIZE_FIXED",
    "ROW_FORMAT_DYNAMIC",
    "RESERVED_WORD",
    "DOWNGRADE",
    "INDEX_PREFIX",
    "UNIQUE_PREFIX",
    "ZERO_DATE_IN_KEY",
    "PG_COLLATION_MISSING",
    "PG_TABLESPACE_IGNORED",
    "DEFINER_CHANGED",
    "DB_QUALIFIED_REFS",
    "RENAMED_REFS",
    "NAME_COLLISION",
    "FK_ON_MYISAM",
    "FK_NON_STANDARD_KEY",
    "PG17_SEARCH_PATH",
    "PG_SECDEF_OWNER",
];

fn loads(a: PlanAction) -> bool {
    matches!(a, PlanAction::Create | PlanAction::DropRecreate | PlanAction::Truncate | PlanAction::Append)
}

fn runs_ddl(a: PlanAction) -> bool {
    matches!(a, PlanAction::Create | PlanAction::DropRecreate)
}

fn kind_rank(kind: CopyObjectKind) -> u8 {
    use CopyObjectKind::*;
    match kind {
        Database => 0,
        Schema => 1,
        Extension => 2,
        Collation => 3,
        Type => 4,
        Domain => 5,
        Sequence => 6,
        Function => 7,
        Table => 8,
        Partition => 9,
        View => 10,
        MaterializedView => 11,
        TruncateGroup => 0,
        Constraint => 12,
        Index => 13,
        ForeignKey => 15,
        Trigger => 16,
        Policy => 17,
        SequenceValue | AutoIncrement => 20,
        Refresh => 21,
        Comment => 22,
        Owner => 23,
        Grant => 24,
        Analyze => 25,
    }
}

/// Everything the action pass decides for one item.
struct Decided {
    action: PlanAction,
    exists: bool,
    has_rows: Option<bool>,
    target_est: Option<u64>,
    /// Table: run the DDL (create / drop_recreate) - false for resume truncates.
    ddl: bool,
}

pub(crate) async fn assemble(input: &PlanInput<'_>, family: CopyFamily, c: Collected) -> Result<PlannedCopy> {
    let opts = &input.req.options;
    let content = opts.content;
    let Collected { drafts, mut warnings, mut blockers, source_facts, target_facts, mut effective, session_sql, target, ops, target_db_missing } = c;
    debug_assert_eq!(ops.family, family);

    // ---- selection: unselected tables leave the plan; their dependants are excluded.
    let unselected: BTreeSet<String> = drafts.iter().filter(|d| d.kind == CopyObjectKind::Table && !d.selected).map(|d| d.id.clone()).collect();
    let mut drafts: Vec<Draft> = drafts.into_iter().filter(|d| !(d.kind == CopyObjectKind::Table && !d.selected)).collect();
    // content: data_only keeps only tables and the finalize data items.
    if content == CopyContent::DataOnly {
        drafts.retain(|d| {
            matches!(d.kind, CopyObjectKind::Table | CopyObjectKind::SequenceValue | CopyObjectKind::AutoIncrement | CopyObjectKind::Analyze | CopyObjectKind::Database)
        });
    }
    if content == CopyContent::SchemaOnly {
        drafts.retain(|d| !matches!(d.kind, CopyObjectKind::SequenceValue | CopyObjectKind::AutoIncrement | CopyObjectKind::Analyze | CopyObjectKind::Refresh));
    }
    if !opts.objects.sequences {
        let had = drafts.iter().any(|d| matches!(d.kind, CopyObjectKind::SequenceValue | CopyObjectKind::AutoIncrement));
        drafts.retain(|d| !matches!(d.kind, CopyObjectKind::SequenceValue | CopyObjectKind::AutoIncrement));
        if had && content != CopyContent::SchemaOnly {
            warnings.push(warning(
                "SEQUENCES_NOT_SET",
                "sequence and AUTO_INCREMENT values are not copied: the application's next INSERT on the target collides with copied keys",
            ));
        }
    }
    // Support objects only when something included needs them. Items attached to a support
    // object (its comment, owner, NOT VALID checks, value) follow it and never pull it in.
    let index: HashMap<String, usize> = drafts.iter().enumerate().map(|(i, d)| (d.id.clone(), i)).collect();
    let mut keep = vec![true; drafts.len()];
    if matches!(input.req.selection, CopySelection::Tables { .. }) {
        let support_parent = |i: usize| drafts[i].parent.as_ref().and_then(|p| index.get(p)).copied().filter(|&j| drafts[j].support);
        let mut needed: BTreeSet<usize> = BTreeSet::new();
        let mut stack: Vec<usize> = (0..drafts.len()).filter(|&i| !drafts[i].support && support_parent(i).is_none()).collect();
        while let Some(i) = stack.pop() {
            for dep in &drafts[i].depends_on {
                if let Some(&j) = index.get(dep) {
                    if drafts[j].support && needed.insert(j) {
                        stack.push(j);
                    }
                }
            }
        }
        for (i, d) in drafts.iter().enumerate() {
            if d.support && !needed.contains(&i) {
                keep[i] = false;
            }
        }
        for i in 0..drafts.len() {
            if let Some(j) = support_parent(i) {
                if !keep[j] {
                    keep[i] = false;
                }
            }
        }
    }
    let mut drafts: Vec<Draft> = drafts.into_iter().zip(keep).filter(|(_, k)| *k).map(|(d, _)| d).collect();
    let index: HashMap<String, usize> = drafts.iter().enumerate().map(|(i, d)| (d.id.clone(), i)).collect();

    // ---- name collisions among relations (target names are already folded where the target folds)
    let mut seen_names: HashMap<(Option<String>, String), String> = HashMap::new();
    for d in &drafts {
        if matches!(d.kind, CopyObjectKind::Table | CopyObjectKind::View | CopyObjectKind::MaterializedView | CopyObjectKind::Partition) {
            let key = (d.target_schema.clone(), d.target_name.clone());
            if let Some(other) = seen_names.get(&key) {
                blockers.push(
                    super::translate::danger("NAME_COLLISION", format!("two source objects get the target name {}", d.target_name))
                        .detail(format!("{other} and {}", d.id))
                        .rec("rename one of them or exclude it"),
                );
            } else {
                seen_names.insert(key, d.id.clone());
            }
        }
    }

    // ---- exclusions (options.exclude, dependants of unselected / excluded / unsupported items)
    let n = drafts.len();
    let mut dependants: Vec<Vec<usize>> = vec![Vec::new(); n];
    for (i, d) in drafts.iter().enumerate() {
        let mut deps: Vec<&String> = d.depends_on.iter().collect();
        if let Some(p) = &d.parent {
            deps.push(p);
        }
        for dep in deps {
            if let Some(&j) = index.get(dep) {
                if j != i {
                    dependants[j].push(i);
                }
            }
        }
    }
    let mut excluded = vec![false; n];
    let mut not_selected = vec![false; n];
    let mut dep_unsupported = vec![false; n];
    for (i, d) in drafts.iter().enumerate() {
        if d.depends_on.iter().chain(d.parent.iter()).any(|x| unselected.contains(x)) {
            not_selected[i] = true;
        }
        if opts.exclude.contains(&d.id) {
            excluded[i] = true;
        }
    }
    // Propagate.
    let mut stack: Vec<usize> = (0..n).filter(|&i| excluded[i] || not_selected[i]).collect();
    while let Some(i) = stack.pop() {
        for &j in &dependants[i] {
            if !excluded[j] && !not_selected[j] {
                if excluded[i] {
                    excluded[j] = true;
                } else {
                    not_selected[j] = true;
                }
                stack.push(j);
            }
        }
    }
    let mut stack: Vec<usize> = (0..n).filter(|&i| drafts[i].unsupported).collect();
    while let Some(i) = stack.pop() {
        for &j in &dependants[i] {
            if !drafts[j].unsupported && !dep_unsupported[j] {
                dep_unsupported[j] = true;
                stack.push(j);
            }
        }
    }

    // ---- actions
    let policy_of = |id: &str| -> ExistingTablePolicy { opts.existing_overrides.iter().find(|o| o.item_id == id).map(|o| o.policy).unwrap_or(opts.existing) };
    let mut decided: Vec<Decided> = Vec::with_capacity(n);
    for (i, d) in drafts.iter().enumerate() {
        let tobj = if target_db_missing { None } else { d.exists_key.as_ref().and_then(|k| target.objects.get(k)) };
        let exists = tobj.is_some();
        let mut dd = Decided { action: PlanAction::Create, exists, has_rows: None, target_est: tobj.and_then(|o| o.est_rows), ddl: true };
        if excluded[i] || not_selected[i] {
            dd.action = PlanAction::Excluded;
            dd.ddl = false;
        } else if d.unsupported || dep_unsupported[i] {
            dd.action = PlanAction::Unsupported;
            dd.ddl = false;
        } else {
            match d.kind {
                CopyObjectKind::Table => {
                    let mut policy = policy_of(&d.id);
                    if content == CopyContent::DataOnly && policy == ExistingTablePolicy::DropRecreate {
                        policy = ExistingTablePolicy::Truncate;
                    }
                    dd.action = if d.always_append {
                        PlanAction::Append
                    } else if !exists {
                        PlanAction::Create
                    } else {
                        match policy {
                            ExistingTablePolicy::Skip => PlanAction::SkipExisting,
                            ExistingTablePolicy::Truncate => PlanAction::Truncate,
                            ExistingTablePolicy::DropRecreate => PlanAction::DropRecreate,
                            ExistingTablePolicy::Append => PlanAction::Append,
                        }
                    };
                    dd.ddl = runs_ddl(dd.action) && !d.always_append;
                }
                CopyObjectKind::Database | CopyObjectKind::Schema | CopyObjectKind::Extension => {
                    dd.action = if exists { PlanAction::SkipExisting } else { PlanAction::Create };
                }
                CopyObjectKind::Comment | CopyObjectKind::Owner | CopyObjectKind::Grant | CopyObjectKind::Refresh | CopyObjectKind::Analyze => {
                    dd.action = PlanAction::Run;
                }
                CopyObjectKind::SequenceValue | CopyObjectKind::AutoIncrement => dd.action = PlanAction::Run,
                CopyObjectKind::TruncateGroup => dd.action = PlanAction::Run,
                _ => {
                    dd.action = if !exists {
                        PlanAction::Create
                    } else if policy_of(&d.id) == ExistingTablePolicy::DropRecreate && !d.drop.is_empty() {
                        PlanAction::DropRecreate
                    } else {
                        PlanAction::SkipExisting
                    };
                }
            }
        }
        decided.push(dd);
    }
    // Children follow their table (parent) and finalize items their target.
    let parent_action = |decided: &[Decided], d: &Draft| -> Option<PlanAction> { d.parent.as_ref().and_then(|p| index.get(p)).map(|&j| decided[j].action) };
    for i in 0..n {
        if matches!(decided[i].action, PlanAction::Excluded | PlanAction::Unsupported) {
            continue;
        }
        let d = &drafts[i];
        let Some(pa) = parent_action(&decided, d) else { continue };
        let a = match d.kind {
            CopyObjectKind::Index
            | CopyObjectKind::Constraint
            | CopyObjectKind::ForeignKey
            | CopyObjectKind::Trigger
            | CopyObjectKind::Policy
            | CopyObjectKind::Partition => match pa {
                PlanAction::Create | PlanAction::DropRecreate => PlanAction::Create,
                PlanAction::Excluded => PlanAction::Excluded,
                PlanAction::Unsupported => PlanAction::Unsupported,
                PlanAction::ResumeDone => {
                    if decided[i].exists {
                        PlanAction::ResumeDone
                    } else {
                        PlanAction::Create
                    }
                }
                _ => PlanAction::SkipExisting,
            },
            CopyObjectKind::Comment | CopyObjectKind::Owner | CopyObjectKind::Grant | CopyObjectKind::Refresh => match pa {
                PlanAction::Create | PlanAction::DropRecreate => PlanAction::Run,
                PlanAction::Excluded => PlanAction::Excluded,
                PlanAction::Unsupported => PlanAction::Unsupported,
                _ => PlanAction::SkipExisting,
            },
            CopyObjectKind::SequenceValue | CopyObjectKind::AutoIncrement | CopyObjectKind::Analyze => match pa {
                // A standalone sequence: its value is set whether it was created or kept (floor rule).
                PlanAction::Create | PlanAction::SkipExisting | PlanAction::DropRecreate | PlanAction::ResumeDone
                    if d.parent.as_ref().and_then(|p| index.get(p)).is_some_and(|&j| drafts[j].kind == CopyObjectKind::Sequence) =>
                {
                    PlanAction::Run
                }
                a if loads(a) => PlanAction::Run,
                PlanAction::Excluded => PlanAction::Excluded,
                PlanAction::Unsupported => PlanAction::Unsupported,
                _ => PlanAction::SkipExisting,
            },
            _ => continue,
        };
        decided[i].action = a;
        decided[i].ddl = runs_ddl(a);
    }

    // ---- resume mapping (5.5)
    let mut resume_count_probe: Vec<(usize, u64)> = Vec::new();
    // Truncates the mapping already explained (DESTRUCTIVE_TRUNCATE or RESUME_RELOAD); every
    // other truncate of a table with rows gets DESTRUCTIVE_TRUNCATE below, also on a resume.
    let mut resume_truncate_noted: BTreeSet<usize> = BTreeSet::new();
    if let Some(rs) = input.resume {
        // Items an earlier job built on (a dependant of theirs got past `pending` / `skipped`):
        // their own DDL had completed.
        let built_on: BTreeSet<String> = drafts
            .iter()
            .filter(|k| rs.items.get(&k.id).is_some_and(|s| !matches!(s.0, CheckpointState::Pending | CheckpointState::Skipped)))
            .flat_map(|k| k.parent.iter().chain(k.depends_on.iter()).cloned())
            .collect();
        for i in 0..n {
            if matches!(decided[i].action, PlanAction::Excluded | PlanAction::Unsupported) {
                continue;
            }
            let d = &drafts[i];
            let Some((state, created_by_run, rows)) = rs.items.get(&d.id).copied() else { continue };
            let exists = decided[i].exists;
            if d.kind == CopyObjectKind::Table {
                let policy = policy_of(&d.id);
                // The run's own table whose CREATE (with its follow-ups: ALTER SEQUENCE … OWNED
                // BY, SET COMPRESSION, REPLICA IDENTITY, MySQL engine checks) may not have
                // completed: still `creating`, `failed` before any row (a pre_data failure
                // checkpoints `failed` too), or `skipped` by a failed dependency. Nothing was built
                // on it, so it is dropped and created again rather than loaded half-built.
                let rebuild = created_by_run
                    && exists
                    && content != CopyContent::DataOnly
                    && !d.drop.is_empty()
                    && !built_on.contains(&d.id)
                    && match state {
                        CheckpointState::Creating => true,
                        CheckpointState::Failed | CheckpointState::Skipped => rows == 0,
                        _ => false,
                    };
                if rebuild {
                    decided[i].action = PlanAction::DropRecreate;
                    decided[i].ddl = true;
                    drafts[i].warnings.push(info("RESUME_RELOAD", "the earlier job did not finish creating this table: it is dropped, created again and loaded"));
                    continue;
                }
                match state {
                    CheckpointState::Skipped if exists && created_by_run => {
                        // Created by this run, then skipped because a dependency failed: never loaded.
                        decided[i].action = PlanAction::Truncate;
                        decided[i].ddl = false;
                    }
                    CheckpointState::Skipped if exists && loads(decided[i].action) => {
                        // A load the earlier job skipped because a dependency failed: it never
                        // ran, so the fresh action stands (with its own danger issues).
                    }
                    CheckpointState::Done | CheckpointState::Skipped => {
                        if exists || decided[i].action == PlanAction::SkipExisting {
                            decided[i].action = PlanAction::ResumeDone;
                            decided[i].ddl = false;
                            drafts[i].warnings.push(info("RESUME_DONE", "finished in an earlier job of this run"));
                        } else {
                            decided[i].action = PlanAction::Create;
                            decided[i].ddl = true;
                            drafts[i]
                                .warnings
                                .push(warning("RESUME_TARGET_CHANGED", "the table finished in an earlier job is gone from the target: it is created and loaded again"));
                        }
                    }
                    CheckpointState::Creating | CheckpointState::Created => {
                        if exists {
                            decided[i].action = PlanAction::Truncate;
                            decided[i].ddl = false;
                        } else {
                            decided[i].action = PlanAction::Create;
                            decided[i].ddl = true;
                        }
                    }
                    CheckpointState::Loading | CheckpointState::Failed => {
                        if !exists {
                            decided[i].action = PlanAction::Create;
                            decided[i].ddl = true;
                        } else if created_by_run || matches!(policy, ExistingTablePolicy::Truncate | ExistingTablePolicy::DropRecreate) {
                            decided[i].action = PlanAction::Truncate;
                            decided[i].ddl = false;
                            resume_count_probe.push((i, rows));
                            resume_truncate_noted.insert(i);
                        } else if rows == 0 {
                            // The interrupted append committed nothing. The checkpoint stores
                            // committed rows (0 for a rolled-back single-transaction PG load;
                            // MySQL/SQLite report 0 when no chunk committed), so a plain re-append
                            // is exact and the user is spared the destructive truncate/skip choice.
                            decided[i].action = PlanAction::Append;
                            decided[i].ddl = false;
                            drafts[i].warnings.push(info("RESUME_APPEND_EMPTY", "the interrupted append had committed no rows; it is appended again"));
                        } else {
                            match rs.partial {
                                ResumePartial::Fail => {
                                    drafts[i].blockers.push(
                                        super::translate::danger("RESUME_APPEND_PARTIAL", "rows of an interrupted append are already in the table")
                                            .rec("resume with partial: truncate (reload) or skip (leave as is)"),
                                    );
                                    decided[i].action = PlanAction::Append;
                                    decided[i].ddl = false;
                                }
                                ResumePartial::Truncate => {
                                    decided[i].action = PlanAction::Truncate;
                                    decided[i].ddl = false;
                                    resume_truncate_noted.insert(i);
                                    drafts[i].warnings.push(super::translate::danger(
                                        "DESTRUCTIVE_TRUNCATE",
                                        "the table is emptied before it is loaded again, including rows that were there before the copy",
                                    ));
                                }
                                ResumePartial::Skip => {
                                    decided[i].action = PlanAction::ResumeDone;
                                    decided[i].ddl = false;
                                    drafts[i].warnings.push(warning("RESUME_APPEND_PARTIAL", "the interrupted append is left as is"));
                                }
                            }
                        }
                    }
                    CheckpointState::Loaded => {
                        decided[i].action = PlanAction::ResumeDone;
                        decided[i].ddl = false;
                    }
                    CheckpointState::Pending => {}
                }
            } else if matches!(state, CheckpointState::Done | CheckpointState::Skipped) {
                let probeable = d.exists_key.is_some();
                if !probeable || exists {
                    decided[i].action = PlanAction::ResumeDone;
                    decided[i].ddl = false;
                }
            } else if exists
                && matches!(d.kind, CopyObjectKind::Index | CopyObjectKind::Constraint | CopyObjectKind::ForeignKey | CopyObjectKind::Trigger | CopyObjectKind::Policy)
            {
                decided[i].action = PlanAction::ResumeDone;
                decided[i].ddl = false;
            }
        }
        // Children follow their resumed parent. The follow pass above saw the fresh actions,
        // where every object the run created already exists and is skipped by default.
        for i in 0..n {
            if matches!(decided[i].action, PlanAction::Excluded | PlanAction::Unsupported) {
                continue;
            }
            let d = &drafts[i];
            let Some(&j) = d.parent.as_ref().and_then(|p| index.get(p)) else { continue };
            let Some((pstate, p_created_by_run, _)) = rs.items.get(&drafts[j].id).copied() else { continue };
            let pa = decided[j].action;
            let index_like =
                matches!(d.kind, CopyObjectKind::Index | CopyObjectKind::Constraint | CopyObjectKind::ForeignKey | CopyObjectKind::Trigger | CopyObjectKind::Policy);
            let finalize = matches!(
                d.kind,
                CopyObjectKind::Comment
                    | CopyObjectKind::Owner
                    | CopyObjectKind::Grant
                    | CopyObjectKind::Refresh
                    | CopyObjectKind::SequenceValue
                    | CopyObjectKind::AutoIncrement
                    | CopyObjectKind::Analyze
            );
            if runs_ddl(pa) && decided[j].ddl {
                // Created (again) by this job: everything on it is new, whatever its checkpoint.
                let a = if index_like || d.kind == CopyObjectKind::Partition {
                    PlanAction::Create
                } else if finalize {
                    PlanAction::Run
                } else {
                    continue;
                };
                decided[i].action = a;
                decided[i].ddl = runs_ddl(a);
            } else if index_like {
                // A table reloaded without DDL, or done: its post-data items run when missing.
                let table_resumed =
                    drafts[j].kind == CopyObjectKind::Table && ((!matches!(pstate, CheckpointState::Pending) && loads(pa)) || pa == PlanAction::ResumeDone);
                if table_resumed && decided[i].action != PlanAction::ResumeDone {
                    decided[i].action = if decided[i].exists { PlanAction::ResumeDone } else { PlanAction::Create };
                    decided[i].ddl = decided[i].action == PlanAction::Create;
                }
            } else if finalize && rs.items.get(&d.id).map(|s| s.0) != Some(CheckpointState::Done) {
                // Finalize items are not checkpointed when cancelled or skipped by a dependency.
                // Sequence values / AUTO_INCREMENT (floor rule: they never move backwards) and
                // ANALYZE follow the data: a table this job loads, or one an earlier job loaded.
                // Comments, owner, grants and refresh follow an object this run created.
                let run = match d.kind {
                    CopyObjectKind::SequenceValue | CopyObjectKind::AutoIncrement | CopyObjectKind::Analyze => {
                        drafts[j].kind == CopyObjectKind::Table
                            && (loads(pa)
                                || (pa == PlanAction::ResumeDone
                                    && matches!(pstate, CheckpointState::Done | CheckpointState::Loaded | CheckpointState::Loading | CheckpointState::Failed)))
                    }
                    _ => p_created_by_run && decided[j].exists,
                };
                if run {
                    decided[i].action = PlanAction::Run;
                    decided[i].ddl = false;
                }
            }
        }
        for (i, rows) in resume_count_probe {
            let Some(key) = drafts[i].exists_key.clone() else { continue };
            let m = input.target.query_meta(&(ops.count_sql)(&key, rows + 1, target.objects.get(&key).is_some_and(|o| o.partitioned))).await?;
            let count = m.rows.first().and_then(|r| r.first()).and_then(|v| meta_u64(v.as_deref())).unwrap_or(0);
            if count > rows {
                drafts[i].warnings.push(
                    super::translate::danger(
                        "DESTRUCTIVE_TRUNCATE",
                        "the table holds more rows than this run wrote (an application wrote meanwhile); they are deleted by the reload",
                    )
                    .detail(format!("this run wrote {rows} rows")),
                );
            } else {
                drafts[i].warnings.push(info("RESUME_RELOAD", "the interrupted table is emptied and loaded again"));
            }
        }
    }

    // ---- data_only: a missing table cannot be created
    if content == CopyContent::DataOnly {
        for i in 0..n {
            if drafts[i].kind == CopyObjectKind::Table && decided[i].action == PlanAction::Create {
                drafts[i].blockers.push(
                    super::translate::danger("TABLE_MISSING", "content is data only and the table does not exist in the target")
                        .rec("copy schema and data, or exclude the table"),
                );
                decided[i].ddl = false;
            }
        }
        let loaded = (0..n).filter(|&i| drafts[i].kind == CopyObjectKind::Table && loads(decided[i].action)).count();
        let skipped = (0..n).filter(|&i| drafts[i].kind == CopyObjectKind::Table && decided[i].action == PlanAction::SkipExisting).count();
        if loaded == 0 && skipped > 0 {
            warnings
                .push(warning("DATA_ONLY_ALL_SKIPPED", "content is data only and every selected table is skipped because it exists").rec("choose truncate or append"));
        }
    }

    // ---- row presence probes (exact, cheap)
    for i in 0..n {
        let d = &drafts[i];
        if d.kind != CopyObjectKind::Table || !decided[i].exists {
            continue;
        }
        if !matches!(decided[i].action, PlanAction::SkipExisting | PlanAction::Truncate | PlanAction::DropRecreate | PlanAction::Append | PlanAction::ResumeDone) {
            continue;
        }
        check_cancel(input.cancel)?;
        let Some(key) = d.exists_key.clone() else { continue };
        let m = input.target.query_meta(&(ops.probe_sql)(&key, target.objects.get(&key).is_some_and(|o| o.partitioned))).await?;
        let has = match m.rows.first() {
            None => false,
            Some(r) => match r.first().and_then(|v| v.as_deref()) {
                Some(v) if family == CopyFamily::Postgres => meta_bool(Some(v)).unwrap_or(true),
                _ => true,
            },
        };
        decided[i].has_rows = Some(has);
    }

    // ---- per-table issues of the actions
    let mut skipped_tables = 0usize;
    let mut skipped_empty = 0usize;
    for i in 0..n {
        let a = decided[i].action;
        let d = &mut drafts[i];
        if d.kind != CopyObjectKind::Table {
            continue;
        }
        let tobj = d.exists_key.as_ref().and_then(|k| target.objects.get(k));
        match a {
            PlanAction::Truncate if decided[i].has_rows == Some(true) && !resume_truncate_noted.contains(&i) => {
                d.warnings.push(super::translate::danger("DESTRUCTIVE_TRUNCATE", "the existing table holds rows; they are deleted before the load"));
            }
            PlanAction::DropRecreate if decided[i].has_rows == Some(true) => {
                d.warnings.push(super::translate::danger("DESTRUCTIVE_DROP", "the existing table holds rows; it is dropped and created again"));
            }
            PlanAction::SkipExisting => {
                skipped_tables += 1;
                if decided[i].has_rows == Some(false) {
                    skipped_empty += 1;
                }
                // SKIPPED_DIFFERS: different columns, or an empty target for a non-empty source.
                if let (Some(t), Some(spec)) = (tobj, &d.data) {
                    let tnames: Vec<String> = t.columns.iter().map(|c| c.name.to_lowercase()).collect();
                    let snames: Vec<String> = spec.columns.iter().map(|c| c.target.to_lowercase()).collect();
                    let differs = !t.columns.is_empty() && snames.iter().any(|s| !tnames.contains(s));
                    let empty = decided[i].has_rows == Some(false) && d.est_rows.unwrap_or(0) > 0;
                    if differs || empty {
                        let why = if differs { "its columns differ from the source's" } else { "it is empty while the source has rows" };
                        d.warnings.push(warning("SKIPPED_DIFFERS", format!("the skipped target table differs: {why}")));
                    }
                }
            }
            PlanAction::Append => {
                if let (Some(t), Some(spec)) = (tobj, &d.data) {
                    if !t.columns.is_empty() {
                        let tnames: Vec<String> = t.columns.iter().map(|c| c.name.to_lowercase()).collect();
                        let snames: Vec<String> = spec.columns.iter().map(|c| c.target.to_lowercase()).collect();
                        let missing_in_target: Vec<&String> = snames.iter().filter(|s| !tnames.contains(s)).collect();
                        let required: Vec<&TCol> = t.columns.iter().filter(|c| c.not_null && !c.has_default && !snames.contains(&c.name.to_lowercase())).collect();
                        if !missing_in_target.is_empty() || !required.is_empty() {
                            let mut det = Vec::new();
                            if !missing_in_target.is_empty() {
                                det.push(format!("not in the target: {}", missing_in_target.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")));
                            }
                            if !required.is_empty() {
                                det.push(format!(
                                    "NOT NULL without default and not in the source: {}",
                                    required.iter().map(|c| c.name.as_str()).collect::<Vec<_>>().join(", ")
                                ));
                            }
                            d.blockers.push(
                                super::translate::danger("APPEND_COLUMNS_MISMATCH", "the source columns do not fit the existing target table")
                                    .detail(det.join("; "))
                                    .rec("choose drop_recreate or exclude the table"),
                            );
                        }
                    }
                }
            }
            _ => {}
        }
        if matches!(a, PlanAction::Truncate | PlanAction::Append) && tobj.is_some_and(|t| t.triggers > 0) {
            d.warnings.push(warning("TARGET_TRIGGERS", "the target table has triggers; they fire during the load"));
        }
        // Privileges (PG; the values are unknown elsewhere).
        if let Some(t) = tobj {
            let missing = match a {
                PlanAction::Append => t.can_insert == Some(false),
                PlanAction::Truncate => t.can_insert == Some(false) || t.can_truncate == Some(false),
                PlanAction::DropRecreate => t.is_owner == Some(false),
                _ => false,
            };
            if missing {
                d.blockers.push(
                    super::translate::danger("TGT_PRIVILEGE", "the target account lacks the privilege this action needs on the table").detail(format!("action {a:?}")),
                );
            }
        }
    }
    if skipped_tables > 0 {
        warnings.push(
            warning("TABLES_SKIPPED_EXISTING", format!("{skipped_tables} table(s) already exist in the target and are skipped (no DDL, no data)"))
                .detail(format!("{skipped_tables} skipped, {skipped_empty} of them empty"))
                .rec("empty tables: append or data only / 빈 테이블은 이어 붙이기 또는 데이터만"),
        );
    }

    // ---- FK to skipped parents
    for i in 0..n {
        if drafts[i].kind != CopyObjectKind::ForeignKey || decided[i].action != PlanAction::Create {
            continue;
        }
        if let Some(&j) = drafts[i].fk_parent.as_ref().and_then(|p| index.get(p)) {
            if decided[j].action == PlanAction::SkipExisting {
                drafts[i].warnings.push(warning("FK_TO_SKIPPED", "the foreign key references a table that is skipped; validation may fail"));
            }
        }
    }

    // ---- drop dependants (drop_recreate)
    let item_by_key: HashMap<TKey, usize> = (0..n).filter_map(|i| drafts[i].exists_key.clone().map(|k| (k, i))).collect();
    let mut drop_deps: Vec<Vec<DropDep>> = vec![Vec::new(); n];
    for i in 0..n {
        if decided[i].action != PlanAction::DropRecreate {
            continue;
        }
        let Some(key) = drafts[i].exists_key.clone() else { continue };
        let mut blocking: Vec<String> = Vec::new();
        for (view, rel) in &target.view_deps {
            if *rel != key || *view == key {
                continue;
            }
            match item_by_key.get(view) {
                Some(&j) if decided[j].action == PlanAction::DropRecreate => drop_deps[i].push(DropDep::Item(j)),
                _ => blocking.push(format!("view {}", (ops.quote_rel)(view))),
            }
        }
        for fk in &target.fks {
            if fk.parent != key || fk.child == key {
                continue;
            }
            match item_by_key.get(&fk.child) {
                Some(&j) if decided[j].action == PlanAction::DropRecreate => drop_deps[i].push(DropDep::Fk(j, fk.child.clone(), fk.name.clone())),
                _ => blocking.push(format!("foreign key {} of {}", fk.name, (ops.quote_rel)(&fk.child))),
            }
        }
        if !blocking.is_empty() {
            drafts[i].blockers.push(
                super::translate::danger("DROP_HAS_DEPENDANTS", "other target objects depend on the object that would be dropped")
                    .detail(blocking.join(", "))
                    .rec("exclude the item or choose another policy"),
            );
        }
    }

    // ---- truncate groups (PG) and load order for existing tables
    let mut new_items: Vec<Draft> = Vec::new();
    let mut new_decided: Vec<Decided> = Vec::new();
    let mut in_group: BTreeSet<usize> = BTreeSet::new();
    let truncating: BTreeSet<usize> = (0..n).filter(|&i| drafts[i].kind == CopyObjectKind::Table && decided[i].action == PlanAction::Truncate).collect();
    if let Some(group_sql) = ops.truncate_group_sql {
        // Union-find over FK edges among truncate tables.
        let mut parent: HashMap<usize, usize> = truncating.iter().map(|&i| (i, i)).collect();
        fn find(p: &mut HashMap<usize, usize>, x: usize) -> usize {
            let mut r = x;
            while p[&r] != r {
                r = p[&r];
            }
            p.insert(x, r);
            r
        }
        let mut grouped: BTreeSet<usize> = BTreeSet::new();
        for fk in &target.fks {
            let (Some(&c), Some(&p)) = (item_by_key.get(&fk.child), item_by_key.get(&fk.parent)) else {
                // A referencing table outside the plan: the parent cannot be truncated.
                if let Some(&p) = item_by_key.get(&fk.parent) {
                    if truncating.contains(&p) && fk.child != fk.parent {
                        drafts[p].blockers.push(
                            super::translate::danger("TRUNCATE_FK_REFERENCED", "a target table outside the truncate group references this table").detail(format!(
                                "{} ({})",
                                (ops.quote_rel)(&fk.child),
                                fk.name
                            )),
                        );
                    }
                }
                continue;
            };
            if c == p {
                continue;
            }
            match (truncating.contains(&c), truncating.contains(&p)) {
                (true, true) => {
                    let (a, b) = (find(&mut parent, c), find(&mut parent, p));
                    if a != b {
                        parent.insert(a, b);
                    }
                    grouped.insert(c);
                    grouped.insert(p);
                }
                (false, true) => {
                    drafts[p].blockers.push(
                        super::translate::danger("TRUNCATE_FK_REFERENCED", "a target table outside the truncate group references this table").detail(format!(
                            "{} ({})",
                            (ops.quote_rel)(&fk.child),
                            fk.name
                        )),
                    );
                }
                _ => {}
            }
        }
        let mut groups: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
        for &i in &grouped {
            let r = find(&mut parent, i);
            groups.entry(r).or_default().push(i);
        }
        for members in groups.values() {
            let mut members = members.clone();
            members.sort_by(|a, b| drafts[*a].id.cmp(&drafts[*b].id));
            let first = &drafts[members[0]];
            let path = first.id.split_once(':').map(|(_, p)| p).unwrap_or("");
            let id = format!("truncate_group:{path}");
            let mut g = Draft::new(CopyObjectKind::TruncateGroup, id.clone(), &first.name, &first.target_name, CopyPhase::Data);
            g.schema = first.schema.clone();
            g.target_schema = first.target_schema.clone();
            let keys: Vec<(TKey, bool)> =
                members.iter().filter_map(|&m| drafts[m].exists_key.clone().map(|k| (k.clone(), target.objects.get(&k).is_some_and(|o| o.partitioned)))).collect();
            g.create = vec![group_sql(&keys)];
            g.warnings.push(
                info("TRUNCATE_GROUPED", "tables linked by foreign keys are emptied together, before their loads (not atomic with the loads)")
                    .detail(members.iter().map(|&m| drafts[m].target_name.clone()).collect::<Vec<_>>().join(", ")),
            );
            for &m in &members {
                drafts[m].depends_on.push(id.clone());
                in_group.insert(m);
            }
            new_items.push(g);
            new_decided.push(Decided { action: PlanAction::Run, exists: false, has_rows: None, target_est: None, ddl: true });
        }
        for &i in &truncating {
            if !in_group.contains(&i) && decided[i].has_rows == Some(true) {
                drafts[i].warnings.push(warning(
                    "TARGET_LOCKED_DURING_LOAD",
                    "TRUNCATE inside the load transaction locks the table until the load commits, blocking every application query on it",
                ));
            }
        }
    } else if ops.family == CopyFamily::Mysql {
        // MySQL has no truncate group: `TRUNCATE TABLE` runs in pre_load under
        // `foreign_key_checks = 0`, so it never raises error 1701 and commits on its own. A
        // target table that this run does not reload (skip_existing, append, excluded, not
        // selected, or in another database) keeps rows that can be left pointing at parent rows
        // the reload removes. The 5.6 orphan probes cover only the FKs this run adds, not these
        // pre-existing ones, so warn (danger, acknowledged) per truncated table they reference.
        for &i in &truncating {
            let Some(key) = drafts[i].exists_key.clone() else { continue };
            let mut referencing: Vec<String> = Vec::new();
            for fk in &target.fks {
                if fk.parent != key || fk.child == key {
                    continue;
                }
                // Safe only when the referencing table is itself fully reloaded from the source.
                let reloaded = item_by_key.get(&fk.child).is_some_and(|&j| matches!(decided[j].action, PlanAction::Truncate | PlanAction::DropRecreate));
                if !reloaded {
                    referencing.push(format!("{} ({})", (ops.quote_rel)(&fk.child), fk.name));
                }
            }
            if !referencing.is_empty() {
                referencing.sort();
                referencing.dedup();
                drafts[i].warnings.push(
                    super::translate::danger(
                        "TRUNCATE_FK_REFERENCED",
                        "a target table that this run does not reload references this table; after it is emptied and loaded again, those rows can point at rows the reload removed",
                    )
                    .detail(referencing.join(", "))
                    .rec("exclude this table, choose append or skip, or reload the referencing table in the same run"),
                );
            }
        }
    }
    // Existing tables with target FKs load parents first (and keep FK checks on MySQL).
    let mut fk_checks: BTreeSet<usize> = BTreeSet::new();
    {
        let existing_loading: BTreeSet<usize> =
            (0..n).filter(|&i| drafts[i].kind == CopyObjectKind::Table && matches!(decided[i].action, PlanAction::Truncate | PlanAction::Append)).collect();
        let mut edges: Vec<(usize, usize)> = Vec::new();
        for fk in &target.fks {
            let (Some(&c), Some(&p)) = (item_by_key.get(&fk.child), item_by_key.get(&fk.parent)) else { continue };
            if c == p || !existing_loading.contains(&c) || !loads(decided[p].action) {
                continue;
            }
            edges.push((c, p));
            fk_checks.insert(c);
        }
        // Cycles: tables on a cycle load without FK checks, in any order.
        let mut cyclic: BTreeSet<usize> = BTreeSet::new();
        for &(c, _) in &edges {
            let mut seen = BTreeSet::new();
            let mut stack = vec![c];
            while let Some(x) = stack.pop() {
                for &(a, b) in &edges {
                    if a == x && seen.insert(b) {
                        if b == c {
                            cyclic.insert(c);
                        }
                        stack.push(b);
                    }
                }
            }
        }
        if !cyclic.is_empty() {
            warnings.push(
                warning("FK_CYCLE", "existing target tables reference each other in a cycle; they load without foreign key checks and their keys are checked afterwards")
                    .detail(cyclic.iter().map(|&i| drafts[i].target_name.clone()).collect::<Vec<_>>().join(", "))
                    .rec("drop_recreate"),
            );
        }
        for (c, p) in edges {
            if cyclic.contains(&c) {
                fk_checks.remove(&c);
                continue;
            }
            let pid = drafts[p].id.clone();
            if !drafts[c].depends_on.contains(&pid) {
                drafts[c].depends_on.push(pid);
            }
        }
    }
    drafts.extend(new_items);
    decided.extend(new_decided);
    let n = drafts.len();
    let index: HashMap<String, usize> = drafts.iter().enumerate().map(|(i, d)| (d.id.clone(), i)).collect();

    // ---- phases and order
    let mut phase: Vec<CopyPhase> = drafts.iter().map(|d| d.phase).collect();
    for i in 0..n {
        if drafts[i].kind == CopyObjectKind::Table {
            phase[i] = if decided[i].ddl && runs_ddl(decided[i].action) {
                CopyPhase::PreData
            } else if loads(decided[i].action) {
                CopyPhase::Data
            } else {
                CopyPhase::PreData
            };
        }
    }
    let mut order = vec![0u32; n];
    for ph in [CopyPhase::PreData, CopyPhase::Data, CopyPhase::PostData, CopyPhase::Finalize, CopyPhase::Verify, CopyPhase::Plan] {
        let members: Vec<usize> = (0..n).filter(|&i| phase[i] == ph).collect();
        let set: BTreeSet<usize> = members.iter().copied().collect();
        let mut indeg: HashMap<usize, usize> = members.iter().map(|&i| (i, 0)).collect();
        let mut outs: HashMap<usize, Vec<usize>> = HashMap::new();
        for &i in &members {
            for dep in drafts[i].depends_on.iter().chain(drafts[i].parent.iter()) {
                if let Some(&j) = index.get(dep) {
                    if set.contains(&j) && j != i {
                        *indeg.get_mut(&i).unwrap() += 1;
                        outs.entry(j).or_default().push(i);
                    }
                }
            }
        }
        let mut ready: BTreeSet<(u8, usize)> = members.iter().filter(|i| indeg[i] == 0).map(|&i| (kind_rank(drafts[i].kind), i)).collect();
        let mut pos = 0u32;
        let mut placed = BTreeSet::new();
        while let Some(&(r, i)) = ready.iter().next() {
            ready.remove(&(r, i));
            order[i] = pos;
            pos += 1;
            placed.insert(i);
            if let Some(next) = outs.get(&i) {
                for &j in next {
                    let e = indeg.get_mut(&j).unwrap();
                    *e -= 1;
                    if *e == 0 {
                        ready.insert((kind_rank(drafts[j].kind), j));
                    }
                }
            }
        }
        // Cycles: the rest in rank order.
        let mut rest: Vec<usize> = members.iter().copied().filter(|i| !placed.contains(i)).collect();
        rest.sort_by_key(|&i| (kind_rank(drafts[i].kind), i));
        for i in rest {
            order[i] = pos;
            pos += 1;
        }
    }

    // ---- compose statements
    let mut sql: Vec<Vec<String>> = vec![Vec::new(); n];
    // Drops, walking pre_data in order (dependants dropped first, inside the first item that needs it).
    let mut pre: Vec<usize> = (0..n).filter(|&i| phase[i] == CopyPhase::PreData).collect();
    pre.sort_by_key(|&i| order[i]);
    let mut dropped: BTreeSet<usize> = BTreeSet::new();
    let mut fk_dropped: BTreeSet<(TKey, String)> = BTreeSet::new();
    #[allow(clippy::too_many_arguments)]
    fn emit_drops(
        i: usize,
        drafts: &[Draft],
        deps: &[Vec<DropDep>],
        dropped: &mut BTreeSet<usize>,
        fk_dropped: &mut BTreeSet<(TKey, String)>,
        ops: &FamilyOps,
        out: &mut Vec<String>,
        depth: usize,
    ) {
        if dropped.contains(&i) || depth > 32 {
            return;
        }
        for dep in &deps[i] {
            match dep {
                DropDep::Item(j) => emit_drops(*j, drafts, deps, dropped, fk_dropped, ops, out, depth + 1),
                DropDep::Fk(j, child, name) => {
                    if !dropped.contains(j) && fk_dropped.insert((child.clone(), name.clone())) {
                        if let Some(s) = (ops.drop_fk_sql)(child, name) {
                            out.push(s);
                        }
                    }
                }
            }
        }
        dropped.insert(i);
        out.extend(drafts[i].drop.iter().cloned());
    }
    for &i in &pre {
        if decided[i].action == PlanAction::DropRecreate && decided[i].ddl {
            let mut out = Vec::new();
            emit_drops(i, &drafts, &drop_deps, &mut dropped, &mut fk_dropped, &ops, &mut out, 0);
            sql[i].extend(out);
        }
    }
    let mut tables: BTreeMap<String, TableDataSpec> = BTreeMap::new();
    let mut has_data = vec![false; n];
    for i in 0..n {
        let a = decided[i].action;
        let d = &drafts[i];
        match a {
            PlanAction::Excluded | PlanAction::SkipExisting | PlanAction::Unsupported | PlanAction::ResumeDone => continue,
            _ => {}
        }
        match d.kind {
            CopyObjectKind::Table => {
                if decided[i].ddl && runs_ddl(a) {
                    sql[i].extend(d.create.iter().cloned());
                }
                let data = content != CopyContent::SchemaOnly && loads(a);
                if data {
                    if let Some(spec) = &d.data {
                        if a == PlanAction::Truncate && !in_group.contains(&i) {
                            if let Some(t) = &d.truncate {
                                sql[i].push(t.clone());
                            }
                        }
                        if let Some(t) = &d.load_template {
                            sql[i].push(t.clone());
                        }
                        let mut spec = spec.clone();
                        spec.fk_checks = fk_checks.contains(&i);
                        spec.est_rows = d.est_rows;
                        tables.insert(d.id.clone(), spec);
                        has_data[i] = true;
                    }
                }
            }
            CopyObjectKind::SequenceValue | CopyObjectKind::AutoIncrement => {
                let floor = {
                    let parent_existing = d.parent.as_ref().and_then(|p| index.get(p)).is_some_and(|&j| {
                        matches!(decided[j].action, PlanAction::Truncate | PlanAction::Append | PlanAction::ResumeDone) || (decided[j].exists && !decided[j].ddl)
                    });
                    parent_existing || (d.parent.is_none() && decided[i].exists)
                };
                if let Some((created, floor_t)) = &d.seq_templates {
                    sql[i].push(if floor { floor_t.clone() } else { created.clone() });
                } else {
                    sql[i].extend(d.create.iter().cloned());
                }
            }
            _ => {
                if d.group.is_none() {
                    sql[i].extend(d.create.iter().cloned());
                }
            }
        }
    }
    // Combined ALTER groups (MySQL keys / FKs).
    let mut groups: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for i in 0..n {
        if let Some((g, _, _)) = &drafts[i].group {
            if decided[i].action == PlanAction::Create {
                groups.entry(g.clone()).or_default().push(i);
            }
        }
    }
    for (g, mut members) in groups {
        members.sort_by_key(|&i| (order[i], i));
        let first = members[0];
        let table = g.split_once('|').map(|(t, _)| t.to_string()).unwrap_or(g.clone());
        let mut combined: Vec<String> = Vec::new();
        let mut separate: Vec<String> = Vec::new();
        for &m in &members {
            let (_, clause, sep) = drafts[m].group.clone().unwrap_or_default();
            if sep {
                separate.push(format!("ALTER TABLE {table} {clause}"));
            } else {
                combined.push(clause);
            }
        }
        let mut stmts = Vec::new();
        if !combined.is_empty() {
            stmts.push(format!("ALTER TABLE {table} {}", combined.join(", ")));
        }
        stmts.extend(separate);
        sql[first] = stmts;
        let fid = drafts[first].id.clone();
        for &m in &members[1..] {
            if !drafts[m].depends_on.contains(&fid) {
                drafts[m].depends_on.push(fid.clone());
            }
        }
    }

    // ---- items
    let mut items: Vec<PlanItem> = Vec::with_capacity(n);
    for i in 0..n {
        let d = &drafts[i];
        let a = decided[i].action;
        let mut item_warnings = d.warnings.clone();
        let mut item_blockers = d.blockers.clone();
        let ddl_runs = decided[i].ddl && runs_ddl(a) || matches!(a, PlanAction::Run);
        if !ddl_runs && a != PlanAction::Unsupported {
            item_warnings.retain(|w| !DDL_ONLY.contains(&w.code.as_str()));
            item_blockers.retain(|b| !DDL_ONLY.contains(&b.code.as_str()));
        }
        match a {
            PlanAction::Excluded => {
                item_warnings.retain(|w| w.severity == Severity::Info);
                item_blockers.clear();
                if not_selected.get(i).copied().unwrap_or(false) {
                    item_warnings.push(info("DEPENDENCY_NOT_SELECTED", "it references a table that is not selected"));
                }
            }
            PlanAction::SkipExisting | PlanAction::ResumeDone => {
                item_blockers.retain(|b| b.code == "RESUME_APPEND_PARTIAL");
                item_warnings.retain(|w| matches!(w.code.as_str(), "SKIPPED_DIFFERS" | "RESUME_DONE" | "RESUME_APPEND_PARTIAL" | "RESUME_TARGET_CHANGED"));
            }
            PlanAction::Unsupported => {
                item_blockers.clear();
                if dep_unsupported.get(i).copied().unwrap_or(false) && !d.unsupported {
                    item_warnings.push(info("UNSUPPORTED_OBJECT", "it depends on an object that is not copied"));
                }
            }
            _ => {}
        }
        for mut b in item_blockers {
            b.item_id = Some(d.id.clone());
            b.severity = Severity::Danger;
            blockers.push(b);
        }
        for w in &mut item_warnings {
            w.item_id = Some(d.id.clone());
        }
        let exists = decided[i].exists && !target_db_missing;
        items.push(PlanItem {
            id: d.id.clone(),
            kind: d.kind,
            parent: d.parent.clone(),
            schema: d.schema.clone(),
            name: d.name.clone(),
            target_schema: d.target_schema.clone(),
            target_name: d.target_name.clone(),
            action: a,
            phase: phase[i],
            order: order[i],
            exists_in_target: exists,
            target_has_rows: decided[i].has_rows,
            target_est_rows: if exists { decided[i].target_est } else { None },
            has_data: has_data[i],
            est_rows: d.est_rows,
            est_bytes: d.est_bytes,
            source_ddl: d.source_ddl.clone(),
            target_sql: std::mem::take(&mut sql[i]),
            warnings: item_warnings,
            depends_on: d.depends_on.clone(),
        });
    }
    items.sort_by_key(|a| (a.phase, a.order));

    // ---- totals
    let mut totals = CopyTotals::default();
    let mut est_rows = 0u64;
    let mut est_bytes = 0u64;
    let mut any_est = false;
    for it in &items {
        if it.kind == CopyObjectKind::Table {
            totals.tables += 1;
        } else {
            totals.objects += 1;
        }
        match it.action {
            PlanAction::Create => totals.create += 1,
            PlanAction::SkipExisting => totals.skip += 1,
            PlanAction::Truncate => totals.truncate += 1,
            PlanAction::DropRecreate => totals.drop_recreate += 1,
            PlanAction::Append => totals.append += 1,
            PlanAction::Unsupported => totals.unsupported += 1,
            PlanAction::Excluded => totals.excluded += 1,
            PlanAction::Run | PlanAction::ResumeDone => {}
        }
        if it.has_data {
            if let Some(r) = it.est_rows {
                est_rows += r;
                any_est = true;
            }
            if let Some(b) = it.est_bytes {
                est_bytes += b;
                any_est = true;
            }
        }
    }
    if any_est {
        totals.est_rows = Some(est_rows);
        totals.est_bytes = Some(est_bytes);
    }

    // ---- effective
    let c = concurrency(opts, family);
    let (ss, ts) = sessions(opts, c);
    effective.table_concurrency = c;
    effective.source_sessions = ss;
    effective.target_sessions = ts;

    // Plan-level blockers / warnings never carry an item id unless they are item issues.
    let plan = CopyPlan {
        plan_id: String::new(),
        family,
        source: source_facts,
        target: target_facts,
        effective,
        items,
        warnings: std::mem::take(&mut warnings),
        blockers: std::mem::take(&mut blockers)
            .into_iter()
            .map(|mut b| {
                b.severity = Severity::Danger;
                b
            })
            .collect(),
        acks: Vec::new(),
        session_sql,
        totals,
        resume_of: input.resume.map(|r| r.run_id.clone()),
        elapsed_ms: 0,
    };
    Ok(PlannedCopy { plan, tables })
}

#[derive(Debug, Clone)]
enum DropDep {
    /// A drop_recreate item that must be dropped first.
    Item(usize),
    /// A foreign key of another drop_recreate table referencing this one.
    Fk(usize, TKey, String),
}

#[cfg(test)]
#[path = "schema/plan_tests.rs"]
mod tests;
