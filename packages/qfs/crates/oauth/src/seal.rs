//! t78: the pure **audit-chain seal** (signed checkpoint) primitives [`sign_seal`] / [`verify_seal`]
//! over the existing ES256 JWS (roadmap **decision V** / §4.6).
//!
//! ## What a seal IS
//! A *seal* (a.k.a. a signed checkpoint) is a short, signed statement over the audit chain HEAD
//! (t76): the head's `(seq, content_hash, prev_hash)` plus the wall-clock `issued_at` it was minted
//! at. It is signed with the SAME AS ES256 key that signs access tokens ([`crate::sign_jws`]), so an
//! independent verifier — holding only the published [`Jwks`] — can confirm three things at once:
//!
//! 1. **Authenticity** — the seal was issued by THIS authorization server (the signature verifies
//!    against the AS public key resolved by the header `kid`), not forged by whoever controls the
//!    audit store.
//! 2. **Position** — the chain reached exactly `seq` events, with this `content_hash`/`prev_hash`
//!    head. Because the seal lands in an append-only witness *outside* the server (t78's WORM /
//!    transparency-log seam), a compromised server cannot later truncate or fork the chain below a
//!    sealed `seq` without contradicting an anchor it can no longer change.
//! 3. **Time** — when the head was sealed (`issued_at`), so a consumer can order seals and bound how
//!    far back tamper-evidence extends.
//!
//! ## Why it lives in `qfs-oauth` (pure), not the binary
//! Like [`crate::sign_jws`]/[`crate::verify_access_token`], a seal is protocol logic with no I/O: it
//! takes the head fields + the AS [`SigningKey`] (sign) or a token + the published [`Jwks`] (verify)
//! and returns a value. Keeping it here makes it golden-vector testable against a fixed key with zero
//! credentials, and reuses the vetted JWS path rather than minting a second signature format. The
//! IMPURE halves — reading the durable head, recomputing the chain over a consumer's stored events,
//! and handing the seal to a WORM witness — live binary-side (`crates/qfs/src/worm.rs`), which is the
//! ONLY place that owns the System DB, the AS key material, and a real WORM target (decision F/V).
//!
//! ## A seal is NOT an access token (a confusion guard)
//! Every seal carries a fixed [`SEAL_KIND`] discriminator in its claims, and [`verify_seal`] rejects
//! any token that does not carry it ([`SealError::WrongKind`]). So a seal can never be replayed as a
//! bearer token, and an access token can never masquerade as a seal — even though both are ES256 JWS
//! signed by the same key. (The mirror of the `alg=ES256` algorithm-confusion guard in
//! [`crate::verify_jws`].)
//!
//! ## Metadata only (RFD §10)
//! A seal carries ONLY chain-position metadata — sequence number, two content/link hashes, and a
//! timestamp. There is structurally NOWHERE to put a secret or a row's payload, exactly the boundary
//! the audit event itself enforces (t76). The private signing key never appears in a seal; only the
//! signature it produced does.

use serde::Serialize;
use serde_json::Value;

use crate::{sign_jws, verify_jws, Jwks, OauthError, SigningKey};

/// The fixed `kind` discriminator a seal's claims carry, distinguishing a seal token from any other
/// ES256 JWS this AS signs (an access token, a future statement). Bumping the suffix is how a future
/// canonical-form change stays distinguishable. [`verify_seal`] rejects a token lacking exactly this
/// value ([`SealError::WrongKind`]).
pub const SEAL_KIND: &str = "qfs.audit.seal.v1";

/// A signed statement over the audit chain HEAD (t76) — the value [`sign_seal`] mints and
/// [`verify_seal`] returns. Exactly the durable head's three columns (`seq`, `content_hash`,
/// `prev_hash` — the t76 `ChainHead`) plus the `issued_at` wall clock the seal was minted at.
/// Metadata only: there is no field a secret or a row payload could ride in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditSeal {
    /// The sealed head's sequence number — the chain reached exactly this many events.
    pub seq: u64,
    /// The sealed head's `content_hash` (the latest event's stable content commitment).
    pub content_hash: String,
    /// The sealed head's predecessor link (`prev_hash`); together with `content_hash` this recomputes
    /// the head's chained `hash`, so the seal commits to the WHOLE head, not just its sequence number.
    pub prev_hash: String,
    /// When the head was sealed (RFC3339 UTC). The binary injects the wall clock; this pure type
    /// only carries it.
    pub issued_at: String,
}

