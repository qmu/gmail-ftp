//! [`envelope`](self) — the pure **envelope-encryption** primitive behind the SQLite
//! credential store (roadmap §4.2, ticket t43).
//!
//! Envelope encryption splits the at-rest secret into two layers so the expensive,
//! passphrase-bound step happens **once**, not per secret:
//!
//! - A random 32-byte **data-key (DEK)** — generated once with [`generate_dek`] — is the
//!   key that actually encrypts every secret value ([`seal`] / [`open`]). Each value gets a
//!   fresh random nonce, so two equal plaintexts never produce equal ciphertext.
//! - The DEK is itself sealed under a **key-encryption-key (KEK)** derived from the user's
//!   passphrase with argon2id ([`derive_kek`]) over a stored per-store salt, then
//!   [`wrap_dek`]ped into the `secret_meta` row. Unlocking the store is a single
//!   [`derive_kek`] + [`unwrap_dek`]; rotating the passphrase re-wraps the SAME DEK without
//!   touching a single secret column.
//!
//! This module is **pure**: no filesystem, no DB, no tokio. It mirrors `local.rs`'s crypto
//! choices verbatim — ChaCha20-Poly1305 AEAD + argon2id KDF + `rand` CSPRNG nonces — so the
//! two backends share one threat model. Like `local.rs` it is `cfg(not(target_arch = "wasm32"))`:
//! the AEAD/KDF code is pure Rust, but its `rand`/`getrandom` CSPRNG has no default Workers backend,
//! and the SQLite store that consumes it lives in the native binary (Workers use `WorkerStore` and
//! never need the envelope). Confining it keeps qfs-secrets wasm-buildable — a documented invariant.
//!
//! ## Secret hygiene
//! Every fallible operation returns the value-free [`EnvelopeError`] (a wrong KEK, a tampered
//! wrap, or a corrupt ciphertext are indistinguishable from the outside — no bytes, no
//! position, no length of the protected material leak through it).

use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use rand::Rng;

/// AEAD / data-key length (ChaCha20-Poly1305: 256-bit).
const KEY_LEN: usize = 32;
/// AEAD nonce length (96-bit).
const NONCE_LEN: usize = 12;
/// The per-store KDF salt length (16 bytes — comfortably above argon2's 8-byte floor).
const SALT_LEN: usize = 16;
/// Magic + version prefix on a wrapped DEK, so a format change is detectable (distinct from
/// `local.rs`'s blob magic — a wrapped DEK is a different artifact than a vault blob).
const WRAP_MAGIC: &[u8] = b"QFSDEK01";

/// A value-free envelope-crypto failure: a wrong KEK, a tampered wrap, a corrupt ciphertext,
/// or an unknown/truncated wrap format. Carries **no** bytes of the protected material (RFD
/// §10 — secrets never enter an error). The three failure causes are deliberately
/// indistinguishable to the caller.
#[derive(Debug, thiserror::Error)]
#[error("envelope crypto operation failed (wrong key or corrupt data)")]
pub struct EnvelopeError;

/// Generate a fresh random 32-byte **data-key (DEK)** from the OS CSPRNG. This is the key
/// that encrypts every secret value; it is itself wrapped under the passphrase-derived KEK
/// ([`wrap_dek`]) before it ever touches storage, so the raw DEK lives only in process memory.
#[must_use]
pub fn generate_dek() -> [u8; KEY_LEN] {
    let mut dek = [0u8; KEY_LEN];
    rand::rng().fill_bytes(&mut dek);
    dek
}

/// Generate a fresh random 16-byte **KDF salt** from the OS CSPRNG. Persisted once alongside
/// the wrapped DEK so the same passphrase reproduces the same KEK on reopen. A salt is public
/// metadata (not a secret), but it must be unpredictable per store, hence the CSPRNG.
#[must_use]
pub fn generate_salt() -> [u8; SALT_LEN] {
    let mut salt = [0u8; SALT_LEN];
    rand::rng().fill_bytes(&mut salt);
    salt
}

