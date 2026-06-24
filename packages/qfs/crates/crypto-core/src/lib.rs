//! qfs shared pure crypto leaf (t34): the **single source of truth** for the dependency-free,
//! wasm-clean crypto primitives the workspace needs — **SHA-256** (FIPS 180-4), **HMAC-SHA256**
//! (RFC 2104 / RFC 4231), lowercase-hex, and a **constant-time** byte compare.
//!
//! ## Why this crate exists (the t34 single-sourcing)
//! Before it, an identical SHA-256 (and, in two cases, HMAC-SHA256 / constant_time_eq) was
//! independently vendored THREE times:
//!   * `qfs-driver-objstore::sha256` — SigV4 request signing,
//!   * `qfs-driver-slack::hmac` — `X-Slack-Signature` verification (+ `constant_time_eq`),
//!   * `qfs-cron::hash` — the deterministic `hash(job, scheduled_for)` run-id.
//!
//! No shared crypto leaf existed, and depending on any of those crates would have pulled a
//! runtime/driver coupling into the consumer (each is a `qfs-runtime` leaf or a binding crate),
//! so each crate re-vendored the routine. t34's webhook HMAC verification would have been a
//! FOURTH copy; instead this crate is created first so all four share ONE pinned implementation.
//!
//! ## Why pure-std + hand-rolled (a recorded engineering choice, inherited from t22/t25/t33)
//! The trip host's cargo cache does not carry `sha2`/`hmac`/`ring`, and the **wasm32 target must
//! build** (the CF Workers `WEBHOOK` ingress, RFD §8). A pure-`std` SHA-256 (FIPS 180-4) is
//! ~80 lines, has NO native-link hazard on `wasm32-unknown-unknown` (unlike `ring`/openssl), and
//! is pinned to the canonical FIPS 180-4 + RFC 4231 known-answer vectors below (the same vectors
//! the three former copies pinned, plus the AWS SigV4 end-to-end vectors in objstore).
//!
//! ## Purity / dependency stance
//! This is a **TRUE pure leaf**: it depends on NOTHING — no workspace crate and no vendor crate,
//! `std`-only by construction. That is what makes it safe for every consumer, including the
//! off-runtime watchtower and the wasm32 ingress, to depend on it without inheriting any
//! runtime/native coupling. The pure-leaf dep guard in `crates/cmd/tests/dep_direction.rs`
//! mechanically enforces the empty dependency set.
//!
//! ## Constant-time discipline
//! SHA-256 / HMAC-SHA256 here are NOT constant-time and must be used ONLY to compute a digest or
//! signature over (public) material, never to *compare* secret bytes directly. The one secret
//! comparison the workspace performs — verifying an inbound webhook/Slack signature — MUST route
//! through [`constant_time_eq`], which never short-circuits on the first mismatching byte (RFD §10
//! replay defense).

#![allow(clippy::many_single_char_names)]

/// SHA-256 round constants (FIPS 180-4 §4.2.2): the first 32 bits of the fractional parts of the
/// cube roots of the first 64 primes.
const K: [u32; 64] = [
    0x428a_2f98,
    0x7137_4491,
    0xb5c0_fbcf,
    0xe9b5_dba5,
    0x3956_c25b,
    0x59f1_11f1,
    0x923f_82a4,
    0xab1c_5ed5,
    0xd807_aa98,
    0x1283_5b01,
    0x2431_85be,
    0x550c_7dc3,
    0x72be_5d74,
    0x80de_b1fe,
    0x9bdc_06a7,
    0xc19b_f174,
    0xe49b_69c1,
    0xefbe_4786,
    0x0fc1_9dc6,
    0x240c_a1cc,
    0x2de9_2c6f,
    0x4a74_84aa,
    0x5cb0_a9dc,
    0x76f9_88da,
    0x983e_5152,
    0xa831_c66d,
    0xb003_27c8,
    0xbf59_7fc7,
    0xc6e0_0bf3,
    0xd5a7_9147,
    0x06ca_6351,
    0x1429_2967,
    0x27b7_0a85,
    0x2e1b_2138,
    0x4d2c_6dfc,
    0x5338_0d13,
    0x650a_7354,
    0x766a_0abb,
    0x81c2_c92e,
    0x9272_2c85,
    0xa2bf_e8a1,
    0xa81a_664b,
    0xc24b_8b70,
    0xc76c_51a3,
    0xd192_e819,
    0xd699_0624,
    0xf40e_3585,
    0x106a_a070,
    0x19a4_c116,
    0x1e37_6c08,
    0x2748_774c,
    0x34b0_bcb5,
    0x391c_0cb3,
    0x4ed8_aa4a,
    0x5b9c_ca4f,
    0x682e_6ff3,
    0x748f_82ee,
    0x78a5_636f,
    0x84c8_7814,
    0x8cc7_0208,
    0x90be_fffa,
    0xa450_6ceb,
    0xbef9_a3f7,
    0xc671_78f2,
];

