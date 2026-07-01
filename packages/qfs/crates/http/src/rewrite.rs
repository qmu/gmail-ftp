//! The **injection-safe typed parameter rewrite** (t32, the hard part).
//!
//! The endpoint query is stored (t31) as a span-normalised, pre-parsed
//! [`qfs_core::StatementSpec`] — the AST, NOT source text. At request time this module walks
//! that AST and replaces every **bare column reference** ([`Expr::Col`]) whose identifier
//! matches a declared route/query param with an [`Expr::Lit`] carrying the **typed**
//! [`qfs_core::Value`] bound for it ([`crate::QueryArgs`]).
//!
//! ## Why this is injection-safe
//!   * The query is parsed exactly ONCE, at registration. The request value NEVER re-enters
//!     the parser and is NEVER concatenated into DSL text.
//!   * A param value becomes a single, typed [`Expr::Lit`] AST node. A malicious path param
//!     `'; REMOVE /mail/inbox` becomes `Expr::Lit(Literal::Str("'; REMOVE /mail/inbox"))` —
//!     one string-literal leaf. Its lowered plan is therefore **structurally identical** to
//!     the plan for a benign string (the injection golden asserts this).
//!   * The rewrite touches only `Expr::Col` nodes that match a declared param name; every
//!     other node (real column references, function calls, the source path) is untouched.
//!
//! ## Param convention
//! Because the frozen grammar has no `:param` placeholder token, a param slot is written as a
//! bare identifier matching a declared route param, and that identifier **must be distinct
//! from any column name** the query references (so the rewrite cannot ambiguously substitute a
//! column). The natural authoring form:
//! `CREATE ENDPOINT GET /items/:p_id AS (/mock/items |> WHERE id = p_id)` — the route
//! param `:p_id` is distinct from the `id` column, so the LHS `id` stays a column and the RHS
//! `p_id` is the param slot bound from the path segment. EVERY [`Expr::Col`] whose identifier
//! is a declared param is substituted; a param name colliding with a column name would
//! (incorrectly) substitute the column too, hence the distinct-name convention. (When t34/t35
//! introduce a first-class `:param` token, this convention is replaced by an unambiguous
//! placeholder node — the rewrite stays the same, only the matched node kind changes.)

use qfs_parser::{
    CallRef, EffectBody, Expr, FnRef, JoinOp, Literal, PipeOp, Pipeline, PlanWrap, Projection,
    Source, Statement,
};

use qfs_core::Value;

use crate::params::QueryArgs;

/// Rewrite `stmt` in place: replace each declared-param column reference with its typed
/// literal. Only identifiers present in `args` are substituted; everything else is preserved.
/// This is the single mutation the request applies to the pre-parsed query — there is no
/// re-parse, so the request carries zero parse-time injection surface.
pub fn bind_params(stmt: &mut Statement, args: &QueryArgs) {
    match stmt {
        Statement::Query(p) => rewrite_pipeline(p, args),
        Statement::Effect(e) => {
            match &mut e.body {
                EffectBody::Values(v) => rewrite_values(v, args),
                EffectBody::Pipeline(p) => rewrite_pipeline(p, args),
                EffectBody::SetWhere { set, filter } => {
                    for a in set {
                        rewrite_expr(&mut a.value, args);
                    }
                    if let Some(f) = filter {
                        rewrite_expr(f, args);
                    }
                }
            }
            if let Some(projs) = &mut e.returning {
                for proj in projs {
                    rewrite_projection(proj, args);
                }
            }
        }
        Statement::Ddl(d) => {
            if let Some(p) = &mut d.do_plan {
                bind_params(p, args);
            }
            if let Some(q) = &mut d.as_query {
                bind_params(q, args);
            }
        }
        Statement::Plan(PlanWrap { inner, .. }) => bind_params(inner, args),
        // A `LET` program (M6, t60): bind params through both the bound value and the body.
        Statement::Let { value, body, .. } => {
            bind_params(value, args);
            bind_params(body, args);
        }
        // A `TRANSACTION { … }` block (M6, t62): bind params through every effect member.
        Statement::Transaction { body, .. } => {
            for member in body {
                bind_params(member, args);
            }
        }
    }
}

fn rewrite_pipeline(p: &mut Pipeline, args: &QueryArgs) {
    rewrite_source(&mut p.source, args);
    for op in &mut p.ops {
        rewrite_pipe_op(op, args);
    }
}

fn rewrite_source(s: &mut Source, args: &QueryArgs) {
    match s {
        // A path source is a structural address (the mount), never a value slot — leave it.
        Source::Path(_) => {}
        Source::Values(v) => rewrite_values(v, args),
        Source::Subquery(p) => rewrite_pipeline(p, args),
        // A bare `LET`-bound name (M6, t60) is a structural reference, never a value slot.
        Source::Name(_) => {}
    }
}

