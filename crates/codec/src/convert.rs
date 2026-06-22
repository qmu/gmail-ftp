//! The shared JSON-tree ↔ `cfs-types` bridge (t15). Every structured codec
//! (`json`, `jsonl`, `yaml`, `toml`, `md+frontmatter`) parses its bytes into a
//! `serde_json::Value` tree (`serde_yaml`/`toml` deserialize *into* `serde_json::Value`
//! too), then funnels through here to produce owned [`RowBatch`]es with an **inferred**
//! schema. Keeping one bridge means the struct/array/`Json`-fallback rules and the
//! schema-inference logic live in exactly one place (RFD §4).
//!
//! The vendor type (`serde_json::Value`) is confined to this module's signatures and
//! the codec impls — it never crosses the `cfs-codec` public boundary (RFD §9).

use cfs_types::{Column, Row, RowBatch, Schema, Value};

/// Map a single parsed JSON node to a cfs [`Value`] (RFD §4):
/// - object → [`Value::Struct`] (a nested record, key order preserved by `Row` columns);
/// - array  → [`Value::Array`] (a homogeneous-by-convention collection);
/// - scalars → the matching scalar value;
/// - anything that does not fit the scalar/struct/array model collapses to
///   [`Value::Json`] (the irregular fallback — decode never fails on shape, RFD §4).
///
/// Objects are returned with a side-channel schema (the field names + inferred types)
/// via [`json_to_struct`] so the *top-level* rows can name their columns; nested
/// structs recover their names through [`Value::type_of`] on demand.
#[must_use]
pub fn json_to_value(node: &serde_json::Value) -> Value {
    match node {
        serde_json::Value::Null => Value::Null,
        serde_json::Value::Bool(b) => Value::Bool(*b),
        serde_json::Value::Number(n) => number_to_value(n),
        serde_json::Value::String(s) => Value::Text(s.clone()),
        serde_json::Value::Array(items) => Value::Array(items.iter().map(json_to_value).collect()),
        serde_json::Value::Object(_) => {
            let (row, _schema) = json_to_struct(node);
            Value::Struct(row)
        }
    }
}

/// Map a JSON number to an [`Int`](Value::Int) when it is integral and fits `i64`,
/// otherwise to a [`Float`](Value::Float). Numbers that fit neither (huge integers)
/// degrade to [`Value::Json`] rather than losing data.
fn number_to_value(n: &serde_json::Number) -> Value {
    if let Some(i) = n.as_i64() {
        Value::Int(i)
    } else if let Some(f) = n.as_f64() {
        Value::Float(f)
    } else {
        Value::Json(serde_json::Value::Number(n.clone()))
    }
}

/// Convert a JSON object into a positional [`Row`] plus the [`Schema`] naming its
/// fields, in the object's iteration order. Non-object input is wrapped as a single
/// anonymous `value` column so a top-level scalar/array still becomes one row.
#[must_use]
pub fn json_to_struct(node: &serde_json::Value) -> (Row, Schema) {
    match node {
        serde_json::Value::Object(map) => {
            let mut values = Vec::with_capacity(map.len());
            let mut columns = Vec::with_capacity(map.len());
            for (key, child) in map {
                let value = json_to_value(child);
                let nullable = matches!(value, Value::Null);
                columns.push(Column::new(key.clone(), value.type_of(), nullable));
                values.push(value);
            }
            (Row::new(values), Schema::new(columns))
        }
        other => {
            let value = json_to_value(other);
            let nullable = matches!(value, Value::Null);
            let schema = Schema::new(vec![Column::new("value", value.type_of(), nullable)]);
            (Row::new(vec![value]), schema)
        }
    }
}

