//! Sink stack tests: CSV (Arrow and raw paths), split, compression, Parquet, abort, empty
//! results and the overwrite guard. Everything runs on the filesystem via `tempfile`.

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use arrow_array::{Array, RecordBatch};
use bulk_core::sink::compress::CompressWriter;
use bulk_core::sink::csv::CsvSink;
use bulk_core::sink::parquet::ParquetSink;
use bulk_core::sink::split::{raw_csv_row_boundary, SplitSink};
use bulk_core::sink::{self, Sink, SinkTarget};
use bulk_core::source::arrow_batch::{schema_from, ArrowBatcher, Cell, ColKind};
use bulk_core::source::Batch;
use bulk_core::testdata::{self, Lcg, NASTY_COLUMNS};
use bulk_core::types::{Compression, CsvOpts, OutputFormat, OutputOpts, ParquetCompression, ParquetOpts, SplitOpts};
use bytes::Bytes;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;

// ---------------------------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------------------------

const NASTY_KINDS: [ColKind; 6] = [ColKind::Int64, ColKind::Utf8, ColKind::Utf8, ColKind::Utf8, ColKind::Utf8, ColKind::Utf8];

/// Deterministic nasty rows (what the sink should reproduce).
fn nasty_rows(n: u64) -> Vec<Vec<Option<String>>> {
    let mut rng = Lcg::new(7);
    (0..n).map(|i| testdata::nasty_row(i, &mut rng)).collect()
}

/// Arrow batches of at most `per_batch` rows built from `rows` through `ArrowBatcher`.
fn nasty_batches(rows: &[Vec<Option<String>>], per_batch: usize) -> Vec<RecordBatch> {
    let cols: Vec<(String, ColKind)> = NASTY_COLUMNS.iter().zip(NASTY_KINDS).map(|(n, k)| (n.to_string(), k)).collect();
    let mut b = ArrowBatcher::with_limits(schema_from(&cols), NASTY_KINDS.to_vec(), per_batch, usize::MAX);
    let mut out = Vec::new();
    for row in rows {
        for cell in row {
            b.push(match cell {
                Some(s) => Cell::Str(s),
                None => Cell::Null,
            })
            .unwrap();
        }
        if b.end_row().unwrap() {
            out.push(b.flush().unwrap().unwrap());
        }
    }
    if let Some(rb) = b.flush().unwrap() {
        out.push(rb);
    }
    out
}

fn target(dir: &Path, stem: &str, opts: OutputOpts) -> SinkTarget {
    SinkTarget { dir: dir.to_path_buf(), stem: stem.to_string(), opts }
}

fn csv_opts(header: bool, delimiter: &str, null_as: &str, bom: bool, crlf: bool) -> CsvOpts {
    CsvOpts { header, delimiter: delimiter.into(), null_as: null_as.into(), bom, crlf }
}

fn output(csv: CsvOpts) -> OutputOpts {
    OutputOpts { csv: Some(csv), ..OutputOpts::default() }
}

/// Parse a CSV file back with the csv crate (flexible, quoting on). Returns (header, rows).
fn read_csv(path: &Path, delimiter: u8) -> (Vec<String>, Vec<Vec<String>>) {
    let bytes = std::fs::read(path).unwrap();
    read_csv_bytes(&bytes, delimiter)
}

fn read_csv_bytes(bytes: &[u8], delimiter: u8) -> (Vec<String>, Vec<Vec<String>>) {
    let mut rdr = csv::ReaderBuilder::new().delimiter(delimiter).has_headers(true).flexible(true).from_reader(bytes);
    let header: Vec<String> = rdr.headers().unwrap().iter().map(|s| s.to_string()).collect();
    let rows: Vec<Vec<String>> = rdr.records().map(|r| r.unwrap().iter().map(|s| s.to_string()).collect()).collect();
    (header, rows)
}

fn expect(cell: &Option<String>, null_as: &str) -> String {
    cell.clone().unwrap_or_else(|| null_as.to_string())
}

fn assert_rows_match(rows: &[Vec<Option<String>>], got: &[Vec<String>], null_as: &str) {
    assert_eq!(got.len(), rows.len(), "row count");
    for (i, (want, have)) in rows.iter().zip(got).enumerate() {
        assert_eq!(have.len(), 6, "row {i} column count");
        for (c, w) in want.iter().enumerate() {
            assert_eq!(have[c], expect(w, null_as), "row {i} col {c}");
        }
    }
}

/// Hand-built raw CSV (as PostgreSQL COPY would send it): header + rows with nasty values.
fn raw_csv_rows(n: u64) -> (Bytes, Vec<Bytes>, Vec<String>) {
    let header = Bytes::from_static(b"id,text,note\n");
    let mut lines = Vec::new();
    for i in 0..n {
        let line = match i % 4 {
            0 => format!("{i},plain,note {i}\n"),
            1 => format!("{i},\"has,comma\",\n"),
            2 => format!("{i},\"line1\nline2\",\"q\"\"uote\"\n"),
            _ => format!("{i},\"crlf\r\nline\",x\n"),
        };
        lines.push(line);
    }
    let data: Vec<Bytes> = lines.iter().map(|l| Bytes::from(l.clone())).collect();
    (header, data, lines)
}

fn file_rows(path: &Path) -> usize {
    read_csv(path, b',').1.len()
}

// ---------------------------------------------------------------------------------------------
// (a) nasty round trip on the Arrow path
// ---------------------------------------------------------------------------------------------

#[test]
fn arrow_nasty_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let rows = nasty_rows(500);
    let batches = nasty_batches(&rows, 64);
    assert!(batches.len() > 1);

    let t = target(dir.path(), "nasty", output(csv_opts(true, ",", "", false, false)));
    let mut sink = sink::build(&t).unwrap();
    assert_eq!(sink.files_so_far(), 0, "files are created lazily");
    for rb in batches {
        sink.write(Batch::Arrow(rb)).unwrap();
    }
    assert_eq!(sink.files_so_far(), 1);
    let rep = sink.finish().unwrap();
    assert_eq!(rep.rows, 500);
    assert_eq!(rep.files, vec![dir.path().join("nasty.csv")]);
    assert_eq!(rep.bytes, std::fs::metadata(&rep.files[0]).unwrap().len());

    let (header, got) = read_csv(&rep.files[0], b',');
    assert_eq!(header, NASTY_COLUMNS.iter().map(|s| s.to_string()).collect::<Vec<_>>());
    assert_rows_match(&rows, &got, "");

    // Every nasty shape actually occurred and survived.
    let texts: Vec<&str> = got.iter().map(|r| r[1].as_str()).collect();
    for needle in ["has,comma", "has \"quote\" inside", "line1\nline2", "crlf\r\nline", " leading and trailing "] {
        assert!(texts.contains(&needle), "missing {needle:?}");
    }
    let unicode: Vec<&str> = got.iter().map(|r| r[5].as_str()).collect();
    for needle in ["한글 텍스트", "emoji 🚀🎉", "日本語, 中文", "ünïcödé"] {
        assert!(unicode.contains(&needle), "missing {needle:?}");
    }
    // The file is RFC 4180: a quoted field with a doubled quote.
    let raw = std::fs::read_to_string(&rep.files[0]).unwrap();
    assert!(raw.contains("\"has \"\"quote\"\" inside\""));
    assert!(raw.contains("\"has,comma\""));
    assert!(!raw.starts_with('\u{feff}'));
}

