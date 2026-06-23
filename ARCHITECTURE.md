# cfs architecture (Rust workspace)

This document records the **crate-boundary rules** of the `cfs` Rust rebuild. It is
the durable companion to [`RFD-0001`](.workaholic/RFDs/0001-cfs-architecture.md) (the
single source of truth) and to the E0 trip artifacts under
`.workaholic/trips/cfs-foundation-e0/`. Every later ticket must add code **inside**
these boundaries without restructuring the workspace.

> Note: the legacy Go `gmail-ftp` program (`main.go`, `internal/`, `go.mod`) lives
> alongside this workspace and is untouched. Per RFD §0 it is *subsumed as a future
> driver*, not merged. The Rust workspace coexists with it.

## Crate map

| Crate (`crates/<dir>`) | package | Role (RFD §) |
|---|---|---|
| `cfs` (bin) | `cfs` | The single binary; thin `main.rs` calling `cfs_cmd::run` (§9). |
| `cmd` | `cfs-cmd` | argv parsing (clap-derive), dispatch into the engine; **no domain logic** (§7). |
| `core` | `cfs-core` | Shared engine glue: 3 registries, `Engine`/`Session`, re-exports, `CfsError` (§3/§6). |
| `lang` | `cfs-lang` | The frozen reserved-keyword closed core (§3); AST lands here in E1. |
| `plan` | `cfs-plan` | Effects-as-data: the typed `Plan` DAG of `EffectNode`s, `PlanApplier`/`commit`, and `PREVIEW` rendering (§3/§6/§10). Depends on `cfs-types` (leaf) for the row model (t09). |
| `driver` | `cfs-driver` | The `Driver` contract (mount + `id()` plan identity, archetype, typed `Schema`, capabilities + parse-time gate, `ProcSig`/pushdown/prelude/`@version`, the `applier()` seam) + owned DTOs (incl. the out-of-crate `Capabilities` builder); owns `CfsError` & `Path` (§5/§9). |
| `codec` | `cfs-codec` | The pure `bytes ↔ rows` `Codec` contract (§4). |
| `types` | `cfs-types` | The canonical type & schema model: `Value`/`Row`/`RowBatch`, `Schema`/`ColumnType`, schema algebra + typed predicates (§4/§5). **Leaf** crate (t05). |
| `server` | `cfs-server` | The server face: `serve` stub + `/server` mount seam (§8). |
| `parser` | `cfs-parser` | Parser front door skeleton; **filled by t02** (§2.2/§9). |
| `pushdown` | `cfs-pushdown` | The pushdown planner (t14, Domain): `LogicalPlan`/`PhysicalPlan`/`ScanNode`/`PushedQuery`, `partition_by_source` (source-split + federation), AST→IR lowering (predicates sourced from the AST, O-t07-3), and `explain()`. Pure; no I/O (§6). |
| `engine` | `cfs-engine` | The local combine engine (t14, Infrastructure): the `CombineEngine` seam + in-house `MiniEvaluator` running the cross-source residual (filter/project/hash-join/set-ops/aggregate/sort/limit/EXPAND). DuckDB rejected, ADR-0002; dependency-light + wasm-clean (§6/§9). |
| `txn` | `cfs-txn` | The transactional correctness envelope (t11): `EffectKey` idempotency, `@version`/ETag preconditions, `CommitStrategy`, saga/ACID executors, audit ledger, `RecoveryReport`. **Pure orchestration** over `cfs-plan`/`cfs-types` — carries **no** tokio of its own (§6). |
| `runtime` | `cfs-runtime` | The async effect interpreter (t10): `Interpreter::commit`, `ApplyDriver`/`DriverRegistry`, `EffectError` taxonomy, `CapabilitySet` gate, the sync→async `PlanApplierBridge`/`SharedApplier`. **tokio is CONFINED here** — the sole impure stage; nothing in the pure spine depends back onto it (§3/§5/§6). |
| `driver-local` | `cfs-driver-local` | The **first concrete driver** (t16): a blob/namespace driver over the host filesystem mounted at `/local` (`ls cp mv rm` + `upsert`/`remove`), least-privilege sandbox, streaming reads, atomic temp+rename writes, copy→verify(size+hash)→[delete]. A **LEAF runtime consumer** — it bridges its synchronous `PlanApplier` to the async `ApplyDriver`; tokio dead-ends here (§5/§6/§10). |
| `secrets` | `cfs-secrets` | The credential / secret store + multi-account resolution (t27): consumer-side, owned-DTO `Secrets` surface reusing the canonical `cfs_types::DriverId`. Depends on `cfs-types` only; `cfs-core` threads a `Secrets` handle into the driver-bind context (§10). |

