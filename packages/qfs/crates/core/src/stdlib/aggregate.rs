//! **Aggregate** built-ins over groups (RFD-0001 §3, ticket t08): `COUNT`/`SUM`/`AVG`/
//! `MIN`/`MAX`, plus the `COUNT(DISTINCT …)` variant. Each is an init / accumulate /
//! finalize machine — the [`AggregateState`] — usable under `AGGREGATE … GROUP BY` (the
//! t04 grammar). Pure: accumulation is fold-over-values with no I/O.
//!
//! ## Aggregate-vs-scalar dispatch (RFD §3, the typed-error rule)
//! `COUNT`/`SUM`/… are valid **only** under `AGGREGATE`. The [`crate::stdlib::FnRegistry`]
//! distinguishes the two by [`BuiltinFn::is_aggregate`](super::BuiltinFn::is_aggregate);
//! using an aggregate in a `WHERE` (or a scalar where an aggregate is required) is a
//! **typed** [`FnError`](super::FnError), never a runtime panic.

use qfs_types::{ColumnType, Value};

use super::{value_type_label, BuiltinFn, FnError};

/// Which aggregate a built-in computes. Carries the `DISTINCT` flag for `COUNT(DISTINCT)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AggregateKind {
    /// `COUNT(x)` — non-null count, or the `DISTINCT` distinct-non-null count.
    Count {
        /// Whether to count distinct values only (`COUNT(DISTINCT x)`).
        distinct: bool,
    },
    /// `SUM(x)` — numeric sum (`Null` over an empty/all-null group).
    Sum,
    /// `AVG(x)` — numeric mean (`Null` over an empty/all-null group).
    Avg,
    /// `MIN(x)` — the minimum (numeric or text).
    Min,
    /// `MAX(x)` — the maximum (numeric or text).
    Max,
    /// `ARRAY_AGG(x)` — collect the group's values (in accumulation order) into one `Array`
    /// value (t92). A faithful collect (keeps nulls), not a numeric fold.
    ArrayAgg,
}

/// The set of aggregate built-ins, in stable (name) order. `COUNT` is registered once
/// (the `DISTINCT` variant is selected at the call site by the parser's argument form;
/// here we expose the plain form and a dedicated factory entry for the distinct case).
pub(super) fn aggregate_builtins() -> Vec<BuiltinFn> {
    vec![
        BuiltinFn::aggregate(
            "COUNT",
            AggregateKind::Count { distinct: false },
            ColumnType::Int,
        ),
        BuiltinFn::aggregate("SUM", AggregateKind::Sum, ColumnType::Float),
        BuiltinFn::aggregate("AVG", AggregateKind::Avg, ColumnType::Float),
        BuiltinFn::aggregate("MIN", AggregateKind::Min, ColumnType::Unknown),
        BuiltinFn::aggregate("MAX", AggregateKind::Max, ColumnType::Unknown),
        BuiltinFn::aggregate(
            "ARRAY_AGG",
            AggregateKind::ArrayAgg,
            ColumnType::Array(Box::new(ColumnType::Unknown)),
        ),
    ]
}

/// A factory for a fresh [`AggregateState`] — one per group (RFD §3 init/accumulate/
/// finalize). Cheap to clone; carries only the aggregate kind.
#[derive(Debug, Clone, Copy)]
pub struct AggregateFactory {
    kind: AggregateKind,
}

impl AggregateFactory {
    /// A factory for the given aggregate kind.
    #[must_use]
    pub fn new(kind: AggregateKind) -> Self {
        Self { kind }
    }

    /// A factory for `COUNT(DISTINCT x)` (the distinct-count variant the parser selects
    /// from the argument form).
    #[must_use]
    pub fn count_distinct() -> Self {
        Self {
            kind: AggregateKind::Count { distinct: true },
        }
    }

    /// Begin a fresh accumulation for one group.
    #[must_use]
    pub fn init(&self) -> AggregateState {
        AggregateState::new(self.kind)
    }
}

/// The running state of one aggregate over one group (RFD §3). `accumulate` folds each
/// row's value in; `finalize` produces the group's result. Pure — no I/O.
#[derive(Debug, Clone)]
pub struct AggregateState {
    kind: AggregateKind,
    count: i64,
    sum: f64,
    min: Option<Value>,
    max: Option<Value>,
    /// Distinct non-null values seen (only populated for `COUNT(DISTINCT)`).
    distinct: Vec<Value>,
    /// Collected values in accumulation order (only populated for `ARRAY_AGG`).
    collected: Vec<Value>,
    /// Whether any non-null numeric value contributed (so `SUM`/`AVG` of an empty group
    /// is `Null`, not `0`).
    saw_numeric: bool,
}

