# Coding Review (Architect) — t05 Type & schema model

Author: Architect
Phase: coding / review-and-testing
Target: t05 — Type & schema model (commit `d0c890a`, branch `work-20260622-230954`)
Scope reviewed: `crates/types/src/{lib,schema,value,unify,predicate,error}.rs`,
`crates/types/tests/model.rs`, `crates/codec/src/lib.rs`, `crates/core/src/lib.rs`,
`crates/driver/src/lib.rs` (NodeSchema), `crates/cmd/tests/dep_direction.rs`,
`ARCHITECTURE.md`, and the touched `Cargo.toml`s. **Analytical review only — no
cargo/test execution** (Architect QA domain).

## Decision

**Approve with observations.**

The new `qfs-types` leaf is a faithful, well-bounded realization of RFD §4/§5 and the
t05 ticket. The spine stays acyclic, the leaf is genuinely a leaf, the dep-direction
test locks the new edges, and the coercion/unify/comparability rules are total with no
panics. I am raising two structural observations (the two-schema reconciliation risk and
the `serde_json::Value` boundary) and several minor ones — none rise to a structural
defect requiring a fix before acceptance, so this is not a "request revision".

---

## 1. Spine integrity — PASS

- **`qfs-types` is a true leaf.** `crates/types/Cargo.toml` declares only `serde` +
  `serde_json` (both workspace-pinned); zero `qfs-*` path deps. The two leaf-preserving
  moves are correct and consistent:
  - `DriverId` is defined *inside* `qfs-types` (`schema.rs:18`) as an owned newtype, not
    imported from `qfs-driver`. This is the right call for leaf status (see D2 verdict).
  - `SchemaSource::describe` takes `&[Name]` (a logical segment list) rather than the
    driver `Path`, so the trait surface does not pull `qfs-driver` upward into the leaf.
    The doc comment (`schema.rs:298-311`) names the boundary adaptation explicitly.
- **New edges are acyclic.** `qfs-codec → qfs-types` and `qfs-core → qfs-types` both
  point at a leaf, so no cycle is possible. `qfs-codec → qfs-driver` (D1) is untouched and
  orthogonal. ARCHITECTURE.md's spine diagram and the new D2 section reflect this exactly.
- **The dep-direction test locks it.** `dep_direction.rs::types_is_a_leaf_and_codec_depends_on_it`
  asserts (a) `qfs-types` depends on no workspace crate, (b) `qfs-codec` depends on it,
  (c) `qfs-core` depends on it. The binary-deps test was updated to include `qfs-types`
  in the workspace-crate set. This is a genuine mechanical guard, not a comment.

One **minor gap**: the leaf test enumerates the forbidden workspace crates by a
hardcoded list. When E1+ adds crates (`qfs-lang` schema use, future `qfs-eval`), the
list must be kept in sync or a new crate could depend *on* `qfs-types`'s position
incorrectly without tripping the test. Proposal: in a later ticket, derive the
workspace-crate set from `cargo metadata`'s `workspace_members` rather than a literal
array (applies to all three list-based tests in this file).

## 2. Translation fidelity — PASS, with one latent restructure risk

**Carries the t04 / E2 / E4 concepts faithfully:**
- Path/struct-nav from t04 (`a.b.c`) → `resolve_path` walks `Struct` without flattening
  (`schema.rs:205-215`), and descending into `Json`/`Unknown` yields `Unknown`
  (late-bound) rather than erroring — exactly RFD §4. `EXPAND` (`expand`, `schema.rs:229`)
  handles `Array(Struct)`, `Array(scalar)`, and `Struct` with the "same operator for mail
  attachments and JSON arrays" intent. The row/column model (`Value`/`Row`/`RowBatch`)
  the E4 drivers and E2 evaluator compute over is owned, positional, and serde-round-trips.
- The typed-predicate IR is deliberately the type model's *own* IR, distinct from the
  parser's `Expr` (predicate.rs:7-10). This is the correct fidelity boundary: it keeps
  the parser vendor-free and makes `qfs-types` the single home of typing rules. The
  planner lowering `Expr → Predicate` is correctly deferred to E2.

**Latent restructure risk — two schema notions coexist (raise to lead).**
There are now **two** schema types in the spine:

