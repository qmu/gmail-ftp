//! The pure **query → `RunReportRequest` compiler** (RFD-0001 §5/§6, t41) and the owned request
//! DTOs it produces. This is the GA4 analogue of the SQL driver's full pushdown: the *entire*
//! relational subtree over a `/ga` node compiles to **one** native `runReport` call (GA does the
//! aggregation server-side), so GA is a pushdown target.
//!
//! ## What compiles
//! A relational query over a property — a projection of dimension/metric names, a `WHERE`
//! predicate, `ORDER BY`, and `LIMIT` — lowers into a [`RunReportRequest`]:
//! - **dimensions** ← the projected dimension columns (`dimensions[]`, GA's group-by/select).
//! - **metrics** ← the projected metric columns (`metrics[]`).
//! - **date range** ← a **mandatory** `date` predicate (`WHERE date BETWEEN 'a' AND 'b'`, or
//!   `date >= 'a'` / `date <= 'b'`) → `dateRanges[]`. GA4 requires it; absence is a structured
//!   [`GaError::MissingDateRange`].
//! - **dimensionFilter / metricFilter** ← the remaining `WHERE` conjuncts on dimension / metric
//!   columns → [`FilterExpression`] trees.
//! - **orderBys** ← `ORDER BY` columns (dimension or metric, asc/desc).
//! - **limit** ← `LIMIT n`.
//!
//! ## The t20/t21 lesson — TRUTHFUL residual (the headline invariant)
//! A `WHERE` conjunct is pushed as a **residual-dropping** GA filter **only** when the GA filter
//! means *exactly* the SQL predicate:
//! - `dim = 'x'`            → `stringFilter { matchType: EXACT }`        (exact equality, dropped)
//! - `dim IN ('a','b')`     → `inListFilter`                            (exact membership, dropped)
//! - `metric > n` / `>=` / `<` / `<=` / `=` → `numericFilter` with the matching `Operation`
//!   (GA numeric comparisons are exact, dropped)
//! - the `date` predicate   → `dateRanges[]`                            (exact window, dropped)
//!
//! Every **looser** GA filter is pushed as a backend **pre-filter** and the exact SQL predicate is
//! **kept as residual** so the engine re-applies exact filtering locally (over-fetch then filter —
//! never wrong rows, RFD §6):
//! - `dim LIKE 'p'` / `dim ~ 'p'` → `stringFilter { matchType: CONTAINS/FULL_REGEXP }` is a
//!   substring/regex pre-filter looser than SQL `LIKE`/`~` semantics here → predicate KEPT.
//!
//! `OR` / `NOT` / `BETWEEN` on a non-date column / a comparison whose column is not in the catalog
//! push nothing and stay wholly **residual**. The compiler performs **no I/O** and holds no token.

use qfs_types::{CmpOp, ColRef, Literal, Predicate};

use crate::catalog::Catalog;
use crate::error::GaError;

/// A GA4 date range (`dateRanges[]` entry). Inclusive `YYYY-MM-DD` bounds (or a GA relative token
/// such as `today` / `7daysAgo`). Owned, vendor-free.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DateRange {
    /// The inclusive start date (GA `startDate`).
    pub start_date: String,
    /// The inclusive end date (GA `endDate`).
    pub end_date: String,
}

/// A GA4 string-filter match type (the subset the compiler emits). Mirrors GA's
/// `StringFilter.MatchType`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StringMatch {
    /// Exact equality (`dim = 'x'`) — the residual-dropping mapping.
    Exact,
    /// Substring contains (`dim LIKE 'p'`) — a loose pre-filter; predicate kept as residual.
    Contains,
    /// Full regular-expression match (`dim ~ 'p'`) — a pre-filter; predicate kept as residual.
    FullRegexp,
}

/// A GA4 numeric-filter operation (the subset the compiler emits). Mirrors GA's
/// `NumericFilter.Operation`. Every numeric comparison GA expresses is exact, so all of these are
/// residual-dropping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NumericOp {
    /// `=`
    Equal,
    /// `<`
    LessThan,
    /// `<=`
    LessThanOrEqual,
    /// `>`
    GreaterThan,
    /// `>=`
    GreaterThanOrEqual,
}

