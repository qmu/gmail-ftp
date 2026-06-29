---
created_at: 2026-06-26T10:00:00+09:00
author: a@qmu.jp
type: enhancement
layer: [Domain]
effort: L
commit_hash: 7337f5e
category: Added
depends_on: [20260626101100-t53-sys-driver-admin-views.md]
---

# t57 — Extended `POLICY` / ACL language

## Overview

Delivers **decision I** and roadmap §1.2's access-control language within milestone **M5**:
the internal authorization vocabulary grows from today's per-handler ALLOW/DENY into **roles,
groups, inheritance, conditional grants, and row/column scoping**, plus a `member_of('/directories/
...')` predicate that lets an *external* directory drive qfs policy (the directory driver itself
lands in t58). `/directories` is a reserved realm — decision P / §1.3. The policy engine already exists as a library: `crates/server/src/policy/` ships
the `Policy`/`Rule`/`Verb`/`VerbSet`/`DriverGlob`/`Effectivity` model, a **pure, default-deny**
`evaluate(policy, plan) -> PolicyDecision` enforcer, the `policy_from_ddl` grammar, and the
`gate.rs` plan gate. What is genuinely **new**: richer model types (roles/groups/inheritance,
conditional `WHERE` grants, row/column scope), the `member_of(...)` predicate hook, and the
grammar that expresses them. **Critically, `POLICY` is already a frozen DDL keyword — this ticket
extends its grammar and semantics and adds NO new top-level keyword** (the closed-core governance
in `crates/lang/src/keywords.rs` `KEYWORDS`/freeze tests stays untouched).

## Exact seams

- `crates/server/src/policy/model.rs` — extend `Verb`/`VerbSet`/`DriverGlob`/`Rule`/`Policy`/
  `Effectivity` with **new** role/group/inheritance types, a conditional-grant field (a
  predicate expression on a `Rule`), and row/column scope. These are owned DTOs; no vendor leak.
- `crates/server/src/policy/enforce.rs` — `evaluate(policy, plan) -> PolicyDecision` stays
  **pure and default-deny**; extend it to resolve role/group membership, apply inheritance order,
  evaluate conditional grants, and intersect row/column scope. The purity invariant is the
  contract — `evaluate` performs no I/O.
- `crates/server/src/policy/grammar.rs` — `policy_from_ddl`/`policy_from_def` grow to parse roles/
  groups/inheritance/conditional `WHERE` clauses and scope, **without adding a keyword**: the
  `member_of(...)` predicate is an ordinary function-call expression (`crates/parser/src/ast.rs`
  `Expr::Fn`/`FnRef` — the "functions are values" seam), not new grammar vocabulary.
- `crates/server/src/policy/gate.rs` — `gate_plan`/`resolve_policy`: where the (now richer) policy
  is resolved for a handler/actor and applied before commit. The actor's identity/membership
  (t45/t55) feeds role/group resolution here.
- `crates/server/src/policy/audit.rs` — `FiredPlanRecord`: deny records must carry the offending
  verb/driver/rule and (new) the failing condition/scope so denials stay legible.
- `member_of('/directories/...')` — the predicate is a pure *call into a resolver*; the resolver is
  satisfied by the t58 `/directories/...` driver via `crates/core/src/registry.rs` `MountRegistry`
  path resolution. In t57 it is a hook with an injectable membership-resolver seam (mockable),
  keeping `evaluate` pure (membership is resolved into the decision context, not fetched inside).
- t53's `/sys/policies` path (`qfs-driver-sys`) — extended policies are still **data**: they
  round-trip as `/sys/policies` rows, so the richer model must serialize/deserialize there.
- Governance guard: `crates/lang/src/keywords.rs` freeze tests (`keyword_count_is_frozen` etc.)
  MUST remain unchanged — this is the proof that no keyword was added.

## Implementation steps

1. **Extend the model (pure, green).** Add role/group/inheritance DTOs, a conditional-grant
   predicate field on `Rule`, and row/column scope to `policy/model.rs`, with serde so they
   round-trip through `/sys/policies`. Unit-test serialization round-trips.
