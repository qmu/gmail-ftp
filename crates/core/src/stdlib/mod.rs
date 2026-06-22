//! The cfs **standard library** of built-in pure functions + the **driver-prelude
//! registration mechanism** (RFD-0001 §3, ticket t08).
//!
//! This module *populates* the second open registry — functions + procedures — that
//! t06's name resolution *resolves against*. It realises two RFD §3 rules:
//!
//! 1. **Closed core + open function registry.** The stdlib is a small, stable
//!    vocabulary (`UPPER`/`SUBSTR`/`DATE_ADD`/`COALESCE`/`COUNT`/…) registered in the
//!    [`StdlibRegistry`]; a `fn(...)` in an expression resolves against it, adding
//!    **zero** keywords to the frozen core.
//! 2. **Aliases are pure functions, never keywords.** A driver contributes
//!    receiver-typed pure aliases (`SEND`, `MERGE`) through its
//!    [`prelude()`](cfs_driver::Driver::prelude); the [`StdlibRegistry`] merges them
//!    **namespaced by `DriverId`** (never flattened into the core namespace) so t06's
//!    receiver-typed resolution keeps them collision-proof and scoped.
//!
//! ## Purity invariant (the safety property, RFD §3)
//! Every built-in is **pure**: a scalar/aggregate fn maps `Value → Value` with **no
//! I/O**; the effectful-*shaped* ones (`READ`, `http.get`) construct a deferred
//! plan/source [`PlanNode`] and **never** perform the read here. The whole module is
//! data-in / data-out — no network, no filesystem, no wall clock (`NOW`/`CURRENT_DATE`
//! read a *frozen* [`EvalCtx`], making PREVIEW reproducible). This is what keeps every
//! plan dry-runnable and golden-testable without credentials.
//!
//! ## Capability gating (least-privilege, RFD §10)
//! `env()`, `READ`, and `http.get` reach outside the pure data plane. They are gated
//! behind the [`EvalCtx::capabilities`] flag (default **off**) so an unattended /
//! pure-eval context denies them; secret values an `env()` returns never appear in an
//! error string (the structured errors carry only the *name* requested).

mod aggregate;
mod context;
mod registry;
mod scalar;
mod tablevalued;

#[cfg(test)]
mod tests;

pub use aggregate::{AggregateFactory, AggregateKind, AggregateState};
pub use context::{EnvSource, MapEnv, NoEnv};
pub use registry::{AliasDecl, Prelude, PreludeError, ResolvedAlias, StdlibRegistry};
pub use tablevalued::{PlanNode, PlanNodeKind};

use cfs_types::{ColumnType, Value};

/// The read-only context the pure built-ins may consult (RFD §3 determinism). `NOW`/
/// `CURRENT_DATE`/`LAST_RUN`/`env` are **data**, frozen per statement — never a live
/// wall-clock read or an ambient lookup mid-evaluation. This is what makes PREVIEW and
/// golden tests reproducible.
pub struct EvalCtx<'a> {
    /// The frozen "now" timestamp for this statement (epoch seconds). Every `NOW()`
    /// call in one statement returns this same value (determinism).
    pub now: i64,
    /// The frozen current date for this statement (epoch days). Every `CURRENT_DATE()`
    /// call in one statement returns this same value.
    pub current_date: i64,
    /// The last successful run timestamp injected by the server/job binding (RFD §8),
    /// or `None` when unset (then `LAST_RUN()` yields `Null`). Injected state, never
    /// ambient.
    pub last_run: Option<i64>,
    /// The capability/policy gate (RFD §10). When `false` (the default for pure-eval),
    /// `env()`/`READ`/`http.get` are **denied** — unattended execution cannot reach
    /// outside the data plane.
    pub capabilities_enabled: bool,
    /// The environment source `env(name)` resolves through; the server can restrict it
    /// per-handler (default-deny in unattended contexts).
    pub env: &'a dyn EnvSource,
}

impl<'a> EvalCtx<'a> {
    /// A deterministic, capability-**denied** context for pure-eval / golden tests:
    /// frozen `now`/`current_date`, no `last_run`, and `env`/`READ`/`http.get` denied.
    #[must_use]
    pub fn pure(now: i64, current_date: i64, env: &'a dyn EnvSource) -> Self {
        Self {
            now,
            current_date,
            last_run: None,
            capabilities_enabled: false,
            env,
        }
    }