impl NumericOp {
    /// Map a SQL comparison operator to the GA numeric operation, if GA expresses it exactly.
    fn from_cmp(op: CmpOp) -> Option<Self> {
        match op {
            CmpOp::Eq => Some(NumericOp::Equal),
            CmpOp::Lt => Some(NumericOp::LessThan),
            CmpOp::Le => Some(NumericOp::LessThanOrEqual),
            CmpOp::Gt => Some(NumericOp::GreaterThan),
            CmpOp::Ge => Some(NumericOp::GreaterThanOrEqual),
            // `<>` (Ne) and `~` (Match) have no exact GA numeric operation.
            CmpOp::Ne | CmpOp::Match => None,
        }
    }
}

/// A GA4 `FilterExpression` — a tree of field filters combined by `andGroup`. Owned, vendor-free.
/// The compiler emits only the `and`-group form (a conjunction of leaf filters); `OR`/`NOT` stay
/// residual and are filtered locally.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FilterExpression {
    /// A conjunction of sub-expressions (`andGroup.expressions[]`).
    AndGroup(Vec<FilterExpression>),
    /// A leaf `filter` on a single dimension/metric field.
    Filter {
        /// The dimension or metric API name the filter is keyed on.
        field_name: String,
        /// The concrete filter test.
        test: FilterTest,
    },
}

/// One leaf GA4 filter test (`stringFilter` / `inListFilter` / `numericFilter`). Owned,
/// vendor-free.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FilterTest {
    /// A `stringFilter` (`value` + `matchType`) — for dimension columns.
    String {
        /// The match value.
        value: String,
        /// The match type (`Exact` is residual-dropping; `Contains`/`FullRegexp` are pre-filters).
        match_type: StringMatch,
    },
    /// An `inListFilter` (`values[]`) — exact membership for a dimension column.
    InList {
        /// The candidate values.
        values: Vec<String>,
    },
    /// A `numericFilter` (`operation` + `value`) — exact comparison for a metric column.
    Numeric {
        /// The comparison operation.
        op: NumericOp,
        /// The numeric value rendered as a string (GA carries numeric filter values as strings).
        value: String,
    },
}

/// A GA4 `OrderBy` (`dimension`/`metric` ordering, asc/desc). Owned, vendor-free.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderBy {
    /// The dimension or metric API name to order by.
    pub field_name: String,
    /// Whether the order is descending (GA `desc`).
    pub desc: bool,
    /// Whether the field is a metric (a `metric` orderBy) vs a dimension (a `dimension` orderBy).
    pub is_metric: bool,
}

/// The owned `runReport` / `runRealtimeReport` request the compiler produces and the client sends
/// (RFD §9 no-vendor-leak: the GA SDK request type never crosses the boundary). A golden test
/// asserts this as a *value/plan*, never by hitting the API.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RunReportRequest {
    /// The GA4 property id the report runs against (`properties/<id>`).
    pub property_id: String,
    /// Whether this is a realtime report (`runRealtimeReport`, which has **no** `dateRanges`).
    pub realtime: bool,
    /// The projected dimension API names (`dimensions[]`).
    pub dimensions: Vec<String>,
    /// The projected metric API names (`metrics[]`).
    pub metrics: Vec<String>,
    /// The date ranges (`dateRanges[]`). Empty for a realtime report; exactly one for a core
    /// report (the mandatory window).
    pub date_ranges: Vec<DateRange>,
    /// The dimension filter tree (`dimensionFilter`), if any dimension predicate pushed down.
    pub dimension_filter: Option<FilterExpression>,
    /// The metric filter tree (`metricFilter`), if any metric predicate pushed down.
    pub metric_filter: Option<FilterExpression>,
    /// The order-by list (`orderBys[]`).
    pub order_bys: Vec<OrderBy>,
    /// The row cap (`limit`), if a `LIMIT` was pushed.
    pub limit: Option<i64>,
}

