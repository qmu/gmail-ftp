//! The **pure lambda evaluator** (RFD-0001 §3 purity invariant, M6 ticket t61): the value
//! engine that gives the closed core's "functions are values" promise (decision H) teeth.
//!
//! Where [`crate::eval`] is the *typing / plan-building* pass (a statement folds into a
//! relation description or an effect [`Plan`](qfs_plan::Plan), never row values), THIS module
//! is the small, in-process **value** evaluator a lambda needs: it turns an
//! [`Expr::Lambda`](qfs_parser::Expr) into a first-class [`Closure`] value, **applies** a
//! closure (binding its parameters and evaluating its body), and implements the higher-order
//! builtins `map` / `filter` / `reduce` over an in-memory collection.
//!
//! ## Closures capture the binding environment (why t61 depends on t60)
//! A [`Closure`] captures the [`ValueEnv`] in force where the lambda literal appears, so a
//! lambda closes over the surrounding `LET`-bound values (`LET suffix = '!' ; … (x) =>
//! concat(x, suffix)`). The capture is by value into an immutable child env, mirroring the
//! t60 lexical-scoping discipline of the `eval`/`resolve` passes.
//!
//! ## Purity (the safety property, RFD §3)
//! Every function here is **pure**: it maps values to values, reads only the read-only
//! [`StdlibRegistry`] + frozen [`EvalCtx`], and performs **no** I/O. A lambda body is
//! expression-only — it can name a scalar builtin or a higher-order builtin, never an effect —
//! so `map`/`filter`/`reduce` over a collection construct no effect node and the safety floor
//! (describe pure / preview touches nothing / commit explicit) is untouched. Errors are the
//! structured, AI-consumable [`EvalError`] arms (`LambdaArity`, `NotAFunction`, `Fn`,
//! `Resolve`), never a panic.

use std::collections::HashMap;

use qfs_parser::{Expr, Literal, Op};
use qfs_types::Value;

use crate::eval::EvalError;
use crate::resolve::ResolveError;
use crate::stdlib::{BuiltinEval, EvalCtx, FnError, HigherOrderKind, StdlibRegistry};

/// A value the lambda evaluator produces: either a plain data [`Value`] or a [`Closure`]
/// (a lambda is a first-class value, decision H). A closure is **not** a [`Value`] — that
/// would pull the parser AST into the leaf `qfs-types` crate and break the acyclic spine —
/// so the function-value lives in this core-local union instead.
#[derive(Debug, Clone, PartialEq)]
pub enum LambdaValue {
    /// A plain data value (the result of a literal, a scalar builtin, a `map`/`filter`, …).
    Data(Value),
    /// A function value — a lambda captured with its environment.
    Closure(Closure),
}

impl LambdaValue {
    /// Extract the plain data [`Value`], or a structured [`EvalError::NotAFunction`] if this
    /// is a closure used where a value is required (e.g. a lambda passed to `UPPER`).
    ///
    /// # Errors
    /// [`EvalError::NotAFunction`] when the value is a [`Closure`].
    pub fn into_data(self) -> Result<Value, EvalError> {
        match self {
            LambdaValue::Data(v) => Ok(v),
            LambdaValue::Closure(_) => Err(EvalError::NotAFunction {
                detail: "a lambda value was used where a plain value is required".to_string(),
            }),
        }
    }

    /// Extract the [`Closure`], or a structured [`EvalError::NotAFunction`] if this is a plain
    /// value used in function position (e.g. `map(coll, 3)`).
    ///
    /// # Errors
    /// [`EvalError::NotAFunction`] when the value is plain data, not a function.
    pub fn into_closure(self) -> Result<Closure, EvalError> {
        match self {
            LambdaValue::Closure(c) => Ok(c),
            LambdaValue::Data(v) => Err(EvalError::NotAFunction {
                detail: format!(
                    "expected a function (lambda) but got a {} value",
                    type_label(&v)
                ),
            }),
        }
    }
}

