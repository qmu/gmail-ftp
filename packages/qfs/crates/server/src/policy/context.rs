//! The **resolved decision context** (t57) — the actor half of `evaluate`, kept strictly
//! separate from the pure enforcer so [`super::enforce::evaluate`] stays I/O-free.
//!
//! The richer ACL adds a *who* axis: a rule may be scoped to a user/role/group ([`Subject`]) and
//! gated on a `member_of('/directories/...')` predicate ([`Condition`]). Resolving *who the actor
//! is* and *which directories they belong to* is **I/O** (it reads identity/membership, t45/t55,
//! and — once t58 ships — a live `/directories/...` driver). That I/O happens **here, before**
//! the enforcer runs: the result is frozen into a [`DecisionContext`] of owned, secret-free data,
//! and the pure `evaluate` reads only that. This is the seam that keeps preview-as-CI able to
//! surface a policy denial with no live credentials.
//!
//! ## Default-deny preserved
//! [`DecisionContext::anonymous`] is the empty context — no user, no roles, no groups, no
//! memberships. Under it, only an `Anyone`/unconditional rule can match, so a richer (narrowed)
//! rule contributes nothing until a real actor is resolved: fail closed.

use std::collections::BTreeSet;

use super::model::{Condition, Policy, RoleGraph, Subject};

/// The acting principal + everything the (impure) resolver pre-computed about it, frozen into
/// owned data the pure enforcer reads. Secret-free: ids/role-labels/group-names/directory refs
/// only — never a credential.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DecisionContext {
    /// The acting user's owned id string (the binary maps a live `qfs-identity` `UserId` onto
    /// this), or `None` for an unauthenticated/anonymous actor.
    pub user: Option<String>,
    /// The actor's effective role labels — already **inheritance-expanded** (see
    /// [`DecisionContext::with_roles`]). t55 `Role`s carried as owned strings.
    pub roles: BTreeSet<String>,
    /// The actor's group/team memberships (owned names).
    pub groups: BTreeSet<String>,
    /// The directory refs the actor is a member of — the **pre-resolved** truth of every
    /// `member_of('/directories/...')` predicate the policy mentions. The enforcer tests
    /// membership by set lookup (pure), never by calling a resolver.
    pub memberships: BTreeSet<String>,
}

impl DecisionContext {
    /// The empty, anonymous context — no actor, no memberships. Under it only an unscoped rule
    /// ([`Subject::Anyone`] with [`Condition::Always`]) can match (the pre-t57 behaviour,
    /// fail-closed for everything else). The default `evaluate(policy, plan)` runs under this.
    #[must_use]
    pub fn anonymous() -> Self {
        Self::default()
    }

    /// A context for a concrete authenticated actor (builder). `roles` are the **directly
    /// granted** roles; pass through [`DecisionContext::with_roles`] to expand inheritance.
    #[must_use]
    pub fn for_user(user: impl Into<String>) -> Self {
        DecisionContext {
            user: Some(user.into()),
            ..Self::default()
        }
    }

    /// Set the actor's roles, **expanding** them under `graph` (additive-only inheritance, t57).
    #[must_use]
    pub fn with_roles(
        mut self,
        roles: impl IntoIterator<Item = String>,
        graph: &RoleGraph,
    ) -> Self {
        let direct: BTreeSet<String> = roles.into_iter().collect();
        self.roles = graph.expand(&direct);
        self
    }

    /// Set the actor's groups (builder).
    #[must_use]
    pub fn with_groups(mut self, groups: impl IntoIterator<Item = String>) -> Self {
        self.groups = groups.into_iter().collect();
        self
    }

    /// Record a resolved directory membership (builder) — the truth of a
    /// `member_of('/directories/...')` predicate for this actor.
    #[must_use]
    pub fn with_membership(mut self, directory: impl Into<String>) -> Self {
        self.memberships.insert(directory.into());
        self
    }

    /// Whether the actor satisfies a rule's [`Subject`] axis. [`Subject::Anyone`] always holds;
    /// a `User`/`Role`/`Group` subject holds only when the resolved actor carries it.
    #[must_use]
    pub fn satisfies_subject(&self, subject: &Subject) -> bool {
        match subject {
            Subject::Anyone => true,
            Subject::User(u) => self.user.as_deref() == Some(u.as_str()),
            Subject::Role(r) => self.roles.contains(r),
            Subject::Group(g) => self.groups.contains(g),
        }
    }

