//! Lowering the parser AST into the planner IR — the concrete answer to the t07
//! carry-over O-t07-3 ("t14 must source predicates from the AST").
//!
//! The t07 evaluator's `PlanSource::Filter` deliberately dropped the predicate `Expr`;
//! this crate does not consume `PlanSource`. Instead it lowers the parser [`Expr`]
//! directly into the typed [`Predicate`] IR (t05), and a whole [`Pipeline`] into a
//! [`LogicalPlan`]. The lowering is pure and total over its inputs: an expression form a
//! `WHERE` cannot be expressed as a typed comparison predicate (e.g. a bare column, an
//! arbitrary `fn(...)`) is reported as a structured [`LowerError`], never silently
//! dropped — so the planner never loses a filter.

use qfs_parser::{Expr, Literal as AstLit, Op, PipeOp, Pipeline, Projection, Source};
use qfs_types::{CmpOp, ColRef, Literal, Name, Pattern, Predicate, Schema, Value};

use crate::logical::{
    Aggregate, Aggregator, JoinKind, JoinOn, LogicalPlan, OrderKey, ScalarExpr, SetKind, SourceId,
};

/// A structured lowering error — an AST shape the planner IR cannot represent.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[non_exhaustive]
pub enum LowerError {
    /// A `WHERE`/`ON` expression that is not a comparison predicate (a bare column, a
    /// raw `fn(...)`, an unsupported operator). Carries a short description for AI repair.
    UnsupportedPredicate {
        /// What was encountered.
        what: String,
    },
    /// A `JOIN ... ON` that is not a simple `lhs = rhs` column equality.
    UnsupportedJoinCondition,
    /// A projection the planner cannot lower to a column list (`fn(...)` projection in a
    /// context that is not an `AGGREGATE`).
    UnsupportedProjection {
        /// What was encountered.
        what: String,
    },
}

impl LowerError {
    /// A stable, machine-readable code (RFD §5).
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            LowerError::UnsupportedPredicate { .. } => "unsupported_predicate",
            LowerError::UnsupportedJoinCondition => "unsupported_join_condition",
            LowerError::UnsupportedProjection { .. } => "unsupported_projection",
        }
    }
}

impl std::fmt::Display for LowerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LowerError::UnsupportedPredicate { what } => {
                write!(f, "unsupported predicate: {what}")
            }
            LowerError::UnsupportedJoinCondition => {
                f.write_str("JOIN ON must be a simple column equality `a = b`")
            }
            LowerError::UnsupportedProjection { what } => {
                write!(f, "unsupported projection: {what}")
            }
        }
    }
}

impl std::error::Error for LowerError {}

/// Lower a parser [`Expr`] into the typed [`Predicate`] IR (O-t07-3). The boolean
/// structure (`AND`/`OR`/`NOT`), comparisons, `IN`, `BETWEEN`, and `LIKE` map onto the
/// typed predicate; anything else is a structured [`LowerError`].
///
/// # Errors
/// [`LowerError::UnsupportedPredicate`] for a non-predicate expression shape.
pub fn lower_predicate(expr: &Expr) -> Result<Predicate, LowerError> {
    match expr {
        Expr::Binary { op, lhs, rhs } => match op {
            Op::And => Ok(Predicate::And(
                Box::new(lower_predicate(lhs)?),
                Box::new(lower_predicate(rhs)?),
            )),
            Op::Or => Ok(Predicate::Or(
                Box::new(lower_predicate(lhs)?),
                Box::new(lower_predicate(rhs)?),
            )),
            Op::Eq | Op::Ne | Op::Lt | Op::Gt | Op::Le | Op::Ge | Op::Match => {
                let col = col_ref(lhs)?;
                let lit = literal(rhs)?;
                Ok(Predicate::Cmp(col, cmp_op(*op)?, lit))
            }
            Op::Not | Op::Like => Err(LowerError::UnsupportedPredicate {
                what: format!("binary `{op:?}` is not a comparison"),
            }),
        },
        Expr::Unary { op: Op::Not, expr } => Ok(Predicate::Not(Box::new(lower_predicate(expr)?))),
        Expr::In { expr, set } => {
            let col = col_ref(expr)?;
            let lits = set.iter().map(literal).collect::<Result<Vec<_>, _>>()?;
            Ok(Predicate::In(col, lits))
        }
        Expr::Between { expr, low, high } => {
            let col = col_ref(expr)?;
            Ok(Predicate::Between(col, literal(low)?, literal(high)?))
        }
        Expr::Like { expr, pattern } => {
            let col = col_ref(expr)?;
            let Expr::Lit(AstLit::Str(p)) = pattern.as_ref() else {
                return Err(LowerError::UnsupportedPredicate {
                    what: "LIKE pattern must be a string literal".into(),
                });
            };
            Ok(Predicate::Like(col, Pattern(p.clone())))
        }
        other => Err(LowerError::UnsupportedPredicate {
            what: describe_expr(other),
        }),
    }
}

