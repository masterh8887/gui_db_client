//! Column-wise access to Arrow cells for the writers and the checksum (no row objects): one
//! [`Col`] per batch column is resolved once, then cells are read by row index. Covers what the
//! source readers build (`source::arrow_batch`): Boolean, Int64, Float64, Decimal128, Utf8,
//! Binary, Date32, Time64(µs), Timestamp(µs[, tz]); anything else falls back to Arrow's
//! display formatter. Owner: copy-data.

use std::borrow::Cow;

use arrow::util::display::{ArrayFormatter, FormatOptions};
use arrow_array::cast::AsArray;
use arrow_array::types::{
    Date32Type, Decimal128Type, Float32Type, Float64Type, Int16Type, Int32Type, Int64Type, Int8Type, Time64MicrosecondType, TimestampMicrosecondType, UInt16Type,
    UInt32Type, UInt64Type, UInt8Type,
};
use arrow_array::{Array, ArrayRef};
use arrow_schema::{DataType, TimeUnit};

use crate::copy::hash::RowHasher;
use crate::error::{CoreError, Result};

/// One cell, borrowed from its array where possible.
#[derive(Debug, Clone, PartialEq)]
pub enum Cell<'a> {
    Null,
    Str(&'a str),
    Bytes(&'a [u8]),
    Int(i64),
    UInt(u64),
    Float(f64),
    Bool(bool),
    /// Unscaled value and scale.
    Decimal(i128, i8),
    /// Days since the epoch.
    Date(i32),
    /// Microseconds since midnight.
    Time(i64),
    /// Microseconds since the epoch (naive / UTC).
    Timestamp(i64),
    /// Formatted by Arrow (types the readers never build).
    Other(String),
}

/// A batch column with its type resolved once.
pub struct Col<'a> {
    array: &'a ArrayRef,
    kind: ColType,
    formatter: Option<ArrayFormatter<'a>>,
}

#[derive(Clone, Copy)]
enum ColType {
    Null,
    Bool,
    I8,
    I16,
    I32,
    I64,
    U8,
    U16,
    U32,
    U64,
    F32,
    F64,
    Dec(i8),
    Utf8,
    LargeUtf8,
    Binary,
    LargeBinary,
    Date32,
    TimeMicros,
    TsMicros,
    Other,
}

impl<'a> Col<'a> {
    pub fn new(array: &'a ArrayRef) -> Result<Col<'a>> {
        let kind = match array.data_type() {
            DataType::Null => ColType::Null,
            DataType::Boolean => ColType::Bool,
            DataType::Int8 => ColType::I8,
            DataType::Int16 => ColType::I16,
            DataType::Int32 => ColType::I32,
            DataType::Int64 => ColType::I64,
            DataType::UInt8 => ColType::U8,
            DataType::UInt16 => ColType::U16,
            DataType::UInt32 => ColType::U32,
            DataType::UInt64 => ColType::U64,
            DataType::Float32 => ColType::F32,
            DataType::Float64 => ColType::F64,
            DataType::Decimal128(_, s) => ColType::Dec(*s),
            DataType::Utf8 => ColType::Utf8,
            DataType::LargeUtf8 => ColType::LargeUtf8,
            DataType::Binary => ColType::Binary,
            DataType::LargeBinary => ColType::LargeBinary,
            DataType::Date32 => ColType::Date32,
            DataType::Time64(TimeUnit::Microsecond) => ColType::TimeMicros,
            DataType::Timestamp(TimeUnit::Microsecond, _) => ColType::TsMicros,
            _ => ColType::Other,
        };
        let formatter = if matches!(kind, ColType::Other) {
            Some(ArrayFormatter::try_new(array.as_ref(), &FormatOptions::default()).map_err(|e| CoreError::Internal(format!("arrow: {e}")))?)
        } else {
            None
        };
        Ok(Col { array, kind, formatter })
    }

    /// Whether the column carries raw bytes.
    pub fn is_binary(&self) -> bool {
        matches!(self.kind, ColType::Binary | ColType::LargeBinary)
    }

    /// Whether the column carries text.
    pub fn is_text(&self) -> bool {
        matches!(self.kind, ColType::Utf8 | ColType::LargeUtf8)
    }

    pub fn cell(&self, row: usize) -> Cell<'a> {
        let a = self.array;
        if a.is_null(row) {
            return Cell::Null;
        }
        match self.kind {
            ColType::Null => Cell::Null,
            ColType::Bool => Cell::Bool(a.as_boolean().value(row)),
            ColType::I8 => Cell::Int(a.as_primitive::<Int8Type>().value(row) as i64),
            ColType::I16 => Cell::Int(a.as_primitive::<Int16Type>().value(row) as i64),
            ColType::I32 => Cell::Int(a.as_primitive::<Int32Type>().value(row) as i64),
            ColType::I64 => Cell::Int(a.as_primitive::<Int64Type>().value(row)),
            ColType::U8 => Cell::UInt(a.as_primitive::<UInt8Type>().value(row) as u64),
            ColType::U16 => Cell::UInt(a.as_primitive::<UInt16Type>().value(row) as u64),
            ColType::U32 => Cell::UInt(a.as_primitive::<UInt32Type>().value(row) as u64),
            ColType::U64 => Cell::UInt(a.as_primitive::<UInt64Type>().value(row)),
            ColType::F32 => Cell::Float(a.as_primitive::<Float32Type>().value(row) as f64),
            ColType::F64 => Cell::Float(a.as_primitive::<Float64Type>().value(row)),
            ColType::Dec(s) => Cell::Decimal(a.as_primitive::<Decimal128Type>().value(row), s),
            ColType::Utf8 => Cell::Str(a.as_string::<i32>().value(row)),
            ColType::LargeUtf8 => Cell::Str(a.as_string::<i64>().value(row)),
            ColType::Binary => Cell::Bytes(a.as_binary::<i32>().value(row)),
            ColType::LargeBinary => Cell::Bytes(a.as_binary::<i64>().value(row)),
            ColType::Date32 => Cell::Date(a.as_primitive::<Date32Type>().value(row)),
            ColType::TimeMicros => Cell::Time(a.as_primitive::<Time64MicrosecondType>().value(row)),
            ColType::TsMicros => Cell::Timestamp(a.as_primitive::<TimestampMicrosecondType>().value(row)),
            ColType::Other => Cell::Other(self.formatter.as_ref().map(|f| f.value(row).to_string()).unwrap_or_default()),
        }
    }
}

