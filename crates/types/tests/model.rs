//! Unit / golden / property tests for the `cfs-types` type & schema model (t05).
//!
//! Covers: construction; struct/array nesting; `resolve_path`; `project`; `expand`
//! (Array(Struct), Array(scalar), Struct, NotExpandable); `unify` widening matrix +
//! Json fallback (golden); `typecheck_predicate` accept/reject; the `typecheck`-style
//! fixture pipeline schema flow (FROM |> WHERE |> EXPAND |> SELECT a.b.c); and the
//! property checks (Row conformance round-trip, unify idempotence/commutativity).

// Integration test: assertions may panic/unwrap freely.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use cfs_types::{
    typecheck_predicate, CmpOp, ColRef, Column, ColumnType, Literal, Pattern, Predicate, Row,
    RowBatch, Schema, TypeError, TypedPredicate, Value,
};

/// A reusable nested fixture schema:
/// `id: Int, name: Text, addr: Struct{ city: Text, geo: Struct{ lat: Float } },
///  tags: Array(Text), items: Array(Struct{ sku: Text, qty: Int }), meta: Json`.
fn fixture_schema() -> Schema {
    let geo = Schema::new(vec![Column::new("lat", ColumnType::Float, false)]);
    let addr = Schema::new(vec![
        Column::new("city", ColumnType::Text, true),
        Column::new("geo", ColumnType::Struct(geo), true),
    ]);
    let item = Schema::new(vec![
        Column::new("sku", ColumnType::Text, false),
        Column::new("qty", ColumnType::Int, false),
    ]);
    Schema::new(vec![
        Column::new("id", ColumnType::Int, false),
        Column::new("name", ColumnType::Text, true),
        Column::new("addr", ColumnType::Struct(addr), true),
        Column::new("tags", ColumnType::Array(Box::new(ColumnType::Text)), true),
        Column::new(
            "items",
            ColumnType::Array(Box::new(ColumnType::Struct(item))),
            true,
        ),
        Column::new("meta", ColumnType::Json, true),
    ])
}

#[test]
fn construction_and_lookup() {
    let s = fixture_schema();
    assert_eq!(s.columns.len(), 6);
    assert_eq!(s.column("id").unwrap().ty, ColumnType::Int);
    assert!(s.column("nope").is_none());
    assert_eq!(
        s.column_names(),
        vec!["id", "name", "addr", "tags", "items", "meta"]
    );
}

#[test]
fn resolve_path_walks_nested_structs() {
    let s = fixture_schema();
    // a.b.c style navigation into nested structs.
    assert_eq!(
        s.resolve_path(&["addr".into(), "geo".into(), "lat".into()])
            .unwrap(),
        ColumnType::Float
    );
    assert_eq!(
        s.resolve_path(&["addr".into(), "city".into()]).unwrap(),
        ColumnType::Text
    );
}

#[test]
fn resolve_path_unknown_column_carries_available() {
    let s = fixture_schema();
    let err = s.resolve_path(&["missing".into()]).unwrap_err();
    assert_eq!(err.code(), "unknown_column");
    match &err {
        TypeError::UnknownColumn { name, available } => {
            assert_eq!(name, "missing");
            assert!(available.contains(&"id".to_string()));
        }
        other => panic!("expected UnknownColumn, got {other:?}"),
    }
}

#[test]
fn resolve_path_into_scalar_is_not_a_struct() {
    let s = fixture_schema();
    let err = s.resolve_path(&["name".into(), "nope".into()]).unwrap_err();
    assert!(matches!(err, TypeError::NotAStruct { .. }));
    assert_eq!(err.code(), "not_a_struct");
}

#[test]
fn resolve_path_into_json_is_late_bound_unknown() {
    let s = fixture_schema();
    // Descending into Json never errors; it yields Unknown (late-bound, RFD §4).
    assert_eq!(
        s.resolve_path(&["meta".into(), "anything".into(), "deep".into()])
            .unwrap(),
        ColumnType::Unknown
    );
}

