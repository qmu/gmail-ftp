# Design vt33 — Server JOB scheduler / cron

Author: Constructor
Status: under-review
Reviewed-by: (pending Architect analytical review, Planner E2E)

## Content

### Scope & inventory (ticket t33)

The JOB scheduler makes `CREATE JOB <name> EVERY <interval> DO <plan>` bindings
(stored in `/server/jobs` by t31) fire on cadence. It is a **thin runtime**: it
constructs no effects and performs no service I/O — it only *causes* an already-built
`Plan` to commit through an INJECTED committer (purity invariant, RFD §3/§6).

### Topology decision — a dedicated leaf crate `qfs-cron`

Following the established E7 leaf-binding pattern (`qfs-http` is the precedent): the
scheduler is a **new leaf crate `qfs-cron`**, NOT a module inside `qfs-server`.

Rationale, weighed against every confinement guard:
- The scheduler must consume `qfs-exec` (`build_plan`: eval a DO `Statement` →
  `Plan`) — exactly the role `qfs-http` plays. `qfs-server` must stay OFF `qfs-exec`
  and runtime-free (CO-t30-1 / the t30/t32 rule), so the scheduler cannot live in
  `qfs-server`. `qfs-cmd` must stay off `qfs-exec`/`qfs-http`, so it cannot live there
  either.
- `qfs-cron` consumes `qfs-server` (the `Binding`/`ServerState`/`JobDef` registry +
  reconcile seam) AND `qfs-exec` (the evaluator) AND `qfs-core` (Plan/commit/Value/
  StatementSpec). It is the one crate that legitimately binds all three for the JOB
  cause — the same justification that made `qfs-http` a new leaf.
- `qfs-cron` is a LEAF: only the terminal `qfs` binary depends on it (the serve
  composition root). That keeps its (feature-gated) tokio daemon loop dead-ended in
  the binary — the precondition the t28 runtime-leaf exemption relies on.
- `qfs-cron` does NOT depend on `qfs-runtime`. The REAL commit path (the runtime
  `Interpreter`/applier with live drivers) is provided by the composition root via an
  injected `Committer` trait — the scheduler only calls it. So tokio-the-COMMIT-
  interpreter never enters `qfs-cron`; its only tokio is the native daemon loop (HTTP/
  cron I/O domain), exactly mirroring `qfs-http`.

Guard impact (all must stay green):
- `dep_direction::exec_is_confined_above_the_spine_and_off_the_runtime`: extend the
  `allowed_exec_consumers` allowlist with `qfs-cron` (the 4th admitted leaf consumer of
  the read/eval executor, same role as `qfs-http`). Add the documented rationale.
- A new guard `cron_binding_is_a_leaf_serve_consumer` (twin of
  `http_binding_is_a_leaf_serve_consumer`): asserts `qfs-cron` depends on
  `qfs-server` + `qfs-exec`, is a leaf (only `qfs` depends on it), and does NOT depend
  on `qfs-runtime`.
- `binary_is_the_thin_entrypoint_plus_the_t28_shell_composition_root`: extend the
  binary allowlist with `qfs-cron`.
- `qfs-cmd` dep_direction guards untouched (qfs-cmd never gains a qfs-cron edge — the
  scheduler composition lives in the binary, like serve).

### Pure core vs native daemon split (t25 optional-runtime feature pattern)

`qfs-cron` mirrors `qfs-driver-slack`'s split:
- **`default = ["native"]`**; **`native = ["dep:tokio"]`**. tokio is `optional`.
- The PURE scheduler core (Schedule math, due-set/missed-policy folding, dispatch
  orchestration over the `Clock` + `JobStore` + `Committer` traits, the `LAST_RUN()`
  AST rewrite, cron parse/validate) has **ZERO tokio / std::thread / global state** and
  builds for `wasm32-unknown-unknown` with `--no-default-features`.
- The NATIVE daemon (`daemon` module, `#[cfg(feature = "native")]`): a tokio interval
  loop calling `tick()` with jitter + per-job timeout. Lives behind the feature; the CF
  Cron `scheduled()` entrypoint shape maps one fire → one `tick()` (a pure call). The
  DO-backed `JobStore` for CF Durable Objects is a parked wiring detail; the wasm build
  of the pure core must pass (the acceptance gate).

### Time type & run-id dep decisions (avoid uncached heavy deps)

- **NO chrono, NO uuid.** Per the recurring trip dependency constraint and the ticket
  directive, instants are the project's standard epoch type: `i64` epoch **seconds**
  (matching `JobDef.last_run: Option<i64>` and `Value::Timestamp(i64)` /
  `ColumnType::Timestamp`). A small alias `type Instant = i64;` and `type Seconds = i64;`
  (a `Duration`-as-project-type) document intent without a vendor type. `DateTime<Utc>`
  in the ticket signatures maps to this `Instant`.
