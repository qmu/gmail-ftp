//! Owned result DTOs the executor returns and the CLI renders: [`RowSet`] (read results) and
//! [`PlanPreview`] (effect dry-run). No vendor types cross this seam (RFD §9) — these are the
//! only shapes the renderer (`output.rs`) ever sees, alongside the owned
//! [`ExecError`](crate::ExecError).
//!
//! ## Stable JSON contract (golden-pinned, ticket t29)
//! A [`RowSet`] serializes to `{"rows":[ {col: value, …}, … ]}` — each row is a JSON object
//! keyed by column name (not a positional array), so an AI agent reads `row.subject` directly.
//! A [`PlanPreview`] serializes to the engine's owned [`Preview`](cfs_core::Preview) plus the
//! CLI-level `committed` flag. Any drift fails the golden tests.

use cfs_core::{Name, Preview, Row, RowBatch, Schema, Value};
use serde::Serialize;

/// An owned set of result rows + their schema — the read-path output the CLI renders. Built
/// from the engine's final [`RowBatch`]; carries the column order so both the JSON object keys
/// and the table columns are deterministic.
#[derive(Debug, Clone, PartialEq)]
pub struct RowSet {
    /// The result schema (column order + names).
    pub schema: Schema,
    /// The result rows, positional to `schema.columns`.
    pub rows: Vec<Row>,
}

impl RowSet {
    /// Build a row set from the engine's final batch.
    #[must_use]
    pub fn from_batch(batch: RowBatch) -> Self {
        Self {
            schema: batch.schema,
            rows: batch.rows,
        }
    }

    /// The column names, in order.
    #[must_use]
    pub fn columns(&self) -> Vec<&str> {
        self.schema
            .columns
            .iter()
            .map(|c| c.name.as_str())
            .collect()
    }

    /// The number of result rows.
    #[must_use]
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    /// Whether the row set is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }
}

/// The serialization shape of one row: an ordered map of `column -> value`. Uses an owned
/// `Vec<(Name, &Value)>` serialized as a JSON object so insertion order (= schema order) is
/// preserved deterministically.
struct RowObject<'a> {
    columns: &'a [Name],
    values: &'a [Value],
}

impl Serialize for RowObject<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let n = self.columns.len().min(self.values.len());
        let mut map = serializer.serialize_map(Some(n))?;
        for i in 0..n {
            map.serialize_entry(&self.columns[i], &ValueJson(&self.values[i]))?;
        }
        map.end()
    }
}

/// Serialize a [`Value`] as its **natural** JSON (not serde's externally-tagged enum form): an
/// `Int` becomes a JSON number, `Text` a string, `Null` JSON null, `Bytes` a base-free byte
/// array, nested `Struct`/`Array`/`Json` recursively. This is the stable agent-facing row shape
/// (`row.subject` is a string, not `{"Text":"…"}`).
struct ValueJson<'a>(&'a Value);

impl Serialize for ValueJson<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::{SerializeMap, SerializeSeq};
        match self.0 {
            Value::Null => serializer.serialize_none(),
            Value::Bool(b) => serializer.serialize_bool(*b),
            Value::Int(i) | Value::Timestamp(i) => serializer.serialize_i64(*i),
            Value::Float(f) => serializer.serialize_f64(*f),
            Value::Text(s) => serializer.serialize_str(s),
            Value::Bytes(b) => {
                let mut seq = serializer.serialize_seq(Some(b.len()))?;
                for byte in b {
                    seq.serialize_element(byte)?;
                }
                seq.end()
            }
            Value::Array(items) => {
                let mut seq = serializer.serialize_seq(Some(items.len()))?;
                for item in items {
                    seq.serialize_element(&ValueJson(item))?;
                }
                seq.end()
            }
            Value::Struct(fields) => {
                let entries: Vec<(&String, &Value)> = fields.iter().collect();
                let mut map = serializer.serialize_map(Some(entries.len()))?;
                for (k, v) in entries {
                    map.serialize_entry(k, &ValueJson(v))?;
                }
                map.end()
            }
            Value::Json(j) => j.serialize(serializer),
            // Value is #[non_exhaustive]: an unmodeled variant falls back to serde's form.
            other => other.serialize(serializer),
        }
    }
}

impl Serialize for RowSet {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let columns: Vec<Name> = self.schema.columns.iter().map(|c| c.name.clone()).collect();
        let rows: Vec<RowObject> = self
            .rows
            .iter()
            .map(|r| RowObject {
                columns: &columns,
                values: &r.values,
            })
            .collect();
        let mut s = serializer.serialize_struct("RowSet", 1)?;
        s.serialize_field("rows", &rows)?;
        s.end()
    }
}

/// The owned dry-run plan summary the CLI renders for an effect statement: the engine's
/// secret-free [`Preview`] plus whether this was a committed apply (vs a PREVIEW).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PlanPreview {
    /// The engine preview (effects, per-target affected estimates, irreversible ids).
    pub preview: Preview,
    /// `false` for a PREVIEW (default), `true` when rendered as the result of a `--commit`.
    pub committed: bool,
}

impl PlanPreview {
    /// A preview (not committed).
    #[must_use]
    pub fn preview(preview: Preview) -> Self {
        Self {
            preview,
            committed: false,
        }
    }

    /// A committed-apply summary.
    #[must_use]
    pub fn committed(preview: Preview) -> Self {
        Self {
            preview,
            committed: true,
        }
    }
}
