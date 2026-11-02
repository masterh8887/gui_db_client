//! In-process fake MySQL server for the legacy-protocol tests (owner: **legacy**; design doc
//! section 10). No MySQL 4 server exists on any developer box or CI image, so the pre-4.1
//! protocol is proven against this fixture, which speaks it byte-exactly:
//!
//! * `Persona::Mysql40`: protocol-10 greeting without `CLIENT_PROTOCOL_41` (8-byte scramble,
//!   caps `0x202c` like 4.0's `check_connection()`, charset byte, status, 13 zero bytes),
//!   `HandshakeResponse320` parsing, `check_scramble_323` against the account's stored
//!   `OLD_PASSWORD()` hash, OK / ERR without SQLSTATE, `ColumnDefinition320` (5 lenenc strings,
//!   `CLIENT_LONG_FLAG` flags), 1-byte EOF, text rows with `0xFB` NULL, 16 MB continuation
//!   packets, `KILL <id>` closing that connection.
//! * `Persona::Mysql41OldAuth`: a 4.1 - 5.5.6 style server (protocol 4.1, no
//!   `CLIENT_PLUGIN_AUTH`) whose account has a pre-4.1 hash: after the `HandshakeResponse41`
//!   it sends the single `0xFE` byte and expects `scramble_323(first 8 scramble bytes)` + NUL
//!   (what mysql_async does with `secure_auth(false)`).
//! * `Persona::Mysql55PluginOldAuth`: 5.5.7+ with `CLIENT_PLUGIN_AUTH`: `AuthSwitchRequest`
//!   (`0xFE`, "mysql_old_password\0", scramble).
//!
//! The 4.1 personas answer result sets with `ColumnDefinition41` and 4.1 EOF / OK packets.
//! Every persona answers `SHOW VARIABLES LIKE '...'`, `SELECT VERSION()`, `SELECT @@var, ...`,
//! `SET ...` (OK), `KILL [QUERY] <id>` and (4.1 personas) `SHOW COLLATION` by itself;
//! everything else comes from the [`Script`] (unscripted -> ERR 1064). Scripted rules are
//! matched first, so a test can override a built-in answer. Received SQL is recorded as raw
//! bytes, so tests can assert that SQL arrived CP949-encoded.
//!
//! Host-block bookkeeping: [`FakeServer::interrupted_handshakes`] counts what a real server
//! adds to `max_connect_errors` (socket dropped after the greeting, short / malformed login
//! packet); [`FakeServer::refuse_connections`] makes it send ERR 1129 / 1130 instead of the
//! greeting.
//!
//! The fixture's own `OLD_PASSWORD()` check ([`check_scramble_323`]) is written from the C
//! reference independently of the client code.

use std::collections::HashMap;
use std::io;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt, BufStream};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// `MYSQL_TYPE_*` codes used by the tests.
pub mod types {
    pub const DECIMAL: u8 = 0;
    pub const TINY: u8 = 1;
    pub const SHORT: u8 = 2;
    pub const LONG: u8 = 3;
    pub const FLOAT: u8 = 4;
    pub const DOUBLE: u8 = 5;
    pub const NULL: u8 = 6;
    pub const TIMESTAMP: u8 = 7;
    pub const LONGLONG: u8 = 8;
    pub const INT24: u8 = 9;
    pub const DATE: u8 = 10;
    pub const TIME: u8 = 11;
    pub const DATETIME: u8 = 12;
    pub const YEAR: u8 = 13;
    pub const NEWDECIMAL: u8 = 246;
    pub const BLOB: u8 = 252;
    pub const VAR_STRING: u8 = 253;
    pub const STRING: u8 = 254;
}

/// Column flag bits used by the tests.
pub mod flags {
    pub const NOT_NULL: u16 = 1;
    pub const PRI_KEY: u16 = 2;
    pub const BLOB: u16 = 16;
    pub const UNSIGNED: u16 = 32;
    pub const ZEROFILL: u16 = 64;
    pub const BINARY: u16 = 128;
    pub const TIMESTAMP: u16 = 1024;
}

const CLIENT_LONG_PASSWORD: u32 = 0x0001;
const CLIENT_LONG_FLAG: u32 = 0x0004;
const CLIENT_CONNECT_WITH_DB: u32 = 0x0008;
const CLIENT_COMPRESS: u32 = 0x0020;
const CLIENT_PROTOCOL_41: u32 = 0x0200;
const CLIENT_TRANSACTIONS: u32 = 0x2000;
const CLIENT_SECURE_CONNECTION: u32 = 0x8000;
const CLIENT_MULTI_STATEMENTS: u32 = 1 << 16;
const CLIENT_MULTI_RESULTS: u32 = 1 << 17;
const CLIENT_PLUGIN_AUTH: u32 = 1 << 19;
const CLIENT_PLUGIN_AUTH_LENENC_CLIENT_DATA: u32 = 1 << 21;

/// Which server the fixture pretends to be.
#[derive(Debug, Clone)]
pub enum Persona {
    /// e.g. `version: "4.0.27-standard-log"`, `charset_name: "euc_kr"`, `charset_id: 19`
    /// (or `"latin1"` / 8).
    Mysql40 { version: &'static str, charset_name: &'static str, charset_id: u8 },
    /// e.g. `version: "4.1.22-standard"` or `"5.0.96"`.
    Mysql41OldAuth { version: &'static str, charset_name: &'static str },
    /// e.g. `version: "5.5.62"`.
    Mysql55PluginOldAuth { version: &'static str, charset_name: &'static str },
}

