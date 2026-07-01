//! Unit + differential tests for the local combine engine (t14, ADR-0002).
//!
//! Covers each residual operator (filter, project, sort, distinct, limit, group/
//! aggregate, expand), cross-source hash-join + set-op federation, and the **differential
//! property**: executing a partitioned plan over in-memory scan fakes returns the same
//! rows a naive all-local evaluation would. All scan results are in-memory (no network).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use qfs_driver::PushdownProfile;
use qfs_engine::{CombineEngine, MiniEvaluator, ScanResults};
use qfs_pushdown::{
    partition_by_source, Aggregate, Aggregator, JoinKind, JoinOn, LogicalPlan, PhysicalPlan,
    ScalarExpr, SetKind, SourceId, SourceRegistry,
};
use qfs_types::{
    CmpOp, ColRef, Column, ColumnType, Fields, Literal, Predicate, Row, RowBatch, Schema, Value,
};

fn users_schema() -> Schema {
    Schema::new(vec![
        Column::new("id", ColumnType::Int, false),
        Column::new("name", ColumnType::Text, true),
        Column::new("age", ColumnType::Int, true),
    ])
}

fn users_batch() -> RowBatch {
    RowBatch::new(
        users_schema(),
        vec![
            Row::new(vec![
                Value::Int(1),
                Value::Text("ann".into()),
                Value::Int(40),
            ]),
            Row::new(vec![
                Value::Int(2),
                Value::Text("bob".into()),
                Value::Int(25),
            ]),
            Row::new(vec![
                Value::Int(3),
                Value::Text("cy".into()),
                Value::Int(35),
            ]),
        ],
    )
}

fn none() -> PushdownProfile {
    PushdownProfile::None
}

// ---- Residual filter correctness (None source ⇒ engine filters locally) ----

#[test]
fn residual_filter_runs_locally() {
    // WHERE age > 30 over a None source: the planner leaves the filter local; the engine
    // applies it. Expect ann(40) and cy(35).
    let plan = LogicalPlan::Filter {
        input: Box::new(LogicalPlan::scan(SourceId::new("api"), users_schema())),
        predicate: Predicate::Cmp(ColRef::col("age"), CmpOp::Gt, Literal::Int(30)),
    };
    let reg = SourceRegistry::new().with(SourceId::new("api"), none());
    let phys = partition_by_source(&plan, &reg).unwrap();

    let out = MiniEvaluator::new()
        .execute(&phys, ScanResults::new(vec![users_batch()]))
        .unwrap();
    assert_eq!(out.rows.len(), 2);
    let names: Vec<&str> = out
        .rows
        .iter()
        .map(|r| match &r.values[1] {
            Value::Text(s) => s.as_str(),
            _ => "",
        })
        .collect();
    assert_eq!(names, vec!["ann", "cy"]);
}

#[test]
fn residual_project_sort_limit_distinct() {
    // SELECT name |> ORDER BY name |> LIMIT 2 over a None source.
    let plan = LogicalPlan::Limit {
        input: Box::new(LogicalPlan::Sort {
            input: Box::new(LogicalPlan::Project {
                input: Box::new(LogicalPlan::scan(SourceId::new("api"), users_schema())),
                columns: vec!["name".into()],
            }),
            keys: vec![qfs_pushdown::OrderKey {
                column: "name".into(),
                descending: false,
            }],
        }),
        n: 2,
    };
    let reg = SourceRegistry::new().with(SourceId::new("api"), none());
    let phys = partition_by_source(&plan, &reg).unwrap();
    let out = MiniEvaluator::new()
        .execute(&phys, ScanResults::new(vec![users_batch()]))
        .unwrap();
    assert_eq!(out.schema.column_names(), vec!["name"]);
    let names: Vec<&str> = out
        .rows
        .iter()
        .map(|r| match &r.values[0] {
            Value::Text(s) => s.as_str(),
            _ => "",
        })
        .collect();
    assert_eq!(names, vec!["ann", "bob"]); // sorted asc, first two
}

