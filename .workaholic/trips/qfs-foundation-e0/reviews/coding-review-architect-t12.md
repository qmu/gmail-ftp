# Coding Review (Architect) — t12 Audit ledger + observability

**Author**: Architect (Neutral)
**Status**: under-review
**Reviewed commit**: `f06d19d` on `work-20260622-230954`
**Ticket**: `20260622214650-t12-audit-ledger-and-observability`
**Mode**: analytical review only (no cargo/test execution)

**Files read**: `crates/runtime/src/{observe,txn,error}.rs`,
`crates/txn/src/{ledger,outcome,saga,report,version,leg,tests}.rs`,
`crates/runtime/tests/txn_commit.rs`, `ARCHITECTURE.md`, and the prior t11 `txn.rs`
(commit `8b76989`) for carry-over comparison.

---

## Decision

**Approve with minor suggestions.**

Both routed t11 safety carry-overs are genuinely closed on the live integration path
(`Interpreter::commit_txn`). The has_intent → `Indeterminate` reconcile shuts the
crash-between-intent-and-apply double-apply hole; the `Conflict{version}` threading replaces
the prior terminal-text inference and carries the real world version, not the expected token.
One latent inconsistency in the **pure** `SagaExecutor::run_acid` executor (it does not treat
`Indeterminate` as hard) is not on any reachable path today but should be tightened for
defense-in-depth — recorded as a carry-over, not a blocker.

---

## 1. has_intent reconcile correctness (the safety fix)

**Verdict: the double-apply hole is closed on the live path.**

The gate in `apply_txn_leg` (`txn.rs:159-179`) is correctly ordered and conservative:

1. `applied(&key).is_some()` → `AlreadyApplied` (sealed = no-op). Checked first, so a fully
   recovered leg is never re-touched.
2. `has_intent(&key) && !is_replay_safe()` → `LegOutcome::Indeterminate { key }`, **before**
   the capability re-check, **before** `record_intent`, and **before** the driver is touched.
   The non-idempotent leg is never re-dispatched. The test
   `crash_between_intent_and_apply_is_indeterminate_for_insert` proves `mock.applied_ids()` is
   empty — no silent replay.

This is the genuine fix for the t11 hole: a crash after `record_intent` but before
`mark_applied` leaves an unsealed intent; on resume the prior code had no way to distinguish
"never started" from "may have landed" and would have re-applied. Now the unsealed-intent
window is detected and, for a non-replay-safe leg, refused.

**Replay-safe classification is correct.** `EffectDescriptor::is_replay_safe`
(`outcome.rs:50-52`) = `Upsert || precondition.is_conditional()`. The reasoning holds:
- `UPSERT` is create-or-update — a second apply converges to the same world state.
- A conditional write (`If-Version`/`If-Match`) is self-guarding: a stale re-apply surfaces as
  a `Conflict`, never a silent double-apply. The `crash_window_conditional_write_is_replay_safe`
  test confirms a guarded `Update` is re-applied (not flagged Indeterminate).
- Unconditional `Insert`/`Remove`/`Call(mail.send)` are correctly **not** replay-safe — a blind
  retry could duplicate a row, double-delete, or fire a side effect twice. `replay_safe_classifies_idempotency_for_reconcile`
  covers all four cases.

**Surfacing `Indeterminate` (hard stop, no auto-replay) is the right conservative behavior.**
`is_hard` (`txn.rs:269-274`) includes `Indeterminate`, so the strategy walk stops, `failure_at`
is set, downstream legs are `skipped`, and on the ACID path `rolled_back` is flagged. An
ambiguous-commit boundary that may have already mutated the world must not be crossed silently;
hard-stopping for `UPSERT`-style re-apply or operator confirmation is exactly RFD §6/§10
apply-once. Choosing this over silent re-application is the safe direction.

