# Round t24 — Planner E2E / External-Interface Review

Author: Planner (Progressive)
Reviewed: t24 GitHub driver — Constructor commit `43008e9`, Architect approval `8ce944a`
Charter: `.workaholic/tickets/todo/a-qmu-jp/20260622214650-t24-driver-github.md`
QA domain: E2E / external-interface (black-box). NOT code review, NOT the Constructor's unit tests.
Status: under-review → **Approve with minor suggestions (7/7 acceptance scenarios validated; one non-blocking wasm-park observation, consistent with accepted t22/t23)**

## How I validated (black-box, from the outside)

I authored a Planner-owned external integration crate, `crates/driver-github/tests/e2e_blackbox.rs`
(14 scenarios), that drives the driver ONLY through its public surface — the `Driver` contract
(`describe`/`capabilities`/`procedures`/`pushdown`), the public plan→`preview`→`commit` loop, the
public `pushdown`/`read` lowering, and a Planner-owned recording HTTP transport (`WireTap`) that
answers from scripted wire responses. No live GitHub, no live PAT, no network — exactly the
mock/temp posture used for t18/t20/t21/t41/t22/t23. A planted canary PAT
(`ghp_PLANTED_CANARY_e2e_must_never_leak_0xCAFE`) lives only in an in-memory secret store.

Results:
- `cargo test -p qfs-driver-github --test e2e_blackbox` → **14/14 pass**.
- Full crate suite (Constructor's 31 in-crate unit tests + my 14 external) → **green**.
- `cargo clippy -p qfs-driver-github --tests -- -D warnings` → **clean**.
- `cargo fmt -p qfs-driver-github -- --check` → **clean**.

## Scenario-by-scenario (maps to acceptance criteria)

**1. DESCRIBE for all 8 namespaces** — `s1_describe_returns_declared_columns_for_all_eight_namespaces`.
Asserted the EXACT column set AND exact column count (no missing, no invented) for issues, pulls,
comments, reviews, runs, releases, files, branches; archetype `ObjectGraphWorkflow` for each; a
sub-collection (`issues/123/comments`) describes with the SUB schema (comment columns, not issue);
the bare repo root returns an honest structured `invalid_path`. **PASS.**

**2. Plan-shape goldens through PREVIEW (no creds)** — `s2_*`.
- `INSERT INTO .../issues/123/comments` previews as a single `INSERT` node at the right path, not
  irreversible (`s2_insert_comment_previews_as_single_insert_node`).
- `UPDATE .../issues/123 SET state='closed'` previews as `UPDATE` and decodes to a **state-only**
  PATCH (title/body/labels all `None`) — `s2_update_state_only_previews_as_update_and_decodes_to_state_only_patch`.
- `CALL github.merge(method=>'squash')` and `CALL github.dispatch(workflow=>'ci.yml', ref=>'main')`
  preview as irreversible `CALL` nodes; `CALL github.review(event=>'APPROVE')` previews as a
  reversible `CALL` — `s2_s7_merge_and_dispatch_preview_as_irreversible_calls_review_is_not`.
- Endpoint correctness driven to the wire (`s2_call_procedures_hit_the_right_endpoints_on_the_wire`):
  merge → `PUT .../pulls/7/merge`; dispatch → `POST .../actions/workflows/ci.yml/dispatches`;
  review → `POST .../pulls/7/reviews`. **PASS.**
- The SELECT pushdown plan-shape is covered jointly under scenario 4/5 (params + residual).

**3. Capability gating AT PARSE TIME** — `s3_update_on_runs_rejected_at_parse_time_with_structured_error`.
`check_capability(runs/55, Update)` fails with structured `UnsupportedVerb { path, verb:"UPDATE",
supported:["SELECT"] }` BEFORE any plan exists; the mock client recorded **zero** calls (the gate
rejects before any I/O). Cross-checked the full node-keyed verb matrix (issues, comments, runs,
files, reviews) from the outside. **PASS.**

**4. Pagination** — `s4_paginated_select_is_one_plan_node_and_follows_link_at_the_wire`.
(a) Plan shape: a paginated `SELECT … WHERE state='open' AND label='bug'` is ONE `ReadPlan` node
carrying the pushed params (the page follow lives at the edge). (b) Wire level: a two-page Link
response (`page1` carries `rel="next"`, `page2` does not) is followed correctly — the client issues
exactly two fetches and merges both pages into one result set; req[1] carries `page=2`. **PASS.**

**5. Pushdown residual truthfulness (the recurring trap — I tried hard to break it)** — `s5_*`.
For `WHERE state='open' AND label='bug'`: `state` is EXACT (pushed `state=open`, dropped from
residual); `label` is a LOSSY set-membership pre-filter (pushed `labels=bug` AND kept as residual).
I then EXECUTED the no-wrong-rows contract: the mock **over-returns** four rows (two real bug
issues, plus two near-matches that the `labels` pre-filter would let slip — issues NOT carrying
`bug`). Re-applying the driver's reported residual locally re-filtered to **exactly** `[1, 4]` — the
two near-match rows were correctly dropped, no wrong rows survived. `s5b` confirms an `OR` and a
non-listable namespace (comments) push NOTHING and keep the whole predicate residual, so nothing is
ever silently pushed in a way that would drop correct rows. **PASS.**

> Harness note (transparency): my residual evaluator initially returned `[]` because the residual
> predicate references the scalar SQL column `label` while the decoded row schema exposes the
> `labels` ARRAY. This is not a driver defect — it is precisely the `label`→`labels` membership
> re-check the driver's `pushdown.rs` module doc specifies the engine must perform. I corrected my
> evaluator to model that exact mapping; the residual then filtered correctly. This actually
> strengthens the finding: the residual is truthful AND its column semantics are documented.

**6. Token safety** — `s6_*`.
- Canary PAT in NO serialized plan or preview (`s6_canary_pat_never_appears_in_a_serialized_plan_or_preview`):
  even a plan whose args carry user text serializes token-free; no `Bearer` either.
- Bearer redaction at the wire (`s6_bearer_redaction_holds_at_the_wire_...`): the PAT IS present as
  a real `Authorization: Bearer …` header value on the wire (it must authenticate), but the
  redacting request `Debug` NEVER reveals it — the only log surface an operator would print shows
  the `***redacted***` placeholder.
- Errors carry no token material (`s6_errors_carry_no_token_material`): a 401 surfaces a structured
  `github_api` error with no PAT and no `Bearer`.
- POST never silently retried (`s6_write_post_is_never_silently_retried_even_on_a_500`): a 500 on a
  comment POST is issued exactly once (at-least-once contract); contrasted with a transient 429 on a
  GET, which IS retried — proving the asymmetry is real, not accidental. **PASS.**

**7. Irreversibility surfacing in PREVIEW** — covered by `s2_s7_...`: merge and dispatch surface as
irreversible in the preview rows AND the `Preview.irreversible` node list AND the human `Display`
`(!)` marker; review does not. The declared `ProcSig.irreversible` agrees with the plan-node flag.
**PASS.**

Plus an end-to-end COMMIT through the public interpreter + bridge
(`e2e_commit_post_comment_through_the_public_interpreter`): a real plan routed to `/github` applies
the decoded `PostComment` effect through `Interpreter::commit` under a granted capability. **PASS.**

## Concern / observation (non-blocking) — the `cargo build --target wasm32-unknown-unknown` line

`cargo build -p qfs-driver-github --target wasm32-unknown-unknown --lib` does NOT build standalone:
`qfs-driver-github → qfs-runtime → tokio (rt-multi-thread)` triggers tokio's
`compile_error!("Only features sync,macros,io-util,rt,time are supported on wasm.")`.

This is NOT a t24 regression and NOT a behavior I can exercise as a defect:
- It is **systemic and pre-existing**: I verified `qfs-driver-s3` (t22) and `qfs-driver-cloudflare`
  (t23) — both already **accepted by the Lead** — fail the wasm `--lib` build **identically**, for
  the same `qfs-runtime → tokio[rt-multi-thread]` reason.
- It is **explicitly disclosed** in t24's own `lib.rs` "Named parks" (live wire + entrypoint parked
  for t38) and the crate trades only in `wasm32`-safe DTOs at its own boundary.