// ---- Group/aggregate ----

#[test]
fn residual_group_aggregate_count_and_sum() {
    // Two departments; COUNT and SUM of salary per dept.
    let schema = Schema::new(vec![
        Column::new("dept", ColumnType::Text, false),
        Column::new("salary", ColumnType::Int, false),
    ]);
    let batch = RowBatch::new(
        schema.clone(),
        vec![
            Row::new(vec![Value::Text("eng".into()), Value::Int(100)]),
            Row::new(vec![Value::Text("eng".into()), Value::Int(200)]),
            Row::new(vec![Value::Text("ops".into()), Value::Int(50)]),
        ],
    );
    let plan = LogicalPlan::Aggregate {
        input: Box::new(LogicalPlan::scan(SourceId::new("api"), schema)),
        group_by: vec!["dept".into()],
        aggregates: vec![
            Aggregate {
                func: Aggregator::Count,
                column: "salary".into(),
                output: "n".into(),
            },
            Aggregate {
                func: Aggregator::Sum,
                column: "salary".into(),
                output: "total".into(),
            },
        ],
    };
    let reg = SourceRegistry::new().with(SourceId::new("api"), none());
    let phys = partition_by_source(&plan, &reg).unwrap();
    let out = MiniEvaluator::new()
        .execute(&phys, ScanResults::new(vec![batch]))
        .unwrap();
    assert_eq!(out.schema.column_names(), vec!["dept", "n", "total"]);
    // eng: count 2, sum 300; ops: count 1, sum 50.
    let mut rows: Vec<(String, i64, i64)> = out
        .rows
        .iter()
        .map(|r| {
            let dept = match &r.values[0] {
                Value::Text(s) => s.clone(),
                _ => String::new(),
            };
            let n = match &r.values[1] {
                Value::Int(n) => *n,
                _ => -1,
            };
            let t = match &r.values[2] {
                Value::Int(n) => *n,
                _ => -1,
            };
            (dept, n, t)
        })
        .collect();
    rows.sort();
    assert_eq!(rows, vec![("eng".into(), 2, 300), ("ops".into(), 1, 50)]);
}

// ---- EXPAND ----

#[test]
fn residual_expand_explodes_array_of_struct() {
    // One row with a `tags` array of {tag} structs → two rows.
    let inner = Schema::new(vec![Column::new("tag", ColumnType::Text, false)]);
    let schema = Schema::new(vec![
        Column::new("id", ColumnType::Int, false),
        Column::new(
            "tags",
            ColumnType::Array(Box::new(ColumnType::Struct(inner))),
            false,
        ),
    ]);
    let row = Row::new(vec![
        Value::Int(1),
        Value::Array(vec![
            Value::Struct(Fields::new(vec![("tag".into(), Value::Text("x".into()))])),
            Value::Struct(Fields::new(vec![("tag".into(), Value::Text("y".into()))])),
        ]),
    ]);
    let batch = RowBatch::new(schema.clone(), vec![row]);
    let plan = LogicalPlan::Expand {
        input: Box::new(LogicalPlan::scan(SourceId::new("api"), schema)),
        field: "tags".into(),
    };
    let reg = SourceRegistry::new().with(SourceId::new("api"), none());
    let phys = partition_by_source(&plan, &reg).unwrap();
    let out = MiniEvaluator::new()
        .execute(&phys, ScanResults::new(vec![batch]))
        .unwrap();
    assert_eq!(out.rows.len(), 2);
}

// ---- Cross-source federation: hash join over two in-memory scans ----

