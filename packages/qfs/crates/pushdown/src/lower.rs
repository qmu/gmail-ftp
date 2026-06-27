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
use qfs_types::{CmpOp, ColRef, Literal, Name, Pattern, Predicate, Schema};

use crate::logical::{
    Aggregate, Aggregator, JoinKind, JoinOn, LogicalPlan, OrderKey, SetKind, SourceId,
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
            // Retain the full addressed VFS path (`/seg/seg`) so a read driver can navigate to
            // the exact node (t28), not just the mount root.
            let vfs = format!("/{}", segs.join("/"));
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
        PipeOp::Select(projs) => Ok(LogicalPlan::Project {
            input: Box::new(input),
            columns: project_columns(projs)?,
        }),
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
        // EXTEND/SET/AS/DECODE/ENCODE/CALL are schema-shaping or effect-adjacent; they
        // are out of the pushdown-split scope (the ticket's FROM/WHERE/SELECT/EXTEND/
        // AGGREGATE/JOIN/UNION/EXCEPT/INTERSECT/EXPAND subset treats EXTEND as a local
        // pass-through). Keep the relation unchanged so the partition is still total.
        PipeOp::Extend(_)
        | PipeOp::Set(_)
        | PipeOp::As(_)
        | PipeOp::Decode(_)
        | PipeOp::Encode(_)
        | PipeOp::Call(_) => Ok(input),
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
        other => Err(LowerError::UnsupportedProjection {
            what: format!("unknown aggregate `{other}`"),
        }),
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
    }
}
