---
created_at: 2026-06-22T21:46:50+09:00
author: a@qmu.jp
type: enhancement
layer: [Infrastructure, Domain]
effort:
commit_hash: 53df891
category: Added
depends_on: [20260622214650-t05-type-schema-model.md, 20260622214650-t09-effect-plan-and-preview-commit.md]
---

# Driver contract (trait): archetype, schema, capabilities, procs, pushdown, prelude, @version

## Overview

This ticket delivers the **single `Driver` trait** every backend (Gmail, Drive, S3/R2,
D1, SQL, git, GitHub, Slack, local FS, generic REST, …) implements to plug into the
federation engine. It is the backbone of **RFD §5 (Driver contract)** and the
governance promise of **RFD §3**: a new service adds **zero keywords** — it only
*declares* a namespace mount, per-node archetype, schema, capabilities, procedures,
pushdown ability, and an optional prelude. That declaration is *everything the engine
and the AI need*: the AI introspects it (`DESCRIBE`) and follows one operating
procedure instead of N SDKs.

The contract is the seam that makes the whole design hold: **capabilities** drive
**parse-time rejection** of unsupported verbs (structured, AI-consumable errors);
**schema** powers `DESCRIBE` and type-checking (t05); **procedures** are the only
legal targets of `CALL` (RFD §3 — `CALL` resolves only declared procs); **pushdown**
tells the planner what a source can execute natively; **prelude** ships pure
receiver-typed aliases like `SEND` (RFD §3, t06). Crucially, this is a **declaration +
metadata** contract plus a thin **execution seam** that returns `Plan` nodes / applies
them — it carries the §3 purity invariant down to the driver boundary. Per RFD §9 the
trait surface uses **owned DTOs only**: vendor/SDK types never leak past a driver.

## Scope

In-scope:
- The `Driver` trait: identity (`DriverId`/namespace), `describe(path)` → archetype +
  `Schema` (t05), `capabilities(path)`, `procedures()`, `pushdown()` profile,
  `prelude()`, and `@version` support declaration.
- The owned DTOs the trait trades in: `Archetype`, `Capabilities`, `ProcSig`,
  `PushdownProfile`, `VersionSupport`, plus the `DriverRegistry` (the **paths**
  open registry of RFD §3).
- **Capability gating** API: a parse/resolve-time check mapping a planned verb at a
  path to allow/reject, emitting a **structured** `CapabilityError` (RFD §5).
- The execution seam: how a driver yields `Plan` effect nodes (t09) and applies them
  via `PlanApplier` — i.e. a `Driver` *is* the backing of `PlanApplier` for its
  `Target`s — without performing I/O during planning.
- A reference **in-memory / fixture driver** for tests (no live creds).

Out-of-scope (deferred):
- Concrete real drivers (Gmail, Drive, S3, SQL, git, GitHub…) — **E4 driver tickets**;
  this defines only the trait + registry they implement.
- Codec registry (`DECODE`/`ENCODE`) — sibling **E3 codec** ticket; drivers expose blob
  bytes, codecs bridge to rows.
- Pushdown *planning/collapse* and the local combine engine — **E2/E3 runtime**
  (sibling "plan optimizer"); here a driver only *declares* what it can push down.
- Auto-batching / parallel application of independent nodes — t10 interpreter.
- Credential store, auth flows, per-handler `POLICY` — **E5/E7** (we only reserve a
  `requires_scopes` hint on `ProcSig` and capabilities).
- `@version` path *parsing* — path/addressing ticket; we consume a resolved coordinate.

## Key components

New crate module `cfs-core::driver` (declaration DTOs, pure, `wasm32`-safe) plus
`cfs-driver` (registry + trait object glue). No vendor deps in the trait surface.

```rust
pub trait Driver: Send + Sync {
    fn id(&self) -> DriverId;                          // namespace, e.g. "mail"
    fn describe(&self, path: &VfsPath) -> Result<NodeDesc, DriverError>;
    fn capabilities(&self, path: &VfsPath) -> Capabilities;
    fn procedures(&self) -> &[ProcSig];                // CALL targets (RFD §3)
    fn pushdown(&self) -> &PushdownProfile;            // what it runs natively (§6)
    fn prelude(&self) -> &[AliasDef] { &[] }           // pure aliases, e.g. SEND
    fn version_support(&self, path: &VfsPath) -> VersionSupport; // @version (§4)
    fn applier(&self) -> &dyn PlanApplier;             // the only impure seam (t09)
}

pub struct NodeDesc { pub archetype: Archetype, pub schema: Schema }      // powers DESCRIBE
pub enum Archetype { Blob, Relational, Append, ObjectGraph }             // RFD §5 four archetypes
pub struct Capabilities { verbs: BitFlags<Verb> }   // INSERT/UPSERT/UPDATE/REMOVE/SELECT/LS/CP/MV/RM
pub struct ProcSig { pub name: ProcId, pub params: Vec<Param>, pub irreversible: bool,
                     pub returns: Option<Schema>, pub requires_scopes: Vec<Scope> }
pub enum PushdownProfile { None, Partial { where_: bool, project: bool, limit: bool,
                           order: bool, join: bool }, Full }
pub enum VersionSupport { None, Snapshot, Versioned }  // git refs / s3 versionId / drive rev
```

