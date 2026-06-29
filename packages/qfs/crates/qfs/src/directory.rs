//! `qfs::directory` — the **live `MembershipResolver` wiring** (t58, roadmap §1.2 decision I).
//!
//! t57 added the `member_of('/directories/<provider>/groups/<g>')` policy predicate and a
//! [`MembershipResolver`] seam, but EXPLICITLY deferred the live directory resolution to t58. This
//! module closes that gap: it wraps the t58 [`DirectorySource`] (the read seam of the
//! `/directories/...` driver) into a concrete [`MembershipResolver`], so a `member_of(...)` rule
//! resolves against an actual directory and grants a member / denies a non-member.
//!
//! ## Where the binary owns this (dep-direction)
//! The policy model lives in `qfs-server`; the directory read lives in `qfs-driver-directory`. The
//! `MembershipResolver` impl that bridges the two MUST NOT pull a driver crate into `qfs-server`
//! (the resolver carries resolved groups as **owned strings** across the boundary). So the bridge
//! lives HERE, in the terminal binary — the one allowlisted leaf that may depend on both
//! `qfs-driver-directory` and (through `qfs-mcp`'s re-export window) the `qfs-server` policy types,
//! without a forbidden direct `qfs-server` edge.
//!
//! ## Beware the authz loop (resolved up front, never inside `evaluate`)
//! A directory read that *feeds* a `member_of` decision must not be gated BY that decision. The
//! resolver consults [`qfs_driver_directory::resolve_is_member`], which reads
//! [`DirectorySource::groups_of`] — a path that does NOT re-enter the `member_of` evaluation. The
//! committer calls [`resolve_memberships`] ONCE up front to freeze the answer into the
//! [`DecisionContext`], keeping `qfs-server`'s policy `evaluate` pure (no I/O, no resolver call).
//!
//! ## Honesty — the live backend is a documented seam
//! [`DirectoryMembershipResolver::in_memory`] resolves against an empty in-memory
//! [`FixtureDirectory`], so a deployment with NO directory configured resolves NO memberships
//! (fail closed). A real LDAP / Active Directory / Entra ID / Google Workspace client implements
//! the SAME [`DirectorySource`] trait against a live connection — that is the seam this slice
//! leaves open (no heavy vendor SDK is pulled in, and no live directory connection is claimed).

use qfs_driver_directory::{resolve_is_member, DirectorySource, FixtureDirectory};
use qfs_mcp::MembershipResolver;

/// A t57 [`MembershipResolver`] backed by a t58 [`DirectorySource`] — the bridge that makes
/// `member_of('/directories/<provider>/groups/<g>')` resolve against a real (or in-memory)
/// directory. Generic over the source so a hermetic [`FixtureDirectory`] (tests / the in-memory
/// case) and a future live client share one impl.
pub struct DirectoryMembershipResolver<S: DirectorySource> {
    source: S,
}

impl<S: DirectorySource> DirectoryMembershipResolver<S> {
    /// Build a resolver over an injected [`DirectorySource`] (the live client, or a fixture).
    #[must_use]
    pub fn new(source: S) -> Self {
        Self { source }
    }
}

impl DirectoryMembershipResolver<FixtureDirectory> {
    /// The default in-memory resolver: an EMPTY [`FixtureDirectory`]. A deployment that has not yet
    /// configured a live directory connection resolves NO memberships (fail closed) — a
    /// `member_of(...)` rule denies everyone until a real [`DirectorySource`] is injected via
    /// [`DirectoryMembershipResolver::new`]. This is the honest default while the live LDAP/AD/
    /// Entra/Workspace client remains a documented seam.
    #[must_use]
    pub fn in_memory() -> Self {
        Self::new(FixtureDirectory::new())
    }
}

