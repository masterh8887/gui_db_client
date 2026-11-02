//! Hashing helpers of the copy feature: SHA-256 for `plan_id` and ack keys (section 2), the
//! order-independent row checksum of 5.6 and hex formatting. Owner: copy-data.
//!
//! SHA-256 comes from the `sha2` crate (already in Cargo.lock); the standard test vectors pin it.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

/// SHA-256 digest of `data` (`sha2`, already in Cargo.lock).
pub fn sha256(data: &[u8]) -> [u8; 32] {
    use sha2::Digest;
    sha2::Sha256::digest(data).into()
}

/// Lower-case hex of `bytes`.
pub fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(DIGITS[(b >> 4) as usize] as char);
        s.push(DIGITS[(b & 0xf) as usize] as char);
    }
    s
}

/// Upper-case hex (MySQL `X'..'` literals, `UNHEX` input).
pub fn hex_upper_into(bytes: &[u8], out: &mut Vec<u8>) {
    const DIGITS: &[u8; 16] = b"0123456789ABCDEF";
    out.reserve(bytes.len() * 2);
    for b in bytes {
        out.push(DIGITS[(b >> 4) as usize]);
        out.push(DIGITS[(b & 0xf) as usize]);
    }
}

/// The first `n` hex digits of SHA-256 over `data`.
pub fn sha256_hex_prefix(data: &[u8], n: usize) -> String {
    let mut h = hex(&sha256(data));
    h.truncate(n);
    h
}

/// Order-independent table checksum (5.6): every row is hashed with `DefaultHasher` (fixed
/// keys, stable within one build) over its canonical written cells, and rows are combined by
/// wrapping addition, so the read-back order does not matter.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TableChecksum {
    pub sum: u64,
    pub rows: u64,
}

impl TableChecksum {
    pub fn add_row(&mut self, row_hash: u64) {
        self.sum = self.sum.wrapping_add(row_hash);
        self.rows += 1;
    }

    pub fn merge(&mut self, other: TableChecksum) {
        self.sum = self.sum.wrapping_add(other.sum);
        self.rows += other.rows;
    }

    /// The rows added since `before` was taken (sums are order-independent wrapping additions,
    /// so `after - before` is the checksum of exactly the appended rows).
    pub fn minus(self, before: TableChecksum) -> TableChecksum {
        TableChecksum { sum: self.sum.wrapping_sub(before.sum), rows: self.rows.saturating_sub(before.rows) }
    }

    /// `<rows>:<16 hex digits>`; what `CopyTableResult.checksum_source/target` carry.
    pub fn to_hex(&self) -> String {
        format!("{}:{:016x}", self.rows, self.sum)
    }
}

/// Builds the hash of one row from its canonical cells.
pub struct RowHasher(DefaultHasher);

impl Default for RowHasher {
    fn default() -> Self {
        RowHasher(DefaultHasher::new())
    }
}

impl RowHasher {
    /// One cell: `None` = NULL (a marker no text produces), else the canonical bytes.
    pub fn cell(&mut self, value: Option<&[u8]>) {
        match value {
            None => 0xFFu8.hash(&mut self.0),
            Some(b) => {
                0x01u8.hash(&mut self.0);
                b.hash(&mut self.0);
            }
        }
    }

    pub fn finish(self) -> u64 {
        self.0.finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_standard_vectors() {
        assert_eq!(hex(&sha256(b"")), "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
        assert_eq!(hex(&sha256(b"abc")), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        assert_eq!(hex(&sha256(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq")), "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1");
        // 1,000,000 x 'a' crosses many blocks.
        let million = vec![b'a'; 1_000_000];
        assert_eq!(hex(&sha256(&million)), "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0");
        assert_eq!(sha256_hex_prefix(b"abc", 12), "ba7816bf8f01");
    }

    #[test]
    fn checksum_is_order_independent_and_null_distinct() {
        let row = |cells: &[Option<&[u8]>]| {
            let mut h = RowHasher::default();
            for c in cells {
                h.cell(*c);
            }
            h.finish()
        };
        let a = row(&[Some(b"1"), Some(b"x")]);
        let b = row(&[Some(b"2"), None]);
        let mut x = TableChecksum::default();
        x.add_row(a);
        x.add_row(b);
        let mut y = TableChecksum::default();
        y.add_row(b);
        y.add_row(a);
        assert_eq!(x, y);
        assert_ne!(row(&[None]), row(&[Some(b"")]));
        assert_ne!(row(&[Some(b"ab"), Some(b"")]), row(&[Some(b"a"), Some(b"b")]));
        assert_eq!(x.rows, 2);
        assert!(x.to_hex().starts_with("2:"));
    }
}
