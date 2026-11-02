//! Text encodings: decoding legacy server bytes into UTF-8 (sources) and encoding UTF-8 into
//! the requested CSV output encoding (sinks). Design: `docs/mysql-compat.md`, sections 5 and 6.
//!
//! Owner: **encoding** (see the ownership table in the design doc). Everything here is
//! driver-neutral apart from the MySQL charset-name / collation-id lookup, which every MySQL
//! client (mysql_async based and the pre-4.1 legacy client) shares.
//!
//! * `Charset::EucKr` is WHATWG "euc-kr" (`encoding_rs::EUC_KR`), which is Windows-949
//!   (UHC / CP949), a strict superset of KS X 1001 EUC-KR. It is used for both names: decoding
//!   is lossless for every byte sequence a Korean legacy server holds, and encoding produces
//!   what Korean Excel and Windows tools call "ANSI".
//! * `Charset::Latin1` is WHATWG "windows-1252", which is exactly MySQL's `latin1` (cp1252 with
//!   the five undefined bytes mapped to U+0081, U+008D, U+008F, U+0090, U+009D).
//! * Decoding never fails: invalid sequences become U+FFFD and are reported through
//!   [`Decoded::replaced`] so the caller can count them (sources log the count, never the data).
//! * Encoding SQL fails (`INVALID`) on a character the server charset cannot represent: a
//!   silently replaced literal would change what the query selects.
//! * Encoding CSV output replaces such a character with [`REPLACEMENT_BYTE`] (`?`) and counts
//!   it; the count reaches `SinkReport::unmappable_chars` and a `JobResult` note. encoding_rs'
//!   own replacement (HTML numeric character references) must never be used.
//!
//! Memory: nothing here allocates per cell or per chunk once warm. [`decode_cell`] reuses the
//! caller's scratch `String`, [`TranscodeWriter`] reuses one output buffer and transcodes at
//! most [`TRANSCODE_CHUNK`] input bytes at a time, and [`Utf8Transcoder`] carries at most 3
//! bytes between calls.

use std::borrow::Cow;
use std::collections::HashMap;
use std::io::{self, Write};

use encoding_rs::{CoderResult, EncoderResult, Encoding};

use crate::error::{CoreError, Result};
use crate::types::CsvEncoding;

/// Byte written to EUC-KR output in place of a character CP949 cannot represent.
pub const REPLACEMENT_BYTE: u8 = b'?';

/// Most input bytes [`TranscodeWriter`] encodes per step, which bounds its output buffer.
pub const TRANSCODE_CHUNK: usize = 64 * 1024;

/// A text encoding BulkDB can read from a server or write to a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Charset {
    /// UTF-8 (MySQL `utf8` / `utf8mb3` / `utf8mb4`).
    Utf8,
    /// CP949 / UHC, a superset of EUC-KR (MySQL `euckr`, MySQL 4.0 `euc_kr`).
    EucKr,
    /// windows-1252 (MySQL `latin1`, also used for `ascii`).
    Latin1,
    /// Not text: bytes are passed through (MySQL `binary`, collation id 63).
    Binary,
}

impl Charset {
    /// Map a MySQL character-set name onto a charset BulkDB decodes. Accepts 4.1+ names
    /// (`euckr`, `latin1`, `utf8`, `utf8mb3`, `utf8mb4`, `binary`, `ascii`), MySQL 4.0
    /// `character_set` values (`euc_kr`, `latin1`, `latin1_de`) and common aliases
    /// (`euc-kr`, `cp949`, `uhc`, `utf-8`). Case-insensitive. `None` for anything else.
    pub fn from_mysql_name(name: &str) -> Option<Charset> {
        match name.trim().to_ascii_lowercase().as_str() {
            "utf8" | "utf8mb3" | "utf8mb4" | "utf-8" => Some(Charset::Utf8),
            "euckr" | "euc_kr" | "euc-kr" | "cp949" | "uhc" | "ks_c_5601-1987" => Some(Charset::EucKr),
            "latin1" | "latin1_de" | "ascii" | "usa7" | "windows-1252" | "cp1252" => Some(Charset::Latin1),
            "binary" => Some(Charset::Binary),
            _ => None,
        }
    }

    /// Human label for notes and errors (`UTF-8`, `EUC-KR (CP949)`, `latin1 (windows-1252)`).
    pub fn label(self) -> &'static str {
        match self {
            Charset::Utf8 => "UTF-8",
            Charset::EucKr => "EUC-KR (CP949)",
            Charset::Latin1 => "latin1 (windows-1252)",
            Charset::Binary => "binary",
        }
    }

    /// The encoding_rs encoding behind this charset; `None` for `Binary`.
    pub fn encoding(self) -> Option<&'static encoding_rs::Encoding> {
        match self {
            Charset::Utf8 => Some(encoding_rs::UTF_8),
            Charset::EucKr => Some(encoding_rs::EUC_KR),
            Charset::Latin1 => Some(encoding_rs::WINDOWS_1252),
            Charset::Binary => None,
        }
    }

    /// Whether text in this charset is stored as UTF-8 already (no transcoding needed).
    pub fn is_utf8_like(self) -> bool {
        matches!(self, Charset::Utf8 | Charset::Binary)
    }
}

impl From<CsvEncoding> for Charset {
    fn from(e: CsvEncoding) -> Self {
        match e {
            CsvEncoding::Utf8 => Charset::Utf8,
            CsvEncoding::EucKr => Charset::EucKr,
        }
    }
}

