//! The invite + membership domain model (roadmap **M5 / t55**): the *joining* half of decision B
//! (every deployment holds its own `users`/`accounts`) — a host operator mints an [`Invite`], the
//! invitee REDEEMS it (one-time, expiring) to create their local identity and a [`Membership`]
//! linking them to the host (and, later, a project).
//!
//! These are plain owned DTOs — no vendor type, no live token. **Crucially, neither [`Invite`] nor
//! any DTO carries the raw invite token**: like a session token (t46) and the argon2id
//! `password_hash` (t45), the one-time invite secret is stored ONLY as a `sha256` digest and never
//! surfaced on a DTO a caller can read, log, or echo (RFD §10). The plaintext token lives exactly
//! once — inside the [`InviteToken`] the binary mints and returns as the one-time URL at create.
//!
//! ## Identity ≠ authorization (§4.1)
//! A [`Membership`] grants *belonging* ("you are a member"), never *capability* ("you may touch X").
//! What a member may do stays default-deny until `POLICY` (t57). Do not let "is a member" leak into
//! an authorization decision — a [`Role`] here is a coarse label for a later ACL, not a grant.

use core::fmt;

use qfs_crypto_core::{constant_time_eq, hex_lower, sha256_hex};

use crate::Secret;

/// An [`Invite`]'s stable internal id — the `invites.id` rowid. A NEW type so an invite id is never
/// confused for a [`crate::UserId`] / [`MembershipId`] or a raw integer in a signature.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct InviteId(pub i64);

impl fmt::Display for InviteId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// A [`Membership`]'s stable internal id — the `memberships.id` rowid.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct MembershipId(pub i64);

impl fmt::Display for MembershipId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// The one-time invite token — the secret the "signup URL" carries. High-entropy, generated from a
/// CSPRNG **in the binary leaf** (OS entropy injected into [`InviteToken::from_entropy`]) so this
/// core stays deterministic and testable (no `rand`/`getrandom` edge here), exactly like the t46
/// [`crate`]-adjacent session token. It is wrapped in the redacting [`Secret`] in transit, and only
/// its `sha256_hex` is ever persisted ([`InviteToken::hash`]). No `Clone` — a token is moved, never
/// silently duplicated.
pub struct InviteToken(Secret);

impl InviteToken {
    /// Build a token from injected OS entropy (the binary passes CSPRNG bytes). The bytes are
    /// lowercase-hex encoded into the opaque token STRING (URL-safe; 32 bytes → 64 hex chars),
    /// wrapped in the redacting [`Secret`]. Deterministic in `entropy` so the core is testable
    /// without owning a CSPRNG.
    #[must_use]
    pub fn from_entropy(entropy: &[u8]) -> Self {
        Self(Secret::from(hex_lower(entropy)))
    }

    /// Wrap a token value PRESENTED at redeem (the secret off the one-time URL / `qfs invite redeem`
    /// argument) for hashing/lookup. The value is treated as opaque; only its hash ever touches the
    /// store.
    #[must_use]
    pub fn from_redeem_value(value: &str) -> Self {
        Self(Secret::from(value))
    }

    /// The `sha256_hex` of the token — the value stored at rest (the `invites.token_hash` key) and
    /// the only representation that ever touches the DB. Preimage-resistant: a stored hash does not
    /// yield the token (a System-DB leak yields no usable invite).
    #[must_use]
    pub fn hash(&self) -> String {
        sha256_hex(self.0.expose())
    }

    /// Whether this token's hash equals `stored_hash`, compared in **constant time** (RFD §10 — the
    /// verification never short-circuits on the first mismatching byte). The store fetches the row by
    /// its indexed `token_hash`; this is the defense-in-depth equality check on the fetched hash.
    #[must_use]
    pub fn matches_hash(&self, stored_hash: &str) -> bool {
        constant_time_eq(self.hash().as_bytes(), stored_hash.as_bytes())
    }

    /// The redacting [`Secret`] wrapping the plaintext token — the ONE door used to put the token on
    /// the wire (the one-time URL), exactly once at create. Named `reveal` so every exposure of a
    /// live token is explicit and grep-able.
    #[must_use]
    pub fn reveal(&self) -> &Secret {
        &self.0
    }
}

/// The scope a [`Membership`] binds to. **OPEN PRODUCT DECISION (flagged, t55 — not baked in):** the
/// roadmap §3.4 leaves the host-wide-vs-project-scoped default explicitly open. [`MembershipScope::Host`]
/// is the least-surprising default for the milestone (the invitee joins *this deployment*); a
/// project-scoped membership carries the project ref. The caller chooses; this is not the final
/// taxonomy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MembershipScope {
    /// Membership of the whole host/deployment (the default initial membership).
    Host,
    /// Membership of a single project (carries the project ref on the row).
    Project,
}

