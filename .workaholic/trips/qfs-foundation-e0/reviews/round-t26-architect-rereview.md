# Architect Re-Review — t26 Required Fix (inflate differential)

- **Reviewer**: Architect (Neutral / structural bridge)
- **Scope**: ONLY the required fix in commit `a870831` (analytical review; no test/build execution)
- **Prior review**: `round-t26-architect.md` on `698f1a5` — approve-with-observations, with one material
  should-fix the Lead promoted to required: ADR-0003 claimed a real-git differential guard for the
  in-house DEFLATE inflater, but the fixture seeded objects in-memory via the *uncompressed*
  `insert_object` path, so `zlib_inflate` (and especially the BTYPE=2 dynamic-Huffman decoder) was
  **dead in tests** — the riskiest hand-rolled parser was untested against canonical git.
- **Decision**: **Approve with observations** — the finding is **genuinely closed**.

---

## Finding under re-review

> ADR-0003's "differentially checked against canonical git" claim was not backed: the riskiest
> hand-rolled path (untrusted compressed bytes, dynamic-Huffman BTYPE=2) was never exercised by any
> test on real git bytes; the fixture built framed objects in memory and bypassed the inflater.

## Verification against the five required checks

### 1. Does the new test drive the REAL production inflate path on real git bytes? — YES

The test stores the **exact committed compressed bytes** via `insert_loose(oid, fx.compressed.to_vec())`
and reads through `ObjectDb::read`. I traced the production path in `objectdb.rs`:

- `LooseObjectDb::insert_loose` (objectdb.rs:226) stores the compressed bytes verbatim under the oid.
- `read` (objectdb.rs:254) → `framed` (objectdb.rs:236) → `if stored.first() == Some(&0x78)` 0x78 zlib
  probe (objectdb.rs:245) → `zlib_inflate(stored)` (objectdb.rs:246) → `parse_framed` (objectdb.rs:256).

This is exactly the chain the test/ADR/module doc describe. The test asserts **both**:
- decoded payload equals git's blob content (`raw.payload == fx.payload`), and
- the recomputed content-address oid equals git's (`frame_and_id(Blob, &raw.payload).0 == fx.oid`),

pinning the inflater AND the SHA-1. The test imports only `insert_loose` — there is **no
`insert_object` (uncompressed) shortcut** anywhere in `inflate_differential.rs` (grep-confirmed), so the
former bypass is gone. A third test also exercises the same object through the `Arc<dyn ObjectDb>`
trait-object path the `Repo` actually uses.

### 2. Is BTYPE=2 dynamic-Huffman actually exercised, and pinned? — YES

The `BIG` fixture (oid `faa97ff…`) declares `btype: 2`, and `assert_inflates_to_real_git` first asserts
`deflate_btype(fx.compressed) == fx.btype` **before** reading, so a fixture cannot silently regress to
fixed/stored and quietly drop the coverage. I independently decoded `big.blob.z`: first DEFLATE block is
**BTYPE=2** (2272 compressed → 8432 framed/payload bytes). The decoder genuinely implements that path —
`dynamic_trees` (inflate.rs:163) reads HLIT/HDIST/HCLEN, builds the code-length tree over `CL_ORDER`, and
handles the 16/17/18 repeat codes (inflate.rs:185-211) — so the BTYPE=2 fixture truly traverses the
previously-dead code, not a fallback. `dynamic_huffman_btype2_blob_inflates_to_real_git` is a dedicated
test for it.

### 3. Are fixtures genuine real-git output, and is the test hermetic? — YES

I independently verified each `*.blob.z` with a reference zlib (analytical decode, no crate build):
each decompresses to `blob <len>\0<payload>`, the SHA-1 over the framed bytes **equals the asserted oid**
(`45b983b…`, `d4c4ca6…`, `faa97ff…`), the payloads equal the committed `*.payload` files, and the BTYPEs
match (1/1/2). These are authentic `git hash-object -w` artifacts. Fixtures are committed binary blobs
(`include_bytes!`), so the test needs **no runtime git, no network, no creds** — hermetic and offline.
The old hand-pasted stored-block vector (`0x78,0x01,0x01,0x03,0x00,0xfc,0xff,…`) is **removed** and
superseded by the committed `stored.*` fixture (BTYPE=0, guarded by `stored_block_btype0_roundtrips…`).
The one remaining inline byte array (inflate.rs:358) is the documented reference "hello world"
fixed-Huffman unit vector — legitimately not a git-object fixture.