| | type | columns | extras |
|---|---|---|---|
| `qfs_driver::NodeSchema` | `driver/src/lib.rs:81` | `Vec<String>` (untyped names) | `archetype: Archetype` |
| `qfs_types::Schema` | `types/src/schema.rs:149` | `Vec<Column>` (typed, nested) | — |

And two `describe` seams that return different schema types:
- `Driver::describe(&Path) -> Result<NodeSchema, CfsError>` (driver/src/lib.rs:159)
- `SchemaSource::describe(&[Name]) -> Result<Schema, TypeError>` (types/src/schema.rs:310)

t05 left `NodeSchema` untouched (correctly — it is out of t05's scope, and its doc
already says "typed columns land in E1/E3"). But the reconciliation is now a **deferred
decision, not a resolved one**. When E4 wires real drivers, someone must decide whether:
1. `NodeSchema` *absorbs* `qfs_types::Schema` (its `columns: Vec<String>` becomes
   `Schema`, and `archetype` either moves onto `Schema` or sits beside it), collapsing to
   one schema notion + a separate archetype tag; or
2. the two stay distinct and an explicit `NodeSchema → Schema` adapter lives at the E4
   boundary (the `SchemaSource` impl), with `archetype` carried alongside.

This is a real fork. Leaving it open is acceptable *for t05* — forcing it now would pull
driver/archetype concepts into the leaf or invent an adapter with no caller. But it
should be **recorded as an explicit E1/E4 carry-over** so it is decided deliberately and
not discovered. There is a mild structural smell that `NodeSchema` carries `archetype`
(a driver-shaped concept) while `Schema` carries provenance (a `DriverId`) — provenance
already bridges toward the driver world, so option (1) with archetype-beside-Schema is
my structurally-preferred direction, but that is an E4 call. **Proposal:** add a one-line
carry-over to the E1/E4 ticket(s) naming "reconcile `NodeSchema.columns: Vec<String>`
with `qfs_types::Schema`; decide archetype placement" so the deferral is owned.

**D2 verdict — `DriverId`-in-types is the right call.** The alternative (a driver-owned
identity imported into types) would force `qfs-types → qfs-driver`, destroying leaf
status and the vendor-free guarantee — the inverse of what the type model needs.
`DriverId` is pure owned text carrying no capability/secret (RFD §10), so it has no
natural home in the driver contract that the type model couldn't equally own. The risk is
the mirror-image of the schema risk: if E4's driver registry also wants a `DriverId`-like
key, two identity types could diverge. That is far cheaper to reconcile than a broken
spine (a driver-side key can be a newtype over, or `From`-convert to, `qfs_types::DriverId`),
so D2 is sound. Worth one carry-over note that the driver mount key should reuse
`qfs_types::DriverId` rather than mint a parallel one.

## 3. Governance / G6 — PASS, one boundary choice to ratify

- **No vendor type leaks.** Every public type is owned (`String`, `Vec`, `i64`, etc.) or
  built from owned types. No driver SDK type, no `qfs-driver`/vendor handle in any public
  signature. `Provenance` records `DriverId` (owned text) + an optional source-column
  name — no token, no capability state (RFD §10). This satisfies G6 / boundary B3.
- **`serde_json::Value` as the `Json` carrier — acceptable, but it IS a third-party type
  in a public surface; flag it.** `Value::Json(serde_json::Value)` (value.rs:37) and
  `ColumnType::Json` are the RFD §4 escape for deeply-irregular data. Using `serde_json`'s
  `Value` is pragmatic and keeps `qfs-types` "serde-family only", and the Cargo.toml
  comment frames it as "a pure data tree, not I/O." I accept it, with the observation that
  it is technically a vendor (crate) type in `qfs-types`'s public API — the one place the
  otherwise-owned model exposes an external type. If `serde_json` ever needs replacing, or
  a non-JSON dynamic carrier is wanted, this leaks into every consumer's match arm.
  **Proposal (defer):** if strict ownership is later desired, introduce an owned
  `qfs_types::Json` tree (a thin sum type) and convert at the codec boundary; for now the
  pragmatic choice is fine and explicitly documented, so ratify it rather than churn.

## 4. Coercion / unify / comparability — coherent and total