    /// Builder: inject the last-run timestamp (RFD §8 job state).
    #[must_use]
    pub fn with_last_run(mut self, last_run: Option<i64>) -> Self {
        self.last_run = last_run;
        self
    }

    /// Builder: enable the capability gate so `env()`/`READ`/`http.get` are permitted
    /// (the server enables this only for an authorised handler, RFD §10).
    #[must_use]
    pub fn with_capabilities(mut self, enabled: bool) -> Self {
        self.capabilities_enabled = enabled;
        self
    }
}

/// The structured, AI-consumable error a built-in evaluation can raise (RFD §5). Every
/// arm carries actionable context; **credentials/secret values never appear** (a denied
/// `env('SECRET')` carries only the *name*, never the would-be value).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum FnError {
    /// A `fn(...)` named no registered built-in (and is not a prelude alias). Carries the
    /// unknown name so the AI can correct it.
    UnknownFunction {
        /// The unresolved function name.
        name: String,
    },
    /// A built-in was called with the wrong number of arguments.
    Arity {
        /// The function name.
        name: String,
        /// The argument count the function expects (a representative count).
        expected: usize,
        /// The argument count supplied.
        found: usize,
    },
    /// An argument had a type the built-in cannot accept (e.g. `UPPER(123)`). Carries the
    /// stable type labels so the AI can coerce — never the offending *value*.
    Type {
        /// The function name.
        name: String,
        /// The expected type label (e.g. `Text`).
        expected: &'static str,
        /// The type label actually supplied.
        found: &'static str,
    },
    /// A scalar argument was out of the function's valid domain (e.g. `SUBSTR` start of
    /// 0, a malformed date string). Carries a short, secret-free reason.
    Domain {
        /// The function name.
        name: String,
        /// A short, machine-stable reason (no secret values).
        reason: &'static str,
    },
    /// An aggregate function (`SUM`/`COUNT`/…) was used outside an `AGGREGATE` context
    /// (e.g. in a `WHERE`). A *typed* error, never a runtime panic (RFD §3 dispatch).
    AggregateOutsideAggregate {
        /// The aggregate function name.
        name: String,
    },
    /// A scalar function (`UPPER`/…) was used where an aggregate is required (under
    /// `AGGREGATE` with a `GROUP BY`). The dual of [`FnError::AggregateOutsideAggregate`].
    ScalarInAggregate {
        /// The scalar function name.
        name: String,
    },
    /// A capability-gated built-in (`env`/`READ`/`http.get`) was called with the gate
    /// **off** (RFD §10 least-privilege). Carries only the *name* requested — never the
    /// value an `env()` would have returned.
    CapabilityDenied {
        /// The built-in that was denied (`env`/`READ`/`http.get`).
        builtin: &'static str,
        /// The name/argument requested (e.g. the env var name) — never its value.
        requested: String,
    },
}

impl FnError {
    /// A stable, machine-readable code an AI-facing caller branches on (RFD §5).
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            FnError::UnknownFunction { .. } => "unknown_function",
            FnError::Arity { .. } => "fn_arity",
            FnError::Type { .. } => "fn_type",
            FnError::Domain { .. } => "fn_domain",
            FnError::AggregateOutsideAggregate { .. } => "aggregate_outside_aggregate",
            FnError::ScalarInAggregate { .. } => "scalar_in_aggregate",
            FnError::CapabilityDenied { .. } => "capability_denied",
        }
    }
}

/// The declared signature of a built-in (RFD §5 typed dispatch). Kept deliberately small:
/// the **arity policy** (fixed/variadic) and the **return type**. Per-argument type
/// checks live in each function body (they reject ill-typed `Value`s with
/// [`FnError::Type`]), so a heterogeneous signature (e.g. `COALESCE`) need not be encoded
/// structurally here.
#[derive(Debug, Clone, PartialEq)]
pub struct FnSig {
    /// The minimum number of positional arguments.
    pub min_args: usize,
    /// The maximum number of positional arguments, or `None` for variadic
    /// (`COALESCE`/`CONCAT`).
    pub max_args: Option<usize>,
    /// The built-in's return type (the column type a projection over it carries).
    pub returns: ColumnType,
}

impl FnSig {
    /// A fixed-arity signature of exactly `n` arguments.
    #[must_use]
    pub fn fixed(n: usize, returns: ColumnType) -> Self {
        Self {
            min_args: n,
            max_args: Some(n),
            returns,
        }
    }

