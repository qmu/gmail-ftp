//! `TomlCodec` — a TOML document ↔ rows (RFD §4). A TOML document is a single top-level
//! table, so it decodes to a **single row** whose columns are the top-level keys (nested
//! tables → [`Value::Struct`](qfs_types::Value::Struct), arrays → arrays). Encoding emits
//! a key-stable TOML table from the **first** row (a one-table format), or an empty
//! document for an empty batch.
//!
//! Documented non-preservation (RFD §4): TOML comments and key ordering/section layout
//! are **not** preserved across a round-trip — only the data is (semantic fidelity).

use crate::{CfsError, Codec, RowBatch};

use crate::convert::{row_to_json, rows_to_batch};

/// The `toml` codec.
#[derive(Debug, Default, Clone, Copy)]
pub struct TomlCodec;

impl Codec for TomlCodec {
    fn fmt(&self) -> &str {
        "toml"
    }

    fn decode(&self, bytes: &[u8]) -> Result<RowBatch, CfsError> {
        let text = std::str::from_utf8(bytes).map_err(|e| CfsError::Decode {
            fmt: "toml",
            detail: format!("invalid utf-8: {e}"),
        })?;
        let root: serde_json::Value = toml::from_str(text).map_err(|e| CfsError::Decode {
            fmt: "toml",
            detail: e.to_string(),
        })?;
        // A TOML document is one table → one row.
        Ok(rows_to_batch(std::slice::from_ref(&root)))
    }

    fn encode(&self, batch: &RowBatch) -> Result<Vec<u8>, CfsError> {
        let Some(row) = batch.rows.first() else {
            return Ok(Vec::new());
        };
        let json = row_to_json(row, &batch.schema);
        toml::to_string_pretty(&json)
            .map(String::into_bytes)
            .map_err(|e| CfsError::Encode {
                fmt: "toml",
                detail: e.to_string(),
            })
    }
}
