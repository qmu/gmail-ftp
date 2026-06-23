# Coding E2E (Planner) — t41 Google Analytics GA4 driver

Author: Planner (Progressive)
Phase: Coding — E2E / external testing
Target: t41 — `cfs_driver_ga` (GA4 read-only relational driver)
Method: throwaway external-consumer crate (`/tmp/ga-e2e`, own `[workspace]`, path-deps on
driver-ga + runtime + driver + plan + types + secrets, `test-util` feature enabled). Mocked
`MockGaClient` only — **no live GA, no network, no credentials**. Crate removed after the run.

## Verdict: E2E approved

All five items PASS. No GA mutation occurred (`ga_mutated=false`); no token leaked
(`token_leaked=false`). Neither BLOCKING condition (a token leak or ANY successful GA mutation)
was triggered.

| Item | Result |
| ---- | ------ |
| 1. runReport → typed rows | PASS |
| 2. Pushdown residual (lossy keeps / exact drops) | PASS |
| 3. Read-only enforcement (BLOCKING) | PASS |
| 4. Multi-account / property | PASS |
| 5. Token safety + adversarial robustness | PASS |

---

## Item 1 — runReport → rows: PASS

Query (external consumer): `SELECT country, sessions, totalRevenue` over `/ga/123456789`
`WHERE date BETWEEN '2024-01-01' AND '2024-01-31' AND country = 'JP' AND sessions > 100`
`ORDER BY sessions DESC LIMIT 10`, compiled via `compile(property, false, catalog, spec)`.

The compiled `RunReportRequest` (asserted as a value/plan, never by hitting the API):

```
RunReportRequest {
    property_id: "123456789",
    realtime: false,
    dimensions: ["country"],
    metrics: ["sessions", "totalRevenue"],
    date_ranges: [DateRange { start_date: "2024-01-01", end_date: "2024-01-31" }],
    dimension_filter: Some(Filter { field_name: "country",
        test: String { value: "JP", match_type: Exact } }),
    metric_filter: Some(Filter { field_name: "sessions",
        test: Numeric { op: GreaterThan, value: "100" } }),
    order_bys: [OrderBy { field_name: "sessions", desc: true, is_metric: true }],
    limit: Some(10),
}
residual: None
```

- dimensions[] / metrics[] split correctly from the projection by the catalog.
- dateRanges[] carries the mandatory window.
- dimension filter = EXACT stringFilter `country='JP'`; metric filter = GREATER_THAN
  numericFilter `sessions>100`; orderBys + limit present.

Response decode through the driver read path (`GaDriver::run_report`) over a seeded mock
response yields typed rows, **typed by metric kind**:
- `country` (dimension) → `Value::Text("JP")`
- `sessions` (Integer) → `Value::Int(1234)`
- `totalRevenue` (Currency/Decimal) → `Value::Text("99.50")` (lossless decimal lexical form)
- `bounceRate` (Float) → `Value::Float(0.42)` (separate query)
- `sampled` flag correctly `false` for an un-sampled response.

## Item 2 — Pushdown residual: PASS

- `pagePath LIKE '/blog'` → pushed as `StringMatch::Contains` pre-filter, residual **KEPT**:
  `residual = Some(Like(ColRef{path:["pagePath"]}, Pattern("/blog")))`.
- `pagePath ~ '^/blog'` (regex) → pushed as `StringMatch::FullRegexp` pre-filter, residual
  **KEPT**: `residual = Some(Cmp(pagePath, Match, "^/blog"))`.
- Exact `country = 'US'` → residual **dropped** (`None`).
- Exact `sessions > 5` (metric) → residual **dropped** (`None`).

Evidence: lossy filters produce `Some(...)` so the engine re-applies the exact predicate
locally (over-fetch then filter → results stay exact, never wrong rows); exact mappings drop
the residual.

## Item 3 — Read-only enforcement (BLOCKING): PASS

Three independent enforcement points, all reject; GA is never mutated.

1. **Parse-time capability gate** (`cfs_driver::check_capability`): `SELECT` allowed;
   `INSERT/UPSERT/UPDATE/REMOVE` each rejected with code `unsupported_verb`.
