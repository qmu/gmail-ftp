//! The **pure** policy enforcer (RFD-0001 §3 purity invariant / §10 least privilege).
//!
//! [`evaluate`] is a pure function over `(Policy, Plan)`: it walks the effect DAG, derives
//! `(Verb, driver, path)` from each effect node's **already-carried** `kind` + `target` (E2
//! nodes carry these — the enforcer reads them, never re-derives from driver internals),
//! evaluates the policy's rules top-down, and returns the FIRST denial (with the offending
//! node id, verb, driver, and the matching rule index) or [`PolicyDecision::Allow`] if every
//! effect is permitted.
//!
//! ## Default-deny (fail closed)
//! No matching rule ⇒ the policy's `default` (which is `Deny` for the default/empty policy).
//! A handler with no policy, or an empty policy, therefore **denies every write effect**.
//!
//! ## What is evaluated
//! Only **write/CALL** effects (INSERT/UPSERT/UPDATE/REMOVE/CALL, and `/server` config
//! writes) are gated — these are the effects a COMMIT plan carries. `Read`/`List` nodes are
//! pure dependencies of a write (RFD §6) and are skipped (a pure read produces an empty
//! commit plan and routes through the SEPARATE read path; see the crate docs).
//!
//! ## can ∧ may
//! This is the **may** layer only (does the *handler's policy* permit the verb). The t13
//! capability check ("can the *driver* do the verb") is a distinct, earlier gate with its own
//! error; the two are kept legibly separate — a policy denial never masquerades as a
//! capability error and vice versa.

use qfs_core::{EffectKind, Plan};

use super::model::{Effectivity, Policy, Verb};

/// The result of evaluating a [`Policy`] against a [`Plan`] (RFD §10). `Allow` permits the
/// whole plan; `Deny` carries the FIRST offending effect node + the matching rule index (or
/// `None` when the default-deny fired with no matching rule), all secret-free.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PolicyDecision {
    /// Every write effect in the plan is permitted.
    Allow,
    /// The plan is denied. Carries the offending node and the reason coordinates.
    Deny {
        /// The plan-local node id of the first denied effect.
        node: u32,
        /// The verb of the denied effect.
        verb: Verb,
        /// The driver the denied effect targets (secret-free name only).
        driver: String,
        /// The matching rule index that denied it, or `None` if the default-deny fired (no
        /// rule matched at all).
        rule: Option<usize>,
        /// t37 OBS-2: when a broad `ALLOW ALL` *would* have matched but was held back because the
        /// verb is irreversible (REMOVE/CALL), this records the held-back rule index so the deny
        /// reason reads as "a broad ALL does not grant irreversible verbs" rather than a generic
        /// default-deny. `None` for an ordinary default-deny (no near-match). Secret-free.
        held_by_broad_all: Option<usize>,
    },
}

impl PolicyDecision {
    /// Whether the decision permits the plan.
    #[must_use]
    pub fn is_allow(&self) -> bool {
        matches!(self, PolicyDecision::Allow)
    }

    /// A secret-free, AI-legible denial reason (driver name + verb + rule index only — never
    /// payloads or credentials, RFD §10). `None` for an `Allow`.
    #[must_use]
    pub fn deny_reason(&self) -> Option<String> {
        match self {
            PolicyDecision::Allow => None,
            PolicyDecision::Deny {
                node,
                verb,
                driver,
                rule,
                held_by_broad_all,
            } => Some(match (rule, held_by_broad_all) {
                // t37 OBS-2: a broad `ALLOW ALL` matched the driver/verb but is held back because
                // the verb is irreversible — say so explicitly, so the operator does not read it
                // as an ordinary default-deny and reach for an unrelated fix.
                (None, Some(all_idx)) => format!(
                    "policy denies {} on driver `{}` (node #{node}): a broad `ALLOW ALL` \
                     (rule {all_idx}) does not grant the irreversible verb — add an explicit \
                     `ALLOW {}` to permit it",
                    verb.label(),
                    driver,
                    verb.label()
                ),
                (Some(idx), _) => format!(
                    "policy denies {} on driver `{}` (node #{node}, rule {idx})",
                    verb.label(),
                    driver
                ),
                (None, None) => format!(
                    "policy denies {} on driver `{}` (node #{node}, default-deny: no rule \
                     matched)",
                    verb.label(),
                    driver
                ),
            }),
        }
    }
}

