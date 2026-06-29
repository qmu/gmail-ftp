//! [`recipient_wrap`](self) — the pure **per-recipient (end-to-end) DEK wrap** primitive behind a
//! high-sensitivity connection's envelope (roadmap **decision U** / §4.5, ticket t80).
//!
//! ## What it is, and why it lives here
//! The default credential store (t43) wraps a connection's data-key (DEK) under ONE
//! passphrase-derived KEK the *server* re-derives, so the managed tier can execute a plan
//! unattended (decision C/F). For a connection too sensitive for that trust boundary, decision U
//! instead wraps the DEK **per recipient**: separately to each authorized member's PUBLIC key, so the
//! DEK is recoverable **only** by a member who holds the matching PRIVATE key — and **not by the
//! server at rest**. The opposite trust model to t43: the server storing the ciphertext cannot by
//! itself decrypt a high-sensitivity secret; only an authorized recipient can.
//!
//! This module is the **cryptographic recipient model**, not the DB store (that is binary-side, in
//! `crates/qfs/src/e2e_store.rs`) and not the lifecycle op (rotation/revocation is t79). It lives in
//! `qfs-oauth` because the vetted, pure-Rust RustCrypto `p256` tree already vendored here for ES256
//! (t48) ALSO provides ECDH (`p256::ecdh`) — so the asymmetric primitive reuses one curve crate
//! rather than hand-rolling key agreement or pulling a second curve (the non-negotiable). Keeping it
//! out of `qfs-secrets` preserves that leaf's wasm-buildable purity (it must not gain a `p256` edge).
//!
//! ## The scheme (ECIES-style sealed wrap over P-256 ECDH + ChaCha20-Poly1305)
//! To wrap the DEK to a recipient's public key `R`:
//!  1. Generate an **ephemeral** P-256 keypair `(e, E)` from caller-supplied OS entropy (the binary
//!     owns the CSPRNG — the same entropy-injection discipline [`crate::SigningKey::generate`] uses,
//!     keeping this leaf off a `rand`/`getrandom` edge).
//!  2. ECDH: `shared = e · R` (a point on the curve; `p256::ecdh::diffie_hellman`).
//!  3. Derive a per-recipient **KEK** = `SHA-256(shared.x ‖ E)` (the ephemeral public key is bound
//!     into the KDF so the wrap is tied to this exact exchange).
//!  4. AEAD-seal the DEK under the KEK with a fresh nonce: `WRAP_MAGIC ‖ E ‖ nonce ‖ ciphertext`.
//!
//! To unwrap, the recipient recomputes `shared = r · E` from their PRIVATE scalar `r` and the
//! embedded ephemeral public key `E`, derives the SAME KEK, and AEAD-opens the DEK. A NON-recipient
//! (no matching `r`) derives a different shared secret, hence a different KEK, and AEAD authentication
//! fails — they CANNOT unwrap. That is the whole E2E property, proven hermetically in the tests.
//!
//! ## Secret hygiene (RFD §10)
//! Every fallible op returns the value-free [`RecipientWrapError`] (no DEK, no scalar, no shared
//! secret ever enters an error). [`RecipientKey`] holds the private scalar only inside the
//! zeroized-on-drop `p256::SecretKey` and is intentionally NOT `Clone`/`Debug`/`Serialize`; the
//! private scalar leaves only inside a redacting [`Secret`]. Only PUBLIC key bytes are ever rendered.

use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use p256::ecdh::diffie_hellman;
use p256::elliptic_curve::sec1::ToEncodedPoint;
use p256::{PublicKey, SecretKey};
use qfs_secrets::Secret;

/// The P-256 secret-scalar / DEK width: 32 bytes.
const SCALAR_BYTES: usize = 32;
/// The AEAD nonce length (ChaCha20-Poly1305: 96-bit).
const NONCE_LEN: usize = 12;
/// The uncompressed SEC1 public-key encoding length (`0x04 ‖ X(32) ‖ Y(32)`).
const PUB_SEC1_LEN: usize = 65;
/// Magic + version prefix on a per-recipient wrapped DEK so a format change is detectable (distinct
/// from the envelope's `QFSDEK01` — a recipient wrap is a different artifact than a passphrase wrap).
const WRAP_MAGIC: &[u8] = b"QFSE2E01";

