//! The thin **JWS compact-serialization** primitives (RFC 7515) [`sign_jws`] / [`verify_jws`],
//! built over `p256` ES256 + the crate's base64url helpers. These are the primitives **t49/t50**
//! consume to mint/verify access tokens; t48 issues no tokens (it golden-vector tests them).
//!
//! A compact JWS is `base64url(header) . base64url(payload) . base64url(signature)`. For ES256 the
//! signature is the IEEE-P1363 fixed-size `r || s` (64 bytes) — exactly what [`p256::ecdsa::Signature`]
//! `to_bytes()` produces. The header is the minimal `{"alg":"ES256","typ":"JWT","kid":<kid>}`.
//!
//! Determinism: `p256`'s ECDSA signer is RFC 6979 deterministic, so a fixed key + fixed claims
//! produce a FIXED token — the property the golden-vector test pins.

use p256::ecdsa::signature::{Signer, Verifier};
use p256::ecdsa::Signature;
use serde::Serialize;
use serde_json::Value;

use crate::key::{b64url_decode, b64url_encode, verifying_key_from_coords};
use crate::{Jwks, OauthError, SigningKey, ALG_ES256};

/// The verified claim set returned by [`verify_jws`]: the decoded JWS payload as a JSON object.
/// t49/t50 will read the registered claims (`iss`/`aud`/`exp`/…) off this; t48 only round-trips it.
pub type Claims = Value;

/// The fixed JOSE header value `typ` carries for an access token.
const TYP_JWT: &str = "JWT";

/// Sign `claims` into a compact ES256 JWS with `key`. The header pins `alg=ES256`, `typ=JWT`, and
/// the key's `kid` (so a verifier can select the public key from the JWKS).
///
/// # Errors
/// [`OauthError::Signing`] if the claims cannot be serialized or the ECDSA sign fails.
pub fn sign_jws<C: Serialize>(claims: &C, key: &SigningKey) -> Result<String, OauthError> {
    let header = format!(
        r#"{{"alg":"{ALG_ES256}","typ":"{TYP_JWT}","kid":"{kid}"}}"#,
        kid = key.kid()
    );
    let payload = serde_json::to_vec(claims).map_err(|_| OauthError::Signing)?;
    let signing_input = format!(
        "{}.{}",
        b64url_encode(header.as_bytes()),
        b64url_encode(&payload)
    );
    // ES256: ECDSA over SHA-256 (the digest is internal to the p256 signer); the JWS signature is
    // the fixed-size r||s, base64url-encoded.
    let signature: Signature = key
        .inner()
        .try_sign(signing_input.as_bytes())
        .map_err(|_| OauthError::Signing)?;
    Ok(format!(
        "{signing_input}.{}",
        b64url_encode(&signature.to_bytes())
    ))
}

