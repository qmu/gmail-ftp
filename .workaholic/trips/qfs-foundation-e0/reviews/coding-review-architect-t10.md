# Architect Coding Review — t10 (Interpreter: apply plans with auto-batching + parallelism)

- Reviewer: Architect (Neutral / structural bridge)
- Commit: `a7031c7` on `work-20260622-230954`
- Scope: analytical review only (no cargo/test execution)
- Files: `crates/runtime/src/{interpreter,schedule,batch,driver,caps,outcome,error,lib}.rs`,
  `crates/runtime/tests/interpreter.rs`, `crates/runtime/Cargo.toml`, `ARCHITECTURE.md`,
  cross-checked against `crates/plan/src/{topo,node,plan}.rs` and `crates/plan/tests/purity_deps.rs`.

## Decision

**Approve with observations.**

No concurrency/safety defect was found. The scheduler's correctness rests on a single
load-bearing structural invariant that holds, the spine/purity story is intact, and the
determinism and irreversible/retry guarantees are enforced on every path I traced. The
observations below are throughput/forward-compat refinements, not blockers.

## Spine / purity — sound

- `qfs-runtime` depends on `qfs-plan` + `qfs-types` only (`Cargo.toml`), plus tokio/futures/
  async-trait/serde/tracing. **No `qfs-core` dependency** — the runtime → core inversion the
  t07 guidance warned against is avoided, and the runtime walks `qfs-plan` effect types
  exclusively.
- **tokio is genuinely confined.** `grep` confirms no `crates/*/Cargo.toml` other than
  `crates/runtime` references `qfs-runtime`, i.e. **nothing depends back up onto the runtime**.
  `qfs-plan`'s `purity_deps.rs` test walks `qfs-plan`'s *forward* resolve closure only; since
  the edge is `qfs-runtime → qfs-plan` (never the reverse), tokio can never enter qfs-plan's
  closure. The purity dep-closure test stays valid and green by construction.
- **Acyclic.** The new crate is a leaf consumer at the runtime layer; it adds no back-edge.
  `ARCHITECTURE.md`'s spine is preserved. (Observation O3 below: this is not yet *mechanically*
  asserted.)
- No vendor type crosses the `ApplyDriver` seam; batching keys on owned `DriverId` +
  `EffectKind` label (RFD §9). The `ApplyCx` carries no credentials; the ledger records metadata
  only (`outcome.rs`), confirmed by `ledger_json_is_stable_and_secret_free`.

## Scheduling correctness under parallelism — sound

The crux: **all `Frontier` mutation is single-threaded on the driver loop.** `run_group`
futures never touch the frontier; their results are folded back only after
`in_flight.next().await`, sequentially, in the one loop task. There is therefore no data race
on `indegree`/`dispatched`/`settled`/`tainted`.

- **No premature dispatch of a dependent before an ancestor's failure is observed.** A node is
  surfaced as `Ready::Run` only when its in-degree is 0 (`Frontier::ready`). In-degree reaches
  0 *only* via `relax_children`, which is called *only* from `complete` / `fail` / the
  `ready()` skip branch — i.e. after every parent has settled. So a child cannot become ready
  until all parents (including a failing one, whose `fail()` both taints and relaxes) have
  settled. At that point `tainting_parent` sees the tainted parent and emits `Ready::Skip`.
  The failed-node → transitive-dependents-skipped semantics of t09 are preserved exactly, and
  the diamond case (one parent fails, one succeeds → child skipped) is correct because the
  child only surfaces after *both* settle.
- **In-flight drain on failure is sound.** `fail()` taints but does not cancel outstanding
  futures; the loop keeps awaiting `in_flight` until it empties, folding each result. New
  dependents of the failed node surface as skips on subsequent `ready()` passes. Matches the
  ticket's "stop scheduling new dependents but drain in-flight."
