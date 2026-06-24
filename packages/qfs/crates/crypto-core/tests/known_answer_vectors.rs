//! t34 single-sourcing guard: pin the cross-crate crypto vectors at the shared leaf so the three
//! former copies (objstore SigV4, slack signature, cron run-id) cannot drift after being
//! re-pointed here. These are the canonical FIPS 180-4 + RFC 4231 known-answer vectors plus the
//! end-to-end shapes each former consumer relied on.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use qfs_crypto_core::{constant_time_eq, hex_lower, hmac_sha256, sha256, sha256_hex};

#[test]
fn sha256_fips_180_4_vectors() {
    assert_eq!(
        sha256_hex(b""),
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
    assert_eq!(
        sha256_hex(b"abc"),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    assert_eq!(
        sha256_hex(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
        "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
    );
}

#[test]
fn hmac_sha256_rfc4231_test_case_2() {
    // The vector both objstore and slack pinned: key = "Jefe", data = "what do ya want for
    // nothing?". This is what the slack `X-Slack-Signature` verification and the SigV4 key
    // derivation both ultimately exercise.
    let tag = hmac_sha256(b"Jefe", b"what do ya want for nothing?");
    assert_eq!(
        hex_lower(&tag),
        "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
    );
}

#[test]
fn sha256_returns_32_bytes() {
    assert_eq!(sha256(b"anything").len(), 32);
}

#[test]
fn constant_time_eq_replay_defense_shape() {
    // The slack signature compare shape (`v0=<hex>` against the recomputed tag): equal, byte
    // mismatch, and length mismatch must all be handled, the latter two as `false`.
    assert!(constant_time_eq(b"v0=abcd", b"v0=abcd"));
    assert!(!constant_time_eq(b"v0=abcd", b"v0=abce"));
    assert!(!constant_time_eq(b"v0=abcd", b"v0=abc"));
    assert!(constant_time_eq(b"", b""));
}