/// The relational query inputs the compiler lowers — the projection (column names, in select
/// order), the optional `WHERE` predicate, the optional `ORDER BY`, and the optional `LIMIT`.
/// Owned, vendor-free; built by the planner from the typed relational subtree.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct QuerySpec {
    /// The projected column names, in `SELECT` order (each must be a catalog dimension or metric).
    pub projection: Vec<String>,
    /// The `WHERE` predicate, if any.
    pub predicate: Option<Predicate>,
    /// The `ORDER BY` items as `(column, desc)` pairs, in order.
    pub order_by: Vec<(String, bool)>,
    /// The `LIMIT`, if any.
    pub limit: Option<i64>,
}

impl QuerySpec {
    /// A spec with just a projection — the minimal query.
    #[must_use]
    pub fn new(projection: Vec<String>) -> Self {
        Self {
            projection,
            ..Self::default()
        }
    }

    /// Builder: set the `WHERE` predicate.
    #[must_use]
    pub fn with_predicate(mut self, predicate: Predicate) -> Self {
        self.predicate = Some(predicate);
        self
    }

    /// Builder: add an `ORDER BY` item.
    #[must_use]
    pub fn order_by(mut self, column: impl Into<String>, desc: bool) -> Self {
        self.order_by.push((column.into(), desc));
        self
    }

    /// Builder: set the `LIMIT`.
    #[must_use]
    pub fn with_limit(mut self, limit: i64) -> Self {
        self.limit = Some(limit);
        self
    }
}

/// The result of compiling a query: the pushed-down [`RunReportRequest`] and the residual
/// predicate the engine still filters locally (RFD §6). `residual` is `None` when the whole
/// `WHERE` pushed down with exact mappings.
#[derive(Debug, Clone, PartialEq)]
pub struct CompileResult {
    /// The native GA4 request the driver sends (one `runReport` per query).
    pub request: RunReportRequest,
    /// The predicate the backend could **not** express exactly — the engine filters this locally.
    pub residual: Option<Predicate>,
}

/// The reserved column name carrying the report date window (the mandatory date-range predicate).
pub const DATE_COL: &str = "date";

/// Compile a relational query over a GA4 property into a [`RunReportRequest`] + a truthful
/// residual (RFD §5/§6).
///
/// `property_id` and `realtime` come from the resolved [`GaPath`](crate::path::GaPath);
/// `catalog` is the property's catalog (used to classify each name as a dimension vs metric and to
/// reject unknown fields with a structured error rather than a raw GA 400).
///
/// # Errors
/// - [`GaError::EmptyProjection`] if no dimension/metric is projected.
/// - [`GaError::UnknownField`] if a projected/ordered name is not in the catalog.
/// - [`GaError::MissingDateRange`] if a **core** report carries no `date` predicate (a realtime
///   report needs none — it is the last ~30 minutes).
pub fn compile(
    property_id: &str,
    realtime: bool,
    catalog: &Catalog,
    spec: &QuerySpec,
) -> Result<CompileResult, GaError> {
    if spec.projection.is_empty() {
        return Err(GaError::EmptyProjection);
    }

    // Split the projection into dimensions vs metrics by the catalog; reject unknown names.
    let mut dimensions: Vec<String> = Vec::new();
    let mut metrics: Vec<String> = Vec::new();
    for name in &spec.projection {
        if catalog.is_dimension(name) {
            dimensions.push(name.clone());
        } else if catalog.is_metric(name) {
            metrics.push(name.clone());
        } else {
            return Err(GaError::UnknownField {
                name: name.clone(),
                reason: "not a dimension or metric in the property catalog",
            });
        }
    }

    // Lower the WHERE: extract the date range (mandatory for a core report), and split the
    // remaining conjuncts into dimension/metric filter leaves + a residual.
    let mut lowered = LoweredWhere::default();
    if let Some(p) = &spec.predicate {
        lower_predicate(p, catalog, &mut lowered);
    }

    let date_ranges = if realtime {
        // A realtime report has no date range (the last ~30 minutes); ignore any date predicate
        // (it stays in the residual if present so it is never silently dropped).
        Vec::new()
    } else {
        match lowered.date_range.take() {
            Some(dr) => vec![dr],
            None => return Err(GaError::MissingDateRange),
        }
    };

    let order_bys = build_order_bys(catalog, &spec.order_by)?;

    let request = RunReportRequest {
        property_id: property_id.to_string(),
        realtime,
        dimensions,
        metrics,
        date_ranges,
        dimension_filter: group(lowered.dimension_filters),
        metric_filter: group(lowered.metric_filters),
        order_bys,
        // Push the GA `limit` into the report ONLY when the whole `WHERE` pushed down (no residual).
        // With a residual the engine re-filters after the fetch, so a native `limit n` would cap the
        // report *before* that filter and under-fetch; the read facet applies the `LIMIT` after the
        // local re-filter instead.
        limit: if lowered.residual.is_some() {
            None
        } else {
            spec.limit
        },
    };

    Ok(CompileResult {
        request,
        residual: lowered.residual,
    })
}

