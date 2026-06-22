# Coding Phase — E2E / External Testing — t11 (Transactions, Idempotency, Optimistic Concurrency)

**Agent**: Planner (Progressive)
**Role**: E2E / external-consumer validation only (no code review, no unit tests, no production code)
**Ticket**: `20260622214650-t11-transactions-idempotency-concurrency.md`
**Method**: A throwaway external crate (`/tmp/t11_e2e`, own `[workspace]`, path-deps on `crates/{txn,runtime,plan,types}` + tokio/async-trait) consuming ONLY the public API, with in-memory `ApplyDriver`s and an in-memory `LegApplier` (controllable version / failure / dup; NO network, NO credentials). Removed after testing.

Two public seams were exercised as a consumer would use them:
- `cfs_runtime::Interpreter::commit_txn(plan, caps, plan_id, preconditions, transactional, ledger)` — the async integration entrypoint (idempotency dedup via ledger, conflict surfacing, ACID rollback flag).
- `cfs_txn::SagaExecutor::run_saga` / `run_acid` over the synchronous `LegApplier` — the pure orchestration policy (bounded conflict re-read/re-base, reverse-order compensation, irreversible-never-compensated). The t11 ticket's saga compensation and bounded conflict-retry live in this seam; `commit_txn` at E0 surfaces failure boundaries and relies on the ledger for resume, so both seams were validated together.

**Overall result**: 14/14 checks PASS across all 5 items. No panics. Deterministic recovery report confirmed.

---

## Item 1 — Idempotency / apply-once — PASS

- **1a (PASS)** Re-run the same plan with the **same `plan_id` + same ledger** (simulated retry / webhook redelivery): the in-memory driver's REAL mutation counter is `2` after run 1 and **stays `2`** after the re-run — no doubling.
- **1b (PASS)** The re-run reports `AlreadyApplied` for **every** leg (`already_applied=2`, `fresh_applied=0`). The `EffectKey` + ledger `applied()` check makes re-apply a no-op.
- **1c (PASS)** **Crash window**: a ledger pre-seeded with `mark_applied` for leg #0 (intent-append + apply happened) but leg #1 left unsealed. The re-run skips the sealed leg (`AlreadyApplied`) and completes the unsealed leg exactly once — driver mutations = `1` (only the unsealed leg ran). No double-apply across a partial ledger.

**Apply-once evidence (load-bearing):**
```
run1:        applied_count=2  mutation_count=2  already_applied=0
re-run:      applied_count=0  mutation_count=2  already_applied=2   <- count stable, no doubling
crash-resume:driver mutations=1 already_applied=1 fresh_applied=1   <- sealed leg skipped
```

## Item 2 — Optimistic concurrency — PASS

- **2a (PASS)** `Precondition::IfVersion("v7")` against a world at `v7` → `Applied`; the write lands (mutations=1).
- **2b (PASS)** Stale write (`IfVersion("v7")` against world at `v9`, `conflict_retries=0`) → typed **`Conflict(Version("v9"))`** carrying the version the world actually holds; **zero writes** (no lost update). The conflict is branchable (`failure_at=Some(#0)`).
- **2c (PASS)** Transient conflict with `conflict_retries=1`: the leg conflicts once, the bounded **re-read/re-base** re-conditions the write on the world version, and the retry succeeds → `Applied`, mutations=1.
- **2d (PASS)** Persistent conflict with `conflict_retries=2`: after the bounded retries are exhausted the executor **surfaces the typed `Conflict`** (mutations=0) rather than blindly overwriting.

**Conflict dump (2b):**
```
2b conflict dump: outcome=conflict failure_at=Some(NodeId(0)) legs[0]=Conflict(Version("v9"))
```

## Item 3 — Transaction atomicity + saga compensation — PASS

- **3a (PASS)** Single-source plan (driver `sql`, declared `Transactional`) where the **later** leg fails → `select_strategy` chose `single_source_acid`; the report sets `rolled_back=true` with `failure_at=Some(#1)`. The transaction is signalled atomic-failed (the driver issues the real `ROLLBACK`); no partial commit is acknowledged to the consumer.
- **3b (PASS)** Cross-source saga (`gmail` then `s3` x2) where leg #2 fails → the two earlier applied legs are **compensated in reverse order**. `compensated = [#1, #0]`, deletes ran `[#1, #0]`, and **no created resource remains** (`created` empty). A created resource is deleted; the inverse of an `Insert` is `DeleteCreated`.
- **3c (PASS)** `CpStep::mv_sequence()` = `[COPY, VERIFY, DELETE]` — copy → verify → delete; the source delete is **never** reached before the copy is verified (no delete-before-verify).

