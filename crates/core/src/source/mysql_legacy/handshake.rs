//! Protocol-10 greeting, the pre-4.1 authentication packet and the 3.23 scramble.
//! Owner: **legacy**. Design: `docs/mysql-compat.md` section 7.
//!
//! Greeting (`Protocol::HandshakeV10`) as a 3.23 / 4.0 server sends it:
//!
//! ```text
//! int<1>       protocol version (10; 9 = pre-3.22, refused)
//! string<NUL>  server version ("4.0.27-standard-log")
//! int<4>       connection id
//! string[8]    scramble (auth-plugin-data part 1)
//! int<1>       filler 0x00
//! int<2>       capability flags (lower 16 bits; no CLIENT_PROTOCOL_41 = 0x0200)
//! int<1>       server charset number (4.0 table: 8 latin1, 19 euc_kr, ...)   [optional]
//! int<2>       status flags                                                   [optional]
//! string[13]   zero filler                                                    [optional]
//! ```
//!
//! A 4.1+ server additionally sends upper capability flags, the scramble length, 10 reserved
//! bytes, scramble part 2 (12 bytes + NUL) and, from 5.5.7, the auth plugin name. A server
//! that refuses the host sends an ERR packet (0xFF, int<2> code, message) instead of the
//! greeting (1129 host blocked, 1130 host not allowed).
//!
//! Client answer without CLIENT_PROTOCOL_41 (`HandshakeResponse320`, sequence id 1):
//!
//! ```text
//! int<2>       client flags (LONG_PASSWORD | LONG_FLAG | TRANSACTIONS [| CONNECT_WITH_DB])
//! int<3>       max packet size (0xFFFFFF)
//! string<NUL>  user (encoded with the connection's legacy charset)
//! if CONNECT_WITH_DB: string<NUL> scramble_323 response (8 bytes, empty for no password)
//!                     string<NUL> database
//! else:               string<EOF> scramble_323 response
//! ```
//!
//! A 4.0 server does not advertise `CLIENT_LONG_PASSWORD` (its greeting usually carries
//! `0x202c`), but it checks the client's bit to choose the 3.23 scramble over the 3.21 one, so
//! the client always sends it, exactly like the 4.0 libmysqlclient.
//!
//! The server answers OK (0x00) or ERR (0xFF). The password and the scramble are secrets:
//! never log them, never put them in errors, and [`Greeting`]'s `Debug` redacts the scramble.

use std::net::SocketAddr;
use std::time::Duration;

use tokio::io::AsyncWriteExt;
use tokio::net::{TcpSocket, TcpStream};

use super::packet::PacketIo;
use super::protocol;
use crate::encoding::Charset;
use crate::error::{CoreError, Result};
use crate::source::mysql::ServerVersion;

/// Capability flag bits (lower 16 bits are all a pre-4.1 server knows).
pub mod caps {
    pub const CLIENT_LONG_PASSWORD: u32 = 0x0001;
    pub const CLIENT_FOUND_ROWS: u32 = 0x0002;
    pub const CLIENT_LONG_FLAG: u32 = 0x0004;
    pub const CLIENT_CONNECT_WITH_DB: u32 = 0x0008;
    pub const CLIENT_NO_SCHEMA: u32 = 0x0010;
    pub const CLIENT_COMPRESS: u32 = 0x0020;
    pub const CLIENT_LOCAL_FILES: u32 = 0x0080;
    pub const CLIENT_IGNORE_SPACE: u32 = 0x0100;
    /// 4.1+ (`CLIENT_CHANGE_USER` in 4.0 headers, never sent by a 4.0 server).
    pub const CLIENT_PROTOCOL_41: u32 = 0x0200;
    pub const CLIENT_INTERACTIVE: u32 = 0x0400;
    pub const CLIENT_SSL: u32 = 0x0800;
    pub const CLIENT_TRANSACTIONS: u32 = 0x2000;
    /// 4.1.1+: 20-byte scramble (mysql_async needs it).
    pub const CLIENT_SECURE_CONNECTION: u32 = 0x8000;
}