## Dependency spine (acyclic, no back-edges)

```
cfs (bin) → cfs-cmd → cfs-core → { cfs-lang, cfs-plan, cfs-driver, cfs-codec, cfs-types }
                         ▲
                   cfs-server ── depends on cfs-core
cfs-codec  → cfs-driver        (shared CfsError, decision D1 — acyclic)
cfs-codec  → cfs-types         (canonical Value/Row/RowBatch row model, t05 — acyclic)
cfs-driver → cfs-plan          (Driver::applier returns the PlanApplier seam; Plan nodes)
cfs-driver → cfs-types         (Driver::describe returns the canonical typed Schema, t13)
cfs-plan   → cfs-types         (effect nodes carry the canonical RowBatch/DriverId, t09 — acyclic)
cfs-txn    → { cfs-plan, cfs-types }  (t11 transactional envelope: PURE orchestration — EffectKey
                                idempotency, @version/ETag preconditions, CommitStrategy, saga/ACID
                                executors, audit ledger, RecoveryReport; NO tokio of its own)
cfs-runtime → { cfs-plan, cfs-types, cfs-txn }  (t10/t11 — tokio CONFINED here; the runtime bridges
                                its async ApplyDriver to cfs-txn's synchronous LegApplier seam)
cfs-parser → cfs-lang          (consumes the frozen keyword consts / AST)
cfs-pushdown → { cfs-driver, cfs-types, cfs-plan, cfs-parser }  (t14 pushdown planner: PURE.
                                PushdownProfile accessors from cfs-driver; the typed Predicate
                                + Schema::join from cfs-types; AST lowering from cfs-parser. No
                                I/O/async/vendor — acyclic, below cfs-core.)
cfs-engine → { cfs-pushdown, cfs-types }  (t14 local combine engine: the MiniEvaluator over the
                                PhysicalPlan. Dependency closure = serde family only; wasm-clean.)
cfs-core   → cfs-pushdown       (t14 integration seam: cfs_core::plan wires query AST → LogicalPlan
                                → PhysicalPlan via the live MountRegistry; surfaces ScanNodes for T10.)
cfs-secrets → cfs-types        (t27 credential/secret store + multi-account resolution: consumer-side,
                                owned-DTO, reuses the canonical DriverId. LEAF over cfs-types — acyclic.)
cfs-core   → cfs-secrets       (t27 bind-context credential surface: the Engine threads a Secrets
                                handle into the driver-bind context; cfs-core re-exports it.)
cfs-driver-local → { cfs-driver, cfs-plan, cfs-types, cfs-codec, cfs-runtime }  (t16 FIRST concrete
                                driver; LEAF runtime consumer — bridges the synchronous PlanApplier →
                                async ApplyDriver and registers it in the DriverRegistry. Nothing
                                depends back onto it, so tokio dead-ends here and never re-enters the
                                spine — the precedent the next 11 drivers follow.)
cfs-types  → (serde only)      (LEAF: no workspace deps; the vendor-free type model, t05)
```

Arrows point toward more-foundational crates. There are **no cycles** and **no
back-edges**. Mechanically enforced by `crates/cmd/tests/dep_direction.rs` (a
`cargo metadata` test).

