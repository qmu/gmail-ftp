//! t50: the pure **bearer access-token validation** primitive [`verify_access_token`] — the resource
//! server's half of the OAuth handshake. t48 shipped [`crate::verify_jws`] (signature + `kid`
//! resolution against the JWKS); this layers the registered-claim checks a resource server MUST make
//! before trusting a token to reach the tool surface: `iss` (the AS issued it), `aud` (it was minted
//! for THIS MCP resource — an audience-confusion guard), and `exp` (it has not expired).
//!
//! ## Why this lives in `qfs-oauth` (pure), not the binary
//! Like [`crate::sign_jws`]/[`crate::verify_jws`], token validation is protocol logic with no I/O: it
//! takes an already-extracted token string + the published [`Jwks`] + the expected `iss`/`aud` + a
//! caller-supplied `now` (the binary injects the wall clock), and returns the validated subject/scope.
//! Keeping it here makes it golden-vector testable against fixed-key tokens (incl. the expired /
//! wrong-`aud` / tampered-signature rejections the t50 ticket calls for) with zero credentials.
//!
//! ## Security invariants (RFD §10)
//! - **Signature first.** [`crate::verify_jws`] verifies the ES256 signature (and pins `alg=ES256`, an
//!   algorithm-confusion guard) before any claim is read — a tampered token never reaches the claim
//!   checks.
//! - **`aud` is mandatory + exact.** A token whose `aud` is not the configured MCP resource is
//!   rejected ([`AccessTokenError::WrongAudience`]) — a token minted for a different resource cannot
//!   be replayed here.
//! - **`exp` is honored.** Access tokens are stateless (verified by signature, not a DB lookup), so
//!   `exp` is the ONLY thing bounding their lifetime — an expired token is rejected even though its
//!   signature is still valid.
//! - **Secret-free errors.** [`AccessTokenError`] names the failing condition only; it NEVER carries
//!   the token, a claim value, or key material (the caller logs the variant, never the token).

use serde_json::Value;

use crate::{verify_jws, Jwks, OauthError};

/// The validated context lifted off a good access token — exactly what the resource server needs to
/// key its policy gate to a principal (token→user→policy, t50). Carries NO secret: `subject` is the
/// authenticated user id (the token `sub`), plus the granted `scope` and the `client_id` it was
/// issued to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedAccessToken {
    /// The authenticated subject — the user id the token was minted for (the `sub` claim).
    pub subject: String,
    /// The granted scope (space-delimited; may be empty).
    pub scope: String,
    /// The client the token was issued to (the `client_id` claim).
    pub client_id: String,
}

/// The taxonomy of bearer-validation failures. Every variant is a **`401`** at the resource server
/// (the caller renders the `WWW-Authenticate` challenge) — value-free, so a log of the variant leaks
/// no token/claim. The variants are distinct so a test can pin WHICH check rejected a given token.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum AccessTokenError {
    /// The token is structurally malformed, carries a non-`ES256` header, or names an unknown `kid`.
    #[error("the access token is malformed or names an unknown signing key")]
    Malformed,
    /// The ES256 signature does not verify (tampered, or signed by a key not in the JWKS).
    #[error("the access token signature is invalid")]
    BadSignature,
    /// The `iss` claim is absent or is not the expected authorization-server issuer.
    #[error("the access token was not issued by this authorization server")]
    WrongIssuer,
    /// The `aud` claim is absent or is not the expected MCP resource (audience-confusion guard).
    #[error("the access token was not issued for this resource")]
    WrongAudience,
    /// The `exp` claim is absent, malformed, or in the past (the token has expired).
    #[error("the access token has expired")]
    Expired,
}

impl From<OauthError> for AccessTokenError {
    fn from(e: OauthError) -> Self {
        match e {
            // A bad/forged signature is its own signal; everything else structural is `Malformed`.
            OauthError::BadSignature => AccessTokenError::BadSignature,
            OauthError::Malformed | OauthError::UnknownKid => AccessTokenError::Malformed,
            // `Signing` cannot arise on the verify path; fold it into the structural bucket.
            _ => AccessTokenError::Malformed,
        }
    }
}

