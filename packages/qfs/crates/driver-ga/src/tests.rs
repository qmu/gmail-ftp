//! In-crate tests for the GA4 read-only relational driver (t41) — all against a **mocked GA4 Data
//! API + in-memory fixtures**: no live GA, no network, no credentials. They assert the runReport
//! request shape (dimensions/metrics/dateRanges/filters/order/limit), the response → typed-rows
//! decode, the WHERE → GA filter pushdown with a TRUTHFUL residual, read-only enforcement (writes
//! rejected at the gate AND at the applier), property/multi-account selection, sampling surfacing,
//! and that a token never leaks.

use std::sync::Arc;

use qfs_driver::{check_capability, Archetype, Driver, Path, PushdownProfile, Verb};
use qfs_plan::{EffectKind, EffectNode, NodeId, PlanApplier, Target, VfsPath};
use qfs_types::{CmpOp, ColRef, ColumnType, DriverId, Literal, Pattern, Predicate, Value};

use super::*;
use crate::catalog::{Catalog, GaDimension, GaMetric, MetricKind};
use crate::client::{request_to_json, MockGaClient, RecordedCall};
use crate::compile::{
    compile, FilterExpression, FilterTest, NumericOp, QuerySpec, RunReportRequest, StringMatch,
};
use crate::report::{response_to_rows, ReportResponse, ReportRow};

/// A representative property catalog: `date`/`country`/`pagePath` dimensions and
/// `sessions`(int)/`totalRevenue`(currency)/`bounceRate`(float) metrics.
fn fixture_catalog() -> Catalog {
    Catalog::new(
        vec![
            GaDimension::for_test("date", "Date", "Time"),
            GaDimension::for_test("country", "Country", "Geography"),
            GaDimension::for_test("pagePath", "Page path", "Page / screen"),
        ],
        vec![
            GaMetric::for_test("sessions", "Sessions", "Session", MetricKind::Integer),
            GaMetric::for_test(
                "totalRevenue",
                "Total revenue",
                "Revenue",
                MetricKind::Currency,
            ),
            GaMetric::for_test("bounceRate", "Bounce rate", "Session", MetricKind::Float),
        ],
    )
}

/// A `date BETWEEN start AND end` predicate.
fn date_between(start: &str, end: &str) -> Predicate {
    Predicate::Between(
        ColRef::col("date"),
        Literal::Text(start.to_string()),
        Literal::Text(end.to_string()),
    )
}

// ---------------------------------------------------------------------------------------------
// Path parsing
// ---------------------------------------------------------------------------------------------

#[test]
fn path_parses_root_property_and_realtime() {
    assert_eq!(GaPath::parse_str("/ga").unwrap(), GaPath::Root);
    assert_eq!(
        GaPath::parse_str("/ga/123456789").unwrap(),
        GaPath::Property {
            property_id: "123456789".to_string()
        }
    );
    assert_eq!(
        GaPath::parse_str("/ga/123456789/realtime").unwrap(),
        GaPath::Realtime {
            property_id: "123456789".to_string()
        }
    );
    // Property id surfaces for credential/property selection.
    assert_eq!(
        GaPath::parse_str("/ga/123456789/realtime")
            .unwrap()
            .property_id(),
        Some("123456789")
    );
}

#[test]
fn path_rejects_non_ga_and_unexpected_segments() {
    assert!(GaPath::parse_str("/drive/my").is_err());
    let err = GaPath::parse_str("/ga/123/bogus").unwrap_err();
    assert_eq!(err.code(), "invalid_path");
}

// ---------------------------------------------------------------------------------------------
// DESCRIBE / catalog → typed schema
// ---------------------------------------------------------------------------------------------

#[test]
fn describe_schema_maps_dimensions_and_typed_metrics() {
    let schema = fixture_catalog().describe_schema();
    // 3 dimensions + 3 metrics, in catalog order.
    assert_eq!(schema.columns.len(), 6);
    assert_eq!(schema.column("country").unwrap().ty, ColumnType::Text);
    // Metric kinds map to typed columns: integer → Int, currency → Decimal, float → Float.
    assert_eq!(schema.column("sessions").unwrap().ty, ColumnType::Int);
    assert_eq!(
        schema.column("totalRevenue").unwrap().ty,
        ColumnType::Decimal
    );
    assert_eq!(schema.column("bounceRate").unwrap().ty, ColumnType::Float);
}

