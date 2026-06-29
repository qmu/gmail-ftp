//! t56 (roadmap **M5**): the **relying-party (RP) ID-token verification** primitive
//! [`verify_id_token`] — the half of upstream OIDC federation that makes an UPSTREAM IdP's ID token
//! trustworthy before it is allowed to name a local identity.
//!
//! t48–t50 made qfs its OWN authorization server (it MINTS + verifies its own ES256 access tokens —
//! see [`crate::verify_access_token`]). t56 makes qfs ALSO a CLIENT of an upstream AS for the
//! human-login leg: a user signs in through qfs Cloud / Google Workspace / Entra / Okta / a generic
//! OIDC provider, and qfs must verify the ID token that comes back. This module is that verifier.
//!
//! ## Why this lives in `qfs-oauth` (pure), not the binary
//! Like [`crate::verify_jws`] / [`crate::verify_access_token`], ID-token verification is protocol
//! logic with no I/O: it takes the compact JWS, the upstream's already-fetched [`Jwks`], the expected
//! `iss`/`aud`/`nonce`, and a caller-supplied `now`, and returns the validated [`IdTokenClaims`]. The
//! only impure steps of the RP flow — fetching the upstream discovery document + its JWKS, and the
//! browser redirect/code exchange — stay in the binary (the documented native seam). Keeping the
//! verification here makes the security-critical checks golden-vector testable against a FIXTURE
//! upstream (a locally-minted ES256 token + a fixture JWKS) with zero network and zero credentials.
//!
//! ## Security invariants (RFD §10 — verify, never trust)
//! An upstream ID token is UNTRUSTED until ALL of the following pass, checked in this order so a
//! forged token never reaches the claim reads:
//! - **Signature first.** The ES256 signature verifies against the *upstream's* JWKS (a key the
//!   upstream published), with `alg` pinned to `ES256` inside [`crate::verify_jws`] (an
//!   algorithm-confusion guard). A tampered/forged token is [`IdTokenError::BadSignature`].
//! - **`iss` is the configured upstream issuer** — a token from a different IdP cannot be replayed
//!   ([`IdTokenError::WrongIssuer`]).
//! - **`aud` contains OUR client id** — a token minted for a different RP cannot be replayed
//!   ([`IdTokenError::WrongAudience`]).
//! - **`exp` is in the future** — an expired token is rejected even with a valid signature
//!   ([`IdTokenError::Expired`]).
//! - **`nonce` equals the one we minted** for THIS login, compared in CONSTANT TIME — the replay
//!   defense binding the ID token to the exact authorization request we started
//!   ([`IdTokenError::NonceMismatch`]).
//!
//! Only after every check passes are the identity claims (`sub`, `email`, `email_verified`) lifted —
//! and `email_verified` is then honored by the linker (`qfs-identity`), which refuses to link an
//! unverified email. This module does the CRYPTO + protocol checks; the account-linking POLICY is
//! `qfs_identity::link_or_create_from_oidc`.
//!
//! ## Scope honesty (RS256 / discovery seam)
//! The hermetic path verifies an **ES256** upstream (reusing t48's `p256` verify — no new dep, the
//! ticket's "ES256 fixtures avoid it"). A future RS256 upstream is a documented extension point
//! ([`crate::Jwks`] is EC-shaped today); it is NOT claimed to work until a hermetic RS256 fixture
//! proves it. The live discovery fetch + browser redirect are the binary's native seam.

use serde_json::Value;

use qfs_crypto_core::constant_time_eq;

use crate::{verify_jws, Jwks, OauthError};

/// The validated identity lifted off a good upstream ID token — exactly what the federation linker
/// needs to map an upstream subject to a local user. Carries NO token and NO secret: just the
/// provider-scoped `subject`, the `email` + its verification bit, and the `issuer` it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdTokenClaims {
    /// The upstream issuer (`iss`) — the IdP that signed the token (already checked to equal the
    /// configured upstream issuer).
    pub issuer: String,
    /// The upstream-scoped subject (`sub`) — stable per user at that IdP; the trust anchor the
    /// linker keys `(provider, subject)` on.
    pub subject: String,
    /// The email claim (`email`), if present. The linker uses it to match/provision a local user —
    /// but ONLY when [`email_verified`] is true.
    ///
    /// [`email_verified`]: IdTokenClaims::email_verified
    pub email: Option<String>,
    /// The `email_verified` claim (defaults to `false` when absent — fail closed). The linker refuses
    /// to bind an unverified email to a local identity.
    pub email_verified: bool,
}

