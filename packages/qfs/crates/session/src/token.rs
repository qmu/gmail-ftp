//! The opaque session [`SessionToken`] + the at-rest token hash.
//!
//! The token is the bearer secret the cookie carries; it is high-entropy and generated from a
//! CSPRNG **in the binary leaf** (OS entropy injected into [`SessionToken::from_entropy`]) so this
//! core stays deterministic and testable (no `rand`/`getrandom` edge here). It is wrapped in the
//! redacting [`Secret`] in transit, and only its `sha256_hex` is ever persisted.

use qfs_crypto_core::{constant_time_eq, hex_lower, sha256_hex};

use crate::Secret;

/// An opaque, high-entropy session token. Carries the redacting [`Secret`] in transit (so it is
/// redacted in every `{:?}`/log) and exposes exactly two doors: [`SessionToken::hash`] (the value
/// persisted at rest) and [`SessionToken::reveal`] (the one-time cookie value). No `Clone` — a
/// token is moved, never silently duplicated.
pub struct SessionToken(Secret);

impl SessionToken {
    /// Build a token from injected OS entropy (the binary passes CSPRNG bytes). The bytes are
    /// lowercase-hex encoded into the opaque token STRING (URL/cookie-safe; 32 bytes → 64 hex
    /// chars), wrapped in the redacting [`Secret`]. Deterministic in `entropy` so the core is
    /// testable without owning a CSPRNG.
    #[must_use]
    pub fn from_entropy(entropy: &[u8]) -> Self {
        Self(Secret::from(hex_lower(entropy)))
    }

    /// Wrap a token value PRESENTED by a caller (the cookie value off the wire) for hashing/lookup.
    /// The value is treated as opaque; only its hash is ever compared against the store.
    #[must_use]
    pub fn from_cookie_value(value: &str) -> Self {
        Self(Secret::from(value))
    }

    /// The `sha256_hex` of the token — the value stored at rest (the `sessions.token_hash` key) and
    /// the only representation that ever touches the DB. Preimage-resistant: a stored hash does not
    /// yield the token.
    #[must_use]
    pub fn hash(&self) -> String {
        sha256_hex(self.0.expose())
    }

    /// Whether this token's hash equals `stored_hash`, compared in **constant time** (RFD §10 — the
    /// verification never short-circuits on the first mismatching byte). The store uses an indexed
    /// `token_hash` lookup for the row fetch; this is the defense-in-depth equality check on the
    /// fetched hash.
    #[must_use]
    pub fn matches_hash(&self, stored_hash: &str) -> bool {
        constant_time_eq(self.hash().as_bytes(), stored_hash.as_bytes())
    }

    /// The redacting [`Secret`] wrapping the plaintext token — the ONE door used to put the token on
    /// the wire (the cookie value), exactly once at issue. Named `reveal` so every wire-exposure of
    /// a live token is explicit and grep-able.
    #[must_use]
    pub fn reveal(&self) -> &Secret {
        &self.0
    }
}

/// The at-rest hash of a presented token VALUE (the cookie string) — `sha256_hex(value)`. The free
/// function the request-authentication path uses to turn an incoming cookie token into the lookup
/// key without constructing a [`SessionToken`]. Identical to `SessionToken::from_cookie_value(v)
/// .hash()`.
#[must_use]
pub fn token_hash(value: &str) -> String {
    sha256_hex(value.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_entropy_hex_encodes_and_hash_is_stable() {
        let t = SessionToken::from_entropy(&[0x00, 0x0f, 0xff]);
        // The token value is the lowercase hex of the entropy.
        assert_eq!(t.reveal().expose_str(), Some("000fff"));
        // The at-rest hash is sha256_hex of that token string, and is deterministic.
        let h = t.hash();
        assert_eq!(h.len(), 64, "sha256_hex is 64 hex chars");
        assert_eq!(h, SessionToken::from_entropy(&[0x00, 0x0f, 0xff]).hash());
    }

    #[test]
    fn token_hash_free_fn_matches_from_cookie_value() {
        let presented = "000fff";
        assert_eq!(
            token_hash(presented),
            SessionToken::from_cookie_value(presented).hash()
        );
        // And a presented token whose value equals an issued token's value hashes identically — the
        // round-trip the cookie carries (issue → wire → present).
        let issued = SessionToken::from_entropy(&[0x00, 0x0f, 0xff]);
        assert_eq!(token_hash("000fff"), issued.hash());
    }

    #[test]
    fn matches_hash_is_constant_time_eq() {
        let t = SessionToken::from_entropy(&[1, 2, 3, 4]);
        assert!(t.matches_hash(&t.hash()));
        assert!(!t.matches_hash("deadbeef"));
        assert!(!t.matches_hash(""));
    }

    #[test]
    fn token_debug_redacts_the_value() {
        let t = SessionToken::from_entropy(&[0xab, 0xcd]);
        // The wrapped Secret redacts; the token value never appears in a debug dump.
        let dumped = format!("{:?}", t.reveal());
        assert!(
            !dumped.contains("abcd"),
            "token value must not appear: {dumped}"
        );
        assert!(dumped.contains("redacted"));
    }
}
