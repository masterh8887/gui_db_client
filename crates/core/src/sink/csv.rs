//! CSV sink: RawCsv batches are written as-is; Arrow batches are formatted row by row with the
//! `csv` crate (RFC 4180 quoting, configurable delimiter / NULL text / CRLF / BOM / encoding).
//!
//! One `CsvSink` is one file. The writer it is given is already open (the factory in
//! [`crate::sink::split`] creates files lazily and checks the overwrite rule); the sink adds a
//! 1 MB `BufWriter`, the output encoder and a byte counter in front of it:
//! `BufWriter -> TranscodeWriter -> CountingWriter -> W`. Everything above the encoder is UTF-8
//! (Arrow-formatted cells, header, NULL text, RawCsv chunks from PostgreSQL COPY); with
//! `CsvOpts.encoding = euc-kr` the encoder writes CP949 and the counter (and so splitting and
//! progress) sees encoded bytes. Characters CP949 cannot represent become `?` and are counted
//! in `SinkReport::unmappable_chars`. Nothing beyond one batch, the cell
//! scratch buffer, that 1 MB buffer and the encoder's 64 KB step buffer is ever held in memory.

use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use arrow::util::display::{ArrayFormatter, FormatOptions};
use arrow_array::RecordBatch;
use bytes::Bytes;
use csv::{QuoteStyle, Terminator, WriterBuilder};

use crate::encoding::{Charset, TranscodeWriter};
use crate::error::{CoreError, Result};
use crate::sink::compress::{CompressWriter, CountingWriter};
use crate::sink::{Sink, SinkReport};
use crate::source::Batch;
use crate::types::{CsvEncoding, CsvOpts};

/// Size of the buffer in front of the file / compressor.
pub const FILE_BUFFER_BYTES: usize = 1024 * 1024;
/// Internal buffer of the per-batch `csv::Writer` (flushes into the 1 MB buffer above).
const CSV_BUFFER_BYTES: usize = 64 * 1024;

/// UTF-8 byte order mark written first when [`CsvOpts::writes_bom`] (`bom` set and the encoding
/// is UTF-8; never for EUC-KR).
pub const UTF8_BOM: &[u8] = b"\xEF\xBB\xBF";
/// `Date32` columns.
pub const DATE_FORMAT: &str = "%Y-%m-%d";
/// Naive timestamps.
pub const TIMESTAMP_FORMAT: &str = "%Y-%m-%d %H:%M:%S%.6f";
/// Timestamps with a zone: same, plus `+09:00`.
pub const TIMESTAMP_TZ_FORMAT: &str = "%Y-%m-%d %H:%M:%S%.6f%:z";
/// Time-of-day columns.
pub const TIME_FORMAT: &str = "%H:%M:%S%.6f";

/// Delimiter byte for `CsvOpts.delimiter` (first byte; `,` when empty).
pub fn delimiter_byte(opts: &CsvOpts) -> Result<u8> {
    match opts.delimiter.chars().next() {
        None => Ok(b','),
        Some(c) if c.is_ascii() && c != '"' && c != '\n' && c != '\r' => Ok(c as u8),
        Some(c) => Err(CoreError::Invalid(format!("csv delimiter must be a single ASCII character, got {c:?}"))),
    }
}

/// Output-encoding check: with EUC-KR the delimiter must not be an ASCII letter. CP949 trail
/// bytes include `A-Z` and `a-z` (never `,`, `;`, tab, `|`, `"`, CR or LF), so a letter
/// delimiter would show up inside encoded characters that the UTF-8 quoting never saw.
pub fn check_encoding(opts: &CsvOpts) -> Result<()> {
    if opts.effective_encoding() == CsvEncoding::EucKr {
        let d = delimiter_byte(opts)?;
        if d.is_ascii_alphabetic() {
            return Err(CoreError::Invalid(format!(
                "csv delimiter {:?} cannot be used with the euc-kr encoding (CP949 characters contain ASCII letters as their second byte)",
                d as char
            )));
        }
    }
    Ok(())
}

/// Closure that closes the output writer and returns the on-disk byte count. Set for
/// compressed outputs, where the count only exists once the compressor thread has finished.
type Finisher<W> = Box<dyn FnOnce(W) -> Result<u64> + Send>;

