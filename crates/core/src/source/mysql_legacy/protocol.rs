//! Pre-4.1 command / response packets. Owner: **legacy**.
//!
//! * `COM_QUERY` (0x03) + SQL bytes; `COM_PING` (0x0E); `COM_QUIT` (0x01); `COM_INIT_DB` (0x02).
//! * OK: `0x00`, lenenc affected rows, lenenc insert id, `int<2>` status (when the server set
//!   `CLIENT_TRANSACTIONS`), optional message.
//! * ERR: `0xFF`, `int<2>` code, message to the end (no `#` + SQLSTATE before 4.1).
//! * EOF: `0xFE` with payload length < 9 (just the one byte before 4.1).
//! * Result set: lenenc column count, N `ColumnDefinition320`, EOF, rows, EOF (or ERR).
//! * `ColumnDefinition320` is five length-encoded strings (MySQL bug #16485: the internals
//!   documentation omits the length field):
//!   `table`, `name`, `length` (3 bytes, `int<3>`), `type` (1 byte),
//!   `flags + decimals` (3 bytes `int<2>` + `int<1>` with `CLIENT_LONG_FLAG`, else 2 bytes).
//! * Text row: one lenenc string per column, `0xFB` = NULL. Length prefixes: `< 0xFB` one
//!   byte, `0xFC` + `int<2>`, `0xFD` + `int<3>`, `0xFE` + `int<8>`.

use crate::encoding::Charset;
use crate::error::{CoreError, Result};
use crate::source::mysql_common::RawColumnDef;

pub const COM_QUIT: u8 = 0x01;
pub const COM_INIT_DB: u8 = 0x02;
pub const COM_QUERY: u8 = 0x03;
pub const COM_PROCESS_KILL: u8 = 0x0C;
pub const COM_PING: u8 = 0x0E;

fn malformed(what: &str) -> CoreError {
    CoreError::Connect(format!("malformed MySQL packet: {what}"))
}

/// Length-encoded integer at `*pos` (advanced). `Ok(None)` for `0xFB` (NULL).
pub fn read_lenenc_int(buf: &[u8], pos: &mut usize) -> Result<Option<u64>> {
    let first = *buf.get(*pos).ok_or_else(|| malformed("truncated length"))?;
    let width = match first {
        0..=0xFA => {
            *pos += 1;
            return Ok(Some(first as u64));
        }
        0xFB => {
            *pos += 1;
            return Ok(None);
        }
        0xFC => 2,
        0xFD => 3,
        0xFE => 8,
        0xFF => return Err(malformed("0xFF is not a length prefix")),
    };
    let bytes = buf.get(*pos + 1..*pos + 1 + width).ok_or_else(|| malformed("truncated length"))?;
    let mut v = [0u8; 8];
    v[..width].copy_from_slice(bytes);
    *pos += 1 + width;
    Ok(Some(u64::from_le_bytes(v)))
}

