//! Codec execution for the read path (RFD-0001 §4): apply the `DECODE`/`ENCODE <fmt>` pipe
//! stages that the pushdown lowering deliberately drops.
//!
//! Codecs are schema-shaping, **local-only** transforms (never pushed to a driver), so they run
//! after the residual [`MiniEvaluator`](qfs_engine::MiniEvaluator) has produced the final batch.
//! The chain runs in pipeline order: `decode` turns a blob source's `content` bytes into rows
//! (via the registry codec's pure `bytes -> RowBatch`); `encode` collapses rows back into encoded
//! bytes (`RowBatch -> bytes`).
//!
//! ## Scope (T2)
//! Codec stages must be the **final** pipeline stages (a tail). A relational op over DECODEd data
//! needs the decoded schema, which is data-dependent and late-bound (the planner only knows the
//! blob source's `describe` schema) — so a codec followed by a relational op returns a clear
//! `codec_then_query` error instead of silently mis-ordering. Threading a backend's custom
//! [`CodecRegistry`] (beyond the six builtins) is a clean follow-up; today every codec is a builtin.

use qfs_core::{CodecRegistry, Column, ColumnType, Row, RowBatch, Schema, Value};
use qfs_parser::{PipeOp, Statement};

use crate::error::{ErrorKind, ExecError};

/// The well-known column a blob/single-file read carries its raw bytes under (matches the local
/// driver's content column and the write-side `CONTENT_COL`).
const CONTENT_COL: &str = "content";

/// One codec stage in pipeline order.
enum Stage<'a> {
    /// `DECODE <fmt>` — bytes (the `content` column) into rows.
    Decode(&'a str),
    /// `ENCODE <fmt>` — rows into encoded bytes.
    Encode(&'a str),
}

/// Apply the statement's `DECODE`/`ENCODE` chain to the executed `batch`, in pipeline order.
/// Returns `batch` unchanged when the pipeline carries no codec stages (the common case — zero
/// cost, no registry built).
///
/// # Errors
/// [`ExecError`] (usage class) if a codec is unknown, a `DECODE` has no blob to read, the bytes
/// are not valid for the format, or a relational op follows a codec (`codec_then_query`).
pub(crate) fn apply_codecs(batch: RowBatch, stmt: &Statement) -> Result<RowBatch, ExecError> {
    let chain = codec_chain(stmt)?;
    if chain.is_empty() {
        return Ok(batch);
    }
    // The six shipped codecs are builtins; resolve through a fresh builtin registry. A
    // backend-registered custom codec would thread the engine's CodecRegistry here instead.
    let registry = CodecRegistry::with_builtins();
    let mut current = batch;
    for stage in chain {
        current = apply_one(current, stage, &registry)?;
    }
    Ok(current)
}

/// Collect the ordered codec stages, rejecting a relational op positioned after any codec
/// (`codec_then_query`). Schema-neutral / effect-adjacent ops (`EXTEND`/`SET`/`AS`/`CALL`) are
/// positionally harmless and ignored.
fn codec_chain(stmt: &Statement) -> Result<Vec<Stage<'_>>, ExecError> {
    let Statement::Query(pipeline) = stmt else {
        return Ok(Vec::new());
    };
    let mut chain = Vec::new();
    let mut after_codec = false;
    for op in &pipeline.ops {
        match op {
            PipeOp::Decode(c) => {
                chain.push(Stage::Decode(&c.fmt));
                after_codec = true;
            }
            PipeOp::Encode(c) => {
                chain.push(Stage::Encode(&c.fmt));
                after_codec = true;
            }
            PipeOp::Extend(_) | PipeOp::Set(_) | PipeOp::As(_) | PipeOp::Call(_) => {}
            _ if after_codec => {
                return Err(ExecError::new(
                    ErrorKind::Usage,
                    "codec_then_query",
                    "querying DECODEd data is not yet supported — DECODE/ENCODE must be the \
                     final pipeline stages"
                        .to_string(),
                ));
            }
            _ => {}
        }
    }
    Ok(chain)
}

/// Run one codec stage over `batch` through the registry.
fn apply_one(
    batch: RowBatch,
    stage: Stage<'_>,
    reg: &CodecRegistry,
) -> Result<RowBatch, ExecError> {
    match stage {
        Stage::Decode(fmt) => {
            let codec = reg.resolve(fmt).map_err(|e| ExecError::from_qfs(&e))?;
            let bytes = blob_bytes(&batch)?;
            codec.decode(&bytes).map_err(|e| ExecError::from_qfs(&e))
        }
        Stage::Encode(fmt) => {
            let codec = reg.resolve(fmt).map_err(|e| ExecError::from_qfs(&e))?;
            let bytes = codec.encode(&batch).map_err(|e| ExecError::from_qfs(&e))?;
            Ok(encoded_batch(bytes))
        }
    }
}

