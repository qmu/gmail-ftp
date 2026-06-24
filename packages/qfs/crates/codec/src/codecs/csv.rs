//! `CsvCodec` — delimited text ↔ rows (RFD §4). The **header row** names the columns;
//! each subsequent record is a row. Cells are decoded as [`Value::Text`] unless they
//! parse cleanly as an integer, float, or boolean (a light, lossless type hint) — an
//! empty cell decodes to [`Value::Null`]. Encoding writes the header from the schema and
//! renders each cell back to text (deterministic column order, RFD §6).
//!
//! Nested struct/array cells have no flat CSV representation, so they are rendered as
//! their compact JSON text on encode (documented projection). The default delimiter is
//! `,`; [`CsvCodec::with_delimiter`] selects another (e.g. `\t`).

use qfs_types::{Column, ColumnType, Row, Schema, Value};

use crate::convert::value_to_json;
use crate::{CfsError, Codec, RowBatch};

/// The `csv` codec. Carries its delimiter so a TSV variant is just a delimiter swap.
#[derive(Debug, Clone, Copy)]
pub struct CsvCodec {
    delimiter: u8,
}

impl Default for CsvCodec {
    fn default() -> Self {
        Self { delimiter: b',' }
    }
}

impl CsvCodec {
    /// A CSV codec with a custom field delimiter (e.g. `b'\t'` for TSV).
    #[must_use]
    pub fn with_delimiter(delimiter: u8) -> Self {
        Self { delimiter }
    }
}

impl Codec for CsvCodec {
    fn fmt(&self) -> &str {
        "csv"
    }

    fn decode(&self, bytes: &[u8]) -> Result<RowBatch, CfsError> {
        let mut reader = csv::ReaderBuilder::new()
            .delimiter(self.delimiter)
            .has_headers(true)
            .flexible(true)
            .from_reader(bytes);

        let headers: Vec<String> = reader
            .headers()
            .map_err(|e| CfsError::Decode {
                fmt: "csv",
                detail: e.to_string(),
            })?
            .iter()
            .map(ToString::to_string)
            .collect();

        let mut rows = Vec::new();
        for (lineno, record) in reader.records().enumerate() {
            let record = record.map_err(|e| CfsError::Decode {
                fmt: "csv",
                detail: format!("record {}: {e}", lineno + 1),
            })?;
            let values = record.iter().map(infer_cell).collect();
            rows.push(Row::new(values));
        }

        let schema = infer_schema(&headers, &rows);
        Ok(RowBatch::new(schema, rows))
    }

    fn encode(&self, batch: &RowBatch) -> Result<Vec<u8>, CfsError> {
        let mut writer = csv::WriterBuilder::new()
            .delimiter(self.delimiter)
            .from_writer(Vec::new());

        writer
            .write_record(batch.schema.columns.iter().map(|c| c.name.as_str()))
            .map_err(|e| CfsError::Encode {
                fmt: "csv",
                detail: e.to_string(),
            })?;

        for row in &batch.rows {
            let cells: Vec<String> = row.values.iter().map(render_cell).collect();
            writer.write_record(&cells).map_err(|e| CfsError::Encode {
                fmt: "csv",
                detail: e.to_string(),
            })?;
        }

        writer.into_inner().map_err(|e| CfsError::Encode {
            fmt: "csv",
            detail: e.to_string(),
        })
    }
}

/// A light type hint per cell: empty → `Null`; `true`/`false` → `Bool`; clean integer →
/// `Int`; clean float → `Float`; otherwise `Text` (lossless default).
fn infer_cell(cell: &str) -> Value {
    if cell.is_empty() {
        return Value::Null;
    }
    match cell {
        "true" => return Value::Bool(true),
        "false" => return Value::Bool(false),
        _ => {}
    }
    if let Ok(i) = cell.parse::<i64>() {
        return Value::Int(i);
    }
    if let Ok(f) = cell.parse::<f64>() {
        if f.is_finite() {
            return Value::Float(f);
        }
    }
    Value::Text(cell.to_string())
}

/// Infer the schema: one column per header, typed by unioning the cell types in that
/// column (a column with mixed `Int`/`Text` widens to `Text`; an all-empty column is
/// nullable `Unknown`).
fn infer_schema(headers: &[String], rows: &[Row]) -> Schema {
    let columns = headers
        .iter()
        .enumerate()
        .map(|(i, name)| {
            let mut ty = ColumnType::Unknown;
            let mut nullable = false;
            for row in rows {
                match row.values.get(i) {
                    None | Some(Value::Null) => nullable = true,
                    Some(v) => ty = widen_cell(&ty, &v.type_of()),
                }
            }
            Column::new(name.clone(), ty, nullable)
        })
        .collect();
    Schema::new(columns)
}

/// Column-type widening for CSV cells: `Unknown` is bottom; equal types stay; any
/// mismatch (including `Int` vs `Float`) widens to `Text`, the lossless CSV carrier.
fn widen_cell(acc: &ColumnType, cell: &ColumnType) -> ColumnType {
    match (acc, cell) {
        (ColumnType::Unknown, other) | (other, ColumnType::Unknown) => other.clone(),
        (a, b) if a == b => a.clone(),
        _ => ColumnType::Text,
    }
}

/// Render a cell back to text for encoding. Scalars use their natural lexical form;
/// `Null` → empty; nested struct/array/json → compact JSON (documented projection).
fn render_cell(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::Bool(b) => b.to_string(),
        Value::Int(i) | Value::Timestamp(i) => i.to_string(),
        Value::Float(f) => f.to_string(),
        Value::Text(s) => s.clone(),
        Value::Bytes(_) | Value::Struct(_) | Value::Array(_) | Value::Json(_) => {
            serde_json::to_string(&value_to_json(value)).unwrap_or_default()
        }
        // `Value` is `#[non_exhaustive]`: a future variant renders as its JSON text.
        _ => serde_json::to_string(&value_to_json(value)).unwrap_or_default(),
    }
}