- **Termination is well-guarded.** Break only when `is_done() && in_flight.is_empty()`, or when
  a pass made no progress with nothing in flight (defensive guard against a malformed plan).
  The `progressed` flag correctly distinguishes "another pass needed to surface skips" from
  "genuinely stuck," so there is no busy-spin and no premature break that would drop nodes.
  `assemble` then emits every settled node in stable topo order.
- **Two-level semaphore is deadlock-free.** Permits are clamped `>= 1` (`ConcurrencyLimits::new`),
  global-then-per-driver acquisition order is uniform across all groups, and permits are held
  for the driver-call lifetime then dropped. The per-driver semaphore map is created once before
  the loop and reused across iterations. The `independent_branches…`/`per_driver_cap…` tests
  confirm the caps bind and real overlap occurs.

## Batching correctness — sound

- Coalescing keys on `(DriverId, kind_label)` with `Call(proc)` folded into the label
  (`CALL:<proc>`), so distinct procs do **not** merge (`distinct_call_procs_do_not_coalesce`),
  while N same-kind rows collapse to one batch (`n_independent_…_coalesce_into_one_batch`,
  N+1 → 1). Grouping is over the whole materialized ready-set, not pairwise — the property the
  ticket flags as the hard part.
- **Safety of merging:** only nodes at the *same frontier* (all deps already settled) are ever
  coalesced, and the frontier only surfaces a node once all its ancestors are done. So two
  effects that must stay ordered (A→B) are never in the same group — they land in different
  frontiers. Independent same-key effects are, by definition of the DAG, order-insensitive, so
  merging them is semantically safe. Different sub-paths under the same `(driver,kind)` are the
  intended batch (e.g. N message-modifies); the runtime correctly defers any same-source subtree
  *collapse* (pushdown) to E3, batching only at the driver-call boundary.
- Result fan-out is by position with an explicit `EffectInput::id` carried alongside, and
  `normalise_len` defensively aligns a misbehaving driver's vector rather than panicking — good
  total-function hygiene consistent with the workspace's no-panic lint posture.

## Irreversible + retry safety — sound on every path

- The retry gate is `e.is_retryable() && !irreversible && !last` (`run_group`), with
  `irreversible` read per-leg from the owned `EffectInput`. `Remove` is inherently irreversible
  (`is_inherently_irreversible`), and declared-irreversible `Call`s carry the bit — both paths
  pin to their first outcome. `TimedOut.is_retryable()` is true but is still gated by
  `!irreversible`, so an irreversible leg that *times out* is **not** retried. Verified against
  `irreversible_leg_is_never_retried` and `remove_is_inherently_irreversible_and_not_retried`.
- **Timeout-as-whole-subset is conservative-correct.** On a batch timeout the runtime cannot know
  which legs landed, so it maps every leg of the in-flight subset to `TimedOut`; reversible legs
  may retry (at-least-once, acceptable for UPSERT-style idempotent effects), irreversible legs
  are pinned (never double-fired). This is the safe direction for the COMMIT story: it never
  silently re-issues an irreversible side effect, and the ledger records the timeout so a human/
  AI recovery (t12) can reconcile. The retry re-dispatches only the still-`pending` legs, so
  succeeded/terminal/irreversible legs are not re-sent — batching is preserved without
  re-applying landed work.
- Capability re-check happens *before* dispatch (`commit` step 2); a denial fails the node and
  taints it so dependents skip, with the driver never called (`ungranted_…_before_dispatch`,
  `capability_denied_node_skips_its_dependents`).

## Determinism — sound

`assemble` rebuilds the ledger by walking `qfs_plan::topo_order` (stable Kahn with NodeId-sorted
ready set) and emitting each node's settled entry, **discarding wall-clock completion order**.
Within-frontier dispatch is NodeId-sorted (BTreeMap in `coalesce` + `sort_by_key`). So the
`Outcome` ledger is identical regardless of interleaving — exactly what the t12 audit/AI story
needs. Durations are serialized as integer millis (float-free) for golden stability.

