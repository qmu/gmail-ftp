# ADR 0003 — git object access: `gix` (gitoxide) vs. an in-house loose-object reader

- **Status**: Accepted (locked)
- **Date**: 2026-06-23
- **Deciders**: qfs-foundation-e0 trip team (Constructor authored; Architect/Planner review)
- **Ticket**: t26 — git object-model driver (`qfs-driver-git`, all four archetypes over the
  **local** git object DB; not the GitHub HTTP API)
- **Supersedes / superseded by**: none
- **References**: RFD-0001 §1 (single, *lean* binary + `wasm32` Workers target), §9
  (Implementation: no heavy vendor SDKs / owned DTOs at the boundary), ADR-0001
  (winnow-vs-chumsky: dependency-weight / wasm-buildability criteria), ADR-0002
  (DuckDB rejected on footprint / wasm grounds — the *same decision shape*), and the
  hand-rolled-crypto precedent already shipped in this workspace
  (`crates/driver-objstore/src/sha256.rs`, `crates/driver-slack/src/hmac.rs` — own
  SHA-256/HMAC because the trip cargo cache carries neither `sha2` nor `ring`).

## Decision

**`qfs-driver-git` reads the local git object database with an in-house, dependency-free
loose-object reader (a small pure-Rust DEFLATE inflater + SHA-1 content addressing + the
`<type> <len>\0<payload>` object framing), behind an internal `ObjectDb` seam.** The
`gix` (gitoxide) crate is **not** taken as a production dependency. The `ObjectDb` trait is
the reversibility seam: a `GixObjectDb` could be added later behind a non-default cargo
feature without touching any caller, exactly as ADR-0002 kept the combine-engine choice
reversible behind the `CombineEngine` trait and ADR-0001 kept the parser choice reversible
behind an owned `ParseError`.

The reader covers exactly what the tests drive: loose objects (commit / tree / blob /
annotated tag), refs (`refs/heads/*`, `refs/tags/*`, `HEAD`), packed-refs, and the reflog.
Pack-file reading is implemented only as far as needed (a full delta/pack resolver is a named
park behind the same `ObjectDb` seam).

The inflater's correctness against canonical git is pinned by a **committed-real-git-bytes
differential** (`crates/driver-git/tests/inflate_differential.rs` +
`crates/driver-git/tests/fixtures/loose/`): three blobs were materialised **once** by the host
`git 2.50.1` (`git hash-object -w`), and the **exact compressed `.git/objects` bytes** were
checked into the crate. The test drives the real production read path
(`LooseObjectDb::insert_loose` → `ObjectDb::read` → the `0x78` zlib probe → the in-house
`zlib_inflate` → `parse_framed`) on those bytes and asserts both the decoded payload **and** the
content-addressed oid equal git's. This needs **no runtime `git`, no network, no creds** — it is
hermetic and offline by construction. **DEFLATE block-type coverage** is explicit and guarded
(each fixture asserts its first-block BTYPE so coverage cannot silently drift):
- **BTYPE=2 dynamic-Huffman** — a large, varied blob (git oid `faa97ff…`, 2272 compressed →
  8442 framed bytes). This is the most complex decoder path (code-length tree, the 16/17/18
  repeat codes, the dynamic literal/distance trees) and exactly what real git loose objects use
  for non-trivial content.
- **BTYPE=1 fixed-Huffman** — two small/repetitive blobs (`45b983b…`, `d4c4ca6…`).
- **BTYPE=0 stored** — pinned by an inline inflate unit test against a committed level-0 zlib
  stream (`tests/fixtures/loose/stored.*`), since git/zlib rarely emits a stored block for normal
  content.

## Context

The ticket says "thin over `gix`". Before committing to it I measured the deployment-relevant
facts (not faith), the same way ADR-0001/0002 did:

1. **Offline availability.** The trip cargo cache (`~/.cargo/registry/cache`, 216 crates)
   carries **no** `gix`, and **none** of its transitive zlib/SHA-1 stack (`flate2`,
   `miniz_oxide`, `sha1`, `crc32fast`, `adler`, `libz-sys`). `cargo add gix --dry-run`
   reaches crates.io and resolves `gix v0.85` — but with its *default* feature set it pulls
   a very large transitive closure (the `gix-*` family: `gix-object`, `gix-pack`,
   `gix-odb`, `gix-ref`, `gix-revision`, `gix-diff`, `gix-blame`, `gix-worktree`,
   `gix-status`, `gix-index`, … plus `flate2`/`miniz_oxide`/`sha1`/`crc32fast`), tens of
   crates that would all have to be fetched and compiled fresh.

2. **Disk envelope.** The build host is at **97% full (3.7 GiB free)**; the trip
   deliberately keeps `target/` lean (`debug=0`, `incremental=false`). Fetching +
   compiling gix's closure is exactly the kind of footprint blow-up the trip is
   constrained against — and the precise risk class ADR-0002 rejected DuckDB over.

