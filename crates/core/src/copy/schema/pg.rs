//! PostgreSQL -> PostgreSQL planning (3.5). The catalog is read per schema on the read-only
//! plan session under `SET search_path = <schema>, pg_catalog` with the pretty `pg_get_*def`
//! variants; every catalog query starts with a `/* bulkdb:pg:<name> */` tag (logs, tests).
//!
//! Never copied: system schemas, objects that belong to an extension (`CREATE EXTENSION`
//! creates them), partition indexes, inherited constraints / triggers, internal triggers,
//! invalid indexes.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use crate::copy::plan::{
    check_cancel, meta_bool, selected, target_schema_name, target_table_name, Collected, Draft, FamilyOps, PlanInput, TClass, TCol, TFk, TKey, TObj, TargetState,
};
use crate::copy::translate::pg::*;
use crate::copy::translate::{danger, dq, dq2, info, item_id, item_id_of, pg_str, warning, IssueExt, SOURCE_VALUE, TARGET_VALUE};
use crate::copy::types::*;
use crate::error::{CoreError, Result};
use crate::types::Severity;

// =============================================================================================
// Catalog SQL
// =============================================================================================

const GRANTS: &str = "(SELECT json_agg(json_build_array(CASE WHEN a.grantee = 0 THEN 'PUBLIC' ELSE pg_catalog.pg_get_userbyid(a.grantee) END, a.privilege_type)) FROM pg_catalog.aclexplode({acl}) a WHERE a.grantee <> {owner})::text";

fn grants(acl: &str, owner: &str) -> String {
    GRANTS.replace("{acl}", acl).replace("{owner}", owner)
}

/// Version and encoding (ASCII only): a SQL_ASCII source switches the catalog reader to raw
/// bytes before any text column is read (3.5).
pub(crate) const Q_VERSION: &str = "/* bulkdb:pg:version */ SELECT current_setting('server_version_num') AS version_num, current_setting('server_encoding') AS encoding";

fn q_facts(v: u16) -> String {
    Q_FACTS.replace("{loc}", if v >= 15 { "d.datlocprovider::text" } else { "NULL::text" })
}

const Q_FACTS: &str = "/* bulkdb:pg:facts */ SELECT current_setting('server_version_num') AS version_num, current_setting('server_encoding') AS encoding, \
current_user AS usr, r.rolsuper AS superuser, r.rolbypassrls AS bypassrls, d.datcollate AS datcollate, d.datctype AS datctype, \
{loc} AS locprovider, current_setting('lc_monetary') AS lc_monetary, \
EXISTS (SELECT 1 FROM pg_catalog.pg_largeobject_metadata) AS has_lo \
FROM pg_catalog.pg_roles r, pg_catalog.pg_database d WHERE r.rolname = current_user AND d.datname = current_database()";

fn q_schemas() -> String {
    format!(
        "/* bulkdb:pg:schemas */ SELECT n.oid::text AS oid, n.nspname AS name, pg_catalog.pg_get_userbyid(n.nspowner) AS owner, \
pg_catalog.obj_description(n.oid, 'pg_namespace') AS comment, {} AS grants FROM pg_catalog.pg_namespace n \
WHERE n.nspname NOT IN ('pg_catalog', 'information_schema', 'pg_toast') AND n.nspname NOT LIKE 'pg\\_temp\\_%' AND n.nspname NOT LIKE 'pg\\_toast\\_temp\\_%' \
AND NOT EXISTS (SELECT 1 FROM pg_catalog.pg_depend d WHERE d.classid = 'pg_catalog.pg_namespace'::regclass AND d.objid = n.oid AND d.deptype = 'e') ORDER BY n.nspname",
        grants("n.nspacl", "n.nspowner")
    )
}

pub(crate) const Q_EXTENSIONS: &str = "/* bulkdb:pg:extensions */ SELECT e.oid::text AS oid, e.extname AS name, n.nspname AS schema, e.extversion AS version, \
(SELECT json_agg(json_build_array(cn.nspname, c.relname, COALESCE(u.cond, ''))) FROM unnest(e.extconfig, e.extcondition) AS u(cfg, cond) \
JOIN pg_catalog.pg_class c ON c.oid = u.cfg JOIN pg_catalog.pg_namespace cn ON cn.oid = c.relnamespace)::text AS config \
FROM pg_catalog.pg_extension e JOIN pg_catalog.pg_namespace n ON n.oid = e.extnamespace ORDER BY e.extname";

pub(crate) const Q_EXT_MEMBERS: &str = "/* bulkdb:pg:ext_members */ SELECT d.classid::regclass::text AS cls, d.objid::text AS objid, d.refobjid::text AS ext \
FROM pg_catalog.pg_depend d WHERE d.deptype = 'e' AND d.refclassid = 'pg_catalog.pg_extension'::regclass";

pub(crate) const Q_DEPS: &str = "/* bulkdb:pg:deps */ SELECT CASE WHEN d.classid IN ('pg_catalog.pg_rewrite'::regclass, 'pg_catalog.pg_attrdef'::regclass) THEN 'pg_class' ELSE d.classid::regclass::text END AS cls, \
CASE WHEN d.classid = 'pg_catalog.pg_rewrite'::regclass THEN (SELECT r.ev_class::text FROM pg_catalog.pg_rewrite r WHERE r.oid = d.objid) \
WHEN d.classid = 'pg_catalog.pg_attrdef'::regclass THEN (SELECT a.adrelid::text FROM pg_catalog.pg_attrdef a WHERE a.oid = d.objid) ELSE d.objid::text END AS objid, \
d.refclassid::regclass::text AS refcls, d.refobjid::text AS refobjid FROM pg_catalog.pg_depend d \
WHERE d.deptype = 'n' AND d.refobjid >= 16384 \
AND d.classid IN ('pg_catalog.pg_class'::regclass, 'pg_catalog.pg_proc'::regclass, 'pg_catalog.pg_type'::regclass, 'pg_catalog.pg_rewrite'::regclass, 'pg_catalog.pg_constraint'::regclass, 'pg_catalog.pg_attrdef'::regclass, 'pg_catalog.pg_trigger'::regclass, 'pg_catalog.pg_policy'::regclass) \
AND d.refclassid IN ('pg_catalog.pg_class'::regclass, 'pg_catalog.pg_proc'::regclass, 'pg_catalog.pg_type'::regclass, 'pg_catalog.pg_collation'::regclass)";

pub(crate) const Q_ROLE_SETTINGS: &str = "/* bulkdb:pg:role_settings */ SELECT COALESCE(pg_catalog.pg_get_userbyid(NULLIF(s.setrole, 0)), 'all roles') AS role, \
array_to_json(s.setconfig)::text AS config FROM pg_catalog.pg_db_role_setting s WHERE s.setdatabase = (SELECT oid FROM pg_catalog.pg_database WHERE datname = current_database())";

pub(crate) const Q_DB_UNSUPPORTED: &str =
    "/* bulkdb:pg:db_unsupported */ SELECT 'event trigger' AS kind, evtname::text AS name, 'pg_event_trigger' AS cls, oid::text AS oid FROM pg_catalog.pg_event_trigger \
UNION ALL SELECT 'publication', pubname::text, 'pg_publication', oid::text FROM pg_catalog.pg_publication \
UNION ALL SELECT 'foreign server', srvname::text, 'pg_foreign_server', oid::text FROM pg_catalog.pg_foreign_server \
UNION ALL SELECT 'cast', castsource::regtype::text || ' AS ' || casttarget::regtype::text, 'pg_cast', oid::text FROM pg_catalog.pg_cast WHERE oid >= 16384";

fn q_collations(s: &str) -> String {
    format!("/* bulkdb:pg:collations */ SELECT c.oid::text AS oid, c.collname AS name, to_jsonb(c)::text AS j, pg_catalog.pg_get_userbyid(c.collowner) AS owner, pg_catalog.obj_description(c.oid, 'pg_collation') AS comment FROM pg_catalog.pg_collation c JOIN pg_catalog.pg_namespace n ON n.oid = c.collnamespace WHERE n.nspname = {}", pg_str(s))
}

fn q_types(s: &str) -> String {
    format!(
        "/* bulkdb:pg:types */ SELECT t.oid::text AS oid, t.typname AS name, t.typtype AS type, \
(SELECT json_agg(e.enumlabel ORDER BY e.enumsortorder) FROM pg_catalog.pg_enum e WHERE e.enumtypid = t.oid)::text AS labels, \
CASE WHEN t.typtype = 'd' THEN pg_catalog.format_type(t.typbasetype, t.typtypmod) END AS base, t.typnotnull AS notnull, t.typdefault AS dflt, \
CASE WHEN t.typtype = 'd' AND t.typcollation <> 0 AND t.typcollation <> bt.typcollation THEN pg_catalog.quote_ident(cn.nspname) || '.' || pg_catalog.quote_ident(co.collname) END AS collation, \
(SELECT json_agg(json_build_array(a.attname, pg_catalog.format_type(a.atttypid, a.atttypmod), CASE WHEN a.attcollation <> atp.typcollation THEN pg_catalog.quote_ident(acn.nspname) || '.' || pg_catalog.quote_ident(aco.collname) END) ORDER BY a.attnum) \
FROM pg_catalog.pg_attribute a JOIN pg_catalog.pg_type atp ON atp.oid = a.atttypid LEFT JOIN pg_catalog.pg_collation aco ON aco.oid = a.attcollation LEFT JOIN pg_catalog.pg_namespace acn ON acn.oid = aco.collnamespace \
WHERE t.typtype = 'c' AND a.attrelid = t.typrelid AND a.attnum > 0 AND NOT a.attisdropped)::text AS attrs, \
(SELECT json_build_array(pg_catalog.format_type(r.rngsubtype, NULL), CASE WHEN r.rngsubdiff::oid <> 0 THEN r.rngsubdiff::regproc::text END, \
(SELECT pn.nspname FROM pg_catalog.pg_proc p JOIN pg_catalog.pg_namespace pn ON pn.oid = p.pronamespace WHERE p.oid = r.rngsubdiff::oid)) FROM pg_catalog.pg_range r WHERE r.rngtypid = t.oid)::text AS range, \
pg_catalog.pg_get_userbyid(t.typowner) AS owner, pg_catalog.obj_description(t.oid, 'pg_type') AS comment, {} AS grants \
FROM pg_catalog.pg_type t JOIN pg_catalog.pg_namespace n ON n.oid = t.typnamespace LEFT JOIN pg_catalog.pg_type bt ON bt.oid = t.typbasetype \
LEFT JOIN pg_catalog.pg_collation co ON co.oid = t.typcollation LEFT JOIN pg_catalog.pg_namespace cn ON cn.oid = co.collnamespace \
WHERE n.nspname = {} AND (t.typtype IN ('e', 'd', 'r') OR (t.typtype = 'c' AND (SELECT c.relkind FROM pg_catalog.pg_class c WHERE c.oid = t.typrelid) = 'c')) ORDER BY t.typname",
        grants("t.typacl", "t.typowner"),
        pg_str(s)
    )
}

fn q_sequences(s: &str) -> String {
    format!(
        "/* bulkdb:pg:sequences */ SELECT c.oid::text AS oid, c.relname AS name, pg_catalog.format_type(s.seqtypid, NULL) AS type, s.seqstart AS start, s.seqincrement AS increment, \
s.seqmax AS max, s.seqmin AS min, s.seqcache AS cache, s.seqcycle AS cycle, d.deptype AS deptype, own.nspname AS owner_schema, oc.relname AS owner_table, a.attname AS owner_column, \
pg_catalog.pg_get_userbyid(c.relowner) AS owner, pg_catalog.obj_description(c.oid, 'pg_class') AS comment, {} AS grants \
FROM pg_catalog.pg_sequence s JOIN pg_catalog.pg_class c ON c.oid = s.seqrelid JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
LEFT JOIN pg_catalog.pg_depend d ON d.classid = 'pg_catalog.pg_class'::regclass AND d.objid = c.oid AND d.refclassid = 'pg_catalog.pg_class'::regclass AND d.deptype IN ('a', 'i') \
LEFT JOIN pg_catalog.pg_class oc ON oc.oid = d.refobjid LEFT JOIN pg_catalog.pg_namespace own ON own.oid = oc.relnamespace \
LEFT JOIN pg_catalog.pg_attribute a ON a.attrelid = d.refobjid AND a.attnum = d.refobjsubid WHERE n.nspname = {} ORDER BY c.relname",
        grants("c.relacl", "c.relowner"),
        pg_str(s)
    )
}

