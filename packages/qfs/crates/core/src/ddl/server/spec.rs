//! [`StatementSpec`] / [`PlanSpec`] — the deferred-body serializable specs (t31, RFD §8).
//!
//! A spec is a *fully-parsed* `qfs_parser::Statement` wrapped so it serializes as plain data
//! and rehydrates via serde WITHOUT re-parsing (the runtime cannot hit a parse error at fire
//! time). It is **data**, never a live `Plan` that could be committed by accident (purity).
//!
//! ## Span normalisation (the CREATE ≡ INSERT equivalence enabler)
//! A body parsed from an inline `CREATE … DO <plan>` and the same body parsed from an
//! `INSERT INTO /server/…` string column carry **different source spans** (the byte offsets
//! differ by the surrounding statement). Spans are diagnostic-only metadata, not semantics,
//! so the spec zeroes every span on construction ([`normalize_spans`]). Two specs built from
//! equivalent bodies therefore compare equal and serialize byte-identically — which is what
//! makes a body-bearing `CREATE` and its `INSERT` twin store one canonical structure.

use serde::{Deserialize, Serialize};

use qfs_parser::{
    CallRef, Codec, Expr, FnRef, JoinOp, PathExpr, PipeOp, Pipeline, PlanWrap, Projection, Source,
    Statement,
};

/// A fully-parsed, serializable representation of a statement body (`AS <query>`). The parsed
/// AST is stored span-normalised so the canonical form is independent of the body's source
/// offset. Serializes as plain data; rehydrates via serde with no re-parse.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StatementSpec {
    /// The parsed, span-normalised statement.
    stmt: Statement,
}

impl StatementSpec {
    /// Build a spec from a parsed statement, normalising spans so the canonical form is
    /// independent of where the body was parsed from.
    #[must_use]
    pub fn from_statement(mut stmt: Statement) -> Self {
        normalize_spans(&mut stmt);
        Self { stmt }
    }

    /// Build a spec from a borrowed statement (clones + normalises). Convenience for the
    /// optional-body desugar path (`.map(StatementSpec::from_statement_ref)`).
    #[must_use]
    pub fn from_statement_ref(stmt: &Statement) -> Self {
        Self::from_statement(stmt.clone())
    }

    /// The stored (span-normalised) statement.
    #[must_use]
    pub fn statement(&self) -> &Statement {
        &self.stmt
    }

    /// The canonical serialized form: deterministic JSON over the span-normalised AST. Two
    /// specs from equivalent bodies produce the **same** string — the stored config-row value
    /// that makes CREATE ≡ INSERT exact. serde_json key order follows struct field order
    /// (stable), so this is deterministic.
    #[must_use]
    pub fn canonical(&self) -> String {
        serde_json::to_string(&self.stmt).unwrap_or_default()
    }

    /// Rehydrate a spec from its canonical serialized form (the runtime's fire-time path).
    /// No parser is invoked — a malformed source can never surface here.
    ///
    /// # Errors
    /// A serde error string if the canonical form is not a valid serialized statement.
    pub fn from_canonical(canonical: &str) -> Result<Self, String> {
        let stmt: Statement = serde_json::from_str(canonical).map_err(|e| e.to_string())?;
        Ok(Self { stmt })
    }
}

/// A fully-parsed, serializable representation of an effect-plan body (`DO <plan>`). Kept as
/// **data** (never a live `qfs_plan::Plan`) so embedding it does not execute it (purity). A
/// thin newtype over [`StatementSpec`] so the body's *role* (a plan to run later) is typed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlanSpec {
    /// The parsed, span-normalised plan body.
    spec: StatementSpec,
}

impl PlanSpec {
    /// Build a plan spec from a parsed statement (span-normalised).
    #[must_use]
    pub fn from_statement(stmt: Statement) -> Self {
        Self {
            spec: StatementSpec::from_statement(stmt),
        }
    }

    /// Build a plan spec from a borrowed statement (clones + normalises).
    #[must_use]
    pub fn from_statement_ref(stmt: &Statement) -> Self {
        Self {
            spec: StatementSpec::from_statement_ref(stmt),
        }
    }

    /// The stored (span-normalised) statement.
    #[must_use]
    pub fn statement(&self) -> &Statement {
        self.spec.statement()
    }

    /// The canonical serialized form (see [`StatementSpec::canonical`]).
    #[must_use]
    pub fn canonical(&self) -> String {
        self.spec.canonical()
    }

    /// Rehydrate from the canonical serialized form (no re-parse).
    ///
    /// # Errors
    /// A serde error string if the canonical form is invalid.
    pub fn from_canonical(canonical: &str) -> Result<Self, String> {
        Ok(Self {
            spec: StatementSpec::from_canonical(canonical)?,
        })
    }
}

// ---------------------------------------------------------------------------
// Span normalisation: zero every Span in the owned AST (diagnostic-only metadata)
// ---------------------------------------------------------------------------

const ZERO: qfs_parser::Span = qfs_parser::Span::new(0, 0);

