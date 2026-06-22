# Coding E2E — Planner — t07 (Evaluator → effect-plan)

Author: Planner (Progressive)
Phase: Coding / review-and-testing
Target ticket: t07 — Evaluator → effect-plan (pure evaluation)
Method: external consumer harness (throwaway crate, own `[workspace]`, path-deps on
`crates/{core,parser,driver,types,plan}`, no production code, removed after the run).

## Harness shape

A fake out-of-crate `/mail` driver registered in a `MountRegistry`, with typed schemas:
- `/mail/inbox` — read-only relation (capabilities: `SELECT` only); schema `id:Int, subject:Text, body:Text`.
- `/mail/outbox` — writable relation (capabilities: `SELECT/INSERT/UPSERT/REMOVE`); schema `id:Int, to:Text, subject:Text`.
- a declared `send` procedure (irreversible).
- **`applier()` returns a PoisonedApplier whose `apply()` PANICS** — so any I/O during
  evaluation or `PREVIEW` would crash the harness.

Statements were parsed with `cfs_parser::parse_statement` and evaluated with
`cfs_core::Evaluator::new(&mounts).eval(&stmt)`. All 24 sub-checks passed; the poisoned
applier never fired.

## PASS/FAIL per check

| Check | Result | Evidence |
| ----- | ------ | -------- |
| C1 query → `EvalValue::Relation` | PASS | `FROM /mail/inbox \|> WHERE id > 1 \|> SELECT id, subject` evaluated to `Relation` |
| C1 projection columns | PASS | output columns `["id","subject"]` |
| C1 `id` typed `Int` (schema threaded from `describe`) | PASS | `id : Int` |
| C1 `subject` typed `Text` | PASS | `subject : Text` |
| C2 `INSERT … RETURNING` → `EvalValue::Plan` | PASS | evaluated to `Plan` |
| C2 plan has an `Insert` node | PASS | plan kinds `["INSERT"]` |
| C2 `RETURNING` columns | PASS | `RETURNING` schema `["id"]` |
| C2 `RETURNING` typed | PASS | `RETURNING id : Int` (typed against `/mail/outbox` describe) |
| C2 `PREVIEW` renders the insert, applies nothing | PASS | 1 preview row, `is_pure=false`, poisoned applier silent |
| C3 `REMOVE` → plan with `Remove` node | PASS | plan kinds `["REMOVE"]` |
| C3 `Remove` node carries `irreversible` | PASS | node `irreversible = true` |
| C3 `PREVIEW` flags irreversible | PASS | `preview.irreversible = [NodeId(0)]` |
| C3 `Plan::is_irreversible()` | PASS | `true` |
| C4 `INSERT … FROM (subquery)` → Read + write nodes | PASS | kinds `["READ","INSERT"]` |
| C4 write depends on Read (a DAG edge) | PASS | `deps = [(NodeId(0), NodeId(1))]` (read → write) |
| C5 unknown column → structured `EvalError` | PASS | code `unknown_column`, no panic |
| C6 unsupported verb → capability error BEFORE any plan | PASS | code `unsupported_verb`, returned by the resolver before plan build |
| C7 unrouted path → structured `EvalError` | PASS | code `unrouted_path`, no panic |
| C8 purity — no panic from poisoned applier | PASS | eval + preview of every effect plan; applier never invoked |
| C8 adversarial statements branchable, no panic | PASS | 6 adversarial statements (NOT/AND/`~`/LIMIT/DISTINCT/IN/EXPAND/UPSERT/BETWEEN/`SELECT *`) all total |

## Query output schema (C1)

```
QUERY OUTPUT SCHEMA: ["id:Int", "subject:Text"]
```

`SELECT id, subject` over `/mail/inbox` narrowed the 3-column described schema
(`id:Int, subject:Text, body:Text`) to the two projected columns and **carried their real
types** through `WHERE` and `SELECT` — proving schema threading reuses the driver's
`describe` types rather than re-deriving or erasing them.

## INSERT plan PREVIEW (C2)

`INSERT INTO /mail/outbox VALUES (1, 'a@b.c', 'hi') RETURNING id`:

```
PREVIEW: 1 effect(s)
  #0 INSERT -> mail:/mail/outbox [affected 1]
  total affected: 1
```

The plan carries a typed `RETURNING` schema (`id : Int`). `PREVIEW` rendered a dry-run
summary and applied nothing — the poisoned `applier()` was never called.

## Structured error dumps

Error #1 — unknown column in `SELECT` (`FROM /mail/inbox |> SELECT id, nonexistent_col`):

```
Type(UnknownColumn { name: "nonexistent_col", available: ["id", "subject", "body"] })
```

Branchable code `unknown_column`; the error helpfully lists the available columns for AI
recovery. No panic.

Error #2 — capability denial, gated **before** any plan
(`REMOVE /mail/inbox WHERE id = 1`, inbox is read-only):

```
Resolve(UnsupportedVerb { path: "/mail/inbox", verb: "REMOVE", supported: ["SELECT"] })
```

Branchable code `unsupported_verb`. It surfaces as a `Resolve(...)` error because
`Evaluator::eval` runs the t06 resolve-time capability gate first and returns the error;
`eval()` never produces an `EvalValue`, so **no plan is ever constructed** for a denied
verb. The error names the rejected verb and the supported set (`["SELECT"]`).

(Unrouted-path control, C7: `UnroutedPath { path: "/nope/whatever" }`, code `unrouted_path`.)

## Concern + proposal (Critical Review Policy)

**Concern (business-traceability of the `RETURNING`/effect-target schema typing).** The
typed `RETURNING id:Int` only holds because `/mail/outbox` returns a real schema from
`describe`. Per the evaluator's `describe_schema` helper, a node whose driver **cannot**
`describe` (returns an error) degrades silently to an *empty* schema, and `RETURNING`/
projection over it would then synthesise `Unknown`-typed columns rather than error. For a
real driver mid-rollout (a node that exists but is not yet describable), an AI caller could
receive a confidently-shaped-but-untyped `RETURNING` with no signal that typing was
late-bound. This is correct for t07's "keep the evaluator total" intent, but it is a
business-observability gap: the consumer cannot distinguish "typed against a real schema"
from "fell back to empty/Unknown."

**Proposal.** No change to t07 is required to ship (the fallback is the intended,
documented behaviour and all in-scope acceptance criteria pass). For a downstream ticket,
surface the late-binding in `PREVIEW`/`DESCRIBE` output — e.g. a per-column or per-relation
`provenance`/`late_bound` marker the AI envelope can branch on — so a `RETURNING` derived
from an empty fallback schema is visibly distinguishable from one typed against a real
`describe`. This preserves totality while restoring traceability of *why* a column is
`Unknown`.

## Verdict

**E2E approved.** All in-scope t07 behaviour validated from the outside: query-side schema
threading (`id:Int, subject:Text`), effect plans for `INSERT`/`UPSERT`/`REMOVE` with the
right node kinds, typed `RETURNING`, irreversible flagging surfaced by `PREVIEW`, the
`INSERT … FROM (subquery)` Read→write DAG edge, three distinct structured error codes
(`unknown_column`, `unsupported_verb` gated pre-plan, `unrouted_path`), and the purity
invariant — the poisoned `applier()` never fired during evaluation or `PREVIEW`, and no
adversarial statement panicked.
