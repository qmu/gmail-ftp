//! The owned session model: [`Session`] and its handle [`SessionId`].
//!
//! These are plain owned DTOs — no live token, no vendor type. **Crucially, [`Session`] does NOT
//! carry the plaintext token**: the token's `sha256_hex` IS the session's handle ([`SessionId`]),
//! and the plaintext exists only in the [`crate::SessionToken`]-wrapped cookie value at issue. A
//! token hash is preimage-resistant — knowing it does not let you forge the cookie — so it is safe
//! to carry/compare/store; the live token is not.

use core::fmt;

use crate::UserId;

/// A session's stable handle: the **at-rest hash** of its opaque token (`sha256_hex(token)`), which
/// is also the `sessions.token_hash` primary key. A NEW type so a session handle is never confused
/// for a raw string in a signature. NOT the live token — the hash cannot be reversed to it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SessionId(pub String);

impl SessionId {
    /// Borrow the hash as a `&str` (for a store lookup / a `rotated_from` link).
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// One server-side session: one row of the System-DB `sessions` table. It binds a request to a t45
/// [`UserId`] (authentication state) and carries its absolute [`Session::expires_at`]. Holds no live
/// token — the [`Session::id`] is the token's hash, not the token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    /// The session handle — the at-rest token hash (the `token_hash` primary key).
    pub id: SessionId,
    /// The authenticated human this session proves (the t45 `users` row). Proves WHO, not
    /// WHAT-may-you-do — authorization is M2 (decision §4.1).
    pub user_id: UserId,
    /// When the session was created (RFC 3339 UTC; the store stamps it).
    pub created_at: String,
    /// The absolute expiry (RFC 3339 UTC). A lookup past this instant treats the session as absent.
    pub expires_at: String,
    /// The PRIOR session's hash when this row was minted by a [`crate::SessionStore::rotate`]
    /// (sign-in / consent) — an audit breadcrumb. `None` for a freshly created session.
    pub rotated_from: Option<SessionId>,
}

impl Session {
    /// Whether this session is expired at `now` (an RFC 3339 UTC instant in the SAME fixed-width
    /// `YYYY-MM-DDTHH:MM:SSZ` form the store stamps). The store also enforces expiry in SQL; this
    /// pure check mirrors it for callers/tests. Fixed-width RFC 3339 UTC sorts lexicographically,
    /// so a string compare is a correct chronological compare here.
    #[must_use]
    pub fn is_expired(&self, now: &str) -> bool {
        now >= self.expires_at.as_str()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_id_displays_and_borrows_the_hash() {
        let id = SessionId("abc123".to_string());
        assert_eq!(id.to_string(), "abc123");
        assert_eq!(id.as_str(), "abc123");
    }

    #[test]
    fn is_expired_compares_rfc3339_lexically() {
        let s = Session {
            id: SessionId("h".into()),
            user_id: UserId(1),
            created_at: "2026-06-28T00:00:00Z".into(),
            expires_at: "2026-06-29T00:00:00Z".into(),
            rotated_from: None,
        };
        assert!(!s.is_expired("2026-06-28T23:59:59Z"), "before expiry: live");
        assert!(s.is_expired("2026-06-29T00:00:00Z"), "at expiry: expired");
        assert!(
            s.is_expired("2026-06-30T12:00:00Z"),
            "after expiry: expired"
        );
    }
}
