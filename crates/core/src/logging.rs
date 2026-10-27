//! tracing setup: daily log files in `<config>/logs`, an in-memory ring buffer for the UI log
//! panel, and a masking layer so passwords never reach any output. `mask_secrets` is unit
//! tested against `password=`, `PGPASSWORD`, DSN passwords and `Authorization` headers.
//!
//! Masking happens twice on purpose: [`RingLayer`] masks the message it hands to the UI, and
//! [`MaskingMakeWriter`] masks every line before it reaches the file appender. A driver that
//! logs a connection string by mistake therefore never leaks a password anywhere.

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::io::{self, Write};
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};

use regex::Regex;
use tokio::sync::broadcast;
use tracing::field::{Field, Visit};
use tracing::{Event, Subscriber};
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::{Context, Layer, SubscriberExt};
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::{EnvFilter, Registry};

use crate::error::Result;
use crate::types::{LogEvent, LogLevel};

pub const RING_CAPACITY: usize = 2000;

pub struct LogBuffer {
    ring: Mutex<VecDeque<LogEvent>>,
    tx: broadcast::Sender<LogEvent>,
}

impl Default for LogBuffer {
    fn default() -> Self {
        Self::new()
    }
}

impl LogBuffer {
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(1024);
        LogBuffer { ring: Mutex::new(VecDeque::with_capacity(RING_CAPACITY)), tx }
    }

    pub fn push(&self, ev: LogEvent) {
        let mut r = self.ring.lock().unwrap();
        if r.len() >= RING_CAPACITY {
            r.pop_front();
        }
        r.push_back(ev.clone());
        drop(r);
        let _ = self.tx.send(ev);
    }

    pub fn recent(&self, n: usize) -> Vec<LogEvent> {
        let r = self.ring.lock().unwrap();
        r.iter().rev().take(n).cloned().collect::<Vec<_>>().into_iter().rev().collect()
    }

    pub fn subscribe(&self) -> broadcast::Receiver<LogEvent> {
        self.tx.subscribe()
    }

    /// Convenience for code paths outside `tracing` (e.g. server NOTICE messages).
    pub fn log(&self, job_id: Option<&str>, level: LogLevel, message: impl Into<String>) {
        self.push(LogEvent { job_id: job_id.map(|s| s.to_string()), level, message: mask_secrets(&message.into()), at: crate::types::now_rfc3339(), target: None });
    }
}

// ---------------------------------------------------------------------------------------------
// Masking
// ---------------------------------------------------------------------------------------------

/// Replacement for a masked secret.
pub const MASK: &str = "***";

struct Patterns {
    /// `"password": "x"` (JSON / TOML-ish quoted keys).
    json: Regex,
    /// `password=x`, `Password: x`, `PGPASSWORD=x`, `pwd='x'`, `sshPassword = "x"` ...
    kv: Regex,
    /// `Authorization: Bearer x` / `Basic x`.
    auth: Regex,
    /// `sshpass -p x`.
    sshpass: Regex,
    /// `scheme://user:pass@host` and `user:pass@host` in DSNs.
    url: Regex,
}

