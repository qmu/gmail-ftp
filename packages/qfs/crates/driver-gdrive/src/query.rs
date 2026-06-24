//! WHERE → Drive `q` search pushdown (RFD-0001 §6 "push down what the backend runs natively;
//! combine residual filters locally").
//!
//! Drive's `files.list` accepts a `q` parameter in Drive's query syntax (`name = 'x'`,
//! `name contains 'x'`, `mimeType = 'x'`, `'<id>' in parents`, `fullText contains 'x'`,
//! `modifiedTime > '<rfc3339>'`, `trashed = false`, …). The planner lowers a typed `WHERE`
//! predicate into this query for the subset Drive covers; predicates Drive cannot express stay
//! **residual** for the engine to filter locally. This module is the pure translation — it
//! builds the `q` string and reports the residual; it performs **no I/O** and holds no token.
//!
//! ## The t20 lesson — TRUTHFUL residual (the headline invariant)
//! A Drive operator is pushed as a residual-dropping term **only** when it means *exactly* the
//! SQL predicate. Otherwise it is pushed as a cheap **pre-filter** and the exact predicate is
//! **kept as residual** so the engine re-applies exact filtering locally (over-fetch then
//! filter — never wrong rows). The mapping splits into:
//!
//! **Exact** (term ≡ predicate, residual dropped):
//! - `name = 'x'`        → `name = 'x'`        (Drive `name =` is exact-string equality)
//! - `mime_type = 'x'`   → `mimeType = 'x'`    (exact MIME equality)
//! - `trashed = b`       → `trashed = <b>`     (exact trash-flag membership)
//! - a parent scope (`'<parentId>' in parents`) → exact parent membership
//!
//! **Pre-filter (lossy, predicate KEPT as residual):**
//! - `name LIKE 'p'` / `name ~ 'p'` → `name contains 'p'` (Drive `contains` is a token/substring
//!   match, looser than `LIKE`/regex — over-fetch, re-check locally).
//! - `fullText`/`text = 'p'`        → `fullText contains 'p'` (loose full-text — re-check).
//! - `modified_time > <ms>`         → `modifiedTime > '<rfc3339>'` (second-granular RFC-3339,
//!   looser than the exact ms comparison — re-check).
//!
//! `OR`/`NOT`/`IN`/`BETWEEN`/unsupported columns push nothing and stay wholly **residual**.

use qfs_types::{CmpOp, ColRef, Literal, Predicate};

/// The pushed-down Drive `q` string and the residual predicate the engine still filters locally
/// (RFD §6). `query` is empty when nothing pushed down; `residual` is `None` when the whole
/// predicate pushed down with an exact mapping.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct PushdownResult {
    /// The Drive `q` search string (terms joined with ` and `; empty if none pushed down).
    pub query: String,
    /// The predicate the backend could **not** express exactly — the engine filters this locally.
    pub residual: Option<Predicate>,
}

/// Build the Drive `q` string for a `WHERE` predicate, scoped to an optional `parent` folder id
/// (a `/drive/...` folder scan contributes an exact `'<parent>' in parents` term). Returns the
/// pushed query + the residual.
///
/// The translation is conservative about **correctness**, not about what it pushes: a term is
/// pushed whenever Drive can pre-filter on it, but a conjunct is dropped from the residual
/// **only** when its Drive term means *exactly* the SQL predicate (the Exact class). Every lossy
/// term (`contains`, `fullText contains`, the `modifiedTime` bound) keeps its original predicate
/// as residual so the engine re-applies exact filtering locally (RFD §6 over-fetch-then-filter).
#[must_use]
pub fn build_query(parent: Option<&str>, predicate: Option<&Predicate>) -> PushdownResult {
    let mut terms: Vec<String> = Vec::new();
    if let Some(parent) = parent {
        terms.push(format!("'{}' in parents", escape(parent)));
    }
    let residual = match predicate {
        None => None,
        Some(p) => lower(p, &mut terms),
    };
    PushdownResult {
        query: terms.join(" and "),
        residual,
    }
}

/// The outcome of lowering one comparison/`LIKE` into a Drive query term.
enum Lowered {
    /// The Drive term means *exactly* the SQL predicate — push it and drop the predicate from
    /// the residual.
    Exact(String),
    /// The Drive term is *looser* than the SQL predicate — push it as a backend pre-filter but
    /// keep the original predicate as residual so the engine re-applies exact filtering locally.
    PreFilter(String),
}