/// How an effect node is classified for policy purposes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EffectClass {
    /// A pure read dependency (`Read`/`List`) — NOT policy-bearing (a read goes through the
    /// separate read path; a write's read dependency is not gated).
    Read,
    /// A policy-bearing write/CALL effect, classified as `verb`.
    Verb(Verb),
    /// An effect kind the policy layer does not recognize (a NEW `EffectKind` variant added
    /// after this layer was written). Fail-closed: the enforcer DENIES it — a new effect must
    /// not be silently permitted (RFD §10 default-deny).
    Unknown,
}

/// Classify a plan [`EffectKind`]. `Read`/`List` are pure dependencies (skipped); the write
/// verbs map to their [`Verb`]; an unrecognized future kind is [`EffectClass::Unknown`]
/// (fail-closed denied).
#[must_use]
pub fn classify_effect(kind: &EffectKind) -> EffectClass {
    match kind {
        EffectKind::Read | EffectKind::List => EffectClass::Read,
        EffectKind::Insert => EffectClass::Verb(Verb::Insert),
        EffectKind::Upsert => EffectClass::Verb(Verb::Upsert),
        EffectKind::Update => EffectClass::Verb(Verb::Update),
        EffectKind::Remove => EffectClass::Verb(Verb::Remove),
        EffectKind::Call(_) => EffectClass::Verb(Verb::Call),
        // A `/server` self-config write maps to the verb its op implies — these are governed
        // exactly like any other effect (a handler that rewrites `/server` must be granted it).
        EffectKind::ServerConfigWrite { op, .. } => EffectClass::Verb(match op {
            qfs_core::ServerWriteOp::Insert => Verb::Insert,
            qfs_core::ServerWriteOp::Upsert => Verb::Upsert,
            qfs_core::ServerWriteOp::Update => Verb::Update,
            qfs_core::ServerWriteOp::Remove => Verb::Remove,
        }),
        // A future `EffectKind` variant: fail-closed (deny). The match is intentionally NOT a
        // bare `_` for the known set — only this one catch-all for genuinely-new variants.
        _ => EffectClass::Unknown,
    }
}

/// Back-compat helper: the policy [`Verb`] of an effect, or `None` for a read dependency.
/// (An [`EffectClass::Unknown`] also yields `None`; callers that need fail-closed handling use
/// [`classify_effect`] directly — the enforcer does.)
#[must_use]
pub fn verb_for_effect(kind: &EffectKind) -> Option<Verb> {
    match classify_effect(kind) {
        EffectClass::Verb(v) => Some(v),
        EffectClass::Read | EffectClass::Unknown => None,
    }
}

