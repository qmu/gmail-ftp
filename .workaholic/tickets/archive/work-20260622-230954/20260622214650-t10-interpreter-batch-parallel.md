---
created_at: 2026-06-22T21:46:50+09:00
author: a@qmu.jp
type: enhancement
layer: [Domain, Infrastructure]
effort:
commit_hash: 52c14d1
category: Added
depends_on: [20260622214650-t09-effect-plan-and-preview-commit.md, 20260622214650-t13-driver-contract-trait.md]
---

# Interpreter: apply plans with auto-batching + parallelism

## Overview

This ticket delivers the **impure edge** of qfs: the interpreter that takes a typed
effect-plan DAG (built by t09) and *applies* it against the world. Per RFD §3 the only
impure operation in the system is `COMMIT : Plan -> World -> World`; everything upstream
constructs effects as data. This ticket implements the executor for that signature.

The defining behavior (RFD §6) is **Haxl-style auto-batching with parallelism**: the
interpreter sees the *whole* graph before running anything, so independent effects at the
same DAG frontier are grouped per-driver and dispatched as one batched call, while dependent
chains run in order. This is the mechanism that collapses Gmail N+1 message listing into a
single batched fetch — the planner emits N independent leaf effects, the interpreter coalesces
them. It also enforces backpressure and concurrency limits so a wide frontier cannot exhaust
file descriptors, rate limits, or memory.

Pushdown federation (collapsing same-source subtrees into one native query) is **out of scope**
here and lives in E3; this interpreter executes whatever leaves the plan presents, batching at
the driver-call boundary only.

## Scope

In-scope:
- Walk the effect-plan DAG honoring dependency edges; compute ready-frontiers.
- Group ready effects by `(driver, op)` and invoke the driver's **batch** entrypoint once per group.
- Bounded parallelism across independent groups (global + per-driver concurrency caps, backpressure via a semaphore/buffered scheduler).
- Per-leg timeouts and bounded retries on retryable effects; propagate the audit-ledger entries (RFD §6).
- Wire `COMMIT` (from t09) to this executor; honor the `irreversible` flag (no auto-retry on irreversible legs).
- Capability gating re-check at apply time (defense in depth; parse-time check is t13).

Out-of-scope (deferred):
- Pushdown / same-source subtree collapse → E3 federation ticket.
- Cross-source transaction orchestration & partial-failure recovery (cp = copy→verify→delete) → sibling E2 transactions ticket.
- The actual `PREVIEW` rendering and plan construction → t09.
- Concrete driver batch implementations (Gmail/Drive/etc.) → E4; this ticket consumes the `Driver` trait from t13 with a mock driver.

## Key components

New crate/module `qfs-runtime` (`src/runtime/`):

- `interpreter.rs` — the executor.
  ```rust
  pub struct Interpreter {
      drivers: Arc<DriverRegistry>,   // from t13
      limits: ConcurrencyLimits,
  }
  pub struct ConcurrencyLimits { pub global: usize, pub per_driver: usize }

  impl Interpreter {
      pub async fn commit(&self, plan: Plan, caps: &CapabilitySet)
          -> Result<Outcome, ApplyError>;
  }
  ```
- `schedule.rs` — DAG frontier scheduler.
  ```rust
  /// Topo-ordered ready-set iterator over Plan nodes; yields the next batch of
  /// effects whose deps are all satisfied.
  struct Frontier<'p> { plan: &'p Plan, done: BitSet, .. }
  fn next_ready(&mut self) -> Vec<EffectId>;
  ```
- `batch.rs` — coalesce ready effects by `(DriverId, EffectKind)` into one `BatchRequest`,
  fan results back to per-effect `EffectResult`. Keys must be derived from **owned DTOs**, never
  vendor types (RFD §9) — driver SDK types stay behind the `Driver` boundary.
- `Driver` trait surface consumed (defined in t13):
  ```rust
  async fn apply_batch(&self, kind: EffectKind, effects: &[EffectInput], cx: &ApplyCx)
      -> Vec<Result<EffectOutput, EffectError>>;
  ```
  A driver that lacks a true batch endpoint provides a default that maps over singletons —
  batching is an interpreter contract, not a driver requirement.