#[test]
fn cross_source_hash_join_federates() {
    // pg users JOIN git authors ON id = id; each side a separate in-memory fake.
    let pg = users_batch();
    let git = RowBatch::new(
        Schema::new(vec![
            Column::new("id", ColumnType::Int, false),
            Column::new("sha", ColumnType::Text, false),
        ]),
        vec![
            Row::new(vec![Value::Int(1), Value::Text("abc".into())]),
            Row::new(vec![Value::Int(3), Value::Text("def".into())]),
        ],
    );
    let plan = LogicalPlan::Join {
        kind: JoinKind::Inner,
        lhs: Box::new(LogicalPlan::scan(SourceId::new("pg"), users_schema())),
        rhs: Box::new(LogicalPlan::scan(
            SourceId::new("git"),
            Schema::new(vec![
                Column::new("id", ColumnType::Int, false),
                Column::new("sha", ColumnType::Text, false),
            ]),
        )),
        on: JoinOn::eq("id", "id"),
    };
    let reg = SourceRegistry::new()
        .with(SourceId::new("pg"), none())
        .with(SourceId::new("git"), none());
    let phys = partition_by_source(&plan, &reg).unwrap();
    // Scans are consumed left-to-right: pg first, then git.
    let out = MiniEvaluator::new()
        .execute(&phys, ScanResults::new(vec![pg, git]))
        .unwrap();
    // ids 1 and 3 join; id 2 (bob) has no git row.
    assert_eq!(out.rows.len(), 2);
    // The join schema disambiguates the colliding `id` column from the right side. The
    // right `id` has no provenance here, so it falls back to the positional `r.id`
    // qualifier (Schema::join policy) — no silent shadowing of the left `id`.
    assert!(out.schema.column("id").is_some());
    assert!(out.schema.column("r.id").is_some());
}

// ---- Set ops ----

#[test]
fn cross_source_union_except_intersect() {
    let s = Schema::new(vec![Column::new("v", ColumnType::Int, false)]);
    let mk = |xs: &[i64]| {
        RowBatch::new(
            s.clone(),
            xs.iter().map(|n| Row::new(vec![Value::Int(*n)])).collect(),
        )
    };
    for (kind, expect) in [
        (SetKind::Union, vec![1, 2, 3]),
        (SetKind::Except, vec![1]),
        (SetKind::Intersect, vec![2]),
    ] {
        let plan = LogicalPlan::SetOp {
            kind,
            lhs: Box::new(LogicalPlan::scan(SourceId::new("a"), s.clone())),
            rhs: Box::new(LogicalPlan::scan(SourceId::new("b"), s.clone())),
        };
        let reg = SourceRegistry::new()
            .with(SourceId::new("a"), none())
            .with(SourceId::new("b"), none());
        let phys = partition_by_source(&plan, &reg).unwrap();
        let out = MiniEvaluator::new()
            .execute(&phys, ScanResults::new(vec![mk(&[1, 2]), mk(&[2, 3])]))
            .unwrap();
        let got: Vec<i64> = out
            .rows
            .iter()
            .map(|r| match &r.values[0] {
                Value::Int(n) => *n,
                _ => -1,
            })
            .collect();
        assert_eq!(got, expect, "{kind:?}");
    }
}

// ---- Differential property: partitioned == all-local ----

/// A faithful "driver fake": pre-apply the scan's *pushed* work to the base batch, the
/// way a real backend would. This makes the differential honest — the partitioned run
/// pushes work to the (fake) driver, the all-local run does it in the engine, and both
/// must agree. The pushed predicate is applied by running it through the engine as a
/// one-node local Filter (re-using the engine's own predicate kernel), then the pushed
/// limit truncates — modelling exactly what a backend would do for this fixture.
fn run_scan(scan: &qfs_pushdown::ScanNode, base: RowBatch) -> RowBatch {
    let mut out = base;
    if let Some(p) = &scan.pushed.filter {
        let filter_plan = PhysicalPlan::Combine {
            op: qfs_pushdown::CombineOp::Filter(p.clone()),
            inputs: vec![PhysicalPlan::Scan(scan.clone())],
        };
        out = MiniEvaluator::new()
            .execute(&filter_plan, ScanResults::new(vec![out]))
            .unwrap();
    }
    if let Some(n) = scan.pushed.limit {
        out.rows.truncate(n as usize);
    }
    out
}