#[test]
fn fetch_catalog_records_property_and_powers_describe() {
    let mock = MockGaClient::new().with_catalog(fixture_catalog());
    let driver = GaDriver::new(Arc::new(mock));
    let catalog = driver.fetch_catalog("123456789").unwrap();
    assert_eq!(catalog.describe_schema().columns.len(), 6);
    // The driver reports the relational archetype for a property node.
    let desc = driver.describe(&Path::new("/ga/123456789")).unwrap();
    assert_eq!(desc.archetype, Archetype::RelationalTable);
    // The root is not describable as a relation.
    assert!(driver.describe(&Path::new("/ga")).is_err());
}

// ---------------------------------------------------------------------------------------------
// Compile golden: a representative report → the correct RunReportRequest (a value/plan assertion)
// ---------------------------------------------------------------------------------------------

#[test]
fn representative_report_compiles_to_correct_run_report_request() {
    // SELECT country, sessions /ga/123 |> WHERE date BETWEEN '2024-01-01' AND '2024-01-31'
    //   AND country = 'JP' AND sessions > 100 |> ORDER BY sessions DESC |> LIMIT 10
    let predicate = Predicate::And(
        Box::new(Predicate::And(
            Box::new(date_between("2024-01-01", "2024-01-31")),
            Box::new(Predicate::Cmp(
                ColRef::col("country"),
                CmpOp::Eq,
                Literal::Text("JP".to_string()),
            )),
        )),
        Box::new(Predicate::Cmp(
            ColRef::col("sessions"),
            CmpOp::Gt,
            Literal::Int(100),
        )),
    );
    let spec = QuerySpec::new(vec!["country".to_string(), "sessions".to_string()])
        .with_predicate(predicate)
        .order_by("sessions", true)
        .with_limit(10);

    let result = compile("123456789", false, &fixture_catalog(), &spec).unwrap();
    let req = &result.request;

    assert_eq!(req.property_id, "123456789");
    assert!(!req.realtime);
    assert_eq!(req.dimensions, vec!["country".to_string()]);
    assert_eq!(req.metrics, vec!["sessions".to_string()]);
    assert_eq!(
        req.date_ranges,
        vec![DateRange {
            start_date: "2024-01-01".to_string(),
            end_date: "2024-01-31".to_string(),
        }]
    );
    // country = 'JP' → EXACT stringFilter (dimensionFilter).
    assert_eq!(
        req.dimension_filter,
        Some(FilterExpression::Filter {
            field_name: "country".to_string(),
            test: FilterTest::String {
                value: "JP".to_string(),
                match_type: StringMatch::Exact,
            },
        })
    );
    // sessions > 100 → GREATER_THAN numericFilter (metricFilter).
    assert_eq!(
        req.metric_filter,
        Some(FilterExpression::Filter {
            field_name: "sessions".to_string(),
            test: FilterTest::Numeric {
                op: NumericOp::GreaterThan,
                value: "100".to_string(),
            },
        })
    );
    assert_eq!(req.order_bys.len(), 1);
    assert!(req.order_bys[0].desc && req.order_bys[0].is_metric);
    assert_eq!(req.order_bys[0].field_name, "sessions");
    assert_eq!(req.limit, Some(10));

    // All three conjuncts mapped EXACTLY (date range + exact string + numeric) → NO residual.
    assert_eq!(result.residual, None, "exact mappings drop the residual");
}

// ---------------------------------------------------------------------------------------------
// WHERE → GA filter pushdown with a TRUTHFUL residual (the t20/t21 lesson)
// ---------------------------------------------------------------------------------------------

#[test]
fn exact_dimension_equality_drops_residual() {
    let spec = QuerySpec::new(vec!["country".to_string(), "sessions".to_string()]).with_predicate(
        Predicate::And(
            Box::new(date_between("2024-01-01", "2024-01-31")),
            Box::new(Predicate::Cmp(
                ColRef::col("country"),
                CmpOp::Eq,
                Literal::Text("US".to_string()),
            )),
        ),
    );
    let result = compile("123", false, &fixture_catalog(), &spec).unwrap();
    assert!(matches!(
        result.request.dimension_filter,
        Some(FilterExpression::Filter {
            test: FilterTest::String {
                match_type: StringMatch::Exact,
                ..
            },
            ..
        })
    ));
    assert_eq!(result.residual, None);
}

