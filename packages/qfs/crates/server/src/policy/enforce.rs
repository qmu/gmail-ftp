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

use super::context::DecisionContext;
use super::model::{Effectivity, Policy, Rule, Verb};

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
        /// t57: when the default-deny fired because a rule matched the verb/driver but failed one
        /// of the richer axes (subject / realm-scope / `member_of` condition), this names the
        /// *failing axis* (secret-free — `"actor"`, `"scope /members/alice/**"`,
        /// `"member_of('/directories/...')"`) so a narrowed denial stays legible rather than
        /// reading as an unscoped default-deny. `None` when no near-match was found.
        detail: Option<String>,
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
                detail,
            } => Some(match (rule, held_by_broad_all, detail) {
                // t37 OBS-2: a broad `ALLOW ALL` matched the driver/verb but is held back because
                // the verb is irreversible — say so explicitly, so the operator does not read it
                // as an ordinary default-deny and reach for an unrelated fix.
                (None, Some(all_idx), _) => format!(
                    "policy denies {} on driver `{}` (node #{node}): a broad `ALLOW ALL` \
                     (rule {all_idx}) does not grant the irreversible verb — add an explicit \
                     `ALLOW {}` to permit it",
                    verb.label(),
                    driver,
                    verb.label()
                ),
                (Some(idx), _, _) => format!(
                    "policy denies {} on driver `{}` (node #{node}, rule {idx})",
                    verb.label(),
                    driver
                ),
                // t57: a rule matched the verb/driver but the actor/scope/condition axis failed —
                // name the failing axis so the narrowed denial is legible.
                (None, None, Some(axis)) => format!(
                    "policy denies {} on driver `{}` (node #{node}, default-deny: a rule matched \
                     the verb/driver but the {axis} did not apply to the actor)",
                    verb.label(),
                    driver
                ),
                (None, None, None) => format!(
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

/// Evaluate `policy` against `plan` under the **anonymous** decision context (RFD §10). Pure:
/// no I/O, no mutation. This is the back-compat entry point — equivalent to
/// [`evaluate_with_context`] with [`DecisionContext::anonymous`]. Under the anonymous context
/// only unscoped (`FOR anyone`, no condition) rules can match, so a pre-t57 policy behaves
/// exactly as before, and a t57-narrowed rule contributes nothing until a real actor is
/// resolved (fail closed).
#[must_use]
pub fn evaluate(policy: &Policy, plan: &Plan) -> PolicyDecision {
    evaluate_with_context(policy, plan, &DecisionContext::anonymous())
}

/// Whether `rule` matches the effect `(verb, driver, path)` **for the resolved actor `ctx`**
/// (t57). All five axes must hold: the verb+driver glob (with the irreversible-strictness rule),
/// the [`Subject`](super::model::Subject), the realm-scoped path, and the
/// [`Condition`](super::model::Condition). Pure — `ctx` is already resolved, so this performs no
/// I/O.
fn rule_matches_in_context(
    rule: &Rule,
    verb: Verb,
    driver: &str,
    path: &str,
    ctx: &DecisionContext,
) -> bool {
    rule.matches(verb, driver, path)
        && ctx.satisfies_subject(&rule.subject)
        && rule.scope.as_ref().is_none_or(|s| s.matches_path(path))
        && ctx.satisfies_condition(&rule.condition)
}

/// Evaluate `policy` against `plan` for a **resolved** [`DecisionContext`] (t57). Pure: no I/O,
/// no mutation — the actor's identity/roles/memberships were resolved up front (see
/// [`super::context::resolve_memberships`]) and frozen into `ctx`, so this is a total function
/// over `(policy, plan, ctx)`.
///
/// Walks the effect nodes in plan order, classifies each into a `(verb, driver, path)`,
/// evaluates the rules top-down (the FIRST matching rule decides — so an earlier `DENY` overrides
/// a later `ALLOW`; deny-by-precedence), and returns the FIRST denial or [`PolicyDecision::Allow`]
/// if every write effect is permitted. Default-deny: a write effect that no rule matches (or whose
/// matching rules all failed the actor/scope/condition axes) falls to `policy.default` — `Deny`
/// for the default/empty policy.
#[must_use]
pub fn evaluate_with_context(
    policy: &Policy,
    plan: &Plan,
    ctx: &DecisionContext,
) -> PolicyDecision {
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
                    detail: None,
                };
            }
        };

        // Walk rules top-down; the first rule that matches (in this actor's context) decides
        // this effect. First-match means an earlier DENY wins over a later ALLOW.
        let mut decided: Option<(Effectivity, Option<usize>)> = None;
        for (idx, rule) in policy.rules.iter().enumerate() {
            if rule_matches_in_context(rule, verb, &driver, path, ctx) {
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
            // t57: when the default-deny fired but a rule DID match the verb/driver and only the
            // actor/scope/condition axis held it back, name that failing axis (secret-free) so the
            // denial is legible as a narrowed grant that did not apply, not a missing rule.
            let detail = if rule.is_none() && held_by_broad_all.is_none() {
                near_miss_axis(policy, verb, &driver, path, ctx)
            } else {
                None
            };
            return PolicyDecision::Deny {
                node: node.id.index(),
                verb,
                driver,
                rule,
                held_by_broad_all,
                detail,
            };
        }
    }
    PolicyDecision::Allow
}