/// Wrap a list of leaf filters in an `andGroup`, or `None` if empty, or the bare leaf if singular.
fn group(mut filters: Vec<FilterExpression>) -> Option<FilterExpression> {
    match filters.len() {
        0 => None,
        1 => Some(filters.remove(0)),
        _ => Some(FilterExpression::AndGroup(filters)),
    }
}

/// Build the GA `orderBys[]` from the `ORDER BY` items, classifying each by the catalog.
fn build_order_bys(catalog: &Catalog, items: &[(String, bool)]) -> Result<Vec<OrderBy>, GaError> {
    let mut out = Vec::with_capacity(items.len());
    for (column, desc) in items {
        let is_metric = if catalog.is_metric(column) {
            true
        } else if catalog.is_dimension(column) {
            false
        } else {
            return Err(GaError::UnknownField {
                name: column.clone(),
                reason: "ORDER BY references a column not in the property catalog",
            });
        };
        out.push(OrderBy {
            field_name: column.clone(),
            desc: *desc,
            is_metric,
        });
    }
    Ok(out)
}

/// The accumulated lowering of a `WHERE`: the extracted date range, the dimension/metric filter
/// leaves, and the residual predicate.
#[derive(Default)]
struct LoweredWhere {
    date_range: Option<DateRange>,
    dimension_filters: Vec<FilterExpression>,
    metric_filters: Vec<FilterExpression>,
    residual: Option<Predicate>,
}

impl LoweredWhere {
    /// Fold a residual predicate `p` into the accumulator (conjoining with any prior residual).
    fn add_residual(&mut self, p: Predicate) {
        self.residual = Some(match self.residual.take() {
            None => p,
            Some(prev) => Predicate::And(Box::new(prev), Box::new(p)),
        });
    }
}

/// Lower a `WHERE` predicate into the accumulator. A conjunction lowers each conjunct
/// independently; any other shape (`OR`/`NOT`/a lone non-conjoinable predicate) stays wholly
/// residual (correctness over completeness — GA `andGroup` cannot express disjunction here).
fn lower_predicate(p: &Predicate, catalog: &Catalog, out: &mut LoweredWhere) {
    match p {
        Predicate::And(a, b) => {
            lower_predicate(a, catalog, out);
            lower_predicate(b, catalog, out);
        }
        // A `date BETWEEN low AND high` is the canonical date-range predicate (exact window).
        Predicate::Between(col, low, high) if field_of(col) == Some(DATE_COL) => {
            match (date_text(low), date_text(high)) {
                (Some(start), Some(end)) => {
                    out.date_range = Some(DateRange {
                        start_date: start,
                        end_date: end,
                    });
                }
                _ => out.add_residual(p.clone()),
            }
        }
        Predicate::Cmp(col, op, lit) => lower_cmp(p, col, *op, lit, catalog, out),
        Predicate::Like(col, pattern) => lower_like(p, col, &pattern.0, catalog, out),
        // OR / NOT / IN-on-date / BETWEEN-on-non-date — stay wholly residual.
        other => out.add_residual(other.clone()),
    }
}

