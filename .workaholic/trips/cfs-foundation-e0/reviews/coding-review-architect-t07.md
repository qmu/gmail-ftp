# Coding Review (Architect) — t07 Evaluator → effect-plan (pure)

Author: Architect
Reviewed: commit `88b0c8c` on `work-20260622-230954`
Artifacts: `crates/core/src/eval.rs`, `crates/core/src/eval/tests.rs`, `crates/core/src/lib.rs`, `ARCHITECTURE.md`; cross-checked against `crates/types/src/{schema.rs,unify.rs}`, `crates/plan/Cargo.toml`, `crates/core/Cargo.toml`, the t10/t14 tickets, and `crates/cmd/tests/dep_direction.rs`.
Method: analytical / structural review only — no cargo, no tests executed.

## Decision

**Approve with observations.**

The evaluator is a clean pure fold: resolve-gate first, then a total left-fold of the pipeline into a `PlanSource` tree and a `Plan` DAG for writes, with schema threaded stage by stage through the t05 algebra. The purity invariant holds structurally. I raise four observations and one carry-over; none rise to a defect requiring revision.

---

## 1. Purity — genuinely I/O-free; the panic-applier proof is sound but narrow

Evaluation reads only `MountRegistry::resolve_path` (pure routing) and `Driver::describe` (the pure schema seam). No signature in the module takes or returns a `World`, client, or token. `describe_schema` swallows a driver error into `Schema::empty()` so even a describe failure cannot escalate into an impure retry path. The single impure seam — `PlanApplier::apply` — is never reached: the write path goes through `PlanBuilder`/`EffectNode` construction only, and `preview()` is a pure render.