// ---------------------------------------------------------------------------------------------
// (b) RawCsv pass-through
// ---------------------------------------------------------------------------------------------

#[test]
fn raw_csv_passes_through_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let (header, data, lines) = raw_csv_rows(9);
    let t = target(dir.path(), "raw", output(csv_opts(true, ",", "", false, false)));
    let mut sink = sink::build(&t).unwrap();
    sink.write(Batch::RawCsvHeader(header.clone())).unwrap();
    // Two batches: 5 rows then 4 rows.
    let b1: Vec<u8> = data[..5].iter().flat_map(|b| b.iter().copied()).collect();
    let b2: Vec<u8> = data[5..].iter().flat_map(|b| b.iter().copied()).collect();
    sink.write(Batch::RawCsv { data: Bytes::from(b1), rows: 5 }).unwrap();
    sink.write(Batch::RawCsv { data: Bytes::from(b2), rows: 4 }).unwrap();
    let rep = sink.finish().unwrap();
    assert_eq!(rep.rows, 9);

    let mut want = header.to_vec();
    for l in &lines {
        want.extend_from_slice(l.as_bytes());
    }
    let got = std::fs::read(&rep.files[0]).unwrap();
    assert_eq!(got, want, "bytes must be untouched");
    assert_eq!(rep.bytes, want.len() as u64);
}

// ---------------------------------------------------------------------------------------------
// (c) crlf + bom + custom delimiter / null text
// ---------------------------------------------------------------------------------------------

#[test]
fn crlf_bom_delimiter_null_as() {
    let dir = tempfile::tempdir().unwrap();
    let rows = nasty_rows(120);
    let t = target(dir.path(), "excel", output(csv_opts(true, ";", "NULL", true, true)));
    let mut sink = sink::build(&t).unwrap();
    for rb in nasty_batches(&rows, 50) {
        sink.write(Batch::Arrow(rb)).unwrap();
    }
    let rep = sink.finish().unwrap();
    let bytes = std::fs::read(&rep.files[0]).unwrap();
    assert_eq!(&bytes[..3], b"\xEF\xBB\xBF", "BOM first");
    let text = std::str::from_utf8(&bytes[3..]).unwrap();
    assert!(text.starts_with("id;text;num;note;flag;unicode\r\n"));
    // Every record terminator is CRLF; the only bare `\n` are inside quoted values.
    let mut in_quotes = false;
    let b = text.as_bytes();
    for (i, &c) in b.iter().enumerate() {
        match c {
            b'"' => in_quotes = !in_quotes,
            b'\n' if !in_quotes => assert_eq!(b[i - 1], b'\r', "bare LF terminator at {i}"),
            _ => {}
        }
    }
    let (header, got) = read_csv_bytes(&bytes, b';');
    assert_eq!(header[0], "id");
    assert_rows_match(&rows, &got, "NULL");
    assert!(got.iter().any(|r| r[5] == "NULL"), "NULL text used for unicode NULLs");
    assert!(got.iter().any(|r| r[1].is_empty()), "empty strings stay empty, distinct from NULL");
}

#[test]
fn tsv_extension_and_tab_delimiter() {
    let dir = tempfile::tempdir().unwrap();
    let rows = nasty_rows(10);
    let t = target(dir.path(), "t", output(csv_opts(true, "\t", "", false, false)));
    let mut sink = sink::build(&t).unwrap();
    for rb in nasty_batches(&rows, 10) {
        sink.write(Batch::Arrow(rb)).unwrap();
    }
    let rep = sink.finish().unwrap();
    assert_eq!(rep.files, vec![dir.path().join("t.tsv")]);
    let (_, got) = read_csv(&rep.files[0], b'\t');
    assert_rows_match(&rows, &got, "");
}

#[test]
fn bad_delimiter_is_rejected_at_build() {
    let dir = tempfile::tempdir().unwrap();
    let t = target(dir.path(), "bad", output(csv_opts(true, "│", "", false, false)));
    let err = sink::build(&t).err().expect("a non-ASCII delimiter must be rejected");
    assert_eq!(err.code(), "INVALID");
}

// ---------------------------------------------------------------------------------------------
// (d) split by rows: Arrow and RawCsv
// ---------------------------------------------------------------------------------------------

fn split_rows_opts(rows: u64, bom: bool) -> OutputOpts {
    OutputOpts { csv: Some(csv_opts(true, ",", "", bom, false)), split: Some(SplitOpts { rows: Some(rows), megabytes: None }), ..OutputOpts::default() }
}

#[test]
fn split_by_rows_arrow_path() {
    let dir = tempfile::tempdir().unwrap();
    let rows = nasty_rows(10);
    let t = target(dir.path(), "s", split_rows_opts(4, false));
    let mut sink = sink::build(&t).unwrap();
    // Batches of 3 so pieces straddle file boundaries (3,3,3,1 -> files 4,4,2).
    for rb in nasty_batches(&rows, 3) {
        sink.write(Batch::Arrow(rb)).unwrap();
    }
    assert_eq!(sink.files_so_far(), 3);
    let rep = sink.finish().unwrap();
    assert_eq!(rep.rows, 10);
    assert_eq!(rep.files, vec![dir.path().join("s_part0001.csv"), dir.path().join("s_part0002.csv"), dir.path().join("s_part0003.csv")]);
    let counts: Vec<usize> = rep.files.iter().map(|p| file_rows(p)).collect();
    assert_eq!(counts, vec![4, 4, 2]);
    let mut all = Vec::new();
    for p in &rep.files {
        let (header, got) = read_csv(p, b',');
        assert_eq!(header[0], "id", "header repeated in {}", p.display());
        all.extend(got);
    }
    assert_rows_match(&rows, &all, "");
    assert_eq!(rep.bytes, rep.files.iter().map(|p| std::fs::metadata(p).unwrap().len()).sum::<u64>());
}