- `enum Verb { Select, Insert, Upsert, Update, Remove, Ls, Cp, Mv, Rm }` — closed,
  mirrors RFD §3 universal verbs; a node’s archetype implies sensible defaults a driver
  may narrow.
- `struct DriverRegistry { mounts: Map<DriverId, Arc<dyn Driver>> }` with
  `resolve(path) -> (Arc<dyn Driver>, sub-path)` — the **paths** registry of RFD §3.
- `fn check_capability(reg, path, verb) -> Result<(), CapabilityError>` and
  `fn check_proc(reg, ProcId, args) -> Result<&ProcSig, CapabilityError>` — the
  parse/resolve-time gate (called by t06 resolution). `CapabilityError` is structured
  (driver, path, verb, supported-list) for AI consumption (RFD §5).
- `enum DriverError { Unsupported, NotFound, Structured(..) }` — owned, no vendor leak.

## Implementation steps

1. Define the declaration DTOs (`Archetype`, `Capabilities`/`Verb`, `ProcSig`/`Param`,
   `PushdownProfile`, `VersionSupport`, `NodeDesc`) in `cfs-core::driver`; derive
   `Debug`/`Clone`/`Serialize` for `-json DESCRIBE` and golden tests.
2. Define the `Driver` trait over those DTOs + `Schema` (t05) and `PlanApplier`/`Plan`
   (t09); keep it object-safe (`Arc<dyn Driver>`).
3. Implement `DriverRegistry` with longest-mount-prefix `resolve`; reject ambiguous /
   unmounted paths with a structured error.
4. Implement `check_capability` / `check_proc`; wire them as the gate t06 resolution
   and the evaluator call before building any `Plan` node.
5. Define `CapabilityError`/`DriverError` and a `to_structured()` rendering (verb,
   path, archetype, `supported: [...]`) suitable for AI.
6. Implement `describe` → `DESCRIBE` output projection (archetype + columns + caps +
   procs + version support).
7. Build a `FixtureDriver` (in-memory blob+relational sub-paths, a couple procs incl.
   one `irreversible`, a partial pushdown profile) and a no-op/in-memory `PlanApplier`.
8. Capability-gating golden tests + `DESCRIBE` snapshot tests against the fixture.

## Considerations

- **Owned DTOs / no vendor leak (RFD §9):** the trait must compile with zero SDK deps;
  drivers translate vendor responses into `Schema`/`Row`/`Plan` internally. Enforce
  with a workspace lint / dep-graph check so `cfs-core` never gains a vendor edge.
- **Capability gating is parse-time, not run-time (RFD §5):** the gate must run during
  resolution so an unsupported verb fails *before* a `Plan` exists — a hard
  ordering constraint with t06/evaluator. Genuinely tricky part: capabilities are
  *per-node* (path-dependent), and a single driver mixes archetypes on sub-paths (git:
  blob `@ref/path`, relational `commits`, append `refs`) — `capabilities(path)` must be
  path-keyed, not driver-global.
- **Purity invariant (RFD §3):** `describe`/`capabilities`/`procedures`/`pushdown` are
  pure and must do no I/O (so `PREVIEW` and CI dry-runs never touch the World); the only
  impure method is the `applier()` seam, exercised solely under `COMMIT`. Keep the
  introspective half callable in `wasm32` with no network.
- **Least-privilege & secrets (RFD §10):** `ProcSig.requires_scopes` lets the server
  `POLICY` reason about blast radius; the trait must never carry or log credentials —
  auth is injected at construction (E5), not part of the contract surface.
- **Idempotency / recovery (RFD §6):** surface `irreversible` per proc and per effect
  node so `PREVIEW` can warn and policy can block; prefer `Upsert`-capable nodes for
  retry-safety. `@version`/ETag declared via `VersionSupport` enables optimistic
  concurrency for read-then-write.
- **Observability:** `DriverError`/`CapabilityError` are structured and carry enough
  context (driver, path, verb, supported set) for the audit log and AI feedback loop.
- **Coding standards:** trait lives in `cfs-core::driver`; concrete drivers in their own
  crates under `drivers/` (E4); registry in `cfs-driver`. Object-safe trait; no `async`
  in the introspective methods (keep them `wasm32`-pure).

## Acceptance criteria

- `cargo build` + `cargo clippy -- -D warnings` green across the workspace; `cfs-core`
  builds for `wasm32-unknown-unknown`.
- A **no-vendor-deps** assertion on `cfs-core::driver` (dep-graph test) passes.
- **Capability golden test:** planning `UPDATE` against an append-only fixture node is
  rejected at resolve time with a structured `CapabilityError` listing the supported
  verbs; the error serializes stably (snapshot).
- **`DESCRIBE` golden/snapshot test:** describing a multi-archetype fixture emits the
  correct per-path archetype, schema columns, capability set, proc signatures (incl.
  `irreversible`), pushdown profile, and `@version` support.
- **Plan assertion:** a `CALL fixture.proc(...)` resolves only because the driver
  declares it (an undeclared `CALL` is rejected structurally), and produces a `Plan`
  with one `Call` node tagged `irreversible` where declared — **no I/O performed**
  (in-memory applier, no live creds).
- Pure-introspection methods proven side-effect-free (run under a test harness with no
  network/credentials available).
