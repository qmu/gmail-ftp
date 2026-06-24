# Coding E2E Review (Planner) — t05 Type & Schema Model

- Author: Planner
- Phase: Coding / review-and-testing
- Target: t05 — Type & schema model (`qfs-types`)
- Method: external consumer. A throwaway crate in `/tmp/t05-e2e` (own `[workspace]`,
  path-dep on `crates/types`, plus a compile-only check against the `qfs-core`
  re-export). PUBLIC API only, no production code added. Crate removed after the run.
- Verdict: **E2E approved**

## What was exercised

31 assertions across the four required items, all run from outside the crate via the
public surface (`Value, Row, RowBatch, Schema, Column, ColumnType, DriverId,
Provenance, Predicate, ColRef, CmpOp, Literal, Pattern, TypeError`,
`typecheck_predicate`). The same symbol set was confirmed reachable through the
`qfs_core` re-export (compile-only) — the path real consumers (CLI `-json`, evaluator,
drivers) will use.

## PASS / FAIL per item

### Item 1 — schema build, `type_of`, row-vs-schema conformance: **PASS**
- Built a schema with scalars (`Int`, `Text`), a nested `Struct` (`address{city,zip}`),
  and an `Array(Text)` column; built a conforming `Row` and a 2-row `RowBatch`.
- `Value::type_of`: `Int→Int`, `Text→Text`, `Null→Unknown` (null carries no type),
  `Array[Text]→Array(Text)`, empty `Array→Array(Unknown)` — all as documented.
- Conformance: conforming row passes; `Null` in nullable columns passes; `Null` in a
  non-nullable column is **rejected**; wrong-arity row is **rejected**;
  `RowBatch::is_conformant()` is true. Matches ticket step 3.

### Item 2 — schema algebra (`resolve_path` / `project` / `expand` / `unify`): **PASS**
- `resolve_path(a.b.c)` returns the nested `Int`. Unknown segment → `unknown_column`;
  navigating into a scalar (`name.x`) → `not_a_struct`.
- `project([name,id])` preserves requested order; unknown column → `unknown_column`
  carrying the available-names list.
- `expand(Array(Struct))` flattens element columns in place
  (`msg_id, filename, size`); `expand(Array(Text))` yields a single `Text` column;
  expanding a scalar (`id`) → `not_expandable`.
- `unify`: numeric widening `Int∨Float⇒Float`; side-exclusive columns promoted to
  `nullable` and both retained; irreconcilable `Text∨Bool⇒Json` (no error, het.
  tolerance); recurses into `Struct` (inner `Int∨Float⇒Float`) and into `Array`
  element (`Array(Int)∨Array(Float)⇒Array(Float)`); `unify(a,a)==a` (idempotent).

### Item 3 — predicate typechecking: **PASS**
- Accepts `Int = 5`, `Int < 10`, `name LIKE 'a%'`.
- Rejects `Int < Text` → `incomparable_types` with branchable fields
  (`op=Lt, lhs=Int, rhs=Text`); rejects `LIKE` on `Int` → `incomparable_types`.
- Predicate on a missing column → `unknown_column`.
- Adversarial/empty schema: no panics — `typecheck_predicate`, `project`, and
  `expand` all return structured errors; `resolve_path(&[])` returns the anonymous
  whole-relation struct without panicking.

### Item 4 — serde JSON round-trip (the `-json` output path): **PASS**
- `Schema`, `RowBatch`, and `TypeError` each serialize to JSON and parse back equal
  (`== original`). Confirms the `-json` output path is end-to-end serializable.

## Sample JSON

### Schema
```json
{
  "columns": [
    { "name": "id", "ty": "Int", "nullable": false,
      "provenance": { "driver": "sql", "source_col": "user_id" } },
    { "name": "name", "ty": "Text", "nullable": false,
      "provenance": { "driver": null, "source_col": null } },
    { "name": "address", "ty": { "Struct": { "columns": [
        { "name": "city", "ty": "Text", "nullable": false,
          "provenance": { "driver": null, "source_col": null } },
        { "name": "zip", "ty": "Int", "nullable": true,
          "provenance": { "driver": null, "source_col": null } }
      ] } }, "nullable": true,
      "provenance": { "driver": null, "source_col": null } },
    { "name": "tags", "ty": { "Array": "Text" }, "nullable": true,
      "provenance": { "driver": null, "source_col": null } }
  ]
}
```

### RowBatch (rows abbreviated; schema as above)
```json
{
  "schema": { "...": "same Schema as above" },
  "rows": [
    { "values": [
        { "Int": 7 },
        { "Text": "alice" },
        { "Struct": { "values": [ { "Text": "kyoto" }, { "Int": 606 } ] } },
        { "Array": [ { "Text": "a" }, { "Text": "b" } ] }
    ] },
    { "values": [ { "Int": 1 }, { "Text": "bob" }, "Null", "Null" ] }
  ]
}
```

## Structured `TypeError` dumps (code + Display + Debug)

1. `unknown_column` (from `project(["ghost"])`), also shown serde-serialized:
```
code="unknown_column"
display="unknown column `ghost`; available: ["id", "name", "address", "tags"]"
debug=UnknownColumn { name: "ghost", available: ["id", "name", "address", "tags"] }
json={"UnknownColumn":{"name":"ghost","available":["id","name","address","tags"]}}
```

2. `incomparable_types` (from `typecheck_predicate(Int < Text)`):
```
code="incomparable_types"
display="incomparable types for Lt: Int vs Text"
debug=IncomparableTypes { op: Lt, lhs: Int, rhs: Text }
```

3. (additional, for coverage) `not_a_struct` (from `resolve_path(["name","x"])`):
```
code="not_a_struct"
display="cannot navigate into `name`: not a struct (Text)"
debug=NotAStruct { segment: "name", ty: Text }
```

4. (additional) `not_expandable` (from `expand("id")`):
```
code="not_expandable"
display="cannot EXPAND `id`: not a collection (Int)"
debug=NotExpandable { field: "id", ty: Int }
```

All codes are stable (`&'static str`) and the variants carry branchable context
(available names, op, lhs/rhs) — AI-consumable per RFD §5.

## Concern + proposal (Critical Review Policy)

- **Concern (business/observability, non-blocking):** `Provenance.driver` serializes as
  a bare string (`"driver": "sql"`) because `DriverId` is a transparent newtype, while
  `ColumnType` uses externally-tagged objects (`{"Struct": …}`, `{"Array": …}`). The
  `-json` contract is internally consistent and round-trips, so this is cosmetic, not a
  defect. The risk is purely downstream: an external tool consuming the `-json` schema
  must know `driver` is a scalar string, not a tagged object.
- **Proposal:** when the `-json` schema becomes a published external contract (CLI
  `DESCRIBE --json`), pin it with a golden snapshot and a one-line note in the output
  docs that `provenance.driver` is a plain string. No code change needed in t05; this
  is a documentation/contract-freeze action for the CLI epic that surfaces it.

## Overall

All four items pass with zero failures and zero panics across valid, error, and
adversarial/empty inputs; the model is fully serde round-trippable; the public surface
is reachable through `qfs_core`. **E2E approved.**
