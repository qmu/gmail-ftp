//! The applied-effect **audit ledger** (RFD-0001 §6 recovery substrate, §10 audit).
//!
//! The ledger is the recovery-of-record: it records an [`EffectDescriptor`]
//! **before** the driver is touched (`record_intent`) and the [`EffectReceipt`]
//! **after** a successful apply (`mark_applied`). A crash mid-saga therefore leaves a
//! durable record the next run reads via [`AuditLedger::applied`] — an already-applied
//! [`EffectKey`] makes re-apply a no-op ([`LegOutcome::AlreadyApplied`](crate::LegOutcome::AlreadyApplied)).
//!
//! ## Crash-window reconcile (t12)
//! An intent recorded with **no** matching `applied` (a crash between `record_intent` and
//! `mark_applied`) is the genuinely hard case: the side effect may or may not have landed. The
//! runtime's resume gate queries [`AuditLedger::has_intent`] for exactly this window and, for a
//! leg that is **not** replay-safe ([`EffectDescriptor::is_replay_safe`] — a non-idempotent
//! `Insert`/`Call`/`Remove` with no conditional guard), refuses a silent replay, surfacing
//! [`LegOutcome::Indeterminate`](crate::LegOutcome::Indeterminate) for `UPSERT`-style re-apply
//! or operator confirmation (RFD §6/§10 apply-once). A replay-safe leg (`UPSERT` or a
//! conditionally-guarded write) is re-applied: the driver-side dedup / `Conflict` catch makes
//! that convergent. So apply-once now holds for non-idempotent legs too, not only
//! driver-idempotent ones.
//!
//! Only the **contract** lives at E0 plus an in-memory default impl; the real **durable** sink
//! (append-only file / structured-log with fsync-intent-before-apply) is deferred to the E8
//! deployment ticket — the in-memory ledger here is process-local, so the reconcile guard is
//! exercised within a single process / test, not yet across a real OS crash. Keeping
//! [`AuditLedger`] the sole seam makes swapping the backend trivial. The ledger records
//! **metadata only** — never payloads or credentials (RFD §10): the descriptor is already
//! redacted at its boundary.

use std::collections::HashMap;
use std::sync::Mutex;

use crate::key::EffectKey;
use crate::outcome::{EffectDescriptor, EffectReceipt};

/// The audit-ledger contract (RFD §6/§10). `record_intent` is append-before-apply;
/// `mark_applied` seals it; `applied` is the idempotent-resume / dedup query.
///
/// `Send + Sync` so an `Arc<dyn AuditLedger>` can be shared across the (future) parallel
/// apply without the trait itself owning any concurrency primitive — the impl decides its
/// own interior mutability.
pub trait AuditLedger: Send + Sync {
    /// Append the intent to apply `descriptor` (keyed by `key`) **before** the driver is
    /// touched. Idempotent: recording the same key twice is harmless (the second is a
    /// no-op overwrite of identical intent).
    fn record_intent(&self, key: &EffectKey, descriptor: &EffectDescriptor);

    /// Seal the leg as applied, recording its `receipt`. Called **after** a successful
    /// driver apply, so `applied(key)` thereafter returns the receipt.
    fn mark_applied(&self, key: &EffectKey, receipt: &EffectReceipt);

    /// The receipt for `key` if it was already applied — the dedup / resume query. `None`
    /// means "never applied" (apply it); `Some` means "already done" (skip, `AlreadyApplied`).
    fn applied(&self, key: &EffectKey) -> Option<EffectReceipt>;

    /// Whether an *intent* was recorded for `key` (apply may or may not have completed) —
    /// the crash-detection query: an intent with no matching `applied` is a leg that may
    /// have partially landed and must be reconciled on resume.
    fn has_intent(&self, key: &EffectKey) -> bool;
}

/// The default **in-memory** ledger (the E0 impl; the durable sink is E8). Interior
/// mutability via a `Mutex` so it satisfies the `&self` ledger contract while being
/// `Send + Sync` for shared/parallel use. A poisoned lock degrades to a no-op / `None`
/// (lib stays panic-free) rather than unwrapping.
#[derive(Default)]
pub struct InMemoryLedger {
    intents: Mutex<HashMap<EffectKey, EffectDescriptor>>,
    applied: Mutex<HashMap<EffectKey, EffectReceipt>>,
}

impl InMemoryLedger {
    /// An empty in-memory ledger.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The number of sealed (applied) entries — a test/inspection aid.
    #[must_use]
    pub fn applied_count(&self) -> usize {
        self.applied.lock().map(|m| m.len()).unwrap_or(0)
    }

    /// The number of recorded intents — a test/inspection aid (intents ≥ applied; the
    /// difference is the set of legs that recorded intent but never sealed = crash window).
    #[must_use]
    pub fn intent_count(&self) -> usize {
        self.intents.lock().map(|m| m.len()).unwrap_or(0)
    }
}

impl AuditLedger for InMemoryLedger {
    fn record_intent(&self, key: &EffectKey, descriptor: &EffectDescriptor) {
        if let Ok(mut intents) = self.intents.lock() {
            intents.insert(key.clone(), descriptor.clone());
        }
    }

    fn mark_applied(&self, key: &EffectKey, receipt: &EffectReceipt) {
        if let Ok(mut applied) = self.applied.lock() {
            applied.insert(key.clone(), receipt.clone());
        }
    }

    fn applied(&self, key: &EffectKey) -> Option<EffectReceipt> {
        self.applied.lock().ok().and_then(|m| m.get(key).cloned())
    }

    fn has_intent(&self, key: &EffectKey) -> bool {
        self.intents
            .lock()
            .map(|m| m.contains_key(key))
            .unwrap_or(false)
    }
}