fn q_functions(s: &str, v: u16) -> String {
    let kind = if v >= 11 { "p.prokind::text" } else { "CASE WHEN p.proisagg THEN 'a' WHEN p.proiswindow THEN 'w' ELSE 'f' END" };
    format!(
        "/* bulkdb:pg:functions */ SELECT f.*, CASE WHEN f.kind <> 'a' THEN pg_catalog.pg_get_functiondef(f.oid::oid) END AS def FROM (SELECT p.oid::text AS oid, p.proname AS name, \
pg_catalog.pg_get_function_identity_arguments(p.oid) AS args, \
{kind} AS kind, \
pg_catalog.quote_ident(n.nspname) || '.' || pg_catalog.quote_ident(p.proname) AS qname, p.prosecdef AS secdef, array_to_json(p.proconfig)::text AS config, \
pg_catalog.pg_get_userbyid(p.proowner) AS owner, pg_catalog.obj_description(p.oid, 'pg_proc') AS comment, {} AS grants \
FROM pg_catalog.pg_proc p JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace WHERE n.nspname = {}) f ORDER BY f.name, f.args",
        grants("p.proacl", "p.proowner"),
        pg_str(s)
    )
}

fn q_relations(s: &str) -> String {
    format!(
        "/* bulkdb:pg:relations */ SELECT c.oid::text AS oid, c.relname AS name, c.relkind AS kind, c.relpersistence AS persistence, c.relispartition AS is_partition, \
pg_catalog.pg_get_expr(c.relpartbound, c.oid, true) AS bound, CASE WHEN c.relkind = 'p' THEN pg_catalog.pg_get_partkeydef(c.oid) END AS partkey, \
CASE WHEN c.relkind = 'p' THEN (SELECT json_agg(CASE WHEN k.coll = 100 THEN 'default' ELSE '' END ORDER BY k.ord) FROM pg_catalog.pg_partitioned_table pt, \
unnest(pt.partcollation::oid[]) WITH ORDINALITY k(coll, ord) WHERE pt.partrelid = c.oid)::text END AS part_colls, \
array_to_json(c.reloptions)::text AS reloptions, c.relrowsecurity AS rls, c.relforcerowsecurity AS force_rls, c.relreplident AS replident, c.reltuples::bigint AS est_rows, \
CASE WHEN c.relkind IN ('r', 'p', 'm') THEN pg_catalog.pg_total_relation_size(c.oid) END AS bytes, pg_catalog.pg_get_userbyid(c.relowner) AS owner, \
pg_catalog.pg_has_role(c.relowner, 'USAGE') AS is_owner, pg_catalog.obj_description(c.oid, 'pg_class') AS comment, c.reltablespace <> 0 AS has_tablespace, \
CASE WHEN c.relkind IN ('v', 'm') THEN pg_catalog.pg_get_viewdef(c.oid, true) END AS viewdef, \
(SELECT json_agg(json_build_array(pn.nspname, pc.relname) ORDER BY i.inhseqno) FROM pg_catalog.pg_inherits i JOIN pg_catalog.pg_class pc ON pc.oid = i.inhparent \
JOIN pg_catalog.pg_namespace pn ON pn.oid = pc.relnamespace WHERE i.inhrelid = c.oid)::text AS parents, \
EXISTS (SELECT 1 FROM pg_catalog.pg_depend d WHERE d.classid = 'pg_catalog.pg_class'::regclass AND d.objid = c.oid AND d.deptype = 'e') AS ext_member, \
c.reloftype <> 0 AS typed, c.reltype::text AS rowtype, {} AS grants \
FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace WHERE n.nspname = {} AND c.relkind IN ('r', 'p', 'v', 'm', 'f') ORDER BY c.relname",
        grants("c.relacl", "c.relowner"),
        pg_str(s)
    )
}

fn q_columns(s: &str, v: u16) -> String {
    let generated = if v >= 12 { "a.attgenerated::text" } else { "''::text" };
    let compression = if v >= 14 { "a.attcompression::text" } else { "NULL::text" };
    format!(
        "/* bulkdb:pg:columns */ SELECT a.attrelid::text AS rel, a.attnum AS num, a.attname AS name, pg_catalog.format_type(a.atttypid, a.atttypmod) AS type, a.attnotnull AS notnull, \
pg_catalog.pg_get_expr(ad.adbin, ad.adrelid, true) AS dflt, a.attidentity::text AS identity, {generated} AS generated, a.attislocal AS local, \
CASE WHEN a.attcollation <> t.typcollation AND a.attcollation <> 0 THEN pg_catalog.quote_ident(cn.nspname) || '.' || pg_catalog.quote_ident(co.collname) END AS collation, \
cn.nspname AS coll_schema, co.collname AS coll_name, COALESCE(a.attstattarget::text, '-1') AS stattarget, a.attstorage AS storage, t.typstorage AS typstorage, \
{compression} AS compression, pg_catalog.col_description(a.attrelid, a.attnum) AS comment, t.typname AS typname, a.attcollation = 100 AS dflt_coll \
FROM pg_catalog.pg_attribute a JOIN pg_catalog.pg_class c ON c.oid = a.attrelid JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace JOIN pg_catalog.pg_type t ON t.oid = a.atttypid \
LEFT JOIN pg_catalog.pg_attrdef ad ON ad.adrelid = a.attrelid AND ad.adnum = a.attnum \
LEFT JOIN pg_catalog.pg_collation co ON co.oid = a.attcollation AND a.attcollation <> t.typcollation LEFT JOIN pg_catalog.pg_namespace cn ON cn.oid = co.collnamespace \
WHERE n.nspname = {} AND c.relkind IN ('r', 'p', 'v', 'm', 'f') AND a.attnum > 0 AND NOT a.attisdropped ORDER BY a.attrelid, a.attnum",
        pg_str(s)
    )
}

fn q_constraints(s: &str, v: u16) -> String {
    let enforced = if v >= 18 { "con.conenforced::text" } else { "NULL::text" };
    format!(
        "/* bulkdb:pg:constraints */ SELECT con.oid::text AS oid, con.conrelid::text AS rel, con.contypid::text AS typ, con.conname AS name, con.contype AS type, \
pg_catalog.pg_get_constraintdef(con.oid, true) AS def, con.convalidated AS validated, con.conislocal AS local, {enforced} AS enforced, \
array_to_json(con.conkey)::text AS keys, fn.nspname AS fschema, fc.relname AS ftable, pg_catalog.obj_description(con.oid, 'pg_constraint') AS comment \
FROM pg_catalog.pg_constraint con JOIN pg_catalog.pg_namespace n ON n.oid = con.connamespace LEFT JOIN pg_catalog.pg_class fc ON fc.oid = con.confrelid \
LEFT JOIN pg_catalog.pg_namespace fn ON fn.oid = fc.relnamespace WHERE n.nspname = {} ORDER BY con.conrelid, con.conname",
        pg_str(s)
    )
}

fn q_indexes(s: &str) -> String {
    format!(
        "/* bulkdb:pg:indexes */ SELECT i.indexrelid::text AS oid, i.indrelid::text AS rel, ic.relname AS name, pg_catalog.pg_get_indexdef(i.indexrelid, 0, true) AS def, \
i.indisvalid AS valid, i.indisclustered AS clustered, i.indisreplident AS replident, ic.relispartition AS is_partition, \
EXISTS (SELECT 1 FROM pg_catalog.pg_constraint c WHERE c.conindid = i.indexrelid AND c.conrelid = i.indrelid AND c.contype IN ('p', 'u', 'x')) AS backs_constraint, \
pg_catalog.obj_description(i.indexrelid, 'pg_class') AS comment, ic.reltablespace <> 0 AS has_tablespace, (i.indexprs IS NOT NULL OR i.indpred IS NOT NULL) AS has_exprs, \
EXISTS (SELECT 1 FROM pg_catalog.pg_depend d WHERE d.classid = 'pg_catalog.pg_class'::regclass AND d.objid = i.indexrelid AND d.deptype = 'e') AS ext_member \
FROM pg_catalog.pg_index i JOIN pg_catalog.pg_class ic ON ic.oid = i.indexrelid JOIN pg_catalog.pg_namespace n ON n.oid = ic.relnamespace WHERE n.nspname = {} ORDER BY ic.relname",
        pg_str(s)
    )
}

fn q_triggers(s: &str, v: u16) -> String {
    let parent = if v >= 13 { "t.tgparentid::text" } else { "'0'" };
    format!(
        "/* bulkdb:pg:triggers */ SELECT t.oid::text AS oid, t.tgrelid::text AS rel, t.tgname AS name, pg_catalog.pg_get_triggerdef(t.oid, true) AS def, t.tgenabled AS enabled, \
t.tgisinternal AS internal, {parent} AS parentid, pg_catalog.obj_description(t.oid, 'pg_trigger') AS comment, t.tgfoid::text AS func \
FROM pg_catalog.pg_trigger t JOIN pg_catalog.pg_class c ON c.oid = t.tgrelid JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace WHERE n.nspname = {} ORDER BY t.tgrelid, t.tgname",
        pg_str(s)
    )
}

fn q_policies(s: &str) -> String {
    format!(
        "/* bulkdb:pg:policies */ SELECT p.oid::text AS oid, p.polrelid::text AS rel, p.polname AS name, p.polcmd AS cmd, p.polpermissive AS permissive, \
(SELECT json_agg(CASE WHEN r = 0 THEN 'PUBLIC' ELSE pg_catalog.quote_ident(pg_catalog.pg_get_userbyid(r)) END) FROM unnest(p.polroles) r)::text AS roles, \
pg_catalog.pg_get_expr(p.polqual, p.polrelid, true) AS qual, pg_catalog.pg_get_expr(p.polwithcheck, p.polrelid, true) AS withcheck \
FROM pg_catalog.pg_policy p JOIN pg_catalog.pg_class c ON c.oid = p.polrelid JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace WHERE n.nspname = {}",
        pg_str(s)
    )
}

fn q_unsupported(s: &str) -> String {
    let n = pg_str(s);
    format!(
        "/* bulkdb:pg:unsupported */ SELECT 'rule' AS kind, c.relname || '.' || r.rulename AS name, 'pg_rewrite' AS cls, r.oid::text AS oid FROM pg_catalog.pg_rewrite r \
JOIN pg_catalog.pg_class c ON c.oid = r.ev_class JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace WHERE n.nspname = {n} AND r.rulename <> '_RETURN' \
UNION ALL SELECT 'operator', o.oprname::text, 'pg_operator', o.oid::text FROM pg_catalog.pg_operator o JOIN pg_catalog.pg_namespace n ON n.oid = o.oprnamespace WHERE n.nspname = {n} \
UNION ALL SELECT 'operator class', o.opcname::text, 'pg_opclass', o.oid::text FROM pg_catalog.pg_opclass o JOIN pg_catalog.pg_namespace n ON n.oid = o.opcnamespace WHERE n.nspname = {n} \
UNION ALL SELECT 'operator family', o.opfname::text, 'pg_opfamily', o.oid::text FROM pg_catalog.pg_opfamily o JOIN pg_catalog.pg_namespace n ON n.oid = o.opfnamespace WHERE n.nspname = {n} \
UNION ALL SELECT 'text search configuration', o.cfgname::text, 'pg_ts_config', o.oid::text FROM pg_catalog.pg_ts_config o JOIN pg_catalog.pg_namespace n ON n.oid = o.cfgnamespace WHERE n.nspname = {n} \
UNION ALL SELECT 'text search dictionary', o.dictname::text, 'pg_ts_dict', o.oid::text FROM pg_catalog.pg_ts_dict o JOIN pg_catalog.pg_namespace n ON n.oid = o.dictnamespace WHERE n.nspname = {n} \
UNION ALL SELECT 'statistics object', o.stxname::text, 'pg_statistic_ext', o.oid::text FROM pg_catalog.pg_statistic_ext o JOIN pg_catalog.pg_namespace n ON n.oid = o.stxnamespace WHERE n.nspname = {n} \
UNION ALL SELECT 'conversion', o.conname::text, 'pg_conversion', o.oid::text FROM pg_catalog.pg_conversion o JOIN pg_catalog.pg_namespace n ON n.oid = o.connamespace WHERE n.nspname = {n}"
    )
}

fn in_list(names: &[String]) -> String {
    if names.is_empty() {
        return "(NULL)".into();
    }
    format!("({})", names.iter().map(|n| pg_str(n)).collect::<Vec<_>>().join(", "))
}

// =============================================================================================
// Model
// =============================================================================================

type Row = BTreeMap<String, Option<String>>;

fn rows(m: &MetaTable) -> Vec<Row> {
    m.rows.iter().map(|r| m.columns.iter().cloned().zip(r.iter().cloned()).collect()).collect()
}

fn s<'a>(r: &'a Row, k: &str) -> &'a str {
    r.get(k).and_then(|v| v.as_deref()).unwrap_or("")
}

fn o(r: &Row, k: &str) -> Option<String> {
    r.get(k).cloned().flatten().filter(|v| !v.is_empty())
}

fn b(r: &Row, k: &str) -> bool {
    meta_bool(r.get(k).and_then(|v| v.as_deref())).unwrap_or(false)
}

fn i(r: &Row, k: &str) -> i128 {
    s(r, k).trim().parse::<i128>().unwrap_or(0)
}

fn json_strings(v: Option<String>) -> Vec<String> {
    v.and_then(|t| serde_json::from_str::<Vec<serde_json::Value>>(&t).ok())
        .map(|a| a.into_iter().map(|x| x.as_str().map(str::to_string).unwrap_or_else(|| x.to_string())).collect())
        .unwrap_or_default()
}