/// The claims a seal's JWS payload carries — the [`AuditSeal`] fields plus the [`SEAL_KIND`]
/// discriminator. Serialized by value-borrowing so signing never copies the head strings.
#[derive(Serialize)]
struct SealClaims<'a> {
    /// The fixed seal discriminator ([`SEAL_KIND`]).
    kind: &'a str,
    /// The sealed head's sequence number.
    seq: u64,
    /// The sealed head's content hash.
    content_hash: &'a str,
    /// The sealed head's predecessor link.
    prev_hash: &'a str,
    /// The seal's issue time (RFC3339 UTC).
    issued_at: &'a str,
}

/// The value-free seal-verification taxonomy (AI-consumable; RFD §10). Each variant names the
/// failing condition only — never the seal, a claim value, or key material — so a log of the variant
/// leaks nothing. Distinct variants let a test pin WHICH check rejected a given token.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SealError {
    /// The token is structurally malformed, carries a non-`ES256` header, names an unknown `kid`, or
    /// is missing a required seal claim.
    #[error("the audit seal is malformed or names an unknown signing key")]
    Malformed,
    /// The ES256 signature does not verify (tampered, or signed by a key not in the JWKS) — the seal
    /// was not issued by this authorization server.
    #[error("the audit seal signature is invalid")]
    BadSignature,
    /// The token verified, but its claims do not carry the [`SEAL_KIND`] discriminator — it is some
    /// other ES256 JWS (e.g. an access token), not a seal (a token-confusion guard).
    #[error("the token is not a qfs audit seal")]
    WrongKind,
}

impl From<OauthError> for SealError {
    fn from(e: OauthError) -> Self {
        match e {
            // A bad/forged signature is its own signal; everything else structural is `Malformed`.
            OauthError::BadSignature => SealError::BadSignature,
            _ => SealError::Malformed,
        }
    }
}

/// Sign `seal` into a compact ES256 JWS checkpoint with the AS `key` (reusing [`sign_jws`]). The
/// header pins `alg=ES256` + the key's `kid`; the payload is the [`AuditSeal`] fields plus the
/// [`SEAL_KIND`] discriminator. Deterministic (RFC 6979): a fixed key + fixed head + fixed
/// `issued_at` produce a byte-identical token — the golden-vector property a witness can pin.
///
/// # Errors
/// [`OauthError::Signing`] if the claims cannot be serialized or the ECDSA sign fails.
pub fn sign_seal(seal: &AuditSeal, key: &SigningKey) -> Result<String, OauthError> {
    let claims = SealClaims {
        kind: SEAL_KIND,
        seq: seal.seq,
        content_hash: &seal.content_hash,
        prev_hash: &seal.prev_hash,
        issued_at: &seal.issued_at,
    };
    sign_jws(&claims, key)
}