/// Length of the pre-4.1 scramble and of the scramble_323 response.
pub const SCRAMBLE_LENGTH_323: usize = 8;

/// A parsed server greeting.
#[derive(Clone, PartialEq, Eq)]
pub struct Greeting {
    pub protocol_version: u8,
    pub server_version: String,
    pub connection_id: u32,
    /// 8 bytes (pre-4.1) or 20 bytes (4.1+). Secret: never logged.
    pub scramble: Vec<u8>,
    /// Lower 16 bits, plus the upper 16 bits on 4.1+.
    pub capabilities: u32,
    /// Server default charset number, when sent.
    pub charset: Option<u8>,
    pub status: Option<u16>,
}

impl std::fmt::Debug for Greeting {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Greeting")
            .field("protocol_version", &self.protocol_version)
            .field("server_version", &self.server_version)
            .field("connection_id", &self.connection_id)
            .field("scramble", &"<redacted>")
            .field("capabilities", &format_args!("{:#06x}", self.capabilities))
            .field("charset", &self.charset)
            .field("status", &self.status)
            .finish()
    }
}

impl Greeting {
    /// The server speaks the pre-4.1 protocol (no `CLIENT_PROTOCOL_41`): use the legacy client.
    pub fn is_pre41(&self) -> bool {
        self.capabilities & caps::CLIENT_PROTOCOL_41 == 0
    }

    pub fn version(&self) -> ServerVersion {
        ServerVersion::parse(&self.server_version)
    }

    /// 4.0 charset number -> MySQL 4.0 charset name (`8` -> `latin1`, `19` -> `euc_kr`, ...),
    /// for the plan when `SHOW VARIABLES LIKE 'character_set'` is not readable. The numbers
    /// are those of the 4.0 `sql/share/charsets/Index` (4.1 kept them as collation ids).
    pub fn charset_name_40(&self) -> Option<&'static str> {
        let name = match self.charset? {
            1 => "big5",
            2 => "czech",
            3 => "dec8",
            4 => "dos",
            5 => "german1",
            6 => "hp8",
            7 => "koi8_ru",
            8 => "latin1",
            9 => "latin2",
            10 => "swe7",
            11 => "usa7",
            12 => "ujis",
            13 => "sjis",
            14 => "cp1251",
            15 => "danish",
            16 => "hebrew",
            17 => "win1251",
            18 => "tis620",
            19 => "euc_kr",
            20 => "estonia",
            21 => "hungarian",
            22 => "koi8_ukr",
            23 => "win1251ukr",
            24 => "gb2312",
            25 => "greek",
            26 => "win1250",
            27 => "croat",
            28 => "gbk",
            29 => "cp1257",
            30 => "latin5",
            31 => "latin1_de",
            _ => return None,
        };
        Some(name)
    }

    /// Charset of the user name / password bytes and of messages before the plan is known:
    /// the greeting's charset when BulkDB can decode it, else latin1.
    pub fn auth_charset(&self) -> Charset {
        self.charset_name_40().and_then(Charset::from_mysql_name).filter(|c| *c != Charset::Binary).unwrap_or(Charset::Latin1)
    }
}

fn short_greeting() -> CoreError {
    CoreError::Connect("the server's greeting is truncated; is this a MySQL server?".into())
}

