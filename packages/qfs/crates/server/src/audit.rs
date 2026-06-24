//! The server audit ledger (RFD-0001 §6/§10): an append-only record of every `/server`
//! mutation (who/op/node/before-after) and every fired plan. Owned, **secret-free** data —
//! it records *names* and *ops*, never a row's credential-bearing contents (RFD §10).
//!
//! The ledger is the applied-effect record the operator (and a future recovery pass) reads.
//! It is drained on shutdown (`Runtime::run` flushes it on `ctrl_c`).

use std::sync::Mutex;

use crate::driver::ConfigChange;

/// One audit entry. `#[non_exhaustive]` so the entry shape can grow (a `who` principal lands
/// with the t34 policy engine) without a breaking change.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum AuditEntry {
    /// A committed `/server` config mutation.
    ConfigWrite {
        /// Who performed it (the principal/source; `"boot"` during boot, request-derived
        /// later). A label, never a credential.
        who: String,
        /// The `/server/<node>` collection mutated.
        node: &'static str,
        /// The write op label (`INSERT`/`UPSERT`/`UPDATE`/`REMOVE`).
        op: &'static str,
        /// The affected row name (the config key).
        name: Option<String>,
        /// Whether the row existed before the apply (the "before" half of before/after).
        existed_before: bool,
        /// Whether the row exists after the apply (the "after" half).
        exists_after: bool,
    },
    /// A fired plan (a job / trigger / endpoint plan executed by a binding). Recorded by
    /// E7 bindings; the shape is reserved here so the ledger is the one funnel (RFD §6).
    PlanFired {
        /// What fired it (`"job:nightly"`, `"endpoint:recent"`, …).
        cause: String,
    },
    /// A policy-evaluated fired plan (t35): the full [`crate::policy::FiredPlanRecord`] — handler,
    /// bound policy, allow/deny decision (deny carries verb/driver/rule), secret-free per-effect
    /// summaries, ts. Emitted for EVERY fired plan (allow AND deny) so the ledger is the single
    /// unattended-execution funnel (RFD §6/§10).
    FiredPlan(crate::policy::FiredPlanRecord),
}

impl AuditEntry {
    /// Build a config-write entry from an applied [`ConfigChange`] and the acting principal.
    #[must_use]
    pub fn from_change(who: impl Into<String>, change: &ConfigChange) -> Self {
        AuditEntry::ConfigWrite {
            who: who.into(),
            node: change.node.segment(),
            op: change.op.label(),
            name: change.name.clone(),
            existed_before: change.existed_before,
            exists_after: change.exists_after,
        }
    }

    /// A one-line, secret-free rendering for the drain log / operator output.
    #[must_use]
    pub fn summary(&self) -> String {
        match self {
            AuditEntry::ConfigWrite {
                who,
                node,
                op,
                name,
                existed_before,
                exists_after,
            } => format!(
                "{who} {op} /server/{node} name={} before={existed_before} after={exists_after}",
                name.as_deref().unwrap_or("-")
            ),
            AuditEntry::PlanFired { cause } => format!("fired {cause}"),
            AuditEntry::FiredPlan(r) => r.summary(),
        }
    }
}

/// The append-only audit sink. Thread-safe (a `Mutex` over the entry log) so the run loop
/// and (future) inbound bindings can both record. Never holds the lock across an `.await`
/// (each `record` is a short critical section).
#[derive(Debug, Default)]
pub struct AuditSink {
    entries: Mutex<Vec<AuditEntry>>,
}

impl AuditSink {
    /// A fresh, empty ledger.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Append one entry. A poisoned lock degrades to a dropped record (the audit never
    /// breaks the operation, RFD §6) rather than a panic — the lib stays panic-free.
    pub fn record(&self, entry: AuditEntry) {
        if let Ok(mut log) = self.entries.lock() {
            log.push(entry);
        }
    }

    /// Append one t35 fired-plan record (the policy-evaluated fire — allow or deny). The single
    /// funnel every E7 committer calls so no plan fires unaudited (RFD §6/§10).
    pub fn record_fired(&self, record: crate::policy::FiredPlanRecord) {
        self.record(AuditEntry::FiredPlan(record));
    }

    /// The number of recorded [`AuditEntry::FiredPlan`] entries — the t35 fired-plan count
    /// (test/observability aid; one per evaluated handler plan).
    #[must_use]
    pub fn fired_count(&self) -> usize {
        self.entries
            .lock()
            .map(|l| {
                l.iter()
                    .filter(|e| matches!(e, AuditEntry::FiredPlan(_)))
                    .count()
            })
            .unwrap_or(0)
    }

    /// The number of recorded entries (test/observability aid).
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.lock().map(|l| l.len()).unwrap_or(0)
    }

    /// Whether the ledger is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Take a snapshot of all entries (for draining on shutdown / assertions).
    #[must_use]
    pub fn snapshot(&self) -> Vec<AuditEntry> {
        self.entries.lock().map(|l| l.clone()).unwrap_or_default()
    }

    /// Drain the ledger to the trace log on shutdown, returning how many entries were
    /// flushed. Secret-free (each entry's `summary` is names + ops only).
    pub fn drain(&self) -> usize {
        let entries = self.snapshot();
        for entry in &entries {
            tracing::info!(target: "qfs::server::audit", "{}", entry.summary());
        }
        entries.len()
    }
}