- **run_id is DETERMINISTIC**: `run_id = stable_key(job_name, scheduled_for)` — a hash
  of `"<job>\0<scheduled_for>"`. This is BETTER for idempotency than a random UUID (a
  retried fire for the same `scheduled_for` yields the same run_id → ledger dedup) AND
  avoids the uuid dep + any randomness (no Math.random / SystemTime in the pure path).
  The hash is the dependency-free SHA-256 already proven wasm-clean in the tree
  (`qfs-driver-objstore::sha256`); to avoid a cross-leaf dep we vendor the ~80-line FIPS
  180-4 routine into a private `hash` module (the same recorded engineering choice the
  objstore crate made — sha2/ring not cached, must build wasm32). run_id is its lower
  hex, prefixed (`run-<hex16>`), an owned `String`.

### Components (all in `qfs-cron`)

- `Schedule::{Every(Seconds), Cron(CronExpr)}` with
  `next_after(from: Instant) -> Option<Instant>`. `Every` = anchor + n·interval (first
  boundary strictly after `from`). `Cron` = a restricted 5-field cron (min hour dom mon
  dow) parsed+validated at LOAD time into structured `CronExpr` (`*`, `n`, `a,b`, `a-b`,
  `*/step`); a parse failure is a structured `ScheduleError`, never a panic.
- `MissedPolicy::{Skip, CatchUp{max:u32}, Coalesce}` (default `Coalesce`). `due_set(last,
  now)` enumerates boundaries in `(last, now]`, then folds: `Skip`=newest only,
  `CatchUp{max}`=oldest `≤max`, `Coalesce`=1 (the newest boundary, one run covering the
  gap).
- `Clock` trait (`now() -> Instant`); `SystemClock` (`#[cfg(feature="native")]`, wall
  clock via `SystemTime`) and `MockClock` (a `Cell<Instant>` for golden/plan tests — no
  wall-clock flake, wasm-safe).
- `JobStore` trait: `load_enabled() -> Vec<JobBinding>`, `run_state(job) -> RunState`,
  `acquire_lease(job, run_id, ttl) -> Lease` (single-flight), `record_run(job, RunRecord)`,
  plus `is_committed(job, run_id) -> bool` (idempotency check). `MemJobStore` (tests,
  an in-process `Mutex`-guarded map). `LedgerJobStore` over the `/server` store lives in
  the binary composition root (it touches `ServerState`/runtime); the pure core only
  knows the trait.
- `JobBinding { name, schedule, plan: PlanSpec (the t31 canonical body, rehydrated via
  from_canonical — NO re-parse), policy: PolicyRef, missed: MissedPolicy, enabled }`.
  Owned DTO. `plan` is rehydrated from `JobDef.plan` (the canonical StatementSpec text).
- `RunState { last_run_at: Option<Instant>, last_status: RunStatus, last_plan_hash:
  Option<String> }`. `RunStatus::{Never, Success, Failed, Disabled}`.
- `LAST_RUN()` binding: a pure AST rewrite (`bind_last_run(&mut Statement, boundary)`),
  the injection-safe twin of t32 `bind_params`. It replaces every
  `Expr::Fn(FnRef{name:"LAST_RUN", args:[]})` with `Expr::Lit(Literal::Int(boundary))`
  (epoch seconds; sentinel epoch `0` on first run / NULL last_run_at). Scoped to JOB
  query evaluation only by construction — it runs ONLY at dispatch on the JOB's DO body,
  never registered into the global stdlib namespace.
- `Scheduler<S: JobStore, C: Clock>` with the injected `Committer`:
  - `tick(&self) -> Vec<Dispatched>`: load enabled → per job compute due set from
    `run_state.last_run_at` + `now` → apply MissedPolicy → dispatch each due boundary.
  - `dispatch(&self, job, scheduled_for) -> RunRecord`: deterministic run_id →
    `is_committed` short-circuit (retried run_id after success = no-op) → `acquire_lease`
    single-flight TTL (a concurrent dispatch loses the lease → no-op) → `bind_last_run`
    on the rehydrated DO body → `Committer::commit(stmt, policy)` (the composition root
    builds the Plan via qfs-exec build_plan and runs the REAL applier) → on success
    `record_run` advancing `last_run_at` to **`scheduled_for` (NOT now)** + store
    `last_plan_hash` → release lease. On failure: leave `last_run_at` unmoved (next tick
    re-covers), record Failed, bump the failure counter.
- `Committer` trait: `commit(&self, stmt: &Statement, policy: &PolicyRef) -> Result<
  CommitOutcome>` where `CommitOutcome { plan_hash: String, affected: u64 }`. The pure
  core calls it; the binary provides the real one (build_plan + runtime applier). A
  `PreviewCommitter`/`RecordingCommitter` test double does the build_plan + RecordingApplier
  path (no live creds) for plan-assertion + idempotency tests.
