//! The consumer-side [`IdentityStore`] trait + its structured [`IdentityError`].
//!
//! This is the **seam**: the trait is owned-DTO only and carries no SQLite/rusqlite type, so the
//! domain stays a leaf. The concrete implementation (a `rusqlite` store over the System DB) is
//! INJECTED by the binary — it lives in `qfs-store` (`SqliteIdentityStore`), the one crate that owns
//! a real DB connection. The dep-direction guard keeps that I/O out of this crate.
//!
//! Sign-up ([`IdentityStore::signup_local`]) is **one transaction** inserting a `users` row + a
//! `local` `accounts` row; a duplicate email or `(provider, subject)` returns a structured error, not
//! a panic. The plaintext password never reaches this trait — the binary hashes it (the plaintext is
//! zeroized) and hands in a [`PasswordHash`]. The lone method that needs the live password,
//! [`IdentityStore::verify_password`], takes a [`Secret`] and returns only a bool.

use crate::model::{Account, SoleUser, User, UserId};
use crate::password::PasswordHash;
use crate::Secret;

/// The identity persistence surface drivers/CLI call. Consumer-side: it trades only in owned identity
/// DTOs + the redacting [`Secret`]/[`PasswordHash`]. The SQLite impl is injected (see module docs).
///
/// `Send + Sync` so a built store can be shared, mirroring the `Secrets` handle.
pub trait IdentityStore: Send + Sync {
    /// Create a bare `users` row for `email` (no account linked). Most callers want the atomic
    /// [`Self::signup_local`]; this is the lower-level primitive.
    ///
    /// # Errors
    /// [`IdentityError::DuplicateEmail`] if the email is already taken; [`IdentityError::Backend`]
    /// on a store failure.
    fn create_user(&self, email: &str) -> Result<User, IdentityError>;

    /// Look up a user by their primary email. `Ok(None)` when no such user exists (not an error).
    ///
    /// # Errors
    /// [`IdentityError::Backend`] on a store failure.
    fn find_user_by_email(&self, email: &str) -> Result<Option<User>, IdentityError>;

    /// Look up a user by their stable internal id. `Ok(None)` when no such user exists (not an
    /// error). The federated-login linker ([`crate::link_or_create_from_oidc`]) uses this to resolve
    /// the [`User`] an already-linked `(provider, subject)` account points at.
    ///
    /// # Errors
    /// [`IdentityError::Backend`] on a store failure.
    fn find_user_by_id(&self, id: UserId) -> Result<Option<User>, IdentityError>;

    /// Link an account to `user_id`. `password_hash` is `Some` for a `local` provider and `None` for
    /// an OAuth/OIDC provider (which carries no password). The `(provider, subject)` pair is unique.
    ///
    /// # Errors
    /// [`IdentityError::DuplicateAccount`] if `(provider, subject)` already exists;
    /// [`IdentityError::Backend`] on a store failure.
    fn create_account(
        &self,
        user_id: UserId,
        provider: &str,
        subject: &str,
        password_hash: Option<&PasswordHash>,
    ) -> Result<Account, IdentityError>;

    /// Look up an account by `(provider, subject)`. `Ok(None)` when none exists.
    ///
    /// # Errors
    /// [`IdentityError::Backend`] on a store failure.
    fn find_account(&self, provider: &str, subject: &str)
        -> Result<Option<Account>, IdentityError>;

    /// Verify `candidate` against the stored hash of the `(provider, subject)` account, in constant
    /// time. Returns `false` (never an error, never a leak) when the account is absent, has no
    /// password (an OAuth account), or the password is wrong. The hash never leaves the store.
    ///
    /// # Errors
    /// [`IdentityError::Backend`] on a store failure (a read error — distinct from a wrong password,
    /// which is a plain `Ok(false)`).
    fn verify_password(
        &self,
        provider: &str,
        subject: &str,
        candidate: &Secret,
    ) -> Result<bool, IdentityError>;

    /// Atomic local sign-up: insert a `users` row + a `local` `accounts` row (subject = email) in ONE
    /// transaction, returning the created [`User`]. The password is already hashed by the caller (the
    /// plaintext is zeroized before this is called); only the [`PasswordHash`] crosses the seam.
    ///
    /// # Errors
    /// [`IdentityError::DuplicateEmail`] if the email is taken; [`IdentityError::DuplicateAccount`]
    /// if the local subject is taken; [`IdentityError::Backend`] on a store failure. On any error the
    /// transaction rolls back — no half-created user.
    fn signup_local(
        &self,
        email: &str,
        password_hash: &PasswordHash,
    ) -> Result<User, IdentityError>;

    /// Resolve the *sole* user for a session-less `whoami` (sessions are t46): [`SoleUser::One`] iff
    /// exactly one user exists, else [`SoleUser::None`]/[`SoleUser::Many`].
    ///
    /// # Errors
    /// [`IdentityError::Backend`] on a store failure.
    fn sole_user(&self) -> Result<SoleUser, IdentityError>;
}

/// A structured, secret-free identity-store error (AI-consumable). No variant carries a password or a
/// hash; `Backend` carries a description of the failing *operation* only.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum IdentityError {
    /// A user already exists with this primary email.
    #[error("a user already exists for that email")]
    DuplicateEmail,
    /// An account already exists for this `(provider, subject)` pair.
    #[error("an account already exists for that provider and subject")]
    DuplicateAccount,
    /// A backend failure (I/O, decode, transaction) — the message describes the operation, never a
    /// secret.
    #[error("identity store backend error: {0}")]
    Backend(String),
}