/// The taxonomy of upstream-ID-token verification failures. Every variant fails the login closed and
/// is value-free (a log of the variant leaks no token/claim/key material). The variants are distinct
/// so a test can pin WHICH check rejected a given token.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum IdTokenError {
    /// The token is structurally malformed, carries a non-`ES256` header, or names a `kid` absent
    /// from the upstream JWKS.
    #[error("the upstream ID token is malformed or names an unknown signing key")]
    Malformed,
    /// The ES256 signature does not verify against the upstream's published JWKS (forged/tampered,
    /// or signed by a key the upstream does not publish).
    #[error("the upstream ID token signature is invalid")]
    BadSignature,
    /// The `iss` claim is absent or is not the configured upstream issuer.
    #[error("the upstream ID token was not issued by the expected issuer")]
    WrongIssuer,
    /// The `aud` claim is absent or does not contain our client id (audience-confusion guard).
    #[error("the upstream ID token was not issued for this client")]
    WrongAudience,
    /// The `exp` claim is absent, malformed, or in the past (the token has expired).
    #[error("the upstream ID token has expired")]
    Expired,
    /// The `nonce` claim is absent or does not equal the nonce we minted for this login (replay
    /// defense).
    #[error("the upstream ID token nonce does not match the one minted for this login")]
    NonceMismatch,
}

impl From<OauthError> for IdTokenError {
    fn from(e: OauthError) -> Self {
        match e {
            OauthError::BadSignature => IdTokenError::BadSignature,
            // Everything else structural (wrong segment count, bad base64url/JSON, non-ES256 header,
            // unknown kid) folds into the malformed bucket.
            _ => IdTokenError::Malformed,
        }
    }
}

/// Verify a compact ES256 **upstream OIDC ID token** for the relying-party login leg: check the
/// signature against the upstream's `jwks` (t48 [`verify_jws`]), then the registered claims —
/// `iss == expected_iss`, `aud` contains `expected_aud` (our client id), `exp > now_unix`, and
/// `nonce == expected_nonce` (constant-time). On success, lift the [`IdTokenClaims`]
/// (`sub`/`email`/`email_verified`) the federation linker keys a local identity to.
///
/// The token string is read by reference only and never logged; on ANY failure the most specific
/// [`IdTokenError`] is returned and NO identity is provisioned (fail closed).
///
/// # Errors
/// The first failing check, as the matching [`IdTokenError`] variant (see the type's docs).
pub fn verify_id_token(
    token: &str,
    jwks: &Jwks,
    expected_iss: &str,
    expected_aud: &str,
    expected_nonce: &str,
    now_unix: u64,
) -> Result<IdTokenClaims, IdTokenError> {
    // 1. Signature + structural validation against the UPSTREAM's JWKS (alg pinned to ES256 inside).
    let claims: Value = verify_jws(token, jwks)?;

    // 2. Issuer: the token must come from the upstream IdP we configured to trust.
    if claims.get("iss").and_then(Value::as_str) != Some(expected_iss) {
        return Err(IdTokenError::WrongIssuer);
    }

    // 3. Audience: the token must have been minted for OUR client id (string, or array membership —
    //    OIDC ID tokens commonly carry the array form when there are additional audiences).
    if !audience_contains(claims.get("aud"), expected_aud) {
        return Err(IdTokenError::WrongAudience);
    }

    // 4. Expiry: an absent/malformed `exp` is fatal — we never trust an unbounded ID token.
    let exp = claims
        .get("exp")
        .and_then(Value::as_u64)
        .ok_or(IdTokenError::Expired)?;
    if exp <= now_unix {
        return Err(IdTokenError::Expired);
    }

    // 5. Nonce: bind the token to the EXACT authorization request we started (replay defense). An
    //    absent nonce never matches; the compare is constant-time so it does not leak via timing.
    let nonce = claims
        .get("nonce")
        .and_then(Value::as_str)
        .ok_or(IdTokenError::NonceMismatch)?;
    if !constant_time_eq(nonce.as_bytes(), expected_nonce.as_bytes()) {
        return Err(IdTokenError::NonceMismatch);
    }

    // All checks passed — lift the identity claims. `email_verified` defaults to false (fail closed)
    // so a token omitting it never auto-links an email downstream.
    Ok(IdTokenClaims {
        issuer: expected_iss.to_string(),
        subject: claims
            .get("sub")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        email: claims
            .get("email")
            .and_then(Value::as_str)
            .map(str::to_string),
        email_verified: claims
            .get("email_verified")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    })
}

