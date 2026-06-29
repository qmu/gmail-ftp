//! t81 (roadmap **M5** — decision U / §3.3): the **shared-connection USE gate** — the pure
//! actor-policy decision that says whether a team member may USE a project/team-owned connection.
//!
//! ## What this adds over the plan gate
//! [`super::enforce::evaluate_with_context`] gates a *plan's effects* by `(verb, driver, path)` for
//! the resolved actor. t81 adds a second, narrower question the binary asks at **bind time**, BEFORE
//! the secret is decrypted: *does this member's policy grant them the shared connection's scope at
//! all?* A project-owned connection (`owner_scope = project`) is the team's credential; the member
//! uses it *as the team*, so the bind must be gated on the actor — not on who holds a token (§3.3,
//! "policy gates the actor, the connection picks the credential").
//!
//! [`evaluate_shared_use`] answers exactly that: given the project [`Policy`], the resolved
//! [`DecisionContext`] (actor / roles / groups / memberships), and the connection's realm **scope**
//! (`/projects/<proj>/…`, a t71 realm path), it returns [`SharedUseDecision::Allow`] iff some rule
//! grants this actor that scope, else a fail-closed [`SharedUseDecision::Deny`] with a secret-free
//! reason. The binary turns this into the `actor_granted` boolean it feeds the pure
//! `qfs_secrets::shared_use_gate` — so the secret is resolved ONLY after a passing gate.
//!
//! ## Why USE is scope-only (not verb-scoped)
//! "Using" a shared connection is not a single verb — the plan's individual verbs are still gated by
//! the ordinary plan gate ([`super::gate_plan_with_context`]). The USE gate asks only the coarser
//! reachability question ("is the actor granted this connection's scope?"), so it ignores the rule's
//! [`VerbSet`](super::model::VerbSet) and consults the **who** (subject), **where** (realm scope),
//! and **conditional** (`member_of`) axes. A scope-less ALLOW rule (no `AT` clause) for the actor is
//! a broad grant that covers every realm, so it grants the connection's scope too.
//!
//! ## Default-deny preserved
//! No matching ALLOW ⇒ [`SharedUseDecision::Deny`]. An earlier matching `DENY` wins over a later
//! `ALLOW` (first-match, deny-by-precedence — the same rule order semantics as the plan enforcer).
//! Under the anonymous context only an unscoped `Anyone` rule can match, so a narrowed (team) policy
//! contributes nothing until a real actor is resolved: fail closed.

use super::context::DecisionContext;
use super::model::{Effectivity, Policy};

/// The result of the shared-connection USE gate (t81). `Allow` permits the member to USE the
/// project-owned connection (the bind may resolve the secret); `Deny` carries a secret-free reason
/// (the connection scope + the failing axis — never a credential).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SharedUseDecision {
    /// The actor's policy grants the connection's scope — the bind may proceed.
    Allow,
    /// The actor's policy does NOT grant the connection's scope — fail closed.
    Deny {
        /// A secret-free, AI-legible reason (scope + failing axis only).
        reason: String,
    },
}

impl SharedUseDecision {
    /// Whether the member is permitted to USE the shared connection.
    #[must_use]
    pub fn is_allow(&self) -> bool {
        matches!(self, SharedUseDecision::Allow)
    }

    /// The secret-free denial reason, or `None` for an allow.
    #[must_use]
    pub fn deny_reason(&self) -> Option<&str> {
        match self {
            SharedUseDecision::Allow => None,
            SharedUseDecision::Deny { reason } => Some(reason),
        }
    }
}

