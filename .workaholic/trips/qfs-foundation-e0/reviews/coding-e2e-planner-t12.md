# Coding Phase — E2E / External Validation: t12 (Audit ledger + observability)

- **Reviewer**: Planner (Progressive) — E2E / external-consumer testing only (no code review)
- **Ticket**: `20260622214650-t12-audit-ledger-and-observability.md`
- **Method**: throwaway external crate (`/tmp/t12-e2e`, own `[workspace]`, path-deps on
  `crates/{runtime,txn,plan,types}` + tokio/tracing/tracing-subscriber/serde_json). No
  production code touched; no network; in-memory `ApplyDriver`s only. Crate removed after the run.
- **Public surface exercised**: `qfs_runtime::Interpreter::commit_txn(...) -> (CommitStrategy,
  RecoveryReport)`; `qfs_txn::{AuditLedger, InMemoryLedger, EffectKey, EffectLeg,
  LegOutcome::{Applied,AlreadyApplied,Conflict,Indeterminate,Failed}, Precondition, Version}`;
  `qfs_runtime::EffectError::Conflict{version}`; a custom `tracing-subscriber` capture layer.

## Overall verdict: **E2E blocked — RecoveryReport containing a `Conflict` cannot be serialized to JSON**

Five of six functional guarantees pass cleanly and impressively (audit determinism, the
secret-free guarantee, observability, and the t11 has_intent reconcile contrast are all
verified hard). The block is a single, narrow, **reproducible serialization defect**: any commit
that surfaces an optimistic-concurrency conflict (`LegOutcome::Conflict(Version)`) produces a
`RecoveryReport` that **fails to serialize to JSON** at runtime. Since the ticket's own
acceptance criteria and the `RecoveryReport` doc-comment promise this report is the serializable,
golden-testable, JSON audit-of-record, a conflicting commit cannot be audited via the documented
JSON path. The conflict *semantics* are correct (typed, carries the real world version,
secret-free) — only the serialization of that one variant is broken.

---

## PASS / FAIL per item

### ITEM 1 — Audit record + deterministic order — **PASS**

Mixed plan (`n0` INSERT applied, `n1` INSERT failed at `/fail`, `n2` INSERT skipped via the
post-failure short-circuit) run through `commit_txn`; `RecoveryReport` serialized to JSON.

- Every leg is recorded with its disposition, in stable plan (topological) order:
  `dispositions: ["applied", "failed", "failed"]`, `failure_at: 1`.
- Each leg carries identity + shape: `id`, idempotency `key`, `kind`, `target {driver, path}`,
  `irreversible`, and the tagged `outcome`.
- **Determinism**: two independent runs produced **byte-identical** JSON (`A == B: true`). Leg
  order is plan order (the `commit_txn` walks `topo_order`), independent of wall-clock.

Note: the post-dependency-failure legs are reported as `outcome:"failed"` with reason
`"skipped: a prior leg in the commit failed"` (the report models a not-attempted leg as a
terminal skip, per `LegRecord::skipped`). That is a faithful disposition, not a defect.

### ITEM 2 — Secret-free guarantee (RFD §10) — **PASS**

Planted two obvious secrets in every effect's `args` row values: `SECRET-TOKEN-XYZ` and
`hunter2-PLAINTEXT-PASSWORD`. Searched **both** the serialized report JSON and the captured
tracing spans/events (custom capture layer recording every span field + event field + message).

```
secret "SECRET-TOKEN-XYZ":            in_report_json=false  in_captured_tracing=false
secret "hunter2-PLAINTEXT-PASSWORD":  in_report_json=false  in_captured_tracing=false
secret "PLAINTEXT":                   in_report_json=false  in_captured_tracing=false
secret "hunter2":                     in_report_json=false  in_captured_tracing=false
ANY secret leaked: false
```

The report records only identity/shape (driver, path, kind, key, counts) and the spans/events
likewise carry metadata only — the row payload (where the secrets live) is never serialized or
logged. The redaction boundary holds: `EffectDescriptor` records `arg_rows` (a count), never the
rows. **The RFD §10 guarantee is verified hard.**

### ITEM 3 — Observability (spans/events, stable ids, per-leg metadata) — **PASS (with one caveat, see below)**

Captured spans/events for a clean 2-leg commit:

