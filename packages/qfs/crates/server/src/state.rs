//! The server's own configuration as **owned, vendor-free data** (RFD-0001 §8/§9).
//!
//! [`ServerState`] is the single source of truth for the running server: its endpoints,
//! triggers, jobs, views, policies, and webhooks. It is **not special** — it is the
//! state of the `/server/...` driver, mutated only by an
//! [`EffectKind::ServerConfigWrite`](qfs_core::EffectKind::ServerConfigWrite) under
//! `COMMIT` (see [`crate::driver`] / [`crate::runtime`]).
//!
//! ## Least-privilege & secrets (RFD §10)
//! Every DTO references policies / credentials **by handle**, never an inline token. The
//! `Debug` impls are derived (the fields are handles + routes + plan ids, not secrets),
//! but [`ServerState`] is **never logged verbatim** — the runtime logs *counts* and
//! *names*, never the whole registry, so a future credential-bearing field cannot leak by
//! an incidental `{:?}`. `POLICY` rows are stored now but **enforced in t34**; until then
//! no handler runs constrained by them (documented gap).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// An opaque reference to a parsed statement / effect-plan body (RFD §8). The runtime
/// stores the config row's plan as its **source text** — an owned, vendor-free string the
/// binding (E7) re-parses and lowers when it fires. Keeping it as text (not a live
/// `qfs_plan::Plan`) keeps [`ServerState`] `Serialize`/`Deserialize` and snapshot-stable
/// (a `Plan` carries `NodeId`s that are not a stable serialized identity).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct StatementSource(pub String);

impl StatementSource {
    /// Construct a statement source from owned text.
    #[must_use]
    pub fn new(src: impl Into<String>) -> Self {
        Self(src.into())
    }

    /// The raw statement text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// An HTTP endpoint definition (`CREATE ENDPOINT name ON 'METHOD /route' AS <query>`).
/// The t31 HTTP binding turns this into an axum route; here it is pure data.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct EndpointDef {
    /// The handler name (the config row key).
    pub name: String,
    /// The HTTP method (`GET`/`POST`/…), uppercased; empty if unspecified.
    pub method: String,
    /// The route path, e.g. `/recent`.
    pub route: String,
    /// The backing query the endpoint serves (`AS <query>`), as source text.
    pub query: StatementSource,
    /// The optional read-only-policy handle (a `/server/policies` row name) the t32 HTTP
    /// binding consults to decide whether a write-lowering query is permitted. `None`
    /// (the t31 default) means the endpoint is read-only by default — a write effect is
    /// refused. The full POLICY engine is t34; this is the registration-time gate handle.
    #[serde(default)]
    pub policy: Option<String>,
}

/// An event-trigger definition (`CREATE TRIGGER name ON <event> DO <plan>`). The t33
/// trigger poller fires `plan` when `on` matches; here it is pure data.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct TriggerDef {
    /// The trigger name (the config row key).
    pub name: String,
    /// The event this trigger fires on (raw, e.g. `inbox`); empty if unspecified.
    pub on: String,
    /// The optional `WHERE <pred>` guard (t34, CO-t31-4), as the canonical StatementSpec source
    /// (a query wrapping the predicate). Empty when the trigger declares no guard — the watchtower
    /// dispatcher treats an empty predicate as "always fire". Rehydrated (no re-parse) + evaluated
    /// over `NEW.*` at fire time.
    #[serde(default)]
    pub predicate: StatementSource,
    /// The effect-plan to run when the trigger fires (`DO <plan>`), as source text.
    pub plan: StatementSource,
    /// The attached `POLICY <name>` handle (t35): the `/server/policies` row the fired plan
    /// commits under (least privilege). `None` = no policy attached ⇒ fail-closed default-deny
    /// at fire time. Resolved against the live policy table when the trigger fires.
    #[serde(default)]
    pub policy: Option<String>,
}

