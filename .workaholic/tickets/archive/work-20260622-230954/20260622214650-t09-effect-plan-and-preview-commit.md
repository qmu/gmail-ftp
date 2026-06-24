---
created_at: 2026-06-22T21:46:50+09:00
author: a@qmu.jp
type: enhancement
layer: [Domain]
effort:
commit_hash: b85c711
category: Added
depends_on: [20260622214650-t05-type-schema-model.md]
---

# Effect-plan representation + PREVIEW/COMMIT semantics

## Overview

This ticket delivers the **effects-as-data spine** of `cfs`: the typed `Plan` —
a DAG of effect nodes — that every write operator evaluates to instead of
performing I/O, plus the `PREVIEW`/`COMMIT` semantics that render and apply it.
This is the heart of RFD §2 (face 3, "Effect-plan") and §6 (Runtime/interpreter),
and it enforces the **purity invariant** of §3: every function has type `… -> Plan`;
the only impure operation is the interpreter (`COMMIT : Plan -> World -> World`).

Without this, `cp/mv/INSERT/UPSERT/UPDATE/REMOVE/CALL` cannot be safely
dry-runnable, testable, or composable, and the server (§8) has nothing to "cause to
run." It is the type that makes `SEND`-as-a-function and unattended automation safe.

## Scope

In-scope:
- The `Plan` value: a typed DAG of `EffectNode`s with dependency edges and an
  `irreversible` flag per node, each tagged with its target driver/path.
- Effect node kinds mirroring the closed-core write verbs: `Read`, `List`,
  `Insert`, `Upsert`, `Update`, `Remove`, `Call`.
- `Plan` construction API used by the evaluator (functions return `Plan`; combinators
  to sequence/depend/merge plans).
- `PREVIEW` rendering: plan tree, per-node **affected counts** (estimated/declared),
  and **irreversible warnings**; human text + `-json` form.
- `COMMIT` entry point trait `PlanApplier` (interface + a no-op/in-memory test
  applier); the contract for ordering, dependency-respecting traversal, and the
  applied-effect ledger hook.

Out-of-scope (deferred):
- Actual driver-backed execution / I/O — the `Driver` trait and real appliers
  (E4 driver tickets).
- Batching + auto-parallelization of independent nodes (Haxl-style) and pushdown
  federation collapse (E2/E3 runtime tickets — sibling "plan optimizer").
- Cross-source transaction orchestration & partial-failure recovery, audit-ledger
  persistence (sibling E2 "transactions/recovery" ticket; we only define the hook).
- Parser/AST and the pure query operators (E1 tickets); we consume the AST, not parse.
- Capability *resolution* logic (E1 driver-contract ticket); we only carry the
  capability/irreversible metadata the planner was handed.

## Key components

New crate module `cfs-core::plan` (pure, no I/O, no vendor deps):

- `enum EffectKind { Read, List, Insert, Upsert, Update, Remove, Call(ProcId) }`
  — closed set, mirrors frozen core write verbs (RFD §3).
- `struct EffectNode { id: NodeId, kind: EffectKind, target: Target, args: Rows,
  irreversible: bool, est_affected: Affected }` where
  `struct Target { driver: DriverId, path: VfsPath }` (owned DTO — **no vendor type
  leaks**, RFD §9), and `enum Affected { Exact(u64), AtMost(u64), Unknown }`.
- `struct Plan { nodes: Vec<EffectNode>, deps: Vec<(NodeId, NodeId)>, returning: Option<Schema> }`
  — a DAG; invariant: acyclic, every dep references an existing node.
- Construction/combinators: `Plan::leaf(node)`, `Plan::pure()` (empty plan for
  query-only statements), `Plan::then(self, other)` (dependency edge),
  `Plan::merge(self, other)` (independent union), `fn depends_on(child, parent)`.
- `trait PlanApplier { fn apply(&mut self, node: &EffectNode) -> Result<AppliedEffect, ApplyError>; }`
  — the **only** impure seam; `COMMIT` walks the DAG in topological order calling it.
- `fn preview(plan: &Plan) -> Preview` / `struct Preview { rows: Vec<PreviewRow>,
  irreversible: Vec<NodeId>, total_affected: Affected }` with `impl Display` and
  `serde::Serialize` (for CLI `-json`, RFD §7).
- `fn commit<A: PlanApplier>(plan: &Plan, applier: &mut A) -> CommitReport`
  — topological traversal; stops/records on first error; returns applied + skipped.

