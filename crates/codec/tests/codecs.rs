//! Unit / golden / round-trip tests for the t15 codec layer: the six builtin codecs
//! (`json`, `jsonl`, `yaml`, `toml`, `csv`, `md+frontmatter`), the struct/array nesting
//! they produce, the value-level `EXPAND` / `a.b.c` path access, and the structured
//! (no-panic) error behaviour on malformed input.
//!
//! Acceptance criteria covered (ticket t15):
//! - each format decodes a fixture to an asserted `RowBatch`;
//! - markdown+frontmatter yields frontmatter columns + a `body` column;
//! - `decode → encode → decode` is value-stable (semantic round-trip);
//! - `EXPAND items` over a `Value::Array` produces one row per element; scalar/absent
//!   `EXPAND` follows the documented rule; `a.b.c` navigates structs without flattening;
//! - malformed input returns a structured `CfsError::Decode` (never a panic).

// Integration test: assertions may panic/unwrap freely.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use cfs_codec::{
    access_row, builtin_codecs, expand, Codec, CsvCodec, JsonCodec, JsonlCodec,
    MarkdownFrontmatterCodec, TomlCodec, YamlCodec,
};
use cfs_driver::CfsError;
use cfs_types::{Column, ColumnType, Row, RowBatch, Schema, Value};

// ---------------------------------------------------------------------------
// JSON
// ---------------------------------------------------------------------------

