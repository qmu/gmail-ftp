//! The fire-path enforcement helper (RFD §8/§10): the single reusable seam every E7 committer
//! (HTTP write-endpoint, cron JOB, watchtower TRIGGER) calls AFTER `build_plan` and BEFORE
//! `commit`, so no handler plan runs unevaluated/unaudited and a deny aborts atomically.
//!
//! It replaces the t34 `AllowAllGate`, the t33 cron committer placeholder gate, and the t32
//! read-only-default with the REAL `evaluate`. It resolves the bound `policy: Option<String>`
//! ref against a [`PolicyTable`] snapshot, runs the pure [`evaluate`], builds the secret-free
//! per-effect summaries, and returns a [`GateOutcome`] the caller turns into a
//! [`FiredPlanRecord`] (always) + a commit/abort decision.

use std::collections::BTreeMap;

use qfs_core::Plan;

use super::audit::{FiredDecision, FiredPlanRecord};
use super::context::DecisionContext;
use super::enforce::{evaluate, evaluate_with_context, PolicyDecision};
use super::grammar::policy_from_def;
use super::model::Policy;
use crate::state::PolicyDef;

/// A read snapshot of `/server/policies` (name → row), the table the binding refreshes on each
/// reconcile and the committer resolves a bound policy ref against. Owned, secret-free.
pub type PolicyTable = BTreeMap<String, PolicyDef>;

/// Resolve a bound `policy` ref against `table` into an owned [`Policy`].
///
/// - `Some(name)` present in the table ⇒ the rehydrated `Policy`.
/// - `Some(name)` ABSENT from the table ⇒ a fail-closed default-deny policy (a dangling ref must
///   never accidentally allow — RFD §10).
/// - `None` (no policy attached) ⇒ a fail-closed default-deny policy (default-deny is the law).
#[must_use]
pub fn resolve_policy(policy: Option<&str>, table: &PolicyTable) -> Policy {
    match policy {
        Some(name) => match table.get(name) {
            Some(def) => policy_from_def(def),
            None => Policy::new(name), // dangling ref ⇒ default-deny
        },
        None => Policy::default(), // no policy ⇒ default-deny (fail closed)
    }
}

/// The result of gating a fired plan: the policy decision + the secret-free effect summaries
/// (so the caller can build the one [`FiredPlanRecord`] for allow AND deny).
#[derive(Debug, Clone)]
pub struct GateOutcome {
    /// The policy decision over the plan.
    pub decision: PolicyDecision,
    /// The secret-free per-effect summaries (`"INSERT mail:/mail/outbox"`).
    pub effects: Vec<String>,
}

impl GateOutcome {
    /// Whether the plan is permitted to commit.
    #[must_use]
    pub fn is_allow(&self) -> bool {
        self.decision.is_allow()
    }

    /// The secret-free denial reason, or `None` for an allow.
    #[must_use]
    pub fn deny_reason(&self) -> Option<String> {
        self.decision.deny_reason()
    }

    /// Build the single [`FiredPlanRecord`] for this fire (allow OR deny). `handler` is the
    /// cause label (`"job:nightly"`); `policy` is the bound policy name (empty if none).
    #[must_use]
    pub fn record(
        &self,
        handler: impl Into<String>,
        policy: impl Into<String>,
        ts: i64,
    ) -> FiredPlanRecord {
        FiredPlanRecord {
            handler: handler.into(),
            policy: policy.into(),
            decision: FiredDecision::from_decision(&self.decision),
            effects: self.effects.clone(),
            ts,
        }
    }
}

/// The secret-free per-effect summaries of a plan (`"<VERB> <driver>:<path>"`), driver + path
/// only — NEVER a row payload or credential (RFD §10). Read dependencies are included with
/// their READ/LIST label (they are not gated but are part of the honest effect summary).
#[must_use]
pub fn effect_summaries(plan: &Plan) -> Vec<String> {
    plan.nodes()
        .iter()
        .map(|n| {
            format!(
                "{} {}:{}",
                n.kind.label(),
                n.target.driver.as_str(),
                n.target.path.as_str()
            )
        })
        .collect()
}

