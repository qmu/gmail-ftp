---
created_at: 2026-06-22T21:46:50+09:00
author: a@qmu.jp
type: enhancement
layer: [Domain]
effort:
commit_hash: 8b76989
category: Added
depends_on: [20260622214650-t10-interpreter-batch-parallel.md]
---

# Transactions, idempotency, optimistic concurrency

## Overview

Implements the transactional/recovery guarantees of RFD 0001 §6 ("Runtime / interpreter")
and the safety posture of §10. The interpreter from ticket #10 already walks the effect-plan
DAG, batches, and parallelizes independent legs. This ticket gives that interpreter its
**correctness envelope**: when every effect in a plan targets a single source whose driver
supports real transactions, the commit runs as one ACID `BEGIN…COMMIT`/rollback; when a plan
spans sources (no distributed txn exists), the commit becomes an **orchestrated best-effort
saga** with explicit partial-failure recovery and an **applied-effect audit ledger** that lets
a later run reconstruct/resume. It also delivers the two idempotency mechanisms the RFD names:
`UPSERT` for retry-safe at-least-once writes, and `@version`/ETag **optimistic concurrency**
for read-then-write. This is what makes server-side unattended plans (§8) and webhook retries
safe without a human in the loop.

## Scope

In scope:
- Single-source ACID commit path: driver-declared transaction capability, `begin/commit/rollback`.
- Cross-source saga executor: ordered legs, per-leg compensation, partial-failure classification.
- `cp` as the canonical recoverable pattern: copy → verify → delete, never delete-before-verify.
- Idempotency: stable effect/idempotency keys; `UPSERT` semantics surfaced through the commit path.
- Optimistic concurrency: capture `@version`/ETag at read, send `If-Match`/expected-version at write,
  surface `Conflict` as a typed, retryable error.
- Audit/applied-effect ledger: append-before-apply, mark-applied-after, used for reconstruction.

Out of scope (deferred):
- The audit ledger *storage backend* / structured-log sink and circuit-breaker/observability plumbing
  beyond the ledger contract → cross-cutting observability ticket (E8).
- Server bindings (`TRIGGER`/`JOB`/`WEBHOOK`) that *consume* idempotency keys for at-least-once
  delivery → E7 server tickets.
