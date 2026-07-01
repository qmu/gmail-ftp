//! The relational operator kernels the [`MiniEvaluator`](crate::MiniEvaluator) runs over
//! [`RowBatch`]es: predicate evaluation, projection, sort, distinct, group/aggregate,
//! expand, hash-join, and set ops. Pure functions over owned values (no I/O).
//!
//! Each kernel returns a [`RowBatch`] (schema + rows). Predicate evaluation is total: an
//! incomparable / late-bound comparison evaluates to `false` (the row is filtered out)
//! rather than panicking — the planner already type-checked pushable predicates, and a
//! residual predicate over heterogeneous data degrades safely.

use std::cmp::Ordering;
use std::collections::BTreeSet;

use qfs_pushdown::{Aggregate, Aggregator, JoinOn, OrderKey, ScalarExpr, SetKind};
use qfs_types::{
    CmpOp, ColRef, Column, ColumnType, Fields, Literal, Name, Pattern, Predicate, Row, RowBatch,
    Schema, Value,
};

/// Evaluate a [`Predicate`] against a row under its schema. Total: a comparison whose
/// operands are not comparable evaluates to `false` (the row does not match).
#[must_use]
pub(crate) fn eval_predicate(p: &Predicate, schema: &Schema, row: &Row) -> bool {
    match p {
        Predicate::And(a, b) => eval_predicate(a, schema, row) && eval_predicate(b, schema, row),
        Predicate::Or(a, b) => eval_predicate(a, schema, row) || eval_predicate(b, schema, row),
        Predicate::Not(inner) => !eval_predicate(inner, schema, row),
        Predicate::Cmp(col, op, lit) => match resolve(col, schema, row) {
            Some(v) => cmp(&v, *op, lit),
            None => false,
        },
        Predicate::In(col, set) => match resolve(col, schema, row) {
            Some(v) => set.iter().any(|lit| cmp(&v, CmpOp::Eq, lit)),
            None => false,
        },
        Predicate::Between(col, low, high) => match resolve(col, schema, row) {
            Some(v) => cmp(&v, CmpOp::Ge, low) && cmp(&v, CmpOp::Le, high),
            None => false,
        },
        Predicate::Like(col, pattern) => match resolve(col, schema, row) {
            Some(Value::Text(s)) => like_match(&s, pattern),
            _ => false,
        },
    }
}

/// Resolve a [`ColRef`] to the row's value. A bare column is a positional lookup; a
/// dotted path navigates `Struct` fields. Missing/unnavigable ⇒ `None`.
fn resolve(col: &ColRef, schema: &Schema, row: &Row) -> Option<Value> {
    let (head, rest) = col.path.split_first()?;
    let idx = schema.columns.iter().position(|c| &c.name == head)?;
    let mut cur = row.values.get(idx)?.clone();
    for seg in rest {
        match cur {
            Value::Struct(fields) => cur = fields.get(seg)?.clone(),
            _ => return None,
        }
    }
    Some(cur)
}

/// Compare a runtime value to a literal under an operator. Numeric values widen
/// (`Int`/`Float`); text compares lexically; `Null` never matches a comparison.
fn cmp(v: &Value, op: CmpOp, lit: &Literal) -> bool {
    let ord = value_cmp(v, lit);
    match (op, ord) {
        (CmpOp::Eq, Some(Ordering::Equal)) => true,
        (CmpOp::Ne, Some(o)) => o != Ordering::Equal,
        (CmpOp::Lt, Some(Ordering::Less)) => true,
        (CmpOp::Le, Some(Ordering::Less | Ordering::Equal)) => true,
        (CmpOp::Gt, Some(Ordering::Greater)) => true,
        (CmpOp::Ge, Some(Ordering::Greater | Ordering::Equal)) => true,
        (CmpOp::Match, _) => match (v, lit) {
            (Value::Text(s), Literal::Text(p)) => regex_lite(s, p),
            _ => false,
        },
        _ => false,
    }
}

