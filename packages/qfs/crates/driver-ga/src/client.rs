//! [`GaClient`] — the thin, **mockable** GA4 Data API seam (RFD-0001 §9 no-heavy-SDK, boundary
//! B3), plus [`GoogleApiGaClient`] (the real client over the t19 [`GoogleApiClient`]) and
//! [`MockGaClient`] (an in-memory fake for tests — no live GA, no network).
//!
//! The trait trades **only** in owned, vendor-free DTOs ([`RunReportRequest`]/[`ReportResponse`]/
//! [`Catalog`]); GA4 JSON never crosses it. The real impl serializes the owned [`RunReportRequest`]
//! into the GA4 `runReport` JSON body (no `Authorization` header — the [`GoogleApiClient`] injects
//! the bearer and refreshes on a 401), sends it, and translates the response JSON into the owned
//! DTOs. The token discipline is wholly inherited from t19: the bearer lives behind a
//! [`qfs_secrets::Secret`], is written only into a header the redacting `HttpRequest` `Debug`
//! hides, and is **never** logged or surfaced in a [`GaError`].

use std::sync::{Arc, Mutex};

use qfs_google_auth::GoogleApiClient;
use qfs_http_core::{HttpMethod, HttpRequest, HttpResponse};

use crate::catalog::{Catalog, GaDimension, GaMetric, MetricKind};
use crate::compile::{FilterExpression, FilterTest, NumericOp, RunReportRequest, StringMatch};
use crate::error::GaError;
use crate::report::{ReportResponse, ReportRow};

/// The GA4 Data API base URL (v1beta). Every report op is a path under this.
const DATA_API_BASE: &str = "https://analyticsdata.googleapis.com/v1beta";

/// The thin GA4 API seam. A driver issues every GA call through this; the real impl rides the t19
/// [`GoogleApiClient`] (bearer + refresh-on-401), the test impl answers from in-memory fixtures.
/// `Send + Sync` so an `Arc<dyn GaClient>` can be shared across the runtime's blocking threads.
pub trait GaClient: Send + Sync {
    /// Run a core report (`properties.runReport`) or, when `request.realtime`, a realtime report
    /// (`properties.runRealtimeReport`), returning the decoded owned [`ReportResponse`].
    ///
    /// # Errors
    /// [`GaError`] on a non-2xx status, a decode failure, or an auth/transport failure.
    fn run_report(&self, request: &RunReportRequest) -> Result<ReportResponse, GaError>;

    /// Fetch the property's dimension + metric catalog (`properties.getMetadata`) → the owned
    /// [`Catalog`] DTO that powers `DESCRIBE`.
    ///
    /// # Errors
    /// [`GaError`] on a non-2xx status, a decode failure, or an auth/transport failure.
    fn get_metadata(&self, property_id: &str) -> Result<Catalog, GaError>;
}

/// The real GA4 client: builds owned [`HttpRequest`]s and sends them through the t19
/// [`GoogleApiClient`], which injects the per-account bearer and refreshes on a 401. The account
/// selection is wholly upstream (the `GoogleApiClient` is constructed per account from a
/// [`qfs_google_auth::TokenSource`]); this client is account-agnostic.
pub struct GoogleApiGaClient {
    api: Arc<GoogleApiClient>,
}

impl GoogleApiGaClient {
    /// Build a GA client over an authenticated [`GoogleApiClient`] (one per account).
    #[must_use]
    pub fn new(api: Arc<GoogleApiClient>) -> Self {
        Self { api }
    }

    /// Send a request through the auth client, mapping its `AuthError` to a secret-free
    /// [`GaError`] and classifying a non-2xx status under `op`.
    fn send(&self, op: &'static str, req: &HttpRequest) -> Result<HttpResponse, GaError> {
        let resp = self.api.send(req).map_err(GaError::from)?;
        if resp.is_success() {
            Ok(resp)
        } else {
            Err(GaError::Api {
                op,
                status: resp.status,
            })
        }
    }

    fn parse_json(op: &'static str, resp: &HttpResponse) -> Result<serde_json::Value, GaError> {
        serde_json::from_slice(&resp.body).map_err(|_| GaError::Decode {
            op,
            reason: "response body was not valid JSON".to_string(),
        })
    }

    /// A JSON-body POST request to a GA4 Data API path.
    fn json_post(
        op: &'static str,
        url: String,
        body: &serde_json::Value,
    ) -> Result<HttpRequest, GaError> {
        let bytes = serde_json::to_vec(body).map_err(|_| GaError::Decode {
            op,
            reason: "could not encode the request body".to_string(),
        })?;
        Ok(HttpRequest::new(HttpMethod::Post, url)
            .header("Content-Type", "application/json")
            .with_body(bytes))
    }
}