/// Lower a whole read [`Pipeline`] into a [`LogicalPlan`], threading the source schema.
/// `source_of` maps a `/driver/...` path to its [`SourceId`]; `schema_of` supplies
/// the leaf schema (the driver's pure `describe`, supplied by the caller so this crate
/// stays I/O-free). This is the AST → planner-IR bridge the pushdown pass runs over.
///
/// # Errors
/// [`LowerError`] if any `WHERE`/`ON`/projection cannot be lowered.
pub fn lower_query(
    pipeline: &Pipeline,
    source_of: &impl Fn(&[String]) -> SourceId,
    schema_of: &impl Fn(&SourceId) -> Schema,
) -> Result<LogicalPlan, LowerError> {
    let mut plan = lower_source(&pipeline.source, source_of, schema_of)?;
    for op in &pipeline.ops {
        plan = lower_op(plan, op, source_of, schema_of)?;
    }
    Ok(plan)
}

fn lower_source(
    source: &Source,
    source_of: &impl Fn(&[String]) -> SourceId,
    schema_of: &impl Fn(&SourceId) -> Schema,
) -> Result<LogicalPlan, LowerError> {
    match source {
        Source::Path(path) => {
            let segs: Vec<String> = path.segments.iter().map(|s| s.name.clone()).collect();
            let src = source_of(&segs);
            let schema = schema_of(&src);
            // Retain the full addressed VFS path so a read driver can navigate to the exact node
            // (t28) — INCLUDING each segment's `@version` (e.g. `/git/app@v1.2/…`) so a time-travel
            // read reaches the driver at the addressed ref instead of the latest. Source routing /
            // schema lookup key on names only (above); the addressed path keeps the ref.
            let mut vfs = String::new();
            for seg in &path.segments {
                vfs.push('/');
                vfs.push_str(&seg.name);
                if let Some(version) = &seg.version {
                    vfs.push('@');
                    vfs.push_str(version);
                }
            }
            Ok(LogicalPlan::scan_at(src, vfs, schema))
        }
        Source::Subquery(inner) => lower_query(inner, source_of, schema_of),
        Source::Values(_) => {
            // An inline VALUES relation has no driver source; model it as a synthetic
            // local source so the partitioner treats it as a (trivially) local leaf.
            let src = SourceId::new("(values)");
            Ok(LogicalPlan::scan(src, Schema::empty()))
        }
        Source::Name(name) => {
            // A `LET`-bound relation (M6, t60) is not a driver mount; the binding is folded
            // upstream by the evaluator. Model it as a synthetic local source so the
            // partitioner treats it as a local leaf with nothing to push into a driver.
            let src = SourceId::new(format!("(let:{name})"));
            Ok(LogicalPlan::scan(src, Schema::empty()))
        }
    }
}

