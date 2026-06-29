//! `qfs-identity::billing` — the **pure billing-tier model** (roadmap **M9 / t67**): a FREE
//! individual tier and a PAID team tier, the per-tier ENTITLEMENTS, and the default-deny ENTITLEMENT
//! GATE that decides whether a paid-only capability is permitted for a given plan.
//!
//! ## Why this lives in `qfs-identity` (a pure leaf)
//! A tier is account/team-level state, the natural neighbour of [`Membership`](crate::Membership) /
//! [`Role`](crate::Role). Keeping the model here makes it a **pure, vendor-free domain core**: it
//! owns no DB, no clock, and — crucially — **no payment SDK**. The plan/subscription rows are
//! recorded as DATA under `/sys/billing` (the System DB, binary leaf); the PAYMENT PROVIDER that
//! moves money is a [`PaymentProvider`](../../qfs/billing/index.html) SEAM in the binary. This module
//! is only the answer to "given a recorded plan, what may it do".
//!
//! ## The fail-closed rule (default-deny toward the LOWER tier — §3 purity / RFD §10)
//! Every ambiguity resolves DOWNWARD to the free tier, never upward to paid:
//! - an **unknown / unrecognised** stored tier label decodes to [`Tier::FreeIndividual`]
//!   ([`Tier::decode`]); a garbled subscription status decodes to a NON-active status
//!   ([`SubscriptionStatus::decode`]);
//! - a paid tier whose subscription is **not active** (past-due, cancelled, …) yields the FREE
//!   entitlements ([`BillingPlan::effective_tier`]) — an unpaid/lapsed team is exactly a free
//!   individual until it pays again;
//! - a **missing** plan (no `/sys/billing` row for the team) is the free default
//!   ([`BillingPlan::free`]).
//!
//! So a paid-only capability ([`Capability::TeamConnections`]) is granted ONLY by an
//! actively-paid team plan, and denied for everything else — the same default-deny posture the
//! policy engine takes for an unmatched write.

use std::fmt;

/// The billing tier of an account or team (roadmap §3.4 / M9). A **closed set** of exactly two
/// tiers: a FREE individual tier (the default for any account) and a PAID team tier (the managed-team
/// offering brokered, team-wide connections). A new tier is a new variant here, never a side-channel
/// flag — the tier is the single authority the entitlement gate reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Tier {
    /// The free individual tier — the DEFAULT for any account, and the fail-closed floor every
    /// ambiguity resolves to. Carries the [`Entitlements::free`] set (no team-wide capabilities).
    #[default]
    FreeIndividual,
    /// The paid team tier (the managed-team offering). Carries the [`Entitlements::paid`] set
    /// (team-wide brokered connections, unbounded members) — but ONLY while its subscription is
    /// [`SubscriptionStatus::Active`] (see [`BillingPlan::effective_tier`]).
    PaidTeam,
}

impl Tier {
    /// The stable wire/storage label (`free-individual` / `paid-team`) — the value persisted in the
    /// `/sys/billing` `tier` column and round-tripped by [`Tier::decode`].
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Tier::FreeIndividual => "free-individual",
            Tier::PaidTeam => "paid-team",
        }
    }

    /// Decode a stored tier label. **Fail-closed:** an unknown / unrecognised value decodes to
    /// [`Tier::FreeIndividual`] (never to the paid tier) — a garbled or attacker-supplied tier can
    /// only ever LOSE entitlements, never gain them (RFD §10 default-deny).
    #[must_use]
    pub fn decode(label: &str) -> Self {
        match label {
            "paid-team" => Tier::PaidTeam,
            _ => Tier::FreeIndividual,
        }
    }

    /// The entitlement set this tier carries on its own (before the subscription-status check). Use
    /// [`BillingPlan::entitlements`] for the gate — it folds in the active-subscription rule.
    #[must_use]
    pub fn entitlements(self) -> Entitlements {
        match self {
            Tier::FreeIndividual => Entitlements::free(),
            Tier::PaidTeam => Entitlements::paid(),
        }
    }
}