impl MembershipScope {
    /// The stable on-disk string for this scope.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            MembershipScope::Host => "host",
            MembershipScope::Project => "project",
        }
    }

    /// Decode a stored scope string. Unknown values decode to [`MembershipScope::Host`] (a
    /// forward-compatible read; scope gates nothing yet — authorization is t57).
    #[must_use]
    pub fn decode(s: &str) -> Self {
        match s {
            "project" => MembershipScope::Project,
            _ => MembershipScope::Host,
        }
    }
}

impl fmt::Display for MembershipScope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A coarse membership role. **OPEN PRODUCT DECISION (flagged, t55 — not baked in):** the role
/// taxonomy (super-admin / project-admin / member) overlaps the t53 admin split the roadmap §3.4 info
/// box leaves open. This is a *label* on the membership for a LATER ACL (`POLICY`, t57), NOT an
/// authorization grant — identity ≠ authorization (§4.1). [`Role::Member`] is the default. Unknown
/// stored values decode to [`Role::Member`] (a member, not an admin — fail toward least privilege).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Role {
    /// The host/team owner (the operator who mints invites).
    Owner,
    /// An administrator (reserved for the t53/t57 admin split; not privileged yet).
    Admin,
    /// An ordinary member — the default for a redeemed invite.
    #[default]
    Member,
}

impl Role {
    /// The stable on-disk string for this role.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Role::Owner => "owner",
            Role::Admin => "admin",
            Role::Member => "member",
        }
    }

    /// Decode a stored role string. Unknown values decode to [`Role::Member`] — fail toward LEAST
    /// privilege (an unrecognised role is a plain member, never silently an admin).
    #[must_use]
    pub fn decode(s: &str) -> Self {
        match s {
            "owner" => Role::Owner,
            "admin" => Role::Admin,
            _ => Role::Member,
        }
    }
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The lifecycle state of an [`Invite`], derived from its timestamp columns at a given `now` (the
/// store has no separate status column — the timestamps ARE the truth, so a state can never drift
/// from them). The precedence is **revoked → redeemed → expired → pending**: a revoked invite reads
/// revoked even if also expired, and a redeemed one reads redeemed even past its expiry (the audit
/// fact "it was used" outranks "it would have expired").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InviteStatus {
    /// Live and unused — redeemable.
    Pending,
    /// Already redeemed (single-use; a replay is rejected).
    Redeemed,
    /// Explicitly revoked by an operator before use.
    Revoked,
    /// Past its `expires_at` and never used.
    Expired,
}

impl InviteStatus {
    /// The stable human/string label for this status.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            InviteStatus::Pending => "pending",
            InviteStatus::Redeemed => "redeemed",
            InviteStatus::Revoked => "revoked",
            InviteStatus::Expired => "expired",
        }
    }
}

impl fmt::Display for InviteStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The parameters for minting an invite ([`crate::InviteStore::create_invite`]). The token itself is
/// NOT here — the binary mints it from a CSPRNG and passes only its `sha256` digest across the seam,
/// keeping the raw token out of the domain/store (it is returned to the operator exactly once).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewInvite {
    /// The optional invitee email. `Some` when inviting a known address (the delivery target when
    /// mail is configured — a documented seam); `None` for a bare one-time URL handed out of band.
    pub email: Option<String>,
    /// The membership scope the redeemer will join (host-wide by default; see [`MembershipScope`]).
    pub scope: MembershipScope,
    /// The project ref for a [`MembershipScope::Project`] invite (else `None`).
    pub project: Option<String>,
    /// The initial membership role granted on redeem (a label, not a grant — see [`Role`]).
    pub role: Role,
    /// The time-to-live in seconds; the store computes `expires_at = now + ttl` on its own clock.
    pub ttl_secs: i64,
    /// The operator who minted the invite (the `users.id`), recorded for audit. `None` if minted by
    /// an unauthenticated bootstrap path (e.g. the very first owner).
    pub created_by: Option<crate::UserId>,
}

/// One row of the System-DB `invites` table — an outstanding (or spent) invitation. **No raw token
/// field — by design.** The token's digest lives in the store's `token_hash` column and is matched
/// internally by [`crate::InviteStore::accept_invite`]; it is never exposed on this DTO. The
/// lifecycle is computed from the timestamps via [`Invite::status_at`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invite {
    /// The stable internal id.
    pub id: InviteId,
    /// The optional invitee email (a handle, safe to echo — not a secret).
    pub email: Option<String>,
    /// The membership scope the redeemer joins.
    pub scope: MembershipScope,
    /// The project ref for a project-scoped invite.
    pub project: Option<String>,
    /// The initial role granted on redeem.
    pub role: Role,
    /// The minting operator (for audit).
    pub created_by: Option<crate::UserId>,
    /// When the invite was created (RFC 3339 UTC; the store stamps it).
    pub created_at: String,
    /// The absolute expiry (RFC 3339 UTC) — a redeem at/after this is rejected.
    pub expires_at: String,
    /// When the invite was redeemed (RFC 3339 UTC), or `None` if still unused.
    pub consumed_at: Option<String>,
    /// When the invite was revoked (RFC 3339 UTC), or `None` if never revoked.
    pub revoked_at: Option<String>,
}

