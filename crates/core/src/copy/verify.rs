//! Verification and outcome (section 5.6): row counts, the `lossy` rule, problems, the MySQL FK
//! orphan probes and the outcome of a whole run. Owner: copy-data.

use super::types::*;
use super::{CODE_DATA_WARNINGS, CODE_INVALID_DATE, CODE_ROWS_SKIPPED, CODE_UNMAPPABLE, CODE_VERIFY_MISMATCH};

/// Verify outcome of one loaded table from the counts and the loss counters (5.6): a count
/// difference is `mismatch`; equal (or uncounted) rows with data warnings, skipped rows or
/// unmappable characters are `lossy` (never `match` with altered values); else `match`, or
/// `not_run` without a count.
pub fn table_outcome(rows_read: u64, target_count: Option<u64>, data_warnings: u64, unmappable: u64) -> VerifyOutcome {
    match target_count {
        Some(c) if c != rows_read => VerifyOutcome::Mismatch,
        _ if data_warnings > 0 || unmappable > 0 => VerifyOutcome::Lossy,
        Some(_) => VerifyOutcome::Match,
        None => VerifyOutcome::NotRun,
    }
}

/// Final state and problem codes of a loaded table (5.6). `Failed` for a mismatch, data
/// warnings, skipped rows, or unmappable characters under `fail`; problems (`done with
/// problems`) for unmappable characters under `replace` and replaced invalid dates.
pub fn table_verdict(t: &CopyTableResult, skipped_rows: u64, on_unmappable: UnmappablePolicy) -> (CopyItemState, Vec<String>) {
    let mut problems: Vec<String> = t.problems.clone();
    let mut failed = t.state == CopyItemState::Failed;
    if t.verify == VerifyOutcome::Mismatch {
        failed = true;
        push_unique(&mut problems, CODE_VERIFY_MISMATCH);
    }
    if t.data_warnings > 0 {
        failed = true;
        push_unique(&mut problems, CODE_DATA_WARNINGS);
    }
    if skipped_rows > 0 {
        failed = true;
        push_unique(&mut problems, CODE_ROWS_SKIPPED);
    }
    if t.unmappable_chars > 0 {
        if on_unmappable == UnmappablePolicy::Fail {
            failed = true;
        }
        push_unique(&mut problems, CODE_UNMAPPABLE);
    }
    if t.value_fixes.iter().any(|f| f.code == CODE_INVALID_DATE && f.count > 0) {
        push_unique(&mut problems, CODE_INVALID_DATE);
    }
    (if failed { CopyItemState::Failed } else { t.state }, problems)
}

fn push_unique(v: &mut Vec<String>, code: &str) {
    if !v.iter().any(|c| c == code) {
        v.push(code.to_string());
    }
}

/// Problem codes that make a run "done with problems" (not failures by themselves).
pub fn is_problem_only(code: &str) -> bool {
    matches!(code, "UNMAPPABLE_CHARS" | "INVALID_DATE" | "FK_ORPHANS" | "SKIPPED_DIFFERS")
}

/// Totals of a result: (`problems` = tables / items with problem codes, `verified`).
pub fn summarize(tables: &[CopyTableResult], verify: VerifyMode, extra_problems: u32) -> (u32, bool) {
    let problems = tables.iter().filter(|t| !t.problems.is_empty()).count() as u32 + extra_problems;
    let verified = verify != VerifyMode::None
        && problems == 0
        && tables.iter().all(|t| t.state != CopyItemState::Failed)
        && tables
            .iter()
            .filter(|t| t.state != CopyItemState::Skipped && (t.target_count.is_some() || t.verify != VerifyOutcome::NotRun))
            .all(|t| t.verify == VerifyOutcome::Match);
    (problems, verified)
}

/// One foreign key of a MySQL `ALTER TABLE … ADD CONSTRAINT … FOREIGN KEY (…) REFERENCES …`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForeignKey {
    pub name: String,
    pub child: String,
    pub child_cols: Vec<String>,
    pub parent: String,
    pub parent_cols: Vec<String>,
}

fn split_cols(s: &str) -> Vec<String> {
    s.split(',').map(|c| c.trim().to_string()).filter(|c| !c.is_empty()).collect()
}

/// The FKs added by a MySQL post-data statement (best effort; quoted identifiers kept quoted).
pub fn mysql_foreign_keys(sql: &str) -> Vec<ForeignKey> {
    let table = regex::Regex::new(r"(?is)^\s*ALTER\s+TABLE\s+((?:`(?:[^`]|``)+`\s*\.\s*)?`(?:[^`]|``)+`)").expect("regex");
    let fk =
        regex::Regex::new(r"(?is)CONSTRAINT\s+(`(?:[^`]|``)+`)\s+FOREIGN\s+KEY\s*\(([^)]*)\)\s*REFERENCES\s+((?:`(?:[^`]|``)+`\s*\.\s*)?`(?:[^`]|``)+`)\s*\(([^)]*)\)")
            .expect("regex");
    let Some(child) = table.captures(sql).map(|c| c[1].to_string()) else {
        return Vec::new();
    };
    fk.captures_iter(sql)
        .map(|c| ForeignKey { name: c[1].to_string(), child: child.clone(), child_cols: split_cols(&c[2]), parent: c[3].to_string(), parent_cols: split_cols(&c[4]) })
        .filter(|f| !f.child_cols.is_empty() && f.child_cols.len() == f.parent_cols.len())
        .collect()
}

