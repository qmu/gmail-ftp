---
created_at: 2026-06-22T21:46:50+09:00
author: a@qmu.jp
type: enhancement
layer: [Infrastructure]
effort:
commit_hash: bec8050
category: Added
depends_on: [20260622214650-t31-server-binding-ddl.md]
---

# Server: scheduler (JOB / cron)

## Overview

This ticket delivers the **JOB scheduler** for the `cfs` server: the runtime that
makes `CREATE JOB <name> EVERY <interval> DO <plan>` bindings actually fire on a
schedule (RFD §8, "Bindings = what causes a plan to run"; §3 frozen DDL keywords
`JOB`/`EVERY`/`DO`). A `JOB` is one of the four "what causes a plan to run" cases —
the schedule-driven one, alongside `ENDPOINT` (request) and `TRIGGER`/`WEBHOOK`
(event). The DDL that *defines* JOB rows lands in t31 (binding DDL → `INSERT INTO
/server/jobs`); this ticket builds the loop that reads those rows, evaluates each
JOB's `DO` plan on cadence, and applies it through the existing PREVIEW/COMMIT
interpreter (RFD §6).

Two things make a scheduler more than a `sleep` loop, and both come straight from
the RFD: (1) **durable `LAST_RUN()` state** so a JOB's `DO` plan can run *incremental*
queries (e.g. "messages since last run") — the stateful-watcher concern the RFD maps
to a Durable Object on Cloudflare (§8 deployment mapping); and (2) **idempotent,
recoverable execution** (§6 idempotency, §6 audit ledger) so a missed or retried run
does not double-apply effects. The scheduler stays a thin runtime: it never
*performs* I/O of its own — it constructs nothing, it only *causes* an already-built
`Plan` to be committed (purity invariant, §3).

## Scope

In scope:
- A `Scheduler` runtime that loads enabled `JOB` rows from `/server/jobs`, computes
  next-fire times from `EVERY <interval>` (and cron expressions), and dispatches due jobs.
- Durable per-JOB run state: `last_run_at`, `last_status`, `last_plan_hash`,
  `running` lease — backing the `LAST_RUN()` registry function inside JOB query scope.
- Missed-run policy (catch-up vs. skip-to-now vs. coalesce) selected per JOB.
- Idempotent dispatch: single-flight lease per JOB, run-id keyed audit ledger entries,
  reuse of `UPSERT`/`@version` semantics so retried runs are safe.
- A pluggable `Clock` + `JobStore` trait so the same loop runs as an EC2 daemon thread
  *and* compiles to the Cloudflare Cron-Trigger entrypoint (wasm).

