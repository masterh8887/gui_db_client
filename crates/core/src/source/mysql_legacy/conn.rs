//! One authenticated pre-4.1 connection. Owner: **legacy**.
//!
//! Strictly sequential (one command in flight). A half-read result set makes the connection
//! unusable: the owner drops it (closing the socket aborts the statement on the server) and
//! reconnects lazily on the next use. [`LegacyConn::is_idle`] tells the owner which case it is.

use std::borrow::Cow;
use std::time::Duration;

use tokio::io::{AsyncWriteExt, BufStream};
use tokio::net::TcpStream;

use super::handshake::{auth_packet_320, caps, client_flags_320, decline, scramble_323, Greeting};
use super::packet::PacketIo;
use super::protocol::{self, OkPacket, COM_INIT_DB, COM_PING, COM_QUERY, COM_QUIT};
use crate::encoding::Charset;
use crate::error::{CoreError, Result};
use crate::source::mysql_common::{MetaColumn, MetaRows, RawColumnDef};
use crate::source::{ConnectParams, SslMode};

/// Read buffer of the socket (rows are read straight into the row buffer when larger).
const READ_BUFFER: usize = 64 * 1024;
/// Row buffer capacity kept after a result set that needed more (e.g. a 16 MB cell).
const KEEP_ROW_BUFFER: usize = 1024 * 1024;

/// Encode UTF-8 text (SQL, user name, password, database name) for the wire. ASCII is the same
/// in every charset this client speaks, so it is borrowed as is; anything else goes through
/// [`crate::encoding::encode_sql`] (`INVALID` for a character the charset cannot represent).
pub(crate) fn encode_text(cs: Charset, text: &str) -> Result<Cow<'_, [u8]>> {
    if text.is_ascii() {
        return Ok(Cow::Borrowed(text.as_bytes()));
    }
    crate::encoding::encode_sql(cs, text)
}

/// The legacy client has no TLS: `Require` / `VerifyFull` -> `CONNECT`; `Prefer` continues in
/// clear text (like the modern client in this build, which has no TLS backend either).
pub(crate) fn check_tls(params: &ConnectParams) -> Result<()> {
    if matches!(params.ssl, SslMode::Require | SslMode::VerifyFull) {
        return Err(CoreError::Connect("TLS is not supported for MySQL < 4.1 (pre-4.1 protocol); set sslmode to prefer or disable, or use an SSH tunnel".into()));
    }
    Ok(())
}

/// User name, password and database encoded for the login packet.
pub(crate) struct LoginText<'a> {
    user: Cow<'a, [u8]>,
    /// Secret: never logged, dropped right after the scramble is computed.
    password: Cow<'a, [u8]>,
    database: Option<Cow<'a, [u8]>>,
}

/// One login field; `INVALID` names the field (and, unless it is the password, the
/// character) that `charset` cannot represent.
fn encode_login_field<'a>(charset: Charset, what: &str, text: &'a str, secret: bool) -> Result<Cow<'a, [u8]>> {
    encode_text(charset, text).map_err(|_| {
        let which = match crate::encoding::find_unrepresentable(charset, text) {
            Some(u) if !secret => format!(" {:?} (U+{:04X})", u.ch, u.ch as u32),
            _ => String::new(),
        };
        CoreError::Invalid(format!(
            "the {what} contains a character{which} that the server charset {} cannot represent; choose the connection encoding the account was created with",
            charset.label()
        ))
    })
}

/// Encode the login text for a pre-4.1 server. Pure (no I/O): it runs before the socket is
/// opened when the charset is known up front (explicit `encoding`), otherwise right after the
/// greeting inside [`LegacyConn::handshake`].
pub(crate) fn encode_login(params: &ConnectParams, charset: Charset) -> Result<LoginText<'_>> {
    Ok(LoginText {
        user: encode_login_field(charset, "user name", params.user.as_deref().unwrap_or(""), false)?,
        password: encode_login_field(charset, "password", params.password.as_deref().unwrap_or(""), true)?,
        database: match params.database.as_deref().filter(|d| !d.is_empty()) {
            Some(db) => Some(encode_login_field(charset, "database name", db, false)?),
            None => None,
        },
    })
}

