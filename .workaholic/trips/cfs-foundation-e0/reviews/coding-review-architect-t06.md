# Coding Review (Architect) — t06 Name Resolution: CALL + receiver-typed aliases

- Reviewer: Architect (Neutral / structural bridge — analytical review only, no test execution)
- Target: t06, commit `cf0ae6d` on `work-20260622-230954`
- Files read: `crates/core/src/resolve.rs`, `crates/core/src/resolve/tests.rs`,
  `crates/core/src/{lib,registry}.rs`, `crates/core/Cargo.toml`,
  `crates/cmd/tests/dep_direction.rs`, `crates/driver/src/lib.rs` (Verb/ProcSig/AliasFn/
  resolve_proc/check_capability/Driver), `crates/plan/src/lib.rs` (WriteVerb/kind_for_verb),
  `crates/parser/src/ast.rs` (EffectVerb), `crates/parser/Cargo.toml`,
  `crates/cmd/Cargo.toml`, `ARCHITECTURE.md`.

## Decision

**Approve with observations.**

The resolver is a faithful, genuinely-pure realization of the t06 ticket and the t13/t09
contracts. The reserved `cfs-core → cfs-parser` edge is wired one-directionally and the
dep-direction test is a real guard for the new topology. No defect blocks acceptance.
Two structural observations carry forward (the `WriteVerb` mirror and a latent purity
edge), neither of which is in-scope to fix in t06.

## 1. The core→parser edge — correctly wired and acyclic

Confirmed structurally sound:

- `crates/core/Cargo.toml` declares `cfs-parser = { path = "../parser" }`; `resolve.rs`
  consumes `cfs_parser::Statement` and the AST node types. The edge exists and points
  toward the more-foundational crate (core → parser), matching the ARCHITECTURE.md spine.
- **No back-edge**: `crates/parser/Cargo.toml` depends only on `cfs-lang` + `winnow` +
  `serde`. `cfs-parser` does not name `cfs-core`. The spine stays acyclic.
- **cmd still logic-free**: `crates/cmd/Cargo.toml` depends on `cfs-core` + `cfs-server`
  only — no direct `cfs-parser` edge. The resolver lives in `cfs-core`, so cmd reaches
  the parser only transitively through the hub, exactly as G5/C4 require.
- **The dep test is a genuine guard for the NEW topology**, not a stale assertion.
  `core_depends_on_parser_one_directionally` now asserts the edge is *present*
  (`core_deps` contains `cfs-parser`) AND the back-edge is *absent* (`parser_deps` lacks
  `cfs-core`). This is the inversion of the E0 "edge absent" assertion the ARCHITECTURE.md
  "Reserved edge" section described, so the guard tracks reality. `cfs-parser` also remains
  in cmd's `forbidden` list, so cmd can never grow a direct parser edge undetected.

Translation fidelity: ARCHITECTURE.md's "Reserved edge" subsection (lines 76–81) still
reads as "not yet wired … asserts the edge is absent at E0", which now contradicts the
wired reality and the lib.rs/Cargo.toml comments. Not a code defect, but a doc-drift the
team should reconcile (observation O-A below).

## 2. Resolution fidelity — faithful to the t13 contract and ticket intent

**CALL routing** (`resolve_call`): namespace → `resolve_driver_namespace` (mount router on
`/<ns>`, with an exact-mount fallback) → `resolve_proc` (the t13 driver-contract function,
not a reimplementation) → `check_args`. The miss classes map cleanly onto distinct
structured arms: `UnknownDriver`, `UnknownProcedure` (carrying the driver's available proc
list for AI recovery), `ArityMismatch`, `UnknownArg` (carrying declared param names).
Namespace isolation (`git.merge` ≠ `github.merge`) is real because the qualified key is
built from `driver.id()`, and the test asserts distinct refs. This faithfully uses the
t13 surface (`resolve_proc` / `Driver::procedures`) rather than duplicating it.

**Receiver typing** (`resolve_pipeline` → `source_receiver` → `resolve_alias`) matches the
ticket's hard-part spec precisely: the receiver is the driver of the `FROM` path
(`path_receiver` via longest-prefix `resolve_path`); `VALUES` and subquery sources yield
`None` → fail closed. The alias is matched against the receiver's `prelude()`, desugars to
the underlying proc (`split_qualified(alias.desugars_to)` → `ResolvedCall` on the receiver),
and preserves the irreversible flag by re-resolving the target proc. The three fail-closed
arms are all reachable and tested: `UnknownReceiver` (no receiver), `AliasNotProvided`
(exactly one non-receiver provider), `AmbiguousAlias` (>1 non-receiver provider, naming
candidates and directing to qualified CALL). The disambiguation rule — "receiver ships it
⇒ that binding wins even if others also ship it" — is the correct reading of receiver
typing and is the right call; only when the receiver does *not* ship it does multiplicity
become ambiguity.

