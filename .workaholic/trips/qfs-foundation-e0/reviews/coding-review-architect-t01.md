# Coding Review — Architect — t01 (commit 9557bda)

- **Author**: Architect
- **Reviewed**: t01 Rust workspace scaffold (`crates/*`, root `Cargo.toml`, `.github/workflows/ci.yml`, `ARCHITECTURE.md`)
- **Method**: Analytical only — read every `Cargo.toml`, every `src`, the CI workflow, and `ARCHITECTURE.md`; reasoned structurally against `models/model-v1.md` (guards G1–G6 + the acyclic spine), `designs/design-v1.md`, and `plan.md` A3 acceptance criteria (C1/C4/C5). No `cargo` was run (Constructor/Planner domain).
- **Decision**: **Approve with minor suggestions.**

---

## 1. Acyclic dependency spine — VERIFIED, no cycle

Read directly from the nine manifests (`dependencies` sections only):

| Crate | Direct workspace deps |
|---|---|
| `qfs` (bin) | `qfs-cmd` |
| `qfs-cmd` | `qfs-core`, `qfs-server` |
| `qfs-core` | `qfs-driver`, `qfs-codec`, `qfs-lang`, `qfs-plan` |
| `qfs-server` | `qfs-core` |
| `qfs-codec` | `qfs-driver` |
| `qfs-driver` | `qfs-plan` |
| `qfs-lang` | — (leaf) |
| `qfs-plan` | — (leaf) |
| `qfs-parser` | `qfs-lang` |

Topological order exists: `qfs-lang, qfs-plan` (level 0) → `qfs-driver` → `qfs-codec` → `qfs-core` → `qfs-server` (and `qfs-cmd`) → `qfs-cmd` → `qfs` (bin). `qfs-parser` sits at level 1 (depends only on `qfs-lang`). **No back-edge, no cycle.** Nothing depends on `qfs-cmd` except the `qfs` binary. The spine matches model-v1 §4 and design-v1 §1.2, and is *mechanically* locked by `crates/cmd/tests/dep_direction.rs` (four `cargo metadata` assertions: cmd has no domain-crate edge; nothing-but-`qfs` depends on cmd; the binary depends on cmd only among workspace crates; core does not yet depend on parser and parser never depends on core).

Note: the implemented spine carries two edges the model-v1 §4 *diagram* did not draw as arrows — `qfs-driver → qfs-plan` and `qfs-codec → qfs-driver`. Both are acyclic and both are explicitly anticipated in the model-v1 §4 *prose* ("`qfs-driver` returning a `qfs-plan::Plan` node") and the design. They are RFD-justified (trait signatures reference `Plan`; the shared error). This is fidelity, not drift.

## 2. Decision D1 (CfsError + Path in qfs-driver) — STRUCTURALLY SOUND

design-v1 §2.3 placed `CfsError`/`Path` in `qfs-core`. The Constructor moved them to `qfs-driver` because the `Driver`/`Codec` trait signatures return `Result<_, CfsError>` and take `&Path`, while the spine mandates `qfs-core → qfs-driver`; keeping the error in `qfs-core` would force the back-edge `qfs-driver → qfs-core` — exactly the cycle model-v1 §4 forbids and which I flagged in my own Review Notes as a revision trigger. The fix:

- canonical `CfsError`/`Path` live in `qfs-driver` (the lowest crate the signatures need);
- `qfs-codec` takes a one-line `qfs-driver` dep purely for the shared error (acyclic, RFD-justified — both trait crates return `Result<_, CfsError>`);
- `qfs-core` *re-exports* both (`pub use qfs_driver::{… CfsError, …, Path, …}`), so the rest of the workspace still names `qfs_core::CfsError` / `qfs_core::Path`.

**Judgment: faithful to the model's "one error enum, acyclic spine" intent.** There is exactly one error enum; the spine stays acyclic; consumers above `qfs-core` are unaffected by where the type physically lives. The decision is documented in three places that agree verbatim (`crates/driver/src/error.rs` header, `qfs-driver` package description, `ARCHITECTURE.md` §"Decision D1"). This is the *correct* resolution of the design-v1/model-v1 tension, and it is the kind of decision the model explicitly delegated ("verify it faithfully encodes the RFD, not redesign it" — A2; the Constructor preserved the contract while removing a cycle the design hadn't noticed).