/// Parse the first packet payload of a connection. ERR instead of a greeting -> the mapped
/// server error; protocol version other than 10 -> `CONNECT` ("server too old").
pub fn parse_greeting(payload: &[u8]) -> Result<Greeting> {
    let Some(&protocol_version) = payload.first() else { return Err(short_greeting()) };
    if protocol::is_err(payload) {
        return Err(protocol::parse_err(payload)?.into_core(Charset::Latin1));
    }
    if protocol_version != 10 {
        return Err(CoreError::Connect(format!("unsupported MySQL protocol version {protocol_version} (MySQL 3.22 or newer is required; is this a MySQL server?)")));
    }
    let rest = &payload[1..];
    let nul = rest.iter().position(|b| *b == 0).ok_or_else(short_greeting)?;
    let server_version = String::from_utf8_lossy(&rest[..nul]).into_owned();
    let mut pos = 1 + nul + 1;
    let take = |pos: &mut usize, n: usize| -> Option<&[u8]> {
        let s = payload.get(*pos..*pos + n)?;
        *pos += n;
        Some(s)
    };
    let id = take(&mut pos, 4).ok_or_else(short_greeting)?;
    let connection_id = u32::from_le_bytes([id[0], id[1], id[2], id[3]]);
    let mut scramble = take(&mut pos, SCRAMBLE_LENGTH_323).ok_or_else(short_greeting)?.to_vec();
    take(&mut pos, 1).ok_or_else(short_greeting)?; // filler
    let mut capabilities = take(&mut pos, 2).map(|c| u16::from_le_bytes([c[0], c[1]]) as u32).unwrap_or(0);
    let charset = take(&mut pos, 1).map(|c| c[0]);
    let status = take(&mut pos, 2).map(|s| u16::from_le_bytes([s[0], s[1]]));
    if capabilities & caps::CLIENT_PROTOCOL_41 != 0 {
        // 4.1+: upper capabilities, scramble length, 10 reserved bytes, scramble part 2.
        if let Some(upper) = take(&mut pos, 2) {
            capabilities |= (u16::from_le_bytes([upper[0], upper[1]]) as u32) << 16;
        }
        let auth_len = take(&mut pos, 1).map(|l| l[0]).unwrap_or(0);
        take(&mut pos, 10);
        if capabilities & caps::CLIENT_SECURE_CONNECTION != 0 {
            let want = (auth_len as usize).saturating_sub(SCRAMBLE_LENGTH_323).max(13);
            let avail = payload.len().saturating_sub(pos).min(want);
            let part2 = take(&mut pos, avail).unwrap_or_default();
            let part2 = part2.strip_suffix(&[0]).unwrap_or(part2);
            scramble.extend_from_slice(part2);
        }
    }
    Ok(Greeting { protocol_version, server_version, connection_id, scramble, capabilities, charset, status })
}

/// MySQL's `hash_password()` (pre-4.1): two 31-bit words. Spaces and tabs in the password
/// are skipped. Arithmetic is `u32` wrapping (the low 31 bits equal the C `ulong` result on
/// 64-bit servers):
///
/// ```text
/// nr = 1345345333; add = 7; nr2 = 0x12345671
/// for b in password (skip ' ' and '\t'):
///     nr  ^= (((nr & 63) + add) * b) + (nr << 8)
///     nr2 += (nr2 << 8) ^ nr
///     add += b
/// result = [nr & 0x7FFFFFFF, nr2 & 0x7FFFFFFF]
/// ```
///
/// Test vector (MySQL manual): `OLD_PASSWORD('mypass')` = `6f8c114b58f2ce9e`.
pub fn hash_password_323(password: &[u8]) -> [u32; 2] {
    let mut nr: u32 = 1_345_345_333;
    let mut add: u32 = 7;
    let mut nr2: u32 = 0x1234_5671;
    for &b in password {
        if b == b' ' || b == b'\t' {
            continue;
        }
        let tmp = b as u32;
        nr ^= ((nr & 63).wrapping_add(add)).wrapping_mul(tmp).wrapping_add(nr << 8);
        nr2 = nr2.wrapping_add((nr2 << 8) ^ nr);
        add = add.wrapping_add(tmp);
    }
    [nr & 0x7FFF_FFFF, nr2 & 0x7FFF_FFFF]
}