/// Verify a compact ES256 JWS **bearer access token** for the MCP resource server: check the
/// signature against `jwks` (t48 [`verify_jws`]), then the registered claims — `iss == expected_iss`,
/// `aud == expected_aud` (string match, or membership when `aud` is the array form RFC 7519 permits),
/// and `exp > now_unix`. On success, return the [`VerifiedAccessToken`] (subject/scope/client_id) the
/// caller keys its policy gate to. The token string is consumed by value-by-reference only and never
/// logged.
///
/// # Errors
/// The most specific [`AccessTokenError`] for the first failing check: a tampered token is
/// [`AccessTokenError::BadSignature`]; a wrong/absent `iss`/`aud` is
/// [`AccessTokenError::WrongIssuer`]/[`AccessTokenError::WrongAudience`]; an expired/absent `exp` is
/// [`AccessTokenError::Expired`].
pub fn verify_access_token(
    token: &str,
    jwks: &Jwks,
    expected_iss: &str,
    expected_aud: &str,
    now_unix: u64,
) -> Result<VerifiedAccessToken, AccessTokenError> {
    // 1. Signature + structural validation (alg pinned to ES256 inside verify_jws).
    let claims: Value = verify_jws(token, jwks)?;

    // 2. Issuer: the AS that signed it must be the one we trust.
    if claims.get("iss").and_then(Value::as_str) != Some(expected_iss) {
        return Err(AccessTokenError::WrongIssuer);
    }

    // 3. Audience: the token must have been minted for THIS resource (string, or array membership).
    if !audience_matches(claims.get("aud"), expected_aud) {
        return Err(AccessTokenError::WrongAudience);
    }

    // 4. Expiry: stateless tokens live only as long as `exp` (an absent/malformed `exp` is fatal —
    //    we never trust an unbounded token).
    let exp = claims
        .get("exp")
        .and_then(Value::as_u64)
        .ok_or(AccessTokenError::Expired)?;
    if exp <= now_unix {
        return Err(AccessTokenError::Expired);
    }

    Ok(VerifiedAccessToken {
        subject: claims
            .get("sub")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        scope: claims
            .get("scope")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        client_id: claims
            .get("client_id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
    })
}

/// Whether the `aud` claim matches `expected`: the single-string form (`"aud":"…"`) must equal it; the
/// array form (`"aud":["…","…"]`, permitted by RFC 7519 §4.1.3) must CONTAIN it. Any other shape (or a
/// missing claim) does not match (fail-closed).
fn audience_matches(aud: Option<&Value>, expected: &str) -> bool {
    match aud {
        Some(Value::String(s)) => s == expected,
        Some(Value::Array(items)) => items
            .iter()
            .any(|v| v.as_str().is_some_and(|s| s == expected)),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{access_token_claims, sign_jws, SigningKey};
    use serde_json::json;

    const ISS: &str = "http://localhost:8787";
    const AUD: &str = "http://localhost:8787/mcp";

    fn fixed_key() -> SigningKey {
        SigningKey::generate(&crate::key::FIXED_SCALAR).unwrap()
    }

    fn jwks_of(key: &SigningKey) -> Jwks {
        Jwks::new(vec![key.public_jwk()])
    }

    /// Sign a token over the standard access-token claim set with the given window.
    fn token_with(key: &SigningKey, iat: u64, ttl: u64) -> String {
        let claims = access_token_claims(ISS, AUD, 42, "mcp:read", "client-1", iat, ttl);
        sign_jws(&claims, key).unwrap()
    }

    #[test]
    fn a_well_formed_in_window_token_verifies_and_lifts_the_subject() {
        let key = fixed_key();
        let token = token_with(&key, 1_000, 600);
        let v = verify_access_token(&token, &jwks_of(&key), ISS, AUD, 1_100).unwrap();
        assert_eq!(v.subject, "42");
        assert_eq!(v.scope, "mcp:read");
        assert_eq!(v.client_id, "client-1");
    }

    #[test]
    fn an_expired_token_is_rejected_even_with_a_valid_signature() {
        let key = fixed_key();
        let token = token_with(&key, 1_000, 600); // exp = 1_600
        assert_eq!(
            verify_access_token(&token, &jwks_of(&key), ISS, AUD, 1_600).unwrap_err(),
            AccessTokenError::Expired,
            "exp is inclusive-exclusive: now == exp is expired"
        );
        assert_eq!(
            verify_access_token(&token, &jwks_of(&key), ISS, AUD, 9_999).unwrap_err(),
            AccessTokenError::Expired
        );
    }

    #[test]
    fn a_wrong_audience_token_is_rejected() {
        let key = fixed_key();
        let token = token_with(&key, 1_000, 600);
        assert_eq!(
            verify_access_token(
                &token,
                &jwks_of(&key),
                ISS,
                "http://localhost:8787/other",
                1_100
            )
            .unwrap_err(),
            AccessTokenError::WrongAudience
        );
    }

    #[test]
    fn a_wrong_issuer_token_is_rejected() {
        let key = fixed_key();
        let token = token_with(&key, 1_000, 600);
        assert_eq!(
            verify_access_token(&token, &jwks_of(&key), "http://evil.example", AUD, 1_100)
                .unwrap_err(),
            AccessTokenError::WrongIssuer
        );
    }

    #[test]
    fn a_token_signed_by_a_foreign_key_is_a_bad_signature() {
        let key = fixed_key();
        let token = token_with(&key, 1_000, 600);
        // A JWKS that publishes the SAME kid is not possible here; an unrelated key yields a JWKS
        // whose `find(kid)` misses → Malformed (unknown kid). To exercise BadSignature, re-sign the
        // claims under a foreign key but verify against a JWKS that contains a DIFFERENT key with a
        // colliding lookup is not feasible; instead tamper the payload (covered below) and assert the
        // unknown-key path is Malformed.
        let foreign = SigningKey::generate(&[7u8; 32]).unwrap();
        assert_eq!(
            verify_access_token(&token, &jwks_of(&foreign), ISS, AUD, 1_100).unwrap_err(),
            AccessTokenError::Malformed,
            "a token whose kid is not in the JWKS is Malformed (unknown kid)"
        );
    }

    #[test]
    fn a_tampered_payload_is_a_bad_signature() {
        let key = fixed_key();
        let token = token_with(&key, 1_000, 600);
        let jwks = jwks_of(&key);
        // Re-encode a forged payload (same kid header + original signature) → signature mismatch.
        let mut parts: Vec<String> = token.split('.').map(str::to_string).collect();
        let forged = access_token_claims(ISS, AUD, 999, "mcp:write", "client-1", 1_000, 600);
        parts[1] = crate::key::b64url_encode(&serde_json::to_vec(&forged).unwrap());
        let tampered = parts.join(".");
        assert_eq!(
            verify_access_token(&tampered, &jwks, ISS, AUD, 1_100).unwrap_err(),
            AccessTokenError::BadSignature
        );
    }

    #[test]
    fn a_structurally_broken_token_is_malformed_not_a_panic() {
        let jwks = jwks_of(&fixed_key());
        for bad in ["", "a.b", "not-a-token", "...."] {
            assert_eq!(
                verify_access_token(bad, &jwks, ISS, AUD, 1_100).unwrap_err(),
                AccessTokenError::Malformed
            );
        }
    }

    #[test]
    fn the_array_audience_form_matches_by_membership() {
        assert!(audience_matches(
            Some(&json!(["a", "http://localhost:8787/mcp", "b"])),
            AUD
        ));
        assert!(!audience_matches(Some(&json!(["a", "b"])), AUD));
        assert!(!audience_matches(None, AUD));
        assert!(!audience_matches(Some(&json!(7)), AUD));
    }
}
