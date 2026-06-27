//! Unit + golden tests for the pushdown planner (t14): the source-split decision per
//! `PushdownProfile` (Full pushes all; None pushes nothing; Partial splits correctly),
//! cross-source JOIN/UNION federation, predicate provenance (single-source pushed,
//! two-source not), capability denial at plan time, and the deterministic `explain()`
//! golden strings. All fakes are in-memory (no network).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use qfs_driver::PushdownProfile;
use qfs_pushdown::{
    explain, partition_by_source, Aggregate, Aggregator, JoinOn, LogicalPlan, OrderKey,
    PhysicalPlan, PlanError, SetKind, SourceId, SourceRegistry,
};
use qfs_types::{CmpOp, ColRef, Column, ColumnType, Literal, Predicate, Schema};

fn rel_schema() -> Schema {
    Schema::new(vec![
        Column::new("id", ColumnType::Int, false),
        Column::new("name", ColumnType::Text, true),
        Column::new("age", ColumnType::Int, true),
    ])
}

fn scan(src: &str) -> LogicalPlan {
    LogicalPlan::scan(SourceId::new(src), rel_schema())
}

fn pred_age_gt_30() -> Predicate {
    Predicate::Cmp(ColRef::col("age"), CmpOp::Gt, Literal::Int(30))
}

fn full() -> PushdownProfile {
    PushdownProfile::Full
}

fn none() -> PushdownProfile {
    PushdownProfile::None
}

fn partial_where_project() -> PushdownProfile {
    PushdownProfile::Partial {
        where_: true,
        project: true,
        limit: false,
        order: false,
        join: false,
        aggregate: false,
        distinct: false,
        group_by: false,
    }
}

// ---- Full pushes all ----

#[test]
fn full_profile_pushes_entire_chain_to_one_scan() {
    // /db |> WHERE age > 30 |> SELECT id, name |> LIMIT 5
    let plan = LogicalPlan::Limit {
        input: Box::new(LogicalPlan::Project {
            input: Box::new(LogicalPlan::Filter {
                input: Box::new(scan("db")),
                predicate: pred_age_gt_30(),
            }),
            columns: vec!["id".into(), "name".into()],
        }),
        n: 5,
    };
    let reg = SourceRegistry::new().with(SourceId::new("db"), full());
    let phys = partition_by_source(&plan, &reg).unwrap();

    // Exactly one Scan, zero local Combine.
    assert_eq!(phys.scan_count(), 1);
    let PhysicalPlan::Scan(node) = &phys else {
        panic!("expected a bare Scan, got {phys:?}");
    };
    assert!(node.pushed.filter.is_some());
    assert_eq!(
        node.pushed.project.as_deref(),
        Some(&["id".into(), "name".into()][..])
    );
    assert_eq!(node.pushed.limit, Some(5));

    // Golden explain.
    assert_eq!(
        explain(&phys),
        "Scan[db] pushed=[where, project(id,name), limit 5]\n"
    );
}

// ---- None pushes nothing ----

#[test]
fn none_profile_pushes_nothing_all_residual_local() {
    let plan = LogicalPlan::Project {
        input: Box::new(LogicalPlan::Filter {
            input: Box::new(scan("api")),
            predicate: pred_age_gt_30(),
        }),
        columns: vec!["id".into()],
    };
    let reg = SourceRegistry::new().with(SourceId::new("api"), none());
    let phys = partition_by_source(&plan, &reg).unwrap();

    // Still one native scan (the read itself), but everything else is a local combine.
    assert_eq!(phys.scan_count(), 1);
    let scans = phys.scans();
    assert!(scans[0].pushed.is_bare(), "None must push nothing");

    assert_eq!(
        explain(&phys),
        "Combine[Project(id)]\n  Combine[Filter]\n    Scan[api] pushed=[]\n"
    );
}

// ---- Partial splits correctly ----

#[test]
fn partial_profile_splits_pushable_from_residual() {
    // WHERE + SELECT are pushable; LIMIT + DISTINCT are not → they stay local above.
    // /db |> WHERE age>30 |> SELECT id,name |> DISTINCT |> LIMIT 3
    let plan = LogicalPlan::Limit {
        input: Box::new(LogicalPlan::Distinct {
            input: Box::new(LogicalPlan::Project {
                input: Box::new(LogicalPlan::Filter {
                    input: Box::new(scan("db")),
                    predicate: pred_age_gt_30(),
                }),
                columns: vec!["id".into(), "name".into()],
            }),
        }),
        n: 3,
    };
    let reg = SourceRegistry::new().with(SourceId::new("db"), partial_where_project());
    let phys = partition_by_source(&plan, &reg).unwrap();

    let scans = phys.scans();
    assert_eq!(scans.len(), 1);
    // where + project pushed.
    assert!(scans[0].pushed.filter.is_some());
    assert!(scans[0].pushed.project.is_some());
    // limit/distinct NOT pushed.
    assert_eq!(scans[0].pushed.limit, None);
    assert!(!scans[0].pushed.distinct);

    // The residual local chain: Limit over Distinct over the partial scan.
    assert_eq!(
        explain(&phys),
        "Combine[Limit 3]\n  Combine[Distinct]\n    Scan[db] pushed=[where, project(id,name)]\n"
    );
}

