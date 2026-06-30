//! `compile` — the pure **qfs query → parameterized SQL plan** compiler (RFD-0001 §5/§6, ticket
//! steps 6/7). It lowers a relational query (projection, `WHERE`, `ORDER BY`, `LIMIT`) over a
//! catalogued table into a [`SelectPlan`] (the emitter renders it to parameterized SQL) plus a
//! **truthful residual** `Option<Predicate>` the engine still filters locally.
//!
//! ## The t20/t21 lesson — SQL is the lucky exact case (the headline invariant)
//! SQL is a full backend: when a predicate compiles to a SQL `WHERE` that is **exactly
//! equivalent**, the residual is **dropped** (the backend filters it precisely). When a construct
//! cannot be faithfully rendered to portable SQL with identical semantics, it is **kept as
//! residual** and the engine re-applies it over the (over-fetched) rows — never wrong rows:
//! - `col = / <> / < / > / <= / >= lit`  → exact comparison         → DROPPED
//! - `col IN (a, b, ...)`                → exact membership          → DROPPED
//! - `col BETWEEN low AND high`          → exact range               → DROPPED
//! - `a AND b` where both compile        → conjunction               → DROPPED
//! - `LIKE` (qfs glob vs SQL `LIKE` differ), `~` (regex not portable across pg/mysql/sqlite),
//!   `OR` / `NOT`, or any leaf referencing an unknown column → KEPT as residual.
//!
//! Every value operand becomes a bound [`Param`]; the compiler performs **no I/O** and holds no
//! credential — it reads only the owned [`TableCatalog`] and the typed [`Predicate`].

use qfs_types::{CmpOp, ColRef, Literal, Predicate};

use crate::catalog::TableCatalog;
use crate::emit::{OrderTerm, Param, SelectPlan, SqlPredicate};
use crate::error::SqlError;

/// The relational query inputs the compiler lowers — the projection (column names in SELECT
/// order), the optional `WHERE`, the optional `ORDER BY` as `(column, desc)`, and the optional
/// `LIMIT`. Owned, vendor-free; built by the planner from the typed relational subtree.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct QuerySpec {
    /// The projected column names, in SELECT order (empty ⇒ all columns / `SELECT *`).
    pub projection: Vec<String>,
    /// The `WHERE` predicate, if any.
    pub predicate: Option<Predicate>,
    /// The `ORDER BY` items as `(column, desc)`, in order.
    pub order_by: Vec<(String, bool)>,
    /// The `LIMIT`, if any.
    pub limit: Option<i64>,
}

impl QuerySpec {
    /// A spec with just a projection.
    #[must_use]
    pub fn new(projection: Vec<String>) -> Self {
        Self {
            projection,
            ..Self::default()
        }
    }

    /// Builder: set the `WHERE` predicate.
    #[must_use]
    pub fn with_predicate(mut self, predicate: Predicate) -> Self {
        self.predicate = Some(predicate);
        self
    }

    /// Builder: add an `ORDER BY` item.
    #[must_use]
    pub fn order_by(mut self, column: impl Into<String>, desc: bool) -> Self {
        self.order_by.push((column.into(), desc));
        self
    }

    /// Builder: set the `LIMIT`.
    #[must_use]
    pub fn with_limit(mut self, limit: i64) -> Self {
        self.limit = Some(limit);
        self
    }
}

/// The result of compiling a query: the pushed-down [`SelectPlan`] and the residual predicate the
/// engine still filters locally (RFD §6). `residual` is `None` when the whole `WHERE` pushed down
/// with exact mappings.
#[derive(Debug, Clone, PartialEq)]
pub struct CompileResult {
    /// The native SQL plan the emitter renders (one SELECT per query).
    pub plan: SelectPlan,
    /// The predicate the backend could **not** express exactly — the engine filters this locally.
    pub residual: Option<Predicate>,
}

