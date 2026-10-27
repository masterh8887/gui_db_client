//! Compressing writer: gzip / zstd on its own thread behind a bounded channel so the CSV
//! formatter and the compressor overlap. Counts compressed bytes written to disk.
//!
//! Memory stays fixed: at most [`CHUNK_QUEUE`] chunks of [`CHUNK_BYTES`] wait in the channel
//! (8 MB), plus the encoder's own window.
//!
//! Also home of [`CountingWriter`], the byte counter every single-file sink puts directly in
//! front of the file so that `bytes_out` is the on-disk size.

use std::fs::File;
use std::io::{self, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::sync::Arc;
use std::thread::JoinHandle;

use crate::error::{CoreError, Result};
use crate::types::Compression;

/// Largest chunk handed to the compressor thread in one message.
pub const CHUNK_BYTES: usize = 1024 * 1024;
/// Chunks that may wait between the formatter and the compressor.
pub const CHUNK_QUEUE: usize = 8;
/// gzip level (flate2 scale 0-9).
pub const GZIP_LEVEL: u32 = 6;
/// zstd level (1-22).
pub const ZSTD_LEVEL: i32 = 3;

// ---------------------------------------------------------------------------------------------
// CountingWriter
// ---------------------------------------------------------------------------------------------

/// A `Write` adapter that adds every byte passed through it to a shared counter.
///
/// The counter is an `Arc<AtomicU64>` so a sink can hand it out for progress reporting while
/// the writer is owned by a `BufWriter` or a compressor thread.
pub struct CountingWriter<W> {
    inner: W,
    count: Arc<AtomicU64>,
}

impl<W: Write> CountingWriter<W> {
    /// Wrap `inner` with a fresh counter.
    pub fn new(inner: W) -> Self {
        CountingWriter { inner, count: Arc::new(AtomicU64::new(0)) }
    }

    /// Wrap `inner` and add to an existing counter.
    pub fn with_counter(inner: W, count: Arc<AtomicU64>) -> Self {
        CountingWriter { inner, count }
    }

    /// Shared handle to the counter.
    pub fn counter(&self) -> Arc<AtomicU64> {
        self.count.clone()
    }

    /// Bytes passed through so far.
    pub fn bytes(&self) -> u64 {
        self.count.load(Ordering::Relaxed)
    }

    /// Unwrap the inner writer (nothing is flushed).
    pub fn into_inner(self) -> W {
        self.inner
    }
}

impl<W: Write> Write for CountingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.count.fetch_add(n as u64, Ordering::Relaxed);
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

// ---------------------------------------------------------------------------------------------
// CompressWriter
// ---------------------------------------------------------------------------------------------

/// The encoder running on the compressor thread.
enum Encoder {
    Gzip(flate2::write::GzEncoder<CountingWriter<File>>),
    Zstd(zstd::stream::write::Encoder<'static, CountingWriter<File>>),
}

impl Encoder {
    fn new(kind: Compression, out: CountingWriter<File>) -> io::Result<Encoder> {
        match kind {
            Compression::Gzip => Ok(Encoder::Gzip(flate2::write::GzEncoder::new(out, flate2::Compression::new(GZIP_LEVEL)))),
            Compression::Zstd => {
                // `Encoder::multithread` needs the `zstdmt` cargo feature, which the workspace
                // does not enable; the single-threaded encoder already runs on its own thread.
                let enc = zstd::stream::write::Encoder::new(out, ZSTD_LEVEL)?;
                Ok(Encoder::Zstd(enc))
            }
        }
    }

    fn write_all(&mut self, chunk: &[u8]) -> io::Result<()> {
        match self {
            Encoder::Gzip(e) => e.write_all(chunk),
            Encoder::Zstd(e) => e.write_all(chunk),
        }
    }

    /// Write the stream trailer and return the file writer.
    fn finish(self) -> io::Result<CountingWriter<File>> {
        match self {
            Encoder::Gzip(e) => e.finish(),
            Encoder::Zstd(e) => e.finish(),
        }
    }
}

/// Body of the compressor thread: drain the channel into the encoder, then finish the stream.
/// Returns the number of bytes on disk.
fn compress_loop(rx: Receiver<Vec<u8>>, file: File, kind: Compression, count: Arc<AtomicU64>) -> io::Result<u64> {
    let out = CountingWriter::with_counter(file, count.clone());
    let mut enc = Encoder::new(kind, out)?;
    for chunk in rx {
        enc.write_all(&chunk)?;
    }
    let mut out = enc.finish()?;
    out.flush()?;
    Ok(count.load(Ordering::Relaxed))
}

/// A `Write` that compresses on a background thread. `bytes_out` is the on-disk byte count.
///
/// Writes are cut into chunks of at most [`CHUNK_BYTES`] and sent through a bounded channel of
/// [`CHUNK_QUEUE`] entries; when the compressor falls behind, `write` blocks (back-pressure all
/// the way to the source). Errors raised on the compressor thread surface on the next `write`,
/// `flush` or [`CompressWriter::finish`].
pub struct CompressWriter {
    kind: Compression,
    tx: Option<SyncSender<Vec<u8>>>,
    worker: Option<JoinHandle<io::Result<u64>>>,
    /// First error reported by the compressor thread; repeated on every later call.
    failed: Option<String>,
    /// Compressed bytes written to the file so far.
    pub bytes_out: Arc<AtomicU64>,
}

impl CompressWriter {
    /// Start the compressor thread over `inner` (an already created, empty file).
    pub fn new(inner: File, kind: Compression) -> Result<Self> {
        let bytes_out = Arc::new(AtomicU64::new(0));
        let (tx, rx) = sync_channel::<Vec<u8>>(CHUNK_QUEUE);
        let count = bytes_out.clone();
        let worker = std::thread::Builder::new()
            .name(format!(
                "bulkdb-{}",
                match kind {
                    Compression::Gzip => "gzip",
                    Compression::Zstd => "zstd",
                }
            ))
            .spawn(move || compress_loop(rx, inner, kind, count))
            .map_err(|e| CoreError::Io(format!("cannot start compressor thread: {e}")))?;
        Ok(CompressWriter { kind, tx: Some(tx), worker: Some(worker), failed: None, bytes_out })
    }

    /// Which codec this writer runs.
    pub fn kind(&self) -> Compression {
        self.kind
    }

    /// Close the channel and wait for the thread; returns its result.
    fn join(&mut self) -> io::Result<u64> {
        drop(self.tx.take());
        match self.worker.take() {
            Some(handle) => match handle.join() {
                Ok(r) => r,
                Err(_) => Err(io::Error::other("compressor thread panicked")),
            },
            None => Err(io::Error::other("compressor already finished")),
        }
    }

    /// Turn a failed send (the thread is gone) into the thread's error and remember it.
    fn thread_error(&mut self) -> io::Error {
        if self.failed.is_none() {
            let msg = match self.join() {
                Ok(_) => "compressor thread exited early".to_string(),
                Err(e) => e.to_string(),
            };
            self.failed = Some(msg);
        }
        io::Error::other(format!("compression failed: {}", self.failed.as_deref().unwrap_or_default()))
    }

    /// Flush and finish the stream; returns bytes on disk.
    pub fn finish(mut self) -> Result<u64> {
        if let Some(msg) = self.failed.take() {
            return Err(CoreError::Io(format!("compression failed: {msg}")));
        }
        let bytes = self.join().map_err(|e| CoreError::Io(format!("compression failed: {e}")))?;
        Ok(bytes)
    }
}

impl Write for CompressWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if let Some(msg) = &self.failed {
            return Err(io::Error::other(format!("compression failed: {msg}")));
        }
        if buf.is_empty() {
            return Ok(0);
        }
        for chunk in buf.chunks(CHUNK_BYTES) {
            let sent = match &self.tx {
                Some(tx) => tx.send(chunk.to_vec()).is_ok(),
                None => return Err(io::Error::other("compressor already finished")),
            };
            if !sent {
                return Err(self.thread_error());
            }
        }
        Ok(buf.len())
    }

    /// Nothing is buffered here: every write already went to the channel. The compressed
    /// stream itself is only finalised by [`CompressWriter::finish`].
    fn flush(&mut self) -> io::Result<()> {
        match &self.failed {
            Some(msg) => Err(io::Error::other(format!("compression failed: {msg}"))),
            None => Ok(()),
        }
    }
}

