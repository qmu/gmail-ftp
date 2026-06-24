//! The **pure WHERE-guard evaluator** (t34, CO-t31-4): evaluate a trigger's optional
//! `WHERE <pred>` over the event's `NEW.*` row. PURE — no I/O, no mutation, wasm-portable.
//!
//! The guard is stored as a `StatementSpec` (a `Statement::Query` wrapping the predicate over an
//! empty `VALUES` source). This module:
//!   1. extracts the single `WHERE` op's [`Expr`] from the rehydrated statement,
//!   2. lowers it to a typed [`qfs_core::Predicate`] via [`qfs_pushdown::lower_predicate`]
//!      (the same lowering the read planner uses — no bespoke predicate language), and
//!   3. evaluates the typed predicate over `NEW.*` with a total evaluator (an incomparable /
//!      missing column ⇒ the row does not match, never a panic — mirroring the engine's residual
//!      kernel so a guard cannot crash dispatch).
//!
//! An ABSENT guard (empty predicate spec) is "always fire". A guard that fails to rehydrate /
//! lower / is not a predicate shape is treated as **non-matching** (fail-closed: a malformed guard
//! never fires the handler) and surfaced to the caller as an error so it can be logged.

use std::cmp::Ordering;

use qfs_core::{CmpOp, ColRef, Literal, Pattern, Predicate, Row, Value};
use qfs_parser::{PipeOp, Statement};

/// A structured, secret-free guard error (a malformed WHERE never fires — fail-closed).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum GuardError {
    /// The rehydrated guard statement was not the expected `Query |> WHERE` shape.
    #[error("trigger WHERE guard is not a predicate query")]
    NotAPredicate,
    /// The predicate expression could not be lowered to a typed predicate.
    #[error("trigger WHERE guard could not be lowered: {0}")]
    Lower(String),
}

/// Evaluate the (already-rehydrated) guard statement over the `new` row + its column names.
/// Returns `Ok(true)` if the row matches (the handler should fire), `Ok(false)` if it does not.
/// A malformed guard is a fail-closed [`GuardError`] (the caller logs it and does NOT fire).
///
/// `columns` is the positional name list for `new.values` (the `NEW.*` field names), so a
/// `NEW.priority` reference resolves to the right value.
///
/// # Errors
/// [`GuardError`] if the guard is not a predicate query or cannot be lowered.
pub fn guard_matches(guard: &Statement, columns: &[String], new: &Row) -> Result<bool, GuardError> {
    let expr = match guard {
        Statement::Query(p) => match p.ops.first() {
            Some(PipeOp::Where(e)) => e,
            _ => return Err(GuardError::NotAPredicate),
        },
        _ => return Err(GuardError::NotAPredicate),
    };
    let predicate =
        qfs_pushdown::lower_predicate(expr).map_err(|e| GuardError::Lower(e.to_string()))?;
    Ok(eval_predicate(&predicate, columns, new))
}

/// A total predicate evaluator over a positionally-named row. Mirrors the engine's residual
/// kernel: a comparison whose operands are not comparable evaluates to `false` (the row does not
/// match) rather than panicking. Lives here (not reused from `qfs-engine`, where it is
/// `pub(crate)`) so the pure watchtower core stays a small wasm-portable leaf.
fn eval_predicate(p: &Predicate, columns: &[String], row: &Row) -> bool {
    match p {
        Predicate::And(a, b) => eval_predicate(a, columns, row) && eval_predicate(b, columns, row),
        Predicate::Or(a, b) => eval_predicate(a, columns, row) || eval_predicate(b, columns, row),
        Predicate::Not(inner) => !eval_predicate(inner, columns, row),
        Predicate::Cmp(col, op, lit) => match resolve(col, columns, row) {
            Some(v) => cmp(&v, *op, lit),
            None => false,
        },
        Predicate::In(col, set) => match resolve(col, columns, row) {
            Some(v) => set.iter().any(|lit| cmp(&v, CmpOp::Eq, lit)),
            None => false,
        },
        Predicate::Between(col, low, high) => match resolve(col, columns, row) {
            Some(v) => cmp(&v, CmpOp::Ge, low) && cmp(&v, CmpOp::Le, high),
            None => false,
        },
        Predicate::Like(col, pattern) => match resolve(col, columns, row) {
            Some(Value::Text(s)) => like_match(&s, pattern),
            _ => false,
        },
    }
}

/// Resolve a [`ColRef`] to the row's value. A `NEW.<col>` path (a leading `NEW` segment) is the
/// canonical guard form — the leading `NEW` is stripped so `<col>` resolves against the event row's
/// field names; a bare `<col>` (no `NEW` prefix) also resolves directly. The remaining segments
/// after the column are struct navigation. Missing / unnavigable ⇒ `None`.
fn resolve(col: &ColRef, columns: &[String], row: &Row) -> Option<Value> {
    // Strip a leading `NEW.` so `WHERE NEW.body …` resolves `body` against the NEW.* field names.
    let path: &[qfs_core::Name] = match col.path.split_first() {
        Some((head, rest)) if head == "NEW" && !rest.is_empty() => rest,
        _ => &col.path,
    };
    let (head, rest) = path.split_first()?;
    let idx = columns.iter().position(|c| c == head.as_str())?;
    let mut cur = row.values.get(idx)?.clone();
    for seg in rest {
        match cur {
            Value::Struct(fields) => cur = fields.get(seg.as_str())?.clone(),
            _ => return None,
        }
    }
    Some(cur)
}

