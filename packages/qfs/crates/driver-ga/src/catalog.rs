//! Owned GA4 **catalog** DTOs (`GaDimension`, `GaMetric`, [`MetricKind`]) and the catalog → typed
//! [`Schema`] reconciliation that powers `DESCRIBE /ga/<property>` (RFD-0001 §5/§9).
//!
//! GA4's `properties.getMetadata` returns the property's dimension + metric catalog (including
//! custom dimensions/metrics). Those JSON shapes are translated into these owned DTOs at the
//! [`crate::client`] boundary; the `Driver` surface carries **zero** google types. A [`Catalog`]
//! both answers `DESCRIBE` (the AI's introspection surface) and validates that a projected/filtered
//! name is a real dimension or metric — turning a would-be raw GA 400 into a structured
//! [`GaError::UnknownField`](crate::error::GaError::UnknownField) the AI can self-correct.

use qfs_types::{Column, ColumnType, Schema};

/// The kind of a GA4 metric value, which fixes the typed qfs column it decodes into.
/// Mirrors GA4's `MetricType` (the queryable subset). Owned, vendor-free.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum MetricKind {
    /// An integer count (e.g. `sessions`, `activeUsers`). Decodes to [`ColumnType::Int`].
    Integer,
    /// A floating-point measure (e.g. `bounceRate`, `engagementRate`). Decodes to
    /// [`ColumnType::Float`].
    Float,
    /// A currency amount (e.g. `totalRevenue`). Carried as a decimal lexical form
    /// ([`ColumnType::Decimal`]) so no precision is lost.
    Currency,
    /// A percentage (0..100). Decodes to [`ColumnType::Float`].
    Percent,
    /// A duration in seconds. Decodes to [`ColumnType::Float`].
    Seconds,
}

impl MetricKind {
    /// The canonical qfs [`ColumnType`] this metric kind decodes into.
    #[must_use]
    pub const fn column_type(self) -> ColumnType {
        match self {
            MetricKind::Integer => ColumnType::Int,
            MetricKind::Float | MetricKind::Percent | MetricKind::Seconds => ColumnType::Float,
            MetricKind::Currency => ColumnType::Decimal,
        }
    }

    /// Parse a GA4 `MetricType` token (e.g. `TYPE_INTEGER`, `TYPE_FLOAT`, `TYPE_CURRENCY`) into
    /// the owned kind. Unrecognized/standard-measure types default to [`MetricKind::Float`]
    /// (GA returns metric values as strings; a float parse is the safe general decode).
    #[must_use]
    pub fn from_ga_type(ty: &str) -> Self {
        match ty {
            "TYPE_INTEGER" => MetricKind::Integer,
            "TYPE_CURRENCY" => MetricKind::Currency,
            "TYPE_SECONDS" | "TYPE_MILLISECONDS" | "TYPE_MINUTES" | "TYPE_HOURS" => {
                MetricKind::Seconds
            }
            // TYPE_FLOAT, TYPE_PERCENT, TYPE_STANDARD, and anything else → a float measure.
            _ => MetricKind::Float,
        }
    }
}

/// One GA4 dimension descriptor (the catalog group-by axes). Owned, vendor-free. A dimension
/// value is always a string in GA's response, so it decodes to [`ColumnType::Text`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct GaDimension {
    /// The API name (e.g. `country`, `date`, `pagePath`) — the column name and filter key.
    pub api_name: String,
    /// The human display name (e.g. `Country`), surfaced for introspection.
    pub display_name: String,
    /// The dimension category (e.g. `Geography`, `Time`), surfaced for introspection.
    pub category: String,
}

impl GaDimension {
    /// A test/consumer constructor for a dimension (the DTO is `#[non_exhaustive]`).
    #[cfg(any(test, feature = "test-util"))]
    #[must_use]
    pub fn for_test(api_name: &str, display_name: &str, category: &str) -> Self {
        Self {
            api_name: api_name.to_string(),
            display_name: display_name.to_string(),
            category: category.to_string(),
        }
    }
}

/// One GA4 metric descriptor (the catalog measures). Owned, vendor-free. The [`MetricKind`] fixes
/// the typed column it decodes into.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct GaMetric {
    /// The API name (e.g. `sessions`, `totalRevenue`) — the column name and filter key.
    pub api_name: String,
    /// The human display name (e.g. `Sessions`), surfaced for introspection.
    pub display_name: String,
    /// The metric category, surfaced for introspection.
    pub category: String,
    /// The metric value kind, which fixes the typed qfs column.
    pub kind: MetricKind,
}

impl GaMetric {
    /// A test/consumer constructor for a metric (the DTO is `#[non_exhaustive]`).
    #[cfg(any(test, feature = "test-util"))]
    #[must_use]
    pub fn for_test(api_name: &str, display_name: &str, category: &str, kind: MetricKind) -> Self {
        Self {
            api_name: api_name.to_string(),
            display_name: display_name.to_string(),
            category: category.to_string(),
            kind,
        }
    }
}

/// A property's GA4 dimension + metric catalog (from `properties.getMetadata`), owned and
/// vendor-free. Answers `DESCRIBE` and validates query field references against the real catalog.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct Catalog {
    /// The property's dimensions (the group-by/select axes).
    pub dimensions: Vec<GaDimension>,
    /// The property's metrics (the measures).
    pub metrics: Vec<GaMetric>,
}

impl Catalog {
    /// Construct a catalog from its dimensions + metrics.
    #[must_use]
    pub fn new(dimensions: Vec<GaDimension>, metrics: Vec<GaMetric>) -> Self {
        Self {
            dimensions,
            metrics,
        }
    }

    /// Look up a dimension by API name.
    #[must_use]
    pub fn dimension(&self, api_name: &str) -> Option<&GaDimension> {
        self.dimensions.iter().find(|d| d.api_name == api_name)
    }

    /// Look up a metric by API name.
    #[must_use]
    pub fn metric(&self, api_name: &str) -> Option<&GaMetric> {
        self.metrics.iter().find(|m| m.api_name == api_name)
    }

    /// Whether `api_name` is a known dimension in this catalog.
    #[must_use]
    pub fn is_dimension(&self, api_name: &str) -> bool {
        self.dimension(api_name).is_some()
    }

    /// Whether `api_name` is a known metric in this catalog.
    #[must_use]
    pub fn is_metric(&self, api_name: &str) -> bool {
        self.metric(api_name).is_some()
    }

    /// The canonical `DESCRIBE` [`Schema`] for this property: every dimension column (typed
    /// [`ColumnType::Text`]) followed by every metric column (typed from its [`MetricKind`]),
    /// in catalog order. Stable order powers golden snapshots. This is the full queryable
    /// surface; a concrete `SELECT` projects a subset of these columns (see
    /// [`crate::compile`]). All columns are reported nullable because a report row may omit a
    /// value for a sparse dimension/metric combination.
    #[must_use]
    pub fn describe_schema(&self) -> Schema {
        let mut columns: Vec<Column> =
            Vec::with_capacity(self.dimensions.len() + self.metrics.len());
        for d in &self.dimensions {
            columns.push(Column::new(d.api_name.clone(), ColumnType::Text, true));
        }
        for m in &self.metrics {
            columns.push(Column::new(m.api_name.clone(), m.kind.column_type(), true));
        }
        Schema::new(columns)
    }
}