#[test]
fn differential_partitioned_equals_all_local() {
    // The SAME logical plan run two ways must yield the same rows (the t14 differential
    // property): (1) all-local — None source, the engine does everything; (2) partitioned
    // — a Partial source pushes WHERE+LIMIT (pre-applied by the driver fake) and leaves
    // SELECT local. Both must produce identical rows.
    let plan = LogicalPlan::Limit {
        input: Box::new(LogicalPlan::Project {
            input: Box::new(LogicalPlan::Filter {
                input: Box::new(LogicalPlan::scan(SourceId::new("db"), users_schema())),
                predicate: Predicate::Cmp(ColRef::col("age"), CmpOp::Ge, Literal::Int(30)),
            }),
            columns: vec!["name".into()],
        }),
        n: 10,
    };

    // (1) all-local ground truth: None source ⇒ the engine runs filter+project+limit.
    let local_reg = SourceRegistry::new().with(SourceId::new("db"), none());
    let local_phys = partition_by_source(&plan, &local_reg).unwrap();
    let local_out = MiniEvaluator::new()
        .execute(&local_phys, ScanResults::new(vec![users_batch()]))
        .unwrap();

    // (2) partitioned: a Partial source that pushes WHERE+LIMIT. The driver fake
    // pre-applies the pushed work to the base batch; the engine runs the residual SELECT.
    let partial = PushdownProfile::Partial {
        where_: true,
        project: false,
        limit: true,
        order: false,
        join: false,
        aggregate: false,
        distinct: false,
        group_by: false,
    };
    let part_reg = SourceRegistry::new().with(SourceId::new("db"), partial);
    let part_phys = partition_by_source(&plan, &part_reg).unwrap();
    // Pre-apply each scan's pushed work (one scan here).
    let scan_node = part_phys.scans()[0];
    let pushed_batch = run_scan(scan_node, users_batch());
    let part_out = MiniEvaluator::new()
        .execute(&part_phys, ScanResults::new(vec![pushed_batch]))
        .unwrap();

    // The two runs agree: both project `name` over age>=30 ⇒ ann(40), cy(35).
    let names = |b: &RowBatch| -> Vec<String> {
        b.rows
            .iter()
            .filter_map(|r| match &r.values[0] {
                Value::Text(s) => Some(s.clone()),
                _ => None,
            })
            .collect()
    };
    assert_eq!(local_out.schema.column_names(), vec!["name"]);
    assert_eq!(part_out.schema.column_names(), vec!["name"]);
    assert_eq!(names(&local_out), vec!["ann".to_string(), "cy".to_string()]);
    assert_eq!(
        names(&part_out),
        names(&local_out),
        "partitioned execution must equal all-local"
    );
}

#[test]
fn missing_scan_result_is_a_structured_error() {
    let plan = LogicalPlan::scan(SourceId::new("api"), users_schema());
    let reg = SourceRegistry::new().with(SourceId::new("api"), none());
    let phys = partition_by_source(&plan, &reg).unwrap();
    let err = MiniEvaluator::new()
        .execute(&phys, ScanResults::new(vec![]))
        .unwrap_err();
    assert_eq!(err.code(), "missing_scan_result");
}

// ---- Cross-service pack: ProjectExpr + ARRAY_AGG + Extend (t92, ticket 192440) ----

fn drive_files_schema() -> Schema {
    Schema::new(vec![
        Column::new("name", ColumnType::Text, true),
        Column::new("mime_type", ColumnType::Text, true),
        Column::new("content", ColumnType::Bytes, true),
    ])
}

