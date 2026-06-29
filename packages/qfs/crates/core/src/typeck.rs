//! The **static primitive type checker**, run at **plan time** (RFD-0001 §3 purity
//! invariant, §5 AI-facing structured errors; roadmap **decision T**, ticket t75).
//!
//! This is a pure, before-any-I/O pass over the parser [`Expr`] tree: it infers a [`Ty`]
//! for every expression, checks the operand types of comparisons / predicates, the static
//! argument types of built-in / stdlib calls, and the parameter/body types of lambdas
//! (including the element type a `map`/`filter`/`reduce` applies a lambda over). A mismatch
//! — a string compared to an `i64` column, a lambda annotated `(x: string)` applied to a
//! collection of ints, `UPPER` handed a number — surfaces as a structured, secret-free
//! [`EvalError`] during **`preview`/plan building**, never as a surprise mid-commit.
//!
//! ## Where it runs (the purity floor is untouched)
//! The checker is wired into the pure plan pass ([`crate::eval::Evaluator`]) wherever an
//! expression rides a stage whose input [`Schema`] is known: today the `WHERE` filter of a
//! read pipeline and the filter of a `SET … WHERE` effect. Column types come from each
//! driver's pure `describe` contract, so a whole pipeline type-checks **offline** before it
//! touches a credential — and a plan that fails the check never reaches the applier (a
//! type-failing plan can never be committed). The checker performs **no I/O**, allocates no
//! effect node, and reads only the read-only [`StdlibRegistry`] for call typing — so the §3
//! purity invariant and the effect semantics are unchanged; this is a *check added to* the
//! plan pass, not a new effect.
//!
//! ## Scope (this slice)
//! Literal + column + comparison + predicate (`IN`/`BETWEEN`/`LIKE`/`ANY`) typing, built-in
//! argument typing against the [`FnSig::arg_types`](crate::FnSig) contract, and lambda
//! parameter/body checking using t61's retained [`TypeAnn`](qfs_parser::TypeAnn). Full
//! let-polymorphic inference (inferring an *unannotated* lambda parameter from its use site
//! across a whole program) is deliberately **out of scope** — an unannotated parameter binds
//! at the element type a `map`/`filter`/`reduce` supplies, otherwise stays late-bound
//! (`Unknown`), so the check is conservative and never *false-rejects* a well-typed program.

use std::collections::HashMap;

use qfs_parser::{Expr, FnRef, Literal, Op};
use qfs_types::{CmpOp, ColumnType, Schema, TypeError};

use crate::eval::EvalError;
use crate::stdlib::{BuiltinEval, FnError, StdlibRegistry};

/// The static type lattice of the checker (decision T): the primitive [`ColumnType`]s of
/// `qfs-types` (`bool`/`i64`/`f64`/`string`/…, plus the late-bound `Unknown`/`Json`), a
/// **function** type (a lambda is a first-class value, decision H), and the **`Resource`**
/// value (t73). Functions and `Resource` are CamelCase, primitives lowercase — matching the
/// surface naming the roadmap settled on.
///
/// The function/`Resource` cases live here (not in `qfs-types`) because they are not column
/// types — a relation column is never a closure — so the leaf type crate stays free of the
/// closure/parser notion while the checker still reasons about both uniformly.
#[derive(Debug, Clone, PartialEq)]
pub enum Ty {
    /// A primitive / column type (`bool`, `i64`, `f64`, `string`, `Date`, …, or the
    /// late-bound `Unknown`/`Json`).
    Prim(ColumnType),
    /// A function (lambda) type: the parameter types and the body's result type.
    Fn {
        /// The parameter types, in order.
        params: Vec<Ty>,
        /// The body's inferred result type.
        ret: Box<Ty>,
    },
    /// The opaque `Resource` value (t73 resource literal). Not a column type; carried so the
    /// lattice is closed over every first-class value a lambda parameter may be annotated as.
    Resource,
}

impl Ty {
    /// The late-bound primitive (`Unknown`): the conservative top used for a column from an
    /// undescribable source, a `Null` literal, or an unrecognised type annotation.
    #[must_use]
    pub fn unknown() -> Self {
        Ty::Prim(ColumnType::Unknown)
    }

