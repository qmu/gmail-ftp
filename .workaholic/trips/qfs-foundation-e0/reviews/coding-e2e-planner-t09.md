# Coding E2E Review — Planner — t09 (Effect-plan + PREVIEW/COMMIT)

Author: Planner
Role: E2E / external-interface testing (no code review)
Target: t09 — Effect-plan representation + PREVIEW/COMMIT semantics
Crate under test: `qfs-plan` (public API; also re-exported via `qfs_core`)

## Method

A small throwaway consumer crate was built in `/tmp/t09-e2e` with its own
`[workspace]` and path-deps on `crates/plan` + `crates/types`. It contains **no
production code** and was removed after the run. It drives only the PUBLIC API
(`Plan`, `PlanBuilder`, `EffectNode`, `EffectKind`, `Target`, `Affected`, `preview`,
`commit`, `PlanApplier`, `RecordingApplier`, `PlanError`) exactly as an outside
consumer would. 46 assertions, all green; no network, no credentials.

## Results — PASS/FAIL per item

### Item 1 — validate() acceptance + structured rejection — PASS
- Mixed `Read(#0) -> Insert(#1) -> Remove(#2) -> Call mail.send(#3)` DAG built via
  `PlanBuilder`; `validate()` returns `Ok`. 4 nodes, 3 dep edges. PASS
- `Remove` and the `mail.send` `Call` carry `irreversible == true`; `Read`/`Insert`
  reversible. PASS
- Cyclic plan (back-edge `#3 -> #0`) rejected with `PlanError::Cyclic`. PASS
- Dangling-dep plan (edge to `NodeId(999)`) rejected with
  `PlanError::DanglingDep { child: #999, parent: #1 }`. PASS
- Duplicate-id plan (two leaves sharing `NodeId(7)` via `merge`) rejected with
  `PlanError::DuplicateId(#7)`. PASS

### Item 2 — PREVIEW — PASS
- (a) Applies nothing: `preview(&plan)` takes `&plan`; plan still validates and node
  count unchanged afterward. No applier is involved. PASS
- (b) Per-node affected + combined total: `Read` `AtMost(100)`, `Insert` `Exact(3)`
  (derived from a 3-row `RowBatch`), combined `total_affected == AtMost(109)`
  (`AtMost(100)+Exact(3)+AtMost(5)+Exact(1)`, with `AtMost` correctly dominating
  `Exact`). PASS
- (c) Irreversible nodes flagged explicitly: `preview.irreversible == [#2, #3]`. PASS
- (d) Deterministic + secret-free: Display rendered twice byte-identical; rows in
  topo order `[#0,#1,#2,#3]`; Display+JSON contain none of
  token/password/secret/bearer/credential/api_key. PASS
- Pure read-only plan: `Plan::pure()` previews with `is_pure == true` and Display
  `"PREVIEW: pure query — no effects to apply."`. PASS

#### Sample PREVIEW Display
```
PREVIEW: 4 effect(s)
  #0 READ -> gmail:/mail/inbox [affected <=100]
  #1 INSERT -> s3:/bucket/out [affected 3]
  #2 REMOVE -> gmail:/mail/inbox [affected <=5] (!)
  #3 CALL mail.send -> gmail:/mail/outbox [affected 1] (!)
  (!) irreversible: 2 node(s) [#2, #3]
  total affected: <=109
```

#### Sample PREVIEW JSON (abridged)
```json
{
  "rows": [
    { "id": 0, "verb": "READ",   "target": {"driver":"gmail","path":"/mail/inbox"},  "affected": {"at_most":100}, "irreversible": false },
    { "id": 1, "verb": "INSERT", "target": {"driver":"s3","path":"/bucket/out"},     "affected": {"exact":3},     "irreversible": false },
    { "id": 2, "verb": "REMOVE", "target": {"driver":"gmail","path":"/mail/inbox"},  "affected": {"at_most":5},   "irreversible": true },
    { "id": 3, "verb": "CALL mail.send", "target": {"driver":"gmail","path":"/mail/outbox"}, "affected": {"exact":1}, "irreversible": true }
  ],
  "irreversible": [2, 3],
  "total_affected": {"at_most": 109},
  "is_pure": false
}
```

