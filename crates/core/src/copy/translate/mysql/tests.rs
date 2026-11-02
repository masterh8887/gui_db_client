//! `SHOW CREATE TABLE` parser fixtures (4.0 - 8.0) and per-target-version translation rules.

use super::*;

const T40: &str = "CREATE TABLE `t` (
  `id` int(11) NOT NULL auto_increment,
  `name` varchar(20) NOT NULL default '',
  `kind` enum('가','나') NOT NULL default '가',
  `created` timestamp(14) NOT NULL,
  `stamp8` timestamp(8) NOT NULL,
  `stamp6` timestamp(6) NOT NULL,
  `stamp12` timestamp(12) NOT NULL,
  `d` date NOT NULL default '0000-00-00',
  `price` decimal(8,2) NOT NULL default '0.00',
  `ratio` float default NULL,
  `pw` varchar(20) binary NOT NULL default '',
  `body` text,
  `data` blob,
  `y` year(2) NOT NULL default '00',
  `note` varchar(10) default 'it''s \\\\ \\'ok\\'',
  PRIMARY KEY  (`id`),
  KEY `idx_name` (`name`(10))
) TYPE=MyISAM COMMENT='한국어 설명'";

const T80: &str = "CREATE TABLE `orders` (
  `id` bigint unsigned NOT NULL AUTO_INCREMENT,
  `code` varchar(32) COLLATE utf8mb4_bin NOT NULL,
  `amount` decimal(10,2) NOT NULL DEFAULT '0.00',
  `created_at` datetime(3) NOT NULL DEFAULT CURRENT_TIMESTAMP(3),
  `updated_at` timestamp NULL DEFAULT NULL ON UPDATE CURRENT_TIMESTAMP,
  `uuid` binary(16) NOT NULL DEFAULT (uuid_to_bin(uuid())),
  `meta` json DEFAULT NULL,
  `total` decimal(11,2) GENERATED ALWAYS AS ((`amount` * 1.1)) VIRTUAL,
  `secret` varchar(10) DEFAULT NULL /*!80023 INVISIBLE */,
  `flag` bit(1) NOT NULL DEFAULT b'0',
  PRIMARY KEY (`id`,`created_at`),
  UNIQUE KEY `uk_code` (`code`,`created_at`),
  KEY `idx_amount_desc` (`amount` DESC) /*!80000 INVISIBLE */,
  KEY `idx_func` ((lower(`code`))),
  CONSTRAINT `orders_chk_1` CHECK ((`amount` >= 0))
) ENGINE=InnoDB AUTO_INCREMENT=101 DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci STATS_PERSISTENT=0
/*!50100 PARTITION BY RANGE (year(`created_at`))
(PARTITION p2023 VALUES LESS THAN (2024) ENGINE = InnoDB,
 PARTITION pmax VALUES LESS THAN MAXVALUE ENGINE = InnoDB) */";

fn src(v: &str) -> MySourceCtx {
    let ver = MyVersion::parse(v);
    MySourceCtx { ver, db: "shop".into(), text_protocol: !ver.at(4, 1, 0), server_charset: Some(if ver.at(4, 1, 0) { "latin1".into() } else { "euc_kr".into() }) }
}

fn tgt(v: &str, vars: &[(&str, &str)]) -> MyTargetCtx {
    let ver = MyVersion::parse(v);
    let vars: BTreeMap<String, String> = vars.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect();
    let utf8mb4 = ver.at(5, 5, 3);
    let mode = vars.get("sql_mode").cloned().unwrap_or_else(|| default_global_sql_mode(&ver).to_string());
    MyTargetCtx {
        ver,
        vars,
        db: "shop2".into(),
        charset: if utf8mb4 { "utf8mb4".into() } else { "utf8".into() },
        user_collation: None,
        engine: MysqlEngineChoice::Auto,
        zero: resolve_zero_dates(ZeroDatePolicy::Auto, &mode),
        indexes_after_data: true,
        fix_row_size: true,
    }
}

fn tr(ddl: &str, s: &MySourceCtx, t: &MyTargetCtx) -> MyTableOut {
    let table = parse_create_table(ddl);
    translate_table(&table, &table.name, s, t, &MyTableOverrides::default(), &BTreeMap::new())
}

fn codes(i: &[CopyIssue]) -> Vec<&str> {
    i.iter().map(|x| x.code.as_str()).collect()
}

fn line<'a>(sql: &'a str, col: &str) -> &'a str {
    sql.lines().find(|l| l.trim_start().starts_with(&format!("`{col}`"))).unwrap_or_else(|| panic!("no column {col} in\n{sql}")).trim().trim_end_matches(',')
}

#[test]
fn parses_the_4_0_euckr_table() {
    let t = parse_create_table(T40);
    assert!(t.unparsed.is_empty(), "{:?}", t.unparsed);
    assert_eq!(t.name, "t");
    assert_eq!(t.engine(), Some("MyISAM"));
    assert_eq!(t.option("COMMENT"), Some("한국어 설명"));
    let kind = t.column("kind").unwrap();
    assert_eq!(kind.values, vec!["가".to_string(), "나".to_string()]);
    assert_eq!(kind.default, Some(MyDefault::Literal("가".into())));
    let created = t.column("created").unwrap();
    assert_eq!((created.ty.as_str(), created.args.as_slice()), ("timestamp", &["14".to_string()][..]));
    assert!(t.column("pw").unwrap().binary_attr);
    assert!(t.column("id").unwrap().auto_increment);
    assert_eq!(t.column("note").unwrap().default, Some(MyDefault::Literal("it's \\ 'ok'".into())));
    assert_eq!(t.keys.len(), 2);
    assert_eq!(t.keys[1].parts[0].prefix, Some(10));
    assert_eq!(t.pk_columns(), vec!["id".to_string()]);
}

