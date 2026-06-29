//! Unit tests for the plan-time static primitive type checker (decision T, ticket t75).
//!
//! Each case parses a real qfs `WHERE` predicate (so the parser + AST are exercised), then
//! type-checks it against a hand-built schema with [`check_expr`]. The matrix covers every
//! green-bar behaviour at the checker level: a well-typed expression / predicate / lambda
//! checks, a mismatched comparison is a structured error, a built-in handed a bad argument
//! type is rejected, an annotated lambda parameter is enforced, and a lambda applied to the
//! wrong element type is rejected. All hermetic — no I/O, no credentials.

use super::*;
use qfs_parser::{parse_statement, PipeOp, Statement};
use qfs_types::{Column, ColumnType, Schema};

/// Extract the first `WHERE` predicate expression from a parsed read pipeline.
fn where_expr(src: &str) -> Expr {
    let Statement::Query(pipeline) = parse_statement(src).expect("parse") else {
        panic!("expected a query pipeline");
    };
    for op in pipeline.ops {
        if let PipeOp::Where(e) = op {
            return e;
        }
    }
    panic!("no WHERE clause in `{src}`");
}

/// A schema with the given `(name, type)` columns (all nullable for brevity).
fn schema(cols: &[(&str, ColumnType)]) -> Schema {
    Schema::new(
        cols.iter()
            .map(|(n, t)| Column::new((*n).to_string(), t.clone(), true))
            .collect(),
    )
}

fn core() -> StdlibRegistry {
    StdlibRegistry::with_core()
}

// ---- well-typed comparisons / predicates ----------------------------------

#[test]
fn well_typed_comparison_checks_to_bool() {
    let s = schema(&[("amount", ColumnType::Int)]);
    let ty = check_expr(
        &where_expr("/t |> WHERE amount > 100"),
        &TyEnv::new(),
        &s,
        Some(&core()),
    )
    .expect("a numeric comparison type-checks");
    assert_eq!(ty, Ty::Prim(ColumnType::Bool));
}

#[test]
fn numeric_widening_int_vs_float_is_allowed() {
    let s = schema(&[("amount", ColumnType::Int)]);
    check_expr(
        &where_expr("/t |> WHERE amount >= 3.5"),
        &TyEnv::new(),
        &s,
        Some(&core()),
    )
    .expect("Int vs Float widens");
}

// ---- mismatched comparison → structured plan-time error -------------------

#[test]
fn mismatched_comparison_is_a_structured_error() {
    let s = schema(&[("total", ColumnType::Int)]);
    let err = check_expr(
        &where_expr("/t |> WHERE total == 'paid'"),
        &TyEnv::new(),
        &s,
        Some(&core()),
    )
    .expect_err("comparing an i64 column to a string is rejected");
    assert_eq!(err.code(), "incomparable_types");
    assert!(matches!(
        err,
        EvalError::Type(TypeError::IncomparableTypes { .. })
    ));
}

#[test]
fn late_bound_column_comparison_is_lenient() {
    // An undescribable (empty) schema late-binds every column, so the comparison defers to
    // runtime rather than false-rejecting (the conservative posture).
    check_expr(
        &where_expr("/t |> WHERE whatever == 'x'"),
        &TyEnv::new(),
        &Schema::empty(),
        Some(&core()),
    )
    .expect("a late-bound column comparison is not rejected");
}

// ---- built-in / stdlib call with a bad argument type ----------------------

#[test]
fn builtin_with_bad_arg_type_is_rejected() {
    let s = schema(&[("amount", ColumnType::Int)]);
    let err = check_expr(
        &where_expr("/t |> WHERE UPPER(amount) == 'x'"),
        &TyEnv::new(),
        &s,
        Some(&core()),
    )
    .expect_err("UPPER of an i64 column is rejected");
    assert_eq!(err.code(), "fn_type");
}

#[test]
fn builtin_with_good_arg_type_checks() {
    let s = schema(&[("name", ColumnType::Text)]);
    check_expr(
        &where_expr("/t |> WHERE UPPER(name) == 'X'"),
        &TyEnv::new(),
        &s,
        Some(&core()),
    )
    .expect("UPPER of a text column type-checks");
}