fn rewrite_values(v: &mut qfs_parser::Values, args: &QueryArgs) {
    for row in &mut v.rows {
        for e in row {
            rewrite_expr(e, args);
        }
    }
}

fn rewrite_pipe_op(op: &mut PipeOp, args: &QueryArgs) {
    match op {
        PipeOp::Where(e) => rewrite_expr(e, args),
        PipeOp::Select(projs) | PipeOp::Aggregate(projs) => {
            for p in projs {
                rewrite_projection(p, args);
            }
        }
        PipeOp::Extend(assigns) | PipeOp::Set(assigns) => {
            for a in assigns {
                rewrite_expr(&mut a.value, args);
            }
        }
        PipeOp::GroupBy(exprs) => {
            for e in exprs {
                rewrite_expr(e, args);
            }
        }
        PipeOp::OrderBy(keys) => {
            for k in keys {
                rewrite_expr(&mut k.expr, args);
            }
        }
        PipeOp::Join(JoinOp { source, on }) => {
            rewrite_source(source, args);
            rewrite_expr(on, args);
        }
        PipeOp::Union(p) | PipeOp::Except(p) | PipeOp::Intersect(p) => rewrite_pipeline(p, args),
        PipeOp::Call(c) => rewrite_call(c, args),
        // No expression payload to rewrite.
        PipeOp::Limit(_)
        | PipeOp::Distinct
        | PipeOp::As(_)
        | PipeOp::Expand(_)
        | PipeOp::Decode(_)
        | PipeOp::Encode(_) => {}
    }
}

fn rewrite_projection(p: &mut Projection, args: &QueryArgs) {
    if let Projection::Expr { expr, .. } = p {
        rewrite_expr(expr, args);
    }
}

fn rewrite_call(c: &mut CallRef, args: &QueryArgs) {
    for a in &mut c.args {
        rewrite_expr(&mut a.value, args);
    }
}

fn rewrite_fn(f: &mut FnRef, args: &QueryArgs) {
    for a in &mut f.args {
        rewrite_expr(a, args);
    }
}

/// The core substitution: a bare [`Expr::Col`] whose identifier is a declared param becomes a
/// typed [`Expr::Lit`]. Recurses into composite expressions. A [`Expr::Path`] (struct
/// navigation `a.b`) is NOT a param slot — params are single bare identifiers — so it is left
/// as a column reference even if its first segment happens to match a param name.
fn rewrite_expr(e: &mut Expr, args: &QueryArgs) {
    match e {
        Expr::Col(ident) => {
            if let Some(value) = args.get(ident) {
                *e = Expr::Lit(value_to_literal(value));
            }
        }
        Expr::Fn(f) => rewrite_fn(f, args),
        Expr::Binary { lhs, rhs, .. } => {
            rewrite_expr(lhs, args);
            rewrite_expr(rhs, args);
        }
        Expr::Unary { expr, .. } => rewrite_expr(expr, args),
        Expr::In { expr, set } | Expr::AnyOp { expr, set, .. } => {
            rewrite_expr(expr, args);
            for s in set {
                rewrite_expr(s, args);
            }
        }
        Expr::Between { expr, low, high } => {
            rewrite_expr(expr, args);
            rewrite_expr(low, args);
            rewrite_expr(high, args);
        }
        Expr::Like { expr, pattern } => {
            rewrite_expr(expr, args);
            rewrite_expr(pattern, args);
        }
        // A lambda body (M6, t61) is walked like any sub-expression so a bound param slot
        // inside it is still substituted.
        Expr::Lambda { body, .. } => rewrite_expr(body, args),
        // t92 composite constructors: substitute bound param slots inside each element/field.
        Expr::Array(elems) => {
            for el in elems {
                rewrite_expr(el, args);
            }
        }
        Expr::Struct(fields) => {
            for (_, v) in fields {
                rewrite_expr(v, args);
            }
        }
        // A literal is already a value; a path is struct navigation, not a param slot.
        Expr::Lit(_) | Expr::Path(_) => {}
    }
}

/// Collect the set of **bare column identifiers** ([`Expr::Col`]) the statement references.
/// This is the registration-time shadow check's input: if a declared route-param name appears
/// here, substituting it would replace a real column node (access widening), so the endpoint is
/// refused at registration. The walk mirrors [`bind_params`] exactly, so it sees every position
/// a substitution could touch — keeping the check and the rewrite in lockstep (one cannot drift
/// from the other). Struct-navigation paths ([`Expr::Path`]) are not bare columns and are not a
/// param substitution target, so they are deliberately NOT collected.
#[must_use]
pub fn referenced_columns(stmt: &Statement) -> std::collections::BTreeSet<String> {
    let mut cols = std::collections::BTreeSet::new();
    collect_stmt(stmt, &mut cols);
    cols
}

