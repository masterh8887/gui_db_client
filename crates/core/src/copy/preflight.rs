//! Copy preflight (section 5.3): the 5.3 issues, server identity / `same_server`, acks and
//! `plan_id`. `build_plan` (copy-schema) creates the items and translation issues; `finalize`
//! adds the 5.3 issues, the session settings when the planner left them empty, fills `acks`
//! and sets `plan_id`. Owner: copy-data.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{json, Value};

use super::data::mysql_encode;
use super::hash::sha256_hex_prefix;
use super::types::*;
use crate::error::Result;
use crate::types::{ConnTag, Connection, Severity};

/// Result of comparing two server identities.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SameServer {
    Same,
    Unsure,
    Different,
}

/// Codes computed from live state: excluded from `plan_id` (section 2, 5.3).
pub const ENV_CODES: &[&str] = &[
    "TARGET_NOT_WRITABLE",
    "PRODUCTION_CONFIRM",
    "WORK_MEM_HIGH",
    "CONCURRENT_SESSION",
    "SESSIONS_UNKNOWN",
    "SRC_MYISAM_LOCK",
    "SRC_LONG_SNAPSHOT",
    "DISK_LOW",
    "RUN_IN_USE",
    "TARGET_IN_USE",
    "SOURCE_BEING_WRITTEN",
    "PLAN_CHANGED",
    "ENUM_ERROR_VALUE",
];

pub fn is_env_code(code: &str) -> bool {
    ENV_CODES.contains(&code)
}

/// Everything `finalize` needs besides the plan.
pub struct FinalizeInput<'a> {
    pub source_conn: &'a Connection,
    pub target_conn: &'a Connection,
    pub source_identity: &'a ServerIdentity,
    pub target: &'a TargetFacts,
    /// `copy_start` / `copy_resume`: the typed production name; `None` while planning.
    pub confirm_production: Option<&'a str>,
    pub at_start: bool,
    /// Source facts (versions, replica status), when read.
    pub source: Option<&'a TargetFacts>,
    /// Live-state issues computed by the caller (`TARGET_IN_USE`, `RUN_IN_USE`,
    /// `SOURCE_BEING_WRITTEN`, `WORK_MEM_HIGH`, `RESUME_*`, ...).
    pub extra: Vec<CopyIssue>,
    /// MySQL target `SHOW GRANTS` rows (`TGT_PRIVILEGE`); `None` = unknown.
    pub grants: Option<&'a MetaTable>,
}

impl<'a> FinalizeInput<'a> {
    pub fn new(source_conn: &'a Connection, target_conn: &'a Connection, source_identity: &'a ServerIdentity, target: &'a TargetFacts) -> FinalizeInput<'a> {
        FinalizeInput { source_conn, target_conn, source_identity, target, confirm_production: None, at_start: false, source: None, extra: Vec::new(), grants: None }
    }
}

/// An issue.
pub fn issue(code: &str, severity: Severity, message: impl Into<String>) -> CopyIssue {
    CopyIssue { code: code.to_string(), severity, message: message.into(), detail: None, recommendation: None, item_id: None }
}

/// `'text'` for PostgreSQL (E'' with doubled backslashes when needed).
pub fn pg_literal(s: &str) -> String {
    if s.contains('\\') {
        format!("E'{}'", s.replace('\\', "\\\\").replace('\'', "''"))
    } else {
        format!("'{}'", s.replace('\'', "''"))
    }
}

fn truthy(v: Option<&String>) -> bool {
    v.is_some_and(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "on" | "1" | "true" | "t" | "yes"))
}

/// Server identity from the facts read on a meta session (5.3).
///
/// * PG: `system_identifier` when readable (key `pg:sys:<id>`), else `pg:<start time>@<port>`;
///   the start-time key is also kept in `group_name` so two sides of which only one could read
///   `pg_control_system()` still compare.
/// * MySQL 5.6+: `@@server_uuid`; replica status `Source_UUID` / `Master_UUID` in `replica_of`;
///   `group_replication_group_name`. Before 5.6: `mysql-weak|host|port|start|datadir` (`weak`).
/// * SQLite: canonical parent directory + file name.
pub fn identity(facts: &TargetFacts) -> ServerIdentity {
    let v = |k: &str| facts.vars.get(k).map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    match facts.family {
        CopyFamily::Sqlite => ServerIdentity { key: format!("sqlite:{}", super::target::sqlite::identity_key(&facts.endpoint)), ..ServerIdentity::default() },
        CopyFamily::Postgres => {
            let start_key = match (v("start_time"), v("port")) {
                (Some(s), Some(p)) => Some(format!("pg:{s}@{p}")),
                _ => None,
            };
            let sys = v("system_identifier");
            let key = match (&sys, &start_key) {
                (Some(s), _) => format!("pg:sys:{s}"),
                (None, Some(k)) => k.clone(),
                (None, None) => format!("pg:{}", facts.endpoint),
            };
            ServerIdentity {
                key,
                system_identifier: sys,
                group_name: start_key,
                in_recovery: truthy(facts.vars.get("pg_is_in_recovery")),
                read_only: truthy(facts.vars.get("pg_is_in_recovery")),
                ..ServerIdentity::default()
            }
        }
        CopyFamily::Mysql => {
            let uuid = v("server_uuid");
            let read_only = truthy(facts.vars.get("read_only"));
            match uuid {
                Some(u) => ServerIdentity {
                    key: format!("mysql:uuid:{u}"),
                    server_uuid: Some(u),
                    replica_of: v("replica_source_uuid"),
                    group_name: v("group_replication_group_name"),
                    read_only,
                    ..ServerIdentity::default()
                },
                None => {
                    let start = match (v("now").and_then(|n| n.parse::<i64>().ok()), v("uptime").and_then(|u| u.parse::<i64>().ok())) {
                        (Some(n), Some(u)) => (n - u).to_string(),
                        _ => String::new(),
                    };
                    let host = v("hostname").unwrap_or_else(|| facts.endpoint.split(':').next().unwrap_or("").to_string());
                    let port = v("port").unwrap_or_else(|| facts.endpoint.rsplit(':').next().unwrap_or("").to_string());
                    ServerIdentity {
                        key: format!("mysql-weak|{host}|{port}|{start}|{}", v("datadir").unwrap_or_default()),
                        replica_of: v("replica_source_uuid"),
                        read_only,
                        weak: true,
                        ..ServerIdentity::default()
                    }
                }
            }
        }
    }
}

fn weak_parts(key: &str) -> Option<(&str, &str, Option<i64>, &str)> {
    let mut p = key.strip_prefix("mysql-weak|")?.split('|');
    let host = p.next()?;
    let port = p.next()?;
    let start = p.next()?.parse::<i64>().ok();
    let datadir = p.next().unwrap_or("");
    Some((host, port, start, datadir))
}

/// Whether the comma-joined replica-source list `list` (every replication channel, see
/// `facts::replica_source_uuids`) names `uuid`.
fn names_uuid(list: &Option<String>, uuid: &Option<String>) -> bool {
    match (list, uuid) {
        (Some(l), Some(u)) if !u.trim().is_empty() => l.split(',').any(|x| x.trim().eq_ignore_ascii_case(u.trim())),
        _ => false,
    }
}

