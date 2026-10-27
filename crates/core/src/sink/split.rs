//! Split sink: opens `<stem>_partNNNN<ext>` every N rows or M bytes and repeats the CSV header
//! per file. Also the place where the whole stack (csv/parquet + compression) is assembled.
//!
//! Row splitting is exact: Arrow batches are sliced (`RecordBatch::slice`, zero copy) and
//! RawCsv batches are cut at a row boundary found by a quote-aware newline scan, so every file
//! but the last holds exactly N rows. Byte splitting rolls over at the first batch boundary
//! after the current file reached M MB, as measured by the inner sink's
//! [`Sink::bytes_out`]: bytes on disk for CSV (after compression), bytes on disk plus the
//! expected size of the buffered row group for Parquet. Parquet row groups are capped at M MB
//! as well ([`open_single`]), so a part never has to wait for a 128 MB row group, and the parts
//! of one output share a [`SizeCalibration`] so every part after the first estimates its
//! buffered row group with the compression ratio measured so far.
//!
//! [`Sink::bytes_out`] of the split sink is a high-water mark: it never decreases, even when a
//! closed Parquet part turns out a little smaller than its last estimate.
//!
//! Files are created lazily by a factory closure the moment the first bytes for them arrive
//! (or at `finish` for an empty result), which is also where the overwrite rule is enforced.
//!
//! Failure rule: no file of a run that did not complete keeps its final name. `abort` renames
//! everything to `.partial`; a failed `finish` does the same on its own (the pipeline does not
//! call `abort` after `finish`), and a file whose sink failed while being opened (header) or
//! closed (final flush, compressor trailer, Parquet footer) is renamed right away and reported
//! by the later `abort`. The single-file sinks of this module rename their own file when their
//! `finish` fails; for a foreign inner sink, [`SplitSink::with_path_fn`] lets the split sink
//! find and rename the file itself.

use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};

use bytes::Bytes;

use crate::error::{CoreError, Result};
use crate::sink::compress::CompressWriter;
use crate::sink::csv::CsvSink;
use crate::sink::naming::partial_path;
use crate::sink::parquet::{ParquetSink, SizeCalibration};
use crate::sink::{Sink, SinkReport, SinkTarget};
use crate::source::Batch;
use crate::types::OutputFormat;

/// Creates the single-file sink for file number `n` (`n` as passed to `SinkTarget::path_for`:
/// `0` for an unsplit output, `1..` for parts).
pub type SinkFactory = Box<dyn FnMut(u32) -> Result<Box<dyn Sink>> + Send>;

/// Maps file number `n` (as passed to the [`SinkFactory`]) to the path the factory's sink
/// writes. Lets [`SplitSink`] locate a file whose inner sink failed in `finish`.
pub type PathFn = Box<dyn Fn(u32) -> PathBuf + Send>;

/// Byte offset just past the `n`-th row terminator in `data`, or `data.len()` when `data`
/// holds fewer than `n` complete rows. Quote-aware: `\n` inside a `"..."` field does not end a
/// row (RFC 4180 doubled quotes toggle twice and so cancel out). `\r\n` is handled because the
/// `\n` is what is counted.
pub fn raw_csv_row_boundary(data: &[u8], n: u64) -> usize {
    if n == 0 {
        return 0;
    }
    let mut in_quotes = false;
    let mut rows = 0u64;
    for (i, &b) in data.iter().enumerate() {
        match b {
            b'"' => in_quotes = !in_quotes,
            b'\n' if !in_quotes => {
                rows += 1;
                if rows == n {
                    return i + 1;
                }
            }
            _ => {}
        }
    }
    data.len()
}

pub struct SplitSink {
    factory: SinkFactory,
    /// Roll over once a file holds this many rows (`None`: no row limit).
    max_rows: Option<u64>,
    /// Roll over before the next batch once a file holds at least this many bytes.
    max_bytes: Option<u64>,
    /// Single-file output: `path_for(0)`, never rolls over.
    single: bool,
    /// `Batch::RawCsvHeader`, replayed at the top of every file.
    header: Option<Bytes>,
    current: Option<Box<dyn Sink>>,
    /// File number (`n` given to the factory) of `current`.
    current_index: u32,
    current_rows: u64,
    /// Path of file `n`, when known (always for [`build_stack`]).
    path_fn: Option<PathFn>,
    opened: u32,
    rows: u64,
    done_bytes: u64,
    /// Highest `done_bytes + current.bytes_out()` seen after a write or before a close: what
    /// [`Sink::bytes_out`] reports, so the progress counter never moves backwards.
    reported_bytes: u64,
    /// Completed files, under their final names.
    files: Vec<PathBuf>,
    /// Files whose sink failed while opening or closing, already renamed to `.partial`.
    failed: Vec<PathBuf>,
}