/// MariaDB numbers each `..._nopad_...` collation as its PAD SPACE sibling + 1024.
const MARIADB_NOPAD_OFFSET: u16 = 1024;

/// Well-known collation ids (MySQL 4.1 to 8.4, MariaDB 5.5 to 11.x) of the charsets BulkDB
/// decodes. Everything else (big5, sjis, gbk, ucs2, ...) is left out on purpose: such columns
/// fall back to the connection's legacy charset. Entries are inclusive id ranges `(first, last)`.
const BUILTIN_COLLATIONS: &[(Charset, &[(u16, u16)])] = &[
    (Charset::Binary, &[(63, 63)]),
    // latin1_german1_ci, latin1_swedish_ci, latin1_danish_ci, latin1_german2_ci, latin1_bin,
    // latin1_general_ci, latin1_general_cs, latin1_spanish_ci; ascii_general_ci, ascii_bin.
    (Charset::Latin1, &[(5, 5), (8, 8), (15, 15), (31, 31), (47, 49), (94, 94), (11, 11), (65, 65)]),
    // euckr_korean_ci, euckr_bin.
    (Charset::EucKr, &[(19, 19), (85, 85)]),
    // utf8(mb3)_general_ci, utf8mb3_tolower_ci (8.0), utf8_bin, the utf8_*_ci UCA set,
    // utf8_general_mysql500_ci; MariaDB utf8mb3_croatian_ci / myanmar_ci / thai_520_w2.
    (Charset::Utf8, &[(33, 33), (76, 76), (83, 83), (192, 215), (223, 223), (576, 578)]),
    // utf8mb4_general_ci, utf8mb4_bin, the utf8mb4_*_ci UCA set, the MySQL 8 utf8mb4_*_0900
    // set; MariaDB utf8mb4_croatian_ci / myanmar_ci / thai_520_w2.
    (Charset::Utf8, &[(45, 46), (224, 247), (255, 323), (608, 610)]),
];

/// PAD SPACE ids whose MariaDB NO PAD sibling (`id + 1024`) the builtin table also knows.
const MARIADB_NOPAD_BASES: &[u16] = &[8, 47, 11, 65, 19, 85, 33, 83, 192, 214, 45, 46, 224, 246];

/// MySQL collation id -> charset, for decoding result columns per column on a pass-through
/// connection (`character_set_results = NULL`, where each column definition carries the
/// collation id of the column itself). Filled from `SHOW COLLATION` at connect (4.1+); the
/// [`CollationMap::builtin`] table covers the well-known ids when that query is not possible.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CollationMap {
    ids: HashMap<u16, Charset>,
}

impl CollationMap {
    /// Empty map (every lookup is `None`).
    pub fn new() -> Self {
        CollationMap::default()
    }

    /// Well-known collation ids: 63 binary; latin1 5, 8, 15, 31, 47, 48, 49, 94 and ascii 11,
    /// 65; euckr 19, 85; utf8mb3 33, 76, 83, 192-215, 223, 576-578; utf8mb4 45, 46, 224-247,
    /// 255-323, 608-610; plus the MariaDB NO PAD variants (`id + 1024`) of the common ones.
    pub fn builtin() -> Self {
        let mut m = CollationMap::new();
        for (cs, ranges) in BUILTIN_COLLATIONS {
            for &(first, last) in ranges.iter() {
                for id in first..=last {
                    m.ids.insert(id, *cs);
                }
            }
        }
        for base in MARIADB_NOPAD_BASES {
            if let Some(cs) = m.get(*base) {
                m.ids.insert(base + MARIADB_NOPAD_OFFSET, cs);
            }
        }
        m
    }

    /// Record one `SHOW COLLATION` row (`Id`, `Charset`). Unknown charset names are skipped,
    /// so their columns fall back to the connection's legacy charset.
    pub fn insert_named(&mut self, id: u16, charset_name: &str) {
        if let Some(cs) = Charset::from_mysql_name(charset_name) {
            self.ids.insert(id, cs);
        }
    }

    pub fn get(&self, id: u16) -> Option<Charset> {
        self.ids.get(&id).copied()
    }

    pub fn len(&self) -> usize {
        self.ids.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }
}

/// Result of [`decode_cell`]: the text (borrowed from the input when it already was valid
/// UTF-8 / ASCII, else from the scratch buffer) and whether U+FFFD replacements happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Decoded<'a> {
    pub text: &'a str,
    pub replaced: bool,
}

/// Decode one cell (or one metadata value) from `cs` into UTF-8.
///
/// * `Utf8`: zero-copy when valid, else decoded into `scratch` with U+FFFD for every invalid
///   sequence (`replaced = true`).
/// * `EucKr` / `Latin1`: zero-copy when the bytes are ASCII, else decoded into `scratch`.
/// * `Binary`: treated as UTF-8 (callers route binary columns to `Cell::Bytes` instead).
///
/// `scratch` is cleared and refilled; its capacity is reused, so a warm scratch buffer means
/// no allocation per cell. The decoder itself is a stack value (encoding_rs decoders do not
/// allocate).
pub fn decode_cell<'a>(cs: Charset, bytes: &'a [u8], scratch: &'a mut String) -> Decoded<'a> {
    let enc = match cs {
        Charset::Utf8 | Charset::Binary => match std::str::from_utf8(bytes) {
            Ok(s) => return Decoded { text: s, replaced: false },
            Err(_) => encoding_rs::UTF_8,
        },
        Charset::EucKr | Charset::Latin1 => {
            if let Some(s) = ascii_str(bytes) {
                // ASCII is valid UTF-8 and identical in both encodings.
                return Decoded { text: s, replaced: false };
            }
            cs.encoding().unwrap_or(encoding_rs::UTF_8)
        }
    };
    let replaced = decode_into(enc, bytes, scratch);
    Decoded { text: scratch.as_str(), replaced }
}