/// A partial ordering between a runtime value and a literal (numeric widening; text
/// lexical; bool false<true). Incomparable / null ⇒ `None`.
fn value_cmp(v: &Value, lit: &Literal) -> Option<Ordering> {
    match (v, lit) {
        (Value::Null, _) | (_, Literal::Null) => None,
        (Value::Int(a), Literal::Int(b)) => Some(a.cmp(b)),
        (Value::Int(a), Literal::Float(b)) => (*a as f64).partial_cmp(b),
        (Value::Float(a), Literal::Float(b)) => a.partial_cmp(b),
        (Value::Float(a), Literal::Int(b)) => a.partial_cmp(&(*b as f64)),
        (Value::Timestamp(a), Literal::Int(b)) => Some(a.cmp(b)),
        (Value::Text(a), Literal::Text(b)) => Some(a.as_str().cmp(b.as_str())),
        (Value::Bool(a), Literal::Bool(b)) => Some(a.cmp(b)),
        _ => None,
    }
}

/// A minimal `LIKE` matcher: `%` = any run, `_` = any single char. Anchored.
fn like_match(s: &str, pattern: &Pattern) -> bool {
    like_inner(s.as_bytes(), pattern.0.as_bytes())
}

fn like_inner(s: &[u8], p: &[u8]) -> bool {
    match p.split_first() {
        None => s.is_empty(),
        Some((b'%', rest)) => {
            // `%` matches zero or more chars: try every suffix of `s`.
            (0..=s.len()).any(|i| like_inner(&s[i..], rest))
        }
        Some((b'_', rest)) => !s.is_empty() && like_inner(&s[1..], rest),
        Some((c, rest)) => s.first() == Some(c) && like_inner(&s[1..], rest),
    }
}

/// A tiny anchored regex subset for `~`: only treats `.*`/`.` specially, otherwise a
/// substring test. Kept minimal (the residual rarely needs `~`); a full engine is E4.
fn regex_lite(s: &str, p: &str) -> bool {
    if let Some(inner) = p.strip_prefix(".*").and_then(|x| x.strip_suffix(".*")) {
        s.contains(inner)
    } else {
        s.contains(p)
    }
}

/// Filter a batch by a predicate.
#[must_use]
pub(crate) fn filter(batch: RowBatch, p: &Predicate) -> RowBatch {
    let schema = batch.schema.clone();
    let rows = batch
        .rows
        .into_iter()
        .filter(|r| eval_predicate(p, &schema, r))
        .collect();
    RowBatch::new(schema, rows)
}

/// Project a batch to a column list (`*`/empty is identity). Unknown columns are dropped.
#[must_use]
pub(crate) fn project(batch: RowBatch, columns: &[Name]) -> RowBatch {
    if columns.is_empty() || columns == ["*".to_string()] {
        return batch;
    }
    let indices: Vec<(usize, Column)> = columns
        .iter()
        .filter_map(|name| {
            batch
                .schema
                .columns
                .iter()
                .position(|c| &c.name == name)
                .map(|i| (i, batch.schema.columns[i].clone()))
        })
        .collect();
    let schema = Schema::new(indices.iter().map(|(_, c)| c.clone()).collect());
    let rows = batch
        .rows
        .into_iter()
        .map(|r| {
            Row::new(
                indices
                    .iter()
                    .map(|(i, _)| r.values.get(*i).cloned().unwrap_or(Value::Null))
                    .collect(),
            )
        })
        .collect();
    RowBatch::new(schema, rows)
}

