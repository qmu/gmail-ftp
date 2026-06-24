# Coding Review — Architect — t09 (Effect-plan representation + PREVIEW/COMMIT semantics)

- Reviewer: Architect (Neutral / structural bridge)
- Target: commit `788efe9` ([Constructor] Implement t09 effect-plan representation and PREVIEW/COMMIT semantics)
- Scope reviewed: `crates/plan/src/{lib,ids,node,plan,topo,preview,apply,tests}.rs`, `crates/plan/tests/{golden_preview,purity_deps}.rs`, `crates/plan/Cargo.toml`, `crates/core/src/lib.rs`, `crates/driver/src/path.rs`, `ARCHITECTURE.md` diff.
- Method: analytical review only — no cargo/test execution (Architect QA domain).

## Decision: Approve with observations

The implementation faithfully realizes RFD §3 (purity) / §6 (runtime) / §10 (irreversible warnings) and lands the effects-as-data spine the t10/t11/t12 tickets will build inside without restructuring `Plan`. The seams are correctly placed and the dependency direction is clean. Observations below are forward-looking reconciliation risks to record, not defects.

## 1. Purity / boundary fidelity — SOUND

- `Plan`, `EffectNode`, `EffectKind`, `Target`, `VfsPath`, `Affected` are all owned, vendor-free data. No `async`, no `std::fs`, no sockets anywhere in `src/`. The only `Vec`/`String`/`BTreeMap` allocation is pure CPU. Constructing and previewing a plan provably does no I/O.
- The single impure seam is genuinely singular: `PlanApplier::apply` is the only mutating-the-world method, `commit` is its only caller, and `preview` never touches it. The module rustdoc states the invariant load-bearingly. This is exactly the §3 shape.
- `preview()` secret-freeness: a `Preview` is built only from `NodeId`, a verb label string, `Target` (`DriverId` + `VfsPath`), `Affected`, and the `irreversible` bool. Notably it does **not** include `EffectNode::args` (the `RowBatch`), so even row payloads never reach the preview surface — a stronger secret-free guarantee than "no credentials". `DriverId`/`VfsPath` are opaque identity/coordinate strings. `ApplyError::reason` is documented secret-free and the test double emits only a fixed literal. The boundary (secrets enter at the applier, E4) is crisp.
- Purity dep-closure test (`purity_deps.rs`): meaningfully locks the invariant. It does a real BFS over the `cargo metadata` resolve graph from the `qfs-plan` node (not a flat package scan), `--filter-platform` to the build target, and rejects tokio/async-std/smol/reqwest/hyper/ureq/curl/mio/socket/google-/aws-/octocrab/rusoto. This will catch a stray transitive HTTP/SDK dep regressing in. One observation (O1) below on the closure's blind spot.

## 2. DAG correctness fidelity — SOUND for t10/t11 needs