The `PanicApplier` test proves purity **at the construction+preview boundary**: `eval(...)` then `preview(&plan)` never invokes `apply`. That is a real proof for what t07 does. Its limit is that it only exercises seams t07 actually calls — it cannot prove a *future* eval addition stays pure, because nothing forces a new code path through the applier. The structural guarantee that actually closes this is the `Cargo.toml` deny of async/HTTP deps (the ticket's load-bearing property); the panic applier is the dynamic complement to that static guard. Both are present. No impure path found.

Observation 1a: `describe`-error-to-empty-schema is a deliberate totality choice (path routed ⇒ relation exists, columns late-bound). It is sound for purity, but it means a genuinely broken driver describe is indistinguishable from a legitimately schemaless node. That is a t10/diagnostics concern, not a t07 purity defect — worth a one-line carry-over note rather than a change here.

## 2. PlanSource placement — correct for now; flag as an explicit carry-over for t10/t14

Defining `PlanSource` in `cfs_core::eval` (not `cfs-plan`) is the right call **for t07** and I do not ask to move it, for three structural reasons:

- `cfs-plan` is deliberately parser-free and sits *below* core on the acyclic spine (`cfs-driver → cfs-plan → cfs-types`). `PlanSource` is produced by folding `cfs_parser` AST and routing through `MountRegistry` — both live in core. Pushing `PlanSource` into `cfs-plan` would either drag parser/registry concerns down the spine or force an awkward AST-free node type. Keeping it in core preserves the boundary.
- t14 **does not consume `PlanSource`**. Its ticket defines its own `enum LogicalPlan` ("one variant per closed-core query keyword … built from the AST by a lowering already owned upstream") in a *new* `cfs-plan`/`cfs-engine` crate. So the pushdown planner is not blocked by `PlanSource` living in core; at worst there is a lowering `PlanSource → LogicalPlan` (or t14 re-lowers from the AST directly).
- `grep` confirms `PlanSource` has **zero** consumers outside `crates/core/src/eval` today, so no premature coupling exists.

The real structural risk is t10. The interpreter is a *new* crate `cfs-runtime` that *executes* plans. If t10 ends up needing to walk `PlanSource` (e.g. to execute the read leg of an `INSERT … FROM <query>`), it would need a dependency on `cfs-core` — and core re-exports parser, registries, and the whole engine surface. A runtime executor depending on the hub crate is an inversion worth avoiding. Two mitigations, both already partially in place:

- t07 already anchors the read leg as an `EffectNode { kind: Read, target }` inside the `Plan` (the `INSERT … FROM` test shows the Read dep node). So the *plan DAG* t10 executes does **not** require `PlanSource` — the relational detail is collapsed to a `Read` node with a `Target`. This is the right shape: t10 walks `cfs-plan` types only.
- The relational detail that a Read node currently *drops* (the filter/project/expand sub-tree) is exactly what t14 wants for pushdown. That detail lives in `PlanSource`, which is not on the Read node. Today the Read node carries only `Target` + `Affected::Unknown`.

**Recorded carry-over (O-t07-1):** decide, at t10/t14 kickoff, where the relational sub-tree lives once a write's read-leg or a pushdown pass needs it. Options: (a) relocate `PlanSource` into the new `cfs-plan`/`cfs-engine` query crate t14 introduces and have core depend *up* into it; (b) keep `PlanSource` in core and have t14 lower from the AST independently, treating `PlanSource` as core's private representation. (b) keeps the spine cleanest. Either way, t07's choice is reversible and not load-bearing on the wrong side — approve.

## 3. Schema threading — faithful for the closed-core ops; two soft edges

Scan/Project/Expand/SetOp all route through the t05 algebra correctly:

- **Scan** = `Driver::describe(...).schema` — single source of truth.
- **Project** uses `Schema::project` for bare columns (real types preserved; the test asserts `id` stays `Int`, not `Unknown`), and `*`-alone preserves the full schema.
- **Expand** delegates to `Schema::expand` (which itself enforces Array/Struct-only and flattens), so type errors surface as `TypeError::NotExpandable`.
- **SetOp** uses `Schema::unify` (column-wise widening LUB) — correct per RFD §4.

Two soft edges, both acceptable-with-note:

- **Join bypasses the t05 algebra.** `fold_op`'s `Join` arm does a raw `columns.clone() + extend` and `Schema::new(cols)` — no algebra call, no duplicate-name handling. On a self-join or two sources sharing a column name (e.g. both have `id`), the joined schema carries two columns named `id`, and a later `project(["id"])` resolves to the first silently. t05 has no `join_schema` helper today, so the Constructor hand-rolled it; that is reasonable for t07's scope (Join schema is structurally "both sides' columns"). **Proposal:** add a t05 `Schema::join(lhs, rhs, on)` (or at least a documented name-collision policy — qualify/suffix duplicates) as a small follow-up so Join has the same single-source-of-truth guarantee as Project/Expand/SetOp. Not a t07 blocker; record as carry-over O-t07-2.

- **`Codec` carries no schema and `PlanSource::schema()` falls through to `input.schema()`** for Filter/Shape/Codec. For Filter/Shape that is exactly right (schema-preserving). For Codec it is a deliberate late-bind (DECODE changes the row shape, but the new schema depends on the codec, resolved later). Documented as such. Acceptable; the `fmt` string is retained so t10 can re-derive.

## 4. Late-bound `Unknown` — sound deferral, with one information-loss caveat for t14

Marking EXTEND/SET columns, computed/aliased projections, and `VALUES` literal columns as `ColumnType::Unknown` is a **sound** deferral: t07 deliberately does not own pure-expression typing (that is t10/runtime), and `Unknown` is a first-class, queryable type in t05 (`resolve_path` already produces it for Json descent). Bare columns keep their real type, which is the case that matters most for downstream projection/RETURNING typing — and the tests confirm it.

Caveat for t14 (not t07): a WHERE predicate produces *no* schema change (Filter is schema-preserving) but its **predicate expression is also dropped** — `PlanSource::Filter` stores only `input`, not the predicate AST. Likewise `Project` does not retain the source expressions for computed columns, and `Join` drops the `on` expression. t14's pushdown negotiation needs the *predicate and projection expressions* (to ask a driver "can you run `WHERE active = true` natively?"), and the late-bound `Unknown` type plus the dropped expression means that information is not on the `PlanSource` tree today. This is **not** a t07 defect — t07's contract is "fold to a relation description + thread output schema," and pushdown is explicitly out of scope (deferred to E3/t14). But it is the single most important thing for t14 to know: **t14 will need to either re-lower from the AST (which still has the expressions) or t07's `PlanSource` will need to retain predicate/projection/`on` expression nodes.** Recommend recording as carry-over O-t07-3 so t14 does not assume `PlanSource` is pushdown-ready.

## 5. Verb pipeline + governance — drift risk well closed; ordering correct

The two-hop `write_verb_for ∘ kind_for_verb` with **no `_` arm at either hop**, combined with `cfs_parser::EffectVerb` staying non-`#[non_exhaustive]` (asserted in the module doc and inherited from t04), is the correct mechanism: adding a fifth verb breaks the cross-crate match and forces both the resolver (t06) and this evaluator to be updated rather than silently dropping the verb. `effect_kind_for` re-exports the composed mapping so callers (and the test `each_effect_verb_maps_to_its_kind`) verify it without reaching into two crates. This fully closes the drift risk *as long as the `EffectVerb` non-exhaustive ban is itself test-enforced* — the doc comment asserts t04 enforces it; I did not re-verify that freeze test here, so I flag it as an assumption to confirm (O-t07-4): if t04 ever marks `EffectVerb` `#[non_exhaustive]`, this whole guarantee silently degrades to a compile that *requires* a `_` arm.

**Resolve-before-eval ordering is correct.** Running the t06 capability/procedure gate *before* the fold means a denied verb or unknown procedure never produces a `Plan` — the capability gate fires before a plan exists, which is exactly the least-privilege property RFD §5 wants (the test `capability_denied_verb_never_reaches_a_plan` asserts no plan is built). Good.

Minor structural nit (not blocking): `eval_write` computes `render_path(target.segments)` **twice** — once in the body, once again in `effect_input_schema` for the RETURNING case — and `effect_input_schema` re-folds the sub-pipeline a second time (`fold_query(p)` runs once for the dep node and again for the RETURNING schema). Pure and correct, but redundant work and a second traversal. **Proposal:** thread the already-folded `PlanSource`/resolved `(driver, vfs)` into the RETURNING computation instead of recomputing. Cosmetic; record as O-t07-5.

## 6. Will t10/t14 build on this without restructuring PlanSource?

- **t10 (interpreter):** Yes, without touching `PlanSource`. t10 executes the `Plan` DAG (`cfs-plan` types) — the read leg is already collapsed to an `EffectNode { Read, Target }` with a dependency edge, so t10 walks frontiers over `cfs-plan` only and never needs `PlanSource` (and thus avoids a `cfs-runtime → cfs-core` inversion). The batching the ticket describes operates on `(driver, op)` groups of `EffectNode`s, which t07 produces correctly. Confirmed compatible.
- **t14 (pushdown):** Partially. t14 defines its own `LogicalPlan` and lowers from the AST, so it is *not* blocked by `PlanSource`. But if anyone assumed `PlanSource` is the pushdown IR, they would hit the expression-loss caveat (§4) and the Join-collision edge (§3). The structural recommendation is to treat `PlanSource` as **t07's internal relation description for schema-threading and RETURNING typing**, and let t14 build its pushdown IR from the AST (which retains predicates/`on`/computed exprs) — not to grow `PlanSource` into the pushdown plan. That keeps each IR honest to its purpose.

---

## Carry-overs (for plan.md / later tickets)

- **O-t07-1** — PlanSource placement: at t10/t14 kickoff decide whether `PlanSource` relocates into t14's query crate or stays core-private with t14 re-lowering from the AST (recommend the latter; keeps the spine acyclic, avoids `cfs-runtime → cfs-core`).
- **O-t07-2** — Join schema: add a t05 `Schema::join` (or a documented duplicate-name policy) so Join threads through the algebra like Project/Expand/SetOp.
- **O-t07-3** — Pushdown readiness: `PlanSource` retains output *schema* but drops WHERE/JOIN-`on`/computed-projection *expressions*; t14 must source these from the AST or t07 must grow expression-bearing nodes. Do not assume `PlanSource` is pushdown-ready.
- **O-t07-4** — Confirm a t04 freeze test forbids `#[non_exhaustive]` on `cfs_parser::EffectVerb`; the whole no-`_`-arm drift guarantee rests on it.
- **O-t07-5** — `eval_write` re-folds the sub-pipeline and re-renders the path for RETURNING; thread the already-computed values through (cosmetic, pure).

## Bottom line

Pure, total, schema-faithful for the closed-core ops, correct verb governance and resolve-first ordering. `PlanSource` placement is correct for t07 and reversible; the only thing the team must carry forward is that `PlanSource` is t07's schema-threading IR, **not** the pushdown IR (expression-bearing), and that t10 should stay on `cfs-plan` types to avoid depending on core. **Approve with observations.**