**One forward-looking observation (a minor suggestion, not a blocker).** Placing the *canonical* `CfsError`/`Path` in `qfs-driver` makes `qfs-driver` carry a second responsibility — "the driver contract" *and* "the home of the two universal primitives." For E0–E2 this is harmless. The latent risk surfaces if a future crate needs `CfsError`/`Path` but should **not** depend on the whole driver contract — e.g. an E7 server-config or E2 audit type that wants the error without pulling `Driver`/`Archetype`/`Capabilities` into scope. Today every consumer either is above `qfs-core` (gets the re-export for free) or is `qfs-codec` (legitimately needs the error). So no crate is forced into an unwanted edge *yet*.

- **Proposal (defer, do not act now):** if/when a crate needs `CfsError`/`Path` without the driver contract, introduce a leaf `qfs-types` (or `qfs-error`) crate holding *only* `CfsError` + `Path`, and have `qfs-driver`/`qfs-codec`/`qfs-core` re-export from it. Doing this **now** would be churn against a hypothetical: it is a pure move-plus-reexport, fully mechanical, and `qfs-core`'s re-export surface (`qfs_core::CfsError`, `qfs_core::Path`) is the stable public name everything else already uses — so the future extraction is *invisible* to every downstream crate. The current placement does **not** foreclose the leaf-crate option; it defers it at zero cost. I therefore do **not** request the leaf crate now. Recommend a one-line `// TODO` note in `crates/driver/src/error.rs` recording that `CfsError`/`Path` may later be extracted to a `qfs-types` leaf if a non-driver consumer appears, so the option is visibly reserved rather than rediscovered.

## 3. Guards G1–G6 + acceptance criteria — all satisfied

- **G1 / C1 (keyword golden, single source) — PASS.** `crates/lang/src/keywords.rs` holds one fixture `KEYWORDS: &[&str]` plus a `Keyword` enum; the freeze test asserts the enum's `text()` surfaces equal the fixture (order-independent multiset) and locks `KEYWORDS.len() == 38` with a no-duplicates check. I independently transcribed RFD §3 (16 query + 7 effect + 2 codec + 2 plan + 11 server-DDL = **38**) and `OPERATORS.len() == 15` (`|>` + 14) — both match the RFD verbatim. There is exactly one hand-transcription (the enum is derived/checked against the fixture, not a second source), satisfying C1's "no drift between two transcriptions."
- **G2 (registries generic over trait objects) — PASS.** `registry.rs` stores `Arc<dyn Driver>` / `Arc<dyn Codec>` / owned `ProcedureDecl`; `register`/`resolve` never name a concrete driver/codec type. A new E4 driver implements the trait and calls `register` — zero `qfs-core` edits. Object-safety is separately proven (`driver_is_object_safe`, `codec_is_object_safe`).
- **G3 (purity via plain test, not trybuild) — PASS.** `qfs-driver` `dummy_driver_is_pure` and `qfs-codec` `dummy_codec_is_pure` instantiate no-I/O dummy impls. The trait signatures are the real enforcer: every `Driver`/`Codec` method takes `&self` (never `&mut self`), returns owned data / `Result<_, CfsError>` (never a future, executor, or `()`-with-effects). `COMMIT` is deliberately absent; the `_commit_seam_reserved_for_e2(&Plan)` marker keeps the `Plan` reference live and documents the reserved impure seam. This matches A3's "plain `#[test]`, not trybuild; the signature is the primary enforcer."
- **G4 (server-is-a-driver seam + TODO anchor) — PASS.** `crates/server/src/mount.rs` defines `SERVER_MOUNT = "/server"` and a `// TODO(E7): register /server as a Driver` block stating the server must be registered into `MountRegistry` like any other driver and that `CREATE …` desugars to `INSERT INTO /server/...`. The seam is reserved, not closed off; `server_mount_seam_is_reserved` locks the constant.
- **G5 / C4 (cmd logic-free) — PASS.** `qfs-cmd` depends on `qfs-core` + `qfs-server` only (manifest + `dep_direction.rs`). The dispatch arms (`dispatch_run`, `dispatch_shell`, `serve`) return `CfsError::NotImplemented`; the only "logic" present is argv parsing (clap), output-mode selection, error rendering, and tracing init — all command-boundary concerns, no engine/domain logic. Faithful to the "two faces of one engine" boundary B1.
- **G6 (parser behind owned ParseError) — PASS (skeleton).** `qfs-parser` exposes an owned `#[non_exhaustive] ParseError` and `parse_statement` with a library-free signature; no vendor type is reachable (no parser lib is even a dependency yet — t02 adds it). The reversibility contract is documented for t02. As a t01 skeleton this is exactly the reserved boundary B6 requires; the `pub use` audit lands with t02 when a real library is introduced.
- **C5 (reserved core→parser edge) — PASS.** The edge is declared as a comment in `crates/core/Cargo.toml` and `ARCHITECTURE.md`, *not* wired, and `dep_direction.rs::core_does_not_depend_on_parser_yet_but_reserves_the_edge` asserts both directions (core has no parser dep at E0; parser never depends on core). E1 can add it one-directionally without a cycle.

