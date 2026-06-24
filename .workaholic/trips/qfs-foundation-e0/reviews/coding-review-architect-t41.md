# Coding Review (Architect) — t41 Google Analytics GA4 driver

- Author: Architect (Neutral)
- Status: under-review
- Target: t41 `qfs-driver-ga` (commit `40f5b7d`)
- Reviewed: `crates/driver-ga/src/{lib,path,catalog,compile,report,client,applier,error}.rs`, `crates/driver-ga/src/tests.rs`, `crates/cmd/tests/dep_direction.rs`, `crates/driver-ga/Cargo.toml`, `ARCHITECTURE.md`
- Method: analytical/structural review only (no test execution)

## Decision: Approve with minor suggestions

The driver is structurally sound. The headline invariant — residual truthfulness for
dimension/metric filters (the t20 defect class) — is correct and well-tested, and read-only
is genuinely enforced at both the capability gate and the applier. I raise one real but
narrow correctness gap (single-sided `date` bound) and two smaller observations, each with a
concrete fix. None rises to a wrong-rows defect or a write-path that can mutate GA, so this is
not a revision request.

---

## 1. Residual truthfulness (t20 defect class) — SOUND for filters

The core invariant holds. In `compile.rs`:

- `dim = 'x'` → `StringMatch::Exact`, residual **dropped** (`lower_cmp`, Eq/Text arm). Correct:
  GA `EXACT` stringFilter is exactly SQL equality.
- `metric {=,<,<=,>,>=} n` → `numericFilter` via `NumericOp::from_cmp`, residual **dropped**.
  Correct: GA numeric comparisons are exact. `Ne`/`Match` return `None` from `from_cmp` and
  fall to `add_residual` — correct, GA has no exact `<>` numeric op.
- `dim LIKE p` → `StringMatch::Contains`, predicate **KEPT** as residual (`lower_like`). Correct:
  CONTAINS is a substring superset of SQL LIKE glob semantics → over-fetch then re-filter.
- `dim ~ p` → `StringMatch::FullRegexp`, predicate **KEPT** as residual (`lower_cmp`, Match arm).
  Correct: GA's RE2 dialect differs from the engine's `~`, so the pushed filter is a loose
  pre-filter and the exact predicate is re-applied locally.
- `OR` / `NOT` / IN-on-date / BETWEEN-on-non-date → the `other =>` catch-all in
  `lower_predicate` keeps them wholly residual; `dimension_filter`/`metric_filter` carry
  nothing for them. Correct: GA's `andGroup`-only emission cannot express disjunction/negation.
- A comparison/LIKE on a column outside the catalog falls to `add_residual` (the `else` /
  `_` arms in `lower_cmp`/`lower_like`) rather than being silently pushed. Correct.

This is the same discipline the team applied in t20/t21, and tests
`loose_like_pushes_contains_but_keeps_residual`, `regex_match_pushes_full_regexp_but_keeps_residual`,
`or_predicate_stays_wholly_residual`, `exact_dimension_equality_drops_residual`, and
`metric_comparison_operators_map_exactly` pin it. No lossy GA filter can drop to a `None`
residual and return wrong rows. Verdict: **truthful**.

### Concern 1 (minor correctness) — single-sided `date` bound emits a degenerate range, drops residual

`lower_cmp`'s date branch handles `date >= a` / `date > a` / `date = a` (sets `start_date`,
and for `Eq` also `end_date`) and `date <= b` / `date < b` (sets `end_date`), each `return`ing
**without** adding to the residual. When the `WHERE` supplies only one side — e.g.
`WHERE date >= '2024-01-01'` with no upper bound — the compiler produces
`DateRange { start_date: "2024-01-01", end_date: "" }` and `residual: None`, then serializes
`{"startDate":"2024-01-01","endDate":""}` (`request_to_json`). GA4 rejects an empty `endDate`
with a 400, surfacing as an opaque `GaError::Api { op: "runReport", status: 400 }`.

