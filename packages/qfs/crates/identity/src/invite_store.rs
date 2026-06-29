//! The consumer-side [`InviteStore`] trait + its structured [`InviteError`] and [`Redemption`]
//! outcome (roadmap **M5 / t55**).
//!
//! This is the **seam**: the trait is owned-DTO only and carries no SQLite/rusqlite type, so the
//! domain stays a leaf. The concrete implementation (a `rusqlite` store over the System DB) is
//! INJECTED by the binary — it lives in `qfs-store` (`SqliteInviteStore`), the one crate that owns a
//! real DB connection (the same split t45's `IdentityStore` / t46's `SessionStore` use).
//!
//! The trait trades only in token HASHES, never the plaintext token: the binary mints the opaque
//! token ([`crate::InviteToken`]) from a CSPRNG and hands only its `sha256` digest across this seam,
//! so a raw token never reaches the store. [`InviteStore::accept_invite`] is the security pivot — it
//! looks the digest up, verifies it constant-time, rejects an expired / consumed / revoked invite,
//! creates the redeemer's local identity (a `users` row + a `local` `accounts` row, reusing t45's
//! sign-up path), inserts the `memberships` row, and burns the invite (sets `consumed_at`) — all in
//! ONE transaction so a replay can never double-redeem.

use crate::model::User;
use crate::password::PasswordHash;
use crate::{Invite, InviteId, Membership, NewInvite, UserId};

/// The outcome of a successful [`InviteStore::accept_invite`]: the freshly-created [`User`] and the
/// [`Membership`] that joins them to the host/project. Authentication state (a t46 session) is
/// established ABOVE this seam by the binary — the domain leaf creates the *belonging*, not the
/// session (it has no `qfs-session` edge).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Redemption {
    /// The local user the invite created.
    pub user: User,
    /// The membership linking that user to the host (or a project).
    pub membership: Membership,
}

/// The invite persistence surface the binary's admin + redeem paths call. Consumer-side: it trades
/// only in owned invite/membership DTOs + token HASHES (`&str`) + a [`PasswordHash`]. The SQLite impl
/// is injected (see module docs).
///
/// `Send + Sync` so a built store can be shared, mirroring the identity / session handles.
pub trait InviteStore: Send + Sync {
    /// Mint an invite: persist `new`'s metadata plus the `sha256` `token_hash` of a freshly-minted
    /// one-time token (the raw token is NOT passed — the binary keeps it to render the one-time URL
    /// and hands only its hash here), with `expires_at = now + new.ttl_secs`. Returns the created
    /// [`Invite`] (which carries no token).
    ///
    /// # Errors
    /// [`InviteError::Backend`] on a store failure (including an unknown `created_by` user — a
    /// foreign-key violation).
    fn create_invite(&self, new: &NewInvite, token_hash: &str) -> Result<Invite, InviteError>;

    /// Look up an invite by its `token_hash` (no lifecycle filtering — returns it whatever its
    /// state). `Ok(None)` when no invite has that digest. Mostly a test/inspection helper; the redeem
    /// path uses [`Self::accept_invite`], which checks the lifecycle atomically.
    ///
    /// # Errors
    /// [`InviteError::Backend`] on a store failure.
    fn find_invite_by_token_hash(&self, token_hash: &str) -> Result<Option<Invite>, InviteError>;

    /// Look up an invite by id (for an operator inspecting / revoking it). `Ok(None)` when absent.
    ///
    /// # Errors
    /// [`InviteError::Backend`] on a store failure.
    fn find_invite(&self, id: InviteId) -> Result<Option<Invite>, InviteError>;

    /// REDEEM `token_hash` (the digest of the presented one-time token): look the invite up, verify
    /// the hash in constant time, reject an expired / consumed / revoked invite, create the
    /// redeemer's local identity (`users` + a `local` `accounts` row with `password_hash`, reusing
    /// t45's sign-up shape), insert the `memberships` row, and mark the invite consumed — ALL in one
    /// transaction. Single-use: a second redeem of the same token fails
    /// [`InviteError::AlreadyConsumed`]. Returns the created [`Redemption`].
    ///
    /// `email` is the address the redeemer signs up with (the invite's `email`, if any, is advisory —
    /// the binary may pin redeem to it). `password_hash` is already derived by the caller (the
    /// plaintext is zeroized before this is called); only the [`PasswordHash`] crosses the seam.
    ///
    /// # Errors
    /// - [`InviteError::NotFound`] if no invite matches the digest (a wrong/forged token);
    /// - [`InviteError::Expired`] / [`InviteError::AlreadyConsumed`] / [`InviteError::Revoked`] on a
    ///   replay of a spent invite;
    /// - [`InviteError::DuplicateEmail`] / [`InviteError::DuplicateAccount`] if the redeemer's email
    ///   already has a user/account;
    /// - [`InviteError::Backend`] on a store failure.
    ///
    /// On ANY error the transaction rolls back — no half-created user, and the invite stays unspent
    /// (so a transient failure does not burn a still-valid invite).
    fn accept_invite(
        &self,
        token_hash: &str,
        email: &str,
        password_hash: &PasswordHash,
    ) -> Result<Redemption, InviteError>;

    /// Revoke the pending invite `id` (set `revoked_at = now`), so its token can no longer redeem.
    /// Returns `true` if a still-revocable invite was revoked, `false` if none matched or it was
    /// already consumed/revoked (idempotent, not an error).
    ///
    /// # Errors
    /// [`InviteError::Backend`] on a store failure.
    fn revoke_invite(&self, id: InviteId) -> Result<bool, InviteError>;

    /// List the memberships of `user_id` (host + any project rows), ordered by id. Empty when the
    /// user belongs to nothing yet.
    ///
    /// # Errors
    /// [`InviteError::Backend`] on a store failure.
    fn list_memberships(&self, user_id: UserId) -> Result<Vec<Membership>, InviteError>;
}

/// A structured, secret-free invite-store error (AI-consumable). No variant carries a token or a
/// hash; `Backend` describes the failing *operation* only. The three replay variants
/// ([`Expired`]/[`AlreadyConsumed`]/[`Revoked`]) are distinct so a caller (and an audit record) can
/// tell *why* a redeem was refused without leaking the token.
///
/// [`Expired`]: InviteError::Expired
/// [`AlreadyConsumed`]: InviteError::AlreadyConsumed
/// [`Revoked`]: InviteError::Revoked
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum InviteError {
    /// No invite matches the presented token digest (a wrong, forged, or already-deleted token).
    #[error("no invite matches that token")]
    NotFound,
    /// The invite is past its `expires_at` — a redeem is refused.
    #[error("this invite has expired")]
    Expired,
    /// The invite was already redeemed — single-use; a replay is refused.
    #[error("this invite has already been redeemed")]
    AlreadyConsumed,
    /// The invite was revoked by an operator before use — a redeem is refused.
    #[error("this invite has been revoked")]
    Revoked,
    /// The redeemer's email already has a user (the joining identity must be new).
    #[error("a user already exists for that email")]
    DuplicateEmail,
    /// The redeemer's `(provider, subject)` local account already exists.
    #[error("an account already exists for that provider and subject")]
    DuplicateAccount,
    /// A backend failure (I/O, decode, transaction) — the message describes the operation, never a
    /// secret.
    #[error("invite store backend error: {0}")]
    Backend(String),
}