#[test]
fn project_selects_subset_in_order() {
    let s = fixture_schema();
    let p = s.project(&["name".into(), "id".into()]).unwrap();
    assert_eq!(p.column_names(), vec!["name", "id"]);
    let err = s.project(&["id".into(), "ghost".into()]).unwrap_err();
    assert!(matches!(err, TypeError::UnknownColumn { .. }));
}

#[test]
fn expand_array_of_struct_flattens_element_columns() {
    let s = fixture_schema();
    // EXPAND items (Array(Struct{sku,qty})) replaces `items` with sku,qty in place.
    let e = s.expand(&"items".into()).unwrap();
    let names = e.column_names();
    assert_eq!(
        names,
        vec!["id", "name", "addr", "tags", "sku", "qty", "meta"]
    );
    assert_eq!(e.column("sku").unwrap().ty, ColumnType::Text);
}

#[test]
fn expand_array_of_scalar_yields_element_column() {
    let s = fixture_schema();
    // EXPAND tags (Array(Text)) replaces `tags` with a single Text column.
    let e = s.expand(&"tags".into()).unwrap();
    assert_eq!(e.column("tags").unwrap().ty, ColumnType::Text);
    assert!(!e.column("tags").unwrap().nullable);
}

#[test]
fn expand_struct_flattens_one_level() {
    let s = fixture_schema();
    let e = s.expand(&"addr".into()).unwrap();
    // addr replaced by its fields (city, geo) flattened in place.
    let names = e.column_names();
    assert_eq!(
        names,
        vec!["id", "name", "city", "geo", "tags", "items", "meta"]
    );
}

#[test]
fn expand_scalar_is_not_expandable() {
    let s = fixture_schema();
    let err = s.expand(&"id".into()).unwrap_err();
    assert!(matches!(err, TypeError::NotExpandable { .. }));
    assert_eq!(err.code(), "not_expandable");
}

#[test]
fn unify_divergent_schemas_nullable_widen_and_json_fallback() {
    // Side A: id:Int(not null), score:Int, shape:Struct{x:Int}, only_a:Text.
    let a = Schema::new(vec![
        Column::new("id", ColumnType::Int, false),
        Column::new("score", ColumnType::Int, false),
        Column::new(
            "shape",
            ColumnType::Struct(Schema::new(vec![Column::new("x", ColumnType::Int, false)])),
            false,
        ),
        Column::new("only_a", ColumnType::Text, false),
    ]);
    // Side B: id:Int, score:Float (numeric widen), shape:Text (irreconcilable->Json),
    // only_b:Bool.
    let b = Schema::new(vec![
        Column::new("id", ColumnType::Int, false),
        Column::new("score", ColumnType::Float, false),
        Column::new("shape", ColumnType::Text, false),
        Column::new("only_b", ColumnType::Bool, false),
    ]);

    let u = Schema::unify(&a, &b).unwrap();

    // id: identical, stays Int, not nullable.
    assert_eq!(u.column("id").unwrap().ty, ColumnType::Int);
    assert!(!u.column("id").unwrap().nullable);
    // score: Int ∨ Float ⇒ Float.
    assert_eq!(u.column("score").unwrap().ty, ColumnType::Float);
    // shape: Struct ∨ Text ⇒ Json fallback.
    assert_eq!(u.column("shape").unwrap().ty, ColumnType::Json);
    // only_a present in A only ⇒ kept, nullable.
    assert!(u.column("only_a").unwrap().nullable);
    // only_b present in B only ⇒ appended, nullable.
    assert!(u.column("only_b").unwrap().nullable);
    // Column order: A's columns first, then B-only columns.
    assert_eq!(
        u.column_names(),
        vec!["id", "score", "shape", "only_a", "only_b"]
    );
}