/// MySQL's `my_rnd` generator as `scramble_323` / `check_scramble_323` use it.
struct Rnd323 {
    seed1: u64,
    seed2: u64,
}

impl Rnd323 {
    const MAX: u64 = 0x3FFF_FFFF;

    fn new(seed1: u32, seed2: u32) -> Self {
        Rnd323 { seed1: seed1 as u64 % Self::MAX, seed2: seed2 as u64 % Self::MAX }
    }

    fn next(&mut self) -> f64 {
        self.seed1 = (self.seed1 * 3 + self.seed2) % Self::MAX;
        self.seed2 = (self.seed1 + self.seed2 + 33) % Self::MAX;
        self.seed1 as f64 / Self::MAX as f64
    }
}

/// MySQL's `scramble_323(message, password)`: the 8-byte response to an 8-byte scramble.
/// Uses `my_rnd` exactly as the server does (f64 arithmetic, so the result matches
/// `check_scramble_323` bit for bit):
///
/// ```text
/// hp = hash_password_323(password); hm = hash_password_323(message[..8])
/// max = 0x3FFFFFFF; seed1 = (hp[0] ^ hm[0]) % max; seed2 = (hp[1] ^ hm[1]) % max
/// rnd(): seed1 = (seed1 * 3 + seed2) % max; seed2 = (seed1 + seed2 + 33) % max;
///        return seed1 as f64 / max as f64
/// out[i] = floor(rnd() * 31.0) + 64            for i in 0..8
/// extra  = floor(rnd() * 31.0); out[i] ^= extra
/// ```
///
/// An empty password yields an empty response (the caller sends just the terminator); this
/// function then returns zeros, and callers must check for the empty password first.
pub fn scramble_323(message: &[u8], password: &[u8]) -> [u8; SCRAMBLE_LENGTH_323] {
    let mut out = [0u8; SCRAMBLE_LENGTH_323];
    if password.is_empty() {
        return out;
    }
    let message = &message[..message.len().min(SCRAMBLE_LENGTH_323)];
    let hp = hash_password_323(password);
    let hm = hash_password_323(message);
    let mut rnd = Rnd323::new(hp[0] ^ hm[0], hp[1] ^ hm[1]);
    for o in out.iter_mut() {
        *o = ((rnd.next() * 31.0).floor() as u8) + 64;
    }
    let extra = (rnd.next() * 31.0).floor() as u8;
    for o in out.iter_mut() {
        *o ^= extra;
    }
    out
}

/// Client flags for the 320 response: `LONG_PASSWORD | LONG_FLAG`, plus `TRANSACTIONS` when
/// the server offers it and `CONNECT_WITH_DB` when a database is given and the server offers
/// it. `LONG_PASSWORD` and `LONG_FLAG` are always sent (a 4.0 server never advertises
/// `LONG_PASSWORD` but needs it for the 3.23 scramble; without `LONG_FLAG` column flags are one
/// byte). Never `CLIENT_PROTOCOL_41`, never `CLIENT_SSL` / `CLIENT_COMPRESS` /
/// `CLIENT_LOCAL_FILES` (not supported by this client).
pub fn client_flags_320(greeting: &Greeting, with_db: bool) -> u16 {
    let mut flags = caps::CLIENT_LONG_PASSWORD | caps::CLIENT_LONG_FLAG;
    if greeting.capabilities & caps::CLIENT_TRANSACTIONS != 0 {
        flags |= caps::CLIENT_TRANSACTIONS;
    }
    if with_db && greeting.capabilities & caps::CLIENT_CONNECT_WITH_DB != 0 {
        flags |= caps::CLIENT_CONNECT_WITH_DB;
    }
    flags as u16
}