impl GaClient for GoogleApiGaClient {
    fn run_report(&self, request: &RunReportRequest) -> Result<ReportResponse, GaError> {
        let (op, method) = if request.realtime {
            ("runRealtimeReport", "runRealtimeReport")
        } else {
            ("runReport", "runReport")
        };
        let body = request_to_json(request);
        let url = format!(
            "{DATA_API_BASE}/properties/{}:{method}",
            request.property_id
        );
        let req = Self::json_post(op, url, &body)?;
        let resp = self.send(op, &req)?;
        let json = Self::parse_json(op, &resp)?;
        decode_report(&json).ok_or(GaError::Decode {
            op,
            reason: "report JSON missing the expected rows/metadata shape".to_string(),
        })
    }

    fn get_metadata(&self, property_id: &str) -> Result<Catalog, GaError> {
        let op = "getMetadata";
        let url = format!("{DATA_API_BASE}/properties/{property_id}/metadata");
        let resp = self.send(op, &HttpRequest::new(HttpMethod::Get, url))?;
        let json = Self::parse_json(op, &resp)?;
        Ok(decode_catalog(&json))
    }
}

/// Serialize an owned [`RunReportRequest`] into the GA4 `runReport`/`runRealtimeReport` JSON body.
/// A core report carries `dateRanges`; a realtime report omits them. Pure, deterministic — a
/// golden test can assert the exact wire body.
#[must_use]
pub fn request_to_json(request: &RunReportRequest) -> serde_json::Value {
    let mut body = serde_json::Map::new();
    body.insert(
        "dimensions".to_string(),
        serde_json::Value::Array(
            request
                .dimensions
                .iter()
                .map(|d| serde_json::json!({ "name": d }))
                .collect(),
        ),
    );
    body.insert(
        "metrics".to_string(),
        serde_json::Value::Array(
            request
                .metrics
                .iter()
                .map(|m| serde_json::json!({ "name": m }))
                .collect(),
        ),
    );
    if !request.realtime {
        body.insert(
            "dateRanges".to_string(),
            serde_json::Value::Array(
                request
                    .date_ranges
                    .iter()
                    .map(|dr| {
                        serde_json::json!({ "startDate": dr.start_date, "endDate": dr.end_date })
                    })
                    .collect(),
            ),
        );
    }
    if let Some(f) = &request.dimension_filter {
        body.insert("dimensionFilter".to_string(), filter_to_json(f));
    }
    if let Some(f) = &request.metric_filter {
        body.insert("metricFilter".to_string(), filter_to_json(f));
    }
    if !request.order_bys.is_empty() {
        body.insert(
            "orderBys".to_string(),
            serde_json::Value::Array(
                request
                    .order_bys
                    .iter()
                    .map(|o| {
                        if o.is_metric {
                            serde_json::json!({ "metric": { "metricName": o.field_name }, "desc": o.desc })
                        } else {
                            serde_json::json!({ "dimension": { "dimensionName": o.field_name }, "desc": o.desc })
                        }
                    })
                    .collect(),
            ),
        );
    }
    if let Some(limit) = request.limit {
        body.insert("limit".to_string(), serde_json::json!(limit.to_string()));
    }
    serde_json::Value::Object(body)
}

/// Serialize an owned [`FilterExpression`] into the GA4 `FilterExpression` JSON tree.
fn filter_to_json(expr: &FilterExpression) -> serde_json::Value {
    match expr {
        FilterExpression::AndGroup(children) => {
            serde_json::json!({
                "andGroup": { "expressions": children.iter().map(filter_to_json).collect::<Vec<_>>() }
            })
        }
        FilterExpression::Filter { field_name, test } => match test {
            FilterTest::String { value, match_type } => {
                let mt = match match_type {
                    StringMatch::Exact => "EXACT",
                    StringMatch::Contains => "CONTAINS",
                    StringMatch::FullRegexp => "FULL_REGEXP",
                };
                serde_json::json!({
                    "filter": { "fieldName": field_name, "stringFilter": { "value": value, "matchType": mt } }
                })
            }
            FilterTest::InList { values } => serde_json::json!({
                "filter": { "fieldName": field_name, "inListFilter": { "values": values } }
            }),
            FilterTest::Numeric { op, value } => {
                let operation = match op {
                    NumericOp::Equal => "EQUAL",
                    NumericOp::LessThan => "LESS_THAN",
                    NumericOp::LessThanOrEqual => "LESS_THAN_OR_EQUAL",
                    NumericOp::GreaterThan => "GREATER_THAN",
                    NumericOp::GreaterThanOrEqual => "GREATER_THAN_OR_EQUAL",
                };
                serde_json::json!({
                    "filter": {
                        "fieldName": field_name,
                        "numericFilter": { "operation": operation, "value": { "doubleValue": value } }
                    }
                })
            }
        },
    }
}

/// Translate a GA4 `runReport` response JSON into the owned [`ReportResponse`]. Returns `None`
/// only on a wholly unrecognizable shape; an empty `rows` is a valid (empty) report.
fn decode_report(json: &serde_json::Value) -> Option<ReportResponse> {
    if !json.is_object() {
        return None;
    }
    let rows = json
        .get("rows")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(decode_report_row).collect())
        .unwrap_or_default();
    // `metadata.samplingMetadatas` present and non-empty ⇒ the result is sampled (an estimate).
    let sampled = json
        .get("metadata")
        .and_then(|m| m.get("samplingMetadatas"))
        .and_then(|s| s.as_array())
        .is_some_and(|a| !a.is_empty());
    Some(ReportResponse::new(rows, sampled))
}

