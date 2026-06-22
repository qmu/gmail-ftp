---
created_at: 2026-06-22T21:46:50+09:00
author: a@qmu.jp
type: enhancement
layer: [Infrastructure, Config]
effort:
commit_hash:
category:
depends_on: [20260622214650-t10-interpreter-batch-parallel.md]
---

# Audit ledger + observability

## Overview

This ticket implements the runtime durability and observability spine described in
RFD 0001 §6 (Runtime/interpreter — "Observability" and "Transactions" bullets) and §10
(Security — "full audit ledger; idempotent, recoverable effects"). The interpreter from
t10 evaluates a `Plan` (typed DAG of effects) and applies it at the edge. Here we make
that application **auditable and recoverable**: every applied effect is appended to an
append-only **applied-effect ledger** that records the *intended* plan node alongside the
*committed* result, so a partially-failed cross-source plan (e.g. `cp = copy→verify→delete`)
can be reconstructed and resumed. We also wrap every external leg with **per-leg timeouts,
bounded retries with backoff, and circuit breakers**, and emit **structured logs carrying a
trace id** that threads through plan → effect → external call. On the server (E7) this same
ledger *is* the action/event log: every fired plan (endpoint/trigger/job) lands in it.

This is pure runtime infrastructure — it adds **zero keywords** (closed-core grammar is
untouched) and introduces **no driver**; it observes and persists what the interpreter
already does.

## Scope

In scope:
- `Ledger` trait + append-only writer; intended-vs-committed effect records.
- Recovery read path: given a plan id, find the last consistent point and re-derive remaining effects.
- Per-leg execution policy: timeout, bounded retry w/ exponential backoff + jitter, circuit breaker per `(driver, endpoint)`.
- Structured logging (`tracing`) with a propagated `TraceId`/`PlanId`/`EffectId` span hierarchy.
- A local file/JSONL ledger backend (default) behind the trait.

Out of scope (deferred):
- Server bindings that *fire* plans (`ENDPOINT/TRIGGER/JOB`) and surface the ledger as `/server/events` — E7 server ticket.
- `POLICY` capability gating enforcement at fire time — E5/E8 security ticket (this ticket only *records* the policy id on each effect).
- D1/R2/KV-backed ledger for the Workers deployment — E7 deployment ticket (trait makes this a drop-in).
- The interpreter's batch/parallel scheduling itself — t10 (this ticket consumes its `Plan` + per-effect hooks).

## Key components

New crate-internal module `runtime::observe` and `runtime::ledger`.

- `struct TraceId(Ulid)`, `struct PlanId(Ulid)`, `struct EffectId(Ulid)` — owned id types; no vendor ids leak.
- `enum EffectOutcome { Committed { returning: Option<RowBatch>, version: Option<String> }, Failed { error: EffectError }, Skipped }` — owned DTO; never holds a vendor SDK type.
- `struct AppliedEffect { plan_id, effect_id, trace_id, intended: EffectNode, outcome: EffectOutcome, irreversible: bool, policy_id: Option<String>, started_at, finished_at }` — the ledger record (intended plan node vs committed effect).
- `trait Ledger { fn append(&self, rec: &AppliedEffect) -> Result<(), LedgerError>; fn read_plan(&self, id: PlanId) -> Result<Vec<AppliedEffect>, LedgerError>; }` — small consumer-side trait (cf. RFD §9 "consumer-side small traits"). Append must be durable (fsync/flush) before the effect is considered acknowledged.
- `struct JsonlLedger { path }` — default append-only `serde_json`-per-line backend; one file, monotonic offsets.
- `struct LegPolicy { timeout: Duration, max_retries: u32, backoff: Backoff, breaker: BreakerConfig }` and `async fn run_leg<F>(policy, ctx, f) -> Result<T, EffectError>` — wraps a single external call.
- `struct CircuitBreaker` keyed by `(driver, endpoint)`; states `Closed/Open/HalfOpen`; trips on consecutive failures / error-rate window.
- `enum EffectError { Timeout, RetriesExhausted, CircuitOpen, Driver(DriverError), .. }` — retry classifier decides which are retryable (timeouts/5xx/throttle yes; 4xx/validation no).
- `struct Recovery` — `fn resume(plan: &Plan, prior: &[AppliedEffect]) -> Plan`: diffs intended DAG against committed ledger records, returns the residual sub-plan (idempotent re-apply of incomplete effects; honors `irreversible` by refusing silent replay of an effect whose commit is ambiguous).
- Interpreter integration: t10's apply loop calls `ledger.append` for *intended* (pre) then *outcome* (post) per effect, all inside a `tracing` span carrying the ids.

