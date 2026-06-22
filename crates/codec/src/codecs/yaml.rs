//! `YamlCodec` — a YAML document ↔ rows (RFD §4). YAML is deserialized straight into a
//! `serde_json::Value` tree (so the one [`crate::convert`] bridge handles struct/array
//! nesting), then mapped to rows exactly like JSON: a top-level sequence → one row per
//! element, a top-level mapping → a single row. Encoding emits a key-stable YAML
//! sequence of mappings (deterministic, RFD §6).
//!
//! Documented non-preservation (RFD §4): YAML comments and anchors are **not** preserved
//! across a decode→encode round-trip — only the *data* round-trips (semantic fidelity).

use crate::{CfsError, Codec, RowBatch};

use crate::convert::{row_to_json, rows_to_batch};

/// The `yaml` codec.
#[derive(Debug, Default, Clone, Copy)]
pub struct YamlCodec;

impl Codec for YamlCodec {
    fn fmt(&self) -> &str {
        "yaml"
    }

    fn decode(&self, bytes: &[u8]) -> Result<RowBatch, CfsError> {
        let root: serde_json::Value =
            serde_yaml::from_slice(bytes).map_err(|e| CfsError::Decode {
                fmt: "yaml",
                detail: e.location().map_or_else(
                    || e.to_string(),
                    |loc| format!("line {}, column {}: {e}", loc.line(), loc.column()),
                ),
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
        serde_yaml::to_string(&serde_json::Value::Array(array))
            .map(String::into_bytes)
            .map_err(|e| CfsError::Encode {
                fmt: "yaml",
                detail: e.to_string(),
            })
    }
}