/// Exact decimal text of an unscaled value at `scale` (`12345`, 2 -> `123.45`).
pub fn decimal_text(v: i128, scale: i8) -> String {
    if scale <= 0 {
        let mut s = v.to_string();
        for _ in 0..(-scale) {
            s.push('0');
        }
        return s;
    }
    let neg = v < 0;
    let digits = v.unsigned_abs().to_string();
    let scale = scale as usize;
    let padded = if digits.len() <= scale { format!("{}{}", "0".repeat(scale - digits.len() + 1), digits) } else { digits };
    let (int, frac) = padded.split_at(padded.len() - scale);
    format!("{}{int}.{frac}", if neg { "-" } else { "" })
}

/// `YYYY-MM-DD` of days since the epoch.
pub fn date_text(days: i32) -> String {
    chrono::NaiveDate::from_num_days_from_ce_opt(days + 719_163).map(|d| d.format("%Y-%m-%d").to_string()).unwrap_or_else(|| days.to_string())
}

/// `HH:MM:SS[.ffffff]` of microseconds since midnight (MySQL TIME may exceed 24 h or be
/// negative).
pub fn time_text(micros: i64) -> String {
    let neg = micros < 0;
    let m = micros.unsigned_abs();
    let (secs, frac) = (m / 1_000_000, m % 1_000_000);
    let base = format!("{}{:02}:{:02}:{:02}", if neg { "-" } else { "" }, secs / 3600, (secs / 60) % 60, secs % 60);
    if frac == 0 {
        base
    } else {
        format!("{base}.{frac:06}")
    }
}

/// Shortest round-trip text of a float, the form every writer sends: Rust's `Display`, or its
/// exponent form (`1e300`, `1.5e-7`) for very large or very small magnitudes, which `Display`
/// would spell out with hundreds of zeros. Both parse back to the same `f64` in MySQL.
pub fn float_text(f: f64) -> String {
    let a = f.abs();
    if a != 0.0 && a.is_finite() && !(1e-5..1e16).contains(&a) {
        format!("{f:e}")
    } else {
        format!("{f}")
    }
}

/// The text of `f` rounded to single precision: the shortest round-trip text of the nearest
/// `f32`, widened the way the MySQL driver widens a FLOAT it reads back (`source::mysql`
/// `widen_f32`), then [`float_text`]. `0.10000000149012` (a 4.0 FLOAT read as `c + 0e0`)
/// becomes `0.1`, which parses back to the same `f32`.
pub fn float32_text(f: f64) -> String {
    let g = f as f32;
    if !g.is_finite() {
        return float_text(f);
    }
    float_text(format!("{g}").parse::<f64>().unwrap_or(g as f64))
}

