//! **Differential inflate test against canonical git output** (ADR-0003 correctness guard).
//!
//! The in-house zlib/DEFLATE inflater is the crate's highest-risk hand-rolled component: it
//! parses untrusted compressed bytes. This test drives the **real production read path**
//! (`LooseObjectDb::insert_loose` → `ObjectDb::read` → the `0x78` zlib probe → `zlib_inflate` →
//! `parse_framed`) on the **exact compressed bytes a real `git` produced** for three blobs,
//! committed under `tests/fixtures/loose/` (generated ONCE by `git 2.50.1`; the test needs no
//! runtime `git`, no network, no creds — fully hermetic). It asserts both the decoded **payload**
//! and the **content-addressed oid** match what git recorded.
//!
//! Crucially it covers the **dynamic-Huffman (BTYPE=2)** block — the most complex decoder path and
//! exactly what real git loose objects use for non-trivial content — alongside fixed-Huffman
//! (BTYPE=1). The stored-block (BTYPE=0) decoder is pinned by an inline unit test in the inflate
//! module (`stored_block_btype0_roundtrips_committed_fixture`).
//!
//! Before this test, the in-memory fixture seeded objects via the *uncompressed* `insert_object`
//! path, so `zlib_inflate` was never exercised on a fixture read. This closes that gap with REAL
//! git bytes, making ADR-0003's "differentially checked against canonical git" guarantee true.

// Integration tests are a separate crate without the lib's `#![cfg_attr(test, allow(...))]`, so
// opt the test-only assertions/panics in explicitly (the workspace forbids them in lib code).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;

use qfs_driver_git::{frame_and_id, LooseObjectDb, ObjectDb, ObjectKind, Oid};

/// One committed real-git loose-object fixture: name, expected oid, expected DEFLATE block type,
/// the compressed bytes, and the decoded blob payload.
struct LooseFixture {
    name: &'static str,
    oid: &'static str,
    btype: u8,
    compressed: &'static [u8],
    payload: &'static [u8],
}

const SMALL: LooseFixture = LooseFixture {
    name: "small",
    oid: "45b983be36b73c0788dc9cbcb76cbb80fc7bb057",
    btype: 1,
    compressed: include_bytes!("fixtures/loose/small.blob.z"),
    payload: include_bytes!("fixtures/loose/small.blob.payload"),
};

const FIXED: LooseFixture = LooseFixture {
    name: "fixed",
    oid: "d4c4ca6d43306c0d7540b3aacc7e57485ff3cb10",
    btype: 1,
    compressed: include_bytes!("fixtures/loose/fixed.blob.z"),
    payload: include_bytes!("fixtures/loose/fixed.blob.payload"),
};

/// The dynamic-Huffman (BTYPE=2) case — large, varied content that forces git/zlib to emit a
/// dynamic-Huffman block. This is the critical coverage: the BTYPE=2 decoder (code-length tree,
/// the 16/17/18 repeat codes, dynamic lit/dist trees) is the most complex path and was previously
/// untested against real git output.
const BIG: LooseFixture = LooseFixture {
    name: "big",
    oid: "faa97ff147f1c13ecac7ac2c054c2d9a3f881420",
    btype: 2,
    compressed: include_bytes!("fixtures/loose/big.blob.z"),
    payload: include_bytes!("fixtures/loose/big.blob.payload"),
};

/// Assert the first DEFLATE block's BTYPE matches the manifest (so a fixture cannot silently
/// regress to a different block type and quietly drop the dynamic-Huffman coverage).
fn deflate_btype(zlib_stream: &[u8]) -> u8 {
    // zlib: 2-byte header, then the DEFLATE body. First byte: bit0=BFINAL, bits1-2=BTYPE
    // (LSB-first). The body starts at offset 2 (no preset dictionary in git loose objects).
    (zlib_stream[2] >> 1) & 0x3
}

fn assert_inflates_to_real_git(fx: &LooseFixture) {
    // Guard: the committed fixture is the expected DEFLATE block type (pins BTYPE=2 coverage).
    assert_eq!(
        deflate_btype(fx.compressed),
        fx.btype,
        "{}: committed fixture is not BTYPE={} (the differential coverage would drift)",
        fx.name,
        fx.btype
    );

    // Drive the REAL production read path: store the exact git-compressed bytes under the oid,
    // then `read` — which probes the 0x78 zlib header, runs the in-house inflater, and parses the
    // framed object.
    let oid = Oid::parse(fx.oid).unwrap();
    let mut db = LooseObjectDb::new();
    db.insert_loose(oid.clone(), fx.compressed.to_vec());

    let raw = db.read(&oid).unwrap_or_else(|e| {
        panic!(
            "{}: in-house inflate of real git bytes failed: {e}",
            fx.name
        )
    });
    assert_eq!(raw.kind, ObjectKind::Blob, "{}: kind", fx.name);
    assert_eq!(
        raw.payload, fx.payload,
        "{}: inflated payload differs from git's blob content",
        fx.name
    );

    // Differential oid: framing the inflated payload reproduces git's exact content-addressed oid
    // (SHA-1 over `<type> <len>\0<payload>`). This pins BOTH the inflater AND the SHA-1.
    let (recomputed, _framed) = frame_and_id(ObjectKind::Blob, &raw.payload);
    assert_eq!(
        recomputed.as_str(),
        fx.oid,
        "{}: recomputed content-address oid differs from git's",
        fx.name
    );
}

#[test]
fn fixed_huffman_btype1_blobs_inflate_to_real_git() {
    assert_inflates_to_real_git(&SMALL);
    assert_inflates_to_real_git(&FIXED);
}

#[test]
fn dynamic_huffman_btype2_blob_inflates_to_real_git() {
    // The headline coverage: a real-git dynamic-Huffman loose object round-trips through the
    // in-house inflater to git's exact bytes + oid.
    assert_inflates_to_real_git(&BIG);
}

#[test]
fn reading_through_shared_objectdb_uses_the_inflater() {
    // The driver reads objects through an `Arc<dyn ObjectDb>`; confirm the dynamic-Huffman object
    // inflates correctly through the trait-object path the `Repo` actually uses.
    let oid = Oid::parse(BIG.oid).unwrap();
    let mut db = LooseObjectDb::new();
    db.insert_loose(oid.clone(), BIG.compressed.to_vec());
    let shared: Arc<dyn ObjectDb> = Arc::new(db);
    let raw = shared.read(&oid).unwrap();
    assert_eq!(raw.payload, BIG.payload);
}
