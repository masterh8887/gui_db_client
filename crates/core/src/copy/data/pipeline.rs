//! The reader half of a table copy (4.1): a source stream is forwarded batch by batch to the
//! writer's `load` through a channel of depth 1, each batch carrying its share of the 16 MiB
//! in-flight budget (`Budgeted.permit`). Owner: copy-data.
//!
//! * Memory: the reader holds at most the batch it is sending; the writer drops the permit after
//!   encoding or forwarding. Permits are KiB units of a [`Budget`] shared by the job's readers.
//! * Optional transcoding of PostgreSQL SQL_ASCII raw CSV (3.5) with a streaming decoder.
//! * Optional source-side checksum (5.6) of what is written: canonical Arrow cells (SQLite),
//!   normalised MySQL fields, or raw CSV records (PG).
//! * Cancellation: the token stops forwarding; the source stream is polled one last time so the
//!   driver can cancel its query, then dropped.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use futures::StreamExt;
use tokio::sync::{mpsc, Semaphore};
use tokio_util::sync::CancellationToken;

use crate::copy::data::cells::checksum_batch;
use crate::copy::hash::{RowHasher, TableChecksum};
use crate::copy::types::*;
use crate::error::{CoreError, Result};
use crate::source::{Batch, SourceStream};

/// In-flight budget of one job (4.1): 16 MiB in KiB permits.
pub const BUDGET_BYTES: usize = 16 << 20;
const PERMIT_UNIT: usize = 1024;

/// The job-wide in-flight byte budget.
#[derive(Clone)]
pub struct Budget {
    sem: Arc<Semaphore>,
    /// Permits one reader may hold at most (`16 MiB / concurrency`).
    per_reader: u32,
}

impl Budget {
    pub fn new(concurrency: u8) -> Budget {
        let total = (BUDGET_BYTES / PERMIT_UNIT) as u32;
        Budget { sem: Arc::new(Semaphore::new(total as usize)), per_reader: (total / concurrency.max(1) as u32).max(1) }
    }

    /// Permits for a batch of `bytes`: `min(bytes, 16 MiB / c)` in KiB, at least 1.
    pub fn permits_for(&self, bytes: u64) -> u32 {
        (bytes.div_ceil(PERMIT_UNIT as u64) as u32).clamp(1, self.per_reader)
    }

    pub async fn acquire(&self, bytes: u64) -> Result<tokio::sync::OwnedSemaphorePermit> {
        self.sem.clone().acquire_many_owned(self.permits_for(bytes)).await.map_err(|_| CoreError::Internal("copy budget closed".into()))
    }

    pub fn available(&self) -> usize {
        self.sem.available_permits()
    }
}

/// Where the reader sends.
pub enum Feed {
    Raw(mpsc::Sender<Budgeted<Bytes>>),
    Arrow(mpsc::Sender<Budgeted<arrow_array::RecordBatch>>),
}

/// The channel pair of one load (depth 1).
pub fn channel(raw_csv: bool) -> (Feed, LoadInput) {
    if raw_csv {
        let (tx, rx) = mpsc::channel(1);
        (Feed::Raw(tx), LoadInput::RawCsv(rx))
    } else {
        let (tx, rx) = mpsc::channel(1);
        (Feed::Arrow(tx), LoadInput::Arrow(rx))
    }
}

/// How the reader computes the source-side checksum.
pub enum ChecksumMode {
    /// Canonical Arrow cells (SQLite: values are written as read).
    Canonical,
    /// PG raw CSV records.
    RawCsv,
}

/// Streaming transcoder of SQL_ASCII raw CSV into UTF-8 (3.5).
pub struct Transcoder {
    decoder: encoding_rs::Decoder,
    replaced: u64,
}

impl Transcoder {
    pub fn new(enc: PgSourceEncoding) -> Transcoder {
        let e = match enc {
            PgSourceEncoding::EucKr => encoding_rs::EUC_KR,
            PgSourceEncoding::Latin1 => encoding_rs::WINDOWS_1252,
            PgSourceEncoding::Utf8 => encoding_rs::UTF_8,
        };
        Transcoder { decoder: e.new_decoder_without_bom_handling(), replaced: 0 }
    }

    /// Decode one chunk (a multi-byte character split across chunks is carried over).
    pub fn push(&mut self, input: &[u8], last: bool) -> Bytes {
        let mut out = String::with_capacity(self.decoder.max_utf8_buffer_length(input.len()).unwrap_or(input.len() * 3));
        let (_, _, had) = self.decoder.decode_to_string(input, &mut out, last);
        if had {
            self.replaced += out.chars().filter(|c| *c == '\u{FFFD}').count() as u64;
        }
        Bytes::from(out.into_bytes())
    }

    pub fn replaced(&self) -> u64 {
        self.replaced
    }
}