/// Find the first rule that matched the verb+driver but failed one of the t57 axes, and name the
/// failing axis (secret-free). Pure: scans the rules already in hand. `None` if no near-match.
fn near_miss_axis(
    policy: &Policy,
    verb: Verb,
    driver: &str,
    path: &str,
    ctx: &DecisionContext,
) -> Option<String> {
    for rule in &policy.rules {
        if !rule.matches(verb, driver, path) {
            continue;
        }
        if !ctx.satisfies_subject(&rule.subject) {
            return Some("actor".to_string());
        }
        if let Some(scope) = &rule.scope {
            if !scope.matches_path(path) {
                return Some(format!("scope {}", scope.render()));
            }
        }
        if !ctx.satisfies_condition(&rule.condition) {
            // The condition label is secret-free (a directory ref, never a credential).
            if let Some(label) = rule.condition.label() {
                return Some(label);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::context::DecisionContext;
    use crate::policy::model::{
        Condition, DriverGlob, RoleGraph, Rule, ScopeGlob, Subject, VerbSet,
    };
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
    fn sys_policies_write_is_default_denied_then_granted() {
        // t53: a `/sys/*` write is high-privilege and routes through the SAME default-deny policy
        // engine as any other driver (the path is the authorization subject). An empty/default
        // policy denies `INSERT INTO /sys/policies`; an explicit `ALLOW INSERT on driver sys`
        // grants it. This is what "policy-gated" means for the admin surface — no special case.
        let plan = plan_of(vec![write_node(
            0,
            EffectKind::Insert,
            "sys",
            "/sys/policies",
        )]);
        // Default-deny: a super-admin grant is NOT implicit.
        match evaluate(&Policy::default(), &plan) {
            PolicyDecision::Deny { verb, driver, .. } => {
                assert_eq!(verb, Verb::Insert);
                assert_eq!(driver, "sys");
            }
            PolicyDecision::Allow => panic!("/sys writes must be default-denied"),
        }
        // An explicit grant scoped to the `sys` driver permits the policy-grant insert.
        let granted = Policy::new("admin").with_rule(Rule::allow(
            VerbSet::one(Verb::Insert),
            DriverGlob::new("sys"),
        ));
        assert!(evaluate(&granted, &plan).is_allow());
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

    // ---- t57: actor / role-scoped rules, scoped-path conditions, member_of ----------------

    /// An `admin`-role rule grants the verb only to an actor whose resolved (inheritance-expanded)
    /// role set includes `admin`; everyone else falls to the fail-closed default-deny.
    #[test]
    fn role_scoped_rule_grants_only_the_matching_actor() {
        let policy = Policy::new("ops").with_rule(
            Rule::allow(VerbSet::one(Verb::Insert), DriverGlob::new("mail"))
                .for_subject(Subject::Role("admin".into())),
        );
        let plan = plan_of(vec![write_node(
            0,
            EffectKind::Insert,
            "mail",
            "/mail/outbox",
        )]);

        // An admin (directly, or via inheritance owner⊃admin) ⇒ allow.
        let graph = RoleGraph::new().inherits("owner", "admin");
        let admin = DecisionContext::for_user("a").with_roles(["admin".to_string()], &graph);
        assert!(evaluate_with_context(&policy, &plan, &admin).is_allow());
        let owner = DecisionContext::for_user("o").with_roles(["owner".to_string()], &graph);
        assert!(
            evaluate_with_context(&policy, &plan, &owner).is_allow(),
            "owner inherits admin (additive inheritance)"
        );

        // A plain member ⇒ default-deny, and the deny names the failing *actor* axis (legible).
        let member = DecisionContext::for_user("m").with_roles(["member".to_string()], &graph);
        let decision = evaluate_with_context(&policy, &plan, &member);
        match &decision {
            PolicyDecision::Deny { rule, detail, .. } => {
                assert_eq!(*rule, None, "narrowed rule did not match ⇒ default-deny");
                assert_eq!(detail.as_deref(), Some("actor"), "names the failing axis");
            }
            PolicyDecision::Allow => panic!("a non-admin must be denied"),
        }
        // The anonymous default path also denies (default-deny still holds for an unmatched actor).
        assert!(!evaluate(&policy, &plan).is_allow());
    }

    /// A realm-scoped rule grants only within its realm sub-tree; a node in another realm (or a
    /// different principal) falls to default-deny.
    #[test]
    fn scoped_path_rule_matches_within_its_realm_only() {
        let policy = Policy::new("alice-mail").with_rule(
            Rule::allow(VerbSet::one(Verb::Insert), DriverGlob::any())
                .scoped(ScopeGlob::parse("/members/alice/**").unwrap()),
        );
        let ctx = DecisionContext::anonymous(); // scope is actor-independent here

        let in_scope = plan_of(vec![write_node(
            0,
            EffectKind::Insert,
            "mail",
            "/members/alice/mail/outbox",
        )]);
        assert!(evaluate_with_context(&policy, &in_scope, &ctx).is_allow());

        // Same realm, different principal ⇒ deny (the scope names the failing axis).
        let other_principal = plan_of(vec![write_node(
            0,
            EffectKind::Insert,
            "mail",
            "/members/bob/mail/outbox",
        )]);
        match evaluate_with_context(&policy, &other_principal, &ctx) {
            PolicyDecision::Deny { detail, .. } => {
                assert_eq!(detail.as_deref(), Some("scope /members/alice/**"));
            }
            PolicyDecision::Allow => panic!("another principal must be denied"),
        }

        // Different realm ⇒ deny (the realm gate, decision P).
        let other_realm = plan_of(vec![write_node(
            0,
            EffectKind::Insert,
            "mail",
            "/projects/alice/mail/outbox",
        )]);
        assert!(!evaluate_with_context(&policy, &other_realm, &ctx).is_allow());
    }

    /// A `member_of(...)` conditional grant applies only when the directory membership was
    /// pre-resolved into the context; otherwise default-deny — and the deny reason is secret-free
    /// (it names the directory ref + verb + driver, never a credential).
    #[test]
    fn member_of_condition_gates_the_grant_and_decision_is_secret_free() {
        let dir = "/directories/google/groups/eng";
        let policy = Policy::new("eng-only").with_rule(
            Rule::allow(VerbSet::one(Verb::Insert), DriverGlob::new("mail"))
                .when(Condition::MemberOf(dir.into())),
        );
        let plan = plan_of(vec![write_node(
            0,
            EffectKind::Insert,
            "mail",
            "/mail/outbox",
        )]);

        // Member ⇒ allow (membership pre-resolved into the context).
        let member = DecisionContext::for_user("u").with_membership(dir);
        assert!(evaluate_with_context(&policy, &plan, &member).is_allow());

        // Non-member ⇒ default-deny; the reason names the directory ref but no secret/payload.
        let outsider = DecisionContext::for_user("u");
        let decision = evaluate_with_context(&policy, &plan, &outsider);
        assert!(!decision.is_allow());
        let reason = decision.deny_reason().unwrap();
        assert!(
            reason.contains("member_of"),
            "names the failing condition: {reason}"
        );
        assert!(reason.contains("mail") && reason.contains("INSERT"));
        // Secret-free: only driver/verb/condition-ref appear — assert no obvious secret markers.
        assert!(!reason.to_lowercase().contains("token"));
        assert!(!reason.to_lowercase().contains("secret"));
        assert!(!reason.to_lowercase().contains("password"));
    }

    /// Deny-precedence / first-match: an earlier `DENY` overrides a later `ALLOW` for the same
    /// actor/effect (the enforcer takes the FIRST matching rule top-down).
    #[test]
    fn earlier_deny_overrides_later_allow_first_match() {
        let policy = Policy::new("precedence")
            .with_rule(
                Rule::deny(VerbSet::one(Verb::Insert), DriverGlob::new("mail"))
                    .for_subject(Subject::Role("intern".into())),
            )
            .with_rule(Rule::allow(
                VerbSet::one(Verb::Insert),
                DriverGlob::new("mail"),
            ));
        let plan = plan_of(vec![write_node(
            0,
            EffectKind::Insert,
            "mail",
            "/mail/outbox",
        )]);

        // An intern hits the earlier DENY first ⇒ deny (rule index 0).
        let graph = RoleGraph::new();
        let intern = DecisionContext::for_user("i").with_roles(["intern".to_string()], &graph);
        match evaluate_with_context(&policy, &plan, &intern) {
            PolicyDecision::Deny { rule, .. } => assert_eq!(rule, Some(0), "earlier DENY wins"),
            PolicyDecision::Allow => panic!("intern must be denied by the earlier rule"),
        }
        // A non-intern skips the DENY (subject mismatch) and hits the later ALLOW ⇒ allow.
        let other = DecisionContext::for_user("o").with_roles(["staff".to_string()], &graph);
        assert!(evaluate_with_context(&policy, &plan, &other).is_allow());
    }

    /// A pre-t57 (unscoped) policy behaves identically under the anonymous context — the back-compat
    /// guarantee that keeps the existing 1605 tests green.
    #[test]
    fn unscoped_policy_is_unchanged_under_anonymous_context() {
        let policy = Policy::new("api")
            .with_rule(Rule::allow(VerbSet::one(Verb::Insert), DriverGlob::any()));
        let plan = plan_of(vec![write_node(0, EffectKind::Insert, "log", "/log")]);
        // Both the back-compat `evaluate` and the explicit anonymous context agree.
        assert!(evaluate(&policy, &plan).is_allow());
        assert!(evaluate_with_context(&policy, &plan, &DecisionContext::anonymous()).is_allow());
    }
}