impl Persona {
    fn version(&self) -> &'static str {
        match self {
            Persona::Mysql40 { version, .. } | Persona::Mysql41OldAuth { version, .. } | Persona::Mysql55PluginOldAuth { version, .. } => version,
        }
    }

    fn charset_name(&self) -> &'static str {
        match self {
            Persona::Mysql40 { charset_name, .. } | Persona::Mysql41OldAuth { charset_name, .. } | Persona::Mysql55PluginOldAuth { charset_name, .. } => charset_name,
        }
    }

    fn is_40(&self) -> bool {
        matches!(self, Persona::Mysql40 { .. })
    }

    /// Default collation id of a 4.1+ charset name.
    fn collation_id(&self) -> u8 {
        match self.charset_name() {
            "euckr" | "euc_kr" => 19,
            "utf8" | "utf8mb3" => 33,
            "utf8mb4" => 45,
            "binary" => 63,
            _ => 8,
        }
    }
}

/// An account the fixture accepts.
#[derive(Debug, Clone)]
pub struct Account {
    pub user: Vec<u8>,
    /// Stored pre-4.1 hash, as `OLD_PASSWORD()` prints it split in two words (e.g.
    /// `mypass` -> `[0x6f8c114b, 0x58f2ce9e]`), so the fixture checks the client's
    /// `hash_password_323` independently of the client code. `None` = empty password.
    pub old_hash: Option<[u32; 2]>,
}

/// One result column.
#[derive(Debug, Clone)]
pub struct FakeColumn {
    pub table: Vec<u8>,
    pub name: Vec<u8>,
    /// `MYSQL_TYPE_*` code.
    pub column_type: u8,
    pub flags: u16,
    pub decimals: u8,
    pub length: u32,
    /// Collation id (4.1 personas only).
    pub collation: u16,
}

impl FakeColumn {
    /// Column `name` of table `t` with type `column_type`, length 11, no flags, latin1.
    pub fn new(name: impl AsRef<[u8]>, column_type: u8) -> Self {
        FakeColumn { table: b"t".to_vec(), name: name.as_ref().to_vec(), column_type, flags: 0, decimals: 0, length: 11, collation: 8 }
    }

    pub fn flags(mut self, flags: u16) -> Self {
        self.flags = flags;
        self
    }

    pub fn decimals(mut self, decimals: u8) -> Self {
        self.decimals = decimals;
        self
    }

    pub fn length(mut self, length: u32) -> Self {
        self.length = length;
        self
    }

    pub fn collation(mut self, collation: u16) -> Self {
        self.collation = collation;
        self
    }
}

/// Generator of row `i` (cells as text bytes; `None` = NULL).
pub type RowGen = Arc<dyn Fn(u64) -> Vec<Option<Vec<u8>>> + Send + Sync>;

/// Rows of a scripted result.
#[derive(Clone)]
pub enum RowSource {
    Fixed(Vec<Vec<Option<Vec<u8>>>>),
    /// `count` rows produced lazily (fixed-memory streaming tests, 16 MB rows).
    Generated {
        count: u64,
        gen: RowGen,
    },
    /// Rows until the client disconnects or the connection is killed (cancel tests).
    Endless {
        gen: RowGen,
    },
    /// `rows` rows (flushed), then nothing for `millis` (a slow scan between two rows; a
    /// `KILL` of the session ends the wait), then the final EOF.
    Stalled {
        rows: u64,
        gen: RowGen,
        millis: u64,
    },
}

/// What the fixture answers to a query.
#[derive(Clone)]
pub enum Reply {
    Ok {
        affected_rows: u64,
    },
    Err {
        code: u16,
        message: Vec<u8>,
    },
    Rows {
        columns: Vec<FakeColumn>,
        rows: RowSource,
    },
    /// Say nothing for `millis` (a long sort / `COUNT(*)` before the first byte), then reply.
    Stall {
        millis: u64,
        then: Box<Reply>,
    },
}

/// Query -> reply rules, matched on the raw `COM_QUERY` bytes (first match wins).
#[derive(Clone, Default)]
pub struct Script {
    rules: Vec<(Vec<u8>, bool, Reply)>,
}

impl Script {
    pub fn new() -> Self {
        Script::default()
    }

    /// Reply when the query bytes equal `sql` exactly.
    pub fn on_exact(mut self, sql: &[u8], reply: Reply) -> Self {
        self.rules.push((sql.to_vec(), false, reply));
        self
    }

    /// Reply when the query bytes contain `needle` (e.g. the SQL after the `/* bulkdb-.. */` tag).
    pub fn on_contains(mut self, needle: &[u8], reply: Reply) -> Self {
        self.rules.push((needle.to_vec(), true, reply));
        self
    }

    fn find(&self, sql: &[u8]) -> Option<&Reply> {
        self.rules
            .iter()
            .find(|(pat, contains, _)| if *contains { !pat.is_empty() && sql.windows(pat.len()).any(|w| w == pat.as_slice()) } else { sql == pat.as_slice() })
            .map(|(_, _, r)| r)
    }
}

/// What happened on the fixture, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Connected {
        id: u32,
    },
    /// The client closed the socket (or the socket failed) after the greeting, before sending
    /// an auth packet. A real server counts this toward `max_connect_errors`; see
    /// [`FakeServer::interrupted_handshakes`].
    AbortedHandshake {
        id: u32,
    },
    Auth {
        id: u32,
        user: Vec<u8>,
        ok: bool,
    },
    Query {
        id: u32,
        sql: Vec<u8>,
    },
    /// A `KILL <target>` (statement or `COM_PROCESS_KILL`) arrived, whether or not the target
    /// was still alive (a real server answers 1094 for a session that already ended).
    Killed {
        target: u32,
    },
    Disconnected {
        id: u32,
    },
}