/// `bytes` as `&str` when it is pure ASCII.
fn ascii_str(bytes: &[u8]) -> Option<&str> {
    if bytes.is_ascii() {
        std::str::from_utf8(bytes).ok()
    } else {
        None
    }
}

/// Decode all of `bytes` into `scratch` (cleared first) with replacement; returns whether any
/// replacement happened.
fn decode_into(enc: &'static Encoding, bytes: &[u8], scratch: &mut String) -> bool {
    scratch.clear();
    let mut decoder = enc.new_decoder_without_bom_handling();
    let mut src = bytes;
    let mut replaced = false;
    loop {
        // Worst case for what is left; `reserve` is a no-op while the capacity suffices.
        let need = decoder.max_utf8_buffer_length(src.len()).unwrap_or(src.len().saturating_mul(3)).max(4);
        scratch.reserve(need);
        let (result, read, had_replacements) = decoder.decode_to_string(src, scratch, true);
        replaced |= had_replacements;
        src = &src[read..];
        if result == CoderResult::InputEmpty {
            return replaced;
        }
    }
}

/// A character of UTF-8 text that a charset cannot represent, with its position in the text
/// that was checked: byte offset, 1-based line and 1-based column (counted in characters).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Unrepresentable {
    pub ch: char,
    pub offset: usize,
    pub line: usize,
    pub column: usize,
}

impl Unrepresentable {
    /// `ch` at byte `offset` of `text`.
    pub fn at(text: &str, ch: char, offset: usize) -> Self {
        let before = text.get(..offset).unwrap_or(text);
        let column = before.rsplit('\n').next().map(|l| l.chars().count()).unwrap_or(0) + 1;
        let line = before.matches('\n').count() + 1;
        Unrepresentable { ch, offset, line, column }
    }

    /// The same character located in `outer`, of which the checked text is the substring that
    /// starts at byte `base` (e.g. the guarded statement inside the SQL the user typed).
    pub fn within(self, outer: &str, base: usize) -> Self {
        Unrepresentable::at(outer, self.ch, base + self.offset)
    }

    /// `'😀' (U+1F600) at line 2, column 14`.
    pub fn describe(&self) -> String {
        format!("{:?} (U+{:04X}) at line {}, column {}", self.ch, self.ch as u32, self.line, self.column)
    }
}

/// The first character of `sql` that `cs` cannot represent, with its position in `sql`
/// (`None` when all of it can, always for UTF-8 / binary and for pure ASCII).
pub fn find_unrepresentable(cs: Charset, sql: &str) -> Option<Unrepresentable> {
    if cs.is_utf8_like() || sql.is_ascii() {
        return None;
    }
    encode_strict(cs, sql).err().map(|(c, at)| Unrepresentable::at(sql, c, at))
}

/// `INVALID` for SQL holding a character the connection charset cannot represent, at the
/// position recorded in `u` (callers locate it in the user's own SQL text).
pub fn sql_encoding_error(cs: Charset, u: &Unrepresentable) -> CoreError {
    CoreError::Invalid(format!(
        "the SQL contains {}, which the connection character set {} cannot represent; remove it or choose a connection encoding that can represent it",
        u.describe(),
        cs.label()
    ))
}

/// Encode extraction / metadata SQL (UTF-8, as the user and `sqlguard` saw it) into the
/// connection's SQL charset. `Utf8` and `Binary` borrow the input, as does pure-ASCII SQL for
/// every charset. A character the target cannot represent -> `INVALID` naming the character
/// and its line / column in `sql` (never a silent `?`). Callers that wrap the user's SQL (query
/// tag, `EXPLAIN` prefix) check the unwrapped text first ([`find_unrepresentable`]), so the
/// position refers to what the user typed.
///
/// Structural ASCII (`'`, `"`, `` ` ``, `\`, `;`, `-`, `#`, `/`, `*`) never appears inside a
/// CP949 or windows-1252 multi-byte sequence, so lexing the UTF-8 text (statement separator
/// checks) is equivalent to lexing the encoded bytes.
pub fn encode_sql(cs: Charset, sql: &str) -> Result<Cow<'_, [u8]>> {
    if cs.is_utf8_like() || sql.is_ascii() {
        return Ok(Cow::Borrowed(sql.as_bytes()));
    }
    match encode_strict(cs, sql) {
        Ok(bytes) => Ok(Cow::Owned(bytes)),
        Err((c, at)) => Err(sql_encoding_error(cs, &Unrepresentable::at(sql, c, at))),
    }
}

/// Encode `text` into `cs` (not UTF-8 / binary) without replacement: the bytes, or the first
/// character that cannot be represented and its byte offset in `text`.
fn encode_strict(cs: Charset, text: &str) -> std::result::Result<Vec<u8>, (char, usize)> {
    let enc = cs.encoding().unwrap_or(encoding_rs::UTF_8);
    let mut encoder = enc.new_encoder();
    let mut out = Vec::with_capacity(encoder.max_buffer_length_from_utf8_without_replacement(text.len()).unwrap_or(text.len()));
    let mut rest = text;
    loop {
        let (result, read) = encoder.encode_from_utf8_to_vec_without_replacement(rest, &mut out, true);
        rest = &rest[read..];
        match result {
            EncoderResult::InputEmpty => return Ok(out),
            EncoderResult::OutputFull => out.reserve(rest.len().saturating_mul(2).max(16)),
            // `read` includes the unmappable character.
            EncoderResult::Unmappable(c) => return Err((c, text.len() - rest.len() - c.len_utf8())),
        }
    }
}