/// The writer stack in front of `W`: buffer (UTF-8) -> output encoder -> byte counter.
type Out<W> = BufWriter<TranscodeWriter<CountingWriter<W>>>;

pub struct CsvSink<W: Write + Send> {
    out: Option<Out<W>>,
    opts: CsvOpts,
    path: PathBuf,
    delimiter: u8,
    rows: u64,
    /// `Batch::RawCsvHeader`, remembered so it can be written at the top of the file.
    header: Option<Bytes>,
    /// BOM (and any known header) written.
    started: bool,
    /// A header line has been written to this file.
    header_written: bool,
    /// Bytes handed to `W` (on-disk size for a plain file).
    own_bytes: Arc<AtomicU64>,
    /// Overrides `own_bytes` when `W` compresses (the compressed size).
    disk_bytes: Option<Arc<AtomicU64>>,
    finisher: Option<Finisher<W>>,
    /// Final on-disk size once finished.
    final_bytes: Option<u64>,
    /// Scratch buffer for one formatted cell.
    cell: String,
    /// Characters written as `?` (EUC-KR output), final once the sink is closed.
    unmappable: u64,
}

impl<W: Write + Send> CsvSink<W> {
    /// Sink over an already open writer. `path` is only used for the report and `.partial`
    /// renaming. `bytes_out` counts the bytes handed to `writer`.
    pub fn new(writer: W, path: PathBuf, opts: CsvOpts) -> Self {
        let delimiter = delimiter_byte(&opts).unwrap_or(b',');
        let counting = CountingWriter::new(writer);
        let own_bytes = counting.counter();
        let encoder = TranscodeWriter::new(counting, Charset::from(opts.effective_encoding()));
        CsvSink {
            out: Some(BufWriter::with_capacity(FILE_BUFFER_BYTES, encoder)),
            opts,
            path,
            delimiter,
            rows: 0,
            header: None,
            started: false,
            header_written: false,
            own_bytes,
            disk_bytes: None,
            finisher: None,
            final_bytes: None,
            cell: String::with_capacity(256),
            unmappable: 0,
        }
    }

    /// Validate the options up front (delimiter, delimiter vs encoding); `new` falls back to
    /// `,` silently.
    pub fn validate(opts: &CsvOpts) -> Result<()> {
        delimiter_byte(opts)?;
        check_encoding(opts)
    }

    /// Rows written so far.
    pub fn rows(&self) -> u64 {
        self.rows
    }

    /// Characters written as `?` so far (bytes still in the 1 MB buffer are not encoded yet).
    pub fn unmappable_chars(&self) -> u64 {
        match &self.out {
            Some(out) => out.get_ref().unmappable(),
            None => self.unmappable,
        }
    }

    /// Path of the file this sink writes.
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    fn out(&mut self) -> Result<&mut Out<W>> {
        self.out.as_mut().ok_or_else(|| CoreError::Io("csv sink is closed".into()))
    }

    /// Write the BOM and, when known, the raw header. Idempotent.
    fn begin(&mut self) -> Result<()> {
        if !self.started {
            self.started = true;
            if self.opts.writes_bom() {
                self.out()?.write_all(UTF8_BOM)?;
            }
        }
        if !self.header_written {
            if let Some(h) = self.header.clone() {
                self.out()?.write_all(&h)?;
                self.header_written = true;
            }
        }
        Ok(())
    }

    fn write_raw(&mut self, data: &[u8], rows: u64) -> Result<()> {
        self.begin()?;
        let out = self.out()?;
        out.write_all(data)?;
        out.flush()?;
        self.rows += rows;
        Ok(())
    }