    /// Whether the actor satisfies a rule's [`Condition`] axis. [`Condition::Always`] always
    /// holds; `member_of(dir)` holds iff `dir` was pre-resolved into [`Self::memberships`].
    #[must_use]
    pub fn satisfies_condition(&self, condition: &Condition) -> bool {
        match condition {
            Condition::Always => true,
            Condition::MemberOf(dir) => self.memberships.contains(dir),
        }
    }
}

/// The **membership-resolver seam** (t57): the pure boundary that turns a
/// `member_of('/directories/...')` ref into a yes/no for an actor. The live impl (t58) routes
/// the `/directories/...` path through the `qfs-core` `MountRegistry`;
/// in t57 it is injectable and mocked, so the enforcer is unit-testable with no directory present.
///
/// The resolver is consulted **once, up front** ([`resolve_memberships`]) to build the
/// [`DecisionContext`]; it is *never* called from inside the pure `evaluate`.
pub trait MembershipResolver {
    /// Whether `actor` (its owned user id, or `None` for anonymous) is a member of `directory`.
    fn is_member(&self, actor: Option<&str>, directory: &str) -> bool;
}

/// Pre-resolve every `member_of('/directories/...')` ref a `policy` mentions into `ctx`, using
/// `resolver`. After this, the (pure) enforcer can decide every conditional grant by set lookup.
/// Returns the enriched context. This is the ONE place the (impure) resolver is consulted.
#[must_use]
pub fn resolve_memberships<R: MembershipResolver + ?Sized>(
    mut ctx: DecisionContext,
    policy: &Policy,
    resolver: &R,
) -> DecisionContext {
    let actor = ctx.user.clone();
    for rule in &policy.rules {
        if let Some(dir) = rule.condition.member_of_ref() {
            if !ctx.memberships.contains(dir) && resolver.is_member(actor.as_deref(), dir) {
                ctx.memberships.insert(dir.to_string());
            }
        }
    }
    ctx
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::model::{DriverGlob, Rule, Verb, VerbSet};

    /// A mock resolver: the actor is a member of exactly the directories in its allow-list.
    struct MockResolver {
        member_of: Vec<(Option<String>, String)>,
    }

    impl MembershipResolver for MockResolver {
        fn is_member(&self, actor: Option<&str>, directory: &str) -> bool {
            self.member_of
                .iter()
                .any(|(a, d)| a.as_deref() == actor && d == directory)
        }
    }

    #[test]
    fn anonymous_satisfies_only_anyone_and_always() {
        let ctx = DecisionContext::anonymous();
        assert!(ctx.satisfies_subject(&Subject::Anyone));
        assert!(!ctx.satisfies_subject(&Subject::Role("admin".into())));
        assert!(ctx.satisfies_condition(&Condition::Always));
        assert!(!ctx.satisfies_condition(&Condition::MemberOf("/directories/x".into())));
    }

    #[test]
    fn role_inheritance_is_expanded_additively() {
        // owner ⊃ admin ⊃ member: an owner actor effectively holds all three.
        let graph = RoleGraph::new()
            .inherits("owner", "admin")
            .inherits("admin", "member");
        let ctx = DecisionContext::for_user("u1").with_roles(["owner".to_string()], &graph);
        assert!(ctx.satisfies_subject(&Subject::Role("owner".into())));
        assert!(ctx.satisfies_subject(&Subject::Role("admin".into())));
        assert!(ctx.satisfies_subject(&Subject::Role("member".into())));
        assert!(!ctx.satisfies_subject(&Subject::Role("ghost".into())));
    }

    #[test]
    fn member_of_is_resolved_into_the_context_not_inside_evaluate() {
        let policy = Policy::new("p").with_rule(
            Rule::allow(VerbSet::one(Verb::Insert), DriverGlob::any())
                .when(Condition::MemberOf("/directories/google/groups/eng".into())),
        );
        let resolver = MockResolver {
            member_of: vec![(
                Some("u1".to_string()),
                "/directories/google/groups/eng".to_string(),
            )],
        };
        // u1 is a member ⇒ the directory ref lands in the context.
        let ctx = resolve_memberships(DecisionContext::for_user("u1"), &policy, &resolver);
        assert!(ctx.satisfies_condition(&Condition::MemberOf(
            "/directories/google/groups/eng".into()
        )));
        // u2 is NOT ⇒ nothing resolved (fail closed).
        let ctx2 = resolve_memberships(DecisionContext::for_user("u2"), &policy, &resolver);
        assert!(ctx2.memberships.is_empty());
    }
}