/// Evaluate a per-row [`ScalarExpr`] against a row under its schema (t92). Total: a column
/// that does not resolve is `Null` (mirroring the projection/predicate late-binding), never a
/// panic. A `Struct`/`Array` constructor builds the mirrored [`Value`] from its evaluated
/// field/element expressions (field order preserved).
#[must_use]
pub(crate) fn eval_value(expr: &ScalarExpr, schema: &Schema, row: &Row) -> Value {
    match expr {
        ScalarExpr::Col(col) => resolve(col, schema, row).unwrap_or(Value::Null),
        ScalarExpr::Lit(v) => v.clone(),
        ScalarExpr::Array(elems) => {
            Value::Array(elems.iter().map(|e| eval_value(e, schema, row)).collect())
        }
        ScalarExpr::Struct(fields) => Value::Struct(Fields::new(
            fields
                .iter()
                .map(|(name, e)| (name.clone(), eval_value(e, schema, row)))
                .collect(),
        )),
    }
}

/// A **computed** projection (t92): each output column is a per-row [`ScalarExpr`] (a struct/
/// array constructor over the input columns). Unlike name-only [`project`], this evaluates an
/// expression per row, so `SELECT {filename: name, bytes: content} AS att` produces a real
/// `Struct` value. Output column types are late-bound (`Unknown`) — the value carries its shape.
#[must_use]
pub(crate) fn project_expr(batch: RowBatch, projections: &[(Name, ScalarExpr)]) -> RowBatch {
    let schema = Schema::new(
        projections
            .iter()
            .map(|(name, _)| Column::new(name.clone(), ColumnType::Unknown, true))
            .collect(),
    );
    let src = batch.schema;
    let rows = batch
        .rows
        .into_iter()
        .map(|r| {
            Row::new(
                projections
                    .iter()
                    .map(|(_, e)| eval_value(e, &src, &r))
                    .collect(),
            )
        })
        .collect();
    RowBatch::new(schema, rows)
}

/// `EXTEND`/`SET` (t92): add or overwrite columns with per-row computed values. An assignment
/// naming an existing column overwrites it in place; a new name is appended. Assignments apply
/// left-to-right over a progressively-updated row, so a later assignment can read an earlier
/// one. Output column types are late-bound (`Unknown`).
#[must_use]
pub(crate) fn extend(batch: RowBatch, assignments: &[(Name, ScalarExpr)]) -> RowBatch {
    // Resolve the output column layout once, recording each assignment's target index.
    let mut out_cols: Vec<Column> = batch.schema.columns.clone();
    let mut targets: Vec<usize> = Vec::with_capacity(assignments.len());
    for (name, _) in assignments {
        if let Some(i) = out_cols.iter().position(|c| &c.name == name) {
            out_cols[i].ty = ColumnType::Unknown;
            targets.push(i);
        } else {
            out_cols.push(Column::new(name.clone(), ColumnType::Unknown, true));
            targets.push(out_cols.len() - 1);
        }
    }
    let schema = Schema::new(out_cols);
    let width = schema.columns.len();
    let rows = batch
        .rows
        .into_iter()
        .map(|r| {
            // Pad the row to the full output width; evaluate each assignment against the
            // progressively-updated row (so a later assignment sees an earlier one).
            let mut values = r.values;
            values.resize(width, Value::Null);
            for ((_, expr), &idx) in assignments.iter().zip(&targets) {
                let cur = Row::new(values.clone());
                values[idx] = eval_value(expr, &schema, &cur);
            }
            Row::new(values)
        })
        .collect();
    RowBatch::new(schema, rows)
}

/// Cap a batch to at most `n` rows.
#[must_use]
pub(crate) fn limit(mut batch: RowBatch, n: u64) -> RowBatch {
    batch.rows.truncate(n as usize);
    batch
}

/// Stable-sort a batch by the order keys (deterministic; ties keep input order).
#[must_use]
pub(crate) fn sort(mut batch: RowBatch, keys: &[OrderKey]) -> RowBatch {
    let positions: Vec<(usize, bool)> = keys
        .iter()
        .filter_map(|k| {
            batch
                .schema
                .columns
                .iter()
                .position(|c| c.name == k.column)
                .map(|i| (i, k.descending))
        })
        .collect();
    batch.rows.sort_by(|a, b| {
        for (idx, desc) in &positions {
            let ord = order_values(a.values.get(*idx), b.values.get(*idx));
            let ord = if *desc { ord.reverse() } else { ord };
            if ord != Ordering::Equal {
                return ord;
            }
        }
        Ordering::Equal
    });
    batch
}

