---
created_at: 2026-06-22T21:46:50+09:00
author: a@qmu.jp
type: enhancement
layer: [Infrastructure, Config]
effort:
commit_hash:
category:
depends_on: []
---

# Rust workspace + single-binary scaffold (CLI + server)

## Overview

This ticket lays the foundation (epic **E0**) for the from-scratch Rust rebuild of `cfs`
described in RFD 0001. It delivers a Cargo workspace producing **one binary** that is both a
CLI and a server (`cfs serve`), with the **module/crate boundaries** that every later ticket
fills in. It implements §9 (Implementation: Rust, single binary, also `wasm32`) and stands up
the skeletal seams for §2 (three faces: VFS / pipe-SQL / effect-plan), §3 (closed core +
three open registries), §5 (driver contract), and §8 (server-is-a-driver). No grammar, no
drivers, no real I/O yet — only the typed scaffolding, the dispatch from CLI/server into a
shared core, build/lint/CI, and Linux/EC2 cross-compilation. The goal is that subsequent
tickets add code **inside** these boundaries without restructuring.

## Scope

In scope:
- Cargo **workspace** with the crate split below; one `[[bin]]` named `cfs`.
- A `cmd` layer that parses argv and dispatches: interactive shell stub, `cfs run '<stmt>'`/`-e`,
  `cfs serve <config.cfs>` (all returning "not yet implemented" structured errors for now).
- Empty-but-typed module seams for `core`, `lang`, `plan`, `driver`, `codec`, `server`.
- The three **open-registry** container types (paths/mounts, functions+procedures, codecs) as
  empty registries with a `register`/`resolve` surface.
- The `Driver` and `Codec` **traits** (consumer-side, small) and owned-DTO marker conventions.
- Workspace lints (`clippy` deny-warnings, `rustfmt`), `cargo build`/`test` green, a CI workflow.
- Cross-compile target `aarch64-unknown-linux-gnu` (EC2 Graviton; this host is `aarch64`) and
  `x86_64-unknown-linux-gnu`, validated in CI.

Out of scope (deferred):
- Grammar/parser (winnow spike) and AST → **E1 / sibling ticket t02**.
- Effect-plan DAG, PREVIEW/COMMIT interpreter → **E2**.
- Federation, pushdown, local combine engine (DuckDB vs own) → **E3**.
- Any concrete driver (mail/drive/s3/git/…) and codec impls → **E4 / E-codec tickets**.
- Credential store / capability *enforcement* logic → **E5** (we only define the trait shapes).
- `wasm32-unknown-unknown` build wiring → its own E0 sibling (we keep code wasm-friendly but do
  not add the target to CI here).

## Key components

Workspace crates (lib crates + one bin), respecting RFD §3/§5/§9:

- `cfs` (bin, `src/main.rs`) — thin; calls `cfs-cmd::run(std::env::args())`.
- `cfs-cmd` — argv parsing (clap derive), subcommand dispatch into `core`. Holds the
  shell/run/serve entrypoints; no domain logic.
- `cfs-core` — shared engine glue: the registry container, the `Engine`/`Session` context that
  threads registries + (future) capabilities + audit sink. Re-exports the trait seams.
- `cfs-lang` — placeholder for AST/grammar (frozen-keyword enums land here in E1). Ship the
  **reserved keyword** list as a `const` set now so the closed core is documented in one place.
- `cfs-plan` — placeholder for `enum Effect`, `struct Plan` (typed DAG), `irreversible` flag.
- `cfs-driver` — the `Driver` trait + capability/archetype enums:
  ```rust
  pub enum Archetype { BlobNamespace, RelationalTable, AppendLog, ObjectGraphWorkflow }
  pub trait Driver: Send + Sync {
      fn mount(&self) -> &str;                 // "/mail", "/s3", ...
      fn describe(&self, path: &Path) -> Result<NodeSchema, CfsError>;
      fn capabilities(&self, path: &Path) -> Capabilities;   // gate verbs at parse time (§5)
      fn procedures(&self) -> &[ProcedureDecl];              // CALL targets
      fn prelude(&self) -> &[AliasFn] { &[] }                // pure SEND-style aliases
  }
  ```
  Owned DTOs only: a `sealed`/marker convention so **no vendor SDK type crosses this boundary** (§9).
- `cfs-codec` — the `Codec` trait: pure `bytes ↔ rows`.
  ```rust
  pub trait Codec: Send + Sync {
      fn fmt(&self) -> &str;                                  // "json","yaml","md+frontmatter"
      fn decode(&self, bytes: &[u8]) -> Result<RowBatch, CfsError>;
      fn encode(&self, rows: &RowBatch) -> Result<Vec<u8>, CfsError>;
  }
  ```
