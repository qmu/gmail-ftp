//! The consumer-side [`SessionStore`] trait + its structured [`SessionError`].
//!
//! This is the **seam**: the trait carries no SQLite/rusqlite type, so the domain stays a leaf. The
//! concrete implementation (a `rusqlite` store over the System DB) is INJECTED by the binary — it
//! lives in `qfs-store` (`SqliteSessionStore`), the one crate that owns a real DB connection (the
//! same split t45's `IdentityStore` uses).
//!
//! The trait trades only in TOKEN HASHES, never the plaintext token: the binary hashes the opaque
//! token ([`crate::SessionToken::hash`]) and hands the hash across this seam, so a raw token never
//! reaches the store. Expiry is enforced on every [`SessionStore::lookup`]; an expired session is
//! treated as absent and lazily reaped.

use crate::{Session, UserId};

/// The session persistence surface the binary's authenticated face calls. Consumer-side: it trades
/// only in owned [`Session`] DTOs + token HASHES (`&str`). The SQLite impl is injected (module docs).
///
/// `Send + Sync` so a built store can be shared across the listener's connection tasks.
pub trait SessionStore: Send + Sync {
    /// Create a session for `user_id` keyed by `token_hash` (the `sha256_hex` of a freshly minted
    /// token), expiring `ttl_secs` from now. Returns the created [`Session`]. The raw token is NOT
    /// passed — the caller keeps it (to put in the cookie) and hands only its hash here.
    ///
    /// # Errors
    /// [`SessionError::Backend`] on a store failure (including an unknown `user_id` — a foreign-key
    /// violation, since a session must belong to a real t45 user).
    fn create(
        &self,
        user_id: UserId,
        token_hash: &str,
        ttl_secs: i64,
    ) -> Result<Session, SessionError>;

    /// Look up the LIVE session for `token_hash` (the hash of the presented cookie token).
    /// `Ok(None)` when no such session exists OR it has expired (expired rows are lazily reaped and
    /// reported as absent — never an error). The verification is constant-time on the stored hash.
    ///
    /// # Errors
    /// [`SessionError::Backend`] on a store failure (a read error — distinct from "no session",
    /// which is a plain `Ok(None)`).
    fn lookup(&self, token_hash: &str) -> Result<Option<Session>, SessionError>;

    /// Rotate the session identified by `old_token_hash`: mint a NEW session for the SAME user keyed
    /// by `new_token_hash` (expiring `ttl_secs` from now) with `rotated_from` set to the old hash,
    /// and expire (delete) the old row — atomically. Rotation on a privilege-relevant event
    /// (sign-in, later consent) limits session fixation. Returns the NEW [`Session`].
    ///
    /// # Errors
    /// [`SessionError::NotFound`] if no live session exists for `old_token_hash`;
    /// [`SessionError::Backend`] on a store failure. On any error the transaction rolls back — the
    /// old session is left intact and no new one is created.
    fn rotate(
        &self,
        old_token_hash: &str,
        new_token_hash: &str,
        ttl_secs: i64,
    ) -> Result<Session, SessionError>;

    /// Revoke (delete) the session for `token_hash` (sign-out). Returns `true` if a row was removed,
    /// `false` if none matched (already gone / never existed) — idempotent, not an error.
    ///
    /// # Errors
    /// [`SessionError::Backend`] on a store failure.
    fn revoke(&self, token_hash: &str) -> Result<bool, SessionError>;
}

/// A structured, secret-free session-store error (AI-consumable). No variant carries a token or a
/// hash; `Backend` describes the failing *operation* only.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SessionError {
    /// No live session exists for the given token hash (used by [`SessionStore::rotate`], which
    /// needs an existing session to rotate FROM).
    #[error("no live session for that token")]
    NotFound,
    /// A backend failure (I/O, decode, transaction) — the message describes the operation, never a
    /// secret.
    #[error("session store backend error: {0}")]
    Backend(String),
}