/// Verify a compact ES256 JWS `token` against the published `jwks`: resolve the signer by the
/// header `kid`, check the signature over `header.payload`, and return the decoded [`Claims`]. The
/// header `alg` MUST be `ES256` (an algorithm-confusion guard).
///
/// # Errors
/// - [`OauthError::Malformed`] — wrong segment count, bad base64url/JSON, or a non-ES256 header.
/// - [`OauthError::UnknownKid`] — the header `kid` matches no published key.
/// - [`OauthError::BadSignature`] — the signature does not verify (tamper / wrong key).
pub fn verify_jws(token: &str, jwks: &Jwks) -> Result<Claims, OauthError> {
    // 1. Split into exactly three segments.
    let mut parts = token.split('.');
    let (h_b64, p_b64, s_b64) = match (parts.next(), parts.next(), parts.next(), parts.next()) {
        (Some(h), Some(p), Some(s), None) => (h, p, s),
        _ => return Err(OauthError::Malformed),
    };

    // 2. Decode + validate the header (alg pinned to ES256; a kid must be present).
    let header: Value =
        serde_json::from_slice(&b64url_decode(h_b64)?).map_err(|_| OauthError::Malformed)?;
    if header.get("alg").and_then(Value::as_str) != Some(ALG_ES256) {
        return Err(OauthError::Malformed);
    }
    let kid = header
        .get("kid")
        .and_then(Value::as_str)
        .ok_or(OauthError::Malformed)?;

    // 3. Resolve the signing key from the JWKS by kid.
    let jwk = jwks.find(kid).ok_or(OauthError::UnknownKid)?;
    let vk = verifying_key_from_coords(&jwk.x, &jwk.y)?;

    // 4. Verify the signature over the EXACT `header.payload` signing input.
    let signing_input = format!("{h_b64}.{p_b64}");
    let sig = Signature::from_slice(&b64url_decode(s_b64)?).map_err(|_| OauthError::Malformed)?;
    vk.verify(signing_input.as_bytes(), &sig)
        .map_err(|_| OauthError::BadSignature)?;

    // 5. Decode the now-trusted payload.
    serde_json::from_slice(&b64url_decode(p_b64)?).map_err(|_| OauthError::Malformed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fixed_key() -> SigningKey {
        SigningKey::generate(&crate::key::FIXED_SCALAR).unwrap()
    }

    fn jwks_of(key: &SigningKey) -> Jwks {
        Jwks::new(vec![key.public_jwk()])
    }

    #[test]
    fn sign_then_verify_round_trips_the_claims() {
        let key = fixed_key();
        let claims =
            json!({"iss": "http://localhost:8787", "sub": "user-1", "exp": 9_999_999_999u64});
        let token = sign_jws(&claims, &key).unwrap();
        // Compact JWS has exactly three dot-separated segments.
        assert_eq!(token.split('.').count(), 3);

        let verified = verify_jws(&token, &jwks_of(&key)).unwrap();
        assert_eq!(verified, claims);
    }

    #[test]
    fn signing_is_deterministic_for_a_fixed_key_and_claims() {
        // RFC 6979 deterministic ECDSA: the SAME key + claims produce a byte-identical token — the
        // golden-vector property a fixture would pin.
        let key = fixed_key();
        let claims = json!({"sub": "golden"});
        let a = sign_jws(&claims, &key).unwrap();
        let b = sign_jws(&claims, &key).unwrap();
        assert_eq!(a, b, "ES256 signing must be deterministic (RFC 6979)");
    }

    #[test]
    fn a_tampered_payload_fails_verification() {
        let key = fixed_key();
        let token = sign_jws(&json!({"sub": "user-1"}), &key).unwrap();
        let jwks = jwks_of(&key);

        // Flip the payload to a DIFFERENT claim set, re-encoded, keeping the original signature.
        let mut parts: Vec<&str> = token.split('.').collect();
        let forged_payload = b64url_encode(br#"{"sub":"admin"}"#);
        parts[1] = &forged_payload;
        let tampered = parts.join(".");
        assert!(tampered != token);
        assert!(matches!(
            verify_jws(&tampered, &jwks),
            Err(OauthError::BadSignature)
        ));
    }

    #[test]
    fn a_flipped_signature_byte_fails_verification() {
        let key = fixed_key();
        let token = sign_jws(&json!({"sub": "x"}), &key).unwrap();
        let jwks = jwks_of(&key);

        let mut parts: Vec<&str> = token.split('.').collect();
        // Decode the signature, flip one byte, re-encode (still a valid 64-byte sig, wrong value).
        let mut sig = b64url_decode(parts[2]).unwrap();
        sig[0] ^= 0x01;
        let bad = b64url_encode(&sig);
        parts[2] = &bad;
        let tampered = parts.join(".");
        assert!(matches!(
            verify_jws(&tampered, &jwks),
            Err(OauthError::BadSignature)
        ));
    }

    #[test]
    fn an_unknown_kid_is_rejected_before_any_crypto() {
        let key = fixed_key();
        let token = sign_jws(&json!({"sub": "x"}), &key).unwrap();
        // A JWKS that does NOT contain the signer's key.
        let other = SigningKey::generate(&[9u8; 32]).unwrap();
        assert!(matches!(
            verify_jws(&token, &jwks_of(&other)),
            Err(OauthError::UnknownKid)
        ));
    }

    #[test]
    fn structurally_malformed_tokens_are_rejected_not_panicked() {
        let jwks = jwks_of(&fixed_key());
        for bad in [
            "",
            "a.b",
            "a.b.c.d",
            "not-base64!.x.y",
            "....",
            "onlyonesegment",
        ] {
            assert!(verify_jws(bad, &jwks).is_err(), "{bad:?} must be rejected");
        }
    }

    #[test]
    fn an_alg_none_header_is_rejected_algorithm_confusion_guard() {
        let key = fixed_key();
        let jwks = jwks_of(&key);
        // Forge a header claiming `alg: none` over real claims, with an empty signature.
        let header = b64url_encode(br#"{"alg":"none","typ":"JWT","kid":"x"}"#);
        let payload = b64url_encode(br#"{"sub":"admin"}"#);
        let forged = format!("{header}.{payload}.");
        assert!(matches!(
            verify_jws(&forged, &jwks),
            Err(OauthError::Malformed)
        ));
    }
}