    /// The underlying [`ColumnType`] if this is a primitive, else `None` (a function /
    /// `Resource` is not a column type).
    #[must_use]
    pub fn as_prim(&self) -> Option<&ColumnType> {
        match self {
            Ty::Prim(ct) => Some(ct),
            Ty::Fn { .. } | Ty::Resource => None,
        }
    }
}

/// The lexical type environment: a name → [`Ty`] binding for lambda parameters in scope.
/// Cheap to extend by value ([`TyEnv::bind`]) so each lambda body type-checks under its own
/// immutable child env (mirrors the value evaluator's [`ValueEnv`](crate::ValueEnv)).
#[derive(Debug, Clone, Default)]
pub struct TyEnv {
    vars: HashMap<String, Ty>,
}

impl TyEnv {
    /// An empty type environment (no parameters in scope).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A child env with `name` bound to `ty` (shadowing any same-named outer binding).
    #[must_use]
    pub fn bind(&self, name: impl Into<String>, ty: Ty) -> Self {
        let mut vars = self.vars.clone();
        vars.insert(name.into(), ty);
        Self { vars }
    }

    /// The type bound to `name`, if it is a parameter in scope.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&Ty> {
        self.vars.get(name)
    }
}

/// Type-check an expression at plan time, returning its inferred [`Ty`] (decision T).
///
/// Column references resolve against `schema` (from the driver's pure `describe`); lambda
/// parameters resolve against `env`; `fn(...)` calls type against `stdlib` (when wired —
/// `None` keeps the call late-bound, the t07 behaviour). The check is **total and pure**:
/// every failure is a structured [`EvalError`] (a [`TypeError`] for a comparison mismatch, a
/// [`FnError`] for a call mismatch), never a panic and never any I/O.
///
/// # Errors
/// [`EvalError::Type`] for an incomparable comparison / predicate operand, or
/// [`EvalError::Fn`] for an unknown function, a bad static argument type, or an aggregate
/// used outside an `AGGREGATE` context.
pub fn check_expr(
    expr: &Expr,
    env: &TyEnv,
    schema: &Schema,
    stdlib: Option<&StdlibRegistry>,
) -> Result<Ty, EvalError> {
    match expr {
        Expr::Lit(lit) => Ok(Ty::Prim(literal_type(lit))),
        // A bare identifier is a lambda parameter (env), a `true`/`false`/`null` literal word
        // the lexer surfaces lowercase as an identifier, or a column resolved against the
        // schema. An unresolved column stays late-bound (`Unknown`) rather than erroring here
        // — projection is where an unknown column is a hard error (t05); a `WHERE` over an
        // undescribable column degrades to late-bound, preserving the pre-t75 leniency.
        Expr::Col(name) => {
            if let Some(ty) = env.get(name) {
                return Ok(ty.clone());
            }
            Ok(match name.to_ascii_lowercase().as_str() {
                "true" | "false" => Ty::Prim(ColumnType::Bool),
                "null" => Ty::unknown(),
                _ => Ty::Prim(column_type(name, schema)),
            })
        }
        // Struct navigation `a.b.c` resolves against the schema; an unresolvable path is
        // late-bound (`Unknown`), matching the `Json`/`Unknown` late-binding of t05.
        Expr::Path(segs) => Ok(Ty::Prim(
            schema.resolve_path(segs).unwrap_or(ColumnType::Unknown),
        )),
        Expr::Fn(fnref) => check_fn(fnref, env, schema, stdlib),
        Expr::Lambda { params, body } => {
            // Each parameter binds at its annotation (if present, t61's retained `TypeAnn`),
            // else stays late-bound; the body type-checks under those bindings. This is the
            // enforcement of an annotated lambda parameter — a body that misuses the declared
            // type (`(x: i64) => x ~ 'p'`) fails right here.
            let mut lenv = env.clone();
            let mut param_tys = Vec::with_capacity(params.len());
            for p in params {
                let ty = param_type(p.ty.as_ref().map(|a| a.name.as_str()));
                lenv = lenv.bind(p.name.clone(), ty.clone());
                param_tys.push(ty);
            }
            let ret = check_expr(body, &lenv, schema, stdlib)?;
            Ok(Ty::Fn {
                params: param_tys,
                ret: Box::new(ret),
            })
        }
        Expr::Binary { op, lhs, rhs } => check_binary(*op, lhs, rhs, env, schema, stdlib),
        Expr::Unary { op: _, expr } => {
            // `NOT <expr>` — type-check the operand to surface nested errors; the result is a
            // boolean predicate.
            check_expr(expr, env, schema, stdlib)?;
            Ok(Ty::Prim(ColumnType::Bool))
        }
        Expr::In { expr, set } | Expr::AnyOp { expr, set, .. } => {
            // `<e> IN (<set>)` / `<e> <op> ANY (<set>)`: every set member must be
            // equality-comparable to the tested expression.
            let lt = check_expr(expr, env, schema, stdlib)?;
            for member in set {
                let rt = check_expr(member, env, schema, stdlib)?;
                require_comparable(CmpOp::Eq, &lt, &rt)?;
            }
            Ok(Ty::Prim(ColumnType::Bool))
        }
        Expr::Between { expr, low, high } => {
            let lt = check_expr(expr, env, schema, stdlib)?;
            let lo = check_expr(low, env, schema, stdlib)?;
            let hi = check_expr(high, env, schema, stdlib)?;
            require_comparable(CmpOp::Ge, &lt, &lo)?;
            require_comparable(CmpOp::Le, &lt, &hi)?;
            Ok(Ty::Prim(ColumnType::Bool))
        }
        Expr::Like { expr, pattern } => {
            // `LIKE` requires a `Text` (or late-bound) operand on both sides.
            let lt = check_expr(expr, env, schema, stdlib)?;
            let rt = check_expr(pattern, env, schema, stdlib)?;
            require_comparable(CmpOp::Match, &lt, &rt)?;
            Ok(Ty::Prim(ColumnType::Bool))
        }
    }
}

