//! Unit tests for the pure lambda evaluator (M6, ticket t61): closure construction +
//! application, parameter scoping, closure-over-`LET`, the higher-order builtins
//! `map`/`filter`/`reduce`, and the structured arity / not-a-function errors. All pure: no
//! I/O, no network, no credentials.

use super::*;
use qfs_parser::parse_statement;
use qfs_parser::{Expr, PipeOp, Statement};

/// A deterministic, capability-denied pure-eval context.
fn ctx() -> EvalCtx<'static> {
    use crate::stdlib::NoEnv;
    static ENV: NoEnv = NoEnv;
    EvalCtx::pure(1_700_000_000, 20_000, &ENV)
}

/// Parse an expression by lifting it out of a `WHERE` predicate — the full expression
/// grammar (function calls, lambdas as arguments, comparisons) is available there.
fn parse_expr(src: &str) -> Expr {
    let stmt = parse_statement(&format!("/t |> WHERE {src}")).expect("parse");
    let Statement::Query(p) = stmt else {
        panic!("expected a query, got {stmt:?}");
    };
    let PipeOp::Where(expr) = p.ops.into_iter().next().unwrap() else {
        panic!("expected a WHERE op");
    };
    expr
}

/// Evaluate a bare expression with the core stdlib under `env`.
fn eval(src: &str, env: &ValueEnv) -> Result<LambdaValue, EvalError> {
    let expr = parse_expr(src);
    let reg = StdlibRegistry::with_core();
    eval_expr(&expr, env, &reg, &ctx())
}

#[test]
fn lambda_literal_evaluates_to_a_closure() {
    // `LET f = (x) => upper(x)` — the lambda is a first-class value (a closure).
    let v = eval("(x) => UPPER(x)", &ValueEnv::new()).expect("eval");
    let LambdaValue::Closure(c) = v else {
        panic!("expected a closure, got {v:?}");
    };
    assert_eq!(c.arity(), 1, "a one-parameter lambda");
}

#[test]
fn closure_applies_binding_its_parameter() {
    // Build the closure `(x) => upper(x)`, then apply it to `'hi'`.
    let reg = StdlibRegistry::with_core();
    let c = match eval("(x) => UPPER(x)", &ValueEnv::new()).unwrap() {
        LambdaValue::Closure(c) => c,
        other => panic!("expected closure, got {other:?}"),
    };
    let out = apply(
        &c,
        vec![LambdaValue::Data(Value::Text("hi".into()))],
        &reg,
        &ctx(),
    )
    .expect("apply")
    .into_data()
    .unwrap();
    assert_eq!(out, Value::Text("HI".into()));
}

#[test]
fn closure_captures_outer_let_binding() {
    // A lambda closes over an outer value binding (closure-over-LET, why t61 depends on t60):
    // `suffix` is captured at the lambda literal and used when the closure is later applied.
    let reg = StdlibRegistry::with_core();
    let env = ValueEnv::new().bind("suffix", LambdaValue::Data(Value::Text("!".into())));
    let c = match eval("(x) => CONCAT(x, suffix)", &env).unwrap() {
        LambdaValue::Closure(c) => c,
        other => panic!("expected closure, got {other:?}"),
    };
    let out = apply(
        &c,
        vec![LambdaValue::Data(Value::Text("hey".into()))],
        &reg,
        &ctx(),
    )
    .unwrap()
    .into_data()
    .unwrap();
    assert_eq!(out, Value::Text("hey!".into()));
}

#[test]
fn parameter_shadows_outer_binding_inside_the_body() {
    // The lambda parameter `x` shadows an outer `x` binding for the body's scope (lexical).
    let env = ValueEnv::new().bind("x", LambdaValue::Data(Value::Text("outer".into())));
    let reg = StdlibRegistry::with_core();
    let c = match eval("(x) => UPPER(x)", &env).unwrap() {
        LambdaValue::Closure(c) => c,
        other => panic!("{other:?}"),
    };
    let out = apply(
        &c,
        vec![LambdaValue::Data(Value::Text("inner".into()))],
        &reg,
        &ctx(),
    )
    .unwrap()
    .into_data()
    .unwrap();
    assert_eq!(
        out,
        Value::Text("INNER".into()),
        "the parameter wins for the body"
    );
}

