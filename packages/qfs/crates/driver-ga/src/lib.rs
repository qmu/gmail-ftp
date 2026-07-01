//! `qfs-driver-ga` — the **Google Analytics 4 read-only relational `Driver`** (RFD-0001 §5, E4
//! t41). GA4's Data API (`properties.runReport` / `runRealtimeReport`) is fundamentally a *query*
//! surface — you ask for **metrics** grouped by **dimensions** over a **date range** with
//! **filters**, and GA aggregates server-side and returns rows. That maps onto the qfs
//! **relational archetype** with one honest constraint: GA is a **query source, never a mutate
//! target** (you do not `INSERT`/`UPDATE`/`REMOVE` analytics data). Like the SQL driver, the
//! *entire* pipeline pushes down to one native `runReport` call (GA does the aggregation), so this
//! is a **pushdown target** (RFD §5 relational, §6 pushdown). It reuses the shared Google OAuth
//! base (t19) and the driver contract (t13).
//!
//! ## Surface
//! - [`GaDriver`] — the introspective `Driver`: `mount()` = `/ga`, the
//!   [`Archetype::RelationalTable`] per-node archetype + a typed [`Schema`] (the property's
//!   dimension+metric catalog), **`SELECT`-only** capabilities (every write verb absent ⇒ rejected
//!   at the parse-time gate), and a `Partial` [`PushdownProfile`] declaring
//!   `where_/project/limit/order/aggregate/group_by` (one `runReport` runs the whole query).
//! - [`GaApplier`] — the apply leg the contract returns from `applier()`. GA is **read-only**, so
//!   it rejects every effect with a structured [`GaError::ReadOnly`] (belt-and-suspenders behind
//!   the capability gate). Report rows flow through the pure read path ([`report`]), never here.
//! - [`ga_apply_driver`] — wraps the (reject-all) applier in a [`qfs_runtime::PlanApplierBridge`]
//!   ready to `register` under the driver id `ga`, so a (read) plan over `/ga` is routed correctly
//!   and any stray write is rejected end-to-end.
//!
//! ## Read-only enforcement (RFD §5/§10 — the honest archetype)
//! Read-only is enforced in **two** places that cannot drift apart:
//! 1. **Parse-time** — [`GaDriver::capabilities`] returns a `SELECT`-only set, so
//!    `qfs_driver::check_capability` rejects `INSERT`/`UPSERT`/`UPDATE`/`REMOVE` structurally
//!    before a `Plan` exists, with the supported-verb list for AI recovery.
//! 2. **Apply-time** — [`GaApplier`] rejects every effect, so even a hand-built plan that bypassed
//!    the gate cannot mutate GA. The least-privilege scope is `analytics.readonly` (never a
//!    broader Google scope, RFD §10).
//!
//! ## Query → `RunReportRequest` with a TRUTHFUL residual (the t20/t21 lesson)
//! [`compile::compile`] lowers a relational query (projection of dimension/metric names, `WHERE`,
//! `ORDER BY`, `LIMIT`) into one [`compile::RunReportRequest`]. A `WHERE` conjunct is pushed as a
//! residual-dropping GA filter **only** when the GA filter means *exactly* the SQL predicate
//! (`dim = 'x'` → EXACT `stringFilter`; `metric > n` → `numericFilter`; the `date` predicate →
//! `dateRanges[]`). Every looser GA filter (`dim LIKE`/`~` → CONTAINS/FULL_REGEXP) is pushed as a
//! pre-filter and the exact predicate is **kept as residual** so the engine re-applies exact
//! filtering — over-fetch then filter, never wrong rows (RFD §6). `OR`/`NOT`/unsupported columns
//! stay wholly residual.
//!
//! ## Mandatory date range + catalog validation (RFD §5)
//! GA4 requires a date range on every core report; the compiler errors with
//! [`GaError::MissingDateRange`] (structured, AI-correctable) rather than fabricating one. A
//! projected/filtered name that is neither a catalog dimension nor metric yields
//! [`GaError::UnknownField`] — turning a would-be raw GA 400 into a self-correctable error. A
//! realtime report (`/ga/<property>/realtime`) needs no date range (the last ~30 minutes).
//!
//! ## Auth + multi-account + least privilege (RFD §10)
//! Auth (tokens, refresh, multi-account) comes from t19; credential storage from t27. The bearer
//! is injected by the t19 `GoogleApiClient` and lives behind a [`qfs_secrets::Secret`]; it is
//! **never** logged, never in a DTO, never in a [`GaError`]. Multi-account is the t19 base: one
//! `GoogleApiClient` per account (and the GA property id selects which property under that
//! account); the driver is account-agnostic (the resolved account is bound at client
//! construction).
//!
//! ## No vendor leak (RFD §9)
//! GA4 JSON is translated into owned DTOs at the [`client`] boundary; the `Driver` surface and the
//! compiled request carry zero google types. The HTTP client is behind the mockable [`GaClient`]
//! trait so it mocks in tests (no live GA, no network) and `reqwest` stays in `qfs-driver-http` —
//! this crate rides the t19 `HttpExchange` seam.
//!
//! ## Named parks (deferred)
//! - **Sampling / quota — surfaced, backoff via the runtime.** [`report::ReportResponse`] carries
//!   a `sampled` flag (from `metadata.samplingMetadatas`); the GA4 quota `RESOURCE_EXHAUSTED`
//!   maps to a retryable 429 [`GaError`] so the runtime's t12 retry/circuit-breaker backs off. A
//!   dedicated sampling marker *column* and per-property quota accounting are follow-ups.
//! - **`checkCompatibility` pre-flight — catalog-driven validation present.** Unknown fields are
//!   rejected against the catalog ([`GaError::UnknownField`]); the GA `checkCompatibility`
//!   round-trip for incompatible dimension×metric *combinations* is a follow-up.
//! - **Service-account credential path — credential store is t27.** The `analytics.readonly`
//!   OAuth path is wired; a service-account JWT path for unattended server reports lands with the
//!   credential-store work.