/// Undo the classic Korean legacy double encoding in metadata read from a 4.1+ server:
/// CP949 bytes sent over a `latin1` connection were stored in the (utf8) data dictionary as
/// windows-1252 characters. When every character of `text` maps to windows-1252, at least one
/// is non-ASCII, and the resulting bytes decode in `legacy` without errors, return that
/// decoding; otherwise `None` (leave the text alone). Only for metadata (names, comments,
/// `SHOW CREATE TABLE`), only on `TextPlan::PassThrough` connections whose client charset is
/// `latin1`.
pub fn repair_latin1_mojibake(text: &str, legacy: Charset) -> Option<String> {
    if text.is_ascii() || matches!(legacy, Charset::Latin1 | Charset::Binary) {
        return None;
    }
    let target = legacy.encoding()?;
    let mut encoder = encoding_rs::WINDOWS_1252.new_encoder();
    // windows-1252 never needs more bytes than UTF-8 for the same text.
    let mut bytes = Vec::with_capacity(text.len());
    let mut rest = text;
    loop {
        let (result, read) = encoder.encode_from_utf8_to_vec_without_replacement(rest, &mut bytes, true);
        rest = &rest[read..];
        match result {
            EncoderResult::InputEmpty => break,
            EncoderResult::OutputFull => bytes.reserve(rest.len().max(16)),
            // Not windows-1252 text, so not the latin1 double encoding.
            EncoderResult::Unmappable(_) => return None,
        }
    }
    let decoded = target.decode_without_bom_handling_and_without_replacement(&bytes)?;
    (decoded != text).then(|| decoded.into_owned())
}

/// Number of bytes of the UTF-8 sequence that starts with `lead` (1 for anything that cannot
/// start a multi-byte sequence).
fn utf8_width(lead: u8) -> usize {
    match lead {
        0xC2..=0xDF => 2,
        0xE0..=0xEF => 3,
        0xF0..=0xF4 => 4,
        _ => 1,
    }
}

/// Streaming UTF-8 -> target encoder used by the CSV sink. Input may be split anywhere,
/// including inside a UTF-8 sequence (RawCsv chunks): up to 3 trailing bytes are carried to
/// the next call. Output never ends inside an encoded character. Unmappable characters become
/// [`REPLACEMENT_BYTE`] and are counted. Invalid UTF-8 input (cannot happen for Arrow-formatted
/// text; possible in RawCsv) is replaced the same way and counted, one `?` per maximal invalid
/// subsequence (the WHATWG / `from_utf8_lossy` rule).
#[derive(Debug)]
pub struct Utf8Transcoder {
    target: Charset,
    carry: [u8; 4],
    carry_len: usize,
    unmappable: u64,
}

impl Utf8Transcoder {
    /// `Utf8` (and `Binary`) is a pass-through; `EucKr` / `Latin1` transcode.
    pub fn new(target: Charset) -> Self {
        Utf8Transcoder { target, carry: [0; 4], carry_len: 0, unmappable: 0 }
    }

    pub fn target(&self) -> Charset {
        self.target
    }

    pub fn is_passthrough(&self) -> bool {
        self.target.is_utf8_like()
    }

    /// Append the encoding of `input` to `out`. Never fails today; the `Result` leaves room
    /// for a strict mode.
    pub fn push(&mut self, input: &[u8], out: &mut Vec<u8>) -> Result<()> {
        if self.is_passthrough() {
            out.extend_from_slice(input);
            return Ok(());
        }
        let mut input = input;
        if self.carry_len > 0 {
            input = self.complete_carry(input, out);
            if self.carry_len > 0 {
                // Input exhausted while the carried sequence is still incomplete.
                return Ok(());
            }
        }
        loop {
            match std::str::from_utf8(input) {
                Ok(s) => {
                    self.encode_str(s, out);
                    return Ok(());
                }
                Err(e) => {
                    let valid = e.valid_up_to();
                    if let Ok(s) = std::str::from_utf8(&input[..valid]) {
                        self.encode_str(s, out);
                    }
                    match e.error_len() {
                        Some(n) => {
                            self.replace(out);
                            input = &input[valid + n..];
                        }
                        None => {
                            // An incomplete sequence at the end (at most 3 bytes): keep it.
                            let tail = &input[valid..];
                            self.carry[..tail.len()].copy_from_slice(tail);
                            self.carry_len = tail.len();
                            return Ok(());
                        }
                    }
                }
            }
        }
    }