/// Compare a runtime value to a literal under an operator (numeric widening; text lexical; `Null`
/// never matches). Mirrors the engine kernel's comparison semantics.
fn cmp(v: &Value, op: CmpOp, lit: &Literal) -> bool {
    let ord = value_cmp(v, lit);
    match (op, ord) {
        (CmpOp::Eq, Some(Ordering::Equal)) => true,
        (CmpOp::Ne, Some(o)) => o != Ordering::Equal,
        (CmpOp::Lt, Some(Ordering::Less)) => true,
        (CmpOp::Le, Some(Ordering::Less | Ordering::Equal)) => true,
        (CmpOp::Gt, Some(Ordering::Greater)) => true,
        (CmpOp::Ge, Some(Ordering::Greater | Ordering::Equal)) => true,
        _ => false,
    }
}

/// A partial ordering between a runtime value and a literal (numeric widening; text lexical; bool
/// false<true). Incomparable / null ⇒ `None`.
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

/// A minimal SQL-`LIKE` match (`%` = any run, `_` = one char). Sufficient for a guard; the engine
/// owns the full residual matcher.
fn like_match(s: &str, pattern: &Pattern) -> bool {
    like_glob(s, &pattern.0)
}

/// A tiny glob matcher for `%`/`_` (no escapes — a guard pattern is a small literal). Recursive
/// over chars; bounded by the (small) pattern length.
fn like_glob(s: &str, pat: &str) -> bool {
    let s: Vec<char> = s.chars().collect();
    let p: Vec<char> = pat.chars().collect();
    glob(&s, &p)
}

fn glob(s: &[char], p: &[char]) -> bool {
    match p.split_first() {
        None => s.is_empty(),
        Some((&'%', rest)) => {
            // `%` matches zero or more chars: try every suffix.
            (0..=s.len()).any(|i| glob(&s[i..], rest))
        }
        Some((&'_', rest)) => !s.is_empty() && glob(&s[1..], rest),
        Some((&c, rest)) => !s.is_empty() && s[0] == c && glob(&s[1..], rest),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use qfs_parser::{Pipeline, Source, Values};

    fn guard_stmt(where_src: &str) -> Statement {
        // Build the same `Query |> WHERE` carrier the grammar produces for a TRIGGER WHERE: the
        // predicate is parsed as an expression and wrapped over an empty VALUES source. We parse a
        // real `CREATE TRIGGER … WHERE … DO …` so the carrier is byte-identical to production.
        let src =
            format!("CREATE TRIGGER t ON e WHERE {where_src} DO INSERT INTO /log VALUES ('x')");
        let stmt = qfs_parser::parse_statement(&src).expect("parse trigger");
        let qfs_parser::Statement::Ddl(d) = stmt else {
            panic!("expected ddl")
        };
        *d.where_pred.expect("where_pred present")
    }

    // Silence unused-import warnings for the carrier types in case the helper changes.
    #[allow(unused)]
    fn _carrier() -> Statement {
        Statement::Query(Pipeline {
            source: Source::Values(Values {
                columns: None,
                rows: Vec::new(),
            }),
            ops: Vec::new(),
        })
    }

    #[test]
    fn matching_guard_fires() {
        let g = guard_stmt("priority > 3");
        let cols = vec!["priority".to_string()];
        let row = Row::new(vec![Value::Int(5)]);
        assert!(guard_matches(&g, &cols, &row).unwrap());
    }

    #[test]
    fn new_dot_col_guard_strips_the_new_prefix_and_resolves_the_field() {
        // The canonical guard form `WHERE NEW.body LIKE '%urgent%'`: the leading `NEW` is stripped
        // so `body` resolves against the event's NEW.* field names.
        let g = guard_stmt("NEW.body LIKE '%urgent%'");
        let cols = vec!["body".to_string()];
        assert!(guard_matches(
            &g,
            &cols,
            &Row::new(vec![Value::Text("an urgent note".into())])
        )
        .unwrap());
        assert!(!guard_matches(&g, &cols, &Row::new(vec![Value::Text("calm".into())])).unwrap());
    }

    #[test]
    fn failing_guard_does_not_fire() {
        let g = guard_stmt("priority > 3");
        let cols = vec!["priority".to_string()];
        let row = Row::new(vec![Value::Int(1)]);
        assert!(!guard_matches(&g, &cols, &row).unwrap());
    }

    #[test]
    fn missing_column_does_not_match() {
        let g = guard_stmt("priority > 3");
        let cols = vec!["other".to_string()];
        let row = Row::new(vec![Value::Int(99)]);
        assert!(!guard_matches(&g, &cols, &row).unwrap());
    }
}
