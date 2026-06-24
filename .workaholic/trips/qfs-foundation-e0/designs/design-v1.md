# Design v1

- **Author**: Constructor
- **Status**: draft
- **Reviewed-by**: (pending)
- **Trip**: qfs-foundation-e0
- **Scope**: RFD-0001 epic **E0** — ticket **t01** (Rust workspace + single-binary scaffold) and gated ticket **t02** (parser-library decision spike).
- **References**: `.workaholic/RFDs/0001-qfs-architecture.md` (§2, §3, §5, §8, §9, §10); tickets `20260622214650-t01-…`, `20260622214650-t02-…`.

---

## 0. Night-mode assumptions (recorded)

This trip launched via `/trip night` with an empty instruction; the lead recorded the interpretation as **build E0 = t01 then t02, scope fixed**. Where the tickets leave a choice, I record the assumption here rather than pausing:

- **A1 — Toolchain pin.** `rust-toolchain.toml` pins `1.96.0` (the only installed toolchain) with components `rustfmt`, `clippy` and the two installed targets. We do **not** attempt to install a different channel (system-safety: `system_changes_authorized=false`).
- **A2 — wasm32 target absence.** `wasm32-unknown-unknown` is **not installed** and `rustup target add` is a user-toolchain mutation. Per t01 it is explicitly out of scope, so t01 needs nothing here. t02's acceptance criteria require a `wasm32` build of `qfs-parser`; this is a **blocker risk** (R5) handled as CI-only / parked — see §6 and §7.
- **A3 — Crate naming.** Workspace member directories use the bare names (`cmd`, `core`, `lang`, `plan`, `driver`, `codec`, `server`, `parser`); each crate's `package.name` is the `qfs-`-prefixed form (`qfs-cmd`, …) the tickets use. The single binary's package is `qfs` with `[[bin]] name = "qfs"`.
- **A4 — Async/runtime.** No async runtime (tokio) is introduced at E0. Dispatch is synchronous and returns `Result<_, CfsError>`. This keeps core crates wasm-friendly (no threads/`std::fs`) per t01 considerations; a runtime is an E2/E7 concern.
- **A5 — Edition.** Rust 2021 edition (stable, broadest tooling support under 1.96.0). 2024 edition is avoided to reduce lint churn at the foundation layer.
- **A6 — Parser default.** Per RFD §9 and t02, **winnow is the working default**; chumsky wins only if the committed golden error-corpus shows decisive structured-error-recovery superiority. The spike — not opinion — decides, and the ADR records it.

System-safety detection result: `{"is_provisioning": false, "system_changes_authorized": false}`. **Consequence:** every quality gate runs project-local. No `npm -g`, no `pip`, no `sudo`, no edits to `~/.cargo`, `~/.rustup`, `~/.gitconfig`, or shell profiles. The mandated per-shell `. "$HOME/.cargo/env"` is a read-only source of an existing env script (not a profile edit) and is permitted.

---

## 1. Scope & inventory

### 1.1 What is being replaced
The existing Go program (`main.go` + `internal/{audit,auth,gmail,shell}`) is the `gmail-ftp` FTP-style Gmail shell. Per RFD preamble it is **subsumed as a future driver, not merged or refactored**. E0 touches **none** of it; the Rust workspace is added alongside, and the Go tree stays compiling/untouched. No Go file is edited or deleted in this trip.

### 1.2 Workspace crate list (t01) — exact members and E0 content