    /// Feed the carried prefix plus the first bytes of `input`; returns the unconsumed input.
    fn complete_carry<'i>(&mut self, input: &'i [u8], out: &mut Vec<u8>) -> &'i [u8] {
        let carried = self.carry_len;
        let width = utf8_width(self.carry[0]).max(carried + 1);
        let take = (width - carried).min(input.len());
        let mut tmp = [0u8; 4];
        tmp[..carried].copy_from_slice(&self.carry[..carried]);
        tmp[carried..carried + take].copy_from_slice(&input[..take]);
        let len = carried + take;
        match std::str::from_utf8(&tmp[..len]) {
            Ok(s) => {
                self.carry_len = 0;
                self.encode_str(s, out);
                &input[take..]
            }
            Err(e) => match e.error_len() {
                None => {
                    // Still incomplete: `take == input.len()`.
                    self.carry[..len].copy_from_slice(&tmp[..len]);
                    self.carry_len = len;
                    &input[take..]
                }
                Some(n) => {
                    // The carried prefix was valid so far, so the bad sequence covers all of
                    // it; continue right after the bad sequence.
                    self.carry_len = 0;
                    self.replace(out);
                    let consumed = (e.valid_up_to() + n).saturating_sub(carried).min(input.len());
                    &input[consumed..]
                }
            },
        }
    }

    /// Encode valid UTF-8 text, writing `?` (counted) for every unmappable character.
    fn encode_str(&mut self, s: &str, out: &mut Vec<u8>) {
        if s.is_empty() {
            return;
        }
        let Some(enc) = self.target.encoding() else {
            out.extend_from_slice(s.as_bytes());
            return;
        };
        let mut encoder = enc.new_encoder();
        let mut rest = s;
        loop {
            let need = encoder.max_buffer_length_from_utf8_without_replacement(rest.len()).unwrap_or(rest.len().saturating_mul(2)).max(4);
            out.reserve(need);
            let (result, read) = encoder.encode_from_utf8_to_vec_without_replacement(rest, out, false);
            rest = &rest[read..];
            match result {
                EncoderResult::InputEmpty => return,
                EncoderResult::OutputFull => {}
                // `read` includes the unmappable character.
                EncoderResult::Unmappable(_) => self.replace(out),
            }
        }
    }

    fn replace(&mut self, out: &mut Vec<u8>) {
        out.push(REPLACEMENT_BYTE);
        self.unmappable += 1;
    }

    /// Flush a dangling partial sequence (replaced and counted).
    pub fn finish(&mut self, out: &mut Vec<u8>) -> Result<()> {
        if self.carry_len > 0 {
            self.carry_len = 0;
            if self.is_passthrough() {
                return Ok(());
            }
            self.replace(out);
        }
        Ok(())
    }

    /// Characters replaced so far.
    pub fn unmappable(&self) -> u64 {
        self.unmappable
    }
}

/// `Write` adapter: UTF-8 bytes in, target-encoded bytes out to `inner`. The CSV sink puts it
/// between its `BufWriter` and the `CountingWriter`, so byte counters see encoded bytes.
/// For a pass-through target it forwards writes unchanged. Each `write` transcodes at most
/// [`TRANSCODE_CHUNK`] bytes into one reused buffer, so memory stays fixed however large the
/// writes are (a `BufWriter` passes writes larger than its capacity straight through).
pub struct TranscodeWriter<W: Write> {
    inner: W,
    transcoder: Utf8Transcoder,
    buf: Vec<u8>,
}

impl<W: Write> TranscodeWriter<W> {
    pub fn new(inner: W, target: Charset) -> Self {
        TranscodeWriter { inner, transcoder: Utf8Transcoder::new(target), buf: Vec::new() }
    }

    pub fn get_ref(&self) -> &W {
        &self.inner
    }

    pub fn get_mut(&mut self) -> &mut W {
        &mut self.inner
    }

    /// Characters replaced so far.
    pub fn unmappable(&self) -> u64 {
        self.transcoder.unmappable()
    }

    /// Flush the carried partial sequence and return the writer and the replacement count.
    /// The inner writer itself is not flushed.
    pub fn finish(mut self) -> io::Result<(W, u64)> {
        self.buf.clear();
        self.transcoder.finish(&mut self.buf).map_err(io::Error::other)?;
        self.inner.write_all(&self.buf)?;
        let n = self.transcoder.unmappable();
        Ok((self.inner, n))
    }
}