**Capability gating** (`resolve_effect`): routes the target through `resolve_path`,
rebuilds a driver-local `Path`, and calls the t13 `check_capability`, mapping its
`UnsupportedVerb` onto the structured `ResolveError::UnsupportedVerb` with the supported
set. Verb derivation uses `capability_verb_for` (the canonical map). This is the
parse-time gate "before a Plan exists" the ticket and RFD §5 demand.

**Error arms are complete and branchable**: `ResolveError::code()` gives each arm a
distinct stable string, asserted unique by `error_codes_are_distinct_and_stable`. The enum
is `#[non_exhaustive]`, so AI-facing consumers can branch on `code()` while the team
retains room to add arms. Good fidelity to RFD §5 (machine-parseable, never prose, never
credentials).

One small fidelity note (observation O-B): the ticket's example sketch named the alias
class `CallableKind::{Procedure, AliasFn, CoreFn}` and a `ResolvedRef` carrying an `FnSig`.
The implementation collapses this to a single `ResolvedCall` (driver/proc/qualified/
irreversible) and defers the `CoreFn`/signature-typed path to the function-registry ticket
(`resolve_expr_fns` explicitly ignores non-prelude `FnRef`s). This is a reasonable scope
trim — t06's acceptance criteria are all about CALL, alias desugaring, ambiguity, capability
and purity, all of which are met — but it does mean "Core/registry `fn(...)` calls resolved
by signature" (ticket identifier class 3) is genuinely deferred, not delivered. The code
comments say so honestly. Acceptable; flag for the function-registry ticket owner.

## 3. Purity — genuinely pure, with one latent edge

The resolver is pure by construction: it borrows `&MountRegistry`, reads
`procedures()`/`prelude()`/`capabilities()` (all t13 pure data accessors), and never
touches `Driver::applier()` (the sole impure seam). No `std::fs`, no sockets, no threads,
no `&mut self`. The unit tests run entirely on in-memory `TestDriver`s with a `NoopApplier`
and no creds. This satisfies the ticket's "side-effect-free module" standard and the G3
type-level purity invariant.

**On the omitted `ImpureCallable` arm.** The Constructor's reasoning — every driver
callable is plan-constructing by the t13 contract, so the `-> Plan` invariant holds by
construction and a runtime reject arm is dead code — is **sound *given the current
contract***. `ProcSig` (t13) has no return-type field that could be non-Plan: `CALL`
desugars to an effect node, aliases desugar to `CALL`, and the only impure op (`COMMIT`)
is reserved to E2 and absent from the trait. There is no callable shape in the t13 surface
that *could* be non-Plan, so the arm has nothing to reject and including it would be
unreachable code (a clippy/coverage liability). I concur with omitting it.