/// Type-check a binary expression. Logical `AND`/`OR` recurse to surface nested errors and
/// yield a boolean; every comparison (`==`/`<>`/`<`/`>`/`<=`/`>=`/`~`) checks operand
/// comparability via the [`require_comparable`] matrix and yields a boolean.
fn check_binary(
    op: Op,
    lhs: &Expr,
    rhs: &Expr,
    env: &TyEnv,
    schema: &Schema,
    stdlib: Option<&StdlibRegistry>,
) -> Result<Ty, EvalError> {
    let lt = check_expr(lhs, env, schema, stdlib)?;
    let rt = check_expr(rhs, env, schema, stdlib)?;
    match op {
        // Logical structure: operands already checked; the result is a predicate.
        Op::And | Op::Or | Op::Not => Ok(Ty::Prim(ColumnType::Bool)),
        Op::Eq | Op::Ne | Op::Lt | Op::Gt | Op::Le | Op::Ge | Op::Like | Op::Match => {
            let cmp = cmp_op(op);
            require_comparable(cmp, &lt, &rt)?;
            Ok(Ty::Prim(ColumnType::Bool))
        }
    }
}

/// Type-check a `fn(...)` call (decision T). Arity is checked first (t08), then either:
/// higher-order application (`map`/`filter`/`reduce`) is checked against the collection's
/// element type, or each static argument-type contract ([`FnSig::arg_types`]) is enforced.
/// An aggregate used in scalar/predicate position is the t08 structured error. A receiver
/// alias (driver prelude) stays late-bound. The result is the built-in's declared return
/// type.
fn check_fn(
    fnref: &FnRef,
    env: &TyEnv,
    schema: &Schema,
    stdlib: Option<&StdlibRegistry>,
) -> Result<Ty, EvalError> {
    let Some(reg) = stdlib else {
        // No registry wired: keep the call late-bound (t07 behaviour). Still type-check the
        // argument subexpressions so a comparison mismatch nested in an argument is caught.
        for arg in &fnref.args {
            check_expr(arg, env, schema, None)?;
        }
        return Ok(Ty::unknown());
    };
    let name = fnref.name.as_str();
    let Some(builtin) = reg.builtin(name) else {
        // Not a core built-in. A receiver-scoped prelude alias is late-bound here (its
        // typing is t06's receiver concern); anything else is an unknown function.
        if reg.alias_providers(name).is_empty() {
            return Err(FnError::UnknownFunction {
                name: name.to_string(),
            }
            .into());
        }
        for arg in &fnref.args {
            check_expr(arg, env, schema, stdlib)?;
        }
        return Ok(Ty::unknown());
    };
    if !builtin.sig.accepts_arity(fnref.args.len()) {
        return Err(FnError::Arity {
            name: name.to_string(),
            expected: builtin.sig.min_args,
            found: fnref.args.len(),
        }
        .into());
    }
    // Higher-order built-ins apply a lambda over a collection — checked specially.
    if builtin.higher_order_kind().is_some() {
        return check_higher_order(fnref, env, schema, reg);
    }
    // An aggregate in scalar/predicate position is a typed misuse (RFD §3 dispatch).
    if matches!(builtin.eval, BuiltinEval::Aggregate(_)) {
        return Err(FnError::AggregateOutsideAggregate {
            name: name.to_string(),
        }
        .into());
    }
    // Static per-argument type contract (t75): a statically-known mismatch is a plan-time
    // error, before any I/O.
    for (i, arg) in fnref.args.iter().enumerate() {
        let arg_ty = check_expr(arg, env, schema, stdlib)?;
        if let Some(Some(expected)) = builtin.sig.arg_types.get(i) {
            require_assignable(name, expected, &arg_ty)?;
        }
    }
    Ok(Ty::Prim(builtin.sig.returns.clone()))
}