impl fmt::Display for Tier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The lifecycle status of a paid subscription (the provider's view, recorded as the `/sys/billing`
/// `status` column). Only [`SubscriptionStatus::Active`] grants the paid entitlements; every other
/// status (and a free plan, which is implicitly [`SubscriptionStatus::Inactive`]) falls to the free
/// floor. A NON-paid free plan stores [`SubscriptionStatus::Inactive`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SubscriptionStatus {
    /// The subscription is current and paid — the ONE status that unlocks the paid entitlements.
    Active,
    /// The subscription lapsed (a payment failed). Treated as the free floor until it recovers.
    PastDue,
    /// The subscription was cancelled. Treated as the free floor.
    Canceled,
    /// No active subscription (the free tier's status, or an unknown/garbled stored value). The
    /// fail-closed default.
    #[default]
    Inactive,
}

impl SubscriptionStatus {
    /// The stable wire/storage label.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            SubscriptionStatus::Active => "active",
            SubscriptionStatus::PastDue => "past-due",
            SubscriptionStatus::Canceled => "canceled",
            SubscriptionStatus::Inactive => "inactive",
        }
    }

    /// Decode a stored status label. **Fail-closed:** an unknown / unrecognised value decodes to
    /// [`SubscriptionStatus::Inactive`] (a NON-active status) — a garbled status can never be read as
    /// "active" and so can never unlock the paid tier.
    #[must_use]
    pub fn decode(label: &str) -> Self {
        match label {
            "active" => SubscriptionStatus::Active,
            "past-due" => SubscriptionStatus::PastDue,
            "canceled" => SubscriptionStatus::Canceled,
            _ => SubscriptionStatus::Inactive,
        }
    }

    /// Whether this status is the one (active) status that grants the paid entitlements.
    #[must_use]
    pub fn is_active(self) -> bool {
        matches!(self, SubscriptionStatus::Active)
    }
}

impl fmt::Display for SubscriptionStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A paid-tier capability — a feature the FREE tier does not carry and the PAID (actively-subscribed)
/// team tier does. A **closed set**: a new gated feature adds a variant here so the gate stays the
/// single authority. Today the one paid-only capability is team-wide brokered connections (t66/t81),
/// the managed-team feature M9 monetises.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Capability {
    /// Provision/USE a team-wide **brokered** connection (t66 broker, t81 shared connections). The
    /// managed-team feature: a free individual cannot share a team connection.
    TeamConnections,
    /// Add another member to the team (the free tier is a single individual; a team is many).
    AddTeamMember,
}

impl Capability {
    /// A short, stable, secret-free code for structured surfaces / deny reasons.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Capability::TeamConnections => "team_connections",
            Capability::AddTeamMember => "add_team_member",
        }
    }
}

impl fmt::Display for Capability {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.code())
    }
}

/// The concrete LIMITS / capabilities a tier carries (roadmap §3.4). A pure value — the gate
/// ([`Entitlements::permits`]) reads it, nothing here performs I/O. Adding an entitlement is a field
/// here + its mapping in [`Entitlements::free`] / [`Entitlements::paid`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Entitlements {
    /// Whether team-wide brokered connections ([`Capability::TeamConnections`]) are permitted.
    pub team_connections: bool,
    /// The maximum number of members. The free individual tier is a single person (`1`); the paid
    /// team tier is effectively unbounded ([`u32::MAX`]).
    pub max_members: u32,
}

impl Entitlements {
    /// The FREE individual entitlements: no team-wide connections, a single member. This is also the
    /// fail-closed floor every ambiguity resolves to.
    #[must_use]
    pub fn free() -> Self {
        Self {
            team_connections: false,
            max_members: 1,
        }
    }

    /// The PAID team entitlements: team-wide brokered connections, unbounded members.
    #[must_use]
    pub fn paid() -> Self {
        Self {
            team_connections: true,
            max_members: u32::MAX,
        }
    }

    /// Whether this entitlement set permits `capability`. The pure gate primitive — default-deny is
    /// implicit because the free set says `false` to every paid-only capability.
    #[must_use]
    pub fn permits(&self, capability: Capability) -> bool {
        match capability {
            Capability::TeamConnections => self.team_connections,
            // A team admits more than one member exactly when team-wide features are entitled.
            Capability::AddTeamMember => self.max_members > 1,
        }
    }
}