#[test]
fn partial_stops_pushing_after_first_local_op() {
    // A Partial source that supports WHERE but NOT project: a SELECT below a pushed WHERE
    // forces project local; a *second* WHERE above the SELECT cannot be pushed back below
    // the local project, so it too stays local.
    let profile = PushdownProfile::Partial {
        where_: true,
        project: false,
        limit: false,
        order: false,
        join: false,
        aggregate: false,
        distinct: false,
        group_by: false,
    };
    // /db |> WHERE age>30 |> SELECT id |> WHERE id>0
    let plan = LogicalPlan::Filter {
        input: Box::new(LogicalPlan::Project {
            input: Box::new(LogicalPlan::Filter {
                input: Box::new(scan("db")),
                predicate: pred_age_gt_30(),
            }),
            columns: vec!["id".into()],
        }),
        predicate: Predicate::Cmp(ColRef::col("id"), CmpOp::Gt, Literal::Int(0)),
    };
    let reg = SourceRegistry::new().with(SourceId::new("db"), profile);
    let phys = partition_by_source(&plan, &reg).unwrap();
    let scans = phys.scans();
    // The first WHERE pushed; the project + outer WHERE are local.
    assert!(scans[0].pushed.filter.is_some());
    assert!(scans[0].pushed.project.is_none());
    assert_eq!(
        explain(&phys),
        "Combine[Filter]\n  Combine[Project(id)]\n    Scan[db] pushed=[where]\n"
    );
}

// ---- Federation: cross-source JOIN runs locally over each side ----

#[test]
fn cross_source_join_federates_locally_over_two_scans() {
    // /pg JOIN /git ON pg.id = git.id — two different sources → a local HashJoin
    // over each side's pushed-down scan.
    let plan = LogicalPlan::Join {
        kind: qfs_pushdown::JoinKind::Inner,
        lhs: Box::new(LogicalPlan::Filter {
            input: Box::new(scan("pg")),
            predicate: pred_age_gt_30(),
        }),
        rhs: Box::new(scan("git")),
        on: JoinOn::eq("id", "id"),
    };
    let reg = SourceRegistry::new()
        .with(SourceId::new("pg"), full())
        .with(SourceId::new("git"), full());
    let phys = partition_by_source(&plan, &reg).unwrap();

    // Two native scans, combined by a local HashJoin.
    assert_eq!(phys.scan_count(), 2);
    let PhysicalPlan::Combine { op, .. } = &phys else {
        panic!("expected a Combine[HashJoin]");
    };
    assert_eq!(op.label(), "HashJoin");

    // Each side pushed its own work to its own source.
    assert_eq!(
        explain(&phys),
        "Combine[HashJoin(id = id)]\n  Scan[pg] pushed=[where]\n  Scan[git] pushed=[]\n"
    );
}

#[test]
fn cross_source_union_federates_locally() {
    let plan = LogicalPlan::SetOp {
        kind: SetKind::Union,
        lhs: Box::new(scan("s3")),
        rhs: Box::new(scan("drive")),
    };
    let reg = SourceRegistry::new()
        .with(SourceId::new("s3"), full())
        .with(SourceId::new("drive"), full());
    let phys = partition_by_source(&plan, &reg).unwrap();
    assert_eq!(phys.scan_count(), 2);
    assert_eq!(
        explain(&phys),
        "Combine[Union]\n  Scan[s3] pushed=[]\n  Scan[drive] pushed=[]\n"
    );
}

// ---- Predicate provenance: single-source pushed; two-source residual ----

#[test]
fn single_source_predicate_over_join_is_pushed_to_its_side() {
    // A WHERE that references only the pg side, applied to the pg scan *before* the join,
    // is pushed to pg; the join is the federated residual. (Two-source predicates over a
    // join would be lowered above the Join node and stay local — see the residual filter
    // test below.)
    let plan = LogicalPlan::Join {
        kind: qfs_pushdown::JoinKind::Inner,
        lhs: Box::new(LogicalPlan::Filter {
            input: Box::new(scan("pg")),
            predicate: pred_age_gt_30(),
        }),
        rhs: Box::new(scan("git")),
        on: JoinOn::eq("id", "id"),
    };
    let reg = SourceRegistry::new()
        .with(SourceId::new("pg"), full())
        .with(SourceId::new("git"), full());
    let phys = partition_by_source(&plan, &reg).unwrap();
    // pg's scan carries the pushed filter; git's does not — the predicate did not leak
    // to the wrong side.
    let scans = phys.scans();
    assert!(
        scans[0].pushed.filter.is_some(),
        "pg-only predicate pushed to pg"
    );
    assert!(
        scans[1].pushed.filter.is_none(),
        "git side carries no predicate"
    );
}