#[test]
fn loose_like_pushes_contains_but_keeps_residual() {
    // pagePath LIKE '/blog' is a CONTAINS pre-filter (looser than SQL LIKE), so the predicate is
    // KEPT as residual — over-fetch then filter, never wrong rows.
    let like = Predicate::Like(ColRef::col("pagePath"), Pattern("/blog".to_string()));
    let spec = QuerySpec::new(vec!["pagePath".to_string(), "sessions".to_string()]).with_predicate(
        Predicate::And(
            Box::new(date_between("2024-01-01", "2024-01-31")),
            Box::new(like.clone()),
        ),
    );
    let result = compile("123", false, &fixture_catalog(), &spec).unwrap();
    assert!(matches!(
        result.request.dimension_filter,
        Some(FilterExpression::Filter {
            test: FilterTest::String {
                match_type: StringMatch::Contains,
                ..
            },
            ..
        })
    ));
    // The loose CONTAINS pre-filter KEEPS the exact LIKE predicate as residual.
    assert_eq!(result.residual, Some(like));
}

#[test]
fn regex_match_pushes_full_regexp_but_keeps_residual() {
    let m = Predicate::Cmp(
        ColRef::col("pagePath"),
        CmpOp::Match,
        Literal::Text("^/blog".to_string()),
    );
    let spec = QuerySpec::new(vec!["pagePath".to_string(), "sessions".to_string()]).with_predicate(
        Predicate::And(
            Box::new(date_between("2024-01-01", "2024-01-31")),
            Box::new(m.clone()),
        ),
    );
    let result = compile("123", false, &fixture_catalog(), &spec).unwrap();
    assert!(matches!(
        result.request.dimension_filter,
        Some(FilterExpression::Filter {
            test: FilterTest::String {
                match_type: StringMatch::FullRegexp,
                ..
            },
            ..
        })
    ));
    assert_eq!(result.residual, Some(m));
}

#[test]
fn or_predicate_stays_wholly_residual() {
    // An OR cannot be expressed by GA's andGroup, so it pushes nothing and stays residual.
    let or = Predicate::Or(
        Box::new(Predicate::Cmp(
            ColRef::col("country"),
            CmpOp::Eq,
            Literal::Text("US".to_string()),
        )),
        Box::new(Predicate::Cmp(
            ColRef::col("country"),
            CmpOp::Eq,
            Literal::Text("JP".to_string()),
        )),
    );
    let spec = QuerySpec::new(vec!["country".to_string(), "sessions".to_string()]).with_predicate(
        Predicate::And(
            Box::new(date_between("2024-01-01", "2024-01-31")),
            Box::new(or.clone()),
        ),
    );
    let result = compile("123", false, &fixture_catalog(), &spec).unwrap();
    assert_eq!(result.request.dimension_filter, None);
    assert_eq!(result.residual, Some(or));
}

#[test]
fn metric_comparison_operators_map_exactly() {
    for (op, ga) in [
        (CmpOp::Eq, NumericOp::Equal),
        (CmpOp::Lt, NumericOp::LessThan),
        (CmpOp::Le, NumericOp::LessThanOrEqual),
        (CmpOp::Gt, NumericOp::GreaterThan),
        (CmpOp::Ge, NumericOp::GreaterThanOrEqual),
    ] {
        let spec = QuerySpec::new(vec!["sessions".to_string()]).with_predicate(Predicate::And(
            Box::new(date_between("2024-01-01", "2024-01-31")),
            Box::new(Predicate::Cmp(ColRef::col("sessions"), op, Literal::Int(5))),
        ));
        let result = compile("123", false, &fixture_catalog(), &spec).unwrap();
        assert_eq!(
            result.request.metric_filter,
            Some(FilterExpression::Filter {
                field_name: "sessions".to_string(),
                test: FilterTest::Numeric {
                    op: ga,
                    value: "5".to_string()
                },
            })
        );
        assert_eq!(result.residual, None, "exact numeric op drops residual");
    }
}

// ---------------------------------------------------------------------------------------------
// Mandatory date range + structured field errors
// ---------------------------------------------------------------------------------------------

#[test]
fn missing_date_range_is_a_structured_error() {
    let spec = QuerySpec::new(vec!["sessions".to_string()]);
    let err = compile("123", false, &fixture_catalog(), &spec).unwrap_err();
    assert_eq!(err.code(), "missing_date_range");
}