/// Derive a 32-byte **key-encryption-key (KEK)** from `passphrase` + `salt` with argon2id —
/// the same KDF `local.rs` uses for its at-rest key. The KEK never touches storage; it is
/// re-derived on each unlock and used only to [`wrap_dek`] / [`unwrap_dek`] the DEK.
///
/// # Errors
/// [`EnvelopeError`] if argon2 rejects the inputs (e.g. a too-short salt). The error names no
/// material.
pub fn derive_kek(passphrase: &[u8], salt: &[u8]) -> Result<[u8; KEY_LEN], EnvelopeError> {
    use argon2::Argon2;
    let argon = Argon2::default();
    let mut kek = [0u8; KEY_LEN];
    argon
        .hash_password_into(passphrase, salt, &mut kek)
        .map_err(|_| EnvelopeError)?;
    Ok(kek)
}

/// AEAD-seal the DEK under the KEK: `WRAP_MAGIC || nonce || ciphertext`, a fresh random nonce
/// per call. The result is what lands in `secret_meta.wrapped_dek`; without the KEK it reveals
/// nothing about the DEK.
///
/// # Errors
/// [`EnvelopeError`] if the AEAD seal fails (not reachable for a 32-byte input in practice).
pub fn wrap_dek(kek: &[u8; KEY_LEN], dek: &[u8; KEY_LEN]) -> Result<Vec<u8>, EnvelopeError> {
    let cipher = ChaCha20Poly1305::new(Key::from_slice(kek));
    let mut nonce_bytes = [0u8; NONCE_LEN];
    rand::rng().fill_bytes(&mut nonce_bytes);
    let nonce = Nonce::from_slice(&nonce_bytes);
    let ct = cipher
        .encrypt(nonce, dek.as_slice())
        .map_err(|_| EnvelopeError)?;
    let mut out = Vec::with_capacity(WRAP_MAGIC.len() + NONCE_LEN + ct.len());
    out.extend_from_slice(WRAP_MAGIC);
    out.extend_from_slice(&nonce_bytes);
    out.extend_from_slice(&ct);
    Ok(out)
}

/// AEAD-open a [`wrap_dek`]ped DEK with the KEK. A wrong KEK, a tampered wrap, or an
/// unknown/truncated format all fail authentication and return [`EnvelopeError`] — never a
/// partial DEK, never a distinguishing detail.
///
/// # Errors
/// [`EnvelopeError`] on a bad magic, truncation, or a failed AEAD open.
pub fn unwrap_dek(kek: &[u8; KEY_LEN], wrapped: &[u8]) -> Result<[u8; KEY_LEN], EnvelopeError> {
    let rest = wrapped.strip_prefix(WRAP_MAGIC).ok_or(EnvelopeError)?;
    if rest.len() < NONCE_LEN {
        return Err(EnvelopeError);
    }
    let (nonce_bytes, ct) = rest.split_at(NONCE_LEN);
    let cipher = ChaCha20Poly1305::new(Key::from_slice(kek));
    let nonce = Nonce::from_slice(nonce_bytes);
    let plaintext = cipher.decrypt(nonce, ct).map_err(|_| EnvelopeError)?;
    let dek: [u8; KEY_LEN] = plaintext.as_slice().try_into().map_err(|_| EnvelopeError)?;
    Ok(dek)
}

/// **Re-wrap** a [`wrap_dek`]ped DEK from the old KEK to a new KEK WITHOUT re-encrypting a single
/// secret value (t79 — credential rotation / passphrase change). Unwrap the DEK under `old_kek`, then
/// wrap the SAME DEK under `new_kek` with a fresh nonce. This is the whole point of envelope
/// encryption: a passphrase change (or a scheduled key rotation) is ONE re-wrap of the single
/// `secret_meta.wrapped_dek`, not N re-seals of every secret column — and because the DEK is
/// unchanged, every value sealed under it ([`seal`]) still [`open`]s afterwards.
///
/// A **wrong** `old_kek` fails authentication in [`unwrap_dek`] and returns [`EnvelopeError`] BEFORE
/// any re-wrap happens — there is no silent re-key under a wrong old passphrase. The raw DEK lives
/// only transiently on the stack here and never touches storage or a log.
///
/// # Errors
/// [`EnvelopeError`] if `old_kek` cannot unwrap `wrapped` (wrong key / tampered / unknown format) or
/// the re-wrap seal fails. The error names no key material (the three causes are indistinguishable).
pub fn rewrap_dek(
    old_kek: &[u8; KEY_LEN],
    new_kek: &[u8; KEY_LEN],
    wrapped: &[u8],
) -> Result<Vec<u8>, EnvelopeError> {
    let dek = unwrap_dek(old_kek, wrapped)?;
    wrap_dek(new_kek, &dek)
}