#[test]
fn parses_8_0_checks_partitions_functional_and_invisible_keys() {
    let t = parse_create_table(T80);
    assert!(t.unparsed.is_empty(), "{:?}", t.unparsed);
    assert_eq!(t.option("STATS_PERSISTENT"), Some("0"));
    assert_eq!(t.option("AUTO_INCREMENT"), Some("101"));
    assert!(t.partition.as_deref().unwrap().starts_with("PARTITION BY RANGE (year(`created_at`))"));
    assert!(t.column("secret").unwrap().invisible);
    assert_eq!(t.column("uuid").unwrap().default, Some(MyDefault::Expr("(uuid_to_bin(uuid()))".into())));
    assert_eq!(t.column("created_at").unwrap().default, Some(MyDefault::CurrentTimestamp(Some("3".into()))));
    assert_eq!(t.column("updated_at").unwrap().on_update, Some(String::new()));
    assert_eq!(t.column("total").unwrap().generated, Some(("((`amount` * 1.1))".into(), false)));
    assert_eq!(t.column("flag").unwrap().default, Some(MyDefault::Raw("b'0'".into())));
    let desc = t.keys.iter().find(|k| k.name.as_deref() == Some("idx_amount_desc")).unwrap();
    assert!(desc.invisible && desc.parts[0].desc);
    let func = t.keys.iter().find(|k| k.name.as_deref() == Some("idx_func")).unwrap();
    assert_eq!(func.parts[0].expr.as_deref(), Some("(lower(`code`))"));
    assert_eq!(t.checks[0].expr, "((`amount` >= 0))");
    assert_eq!(t.checks[0].name.as_deref(), Some("orders_chk_1"));
}

#[test]
fn parses_4_1_and_5_5_charsets_and_fks() {
    let t41 = parse_create_table(
        "CREATE TABLE `c` (\n  `id` int(11) NOT NULL default '0',\n  `name` varchar(10) character set euckr collate euckr_bin default NULL,\n  PRIMARY KEY  (`id`)\n) ENGINE=InnoDB DEFAULT CHARSET=latin1",
    );
    assert!(t41.unparsed.is_empty(), "{:?}", t41.unparsed);
    let n = t41.column("name").unwrap();
    assert_eq!((n.charset.as_deref(), n.collation.as_deref()), (Some("euckr"), Some("euckr_bin")));
    let t55 = parse_create_table(
        "CREATE TABLE `o` (\n  `id` int(11) NOT NULL,\n  `cid` int(11) DEFAULT NULL,\n  KEY `cid` (`cid`) USING BTREE,\n  CONSTRAINT `o_ibfk_1` FOREIGN KEY (`cid`) REFERENCES `c` (`id`) ON DELETE SET NULL ON UPDATE CASCADE\n) ENGINE=InnoDB DEFAULT CHARSET=utf8 ROW_FORMAT=COMPACT",
    );
    assert!(t55.unparsed.is_empty(), "{:?}", t55.unparsed);
    assert_eq!(t55.fks[0].ref_table, "c");
    assert_eq!(t55.fks[0].on_delete.as_deref(), Some("SET NULL"));
    assert_eq!(t55.fks[0].on_update.as_deref(), Some("CASCADE"));
    assert_eq!(t55.keys[0].using.as_deref(), Some("BTREE"));
}

#[test]
fn unknown_clauses_are_blockers_not_dropped() {
    let t = parse_create_table("CREATE TABLE `t` (\n  `a` int(11) NOT NULL FROBNICATE 3\n) ENGINE=InnoDB WITH SYSTEM VERSIONING");
    assert_eq!(t.unparsed.len(), 2, "{:?}", t.unparsed);
    let out = tr("CREATE TABLE `t` (\n  `a` int(11) NOT NULL FROBNICATE 3\n) ENGINE=InnoDB", &src("5.7.44"), &tgt("8.0.36", &[]));
    assert!(codes(&out.blockers).contains(&"UNPARSED_CLAUSE"));
    let out = tr("CREATE TABLE `t` (\n  `a` inet6 NOT NULL\n) ENGINE=InnoDB", &src("10.6.12-MariaDB"), &tgt("8.0.36", &[]));
    assert!(codes(&out.blockers).contains(&"UNPARSED_CLAUSE") || codes(&out.blockers).contains(&"UNSUPPORTED_COLUMN"));
}

