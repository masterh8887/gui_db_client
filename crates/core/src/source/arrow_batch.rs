//! Shared helper for drivers that decode rows themselves (MySQL, MSSQL, SQLite, ODBC, Oracle
//! and PostgreSQL binary COPY): accumulate typed cells into Arrow builders and cut a
//! `RecordBatch` at 64K rows or 8 MB, whichever comes first.
//!
//! Column types are limited to a small, well supported set so that every driver produces the
//! same CSV/Parquet for the same logical values.

use std::sync::Arc;

use arrow_array::builder::{
    BinaryBuilder, BooleanBuilder, Date32Builder, Decimal128Builder, Float64Builder, Int64Builder, StringBuilder, Time64MicrosecondBuilder, TimestampMicrosecondBuilder,
};
use arrow_array::{ArrayRef, RecordBatch};
use arrow_schema::{DataType, Field, Schema, SchemaRef, TimeUnit};

use crate::error::{CoreError, Result};
use crate::{BATCH_BYTES, BATCH_ROWS};

/// Logical column kinds BulkDB emits. Drivers map their native types onto these.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColKind {
    Bool,
    Int64,
    Float64,
    /// Decimal with precision/scale; values are pushed as i128 scaled by `10^scale`.
    Decimal {
        precision: u8,
        scale: i8,
    },
    Utf8,
    Binary,
    /// Days since 1970-01-01.
    Date,
    /// Microseconds since midnight.
    Time,
    /// Microseconds since epoch, naive (no zone).
    Timestamp,
    /// Microseconds since epoch, UTC.
    TimestampTz,
}

impl ColKind {
    pub fn data_type(self) -> DataType {
        match self {
            ColKind::Bool => DataType::Boolean,
            ColKind::Int64 => DataType::Int64,
            ColKind::Float64 => DataType::Float64,
            ColKind::Decimal { precision, scale } => DataType::Decimal128(precision, scale),
            ColKind::Utf8 => DataType::Utf8,
            ColKind::Binary => DataType::Binary,
            ColKind::Date => DataType::Date32,
            ColKind::Time => DataType::Time64(TimeUnit::Microsecond),
            ColKind::Timestamp => DataType::Timestamp(TimeUnit::Microsecond, None),
            ColKind::TimestampTz => DataType::Timestamp(TimeUnit::Microsecond, Some(Arc::from("UTC"))),
        }
    }
}