/// A total ordering between two runtime values for sorting (Null sorts first).
fn order_values(a: Option<&Value>, b: Option<&Value>) -> Ordering {
    match (a, b) {
        (None, None) => Ordering::Equal,
        (None, _) | (Some(Value::Null), _) => Ordering::Less,
        (_, None) | (_, Some(Value::Null)) => Ordering::Greater,
        (Some(x), Some(y)) => match (x, y) {
            (Value::Int(p), Value::Int(q)) => p.cmp(q),
            (Value::Float(p), Value::Float(q)) => p.partial_cmp(q).unwrap_or(Ordering::Equal),
            (Value::Int(p), Value::Float(q)) => {
                (*p as f64).partial_cmp(q).unwrap_or(Ordering::Equal)
            }
            (Value::Float(p), Value::Int(q)) => {
                p.partial_cmp(&(*q as f64)).unwrap_or(Ordering::Equal)
            }
            (Value::Timestamp(p), Value::Timestamp(q)) => p.cmp(q),
            (Value::Text(p), Value::Text(q)) => p.cmp(q),
            (Value::Bool(p), Value::Bool(q)) => p.cmp(q),
            // Mixed/other kinds: compare by a stable debug rendering (deterministic).
            _ => format!("{x:?}").cmp(&format!("{y:?}")),
        },
    }
}

/// Deduplicate rows (preserving first-seen order), keyed by a stable rendering.
#[must_use]
pub(crate) fn distinct(batch: RowBatch) -> RowBatch {
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let schema = batch.schema.clone();
    let rows = batch
        .rows
        .into_iter()
        .filter(|r| seen.insert(row_key(r)))
        .collect();
    RowBatch::new(schema, rows)
}

/// A stable string key for a row (used by distinct / set ops / hash-join probe).
fn row_key(r: &Row) -> String {
    format!("{:?}", r.values)
}

/// A stable string key for a single value (hash-join key).
fn value_key(v: &Value) -> String {
    format!("{v:?}")
}

/// Group + aggregate a batch (RFD §4). Empty `group_by` ⇒ a single output row over the
/// whole batch. Output schema is the group columns followed by one column per aggregate.
#[must_use]
pub(crate) fn aggregate(batch: RowBatch, group_by: &[Name], aggs: &[Aggregate]) -> RowBatch {
    let group_idx: Vec<usize> = group_by
        .iter()
        .filter_map(|g| batch.schema.columns.iter().position(|c| &c.name == g))
        .collect();
    let agg_idx: Vec<Option<usize>> = aggs
        .iter()
        .map(|a| batch.schema.columns.iter().position(|c| c.name == a.column))
        .collect();

    // Group rows by their group-column key, preserving first-seen group order.
    let mut order: Vec<String> = Vec::new();
    let mut groups: std::collections::BTreeMap<String, Vec<Row>> =
        std::collections::BTreeMap::new();
    let mut first_key_row: std::collections::BTreeMap<String, Row> =
        std::collections::BTreeMap::new();
    for row in batch.rows {
        let key: Vec<String> = group_idx
            .iter()
            .map(|i| value_key(row.values.get(*i).unwrap_or(&Value::Null)))
            .collect();
        let key = key.join("\u{1}");
        if !groups.contains_key(&key) {
            order.push(key.clone());
            first_key_row.insert(key.clone(), row.clone());
        }
        groups.entry(key).or_default().push(row);
    }
    // Whole-relation aggregate with no rows still yields one row (e.g. COUNT = 0).
    if group_by.is_empty() && order.is_empty() {
        order.push(String::new());
        groups.insert(String::new(), Vec::new());
    }

    let mut out_cols: Vec<Column> = group_by
        .iter()
        .map(|g| Column::new(g.clone(), ColumnType::Unknown, true))
        .collect();
    for a in aggs {
        let ty = match a.func {
            Aggregator::Count => ColumnType::Int,
            Aggregator::ArrayAgg => ColumnType::Array(Box::new(ColumnType::Unknown)),
            _ => ColumnType::Unknown,
        };
        out_cols.push(Column::new(a.output.clone(), ty, true));
    }
    let schema = Schema::new(out_cols);

    let mut out_rows = Vec::with_capacity(order.len());
    for key in &order {
        let rows = groups.get(key).cloned().unwrap_or_default();
        let mut values: Vec<Value> = Vec::new();
        if let Some(sample) = first_key_row.get(key) {
            for i in &group_idx {
                values.push(sample.values.get(*i).cloned().unwrap_or(Value::Null));
            }
        } else {
            for _ in &group_idx {
                values.push(Value::Null);
            }
        }
        for (a, idx) in aggs.iter().zip(&agg_idx) {
            values.push(run_aggregate(a.func, *idx, &rows));
        }
        out_rows.push(Row::new(values));
    }
    RowBatch::new(schema, out_rows)
}

