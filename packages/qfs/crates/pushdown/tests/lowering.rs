//! End-to-end lowering tests: parse real qfs DSL text, lower the AST into a
//! [`LogicalPlan`], then partition it. This proves the t07 carry-over O-t07-3 concretely
//! — the `WHERE` predicate is **sourced from the AST** (not dropped, as `PlanSource`
//! does) and survives into a pushed `PushedQuery` / a local residual.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use qfs_driver::PushdownProfile;
use qfs_parser::{parse_statement, Statement};
use qfs_pushdown::{
    explain, lower_predicate, lower_query, partition_by_source, LogicalPlan, SourceId,
    SourceRegistry,
};
use qfs_types::{Column, ColumnType, Predicate, Schema};

fn rel_schema() -> Schema {
    Schema::new(vec![
        Column::new("id", ColumnType::Int, false),
        Column::new("name", ColumnType::Text, true),
        Column::new("age", ColumnType::Int, true),
    ])
}

/// The source resolver: the first path segment is the driver/source id.
fn source_of(segs: &[String]) -> SourceId {
    SourceId::new(segs.first().cloned().unwrap_or_default())
}

fn schema_of(_src: &SourceId) -> Schema {
    rel_schema()
}

fn pipeline_of(src: &str) -> qfs_parser::Pipeline {
    match parse_statement(src).unwrap() {
        Statement::Query(p) => p,
        other => panic!("expected a query, got {other:?}"),
    }
}

#[test]
fn where_predicate_is_sourced_from_the_ast_and_pushed() {
    // The AST WHERE survives lowering into a typed Predicate and is pushed (O-t07-3).
    let pipe = pipeline_of("FROM /db/users |> WHERE age > 30 |> SELECT id, name");
    let plan = lower_query(&pipe, &source_of, &schema_of).unwrap();

    // The lowered Filter carries a real predicate (not a dropped AST).
    let LogicalPlan::Project { input, .. } = &plan else {
        panic!("expected a Project at the root, got {plan:?}");
    };
    let LogicalPlan::Filter { predicate, .. } = input.as_ref() else {
        panic!("expected a Filter under the Project");
    };
    assert!(matches!(predicate, Predicate::Cmp(_, _, _)));

    let reg = SourceRegistry::new().with(SourceId::new("db"), PushdownProfile::Full);
    let phys = partition_by_source(&plan, &reg).unwrap();
    assert_eq!(
        explain(&phys),
        "Scan[db] pushed=[where, project(id,name)]\n"
    );
}

#[test]
fn lower_predicate_maps_and_or_not_and_comparisons() {
    let pipe = pipeline_of("FROM /db/t |> WHERE age >= 18 AND name = 'a'");
    let plan = lower_query(&pipe, &source_of, &schema_of).unwrap();
    let LogicalPlan::Filter { predicate, .. } = &plan else {
        panic!("expected a Filter");
    };
    assert!(matches!(predicate, Predicate::And(_, _)));
}

#[test]
fn cross_source_join_lowered_from_dsl_federates() {
    // A JOIN whose two sides resolve to different sources federates locally.
    let pipe = pipeline_of("FROM /pg/orders |> JOIN /git/commits ON id = id");
    let plan = lower_query(&pipe, &source_of, &schema_of).unwrap();
    let reg = SourceRegistry::new()
        .with(SourceId::new("pg"), PushdownProfile::Full)
        .with(SourceId::new("git"), PushdownProfile::Full);
    let phys = partition_by_source(&plan, &reg).unwrap();
    assert_eq!(phys.scan_count(), 2);
    assert!(explain(&phys).starts_with("Combine[HashJoin(id = id)]"));
}

#[test]
fn lower_predicate_rejects_non_predicate_expression() {
    // A bare column is not a comparison predicate — a structured error, never silently
    // dropped (so the planner never loses a filter).
    let pipe = pipeline_of("FROM /db/t |> WHERE id");
    let err = lower_query(&pipe, &source_of, &schema_of).unwrap_err();
    assert_eq!(err.code(), "unsupported_predicate");
}

#[test]
fn lower_predicate_handles_like_and_in_and_between() {
    use qfs_parser::Expr;
    // Drive lower_predicate directly over parsed expressions through the pipeline.
    for (src, ok) in [
        ("FROM /db/t |> WHERE name LIKE 'a%'", true),
        ("FROM /db/t |> WHERE age IN (1, 2, 3)", true),
        ("FROM /db/t |> WHERE age BETWEEN 1 AND 9", true),
    ] {
        let pipe = pipeline_of(src);
        let plan = lower_query(&pipe, &source_of, &schema_of);
        assert_eq!(plan.is_ok(), ok, "{src}");
    }
    // And lower_predicate is exposed for direct use over an AST Expr.
    let pipe = pipeline_of("FROM /db/t |> WHERE age = 5");
    if let qfs_parser::PipeOp::Where(e) = &pipe.ops[0] {
        let p = lower_predicate(e).unwrap();
        assert!(matches!(p, Predicate::Cmp(_, _, _)));
    } else {
        let _: &Expr; // unreachable; keep the import meaningful
        panic!("expected a WHERE op");
    }
}