#[test]
fn realtime_needs_no_date_range() {
    let spec = QuerySpec::new(vec!["country".to_string(), "sessions".to_string()]);
    let result = compile("123", true, &fixture_catalog(), &spec).unwrap();
    assert!(result.request.realtime);
    assert!(result.request.date_ranges.is_empty());
}

#[test]
fn empty_projection_is_a_structured_error() {
    let spec = QuerySpec::new(vec![]);
    let err = compile("123", false, &fixture_catalog(), &spec).unwrap_err();
    assert_eq!(err.code(), "empty_projection");
}

#[test]
fn unknown_field_is_a_structured_error_not_a_raw_400() {
    let spec = QuerySpec::new(vec!["notAColumn".to_string()])
        .with_predicate(date_between("2024-01-01", "2024-01-31"));
    let err = compile("123", false, &fixture_catalog(), &spec).unwrap_err();
    assert_eq!(err.code(), "unknown_field");
}

#[test]
fn half_open_date_bounds_form_the_range() {
    // WHERE date >= 'a' AND date <= 'b' → a single dateRange.
    let spec = QuerySpec::new(vec!["sessions".to_string()]).with_predicate(Predicate::And(
        Box::new(Predicate::Cmp(
            ColRef::col("date"),
            CmpOp::Ge,
            Literal::Text("2024-02-01".to_string()),
        )),
        Box::new(Predicate::Cmp(
            ColRef::col("date"),
            CmpOp::Le,
            Literal::Text("2024-02-29".to_string()),
        )),
    ));
    let result = compile("123", false, &fixture_catalog(), &spec).unwrap();
    assert_eq!(
        result.request.date_ranges,
        vec![DateRange {
            start_date: "2024-02-01".to_string(),
            end_date: "2024-02-29".to_string(),
        }]
    );
    assert_eq!(result.residual, None);
}

// ---------------------------------------------------------------------------------------------
// Read-only enforcement: writes rejected at the parse-time gate AND at the applier
// ---------------------------------------------------------------------------------------------

#[test]
fn select_is_allowed_writes_rejected_at_capability_gate() {
    let driver = GaDriver::new(Arc::new(MockGaClient::new()));
    let path = Path::new("/ga/123456789");
    // SELECT passes.
    assert!(check_capability(&driver, &path, Verb::Select).is_ok());
    // Every write verb is rejected structurally with the supported set for AI recovery.
    for verb in [Verb::Insert, Verb::Upsert, Verb::Update, Verb::Remove] {
        let err = check_capability(&driver, &path, verb).unwrap_err();
        assert_eq!(err.code(), "unsupported_verb");
    }
}

#[test]
fn applier_rejects_every_write_as_read_only() {
    let mut applier = GaApplier::new(Arc::new(MockGaClient::new()));
    let target = Target::new(DriverId::new("ga"), VfsPath::new("/ga/123"));
    let node = EffectNode::new(NodeId(0), EffectKind::Insert, target);
    // The synchronous PlanApplier leg rejects it.
    let err = PlanApplier::apply(&mut applier, &node).unwrap_err();
    assert!(err.to_string().contains("read-only"));
    // The shared (async-bridge) leg rejects it with a capability-denied class.
    let shared_err = qfs_runtime::SharedApplier::apply_shared(&applier, &node).unwrap_err();
    assert_eq!(shared_err.code(), "capability_denied");
}

// ---------------------------------------------------------------------------------------------
// runReport request shape (JSON) + response → typed rows + sampling + multi-account
// ---------------------------------------------------------------------------------------------

#[test]
fn request_to_json_has_the_expected_ga_wire_shape() {
    let req = RunReportRequest {
        property_id: "123".to_string(),
        realtime: false,
        dimensions: vec!["country".to_string()],
        metrics: vec!["sessions".to_string()],
        date_ranges: vec![DateRange {
            start_date: "2024-01-01".to_string(),
            end_date: "2024-01-31".to_string(),
        }],
        dimension_filter: Some(FilterExpression::Filter {
            field_name: "country".to_string(),
            test: FilterTest::String {
                value: "JP".to_string(),
                match_type: StringMatch::Exact,
            },
        }),
        metric_filter: None,
        order_bys: vec![OrderBy {
            field_name: "sessions".to_string(),
            desc: true,
            is_metric: true,
        }],
        limit: Some(10),
    };
    let json = request_to_json(&req);
    assert_eq!(json["dimensions"][0]["name"], "country");
    assert_eq!(json["metrics"][0]["name"], "sessions");
    assert_eq!(json["dateRanges"][0]["startDate"], "2024-01-01");
    assert_eq!(
        json["dimensionFilter"]["filter"]["stringFilter"]["matchType"],
        "EXACT"
    );
    assert_eq!(json["orderBys"][0]["metric"]["metricName"], "sessions");
    assert_eq!(json["orderBys"][0]["desc"], true);
    // GA carries limit as a string.
    assert_eq!(json["limit"], "10");
    // A realtime request omits dateRanges entirely.
    let rt = RunReportRequest {
        realtime: true,
        date_ranges: vec![],
        ..req
    };
    assert!(request_to_json(&rt).get("dateRanges").is_none());
}