#[test]
fn split_by_rows_raw_csv_path() {
    let dir = tempfile::tempdir().unwrap();
    let (header, data, lines) = raw_csv_rows(10);
    let t = target(dir.path(), "r", split_rows_opts(4, true));
    let mut sink = sink::build(&t).unwrap();
    sink.write(Batch::RawCsvHeader(header.clone())).unwrap();
    // One 7-row batch and one 3-row batch; the first must be cut 4 | 3 at a row boundary.
    let b1: Vec<u8> = data[..7].iter().flat_map(|b| b.iter().copied()).collect();
    let b2: Vec<u8> = data[7..].iter().flat_map(|b| b.iter().copied()).collect();
    sink.write(Batch::RawCsv { data: Bytes::from(b1), rows: 7 }).unwrap();
    sink.write(Batch::RawCsv { data: Bytes::from(b2), rows: 3 }).unwrap();
    let rep = sink.finish().unwrap();
    assert_eq!(rep.rows, 10);
    assert_eq!(rep.files.len(), 3);
    for (i, p) in rep.files.iter().enumerate() {
        let bytes = std::fs::read(p).unwrap();
        assert_eq!(&bytes[..3], b"\xEF\xBB\xBF", "BOM in part {}", i + 1);
        assert!(bytes[3..].starts_with(&header), "header in part {}", i + 1);
        let body = &bytes[3 + header.len()..];
        let want: String = lines[i * 4..((i + 1) * 4).min(10)].concat();
        assert_eq!(body, want.as_bytes(), "part {} rows", i + 1);
    }
    let counts: Vec<usize> = rep.files.iter().map(|p| file_rows(p)).collect();
    assert_eq!(counts, vec![4, 4, 2]);
}

#[test]
fn split_exact_multiple_has_no_empty_trailing_file() {
    let dir = tempfile::tempdir().unwrap();
    let rows = nasty_rows(8);
    let t = target(dir.path(), "m", split_rows_opts(4, false));
    let mut sink = sink::build(&t).unwrap();
    for rb in nasty_batches(&rows, 8) {
        sink.write(Batch::Arrow(rb)).unwrap();
    }
    let rep = sink.finish().unwrap();
    assert_eq!(rep.files.len(), 2);
    assert_eq!(rep.files.iter().map(|p| file_rows(p)).collect::<Vec<_>>(), vec![4, 4]);
}

#[test]
fn row_boundary_scanner_handles_quoted_newlines() {
    let (_, data, lines) = raw_csv_rows(8);
    let all: Vec<u8> = data.iter().flat_map(|b| b.iter().copied()).collect();
    for n in 1..=8u64 {
        let want: String = lines[..n as usize].concat();
        assert_eq!(&all[..raw_csv_row_boundary(&all, n)], want.as_bytes(), "n = {n}");
    }
    assert_eq!(raw_csv_row_boundary(&all, 9), all.len());
}

// ---------------------------------------------------------------------------------------------
// (e) split by megabytes
// ---------------------------------------------------------------------------------------------

#[test]
fn split_by_megabytes_rolls_over() {
    let dir = tempfile::tempdir().unwrap();
    let opts = OutputOpts { csv: Some(csv_opts(true, ",", "", false, false)), split: Some(SplitOpts { rows: None, megabytes: Some(1) }), ..OutputOpts::default() };
    let t = target(dir.path(), "mb", opts);
    let mut sink = sink::build(&t).unwrap();
    // ~512 KB per batch: 7 batches -> ~3.5 MB -> at least 3 files, each <= 1 MB + one batch.
    let (header, _, _) = raw_csv_rows(1);
    sink.write(Batch::RawCsvHeader(header)).unwrap();
    let line = format!("{},{},{}\n", 1, "x".repeat(500), "y".repeat(500));
    let rows_per_batch = 512 * 1024 / line.len();
    let batch: String = line.repeat(rows_per_batch);
    for _ in 0..7 {
        sink.write(Batch::RawCsv { data: Bytes::from(batch.clone()), rows: rows_per_batch as u64 }).unwrap();
    }
    let rep = sink.finish().unwrap();
    assert_eq!(rep.rows, 7 * rows_per_batch as u64);
    assert!(rep.files.len() >= 3, "got {} files", rep.files.len());
    for p in &rep.files {
        let len = std::fs::metadata(p).unwrap().len();
        assert!(len <= 1024 * 1024 + batch.len() as u64 + 64, "{} is {len} bytes", p.display());
        assert!(std::fs::read(p).unwrap().starts_with(b"id,text,note\n"));
    }
    let total: usize = rep.files.iter().map(|p| file_rows(p)).sum();
    assert_eq!(total, 7 * rows_per_batch);
}

// ---------------------------------------------------------------------------------------------
// (f) gzip / zstd decompress to the plain output
// ---------------------------------------------------------------------------------------------

fn write_nasty_with(dir: &Path, stem: &str, compress: Option<Compression>, rows: &[Vec<Option<String>>]) -> (PathBuf, u64) {
    let opts = OutputOpts { csv: Some(csv_opts(true, ",", "\\N", false, false)), compress, ..OutputOpts::default() };
    let t = target(dir, stem, opts);
    let mut sink = sink::build(&t).unwrap();
    for rb in nasty_batches(rows, 1000) {
        sink.write(Batch::Arrow(rb)).unwrap();
    }
    let rep = sink.finish().unwrap();
    assert_eq!(rep.files.len(), 1);
    (rep.files[0].clone(), rep.bytes)
}

#[test]
fn gzip_and_zstd_match_plain_output() {
    let dir = tempfile::tempdir().unwrap();
    let rows = nasty_rows(50_000);
    let (plain_path, plain_bytes) = write_nasty_with(dir.path(), "plain", None, &rows);
    let plain = std::fs::read(&plain_path).unwrap();
    assert_eq!(plain_bytes, plain.len() as u64);
    assert!(plain.len() > 2 * 1024 * 1024, "enough data to cross the 1 MB chunk size");

    let (gz_path, gz_bytes) = write_nasty_with(dir.path(), "gz", Some(Compression::Gzip), &rows);
    assert_eq!(gz_path, dir.path().join("gz.csv.gz"));
    assert_eq!(gz_bytes, std::fs::metadata(&gz_path).unwrap().len(), "reported bytes are the compressed size");
    assert!(gz_bytes < plain_bytes);
    let mut back = Vec::new();
    flate2::read::GzDecoder::new(File::open(&gz_path).unwrap()).read_to_end(&mut back).unwrap();
    assert_eq!(back, plain);

    let (zst_path, zst_bytes) = write_nasty_with(dir.path(), "zst", Some(Compression::Zstd), &rows);
    assert_eq!(zst_path, dir.path().join("zst.csv.zst"));
    assert_eq!(zst_bytes, std::fs::metadata(&zst_path).unwrap().len());
    assert!(zst_bytes < plain_bytes);
    let back = zstd::stream::decode_all(File::open(&zst_path).unwrap()).unwrap();
    assert_eq!(back, plain);
}