/// Build the `HandshakeResponse320` payload (user / database already encoded with the
/// connection charset; `auth` is the scramble_323 response or empty).
pub fn auth_packet_320(flags: u16, user: &[u8], auth: &[u8], database: Option<&[u8]>) -> Vec<u8> {
    let mut out = Vec::with_capacity(5 + user.len() + auth.len() + database.map_or(0, |d| d.len()) + 3);
    out.extend_from_slice(&flags.to_le_bytes());
    out.extend_from_slice(&[0xFF, 0xFF, 0xFF]);
    out.extend_from_slice(user);
    out.push(0);
    match database {
        Some(db) if flags as u32 & caps::CLIENT_CONNECT_WITH_DB != 0 => {
            out.extend_from_slice(auth);
            out.push(0);
            out.extend_from_slice(db);
            out.push(0);
        }
        _ => out.extend_from_slice(auth),
    }
    out
}

async fn connect_tcp(addr: SocketAddr) -> std::io::Result<TcpStream> {
    let socket = if addr.is_ipv4() { TcpSocket::new_v4()? } else { TcpSocket::new_v6()? };
    socket.set_keepalive(true)?;
    let stream = socket.connect(addr).await?;
    stream.set_nodelay(true)?;
    Ok(stream)
}

async fn connect_and_greet(host: &str, port: u16) -> Result<(TcpStream, Greeting)> {
    let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host, port)).await.map_err(|e| CoreError::Connect(format!("cannot resolve {host}:{port}: {e}")))?.collect();
    let mut last = None;
    let mut stream = None;
    for addr in addrs {
        match connect_tcp(addr).await {
            Ok(s) => {
                stream = Some(s);
                break;
            }
            Err(e) => last = Some(e),
        }
    }
    let mut stream = stream.ok_or_else(|| match last {
        Some(e) => CoreError::Connect(format!("cannot connect to MySQL at {host}:{port}: {e}")),
        None => CoreError::Connect(format!("cannot resolve {host}:{port}: no addresses")),
    })?;
    let mut payload = Vec::new();
    PacketIo::new(&mut stream, 0).read_payload(&mut payload).await?;
    let greeting = parse_greeting(&payload)?;
    Ok((stream, greeting))
}

/// Connect to `host:port` (TCP keepalive, nodelay) within `timeout` and read the greeting
/// (sequence id 0). Returns the open socket, positioned so the next packet written carries
/// sequence id 1, and the parsed greeting. Used by the dispatcher in `auto` mode and by
/// [`super::MySqlLegacyDatabase::connect`].
pub async fn peek_greeting(host: &str, port: u16, timeout: Duration) -> Result<(TcpStream, Greeting)> {
    match tokio::time::timeout(timeout, connect_and_greet(host, port)).await {
        Ok(r) => r,
        Err(_) => Err(CoreError::Connect(format!("no MySQL greeting from {host}:{port} within {} s", timeout.as_secs_f32()))),
    }
}

/// User name of the login packet [`decline`] sends: no secret, and not expected to be an
/// account (at most 16 characters, the limit before MySQL 5.7.8).
pub const DECLINE_USER: &str = "bulkdb_nologin";
/// Upper bound for [`decline`]; it waits for one or two server answers only.
const DECLINE_TIMEOUT: Duration = Duration::from_secs(5);