/// A function value: a lambda's parameter names + body, captured with the [`ValueEnv`] in
/// force where the lambda literal appeared (so it closes over outer `LET` bindings).
#[derive(Debug, Clone, PartialEq)]
pub struct Closure {
    /// The parameter names bound when the closure is applied.
    params: Vec<String>,
    /// The body expression, evaluated with the params (and the captured env) in scope.
    body: Expr,
    /// The environment captured at the lambda literal (lexical, by-value capture).
    captured: ValueEnv,
}

impl Closure {
    /// The number of parameters this closure declares (its arity).
    #[must_use]
    pub fn arity(&self) -> usize {
        self.params.len()
    }
}

/// The lexical value environment: a name → [`LambdaValue`] binding map. Cheap to extend by
/// value ([`ValueEnv::bind`]) so each application/`LET` gets its own immutable child env —
/// shadowing is a plain re-insert and the parent env is never mutated (mirrors the t60
/// scoping discipline). A bound value is data or a closure, never a secret (purity floor).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ValueEnv {
    vars: HashMap<String, LambdaValue>,
}

impl ValueEnv {
    /// An empty environment.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A child env with `name` bound to `value` (shadowing any same-named outer binding).
    #[must_use]
    pub fn bind(&self, name: impl Into<String>, value: LambdaValue) -> Self {
        let mut vars = self.vars.clone();
        vars.insert(name.into(), value);
        Self { vars }
    }

    /// The value bound to `name`, if it is in scope.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&LambdaValue> {
        self.vars.get(name)
    }
}

/// Evaluate an expression to a [`LambdaValue`] under `env` (M6, ticket t61). Pure — reads only
/// the read-only `stdlib` + frozen `ctx`, performs no I/O.
///
/// Supported forms are the **expression-only** surface a pure lambda body uses: literals,
/// variable / struct-path references (resolved against `env`), nested lambdas (which produce
/// closures), scalar builtin calls, the higher-order builtins `map`/`filter`/`reduce`, and the
/// boolean/comparison operators (so a `filter` predicate like `(x) => x > 2` evaluates). An
/// effect can never be named here (a lambda is pure, RFD §3); an unsupported body form is a
/// structured [`EvalError`], never a panic.
///
/// # Errors
/// [`EvalError`] for an unbound name, a wrong-arity application/builtin call, a value used in
/// function position (or vice versa), or an unsupported body form.
pub fn eval_expr(
    expr: &Expr,
    env: &ValueEnv,
    stdlib: &StdlibRegistry,
    ctx: &EvalCtx,
) -> Result<LambdaValue, EvalError> {
    match expr {
        Expr::Lit(lit) => Ok(LambdaValue::Data(literal_to_value(lit))),
        // A bare identifier is a variable reference resolved against the environment. The
        // lexer surfaces the literal words true/false/null as identifiers — honour them.
        Expr::Col(name) => match env.get(name) {
            Some(v) => Ok(v.clone()),
            None => match name.to_ascii_lowercase().as_str() {
                "true" => Ok(LambdaValue::Data(Value::Bool(true))),
                "false" => Ok(LambdaValue::Data(Value::Bool(false))),
                "null" => Ok(LambdaValue::Data(Value::Null)),
                _ => Err(EvalError::Resolve(ResolveError::UnknownBinding {
                    name: name.clone(),
                })),
            },
        },
        // Struct navigation `a.b.c`: resolve the head var, then walk named struct fields.
        Expr::Path(segs) => eval_path(segs, env),
        // A lambda literal becomes a closure capturing the current environment.
        Expr::Lambda { params, body } => Ok(LambdaValue::Closure(Closure {
            params: params.iter().map(|p| p.name.clone()).collect(),
            body: (**body).clone(),
            captured: env.clone(),
        })),
        Expr::Fn(fnref) => eval_fn(&fnref.name, &fnref.args, env, stdlib, ctx),
        Expr::Binary { op, lhs, rhs } => eval_binary(*op, lhs, rhs, env, stdlib, ctx),
        Expr::Unary { op: Op::Not, expr } => {
            let v = eval_expr(expr, env, stdlib, ctx)?.into_data()?;
            Ok(LambdaValue::Data(Value::Bool(!is_truthy(&v))))
        }
        // Any remaining form (IN / BETWEEN / LIKE / ANY / arithmetic) is outside the small
        // pure expression surface a lambda body uses this slice — a structured error, never a
        // panic. (Arithmetic is not even in the frozen operator set, RFD §3.)
        _ => Err(EvalError::Fn(FnError::Domain {
            name: "lambda".to_string(),
            reason: "unsupported_expression_in_lambda_body",
        })),
    }
}

