# ADR 0003 — git object access: `gix` (gitoxide) vs. an in-house loose-object reader

- **Status**: Accepted (locked)
- **Date**: 2026-06-23
- **Deciders**: cfs-foundation-e0 trip team (Constructor authored; Architect/Planner review)
- **Ticket**: t26 — git object-model driver (`cfs-driver-git`, all four archetypes over the
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

**`cfs-driver-git` reads the local git object database with an in-house, dependency-free
loose-object reader (a small pure-Rust DEFLATE inflater + SHA-1 content addressing + the
`<type> <len>\0<payload>` object framing), behind an internal `ObjectDb` seam.** The
`gix` (gitoxide) crate is **not** taken as a production dependency. The `ObjectDb` trait is
the reversibility seam: a `GixObjectDb` could be added later behind a non-default cargo
feature without touching any caller, exactly as ADR-0002 kept the combine-engine choice
reversible behind the `CombineEngine` trait and ADR-0001 kept the parser choice reversible
behind an owned `ParseError`.

The reader covers exactly what the **committed fixture repository** drives: loose objects
(commit / tree / blob / annotated tag), refs (`refs/heads/*`, `refs/tags/*`, `HEAD`),
packed-refs, and the reflog. Pack-file reading is implemented only as far as the fixture
needs (the fixture is authored to keep its referenced objects loose); a full delta/pack
resolver is a named park behind the same `ObjectDb` seam.

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
   `std` + owned `cfs_types` values keeps that property; gix's closure (parallel/`crc32fast`/
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
| Correctness guard | Battle-tested | Pinned to **real git output**: the fixture repo is built by the **system `git`** at test time, so our reader is differentially checked against canonical git bytes/oids |

Honest counter-point (as ADR-0002 recorded for the evaluator): a hand-rolled reader must be
*correct*. We mitigate exactly as ADR-0002 did — with a **differential property**: the
fixture repo is materialised by invoking the host's real `git` (a dev-time test fixture,
not a production dependency), so every oid our SHA-1 computes and every object our inflater
decodes is checked against what canonical git produced. A future need for pack-delta chains
or remote transport reopens `GixObjectDb` behind the `ObjectDb` feature seam without a
rewrite.

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
from the carry-over `cfs-crypto-core` objstore/slack HMAC surface: t26 does not consume
`cfs-crypto-core`, and the git oid hash does not entangle with it.
