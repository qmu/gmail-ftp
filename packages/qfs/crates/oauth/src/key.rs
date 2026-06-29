//! [`SigningKey`] — the AS's ES256 signing key, plus the value-free [`OauthError`] taxonomy and the
//! base64url / RFC 7638 `kid`-thumbprint helpers shared across the crate.
//!
//! The private key NEVER lives as a bare `Vec<u8>`/`String` here: it is reconstructed from a
//! [`qfs_secrets::Secret`] ([`SigningKey::from_secret_scalar`]) and re-exposed only back into a
//! `Secret` for the binary to envelope-encrypt at rest ([`SigningKey::secret_scalar`]). The held
//! [`p256::ecdsa::SigningKey`] is itself zeroized on drop and never serialized; only the derived
//! PUBLIC [`Jwk`] is ever rendered.

use base64::Engine as _;
use p256::ecdsa::{SigningKey as P256SigningKey, VerifyingKey};
use qfs_secrets::Secret;

use crate::jwks::Jwk;

/// The base64url-no-pad engine (RFC 7515 §2 / RFC 4648 §5) used for every JWS segment, JWK
/// coordinate, and thumbprint in this crate.
pub(crate) const B64URL: base64::engine::general_purpose::GeneralPurpose =
    base64::engine::general_purpose::URL_SAFE_NO_PAD;

/// Encode bytes as base64url without padding.
pub(crate) fn b64url_encode(bytes: &[u8]) -> String {
    B64URL.encode(bytes)
}

/// Decode base64url-no-pad bytes, mapping any error to the value-free [`OauthError::Malformed`].
pub(crate) fn b64url_decode(s: &str) -> Result<Vec<u8>, OauthError> {
    B64URL.decode(s).map_err(|_| OauthError::Malformed)
}

/// The fixed scalar/coordinate width for P-256 (256-bit field): 32 bytes.
const P256_FIELD_BYTES: usize = 32;

/// A value-free OAuth-AS error (AI-consumable; RFD §10). It names the failing *operation*, never a
/// byte of key material — a bad key, a malformed token, an unknown `kid`, and a failed signature
/// verification are deliberately coarse so nothing about the protected key leaks through the error.
#[derive(Debug, thiserror::Error)]
pub enum OauthError {
    /// Generating or reconstructing a signing key failed (e.g. a scalar out of the curve range).
    #[error("oauth signing key is invalid")]
    InvalidKey,
    /// Producing a JWS signature failed.
    #[error("oauth JWS signing failed")]
    Signing,
    /// A token was structurally malformed (wrong segment count, bad base64url, bad JSON, or a
    /// header that is not the expected ES256 shape).
    #[error("oauth JWS token is malformed")]
    Malformed,
    /// The token's `kid` matched no key in the presented JWK set.
    #[error("oauth JWS token references an unknown key id")]
    UnknownKid,
    /// The signature did not verify against the resolved public key (tamper / wrong key).
    #[error("oauth JWS signature verification failed")]
    BadSignature,
}

/// The AS's active ES256 signing key: the private [`p256::ecdsa::SigningKey`] (zeroized on drop,
/// never serialized) plus the deterministic RFC 7638 `kid` thumbprint of its public key. Construct
/// it with [`SigningKey::generate`] (fresh) or [`SigningKey::from_secret_scalar`] (reload from the
/// envelope-decrypted scalar). It is intentionally NOT `Clone`/`Debug`/`Serialize`.
pub struct SigningKey {
    kid: String,
    inner: P256SigningKey,
}

