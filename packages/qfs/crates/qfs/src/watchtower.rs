//! The `qfs serve` **watchtower composition** (t34): the binary wires the watchtower binding.
//!
//! Like the HTTP + cron bindings, `qfs-watchtower` is a LEAF that consumes `qfs-server` (the
//! registry + reconcile seam) AND `qfs-exec` (the build_plan committer) — composing it HERE (the
//! terminal binary) keeps `qfs-cmd` off it and lets its feature-gated tokio (the LocalBus MPSC +
//! the dispatch loop) dead-end in the sink. The binary:
//!   * builds the shared [`qfs_watchtower::LocalBus`] + the [`qfs_watchtower::WatchtowerBinding`]
//!     (reconciled by the runtime from `/server/{webhooks,triggers}`),
//!   * builds a [`WatchtowerCommitter`] that lowers a fired trigger's plan via `qfs_exec::build_plan`
//!     and commits it (the PREVIEW path — the live-driver applier is the same parked carry-over as
//!     cron/HTTP),
//!   * spawns a **dispatch loop** that drains the bus, matches each event against the live trigger
//!     set, gates the WHERE, binds NEW.*, commits, and acks ONLY on success (at-least-once), and
//!   * builds the HTTP **fallback closure** that routes `/hooks/...` to the webhook `ingest` (so
//!     `qfs-http` gains no dependency on `qfs-watchtower` — option b of the t34 serve decision).
//!
//! ## Parked wiring (carry-over, recorded honestly)
//!   * **The live-driver applier**: the committer here builds the plan and applies it through a
//!     `RecordingApplier` (the PREVIEW path) — the SAME state cron/HTTP are in at this stage. The
//!     live-effect COMMIT through the runtime `Interpreter` is the E2/E4 carry-over (t35/t38).
//!   * **Durable bus + cursor + dedup ledger**: the LocalBus spool, the watcher cursors, and the
//!     dispatcher's idempotency ledger are in-memory in this binary; persisting them via `/server`
//!     (EC2) or Durable Objects + CF Queues (CF) is the deferred deployment detail (E7/t35).
//!   * **Watcher task spawning**: the reconciled watcher SET is exposed (the daemon would spawn one
//!     poll task per watcher); wiring the live poll loop against real read drivers is parked with
//!     the cron daemon's live-driver carry-over (an unregistered source is a structured error,
//!     never a panic).

use std::sync::Arc;

use qfs_core::Engine;
use qfs_secrets::Secrets;
use qfs_watchtower::{
    AllowAllGate, AuditSink, Committer, Dispatcher, FireError, FireOutcome, LocalBus, TriggerDef,
    WatchtowerBinding, WebhookIngest,
};

/// The committer the binary injects into the dispatch loop: builds a fired trigger's plan via
/// `qfs_exec::build_plan` over the serve engine, enforces the trigger's bound POLICY against it
/// (t35, default-deny / atomic abort), and commits it (PREVIEW path). Reuses qfs-watchtower's own
/// `RecordingCommitter` shape — the binary supplies the serve engine + the live policy table.
pub struct WatchtowerCommitter {
    inner: qfs_watchtower::RecordingCommitter,
}

impl WatchtowerCommitter {
    /// Build a committer over a clone of the serve engine's registries (mounts + codecs) wired
    /// with the live `/server/policies` table the trigger's bound policy ref resolves against.
    #[must_use]
    pub fn new(engine: Engine, policies: qfs_watchtower::PolicyTableHandle) -> Self {
        Self {
            inner: qfs_watchtower::RecordingCommitter::with_engine(engine).with_policies(policies),
        }
    }
}

impl Committer for WatchtowerCommitter {
    fn commit(
        &self,
        trigger: &str,
        stmt: &qfs_watchtower::Statement,
        policy: Option<&str>,
    ) -> Result<FireOutcome, FireError> {
        self.inner.commit(trigger, stmt, policy)
    }
}

/// Build the watchtower binding + the shared bus + the HTTP fallback closure. Returns the binding
/// (to register into the runtime so it reconciles), the bus receiver (for the dispatch loop), the
/// shared bus handle, the live-trigger handle, and the fallback the HTTP listener invokes for
/// `/hooks/...`.
///
/// The bus capacity is bounded (back-pressure + a finite redelivery spool); 1024 is ample for the
/// loopback serve contract.
#[must_use]
pub fn build_watchtower(
    secrets: Arc<dyn Secrets>,
) -> (
    WatchtowerBinding,
    tokio::sync::mpsc::Receiver<qfs_watchtower::Event>,
    Arc<dyn qfs_watchtower::EventBus>,
    qfs_http::Fallback,
    qfs_watchtower::PolicyTableHandle,
) {
    let (local_bus, rx) = LocalBus::new(1024);
    let bus: Arc<dyn qfs_watchtower::EventBus> = Arc::new(local_bus);
    let binding = WatchtowerBinding::new(secrets, Arc::clone(&bus));
    let policies = binding.policies_handle();

    // The HTTP fallback: route `/hooks/...` to the shared webhook ingest core. The closure holds an
    // Arc clone of the SAME WebhookIngest the binding reconciles, so the listener + the runtime
    // share one route set + bus. A non-/hooks path returns `None` (the listener 404s as usual).
    let ingest: Arc<WebhookIngest> = binding.ingest_core();
    let fallback: qfs_http::Fallback = Arc::new(move |req: &qfs_http::HttpRequest| {
        if !req.path.starts_with("/hooks/") {
            return None;
        }
        let now = now_secs();
        let out = ingest.ingest(&req.path, &req.headers, &req.body, now);
        let body = format!(
            r#"{{"status":{},"published":{}}}"#,
            out.status, out.published
        );
        Some(qfs_http::HttpResponse::new(
            out.status,
            "application/json",
            body.into_bytes(),
        ))
    });

    (binding, rx, bus, fallback, policies)
}

/// Spawn the dispatch loop: drain the bus receiver, match each event against the live trigger set,
/// dispatch (WHERE gate → NEW.* bind → policy gate → COMMIT), and ack ONLY on a successful commit
/// (at-least-once; a commit failure leaves the event un-acked in the spool for redelivery). Runs
/// until the channel closes (the bus is dropped on shutdown).
pub fn spawn_dispatch_loop(
    mut rx: tokio::sync::mpsc::Receiver<qfs_watchtower::Event>,
    bus: Arc<dyn qfs_watchtower::EventBus>,
    triggers: Arc<std::sync::RwLock<Arc<Vec<TriggerDef>>>>,
    audit: Arc<AuditSink>,
    engine: Engine,
    policies: qfs_watchtower::PolicyTableHandle,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let committer = WatchtowerCommitter::new(engine, policies);
        let dispatcher = Dispatcher::new(AllowAllGate);
        while let Some(event) = rx.recv().await {
            let snapshot = triggers
                .read()
                .map(|g| Arc::clone(&g))
                .unwrap_or_else(|_| Arc::new(Vec::new()));
            match dispatcher.handle(&event, &snapshot, &committer, &audit) {
                Ok(outcome) => {
                    if outcome.should_ack() {
                        let _ = bus.ack(&event.id);
                    }
                }
                Err(e) => {
                    // A COMMIT failure: do NOT ack — the event stays in the spool for redelivery
                    // (at-least-once). Log the secret-free reason.
                    tracing::warn!(
                        target: "qfs::watchtower",
                        event = %event.id.as_str(),
                        error = %e,
                        "dispatch failed; leaving event un-acked for redelivery"
                    );
                }
            }
        }
    })
}

/// The current epoch second (the receipt clock the ingest/dispatch stamp events with).
fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
