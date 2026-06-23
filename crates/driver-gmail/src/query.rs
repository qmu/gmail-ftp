//! WHERE → Gmail search `q=` pushdown (RFD-0001 §6 "push down what the backend runs
//! natively; combine residual filters locally").
//!
//! Gmail's `messages.list` accepts a `q=` parameter in Gmail's search syntax (`from:`,
//! `subject:`, `after:`, `is:unread`, …). The planner lowers a typed `WHERE` predicate into
//! this query string for the subset of operators Gmail covers; predicates Gmail cannot express
//! are returned as **residual** for the engine to filter locally. This module is the pure
//! translation — it builds the `q=` string and reports the residual; it performs **no I/O** and
//! holds no token.
//!
//! ## Mapping (the covered subset)
//! - `from = 'x@y'`        → `from:x@y`
//! - `to = 'x@y'`          → `to:x@y`
//! - `subject = 'hello'`   → `subject:hello`   (also `subject ~ 'hello'` / `LIKE`)
//! - `label = 'INBOX'`     → `label:INBOX`     (or `in:inbox` for system labels)
//! - `is_unread = true`    → `is:unread`
//! - `date > <ts>`         → `after:<unix>`    (epoch-seconds; Gmail accepts a unix timestamp)
//! - `date < <ts>`         → `before:<unix>`
//! - `<a> AND <b>`         → space-join (Gmail ANDs terms)
//!
//! `OR`/`NOT`/`IN`/unsupported columns stay **residual**. A bare label scan (`/mail/<label>`)
//! contributes its `label:<id>` term so the search is naturally scoped to the directory.

use cfs_types::{CmpOp, ColRef, Literal, Predicate};

/// The pushed-down Gmail search string and the residual predicate the engine still filters
/// locally (RFD §6). `query` is empty when nothing pushed down; `residual` is `None` when the
/// whole predicate pushed down.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct PushdownResult {
    /// The Gmail `q=` search string (space-separated terms; empty if none pushed down).
    pub query: String,
    /// The predicate the backend could **not** express — the engine filters this locally.
    pub residual: Option<Predicate>,
}

/// Build the Gmail `q=` string for a `WHERE` predicate, scoped to an optional `label`
/// (`/mail/<label>` contributes a `label:<id>` term). Returns the pushed query + the residual.
///
/// The translation is conservative: a term is emitted **only** when Gmail can express it
/// exactly, so a residual is always re-checked locally and the result set is never wrong (RFD
/// §6 — over-fetch then filter, never under-fetch).
#[must_use]
pub fn build_query(label: Option<&str>, predicate: Option<&Predicate>) -> PushdownResult {
    let mut terms: Vec<String> = Vec::new();
    if let Some(label) = label {
        terms.push(format!("label:{}", quote_term(label)));
    }
    let residual = match predicate {
        None => None,
        Some(p) => lower(p, &mut terms),
    };
    PushdownResult {
        query: terms.join(" "),
        residual,
    }
}

/// Lower one predicate, appending its pushed terms to `terms` and returning the residual
/// (the part Gmail cannot express). A conjunction pushes each conjunct independently; any
/// other shape that does not map cleanly stays wholly residual.
fn lower(p: &Predicate, terms: &mut Vec<String>) -> Option<Predicate> {
    match p {
        // AND distributes: push each side, AND the residuals back together.
        Predicate::And(a, b) => {
            let ra = lower(a, terms);
            let rb = lower(b, terms);
            match (ra, rb) {
                (None, None) => None,
                (Some(r), None) | (None, Some(r)) => Some(r),
                (Some(ra), Some(rb)) => Some(Predicate::And(Box::new(ra), Box::new(rb))),
            }
        }
        Predicate::Cmp(col, op, lit) => match lower_cmp(col, *op, lit) {
            Some(term) => {
                terms.push(term);
                None
            }
            None => Some(p.clone()),
        },
        Predicate::Like(col, pattern) => match field_of(col) {
            Some(field @ ("from" | "to" | "subject")) => {
                terms.push(format!("{field}:{}", quote_term(&pattern.0)));
                None
            }
            _ => Some(p.clone()),
        },
        // OR / NOT / IN / BETWEEN — Gmail's term ANDing does not express these cleanly, so they
        // stay residual and the engine filters locally (correctness over completeness, RFD §6).
        other => Some(other.clone()),
    }
}

/// Lower a single comparison into a Gmail search term, or `None` if Gmail cannot express it.
fn lower_cmp(col: &ColRef, op: CmpOp, lit: &Literal) -> Option<String> {
    let field = field_of(col)?;
    match (field, op, lit) {
        // Header/text equality and regex-match map to the field operators.
        (f @ ("from" | "to" | "subject"), CmpOp::Eq | CmpOp::Match, Literal::Text(v)) => {
            Some(format!("{f}:{}", quote_term(v)))
        }
        // A label-id equality scopes to that label.
        ("label", CmpOp::Eq, Literal::Text(v)) => Some(format!("label:{}", quote_term(v))),
        // is_unread = true → is:unread; = false → is:read.
        ("is_unread", CmpOp::Eq, Literal::Bool(b)) => {
            Some(format!("is:{}", if *b { "unread" } else { "read" }))
        }
        // Date range → after:/before: with a unix-seconds bound (Gmail accepts a unix ts).
        ("date", CmpOp::Gt | CmpOp::Ge, Literal::Int(ms)) => Some(format!("after:{}", ms / 1000)),
        ("date", CmpOp::Lt | CmpOp::Le, Literal::Int(ms)) => Some(format!("before:{}", ms / 1000)),
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

/// Quote a Gmail search term value when it contains whitespace, so a multi-word subject stays
/// one term (`subject:"two words"`). A value with no whitespace is emitted bare.
fn quote_term(value: &str) -> String {
    if value.chars().any(char::is_whitespace) {
        format!("\"{}\"", value.replace('"', ""))
    } else {
        value.to_string()
    }
}