fn run_aggregate(func: Aggregator, col: Option<usize>, rows: &[Row]) -> Value {
    let vals: Vec<&Value> = match col {
        Some(i) => rows
            .iter()
            .filter_map(|r| r.values.get(i))
            .filter(|v| !matches!(v, Value::Null))
            .collect(),
        None => Vec::new(),
    };
    match func {
        Aggregator::Count => Value::Int(if col.is_some() {
            vals.len() as i64
        } else {
            rows.len() as i64
        }),
        Aggregator::Sum => {
            let mut acc = 0.0_f64;
            let mut any_float = false;
            for v in &vals {
                match v {
                    Value::Int(n) => acc += *n as f64,
                    Value::Float(f) => {
                        any_float = true;
                        acc += f;
                    }
                    _ => {}
                }
            }
            if any_float {
                Value::Float(acc)
            } else {
                Value::Int(acc as i64)
            }
        }
        Aggregator::Min => fold_extreme(&vals, Ordering::Less),
        Aggregator::Max => fold_extreme(&vals, Ordering::Greater),
        // `ARRAY_AGG(col)` collects the column's per-row values in row order into one `Array`.
        // Unlike the numeric aggregates it keeps every row's cell (including nulls) — it is a
        // faithful collect, not a fold — so N input rows pack into one Array of N elements.
        Aggregator::ArrayAgg => Value::Array(match col {
            Some(i) => rows
                .iter()
                .filter_map(|r| r.values.get(i).cloned())
                .collect(),
            None => Vec::new(),
        }),
    }
}

fn fold_extreme(vals: &[&Value], want: Ordering) -> Value {
    let mut best: Option<&Value> = None;
    for v in vals {
        match best {
            None => best = Some(v),
            Some(b) => {
                if order_values(Some(v), Some(b)) == want {
                    best = Some(v);
                }
            }
        }
    }
    best.cloned().unwrap_or(Value::Null)
}

/// `EXPAND <field>` — explode a nested collection column into rows (RFD §4). An `Array`
/// of structs flattens each element's fields; an `Array` of scalars yields one row per
/// element; a `Struct` flattens one level. Non-collection fields pass the row through.
#[must_use]
pub(crate) fn expand(batch: RowBatch, field: &Name) -> RowBatch {
    let Some(idx) = batch.schema.columns.iter().position(|c| &c.name == field) else {
        return batch;
    };
    // Output schema: replace the field column per the type model's `expand`.
    let schema = batch.schema.expand(field).unwrap_or(batch.schema.clone());
    let mut out_rows = Vec::new();
    for row in batch.rows {
        let target = row.values.get(idx).cloned().unwrap_or(Value::Null);
        match target {
            Value::Array(items) => {
                for item in items {
                    out_rows.push(splice_row(&row, idx, expand_item(item)));
                }
            }
            Value::Struct(fields) => {
                out_rows.push(splice_row(&row, idx, fields.into_values()));
            }
            // A scalar/Null field is not expandable: keep the row unchanged.
            other => out_rows.push(splice_row(&row, idx, vec![other])),
        }
    }
    RowBatch::new(schema, out_rows)
}

