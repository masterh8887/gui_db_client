//! `bulk-cli export --encoding`: SQLite data with Korean text written as an EUC-KR (CP949) CSV,
//! checked byte for byte. Runs the binary as a child process with `BULKDB_CONFIG_DIR` pointing
//! at a throw-away directory and `BULKDB_CREDENTIALS=memory` (same harness as `cli_sqlite.rs`).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};

use bulk_core::encoding::{decode_cell, Charset};

/// CP949 bytes of "한글".
const HANGUL_CP949: &[u8] = b"\xC7\xD1\xB1\xDB";

/// Korean text from SQLite: KS X 1001 Hangul, a UHC-only syllable (똠), Hangul in a column
/// name, an emoji CP949 cannot represent, a quoted field and a NULL.
const QUERY: &str = "select 1 as id, '한글' as \"이름\", 'x' as note \
                     union all select 2, '똠방각하', '쉼표, 따옴표 \"' \
                     union all select 3, 'emoji 🚀', null";

/// What the EUC-KR file must decode to (NULL as empty text, LF line ends).
const EXPECTED_TEXT: &str = "id,이름,note\n1,한글,x\n2,똠방각하,\"쉼표, 따옴표 \"\"\"\n3,emoji ?,\n";

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("bulk-cli-enc-{}-{n}-{tag}", std::process::id()));
        fs::create_dir_all(&dir).expect("create temp dir");
        TempDir(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct Fixture {
    dir: TempDir,
    db: PathBuf,
    config: PathBuf,
}

impl Fixture {
    fn new(tag: &str) -> Self {
        let dir = TempDir::new(tag);
        let db = dir.path().join("fixture.db");
        bulk_core::testdata::sqlite_fixture(&db, 1, 1, 1).expect("build sqlite fixture");
        let config = dir.path().join("config");
        fs::create_dir_all(&config).expect("create config dir");
        Fixture { dir, db, config }
    }

    fn out(&self, name: &str) -> String {
        self.dir.path().join(name).to_string_lossy().into_owned()
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_bulk-cli"))
            .env("BULKDB_CONFIG_DIR", &self.config)
            .env("BULKDB_CREDENTIALS", "memory")
            .env_remove("RUST_LOG")
            .stdin(Stdio::null())
            .args(args)
            .output()
            .expect("spawn bulk-cli")
    }

    /// `[--json] export --driver sqlite --path <db> <args>`.
    fn export(&self, json: bool, args: &[&str]) -> Output {
        let db = self.db.to_str().expect("utf-8 temp path");
        let mut argv = if json { vec!["--json"] } else { vec![] };
        argv.extend_from_slice(&["export", "--driver", "sqlite", "--path", db]);
        argv.extend_from_slice(args);
        self.run(&argv)
    }
}

fn describe(o: &Output) -> String {
    format!("exit {:?}\n--- stdout\n{}\n--- stderr\n{}", o.status.code(), String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr))
}

/// Decode a CP949 file the way BulkDB decodes server text; fails on any invalid byte.
fn decode_cp949(bytes: &[u8]) -> String {
    let mut scratch = String::new();
    let d = decode_cell(Charset::EucKr, bytes, &mut scratch);
    assert!(!d.replaced, "the file is valid CP949");
    d.text.to_string()
}