**The t11 doc is now accurate (no overclaim).** The `ledger.rs` module doc (lines 9-19) states
the reconcile guard is exercised "within a single process / test, not yet across a real OS
crash" because the in-memory ledger is process-local, and that the durable fsync-intent-before-apply
sink is E8. That is an honest scoping: the *mechanism* (intent-before-apply + has_intent gate +
Indeterminate) is real and tested in-process; only the *durable substrate* that would make it
survive a real crash is deferred. The doc no longer claims crash-durability it does not have.

## 2. Conflict{version} threading

**Verdict: the real world version is now carried; the boundary split is clean.**

Prior t11 (`8b76989:txn.rs:165-178`) inferred a conflict by string-matching the terminal
reason (`reason.contains("conflict") || "precondition" || "412"`) and then surfaced
`precondition.if_match_header()` — the **expected** token — as the world version, with an
inline admission "The world's version is not carried by the runtime error DTO at E0". That was
both brittle (text inference) and wrong-coordinate (expected, not actual).

t12 replaces this with a typed `EffectError::Conflict { version: String }` (`error.rs:50-60`).
The driver returns the version the world **actually** holds; `map_effect_error`
(`txn.rs:228-239`) threads it straight into `LegOutcome::Conflict(Version::new(version))` — no
text inference, no expected-token guessing. `optimistic_conflict_surfaces_typed` asserts the
surfaced version is `v2-world` (the driver's world coordinate), distinct from the precondition's
expected `v1`. This is the saga's correct re-read coordinate (`saga.rs:206 rebase_precondition`
re-bases on the world version), so there is no lost update.

**The opaque-String / Version split is clean and non-leaky.** At the runtime error boundary the
world coordinate is an opaque `String` (`EffectError::Conflict { version: String }`) — the
runtime never parses it. At the txn boundary it is wrapped in `qfs_txn::Version` (an owned
newtype, equality-comparable only, never parsed — `version.rs:9-31`). No vendor SDK type
crosses either boundary; the conversion is a single `Version::new(version)`. The doc comments on
both sides (`error.rs:50-60`, `txn.rs:218-223`) correctly state the world version comes from the
driver, not the expected token.

**Good defensive nuance**: a `Conflict` on an *unconditional* write (no precondition to
reconcile against — a driver-contract anomaly) maps to a *terminal* `Failed` that still
preserves the world version in its reason, rather than a typed `Conflict` (`txn.rs:232-238`).
`conflict_on_unconditional_write_is_terminal` covers it. This avoids surfacing a typed conflict
that has nothing to reconcile against.

## 3. Observability

**Verdict: deterministic, metadata-only, and the global-subscriber test is sound (with a
documented caveat).**

**Deterministic, no wall-clock/RNG.** `TraceId::mint` (`observe.rs:31-37`) is
`format!("t:{plan_id}:{seq:08x}")` where `seq` is a process-monotonic `AtomicU64`. No
`SystemTime`, no RNG, no ULID-now. Audit output is reproducible for golden tests modulo the
monotonic sequence (which is per-process-ordered, not wall-clock). The doc is explicit that real
ULID minting is the E8 ticket. The id is owned text (no vendor trace handle leaks).

**Spans/events carry only metadata.** Root span (`txn.rs:75-80`): `trace_id`, `plan_id`,
`strategy.code()`. Per-leg span (`txn.rs:150-156`): `effect.id` (index), driver, path label,
kind label. Per-leg event (`txn.rs:111-118`): id, driver, kind label, `irreversible`, outcome
code. The Indeterminate warn (`txn.rs:173-177`): the `EffectKey` (a hash handle, not a payload)
and kind label. None of these renders a row value, credential, or version literal. The
`effect.path` is a VFS path label (driver/path identity), which is shape, not secret.