    fn write_arrow(&mut self, batch: &RecordBatch) -> Result<()> {
        self.begin()?;
        let Self { out, opts, delimiter, header_written, cell, .. } = self;
        let out = out.as_mut().ok_or_else(|| CoreError::Io("csv sink is closed".into()))?;

        let options = FormatOptions::new()
            .with_null(&opts.null_as)
            .with_date_format(Some(DATE_FORMAT))
            .with_timestamp_format(Some(TIMESTAMP_FORMAT))
            .with_timestamp_tz_format(Some(TIMESTAMP_TZ_FORMAT))
            .with_time_format(Some(TIME_FORMAT))
            .with_display_error(false);
        let formatters = batch.columns().iter().map(|c| ArrayFormatter::try_new(c.as_ref(), &options)).collect::<std::result::Result<Vec<_>, _>>()?;

        let mut wtr = WriterBuilder::new()
            .delimiter(*delimiter)
            .terminator(if opts.crlf { Terminator::CRLF } else { Terminator::Any(b'\n') })
            .quote_style(QuoteStyle::Necessary)
            .double_quote(true)
            .has_headers(false)
            .buffer_capacity(CSV_BUFFER_BYTES)
            .from_writer(&mut *out);

        if opts.header && !*header_written {
            wtr.write_record(batch.schema().fields().iter().map(|f| f.name().as_str()))?;
            *header_written = true;
        }
        for row in 0..batch.num_rows() {
            for f in &formatters {
                cell.clear();
                f.value(row).write(&mut *cell)?;
                wtr.write_field(cell.as_bytes())?;
            }
            wtr.write_record(None::<&[u8]>)?;
        }
        wtr.flush()?;
        drop(wtr);
        out.flush()?;
        self.rows += batch.num_rows() as u64;
        Ok(())
    }

    /// Write BOM / header if still pending, flush every buffer and close the writer (waiting
    /// for the compressor thread when there is one). Returns the on-disk byte count.
    ///
    /// The writer is moved out of `self` before anything that can fail with it in hand, so on
    /// `Err` it has already been dropped and the file handle released by the time this
    /// returns (needed before the file can be renamed on Windows).
    fn close(&mut self) -> Result<u64> {
        self.begin()?;
        let mut out = self.out.take().ok_or_else(|| CoreError::Io("csv sink is closed".into()))?;
        out.flush()?;
        let encoder = out.into_inner().map_err(|e| CoreError::Io(e.to_string()))?;
        let (counting, unmappable) = encoder.finish()?;
        self.unmappable = unmappable;
        let own = counting.bytes();
        let writer = counting.into_inner();
        match self.finisher.take() {
            Some(finish) => finish(writer),
            None => {
                let mut writer = writer;
                writer.flush()?;
                Ok(own)
            }
        }
    }
}

impl CsvSink<CompressWriter> {
    /// Sink whose output goes through a [`CompressWriter`]: `bytes_out` is the compressed size
    /// and `finish` waits for the compressor thread.
    pub fn compressed(writer: CompressWriter, path: PathBuf, opts: CsvOpts) -> Self {
        let disk = writer.bytes_out.clone();
        let mut sink = CsvSink::new(writer, path, opts);
        sink.disk_bytes = Some(disk);
        sink.finisher = Some(Box::new(|w: CompressWriter| w.finish()));
        sink
    }
}

impl<W: Write + Send> Sink for CsvSink<W> {
    fn write(&mut self, batch: Batch) -> Result<()> {
        match batch {
            Batch::RawCsvHeader(h) => {
                self.header = Some(h);
                self.begin()
            }
            Batch::RawCsv { data, rows } => self.write_raw(&data, rows),
            Batch::Arrow(rb) => self.write_arrow(&rb),
        }
    }

    /// Flush and close the file. An empty result still gets BOM + header so the file is valid.
    ///
    /// On failure (final flush on a full disk, compressor trailer error, ...) the file is
    /// closed and renamed to `<name>.partial` before the error is returned: the pipeline does
    /// not call [`Sink::abort`] after a failed `finish`, and a truncated file must never keep
    /// its final name.
    fn finish(mut self: Box<Self>) -> Result<SinkReport> {
        match self.close() {
            Ok(bytes) => {
                self.final_bytes = Some(bytes);
                Ok(SinkReport { rows: self.rows, bytes, files: vec![self.path.clone()], unmappable_chars: self.unmappable })
            }
            Err(e) => {
                // `close` still holds the writer only when `begin` failed; drop it so the file
                // is closed before the rename.
                drop(self.out.take());
                crate::sink::to_partial(&self.path);
                Err(e)
            }
        }
    }