/// The canonical **written** text of a cell (5.6 checksum; also what the text writers send for
/// non-temporal values): `None` for NULL, binaries as lower-case hex.
pub fn canonical(cell: &Cell<'_>) -> Option<Cow<'static, [u8]>> {
    Some(match cell {
        Cell::Null => return None,
        Cell::Str(s) => Cow::Owned(s.as_bytes().to_vec()),
        Cell::Bytes(b) => Cow::Owned(crate::copy::hash::hex(b).into_bytes()),
        Cell::Int(i) => Cow::Owned(i.to_string().into_bytes()),
        Cell::UInt(u) => Cow::Owned(u.to_string().into_bytes()),
        Cell::Float(f) => Cow::Owned(float_text(*f).into_bytes()),
        Cell::Bool(b) => Cow::Borrowed(if *b { b"1" } else { b"0" }),
        Cell::Decimal(v, s) => Cow::Owned(decimal_text(*v, *s).into_bytes()),
        Cell::Date(d) => Cow::Owned(date_text(*d).into_bytes()),
        Cell::Time(t) => Cow::Owned(time_text(*t).into_bytes()),
        Cell::Timestamp(t) => Cow::Owned(crate::copy::data::values::format_micros(*t).unwrap_or_default().into_bytes()),
        Cell::Other(s) => Cow::Owned(s.clone().into_bytes()),
    })
}

/// Hash every row of `batch` (all columns, canonical form) into `sum`.
pub fn checksum_batch(batch: &arrow_array::RecordBatch, sum: &mut crate::copy::hash::TableChecksum) -> Result<()> {
    let cols: Vec<Col<'_>> = batch.columns().iter().map(Col::new).collect::<Result<_>>()?;
    for row in 0..batch.num_rows() {
        let mut h = RowHasher::default();
        for c in &cols {
            let cell = c.cell(row);
            h.cell(canonical(&cell).as_deref());
        }
        sum.add_row(h.finish());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use arrow_array::{BinaryArray, Decimal128Array, Int64Array, StringArray};

    #[test]
    fn decimals_dates_times() {
        assert_eq!(decimal_text(12345, 2), "123.45");
        assert_eq!(decimal_text(-5, 2), "-0.05");
        assert_eq!(decimal_text(0, 3), "0.000");
        assert_eq!(decimal_text(7, 0), "7");
        assert_eq!(decimal_text(7, -2), "700");
        assert_eq!(date_text(0), "1970-01-01");
        assert_eq!(date_text(12_478), "2004-03-01");
        assert_eq!(time_text(3_723_000_000), "01:02:03");
        assert_eq!(time_text(-3_600_000_001), "-01:00:00.000001");
        assert_eq!(float_text(0.1), "0.1");
        assert_eq!(float_text(1e300), "1e300");
        assert_eq!(float_text(1.5e-7), "1.5e-7");
        assert_eq!(float_text(-2.5), "-2.5");
        assert_eq!(float32_text(0.10000000149012), "0.1");
        assert_eq!(float32_text(0.1), "0.1");
        assert_eq!(float32_text(16777217.0), "16777216");
        assert_eq!(float32_text(1.0000000116861e-7), "1e-7");
    }

    #[test]
    fn cells_by_type() {
        let ints: ArrayRef = Arc::new(Int64Array::from(vec![Some(1), None]));
        let strs: ArrayRef = Arc::new(StringArray::from(vec![Some("가"), Some("")]));
        let bins: ArrayRef = Arc::new(BinaryArray::from(vec![Some(&[0xde, 0xad][..]), None]));
        let decs: ArrayRef = Arc::new(Decimal128Array::from(vec![Some(12345), None]).with_precision_and_scale(9, 2).unwrap());
        let c = Col::new(&ints).unwrap();
        assert_eq!(c.cell(0), Cell::Int(1));
        assert_eq!(c.cell(1), Cell::Null);
        assert!(Col::new(&strs).unwrap().is_text());
        assert_eq!(Col::new(&strs).unwrap().cell(1), Cell::Str(""));
        let b = Col::new(&bins).unwrap();
        assert!(b.is_binary());
        assert_eq!(canonical(&b.cell(0)).unwrap().as_ref(), b"dead");
        assert_eq!(canonical(&Col::new(&decs).unwrap().cell(0)).unwrap().as_ref(), b"123.45");
    }
}