Out of scope (deferred):
- The `CREATE JOB` parser/DDL and `/server/jobs` schema → **t31** (depends_on).
- `TRIGGER`/`WEBHOOK` event delivery and the Queues mapping → sibling E7 trigger ticket.
- `ENDPOINT` request routing → sibling E7 endpoint ticket.
- `POLICY` enforcement *engine* → sibling E7 policy ticket (this ticket only *threads*
  the handler's policy/capabilities into the commit call; it does not author the evaluator).
- The plan interpreter / PREVIEW / COMMIT itself → E2 (consumed here, not built).

## Key components

New crate module `server::scheduler` (binary-internal, no vendor types leak — owned DTOs).

- `pub struct JobBinding { name: String, schedule: Schedule, plan: PlanTemplate,
  policy: PolicyRef, missed: MissedPolicy, enabled: bool }` — owned DTO read from
  `/server/jobs`; `plan` is the parsed-but-unevaluated `DO` body (a `-> Plan` thunk,
  honoring the purity invariant).
- `pub enum Schedule { Every(Duration), Cron(CronExpr) }` with
  `fn next_after(&self, from: DateTime<Utc>) -> Option<DateTime<Utc>>`.
- `pub enum MissedPolicy { Skip, CatchUp { max: u32 }, Coalesce }` — what to do when the
  process was down across one or more due times.
- `pub trait JobStore { fn load_enabled(&self) -> Result<Vec<JobBinding>>;
  fn run_state(&self, job: &str) -> Result<RunState>;
  fn acquire_lease(&self, job: &str, run_id: Uuid, ttl: Duration) -> Result<Lease>;
  fn record_run(&self, job: &str, run: RunRecord) -> Result<()>; }` — backed by the
  audit ledger / `/server` store on EC2, by a Durable Object on Cloudflare.
- `pub struct RunState { last_run_at: Option<DateTime<Utc>>, last_status: RunStatus,
  last_plan_hash: Option<Hash> }`.
- `pub trait Clock { fn now(&self) -> DateTime<Utc>; }` — `SystemClock` for prod, a
  `MockClock` for golden/plan tests (no wall-clock flake, no live creds).
- `fn last_run(state: &RunState) -> Value` — the registry binding for `LAST_RUN()`,
  injected into the JOB's query evaluation scope (NULL → sentinel epoch on first run).
- `pub struct Scheduler<S: JobStore, C: Clock> { … }` with
  `async fn tick(&self) -> Vec<Dispatched>` (one evaluation pass; the daemon calls it
  in a loop, the Cron Trigger calls it once per fire) and
  `async fn dispatch(&self, job: &JobBinding, scheduled_for: DateTime<Utc>) -> RunRecord`.
- Reuses E2 `Interpreter::commit(plan, policy, capabilities)` — the scheduler is just a
  caller; capability gating and PREVIEW-in-CI come from existing machinery.

## Implementation steps

1. Define the owned DTOs (`JobBinding`, `Schedule`, `MissedPolicy`, `RunState`,
   `RunRecord`, `RunStatus`) in `server::scheduler`; no chrono/vendor types in public
   signatures beyond the project's standard `DateTime<Utc>` alias.
2. Implement `Schedule::next_after`: `Every` = anchor + n·interval; `Cron` = parse a
   restricted 5-field cron (validate at load, structured error like other parse errors).
3. Define `JobStore` + `Clock` traits; provide a `LedgerJobStore` over the `/server`
   store and a `MemJobStore`/`MockClock` for tests.
4. Implement `LAST_RUN()` as a registry function bound from `RunState`, scoped to JOB
   query evaluation only (not available in arbitrary statements).
5. Implement `Scheduler::tick`: load enabled jobs → for each, compute due set from
   `run_state.last_run_at` and `now` → apply `MissedPolicy` to fold/cap the due set.
6. Implement `dispatch`: generate `run_id`; `acquire_lease` (single-flight, TTL); inject
   `LAST_RUN()`; evaluate `DO` thunk → `Plan`; `commit` under the JOB's policy; on success
   `record_run` (advance `last_run_at`, store `last_plan_hash`); release lease.
7. Idempotency: key ledger entries by `(job, run_id)`; ensure a retried `dispatch` with the
   same `run_id` is a no-op if already committed; prefer `UPSERT`/`@version` in effects.
8. Daemon wiring: a `tokio` interval loop calling `tick()` with jitter + per-job timeout.
9. Cloudflare wiring: a `scheduled()` entrypoint that maps one Cron Trigger fire to one
   `tick()`, with `JobStore` backed by the Durable Object (no shared mutable global).
10. Structured logs + audit-ledger entry per fire (scheduled-for, run-id, status, counts);
    metrics for missed/coalesced/failed runs.

## Considerations

- **Hard part — exactly-once vs. at-least-once.** A cron fire on two replicas (or a
  retried Cron Trigger) must not double-commit. Resolve with the per-JOB lease
  (`acquire_lease`) plus run-id-keyed ledger entries; effects themselves stay idempotent
  (`UPSERT`, `@version`/ETag, RFD §6) so the worst case is a safe no-op, not duplication.
- **Hard part — `LAST_RUN()` advance ordering.** Advance `last_run_at` only *after* a
  successful commit, and store the `scheduled_for` boundary (not `now`) so an incremental
  query never skips a window on slow runs. Failed runs leave `last_run_at` unmoved so the
  next tick re-covers the window (at-least-once, idempotent).
- **Missed-run policy** is per-JOB and explicit: `Skip` (only newest window), `CatchUp{max}`
  (replay capped windows), `Coalesce` (one run covering the whole gap). Default = `Coalesce`
  to avoid thundering catch-up after downtime.
- **Least privilege / secrets** (RFD §10): each JOB commits under its handler `POLICY` and
  capability set; the scheduler never widens scope and never logs credentials or plan
  payloads — only counts/hashes go to the ledger.
- **Observability** (RFD §6): per-run timeout, bounded retries, circuit-breaker on
  repeated failure (auto-disable a flapping JOB with a ledger note), structured logs.
- **Portability** (RFD §8/§9): all wall-clock and persistence behind `Clock`/`JobStore`
  so the identical loop runs on EC2 and compiles to `wasm32` for Cron Triggers; no
  std-thread/global-state assumptions in the wasm path.
- **Coding standards**: scheduler is a *caller* of the interpreter — it constructs no
  effects and performs no service I/O directly (purity invariant); owned DTOs only.

## Acceptance criteria

- `cargo build`, `cargo build --target wasm32-unknown-unknown`, and `cargo clippy
  -- -D warnings` are green.
- `Schedule::next_after` golden tests: fixed `MockClock` inputs → expected next-fire
  times for representative `EVERY` durations and cron expressions; invalid cron → a
  structured parse error (not a panic).
- **Plan assertions (no live creds):** for a JOB whose `DO` body references `LAST_RUN()`,
  dispatching against a `MemJobStore` produces the expected `Plan` (PREVIEW), and
  `LAST_RUN()` resolves to the stored boundary (sentinel epoch on first run).
- Idempotency test: two concurrent `dispatch` calls with the same due time yield exactly
  one committed run (one acquires the lease; the other no-ops); a retried `dispatch` with
  the same `run_id` after success commits nothing further.
- Missed-run test: with `last_run_at` several intervals behind `now`, each `MissedPolicy`
  variant produces the documented due set (`Skip`=1, `CatchUp{max:n}`≤n, `Coalesce`=1).
- `last_run_at` advances only on commit success and to `scheduled_for`; a forced-failure
  dispatch leaves it unchanged and re-covers the window on the next `tick`.
- Every fire writes one audit-ledger entry (job, run-id, scheduled-for, status, counts);
  no secrets or plan payloads appear in logs (asserted by a log-scrub test).
