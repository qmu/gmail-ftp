//! The **`NEW.*` plan binding** (t34): rewrite a handler statement so each `NEW.<col>` reference
//! becomes the typed [`Value`] from the event's `new` row. PURE — no I/O, no mutation; the
//! structural twin of the t32 HTTP `bind_params` rewrite (an injection-safe typed substitution),
//! but keyed on the `NEW.<col>` struct-navigation path rather than a route param.
//!
//! ## Why this is injection-safe (same property as t32)
//! The handler body is parsed exactly ONCE (at `CREATE TRIGGER`, into the stored `PlanSpec`). At
//! fire time the event value NEVER re-enters the parser and is NEVER concatenated into DSL text: a
//! `NEW.<col>` path node is replaced by a single typed [`Expr::Lit`] leaf carrying the value
//! verbatim. A malicious payload field is therefore one string-literal node — its lowered plan is
//! structurally identical to the plan for a benign value.
//!
//! Only `NEW.<col>` (an [`Expr::Path`] whose head segment is `NEW`) is substituted; a bare column,
//! a real struct path, a function call, and the source path are all untouched.

use std::collections::BTreeMap;

use qfs_core::Value;
use qfs_parser::{
    CallRef, EffectBody, Expr, FnRef, Ident, JoinOp, Literal, PipeOp, Pipeline, PlanWrap,
    Projection, Source, Statement,
};

/// The `NEW.*` binding environment: the event row's `col -> Value` map.
#[derive(Debug, Clone, Default)]
pub struct NewBindings {
    fields: BTreeMap<String, Value>,
}

impl NewBindings {
    /// Build a binding env from a positional `(columns, row values)` pair (the event's `NEW.*`).
    #[must_use]
    pub fn from_row(columns: &[String], values: &[Value]) -> Self {
        let mut fields = BTreeMap::new();
        for (name, value) in columns.iter().zip(values.iter()) {
            fields.insert(name.clone(), value.clone());
        }
        Self { fields }
    }

    /// Look up a `NEW.<col>` value.
    #[must_use]
    pub fn get(&self, col: &str) -> Option<&Value> {
        self.fields.get(col)
    }
}

/// Rewrite `stmt` in place: replace each `NEW.<col>` reference with its typed literal. Only
/// `NEW.<col>` paths present in `binds` are substituted; everything else is preserved. The single
/// mutation the fire applies to the pre-parsed plan body — no re-parse, zero parse-time injection.
pub fn bind_new(stmt: &mut Statement, binds: &NewBindings) {
    match stmt {
        Statement::Query(p) => rewrite_pipeline(p, binds),
        Statement::Effect(e) => {
            match &mut e.body {
                EffectBody::Values(v) => rewrite_values(v, binds),
                EffectBody::Pipeline(p) => rewrite_pipeline(p, binds),
                EffectBody::SetWhere { set, filter } => {
                    for a in set {
                        rewrite_expr(&mut a.value, binds);
                    }
                    if let Some(f) = filter {
                        rewrite_expr(f, binds);
                    }
                }
            }
            if let Some(projs) = &mut e.returning {
                for proj in projs {
                    rewrite_projection(proj, binds);
                }
            }
        }
        Statement::Ddl(d) => {
            if let Some(p) = &mut d.do_plan {
                bind_new(p, binds);
            }
            if let Some(q) = &mut d.as_query {
                bind_new(q, binds);
            }
            if let Some(w) = &mut d.where_pred {
                bind_new(w, binds);
            }
        }
        Statement::Plan(PlanWrap { inner, .. }) => bind_new(inner, binds),
    }
}

fn rewrite_pipeline(p: &mut Pipeline, binds: &NewBindings) {
    rewrite_source(&mut p.source, binds);
    for op in &mut p.ops {
        rewrite_pipe_op(op, binds);
    }
}

fn rewrite_source(s: &mut Source, binds: &NewBindings) {
    match s {
        Source::Path(_) => {}
        Source::Values(v) => rewrite_values(v, binds),
        Source::Subquery(p) => rewrite_pipeline(p, binds),
    }
}

fn rewrite_values(v: &mut qfs_parser::Values, binds: &NewBindings) {
    for row in &mut v.rows {
        for e in row {
            rewrite_expr(e, binds);
        }
    }
}

