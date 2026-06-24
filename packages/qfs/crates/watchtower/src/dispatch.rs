//! [`Dispatcher`] — match an [`Event`] against the registered triggers, gate it through the
//! optional `WHERE NEW.*` predicate, bind `NEW.*` into the handler plan, pass the policy gate hook,
//! and `COMMIT` through the injected [`Committer`] — acking ONLY after a successful commit.
//!
//! ## At-least-once + idempotency (the hard part)
//! Delivery is at-least-once: the bus redelivers an un-acked event after a crash. Correctness comes
//! from idempotent handlers (`UPSERT`/`@version`) PLUS the [`Event::dedup_key`] the dispatcher
//! tracks in an **idempotency ledger** ([`Dispatcher::seen`]): the FIRST successful commit of a
//! dedup_key records it; a re-delivered event with the same dedup_key is a NO-OP (acked without a
//! second commit). So delivering the same Event twice yields ONE net effect.
//!
//! NOTE: a non-idempotent proc (`CALL mail.send`) still needs an explicit dedupe guard in the
//! plan — the ledger collapses *redelivery* of the same dedup_key, but two genuinely-distinct
//! events (distinct dedup_keys) each fire once, as intended.
//!
//! ## Purity
//! `handle` builds the bound handler `Statement` and evaluates the WHERE guard with NO I/O and NO
//! mutation; the ONLY effect is the injected `Committer::commit` (the COMMIT boundary). A failing
//! guard / a policy denial / a build error fires NOTHING (zero commit, zero driver call).

use std::collections::HashSet;
use std::sync::Mutex;

use qfs_core::{Row, StatementSpec};
use qfs_server::{AuditSink, FiredDecision, FiredPlanRecord, TriggerDef};

use crate::bind::{bind_new, NewBindings};
use crate::commit::{Committer, FireError, FireOutcome, PolicyGate};
use crate::event::Event;
use crate::predicate::guard_matches;

/// The result of dispatching one event (for tests + the bus ack decision).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Dispatched {
    /// One or more handlers fired and committed; the event may be acked. Carries the count of
    /// committed plans (for the audit / observability assertion).
    Fired(usize),
    /// The event was a redelivery of an already-committed dedup_key — a no-op; ack it.
    Duplicate,
    /// No registered trigger matched the event (ack it; nothing to do).
    NoMatch,
    /// Every matching handler was gated out (WHERE failed / policy denied) — zero fires; ack it.
    Gated,
}

impl Dispatched {
    /// Whether the event should be acked (every terminal outcome acks; a commit FAILURE returns an
    /// `Err` from `handle`, NOT a `Dispatched`, so it is NOT acked and is redelivered).
    #[must_use]
    pub fn should_ack(&self) -> bool {
        true
    }
}

/// The pure dispatch core. Holds the policy gate + the idempotency ledger; the trigger set is
/// passed per-call (a cloned `ServerState` snapshot, never a held lock). The committer + audit are
/// passed by reference so the same dispatcher serves many events.
pub struct Dispatcher<G: PolicyGate> {
    gate: G,
    /// The idempotency ledger: dedup_keys that have COMMITTED at least once. A redelivery of a
    /// recorded key is a no-op (the at-least-once → effectively-once collapse for idempotent
    /// handlers). In-memory now; a durable store is the E7/DO carry-over.
    seen: Mutex<HashSet<String>>,
}

impl<G: PolicyGate> Dispatcher<G> {
    /// Construct a dispatcher over a policy gate (the t34 default is [`crate::AllowAllGate`]).
    #[must_use]
    pub fn new(gate: G) -> Self {
        Self {
            gate,
            seen: Mutex::new(HashSet::new()),
        }
    }

    /// Whether `dedup_key` has already committed (test/observability aid).
    #[must_use]
    pub fn has_seen(&self, dedup_key: &str) -> bool {
        self.seen
            .lock()
            .map(|s| s.contains(dedup_key))
            .unwrap_or(false)
    }

