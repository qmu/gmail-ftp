//! [`Jwk`] / [`Jwks`] — the JSON Web Key (RFC 7517) + JSON Web Key Set (the `/jwks.json` document).
//!
//! Only PUBLIC key material is ever represented here: a P-256 verification key as
//! (`kty=EC`, `crv=P-256`, `x`, `y`, `use=sig`, `alg=ES256`, `kid`). There is deliberately **no**
//! `d` (private scalar) member — a `Jwk` cannot carry a private key, so publishing one cannot leak
//! the signing key. Multiple keys are publishable in one [`Jwks`] so a future rotation can overlap
//! an `active` and a `retiring` key (a client accepts a token signed by any published `kid`).

use serde::{Deserialize, Serialize};

use crate::ALG_ES256;

/// One public JSON Web Key (RFC 7517) for an ES256 verification key. Field order matches the
/// declaration so the rendered JSON is stable; `use` is renamed from the Rust-reserved `use_`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Jwk {
    /// Key type — always `EC` for the elliptic-curve ES256 key.
    pub kty: String,
    /// Curve — always `P-256` (the NIST secp256r1 curve ES256 signs over).
    pub crv: String,
    /// The base64url-no-pad `x` affine coordinate (32 bytes).
    pub x: String,
    /// The base64url-no-pad `y` affine coordinate (32 bytes).
    pub y: String,
    /// Public-key use — `sig` (this key verifies signatures, it does not encrypt).
    #[serde(rename = "use")]
    pub use_: String,
    /// The algorithm this key is used with — `ES256`.
    pub alg: String,
    /// The key id (RFC 7638 thumbprint) a JWS header references to select this key.
    pub kid: String,
}

impl Jwk {
    /// Build a P-256 signing-use JWK from its base64url coordinates + `kid`. The constant members
    /// (`kty`/`crv`/`use`/`alg`) are fixed for the ES256 curve.
    #[must_use]
    pub fn ec_p256(kid: String, x: String, y: String) -> Self {
        Self {
            kty: "EC".to_string(),
            crv: "P-256".to_string(),
            x,
            y,
            use_: "sig".to_string(),
            alg: ALG_ES256.to_string(),
            kid,
        }
    }
}

/// A JSON Web Key Set — the `/jwks.json` body. One entry per published key (the active key, plus any
/// `retiring` keys during a rotation overlap).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Jwks {
    /// The published public keys, newest/active first by convention.
    pub keys: Vec<Jwk>,
}

impl Jwks {
    /// A key set from an ordered list of public keys.
    #[must_use]
    pub fn new(keys: Vec<Jwk>) -> Self {
        Self { keys }
    }

    /// Find the key with id `kid`, if published (the JWS verify path resolves the signer this way).
    #[must_use]
    pub fn find(&self, kid: &str) -> Option<&Jwk> {
        self.keys.iter().find(|k| k.kid == kid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SigningKey;

    fn fixed_key() -> SigningKey {
        SigningKey::generate(&crate::key::FIXED_SCALAR).unwrap()
    }

    #[test]
    fn jwk_has_the_exact_public_es256_shape_and_no_private_member() {
        let jwk = fixed_key().public_jwk();
        let v = serde_json::to_value(&jwk).unwrap();
        assert_eq!(v["kty"], "EC");
        assert_eq!(v["crv"], "P-256");
        assert_eq!(v["use"], "sig");
        assert_eq!(v["alg"], "ES256");
        assert!(v["kid"].is_string() && !v["kid"].as_str().unwrap().is_empty());
        assert!(v["x"].is_string() && v["y"].is_string());
        // CROWN-JEWEL INVARIANT: a published JWK NEVER carries the private scalar `d`.
        assert!(v.get("d").is_none(), "a public JWK must not contain `d`");
        // Exactly the seven public members, nothing more.
        assert_eq!(v.as_object().unwrap().len(), 7);
    }

    #[test]
    fn a_jwks_publishes_multiple_keys_and_finds_by_kid() {
        let active = fixed_key().public_jwk();
        // A second, distinct key (the `retiring` overlap a rotation would publish).
        let retiring = SigningKey::generate(&[7u8; 32]).unwrap().public_jwk();
        assert_ne!(active.kid, retiring.kid);

        let set = Jwks::new(vec![active.clone(), retiring.clone()]);
        assert_eq!(set.keys.len(), 2);
        assert_eq!(set.find(&active.kid), Some(&active));
        assert_eq!(set.find(&retiring.kid), Some(&retiring));
        assert!(set.find("no-such-kid").is_none());
    }

    #[test]
    fn jwks_round_trips_through_json() {
        let set = Jwks::new(vec![fixed_key().public_jwk()]);
        let json = serde_json::to_string(&set).unwrap();
        assert!(json.contains("\"keys\""));
        let back: Jwks = serde_json::from_str(&json).unwrap();
        assert_eq!(set, back);
    }
}