/// Whether two identities are the same server (5.3).
pub fn same_server(a: &ServerIdentity, b: &ServerIdentity) -> SameServer {
    // PostgreSQL: system identifiers (a standby shares its primary's), else start time + port.
    if let (Some(x), Some(y)) = (&a.system_identifier, &b.system_identifier) {
        return if x == y { SameServer::Same } else { SameServer::Different };
    }
    if a.key.starts_with("pg:") || b.key.starts_with("pg:") {
        let same = matches!((&a.group_name, &b.group_name), (Some(x), Some(y)) if x == y) || a.key == b.key;
        if same {
            return SameServer::Same;
        }
        // A standby and its primary differ in start time and often in port: without the
        // system identifier of both sides they cannot be told apart.
        return if a.in_recovery || b.in_recovery { SameServer::Unsure } else { SameServer::Different };
    }
    // MySQL 5.6+: uuid, replica-of (any replication channel), group name.
    if a.server_uuid.is_some() || b.server_uuid.is_some() {
        let eq = |x: &Option<String>, y: &Option<String>| matches!((x, y), (Some(x), Some(y)) if !x.is_empty() && x.eq_ignore_ascii_case(y));
        if eq(&a.server_uuid, &b.server_uuid)
            || names_uuid(&a.replica_of, &b.server_uuid)
            || names_uuid(&b.replica_of, &a.server_uuid)
            || eq(&a.group_name, &b.group_name)
        {
            return SameServer::Same;
        }
        return SameServer::Different;
    }
    // MySQL before 5.6: hostname:port (+ start time within 2 s and datadir).
    if let (Some(x), Some(y)) = (weak_parts(&a.key), weak_parts(&b.key)) {
        if !(x.0.eq_ignore_ascii_case(y.0) && x.1 == y.1) {
            return SameServer::Different;
        }
        let start_same = matches!((x.2, y.2), (Some(s), Some(t)) if (s - t).abs() <= 2);
        return if start_same && !x.3.is_empty() && x.3 == y.3 { SameServer::Same } else { SameServer::Unsure };
    }
    if !a.key.is_empty() && a.key == b.key {
        SameServer::Same
    } else {
        SameServer::Different
    }
}

/// Session settings of both sides when the planner left them empty (4.1 / 4.3).
pub fn fill_session_sql(plan: &mut CopyPlan, target: &TargetFacts, source: Option<&TargetFacts>, lock_timeout_s: u32, zero_dates: ZeroDatePolicy) {
    if plan.session_sql.target.is_empty() {
        plan.session_sql.target = match plan.family {
            CopyFamily::Mysql => {
                let effective = plan
                    .effective
                    .zero_dates
                    .unwrap_or_else(|| mysql_encode::resolve_zero_dates(zero_dates, target.vars.get("sql_mode").map(String::as_str).unwrap_or("")));
                mysql_encode::session_prelude(target, effective, lock_timeout_s)
            }
            CopyFamily::Postgres => super::target::pg::write_session_sql(lock_timeout_s),
            CopyFamily::Sqlite => Vec::new(),
        };
    }
    if plan.session_sql.source.is_empty() {
        plan.session_sql.source = match plan.family {
            CopyFamily::Postgres => super::target::pg::source_session_sql(false),
            CopyFamily::Mysql => {
                let mut v = Vec::new();
                if let Some(s) = source {
                    if s.at_least(5, 6, 5) && s.flavor == "mysql" {
                        v.push("SET SESSION TRANSACTION READ ONLY".to_string());
                    }
                    if s.at_least(4, 1, 3) {
                        v.push("SET time_zone = '+00:00'".to_string());
                    }
                }
                v
            }
            CopyFamily::Sqlite => Vec::new(),
        };
    }
}

/// `TGT_PRIVILEGE` (MySQL) from `SHOW GRANTS` (best effort: no row for `*.*` or the database
/// means unknown, no issue).
pub fn mysql_privilege_issue(grants: &MetaTable, database: &str, plan: &CopyPlan) -> Option<CopyIssue> {
    let mut privs: BTreeSet<String> = BTreeSet::new();
    let mut relevant = false;
    let db_l = database.to_ascii_lowercase();
    for row in &grants.rows {
        let Some(line) = row.first().and_then(|v| v.as_deref()) else { continue };
        let up = line.to_ascii_uppercase();
        let (Some(on), Some(to)) = (up.find(" ON "), up.rfind(" TO ")) else { continue };
        if !up.starts_with("GRANT ") || on > to {
            continue;
        }
        let object = line[on + 4..to].trim().replace('`', "").to_ascii_lowercase();
        let matches = object == "*.*" || object.strip_suffix(".*").is_some_and(|pat| like_match(pat, &db_l));
        if !matches {
            continue;
        }
        relevant = true;
        for p in up["GRANT ".len()..on].split(',') {
            privs.insert(p.trim().to_string());
        }
    }
    if !relevant || privs.contains("ALL PRIVILEGES") || privs.contains("ALL") {
        return None;
    }
    let mut need = vec!["CREATE", "INSERT", "ALTER", "INDEX"];
    let has = |k: CopyObjectKind| plan.items.iter().any(|i| i.kind == k && matches!(i.action, PlanAction::Create | PlanAction::DropRecreate | PlanAction::Run));
    if has(CopyObjectKind::ForeignKey) {
        need.push("REFERENCES");
    }
    if has(CopyObjectKind::View) {
        need.push("CREATE VIEW");
    }
    if has(CopyObjectKind::Trigger) {
        need.push("TRIGGER");
    }
    if plan.items.iter().any(|i| i.action == PlanAction::DropRecreate) {
        need.push("DROP");
    }
    let missing: Vec<&str> = need.into_iter().filter(|p| !privs.contains(*p)).collect();
    if missing.is_empty() {
        return None;
    }
    let mut i = issue("TGT_PRIVILEGE", Severity::Danger, format!("the target account lacks privileges on {database}: {}", missing.join(", ")));
    i.recommendation = Some(format!("GRANT {} ON `{database}`.* TO the copy account", missing.join(", ")));
    Some(i)
}

/// MySQL grant database patterns: `%` / `_` wildcards, `\%` / `\_` literals.
fn like_match(pat: &str, s: &str) -> bool {
    let mut re = String::from("^");
    let mut chars = pat.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => match chars.next() {
                Some(n) => re.push_str(&regex::escape(&n.to_string())),
                None => re.push_str("\\\\"),
            },
            '%' => re.push_str(".*"),
            '_' => re.push('.'),
            other => re.push_str(&regex::escape(&other.to_string())),
        }
    }
    re.push('$');
    regex::Regex::new(&re).map(|r| r.is_match(s)).unwrap_or(false)
}