fn patterns() -> &'static Patterns {
    static P: OnceLock<Patterns> = OnceLock::new();
    P.get_or_init(|| {
        // Keys: anything ending in password / passwd / passphrase / secret (sshPassword,
        // PGPASSWORD, client_secret ...), plus pwd, mysql_pwd, api keys and tokens.
        // Same family as `connect::is_connection_string_secret_key`: any key ending in
        // pwd / password / passwd / passphrase / token / api key / private key (optionally
        // followed by digits, e.g. PWD2), or containing "secret".
        const KEYS: &str = r"(?:[A-Za-z0-9_.-]*(?:password|passwd|passphrase|secret[A-Za-z0-9_.-]*|pwd|token|api[_-]?key|private[_-]?key)[0-9]*)";
        Patterns {
            json: Regex::new(&format!(r#"(?i)("{KEYS}"\s*:\s*")([^"]*)(")"#)).expect("json pattern"),
            kv: Regex::new(&format!(r#"(?i)(\b{KEYS}\s*[=:]\s*)(?:"([^"]*)"|'([^']*)'|([^\s;,&"']+))"#)).expect("kv pattern"),
            auth: Regex::new(r"(?i)(authorization\s*[:=]\s*(?:bearer|basic)\s+)(\S+)").expect("auth pattern"),
            sshpass: Regex::new(r#"(?i)(sshpass\s+-p\s*)(?:'([^']*)'|"([^"]*)"|(\S+))"#).expect("sshpass pattern"),
            url: Regex::new(r"(?i)(\b[A-Za-z][A-Za-z0-9+.-]*://[^\s/:@]+:)([^\s/@]+)(@)|(\b[A-Za-z0-9_.-]+:)([^\s/:@]+)(@[A-Za-z0-9\[])").expect("url pattern"),
        }
    })
}

/// Replace secrets in free text with `***`. Keys and surrounding punctuation are kept so the
/// line stays readable: `password=hunter2` -> `password=***`,
/// `postgres://bob:pw@db` -> `postgres://bob:***@db`.
pub fn mask_secrets(input: &str) -> String {
    let p = patterns();
    let s = p.json.replace_all(input, |c: &regex::Captures| format!("{}{MASK}{}", &c[1], &c[3]));
    let s = p.kv.replace_all(&s, |c: &regex::Captures| {
        if c.get(2).is_some() {
            format!("{}\"{MASK}\"", &c[1])
        } else if c.get(3).is_some() {
            format!("{}'{MASK}'", &c[1])
        } else {
            format!("{}{MASK}", &c[1])
        }
    });
    let s = p.auth.replace_all(&s, |c: &regex::Captures| format!("{}{MASK}", &c[1]));
    let s = p.sshpass.replace_all(&s, |c: &regex::Captures| {
        if c.get(2).is_some() {
            format!("{}'{MASK}'", &c[1])
        } else if c.get(3).is_some() {
            format!("{}\"{MASK}\"", &c[1])
        } else {
            format!("{}{MASK}", &c[1])
        }
    });
    let s = p.url.replace_all(&s, |c: &regex::Captures| if c.get(1).is_some() { format!("{}{MASK}{}", &c[1], &c[3]) } else { format!("{}{MASK}{}", &c[4], &c[6]) });
    s.into_owned()
}

/// `io::Write` adapter that masks every complete line before passing it on. Partial lines are
/// buffered until the newline (or until flush / drop).
pub struct MaskingWriter<W: Write> {
    inner: W,
    buf: Vec<u8>,
}

impl<W: Write> MaskingWriter<W> {
    pub fn new(inner: W) -> Self {
        MaskingWriter { inner, buf: Vec::new() }
    }

    fn write_masked(&mut self, line: &[u8]) -> io::Result<()> {
        let text = String::from_utf8_lossy(line);
        let masked = mask_secrets(&text);
        self.inner.write_all(masked.as_bytes())
    }

    fn drain_lines(&mut self) -> io::Result<()> {
        while let Some(pos) = self.buf.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = self.buf.drain(..=pos).collect();
            self.write_masked(&line)?;
        }
        Ok(())
    }
}

impl<W: Write> Write for MaskingWriter<W> {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.buf.extend_from_slice(data);
        self.drain_lines()?;
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.drain_lines()?;
        if !self.buf.is_empty() {
            let rest = std::mem::take(&mut self.buf);
            self.write_masked(&rest)?;
        }
        self.inner.flush()
    }
}

impl<W: Write> Drop for MaskingWriter<W> {
    fn drop(&mut self) {
        let _ = self.flush();
    }
}

/// `MakeWriter` wrapper: every writer it hands out is a [`MaskingWriter`].
pub struct MaskingMakeWriter<M>(pub M);

impl<'a, M> MakeWriter<'a> for MaskingMakeWriter<M>
where
    M: MakeWriter<'a>,
{
    type Writer = MaskingWriter<M::Writer>;

    fn make_writer(&'a self) -> Self::Writer {
        MaskingWriter::new(self.0.make_writer())
    }

    fn make_writer_for(&'a self, meta: &tracing::Metadata<'_>) -> Self::Writer {
        MaskingWriter::new(self.0.make_writer_for(meta))
    }
}

// ---------------------------------------------------------------------------------------------
// Ring buffer layer
// ---------------------------------------------------------------------------------------------

/// Stored in span extensions: the `job_id` field recorded on a span.
struct JobIdExt(String);

/// Collects `message`, `job_id` and the remaining fields of an event / span.
#[derive(Default)]
struct FieldVisitor {
    message: String,
    job_id: Option<String>,
    extra: String,
}

impl Visit for FieldVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        match field.name() {
            "message" => self.message = format!("{value:?}"),
            "job_id" => self.job_id = Some(format!("{value:?}").trim_matches('"').to_string()),
            name => {
                let _ = write!(self.extra, " {name}={value:?}");
            }
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        match field.name() {
            "message" => self.message = value.to_string(),
            "job_id" => self.job_id = Some(value.to_string()),
            name => {
                let _ = write!(self.extra, " {name}={value}");
            }
        }
    }
}

/// `tracing` layer that turns every event into a [`LogEvent`] (masked) in a [`LogBuffer`].
/// `job_id` comes from a `job_id` field on the event itself or on the innermost enclosing span
/// that has one.
pub struct RingLayer {
    buffer: Arc<LogBuffer>,
}

impl RingLayer {
    pub fn new(buffer: Arc<LogBuffer>) -> Self {
        RingLayer { buffer }
    }
}

fn map_level(level: &tracing::Level) -> LogLevel {
    match *level {
        tracing::Level::TRACE => LogLevel::Trace,
        tracing::Level::DEBUG => LogLevel::Debug,
        tracing::Level::INFO => LogLevel::Info,
        tracing::Level::WARN => LogLevel::Warn,
        tracing::Level::ERROR => LogLevel::Error,
    }
}

impl<S> Layer<S> for RingLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(&self, attrs: &tracing::span::Attributes<'_>, id: &tracing::span::Id, ctx: Context<'_, S>) {
        let mut v = FieldVisitor::default();
        attrs.record(&mut v);
        if let Some(job) = v.job_id {
            if let Some(span) = ctx.span(id) {
                span.extensions_mut().insert(JobIdExt(job));
            }
        }
    }

    fn on_record(&self, id: &tracing::span::Id, values: &tracing::span::Record<'_>, ctx: Context<'_, S>) {
        let mut v = FieldVisitor::default();
        values.record(&mut v);
        if let Some(job) = v.job_id {
            if let Some(span) = ctx.span(id) {
                span.extensions_mut().replace(JobIdExt(job));
            }
        }
    }

    fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
        let mut v = FieldVisitor::default();
        event.record(&mut v);
        let job_id =
            v.job_id.or_else(|| ctx.event_scope(event).and_then(|scope| scope.into_iter().find_map(|span| span.extensions().get::<JobIdExt>().map(|j| j.0.clone()))));
        let message = if v.extra.is_empty() {
            v.message
        } else if v.message.is_empty() {
            v.extra.trim_start().to_string()
        } else {
            format!("{}{}", v.message, v.extra)
        };
        let meta = event.metadata();
        self.buffer.push(LogEvent {
            job_id,
            level: map_level(meta.level()),
            message: mask_secrets(&message),
            at: crate::types::now_rfc3339(),
            target: Some(meta.target().to_string()),
        });
    }
}