/// Evaluate `policy` against `plan` (RFD §10). Pure: no I/O, no mutation. Walks the effect
/// nodes in plan order, classifies each into a `(verb, driver, path)`, evaluates the rules
/// top-down, and returns the FIRST denial or [`PolicyDecision::Allow`] if every write effect
/// is permitted.
///
/// Default-deny: a write effect that no rule matches falls to `policy.default` — which is
/// `Deny` for the default/empty policy, so a no-rule policy denies every write.
#[must_use]
pub fn evaluate(policy: &Policy, plan: &Plan) -> PolicyDecision {
    for node in plan.nodes() {
        let driver = node.target.driver.as_str().to_string();
        let path = node.target.path.as_str();
        // Only write/CALL effects are policy-bearing; read dependencies are skipped; an
        // unrecognized future kind is denied fail-closed (default-deny, RFD §10).
        let verb = match classify_effect(&node.kind) {
            EffectClass::Read => continue,
            EffectClass::Verb(v) => v,
            EffectClass::Unknown => {
                return PolicyDecision::Deny {
                    node: node.id.index(),
                    // No owned verb for an unknown kind; report the closest irreversible
                    // class so the operator treats it with maximal caution.
                    verb: Verb::Call,
                    driver,
                    rule: None,
                    held_by_broad_all: None,
                };
            }
        };

        // Walk rules top-down; the first rule that matches decides this effect.
        let mut decided: Option<(Effectivity, Option<usize>)> = None;
        for (idx, rule) in policy.rules.iter().enumerate() {
            if rule.matches(verb, &driver, path) {
                decided = Some((rule.effect, Some(idx)));
                break;
            }
        }
        // No rule matched ⇒ fall to the policy default (fail-closed for the default policy).
        let (effect, rule) = decided.unwrap_or((policy.default, None));

        if effect == Effectivity::Deny {
            // t37 OBS-2: when this denial is a default-deny of an irreversible verb, detect a
            // broad `ALLOW ALL` allow rule that matched the driver/verbset but was held back by
            // the irreversible-strictness rule — so the reason can name that near-match instead
            // of reading as a generic default-deny. Pure: scans the rules already in hand.
            let held_by_broad_all = if rule.is_none() && verb.is_irreversible_class() {
                policy.rules.iter().position(|r| {
                    r.effect == Effectivity::Allow
                        && r.is_broad_all()
                        && r.verbs.contains(verb)
                        && r.driver.matches(&driver, path)
                })
            } else {
                None
            };
            return PolicyDecision::Deny {
                node: node.id.index(),
                verb,
                driver,
                rule,
                held_by_broad_all,
            };
        }
    }
    PolicyDecision::Allow
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::model::{DriverGlob, Rule, VerbSet};
    use qfs_core::{DriverId, EffectNode, NodeId, ProcId, Target, VfsPath};

    fn write_node(id: u32, kind: EffectKind, driver: &str, path: &str) -> EffectNode {
        EffectNode::new(
            NodeId(id),
            kind,
            Target::new(DriverId::new(driver), VfsPath::new(path)),
        )
    }

    fn plan_of(nodes: Vec<EffectNode>) -> Plan {
        let mut p = Plan::pure();
        p.nodes = nodes;
        p
    }

    #[test]
    fn empty_policy_denies_every_effect() {
        let policy = Policy::default();
        let plan = plan_of(vec![write_node(
            0,
            EffectKind::Insert,
            "mail",
            "/mail/inbox",
        )]);
        match evaluate(&policy, &plan) {
            PolicyDecision::Deny {
                node, verb, rule, ..
            } => {
                assert_eq!(node, 0);
                assert_eq!(verb, Verb::Insert);
                assert_eq!(rule, None, "default-deny carries no rule index");
            }
            PolicyDecision::Allow => panic!("empty policy must deny"),
        }
    }

    #[test]
    fn select_only_plan_is_allowed_even_under_empty_policy() {
        // A pure read produces no write nodes; the commit plan is empty ⇒ Allow.
        let policy = Policy::default();
        let plan = plan_of(vec![write_node(0, EffectKind::Read, "mail", "/mail/inbox")]);
        assert!(evaluate(&policy, &plan).is_allow());
    }

    #[test]
    fn allow_insert_permits_insert_denies_remove() {
        let policy = Policy::new("api")
            .with_rule(Rule::allow(VerbSet::one(Verb::Insert), DriverGlob::any()));
        let allowed = plan_of(vec![write_node(0, EffectKind::Insert, "log", "/log")]);
        assert!(evaluate(&policy, &allowed).is_allow());

        let denied = plan_of(vec![write_node(1, EffectKind::Remove, "log", "/log")]);
        match evaluate(&policy, &denied) {
            PolicyDecision::Deny { verb, .. } => assert_eq!(verb, Verb::Remove),
            PolicyDecision::Allow => panic!("REMOVE not granted ⇒ deny"),
        }
    }

    #[test]
    fn call_is_denied_without_explicit_allow_call() {
        let policy = Policy::new("api")
            .with_rule(Rule::allow(VerbSet::one(Verb::Insert), DriverGlob::any()));
        let plan = plan_of(vec![write_node(
            0,
            EffectKind::Call(ProcId::new("mail.send")),
            "mail",
            "/mail/outbox",
        )]);
        match evaluate(&policy, &plan) {
            PolicyDecision::Deny { verb, driver, .. } => {
                assert_eq!(verb, Verb::Call);
                assert_eq!(driver, "mail");
            }
            PolicyDecision::Allow => panic!("CALL not granted ⇒ deny"),
        }
    }

    #[test]
    fn allow_all_token_does_not_grant_irreversible() {
        // A broad `ALLOW ALL` grants reversible writes but NOT REMOVE/CALL.
        let policy = Policy::new("broad")
            .with_rule(Rule::allow(VerbSet::all(), DriverGlob::any()).as_all_token());

        let insert = plan_of(vec![write_node(0, EffectKind::Insert, "log", "/log")]);
        assert!(evaluate(&policy, &insert).is_allow(), "ALL grants INSERT");

        let remove = plan_of(vec![write_node(0, EffectKind::Remove, "log", "/log")]);
        let decision = evaluate(&policy, &remove);
        assert!(!decision.is_allow(), "ALL must NOT grant REMOVE");
        // t37 OBS-2: the deny reason names the held-back broad ALL, not a generic default-deny.
        match &decision {
            PolicyDecision::Deny {
                held_by_broad_all, ..
            } => assert!(
                held_by_broad_all.is_some(),
                "an irreversible verb held back by a broad ALL must record the near-match rule"
            ),
            PolicyDecision::Allow => panic!("must deny"),
        }
        let reason = decision.deny_reason().unwrap();
        assert!(
            reason.contains("broad `ALLOW ALL`") && reason.contains("ALLOW REMOVE"),
            "OBS-2 reason should explain the broad-ALL hold-back: {reason}"
        );

        let call = plan_of(vec![write_node(
            0,
            EffectKind::Call(ProcId::new("mail.send")),
            "mail",
            "/mail",
        )]);
        assert!(
            !evaluate(&policy, &call).is_allow(),
            "ALL must NOT grant CALL"
        );
    }

    #[test]
    fn explicit_verb_list_grants_irreversible() {
        // An explicit `ALLOW REMOVE,CALL` DOES grant them (not a broad ALL token).
        let policy = Policy::new("cleanup").with_rule(Rule::allow(
            VerbSet::from_verbs(&[Verb::Remove, Verb::Call]),
            DriverGlob::any(),
        ));
        let remove = plan_of(vec![write_node(0, EffectKind::Remove, "log", "/log")]);
        assert!(evaluate(&policy, &remove).is_allow());
    }

    #[test]
    fn driver_scoped_rule_denies_other_driver() {
        let policy = Policy::new("mailonly").with_rule(Rule::allow(
            VerbSet::one(Verb::Insert),
            DriverGlob::new("mail"),
        ));
        let mail = plan_of(vec![write_node(0, EffectKind::Insert, "mail", "/mail/x")]);
        assert!(evaluate(&policy, &mail).is_allow());
        let other = plan_of(vec![write_node(0, EffectKind::Insert, "s3", "/s3/x")]);
        assert!(!evaluate(&policy, &other).is_allow());
    }

    #[test]
    fn first_denial_is_returned() {
        let policy = Policy::new("api")
            .with_rule(Rule::allow(VerbSet::one(Verb::Insert), DriverGlob::any()));
        let plan = plan_of(vec![
            write_node(0, EffectKind::Insert, "log", "/log"),
            write_node(1, EffectKind::Remove, "log", "/log"),
            write_node(2, EffectKind::Remove, "s3", "/s3"),
        ]);
        match evaluate(&policy, &plan) {
            PolicyDecision::Deny { node, .. } => assert_eq!(node, 1, "first denial wins"),
            PolicyDecision::Allow => panic!(),
        }
    }
}