### 4. Is ADR-0003 now accurate? — YES

The ADR's correctness-guard wording now matches what the test does: it names the real production chain
(`insert_loose → read → 0x78 probe → zlib_inflate → parse_framed`), states "asserts both the decoded
payload **and** the content-addressed oid equal git's", and explicitly itemizes the guarded BTYPE
coverage (2/1/0) with the assertion-pins-coverage rationale. The earlier overstated phrasing
("the fixture's objects are produced by the host git, so what we inflate must equal the committed bytes")
is gone from both the ADR and the inflate module doc. The Comparison table's "Correctness guard" cell and
the honest counter-point now describe committed real-git compressed bytes including the BTYPE=2 path. The
**Accepted (locked)** status is now genuinely backed.

### 5. Are the parks honest? — YES

- `plan_rebase` (planner.rs) is now documented as a **named park — placeholder semantics**: it
  "delegates verbatim to `plan_merge`", produces a merge-shaped result (NOT linear per-commit replay),
  and is honest about the one invariant that holds (typed `MergeConflict`, zero effects in PREVIEW). An
  in-body `// PARK:` comment marks it. The earlier doc that implied rebase *was* the intended semantics
  is corrected. The lib.rs named-parks list also carries the `git.rebase` placeholder entry.
- Read-side vs apply-side reflog independence is documented in lib.rs:56-62: the read path holds an
  `Arc<dyn ObjectDb>` while the apply path owns a mutable store; a `/reflog` SELECT does not reflect a
  just-applied ref move until reconciliation; the authoritative post-COMMIT record is the applier's own
  reflog. Unifying both is a named park. This is an accurate structural statement.

---

## Observations (Critical Review Policy — concern + proposal)

1. **(Structural, non-blocking) Coverage breadth vs. the bypass that was the actual finding.** The fix
   correctly closes the dead-path gap for **blobs**. The differential currently pins the inflater on three
   blobs; the inflater also feeds commit/tree/tag reads, which share the *same* inflate+frame chain, so the
   decoder risk is covered — but the *parse_framed → parse_commit/parse_tree/parse_tag* leg past inflate is
   still only exercised by the in-memory `insert_object` fixtures, not by real-git compressed bytes.
   - **Proposal**: as a t27 follow-up (not a t26 blocker — out of the required-fix scope), add one
     committed real-git **tree** and **commit** loose fixture to the same harness so the post-inflate
     parsers are also pinned to canonical git bytes end-to-end. The harness already generalizes over
     `LooseFixture`; only `ObjectKind` needs to vary.

2. **(Minor, non-blocking) BTYPE=2 single-fixture sensitivity.** One BTYPE=2 fixture exercises the dynamic
   path, but a single sample may not hit every repeat-code branch (17 vs 18, distance-tree edge cases).
   - **Proposal**: optionally add a second dynamic-Huffman fixture with a distinct content profile (e.g.
     highly-repetitive vs. high-entropy) in t27 to widen branch coverage. Not required to close this
     finding — the current fixture already drives the dynamic lit/dist + code-length trees.

Neither observation blocks acceptance; both are forward-looking breadth suggestions, explicitly outside
the required fix.

## Verdict

The required fix in `a870831` **genuinely closes** the finding. The differential test now drives the real
production inflate path on authentic `git hash-object -w` bytes, asserting decoded payload **and**
recomputed oid against git's; the previously-dead BTYPE=2 dynamic-Huffman decoder is exercised and pinned
by a BTYPE assertion; fixtures are committed real-git binaries and the test is hermetic; the old
hand-pasted vectors are superseded; ADR-0003's guard wording and Accepted status are now accurate; and the
`plan_rebase` and read/apply reflog parks are documented honestly.

**Decision: Approve with observations.**
