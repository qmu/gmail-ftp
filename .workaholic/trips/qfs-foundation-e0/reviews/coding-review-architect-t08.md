# Coding Review (Architect) — t08 Stdlib + driver-prelude mechanism

- Author: Architect
- Ticket: `20260622214650-t08-stdlib-and-driver-preludes.md` (E1, final)
- Commit reviewed: `0dbaf44` on `work-20260622-230954`
- Scope: analytical review only (no cargo/test execution)
- Decision: **Approve with observations**

## Files reviewed
- `crates/core/src/stdlib/mod.rs` (EvalCtx, FnError, FnSig, BuiltinFn/BuiltinEval, type labels)
- `crates/core/src/stdlib/scalar.rs` (string/path/date/number/conditional + civil-day algorithm)
- `crates/core/src/stdlib/context.rs` (NOW/CURRENT_DATE/LAST_RUN/env, EnvSource)
- `crates/core/src/stdlib/aggregate.rs` (COUNT/SUM/AVG/MIN/MAX + DISTINCT)
- `crates/core/src/stdlib/tablevalued.rs` (READ/http.get → PlanNode)
- `crates/core/src/stdlib/registry.rs` (StdlibRegistry, Prelude, parse+purity gate)
- `crates/core/src/eval.rs` (`with_stdlib`, `type_of_fn`, `project_schema`)
- `crates/core/src/resolve.rs` (t06 alias resolution, for integration check)
- `crates/driver/src/lib.rs` (`Driver::prelude` / `AliasFn`)
- `crates/core/src/lib.rs`, `ARCHITECTURE.md`, `crates/core/src/stdlib/tests.rs`

## Verdict by review axis

### Purity & determinism — sound
- Every scalar is `fn(&[Value], &EvalCtx) -> Result<Value, FnError>` with no I/O, no
  `std::fs`, no socket, no `SystemTime`. Confirmed by inspection of every body in
  `scalar.rs`/`context.rs`. Aggregates fold values only.
- Context fns read the **frozen** `EvalCtx`: `now`/`current_date` are `i64` snapshots,
  `LAST_RUN` is injected state (`Option<i64>`), `env` goes through an injected
  `EnvSource`. No wall-clock read anywhere — PREVIEW/golden reproducibility holds. The
  determinism test (`now_and_current_date_are_frozen_per_statement`) asserts equality of
  two NOW() calls in one ctx.
- `env` is secret-free and capability-gated: gate-off returns `CapabilityDenied { requested
  }` carrying only the *name*; `FnError::Type/Domain` never embed values; the test asserts
  the secret string never appears in the error's debug form. Good.
- READ/http.get are correctly **deferred**: both construct a `PlanNode` (a description) and
  perform no I/O; both are gated behind `capabilities_enabled` so an unattended context
  cannot even plan an unauthorized read. This is the right realization of the purity
  invariant for the effectful-shaped builtins.

### Registry fit — correct
- `StdlibRegistry` keys core builtins in a `BTreeMap` → deterministic iteration (mirrors
  the other registries; matches the test-stability rationale). Unknown fn → structured
  `FnError::UnknownFunction` (membership via `classify_fn`; typed form via `type_of_fn`).
- `with_core()` assembles families in a stable order then re-keys. Aggregate-vs-scalar is
  exposed via `is_aggregate(name)`. This is a faithful population of the second open
  registry.

### Typing — soundly tightens t07
- `type_of_fn` folds the builtin's declared `FnSig::returns` into the projected column
  type, replacing t07's blanket `Unknown` for `Expr::Fn`. Arity is checked
  (`accepts_arity`), and unknown/mis-contexted functions become a structured error rather
  than a silent Unknown. Without a wired registry (`stdlib: None`), the late-bound t07
  behavior is preserved — a clean, opt-in tightening.
- Aggregate-vs-scalar context is enforced at the projection head: `Select` passes
  `under_aggregate=false` (eval.rs:410), `Aggregate` passes `true` (eval.rs:417). SUM in a
  plain SELECT → `AggregateOutsideAggregate`; a top-level scalar projection under AGGREGATE
  → `ScalarInAggregate`. Scalars nested *inside* an aggregate's argument stay legal because
  only the projection head carries the flag. Correct for the cases the evaluator types
  today (see Observation 2 for the boundary).

### Prelude mechanism — right design, with a wiring gap (Observation 1)
- Parsing alias bodies as real qfs source and purity-checking that the body is exactly one
  `FROM .. |> CALL d.p` (single CALL, all-CALL ops, plain `Source::Path` receiver) is the
  correct purity-by-construction realization: an impure body yields `Impure`, an
  unparseable one `Parse`, an in-prelude dup `Duplicate`. DriverId-namespacing (per-driver
  `Vec<ResolvedAlias>`, never flattened) keeps the same alias on two drivers scoped — the
  collision-proofing the RFD requires. `alias_providers` returns deterministic order.

### Date/time without chrono — sound, not a correctness risk
- Howard Hinnant's `days_from_civil`/`civil_from_days` is a well-known, exact,
  branch-tested proleptic-Gregorian algorithm; `parse_iso_date` validates length,
  separators, month range, and per-month day count (incl. leap years). For an ISO
  `YYYY-MM-DD` epoch-day model this is fully sufficient and keeps the stdlib
  dependency-light and deterministic. A real date lib would only be warranted if/when
  timezone-aware timestamps or non-ISO formats enter scope (not this ticket). The
  round-trip invariant `FORMAT_DATE(PARSE_DATE(s)) == s` is the right contract.