**Reverse-order compensation trace (3b):**
```
APPLY      #0 on gmail:/mail/A
APPLY      #1 on s3:/bucket/B
(leg #2 s3:/bucket/C fails)
COMPENSATE delete #1 on s3:/bucket/B   <- newest applied first
COMPENSATE delete #0 on gmail:/mail/A  <- reverse order
compensated = [NodeId(1), NodeId(0)]   created-remaining = []
```

## Item 4 — Irreversible safety — PASS

- **4a (PASS)** A `REMOVE` node carries `irreversible=true` inherently (from `EffectKind::is_inherently_irreversible`).
- **4b (PASS)** In a failing saga with a reversible `Insert` (#0), an irreversible `REMOVE` (#1, even with a `DeleteCreated` compensation wrongly registered), and a failing leg (#2): compensation undoes **only** the reversible leg → `compensated = [#0]`. The irreversible `REMOVE` is **never** compensated.
- **4c (PASS)** An irreversible `CALL mail.send` that conflicts, with `conflict_retries=3`: the executor forces `max_conflict_attempts=0` for irreversible legs, so it **never re-reads/re-applies** — it surfaces the `Conflict` immediately (mutations=0). Irreversible legs are applied at most once and never retried.

## Item 5 — No panics, deterministic recovery report — PASS

- **5a (PASS)** The same failing saga run twice produces a **byte-identical `RecoveryReport`** (`==` on the whole struct: same leg order, outcomes, `failure_at=Some(#2)`, `compensated=[#1,#0]`).
- **5b (PASS)** `EffectKey::derive(plan_id, node)` is deterministic across derivations (same `(plan_id, node)` → equal key) — idempotency-key stability.
- **5c (PASS)** A clean re-run over a fully-applied saga reports `AlreadyApplied` for all legs and mutates nothing further (mutations `2 → 2`).
- **No panics** observed in any path (conflict, failure, crash-resume, compensation, irreversible). The harness ran to completion with `exit 0`.

---

## Concern / trade-off (Critical Review Policy)

**Concern (business-outcome lens):** At E0 the two halves of the t11 guarantee live in **different public seams**. `Interpreter::commit_txn` performs idempotency dedup and ACID rollback signalling, but it does **not itself run saga reverse-order compensation** (its `compensated` vector is always empty — compensation directives are noted as E4-supplied). The full saga compensation + bounded conflict re-read I validated runs through `cfs_txn::SagaExecutor` driven by a `LegApplier`. A consumer who only calls `commit_txn` for a cross-source plan today gets a correct **failure boundary + resumable ledger**, but the automatic reverse-order undo is not yet wired into that async entrypoint.

This is **consistent with the ticket's stated E0 scope** ("compensation directives are E4-supplied; at E0 the report records the failure boundary and the ledger enables a recovering re-run"), so it is not a defect — the underlying `SagaExecutor` policy is correct and fully exercised. But from the stakeholder's perspective the end-to-end "cross-source plan auto-compensates on failure via one call" story is split across two APIs.

**Constructive proposal:** In a follow-up (E4, when the async `LegApplier` bridge lands), wire `Interpreter::commit_txn` to drive `SagaExecutor::run_saga` for the `CrossSourceSaga` strategy so the async entrypoint emits a populated `compensated` vector — closing the loop so a single `commit_txn` call delivers the same reverse-order compensation a consumer gets from the pure executor today. Until then, the runtime's `commit_txn` docstring already states this boundary clearly, which preserves stakeholder traceability.

---

## Verdict

**E2E approved.** All five required behaviours are demonstrably correct from an external consumer's vantage with no network and no production code: apply-once idempotency (mutation count stable across re-run and across a partial-ledger crash), typed optimistic-concurrency `Conflict` with bounded re-read recovery and persistent-conflict surfacing, single-source ACID rollback, cross-source reverse-order saga compensation with no surviving partial writes, irreversible effects never compensated or retried, and a deterministic panic-free recovery report. The one observation above is a scope-aligned wiring gap (compensation auto-driven only through `SagaExecutor` at E0), not a correctness failure.