#[test]
fn golden_4_0_euckr_into_8_0() {
    let out = tr(T40, &src("4.0.27-standard"), &tgt("8.0.36", &[]));
    let sql = &out.create_sql;
    assert!(sql.starts_with("CREATE TABLE `shop2`.`t` (\n"), "{sql}");
    assert_eq!(line(sql, "id"), "`id` int NOT NULL AUTO_INCREMENT");
    assert_eq!(line(sql, "name"), "`name` varchar(20) NOT NULL DEFAULT ''");
    assert_eq!(line(sql, "kind"), "`kind` enum('가','나') NOT NULL DEFAULT '가'");
    assert_eq!(line(sql, "created"), "`created` timestamp NOT NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP");
    // zero_dates auto on 8.0 resolves to null.
    assert_eq!(line(sql, "stamp8"), "`stamp8` timestamp NULL DEFAULT NULL");
    assert_eq!(line(sql, "d"), "`d` date DEFAULT NULL");
    assert_eq!(line(sql, "price"), "`price` decimal(9,2) NOT NULL DEFAULT '0.00'");
    assert_eq!(line(sql, "ratio"), "`ratio` float DEFAULT NULL");
    assert_eq!(line(sql, "pw"), "`pw` varchar(20) COLLATE utf8mb4_bin NOT NULL DEFAULT ''");
    assert_eq!(line(sql, "body"), "`body` mediumtext");
    assert_eq!(line(sql, "data"), "`data` blob");
    assert_eq!(line(sql, "y"), "`y` year DEFAULT NULL");
    assert_eq!(line(sql, "note"), "`note` varchar(10) DEFAULT 'it''s \\\\ ''ok'''");
    assert!(sql.contains("  PRIMARY KEY (`id`)"));
    assert!(!sql.contains("idx_name"), "secondary keys go after the data on InnoDB");
    assert!(sql.ends_with(") ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci ROW_FORMAT=DYNAMIC COMMENT='한국어 설명'"), "{sql}");
    assert_eq!(out.post_keys.len(), 1);
    assert_eq!(out.post_keys[0].clause, "ADD KEY `idx_name` (`name`(10))");
    assert_eq!(out.engine, "InnoDB");
    assert!(out.first_timestamp_auto);
    let w = codes(&out.warnings);
    for c in ["ENGINE_CHANGED", "COLLATION_CHANGED", "ZERO_DATE_NULL", "TEXT_PROMOTED", "DECIMAL_WIDENED", "YEAR2_WIDENED", "TIMESTAMP_SEMANTICS", "TABLE_OPTION_DROPPED"]
    {
        assert!(w.contains(&c), "{c} missing in {w:?}");
    }
    assert!(out.blockers.is_empty(), "{:?}", out.blockers);
    // Per-column reads of a 4.0 (text protocol) source.
    assert_eq!(
        out.select_sql,
        "SELECT `id`, `name`, `kind`, UNIX_TIMESTAMP(`created`) AS `created`, UNIX_TIMESTAMP(`stamp8`) AS `stamp8`, UNIX_TIMESTAMP(`stamp6`) AS `stamp6`, \
         UNIX_TIMESTAMP(`stamp12`) AS `stamp12`, CAST(`d` AS CHAR) AS `d`, `price`, (`ratio` + 0e0) AS `ratio`, `pw`, `body`, `data`, (`y` + 0) AS `y`, `note` FROM `shop`.`t`"
    );
    assert!(out.verify_sql.starts_with("SELECT `id`, `name`, `kind`, CAST(`created` AS CHAR) AS `created`"));
    assert!(out.verify_sql.ends_with("FROM `shop2`.`t`"));
    let col = |n: &str| out.columns.iter().find(|c| c.source == n).unwrap();
    assert_eq!((col("created").value, col("created").zero_date), (ValueKind::Timestamp, ZeroDateAction::Min));
    assert_eq!(col("stamp8").zero_date, ZeroDateAction::Null);
    assert_eq!(col("stamp8").invalid_date, ZeroDateAction::Null);
    assert_eq!((col("d").read, col("d").zero_date), (SourceRead::CastChar, ZeroDateAction::Null));
    assert_eq!(col("ratio").read, SourceRead::FloatWiden);
    assert!(col("pw").binary_attr);
    assert_eq!(col("data").value, ValueKind::Binary);
    assert_eq!(col("y").read, SourceRead::YearNumber);
    assert_eq!(col("name").max_bytes, Some(80));
}

#[test]
fn reads_and_timestamps_by_source_version() {
    let ddl = "CREATE TABLE `t` (\n  `a` datetime NOT NULL default '0000-00-00 00:00:00',\n  `b` timestamp(14) NOT NULL,\n  `c` double default NULL\n) TYPE=MyISAM";
    let out = tr(ddl, &src("4.0.1"), &tgt("8.0.36", &[]));
    assert!(out.select_sql.contains("CONCAT(`a`) AS `a`"), "{}", out.select_sql);
    assert!(codes(&out.warnings).contains(&"FLOAT_TEXT_PRECISION"));
    let ddl41 = "CREATE TABLE `t` (\n  `a` datetime NOT NULL default '0000-00-00 00:00:00',\n  `b` timestamp NOT NULL default CURRENT_TIMESTAMP on update CURRENT_TIMESTAMP\n) ENGINE=MyISAM DEFAULT CHARSET=euckr";
    let mut s = src("4.1.2");
    s.text_protocol = false;
    let out = tr(ddl41, &s, &tgt("8.0.36", &[]));
    assert!(out.select_sql.contains("UNIX_TIMESTAMP(`b`) AS `b`") && out.select_sql.contains("CAST(`a` AS CHAR) AS `a`"));
    assert!(!out.first_timestamp_auto, "4.1 prints the auto-set clauses itself");
    let out = tr(ddl41, &src("4.1.3"), &tgt("8.0.36", &[]));
    assert!(out.select_sql.contains("CAST(`b` AS CHAR) AS `b`"));
    assert_eq!(line(&out.create_sql, "b"), "`b` timestamp NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP", "null policy: zero values become NULL");
    // DATETIME DEFAULT CURRENT_TIMESTAMP needs 5.6.5+.
    let out = tr(
        "CREATE TABLE `t` (\n  `a` datetime NOT NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP\n) ENGINE=InnoDB DEFAULT CHARSET=utf8",
        &src("5.7.44"),
        &tgt("5.5.62", &[]),
    );
    assert_eq!(line(&out.create_sql, "a"), "`a` datetime NOT NULL");
    assert!(codes(&out.warnings).contains(&"DEFAULT_DROPPED"));
}