impl SplitSink {
    /// Split every `rows` rows and/or `megabytes` MB. Zero or `None` means no limit on that
    /// axis; with neither limit this behaves like [`SplitSink::single`] but still numbers the
    /// file `_part0001`.
    pub fn new(factory: SinkFactory, rows: Option<u64>, megabytes: Option<u64>) -> Self {
        SplitSink {
            factory,
            max_rows: rows.filter(|r| *r > 0),
            max_bytes: megabytes.filter(|m| *m > 0).map(|m| m.saturating_mul(1024 * 1024)),
            single: false,
            header: None,
            current: None,
            current_index: 0,
            current_rows: 0,
            path_fn: None,
            opened: 0,
            rows: 0,
            done_bytes: 0,
            reported_bytes: 0,
            files: Vec::new(),
            failed: Vec::new(),
        }
    }

    /// Tell the sink which path the factory uses for file `n`. Without it the split sink relies
    /// on the inner sink to rename its own file to `.partial` when its `finish` fails (the
    /// sinks of this module do) and cannot report that file from [`Sink::abort`].
    pub fn with_path_fn(mut self, path_fn: PathFn) -> Self {
        self.path_fn = Some(path_fn);
        self
    }

    /// One file (`path_for(0)`), created lazily on the first write or at `finish`.
    pub fn single(factory: SinkFactory) -> Self {
        let mut s = SplitSink::new(factory, None, None);
        s.single = true;
        s
    }

    fn next_index(&self) -> u32 {
        if self.single {
            0
        } else {
            self.opened + 1
        }
    }

    /// Open the next file (replaying the CSV header) if none is open. If the header cannot be
    /// written, the just-created file is aborted (renamed to `.partial`) and remembered for
    /// [`Sink::abort`].
    fn ensure_open(&mut self) -> Result<&mut Box<dyn Sink>> {
        if self.current.is_none() {
            let n = self.next_index();
            let mut sink = (self.factory)(n)?;
            self.opened += 1;
            self.current_rows = 0;
            self.current_index = n;
            if let Some(h) = &self.header {
                if let Err(e) = sink.write(Batch::RawCsvHeader(h.clone())) {
                    self.failed.extend(sink.abort());
                    return Err(e);
                }
            }
            self.current = Some(sink);
        }
        Ok(self.current.as_mut().expect("just opened"))
    }

    /// Close the current file and account for it. If its sink fails to close, the file is
    /// left as (or renamed to) `.partial` and remembered for [`Sink::abort`].
    fn close_current(&mut self) -> Result<()> {
        // Keep the open file's last estimate: its final size may come out slightly smaller.
        self.note_progress();
        if let Some(sink) = self.current.take() {
            self.current_rows = 0;
            // Whether the inner sink created a file at all (a Parquet sink that never saw a
            // batch has not), so a failed close never renames a file it does not own.
            let created = sink.files_so_far() > 0;
            match sink.finish() {
                Ok(rep) => {
                    self.done_bytes += rep.bytes;
                    self.files.extend(rep.files);
                }
                Err(e) => {
                    self.settle_failed_close(created);
                    return Err(e);
                }
            }
        }
        Ok(())
    }

    /// After the inner sink of file `current_index` failed in `finish`: make sure that file is
    /// `.partial` (the sinks of this module already renamed it; a foreign sink may not have)
    /// and remember it so `abort` reports it. Needs the path, i.e. [`SplitSink::with_path_fn`].
    fn settle_failed_close(&mut self, created: bool) {
        let Some(path_fn) = &self.path_fn else { return };
        if !created {
            return;
        }
        let path = path_fn(self.current_index);
        let partial = if path.exists() { crate::sink::to_partial(&path) } else { partial_path(&path) };
        if partial.exists() {
            self.failed.push(partial);
        }
    }

    /// Rename every file of this run to `.partial`: completed parts, the open file (closed by
    /// its own `abort`) and those already renamed after a failure. Returns the `.partial`
    /// paths in file order.
    fn abort_all(&mut self) -> Vec<PathBuf> {
        let mut renamed: Vec<PathBuf> = self.files.drain(..).map(|p| crate::sink::to_partial(&p)).collect();
        renamed.append(&mut self.failed);
        if let Some(cur) = self.current.take() {
            renamed.extend(cur.abort());
        }
        renamed
    }

    /// The fallible part of `finish`: an empty result still produces one (header-only) file,
    /// then the open file is closed.
    fn close_all(&mut self) -> Result<()> {
        self.ensure_open()?;
        self.close_current()
    }

    /// Completed bytes plus what the open file reports right now (may dip when a Parquet part
    /// closes smaller than its estimate; see `reported_bytes`).
    fn live_bytes(&self) -> u64 {
        self.done_bytes.saturating_add(self.current.as_ref().map(|c| c.bytes_out()).unwrap_or(0))
    }