impl SigningKey {
    /// Build a signing key from 32 bytes of caller-supplied OS entropy (the binary owns the CSPRNG
    /// — the same entropy-injection discipline `qfs-session` uses, keeping this leaf off a
    /// `rand`/`getrandom` edge). The bytes are interpreted as the P-256 secret scalar; a uniformly
    /// random 32-byte value is a valid scalar with overwhelming probability, and the negligible
    /// out-of-range case is reported as [`OauthError::InvalidKey`] (the caller retries with fresh
    /// entropy).
    ///
    /// # Errors
    /// [`OauthError::InvalidKey`] if `entropy` is zero or not a valid P-256 scalar.
    pub fn generate(entropy: &[u8; P256_FIELD_BYTES]) -> Result<Self, OauthError> {
        let inner =
            P256SigningKey::from_bytes(entropy.into()).map_err(|_| OauthError::InvalidKey)?;
        let kid = Self::compute_kid(inner.verifying_key());
        Ok(Self { kid, inner })
    }

    /// Reconstruct a signing key from the envelope-decrypted private scalar carried in a
    /// [`Secret`]. The `kid` is RE-DERIVED from the resulting public key (so it is always a function
    /// of the key material, never trusted from storage), which also rejects a corrupted scalar.
    ///
    /// # Errors
    /// [`OauthError::InvalidKey`] if the secret is not exactly 32 bytes or not a valid scalar.
    pub fn from_secret_scalar(secret: &Secret) -> Result<Self, OauthError> {
        let bytes: [u8; P256_FIELD_BYTES] = secret
            .expose()
            .try_into()
            .map_err(|_| OauthError::InvalidKey)?;
        let inner =
            P256SigningKey::from_bytes((&bytes).into()).map_err(|_| OauthError::InvalidKey)?;
        let kid = Self::compute_kid(inner.verifying_key());
        Ok(Self { kid, inner })
    }

    /// The key id (RFC 7638 base64url SHA-256 JWK thumbprint of the public key). Stable across
    /// reloads of the same key — the property the second-boot key-reuse assertion checks.
    #[must_use]
    pub fn kid(&self) -> &str {
        &self.kid
    }

    /// The PUBLIC key as a JWK (`kty=EC` / `crv=P-256` / `use=sig` / `alg=ES256` / this `kid`) —
    /// the only key material this crate ever serializes. Carries no private scalar.
    #[must_use]
    pub fn public_jwk(&self) -> Jwk {
        let (x, y) = encode_public_coords(self.inner.verifying_key());
        Jwk::ec_p256(self.kid.clone(), x, y)
    }

    /// The PRIVATE scalar re-wrapped in a fresh [`Secret`] for the binary to envelope-encrypt at
    /// rest. The bytes leave the key ONLY inside the redacting/zeroized wrapper — never as a bare
    /// `Vec<u8>`. Used once, at generation time, to persist the new key.
    #[must_use]
    pub fn secret_scalar(&self) -> Secret {
        Secret::new(self.inner.to_bytes().to_vec())
    }

    /// The held private ECDSA key (crate-internal, for [`crate::sign_jws`]).
    pub(crate) fn inner(&self) -> &P256SigningKey {
        &self.inner
    }