/// Apply a [`Closure`] to `args` (M6, ticket t61): bind each parameter to its argument in a
/// child of the captured environment, then evaluate the body. Pure.
///
/// # Errors
/// [`EvalError::LambdaArity`] if the argument count does not match the closure's parameter
/// count; any [`EvalError`] the body evaluation raises.
pub fn apply(
    closure: &Closure,
    args: Vec<LambdaValue>,
    stdlib: &StdlibRegistry,
    ctx: &EvalCtx,
) -> Result<LambdaValue, EvalError> {
    if args.len() != closure.params.len() {
        return Err(EvalError::LambdaArity {
            expected: closure.params.len(),
            found: args.len(),
        });
    }
    let mut env = closure.captured.clone();
    for (param, arg) in closure.params.iter().zip(args) {
        env = env.bind(param.clone(), arg);
    }
    eval_expr(&closure.body, &env, stdlib, ctx)
}

// ---- internals ------------------------------------------------------------

/// Evaluate a `fn(args)` call: dispatch the higher-order builtins (`map`/`filter`/`reduce`)
/// to the closure-application path, and a scalar builtin to its pure `Value → Value` body.
fn eval_fn(
    name: &str,
    args: &[Expr],
    env: &ValueEnv,
    stdlib: &StdlibRegistry,
    ctx: &EvalCtx,
) -> Result<LambdaValue, EvalError> {
    let Some(builtin) = stdlib.builtin(name) else {
        return Err(EvalError::Fn(FnError::UnknownFunction {
            name: name.to_string(),
        }));
    };
    if !builtin.sig.accepts_arity(args.len()) {
        return Err(EvalError::Fn(FnError::Arity {
            name: name.to_string(),
            expected: builtin.sig.min_args,
            found: args.len(),
        }));
    }
    if let Some(kind) = builtin.higher_order_kind() {
        return apply_higher_order(kind, args, env, stdlib, ctx);
    }
    match &builtin.eval {
        // A pure scalar: evaluate every argument to a plain value, then call its body.
        BuiltinEval::Scalar(f) => {
            let mut values = Vec::with_capacity(args.len());
            for a in args {
                values.push(eval_expr(a, env, stdlib, ctx)?.into_data()?);
            }
            Ok(LambdaValue::Data(f(&values, ctx).map_err(EvalError::Fn)?))
        }
        // Aggregate / table-valued builtins are not callable in a pure scalar lambda body.
        BuiltinEval::Aggregate(_) | BuiltinEval::TableValued(_) | BuiltinEval::HigherOrder(_) => {
            Err(EvalError::Fn(FnError::Domain {
                name: name.to_string(),
                reason: "not_callable_in_lambda_body",
            }))
        }
    }
}

