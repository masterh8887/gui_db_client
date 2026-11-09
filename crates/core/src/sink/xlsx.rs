//! XLSX (Excel 2007+) sink. Requires Arrow batches; returns an error for PostgreSQL raw-CSV
//! output (use CSV or Parquet for PostgreSQL sources).
//!
//! Memory model: the `constant_memory` feature of `rust_xlsxwriter` is enabled, which writes
//! each cell to a temp file as it arrives rather than buffering the whole worksheet in a
//! `BTreeMap`. RSS stays proportional to one row rather than to the total row count.
//!
//! Row limit: Excel allows at most 1 048 576 rows per sheet. [`crate::sink::split::build_stack`]
//! applies an implicit 1 000 000-row split when no split is configured, so each file stays well
//! under that limit. The sink itself enforces the hard ceiling and returns an error if it is hit.

use std::path::PathBuf;

use arrow::util::display::{ArrayFormatter, FormatOptions};
use arrow_array::{Array, BooleanArray, Decimal128Array, Float32Array, Float64Array, Int16Array, Int32Array, Int64Array, Int8Array, RecordBatch, StringArray, UInt16Array, UInt32Array, UInt64Array, UInt8Array};
use arrow_schema::DataType;
use rust_xlsxwriter::{Format, Workbook, Worksheet, XlsxError};

use crate::error::{CoreError, Result};
use crate::sink::{Sink, SinkReport};
use crate::source::Batch;
use crate::types::XlsxOpts;

/// Excel hard limit: rows 1..=1 048 576 (1-based), or indices 0..=1 048 575 (0-based).
const EXCEL_MAX_ROWS: u32 = 1_048_576;

fn xlsx_err(ctx: &str, e: XlsxError) -> CoreError {
    CoreError::Io(format!("xlsx {ctx}: {e}"))
}

pub struct XlsxSink {
    workbook: Box<Workbook>,
    path: PathBuf,
    opts: XlsxOpts,
    /// Next row to write (0-based).
    row: u32,
    headers_written: bool,
    data_rows: u64,
    has_data: bool,
}

impl XlsxSink {
    pub fn new(path: PathBuf, opts: XlsxOpts) -> Result<Self> {
        let mut workbook = Box::new(Workbook::new());
        {
            let ws = workbook.add_worksheet_with_constant_memory();
            if let Some(name) = &opts.sheet_name {
                ws.set_name(name).map_err(|e| xlsx_err("sheet name", e))?;
            }
        }
        Ok(XlsxSink { workbook, path, opts, row: 0, headers_written: false, data_rows: 0, has_data: false })
    }

    fn ws(&mut self) -> Result<&mut Worksheet> {
        self.workbook.worksheet_from_index(0).map_err(|e| xlsx_err("get worksheet", e))
    }

    fn write_header(&mut self, schema: &arrow_schema::Schema) -> Result<()> {
        if self.opts.header {
            let bold = Format::new().set_bold();
            let row = self.row;
            for (col, field) in schema.fields().iter().enumerate() {
                self.ws()?
                    .write_string_with_format(row, col as u16, field.name(), &bold)
                    .map_err(|e| xlsx_err(&format!("header col {col}"), e))?;
            }
            self.row += 1;
        }
        self.headers_written = true;
        Ok(())
    }

    fn write_batch(&mut self, rb: &RecordBatch) -> Result<()> {
        if !self.headers_written {
            self.write_header(rb.schema_ref())?;
        }
        if rb.num_rows() == 0 {
            return Ok(());
        }
        let fmt_opts = FormatOptions::new()
            .with_null("")
            .with_date_format(Some(crate::sink::csv::DATE_FORMAT))
            .with_timestamp_format(Some(crate::sink::csv::TIMESTAMP_FORMAT))
            .with_timestamp_tz_format(Some(crate::sink::csv::TIMESTAMP_TZ_FORMAT))
            .with_time_format(Some(crate::sink::csv::TIME_FORMAT))
            .with_display_error(false);

        for row_idx in 0..rb.num_rows() {
            if self.row >= EXCEL_MAX_ROWS {
                return Err(CoreError::Invalid(format!(
                    "Excel row limit ({EXCEL_MAX_ROWS}) reached. Add a row split to spread the output across multiple files."
                )));
            }
            let excel_row = self.row;
            for (col_idx, column) in rb.columns().iter().enumerate() {
                if column.is_null(row_idx) {
                    continue;
                }
                let ws = self.ws()?;
                write_cell(ws, excel_row, col_idx as u16, column.as_ref(), row_idx, &fmt_opts)
                    .map_err(|e| xlsx_err(&format!("row {excel_row} col {col_idx}"), e))?;
            }
            self.row += 1;
            self.data_rows += 1;
        }
        self.has_data = true;
        Ok(())
    }
}