struct Shared {
    persona: Persona,
    accounts: Vec<Account>,
    script: Script,
    events: Mutex<Vec<Event>>,
    queries: Mutex<Vec<Vec<u8>>>,
    next_id: AtomicU32,
    /// Per-connection token: cancelled by `KILL <id>` (and by shutdown, via the parent).
    sessions: Mutex<HashMap<u32, CancellationToken>>,
    /// Handshakes that ended after the greeting without a well-formed login packet (socket
    /// closed or failed, short / malformed packet): what a real server adds to the host's
    /// `max_connect_errors` count (4.0 `check_connection` -> `inc_host_errors`).
    interrupted: AtomicU32,
    /// ERR sent instead of the greeting (e.g. 1129 host blocked), when set.
    refuse: Mutex<Option<(u16, Vec<u8>)>>,
}

impl Shared {
    fn event(&self, e: Event) {
        self.events.lock().unwrap().push(e);
    }

    fn interrupted(&self, id: u32) {
        self.interrupted.fetch_add(1, Ordering::SeqCst);
        self.event(Event::AbortedHandshake { id });
    }
}

/// A running fake server on `127.0.0.1:<random port>`.
pub struct FakeServer {
    pub addr: SocketAddr,
    shared: Arc<Shared>,
    stop: CancellationToken,
    task: JoinHandle<()>,
}

impl FakeServer {
    /// Bind and serve connections on a background task until [`FakeServer::shutdown`].
    pub async fn start(persona: Persona, accounts: Vec<Account>, script: Script) -> FakeServer {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind the fake MySQL server");
        let addr = listener.local_addr().unwrap();
        let shared = Arc::new(Shared {
            persona,
            accounts,
            script,
            events: Mutex::new(Vec::new()),
            queries: Mutex::new(Vec::new()),
            next_id: AtomicU32::new(100),
            sessions: Mutex::new(HashMap::new()),
            interrupted: AtomicU32::new(0),
            refuse: Mutex::new(None),
        });
        let stop = CancellationToken::new();
        let task = tokio::spawn({
            let shared = shared.clone();
            let stop = stop.clone();
            async move {
                loop {
                    let accepted = tokio::select! {
                        _ = stop.cancelled() => break,
                        a = listener.accept() => a,
                    };
                    let Ok((socket, _)) = accepted else { continue };
                    let id = shared.next_id.fetch_add(1, Ordering::SeqCst);
                    let token = stop.child_token();
                    shared.sessions.lock().unwrap().insert(id, token.clone());
                    let shared = shared.clone();
                    tokio::spawn(async move {
                        let _ = socket.set_nodelay(true);
                        serve(shared.clone(), socket, id, token).await;
                        shared.sessions.lock().unwrap().remove(&id);
                    });
                }
            }
        });
        FakeServer { addr, shared, stop, task }
    }

    pub fn port(&self) -> u16 {
        self.addr.port()
    }

    pub fn events(&self) -> Vec<Event> {
        self.shared.events.lock().unwrap().clone()
    }

    /// Handshakes interrupted before a well-formed auth packet arrived (dropped socket after
    /// the greeting, short or malformed login packet). Each one would count toward a real
    /// server's `max_connect_errors` and eventually block the client host until `FLUSH HOSTS`.
    pub fn interrupted_handshakes(&self) -> u32 {
        self.shared.interrupted.load(Ordering::SeqCst)
    }

    /// From now on answer every new connection with this ERR instead of a greeting (like a
    /// server refusing the host: 1129 blocked, 1130 not allowed) and close it.
    pub fn refuse_connections(&self, code: u16, message: &str) {
        *self.shared.refuse.lock().unwrap() = Some((code, message.as_bytes().to_vec()));
    }

    /// Raw SQL bytes of every `COM_QUERY`, in order.
    pub fn queries(&self) -> Vec<Vec<u8>> {
        self.shared.queries.lock().unwrap().clone()
    }

    /// Wait until `pred` holds for the event list (polling), up to `timeout`.
    pub async fn wait_for(&self, timeout: Duration, pred: impl Fn(&[Event]) -> bool) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if pred(&self.events()) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    pub async fn shutdown(self) {
        self.stop.cancel();
        let _ = self.task.await;
    }
}

// ---------------------------------------------------------------------------------------------
// OLD_PASSWORD() (independent of the client code, from the C reference in password.c)
// ---------------------------------------------------------------------------------------------

fn old_hash(bytes: &[u8]) -> [u32; 2] {
    let (mut nr, mut add, mut nr2) = (1_345_345_333u64, 7u64, 0x1234_5671u64);
    for &c in bytes.iter().filter(|c| **c != b' ' && **c != b'\t') {
        let tmp = c as u64;
        nr ^= (((nr & 63) + add) * tmp) + (nr << 8);
        nr &= 0xFFFF_FFFF_FFFF;
        nr2 = (nr2 + ((nr2 << 8) ^ nr)) & 0xFFFF_FFFF_FFFF;
        add += tmp;
    }
    [(nr & 0x7FFF_FFFF) as u32, (nr2 & 0x7FFF_FFFF) as u32]
}