Purity invariant preserved: pure query/transform stages are never ledgered; only `COMMIT` of effect nodes touches the ledger. `CALL driver.x` plan nodes are recorded the same as CRUD effects.

## Implementation steps

1. Add deps: `tracing`, `tracing-subscriber` (env-filter + json formatter), `ulid`, `serde_json`, `backon` (or hand-rolled backoff). Gate the json subscriber so CLI default stays human-readable.
2. Define id newtypes and `AppliedEffect`/`EffectOutcome`/`EffectError` with `Serialize/Deserialize`.
3. Define the `Ledger` trait + `JsonlLedger` (append with `OpenOptions::append`, flush, durable line framing); unit-test round-trip `append`→`read_plan`.
4. Implement `LegPolicy::run_leg`: timeout via `tokio::time::timeout`, retry loop using the classifier, jittered exponential backoff, breaker check before each attempt and state update after.
5. Implement `CircuitBreaker` with per-key state map (`DashMap`/`Mutex<HashMap>`); add tests for trip/open→half-open→close transitions.
6. Wire `tracing` spans: root span per plan (`plan_id`, `trace_id`), child span per effect (`effect_id`, `driver`, `endpoint`); ensure ids appear in every log line.
7. Integrate with t10 interpreter: emit intended record before apply, outcome record after; wrap each external leg in `run_leg`.
8. Implement `Recovery::resume`: build committed-set from ledger, diff against plan DAG, return residual `Plan`; cover the `cp` copy→verify→delete partial-failure case.
9. Add a `--trace`/`-json` log toggle and a `cfs ledger show <plan-id>` debug subcommand (read path only).
10. Golden tests: serialize a fixed plan's ledger to a golden JSONL; assert byte-stable (modulo redacted timestamps/ids).

## Considerations

- **Least-privilege & secrets**: ledger records and structured logs MUST NOT contain credentials, tokens, or full payloads — record the *path/effect shape*, counts, `@version`/ETag, and a content hash, not bodies. Add a redaction pass; assert no secret material in the golden ledger. Record the `policy_id` that authorized the effect for later audit (enforcement is E5/E8).
- **Idempotency / recovery**: recovery leans on `UPSERT`/`@version` (RFD §6). The genuinely hard part is an effect whose **commit is ambiguous** (network drop after the side effect landed): mark such effects `Indeterminate` in the ledger and require `UPSERT`-style re-apply or operator confirmation before replaying an `irreversible` node — never blind-retry `CALL mail.send` / deletes.
- **Cross-source ordering**: a single-source plan is a real transaction; cross-source is orchestrated best-effort. The ledger must capture enough ordering (monotonic per-plan sequence + offsets) to reconstruct the exact prefix that committed.
- **Observability**: one trace id per plan execution, propagated into driver HTTP calls (header) so external traces correlate. Breaker state changes and retry exhaustion are themselves logged events.
- **Append durability vs. throughput**: fsync-per-append is correct but slow under batch/parallel apply; allow a batched-flush mode with a documented durability window, default to safe.
- **Directory structure / coding standards**: keep all of this under `runtime::{ledger,observe}`; the `Ledger`/breaker types are owned DTOs with no vendor leak (RFD §9); the trait stays small and consumer-side so a D1/R2 backend drops in for Workers without touching the interpreter.

## Acceptance criteria

- `cargo build` and `cargo clippy --all-targets -- -D warnings` are green; `cargo test` passes with no live credentials.
- Unit: `JsonlLedger` append→`read_plan` round-trips; records carry intended `EffectNode` + `EffectOutcome`.
- Unit: retry classifier retries timeouts/throttle, does not retry validation/4xx; backoff is bounded by `max_retries`.
- Unit: circuit breaker transitions `Closed→Open→HalfOpen→Closed` under a scripted failure/success sequence.
- **Plan assertion**: applying a fixture plan with a forced mid-DAG failure yields a ledger whose committed prefix matches the intended prefix, and `Recovery::resume` returns exactly the residual effects (and refuses to silently replay an `irreversible`/`Indeterminate` node).
- **Golden test**: the serialized ledger for a fixed plan is byte-stable after redacting timestamps/ids; the golden contains **no secret material**.
- Every emitted log line for an applied effect carries `trace_id`, `plan_id`, and `effect_id`.
- Pure query/transform-only statements (no effects) produce **zero** ledger appends.