/// Apply a higher-order builtin (`map`/`filter`/`reduce`) over an in-memory collection and a
/// lambda. The first argument is the collection (an `Array` value); the second is the lambda
/// (a [`Closure`]); `reduce`'s optional third is the initial accumulator.
fn apply_higher_order(
    kind: HigherOrderKind,
    args: &[Expr],
    env: &ValueEnv,
    stdlib: &StdlibRegistry,
    ctx: &EvalCtx,
) -> Result<LambdaValue, EvalError> {
    let collection = eval_expr(&args[0], env, stdlib, ctx)?.into_data()?;
    let closure = eval_expr(&args[1], env, stdlib, ctx)?.into_closure()?;
    let items = as_array(collection)?;
    match kind {
        HigherOrderKind::Map => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                let mapped = apply(&closure, vec![LambdaValue::Data(item)], stdlib, ctx)?;
                out.push(mapped.into_data()?);
            }
            Ok(LambdaValue::Data(Value::Array(out)))
        }
        HigherOrderKind::Filter => {
            let mut out = Vec::new();
            for item in items {
                let keep = apply(&closure, vec![LambdaValue::Data(item.clone())], stdlib, ctx)?
                    .into_data()?;
                if is_truthy(&keep) {
                    out.push(item);
                }
            }
            Ok(LambdaValue::Data(Value::Array(out)))
        }
        HigherOrderKind::Reduce => {
            // `reduce(coll, fn, init)` folds from `init`; `reduce(coll, fn)` from the first
            // element (and an empty collection then yields Null — nothing to fold).
            let mut iter = items.into_iter();
            let mut acc = match args.get(2) {
                Some(init_expr) => eval_expr(init_expr, env, stdlib, ctx)?.into_data()?,
                None => match iter.next() {
                    Some(first) => first,
                    None => return Ok(LambdaValue::Data(Value::Null)),
                },
            };
            for item in iter {
                acc = apply(
                    &closure,
                    vec![LambdaValue::Data(acc), LambdaValue::Data(item)],
                    stdlib,
                    ctx,
                )?
                .into_data()?;
            }
            Ok(LambdaValue::Data(acc))
        }
    }
}

/// Evaluate a boolean / comparison binary operator over two values. Arithmetic is not part of
/// the frozen operator set (RFD §3), so only the logical/comparison operators are supported.
fn eval_binary(
    op: Op,
    lhs: &Expr,
    rhs: &Expr,
    env: &ValueEnv,
    stdlib: &StdlibRegistry,
    ctx: &EvalCtx,
) -> Result<LambdaValue, EvalError> {
    // Short-circuit the logical operators.
    if matches!(op, Op::And | Op::Or) {
        let l = is_truthy(&eval_expr(lhs, env, stdlib, ctx)?.into_data()?);
        let result = match op {
            Op::And => l && is_truthy(&eval_expr(rhs, env, stdlib, ctx)?.into_data()?),
            Op::Or => l || is_truthy(&eval_expr(rhs, env, stdlib, ctx)?.into_data()?),
            _ => unreachable!(),
        };
        return Ok(LambdaValue::Data(Value::Bool(result)));
    }
    let l = eval_expr(lhs, env, stdlib, ctx)?.into_data()?;
    let r = eval_expr(rhs, env, stdlib, ctx)?.into_data()?;
    let b = match op {
        Op::Eq => values_equal(&l, &r),
        Op::Ne => !values_equal(&l, &r),
        Op::Lt => compare(&l, &r)? == std::cmp::Ordering::Less,
        Op::Le => compare(&l, &r)? != std::cmp::Ordering::Greater,
        Op::Gt => compare(&l, &r)? == std::cmp::Ordering::Greater,
        Op::Ge => compare(&l, &r)? != std::cmp::Ordering::Less,
        // NOT/LIKE/Match/And/Or are not value comparisons reachable here.
        _ => {
            return Err(EvalError::Fn(FnError::Domain {
                name: "lambda".to_string(),
                reason: "unsupported_operator_in_lambda_body",
            }))
        }
    };
    Ok(LambdaValue::Data(Value::Bool(b)))
}