impl<S: DirectorySource> MembershipResolver for DirectoryMembershipResolver<S> {
    fn is_member(&self, actor: Option<&str>, directory: &str) -> bool {
        // Reads `DirectorySource::groups_of` (NOT a `member_of` re-entry), so it cannot cycle
        // through the policy that consults it. Fail-closed on anonymous / bad ref / backend error.
        resolve_is_member(&self.source, actor, directory)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use qfs_core::{EffectKind, EffectNode, NodeId, Plan, Target, VfsPath};
    use qfs_mcp::{
        evaluate_with_context, resolve_memberships, Condition, DecisionContext, DriverGlob, Policy,
        Rule, Verb, VerbSet,
    };
    use qfs_types::DriverId;

    /// A small fixture directory: alice belongs to `eng`, bob belongs to nothing.
    fn engineering_directory() -> FixtureDirectory {
        FixtureDirectory::new()
            .with_user("alice@corp.example", "Alice")
            .with_user("bob@corp.example", "Bob")
            .with_group("eng", "Engineering")
            .with_member("alice@corp.example", "eng")
    }

    /// A single-effect `INSERT` plan the policy gates (the driver/path are immaterial — the rule's
    /// `DriverGlob::any()` matches any target; the `member_of` CONDITION is what gates).
    fn insert_plan() -> Plan {
        Plan::leaf(EffectNode::new(
            NodeId(0),
            EffectKind::Insert,
            Target::new(DriverId::new("mail"), VfsPath::new("/mail/inbox")),
        ))
    }

    /// THE load-bearing t58 deliverable: a `member_of('/directories/google/groups/eng')` policy
    /// grants a member and denies a non-member, resolved LIVE through the directory driver's source.
    /// This makes t57's deferred `member_of` genuinely live for the fixture/in-memory case — proven
    /// end-to-end through the real policy path (`resolve_memberships` → `evaluate_with_context`).
    #[test]
    fn member_of_grants_member_and_denies_non_member_via_directory() {
        let dir_ref = "/directories/google/groups/eng";
        let policy = Policy::new("eng-only").with_rule(
            Rule::allow(VerbSet::one(Verb::Insert), DriverGlob::any())
                .when(Condition::MemberOf(dir_ref.into())),
        );
        let plan = insert_plan();
        let resolver = DirectoryMembershipResolver::new(engineering_directory());

        // alice IS a member of eng ⇒ the directory read resolves the membership into the context,
        // and the conditional grant applies ⇒ Allow.
        let alice = resolve_memberships(
            DecisionContext::for_user("alice@corp.example"),
            &policy,
            &resolver,
        );
        assert!(
            alice.satisfies_condition(&Condition::MemberOf(dir_ref.into())),
            "alice's eng membership resolved into the context"
        );
        assert!(
            evaluate_with_context(&policy, &plan, &alice).is_allow(),
            "a member of eng is granted the conditional INSERT"
        );

        // bob is NOT a member ⇒ nothing resolves (fail closed) ⇒ the conditional grant never
        // applies ⇒ Deny.
        let bob = resolve_memberships(
            DecisionContext::for_user("bob@corp.example"),
            &policy,
            &resolver,
        );
        assert!(bob.memberships.is_empty(), "bob resolves no eng membership");
        assert!(
            !evaluate_with_context(&policy, &plan, &bob).is_allow(),
            "a non-member of eng is denied the conditional INSERT"
        );
    }

    /// The honest default: an empty in-memory directory resolves NO memberships, so a
    /// `member_of(...)` rule denies even a named actor until a real [`DirectorySource`] is wired
    /// (fail closed).
    #[test]
    fn in_memory_default_resolves_no_memberships() {
        let dir_ref = "/directories/google/groups/eng";
        let policy = Policy::new("eng-only").with_rule(
            Rule::allow(VerbSet::one(Verb::Insert), DriverGlob::any())
                .when(Condition::MemberOf(dir_ref.into())),
        );
        let plan = insert_plan();
        let resolver = DirectoryMembershipResolver::in_memory();

        let ctx = resolve_memberships(
            DecisionContext::for_user("alice@corp.example"),
            &policy,
            &resolver,
        );
        assert!(
            ctx.memberships.is_empty(),
            "no directory configured ⇒ fail closed"
        );
        assert!(!evaluate_with_context(&policy, &plan, &ctx).is_allow());
    }
}
