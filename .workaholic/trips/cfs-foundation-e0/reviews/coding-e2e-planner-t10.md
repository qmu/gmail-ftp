# Coding-Phase E2E Review — t10 Interpreter (auto-batching + parallelism)

- Author: Planner (Progressive)
- Phase/Step: coding / review-and-testing
- Target: t10 — `cfs-runtime` `Interpreter` (`commit` / `preview`), `ApplyDriver`, `DriverRegistry`,
  `CapabilitySet`, `ConcurrencyLimits`, `RetryPolicy`, `Outcome`/`LedgerEntry`
- Method: external consumer. A throwaway crate under `/tmp/t10_e2e` (own `[workspace]`,
  path-deps on `crates/runtime`, `crates/plan`, `crates/types` + tokio/async-trait; no
  production code) implemented **in-memory `ApplyDriver`s only** — recording, latency,
  shared-overlap, and failing mocks. **No real network, no live credentials.** The crate
  was removed after the run.

## Verdict: E2E approved

All 16 sub-checks across the 6 required items passed. The interpreter behaves correctly
from the outside: it collapses N+1 into one batch, parallelizes across drivers under the
two-level caps, serializes a single driver under `per_driver:1`, skips dependents of a
failed node deterministically, honors the irreversible/retry rule, performs no I/O on
preview, and survives adversarial plans without panicking.

## Per-item results

### Item 1 — Batching: PASS
- **1a** 5 independent same-`(driver,kind)` INSERTs ran as **ONE** `apply_batch` call.
  Evidence: recorded `apply_batch` call sizes = `[5]` (not five calls of size 1); ledger
  applied = 5. This is the N+1 -> 1 property.
- **1b** Distinct kinds on the same driver are **not** merged: INSERT x3 + UPDATE x2 produced
  two batches with sizes (sorted) `[2, 3]`, kinds `["INSERT", "UPDATE"]` — never one merged
  batch of 5.
- **1c** Distinct CALL procs on the same driver are **not** merged: `mail.send` x3 +
  `sms.send` x4 produced two batches with sizes (sorted) `[3, 4]` — never one merged batch
  of 7. The grouping key folds the proc id into the CALL key, as intended.

### Item 2 — Parallelism: PASS
- **2a** Two independent branches across drivers A and B (120 ms artificial latency each)
  actually overlap. Measured **max concurrent `apply_batch` across drivers = 2** (>= 2).
- **2b** `ConcurrencyLimits{ global:4, per_driver:1 }` serializes a single driver while
  others still parallelize. Driver S had **two** distinct-kind groups (INSERT + UPDATE):
  - S `apply_batch` calls = **2** (so two groups really were dispatched),
  - S **max in-flight = 1** (the two groups serialized — per-driver cap honored),
  - cross-driver max overlap = **2** (driver T overlapped S's serialized run — others still
    parallelize).

### Item 3 — Topo + failure: PASS
- B depends on A; A made to **fail terminally**.
- **3a** Ledger records A `failed` (attempts=1).
- **3b** B is recorded `skipped` with `cause = A`, and **B's driver `apply_batch` was never
  called** (the recording mock saw zero calls).
- **3c** Determinism: the serialized `Outcome` JSON was **identical across 5 repeated runs**
  (same plan + same fakes). Entries are emitted in stable topological order, independent of
  wall-clock completion interleaving.

Failure-skip ledger (deterministic, captured verbatim):

```json
{
  "ledger": [
    { "id": 0, "driver": "A", "kind": "insert", "irreversible": false,
      "status": { "status": "failed",
                  "error": { "class": "terminal", "reason": "injected permanent failure" },
                  "attempts": 1 },
      "duration": 0 },
    { "id": 1, "driver": "B", "kind": "insert", "irreversible": false,
      "status": { "status": "skipped", "cause": 0 },
      "duration": 0 }
  ]
}
```

### Item 4 — Irreversible / retry: PASS
RetryPolicy `max_attempts = 3`. A failing mock returns a **retryable** error each time.
- **4a** Reversible INSERT: driver attempts = **3**, ledger `attempts = 3` — retried up to the
  bound, then recorded failed.
- **4b** Irreversible REMOVE (inherently irreversible): driver attempts = **1**, ledger
  `attempts = 1` — **not** retried despite the retryable error.
- **4c** Declared-irreversible CALL `mail.send`: driver attempts = **1**, ledger `attempts = 1`
  — **not** retried. The irreversible veto applies to both `Remove` and declared-irreversible
  `Call`.

### Item 5 — PREVIEW: PASS
`preview` on a 2-node plan (INSERT then REMOVE) made **zero driver calls** (recording mock saw
none) and still produced a 2-entry ledger. Confirms preview is a no-I/O dry run.

### Item 6 — No panics on adversarial plans: PASS
- **6a** Empty plan (`Plan::pure()`): `Ok` with an empty ledger, no panic.
- **6b** Single node: `Ok` with a 1-entry ledger, no panic.
- **6c** Wide fan-out (200 independent same-`(driver,kind)` nodes): `Ok` with a 200-entry
  ledger and **ONE** `apply_batch` of size 200 — the batching property holds at scale, no
  panic, no unbounded task spawn observed.

## Concern (Critical Review Policy) and proposal

- **Concern (business-outcome):** the batching/concurrency guarantees that protect the
  business case (Gmail N+1 -> 1, bounded blast radius so a wide frontier cannot exhaust rate
  limits / fds) are currently asserted only by `cfs-runtime`'s own unit tests and this
  throwaway. Once a real E4 driver lands, a regression in the coalescing key or the two-level
  semaphore could silently restore the N+1 pattern and only surface as a production rate-limit
  incident — exactly the failure cfs is meant to prevent.
- **Proposal:** in the E4 driver tickets, carry forward a lightweight "batch-size observed ==
  1 call of size N" assertion (the mock pattern used here) as an integration check against the
  first concrete driver, and surface the per-group batch size in the audit ledger / tracing
  span so operators can see coalescing actually happened in production. This keeps the
  N+1 -> 1 promise traceable end to end rather than only at the unit boundary.

## Notes
- No production code was modified. Throwaway crate removed after the run.
- No live credentials or network used anywhere — in-memory mocks only.