- **`widen` (unify.rs:32-53) is total and panic-free.** Every `ColumnType` pair resolves:
  `Unknown` is bottom (widens to the other), `{Int,Float} → Float`, `Struct∨Struct` →
  recursive unify (with a defensive `Err → Json` fallback that is currently unreachable
  but harmless), `Array∨Array` → element-wise, identical → self, everything else → `Json`.
  No arm can panic; the `Box` recursion terminates on finite types. The module-level
  widening matrix doc matches the code.
- **`unify_schema` is total and matches the documented semantics** (missing column →
  nullable; `nullable = a||b OR types disagreed — note the disagreement-nullability is
  folded into `widen` returning `Json`, while the explicit `||` covers declared
  nullability). `merge_provenance` correctly drops to `None` on disagreement. Idempotence
  and commutativity-up-to-order are asserted by the property tests.
- **`comparable` (predicate.rs:184-211) is total.** `Unknown`/`Json` defer to runtime;
  `Match` is Text-only on both sides; numeric/temporal sets are handled; ordering vs
  equality split is correct (equality on identical scalars, ordering on a restricted
  orderable set). `Bytes`/`Bool`/`Uuid` ordering correctly falls through to incomparable.
  All rejections produce `TypeError::IncomparableTypes` with op + both types — no panic,
  AI-repairable.
- **`TypeError` is structured** (error.rs) with stable `code()` strings, `#[non_exhaustive]`,
  `Display`, and `std::error::Error`. Not stringly-typed; carries `available` columns on
  `UnknownColumn`. Matches the closed-core/structured-error policy from t04.

**Minor coherence observations (non-blocking):**
- **`unify` Json-fallback nullability.** When two scalar types disagree and widen to
  `Json`, the result is *not* forced nullable (only `a.nullable || b.nullable` applies).
  That is defensible (a `Json` cell still holds a value), but the unify.rs:69 comment says
  "Null appears … OR the types disagreed," which over-promises vs the code (the code does
  not set `nullable` on type disagreement). Proposal: either align the comment to the code,
  or decide that a widened-to-Json column should be nullable and set it — a one-line
  semantic choice worth making explicit, not a defect.
- **`Decimal`/`Uuid`/`Date` have no distinct `Value` variant** (value.rs carries them as
  `Text`/`Int`), and `conforms_to`/`type_of` reflect that (`type_of` can never *return*
  `Decimal`/`Uuid`/`Date`). This is a deliberate lexical-carrier choice and is documented,
  but it means `Value::type_of()` is lossy w.r.t. those `ColumnType`s — a `Row` typed as
  `Decimal` reports `Text` from `type_of`. Fine for the conformance check (which accepts
  `Text` for `Decimal`/`Uuid` and `Int` for `Timestamp`/`Date`), but E2 must not rely on
  `type_of` to recover the *declared* column type — it recovers the *value* type only.
  Worth a sentence in the E2 evaluator ticket.
- **`resolve_path` of an empty path returns `Struct(self.clone())`** (schema.rs:206-208).
  Reasonable ("the whole relation as an anonymous struct"), but undocumented as a public
  contract beyond the inline comment; an empty `ColRef.path` would hit it via
  `typecheck_predicate`. Low risk; consider asserting non-empty paths at the parser
  boundary in t04/E1.

## Cross-artifact coherence

ARCHITECTURE.md (spine diagram + new **Decision D2** section) faithfully describes the
implemented edges, the leaf rationale, the `DriverId`-in-types and `SchemaSource`-takes-
segments choices, and points at the enforcing test by name. The codec re-export
(`codec/src/lib.rs:26`) and core re-export (`core/src/lib.rs:38-42`) give the workspace one
`Schema`/`Value`/`TypeError` surface, matching the doc's "consumers depend on qfs-core
only" intent. Docs, code, and test agree.

## Summary of carry-overs for the lead

1. **Two-schema reconciliation (E1/E4):** record an explicit carry-over to reconcile
   `qfs_driver::NodeSchema.columns: Vec<String>` with `qfs_types::Schema`, and decide
   archetype placement. Latent restructure risk if discovered rather than decided.
2. **`DriverId` reuse (E4):** the driver mount/registry key should reuse
   `qfs_types::DriverId`, not mint a parallel identity.
3. **`serde_json::Value` Json carrier:** ratified for now; revisit only if strict
   ownership or a non-JSON carrier is later required.
4. **Minor:** align the unify nullability comment vs code; derive dep-test crate lists
   from `cargo metadata`; document `type_of` lossiness and empty-path resolution for E2.