Touched: `cfs-core::types` (T05 `Schema`/`Rows`/`VfsPath`/`DriverId`), and the
evaluator entry (consumes AST → emits `Plan`; thin wiring only here).

## Implementation steps

1. Define `NodeId`, `DriverId`, `ProcId`, `Affected`, `Target` newtypes in `plan/ids.rs`.
2. Define `EffectKind` and `EffectNode` (`plan/node.rs`); derive `Debug, Clone, PartialEq, Serialize`.
3. Define `Plan` + DAG invariants; add `debug_assert`-backed `validate()` (acyclic, dep refs exist).
4. Implement combinators (`leaf/pure/then/merge/depends_on`) returning new `Plan`s (immutable build).
5. Implement topological order (`plan/topo.rs`) returning a stable, deterministic order
   (sort by NodeId within a layer) — golden-test friendly.
6. Implement `preview()` → `Preview`, with `Display` (tree + counts + ⚠ irreversible)
   and `Serialize` for `-json`.
7. Define `trait PlanApplier`, `AppliedEffect`, `ApplyError`; provide `RecordingApplier`
   (test double that records calls and returns declared `Affected`, performs no I/O).
8. Implement `commit()`: topo-walk, dependency-respecting, collect `CommitReport`
   (applied / skipped-due-to-failed-dep), surface ledger hook (`on_applied` callback).
9. Mark irreversible nodes (`Remove`, `Call` of declared-irreversible procs e.g.
   `mail.send`); `preview` must list them explicitly (RFD §10).
10. Wire the evaluator's write paths to emit `Plan` (placeholder driver metadata) so
    `cfs run` can render PREVIEW end-to-end with the test applier.
11. Unit + golden tests; clippy; docs on the module (purity invariant stated in rustdoc).

## Considerations

- **Purity invariant is load-bearing**: keep `cfs-core::plan` free of `async`, I/O,
  and vendor SDK types. The compiler should make "constructing a plan does I/O"
  unrepresentable. `CALL driver.x` builds a `Call` node — it never performs the call.
- **Least-privilege / secrets**: `Plan` carries `DriverId` + `VfsPath` only, never
  credentials or tokens; previews must be safe to log. The applier (out of scope)
  is where secrets enter — keep that boundary crisp so the audit ledger and POLICY
  gating (RFD §8/§10) attach at the applier, not the plan.
- **Idempotency / recovery**: model `Upsert` distinctly from `Insert` so retry-safe
  effects are first-class (RFD §6). `commit()` must be re-runnable against a partially
  applied plan; expose enough in `CommitReport` (applied NodeIds) for a future
  recovery pass to reconstruct from the ledger. The hard part — cross-source
  orchestration/2-phase — is deliberately deferred; here we only guarantee
  deterministic topo order + per-node applied/skipped accounting so recovery is *possible*.
- **Observability**: every node is individually addressable (`NodeId`) and
  serializable; the `on_applied` hook is the single funnel for structured logs and
  the audit ledger. Preview output must be deterministic (stable ordering) for
  golden tests and for diff-based CI dry-runs.
- **Irreversible warnings** are the genuinely user-facing hard part: a `Remove` over a
  *set* may report `Affected::AtMost(n)` (count not known until apply). Be honest in
  the preview (`AtMost`/`Unknown`) rather than fabricating exact counts.
- **Directory/coding standards**: one concern per file under `plan/`; no `unwrap` in
  library code; all public types `#[non_exhaustive]` where the closed-core set could
  gain internal fields without being a breaking grammar change (the *grammar* stays
  frozen; representation may evolve).

## Acceptance criteria

- `cargo build`, `cargo clippy --all-targets -- -D warnings`, `cargo test` all green;
  `cfs-core::plan` has **zero** I/O / async / vendor dependencies (enforce via a
  module-level test that the crate's dep set excludes HTTP/SDK crates).
- Plan assertions: building `INSERT` then `CALL mail.send` produces a 2-node DAG with
  one dependency edge; `Call(mail.send)` and any `Remove` node have `irreversible == true`.
- `Plan::validate()` rejects cyclic graphs and dangling dep references (unit tests).
- `topo()` is deterministic: same plan → identical order across runs (golden test).
- **Golden test**: `preview()` of a representative mixed plan (read + insert + remove +
  call) matches a checked-in golden string and golden JSON, including the irreversible
  warning section and `total_affected`.
- `commit()` with `RecordingApplier` applies nodes in dependency order, and when a
  parent node fails, dependent nodes are reported **skipped** (not applied) — asserted
  via the recorded call log. **No live credentials** and no network used in any test.