/// The column `name` of the first parent (`parents`: `[schema, table]` pairs) as the source
/// catalog has it, when that parent's schema was read.
fn parent_column<'a>(cats: &'a [SchemaCat], parents: &[Vec<Option<String>>], name: &str) -> Option<&'a Row> {
    let p = parents.first()?;
    let (ps, pn) = (p.first().cloned().flatten()?, p.get(1).cloned().flatten()?);
    let cat = cats.iter().find(|c| c.name == ps)?;
    let rel = cat.relations.iter().find(|r| s(r, "name") == pn)?;
    cat.columns.iter().find(|c| s(c, "rel") == s(rel, "oid") && s(c, "name") == name)
}

fn json_rows(v: Option<String>) -> Vec<Vec<Option<String>>> {
    v.and_then(|t| serde_json::from_str::<Vec<Vec<serde_json::Value>>>(&t).ok())
        .map(|a| a.into_iter().map(|r| r.into_iter().map(|x| x.as_str().map(str::to_string)).collect()).collect())
        .unwrap_or_default()
}

/// Everything read from one schema.
#[derive(Debug, Default)]
struct SchemaCat {
    name: String,
    row: Row,
    collations: Vec<Row>,
    types: Vec<Row>,
    sequences: Vec<Row>,
    functions: Vec<Row>,
    relations: Vec<Row>,
    columns: Vec<Row>,
    constraints: Vec<Row>,
    indexes: Vec<Row>,
    triggers: Vec<Row>,
    policies: Vec<Row>,
    unsupported: Vec<Row>,
}

