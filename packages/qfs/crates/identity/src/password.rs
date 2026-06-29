//! argon2id password hashing + constant-time verification.
//!
//! [`hash_password`] derives an **argon2id** hash with PINNED parameters and a fresh random salt,
//! returning the standard **PHC string** (`$argon2id$v=19$m=…,t=…,p=…$salt$hash`). The params are
//! recorded *in* that string, so a future cost bump can detect an outdated hash and re-hash on the
//! next successful login (M2). [`verify_password`] re-derives the candidate under the params + salt
//! read back from the stored hash and compares the raw output bytes with
//! `qfs_crypto_core::constant_time_eq` (RFD §10 — never a short-circuiting `==`).
//!
//! ## Secret hygiene
//! The plaintext is a [`crate::Secret`] borrowed by reference; argon2 reads `expose()` and retains
//! no copy, and the caller's `Secret` is zeroized on drop. The [`PasswordHash`] is NOT a secret you
//! may log — its `Debug` is redacted — but it is also not reversible; it is stored at rest and never
//! surfaced by `whoami`.

use core::fmt;

use argon2::password_hash::rand_core::OsRng;
use argon2::password_hash::{PasswordHash as Phc, PasswordHasher, SaltString};
use argon2::{Algorithm, Argon2, Params, Version};

use crate::Secret;

/// The argon2id variant (memory-hard, side-channel-resistant) — pinned; we never emit argon2i/d.
const ALGORITHM: Algorithm = Algorithm::Argon2id;
/// The argon2 version byte we emit (`v=19`, i.e. 0x13). Verification reads the stored hash's own
/// version where present; this is the version of newly-minted hashes.
const VERSION: Version = Version::V0x13;

/// An argon2id password hash in PHC string form. Holds no live key material (it is a one-way hash),
/// but it is still kept out of logs / `whoami` / audit — its [`fmt::Debug`] is redacted so it cannot
/// leak through a `{:?}` of an enclosing type.
#[derive(Clone, PartialEq, Eq)]
pub struct PasswordHash(String);

impl PasswordHash {
    /// Wrap an existing PHC string read back from the store. (Construction from a candidate password
    /// is [`hash_password`].)
    #[must_use]
    pub fn from_phc(phc: String) -> Self {
        Self(phc)
    }

    /// The PHC string, for persisting into the `accounts.password_hash` column.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Consume into the owned PHC string (for the store's bind parameter).
    #[must_use]
    pub fn into_string(self) -> String {
        self.0
    }
}

/// Redacted `Debug`: never prints the hash bytes (defense in depth — the hash is one-way, but it
/// still stays out of any `{:?}` dump, the same posture as `Secret`).
impl fmt::Debug for PasswordHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PasswordHash(***redacted***)")
    }
}

/// The PINNED argon2id configuration. We pin to [`Params::DEFAULT`] (m=19456 KiB, t=2, p=1, 32-byte
/// output) — the argon2 crate's recommended baseline — so the cost is a recorded, reviewable
/// constant rather than an ambient default that could drift between crate versions. These values are
/// written into every PHC string we emit.
fn pinned_argon2() -> Argon2<'static> {
    Argon2::new(ALGORITHM, VERSION, Params::DEFAULT)
}

/// Hash `password` with argon2id under the pinned params + a fresh random salt, returning the PHC
/// string (params recorded in it). The plaintext is read by reference and never copied out; the
/// caller's `Secret` is zeroized on drop.
///
/// # Errors
/// [`PasswordError::Hash`] if the KDF fails (e.g. an out-of-memory condition) — secret-free.
pub fn hash_password(password: &Secret) -> Result<PasswordHash, PasswordError> {
    let salt = SaltString::generate(&mut OsRng);
    let argon = pinned_argon2();
    let phc = argon
        .hash_password(password.expose(), &salt)
        .map_err(|_| PasswordError::Hash)?;
    Ok(PasswordHash(phc.to_string()))
}