#[test]
fn zero_dates_per_policy_and_keys() {
    let ddl = "CREATE TABLE `t` (\n  `d` date NOT NULL default '0000-00-00',\n  `u` datetime NOT NULL default '0000-00-00 00:00:00',\n  `p` date NOT NULL default '2020-00-15',\n  PRIMARY KEY  (`d`),\n  UNIQUE KEY `uu` (`u`)\n) TYPE=MyISAM";
    let mut t = tgt("8.0.36", &[]);
    t.zero = ZeroDatePolicy::Null;
    let out = tr(ddl, &src("4.0.27"), &t);
    assert_eq!(line(&out.create_sql, "d"), "`d` date NOT NULL DEFAULT '1000-01-01'", "a PRIMARY KEY column uses min even under null");
    assert_eq!(line(&out.create_sql, "u"), "`u` datetime DEFAULT NULL");
    assert_eq!(line(&out.create_sql, "p"), "`p` date DEFAULT NULL");
    assert!(codes(&out.warnings).contains(&"ZERO_DATE_IN_KEY"));
    assert_eq!(out.columns[0].zero_date, ZeroDateAction::Min);
    t.zero = ZeroDatePolicy::Min;
    let out = tr(ddl, &src("4.0.27"), &t);
    assert_eq!(line(&out.create_sql, "u"), "`u` datetime NOT NULL DEFAULT '1000-01-01 00:00:00'");
    assert_eq!(line(&out.create_sql, "p"), "`p` date NOT NULL DEFAULT '2020-01-15'");
    assert!(codes(&out.warnings).contains(&"ZERO_DATE_MIN"));
    t.zero = ZeroDatePolicy::Keep;
    let out = tr(ddl, &src("4.0.27"), &t);
    assert_eq!(line(&out.create_sql, "u"), "`u` datetime NOT NULL DEFAULT '0000-00-00 00:00:00'");
    assert!(codes(&out.warnings).contains(&"ZERO_DATE_KEEP"));
    // TIMESTAMP minimum under min; keep on 5.6 (non-strict default) resolves to keep.
    let out = tr(
        "CREATE TABLE `t` (\n  `a` timestamp(14) NOT NULL,\n  `b` timestamp(14) NOT NULL\n) TYPE=MyISAM",
        &src("4.0.27"),
        &MyTargetCtx { zero: ZeroDatePolicy::Min, ..tgt("8.0.36", &[]) },
    );
    assert_eq!(line(&out.create_sql, "b"), "`b` timestamp NOT NULL DEFAULT '1970-01-01 00:00:01'");
    assert_eq!(tgt("5.6.51", &[]).zero, ZeroDatePolicy::Keep);
    assert_eq!(tgt("5.7.44", &[]).zero, ZeroDatePolicy::Null);
    assert_eq!(resolve_zero_dates(ZeroDatePolicy::Auto, "STRICT_TRANS_TABLES,NO_ZERO_IN_DATE"), ZeroDatePolicy::Null);
    assert_eq!(resolve_zero_dates(ZeroDatePolicy::Auto, "NO_ZERO_IN_DATE"), ZeroDatePolicy::Keep);
}

#[test]
fn long_indexed_varchar_5_6_compact_vs_8_0() {
    let ddl = "CREATE TABLE `u` (\n  `id` int(11) NOT NULL,\n  `email` varchar(255) NOT NULL,\n  `tag` varchar(255) DEFAULT NULL,\n  PRIMARY KEY (`id`),\n  UNIQUE KEY `uk_email` (`email`),\n  KEY `idx_tag` (`tag`)\n) ENGINE=InnoDB DEFAULT CHARSET=utf8";
    let out = tr(ddl, &src("5.5.62"), &tgt("5.6.51", &[]));
    assert!(!out.create_sql.contains("ROW_FORMAT"), "5.6 without Barracuda stays COMPACT");
    let uk = out.post_keys.iter().find(|k| k.name == "uk_email").unwrap();
    assert_eq!(uk.clause, "ADD UNIQUE KEY `uk_email` (`email`(191))");
    assert!(codes(&uk.warnings).contains(&"UNIQUE_PREFIX"));
    assert_eq!(uk.warnings[0].severity, crate::types::Severity::Danger);
    let k = out.post_keys.iter().find(|k| k.name == "idx_tag").unwrap();
    assert_eq!(k.clause, "ADD KEY `idx_tag` (`tag`(191))");
    assert!(codes(&k.warnings).contains(&"INDEX_PREFIX"));
    // 5.6 with Barracuda + file_per_table + large_prefix: DYNAMIC rows, no prefix.
    let out = tr(ddl, &src("5.5.62"), &tgt("5.6.51", &[("innodb_file_format", "Barracuda"), ("innodb_file_per_table", "ON"), ("innodb_large_prefix", "ON")]));
    assert!(out.create_sql.contains("ROW_FORMAT=DYNAMIC"));
    assert!(out.post_keys.iter().all(|k| !k.clause.contains("(191)")));
    let out = tr(ddl, &src("5.5.62"), &tgt("8.0.36", &[]));
    assert!(out.post_keys.iter().all(|k| !k.clause.contains("(191)") && k.warnings.iter().all(|w| !w.code.ends_with("_PREFIX"))), "{:?}", out.post_keys);
    // MyISAM: 1000 bytes per key.
    let mut t = tgt("8.0.36", &[]);
    t.engine = MysqlEngineChoice::Myisam;
    let out = tr(ddl, &src("5.5.62"), &t);
    assert!(out.create_sql.contains("UNIQUE KEY `uk_email` (`email`(250))"), "{}", out.create_sql);
    assert!(out.disable_keys && out.post_keys.is_empty());
}