#[test]
fn unify_golden_snapshot() {
    let a = Schema::new(vec![
        Column::new("a", ColumnType::Int, false),
        Column::new("b", ColumnType::Text, false),
    ]);
    let b = Schema::new(vec![
        Column::new("a", ColumnType::Float, false),
        Column::new("c", ColumnType::Bool, false),
    ]);
    let u = Schema::unify(&a, &b).unwrap();
    let json = serde_json::to_value(&u).unwrap();
    // Golden snapshot of the unified schema (a widens to Float; b kept nullable;
    // c appended nullable).
    let expected = serde_json::json!({
        "columns": [
            {"name": "a", "ty": "Float", "nullable": false,
             "provenance": {"driver": null, "source_col": null}},
            {"name": "b", "ty": "Text", "nullable": true,
             "provenance": {"driver": null, "source_col": null}},
            {"name": "c", "ty": "Bool", "nullable": true,
             "provenance": {"driver": null, "source_col": null}}
        ]
    });
    assert_eq!(json, expected);
}

#[test]
fn typecheck_predicate_accepts_well_typed_and_carries_type() {
    let s = fixture_schema();
    let p = Predicate::Cmp(ColRef::col("id"), CmpOp::Lt, Literal::Int(5));
    let tp = typecheck_predicate(&p, &s).unwrap();
    match tp {
        TypedPredicate::Cmp { col_ty, .. } => assert_eq!(col_ty, ColumnType::Int),
        other => panic!("expected Cmp, got {other:?}"),
    }
}

#[test]
fn typecheck_predicate_rejects_int_lt_text() {
    let s = fixture_schema();
    let p = Predicate::Cmp(ColRef::col("id"), CmpOp::Lt, Literal::Text("x".into()));
    let err = typecheck_predicate(&p, &s).unwrap_err();
    match err {
        TypeError::IncomparableTypes { op, lhs, rhs } => {
            assert_eq!(op, CmpOp::Lt);
            assert_eq!(lhs, ColumnType::Int);
            assert_eq!(rhs, ColumnType::Text);
        }
        other => panic!("expected IncomparableTypes, got {other:?}"),
    }
}

#[test]
fn typecheck_predicate_like_requires_text() {
    let s = fixture_schema();
    // LIKE on a Text column: ok.
    let ok = Predicate::Like(ColRef::col("name"), Pattern("a%".into()));
    assert!(typecheck_predicate(&ok, &s).is_ok());
    // LIKE on an Int column: rejected.
    let bad = Predicate::Like(ColRef::col("id"), Pattern("a%".into()));
    let err = typecheck_predicate(&bad, &s).unwrap_err();
    assert!(matches!(err, TypeError::IncomparableTypes { .. }));
}

#[test]
fn typecheck_predicate_numeric_widening_and_logical_structure() {
    let s = fixture_schema();
    // id (Int) = 1.0 (Float): numeric widening allows equality.
    let p = Predicate::And(
        Box::new(Predicate::Cmp(
            ColRef::col("id"),
            CmpOp::Eq,
            Literal::Float(1.0),
        )),
        Box::new(Predicate::Not(Box::new(Predicate::Like(
            ColRef::col("name"),
            Pattern("z%".into()),
        )))),
    );
    assert!(typecheck_predicate(&p, &s).is_ok());
}

#[test]
fn typecheck_predicate_in_and_between() {
    let s = fixture_schema();
    let in_ok = Predicate::In(ColRef::col("id"), vec![Literal::Int(1), Literal::Int(2)]);
    assert!(typecheck_predicate(&in_ok, &s).is_ok());
    let between_bad = Predicate::Between(
        ColRef::col("id"),
        Literal::Text("a".into()),
        Literal::Int(9),
    );
    assert!(typecheck_predicate(&between_bad, &s).is_err());
}