/// Whether the `aud` claim contains `expected`: the single-string form (`"aud":"…"`) must equal it;
/// the array form (`"aud":["…","…"]`, common in OIDC ID tokens) must CONTAIN it. Any other shape (or
/// a missing claim) does not match (fail-closed).
fn audience_contains(aud: Option<&Value>, expected: &str) -> bool {
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
    use crate::{sign_jws, SigningKey};
    use serde_json::json;

    const ISS: &str = "https://idp.example";
    const AUD: &str = "qfs-rp-client-1";
    const NONCE: &str = "minted-nonce-abc123";

    /// The fixture UPSTREAM signing key (a stable golden ES256 key — stands in for the IdP's key).
    fn upstream_key() -> SigningKey {
        SigningKey::generate(&crate::key::FIXED_SCALAR).unwrap()
    }

    /// The upstream's published JWKS (what the binary would fetch from the upstream `jwks_uri`).
    fn upstream_jwks(key: &SigningKey) -> Jwks {
        Jwks::new(vec![key.public_jwk()])
    }

    /// Mint a fixture upstream ID token over a standard OIDC claim set.
    fn id_token(key: &SigningKey, exp: u64, email_verified: bool) -> String {
        let claims = json!({
            "iss": ISS,
            "sub": "upstream-subject-1",
            "aud": [AUD, "another-audience"],
            "exp": exp,
            "nonce": NONCE,
            "email": "alice@example.com",
            "email_verified": email_verified,
        });
        sign_jws(&claims, key).unwrap()
    }

    #[test]
    fn a_valid_upstream_id_token_verifies_and_lifts_the_identity() {
        let key = upstream_key();
        let token = id_token(&key, 9_999, true);
        let claims = verify_id_token(&token, &upstream_jwks(&key), ISS, AUD, NONCE, 1_000).unwrap();
        assert_eq!(claims.issuer, ISS);
        assert_eq!(claims.subject, "upstream-subject-1");
        assert_eq!(claims.email.as_deref(), Some("alice@example.com"));
        assert!(claims.email_verified);
    }

    #[test]
    fn a_token_signed_by_a_key_outside_the_upstream_jwks_is_rejected() {
        let key = upstream_key();
        let token = id_token(&key, 9_999, true);
        // A JWKS that does NOT contain the signer's key → unknown kid → Malformed (fail closed).
        let foreign = SigningKey::generate(&[7u8; 32]).unwrap();
        assert_eq!(
            verify_id_token(&token, &upstream_jwks(&foreign), ISS, AUD, NONCE, 1_000).unwrap_err(),
            IdTokenError::Malformed,
        );
    }

    #[test]
    fn a_tampered_payload_is_a_bad_signature() {
        let key = upstream_key();
        let token = id_token(&key, 9_999, true);
        let jwks = upstream_jwks(&key);
        // Forge the payload (elevate to a different subject) while keeping the original signature.
        let mut parts: Vec<String> = token.split('.').map(str::to_string).collect();
        let forged = json!({
            "iss": ISS, "sub": "attacker", "aud": AUD, "exp": 9_999u64, "nonce": NONCE,
            "email": "attacker@example.com", "email_verified": true,
        });
        parts[1] = crate::key::b64url_encode(&serde_json::to_vec(&forged).unwrap());
        let tampered = parts.join(".");
        assert_eq!(
            verify_id_token(&tampered, &jwks, ISS, AUD, NONCE, 1_000).unwrap_err(),
            IdTokenError::BadSignature,
        );
    }

    #[test]
    fn an_expired_upstream_token_is_rejected() {
        let key = upstream_key();
        let token = id_token(&key, 1_600, true); // exp = 1_600
        assert_eq!(
            verify_id_token(&token, &upstream_jwks(&key), ISS, AUD, NONCE, 1_600).unwrap_err(),
            IdTokenError::Expired,
            "exp is inclusive-exclusive: now == exp is expired"
        );
        assert_eq!(
            verify_id_token(&token, &upstream_jwks(&key), ISS, AUD, NONCE, 9_999).unwrap_err(),
            IdTokenError::Expired,
        );
    }

    #[test]
    fn a_wrong_audience_token_is_rejected() {
        let key = upstream_key();
        let token = id_token(&key, 9_999, true);
        assert_eq!(
            verify_id_token(
                &token,
                &upstream_jwks(&key),
                ISS,
                "some-other-client",
                NONCE,
                1_000
            )
            .unwrap_err(),
            IdTokenError::WrongAudience,
        );
    }

    #[test]
    fn a_wrong_issuer_token_is_rejected() {
        let key = upstream_key();
        let token = id_token(&key, 9_999, true);
        assert_eq!(
            verify_id_token(
                &token,
                &upstream_jwks(&key),
                "https://evil.example",
                AUD,
                NONCE,
                1_000
            )
            .unwrap_err(),
            IdTokenError::WrongIssuer,
        );
    }

    #[test]
    fn a_wrong_nonce_token_is_rejected_replay_defense() {
        let key = upstream_key();
        let token = id_token(&key, 9_999, true);
        assert_eq!(
            verify_id_token(
                &token,
                &upstream_jwks(&key),
                ISS,
                AUD,
                "a-different-nonce",
                1_000
            )
            .unwrap_err(),
            IdTokenError::NonceMismatch,
        );
    }

    #[test]
    fn a_token_without_a_nonce_is_rejected() {
        let key = upstream_key();
        // Mint a token deliberately OMITTING the nonce claim.
        let claims = json!({
            "iss": ISS, "sub": "s", "aud": AUD, "exp": 9_999u64,
            "email": "a@b.com", "email_verified": true,
        });
        let token = sign_jws(&claims, &key).unwrap();
        assert_eq!(
            verify_id_token(&token, &upstream_jwks(&key), ISS, AUD, NONCE, 1_000).unwrap_err(),
            IdTokenError::NonceMismatch,
        );
    }

    #[test]
    fn email_verified_defaults_to_false_when_absent() {
        let key = upstream_key();
        let claims = json!({
            "iss": ISS, "sub": "s", "aud": AUD, "exp": 9_999u64, "nonce": NONCE,
            "email": "a@b.com",
        });
        let token = sign_jws(&claims, &key).unwrap();
        let lifted = verify_id_token(&token, &upstream_jwks(&key), ISS, AUD, NONCE, 1_000).unwrap();
        assert!(
            !lifted.email_verified,
            "an absent email_verified must default to false (fail closed)"
        );
    }

    #[test]
    fn structurally_broken_tokens_are_malformed_not_a_panic() {
        let jwks = upstream_jwks(&upstream_key());
        for bad in ["", "a.b", "not-a-token", "...."] {
            assert_eq!(
                verify_id_token(bad, &jwks, ISS, AUD, NONCE, 1_000).unwrap_err(),
                IdTokenError::Malformed,
            );
        }
    }

    #[test]
    fn the_single_string_audience_form_is_accepted() {
        let key = upstream_key();
        let claims = json!({
            "iss": ISS, "sub": "s", "aud": AUD, "exp": 9_999u64, "nonce": NONCE,
            "email": "a@b.com", "email_verified": true,
        });
        let token = sign_jws(&claims, &key).unwrap();
        assert!(verify_id_token(&token, &upstream_jwks(&key), ISS, AUD, NONCE, 1_000).is_ok());
    }
}