/// The orphan probe of one FK (5.6): one row when a child row has no parent.
pub fn orphan_probe_sql(fk: &ForeignKey) -> String {
    let on: Vec<String> = fk.child_cols.iter().zip(&fk.parent_cols).map(|(c, p)| format!("c.{c} = p.{p}")).collect();
    let not_null: Vec<String> = fk.child_cols.iter().map(|c| format!("c.{c} IS NOT NULL")).collect();
    format!(
        "SELECT 1 AS orphan FROM {} c LEFT JOIN {} p ON {} WHERE {} AND p.{} IS NULL LIMIT 1",
        fk.child,
        fk.parent,
        on.join(" AND "),
        not_null.join(" AND "),
        fk.parent_cols[0]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table(state: CopyItemState, verify: VerifyOutcome) -> CopyTableResult {
        CopyTableResult {
            item_id: "table:\"t\"".into(),
            target_name: "t".into(),
            state,
            rows_read: 10,
            rows_written: 10,
            target_count: Some(10),
            verify,
            checksum_source: None,
            checksum_target: None,
            unmappable_chars: 0,
            data_warnings: 0,
            warning_samples: vec![],
            value_fixes: vec![],
            problems: vec![],
            duration_ms: 1,
            method: None,
            error: None,
        }
    }

    #[test]
    fn outcomes() {
        assert_eq!(table_outcome(10, Some(10), 0, 0), VerifyOutcome::Match);
        assert_eq!(table_outcome(10, Some(9), 0, 0), VerifyOutcome::Mismatch);
        assert_eq!(table_outcome(10, Some(10), 2, 0), VerifyOutcome::Lossy);
        assert_eq!(table_outcome(10, Some(10), 0, 3), VerifyOutcome::Lossy);
        assert_eq!(table_outcome(10, None, 0, 0), VerifyOutcome::NotRun);
        assert_eq!(table_outcome(10, None, 1, 0), VerifyOutcome::Lossy);
        assert_eq!(table_outcome(10, Some(8), 1, 0), VerifyOutcome::Mismatch);
    }

    #[test]
    fn verdicts_and_problems() {
        let ok = table(CopyItemState::Done, VerifyOutcome::Match);
        assert_eq!(table_verdict(&ok, 0, UnmappablePolicy::Replace), (CopyItemState::Done, vec![]));
        let mut lossy = table(CopyItemState::Done, VerifyOutcome::Lossy);
        lossy.unmappable_chars = 2;
        assert_eq!(table_verdict(&lossy, 0, UnmappablePolicy::Replace), (CopyItemState::Done, vec![CODE_UNMAPPABLE.to_string()]));
        assert_eq!(table_verdict(&lossy, 0, UnmappablePolicy::Fail).0, CopyItemState::Failed);
        let mut warn = table(CopyItemState::Done, VerifyOutcome::Lossy);
        warn.data_warnings = 1;
        assert_eq!(table_verdict(&warn, 0, UnmappablePolicy::Replace).0, CopyItemState::Failed);
        assert_eq!(table_verdict(&ok, 3, UnmappablePolicy::Replace).1, vec![CODE_ROWS_SKIPPED.to_string()]);
        let mismatch = table(CopyItemState::Done, VerifyOutcome::Mismatch);
        assert_eq!(table_verdict(&mismatch, 0, UnmappablePolicy::Replace), (CopyItemState::Failed, vec![CODE_VERIFY_MISMATCH.to_string()]));
        let mut fixed = ok.clone();
        fixed.value_fixes = vec![ValueFix { column: "d".into(), code: CODE_INVALID_DATE.into(), count: 2 }];
        assert_eq!(table_verdict(&fixed, 0, UnmappablePolicy::Replace), (CopyItemState::Done, vec![CODE_INVALID_DATE.to_string()]));
        let mut with_problem = ok.clone();
        with_problem.problems = vec![CODE_INVALID_DATE.into()];
        assert_eq!(summarize(std::slice::from_ref(&ok), VerifyMode::RowCount, 0), (0, true));
        assert_eq!(summarize(&[ok.clone(), with_problem], VerifyMode::RowCount, 0), (1, false));
        assert_eq!(summarize(std::slice::from_ref(&ok), VerifyMode::None, 0), (0, false));
        assert_eq!(summarize(&[ok], VerifyMode::RowCount, 1), (1, false));
    }

    #[test]
    fn fk_probe_from_alter() {
        let sql = "ALTER TABLE `shop`.`orders` ADD CONSTRAINT `o_c` FOREIGN KEY (`customer_id`, `region`) REFERENCES `shop`.`customers` (`id`, `region`) ON DELETE CASCADE, ADD CONSTRAINT `o_p` FOREIGN KEY (`product_id`) REFERENCES `shop`.`products` (`id`)";
        let fks = mysql_foreign_keys(sql);
        assert_eq!(fks.len(), 2);
        assert_eq!(fks[0].child, "`shop`.`orders`");
        assert_eq!(fks[0].child_cols, vec!["`customer_id`", "`region`"]);
        assert_eq!(fks[1].parent, "`shop`.`products`");
        assert_eq!(
            orphan_probe_sql(&fks[1]),
            "SELECT 1 AS orphan FROM `shop`.`orders` c LEFT JOIN `shop`.`products` p ON c.`product_id` = p.`id` WHERE c.`product_id` IS NOT NULL AND p.`id` IS NULL LIMIT 1"
        );
        crate::copy::guard::check_read(CopyFamily::Mysql, &orphan_probe_sql(&fks[0])).unwrap();
        assert!(mysql_foreign_keys("CREATE TABLE t (a int)").is_empty());
    }
}