impl Drop for CompressWriter {
    /// Close the channel and wait for the thread so the file handle is released (needed before
    /// the file can be renamed to `.partial` on Windows). Errors are ignored: the owner either
    /// already called `finish` or is aborting.
    fn drop(&mut self) {
        if self.worker.is_some() {
            let _ = self.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    #[test]
    fn counting_writer_counts() {
        let mut w = CountingWriter::new(Vec::new());
        w.write_all(b"hello").unwrap();
        w.write_all(b" world").unwrap();
        assert_eq!(w.bytes(), 11);
        assert_eq!(w.into_inner(), b"hello world");
    }

    #[test]
    fn gzip_round_trip_and_count() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.gz");
        let file = File::create(&path).unwrap();
        let mut w = CompressWriter::new(file, Compression::Gzip).unwrap();
        let payload: Vec<u8> = (0..3 * CHUNK_BYTES).map(|i| (i % 251) as u8).collect();
        w.write_all(&payload).unwrap();
        w.flush().unwrap();
        let on_disk = w.finish().unwrap();
        assert_eq!(on_disk, std::fs::metadata(&path).unwrap().len());
        let mut dec = flate2::read::GzDecoder::new(File::open(&path).unwrap());
        let mut back = Vec::new();
        dec.read_to_end(&mut back).unwrap();
        assert_eq!(back, payload);
    }

    #[test]
    fn zstd_round_trip_and_count() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.zst");
        let file = File::create(&path).unwrap();
        let mut w = CompressWriter::new(file, Compression::Zstd).unwrap();
        w.write_all(b"zstd payload ").unwrap();
        w.write_all(&vec![b'a'; CHUNK_BYTES + 17]).unwrap();
        let on_disk = w.finish().unwrap();
        assert_eq!(on_disk, std::fs::metadata(&path).unwrap().len());
        let back = zstd::stream::decode_all(File::open(&path).unwrap()).unwrap();
        assert_eq!(back.len(), 13 + CHUNK_BYTES + 17);
        assert!(back.starts_with(b"zstd payload "));
    }

    #[test]
    fn drop_without_finish_releases_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.gz");
        {
            let mut w = CompressWriter::new(File::create(&path).unwrap(), Compression::Gzip).unwrap();
            w.write_all(b"abc").unwrap();
        }
        let partial = crate::sink::to_partial(&path);
        assert!(partial.exists(), "file must be renameable after drop");
    }
}
