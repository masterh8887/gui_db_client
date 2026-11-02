//! MySQL planning over canned SHOW output: 4.0 EUC-KR into 8.0 / 5.6 / 8.4, the charset sampler,
//! version rules, FK rules, views and triggers on 5.x.

use std::sync::Arc;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use super::*;
use crate::copy::plan::{build_plan, PlanInput};
use crate::copy::schema::SourceMeta;
use crate::copy::testkit::{CannedMeta, RecordingTarget};
use crate::types::ServerInfo;

fn mt(cols: &[&str], rows: &[&[Option<&str>]]) -> MetaTable {
    MetaTable::from_rows(cols, rows)
}

fn status(rows: &[(&str, &str, &str)]) -> MetaTable {
    // (name, engine, rows); 4.0 calls the engine column `Type`.
    let cols = ["Name", "Type", "Rows", "Data_length", "Index_length", "Auto_increment", "Comment"];
    MetaTable {
        columns: cols.iter().map(|c| c.to_string()).collect(),
        rows: rows
            .iter()
            .map(|(n, e, r)| vec![Some(n.to_string()), Some(e.to_string()), Some(r.to_string()), Some("4096".into()), Some("1024".into()), None, Some(String::new())])
            .collect(),
    }
}

fn create(name: &str, ddl: &str) -> MetaTable {
    mt(&["Table", "Create Table"], &[&[Some(name), Some(ddl)]])
}

const T40: &str = "CREATE TABLE `t` (\n  `id` int(11) NOT NULL auto_increment,\n  `name` varchar(20) NOT NULL default '',\n  `kind` enum('가','나') NOT NULL default '가',\n  `created` timestamp(14) NOT NULL,\n  `d` date NOT NULL default '0000-00-00',\n  `price` decimal(8,2) NOT NULL default '0.00',\n  `data` blob,\n  PRIMARY KEY  (`id`),\n  KEY `idx_name` (`name`(10))\n) TYPE=MyISAM";
const PARENT40: &str = "CREATE TABLE `parent` (\n  `id` int(11) NOT NULL default '0',\n  PRIMARY KEY  (`id`)\n) TYPE=InnoDB";
const CHILD40: &str = "CREATE TABLE `child` (\n  `id` int(11) NOT NULL default '0',\n  `pid` int(11) default NULL,\n  PRIMARY KEY  (`id`),\n  KEY `pid` (`pid`),\n  CONSTRAINT `0_12` FOREIGN KEY (`pid`) REFERENCES `parent` (`id`)\n) TYPE=InnoDB";

fn source_40(charset: &str) -> CannedMeta {
    CannedMeta::new()
        .answer("SHOW VARIABLES LIKE 'character_set'", mt(&["Variable_name", "Value"], &[&[Some("character_set"), Some(charset)]]))
        .answer("SHOW VARIABLES LIKE 'timezone'", mt(&["Variable_name", "Value"], &[&[Some("timezone"), Some("KST")]]))
        .answer("SHOW TABLE STATUS FROM `shop`", status(&[("t", "MyISAM", "100"), ("parent", "InnoDB", "5"), ("child", "InnoDB", "7")]))
        .answer("SHOW CREATE TABLE `shop`.`t`", create("t", T40))
        .answer("SHOW CREATE TABLE `shop`.`parent`", create("parent", PARENT40))
        .answer("SHOW CREATE TABLE `shop`.`child`", create("child", CHILD40))
}

fn facts(version: &str, vars: &[(&str, &str)]) -> TargetFacts {
    let mut f = TargetFacts::new(CopyFamily::Mysql, if version.contains("MariaDB") { "mariadb" } else { "mysql" }, version, "shop2");
    f.vars = vars.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect();
    f
}

fn req() -> CopyRequest {
    CopyRequest {
        source: CopyEndpoint { conn_id: "s".into(), database: "shop".into(), schema: None, password: None },
        target: CopyEndpoint { conn_id: "t".into(), database: "shop2".into(), schema: None, password: None },
        selection: CopySelection::All,
        options: CopyOptions::default(),
        name: None,
    }
}