fn collect_stmt(stmt: &Statement, out: &mut std::collections::BTreeSet<String>) {
    match stmt {
        Statement::Query(p) => collect_pipeline(p, out),
        Statement::Effect(e) => {
            match &e.body {
                EffectBody::Values(v) => collect_values(v, out),
                EffectBody::Pipeline(p) => collect_pipeline(p, out),
                EffectBody::SetWhere { set, filter } => {
                    for a in set {
                        collect_expr(&a.value, out);
                    }
                    if let Some(f) = filter {
                        collect_expr(f, out);
                    }
                }
            }
            if let Some(projs) = &e.returning {
                for proj in projs {
                    collect_projection(proj, out);
                }
            }
        }
        Statement::Ddl(d) => {
            if let Some(p) = &d.do_plan {
                collect_stmt(p, out);
            }
            if let Some(q) = &d.as_query {
                collect_stmt(q, out);
            }
        }
        Statement::Plan(PlanWrap { inner, .. }) => collect_stmt(inner, out),
        // A `LET` program (M6, t60): collect referenced columns from value + body.
        Statement::Let { value, body, .. } => {
            collect_stmt(value, out);
            collect_stmt(body, out);
        }
        // A `TRANSACTION { … }` block (M6, t62): collect referenced columns from every member.
        Statement::Transaction { body, .. } => {
            for member in body {
                collect_stmt(member, out);
            }
        }
    }
}

fn collect_pipeline(p: &Pipeline, out: &mut std::collections::BTreeSet<String>) {
    collect_source(&p.source, out);
    for op in &p.ops {
        collect_pipe_op(op, out);
    }
}

fn collect_source(s: &Source, out: &mut std::collections::BTreeSet<String>) {
    match s {
        Source::Path(_) => {}
        Source::Values(v) => collect_values(v, out),
        Source::Subquery(p) => collect_pipeline(p, out),
        // A bare `LET`-bound name (M6, t60) references no declared column.
        Source::Name(_) => {}
    }
}

fn collect_values(v: &qfs_parser::Values, out: &mut std::collections::BTreeSet<String>) {
    for row in &v.rows {
        for e in row {
            collect_expr(e, out);
        }
    }
}

fn collect_pipe_op(op: &PipeOp, out: &mut std::collections::BTreeSet<String>) {
    match op {
        PipeOp::Where(e) => collect_expr(e, out),
        PipeOp::Select(projs) | PipeOp::Aggregate(projs) => {
            for p in projs {
                collect_projection(p, out);
            }
        }
        PipeOp::Extend(assigns) | PipeOp::Set(assigns) => {
            for a in assigns {
                collect_expr(&a.value, out);
            }
        }
        PipeOp::GroupBy(exprs) => {
            for e in exprs {
                collect_expr(e, out);
            }
        }
        PipeOp::OrderBy(keys) => {
            for k in keys {
                collect_expr(&k.expr, out);
            }
        }
        PipeOp::Join(JoinOp { source, on }) => {
            collect_source(source, out);
            collect_expr(on, out);
        }
        PipeOp::Union(p) | PipeOp::Except(p) | PipeOp::Intersect(p) => collect_pipeline(p, out),
        PipeOp::Call(c) => {
            for a in &c.args {
                collect_expr(&a.value, out);
            }
        }
        PipeOp::Limit(_)
        | PipeOp::Distinct
        | PipeOp::As(_)
        | PipeOp::Expand(_)
        | PipeOp::Decode(_)
        | PipeOp::Encode(_) => {}
    }
}

fn collect_projection(p: &Projection, out: &mut std::collections::BTreeSet<String>) {
    if let Projection::Expr { expr, .. } = p {
        collect_expr(expr, out);
    }
}