#[test]
fn row_size_limits() {
    let wide = |n: usize| {
        let cols: Vec<String> = (0..n).map(|i| format!("  `c{i}` varchar(255) DEFAULT NULL")).collect();
        format!("CREATE TABLE `w` (\n  `id` int(11) NOT NULL,\n{},\n  PRIMARY KEY (`id`)\n) ENGINE=InnoDB DEFAULT CHARSET=latin1", cols.join(",\n"))
    };
    // 70 x varchar(255) utf8mb4 = over 65,535 bytes: the widest become TEXT.
    let out = tr(&wide(70), &src("5.5.62"), &tgt("8.0.36", &[]));
    assert!(codes(&out.warnings).contains(&"ROW_SIZE_FIXED"), "{:?}", out.warnings);
    assert!(out.create_sql.contains("`c0` text") || out.create_sql.contains(" text DEFAULT NULL"));
    assert!(out.blockers.is_empty(), "{:?}", out.blockers);
    // 30 x varchar(255): fine on DYNAMIC (8.0), over the in-row limit on 5.6 COMPACT.
    assert!(tr(&wide(30), &src("5.5.62"), &tgt("8.0.36", &[])).blockers.is_empty());
    let out = tr(&wide(30), &src("5.5.62"), &tgt("5.6.51", &[]));
    assert!(codes(&out.blockers).contains(&"INNODB_ROW_SIZE"), "{:?}", out.blockers);
    let mut t = tgt("5.6.51", &[]);
    t.fix_row_size = false;
    assert!(codes(&tr(&wide(70), &src("5.5.62"), &t).blockers).contains(&"ROW_SIZE_LIMIT"));
    // VARCHAR wider than 65,535 bytes in the target charset becomes MEDIUMTEXT.
    let out = tr("CREATE TABLE `v` (\n  `a` varchar(20000) DEFAULT NULL\n) ENGINE=InnoDB DEFAULT CHARSET=latin1", &src("5.5.62"), &tgt("8.0.36", &[]));
    assert_eq!(line(&out.create_sql, "a"), "`a` mediumtext DEFAULT NULL");
    assert!(codes(&out.warnings).contains(&"VARCHAR_TO_TEXT"));
}

#[test]
fn engine_rules() {
    let grouped = "CREATE TABLE `g` (\n  `grp` int(11) NOT NULL,\n  `id` int(11) NOT NULL auto_increment,\n  PRIMARY KEY  (`grp`,`id`)\n) TYPE=MyISAM";
    let out = tr(grouped, &src("4.0.27"), &tgt("8.0.36", &[]));
    assert_eq!(out.engine, "MyISAM");
    assert!(codes(&out.warnings).contains(&"AUTOINC_GROUPED"));
    let ft = "CREATE TABLE `f` (\n  `id` int(11) NOT NULL,\n  `body` text,\n  PRIMARY KEY  (`id`),\n  FULLTEXT KEY `ft` (`body`)\n) TYPE=MyISAM";
    let out = tr(ft, &src("4.0.27"), &tgt("5.5.62", &[]));
    assert_eq!(out.engine, "MyISAM");
    assert!(codes(&out.warnings).contains(&"ENGINE_KEPT_MYISAM"));
    assert!(out.create_sql.contains("FULLTEXT KEY `ft` (`body`)"));
    let out = tr(ft, &src("4.0.27"), &tgt("8.0.36", &[]));
    assert_eq!(out.engine, "InnoDB");
    assert!(out.post_keys[0].separate, "InnoDB builds one FULLTEXT index per ALTER");
    let heap = tr("CREATE TABLE `h` (\n  `id` int(11) NOT NULL\n) TYPE=HEAP", &src("4.0.27"), &MyTargetCtx { engine: MysqlEngineChoice::Keep, ..tgt("8.0.36", &[]) });
    assert_eq!(heap.engine, "MEMORY");
    let merge = tr("CREATE TABLE `m` (\n  `id` int(11) NOT NULL\n) ENGINE=MRG_MyISAM UNION=(`a`,`b`)", &src("5.5.62"), &tgt("8.0.36", &[]));
    assert!(merge.unsupported.is_some());
    let fed = "CREATE TABLE `x` (\n  `id` int(11) NOT NULL\n) ENGINE=FEDERATED CONNECTION='mysql://u:secret@h/db/x'";
    assert!(!redact_connection(fed).contains("secret"));
    assert!(tr(fed, &src("5.5.62"), &tgt("8.0.36", &[])).unsupported.is_some());
    // 5.0 target: no InnoDB default under auto -> the source engine.
    let out = tr("CREATE TABLE `t` (\n  `id` int(11) NOT NULL\n) TYPE=MyISAM", &src("4.0.27"), &tgt("5.0.96", &[]));
    assert_eq!(out.engine, "MyISAM");
    assert!(out.create_sql.contains("DEFAULT CHARSET=utf8 COLLATE=utf8_general_ci"), "{}", out.create_sql);
}