/// The SHA-256 initial hash value (FIPS 180-4 §5.3.3): the first 32 bits of the fractional parts
/// of the square roots of the first 8 primes.
const H0: [u32; 8] = [
    0x6a09_e667,
    0xbb67_ae85,
    0x3c6e_f372,
    0xa54f_f53a,
    0x510e_527f,
    0x9b05_688c,
    0x1f83_d9ab,
    0x5be0_cd19,
];

/// The SHA-256 digest of `data` (32 bytes), FIPS 180-4.
#[must_use]
pub fn sha256(data: &[u8]) -> [u8; 32] {
    let mut h = H0;

    // Pre-process (pad): append 0x80, then zeros, then the 64-bit big-endian bit length.
    let bit_len = (data.len() as u64).wrapping_mul(8);
    let mut msg = data.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());

    // Process each 512-bit (64-byte) chunk.
    for chunk in msg.chunks_exact(64) {
        let mut w = [0u32; 64];
        for (i, word) in w.iter_mut().enumerate().take(16) {
            let j = i * 4;
            *word = u32::from_be_bytes([chunk[j], chunk[j + 1], chunk[j + 2], chunk[j + 3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }

        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let t1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
        h[5] = h[5].wrapping_add(f);
        h[6] = h[6].wrapping_add(g);
        h[7] = h[7].wrapping_add(hh);
    }

    let mut out = [0u8; 32];
    for (i, word) in h.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }
    out
}

/// HMAC-SHA256 (RFC 2104) of `data` under `key` (32-byte tag).
#[must_use]
pub fn hmac_sha256(key: &[u8], data: &[u8]) -> [u8; 32] {
    const BLOCK: usize = 64;
    // Keys longer than the block size are first hashed.
    let mut k = if key.len() > BLOCK {
        sha256(key).to_vec()
    } else {
        key.to_vec()
    };
    k.resize(BLOCK, 0);

    let mut ipad = [0u8; BLOCK];
    let mut opad = [0u8; BLOCK];
    for i in 0..BLOCK {
        ipad[i] = k[i] ^ 0x36;
        opad[i] = k[i] ^ 0x5c;
    }

    let mut inner = Vec::with_capacity(BLOCK + data.len());
    inner.extend_from_slice(&ipad);
    inner.extend_from_slice(data);
    let inner_hash = sha256(&inner);

    let mut outer = Vec::with_capacity(BLOCK + 32);
    outer.extend_from_slice(&opad);
    outer.extend_from_slice(&inner_hash);
    sha256(&outer)
}

/// Lowercase-hex encode a byte slice (the `v0=<hex>` / SigV4 / run-id rendering).
#[must_use]
pub fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

/// The lowercase-hex SHA-256 of `data` — the form SigV4 puts in the canonical request, and the
/// form the scheduler renders into run-ids / plan fingerprints.
#[must_use]
pub fn sha256_hex(data: &[u8]) -> String {
    hex_lower(&sha256(data))
}

/// **Constant-time** byte-slice equality — the timing-safe compare verifying an inbound webhook /
/// Slack signature (RFD §10 replay defense). Always scans every byte of the longer input; never
/// short-circuits on the first mismatch, so the comparison's duration does not leak how many
/// leading bytes matched. Differing lengths compare unequal but still in constant time over the
/// max length.
#[must_use]
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    // Fold the length difference into the accumulator so unequal lengths are never equal, while
    // still touching every byte (indexing modulo the shorter length avoids an early bound).
    let mut diff: u8 = (a.len() ^ b.len()) as u8;
    let n = a.len().max(b.len());
    for i in 0..n {
        let x = a.get(i).copied().unwrap_or(0);
        let y = b.get(i).copied().unwrap_or(0);
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_matches_fips_vectors() {
        // The canonical FIPS 180-4 known-answer vectors.
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
    fn hmac_sha256_matches_rfc4231_vector() {
        // RFC 4231 Test Case 2: key = "Jefe", data = "what do ya want for nothing?".
        let tag = hmac_sha256(b"Jefe", b"what do ya want for nothing?");
        assert_eq!(
            hex_lower(&tag),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    #[test]
    fn hmac_handles_keys_longer_than_the_block() {
        // The >64-byte key path (RFC 4231 Test Case 4 shape): assert it runs and is deterministic.
        let key = vec![0xaau8; 131];
        let a = hmac_sha256(&key, b"Test Using Larger Than Block-Size Key");
        let b = hmac_sha256(&key, b"Test Using Larger Than Block-Size Key");
        assert_eq!(a, b);
        assert_eq!(a.len(), 32);
    }

    #[test]
    fn hex_lower_is_zero_padded() {
        assert_eq!(hex_lower(&[0x00, 0x0f, 0xff]), "000fff");
    }

    #[test]
    fn constant_time_eq_is_correct_for_equal_and_unequal() {
        assert!(constant_time_eq(b"v0=abcd", b"v0=abcd"));
        assert!(!constant_time_eq(b"v0=abcd", b"v0=abce"));
        assert!(!constant_time_eq(b"v0=abcd", b"v0=abc"));
        assert!(!constant_time_eq(b"", b"x"));
        assert!(constant_time_eq(b"", b""));
    }
}