3. **`wasm32` cleanliness (RFD §1/§9).** The driver itself carries no wasm requirement at
   t26, but the workspace default is "wasm-clean by construction". An in-house reader over
   `std` + owned `qfs_types` values keeps that property; gix's closure (parallel/`crc32fast`/
   pack-cache machinery) is heavier to keep wasm-clean and is unnecessary for a local,
   fixture-driven object read.

4. **What the driver actually needs.** Like ADR-0002 (the heavy SQL work is *pushed down*,
   so the local engine only ever runs a small residual), the git driver only needs to
   **read** committed objects and **build** new ones as pure plan effects. That is a small,
   closed surface: inflate a loose object, parse four object kinds, walk parents/trees,
   compute a content-addressed oid for an object we are about to write. A general-purpose
   git toolkit (worktree mutation, status, blame engine, pack delta chains, mailmap, …) is
   far more than the fixture-driven acceptance set requires.

This is the same decision shape as ADR-0001 (winnow vs chumsky) and ADR-0002 (own evaluator
vs DuckDB): a capable, heavy dependency vs. the RFD §9 "lean, wasm-clean, owned-boundary"
default — resolved on measured footprint/offline/wasm facts against a deliberately small
required surface. The workspace has already made this exact call twice for crypto
(own SHA-256/HMAC, ADR-cited above) because the cache lacked `sha2`/`ring`; git's SHA-1 +
DEFLATE is the same situation.

## Comparison (evidence, not opinion)

| Criterion | `gix` (default features) | In-house `ObjectDb` reader |
| --- | --- | --- |
| Offline in trip cache | **No** — gix + its zlib/SHA-1 stack absent; full fresh fetch | **Yes** — pure `std`, zero new crates |
| Added transitive crates | Tens (`gix-*` + `flate2`/`miniz_oxide`/`sha1`/`crc32fast`/…) | **0** |
| Disk cost on a 97%-full host | Large fetch + compile — the ADR-0002 footprint hazard | Negligible (a few source modules) |
| `wasm32` cleanliness (RFD §1/§9) | Heavier closure to keep wasm-clean | Wasm-clean by construction (`std` + owned values) |
| Capability vs. need | Full git toolkit ≫ fixture-driven read+plan | Exactly the loose-object read + oid-compute surface |
| Correctness guard | Battle-tested | Pinned to **real git output**: committed compressed `.git/objects` byte fixtures (generated once by the host `git`) the in-house inflater decodes back to git's exact bytes/oids — including the BTYPE=2 dynamic-Huffman path — with no runtime `git`/network/creds |

Honest counter-point (as ADR-0002 recorded for the evaluator): a hand-rolled reader must be
*correct*. We mitigate exactly as ADR-0002 did — with a **differential property**, here against
**canonical git output**: real loose-object bytes were produced **once** by the host `git`
(`git hash-object -w`) and the exact compressed `.git/objects` bytes were **committed** as test
fixtures (`crates/driver-git/tests/fixtures/loose/`), so the test inflates them deterministically
with **no runtime `git` dependency, no network, no creds**. Every object our inflater decodes and
every oid our SHA-1 recomputes is checked against what canonical git produced — and the coverage
explicitly includes the **BTYPE=2 dynamic-Huffman** decoder (the riskiest path, and the one real
git loose objects actually use), with the fixtures' DEFLATE block types asserted so coverage
cannot silently drift. A future need for pack-delta chains or remote transport reopens
`GixObjectDb` behind the `ObjectDb` feature seam without a rewrite.

## Consequences

- **Positive**: the default build stays a single lean binary with **zero** new dependency
  crates, the 97%-full disk is not threatened, `wasm32` reachability is preserved, and no
  git SHA-1/DEFLATE type ever crosses the crate boundary (RFD §9 owned DTOs). The
  `ObjectDb` trait keeps the door open for an optional `gix`-backed impl (native-only,
  behind a non-default feature) if pack-delta or large-repo performance ever justifies it.
- **Negative / accepted**: we own the correctness of a loose-object inflater + SHA-1 +
  object parser. Scope is bounded to the four object kinds, refs/packed-refs, and the
  reflog; full pack-delta resolution, partial clone, submodules, LFS, and remote transport
  are explicitly out of scope (named parks). The differential-against-real-`git` fixture is
  the guard.
- **Reversibility**: because no git vendor type crosses the `ObjectDb` / DTO boundary
  (owned DTOs only, RFD §9), swapping in a `gix` backend is a feature-gated addition, not a
  refactor.

## Notes on the SHA-1 used for object addressing

git addresses objects by **SHA-1** over `<type> <len>\0<payload>`. This SHA-1 is used ONLY
as a content address (the same role git uses it for) — never to authenticate a message or
compare a secret — so its known collision weakness is not in scope here, exactly as the
objstore SHA-256 note records for its (non-constant-time) signing hash. It is **separate**
from the carry-over `qfs-crypto-core` objstore/slack HMAC surface: t26 does not consume
`qfs-crypto-core`, and the git oid hash does not entangle with it.