/// Verify a compact ES256 JWS seal `token` against the published `jwks`: check the signature
/// (resolving the AS public key by the header `kid`, `alg` pinned to ES256 inside [`verify_jws`]),
/// confirm the [`SEAL_KIND`] discriminator, and return the decoded [`AuditSeal`]. This proves the
/// seal's AUTHENTICITY + the head it commits to; the CONTINUITY check (that the head matches a
/// recomputed chain over a consumer's stored events) is the binary-side companion, since the events
/// live in `qfs-store`.
///
/// # Errors
/// - [`SealError::BadSignature`] — the signature does not verify (tamper / wrong key).
/// - [`SealError::Malformed`] — wrong segment count, bad base64url/JSON, a non-ES256 header, an
///   unknown `kid`, or a missing/ill-typed seal claim.
/// - [`SealError::WrongKind`] — the token verifies but is not a seal (no [`SEAL_KIND`]).
pub fn verify_seal(token: &str, jwks: &Jwks) -> Result<AuditSeal, SealError> {
    // 1. Signature + structural validation (alg pinned to ES256 inside verify_jws).
    let claims: Value = verify_jws(token, jwks)?;

    // 2. Kind: a verified token that is not a seal is rejected before any field is trusted.
    if claims.get("kind").and_then(Value::as_str) != Some(SEAL_KIND) {
        return Err(SealError::WrongKind);
    }

    // 3. Lift the head fields off the now-trusted claims (a missing/ill-typed field is Malformed).
    let seq = claims
        .get("seq")
        .and_then(Value::as_u64)
        .ok_or(SealError::Malformed)?;
    let content_hash = claims
        .get("content_hash")
        .and_then(Value::as_str)
        .ok_or(SealError::Malformed)?
        .to_string();
    let prev_hash = claims
        .get("prev_hash")
        .and_then(Value::as_str)
        .ok_or(SealError::Malformed)?
        .to_string();
    let issued_at = claims
        .get("issued_at")
        .and_then(Value::as_str)
        .ok_or(SealError::Malformed)?
        .to_string();

    Ok(AuditSeal {
        seq,
        content_hash,
        prev_hash,
        issued_at,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{access_token_claims, sign_jws};
    use serde_json::json;

    fn fixed_key() -> SigningKey {
        SigningKey::generate(&crate::key::FIXED_SCALAR).unwrap()
    }

    fn jwks_of(key: &SigningKey) -> Jwks {
        Jwks::new(vec![key.public_jwk()])
    }

    fn sample_seal() -> AuditSeal {
        AuditSeal {
            seq: 7,
            content_hash: "a".repeat(64),
            prev_hash: "b".repeat(64),
            issued_at: "2026-06-28T00:00:00Z".to_string(),
        }
    }

    #[test]
    fn sign_then_verify_round_trips_the_sealed_head() {
        let key = fixed_key();
        let seal = sample_seal();
        let token = sign_seal(&seal, &key).unwrap();
        assert_eq!(token.split('.').count(), 3, "a seal is a compact JWS");

        let verified = verify_seal(&token, &jwks_of(&key)).unwrap();
        assert_eq!(verified, seal, "the verified seal equals the sealed head");
    }

    #[test]
    fn signing_is_deterministic_for_a_fixed_key_and_head() {
        // RFC 6979 deterministic ECDSA: a fixed key + fixed head + fixed issued_at produce a
        // byte-identical seal — the golden-vector property a witness pins.
        let key = fixed_key();
        let seal = sample_seal();
        assert_eq!(
            sign_seal(&seal, &key).unwrap(),
            sign_seal(&seal, &key).unwrap()
        );
    }

    #[test]
    fn a_tampered_seal_payload_fails_verification() {
        let key = fixed_key();
        let token = sign_seal(&sample_seal(), &key).unwrap();
        let jwks = jwks_of(&key);

        // Forge a different sealed seq, keeping the original signature.
        let mut parts: Vec<String> = token.split('.').map(str::to_string).collect();
        let forged = json!({
            "kind": SEAL_KIND,
            "seq": 9999,
            "content_hash": "a".repeat(64),
            "prev_hash": "b".repeat(64),
            "issued_at": "2026-06-28T00:00:00Z",
        });
        parts[1] = crate::key::b64url_encode(&serde_json::to_vec(&forged).unwrap());
        let tampered = parts.join(".");
        assert_eq!(
            verify_seal(&tampered, &jwks).unwrap_err(),
            SealError::BadSignature
        );
    }

    #[test]
    fn a_seal_signed_by_a_foreign_key_is_rejected() {
        let key = fixed_key();
        let token = sign_seal(&sample_seal(), &key).unwrap();
        // A JWKS that does NOT contain the signer's key → the kid resolves to nothing → Malformed.
        let foreign = SigningKey::generate(&[3u8; 32]).unwrap();
        assert_eq!(
            verify_seal(&token, &jwks_of(&foreign)).unwrap_err(),
            SealError::Malformed
        );
    }

    #[test]
    fn an_access_token_is_not_accepted_as_a_seal() {
        // A real, validly-signed access token (no `kind` claim) must NOT verify as a seal — the
        // token-confusion guard. It verifies as a JWS (same key) but is rejected for the missing kind.
        let key = fixed_key();
        let claims = access_token_claims(
            "http://localhost:8787",
            "http://localhost:8787/mcp",
            42,
            "mcp:read",
            "client-1",
            1_000,
            600,
        );
        let access = sign_jws(&claims, &key).unwrap();
        assert_eq!(
            verify_seal(&access, &jwks_of(&key)).unwrap_err(),
            SealError::WrongKind
        );
    }

    #[test]
    fn a_structurally_broken_seal_is_malformed_not_a_panic() {
        let jwks = jwks_of(&fixed_key());
        for bad in ["", "a.b", "not-a-token", "...."] {
            assert_eq!(verify_seal(bad, &jwks).unwrap_err(), SealError::Malformed);
        }
    }

    #[test]
    fn no_secret_leaks_into_a_seal() {
        // A seal carries ONLY chain-position metadata — there is no field for a secret. A would-be
        // token never appears in the seal's claims because there is nowhere to put it.
        let key = fixed_key();
        let token = sign_seal(&sample_seal(), &key).unwrap();
        // Decode the JWS payload and assert it carries only the labelled head fields.
        let payload_b64 = token.split('.').nth(1).unwrap();
        let payload = crate::key::b64url_decode(payload_b64).unwrap();
        let body = String::from_utf8_lossy(&payload);
        assert!(body.contains("\"kind\":\"qfs.audit.seal.v1\""));
        assert!(body.contains("\"seq\":7"));
        assert!(!body.contains("super-secret-token"));
    }
}