#[test]
fn aggregate_in_predicate_position_is_rejected() {
    let s = schema(&[("id", ColumnType::Int)]);
    let err = check_expr(
        &where_expr("/t |> WHERE SUM(id) > 0"),
        &TyEnv::new(),
        &s,
        Some(&core()),
    )
    .expect_err("an aggregate in a predicate is a typed misuse");
    assert_eq!(err.code(), "aggregate_outside_aggregate");
}

#[test]
fn unknown_function_is_rejected() {
    let err = check_expr(
        &where_expr("/t |> WHERE NOPE(x) == 1"),
        &TyEnv::new(),
        &Schema::empty(),
        Some(&core()),
    )
    .expect_err("an unknown function is rejected");
    assert_eq!(err.code(), "unknown_function");
}

// ---- lambdas: well-typed body, annotated-param enforcement ----------------

#[test]
fn well_typed_lambda_checks_to_a_function_type() {
    let ty = check_expr(
        &where_expr("/t |> WHERE (n: i64) => n > 0"),
        &TyEnv::new(),
        &Schema::empty(),
        Some(&core()),
    )
    .expect("a well-typed lambda checks");
    assert!(
        matches!(ty, Ty::Fn { .. }),
        "a lambda has a function type, got {ty:?}"
    );
}

#[test]
fn annotated_lambda_param_is_enforced_in_the_body() {
    // The parameter is annotated `i64`, so the `~` (text-match) in the body is ill-typed
    // (`i64 ~ string`) — enforced right at the lambda body, no application needed.
    let err = check_expr(
        &where_expr("/t |> WHERE (n: i64) => n ~ 'p'"),
        &TyEnv::new(),
        &Schema::empty(),
        Some(&core()),
    )
    .expect_err("an annotated param misused in the body is rejected");
    assert_eq!(err.code(), "incomparable_types");
}

#[test]
fn unannotated_lambda_param_stays_late_bound() {
    // Without an annotation the parameter is late-bound, so the same body does not
    // false-reject (full inference is out of scope this slice).
    check_expr(
        &where_expr("/t |> WHERE (n) => n ~ 'p'"),
        &TyEnv::new(),
        &Schema::empty(),
        Some(&core()),
    )
    .expect("an unannotated param defers to runtime");
}

// ---- higher-order application: element-type checking ----------------------

#[test]
fn map_over_collection_checks_element_type() {
    let s = schema(&[("nums", ColumnType::Array(Box::new(ColumnType::Int)))]);
    let ty = check_expr(
        &where_expr("/t |> WHERE map(nums, (n: i64) => n > 0)"),
        &TyEnv::new(),
        &s,
        Some(&core()),
    )
    .expect("map over an i64 collection with a matching lambda checks");
    assert_eq!(ty, Ty::Prim(ColumnType::Array(Box::new(ColumnType::Bool))));
}

#[test]
fn lambda_applied_to_wrong_element_type_is_rejected() {
    // `nums` is a collection of i64; the lambda parameter is annotated `string`, so applying
    // it via `map` is rejected at plan time.
    let s = schema(&[("nums", ColumnType::Array(Box::new(ColumnType::Int)))]);
    let err = check_expr(
        &where_expr("/t |> WHERE map(nums, (n: string) => n)"),
        &TyEnv::new(),
        &s,
        Some(&core()),
    )
    .expect_err("a string-typed lambda applied to an i64 collection is rejected");
    assert_eq!(err.code(), "fn_type");
}

// ---- no registry → late-bound (t07 behaviour preserved) -------------------

#[test]
fn without_a_registry_calls_stay_late_bound() {
    let s = schema(&[("amount", ColumnType::Int)]);
    // A bad-arg built-in would be rejected *with* a registry; with none, the call is
    // late-bound and the comparison still checks (the argument subexpressions are walked).
    let ty = check_expr(
        &where_expr("/t |> WHERE UPPER(amount) == 'x'"),
        &TyEnv::new(),
        &s,
        None,
    )
    .expect("no registry leaves the call late-bound");
    assert_eq!(ty, Ty::Prim(ColumnType::Bool));
}