/// Close a connection whose greeting was read but that BulkDB will not use (the server speaks
/// the other protocol, TLS is required but impossible, the login text cannot be encoded in the
/// server charset, ...) **without leaving the server an interrupted handshake**.
///
/// MySQL counts a client that disconnects after the greeting without a well-formed login packet
/// (or that sends a malformed one) toward `max_connect_errors` for its host (4.0:
/// `check_connection` -> `inc_host_errors`; 5.6+: host-cache handshake errors) and blocks the
/// host once the limit (default 10) is passed, until a DBA runs `FLUSH HOSTS`. A failed login
/// does not count (on 4.0 a well-formed packet even resets the counter). So this sends a login
/// packet that carries no secret, user [`DECLINE_USER`] with an empty password and no database
/// (`HandshakeResponse320` after a pre-4.1 greeting, `HandshakeResponse41` otherwise), and
/// waits for the server's verdict: ERR (the expected "access denied") -> close; OK (an
/// anonymous or password-less account matched) -> `COM_QUIT`, then close; a 4.1 - 5.5
/// old-password request -> a dummy 8-byte scramble (wrong by construction), the final answer,
/// then close. Best effort, bounded by `timeout` (at most 5 s); errors are ignored.
pub async fn decline(stream: TcpStream, greeting: &Greeting, timeout: Duration) {
    let work = async move {
        let mut io = PacketIo::new(stream, 1);
        let login =
            if greeting.is_pre41() { auth_packet_320(client_flags_320(greeting, false), DECLINE_USER.as_bytes(), &[], None) } else { decline_packet_41(greeting) };
        io.write_payload(&login).await?;
        let mut reply = Vec::new();
        io.read_payload(&mut reply).await?;
        if reply == [0xFE] {
            let mut wrong = [b'@'; SCRAMBLE_LENGTH_323 + 1];
            wrong[SCRAMBLE_LENGTH_323] = 0;
            io.write_payload(&wrong).await?;
            io.read_payload(&mut reply).await?;
        }
        if protocol::is_ok(&reply) {
            io.reset_seq();
            io.write_payload(&[protocol::COM_QUIT]).await?;
        }
        let _ = io.get_mut().shutdown().await;
        Ok::<(), CoreError>(())
    };
    if let Ok(Err(e)) = tokio::time::timeout(timeout.min(DECLINE_TIMEOUT), work).await {
        tracing::debug!("mysql: closing an unused handshake: {e}");
    }
}

