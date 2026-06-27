//! `LAST_RUN()` — the durable-state registry binding, **scoped to JOB query evaluation only**
//! (RFD §8). The stateful-watcher concern: a JOB's `DO` body can run an *incremental* query
//! (e.g. "messages since last run") by referencing `LAST_RUN()`, which resolves to the JOB's
//! stored high-water boundary.
//!
//! ## Implemented as an injection-safe AST rewrite (the t32 `bind_params` twin)
//! At dispatch the scheduler walks the rehydrated DO body and replaces every nullary
//! `LAST_RUN()` call ([`Expr::Fn`] named `LAST_RUN` with no args) with a single typed literal —
//! the boundary as `Expr::Lit(Literal::Int(<epoch-seconds>))`. Because the body is the
//! pre-parsed t31 spec (never re-parsed) and the boundary becomes ONE literal leaf, there is zero
//! parse-time injection surface and the rewrite is structurally identical for any boundary value.
//!
//! `LAST_RUN()` is **never** registered into the global stdlib namespace — the substitution runs
//! ONLY here, on the JOB's DO body at dispatch, so the binding is scoped to JOB evaluation by
//! construction (it cannot leak into arbitrary statements). On first run (`last_run_at = None`)
//! the boundary is the sentinel epoch `0`.

use qfs_parser::{
    CallRef, EffectBody, Expr, FnRef, JoinOp, Literal, PipeOp, Pipeline, PlanWrap, Projection,
    Source, Statement,
};

use crate::schedule::Instant;

/// The case-insensitive function name `LAST_RUN()` is matched on.
const LAST_RUN_FN: &str = "LAST_RUN";

/// Rewrite `stmt` in place: replace every nullary `LAST_RUN()` call with the typed literal
/// `boundary` (epoch seconds; sentinel `0` on first run). The single mutation the dispatch
/// applies to the pre-parsed DO body — no re-parse, zero injection surface.
pub fn bind_last_run(stmt: &mut Statement, boundary: Instant) {
    match stmt {
        Statement::Query(p) => rewrite_pipeline(p, boundary),
        Statement::Effect(e) => {
            match &mut e.body {
                EffectBody::Values(v) => rewrite_values(v, boundary),
                EffectBody::Pipeline(p) => rewrite_pipeline(p, boundary),
                EffectBody::SetWhere { set, filter } => {
                    for a in set {
                        rewrite_expr(&mut a.value, boundary);
                    }
                    if let Some(f) = filter {
                        rewrite_expr(f, boundary);
                    }
                }
            }
            if let Some(projs) = &mut e.returning {
                for proj in projs {
                    rewrite_projection(proj, boundary);
                }
            }
        }
        Statement::Ddl(d) => {
            if let Some(p) = &mut d.do_plan {
                bind_last_run(p, boundary);
            }
            if let Some(q) = &mut d.as_query {
                bind_last_run(q, boundary);
            }
        }
        Statement::Plan(PlanWrap { inner, .. }) => bind_last_run(inner, boundary),
        // A `LET` program (M6, t60): rewrite `LAST_RUN()` through value + body.
        Statement::Let { value, body, .. } => {
            bind_last_run(value, boundary);
            bind_last_run(body, boundary);
        }
        // A `TRANSACTION { … }` block (M6, t62): rewrite `LAST_RUN()` through every member.
        Statement::Transaction { body, .. } => {
            for member in body {
                bind_last_run(member, boundary);
            }
        }
    }
}

/// Whether `stmt` references `LAST_RUN()` anywhere (test/observability introspection).
#[must_use]
pub fn references_last_run(stmt: &Statement) -> bool {
    let mut found = false;
    detect_stmt(stmt, &mut found);
    found
}

fn rewrite_pipeline(p: &mut Pipeline, boundary: Instant) {
    rewrite_source(&mut p.source, boundary);
    for op in &mut p.ops {
        rewrite_pipe_op(op, boundary);
    }
}

fn rewrite_source(s: &mut Source, boundary: Instant) {
    match s {
        Source::Path(_) => {}
        Source::Values(v) => rewrite_values(v, boundary),
        Source::Subquery(p) => rewrite_pipeline(p, boundary),
        // A bare `LET`-bound name (M6, t60) carries no `LAST_RUN()` call to rewrite.
        Source::Name(_) => {}
    }
}

