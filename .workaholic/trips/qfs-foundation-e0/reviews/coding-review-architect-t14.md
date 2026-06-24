# Coding Review (Architect) — t14 Pushdown planner + local combine engine

- **Reviewer**: Architect (Neutral / structural bridge)
- **Commit**: `c92c80f` on `work-20260622-230954`
- **Ticket**: `20260622214650-t14-pushdown-planner-and-local-engine.md` (last E3 ticket)
- **Method**: Analytical review only (no cargo/test execution — per QA differentiation).
- **Files read**: `crates/pushdown/src/{logical,physical,planner,lower,error,explain,lib}.rs`,
  `crates/engine/src/{combine,eval,scan,lib}.rs`, `crates/core/src/plan.rs`,
  `crates/types/src/schema.rs` (`Schema::join`), `crates/driver/src/lib.rs` (`PushdownProfile`),
  `docs/adr/0002-local-combine-engine.md`, `crates/{pushdown,engine}/Cargo.toml`,
  `crates/engine/tests/engine.rs`, `ARCHITECTURE.md`.

## Decision

**Approve with observations.**

The split is structurally sound and, on the cases I traced by hand, semantically total: every
op the driver cannot run is re-grafted as a local `CombineOp`, never dropped; the "once local,
stays local" pin (`Acc::local_pinned`) correctly prevents the one reordering hazard. O-t07-3 is
genuinely honored — predicates and `ON`/aggregate terms are lowered directly from the parser
`Expr`/`Pipeline` into the typed IR, and a non-lowerable shape is a structured `LowerError`,
never a silent drop. ADR-0002 is evidence-based and reversible. I raise three observations below;
none is a correctness defect that gates the merge, so I do not request revision. The two
load-bearing ones (O1 federated-residual column naming, O2 EXPAND/Aggregate schema fidelity)
should be tracked into E4, where qualified-name resolution and live driver schemas land.

## What I verified

### O-t07-3 honored — predicates sourced from the AST, never silently dropped (PASS)

`lower.rs::lower_predicate` lowers the parser `Expr` straight into `qfs_types::Predicate`
(`AND`/`OR`/`NOT`, `Cmp`, `In`, `Between`, `Like`). Every shape it cannot represent (a bare
column, a raw `fn(...)`, `LIKE` with a non-literal pattern, a non-equi `JOIN ON`) returns a
structured `LowerError` with a stable `code()` and an AI-repair `what` string — it is impossible
for a `WHERE`/`ON` to vanish. This is the concrete discharge of the t07 carry-over: the crate
never touches the lossy `PlanSource`; `lib.rs` and `lower.rs` both state this explicitly and the
dependency edge is on `qfs-parser`, not on the evaluator's schema-threading IR. `qfs_core::plan`
runs the lowering over the parser `Pipeline`, confirming the AST (not `PlanSource`) is the source.

### Pushdown split correctness (PASS, with the reordering hazard correctly handled)

- **Full / None / Partial gating.** `walk_chain` queries the profile by intent
  (`supports_where`/`_project`/`_limit`/`_order`/`_distinct`/`_aggregate`/`_group_by`). `Full`
  answers true for all, `None` false for all, `Partial` per declared flag — so Full pushes
  everything, None pushes nothing (`PushedQuery::is_bare`), Partial splits per flag. I cannot
  construct a case where an op is pushed that the profile denies: each branch's `if` guard is
  exactly `profile.supports_X()`.
- **Aggregate gating is correctly conjunctive.** `can_push = !local_pinned && supports_aggregate
  && (group_by.is_empty() || supports_group_by)`. A driver that can aggregate whole-relation but
  not bucket (`supports_aggregate && !supports_group_by`) correctly keeps a *grouped* aggregate
  local while still being eligible for an ungrouped one. Sound.
- **The "once local, stays local" pin is the load-bearing correctness rule and it is right.**
  I traced `Scan |> Sort |> Filter` against a profile with `where_=true, order=false`: post-order
  visits `Sort` first → unsupported → `force_local` (pins) → `Filter` sees `local_pinned()` →
  forced local even though `where_` is true. Pushing the filter would place it *below* the local
  Sort, which is a no-op for row sets but the pin is still the correct invariant (it generalizes
  to ops where order matters, e.g. `Limit` below `Sort`, or a future projection that renames a
  filtered column). The residual is built innermost-first and re-applied outermost-last in
  `finish`, preserving pipeline order. I found **no** path that drops an op or pushes an
  unsupported one.
