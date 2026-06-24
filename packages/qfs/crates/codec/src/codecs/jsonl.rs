//! `JsonlCodec` — newline-delimited JSON (NDJSON / JSON Lines, RFD §4). **One JSON
//! value per non-blank line** decodes to one row; encoding emits one compact,
//! key-stable JSON object per line (deterministic, RFD §6). This is the streaming
//! sibling of [`JsonCodec`](super::json::JsonCodec): each line is independent, so it
//! suits large/append-only logs.

use crate::{CfsError, Codec, RowBatch};

use crate::convert::{row_to_json, rows_to_batch};

/// The `jsonl` (NDJSON) codec.
#[derive(Debug, Default, Clone, Copy)]
pub struct JsonlCodec;

impl Codec for JsonlCodec {
    fn fmt(&self) -> &str {
        "jsonl"
    }

    fn decode(&self, bytes: &[u8]) -> Result<RowBatch, CfsError> {
        let text = std::str::from_utf8(bytes).map_err(|e| CfsError::Decode {
            fmt: "jsonl",
            detail: format!("invalid utf-8: {e}"),
        })?;
        let mut nodes = Vec::new();
        for (lineno, line) in text.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let node: serde_json::Value =
                serde_json::from_str(line).map_err(|e| CfsError::Decode {
                    fmt: "jsonl",
                    detail: format!("line {}: {e}", lineno + 1),
                })?;
            nodes.push(node);
        }
        Ok(rows_to_batch(&nodes))
    }

    fn encode(&self, batch: &RowBatch) -> Result<Vec<u8>, CfsError> {
        let mut out = Vec::new();
        for row in &batch.rows {
            let json = row_to_json(row, &batch.schema);
            let line = serde_json::to_vec(&json).map_err(|e| CfsError::Encode {
                fmt: "jsonl",
                detail: e.to_string(),
            })?;
            out.extend_from_slice(&line);
            out.push(b'\n');
        }
        Ok(out)
    }
}