**Global-subscriber test approach is sound but carries a known global-state caveat.**
`install_capture` (`txn_commit.rs:594-603`) uses a `OnceLock` + `set_global_default` once, with
a best-effort fallback if another harness already set a global. This is the correct way to make
the interpreter's `info_span!`/`info!` callsites *not* statically short-circuited (the
`max_level_hint` → `TRACE` only takes effect through a global subscriber). The single
observability test reads back only the lines containing its unique `plan-obs-unique` plan id,
so it is robust to other tests' spans interleaving in the shared capture. This is a reasonable
trade-off and not a flaky landmine *as written* — but see the minor suggestion below; a global
default is process-wide shared mutable state and any *future* test that also wants to assert on
captured tracing output would have to coordinate through the same `CAPTURE`.

## 4. Secret-free guarantee

**Verdict: genuinely secret-free under the payload-bait tests; no observed leak path.**

The bait is real: `secret_bearing_node` (`txn_commit.rs:529-544`) puts `PASSWORD-12345` in a
row value, and the mixed-plan test also bait-checks `super-secret-token`. Two independent
assertions hold:
- The serialized `RecoveryReport` (`mixed_plan_audit_is_ordered_and_secret_free`) contains
  neither bait string, but does carry the secret-free shape (key `k:plan-mix:0:`, kind
  `"insert"`, target path).
- The captured tracing lines (`observability_spans_carry_ids_and_are_secret_free`) contain no
  `PASSWORD-12345`.

Structurally this is guaranteed at the descriptor boundary: `EffectDescriptor`
(`outcome.rs:18-33`) records `arg_rows: usize` (a *count*) instead of the `RowBatch`, and the
report/ledger only ever serialize the descriptor + receipt + outcome — none of which holds a
payload. The `EffectReceipt` carries `affected: u64` and an optional `Version` (an opaque
coordinate, redaction the driver is responsible for, not a payload). I traced every field that
reaches a span, event, or ledger entry and found no path from a row `Value` or credential into
serialized audit state. The qfs-txn `recovery_report_json_is_secret_free_and_stable` test
adds a third bait (`Int(7)` payload not present in JSON).

## 5. Scope honesty (E8 deferral)

**Verdict: honest, and the `AuditLedger` seam is sufficient for the E8 file backend to drop in.**

Deferred to E8 (per ticket Scope-out and `ledger.rs:21-27`): the durable JSONL/file sink (with
fsync-intent-before-apply), the circuit breaker, the per-leg retry/backoff policy, and the
`qfs ledger show` CLI. The ledger doc states this plainly and ties the durability gap to a
concrete limitation (in-memory = process-local). This is honest deferral, not hand-waving.

The seam is genuinely swappable: `AuditLedger` (`ledger.rs:41-59`) is a small consumer-side
trait with four methods (`record_intent`/`mark_applied`/`applied`/`has_intent`), `Send + Sync`,
taking `&self` with interior mutability left to the impl. A JSONL/file backend implements the
same four methods with append-and-fsync semantics; nothing in the runtime or saga executor
depends on `InMemoryLedger` concretely — `commit_txn` takes `&dyn AuditLedger`. The descriptor
is already redacted at its boundary, so the durable sink inherits the secret-free guarantee for
free. No seam change is needed for E8.

**Note**: the ticket's `EffectOutcome { Committed/Failed/Skipped }`,
`AppliedEffect { started_at, finished_at, policy_id }`, `LegPolicy`, `CircuitBreaker`, and
`Recovery::resume` are described as t12 key components but are realized here as the
`EffectDescriptor`/`EffectReceipt`/`LegRecord`/`RecoveryReport` family minus the timing,
policy_id, breaker, and explicit `Recovery::resume` diff. The deferral of breaker/retry/policy
to E8 is consistent and stated; the `policy_id` field and wall-clock `started_at/finished_at`
are simply absent (and correctly so — wall-clock would break the determinism story). This is
acceptable for E0 but the E8 ticket should explicitly carry the `policy_id` audit field and the
`Recovery::resume` residual-sub-plan diff, since the ticket lists them as the audit substrate.

## 6. Determinism (audit / RecoveryReport ordering)

**Verdict: deterministic, preserved from t10 topo order.**