/// Quote-aware CSV record hasher (PG raw CSV, `"` quoting): records end at LF outside quotes;
/// a record split across chunks is carried over.
#[derive(Default)]
pub struct CsvRecordHasher {
    carry: Vec<u8>,
    in_quotes: bool,
    pub sum: TableChecksum,
}

impl CsvRecordHasher {
    pub fn push(&mut self, data: &[u8]) {
        let mut start = 0;
        for (i, &b) in data.iter().enumerate() {
            match b {
                b'"' => self.in_quotes = !self.in_quotes,
                b'\n' if !self.in_quotes => {
                    let mut h = RowHasher::default();
                    if self.carry.is_empty() {
                        h.cell(Some(&data[start..i]));
                    } else {
                        self.carry.extend_from_slice(&data[start..i]);
                        h.cell(Some(&self.carry));
                        self.carry.clear();
                    }
                    self.sum.add_row(h.finish());
                    start = i + 1;
                }
                _ => {}
            }
        }
        self.carry.extend_from_slice(&data[start..]);
    }

    pub fn finish(mut self) -> TableChecksum {
        if !self.carry.is_empty() {
            let mut h = RowHasher::default();
            h.cell(Some(&self.carry));
            self.sum.add_row(h.finish());
            self.carry.clear();
        }
        self.sum
    }
}

/// What the reader saw.
#[derive(Debug, Default, Clone)]
pub struct ReadOutcome {
    pub rows: u64,
    pub bytes: u64,
    /// U+FFFD produced by decoding a non-UTF-8 source (`SourceStream.replaced`) or by the
    /// SQL_ASCII transcoder.
    pub replaced: u64,
    pub checksum: Option<TableChecksum>,
}

/// Options of [`feed`].
pub struct FeedOpts<'a> {
    pub spec: &'a TableDataSpec,
    pub budget: &'a Budget,
    pub checksum: Option<ChecksumMode>,
    pub cancel: CancellationToken,
    /// `on_unmappable: fail`: the first U+FFFD produced by decoding fails the table.
    pub fail_on_replaced: bool,
}

/// Forward `stream` into `feed`. `on_batch(rows, bytes)` is called after each batch was handed
/// to the writer. Returns when the stream ends, the writer hung up (its error wins, so this
/// returns `Ok` with what was sent), or the token fired (`Cancelled`).
pub async fn feed(stream: SourceStream, feed: Feed, opts: FeedOpts<'_>, on_batch: impl FnMut(u64, u64)) -> Result<ReadOutcome> {
    let r = feed_inner(stream, &feed, &opts, on_batch).await;
    if r.is_err() {
        // The writer must see the abnormal end before the channel closes.
        opts.cancel.cancel();
    }
    drop(feed);
    r
}