## Observations (concerns + proposals)

### Observation 1 — Two parallel prelude paths; the parse/purity gate is not yet on the resolution path (translation-fidelity gap; record for E4)
The t06 resolver consumes aliases directly from `Driver::prelude() -> &[AliasFn]`
(`resolve.rs:485`, `:520`) — already-parsed `AliasFn`s handed over by the driver. The new
`StdlibRegistry` prelude facility (`register_prelude` → parse qfs body → purity-check →
`ResolvedAlias` → `as_alias_fn`) is a **separate, currently-unwired** path: nothing in
`resolve.rs` calls `StdlibRegistry::prelude_alias_fns`/`alias_providers`. So today a driver
can hand the resolver an `AliasFn` that never passed the qfs-source purity gate, and there
are two `alias_providers` implementations (one in the registry, one in the resolver over
`mounts.drivers()`).

This is acceptable for t08 (the ticket's job is to *define* the mechanism; E4 ships real
drivers), and the design is the right one — the purity-by-construction gate is exactly where
it belongs. But the bridge is missing. **Proposal:** in E4, have drivers express their
prelude as `AliasDecl` qfs-source bodies and route them through
`StdlibRegistry::register_prelude` so `Driver::prelude()`'s `AliasFn`s are the *output* of
the purity gate, not a hand-authored parallel input; collapse the duplicated
`alias_providers` onto the registry. Record this as an E4 reconciliation item.

### Observation 2 — `under_aggregate` enforcement only covers `Expr::Fn` at the projection head
The aggregate/scalar typed-error fires only for a top-level `Projection::Expr` whose `expr`
is a bare `Expr::Fn` (eval.rs:621-627). An aggregate buried inside a binary expression
(`SUM(x) + 1`) or a WHERE/HAVING predicate is not yet routed through `type_of_fn`, so the
"SUM illegal outside AGGREGATE" guarantee is partial. This matches the ticket's stated scope
(t08 types the projection head; full expression typing is downstream), and the misuse the
acceptance criteria name (SUM as a SELECT projection) *is* caught. **Proposal:** when the
expression evaluator gains recursive `Expr::Fn` typing (t10+), thread `under_aggregate`
through the whole expression walk so nested aggregate misuse is also a typed error, not a
silent Unknown. Record as a follow-on, not a t08 defect.

### Observation 3 — PlanNode is a stdlib-local DTO, not folded into `qfs_plan::EffectNode` (reconciliation debt for t10)
`stdlib::tablevalued::PlanNode { kind: Read | HttpGet }` is an owned, vendor-free DTO that
deliberately does **not** reference `qfs_plan::EffectNode` (ARCHITECTURE.md confirms
`qfs-plan` carries the effect DAG). For t08 (construct-only, no runtime) this is the correct
boundary-respecting choice and keeps the dependency direction clean. But READ/http.get
source nodes must eventually become real source/effect nodes the planner threads. **This is
a recorded reconciliation debt for t10/E2:** the runtime must define how a `PlanNode::Read`
/`HttpGet` lifts into the `qfs_plan` DAG (a `Read` source node + the codec-decode chain the
docs reference). Flagging now so t10 owns the lift rather than discovering two node models.

### Observation 4 — "alias bodies must be FROM-led" is an acceptable, well-chosen deviation
The ticket sketches `fn SEND(d) = d |> CALL mail.send`; the implementation requires
`FROM /mail/drafts |> CALL mail.send` (a `Source::Path` receiver, enforced by
`plain_source`). This is an acceptable deviation: parsing the body as a *real* qfs
`Statement::Query` is what makes the purity check by-construction rather than by-convention,
and a FROM-led pipeline is the genuine qfs surface form (a bare `d |>` is the desugar input,
not parseable source). The desugar target (`mail.send`) is correctly recovered from the lone
`CallRef`. **Minor proposal:** document this constraint on `AliasDecl::body` (the doc comment
still shows the `d |> CALL` form at registry.rs:37) so a driver author writes a parseable
body the first time. Documentation-only.

## Cross-cutting integrity
- Dependency direction preserved: stdlib is in `qfs-core`, depends down on
  `qfs-types`/`qfs-parser`/`qfs-driver`; no new crate, no back-edge, no vendor types across
  the boundary (owned DTOs only). `qfs-plan` is untouched (PlanNode is local).
- `FnError`/`PreludeError` are `#[non_exhaustive]` with stable `code()` strings —
  AI-consumable, additive-evolution-safe. `value_type_label` degrades a future `Value`
  variant to `"Unknown"` rather than failing to compile (panic-free lib code).
- Null-propagation and SQL semantics (CONCAT empty-string nulls, COALESCE/IF deliberate null
  inspection, aggregate null-skip, empty-group → Null) are consistent and tested.

## Decision
**Approve with observations.** No defect blocks t08: purity, determinism, capability gating,
deterministic registry, sound typing-tightening, and a correct purity-by-construction prelude
gate are all present and tested. The four observations are forward-looking reconciliation
items — Observation 1 (wire the registry prelude gate into resolution) and Observation 3
(lift PlanNode into `qfs_plan::EffectNode`) are the two to carry explicitly into E4 and t10
respectively.