#![forbid(unsafe_code)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

mod applier;
pub mod catalog;
pub mod client;
pub mod compile;
mod error;
mod path;
pub mod report;

use std::sync::Arc;

use qfs_driver::{
    Archetype, Capabilities, Driver, NodeDesc, Path, ProcSig, PushdownProfile, Verb, VersionSupport,
};
use qfs_plan::PlanApplier;
use qfs_runtime::PlanApplierBridge;

pub use applier::GaApplier;
pub use catalog::{Catalog, GaDimension, GaMetric, MetricKind};
pub use client::{GaClient, GoogleApiGaClient, MockGaClient, RecordedCall};
pub use compile::{
    compile, CompileResult, DateRange, FilterExpression, FilterTest, NumericOp, OrderBy, QuerySpec,
    RunReportRequest, StringMatch, DATE_COL,
};
pub use error::GaError;
pub use path::{GaPath, DEPRECATED_MOUNT, MOUNT, REALTIME_SEGMENT};
pub use report::{response_to_rows, ReportResponse, ReportRow};

/// The least-privilege GA scope — **read-only** analytics. NOT a write/admin scope (GA has no
/// write surface here) and NOT a full-account scope (RFD §10 blast radius). Declared so the
/// server `POLICY` can reason about blast radius.
pub const ANALYTICS_READONLY_SCOPE: &str = "https://www.googleapis.com/auth/analytics.readonly";

/// The Google Analytics 4 driver (RFD §5). Owns the read-only [`GaApplier`] the contract returns
/// from `applier()` and the declared pushdown profile. Construct with [`GaDriver::new`], injecting
/// the [`GaClient`] (auth is injected there at construction — the real client wraps a per-account
/// `GoogleApiClient`; never on the contract surface).
pub struct GaDriver {
    client: Arc<dyn GaClient>,
    applier: GaApplier,
    pushdown: PushdownProfile,
    procs: Vec<ProcSig>,
}