/// Length-encoded string at `*pos` (advanced), borrowed. `Ok(None)` for NULL.
pub fn read_lenenc_bytes<'a>(buf: &'a [u8], pos: &mut usize) -> Result<Option<&'a [u8]>> {
    let Some(len) = read_lenenc_int(buf, pos)? else { return Ok(None) };
    let end = usize::try_from(len).ok().and_then(|l| pos.checked_add(l)).filter(|e| *e <= buf.len()).ok_or_else(|| malformed("string runs past the packet"))?;
    let s = &buf[*pos..end];
    *pos = end;
    Ok(Some(s))
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OkPacket {
    pub affected_rows: u64,
    pub insert_id: u64,
    pub status: Option<u16>,
    pub message: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErrPacket {
    pub code: u16,
    /// Always `None` before 4.1.
    pub sqlstate: Option<String>,
    /// Raw message bytes (server charset).
    pub message: Vec<u8>,
}

impl ErrPacket {
    /// Decode the message with the connection charset and map through
    /// `mysql_common::map_server_error`.
    pub fn into_core(self, charset: Charset) -> CoreError {
        let mut scratch = String::new();
        let msg = crate::encoding::decode_cell(charset, &self.message, &mut scratch).text.to_string();
        crate::source::mysql_common::map_server_error(self.code, self.sqlstate.as_deref(), msg)
    }
}

pub fn is_ok(payload: &[u8]) -> bool {
    payload.first() == Some(&0x00)
}

pub fn is_err(payload: &[u8]) -> bool {
    payload.first() == Some(&0xFF)
}

/// EOF marker: `0xFE` and shorter than 9 bytes (a row can start with `0xFE` only as an
/// 8-byte length prefix, which makes it at least 9 bytes long).
pub fn is_eof(payload: &[u8]) -> bool {
    payload.first() == Some(&0xFE) && payload.len() < 9
}

pub fn parse_ok(payload: &[u8], transactions: bool) -> Result<OkPacket> {
    if !is_ok(payload) {
        return Err(malformed("expected an OK packet"));
    }
    let mut pos = 1;
    let affected_rows = read_lenenc_int(payload, &mut pos)?.unwrap_or(0);
    let insert_id = read_lenenc_int(payload, &mut pos)?.unwrap_or(0);
    let mut status = None;
    if transactions && payload.len() >= pos + 2 {
        status = Some(u16::from_le_bytes([payload[pos], payload[pos + 1]]));
        pos += 2;
    }
    // 4.0 sends the message as a length-encoded string; tolerate a bare one.
    let rest = &payload[pos..];
    let message = if rest.is_empty() {
        Vec::new()
    } else {
        let mut p = 0;
        match read_lenenc_bytes(rest, &mut p) {
            Ok(Some(m)) if p == rest.len() => m.to_vec(),
            _ => rest.to_vec(),
        }
    };
    Ok(OkPacket { affected_rows, insert_id, status, message })
}

pub fn parse_err(payload: &[u8]) -> Result<ErrPacket> {
    if !is_err(payload) || payload.len() < 3 {
        return Err(malformed("expected an ERR packet"));
    }
    let code = u16::from_le_bytes([payload[1], payload[2]]);
    let rest = &payload[3..];
    // 4.1+: '#' + 5-character SQLSTATE before the message.
    if rest.len() >= 6 && rest[0] == b'#' && rest[1..6].iter().all(|c| c.is_ascii_alphanumeric()) {
        let state = String::from_utf8_lossy(&rest[1..6]).into_owned();
        return Ok(ErrPacket { code, sqlstate: Some(state), message: rest[6..].to_vec() });
    }
    Ok(ErrPacket { code, sqlstate: None, message: rest.to_vec() })
}

fn le_uint(b: &[u8]) -> u32 {
    let mut v = [0u8; 4];
    let n = b.len().min(4);
    v[..n].copy_from_slice(&b[..n]);
    u32::from_le_bytes(v)
}

/// Parse a `ColumnDefinition320`; the name is decoded with `charset`. `collation` is `None`.
///
/// The flags field is read by its actual width (3 bytes with `CLIENT_LONG_FLAG`, else 2), so a
/// server that ignores the negotiated flag still parses; `long_flag` only documents what was
/// asked for. `COM_FIELD_LIST` appends a default value, which is ignored.
pub fn parse_column_def_320(payload: &[u8], long_flag: bool, charset: Charset) -> Result<RawColumnDef> {
    let _ = long_flag;
    let mut pos = 0;
    let mut field = |what: &str| -> Result<&[u8]> { read_lenenc_bytes(payload, &mut pos)?.ok_or_else(|| malformed(&format!("column definition: NULL {what}"))) };
    let _table = field("table")?;
    let name = field("name")?;
    let length = field("length")?;
    let ty = field("type")?;
    let flags = field("flags")?;
    if ty.is_empty() || length.is_empty() || length.len() > 4 || flags.len() < 2 {
        return Err(malformed("column definition fields have unexpected sizes"));
    }
    let (flags, decimals) = if flags.len() >= 3 { (u16::from_le_bytes([flags[0], flags[1]]), flags[2]) } else { (flags[0] as u16, flags[1]) };
    let mut scratch = String::new();
    let name = crate::encoding::decode_cell(charset, name, &mut scratch).text.to_string();
    Ok(RawColumnDef { name, column_type: ty[0], flags, decimals, length: le_uint(length), collation: None })
}

/// Split a text row into per-column byte ranges of `payload` (`None` = NULL), reusing `out`.
/// Fails when the row does not hold exactly `columns` values.
pub fn split_text_row(payload: &[u8], columns: usize, out: &mut Vec<Option<(usize, usize)>>) -> Result<()> {
    out.clear();
    let mut pos = 0;
    for _ in 0..columns {
        match read_lenenc_bytes(payload, &mut pos)? {
            None => out.push(None),
            Some(s) => out.push(Some((pos - s.len(), pos))),
        }
    }
    if pos != payload.len() {
        return Err(malformed(&format!("row has more than {columns} values")));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lenenc_ints() {
        let buf = [0x05, 0xFB, 0xFC, 0x34, 0x12, 0xFD, 1, 2, 3, 0xFE, 1, 0, 0, 0, 0, 0, 0, 1];
        let mut p = 0;
        assert_eq!(read_lenenc_int(&buf, &mut p).unwrap(), Some(5));
        assert_eq!(read_lenenc_int(&buf, &mut p).unwrap(), None);
        assert_eq!(read_lenenc_int(&buf, &mut p).unwrap(), Some(0x1234));
        assert_eq!(read_lenenc_int(&buf, &mut p).unwrap(), Some(0x030201));
        assert_eq!(read_lenenc_int(&buf, &mut p).unwrap(), Some(0x0100_0000_0000_0001));
        assert_eq!(p, buf.len());
        assert!(read_lenenc_int(&[0xFC, 1], &mut 0).is_err());
        assert!(read_lenenc_int(&[0xFF], &mut 0).is_err());
        assert!(read_lenenc_bytes(&[3, b'a'], &mut 0).is_err());
    }

    #[test]
    fn ok_err_eof() {
        // 4.0 OK with status (TRANSACTIONS) and no message.
        let ok = parse_ok(&[0x00, 0x01, 0x00, 0x02, 0x00], true).unwrap();
        assert_eq!(ok, OkPacket { affected_rows: 1, insert_id: 0, status: Some(2), message: vec![] });
        let ok = parse_ok(&[0x00, 0x00, 0x00, 0x02, b'h', b'i'], false).unwrap();
        assert_eq!(ok.status, None);
        assert_eq!(ok.message, b"hi");
        // 4.0 ERR: no SQLSTATE.
        let err = parse_err(b"\xff\x15\x04Access denied for user: 'x@localhost' (Using password: YES)").unwrap();
        assert_eq!(err.code, 1045);
        assert_eq!(err.sqlstate, None);
        assert!(matches!(err.into_core(Charset::Latin1), CoreError::Auth(m) if m.contains("Access denied")));
        // 4.1 ERR with SQLSTATE.
        let err = parse_err(b"\xff\x28\x04#42000Unknown database 'x'").unwrap();
        assert_eq!((err.code, err.sqlstate.as_deref(), err.message.as_slice()), (1064, Some("42000"), &b"Unknown database 'x'"[..]));
        assert!(is_eof(&[0xFE]));
        assert!(is_eof(&[0xFE, 0, 0, 2, 0]));
        assert!(!is_eof(&[0xFE, 1, 0, 0, 0, 0, 0, 0, 0]));
        assert!(parse_err(&[0xFF, 1]).is_err());
    }

    #[test]
    fn errors_map_to_core_codes() {
        let e = |code: u16| ErrPacket { code, sqlstate: None, message: b"m".to_vec() }.into_core(Charset::Latin1);
        assert!(matches!(e(1045), CoreError::Auth(_)));
        assert!(matches!(e(1317), CoreError::Cancelled));
        assert!(matches!(e(1146), CoreError::Query { code: Some(c), .. } if c == "1146"));
        // A Korean (EUC-KR errmsg.sys) message decodes with the connection charset.
        let (msg, _, _) = encoding_rs::EUC_KR.encode("테이블이 없습니다");
        let err = ErrPacket { code: 1146, sqlstate: None, message: msg.into_owned() }.into_core(Charset::EucKr);
        assert!(err.to_string().contains("테이블이 없습니다"), "{err}");
    }

    fn lenenc(out: &mut Vec<u8>, b: &[u8]) {
        out.push(b.len() as u8);
        out.extend_from_slice(b);
    }

    #[test]
    fn column_def_320() {
        let mut p = Vec::new();
        lenenc(&mut p, b"t");
        lenenc(&mut p, "이름".as_bytes());
        lenenc(&mut p, &[0x0B, 0x00, 0x00]);
        lenenc(&mut p, &[3]);
        lenenc(&mut p, &[0x23, 0x42, 0x00]);
        let def = parse_column_def_320(&p, true, Charset::Utf8).unwrap();
        assert_eq!(def, RawColumnDef { name: "이름".into(), column_type: 3, flags: 0x4223, decimals: 0, length: 11, collation: None });
        // Without LONG_FLAG: one flag byte + decimals.
        let mut p = Vec::new();
        lenenc(&mut p, b"t");
        lenenc(&mut p, b"d");
        lenenc(&mut p, &[0x0A, 0x00, 0x00]);
        lenenc(&mut p, &[0]);
        lenenc(&mut p, &[0x01, 0x02]);
        let def = parse_column_def_320(&p, false, Charset::Latin1).unwrap();
        assert_eq!((def.flags, def.decimals, def.column_type), (1, 2, 0));
        assert!(parse_column_def_320(&[1, b't'], true, Charset::Latin1).is_err());
    }

    #[test]
    fn text_rows() {
        let mut out = Vec::new();
        let row = [1, b'7', 0xFB, 0, 2, b'a', b'b'];
        split_text_row(&row, 4, &mut out).unwrap();
        assert_eq!(out, vec![Some((1, 2)), None, Some((4, 4)), Some((5, 7))]);
        assert!(split_text_row(&row, 3, &mut out).is_err());
        assert!(split_text_row(&row, 5, &mut out).is_err());
        let mut big = vec![0xFC, 0x00, 0x01];
        big.extend(vec![b'x'; 256]);
        split_text_row(&big, 1, &mut out).unwrap();
        assert_eq!(out, vec![Some((3, 259))]);
    }
}