/// `HandshakeResponse41` without credentials, for [`decline`]: flags `LONG_PASSWORD |
/// LONG_FLAG | PROTOCOL_41` plus `TRANSACTIONS` / `SECURE_CONNECTION` when offered, max packet
/// 16 MB, the greeting's charset, 23 zero bytes, [`DECLINE_USER`] NUL and an empty auth
/// response (one zero byte: a length of 0 with `SECURE_CONNECTION`, an empty NUL-terminated
/// string without it). No `CLIENT_PLUGIN_AUTH`, `CLIENT_SSL` or `CONNECT_WITH_DB`.
pub fn decline_packet_41(greeting: &Greeting) -> Vec<u8> {
    let flags = caps::CLIENT_LONG_PASSWORD
        | caps::CLIENT_LONG_FLAG
        | caps::CLIENT_PROTOCOL_41
        | (greeting.capabilities & (caps::CLIENT_TRANSACTIONS | caps::CLIENT_SECURE_CONNECTION));
    let mut out = Vec::with_capacity(32 + DECLINE_USER.len() + 2);
    out.extend_from_slice(&flags.to_le_bytes());
    out.extend_from_slice(&(16u32 << 20).to_le_bytes());
    out.push(greeting.charset.unwrap_or(8));
    out.extend_from_slice(&[0; 23]);
    out.extend_from_slice(DECLINE_USER.as_bytes());
    out.push(0);
    out.push(0);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A MySQL 4.0.27 greeting as the 4.0 `check_connection()` builds it: caps 0x202c
    /// (LONG_FLAG | CONNECT_WITH_DB | COMPRESS | TRANSACTIONS), charset 19 (euc_kr),
    /// status 2 (AUTOCOMMIT), 13 zero bytes.
    fn greeting_40() -> Vec<u8> {
        let mut g = vec![10];
        g.extend_from_slice(b"4.0.27-standard-log\0");
        g.extend_from_slice(&42u32.to_le_bytes());
        g.extend_from_slice(b"Hk/%@x2Q");
        g.push(0);
        g.extend_from_slice(&0x202Cu16.to_le_bytes());
        g.push(19);
        g.extend_from_slice(&2u16.to_le_bytes());
        g.extend_from_slice(&[0; 13]);
        g
    }

    /// A MySQL 4.1.22 / 5.0.x greeting: caps 0xa22c, upper caps 0, auth length 0 (4.1/5.0
    /// send 0 there), 10 reserved zero bytes, 12 more scramble bytes + NUL.
    fn greeting_41(version: &str, upper: u16, auth_len: u8, plugin: Option<&str>) -> Vec<u8> {
        let mut g = vec![10];
        g.extend_from_slice(version.as_bytes());
        g.push(0);
        g.extend_from_slice(&7u32.to_le_bytes());
        g.extend_from_slice(b"abcdefgh");
        g.push(0);
        g.extend_from_slice(&0xA22Cu16.to_le_bytes());
        g.push(8);
        g.extend_from_slice(&2u16.to_le_bytes());
        g.extend_from_slice(&upper.to_le_bytes());
        g.push(auth_len);
        g.extend_from_slice(&[0; 10]);
        g.extend_from_slice(b"ijklmnopqrst\0");
        if let Some(p) = plugin {
            g.extend_from_slice(p.as_bytes());
            g.push(0);
        }
        g
    }

    #[test]
    fn parses_40_greeting() {
        let g = parse_greeting(&greeting_40()).unwrap();
        assert!(g.is_pre41());
        assert_eq!(g.server_version, "4.0.27-standard-log");
        assert_eq!(g.connection_id, 42);
        assert_eq!(g.scramble, b"Hk/%@x2Q");
        assert_eq!(g.capabilities, 0x202C);
        assert_eq!(g.charset, Some(19));
        assert_eq!(g.status, Some(2));
        assert_eq!(g.charset_name_40(), Some("euc_kr"));
        assert_eq!(g.auth_charset(), Charset::EucKr);
        assert_eq!((g.version().major, g.version().minor, g.version().patch), (4, 0, 27));
        assert!(!format!("{g:?}").contains("Hk/%"), "Debug must redact the scramble");
    }

    #[test]
    fn parses_323_greeting_without_tail() {
        // 3.23 servers before 3.23.x sometimes stop after the capabilities.
        let mut g = greeting_40();
        g.truncate(g.len() - 18);
        let g = parse_greeting(&g).unwrap();
        assert_eq!((g.charset, g.status), (None, None));
        assert_eq!(g.auth_charset(), Charset::Latin1);
    }

    #[test]
    fn parses_41_and_55_greetings() {
        let g = parse_greeting(&greeting_41("4.1.22-standard", 0, 0, None)).unwrap();
        assert!(!g.is_pre41());
        assert_eq!(g.scramble, b"abcdefghijklmnopqrst");
        let g = parse_greeting(&greeting_41("5.5.62", 0x0008, 21, Some("mysql_native_password"))).unwrap();
        assert_eq!(g.capabilities, 0x0008_A22C);
        assert_eq!(g.scramble.len(), 20);
    }

    #[test]
    fn greeting_errors() {
        assert!(matches!(parse_greeting(b""), Err(CoreError::Connect(_))));
        assert!(matches!(parse_greeting(&[9, b'3', 0]), Err(CoreError::Connect(m)) if m.contains("protocol version 9")));
        let err = parse_greeting(b"\xff\x6a\x04Host '10.0.0.1' is not allowed to connect to this MySQL server").unwrap_err();
        assert!(matches!(err, CoreError::Connect(m) if m.contains("not allowed") && m.contains("1130")));
        assert!(matches!(parse_greeting(&greeting_40()[..20]), Err(CoreError::Connect(_))));
    }

    #[test]
    fn old_password_hashes() {
        // MySQL manual: OLD_PASSWORD('mypass'); 4.0 manual: PASSWORD('badpwd').
        assert_eq!(hash_password_323(b"mypass"), [0x6f8c_114b, 0x58f2_ce9e]);
        assert_eq!(hash_password_323(b"badpwd"), [0x7f84_5540, 0x57dd_964b]);
        // Spaces and tabs are skipped.
        assert_eq!(hash_password_323(b"my pass\t"), hash_password_323(b"mypass"));
    }

    #[test]
    fn scramble_323_vectors() {
        // Vectors from go-sql-driver/mysql (TestOldPass), computed with the C reference.
        let msg = [9u8, 8, 7, 6, 5, 4, 3, 2];
        let hex = |b: [u8; 8]| b.iter().map(|x| format!("{x:02x}")).collect::<String>();
        assert_eq!(hex(scramble_323(&msg, b" pass")), "47575c5a435b4251");
        assert_eq!(hex(scramble_323(&msg, b"pass ")), "47575c5a435b4251");
        assert_eq!(hex(scramble_323(&msg, b"123\t456")), "575c47505b5b5559");
        assert_eq!(hex(scramble_323(&msg, b"C0mpl!ca ted#PASS123")), "5d5d554849584a45");
        // Every byte is in 64..=126 before the XOR and the response never contains NUL.
        assert!(scramble_323(b"Hk/%@x2Q", b"mypass").iter().all(|b| *b != 0));
        // Only the first 8 bytes of a 20-byte 4.1 scramble count.
        assert_eq!(scramble_323(b"abcdefghijklmnopqrst", b"x"), scramble_323(b"abcdefgh", b"x"));
    }

    #[test]
    fn auth_packet_layout() {
        let g = parse_greeting(&greeting_40()).unwrap();
        let flags = client_flags_320(&g, true);
        assert_eq!(flags as u32, caps::CLIENT_LONG_PASSWORD | caps::CLIENT_LONG_FLAG | caps::CLIENT_TRANSACTIONS | caps::CLIENT_CONNECT_WITH_DB);
        let p = auth_packet_320(flags, b"bulk", b"ABCDEFGH", Some(b"shop"));
        assert_eq!(p, b"\x0d\x20\xff\xff\xffbulk\0ABCDEFGH\0shop\0");
        let flags = client_flags_320(&g, false);
        assert_eq!(flags, 0x2005);
        assert_eq!(auth_packet_320(flags, b"bulk", b"", Some(b"shop")), b"\x05\x20\xff\xff\xffbulk\0");
        let mut no_tx = g.clone();
        no_tx.capabilities = 0;
        assert_eq!(client_flags_320(&no_tx, true), 0x0005);
    }

    /// The credential-free 4.1 login [`decline`] sends: well-formed (a server counts a short or
    /// malformed packet like a dropped connection), no secret, no TLS / plugin flags.
    #[test]
    fn decline_packet_41_layout() {
        let g = parse_greeting(&greeting_41("5.0.96", 0, 0, None)).unwrap();
        let p = decline_packet_41(&g);
        let flags = u32::from_le_bytes([p[0], p[1], p[2], p[3]]);
        assert_eq!(flags, caps::CLIENT_LONG_PASSWORD | caps::CLIENT_LONG_FLAG | caps::CLIENT_PROTOCOL_41 | caps::CLIENT_TRANSACTIONS | caps::CLIENT_SECURE_CONNECTION);
        assert_eq!(&p[4..8], &(16u32 << 20).to_le_bytes());
        assert_eq!(p[8], 8, "the greeting's charset");
        assert!(p[9..32].iter().all(|b| *b == 0));
        assert_eq!(&p[32..], b"bulkdb_nologin\0\0");
        assert!(DECLINE_USER.len() <= 16);
        // Pre-4.1: the HandshakeResponse320 is at least MIN_HANDSHAKE_SIZE (6) bytes.
        let g40 = parse_greeting(&greeting_40()).unwrap();
        let p = auth_packet_320(client_flags_320(&g40, false), DECLINE_USER.as_bytes(), &[], None);
        assert_eq!(p, b"\x05\x20\xff\xff\xffbulkdb_nologin\0");
    }
}
