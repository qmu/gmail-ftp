//! The [`HttpBinding`] (t32): the [`qfs_server::Binding`] (kind `Http`) that reconciles the
//! `/server/endpoints` registry into a live, hot-swapped route table.
//!
//! ## Reconcile = converge + atomic swap (the t30 rule)
//! `reconcile(&state)` is **synchronous** (the t30 / CO-t30-1 contract): it is handed an owned
//! [`qfs_server::ServerState`] snapshot and rebuilds a fresh [`Router`] from
//! `state.endpoints`, then swaps a shared `Arc<Router>` pointer atomically. The async request
//! handler reads that pointer by cloning the `Arc` (a brief lock, never held across an
//! `.await`) — so a `/server/endpoints` mutation re-binds the live routes on the very next
//! request with zero downtime and no torn state. A write-lowering endpoint is REFUSED at this
//! reconcile (the registration-time policy gate); a refused endpoint is skipped (it never
//! becomes a route) and logged, so one bad row does not tear down the table.
//!
//! ## arc-swap without an external crate
//! `arc-swap` is not in the offline cache; a `std::sync::RwLock<Arc<Router>>` gives the same
//! observable semantics — readers clone the `Arc` under a momentary read guard and drop it
//! immediately, the reconcile takes the write guard for the pointer assignment only. The
//! router itself is immutable once built, so a request holds a consistent snapshot for its
//! whole lifetime even as a concurrent reconcile swaps in a new one.

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

use qfs_core::Engine;
use qfs_exec::ReadRegistry;
use qfs_server::{Binding, BindingKind, PolicyDef, ServerError, ServerState};

use crate::handler::EndpointCtx;
use crate::route::{compile_endpoint, Router};

/// The HTTP serving binding. Holds the engine (mounts + codecs), the read-driver registry, the
/// bounded result cap, and the atomically swappable route table. Constructed by the `qfs`
/// binary's serve composition root and registered into the [`qfs_server::Runtime`].
pub struct HttpBinding {
    engine: Arc<Engine>,
    reads: Arc<ReadRegistry>,
    max_rows: usize,
    /// The atomically swappable route table (the arc-swap pointer; see the module doc).
    router: Arc<RwLock<Arc<Router>>>,
    /// The resolved policies from the last reconcile (the endpoint policy-handle lookup).
    policies: Arc<RwLock<Arc<BTreeMap<String, PolicyDef>>>>,
}

impl HttpBinding {
    /// Construct a binding over a shared engine + read registry, with the given result cap.
    /// Starts with an empty router (boot reconciles it to the registry).
    #[must_use]
    pub fn new(engine: Arc<Engine>, reads: Arc<ReadRegistry>, max_rows: usize) -> Self {
        Self {
            engine,
            reads,
            max_rows,
            router: Arc::new(RwLock::new(Arc::new(Router::new()))),
            policies: Arc::new(RwLock::new(Arc::new(BTreeMap::new()))),
        }
    }

    /// A shared handle to the live route table, for the async listener / request dispatch.
    /// Reading it clones the inner `Arc<Router>` under a momentary guard (never across await).
    #[must_use]
    pub fn router_handle(&self) -> Arc<RwLock<Arc<Router>>> {
        Arc::clone(&self.router)
    }

    /// Snapshot the current live router (clones the `Arc`; the guard is dropped immediately).
    #[must_use]
    pub fn current_router(&self) -> Arc<Router> {
        self.router
            .read()
            .map(|g| Arc::clone(&g))
            .unwrap_or_else(|_| Arc::new(Router::new()))
    }

    /// A shared handle to the live policy table (refreshed on every reconcile). Used to build a
    /// request context that tracks hot policy changes.
    #[must_use]
    pub fn policies_handle(&self) -> Arc<RwLock<Arc<BTreeMap<String, PolicyDef>>>> {
        Arc::clone(&self.policies)
    }

    /// Build the per-request [`EndpointCtx`] from the binding's shared state. The context holds
    /// the LIVE policy handle, so a hot reconcile's policy change is visible to the next request.
    #[must_use]
    pub fn ctx(&self) -> EndpointCtx {
        EndpointCtx::new(
            Arc::clone(&self.engine),
            Arc::clone(&self.reads),
            Arc::clone(&self.policies),
            self.max_rows,
        )
    }

    /// The bounded result-row cap.
    #[must_use]
    pub fn max_rows(&self) -> usize {
        self.max_rows
    }
}

impl Binding for HttpBinding {
    fn kind(&self) -> BindingKind {
        BindingKind::Http
    }

    fn reconcile(&mut self, state: &ServerState) -> Result<(), ServerError> {
        // Resolve the policy table FIRST so the registration gate can read an endpoint's
        // granting policy (a clone — the snapshot is owned, no lock held across this work).
        let policies: BTreeMap<String, PolicyDef> = state.policies.clone();

        // Rebuild the route table from the owned snapshot. A write-lowering / malformed
        // endpoint is REFUSED (the registration-time gate) — it is skipped + logged so one bad
        // row never tears down the whole table.
        let mut routes = Vec::with_capacity(state.endpoints.len());
        for def in state.endpoints.values() {
            let policy = def.policy.as_deref().and_then(|h| policies.get(h));
            match compile_endpoint(def, &self.engine, policy) {
                Ok(route) => routes.push(route),
                Err(err) => {
                    // Refused / malformed: do NOT register the route. Log the class + endpoint
                    // name only (no query text, no secrets — RFD §10).
                    tracing::warn!(
                        target: "qfs::http",
                        endpoint = %def.name,
                        reason = %err,
                        "endpoint not registered (policy/compile refusal)"
                    );
                }
            }
        }

        let new_router = Arc::new(Router::from_routes(routes));
        let route_count = new_router.len();

        // Atomic swap (the arc-swap pointer): take the write guard for the assignment only.
        if let Ok(mut guard) = self.router.write() {
            *guard = new_router;
        } else {
            return Err(ServerError::Reconcile {
                kind: BindingKind::Http.label().to_string(),
                reason: "http route table lock poisoned".to_string(),
            });
        }
        if let Ok(mut guard) = self.policies.write() {
            *guard = Arc::new(policies);
        }

        tracing::info!(
            target: "qfs::http",
            routes = route_count,
            endpoints = state.endpoints.len(),
            "http route table reconciled"
        );
        Ok(())
    }
}