/// A value-free per-recipient-wrap failure (RFD §10): it names the failing *operation*, never a byte
/// of the DEK, the private scalar, or the ECDH shared secret. A bad public key, a malformed wrap, a
/// wrong-recipient unwrap, and a failed seal are deliberately coarse so nothing about the protected
/// material leaks through the error.
#[derive(Debug, thiserror::Error)]
pub enum RecipientWrapError {
    /// Recipient key material (a public key, an ephemeral scalar, or a stored private scalar) was not
    /// a valid P-256 point/scalar of the right length.
    #[error("recipient key material is invalid")]
    InvalidKey,
    /// The wrapped-DEK blob had a bad magic, was truncated, or carried a malformed ephemeral key.
    #[error("recipient DEK wrap is malformed")]
    Malformed,
    /// The wrap did not AEAD-open under the recipient's private key — a NON-recipient (wrong key) or a
    /// tampered wrap. This is the fail-closed E2E refusal; it leaks no bytes and is indistinguishable
    /// from a tamper.
    #[error("recipient DEK unwrap failed (wrong recipient key or corrupt wrap)")]
    Unwrap,
    /// Sealing the DEK under the derived per-recipient KEK failed (not reachable for a 32-byte DEK in
    /// practice).
    #[error("recipient DEK wrap sealing failed")]
    Seal,
}

/// A recipient's P-256 keypair: the private scalar (held only inside the zeroized-on-drop
/// `p256::SecretKey`, never serialized) plus the means to publish its PUBLIC key. The member keeps
/// this client-side; only [`RecipientKey::public_key_sec1`] is ever stored in `/sys/users`.
///
/// Intentionally NOT `Clone`/`Debug`/`Serialize` (it holds private key material): the private scalar
/// leaves only inside a redacting [`Secret`] via [`RecipientKey::secret_scalar`], mirroring
/// [`crate::SigningKey`].
pub struct RecipientKey {
    inner: SecretKey,
}

impl RecipientKey {
    /// Build a recipient keypair from 32 bytes of caller-supplied OS entropy (the binary owns the
    /// CSPRNG — the same discipline [`crate::SigningKey::generate`] follows, keeping this leaf off a
    /// `rand`/`getrandom` edge). The bytes are the P-256 secret scalar; a uniformly random 32-byte
    /// value is valid with overwhelming probability, and the negligible out-of-range case is reported
    /// as [`RecipientWrapError::InvalidKey`] (the caller retries with fresh entropy).
    ///
    /// # Errors
    /// [`RecipientWrapError::InvalidKey`] if `entropy` is zero or not a valid P-256 scalar.
    pub fn generate(entropy: &[u8; SCALAR_BYTES]) -> Result<Self, RecipientWrapError> {
        let inner =
            SecretKey::from_bytes(entropy.into()).map_err(|_| RecipientWrapError::InvalidKey)?;
        Ok(Self { inner })
    }

    /// Reconstruct a recipient keypair from the private scalar carried in a [`Secret`] (the member's
    /// at-rest reload path, decrypted client-side and never a bare `Vec<u8>`).
    ///
    /// # Errors
    /// [`RecipientWrapError::InvalidKey`] if the secret is not exactly 32 bytes or not a valid scalar.
    pub fn from_secret_scalar(secret: &Secret) -> Result<Self, RecipientWrapError> {
        let bytes: [u8; SCALAR_BYTES] = secret
            .expose()
            .try_into()
            .map_err(|_| RecipientWrapError::InvalidKey)?;
        let inner =
            SecretKey::from_bytes((&bytes).into()).map_err(|_| RecipientWrapError::InvalidKey)?;
        Ok(Self { inner })
    }

    /// The PUBLIC key as uncompressed SEC1 bytes (`0x04 ‖ X(32) ‖ Y(32)`, 65 bytes) — the only key
    /// material this type ever exposes. This is what a member registers in `/sys/users.public_key`
    /// and what [`wrap_dek_to_recipient`] consumes. Carries no private scalar.
    #[must_use]
    pub fn public_key_sec1(&self) -> Vec<u8> {
        self.inner
            .public_key()
            .to_encoded_point(false)
            .as_bytes()
            .to_vec()
    }

    /// The PRIVATE scalar re-wrapped in a fresh [`Secret`] so the member's client can persist it at
    /// rest only inside the redacting/zeroized wrapper — never as a bare `Vec<u8>`.
    #[must_use]
    pub fn secret_scalar(&self) -> Secret {
        Secret::new(self.inner.to_bytes().to_vec())
    }
}