/// Type-check a higher-order application (`map`/`filter`/`reduce`, t61) against the static
/// element type of its collection argument (decision T). The collection's element type binds
/// the lambda's element parameter; an annotated parameter whose declared type cannot accept
/// that element is the "lambda applied to the wrong argument type" rejection. The body then
/// type-checks under the bound parameters.
fn check_higher_order(
    fnref: &FnRef,
    env: &TyEnv,
    schema: &Schema,
    reg: &StdlibRegistry,
) -> Result<Ty, EvalError> {
    let name = fnref.name.as_str();
    // arg[0] is the collection; its element type (or late-bound `Unknown`).
    let coll_ty = check_expr(&fnref.args[0], env, schema, Some(reg))?;
    let elem = match coll_ty.as_prim() {
        Some(ColumnType::Array(e)) => Ty::Prim((**e).clone()),
        _ => Ty::unknown(),
    };
    let is_reduce = name.eq_ignore_ascii_case("reduce");
    match &fnref.args[1] {
        Expr::Lambda { params, body } => {
            // `reduce` folds `(acc, element)`; `map`/`filter` apply `(element)`. Locate the
            // element parameter (the last one) and check the collection element is assignable
            // to its annotation.
            let elem_idx = if is_reduce { 1 } else { 0 };
            let mut lenv = env.clone();
            let mut param_tys = Vec::with_capacity(params.len());
            for (i, p) in params.iter().enumerate() {
                let declared = param_type(p.ty.as_ref().map(|a| a.name.as_str()));
                // The element parameter must accept the collection's element type.
                if i == elem_idx {
                    if let (Some(want), Some(got)) = (declared.as_prim(), elem.as_prim()) {
                        if !assignable(want, got) {
                            return Err(FnError::Type {
                                name: name.to_string(),
                                expected: type_label(want),
                                found: type_label(got),
                            }
                            .into());
                        }
                    }
                }
                lenv = lenv.bind(p.name.clone(), declared.clone());
                param_tys.push(declared);
            }
            let ret = check_expr(body, &lenv, schema, Some(reg))?;
            // `map` yields a collection of the body type; `filter` preserves the collection;
            // `reduce` yields the fold's accumulator (the body) type.
            Ok(match name {
                _ if name.eq_ignore_ascii_case("map") => match ret.as_prim() {
                    Some(ct) => Ty::Prim(ColumnType::Array(Box::new(ct.clone()))),
                    None => Ty::Prim(ColumnType::Array(Box::new(ColumnType::Unknown))),
                },
                _ if name.eq_ignore_ascii_case("filter") => coll_ty,
                _ => ret,
            })
        }
        // The function argument is not a literal lambda (e.g. a bound name): late-bound, but
        // still type-check it to surface a non-function misuse early.
        other => {
            check_expr(other, env, schema, Some(reg))?;
            Ok(Ty::unknown())
        }
    }
}