/// Translate one GA4 report row JSON (`dimensionValues[].value` + `metricValues[].value`) into the
/// owned [`ReportRow`].
fn decode_report_row(json: &serde_json::Value) -> Option<ReportRow> {
    let dims = json
        .get("dimensionValues")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().map(value_string).collect())
        .unwrap_or_default();
    let metrics = json
        .get("metricValues")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().map(value_string).collect())
        .unwrap_or_default();
    Some(ReportRow::new(dims, metrics))
}

/// Read the `value` string from a GA4 dimension/metric value object (defaults to empty).
fn value_string(v: &serde_json::Value) -> String {
    v.get("value")
        .and_then(|x| x.as_str())
        .unwrap_or_default()
        .to_string()
}

/// Translate a GA4 `getMetadata` response JSON into the owned [`Catalog`].
fn decode_catalog(json: &serde_json::Value) -> Catalog {
    let dimensions = json
        .get("dimensions")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(decode_dimension).collect())
        .unwrap_or_default();
    let metrics = json
        .get("metrics")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(decode_metric).collect())
        .unwrap_or_default();
    Catalog::new(dimensions, metrics)
}

/// Translate one GA4 metadata dimension JSON into the owned [`GaDimension`].
fn decode_dimension(json: &serde_json::Value) -> Option<GaDimension> {
    let api_name = json.get("apiName").and_then(|v| v.as_str())?.to_string();
    let display_name = json
        .get("uiName")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let category = json
        .get("category")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    Some(GaDimension {
        api_name,
        display_name,
        category,
    })
}

/// Translate one GA4 metadata metric JSON into the owned [`GaMetric`].
fn decode_metric(json: &serde_json::Value) -> Option<GaMetric> {
    let api_name = json.get("apiName").and_then(|v| v.as_str())?.to_string();
    let display_name = json
        .get("uiName")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let category = json
        .get("category")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let kind = json
        .get("type")
        .and_then(|v| v.as_str())
        .map_or(MetricKind::Float, MetricKind::from_ga_type);
    Some(GaMetric {
        api_name,
        display_name,
        category,
        kind,
    })
}

/// One recorded GA API call (the op + its salient owned arguments) — what a test asserts the
/// driver issued. Secret-free by construction (no token ever enters this seam).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum RecordedCall {
    /// A `runReport` / `runRealtimeReport` call carrying the full compiled request (so a test
    /// asserts the exact dimensions/metrics/dateRanges/filters/order/limit pushed down).
    RunReport {
        /// The compiled request the driver sent (boxed: a `RunReportRequest` is large relative to
        /// the other recorded variant).
        request: Box<RunReportRequest>,
    },
    /// A `getMetadata` call for one property id.
    GetMetadata {
        /// The property id whose catalog was fetched.
        property_id: String,
    },
}

/// An in-memory mock GA client (tests / CI / wasm): answers from pre-seeded fixtures and
/// **records** every call so a test asserts the exact API surface the driver exercised — with
/// **no socket and no credentials**.
#[derive(Default)]
pub struct MockGaClient {
    catalog: Catalog,
    responses: Mutex<Vec<ReportResponse>>,
    recorded: Mutex<Vec<RecordedCall>>,
}

impl MockGaClient {
    /// An empty mock.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Seed the catalog returned by `get_metadata`.
    #[must_use]
    pub fn with_catalog(mut self, catalog: Catalog) -> Self {
        self.catalog = catalog;
        self
    }

    /// Queue one report response returned (FIFO) by `run_report`.
    #[must_use]
    pub fn with_report(self, response: ReportResponse) -> Self {
        if let Ok(mut q) = self.responses.lock() {
            q.push(response);
        }
        self
    }

    /// The calls this mock received, in order — what a test asserts against.
    #[must_use]
    pub fn recorded(&self) -> Vec<RecordedCall> {
        self.recorded.lock().map(|r| r.clone()).unwrap_or_default()
    }

    fn record(&self, call: RecordedCall) {
        if let Ok(mut r) = self.recorded.lock() {
            r.push(call);
        }
    }
}

impl GaClient for MockGaClient {
    fn run_report(&self, request: &RunReportRequest) -> Result<ReportResponse, GaError> {
        self.record(RecordedCall::RunReport {
            request: Box::new(request.clone()),
        });
        let response = self
            .responses
            .lock()
            .ok()
            .and_then(|mut q| {
                if q.is_empty() {
                    None
                } else {
                    Some(q.remove(0))
                }
            })
            .unwrap_or_default();
        Ok(response)
    }

    fn get_metadata(&self, property_id: &str) -> Result<Catalog, GaError> {
        self.record(RecordedCall::GetMetadata {
            property_id: property_id.to_string(),
        });
        Ok(self.catalog.clone())
    }
}