2. **Applier (hand-built mutating effect)**: `GaApplier` `PlanApplier::apply` rejects
   `Insert/Update/Remove`, e.g.
   `apply #0 failed: capability denied: /ga is read-only; cannot INSERT at "/ga/123"`.
   The shared async-bridge leg (`SharedApplier::apply_shared`) rejects with code
   `capability_denied`.
3. **End-to-end through the async interpreter** — the strongest test: a hand-built mutating
   `Plan::leaf(UPDATE /ga/987654321)` registered under the `ga` bridge and committed with
   `CapabilitySet::allow_all()` (so the runtime cap-gate does **not** pre-block — the applier
   itself must reject). The interpreter ledger entry:

```
LedgerEntry { id: NodeId(0), driver: "ga", kind: Update, irreversible: false,
    status: Failed { error: CapabilityDenied { driver: "ga",
        verb: "UPDATE at \"/ga/987654321\"" }, attempts: 1 } }
```

`failed_count() == 1`, `applied_ids()` empty → GA never mutated. The driver declares **no**
mutating procedures (`procedures()` empty). No write path exists.

## Item 4 — Multi-account / property: PASS

Two independent account clients (each its own `MockGaClient`) over properties `111111111` and
`222222222`. The mock records the exact calls:
- Client A: `GetMetadata{property_id:"111111111"}` then `RunReport{request.property_id:"111111111"}`.
- Client B: `GetMetadata{property_id:"222222222"}` then `RunReport{request.property_id:"222222222"}`.
- Each account client saw **only** its own property (2 calls each, no cross-talk) — the right
  property id and the right account client are used for each query.

## Item 5 — Token safety + adversarial robustness: PASS

Token-absent proof:
- A canary token (`ya29.SECRET-CANARY-TOKEN-zzz`) planted as a `cfs_secrets::Secret` is absent
  from both `Debug` (`Secret(***redacted***)`) and `Display` (`***redacted***`); the `REDACTED`
  marker is present. A leak would require an explicit `.expose()`.
- Every `GaError` variant (`read_only`, `missing_date_range`, `empty_projection`,
  `unknown_field`, `api_status`, `decode`, `invalid_path`) — scanning combined `Debug || Display`
  — contains no `ya29` / `bearer` / `canary` / token-ish content. Errors are secret-free by
  construction (`From<AuthError>` reduces to a stable code + reauthorize flag only).

No panics on adversarial queries — all return structured errors:
- missing date range → `missing_date_range`
- unknown field → `unknown_field`
- empty projection → `empty_projection`
- response arity mismatch → `decode` (no panic)
- malformed path `/ga/123/bogus/extra` → `invalid_path` (no panic)

---

## Concern + proposal (Critical Review Policy)

**Concern (business/observability, non-blocking):** sampling is surfaced only as a boolean
`sampled` flag on the read path. For an AI/agent consumer making downstream decisions, a bare
boolean does not convey *how* sampled (sampling read/space sizes) nor attach to the result rows,
so an agent could under-react to a heavily-estimated report. The ticket itself names a dedicated
sampling marker *column* / per-property quota accounting as a deferred follow-up, and the doc
comments record it as a named park — so this is expected scope, not a t41 defect.

**Proposal:** in the follow-up that lands the sampling marker column, surface the GA
`samplingMetadatas` ratio (samples-read / samples-space) as a typed result-set annotation or a
`_sampled`/`_sampling_ratio` column, so the agent can branch on the *degree* of estimation, not
just its presence. This preserves the t41 read-only archetype and the no-vendor-leak boundary
(still owned DTOs) while improving the business outcome (decisions not made on silent estimates).

## Notes / boundary confirmations

- Validated entirely as an external consumer through the public `cfs_driver_ga` surface; no
  production code modified, no internal/unit tests run, no code review performed (Planner QA
  domain = E2E/external only).
- The throwaway crate (`/tmp/ga-e2e`) was built, run, and removed; no live GA, no network.