#[test]
fn compressed_split_repeats_header_per_part() {
    let dir = tempfile::tempdir().unwrap();
    let rows = nasty_rows(10);
    let opts = OutputOpts {
        csv: Some(csv_opts(true, ",", "", true, false)),
        compress: Some(Compression::Gzip),
        split: Some(SplitOpts { rows: Some(4), megabytes: None }),
        ..OutputOpts::default()
    };
    let t = target(dir.path(), "gs", opts);
    let mut sink = sink::build(&t).unwrap();
    for rb in nasty_batches(&rows, 10) {
        sink.write(Batch::Arrow(rb)).unwrap();
    }
    let rep = sink.finish().unwrap();
    assert_eq!(rep.files.len(), 3);
    let mut all = Vec::new();
    for p in &rep.files {
        assert!(p.to_string_lossy().ends_with(".csv.gz"));
        let mut back = Vec::new();
        flate2::read::GzDecoder::new(File::open(p).unwrap()).read_to_end(&mut back).unwrap();
        assert_eq!(&back[..3], b"\xEF\xBB\xBF");
        let (header, got) = read_csv_bytes(&back, b',');
        assert_eq!(header[0], "id");
        all.extend(got);
    }
    assert_rows_match(&rows, &all, "");
}

// ---------------------------------------------------------------------------------------------
// (g) parquet
// ---------------------------------------------------------------------------------------------

fn parquet_opts(compression: ParquetCompression, row_group_mb: u32) -> OutputOpts {
    OutputOpts { format: OutputFormat::Parquet, csv: None, parquet: Some(ParquetOpts { compression, row_group_mb }), ..OutputOpts::default() }
}

#[test]
fn parquet_round_trip_zstd() {
    let dir = tempfile::tempdir().unwrap();
    let rows = nasty_rows(2000);
    let t = target(dir.path(), "p", parquet_opts(ParquetCompression::Zstd, 128));
    let mut sink = sink::build(&t).unwrap();
    for rb in nasty_batches(&rows, 300) {
        sink.write(Batch::Arrow(rb)).unwrap();
    }
    let rep = sink.finish().unwrap();
    assert_eq!(rep.rows, 2000);
    assert_eq!(rep.files, vec![dir.path().join("p.parquet")]);
    assert_eq!(rep.bytes, std::fs::metadata(&rep.files[0]).unwrap().len());

    let builder = ParquetRecordBatchReaderBuilder::try_new(File::open(&rep.files[0]).unwrap()).unwrap();
    let meta = builder.metadata().clone();
    assert_eq!(meta.file_metadata().num_rows(), 2000);
    assert_eq!(meta.file_metadata().created_by(), Some("BulkDB"));
    assert_eq!(meta.num_row_groups(), 1, "2000 small rows fit one row group");
    assert!(matches!(meta.row_group(0).column(0).compression(), parquet::basic::Compression::ZSTD(_)));
    assert_eq!(builder.schema().fields().len(), 6);
    assert_eq!(builder.schema().field(0).name(), "id");

    let mut total = 0usize;
    let mut first_rows: Vec<(i64, Option<String>, Option<String>)> = Vec::new();
    for rb in builder.with_batch_size(512).build().unwrap() {
        let rb = rb.unwrap();
        let ids = rb.column(0).as_any().downcast_ref::<arrow_array::Int64Array>().unwrap();
        let texts = rb.column(1).as_any().downcast_ref::<arrow_array::StringArray>().unwrap();
        let unicode = rb.column(5).as_any().downcast_ref::<arrow_array::StringArray>().unwrap();
        for i in 0..rb.num_rows() {
            if first_rows.len() < 50 {
                let text = if texts.is_null(i) { None } else { Some(texts.value(i).to_string()) };
                let uni = if unicode.is_null(i) { None } else { Some(unicode.value(i).to_string()) };
                first_rows.push((ids.value(i), text, uni));
            }
        }
        total += rb.num_rows();
    }
    assert_eq!(total, 2000);
    for (i, (id, text, uni)) in first_rows.iter().enumerate() {
        assert_eq!(*id, i as i64);
        assert_eq!(*text, rows[i][1]);
        assert_eq!(*uni, rows[i][5]);
    }
}

/// High-entropy batches (hex noise) that dictionary encoding cannot shrink, so row-group
/// sizes are predictable: ~40 bytes per row before compression.
fn noisy_batches(rows: usize, per_batch: usize) -> Vec<RecordBatch> {
    let cols = vec![("id".to_string(), ColKind::Int64), ("a".to_string(), ColKind::Utf8), ("b".to_string(), ColKind::Utf8)];
    let mut b = ArrowBatcher::with_limits(schema_from(&cols), vec![ColKind::Int64, ColKind::Utf8, ColKind::Utf8], per_batch, usize::MAX);
    let mut rng = Lcg::new(99);
    let mut out = Vec::new();
    for i in 0..rows {
        b.push(Cell::I64(i as i64)).unwrap();
        let a = format!("{:016x}", rng.next_u64());
        let c = format!("{:016x}", rng.next_u64());
        b.push(Cell::Str(&a)).unwrap();
        b.push(Cell::Str(&c)).unwrap();
        if b.end_row().unwrap() {
            out.push(b.flush().unwrap().unwrap());
        }
    }
    if let Some(rb) = b.flush().unwrap() {
        out.push(rb);
    }
    out
}

#[test]
fn parquet_snappy_and_none_codecs_and_row_groups() {
    let dir = tempfile::tempdir().unwrap();
    let batches = noisy_batches(120_000, 10_000);
    for (name, comp, want) in [("snappy", ParquetCompression::Snappy, "SNAPPY"), ("none", ParquetCompression::None, "UNCOMPRESSED")] {
        // 1 MB row groups over ~4.8 MB of noise -> several row groups.
        let t = target(dir.path(), name, parquet_opts(comp, 1));
        let mut sink = sink::build(&t).unwrap();
        for rb in &batches {
            sink.write(Batch::Arrow(rb.clone())).unwrap();
        }
        let rep = sink.finish().unwrap();
        let builder = ParquetRecordBatchReaderBuilder::try_new(File::open(&rep.files[0]).unwrap()).unwrap();
        let meta = builder.metadata();
        assert_eq!(meta.file_metadata().num_rows(), 120_000);
        assert!(meta.num_row_groups() > 1, "{name}: expected several row groups, got {}", meta.num_row_groups());
        let codec = meta.row_group(0).column(1).compression().to_string();
        assert_eq!(codec, want, "{name}");
    }
}

#[test]
fn parquet_rejects_raw_csv() {
    let dir = tempfile::tempdir().unwrap();
    let t = target(dir.path(), "bad", parquet_opts(ParquetCompression::Zstd, 128));
    let mut sink = sink::build(&t).unwrap();
    let err = sink.write(Batch::RawCsv { data: Bytes::from_static(b"1,2\n"), rows: 1 }).unwrap_err();
    assert_eq!(err.code(), "INVALID");
}

#[test]
fn parquet_split_by_rows() {
    let dir = tempfile::tempdir().unwrap();
    let rows = nasty_rows(10);
    let opts = OutputOpts { split: Some(SplitOpts { rows: Some(4), megabytes: None }), ..parquet_opts(ParquetCompression::Snappy, 128) };
    let t = target(dir.path(), "ps", opts);
    let mut sink = sink::build(&t).unwrap();
    for rb in nasty_batches(&rows, 10) {
        sink.write(Batch::Arrow(rb)).unwrap();
    }
    let rep = sink.finish().unwrap();
    assert_eq!(
        rep.files.iter().map(|p| p.file_name().unwrap().to_string_lossy().to_string()).collect::<Vec<_>>(),
        vec!["ps_part0001.parquet", "ps_part0002.parquet", "ps_part0003.parquet"]
    );
    let counts: Vec<i64> =
        rep.files.iter().map(|p| ParquetRecordBatchReaderBuilder::try_new(File::open(p).unwrap()).unwrap().metadata().file_metadata().num_rows()).collect();
    assert_eq!(counts, vec![4, 4, 2]);
}