/// A recorded billing plan: the [`Tier`] plus its subscription [`SubscriptionStatus`] — the shape the
/// `/sys/billing` row carries (alongside its `team_id` / `current_period_end` metadata, which do not
/// affect the gate). The ENTITLEMENT GATE reads a `BillingPlan`, never a bare tier, so the
/// active-subscription rule is never bypassed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BillingPlan {
    /// The recorded tier (what was bought).
    pub tier: Tier,
    /// The subscription's lifecycle status (whether it is currently paid).
    pub status: SubscriptionStatus,
}

impl Default for BillingPlan {
    fn default() -> Self {
        Self::free()
    }
}

impl BillingPlan {
    /// The free default — what a team with NO recorded `/sys/billing` row resolves to (a missing
    /// plan is the free floor, never paid).
    #[must_use]
    pub fn free() -> Self {
        Self {
            tier: Tier::FreeIndividual,
            status: SubscriptionStatus::Inactive,
        }
    }

    /// An actively-paid team plan (the only configuration that unlocks paid entitlements).
    #[must_use]
    pub fn paid_team() -> Self {
        Self {
            tier: Tier::PaidTeam,
            status: SubscriptionStatus::Active,
        }
    }

    /// Decode a plan from its stored labels. **Fail-closed at every axis:** an unknown tier decodes
    /// free, a garbled status decodes non-active — so a corrupted row can only ever resolve to the
    /// free floor.
    #[must_use]
    pub fn decode(tier: &str, status: &str) -> Self {
        Self {
            tier: Tier::decode(tier),
            status: SubscriptionStatus::decode(status),
        }
    }

    /// The **effective** tier the gate uses — the recorded tier, DOWNGRADED to free unless the paid
    /// subscription is active. This is the fail-closed rule in one place: a paid tier whose
    /// subscription lapsed is, for entitlement purposes, exactly a free individual.
    #[must_use]
    pub fn effective_tier(&self) -> Tier {
        match self.tier {
            Tier::PaidTeam if self.status.is_active() => Tier::PaidTeam,
            // Paid-but-not-active, or free: the free floor.
            _ => Tier::FreeIndividual,
        }
    }

    /// The entitlements this plan grants (the effective tier's set — folding in the
    /// active-subscription rule).
    #[must_use]
    pub fn entitlements(&self) -> Entitlements {
        self.effective_tier().entitlements()
    }

    /// Whether this plan permits `capability` — the top-level ENTITLEMENT GATE. Default-deny toward
    /// the free floor: a paid-only capability is granted only by an actively-paid team plan.
    #[must_use]
    pub fn permits(&self, capability: Capability) -> bool {
        self.entitlements().permits(capability)
    }

    /// Gate `capability`, returning a structured, secret-free [`EntitlementDenied`] when the plan
    /// does not entitle it (so a caller can refuse a paid-only feature with a legible reason rather
    /// than a bare `false`). The fail-closed enforcement primitive the binary's broker gate calls.
    ///
    /// # Errors
    /// [`EntitlementDenied`] when `capability` is not entitled by this plan's effective tier.
    pub fn gate(&self, capability: Capability) -> Result<(), EntitlementDenied> {
        if self.permits(capability) {
            Ok(())
        } else {
            Err(EntitlementDenied {
                capability,
                effective_tier: self.effective_tier(),
            })
        }
    }
}

/// A paid-only capability was denied for a plan (the fail-closed gate refusal). Secret-free: it names
/// the capability + the effective tier only — never a payment secret, a card, or a provider id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EntitlementDenied {
    /// The capability that was refused.
    pub capability: Capability,
    /// The effective tier that lacked it (already folded through the active-subscription rule).
    pub effective_tier: Tier,
}

impl fmt::Display for EntitlementDenied {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "the `{}` capability requires the paid team tier; the effective tier is `{}`",
            self.capability.code(),
            self.effective_tier
        )
    }
}