- `Outcome` — applied-effect ledger entries (effect id, driver, status, `irreversible`,
  duration) feeding the audit log (RFD §6, §10). Owned, serializable (`-json`).
- `ApplyError` / `EffectError` — structured (machine-readable for AI), distinguishing
  retryable vs terminal vs capability-denied.

Respects: closed-core grammar (interpreter adds no keywords), three open registries (resolves
drivers/procs via registries only), effects-as-data + purity invariant (interpreter is the sole
impure stage), owned DTOs, capability gating (re-checked here).

## Implementation steps

1. Define `ConcurrencyLimits`, `Outcome`, `ApplyError`, `EffectError` in `qfs-runtime`.
2. Implement `Frontier`: validate the DAG is acyclic, compute in-degree, yield ready-sets as deps resolve.
3. Implement `batch.rs` coalescing: stable grouping key `(DriverId, EffectKind)`; preserve effect identity for result fan-out.
4. Implement the scheduler loop: pull a frontier, group it, spawn one task per group under a global `Semaphore(global)` and a per-driver `Semaphore(per_driver)` (backpressure).
5. Invoke `Driver::apply_batch`; wrap each leg in per-leg timeout + bounded retry (skip retry when `irreversible`).
6. On each completed effect: append a ledger entry, mark node done, advance the frontier; on terminal error, stop scheduling new dependents but drain in-flight.
7. Re-check capability gating per effect against `CapabilitySet` before dispatch.
8. Wire `COMMIT` (t09) → `Interpreter::commit`; surface `Outcome` to CLI (`-json`) and audit log.
9. Tests: mock `Driver` that records batch group sizes; golden plan→ledger fixtures.

## Considerations

- **Hard part — correct batching frontier.** Grouping must happen *across the whole ready-set*, not
  pairwise, or N+1 won't collapse. Resolve by materializing the full frontier before grouping; assert
  in tests that the mock driver's `apply_batch` is called once (not N times) for N independent same-kind effects.
- **Backpressure vs. throughput.** Two-level semaphores (global + per-driver) bound blast radius and
  respect upstream rate limits; a wide frontier must not spawn unbounded tasks. Make limits config-driven.
- **Idempotency / recovery (RFD §6, §10).** Retries only on retryable, non-`irreversible` legs; `UPSERT`-style
  effects are retry-safe, `CALL mail.send`/deletes are not. The applied-effect **ledger is the recovery
  substrate** — every leg logged before/after apply so a crash can be reconstructed.
- **Least-privilege & secrets (RFD §10).** Capability re-check at apply time; credentials live behind the
  `Driver` boundary and are **never** logged — ledger entries record effect metadata, not payloads/tokens.
- **Observability.** Per-leg timeouts, bounded retries, structured (not string) errors for AI consumption,
  duration + status per effect in the ledger; emit tracing spans per group.
- **Coding standards / structure.** Async via tokio; small consumer-side traits; owned DTOs only — no vendor
  type leaks past drivers. Keep `qfs-runtime` free of any concrete driver dependency (test with a mock).

## Acceptance criteria

- `cargo build` and `cargo clippy -- -D warnings` green; `cargo test -p qfs-runtime` green.
- **Batching assertion:** given a plan with N independent same-`(driver,kind)` leaf effects, the mock
  driver's `apply_batch` is invoked exactly once with N effects (N+1 → 1).
- **Ordering assertion:** for a dependent chain A→B→C, the ledger records A, B, C in dependency order;
  B is never dispatched before A's result is recorded.
- **Concurrency assertion:** with `global=2`, no more than 2 driver groups are ever in flight (instrumented mock).
- **Idempotency/irreversible assertion:** a retryable failure on a non-irreversible effect retries up to the
  bound; an `irreversible` effect is never retried.
- **Capability assertion:** an effect whose driver/verb is not in the `CapabilitySet` is rejected before
  dispatch with a structured `capability-denied` error.
- Golden test: a fixed plan produces a deterministic ledger (`Outcome`) fixture; `-json` output matches.
- No live credentials used in any test (mock `Driver` only).