#[test]
fn response_decodes_into_typed_rows_per_metric_kind() {
    let catalog = fixture_catalog();
    let req = RunReportRequest {
        property_id: "123".to_string(),
        dimensions: vec!["country".to_string()],
        metrics: vec!["sessions".to_string(), "totalRevenue".to_string()],
        ..RunReportRequest::default()
    };
    let response = ReportResponse::new(
        vec![ReportRow::new(
            vec!["JP".to_string()],
            vec!["1234".to_string(), "99.50".to_string()],
        )],
        false,
    );
    let rows = response_to_rows(&req, &catalog, &response).unwrap();
    assert_eq!(rows.len(), 1);
    // country → Text, sessions(int) → Int, totalRevenue(currency) → decimal Text.
    assert_eq!(rows[0].values[0], Value::Text("JP".to_string()));
    assert_eq!(rows[0].values[1], Value::Int(1234));
    assert_eq!(rows[0].values[2], Value::Text("99.50".to_string()));
}

#[test]
fn arity_mismatch_in_response_is_a_structured_decode_error() {
    let catalog = fixture_catalog();
    let req = RunReportRequest {
        dimensions: vec!["country".to_string()],
        metrics: vec!["sessions".to_string()],
        ..RunReportRequest::default()
    };
    // Two dimension values for a one-dimension request — malformed shape.
    let response = ReportResponse::new(
        vec![ReportRow::new(
            vec!["JP".to_string(), "US".to_string()],
            vec!["1".to_string()],
        )],
        false,
    );
    let err = response_to_rows(&req, &catalog, &response).unwrap_err();
    assert_eq!(err.code(), "decode");
}

#[test]
fn sampling_metadata_is_surfaced_through_the_read_path() {
    let catalog = fixture_catalog();
    let request = RunReportRequest {
        property_id: "123".to_string(),
        dimensions: vec!["country".to_string()],
        metrics: vec!["sessions".to_string()],
        date_ranges: vec![DateRange {
            start_date: "2024-01-01".to_string(),
            end_date: "2024-01-31".to_string(),
        }],
        ..RunReportRequest::default()
    };
    // A sampled response.
    let mock = MockGaClient::new()
        .with_catalog(catalog.clone())
        .with_report(ReportResponse::new(
            vec![ReportRow::new(
                vec!["JP".to_string()],
                vec!["10".to_string()],
            )],
            true,
        ));
    let driver = GaDriver::new(Arc::new(mock));
    let (rows, sampled) = driver.run_report(&request, &catalog).unwrap();
    assert_eq!(rows.len(), 1);
    assert!(
        sampled,
        "sampling metadata must be surfaced, not silently dropped"
    );
}

#[test]
fn report_run_records_the_compiled_request_for_the_selected_property() {
    // Multi-account/property selection: the property id flows into both getMetadata and runReport,
    // and the mock records the exact compiled request the driver sent.
    let mock = Arc::new(
        MockGaClient::new()
            .with_catalog(fixture_catalog())
            .with_report(ReportResponse::new(
                vec![ReportRow::new(
                    vec!["JP".to_string()],
                    vec!["7".to_string()],
                )],
                false,
            )),
    );
    let driver = GaDriver::new(mock.clone());

    let catalog = driver.fetch_catalog("987654321").unwrap();
    let spec = QuerySpec::new(vec!["country".to_string(), "sessions".to_string()])
        .with_predicate(date_between("2024-03-01", "2024-03-31"));
    let result = compile("987654321", false, &catalog, &spec).unwrap();
    let _ = driver.run_report(&result.request, &catalog).unwrap();

    let calls = mock.recorded();
    assert_eq!(
        calls[0],
        RecordedCall::GetMetadata {
            property_id: "987654321".to_string()
        }
    );
    match &calls[1] {
        RecordedCall::RunReport { request } => {
            assert_eq!(request.property_id, "987654321");
            assert_eq!(request.dimensions, vec!["country".to_string()]);
            assert_eq!(request.metrics, vec!["sessions".to_string()]);
        }
        other => panic!("expected RunReport, got {other:?}"),
    }
}