impl GaDriver {
    /// Build a GA driver over `client`. In production `client` is a [`GoogleApiGaClient`] wrapping
    /// a per-account `GoogleApiClient` (bearer + refresh-on-401); in tests it is a
    /// [`MockGaClient`].
    #[must_use]
    pub fn new(client: Arc<dyn GaClient>) -> Self {
        Self {
            client: Arc::clone(&client),
            applier: GaApplier::new(client),
            // The whole relational subtree compiles to one runReport: GA runs WHERE
            // (dimension/metric filters), projection (dimensions+metrics), LIMIT, ORDER BY,
            // and the aggregation/group_by server-side. Joins/distinct stay local. Residual
            // WHERE predicates combine locally — see `compile::compile`.
            pushdown: PushdownProfile::Partial {
                where_: true,
                project: true,
                limit: true,
                order: true,
                join: false,
                aggregate: true,
                distinct: false,
                group_by: true,
            },
            // GA is read-only: NO mutating procedures are declared.
            procs: Vec::new(),
        }
    }

    /// Borrow the read-only applier (e.g. to build the runtime bridge).
    #[must_use]
    pub fn ga_applier(&self) -> &GaApplier {
        &self.applier
    }

    /// Fetch a property's dimension+metric catalog through the injected client (the impure
    /// `getMetadata` call powering `DESCRIBE`). Kept separate from the pure introspective
    /// [`Driver::describe`] (which cannot do I/O): the engine fetches the catalog here, then
    /// `describe`-projects it.
    ///
    /// # Errors
    /// [`GaError`] on a non-2xx status, a decode failure, or an auth/transport failure.
    pub fn fetch_catalog(&self, property_id: &str) -> Result<Catalog, GaError> {
        self.client.get_metadata(property_id)
    }

    /// Run a compiled report through the injected client and project the response onto the
    /// requested column order as typed rows (the read path; the only place GA report I/O happens).
    ///
    /// # Errors
    /// [`GaError`] on a non-2xx status, a decode/arity failure, or an auth/transport failure.
    pub fn run_report(
        &self,
        request: &RunReportRequest,
        catalog: &Catalog,
    ) -> Result<(Vec<qfs_types::Row>, bool), GaError> {
        let response = self.client.run_report(request)?;
        let rows = response_to_rows(request, catalog, &response)?;
        Ok((rows, response.sampled))
    }

    /// Compile + run a relational query against a `/ga` property in one call: parse the path → fetch
    /// the property catalog → compile to a `runReport` request (the mandatory date range + the
    /// dimension/metric field validation live in [`compile`]) → run it → project the response onto
    /// its typed schema. Returns the projected rows, their schema (in `dimensions then metrics`
    /// order), and the residual the engine still re-filters locally (RFD §6). The read counterpart
    /// of [`fetch_catalog`](Self::fetch_catalog) + [`run_report`](Self::run_report) composed.
    ///
    /// # Errors
    /// [`GaError`] when the path is not a concrete property, the projection is empty, a field is
    /// unknown, a core report carries no `date` predicate, or the client hits an auth / transport /
    /// API failure (secret-free `code`).
    pub fn execute_query(
        &self,
        path: &Path,
        spec: &QuerySpec,
    ) -> Result<
        (
            Vec<qfs_types::Row>,
            qfs_types::Schema,
            Option<qfs_types::Predicate>,
        ),
        GaError,
    > {
        let (property_id, realtime) = match GaPath::parse(path)? {
            GaPath::Property { property_id } => (property_id, false),
            GaPath::Realtime { property_id } => (property_id, true),
            GaPath::Root => {
                return Err(GaError::InvalidPath {
                    path: path.as_str().to_string(),
                    reason: "not a concrete /ga/<propertyId> report",
                })
            }
        };
        let catalog = self.fetch_catalog(&property_id)?;
        let compiled = compile(&property_id, realtime, &catalog, spec)?;
        let (rows, _sampled) = self.run_report(&compiled.request, &catalog)?;
        // Project the property's full describe schema onto the requested (dimensions then metrics)
        // column order — the order `response_to_rows` emits row values in.
        let full = catalog.describe_schema();
        let schema = qfs_types::Schema::new(
            compiled
                .request
                .dimensions
                .iter()
                .chain(compiled.request.metrics.iter())
                .filter_map(|name| full.columns.iter().find(|c| &c.name == name).cloned())
                .collect(),
        );
        Ok((rows, schema, compiled.residual))
    }