2. **Membership-resolver seam.** Define a pure `MembershipResolver` trait (e.g.
   `is_member(actor, '/directories/...') -> bool`) and thread a resolved membership context into
   the decision; provide a mock impl for tests. Keep `evaluate` taking the resolved context, so
   it stays I/O-free.
3. **Extend `evaluate` (still default-deny).** Resolve roles/groups, apply inheritance ordering,
   evaluate conditional grants (including `member_of(...)`), and apply row/column scope to the
   decision. Default-deny on any non-match; first-denial reporting preserved.
4. **Grow the grammar (NO new keyword).** Extend `policy_from_ddl` to parse roles/groups/
   inheritance/`WHERE` conditions/scope, with `member_of(...)` parsed as an ordinary call
   expression via `Expr::Fn`. Add golden tests; assert `keywords.rs` freeze tests still pass.
5. **Wire the gate + `/sys/policies` round-trip + docs.** Resolve the richer policy in `gate.rs`
   from the actor's membership; ensure `/sys/policies` (t53) reads/writes the extended shape.
   Update `docs/` honestly (the `member_of` example is real only once t58 can drive it — gate the
   doc claim). Bump patch in `crates/qfs/Cargo.toml`; run `cargo build/test/clippy/fmt` +
   `cargo run -p xtask -- gen-docs --check`.

## Key files

- `crates/server/src/policy/model.rs` — roles/groups/inheritance/conditions/scope DTOs.
- `crates/server/src/policy/enforce.rs` — extended pure `evaluate`; `MembershipResolver` seam.
- `crates/server/src/policy/grammar.rs` — extended `policy_from_ddl` (no new keyword).
- `crates/server/src/policy/gate.rs`, `audit.rs` — resolution + legible deny records.
- `crates/server/src/state.rs` `PolicyDef` / `crates/core/src/ddl/server.rs` — `/sys`/`/server`
  policy schema reflects the new fields.
- `docs/server.md` is generated — change the source, never hand-edit; regenerate via xtask.

## Considerations

- **No new keyword (governance, the load-bearing rule).** `POLICY` is already frozen vocabulary;
  everything new is grammar *under* `POLICY` plus function-valued predicates (`member_of` via
  `Expr::Fn`). The `crates/lang/src/keywords.rs` freeze tests (`keyword_count_is_frozen`,
  `keyword_enum_matches_golden_fixture`) must stay green untouched — that is the proof.
- **Purity + default-deny (the safety floor for authz).** `evaluate` stays a pure function over
  the plan + a *resolved* decision context; it performs no I/O and never mutates the plan. Default
  is deny everything; richer features only *narrow or widen explicitly*. A handler/actor with no
  matching grant is denied. This keeps preview-as-CI able to surface policy denials with no creds.
- **Row/column scope = data-level authz.** This is the first time policy reaches inside rows;
  scope must compose with the verb/driver decision as an intersection (`can ∧ may ∧ scope`), and
  a scope miss must abort the whole plan atomically (no partial effects), aligning with the
  existing atomic-deny behavior.
- **External directory is a hook, not a dependency edge here.** t57 owns the `member_of(...)`
  predicate and a mockable resolver; t58 supplies the live `/directories/...` driver. Keep the
  resolver injectable so policy can be unit-tested with no directory present, and so the doc
  example (`member_of('/directories/google/groups/...')`) is only advertised once t58 ships.
- **Authz is in `crates/server` (pure core), not the binary leaf.** No tokio; the live resolver
  wiring lands on `crates/qfs`. Respect dep-direction.
- **Open decision (flag).** Role-resolution precedence vs. explicit deny, and whether
  inheritance is additive-only (allow-union) or can subtract, are semantics worth pinning down
  with an example in the ticket review rather than guessing.
- **Versioning.** One PR + patch bump in `crates/qfs/Cargo.toml` + `v0.0.x` tag on ship.
