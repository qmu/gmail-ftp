//! WHERE → Slack `conversations.history` query-param pushdown (RFD-0001 §6 "push down what the
//! backend runs natively; combine residual filters locally").
//!
//! ## Scoped to `oldest`/`latest`/`limit` (the ticket scopes richer pushdown to E3)
//! `conversations.history`/`.replies` accept a **time window** + a page size as native params:
//! `oldest`/`latest` bound the message `ts` range and `limit` caps the page. The planner lowers a
//! typed `WHERE` on the `ts` column into these params; anything Slack cannot express stays
//! **residual** for the engine to filter locally.
//!
//! ## The t20 lesson — TRUTHFUL residual (the headline invariant)
//! Slack's `oldest`/`latest` window params are **inclusive** bounds on `ts`. So the truthfulness of
//! dropping the residual depends on the comparison operator:
//! - `ts >= X` → `oldest=X` and `ts <= X` → `latest=X` are **exact** (inclusive ≡ inclusive), so
//!   the conjunct **drops** from the residual.
//! - `ts >  X` → `oldest=X` and `ts <  X` → `latest=X` are a **lossy pre-filter**: Slack's
//!   inclusive bound also returns the `ts == X` boundary row, so the **strict** comparison is
//!   **kept as residual** to re-exclude that one row locally. Dropping it here would over-return a
//!   wrong row — exactly the t20 defect.
//!
//! Any other predicate (`text LIKE`, `user =`, `OR`, …) Slack cannot push, so it is **kept**
//! residual and the engine re-filters locally — over-fetch then filter, never wrong rows. This
//! module is the pure translation: it builds the param list + the residual; it performs **no I/O**
//! and holds no token.

use qfs_types::{CmpOp, ColRef, Literal, Predicate};

/// The pushed-down Slack query params and the residual predicate the engine still filters locally
/// (RFD §6). `params` is empty when nothing pushed down; `residual` is `None` when the whole
/// predicate pushed down exactly.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct PushdownResult {
    /// The `(key, value)` query params to add to the request (stable order).
    pub params: Vec<(String, String)>,
    /// The predicate the backend could **not** express — the engine filters this locally.
    pub residual: Option<Predicate>,
}

/// Build the Slack history query params for a `WHERE` predicate on a message log. Returns the
/// pushed params (`oldest`/`latest`) + the residual.
///
/// Only the message-log nodes (messages/replies/dms) push a time window; the relational `users`
/// directory and the blob `files` namespace push nothing here (their predicate stays wholly
/// residual — correctness over completeness).
#[must_use]
pub fn build_params(predicate: Option<&Predicate>) -> PushdownResult {
    let mut params: Vec<(String, String)> = Vec::new();
    let residual = match predicate {
        None => None,
        Some(p) => lower(p, &mut params),
    };
    PushdownResult { params, residual }
}

/// Lower one predicate, appending its pushed params to `params` and returning the residual. A
/// conjunction pushes each conjunct independently; an exact `ts`-boundary conjunct drops out of the
/// residual while every other conjunct stays in it. Any non-`AND` shape stays wholly residual.
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
            // An INCLUSIVE boundary (`>=`/`<=`) means *exactly* the comparison — push it, drop the
            // predicate.
            Some(Lowered::Exact(param)) => {
                params.push(param);
                None
            }
            // A STRICT boundary (`>`/`<`) is lowered to Slack's INCLUSIVE `oldest`/`latest`, which
            // over-returns the boundary row itself — so the strict comparison is KEPT as an exact
            // residual that re-excludes that one boundary row locally (the t20 lesson — over-fetch
            // then filter, never wrong rows).
            Some(Lowered::PreFilter(param)) => {
                params.push(param);
                Some(p.clone())
            }
            None => Some(p.clone()),
        },
        // OR / NOT / IN / BETWEEN / LIKE — Slack's window params cannot express these, so they
        // stay residual and the engine filters locally (correctness over completeness).
        other => Some(other.clone()),
    }
}

/// The outcome of lowering one `ts` comparison into a Slack window param.
enum Lowered {
    /// The param means *exactly* the SQL predicate (an inclusive `>=`/`<=` boundary) — push it and
    /// drop the predicate.
    Exact((String, String)),
    /// The param is *looser* than the SQL predicate (a strict `>`/`<` lowered to Slack's inclusive
    /// boundary) — push it as a pre-filter but keep the original strict comparison as residual so
    /// the engine re-excludes the boundary row locally.
    PreFilter((String, String)),
}

/// Lower a single comparison on `ts` into a Slack window param, or `None` if Slack cannot express
/// it. Slack's `oldest`/`latest` are **inclusive** bounds, so:
/// - `ts >= X` → `oldest=X` (Exact — inclusive matches inclusive);
/// - `ts <= X` → `latest=X` (Exact);
/// - `ts >  X` → `oldest=X` (PreFilter — Slack would also return the `ts == X` row, so the strict
///   `>` is kept residual to re-exclude it);
/// - `ts <  X` → `latest=X` (PreFilter — symmetric).
fn lower_cmp(col: &ColRef, op: CmpOp, lit: &Literal) -> Option<Lowered> {
    let field = field_of(col)?;
    if field != "ts" {
        return None;
    }
    let value = match lit {
        Literal::Text(v) => v.clone(),
        Literal::Int(v) => v.to_string(),
        _ => return None,
    };
    match op {
        CmpOp::Ge => Some(Lowered::Exact(("oldest".to_string(), value))),
        CmpOp::Le => Some(Lowered::Exact(("latest".to_string(), value))),
        CmpOp::Gt => Some(Lowered::PreFilter(("oldest".to_string(), value))),
        CmpOp::Lt => Some(Lowered::PreFilter(("latest".to_string(), value))),
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