#[test]
fn golden_ddl_across_target_versions() {
    let ddl = "CREATE TABLE `p` (\n  `id` int(11) NOT NULL auto_increment,\n  `name` varchar(50) NOT NULL default '',\n  `j` text,\n  PRIMARY KEY  (`id`),\n  KEY `n` (`name`)\n) TYPE=MyISAM";
    let cases: &[(&str, &str)] = &[
        ("5.0.96", ") ENGINE=MyISAM DEFAULT CHARSET=utf8 COLLATE=utf8_general_ci"),
        ("5.5.2", ") ENGINE=InnoDB DEFAULT CHARSET=utf8 COLLATE=utf8_general_ci"),
        ("5.6.51", ") ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_general_ci"),
        ("5.7.44", ") ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_general_ci ROW_FORMAT=DYNAMIC"),
        ("8.0.36", ") ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci ROW_FORMAT=DYNAMIC"),
        ("8.4.3", ") ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci ROW_FORMAT=DYNAMIC"),
        ("9.0.1", ") ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci ROW_FORMAT=DYNAMIC"),
    ];
    for (v, tail) in cases {
        let out = tr(ddl, &src("4.0.27"), &tgt(v, &[]));
        assert!(out.create_sql.ends_with(tail), "{v}: {}", out.create_sql);
        let id = line(&out.create_sql, "id");
        if MyVersion::parse(v).at(8, 0, 17) {
            assert_eq!(id, "`id` int NOT NULL AUTO_INCREMENT", "{v}");
        } else {
            assert_eq!(id, "`id` int(11) NOT NULL AUTO_INCREMENT", "{v}");
        }
        // 4.0 text is EUC-KR (2 bytes per char): TEXT is promoted on every utf8 target.
        assert_eq!(line(&out.create_sql, "j"), "`j` mediumtext", "{v}");
    }
}

#[test]
fn collations_mapping_pad_notes_and_legacy_preset() {
    let ddl = "CREATE TABLE `c` (\n  `a` varchar(10) CHARACTER SET latin1 COLLATE latin1_general_cs DEFAULT NULL,\n  `b` varchar(10) CHARACTER SET utf8 COLLATE utf8_unicode_ci DEFAULT NULL,\n  `c` varchar(10) DEFAULT NULL,\n  UNIQUE KEY `uc` (`c`)\n) ENGINE=InnoDB DEFAULT CHARSET=euckr";
    let out = tr(ddl, &src("5.5.62"), &tgt("8.0.36", &[]));
    assert_eq!(line(&out.create_sql, "a"), "`a` varchar(10) COLLATE utf8mb4_0900_as_cs DEFAULT NULL");
    assert_eq!(line(&out.create_sql, "b"), "`b` varchar(10) COLLATE utf8mb4_unicode_ci DEFAULT NULL");
    assert_eq!(line(&out.create_sql, "c"), "`c` varchar(10) DEFAULT NULL");
    let cc = out.warnings.iter().find(|w| w.code == "COLLATION_CHANGED").unwrap();
    assert!(cc.recommendation.as_deref().unwrap().contains("NO PAD"), "{cc:?}");
    assert!(out.post_keys[0].warnings.iter().any(|w| w.code == "COLLATION_UNIQUE_RISK"));
    let out = tr(ddl, &src("5.5.62"), &tgt("5.7.44", &[]));
    assert_eq!(line(&out.create_sql, "a"), "`a` varchar(10) COLLATE utf8mb4_bin DEFAULT NULL");
    // `legacy` preset: PAD SPACE.
    let mut t = tgt("8.0.36", &[]);
    t.user_collation = Some(t.legacy_collation("utf8mb4"));
    let out = tr(ddl, &src("5.5.62"), &t);
    assert!(out.create_sql.contains("COLLATE=utf8mb4_unicode_520_ci"), "{}", out.create_sql);
    assert_eq!(tgt("5.5.62", &[]).legacy_collation("utf8mb4"), "utf8mb4_general_ci");
    // A 0900 collation on a pre-8.0 target.
    let out =
        tr("CREATE TABLE `z` (\n  `a` varchar(5) DEFAULT NULL\n) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci", &src("8.0.36"), &tgt("5.7.44", &[]));
    assert!(out.create_sql.contains("COLLATE=utf8mb4_unicode_520_ci"));
    assert!(codes(&out.warnings).contains(&"DOWNGRADE"));
}