/// Resolve a struct-navigation path `a.b.c` against the environment: look up the head
/// variable, then walk named struct fields.
fn eval_path(segs: &[String], env: &ValueEnv) -> Result<LambdaValue, EvalError> {
    let Some((head, rest)) = segs.split_first() else {
        return Err(EvalError::Fn(FnError::Domain {
            name: "lambda".to_string(),
            reason: "empty_path",
        }));
    };
    let mut value = match env.get(head) {
        Some(LambdaValue::Data(v)) => v.clone(),
        Some(LambdaValue::Closure(_)) => {
            return Err(EvalError::NotAFunction {
                detail: "cannot navigate fields of a function value".to_string(),
            })
        }
        None => {
            return Err(EvalError::Resolve(ResolveError::UnknownBinding {
                name: head.clone(),
            }))
        }
    };
    for seg in rest {
        value = match value {
            Value::Struct(fields) => fields.get(seg).cloned().unwrap_or(Value::Null),
            _ => Value::Null,
        };
    }
    Ok(LambdaValue::Data(value))
}

/// Require an `Array` value (the collection a `map`/`filter`/`reduce` folds over), or a
/// structured type error carrying only the offending shape's label (never the value).
fn as_array(value: Value) -> Result<Vec<Value>, EvalError> {
    match value {
        Value::Array(items) => Ok(items),
        other => Err(EvalError::Fn(FnError::Type {
            name: "map/filter/reduce".to_string(),
            expected: "Array",
            found: type_label(&other),
        })),
    }
}

/// Map a parser [`Literal`] to a runtime [`Value`] (the lambda-evaluator's local lowering;
/// mirrors the evaluator's, kept private so the two passes stay decoupled).
fn literal_to_value(lit: &Literal) -> Value {
    match lit {
        Literal::Str(s) => Value::Text(s.clone()),
        Literal::Int(n) => Value::Int(*n),
        Literal::Float(f) => Value::Float(*f),
        Literal::Bool(b) => Value::Bool(*b),
        Literal::Null => Value::Null,
        Literal::Size { value, .. } => Value::Int(*value as i64),
        Literal::Typed { raw, .. } => Value::Text(raw.clone()),
    }
}

/// SQL-ish truthiness for a filter predicate / logical operator: `Bool(true)` is true,
/// everything else (including `Null` and non-booleans) is false.
fn is_truthy(v: &Value) -> bool {
    matches!(v, Value::Bool(true))
}

/// Structural equality for the `==`/`<>` operators (numeric `Int`/`Float` compare across the
/// two numeric kinds; otherwise the canonical [`Value`] equality).
fn values_equal(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Int(x), Value::Float(y)) | (Value::Float(y), Value::Int(x)) => (*x as f64) == *y,
        _ => a == b,
    }
}

/// Total-ish ordering for the comparison operators over the numeric / text scalars. An
/// incomparable pair is a structured type error (never a panic).
fn compare(a: &Value, b: &Value) -> Result<std::cmp::Ordering, EvalError> {
    use std::cmp::Ordering;
    let ord = match (a, b) {
        (Value::Int(x), Value::Int(y)) => x.cmp(y),
        (Value::Float(x), Value::Float(y)) => x.partial_cmp(y).unwrap_or(Ordering::Equal),
        (Value::Int(x), Value::Float(y)) => (*x as f64).partial_cmp(y).unwrap_or(Ordering::Equal),
        (Value::Float(x), Value::Int(y)) => x.partial_cmp(&(*y as f64)).unwrap_or(Ordering::Equal),
        (Value::Text(x), Value::Text(y)) => x.cmp(y),
        _ => {
            return Err(EvalError::Fn(FnError::Type {
                name: "lambda".to_string(),
                expected: "comparable scalar",
                found: type_label(a),
            }))
        }
    };
    Ok(ord)
}

/// A stable, secret-free type label for a [`Value`] (for the structured errors).
fn type_label(v: &Value) -> &'static str {
    match v {
        Value::Null => "Null",
        Value::Bool(_) => "Bool",
        Value::Int(_) => "Int",
        Value::Float(_) => "Float",
        Value::Text(_) => "Text",
        Value::Bytes(_) => "Bytes",
        Value::Timestamp(_) => "Timestamp",
        Value::Struct(_) => "Struct",
        Value::Array(_) => "Array",
        Value::Json(_) => "Json",
        _ => "Unknown",
    }
}

#[cfg(test)]
mod tests;