### tokio confinement — the generic runtime-leaf rule (t10/t16)

`cfs-runtime` is the sole impure stage; tokio/futures live there and MUST NOT enter the
pure spine's closure (the invariant that keeps `cfs-plan`'s purity dep-closure test green).
`dep_direction.rs::runtime_is_confined_to_plan_and_types` enforces this in three parts:

1. **(a) source pinned** — `cfs-runtime`'s own workspace deps are exactly `{cfs-plan,
   cfs-types, cfs-txn}` (and `cfs-txn` is itself pure, carrying no tokio), so tokio's source
   is one crate.
2. **(b) generic leaf confinement** — **every** crate that depends on `cfs-runtime` must be a
   **leaf**: no workspace crate may depend back onto it. This encodes *why* a runtime consumer
   is safe (tokio dead-ends in a leaf and cannot transit back into the spine) rather than
   *which* crates were waved through. A non-leaf gaining the edge (e.g. `cfs-core → cfs-runtime`)
   fails automatically — **no per-driver test edit is needed** as the next 11 driver crates land.
3. **(b') identity allowlist** — a small named allowlist (`cfs-driver-local`, `cfs`) pins the
   *intent*: an unintended new runtime consumer is caught even if it happens to be a leaf today.
   A new driver-impl leaf appends its name here (a one-line, reviewable signal); the generic
   leaf check guarantees the append was safe.

`cfs-driver-local` is the first such leaf consumer: it bridges its synchronous `PlanApplier`
to the async `ApplyDriver` and registers it in the `DriverRegistry`.

### Decision D1 — where `CfsError` / `Path` live

design-v1 nominally placed `CfsError` and `Path` in `cfs-core`, but the `Driver` and
`Codec` trait signatures return `Result<_, CfsError>` and take `&Path`, while the
spine requires `cfs-core → cfs-driver`. Putting them in `cfs-core` would force a
cycle. They therefore live in **`cfs-driver`** (the lowest crate the signatures
need); `cfs-codec` depends on `cfs-driver` for the shared error, and `cfs-core`
**re-exports** both so the workspace still sees one `cfs_core::CfsError` /
`cfs_core::Path`. This preserves "one error enum, shared workspace-wide" while keeping
the spine strictly acyclic.

### Decision D2 — the `cfs-types` leaf (t05)

The canonical type & schema model (`Value`/`Row`/`RowBatch`, `Schema`/`ColumnType`,
`TypeError`, schema algebra, typed predicates) lives in a dedicated **leaf** crate
`cfs-types` (pre-reserved by the Architect). E0 shipped placeholder `Value`/`Row`/
`RowBatch` in `cfs-codec`; t05 promotes them to the canonical typed model and
`cfs-codec` now **re-exports** them from `cfs-types`, so the `bytes ↔ rows` boundary
and the evaluator speak one row model. The crate depends on **no other workspace
crate** (only `serde`/`serde_json`, the latter solely as the carrier for the `Json`
value of irregular columns), keeping it the lowest node in the spine and the type
model vendor-free (RFD §9, G6). To preserve that leaf status, `DriverId` is defined
*inside* `cfs-types` (an owned newtype, never a vendor handle) rather than imported
from `cfs-driver`, and the `SchemaSource` trait takes a logical segment list
(`&[Name]`) instead of the driver `Path`; E4 adapts the driver `Path` at the boundary.
`cfs-core` depends on `cfs-types` and re-exports it so the workspace sees one
`cfs_core::Schema` / `Value` / `TypeError`. Mechanically enforced by
`dep_direction.rs::types_is_a_leaf_and_codec_depends_on_it`.

### Wired edge — `cfs-core → cfs-parser` (acceptance criterion C5, E1)

The edge is now **wired** (E1): name resolution (`cfs_core::resolve`, t06) and the pure
evaluator (`cfs_core::eval`, t07) both consume the parsed `cfs_parser::Statement` AST —
the resolver binds `CALL`/alias names and gates effect verbs, and the evaluator folds a
statement into a `cfs_plan::Plan` (effect-plan) + logical `PlanSource` relation. The edge
is one-directional so `cfs-parser` can never depend on `cfs-core` (cycle prevention).
`dep_direction.rs::core_depends_on_parser_one_directionally` asserts the edge is present
and the back-edge absent.

## Boundary rules (must hold for every later ticket)

1. **Closed core / one keyword home (G1).** The reserved-keyword set lives only in
   `cfs-lang::KEYWORDS`. A new backend adds *zero* keywords — it registers a path,
   procedure, or codec instead. The freeze test (`cfs-lang`) locks the set.
2. **Open registries generic over trait objects (G2).** Extension = `register(...)`
   into one of the three `cfs-core` registries. Registries hold `Arc<dyn Driver>` /
   `Arc<dyn Codec>` / owned `ProcedureDecl`, never concrete types. `MountRegistry`
   additionally routes a full path to a driver by **longest mount-prefix**
   (`MountRegistry::resolve_path`), so overlapping mounts (`/g`, `/git`) resolve
   deterministically and the matched mount is stripped to a driver-local sub-path.
3. **Purity invariant at the type level (G3).** `Driver` / `Codec` methods return
   data (or `Plan` nodes); none take `&mut self`, return a future, or perform I/O. The
   only impure op (`COMMIT : Plan -> World`) is reserved for E2 and absent from the
   traits. Proven by the no-I/O dummy-impl tests in `cfs-driver` / `cfs-codec`.
4. **Server is a driver (G4).** `cfs-server` reserves `/server` as a mount and a
   `TODO(E7)` anchor; the server must be registered as a `Driver`, never a bespoke
   subsystem.
5. **cmd is logic-free (G5 / C4).** `cfs-cmd` depends on `cfs-core` + `cfs-server`
   only. Enforced by `dep_direction.rs`.
6. **Parser boundary reversibility (G6).** The chosen parser library's types never
   appear in `cfs-parser`'s public API — wrapped behind an owned `ParseError` and the
   `parse_statement` signature (t02).
7. **No vendor leak / owned DTOs (B3).** No SDK/vendor type crosses the `cfs-driver`
   boundary. DTOs are owned and `#[non_exhaustive]` with `new` constructors.