/// AEAD-seal one secret value under the DEK with a **fresh random nonce**. Returns the nonce
/// (stored beside the ciphertext) and the ciphertext (the `secret_store` columns). Per-value
/// nonces mean two equal plaintexts never share ciphertext.
///
/// # Errors
/// [`EnvelopeError`] if the AEAD seal fails.
pub fn seal(
    dek: &[u8; KEY_LEN],
    plaintext: &[u8],
) -> Result<([u8; NONCE_LEN], Vec<u8>), EnvelopeError> {
    let cipher = ChaCha20Poly1305::new(Key::from_slice(dek));
    let mut nonce_bytes = [0u8; NONCE_LEN];
    rand::rng().fill_bytes(&mut nonce_bytes);
    let nonce = Nonce::from_slice(&nonce_bytes);
    let ct = cipher
        .encrypt(nonce, plaintext)
        .map_err(|_| EnvelopeError)?;
    Ok((nonce_bytes, ct))
}

/// AEAD-open one secret value sealed by [`seal`]. `nonce` is the stored per-value nonce (a
/// blob from the DB); a wrong length, a wrong DEK, or a tampered ciphertext fail
/// authentication and return [`EnvelopeError`] without leaking bytes.
///
/// # Errors
/// [`EnvelopeError`] on a wrong nonce length or a failed AEAD open.
pub fn open(
    dek: &[u8; KEY_LEN],
    nonce: &[u8],
    ciphertext: &[u8],
) -> Result<Vec<u8>, EnvelopeError> {
    if nonce.len() != NONCE_LEN {
        return Err(EnvelopeError);
    }
    let cipher = ChaCha20Poly1305::new(Key::from_slice(dek));
    let nonce = Nonce::from_slice(nonce);
    cipher.decrypt(nonce, ciphertext).map_err(|_| EnvelopeError)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole envelope round-trips: a passphrase-derived KEK wraps then unwraps the DEK,
    /// and that DEK seals then opens a value back to the original plaintext.
    #[test]
    fn dek_wrap_and_value_seal_round_trip() {
        let salt = generate_salt();
        let kek = derive_kek(b"correct horse battery staple", &salt).unwrap();
        let dek = generate_dek();

        let wrapped = wrap_dek(&kek, &dek).unwrap();
        let unwrapped = unwrap_dek(&kek, &wrapped).unwrap();
        assert_eq!(unwrapped, dek, "the unwrapped DEK matches the original");

        let plaintext = b"ghp_super_secret_token";
        let (nonce, ct) = seal(&dek, plaintext).unwrap();
        let opened = open(&dek, &nonce, &ct).unwrap();
        assert_eq!(opened, plaintext, "the opened value matches the original");

        // The ciphertext must NOT contain the plaintext (it is genuinely encrypted).
        assert!(
            !ct.windows(plaintext.len()).any(|w| w == plaintext),
            "plaintext leaked into the ciphertext"
        );
    }

    /// A fresh nonce per seal means two equal plaintexts under the same DEK produce different
    /// ciphertext — no equality oracle across secret columns.
    #[test]
    fn equal_plaintexts_seal_to_different_ciphertext() {
        let dek = generate_dek();
        let (n1, c1) = seal(&dek, b"same").unwrap();
        let (n2, c2) = seal(&dek, b"same").unwrap();
        assert!(
            n1 != n2 || c1 != c2,
            "nonces (and thus ciphertexts) must differ"
        );
        // Both still open to the same value.
        assert_eq!(open(&dek, &n1, &c1).unwrap(), b"same");
        assert_eq!(open(&dek, &n2, &c2).unwrap(), b"same");
    }

    /// A DEK re-wrap (t79) rotates the KEK without re-encrypting any value: a value sealed under
    /// the DEK, then a re-wrap of that DEK from the old KEK to a NEW KEK, still opens the value;
    /// the old KEK no longer unwraps the new wrapped-DEK, and the new KEK does.
    #[test]
    fn rewrap_rotates_the_kek_but_keeps_values_decryptable() {
        let salt = generate_salt();
        let old_kek = derive_kek(b"old passphrase", &salt).unwrap();
        let dek = generate_dek();
        let wrapped = wrap_dek(&old_kek, &dek).unwrap();

        // A value sealed under the DEK (the DEK never changes across a re-wrap).
        let (nonce, ct) = seal(&dek, b"ghp_rotme").unwrap();

        // Re-wrap the DEK under a NEW KEK (a fresh salt — a passphrase change).
        let new_salt = generate_salt();
        let new_kek = derive_kek(b"new passphrase", &new_salt).unwrap();
        let rewrapped = rewrap_dek(&old_kek, &new_kek, &wrapped).unwrap();

        // The NEW KEK unwraps the re-wrapped DEK, and it is byte-identical (the same DEK).
        let recovered = unwrap_dek(&new_kek, &rewrapped).unwrap();
        assert_eq!(recovered, dek, "the re-wrapped DEK is the same data-key");
        // The value sealed before the re-wrap still opens (no re-seal needed).
        assert_eq!(open(&recovered, &nonce, &ct).unwrap(), b"ghp_rotme");
        // The OLD KEK no longer unwraps the new wrapped-DEK.
        assert!(unwrap_dek(&old_kek, &rewrapped).is_err());
    }

    /// A wrong OLD KEK refuses the re-wrap (no silent re-key) — authentication fails first.
    #[test]
    fn rewrap_under_a_wrong_old_kek_is_refused() {
        let salt = generate_salt();
        let old_kek = derive_kek(b"right", &salt).unwrap();
        let dek = generate_dek();
        let wrapped = wrap_dek(&old_kek, &dek).unwrap();

        let wrong_old = derive_kek(b"wrong", &salt).unwrap();
        let new_kek = derive_kek(b"new", &generate_salt()).unwrap();
        assert!(
            rewrap_dek(&wrong_old, &new_kek, &wrapped).is_err(),
            "a wrong old KEK must not re-wrap the DEK"
        );
    }

    /// A wrong KEK fails to unwrap the DEK — authentication fails, no bytes leak.
    #[test]
    fn wrong_kek_fails_to_unwrap_without_leaking() {
        let salt = generate_salt();
        let kek = derive_kek(b"right", &salt).unwrap();
        let dek = generate_dek();
        let wrapped = wrap_dek(&kek, &dek).unwrap();

        let wrong = derive_kek(b"wrong", &salt).unwrap();
        let err = unwrap_dek(&wrong, &wrapped).unwrap_err();
        // The error is value-free: it names no key material.
        assert_eq!(
            err.to_string(),
            "envelope crypto operation failed (wrong key or corrupt data)"
        );
    }

    /// A tampered wrapped-DEK byte fails authentication (AEAD integrity).
    #[test]
    fn tampered_wrap_fails() {
        let salt = generate_salt();
        let kek = derive_kek(b"pass", &salt).unwrap();
        let dek = generate_dek();
        let mut wrapped = wrap_dek(&kek, &dek).unwrap();
        // Flip a bit in the ciphertext tail.
        let last = wrapped.len() - 1;
        wrapped[last] ^= 0x01;
        assert!(unwrap_dek(&kek, &wrapped).is_err());
    }

    /// A tampered secret ciphertext fails to open (per-value AEAD integrity).
    #[test]
    fn tampered_value_ciphertext_fails() {
        let dek = generate_dek();
        let (nonce, mut ct) = seal(&dek, b"token").unwrap();
        ct[0] ^= 0xff;
        assert!(open(&dek, &nonce, &ct).is_err());
    }

    /// A wrong-length nonce is rejected before touching the cipher (no panic).
    #[test]
    fn wrong_nonce_length_is_rejected() {
        let dek = generate_dek();
        let (_n, ct) = seal(&dek, b"token").unwrap();
        assert!(open(&dek, &[0u8; 4], &ct).is_err());
    }

    /// An unknown/truncated wrap format is a clean error, not a panic.
    #[test]
    fn unknown_wrap_format_is_an_error() {
        let kek = [3u8; KEY_LEN];
        assert!(unwrap_dek(&kek, b"not-a-wrap").is_err());
        assert!(unwrap_dek(&kek, WRAP_MAGIC).is_err()); // magic but truncated
    }
}