fn lower_op(
    input: LogicalPlan,
    op: &PipeOp,
    source_of: &impl Fn(&[String]) -> SourceId,
    schema_of: &impl Fn(&SourceId) -> Schema,
) -> Result<LogicalPlan, LowerError> {
    match op {
        PipeOp::Where(e) => Ok(LogicalPlan::Filter {
            input: Box::new(input),
            predicate: lower_predicate(e)?,
        }),
        PipeOp::Select(projs) => {
            // A projection of only plain columns (`*`/`col`/`col AS a`) lowers to the pushable
            // name-only `Project`. A projection with any **computed** term (a struct/array
            // constructor, t92) lowers to the local `ProjectExpr` carrying the per-row exprs.
            if projs.iter().all(is_plain_projection) {
                Ok(LogicalPlan::Project {
                    input: Box::new(input),
                    columns: project_columns(projs)?,
                })
            } else {
                Ok(LogicalPlan::ProjectExpr {
                    input: Box::new(input),
                    projections: project_expr_terms(projs)?,
                })
            }
        }
        PipeOp::Limit(n) => Ok(LogicalPlan::Limit {
            input: Box::new(input),
            n: (*n).max(0) as u64,
        }),
        PipeOp::Distinct => Ok(LogicalPlan::Distinct {
            input: Box::new(input),
        }),
        PipeOp::OrderBy(keys) => {
            let keys = keys
                .iter()
                .map(|k| {
                    Ok(OrderKey {
                        column: col_name(&k.expr)?,
                        descending: k.descending,
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;
            Ok(LogicalPlan::Sort {
                input: Box::new(input),
                keys,
            })
        }
        PipeOp::GroupBy(exprs) => {
            // GROUP BY alone records the grouping columns; an empty aggregate set is a
            // valid grouped relation (the AGGREGATE op, if present, follows and merges).
            let group_by = exprs.iter().map(col_name).collect::<Result<Vec<_>, _>>()?;
            Ok(LogicalPlan::Aggregate {
                input: Box::new(input),
                group_by,
                aggregates: Vec::new(),
            })
        }
        PipeOp::Aggregate(projs) => {
            let aggregates = lower_aggregates(projs)?;
            // If the input is already a GROUP BY node, fold the aggregates onto it so a
            // `GROUP BY x |> AGGREGATE count(y)` is one Aggregate node.
            match input {
                LogicalPlan::Aggregate {
                    input,
                    group_by,
                    aggregates: existing,
                } if existing.is_empty() => Ok(LogicalPlan::Aggregate {
                    input,
                    group_by,
                    aggregates,
                }),
                other => Ok(LogicalPlan::Aggregate {
                    input: Box::new(other),
                    group_by: Vec::new(),
                    aggregates,
                }),
            }
        }
        PipeOp::Expand(field) => Ok(LogicalPlan::Expand {
            input: Box::new(input),
            field: field.last().cloned().unwrap_or_default(),
        }),
        PipeOp::Join(join) => {
            let rhs = lower_source(&join.source, source_of, schema_of)?;
            let on = lower_join_on(&join.on)?;
            Ok(LogicalPlan::Join {
                kind: JoinKind::Inner,
                lhs: Box::new(input),
                rhs: Box::new(rhs),
                on,
            })
        }
        PipeOp::Union(p) => set_op(input, p, SetKind::Union, source_of, schema_of),
        PipeOp::Except(p) => set_op(input, p, SetKind::Except, source_of, schema_of),
        PipeOp::Intersect(p) => set_op(input, p, SetKind::Intersect, source_of, schema_of),
        // EXTEND/SET add or overwrite columns with per-row computed values (t92): lowered to a
        // local `Extend` residual op carrying the assignments. Never pushed (a driver cannot
        // evaluate the constructor); the engine evaluates it after the scan returns.
        PipeOp::Extend(asgns) | PipeOp::Set(asgns) => {
            let assignments = asgns
                .iter()
                .map(|a| Ok((a.name.clone(), lower_scalar(&a.value)?)))
                .collect::<Result<Vec<_>, _>>()?;
            Ok(LogicalPlan::Extend {
                input: Box::new(input),
                assignments,
            })
        }
        // AS/DECODE/ENCODE/CALL are schema-shaping or effect-adjacent; out of the pushdown-split
        // scope. Keep the relation unchanged so the partition is still total.
        PipeOp::As(_) | PipeOp::Decode(_) | PipeOp::Encode(_) | PipeOp::Call(_) => Ok(input),
    }
}

fn set_op(
    lhs: LogicalPlan,
    rhs_pipe: &Pipeline,
    kind: SetKind,
    source_of: &impl Fn(&[String]) -> SourceId,
    schema_of: &impl Fn(&SourceId) -> Schema,
) -> Result<LogicalPlan, LowerError> {
    let rhs = lower_query(rhs_pipe, source_of, schema_of)?;
    Ok(LogicalPlan::SetOp {
        kind,
        lhs: Box::new(lhs),
        rhs: Box::new(rhs),
    })
}

fn lower_join_on(on: &Expr) -> Result<JoinOn, LowerError> {
    match on {
        Expr::Binary {
            op: Op::Eq,
            lhs,
            rhs,
        } => Ok(JoinOn::eq(col_name(lhs)?, col_name(rhs)?)),
        _ => Err(LowerError::UnsupportedJoinCondition),
    }
}

fn lower_aggregates(projs: &[Projection]) -> Result<Vec<Aggregate>, LowerError> {
    let mut out = Vec::new();
    for p in projs {
        let Projection::Expr { expr, alias } = p else {
            return Err(LowerError::UnsupportedProjection {
                what: "`*` is not an aggregate term".into(),
            });
        };
        let Expr::Fn(fnref) = expr else {
            return Err(LowerError::UnsupportedProjection {
                what: "AGGREGATE term must be an aggregate function call".into(),
            });
        };
        let func = aggregator(&fnref.name)?;
        let column = match fnref.args.first() {
            Some(Expr::Col(c)) => c.clone(),
            Some(Expr::Path(p)) => p.join("."),
            _ => "*".to_string(),
        };
        let output = alias
            .clone()
            .unwrap_or_else(|| format!("{}_{}", func.label(), column));
        out.push(Aggregate {
            func,
            column,
            output,
        });
    }
    Ok(out)
}

fn aggregator(name: &str) -> Result<Aggregator, LowerError> {
    match name.to_ascii_uppercase().as_str() {
        "COUNT" => Ok(Aggregator::Count),
        "SUM" => Ok(Aggregator::Sum),
        "MIN" => Ok(Aggregator::Min),
        "MAX" => Ok(Aggregator::Max),
        "ARRAY_AGG" => Ok(Aggregator::ArrayAgg),
        other => Err(LowerError::UnsupportedProjection {
            what: format!("unknown aggregate `{other}`"),
        }),
    }
}

/// Whether a projection term is a plain, pushable column (`*`, `col`, `col AS a`, or a
/// dotted path) — i.e. it names an existing column rather than computing a new value.
fn is_plain_projection(p: &Projection) -> bool {
    match p {
        Projection::Star => true,
        Projection::Expr { expr, .. } => matches!(expr, Expr::Col(_) | Expr::Path(_)),
    }
}

/// Lower a computed `SELECT` projection list into `(output name, ScalarExpr)` terms. A `*`
/// cannot be mixed with computed terms (there is no single output name for it), so it is a
/// structured [`LowerError`] here.
fn project_expr_terms(projs: &[Projection]) -> Result<Vec<(Name, ScalarExpr)>, LowerError> {
    let mut out = Vec::with_capacity(projs.len());
    for (i, p) in projs.iter().enumerate() {
        match p {
            Projection::Star => {
                return Err(LowerError::UnsupportedProjection {
                    what: "`*` cannot be mixed with a computed projection".into(),
                })
            }
            Projection::Expr { expr, alias } => {
                let name = alias.clone().unwrap_or_else(|| default_proj_name(expr, i));
                out.push((name, lower_scalar(expr)?));
            }
        }
    }
    Ok(out)
}

/// The default output column name for an un-aliased computed projection term: a bare column
/// keeps its name; a path keeps its dotted join; anything else is `exprN` (its position).
fn default_proj_name(expr: &Expr, idx: usize) -> Name {
    match expr {
        Expr::Col(c) => c.clone(),
        Expr::Path(segs) => segs.join("."),
        _ => format!("expr{idx}"),
    }
}

/// Lower a parser [`Expr`] into a per-row [`ScalarExpr`] (t92). Only the forms the engine can
/// evaluate with the type model alone are accepted: a column / path reference, a constant
/// literal (constant-folded to a [`Value`]), and the `[ ]`/`{ }` constructors (recursively).
/// A scalar `fn(...)` / operator expression is a structured [`LowerError`] (it would need the
/// stdlib registry the engine does not depend on).
fn lower_scalar(expr: &Expr) -> Result<ScalarExpr, LowerError> {
    match expr {
        // The lexer surfaces the keyword constants true/false/null as identifiers; in a value
        // position they are literals, not column references (mirrors the evaluator's VALUES rule).
        Expr::Col(name) => Ok(match name.to_ascii_lowercase().as_str() {
            "true" => ScalarExpr::Lit(Value::Bool(true)),
            "false" => ScalarExpr::Lit(Value::Bool(false)),
            "null" => ScalarExpr::Lit(Value::Null),
            _ => ScalarExpr::Col(ColRef::col(name.clone())),
        }),
        Expr::Path(segs) => Ok(ScalarExpr::Col(ColRef::path(segs.clone()))),
        Expr::Lit(lit) => Ok(ScalarExpr::Lit(lit_to_value(lit))),
        Expr::Array(elems) => Ok(ScalarExpr::Array(
            elems.iter().map(lower_scalar).collect::<Result<_, _>>()?,
        )),
        Expr::Struct(fields) => Ok(ScalarExpr::Struct(
            fields
                .iter()
                .map(|(n, e)| Ok((n.clone(), lower_scalar(e)?)))
                .collect::<Result<Vec<_>, LowerError>>()?,
        )),
        other => Err(LowerError::UnsupportedProjection {
            what: format!(
                "{} is not a per-row scalar expression (only columns, literals, and [ ]/{{ }} constructors)",
                describe_expr(other)
            ),
        }),
    }
}

/// Constant-fold a parser [`Literal`] into a runtime [`Value`] (the constant leaf of a
/// [`ScalarExpr`]). Mirrors the evaluator's `literal_to_value` lowering.
fn lit_to_value(lit: &AstLit) -> Value {
    match lit {
        AstLit::Str(s) => Value::Text(s.clone()),
        AstLit::Int(n) => Value::Int(*n),
        AstLit::Float(f) => Value::Float(*f),
        AstLit::Bool(b) => Value::Bool(*b),
        AstLit::Null => Value::Null,
        AstLit::Size { value, .. } => Value::Int(*value as i64),
        AstLit::Typed { raw, .. } => Value::Text(raw.clone()),
        AstLit::Bytes(b) => Value::Bytes(b.clone()),
    }
}

fn project_columns(projs: &[Projection]) -> Result<Vec<Name>, LowerError> {
    let mut out = Vec::new();
    for p in projs {
        match p {
            Projection::Star => {
                // `*` is represented as the empty projection meaning "all columns"; the
                // planner treats an empty projection as identity. Use a sentinel column.
                out.push("*".to_string());
            }
            Projection::Expr { expr, alias } => match expr {
                Expr::Col(c) => out.push(alias.clone().unwrap_or_else(|| c.clone())),
                Expr::Path(segs) => out.push(alias.clone().unwrap_or_else(|| segs.join("."))),
                _ => {
                    return Err(LowerError::UnsupportedProjection {
                        what: describe_expr(expr),
                    })
                }
            },
        }
    }
    Ok(out)
}

// ---- expression helpers ----

fn col_ref(expr: &Expr) -> Result<ColRef, LowerError> {
    match expr {
        Expr::Col(c) => Ok(ColRef::col(c.clone())),
        Expr::Path(segs) => Ok(ColRef::path(segs.clone())),
        other => Err(LowerError::UnsupportedPredicate {
            what: format!("expected a column, found {}", describe_expr(other)),
        }),
    }
}

fn col_name(expr: &Expr) -> Result<Name, LowerError> {
    match expr {
        Expr::Col(c) => Ok(c.clone()),
        Expr::Path(segs) => Ok(segs.join(".")),
        other => Err(LowerError::UnsupportedPredicate {
            what: format!("expected a column name, found {}", describe_expr(other)),
        }),
    }
}

fn literal(expr: &Expr) -> Result<Literal, LowerError> {
    match expr {
        Expr::Lit(AstLit::Str(s)) => Ok(Literal::Text(s.clone())),
        Expr::Lit(AstLit::Int(n)) => Ok(Literal::Int(*n)),
        Expr::Lit(AstLit::Float(f)) => Ok(Literal::Float(*f)),
        Expr::Lit(AstLit::Bool(b)) => Ok(Literal::Bool(*b)),
        Expr::Lit(AstLit::Null) => Ok(Literal::Null),
        other => Err(LowerError::UnsupportedPredicate {
            what: format!("expected a literal, found {}", describe_expr(other)),
        }),
    }
}

fn cmp_op(op: Op) -> Result<CmpOp, LowerError> {
    match op {
        Op::Eq => Ok(CmpOp::Eq),
        Op::Ne => Ok(CmpOp::Ne),
        Op::Lt => Ok(CmpOp::Lt),
        Op::Gt => Ok(CmpOp::Gt),
        Op::Le => Ok(CmpOp::Le),
        Op::Ge => Ok(CmpOp::Ge),
        Op::Match => Ok(CmpOp::Match),
        Op::And | Op::Or | Op::Not | Op::Like => Err(LowerError::UnsupportedPredicate {
            what: format!("`{op:?}` is not a comparison operator"),
        }),
    }
}

fn describe_expr(expr: &Expr) -> String {
    match expr {
        Expr::Lit(_) => "literal".into(),
        Expr::Col(c) => format!("column `{c}`"),
        Expr::Path(p) => format!("path `{}`", p.join(".")),
        Expr::Fn(f) => format!("fn `{}`", f.name),
        Expr::Binary { op, .. } => format!("binary `{op:?}`"),
        Expr::Unary { op, .. } => format!("unary `{op:?}`"),
        Expr::In { .. } => "IN".into(),
        Expr::Between { .. } => "BETWEEN".into(),
        Expr::Like { .. } => "LIKE".into(),
        Expr::AnyOp { .. } => "ANY".into(),
        // A lambda is a value, never a backend-pushable predicate (M6 t61) — it surfaces
        // here only as the secret-free label of an unsupported predicate shape.
        Expr::Lambda { .. } => "lambda".into(),
        // Composite constructors (t92): valid in a projection (lowered to a `ScalarExpr`),
        // never a backend-pushable predicate — labelled here for the unsupported case.
        Expr::Array(_) => "array".into(),
        Expr::Struct(_) => "struct".into(),
    }
}
