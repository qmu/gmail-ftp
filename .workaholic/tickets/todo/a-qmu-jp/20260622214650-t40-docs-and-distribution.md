---
created_at: 2026-06-22T21:46:50+09:00
author: a@qmu.jp
type: housekeeping
layer: [Infrastructure]
effort:
commit_hash:
category:
depends_on: [20260622214650-t36-deployment-targets.md]
---

# Docs + distribution (single-binary release)

## Overview

This ticket delivers the **authoritative documentation set** and the **single-binary
distribution/release pipeline** for `cfs`. It implements the distribution promise in RFD
§1 ("a single static binary that runs as a CLI locally, as a daemon on EC2, or compiled to
`wasm32` for Cloudflare Workers") and §9 (Rust single binary + `wasm32-unknown-unknown`),
and it makes the closed-core grammar and three open registries of §3 *legible* to both the
AI agent and human operators.

Docs are not an afterthought here: per RFD §1 the entire product exists so an AI learns
*one* small grammar and one operating procedure (`DESCRIBE → write → PREVIEW → COMMIT`).
The **language reference** is therefore a load-bearing artifact — it is the contract the
agent reads. The README is the spec; the grammar/reserved-word list is governance (§3's
"keyword set is frozen"); the driver catalog and server guide document the three open
registries and the binding forms of §8.

## Scope

In scope:
- `README.md` as the authoritative top-level spec (vision, core model, install, quickstart).
- `docs/language.md`: pipe-SQL grammar (EBNF), the **frozen reserved-word table**, the
  open-registry governance rules, purity invariant.
- `docs/drivers.md`: driver catalog (archetypes, capabilities, procedures, codecs) generated
  from the live driver registry.
- `docs/server.md`: server guide (`CREATE ENDPOINT|TRIGGER|JOB|VIEW|WEBHOOK|POLICY`, bindings,
  deployment mapping).
- Release pipeline producing static Linux (`x86_64`/`aarch64` musl) + macOS (`x86_64`/`aarch64`)
  binaries and a `wasm32-unknown-unknown` artifact; `install.sh`; SemVer + `cfs --version`.

Out of scope (deferred):
- Actual deployment-target wiring (Worker/EC2 packaging, native CF bindings) — ticket **t36**
  (this ticket's dependency); we *document* the mapping, t36 *builds* it.
- AI operating-procedure prose / agent prompt assets — sibling E8 "AI procedure" ticket.
- Per-driver auth setup pages beyond catalog stubs — owned by the E5 auth tickets.

## Key components

New crate `xtask` (cargo-xtask pattern; not shipped in the binary):
- `xtask::dist` — cross-compile matrix, strip, `sha256`, tarball, assemble `dist/`.
- `xtask::gen_docs` — render reference docs from in-binary metadata so docs cannot drift.

Doc-generation surface added to existing core crates (introspection, not new behavior):
- `cfs-lang`: `pub const RESERVED_KEYWORDS: &[&str]` (the §3 frozen set) and
  `pub fn grammar_ebnf() -> &'static str`; a `#[test]` golden-compares these to `docs/`.
- `cfs-driver`: extend the `Driver` trait with already-required descriptors so the catalog
  is generated, never hand-written:
  ```rust
  pub struct DriverDoc {
      pub mount: &'static str,        // /driver/...  (paths registry)
      pub archetypes: Vec<Archetype>, // Blob | Relational | Append | ObjectGraph
      pub capabilities: CapabilitySet,// universal verbs per node
      pub procedures: Vec<ProcDoc>,   // CALL driver.action(...)
      pub prelude_fns: Vec<FnDoc>,    // pure aliases (SEND, MERGE)
      pub codecs: Vec<&'static str>,  // DECODE/ENCODE fmts contributed
  }
  pub trait Driver { fn doc(&self) -> DriverDoc; /* ...existing... */ }
  ```
  `DriverDoc` is an **owned DTO** — no vendor SDK types leak (RFD §9). `gen_docs` walks the
  function/codec registries and the path registry (the three open namespaces of §3).
- Version surface: `cfs::version::VERSION` from `env!("CARGO_PKG_VERSION")`, plus build
  metadata (git sha, target triple, wasm-capable flag) via a `build.rs`.

Release/CI:
- `.github/workflows/release.yml` — tag-triggered, runs `cargo xtask dist`, attaches artifacts.
- `install.sh` — detects OS/arch, downloads the matching tarball, verifies `sha256`.

## Implementation steps

1. Add `xtask` workspace member; wire `cargo xtask <dist|gen-docs>`.
2. Expose `RESERVED_KEYWORDS` + `grammar_ebnf()` in `cfs-lang`; transcribe the §3 frozen
   keyword set verbatim (query/effect/codec/plan/server-DDL/operators).
3. Add `Driver::doc()` / `DriverDoc` and implement for every existing driver.
4. Build `xtask::gen_docs`: render `docs/language.md`, `docs/drivers.md`, `docs/server.md`
   from the registries; write README quickstart section from a checked-in template.
5. Add golden tests: generated docs must equal committed `docs/*` (fail CI on drift).
6. `build.rs` emitting version/build metadata; implement `cfs --version` long form.
7. `xtask::dist`: cross-compile the 4 native targets (musl static) + `wasm32`; strip,
   checksum, tarball into `dist/`.
8. `install.sh` + `release.yml`; document SemVer policy (grammar = stable surface).
9. Write `## Deploy`/release section in README pointing at t36 for target wiring.

## Considerations

- **Docs-as-derived (anti-drift):** the reference must be generated from the binary's own
  registries, never authored twice. Golden tests are the enforcement; a frozen-keyword test
  guards the §3 governance promise (adding a keyword fails CI by design).
- **Capability gating in the catalog:** the catalog must show *which universal verbs each
  node supports* (RFD §5) so the AI never plans a rejected op; render unsupported verbs
  explicitly, not by omission.
- **Purity invariant must be documented and testable:** language.md states every fn/alias is
  `… -> Plan`; back it with a doctest showing `SEND(d)` desugars to `CALL mail.send` and
  performs no I/O (RFD §3, §6).
- **Least-privilege / secrets:** README + server.md must show `CREATE POLICY` and credential
  handling; docs examples use placeholder creds only — **no live tokens in any example or
  golden file** (RFD §10). The release pipeline ships no secrets.
- **wasm hardest part:** not all native deps compile to `wasm32-unknown-unknown` (TLS,
  filesystem). Gate non-wasm code behind features; `xtask dist` must *fail loudly* if the
  wasm artifact pulls a non-wasm symbol, rather than silently producing a broken artifact.
  Deeper target wiring is t36's job — here we only prove the artifact builds.
- **Reproducibility/observability:** embed git sha + target in the binary; `cfs --version`
  is the field-debug anchor. Tarballs carry `sha256`; `install.sh` verifies before exec.
- **Directory standards:** `docs/` for reference, `xtask/` for build tooling, no doc logic
  in shipped crates beyond pure introspection (`doc()` is read-only, side-effect-free).

## Acceptance criteria

- `cargo build --release` and `cargo build --target wasm32-unknown-unknown` both succeed;
  `cargo clippy --all-targets -- -D warnings` is clean.
- `cargo xtask gen-docs` is idempotent; the golden test (`generated == committed docs/`)
  passes, and editing the keyword list without updating `docs/language.md` fails CI.
- Frozen-keyword test asserts `RESERVED_KEYWORDS` matches the RFD §3 set exactly.
- Purity doctest: `SEND(d)` produces a `Plan` (a `CALL mail.send` node) and executes no I/O.
- `cargo xtask dist` produces 4 native tarballs + 1 wasm artifact, each with a verified
  `sha256`; the wasm artifact contains no non-wasm symbol (linker check passes).
- `install.sh` on a clean Linux + macOS host fetches, verifies checksum, and yields a
  runnable `cfs --version` printing semver + git sha + target triple.
- No live credentials appear in README, docs, golden files, or release artifacts (grep gate).
