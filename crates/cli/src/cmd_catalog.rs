//! `catalog`: databases, schemas, tables, columns, indexes, DDL and the exact-count job.

use std::process::ExitCode;
use std::sync::Arc;

use anyhow::{bail, Context};
use bulk_core::types::{JobState, TableRef};
use bulk_core::BulkDb;

use crate::args::{CatalogArgs, CatalogCmd, CatalogCommon, CatalogCountArgs, CatalogTableArgs};
use crate::connect;
use crate::output::{enum_name, fmt_duration_ms, human_count, opt, opt_bytes, print_json, yes_no, Align, Table};
use crate::progress::{wait_for_job, WaitOptions};
use crate::{EXIT_CANCELLED, EXIT_DONE, EXIT_FAILED};

/// `NAME`, `SCHEMA.NAME` or `DB.SCHEMA.NAME`, with explicit `--db` / `--schema` taking
/// precedence over the qualified parts.
pub fn parse_table_ref(spec: &str, db: Option<&str>, schema: Option<&str>) -> anyhow::Result<TableRef> {
    let parts: Vec<&str> = spec.split('.').collect();
    let (d, s, n) = match parts.as_slice() {
        [n] => (None, None, *n),
        [s, n] => (None, Some(*s), *n),
        [d, s, n] => (Some(*d), Some(*s), *n),
        _ => bail!("--table must be NAME, SCHEMA.NAME or DB.SCHEMA.NAME (got '{spec}')"),
    };
    if n.trim().is_empty() {
        bail!("--table has an empty table name ('{spec}')");
    }
    Ok(TableRef {
        database: db.map(str::to_string).or_else(|| d.map(str::to_string)).filter(|x| !x.is_empty()),
        schema: schema.map(str::to_string).or_else(|| s.map(str::to_string)).filter(|x| !x.is_empty()),
        name: n.to_string(),
    })
}

fn table_ref(args: &CatalogTableArgs) -> anyhow::Result<TableRef> {
    parse_table_ref(&args.table, args.common.db.as_deref(), args.schema.as_deref())
}

/// Resolve the connection of a table-level listing. Columns, indexes and DDL have no per-call
/// refresh in the core, so `--refresh` drops the connection's cached metadata
/// (`catalog_invalidate`, the desktop tree's refresh) before the lookup.
async fn resolve_for_table(db: &Arc<BulkDb>, args: &CatalogTableArgs) -> anyhow::Result<connect::ConnHandle> {
    let handle = connect::resolve(db, &args.common.target).await?;
    if args.common.refresh {
        db.catalog_invalidate(handle.id());
    }
    Ok(handle)
}