```
SPAN[commit_txn] plan_id=plan-obs strategy=cross_source_saga trace_id=t:plan-obs:00000003
SPAN[effect]     effect.driver=a effect.id=0 effect.kind=INSERT effect.path=/a/one
EVENT            effect.driver=a effect.id=0 effect.irreversible=false effect.kind=INSERT outcome=applied message="leg applied"
SPAN[effect]     effect.driver=a effect.id=1 effect.kind=UPSERT effect.path=/a/two
EVENT            effect.driver=a effect.id=1 effect.irreversible=false effect.kind=UPSERT outcome=applied message="leg applied"
```

- Root span carries `trace_id` **and** `plan_id` (and `strategy`): **true**.
- Per-leg child spans + the per-leg event carry `effect.id`, `effect.driver`, `effect.kind`
  (and `effect.path`, `effect.irreversible`), plus the per-leg `outcome` code: **true**.
- `trace_id` is structurally `t:<plan_id>:<hex-seq>` — **carries no wall-clock / RNG component**
  (it is minted from the plan id + a process-monotonic atomic counter), so trace output is
  reproducible in structure across runs. Verified: prefix `t:plan-obs:` is stable across two
  runs; the trailing component is a plain hex sequence, not a timestamp.

**Caveat (not a block, by design):** "two runs of the same plan produce the *same* trace_id"
holds only in the *deterministic-structure / no-wall-clock* sense, **not as bitwise-identical
ids**. Run 1 → `t:plan-obs:00000003`, Run 2 → `t:plan-obs:00000004`: the sequence increments per
mint via a **process-global** `AtomicU64`, so two executions in one process get distinct, ordered
ids by design (`observe.rs` documents this explicitly: "two executions of the same plan in one
process get distinct, ordered ids"). This is the intended E0 behaviour (the durable ULID minting
is deferred to E8), and it is wall-clock-free and deterministic *given the mint sequence* — but a
consumer must **not** assume the trace_id alone is a stable cross-run correlation key; the stable
cross-run key is `plan_id`, which the root span and the idempotency `key` both carry. I flag this
only so downstream (E7 server `/server/events`, golden tests) correlates on `plan_id`/`EffectKey`
rather than the raw `trace_id`. No code change requested for t12.

### ITEM 4 — has_intent reconcile (t11 fix): Indeterminate vs safe replay — **PASS**

Simulated the ambiguous crash window (intent appended, apply unsealed) by seeding a fresh
`InMemoryLedger` with `record_intent(key, descriptor)` derived exactly as the runtime does
(`EffectKey::derive` + `EffectLeg::from_node`), then re-running `commit_txn`. Mutation counter on
the in-memory driver proves whether the side effect fired again.

```
INSERT run1 outcome: applied        | mutation count: 1
INSERT resume (intent unsealed):    | outcome: indeterminate  | Indeterminate: true | mutation count: 0
UPSERT resume (intent unsealed):    | outcome: applied        | replayed: true      | mutation count: 1
```

- A non-idempotent **INSERT** with an unsealed intent surfaces `LegOutcome::Indeterminate` and the
  driver is **never re-invoked** — mutation count stays **0** (no double-apply). The leg is
  treated as a hard failure that stops the saga (apply-once preserved, RFD §6/§10).
- The contrasting **UPSERT** is **replay-safe** (`EffectDescriptor::is_replay_safe()` true): it
  re-applies and converges — mutation count `1`, outcome `Applied`.

The Indeterminate-vs-replay contrast is exactly the t11 fix the ticket calls out, and it behaves
correctly. (Caveat inherent to E0, already documented in `ledger.rs`: `InMemoryLedger` is
process-local, so this exercises the reconcile *guard* within one process, not across a real OS
crash; the durable fsync-before-apply sink is the E8 ticket. The guard logic itself is fully
validated.)

### ITEM 5 — Conflict{version} (stale-version write) — **PARTIAL: semantics PASS, serialization FAIL** → the block

A conditional `UPDATE` with a **stale** `Precondition::IfVersion("stale-v1")` against an
in-memory driver that returns `EffectError::Conflict{ version: "world-v9-REAL" }`:

- **Semantics PASS**: the txn bridge surfaces a typed `LegOutcome::Conflict(Version)` carrying the
  **real world version** `"world-v9-REAL"` (not the stale expected token) — verified
  `v.as_str() == "world-v9-REAL"`. The conflict leg's debug rendering contains **no secret**
  (secret-free holds here too).
- **Serialization FAIL**: serializing the `RecoveryReport` that contains that conflict leg fails:

```
WARNING: RecoveryReport containing a Conflict FAILED to serialize to JSON:
  cannot serialize tagged newtype variant LegOutcome::Conflict containing a string
```

### PROBE — which `LegOutcome` variants serialize (scoping the defect)

Serializing each variant in isolation under `#[serde(tag = "outcome")]` (internal tagging):

```
Applied(receipt):     OK   {"outcome":"applied","id":0,"affected":1,"new_version":null}
AlreadyApplied:       OK   {"outcome":"already_applied"}
Conflict(Version):    FAIL cannot serialize tagged newtype variant LegOutcome::Conflict containing a string
Indeterminate{key}:   OK   {"outcome":"indeterminate","key":"k:p:0:..."}
Failed(terminal):     OK   {"outcome":"failed","class":"terminal","reason":"x"}
```

Exactly **one** variant breaks: `Conflict(Version)`.

---

## Root cause (external observation, for the Constructor)

`qfs_txn::LegOutcome` is `#[serde(tag = "outcome")]` (serde **internal tagging**). Serde's
internal tagging cannot represent a **newtype variant that wraps a non-struct/non-map value**:
`Conflict(Version)` wraps `Version(pub String)`, a newtype-over-`String`, so serialization fails
at runtime with *"cannot serialize tagged newtype variant … containing a string"*. The sibling
struct variant `Indeterminate { key }` serializes fine precisely because it is a struct variant
(a map), and `Failed(EffectError)` serializes because `EffectError` is itself a struct/map under
its own `#[serde(tag = "class")]`. The defect is structural to the `Conflict` variant's shape +
internal tagging, not to the data.

## Business impact (Planner lens)

The `RecoveryReport`/ledger **is** the audit-of-record and (per the ticket) the server's
`/server/events` substrate. Optimistic-concurrency conflict is a *routine, expected* outcome of a
read-then-write under concurrency — not an edge case. Today, the moment a commit hits a conflict,
the JSON audit trail for that whole commit **cannot be emitted** (a hard serde error, which a
naive caller doing `to_string(&report).unwrap()` would turn into a panic at the worst possible
moment — a contended write). That defeats the auditability and recoverability the ticket exists
to deliver, and it would break the E7 server's event log and any golden test that includes a
conflict. The fix is small and the surrounding machinery is otherwise excellent, so this is a
focused "Request revision", not a redesign.

## Proposed remedies (any one unblocks; Constructor's call)

1. **Make `Conflict` a struct variant** — change `Conflict(Version)` to
   `Conflict { version: Version }` (mirrors `Indeterminate { key }`). Smallest, most consistent
   fix; serializes as `{"outcome":"conflict","version":"world-v9-REAL"}`. Touches the few
   match-sites in `runtime/src/txn.rs` and any pattern on `Conflict(_)`.
2. **Switch the enum to adjacent tagging** — `#[serde(tag = "outcome", content = "data")]`, which
   *can* represent newtype-of-primitive variants. Changes the JSON shape of **all** variants
   (wraps their bodies under a `data` key), so it would churn golden snapshots — less surgical.
3. **Wrap the version** in a tiny struct field even if kept as a tuple — least idiomatic; option 1
   is cleaner.

I recommend **option 1** (struct variant) for consistency with `Indeterminate` and minimal JSON
churn. Whichever is chosen, add a regression assertion that a `RecoveryReport` containing **every**
`LegOutcome` variant — including `Conflict` — round-trips through `serde_json` (the golden-test
acceptance criterion in the ticket should cover the conflict case explicitly; today's golden plan
evidently has no conflict leg, which is how this slipped through).

## Secondary observations (non-blocking, for the record)

- **trace_id cross-run correlation** (Item 3 caveat): document/standardise that downstream
  correlates on `plan_id` + `EffectKey`, not the raw process-monotonic `trace_id`. No t12 change.
- **No panics**: all six scenarios ran to completion without a panic in library code (the test
  harness deliberately surfaces the serde error as a handled `Err`, not an `unwrap`).
- The audit record's deterministic ordering, the secret-free guarantee, and the
  Indeterminate/replay reconcile are all production-grade and verified hard.

## Re-test plan on fix

Re-run the same external crate: assert `to_string_pretty(&report)` **succeeds** for the
stale-version commit and that the JSON carries `"outcome":"conflict"` + the real world version,
with no secret present; re-confirm the variant probe shows `Conflict: OK`; spot-check Items 1–4
are unaffected.
