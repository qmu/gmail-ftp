//! Value-level `EXPAND` and `a.b.c` path access (RFD §4 nested-data layer). The
//! *type*-level counterparts ([`cfs_types::Schema::expand`] /
//! [`cfs_types::Schema::resolve_path`]) describe the schema transform; these operate on
//! the runtime [`RowBatch`]/[`Value`] so the query side can actually explode collections
//! and navigate structs. One `EXPAND` serves mail attachments and JSON arrays alike.

use cfs_types::{Row, RowBatch, Schema, Value};

/// Explode a named collection column into rows (RFD §4 `EXPAND`). For each input row,
/// the value at `field`:
/// - [`Value::Array`] → emits **one output row per element**; an array of
///   [`Value::Struct`] flattens each element's fields into the row columns (matching
///   [`Schema::expand`]); an array of scalars replaces the field column with the element.
/// - [`Value::Struct`] → flattens the struct's fields into the row (de-nesting one level).
/// - **scalar / `Null` / `Json` / absent** → the row passes through **unchanged** but
///   with the field column dropped from the schema only if it was expandable. The
///   documented rule: a non-collection `EXPAND` is a **passthrough** that yields zero
///   *extra* rows (it does not multiply), and an empty array yields **zero** rows for
///   that input (the row is filtered out).
///
/// The schema is computed once via [`Schema::expand`]; if the field is not expandable
/// at the type level the original batch is returned unchanged (passthrough).
///
/// # Errors
/// Never errors: it degrades to passthrough so a pipeline stays runnable (RFD §4). The
/// `Result` shape is reserved for a future strict mode.
#[must_use]
pub fn expand(batch: &RowBatch, field: &str) -> RowBatch {
    let Some(idx) = batch.schema.columns.iter().position(|c| c.name == field) else {
        // Absent field: passthrough (RFD §4 documented rule).
        return batch.clone();
    };

    let Ok(out_schema) = batch.schema.expand(&field.to_string()) else {
        // Not expandable (scalar/Json/Unknown): passthrough unchanged.
        return batch.clone();
    };

    let mut out_rows = Vec::new();
    for row in &batch.rows {
        let target = row.values.get(idx).cloned().unwrap_or(Value::Null);
        match target {
            Value::Array(items) => {
                // One output row per element. Empty array → zero rows (filtered).
                for item in items {
                    out_rows.push(splice_row(row, idx, expand_element(item)));
                }
            }
            Value::Struct(inner) => {
                out_rows.push(splice_row(row, idx, inner.values));
            }
            // Scalar/Null/Json reaching here despite an expandable schema (irregular
            // row): keep it as a single passthrough value in the replacement slot.
            other => out_rows.push(splice_row(row, idx, vec![other])),
        }
    }
    RowBatch::new(out_schema, out_rows)
}

/// The replacement values an array element contributes: a struct element flattens to its
/// fields; any other element is a single value (matching [`Schema::expand`]'s
/// array-of-struct vs array-of-scalar split).
fn expand_element(item: Value) -> Vec<Value> {
    match item {
        Value::Struct(inner) => inner.values,
        other => vec![other],
    }
}

/// Replace the value at `idx` in `row` with `replacement` (zero or more values),
/// preserving the surrounding columns — the row counterpart of the schema splice in
/// [`Schema::expand`].
fn splice_row(row: &Row, idx: usize, replacement: Vec<Value>) -> Row {
    let mut values = Vec::with_capacity(row.values.len() + replacement.len());
    values.extend_from_slice(&row.values[..idx]);
    values.extend(replacement);
    if idx < row.values.len() {
        values.extend_from_slice(&row.values[idx + 1..]);
    }
    Row::new(values)
}

/// Navigate a dotted path `a.b.c` into a [`Value`] **without flattening** (RFD §4 path
/// access). `path` is the segment list *after* the head column is already selected:
/// callers resolve the head column positionally against the schema, then hand the head
/// value plus the remaining segments here.
///
/// - Into a [`Value::Struct`], the next segment selects the field by its **schema name**
///   (so the caller passes the struct's schema alongside).
/// - Into a [`Value::Json`], navigation is late-bound: it walks the JSON object keys and
///   returns the sub-tree as a fresh [`Value::Json`] (or [`Value::Null`] if absent).
/// - Into a scalar/array with remaining segments → [`None`] (no such path).
///
/// Returns the navigated [`Value`] (cloned) or [`None`] if the path does not resolve.
#[must_use]
pub fn access(value: &Value, schema: &Schema, path: &[&str]) -> Option<Value> {
    let Some((head, rest)) = path.split_first() else {
        return Some(value.clone());
    };
    match value {
        Value::Struct(row) => {
            let idx = schema.columns.iter().position(|c| &c.name == head)?;
            let child = row.values.get(idx)?;
            let child_schema = match &schema.columns.get(idx)?.ty {
                cfs_types::ColumnType::Struct(s) => s.clone(),
                _ => Schema::empty(),
            };
            access(child, &child_schema, rest)
        }
        Value::Json(json) => access_json(json, path).map(Value::Json),
        _ => None,
    }
}

/// Late-bound navigation into a raw JSON sub-tree (RFD §4 `Json`-column path access).
fn access_json(json: &serde_json::Value, path: &[&str]) -> Option<serde_json::Value> {
    let Some((head, rest)) = path.split_first() else {
        return Some(json.clone());
    };
    match json {
        serde_json::Value::Object(map) => access_json(map.get(*head)?, rest),
        _ => None,
    }
}

/// Navigate a path from a top-level [`Row`] against its [`Schema`]: resolves the head
/// column by name, then descends with [`access`]. The convenience entry the query side
/// uses for `a.b.c` over a relation.
#[must_use]
pub fn access_row(row: &Row, schema: &Schema, path: &[&str]) -> Option<Value> {
    let (head, _rest) = path.split_first()?;
    let idx = schema.columns.iter().position(|c| c.name == *head)?;
    let value = row.values.get(idx)?;
    let head_schema = match &schema.columns.get(idx)?.ty {
        cfs_types::ColumnType::Struct(s) => s.clone(),
        _ => Schema::empty(),
    };
    // `access` expects the value plus the *remaining* path; the head is already consumed.
    access(value, &head_schema, &path[1..])
}