    /// A range-arity signature accepting `min..=max` arguments.
    #[must_use]
    pub fn range(min: usize, max: usize, returns: ColumnType) -> Self {
        Self {
            min_args: min,
            max_args: Some(max),
            returns,
        }
    }

    /// A variadic signature accepting `min` or more arguments.
    #[must_use]
    pub fn variadic(min: usize, returns: ColumnType) -> Self {
        Self {
            min_args: min,
            max_args: None,
            returns,
        }
    }

    /// Whether `argc` arguments satisfy this arity policy.
    #[must_use]
    pub fn accepts_arity(&self, argc: usize) -> bool {
        argc >= self.min_args && self.max_args.is_none_or(|max| argc <= max)
    }
}

/// How a built-in evaluates (RFD §3): a pure scalar `Value → Value`, a grouped aggregate
/// (init/accumulate/finalize), or an effectful-*shaped* table source that constructs a
/// deferred [`PlanNode`] (never performing I/O here).
pub enum BuiltinEval {
    /// A pure scalar: maps argument values to a result value, consulting the read-only
    /// [`EvalCtx`] (for `NOW`/`env`/…). Never performs I/O.
    Scalar(fn(&[Value], &EvalCtx) -> Result<Value, FnError>),
    /// A grouped aggregate (`COUNT`/`SUM`/…): a factory producing an
    /// init/accumulate/finalize [`AggregateState`]. Only valid under `AGGREGATE`.
    Aggregate(AggregateKind),
    /// A table-valued / effectful-shaped source (`READ`/`http.get`): constructs a
    /// deferred [`PlanNode`] (gated by [`EvalCtx::capabilities_enabled`]) but performs
    /// **no** network/file I/O during evaluation.
    TableValued(fn(&[Value], &EvalCtx) -> Result<PlanNode, FnError>),
}

/// A single registered built-in (RFD §3). Name + signature + the evaluation strategy.
pub struct BuiltinFn {
    /// The function's surface name (e.g. `UPPER`, `COUNT`, `http.get`).
    pub name: String,
    /// The declared arity policy + return type.
    pub sig: FnSig,
    /// How it evaluates (scalar / aggregate / table-valued).
    pub eval: BuiltinEval,
}

impl BuiltinFn {
    /// Construct a scalar built-in.
    fn scalar(name: &str, sig: FnSig, f: fn(&[Value], &EvalCtx) -> Result<Value, FnError>) -> Self {
        Self {
            name: name.to_string(),
            sig,
            eval: BuiltinEval::Scalar(f),
        }
    }

    /// Construct an aggregate built-in.
    fn aggregate(name: &str, kind: AggregateKind, returns: ColumnType) -> Self {
        Self {
            name: name.to_string(),
            sig: FnSig::range(1, 1, returns),
            eval: BuiltinEval::Aggregate(kind),
        }
    }

    /// Construct a table-valued built-in.
    fn table_valued(
        name: &str,
        sig: FnSig,
        f: fn(&[Value], &EvalCtx) -> Result<PlanNode, FnError>,
    ) -> Self {
        Self {
            name: name.to_string(),
            sig,
            eval: BuiltinEval::TableValued(f),
        }
    }

    /// Whether this built-in is an aggregate (used by aggregate-vs-scalar dispatch).
    #[must_use]
    pub fn is_aggregate(&self) -> bool {
        matches!(self.eval, BuiltinEval::Aggregate(_))
    }
}

/// The stable type label for a [`Value`], for [`FnError::Type`] (no value content). Mirrors
/// [`Value::type_of`] but yields a `'static` label suitable for the structured error.
#[must_use]
pub(crate) fn value_type_label(v: &Value) -> &'static str {
    match v {
        Value::Null => "Null",
        Value::Bool(_) => "Bool",
        Value::Int(_) => "Int",
        Value::Float(_) => "Float",
        Value::Text(_) => "Text",
        Value::Bytes(_) => "Bytes",
        Value::Timestamp(_) => "Timestamp",
        Value::Struct(_) => "Struct",
        Value::Array(_) => "Array",
        Value::Json(_) => "Json",
        // `Value` is `#[non_exhaustive]`; a future variant reports an honest fallback
        // label rather than failing to compile (lib code stays panic-free).
        _ => "Unknown",
    }
}