/// `check_scramble_323(scrambled, message, hash_pass)`: `true` when the client's 8-byte
/// response matches.
pub fn check_scramble_323(scrambled: &[u8], message: &[u8], hash_pass: [u32; 2]) -> bool {
    if scrambled.len() != 8 {
        return false;
    }
    let hm = old_hash(&message[..8]);
    let max = 0x3FFF_FFFFu64;
    let mut s1 = ((hash_pass[0] ^ hm[0]) as u64) % max;
    let mut s2 = ((hash_pass[1] ^ hm[1]) as u64) % max;
    let mut rnd = || {
        s1 = (s1 * 3 + s2) % max;
        s2 = (s1 + s2 + 33) % max;
        s1 as f64 / max as f64
    };
    let mut expect = [0u8; 8];
    for e in expect.iter_mut() {
        *e = (rnd() * 31.0).floor() as u8 + 64;
    }
    let extra = (rnd() * 31.0).floor() as u8;
    scrambled.iter().zip(expect).all(|(s, e)| *s == e ^ extra)
}

// ---------------------------------------------------------------------------------------------
// Wire
// ---------------------------------------------------------------------------------------------

const MAX_PAYLOAD: usize = 0xFF_FFFF;

struct Wire {
    s: BufStream<TcpStream>,
    seq: u8,
}

impl Wire {
    /// One logical payload; `None` on a clean EOF before a header. Sets the reply sequence.
    async fn read(&mut self) -> io::Result<Option<(u8, Vec<u8>)>> {
        let mut out = Vec::new();
        let mut first_seq = None;
        loop {
            let mut h = [0u8; 4];
            match self.s.read_exact(&mut h).await {
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof && first_seq.is_none() => return Ok(None),
                Err(e) => return Err(e),
            }
            let len = u32::from_le_bytes([h[0], h[1], h[2], 0]) as usize;
            first_seq.get_or_insert(h[3]);
            self.seq = h[3].wrapping_add(1);
            let start = out.len();
            out.resize(start + len, 0);
            self.s.read_exact(&mut out[start..]).await?;
            if len < MAX_PAYLOAD {
                return Ok(Some((first_seq.unwrap(), out)));
            }
        }
    }

    async fn write(&mut self, payload: &[u8]) -> io::Result<()> {
        let mut rest = payload;
        loop {
            let n = rest.len().min(MAX_PAYLOAD);
            let l = (n as u32).to_le_bytes();
            self.s.write_all(&[l[0], l[1], l[2], self.seq]).await?;
            self.seq = self.seq.wrapping_add(1);
            self.s.write_all(&rest[..n]).await?;
            rest = &rest[n..];
            if n < MAX_PAYLOAD {
                return Ok(());
            }
        }
    }

    async fn flush(&mut self) -> io::Result<()> {
        self.s.flush().await
    }
}

fn lenenc_int(out: &mut Vec<u8>, v: u64) {
    match v {
        0..=250 => out.push(v as u8),
        251..=0xFFFF => {
            out.push(0xFC);
            out.extend_from_slice(&(v as u16).to_le_bytes());
        }
        0x1_0000..=0xFF_FFFF => {
            out.push(0xFD);
            out.extend_from_slice(&(v as u32).to_le_bytes()[..3]);
        }
        _ => {
            out.push(0xFE);
            out.extend_from_slice(&v.to_le_bytes());
        }
    }
}

fn lenenc_str(out: &mut Vec<u8>, b: &[u8]) {
    lenenc_int(out, b.len() as u64);
    out.extend_from_slice(b);
}

fn read_lenenc(b: &[u8], pos: &mut usize) -> Option<u64> {
    let f = *b.get(*pos)?;
    let w = match f {
        0..=250 => {
            *pos += 1;
            return Some(f as u64);
        }
        0xFC => 2,
        0xFD => 3,
        0xFE => 8,
        _ => return None,
    };
    let mut v = [0u8; 8];
    v[..w].copy_from_slice(b.get(*pos + 1..*pos + 1 + w)?);
    *pos += 1 + w;
    Some(u64::from_le_bytes(v))
}

fn cstr<'a>(b: &'a [u8], pos: &mut usize) -> &'a [u8] {
    let rest = &b[(*pos).min(b.len())..];
    let end = rest.iter().position(|c| *c == 0).unwrap_or(rest.len());
    *pos += end + 1;
    &rest[..end]
}

/// Encoding of results for the negotiated protocol.
#[derive(Clone, Copy)]
struct Proto {
    v41: bool,
    /// 4.0: the client sent `CLIENT_TRANSACTIONS` (OK carries the status).
    transactions: bool,
    long_flag: bool,
}

impl Proto {
    fn ok(&self, affected: u64) -> Vec<u8> {
        let mut p = vec![0];
        lenenc_int(&mut p, affected);
        lenenc_int(&mut p, 0);
        if self.v41 {
            p.extend_from_slice(&[2, 0, 0, 0]);
        } else if self.transactions {
            p.extend_from_slice(&[2, 0]);
        }
        p
    }

    fn err(&self, code: u16, message: &[u8]) -> Vec<u8> {
        let mut p = vec![0xFF];
        p.extend_from_slice(&code.to_le_bytes());
        if self.v41 {
            p.extend_from_slice(if code == 1064 { b"#42000" } else { b"#HY000" });
        }
        p.extend_from_slice(message);
        p
    }

    fn eof(&self) -> Vec<u8> {
        if self.v41 {
            vec![0xFE, 0, 0, 2, 0]
        } else {
            vec![0xFE]
        }
    }