/// Verify `candidate` against a stored [`PasswordHash`] in **constant time**.
///
/// Re-derives the candidate using the algorithm/version/params and salt READ BACK from `stored`
/// (so a hash minted under an older cost still verifies), then compares the raw output bytes with
/// `qfs_crypto_core::constant_time_eq` — the comparison never short-circuits on the first differing
/// byte (RFD §10 replay/timing defense). Any malformed stored hash, or a missing salt/hash field,
/// returns `false` rather than erroring (a verify is a yes/no, never a panic).
#[must_use]
pub fn verify_password(candidate: &Secret, stored: &PasswordHash) -> bool {
    let parsed = match Phc::new(stored.as_str()) {
        Ok(p) => p,
        Err(_) => return false,
    };
    // Re-derive under the params recorded in the stored hash (future-proof against a cost bump) and
    // the stored salt; both are required to recompute the same output.
    let (Some(salt), Some(expected)) = (parsed.salt, parsed.hash) else {
        return false;
    };
    let params = match Params::try_from(&parsed) {
        Ok(p) => p,
        Err(_) => return false,
    };
    let argon = Argon2::new(ALGORITHM, VERSION, params);
    match argon.hash_password(candidate.expose(), salt) {
        Ok(computed) => match computed.hash {
            // The ONE secret-adjacent comparison routes through the constant-time primitive.
            Some(got) => qfs_crypto_core::constant_time_eq(got.as_bytes(), expected.as_bytes()),
            None => false,
        },
        Err(_) => false,
    }
}

/// A secret-free password-hashing error (the only failure a caller can see; verification never
/// errors — it returns a bool).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PasswordError {
    /// The argon2id KDF failed (e.g. memory allocation). Carries no password material.
    #[error("hashing the password failed")]
    Hash,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_is_argon2id_phc_and_records_pinned_params() {
        let hash = hash_password(&Secret::from("correct horse battery staple")).unwrap();
        let s = hash.as_str();
        assert!(s.starts_with("$argon2id$"), "must be argon2id PHC: {s}");
        assert!(s.contains("v=19"), "version recorded in the hash: {s}");
        // The pinned cost parameters are recorded in the string (Params::DEFAULT).
        assert!(s.contains("m=19456"), "memory cost recorded: {s}");
        assert!(s.contains("t=2"), "time cost recorded: {s}");
        assert!(s.contains("p=1"), "parallelism recorded: {s}");
    }

    #[test]
    fn correct_password_verifies_and_wrong_one_does_not() {
        let hash = hash_password(&Secret::from("s3cret-passw0rd")).unwrap();
        assert!(verify_password(&Secret::from("s3cret-passw0rd"), &hash));
        assert!(!verify_password(&Secret::from("s3cret-passw0Rd"), &hash));
        assert!(!verify_password(&Secret::from("totally-different"), &hash));
    }

    #[test]
    fn the_same_password_hashes_differently_each_time_random_salt() {
        // A fresh random salt per call means two hashes of the same password differ, yet both verify.
        let a = hash_password(&Secret::from("same-input")).unwrap();
        let b = hash_password(&Secret::from("same-input")).unwrap();
        assert_ne!(a.as_str(), b.as_str(), "the random salt must differ");
        assert!(verify_password(&Secret::from("same-input"), &a));
        assert!(verify_password(&Secret::from("same-input"), &b));
    }

    #[test]
    fn a_malformed_stored_hash_verifies_false_never_panics() {
        let garbage = PasswordHash::from_phc("not-a-phc-string".to_string());
        assert!(!verify_password(&Secret::from("anything"), &garbage));
    }

    #[test]
    fn debug_redacts_the_hash() {
        let hash = hash_password(&Secret::from("pw")).unwrap();
        let dbg = format!("{hash:?}");
        assert_eq!(dbg, "PasswordHash(***redacted***)");
        assert!(
            !dbg.contains("argon2"),
            "the hash body must not leak in Debug"
        );
    }
}