- Idempotency: ledger entries keyed by `(job, run_id)`; `is_committed` makes a retried
  dispatch a no-op; the lease makes two concurrent dispatch for the same due time → ONE
  commits. Effects prefer UPSERT/@version (the DO body authoring concern; the scheduler
  threads the policy, never widens scope).
- Circuit-breaker: `RunState`/store tracks consecutive failures; after `MAX_FAILURES`
  (default 5) the JOB is auto-disabled with a ledger note (status `Disabled`); `tick`
  skips disabled jobs.
- Audit ledger per fire: `RunRecord { job, run_id, scheduled_for, status, applied_count,
  failure_note }` — counts/hashes only, NO secrets, NO plan payloads. A log-scrub test
  asserts the structured log line + RunRecord Debug carries no DO source / token text.

### Quality strategy (internal QA — Constructor domain)

Tests (in `qfs-cron`, `MockClock` + `MemJobStore` + `RecordingCommitter`, no live creds):
- `next_after` goldens: `Every` boundaries from a fixed anchor; cron `*/15 * * * *`,
  `0 */2 * * *`, `30 9 * * 1-5`; invalid cron (`60 * * * *`, `* * *`, `bad`) → structured
  `ScheduleError` (not panic).
- `LAST_RUN()` rewrite: a DO body `... WHERE ts > LAST_RUN()` → PREVIEW Plan resolves the
  literal to the stored boundary; sentinel `0` on first run; the rewritten plan is
  structurally a literal-leaf (injection-safe, mirrors the t32 golden).
- Idempotency: two concurrent dispatch same due → exactly one commits (lease); retried
  run_id after success → no-op.
- Missed-run: last_run several intervals behind now → `Skip`=1, `CatchUp{max:n}`≤n,
  `Coalesce`=1 due boundaries.
- `last_run_at` advances ONLY on commit success and to `scheduled_for`; a forced-failure
  committer leaves it unchanged + re-covers the window next tick.
- One audit RunRecord per fire; circuit-breaker auto-disables after N failures.
- log-scrub: no secrets / DO payloads in the RunRecord / log projection.
- Gates: `cargo build` (native), `cargo build --target wasm32-unknown-unknown
  --no-default-features` (pure core), `cargo clippy --workspace --all-targets -D
  warnings`, `cargo test --workspace` (was 990), `cargo fmt`.

### Delivery plan

1. Create `qfs-cron` crate (Cargo.toml: native/default feature split; deps qfs-server,
   qfs-exec, qfs-core, qfs-parser, optional tokio). Register in workspace `members` (it
   is `crates/*` — auto-member).
2. Pure modules: `hash` (vendored sha256), `schedule` (Schedule/CronExpr/next_after),
   `policy` (MissedPolicy/due_set), `clock` (Clock/MockClock; SystemClock native),
   `store` (JobStore/RunState/RunRecord/Lease/MemJobStore), `lastrun` (bind_last_run),
   `commit` (Committer/CommitOutcome/RecordingCommitter), `scheduler` (Scheduler/tick/
   dispatch/circuit-breaker), `binding` (CronBinding impl qfs_server::Binding kind Cron).
3. Native `daemon` module (`#[cfg(feature="native")]`): tokio interval loop + jitter +
   per-job timeout; a `scheduled()`-shape note for CF (parked DO JobStore).
4. Binary wiring: extend `crates/qfs/src/serve.rs` to register `CronBinding` alongside
   `HttpBinding`; provide the real `Committer` + `LedgerJobStore`. (Minimal — keep serve
   green; the real-applier wiring is threaded but the DO-backed store is parked.)
5. Extend the dep_direction guards (allowlists + new leaf guard). Run all gates, fix,
   fmt.

### Risk assessment

- **Disk ~98%**: NO new external dep (no chrono/uuid/cron crate); tokio is already in
  the cache (workspace dep). The vendored sha256 adds ~80 LoC, zero deps. Lowest disk
  risk path.
- **wasm purity**: the pure core must not transitively pull tokio. The feature split
  (`--no-default-features`) is the load-bearing fence (the t25/slack lesson: absence of
  `native`, not presence of a marker, is what excludes tokio).
- **Guard regression**: extending two allowlists + one new guard is the only spine
  surface change; mirrors the t32 qfs-http precedent exactly, so the topology is already
  blessed.
- **Real-commit wiring depth**: the ticket says route real-driver effects through the
  runtime interpreter at the composition root. To keep serve green and scope bounded,
  the binary wires the `Committer` against the runtime applier where a registry is
  available; the DO-backed JobStore + live-driver cron firing E2E is a parked wiring
  detail (carry-over), while the PURE core + PREVIEW path is fully tested. Recorded as
  an assumption.

## Review Notes

(pending)