`commit_txn` walks `qfs_plan::topo_order(plan)` (`txn.rs:68`), the same stable topological order
the batched commit's `assemble` uses, so the `RecoveryReport.legs` are in plan order regardless
of wall-clock interleaving (`mixed_plan_audit_is_ordered_and_secret_free` asserts
`[0,1,2]`). The `EffectKey` is deterministic (FNV-1a over canonical bytes, golden-tested,
reorder-stable). The `TraceId` is monotonic without wall-clock. The whole audit/RecoveryReport
projection is therefore reproducible for the AI/audit story.

---

## Concerns and proposals

### Concern 1 (minor, latent — carry-over): `run_acid` does not treat `Indeterminate` as hard

In `SagaExecutor::run_acid` (`saga.rs:191`), `hard_failure` is
`matches!(outcome, LegOutcome::Failed(_) | LegOutcome::Conflict(_))` — it omits
`Indeterminate`, whereas `run_saga` (`saga.rs:143`) and the runtime's own `is_hard`
(`txn.rs:269-274`) both include it. If a future `LegApplier` ever surfaced `Indeterminate`
*inside* `run_acid`, the ACID walk would continue applying subsequent legs past an
ambiguous-commit boundary and would not set `failure_at`/`rolled_back` — a silent
gap in the same apply-once invariant t12 is closing.

This is **not reachable today**: the live runtime path (`commit_txn`) uses its own inline loop
(correct), `run_acid`/`run_saga` are called only from qfs-txn's own tests, and no in-tree
`LegApplier::apply` returns `Indeterminate` (the apply seam doc at `leg.rs:96-102` lists
Applied/Conflict/Failed only). So it is a dormant inconsistency, not a current double-apply
hole — hence "minor / observation", not "request revision".

**Proposal**: add `| LegOutcome::Indeterminate { .. }` to `run_acid`'s `hard_failure` match so
the pure executor is uniform with `run_saga` and the runtime bridge (one-line change), and add a
single `run_acid`-level test asserting an applier-surfaced `Indeterminate` stops the ACID walk
and flags `rolled_back`. Track in the E8 durable-ledger ticket if not done inline.

### Concern 2 (minor): global tracing subscriber is shared process-wide mutable state

`set_global_default` installs a process-wide subscriber that any future tracing-capture test
must coordinate through `CAPTURE`. As written it is sound (unique-plan-id filtering), but it is
a coordination point a later test could trip over (e.g. asserting absence of a line that another
test's span produced).

**Proposal**: keep the global default (it is genuinely required to lift the static level filter,
as the doc explains), but document in the test module header that `CAPTURE` is the single shared
sink and that any new tracing assertion must filter by a unique plan id (as this test does)
rather than asserting on the full buffer. Optionally gate the observability test behind a
`--test-threads`-independent unique-id discipline (already followed).

---

## Cross-artifact coherence

The implementation is coherent with the t11 transactional model and the t10 deterministic
interpreter: the audit ledger is the recovery substrate the saga/ACID executors already
referenced, the reconcile gate extends the existing idempotency seam without a new trait, and
the observability surface is additive (spans/events) over the existing apply loop with zero
grammar/keyword change and no new driver — matching the ticket's "pure runtime infrastructure"
framing. ARCHITECTURE.md's qfs-txn description (pure orchestration, no tokio, audit ledger +
RecoveryReport) remains accurate; the spine is unchanged.

## Carry-overs recorded

- **CO-t12-1 (E8, minor)**: `SagaExecutor::run_acid` should treat `LegOutcome::Indeterminate`
  as a hard failure (uniform with `run_saga` / the runtime bridge), with a covering test.
  Latent today; tighten for defense-in-depth.
- **CO-t12-2 (E8)**: the durable JSONL/file ledger, circuit breaker, per-leg retry/backoff
  policy, `qfs ledger show` CLI, the `policy_id` audit field, and the explicit
  `Recovery::resume` residual-sub-plan diff are deferred — the `AuditLedger` seam is sufficient
  for the file backend to drop in unchanged.
</content>
</invoke>