fn ops() -> FamilyOps {
    FamilyOps {
        family: CopyFamily::Postgres,
        probe_sql: |k, partitioned| {
            format!("SELECT EXISTS (SELECT 1 FROM {}{}) AS has_rows", if partitioned { "" } else { "ONLY " }, dq2(k.schema.as_deref().unwrap_or("public"), &k.name))
        },
        count_sql: |k, n, partitioned| {
            format!(
                "SELECT COUNT(*) AS n FROM (SELECT 1 FROM {}{} LIMIT {n}) q",
                if partitioned { "" } else { "ONLY " },
                dq2(k.schema.as_deref().unwrap_or("public"), &k.name)
            )
        },
        drop_fk_sql: |k, name| Some(format!("ALTER TABLE {} DROP CONSTRAINT {}", dq2(k.schema.as_deref().unwrap_or("public"), &k.name), dq(name))),
        truncate_group_sql: Some(|keys| {
            format!(
                "TRUNCATE {}",
                keys.iter()
                    .map(|(k, partitioned)| format!("{}{}", if *partitioned { "" } else { "ONLY " }, dq2(k.schema.as_deref().unwrap_or("public"), &k.name)))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        }),
        quote_rel: |k| dq2(k.schema.as_deref().unwrap_or("public"), &k.name),
    }
}

// =============================================================================================
// Target state
// =============================================================================================

struct TargetPg {
    state: TargetState,
    facts: Row,
    schemas: BTreeMap<String, (bool, bool)>,
    collations: BTreeSet<(String, String)>,
    available: BTreeMap<String, (Option<String>, Option<String>, bool)>,
    roles: Option<BTreeSet<String>>,
}

async fn read_target(input: &PlanInput<'_>, tschemas: &[String], need_roles: bool, tmajor: u16) -> Result<TargetPg> {
    let t = input.target;
    let gen = if tmajor >= 12 { "a.attgenerated::text" } else { "''::text" };
    let trusted =
        if tmajor >= 13 { "COALESCE((SELECT bool_or(v.trusted) FROM pg_catalog.pg_available_extension_versions v WHERE v.name = a.name), false)" } else { "false" };
    let list = in_list(tschemas);
    let loc = if tmajor >= 15 { "d.datlocprovider::text" } else { "NULL::text" };
    let facts = rows(&t.query_meta(&format!("/* bulkdb:pg:t_facts */ SELECT current_setting('server_encoding') AS encoding, d.datcollate AS datcollate, d.datctype AS datctype, \
{loc} AS locprovider, current_setting('lc_monetary') AS lc_monetary, r.rolsuper AS superuser, \
has_database_privilege(current_database(), 'CREATE') AS can_create FROM pg_catalog.pg_database d, pg_catalog.pg_roles r WHERE d.datname = current_database() AND r.rolname = current_user")).await?)
    .into_iter()
    .next()
    .unwrap_or_default();
    let mut st = TargetState::default();
    let mut schemas = BTreeMap::new();
    for r in rows(&t.query_meta(&format!("/* bulkdb:pg:t_schemas */ SELECT n.nspname AS name, has_schema_privilege(n.oid, 'CREATE') AS can_create, has_schema_privilege(n.oid, 'USAGE') AS can_use FROM pg_catalog.pg_namespace n WHERE n.nspname IN {list}")).await?) {
        st.objects.insert(TKey::of(TClass::Schema, None, None, s(&r, "name")), TObj { kind: "schema".into(), ..Default::default() });
        schemas.insert(s(&r, "name").to_string(), (b(&r, "can_create"), b(&r, "can_use")));
    }
    let rels = rows(
        &t.query_meta(&format!(
            "/* bulkdb:pg:t_relations */ SELECT n.nspname AS schema, c.relname AS name, c.relkind AS kind, c.reltuples::bigint AS est_rows, \
CASE WHEN c.relkind IN ('r', 'p', 'f') THEN has_table_privilege(c.oid, 'INSERT') END AS can_insert, CASE WHEN c.relkind IN ('r', 'p') THEN has_table_privilege(c.oid, 'TRUNCATE') END AS can_truncate, pg_has_role(c.relowner, 'USAGE') AS is_owner, \
(SELECT count(*) FROM pg_catalog.pg_trigger g WHERE g.tgrelid = c.oid AND NOT g.tgisinternal) AS triggers \
FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace WHERE n.nspname IN {list} AND c.relkind IN ('r', 'p', 'v', 'm', 'f', 'S', 'i', 'I')"
        ))
        .await?,
    );
    for r in &rels {
        let (sc, name, kind) = (s(r, "schema"), s(r, "name"), s(r, "kind"));
        let class = match kind {
            "S" => TClass::Sequence,
            "i" | "I" => TClass::Index,
            _ => TClass::Relation,
        };
        let est = s(r, "est_rows").parse::<i64>().ok().filter(|v| *v >= 0).map(|v| v as u64);
        st.objects.insert(
            TKey::of(class, Some(sc), None, name),
            TObj {
                kind: kind.to_string(),
                est_rows: est,
                columns: Vec::new(),
                triggers: s(r, "triggers").parse().unwrap_or(0),
                can_insert: meta_bool(r.get("can_insert").and_then(|v| v.as_deref())),
                can_truncate: meta_bool(r.get("can_truncate").and_then(|v| v.as_deref())),
                is_owner: meta_bool(r.get("is_owner").and_then(|v| v.as_deref())),
                partitioned: kind == "p",
            },
        );
    }
    for r in rows(
        &t.query_meta(&format!(
            "/* bulkdb:pg:t_columns */ SELECT n.nspname AS schema, c.relname AS tbl, a.attname AS name, pg_catalog.format_type(a.atttypid, a.atttypmod) AS type, a.attnotnull AS notnull, \
(a.atthasdef OR a.attidentity <> '') AS has_default, {gen} AS generated \
FROM pg_catalog.pg_attribute a JOIN pg_catalog.pg_class c ON c.oid = a.attrelid JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
WHERE n.nspname IN {list} AND c.relkind IN ('r', 'p') AND a.attnum > 0 AND NOT a.attisdropped ORDER BY c.relname, a.attnum"
        ))
        .await?,
    ) {
        if !s(&r, "generated").is_empty() {
            continue;
        }
        if let Some(obj) = st.objects.get_mut(&TKey::rel(Some(s(&r, "schema")), s(&r, "tbl"))) {
            obj.columns.push(TCol { name: s(&r, "name").into(), type_text: s(&r, "type").into(), not_null: b(&r, "notnull"), has_default: b(&r, "has_default") });
        }
    }
    for r in rows(
        &t.query_meta(&format!(
            "/* bulkdb:pg:t_types */ SELECT n.nspname AS schema, t.typname AS name FROM pg_catalog.pg_type t JOIN pg_catalog.pg_namespace n ON n.oid = t.typnamespace \
WHERE n.nspname IN {list} AND (t.typtype IN ('e', 'd', 'r') OR (t.typtype = 'c' AND (SELECT c.relkind FROM pg_catalog.pg_class c WHERE c.oid = t.typrelid) = 'c'))"
        ))
        .await?,
    ) {
        st.objects.insert(TKey::of(TClass::Type, Some(s(&r, "schema")), None, s(&r, "name")), TObj { kind: "type".into(), ..Default::default() });
    }
    for r in rows(
        &t.query_meta(&format!(
            "/* bulkdb:pg:t_functions */ SELECT n.nspname AS schema, p.proname || '(' || pg_catalog.pg_get_function_identity_arguments(p.oid) || ')' AS name \
FROM pg_catalog.pg_proc p JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace WHERE n.nspname IN {list}"
        ))
        .await?,
    ) {
        st.objects.insert(TKey::of(TClass::Function, Some(s(&r, "schema")), None, s(&r, "name")), TObj { kind: "function".into(), ..Default::default() });
    }
    for r in rows(
        &t.query_meta(&format!(
            "/* bulkdb:pg:t_constraints */ SELECT n.nspname AS schema, COALESCE(c.relname, ty.typname) AS tbl, con.conname AS name, con.contype AS type, \
cn.nspname AS child_schema, c.relname AS child, pn.nspname AS parent_schema, p.relname AS parent \
FROM pg_catalog.pg_constraint con JOIN pg_catalog.pg_namespace n ON n.oid = con.connamespace LEFT JOIN pg_catalog.pg_class c ON c.oid = con.conrelid \
LEFT JOIN pg_catalog.pg_namespace cn ON cn.oid = c.relnamespace LEFT JOIN pg_catalog.pg_type ty ON ty.oid = con.contypid \
LEFT JOIN pg_catalog.pg_class p ON p.oid = con.confrelid LEFT JOIN pg_catalog.pg_namespace pn ON pn.oid = p.relnamespace \
WHERE n.nspname IN {list} OR (con.contype = 'f' AND pn.nspname IN {list})"
        ))
        .await?,
    ) {
        st.objects
            .insert(TKey::of(TClass::Constraint, Some(s(&r, "schema")), Some(s(&r, "tbl")), s(&r, "name")), TObj { kind: s(&r, "type").into(), ..Default::default() });
        if s(&r, "type") == "f" {
            st.fks.push(TFk {
                child: TKey::rel(Some(s(&r, "child_schema")), s(&r, "child")),
                name: s(&r, "name").into(),
                parent: TKey::rel(Some(s(&r, "parent_schema")), s(&r, "parent")),
            });
        }
    }
    for r in rows(
        &t.query_meta(&format!(
            "/* bulkdb:pg:t_triggers */ SELECT n.nspname AS schema, c.relname AS tbl, t.tgname AS name FROM pg_catalog.pg_trigger t JOIN pg_catalog.pg_class c ON c.oid = t.tgrelid \
JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace WHERE n.nspname IN {list} AND NOT t.tgisinternal \
UNION ALL SELECT n.nspname, c.relname, '#policy:' || p.polname FROM pg_catalog.pg_policy p JOIN pg_catalog.pg_class c ON c.oid = p.polrelid \
JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace WHERE n.nspname IN {list}"
        ))
        .await?,
    ) {
        let name = s(&r, "name");
        let (class, name) = match name.strip_prefix("#policy:") {
            Some(p) => (TClass::Policy, p),
            None => (TClass::Trigger, name),
        };
        st.objects.insert(TKey::of(class, Some(s(&r, "schema")), Some(s(&r, "tbl")), name), TObj::default());
    }
    for r in rows(
        &t.query_meta(&format!(
            "/* bulkdb:pg:t_view_deps */ SELECT DISTINCT vn.nspname AS vschema, v.relname AS view, rn.nspname AS rschema, rc.relname AS rel \
FROM pg_catalog.pg_depend d JOIN pg_catalog.pg_rewrite w ON d.classid = 'pg_catalog.pg_rewrite'::regclass AND d.objid = w.oid \
JOIN pg_catalog.pg_class v ON v.oid = w.ev_class JOIN pg_catalog.pg_namespace vn ON vn.oid = v.relnamespace \
JOIN pg_catalog.pg_class rc ON d.refclassid = 'pg_catalog.pg_class'::regclass AND rc.oid = d.refobjid JOIN pg_catalog.pg_namespace rn ON rn.oid = rc.relnamespace \
WHERE rc.oid <> v.oid AND rn.nspname IN {list}"
        ))
        .await?,
    ) {
        st.view_deps.push((TKey::rel(Some(s(&r, "vschema")), s(&r, "view")), TKey::rel(Some(s(&r, "rschema")), s(&r, "rel"))));
    }
    let mut collations = BTreeSet::new();
    for r in rows(&t.query_meta("/* bulkdb:pg:t_collations */ SELECT n.nspname AS schema, c.collname AS name FROM pg_catalog.pg_collation c JOIN pg_catalog.pg_namespace n ON n.oid = c.collnamespace").await?) {
        collations.insert((s(&r, "schema").to_string(), s(&r, "name").to_string()));
        st.objects.insert(TKey::of(TClass::Collation, Some(s(&r, "schema")), None, s(&r, "name")), TObj::default());
    }
    let mut available = BTreeMap::new();
    for r in rows(
        &t.query_meta(&format!(
            "/* bulkdb:pg:t_extensions */ SELECT a.name AS name, a.default_version AS default_version, a.installed_version AS installed_version, \
{trusted} AS trusted FROM pg_catalog.pg_available_extensions a"
        ))
        .await?,
    ) {
        if o(&r, "installed_version").is_some() {
            st.objects.insert(TKey::of(TClass::Extension, None, None, s(&r, "name")), TObj::default());
        }
        available.insert(s(&r, "name").to_string(), (o(&r, "default_version"), o(&r, "installed_version"), b(&r, "trusted")));
    }
    let roles = if need_roles {
        Some(rows(&t.query_meta("/* bulkdb:pg:t_roles */ SELECT rolname AS name FROM pg_catalog.pg_roles").await?).iter().map(|r| s(r, "name").to_string()).collect())
    } else {
        None
    };
    Ok(TargetPg { state: st, facts, schemas, collations, available, roles })
}

// =============================================================================================
// Collector
// =============================================================================================

/// An object id by (catalog, oid).
type OidMap = HashMap<(String, String), String>;

fn seq_opts(r: &Row) -> PgSeqOptions {
    PgSeqOptions {
        type_sql: s(r, "type").to_string(),
        start: i(r, "start"),
        increment: i(r, "increment"),
        min: i(r, "min"),
        max: i(r, "max"),
        cache: i(r, "cache"),
        cycle: b(r, "cycle"),
    }
}

pub(crate) async fn collect(input: &PlanInput<'_>) -> Result<Collected> {
    let req = input.req;
    let opts = &req.options;
    let tf = input.target.facts();
    let tmajor = tf.major;
    let mut warnings: Vec<CopyIssue> = Vec::new();
    let mut blockers: Vec<CopyIssue> = Vec::new();
    if req.target.schema.is_some() && req.source.schema.is_none() {
        return Err(CoreError::Invalid("copy: a target schema needs a source schema".into()));
    }

    // ---- source facts
    let info_major: u16 = input.source_server.version.split(|c: char| !c.is_ascii_digit()).find(|p| !p.is_empty()).and_then(|p| p.parse().ok()).unwrap_or(0);
    let version = input.source.query(Q_VERSION).await?;
    let vnum: u32 = version.get(0, "version_num").and_then(|v| v.trim().parse().ok()).unwrap_or(0);
    // SQL_ASCII (3.5): the server sends stored bytes unconverted; catalog text (comments, enum
    // labels, defaults, view and function bodies, names) is read raw and decoded from
    // pg.source_encoding, before any text column is read.
    let raw_catalog = version.get(0, "encoding").is_some_and(|e| e.eq_ignore_ascii_case("SQL_ASCII"));
    if raw_catalog {
        input.source.decode_sql_ascii(opts.pg.source_encoding);
    }
    let catalog_major = if vnum > 0 { (vnum / 10_000) as u16 } else { info_major };
    let facts = rows(&input.source.query(&q_facts(catalog_major)).await?).into_iter().next().unwrap_or_default();
    let num: u32 = s(&facts, "version_num").parse().unwrap_or(vnum);
    let smajor: u16 = if num > 0 { (num / 10_000) as u16 } else { info_major };
    if smajor > 0 && smajor < 10 {
        blockers.push(danger("SOURCE_VERSION_UNSUPPORTED", format!("PostgreSQL {smajor} sources are not supported (10 or newer)")));
    }
    if tmajor > 0 && tmajor < 10 {
        blockers.push(danger("TARGET_VERSION_UNSUPPORTED", format!("PostgreSQL {tmajor} targets are not supported (10 or newer)")));
    }
    if smajor > tmajor && tmajor > 0 {
        warnings.push(danger("DOWNGRADE", format!("the source (PostgreSQL {smajor}) is newer than the target (PostgreSQL {tmajor})")));
    }
    let src_encoding = s(&facts, "encoding").to_string();
    let sql_ascii = src_encoding.eq_ignore_ascii_case("SQL_ASCII") || raw_catalog;
    if sql_ascii && !raw_catalog {
        input.source.decode_sql_ascii(opts.pg.source_encoding);
    }
    if sql_ascii && opts.pg.source_encoding.is_none() {
        blockers.push(
            danger("PG_SQL_ASCII_SOURCE", "the source database is SQL_ASCII: its bytes are not converted")
                .rec("set pg.source_encoding (euc-kr, latin1 or utf8) to the encoding the data really has"),
        );
    }
    let superuser = b(&facts, "superuser");
    let bypassrls = b(&facts, "bypassrls");
    check_cancel(input.cancel)?;

    // ---- schemas, extensions, members, dependencies
    let all_schemas = rows(&input.source.query(&q_schemas()).await?);
    let schema_rows: Vec<Row> = match &req.source.schema {
        Some(sc) => all_schemas.into_iter().filter(|r| s(r, "name") == sc).collect(),
        None => all_schemas,
    };
    if let Some(sc) = &req.source.schema {
        if schema_rows.is_empty() {
            return Err(CoreError::NotFound(format!("copy: schema {sc} does not exist in the source database")));
        }
    }
    let copied: BTreeSet<String> = schema_rows.iter().map(|r| s(r, "name").to_string()).collect();
    let extensions = rows(&input.source.query(Q_EXTENSIONS).await?);
    let mut ext_members: HashMap<(String, String), String> = HashMap::new();
    for r in rows(&input.source.query(Q_EXT_MEMBERS).await?) {
        ext_members.insert((s(&r, "cls").trim_start_matches("pg_catalog.").to_string(), s(&r, "objid").to_string()), s(&r, "ext").to_string());
    }
    let deps = rows(&input.source.query(Q_DEPS).await?);
    let mut cats: Vec<SchemaCat> = Vec::new();
    for sr in &schema_rows {
        check_cancel(input.cancel)?;
        let name = s(sr, "name").to_string();
        input.source.query(&format!("SET search_path = {}, pg_catalog", dq(&name))).await?;
        let q = |sql: String| async move { input.source.query(&sql).await.map(|m| rows(&m)) };
        cats.push(SchemaCat {
            name: name.clone(),
            row: sr.clone(),
            collations: q(q_collations(&name)).await?,
            types: q(q_types(&name)).await?,
            sequences: q(q_sequences(&name)).await?,
            functions: q(q_functions(&name, smajor)).await?,
            relations: q(q_relations(&name)).await?,
            columns: q(q_columns(&name, smajor)).await?,
            constraints: q(q_constraints(&name, smajor)).await?,
            indexes: q(q_indexes(&name)).await?,
            triggers: q(q_triggers(&name, smajor)).await?,
            policies: q(q_policies(&name)).await?,
            unsupported: q(q_unsupported(&name)).await.unwrap_or_default(),
        });
    }
    let _ = input.source.query("SET search_path = pg_catalog").await;

    // ---- names
    let ts_of = |sc: &str| target_schema_name(req, sc);
    let tschemas: Vec<String> = copied.iter().map(|sc| ts_of(sc)).collect::<BTreeSet<_>>().into_iter().collect();
    let need_roles = opts.objects.owners || opts.objects.grants;
    let tgt = read_target(input, &tschemas, need_roles, tmajor).await?;
    let renamed_schema = copied.iter().any(|sc| ts_of(sc) != *sc);

    // ---- target checks of the database
    let tenc = o(&tgt.facts, "encoding").or_else(|| tf.vars.get("server_encoding").cloned()).unwrap_or_default();
    if !tenc.is_empty() && !tenc.eq_ignore_ascii_case("UTF8") && !tenc.eq_ignore_ascii_case(&src_encoding) {
        warnings.push(warning("PG_ENCODING", format!("the target database encoding is {tenc} (source {src_encoding}): characters it cannot hold fail the COPY")));
    }
    let tcollate = o(&tgt.facts, "datcollate").or_else(|| tf.vars.get("datcollate").cloned());
    let tctype = o(&tgt.facts, "datctype").or_else(|| tf.vars.get("datctype").cloned());
    let tprov = o(&tgt.facts, "locprovider").or_else(|| tf.vars.get("datlocprovider").cloned());
    if tcollate.is_some()
        && (tcollate != o(&facts, "datcollate")
            || tctype != o(&facts, "datctype")
            || (tprov.is_some() && o(&facts, "locprovider").is_some() && tprov != o(&facts, "locprovider")))
    {
        warnings.push(
            warning(
                "PG_DEFAULT_COLLATION_DIFFERS",
                "the target database's default collation differs: ORDER BY, LIKE index use and range partitioning of text keys can change",
            )
            .detail(format!(
                "source {}/{} -> target {}/{}",
                s(&facts, "datcollate"),
                s(&facts, "datctype"),
                tcollate.unwrap_or_default(),
                tctype.unwrap_or_default()
            )),
        );
    }
    let t_superuser = b(&tgt.facts, "superuser") || tf.vars.get("is_superuser").is_some_and(|v| v == "on" || v == "t" || v == "true");
    if !b(&tgt.facts, "can_create") && tgt.facts.contains_key("can_create") && tschemas.iter().any(|t| !tgt.schemas.contains_key(t)) {
        blockers.push(danger("TGT_PRIVILEGE", "the target account cannot create schemas in the target database (CREATE on the database)"));
    }
    for t in &tschemas {
        if let Some((create, usage)) = tgt.schemas.get(t) {
            if !create || !usage {
                blockers.push(danger("TGT_PRIVILEGE", format!("the target account lacks CREATE or USAGE on schema {t}")));
            }
        }
    }
    let role_settings = rows(&input.source.query(Q_ROLE_SETTINGS).await.unwrap_or_default());
    if !role_settings.is_empty() {
        let det: Vec<String> = role_settings.iter().map(|r| format!("{}: {}", s(r, "role"), json_strings(o(r, "config")).join(", "))).collect();
        warnings.push(warning("PG_DB_ROLE_SETTINGS", "database and role settings of the source are not copied").detail(det.join("; ")));
    }
    let mut unsupported_names: Vec<String> = Vec::new();
    for r in rows(&input.source.query(Q_DB_UNSUPPORTED).await.unwrap_or_default()).iter().chain(cats.iter().flat_map(|c| c.unsupported.iter())) {
        if ext_members.contains_key(&(s(r, "cls").to_string(), s(r, "oid").to_string())) {
            continue;
        }
        unsupported_names.push(format!("{} {}", s(r, "kind"), s(r, "name")));
    }
    if !unsupported_names.is_empty() {
        warnings.push(warning("UNSUPPORTED_OBJECT", "objects of kinds BulkDB does not copy").detail(unsupported_names.join(", ")));
    }

    // ---- oid -> item id maps
    let mut by_oid: OidMap = HashMap::new();
    let mut rel_info: HashMap<String, (String, String, String)> = HashMap::new(); // oid -> (schema, name, kind)
    for c in &cats {
        for r in &c.relations {
            let kind = s(r, "kind");
            let k = match kind {
                "v" => CopyObjectKind::View,
                "m" => CopyObjectKind::MaterializedView,
                _ if b(r, "is_partition") => CopyObjectKind::Partition,
                _ => CopyObjectKind::Table,
            };
            let id = item_id(k, &[&c.name, s(r, "name")]);
            by_oid.insert(("pg_class".into(), s(r, "oid").into()), id.clone());
            by_oid.insert(("pg_type".into(), s(r, "rowtype").into()), id);
            rel_info.insert(s(r, "oid").into(), (c.name.clone(), s(r, "name").into(), kind.into()));
        }
        for r in &c.types {
            let k = if s(r, "type") == "d" { CopyObjectKind::Domain } else { CopyObjectKind::Type };
            by_oid.insert(("pg_type".into(), s(r, "oid").into()), item_id(k, &[&c.name, s(r, "name")]));
        }
        for r in &c.sequences {
            by_oid.insert(("pg_class".into(), s(r, "oid").into()), item_id(CopyObjectKind::Sequence, &[&c.name, s(r, "name")]));
        }
        for r in &c.functions {
            by_oid.insert(("pg_proc".into(), s(r, "oid").into()), format!("{}({})", item_id(CopyObjectKind::Function, &[&c.name, s(r, "name")]), s(r, "args")));
        }
        for r in &c.collations {
            by_oid.insert(("pg_collation".into(), s(r, "oid").into()), item_id(CopyObjectKind::Collation, &[&c.name, s(r, "name")]));
        }
        for r in &c.constraints {
            if let Some((_, t, _)) = rel_info.get(s(r, "rel")) {
                let k = if s(r, "type") == "f" { CopyObjectKind::ForeignKey } else { CopyObjectKind::Constraint };
                by_oid.insert(("pg_constraint".into(), s(r, "oid").into()), item_id(k, &[&c.name, t, s(r, "name")]));
            }
        }
        for r in &c.indexes {
            if let Some((_, t, _)) = rel_info.get(s(r, "rel")) {
                by_oid.insert(("pg_class".into(), s(r, "oid").into()), item_id(CopyObjectKind::Index, &[&c.name, t, s(r, "name")]));
            }
        }
        for r in &c.triggers {
            if let Some((_, t, _)) = rel_info.get(s(r, "rel")) {
                by_oid.insert(("pg_trigger".into(), s(r, "oid").into()), item_id(CopyObjectKind::Trigger, &[&c.name, t, s(r, "name")]));
            }
        }
        for r in &c.policies {
            if let Some((_, t, _)) = rel_info.get(s(r, "rel")) {
                by_oid.insert(("pg_policy".into(), s(r, "oid").into()), item_id(CopyObjectKind::Policy, &[&c.name, t, s(r, "name")]));
            }
        }
    }
    // Constraint-backed indexes stand for their constraint (an FK depends on the parent's key).
    for c in &cats {
        for r in c.indexes.iter().filter(|r| b(r, "backs_constraint")) {
            if let Some(con) = c.constraints.iter().find(|x| x.get("rel") == r.get("rel") && s(x, "name") == s(r, "name")) {
                if let Some(id) = by_oid.get(&("pg_constraint".to_string(), s(con, "oid").to_string())).cloned() {
                    by_oid.insert(("pg_class".into(), s(r, "oid").into()), id);
                }
            }
        }
    }
    let ext_ids: HashMap<String, String> = extensions.iter().map(|r| (s(r, "oid").to_string(), item_id(CopyObjectKind::Extension, &[s(r, "name")]))).collect();
    let mut dep_map: HashMap<String, BTreeSet<String>> = HashMap::new();
    let mut proc_users: HashMap<String, Vec<String>> = HashMap::new();
    for r in &deps {
        let obj = (s(r, "cls").trim_start_matches("pg_catalog.").to_string(), s(r, "objid").to_string());
        let reff = (s(r, "refcls").trim_start_matches("pg_catalog.").to_string(), s(r, "refobjid").to_string());
        let Some(from) = by_oid.get(&obj) else { continue };
        let to = by_oid.get(&reff).cloned().or_else(|| ext_members.get(&reff).and_then(|e| ext_ids.get(e)).cloned());
        if let Some(to) = to {
            if &to != from {
                dep_map.entry(from.clone()).or_default().insert(to);
            }
        }
        if reff.0 == "pg_proc" {
            proc_users.entry(reff.1.clone()).or_default().push(obj.1.clone());
        }
    }
    let deps_of = |id: &str| -> Vec<String> { dep_map.get(id).map(|s| s.iter().cloned().collect()).unwrap_or_default() };

    // ---- drafts
    let mut drafts: Vec<Draft> = Vec::new();
    let mut lo_columns = false;
    let mut money_columns = false;
    let mut tablespace = false;
    let mut invalid_indexes: Vec<String> = Vec::new();
    let mut secdef: Vec<String> = Vec::new();
    let comments = opts.objects.comments;
    // Text partition keys on the database default collation get it by name (3.5).
    let default_coll = named_default_collation(s(&facts, "datcollate"), o(&facts, "locprovider").as_deref(), &tgt.collations);
    let mut comment_drafts: Vec<Draft> = Vec::new();
    let mut owner_drafts: Vec<Draft> = Vec::new();
    let add_comment = |out: &mut Vec<Draft>, target_id: &str, stmts: Vec<String>, ts: &str| {
        if stmts.is_empty() {
            return;
        }
        let mut d = Draft::new(CopyObjectKind::Comment, item_id_of(CopyObjectKind::Comment, target_id), target_id, target_id, CopyPhase::Finalize);
        d.parent = Some(target_id.to_string());
        d.depends_on = vec![target_id.to_string()];
        d.create = std::iter::once(search_path(ts)).chain(stmts).collect();
        out.push(d);
    };
    let add_owner_grants = |out: &mut Vec<Draft>, target_id: &str, object_sql: &str, owner: &str, grant_rows: Vec<Vec<Option<String>>>| {
        if opts.objects.owners && !owner.is_empty() {
            let mut d = Draft::new(CopyObjectKind::Owner, item_id_of(CopyObjectKind::Owner, target_id), target_id, target_id, CopyPhase::Finalize);
            d.parent = Some(target_id.to_string());
            d.depends_on = vec![target_id.to_string()];
            d.create = vec![format!("ALTER {object_sql} OWNER TO {}", dq(owner))];
            if tgt.roles.as_ref().is_some_and(|r| !r.contains(owner)) {
                d.create.clear();
                d.warnings.push(warning("PG_ROLE_MISSING", format!("role {owner} does not exist on the target; the owner is not set")));
            }
            out.push(d);
        }
        if opts.objects.grants && !grant_rows.is_empty() {
            let mut d = Draft::new(CopyObjectKind::Grant, item_id_of(CopyObjectKind::Grant, target_id), target_id, target_id, CopyPhase::Finalize);
            d.parent = Some(target_id.to_string());
            d.depends_on = vec![target_id.to_string()];
            let on = object_sql.splitn(2, ' ').collect::<Vec<_>>();
            let target_sql = if on.len() == 2 { format!("{} {}", on[0], on[1]) } else { object_sql.to_string() };
            let mut missing = Vec::new();
            for g in grant_rows {
                let (Some(Some(role)), Some(Some(privilege))) = (g.first().cloned(), g.get(1).cloned()) else { continue };
                if role != "PUBLIC" && tgt.roles.as_ref().is_some_and(|r| !r.contains(&role)) {
                    missing.push(role);
                    continue;
                }
                let grantee = if role == "PUBLIC" { role } else { dq(&role) };
                d.create.push(format!("GRANT {privilege} ON {target_sql} TO {grantee}"));
            }
            if !missing.is_empty() {
                d.warnings.push(warning("PG_ROLE_MISSING", "roles missing on the target; their grants are skipped").detail(missing.join(", ")));
            }
            out.push(d);
        }
    };

    for c in &cats {
        let sc = c.name.as_str();
        let ts = ts_of(sc);
        let schema_id = item_id(CopyObjectKind::Schema, &[sc]);
        let mut sd = Draft::new(CopyObjectKind::Schema, schema_id.clone(), sc, &ts, CopyPhase::PreData);
        sd.target_schema = Some(ts.clone());
        sd.create = vec![format!("CREATE SCHEMA IF NOT EXISTS {}", dq(&ts))];
        sd.exists_key = Some(TKey::of(TClass::Schema, None, None, &ts));
        sd.support = true;
        drafts.push(sd);
        if comments {
            if let Some(cm) = o(&c.row, "comment") {
                add_comment(&mut comment_drafts, &schema_id, vec![format!("COMMENT ON SCHEMA {} IS {}", dq(&ts), pg_str(&cm))], &ts);
            }
        }
        add_owner_grants(&mut owner_drafts, &schema_id, &format!("SCHEMA {}", dq(&ts)), s(&c.row, "owner"), json_rows(o(&c.row, "grants")));
        let base = |kind: CopyObjectKind, id: String, name: &str, tname: &str, phase: CopyPhase| -> Draft {
            let mut d = Draft::new(kind, id, name, tname, phase);
            d.schema = Some(sc.to_string());
            d.target_schema = Some(ts.clone());
            d.depends_on.push(schema_id.clone());
            d
        };

        // Collations.
        for r in &c.collations {
            if ext_members.contains_key(&("pg_collation".to_string(), s(r, "oid").to_string())) {
                continue;
            }
            let id = item_id(CopyObjectKind::Collation, &[sc, s(r, "name")]);
            let j: serde_json::Value = serde_json::from_str(s(r, "j")).unwrap_or_default();
            let mut d = base(CopyObjectKind::Collation, id, s(r, "name"), s(r, "name"), CopyPhase::PreData);
            match create_collation(&dq2(&ts, s(r, "name")), &j) {
                Some(sql) => d.create = vec![sql],
                None => {
                    d.unsupported = true;
                    d.warnings.push(warning("UNSUPPORTED_OBJECT", "this collation cannot be recreated"));
                }
            }
            d.drop = vec![format!("DROP COLLATION {} RESTRICT", dq2(&ts, s(r, "name")))];
            d.exists_key = Some(TKey::of(TClass::Collation, Some(&ts), None, s(r, "name")));
            d.support = true;
            drafts.push(d);
        }
        // Types and domains.
        if opts.objects.types {
            for r in &c.types {
                if ext_members.contains_key(&("pg_type".to_string(), s(r, "oid").to_string())) {
                    continue;
                }
                let ty = s(r, "type");
                let kind = if ty == "d" { CopyObjectKind::Domain } else { CopyObjectKind::Type };
                let id = item_id(kind, &[sc, s(r, "name")]);
                let q = dq2(&ts, s(r, "name"));
                let mut d = base(kind, id.clone(), s(r, "name"), s(r, "name"), CopyPhase::PreData);
                d.support = true;
                d.exists_key = Some(TKey::of(TClass::Type, Some(&ts), None, s(r, "name")));
                d.drop = vec![format!("DROP {} {q} RESTRICT", if ty == "d" { "DOMAIN" } else { "TYPE" })];
                d.depends_on.extend(deps_of(&id));
                match ty {
                    "e" => d.create = vec![search_path(&ts), create_enum(&q, &json_strings(o(r, "labels")))],
                    "c" => {
                        let attrs: Vec<(String, String, Option<String>)> = json_rows(o(r, "attrs"))
                            .into_iter()
                            .map(|a| (a.first().cloned().flatten().unwrap_or_default(), a.get(1).cloned().flatten().unwrap_or_default(), a.get(2).cloned().flatten()))
                            .collect();
                        d.create = vec![search_path(&ts), create_composite(&q, &attrs)];
                    }
                    "r" => {
                        let rr = json_strings(o(r, "range"));
                        let diff_schema = rr.get(2).cloned().unwrap_or_default();
                        let diff = rr.get(1).filter(|v| !v.is_empty() && v.as_str() != "null").cloned();
                        if diff.is_some() && diff_schema != "pg_catalog" {
                            d.unsupported = true;
                            d.warnings.push(warning("UNSUPPORTED_OBJECT", "range types with a custom subtype diff function are not copied"));
                        } else {
                            d.create = vec![search_path(&ts), create_range(&q, rr.first().map(String::as_str).unwrap_or("integer"), diff.as_deref())];
                        }
                    }
                    _ => {
                        // Domain: validated checks inline, NOT VALID ones in post_data.
                        let mut checks = Vec::new();
                        for con in c.constraints.iter().filter(|x| s(x, "typ") == s(r, "oid") && s(x, "type") == "c") {
                            if b(con, "validated") {
                                checks.push((s(con, "name").to_string(), s(con, "def").to_string()));
                            } else {
                                let mut cd = base(
                                    CopyObjectKind::Constraint,
                                    item_id(CopyObjectKind::Constraint, &[sc, s(r, "name"), s(con, "name")]),
                                    s(con, "name"),
                                    s(con, "name"),
                                    CopyPhase::PostData,
                                );
                                cd.parent = Some(id.clone());
                                cd.depends_on.push(id.clone());
                                cd.create = vec![search_path(&ts), format!("ALTER DOMAIN {q} ADD CONSTRAINT {} {}", dq(s(con, "name")), s(con, "def"))];
                                drafts.push(cd);
                            }
                        }
                        d.create =
                            vec![search_path(&ts), create_domain(&q, s(r, "base"), o(r, "collation").as_deref(), o(r, "dflt").as_deref(), b(r, "notnull"), &checks)];
                    }
                }
                if comments {
                    if let Some(cm) = o(r, "comment") {
                        add_comment(&mut comment_drafts, &id, vec![format!("COMMENT ON {} {q} IS {}", if ty == "d" { "DOMAIN" } else { "TYPE" }, pg_str(&cm))], &ts);
                    }
                }
                // Type and domain ACLs (`typacl`): `GRANT USAGE ON TYPE | DOMAIN …`.
                add_owner_grants(&mut owner_drafts, &id, &format!("{} {q}", if ty == "d" { "DOMAIN" } else { "TYPE" }), s(r, "owner"), json_rows(o(r, "grants")));
                drafts.push(d);
            }
        }
        // Functions and procedures.
        if opts.objects.routines {
            for r in &c.functions {
                if ext_members.contains_key(&("pg_proc".to_string(), s(r, "oid").to_string())) {
                    continue;
                }
                let args = s(r, "args");
                let id = format!("{}({args})", item_id(CopyObjectKind::Function, &[sc, s(r, "name")]));
                let mut d = base(CopyObjectKind::Function, id.clone(), &format!("{}({args})", s(r, "name")), &format!("{}({args})", s(r, "name")), CopyPhase::PreData);
                d.support = true;
                d.exists_key = Some(TKey::of(TClass::Function, Some(&ts), None, &format!("{}({args})", s(r, "name"))));
                let proc_kw = if s(r, "kind") == "p" { "PROCEDURE" } else { "FUNCTION" };
                let q = format!("{}({args})", dq2(&ts, s(r, "name")));
                d.drop = vec![format!("DROP {proc_kw} {q} RESTRICT")];
                d.depends_on.extend(deps_of(&id));
                match (s(r, "kind"), o(r, "def")) {
                    ("a", _) | (_, None) => {
                        d.unsupported = true;
                        d.warnings.push(warning("UNSUPPORTED_OBJECT", "aggregates are not copied"));
                    }
                    (_, Some(def)) => {
                        d.source_ddl = Some(def.clone());
                        d.create =
                            vec![search_path(&ts), "SET check_function_bodies = off".into(), rewrite_function_header(&def, s(r, "qname"), &dq2(&ts, s(r, "name")))];
                    }
                }
                if b(r, "secdef") && !opts.objects.owners {
                    secdef.push(q.clone());
                    d.warnings.push(warning("PG_SECDEF_OWNER", "a SECURITY DEFINER function becomes owned by the copy account and runs with its rights"));
                }
                if tmajor >= 17 && !json_strings(o(r, "config")).iter().any(|c| c.starts_with("search_path=")) {
                    let users = proc_users.get(s(r, "oid")).cloned().unwrap_or_default();
                    let used_by_index_or_mv = users.iter().any(|u| {
                        cats.iter().any(|c2| {
                            c2.indexes.iter().any(|ix| s(ix, "oid") == u && b(ix, "has_exprs"))
                                || c2.relations.iter().any(|rel| s(rel, "oid") == u && s(rel, "kind") == "m")
                        })
                    });
                    if used_by_index_or_mv {
                        d.warnings.push(
                            warning(
                                "PG17_SEARCH_PATH",
                                "PostgreSQL 17 runs index builds and REFRESH with search_path = pg_catalog, pg_temp: unqualified references in this function fail",
                            )
                            .rec("ALTER FUNCTION … SET search_path = … on the source or after the copy"),
                        );
                    }
                }
                if renamed_schema {
                    d.warnings.push(warning("RENAMED_REFS", "references inside function bodies are not rewritten to the renamed schema"));
                }
                if comments {
                    if let Some(cm) = o(r, "comment") {
                        add_comment(&mut comment_drafts, &id, vec![format!("COMMENT ON {proc_kw} {q} IS {}", pg_str(&cm))], &ts);
                    }
                }
                add_owner_grants(&mut owner_drafts, &id, &format!("{proc_kw} {q}"), s(r, "owner"), json_rows(o(r, "grants")));
                drafts.push(d);
            }
        }
        // Sequences: standalone and serial-owned are created; identity sequences come with their column.
        let mut seq_by_owner: BTreeMap<(String, String), Row> = BTreeMap::new();
        for r in &c.sequences {
            if ext_members.contains_key(&("pg_class".to_string(), s(r, "oid").to_string())) {
                continue;
            }
            let deptype = s(r, "deptype");
            if !s(r, "owner_table").is_empty() {
                seq_by_owner.insert((s(r, "owner_table").to_string(), s(r, "owner_column").to_string()), r.clone());
            }
            let id = item_id(CopyObjectKind::Sequence, &[sc, s(r, "name")]);
            let q = dq2(&ts, s(r, "name"));
            if deptype != "i" {
                let mut d = base(CopyObjectKind::Sequence, id.clone(), s(r, "name"), s(r, "name"), CopyPhase::PreData);
                d.support = true;
                d.exists_key = Some(TKey::of(TClass::Sequence, Some(&ts), None, s(r, "name")));
                d.create = vec![create_sequence(&q, &seq_opts(r))];
                d.drop = vec![format!("DROP SEQUENCE {q} RESTRICT")];
                if deptype == "a" && s(r, "owner_schema") == sc {
                    d.parent = None;
                }
                if comments {
                    if let Some(cm) = o(r, "comment") {
                        add_comment(&mut comment_drafts, &id, vec![format!("COMMENT ON SEQUENCE {q} IS {}", pg_str(&cm))], &ts);
                    }
                }
                add_owner_grants(&mut owner_drafts, &id, &format!("SEQUENCE {q}"), s(r, "owner"), json_rows(o(r, "grants")));
                drafts.push(d);
            }
            // The value (finalize).
            let owner_table =
                if s(r, "owner_schema") == sc && !s(r, "owner_table").is_empty() { Some(item_id(CopyObjectKind::Table, &[sc, s(r, "owner_table")])) } else { None };
            let mut v = base(CopyObjectKind::SequenceValue, item_id(CopyObjectKind::SequenceValue, &[sc, s(r, "name")]), s(r, "name"), s(r, "name"), CopyPhase::Finalize);
            let lit = pg_str(&q);
            let agg = if i(r, "increment") < 0 { "LEAST" } else { "GREATEST" };
            let created = format!("SELECT pg_catalog.setval({lit}, {SOURCE_VALUE}, <is_called>)");
            let mut floor_lit = lit.clone();
            if let Some(t) = &owner_table {
                v.parent = Some(t.clone());
                v.depends_on.push(t.clone());
                // An existing target table may own a sequence of another name.
                let tt = target_table_name(opts, Some(sc), s(r, "owner_table"), false);
                if tgt.state.objects.contains_key(&TKey::rel(Some(&ts), &tt)) {
                    let m = input
                        .target
                        .query_meta(&format!("SELECT pg_catalog.pg_get_serial_sequence({}, {}) AS seq", pg_str(&dq2(&ts, &tt)), pg_str(s(r, "owner_column"))))
                        .await?;
                    if let Some(seq) = m.get(0, "seq").filter(|v| !v.is_empty()) {
                        floor_lit = pg_str(seq);
                    }
                }
            } else {
                v.parent = Some(id.clone());
                v.depends_on.push(id.clone());
                v.exists_key = Some(TKey::of(TClass::Sequence, Some(&ts), None, s(r, "name")));
            }
            let floor = format!("SELECT pg_catalog.setval({floor_lit}, {agg}({SOURCE_VALUE}, {TARGET_VALUE}), true)");
            v.seq_templates = Some((created, floor));
            drafts.push(v);
        }

        // Relations.
        for r in &c.relations {
            check_cancel(input.cancel)?;
            let oid = s(r, "oid");
            let name = s(r, "name");
            let kind = s(r, "kind");
            if b(r, "ext_member") {
                continue;
            }
            let tname = target_table_name(opts, Some(sc), name, false);
            let q = dq2(&ts, &tname);
            if b(r, "has_tablespace") {
                tablespace = true;
            }
            let reloptions = json_strings(o(r, "reloptions"));
            let cols: Vec<&Row> = c.columns.iter().filter(|x| s(x, "rel") == oid).collect();
            match kind {
                "v" | "m" => {
                    if !opts.objects.views {
                        continue;
                    }
                    let k = if kind == "m" { CopyObjectKind::MaterializedView } else { CopyObjectKind::View };
                    let id = item_id(k, &[sc, name]);
                    let mut d = base(k, id.clone(), name, &tname, CopyPhase::PreData);
                    d.exists_key = Some(TKey::rel(Some(&ts), &tname));
                    d.create = vec![search_path(&ts), create_view(&q, kind == "m", &reloptions, s(r, "viewdef"))];
                    d.drop = vec![format!("DROP {} {q} RESTRICT", if kind == "m" { "MATERIALIZED VIEW" } else { "VIEW" })];
                    d.source_ddl = o(r, "viewdef");
                    d.depends_on.extend(deps_of(&id));
                    if renamed_schema {
                        d.warnings.push(warning("RENAMED_REFS", "references to other schemas are not rewritten to the renamed schema"));
                    }
                    if kind == "m" && opts.objects.refresh_matviews {
                        let mut rd = base(CopyObjectKind::Refresh, item_id(CopyObjectKind::Refresh, &[sc, name]), name, &tname, CopyPhase::Finalize);
                        rd.parent = Some(id.clone());
                        rd.depends_on.push(id.clone());
                        rd.create = vec![format!("REFRESH MATERIALIZED VIEW {q}")];
                        drafts.push(rd);
                    }
                    if comments {
                        let mut stmts = Vec::new();
                        if let Some(cm) = o(r, "comment") {
                            stmts.push(format!("COMMENT ON {} {q} IS {}", if kind == "m" { "MATERIALIZED VIEW" } else { "VIEW" }, pg_str(&cm)));
                        }
                        for col in &cols {
                            if let Some(cm) = o(col, "comment") {
                                stmts.push(format!("COMMENT ON COLUMN {q}.{} IS {}", dq(s(col, "name")), pg_str(&cm)));
                            }
                        }
                        add_comment(&mut comment_drafts, &id, stmts, &ts);
                    }
                    add_owner_grants(&mut owner_drafts, &id, &format!("TABLE {q}"), s(r, "owner"), json_rows(o(r, "grants")));
                    drafts.push(d);
                }
                "f" => {
                    let id = item_id(CopyObjectKind::Table, &[sc, name]);
                    let mut d = base(CopyObjectKind::Table, id, name, &tname, CopyPhase::PreData);
                    d.selected = selected(req, Some(sc), name);
                    d.unsupported = true;
                    d.warnings.push(warning("UNSUPPORTED_OBJECT", "foreign tables are not copied"));
                    drafts.push(d);
                }
                _ => {
                    let is_partition = b(r, "is_partition");
                    let k = if is_partition { CopyObjectKind::Partition } else { CopyObjectKind::Table };
                    let id = item_id(k, &[sc, name]);
                    let mut d = base(k, id.clone(), name, &tname, CopyPhase::PreData);
                    d.exists_key = Some(TKey::rel(Some(&ts), &tname));
                    d.selected = is_partition || selected(req, Some(sc), name);
                    let parents = json_rows(o(r, "parents"));
                    if is_partition {
                        if let Some(p) = parents.first() {
                            let (ps, pn) = (p.first().cloned().flatten().unwrap_or_default(), p.get(1).cloned().flatten().unwrap_or_default());
                            let pid = by_oid
                                .iter()
                                .find(|(k, v)| {
                                    k.0 == "pg_class"
                                        && (v.as_str() == item_id(CopyObjectKind::Table, &[&ps, &pn]) || v.as_str() == item_id(CopyObjectKind::Partition, &[&ps, &pn]))
                                })
                                .map(|(_, v)| v.clone())
                                .unwrap_or_else(|| item_id(CopyObjectKind::Table, &[&ps, &pn]));
                            d.parent = Some(pid.clone());
                            d.depends_on.push(pid);
                        }
                    }
                    if b(r, "typed") {
                        d.unsupported = true;
                        d.warnings.push(warning("UNSUPPORTED_OBJECT", "typed tables are not copied"));
                        drafts.push(d);
                        continue;
                    }
                    if !d.selected {
                        drafts.push(d);
                        continue;
                    }
                    // Columns.
                    let mut defs: Vec<PgColumnDef> = Vec::new();
                    let mut data_cols: Vec<DataColumn> = Vec::new();
                    let mut follow: Vec<String> = Vec::new();
                    let mut missing_coll: Vec<String> = Vec::new();
                    let mut virtual_downgraded: Vec<String> = Vec::new();
                    for col in &cols {
                        let cname = s(col, "name");
                        // 's' stored, 'v' PG18 virtual: computed by the target, never copied.
                        let generated = matches!(s(col, "generated"), "s" | "v");
                        let generated_virtual = s(col, "generated") == "v";
                        let identity = s(col, "identity").chars().next().filter(|ch| *ch == 'a' || *ch == 'd');
                        if matches!(s(col, "typname"), "oid" | "lo") {
                            lo_columns = true;
                        }
                        if s(col, "typname") == "money" {
                            money_columns = true;
                        }
                        let mut collation = o(col, "collation");
                        if let (Some(cs), Some(cn)) = (o(col, "coll_schema"), o(col, "coll_name")) {
                            if !copied.contains(&cs) && !tgt.collations.contains(&(cs.clone(), cn.clone())) && !tgt.collations.is_empty() {
                                if opts.pg.collation_fallback {
                                    collation = None;
                                    d.warnings.push(warning(
                                        "PG_COLLATION_MISSING",
                                        format!("collation {cs}.{cn} of column {cname} is missing on the target; the database default is used"),
                                    ));
                                } else {
                                    missing_coll.push(format!("{cs}.{cn} ({cname})"));
                                }
                            }
                        }
                        if !b(col, "local") && (is_partition || !parents.is_empty()) {
                            // Inherited column: comes with the parent. A default or NOT NULL of
                            // the child's own follows its CREATE, as pg_dump does.
                            if let Some(pc) = parent_column(&cats, &parents, cname) {
                                if !generated && identity.is_none() && o(col, "dflt") != o(pc, "dflt") {
                                    follow.push(match o(col, "dflt") {
                                        Some(v) => format!("ALTER TABLE ONLY {q} ALTER COLUMN {} SET DEFAULT {v}", dq(cname)),
                                        None => format!("ALTER TABLE ONLY {q} ALTER COLUMN {} DROP DEFAULT", dq(cname)),
                                    });
                                }
                                if b(col, "notnull") && !b(pc, "notnull") {
                                    follow.push(format!("ALTER TABLE ONLY {q} ALTER COLUMN {} SET NOT NULL", dq(cname)));
                                }
                            }
                        } else {
                            let identity_seq =
                                identity.and_then(|_| seq_by_owner.get(&(name.to_string(), cname.to_string()))).map(|sr| (dq2(&ts, s(sr, "name")), seq_opts(sr)));
                            let not_null_name = c
                                .constraints
                                .iter()
                                .find(|x| s(x, "rel") == oid && s(x, "type") == "n" && json_strings(o(x, "keys")).first().map(String::as_str) == Some(s(col, "num")))
                                .map(|x| s(x, "name").to_string());
                            defs.push(PgColumnDef {
                                name: cname.to_string(),
                                type_sql: s(col, "type").to_string(),
                                // A column on the database default collation gets the source default by
                                // name when the target has it (3.5), so a target with another default
                                // keeps the source ordering.
                                collation: collation.or_else(|| if o(col, "collation").is_none() && b(col, "dflt_coll") { default_coll.clone() } else { None }),
                                default: if generated || identity.is_some() { None } else { o(col, "dflt") },
                                not_null: b(col, "notnull") && !(is_partition && !b(col, "local")),
                                not_null_name,
                                identity,
                                identity_seq,
                                generated: if generated { o(col, "dflt") } else { None },
                                generated_virtual,
                            });
                            if generated_virtual && tmajor < 18 {
                                virtual_downgraded.push(cname.to_string());
                            }
                        }
                        let st: i64 = s(col, "stattarget").parse().unwrap_or(-1);
                        if st >= 0 {
                            follow.push(format!("ALTER TABLE ONLY {q} ALTER COLUMN {} SET STATISTICS {st}", dq(cname)));
                        }
                        if !s(col, "storage").is_empty() && s(col, "storage") != s(col, "typstorage") {
                            let mode = match s(col, "storage") {
                                "p" => "PLAIN",
                                "e" => "EXTERNAL",
                                "m" => "MAIN",
                                _ => "EXTENDED",
                            };
                            follow.push(format!("ALTER TABLE ONLY {q} ALTER COLUMN {} SET STORAGE {mode}", dq(cname)));
                        }
                        if smajor >= 14 && tmajor >= 14 {
                            match s(col, "compression") {
                                "p" => follow.push(format!("ALTER TABLE ONLY {q} ALTER COLUMN {} SET COMPRESSION pglz", dq(cname))),
                                "l" => follow.push(format!("ALTER TABLE ONLY {q} ALTER COLUMN {} SET COMPRESSION lz4", dq(cname))),
                                _ => {}
                            }
                        }
                        if !generated {
                            data_cols.push(DataColumn {
                                source: cname.to_string(),
                                target: cname.to_string(),
                                value: ValueKind::Raw,
                                read: SourceRead::Plain,
                                zero_date: ZeroDateAction::NotTemporal,
                                invalid_date: ZeroDateAction::NotTemporal,
                                binary_attr: false,
                                max_bytes: None,
                            });
                        }
                    }
                    if !virtual_downgraded.is_empty() {
                        d.warnings.push(warning(
                            "DOWNGRADE",
                            format!(
                                "virtual generated columns need PostgreSQL 18 on the target: {} become STORED (computed and stored on write)",
                                virtual_downgraded.join(", ")
                            ),
                        ));
                    }
                    if !missing_coll.is_empty() {
                        d.blockers.push(
                            danger("PG_COLLATION_MISSING", "collations used by the table do not exist on the target")
                                .detail(missing_coll.join(", "))
                                .rec("pg.collation_fallback, or create the collations on the target"),
                        );
                    }
                    // Constraints: validated checks inline (NOT VALID / NOT ENFORCED elsewhere).
                    let mut inline = Vec::new();
                    for con in c.constraints.iter().filter(|x| s(x, "rel") == oid) {
                        let ct = s(con, "type");
                        let cname = s(con, "name");
                        if !b(con, "local") {
                            continue;
                        }
                        let not_enforced = s(con, "enforced") == "false";
                        let cid = item_id(if ct == "f" { CopyObjectKind::ForeignKey } else { CopyObjectKind::Constraint }, &[sc, name, cname]);
                        let alter = format!("ALTER TABLE {}{q} ADD CONSTRAINT {} {}", if kind == "p" { "" } else { "ONLY " }, dq(cname), s(con, "def"));
                        match ct {
                            "c" if b(con, "validated") && !not_enforced => inline.push(format!("CONSTRAINT {} {}", dq(cname), s(con, "def"))),
                            "c" | "p" | "u" | "x" | "f" => {
                                let k = if ct == "f" { CopyObjectKind::ForeignKey } else { CopyObjectKind::Constraint };
                                let phase = if opts.indexes_after_data || ct == "f" || (ct == "c") { CopyPhase::PostData } else { CopyPhase::PreData };
                                let mut cd = base(k, cid.clone(), cname, cname, phase);
                                cd.parent = Some(id.clone());
                                cd.depends_on.push(id.clone());
                                cd.depends_on.extend(deps_of(&cid));
                                cd.exists_key = Some(TKey::of(TClass::Constraint, Some(&ts), Some(&tname), cname));
                                cd.create = vec![search_path(&ts), alter];
                                cd.drop = vec![format!("ALTER TABLE {q} DROP CONSTRAINT {}", dq(cname))];
                                if not_enforced && tmajor < 18 {
                                    cd.unsupported = true;
                                    cd.warnings.push(warning("DOWNGRADE", "NOT ENFORCED constraints need PostgreSQL 18"));
                                }
                                if ct == "f" {
                                    let pid = (|| {
                                        let (fs, ft) = (o(con, "fschema")?, o(con, "ftable")?);
                                        copied.contains(&fs).then(|| item_id(CopyObjectKind::Table, &[&fs, &ft]))
                                    })();
                                    if let Some(p) = &pid {
                                        if !cd.depends_on.contains(p) {
                                            cd.depends_on.push(p.clone());
                                        }
                                    }
                                    cd.fk_parent = pid;
                                }
                                if comments {
                                    if let Some(cm) = o(con, "comment") {
                                        add_comment(&mut comment_drafts, &cid, vec![format!("COMMENT ON CONSTRAINT {} ON {q} IS {}", dq(cname), pg_str(&cm))], &ts);
                                    }
                                }
                                drafts.push(cd);
                            }
                            _ => {}
                        }
                    }
                    let partition_of = if is_partition {
                        parents.first().map(|p| {
                            let (ps, pn) = (p.first().cloned().flatten().unwrap_or_default(), p.get(1).cloned().flatten().unwrap_or_default());
                            (dq2(&ts_of(&ps), &target_table_name(opts, Some(&ps), &pn, false)), s(r, "bound").to_string())
                        })
                    } else {
                        None
                    };
                    let inherits: Vec<String> = if is_partition {
                        Vec::new()
                    } else {
                        parents
                            .iter()
                            .map(|p| {
                                let (ps, pn) = (p.first().cloned().flatten().unwrap_or_default(), p.get(1).cloned().flatten().unwrap_or_default());
                                if copied.contains(&ps) {
                                    dq2(&ts_of(&ps), &target_table_name(opts, Some(&ps), &pn, false))
                                } else {
                                    dq2(&ps, &pn)
                                }
                            })
                            .collect()
                    };
                    for p in &parents {
                        let (ps, pn) = (p.first().cloned().flatten().unwrap_or_default(), p.get(1).cloned().flatten().unwrap_or_default());
                        let pid = item_id(CopyObjectKind::Table, &[&ps, &pn]);
                        if !d.depends_on.contains(&pid) && copied.contains(&ps) && !is_partition {
                            d.depends_on.push(pid);
                        }
                    }
                    let def = PgTableDef {
                        unlogged: s(r, "persistence") == "u",
                        columns: defs,
                        constraints: inline,
                        inherits,
                        partition_by: o(r, "partkey").map(|pk| match &default_coll {
                            Some(dc) => {
                                let colls: Vec<Option<String>> = json_strings(o(r, "part_colls")).iter().map(|k| (k == "default").then(|| dc.clone())).collect();
                                partkey_with_collations(&pk, &colls)
                            }
                            None => pk,
                        }),
                        partition_of,
                        reloptions: reloptions.clone(),
                    };
                    let mut create = vec![search_path(&ts), create_table(&q, &def, tmajor)];
                    // DROP TABLE would drop the serial sequences the table owns, which are items
                    // of their own (created before the table, kept or done on a resume): release
                    // them first so the CREATE that follows finds them.
                    let mut drops: Vec<String> = Vec::new();
                    for ((tbl, col), sr) in &seq_by_owner {
                        if tbl == name && s(sr, "deptype") == "a" {
                            create.push(format!("ALTER SEQUENCE {} OWNED BY {q}.{}", dq2(&ts, s(sr, "name")), dq(col)));
                            drops.push(format!("ALTER SEQUENCE IF EXISTS {} OWNED BY NONE", dq2(&ts, s(sr, "name"))));
                            let sid = item_id(CopyObjectKind::Sequence, &[sc, s(sr, "name")]);
                            if !d.depends_on.contains(&sid) {
                                d.depends_on.push(sid);
                            }
                        }
                    }
                    create.extend(follow);
                    match s(r, "replident") {
                        "f" => create.push(format!("ALTER TABLE ONLY {q} REPLICA IDENTITY FULL")),
                        "n" => create.push(format!("ALTER TABLE ONLY {q} REPLICA IDENTITY NOTHING")),
                        _ => {}
                    }
                    d.create = create;
                    drops.push(format!("DROP TABLE {q} RESTRICT"));
                    d.drop = drops;
                    d.depends_on.extend(deps_of(&id));
                    d.est_rows = s(r, "est_rows").parse::<i64>().ok().filter(|v| *v >= 0).map(|v| v as u64);
                    d.est_bytes = s(r, "bytes").parse::<u64>().ok();
                    // RLS on the source: a non-owner read would silently filter rows.
                    if b(r, "rls") && !(superuser || bypassrls || (b(r, "is_owner") && !b(r, "force_rls"))) {
                        d.blockers.push(
                            danger("SRC_RLS", "row level security on the source would hide rows from the copy account")
                                .rec("copy with the table owner, a superuser or a BYPASSRLS role"),
                        );
                    }
                    // Data: ordinary tables (FROM ONLY) and partitioned roots; partitions route through the root.
                    if !is_partition {
                        let only = if kind == "p" { "" } else { "ONLY " };
                        let list = data_cols.iter().map(|c| dq(&c.source)).collect::<Vec<_>>().join(", ");
                        let tlist = data_cols.iter().map(|c| dq(&c.target)).collect::<Vec<_>>().join(", ");
                        let spec = TableDataSpec {
                            item_id: id.clone(),
                            source_sql: format!("SELECT {list} FROM {only}{}", dq2(sc, name)),
                            verify_sql: format!("SELECT {tlist} FROM {only}{q}"),
                            raw_csv: true,
                            transcode: if sql_ascii { opts.pg.source_encoding } else { None },
                            target: TargetTable { schema: Some(ts.clone()), name: tname.clone() },
                            columns: data_cols,
                            est_rows: d.est_rows,
                            first_timestamp_auto: false,
                            fk_checks: false,
                        };
                        d.truncate = Some(format!("TRUNCATE {only}{q}"));
                        d.load_template = Some(crate::copy::target::pg::copy_in_sql(&spec));
                        d.data = Some(spec);
                        if opts.objects.analyze {
                            let mut ad = base(CopyObjectKind::Analyze, item_id(CopyObjectKind::Analyze, &[sc, name]), name, &tname, CopyPhase::Finalize);
                            ad.parent = Some(id.clone());
                            ad.depends_on.push(id.clone());
                            ad.create = vec![format!("ANALYZE {q}")];
                            drafts.push(ad);
                        }
                    }
                    // Indexes (not backing constraints, valid, not partition children).
                    for ix in c.indexes.iter().filter(|x| s(x, "rel") == oid) {
                        if b(ix, "backs_constraint") || b(ix, "is_partition") || b(ix, "ext_member") {
                            continue;
                        }
                        if !b(ix, "valid") {
                            invalid_indexes.push(format!("{sc}.{}", s(ix, "name")));
                            continue;
                        }
                        if b(ix, "has_tablespace") {
                            tablespace = true;
                        }
                        let iid = item_id(CopyObjectKind::Index, &[sc, name, s(ix, "name")]);
                        let mut xd = base(
                            CopyObjectKind::Index,
                            iid.clone(),
                            s(ix, "name"),
                            s(ix, "name"),
                            if opts.indexes_after_data { CopyPhase::PostData } else { CopyPhase::PreData },
                        );
                        xd.parent = Some(id.clone());
                        xd.depends_on.push(id.clone());
                        xd.depends_on.extend(deps_of(&iid));
                        xd.exists_key = Some(TKey::of(TClass::Index, Some(&ts), None, s(ix, "name")));
                        let mut stmts = vec![search_path(&ts), index_sql(s(ix, "def"), kind == "p")];
                        if b(ix, "clustered") {
                            stmts.push(format!("ALTER TABLE {q} CLUSTER ON {}", dq(s(ix, "name"))));
                        }
                        if b(ix, "replident") {
                            stmts.push(format!("ALTER TABLE ONLY {q} REPLICA IDENTITY USING INDEX {}", dq(s(ix, "name"))));
                        }
                        xd.create = stmts;
                        xd.drop = vec![format!("DROP INDEX {} RESTRICT", dq2(&ts, s(ix, "name")))];
                        if comments {
                            if let Some(cm) = o(ix, "comment") {
                                add_comment(&mut comment_drafts, &iid, vec![format!("COMMENT ON INDEX {} IS {}", dq2(&ts, s(ix, "name")), pg_str(&cm))], &ts);
                            }
                        }
                        drafts.push(xd);
                    }
                    // Constraint-backed index options (CLUSTER ON, REPLICA IDENTITY USING INDEX).
                    for ix in c.indexes.iter().filter(|x| s(x, "rel") == oid && b(x, "backs_constraint")) {
                        let cid = item_id(CopyObjectKind::Constraint, &[sc, name, s(ix, "name")]);
                        if let Some(cd) = drafts.iter_mut().find(|x| x.id == cid) {
                            if b(ix, "clustered") {
                                cd.create.push(format!("ALTER TABLE {q} CLUSTER ON {}", dq(s(ix, "name"))));
                            }
                            if b(ix, "replident") {
                                cd.create.push(format!("ALTER TABLE ONLY {q} REPLICA IDENTITY USING INDEX {}", dq(s(ix, "name"))));
                            }
                        }
                    }
                    // Triggers last, each followed by its enable state.
                    if opts.objects.triggers {
                        for tg in c.triggers.iter().filter(|x| s(x, "rel") == oid) {
                            if b(tg, "internal") || s(tg, "parentid") != "0" {
                                continue;
                            }
                            let gid = item_id(CopyObjectKind::Trigger, &[sc, name, s(tg, "name")]);
                            let mut gd = base(CopyObjectKind::Trigger, gid.clone(), s(tg, "name"), s(tg, "name"), CopyPhase::PostData);
                            gd.parent = Some(id.clone());
                            gd.depends_on.push(id.clone());
                            if let Some(f) = by_oid.get(&("pg_proc".to_string(), s(tg, "func").to_string())) {
                                gd.depends_on.push(f.clone());
                            }
                            gd.exists_key = Some(TKey::of(TClass::Trigger, Some(&ts), Some(&tname), s(tg, "name")));
                            let mut stmts = vec![search_path(&ts), s(tg, "def").to_string()];
                            stmts.extend(trigger_enable_sql(&q, s(tg, "name"), s(tg, "enabled")));
                            gd.create = stmts;
                            gd.drop = vec![format!("DROP TRIGGER {} ON {q} RESTRICT", dq(s(tg, "name")))];
                            if comments {
                                if let Some(cm) = o(tg, "comment") {
                                    add_comment(&mut comment_drafts, &gid, vec![format!("COMMENT ON TRIGGER {} ON {q} IS {}", dq(s(tg, "name")), pg_str(&cm))], &ts);
                                }
                            }
                            drafts.push(gd);
                        }
                    }
                    // Row level security and policies.
                    let pols: Vec<&Row> = c.policies.iter().filter(|x| s(x, "rel") == oid).collect();
                    if b(r, "rls") || b(r, "force_rls") {
                        let mut pd = base(
                            CopyObjectKind::Policy,
                            item_id(CopyObjectKind::Policy, &[sc, name, "(row level security)"]),
                            "(row level security)",
                            "(row level security)",
                            CopyPhase::PostData,
                        );
                        pd.parent = Some(id.clone());
                        pd.depends_on.push(id.clone());
                        pd.create = vec![format!("ALTER TABLE ONLY {q} ENABLE ROW LEVEL SECURITY")];
                        if b(r, "force_rls") {
                            pd.create.push(format!("ALTER TABLE ONLY {q} FORCE ROW LEVEL SECURITY"));
                        }
                        drafts.push(pd);
                    }
                    for p in pols {
                        let pid = item_id(CopyObjectKind::Policy, &[sc, name, s(p, "name")]);
                        let mut pd = base(CopyObjectKind::Policy, pid.clone(), s(p, "name"), s(p, "name"), CopyPhase::PostData);
                        pd.parent = Some(id.clone());
                        pd.depends_on.push(id.clone());
                        pd.depends_on.extend(deps_of(&pid));
                        pd.exists_key = Some(TKey::of(TClass::Policy, Some(&ts), Some(&tname), s(p, "name")));
                        let roles = json_strings(o(p, "roles"));
                        pd.create = vec![
                            search_path(&ts),
                            create_policy(s(p, "name"), &q, b(p, "permissive"), s(p, "cmd"), &roles, o(p, "qual").as_deref(), o(p, "withcheck").as_deref()),
                        ];
                        pd.drop = vec![format!("DROP POLICY {} ON {q} RESTRICT", dq(s(p, "name")))];
                        drafts.push(pd);
                    }
                    if comments {
                        let mut stmts = Vec::new();
                        if let Some(cm) = o(r, "comment") {
                            stmts.push(format!("COMMENT ON TABLE {q} IS {}", pg_str(&cm)));
                        }
                        for col in &cols {
                            if let Some(cm) = o(col, "comment") {
                                stmts.push(format!("COMMENT ON COLUMN {q}.{} IS {}", dq(s(col, "name")), pg_str(&cm)));
                            }
                        }
                        add_comment(&mut comment_drafts, &id, stmts, &ts);
                    }
                    add_owner_grants(&mut owner_drafts, &id, &format!("TABLE {q}"), s(r, "owner"), json_rows(o(r, "grants")));
                    drafts.push(d);
                }
            }
        }
    }

    // ---- extensions (and their configuration tables' rows)
    if opts.objects.extensions {
        for r in &extensions {
            let name = s(r, "name");
            if name == "plpgsql" {
                continue;
            }
            let ext_schema = s(r, "schema");
            let id = item_id(CopyObjectKind::Extension, &[name]);
            let mut d = Draft::new(CopyObjectKind::Extension, id.clone(), name, name, CopyPhase::PreData);
            let ts = if copied.contains(ext_schema) { ts_of(ext_schema) } else { ext_schema.to_string() };
            d.target_schema = Some(ts.clone());
            d.create = vec![format!("CREATE EXTENSION IF NOT EXISTS {} WITH SCHEMA {}", dq(name), dq(&ts))];
            d.exists_key = Some(TKey::of(TClass::Extension, None, None, name));
            d.support = true;
            if copied.contains(ext_schema) {
                d.depends_on.push(item_id(CopyObjectKind::Schema, &[ext_schema]));
            }
            match tgt.available.get(name) {
                None if !tgt.available.is_empty() => d.blockers.push(danger("PG_EXTENSION_MISSING", format!("extension {name} is not available on the target"))),
                Some((default, installed, trusted)) => {
                    let tv = installed.clone().or_else(|| default.clone());
                    if tv.as_deref().is_some_and(|v| v != s(r, "version")) {
                        d.warnings.push(info("PG_EXTENSION_VERSION", format!("extension {name}: source {} / target {}", s(r, "version"), tv.unwrap_or_default())));
                    }
                    if !t_superuser && !trusted && installed.is_none() {
                        d.warnings.push(warning("PG_EXTENSION_PRIVILEGE", format!("extension {name} is not trusted: creating it needs a superuser")));
                    }
                }
                None => {}
            }
            drafts.push(d);
            // Configuration tables: rows matching extcondition, appended.
            for cfg in json_rows(o(r, "config")) {
                let (cs, ct, cond) = (
                    cfg.first().cloned().flatten().unwrap_or_default(),
                    cfg.get(1).cloned().flatten().unwrap_or_default(),
                    cfg.get(2).cloned().flatten().unwrap_or_default(),
                );
                let Some(cat) = cats.iter().find(|c| c.name == cs) else { continue };
                let Some(rel) = cat.relations.iter().find(|x| s(x, "name") == ct) else { continue };
                let cols: Vec<&Row> = cat.columns.iter().filter(|x| s(x, "rel") == s(rel, "oid") && !matches!(s(x, "generated"), "s" | "v")).collect();
                let tts = ts_of(&cs);
                let q = dq2(&tts, &ct);
                let tid = item_id(CopyObjectKind::Table, &[&cs, &ct]);
                let mut td = Draft::new(CopyObjectKind::Table, tid.clone(), &ct, &ct, CopyPhase::Data);
                td.schema = Some(cs.clone());
                td.target_schema = Some(tts.clone());
                td.always_append = true;
                td.depends_on.push(id.clone());
                td.exists_key = Some(TKey::rel(Some(&tts), &ct));
                td.warnings.push(info("EXT_CONFIG_ROWS", format!("rows of extension {name}'s configuration table that the extension did not create are appended")));
                let data_cols: Vec<DataColumn> = cols
                    .iter()
                    .map(|c| DataColumn {
                        source: s(c, "name").to_string(),
                        target: s(c, "name").to_string(),
                        value: ValueKind::Raw,
                        read: SourceRead::Plain,
                        zero_date: ZeroDateAction::NotTemporal,
                        invalid_date: ZeroDateAction::NotTemporal,
                        binary_attr: false,
                        max_bytes: None,
                    })
                    .collect();
                let list = data_cols.iter().map(|c| dq(&c.source)).collect::<Vec<_>>().join(", ");
                let spec = TableDataSpec {
                    item_id: tid.clone(),
                    source_sql: format!("SELECT {list} FROM ONLY {} {cond}", dq2(&cs, &ct)).trim_end().to_string(),
                    verify_sql: format!("SELECT {list} FROM ONLY {q}"),
                    raw_csv: true,
                    transcode: if sql_ascii { opts.pg.source_encoding } else { None },
                    target: TargetTable { schema: Some(tts.clone()), name: ct.clone() },
                    columns: data_cols,
                    est_rows: None,
                    first_timestamp_auto: false,
                    fk_checks: false,
                };
                td.load_template = Some(crate::copy::target::pg::copy_in_sql(&spec));
                td.data = Some(spec);
                td.selected = selected(req, Some(&cs), &ct);
                drafts.push(td);
            }
        }
    }
    drafts.extend(comment_drafts);
    drafts.extend(owner_drafts);

    // ---- plan-level notes
    if lo_columns && b(&facts, "has_lo") {
        warnings.push(warning("PG_LARGE_OBJECTS", "oid / lo columns point at large objects, which are not copied"));
    }
    if money_columns && o(&tgt.facts, "lc_monetary").is_some() && o(&tgt.facts, "lc_monetary") != o(&facts, "lc_monetary") {
        warnings.push(warning("PG_MONEY_LOCALE", "lc_monetary differs: COPY text of money values depends on it"));
    }
    if tablespace {
        warnings.push(info("PG_TABLESPACE_IGNORED", "tablespaces are ignored: objects go to the target's default tablespace"));
    }
    if !invalid_indexes.is_empty() {
        warnings.push(info("INVALID_INDEX_SKIPPED", "invalid indexes are not copied").detail(invalid_indexes.join(", ")));
    }
    let _ = secdef;
    let _ = Severity::Info;

    let lock_timeout = opts.target_lock_timeout_s.unwrap_or(5).clamp(1, 600);
    let mut target_session = crate::copy::target::pg::write_session_sql(lock_timeout);
    target_session.push("SET check_function_bodies = off".into());
    let c = crate::copy::plan::concurrency(opts, CopyFamily::Postgres);
    Ok(Collected {
        drafts,
        warnings,
        blockers,
        source_facts: CopyServerFacts {
            driver: "postgres".into(),
            version: input.source_server.version.clone(),
            database: req.source.database.clone(),
            endpoint: crate::copy::target::endpoint_text(input.source_conn),
            charset: Some(src_encoding),
            collation: o(&facts, "datcollate"),
            user: o(&facts, "usr").or_else(|| input.source_server.user.clone()),
            notes: Vec::new(),
        },
        target_facts: CopyServerFacts {
            driver: "postgres".into(),
            version: tf.version.clone(),
            database: req.target.database.clone(),
            endpoint: if tf.endpoint.is_empty() { crate::copy::target::endpoint_text(input.target_conn) } else { tf.endpoint.clone() },
            charset: if tenc.is_empty() { None } else { Some(tenc) },
            collation: o(&tgt.facts, "datcollate").or_else(|| tf.vars.get("datcollate").cloned()),
            user: tf.user.clone(),
            notes: Vec::new(),
        },
        effective: CopyEffective {
            charset: None,
            collation: None,
            engine: None,
            zero_dates: None,
            time_zone: None,
            load_method: LoadMethod::PgCopyCsv,
            table_concurrency: c,
            chunk_bytes: None,
            consistent_snapshot: true,
            source_sessions: 1,
            target_sessions: 2,
        },
        session_sql: SessionSql { source: crate::copy::target::pg::source_session_sql(sql_ascii && opts.pg.source_encoding.is_some()), target: target_session },
        target: tgt.state,
        ops: ops(),
        target_db_missing: false,
    })
}

#[cfg(test)]
mod tests;