/// Evaluate whether the resolved actor `ctx` is granted USE of a project-owned connection whose
/// realm `scope` is `conn_scope`, under the project `policy` (t81). Pure: no I/O, no mutation — the
/// actor was resolved up front (see [`super::context::resolve_memberships`]) and frozen into `ctx`.
///
/// Walks the rules top-down; the FIRST rule that applies to this actor AND covers `conn_scope`
/// decides (an earlier `DENY` overrides a later `ALLOW`). A rule *applies + covers* when:
/// - its [`Subject`](super::model::Subject) holds for the actor ([`DecisionContext::satisfies_subject`]),
/// - its realm scope covers `conn_scope` — a scope-less rule covers every realm; a scoped rule must
///   [`ScopeGlob::matches_path`](super::model::ScopeGlob::matches_path) the connection scope (the
///   realm gate applies, so a `/members/…` grant never covers a `/projects/…` connection), and
/// - its [`Condition`](super::model::Condition) holds ([`DecisionContext::satisfies_condition`]).
///
/// No applicable ALLOW ⇒ fail-closed [`SharedUseDecision::Deny`] (default-deny, decision U / §3.3).
/// The verb axis is intentionally NOT consulted (USE is coarser than a single verb — the plan's
/// verbs are gated separately by the plan enforcer).
#[must_use]
pub fn evaluate_shared_use(
    policy: &Policy,
    ctx: &DecisionContext,
    conn_scope: &str,
) -> SharedUseDecision {
    for rule in &policy.rules {
        let covers_scope = rule
            .scope
            .as_ref()
            .is_none_or(|s| s.matches_path(conn_scope));
        if ctx.satisfies_subject(&rule.subject)
            && covers_scope
            && ctx.satisfies_condition(&rule.condition)
        {
            // First matching rule decides (deny-by-precedence): an earlier DENY wins.
            return match rule.effect {
                Effectivity::Allow => SharedUseDecision::Allow,
                Effectivity::Deny => SharedUseDecision::Deny {
                    reason: format!(
                        "policy denies USE of the shared connection at scope `{conn_scope}` \
                         (an explicit DENY rule applies to the actor)"
                    ),
                },
            };
        }
    }
    // No applicable ALLOW ⇒ default-deny. The reason names the scope + the failing axis (secret-free)
    // so a narrowed (team) grant that did not apply reads legibly, not as a missing connection.
    let detail = near_miss_axis(policy, ctx, conn_scope)
        .unwrap_or_else(|| "no rule grants the actor this scope".to_string());
    SharedUseDecision::Deny {
        reason: format!(
            "policy denies USE of the shared connection at scope `{conn_scope}` \
             (default-deny: {detail})"
        ),
    }
}