#[test]
fn two_source_predicate_over_join_stays_local_on_neither_side() {
    // A WHERE applied *above* a cross-source Join (referencing joined columns) cannot be
    // pushed to either source — it must be a local Filter over the HashJoin.
    let plan = LogicalPlan::Filter {
        input: Box::new(LogicalPlan::Join {
            kind: qfs_pushdown::JoinKind::Inner,
            lhs: Box::new(scan("pg")),
            rhs: Box::new(scan("git")),
            on: JoinOn::eq("id", "id"),
        }),
        predicate: pred_age_gt_30(),
    };
    let reg = SourceRegistry::new()
        .with(SourceId::new("pg"), full())
        .with(SourceId::new("git"), full());
    let phys = partition_by_source(&plan, &reg).unwrap();

    // Neither scan pushed the predicate; it is a local Filter above the HashJoin.
    for s in phys.scans() {
        assert!(
            s.pushed.filter.is_none(),
            "two-source predicate must not be pushed"
        );
    }
    assert_eq!(
        explain(&phys),
        "Combine[Filter]\n  Combine[HashJoin(id = id)]\n    Scan[pg] pushed=[]\n    Scan[git] pushed=[]\n"
    );
}

// ---- Aggregate / group_by gating ----

#[test]
fn aggregate_pushed_only_when_both_aggregate_and_group_by_supported() {
    let aggs = vec![Aggregate {
        func: Aggregator::Count,
        column: "id".into(),
        output: "n".into(),
    }];
    let plan = LogicalPlan::Aggregate {
        input: Box::new(scan("db")),
        group_by: vec!["name".into()],
        aggregates: aggs.clone(),
    };
    // Profile supports aggregate but NOT group_by → must stay local.
    let no_group = PushdownProfile::Partial {
        where_: false,
        project: false,
        limit: false,
        order: false,
        join: false,
        aggregate: true,
        distinct: false,
        group_by: false,
    };
    let reg = SourceRegistry::new().with(SourceId::new("db"), no_group);
    let phys = partition_by_source(&plan, &reg).unwrap();
    assert!(
        phys.scans()[0].pushed.aggregates.is_empty(),
        "no group_by ⇒ local"
    );
    assert_eq!(
        explain(&phys),
        "Combine[Aggregate(by name: count(id))]\n  Scan[db] pushed=[]\n"
    );

    // Full profile pushes the grouped aggregate.
    let reg2 = SourceRegistry::new().with(SourceId::new("db"), full());
    let phys2 = partition_by_source(&plan, &reg2).unwrap();
    assert_eq!(phys2.scans()[0].pushed.aggregates.len(), 1);
    assert_eq!(
        explain(&phys2),
        "Scan[db] pushed=[group_by(name), aggregate(count(id))]\n"
    );
}

// ---- Capability denial at plan time ----

#[test]
fn unreadable_source_is_denied_at_plan_time() {
    let mut reg = SourceRegistry::new();
    reg.register_unreadable(SourceId::new("locked"), full());
    let plan = scan("locked");
    let err = partition_by_source(&plan, &reg).unwrap_err();
    assert_eq!(err.code(), "capability_denied");
    assert!(matches!(err, PlanError::CapabilityDenied { .. }));
}

#[test]
fn unknown_source_is_rejected_not_partially_scanned() {
    let reg = SourceRegistry::new(); // empty
    let err = partition_by_source(&scan("ghost"), &reg).unwrap_err();
    assert_eq!(err.code(), "unknown_source");
}

// ---- Determinism: identical input ⇒ byte-identical explain ----

#[test]
fn explain_is_deterministic() {
    let plan = LogicalPlan::Sort {
        input: Box::new(scan("db")),
        keys: vec![OrderKey {
            column: "age".into(),
            descending: true,
        }],
    };
    let reg = SourceRegistry::new().with(SourceId::new("db"), full());
    let a = explain(&partition_by_source(&plan, &reg).unwrap());
    let b = explain(&partition_by_source(&plan, &reg).unwrap());
    assert_eq!(a, b);
    assert_eq!(a, "Scan[db] pushed=[order(age desc)]\n");
}