    /// Raise the progress high-water mark to the current live value.
    fn note_progress(&mut self) {
        self.reported_bytes = self.reported_bytes.max(self.live_bytes());
    }

    /// Whether the open file is full (row limit reached or byte limit reached).
    fn current_full(&self) -> bool {
        if self.single {
            return false;
        }
        let Some(cur) = &self.current else { return false };
        if self.max_rows.is_some_and(|m| self.current_rows >= m) {
            return true;
        }
        self.max_bytes.is_some_and(|m| cur.bytes_out() >= m)
    }

    /// Rows that still fit in the current file (`u64::MAX` without a row limit).
    fn room(&self) -> u64 {
        match self.max_rows {
            Some(m) if !self.single => m.saturating_sub(self.current_rows),
            _ => u64::MAX,
        }
    }

    /// Close the current file if it is full, so the next rows start a new one. Called only
    /// when more rows are about to be written: a file that ends exactly on the limit is never
    /// followed by an empty one.
    fn roll_over_if_full(&mut self) -> Result<()> {
        if self.current_full() {
            self.close_current()?;
        }
        Ok(())
    }

    fn write_arrow(&mut self, rb: arrow_array::RecordBatch) -> Result<()> {
        let total = rb.num_rows();
        if total == 0 {
            // Carries the schema only (header for an otherwise empty file); never rolls over.
            return self.ensure_open()?.write(Batch::Arrow(rb));
        }
        let mut offset = 0usize;
        while offset < total {
            self.roll_over_if_full()?;
            let take = ((total - offset) as u64).min(self.room()) as usize;
            let piece = if offset == 0 && take == total { rb.clone() } else { rb.slice(offset, take) };
            self.ensure_open()?.write(Batch::Arrow(piece))?;
            self.current_rows += take as u64;
            self.rows += take as u64;
            offset += take;
        }
        Ok(())
    }

    fn write_raw(&mut self, mut data: Bytes, rows: u64) -> Result<()> {
        if data.is_empty() {
            return Ok(());
        }
        if rows == 0 {
            // The source did not count rows: cannot split exactly, pass through.
            self.roll_over_if_full()?;
            return self.ensure_open()?.write(Batch::RawCsv { data, rows });
        }
        let mut left = rows;
        while left > 0 && !data.is_empty() {
            self.roll_over_if_full()?;
            let take = left.min(self.room());
            // Everything that is left fits: no scan needed. Otherwise cut after `take` rows.
            let cut = if take == left { data.len() } else { raw_csv_row_boundary(&data, take) };
            let head = data.split_to(cut);
            self.ensure_open()?.write(Batch::RawCsv { data: head, rows: take })?;
            self.current_rows += take;
            self.rows += take;
            left -= take;
        }
        Ok(())
    }
}

impl Sink for SplitSink {
    fn write(&mut self, batch: Batch) -> Result<()> {
        let result = match batch {
            Batch::RawCsvHeader(h) => {
                self.header = Some(h.clone());
                match &mut self.current {
                    Some(cur) => cur.write(Batch::RawCsvHeader(h)),
                    None => Ok(()),
                }
            }
            Batch::Arrow(rb) => self.write_arrow(rb),
            Batch::RawCsv { data, rows } => self.write_raw(data, rows),
        };
        self.note_progress();
        result
    }

    /// Close the last file and report every file written. An empty result still produces one
    /// (header-only) file.
    ///
    /// On failure every file of the run, the earlier complete parts included, is renamed to
    /// `.partial` before the error is returned: the pipeline does not call [`Sink::abort`]
    /// after a failed `finish`, and a failed run must not leave complete-looking output.
    fn finish(mut self: Box<Self>) -> Result<SinkReport> {
        if let Err(e) = self.close_all() {
            self.abort_all();
            return Err(e);
        }
        Ok(SinkReport { rows: self.rows, bytes: self.done_bytes, files: std::mem::take(&mut self.files) })
    }

    fn abort(mut self: Box<Self>) -> Vec<PathBuf> {
        self.abort_all()
    }

    /// Bytes of the completed parts plus the open part's progress value, as a high-water mark
    /// (never decreases between calls).
    fn bytes_out(&self) -> u64 {
        self.reported_bytes.max(self.live_bytes())
    }

    fn files_so_far(&self) -> u32 {
        self.opened
    }
}

// ---------------------------------------------------------------------------------------------
// Stack assembly
// ---------------------------------------------------------------------------------------------