/// Find the first rule that would have granted USE but for one failing t81 axis, and name it
/// (secret-free). Pure: scans the rules already in hand. `None` if no near-match.
fn near_miss_axis(policy: &Policy, ctx: &DecisionContext, conn_scope: &str) -> Option<String> {
    for rule in &policy.rules {
        if rule.effect != Effectivity::Allow {
            continue;
        }
        if !ctx.satisfies_subject(&rule.subject) {
            return Some("a rule's actor scope did not apply".to_string());
        }
        if let Some(scope) = &rule.scope {
            if !scope.matches_path(conn_scope) {
                return Some(format!(
                    "the granted scope {} does not cover it",
                    scope.render()
                ));
            }
        }
        if !ctx.satisfies_condition(&rule.condition) {
            // The condition label is secret-free (a directory ref, never a credential).
            if let Some(label) = rule.condition.label() {
                return Some(format!("the condition {label} did not hold"));
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
        Condition, DriverGlob, RoleGraph, Rule, ScopeGlob, Subject, Verb, VerbSet,
    };

    /// A team policy that grants the `member` role USE of the `/projects/acme/**` realm.
    fn acme_team_policy() -> Policy {
        Policy::new("acme-team").with_rule(
            Rule::allow(VerbSet::all(), DriverGlob::any())
                .for_subject(Subject::Role("member".into()))
                .scoped(ScopeGlob::parse("/projects/acme/**").unwrap()),
        )
    }

    #[test]
    fn a_granted_member_may_use_the_shared_connection() {
        let policy = acme_team_policy();
        let graph = RoleGraph::new();
        let alice = DecisionContext::for_user("alice").with_roles(["member".to_string()], &graph);
        let decision = evaluate_shared_use(&policy, &alice, "/projects/acme/connections/github");
        assert!(
            decision.is_allow(),
            "a granted member is allowed: {decision:?}"
        );
    }

    #[test]
    fn an_ungranted_member_is_default_denied() {
        let policy = acme_team_policy();
        let graph = RoleGraph::new();
        // Bob has no `member` role ⇒ the team rule's subject does not apply ⇒ default-deny.
        let bob = DecisionContext::for_user("bob").with_roles(["guest".to_string()], &graph);
        let decision = evaluate_shared_use(&policy, &bob, "/projects/acme/connections/github");
        assert!(!decision.is_allow(), "an ungranted member must be denied");
        let reason = decision.deny_reason().unwrap();
        assert!(reason.contains("/projects/acme/connections/github"));
        // Secret-free.
        for forbidden in ["token", "secret", "password", "ciphertext"] {
            assert!(
                !reason.to_lowercase().contains(forbidden),
                "leaked `{forbidden}`: {reason}"
            );
        }
    }

    #[test]
    fn two_members_with_different_policies_get_different_reach_over_the_same_connection() {
        // The SAME shared connection (`/projects/acme/**`), two members, two policies ⇒ different
        // reach — the headline t81 property (step 4).
        let policy = acme_team_policy();
        let graph = RoleGraph::new();
        let scope = "/projects/acme/connections/github";

        let granted = DecisionContext::for_user("alice").with_roles(["member".to_string()], &graph);
        let ungranted = DecisionContext::for_user("bob").with_roles(["guest".to_string()], &graph);

        assert!(evaluate_shared_use(&policy, &granted, scope).is_allow());
        assert!(!evaluate_shared_use(&policy, &ungranted, scope).is_allow());
    }

    #[test]
    fn a_grant_in_another_realm_does_not_cover_a_projects_connection() {
        // A `/members/alice/**` grant must NOT cover a `/projects/acme/**` connection (the realm
        // gate, decision P) — even for the same actor.
        let policy = Policy::new("self-only").with_rule(
            Rule::allow(VerbSet::all(), DriverGlob::any())
                .scoped(ScopeGlob::parse("/members/alice/**").unwrap()),
        );
        let ctx = DecisionContext::for_user("alice");
        let decision = evaluate_shared_use(&policy, &ctx, "/projects/acme/connections/github");
        assert!(
            !decision.is_allow(),
            "a cross-realm grant must not cover the connection"
        );
    }

    #[test]
    fn a_scope_less_allow_covers_every_realm() {
        // A broad `ALLOW` with no `AT` clause grants every realm, so it covers the connection scope.
        let policy = Policy::new("broad")
            .with_rule(Rule::allow(VerbSet::one(Verb::Insert), DriverGlob::any()));
        let ctx = DecisionContext::anonymous();
        assert!(evaluate_shared_use(&policy, &ctx, "/projects/acme/connections/github").is_allow());
    }

    #[test]
    fn an_earlier_deny_overrides_a_later_allow_first_match() {
        // Deny-precedence: an explicit DENY for the actor's role wins over a later team ALLOW.
        let policy = Policy::new("precedence")
            .with_rule(
                Rule::deny(VerbSet::all(), DriverGlob::any())
                    .for_subject(Subject::Role("intern".into()))
                    .scoped(ScopeGlob::parse("/projects/acme/**").unwrap()),
            )
            .with_rule(
                Rule::allow(VerbSet::all(), DriverGlob::any())
                    .scoped(ScopeGlob::parse("/projects/acme/**").unwrap()),
            );
        let graph = RoleGraph::new();
        let scope = "/projects/acme/connections/github";

        let intern = DecisionContext::for_user("i").with_roles(["intern".to_string()], &graph);
        assert!(
            !evaluate_shared_use(&policy, &intern, scope).is_allow(),
            "the earlier DENY must win for an intern"
        );
        // A non-intern skips the DENY (subject mismatch) and hits the later ALLOW.
        let staff = DecisionContext::for_user("s").with_roles(["staff".to_string()], &graph);
        assert!(evaluate_shared_use(&policy, &staff, scope).is_allow());
    }

    #[test]
    fn empty_policy_denies_use_default_deny() {
        let decision = evaluate_shared_use(
            &Policy::default(),
            &DecisionContext::for_user("x"),
            "/projects/acme/connections/github",
        );
        assert!(
            !decision.is_allow(),
            "an empty policy denies USE (fail closed)"
        );
    }

    #[test]
    fn a_member_of_condition_gates_use() {
        let dir = "/directories/google/groups/acme";
        let policy = Policy::new("eng").with_rule(
            Rule::allow(VerbSet::all(), DriverGlob::any())
                .scoped(ScopeGlob::parse("/projects/acme/**").unwrap())
                .when(Condition::MemberOf(dir.into())),
        );
        let scope = "/projects/acme/connections/github";
        // Member of the directory (pre-resolved into the context) ⇒ allow.
        let member = DecisionContext::for_user("u").with_membership(dir);
        assert!(evaluate_shared_use(&policy, &member, scope).is_allow());
        // Non-member ⇒ default-deny, reason names the condition (secret-free).
        let outsider = DecisionContext::for_user("u");
        let decision = evaluate_shared_use(&policy, &outsider, scope);
        assert!(!decision.is_allow());
        assert!(decision.deny_reason().unwrap().contains("member_of"));
    }
}
