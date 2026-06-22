//! `JsonCodec` — a JSON document ↔ rows (RFD §4). A top-level **array** decodes to one
//! row per element; a top-level **object** decodes to a single row; any other top-level
//! value decodes to one row with a single `value` column. Encoding emits a pretty,
//! key-stable JSON **array** of objects (deterministic, RFD §6).

use crate::{CfsError, Codec, RowBatch};

use crate::convert::{row_to_json, rows_to_batch};

/// The `json` codec.
#[derive(Debug, Default, Clone, Copy)]
pub struct JsonCodec;

impl Codec for JsonCodec {
    fn fmt(&self) -> &str {
        "json"
    }

    fn decode(&self, bytes: &[u8]) -> Result<RowBatch, CfsError> {
        let root: serde_json::Value =
            serde_json::from_slice(bytes).map_err(|e| CfsError::Decode {
                fmt: "json",
                detail: format!("line {}, column {}: {e}", e.line(), e.column()),
            })?;
        let nodes = match root {
            serde_json::Value::Array(items) => items,
            other => vec![other],
        };
        Ok(rows_to_batch(&nodes))
    }

    fn encode(&self, batch: &RowBatch) -> Result<Vec<u8>, CfsError> {
        let array: Vec<serde_json::Value> = batch
            .rows
            .iter()
            .map(|row| row_to_json(row, &batch.schema))
            .collect();
        serde_json::to_vec_pretty(&serde_json::Value::Array(array))
            .map(|mut v| {
                v.push(b'\n');
                v
            })
            .map_err(|e| CfsError::Encode {
                fmt: "json",
                detail: e.to_string(),
            })
    }
}