#[test]
fn execute_query_fetches_catalog_compiles_runs_and_types_the_projected_rows() {
    // The read facet's one-call path: parse /ga/<property> → fetch catalog → compile → runReport →
    // project the response onto its typed (dimensions then metrics) schema.
    let mock = Arc::new(
        MockGaClient::new()
            .with_catalog(fixture_catalog())
            .with_report(ReportResponse::new(
                vec![ReportRow::new(
                    vec!["JP".to_string()],
                    vec!["7".to_string()],
                )],
                false,
            )),
    );
    let driver = GaDriver::new(mock);
    let spec = QuerySpec::new(vec!["country".to_string(), "sessions".to_string()])
        .with_predicate(date_between("2024-03-01", "2024-03-31"));
    let (rows, schema, residual) = driver
        .execute_query(&Path::new("/ga/987654321"), &spec)
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        schema.column_names(),
        vec!["country".to_string(), "sessions".to_string()],
        "the projected schema is the requested dimensions then metrics, in order"
    );
    assert!(matches!(&rows[0].values[0], qfs_types::Value::Text(s) if s == "JP"));
    assert!(
        residual.is_none(),
        "an exact date predicate pushes down fully"
    );
}

#[test]
fn execute_query_on_the_ga_root_is_a_structured_error_not_a_report() {
    let driver = GaDriver::new(Arc::new(
        MockGaClient::new().with_catalog(fixture_catalog()),
    ));
    let spec = QuerySpec::new(vec!["country".to_string()]);
    let err = driver.execute_query(&Path::new("/ga"), &spec).unwrap_err();
    assert_eq!(err.code(), "invalid_path");
}

// ---------------------------------------------------------------------------------------------
// Driver surface invariants + token safety
// ---------------------------------------------------------------------------------------------

#[test]
fn driver_declares_relational_pushdown_and_read_only_surface() {
    let driver = GaDriver::new(Arc::new(MockGaClient::new()));
    assert_eq!(driver.mount(), "/ga");
    assert_eq!(driver.id(), DriverId::new("ga"));
    // Read-only: no mutating procedures declared.
    assert!(driver.procedures().is_empty());
    // Pushdown declares the relational vocabulary GA runs natively (one runReport).
    match driver.pushdown() {
        PushdownProfile::Partial {
            where_,
            project,
            limit,
            order,
            aggregate,
            group_by,
            ..
        } => {
            assert!(*where_ && *project && *limit && *order && *aggregate && *group_by);
        }
        other => panic!("expected Partial pushdown, got {other:?}"),
    }
}

#[test]
fn errors_never_carry_a_token_or_credential() {
    // The structured errors are secret-free by construction; assert the Debug/Display surfaces
    // carry only the structured, non-secret fields.
    let read_only = GaError::ReadOnly {
        path: "/ga/123".to_string(),
        verb: "INSERT",
    };
    assert_eq!(read_only.code(), "read_only");
    assert!(!format!("{read_only:?}").to_lowercase().contains("bearer"));

    let api = GaError::Api {
        op: "runReport",
        status: 429,
    };
    // GA4 quota exhaustion (429) is retryable so the runtime backs off.
    assert!(api.is_retryable());

    let auth: GaError = qfs_google_auth::AuthError::Invalid {
        reason: "malformed account".to_string(),
    }
    .into();
    assert_eq!(auth.code(), "auth");
    assert!(!format!("{auth}").to_lowercase().contains("token"));
}

#[test]
fn bridge_constructs_and_routes_read_only() {
    // The runtime bridge constructs from the read-only applier (a runtime leaf); a stray write
    // routed through it is rejected.
    let driver = GaDriver::new(Arc::new(MockGaClient::new()));
    let _bridge = ga_apply_driver(&driver);
    // Sanity: the applier the bridge wraps rejects writes.
    let mut applier = driver.ga_applier().clone();
    let node = EffectNode::new(
        NodeId(0),
        EffectKind::Update,
        Target::new(DriverId::new("ga"), VfsPath::new("/ga/123")),
    );
    assert!(PlanApplier::apply(&mut applier, &node).is_err());
}
