//! [`WatchtowerBinding`] — the top-level [`qfs_server::Binding`] (kind `Ingest`) that owns the
//! event bus, the webhook routes, and the desired watcher set, and converges ALL THREE to
//! `ServerState` on every committed `/server` mutation.
//!
//! `reconcile` (sync, the t30 contract) rebuilds two atomically-swapped snapshots from a cloned
//! `ServerState`: (1) the webhook route set (delegated to the inner [`WebhookBinding`]), and (2)
//! the DESIRED watcher set (derived from the poll-source triggers). The async watcher TASKS + the
//! bus subscriber loop are spawned by the binary's serve composition root (the daemon), which reads
//! the desired-watcher snapshot — the same pattern as the qfs-http listener reading the route
//! table. Idempotent: re-reconciling the same state swaps in an equal snapshot (a no-op for the
//! daemon). The write guard is held only for the pointer swap, never across an `.await`.

use std::sync::{Arc, RwLock};

use qfs_server::{Binding, BindingKind, ServerError, ServerState, TriggerDef};

use crate::bus::EventBus;
use crate::watcher::Watcher;
use crate::webhook::{WebhookBinding, WebhookRoutes};
use qfs_secrets::Secrets;

/// The desired watcher set derived from `/server/triggers` whose `on` names a poll source (a path,
/// e.g. `/mail/inbox`). Immutable once built; the binding swaps the `Arc` pointer atomically so the
/// daemon reads a consistent snapshot. Each watcher polls its source on the trigger's cadence.
#[derive(Debug, Default)]
pub struct WatcherSet {
    /// The watchers to run (one per poll-source trigger).
    pub watchers: Vec<Watcher>,
}

impl WatcherSet {
    /// Derive the desired watcher set from the trigger registry: a trigger whose `on` is a source
    /// PATH (`/driver/...`) is a poll source. A trigger whose `on` is an event-kind label
    /// (`webhook`/`row_appended`/…) is NOT a watcher (it dispatches off the bus). The default poll
    /// interval is 60s (a `WATCH … EVERY` cadence is the carry-over; the interval util is shared
    /// with cron per the ticket).
    #[must_use]
    pub fn from_state(state: &ServerState) -> Self {
        let mut watchers = Vec::new();
        for t in state.triggers.values() {
            if t.on.starts_with('/') {
                // A poll-source trigger: watch the source path, diff on `id`, 60s cadence.
                watchers.push(Watcher::new(t.on.clone(), 60, "id"));
            }
        }
        Self { watchers }
    }

    /// The number of desired watchers (test/observability aid).
    #[must_use]
    pub fn len(&self) -> usize {
        self.watchers.len()
    }

    /// Whether the watcher set is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.watchers.is_empty()
    }
}

/// The top-level watchtower binding. Owns the bus (shared with the dispatcher + producers), the
/// inner webhook binding (which owns the route swap), and the desired-watcher swap pointer the
/// daemon reads. Constructed by the `qfs` binary's serve composition root.
pub struct WatchtowerBinding {
    webhook: WebhookBinding,
    bus: Arc<dyn EventBus>,
    watchers: Arc<RwLock<Arc<WatcherSet>>>,
    /// The live trigger set (atomically swapped on reconcile) — the binary's dispatch loop reads
    /// this to match an event off the bus without holding the runtime's state guard.
    triggers: Arc<RwLock<Arc<Vec<TriggerDef>>>>,
    /// The live `/server/policies` table (t35); the committer resolves a trigger's bound policy
    /// ref against this snapshot at fire time.
    policies: PolicyTableHandle,
}

/// The shared, atomically-swappable `/server/policies` table handle (t35) the committer resolves
/// a trigger's bound policy ref against. An alias so the composition root + binding agree.
pub type PolicyTableHandle = Arc<RwLock<Arc<qfs_server::PolicyTable>>>;

impl WatchtowerBinding {
    /// Construct over a shared secrets surface + event bus. Starts empty (boot reconciles).
    #[must_use]
    pub fn new(secrets: Arc<dyn Secrets>, bus: Arc<dyn EventBus>) -> Self {
        Self {
            webhook: WebhookBinding::new(secrets, Arc::clone(&bus)),
            bus,
            watchers: Arc::new(RwLock::new(Arc::new(WatcherSet::default()))),
            triggers: Arc::new(RwLock::new(Arc::new(Vec::new()))),
            policies: Arc::new(RwLock::new(Arc::new(qfs_server::PolicyTable::new()))),
        }
    }