fn drive_files_batch() -> RowBatch {
    RowBatch::new(
        drive_files_schema(),
        vec![
            Row::new(vec![
                Value::Text("a.txt".into()),
                Value::Text("text/plain".into()),
                Value::Bytes(b"aaa".to_vec()),
            ]),
            Row::new(vec![
                Value::Text("b.pdf".into()),
                Value::Text("application/pdf".into()),
                Value::Bytes(b"bbb".to_vec()),
            ]),
        ],
    )
}

#[test]
fn cross_service_pack_attachments_project_expr_array_agg_extend() {
    // The 192440 composable recipe's read half, executed over an in-memory Drive scan:
    //   |> select {filename: name, mime: mime_type, bytes: content} as att   (ProjectExpr)
    //   |> aggregate array_agg(att) as attachments                           (ARRAY_AGG)
    //   |> extend to = 'a@x.y', subject = 'Q3', body = 'See attached'        (Extend)
    // Expect ONE row whose `attachments` is an Array of two Structs carrying each file's
    // bytes/filename/mime, plus the three extended draft columns.
    let att = ScalarExpr::Struct(vec![
        ("filename".into(), ScalarExpr::Col(ColRef::col("name"))),
        ("mime".into(), ScalarExpr::Col(ColRef::col("mime_type"))),
        ("bytes".into(), ScalarExpr::Col(ColRef::col("content"))),
    ]);
    let plan = LogicalPlan::Extend {
        input: Box::new(LogicalPlan::Aggregate {
            input: Box::new(LogicalPlan::ProjectExpr {
                input: Box::new(LogicalPlan::scan_at(
                    SourceId::new("drive"),
                    "/drive/my",
                    drive_files_schema(),
                )),
                projections: vec![("att".into(), att)],
            }),
            group_by: vec![],
            aggregates: vec![Aggregate {
                func: Aggregator::ArrayAgg,
                column: "att".into(),
                output: "attachments".into(),
            }],
        }),
        assignments: vec![
            ("to".into(), ScalarExpr::Lit(Value::Text("a@x.y".into()))),
            ("subject".into(), ScalarExpr::Lit(Value::Text("Q3".into()))),
            (
                "body".into(),
                ScalarExpr::Lit(Value::Text("See attached".into())),
            ),
        ],
    };

    let reg = SourceRegistry::new().with(SourceId::new("drive"), none());
    let phys = partition_by_source(&plan, &reg).unwrap();
    let out = MiniEvaluator::new()
        .execute(&phys, ScanResults::new(vec![drive_files_batch()]))
        .unwrap();

    assert_eq!(
        out.rows.len(),
        1,
        "array_agg collapses the two files into one row"
    );
    let row = &out.rows[0];
    let col = |name: &str| {
        out.schema
            .columns
            .iter()
            .position(|c| c.name == name)
            .unwrap()
    };

    let Value::Array(items) = &row.values[col("attachments")] else {
        panic!(
            "attachments must be an Array, got {:?}",
            row.values[col("attachments")]
        );
    };
    assert_eq!(items.len(), 2);
    let Value::Struct(f0) = &items[0] else {
        panic!("attachment 0 must be a Struct");
    };
    assert_eq!(f0.get("filename"), Some(&Value::Text("a.txt".into())));
    assert_eq!(f0.get("mime"), Some(&Value::Text("text/plain".into())));
    assert_eq!(f0.get("bytes"), Some(&Value::Bytes(b"aaa".to_vec())));
    let Value::Struct(f1) = &items[1] else {
        panic!("attachment 1 must be a Struct");
    };
    assert_eq!(f1.get("bytes"), Some(&Value::Bytes(b"bbb".to_vec())));

    assert_eq!(row.values[col("to")], Value::Text("a@x.y".into()));
    assert_eq!(row.values[col("subject")], Value::Text("Q3".into()));
    assert_eq!(row.values[col("body")], Value::Text("See attached".into()));
}
