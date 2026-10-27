//! `Sink`: write batches to files. Sinks run on a dedicated blocking thread; the pipeline
//! (`job.rs`) feeds them from the bounded channel.
//!
//! * [`csv::CsvSink`] — RawCsv bytes pass through; Arrow batches are formatted with the `csv` crate.
//! * [`parquet::ParquetSink`] — Arrow only, 128 MB row groups, ZSTD by default.
//! * [`compress::CompressWriter`] — gzip / zstd on a separate thread behind another bounded channel.
//! * [`split::SplitSink`] — opens a new file every N rows or M bytes (Excel preset: 1,000,000 rows).
//!
//! [`build`] assembles the stack from `OutputOpts`.

use std::path::{Path, PathBuf};

use crate::error::Result;
use crate::source::Batch;
use crate::types::OutputOpts;

pub mod compress;
pub mod csv;
pub mod naming;
pub mod parquet;
pub mod split;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SinkReport {
    pub rows: u64,
    /// Bytes written to disk (after compression).
    pub bytes: u64,
    pub files: Vec<PathBuf>,
}

/// A file writer at the end of the pipeline.
///
/// Failure rule, followed by every sink in this module (csv, parquet, split, and the
/// compression layer under them): **no file of a run that did not complete keeps its final
/// name.** Whenever a sink gives up — [`Sink::finish`] returning `Err`, or [`Sink::abort`] —
/// every file this sink created is closed first and then renamed to `<name>.partial` (see
/// [`to_partial`]), so a truncated CSV or a Parquet file without a footer can never be mistaken
/// for a finished export. A new sink implementation must follow the same rule.
pub trait Sink: Send {
    /// Append one batch. On `Err` the pipeline stops writing and calls [`Sink::abort`].
    fn write(&mut self, batch: Batch) -> Result<()>;

    /// Flush, close and return what was written; on `Ok` every file carries its final name and
    /// is complete (an empty result still gets its CSV header / Parquet schema).
    ///
    /// On `Err` (final flush on a full disk, compressor trailer, Parquet footer, ...) every file
    /// this sink created is closed and renamed to `.partial` before the error is returned. The
    /// pipeline does not call [`Sink::abort`] after a failed `finish`, so the sink must do it
    /// itself.
    fn finish(self: Box<Self>) -> Result<SinkReport>;

    /// Give up (cancel, source error, failed write): every file this sink created is closed
    /// and renamed to `<name>.partial`, including files an earlier failure already renamed.
    /// Returns the `.partial` paths. Never fails; renames are best effort.
    fn abort(self: Box<Self>) -> Vec<PathBuf>;

    /// Bytes written to disk so far (progress counter).
    fn bytes_out(&self) -> u64;

    /// Files opened so far (progress counter).
    fn files_so_far(&self) -> u32;
}

/// Everything a sink needs to know about the target.
#[derive(Debug, Clone)]
pub struct SinkTarget {
    /// Output directory (must exist and be writable; preflight checks it).
    pub dir: PathBuf,
    /// File stem without extension (already expanded from the name pattern).
    pub stem: String,
    pub opts: OutputOpts,
}

impl SinkTarget {
    /// Extension chain, e.g. `.csv`, `.csv.gz`, `.parquet`.
    pub fn extension(&self) -> String {
        naming::extension(&self.opts)
    }

    /// Path of the n-th file (`n` is 1-based). Single-file outputs use `n = 0` (no suffix).
    pub fn path_for(&self, n: u32) -> PathBuf {
        let name = if n == 0 { format!("{}{}", self.stem, self.extension()) } else { format!("{}_part{:04}{}", self.stem, n, self.extension()) };
        self.dir.join(name)
    }
}

/// Build the sink stack described by `opts`. Does not create files until the first write.
pub fn build(target: &SinkTarget) -> Result<Box<dyn Sink>> {
    split::build_stack(target)
}

/// Rename `path` to `path.partial` (keeping the existing extension), best effort.
pub fn to_partial(path: &Path) -> PathBuf {
    let mut name = path.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
    name.push_str(".partial");
    let partial = path.with_file_name(name);
    let _ = std::fs::rename(path, &partial);
    partial
}