## 4. Translation fidelity — E1 / E2 / E7 land inside the seams

- **E1 (grammar/AST in `qfs-lang`, parser in `qfs-parser`):** `qfs-lang` is explicitly the AST home ("AST sum types land here in E1") and is a leaf; `qfs-parser → qfs-lang` is wired and `qfs-parser`'s `parse_statement`/`ParseError` shapes are stable. E1 adds the AST enum to `qfs-lang`, fills `parse_statement` in `qfs-parser`, and wires the reserved `qfs-core → qfs-parser` edge — **no restructure**.
- **E2 (effect-plan/interpreter in `qfs-plan`):** `qfs-plan` owns `Effect`/`Plan`/`irreversible` (construction-only). The interpreter (`COMMIT : Plan -> World`) is reserved and *named* in three crates (`qfs-plan` docs, the `qfs-driver` marker, `Engine::audit_sink`/`Session::irreversible` seams). E2 adds the interpreter as a runtime over `qfs-plan` — **inside the seam**.
- **E7 (server DDL):** `qfs-server` is positioned as a `Driver` over `/server`; the DDL keywords (`CREATE ENDPOINT/TRIGGER/JOB/VIEW/MATERIALIZED VIEW/WEBHOOK/POLICY`, `DO/EVERY/ON`) are already in the frozen `KEYWORDS` set; the `TODO(E7)` anchor states the implementation path. E7 implements `Driver` for `qfs-server` and registers it — **no new "server is special" boundary**.

The `Engine`/`Session` split (reserved `audit_sink`, `capabilities_enforced`, `irreversible` seams) and the `Capabilities`/`Archetype`/`NodeSchema`/`ProcedureDecl`/`AliasFn` DTOs (all `#[non_exhaustive]` with `new` constructors, blocking out-of-crate struct literals) mean E3/E4/E5 extend these types additively. The wasm-friendliness constraint (B7) is honored: no core crate names threads/`std::fs`/sockets, `unsafe_code = "forbid"` workspace-wide, and `tracing`/`std::path` live only at the `cmd`/`server` boundary (not the wasm-targeted core/lang/plan/driver/codec/parser set).

## 5. Concerns (Critical Review Policy — at least one, with a proposal)

1. **D1 single-responsibility creep in `qfs-driver` (minor, forward-looking).** Covered in §2 — proposal: a `// TODO` note reserving a future `qfs-types` leaf extraction; do not extract now.
2. **CI cross job uses native x86_64 on the runner, cross-link for aarch64 — verify the binary actually links on the *target* in CI.** `.github/workflows/ci.yml`'s `cross` matrix builds the whole workspace for x86_64 natively (fine) and aarch64 with `gcc-aarch64-linux-gnu` (fine for *building*, but the produced aarch64 binary is not run). This is correct for a *compile* gate and matches the A2/A3 "x86_64 is CI-only, native aarch64 is the local proof" decision — but the morning report should state, per R5/P1 wording, that **no cross-built binary is executed in CI** (compile-only cross-check). *Proposal:* leave the CI as-is (executing a cross-built binary needs an emulator, out of E0 scope); add one comment line in `ci.yml` clarifying "compile-only cross-check; no cross binary is executed." Non-blocking.
3. **`Session::irreversible` duplicates `Plan::is_irreversible` (minor).** `qfs-core::Session` carries an `irreversible: bool` that "mirrors `qfs_plan::Plan::is_irreversible`." Two sources of the same truth invite drift once E2 populates plans. *Proposal:* in E2, derive the session flag from the active `Plan` rather than storing it independently (or document it as a per-request override gate, distinct from the plan's own property). Recorded for E2; not a t01 defect.

## 6. Verdict

The scaffold is a faithful structural projection of RFD §2/§3/§5/§8/§9: one binary / two faces, closed core (one keyword home, frozen at 38), three open registries generic over trait objects, a narrow pure `Driver`/`Codec` contract, effects-as-data, and a server-is-a-driver seam — all on a strictly acyclic, mechanically-locked dependency spine. Decision D1 correctly resolves a cycle the design-v1 placement would have introduced, while preserving the "one error enum, shared workspace-wide" invariant. E1/E2/E7 land inside these seams without restructuring.

No structural defect would force a restructure; the three concerns are forward-looking/minor and each carries a deferred proposal. **Approve with minor suggestions.**