#[test]
fn enum_duplicates_under_ci_collations() {
    let out = tr("CREATE TABLE `e` (\n  `a` enum('Ａ','A') default NULL\n) TYPE=MyISAM", &src("4.0.27"), &tgt("8.0.36", &[]));
    assert!(codes(&out.blockers).contains(&"ENUM_DUPLICATE"));
    let out = tr("CREATE TABLE `e` (\n  `a` enum('가','나') default NULL\n) TYPE=MyISAM", &src("4.0.27"), &tgt("8.0.36", &[]));
    assert!(out.blockers.is_empty());
}

#[test]
fn version_specific_columns_and_keys() {
    // 8.0 source into 5.7: functional key, invisible column, expression default, CHECK, srid.
    let out = tr(T80, &src("8.0.36"), &tgt("5.7.44", &[]));
    let b = codes(&out.blockers);
    assert!(b.contains(&"UNSUPPORTED_COLUMN"), "invisible column needs 8.0.23: {b:?}");
    let w = codes(&out.warnings);
    assert!(w.contains(&"DOWNGRADE"));
    assert!(!out.create_sql.contains("CHECK"), "CHECK is ignored before 8.0.16");
    assert!(out.post_keys.iter().all(|k| k.name != "idx_func"), "functional keys need 8.0.13");
    // Same table into 8.0: kept.
    let out = tr(T80, &src("8.0.36"), &tgt("8.0.36", &[]));
    assert!(out.blockers.is_empty(), "{:?}", out.blockers);
    assert!(out.create_sql.contains("CONSTRAINT `orders_chk_1` CHECK ((`amount` >= 0))"));
    assert!(out.create_sql.contains("`total` decimal(11,2) GENERATED ALWAYS AS ((`amount` * 1.1)) VIRTUAL"));
    assert!(out.create_sql.contains("`uuid` binary(16) NOT NULL DEFAULT (uuid_to_bin(uuid()))"));
    assert!(out.create_sql.contains("`created_at` datetime(3) NOT NULL DEFAULT CURRENT_TIMESTAMP(3)"));
    assert!(out.create_sql.contains("`secret` varchar(10) DEFAULT NULL INVISIBLE"));
    assert!(out.create_sql.contains("\nPARTITION BY RANGE (year(`created_at`))"));
    assert!(out.post_keys.iter().any(|k| k.clause == "ADD KEY `idx_amount_desc` (`amount` DESC) INVISIBLE"));
    assert!(out.post_keys.iter().any(|k| k.clause == "ADD KEY `idx_func` ((lower(`code`)))"));
    assert!(!out.columns.iter().any(|c| c.source == "total"), "generated columns are never loaded");
    // BIT is read plainly but verified unsigned (the writer hashes the u64 it loads).
    assert!(out.select_sql.contains(", `flag` FROM "), "{}", out.select_sql);
    assert!(out.verify_sql.contains("CAST(`flag` AS UNSIGNED) AS `flag`"), "{}", out.verify_sql);
    assert!(codes(&out.warnings).contains(&"TABLE_OPTION_DROPPED"), "STATS_PERSISTENT dropped");
    // Partitioning a MyISAM table on 8.0 is refused.
    let mut t = tgt("8.0.36", &[]);
    t.engine = MysqlEngineChoice::Myisam;
    assert!(codes(&tr(T80, &src("8.0.36"), &t).blockers).contains(&"PARTITION_UNSUPPORTED"));
    // sql_require_primary_key.
    let out = tr("CREATE TABLE `n` (\n  `a` int(11) DEFAULT NULL\n) TYPE=MyISAM", &src("4.0.27"), &tgt("8.0.36", &[("sql_require_primary_key", "ON")]));
    assert!(codes(&out.blockers).contains(&"TGT_REQUIRES_PK"));
    // Reserved words new on the target.
    let out = tr("CREATE TABLE `rank` (\n  `rows` int(11) DEFAULT NULL\n) ENGINE=InnoDB DEFAULT CHARSET=utf8", &src("5.7.44"), &tgt("8.0.36", &[]));
    let rw = out.warnings.iter().find(|w| w.code == "RESERVED_WORD").unwrap();
    assert!(rw.detail.as_deref().unwrap().contains("rank") && rw.detail.as_deref().unwrap().contains("rows"));
    // DDL placement: indexes with the table when indexes_after_data = false.
    let mut t = tgt("8.0.36", &[]);
    t.indexes_after_data = false;
    let out = tr(T40, &src("4.0.27"), &t);
    assert!(out.post_keys.is_empty() && out.create_sql.contains("KEY `idx_name` (`name`(10))"));
}