Why this is *minor*, not a wrong-rows defect: the malformed window never returns rows — it
fails at the API. So it does not violate the t20 wrong-rows invariant. But it is two smaller
defects worth fixing: (a) the open-side `date` predicate is dropped from the residual even
though the emitted window does not faithfully represent it, and (b) the user gets an
un-self-correctable raw GA 400 instead of a structured, AI-recoverable error — exactly the
"turn a raw GA 400 into a structured qfs error" goal the module docs claim.

The `half_open_date_bounds_form_the_range` test only exercises the case where **both** bounds
are present; the single-sided case is untested.

Proposal (structural, preserves fidelity): after lowering, validate the assembled
`date_range` for a core report — if `start_date` or `end_date` is empty, either
(i) error with `GaError::MissingDateRange` (or a sibling `IncompleteDateRange`) so the AI adds
the missing bound, or (ii) fill the open side with a documented relative token
(`today` / GA's `NoT` sentinel) **and** keep the single-sided `date` predicate in the residual
so the half-open semantics are re-applied locally. Option (i) is the smaller, more honest
change and matches the existing `MissingDateRange` contract. Add a test for the single-bound
case either way.

---

## 2. Read-only enforcement — SOUND at both layers

- **Capability gate**: `GaDriver::caps_for` returns `Capabilities::from_verbs(&[Verb::Select])`
  for a concrete property/realtime node and `Capabilities::none()` for the root / invalid path.
  No write verb is ever present, so `qfs_driver::check_capability` rejects
  INSERT/UPSERT/UPDATE/REMOVE structurally before a `Plan` exists. `procedures()` returns an
  empty slice — no mutating procedure smuggles a write in. Test
  `select_is_allowed_writes_rejected_at_capability_gate` confirms `unsupported_verb`.
- **Applier**: `GaApplier` implements both `PlanApplier::apply` and `SharedApplier::apply_shared`
  via the single `reject` helper → every effect, regardless of kind, returns
  `GaError::ReadOnly`. The `From<GaError> for EffectError` maps `ReadOnly` to
  `EffectError::CapabilityDenied`. So even a hand-built plan that bypassed the gate cannot
  mutate GA. Test `applier_rejects_every_write_as_read_only` exercises both legs.

The two layers cannot drift because there is no write code path at all (no `client` write
method exists; the `client` field on the applier is `#[allow(dead_code)]`). Verdict:
**genuinely enforced; no plan can mutate GA**.

Observation (defensive, not a defect): `verb_label` maps `EffectKind::Read`/`List` to
`"READ"`/`"LIST"` and still rejects them as `ReadOnly`. That is correct for the *apply* leg
(reads flow through `report.rs`, never the applier), but the resulting message
"`cannot READ ... read-only`" reads oddly. Cosmetic only — consider a distinct reason for the
read kinds, or a comment that reads never legitimately reach the applier.

---

## 3. runReport correctness

- **dims/metrics split**: `compile` classifies each projected name via `catalog.is_dimension`
  then `is_metric`, erroring `UnknownField` otherwise. `orderBys` re-classify the same way.
  Correct and catalog-driven.
- **date → dateRanges**: mandatory for a core report (`MissingDateRange` when absent); a
  realtime report takes the empty-range branch and any date predicate stays in the residual
  rather than being silently dropped. Correct, modulo Concern 1.
- **filter trees**: `group()` collapses 0→None / 1→bare leaf / N→`AndGroup`; dimension and
  metric leaves are routed to the correct `dimensionFilter`/`metricFilter`. Correct.
- **ORDER/LIMIT pushdown**: `order_bys` carry the `is_metric` flag → `filter_to_json` emits the
  `metric`/`dimension` orderBy form; `limit` is serialized as a **string** (GA4 wire convention
  for int64). Correct.
- **wire serialization**: `request_to_json` builds a `serde_json::Map` (deterministic key
  insertion) and omits `dateRanges` entirely when `realtime` — matching
  `runRealtimeReport`'s schema. Filters/orderBys/limit are conditionally inserted. The golden
  `representative_report_compiles_to_correct_run_report_request` pins the value/plan. Correct
  and deterministic.

Observation: the `FilterTest::InList` variant exists and serializes (`inListFilter`), but the
compiler never emits it — `IN` falls to the residual catch-all (the module doc claims
`IN → inListFilter` as exact-droppable). Not a defect (residual is truthful), but the doc
overstates current behavior; either wire `Predicate::In` (a known exact mapping → droppable
residual, a real pushdown win) or soften the doc/`StringMatch`-adjacent comment to say IN is
currently residual. I recommend the former as a small follow-up since it is a clean exact map.

---

## 4. Token safety — SOUND

`qfs-driver-ga` holds no token. `GoogleApiGaClient` wraps `Arc<GoogleApiClient>` (t19), which
injects the bearer and refreshes on 401; this crate builds an `HttpRequest` with only a
`Content-Type` header and **never** an `Authorization` header. `GaError` arms carry only path /
verb / status / op / field-name / fixed reason; the `From<AuthError>` reduces an auth failure
to its stable `code` + `reauthorize` flag — no token, URL-with-query, or header value crosses.
The `Decode` arm explicitly never carries the body. Verdict: **no leak surface**.

`reqwest` confinement: `Cargo.toml` depends on `qfs-google-auth` + `qfs-http-core` only for
HTTP — **no `reqwest`, no `qfs-driver-http`**. ga rides the runtime-free `HttpExchange` seam
over the shared `qfs-http-core` DTOs, so reqwest stays in `qfs-driver-http` and ga stays a
runtime leaf. Verdict: **confined**.

## 5. Spine — SOUND

`qfs-driver-ga` depends on `qfs-runtime` (for `PlanApplierBridge`), so the generic leaf-
confinement check (`dep_direction.rs` test (b)) applies: nothing depends back onto ga, so tokio
dead-ends in this leaf. The named allowlist append (`"qfs-driver-ga"` at line 328) is the
single-line reviewable "intent" signal the test design intends; check (b) guarantees the append
was safe. Clean leaf; allowlist composes. Verdict: **clean**.

## 6. Honesty of parks — HONEST

The lib.rs "Named parks" section accurately scopes what is deferred vs done:
- **Sampling**: `ReportResponse.sampled` is decoded from `metadata.samplingMetadatas` and
  surfaced via `run_report`'s return tuple; a dedicated *marker column* and per-property quota
  accounting are honestly parked. Matches the code.
- **Quota**: `GaError::is_retryable` maps 429/5xx + `auth_network` to retryable so the t12
  runtime backs off; honest.
- **checkCompatibility**: catalog-driven `UnknownField` validation is present; the GA
  round-trip for incompatible *combinations* is honestly parked.
- **Service-account JWT**: the `analytics.readonly` OAuth path is wired; the SA path is parked
  to the t27 credential store.
- **pivot/batch reports**: out of scope per the ticket; not claimed.

The parks do not overclaim. (See §3 for the one place — `IN`/`inListFilter` — where the *doc*
slightly overstates current pushdown; that is a doc-vs-code nit, not a park dishonesty.)

---

## Summary

- Residual truthfulness (filters): **truthful** — no lossy GA filter drops the predicate.
- Read-only: **genuinely enforced at both layers**; no plan can mutate GA.
- One minor correctness gap: single-sided `date` bound emits a degenerate window and drops the
  residual (fails as an opaque GA 400, never wrong rows) — fix with range validation + a test.
- Two smaller observations: `IN`→`inListFilter` doc-vs-code mismatch; cosmetic read-kind verb
  label in the applier.

Decision: **Approve with minor suggestions**.