/// Everything checked between the greeting and the login packet.
fn login_text<'a>(greeting: &Greeting, params: &'a ConnectParams, charset: Charset) -> Result<LoginText<'a>> {
    check_tls(params)?;
    if !greeting.is_pre41() {
        return Err(CoreError::Connect(format!("MySQL {} speaks the 4.1+ protocol; set the connection protocol to auto or modern", greeting.server_version)));
    }
    encode_login(params, charset)
}

/// ERR during login: 1045 -> `AUTH` (via `map_server_error`); the database named in the
/// login packet is unknown (1049) or not accessible (1044) -> `CONNECT`, because the
/// connection settings are wrong rather than a query.
fn login_error(err: protocol::ErrPacket, charset: Charset) -> CoreError {
    match err.code {
        1044 | 1049 => {
            let code = err.code;
            let mut scratch = String::new();
            let msg = crate::encoding::decode_cell(charset, &err.message, &mut scratch).text.to_string();
            CoreError::Connect(format!("{msg} ({code})"))
        }
        _ => err.into_core(charset),
    }
}

/// Outcome of starting a `COM_QUERY`.
#[derive(Debug)]
pub enum QueryStart {
    /// The statement returned no result set.
    Done(OkPacket),
    /// A result set follows; rows are read with [`LegacyConn::next_row`].
    Rows(Vec<RawColumnDef>),
}

pub struct LegacyConn {
    io: PacketIo<BufStream<TcpStream>>,
    greeting: Greeting,
    /// Charset for SQL, user name, error messages and column names (the plan's charset).
    charset: Charset,
    /// Server granted `CLIENT_LONG_FLAG` / `CLIENT_TRANSACTIONS`.
    long_flag: bool,
    transactions: bool,
    /// Reused payload buffer (holds the current row).
    buf: Vec<u8>,
    /// A command is in flight or a result set is open (rows not fully read).
    in_result: bool,
    /// The socket failed; the connection must be dropped.
    broken: bool,
}

impl std::fmt::Debug for LegacyConn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LegacyConn").field("server", &self.greeting.server_version).field("connection_id", &self.greeting.connection_id).finish()
    }
}

impl LegacyConn {
    /// Send the `HandshakeResponse320` on a socket whose greeting was already read
    /// ([`super::handshake::peek_greeting`]) and read OK / ERR. `charset` encodes the user
    /// name and password bytes. TLS: `SslMode::Require` / `VerifyFull` -> `CONNECT` error
    /// ("TLS is not supported for MySQL < 4.1"); `Prefer` continues in clear text.
    /// 1045 -> `AUTH`. Bounded by `params.connect_timeout`.
    ///
    /// Every check that fails here (TLS required, a 4.1+ greeting, login text the charset
    /// cannot represent) still completes the protocol through [`decline`] before the error is
    /// returned: a socket dropped after the greeting without a login packet counts toward the
    /// server's `max_connect_errors` and eventually blocks this host (design doc section 3).
    pub async fn handshake(stream: TcpStream, greeting: Greeting, params: &ConnectParams, charset: Charset) -> Result<Self> {
        let login = match login_text(&greeting, params, charset) {
            Ok(login) => login,
            Err(e) => {
                decline(stream, &greeting, params.connect_timeout).await;
                return Err(e);
            }
        };
        let timeout = params.connect_timeout;
        match tokio::time::timeout(timeout, Self::authenticate(stream, greeting, login, charset)).await {
            Ok(r) => r,
            Err(_) => Err(CoreError::Connect(format!("MySQL login did not finish within {} s", timeout.as_secs_f32()))),
        }
    }