#[test]
fn views_triggers_and_definers() {
    let view = "CREATE ALGORITHM=UNDEFINED DEFINER=`app`@`%` SQL SECURITY DEFINER VIEW `v` AS select `shop`.`t`.`a` AS `a`,'shop.t' AS `lit` from (`shop`.`t` join `shop`.`u` on((`shop`.`t`.`a` = `shop`.`u`.`a`)))";
    let (sql, definer) = view_create_sql(view, "shop", "shop2", "v", false);
    assert_eq!(definer, Some(("app".to_string(), "%".to_string())));
    assert_eq!(
        sql,
        "CREATE ALGORITHM=UNDEFINED SQL SECURITY DEFINER VIEW `shop2`.`v` AS select `shop2`.`t`.`a` AS `a`,'shop.t' AS `lit` from (`shop2`.`t` join `shop2`.`u` on((`shop2`.`t`.`a` = `shop2`.`u`.`a`)))"
    );
    let (kept, _) = view_create_sql(view, "shop", "shop2", "v", true);
    assert!(kept.contains("DEFINER=`app`@`%`"));
    assert_eq!(qualified_refs(view, "shop"), ["t", "u"].iter().map(|s| s.to_string()).collect());
    // View order by references.
    let names = vec!["v2".to_string(), "v1".to_string()];
    let deps = vec![["v1".to_string()].into_iter().collect(), BTreeSet::new()];
    assert_eq!(order_views(&names, &deps), vec![1, 0]);

    let trg = "CREATE DEFINER=`root`@`localhost` TRIGGER `trg` BEFORE INSERT ON `t` FOR EACH ROW SET NEW.x = (SELECT COUNT(*) FROM shop.u)";
    let (sql, definer, refs) = trigger_create_sql(trg, "shop", "shop2", "t", false);
    assert_eq!(sql, "CREATE TRIGGER `shop2`.`trg` BEFORE INSERT ON `shop2`.`t` FOR EACH ROW SET NEW.x = (SELECT COUNT(*) FROM shop.u)");
    assert!(definer.is_some() && refs, "the body names the source database");
    let (_, _, refs) = trigger_create_sql("CREATE TRIGGER `trg` AFTER UPDATE ON `t` FOR EACH ROW SET @x = 1", "shop", "shop2", "t", false);
    assert!(!refs);
    // 5.0.10 - 5.1.20: built from SHOW TRIGGERS columns.
    assert_eq!(trigger_from_columns("trg", "before", "insert", "t", "SET NEW.a = 1"), "CREATE TRIGGER `trg` BEFORE INSERT ON `t` FOR EACH ROW SET NEW.a = 1");
    let t = tgt("8.0.36", &[]);
    assert_eq!(trigger_sql_mode("STRICT_TRANS_TABLES,NO_AUTO_CREATE_USER", &t), "STRICT_TRANS_TABLES");
    assert_eq!(trigger_sql_mode("STRICT_TRANS_TABLES,NO_AUTO_CREATE_USER", &tgt("5.7.44", &[])), "STRICT_TRANS_TABLES,NO_AUTO_CREATE_USER");
}

#[test]
fn lexer_handles_versioned_comments_and_strings() {
    let s = unversion_comments("a /*!50100 PARTITION BY HASH (id) */ 'x /*!123 y */' /* plain */");
    assert!(s.contains("PARTITION BY HASH (id)") && !s.contains("/*!50100"));
    assert!(s.contains("'x /*!123 y */'"), "strings are untouched: {s}");
    assert_eq!(s.len(), "a /*!50100 PARTITION BY HASH (id) */ 'x /*!123 y */' /* plain */".len());
    let toks = lex("`a``b` 'c''d' b'01' _utf8mb4'x' 1.5e3 -- comment\n x");
    assert_eq!(toks[0].v, "a`b");
    assert_eq!(toks[1].v, "c'd");
    assert_eq!(toks[2].k, TK::Prefixed);
    assert_eq!(toks[3].k, TK::Prefixed);
    assert_eq!(toks[4].v, "1.5e3");
    assert_eq!(toks[5].v, "x");
    assert_eq!(MyVersion::parse("5.5.5-10.6.12-MariaDB"), MyVersion { maria: true, major: 10, minor: 6, patch: 12 });
    assert_eq!(MyVersion::parse("5.7.44-ndb-7.6.29").minor, 7);
}

#[test]
fn treat_as_binary_keeps_the_char_bytes() {
    // 4.0 `char(16) binary` holding hash bytes (BINARY_ATTR_UNDECODABLE): VARBINARY keeps the
    // bytes the source CHAR returns (trailing spaces stripped); BINARY(16) would pad a 13-byte
    // value with 0x00 to 16 bytes and the checksum would differ.
    let ddl = "CREATE TABLE `h` (\n  `id` int(11) NOT NULL default '0',\n  `hash` char(16) binary NOT NULL default '',\n  `memo` varchar(20) binary default NULL,\n  PRIMARY KEY  (`id`)\n) TYPE=MyISAM";
    let table = parse_create_table(ddl);
    let mut ov = MyTableOverrides::default();
    ov.columns.insert("hash".into(), ColumnTreatment::Binary);
    ov.columns.insert("memo".into(), ColumnTreatment::Binary);
    let out = translate_table(&table, &table.name, &src("4.0.27"), &tgt("8.0.36", &[]), &ov, &BTreeMap::new());
    assert!(out.create_sql.contains("`hash` varbinary(16)"), "{}", out.create_sql);
    assert!(!out.create_sql.contains("`hash` binary("), "{}", out.create_sql);
    assert!(out.create_sql.contains("`memo` varbinary(20)"), "{}", out.create_sql);
    let col = |n: &str| out.columns.iter().find(|c| c.source == n).unwrap();
    assert_eq!((col("hash").value, col("hash").max_bytes), (ValueKind::Binary, Some(16)));
    // Without the override the column stays text.
    let out = translate_table(&table, &table.name, &src("4.0.27"), &tgt("8.0.36", &[]), &MyTableOverrides::default(), &BTreeMap::new());
    assert!(out.create_sql.contains("`hash` char(16)"), "{}", out.create_sql);
}