async fn feed_inner(stream: SourceStream, feed: &Feed, opts: &FeedOpts<'_>, mut on_batch: impl FnMut(u64, u64)) -> Result<ReadOutcome> {
    let SourceStream { stream, replaced, .. } = stream;
    let mut stream = stream;
    let mut out = ReadOutcome::default();
    let mut transcoder = opts.spec.transcode.map(Transcoder::new);
    let mut raw_hasher = matches!(opts.checksum, Some(ChecksumMode::RawCsv)).then(CsvRecordHasher::default);
    let mut canon = matches!(opts.checksum, Some(ChecksumMode::Canonical)).then(TableChecksum::default);
    let cancel = opts.cancel.clone();
    loop {
        let next = tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                // Let the driver send its server-side cancel, then drop the stream.
                let _ = tokio::time::timeout(Duration::from_millis(500), stream.next()).await;
                return Err(CoreError::Cancelled);
            }
            n = stream.next() => n,
        };
        let batch = match next {
            None => break,
            Some(Err(e)) => return Err(e),
            Some(Ok(b)) => b,
        };
        let (rows, sent_bytes, sent) = match (batch, feed) {
            (Batch::RawCsvHeader(_), _) => continue,
            (Batch::RawCsv { data, rows }, Feed::Raw(tx)) => {
                let data = match transcoder.as_mut() {
                    Some(t) => t.push(&data, false),
                    None => data,
                };
                if let Some(h) = raw_hasher.as_mut() {
                    h.push(&data);
                }
                let n = data.len() as u64;
                let permit = tokio::select! {
                    biased;
                    _ = cancel.cancelled() => return Err(CoreError::Cancelled),
                    p = opts.budget.acquire(n) => p?,
                };
                let ok = tokio::select! {
                    biased;
                    _ = cancel.cancelled() => return Err(CoreError::Cancelled),
                    r = tx.send(Budgeted { item: data, rows, permit }) => r.is_ok(),
                };
                (rows, n, ok)
            }
            (Batch::Arrow(b), Feed::Arrow(tx)) => {
                if let Some(sum) = canon.as_mut() {
                    checksum_batch(&b, sum)?;
                }
                let rows = b.num_rows() as u64;
                let n = b.get_array_memory_size() as u64;
                let permit = tokio::select! {
                    biased;
                    _ = cancel.cancelled() => return Err(CoreError::Cancelled),
                    p = opts.budget.acquire(n) => p?,
                };
                let ok = tokio::select! {
                    biased;
                    _ = cancel.cancelled() => return Err(CoreError::Cancelled),
                    r = tx.send(Budgeted { item: b, rows, permit }) => r.is_ok(),
                };
                (rows, n, ok)
            }
            (Batch::RawCsv { .. }, Feed::Arrow(_)) | (Batch::Arrow(_), Feed::Raw(_)) => {
                return Err(CoreError::Internal("copy: the source stream format does not match the writer".into()));
            }
        };
        if !sent {
            // The writer stopped (its own error is the result of the load).
            break;
        }
        out.rows += rows;
        if opts.fail_on_replaced {
            let n = replaced.as_ref().map(|r| r.load(Ordering::Relaxed)).unwrap_or(0) + transcoder.as_ref().map(|t| t.replaced()).unwrap_or(0);
            if n > 0 {
                return Err(crate::copy::copy_fail(
                    crate::copy::CODE_UNMAPPABLE,
                    format!("{n} characters of {} could not be decoded from the source character set", opts.spec.target.name),
                ));
            }
        }
        out.bytes += sent_bytes;
        on_batch(rows, sent_bytes);
    }
    if let (Some(t), Feed::Raw(tx)) = (transcoder.as_mut(), feed) {
        let tail = t.push(&[], true);
        if !tail.is_empty() {
            if let Some(h) = raw_hasher.as_mut() {
                h.push(&tail);
            }
            let permit = opts.budget.acquire(tail.len() as u64).await?;
            let _ = tx.send(Budgeted { item: tail, rows: 0, permit }).await;
        }
    }
    out.replaced = replaced.map(|r| r.load(Ordering::Relaxed)).unwrap_or(0) + transcoder.map(|t| t.replaced()).unwrap_or(0);
    out.checksum = raw_hasher.map(CsvRecordHasher::finish).or(canon);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budget_permits() {
        let b = Budget::new(4);
        assert_eq!(b.permits_for(0), 1);
        assert_eq!(b.permits_for(1), 1);
        assert_eq!(b.permits_for(1025), 2);
        assert_eq!(b.permits_for(100 << 20), (16 << 10) / 4, "capped at 16 MiB / c");
        assert_eq!(b.available(), 16 << 10);
    }

    #[tokio::test]
    async fn permits_are_released_after_the_writer_drops_them() {
        let b = Budget::new(1);
        let p = b.acquire(8 << 20).await.unwrap();
        assert_eq!(b.available(), 8 << 10);
        drop(p);
        assert_eq!(b.available(), 16 << 10);
        // A full budget blocks the next reader until a permit is dropped.
        let all = b.acquire(16 << 20).await.unwrap();
        let blocked = tokio::time::timeout(Duration::from_millis(50), b.acquire(1)).await;
        assert!(blocked.is_err());
        drop(all);
        assert!(tokio::time::timeout(Duration::from_millis(50), b.acquire(1)).await.is_ok());
    }

    #[test]
    fn csv_records_hash_independently_of_chunking() {
        let data = b"1,\"a\nb\",x\n2,\"\"\"q\"\"\",\n3,,\n";
        let mut whole = CsvRecordHasher::default();
        whole.push(data);
        let whole = whole.finish();
        assert_eq!(whole.rows, 3);
        for split in 1..data.len() {
            let mut h = CsvRecordHasher::default();
            h.push(&data[..split]);
            h.push(&data[split..]);
            assert_eq!(h.finish(), whole, "split at {split}");
        }
    }

    #[test]
    fn transcoder_keeps_split_characters() {
        let (bytes, _, _) = encoding_rs::EUC_KR.encode("가,\"나\"\n");
        let mut t = Transcoder::new(PgSourceEncoding::EucKr);
        let mut out = Vec::new();
        for b in bytes.iter() {
            out.extend_from_slice(&t.push(std::slice::from_ref(b), false));
        }
        out.extend_from_slice(&t.push(&[], true));
        assert_eq!(String::from_utf8(out).unwrap(), "가,\"나\"\n");
        assert_eq!(t.replaced(), 0);
        let mut t = Transcoder::new(PgSourceEncoding::Utf8);
        let s = t.push(b"a\xffb", true);
        assert_eq!(&s[..], "a\u{FFFD}b".as_bytes());
        assert_eq!(t.replaced(), 1);
    }
}