- `validate()` checks all three invariants (duplicate id via O(n²) pairwise — fine at plan scale; dangling dep; acyclic-by-successful-topo). Cycle detection delegates to `topo_order` returning `None`, which is the canonical Kahn termination test — correct and avoids a second graph traversal.
- Kahn topo with a `NodeId`-sorted ready set (BTreeMap keys + `partition_point` insertion) gives a deterministic, layer-stable order. This is precisely the substrate t10's auto-batching needs: independent nodes surface in the same ready "layer", so a batching/parallelizing interpreter can drain a ready-layer concurrently and still reproduce a deterministic serial order for golden dry-runs. The `merge` (independent union, no edge) vs `then` (sink→root edges) distinction correctly encodes "parallelizable" vs "must-sequence" for that future interpreter.
- skip-dependents-on-failure (`commit`): the `tainted` set is seeded by direct failures and grows transitively because the topo walk visits a child only after its parents, and a child is tainted if any parent is tainted. Walking in topo order is what makes the single-pass taint propagation correct — a dependent can never be applied before its failed ancestor is marked. This is the per-node applied/skipped accounting t11's recovery pass needs to reconstruct progress; `CommitReport.applied` carrying `NodeId`s makes `commit` re-runnable against a partially-applied plan as the ticket requires.
- Insert-vs-Upsert: preserved as distinct `EffectKind` variants with a dedicated test (`upsert_is_distinct_from_insert`). This keeps idempotency/retry-safety first-class for t11 — a recovery pass can safely re-drive `Upsert` nodes but must not blindly re-drive `Insert`. Good.
- `irreversible` soundness: `Remove` is inherently irreversible (decided in `EffectKind::is_inherently_irreversible`, applied in `EffectNode::new`); `Call` irreversibility is per-node/per-proc (planner-declared, `.irreversible(true)`). This two-tier model is correct for the COMMIT safety story: the grammar can't make a `Remove` reversible, while `mail.send`-style irreversibility stays a planner/registry decision. `Plan::is_irreversible` is the plan-level gate t12/E7 POLICY will read (already mirrored onto `Session.irreversible` in `qfs-core`).

## 3. Spine / seam fidelity — CORRECT

- New edge `qfs-plan → qfs-types` is correct and acyclic: `qfs-types` is the serde-only leaf, and the spine is now `qfs-driver → qfs-plan → qfs-types`. Recorded in `ARCHITECTURE.md` (both the crate table and the edge list). `DriverId` is re-used from `qfs-types` rather than redefined — one driver-identity type workspace-wide. Correct.
- Parser-free decision is correct. The evaluator (AST→Plan) is reserved for `qfs-core` via the existing `qfs-core → qfs-parser` edge, so `qfs-plan` keeps zero parser dependency and stays a low pure node. This matches the established C5 reserved-edge discipline.
- `WriteVerb`/`kind_for_verb` seam: the right way to defer AST→Plan without a parser dep. `qfs-plan` exposes a 4-variant verb enum + a total mapping function so the E1 evaluator translates `qfs_parser::EffectVerb` → `WriteVerb` → `EffectKind` without `qfs-plan` importing the AST type. The drift risk is real but bounded — see O2.

## Observations (record-and-carry; no fix required now)

### O1 — Purity dep-closure has a known blind spot (LOW; record)
The `FORBIDDEN` substring list is a denylist, so a *new-named* I/O/SDK crate (e.g. a future `isahc`, `attohttpc`, `surf`, or a vendor crate not matching `google-`/`aws-`) would pass undetected. This is acceptable for E0 (the list covers every crate family the RFD anticipates) but is a maintenance liability: the guard is only as good as the list. Proposal (for a later epic, not now): complement the denylist with an *allowlist* assertion — assert the resolved closure of `qfs-plan` is a subset of `{qfs-plan, qfs-types, serde, serde_json, serde_derive, and their pure proc-macro/ryu/itoa/memchr-class deps}`. An allowlist makes "a plan does I/O" unrepresentable by construction rather than by enumeration. Record as a hardening follow-up.

### O2 — `WriteVerb` duplicates the AST verb enum (drift risk; record, mitigation cheap)
`WriteVerb{Insert,Upsert,Update,Remove}` is a hand-mirror of the future `qfs_parser::EffectVerb`. If E1 adds/renames a write verb in the AST, nothing in `qfs-plan` forces `WriteVerb` (or `kind_for_verb`) to follow — the two enums can silently diverge, and because `qfs-plan` can't import the AST, the compiler can't catch it. Three structural mitigations, in preference order, to record for E1:
  1. **Locate the translation at the seam owner.** Since the evaluator lives in `qfs-core` (which *does* depend on both `qfs-parser` and `qfs-plan`), the canonical `EffectVerb → EffectKind` match should live there as an exhaustive `match` over the AST enum. An exhaustive match (no `_` arm) makes the compiler fail the day E1 adds a verb. In that design `qfs-plan::WriteVerb`/`kind_for_verb` become redundant and arguably should be *removed* in E1 rather than maintained as a second mirror.
  2. If `WriteVerb` is kept as a `qfs-plan`-local convenience, add an E1 conformance test in `qfs-core` asserting `WriteVerb` and `EffectVerb` have matching variant sets (round-trip every `EffectVerb` through `kind_for_verb`), so drift trips CI.
  3. At minimum, cross-reference the two enums in rustdoc on both sides so the coupling is discoverable. (Partly done already on `WriteVerb`.)