fn write_cell(
    ws: &mut Worksheet,
    row: u32,
    col: u16,
    array: &dyn Array,
    idx: usize,
    fmt_opts: &FormatOptions,
) -> std::result::Result<(), XlsxError> {
    macro_rules! write_int {
        ($A:ty) => {{
            ws.write_number(row, col, array.as_any().downcast_ref::<$A>().unwrap().value(idx) as f64)?;
        }};
    }
    match array.data_type() {
        DataType::Int8 => write_int!(Int8Array),
        DataType::Int16 => write_int!(Int16Array),
        DataType::Int32 => write_int!(Int32Array),
        DataType::Int64 => write_int!(Int64Array),
        DataType::UInt8 => write_int!(UInt8Array),
        DataType::UInt16 => write_int!(UInt16Array),
        DataType::UInt32 => write_int!(UInt32Array),
        DataType::UInt64 => write_int!(UInt64Array),
        DataType::Float32 => {
            ws.write_number(row, col, array.as_any().downcast_ref::<Float32Array>().unwrap().value(idx) as f64)?;
        }
        DataType::Float64 => {
            ws.write_number(row, col, array.as_any().downcast_ref::<Float64Array>().unwrap().value(idx))?;
        }
        DataType::Decimal128(_, scale) => {
            let v = array.as_any().downcast_ref::<Decimal128Array>().unwrap().value(idx);
            ws.write_number(row, col, v as f64 / 10f64.powi(*scale as i32))?;
        }
        DataType::Boolean => {
            ws.write_boolean(row, col, array.as_any().downcast_ref::<BooleanArray>().unwrap().value(idx))?;
        }
        DataType::Utf8 => {
            ws.write_string(row, col, array.as_any().downcast_ref::<StringArray>().unwrap().value(idx))?;
        }
        DataType::LargeUtf8 => {
            ws.write_string(row, col, array.as_any().downcast_ref::<arrow_array::LargeStringArray>().unwrap().value(idx))?;
        }
        DataType::Binary | DataType::LargeBinary | DataType::FixedSizeBinary(_) => {
            // binary columns: leave cell empty
        }
        _ => {
            // Dates, timestamps, times: format as ISO strings via ArrayFormatter
            if let Ok(fmt) = ArrayFormatter::try_new(array, fmt_opts) {
                let s = fmt.value(idx).to_string();
                if !s.is_empty() {
                    ws.write_string(row, col, &s)?;
                }
            }
        }
    }
    Ok(())
}

impl Sink for XlsxSink {
    fn write(&mut self, batch: Batch) -> Result<()> {
        match batch {
            Batch::Arrow(rb) => self.write_batch(&rb),
            Batch::RawCsv { .. } | Batch::RawCsvHeader(_) => Err(CoreError::Invalid(
                "XLSX format does not support PostgreSQL raw-CSV output. Use CSV or Parquet instead.".into(),
            )),
        }
    }

    fn finish(self: Box<Self>) -> Result<SinkReport> {
        let mut this = *self;
        // Ensure an empty export still produces a valid file (header-only or schema-only).
        if !this.headers_written {
            // Nothing was written yet; produce an empty workbook.
        }
        this.workbook.save(&this.path).map_err(|e| xlsx_err(&format!("save '{}'", this.path.display()), e))?;
        let bytes = std::fs::metadata(&this.path).map(|m| m.len()).unwrap_or(0);
        Ok(SinkReport { rows: this.data_rows, bytes, files: vec![this.path], unmappable_chars: 0 })
    }

    fn abort(self: Box<Self>) -> Vec<PathBuf> {
        let path = (*self).path;
        if path.exists() {
            vec![crate::sink::to_partial(&path)]
        } else {
            Vec::new()
        }
    }

    /// Before `finish()` the XLSX data is in a temp file; this returns a rough estimate.
    fn bytes_out(&self) -> u64 {
        self.data_rows.saturating_mul(80)
    }

    /// Returns 1 once we have started writing (a file will be produced at `finish()`).
    fn files_so_far(&self) -> u32 {
        if self.has_data || self.headers_written { 1 } else { 0 }
    }
}