const MB: u64 = 1024 * 1024;

/// Parquet split by MB with a row-group size (128 MB) far above the split size: parts must
/// close near 1 MB (not after a whole row group), row groups are capped at the split size, the
/// progress counter never moves backwards, and every row lands in exactly one part, in order.
#[test]
fn parquet_split_by_megabytes_honours_the_limit() {
    let dir = tempfile::tempdir().unwrap();
    // ~48 encoded bytes per row of incompressible noise: ~96 KB per batch, ~4.8 MB in total.
    let total_rows = 100_000usize;
    let batches = noisy_batches(total_rows, 2_000);
    let opts = OutputOpts { split: Some(SplitOpts { rows: None, megabytes: Some(1) }), ..parquet_opts(ParquetCompression::None, 128) };
    let t = target(dir.path(), "pmb", opts);
    let mut sink = sink::build(&t).unwrap();
    let mut last = 0u64;
    let mut grew_in_first_part = 0;
    for rb in &batches {
        sink.write(Batch::Arrow(rb.clone())).unwrap();
        let now = sink.bytes_out();
        assert!(now >= last, "bytes_out went backwards: {last} -> {now}");
        if sink.files_so_far() == 1 && now > last {
            grew_in_first_part += 1;
        }
        last = now;
    }
    // Before the fix the counter only moved when a 128 MB row group was flushed, i.e. never.
    assert!(grew_in_first_part >= 5, "bytes_out grew on only {grew_in_first_part} batches of the first part");
    let rep = sink.finish().unwrap();
    assert_eq!(rep.rows, total_rows as u64);
    assert!(rep.files.len() >= 4, "expected several ~1 MB parts, got {}", rep.files.len());

    let sizes: Vec<u64> = rep.files.iter().map(|p| std::fs::metadata(p).unwrap().len()).collect();
    assert_eq!(rep.bytes, sizes.iter().sum::<u64>());
    let per_batch = rep.bytes / batches.len() as u64;
    let mut next_id = 0i64;
    for (i, (path, &len)) in rep.files.iter().zip(&sizes).enumerate() {
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        assert_eq!(name, format!("pmb_part{:04}.parquet", i + 1));
        // Rolls over at the first batch boundary at or past 1 MB: one batch (+ footer) over.
        assert!(len <= MB + per_batch + 64 * 1024, "{name} is {len} bytes (batch ~{per_batch})");
        if i + 1 < rep.files.len() {
            assert!(len >= MB - MB / 8, "{name} closed early at {len} bytes");
        }
        let builder = ParquetRecordBatchReaderBuilder::try_new(File::open(path).unwrap()).unwrap();
        for rg in builder.metadata().row_groups() {
            assert!(rg.compressed_size() as u64 <= MB + per_batch, "{name}: row group of {} bytes", rg.compressed_size());
        }
        for rb in builder.build().unwrap() {
            let rb = rb.unwrap();
            let ids = rb.column(0).as_any().downcast_ref::<arrow_array::Int64Array>().unwrap();
            for v in ids.values().iter() {
                assert_eq!(*v, next_id, "{name}: rows out of order");
                next_id += 1;
            }
        }
    }
    assert_eq!(next_id as usize, total_rows);
}

/// The Parquet progress counter includes the row group still in memory, so it moves with
/// every batch even when no row group is flushed before `finish`.
#[test]
fn parquet_bytes_out_tracks_the_buffered_row_group() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("progress.parquet");
    let mut sink = Box::new(ParquetSink::new(path.clone(), ParquetOpts { compression: ParquetCompression::None, row_group_mb: 128 }));
    assert_eq!(sink.row_group_bytes(), 128 * MB as usize);
    let mut last = sink.bytes_out();
    for rb in noisy_batches(20_000, 2_000) {
        sink.write(Batch::Arrow(rb)).unwrap();
        let now = sink.bytes_out();
        assert!(now > last, "bytes_out did not grow: {last} -> {now}");
        last = now;
    }
    let rep = sink.finish().unwrap();
    let meta = ParquetRecordBatchReaderBuilder::try_new(File::open(&path).unwrap()).unwrap().metadata().clone();
    assert_eq!(meta.num_row_groups(), 1, "nothing may be flushed before finish");
    // The estimate is close to the real size (uncompressed noise: footer and page headers only).
    let diff = rep.bytes.abs_diff(last);
    assert!(diff <= rep.bytes / 10, "estimate {last} vs on-disk {}", rep.bytes);
}

/// Stream `rows` wide rows (compressible text, ~200 bytes per row) into `sink` in batches of
/// `per_batch`, asserting after every batch that `bytes_out` never decreases. Returns the
/// number of batches written.
fn write_wide(sink: &mut dyn Sink, rows: u64, per_batch: usize) -> usize {
    fn send(sink: &mut dyn Sink, rb: RecordBatch, last: &mut u64, batches: &mut usize) {
        sink.write(Batch::Arrow(rb)).unwrap();
        let now = sink.bytes_out();
        assert!(now >= *last, "bytes_out went backwards after batch {}: {} -> {now}", *batches, *last);
        *last = now;
        *batches += 1;
    }
    let cols: Vec<(String, ColKind)> = testdata::WIDE_COLUMNS.iter().map(|c| (c.to_string(), ColKind::Utf8)).collect();
    let mut b = ArrowBatcher::with_limits(schema_from(&cols), vec![ColKind::Utf8; cols.len()], per_batch, usize::MAX);
    let mut rng = Lcg::new(7);
    let (mut last, mut batches) = (0u64, 0usize);
    for i in 0..rows {
        for v in testdata::wide_row(i, &mut rng) {
            b.push(Cell::Str(&v)).unwrap();
        }
        if b.end_row().unwrap() {
            send(sink, b.flush().unwrap().unwrap(), &mut last, &mut batches);
        }
    }
    if let Some(rb) = b.flush().unwrap() {
        send(sink, rb, &mut last, &mut batches);
    }
    batches
}

