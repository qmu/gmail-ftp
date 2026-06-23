//! Owned `runReport` **response** DTOs and the pure decode into typed cfs rows (RFD-0001 §5/§9).
//!
//! GA4's `runReport` returns rows of `dimensionValues[]` (always strings) + `metricValues[]`
//! (strings parsed per the metric's [`MetricKind`](crate::catalog::MetricKind)), plus
//! `metadata.samplingMetadatas` flagging an estimated (sampled) result. This module owns the
//! vendor-free [`ReportResponse`] those decode into, and [`response_to_rows`], the pure projection
//! of a response onto the requested `dimensions + metrics` column order as typed
//! [`Row`](cfs_types::Row)s. No network, no token.
//!
//! ## Sampling surfacing (operation/observability)
//! High-cardinality/large queries are **sampled** — the result is an estimate. The decode carries
//! `sampled` on the response so a consumer is never silently handed estimates; the driver surfaces
//! it (a query note / a `sampled` marker) rather than dropping it.

use cfs_types::{Row, Value};

use crate::catalog::{Catalog, MetricKind};
use crate::compile::RunReportRequest;
use crate::error::GaError;

/// One decoded report row: the dimension values (strings, in `dimensions[]` order) followed by
/// the metric values (strings, in `metrics[]` order). Owned, vendor-free — the typed mapping to
/// cfs [`Value`]s happens in [`response_to_rows`] using the catalog's metric kinds.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ReportRow {
    /// The dimension string values, in `dimensions[]` order.
    pub dimension_values: Vec<String>,
    /// The metric string values, in `metrics[]` order.
    pub metric_values: Vec<String>,
}

impl ReportRow {
    /// Construct a report row from its dimension + metric string values.
    #[must_use]
    pub fn new(dimension_values: Vec<String>, metric_values: Vec<String>) -> Self {
        Self {
            dimension_values,
            metric_values,
        }
    }
}

/// A decoded `runReport` / `runRealtimeReport` response (RFD §9 no-vendor-leak). Owned,
/// vendor-free.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct ReportResponse {
    /// The report rows, in GA's returned order.
    pub rows: Vec<ReportRow>,
    /// Whether GA flagged the result as **sampled** (an estimate) via
    /// `metadata.samplingMetadatas`. Surfaced so downstream decisions aren't made on estimates
    /// silently.
    pub sampled: bool,
}

impl ReportResponse {
    /// Construct a response from its rows and the sampling flag.
    #[must_use]
    pub fn new(rows: Vec<ReportRow>, sampled: bool) -> Self {
        Self { rows, sampled }
    }
}

/// Project a decoded [`ReportResponse`] onto the requested `dimensions + metrics` column order as
/// typed [`Row`]s (RFD §5): each dimension value becomes a [`Value::Text`]; each metric value is
/// parsed per its catalog [`MetricKind`] (integer → [`Value::Int`], currency → a decimal
/// [`Value::Text`], otherwise [`Value::Float`]). A value that fails to parse decodes to
/// [`Value::Null`] (a sparse cell), never an error — the schema marks every report column
/// nullable. Pure: no network, no token.
///
/// # Errors
/// [`GaError::Decode`] if a row's arity does not match the request's dimension/metric counts
/// (a malformed response shape — secret-free, never the body).
pub fn response_to_rows(
    request: &RunReportRequest,
    catalog: &Catalog,
    response: &ReportResponse,
) -> Result<Vec<Row>, GaError> {
    let n_dims = request.dimensions.len();
    let n_metrics = request.metrics.len();
    let mut out = Vec::with_capacity(response.rows.len());
    for row in &response.rows {
        if row.dimension_values.len() != n_dims || row.metric_values.len() != n_metrics {
            return Err(GaError::Decode {
                op: "runReport",
                reason: "report row arity does not match the requested dimensions/metrics"
                    .to_string(),
            });
        }
        let mut values: Vec<Value> = Vec::with_capacity(n_dims + n_metrics);
        for v in &row.dimension_values {
            values.push(Value::Text(v.clone()));
        }
        for (metric_name, raw) in request.metrics.iter().zip(&row.metric_values) {
            let kind = catalog
                .metric(metric_name)
                .map_or(MetricKind::Float, |m| m.kind);
            values.push(decode_metric(kind, raw));
        }
        out.push(Row::new(values));
    }
    Ok(out)
}

/// Decode one raw metric string into the typed [`Value`] its [`MetricKind`] fixes. A parse failure
/// yields [`Value::Null`] (a sparse cell), never a panic.
fn decode_metric(kind: MetricKind, raw: &str) -> Value {
    match kind {
        MetricKind::Integer => raw.parse::<i64>().map_or(Value::Null, Value::Int),
        // Currency is carried losslessly as a decimal lexical form (ColumnType::Decimal → Text).
        MetricKind::Currency => {
            if raw.is_empty() {
                Value::Null
            } else {
                Value::Text(raw.to_string())
            }
        }
        MetricKind::Float | MetricKind::Percent | MetricKind::Seconds => {
            raw.parse::<f64>().map_or(Value::Null, Value::Float)
        }
    }
}
