# Coding Review — Architect — t11 (Transactions, idempotency, optimistic concurrency)

- **Reviewer**: Architect (neutral / structural)
- **Target**: t11 — commit `b51aec5` on `work-20260622-230954`
- **Scope**: analytical review only (no cargo/test execution per QA differentiation)
- **Artifacts read**: `crates/txn/src/{key,ledger,version,leg,outcome,saga,strategy,report,lib,tests}.rs`,
  `crates/runtime/src/txn.rs`, `crates/runtime/tests/txn_commit.rs`,
  `crates/cmd/tests/dep_direction.rs`, `ARCHITECTURE.md`,
  `crates/types/src/{value,schema}.rs` (canonicalization determinism)

## Decision

**Approve with observations.**

The transactional envelope is structurally sound, genuinely pure, and the safety
semantics are correct *for the cases the code actually guards*. There is **no blocking
safety defect** under the apply-once contract as long as the as-built recovery contract
is read honestly. The one thing that rises above a suggestion is a **documentation /
honesty gap**: two doc comments (and the ticket Overview) describe a crash-recovery
"reconcile partial apply" capability that the code does **not** implement — the gate is
`applied()`-only, and the advertised `has_intent`-based reconciliation has **zero
consumers**. That is an overclaim to correct + record as a carry-over, not a code defect,
so it does not reach "Request revision." Details below.

---

## 1. Idempotency soundness

### EffectKey is genuinely content-addressed, reorder-stable, and seed-free — correct.

- `EffectKey::derive` (key.rs) hashes a private `Canonical` struct over
  `(plan_id, effect_id, kind.label(), proc, driver, path, args)` with **FNV-1a/64**
  (`fnv1a64`, const offset/prime, no `RandomState`, no per-process seed). This is the
  right call — `DefaultHasher` would have been a latent cross-run non-determinism bug,
  and the doc comment correctly calls that out. The golden test
  (`effect_key_is_deterministic_golden`) pins length + prefix; a true byte-golden on the
  hash suffix would be marginally stronger but the FNV constants are themselves the pin.
- **Reorder stability** holds by construction: the key is a pure function of node content,
  not of frontier/batch position. `effect_key_stable_under_reordering` proves the
  property at the unit level, and structurally it cannot fail because no scheduling state
  enters `Canonical`. This satisfies the t10-reorder concern in the ticket.