fn collect_expr(e: &Expr, out: &mut std::collections::BTreeSet<String>) {
    match e {
        Expr::Col(ident) => {
            out.insert(ident.clone());
        }
        Expr::Fn(f) => {
            for a in &f.args {
                collect_expr(a, out);
            }
        }
        Expr::Binary { lhs, rhs, .. } => {
            collect_expr(lhs, out);
            collect_expr(rhs, out);
        }
        Expr::Unary { expr, .. } => collect_expr(expr, out),
        Expr::In { expr, set } | Expr::AnyOp { expr, set, .. } => {
            collect_expr(expr, out);
            for s in set {
                collect_expr(s, out);
            }
        }
        Expr::Between { expr, low, high } => {
            collect_expr(expr, out);
            collect_expr(low, out);
            collect_expr(high, out);
        }
        Expr::Like { expr, pattern } => {
            collect_expr(expr, out);
            collect_expr(pattern, out);
        }
        // A lambda body (M6, t61) is walked so any bare column it references is collected
        // for the access-widening shadow check.
        Expr::Lambda { body, .. } => collect_expr(body, out),
        // t92 composite constructors: collect bare columns referenced in each element/field.
        Expr::Array(elems) => {
            for el in elems {
                collect_expr(el, out);
            }
        }
        Expr::Struct(fields) => {
            for (_, v) in fields {
                collect_expr(v, out);
            }
        }
        Expr::Lit(_) | Expr::Path(_) => {}
    }
}

/// Collect the `/driver/seg/...` SOURCE PATH strings the statement reads from (every `FROM`
/// path, including joins / subqueries). The registration-time shadow check resolves each to its
/// driver schema so it can compare a declared param name against the source's REAL data
/// columns (the precise access-widening test). `VALUES` / dotted struct paths are not mount
/// sources and are not collected.
#[must_use]
pub fn source_paths(stmt: &Statement) -> Vec<String> {
    let mut paths = Vec::new();
    collect_src_paths_stmt(stmt, &mut paths);
    paths
}

fn collect_src_paths_stmt(stmt: &Statement, out: &mut Vec<String>) {
    match stmt {
        Statement::Query(p) => collect_src_paths_pipeline(p, out),
        Statement::Effect(e) => {
            out.push(path_string(&e.target));
            if let EffectBody::Pipeline(p) = &e.body {
                collect_src_paths_pipeline(p, out);
            }
        }
        Statement::Ddl(d) => {
            if let Some(p) = &d.do_plan {
                collect_src_paths_stmt(p, out);
            }
            if let Some(q) = &d.as_query {
                collect_src_paths_stmt(q, out);
            }
        }
        Statement::Plan(PlanWrap { inner, .. }) => collect_src_paths_stmt(inner, out),
        // A `LET` program (M6, t60): collect source paths from value + body.
        Statement::Let { value, body, .. } => {
            collect_src_paths_stmt(value, out);
            collect_src_paths_stmt(body, out);
        }
        // A `TRANSACTION { … }` block (M6, t62): collect source paths from every member.
        Statement::Transaction { body, .. } => {
            for member in body {
                collect_src_paths_stmt(member, out);
            }
        }
    }
}

fn collect_src_paths_pipeline(p: &Pipeline, out: &mut Vec<String>) {
    collect_src_paths_source(&p.source, out);
    for op in &p.ops {
        match op {
            PipeOp::Join(JoinOp { source, .. }) => collect_src_paths_source(source, out),
            PipeOp::Union(p) | PipeOp::Except(p) | PipeOp::Intersect(p) => {
                collect_src_paths_pipeline(p, out)
            }
            _ => {}
        }
    }
}

fn collect_src_paths_source(s: &Source, out: &mut Vec<String>) {
    match s {
        Source::Path(p) => out.push(path_string(p)),
        Source::Values(_) => {}
        Source::Subquery(p) => collect_src_paths_pipeline(p, out),
        // A bare `LET`-bound name (M6, t60) is not a mount path — nothing to collect.
        Source::Name(_) => {}
    }
}

/// Reconstruct the `/seg/seg` mount-path string from a [`qfs_parser::PathExpr`]'s segments
/// (the registry resolves on this leading-slash form).
fn path_string(p: &qfs_parser::PathExpr) -> String {
    let mut s = String::new();
    for seg in &p.segments {
        s.push('/');
        s.push_str(&seg.name);
    }
    s
}

/// Convert a typed [`qfs_core::Value`] to a parser [`Literal`] AST node. This is a pure data
/// mapping — the value's content is carried verbatim into a typed leaf, never re-parsed.
/// `Null` becomes the null literal; structured/blob values (which a scalar param never
/// produces from [`crate::params::infer_value`]) degrade to their textual form rather than
/// panicking, keeping the rewrite total + injection-safe.
fn value_to_literal(value: &Value) -> Literal {
    match value {
        Value::Null => Literal::Null,
        Value::Bool(b) => Literal::Bool(*b),
        Value::Int(i) | Value::Timestamp(i) => Literal::Int(*i),
        Value::Float(f) => Literal::Float(*f),
        Value::Text(s) => Literal::Str(s.clone()),
        // A scalar request param never yields these; carry a safe textual rendering so the
        // substitution is total (the value is still a single typed literal node).
        other => Literal::Str(format!("{other:?}")),
    }
}