/// The fixture-pipeline schema flow: `FROM fixture |> WHERE id < 5 |>
/// EXPAND items |> SELECT sku, qty` — assert the decorated output schema with **no
/// live creds** (pure, RFD §4). Stands in for the `typecheck(stmt, root)` flow: each
/// op maps Schema -> Schema.
#[test]
fn pipeline_schema_flow_from_where_expand_select() {
    let root = fixture_schema();

    // WHERE id < 5: type-checks, schema unchanged.
    let where_pred = Predicate::Cmp(ColRef::col("id"), CmpOp::Lt, Literal::Int(5));
    typecheck_predicate(&where_pred, &root).unwrap();
    let after_where = root.clone();

    // EXPAND items: Array(Struct{sku,qty}) flattened into rows.
    let after_expand = after_where.expand(&"items".into()).unwrap();

    // SELECT sku, qty: project the exploded element columns.
    let out = after_expand.project(&["sku".into(), "qty".into()]).unwrap();

    assert_eq!(out.column_names(), vec!["sku", "qty"]);
    assert_eq!(out.column("sku").unwrap().ty, ColumnType::Text);
    assert_eq!(out.column("qty").unwrap().ty, ColumnType::Int);
}

// ---- Property checks (deterministic, fixed inputs — no RNG needed) ----

#[test]
fn property_row_conforms_to_its_schema() {
    let schema = Schema::new(vec![
        Column::new("b", ColumnType::Bool, false),
        Column::new("i", ColumnType::Int, false),
        Column::new("t", ColumnType::Text, true),
        Column::new(
            "nested",
            ColumnType::Struct(Schema::new(vec![Column::new("x", ColumnType::Int, false)])),
            false,
        ),
        Column::new("arr", ColumnType::Array(Box::new(ColumnType::Int)), true),
    ]);
    let row = Row::new(vec![
        Value::Bool(true),
        Value::Int(7),
        Value::Null, // t is nullable
        Value::Struct(Row::new(vec![Value::Int(1)])),
        Value::Array(vec![Value::Int(1), Value::Int(2)]),
    ]);
    assert!(row.conforms_to(&schema));
    let batch = RowBatch::new(schema, vec![row]);
    assert!(batch.is_conformant());
}

#[test]
fn property_type_of_round_trips_scalars() {
    assert_eq!(Value::Bool(true).type_of(), ColumnType::Bool);
    assert_eq!(Value::Int(1).type_of(), ColumnType::Int);
    assert_eq!(Value::Float(1.0).type_of(), ColumnType::Float);
    assert_eq!(Value::Text("x".into()).type_of(), ColumnType::Text);
    assert_eq!(Value::Bytes(vec![1]).type_of(), ColumnType::Bytes);
    assert_eq!(Value::Null.type_of(), ColumnType::Unknown);
}

#[test]
fn property_unify_is_idempotent() {
    let s = fixture_schema();
    assert_eq!(Schema::unify(&s, &s).unwrap(), s);
}

#[test]
fn property_unify_commutative_up_to_column_order() {
    let a = Schema::new(vec![
        Column::new("x", ColumnType::Int, false),
        Column::new("y", ColumnType::Text, false),
    ]);
    let b = Schema::new(vec![
        Column::new("y", ColumnType::Text, false),
        Column::new("z", ColumnType::Bool, false),
    ]);
    let ab = Schema::unify(&a, &b).unwrap();
    let ba = Schema::unify(&b, &a).unwrap();

    // Same set of (name, type, nullable), possibly reordered.
    let mut ab_cols: Vec<_> = ab
        .columns
        .iter()
        .map(|c| (c.name.clone(), c.ty.clone(), c.nullable))
        .collect();
    let mut ba_cols: Vec<_> = ba
        .columns
        .iter()
        .map(|c| (c.name.clone(), c.ty.clone(), c.nullable))
        .collect();
    ab_cols.sort_by(|l, r| l.0.cmp(&r.0));
    ba_cols.sort_by(|l, r| l.0.cmp(&r.0));
    assert_eq!(ab_cols, ba_cols);
}