pub async fn run(db: &Arc<BulkDb>, args: CatalogArgs, json: bool) -> anyhow::Result<ExitCode> {
    match args.cmd {
        CatalogCmd::Databases(c) => {
            let handle = connect::resolve(db, &c.target).await?;
            let list = db.catalog_databases(handle.id(), c.refresh).await?;
            if json {
                print_json(&list)?;
            } else {
                let mut t = Table::new(&[
                    ("NAME", Align::Left),
                    ("SIZE", Align::Right),
                    ("ENCODING", Align::Left),
                    ("COLLATION", Align::Left),
                    ("ACCESS", Align::Left),
                    ("CURRENT", Align::Left),
                ]);
                for d in &list {
                    t.row(vec![
                        d.name.clone(),
                        opt_bytes(&d.size_bytes),
                        opt(&d.encoding),
                        opt(&d.collation),
                        yes_no(d.accessible).into(),
                        if d.is_current { "*".into() } else { String::new() },
                    ]);
                }
                t.print();
            }
        }
        CatalogCmd::Schemas(c) => {
            let handle = connect::resolve(db, &c.target).await?;
            let database = database_for(&c, handle.conn());
            let list = db.catalog_schemas(handle.id(), &database, c.refresh).await?;
            if json {
                print_json(&list)?;
            } else {
                let mut t = Table::new(&[("SCHEMA", Align::Left), ("OWNER", Align::Left), ("TABLES", Align::Right)]);
                for s in &list {
                    t.row(vec![s.name.clone(), opt(&s.owner), s.table_count.map(human_count).unwrap_or_else(|| "-".into())]);
                }
                t.print();
            }
        }
        CatalogCmd::Tables(a) => {
            let handle = connect::resolve(db, &a.common.target).await?;
            let database = database_for(&a.common, handle.conn());
            let schema = a.schema.clone().unwrap_or_default();
            let list = db.catalog_tables(handle.id(), &database, &schema, a.common.refresh).await?;
            if json {
                print_json(&list)?;
            } else {
                let mut t = Table::new(&[
                    ("SCHEMA", Align::Left),
                    ("NAME", Align::Left),
                    ("KIND", Align::Left),
                    ("~ROWS", Align::Right),
                    ("TABLE", Align::Right),
                    ("INDEXES", Align::Right),
                    ("TOTAL", Align::Right),
                    ("COMMENT", Align::Left),
                ]);
                for x in &list {
                    t.row(vec![
                        x.schema.clone(),
                        x.name.clone(),
                        enum_name(&x.kind),
                        x.estimated_rows.map(|r| format!("~{}", human_count(r))).unwrap_or_else(|| "-".into()),
                        opt_bytes(&x.table_bytes),
                        opt_bytes(&x.index_bytes),
                        opt_bytes(&x.total_bytes),
                        opt(&x.comment),
                    ]);
                }
                t.print();
                if !list.is_empty() {
                    println!("({} objects; row counts are estimates, use `catalog count` for exact numbers)", list.len());
                }
            }
        }
        CatalogCmd::Columns(a) => {
            let handle = resolve_for_table(db, &a).await?;
            let table = table_ref(&a)?;
            let list = db.catalog_columns(handle.id(), &table).await?;
            if json {
                print_json(&list)?;
            } else {
                let mut t = Table::new(&[
                    ("#", Align::Right),
                    ("NAME", Align::Left),
                    ("TYPE", Align::Left),
                    ("NULL", Align::Left),
                    ("DEFAULT", Align::Left),
                    ("PK", Align::Left),
                    ("UNIQUE", Align::Left),
                    ("FK", Align::Left),
                    ("COMMENT", Align::Left),
                ]);
                for c in &list {
                    t.row(vec![
                        c.position.to_string(),
                        c.name.clone(),
                        c.data_type.clone(),
                        if c.nullable { "YES".into() } else { "NO".into() },
                        opt(&c.default),
                        if c.primary_key { "PK".into() } else { String::new() },
                        if c.unique { "U".into() } else { String::new() },
                        opt(&c.foreign_key),
                        opt(&c.comment),
                    ]);
                }
                t.print();
            }
        }
        CatalogCmd::Indexes(a) => {
            let handle = resolve_for_table(db, &a).await?;
            let table = table_ref(&a)?;
            let list = db.catalog_indexes(handle.id(), &table).await?;
            if json {
                print_json(&list)?;
            } else {
                let mut t = Table::new(&[
                    ("NAME", Align::Left),
                    ("COLUMNS", Align::Left),
                    ("UNIQUE", Align::Left),
                    ("PRIMARY", Align::Left),
                    ("KIND", Align::Left),
                    ("SIZE", Align::Right),
                    ("SCANS", Align::Right),
                    ("INCLUDE", Align::Left),
                    ("PREDICATE", Align::Left),
                ]);
                for i in &list {
                    t.row(vec![
                        i.name.clone(),
                        i.columns.join(", "),
                        yes_no(i.unique).into(),
                        yes_no(i.primary).into(),
                        opt(&i.kind),
                        opt_bytes(&i.size_bytes),
                        i.scans.map(human_count).unwrap_or_else(|| "-".into()),
                        i.include.join(", "),
                        opt(&i.predicate),
                    ]);
                }
                t.print();
            }
        }
        CatalogCmd::Ddl(a) => {
            let handle = resolve_for_table(db, &a).await?;
            let table = table_ref(&a)?;
            let ddl = db.catalog_ddl(handle.id(), &table).await?;
            if json {
                print_json(&serde_json::json!({ "table": table, "ddl": ddl }))?;
            } else {
                println!("{}", ddl.trim_end());
            }
        }
        CatalogCmd::Count(CatalogCountArgs { table: a, cached }) => {
            let handle = connect::resolve(db, &a.common.target).await?;
            let table = table_ref(&a)?;
            if cached {
                return print_cached_count(db, &handle, &table, json);
            }
            let mut events = db.subscribe();
            let job_id = db.catalog_exact_count(handle.id(), &table).await.context("starting the count job")?;
            if !json {
                eprintln!("counting {} (job {job_id}; Ctrl+C cancels)...", table.qualified());
            }
            let outcome = wait_for_job(db, &mut events, &job_id, WaitOptions::default()).await?;
            if json {
                print_json(&serde_json::json!({
                    "jobId": job_id,
                    "state": outcome.done.state,
                    "exactCount": outcome.done.exact_count,
                    "error": outcome.done.error,
                }))?;
            } else {
                match (&outcome.done.exact_count, &outcome.done.error) {
                    (Some(c), _) => println!("{}: {} rows (exact, {} at {})", c.table.qualified(), human_count(c.rows), fmt_duration_ms(c.elapsed_ms), c.at),
                    (None, Some(e)) => println!("{} [{}]: {}", enum_name(&outcome.done.state), e.code, e.message),
                    (None, None) => println!("{} after {}", enum_name(&outcome.done.state), fmt_duration_ms(outcome.elapsed.as_millis() as u64)),
                }
            }
            drop(handle);
            return Ok(ExitCode::from(match outcome.done.state {
                JobState::Done => EXIT_DONE,
                JobState::Cancelled => EXIT_CANCELLED,
                _ => EXIT_FAILED,
            }));
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// `catalog count --cached`: the last exact count stored for `table` on this connection (what
/// the desktop shows next to the estimate), read from the local history database without
/// contacting the server. Exit 0 with the count, 1 when the table was never counted.
fn print_cached_count(db: &BulkDb, handle: &connect::ConnHandle, table: &TableRef, json: bool) -> anyhow::Result<ExitCode> {
    let stored = db.catalog_last_exact_count(handle.id(), table).context("reading the stored exact count")?;
    if json {
        print_json(&serde_json::json!({ "table": table, "cached": true, "exactCount": stored }))?;
    } else {
        match &stored {
            Some(c) => {
                println!("{}: {} rows (exact, {} at {}; stored count, not re-counted)", table.qualified(), human_count(c.rows), fmt_duration_ms(c.elapsed_ms), c.at)
            }
            None => {
                println!("{}: no exact count stored for connection '{}'", table.qualified(), handle.conn().name);
                eprintln!("run `bulk-cli catalog count` without --cached to count it now");
            }
        }
    }
    Ok(ExitCode::from(if stored.is_some() { EXIT_DONE } else { EXIT_FAILED }))
}

/// `--db`, else the connection's database, else empty (the driver's current database).
fn database_for(c: &CatalogCommon, conn: &bulk_core::types::Connection) -> String {
    c.db.clone().or_else(|| conn.database.clone()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_refs() {
        let t = parse_table_ref("orders", None, None).unwrap();
        assert_eq!((t.database, t.schema, t.name.as_str()), (None, None, "orders"));
        let t = parse_table_ref("public.orders", None, None).unwrap();
        assert_eq!((t.schema.as_deref(), t.name.as_str()), (Some("public"), "orders"));
        let t = parse_table_ref("prod.dbo.orders", None, None).unwrap();
        assert_eq!((t.database.as_deref(), t.schema.as_deref(), t.name.as_str()), (Some("prod"), Some("dbo"), "orders"));
        let t = parse_table_ref("x.orders", Some("db2"), Some("s2")).unwrap();
        assert_eq!((t.database.as_deref(), t.schema.as_deref()), (Some("db2"), Some("s2")));
        assert!(parse_table_ref("a.b.c.d", None, None).is_err());
        assert!(parse_table_ref("public.", None, None).is_err());
    }
}