impl std::error::Error for EntitlementDenied {}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two tiers carry the RIGHT entitlements: free = no team connections / single member; paid =
    /// team connections / unbounded members.
    #[test]
    fn each_tier_carries_the_right_entitlements() {
        let free = Entitlements::free();
        assert!(!free.team_connections, "free has no team-wide connections");
        assert_eq!(free.max_members, 1, "free is a single individual");

        let paid = Entitlements::paid();
        assert!(paid.team_connections, "paid grants team-wide connections");
        assert_eq!(paid.max_members, u32::MAX, "paid is unbounded members");

        assert_eq!(Tier::FreeIndividual.entitlements(), free);
        assert_eq!(Tier::PaidTeam.entitlements(), paid);
    }

    /// A paid-only capability is DENIED for the free tier and ALLOWED for the actively-paid team tier
    /// — the headline gate behaviour.
    #[test]
    fn paid_only_capability_denied_for_free_allowed_for_paid() {
        let free = BillingPlan::free();
        assert!(
            !free.permits(Capability::TeamConnections),
            "free tier must NOT permit team connections"
        );
        assert!(free.gate(Capability::TeamConnections).is_err());

        let paid = BillingPlan::paid_team();
        assert!(
            paid.permits(Capability::TeamConnections),
            "actively-paid team tier permits team connections"
        );
        assert!(paid.gate(Capability::TeamConnections).is_ok());
        assert!(paid.permits(Capability::AddTeamMember));
    }

    /// An UNKNOWN / unpaid plan FAILS CLOSED to the free entitlements, never paid: an unknown tier
    /// label decodes free, and a paid tier whose subscription is not active is downgraded to free.
    #[test]
    fn unknown_or_lapsed_plan_fails_closed_to_free() {
        // Unknown / attacker-supplied tier label ⇒ free.
        assert_eq!(Tier::decode("enterprise-unlimited"), Tier::FreeIndividual);
        assert_eq!(Tier::decode(""), Tier::FreeIndividual);
        let unknown = BillingPlan::decode("enterprise-unlimited", "active");
        assert_eq!(unknown.effective_tier(), Tier::FreeIndividual);
        assert!(!unknown.permits(Capability::TeamConnections));

        // A garbled status decodes non-active.
        assert_eq!(
            SubscriptionStatus::decode("???"),
            SubscriptionStatus::Inactive
        );

        // Paid tier, but the subscription LAPSED ⇒ free entitlements (fail closed toward lower tier).
        for status in ["past-due", "canceled", "inactive", "garbled"] {
            let lapsed = BillingPlan::decode("paid-team", status);
            assert_eq!(
                lapsed.effective_tier(),
                Tier::FreeIndividual,
                "a non-active paid subscription ({status}) must fall to the free floor"
            );
            assert!(
                !lapsed.permits(Capability::TeamConnections),
                "a lapsed paid plan ({status}) must NOT keep paid entitlements"
            );
        }

        // The missing-plan default is the free floor.
        assert_eq!(BillingPlan::default(), BillingPlan::free());
    }

    /// The stored labels round-trip through the decoders (recording a tier and reading it back is
    /// lossless for the known set).
    #[test]
    fn tier_and_status_labels_round_trip() {
        for tier in [Tier::FreeIndividual, Tier::PaidTeam] {
            assert_eq!(Tier::decode(tier.as_str()), tier);
        }
        for status in [
            SubscriptionStatus::Active,
            SubscriptionStatus::PastDue,
            SubscriptionStatus::Canceled,
            SubscriptionStatus::Inactive,
        ] {
            assert_eq!(SubscriptionStatus::decode(status.as_str()), status);
        }
        let plan = BillingPlan::paid_team();
        assert_eq!(
            BillingPlan::decode(plan.tier.as_str(), plan.status.as_str()),
            plan
        );
    }

    /// The deny reason is secret-free (names the capability + tier, no payment material).
    #[test]
    fn entitlement_denied_reason_is_secret_free() {
        let denied = BillingPlan::free()
            .gate(Capability::TeamConnections)
            .unwrap_err();
        let reason = denied.to_string();
        assert!(reason.contains("team_connections"));
        assert!(reason.contains("free-individual"));
        let lower = reason.to_lowercase();
        assert!(!lower.contains("card"));
        assert!(!lower.contains("token"));
        assert!(!lower.contains("secret"));
    }
}