- It is the **same resolution the team already adopted** for t22/t23 (my own accepted t22 Planner
  E2E review raised the identical observation with the identical proposal).

**Proposal (business-outcome framed)**: honor the `--target wasm32` acceptance line at the
**composition root**, not the leaf — file a t38 carry-over for a thin wasm Workers entrypoint crate
that composes the driver modules WITHOUT the tokio runtime bridge (shared with the t22/t23
entrypoint work). Recording this carry-over with an owner keeps the wasm promise traceable for the
stakeholder (an AI agent operating GitHub from a Worker) rather than leaving it an unstated
divergence. This is consistent with how t22 and t23 were accepted; t24 should inherit it the same
way.

## Business-outcome assessment

The ticket's reason to exist — letting an AI agent operate GitHub through the same
`DESCRIBE → statement → PREVIEW → COMMIT` loop it uses everywhere else — is delivered and observable
from the outside: all eight namespaces DESCRIBE with typed columns; universal CRUD lowers to the
right REST verbs/endpoints; the three irreducible transitions (`merge`/`dispatch`/`review`) are
`CALL` procedures with honest irreversibility surfaced in PREVIEW; capability gating rejects
unsupported verbs at parse time with an AI-consumable structured error. The two stakeholder-critical
safety properties both survived active attempts to break them: "never return wrong rows" (the
truthful residual re-filtered over-returned rows to exactly the correct set) and "never leak a
credential" (the canary appears in no plan, preview, error, DTO, or log; Bearer is redacted at the
wire; POSTs are never silently retried). The single residual risk to the business outcome (the full
wasm-leaf build) is a known, consistently-handled park, not a surprise.

**Decision: Approve with minor suggestions** — 7/7 acceptance scenarios validated end-to-end; the
only open item is the wasm-leaf build, a documented shared-workspace park identical to accepted
t22/t23, to be carried to t38.