/// Compile a relational query over a catalogued table into a [`SelectPlan`] + a truthful residual.
///
/// `schema`/`table` come from the resolved [`SqlPath`](crate::path::SqlPath); `catalog` is the
/// table's catalog (used to validate every projected/ordered/filtered column with a structured
/// error rather than a raw backend failure).
///
/// # Errors
/// [`SqlError::UnknownColumn`] if a projected or `ORDER BY` name is not a column of the table.
pub fn compile(
    schema: &str,
    table: &TableCatalog,
    spec: &QuerySpec,
) -> Result<CompileResult, SqlError> {
    // Validate the projection against the catalog (an unknown column is a structured error, never
    // a raw backend syntax failure).
    for name in &spec.projection {
        if table.column(name).is_none() {
            return Err(SqlError::UnknownColumn {
                name: name.clone(),
                reason: "not a column of the table (projection)",
            });
        }
    }
    // Validate ORDER BY columns.
    let mut order_by = Vec::with_capacity(spec.order_by.len());
    for (col, desc) in &spec.order_by {
        if table.column(col).is_none() {
            return Err(SqlError::UnknownColumn {
                name: col.clone(),
                reason: "not a column of the table (ORDER BY)",
            });
        }
        order_by.push(OrderTerm {
            col: col.clone(),
            desc: *desc,
        });
    }

    // Lower the WHERE: build the compiled predicate + the bound params, and accumulate the
    // residual for anything that cannot be faithfully rendered.
    let mut params: Vec<Param> = Vec::new();
    let mut residual: Option<Predicate> = None;
    let where_ = match &spec.predicate {
        None => None,
        Some(p) => lower_predicate(p, table, &mut params, &mut residual),
    };

    // A pushed projection (`SELECT <cols>`) narrows the row shape, but the engine still RE-APPLIES
    // the residual predicate over the fetched rows — so the SELECT must keep every column the
    // residual reads (`projected ⊇ residual columns`), or those rows would be missing the columns to
    // re-filter on. Expand the projection to include any residual top-level column not already in it;
    // the caller (facet) narrows back to the requested projection AFTER the residual re-filter. A
    // `SELECT *` (empty projection) already carries every column, so it is left untouched.
    let projection = if spec.projection.is_empty() {
        Vec::new()
    } else {
        let mut projection = spec.projection.clone();
        if let Some(res) = &residual {
            let mut needed = Vec::new();
            residual_columns(res, &mut needed);
            for col in needed {
                if !projection.contains(&col) {
                    projection.push(col);
                }
            }
        }
        projection
    };

    let plan = SelectPlan {
        schema: schema.to_string(),
        table: table.name.clone(),
        projection,
        where_,
        order_by,
        // Push `LIMIT` into the SQL **only** when the whole `WHERE` pushed down (no residual). With
        // a residual, the engine re-filters after the fetch, so a native `LIMIT n` would cap the
        // fetch *before* that filter and under-fetch (drop rows that survive the local re-check).
        // When a residual remains the caller applies the `LIMIT` after re-filtering instead.
        limit: if residual.is_some() { None } else { spec.limit },
        params,
    };

    Ok(CompileResult { plan, residual })
}

/// Fold a residual predicate `p` into the accumulator (conjoining with any prior residual).
fn add_residual(residual: &mut Option<Predicate>, p: Predicate) {
    *residual = Some(match residual.take() {
        None => p,
        Some(prev) => Predicate::And(Box::new(prev), Box::new(p)),
    });
}

/// Lower a typed predicate into a compiled [`SqlPredicate`] (pushing bound params) and a residual.
/// Returns `Some(compiled)` for the part that pushes down exactly; anything non-faithful is added
/// to `residual` and contributes nothing to the compiled predicate.
fn lower_predicate(
    p: &Predicate,
    table: &TableCatalog,
    params: &mut Vec<Param>,
    residual: &mut Option<Predicate>,
) -> Option<SqlPredicate> {
    match p {
        // A conjunction lowers each side independently; the compiled parts AND together, and each
        // side's residual is accumulated separately (so `pushable AND unpushable` pushes the
        // pushable half and keeps the other as residual — the over-fetch-then-filter shape).
        Predicate::And(a, b) => {
            let left = lower_predicate(a, table, params, residual);
            let right = lower_predicate(b, table, params, residual);
            match (left, right) {
                (Some(l), Some(r)) => Some(SqlPredicate::And(Box::new(l), Box::new(r))),
                (Some(only), None) | (None, Some(only)) => Some(only),
                (None, None) => None,
            }
        }
        Predicate::Cmp(col, op, lit) => lower_cmp(p, col, *op, lit, table, params, residual),
        Predicate::In(col, set) => lower_in(p, col, set, table, params, residual),
        Predicate::Between(col, low, high) => {
            lower_between(p, col, low, high, table, params, residual)
        }
        // LIKE (glob semantics differ from SQL LIKE), OR / NOT (cannot guarantee a faithful single
        // SQL predicate combined with an AND-of-leaves shape), and any other form stay WHOLLY
        // residual — correctness over completeness.
        other => {
            add_residual(residual, other.clone());
            None
        }
    }
}