## Boundaries for t11/t12/t14 — clean deferrals

- **t11 (transactions/idempotency):** the per-leg ledger written before/after apply is the
  recovery substrate the ticket promises; `Outcome::applied_ids` already exposes the
  skip-on-rerun set. The cp = copy→verify→delete orchestration is correctly out of scope here.
- **t12 (audit ledger):** deterministic, serializable, secret-free `Outcome` is a direct feed.
- **t14 (pushdown):** batching-at-the-call-boundary leaves same-source subtree collapse entirely
  to E3 without contradicting it — the interpreter executes whatever leaves the plan presents.
- **E4 sync→async adapter gap:** keeping the async `ApplyDriver` in `qfs-runtime` (rather than in
  `qfs-driver`) is the right call — it is what lets `qfs-plan`/`qfs-driver` stay I/O-free while
  the runtime owns tokio. The "real E4 driver bridges its sync `PlanApplier` to `ApplyDriver`
  with a thin adapter" deferral is correctly the right place to land that.

## Observations (constructive, non-blocking)

- **O1 — Wide-frontier eager future materialization vs. the memory-backpressure claim.**
  In `commit` step 3 every coalesced group of the ready-set is `in_flight.push(run_group(...))`
  in one synchronous pass *before* any await; each `run_group` acquires its global/per-driver
  permit *inside* the future. The semaphores therefore bound concurrent *driver calls*
  (fds/rate-limit — the `peak_in_flight <= global` test confirms this), but they do **not**
  bound the number of *pending* `run_group` futures, each of which owns a cloned `BatchGroup`
  (its `RowBatch` inputs). For a frontier with thousands of distinct-key groups, all those
  future state-machines + input clones are resident at once. This is a throughput/memory
  refinement, not a correctness or fd bug, but it is in slight tension with the ticket's "a wide
  frontier cannot exhaust … memory / must not spawn unbounded tasks."
  *Proposal:* cap the number of groups admitted to `in_flight` per pass (e.g. a `ready`-set
  drain bounded by `limits.global * k`, re-pulling the frontier as groups complete), or acquire
  the global permit *before* constructing/pushing the future so pending memory tracks the cap.
  Either keeps the N+1→1 coalescing (which is computed on the full ready-set up front) while
  bounding resident work. Defer to E4 if E0 frontiers are known-shallow, but record the
  rationale.

- **O2 — `preview` re-derives skip propagation independently of `Frontier`.**
  `commit` uses `Frontier` for taint propagation; `preview` re-implements the same logic inline
  with a `tainted: Vec<NodeId>` and a `plan.deps` scan. The two are consistent today, but they
  are two sources of truth for the same t09 semantics and could drift (e.g. if capability-denial
  taint rules change). *Proposal:* have `preview` drive the same `Frontier` (marking would-run
  nodes `complete` and denied/failed nodes `fail`) so there is one skip-propagation
  implementation. Low priority — the current duplication is small and tested.

- **O3 — The acyclic/confinement guarantee is not yet mechanically asserted for `qfs-runtime`.**
  `dep_direction.rs` has no `qfs-runtime` case, so "nothing depends back up onto the runtime"
  and "tokio stays out of the spine crates" rest on review, not CI. *Proposal:* add a
  `dep_direction` assertion that no spine crate (`qfs-core`/`-plan`/`-driver`/`-types`/`-codec`/
  `-lang`/`-parser`) has `qfs-runtime` (or tokio) in its resolved closure — the mechanical
  counterpart of the purity test, locking the confinement in place for later tickets.

## Cross-artifact coherence

The implementation faithfully realizes the t09 DAG semantics (topo order + skip-dependents) and
the RFD §3/§6 COMMIT-as-sole-impure-stage contract, and it sits inside the `ARCHITECTURE.md`
boundaries without restructuring the workspace. Translation fidelity from ticket intent to code
is high; the structural seams for t11/t12/t14/E4 are present and correctly deferred.
