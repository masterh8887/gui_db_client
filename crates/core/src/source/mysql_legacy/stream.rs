//! Extraction over the pre-4.1 text protocol. Owner: **legacy**.
//!
//! * The connection is taken out of the database's slot and moved into a reader task (like
//!   the modern driver); the slot is refilled only when the result set was read to the end.
//! * SQL: `sqlguard` already accepted the UTF-8 text; it is checked once more with
//!   `has_statement_separator` (4.0 lexing defaults), checked for characters
//!   `facts.plan.sql_charset()` cannot represent (`INVALID` naming the line / column in the
//!   user's SQL, before the tag shifts it), tagged with `tag_sql`, encoded and sent as one
//!   `COM_QUERY`. A 4.0 server has no multi-statement support, so `;`-separated text is a
//!   syntax error server-side as well.
//! * Columns: `mysql_common::column_kind_raw` -> `ArrowBatcher` (64K rows / 8 MB per batch,
//!   `fetch_batch_rows` honoured); cells via `mysql_common::text_cell` with
//!   `facts.column_charset(None)`. One reused row buffer; nothing else is retained.
//! * Internal channel depth 2 (like the modern driver); the pipeline adds its own 8.
//! * Cancel: stop reading, drop the socket, spawn `kill_from_side` (`KILL <id>`, which ends
//!   the session on 4.x) and return `Cancelled` at once (well inside the 2 s budget); the
//!   database reconnects lazily for its next use. A consumer that goes away (batch receiver
//!   dropped after a sink failure, stream or `start()` future dropped) is handled the same way:
//!   every wait on the server also races `batch_tx.closed()`, so a slow server scan never keeps
//!   the task, the socket or the server thread alive after the job.
//! * The number of text values that contained invalid bytes is logged once at the end
//!   (count only, never values).

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use arrow_schema::SchemaRef;
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio_util::sync::CancellationToken;

use super::conn::{encode_text, LegacyConn, QueryStart};
use super::protocol::split_text_row;
use super::{spawn_kill, MySqlLegacyDatabase};
use crate::error::{CoreError, Result};
use crate::source::arrow_batch::{schema_from, ArrowBatcher, Cell, ColKind};
use crate::source::mysql::{has_statement_separator, tag_sql, trim_statement, ValueParser};
use crate::source::mysql_common::{column_kind_raw, text_cell, ServerFacts};
use crate::source::{Batch, ConnectParams, Query, SourceStream};
use crate::types::SessionOpts;
use crate::BATCH_ROWS;

/// Batches buffered between the reader task and the pipeline (the pipeline adds its own).
const READER_CHANNEL_DEPTH: usize = 2;
/// How long the reader waits to hand its final error to a consumer that stopped polling.
const ERROR_HANDOFF: Duration = Duration::from_secs(5);

/// Everything the reader task needs besides the connection.
struct StreamCtx {
    facts: ServerFacts,
    params: ConnectParams,
    slot: Arc<Mutex<Option<LegacyConn>>>,
    streaming: Arc<AtomicBool>,
    cancel: CancellationToken,
    max_rows: usize,
    /// Byte limit of one batch (`Query::batch_bytes`).
    max_bytes: usize,
    /// U+FFFD produced by decoding (`SourceStream.replaced`).
    replaced: Arc<AtomicU64>,
}

/// Start the extraction stream (see the module docs).
pub(crate) async fn start(db: &MySqlLegacyDatabase, q: &Query, opts: &SessionOpts, cancel: CancellationToken) -> Result<SourceStream> {
    let facts = &db.facts;
    // Checks that need no server run before the session is taken.
    if has_statement_separator(trim_statement(&q.sql), facts.lex) {
        return Err(CoreError::Invalid("the extraction SQL must be a single SELECT statement (MySQL 4.x runs no statement batches)".into()));
    }
    // Before tagging, so an unrepresentable character is reported where the user typed it.
    facts.check_sql_text(&q.sql)?;
    let sql = tag_sql(&facts.application_name, &q.sql);
    let sql = encode_text(facts.plan.sql_charset(), &sql)?.into_owned();
    let max_rows = opts.fetch_batch_rows.map(|n| (n as usize).clamp(1, BATCH_ROWS)).unwrap_or(BATCH_ROWS);

    let conn = db.take_for_stream().await?;
    db.current_id.store(conn.connection_id(), Ordering::SeqCst);
    let replaced = Arc::new(AtomicU64::new(0));
    let ctx = StreamCtx {
        facts: facts.clone(),
        params: db.params.clone(),
        slot: db.conn.clone(),
        streaming: db.streaming.clone(),
        cancel,
        max_rows,
        max_bytes: q.batch_bytes(),
        replaced: replaced.clone(),
    };
    let (schema_tx, schema_rx) = oneshot::channel();
    let (batch_tx, batch_rx) = mpsc::channel::<Result<Batch>>(READER_CHANNEL_DEPTH);
    tokio::spawn(reader_task(conn, sql, ctx, schema_tx, batch_tx));
    let schema = match schema_rx.await {
        Ok(r) => r?,
        Err(_) => return Err(CoreError::Internal("mysql reader task ended before reporting the result schema".into())),
    };
    let stream = futures::stream::unfold(batch_rx, |mut rx| async move { rx.recv().await.map(|b| (b, rx)) });
    // A pre-4.1 server is never UTF-8 on the wire: every text column is decoded.
    Ok(SourceStream { schema: Some(schema), stream: Box::pin(stream), replaced: Some(replaced) })
}

