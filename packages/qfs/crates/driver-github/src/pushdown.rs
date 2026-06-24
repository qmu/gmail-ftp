//! WHERE → GitHub REST query-param pushdown (RFD-0001 §6 "push down what the backend runs
//! natively; combine residual filters locally").
//!
//! GitHub's `GET /issues` (and `/pulls`) accept server-side filters as query params: `state`,
//! `labels` (a comma-joined AND set), `assignee`. The planner lowers a typed `WHERE` predicate
//! into these params for the subset GitHub covers; predicates GitHub cannot express exactly stay
//! **residual** for the engine to filter locally.
//!
//! ## The t20 lesson — TRUTHFUL residual (the headline invariant)
//! A GitHub query param is pushed as a residual-dropping **exact** mapping **only** when the
//! param means *exactly* the SQL predicate. Otherwise it is pushed as a cheap **pre-filter** and
//! the exact predicate is **kept as residual** so the engine re-applies exact filtering locally
//! (over-fetch then filter — never wrong rows). The mapping:
//!
//! **Exact** (param ≡ predicate, residual dropped):
//! - `state = 'open' | 'closed'`  → `state=<v>`   (GitHub `state` is exact equality).
//! - `assignee = '<login>'`       → `assignee=<v>` (exact single-assignee match).
//!
//! **Pre-filter (lossy, predicate KEPT as residual):**
//! - `label = '<name>'` / `labels = '<name>'` → `labels=<name>`. GitHub `labels` is a *set
//!   membership* filter (the issue carries that label among possibly others); a SQL `=` against
//!   the scalar `label` column re-checked locally over the fetched `labels` array stays exact.
//!   The param narrows the fetch; the residual re-checks membership precisely.
//!
//! Everything else (`OR`/`NOT`/`IN`/`BETWEEN`/`~`/other columns) pushes nothing and stays wholly
//! **residual**. This module is the pure translation — it builds the param list and reports the
//! residual; it performs **no I/O** and holds no token.

use qfs_types::{CmpOp, ColRef, Literal, Predicate};

/// The pushed-down GitHub query params and the residual predicate the engine still filters
/// locally (RFD §6). `params` is empty when nothing pushed down; `residual` is `None` when the
/// whole predicate pushed down with an exact mapping.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct PushdownResult {
    /// The `(key, value)` query params to add to the list URL (stable order).
    pub params: Vec<(String, String)>,
    /// The predicate the backend could **not** express exactly — the engine filters this locally.
    pub residual: Option<Predicate>,
}

/// Build the GitHub list query params for a `WHERE` predicate. Returns the pushed params + the
/// residual.
///
/// The translation is conservative about **correctness**, not about what it pushes: a param is
/// pushed whenever GitHub can pre-filter on it, but a conjunct is dropped from the residual
/// **only** when its param means *exactly* the SQL predicate (the Exact class). Every lossy term
/// keeps its original predicate as residual so the engine re-applies exact filtering locally
/// (RFD §6 over-fetch-then-filter).
#[must_use]
pub fn build_params(predicate: Option<&Predicate>) -> PushdownResult {
    let mut params: Vec<(String, String)> = Vec::new();
    let residual = match predicate {
        None => None,
        Some(p) => lower(p, &mut params),
    };
    PushdownResult { params, residual }
}

/// The outcome of lowering one comparison into a GitHub query param.
enum Lowered {
    /// The param means *exactly* the SQL predicate — push it and drop the predicate.
    Exact((String, String)),
    /// The param is *looser* than the SQL predicate — push it as a pre-filter but keep the
    /// original predicate as residual so the engine re-applies exact filtering locally.
    PreFilter((String, String)),
}

/// Lower one predicate, appending its pushed params to `params` and returning the residual.
/// A conjunction pushes each conjunct independently; an exact conjunct drops out of the residual
/// while a lossy conjunct stays in it (pushed *and* kept). Any other shape stays wholly residual.
fn lower(p: &Predicate, params: &mut Vec<(String, String)>) -> Option<Predicate> {
    match p {
        Predicate::And(a, b) => {
            let ra = lower(a, params);
            let rb = lower(b, params);
            match (ra, rb) {
                (None, None) => None,
                (Some(r), None) | (None, Some(r)) => Some(r),
                (Some(ra), Some(rb)) => Some(Predicate::And(Box::new(ra), Box::new(rb))),
            }
        }
        Predicate::Cmp(col, op, lit) => match lower_cmp(col, *op, lit) {
            Some(Lowered::Exact(param)) => {
                params.push(param);
                None
            }
            // Lossy mapping: push the pre-filter param BUT keep the exact predicate as residual
            // so the engine re-checks it — over-fetch then filter, never wrong rows (RFD §6).
            Some(Lowered::PreFilter(param)) => {
                params.push(param);
                Some(p.clone())
            }
            None => Some(p.clone()),
        },
        // OR / NOT / IN / BETWEEN / LIKE — GitHub's AND-only param set cannot express these, so
        // they stay residual and the engine filters locally (correctness over completeness).
        other => Some(other.clone()),
    }
}

/// Lower a single comparison into a GitHub query param, tagged [`Lowered::Exact`] when the param
/// means exactly the predicate or [`Lowered::PreFilter`] when it is a looser pre-filter, or
/// `None` if GitHub cannot express it.
fn lower_cmp(col: &ColRef, op: CmpOp, lit: &Literal) -> Option<Lowered> {
    let field = field_of(col)?;
    match (field, op, lit) {
        // `state = 'open'|'closed'` is exact equality against the GitHub `state` filter.
        ("state", CmpOp::Eq, Literal::Text(v)) => {
            Some(Lowered::Exact(("state".to_string(), v.clone())))
        }
        // `assignee = '<login>'` is an exact single-assignee filter.
        ("assignee", CmpOp::Eq, Literal::Text(v)) => {
            Some(Lowered::Exact(("assignee".to_string(), v.clone())))
        }
        // `label`/`labels = '<name>'` → the `labels` set-membership pre-filter. GitHub returns
        // issues carrying that label (among others), so the param narrows the fetch but the SQL
        // `=` is re-checked locally over the `labels` array — pre-filter, keep residual.
        ("label" | "labels", CmpOp::Eq, Literal::Text(v)) => {
            Some(Lowered::PreFilter(("labels".to_string(), v.clone())))
        }
        _ => None,
    }
}

/// The single-segment column name of a [`ColRef`], if it is a bare column (not a dotted path).
fn field_of(col: &ColRef) -> Option<&str> {
    match col.path.as_slice() {
        [one] => Some(one.as_str()),
        _ => None,
    }
}