#[test]
fn json_decodes_object_to_single_row() {
    let batch = JsonCodec
        .decode(br#"{"a": 1, "b": "x", "ok": true}"#)
        .unwrap();
    assert_eq!(batch.rows.len(), 1);
    assert_eq!(batch.schema.column_names(), vec!["a", "b", "ok"]);
    assert_eq!(
        batch.rows[0].values,
        vec![Value::Int(1), Value::Text("x".into()), Value::Bool(true)]
    );
}

#[test]
fn json_top_level_array_is_one_row_per_element() {
    let batch = JsonCodec.decode(br#"[{"a":1},{"a":2},{"a":3}]"#).unwrap();
    assert_eq!(batch.rows.len(), 3);
    assert_eq!(batch.rows[2].values, vec![Value::Int(3)]);
}

#[test]
fn json_nested_object_becomes_struct_and_array_becomes_array() {
    let batch = JsonCodec
        .decode(br#"{"meta": {"k": 1}, "tags": ["a", "b"]}"#)
        .unwrap();
    let row = &batch.rows[0];
    assert!(matches!(row.values[0], Value::Struct(_)));
    match &row.values[1] {
        Value::Array(items) => assert_eq!(items.len(), 2),
        other => panic!("expected array, got {other:?}"),
    }
    // The struct column type is recovered structurally (no flattening).
    assert!(matches!(batch.schema.columns[0].ty, ColumnType::Struct(_)));
    assert!(matches!(batch.schema.columns[1].ty, ColumnType::Array(_)));
}

#[test]
fn json_heterogeneous_rows_unify_with_nullable_missing_columns() {
    // Second row lacks `b`; the union schema keeps `b` nullable and fills Null.
    let batch = JsonCodec.decode(br#"[{"a":1,"b":2},{"a":3}]"#).unwrap();
    assert_eq!(batch.schema.column_names(), vec!["a", "b"]);
    let b_col = batch.schema.column("b").unwrap();
    assert!(b_col.nullable);
    assert_eq!(batch.rows[1].values, vec![Value::Int(3), Value::Null]);
    assert!(batch.is_conformant());
}

#[test]
fn json_roundtrip_is_value_stable() {
    let input = br#"[{"a":1,"b":"x"},{"a":2,"b":"y"}]"#;
    let once = JsonCodec.decode(input).unwrap();
    let encoded = JsonCodec.encode(&once).unwrap();
    let twice = JsonCodec.decode(&encoded).unwrap();
    assert_eq!(once, twice);
}

#[test]
fn json_malformed_input_is_structured_error_not_panic() {
    let err = JsonCodec.decode(b"{not valid json").unwrap_err();
    assert!(matches!(err, CfsError::Decode { fmt: "json", .. }));
    assert_eq!(err.code(), "decode_error");
}

// ---------------------------------------------------------------------------
// JSONL
// ---------------------------------------------------------------------------

#[test]
fn jsonl_one_row_per_line_skipping_blanks() {
    let batch = JsonlCodec.decode(b"{\"a\":1}\n\n{\"a\":2}\n").unwrap();
    assert_eq!(batch.rows.len(), 2);
}

#[test]
fn jsonl_roundtrip_is_value_stable() {
    let once = JsonlCodec.decode(b"{\"a\":1}\n{\"a\":2}\n").unwrap();
    let encoded = JsonlCodec.encode(&once).unwrap();
    let twice = JsonlCodec.decode(&encoded).unwrap();
    assert_eq!(once, twice);
}

#[test]
fn jsonl_malformed_line_is_structured_error() {
    let err = JsonlCodec.decode(b"{\"a\":1}\nnot json\n").unwrap_err();
    assert!(matches!(err, CfsError::Decode { fmt: "jsonl", .. }));
}

// ---------------------------------------------------------------------------
// YAML
// ---------------------------------------------------------------------------

#[test]
fn yaml_mapping_decodes_to_one_row() {
    let batch = YamlCodec.decode(b"a: 1\nb: hello\n").unwrap();
    assert_eq!(batch.rows.len(), 1);
    assert_eq!(
        batch.rows[0].values,
        vec![Value::Int(1), Value::Text("hello".into())]
    );
}

#[test]
fn yaml_sequence_is_one_row_per_element() {
    let batch = YamlCodec.decode(b"- a: 1\n- a: 2\n").unwrap();
    assert_eq!(batch.rows.len(), 2);
}

#[test]
fn yaml_roundtrip_is_value_stable() {
    let once = YamlCodec.decode(b"a: 1\nb: x\n").unwrap();
    let encoded = YamlCodec.encode(&once).unwrap();
    let twice = YamlCodec.decode(&encoded).unwrap();
    assert_eq!(once, twice);
}

#[test]
fn yaml_malformed_input_is_structured_error() {
    let err = YamlCodec.decode(b"a: [unterminated").unwrap_err();
    assert!(matches!(err, CfsError::Decode { fmt: "yaml", .. }));
}

// ---------------------------------------------------------------------------
// TOML
// ---------------------------------------------------------------------------

#[test]
fn toml_table_decodes_to_single_row_with_nested_struct() {
    let batch = TomlCodec
        .decode(b"name = \"cfs\"\nversion = 1\n\n[meta]\nk = 2\n")
        .unwrap();
    assert_eq!(batch.rows.len(), 1);
    assert!(batch.schema.column("name").is_some());
    assert!(matches!(
        batch.schema.column("meta").unwrap().ty,
        ColumnType::Struct(_)
    ));
}

#[test]
fn toml_roundtrip_is_value_stable() {
    let once = TomlCodec.decode(b"a = 1\nb = \"x\"\n").unwrap();
    let encoded = TomlCodec.encode(&once).unwrap();
    let twice = TomlCodec.decode(&encoded).unwrap();
    assert_eq!(once, twice);
}

#[test]
fn toml_malformed_input_is_structured_error() {
    let err = TomlCodec.decode(b"a = = 1").unwrap_err();
    assert!(matches!(err, CfsError::Decode { fmt: "toml", .. }));
}

// ---------------------------------------------------------------------------
// CSV
// ---------------------------------------------------------------------------

#[test]
fn csv_header_becomes_columns_with_light_type_hints() {
    let batch = CsvCodec::default()
        .decode(b"id,name,active\n1,alice,true\n2,bob,false\n")
        .unwrap();
    assert_eq!(batch.schema.column_names(), vec!["id", "name", "active"]);
    assert_eq!(batch.rows.len(), 2);
    assert_eq!(
        batch.rows[0].values,
        vec![
            Value::Int(1),
            Value::Text("alice".into()),
            Value::Bool(true)
        ]
    );
    assert_eq!(batch.schema.column("id").unwrap().ty, ColumnType::Int);
}

#[test]
fn csv_empty_cell_is_null_and_column_is_nullable() {
    let batch = CsvCodec::default().decode(b"a,b\n1,\n2,x\n").unwrap();
    assert_eq!(batch.rows[0].values[1], Value::Null);
    assert!(batch.schema.column("b").unwrap().nullable);
}

#[test]
fn csv_roundtrip_is_value_stable() {
    let once = CsvCodec::default().decode(b"a,b\n1,x\n2,y\n").unwrap();
    let encoded = CsvCodec::default().encode(&once).unwrap();
    let twice = CsvCodec::default().decode(&encoded).unwrap();
    assert_eq!(once.rows, twice.rows);
}

#[test]
fn csv_custom_delimiter_tsv() {
    let batch = CsvCodec::with_delimiter(b'\t')
        .decode(b"a\tb\n1\tx\n")
        .unwrap();
    assert_eq!(
        batch.rows[0].values,
        vec![Value::Int(1), Value::Text("x".into())]
    );
}

// ---------------------------------------------------------------------------
// Markdown + frontmatter
// ---------------------------------------------------------------------------

#[test]
fn markdown_frontmatter_yields_columns_plus_body() {
    let doc = b"---\ntitle: Hello\ntags:\n  - a\n  - b\n---\n# Heading\n\nBody text.\n";
    let batch = MarkdownFrontmatterCodec.decode(doc).unwrap();
    assert_eq!(batch.rows.len(), 1);
    assert!(batch.schema.column("title").is_some());
    assert!(batch.schema.column("tags").is_some());
    let body_col = batch.schema.column("body").expect("body column present");
    assert_eq!(body_col.ty, ColumnType::Text);
    let body_idx = batch
        .schema
        .columns
        .iter()
        .position(|c| c.name == "body")
        .unwrap();
    assert_eq!(
        batch.rows[0].values[body_idx],
        Value::Text("# Heading\n\nBody text.\n".into())
    );
}

#[test]
fn markdown_no_frontmatter_is_body_only() {
    let batch = MarkdownFrontmatterCodec.decode(b"just text\n").unwrap();
    assert_eq!(batch.schema.column_names(), vec!["body"]);
    assert_eq!(
        batch.rows[0].values,
        vec![Value::Text("just text\n".into())]
    );
}

#[test]
fn markdown_roundtrip_preserves_frontmatter_and_body() {
    let doc = b"---\ntitle: T\nn: 3\n---\nbody here\n";
    let once = MarkdownFrontmatterCodec.decode(doc).unwrap();
    let encoded = MarkdownFrontmatterCodec.encode(&once).unwrap();
    let twice = MarkdownFrontmatterCodec.decode(&encoded).unwrap();
    assert_eq!(once, twice);
}

#[test]
fn markdown_malformed_frontmatter_is_structured_error() {
    let err = MarkdownFrontmatterCodec
        .decode(b"---\n: : bad yaml :\n---\nbody")
        .unwrap_err();
    assert!(matches!(
        err,
        CfsError::Decode {
            fmt: "md+frontmatter",
            ..
        }
    ));
}

// ---------------------------------------------------------------------------
// EXPAND (value level)
// ---------------------------------------------------------------------------

fn batch_with_array_column() -> RowBatch {
    // schema: id Int, items Array(Int)
    let schema = Schema::new(vec![
        Column::new("id", ColumnType::Int, false),
        Column::new("items", ColumnType::Array(Box::new(ColumnType::Int)), false),
    ]);
    let rows = vec![
        Row::new(vec![
            Value::Int(1),
            Value::Array(vec![Value::Int(10), Value::Int(20)]),
        ]),
        Row::new(vec![Value::Int(2), Value::Array(vec![])]),
    ];
    RowBatch::new(schema, rows)
}

#[test]
fn expand_array_produces_one_row_per_element() {
    let out = expand(&batch_with_array_column(), "items");
    // Row 1 has 2 elements → 2 rows; row 2 has an empty array → 0 rows.
    assert_eq!(out.rows.len(), 2);
    assert_eq!(out.rows[0].values, vec![Value::Int(1), Value::Int(10)]);
    assert_eq!(out.rows[1].values, vec![Value::Int(1), Value::Int(20)]);
    // The exploded column keeps the field name and is now scalar Int.
    assert_eq!(out.schema.column("items").unwrap().ty, ColumnType::Int);
}

#[test]
fn expand_array_of_structs_flattens_element_fields() {
    let elem = Schema::new(vec![
        Column::new("x", ColumnType::Int, false),
        Column::new("y", ColumnType::Int, false),
    ]);
    let schema = Schema::new(vec![
        Column::new("id", ColumnType::Int, false),
        Column::new(
            "pts",
            ColumnType::Array(Box::new(ColumnType::Struct(elem))),
            false,
        ),
    ]);
    let rows = vec![Row::new(vec![
        Value::Int(7),
        Value::Array(vec![Value::Struct(Row::new(vec![
            Value::Int(1),
            Value::Int(2),
        ]))]),
    ])];
    let out = expand(&RowBatch::new(schema, rows), "pts");
    assert_eq!(out.schema.column_names(), vec!["id", "x", "y"]);
    assert_eq!(
        out.rows[0].values,
        vec![Value::Int(7), Value::Int(1), Value::Int(2)]
    );
}

#[test]
fn expand_scalar_field_is_passthrough() {
    let schema = Schema::new(vec![Column::new("a", ColumnType::Int, false)]);
    let batch = RowBatch::new(schema, vec![Row::new(vec![Value::Int(5)])]);
    let out = expand(&batch, "a");
    // Not a collection → unchanged passthrough (documented rule).
    assert_eq!(out, batch);
}

#[test]
fn expand_absent_field_is_passthrough() {
    let batch = batch_with_array_column();
    let out = expand(&batch, "nope");
    assert_eq!(out, batch);
}

// ---------------------------------------------------------------------------
// a.b.c path access (value level)
// ---------------------------------------------------------------------------

#[test]
fn path_access_navigates_nested_structs_without_flattening() {
    // schema: meta Struct{ inner Struct{ k Int } }
    let inner = Schema::new(vec![Column::new("k", ColumnType::Int, false)]);
    let mid = Schema::new(vec![Column::new("inner", ColumnType::Struct(inner), false)]);
    let schema = Schema::new(vec![Column::new("meta", ColumnType::Struct(mid), false)]);
    let row = Row::new(vec![Value::Struct(Row::new(vec![Value::Struct(
        Row::new(vec![Value::Int(42)]),
    )]))]);

    let got = access_row(&row, &schema, &["meta", "inner", "k"]).unwrap();
    assert_eq!(got, Value::Int(42));

    // An intermediate struct is returned whole (no flattening).
    let mid_val = access_row(&row, &schema, &["meta", "inner"]).unwrap();
    assert!(matches!(mid_val, Value::Struct(_)));

    // An absent segment resolves to None (not a panic).
    assert!(access_row(&row, &schema, &["meta", "missing"]).is_none());
}

// ---------------------------------------------------------------------------
// Registry / builtins
// ---------------------------------------------------------------------------

#[test]
fn builtin_codecs_cover_all_six_formats() {
    let names: Vec<String> = builtin_codecs()
        .iter()
        .map(|c| c.fmt().to_string())
        .collect();
    for expected in ["json", "jsonl", "yaml", "toml", "csv", "md+frontmatter"] {
        assert!(
            names.contains(&expected.to_string()),
            "missing codec {expected}"
        );
    }
    assert_eq!(names.len(), 6);
}