impl AggregateState {
    fn new(kind: AggregateKind) -> Self {
        Self {
            kind,
            count: 0,
            sum: 0.0,
            min: None,
            max: None,
            distinct: Vec::new(),
            collected: Vec::new(),
            saw_numeric: false,
        }
    }

    /// Fold one value into the running aggregate. `Null` is skipped for every aggregate
    /// (SQL semantics: aggregates ignore nulls).
    ///
    /// # Errors
    /// [`FnError::Type`] if `SUM`/`AVG` meets a non-numeric value.
    pub fn accumulate(&mut self, v: &Value) -> Result<(), FnError> {
        // ARRAY_AGG is a faithful collect — it keeps every value, including nulls (mirroring
        // the engine's `run_aggregate` collect), so it is handled before the null-skip guard.
        if matches!(self.kind, AggregateKind::ArrayAgg) {
            self.collected.push(v.clone());
            return Ok(());
        }
        if matches!(v, Value::Null) {
            return Ok(());
        }
        match self.kind {
            AggregateKind::Count { distinct } => {
                if distinct {
                    if !self.distinct.iter().any(|d| d == v) {
                        self.distinct.push(v.clone());
                    }
                } else {
                    self.count += 1;
                }
            }
            AggregateKind::Sum | AggregateKind::Avg => {
                let n = numeric(v).ok_or_else(|| FnError::Type {
                    name: self.kind_name().to_string(),
                    expected: "Float",
                    found: value_type_label(v),
                })?;
                self.sum += n;
                self.count += 1;
                self.saw_numeric = true;
            }
            AggregateKind::Min => {
                if self.min.as_ref().is_none_or(|m| less_than(v, m)) {
                    self.min = Some(v.clone());
                }
            }
            AggregateKind::Max => {
                if self.max.as_ref().is_none_or(|m| less_than(m, v)) {
                    self.max = Some(v.clone());
                }
            }
            // Handled by the early collect above (kept for match exhaustiveness).
            AggregateKind::ArrayAgg => {}
        }
        Ok(())
    }

    /// Produce the group's final value (RFD §3). `COUNT` is always an `Int`; `SUM`/`AVG`
    /// are `Null` over an empty/all-null group; `MIN`/`MAX` are `Null` over an empty
    /// group.
    #[must_use]
    pub fn finalize(self) -> Value {
        match self.kind {
            AggregateKind::Count { distinct } => Value::Int(if distinct {
                self.distinct.len() as i64
            } else {
                self.count
            }),
            AggregateKind::Sum => {
                if self.saw_numeric {
                    Value::Float(self.sum)
                } else {
                    Value::Null
                }
            }
            AggregateKind::Avg => {
                if self.saw_numeric && self.count > 0 {
                    Value::Float(self.sum / self.count as f64)
                } else {
                    Value::Null
                }
            }
            AggregateKind::Min => self.min.unwrap_or(Value::Null),
            AggregateKind::Max => self.max.unwrap_or(Value::Null),
            AggregateKind::ArrayAgg => Value::Array(self.collected),
        }
    }

    fn kind_name(&self) -> &'static str {
        match self.kind {
            AggregateKind::Count { .. } => "COUNT",
            AggregateKind::Sum => "SUM",
            AggregateKind::Avg => "AVG",
            AggregateKind::Min => "MIN",
            AggregateKind::Max => "MAX",
            AggregateKind::ArrayAgg => "ARRAY_AGG",
        }
    }
}

/// The numeric value of `v` for `SUM`/`AVG`, or `None` if it is non-numeric.
fn numeric(v: &Value) -> Option<f64> {
    match v {
        Value::Int(n) | Value::Timestamp(n) => Some(*n as f64),
        Value::Float(f) => Some(*f),
        _ => None,
    }
}

/// Whether `a < b` for `MIN`/`MAX` ordering. Numbers compare numerically; text compares
/// lexically; mixed/other types compare as not-less (stable, no panic).
fn less_than(a: &Value, b: &Value) -> bool {
    match (numeric(a), numeric(b)) {
        (Some(x), Some(y)) => x < y,
        _ => match (a, b) {
            (Value::Text(x), Value::Text(y)) => x < y,
            (Value::Bool(x), Value::Bool(y)) => !x & y,
            _ => false,
        },
    }
}