/// With ZSTD on compressible data the writer's estimate of open pages is far above what they
/// take on disk; the learned compression ratio keeps parts near the MB limit instead of
/// closing them at a fraction of it.
#[test]
fn parquet_split_by_megabytes_with_zstd_lands_near_the_limit() {
    let dir = tempfile::tempdir().unwrap();
    let limit = 2 * MB;
    let opts = OutputOpts { split: Some(SplitOpts { rows: None, megabytes: Some(2) }), ..parquet_opts(ParquetCompression::Zstd, 128) };
    let t = target(dir.path(), "pz", opts);
    let mut sink = sink::build(&t).unwrap();
    let rows = 200_000u64;
    let batches = write_wide(sink.as_mut(), rows, 5_000);
    let rep = sink.finish().unwrap();
    assert_eq!(rep.rows, rows);
    assert!(rep.files.len() >= 3, "expected several ~2 MB parts, got {}", rep.files.len());
    let per_batch = rep.bytes / batches as u64;
    let mut total = 0i64;
    for (i, path) in rep.files.iter().enumerate() {
        let len = std::fs::metadata(path).unwrap().len();
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        assert!(len <= limit + per_batch + 64 * 1024, "{name} is {len} bytes (batch ~{per_batch})");
        if i + 1 < rep.files.len() {
            assert!(len >= limit - limit / 8, "{name} closed early at {len} bytes");
        }
        let meta = ParquetRecordBatchReaderBuilder::try_new(File::open(path).unwrap()).unwrap().metadata().clone();
        assert!(matches!(meta.row_group(0).column(0).compression(), parquet::basic::Compression::ZSTD(_)));
        total += meta.file_metadata().num_rows();
    }
    assert_eq!(total as u64, rows);
}

// ---------------------------------------------------------------------------------------------
// (h) abort renames to .partial
// ---------------------------------------------------------------------------------------------

#[test]
fn abort_renames_every_file_to_partial() {
    let dir = tempfile::tempdir().unwrap();
    let rows = nasty_rows(10);
    let opts = OutputOpts {
        csv: Some(csv_opts(true, ",", "", false, false)),
        compress: Some(Compression::Gzip),
        split: Some(SplitOpts { rows: Some(4), megabytes: None }),
        ..OutputOpts::default()
    };
    let t = target(dir.path(), "ab", opts);
    let mut sink = sink::build(&t).unwrap();
    for rb in nasty_batches(&rows, 10) {
        sink.write(Batch::Arrow(rb)).unwrap();
    }
    assert_eq!(sink.files_so_far(), 3);
    let renamed = sink.abort();
    assert_eq!(renamed.len(), 3);
    for (i, p) in renamed.iter().enumerate() {
        assert_eq!(p.file_name().unwrap().to_string_lossy(), format!("ab_part{:04}.csv.gz.partial", i + 1));
        assert!(p.exists(), "{} missing", p.display());
        assert!(!dir.path().join(format!("ab_part{:04}.csv.gz", i + 1)).exists());
    }
}

#[test]
fn abort_parquet_single_file() {
    let dir = tempfile::tempdir().unwrap();
    let rows = nasty_rows(5);
    let t = target(dir.path(), "pa", parquet_opts(ParquetCompression::Zstd, 128));
    let mut sink = sink::build(&t).unwrap();
    for rb in nasty_batches(&rows, 5) {
        sink.write(Batch::Arrow(rb)).unwrap();
    }
    let renamed = sink.abort();
    assert_eq!(renamed, vec![dir.path().join("pa.parquet.partial")]);
    assert!(renamed[0].exists());
}

#[test]
fn abort_before_any_write_renames_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let t = target(dir.path(), "none", output(CsvOpts::default()));
    let sink = sink::build(&t).unwrap();
    assert!(sink.abort().is_empty());
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
}

// ---------------------------------------------------------------------------------------------
// (i) empty result still produces a header-only CSV
// ---------------------------------------------------------------------------------------------

#[test]
fn empty_raw_result_writes_header_only_file() {
    let dir = tempfile::tempdir().unwrap();
    let t = target(dir.path(), "empty", output(csv_opts(true, ",", "", true, false)));
    let mut sink = sink::build(&t).unwrap();
    sink.write(Batch::RawCsvHeader(Bytes::from_static(b"a,b\n"))).unwrap();
    let rep = sink.finish().unwrap();
    assert_eq!(rep.rows, 0);
    assert_eq!(rep.files, vec![dir.path().join("empty.csv")]);
    assert_eq!(std::fs::read(&rep.files[0]).unwrap(), b"\xEF\xBB\xBFa,b\n");
    assert_eq!(rep.bytes, 7);
}

#[test]
fn empty_arrow_result_writes_header_from_schema() {
    let dir = tempfile::tempdir().unwrap();
    let t = target(dir.path(), "empty_arrow", output(csv_opts(true, ",", "", false, false)));
    let mut sink = sink::build(&t).unwrap();
    // A zero-row batch carries the schema (what Arrow sources send for empty results).
    let rb = nasty_batches(&nasty_rows(1), 1).remove(0).slice(0, 0);
    sink.write(Batch::Arrow(rb)).unwrap();
    let rep = sink.finish().unwrap();
    assert_eq!(rep.rows, 0);
    assert_eq!(std::fs::read_to_string(&rep.files[0]).unwrap(), "id,text,num,note,flag,unicode\n");
}

#[test]
fn empty_result_without_any_batch_still_creates_file() {
    let dir = tempfile::tempdir().unwrap();
    let t = target(dir.path(), "nothing", output(csv_opts(true, ",", "", true, false)));
    let sink = sink::build(&t).unwrap();
    let rep = sink.finish().unwrap();
    assert_eq!(rep.files, vec![dir.path().join("nothing.csv")]);
    assert_eq!(std::fs::read(&rep.files[0]).unwrap(), b"\xEF\xBB\xBF");
}

// ---------------------------------------------------------------------------------------------
// (j) overwrite = false refuses an existing file
// ---------------------------------------------------------------------------------------------

#[test]
fn overwrite_false_refuses_existing_file() {
    let dir = tempfile::tempdir().unwrap();
    let existing = dir.path().join("keep.csv");
    std::fs::write(&existing, b"precious\n").unwrap();
    let t = target(dir.path(), "keep", output(CsvOpts::default()));
    let mut sink = sink::build(&t).unwrap();
    let err = sink.write(Batch::RawCsv { data: Bytes::from_static(b"1\n"), rows: 1 }).unwrap_err();
    assert_eq!(err.code(), "IO");
    assert!(err.to_string().contains("already exists"), "{err}");
    assert_eq!(std::fs::read(&existing).unwrap(), b"precious\n", "untouched");

    // Same with split parts and with compression.
    let part = dir.path().join("keep2_part0001.csv.gz");
    std::fs::write(&part, b"x").unwrap();
    let opts = OutputOpts { compress: Some(Compression::Gzip), split: Some(SplitOpts { rows: Some(2), megabytes: None }), ..output(CsvOpts::default()) };
    let mut sink = sink::build(&target(dir.path(), "keep2", opts)).unwrap();
    let err = sink.write(Batch::RawCsv { data: Bytes::from_static(b"1\n"), rows: 1 }).unwrap_err();
    assert_eq!(err.code(), "IO");
    assert_eq!(std::fs::read(&part).unwrap(), b"x");
}