    /// A shared handle to the live trigger set (the dispatch loop reads this).
    #[must_use]
    pub fn triggers_handle(&self) -> Arc<RwLock<Arc<Vec<TriggerDef>>>> {
        Arc::clone(&self.triggers)
    }

    /// A shared handle to the live `/server/policies` table (the committer's fire-time policy
    /// resolution reads this, t35).
    #[must_use]
    pub fn policies_handle(&self) -> PolicyTableHandle {
        Arc::clone(&self.policies)
    }

    /// Snapshot the current live trigger set.
    #[must_use]
    pub fn current_triggers(&self) -> Arc<Vec<TriggerDef>> {
        self.triggers
            .read()
            .map(|g| Arc::clone(&g))
            .unwrap_or_else(|_| Arc::new(Vec::new()))
    }

    /// The shared event bus (so the binary's subscriber loop + the dispatcher share it).
    #[must_use]
    pub fn bus(&self) -> Arc<dyn EventBus> {
        Arc::clone(&self.bus)
    }

    /// The inner webhook binding (so the binary wires its `ingest` into the HTTP listener).
    #[must_use]
    pub fn webhook(&self) -> &WebhookBinding {
        &self.webhook
    }

    /// The shared webhook ingest core (the binary clones this into the HTTP fallback closure that
    /// routes `/hooks/...` to `ingest` — option b: qfs-watchtower serves no HTTP itself).
    #[must_use]
    pub fn ingest_core(&self) -> std::sync::Arc<crate::webhook::WebhookIngest> {
        self.webhook.ingest_core()
    }

    /// A shared handle to the live webhook route set.
    #[must_use]
    pub fn routes_handle(&self) -> Arc<RwLock<Arc<WebhookRoutes>>> {
        self.webhook.routes_handle()
    }

    /// A shared handle to the desired-watcher set (the daemon reads this to spawn/cancel watchers).
    #[must_use]
    pub fn watchers_handle(&self) -> Arc<RwLock<Arc<WatcherSet>>> {
        Arc::clone(&self.watchers)
    }

    /// Snapshot the current desired-watcher set (clones the `Arc`; guard dropped immediately).
    #[must_use]
    pub fn current_watchers(&self) -> Arc<WatcherSet> {
        self.watchers
            .read()
            .map(|g| Arc::clone(&g))
            .unwrap_or_else(|_| Arc::new(WatcherSet::default()))
    }
}

impl Binding for WatchtowerBinding {
    fn kind(&self) -> BindingKind {
        BindingKind::Ingest
    }

    fn reconcile(&mut self, state: &ServerState) -> Result<(), ServerError> {
        // (1) Webhook routes: delegate to the inner binding (its own atomic swap).
        self.webhook.reconcile(state)?;
        // (2) Desired watcher set: rebuild from the poll-source triggers + atomic swap. The daemon
        //     reads this snapshot to spawn/cancel watcher tasks; re-reconciling equal state swaps
        //     an equal snapshot (idempotent — a no-op for the daemon).
        let new_watchers = Arc::new(WatcherSet::from_state(state));
        let count = new_watchers.len();
        if let Ok(mut guard) = self.watchers.write() {
            *guard = new_watchers;
        } else {
            return Err(ServerError::Reconcile {
                kind: BindingKind::Ingest.label().to_string(),
                reason: "watchtower watcher set lock poisoned".to_string(),
            });
        }
        // (3) Live trigger set: the dispatch loop matches an event off the bus against this.
        let new_triggers = Arc::new(state.triggers.values().cloned().collect::<Vec<_>>());
        if let Ok(mut guard) = self.triggers.write() {
            *guard = new_triggers;
        } else {
            return Err(ServerError::Reconcile {
                kind: BindingKind::Ingest.label().to_string(),
                reason: "watchtower trigger set lock poisoned".to_string(),
            });
        }
        // (4) Live policy table (t35): the committer resolves a trigger's bound policy ref
        //     against this snapshot — a hot POLICY change is visible to the next fire.
        if let Ok(mut guard) = self.policies.write() {
            *guard = Arc::new(state.policies.clone());
        }
        tracing::info!(
            target: "qfs::watchtower",
            watchers = count,
            triggers = state.triggers.len(),
            "watchtower reconciled (webhook routes + watcher set + trigger set)"
        );
        Ok(())
    }
}