fn rewrite_pipe_op(op: &mut PipeOp, binds: &NewBindings) {
    match op {
        PipeOp::Where(e) => rewrite_expr(e, binds),
        PipeOp::Select(projs) | PipeOp::Aggregate(projs) => {
            for p in projs {
                rewrite_projection(p, binds);
            }
        }
        PipeOp::Extend(assigns) | PipeOp::Set(assigns) => {
            for a in assigns {
                rewrite_expr(&mut a.value, binds);
            }
        }
        PipeOp::GroupBy(exprs) => {
            for e in exprs {
                rewrite_expr(e, binds);
            }
        }
        PipeOp::OrderBy(keys) => {
            for k in keys {
                rewrite_expr(&mut k.expr, binds);
            }
        }
        PipeOp::Join(JoinOp { source, on }) => {
            rewrite_source(source, binds);
            rewrite_expr(on, binds);
        }
        PipeOp::Union(p) | PipeOp::Except(p) | PipeOp::Intersect(p) => rewrite_pipeline(p, binds),
        PipeOp::Call(c) => rewrite_call(c, binds),
        PipeOp::Limit(_)
        | PipeOp::Distinct
        | PipeOp::As(_)
        | PipeOp::Expand(_)
        | PipeOp::Decode(_)
        | PipeOp::Encode(_) => {}
    }
}

fn rewrite_projection(p: &mut Projection, binds: &NewBindings) {
    if let Projection::Expr { expr, .. } = p {
        rewrite_expr(expr, binds);
    }
}

fn rewrite_call(c: &mut CallRef, binds: &NewBindings) {
    for a in &mut c.args {
        rewrite_expr(&mut a.value, binds);
    }
}

fn rewrite_fn(f: &mut FnRef, binds: &NewBindings) {
    for a in &mut f.args {
        rewrite_expr(a, binds);
    }
}

/// The core substitution: an [`Expr::Path`] `NEW.<col>` (two segments, head `NEW`) becomes a typed
/// [`Expr::Lit`]. Recurses into composite expressions. A bare [`Expr::Col`] is a real column
/// reference (NOT a `NEW.*` slot) and is left untouched.
fn rewrite_expr(e: &mut Expr, binds: &NewBindings) {
    match e {
        Expr::Path(segs) => {
            if let Some(value) = new_field(segs, binds) {
                *e = Expr::Lit(value_to_literal(&value));
            }
        }
        Expr::Fn(f) => rewrite_fn(f, binds),
        Expr::Binary { lhs, rhs, .. } => {
            rewrite_expr(lhs, binds);
            rewrite_expr(rhs, binds);
        }
        Expr::Unary { expr, .. } => rewrite_expr(expr, binds),
        Expr::In { expr, set } | Expr::AnyOp { expr, set, .. } => {
            rewrite_expr(expr, binds);
            for s in set {
                rewrite_expr(s, binds);
            }
        }
        Expr::Between { expr, low, high } => {
            rewrite_expr(expr, binds);
            rewrite_expr(low, binds);
            rewrite_expr(high, binds);
        }
        Expr::Like { expr, pattern } => {
            rewrite_expr(expr, binds);
            rewrite_expr(pattern, binds);
        }
        Expr::Lit(_) | Expr::Col(_) => {}
    }
}

/// If `segs` is `NEW.<col>` (exactly two segments, head `NEW`), return the bound value for `<col>`.
fn new_field(segs: &[Ident], binds: &NewBindings) -> Option<Value> {
    match segs {
        [head, col] if head == "NEW" => binds.get(col).cloned(),
        _ => None,
    }
}

/// Convert a typed [`Value`] to a parser [`Literal`] AST node (pure data mapping — the value is
/// carried verbatim into a typed leaf, never re-parsed). Mirrors the t32 HTTP `value_to_literal`.
fn value_to_literal(value: &Value) -> Literal {
    match value {
        Value::Null => Literal::Null,
        Value::Bool(b) => Literal::Bool(*b),
        Value::Int(i) | Value::Timestamp(i) => Literal::Int(*i),
        Value::Float(f) => Literal::Float(*f),
        Value::Text(s) => Literal::Str(s.clone()),
        // Structured/blob values carry a safe textual rendering so the substitution is total.
        other => Literal::Str(format!("{other:?}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use qfs_parser::parse_statement;

    #[test]
    fn new_field_path_is_bound_to_typed_literal() {
        let mut stmt =
            parse_statement("INSERT INTO /log VALUES (NEW.subject)").expect("parse handler");
        let binds = NewBindings::from_row(
            &["subject".to_string()],
            &[Value::Text("hello".to_string())],
        );
        bind_new(&mut stmt, &binds);
        // The NEW.subject path is now a string literal — assert it round-trips with the value.
        let Statement::Effect(e) = &stmt else {
            panic!("expected effect")
        };
        let EffectBody::Values(v) = &e.body else {
            panic!("expected values body")
        };
        assert_eq!(v.rows[0][0], Expr::Lit(Literal::Str("hello".to_string())));
    }

    #[test]
    fn bare_column_is_not_a_new_slot() {
        let mut stmt = parse_statement("INSERT INTO /log VALUES (subject)").expect("parse handler");
        let before = stmt.clone();
        let binds =
            NewBindings::from_row(&["subject".to_string()], &[Value::Text("x".to_string())]);
        bind_new(&mut stmt, &binds);
        // A bare `subject` is a real column reference, not `NEW.subject` — left untouched.
        assert_eq!(stmt, before);
    }
}