#[test]
fn overwrite_true_replaces_existing_file() {
    let dir = tempfile::tempdir().unwrap();
    let existing = dir.path().join("re.csv");
    std::fs::write(&existing, b"old content that is longer than the new one\n").unwrap();
    let t = target(dir.path(), "re", OutputOpts { overwrite: true, ..output(csv_opts(false, ",", "", false, false)) });
    let mut sink = sink::build(&t).unwrap();
    sink.write(Batch::RawCsv { data: Bytes::from_static(b"1\n"), rows: 1 }).unwrap();
    let rep = sink.finish().unwrap();
    assert_eq!(std::fs::read(&existing).unwrap(), b"1\n");
    assert_eq!(rep.bytes, 2);
}

// ---------------------------------------------------------------------------------------------
// direct use of the single-file sinks
// ---------------------------------------------------------------------------------------------

#[test]
fn csv_sink_over_plain_file_counts_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("direct.csv");
    let file = File::create(&path).unwrap();
    let mut sink: Box<dyn Sink> = Box::new(CsvSink::new(file, path.clone(), csv_opts(true, ",", "", false, false)));
    assert_eq!(sink.files_so_far(), 1);
    let rows = nasty_rows(100);
    for rb in nasty_batches(&rows, 40) {
        sink.write(Batch::Arrow(rb)).unwrap();
    }
    let progress = sink.bytes_out();
    assert_eq!(progress, std::fs::metadata(&path).unwrap().len(), "bytes_out is exact after each batch");
    let rep = sink.finish().unwrap();
    assert_eq!(rep.bytes, progress);
    assert_eq!(rep.rows, 100);
}

#[test]
fn csv_sink_compressed_reports_compressed_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("direct.csv.zst");
    let writer = CompressWriter::new(File::create(&path).unwrap(), Compression::Zstd).unwrap();
    let mut sink: Box<dyn Sink> = Box::new(CsvSink::compressed(writer, path.clone(), csv_opts(true, ",", "", false, false)));
    let rows = nasty_rows(3000);
    for rb in nasty_batches(&rows, 1000) {
        sink.write(Batch::Arrow(rb)).unwrap();
    }
    let rep = sink.finish().unwrap();
    assert_eq!(rep.bytes, std::fs::metadata(&path).unwrap().len());
    let back = zstd::stream::decode_all(File::open(&path).unwrap()).unwrap();
    let (_, got) = read_csv_bytes(&back, b',');
    assert_rows_match(&rows, &got, "");
}

#[test]
fn parquet_sink_direct_with_schema_for_empty_result() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("direct.parquet");
    let schema = nasty_batches(&nasty_rows(1), 1)[0].schema();
    let sink: Box<dyn Sink> = Box::new(ParquetSink::new(path.clone(), ParquetOpts::default()).with_schema(schema));
    let rep = sink.finish().unwrap();
    assert_eq!(rep.rows, 0);
    assert_eq!(rep.files, vec![path.clone()]);
    let builder = ParquetRecordBatchReaderBuilder::try_new(File::open(&path).unwrap()).unwrap();
    assert_eq!(builder.schema().fields().len(), 6);
    assert_eq!(builder.metadata().file_metadata().num_rows(), 0);
}

#[test]
fn split_sink_with_custom_factory() {
    // The factory decides what a "file" is; here plain CSV files numbered by the sink.
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path().to_path_buf();
    let factory = Box::new(move |n: u32| -> bulk_core::Result<Box<dyn Sink>> {
        let path = d.join(format!("f{n}.csv"));
        Ok(Box::new(CsvSink::new(File::create(&path)?, path, csv_opts(false, ",", "", false, false))))
    });
    let mut sink: Box<dyn Sink> = Box::new(SplitSink::new(factory, Some(3), None));
    sink.write(Batch::RawCsvHeader(Bytes::from_static(b"h\n"))).unwrap();
    for i in 0..7 {
        sink.write(Batch::RawCsv { data: Bytes::from(format!("{i}\n")), rows: 1 }).unwrap();
    }
    let rep = sink.finish().unwrap();
    assert_eq!(rep.rows, 7);
    assert_eq!(rep.files.len(), 3);
    assert_eq!(std::fs::read_to_string(&rep.files[0]).unwrap(), "h\n0\n1\n2\n");
    assert_eq!(std::fs::read_to_string(&rep.files[1]).unwrap(), "h\n3\n4\n5\n");
    assert_eq!(std::fs::read_to_string(&rep.files[2]).unwrap(), "h\n6\n");
}

// ---------------------------------------------------------------------------------------------
// (j) a failure while opening or closing a file never leaves it under its final name
// ---------------------------------------------------------------------------------------------

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use bulk_core::sink::naming;
use bulk_core::sink::split::PathFn;
use bulk_core::sink::SinkReport;
use bulk_core::CoreError;

/// A file whose writes and flushes start failing once `fail` is set (a disk filling up).
struct Flaky {
    file: File,
    fail: Arc<AtomicBool>,
}

impl std::io::Write for Flaky {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if self.fail.load(Ordering::SeqCst) {
            return Err(std::io::Error::other("no space left on device"));
        }
        std::io::Write::write(&mut self.file, buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        if self.fail.load(Ordering::SeqCst) {
            return Err(std::io::Error::other("no space left on device"));
        }
        std::io::Write::flush(&mut self.file)
    }
}

/// File names in `dir`, sorted.
fn dir_names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
    names.sort();
    names
}

/// One RawCsv batch per row: `"<i>\n"`.
fn write_rows(sink: &mut dyn Sink, rows: std::ops::Range<u32>) -> bulk_core::Result<()> {
    for i in rows {
        sink.write(Batch::RawCsv { data: Bytes::from(format!("{i}\n")), rows: 1 })?;
    }
    Ok(())
}

/// Split sink (3 rows per file, header `h`) whose parts `f<n>.csv` are `CsvSink`s over
/// [`Flaky`] files that all start failing once the returned flag is set.
fn flaky_split(dir: &Path, with_paths: bool) -> (Box<dyn Sink>, Arc<AtomicBool>) {
    let fail = Arc::new(AtomicBool::new(false));
    let (d, f) = (dir.to_path_buf(), fail.clone());
    let factory = Box::new(move |n: u32| -> bulk_core::Result<Box<dyn Sink>> {
        let path = d.join(format!("f{n}.csv"));
        let file = Flaky { file: File::create(&path)?, fail: f.clone() };
        Ok(Box::new(CsvSink::new(file, path, csv_opts(false, ",", "", false, false))))
    });
    let mut split = SplitSink::new(factory, Some(3), None);
    if with_paths {
        let d = dir.to_path_buf();
        let path_fn: PathFn = Box::new(move |n| d.join(format!("f{n}.csv")));
        split = split.with_path_fn(path_fn);
    }
    let mut sink: Box<dyn Sink> = Box::new(split);
    sink.write(Batch::RawCsvHeader(Bytes::from_static(b"h\n"))).unwrap();
    (sink, fail)
}