fn rewrite_values(v: &mut qfs_parser::Values, boundary: Instant) {
    for row in &mut v.rows {
        for e in row {
            rewrite_expr(e, boundary);
        }
    }
}

fn rewrite_pipe_op(op: &mut PipeOp, boundary: Instant) {
    match op {
        PipeOp::Where(e) => rewrite_expr(e, boundary),
        PipeOp::Select(projs) | PipeOp::Aggregate(projs) => {
            for p in projs {
                rewrite_projection(p, boundary);
            }
        }
        PipeOp::Extend(assigns) | PipeOp::Set(assigns) => {
            for a in assigns {
                rewrite_expr(&mut a.value, boundary);
            }
        }
        PipeOp::GroupBy(exprs) => {
            for e in exprs {
                rewrite_expr(e, boundary);
            }
        }
        PipeOp::OrderBy(keys) => {
            for k in keys {
                rewrite_expr(&mut k.expr, boundary);
            }
        }
        PipeOp::Join(JoinOp { source, on }) => {
            rewrite_source(source, boundary);
            rewrite_expr(on, boundary);
        }
        PipeOp::Union(p) | PipeOp::Except(p) | PipeOp::Intersect(p) => {
            rewrite_pipeline(p, boundary)
        }
        PipeOp::Call(c) => rewrite_call(c, boundary),
        PipeOp::Limit(_)
        | PipeOp::Distinct
        | PipeOp::As(_)
        | PipeOp::Expand(_)
        | PipeOp::Decode(_)
        | PipeOp::Encode(_) => {}
    }
}

fn rewrite_projection(p: &mut Projection, boundary: Instant) {
    if let Projection::Expr { expr, .. } = p {
        rewrite_expr(expr, boundary);
    }
}

fn rewrite_call(c: &mut CallRef, boundary: Instant) {
    for a in &mut c.args {
        rewrite_expr(&mut a.value, boundary);
    }
}

/// The core substitution: a nullary `LAST_RUN()` [`Expr::Fn`] becomes the typed literal
/// `boundary`. A `LAST_RUN(x)` with arguments is NOT the binding (it would be a user function of
/// the same name with args), so only the zero-arg form is substituted. Recurses into composite
/// expressions and into any `Fn` args (so `f(LAST_RUN())` resolves the inner call).
fn rewrite_expr(e: &mut Expr, boundary: Instant) {
    match e {
        Expr::Fn(f) => {
            if is_last_run(f) {
                *e = Expr::Lit(Literal::Int(boundary));
            } else {
                for a in &mut f.args {
                    rewrite_expr(a, boundary);
                }
            }
        }
        Expr::Binary { lhs, rhs, .. } => {
            rewrite_expr(lhs, boundary);
            rewrite_expr(rhs, boundary);
        }
        Expr::Unary { expr, .. } => rewrite_expr(expr, boundary),
        Expr::In { expr, set } | Expr::AnyOp { expr, set, .. } => {
            rewrite_expr(expr, boundary);
            for s in set {
                rewrite_expr(s, boundary);
            }
        }
        Expr::Between { expr, low, high } => {
            rewrite_expr(expr, boundary);
            rewrite_expr(low, boundary);
            rewrite_expr(high, boundary);
        }
        Expr::Like { expr, pattern } => {
            rewrite_expr(expr, boundary);
            rewrite_expr(pattern, boundary);
        }
        // A lambda body (M6, t61) is walked so a nested `LAST_RUN()` is still substituted.
        Expr::Lambda { body, .. } => rewrite_expr(body, boundary),
        Expr::Lit(_) | Expr::Path(_) | Expr::Col(_) => {}
    }
}

fn is_last_run(f: &FnRef) -> bool {
    f.args.is_empty() && f.name.eq_ignore_ascii_case(LAST_RUN_FN)
}

// --- detection-only walk (references_last_run) ---

