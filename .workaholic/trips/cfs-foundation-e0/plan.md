---
instruction: "[night /trip — empty instruction; recorded assumption] Develop and build the cfs foundation (epic E0 of the RFD-0001 Rust rebuild): ticket t01 (Rust workspace + single-binary scaffold, CLI+server, typed module/registry/trait seams, lints+CI+cross-compile), and gated on it, ticket t02 (parser-library decision spike: winnow vs chumsky, ADR + thin parser-skeleton crate). Scope fixed at invocation; do not expand."
phase: coding
step: concurrent-launch
iteration: 0
updated_at: 2026-06-22T23:30:00+09:00
---

# Trip Plan

## Initial Idea

[night /trip — empty instruction; recorded assumption] Develop and build the cfs foundation (epic E0 of the RFD-0001 Rust rebuild): ticket t01 (Rust workspace + single-binary scaffold, CLI+server, typed module/registry/trait seams, lints+CI+cross-compile), and gated on it, ticket t02 (parser-library decision spike: winnow vs chumsky, ADR + thin parser-skeleton crate). Scope fixed at invocation; do not expand.

## Plan Amendments

### A1 — Planning converged at round 1 (Lead, 2026-06-22)
One-turn review (Step 2) returned **unanimous approval**: every per-artifact decision was
"Approve with observations" or "Approve with minor suggestions"; **zero "Request revision"**.
Per the trip-protocol Consensus Gate, planning completes with no respond/escalate/moderate
rounds. Plan is fixed. The plan = `directions/direction-v1.md` + `models/model-v1.md` +
`designs/design-v1.md`, with the carry-over acceptance criteria in A3 below.

### A2 — Dev-environment ground truth (Lead, 2026-06-22, corrects design-v1 risks R8/toolchain)
The Lead verified the build environment directly (the Constructor's design-v1 reasoned from a
momentarily network-restricted subagent shell and over-rated several risks):
- **crates.io is reachable** from both the lead shell and a fresh subagent (`index HTTP 200`);
  a real `cargo fetch` succeeded. The global cargo cache has been **pre-warmed** (76 MB) with the
  full transitive closure: `clap 4.6.1 (derive)`, `thiserror 2.0.18`, `anyhow 1`, `serde 1 (derive)`,
  `serde_json 1`, `tracing 0.1`, `tracing-subscriber 0.3`, `winnow 1.0.3`, `chumsky 0.13.0`.
  → **R8 (offline fetch) is retired.** Use external deps freely; no std-only fallback needed.
  The t02 winnow-vs-chumsky head-to-head can run for real. Builds also work `--offline`.
- **Toolchain pin** must be `channel = "stable"` (NOT `"1.96.0"`) in `rust-toolchain.toml` — the
  Constructor's correct catch; pinning the numeric version triggers a failing offline rustup
  download. Installed: rustc/cargo/clippy/rustfmt 1.96.0; targets `aarch64-` and
  `x86_64-unknown-linux-gnu`.
- **x86_64 cross-link** has no cross-linker locally → the `cfs` binary links only for native
  aarch64 here; x86_64 binary link is **CI-only** (lib crates still cross-compile). Native
  aarch64 build/test is the local proof.
- **wasm32** target is not installed and is **deferred per the tickets** (t01/t02 out-of-scope);
  the t02 wasm32 size datapoint is CI-only / parked. Not a blocker for E0 acceptance.

### A3 — Carry-over acceptance criteria from round-1 reviews (Lead, 2026-06-22)
Folded into the Coding Phase as acceptance criteria (non-blocking review suggestions; no artifact
revision required):
- **G1 / C1** — the reserved-keyword golden set lives in ONE cited committed fixture in `cfs-lang`;
  the golden test asserts against that single source (no drift between two hand-transcriptions).
- **C4** — a `cargo metadata`-based test fails the build if `cfs-cmd` gains a direct dependency on
  `cfs-lang/plan/driver/codec/parser` (mechanically enforce "cmd is logic-free" + dep direction).
- **G3** — purity invariant proven by a plain in-crate `#[test]` instantiating a no-I/O dummy
  `Driver`/`Codec` (NOT via `trybuild`; the trait signature is the primary enforcer).
- **C5** — declare the intended `cfs-core → cfs-parser` edge direction now so E1 cannot introduce a
  cycle.
- **R5/P1 wording** — morning report states cross-compile status precisely: "native aarch64 built &
  tested locally; x86_64 + wasm32 are CI-only".
- Retained spike loser (winnow or chumsky) carries a banner pointing at the ADR so it is not misread
  as a live second parser.