- **Canonicalization determinism** — I checked the one real risk, `serde_json::to_vec`
  over `RowBatch`:
  - `Row`/`RowBatch`/`Schema` are `Vec`-backed and field-ordered → deterministic.
  - `Value::Struct(Fields)` is a `Vec<(Name, Value)>` in insertion order → deterministic
    (good — a `HashMap`-backed struct value would have been a collision/instability bug).
  - `Value::Json(serde_json::Value)` — `Object` is `BTreeMap`-backed (the workspace does
    **not** enable serde_json's `preserve_order` feature, confirmed in Cargo manifests),
    so object keys serialize in sorted order, deterministically. **This is load-bearing
    and currently correct, but it is an implicit dependency**: if anyone ever turns on
    `preserve_order`, two semantically-equal JSON values with different key insertion
    order would derive different EffectKeys (idempotency silently breaks). Worth a one-line
    comment in key.rs nailing the assumption, since the determinism contract is invisible
    from the txn crate.
  - `Value::Float(f64)` participates in the hash. Equal `f64` bit patterns serialize
    identically, so this is fine for *stability*; it is only a concern if a plan ever
    expects `-0.0`/`+0.0` or NaN payloads to dedup — out of scope for E0, noted for
    completeness.
- `unwrap_or_default()` on serialization failure (key.rs:50) degrades to an empty
  fingerprint rather than panicking. For these owned non-Map DTOs serialization cannot
  fail, so this is defensively correct and keeps the lib panic-free; the empty-fingerprint
  fallback would only ever fold *distinct* effects to the same `hash=0` if serialization
  failed for two different nodes, which is unreachable here. Acceptable.

**Collision posture**: 64-bit FNV over canonical bytes. The key string also carries
`plan_id` and `effect_id` literally (`k:{plan}:{node}:{hash16}`), so the *ledger* dedup
handle is effectively `(plan_id, node_id)` + a 64-bit content tiebreak — two effects can
only collide if they share plan **and** node id **and** hash-collide, which is not
reachable for distinct content within one plan. Cross-plan reuse is prevented by the
plan_id prefix. Sound for E0.

### Append-before-apply — correct ordering, but the recovery contract is narrower than advertised.

- Ordering is right in **both** executors: `record_intent(&key,…)` is called *before*
  `applier.apply(…)` (saga.rs:80) and before `driver.apply_batch(…)` (txn.rs:136), and
  `mark_applied` only after a confirmed `Applied` (saga.rs:96, txn.rs:151). Good.
- The dedup/resume gate is **`applied()` only** (saga.rs:76, txn.rs:123). This means:

  > **Apply-once is guaranteed only for effects that are idempotent at the driver**
  > (UPSERT / keyed write / conditional write). For a crash in the window *after*
  > `record_intent` but *before* `mark_applied`, the next run sees `applied()==None` and
  > **re-applies** the effect.

  This is the *correct* behavior for the resume case (you must re-drive a leg whose apply
  you cannot confirm landed), and the ticket's own §97 acknowledges it ("a failed delete
  leaves a harmless duplicate, never a hole"). For UPSERT/keyed writes the re-apply is a
  true no-op at the driver. **But for a plain `Insert` or a non-idempotent `Call` proc,
  the crash-window re-apply produces a duplicate / double-send** — the ledger alone does
  **not** make those apply-once. The lib doc (lib.rs:10-13) says the ledger "make[s] a
  retried / re-delivered effect a no-op" — that is true for the `applied()`-sealed path
  (clean retry) but **not** for the crash-between-append-and-apply path on a
  non-idempotent leg.

- **Honesty gap (the one finding worth recording).** `ledger.rs:42-44` documents
  `has_intent` as "*the crash-detection query: an intent with no matching applied is a
  leg that may have partially landed and must be reconciled on resume.*" The ticket
  Overview makes the same promise ("reconstruct/resume … reconcile"). **No executor calls
  `has_intent`** — I grepped the whole `crates/` tree: its only callers are the unit test
  and the count helpers. So the "reconcile a partially-landed leg" capability is
  *described* but *not built*; the actual resume policy is the simpler, honest
  "re-apply anything not sealed `applied()`." That is a fine E0 policy, but the doc
  comment + ticket overclaim it. **Carry-over**: either (a) downgrade the `has_intent`
  doc to "exposed for the E8 durable-ledger reconcile pass; not yet consumed," or (b)
  implement a reconcile step that, on an intent-without-applied for a **non-idempotent**
  leg, surfaces a `ManualReconcile`/needs-verify boundary instead of a blind re-apply.
  (b) is the real fix and belongs to E8's durable ledger; (a) is the one-line E0 honesty
  fix. Neither blocks t11.

This is a **documentation/scope** correction, not a correctness defect: every test in the
suite uses idempotent (`Upsert`) or keyed legs for the recovery case (`mv_recovers_after_
copy_before_delete` uses `Upsert` copy + keyed `Remove`), so the as-tested behavior is
sound. The risk is only that a future caller reads the doc, trusts apply-once for a raw
`Insert`/`Call`, and gets a duplicate on a real crash.

## 2. Optimistic concurrency

- **Lost-update prevention**: correct. The `Precondition` travels *on the effect node*
  (version.rs doc + `EffectLeg::from_node` lifts it into the descriptor), so the t10
  batch/parallel reorder cannot lose or cross-wire it — the structural claim holds because
  the precondition is never read from interpreter-global state. The applier compares the
  world version to the precondition and returns `Conflict(world_version)` on mismatch
  (saga.rs path; FakeApplier models it faithfully; the typed-conflict test asserts the
  stale write never lands). A stale write is blocked, not silently overwritten.
- **Bounded re-read/re-base saga retry**: cannot loop unboundedly. `apply_leg` runs
  `attempt` from 0 to `max_conflict_attempts` (`conflict_retries`, default 1) and returns
  the `Conflict` once the cap is hit (saga.rs:101). `rebase_precondition` re-conditions on
  the world version, so the retry is a genuine read-then-write, not a blind overwrite —
  good. **One subtle point worth a note**: the rebase is *open-loop* — it re-bases on the
  version the *conflict carried*, then re-applies; if the world moves again between the
  conflict report and the retry, the retry conflicts again and consumes another attempt.
  That is safe (bounded, never a blind write) but the comment "the next write is
  conditioned on fresh state (no lost update)" slightly overstates freshness — it's
  conditioned on *last-observed* state, which is the best an optimistic scheme can do.
  Fine as-is.
- **Irreversible + retry interaction**: correct and important. `max_conflict_attempts` is
  forced to `0` for irreversible legs (saga.rs:85), so an irreversible effect is applied
  **at most once** and is never re-driven on conflict. This is the right safety choice —
  a re-applied irreversible effect (a second `mail.send`) is exactly the harm to avoid.
- **Owned ETag/version (no vendor leak)**: correct. `Version`/`Etag` are owned `String`
  newtypes; `Precondition` is an owned tagged enum; `if_match_header()` returns the owned
  token the golden test asserts. No `reqwest`/SDK type appears in any `qfs-txn` signature
  (confirmed by reading every public signature + the dep test). The driver-boundary
  conversion is the documented contract. Clean.

## 3. Transaction-model honesty

- **Strategy selection** (`select_strategy`): sound and pure. It inspects only *write*
  targets (`is_write` excludes `Read`/`List`), so a read-from-many/write-to-one plan is
  still ACID (`strategy_ignores_read_only_sources` proves it). ACID requires *exactly one*
  write source **and** that source declared `Transactional`; everything else (multi-source,
  or single non-transactional source, or zero writes) falls to the saga default — the
  conservative, recoverable choice. The "many individually-transactional sources still →
  saga" case is correctly handled (no false 2PC). PREVIEW-friendly because it is I/O-free.
- **Saga compensation**: reverse-order, applied-this-run-only — correct. `run_saga`
  compensates `applied_this_run.iter().rev()` (saga.rs:152), skips irreversible legs
  (never compensated), and does **not** compensate `AlreadyApplied` legs (those belong to
  a prior run — correct, compensating someone else's applied effect would be wrong). Test
  `saga_compensates_applied_legs_in_reverse` confirms `[leg1, leg0]` order.
- **cp/mv verify-before-delete**: structurally guaranteed no-data-loss on the recoverable
  path. `CpStep::mv_sequence()` is `[Copy, Verify, Delete]` and the test asserts
  `position(Verify) < position(Delete)`. The recovery test proves a fault after copy /
  before delete leaves the source intact and re-run completes the delete (copy is
  `AlreadyApplied`). **Caveat consistent with §1**: this holds because copy is `Upsert`
  (idempotent) and delete is keyed; the no-data-loss property rests on copy's idempotence,
  not on the ledger detecting the partial. As-built and as-tested, correct.
- **Irreversible effects not compensated/retried**: correct on both axes (no-retry via
  `max_conflict_attempts=0`; no-compensate via the `irreversible` skip in the reverse
  loop). `irreversible_leg_is_not_compensated` proves it.
- **Best-effort cross-source bound is honest**: yes. There is no claim of cross-source
  atomicity anywhere in the code; `CrossSourceSaga` is explicitly "orchestrated
  best-effort" with a failure boundary in the `RecoveryReport` and a `ManualReconcile`
  compensation variant marking the human boundary. The ticket's "no real cross-source 2PC"
  reality is represented faithfully. **One gap to note**: in the *runtime* bridge
  (`commit_txn`, txn.rs), the saga path does **not** actually run compensation — it records
  the failure boundary and stops (txn.rs:105 only sets `rolled_back` for ACID; saga legs
  after failure are `skipped`, compensation is `Vec::new()`). The doc comment (txn.rs:51-53)
  is honest about this ("compensation directives are E4-supplied; at E0 the report records
  the failure boundary and the ledger enables a recovering re-run"). So the *pure*
  `SagaExecutor::run_saga` compensates, but the *wired* interpreter path defers
  compensation to E4. That deferral is acceptable and documented, but it means the
  end-to-end "saga compensates on failure" guarantee is **not yet live through the
  interpreter** — only the pure executor has it. Worth flagging so it isn't assumed wired.

## 4. The bridge shortcut (conflict-by-text)

`map_effect_error` (txn.rs:168-179) infers `Conflict` from the **terminal reason text**
containing `"conflict" | "precondition" | "412"`, guarded by
`precondition.is_conditional()`. Assessment:

- **Acceptable as an E0 stopgap**, because: (a) it is gated on a conditional precondition,
  so an unconditional write can never be mis-promoted to a conflict; (b) the worst-case
  *miss* (a real 412 whose driver message doesn't contain those substrings) degrades to a
  generic `Failed`/`rolled_back`, which is **safe** (it does not lose-update — it just
  fails louder than ideal); (c) the worst-case *false positive* (a non-conflict terminal
  whose message happens to contain "conflict") would mislabel a failure as a conflict, but
  on the ACID path both still set `failure → rolled_back`, so the safety outcome is
  identical; only the report's `class` differs.
- **But it is genuine fragility** and should be a **recorded carry-over**, not left
  implicit. The clean fix is exactly the one the task names: add a structured
  `EffectError::Conflict { version }` (or `{ version: Option<Version> }`) to the *runtime*
  `EffectError` so the driver reports the conflict typed **with the world version it
  actually observed**, instead of the bridge guessing from substrings and then back-filling
  the version with the *precondition's own expected token* (txn.rs:172-177) — which is
  **wrong as a world version**: it reports the version we *expected*, not the version the
  world *holds*, so the runtime-path `Conflict(v)` carries a misleading coordinate (the
  unit-level FakeApplier path carries the true world version; the runtime bridge cannot).
  The integration test `optimistic_conflict_surfaces_typed` asserts `Conflict("v1")` —
  i.e. it asserts the *expected* token, codifying the imprecision. This does not break
  lost-update prevention (the write was already blocked by the driver's 412), but it means
  the auto-retry rebase on the **runtime** path would rebase onto the stale expected
  version, not fresh state — so **auto-retry through the interpreter bridge cannot
  actually recover** the way the pure `SagaExecutor` can. At E0 the interpreter path does
  not auto-retry (it stops on first conflict), so this is latent, not active. **Carry-over,
  E1/E4**: `EffectError::Conflict { version }` + thread the real world version into the
  bridge so interpreter-path auto-retry becomes correct.

## 5. Spine / purity

- **`qfs-txn` is pure**: confirmed by reading every module — no `tokio`, no `async`, no
  vendor SDK type; the only impure reach is the synchronous `LegApplier` trait the runtime
  adapts. `Cargo`-graph confinement is **mechanically locked** by
  `dep_direction.rs::runtime_is_confined_to_plan_and_types`, which now asserts both
  (a') `qfs-txn`'s workspace deps ⊆ `{qfs-plan, qfs-types}` and (a) `qfs-runtime`'s ⊆
  `{qfs-plan, qfs-types, qfs-txn}`, plus the no-back-edge direction. This is the right
  structural guard and it correctly reasons that admitting `runtime → txn` does not widen
  tokio's reach (txn carries no async). Excellent — the confinement claim is enforced, not
  just asserted in prose.
- **Preconditions-as-entrypoint-map vs plan-node field**: the right deferral. `Preconditions`
  = `HashMap<NodeId, Precondition>` (txn.rs:40) lets the evaluator/tests drive optimistic
  concurrency before a plan-node `precondition` field lands. The doc is explicit that
  E1/E4 will thread it onto the node. The one structural risk of a side-map — drift between
  the map and the node it guards — is bounded here because the map is keyed by `NodeId` and
  consumed in the same `commit_txn` call that builds the legs; there is no persistence of
  the map, so no staleness window. Acceptable deferral; just ensure the eventual node-field
  migration removes the map rather than leaving two sources of truth.

## 6. t12 (audit ledger) and the E8 durable deferral

- **t12 will build cleanly on the `AuditLedger` seam.** The trait is the sole seam, is
  `Send + Sync` (so an `Arc<dyn AuditLedger>` works for the future parallel apply), and
  exposes exactly the four operations a durable backend needs
  (`record_intent`/`mark_applied`/`applied`/`has_intent`). `commit_txn` takes
  `&dyn AuditLedger`, so swapping `InMemoryLedger` for a file/append-only impl is a
  call-site change only. Good.
- **Two things t12 should pick up** (so they don't fossilize):
  1. **Make `has_intent` real** — t12/E8 is where the "intent-without-applied → reconcile,
     don't blind-reapply non-idempotent legs" pass belongs. Wire it then, or the §1 honesty
     gap persists.
  2. **Durability ordering** — the in-memory `record_intent` is trivially durable; a real
     file ledger must `fsync` the intent *before* the driver apply or the
     append-before-apply guarantee is only as strong as the OS page cache. The trait shape
     allows it; the contract doc should state the durability requirement so the E8 impl
     doesn't lose the ordering the whole recovery story rests on.

---

## Concern + proposal (Critical Review Policy)

**Concern**: the codebase advertises a crash-recovery *reconcile* capability (`has_intent`
doc comment + ticket Overview + lib "re-delivered effect a no-op") that the as-built gate
(`applied()`-only) does **not** provide for non-idempotent legs, and the runtime bridge's
conflict-by-text + expected-version back-fill makes interpreter-path auto-retry unable to
recover. Both are safe at E0 (no lost update, no data loss on the *tested* idempotent
paths) but the docs overclaim.

**Proposal (structural, preserves fidelity, no E0 rework)**:
1. **One-line E0 honesty fix**: amend the `has_intent` doc and lib.rs idempotency claim to
   scope apply-once to *driver-idempotent* legs (UPSERT/keyed), and label `has_intent` as
   "exposed for the E8 reconcile pass, not yet consumed."
2. **Recorded carry-overs** (no code now):
   - E1/E4: add `EffectError::Conflict { version }` to the runtime error and thread the
     real world version into `map_effect_error`, replacing the substring inference and the
     expected-token back-fill; this is what makes interpreter-path auto-retry correct.
   - E8 (t12/durable ledger): implement the `has_intent` reconcile pass (intent-without-
     applied on a non-idempotent leg ⇒ `ManualReconcile` boundary, not blind re-apply) and
     state the `fsync`-intent-before-apply durability requirement in the `AuditLedger`
     contract doc.

None of these is a present-tense safety defect on the tested paths, so the decision is
**Approve with observations** rather than Request revision.