// ---------------------------------------------------------------------------------------------
// init
// ---------------------------------------------------------------------------------------------

static INIT: OnceLock<()> = OnceLock::new();

/// Build the filter: `RUST_LOG` wins, then `level`, then `info`.
fn env_filter(level: &str) -> EnvFilter {
    let level = match level.trim().to_ascii_lowercase().as_str() {
        "warning" => "warn".to_string(),
        "" => "info".to_string(),
        other => other.to_string(),
    };
    EnvFilter::try_from_default_env().or_else(|_| EnvFilter::try_new(&level)).unwrap_or_else(|_| EnvFilter::new("info"))
}

/// Install the global subscriber. Returns a guard that must be kept alive for the file
/// appender to flush. Safe to call once per process; later calls are no-ops.
///
/// Layers: `EnvFilter` (from `RUST_LOG` or `level`), a `fmt` layer writing daily rolling
/// `bulkdb.log.<date>` files in `log_dir` through a non-blocking, line-masking writer, and
/// [`RingLayer`] feeding `buffer` for the UI.
pub fn init(log_dir: &Path, level: &str, buffer: Arc<LogBuffer>) -> Result<Option<tracing_appender::non_blocking::WorkerGuard>> {
    if INIT.set(()).is_err() {
        return Ok(None);
    }
    std::fs::create_dir_all(log_dir)?;
    let appender = tracing_appender::rolling::daily(log_dir, "bulkdb.log");
    let (non_blocking, guard) = tracing_appender::non_blocking(appender);
    let file_layer = tracing_subscriber::fmt::layer().with_ansi(false).with_target(true).with_writer(MaskingMakeWriter(non_blocking));
    let subscriber = Registry::default().with(env_filter(level)).with(file_layer).with(RingLayer::new(buffer));
    match tracing::subscriber::set_global_default(subscriber) {
        Ok(()) => Ok(Some(guard)),
        Err(_) => {
            // Someone (the CLI, a test harness) installed a subscriber first; keep theirs.
            drop(guard);
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masks_every_known_shape() {
        let cases: &[(&str, &str)] = &[
            ("host=db password=hunter2 dbname=x", "host=db password=*** dbname=x"),
            ("Password=Secr3t;Server=x", "Password=***;Server=x"),
            ("pwd=abc", "pwd=***"),
            ("PWD = abc", "PWD = ***"),
            ("passwd=abc", "passwd=***"),
            ("PGPASSWORD=abc psql", "PGPASSWORD=*** psql"),
            ("export PGPASSWORD='a b'", "export PGPASSWORD='***'"),
            (r#"{"password":"abc","user":"u"}"#, r#"{"password":"***","user":"u"}"#),
            (r#"{"sshPassword": "abc"}"#, r#"{"sshPassword": "***"}"#),
            ("password: abc", "password: ***"),
            ("Password: \"a:b\"", "Password: \"***\""),
            ("Authorization: Bearer eyJhbGciOi", "Authorization: Bearer ***"),
            ("authorization: basic dXNlcjpwYXNz", "authorization: basic ***"),
            ("sshpass -p s3cret ssh host", "sshpass -p *** ssh host"),
            ("sshpass -p 's3 cret' ssh host", "sshpass -p '***' ssh host"),
            ("sshpass -p \"s3 cret\" ssh host", "sshpass -p \"***\" ssh host"),
            ("SSHPASS -ps3cret ssh host", "SSHPASS -p*** ssh host"),
            ("postgres://bob:s3cr3t@db.example.com:5432/app", "postgres://bob:***@db.example.com:5432/app"),
            ("mysql://root:p%40ss@localhost/db", "mysql://root:***@localhost/db"),
            ("dsn bob:pw@host", "dsn bob:***@host"),
            ("client_secret=xyz", "client_secret=***"),
            ("api_key=xyz&x=1", "api_key=***&x=1"),
            ("ssh_key_passphrase=abc", "ssh_key_passphrase=***"),
            // The connection-string secret family (connect::is_connection_string_secret_key).
            ("Driver={X};PWD2=abc;Server=s", "Driver={X};PWD2=***;Server=s"),
            ("priv_key_file_pwd=abc;user=u", "priv_key_file_pwd=***;user=u"),
            ("token=abc", "token=***"),
            ("refresh_token=abc", "refresh_token=***"),
            ("ApiKey=abc;Host=h", "ApiKey=***;Host=h"),
            ("PrivateKey=abc", "PrivateKey=***"),
            ("KeyStoreSecret=abc", "KeyStoreSecret=***"),
            ("client_secret_value=abc", "client_secret_value=***"),
            ("no secrets here user=bob host=db", "no secrets here user=bob host=db"),
            ("cwd=/tmp", "cwd=/tmp"),
            ("tokens=5 keys=3", "tokens=5 keys=3"),
            ("select 1 from t where a = 'password'", "select 1 from t where a = 'password'"),
        ];
        for (input, expected) in cases {
            assert_eq!(mask_secrets(input), *expected, "input: {input}");
        }
    }

    #[test]
    fn masking_writer_masks_lines_and_partials() {
        let mut out: Vec<u8> = Vec::new();
        {
            let mut w = MaskingWriter::new(&mut out);
            w.write_all(b"a password=x\nb pass").unwrap();
            w.write_all(b"wd=y\nc pwd=z").unwrap();
        }
        assert_eq!(String::from_utf8(out).unwrap(), "a password=***\nb passwd=***\nc pwd=***");
    }

    #[test]
    fn ring_layer_collects_events_with_job_ids() {
        let buffer = Arc::new(LogBuffer::new());
        let subscriber = Registry::default().with(RingLayer::new(buffer.clone()));
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!("plain message password=abc");
            tracing::warn!(job_id = "j1", rows = 42, "with field");
            let span = tracing::info_span!("job", job_id = "j2");
            let _g = span.enter();
            tracing::error!("inside span");
            let inner = tracing::info_span!("inner");
            let _g2 = inner.enter();
            tracing::debug!("nested span");
        });
        let events = buffer.recent(10);
        assert_eq!(events.len(), 4);
        assert_eq!(events[0].message, "plain message password=***");
        assert_eq!(events[0].level, LogLevel::Info);
        assert_eq!(events[0].job_id, None);
        assert_eq!(events[1].job_id.as_deref(), Some("j1"));
        assert_eq!(events[1].message, "with field rows=42");
        assert_eq!(events[1].level, LogLevel::Warn);
        assert_eq!(events[2].job_id.as_deref(), Some("j2"));
        assert_eq!(events[2].level, LogLevel::Error);
        assert_eq!(events[3].job_id.as_deref(), Some("j2"), "innermost span without job_id falls back to the parent");
        assert_eq!(events[3].level, LogLevel::Debug);
        assert!(events[0].target.as_deref().unwrap().starts_with("bulk_core"));
    }

    #[test]
    fn ring_buffer_is_bounded() {
        let b = LogBuffer::new();
        for i in 0..(RING_CAPACITY + 10) {
            b.log(None, LogLevel::Info, format!("m{i}"));
        }
        let recent = b.recent(5);
        assert_eq!(recent.len(), 5);
        assert_eq!(recent[4].message, format!("m{}", RING_CAPACITY + 9));
        assert_eq!(b.recent(usize::MAX).len(), RING_CAPACITY);
    }

    #[test]
    fn env_filter_accepts_aliases() {
        // Only checks that the constructors do not panic on the strings the settings allow.
        let _ = env_filter("warning");
        let _ = env_filter("debug");
        let _ = env_filter("");
        let _ = env_filter("bulk_core=trace,info");
    }
}