    /// Compute the RFC 7638 JWK thumbprint `kid`: base64url(SHA-256(canonical-JWK-JSON)). The
    /// canonical form fixes the member order (`crv`,`kty`,`x`,`y`) and the exact field set, so the
    /// thumbprint is a deterministic function of the public key alone.
    fn compute_kid(vk: &VerifyingKey) -> String {
        let (x, y) = encode_public_coords(vk);
        // RFC 7638 §3.2: lexicographically-ordered members, no whitespace.
        let canonical = format!(r#"{{"crv":"P-256","kty":"EC","x":"{x}","y":"{y}"}}"#);
        let digest = qfs_crypto_core::sha256(canonical.as_bytes());
        b64url_encode(&digest)
    }
}

/// Extract the `(x, y)` affine coordinates of a P-256 public key as base64url-no-pad strings (the
/// JWK `x`/`y` members). The uncompressed SEC1 encoding is `0x04 || X(32) || Y(32)`.
pub(crate) fn encode_public_coords(vk: &VerifyingKey) -> (String, String) {
    let point = vk.to_encoded_point(false);
    // `false` (uncompressed) guarantees both coordinates are present.
    let x = point.x().map(|b| b64url_encode(b)).unwrap_or_default();
    let y = point.y().map(|b| b64url_encode(b)).unwrap_or_default();
    (x, y)
}

/// Reconstruct a P-256 [`VerifyingKey`] from a JWK's base64url `(x, y)` coordinates (the verify
/// side of [`crate::verify_jws`]).
///
/// # Errors
/// [`OauthError::Malformed`] if either coordinate is not valid base64url of the right length, or the
/// point is not on the curve.
pub(crate) fn verifying_key_from_coords(
    x_b64: &str,
    y_b64: &str,
) -> Result<VerifyingKey, OauthError> {
    let x = b64url_decode(x_b64)?;
    let y = b64url_decode(y_b64)?;
    if x.len() != P256_FIELD_BYTES || y.len() != P256_FIELD_BYTES {
        return Err(OauthError::Malformed);
    }
    // Build the uncompressed SEC1 point `0x04 || X || Y` and parse it.
    let mut sec1 = Vec::with_capacity(1 + 2 * P256_FIELD_BYTES);
    sec1.push(0x04);
    sec1.extend_from_slice(&x);
    sec1.extend_from_slice(&y);
    VerifyingKey::from_sec1_bytes(&sec1).map_err(|_| OauthError::Malformed)
}

/// A fixed, non-zero 32-byte scalar so the key (and thus the `kid`, public JWK, and signatures) is a
/// stable golden vector across runs. Shared by the `key`/`jwks`/`sign` test modules (hence module
/// level + `pub(crate)`, not buried in one `tests` submodule).
#[cfg(test)]
pub(crate) const FIXED_SCALAR: [u8; 32] = [
    0x4c, 0x0b, 0x1f, 0x77, 0x2a, 0x91, 0x55, 0x3e, 0x8d, 0x24, 0x6b, 0xa0, 0x13, 0xcc, 0x7e, 0x42,
    0x99, 0x01, 0xfe, 0x5b, 0x37, 0x88, 0xd1, 0x0a, 0x64, 0xbb, 0x2f, 0x70, 0x16, 0xe3, 0x4d, 0x09,
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_then_reload_from_secret_yields_the_same_kid_and_public_key() {
        let key = SigningKey::generate(&FIXED_SCALAR).unwrap();
        let secret = key.secret_scalar();
        // The scalar round-trips back into the SAME key (the at-rest reload path).
        let reloaded = SigningKey::from_secret_scalar(&secret).unwrap();
        assert_eq!(key.kid(), reloaded.kid(), "kid is stable across reload");
        assert_eq!(
            key.public_jwk(),
            reloaded.public_jwk(),
            "public JWK is stable across reload"
        );
    }

    #[test]
    fn the_kid_is_a_deterministic_thumbprint_and_url_safe() {
        let kid = SigningKey::generate(&FIXED_SCALAR)
            .unwrap()
            .kid()
            .to_string();
        // base64url(SHA-256(..)) of a 32-byte digest is 43 chars, no padding, no '+'/'/'.
        assert_eq!(kid.len(), 43);
        assert!(!kid.contains('=') && !kid.contains('+') && !kid.contains('/'));
        // Deterministic: the same key recomputes the same kid.
        assert_eq!(kid, SigningKey::generate(&FIXED_SCALAR).unwrap().kid());
    }

    #[test]
    fn a_zero_scalar_is_rejected_without_panicking() {
        assert!(matches!(
            SigningKey::generate(&[0u8; 32]),
            Err(OauthError::InvalidKey)
        ));
        // A wrong-length secret is rejected, not truncated.
        assert!(matches!(
            SigningKey::from_secret_scalar(&Secret::new(vec![1, 2, 3])),
            Err(OauthError::InvalidKey)
        ));
    }

    #[test]
    fn public_coords_round_trip_through_a_verifying_key() {
        let key = SigningKey::generate(&FIXED_SCALAR).unwrap();
        let jwk = key.public_jwk();
        // The published coordinates reconstruct the same verifying key.
        assert!(verifying_key_from_coords(&jwk.x, &jwk.y).is_ok());
    }
}