/// A value pushed into a column. Borrowed where possible so drivers do not allocate.
#[derive(Debug, Clone, Copy)]
pub enum Cell<'a> {
    Null,
    Bool(bool),
    I64(i64),
    F64(f64),
    /// Already scaled integer for Decimal columns.
    Decimal(i128),
    Str(&'a str),
    Bytes(&'a [u8]),
    /// Days since epoch.
    Date(i32),
    /// Microseconds since midnight.
    Time(i64),
    /// Microseconds since epoch.
    Timestamp(i64),
}

enum ColBuilder {
    Bool(BooleanBuilder),
    I64(Int64Builder),
    F64(Float64Builder),
    Dec(Decimal128Builder),
    Str(StringBuilder),
    Bin(BinaryBuilder),
    Date(Date32Builder),
    Time(Time64MicrosecondBuilder),
    Ts(TimestampMicrosecondBuilder),
}

impl ColBuilder {
    fn new(kind: ColKind, capacity: usize) -> ColBuilder {
        match kind {
            ColKind::Bool => ColBuilder::Bool(BooleanBuilder::with_capacity(capacity)),
            ColKind::Int64 => ColBuilder::I64(Int64Builder::with_capacity(capacity)),
            ColKind::Float64 => ColBuilder::F64(Float64Builder::with_capacity(capacity)),
            ColKind::Decimal { precision, scale } => ColBuilder::Dec(Decimal128Builder::with_capacity(capacity).with_data_type(DataType::Decimal128(precision, scale))),
            ColKind::Utf8 => ColBuilder::Str(StringBuilder::with_capacity(capacity, capacity * 16)),
            ColKind::Binary => ColBuilder::Bin(BinaryBuilder::with_capacity(capacity, capacity * 16)),
            ColKind::Date => ColBuilder::Date(Date32Builder::with_capacity(capacity)),
            ColKind::Time => ColBuilder::Time(Time64MicrosecondBuilder::with_capacity(capacity)),
            ColKind::Timestamp => ColBuilder::Ts(TimestampMicrosecondBuilder::with_capacity(capacity)),
            ColKind::TimestampTz => ColBuilder::Ts(TimestampMicrosecondBuilder::with_capacity(capacity).with_timezone("UTC")),
        }
    }

    fn append_null(&mut self) {
        match self {
            ColBuilder::Bool(b) => b.append_null(),
            ColBuilder::I64(b) => b.append_null(),
            ColBuilder::F64(b) => b.append_null(),
            ColBuilder::Dec(b) => b.append_null(),
            ColBuilder::Str(b) => b.append_null(),
            ColBuilder::Bin(b) => b.append_null(),
            ColBuilder::Date(b) => b.append_null(),
            ColBuilder::Time(b) => b.append_null(),
            ColBuilder::Ts(b) => b.append_null(),
        }
    }

    /// Returns approximate bytes added.
    fn append(&mut self, cell: Cell<'_>, col: usize) -> Result<usize> {
        let mismatch = |what: &str| CoreError::Internal(format!("column {col}: cannot store {what} in this column type"));
        match (self, cell) {
            (b, Cell::Null) => {
                b.append_null();
                Ok(1)
            }
            (ColBuilder::Bool(b), Cell::Bool(v)) => {
                b.append_value(v);
                Ok(1)
            }
            (ColBuilder::Bool(b), Cell::I64(v)) => {
                b.append_value(v != 0);
                Ok(1)
            }
            (ColBuilder::I64(b), Cell::I64(v)) => {
                b.append_value(v);
                Ok(8)
            }
            (ColBuilder::I64(b), Cell::Bool(v)) => {
                b.append_value(v as i64);
                Ok(8)
            }
            (ColBuilder::I64(b), Cell::Str(s)) => match s.trim().parse::<i64>() {
                Ok(v) => {
                    b.append_value(v);
                    Ok(8)
                }
                Err(_) => Err(CoreError::Internal(format!("column {col}: '{s}' is not an integer"))),
            },
            (ColBuilder::F64(b), Cell::F64(v)) => {
                b.append_value(v);
                Ok(8)
            }
            (ColBuilder::F64(b), Cell::I64(v)) => {
                b.append_value(v as f64);
                Ok(8)
            }
            (ColBuilder::F64(b), Cell::Str(s)) => match s.trim().parse::<f64>() {
                Ok(v) => {
                    b.append_value(v);
                    Ok(8)
                }
                Err(_) => Err(CoreError::Internal(format!("column {col}: '{s}' is not a number"))),
            },
            (ColBuilder::Dec(b), Cell::Decimal(v)) => {
                b.append_value(v);
                Ok(16)
            }
            (ColBuilder::Dec(b), Cell::I64(v)) => {
                b.append_value(v as i128);
                Ok(16)
            }
            (ColBuilder::Str(b), Cell::Str(s)) => {
                b.append_value(s);
                Ok(s.len() + 4)
            }
            (ColBuilder::Str(b), Cell::Bytes(v)) => {
                b.append_value(String::from_utf8_lossy(v));
                Ok(v.len() + 4)
            }
            (ColBuilder::Str(b), Cell::I64(v)) => {
                b.append_value(v.to_string());
                Ok(12)
            }
            (ColBuilder::Str(b), Cell::F64(v)) => {
                b.append_value(v.to_string());
                Ok(12)
            }
            (ColBuilder::Str(b), Cell::Bool(v)) => {
                b.append_value(if v { "true" } else { "false" });
                Ok(5)
            }
            (ColBuilder::Bin(b), Cell::Bytes(v)) => {
                b.append_value(v);
                Ok(v.len() + 4)
            }
            (ColBuilder::Bin(b), Cell::Str(s)) => {
                b.append_value(s.as_bytes());
                Ok(s.len() + 4)
            }
            (ColBuilder::Date(b), Cell::Date(v)) => {
                b.append_value(v);
                Ok(4)
            }
            (ColBuilder::Time(b), Cell::Time(v)) => {
                b.append_value(v);
                Ok(8)
            }
            (ColBuilder::Ts(b), Cell::Timestamp(v)) => {
                b.append_value(v);
                Ok(8)
            }
            (_, Cell::Bool(_)) => Err(mismatch("bool")),
            (_, Cell::I64(_)) => Err(mismatch("int")),
            (_, Cell::F64(_)) => Err(mismatch("float")),
            (_, Cell::Decimal(_)) => Err(mismatch("decimal")),
            (_, Cell::Str(_)) => Err(mismatch("text")),
            (_, Cell::Bytes(_)) => Err(mismatch("bytes")),
            (_, Cell::Date(_)) => Err(mismatch("date")),
            (_, Cell::Time(_)) => Err(mismatch("time")),
            (_, Cell::Timestamp(_)) => Err(mismatch("timestamp")),
        }
    }

    fn finish(&mut self) -> ArrayRef {
        match self {
            ColBuilder::Bool(b) => Arc::new(b.finish()),
            ColBuilder::I64(b) => Arc::new(b.finish()),
            ColBuilder::F64(b) => Arc::new(b.finish()),
            ColBuilder::Dec(b) => Arc::new(b.finish()),
            ColBuilder::Str(b) => Arc::new(b.finish()),
            ColBuilder::Bin(b) => Arc::new(b.finish()),
            ColBuilder::Date(b) => Arc::new(b.finish()),
            ColBuilder::Time(b) => Arc::new(b.finish()),
            ColBuilder::Ts(b) => Arc::new(b.finish()),
        }
    }
}

/// Build a schema from `(name, kind)` pairs. Duplicate names get `_2`, `_3` suffixes so that
/// Parquet stays valid.
pub fn schema_from(cols: &[(String, ColKind)]) -> SchemaRef {
    let mut seen = std::collections::HashMap::<String, usize>::new();
    let fields: Vec<Field> = cols
        .iter()
        .map(|(name, kind)| {
            let n = seen.entry(name.clone()).or_insert(0);
            *n += 1;
            let final_name = if *n == 1 { name.clone() } else { format!("{name}_{n}") };
            Field::new(final_name, kind.data_type(), true)
        })
        .collect();
    Arc::new(Schema::new(fields))
}

/// Accumulates rows and cuts record batches.
pub struct ArrowBatcher {
    schema: SchemaRef,
    kinds: Vec<ColKind>,
    builders: Vec<ColBuilder>,
    rows: usize,
    bytes: usize,
    col: usize,
    max_rows: usize,
    max_bytes: usize,
}

impl ArrowBatcher {
    pub fn new(schema: SchemaRef, kinds: Vec<ColKind>) -> Self {
        Self::with_limits(schema, kinds, BATCH_ROWS, BATCH_BYTES)
    }

    pub fn with_limits(schema: SchemaRef, kinds: Vec<ColKind>, max_rows: usize, max_bytes: usize) -> Self {
        assert_eq!(schema.fields().len(), kinds.len(), "schema/kind length mismatch");
        let cap = max_rows.min(8192);
        let builders = kinds.iter().map(|k| ColBuilder::new(*k, cap)).collect();
        ArrowBatcher { schema, kinds, builders, rows: 0, bytes: 0, col: 0, max_rows: max_rows.max(1), max_bytes: max_bytes.max(1) }
    }

    pub fn schema(&self) -> SchemaRef {
        self.schema.clone()
    }

    pub fn kinds(&self) -> &[ColKind] {
        &self.kinds
    }

    pub fn num_columns(&self) -> usize {
        self.kinds.len()
    }

    pub fn rows(&self) -> usize {
        self.rows
    }

    pub fn is_empty(&self) -> bool {
        self.rows == 0
    }

    /// Push the next cell of the current row (cells must be pushed in column order).
    pub fn push(&mut self, cell: Cell<'_>) -> Result<()> {
        let col = self.col;
        let b = self.builders.get_mut(col).ok_or_else(|| CoreError::Internal("too many cells in row".into()))?;
        self.bytes += b.append(cell, col)?;
        self.col += 1;
        Ok(())
    }

    /// Finish the current row. Returns `true` when the batch is full and should be flushed.
    pub fn end_row(&mut self) -> Result<bool> {
        if self.col != self.kinds.len() {
            return Err(CoreError::Internal(format!("row has {} cells, expected {}", self.col, self.kinds.len())));
        }
        self.col = 0;
        self.rows += 1;
        Ok(self.rows >= self.max_rows || self.bytes >= self.max_bytes)
    }

    /// Take the accumulated rows as a `RecordBatch`, or `None` when empty.
    pub fn flush(&mut self) -> Result<Option<RecordBatch>> {
        if self.rows == 0 {
            return Ok(None);
        }
        let arrays: Vec<ArrayRef> = self.builders.iter_mut().map(|b| b.finish()).collect();
        self.rows = 0;
        self.bytes = 0;
        self.col = 0;
        let batch = RecordBatch::try_new(self.schema.clone(), arrays)?;
        Ok(Some(batch))
    }
}

/// Convert a `chrono::NaiveDate` to Arrow days.
pub fn days_from_date(d: chrono::NaiveDate) -> i32 {
    let epoch = chrono::NaiveDate::from_ymd_opt(1970, 1, 1).unwrap();
    (d - epoch).num_days() as i32
}

/// Convert a naive timestamp to microseconds since epoch.
pub fn micros_from_naive(ts: chrono::NaiveDateTime) -> i64 {
    ts.and_utc().timestamp_micros()
}

/// Convert a time of day to microseconds since midnight.
pub fn micros_from_time(t: chrono::NaiveTime) -> i64 {
    use chrono::Timelike;
    t.num_seconds_from_midnight() as i64 * 1_000_000 + (t.nanosecond() / 1000) as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cuts_batches_by_rows() {
        let cols = vec![("id".to_string(), ColKind::Int64), ("name".to_string(), ColKind::Utf8)];
        let schema = schema_from(&cols);
        let mut b = ArrowBatcher::with_limits(schema, vec![ColKind::Int64, ColKind::Utf8], 2, usize::MAX);
        b.push(Cell::I64(1)).unwrap();
        b.push(Cell::Str("a")).unwrap();
        assert!(!b.end_row().unwrap());
        b.push(Cell::I64(2)).unwrap();
        b.push(Cell::Null).unwrap();
        assert!(b.end_row().unwrap());
        let batch = b.flush().unwrap().unwrap();
        assert_eq!(batch.num_rows(), 2);
        assert!(b.flush().unwrap().is_none());
    }

    #[test]
    fn dedups_column_names() {
        let cols = vec![("x".to_string(), ColKind::Int64), ("x".to_string(), ColKind::Int64)];
        let schema = schema_from(&cols);
        assert_eq!(schema.field(1).name(), "x_2");
    }

    #[test]
    fn rejects_type_mismatch() {
        let schema = schema_from(&[("d".to_string(), ColKind::Date)]);
        let mut b = ArrowBatcher::new(schema, vec![ColKind::Date]);
        assert!(b.push(Cell::Str("2020-01-01")).is_err());
    }
}
