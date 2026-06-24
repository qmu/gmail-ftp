//! Conversion from the live t30 `qfs_server::ServerState` into the owned, host-agnostic
//! [`crate::BindingSet`] (behind `host-daemon`).
//!
//! `qfs-server` pulls tokio `signal` (no-wasm), so this conversion is gated to the EC2/daemon
//! feature. The wasm-clean Worker side derives its binding set from owned [`crate::DerivationInput`]
//! built by the (parked) Worker entrypoint from the config it is deployed with — it never links
//! `qfs-server`. This is the one place the registry DTOs are projected into the deployment DTOs;
//! both hosts consume the [`crate::BindingSet`] identically afterward.

use qfs_server::{ServerState, StatementSource};

use crate::derive::{
    derive_bindings, DerivationInput, EndpointInput, JobInput, WatcherInput, WebhookInput,
};
use crate::dto::BindingSet;

/// Project a `ServerState` snapshot into the host-agnostic [`DerivationInput`] (owned strings
/// only). The plan/query/predicate sources are carried through so the native-store scanner can
/// find `/cf/d1`·`/cf/r2`·`/cf/kv` references in them.
#[must_use]
pub fn derivation_input(state: &ServerState) -> DerivationInput {
    let endpoints = state
        .endpoints
        .values()
        .map(|e| EndpointInput {
            name: e.name.clone(),
            method: e.method.clone(),
            route: e.route.clone(),
            policy: e.policy.clone(),
            query_source: source_text(&e.query),
        })
        .collect();
    let jobs = state
        .jobs
        .values()
        .map(|j| JobInput {
            name: j.name.clone(),
            every: j.every.clone(),
            policy: j.policy.clone(),
            plan_source: source_text(&j.plan),
        })
        .collect();
    let webhooks = state
        .webhooks
        .values()
        .map(|w| WebhookInput {
            name: w.name.clone(),
            route: w.route.clone(),
            secret_handle: w.secret.clone(),
        })
        .collect();
    let watchers = state
        .triggers
        .values()
        .map(|t| WatcherInput {
            name: t.name.clone(),
            on: t.on.clone(),
            policy: t.policy.clone(),
            plan_source: source_text(&t.plan),
            predicate_source: source_text(&t.predicate),
        })
        .collect();
    let view_sources = state
        .views
        .values()
        .map(|v| source_text(&v.query))
        .collect();
    DerivationInput {
        endpoints,
        jobs,
        webhooks,
        watchers,
        view_sources,
    }
}

/// Derive the [`BindingSet`] directly from a `ServerState` (the daemon's one-call path).
#[must_use]
pub fn bindings_from_state(state: &ServerState) -> BindingSet {
    derive_bindings(&derivation_input(state))
}

/// Boot a `.qfs` config file into a `ServerState` (the in-memory parse→lower→COMMIT path) and
/// derive its [`BindingSet`]. The daemon composition root calls this so the terminal binary never
/// needs a direct `qfs-server` dependency (it stays the thin entrypoint the dep-direction guard
/// pins); the `qfs-server` coupling lives behind `qfs-host`'s `host-daemon` feature.
///
/// # Errors
/// A secret-free, line-located error string on any read / parse / lower / commit failure.
pub fn bindings_from_config(config: &std::path::Path) -> Result<BindingSet, String> {
    let mut rt = qfs_server::Runtime::new();
    rt.boot(config).map_err(|e| format!("boot: {e}"))?;
    Ok(bindings_from_state(&rt.snapshot()))
}

/// Read a `StatementSource`'s owned text (the scanner input). The stored body may be a canonical
/// serialized spec (t31) or raw source; either way the native-store mount paths appear in it as
/// literal `/cf/...` substrings, which the token scanner picks up.
fn source_text(src: &StatementSource) -> String {
    src.as_str().to_string()
}