    fn column(&self, c: &FakeColumn) -> Vec<u8> {
        let mut p = Vec::new();
        if self.v41 {
            lenenc_str(&mut p, b"def");
            lenenc_str(&mut p, b"");
            lenenc_str(&mut p, &c.table);
            lenenc_str(&mut p, &c.table);
            lenenc_str(&mut p, &c.name);
            lenenc_str(&mut p, &c.name);
            p.push(0x0C);
            p.extend_from_slice(&c.collation.to_le_bytes());
            p.extend_from_slice(&c.length.to_le_bytes());
            p.push(c.column_type);
            p.extend_from_slice(&c.flags.to_le_bytes());
            p.push(c.decimals);
            p.extend_from_slice(&[0, 0]);
        } else {
            // ColumnDefinition320: five length-encoded strings.
            lenenc_str(&mut p, &c.table);
            lenenc_str(&mut p, &c.name);
            lenenc_str(&mut p, &c.length.to_le_bytes()[..3]);
            lenenc_str(&mut p, &[c.column_type]);
            if self.long_flag {
                let f = c.flags.to_le_bytes();
                lenenc_str(&mut p, &[f[0], f[1], c.decimals]);
            } else {
                lenenc_str(&mut p, &[c.flags as u8, c.decimals]);
            }
        }
        p
    }
}

fn row_payload(cells: &[Option<Vec<u8>>]) -> Vec<u8> {
    let mut p = Vec::with_capacity(cells.iter().map(|c| c.as_ref().map_or(1, |b| b.len() + 9)).sum());
    for c in cells {
        match c {
            None => p.push(0xFB),
            Some(b) => lenenc_str(&mut p, b),
        }
    }
    p
}

fn text_col(name: &str) -> FakeColumn {
    FakeColumn::new(name, types::VAR_STRING).length(255)
}

/// Built-in variables (name -> value) of a persona.
fn variables(persona: &Persona) -> Vec<(&'static str, String)> {
    let cs = persona.charset_name();
    let mut v = vec![
        ("version", persona.version().to_string()),
        ("max_allowed_packet", "1048576".to_string()),
        ("wait_timeout", "28800".to_string()),
        ("net_read_timeout", "30".to_string()),
        ("net_write_timeout", "60".to_string()),
        ("key_buffer_size", "8388600".to_string()),
        ("version_comment", "BulkDB fake MySQL server".to_string()),
    ];
    if persona.is_40() {
        v.push(("character_set", cs.to_string()));
        v.push(("character_sets", "latin1 big5 czech euc_kr gb2312 gbk latin1_de sjis tis620 ujis dec8 dos german1 hp8 koi8_ru latin2 swe7 usa7".into()));
    } else {
        for name in ["character_set_server", "character_set_database", "character_set_client", "character_set_connection", "character_set_results"] {
            v.push((name, cs.to_string()));
        }
        v.push(("sql_mode", String::new()));
        v.push(("innodb_buffer_pool_size", "134217728".to_string()));
    }
    v
}

/// SQL `LIKE` (`%`, `_`, `\` escape), ASCII case-insensitive.
fn like(pattern: &[u8], s: &[u8]) -> bool {
    match pattern.first() {
        None => s.is_empty(),
        Some(b'%') => (0..=s.len()).any(|i| like(&pattern[1..], &s[i..])),
        Some(b'_') => !s.is_empty() && like(&pattern[1..], &s[1..]),
        Some(b'\\') if pattern.len() > 1 => s.first().is_some_and(|c| c.eq_ignore_ascii_case(&pattern[1])) && like(&pattern[2..], &s[1..]),
        Some(p) => s.first().is_some_and(|c| c.eq_ignore_ascii_case(p)) && like(&pattern[1..], &s[1..]),
    }
}

enum Outcome {
    Continue,
    Close,
}

struct Session {
    shared: Arc<Shared>,
    wire: Wire,
    id: u32,
    proto: Proto,
    user: Vec<u8>,
    database: Option<Vec<u8>>,
    token: CancellationToken,
}

async fn serve(shared: Arc<Shared>, socket: TcpStream, id: u32, token: CancellationToken) {
    shared.event(Event::Connected { id });
    let wire = Wire { s: BufStream::new(socket), seq: 0 };
    let mut session = Session {
        shared: shared.clone(),
        wire,
        id,
        proto: Proto { v41: false, transactions: false, long_flag: true },
        user: Vec::new(),
        database: None,
        token: token.clone(),
    };
    let authed = tokio::select! {
        _ = token.cancelled() => false,
        r = session.handshake() => r.unwrap_or(false),
    };
    if !authed {
        return;
    }
    loop {
        let r = tokio::select! {
            _ = token.cancelled() => break,
            r = session.command() => r,
        };
        match r {
            Ok(Outcome::Continue) => {}
            Ok(Outcome::Close) | Err(_) => break,
        }
    }
    shared.event(Event::Disconnected { id });
}