/// Gate a built `plan` against the resolved `policy`: run the pure [`evaluate`] and assemble the
/// secret-free effect summaries. Pure — no I/O, no mutation. The caller commits ONLY on
/// `is_allow()` (atomic abort on deny: the commit is never invoked, so ZERO effects apply).
#[must_use]
pub fn gate_plan(policy: &Policy, plan: &Plan) -> GateOutcome {
    GateOutcome {
        decision: evaluate(policy, plan),
        effects: effect_summaries(plan),
    }
}

/// Gate a built `plan` against `policy` for a **resolved** actor [`DecisionContext`] (t57). Same
/// contract as [`gate_plan`], but the richer ACL axes (subject / realm-scope / `member_of`
/// condition) are evaluated against `ctx`. Pure — `ctx` is resolved up front (the impure
/// membership lookup is [`super::context::resolve_memberships`], called by the committer before
/// this), so the gate itself does no I/O. The committer commits ONLY on `is_allow()`.
#[must_use]
pub fn gate_plan_with_context(policy: &Policy, plan: &Plan, ctx: &DecisionContext) -> GateOutcome {
    GateOutcome {
        decision: evaluate_with_context(policy, plan, ctx),
        effects: effect_summaries(plan),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_policy_resolves_to_default_deny() {
        let table = PolicyTable::new();
        let p = resolve_policy(None, &table);
        assert!(p.rules.is_empty());
        assert_eq!(p.default, super::super::model::Effectivity::Deny);
    }

    #[test]
    fn dangling_ref_is_default_deny() {
        let table = PolicyTable::new();
        let p = resolve_policy(Some("ghost"), &table);
        assert!(p.rules.is_empty());
        assert_eq!(p.name, "ghost");
        assert_eq!(p.default, super::super::model::Effectivity::Deny);
    }

    #[test]
    fn resolves_present_policy() {
        let mut table = PolicyTable::new();
        table.insert(
            "api".to_string(),
            PolicyDef {
                name: "api".to_string(),
                handler: String::new(),
                allow: vec!["ALLOW INSERT".to_string()],
            },
        );
        let p = resolve_policy(Some("api"), &table);
        assert_eq!(p.rules.len(), 1);
    }

    #[test]
    fn fired_record_is_secret_free_and_one_per_fire() {
        use crate::audit::AuditSink;
        use qfs_core::{DriverId, EffectKind, EffectNode, NodeId, Plan, Target, VfsPath};

        // A plan whose row args would carry a secret payload (not modeled here — the point is the
        // gate summary must NEVER include payloads, only driver + path + verb).
        let mut plan = Plan::pure();
        plan.nodes = vec![EffectNode::new(
            NodeId(0),
            EffectKind::Insert,
            Target::new(DriverId::new("mail"), VfsPath::new("/mail/outbox")),
        )];

        // (1) deny (no policy) ⇒ one deny record carrying verb/driver, no payload.
        let table = PolicyTable::new();
        let policy = resolve_policy(None, &table);
        let outcome = gate_plan(&policy, &plan);
        assert!(!outcome.is_allow());
        let audit = AuditSink::new();
        audit.record_fired(outcome.record("trigger:t", "", 1000));
        assert_eq!(audit.fired_count(), 1, "exactly one fired record");
        let rec = match &audit.snapshot()[0] {
            crate::audit::AuditEntry::FiredPlan(r) => r.clone(),
            _ => panic!("expected FiredPlan"),
        };
        let summary = rec.summary();
        assert!(summary.contains("mail"), "names the driver");
        assert!(summary.contains("INSERT"), "names the verb");
        // The effect summary is driver + path only; no payload column ever appears.
        assert!(rec.effects.iter().all(|e| e == "INSERT mail:/mail/outbox"));
    }
}