fn detect_stmt(stmt: &Statement, found: &mut bool) {
    if *found {
        return;
    }
    match stmt {
        Statement::Query(p) => detect_pipeline(p, found),
        Statement::Effect(e) => {
            match &e.body {
                EffectBody::Values(v) => {
                    for row in &v.rows {
                        for x in row {
                            detect_expr(x, found);
                        }
                    }
                }
                EffectBody::Pipeline(p) => detect_pipeline(p, found),
                EffectBody::SetWhere { set, filter } => {
                    for a in set {
                        detect_expr(&a.value, found);
                    }
                    if let Some(f) = filter {
                        detect_expr(f, found);
                    }
                }
            }
            if let Some(projs) = &e.returning {
                for proj in projs {
                    if let Projection::Expr { expr, .. } = proj {
                        detect_expr(expr, found);
                    }
                }
            }
        }
        Statement::Ddl(d) => {
            if let Some(p) = &d.do_plan {
                detect_stmt(p, found);
            }
            if let Some(q) = &d.as_query {
                detect_stmt(q, found);
            }
        }
        Statement::Plan(PlanWrap { inner, .. }) => detect_stmt(inner, found),
        // A `LET` program (M6, t60): a `LAST_RUN()` may live in value or body.
        Statement::Let { value, body, .. } => {
            detect_stmt(value, found);
            detect_stmt(body, found);
        }
        // A `TRANSACTION { … }` block (M6, t62): a `LAST_RUN()` may live in any member.
        Statement::Transaction { body, .. } => {
            for member in body {
                detect_stmt(member, found);
            }
        }
    }
}

fn detect_pipeline(p: &Pipeline, found: &mut bool) {
    match &p.source {
        Source::Subquery(sp) => detect_pipeline(sp, found),
        Source::Values(v) => {
            for row in &v.rows {
                for x in row {
                    detect_expr(x, found);
                }
            }
        }
        Source::Path(_) => {}
        // A bare `LET`-bound name (M6, t60) is not a `LAST_RUN()` site.
        Source::Name(_) => {}
    }
    for op in &p.ops {
        match op {
            PipeOp::Where(e) => detect_expr(e, found),
            PipeOp::Select(projs) | PipeOp::Aggregate(projs) => {
                for pr in projs {
                    if let Projection::Expr { expr, .. } = pr {
                        detect_expr(expr, found);
                    }
                }
            }
            PipeOp::Extend(assigns) | PipeOp::Set(assigns) => {
                for a in assigns {
                    detect_expr(&a.value, found);
                }
            }
            PipeOp::GroupBy(exprs) => {
                for e in exprs {
                    detect_expr(e, found);
                }
            }
            PipeOp::OrderBy(keys) => {
                for k in keys {
                    detect_expr(&k.expr, found);
                }
            }
            PipeOp::Join(JoinOp { source, on }) => {
                if let Source::Subquery(sp) = source {
                    detect_pipeline(sp, found);
                }
                detect_expr(on, found);
            }
            PipeOp::Union(sp) | PipeOp::Except(sp) | PipeOp::Intersect(sp) => {
                detect_pipeline(sp, found);
            }
            PipeOp::Call(c) => {
                for a in &c.args {
                    detect_expr(&a.value, found);
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
}

fn detect_expr(e: &Expr, found: &mut bool) {
    if *found {
        return;
    }
    match e {
        Expr::Fn(f) => {
            if is_last_run(f) {
                *found = true;
            } else {
                for a in &f.args {
                    detect_expr(a, found);
                }
            }
        }
        Expr::Binary { lhs, rhs, .. } => {
            detect_expr(lhs, found);
            detect_expr(rhs, found);
        }
        Expr::Unary { expr, .. } => detect_expr(expr, found),
        Expr::In { expr, set } | Expr::AnyOp { expr, set, .. } => {
            detect_expr(expr, found);
            for s in set {
                detect_expr(s, found);
            }
        }
        Expr::Between { expr, low, high } => {
            detect_expr(expr, found);
            detect_expr(low, found);
            detect_expr(high, found);
        }
        Expr::Like { expr, pattern } => {
            detect_expr(expr, found);
            detect_expr(pattern, found);
        }
        // A lambda body (M6, t61) is walked so a nested `LAST_RUN()` is still detected.
        Expr::Lambda { body, .. } => detect_expr(body, found),
        Expr::Lit(_) | Expr::Path(_) | Expr::Col(_) => {}
    }
}