    /// The `SELECT`-only capability set (RFD §5): a concrete property node (core or realtime)
    /// admits `SELECT` and nothing else; the virtual root and any invalid path admit nothing.
    /// Every write verb is absent, so the parse-time gate rejects `INSERT`/`UPSERT`/`UPDATE`/
    /// `REMOVE` structurally — read-only by construction.
    fn caps_for(path: &Path) -> Capabilities {
        match GaPath::parse(path) {
            Ok(p) if p.is_property() => Capabilities::from_verbs(&[Verb::Select]),
            Ok(_) | Err(_) => Capabilities::none(),
        }
    }
}

impl Driver for GaDriver {
    fn mount(&self) -> &str {
        MOUNT
    }

    /// The internal **plan/connection identity** stays `ga` even though the mount was renamed to
    /// `/google-analytics` (owner item #8). The default `id()` would derive `google-analytics` from
    /// the mount, but the runtime driver id keys the read-facet registry, the consent-scope map, and
    /// the stored connection selector (`qfs connection add ga`) — renaming it would orphan existing
    /// GA connections. Keeping `ga` confines the rename to the user-facing PATH surface.
    fn id(&self) -> qfs_types::DriverId {
        qfs_types::DriverId::new("ga")
    }

    fn describe(&self, path: &Path) -> Result<NodeDesc, qfs_driver::CfsError> {
        // Every queryable /ga node is the relational archetype. The schema is the property's
        // catalog; the pure introspective method cannot fetch it (no I/O), so it reports the
        // relational archetype with an empty schema placeholder — the engine fills the typed
        // columns from `fetch_catalog(...).describe_schema()`. An invalid/root path is not
        // describable.
        let ga = GaPath::parse(path).map_err(|_| qfs_driver::CfsError::InvalidPath {
            path: path.as_str().to_string(),
            reason: "not a valid /ga address",
        })?;
        if ga.is_property() {
            Ok(NodeDesc::new(
                Archetype::RelationalTable,
                qfs_types::Schema::new(Vec::new()),
            ))
        } else {
            Err(qfs_driver::CfsError::InvalidPath {
                path: path.as_str().to_string(),
                reason: "the /ga root carries no relation; address a property (/ga/<id>)",
            })
        }
    }

    fn capabilities(&self, path: &Path) -> Capabilities {
        Self::caps_for(path)
    }

    fn procedures(&self) -> &[ProcSig] {
        &self.procs
    }

    fn pushdown(&self) -> &PushdownProfile {
        &self.pushdown
    }

    fn version_support(&self, _path: &Path) -> VersionSupport {
        // GA reports are point-in-time aggregates over a date range — no version coordinate.
        VersionSupport::None
    }

    fn applier(&self) -> &dyn PlanApplier {
        &self.applier
    }
}

/// Wrap a [`GaDriver`]'s read-only applier in the runtime [`PlanApplierBridge`], yielding the async
/// `ApplyDriver` ready to `register` into a `DriverRegistry` under the driver id `ga`. A plan
/// routed to `/ga` then executes through the t10 interpreter; any stray write effect is rejected
/// with the structured read-only error.
#[must_use]
pub fn ga_apply_driver(driver: &GaDriver) -> PlanApplierBridge<GaApplier> {
    PlanApplierBridge::new(Arc::new(driver.ga_applier().clone()))
}

#[cfg(test)]
mod tests;