## Progress (continued — coding phase)

- [x] **[Constructor]** coding/concurrent-launch — implemented t01: 9-crate workspace under `crates/` (cfs bin + cmd/core/lang/plan/driver/codec/server/parser), three open registries, Driver/Codec traits with type-level purity, frozen keyword fixture, Effect/Plan placeholders, clippy deny-warnings + rustfmt + CI. **29 unit/integration tests green**, fmt/clippy/build clean on native aarch64. Decision **D1** recorded (CfsError/Path in cfs-driver to keep spine acyclic). Commit `9557bda`.
- [x] **[Architect]** coding/review-and-testing — analytical review of t01: **Approve with minor suggestions**; acyclic spine verified across all 9 manifests, D1 endorsed, guards G1–G6 + C4/C5 pass, E1/E2/E7 confirmed to land inside seams. Commit `415e9f1`.
- [x] **[Planner]** coding/review-and-testing — E2E validation of t01 CLI: **E2E approved**; 7/7 scenarios pass, no panics, `--json` envelope `{"error":{"code","message"}}` parseable by python3 + jq. Commit (e2e review). Exit-code convention 0/1/2 consistent.
- [x] **[Lead]** t01 ACCEPTED — all three coding gates green, no iteration required.
- [x] **[Constructor]** coding/concurrent-launch — implemented t02: throwaway spike crate `spikes/parser-spike` (`publish = false`) parsing `FROM <path> |> WHERE <expr> |> SELECT <cols>` in **both winnow 1.0.3 and chumsky 0.13.0** into ONE shared AST; committed golden error corpus (`spikes/parser-spike/tests/golden/errors.txt`) + cross-parser AST-equality test (both green). **Decision LOCKED in `docs/adr/0001-parser-library.md`: winnow** — token-level expected-sets (vs chumsky's char-level) better suit the AI structured-error path (RFD §5); recovery not decisive (neither lib surfaces multi-errors out of the box); winnow has **zero transitive deps** (chumsky pulls `stacker`/`psm`, a C-built wasm-hostile stack manipulator), ~1.34 s vs ~4.70 s clean compile. Wired `cfs-parser` to winnow behind the owned `ParseError` (span+expected-set+machine code) + `parse_statement(&str) -> Result<Stmt, ParseError>`; winnow confined to the crate-private `grammar` module (no-vendor-leak audit test green). Keyword surface text sourced from the frozen `cfs_lang::Keyword` set. fmt/clippy(`-D warnings`)/build/test all green on native aarch64 (cfs-parser 7 tests, spike 3 tests, no t01 regression). Records below.

### A4 — t02 implementation decisions (Constructor, 2026-06-23)
- **Spike location**: a dedicated workspace member `spikes/parser-spike` (members glob extended to `["crates/*", "spikes/*"]`), `publish = false`, rather than `crates/parser/spikes/` or `examples/`. Rationale: a separate crate keeps winnow AND chumsky entirely out of `cfs-parser` (cfs-parser depends on winnow ONLY), and lets the shared AST be reused by the comparison test + example without leaking into the production crate. The lint relaxation (`#![allow(clippy::unwrap_used, ...)]`) is scoped to the spike crate only; `cfs-parser` keeps the strict workspace policy (panic-free `grammar` module).
- **wasm32 (R5/A2 carry)**: target not installed, deferred per ticket — NOT added. ADR records the wasm32 datapoint as **CI-only / deferred** with the qualitative note (winnow zero-dep/macro-free/wasm-clean; chumsky's `psm` C build is wasm-hostile). Not a blocker; corroborates winnow.
- **A3 retained-loser banner**: BOTH spikes retained under `spikes/parser-spike` as comparison evidence; the chumsky spike (the loser) carries a header banner pointing at `docs/adr/0001-parser-library.md`. The winnow spike likewise banner-marked NOT production.
- **Honest evidence note**: chumsky's multi-error recovery requires explicit `.recover_with(...)` wiring; out of the box `chumsky-recovery-count` is 1 for every corpus case. The ADR is explicit that recovery is therefore not free in either library and does not clear the bar to override the RFD §9 winnow default.

## Progress

- [x] **[Planner]** planning/artifact-generation — wrote `directions/direction-v1.md` (business vision for cfs E0 foundation: value proposition, business/strategic risk, three user personas including the AI agent operating cfs, system positioning for Rust single-binary + closed-core/open-registries, and the business rationale for a night on E0 before the 39 downstream tickets). Night-mode empty-instruction assumption recorded prominently in the artifact.
