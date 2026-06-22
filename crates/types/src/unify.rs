//! [`Schema::unify`] — column-wise least-upper-bound for `UNION` over heterogeneous
//! / sparse sources (RFD §4, the hard part). A `UNION` of Gmail rows and SQL rows, or
//! two JSON documents with different shapes, **must not error**:
//!
//! - **Missing columns become `nullable`** (a column present in one side only is kept,
//!   marked nullable, since the other side contributes `Null`).
//! - **Scalars widen** (`Int` ∨ `Float` ⇒ `Float`; a type ∨ `Unknown` ⇒ that type;
//!   `Unknown` ∨ `Unknown` ⇒ `Unknown`).
//! - **Irreconcilable types degrade to `Json`** rather than failing, keeping the
//!   pipeline runnable while preserving structure where the two sides agree.
//! - **Nested `Struct`s unify recursively**; `Array`s unify element-wise.
//!
//! ## Widening matrix (`widen(a, b)`), symmetric
//! | a \\ b      | `Unknown` | same scalar | `Int`/`Float` | `Struct` | `Array` | other |
//! |-------------|-----------|-------------|---------------|----------|---------|-------|
//! | `Unknown`   | `Unknown` | b           | b             | b        | b       | b     |
//! | same scalar | a         | a           | `Float`*      | `Json`   | `Json`  | `Json`|
//! | `Struct`    | a         | `Json`      | `Json`        | unify    | `Json`  | `Json`|
//! | `Array`     | a         | `Json`      | `Json`        | `Json`   | elem ∨  | `Json`|
//!
//! \* only when the pair is exactly `{Int, Float}` (numeric widening); other scalar
//! mismatches degrade to `Json`.
//!
//! `unify` is **idempotent** (`unify(a, a) == a`) and **commutative up to column
//! order** (proven by property tests).

use crate::error::TypeError;
use crate::schema::{Column, ColumnType, Provenance, Schema};

/// The least upper bound of two column types per the widening matrix above.
#[must_use]
pub fn widen(a: &ColumnType, b: &ColumnType) -> ColumnType {
    use ColumnType::{Array, Float, Int, Json, Struct, Unknown};

    match (a, b) {
        // Unknown is the bottom: it widens to the other side.
        (Unknown, other) | (other, Unknown) => other.clone(),
        // Numeric widening: Int ∨ Float ⇒ Float.
        (Int, Float) | (Float, Int) => Float,
        // Recursive struct unification (heterogeneous JSON shapes, RFD §4).
        (Struct(sa), Struct(sb)) => match unify_schema(sa, sb) {
            Ok(s) => Struct(s),
            // Unification cannot fail today, but degrade defensively to Json.
            Err(_) => Json,
        },
        // Element-wise array widening.
        (Array(ea), Array(eb)) => Array(Box::new(widen(ea, eb))),
        // Identical types are their own LUB.
        _ if a == b => a.clone(),
        // Json absorbs anything irreconcilable (and explicit Json on either side).
        _ => Json,
    }
}

/// Unify two schemas column-wise for `UNION` (RFD §4). Never errors today; the
/// `Result` is kept so a future strict mode can reject (callers already thread it).
///
/// # Errors
/// Reserved: currently always `Ok`. The signature carries `TypeError` so a future
/// strict-union policy can reject without a breaking change.
pub fn unify_schema(a: &Schema, b: &Schema) -> Result<Schema, TypeError> {
    let mut out: Vec<Column> = Vec::new();

    // Pass 1: every column of `a`, widened against its `b` counterpart if present.
    for ca in &a.columns {
        match b.column(&ca.name) {
            Some(cb) => {
                let ty = widen(&ca.ty, &cb.ty);
                // Null appears if either side is nullable OR the types disagreed.
                let nullable = ca.nullable || cb.nullable;
                out.push(Column {
                    name: ca.name.clone(),
                    ty,
                    nullable,
                    provenance: merge_provenance(&ca.provenance, &cb.provenance),
                });
            }
            None => {
                // Present in `a` only ⇒ the other side contributes Null ⇒ nullable.
                out.push(Column {
                    name: ca.name.clone(),
                    ty: ca.ty.clone(),
                    nullable: true,
                    provenance: ca.provenance.clone(),
                });
            }
        }
    }

    // Pass 2: columns present in `b` only, appended after `a`'s columns, nullable.
    for cb in &b.columns {
        if a.column(&cb.name).is_none() {
            out.push(Column {
                name: cb.name.clone(),
                ty: cb.ty.clone(),
                nullable: true,
                provenance: cb.provenance.clone(),
            });
        }
    }

    Ok(Schema::new(out))
}

/// Merge two provenances: keep a value where the two agree, else drop to `None`
/// (a unified column with two distinct sources has no single provenance).
fn merge_provenance(a: &Provenance, b: &Provenance) -> Provenance {
    Provenance {
        driver: if a.driver == b.driver {
            a.driver.clone()
        } else {
            None
        },
        source_col: if a.source_col == b.source_col {
            a.source_col.clone()
        } else {
            None
        },
    }
}

impl Schema {
    /// Column-wise least upper bound of two schemas for `UNION` over heterogeneous
    /// sources (RFD §4). See the [module docs](crate::unify) for the widening matrix.
    /// Idempotent and commutative up to column order; never errors today.
    ///
    /// # Errors
    /// Reserved (currently always `Ok`); see [`unify_schema`].
    pub fn unify(a: &Schema, b: &Schema) -> Result<Schema, TypeError> {
        unify_schema(a, b)
    }
}