/// Source-side environment issues from the plan items (5.3): `SRC_MYISAM_LOCK`,
/// `SRC_LONG_SNAPSHOT`, `SRC_NOT_SNAPSHOT`.
fn source_issues(plan: &CopyPlan) -> Vec<CopyIssue> {
    const GB: u64 = 1 << 30;
    let mut out = Vec::new();
    let loads: Vec<&PlanItem> = plan
        .items
        .iter()
        .filter(|i| i.has_data && matches!(i.action, PlanAction::Create | PlanAction::Truncate | PlanAction::DropRecreate | PlanAction::Append))
        .collect();
    if plan.family == CopyFamily::Mysql {
        for i in &loads {
            let myisam = i.source_ddl.as_deref().is_some_and(|d| {
                let u = d.to_ascii_uppercase();
                u.contains("ENGINE=MYISAM") || u.contains("TYPE=MYISAM")
            });
            if myisam && i.est_bytes.unwrap_or(0) > GB {
                let mut x = issue("SRC_MYISAM_LOCK", Severity::Warning, format!("{} is a MyISAM table over 1 GB: its writers wait while it is read", i.name));
                x.item_id = Some(i.id.clone());
                out.push(x);
            }
        }
    }
    if matches!(plan.family, CopyFamily::Postgres | CopyFamily::Mysql) {
        let total: u64 = loads.iter().map(|i| i.est_bytes.unwrap_or(0)).sum();
        let big = loads.iter().any(|i| i.est_bytes.unwrap_or(0) > 10 * GB);
        if big || (plan.effective.consistent_snapshot && total > 10 * GB) {
            out.push(issue("SRC_LONG_SNAPSHOT", Severity::Info, "a long read holds back vacuum / purge on the source while it runs"));
        }
        if !plan.effective.consistent_snapshot && loads.len() > 1 {
            let mut x = issue("SRC_NOT_SNAPSHOT", Severity::Warning, "tables are read at different times: a live source can yield cross-table inconsistencies");
            x.recommendation = Some("copy from a quiet source, or a replica stopped for the copy".into());
            out.push(x);
        }
    }
    out
}

/// The ENUM columns of the MySQL DDL `ddl` (a `CREATE TABLE` / `SHOW CREATE TABLE` text):
/// (column name, whether `''` is one of its members).
pub fn mysql_enum_columns(ddl: &str) -> Vec<(String, bool)> {
    let b = ddl.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] != b'`' {
            i += 1;
            continue;
        }
        // A quoted identifier (`` doubles a backtick).
        let mut name = Vec::new();
        let mut j = i + 1;
        let mut closed = false;
        while j < b.len() {
            if b[j] == b'`' {
                if j + 1 < b.len() && b[j + 1] == b'`' {
                    name.push(b'`');
                    j += 2;
                    continue;
                }
                closed = true;
                j += 1;
                break;
            }
            name.push(b[j]);
            j += 1;
        }
        i = j;
        if !closed {
            break;
        }
        let mut k = j;
        while k < b.len() && b[k].is_ascii_whitespace() {
            k += 1;
        }
        if k + 5 > b.len() || !b[k..k + 5].eq_ignore_ascii_case(b"enum(") {
            continue;
        }
        k += 5;
        // The member list: 'a','b' with '' and backslash escapes, up to `)`.
        let mut has_empty = false;
        let mut complete = false;
        while k < b.len() {
            match b[k] {
                b' ' | b',' | b'\t' | b'\n' | b'\r' => k += 1,
                b')' => {
                    complete = true;
                    k += 1;
                    break;
                }
                b'\'' => {
                    k += 1;
                    let mut len = 0usize;
                    while k < b.len() {
                        match b[k] {
                            b'\\' => {
                                k += 2;
                                len += 1;
                            }
                            b'\'' if k + 1 < b.len() && b[k + 1] == b'\'' => {
                                k += 2;
                                len += 1;
                            }
                            b'\'' => {
                                k += 1;
                                break;
                            }
                            _ => {
                                k += 1;
                                len += 1;
                            }
                        }
                    }
                    has_empty |= len == 0;
                }
                _ => break,
            }
        }
        i = k;
        if complete {
            out.push((String::from_utf8_lossy(&name).into_owned(), has_empty));
        }
    }
    out
}

/// MySQL: per data item, the (target names of the) source ENUM columns without a `''` member,
/// from the source's `SHOW CREATE TABLE` (else the item's own `CREATE TABLE`): a `''` read
/// from such a column is the error value a non-strict server stored for invalid input, which
/// the strict writer cannot load.
pub fn mysql_enum_error_columns(planned: &PlannedCopy) -> BTreeMap<String, Vec<String>> {
    let mut out = BTreeMap::new();
    if planned.plan.family != CopyFamily::Mysql {
        return out;
    }
    for item in planned.plan.items.iter().filter(|i| planned.tables.contains_key(&i.id)) {
        let spec = &planned.tables[&item.id];
        let create = |s: &&String| s.trim_start().get(..12).is_some_and(|p| p.eq_ignore_ascii_case("CREATE TABLE"));
        let (ddl, by_source) = match (&item.source_ddl, item.target_sql.iter().find(create)) {
            (Some(d), _) => (d.as_str(), true),
            (None, Some(c)) => (c.as_str(), false),
            (None, None) => continue,
        };
        let mut cols: Vec<String> = Vec::new();
        for (name, has_empty) in mysql_enum_columns(ddl) {
            if has_empty {
                continue;
            }
            if let Some(c) = spec.columns.iter().find(|c| (if by_source { &c.source } else { &c.target }) == &name && c.value == ValueKind::Text) {
                if !cols.contains(&c.target) {
                    cols.push(c.target.clone());
                }
            }
        }
        if !cols.is_empty() {
            out.insert(item.id.clone(), cols);
        }
    }
    out
}

/// Tables up to this many (estimated) rows are probed for ENUM error values at plan time.
pub const ENUM_PROBE_MAX_ROWS: u64 = 1_000_000;