/// Create (or truncate) `path` for writing, honouring the overwrite rule.
fn create_file(path: &Path, overwrite: bool) -> Result<File> {
    if !overwrite && path.exists() {
        return Err(CoreError::Io(format!("{} already exists", path.display())));
    }
    let mut options = OpenOptions::new();
    options.write(true);
    if overwrite {
        options.create(true).truncate(true);
    } else {
        options.create_new(true);
    }
    options.open(path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::AlreadyExists {
            CoreError::Io(format!("{} already exists", path.display()))
        } else {
            CoreError::Io(format!("cannot create {}: {e}", path.display()))
        }
    })
}

/// Open the single-file sink for file number `n` of `target` (csv / csv+compression / parquet).
pub fn open_single(target: &SinkTarget, n: u32) -> Result<Box<dyn Sink>> {
    open_single_calibrated(target, n, &SizeCalibration::new())
}

/// [`open_single`] for one part of a multi-file output: a Parquet sink shares `calibration`
/// with the other parts, so its buffered row group is sized with the compression ratio they
/// measured (see [`SizeCalibration`]). CSV sinks ignore it.
pub fn open_single_calibrated(target: &SinkTarget, n: u32, calibration: &SizeCalibration) -> Result<Box<dyn Sink>> {
    let path = target.path_for(n);
    let overwrite = target.opts.overwrite;
    match target.opts.format {
        OutputFormat::Csv => {
            let csv_opts = target.opts.csv.clone().unwrap_or_default();
            CsvSink::<File>::validate(&csv_opts)?;
            match target.opts.compress {
                None => {
                    let file = create_file(&path, overwrite)?;
                    Ok(Box::new(CsvSink::new(file, path, csv_opts)))
                }
                Some(kind) => {
                    let file = create_file(&path, overwrite)?;
                    let writer = CompressWriter::new(file, kind)?;
                    Ok(Box::new(CsvSink::compressed(writer, path, csv_opts)))
                }
            }
        }
        OutputFormat::Parquet => {
            if !overwrite && path.exists() {
                return Err(CoreError::Io(format!("{} already exists", path.display())));
            }
            let mut popts = target.opts.parquet.clone().unwrap_or_default();
            // A part must be able to close near its MB limit: a row group larger than the part
            // would only be flushed (and counted on disk) long after the limit was passed.
            if let Some(split_mb) = target.opts.split.as_ref().and_then(|s| s.megabytes).filter(|m| *m > 0) {
                let cap = u32::try_from(split_mb).unwrap_or(u32::MAX);
                popts.row_group_mb = popts.row_group_mb.min(cap);
            }
            Ok(Box::new(ParquetSink::new(path, popts).with_overwrite(overwrite).with_calibration(calibration.clone())))
        }
    }
}

/// Build the full sink stack for `target` (split -> csv/parquet -> compress -> file).
///
/// With `OutputOpts.split.rows` or `.megabytes` set, files are `<stem>_part0001<ext>`,
/// `_part0002`, ...; otherwise a single `<stem><ext>`. No file is created until the first
/// write (or `finish`).
pub fn build_stack(target: &SinkTarget) -> Result<Box<dyn Sink>> {
    if target.opts.format == OutputFormat::Csv {
        CsvSink::<File>::validate(&target.opts.csv.clone().unwrap_or_default())?;
    }
    let t = target.clone();
    let calibration = SizeCalibration::new();
    let factory: SinkFactory = Box::new(move |n| open_single_calibrated(&t, n, &calibration));
    let t = target.clone();
    let path_fn: PathFn = Box::new(move |n| t.path_for(n));
    let split = target.opts.split.as_ref().filter(|s| s.rows.is_some_and(|r| r > 0) || s.megabytes.is_some_and(|m| m > 0));
    let sink = match split {
        Some(s) => SplitSink::new(factory, s.rows, s.megabytes),
        None => SplitSink::single(factory),
    };
    Ok(Box::new(sink.with_path_fn(path_fn)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn row_boundary_counts_outside_quotes() {
        let data = b"a,\"x\ny\"\r\nb,c\n\"q\"\"\n\",d\nlast\n";
        assert_eq!(raw_csv_row_boundary(data, 0), 0);
        assert_eq!(raw_csv_row_boundary(data, 1), 9);
        assert_eq!(&data[..raw_csv_row_boundary(data, 1)], b"a,\"x\ny\"\r\n");
        assert_eq!(&data[raw_csv_row_boundary(data, 1)..raw_csv_row_boundary(data, 2)], b"b,c\n");
        assert_eq!(&data[raw_csv_row_boundary(data, 2)..raw_csv_row_boundary(data, 3)], b"\"q\"\"\n\",d\n");
        assert_eq!(raw_csv_row_boundary(data, 4), data.len());
        assert_eq!(raw_csv_row_boundary(data, 99), data.len());
    }

    #[test]
    fn row_boundary_without_trailing_newline() {
        assert_eq!(raw_csv_row_boundary(b"a\nb", 2), 3);
        assert_eq!(raw_csv_row_boundary(b"", 1), 0);
    }
}