| Dir | package | Kind | E0 content (what it holds now) |
|---|---|---|---|
| `crates/qfs` | `qfs` | bin (`src/main.rs`) | Thin entrypoint: `std::process::exit(qfs_cmd::run(std::env::args_os()))`. No domain logic. The sole `[[bin]] name = "qfs"`. |
| `crates/cmd` | `qfs-cmd` | lib | clap-derive CLI: `Cli`/`Command` enums for `run`/`-e`, `serve <config>`, interactive `shell` (default). Dispatches into `qfs-core`; every arm returns `CfsError::NotImplemented{..}`. Sets up `tracing` subscriber at this boundary. Logic-free w.r.t. the engine. |
| `crates/core` | `qfs-core` | lib | `Engine` + `Session` context threading the three registries + reserved `capabilities`/`audit_sink`/`irreversible` seams (unused at E0). Defines `MountRegistry`, `ProcRegistry`, `CodecRegistry` with `register`/`resolve`. Owns `CfsError`. Re-exports trait seams from `qfs-driver`/`qfs-codec`. |
| `crates/lang` | `qfs-lang` | lib | Reserved-keyword **const golden set** (RFD §3, frozen) + a typed `Keyword` enum scaffold. No grammar. |
| `crates/plan` | `qfs-plan` | lib | Placeholder `enum Effect`, `struct Plan` (typed DAG: nodes + deps), `irreversible: bool` flag. Constructs only; no interpreter. |
| `crates/driver` | `qfs-driver` | lib | `Driver` trait + `Archetype`, `Capabilities`, `ProcedureDecl`, `AliasFn`, `NodeSchema` enums/structs; owned-DTO marker convention. Purity invariant in doc comments. |
| `crates/codec` | `qfs-codec` | lib | `Codec` trait (`bytes ↔ rows`) + `RowBatch`, `Row`, `Value` minimal owned types. |
| `crates/server` | `qfs-server` | lib | `serve(config) -> Result<(), CfsError>` skeleton; `/server/...` mount stub module. No bindings impl. |
| `crates/parser` | `qfs-parser` | lib | **(t02)** Added as a workspace member by t01-as-prerequisite; populated by t02: spike code under `crates/parser/spikes/`, owned `ParseError`, `parse_statement` signature, re-exported keyword consts. |

Shared types live where they are owned and re-exported upward: `CfsError` in `qfs-core`; `Path`/`RowBatch`/`Value` co-located with their primary consumer (`Path` in `qfs-core` since both driver and codec need it; `RowBatch` in `qfs-codec`). Dependency direction is strictly acyclic: `qfs` → `qfs-cmd` → `qfs-core` → {`qfs-driver`, `qfs-codec`, `qfs-lang`, `qfs-plan`}; `qfs-server` → `qfs-core`; `qfs-parser` → `qfs-lang` (for keyword consts). No crate depends on `qfs-cmd`.

### 1.3 t02 inventory (gated on t01)
- `crates/parser/spikes/common.rs` — shared `SpikeStmt`/`PipeOp`/`Expr`/`CmpOp` AST.
- `crates/parser/spikes/winnow_spike.rs`, `crates/parser/spikes/chumsky_spike.rs` — same grammar, two libs, each `fn parse(&str) -> Result<SpikeStmt, SpikeError>`.
- `crates/parser/src/error.rs` — owned `ParseError` (byte/char span, expected-set, machine code).
- `crates/parser/src/lib.rs` — `pub fn parse_statement(src: &str) -> Result<Stmt, ParseError>` (stub) + AST re-export + keyword consts (re-exported from `qfs-lang`).
- `docs/adr/0001-parser-library.md` — locked decision record.
- Golden corpus + snapshots under `crates/parser/tests/`.

---

## 2. Implementation approach

### 2.1 Cargo workspace layout
Root `Cargo.toml`:
```toml
[workspace]
resolver = "2"
members = ["crates/*"]

[workspace.package]
edition = "2021"
rust-version = "1.96"

[workspace.lints.rust]
unsafe_code = "forbid"

[workspace.lints.clippy]
all = { level = "deny", priority = -1 }
unwrap_used = "deny"
expect_used = "deny"
panic = "deny"
```
Library crates inherit `[lints] workspace = true`. The `unwrap_used`/`expect_used`/`panic = deny` policy on **libs** enforces the t01 "no-`unwrap`/`expect`-in-libs" rule structurally. Tests are exempt (clippy lint levels apply to non-test code; test modules may `unwrap` for assertions, or we allow at the test module).

`rust-toolchain.toml`:
```toml
[toolchain]
channel = "1.96.0"
components = ["rustfmt", "clippy"]
targets = ["aarch64-unknown-linux-gnu", "x86_64-unknown-linux-gnu"]
```