8. **No credentials (B8).** No secret/credential field anywhere; none in tests or CI.

## wasm-friendliness (B7)

The core crates (`cfs-core`, `cfs-lang`, `cfs-plan`, `cfs-driver`, `cfs-codec`,
`cfs-parser`) avoid threads, `std::fs`, and sockets so the future `wasm32` target
(RFD §1/§9) stays cheap. All real I/O lives behind (future) driver impls. `unsafe`
code is `forbid`-den workspace-wide.

## Cross-compile status

- **native `aarch64-unknown-linux-gnu`**: built & tested locally (this host is
  Graviton/aarch64). This is the local proof.
- **`x86_64-unknown-linux-gnu`**: lib crates cross-compile; the full binary link is
  **CI-only** (no x86_64 cross-linker on the local aarch64 host).
- **`wasm32-unknown-unknown`**: **deferred** per t01 — not built locally or in CI yet
  (a parked t02 / future-E0-sibling concern). Code is kept wasm-friendly per B7.

## Lints

Workspace lints (`Cargo.toml` `[workspace.lints]`): `unsafe_code = forbid`; clippy
`all = deny` plus `unwrap_used` / `expect_used` / `panic = deny` on non-test lib code.
Test modules opt out via `#![cfg_attr(test, allow(...))]` (and integration tests via a
file-level `#![allow(...)]`). Gates: `cargo fmt --all --check`, `cargo clippy
--workspace --all-targets --all-features -- -D warnings`, `cargo build --workspace`,
`cargo test --workspace`.