#[test]
fn export_sqlite_korean_text_to_an_euc_kr_csv() {
    let fx = Fixture::new("euckr");
    // Re-read CSV outputs after the run, so the CP949 file is recounted too.
    let o = fx.run(&["settings", "set", "verify_after_export=true"]);
    assert_eq!(o.status.code(), Some(0), "{}", describe(&o));

    let out = fx.out("korean.csv");
    let o = fx.export(true, &["--query", QUERY, "--out", &out, "--encoding", "euc-kr", "--lf", "--yes"]);
    assert_eq!(o.status.code(), Some(0), "{}", describe(&o));
    let bytes = fs::read(&out).expect("output file");

    // Byte level: CP949, no BOM, no UTF-8.
    assert!(bytes.starts_with(b"id,"), "no BOM: {:02X?}", &bytes[..bytes.len().min(8)]);
    assert!(std::str::from_utf8(&bytes).is_err(), "the file is not UTF-8");
    let mut row1 = b"1,".to_vec();
    row1.extend_from_slice(HANGUL_CP949);
    row1.extend_from_slice(b",x\n");
    assert!(bytes.windows(row1.len()).any(|w| w == row1.as_slice()), "row 1 is `1,` + C7D1B1DB + `,x`");
    assert!(bytes.ends_with(b"\n3,emoji ?,\n"), "the emoji is written as '?'");
    assert!(!bytes.windows(4).any(|w| w == "🚀".as_bytes()));
    // Whole file: decodes to exactly the expected text.
    assert_eq!(decode_cp949(&bytes), EXPECTED_TEXT);

    // The result reports the replaced character and the verified row count.
    let v: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap_or_else(|e| panic!("stdout is not JSON ({e}): {}", describe(&o)));
    assert_eq!(v["state"], "done", "{v}");
    assert_eq!(v["result"]["rows"], 3, "{v}");
    assert_eq!(v["result"]["bytes"], bytes.len() as u64, "{v}");
    assert_eq!(v["result"]["verifiedRows"], 3, "CP949 files recount correctly: {v}");
    let notes: Vec<String> = v["result"]["notes"].as_array().map(|a| a.iter().filter_map(|n| n.as_str().map(String::from)).collect()).unwrap_or_default();
    assert!(notes.iter().any(|n| n.starts_with("1 character(s)") && n.contains("EUC-KR (CP949)")), "{notes:?}");

    // The default (and an explicit utf-8) stays UTF-8 and lossless.
    let out8 = fx.out("korean_utf8.csv");
    let o = fx.export(false, &["--query", QUERY, "--out", &out8, "--encoding", "utf8", "--lf", "--yes"]);
    assert_eq!(o.status.code(), Some(0), "{}", describe(&o));
    let text = fs::read_to_string(&out8).expect("utf-8 output");
    assert_eq!(text, EXPECTED_TEXT.replace("emoji ?", "emoji 🚀"));
}

#[test]
fn excel_preset_with_euc_kr_writes_no_bom_and_bom_flag_is_rejected() {
    let fx = Fixture::new("excel");
    let out = fx.out("excel.csv");
    let o = fx.export(false, &["--query", QUERY, "--out", &out, "--encoding", "cp949", "--excel", "--yes"]);
    assert_eq!(o.status.code(), Some(0), "{}", describe(&o));
    // --excel splits every 1,000,000 rows: one part here.
    let files: Vec<PathBuf> = fs::read_dir(fx.dir.path())
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("excel")))
        .collect();
    assert_eq!(files.len(), 1, "{files:?}");
    let bytes = fs::read(&files[0]).unwrap();
    assert!(bytes.starts_with(b"id,"), "no BOM for EUC-KR under --excel");
    assert_eq!(decode_cp949(&bytes), EXPECTED_TEXT.replace('\n', "\r\n"), "CRLF from the preset");

    let bad = fx.out("bad.csv");
    let o = fx.export(false, &["--query", QUERY, "--out", &bad, "--encoding", "euc-kr", "--bom", "--yes"]);
    assert_eq!(o.status.code(), Some(1), "{}", describe(&o));
    let err = String::from_utf8_lossy(&o.stderr);
    assert!(err.contains("--bom") && err.contains("euc-kr"), "{err}");
    assert!(!Path::new(&bad).exists(), "nothing written for a usage error");

    // Parquet is always UTF-8.
    let pq = fx.out("x.parquet");
    let o = fx.export(false, &["--query", QUERY, "--out", &pq, "--encoding", "euc-kr", "--yes"]);
    assert_eq!(o.status.code(), Some(1), "{}", describe(&o));
}