    /// Dispatch `event` against `triggers`, committing every matching, gated-in handler through
    /// `committer` and recording one audit entry per fire. Acks (returns `Ok(Dispatched)`) on any
    /// terminal outcome; returns `Err(FireError)` on a COMMIT failure so the caller does NOT ack
    /// (the event is redelivered — at-least-once).
    ///
    /// # Errors
    /// [`FireError`] if a matching handler's plan build or commit FAILS (the event stays un-acked).
    pub fn handle(
        &self,
        event: &Event,
        triggers: &[TriggerDef],
        committer: &dyn Committer,
        audit: &AuditSink,
    ) -> Result<Dispatched, FireError> {
        // Idempotency ledger: a redelivery of an already-committed dedup_key is a no-op. Checked
        // FIRST so a redelivered event never re-runs a (possibly non-idempotent) handler.
        if self.has_seen(&event.dedup_key) {
            tracing::debug!(
                target: "qfs::watchtower",
                dedup_key = %event.dedup_key,
                "duplicate event (already committed); acking without re-fire"
            );
            return Ok(Dispatched::Duplicate);
        }

        // The NEW.* binding env + the column names (carried with the event payload). Built ONCE.
        let columns = event.columns.clone();
        let binds = NewBindings::from_row(&columns, &event.new.values);

        let matched: Vec<&TriggerDef> = triggers
            .iter()
            .filter(|t| t.on == event.source.as_str() || t.on == event.kind.label())
            .collect();
        if matched.is_empty() {
            return Ok(Dispatched::NoMatch);
        }

        let mut fired = 0usize;
        for trigger in matched {
            // 1. WHERE gating over NEW.* (a failing / malformed guard fires NOTHING).
            if !self.passes_guard(trigger, &columns, &event.new) {
                continue;
            }
            // 2. Build the NEW.*-bound handler statement (pure — no I/O, no commit yet).
            let bound = match self.bound_handler(trigger, &binds) {
                Some(stmt) => stmt,
                None => {
                    // A declared-but-empty / un-rehydratable handler fires nothing (logged).
                    tracing::warn!(
                        target: "qfs::watchtower",
                        trigger = %trigger.name,
                        "trigger handler is empty or could not rehydrate; skipping"
                    );
                    continue;
                }
            };
            // 3. Statement-level gate hook (a no-op pass-through in the live composition; the
            //    load-bearing plan-level POLICY engine is enforced in the committer, t35).
            if let Err(reason) = self.gate.check(&trigger.name, &bound) {
                tracing::warn!(
                    target: "qfs::watchtower",
                    trigger = %trigger.name,
                    %reason,
                    "statement-level gate denied fire"
                );
                continue;
            }
            // 4. COMMIT through the injected committer, threading the trigger's bound POLICY ref
            //    (t35): the committer resolves it, runs the pure enforcer over the built plan,
            //    emits the ONE FiredPlanRecord (allow + deny), and aborts atomically on deny
            //    (ZERO effects). A POLICY DENIAL fires nothing (Gated, the event is acked — it is
            //    a permanent decision, not a transient failure to retry). A BUILD/APPLY failure
            //    bubbles up so the event is NOT acked (redelivered — at-least-once).
            match committer.commit(&trigger.name, &bound, trigger.policy.as_deref()) {
                Ok(outcome) => {
                    self.record_allow(trigger, &outcome, audit);
                    fired += 1;
                }
                Err(FireError::PolicyDenied {
                    reason,
                    verb,
                    driver,
                    rule,
                    effects,
                }) => {
                    // A POLICY denial: emit the ONE deny fired-plan record, fire nothing. This is
                    // a terminal decision (ack), not a redeliverable failure.
                    self.record_deny(trigger, verb, driver, rule, effects, audit);
                    tracing::warn!(
                        target: "qfs::watchtower",
                        trigger = %trigger.name,
                        %reason,
                        "policy denied fire (atomic abort, zero effects)"
                    );
                    continue;
                }
                Err(e) => return Err(e),
            }
        }

        // Advance the idempotency ledger only AFTER all matching handlers committed (so a partial
        // commit failure above redelivers and re-attempts — at-least-once, the ledger is the
        // effectively-once collapse for the SUCCESSFUL path).
        if fired > 0 {
            if let Ok(mut seen) = self.seen.lock() {
                seen.insert(event.dedup_key.clone());
            }
            Ok(Dispatched::Fired(fired))
        } else {
            // Matched but every handler gated out (WHERE failed / policy denied / empty).
            Ok(Dispatched::Gated)
        }
    }

    /// Evaluate the trigger's optional `WHERE` guard over `NEW.*`. No guard ⇒ always fire. A
    /// malformed guard ⇒ fail-closed (does NOT fire) + logged.
    fn passes_guard(&self, trigger: &TriggerDef, columns: &[String], new: &Row) -> bool {
        let canonical = trigger.predicate.as_str();
        if canonical.is_empty() {
            return true; // no guard
        }
        let spec = match StatementSpec::from_canonical(canonical) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(
                    target: "qfs::watchtower",
                    trigger = %trigger.name,
                    error = %e,
                    "trigger WHERE guard failed to rehydrate; not firing (fail-closed)"
                );
                return false;
            }
        };
        match guard_matches(spec.statement(), columns, new) {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(
                    target: "qfs::watchtower",
                    trigger = %trigger.name,
                    error = %e,
                    "trigger WHERE guard could not be evaluated; not firing (fail-closed)"
                );
                false
            }
        }
    }

    /// Rehydrate the trigger's handler plan body and bind `NEW.*` into it (pure). `None` if the
    /// handler is empty or could not rehydrate.
    fn bound_handler(
        &self,
        trigger: &TriggerDef,
        binds: &NewBindings,
    ) -> Option<qfs_parser::Statement> {
        let canonical = trigger.plan.as_str();
        if canonical.is_empty() {
            return None;
        }
        let spec = qfs_core::PlanSpec::from_canonical(canonical).ok()?;
        let mut stmt = spec.statement().clone();
        bind_new(&mut stmt, binds);
        Some(stmt)
    }

    /// Record the ONE allow fired-plan record per committed plan (t35). Secret-free: handler +
    /// policy + effect summaries (driver + path + verb), never a payload.
    fn record_allow(&self, trigger: &TriggerDef, outcome: &FireOutcome, audit: &AuditSink) {
        audit.record_fired(FiredPlanRecord {
            handler: format!("trigger:{}", trigger.name),
            policy: trigger.policy.clone().unwrap_or_default(),
            decision: FiredDecision::Allow,
            effects: outcome.effects.clone(),
            ts: now_secs(),
        });
        tracing::info!(
            target: "qfs::watchtower",
            trigger = %trigger.name,
            affected = outcome.affected,
            "trigger fired and committed (policy allowed)"
        );
    }

    /// Record the ONE deny fired-plan record (t35): the offending verb/driver/rule index +
    /// the (aborted) plan's effect summaries. ZERO effects were applied.
    fn record_deny(
        &self,
        trigger: &TriggerDef,
        verb: String,
        driver: String,
        rule: Option<usize>,
        effects: Vec<String>,
        audit: &AuditSink,
    ) {
        audit.record_fired(FiredPlanRecord {
            handler: format!("trigger:{}", trigger.name),
            policy: trigger.policy.clone().unwrap_or_default(),
            decision: FiredDecision::Deny { verb, driver, rule },
            effects,
            ts: now_secs(),
        });
    }
}

/// The current epoch second (the receipt clock the t35 fired-plan audit record stamps).
fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
