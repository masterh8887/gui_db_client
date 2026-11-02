//! PostgreSQL planning over canned catalog rows: identity vs serial, enums and domains (NOT VALID
//! checks), extension members and configuration tables, partitions, inheritance, views and
//! matviews, functions (header rewrite, rowtype order, PG17 search_path), triggers with enable
//! states, policies, comments, FK placement, setval templates, source and target checks.

use tokio_util::sync::CancellationToken;

use super::*;
use crate::copy::plan::{build_plan, PlanInput};
use crate::copy::schema::SourceMeta;
use crate::copy::testkit::{CannedMeta, RecordingTarget};
use crate::types::{Connection, ServerInfo};

fn t(cols: &[&str], rows: &[&[Option<&str>]]) -> MetaTable {
    MetaTable::from_rows(cols, rows)
}

const N: Option<&str> = None;

fn source(encoding: &str, version_num: &str) -> CannedMeta {
    CannedMeta::new()
        .answer(
            "bulkdb:pg:facts",
            t(
                &["version_num", "encoding", "usr", "superuser", "bypassrls", "datcollate", "datctype", "locprovider", "lc_monetary", "has_lo"],
                &[&[
                    Some(version_num),
                    Some(encoding),
                    Some("copier"),
                    Some("f"),
                    Some("f"),
                    Some("ko_KR.UTF-8"),
                    Some("ko_KR.UTF-8"),
                    Some("c"),
                    Some("ko_KR.UTF-8"),
                    Some("f"),
                ]],
            ),
        )
        .answer("bulkdb:pg:schemas", t(&["oid", "name", "owner", "comment", "grants"], &[&[Some("2200"), Some("app"), Some("owner"), Some("application"), N]]))
        .answer(
            "bulkdb:pg:extensions",
            t(
                &["oid", "name", "schema", "version", "config"],
                &[
                    &[Some("13000"), Some("plpgsql"), Some("pg_catalog"), Some("1.0"), N],
                    &[Some("80001"), Some("citext"), Some("public"), Some("1.6"), N],
                    &[Some("80002"), Some("postgis"), Some("app"), Some("3.4.2"), Some(r#"[["app","spatial_ref_sys","WHERE srid > 999000"]]"#)],
                ],
            ),
        )
        .answer(
            "bulkdb:pg:ext_members",
            t(
                &["cls", "objid", "ext"],
                &[&[Some("pg_type"), Some("90001"), Some("80001")], &[Some("pg_class"), Some("30009"), Some("80002")], &[Some("pg_proc"), Some("40009"), Some("80002")]],
            ),
        )
        .answer(
            "bulkdb:pg:deps",
            t(
                &["cls", "objid", "refcls", "refobjid"],
                &[
                    &[Some("pg_class"), Some("30001"), Some("pg_type"), Some("50001")],
                    &[Some("pg_class"), Some("30001"), Some("pg_class"), Some("60002")],
                    &[Some("pg_class"), Some("30002"), Some("pg_type"), Some("90001")],
                    &[Some("pg_class"), Some("30003"), Some("pg_class"), Some("30001")],
                    &[Some("pg_class"), Some("30004"), Some("pg_class"), Some("30003")],
                    &[Some("pg_class"), Some("30004"), Some("pg_proc"), Some("40001")],
                    &[Some("pg_trigger"), Some("70001"), Some("pg_proc"), Some("40002")],
                    &[Some("pg_proc"), Some("40001"), Some("pg_type"), Some("31001")],
                    &[Some("pg_constraint"), Some("75001"), Some("pg_class"), Some("30002")],
                    &[Some("pg_constraint"), Some("75001"), Some("pg_class"), Some("32001")],
                ],
            ),
        )
        .answer("bulkdb:pg:role_settings", t(&["role", "config"], &[&[Some("all roles"), Some(r#"["search_path=app, public"]"#)]]))
        .answer("bulkdb:pg:db_unsupported", t(&["kind", "name", "cls", "oid"], &[&[Some("cast"), Some("text AS mood"), Some("pg_cast"), Some("99001")]]))
        .answer(
            "bulkdb:pg:types",
            t(
                &["oid", "name", "type", "labels", "base", "notnull", "dflt", "collation", "attrs", "range", "owner", "comment"],
                &[
                    &[Some("50001"), Some("mood"), Some("e"), Some(r#"["sad","ok"]"#), N, Some("f"), N, N, N, N, Some("owner"), Some("feelings")],
                    &[Some("50002"), Some("posint"), Some("d"), N, Some("integer"), Some("f"), N, N, N, N, Some("owner"), N],
                    &[Some("90001"), Some("citext"), Some("b"), N, N, Some("f"), N, N, N, N, Some("owner"), N],
                ],
            ),
        )
        .answer(
            "bulkdb:pg:sequences",
            t(
                &[
                    "oid",
                    "name",
                    "type",
                    "start",
                    "increment",
                    "max",
                    "min",
                    "cache",
                    "cycle",
                    "deptype",
                    "owner_schema",
                    "owner_table",
                    "owner_column",
                    "owner",
                    "comment",
                    "grants",
                ],
                &[
                    &[
                        Some("60001"),
                        Some("orders_id_seq"),
                        Some("integer"),
                        Some("1"),
                        Some("1"),
                        Some("2147483647"),
                        Some("1"),
                        Some("1"),
                        Some("f"),
                        Some("i"),
                        Some("app"),
                        Some("orders"),
                        Some("id"),
                        Some("owner"),
                        N,
                        N,
                    ],
                    &[
                        Some("60002"),
                        Some("orders_n_seq"),
                        Some("bigint"),
                        Some("1"),
                        Some("1"),
                        Some("9223372036854775807"),
                        Some("1"),
                        Some("1"),
                        Some("f"),
                        Some("a"),
                        Some("app"),
                        Some("orders"),
                        Some("n"),
                        Some("owner"),
                        N,
                        N,
                    ],
                    &[
                        Some("60003"),
                        Some("down_seq"),
                        Some("bigint"),
                        Some("-1"),
                        Some("-1"),
                        Some("-1"),
                        Some("-9223372036854775808"),
                        Some("1"),
                        Some("f"),
                        N,
                        N,
                        N,
                        N,
                        Some("owner"),
                        N,
                        N,
                    ],
                ],
            ),
        )
        .answer(
            "bulkdb:pg:functions",
            t(
                &["oid", "name", "args", "kind", "qname", "secdef", "config", "owner", "comment", "grants", "def"],
                &[
                    &[
                        Some("40001"),
                        Some("f"),
                        Some("o orders"),
                        Some("f"),
                        Some("app.f"),
                        Some("f"),
                        N,
                        Some("owner"),
                        N,
                        N,
                        Some("CREATE OR REPLACE FUNCTION app.f(o orders)\n RETURNS integer\n LANGUAGE sql\nAS $function$select o.id$function$\n"),
                    ],
                    &[
                        Some("40002"),
                        Some("trg_fn"),
                        Some(""),
                        Some("f"),
                        Some("app.trg_fn"),
                        Some("f"),
                        Some(r#"["search_path=app"]"#),
                        Some("owner"),
                        N,
                        N,
                        Some("CREATE OR REPLACE FUNCTION app.trg_fn()\n RETURNS trigger\n LANGUAGE plpgsql\nAS $function$begin return new; end$function$\n"),
                    ],
                    &[Some("40003"), Some("agg_sum"), Some("integer"), Some("a"), Some("app.agg_sum"), Some("f"), N, Some("owner"), N, N, N],
                    &[
                        Some("40004"),
                        Some("sec_f"),
                        Some(""),
                        Some("f"),
                        Some("app.sec_f"),
                        Some("t"),
                        N,
                        Some("owner"),
                        N,
                        N,
                        Some("CREATE OR REPLACE FUNCTION app.sec_f()\n RETURNS void\n LANGUAGE sql\n SECURITY DEFINER\nAS $function$select$function$\n"),
                    ],
                    &[
                        Some("40009"),
                        Some("st_x"),
                        Some("geometry"),
                        Some("f"),
                        Some("app.st_x"),
                        Some("f"),
                        N,
                        Some("owner"),
                        N,
                        N,
                        Some("CREATE OR REPLACE FUNCTION app.st_x(geometry) …"),
                    ],
                ],
            ),
        )
        .answer(
            "bulkdb:pg:relations",
            t(
                &[
                    "oid",
                    "name",
                    "kind",
                    "persistence",
                    "is_partition",
                    "bound",
                    "partkey",
                    "reloptions",
                    "rls",
                    "force_rls",
                    "replident",
                    "est_rows",
                    "bytes",
                    "owner",
                    "is_owner",
                    "comment",
                    "has_tablespace",
                    "viewdef",
                    "parents",
                    "ext_member",
                    "typed",
                    "rowtype",
                    "grants",
                ],
                &[
                    &[
                        Some("30001"),
                        Some("orders"),
                        Some("r"),
                        Some("p"),
                        Some("f"),
                        N,
                        N,
                        N,
                        Some("f"),
                        Some("f"),
                        Some("d"),
                        Some("1000"),
                        Some("65536"),
                        Some("owner"),
                        Some("f"),
                        Some("the orders"),
                        Some("f"),
                        N,
                        N,
                        Some("f"),
                        Some("f"),
                        Some("31001"),
                        N,
                    ],
                    &[
                        Some("30002"),
                        Some("customers"),
                        Some("r"),
                        Some("p"),
                        Some("f"),
                        N,
                        N,
                        N,
                        Some("t"),
                        Some("f"),
                        Some("d"),
                        Some("-1"),
                        Some("8192"),
                        Some("owner"),
                        Some("f"),
                        N,
                        Some("f"),
                        N,
                        N,
                        Some("f"),
                        Some("f"),
                        Some("31002"),
                        N,
                    ],
                    &[
                        Some("30003"),
                        Some("v_orders"),
                        Some("v"),
                        Some("p"),
                        Some("f"),
                        N,
                        N,
                        Some(r#"["security_invoker=true"]"#),
                        Some("f"),
                        Some("f"),
                        Some("n"),
                        Some("-1"),
                        N,
                        Some("owner"),
                        Some("f"),
                        N,
                        Some("f"),
                        Some(" SELECT id\n   FROM orders;"),
                        N,
                        Some("f"),
                        Some("f"),
                        Some("31003"),
                        N,
                    ],
                    &[
                        Some("30004"),
                        Some("mv"),
                        Some("m"),
                        Some("p"),
                        Some("f"),
                        N,
                        N,
                        N,
                        Some("f"),
                        Some("f"),
                        Some("d"),
                        Some("0"),
                        Some("0"),
                        Some("owner"),
                        Some("f"),
                        N,
                        Some("f"),
                        Some(" SELECT f(o.*) AS f\n   FROM v_orders o;"),
                        N,
                        Some("f"),
                        Some("f"),
                        Some("31004"),
                        N,
                    ],
                    &[
                        Some("30005"),
                        Some("meas"),
                        Some("p"),
                        Some("p"),
                        Some("f"),
                        N,
                        Some("RANGE (logdate)"),
                        N,
                        Some("f"),
                        Some("f"),
                        Some("d"),
                        Some("0"),
                        Some("0"),
                        Some("owner"),
                        Some("f"),
                        N,
                        Some("f"),
                        N,
                        N,
                        Some("f"),
                        Some("f"),
                        Some("31005"),
                        N,
                    ],
                    &[
                        Some("30006"),
                        Some("meas_2024"),
                        Some("r"),
                        Some("p"),
                        Some("t"),
                        Some("FOR VALUES FROM ('2024-01-01') TO ('2025-01-01')"),
                        N,
                        N,
                        Some("f"),
                        Some("f"),
                        Some("d"),
                        Some("10"),
                        Some("8192"),
                        Some("owner"),
                        Some("f"),
                        N,
                        Some("f"),
                        N,
                        Some(r#"[["app","meas"]]"#),
                        Some("f"),
                        Some("f"),
                        Some("31006"),
                        N,
                    ],
                    &[
                        Some("30007"),
                        Some("base"),
                        Some("r"),
                        Some("p"),
                        Some("f"),
                        N,
                        N,
                        N,
                        Some("f"),
                        Some("f"),
                        Some("d"),
                        Some("5"),
                        Some("8192"),
                        Some("owner"),
                        Some("f"),
                        N,
                        Some("f"),
                        N,
                        N,
                        Some("f"),
                        Some("f"),
                        Some("31007"),
                        N,
                    ],
                    &[
                        Some("30008"),
                        Some("child"),
                        Some("r"),
                        Some("p"),
                        Some("f"),
                        N,
                        N,
                        N,
                        Some("f"),
                        Some("f"),
                        Some("d"),
                        Some("3"),
                        Some("8192"),
                        Some("owner"),
                        Some("f"),
                        N,
                        Some("f"),
                        N,
                        Some(r#"[["app","base"]]"#),
                        Some("f"),
                        Some("f"),
                        Some("31008"),
                        N,
                    ],
                    &[
                        Some("30009"),
                        Some("spatial_ref_sys"),
                        Some("r"),
                        Some("p"),
                        Some("f"),
                        N,
                        N,
                        N,
                        Some("f"),
                        Some("f"),
                        Some("d"),
                        Some("8500"),
                        Some("1"),
                        Some("owner"),
                        Some("f"),
                        N,
                        Some("f"),
                        N,
                        N,
                        Some("t"),
                        Some("f"),
                        Some("31009"),
                        N,
                    ],
                ],
            ),
        )
        .answer(
            "bulkdb:pg:columns",
            t(
                &[
                    "rel",
                    "num",
                    "name",
                    "type",
                    "notnull",
                    "dflt",
                    "identity",
                    "generated",
                    "local",
                    "collation",
                    "coll_schema",
                    "coll_name",
                    "stattarget",
                    "storage",
                    "typstorage",
                    "compression",
                    "comment",
                    "typname",
                ],
                &[
                    &[
                        Some("30001"),
                        Some("1"),
                        Some("id"),
                        Some("integer"),
                        Some("t"),
                        N,
                        Some("d"),
                        Some(""),
                        Some("t"),
                        N,
                        N,
                        N,
                        Some("-1"),
                        Some("p"),
                        Some("p"),
                        N,
                        N,
                        Some("int4"),
                    ],
                    &[
                        Some("30001"),
                        Some("2"),
                        Some("n"),
                        Some("bigint"),
                        Some("t"),
                        Some("nextval('orders_n_seq'::regclass)"),
                        Some(""),
                        Some(""),
                        Some("t"),
                        N,
                        N,
                        N,
                        Some("200"),
                        Some("p"),
                        Some("p"),
                        N,
                        N,
                        Some("int8"),
                    ],
                    &[
                        Some("30001"),
                        Some("3"),
                        Some("m"),
                        Some("mood"),
                        Some("f"),
                        N,
                        Some(""),
                        Some(""),
                        Some("t"),
                        N,
                        N,
                        N,
                        Some("-1"),
                        Some("p"),
                        Some("p"),
                        N,
                        N,
                        Some("mood"),
                    ],
                    &[
                        Some("30001"),
                        Some("4"),
                        Some("total"),
                        Some("numeric"),
                        Some("f"),
                        Some("n * 2"),
                        Some(""),
                        Some("s"),
                        Some("t"),
                        N,
                        N,
                        N,
                        Some("-1"),
                        Some("m"),
                        Some("m"),
                        N,
                        N,
                        Some("numeric"),
                    ],
                    &[
                        Some("30001"),
                        Some("5"),
                        Some("note"),
                        Some("text"),
                        Some("f"),
                        N,
                        Some(""),
                        Some(""),
                        Some("t"),
                        Some("pg_catalog.\"C\""),
                        Some("pg_catalog"),
                        Some("C"),
                        Some("-1"),
                        Some("e"),
                        Some("x"),
                        Some("l"),
                        Some("메모"),
                        Some("text"),
                    ],
                    &[
                        Some("30002"),
                        Some("1"),
                        Some("id"),
                        Some("integer"),
                        Some("t"),
                        N,
                        Some(""),
                        Some(""),
                        Some("t"),
                        N,
                        N,
                        N,
                        Some("-1"),
                        Some("p"),
                        Some("p"),
                        N,
                        N,
                        Some("int4"),
                    ],
                    &[
                        Some("30002"),
                        Some("2"),
                        Some("email"),
                        Some("citext"),
                        Some("f"),
                        N,
                        Some(""),
                        Some(""),
                        Some("t"),
                        N,
                        N,
                        N,
                        Some("-1"),
                        Some("x"),
                        Some("x"),
                        N,
                        N,
                        Some("citext"),
                    ],
                    &[
                        Some("30003"),
                        Some("1"),
                        Some("id"),
                        Some("integer"),
                        Some("f"),
                        N,
                        Some(""),
                        Some(""),
                        Some("t"),
                        N,
                        N,
                        N,
                        Some("-1"),
                        Some("p"),
                        Some("p"),
                        N,
                        N,
                        Some("int4"),
                    ],
                    &[
                        Some("30005"),
                        Some("1"),
                        Some("logdate"),
                        Some("date"),
                        Some("t"),
                        N,
                        Some(""),
                        Some(""),
                        Some("t"),
                        N,
                        N,
                        N,
                        Some("-1"),
                        Some("p"),
                        Some("p"),
                        N,
                        N,
                        Some("date"),
                    ],
                    &[
                        Some("30005"),
                        Some("2"),
                        Some("v"),
                        Some("integer"),
                        Some("f"),
                        N,
                        Some(""),
                        Some(""),
                        Some("t"),
                        N,
                        N,
                        N,
                        Some("-1"),
                        Some("p"),
                        Some("p"),
                        N,
                        N,
                        Some("int4"),
                    ],
                    &[
                        Some("30006"),
                        Some("1"),
                        Some("logdate"),
                        Some("date"),
                        Some("t"),
                        N,
                        Some(""),
                        Some(""),
                        Some("f"),
                        N,
                        N,
                        N,
                        Some("-1"),
                        Some("p"),
                        Some("p"),
                        N,
                        N,
                        Some("date"),
                    ],
                    &[
                        Some("30006"),
                        Some("2"),
                        Some("v"),
                        Some("integer"),
                        Some("f"),
                        N,
                        Some(""),
                        Some(""),
                        Some("f"),
                        N,
                        N,
                        N,
                        Some("-1"),
                        Some("p"),
                        Some("p"),
                        N,
                        N,
                        Some("int4"),
                    ],
                    &[
                        Some("30007"),
                        Some("1"),
                        Some("id"),
                        Some("integer"),
                        Some("f"),
                        N,
                        Some(""),
                        Some(""),
                        Some("t"),
                        N,
                        N,
                        N,
                        Some("-1"),
                        Some("p"),
                        Some("p"),
                        N,
                        N,
                        Some("int4"),
                    ],
                    &[
                        Some("30008"),
                        Some("1"),
                        Some("id"),
                        Some("integer"),
                        Some("f"),
                        N,
                        Some(""),
                        Some(""),
                        Some("f"),
                        N,
                        N,
                        N,
                        Some("-1"),
                        Some("p"),
                        Some("p"),
                        N,
                        N,
                        Some("int4"),
                    ],
                    &[
                        Some("30008"),
                        Some("2"),
                        Some("extra"),
                        Some("text"),
                        Some("f"),
                        N,
                        Some(""),
                        Some(""),
                        Some("t"),
                        N,
                        N,
                        N,
                        Some("-1"),
                        Some("x"),
                        Some("x"),
                        N,
                        N,
                        Some("text"),
                    ],
                    &[
                        Some("30009"),
                        Some("1"),
                        Some("srid"),
                        Some("integer"),
                        Some("t"),
                        N,
                        Some(""),
                        Some(""),
                        Some("t"),
                        N,
                        N,
                        N,
                        Some("-1"),
                        Some("p"),
                        Some("p"),
                        N,
                        N,
                        Some("int4"),
                    ],
                ],
            ),
        )
        .answer(
            "bulkdb:pg:constraints",
            t(
                &["oid", "rel", "typ", "name", "type", "def", "validated", "local", "enforced", "keys", "fschema", "ftable", "comment"],
                &[
                    &[Some("75010"), Some("30001"), Some("0"), Some("orders_pkey"), Some("p"), Some("PRIMARY KEY (id)"), Some("t"), Some("t"), N, Some("[1]"), N, N, N],
                    &[Some("75011"), Some("30001"), Some("0"), Some("orders_n_check"), Some("c"), Some("CHECK (n > 0)"), Some("t"), Some("t"), N, Some("[2]"), N, N, N],
                    &[
                        Some("75012"),
                        Some("30001"),
                        Some("0"),
                        Some("orders_m_check"),
                        Some("c"),
                        Some("CHECK (m IS NOT NULL) NOT VALID"),
                        Some("f"),
                        Some("t"),
                        N,
                        Some("[3]"),
                        N,
                        N,
                        N,
                    ],
                    &[
                        Some("75001"),
                        Some("30001"),
                        Some("0"),
                        Some("orders_customer_fk"),
                        Some("f"),
                        Some("FOREIGN KEY (n) REFERENCES customers(id) DEFERRABLE INITIALLY DEFERRED"),
                        Some("t"),
                        Some("t"),
                        N,
                        Some("[2]"),
                        Some("app"),
                        Some("customers"),
                        Some("to the customer"),
                    ],
                    &[
                        Some("75020"),
                        Some("30002"),
                        Some("0"),
                        Some("customers_pkey"),
                        Some("p"),
                        Some("PRIMARY KEY (id)"),
                        Some("t"),
                        Some("t"),
                        N,
                        Some("[1]"),
                        N,
                        N,
                        N,
                    ],
                    &[Some("75030"), Some("0"), Some("50002"), Some("posint_check"), Some("c"), Some("CHECK (VALUE > 0)"), Some("t"), Some("t"), N, N, N, N, N],
                    &[
                        Some("75031"),
                        Some("0"),
                        Some("50002"),
                        Some("posint_legacy"),
                        Some("c"),
                        Some("CHECK (VALUE < 1000) NOT VALID"),
                        Some("f"),
                        Some("t"),
                        N,
                        N,
                        N,
                        N,
                        N,
                    ],
                    &[Some("75040"), Some("30006"), Some("0"), Some("meas_v_check"), Some("c"), Some("CHECK (v > 0)"), Some("t"), Some("f"), N, Some("[2]"), N, N, N],
                ],
            ),
        )
        .answer(
            "bulkdb:pg:indexes",
            t(
                &[
                    "oid",
                    "rel",
                    "name",
                    "def",
                    "valid",
                    "clustered",
                    "replident",
                    "is_partition",
                    "backs_constraint",
                    "comment",
                    "has_tablespace",
                    "has_exprs",
                    "ext_member",
                ],
                &[
                    &[
                        Some("32010"),
                        Some("30001"),
                        Some("orders_pkey"),
                        Some("CREATE UNIQUE INDEX orders_pkey ON orders USING btree (id)"),
                        Some("t"),
                        Some("f"),
                        Some("f"),
                        Some("f"),
                        Some("t"),
                        N,
                        Some("f"),
                        Some("f"),
                        Some("f"),
                    ],
                    &[
                        Some("32001"),
                        Some("30002"),
                        Some("customers_pkey"),
                        Some("CREATE UNIQUE INDEX customers_pkey ON customers USING btree (id)"),
                        Some("t"),
                        Some("f"),
                        Some("f"),
                        Some("f"),
                        Some("t"),
                        N,
                        Some("f"),
                        Some("f"),
                        Some("f"),
                    ],
                    &[
                        Some("32002"),
                        Some("30001"),
                        Some("orders_m_idx"),
                        Some("CREATE INDEX orders_m_idx ON orders USING btree (m)"),
                        Some("t"),
                        Some("t"),
                        Some("f"),
                        Some("f"),
                        Some("f"),
                        Some("by mood"),
                        Some("t"),
                        Some("f"),
                        Some("f"),
                    ],
                    &[
                        Some("32003"),
                        Some("30001"),
                        Some("orders_bad_idx"),
                        Some("CREATE INDEX orders_bad_idx ON orders USING btree (n)"),
                        Some("f"),
                        Some("f"),
                        Some("f"),
                        Some("f"),
                        Some("f"),
                        N,
                        Some("f"),
                        Some("f"),
                        Some("f"),
                    ],
                    &[
                        Some("32004"),
                        Some("30005"),
                        Some("meas_idx"),
                        Some("CREATE INDEX meas_idx ON ONLY meas USING btree (v)"),
                        Some("t"),
                        Some("f"),
                        Some("f"),
                        Some("f"),
                        Some("f"),
                        N,
                        Some("f"),
                        Some("f"),
                        Some("f"),
                    ],
                    &[
                        Some("32005"),
                        Some("30006"),
                        Some("meas_2024_v_idx"),
                        Some("CREATE INDEX meas_2024_v_idx ON meas_2024 USING btree (v)"),
                        Some("t"),
                        Some("f"),
                        Some("f"),
                        Some("t"),
                        Some("f"),
                        N,
                        Some("f"),
                        Some("f"),
                        Some("f"),
                    ],
                ],
            ),
        )
        .answer(
            "bulkdb:pg:triggers",
            t(
                &["oid", "rel", "name", "def", "enabled", "internal", "parentid", "comment", "func"],
                &[
                    &[
                        Some("70001"),
                        Some("30001"),
                        Some("trg"),
                        Some("CREATE TRIGGER trg BEFORE INSERT ON orders FOR EACH ROW EXECUTE FUNCTION trg_fn()"),
                        Some("D"),
                        Some("f"),
                        Some("0"),
                        N,
                        Some("40002"),
                    ],
                    &[
                        Some("70002"),
                        Some("30001"),
                        Some("RI_ConstraintTrigger_c_1"),
                        Some("CREATE CONSTRAINT TRIGGER …"),
                        Some("O"),
                        Some("t"),
                        Some("0"),
                        N,
                        Some("1644"),
                    ],
                    &[Some("70003"), Some("30006"), Some("meas_trg"), Some("CREATE TRIGGER meas_trg …"), Some("O"), Some("f"), Some("70004"), N, Some("40002")],
                ],
            ),
        )
        .answer(
            "bulkdb:pg:policies",
            t(
                &["oid", "rel", "name", "cmd", "permissive", "roles", "qual", "withcheck"],
                &[&[Some("76001"), Some("30002"), Some("own_rows"), Some("r"), Some("t"), Some(r#"["PUBLIC"]"#), Some("(id > 0)"), N]],
            ),
        )
        .answer("bulkdb:pg:unsupported", t(&["kind", "name", "cls", "oid"], &[&[Some("operator"), Some("@@@"), Some("pg_operator"), Some("98001")]]))
}

fn target(version: &str) -> RecordingTarget {
    target_rel(version, &[&[Some("app"), Some("base"), Some("r"), Some("0"), Some("t"), Some("t"), Some("t"), Some("0")]])
}

fn target_rel(version: &str, rels: &[&[Option<&str>]]) -> RecordingTarget {
    let mut f = TargetFacts::new(CopyFamily::Postgres, "postgres", version, "app");
    f.endpoint = "new:5432".into();
    RecordingTarget::new(f)
        .answer(
            "bulkdb:pg:t_facts",
            t(
                &["encoding", "datcollate", "datctype", "locprovider", "lc_monetary", "superuser", "can_create"],
                &[&[Some("UTF8"), Some("en_US.UTF-8"), Some("en_US.UTF-8"), Some("c"), Some("en_US.UTF-8"), Some("f"), Some("t")]],
            ),
        )
        .answer("bulkdb:pg:t_relations", t(&["schema", "name", "kind", "est_rows", "can_insert", "can_truncate", "is_owner", "triggers"], rels))
        .answer("bulkdb:pg:t_schemas", t(&["name", "can_create", "can_use"], &[&[Some("app"), Some("t"), Some("t")]]))
        .answer("bulkdb:pg:t_collations", t(&["schema", "name"], &[&[Some("pg_catalog"), Some("C")], &[Some("pg_catalog"), Some("default")]]))
        .answer(
            "bulkdb:pg:t_extensions",
            t(
                &["name", "default_version", "installed_version", "trusted"],
                &[&[Some("citext"), Some("1.6"), N, Some("t")], &[Some("postgis"), Some("3.4.2"), N, Some("f")]],
            ),
        )
        .answer("SELECT EXISTS (SELECT 1 FROM ONLY \"app\".\"base\")", t(&["has_rows"], &[&[Some("f")]]))
}

fn req() -> CopyRequest {
    CopyRequest {
        source: CopyEndpoint { conn_id: "s".into(), database: "app".into(), schema: None, password: None },
        target: CopyEndpoint { conn_id: "t".into(), database: "app".into(), schema: None, password: None },
        selection: CopySelection::All,
        options: CopyOptions::default(),
        name: None,
    }
}

async fn plan(req: &CopyRequest, src: &CannedMeta, tgt: &RecordingTarget) -> PlannedCopy {
    let conn = Connection { driver: "postgres".into(), host: Some("old".into()), port: Some(5432), ..Default::default() };
    let tconn = Connection { driver: "postgres".into(), ..Default::default() };
    let server = ServerInfo { driver: "postgres".into(), version: "15.6".into(), ..Default::default() };
    let cancel = CancellationToken::new();
    build_plan(PlanInput {
        req,
        source: src,
        source_conn: &conn,
        source_server: &server,
        mysql_source: None,
        target: tgt,
        target_conn: &tconn,
        resume: None,
        cancel: &cancel,
    })
    .await
    .unwrap()
}

fn item<'a>(p: &'a PlannedCopy, id: &str) -> &'a PlanItem {
    p.plan.items.iter().find(|i| i.id == id).unwrap_or_else(|| panic!("no item {id}: {:?}", p.plan.items.iter().map(|i| &i.id).collect::<Vec<_>>()))
}

fn codes(i: &[CopyIssue]) -> Vec<&str> {
    i.iter().map(|x| x.code.as_str()).collect()
}

#[tokio::test]
async fn pg15_into_pg17_catalog_to_items() {
    let src = source("UTF8", "150006");
    let p = plan(&req(), &src, &target("17.2")).await;
    assert!(src.queries().iter().any(|q| q == "SET search_path = \"app\", pg_catalog"), "catalog read under the schema's search_path");

    let schema = item(&p, "schema:\"app\"");
    assert_eq!(schema.action, PlanAction::SkipExisting, "the schema exists");
    let citext = item(&p, "extension:\"citext\"");
    assert_eq!(citext.target_sql, vec!["CREATE EXTENSION IF NOT EXISTS \"citext\" WITH SCHEMA \"public\"".to_string()]);
    let postgis = item(&p, "extension:\"postgis\"");
    assert!(codes(&postgis.warnings).contains(&"PG_EXTENSION_PRIVILEGE"));
    assert!(!p.plan.items.iter().any(|i| i.id.contains("st_x")), "extension members are never copied");
    // Extension configuration table: rows matching extcondition, appended.
    let srs = item(&p, "table:\"app\".\"spatial_ref_sys\"");
    assert_eq!((srs.action, srs.phase), (PlanAction::Append, CopyPhase::Data));
    assert!(p.tables[&srs.id].source_sql.ends_with("FROM ONLY \"app\".\"spatial_ref_sys\" WHERE srid > 999000"));
    assert!(codes(&srs.warnings).contains(&"EXT_CONFIG_ROWS"));

    let mood = item(&p, "type:\"app\".\"mood\"");
    assert_eq!(mood.target_sql, vec!["SET search_path = \"app\", pg_catalog".to_string(), "CREATE TYPE \"app\".\"mood\" AS ENUM ('sad', 'ok')".to_string()]);
    let dom = item(&p, "domain:\"app\".\"posint\"");
    assert_eq!(dom.target_sql[1], "CREATE DOMAIN \"app\".\"posint\" AS integer CONSTRAINT \"posint_check\" CHECK (VALUE > 0)");
    let legacy = item(&p, "constraint:\"app\".\"posint\".\"posint_legacy\"");
    assert_eq!(legacy.phase, CopyPhase::PostData);
    assert_eq!(legacy.target_sql[1], "ALTER DOMAIN \"app\".\"posint\" ADD CONSTRAINT \"posint_legacy\" CHECK (VALUE < 1000) NOT VALID");

    // Identity comes with its column; the serial sequence is created before its table.
    assert!(!p.plan.items.iter().any(|i| i.id == "sequence:\"app\".\"orders_id_seq\""));
    let nseq = item(&p, "sequence:\"app\".\"orders_n_seq\"");
    assert_eq!(
        nseq.target_sql,
        vec!["CREATE SEQUENCE \"app\".\"orders_n_seq\" AS bigint START WITH 1 INCREMENT BY 1 MINVALUE 1 MAXVALUE 9223372036854775807 CACHE 1".to_string()]
    );
    let orders = item(&p, "table:\"app\".\"orders\"");
    assert!(nseq.order < orders.order && item(&p, "type:\"app\".\"mood\"").order < orders.order);
    let create = &orders.target_sql[1];
    assert!(create.starts_with("CREATE TABLE \"app\".\"orders\" (\n    \"id\" integer GENERATED BY DEFAULT AS IDENTITY (SEQUENCE NAME \"app\".\"orders_id_seq\" START WITH 1 INCREMENT BY 1 MINVALUE 1 MAXVALUE 2147483647 CACHE 1) NOT NULL,"), "{create}");
    assert!(create.contains("\"n\" bigint DEFAULT nextval('orders_n_seq'::regclass) NOT NULL"));
    assert!(create.contains("\"total\" numeric GENERATED ALWAYS AS (n * 2) STORED"));
    assert!(create.contains("\"note\" text COLLATE pg_catalog.\"C\""));
    assert!(create.contains("CONSTRAINT \"orders_n_check\" CHECK (n > 0)"));
    assert!(!create.contains("orders_m_check"), "NOT VALID checks go to post_data");
    assert!(orders.target_sql.contains(&"ALTER SEQUENCE \"app\".\"orders_n_seq\" OWNED BY \"app\".\"orders\".\"n\"".to_string()));
    assert!(orders.target_sql.contains(&"ALTER TABLE ONLY \"app\".\"orders\" ALTER COLUMN \"n\" SET STATISTICS 200".to_string()));
    assert!(orders.target_sql.contains(&"ALTER TABLE ONLY \"app\".\"orders\" ALTER COLUMN \"note\" SET STORAGE EXTERNAL".to_string()));
    assert!(orders.target_sql.contains(&"ALTER TABLE ONLY \"app\".\"orders\" ALTER COLUMN \"note\" SET COMPRESSION lz4".to_string()));
    let spec = &p.tables[&orders.id];
    assert!(spec.raw_csv);
    assert_eq!(spec.source_sql, "SELECT \"id\", \"n\", \"m\", \"note\" FROM ONLY \"app\".\"orders\"", "generated columns are left out");
    assert_eq!(
        orders.target_sql.last().unwrap(),
        "COPY \"app\".\"orders\" (\"id\", \"n\", \"m\", \"note\") FROM STDIN (FORMAT csv, HEADER false, DELIMITER ',', NULL '', QUOTE '\"', ESCAPE '\"')"
    );
    assert_eq!((orders.est_rows, orders.est_bytes), (Some(1000), Some(65536)));

    // post_data: keys, NOT VALID checks, FKs, triggers.
    let pk = item(&p, "constraint:\"app\".\"orders\".\"orders_pkey\"");
    assert_eq!((pk.phase, pk.target_sql[1].as_str()), (CopyPhase::PostData, "ALTER TABLE ONLY \"app\".\"orders\" ADD CONSTRAINT \"orders_pkey\" PRIMARY KEY (id)"));
    assert_eq!(
        item(&p, "constraint:\"app\".\"orders\".\"orders_m_check\"").target_sql[1],
        "ALTER TABLE ONLY \"app\".\"orders\" ADD CONSTRAINT \"orders_m_check\" CHECK (m IS NOT NULL) NOT VALID"
    );
    let fk = item(&p, "foreign_key:\"app\".\"orders\".\"orders_customer_fk\"");
    assert!(fk.target_sql[1].ends_with("FOREIGN KEY (n) REFERENCES customers(id) DEFERRABLE INITIALLY DEFERRED"));
    assert!(
        fk.depends_on.contains(&"table:\"app\".\"customers\"".to_string()) && fk.depends_on.contains(&"constraint:\"app\".\"customers\".\"customers_pkey\"".to_string())
    );
    let ix = item(&p, "index:\"app\".\"orders\".\"orders_m_idx\"");
    assert_eq!(
        ix.target_sql[1..],
        ["CREATE INDEX orders_m_idx ON orders USING btree (m)".to_string(), "ALTER TABLE \"app\".\"orders\" CLUSTER ON \"orders_m_idx\"".to_string()]
    );
    assert!(!p.plan.items.iter().any(|i| i.name == "orders_bad_idx" || i.name == "meas_2024_v_idx"));
    assert_eq!(item(&p, "index:\"app\".\"meas\".\"meas_idx\"").target_sql[1], "CREATE INDEX meas_idx ON meas USING btree (v)");
    let trg = item(&p, "trigger:\"app\".\"orders\".\"trg\"");
    assert_eq!(
        trg.target_sql[1..],
        [
            "CREATE TRIGGER trg BEFORE INSERT ON orders FOR EACH ROW EXECUTE FUNCTION trg_fn()".to_string(),
            "ALTER TABLE \"app\".\"orders\" DISABLE TRIGGER \"trg\"".to_string()
        ]
    );
    assert!(trg.depends_on.contains(&"function:\"app\".\"trg_fn\"()".to_string()));
    assert!(trg.order > pk.order && trg.order > fk.order, "triggers last");
    assert!(!p.plan.items.iter().any(|i| i.name.starts_with("RI_ConstraintTrigger") || i.name == "meas_trg"));

    // Partitions and inheritance.
    let meas = item(&p, "table:\"app\".\"meas\"");
    assert!(meas.target_sql[1].ends_with(") PARTITION BY RANGE (logdate)"));
    assert_eq!(p.tables[&meas.id].source_sql, "SELECT \"logdate\", \"v\" FROM \"app\".\"meas\"", "a partitioned root reads its partitions");
    let part = item(&p, "partition:\"app\".\"meas_2024\"");
    assert_eq!(part.target_sql[1], "CREATE TABLE \"app\".\"meas_2024\" PARTITION OF \"app\".\"meas\" FOR VALUES FROM ('2024-01-01') TO ('2025-01-01')");
    assert!(!part.has_data && part.depends_on.contains(&meas.id));
    let child = item(&p, "table:\"app\".\"child\"");
    assert_eq!(child.target_sql[1], "CREATE TABLE \"app\".\"child\" (\n    \"extra\" text\n) INHERITS (\"app\".\"base\")");
    assert_eq!(p.tables[&child.id].source_sql, "SELECT \"id\", \"extra\" FROM ONLY \"app\".\"child\"");
    assert_eq!(item(&p, "table:\"app\".\"base\"").action, PlanAction::SkipExisting);

    // Views, matviews, functions.
    let v = item(&p, "view:\"app\".\"v_orders\"");
    assert_eq!(v.target_sql[1], "CREATE VIEW \"app\".\"v_orders\" WITH (security_invoker=true) AS\nSELECT id\n   FROM orders");
    assert!(v.order > orders.order);
    let mv = item(&p, "materialized_view:\"app\".\"mv\"");
    assert!(mv.target_sql[1].ends_with("WITH NO DATA") && mv.order > v.order);
    assert_eq!(item(&p, "refresh:\"app\".\"mv\"").target_sql, vec!["REFRESH MATERIALIZED VIEW \"app\".\"mv\"".to_string()]);
    let f = item(&p, "function:\"app\".\"f\"(o orders)");
    assert!(f.target_sql[2].starts_with("CREATE FUNCTION \"app\".\"f\"(o orders)"), "{}", f.target_sql[2]);
    assert!(f.order > orders.order, "a function taking a table's rowtype comes after that table");
    assert!(codes(&f.warnings).contains(&"PG17_SEARCH_PATH"));
    assert!(!codes(&item(&p, "function:\"app\".\"trg_fn\"()").warnings).contains(&"PG17_SEARCH_PATH"));
    assert_eq!(item(&p, "function:\"app\".\"agg_sum\"(integer)").action, PlanAction::Unsupported);
    assert!(codes(&item(&p, "function:\"app\".\"sec_f\"()").warnings).contains(&"PG_SECDEF_OWNER"));

    // Finalize: setval templates, comments, analyze.
    let sv = item(&p, "sequence_value:\"app\".\"orders_id_seq\"");
    assert_eq!(sv.target_sql, vec!["SELECT pg_catalog.setval('\"app\".\"orders_id_seq\"', <source value>, <is_called>)".to_string()]);
    assert_eq!(sv.parent.as_deref(), Some("table:\"app\".\"orders\""));
    let down = item(&p, "sequence_value:\"app\".\"down_seq\"");
    assert!(down.target_sql[0].contains("<source value>"));
    let cm = item(&p, "comment:table:\"app\".\"orders\"");
    assert!(cm.target_sql.contains(&"COMMENT ON TABLE \"app\".\"orders\" IS 'the orders'".to_string()));
    assert!(cm.target_sql.contains(&"COMMENT ON COLUMN \"app\".\"orders\".\"note\" IS '메모'".to_string()));
    assert_eq!(item(&p, "comment:schema:\"app\"").action, PlanAction::SkipExisting);
    assert_eq!(
        item(&p, "comment:foreign_key:\"app\".\"orders\".\"orders_customer_fk\"").target_sql[1],
        "COMMENT ON CONSTRAINT \"orders_customer_fk\" ON \"app\".\"orders\" IS 'to the customer'"
    );
    assert_eq!(item(&p, "analyze:\"app\".\"orders\"").target_sql, vec!["ANALYZE \"app\".\"orders\"".to_string()]);

    // Source and target checks.
    let rls = p.plan.blockers.iter().find(|b| b.code == "SRC_RLS").expect("SRC_RLS");
    assert_eq!(rls.item_id.as_deref(), Some("table:\"app\".\"customers\""));
    let w = codes(&p.plan.warnings);
    for c in ["PG_DEFAULT_COLLATION_DIFFERS", "PG_DB_ROLE_SETTINGS", "UNSUPPORTED_OBJECT", "PG_TABLESPACE_IGNORED", "INVALID_INDEX_SKIPPED"] {
        assert!(w.contains(&c), "{c} missing in {w:?}");
    }
    let unsupported = p.plan.warnings.iter().find(|x| x.code == "UNSUPPORTED_OBJECT").unwrap();
    assert!(unsupported.detail.as_deref().unwrap().contains("operator @@@") && unsupported.detail.as_deref().unwrap().contains("cast text AS mood"));
    assert_eq!(p.plan.effective.load_method, LoadMethod::PgCopyCsv);
    assert!(p.plan.effective.consistent_snapshot);
    assert!(p.plan.session_sql.target.contains(&"SET check_function_bodies = off".to_string()));
    assert!(p.plan.session_sql.source.contains(&"SET row_security = off".to_string()));
    assert_eq!(p.plan.source.charset.as_deref(), Some("UTF8"));
    assert_eq!(p.plan.target.endpoint, "new:5432");
}

#[tokio::test]
async fn schema_rename_rewrites_function_headers_and_targets() {
    let src = source("UTF8", "150006");
    let mut r = req();
    r.source.schema = Some("app".into());
    r.target.schema = Some("app2".into());
    r.target.database = "other".into();
    let p = plan(&r, &src, &target("16.2")).await;
    let orders = item(&p, "table:\"app\".\"orders\"");
    assert_eq!(orders.target_schema.as_deref(), Some("app2"));
    assert_eq!(orders.target_sql[0], "SET search_path = \"app2\", pg_catalog");
    assert!(orders.target_sql[1].starts_with("CREATE TABLE \"app2\".\"orders\""));
    assert_eq!(p.tables[&orders.id].target, TargetTable { schema: Some("app2".into()), name: "orders".into() });
    let f = item(&p, "function:\"app\".\"f\"(o orders)");
    assert!(f.target_sql[2].starts_with("CREATE FUNCTION \"app2\".\"f\"(o orders)"));
    assert!(codes(&f.warnings).contains(&"RENAMED_REFS"));
    assert!(!codes(&f.warnings).contains(&"PG17_SEARCH_PATH"), "16 has no restricted search_path");
    assert_eq!(item(&p, "schema:\"app\"").target_sql, vec!["CREATE SCHEMA IF NOT EXISTS \"app2\"".to_string()]);
    // A target schema without a source schema is invalid.
    let mut r = req();
    r.target.schema = Some("x".into());
    let conn = Connection { driver: "postgres".into(), ..Default::default() };
    let server = ServerInfo { driver: "postgres".into(), version: "15.6".into(), ..Default::default() };
    let cancel = CancellationToken::new();
    let tgt = target("16.2");
    let res = build_plan(PlanInput {
        req: &r,
        source: &src,
        source_conn: &conn,
        source_server: &server,
        mysql_source: None,
        target: &tgt,
        target_conn: &conn,
        resume: None,
        cancel: &cancel,
    })
    .await;
    assert!(matches!(res, Err(CoreError::Invalid(_))));
}

#[tokio::test]
async fn sql_ascii_versions_and_selection() {
    let p = plan(&req(), &source("SQL_ASCII", "150006"), &target("16.2")).await;
    assert!(codes(&p.plan.blockers).contains(&"PG_SQL_ASCII_SOURCE"));
    let mut r = req();
    r.options.pg.source_encoding = Some(PgSourceEncoding::EucKr);
    let p = plan(&r, &source("SQL_ASCII", "150006"), &target("16.2")).await;
    assert!(!codes(&p.plan.blockers).contains(&"PG_SQL_ASCII_SOURCE"));
    assert_eq!(p.tables["table:\"app\".\"orders\""].transcode, Some(PgSourceEncoding::EucKr));
    assert!(p.plan.session_sql.source.contains(&"SET client_encoding = 'SQL_ASCII'".to_string()));
    let p = plan(&req(), &source("UTF8", "90624"), &target("16.2")).await;
    assert!(codes(&p.plan.blockers).contains(&"SOURCE_VERSION_UNSUPPORTED"));
    let p = plan(&req(), &source("UTF8", "170002"), &target("16.2")).await;
    assert!(p.plan.warnings.iter().any(|w| w.code == "DOWNGRADE" && w.severity == Severity::Danger));
    // Selection: only orders. Its type, sequences and functions used by it come along; the view
    // on it stays, the matview using an unselected function's view too; customers' FK excluded.
    let mut r = req();
    r.selection = CopySelection::Tables { tables: vec![crate::types::TableRef::new(Some("app"), "orders")] };
    let p = plan(&r, &source("UTF8", "150006"), &target("16.2")).await;
    assert!(p.plan.items.iter().any(|i| i.id == "type:\"app\".\"mood\""));
    assert!(p.plan.items.iter().any(|i| i.id == "sequence:\"app\".\"orders_n_seq\""));
    assert!(!p.plan.items.iter().any(|i| i.id == "domain:\"app\".\"posint\""), "unneeded support objects are left out");
    assert!(!p.plan.items.iter().any(|i| i.id == "table:\"app\".\"customers\""));
    let fk = item(&p, "foreign_key:\"app\".\"orders\".\"orders_customer_fk\"");
    assert_eq!(fk.action, PlanAction::Excluded);
}

#[tokio::test]
async fn existing_sequences_use_the_floor_rule() {
    let src = source("UTF8", "150006");
    let tgt = target_rel("16.2", &[&[Some("app"), Some("orders"), Some("r"), Some("10"), Some("t"), Some("f"), Some("f"), Some("1")]])
        .answer("pg_get_serial_sequence('\"app\".\"orders\"', 'n')", t(&["seq"], &[&[Some("app.orders_n_seq2")]]))
        .answer("SELECT EXISTS (SELECT 1 FROM ONLY \"app\".\"orders\")", t(&["has_rows"], &[&[Some("t")]]));
    let mut r = req();
    r.options.existing = ExistingTablePolicy::Append;
    let p = plan(&r, &src, &tgt).await;
    let orders = item(&p, "table:\"app\".\"orders\"");
    assert_eq!(orders.action, PlanAction::Append);
    assert!(codes(&orders.warnings).contains(&"TARGET_TRIGGERS"));
    let sv = item(&p, "sequence_value:\"app\".\"orders_n_seq\"");
    assert_eq!(sv.target_sql, vec!["SELECT pg_catalog.setval('app.orders_n_seq2', GREATEST(<source value>, <target value>), true)".to_string()]);
    // Truncate needs TRUNCATE and drop_recreate ownership.
    r.options.existing = ExistingTablePolicy::Truncate;
    let p = plan(&r, &src, &tgt).await;
    assert!(p.plan.blockers.iter().any(|b| b.code == "TGT_PRIVILEGE" && b.item_id.as_deref() == Some("table:\"app\".\"orders\"")));
    assert_eq!(item(&p, "table:\"app\".\"orders\"").target_sql[0], "TRUNCATE ONLY \"app\".\"orders\"");
}

/// Every statement of real PostgreSQL plans (pg15 into 16 and 17, all four policies) passes the
/// executor's allow-lists in the context the executor checks it (5.2): the per-item `SET
/// search_path`, `SET check_function_bodies = off` before function bodies, drops with RESTRICT,
/// truncates, setval in both forms, comments, owners, grants, refresh, analyze.
#[tokio::test]
async fn pg_plans_pass_the_executor_allow_lists() {
    let src = source("UTF8", "150006");
    let mut seen: Vec<String> = Vec::new();
    for version in ["16.4", "17.2"] {
        for policy in [ExistingTablePolicy::Skip, ExistingTablePolicy::Truncate, ExistingTablePolicy::DropRecreate, ExistingTablePolicy::Append] {
            let mut r = req();
            r.options.existing = policy;
            r.options.objects.owners = true;
            r.options.objects.grants = true;
            let p = plan(&r, &src, &target(version)).await;
            assert_eq!(crate::copy::exec::refused_statements(&p), Vec::<String>::new(), "{version} {policy:?}");
            seen.extend(crate::copy::exec::executed_statements(&p).into_iter().map(|(s, _)| s));
        }
    }
    for want in ["SET search_path = \"app\", pg_catalog", "SET check_function_bodies = off", "CREATE TABLE", "SELECT pg_catalog.setval("] {
        assert!(seen.iter().any(|s| s.starts_with(want)), "{want} never planned: {seen:#?}");
    }
    assert!(seen.iter().filter(|s| s.starts_with("DROP ")).all(|s| s.trim_end().ends_with("RESTRICT")), "every PG drop is RESTRICT");
}

/// 3.5 gaps: text partition keys and columns on the database default collation get that collation by
/// name; inheritance children and partitions keep their own default / NOT NULL on inherited
/// columns (`ALTER TABLE ONLY … SET DEFAULT | DROP DEFAULT | SET NOT NULL` after the CREATE);
/// type ACLs become `GRANT USAGE ON TYPE`.
#[tokio::test]
async fn partition_key_collation_child_overrides_and_type_grants() {
    let facts_cols = ["version_num", "encoding", "usr", "superuser", "bypassrls", "datcollate", "datctype", "locprovider", "lc_monetary", "has_lo"];
    let rel_cols = [
        "oid",
        "name",
        "kind",
        "persistence",
        "is_partition",
        "bound",
        "partkey",
        "part_colls",
        "reloptions",
        "rls",
        "force_rls",
        "replident",
        "est_rows",
        "bytes",
        "owner",
        "is_owner",
        "comment",
        "has_tablespace",
        "viewdef",
        "parents",
        "ext_member",
        "typed",
        "rowtype",
        "grants",
    ];
    let rel = |oid: &'static str,
               name: &'static str,
               kind: &'static str,
               part: bool,
               bound: Option<&'static str>,
               partkey: Option<&'static str>,
               colls: Option<&'static str>,
               parents: Option<&'static str>| {
        vec![
            Some(oid),
            Some(name),
            Some(kind),
            Some("p"),
            Some(if part { "t" } else { "f" }),
            bound,
            partkey,
            colls,
            N,
            Some("f"),
            Some("f"),
            Some("d"),
            Some("10"),
            Some("8192"),
            Some("owner"),
            Some("t"),
            N,
            Some("f"),
            N,
            parents,
            Some("f"),
            Some("f"),
            Some("1"),
            N,
        ]
    };
    let col_cols = [
        "rel",
        "num",
        "name",
        "type",
        "notnull",
        "dflt",
        "identity",
        "generated",
        "local",
        "collation",
        "coll_schema",
        "coll_name",
        "stattarget",
        "storage",
        "typstorage",
        "compression",
        "comment",
        "typname",
        "dflt_coll",
    ];
    let col = |rel: &'static str, num: &'static str, name: &'static str, ty: &'static str, notnull: bool, dflt: Option<&'static str>, local: bool| {
        vec![
            Some(rel),
            Some(num),
            Some(name),
            Some(ty),
            Some(if notnull { "t" } else { "f" }),
            dflt,
            Some(""),
            Some(""),
            Some(if local { "t" } else { "f" }),
            N,
            N,
            N,
            Some("-1"),
            Some("x"),
            Some("x"),
            N,
            N,
            Some(if ty == "text" { "text" } else { "int4" }),
            Some(if ty == "text" { "t" } else { "f" }),
        ]
    };
    let rels = [
        rel("40001", "events", "p", false, N, Some("RANGE (region)"), Some(r#"["default"]"#), N),
        rel("40002", "events_kr", "r", true, Some("FOR VALUES FROM ('a') TO ('m')"), N, N, Some(r#"[["app","events"]]"#)),
        rel("40003", "base", "r", false, N, N, N, N),
        rel("40004", "child", "r", false, N, N, N, Some(r#"[["app","base"]]"#)),
    ];
    let cols = [
        col("40001", "1", "region", "text", false, N, true),
        col("40002", "1", "region", "text", true, Some("'kr'::text"), false),
        col("40003", "1", "id", "integer", false, Some("0"), true),
        col("40003", "2", "note", "text", false, Some("'n'::text"), true),
        col("40004", "1", "id", "integer", true, Some("1"), false),
        col("40004", "2", "note", "text", false, N, false),
        col("40004", "3", "extra", "text", false, N, true),
    ];
    let rel_refs: Vec<&[Option<&str>]> = rels.iter().map(|r| r.as_slice()).collect();
    let col_refs: Vec<&[Option<&str>]> = cols.iter().map(|r| r.as_slice()).collect();
    let src = CannedMeta::new()
        .answer(
            "bulkdb:pg:facts",
            t(
                &facts_cols,
                &[&[
                    Some("150006"),
                    Some("UTF8"),
                    Some("copier"),
                    Some("t"),
                    Some("f"),
                    Some("ko_KR.UTF-8"),
                    Some("ko_KR.UTF-8"),
                    Some("c"),
                    Some("ko_KR.UTF-8"),
                    Some("f"),
                ]],
            ),
        )
        .answer("bulkdb:pg:schemas", t(&["oid", "name", "owner", "comment", "grants"], &[&[Some("2200"), Some("app"), Some("owner"), N, N]]))
        .answer(
            "bulkdb:pg:types",
            t(
                &["oid", "name", "type", "labels", "base", "notnull", "dflt", "collation", "attrs", "range", "owner", "comment", "grants"],
                &[&[Some("50001"), Some("mood"), Some("e"), Some(r#"["sad","ok"]"#), N, Some("f"), N, N, N, N, Some("owner"), N, Some(r#"[["reader","USAGE"]]"#)]],
            ),
        )
        .answer("bulkdb:pg:relations", t(&rel_cols, &rel_refs))
        .answer("bulkdb:pg:columns", t(&col_cols, &col_refs));
    let mut f = TargetFacts::new(CopyFamily::Postgres, "postgres", "17.2", "app");
    f.endpoint = "new:5432".into();
    let tgt = RecordingTarget::new(f)
        .answer(
            "bulkdb:pg:t_facts",
            t(
                &["encoding", "datcollate", "datctype", "locprovider", "lc_monetary", "superuser", "can_create"],
                &[&[Some("UTF8"), Some("C"), Some("C"), Some("c"), Some("C"), Some("t"), Some("t")]],
            ),
        )
        .answer("bulkdb:pg:t_schemas", t(&["name", "can_create", "can_use"], &[&[Some("app"), Some("t"), Some("t")]]))
        .answer("bulkdb:pg:t_collations", t(&["schema", "name"], &[&[Some("pg_catalog"), Some("C")], &[Some("pg_catalog"), Some("ko_KR.utf8")]]))
        .answer("bulkdb:pg:t_roles", t(&["name"], &[&[Some("owner")], &[Some("reader")]]));
    let mut r = req();
    r.options.objects.grants = true;
    let p = plan(&r, &src, &tgt).await;
    assert!(p.plan.blockers.is_empty(), "{:?}", p.plan.blockers);

    let events = item(&p, "table:\"app\".\"events\"");
    assert!(events.target_sql[1].ends_with(") PARTITION BY RANGE (region COLLATE pg_catalog.\"ko_KR.utf8\")"), "{}", events.target_sql[1]);
    assert!(codes(&p.plan.warnings).contains(&"PG_DEFAULT_COLLATION_DIFFERS"));
    // Columns on the default collation get it by name too; non-collatable ones do not.
    assert!(events.target_sql[1].contains("\"region\" text COLLATE pg_catalog.\"ko_KR.utf8\""), "{}", events.target_sql[1]);
    let base = item(&p, "table:\"app\".\"base\"");
    assert!(base.target_sql[1].contains("\"id\" integer DEFAULT 0") && !base.target_sql[1].contains("integer COLLATE"), "{}", base.target_sql[1]);
    assert!(base.target_sql[1].contains("\"note\" text COLLATE pg_catalog.\"ko_KR.utf8\""), "{}", base.target_sql[1]);

    let part = item(&p, "partition:\"app\".\"events_kr\"");
    assert!(part.target_sql.contains(&"ALTER TABLE ONLY \"app\".\"events_kr\" ALTER COLUMN \"region\" SET DEFAULT 'kr'::text".to_string()), "{:#?}", part.target_sql);
    assert!(part.target_sql.contains(&"ALTER TABLE ONLY \"app\".\"events_kr\" ALTER COLUMN \"region\" SET NOT NULL".to_string()), "{:#?}", part.target_sql);

    let child = item(&p, "table:\"app\".\"child\"");
    assert!(child.target_sql[1].contains("\"extra\" text") && child.target_sql[1].contains("INHERITS (\"app\".\"base\")"), "{}", child.target_sql[1]);
    assert!(child.target_sql.contains(&"ALTER TABLE ONLY \"app\".\"child\" ALTER COLUMN \"id\" SET DEFAULT 1".to_string()), "{:#?}", child.target_sql);
    assert!(child.target_sql.contains(&"ALTER TABLE ONLY \"app\".\"child\" ALTER COLUMN \"id\" SET NOT NULL".to_string()));
    assert!(child.target_sql.contains(&"ALTER TABLE ONLY \"app\".\"child\" ALTER COLUMN \"note\" DROP DEFAULT".to_string()), "{:#?}", child.target_sql);
    assert!(!child.target_sql.iter().any(|s| s.contains("\"extra\" SET") || s.contains("\"note\" SET NOT NULL")));

    let grant = p.plan.items.iter().find(|i| i.kind == CopyObjectKind::Grant && i.parent.as_deref() == Some("type:\"app\".\"mood\"")).expect("type grant item");
    assert_eq!(grant.target_sql, vec!["GRANT USAGE ON TYPE \"app\".\"mood\" TO \"reader\"".to_string()]);
    assert_eq!(crate::copy::exec::refused_statements(&p), Vec::<String>::new());
}

/// The canned source with some answers replaced; records the queries and the
/// `decode_sql_ascii` switch in order.
struct Patched {
    inner: CannedMeta,
    over: Vec<(String, MetaTable)>,
    log: std::sync::Mutex<Vec<String>>,
}

#[async_trait::async_trait]
impl SourceMeta for Patched {
    async fn query(&self, sql: &str) -> Result<MetaTable> {
        self.log.lock().unwrap().push(sql.to_string());
        match self.over.iter().find(|(k, _)| sql.contains(k.as_str())) {
            Some((_, t)) => Ok(t.clone()),
            None => self.inner.query(sql).await,
        }
    }

    fn decode_sql_ascii(&self, enc: Option<PgSourceEncoding>) {
        self.log.lock().unwrap().push(format!("decode_sql_ascii {enc:?}"));
    }
}

async fn plan_patched(req: &CopyRequest, src: &Patched, tgt: &RecordingTarget) -> PlannedCopy {
    let conn = Connection { driver: "postgres".into(), ..Default::default() };
    let server = ServerInfo { driver: "postgres".into(), version: "18.0".into(), ..Default::default() };
    let cancel = CancellationToken::new();
    build_plan(PlanInput {
        req,
        source: src,
        source_conn: &conn,
        source_server: &server,
        mysql_source: None,
        target: tgt,
        target_conn: &conn,
        resume: None,
        cancel: &cancel,
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn pg18_virtual_generated_columns() {
    // orders gets `total_v numeric GENERATED ALWAYS AS (n * 2) VIRTUAL` (attgenerated 'v').
    let inner = source("UTF8", "180000");
    let mut cols = inner.query("/* bulkdb:pg:columns */").await.unwrap();
    let total = cols.rows.iter().position(|r| r[2].as_deref() == Some("total")).expect("total column");
    let mut v = cols.rows[total].clone();
    v[1] = Some("9".into());
    v[2] = Some("total_v".into());
    v[7] = Some("v".into());
    cols.rows.push(v);
    let src = Patched { inner, over: vec![("bulkdb:pg:columns".into(), cols)], log: Default::default() };
    let p = plan_patched(&req(), &src, &target("18.0")).await;
    let orders = item(&p, "table:\"app\".\"orders\"");
    let create = &orders.target_sql[1];
    assert!(create.contains("\"total_v\" numeric GENERATED ALWAYS AS (n * 2) VIRTUAL"), "{create}");
    assert!(!create.contains("DEFAULT n * 2"), "{create}");
    assert!(create.contains("\"total\" numeric GENERATED ALWAYS AS (n * 2) STORED"));
    // Computed by the target: never read or loaded.
    assert_eq!(p.tables[&orders.id].source_sql, "SELECT \"id\", \"n\", \"m\", \"note\" FROM ONLY \"app\".\"orders\"");
    assert!(!codes(&orders.warnings).contains(&"DOWNGRADE"));
    // Below 18 the column becomes STORED, with a DOWNGRADE warning.
    let p = plan_patched(&req(), &src, &target("17.2")).await;
    let orders = item(&p, "table:\"app\".\"orders\"");
    assert!(orders.target_sql[1].contains("\"total_v\" numeric GENERATED ALWAYS AS (n * 2) STORED"), "{}", orders.target_sql[1]);
    let w = orders.warnings.iter().find(|w| w.code == "DOWNGRADE").expect("DOWNGRADE");
    assert!(w.message.contains("total_v"), "{}", w.message);
    assert_eq!(p.tables[&orders.id].source_sql, "SELECT \"id\", \"n\", \"m\", \"note\" FROM ONLY \"app\".\"orders\"");
}

#[tokio::test]
async fn sql_ascii_catalog_is_decoded_before_any_text_is_read() {
    // The version query reports SQL_ASCII: the reader switches before the facts (user and
    // collation names) and every catalog query.
    let version = t(&["version_num", "encoding"], &[&[Some("150006"), Some("SQL_ASCII")]]);
    let mut r = req();
    r.options.pg.source_encoding = Some(PgSourceEncoding::EucKr);
    let src = Patched { inner: source("SQL_ASCII", "150006"), over: vec![("bulkdb:pg:version".into(), version.clone())], log: Default::default() };
    let p = plan_patched(&r, &src, &target("16.2")).await;
    assert!(!codes(&p.plan.blockers).contains(&"PG_SQL_ASCII_SOURCE"));
    let log = src.log.lock().unwrap().clone();
    let at = |needle: &str| log.iter().position(|q| q.contains(needle)).unwrap_or_else(|| panic!("{needle} not in {log:#?}"));
    assert!(at("decode_sql_ascii Some(EucKr)") < at("bulkdb:pg:facts"), "{log:#?}");
    assert_eq!(log.iter().filter(|q| q.starts_with("decode_sql_ascii")).count(), 1);
    // Without a chosen encoding the plan is blocked; the catalog is still read without failing.
    let src = Patched { inner: source("SQL_ASCII", "150006"), over: vec![("bulkdb:pg:version".into(), version)], log: Default::default() };
    let p = plan_patched(&req(), &src, &target("16.2")).await;
    assert!(codes(&p.plan.blockers).contains(&"PG_SQL_ASCII_SOURCE"));
    assert!(src.log.lock().unwrap().iter().any(|q| q == "decode_sql_ascii None"));
    // A version answer without the encoding (older canned sources): switched right after the facts.
    let src = Patched { inner: source("SQL_ASCII", "150006"), over: vec![], log: Default::default() };
    plan_patched(&r, &src, &target("16.2")).await;
    let log = src.log.lock().unwrap().clone();
    let at = |needle: &str| log.iter().position(|q| q.contains(needle)).unwrap_or_else(|| panic!("{needle} not in {log:#?}"));
    assert!(at("bulkdb:pg:facts") < at("decode_sql_ascii") && at("decode_sql_ascii") < at("bulkdb:pg:schemas"), "{log:#?}");
    // UTF8 sources never switch.
    let src = Patched { inner: source("UTF8", "150006"), over: vec![], log: Default::default() };
    plan_patched(&req(), &src, &target("16.2")).await;
    assert!(!src.log.lock().unwrap().iter().any(|q| q.starts_with("decode_sql_ascii")));
}