    async fn authenticate(stream: TcpStream, greeting: Greeting, login: LoginText<'_>, charset: Charset) -> Result<Self> {
        let LoginText { user, password, database } = login;
        let flags = client_flags_320(&greeting, database.is_some());
        let auth: Vec<u8> = if password.is_empty() { Vec::new() } else { scramble_323(&greeting.scramble, &password).to_vec() };
        drop(password);
        let packet = auth_packet_320(flags, &user, &auth, database.as_deref());
        let mut conn = LegacyConn {
            io: PacketIo::new(BufStream::with_capacity(READ_BUFFER, 8 * 1024, stream), 1),
            greeting,
            charset,
            long_flag: flags as u32 & caps::CLIENT_LONG_FLAG != 0,
            transactions: flags as u32 & caps::CLIENT_TRANSACTIONS != 0,
            buf: Vec::new(),
            in_result: true,
            broken: false,
        };
        conn.write(&packet).await?;
        conn.read().await?;
        conn.in_result = false;
        if protocol::is_err(&conn.buf) {
            return Err(login_error(protocol::parse_err(&conn.buf)?, charset));
        }
        if protocol::is_eof(&conn.buf) {
            return Err(CoreError::Connect("the server asked for another authentication method; this pre-4.1 client only speaks the 3.23 scheme".into()));
        }
        protocol::parse_ok(&conn.buf, conn.transactions)?;
        if let Some(db) = database.filter(|_| flags as u32 & caps::CLIENT_CONNECT_WITH_DB == 0) {
            // A server without CONNECT_WITH_DB (very old 3.23) needs COM_INIT_DB.
            conn.command(COM_INIT_DB, &db).await.map_err(|e| match e {
                CoreError::Query { code: Some(code), msg } if code == "1049" || code == "1044" => CoreError::Connect(msg),
                e => e,
            })?;
        }
        Ok(conn)
    }

    pub fn greeting(&self) -> &Greeting {
        &self.greeting
    }

    pub fn connection_id(&self) -> u32 {
        self.greeting.connection_id
    }

    pub fn charset(&self) -> Charset {
        self.charset
    }

    /// Switch the charset once the plan is known (after `SHOW VARIABLES LIKE 'character_set'`).
    pub fn set_charset(&mut self, charset: Charset) {
        self.charset = charset;
    }

    /// No command in flight, no open result set and the socket is healthy: the connection can
    /// go back to its slot. Otherwise the owner must drop it.
    pub fn is_idle(&self) -> bool {
        !self.in_result && !self.broken
    }

    async fn read(&mut self) -> Result<()> {
        let r = self.io.read_payload(&mut self.buf).await;
        if r.is_err() {
            self.broken = true;
        }
        r
    }

    async fn write(&mut self, payload: &[u8]) -> Result<()> {
        let r = self.io.write_payload(payload).await;
        if r.is_err() {
            self.broken = true;
        }
        r
    }

    fn ensure_idle(&self) -> Result<()> {
        if self.is_idle() {
            Ok(())
        } else {
            Err(CoreError::Internal("MySQL connection is not idle (a previous result set was not read to the end)".into()))
        }
    }

    /// Simple command answered by OK or ERR.
    async fn command(&mut self, cmd: u8, arg: &[u8]) -> Result<OkPacket> {
        self.ensure_idle()?;
        self.io.reset_seq();
        let mut packet = Vec::with_capacity(1 + arg.len());
        packet.push(cmd);
        packet.extend_from_slice(arg);
        self.in_result = true;
        self.write(&packet).await?;
        self.read().await?;
        self.in_result = false;
        if protocol::is_err(&self.buf) {
            return Err(protocol::parse_err(&self.buf)?.into_core(self.charset));
        }
        protocol::parse_ok(&self.buf, self.transactions).inspect_err(|_| self.broken = true)
    }

