---
created_at: 2026-06-22T22:19:51+09:00
author: a@qmu.jp
type: enhancement
layer: [Infrastructure, Domain]
effort:
commit_hash: 825f64a
category: Added
depends_on: [20260622214650-t13-driver-contract-trait.md, 20260622214650-t19-driver-google-oauth-multi-account.md]
---

# Driver: Google Analytics (GA4 Data API)

## Overview

A **read-only relational driver** exposing Google Analytics 4 as queryable relations under
`/ga/<propertyId>/...`. GA4's Data API (`properties.runReport` / `runRealtimeReport` /
`batchRunReports`) is fundamentally a *query* surface — you ask for **metrics** grouped by
**dimensions** over a **date range** with **filters**, and GA aggregates server-side and
returns rows. That maps onto the qfs relational archetype with one honest constraint: GA is a
**query source, never a mutate target** (you do not `INSERT`/`UPDATE`/`REMOVE` analytics data).
Like the SQL driver, the *entire* pipeline pushes down to one native `runReport` call
(GA does the aggregation), so this is a **pushdown target** (RFD §5 relational archetype, §6
pushdown). It reuses the shared Google OAuth (t19) and the driver contract (t13).

## Scope

In scope:
- `/ga/<propertyId>` relational source: pipe-SQL `FROM … |> WHERE … |> SELECT <dims>,<metrics>
  |> ORDER BY … |> LIMIT …` compiled to a `runReport` request.
- `/ga/<propertyId>/realtime` mapped to `runRealtimeReport` (last ~30 min, limited dims/metrics).
- `DESCRIBE /ga/<propertyId>` → the property's dimension + metric catalog via the
  **Metadata API** (`properties.getMetadata`), including custom dimensions/metrics — the AI's
  introspection surface.
- **SELECT-only capability**: write verbs are rejected at parse time (capability gating).
- Sampling-metadata surfacing and quota-aware execution.

Out of scope (deferred):
- The GA **Admin API** (managing accounts/properties/data-streams) → a future ticket; this
  driver is reporting-only.
- `runPivotReport` pivots and `batchRunReports` fan-out → optional follow-up; v1 = single
  `runReport` + realtime.
- General pushdown planner mechanics → **t14**; here we only *declare* full pushdown via the
  driver contract.
- Shared OAuth/token plumbing → **t19**; credential storage → **t27**.

## Key components

- New crate/module `driver/ga` implementing the `Driver` trait (t13).
- **Query→report compiler** (pure): `RelExpr -> RunReportRequest`. Maps dimensions to
  `dimensions[]` (group-by/select), metrics to `metrics[]`, a mandatory `date` predicate to
  `dateRanges[]`, dimension/metric predicates to `dimensionFilter`/`metricFilter`
  (`FilterExpression` trees), `ORDER BY` to `orderBys[]`, `LIMIT` to `limit`.
- **Catalog/schema** from `getMetadata` → owned DTOs (`GaDimension`, `GaMetric`, type/category),
  cached per property; powers `DESCRIBE`. Optional `checkCompatibility` to validate a
  dimension×metric combination and return a structured error rather than a runtime API 400.
- **Owned response DTOs** decoding `runReport` rows (`dimensionValues`/`metricValues`) into typed
  qfs rows — GA SDK/JSON types never leak past the driver boundary.
- **Capability declaration**: `SELECT` only; `INSERT/UPSERT/UPDATE/REMOVE` absent (parse-time
  rejection). No mutating procedures.
- Pushdown declaration: the whole relational subtree over a `/ga` node is executable by the
  driver (one `runReport` per query).
- Auth adapter over t19 (scope `analytics.readonly`) **plus a service-account path** for
  unattended/server use.

## Implementation steps

1. Module scaffold + `Driver` impl registering the `/ga` mount and the relational archetype.
2. Metadata/catalog: `getMetadata` client → catalog DTOs → `DESCRIBE`; cache per property.
3. Query→`RunReportRequest` compiler with: required date-range handling, dimension/metric
   resolution against the catalog, filter-expression translation, order/limit.
4. `runReport` client + response decoding into typed rows (dimension columns + metric columns,
   typed from the catalog: integer/float/currency/percent).
5. `/ga/<property>/realtime` → `runRealtimeReport` (restricted catalog).
6. Sampling: surface `metadata.samplingMetadatas` (and a `sampled` marker column or query note)
   so consumers know results are estimates.
7. Quota handling: respect GA4 token-bucket quotas; exponential backoff on quota-exhaustion
   (`RESOURCE_EXHAUSTED`); reuse the runtime's retry/circuit-breaker (t12).
8. Auth: wire OAuth (t19, `analytics.readonly`) and a service-account credential path via t27.
9. Capability gating + parse-time rejection of write verbs; clear errors for missing date range
   and incompatible dimension×metric combos.

## Considerations

- **Read-only is the honest archetype** (design/modeless, least-privilege): never pretend GA is
  writable; least-privilege scope is `analytics.readonly`. Don't request broader Google scopes.
- **Mandatory date range**: GA requires a date range. The compiler must require a `date`
  predicate (or apply a documented default window) and error clearly if absent — surprising
  for an AI otherwise.
- **Dimension/metric compatibility**: not every combo is queryable. Prefer a pre-flight
  `checkCompatibility` (or catalog-driven validation) returning a structured qfs error over a
  raw GA 400, so the agent can self-correct.
- **Sampling** (operation/observability): high-cardinality/large queries are sampled; surfacing
  sampling metadata is required so downstream decisions aren't made on estimates silently.
- **Quotas** (operation): GA4 enforces per-property token quotas; the driver must back off and
  report, not hammer — this is the main reliability risk for scheduled server reports.
- **Service-account auth for the server**: scheduled GA reports in a `CREATE JOB` (watchtower)
  should run under a service account, not an interactive OAuth token; design the credential
  store (t27) to hold both.
- Owned DTOs only; GA SDK types never cross the boundary.

## Acceptance criteria

- `DESCRIBE /ga/<property>` returns the dimension+metric catalog (tested against fake
  `getMetadata` JSON — no live creds).
- A representative pipe-SQL report (dimensions, metrics, `WHERE date BETWEEN …` + a dimension
  filter, `ORDER BY`, `LIMIT`) compiles to the **correct `RunReportRequest`** — asserted as a
  *value/plan* (golden test), not by hitting the API.
- Write verbs (`INSERT/UPSERT/UPDATE/REMOVE`) against `/ga/...` are rejected at parse time with
  a capability error.
- Missing date range and incompatible dimension×metric combos produce clear structured errors.
- `runRealtimeReport` mapping for `/ga/<property>/realtime`; sampling metadata surfaced; quota
  backoff path covered by a test.
- `cargo build`, `clippy`, and tests green; no live credentials required in the test suite.