### 2.2 CLI dispatch (`qfs-cmd`, clap derive)
```rust
#[derive(Parser)]
#[command(name = "qfs", version)]
struct Cli { #[command(subcommand)] cmd: Option<Command> }

#[derive(Subcommand)]
enum Command {
    /// Run one statement and exit
    Run { #[arg(short = 'e')] stmt: String, #[arg(long)] json: bool },
    /// Start the server from a .qfs config
    Serve { config: PathBuf },
    // absence of subcommand => interactive shell stub
}

pub fn run<I, T>(args: I) -> i32 where I: IntoIterator<Item = T>, T: Into<OsString> + Clone {
    init_tracing();
    let cli = Cli::parse_from(args);
    let outcome = match cli.cmd {
        Some(Command::Run { stmt, json }) => dispatch_run(&stmt, json),
        Some(Command::Serve { config }) => qfs_server::serve(&config),
        None => dispatch_shell(),
    };
    match outcome { Ok(()) => 0, Err(e) => { e.report(json_flag); 1 } }
}
```
Each `dispatch_*` returns `Err(CfsError::NotImplemented { feature: "…" })` at E0. The binary maps `Ok`→0, `Err`→1 (structured, not a panic). `qfs --help` lists `run`, `serve`, and documents the no-arg interactive shell — satisfied by clap derive doc comments + an `after_help` note.

### 2.3 `CfsError`
```rust
#[derive(Debug, thiserror::Error)]
pub enum CfsError {
    #[error("not yet implemented: {feature}")]
    NotImplemented { feature: &'static str },
    #[error("unknown mount: {0}")]
    UnknownMount(String),
    #[error("parse error")]              // populated in E1
    Parse,
    // … reserved arms grow per epic
}
```
Machine-readable: a `fn code(&self) -> &'static str` and a `fn report(&self, json: bool)` that emits either a human line or a `{"error": {...}}` JSON envelope (mirrors the Go contract for continuity, AI-facing per §5). `thiserror` is the one core dependency justified (derive macro, zero runtime cost, wasm-safe).

### 2.4 Registries (`qfs-core`)
Each registry is a typed container, empty at E0:
```rust
pub struct MountRegistry { mounts: BTreeMap<String, Arc<dyn Driver>> }
impl MountRegistry {
    pub fn new() -> Self { Self { mounts: BTreeMap::new() } }
    pub fn register(&mut self, d: Arc<dyn Driver>) -> Result<(), CfsError> { … }   // keyed by d.mount()
    pub fn resolve(&self, mount: &str) -> Result<Arc<dyn Driver>, CfsError> { … }   // UnknownMount if absent
}
```
`ProcRegistry` (functions + `CALL` procs, keyed by qualified name e.g. `mail.send`) and `CodecRegistry` (keyed by `fmt`) follow the identical `new`/`register`/`resolve` shape. `register` rejects duplicate keys with a structured error. **No async**, `BTreeMap` for deterministic iteration (test stability). These are the RFD §3 "three open registries"; the closed core (keywords) is the only thing that is *not* a registry.

### 2.5 Trait shapes
`Driver` and `Codec` exactly per ticket signatures (consumer-side, small, `Send + Sync`). Critical purity encoding: `describe`/`capabilities`/`procedures`/`prelude` return **data only** (`NodeSchema`, `Capabilities`, `&[…]`); there is no method that takes `&mut self` or returns a future or performs I/O. The lone impure seam (`COMMIT : Plan -> World`) is reserved for E2 and deliberately **absent** from the trait. Owned-DTO convention: doc comment + a sealed marker trait `OwnedDto` (or a `#[non_exhaustive]` discipline) documented in `ARCHITECTURE.md`; no vendor SDK type is reachable from `qfs-driver`'s public API (asserted by §3 compile-test).

### 2.6 `qfs-lang` reserved-keyword golden set (frozen, RFD §3)
Transcribed **verbatim** from RFD §3 as a `const &[&str]` (or a `Keyword` enum + `ALL` slice) — the single home of the closed core:
- Query/transform: `FROM WHERE SELECT EXTEND SET AGGREGATE GROUP BY ORDER BY LIMIT DISTINCT JOIN UNION EXCEPT INTERSECT AS EXPAND`
- Effects: `INSERT INTO UPSERT INTO UPDATE REMOVE VALUES RETURNING CALL`
- Codecs: `DECODE ENCODE`
- Plan: `PREVIEW COMMIT`
- Server DDL: `CREATE ENDPOINT TRIGGER JOB VIEW MATERIALIZED VIEW WEBHOOK POLICY DO EVERY ON`
- Operators (separate const, lexer-facing): `|>` and `= <> < > <= >= AND OR NOT LIKE ~ ANY IN BETWEEN`