The frozen-grammar invariant (§3) bounds the blast radius — the closed core *shouldn't* gain write verbs — so this is a low-probability risk, but it is the same "two enums, no compiler link" pattern worth recording explicitly.

### O3 — `VfsPath` (plan) vs `Path` (driver) is a latent reconciliation, analogous to t05 NodeSchema↔Schema (record)
`qfs_plan::VfsPath` and `qfs_driver::Path` are today byte-identical in shape: opaque owned `String` wrappers with `new(impl Into<String>)` + `as_str()`. The split is *correct and necessary*: `qfs-driver → qfs-plan`, so the plan cannot import the driver's `Path` without a cycle; the rustdoc on `VfsPath` already states this and names E4 as the adapter site. This is exactly the same shape as the t05 `NodeSchema`↔`Schema` reconciliation the ticket flags. Two things to record so it does not rot:
  - **Reconciliation point:** E4 (driver-backed appliers) must define and test the `VfsPath ↔ Path` adaptation explicitly (a `From`/`TryFrom` at the boundary, ideally in `qfs-driver` which sees both), so the two path semantics can't silently diverge once `Path` gains structured parsing (the driver `Path` rustdoc already promises mount-segment/`@version`/predicate parsing in later epics — at that point `VfsPath` stays opaque while `Path` becomes structured, and the lossy direction must be tested).
  - **Direction to assert:** the adapter should be `Path → VfsPath` lossless (a fully-parsed driver path can always render back to the opaque vfs string) and `VfsPath → Path` fallible/parse-validating. Recording this now prevents an E4 surprise where the planner's opaque string and the driver's structured path disagree on normalization.

### O4 — `merge` keeps `self.returning`, `then` prefers `other.returning` (minor asymmetry; record)
`merge` documents "keep self's RETURNING; an independent union has no single result schema" and silently drops `other.returning`; `then` does `other.returning.or(self.returning)`. Both are defensible (a sequence's result is the tail; a union has no canonical result) but the asymmetry is a quiet semantic the E1 evaluator must know. No code change needed — record so the evaluator author treats `RETURNING` composition deliberately rather than inheriting whichever combinator they happen to call. If a future statement form needs both branches' schemas, that's a new combinator, not a change to these.

## Will t10/t11/t12 build inside these seams without restructuring `Plan`?

Yes.
- **t10 (interpreter: batching + parallelism):** the deterministic Kahn layering + `merge`/`then` parallelizable/sequenced distinction is the exact substrate; a parallel interpreter drains ready-layers and `commit`'s topo contract still defines the legal orderings. `#[non_exhaustive]` on `Plan`/`EffectNode`/`EffectKind` lets representation grow (e.g. a per-node batch-group tag) without a breaking change.
- **t11 (transactions / idempotency / recovery):** `CommitReport.{applied,skipped,failed}` + the `on_applied` ledger funnel + Insert/Upsert distinction give recovery everything the ticket scoped (deterministic order + per-node accounting → recovery is *possible*); the cross-source 2-phase orchestration is correctly deferred and not foreclosed.
- **t12 / E7 (POLICY gating):** `Plan::is_irreversible` + the explicit `Preview.irreversible` list + `Session.irreversible` mirror are the gate hooks already in place.

No structural defect found; the `Plan` shape does not need to change for the downstream tickets in scope.
