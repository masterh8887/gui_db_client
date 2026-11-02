//! MySQL packet framing (identical in every protocol version). Owner: **legacy**.
//!
//! ```text
//! int<3>  payload length (little endian)
//! int<1>  sequence id (0 for the first packet of a command, +1 per packet, wraps at 255)
//! bytes   payload
//! ```
//!
//! A payload of 0xFFFFFF bytes or more is split into 0xFFFFFF-byte packets followed by one
//! shorter packet; a payload that is an exact multiple of 0xFFFFFF ends with an empty packet.
//! Readers join the parts. A sequence mismatch or EOF mid-packet is a `CONNECT` error.

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::error::{CoreError, Result};

/// Largest single packet payload.
pub const MAX_PAYLOAD: usize = 0xFF_FFFF;
/// Largest joined payload accepted from the server (MySQL's `max_allowed_packet` ceiling).
pub const MAX_JOINED_PAYLOAD: usize = 1 << 30;

/// An I/O failure on the MySQL socket: the session is gone (`CONNECT`, like the client errors
/// 2006 "server has gone away" / 2013 "lost connection").
pub(crate) fn io_error(e: std::io::Error) -> CoreError {
    if e.kind() == std::io::ErrorKind::UnexpectedEof {
        CoreError::Connect("lost connection to MySQL server: the server closed the connection".into())
    } else {
        CoreError::Connect(format!("lost connection to MySQL server: {e}"))
    }
}

/// Packet reader / writer over a buffered stream.
pub struct PacketIo<S> {
    stream: S,
    seq: u8,
}

impl<S: AsyncRead + AsyncWrite + Unpin + Send> PacketIo<S> {
    /// `next_seq` is the sequence id of the next packet (1 right after the greeting).
    pub fn new(stream: S, next_seq: u8) -> Self {
        PacketIo { stream, seq: next_seq }
    }

    /// Start a new command: the next packet written carries sequence id 0.
    pub fn reset_seq(&mut self) {
        self.seq = 0;
    }

    /// Read one logical payload into `out` (cleared first; capacity reused), joining
    /// continuation packets. Fails beyond [`MAX_JOINED_PAYLOAD`].
    pub async fn read_payload(&mut self, out: &mut Vec<u8>) -> Result<()> {
        out.clear();
        loop {
            let mut header = [0u8; 4];
            self.stream.read_exact(&mut header).await.map_err(io_error)?;
            let len = u32::from_le_bytes([header[0], header[1], header[2], 0]) as usize;
            let seq = header[3];
            if seq != self.seq {
                return Err(CoreError::Connect(format!("MySQL packets out of order (expected sequence {}, got {seq})", self.seq)));
            }
            self.seq = self.seq.wrapping_add(1);
            let start = out.len();
            if start + len > MAX_JOINED_PAYLOAD {
                return Err(CoreError::Connect(format!("MySQL packet larger than {MAX_JOINED_PAYLOAD} bytes")));
            }
            out.resize(start + len, 0);
            self.stream.read_exact(&mut out[start..]).await.map_err(io_error)?;
            if len < MAX_PAYLOAD {
                return Ok(());
            }
        }
    }

    /// Write one logical payload (split as needed) and flush.
    pub async fn write_payload(&mut self, payload: &[u8]) -> Result<()> {
        let mut rest = payload;
        loop {
            let n = rest.len().min(MAX_PAYLOAD);
            let len = (n as u32).to_le_bytes();
            let header = [len[0], len[1], len[2], self.seq];
            self.seq = self.seq.wrapping_add(1);
            self.stream.write_all(&header).await.map_err(io_error)?;
            self.stream.write_all(&rest[..n]).await.map_err(io_error)?;
            rest = &rest[n..];
            // A full packet is always followed by another one (possibly empty).
            if n < MAX_PAYLOAD {
                break;
            }
        }
        self.stream.flush().await.map_err(io_error)
    }

    pub fn get_mut(&mut self) -> &mut S {
        &mut self.stream
    }

    pub fn into_inner(self) -> S {
        self.stream
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(seq: u8, payload: &[u8]) -> Vec<u8> {
        let len = (payload.len() as u32).to_le_bytes();
        let mut v = vec![len[0], len[1], len[2], seq];
        v.extend_from_slice(payload);
        v
    }

    #[tokio::test]
    async fn reads_and_joins_continuations() {
        let big = vec![7u8; MAX_PAYLOAD];
        let mut wire = frame(0, &big);
        wire.extend(frame(1, b"tail"));
        wire.extend(frame(2, b"next"));
        let (mut a, b) = tokio::io::duplex(1 << 16);
        let writer = tokio::spawn(async move {
            a.write_all(&wire).await.unwrap();
            a
        });
        let mut io = PacketIo::new(b, 0);
        let mut out = Vec::new();
        io.read_payload(&mut out).await.unwrap();
        assert_eq!(out.len(), MAX_PAYLOAD + 4);
        assert_eq!(&out[MAX_PAYLOAD..], b"tail");
        io.read_payload(&mut out).await.unwrap();
        assert_eq!(out, b"next");
        drop(writer.await.unwrap());
    }

    #[tokio::test]
    async fn exact_multiple_ends_with_an_empty_packet() {
        let mut wire = frame(0, &vec![1u8; MAX_PAYLOAD]);
        wire.extend(frame(1, b""));
        let (mut a, b) = tokio::io::duplex(1 << 16);
        let writer = tokio::spawn(async move {
            a.write_all(&wire).await.unwrap();
            a
        });
        let mut io = PacketIo::new(b, 0);
        let mut out = Vec::new();
        io.read_payload(&mut out).await.unwrap();
        assert_eq!(out.len(), MAX_PAYLOAD);
        drop(writer.await.unwrap());
    }

    #[tokio::test]
    async fn rejects_out_of_order_and_eof() {
        let (mut a, b) = tokio::io::duplex(1024);
        a.write_all(&frame(5, b"x")).await.unwrap();
        let mut io = PacketIo::new(b, 0);
        let mut out = Vec::new();
        assert!(matches!(io.read_payload(&mut out).await, Err(CoreError::Connect(_))));
        drop(a);
        let (a, b) = tokio::io::duplex(1024);
        drop(a);
        let mut io = PacketIo::new(b, 0);
        assert!(matches!(io.read_payload(&mut out).await, Err(CoreError::Connect(_))));
    }

    #[tokio::test]
    async fn writes_exact_multiple_with_empty_tail() {
        let (a, mut b) = tokio::io::duplex(1 << 16);
        let reader = tokio::spawn(async move {
            let mut all = Vec::new();
            b.read_to_end(&mut all).await.unwrap();
            all
        });
        let mut io = PacketIo::new(a, 3);
        io.write_payload(&vec![1u8; MAX_PAYLOAD]).await.unwrap();
        io.write_payload(b"").await.unwrap();
        drop(io);
        let all = reader.await.unwrap();
        assert_eq!(all.len(), 4 + MAX_PAYLOAD + 4 + 4);
        assert_eq!(&all[..4], &[0xFF, 0xFF, 0xFF, 3]);
        assert_eq!(&all[4 + MAX_PAYLOAD..4 + MAX_PAYLOAD + 4], &[0, 0, 0, 4]);
        assert_eq!(&all[4 + MAX_PAYLOAD + 4..], &[0, 0, 0, 5]);
    }
}