The hole is **latent, not present**: the invariant is enforced *structurally* (by the
shape of `ProcSig`/`AliasFn`) rather than *checked*. If a future ticket gives `ProcSig` a
typed return (the ticket sketch's `FnSig`/`Type::Plan`) or admits a non-Plan callable
class (e.g. a scalar core `fn`), the `-> Plan` assertion the ticket calls "load-bearing"
would need to be reintroduced at that point — it is not latent-guarded today. Recommendation
(observation O-C): record this as an explicit invariant note so the function-registry /
typed-signature ticket re-establishes the check when it introduces a return type. Today's
omission is correct; the risk is purely forward.

## 4. EffectVerb exhaustive match (t09 O2)

The O2 closure is **real and verified at the type level**. `write_verb_for` and
`capability_verb_for` in `resolve.rs` are total matches with no `_` arm. Crucially,
`cfs_parser::EffectVerb` is **NOT** `#[non_exhaustive]` (confirmed at `ast.rs:204`), so a
new variant added to `EffectVerb` forces a non-exhaustive-match *compile error* in both
core functions until it is mapped. That is exactly the drift guard O2 wanted: the source
vocabulary cannot silently grow past its translations. (Had `EffectVerb` been
`#[non_exhaustive]`, a cross-crate match would have *required* a `_` arm and the guard
would be defeated — so the guard's validity rests on `EffectVerb` staying
exhaustively-visible. Worth a one-line comment near the enum noting it must not become
`#[non_exhaustive]` without revisiting these maps — observation O-D.)

**On removing `cfs_plan::WriteVerb` / `kind_for_verb` as the redundant mirror.** This is
the one structural call I'd push back on, and I **recommend keeping the mirror** rather
than removing it, which refines the original O2 recommendation:

- `cfs_plan` must stay parser-free (the spine forbids `cfs-plan → cfs-parser`; plan is
  below core). So `cfs-plan` cannot match on `EffectVerb` directly — it needs *some*
  crate-local verb enum to express `WriteVerb → EffectKind` (`kind_for_verb`) without
  importing the parser. `WriteVerb` is that parser-independent intermediate.
- `write_verb_for` (core) is the AST→`WriteVerb` half; `kind_for_verb` (plan) is the
  `WriteVerb`→`EffectKind` half. They compose: `EffectVerb → WriteVerb → EffectKind`.
  Removing `WriteVerb` would force `EffectKind` mapping up into core (coupling plan's
  effect-kind vocabulary to the parser via core) or push the parser dep down into plan
  (a spine violation). Neither is desirable.
- The genuine O2 risk was a *silent* `EffectVerb`→target drift via a `_` arm. That risk is
  now closed by the no-`_` match in core. The `WriteVerb` enum is not a drift hazard: it
  is `#[non_exhaustive]` and its own `kind_for_verb` is also a no-`_` total match, so a new
  `WriteVerb` variant likewise fails to compile until mapped.

So: keep `WriteVerb`/`kind_for_verb` as the **plan-side, parser-free intermediate**; treat
`write_verb_for` as the canonical *entry* of a two-stage total pipeline, not a redundant
duplicate. The "redundant mirror" framing in the original O2 was written before the
parser-free constraint on plan was load-bearing; the wired edge makes the two-stage split
the correct factoring. (Observation O-E: add a doc line on each function pointing at the
other half so the composition is discoverable.)

## 5. Forward-build readiness (t07 evaluator, t08 stdlib)

t07 (evaluator → effect-plan) and t08 (stdlib) build on this without restructuring:

- t07 consumes `Vec<ResolvedCall>` (carrying `driver`, `proc`, `qualified`, `irreversible`)
  plus the same `EffectVerb → WriteVerb → EffectKind` pipeline already in place — exactly
  the seam an evaluator needs to emit `EffectNode`s. `irreversible` is already threaded for
  E2 PREVIEW/POLICY.
- t08 (stdlib aliases/procs) registers `ProcSig`/`AliasFn` into the existing registries; the
  resolver reads them with zero new code. The deferred `CoreFn`-by-signature path (O-B) is
  the one place t08/function-registry will *extend* (not restructure) `resolve_expr_fns`.

No structural blocker. The resolver's shape (statement walk → per-op receiver threading →
structured result) is stable.

## Observations (each with a proposal)

- **O-A (doc fidelity):** ARCHITECTURE.md "Reserved edge" subsection still says the
  core→parser edge is unwired / asserted-absent. *Proposal:* update that subsection to
  "wired at E1/t06 (edge present, back-edge absent)" to match `lib.rs`, `Cargo.toml`, and
  the test — a one-paragraph edit, ideally in this trip so the durable doc and the code
  agree.
- **O-B (scope clarity):** identifier class 3 (core/registry `fn` by signature) is deferred,
  not delivered. *Proposal:* note this explicitly in the function-registry ticket so the
  `resolve_expr_fns` non-alias branch is the planned extension point.
- **O-C (latent purity):** the `-> Plan` invariant is structural, not checked. *Proposal:*
  add an invariant note (code comment or ticket line) so the typed-signature/function-
  registry ticket reintroduces the `ImpureCallable`/`-> Plan` assertion when `ProcSig`
  gains a return type.
- **O-D (guard durability):** the O2 closure depends on `EffectVerb` staying NOT
  `#[non_exhaustive]`. *Proposal:* one-line comment at `EffectVerb` warning that marking it
  `#[non_exhaustive]` would silently defeat the core-side exhaustive guard.
- **O-E (discoverability):** `write_verb_for` and `kind_for_verb` are two halves of one
  pipeline split across crates. *Proposal:* cross-reference them in their doc comments;
  **keep** the `WriteVerb` mirror (it is the parser-free plan-side intermediate the spine
  requires, not redundancy).

## Bottom line

Faithful translation of intent to structure, acyclic edge correctly wired, real
dep-direction guard, genuine purity, and a genuinely-closed O2 drift risk. Approve with
observations; none block t07/t08.