async fn plan(req: &CopyRequest, src: &dyn SourceMeta, tgt: &RecordingTarget, src_version: &str, encoding: Option<&str>) -> PlannedCopy {
    let mut sconn = Connection { driver: "mysql".into(), host: Some("old".into()), port: Some(3306), ..Default::default() };
    if let Some(e) = encoding {
        sconn.fields.insert("encoding".into(), e.into());
    }
    let tconn = Connection { driver: "mysql".into(), ..Default::default() };
    let server = ServerInfo { driver: "mysql".into(), version: src_version.into(), ..Default::default() };
    let cancel = CancellationToken::new();
    build_plan(PlanInput {
        req,
        source: src,
        source_conn: &sconn,
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
async fn mysql_4_0_euckr_into_8_0_with_a_missing_database() {
    let src = source_40("euc_kr");
    let tgt = RecordingTarget::new(facts("8.0.36", &[("local_infile", "OFF")]));
    let p = plan(&req(), &src, &tgt, "4.0.27-standard", None).await;
    assert!(p.plan.blockers.is_empty(), "{:?}", p.plan.blockers);
    // The contract SQL with the fake 4.0 server.
    let q = src.queries();
    for s in [
        "SHOW VARIABLES LIKE 'character_set'",
        "SHOW VARIABLES LIKE 'timezone'",
        "SHOW TABLE STATUS FROM `shop`",
        "SHOW CREATE TABLE `shop`.`t`",
        "SELECT `name` FROM `shop`.`t` WHERE `name` <> '' LIMIT 1000",
    ] {
        assert!(q.iter().any(|x| x == s), "{s} not in {q:?}");
    }
    assert!(!q.iter().any(|x| x.starts_with("SHOW FULL TABLES") || x.starts_with("SHOW TRIGGERS")), "no 5.0 statements on 4.0: {q:?}");

    let db = item(&p, "database:\"shop2\"");
    assert_eq!((db.action, db.phase, db.order), (PlanAction::Create, CopyPhase::PreData, 0));
    assert_eq!(db.target_sql, vec!["CREATE DATABASE `shop2` DEFAULT CHARACTER SET utf8mb4 COLLATE utf8mb4_0900_ai_ci".to_string()]);

    let t = item(&p, "table:\"t\"");
    assert_eq!((t.action, t.schema.as_deref(), t.target_schema.as_deref()), (PlanAction::Create, Some("shop"), Some("shop2")));
    assert!(t.target_sql[0].starts_with("CREATE TABLE `shop2`.`t` (\n  `id` int NOT NULL AUTO_INCREMENT,"), "{}", t.target_sql[0]);
    assert!(t.target_sql[0].contains("`created` timestamp NOT NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP"));
    assert!(t.target_sql[0].contains("`price` decimal(9,2) NOT NULL DEFAULT '0.00'"));
    assert!(t.target_sql[0].contains("`kind` enum('가','나') NOT NULL DEFAULT '가'"));
    assert!(t.target_sql[0].contains("ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"));
    assert_eq!(t.target_sql[1], "INSERT INTO `shop2`.`t` (`id`, `name`, `kind`, `created`, `d`, `price`, `data`) VALUES <rows>");
    assert_eq!(t.est_rows, Some(100));
    assert_eq!(t.est_bytes, Some(5120));
    let spec = &p.tables["table:\"t\""];
    assert!(spec.first_timestamp_auto);
    assert_eq!(spec.target, TargetTable { schema: Some("shop2".into()), name: "t".into() });
    assert!(spec.source_sql.contains("UNIX_TIMESTAMP(`created`) AS `created`"));

    let k = item(&p, "index:\"t\".\"idx_name\"");
    assert_eq!((k.action, k.phase), (PlanAction::Create, CopyPhase::PostData));
    assert_eq!(k.target_sql, vec!["ALTER TABLE `shop2`.`t` ADD KEY `idx_name` (`name`(10))".to_string()]);
    let pk = item(&p, "index:\"child\".\"pid\"");
    assert_eq!(pk.target_sql, vec!["ALTER TABLE `shop2`.`child` ADD KEY `pid` (`pid`)".to_string()]);
    let fk = item(&p, "foreign_key:\"child\".\"0_12\"");
    assert_eq!(fk.action, PlanAction::Create);
    assert_eq!(fk.target_sql, vec!["ALTER TABLE `shop2`.`child` ADD CONSTRAINT `0_12` FOREIGN KEY (`pid`) REFERENCES `shop2`.`parent` (`id`)".to_string()]);
    assert!(fk.depends_on.contains(&"table:\"parent\"".to_string()));

    let ai = item(&p, "auto_increment:\"t\"");
    assert_eq!(ai.target_sql, vec!["ALTER TABLE `shop2`.`t` AUTO_INCREMENT = <source value>".to_string()]);
    assert_eq!(item(&p, "analyze:\"t\"").target_sql, vec!["ANALYZE TABLE `shop2`.`t`".to_string()]);

    let e = &p.plan.effective;
    assert_eq!((e.charset.as_deref(), e.collation.as_deref(), e.engine.as_deref()), (Some("utf8mb4"), Some("utf8mb4_0900_ai_ci"), Some("InnoDB")));
    assert_eq!(e.zero_dates, Some(ZeroDatePolicy::Null));
    assert_eq!(e.time_zone.as_deref(), Some("+00:00"));
    assert_eq!(e.load_method, LoadMethod::MysqlInsert);
    assert_eq!(e.chunk_bytes, Some(64 << 20));
    assert!(!e.consistent_snapshot, "4.0 and MyISAM: no consistent snapshot");
    assert_eq!((e.source_sessions, e.target_sessions), (2, 3));
    assert_eq!(p.plan.source.charset.as_deref(), Some("euc_kr"));
    assert!(p.plan.source.notes.iter().any(|n| n.contains("KST")));
    assert!(p.plan.target.notes.iter().any(|n| n.contains("6.x and 7.x")));
    assert_eq!(p.plan.session_sql.target[0], "SET NAMES utf8mb4");
    assert!(p.plan.session_sql.target.iter().any(|s| s == "SET SESSION time_zone = '+00:00'"));
    assert!(p.plan.session_sql.source.is_empty());

    // LOAD DATA when the target allows LOCAL INFILE.
    let tgt = RecordingTarget::new(facts("8.0.36", &[("local_infile", "ON")]));
    let p = plan(&req(), &source_40("euc_kr"), &tgt, "4.0.27-standard", None).await;
    let t = item(&p, "table:\"t\"");
    assert!(t.target_sql[1].starts_with("LOAD DATA LOCAL INFILE '<stream>' INTO TABLE `shop2`.`t` CHARACTER SET utf8mb4"), "{}", t.target_sql[1]);
    assert!(t.target_sql[1].contains("SET `data` = UNHEX(@v6)"), "{}", t.target_sql[1]);
    assert_eq!(p.plan.effective.load_method, LoadMethod::MysqlLoadData);
}

#[tokio::test]
async fn existing_target_tables_and_keys_follow_the_policy() {
    let src = source_40("euc_kr");
    let tgt = RecordingTarget::new(facts("8.0.36", &[]))
        .answer("information_schema.SCHEMATA", mt(&["SCHEMA_NAME"], &[&[Some("shop2")]]))
        .answer("information_schema.TABLES", mt(&["TABLE_NAME", "TABLE_TYPE", "ENGINE", "TABLE_ROWS"], &[&[Some("t"), Some("BASE TABLE"), Some("InnoDB"), Some("0")]]))
        .answer(
            "information_schema.COLUMNS",
            mt(
                &["TABLE_NAME", "COLUMN_NAME", "COLUMN_TYPE", "IS_NULLABLE", "COLUMN_DEFAULT", "EXTRA"],
                &[
                    &[Some("t"), Some("id"), Some("int"), Some("NO"), None, Some("auto_increment")],
                    &[Some("t"), Some("name"), Some("varchar(20)"), Some("NO"), Some(""), Some("")],
                ],
            ),
        )
        .answer("SELECT 1 FROM `shop2`.`t` LIMIT 1", mt(&["1"], &[&[Some("1")]]));
    let p = plan(&req(), &src, &tgt, "4.0.27", None).await;
    assert!(!p.plan.items.iter().any(|i| i.kind == CopyObjectKind::Database), "the database exists");
    let t = item(&p, "table:\"t\"");
    assert_eq!((t.action, t.target_has_rows), (PlanAction::SkipExisting, Some(true)));
    assert!(t.warnings.iter().all(|w| w.code != "COLLATION_CHANGED"), "DDL notes of a skipped table are dropped");
    assert_eq!(item(&p, "index:\"t\".\"idx_name\"").action, PlanAction::SkipExisting);
    assert_eq!(item(&p, "auto_increment:\"t\"").action, PlanAction::SkipExisting);
    assert!(codes(&item(&p, "table:\"t\"").warnings).contains(&"SKIPPED_DIFFERS"), "{:?}", item(&p, "table:\"t\"").warnings);
    assert!(codes(&p.plan.warnings).contains(&"TABLES_SKIPPED_EXISTING"));

    let mut r = req();
    r.options.existing = ExistingTablePolicy::Truncate;
    let p = plan(&r, &src, &tgt, "4.0.27", None).await;
    let t = item(&p, "table:\"t\"");
    assert_eq!(t.action, PlanAction::Truncate);
    assert_eq!(t.target_sql[0], "TRUNCATE TABLE `shop2`.`t`");
    assert_eq!(item(&p, "auto_increment:\"t\"").action, PlanAction::Run);

    let mut r = req();
    r.options.existing = ExistingTablePolicy::DropRecreate;
    let p = plan(&r, &src, &tgt, "4.0.27", None).await;
    let t = item(&p, "table:\"t\"");
    assert_eq!(t.target_sql[0], "DROP TABLE `shop2`.`t`");
    assert!(t.target_sql[1].starts_with("CREATE TABLE `shop2`.`t`"));
}

#[tokio::test]
async fn versions_and_charsets() {
    let src = source_40("euc_kr");
    let p = plan(&req(), &src, &RecordingTarget::new(facts("4.1.22", &[])), "4.0.27", None).await;
    assert!(codes(&p.plan.blockers).contains(&"TARGET_VERSION_UNSUPPORTED"));
    let p = plan(&req(), &source_40("big5"), &RecordingTarget::new(facts("8.0.36", &[])), "4.0.27", None).await;
    assert!(codes(&p.plan.blockers).contains(&"CHARSET_UNSUPPORTED"));
    let p = plan(&req(), &source_40("big5"), &RecordingTarget::new(facts("8.0.36", &[])), "4.0.27", Some("euc-kr")).await;
    assert!(!codes(&p.plan.blockers).contains(&"CHARSET_UNSUPPORTED"), "an explicit encoding is accepted");
    let p = plan(&req(), &src, &RecordingTarget::new(facts("10.6.12-MariaDB", &[])), "4.0.27", None).await;
    assert!(codes(&p.plan.warnings).contains(&"MARIADB"));
    let p = plan(&req(), &src, &RecordingTarget::new(facts("5.7.44-ndb-7.6.29", &[])), "4.0.27", None).await;
    assert!(p.plan.target.notes.iter().any(|n| n.contains("NDB")));
    assert!(p.plan.items.iter().find(|i| i.id == "table:\"t\"").unwrap().target_sql[0].contains("COLLATE=utf8mb4_general_ci"));
    // A user charset must exist on the target.
    let mut r = req();
    r.options.mysql.charset = Some("koi9".into());
    let tgt = RecordingTarget::new(facts("8.0.36", &[])).answer("SHOW CHARACTER SET", mt(&["Charset"], &[&[Some("utf8mb4")], &[Some("utf8mb3")]]));
    assert!(codes(&plan(&r, &src, &tgt, "4.0.27", None).await.plan.blockers).contains(&"CHARSET_UNSUPPORTED"));
    // Downgrades.
    let src57 = CannedMeta::new().answer("SHOW TABLE STATUS", status(&[]));
    let p = plan(&req(), &src57, &RecordingTarget::new(facts("5.7.44", &[])), "8.0.36", None).await;
    let d = p.plan.warnings.iter().find(|w| w.code == "DOWNGRADE").unwrap();
    assert_eq!(d.severity, crate::types::Severity::Danger);
}

/// 3.3 `UTF8MB3_ONLY`: a plan-level warning when the whole copy writes utf8 / utf8mb3, an item
/// warning for a table whose override is utf8 / utf8mb3; BMP-only sources never get it.
#[tokio::test]
async fn utf8mb3_targets_warn_per_plan_and_per_table() {
    let ddl = |n: &str, cs: &str| {
        format!("CREATE TABLE `{n}` (\n  `id` int NOT NULL,\n  `s` varchar(20) DEFAULT NULL,\n  PRIMARY KEY (`id`)\n) ENGINE=InnoDB DEFAULT CHARSET={cs}")
    };
    let src = |cs: &'static str| {
        CannedMeta::new()
            .answer("SHOW TABLE STATUS", {
                let mut s = status(&[("a", "InnoDB", "1"), ("b", "InnoDB", "1")]);
                s.columns[1] = "Engine".into();
                s
            })
            .answer("SHOW CREATE TABLE `shop`.`a`", create("a", &ddl("a", cs)))
            .answer("SHOW CREATE TABLE `shop`.`b`", create("b", &ddl("b", cs)))
    };
    let tgt = || RecordingTarget::new(facts("8.0.36", &[])).answer("SHOW CHARACTER SET", mt(&["Charset"], &[&[Some("utf8mb4")], &[Some("utf8mb3")], &[Some("utf8")]]));
    // Whole copy in utf8mb3.
    let mut r = req();
    r.options.mysql.charset = Some("utf8mb3".into());
    let p = plan(&r, &src("utf8mb4"), &tgt(), "8.0.36", None).await;
    assert!(codes(&p.plan.warnings).contains(&"UTF8MB3_ONLY"), "{:?}", p.plan.warnings);
    // One table overridden to utf8: an item warning on that table only.
    let mut r = req();
    r.options.mysql.table_overrides = vec![MysqlTableOverride { table: "b".into(), charset: Some("utf8".into()), collation: None }];
    let p = plan(&r, &src("utf8mb4"), &tgt(), "8.0.36", None).await;
    assert!(!codes(&p.plan.warnings).contains(&"UTF8MB3_ONLY"));
    assert!(codes(&item(&p, "table:\"b\"").warnings).contains(&"UTF8MB3_ONLY"));
    assert!(!codes(&item(&p, "table:\"a\"").warnings).contains(&"UTF8MB3_ONLY"));
    // A source that only holds 3-byte UTF-8 (utf8 tables): nothing to replace, no warning.
    let p = plan(&r, &src("utf8"), &tgt(), "5.5.62", None).await;
    assert!(!codes(&item(&p, "table:\"b\"").warnings).contains(&"UTF8MB3_ONLY"));
}

#[tokio::test]
async fn names_fold_on_case_insensitive_targets() {
    let ddl = |n: &str| format!("CREATE TABLE `{n}` (\n  `id` int(11) NOT NULL default '0'\n) TYPE=MyISAM");
    let src = CannedMeta::new()
        .answer("SHOW TABLE STATUS", status(&[("Users", "MyISAM", "1"), ("users", "MyISAM", "1")]))
        .answer("SHOW CREATE TABLE `shop`.`Users`", create("Users", &ddl("Users")))
        .answer("SHOW CREATE TABLE `shop`.`users`", create("users", &ddl("users")));
    let p = plan(&req(), &src, &RecordingTarget::new(facts("8.0.36", &[("lower_case_table_names", "1")])), "4.0.27", None).await;
    assert!(codes(&p.plan.blockers).contains(&"NAME_COLLISION"), "{:?}", p.plan.blockers);
    let p = plan(&req(), &src, &RecordingTarget::new(facts("8.0.36", &[("lower_case_table_names", "0")])), "4.0.27", None).await;
    assert!(!codes(&p.plan.blockers).contains(&"NAME_COLLISION"));
    let mut r = req();
    r.options.name_case = NameCase::Lower;
    let p = plan(&r, &src, &RecordingTarget::new(facts("8.0.36", &[])), "4.0.27", None).await;
    assert!(codes(&p.plan.blockers).contains(&"NAME_COLLISION"));
}

#[tokio::test]
async fn foreign_key_rules() {
    let parent =
        "CREATE TABLE `p` (\n  `id` int(11) NOT NULL,\n  `code` int(11) NOT NULL,\n  PRIMARY KEY (`id`),\n  KEY `code` (`code`)\n) ENGINE=InnoDB DEFAULT CHARSET=utf8";
    let child = "CREATE TABLE `c` (\n  `id` int(11) NOT NULL,\n  `pid` int(11) DEFAULT NULL,\n  `pcode` int(11) DEFAULT NULL,\n  PRIMARY KEY (`id`),\n  KEY `pid` (`pid`),\n  KEY `pcode` (`pcode`),\n  CONSTRAINT `c_p` FOREIGN KEY (`pid`) REFERENCES `p` (`id`),\n  CONSTRAINT `c_code` FOREIGN KEY (`pcode`) REFERENCES `p` (`code`)\n) ENGINE=InnoDB DEFAULT CHARSET=utf8";
    let src = CannedMeta::new()
        .answer("SHOW TABLE STATUS", status(&[("p", "InnoDB", "1"), ("c", "InnoDB", "1")]))
        .answer("SHOW CREATE TABLE `shop`.`p`", create("p", parent))
        .answer("SHOW CREATE TABLE `shop`.`c`", create("c", child));
    let p = plan(&req(), &src, &RecordingTarget::new(facts("8.4.3", &[])), "5.7.44", None).await;
    assert_eq!(item(&p, "foreign_key:\"c\".\"c_p\"").action, PlanAction::Create);
    let bad = item(&p, "foreign_key:\"c\".\"c_code\"");
    assert_eq!(bad.action, PlanAction::Unsupported);
    assert!(codes(&bad.warnings).contains(&"FK_NON_STANDARD_KEY"));
    // Only the supported FK is in the combined ALTER.
    assert_eq!(
        item(&p, "foreign_key:\"c\".\"c_p\"").target_sql,
        vec!["ALTER TABLE `shop2`.`c` ADD CONSTRAINT `c_p` FOREIGN KEY (`pid`) REFERENCES `shop2`.`p` (`id`)".to_string()]
    );
    // The keys of a table share one ALTER: the first item holds it, the others depend on it.
    let (k1, k2) = (item(&p, "index:\"c\".\"pid\""), item(&p, "index:\"c\".\"pcode\""));
    let (holder, other) = if k1.target_sql.is_empty() { (k2, k1) } else { (k1, k2) };
    assert_eq!(holder.target_sql, vec!["ALTER TABLE `shop2`.`c` ADD KEY `pid` (`pid`), ADD KEY `pcode` (`pcode`)".to_string()]);
    assert!(other.target_sql.is_empty() && other.depends_on.contains(&holder.id));
    // 8.0 accepts the non-standard key.
    let p = plan(&req(), &src, &RecordingTarget::new(facts("8.0.36", &[])), "5.7.44", None).await;
    assert_eq!(item(&p, "foreign_key:\"c\".\"c_code\"").action, PlanAction::Create);
    // MyISAM children cannot hold foreign keys.
    let mut r = req();
    r.options.mysql.engine = MysqlEngineChoice::Myisam;
    let p = plan(&r, &src, &RecordingTarget::new(facts("8.0.36", &[])), "5.7.44", None).await;
    let f = item(&p, "foreign_key:\"c\".\"c_p\"");
    assert_eq!(f.action, PlanAction::Unsupported);
    assert!(codes(&f.warnings).contains(&"FK_ON_MYISAM"));
    // Before 5.6 keys and FKs of a table go into one ALTER.
    let p = plan(&req(), &src, &RecordingTarget::new(facts("5.5.62", &[])), "5.5.62", None).await;
    let all: Vec<&String> = p
        .plan
        .items
        .iter()
        .filter(|i| i.parent.as_deref() == Some("table:\"c\"") && matches!(i.kind, CopyObjectKind::Index | CopyObjectKind::ForeignKey))
        .flat_map(|i| i.target_sql.iter())
        .collect();
    assert_eq!(all.len(), 1, "{all:?}");
    assert!(all[0].contains("ADD KEY `pid`") && all[0].contains("ADD CONSTRAINT `c_p`"));
    assert!(codes(&p.plan.warnings).contains(&"FK_VALIDATE_REBUILD"));
    // Selecting only the child excludes FKs to the unselected parent.
    let mut r = req();
    r.selection = CopySelection::Tables { tables: vec![crate::types::TableRef::new(None, "c")] };
    let p = plan(&r, &src, &RecordingTarget::new(facts("8.0.36", &[])), "5.7.44", None).await;
    let f = item(&p, "foreign_key:\"c\".\"c_p\"");
    assert_eq!(f.action, PlanAction::Excluded);
    assert!(codes(&f.warnings).contains(&"DEPENDENCY_NOT_SELECTED"));
}

const TRIGGER_BODY: &str = "SET NEW.a = NEW.a + (SELECT COUNT(*) FROM shop.t)";

/// A 5.x source: table `t`, views `v` / `v2`, trigger `trg` (its `collation_connection`
/// given), a procedure and a function named `cleanup`, an event `nightly`.
fn source_5x(trigger_collation: &str, trigger_body: &str) -> CannedMeta {
    let original = format!("CREATE DEFINER=`root`@`localhost` TRIGGER `trg` BEFORE INSERT ON `t` FOR EACH ROW {trigger_body}");
    let t = "CREATE TABLE `t` (\n  `a` int(11) NOT NULL,\n  PRIMARY KEY (`a`)\n) ENGINE=InnoDB DEFAULT CHARSET=utf8";
    let view = "CREATE ALGORITHM=UNDEFINED DEFINER=`app`@`%` SQL SECURITY DEFINER VIEW `v` AS select `shop`.`t`.`a` AS `a` from `shop`.`t`";
    let view2 = "CREATE ALGORITHM=UNDEFINED DEFINER=`app`@`%` SQL SECURITY DEFINER VIEW `v2` AS select `shop`.`v`.`a` AS `a` from `shop`.`v`";
    CannedMeta::new()
        .answer("SHOW TABLE STATUS", {
            let mut s = status(&[("t", "InnoDB", "3")]);
            s.columns[1] = "Engine".into();
            s.rows.push(vec![Some("v".into()), None, None, None, None, None, Some("VIEW".into())]);
            s.rows.push(vec![Some("v2".into()), None, None, None, None, None, Some("VIEW".into())]);
            s
        })
        .answer("SHOW FULL TABLES", mt(&["Tables_in_shop", "Table_type"], &[&[Some("v2"), Some("VIEW")], &[Some("v"), Some("VIEW")]]))
        .answer("SHOW CREATE TABLE `shop`.`t`", create("t", t))
        .answer("SHOW CREATE VIEW `shop`.`v2`", mt(&["View", "Create View"], &[&[Some("v2"), Some(view2)]]))
        .answer("SHOW CREATE VIEW `shop`.`v`", mt(&["View", "Create View"], &[&[Some("v"), Some(view)]]))
        .answer(
            "SHOW TRIGGERS",
            mt(
                &["Trigger", "Event", "Table", "Statement", "Timing", "sql_mode", "Definer"],
                &[&[Some("trg"), Some("INSERT"), Some("t"), Some(trigger_body), Some("BEFORE"), Some("STRICT_TRANS_TABLES,NO_AUTO_CREATE_USER"), Some("root@localhost")]],
            ),
        )
        .answer(
            "SHOW CREATE TRIGGER",
            mt(
                &["Trigger", "sql_mode", "SQL Original Statement", "character_set_client", "collation_connection"],
                &[&[Some("trg"), Some("STRICT_TRANS_TABLES,NO_AUTO_CREATE_USER"), Some(original.as_str()), Some("utf8"), Some(trigger_collation)]],
            ),
        )
        .answer("information_schema.ROUTINES", mt(&["ROUTINE_TYPE", "ROUTINE_NAME"], &[&[Some("PROCEDURE"), Some("cleanup")], &[Some("FUNCTION"), Some("cleanup")]]))
        .answer("information_schema.EVENTS", mt(&["EVENT_NAME"], &[&[Some("nightly")]]))
}

#[tokio::test]
async fn views_triggers_and_routines_on_5_x() {
    let src = source_5x("utf8_general_ci", TRIGGER_BODY);
    let tgt = RecordingTarget::new(facts("8.0.36", &[("log_bin", "ON"), ("log_bin_trust_function_creators", "OFF")]));
    let p = plan(&req(), &src, &tgt, "5.5.62-log", None).await;
    let v = item(&p, "view:\"v\"");
    assert_eq!(v.target_sql, vec!["CREATE ALGORITHM=UNDEFINED SQL SECURITY DEFINER VIEW `shop2`.`v` AS select `shop2`.`t`.`a` AS `a` from `shop2`.`t`".to_string()]);
    assert!(codes(&v.warnings).contains(&"DEFINER_CHANGED"));
    let v2 = item(&p, "view:\"v2\"");
    assert!(v.order < v2.order && v2.depends_on.contains(&v.id), "views in dependency order");
    let trg = item(&p, "trigger:\"t\".\"trg\"");
    assert_eq!(trg.phase, CopyPhase::PostData);
    assert_eq!(trg.target_sql[0], "SET SESSION sql_mode = 'STRICT_TRANS_TABLES'");
    assert_eq!(trg.target_sql[1], "CREATE TRIGGER `shop2`.`trg` BEFORE INSERT ON `shop2`.`t` FOR EACH ROW SET NEW.a = NEW.a + (SELECT COUNT(*) FROM shop.t)");
    assert!(trg.target_sql[2].starts_with("SET SESSION sql_mode = '"));
    assert_eq!(trg.target_sql[3], "SET NAMES utf8mb4");
    // The body names the source database; no database of that name on the target: warning.
    let w = trg.warnings.iter().find(|w| w.code == "DB_QUALIFIED_REFS").unwrap();
    assert_eq!(w.severity, crate::types::Severity::Warning);
    // Routines and events: one unsupported item each, not a plan-level warning.
    assert!(!codes(&p.plan.warnings).contains(&"MYSQL_ROUTINES_NOT_COPIED"));
    let r = item(&p, "function:\"cleanup\"(procedure)");
    assert_eq!((r.kind, r.action, r.name.as_str()), (CopyObjectKind::Function, PlanAction::Unsupported, "cleanup"));
    assert!(r.target_sql.is_empty());
    let w = r.warnings.iter().find(|w| w.code == "MYSQL_ROUTINES_NOT_COPIED").unwrap();
    assert_eq!(w.severity, crate::types::Severity::Warning);
    assert!(w.message.contains("procedure cleanup"));
    // The function of the same name and the event (5.1.6+) are items of their own.
    for id in ["function:\"cleanup\"(function)", "function:\"nightly\"(event)"] {
        let i = item(&p, id);
        assert_eq!(i.action, PlanAction::Unsupported, "{id}");
        assert!(codes(&i.warnings).contains(&"MYSQL_ROUTINES_NOT_COPIED"), "{id}");
    }
    assert!(p.plan.totals.unsupported >= 3);
    // Before 5.1.6 there are no events; a `tables` selection lists no routines at all.
    let p50 = plan(&req(), &src, &RecordingTarget::new(facts("8.0.36", &[])), "5.0.96", None).await;
    assert!(p50.plan.items.iter().all(|i| !i.id.ends_with("(event)")));
    assert!(p50.plan.items.iter().any(|i| i.id == "function:\"cleanup\"(procedure)"));
    let mut sel = req();
    sel.selection = CopySelection::Tables { tables: vec![crate::types::TableRef::new(None, "t")] };
    let ps = plan(&sel, &src, &RecordingTarget::new(facts("8.0.36", &[])), "5.5.62", None).await;
    assert!(ps.plan.items.iter().all(|i| i.kind != CopyObjectKind::Function), "{:?}", ps.plan.items.iter().map(|i| &i.id).collect::<Vec<_>>());
    assert!(codes(&p.plan.warnings).contains(&"TGT_BINLOG_TRIGGERS"));
    // A database with the source's name exists on the target server: blocker.
    let tgt = RecordingTarget::new(facts("8.0.36", &[])).answer("information_schema.SCHEMATA", mt(&["SCHEMA_NAME"], &[&[Some("shop")]]));
    let p = plan(&req(), &src, &tgt, "5.5.62", None).await;
    assert!(p.plan.blockers.iter().any(|b| b.code == "DB_QUALIFIED_REFS" && b.item_id.as_deref() == Some("trigger:\"t\".\"trg\"")));
    // 5.0.15: the statement is built from SHOW TRIGGERS.
    let p = plan(&req(), &src, &RecordingTarget::new(facts("8.0.36", &[])), "5.0.15", None).await;
    let trg = item(&p, "trigger:\"t\".\"trg\"");
    assert!(
        trg.target_sql.iter().any(|s| s == "CREATE TRIGGER `shop2`.`trg` BEFORE INSERT ON `shop2`.`t` FOR EACH ROW SET NEW.a = NEW.a + (SELECT COUNT(*) FROM shop.t)"),
        "{:?}",
        trg.target_sql
    );
    // keep_definer keeps accounts that exist on the target.
    let mut r = req();
    r.options.mysql.keep_definer = true;
    let tgt = RecordingTarget::new(facts("8.0.36", &[])).answer("FROM mysql.user", mt(&["1"], &[&[Some("1")]]));
    let p = plan(&r, &src, &tgt, "5.5.62", None).await;
    assert!(item(&p, "view:\"v\"").target_sql[0].contains("DEFINER=`app`@`%`"));
    assert!(!codes(&item(&p, "view:\"v\"").warnings).contains(&"DEFINER_CHANGED"));
}

// ---------------------------------------------------------------------------------------------
// Charset sampler
// ---------------------------------------------------------------------------------------------

fn cp949(s: &str) -> Vec<u8> {
    encoding_rs::EUC_KR.encode(s).0.into_owned()
}

#[test]
fn sampler_classification() {
    let korean = vec![cp949("한국어 데이터"), cp949("홍길동"), b"plain".to_vec()];
    assert_eq!(classify(SampleKind::Latin1, &korean), Some(Finding::Latin1HoldsKorean));
    let western = vec!["café crème".as_bytes().iter().map(|&b| if b == 0xC3 { 0xE9 } else { b }).filter(|b| *b != 0xA9).collect::<Vec<u8>>(), vec![0xE9, 0x20, 0x61]];
    assert_eq!(classify(SampleKind::Latin1, &western), None);
    let utf = vec!["한국어".as_bytes().to_vec(), "홍길동".as_bytes().to_vec()];
    assert_eq!(classify(SampleKind::EucKr, &utf), Some(Finding::EucKrHoldsUtf8));
    assert_eq!(classify(SampleKind::EucKr, &korean), None);
    assert_eq!(classify(SampleKind::BinaryAttr(Charset::EucKr), &[vec![0x00, 0xFF, 0x10, 0x80]]), Some(Finding::BinaryUndecodable));
    assert_eq!(classify(SampleKind::BinaryAttr(Charset::EucKr), &[b"5f4dcc3b5aa765d61d8327deb882cf99".to_vec()]), None);
    assert_eq!(classify(SampleKind::Blob, &korean), Some(Finding::BlobHoldsText));
    assert_eq!(classify(SampleKind::Blob, &[vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00]]), None);
    // Which columns are sampled per decode.
    let col =
        |ty: &str, cs: Option<&str>, bin: bool| MyColumn { name: "c".into(), ty: ty.into(), charset: cs.map(str::to_string), binary_attr: bin, ..Default::default() };
    assert_eq!(sample_kind(Decode::Fixed(Charset::Latin1), true, &col("varchar", None, false), "latin1"), Some(SampleKind::Latin1));
    assert_eq!(sample_kind(Decode::Fixed(Charset::EucKr), true, &col("varchar", None, true), "euckr"), Some(SampleKind::BinaryAttr(Charset::EucKr)));
    assert_eq!(sample_kind(Decode::ServerUtf8, false, &col("varchar", Some("latin1"), false), "utf8"), Some(SampleKind::Latin1));
    assert_eq!(sample_kind(Decode::PassThrough(Charset::EucKr), false, &col("varchar", Some("latin1"), false), "latin1"), None);
    assert_eq!(sample_kind(Decode::ServerUtf8, false, &col("varchar", None, false), "utf8mb4"), None);
    assert_eq!(sample_kind(Decode::ServerUtf8, false, &col("blob", None, false), "utf8"), Some(SampleKind::Blob));
    assert_eq!(sample_kind(Decode::ServerUtf8, false, &col("int", None, false), "utf8"), None);
    assert_eq!(sample_sql(false, "shop", "t", "c"), "SELECT CAST(`c` AS BINARY) FROM `shop`.`t` WHERE `c` <> '' LIMIT 1000");
}

/// A canned source that answers the sampler with raw bytes.
struct RawSource {
    inner: CannedMeta,
    raw: Vec<(String, Vec<Vec<u8>>)>,
}

#[async_trait]
impl SourceMeta for RawSource {
    async fn query(&self, sql: &str) -> crate::error::Result<MetaTable> {
        self.inner.query(sql).await
    }
    async fn query_raw(&self, sql: &str) -> crate::error::Result<Vec<Option<Vec<u8>>>> {
        match self.raw.iter().find(|(k, _)| sql.contains(k.as_str())) {
            Some((_, v)) => Ok(v.iter().cloned().map(Some).collect()),
            None => Ok(vec![]),
        }
    }
}

#[tokio::test]
async fn latin1_server_with_korean_bytes_is_blocked_under_auto() {
    let src =
        RawSource { inner: source_40("latin1"), raw: vec![("SELECT `name` FROM `shop`.`t`".into(), vec![cp949("홍길동"), cp949("한국어"), b"abc".to_vec()])] };
    let tgt = RecordingTarget::new(facts("8.0.36", &[]));
    let p = plan(&req(), &src, &tgt, "4.0.27", None).await;
    let b = p.plan.blockers.iter().find(|b| b.code == "SRC_LATIN1_KOREAN").expect("blocker");
    assert_eq!(b.item_id.as_deref(), Some("table:\"t\""));
    assert!(!b.message.contains("홍길동"), "sampled values never leave the planner");
    // With the encoding set to latin1 explicitly it is a danger warning that needs an ack.
    let p = plan(&req(), &src, &tgt, "4.0.27", Some("latin1")).await;
    assert!(!codes(&p.plan.blockers).contains(&"SRC_LATIN1_KOREAN"));
    let w = item(&p, "table:\"t\"").warnings.iter().find(|w| w.code == "SRC_LATIN1_KOREAN").unwrap();
    assert_eq!(w.severity, crate::types::Severity::Danger);
    // euc-kr decodes it correctly: nothing to sample on latin1 4.0 columns read as CP949.
    let p = plan(&req(), &src, &tgt, "4.0.27", Some("euc-kr")).await;
    assert!(!p.plan.items.iter().flat_map(|i| i.warnings.iter()).any(|w| w.code == "SRC_LATIN1_KOREAN"));
    let _ = Arc::new(());
}

/// Every statement of real MySQL plans passes the executor's allow-lists in the context the
/// executor checks it (5.2): a 4.0 source into a missing database (`CREATE DATABASE`) and into
/// an existing one under every policy, InnoDB and MyISAM (`DISABLE KEYS` / `ENABLE KEYS`), and a
/// 5.x source with views and a trigger whose body is a compound statement, created under its
/// own `sql_mode` and `_bin` collation with the session restored afterwards (`SET NAMES`).
#[tokio::test]
async fn mysql_plans_pass_the_executor_allow_lists() {
    let existing = || {
        RecordingTarget::new(facts("8.0.36", &[]))
            .answer("information_schema.SCHEMATA", mt(&["SCHEMA_NAME"], &[&[Some("shop2")]]))
            .answer(
                "information_schema.TABLES",
                mt(
                    &["TABLE_NAME", "TABLE_TYPE", "ENGINE", "TABLE_ROWS"],
                    &[&[Some("t"), Some("BASE TABLE"), Some("InnoDB"), Some("0")], &[Some("v"), Some("VIEW"), None, None]],
                ),
            )
            .answer("information_schema.TRIGGERS", mt(&["TRIGGER_NAME", "EVENT_OBJECT_TABLE"], &[&[Some("trg"), Some("t")]]))
            .answer("SELECT 1 FROM `shop2`.`t` LIMIT 1", mt(&["1"], &[&[Some("1")]]))
    };
    let body = "BEGIN\n  IF NEW.a > 0 THEN SET NEW.a = NEW.a + 1; END IF;\n  SET NEW.a = NEW.a * 2;\nEND";
    let mut seen: Vec<String> = Vec::new();
    for policy in [ExistingTablePolicy::Skip, ExistingTablePolicy::Truncate, ExistingTablePolicy::DropRecreate, ExistingTablePolicy::Append] {
        for engine in [MysqlEngineChoice::Auto, MysqlEngineChoice::Myisam] {
            let mut r = req();
            r.options.existing = policy;
            r.options.mysql.engine = engine;
            for (src, version) in [(source_40("euc_kr"), "4.0.27"), (source_5x("utf8_bin", body), "5.5.62")] {
                for tgt in [RecordingTarget::new(facts("8.0.36", &[])), existing()] {
                    let p = plan(&r, &src, &tgt, version, None).await;
                    assert_eq!(crate::copy::exec::refused_statements(&p), Vec::<String>::new(), "{policy:?} {engine:?} {version}");
                    seen.extend(crate::copy::exec::executed_statements(&p).into_iter().map(|(s, _)| s));
                }
            }
        }
    }
    for want in [
        "CREATE DATABASE `shop2`",
        "ALTER TABLE `shop2`.`t` DISABLE KEYS",
        "ALTER TABLE `shop2`.`t` ENABLE KEYS",
        "TRUNCATE TABLE `shop2`.`t`",
        "DROP TABLE `shop2`.`t`",
        "SET SESSION sql_mode = 'STRICT_TRANS_TABLES'",
        "SET SESSION collation_connection = 'utf8mb4_bin'",
        "SET NAMES utf8mb4",
        "CREATE TRIGGER `shop2`.`trg` BEFORE INSERT ON `shop2`.`t` FOR EACH ROW BEGIN",
        "ALTER TABLE `shop2`.`t` AUTO_INCREMENT = 42",
    ] {
        assert!(seen.iter().any(|s| s.starts_with(want)), "{want} never planned: {seen:#?}");
    }
}

/// 3.7: MySQL `TRUNCATE` runs under `foreign_key_checks = 0`, so a target table referenced by a
/// table this run does not reload can be left with orphans. The planner warns (danger,
/// acknowledged). A referencing table that is itself reloaded is safe.
#[tokio::test]
async fn mysql_truncate_referenced_by_unreloaded_table_warns() {
    let pddl = "CREATE TABLE `p` (\n  `id` int NOT NULL,\n  PRIMARY KEY (`id`)\n) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4";
    let cddl = "CREATE TABLE `c` (\n  `id` int NOT NULL,\n  `pid` int DEFAULT NULL,\n  PRIMARY KEY (`id`),\n  KEY `pid` (`pid`),\n  CONSTRAINT `c_p_fk` FOREIGN KEY (`pid`) REFERENCES `p` (`id`)\n) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4";
    let source = |with_c: bool| {
        let rows: &[(&str, &str, &str)] = if with_c { &[("p", "InnoDB", "10"), ("c", "InnoDB", "5")] } else { &[("p", "InnoDB", "10")] };
        let mut s = CannedMeta::new().answer("SHOW TABLE STATUS", status(rows)).answer("SHOW CREATE TABLE `shop`.`p`", create("p", pddl));
        if with_c {
            s = s.answer("SHOW CREATE TABLE `shop`.`c`", create("c", cddl));
        }
        s
    };
    let tgt = || {
        RecordingTarget::new(facts("8.0.36", &[]))
            .answer("information_schema.SCHEMATA", mt(&["SCHEMA_NAME"], &[&[Some("shop2")]]))
            .answer(
                "information_schema.TABLES",
                mt(
                    &["TABLE_NAME", "TABLE_TYPE", "ENGINE", "TABLE_ROWS"],
                    &[&[Some("p"), Some("BASE TABLE"), Some("InnoDB"), Some("10")], &[Some("c"), Some("BASE TABLE"), Some("InnoDB"), Some("5")]],
                ),
            )
            .answer(
                "information_schema.COLUMNS",
                mt(
                    &["TABLE_NAME", "COLUMN_NAME", "COLUMN_TYPE", "IS_NULLABLE", "COLUMN_DEFAULT", "EXTRA"],
                    &[
                        &[Some("p"), Some("id"), Some("int"), Some("NO"), None, Some("")],
                        &[Some("c"), Some("id"), Some("int"), Some("NO"), None, Some("")],
                        &[Some("c"), Some("pid"), Some("int"), Some("YES"), None, Some("")],
                    ],
                ),
            )
            .answer(
                "information_schema.KEY_COLUMN_USAGE",
                mt(
                    &["TABLE_SCHEMA", "TABLE_NAME", "CONSTRAINT_NAME", "REFERENCED_TABLE_SCHEMA", "REFERENCED_TABLE_NAME"],
                    &[&[Some("shop2"), Some("c"), Some("c_p_fk"), Some("shop2"), Some("p")]],
                ),
            )
            .answer("SELECT 1 FROM `shop2`.`p` LIMIT 1", mt(&["1"], &[&[Some("1")]]))
            .answer("SELECT 1 FROM `shop2`.`c` LIMIT 1", mt(&["1"], &[&[Some("1")]]))
    };

    // `c`, the referencing table, is not in the source: it keeps its rows -> warn on `p`.
    let mut r = req();
    r.options.existing = ExistingTablePolicy::Truncate;
    let p = plan(&r, &source(false), &tgt(), "8.0.36", None).await;
    let pt = item(&p, "table:\"p\"");
    assert_eq!(pt.action, PlanAction::Truncate);
    let w = pt.warnings.iter().find(|w| w.code == "TRUNCATE_FK_REFERENCED").unwrap_or_else(|| panic!("no TRUNCATE_FK_REFERENCED: {:?}", pt.warnings));
    // Danger severity => `preflight::finalize` raises an acknowledgement the user must accept.
    assert_eq!(w.severity, crate::types::Severity::Danger);
    assert!(w.detail.as_deref().unwrap_or("").contains("c_p_fk"), "{:?}", w.detail);

    // `c` is in the source and truncated too: fully reloaded, so no orphan warning.
    let p = plan(&r, &source(true), &tgt(), "8.0.36", None).await;
    assert!(!codes(&item(&p, "table:\"p\"").warnings).contains(&"TRUNCATE_FK_REFERENCED"), "{:?}", item(&p, "table:\"p\"").warnings);
}

/// 3.3: user-supplied charset / collation names (global and per-table) are bare identifiers the
/// target knows, so a value with spaces or extra clauses can never reach the generated DDL.
#[tokio::test]
async fn charset_and_collation_names_are_validated() {
    let src = source_40("euc_kr");
    let tgt = || {
        RecordingTarget::new(facts("8.0.36", &[]))
            .answer("SHOW CHARACTER SET", mt(&["Charset"], &[&[Some("utf8mb4")], &[Some("euckr")], &[Some("latin1")]]))
            .answer("SHOW COLLATION", mt(&["Collation"], &[&[Some("utf8mb4_0900_ai_ci")], &[Some("utf8mb4_bin")], &[Some("euckr_korean_ci")]]))
    };

    // A charset name that is not a bare identifier is rejected before it can reach the DDL.
    let mut r = req();
    r.options.mysql.charset = Some("utf8mb4 COLLATE x".into());
    assert!(codes(&plan(&r, &src, &tgt(), "4.0.27", None).await.plan.blockers).contains(&"CHARSET_UNSUPPORTED"));

    // A collation unknown to the target, and a malformed collation name, both block.
    let mut r = req();
    r.options.mysql.collation = Some("utf8mb4_unknown_ci".into());
    assert!(codes(&plan(&r, &src, &tgt(), "4.0.27", None).await.plan.blockers).contains(&"COLLATION_UNSUPPORTED"));
    let mut r = req();
    r.options.mysql.collation = Some("bad; DROP".into());
    assert!(codes(&plan(&r, &src, &tgt(), "4.0.27", None).await.plan.blockers).contains(&"COLLATION_UNSUPPORTED"));

    // Per-table overrides are validated too: a bad charset blocks, a known collation passes.
    let mut r = req();
    r.options.mysql.table_overrides = vec![MysqlTableOverride { table: "t".into(), charset: Some("no good".into()), collation: Some("utf8mb4_bin".into()) }];
    let b = plan(&r, &src, &tgt(), "4.0.27", None).await.plan.blockers;
    assert!(codes(&b).contains(&"CHARSET_UNSUPPORTED"), "{b:?}");
    assert!(!codes(&b).contains(&"COLLATION_UNSUPPORTED"), "a known override collation passes: {b:?}");

    // Known charset and the `legacy` collation preset pass (legacy is resolved by BulkDB).
    let mut r = req();
    r.options.mysql.charset = Some("euckr".into());
    r.options.mysql.collation = Some("legacy".into());
    let b = plan(&r, &src, &tgt(), "4.0.27", None).await.plan.blockers;
    assert!(!codes(&b).iter().any(|c| *c == "CHARSET_UNSUPPORTED" || *c == "COLLATION_UNSUPPORTED"), "{b:?}");
}

/// 3.6: the drop_recreate view-dependant probe uses a literal `INSTR` search, not a
/// backslash-escaped `LIKE`, so it finds dependent views whatever the target's global sql_mode is
/// and matches a database name containing `_` literally.
#[tokio::test]
async fn view_dependant_probe_is_sql_mode_independent() {
    let src = source_40("euc_kr");
    let mut r = req();
    r.target.database = "shop_2".into(); // a database name with `_`
    r.options.existing = ExistingTablePolicy::DropRecreate;
    let tgt = RecordingTarget::new(facts("8.0.36", &[]))
        .answer("information_schema.SCHEMATA", mt(&["SCHEMA_NAME"], &[&[Some("shop_2")]]))
        .answer("information_schema.TABLES", mt(&["TABLE_NAME", "TABLE_TYPE", "ENGINE", "TABLE_ROWS"], &[&[Some("t"), Some("BASE TABLE"), Some("InnoDB"), Some("1")]]))
        .answer(
            "information_schema.COLUMNS",
            mt(
                &["TABLE_NAME", "COLUMN_NAME", "COLUMN_TYPE", "IS_NULLABLE", "COLUMN_DEFAULT", "EXTRA"],
                &[&[Some("t"), Some("id"), Some("int"), Some("NO"), None, Some("auto_increment")]],
            ),
        )
        .answer(
            "information_schema.VIEWS",
            mt(&["TABLE_SCHEMA", "TABLE_NAME", "VIEW_DEFINITION"], &[&[Some("shop_2"), Some("v"), Some("select `shop_2`.`t`.`id` from `shop_2`.`t`")]]),
        )
        .answer("SELECT 1 FROM `shop_2`.`t` LIMIT 1", mt(&["1"], &[&[Some("1")]]));
    let p = plan(&r, &src, &tgt, "4.0.27", None).await;
    // The dropped table's dependent view is found -> blocker.
    assert!(p.plan.blockers.iter().any(|b| b.code == "DROP_HAS_DEPENDANTS" && b.item_id.as_deref() == Some("table:\"t\"")), "{:?}", p.plan.blockers);
    // The probe is a literal INSTR search (no LIKE escape that NO_BACKSLASH_ESCAPES would break).
    let vq = tgt.queries().into_iter().map(|(_, s)| s).find(|s| s.contains("information_schema.VIEWS")).expect("VIEWS query");
    assert!(vq.contains("INSTR(VIEW_DEFINITION, '`shop_2`')"), "{vq}");
    assert!(!vq.contains("LIKE"), "{vq}");
}
