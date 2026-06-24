# ADR 0007 — Docs + distribution: generated reference (anti-drift), the xtask build tool, and the CI-only release/wasm pipeline

- **Status**: Accepted (locked)
- **Date**: 2026-06-24
- **Deciders**: qfs-foundation-e0 trip team (Constructor authored; Architect/Planner review)
- **Ticket**: t40 — Docs + distribution (single-binary release). The authoritative documentation
  set + the release/distribution pipeline; the **final** ticket of the trip.
- **Supersedes / superseded by**: none
- **References**: RFD-0001 §1 (single binary: CLI / EC2 daemon / `wasm32` Workers), §3 (closed core
  + three open registries; the keyword set is *frozen*; purity invariant), §4 (codecs), §5 (DESCRIBE
  / capability gating), §8 (server bindings), §9 (lean binary; owned DTOs — no vendor leak; `wasm32`
  target), §10 (least privilege; no token in any artifact). ADR-0001 / ADR-0002 / ADR-0003 /
  ADR-0004 / ADR-0005 / ADR-0006 — the same offline-cache / disk / wasm-buildability decision shape.

## Context

t40 must deliver (a) the authoritative reference docs (README + language/drivers/server) and (b) the
single-binary release pipeline (4 native musl/macOS tarballs + 1 wasm artifact, `install.sh`,
`qfs --version`, SemVer). Two forces shaped the decision:

1. **Docs must not drift from the code.** The language reference is the contract the AI reads; a
   hand-authored driver catalog or keyword table that lags the binary is a correctness bug, not a
   cosmetic one.
2. **The constrained trip host cannot run the release/wasm/musl builds.** A `cargo build --release`
   wedges the near-full disk; the **full-workspace** `wasm32-unknown-unknown` build fails (only the
   pure cores are wasm-clean — t36/ADR-0005, not the whole binary); and musl static cross-link needs
   a cross toolchain that exists only in CI (t01/A2). These are exactly the constraints under which
   ADR-0005 parked the musl/CF artifacts.

## Decision

### 1. The reference docs are GENERATED from the binary's own registries (anti-drift)

`docs/language.md`, `docs/drivers.md`, and `docs/server.md` are rendered by `qfs::docs` from the
binary's live data, never hand-authored:

- **language.md** ← `qfs_lang::RESERVED_KEYWORDS` (an *alias of* the one frozen `KEYWORDS` slice —
  no second transcription) + `qfs_lang::grammar_ebnf()`.
- **drivers.md** ← `qfs::catalog::driver_catalog()`, built by walking the **existing** t39 describe
  surface (`DescribeReport`: archetype / capabilities / procedures / aliases / pushdown) per mount.
- **server.md** ← the frozen server-DDL keyword set + the t36 deployment mapping prose.

A committed-docs-vs-generated golden test (`qfs::docs::tests::committed_docs_match_generated_output`,
in an **existing** test binary) and `cargo run -p xtask -- gen-docs --check` (CI) fail on any drift.
A frozen-keyword test (`qfs_lang::reference::tests`) locks `RESERVED_KEYWORDS` to the 38-entry §3
set; a purity doctest (`GmailDriver::send_alias_plan`) proves `SEND(d)` desugars to a `CALL mail.send`
`Plan` with no I/O.

### 2. The driver catalog reuses describe — NO new required `Driver::doc()` trait method

The ticket sketched adding a required `Driver::doc() -> DriverDoc` method to the `Driver` trait. We
**rejected** that: a new *required* trait method forces every driver crate (and all downstream) to
recompile, which the 147M-class disk cannot survive. Instead `qfs::catalog` folds each driver's
**existing** introspective half (the t39 `DescribeReport`) into an owned `DriverDoc` DTO — no trait
change, no forced recompile, no vendor-type leak (RFD §9). (Had a per-driver descriptor been truly
needed, it would have been added as a *default* trait method so no impl is forced to change; reusing
describe avoided even that.)

### 3. `xtask` is a separate, dep-light, non-shipped build crate

The cargo-xtask pattern: a root `xtask` crate (`publish = false`, NOT in the binary) with one
external dependency — the `qfs` path crate (whose new `[lib]` facet exposes the doc generator). It
adds zero uncached crates. `qfs` gained a thin `[lib]` so both `main.rs` and `xtask` reuse the same
composition root (the describe registry → the catalog → the docs). The convenience alias
`cargo xtask` lives in the gitignored `.cargo/config.toml`; the canonical committed invocation is
`cargo run -p xtask -- <cmd>` (what `release.yml` uses).

### 4. The release / wasm / musl pipeline is CI-only (mirrors ADR-0005)

`cargo run -p xtask -- dist` and `cargo build --release` / `--target wasm32-unknown-unknown` are
**not run locally**. `xtask dist` refuses to execute unless `QFS_DIST_ALLOW=1` (never set locally),
printing the matrix it *would* run so the shape stays reviewable. The pipeline code (cross-compile
matrix, strip, `sha256sum`, tarball, the wasm linker gate that fails loudly on a non-wasm symbol) is
real and reviewable; `.github/workflows/release.yml` executes it in CI on a `v*` tag. `install.sh`
detects OS/arch, downloads the matching tarball, **verifies the sha256 before extracting**, and
installs a runnable `qfs`. No live credential ever appears in a doc, an example, a golden, or a
release artifact (a grep gate enforces this; examples use placeholder handles only — RFD §10).

**Local verification surface (what the trip actually ran):** native *debug* build, `cargo run -p
xtask -- gen-docs` (idempotent), the docs-drift + frozen-keyword + purity tests, `clippy
--all-targets -D warnings`, `cargo fmt --all --check`, and the full `cargo test --workspace`. The
release/wasm/musl half is asserted by code + CI shape, not by a local run.

## Consequences

- **Positive.** Docs cannot drift (generated + golden-locked). No driver recompile was forced. The
  release pipeline is honest about being CI-only, exactly as ADR-0005 parked its artifacts. The
  binary stays lean (xtask never ships); `qfs --version` is the field-debug anchor.
- **Negative / parked.** The 4 native tarballs + the wasm artifact are **not produced or verified on
  the trip host** — they are asserted by reviewable code + `release.yml` and will first truly build
  in CI. The full-binary wasm artifact remains parked behind t36's wasm-clean-facet work (only the
  pure cores compile to `wasm32` today); `xtask dist`'s wasm step builds the wasm-clean facet and
  fails loudly rather than shipping a broken artifact.
- **Follow-ups.** When CI runs `release.yml` on the first `v*` tag, confirm the four tarballs +
  checksums attach and `install.sh` round-trips on a clean Linux + macOS host; finish the
  full-binary wasm facet alongside t36's Workers host.