/// Lower a single comparison `col op lit`. `~` (regex) is NOT portable across the three dialects,
/// so it is kept residual; the other operators are exact and push down.
fn lower_cmp(
    original: &Predicate,
    col: &ColRef,
    op: CmpOp,
    lit: &Literal,
    table: &TableCatalog,
    params: &mut Vec<Param>,
    residual: &mut Option<Predicate>,
) -> Option<SqlPredicate> {
    let Some(name) = bare_col(col) else {
        add_residual(residual, original.clone());
        return None;
    };
    if table.column(name).is_none() {
        add_residual(residual, original.clone());
        return None;
    }
    // `~` regex has no portable, identical SQL form across pg/mysql/sqlite — keep it residual.
    if op == CmpOp::Match {
        add_residual(residual, original.clone());
        return None;
    }
    let param = params.len();
    params.push(Param::from_literal(lit));
    Some(SqlPredicate::Cmp {
        col: name.to_string(),
        op,
        param,
    })
}

/// Lower `col IN (set)` — exact membership, pushes down.
fn lower_in(
    original: &Predicate,
    col: &ColRef,
    set: &[Literal],
    table: &TableCatalog,
    params: &mut Vec<Param>,
    residual: &mut Option<Predicate>,
) -> Option<SqlPredicate> {
    let Some(name) = bare_col(col) else {
        add_residual(residual, original.clone());
        return None;
    };
    if table.column(name).is_none() || set.is_empty() {
        // An empty IN-list (`IN ()`) is not portable; keep it residual (the engine evaluates it).
        add_residual(residual, original.clone());
        return None;
    }
    let indices = set
        .iter()
        .map(|lit| {
            let idx = params.len();
            params.push(Param::from_literal(lit));
            idx
        })
        .collect();
    Some(SqlPredicate::InList {
        col: name.to_string(),
        params: indices,
    })
}

/// Lower `col BETWEEN low AND high` — exact range, pushes down.
fn lower_between(
    original: &Predicate,
    col: &ColRef,
    low: &Literal,
    high: &Literal,
    table: &TableCatalog,
    params: &mut Vec<Param>,
    residual: &mut Option<Predicate>,
) -> Option<SqlPredicate> {
    let Some(name) = bare_col(col) else {
        add_residual(residual, original.clone());
        return None;
    };
    if table.column(name).is_none() {
        add_residual(residual, original.clone());
        return None;
    }
    let low_idx = params.len();
    params.push(Param::from_literal(low));
    let high_idx = params.len();
    params.push(Param::from_literal(high));
    Some(SqlPredicate::Between {
        col: name.to_string(),
        low: low_idx,
        high: high_idx,
    })
}

/// The single-segment column name of a [`ColRef`], if it is a bare column (not a dotted path). A
/// dotted path (`a.b.c`) into a nested column is not a flat SQL column here — kept residual.
fn bare_col(col: &ColRef) -> Option<&str> {
    match col.path.as_slice() {
        [one] => Some(one.as_str()),
        _ => None,
    }
}

/// Collect the **top-level** SQL column names a residual predicate reads (deduped, in encounter
/// order), so a pushed projection can keep them in the SELECT. For a dotted path (`a.b`) the
/// top-level column `a` is the SQL column the residual indexes into locally. The match is
/// exhaustive on purpose: a new [`Predicate`] variant must update this, or the projection could
/// silently drop a column the residual needs (wrong rows).
fn residual_columns(p: &Predicate, out: &mut Vec<String>) {
    match p {
        Predicate::Cmp(col, _, _)
        | Predicate::In(col, _)
        | Predicate::Between(col, _, _)
        | Predicate::Like(col, _) => {
            if let Some(top) = col.path.first() {
                let name = top.as_str().to_string();
                if !out.contains(&name) {
                    out.push(name);
                }
            }
        }
        Predicate::And(a, b) | Predicate::Or(a, b) => {
            residual_columns(a, out);
            residual_columns(b, out);
        }
        Predicate::Not(inner) => residual_columns(inner, out),
    }
}