/// Reader task body: runs the query, streams rows into batches, then settles the session
/// (back into the slot when idle, else dropped) and reports the outcome.
async fn reader_task(mut conn: LegacyConn, sql: Vec<u8>, ctx: StreamCtx, schema_tx: oneshot::Sender<Result<SchemaRef>>, batch_tx: mpsc::Sender<Result<Batch>>) {
    let mut schema_tx = Some(schema_tx);
    let mut aborted = false;
    let outcome = run(&mut conn, &sql, &ctx, &mut schema_tx, &batch_tx, &mut aborted).await;
    let id = conn.connection_id();
    {
        let mut slot = ctx.slot.lock().await;
        if !aborted && conn.is_idle() {
            *slot = Some(conn);
        } else {
            // Half-read result or cancel: closing the socket stops the server from sending.
            drop(conn);
        }
        ctx.streaming.store(false, Ordering::SeqCst);
    }
    if aborted {
        // The 4.x server may still be sorting / scanning without writing: end the thread.
        spawn_kill(ctx.params.clone(), ctx.facts.clone(), id);
    }
    if let Err(e) = outcome {
        if let Some(tx) = schema_tx.take() {
            let _ = tx.send(Err(e));
        } else {
            let _ = tokio::time::timeout(ERROR_HANDOFF, batch_tx.send(Err(e))).await;
        }
    }
}

/// Send a batch unless the job is cancelled or the pipeline went away.
async fn deliver(ctx: &StreamCtx, tx: &mpsc::Sender<Result<Batch>>, batch: Batch, aborted: &mut bool) -> Result<()> {
    tokio::select! {
        biased;
        _ = ctx.cancel.cancelled() => {
            *aborted = true;
            Err(CoreError::Cancelled)
        }
        r = tx.send(Ok(batch)) => match r {
            Ok(()) => Ok(()),
            Err(_) => {
                *aborted = true;
                Err(CoreError::Cancelled)
            }
        },
    }
}

async fn run(
    conn: &mut LegacyConn,
    sql: &[u8],
    ctx: &StreamCtx,
    schema_tx: &mut Option<oneshot::Sender<Result<SchemaRef>>>,
    batch_tx: &mpsc::Sender<Result<Batch>>,
    aborted: &mut bool,
) -> Result<()> {
    let start = tokio::select! {
        biased;
        _ = ctx.cancel.cancelled() => {
            *aborted = true;
            return Err(CoreError::Cancelled);
        }
        // `start()` was dropped while the server was still sorting: nobody waits any more.
        _ = batch_tx.closed() => {
            *aborted = true;
            return Err(CoreError::Cancelled);
        }
        r = conn.query(sql) => r?,
    };
    let defs = match start {
        QueryStart::Rows(defs) => defs,
        QueryStart::Done(_) => {
            // Not a SELECT after all (sqlguard should have refused it): an empty result.
            if let Some(tx) = schema_tx.take() {
                let _ = tx.send(Ok(schema_from(&[])));
            }
            return Ok(());
        }
    };

    let n = defs.len();
    let mut named: Vec<(String, ColKind)> = Vec::with_capacity(n);
    let mut kinds: Vec<ColKind> = Vec::with_capacity(n);
    let mut parsers: Vec<ValueParser> = Vec::with_capacity(n);
    let mut charsets = Vec::with_capacity(n);
    for def in &defs {
        let (kind, parser) = column_kind_raw(def);
        named.push((def.name.clone(), kind));
        kinds.push(kind);
        parsers.push(parser);
        charsets.push(ctx.facts.column_charset(def.collation));
    }
    drop(defs);
    let schema = schema_from(&named);
    if let Some(tx) = schema_tx.take() {
        let _ = tx.send(Ok(schema.clone()));
    }
    let mut batcher = ArrowBatcher::with_limits(schema, kinds, ctx.max_rows, ctx.max_bytes);
    let mut scratch = String::new();
    let mut ranges: Vec<Option<(usize, usize)>> = Vec::with_capacity(n);
    let mut replaced: u64 = 0;

    loop {
        let row = tokio::select! {
            biased;
            _ = ctx.cancel.cancelled() => {
                *aborted = true;
                return Err(CoreError::Cancelled);
            }
            // The consumer dropped the stream (e.g. after a sink failure) while the server is
            // slow to send the next row: stop now; the caller drops the socket and kills the
            // server thread exactly as for a cancel.
            _ = batch_tx.closed() => {
                *aborted = true;
                return Err(CoreError::Cancelled);
            }
            r = conn.next_row() => r?,
        };
        let Some(row) = row else { break };
        split_text_row(row, n, &mut ranges)?;
        for (i, range) in ranges.iter().enumerate() {
            let raw = range.map(|(a, b)| &row[a..b]);
            let cell = text_cell(parsers[i], raw, charsets[i], &mut scratch, i)?;
            replaced += cell.replaced as u64;
            if cell.replaced {
                let n = match &cell.cell {
                    Cell::Str(s) => s.matches('\u{FFFD}').count() as u64,
                    _ => 1,
                };
                ctx.replaced.fetch_add(n, Ordering::Relaxed);
            }
            batcher.push(cell.cell)?;
        }
        if batcher.end_row()? {
            if let Some(b) = batcher.flush()? {
                deliver(ctx, batch_tx, Batch::Arrow(b), aborted).await?;
            }
        }
    }
    if let Some(b) = batcher.flush()? {
        deliver(ctx, batch_tx, Batch::Arrow(b), aborted).await?;
    }
    if replaced > 0 {
        tracing::warn!(
            "mysql {}:{}: {replaced} text values contained bytes that are invalid in {}; they were replaced with U+FFFD",
            ctx.facts.host,
            ctx.facts.port,
            ctx.facts.plan.sql_charset().label()
        );
    }
    Ok(())
}