/// A cron-job definition (`CREATE JOB name EVERY <interval> DO <plan>`). The t32 scheduler
/// fires `plan` every `every`; `last_run` is its persisted high-water mark (recorded by
/// t32, `None` until first fire).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct JobDef {
    /// The job name (the config row key).
    pub name: String,
    /// The cron interval, raw text (e.g. `1h`); empty if unspecified.
    pub every: String,
    /// The effect-plan to run on each fire (`DO <plan>`), as source text.
    pub plan: StatementSource,
    /// The last successful fire time as an epoch second, recorded by the t32 scheduler.
    /// `None` until the first fire (boot is replay-safe — re-applying a config preserves
    /// this only if the row carries it; a fresh INSERT leaves it `None`).
    pub last_run: Option<i64>,
    /// The attached `POLICY <name>` handle (t35): the `/server/policies` row the fired JOB plan
    /// commits under. `None` = no policy ⇒ fail-closed default-deny at fire time.
    #[serde(default)]
    pub policy: Option<String>,
}

/// A view definition (`CREATE [MATERIALIZED] VIEW name AS <query>`). A materialized view
/// is the same row with `materialized = true` (t32 refreshes it); here it is pure data.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ViewDef {
    /// The view name (the config row key).
    pub name: String,
    /// The backing query (`AS <query>`), as source text.
    pub query: StatementSource,
    /// Whether this is a `MATERIALIZED VIEW` (cached + refreshed) vs a logical view.
    pub materialized: bool,
}

/// A least-privilege policy definition (`CREATE POLICY name`). Stored now; **enforced in
/// t34** (the capability-gating engine). `allow` lists scope handles, never credentials.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct PolicyDef {
    /// The policy name (the config row key).
    pub name: String,
    /// The handler / target this policy governs (raw name), empty if unspecified.
    pub handler: String,
    /// The allowed capability scope **handles** (e.g. `mail.read`) — never tokens (§10).
    pub allow: Vec<String>,
}

/// An inbound-webhook definition (`CREATE WEBHOOK name ON '/route'`). The t33 ingestion
/// binding registers the route; here it is pure data.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct WebhookDef {
    /// The webhook name (the config row key).
    pub name: String,
    /// The inbound route, e.g. `/hooks/x`; empty if unspecified.
    pub route: String,
    /// The optional signing-secret HANDLE (t34, RFD §10) — a `qfs-secrets` account id the
    /// watchtower resolves BY HANDLE to verify the inbound HMAC signature. NEVER an inline token,
    /// NEVER logged. Empty for an unsigned (test/internal) webhook (ingest accepts without a
    /// signature check — a documented less-secure mode, signed is the production path).
    #[serde(default)]
    pub secret: String,
}

/// The running server's whole configuration — the source of truth (RFD §6/§8). Each
/// collection is a name-keyed [`BTreeMap`] so the serialized snapshot is **deterministic**
/// (golden-testable) and `UPSERT` is a stable replace-by-name. Owned data only; no vendor
/// types, no secrets. Mutated exclusively by [`crate::driver::apply_server_write`] under
/// `COMMIT` (the purity invariant — building a `/server` write plan mutates nothing).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ServerState {
    /// `/server/endpoints` — name → endpoint.
    pub endpoints: BTreeMap<String, EndpointDef>,
    /// `/server/triggers` — name → trigger.
    pub triggers: BTreeMap<String, TriggerDef>,
    /// `/server/jobs` — name → job.
    pub jobs: BTreeMap<String, JobDef>,
    /// `/server/views` — name → view.
    pub views: BTreeMap<String, ViewDef>,
    /// `/server/policies` — name → policy.
    pub policies: BTreeMap<String, PolicyDef>,
    /// `/server/webhooks` — name → webhook.
    pub webhooks: BTreeMap<String, WebhookDef>,
}

impl ServerState {
    /// An empty server configuration (the boot starting point).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The total number of config rows across every collection — the safe-to-log summary
    /// the runtime emits instead of the registry itself (RFD §10: never log verbatim).
    #[must_use]
    pub fn row_count(&self) -> usize {
        self.endpoints.len()
            + self.triggers.len()
            + self.jobs.len()
            + self.views.len()
            + self.policies.len()
            + self.webhooks.len()
    }

    /// A one-line, secret-free summary (counts per collection) — the audit/log projection
    /// of the registry. Never includes a row's contents.
    #[must_use]
    pub fn summary(&self) -> String {
        format!(
            "endpoints={} triggers={} jobs={} views={} policies={} webhooks={}",
            self.endpoints.len(),
            self.triggers.len(),
            self.jobs.len(),
            self.views.len(),
            self.policies.len(),
            self.webhooks.len(),
        )
    }
}