- `cfs-server` — `serve(config)` skeleton: the server-is-a-driver `/server/...` mount stub; no
  bindings (ENDPOINT/TRIGGER/JOB/VIEW/WEBHOOK/POLICY) implemented yet, only the module.
- Registries (in `cfs-core`): `MountRegistry`, `ProcRegistry` (functions + `CALL` procs),
  `CodecRegistry` — each `register(...) / resolve(...)`, empty at this stage (RFD §3).
- `CfsError` — one structured error enum (machine-readable; AI-facing per §5), shared workspace-wide.

## Implementation steps

1. `cargo new --bin` workspace root; add `[workspace]` with members for each crate above; pin a
   recent stable toolchain in `rust-toolchain.toml`.
2. Create the lib crates with empty-but-compiling module files matching `cmd/core/lang/plan/
   driver/codec/server`; wire `cfs-cmd` → `cfs-core` → trait crates.
3. Define `CfsError` and the trait/enum seams (`Driver`, `Codec`, `Archetype`, `Capabilities`,
   `ProcedureDecl`, `AliasFn`, `RowBatch`/`Path`) as minimal types — enough to compile and to
   pin the **purity invariant** in doc comments (`fn … -> Plan`, only the interpreter is impure).
4. Add the three empty registries with `register/resolve` and unit tests asserting empty/lookup.
5. Implement `cfs-cmd` with clap: `run`/`-e`, interactive shell stub, `serve <config>`; each
   dispatch returns a structured `CfsError::NotImplemented` for now (asserted in tests).
6. Add the frozen-keyword `const` list in `cfs-lang` + a test that the set matches RFD §3.
7. Configure `rustfmt.toml`, `clippy` with `-D warnings` (workspace lints in `Cargo.toml`).
8. Add CI (GitHub Actions): `fmt --check`, `clippy -D warnings`, `build`, `test`, and a
   cross-compile job for `aarch64-unknown-linux-gnu` + `x86_64-unknown-linux-gnu` (cargo/cross).
9. Document the layout and crate-boundary rules in a short `ARCHITECTURE.md` pointing back to RFD 0001.

## Considerations

- **Directory structure / coding standards (実装)**: the crate split *is* the architectural
  contract — keep `cmd` logic-free, keep vendor types out of `cfs-driver`/`cfs-core` (owned
  DTOs, §9). Enforce with `clippy -D warnings` and a no-`unwrap`/`expect`-in-libs lint policy.
- **Closed core enforcement**: keywords live in exactly one place (`cfs-lang`), frozen and
  test-locked, so later tickets cannot smuggle a new keyword instead of registering a path/proc/codec.
- **Purity invariant (hard part)**: it must be *structurally* impossible for `Driver`/`Codec`
  impls to perform I/O at construction/describe time. Resolve by making those methods return
  data/`Plan` nodes only; the lone impure seam (`COMMIT`) is reserved for E2. Encode this in the
  trait signatures now so E4 drivers inherit it for free.
- **Least-privilege & secrets (設計/security §10)**: define `Capabilities`/`POLICY` *shapes* here
  (verb gating per node) even though enforcement lands in E5; never introduce a secret/credential
  field in this ticket — no creds touch the build.
- **Idempotency / recovery / observability**: not exercised yet, but reserve the `audit sink`
  hook and `irreversible` flag in `Engine`/`Plan` so E2 has the seam; structured logging via
  `tracing` set up at the `cmd` boundary.
- **wasm-friendliness**: avoid threads/`std::fs` in core crates so the later `wasm32` target
  stays cheap; keep I/O behind driver impls (not in this ticket) — note in `ARCHITECTURE.md`.
- **Cross-compile reality**: this host is `aarch64` Linux, so EC2 Graviton is the native target;
  validate the `x86_64` cross path in CI to avoid late surprises.

## Acceptance criteria

- `cargo build --workspace` and `cargo test --workspace` are **green**; `cargo clippy
  --workspace --all-targets -- -D warnings` and `cargo fmt --check` pass.
- The workspace produces exactly one `cfs` binary; `cfs --help` lists `run`, `serve`, and the
  interactive shell entry.
- `cfs run 'anything'` and `cfs serve x.cfs` return a **structured** `CfsError::NotImplemented`
  (asserted by tests), not a panic — proving the dispatch seam works.
- Unit test asserts the frozen-keyword set equals RFD §3 (golden list); registries return empty
  and round-trip a `register → resolve`.
- A compile-only test (or `trybuild`) demonstrates the `Driver`/`Codec` trait seams instantiate
  with a dummy in-test impl that performs **no I/O** (purity invariant holds at the type level).
- CI cross-compiles `aarch64-unknown-linux-gnu` and `x86_64-unknown-linux-gnu` successfully.
- **No live credentials** anywhere in the build, tests, or CI.
