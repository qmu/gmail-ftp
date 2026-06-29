//! The owned identity model: [`User`], [`Account`], and their ids ([`UserId`], [`AccountId`]).
//!
//! These are plain owned DTOs — no vendor type, no secret material. **Crucially, [`Account`] does
//! NOT carry the `password_hash`**: the hash is an at-rest persistence detail of the store, never a
//! field a caller can read, log, or surface (RFD §10). Verification goes through
//! [`crate::IdentityStore::verify_password`], which reads the hash internally and returns only a
//! bool.

use core::fmt;

/// The `provider` value for a local password account (vs. an OAuth/OIDC provider in M2). The local
/// account's `subject` is the user's email (the sign-in identifier).
pub const PROVIDER_LOCAL: &str = "local";

/// A [`User`]'s stable internal id — the `users.id` rowid. A NEW type so a user id is never confused
/// for an [`AccountId`] or a raw integer in a signature.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct UserId(pub i64);

impl fmt::Display for UserId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// An [`Account`]'s internal id — the `accounts.id` rowid. This is the **identity** account (a
/// linked sign-in identity), explicitly NOT the t44 credential `qfs_secrets::ConnectionId`; see the
/// crate docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct AccountId(pub i64);

impl fmt::Display for AccountId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// A user's account lifecycle status. Kept tiny and typed (not a bare string) so a caller switches
/// on it exhaustively. `active` is the sign-up default; `disabled` is reserved for a future admin /
/// off-boarding path (not reachable in t45). An unknown stored value decodes to [`UserStatus::Active`]
/// (fail-open on a metadata column — status gates nothing yet, decision §4.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UserStatus {
    /// The normal, signed-up state.
    Active,
    /// Reserved for a future off-boarding path; never produced by t45 sign-up.
    Disabled,
}

impl UserStatus {
    /// The stable on-disk string for this status.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            UserStatus::Active => "active",
            UserStatus::Disabled => "disabled",
        }
    }

    /// Decode a stored status string. Unknown values decode to [`UserStatus::Active`] (status gates
    /// nothing in t45, so a forward-compatible read is safer than an error).
    #[must_use]
    pub fn decode(s: &str) -> Self {
        match s {
            "disabled" => UserStatus::Disabled,
            _ => UserStatus::Active,
        }
    }
}

impl fmt::Display for UserStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A human identity: one row of the System-DB `users` table. The `primary_email` is the unique
/// human handle; one user owns one-or-more [`Account`]s (many-to-one). Carries no secret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct User {
    /// The stable internal id.
    pub id: UserId,
    /// The unique primary email (the human handle; lower-cased on sign-up).
    pub primary_email: String,
    /// When the user was created (RFC 3339 UTC; the store stamps it).
    pub created_at: String,
    /// The account-lifecycle status.
    pub status: UserStatus,
}

/// A linked sign-in identity for a [`User`]: one row of the System-DB `accounts` table. `provider`
/// is [`PROVIDER_LOCAL`] for a password account (subject = email) or an OAuth/OIDC provider id later;
/// `subject` is the provider-scoped identifier, unique per `(provider, subject)`.
///
/// **No `password_hash` field — by design.** The hash lives only in the store column and is read
/// internally by [`crate::IdentityStore::verify_password`]; it is never exposed on this DTO.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Account {
    /// The stable internal id (the IDENTITY account id, not a credential connection id).
    pub id: AccountId,
    /// The owning user.
    pub user_id: UserId,
    /// The provider: [`PROVIDER_LOCAL`] today, an OAuth/OIDC provider id in M2.
    pub provider: String,
    /// The provider-scoped subject (the email for a local account).
    pub subject: String,
    /// When the account was linked (RFC 3339 UTC).
    pub created_at: String,
}

/// The outcome of resolving the *sole* user for a session-less `whoami` (sessions land in t46). With
/// no session, `whoami` with no email can only answer when the deployment has exactly one user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SoleUser {
    /// No users have signed up yet.
    None,
    /// Exactly one user exists — the one `whoami` resolves to without a session.
    One(User),
    /// More than one user exists; `whoami` cannot pick without an email (no session yet).
    Many,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_display_as_their_integer() {
        assert_eq!(UserId(7).to_string(), "7");
        assert_eq!(AccountId(42).to_string(), "42");
    }

    #[test]
    fn user_status_round_trips_and_is_forward_compatible() {
        assert_eq!(UserStatus::Active.as_str(), "active");
        assert_eq!(UserStatus::Disabled.as_str(), "disabled");
        assert_eq!(UserStatus::decode("active"), UserStatus::Active);
        assert_eq!(UserStatus::decode("disabled"), UserStatus::Disabled);
        // An unknown future status decodes to Active (status gates nothing in t45).
        assert_eq!(
            UserStatus::decode("pending-verification"),
            UserStatus::Active
        );
    }
}