A golden test (§3 below) locks the set so later tickets cannot smuggle a new keyword. Multi-word forms (`GROUP BY`, `INSERT INTO`, `MATERIALIZED VIEW`) are stored as their canonical multi-word strings to match §3 exactly; lexing nuance is E1's problem, not the golden lock's.

### 2.7 `qfs-plan` placeholders
```rust
pub enum Effect { /* INSERT, UPSERT, UPDATE, REMOVE, CALL … reserved */ }
pub struct Plan { nodes: Vec<Effect>, deps: Vec<(usize, usize)>, irreversible: bool }
```
Constructors only; no apply. `irreversible` flag reserved per §6/§10.

### 2.8 `qfs-server`
`pub fn serve(config: &Path) -> Result<(), CfsError>` → `Err(NotImplemented{ feature: "serve" })`. A `mount` submodule documents the `/server/...` server-is-a-driver stub (§8). No HTTP, no bindings.

---

## 3. t02 spike approach

### 3.1 Comparison criteria (from t02 + RFD §5/§9), weighted
1. **Error machineability (load-bearing, highest weight)** — span (byte+char) + expected-set + machine code on the structured-error path. This is the *only* reason to choose chumsky over the winnow default.
2. **Multi-error recovery** — does the lib surface multiple errors per input (chumsky's strength)?
3. **Ergonomics of `|>`-chained pipelines & UPPERCASE keywords** — how naturally the grammar expresses qfs constructs.
4. **Span/position reporting** fidelity.
5. **wasm32 build-size delta** — tiebreaker (RFD §1/§9 Workers footprint).
6. **Maintenance/risk** — winnow active (commits this week); chumsky on Codeberg, GitHub archived; combine deprioritized (excluded from the head-to-head per §9).
7. **Parse throughput** on a representative pipeline.

### 3.2 Method
- Shared corpus of ~10 inputs: valid pipelines + deliberately broken (missing `|>`, lowercase keyword, dangling `WHERE`, unterminated string, unknown op).
- Both spikes parse the **valid** inputs into the *identical* shared AST — asserted by a cross-check test (winnow output == chumsky output).
- **Golden snapshots** of rendered errors for each broken input from both libs, committed under `crates/parser/tests/` (prefer committed `.golden`/`insta` snapshots; `insta` is a dev-dep — acceptable as it is test-only and pure-Rust).
- Benchmark throughput (criterion *optional*; a simple timed loop is sufficient and avoids a heavy dev-dep) and record wasm32 size delta per lib.

### 3.3 ADR format (`docs/adr/0001-parser-library.md`)
Standard ADR: **Status / Context (RFD §9 research + the load-bearing §5 requirement) / Decision (the locked lib) / Consequences / Comparison table (criteria × {winnow, chumsky} with the golden evidence + bench/size numbers) / Reversibility (exact swap cost behind the owned `ParseError` wrapper).** The ADR is the durable artifact; spike binaries may rot, the decision must not.

### 3.4 Parser-skeleton crate (post-decision)
Promote the winning lib from dev-dep to dep; populate `src/error.rs` (owned `ParseError`) and `src/lib.rs` (`parse_statement` stub + keyword consts re-exported from `qfs-lang`). **No vendor type in the public API** — asserted by a `pub use` audit test (and the owned-DTO rule of §9 keeps the choice reversible). The losing spike stays under `spikes/` clearly marked non-production (or is deleted per ticket step 9 — design choice: **keep both** under `spikes/` as comparison evidence, marked non-production, since the ADR references them).

---

## 4. Quality strategy

| Gate | Command (all prefixed by `. "$HOME/.cargo/env"`) | Enforces |
|---|---|---|
| Format | `cargo fmt --all --check` | rustfmt; `rustfmt.toml` with `edition = "2021"`, default style |
| Lint | `cargo clippy --workspace --all-targets -- -D warnings` | deny-warnings + the no-`unwrap`/`expect`/`panic` lib policy |
| Build | `cargo build --workspace` | compiles |
| Test | `cargo test --workspace` | unit + golden |
| Cross (CI) | `cargo build --workspace --target {aarch64,x86_64}-unknown-linux-gnu` | both Linux targets |
| wasm (t02, CI/parked) | `cargo build -p qfs-parser --target wasm32-unknown-unknown` | t02 only — see R5 |

### Tests (t01)
- **Dispatch**: `run('anything')` and `serve('x.qfs')` return `CfsError::NotImplemented` (not panic) — asserted.
- **Registry**: each registry returns empty on fresh `new`; `register → resolve` round-trips; duplicate-key `register` is a structured error; `resolve` of absent key → `UnknownMount`/equivalent.
- **Keyword golden**: `qfs-lang::KEYWORDS` equals the RFD §3 golden list (exact, order-independent set compare + count).
- **Purity compile-test**: an in-test dummy `Driver`/`Codec` impl that performs **no I/O** instantiates and is used — proving the seam holds at the type level. A `trybuild` ui-test is *optional* (heavy dev-dep); the simpler in-crate `#[test]` with a no-I/O dummy impl satisfies the acceptance criterion without `trybuild`. **Decision: in-test dummy impl, no `trybuild`** (lighter, equally proves the invariant).
- **No-vendor-leak** (carried into t02): `pub use` audit.

### CI workflow (`.github/workflows/ci.yml`)
GitHub Actions, ubuntu runner. Jobs: `fmt` → `clippy` → `build`/`test` → `cross` (matrix over the two linux-gnu targets, using `rustup target add` *on the runner* — runner mutation is allowed; this host is not). The wasm job for t02 lives here too (runner installs `wasm32-unknown-unknown`). Toolchain via `rust-toolchain.toml` (actions read it). No secrets, no credentials in CI (acceptance criterion).

---

## 5. Delivery plan (ordered)

**t01 (no dependency):**
1. Root `Cargo.toml` workspace + `rust-toolchain.toml` + `rustfmt.toml` + workspace lints.
2. Create the 8 lib/bin crates with empty-but-compiling module files; wire the acyclic dep graph (§1.2).
3. `CfsError` (`qfs-core`) + trait/enum seams (`qfs-driver`, `qfs-codec`, `qfs-plan`) with purity doc-comments.
4. Three registries + their unit tests (empty/lookup/round-trip/duplicate).
5. `qfs-cmd` clap dispatch (`run`/`-e`, shell stub, `serve`) → structured `NotImplemented`; `qfs/src/main.rs` thin entrypoint; `tracing` init.
6. `qfs-lang` frozen-keyword const + golden test.
7. Purity compile-test (in-test dummy impls).
8. `rustfmt.toml`, clippy workspace lints wired; local `fmt`/`clippy`/`build`/`test` green.
9. CI workflow (fmt/clippy/build/test + cross-compile matrix).
10. `ARCHITECTURE.md` (crate-boundary rules, wasm-friendliness note, link back to RFD-0001).

**t02 (gated on t01 — starts only after t01 acceptance criteria are green):**
11. Add `qfs-parser` member (if not already created as the placeholder in step 2; t01 may stub it empty, t02 fills it) with `winnow` + `chumsky` as **dev-dependencies**.
12. Shared spike AST (`spikes/common.rs`).
13. winnow_spike + chumsky_spike implementations.
14. Corpus + golden snapshots + cross-AST-equality test.
15. Bench + wasm32 size delta capture.
16. Score → write `docs/adr/0001-parser-library.md` with the locked choice.
17. Promote winner dev-dep→dep; `src/error.rs` owned `ParseError` + `parse_statement` stub + keyword consts; `pub use` audit test.
18. All gates green (incl. the wasm32 `qfs-parser` build — see R5 for the local-vs-CI handling).

---

## 6. Risk assessment

| ID | Risk | Likelihood | Impact | Mitigation |
|---|---|---|---|---|
| R1 | **x86_64 cross-link fails on this aarch64 host** (no `gcc` cross-linker / `x86_64-…-gcc`). | High locally | Med | Treat x86_64 cross-build as **CI-only**. CI runner has the cross-linker (or we use `cross`). If local link fails, record it as expected and do not block; the aarch64 native build is the local proof. Document in `ARCHITECTURE.md` + ADR. **Do not** install a cross-linker locally (system-safety). |
| R2 | **clippy `unwrap/expect/panic = deny` too aggressive**, blocks legitimate test code. | Med | Low | Apply the strict policy to **non-test** lib code only; allow in `#[cfg(test)]` modules (`#![cfg_attr(test, allow(clippy::unwrap_used))]` or per-module). Binary `main.rs` stays thin and panic-free. |
| R3 | **wasm-friendliness violated** by a transitive dep pulling `std::fs`/threads into core crates. | Low | Med | Core crates (`qfs-core`/`lang`/`plan`/`driver`/`codec`/`plan`) take **zero** I/O deps at E0; only `thiserror` (and t02's parser libs). Keep I/O behind future driver impls. Note rule in `ARCHITECTURE.md`. Do **not** add the wasm32 target in t01 (per ticket). |
| R4 | **clap version / derive API drift** under 1.96.0. | Low | Low | Pin clap `4.x` with the `derive` feature in `Cargo.toml`; lockfile committed. |
| R5 | **t02 wasm32 build blocked**: `wasm32-unknown-unknown` is **not installed**, and `rustup target add` is a user-toolchain mutation forbidden by system-safety. t02's acceptance criteria require `cargo build -p qfs-parser --target wasm32-unknown-unknown`. | High | Med | Run the wasm32 build **in CI only** (runner installs the target freely). Locally, record the wasm32 **build-size** number from the CI artifact for the ADR, or capture it during the planning convergence by asking the lead whether the target may be added — but **night-mode does not ask**: default to **CI-only wasm32 validation** and **park** the local wasm32 size-delta as a CI-sourced number. If even CI cannot run before merge, record a **Night Park** for the wasm32 acceptance line and ship the rest. The owned-`ParseError` wrapper keeps the choice sound regardless. |
| R6 | **chumsky on Codeberg / GitHub archived** — dependency-availability or crates.io lag. | Low | Low | chumsky is published on crates.io (the archive note is about the repo host, not the registry). If unfetchable offline, the spike degrades to winnow-only + a documented note in the ADR; do not block t01. |
| R7 | **insta/criterion heavy dev-deps** bloat build. | Low | Low | Prefer committed `.golden` files + a simple timed loop over `insta`/`criterion`; if `insta` is used, it is test-only and pure-Rust (acceptable). Decision recorded in §3.2/§4. |
| R8 | **Offline crate fetch** (clap/thiserror/winnow/chumsky not cached). | Med | Med | If the cargo registry cache lacks a crate and the network is unavailable in night mode, record a **Night Park** with the exact missing crate; the workspace skeleton (std-only crates) still builds and the keyword/registry/dispatch tests still pass, so park at "t01 minus external-dep features" rather than wedging. |

---

## 7. Engineering-policy alignment & parked items

- **Implementation policy**: crate split *is* the architectural contract; `cmd` logic-free; vendor types kept out of `qfs-driver`/`qfs-core` (owned DTOs); no-`unwrap`/`expect`/`panic` in libs; structured `tracing` at the `cmd` boundary; no secrets/credentials anywhere in build/test/CI.
- **Operation policy**: reserve `audit_sink` hook + `irreversible` flag now (seams for E2); CI as the cross-compile + wasm validation surface; reproducible (committed lockfile, pinned toolchain).
- **System-safety**: all gates project-local; no global/profile/system mutation; `rustup target add` only on CI runners, never on this host.
- **Parked (night-mode, to flag at convergence/coding)**: (1) local x86_64 cross-link (R1) — CI-only; (2) local wasm32 for t02 (R5) — CI-only, size-delta sourced from CI; (3) offline crate fetch (R8) — park to std-only skeleton if a dep is unfetchable. None of these block t01's core acceptance (workspace builds, dispatch/registry/keyword tests green on native aarch64).

---

## Review Notes

(pending one-turn review by Planner and Architect)