/// Derive the per-recipient KEK from the ECDH shared secret and the ephemeral public key:
/// `SHA-256(shared.x ‖ ephemeral_pub_sec1)`. Binding the ephemeral public key into the KDF ties the
/// derived key to this exact exchange (anti-malleability), and SHA-256 over the raw shared
/// `x`-coordinate is the standard KDF shape for an ECIES-style wrap built on a vetted curve.
fn derive_wrap_kek(shared_secret: &[u8], ephemeral_pub_sec1: &[u8]) -> [u8; 32] {
    let mut input = Vec::with_capacity(shared_secret.len() + ephemeral_pub_sec1.len());
    input.extend_from_slice(shared_secret);
    input.extend_from_slice(ephemeral_pub_sec1);
    qfs_crypto_core::sha256(&input)
}

/// **Wrap a DEK to one recipient's public key** (decision U): ECDH-derive a per-recipient KEK from a
/// fresh ephemeral keypair and the recipient's public key, then AEAD-seal `dek` under it. The result
/// is `WRAP_MAGIC ‖ ephemeral_pub(65) ‖ nonce(12) ‖ ciphertext` — what lands in the binary's
/// per-recipient wrapped-DEK row. Without the recipient's PRIVATE key it reveals nothing about the
/// DEK; the server storing it cannot recover the DEK (the E2E property).
///
/// `ephemeral_entropy` (32 bytes) and `nonce` (12 bytes) are supplied by the caller's CSPRNG — the
/// binary owns randomness, keeping this leaf off a `rand` edge (mirrors [`crate::SigningKey`]). A
/// FRESH ephemeral keypair + nonce MUST be used per wrap.
///
/// # Errors
/// [`RecipientWrapError::InvalidKey`] if `recipient_pub_sec1` is not a valid P-256 point or
/// `ephemeral_entropy` is not a valid scalar; [`RecipientWrapError::Seal`] if the AEAD seal fails.
pub fn wrap_dek_to_recipient(
    recipient_pub_sec1: &[u8],
    dek: &Secret,
    ephemeral_entropy: &[u8; SCALAR_BYTES],
    nonce: &[u8; NONCE_LEN],
) -> Result<Vec<u8>, RecipientWrapError> {
    let recipient_pub = PublicKey::from_sec1_bytes(recipient_pub_sec1)
        .map_err(|_| RecipientWrapError::InvalidKey)?;
    let ephemeral = SecretKey::from_bytes(ephemeral_entropy.into())
        .map_err(|_| RecipientWrapError::InvalidKey)?;
    let ephemeral_pub_sec1 = ephemeral
        .public_key()
        .to_encoded_point(false)
        .as_bytes()
        .to_vec();

    // ECDH key agreement against the recipient's public key (vetted p256, NOT hand-rolled).
    let shared = diffie_hellman(ephemeral.to_nonzero_scalar(), recipient_pub.as_affine());
    let kek = derive_wrap_kek(shared.raw_secret_bytes().as_slice(), &ephemeral_pub_sec1);

    let cipher = ChaCha20Poly1305::new(Key::from_slice(&kek));
    let ct = cipher
        .encrypt(Nonce::from_slice(nonce), dek.expose())
        .map_err(|_| RecipientWrapError::Seal)?;

    let mut out = Vec::with_capacity(WRAP_MAGIC.len() + PUB_SEC1_LEN + NONCE_LEN + ct.len());
    out.extend_from_slice(WRAP_MAGIC);
    out.extend_from_slice(&ephemeral_pub_sec1);
    out.extend_from_slice(nonce);
    out.extend_from_slice(&ct);
    Ok(out)
}