/// `ENUM_ERROR_VALUE` (env): a probe per table of [`mysql_enum_error_columns`] — `SELECT 1
/// FROM t WHERE c1 = 0 OR … LIMIT 1` on the source (an ENUM compared with 0 matches its
/// error value) — for tables up to [`ENUM_PROBE_MAX_ROWS`] estimated rows; larger tables are
/// not scanned while planning and fail at run time with the same code. A warning under the
/// default `enum_error: fail` (the table will fail), info under `null`.
pub async fn mysql_enum_issues(
    live: &crate::connect::LiveConnection,
    planned: &PlannedCopy,
    database: &str,
    policy: Option<EnumErrorPolicy>,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<Vec<CopyIssue>> {
    let mut out = Vec::new();
    if planned.plan.family != CopyFamily::Mysql {
        return Ok(out);
    }
    let q = mysql_encode::qi;
    for (id, cols) in mysql_enum_error_columns(planned) {
        let Some(item) = planned.plan.items.iter().find(|i| i.id == id) else { continue };
        if item.est_rows.is_none_or(|n| n > ENUM_PROBE_MAX_ROWS) {
            continue;
        }
        let spec = &planned.tables[&id];
        let source_cols: Vec<&str> = spec.columns.iter().filter(|c| cols.contains(&c.target)).map(|c| c.source.as_str()).collect();
        let cond = source_cols.iter().map(|c| format!("{} = 0", q(c))).collect::<Vec<_>>().join(" OR ");
        let sql = format!("SELECT 1 AS x FROM {}.{} WHERE {cond} LIMIT 1", q(database), q(&item.name));
        let m = match super::facts::source_query_cancellable(live, &sql, cancel, std::time::Duration::from_millis(1500)).await {
            Ok(m) => m,
            Err(crate::error::CoreError::Cancelled) => return Err(crate::error::CoreError::Cancelled),
            Err(e) => {
                tracing::warn!(item = %id, "copy: ENUM error value probe failed: {e}");
                continue;
            }
        };
        if m.is_empty() {
            continue;
        }
        let null = policy == Some(EnumErrorPolicy::Null);
        let mut i = issue(
            "ENUM_ERROR_VALUE",
            if null { Severity::Info } else { Severity::Warning },
            if null {
                format!("{}: ENUM error values ('' stored for invalid input) load as NULL", item.name)
            } else {
                format!("{}: ENUM columns hold the error value '' (stored for invalid input), which the strict target refuses: the table fails", item.name)
            },
        );
        i.detail = Some(cols.join(", "));
        i.item_id = Some(id.clone());
        if !null {
            i.recommendation = Some("load them as NULL (enum_error: null), or exclude the table".into());
        }
        out.push(i);
    }
    Ok(out)
}

/// `UNKNOWN_ITEM` (danger, acknowledged): ids in `options.exclude` / `existing_overrides` that
/// match no plan item. The planner matches them exactly, so such an entry does nothing — e.g. a
/// PostgreSQL table given without its schema — while the user believes a table is left out
/// or protected. The CLI resolves names against the plan and refuses unknown ones before
/// starting; the UI takes its ids from the plan.
pub fn unknown_item_issues(plan: &CopyPlan, options: &CopyOptions) -> Vec<CopyIssue> {
    let known: BTreeSet<&str> = plan.items.iter().map(|i| i.id.as_str()).collect();
    let mut unknown: Vec<String> = Vec::new();
    for id in options.exclude.iter().chain(options.existing_overrides.iter().map(|o| &o.item_id)) {
        if !known.contains(id.as_str()) && !unknown.contains(id) {
            unknown.push(id.clone());
        }
    }
    if unknown.is_empty() {
        return Vec::new();
    }
    let mut i = issue("UNKNOWN_ITEM", Severity::Danger, format!("{} excluded or per-table policy id(s) match no item of this plan and have no effect", unknown.len()));
    i.detail = Some(unknown.join("\n"));
    i.recommendation = Some("use the item ids of the plan (PostgreSQL tables include their schema: table:\"app\".\"orders\")".into());
    vec![i]
}

/// An export preflight issue (`WORK_MEM_HIGH`, `CONCURRENT_SESSION`, ...) as a copy issue, with
/// the same code, severity and texts.
pub fn from_preflight(i: crate::types::PreflightIssue) -> CopyIssue {
    CopyIssue { code: i.code, severity: i.severity, message: i.message, detail: i.detail, recommendation: i.recommendation, item_id: None }
}

/// The source tables this plan reads (its load items), as the monitor's [`TableRef`]s.
pub fn source_tables(plan: &CopyPlan) -> Vec<crate::types::TableRef> {
    let mut out: Vec<crate::types::TableRef> = Vec::new();
    for i in plan.items.iter().filter(|i| {
        i.has_data
            && matches!(i.kind, CopyObjectKind::Table | CopyObjectKind::Partition)
            && matches!(i.action, PlanAction::Create | PlanAction::Truncate | PlanAction::DropRecreate | PlanAction::Append)
    }) {
        let schema = if plan.family == CopyFamily::Postgres { i.schema.as_deref() } else { None };
        let t = crate::types::TableRef::new(schema, &i.name);
        if !out.contains(&t) {
            out.push(t);
        }
    }
    out
}

/// The export preflight's session check (5.3 `CONCURRENT_SESSION` warning, `SESSIONS_UNKNOWN`
/// info) on the source, for the tables this plan reads; run on the plan / meta session, which
/// the monitor leaves out.
pub async fn source_session_issues(monitor: &dyn crate::monitor::Monitor, plan: &CopyPlan) -> Vec<CopyIssue> {
    let tables = source_tables(plan);
    let (issue, _sessions) = crate::preflight::concurrent_sessions(monitor, &tables).await;
    issue.map(from_preflight).into_iter().collect()
}

/// Add the 5.3 issues, fill `acks` and set `plan_id`.
pub fn finalize(planned: &mut PlannedCopy, input: &FinalizeInput<'_>) -> Result<()> {
    let plan = &mut planned.plan;
    let mut add: Vec<CopyIssue> = Vec::new();
    let mut block: Vec<CopyIssue> = Vec::new();

    if input.target_conn.write_allowed != Some(true) {
        let mut i = issue("TARGET_NOT_WRITABLE", Severity::Danger, format!("the target connection {} is not allowed to be written to", input.target_conn.name));
        i.recommendation = Some("allow writes on the target connection (DB copy target) first".into());
        block.push(i);
    }
    if input.target_conn.tag == Some(ConnTag::Production) {
        if !input.at_start {
            add.push(issue("PRODUCTION_CONFIRM", Severity::Info, "실행할 때 대상 이름 입력이 필요합니다 (the target is tagged production: type its name to start)"));
        } else if input.confirm_production.map(str::trim) != Some(input.target_conn.name.trim()) {
            block.push(issue("PRODUCTION_CONFIRM", Severity::Danger, format!("the target is tagged production: type its name ({}) to start", input.target_conn.name)));
        }
    }
    let same_db = plan.source.database.eq_ignore_ascii_case(&plan.target.database) || input.target.database.eq_ignore_ascii_case(&plan.source.database);
    let same_conn = input.source_conn.id == input.target_conn.id && same_db;
    match same_server(input.source_identity, &input.target.identity) {
        _ if same_conn => block.push(issue("SAME_DATABASE", Severity::Danger, "the source and the target are the same database")),
        SameServer::Same if same_db => block.push(issue(
            "SAME_DATABASE",
            Severity::Danger,
            "the source and the target are the same database on the same server (a replica and its primary count as one)",
        )),
        SameServer::Unsure if same_db => add.push(issue(
            "SAME_SERVER_UNSURE",
            Severity::Danger,
            if plan.family == CopyFamily::Postgres {
                "the target may be the primary of the source standby (the system identifier of one side is unreadable) and has the same database name"
            } else {
                "the target may be the source server (same host and port; start time or data directory differ or are unreadable) with the same database name"
            },
        )),
        _ => {}
    }
    let src = input.source_identity;
    let replica_unreadable = input.source.is_some_and(|s| s.vars.get("replica_status").is_some_and(|v| v == "unreadable"));
    let replica = match plan.family {
        // Comparing system identifiers needs both: a standby whose own identifier is readable
        // still cannot be matched with a primary whose identifier is not.
        CopyFamily::Postgres => src.in_recovery && (src.system_identifier.is_none() || input.target.identity.system_identifier.is_none()),
        CopyFamily::Mysql => src.read_only && replica_unreadable,
        CopyFamily::Sqlite => false,
    };
    if replica {
        add.push(issue("SOURCE_IS_REPLICA", Severity::Danger, "If the target is this replica's primary, the source data changes."));
    }
    add.extend(source_issues(plan));
    if plan.family == CopyFamily::Sqlite {
        let est: u64 = plan
            .items
            .iter()
            .filter(|i| i.has_data && matches!(i.action, PlanAction::Create | PlanAction::Truncate | PlanAction::DropRecreate | PlanAction::Append))
            .map(|i| i.est_bytes.unwrap_or(0))
            .sum();
        if let Some(free) = input.target.vars.get("free_bytes").and_then(|v| v.parse::<u64>().ok()) {
            if (est as f64) * 1.2 > free as f64 {
                add.push(issue("DISK_LOW", Severity::Warning, format!("the target disk has {free} bytes free; the copy needs about {}", est + est / 5)));
            }
        }
    }
    if plan.family == CopyFamily::Mysql {
        if let Some(g) = input.grants {
            if let Some(i) = mysql_privilege_issue(g, &plan.target.database, plan) {
                block.push(i);
            }
        }
    }
    for i in &input.extra {
        // Danger issues block, except those acknowledged like warnings: the identity doubts,
        // destructive actions and, as in the export preflight, `WORK_MEM_HIGH`.
        if i.severity == Severity::Danger
            && !matches!(i.code.as_str(), "SAME_SERVER_UNSURE" | "SOURCE_IS_REPLICA" | "DESTRUCTIVE_TRUNCATE" | "DESTRUCTIVE_DROP" | "WORK_MEM_HIGH" | "UNKNOWN_ITEM")
        {
            block.push(i.clone());
        } else {
            add.push(i.clone());
        }
    }
    // Never twice (a re-finalized plan keeps one of each code / item).
    for i in add {
        if !plan.warnings.iter().any(|w| w.code == i.code && w.item_id == i.item_id) {
            plan.warnings.push(i);
        }
    }
    for i in block {
        if !plan.blockers.iter().any(|w| w.code == i.code && w.item_id == i.item_id) {
            plan.blockers.push(i);
        }
    }
    plan.acks = compute_acks(plan);
    plan.plan_id = plan_id(plan, input.source_identity, &input.target.identity);
    Ok(())
}

/// One ack per distinct `warning` / `danger` code (section 2): key = the code when no item
/// carries it, else `<CODE>#<12 hex of SHA-256 over the sorted item ids joined by "\n">`.
pub fn compute_acks(plan: &CopyPlan) -> Vec<AckNeed> {
    let mut codes: BTreeMap<String, (Severity, BTreeSet<String>)> = BTreeMap::new();
    let mut note = |code: &str, sev: Severity, item: Option<&str>| {
        if sev == Severity::Info || code == "PRODUCTION_CONFIRM" {
            return;
        }
        let e = codes.entry(code.to_string()).or_insert((sev, BTreeSet::new()));
        if sev == Severity::Danger {
            e.0 = Severity::Danger;
        }
        if let Some(i) = item {
            e.1.insert(i.to_string());
        }
    };
    for w in &plan.warnings {
        note(&w.code, w.severity, w.item_id.as_deref());
    }
    for it in &plan.items {
        for w in &it.warnings {
            note(&w.code, w.severity, Some(w.item_id.as_deref().unwrap_or(&it.id)));
        }
    }
    codes
        .into_iter()
        .map(|(code, (severity, items))| {
            let items: Vec<String> = items.into_iter().collect();
            let key = if items.is_empty() { code.clone() } else { format!("{code}#{}", sha256_hex_prefix(items.join("\n").as_bytes(), 12)) };
            AckNeed { code, key, severity, items }
        })
        .collect()
}

/// Canonical JSON (object keys sorted, no whitespace).
pub fn canonical_json(v: &Value) -> String {
    match v {
        Value::Object(m) => {
            let mut keys: Vec<&String> = m.keys().collect();
            keys.sort();
            let parts: Vec<String> = keys.iter().map(|k| format!("{}:{}", Value::String((*k).clone()), canonical_json(&m[*k]))).collect();
            format!("{{{}}}", parts.join(","))
        }
        Value::Array(a) => format!("[{}]", a.iter().map(canonical_json).collect::<Vec<_>>().join(",")),
        other => other.to_string(),
    }
}

/// `plan_id` (section 2 "Plan validity"): the first 32 hex digits of SHA-256 over a canonical
/// JSON of the family, both endpoints' identity key / database / schema, `effective`, every
/// item's `(id, kind, action, phase, order, target_sql)` and the non-env issue codes.
pub fn plan_id(plan: &CopyPlan, source: &ServerIdentity, target: &ServerIdentity) -> String {
    let mut codes: Vec<(String, String)> = Vec::new();
    for w in plan.warnings.iter().chain(plan.blockers.iter()) {
        if !is_env_code(&w.code) {
            codes.push((w.code.clone(), w.item_id.clone().unwrap_or_default()));
        }
    }
    for it in &plan.items {
        for w in &it.warnings {
            if !is_env_code(&w.code) {
                codes.push((w.code.clone(), w.item_id.clone().unwrap_or_else(|| it.id.clone())));
            }
        }
    }
    codes.sort();
    codes.dedup();
    let items: Vec<Value> = plan.items.iter().map(|i| json!([i.id, i.kind, i.action, i.phase, i.order, i.target_sql])).collect();
    let doc = json!({
        "family": plan.family,
        "source": [source.key, plan.source.database],
        "target": [target.key, plan.target.database],
        "effective": serde_json::to_value(&plan.effective).unwrap_or(Value::Null),
        "items": items,
        "codes": codes,
    });
    sha256_hex_prefix(canonical_json(&doc).as_bytes(), 32)
}

/// Item ids whose action or `target_sql` differ between two plans (added / removed items too):
/// the detail of `PLAN_CHANGED`.
pub fn plan_diff(old: &CopyPlan, new: &CopyPlan) -> Vec<String> {
    let index: BTreeMap<&str, &PlanItem> = old.items.iter().map(|i| (i.id.as_str(), i)).collect();
    let mut out: Vec<String> = Vec::new();
    for i in &new.items {
        match index.get(i.id.as_str()) {
            Some(o) if o.action == i.action && o.target_sql == i.target_sql => {}
            _ => out.push(i.id.clone()),
        }
    }
    let new_ids: BTreeSet<&str> = new.items.iter().map(|i| i.id.as_str()).collect();
    out.extend(old.items.iter().filter(|i| !new_ids.contains(i.id.as_str())).map(|i| i.id.clone()));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(family: CopyFamily, vars: &[(&str, &str)]) -> TargetFacts {
        let mut f = TargetFacts::new(family, if family == CopyFamily::Postgres { "postgres" } else { "mysql" }, "8.0.36", "db");
        f.endpoint = "db.example:3306".into();
        for (k, v) in vars {
            f.vars.insert(k.to_string(), v.to_string());
        }
        f
    }

    #[test]
    fn identity_rules() {
        // PG system identifier: a standby and its primary are the same cluster whatever the port.
        let a = identity(&facts(CopyFamily::Postgres, &[("system_identifier", "7001"), ("start_time", "t1"), ("port", "5432")]));
        let b = identity(&facts(CopyFamily::Postgres, &[("system_identifier", "7001"), ("start_time", "t2"), ("port", "5433"), ("pg_is_in_recovery", "true")]));
        assert_eq!(same_server(&a, &b), SameServer::Same);
        assert!(b.in_recovery);
        let c = identity(&facts(CopyFamily::Postgres, &[("system_identifier", "7002")]));
        assert_eq!(same_server(&a, &c), SameServer::Different);
        // Unreadable system identifier: start time + port.
        let d = identity(&facts(CopyFamily::Postgres, &[("start_time", "t1"), ("port", "5432")]));
        assert_eq!(same_server(&a, &d), SameServer::Same, "one side without system_identifier compares start time + port");
        let e = identity(&facts(CopyFamily::Postgres, &[("start_time", "t9"), ("port", "5432")]));
        assert_eq!(same_server(&d, &e), SameServer::Different);
        // A standby (identifier readable) and a primary whose identifier is not: start time and
        // port differ, yet they may be one cluster.
        let standby = identity(&facts(CopyFamily::Postgres, &[("system_identifier", "7001"), ("start_time", "t2"), ("port", "5433"), ("pg_is_in_recovery", "true")]));
        let primary_app = identity(&facts(CopyFamily::Postgres, &[("start_time", "t1"), ("port", "5432")]));
        assert_eq!(same_server(&standby, &primary_app), SameServer::Unsure);
        assert_eq!(same_server(&primary_app, &standby), SameServer::Unsure);
        // The fallback key never depends on the session's TimeZone / DateStyle.
        assert!(crate::copy::facts::PG_START_TIME_SQL.contains("AT TIME ZONE 'UTC'") && !crate::copy::facts::PG_START_TIME_SQL.contains("::text"));
        // MySQL uuid, replica of, group name.
        let m1 = identity(&facts(CopyFamily::Mysql, &[("server_uuid", "U1")]));
        let m2 = identity(&facts(CopyFamily::Mysql, &[("server_uuid", "U2"), ("replica_source_uuid", "U1")]));
        let m3 = identity(&facts(CopyFamily::Mysql, &[("server_uuid", "U3"), ("group_replication_group_name", "G")]));
        let m4 = identity(&facts(CopyFamily::Mysql, &[("server_uuid", "U4"), ("group_replication_group_name", "G")]));
        assert_eq!(same_server(&m1, &identity(&facts(CopyFamily::Mysql, &[("server_uuid", "u1")]))), SameServer::Same);
        assert_eq!(same_server(&m2, &m1), SameServer::Same, "a replica and its source");
        assert_eq!(same_server(&m1, &m2), SameServer::Same);
        assert_eq!(same_server(&m3, &m4), SameServer::Same, "same group");
        assert_eq!(same_server(&m1, &m3), SameServer::Different);
        // A multi-source replica: the target is the primary of its second channel.
        let channels = MetaTable {
            columns: vec!["Channel_Name".into(), "Source_UUID".into()],
            rows: vec![vec![Some("a".into()), Some("U9".into())], vec![Some("b".into()), Some("U1".into())], vec![Some("c".into()), Some(String::new())]],
        };
        let list = crate::copy::facts::replica_source_uuids(&channels).unwrap();
        assert_eq!(list, "U9,U1");
        let multi = identity(&facts(CopyFamily::Mysql, &[("server_uuid", "U2"), ("replica_source_uuid", &list)]));
        assert_eq!(same_server(&multi, &m1), SameServer::Same, "channel 2's primary");
        assert_eq!(same_server(&m1, &multi), SameServer::Same);
        assert_eq!(same_server(&multi, &m3), SameServer::Different);
        assert!(crate::copy::facts::replica_source_uuids(&MetaTable::default()).is_none());
        // MySQL < 5.6: hostname/port + start time (2 s) + datadir.
        let w = |now: &str, up: &str, dd: &str, host: &str| {
            identity(&facts(CopyFamily::Mysql, &[("hostname", host), ("port", "3306"), ("now", now), ("uptime", up), ("datadir", dd)]))
        };
        let w1 = w("1000", "100", "/var/lib/mysql", "db1");
        assert!(w1.weak);
        assert_eq!(same_server(&w1, &w("1001", "100", "/var/lib/mysql", "db1")), SameServer::Same);
        assert_eq!(same_server(&w1, &w("1100", "100", "/var/lib/mysql", "db1")), SameServer::Unsure, "start time differs");
        assert_eq!(same_server(&w1, &w("1000", "100", "/data", "db1")), SameServer::Unsure, "datadir differs");
        assert_eq!(same_server(&w1, &w("1000", "100", "/var/lib/mysql", "db2")), SameServer::Different);
        // SQLite paths.
        let dir = tempfile::tempdir().unwrap();
        let mut s1 = TargetFacts::new(CopyFamily::Sqlite, "sqlite", "3.45", "main");
        s1.endpoint = dir.path().join("a.db").to_string_lossy().into_owned();
        let mut s2 = s1.clone();
        s2.endpoint = dir.path().join(".").join("a.db").to_string_lossy().into_owned();
        assert_eq!(same_server(&identity(&s1), &identity(&s2)), SameServer::Same);
        s2.endpoint = dir.path().join("b.db").to_string_lossy().into_owned();
        assert_eq!(same_server(&identity(&s1), &identity(&s2)), SameServer::Different);
    }

    fn plan_with(items: Vec<PlanItem>) -> PlannedCopy {
        let mut p = crate::copy::testkit::empty_plan(CopyFamily::Sqlite);
        p.source.database = "main".into();
        p.target.database = "main".into();
        p.items = items;
        PlannedCopy { plan: p, tables: BTreeMap::new() }
    }

    fn item(id: &str, warnings: Vec<CopyIssue>) -> PlanItem {
        PlanItem {
            id: id.into(),
            kind: CopyObjectKind::Table,
            parent: None,
            schema: None,
            name: id.into(),
            target_schema: None,
            target_name: id.into(),
            action: PlanAction::SkipExisting,
            phase: CopyPhase::PreData,
            order: 0,
            exists_in_target: true,
            target_has_rows: Some(true),
            target_est_rows: None,
            has_data: true,
            est_rows: Some(5),
            est_bytes: None,
            source_ddl: None,
            target_sql: vec![],
            warnings,
            depends_on: vec![],
        }
    }

    #[test]
    fn acks_keys_change_with_items_and_plan_id_ignores_env() {
        let w = |c: &str| issue(c, Severity::Warning, "x");
        let mut p = plan_with(vec![item("table:\"a\"", vec![w("SKIPPED_DIFFERS")]), item("table:\"b\"", vec![w("SKIPPED_DIFFERS")])]);
        p.plan.warnings.push(w("TABLES_SKIPPED_EXISTING"));
        p.plan.warnings.push(issue("PRODUCTION_CONFIRM", Severity::Info, "x"));
        let acks = compute_acks(&p.plan);
        assert_eq!(acks.len(), 2);
        let k1 = acks.iter().find(|a| a.code == "SKIPPED_DIFFERS").unwrap().clone();
        assert!(k1.key.starts_with("SKIPPED_DIFFERS#") && k1.key.len() == "SKIPPED_DIFFERS#".len() + 12);
        assert_eq!(k1.items, vec!["table:\"a\"", "table:\"b\""]);
        assert_eq!(acks.iter().find(|a| a.code == "TABLES_SKIPPED_EXISTING").unwrap().key, "TABLES_SKIPPED_EXISTING");
        p.plan.items[1].warnings.clear();
        let k2 = compute_acks(&p.plan).into_iter().find(|a| a.code == "SKIPPED_DIFFERS").unwrap();
        assert_ne!(k1.key, k2.key, "a different item set is a different key");

        // plan_id: env issues and production confirmation do not change it.
        let id = ServerIdentity { key: "k".into(), ..ServerIdentity::default() };
        let base = plan_id(&p.plan, &id, &id);
        let mut q = p.plan.clone();
        q.warnings.push(issue("TARGET_IN_USE", Severity::Danger, "x"));
        q.blockers.push(issue("PRODUCTION_CONFIRM", Severity::Danger, "x"));
        q.items[0].target_has_rows = Some(false);
        q.items[0].est_rows = Some(999);
        assert_eq!(plan_id(&q, &id, &id), base);
        q.items[0].action = PlanAction::Truncate;
        assert_ne!(plan_id(&q, &id, &id), base);
        assert_eq!(base.len(), 32);
        assert_eq!(plan_diff(&p.plan, &q), vec!["table:\"a\"".to_string()]);
    }

    /// A source monitor that reports sessions on `orders` (or fails).
    struct FakeMonitor {
        fail: bool,
        asked: std::sync::Mutex<Vec<crate::types::TableRef>>,
    }

    #[async_trait::async_trait]
    impl crate::monitor::Monitor for FakeMonitor {
        async fn snapshot(&self, _job: &crate::monitor::JobHandle) -> Result<crate::monitor::ServerSnapshot> {
            Ok(Default::default())
        }
        async fn active_sessions(&self, touching: &[crate::types::TableRef]) -> Result<Vec<crate::types::SessionInfo>> {
            self.asked.lock().unwrap().extend(touching.iter().cloned());
            if self.fail {
                return Err(crate::error::CoreError::Query { code: None, msg: "permission denied for pg_stat_activity".into() });
            }
            Ok(touching
                .iter()
                .filter(|t| t.name == "orders")
                .map(|t| crate::types::SessionInfo { pid: "42".into(), user: Some("app".into()), tables: vec![t.qualified()], ..Default::default() })
                .collect())
        }
    }

    #[tokio::test]
    async fn source_sessions_on_copied_tables_are_reported_like_export() {
        let load = |id: &str, name: &str, action: PlanAction| {
            let mut i = item(id, vec![]);
            i.schema = Some("app".into());
            i.name = name.into();
            i.action = action;
            i
        };
        let mut p = plan_with(vec![
            load("table:\"app\".\"orders\"", "orders", PlanAction::Create),
            load("table:\"app\".\"items\"", "items", PlanAction::Append),
            // Skipped and excluded tables are not read.
            load("table:\"app\".\"old\"", "old", PlanAction::SkipExisting),
            load("table:\"app\".\"gone\"", "gone", PlanAction::Excluded),
        ]);
        p.plan.family = CopyFamily::Postgres;
        let names = |m: &FakeMonitor| m.asked.lock().unwrap().iter().map(|t| t.qualified()).collect::<Vec<_>>();
        let m = FakeMonitor { fail: false, asked: Default::default() };
        let issues = source_session_issues(&m, &p.plan).await;
        assert_eq!(names(&m), ["app.orders", "app.items"]);
        assert_eq!(issues.len(), 1);
        let i = &issues[0];
        assert_eq!((i.code.as_str(), i.severity), ("CONCURRENT_SESSION", Severity::Warning), "same code and severity as export");
        assert!(i.detail.as_deref().unwrap().contains("pid 42 user app on app.orders"), "{:?}", i.detail);
        assert!(i.item_id.is_none());
        // Listing failures are only info.
        let m = FakeMonitor { fail: true, asked: Default::default() };
        let issues = source_session_issues(&m, &p.plan).await;
        assert_eq!(issues.iter().map(|i| (i.code.as_str(), i.severity)).collect::<Vec<_>>(), [("SESSIONS_UNKNOWN", Severity::Info)]);
        // Nothing read: nothing asked.
        p.plan.items.retain(|i| i.action == PlanAction::SkipExisting);
        let m = FakeMonitor { fail: false, asked: Default::default() };
        assert!(source_session_issues(&m, &p.plan).await.is_empty());
        assert!(names(&m).is_empty());

        // In finalize: a warning that needs an acknowledgement, env (not in plan_id); likewise
        // WORK_MEM_HIGH (danger) is acknowledged, not a blocker, as in export.
        let src = Connection { id: "s".into(), name: "src".into(), driver: "sqlite".into(), ..Connection::default() };
        let tgt = Connection { id: "t".into(), name: "tgt".into(), driver: "sqlite".into(), write_allowed: Some(true), ..Connection::default() };
        let dir = tempfile::tempdir().unwrap();
        let mut tf = TargetFacts::new(CopyFamily::Sqlite, "sqlite", "3.45", "main");
        tf.endpoint = dir.path().join("t.db").to_string_lossy().into_owned();
        let mut sf = tf.clone();
        sf.endpoint = dir.path().join("s.db").to_string_lossy().into_owned();
        let sid = identity(&sf);
        let mut plain = plan_with(vec![]);
        finalize(&mut plain, &FinalizeInput::new(&src, &tgt, &sid, &tf)).unwrap();
        let mut with = plan_with(vec![]);
        let mut fi = FinalizeInput::new(&src, &tgt, &sid, &tf);
        let session = from_preflight(crate::preflight::concurrent_session_issue(
            &[crate::types::SessionInfo { pid: "7".into(), ..Default::default() }],
            &[crate::types::TableRef::new(None, "t")],
        ));
        let work_mem = from_preflight(crate::preflight::check_work_mem("4GB", 8 << 30).expect("too high"));
        assert_eq!(work_mem.severity, Severity::Danger);
        fi.extra = vec![session, work_mem];
        finalize(&mut with, &fi).unwrap();
        assert!(with.plan.blockers.is_empty(), "{:?}", with.plan.blockers);
        let acked: Vec<&str> = with.plan.acks.iter().map(|a| a.key.as_str()).collect();
        assert!(acked.contains(&"CONCURRENT_SESSION") && acked.contains(&"WORK_MEM_HIGH"), "{acked:?}");
        assert_eq!(with.plan.plan_id, plain.plan.plan_id);
    }

    #[test]
    fn finalize_adds_safety_issues() {
        let src = Connection { id: "s".into(), name: "src".into(), driver: "sqlite".into(), ..Connection::default() };
        let mut tgt = Connection { id: "t".into(), name: "Prod DB".into(), driver: "sqlite".into(), tag: Some(ConnTag::Production), ..Connection::default() };
        let dir = tempfile::tempdir().unwrap();
        let mut tf = TargetFacts::new(CopyFamily::Sqlite, "sqlite", "3.45", "main");
        tf.endpoint = dir.path().join("t.db").to_string_lossy().into_owned();
        tf.identity = identity(&tf);
        let mut sf = tf.clone();
        sf.endpoint = dir.path().join("s.db").to_string_lossy().into_owned();
        let sid = identity(&sf);
        // Not write-allowed + production at plan time.
        let mut p = plan_with(vec![]);
        finalize(&mut p, &FinalizeInput::new(&src, &tgt, &sid, &tf)).unwrap();
        assert!(p.plan.blockers.iter().any(|b| b.code == "TARGET_NOT_WRITABLE"));
        assert!(p.plan.warnings.iter().any(|b| b.code == "PRODUCTION_CONFIRM" && b.severity == Severity::Info));
        let planned_id = p.plan.plan_id.clone();
        // Start time: a wrong name blocks, the right one passes; plan_id unchanged.
        tgt.write_allowed = Some(true);
        let mut p2 = plan_with(vec![]);
        let mut fi = FinalizeInput::new(&src, &tgt, &sid, &tf);
        fi.at_start = true;
        fi.confirm_production = Some("prod");
        finalize(&mut p2, &fi).unwrap();
        assert!(p2.plan.blockers.iter().any(|b| b.code == "PRODUCTION_CONFIRM"));
        let mut p3 = plan_with(vec![]);
        fi.confirm_production = Some("Prod DB");
        finalize(&mut p3, &fi).unwrap();
        assert!(p3.plan.blockers.is_empty(), "{:?}", p3.plan.blockers);
        assert_eq!(p3.plan.plan_id, planned_id, "the same request planned and started has the same plan_id");
        // Same file: SAME_DATABASE.
        let mut p4 = plan_with(vec![]);
        finalize(&mut p4, &FinalizeInput::new(&src, &tgt, &tf.identity.clone(), &tf)).unwrap();
        assert!(p4.plan.blockers.iter().any(|b| b.code == "SAME_DATABASE"));
        // Same connection and database always.
        let mut p5 = plan_with(vec![]);
        finalize(&mut p5, &FinalizeInput::new(&tgt, &tgt, &sid, &tf)).unwrap();
        assert!(p5.plan.blockers.iter().any(|b| b.code == "SAME_DATABASE"));
    }

    #[test]
    fn enum_columns_and_their_error_value() {
        let ddl = "CREATE TABLE `t` (\n  `id` int(11) NOT NULL,\n  `state` enum('Y','N') NOT NULL default 'Y',\n  `kind` ENUM('', 'a''b', 'c)') default NULL,\n  `x``y` enum('\\'',',') default NULL,\n  `note` varchar(10) default 'enum(''z'')'\n) TYPE=MyISAM";
        assert_eq!(mysql_enum_columns(ddl), vec![("state".to_string(), false), ("kind".to_string(), true), ("x`y".to_string(), false)]);
        let mut p = plan_with(vec![item("table:\"t\"", vec![])]);
        p.plan.family = CopyFamily::Mysql;
        p.plan.items[0].source_ddl = Some(ddl.to_string());
        let col = |s: &str, t: &str, v: ValueKind| DataColumn {
            source: s.into(),
            target: t.into(),
            value: v,
            read: SourceRead::Plain,
            zero_date: ZeroDateAction::NotTemporal,
            invalid_date: ZeroDateAction::NotTemporal,
            binary_attr: false,
            max_bytes: None,
        };
        p.tables.insert(
            "table:\"t\"".into(),
            TableDataSpec {
                item_id: "table:\"t\"".into(),
                source_sql: String::new(),
                verify_sql: String::new(),
                raw_csv: false,
                transcode: None,
                target: TargetTable { schema: None, name: "t".into() },
                columns: vec![
                    col("id", "id", ValueKind::Integer),
                    col("state", "status", ValueKind::Text),
                    col("kind", "kind", ValueKind::Text),
                    col("x`y", "xy", ValueKind::Binary),
                ],
                est_rows: None,
                first_timestamp_auto: false,
                fk_checks: false,
            },
        );
        // `kind` has a '' member; `x`y` is read as bytes: only `state` (target name `status`).
        let m = mysql_enum_error_columns(&p);
        assert_eq!(m.get("table:\"t\""), Some(&vec!["status".to_string()]));
    }

    #[test]
    fn unknown_exclusions_and_overrides_are_reported() {
        let p = plan_with(vec![item("table:\"app\".\"orders\"", vec![])]);
        let mut o = CopyOptions::default();
        assert!(unknown_item_issues(&p.plan, &o).is_empty());
        o.exclude = vec!["table:\"app\".\"orders\"".into(), "table:\"orders\"".into()];
        o.existing_overrides = vec![ItemPolicy { item_id: "table:\"customers\"".into(), policy: ExistingTablePolicy::Skip }];
        let i = unknown_item_issues(&p.plan, &o);
        assert_eq!(i.len(), 1);
        assert_eq!(i[0].code, "UNKNOWN_ITEM");
        assert_eq!(i[0].detail.as_deref(), Some("table:\"orders\"\ntable:\"customers\""));
    }

    /// A PG standby source whose identifier is readable, written into a primary whose
    /// identifier is not: SOURCE_IS_REPLICA, and SAME_SERVER_UNSURE under the same name.
    #[test]
    fn standby_source_with_a_one_sided_identifier_is_flagged() {
        let src = Connection { id: "s".into(), name: "standby".into(), driver: "postgres".into(), ..Connection::default() };
        let tgt = Connection { id: "t".into(), name: "primary".into(), driver: "postgres".into(), write_allowed: Some(true), ..Connection::default() };
        let mut sf = facts(CopyFamily::Postgres, &[("system_identifier", "7001"), ("start_time", "t2"), ("port", "5433"), ("pg_is_in_recovery", "true")]);
        sf.identity = identity(&sf);
        let mut tf = facts(CopyFamily::Postgres, &[("start_time", "t1"), ("port", "5432")]);
        tf.identity = identity(&tf);
        let mut p = plan_with(vec![]);
        p.plan.family = CopyFamily::Postgres;
        p.plan.source.database = "db".into();
        p.plan.target.database = "db".into();
        let mut fi = FinalizeInput::new(&src, &tgt, &sf.identity, &tf);
        fi.source = Some(&sf);
        finalize(&mut p, &fi).unwrap();
        let codes: Vec<&str> = p.plan.warnings.iter().map(|w| w.code.as_str()).collect();
        assert!(codes.contains(&"SOURCE_IS_REPLICA"), "{codes:?}");
        assert!(codes.contains(&"SAME_SERVER_UNSURE"), "{codes:?}");
        assert!(p.plan.blockers.is_empty(), "{:?}", p.plan.blockers);
        // Both identifiers readable and different: neither.
        let mut tf2 = facts(CopyFamily::Postgres, &[("system_identifier", "8001"), ("start_time", "t1"), ("port", "5432")]);
        tf2.identity = identity(&tf2);
        let mut p2 = plan_with(vec![]);
        p2.plan.family = CopyFamily::Postgres;
        let mut fi = FinalizeInput::new(&src, &tgt, &sf.identity, &tf2);
        fi.source = Some(&sf);
        finalize(&mut p2, &fi).unwrap();
        assert!(!p2.plan.warnings.iter().any(|w| matches!(w.code.as_str(), "SOURCE_IS_REPLICA" | "SAME_SERVER_UNSURE")), "{:?}", p2.plan.warnings);
    }

    #[test]
    fn mysql_privileges() {
        let mut p = crate::copy::testkit::empty_plan(CopyFamily::Mysql);
        p.target.database = "shop".into();
        let g = |rows: &[&str]| MetaTable { columns: vec!["Grants".into()], rows: rows.iter().map(|r| vec![Some(r.to_string())]).collect() };
        assert!(mysql_privilege_issue(&g(&["GRANT ALL PRIVILEGES ON *.* TO `root`@`%`"]), "shop", &p).is_none());
        assert!(mysql_privilege_issue(&g(&["GRANT USAGE ON *.* TO `u`@`%`", "GRANT SELECT, INSERT, CREATE, ALTER, INDEX ON `shop`.* TO `u`@`%`"]), "shop", &p).is_none());
        let i = mysql_privilege_issue(&g(&["GRANT USAGE ON *.* TO `u`@`%`", "GRANT SELECT, INSERT ON `shop`.* TO `u`@`%`"]), "shop", &p).unwrap();
        assert!(i.message.contains("CREATE") && i.message.contains("ALTER"));
        assert!(mysql_privilege_issue(&g(&["GRANT `app_role`@`%` TO `u`@`%`"]), "shop", &p).is_none(), "unknown: no issue");
        assert!(mysql_privilege_issue(&g(&["GRANT INSERT ON `sh\\_p`.* TO `u`@`%`"]), "shop", &p).is_none(), "pattern does not match shop -> unknown");
        assert!(mysql_privilege_issue(&MetaTable::default(), "shop", &p).is_none());
    }

    #[test]
    fn canonical_json_sorts_keys() {
        assert_eq!(canonical_json(&json!({"b": 1, "a": [true, null, "x"]})), r#"{"a":[true,null,"x"],"b":1}"#);
        assert_eq!(pg_literal("it's"), "'it''s'");
        assert_eq!(pg_literal("a\\b"), "E'a\\\\b'");
    }
}