/// Zero every source span in `stmt` in place. Spans are byte offsets — diagnostic-only,
/// non-semantic — so zeroing them makes a body's canonical form independent of where it was
/// parsed from. This is what lets a `CREATE … DO <plan>` body and the same body parsed from
/// an `INSERT INTO /server/…` string column compare equal.
pub fn normalize_spans(stmt: &mut Statement) {
    match stmt {
        Statement::Query(p) => normalize_pipeline(p),
        Statement::Effect(e) => {
            normalize_path(&mut e.target);
            match &mut e.body {
                qfs_parser::EffectBody::Values(v) => normalize_values(v),
                qfs_parser::EffectBody::Pipeline(p) => normalize_pipeline(p),
                qfs_parser::EffectBody::SetWhere { set, filter } => {
                    for a in set {
                        normalize_expr(&mut a.value);
                    }
                    if let Some(f) = filter {
                        normalize_expr(f);
                    }
                }
            }
            if let Some(projs) = &mut e.returning {
                for proj in projs {
                    normalize_projection(proj);
                }
            }
        }
        Statement::Ddl(d) => {
            if let Some(p) = &mut d.do_plan {
                normalize_spans(p);
            }
            if let Some(q) = &mut d.as_query {
                normalize_spans(q);
            }
        }
        Statement::Plan(PlanWrap { inner, span, .. }) => {
            *span = ZERO;
            normalize_spans(inner);
        }
        // A `LET` binding (M6, t60): normalise both the bound value and the body so a
        // `LET`-carrying body round-trips identically regardless of where it was parsed.
        Statement::Let { value, body, .. } => {
            normalize_spans(value);
            normalize_spans(body);
        }
        // A `TRANSACTION { … }` block (M6, t62): zero the block span and normalise every member,
        // so a transaction-carrying body round-trips identically regardless of parse offset.
        Statement::Transaction { body, span } => {
            *span = ZERO;
            for member in body {
                normalize_spans(member);
            }
        }
    }
}

fn normalize_pipeline(p: &mut Pipeline) {
    normalize_source(&mut p.source);
    for op in &mut p.ops {
        normalize_pipe_op(op);
    }
}

fn normalize_source(s: &mut Source) {
    match s {
        Source::Path(path) => normalize_path(path),
        Source::Values(v) => normalize_values(v),
        Source::Subquery(p) => normalize_pipeline(p),
        // A bare `LET`-bound name (M6, t60) carries no span to normalise.
        Source::Name(_) => {}
    }
}

fn normalize_values(v: &mut qfs_parser::Values) {
    for row in &mut v.rows {
        for e in row {
            normalize_expr(e);
        }
    }
}

fn normalize_pipe_op(op: &mut PipeOp) {
    match op {
        PipeOp::Where(e) => normalize_expr(e),
        PipeOp::Select(projs) | PipeOp::Aggregate(projs) => {
            for p in projs {
                normalize_projection(p);
            }
        }
        PipeOp::Extend(assigns) | PipeOp::Set(assigns) => {
            for a in assigns {
                normalize_expr(&mut a.value);
            }
        }
        PipeOp::GroupBy(exprs) => {
            for e in exprs {
                normalize_expr(e);
            }
        }
        PipeOp::OrderBy(keys) => {
            for k in keys {
                normalize_expr(&mut k.expr);
            }
        }
        PipeOp::Join(JoinOp { source, on }) => {
            normalize_source(source);
            normalize_expr(on);
        }
        PipeOp::Union(p) | PipeOp::Except(p) | PipeOp::Intersect(p) => normalize_pipeline(p),
        PipeOp::Decode(c) | PipeOp::Encode(c) => normalize_codec(c),
        PipeOp::Call(c) => normalize_call(c),
        // No-span variants.
        PipeOp::Limit(_) | PipeOp::Distinct | PipeOp::As(_) | PipeOp::Expand(_) => {}
    }
}

fn normalize_projection(p: &mut Projection) {
    if let Projection::Expr { expr, .. } = p {
        normalize_expr(expr);
    }
}

fn normalize_path(p: &mut PathExpr) {
    p.span = ZERO;
}

fn normalize_codec(c: &mut Codec) {
    c.span = ZERO;
}

fn normalize_fn(f: &mut FnRef) {
    f.span = ZERO;
    for a in &mut f.args {
        normalize_expr(a);
    }
}

fn normalize_call(c: &mut CallRef) {
    c.span = ZERO;
    for a in &mut c.args {
        normalize_expr(&mut a.value);
    }
}

fn normalize_expr(e: &mut Expr) {
    match e {
        Expr::Fn(f) => normalize_fn(f),
        Expr::Path(_) | Expr::Col(_) | Expr::Lit(_) => {}
        Expr::Binary { lhs, rhs, .. } => {
            normalize_expr(lhs);
            normalize_expr(rhs);
        }
        Expr::Unary { expr, .. } => normalize_expr(expr),
        Expr::In { expr, set } | Expr::AnyOp { expr, set, .. } => {
            normalize_expr(expr);
            for s in set {
                normalize_expr(s);
            }
        }
        Expr::Between { expr, low, high } => {
            normalize_expr(expr);
            normalize_expr(low);
            normalize_expr(high);
        }
        Expr::Like { expr, pattern } => {
            normalize_expr(expr);
            normalize_expr(pattern);
        }
        // A lambda body (M6, t61) is normalized like any sub-expression; its parameter
        // list carries no span-bearing nodes to canonicalize.
        Expr::Lambda { body, .. } => normalize_expr(body),
        // t92 composite constructors: normalize each element/field sub-expression.
        Expr::Array(elems) => {
            for el in elems {
                normalize_expr(el);
            }
        }
        Expr::Struct(fields) => {
            for (_, v) in fields {
                normalize_expr(v);
            }
        }
    }
}