/// **Unwrap a per-recipient wrapped DEK** with the recipient's private key (decision U): recompute
/// the ECDH shared secret from the embedded ephemeral public key and the recipient's PRIVATE scalar,
/// derive the SAME KEK, and AEAD-open the DEK into a redacting [`Secret`].
///
/// A NON-recipient (a different private key) derives a different shared secret ⇒ a different KEK ⇒ the
/// AEAD open FAILS with [`RecipientWrapError::Unwrap`] — they CANNOT unwrap (the E2E property). A
/// tampered wrap fails the same way. The recovered DEK lives only inside the returned `Secret`.
///
/// # Errors
/// [`RecipientWrapError::Malformed`] on a bad magic, truncation, or a malformed ephemeral key;
/// [`RecipientWrapError::Unwrap`] when the wrap does not open under this recipient's key (wrong
/// recipient or tampered). The error names no key material (the causes are indistinguishable).
pub fn unwrap_dek_for_recipient(
    recipient: &RecipientKey,
    wrapped: &[u8],
) -> Result<Secret, RecipientWrapError> {
    let rest = wrapped
        .strip_prefix(WRAP_MAGIC)
        .ok_or(RecipientWrapError::Malformed)?;
    if rest.len() < PUB_SEC1_LEN + NONCE_LEN {
        return Err(RecipientWrapError::Malformed);
    }
    let (ephemeral_pub_sec1, rest) = rest.split_at(PUB_SEC1_LEN);
    let (nonce, ct) = rest.split_at(NONCE_LEN);

    let ephemeral_pub = PublicKey::from_sec1_bytes(ephemeral_pub_sec1)
        .map_err(|_| RecipientWrapError::Malformed)?;
    let shared = diffie_hellman(
        recipient.inner.to_nonzero_scalar(),
        ephemeral_pub.as_affine(),
    );
    let kek = derive_wrap_kek(shared.raw_secret_bytes().as_slice(), ephemeral_pub_sec1);

    let cipher = ChaCha20Poly1305::new(Key::from_slice(&kek));
    let plaintext = cipher
        .decrypt(Nonce::from_slice(nonce), ct)
        .map_err(|_| RecipientWrapError::Unwrap)?;
    Ok(Secret::new(plaintext))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two distinct, fixed, non-zero scalars so the keypairs (and thus every wrap/unwrap assertion)
    /// are stable golden vectors across runs — recipient A and a different recipient B.
    const SCALAR_A: [u8; 32] = [
        0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff,
        0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f,
        0x10, 0x20,
    ];
    const SCALAR_B: [u8; 32] = [
        0xa1, 0xb2, 0xc3, 0xd4, 0xe5, 0xf6, 0x07, 0x18, 0x29, 0x3a, 0x4b, 0x5c, 0x6d, 0x7e, 0x8f,
        0x90, 0x0a, 0x1b, 0x2c, 0x3d, 0x4e, 0x5f, 0x60, 0x71, 0x82, 0x93, 0xa4, 0xb5, 0xc6, 0xd7,
        0xe8, 0xf9,
    ];
    /// A fixed ephemeral scalar + nonce for the wrap (the binary supplies these from the CSPRNG; the
    /// tests pin them so the wrap is deterministic and re-runs are byte-stable).
    const EPHEMERAL: [u8; 32] = [
        0x3c, 0x4d, 0x5e, 0x6f, 0x70, 0x81, 0x92, 0xa3, 0xb4, 0xc5, 0xd6, 0xe7, 0xf8, 0x09, 0x1a,
        0x2b, 0x3c, 0x4d, 0x5e, 0x6f, 0x70, 0x81, 0x92, 0xa3, 0xb4, 0xc5, 0xd6, 0xe7, 0xf8, 0x09,
        0x1a, 0x2b,
    ];
    const NONCE: [u8; 12] = [9, 8, 7, 6, 5, 4, 3, 2, 1, 0, 11, 12];
    /// A planted DEK value, unmistakable if it ever surfaced in an error.
    const DEK: &[u8] = b"0123456789abcdef0123456789ABCDEF"; // 32 bytes

    /// THE E2E property: a DEK wrapped to recipient A's PUBLIC key unwraps to the SAME DEK with A's
    /// PRIVATE key — and recipient B (a different keypair) CANNOT unwrap it (fail-closed, no leak).
    #[test]
    fn a_recipient_unwraps_its_own_wrap_and_a_non_recipient_cannot() {
        let alice = RecipientKey::generate(&SCALAR_A).unwrap();
        let bob = RecipientKey::generate(&SCALAR_B).unwrap();

        let wrapped = wrap_dek_to_recipient(
            &alice.public_key_sec1(),
            &Secret::new(DEK.to_vec()),
            &EPHEMERAL,
            &NONCE,
        )
        .unwrap();

        // Alice (the recipient) recovers the exact DEK.
        let recovered = unwrap_dek_for_recipient(&alice, &wrapped).unwrap();
        assert_eq!(recovered.expose(), DEK, "the recipient recovers the DEK");

        // Bob (NOT the recipient) cannot — a different private key derives a different KEK.
        let err = unwrap_dek_for_recipient(&bob, &wrapped).unwrap_err();
        assert!(
            matches!(err, RecipientWrapError::Unwrap),
            "a non-recipient must fail to unwrap, got {err:?}"
        );
    }

    /// The wrapped blob (the only thing the SERVER stores) does NOT contain the DEK in the clear — the
    /// server-stored ciphertext alone cannot yield the DEK without a recipient private key.
    #[test]
    fn the_wrapped_blob_does_not_contain_the_dek_in_the_clear() {
        let alice = RecipientKey::generate(&SCALAR_A).unwrap();
        let wrapped = wrap_dek_to_recipient(
            &alice.public_key_sec1(),
            &Secret::new(DEK.to_vec()),
            &EPHEMERAL,
            &NONCE,
        )
        .unwrap();
        assert!(
            !wrapped.windows(DEK.len()).any(|w| w == DEK),
            "the DEK leaked into the per-recipient wrap"
        );
    }

    /// The private scalar round-trips through a [`Secret`] back into the SAME keypair (the member's
    /// at-rest reload path), and the public key it republishes matches.
    #[test]
    fn the_private_scalar_round_trips_through_a_secret() {
        let key = RecipientKey::generate(&SCALAR_A).unwrap();
        let reloaded = RecipientKey::from_secret_scalar(&key.secret_scalar()).unwrap();
        assert_eq!(
            key.public_key_sec1(),
            reloaded.public_key_sec1(),
            "the reloaded keypair republishes the same public key"
        );
        // And the reload still unwraps a wrap made to the original public key.
        let wrapped = wrap_dek_to_recipient(
            &key.public_key_sec1(),
            &Secret::new(DEK.to_vec()),
            &EPHEMERAL,
            &NONCE,
        )
        .unwrap();
        assert_eq!(
            unwrap_dek_for_recipient(&reloaded, &wrapped)
                .unwrap()
                .expose(),
            DEK
        );
    }

    /// A tampered wrap byte fails authentication (AEAD integrity) — never a partial DEK.
    #[test]
    fn a_tampered_wrap_fails_to_unwrap() {
        let alice = RecipientKey::generate(&SCALAR_A).unwrap();
        let mut wrapped = wrap_dek_to_recipient(
            &alice.public_key_sec1(),
            &Secret::new(DEK.to_vec()),
            &EPHEMERAL,
            &NONCE,
        )
        .unwrap();
        let last = wrapped.len() - 1;
        wrapped[last] ^= 0x01;
        assert!(matches!(
            unwrap_dek_for_recipient(&alice, &wrapped),
            Err(RecipientWrapError::Unwrap)
        ));
    }

    /// A malformed/truncated/zero-scalar input is a clean error, never a panic; and the error names no
    /// key material (redaction holds on the recipient-wrap surface).
    #[test]
    fn malformed_inputs_are_clean_errors_and_carry_no_material() {
        let alice = RecipientKey::generate(&SCALAR_A).unwrap();
        // Bad magic / truncation.
        assert!(matches!(
            unwrap_dek_for_recipient(&alice, b"not-a-wrap"),
            Err(RecipientWrapError::Malformed)
        ));
        assert!(matches!(
            unwrap_dek_for_recipient(&alice, WRAP_MAGIC),
            Err(RecipientWrapError::Malformed)
        ));
        // A zero scalar is rejected, not silently accepted.
        assert!(matches!(
            RecipientKey::generate(&[0u8; 32]),
            Err(RecipientWrapError::InvalidKey)
        ));
        // A non-point public key is rejected on wrap.
        assert!(matches!(
            wrap_dek_to_recipient(
                b"not-a-point",
                &Secret::new(DEK.to_vec()),
                &EPHEMERAL,
                &NONCE
            ),
            Err(RecipientWrapError::InvalidKey)
        ));
        // Every error renders DEK-free.
        for e in [
            RecipientWrapError::InvalidKey,
            RecipientWrapError::Malformed,
            RecipientWrapError::Unwrap,
            RecipientWrapError::Seal,
        ] {
            let rendered = format!("{e:?} {e}");
            assert!(!rendered.contains("0123456789abcdef"));
        }
    }
}