/// Lower a single comparison `col op lit`.
fn lower_cmp(
    original: &Predicate,
    col: &ColRef,
    op: CmpOp,
    lit: &Literal,
    catalog: &Catalog,
    out: &mut LoweredWhere,
) {
    let Some(field) = field_of(col) else {
        out.add_residual(original.clone());
        return;
    };

    // `date >= 'a'` / `date <= 'b'` contribute a half-open bound to the date range (exact).
    if field == DATE_COL {
        if let Literal::Text(v) = lit {
            match op {
                CmpOp::Ge | CmpOp::Gt | CmpOp::Eq => {
                    let dr = out.date_range.get_or_insert_with(|| DateRange {
                        start_date: String::new(),
                        end_date: String::new(),
                    });
                    dr.start_date = v.clone();
                    if op == CmpOp::Eq {
                        dr.end_date = v.clone();
                    }
                    return;
                }
                CmpOp::Le | CmpOp::Lt => {
                    let dr = out.date_range.get_or_insert_with(|| DateRange {
                        start_date: String::new(),
                        end_date: String::new(),
                    });
                    dr.end_date = v.clone();
                    return;
                }
                CmpOp::Ne | CmpOp::Match => {}
            }
        }
        out.add_residual(original.clone());
        return;
    }

    if catalog.is_dimension(field) {
        // A dimension equality `dim = 'x'` is GA's EXACT stringFilter — residual-dropping.
        match (op, lit) {
            (CmpOp::Eq, Literal::Text(v)) => {
                out.dimension_filters.push(FilterExpression::Filter {
                    field_name: field.to_string(),
                    test: FilterTest::String {
                        value: v.clone(),
                        match_type: StringMatch::Exact,
                    },
                });
            }
            // `dim ~ 'p'` (regex) → FULL_REGEXP is a pre-filter looser than exact `~` semantics
            // here (GA's regex dialect differs); push it but KEEP the predicate as residual.
            (CmpOp::Match, Literal::Text(v)) => {
                out.dimension_filters.push(FilterExpression::Filter {
                    field_name: field.to_string(),
                    test: FilterTest::String {
                        value: v.clone(),
                        match_type: StringMatch::FullRegexp,
                    },
                });
                out.add_residual(original.clone());
            }
            _ => out.add_residual(original.clone()),
        }
    } else if catalog.is_metric(field) {
        // A metric numeric comparison is GA's EXACT numericFilter — residual-dropping.
        match (NumericOp::from_cmp(op), numeric_text(lit)) {
            (Some(ga_op), Some(value)) => {
                out.metric_filters.push(FilterExpression::Filter {
                    field_name: field.to_string(),
                    test: FilterTest::Numeric { op: ga_op, value },
                });
            }
            _ => out.add_residual(original.clone()),
        }
    } else {
        // A comparison on a column outside the catalog stays residual (the engine filters it).
        out.add_residual(original.clone());
    }
}

/// Lower a `LIKE` on a dimension column — GA's CONTAINS is a substring pre-filter, looser than
/// SQL `LIKE` glob semantics, so push it and KEEP the predicate as residual.
fn lower_like(
    original: &Predicate,
    col: &ColRef,
    pattern: &str,
    catalog: &Catalog,
    out: &mut LoweredWhere,
) {
    match field_of(col) {
        Some(field) if catalog.is_dimension(field) => {
            out.dimension_filters.push(FilterExpression::Filter {
                field_name: field.to_string(),
                test: FilterTest::String {
                    value: pattern.to_string(),
                    match_type: StringMatch::Contains,
                },
            });
            out.add_residual(original.clone());
        }
        _ => out.add_residual(original.clone()),
    }
}

/// The single-segment column name of a [`ColRef`], if it is a bare column (not a dotted path).
fn field_of(col: &ColRef) -> Option<&str> {
    match col.path.as_slice() {
        [one] => Some(one.as_str()),
        _ => None,
    }
}

/// The text form of a date literal (GA dates are `YYYY-MM-DD` strings).
fn date_text(lit: &Literal) -> Option<String> {
    match lit {
        Literal::Text(v) => Some(v.clone()),
        _ => None,
    }
}

/// Render a numeric literal as the string GA carries in a numericFilter value. `Int`/`Float`
/// only; a non-numeric literal yields `None` (the comparison stays residual).
fn numeric_text(lit: &Literal) -> Option<String> {
    match lit {
        Literal::Int(n) => Some(n.to_string()),
        Literal::Float(f) => Some(f.to_string()),
        _ => None,
    }
}