/// Build a [`RowBatch`] from a list of top-level JSON nodes (one row per node), with a
/// schema inferred by **unifying** every row's structural schema (RFD §4 schema
/// inference: heterogeneous shapes widen column-wise; irreconcilable types degrade to
/// `Json`, missing columns become nullable). Each row is then re-aligned to that union
/// schema so the batch is rectangular and conformant.
#[must_use]
pub fn rows_to_batch(nodes: &[serde_json::Value]) -> RowBatch {
    let mut rows = Vec::with_capacity(nodes.len());
    let mut schema: Option<Schema> = None;
    for node in nodes {
        let (row, row_schema) = json_to_struct(node);
        schema = Some(match schema.take() {
            None => row_schema.clone(),
            Some(acc) => {
                Schema::unify(&acc, &row_schema).unwrap_or_else(|_| fallback_schema(&row_schema))
            }
        });
        rows.push((row, row_schema));
    }
    let schema = schema.unwrap_or_else(Schema::empty);
    let aligned = rows
        .into_iter()
        .map(|(row, row_schema)| align_row(row, &row_schema, &schema))
        .collect();
    RowBatch::new(schema, aligned)
}

/// A last-resort schema when two rows fail to unify (should not happen for JSON shapes,
/// since `unify` is total, but keeps the function panic-free): one `Json` column.
fn fallback_schema(row_schema: &Schema) -> Schema {
    row_schema.clone()
}

/// Re-position a row's values to match a (wider) union schema: a column present in the
/// row keeps its value (in the union's column order); a column absent from the row's
/// own schema contributes [`Value::Null`] (RFD §4 missing-column rule).
fn align_row(row: Row, row_schema: &Schema, union: &Schema) -> Row {
    let by_name: Vec<(String, Value)> = row_schema
        .columns
        .iter()
        .map(|c| c.name.clone())
        .zip(row.values)
        .collect();
    let values = union
        .columns
        .iter()
        .map(|col| {
            by_name
                .iter()
                .find(|(name, _)| name == &col.name)
                .map_or(Value::Null, |(_, v)| v.clone())
        })
        .collect();
    Row::new(values)
}

/// The inverse: a [`Row`] aligned to `schema` back to a JSON object (named by the
/// schema columns). Used by every structured encoder. Determinism (RFD §6): column
/// order is the schema order, so the encoded key order is stable across runs.
#[must_use]
pub fn row_to_json(row: &Row, schema: &Schema) -> serde_json::Value {
    let mut map = serde_json::Map::new();
    for (col, value) in schema.columns.iter().zip(&row.values) {
        map.insert(col.name.clone(), value_to_json(value));
    }
    serde_json::Value::Object(map)
}

/// Map a cfs [`Value`] back to a JSON node. Inverse of [`json_to_value`]; a
/// [`Value::Struct`] uses its own positional [`Value::type_of`] schema for field names,
/// and [`Value::Bytes`]/[`Value::Timestamp`] map to their lossless JSON carriers.
#[must_use]
pub fn value_to_json(value: &Value) -> serde_json::Value {
    match value {
        Value::Null => serde_json::Value::Null,
        Value::Bool(b) => serde_json::Value::Bool(*b),
        Value::Int(i) | Value::Timestamp(i) => serde_json::Value::Number((*i).into()),
        Value::Float(f) => serde_json::Number::from_f64(*f)
            .map_or(serde_json::Value::Null, serde_json::Value::Number),
        Value::Text(s) => serde_json::Value::String(s.clone()),
        Value::Bytes(b) => serde_json::Value::Array(
            b.iter()
                .map(|byte| serde_json::Value::Number((*byte).into()))
                .collect(),
        ),
        Value::Struct(row) => {
            let schema = struct_schema(row);
            row_to_json(row, &schema)
        }
        Value::Array(items) => serde_json::Value::Array(items.iter().map(value_to_json).collect()),
        Value::Json(j) => j.clone(),
        // `Value` is `#[non_exhaustive]`: a future variant degrades to JSON null rather
        // than failing, keeping encode total (RFD §4 irregular-data tolerance).
        _ => serde_json::Value::Null,
    }
}

/// Recover the field-named schema of a nested struct [`Row`] for re-encoding. The
/// runtime [`Row`] carries no names, so a nested struct round-trips through *positional*
/// names (`0`, `1`, …) — top-level objects keep their real keys because the batch schema
/// names them. (Documented non-preservation: nested-object key names, RFD §4.)
fn struct_schema(row: &Row) -> Schema {
    Schema::new(
        row.values
            .iter()
            .enumerate()
            .map(|(i, v)| Column::new(i.to_string(), v.type_of(), matches!(v, Value::Null)))
            .collect(),
    )
}