impl Invite {
    /// Derive the [`InviteStatus`] at `now` (an RFC-3339 UTC instant in the schema's fixed-width
    /// form, which is lexicographically sortable so a plain string compare orders time correctly).
    /// Precedence revoked → redeemed → expired → pending (see [`InviteStatus`]).
    #[must_use]
    pub fn status_at(&self, now: &str) -> InviteStatus {
        if self.revoked_at.is_some() {
            InviteStatus::Revoked
        } else if self.consumed_at.is_some() {
            InviteStatus::Redeemed
        } else if self.expires_at.as_str() <= now {
            InviteStatus::Expired
        } else {
            InviteStatus::Pending
        }
    }
}

/// One row of the System-DB `memberships` table — the link that says a [`crate::User`] *belongs* to
/// the host (or a project). Belonging only (§4.1): it confers no capability; the ACL is t57.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Membership {
    /// The stable internal id.
    pub id: MembershipId,
    /// The member.
    pub user_id: crate::UserId,
    /// The scope of the membership (host-wide or a project).
    pub scope: MembershipScope,
    /// The project ref for a project-scoped membership.
    pub project: Option<String>,
    /// The member's role label (not an authorization grant — §4.1).
    pub role: Role,
    /// When the membership was created (RFC 3339 UTC; the store stamps it).
    pub created_at: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::UserId;

    #[test]
    fn ids_display_as_their_integer() {
        assert_eq!(InviteId(7).to_string(), "7");
        assert_eq!(MembershipId(42).to_string(), "42");
    }

    #[test]
    fn token_from_entropy_hex_encodes_and_hashes_stably() {
        let t = InviteToken::from_entropy(&[0x00, 0x0f, 0xff]);
        assert_eq!(t.reveal().expose_str(), Some("000fff"));
        let h = t.hash();
        assert_eq!(h.len(), 64, "sha256_hex is 64 hex chars");
        // The redeem round-trip: a value presented at redeem hashes identically to the minted token.
        assert!(InviteToken::from_redeem_value("000fff").matches_hash(&h));
        assert!(!InviteToken::from_redeem_value("000ffe").matches_hash(&h));
    }

    #[test]
    fn token_debug_redacts_the_value() {
        let t = InviteToken::from_entropy(&[0xab, 0xcd]);
        let dumped = format!("{:?}", t.reveal());
        assert!(
            !dumped.contains("abcd"),
            "token value must not appear: {dumped}"
        );
        assert!(dumped.contains("redacted"));
    }

    #[test]
    fn scope_and_role_round_trip_and_are_forward_compatible() {
        assert_eq!(MembershipScope::Host.as_str(), "host");
        assert_eq!(MembershipScope::decode("project"), MembershipScope::Project);
        // An unknown scope decodes to Host.
        assert_eq!(MembershipScope::decode("galaxy"), MembershipScope::Host);
        assert_eq!(Role::default(), Role::Member);
        assert_eq!(Role::decode("owner"), Role::Owner);
        // An unknown role decodes to Member — fail toward least privilege, never silently admin.
        assert_eq!(Role::decode("super-duper-admin"), Role::Member);
    }

    fn invite(consumed: Option<&str>, revoked: Option<&str>, expires: &str) -> Invite {
        Invite {
            id: InviteId(1),
            email: None,
            scope: MembershipScope::Host,
            project: None,
            role: Role::Member,
            created_by: Some(UserId(1)),
            created_at: "2026-01-01T00:00:00Z".into(),
            expires_at: expires.into(),
            consumed_at: consumed.map(str::to_string),
            revoked_at: revoked.map(str::to_string),
        }
    }

    #[test]
    fn status_precedence_is_revoked_then_redeemed_then_expired_then_pending() {
        let now = "2026-06-28T00:00:00Z";
        // Pending: live, unused, unrevoked.
        assert_eq!(
            invite(None, None, "2026-12-31T00:00:00Z").status_at(now),
            InviteStatus::Pending
        );
        // Expired: past expiry, unused.
        assert_eq!(
            invite(None, None, "2026-01-02T00:00:00Z").status_at(now),
            InviteStatus::Expired
        );
        // Redeemed outranks an also-elapsed expiry.
        assert_eq!(
            invite(Some("2026-01-03T00:00:00Z"), None, "2026-01-02T00:00:00Z").status_at(now),
            InviteStatus::Redeemed
        );
        // Revoked outranks everything.
        assert_eq!(
            invite(
                Some("2026-01-03T00:00:00Z"),
                Some("2026-01-03T00:00:00Z"),
                "2026-12-31T00:00:00Z"
            )
            .status_at(now),
            InviteStatus::Revoked
        );
    }
}