impl Session {
    fn scramble(&self) -> Vec<u8> {
        // Printable like the server's create_random_string(), different per connection.
        let mut x = self.id.wrapping_mul(2_654_435_761).wrapping_add(12345);
        (0..20)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                33 + (x % 94) as u8
            })
            .collect()
    }

    /// Greeting + login. `Ok(true)` when the client is authenticated.
    async fn handshake(&mut self) -> io::Result<bool> {
        let refuse = self.shared.refuse.lock().unwrap().clone();
        if let Some((code, message)) = refuse {
            // Like `check_connection` for a blocked / unknown host: ERR in place of the greeting.
            let p = Proto { v41: false, transactions: false, long_flag: true }.err(code, &message);
            self.wire.seq = 0;
            self.wire.write(&p).await?;
            self.wire.flush().await?;
            return Ok(false);
        }
        let persona = self.shared.persona.clone();
        let scramble = self.scramble();
        let mut g = vec![10];
        g.extend_from_slice(persona.version().as_bytes());
        g.push(0);
        g.extend_from_slice(&self.id.to_le_bytes());
        g.extend_from_slice(&scramble[..8]);
        g.push(0);
        match &persona {
            Persona::Mysql40 { charset_id, .. } => {
                let caps = CLIENT_LONG_FLAG | CLIENT_CONNECT_WITH_DB | CLIENT_COMPRESS | CLIENT_TRANSACTIONS;
                g.extend_from_slice(&(caps as u16).to_le_bytes());
                g.push(*charset_id);
                g.extend_from_slice(&2u16.to_le_bytes());
                g.extend_from_slice(&[0; 13]);
            }
            Persona::Mysql41OldAuth { .. } | Persona::Mysql55PluginOldAuth { .. } => {
                let plugin = matches!(persona, Persona::Mysql55PluginOldAuth { .. });
                let mut caps = CLIENT_LONG_PASSWORD
                    | CLIENT_LONG_FLAG
                    | CLIENT_CONNECT_WITH_DB
                    | CLIENT_PROTOCOL_41
                    | CLIENT_TRANSACTIONS
                    | CLIENT_SECURE_CONNECTION
                    | CLIENT_MULTI_STATEMENTS
                    | CLIENT_MULTI_RESULTS;
                if plugin {
                    caps |= CLIENT_PLUGIN_AUTH;
                }
                g.extend_from_slice(&(caps as u16).to_le_bytes());
                g.push(persona.collation_id());
                g.extend_from_slice(&2u16.to_le_bytes());
                g.extend_from_slice(&((caps >> 16) as u16).to_le_bytes());
                g.push(if plugin { 21 } else { 0 });
                g.extend_from_slice(&[0; 10]);
                g.extend_from_slice(&scramble[8..20]);
                g.push(0);
                if plugin {
                    g.extend_from_slice(b"mysql_native_password\0");
                }
            }
        }
        self.wire.seq = 0;
        self.wire.write(&g).await?;
        self.wire.flush().await?;

        let (seq, auth) = match self.wire.read().await {
            Ok(Some(packet)) => packet,
            Ok(None) | Err(_) => {
                self.shared.interrupted(self.id);
                return Ok(false);
            }
        };
        if persona.is_40() {
            self.login_320(seq, &auth, &scramble).await
        } else {
            self.login_41(&auth, &scramble).await
        }
    }

    async fn deny(&mut self, using_password: bool) -> io::Result<bool> {
        self.shared.event(Event::Auth { id: self.id, user: self.user.clone(), ok: false });
        let msg = if self.proto.v41 {
            format!("Access denied for user '{}'@'localhost' (using password: {})", String::from_utf8_lossy(&self.user), if using_password { "YES" } else { "NO" })
        } else {
            format!("Access denied for user: '{}@localhost' (Using password: {})", String::from_utf8_lossy(&self.user), if using_password { "YES" } else { "NO" })
        };
        let p = self.proto.err(1045, msg.as_bytes());
        self.wire.write(&p).await?;
        self.wire.flush().await?;
        Ok(false)
    }

    /// After the password check: database check, OK, `Auth` event.
    async fn accept(&mut self) -> io::Result<bool> {
        if self.database.as_deref() == Some(b"missing_db") {
            self.shared.event(Event::Auth { id: self.id, user: self.user.clone(), ok: false });
            let p = self.proto.err(1049, b"Unknown database 'missing_db'");
            self.wire.write(&p).await?;
            self.wire.flush().await?;
            return Ok(false);
        }
        self.shared.event(Event::Auth { id: self.id, user: self.user.clone(), ok: true });
        let p = self.proto.ok(0);
        self.wire.write(&p).await?;
        self.wire.flush().await?;
        Ok(true)
    }

    async fn login_320(&mut self, seq: u8, auth: &[u8], scramble: &[u8]) -> io::Result<bool> {
        // 4.0 `check_connection`: a packet shorter than MIN_HANDSHAKE_SIZE (6) is a handshake
        // error that counts toward max_connect_errors, like a dropped socket.
        if seq != 1 || auth.len() < 6 {
            self.shared.interrupted(self.id);
            let p = self.proto.err(1043, b"Bad handshake");
            self.wire.write(&p).await?;
            self.wire.flush().await?;
            return Ok(false);
        }
        let client = u16::from_le_bytes([auth[0], auth[1]]) as u32;
        self.proto = Proto { v41: false, transactions: client & CLIENT_TRANSACTIONS != 0, long_flag: client & CLIENT_LONG_FLAG != 0 };
        if client & CLIENT_PROTOCOL_41 != 0 || client & CLIENT_LONG_PASSWORD == 0 {
            // A 4.0 server would read a 4.1 response as garbage / use the 3.21 scheme.
            let p = self.proto.err(1043, b"Bad handshake");
            self.wire.write(&p).await?;
            self.wire.flush().await?;
            return Ok(false);
        }
        let mut pos = 5;
        self.user = cstr(auth, &mut pos).to_vec();
        let response = if client & CLIENT_CONNECT_WITH_DB != 0 {
            let r = cstr(auth, &mut pos).to_vec();
            let db = cstr(auth, &mut pos).to_vec();
            self.database = Some(db).filter(|d| !d.is_empty());
            r
        } else {
            let r = &auth[pos.min(auth.len())..];
            r.strip_suffix(&[0]).unwrap_or(r).to_vec()
        };
        let account = self.shared.accounts.iter().find(|a| a.user == self.user).cloned();
        let ok = match (&account, account.as_ref().and_then(|a| a.old_hash)) {
            (None, _) => false,
            (Some(_), None) => response.is_empty(),
            (Some(_), Some(hash)) => check_scramble_323(&response, &scramble[..8], hash),
        };
        if !ok {
            return self.deny(!response.is_empty()).await;
        }
        self.accept().await
    }

    async fn login_41(&mut self, auth: &[u8], scramble: &[u8]) -> io::Result<bool> {
        self.proto = Proto { v41: true, transactions: true, long_flag: true };
        if auth.len() < 32 {
            self.shared.interrupted(self.id);
            let p = self.proto.err(1043, b"Bad handshake");
            self.wire.write(&p).await?;
            self.wire.flush().await?;
            return Ok(false);
        }
        let client = u32::from_le_bytes([auth[0], auth[1], auth[2], auth[3]]);
        let mut pos = 32;
        self.user = cstr(auth, &mut pos).to_vec();
        let response: Vec<u8> = if client & CLIENT_PLUGIN_AUTH_LENENC_CLIENT_DATA != 0 {
            let n = read_lenenc(auth, &mut pos).unwrap_or(0) as usize;
            let r = auth.get(pos..pos + n).unwrap_or_default().to_vec();
            pos += n;
            r
        } else if client & CLIENT_SECURE_CONNECTION != 0 {
            let n = auth.get(pos).copied().unwrap_or(0) as usize;
            let r = auth.get(pos + 1..pos + 1 + n).unwrap_or_default().to_vec();
            pos += 1 + n;
            r
        } else {
            cstr(auth, &mut pos).to_vec()
        };
        if client & CLIENT_CONNECT_WITH_DB != 0 {
            self.database = Some(cstr(auth, &mut pos).to_vec()).filter(|d| !d.is_empty());
        }
        let Some(account) = self.shared.accounts.iter().find(|a| a.user == self.user).cloned() else {
            return self.deny(!response.is_empty()).await;
        };
        let Some(hash) = account.old_hash else {
            return if response.is_empty() { self.accept().await } else { self.deny(true).await };
        };
        // The account has a pre-4.1 hash: ask for the 3.23 scramble.
        let switch = match self.shared.persona {
            Persona::Mysql55PluginOldAuth { .. } => {
                let mut p = vec![0xFE];
                p.extend_from_slice(b"mysql_old_password\0");
                p.extend_from_slice(&scramble[..20]);
                p.push(0);
                p
            }
            _ => vec![0xFE],
        };
        self.wire.write(&switch).await?;
        self.wire.flush().await?;
        let Some((_, reply)) = self.wire.read().await? else {
            // The client refused (secure_auth) and hung up.
            self.shared.event(Event::Auth { id: self.id, user: self.user.clone(), ok: false });
            return Ok(false);
        };
        let reply = reply.strip_suffix(&[0]).unwrap_or(&reply);
        if check_scramble_323(reply, &scramble[..8], hash) {
            self.accept().await
        } else {
            self.deny(true).await
        }
    }

    async fn send(&mut self, payload: &[u8]) -> io::Result<()> {
        self.wire.write(payload).await?;
        self.wire.flush().await
    }

    async fn command(&mut self) -> io::Result<Outcome> {
        let Some((seq, packet)) = self.wire.read().await? else { return Ok(Outcome::Close) };
        if seq != 0 {
            let p = self.proto.err(1156, b"Got packets out of order");
            self.send(&p).await?;
            return Ok(Outcome::Close);
        }
        let Some((&cmd, arg)) = packet.split_first() else { return Ok(Outcome::Close) };
        match cmd {
            0x01 => Ok(Outcome::Close),
            0x02 => {
                let p = if arg == b"missing_db" { self.proto.err(1049, b"Unknown database 'missing_db'") } else { self.proto.ok(0) };
                self.send(&p).await?;
                Ok(Outcome::Continue)
            }
            0x03 => {
                self.shared.event(Event::Query { id: self.id, sql: arg.to_vec() });
                self.shared.queries.lock().unwrap().push(arg.to_vec());
                self.query(arg).await
            }
            0x0C if arg.len() >= 4 => {
                let target = u32::from_le_bytes([arg[0], arg[1], arg[2], arg[3]]);
                self.kill(target).await
            }
            0x0E => {
                let p = self.proto.ok(0);
                self.send(&p).await?;
                Ok(Outcome::Continue)
            }
            _ => {
                let p = self.proto.err(1047, b"Unknown command");
                self.send(&p).await?;
                Ok(Outcome::Continue)
            }
        }
    }

    async fn kill(&mut self, target: u32) -> io::Result<Outcome> {
        self.shared.event(Event::Killed { target });
        let token = self.shared.sessions.lock().unwrap().get(&target).cloned();
        match token {
            Some(t) => {
                t.cancel();
                if target == self.id {
                    return Ok(Outcome::Close);
                }
                let p = self.proto.ok(0);
                self.send(&p).await?;
            }
            None => {
                let p = self.proto.err(1094, format!("Unknown thread id: {target}").as_bytes());
                self.send(&p).await?;
            }
        }
        Ok(Outcome::Continue)
    }

    async fn query(&mut self, sql: &[u8]) -> io::Result<Outcome> {
        if let Some(reply) = self.shared.script.find(sql).cloned() {
            return self.reply(reply).await;
        }
        let text = String::from_utf8_lossy(sql).trim().to_string();
        let lower = text.to_ascii_lowercase();
        if let Some(rest) = lower.strip_prefix("kill ") {
            let id = rest.trim_start_matches("query ").trim_start_matches("connection ").trim();
            if let Ok(target) = id.parse::<u32>() {
                return self.kill(target).await;
            }
        }
        if lower.starts_with("set ") {
            let p = self.proto.ok(0);
            self.send(&p).await?;
            return Ok(Outcome::Continue);
        }
        if let Some(pattern) = lower.strip_prefix("show variables like ") {
            let pattern = pattern.trim().trim_matches('\'');
            let rows: Vec<Vec<Option<Vec<u8>>>> = variables(&self.shared.persona)
                .into_iter()
                .filter(|(name, _)| like(pattern.as_bytes(), name.as_bytes()))
                .map(|(name, value)| vec![Some(name.as_bytes().to_vec()), Some(value.into_bytes())])
                .collect();
            return self.reply(Reply::Rows { columns: vec![text_col("Variable_name"), text_col("Value")], rows: RowSource::Fixed(rows) }).await;
        }
        if lower == "show collation" && !self.shared.persona.is_40() {
            let rows = [("latin1_swedish_ci", "latin1", 8), ("euckr_korean_ci", "euckr", 19), ("utf8_general_ci", "utf8", 33), ("binary", "binary", 63)]
                .iter()
                .map(|(c, cs, id)| {
                    vec![
                        Some(c.as_bytes().to_vec()),
                        Some(cs.as_bytes().to_vec()),
                        Some(id.to_string().into_bytes()),
                        Some(b"Yes".to_vec()),
                        Some(b"Yes".to_vec()),
                        Some(b"1".to_vec()),
                    ]
                })
                .collect();
            let columns = ["Collation", "Charset", "Id", "Default", "Compiled", "Sortlen"].iter().map(|n| text_col(n)).collect();
            return self.reply(Reply::Rows { columns, rows: RowSource::Fixed(rows) }).await;
        }
        if let Some(list) = lower.strip_prefix("select ") {
            if let Some(reply) = self.select_builtin(list) {
                return self.reply(reply).await;
            }
        }
        let shown: String = text.chars().take(80).collect();
        let msg = format!("You have an error in your SQL syntax near '{shown}' at line 1");
        self.reply(Reply::Err { code: 1064, message: msg.into_bytes() }).await
    }

    /// `SELECT VERSION()`, `SELECT @@a, @@b`, `DATABASE()`, `USER()`, `CONNECTION_ID()`.
    fn select_builtin(&self, list: &str) -> Option<Reply> {
        let vars = variables(&self.shared.persona);
        let mut columns = Vec::new();
        let mut row = Vec::new();
        for item in list.split(',') {
            let item = item.trim();
            let value = match item {
                "version()" => Some(self.shared.persona.version().to_string()),
                "database()" => self.database.as_ref().map(|d| String::from_utf8_lossy(d).into_owned()),
                "user()" | "current_user()" => Some(format!("{}@localhost", String::from_utf8_lossy(&self.user))),
                "connection_id()" => Some(self.id.to_string()),
                v if v.starts_with("@@") => {
                    let name = v.trim_start_matches("@@").trim_start_matches("session.").trim_start_matches("global.");
                    if name == "socket" {
                        None
                    } else {
                        Some(vars.iter().find(|(n, _)| *n == name)?.1.clone())
                    }
                }
                _ => return None,
            };
            let numeric = value.as_deref().is_some_and(|v| !v.is_empty() && v.bytes().all(|b| b.is_ascii_digit()));
            columns.push(if numeric { FakeColumn::new(item, types::LONGLONG).length(21) } else { text_col(item) });
            row.push(value.map(String::into_bytes));
        }
        Some(Reply::Rows { columns, rows: RowSource::Fixed(vec![row]) })
    }

    async fn reply(&mut self, mut reply: Reply) -> io::Result<Outcome> {
        while let Reply::Stall { millis, then } = reply {
            tokio::time::sleep(Duration::from_millis(millis)).await;
            reply = *then;
        }
        match reply {
            Reply::Stall { .. } => unreachable!("unwrapped above"),
            Reply::Ok { affected_rows } => {
                let p = self.proto.ok(affected_rows);
                self.send(&p).await?;
            }
            Reply::Err { code, message } => {
                let p = self.proto.err(code, &message);
                self.send(&p).await?;
            }
            Reply::Rows { columns, rows } => {
                let mut head = Vec::new();
                lenenc_int(&mut head, columns.len() as u64);
                self.wire.write(&head).await?;
                for c in &columns {
                    let p = self.proto.column(c);
                    self.wire.write(&p).await?;
                }
                let eof = self.proto.eof();
                self.wire.write(&eof).await?;
                match rows {
                    RowSource::Fixed(rows) => {
                        for r in &rows {
                            self.wire.write(&row_payload(r)).await?;
                        }
                    }
                    RowSource::Generated { count, gen } => {
                        for i in 0..count {
                            if self.token.is_cancelled() {
                                return Ok(Outcome::Close);
                            }
                            self.wire.write(&row_payload(&gen(i))).await?;
                        }
                    }
                    RowSource::Stalled { rows, gen, millis } => {
                        for i in 0..rows {
                            self.wire.write(&row_payload(&gen(i))).await?;
                        }
                        self.wire.flush().await?;
                        let token = self.token.clone();
                        tokio::select! {
                            _ = token.cancelled() => return Ok(Outcome::Close),
                            _ = tokio::time::sleep(Duration::from_millis(millis)) => {}
                        }
                    }
                    RowSource::Endless { gen } => {
                        let mut i = 0;
                        loop {
                            if self.token.is_cancelled() {
                                return Ok(Outcome::Close);
                            }
                            let p = row_payload(&gen(i));
                            let token = self.token.clone();
                            tokio::select! {
                                _ = token.cancelled() => return Ok(Outcome::Close),
                                r = self.wire.write(&p) => r?,
                            }
                            i += 1;
                        }
                    }
                }
                let eof = self.proto.eof();
                self.wire.write(&eof).await?;
                self.wire.flush().await?;
            }
        }
        Ok(Outcome::Continue)
    }
}