    /// `COM_QUERY` with already encoded SQL; reads the column definitions and their EOF.
    pub async fn query(&mut self, sql: &[u8]) -> Result<QueryStart> {
        self.ensure_idle()?;
        self.io.reset_seq();
        let mut packet = Vec::with_capacity(1 + sql.len());
        packet.push(COM_QUERY);
        packet.extend_from_slice(sql);
        self.in_result = true;
        self.write(&packet).await?;
        drop(packet);
        self.read().await?;
        if protocol::is_ok(&self.buf) {
            self.in_result = false;
            return protocol::parse_ok(&self.buf, self.transactions).map(QueryStart::Done).inspect_err(|_| self.broken = true);
        }
        if protocol::is_err(&self.buf) {
            self.in_result = false;
            return Err(protocol::parse_err(&self.buf)?.into_core(self.charset));
        }
        if self.buf.first() == Some(&0xFB) {
            // LOAD DATA LOCAL request: never asked for (no CLIENT_LOCAL_FILES).
            self.broken = true;
            return Err(CoreError::Connect("the server requested a local file (LOAD DATA LOCAL), which BulkDB never sends".into()));
        }
        let mut pos = 0;
        let count = protocol::read_lenenc_int(&self.buf, &mut pos)?.filter(|_| pos == self.buf.len());
        let Some(count) = count else {
            self.broken = true;
            return Err(CoreError::Connect("malformed MySQL packet: expected a result-set column count".into()));
        };
        let count = usize::try_from(count).unwrap_or(usize::MAX);
        let mut defs = Vec::with_capacity(count.min(4096));
        for _ in 0..count {
            self.read().await?;
            if protocol::is_err(&self.buf) {
                return Err(protocol::parse_err(&self.buf)?.into_core(self.charset));
            }
            defs.push(protocol::parse_column_def_320(&self.buf, self.long_flag, self.charset)?);
        }
        self.read().await?;
        if !protocol::is_eof(&self.buf) {
            self.broken = true;
            return Err(CoreError::Connect("malformed MySQL result: no EOF after the column definitions".into()));
        }
        Ok(QueryStart::Rows(defs))
    }

    /// Next row payload of the open result set (borrowed until the next call), `None` after
    /// the final EOF. An ERR mid-result is returned as the mapped error.
    pub async fn next_row(&mut self) -> Result<Option<&[u8]>> {
        if !self.in_result || self.broken {
            return Ok(None);
        }
        self.read().await?;
        if protocol::is_eof(&self.buf) {
            self.in_result = false;
            if self.buf.capacity() > KEEP_ROW_BUFFER {
                self.buf = Vec::new();
            }
            return Ok(None);
        }
        if protocol::is_err(&self.buf) {
            self.in_result = false;
            return Err(protocol::parse_err(&self.buf)?.into_core(self.charset));
        }
        Ok(Some(&self.buf))
    }

    /// Whole metadata result (at most `limit` rows, else `INTERNAL`; the connection is then
    /// left non-idle and must be dropped).
    pub async fn query_meta(&mut self, sql: &[u8], limit: usize) -> Result<MetaRows> {
        let defs = match self.query(sql).await? {
            QueryStart::Done(_) => return Ok(MetaRows::default()),
            QueryStart::Rows(defs) => defs,
        };
        let columns: Vec<MetaColumn> = defs.into_iter().map(|d| MetaColumn { name: d.name, collation: d.collation }).collect();
        let n = columns.len();
        let mut rows = Vec::new();
        let mut ranges = Vec::with_capacity(n);
        while let Some(row) = self.next_row().await? {
            if rows.len() >= limit {
                return Err(CoreError::Internal(format!("MySQL metadata query returned more than {limit} rows")));
            }
            protocol::split_text_row(row, n, &mut ranges)?;
            rows.push(ranges.iter().map(|r| r.map(|(a, b)| row[a..b].to_vec())).collect());
        }
        Ok(MetaRows { columns, rows })
    }

    /// Statement without a result set (a result set, if any, is read and discarded).
    pub async fn exec(&mut self, sql: &[u8]) -> Result<OkPacket> {
        match self.query(sql).await? {
            QueryStart::Done(ok) => Ok(ok),
            QueryStart::Rows(_) => {
                while self.next_row().await?.is_some() {}
                Ok(OkPacket::default())
            }
        }
    }

    pub async fn ping(&mut self) -> Result<()> {
        self.command(COM_PING, &[]).await.map(|_| ())
    }

    /// `COM_QUIT` and close (best effort, bounded by 1 s).
    pub async fn quit(mut self) {
        let fut = async {
            self.io.reset_seq();
            if self.io.write_payload(&[COM_QUIT]).await.is_ok() {
                let _ = self.io.get_mut().shutdown().await;
            }
        };
        let _ = tokio::time::timeout(Duration::from_secs(1), fut).await;
    }
}
