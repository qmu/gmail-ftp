//! A self-contained, dependency-free **SHA-1** (FIPS 180-4) — git's object content-address
//! hash (ADR-0003). git names every object by `SHA-1(<type> <len>\0<payload>)`; this module
//! computes exactly that oid for the loose-object reader and for an object the driver is about
//! to write (so a `WriteLooseObject` effect carries the correct content-addressed oid).
//!
//! ## Why hand-rolled (a recorded engineering choice — ADR-0003)
//! The trip cargo cache carries no `sha1`/`gix`, the host disk is at 97%, and the workspace
//! default is wasm-clean. A pure-`std` SHA-1 is ~70 lines, has no native-link hazard on
//! `wasm32-unknown-unknown`, and is differentially checked against **real `git`** object oids
//! in the fixture tests — so its correctness is pinned by canonical git output. It lives in one
//! private module; no hash type crosses the crate boundary.
//!
//! SHA-1 is used here ONLY as git uses it: a **content address**, never to authenticate a
//! message or compare a secret. Its collision weakness is out of scope for that role (the same
//! note the objstore SHA-256 module records for its signing hash). This is separate from the
//! objstore/slack HMAC surface and the carry-over `qfs-crypto-core`.

#![allow(clippy::many_single_char_names)]

/// Compute the SHA-1 digest of `data` (20 bytes).
#[must_use]
pub fn sha1(data: &[u8]) -> [u8; 20] {
    let mut h: [u32; 5] = [
        0x6745_2301,
        0xEFCD_AB89,
        0x98BA_DCFE,
        0x1032_5476,
        0xC3D2_E1F0,
    ];

    // Pre-processing: append 0x80, then 0x00 until length ≡ 56 (mod 64), then the 64-bit
    // big-endian bit length.
    let bit_len = (data.len() as u64).wrapping_mul(8);
    let mut msg = data.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0x00);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());

    for chunk in msg.chunks_exact(64) {
        let mut w = [0u32; 80];
        for (i, word) in w.iter_mut().enumerate().take(16) {
            let b = i * 4;
            *word = u32::from_be_bytes([chunk[b], chunk[b + 1], chunk[b + 2], chunk[b + 3]]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }

        let (mut a, mut b, mut c, mut d, mut e) = (h[0], h[1], h[2], h[3], h[4]);
        for (i, &word) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | ((!b) & d), 0x5A82_7999u32),
                20..=39 => (b ^ c ^ d, 0x6ED9_EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1B_BCDC),
                _ => (b ^ c ^ d, 0xCA62_C1D6),
            };
            let tmp = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(word);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = tmp;
        }

        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
    }

    let mut out = [0u8; 20];
    for (i, word) in h.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }
    out
}

/// Lowercase-hex encoding of a 20-byte digest — the 40-char oid string git prints.
#[must_use]
pub fn hex(digest: &[u8; 20]) -> String {
    let mut s = String::with_capacity(40);
    for b in digest {
        s.push(char::from_digit((b >> 4) as u32, 16).unwrap_or('0'));
        s.push(char::from_digit((b & 0xf) as u32, 16).unwrap_or('0'));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha1_empty_and_abc_vectors() {
        // FIPS 180-4 / RFC 3174 published vectors.
        assert_eq!(hex(&sha1(b"")), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
        assert_eq!(
            hex(&sha1(b"abc")),
            "a9993e364706816aba3e25717850c26c9cd0d89d"
        );
        assert_eq!(
            hex(&sha1(
                b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"
            )),
            "84983e441c3bd26ebaae4aa1f95129e5e54670f1"
        );
    }

    #[test]
    fn sha1_git_empty_blob_oid() {
        // git's empty-blob oid: SHA-1("blob 0\0") — the canonical content-address framing.
        let mut framed = b"blob 0\0".to_vec();
        let _ = &mut framed;
        assert_eq!(
            hex(&sha1(&framed)),
            "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391"
        );
    }
}