- **JOIN/SetOp always federate (even single-source).** `single_source_chain` returns `None` the
  moment it hits a `Join`/`SetOp`, so even a same-source join is federated locally. This is the
  honest, documented deferral: native join pushdown into one backend is an explicit E4 refinement
  (`planner.rs` doc + `lib.rs`). It is conservative-correct (a federated single-source join still
  returns the right rows; it just doesn't yet exploit the backend), so it is a *performance*
  deferral, not a correctness gap.

### Federation correctness (PASS for the cases traced; see O1)

`federate` recurses into each JOIN/SetOp side via `partition_by_source` (so each side is
maximally pushed to its own source) and wraps them in `HashJoin(on)` / `SetOp(kind)`. The
`MiniEvaluator` implements each residual op as a pure kernel over `RowBatch`:
`hash_join` builds on the right, probes with the left, emits left-cols ++ right-cols using
`Schema::join` for the output schema; `set_op` does distinct union / except / intersect keyed by
a stable row rendering; `aggregate`/`sort`/`distinct`/`expand`/`filter`/`project`/`limit` are
straightforward and total (predicate eval degrades incomparable comparisons to `false` rather
than panicking — appropriate for a residual over heterogeneous data). The differential test
(`differential_partitioned_equals_all_local`) runs one logical plan two ways (None-source
all-local vs. Partial-source partitioned with a faithful driver fake pre-applying the pushed
work) and asserts equal rows — this is the right shape for the differential property the ticket
demands.

### Engine decision ADR-0002 (PASS — evidence-based, reversible, honest)

The decision is grounded in measured facts, not opinion: `wasm32-unknown-unknown`
non-buildability of `libduckdb-sys` (C++/cc/bindgen, no libc/fs on that target; DuckDB's own
wasm is a separate Emscripten artifact), a measured ~49 MB host CLI footprint, and the
observation that the residual operator set is small *by design* because the heavy work is pushed
down. Reversibility is real: `CombineEngine` is the seam, no DuckDB/vendor type crosses
`PushedQuery`/`CombineEngine` (owned DTOs only), and an optional `DuckDbEngine` is described as a
future non-default-feature addition. Leaving `DuckDbEngine` *unimplemented* (rather than a stub
that misleads) is honest. The ADR mirrors ADR-0001's winnow-vs-chumsky reasoning, which keeps the
decision record coherent with the existing architecture.

### Spine / purity (PASS)

- Edges are acyclic and match `ARCHITECTURE.md`: `qfs-pushdown → {qfs-driver, qfs-types,
  qfs-plan, qfs-parser}` (Domain, no I/O/async/vendor), `qfs-engine → {qfs-pushdown, qfs-types}`,
  `qfs-core → qfs-pushdown` (integration seam). No back-edges; both sit below `qfs-core`.
- `qfs-engine` is genuinely serde-only: its `Cargo.toml` declares only `qfs-pushdown` +
  `qfs-types`, and the ADR's `cargo tree` closure is the serde family + `thiserror`. No DuckDB,
  no `cc`, no bindgen. wasm-clean by construction.
- **`Schema::join` collision policy is sound.** Left columns first; a right column whose name
  collides is disambiguated to `<provenance-driver>.<name>`, or `r.<name>` when no provenance —
  never a silent shadow. The engine test asserts both `id` and `r.id` survive a self-join. Types
  and nullability are preserved per side (a JOIN does not widen). This is the structural
  counterpart to the t07 raw concat the doc-comment calls out.

### E4 / T10 consumption (PASS)

`PhysicalPlan::scans()`/`scan_count()` surface the independent `ScanNode`s in left-to-right
order, exactly what T10's batcher parallelizes; the engine's `Cursor` consumes them positionally
in the same order, so plan and results stay aligned. `ScanNode` carries `SourceId` (per-leg
observability) and the resolved `Schema`. E4 drivers declare a `PushdownProfile` and the planner
already negotiates against it via `supports_*` — no restructuring needed; E4 only *refines* (real
SQL generation, single-backend join pushdown). The deferrals are honest and localized.

## Observations (each with a proposal)

### O1 — Residual ops above a federated JOIN must reference *post-`join`* column names (structural, track to E4)

`federate` wraps a unary residual (`Filter`/`Project`/`Sort`/…) directly over a `HashJoin`
whose output schema is `Schema::join(lhs, rhs)` — which **renames** a colliding right column to
`<driver>.<name>`. The residual op, however, carries column names lowered from the AST. The
engine's `eval::resolve`/`project`/`sort` look columns up *by name* in the joined batch schema.
So a residual that references a non-colliding column resolves fine, but one referencing the
*right* side of a name collision must use the qualified `<driver>.<name>` form, and a bare
unqualified reference would silently resolve to the *left* column (a wrong-but-not-crashing
result). In t14 this is latent (no test exercises a residual filter on the renamed side, and the
parser's qualified-name story is still thin), but it is the kind of federation correctness hole
the ticket's "hard part" warns about.
**Proposal**: in E4 (where qualified `a.b` resolution lands), add a planner check or a lowering
rule that resolves a residual column reference against the *federated output* schema (the
`Schema::join` result), so an ambiguous bare reference is either qualified or raised as a
structured "ambiguous column" `PlanError` — never silently bound to the left side. A focused
differential fixture (two sources both exposing `name`, a residual `WHERE name = …` on each side)
would lock it.

### O2 — `EXPAND`/`Aggregate` residual schema is an approximation (acceptable now, tighten in E4)

`walk_chain`'s `Expand` arm returns the *input* schema (the doc-comment notes it as "a
conservative approximation" because the expanded field may be late-bound), and `aggregate_schema`
types non-`Count` aggregates as `Unknown`. The engine's runtime `expand`/`aggregate` kernels
compute the real shape from the data, so the *rows* are correct; only the planner-side `ScanNode`/
residual *schema* is loose. This is fine for a residual (the schema above a local op is informative,
not authoritative) but means `explain()`/schema-driven consumers see `Unknown` where a real type
exists.
**Proposal**: when E4 supplies live driver `describe` schemas, thread the element/aggregate type
through `Schema::expand` (already implemented in `qfs-types`) and the aggregate output types, so
the residual schema is exact. No change needed in t14; just don't let the approximation leak into
a downstream consumer that treats `Unknown` as a hard type.

### O3 — `Sum` widening to `Int` may silently narrow large `Float` accumulations (minor, behavioral)

`eval::run_aggregate`'s `Sum` accumulates into `f64` and, if no float was seen, casts back via
`acc as i64`. For very large integer sums this is the standard f64-precision caveat; for a mixed
Int/Float column it yields `Float`. It is internally consistent and matches what a naive all-local
run does (so the differential property holds), but it is worth a doc note that integer `SUM` over
a residual goes through `f64`.
**Proposal**: add a one-line doc-comment on `run_aggregate` flagging the f64 intermediate, and (if
exactness ever matters) split an integer fast-path that accumulates in `i64`. Non-blocking.

## Cross-cutting coherence

The two new crates land cleanly inside the existing spine and the `ARCHITECTURE.md` crate map +
dependency-spine block were updated to match (Domain `qfs-pushdown`, Infrastructure `qfs-engine`,
`qfs-core → qfs-pushdown` seam). The structured-error discipline (`LowerError`/`PlanError`/
`EngineError` each with stable `code()` and `#[non_exhaustive]`) is consistent with the rest of
the workspace's AI-consumable error policy. Determinism is preserved (rule-based split, stable
`explain()` ordering, stable row-key renderings). No credentials anywhere; `PushedQuery` carries
only owned DTO fields.

**Net**: the split, the federation, and the engine decision are sound for what t14 scopes. The
one genuinely load-bearing concern (O1) is a *latent* federated-column-naming issue that becomes
live only with qualified-name resolution in E4, and it is a wrong-result-not-crash case — so I
flag it prominently for E4 rather than gate the merge. Approve with observations.