/// Pull the single blob's bytes from a content-bearing batch (a single-file `/local` read). A
/// `DECODE` over a multi-row listing or a content-free batch is a usage error with a clear code.
fn blob_bytes(batch: &RowBatch) -> Result<Vec<u8>, ExecError> {
    let idx = batch
        .schema
        .columns
        .iter()
        .position(|c| c.name.as_str() == CONTENT_COL)
        .ok_or_else(|| {
            ExecError::new(
                ErrorKind::Usage,
                "decode_needs_blob",
                "DECODE expects a blob source with a `content` column (e.g. a single \
                 /local/<file>)"
                    .to_string(),
            )
        })?;
    let row = match batch.rows.as_slice() {
        [only] => only,
        rows => {
            return Err(ExecError::new(
                ErrorKind::Usage,
                "decode_needs_single_blob",
                format!(
                    "DECODE expects exactly one blob (a single file); got {} rows",
                    rows.len()
                ),
            ));
        }
    };
    match row.values.get(idx) {
        Some(Value::Bytes(b)) => Ok(b.clone()),
        Some(Value::Text(s)) => Ok(s.clone().into_bytes()),
        _ => Err(ExecError::new(
            ErrorKind::Usage,
            "decode_needs_blob",
            "the `content` column is not bytes".to_string(),
        )),
    }
}

/// Wrap encoded bytes as a single-row, single-column (`content`) text batch — the form the
/// renderer prints. The six builtin formats are all UTF-8 text, so a lossy decode is safe.
fn encoded_batch(bytes: Vec<u8>) -> RowBatch {
    let text = String::from_utf8_lossy(&bytes).into_owned();
    let schema = Schema::new(vec![Column::new(CONTENT_COL, ColumnType::Text, false)]);
    RowBatch::new(schema, vec![Row::new(vec![Value::Text(text)])])
}

#[cfg(test)]
mod tests {
    use super::*;
    use qfs_parser::parse_statement;

    /// A single-file `/local` read batch: the listing columns plus a `content` Bytes column.
    fn blob_batch(bytes: &[u8]) -> RowBatch {
        let schema = Schema::new(vec![
            Column::new("name", ColumnType::Text, false),
            Column::new("content", ColumnType::Bytes, true),
        ]);
        RowBatch::new(
            schema,
            vec![Row::new(vec![
                Value::Text("config.json".to_string()),
                Value::Bytes(bytes.to_vec()),
            ])],
        )
    }

    #[test]
    fn decode_json_then_encode_yaml_transcodes() {
        let stmt = parse_statement("/local/config.json |> decode json |> encode yaml").unwrap();
        let out = apply_codecs(blob_batch(b"{\"k\":1}\n"), &stmt).unwrap();
        // One Text `content` row holding the YAML encoding of the decoded object.
        assert_eq!(out.schema.columns.len(), 1);
        assert_eq!(out.schema.columns[0].name.as_str(), "content");
        let Value::Text(yaml) = &out.rows[0].values[0] else {
            panic!("expected text yaml");
        };
        assert!(
            yaml.contains("k: 1"),
            "yaml encodes the decoded object: {yaml:?}"
        );
    }

    #[test]
    fn decode_alone_unpacks_rows() {
        let stmt = parse_statement("/local/config.json |> decode json").unwrap();
        let out = apply_codecs(blob_batch(b"{\"k\":1}\n"), &stmt).unwrap();
        // The decoded JSON object becomes a row with a `k` column.
        assert!(out.schema.columns.iter().any(|c| c.name.as_str() == "k"));
    }

    #[test]
    fn no_codec_stages_pass_through_unchanged() {
        let stmt = parse_statement("/local/config.json |> select name").unwrap();
        let batch = blob_batch(b"{\"k\":1}\n");
        let out = apply_codecs(batch.clone(), &stmt).unwrap();
        assert_eq!(out.schema.columns.len(), batch.schema.columns.len());
    }

    #[test]
    fn unknown_codec_is_a_usage_error() {
        let stmt = parse_statement("/local/config.json |> decode nope").unwrap();
        let err = apply_codecs(blob_batch(b"{}"), &stmt).unwrap_err();
        assert_eq!(err.kind, ErrorKind::Capability);
    }

    #[test]
    fn relational_op_after_codec_is_rejected() {
        let stmt = parse_statement("/local/config.json |> decode json |> where k == 1").unwrap();
        let err = apply_codecs(blob_batch(b"{\"k\":1}"), &stmt).unwrap_err();
        assert_eq!(err.code, "codec_then_query");
    }
}
