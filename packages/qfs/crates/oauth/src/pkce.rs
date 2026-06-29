//! PKCE (RFC 7636) verification — **`S256` only** (RFD §10: PKCE is mandatory, `plain` is refused).
//!
//! The authorization request commits the client to a `code_challenge`; the token request later
//! presents the `code_verifier`. This module verifies the binding the auth-code grant relies on:
//! `BASE64URL_NOPAD(SHA256(code_verifier)) == code_challenge`, compared in CONSTANT TIME so the check
//! never short-circuits on the first mismatching byte. It reuses the workspace's single SHA-256 +
//! constant-time primitives (`qfs-crypto-core`) and the crate's base64url-no-pad encoder — no new
//! crypto, no `plain` fallback.

use qfs_crypto_core::{constant_time_eq, sha256};

use crate::key::b64url_encode;

/// The only PKCE code-challenge method this AS accepts. `plain` is deliberately UNSUPPORTED (it
/// offers no protection against a leaked authorization code), so a request that omits `S256` is
/// rejected upstream rather than silently downgraded.
pub const PKCE_METHOD_S256: &str = "S256";

/// Whether `code_verifier` satisfies the `S256` `code_challenge`:
/// `BASE64URL_NOPAD(SHA256(code_verifier)) == code_challenge`, with a constant-time compare.
///
/// An empty verifier or challenge cannot match a real challenge (the SHA-256 of the empty string is
/// a fixed non-empty 43-char base64url value), so the caller's "verifier required" check and this
/// equality together reject the missing-verifier case.
#[must_use]
pub fn verify_pkce_s256(code_verifier: &str, code_challenge: &str) -> bool {
    let computed = pkce_challenge_s256(code_verifier);
    constant_time_eq(computed.as_bytes(), code_challenge.as_bytes())
}

/// The `S256` `code_challenge` for a `code_verifier`: `BASE64URL_NOPAD(SHA256(code_verifier))`. The
/// pure derivation a client performs; exposed so the binary (and tests) can build a challenge from a
/// verifier without re-vendoring SHA-256 + base64url.
#[must_use]
pub fn pkce_challenge_s256(code_verifier: &str) -> String {
    b64url_encode(&sha256(code_verifier.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The canonical RFC 7636 Appendix B test vector: this verifier hashes to this challenge.
    const RFC7636_VERIFIER: &str = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
    const RFC7636_CHALLENGE: &str = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";

    #[test]
    fn the_rfc7636_appendix_b_vector_verifies() {
        assert!(verify_pkce_s256(RFC7636_VERIFIER, RFC7636_CHALLENGE));
    }

    #[test]
    fn a_wrong_verifier_is_rejected() {
        assert!(!verify_pkce_s256("not-the-verifier", RFC7636_CHALLENGE));
        // A one-char-off verifier (avalanche: the digest is entirely different).
        assert!(!verify_pkce_s256(
            "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXX",
            RFC7636_CHALLENGE
        ));
    }

    #[test]
    fn an_empty_verifier_or_challenge_never_matches_a_real_challenge() {
        assert!(!verify_pkce_s256("", RFC7636_CHALLENGE));
        assert!(!verify_pkce_s256(RFC7636_VERIFIER, ""));
    }

    #[test]
    fn the_round_trip_holds_for_an_arbitrary_verifier() {
        // Independently compute the challenge for a fresh verifier and confirm it verifies.
        let verifier = "a-high-entropy-code-verifier-0123456789-abcdefghij";
        let challenge = b64url_encode(&sha256(verifier.as_bytes()));
        assert!(verify_pkce_s256(verifier, &challenge));
    }
}