    fn abort(mut self: Box<Self>) -> Vec<PathBuf> {
        // Dropping closes the file (and joins the compressor thread) before the rename.
        drop(self.out.take());
        vec![crate::sink::to_partial(&self.path)]
    }

    fn bytes_out(&self) -> u64 {
        if let Some(b) = self.final_bytes {
            return b;
        }
        match &self.disk_bytes {
            Some(c) => c.load(Ordering::Relaxed),
            None => self.own_bytes.load(Ordering::Relaxed),
        }
    }

    fn files_so_far(&self) -> u32 {
        1
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow_array::{Int64Array, StringArray};
    use arrow_schema::{DataType, Field, Schema};

    fn batch() -> RecordBatch {
        let schema = Arc::new(Schema::new(vec![Field::new("id", DataType::Int64, true), Field::new("s", DataType::Utf8, true)]));
        RecordBatch::try_new(schema, vec![Arc::new(Int64Array::from(vec![Some(1), Some(2), None])), Arc::new(StringArray::from(vec![Some("a,b"), None, Some("x\"y")]))])
            .unwrap()
    }

    #[test]
    fn arrow_to_csv_quotes_and_nulls() {
        let opts = CsvOpts { null_as: "NULL".into(), ..CsvOpts::default() };
        let mut sink = Box::new(CsvSink::new(Vec::<u8>::new(), PathBuf::from("mem.csv"), opts));
        sink.write(Batch::Arrow(batch())).unwrap();
        let bytes = sink.bytes_out();
        assert!(bytes > 0);
        let rep = sink.finish().unwrap();
        assert_eq!(rep.rows, 3);
        assert_eq!(rep.bytes, bytes);
    }

    #[test]
    fn delimiter_rules() {
        assert_eq!(delimiter_byte(&CsvOpts::default()).unwrap(), b',');
        assert_eq!(delimiter_byte(&CsvOpts { delimiter: "\t".into(), ..CsvOpts::default() }).unwrap(), b'\t');
        assert_eq!(delimiter_byte(&CsvOpts { delimiter: String::new(), ..CsvOpts::default() }).unwrap(), b',');
        assert!(delimiter_byte(&CsvOpts { delimiter: "│".into(), ..CsvOpts::default() }).is_err());
        assert!(delimiter_byte(&CsvOpts { delimiter: "\"".into(), ..CsvOpts::default() }).is_err());
    }

    fn euc_kr(delimiter: &str) -> CsvOpts {
        CsvOpts { delimiter: delimiter.into(), encoding: Some(CsvEncoding::EucKr), ..CsvOpts::default() }
    }

    #[test]
    fn letter_delimiters_are_rejected_for_euc_kr_only() {
        for d in [",", ";", "\t", "|"] {
            assert!(CsvSink::<Vec<u8>>::validate(&euc_kr(d)).is_ok(), "{d:?}");
        }
        for d in ["a", "Z"] {
            let err = CsvSink::<Vec<u8>>::validate(&euc_kr(d)).unwrap_err();
            assert_eq!(err.code(), "INVALID", "{d:?}");
            assert!(check_encoding(&CsvOpts { delimiter: d.into(), ..CsvOpts::default() }).is_ok(), "UTF-8 allows {d:?}");
        }
    }

    #[test]
    fn euc_kr_report_counts_unmappable_characters() {
        let schema = Arc::new(Schema::new(vec![Field::new("s", DataType::Utf8, true)]));
        let rb = RecordBatch::try_new(schema, vec![Arc::new(StringArray::from(vec![Some("한글"), Some("🚀🎉"), None]))]).unwrap();
        let mut sink = Box::new(CsvSink::new(Vec::<u8>::new(), PathBuf::from("mem.csv"), CsvOpts { bom: true, ..euc_kr(",") }));
        sink.write(Batch::Arrow(rb)).unwrap();
        assert_eq!(sink.unmappable_chars(), 2, "flushed after each batch");
        let rep = sink.finish().unwrap();
        assert_eq!(rep.unmappable_chars, 2);
        // `s` LF, CP949 "한글" (4 bytes) LF, `??` LF, `""` LF (a lone empty field is quoted);
        // no BOM for EUC-KR although `bom` is set.
        assert_eq!(rep.bytes, 2 + 5 + 3 + 3);
    }
}