#[test]
fn csv_sink_failed_finish_renames_to_partial() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("full.csv");
    let fail = Arc::new(AtomicBool::new(false));
    let file = Flaky { file: File::create(&path).unwrap(), fail: fail.clone() };
    let mut sink: Box<dyn Sink> = Box::new(CsvSink::new(file, path.clone(), csv_opts(true, ",", "", false, false)));
    sink.write(Batch::RawCsvHeader(Bytes::from_static(b"id\n"))).unwrap();
    write_rows(sink.as_mut(), 0..5).unwrap();
    fail.store(true, Ordering::SeqCst);
    let err = sink.finish().unwrap_err();
    assert_eq!(err.code(), "IO");
    assert_eq!(dir_names(dir.path()), vec!["full.csv.partial"]);
    assert_eq!(std::fs::read_to_string(dir.path().join("full.csv.partial")).unwrap(), "id\n0\n1\n2\n3\n4\n");
}

#[test]
fn split_failed_close_on_rollover_leaves_only_partial_files() {
    for with_paths in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let (mut sink, fail) = flaky_split(dir.path(), with_paths);
        write_rows(sink.as_mut(), 0..6).unwrap();
        fail.store(true, Ordering::SeqCst);
        // Row 7 rolls over: closing f2 fails on its final flush.
        assert_eq!(write_rows(sink.as_mut(), 6..7).unwrap_err().code(), "IO");
        // What the pipeline does after a failed write.
        let renamed = sink.abort();
        assert_eq!(dir_names(dir.path()), vec!["f1.csv.partial", "f2.csv.partial"], "with_paths={with_paths}");
        let mut expected = vec![dir.path().join("f1.csv.partial")];
        if with_paths {
            // Only a split sink that knows the paths can report the file that failed to close.
            expected.push(dir.path().join("f2.csv.partial"));
        }
        assert_eq!(renamed, expected);
    }
}

#[test]
fn split_failed_final_finish_renames_every_part() {
    let dir = tempfile::tempdir().unwrap();
    let (mut sink, fail) = flaky_split(dir.path(), true);
    write_rows(sink.as_mut(), 0..7).unwrap();
    fail.store(true, Ordering::SeqCst);
    // The pipeline returns this error without calling `abort`.
    assert_eq!(sink.finish().unwrap_err().code(), "IO");
    assert_eq!(dir_names(dir.path()), vec!["f1.csv.partial", "f2.csv.partial", "f3.csv.partial"]);
    assert_eq!(std::fs::read_to_string(dir.path().join("f1.csv.partial")).unwrap(), "h\n0\n1\n2\n");
    assert_eq!(std::fs::read_to_string(dir.path().join("f3.csv.partial")).unwrap(), "h\n6\n");
}

/// A foreign inner sink that fails at a chosen step and, unlike the sinks of this crate, does
/// not rename its file when `finish` fails.
struct Faulty {
    inner: Box<dyn Sink>,
    fail_header: bool,
    fail_finish: bool,
}

impl Sink for Faulty {
    fn write(&mut self, batch: Batch) -> bulk_core::Result<()> {
        if self.fail_header && matches!(batch, Batch::RawCsvHeader(_)) {
            return Err(CoreError::Io("header write failed".into()));
        }
        self.inner.write(batch)
    }

    fn finish(self: Box<Self>) -> bulk_core::Result<SinkReport> {
        if self.fail_finish {
            // Closes the file, renames nothing.
            drop(self.inner);
            return Err(CoreError::Io("footer write failed".into()));
        }
        self.inner.finish()
    }

    fn abort(self: Box<Self>) -> Vec<PathBuf> {
        self.inner.abort()
    }

    fn bytes_out(&self) -> u64 {
        self.inner.bytes_out()
    }

    fn files_so_far(&self) -> u32 {
        self.inner.files_so_far()
    }
}

/// Split sink (2 rows per file, header `h`) over [`Faulty`] parts `g<n>.csv`: the header
/// write of part `fail_header_on` and the `finish` of part `fail_finish_on` fail.
fn faulty_split(dir: &Path, fail_header_on: u32, fail_finish_on: u32) -> Box<dyn Sink> {
    let d = dir.to_path_buf();
    let factory = Box::new(move |n: u32| -> bulk_core::Result<Box<dyn Sink>> {
        let path = d.join(format!("g{n}.csv"));
        let inner = Box::new(CsvSink::new(File::create(&path)?, path, csv_opts(false, ",", "", false, false)));
        Ok(Box::new(Faulty { inner, fail_header: n == fail_header_on, fail_finish: n == fail_finish_on }))
    });
    let d = dir.to_path_buf();
    let path_fn: PathFn = Box::new(move |n| d.join(format!("g{n}.csv")));
    let mut sink: Box<dyn Sink> = Box::new(SplitSink::new(factory, Some(2), None).with_path_fn(path_fn));
    sink.write(Batch::RawCsvHeader(Bytes::from_static(b"h\n"))).unwrap();
    sink
}

#[test]
fn split_header_write_failure_aborts_the_new_file() {
    let dir = tempfile::tempdir().unwrap();
    let mut sink = faulty_split(dir.path(), 2, 0);
    write_rows(sink.as_mut(), 0..2).unwrap();
    // Row 3 opens g2, whose header write fails.
    assert_eq!(write_rows(sink.as_mut(), 2..3).unwrap_err().code(), "IO");
    assert_eq!(dir_names(dir.path()), vec!["g1.csv", "g2.csv.partial"]);
    let renamed = sink.abort();
    assert_eq!(renamed, vec![dir.path().join("g1.csv.partial"), dir.path().join("g2.csv.partial")]);
    assert_eq!(dir_names(dir.path()), vec!["g1.csv.partial", "g2.csv.partial"]);
}

#[test]
fn split_renames_a_foreign_sink_that_failed_to_close() {
    let dir = tempfile::tempdir().unwrap();
    let mut sink = faulty_split(dir.path(), 0, 3);
    write_rows(sink.as_mut(), 0..5).unwrap();
    assert_eq!(sink.finish().unwrap_err().code(), "IO");
    assert_eq!(dir_names(dir.path()), vec!["g1.csv.partial", "g2.csv.partial", "g3.csv.partial"]);
    assert_eq!(std::fs::read_to_string(dir.path().join("g3.csv.partial")).unwrap(), "h\n4\n");
    // The split sink's name for a renamed file is the one `to_partial` uses.
    let missing = dir.path().join("never.csv");
    assert_eq!(sink::to_partial(&missing), naming::partial_path(&missing));
}

#[test]
fn successful_finish_after_split_keeps_final_names() {
    let dir = tempfile::tempdir().unwrap();
    let (mut sink, _fail) = flaky_split(dir.path(), true);
    write_rows(sink.as_mut(), 0..4).unwrap();
    let rep = sink.finish().unwrap();
    assert_eq!(rep.files, vec![dir.path().join("f1.csv"), dir.path().join("f2.csv")]);
    assert_eq!(dir_names(dir.path()), vec!["f1.csv", "f2.csv"]);
}