#[test]
fn map_applies_a_lambda_over_a_collection() {
    // `map(coll, (x) => upper(x))` evaluated against an in-memory array.
    let env = ValueEnv::new().bind(
        "coll",
        LambdaValue::Data(Value::Array(vec![
            Value::Text("a".into()),
            Value::Text("b".into()),
        ])),
    );
    let out = eval("map(coll, (x) => UPPER(x))", &env)
        .unwrap()
        .into_data()
        .unwrap();
    assert_eq!(
        out,
        Value::Array(vec![Value::Text("A".into()), Value::Text("B".into())])
    );
}

#[test]
fn filter_keeps_elements_whose_lambda_is_truthy() {
    // `filter(coll, (x) => x > 2)` keeps the elements greater than two.
    let env = ValueEnv::new().bind(
        "coll",
        LambdaValue::Data(Value::Array(vec![
            Value::Int(1),
            Value::Int(2),
            Value::Int(3),
            Value::Int(4),
        ])),
    );
    let out = eval("filter(coll, (x) => x > 2)", &env)
        .unwrap()
        .into_data()
        .unwrap();
    assert_eq!(out, Value::Array(vec![Value::Int(3), Value::Int(4)]));
}

#[test]
fn reduce_folds_the_collection_through_the_lambda() {
    // `reduce(coll, (acc, x) => concat(acc, x), '')` concatenates the elements.
    let env = ValueEnv::new().bind(
        "coll",
        LambdaValue::Data(Value::Array(vec![
            Value::Text("a".into()),
            Value::Text("b".into()),
            Value::Text("c".into()),
        ])),
    );
    let out = eval("reduce(coll, (acc, x) => CONCAT(acc, x), '')", &env)
        .unwrap()
        .into_data()
        .unwrap();
    assert_eq!(out, Value::Text("abc".into()));
}

#[test]
fn applying_with_wrong_arity_is_a_structured_error() {
    // A one-parameter lambda applied with two arguments is a typed `LambdaArity` error.
    let reg = StdlibRegistry::with_core();
    let c = match eval("(x) => UPPER(x)", &ValueEnv::new()).unwrap() {
        LambdaValue::Closure(c) => c,
        other => panic!("{other:?}"),
    };
    let err = apply(
        &c,
        vec![
            LambdaValue::Data(Value::Text("a".into())),
            LambdaValue::Data(Value::Text("b".into())),
        ],
        &reg,
        &ctx(),
    )
    .unwrap_err();
    assert_eq!(err.code(), "lambda_arity");
    assert!(matches!(
        err,
        EvalError::LambdaArity {
            expected: 1,
            found: 2
        }
    ));
}

#[test]
fn passing_a_non_function_in_function_position_is_structured() {
    // `map(coll, 3)` — the lambda slot is a plain value, a structured `NotAFunction` error.
    let env = ValueEnv::new().bind("coll", LambdaValue::Data(Value::Array(vec![Value::Int(1)])));
    let err = eval("map(coll, 3)", &env).unwrap_err();
    assert_eq!(err.code(), "not_a_function");
}

#[test]
fn higher_order_builtins_are_registered_with_signatures() {
    // map/filter/reduce are ordinary entries in the open function registry (zero keywords).
    let reg = StdlibRegistry::with_core();
    for name in ["map", "filter", "reduce"] {
        assert!(
            reg.is_builtin(name),
            "{name} should be a registered builtin"
        );
        let b = reg.builtin(name).unwrap();
        assert!(
            b.higher_order_kind().is_some(),
            "{name} is a higher-order builtin"
        );
    }
}