impl<W: Write> Write for TranscodeWriter<W> {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        if self.transcoder.is_passthrough() {
            return self.inner.write(data);
        }
        let step = &data[..data.len().min(TRANSCODE_CHUNK)];
        self.buf.clear();
        self.transcoder.push(step, &mut self.buf).map_err(io::Error::other)?;
        self.inner.write_all(&self.buf)?;
        Ok(step.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// `JobResult` note for `count` replaced characters (`None` when zero). English, no data
/// values: e.g. "12 character(s) cannot be represented in EUC-KR (CP949) and were written as '?'".
pub fn unmappable_note(count: u64, target: CsvEncoding) -> Option<String> {
    (count > 0).then(|| format!("{count} character(s) cannot be represented in {} and were written as '?'", Charset::from(target).label()))
}

/// The first character of `text` that `cs` cannot represent (`None` when all of it can, and
/// always for UTF-8 / binary). Used by the preflight check of `CsvOpts.null_as`.
pub fn first_unmappable(cs: Charset, text: &str) -> Option<char> {
    if cs.is_utf8_like() || text.is_ascii() {
        return None;
    }
    encode_strict(cs, text).err().map(|(c, _)| c)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// CP949 bytes of "한글" (KS X 1001 part, identical in EUC-KR).
    const HANGUL_CP949: &[u8] = b"\xC7\xD1\xB1\xDB";

    fn transcode_all(target: Charset, input: &[u8]) -> (Vec<u8>, u64) {
        let mut t = Utf8Transcoder::new(target);
        let mut out = Vec::new();
        t.push(input, &mut out).unwrap();
        t.finish(&mut out).unwrap();
        (out, t.unmappable())
    }

    #[test]
    fn mysql_names() {
        assert_eq!(Charset::from_mysql_name("euc_kr"), Some(Charset::EucKr));
        assert_eq!(Charset::from_mysql_name("EUCKR"), Some(Charset::EucKr));
        assert_eq!(Charset::from_mysql_name(" cp949 "), Some(Charset::EucKr));
        assert_eq!(Charset::from_mysql_name("utf8mb4"), Some(Charset::Utf8));
        assert_eq!(Charset::from_mysql_name("utf8mb3"), Some(Charset::Utf8));
        assert_eq!(Charset::from_mysql_name("latin1"), Some(Charset::Latin1));
        assert_eq!(Charset::from_mysql_name("latin1_de"), Some(Charset::Latin1));
        assert_eq!(Charset::from_mysql_name("ascii"), Some(Charset::Latin1));
        assert_eq!(Charset::from_mysql_name("binary"), Some(Charset::Binary));
        assert_eq!(Charset::from_mysql_name("sjis"), None);
        assert_eq!(Charset::from_mysql_name(""), None);
    }

    #[test]
    fn labels_encodings_and_csv_mapping() {
        assert_eq!(Charset::EucKr.label(), "EUC-KR (CP949)");
        assert_eq!(Charset::Utf8.label(), "UTF-8");
        assert_eq!(Charset::Latin1.label(), "latin1 (windows-1252)");
        assert_eq!(Charset::Binary.label(), "binary");
        assert_eq!(Charset::EucKr.encoding(), Some(encoding_rs::EUC_KR));
        assert_eq!(Charset::Latin1.encoding(), Some(encoding_rs::WINDOWS_1252));
        assert_eq!(Charset::Utf8.encoding(), Some(encoding_rs::UTF_8));
        assert_eq!(Charset::Binary.encoding(), None);
        assert_eq!(Charset::from(CsvEncoding::EucKr), Charset::EucKr);
        assert_eq!(Charset::from(CsvEncoding::Utf8), Charset::Utf8);
    }

    #[test]
    fn builtin_collations() {
        let m = CollationMap::builtin();
        assert!(!m.is_empty());
        for (id, cs) in [
            (63, Charset::Binary),
            (8, Charset::Latin1),
            (47, Charset::Latin1),
            (94, Charset::Latin1),
            (11, Charset::Latin1),
            (19, Charset::EucKr),
            (85, Charset::EucKr),
            (33, Charset::Utf8),
            (83, Charset::Utf8),
            (192, Charset::Utf8),
            (223, Charset::Utf8),
            (45, Charset::Utf8),
            (46, Charset::Utf8),
            (224, Charset::Utf8),
            (255, Charset::Utf8),
            (309, Charset::Utf8),
            (323, Charset::Utf8),
            (1043, Charset::EucKr),
            (1032, Charset::Latin1),
            (1069, Charset::Utf8),
        ] {
            assert_eq!(m.get(id), Some(cs), "collation id {id}");
        }
        // big5, sjis, gbk, ucs2, utf16: unknown, so the legacy charset applies.
        for id in [1u16, 13, 28, 35, 54, 216, 248, 324] {
            assert_eq!(m.get(id), None, "collation id {id}");
        }
        assert_eq!(CollationMap::new().len(), 0);
    }

    #[test]
    fn collations_from_show_collation() {
        let mut m = CollationMap::new();
        m.insert_named(19, "euckr");
        m.insert_named(8, "latin1");
        m.insert_named(2304, "utf8mb4");
        m.insert_named(13, "sjis");
        assert_eq!(m.len(), 3, "unknown charsets are skipped");
        assert_eq!(m.get(19), Some(Charset::EucKr));
        assert_eq!(m.get(2304), Some(Charset::Utf8));
        assert_eq!(m.get(13), None);
    }

    #[test]
    fn ascii_decodes_without_copy() {
        let mut s = String::new();
        let input = b"abc";
        let d = decode_cell(Charset::EucKr, input, &mut s);
        assert_eq!(d.text, "abc");
        assert!(!d.replaced);
        assert_eq!(d.text.as_ptr(), input.as_ptr(), "borrowed from the input");
        let d = decode_cell(Charset::Utf8, "한글".as_bytes(), &mut s);
        assert_eq!((d.text, d.replaced), ("한글", false));
    }

    #[test]
    fn cp949_decodes_including_uhc_extension() {
        let mut s = String::new();
        let d = decode_cell(Charset::EucKr, HANGUL_CP949, &mut s);
        assert_eq!((d.text, d.replaced), ("한글", false));
        // "똠" is not in KS X 1001 but is in CP949 (UHC extension).
        let (ddom, _, err) = encoding_rs::EUC_KR.encode("똠방각하");
        assert!(!err);
        let d = decode_cell(Charset::EucKr, &ddom, &mut s);
        assert_eq!((d.text, d.replaced), ("똠방각하", false));
    }

    #[test]
    fn invalid_cp949_is_replaced_and_flagged() {
        let mut s = String::new();
        // A lead byte followed by a byte that cannot trail, and a dangling lead byte.
        let d = decode_cell(Charset::EucKr, b"a\xC7\x0Ab\xB1", &mut s);
        assert!(d.replaced);
        assert_eq!(d.text, "a\u{FFFD}\nb\u{FFFD}");
    }

    #[test]
    fn latin1_decodes_cp1252() {
        let mut s = String::new();
        let d = decode_cell(Charset::Latin1, b"caf\xE9 \x80 \x81", &mut s);
        assert_eq!((d.text, d.replaced), ("café € \u{81}", false));
    }

    #[test]
    fn invalid_utf8_is_replaced() {
        let mut s = String::new();
        let d = decode_cell(Charset::Utf8, b"ok\xFFend\xE2\x82", &mut s);
        assert!(d.replaced);
        assert_eq!(d.text, "ok\u{FFFD}end\u{FFFD}");
        let d = decode_cell(Charset::Binary, b"\xC3\xA9", &mut s);
        assert_eq!((d.text, d.replaced), ("é", false));
    }

    #[test]
    fn decode_reuses_scratch_capacity() {
        let mut s = String::with_capacity(1024);
        let cap = s.capacity();
        for _ in 0..100 {
            let d = decode_cell(Charset::EucKr, HANGUL_CP949, &mut s);
            assert_eq!(d.text, "한글");
        }
        assert_eq!(s.capacity(), cap, "no reallocation once warm");
    }

    #[test]
    fn encode_sql_utf8_and_ascii_borrow() {
        let sql = "select '한글'";
        assert!(matches!(encode_sql(Charset::Utf8, sql).unwrap(), Cow::Borrowed(b) if b == sql.as_bytes()));
        assert!(matches!(encode_sql(Charset::Binary, sql).unwrap(), Cow::Borrowed(_)));
        assert!(matches!(encode_sql(Charset::EucKr, "select 1").unwrap(), Cow::Borrowed(b"select 1")));
    }

    #[test]
    fn encode_sql_to_cp949_and_latin1() {
        let got = encode_sql(Charset::EucKr, "select * from t where name = '한글'").unwrap();
        let mut want = b"select * from t where name = '".to_vec();
        want.extend_from_slice(HANGUL_CP949);
        want.push(b'\'');
        assert_eq!(got.as_ref(), want.as_slice());
        assert_eq!(encode_sql(Charset::Latin1, "select 'café'").unwrap().as_ref(), b"select 'caf\xE9'");
    }

    #[test]
    fn encode_sql_names_the_unmappable_character() {
        let err = encode_sql(Charset::EucKr, "select '한글'\nwhere x = '🚀'").unwrap_err();
        assert_eq!(err.code(), "INVALID");
        let msg = err.to_string();
        assert!(msg.contains("'🚀'") && msg.contains("U+1F680") && msg.contains("line 2") && msg.contains("column 12"), "{msg}");
        assert!(msg.contains("EUC-KR (CP949)"), "{msg}");
        let err = encode_sql(Charset::Latin1, "select '한'").unwrap_err();
        assert!(err.to_string().contains("U+D55C"), "{err}");
    }

    #[test]
    fn unrepresentable_positions() {
        assert_eq!(find_unrepresentable(Charset::EucKr, "select '한글'"), None, "Hangul is CP949");
        assert_eq!(find_unrepresentable(Charset::Utf8, "select '🚀'"), None);
        assert_eq!(find_unrepresentable(Charset::EucKr, "select 1"), None);
        let sql = "select '한글',\n  '🚀'";
        let u = find_unrepresentable(Charset::EucKr, sql).unwrap();
        assert_eq!((u.ch, u.line, u.column), ('🚀', 2, 4));
        assert_eq!(&sql[u.offset..u.offset + 4], "🚀");
        assert_eq!(u.describe(), "'🚀' (U+1F680) at line 2, column 4");
        // Re-anchored in the text the checked statement was cut from.
        let outer = format!("\n\n   {sql}");
        let w = u.within(&outer, 5);
        assert_eq!((w.ch, w.line, w.column, w.offset), ('🚀', 4, 4, u.offset + 5));
    }

    #[test]
    fn mojibake_is_repaired() {
        // CP949 "한글" read back as windows-1252 characters.
        let (mojibake, _, _) = encoding_rs::WINDOWS_1252.decode(HANGUL_CP949);
        assert_eq!(mojibake, "ÇÑ±Û");
        assert_eq!(repair_latin1_mojibake(&mojibake, Charset::EucKr).as_deref(), Some("한글"));
        // Trail bytes 0x81..0x9F come back as cp1252 punctuation or C1 controls.
        let (ddom, _, _) = encoding_rs::EUC_KR.encode("똠");
        let (moj, _, _) = encoding_rs::WINDOWS_1252.decode(&ddom);
        assert_eq!(repair_latin1_mojibake(&moj, Charset::EucKr).as_deref(), Some("똠"));
    }

    #[test]
    fn mojibake_repair_leaves_real_text_alone() {
        assert_eq!(repair_latin1_mojibake("customer_id", Charset::EucKr), None, "ASCII");
        assert_eq!(repair_latin1_mojibake("한글", Charset::EucKr), None, "already Korean: not windows-1252");
        assert_eq!(repair_latin1_mojibake("café", Charset::EucKr), None, "dangling lead byte is not CP949");
        assert_eq!(repair_latin1_mojibake("ÇÑ±Û", Charset::Latin1), None, "latin1 -> latin1 is the identity");
        assert_eq!(repair_latin1_mojibake("ÇÑ±Û", Charset::Binary), None);
    }

    #[test]
    fn transcoder_passthrough() {
        let mut t = Utf8Transcoder::new(Charset::Utf8);
        assert!(t.is_passthrough());
        assert_eq!(t.target(), Charset::Utf8);
        let mut out = Vec::new();
        t.push(b"\xED\x95", &mut out).unwrap();
        t.push(b"\x9C", &mut out).unwrap();
        t.finish(&mut out).unwrap();
        assert_eq!(out, "한".as_bytes());
        assert_eq!(t.unmappable(), 0);
        assert!(!Utf8Transcoder::new(Charset::EucKr).is_passthrough());
    }

    #[test]
    fn transcoder_encodes_cp949_and_counts_unmappable() {
        let (out, n) = transcode_all(Charset::EucKr, "id,한글\n1,🚀 x\n".as_bytes());
        let mut want = b"id,".to_vec();
        want.extend_from_slice(HANGUL_CP949);
        want.extend_from_slice(b"\n1,? x\n");
        assert_eq!(out, want);
        assert_eq!(n, 1);
        let (out, n) = transcode_all(Charset::Latin1, "é€😀".as_bytes());
        assert_eq!((out.as_slice(), n), (&b"\xE9\x80?"[..], 1));
    }

    #[test]
    fn transcoder_split_at_every_byte_offset() {
        let text = "a,한글,\"똠방\"\n🚀é,\u{7FF}\u{FFFF}z\r\n";
        let (whole, whole_n) = transcode_all(Charset::EucKr, text.as_bytes());
        let bytes = text.as_bytes();
        for cut in 0..=bytes.len() {
            let mut t = Utf8Transcoder::new(Charset::EucKr);
            let mut out = Vec::new();
            t.push(&bytes[..cut], &mut out).unwrap();
            t.push(&bytes[cut..], &mut out).unwrap();
            t.finish(&mut out).unwrap();
            assert_eq!(out, whole, "cut at {cut}");
            assert_eq!(t.unmappable(), whole_n, "cut at {cut}");
        }
        // One byte at a time.
        let mut t = Utf8Transcoder::new(Charset::EucKr);
        let mut out = Vec::new();
        for b in bytes {
            t.push(std::slice::from_ref(b), &mut out).unwrap();
        }
        t.finish(&mut out).unwrap();
        assert_eq!(out, whole);
    }

    #[test]
    fn transcoder_output_never_ends_inside_a_character() {
        let text = "한글한글";
        let mut t = Utf8Transcoder::new(Charset::EucKr);
        let mut out = Vec::new();
        t.push(&text.as_bytes()[..4], &mut out).unwrap();
        assert_eq!(out.as_slice(), &HANGUL_CP949[..2], "only the complete first character");
        t.push(&text.as_bytes()[4..], &mut out).unwrap();
        assert_eq!(out.len(), 8);
    }

    #[test]
    fn transcoder_replaces_invalid_utf8() {
        // A stray continuation byte, a truncated sequence followed by ASCII, an overlong form.
        let (out, n) = transcode_all(Charset::EucKr, b"a\x80b\xE0\xA0c\xC0\xAFd");
        assert_eq!(out, b"a?b?c??d");
        assert_eq!(n, 4);
        // Truncated sequence split across pushes, then broken by ASCII.
        let mut t = Utf8Transcoder::new(Charset::EucKr);
        let mut out = Vec::new();
        t.push(b"x\xF0\x9F", &mut out).unwrap();
        t.push(b"\x9A", &mut out).unwrap();
        t.push(b"y", &mut out).unwrap();
        t.finish(&mut out).unwrap();
        assert_eq!((out.as_slice(), t.unmappable()), (&b"x?y"[..], 1));
        // A dangling partial sequence at the end is flushed by `finish`.
        let (out, n) = transcode_all(Charset::EucKr, b"z\xED\x95");
        assert_eq!((out.as_slice(), n), (&b"z?"[..], 1));
    }

    #[test]
    fn transcode_writer_bounds_its_buffer_and_finishes() {
        let text = "한글,🚀\n".repeat(30_000); // ~ 330 KB of UTF-8
        let mut w = TranscodeWriter::new(Vec::new(), Charset::EucKr);
        w.write_all(text.as_bytes()).unwrap();
        assert!(w.buf.capacity() <= 4 * TRANSCODE_CHUNK, "buffer stays bounded: {}", w.buf.capacity());
        assert_eq!(w.unmappable(), 30_000);
        assert!(!w.get_ref().is_empty());
        let _ = w.get_mut();
        w.flush().unwrap();
        let (out, n) = w.finish().unwrap();
        assert_eq!(n, 30_000);
        let (decoded, _, err) = encoding_rs::EUC_KR.decode(&out);
        assert!(!err);
        assert_eq!(decoded, "한글,?\n".repeat(30_000));
    }

    #[test]
    fn transcode_writer_passthrough_and_dangling_tail() {
        let mut w = TranscodeWriter::new(Vec::new(), Charset::Utf8);
        w.write_all("한글".as_bytes()).unwrap();
        let (out, n) = w.finish().unwrap();
        assert_eq!((out.as_slice(), n), ("한글".as_bytes(), 0));
        let mut w = TranscodeWriter::new(Vec::new(), Charset::EucKr);
        w.write_all(b"ok\xEA").unwrap();
        let (out, n) = w.finish().unwrap();
        assert_eq!((out.as_slice(), n), (&b"ok?"[..], 1));
    }

    #[test]
    fn notes() {
        assert_eq!(unmappable_note(0, CsvEncoding::EucKr), None);
        let note = unmappable_note(12, CsvEncoding::EucKr).unwrap();
        assert!(note.starts_with("12 character(s)") && note.contains("EUC-KR (CP949)") && note.contains("'?'"), "{note}");
    }

    #[test]
    fn first_unmappable_character() {
        assert_eq!(first_unmappable(Charset::EucKr, "NULL"), None);
        assert_eq!(first_unmappable(Charset::EucKr, "널"), None);
        assert_eq!(first_unmappable(Charset::EucKr, "※·"), None, "KS X 1001 symbols");
        assert_eq!(first_unmappable(Charset::EucKr, "a🚫b"), Some('🚫'));
        assert_eq!(first_unmappable(Charset::Utf8, "🚫"), None);
        assert_eq!(first_unmappable(Charset::Latin1, "é한"), Some('한'));
    }
}