/// The primitive [`ColumnType`] of a parser [`Literal`] (decision T). `Null` is late-bound
/// (`Unknown`) — a bare null carries no type (RFD §4); a `Size` is an integer magnitude; a
/// typed literal (`DATE '…'`) is its introducer's type.
fn literal_type(lit: &Literal) -> ColumnType {
    match lit {
        Literal::Str(_) => ColumnType::Text,
        Literal::Int(_) => ColumnType::Int,
        Literal::Float(_) => ColumnType::Float,
        Literal::Bool(_) => ColumnType::Bool,
        Literal::Null => ColumnType::Unknown,
        Literal::Size { .. } => ColumnType::Int,
        Literal::Typed { ty, .. } => match ty.to_ascii_uppercase().as_str() {
            "DATE" => ColumnType::Date,
            "TIMESTAMP" | "TIME" => ColumnType::Timestamp,
            _ => ColumnType::Unknown,
        },
    }
}

/// The primitive type of a lambda parameter type annotation (t61's retained [`TypeAnn`]).
/// The canonical surface is lowercase, Rust-style: `bool`, fixed-width ints
/// (`i32`/`i64`/`u32`/`u64`), floats (`f32`/`f64`), `string` — beside the temporal/`bytes`
/// column types and the CamelCase `Resource`. An unrecognised annotation (e.g. `Row`) is
/// conservatively late-bound so the checker never *false-rejects* an unmodelled annotation.
fn param_type(annotation: Option<&str>) -> Ty {
    let Some(name) = annotation else {
        return Ty::unknown();
    };
    match name {
        "bool" => Ty::Prim(ColumnType::Bool),
        "i32" | "i64" | "u32" | "u64" | "int" => Ty::Prim(ColumnType::Int),
        "f32" | "f64" | "float" => Ty::Prim(ColumnType::Float),
        "string" | "text" => Ty::Prim(ColumnType::Text),
        "bytes" => Ty::Prim(ColumnType::Bytes),
        "decimal" => Ty::Prim(ColumnType::Decimal),
        "date" => Ty::Prim(ColumnType::Date),
        "timestamp" => Ty::Prim(ColumnType::Timestamp),
        "uuid" => Ty::Prim(ColumnType::Uuid),
        "Resource" | "resource" => Ty::Resource,
        // An unrecognised / structural annotation (`Row`, a struct name) is late-bound.
        _ => Ty::unknown(),
    }
}

/// Resolve a bare column name against the schema, late-binding (`Unknown`) when the schema is
/// itself late-bound (empty / undescribable) or the column is absent — the conservative
/// posture that never false-rejects a column from a driver that does not (yet) describe it.
fn column_type(name: &str, schema: &Schema) -> ColumnType {
    if schema.columns.is_empty() {
        return ColumnType::Unknown;
    }
    schema
        .column(name)
        .map_or(ColumnType::Unknown, |c| c.ty.clone())
}

/// Map a parser [`Op`] to the type-model [`CmpOp`] for the comparability matrix. Logical
/// operators are not comparisons and never reach here.
fn cmp_op(op: Op) -> CmpOp {
    match op {
        Op::Eq => CmpOp::Eq,
        Op::Ne => CmpOp::Ne,
        Op::Lt => CmpOp::Lt,
        Op::Gt => CmpOp::Gt,
        Op::Le => CmpOp::Le,
        Op::Ge => CmpOp::Ge,
        Op::Like | Op::Match => CmpOp::Match,
        // Logical operators are handled structurally in `check_binary`, never mapped.
        Op::And | Op::Or | Op::Not => CmpOp::Eq,
    }
}

/// Require two operands be comparable under `op`, or raise [`TypeError::IncomparableTypes`]
/// (decision T's headline rejection: `where total == 'paid'` against an `i64` column). A
/// function / `Resource` operand in a comparison is never comparable to a primitive.
fn require_comparable(op: CmpOp, lhs: &Ty, rhs: &Ty) -> Result<(), EvalError> {
    match (lhs.as_prim(), rhs.as_prim()) {
        (Some(l), Some(r)) if comparable(op, l, r) => Ok(()),
        (Some(l), Some(r)) => Err(TypeError::IncomparableTypes {
            op,
            lhs: l.clone(),
            rhs: r.clone(),
        }
        .into()),
        // A function / Resource operand against a primitive is not comparable.
        _ => Err(TypeError::IncomparableTypes {
            op,
            lhs: lhs.as_prim().cloned().unwrap_or(ColumnType::Unknown),
            rhs: rhs.as_prim().cloned().unwrap_or(ColumnType::Unknown),
        }
        .into()),
    }
}