/// Lower one predicate, appending its pushed terms to `terms` and returning the residual.
/// A conjunction pushes each conjunct independently; an exact conjunct drops out of the residual
/// while a lossy conjunct stays in it (pushed *and* kept). Any other shape stays wholly residual.
fn lower(p: &Predicate, terms: &mut Vec<String>) -> Option<Predicate> {
    match p {
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
            Some(Lowered::Exact(term)) => {
                terms.push(term);
                None
            }
            // Lossy mapping: push the pre-filter term BUT keep the exact predicate as residual
            // so the engine re-checks it — over-fetch then filter, never wrong rows (RFD §6).
            Some(Lowered::PreFilter(term)) => {
                terms.push(term);
                Some(p.clone())
            }
            None => Some(p.clone()),
        },
        Predicate::Like(col, pattern) => match field_of(col) {
            // `LIKE` has no Drive operator; `contains` is a loose token/substring pre-filter, so
            // push it and KEEP the `LIKE` predicate as residual for exact local re-matching.
            Some("name") => {
                terms.push(format!("name contains '{}'", escape(&pattern.0)));
                Some(p.clone())
            }
            Some("text" | "full_text" | "fullText") => {
                terms.push(format!("fullText contains '{}'", escape(&pattern.0)));
                Some(p.clone())
            }
            _ => Some(p.clone()),
        },
        // OR / NOT / IN / BETWEEN — Drive's `and`-joined terms cannot express these cleanly, so
        // they stay residual and the engine filters locally (correctness over completeness).
        other => Some(other.clone()),
    }
}

/// Lower a single comparison into a Drive query term, tagged [`Lowered::Exact`] when the term
/// means exactly the predicate or [`Lowered::PreFilter`] when it is a looser pre-filter, or
/// `None` if Drive cannot express it.
fn lower_cmp(col: &ColRef, op: CmpOp, lit: &Literal) -> Option<Lowered> {
    let field = field_of(col)?;
    match (field, op, lit) {
        // `name = 'x'` is exact string equality in Drive's query language — fully pushed.
        ("name", CmpOp::Eq, Literal::Text(v)) => {
            Some(Lowered::Exact(format!("name = '{}'", escape(v))))
        }
        // `name ~ 'p'` (regex-match) has no exact Drive operator; `contains` is a loose
        // pre-filter — push it, keep the predicate as residual.
        ("name", CmpOp::Match, Literal::Text(v)) => {
            Some(Lowered::PreFilter(format!("name contains '{}'", escape(v))))
        }
        // `mime_type = 'x'` → exact `mimeType =` equality.
        ("mime_type", CmpOp::Eq, Literal::Text(v)) => {
            Some(Lowered::Exact(format!("mimeType = '{}'", escape(v))))
        }
        // `trashed = b` → exact `trashed = <b>`.
        ("trashed", CmpOp::Eq, Literal::Bool(b)) => Some(Lowered::Exact(format!("trashed = {b}"))),
        // A parent-id membership scopes to exactly that parent (`'<id>' in parents` is exact).
        ("parent", CmpOp::Eq, Literal::Text(v)) => {
            Some(Lowered::Exact(format!("'{}' in parents", escape(v))))
        }
        // Full-text equality maps to the loose `fullText contains` — pre-filter, keep residual.
        ("text" | "full_text" | "fullText", CmpOp::Eq, Literal::Text(v)) => Some(
            Lowered::PreFilter(format!("fullText contains '{}'", escape(v))),
        ),
        // modified_time range → modifiedTime bound. Drive's modifiedTime is RFC-3339 / second-
        // granular and the ms→s truncation drops sub-second precision, so the bound is
        // approximate: pre-filter only, the engine re-checks the exact ms comparison.
        ("modified_time", CmpOp::Gt | CmpOp::Ge, Literal::Int(ms)) => Some(Lowered::PreFilter(
            format!("modifiedTime > '{}'", rfc3339_from_ms(*ms)),
        )),
        ("modified_time", CmpOp::Lt | CmpOp::Le, Literal::Int(ms)) => Some(Lowered::PreFilter(
            format!("modifiedTime < '{}'", rfc3339_from_ms(*ms)),
        )),
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

/// Escape a Drive `q` literal value: Drive single-quoted literals escape `\` and `'`. Owned,
/// dependency-free.
fn escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('\'', "\\'")
}

/// Render an epoch-milliseconds instant as a second-granular RFC-3339 UTC timestamp
/// (`YYYY-MM-DDThh:mm:ssZ`) for the Drive `modifiedTime` bound. Dependency-free civil-date math
/// (the classic days-from-epoch algorithm); the ms→s truncation is exactly why the bound is a
/// lossy pre-filter that keeps its residual.
fn rfc3339_from_ms(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (hour, minute, second) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let (year, month, day) = civil_from_days(days);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

/// Convert a count of days since the Unix epoch into a `(year, month, day)` civil date
/// (Howard Hinnant's algorithm). Pure integer math, no external dep.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}