- `POLICY`/capability-gating enforcement engine (we only *consume* declared capabilities here) → E5/E8.
- Pushdown federation planning itself → E3 (this ticket assumes the plan/leg grouping from #10).

## Key components

New crate `qfs-txn` (pure orchestration over the `Driver` trait; no vendor types):

- `enum CommitStrategy { SingleSourceAcid(SourceId), CrossSourceSaga }` — chosen by the planner
  by inspecting which sources a plan's leaves touch.
- `trait Transactional` (optional super-capability a `Driver` may declare):
  `fn begin(&self) -> Result<TxnHandle>; fn commit(TxnHandle); fn rollback(TxnHandle);`
  Drivers that don't implement it are saga-only.
- `struct EffectKey(String)` — deterministic idempotency key derived from `(plan_id, effect_id,
  canonical(args))`; stable across retries.
- `enum Precondition { None, IfVersion(Version), IfMatchEtag(Etag) }` — owned DTOs (no `reqwest`/
  vendor ETag types leak); attached to each effect node from the read that produced it.
- `enum LegOutcome { Applied(EffectReceipt), AlreadyApplied, Conflict(Version), Failed(EffectError) }`.
- `struct SagaExecutor` — drives `Vec<EffectLeg>`; on failure runs registered `Compensation` for
  applied legs in reverse; emits a `RecoveryReport`.
- `trait AuditLedger { fn record_intent(&self, &EffectKey, &EffectDescriptor); fn mark_applied(&self,
  &EffectKey, &EffectReceipt); fn applied(&self, &EffectKey) -> Option<EffectReceipt>; }` — the
  contract only; default in-memory + file impl, real sink deferred.
- `enum CpStep { Copy, Verify, Delete }` — `mv`/`cp` across mounts compiles to this triple so a crash
  between steps is recoverable from the ledger (verify is idempotent; delete is keyed).

Touches: `qfs-plan` (effect-plan node gains `precondition` + `effect_key`), `qfs-runtime` interpreter
(`#10`) commit entrypoint dispatches on `CommitStrategy`, `Driver` trait (declares `Transactional`,
exposes `apply_effect(effect, precondition) -> LegOutcome`). Purity invariant preserved: planning still
produces a `Plan`; only the `commit` entrypoint is impure.

## Implementation steps

1. Add `precondition: Precondition` and `effect_key: EffectKey` to the effect-plan node in `qfs-plan`;
   thread `@version`/ETag captured during reads into the node that writes back.
2. Define `EffectKey` derivation (canonical arg serialization, stable hash) + golden tests for stability.
3. Extend `Driver` with `apply_effect(&self, &Effect, &Precondition) -> LegOutcome` and the optional
   `Transactional` super-trait; provide a fake in-memory driver for tests.
4. Implement `AuditLedger` contract + in-memory and append-only-file impls (record-intent-before-apply).
5. Implement single-source ACID path: planner detects single `SourceId`, opens `begin`, applies legs,
   `commit`/`rollback` on first error.
6. Implement `SagaExecutor`: ordered apply, `record_intent → apply → mark_applied`, skip legs whose
   `EffectKey` is `applied()` (idempotent resume), reverse-order compensation on failure.
7. Compile cross-mount `cp`/`mv` to `CpStep` triple (copy → verify → delete-keyed); make `mv` recoverable.
8. Implement optimistic-concurrency: drivers honor `Precondition`; map upstream 412/precondition-failed
   to `LegOutcome::Conflict`; bounded auto-retry of read-then-write with backoff, else typed error.
9. Wire interpreter (`#10`) `commit` to select `CommitStrategy` and emit a `RecoveryReport` in `-json`.
10. Plan-level tests: `PREVIEW` shows strategy + idempotency keys + preconditions without executing.

## Considerations

- **Hard part — partial cross-source failure.** No 2PC across Gmail/S3/git. We accept best-effort and
  make it *reconstructable*: ledger is append-before-apply so a crash mid-saga leaves a record that the
  next run reads to resume or compensate. `cp` ordering (verify before delete) guarantees no data loss
  on the recoverable path; a failed delete leaves a harmless duplicate, never a hole.
- **Idempotency vs at-least-once.** Webhook/trigger redelivery (E7) re-runs the same plan; `EffectKey`
  + ledger `applied()` check make re-apply a no-op (`AlreadyApplied`). `UPSERT` covers the case where the
  driver itself is the dedup point; the ledger covers procs/`CALL` that aren't naturally idempotent.
- **Optimistic concurrency races.** Capture-version-at-read must survive the batch/parallel reorder from
  #10; the version travels *on the effect node*, not in interpreter-global state, so reordering is safe.
- **Secrets / least-privilege.** Ledger records effect *descriptors and receipts*, never credential
  material or full payloads (RFD §10: credentials never logged); redact at the `EffectDescriptor` boundary.
- **Observability.** Emit a structured `RecoveryReport` per commit; the ledger is the audit-of-record.
  Real sink/circuit-breakers are E8 — keep the `AuditLedger` trait the only seam so swapping is trivial.
- **Coding standards.** `qfs-txn` stays pure-orchestration; vendor/SDK ETag/txn types are converted to
  owned DTOs (`Version`, `Etag`) at the driver boundary — they must not appear in `qfs-txn` signatures.

## Acceptance criteria

- `cargo build`, `cargo clippy -- -D warnings`, `cargo test` green; no live credentials in tests
  (fake in-memory driver + ledger only).
- Single-source plan over a `Transactional` fake driver: an injected mid-plan error leaves **zero**
  applied effects (rollback proven by ledger).
- Cross-source saga: injected failure on leg N runs compensation for legs `1..N-1` in reverse; a re-run
  of the same plan re-applies nothing (`AlreadyApplied` for every prior `EffectKey`).
- `cp`/`mv` across mounts: a fault injected *after copy, before delete* leaves source intact and, on
  re-run, completes the delete — no data loss, verified by golden assertion.
- Optimistic concurrency: read-then-write where the version changed underneath yields a typed
  `Conflict`; with auto-retry enabled it re-reads and succeeds; golden test asserts the `If-Match` sent.
- **Plan assertions / golden tests**: `PREVIEW` of each above plan emits the chosen `CommitStrategy`,
  the per-effect `EffectKey`, and `Precondition`, and executes nothing (purity invariant intact).
- `EffectKey` derivation is deterministic across runs (golden hash) and stable under batch reordering.