/// The comparability matrix (decision T), mirroring [`qfs_types`]'s predicate rules:
/// - `Unknown`/`Json` are comparable to anything (late-bound, resolved at runtime);
/// - `~` (`Match`) requires both sides be `Text`;
/// - numeric (`Int`/`Float`/`Decimal`) and temporal (`Timestamp`/`Date`) widen within group;
/// - equality additionally holds between identical scalars; ordering between identical
///   orderable scalars.
fn comparable(op: CmpOp, lhs: &ColumnType, rhs: &ColumnType) -> bool {
    use ColumnType::{Date, Decimal, Float, Int, Text, Timestamp};

    if matches!(lhs, ColumnType::Unknown | ColumnType::Json)
        || matches!(rhs, ColumnType::Unknown | ColumnType::Json)
    {
        return true;
    }
    if op == CmpOp::Match {
        return matches!(lhs, Text) && matches!(rhs, Text);
    }
    let numeric = matches!(lhs, Int | Float | Decimal) && matches!(rhs, Int | Float | Decimal);
    let temporal = matches!(lhs, Timestamp | Date) && matches!(rhs, Timestamp | Date);
    if op.is_ordering() {
        numeric
            || temporal
            || (lhs == rhs && matches!(lhs, Text | Timestamp | Date | Decimal | Int | Float))
    } else {
        numeric || temporal || (lhs == rhs && lhs.is_scalar())
    }
}

/// Require an argument's inferred type be assignable to a built-in's declared parameter type
/// (decision T), or raise [`FnError::Type`] (`UPPER(<i64 column>)`). A function / `Resource`
/// argument where a primitive is expected is rejected.
fn require_assignable(fn_name: &str, expected: &ColumnType, actual: &Ty) -> Result<(), EvalError> {
    match actual.as_prim() {
        Some(got) if assignable(expected, got) => Ok(()),
        Some(got) => Err(FnError::Type {
            name: fn_name.to_string(),
            expected: type_label(expected),
            found: type_label(got),
        }
        .into()),
        None => Err(FnError::Type {
            name: fn_name.to_string(),
            expected: type_label(expected),
            found: "Function",
        }
        .into()),
    }
}

/// Whether a value of type `actual` is assignable where `expected` is wanted: a late-bound
/// (`Unknown`/`Json`) side defers to runtime, an exact match holds, and numeric widening
/// (`Int`↔`Float`↔`Decimal`) is allowed.
fn assignable(expected: &ColumnType, actual: &ColumnType) -> bool {
    use ColumnType::{Decimal, Float, Int};
    if matches!(expected, ColumnType::Unknown | ColumnType::Json)
        || matches!(actual, ColumnType::Unknown | ColumnType::Json)
    {
        return true;
    }
    if expected == actual {
        return true;
    }
    matches!(expected, Int | Float | Decimal) && matches!(actual, Int | Float | Decimal)
}

/// A stable, secret-free `'static` label for a [`ColumnType`] (for the structured
/// [`FnError::Type`]).
fn type_label(ct: &ColumnType) -> &'static str {
    match ct {
        ColumnType::Bool => "Bool",
        ColumnType::Int => "Int",
        ColumnType::Float => "Float",
        ColumnType::Decimal => "Decimal",
        ColumnType::Text => "Text",
        ColumnType::Bytes => "Bytes",
        ColumnType::Timestamp => "Timestamp",
        ColumnType::Date => "Date",
        ColumnType::Uuid => "Uuid",
        ColumnType::Struct(_) => "Struct",
        ColumnType::Array(_) => "Array",
        ColumnType::Json => "Json",
        ColumnType::Unknown => "Unknown",
        // `ColumnType` is `#[non_exhaustive]`: a future variant reports an honest fallback
        // rather than failing to compile (lib code stays total).
        _ => "Unknown",
    }
}

#[cfg(test)]
mod tests;