### Item 3 — COMMIT — PASS
- Custom in-memory `PlanApplier` (records applied ids) ran `commit`: applied all 4
  nodes, 0 skips, no failure, `is_complete() == true`. PASS
- Apply order `[#0,#1,#2,#3]` is a valid topological order — every dependency edge
  `(parent,child)` has parent preceding child in the recorded log. PASS
- `on_applied` ledger hook fired once per node, in order. PASS
- Failure case: `RecordingApplier::failing_on(#1)` (the Insert). Result —
  only `#0` (Read) applied; `failed == ApplyError{ id:#1 }`; `#2` and `#3` recorded
  **skipped** (not applied), reasons `DependencyFailed`. The `RecordingApplier`'s own
  call log never received `#2`/`#3` — confirming skipped dependents were never
  attempted. PASS

#### CommitReport — failure on Insert(#1)
```
applied: [AppliedEffect { id: NodeId(0), affected: 100 }]
failed:  Some(ApplyError { id: NodeId(1), reason: "configured failure" })
skipped: [(NodeId(2), DependencyFailed(NodeId(1))),
          (NodeId(3), DependencyFailed(NodeId(2)))]
```
Note: `#3`'s skip reason cites `#2` (its direct parent, itself tainted by `#1`)
rather than the root cause `#1`. Taint propagates transitively along edges, so every
transitive dependent is correctly skipped; the cited reason is the nearest failed
parent, not the root. This is honest and acceptable; flagged only for reviewer
awareness in case a future audit ledger wants root-cause attribution.

### Item 4 — Adversarial plans (no panic) — PASS
- Empty plan (`Plan::pure()`): validate Ok; preview Ok; commit returns empty report.
- Self-dependency (`#0 -> #0`): `validate` returns `PlanError::Cyclic`; `preview` and
  `commit` do not panic (commit returns an empty report on a cyclic plan).
- Large fan-out (1 root + 500 children, 501 nodes): validate Ok; preview has 501
  rows with `total == Exact(501)`; commit applies all 501, root applied first.
- No panics observed anywhere.

## Concern + proposal (Critical Review Policy)

Concern (business/usability, external-consumer ergonomics): `AppliedEffect` is
`#[non_exhaustive]` and exposes **no public constructor**. An external crate
implementing `PlanApplier` therefore cannot build the `Ok(AppliedEffect)` success
value at all — in this test the custom in-memory applier had to delegate the success
value to the bundled `RecordingApplier` as a workaround. This matters for the very
next epic: the E4 driver-backed appliers live in separate crates and will hit this
wall, since the whole point of `PlanApplier` is that real impls land outside
`qfs-plan`. Proposal: add a public constructor `AppliedEffect::new(id: NodeId,
affected: u64) -> Self` (and optionally `ApplyError::new(id, reason)`), keeping the
`#[non_exhaustive]` for forward-compat while making the documented seam usable by
out-of-crate appliers. This is a small, additive change; it does not affect any t09
acceptance criterion (which only exercises `RecordingApplier`), so it need not block
this ticket — recommend it be addressed at the E4 driver boundary at the latest.

## Verdict

**E2E approved.** All four task items pass (46/46 assertions); preview applies
nothing, reports honest per-node + combined affected counts, flags irreversible
nodes, and renders a deterministic secret-free summary; commit applies in valid topo
order and correctly skips every transitive dependent on failure; no panics on
adversarial input. One non-blocking ergonomics concern raised (public
`AppliedEffect` constructor for out-of-crate appliers) with a concrete proposal for
E4. Throwaway test crate removed.