/// Flatten one expanded element into the row's replacement values.
fn expand_item(item: Value) -> Vec<Value> {
    match item {
        Value::Struct(fields) => fields.into_values(),
        other => vec![other],
    }
}

/// Replace position `idx` of `row` with `replacement` values (de-nesting in place).
fn splice_row(row: &Row, idx: usize, replacement: Vec<Value>) -> Row {
    let mut values = Vec::with_capacity(row.values.len() + replacement.len());
    values.extend_from_slice(&row.values[..idx]);
    values.extend(replacement);
    values.extend_from_slice(&row.values[idx + 1..]);
    Row::new(values)
}

/// A hash join over two batches on `on.left = on.right` (RFD §6 federation). Builds a
/// hash table on the right, probes with the left; output columns are the left schema
/// followed by the right schema with collisions disambiguated ([`Schema::join`]).
#[must_use]
pub(crate) fn hash_join(left: RowBatch, right: RowBatch, on: &JoinOn) -> RowBatch {
    let schema = left.schema.join(&right.schema);
    let Some(lk) = left.schema.columns.iter().position(|c| c.name == on.left) else {
        return RowBatch::new(schema, Vec::new());
    };
    let Some(rk) = right.schema.columns.iter().position(|c| c.name == on.right) else {
        return RowBatch::new(schema, Vec::new());
    };
    // Build side: map right join-key → rows.
    let mut table: std::collections::BTreeMap<String, Vec<Row>> = std::collections::BTreeMap::new();
    for row in &right.rows {
        let key = value_key(row.values.get(rk).unwrap_or(&Value::Null));
        table.entry(key).or_default().push(row.clone());
    }
    let mut out_rows = Vec::new();
    for lrow in &left.rows {
        let key = value_key(lrow.values.get(lk).unwrap_or(&Value::Null));
        if let Some(matches) = table.get(&key) {
            for rrow in matches {
                let mut values = lrow.values.clone();
                values.extend(rrow.values.clone());
                out_rows.push(Row::new(values));
            }
        }
    }
    RowBatch::new(schema, out_rows)
}

/// A set op over two batches (RFD §4). `UNION` is the distinct union; `EXCEPT` is left
/// rows absent from the right; `INTERSECT` is rows present in both. Keyed by a stable row
/// rendering; the output schema is the left schema (sides are union-compatible).
#[must_use]
pub(crate) fn set_op(left: RowBatch, right: RowBatch, kind: SetKind) -> RowBatch {
    let schema = left.schema.clone();
    let right_keys: BTreeSet<String> = right.rows.iter().map(row_key).collect();
    match kind {
        SetKind::Union => {
            let mut seen: BTreeSet<String> = BTreeSet::new();
            let mut rows = Vec::new();
            for r in left.rows.into_iter().chain(right.rows) {
                if seen.insert(row_key(&r)) {
                    rows.push(r);
                }
            }
            RowBatch::new(schema, rows)
        }
        SetKind::Except => {
            let mut seen: BTreeSet<String> = BTreeSet::new();
            let rows = left
                .rows
                .into_iter()
                .filter(|r| !right_keys.contains(&row_key(r)) && seen.insert(row_key(r)))
                .collect();
            RowBatch::new(schema, rows)
        }
        SetKind::Intersect => {
            let mut seen: BTreeSet<String> = BTreeSet::new();
            let rows = left
                .rows
                .into_iter()
                .filter(|r| right_keys.contains(&row_key(r)) && seen.insert(row_key(r)))
                .collect();
            RowBatch::new(schema, rows)
        }
    }
}
