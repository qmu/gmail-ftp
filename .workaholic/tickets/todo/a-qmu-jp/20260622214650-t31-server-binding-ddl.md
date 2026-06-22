---
created_at: 2026-06-22T21:46:50+09:00
author: a@qmu.jp
type: enhancement
layer: [Domain]
effort:
commit_hash:
category:
depends_on: [20260622214650-t30-server-runtime-and-self-config-driver.md]
---

# Server binding DDL (CREATE ENDPOINT/TRIGGER/JOB/VIEW/WEBHOOK)

## Overview

This ticket implements the **frozen, driver-agnostic server DDL** described in RFD 0001
§3 (closed-core keywords — "Server DDL") and §8 (Server / bindings = "what causes a plan
to run"). It delivers the parse + desugar layer that turns the human/AI-facing `CREATE …`
forms into ordinary `INSERT INTO /server/*` effect-plans against the self-config driver
landed in t30. No new keywords beyond the reserved set are added; the DDL is pure sugar.

Because the server **is a driver** (`/server/...`), bindings are just data. A user/agent
writes `CREATE ENDPOINT …` and it evaluates — like every other write — to a `Plan` of
`INSERT` effects. The runtime (t30) is *what causes those bound plans to fire*; this ticket
only governs how a binding is *declared and stored*, preserving the purity invariant (§3):
the DDL constructs effects, it never performs I/O.

Implements: RFD §3 (frozen Server DDL keywords), §8 (binding forms + Cloudflare mapping),
§6 (effect-plan as the desugar target).

## Scope

In scope:
- Grammar + AST for: `CREATE ENDPOINT <method> <route> AS <query>`;
  `CREATE TRIGGER <name> ON <event> [WHERE <pred>] DO <plan>`;
  `CREATE JOB <name> EVERY <interval> DO <plan>`;
  `CREATE [MATERIALIZED] VIEW <path> AS <query>`;
  `CREATE WEBHOOK <name> AT <route>`.
- Desugaring each form to one `INSERT INTO /server/{endpoints,triggers,jobs,views,webhooks}`
  effect-plan with an owned, driver-agnostic row DTO.
- Validation against the `/server/*` schema exposed by the t30 self-config driver
  (capability check + column typing) at parse/desugar time.
- The embedded body (`AS <query>` / `DO <plan>`) stored as a **serialized statement/plan
  spec**, parsed and type-checked now, executed later by the runtime.

Out of scope (deferred):
- `CREATE POLICY` and capability-gating semantics for unattended execution → t32
  (unattended-execution safety / POLICY).
- Actually *running* endpoints/triggers/jobs, scheduling, `LAST_RUN()` state, webhook
  ingestion → t30 runtime (this ticket only stores the bindings it executes).
- Cloudflare deployment mapping (Worker/Cron/Queue/DO) → server deploy ticket in E7.
- `INSERT INTO /server/*` plumbing and `/server/*` schema itself → t30 (dependency).

## Key components

New module `core/src/ddl/server.rs` (the closed-core DDL lives with the grammar, not in a
driver — keywords are frozen and shared):

- `enum ServerDdl { Endpoint(EndpointDecl), Trigger(TriggerDecl), Job(JobDecl),
  View(ViewDecl), Webhook(WebhookDecl) }` — sum type, one variant per frozen form.
- DTOs (owned, no vendor types): `EndpointDecl { method: HttpMethod, route: Route,
  query: StatementSpec }`, `TriggerDecl { name, event: EventRef, predicate: Option<Expr>,
  plan: PlanSpec }`, `JobDecl { name, every: Interval, plan: PlanSpec }`,
  `ViewDecl { path: VfsPath, materialized: bool, query: StatementSpec }`,
  `WebhookDecl { name, route: Route }`.
- `StatementSpec` / `PlanSpec` — already-parsed, serializable wrappers around the core
  AST / effect-plan spec (the deferred body). Both implement `serde::Serialize` so the row
  payload is plain data.
- `parse_server_ddl(tokens) -> Result<ServerDdl, ParseError>` — winnow parser hooked into
  the core statement parser; `CREATE` dispatches here.
- `trait DesugarToInsert { fn desugar(self) -> Plan; }` impl on `ServerDdl`, producing a
  single `InsertInto` effect node targeting the right `/server/*` path with a row built from
  the DTO. Reuses the core `Plan`/`Effect` types from E2 — no new effect kinds.
- `enum EventRef` (source-change / webhook ref), `struct Interval` (parsed `EVERY`),
  `enum HttpMethod`, `struct Route` — all driver-agnostic value types.

Respected invariants: closed-core grammar (no new keywords); three open registries
untouched (DDL adds none); effects-as-data + purity (`desugar -> Plan`, no I/O); owned DTOs
(no vendor leaks); capability gating deferred to t32 but column/verb validation done here
against the t30-declared `/server/*` capabilities.

## Implementation steps

1. Add the frozen DDL tokens (`ENDPOINT TRIGGER JOB VIEW MATERIALIZED WEBHOOK DO EVERY ON
   AS AT CREATE`) to the lexer keyword table if not already reserved by t30.
2. Define the `ServerDdl` enum + per-form DTOs and value types in `core/src/ddl/server.rs`.
3. Implement `parse_server_ddl` in winnow: `CREATE` → branch on the next keyword; parse the
   inline `AS <query>` via the core query parser and `DO <plan>` via the core statement
   parser, embedding the result as `StatementSpec`/`PlanSpec`.
4. Parse `EVERY <interval>` (e.g. `EVERY 5m`, `EVERY '1h'`) into `Interval`; `ON <event>`
   into `EventRef`; `[WHERE <pred>]` into `Option<Expr>`.
5. Implement `DesugarToInsert::desugar` for each variant → one `InsertInto` `/server/<kind>`
   effect with the serialized row.
6. Wire the desugar output into the existing PREVIEW/COMMIT path so `CREATE …` previews as
   "insert 1 row into /server/<kind>".
7. Validate the desugared row against the `/server/*` schema/capabilities from t30; reject
   unknown columns / unsupported nodes at parse time with a structured error.
8. Golden tests: statement → AST → desugared `Plan` for all five forms (+ `MATERIALIZED`).

## Considerations

- **Least-privilege / secrets**: a binding's body may name credentialed drivers, but this
  ticket stores only the *spec* — no tokens are captured or logged. Resolving which drivers a
  handler may touch (POLICY) is t32; leave a typed seam (`policy_ref: Option<...>`) on the
  decl DTOs so t32 attaches without a schema migration.
- **Idempotency / recovery**: desugar to `INSERT INTO` per RFD; consider an `UPSERT`-by-name
  variant (re-`CREATE` of a same-named trigger/job) so re-applying a `config.cfs` is
  retry-safe (RFD §6 idempotency). Decide and document; default to `INSERT` + explicit error
  on duplicate name to avoid silent overwrite.
- **Observability**: each desugared insert flows through the standard audit ledger (§6/§10);
  a created binding is one auditable applied effect.
- **Hard part — deferred bodies**: `AS <query>` / `DO <plan>` must be parsed and type-checked
  *now* (so a malformed binding is rejected at `CREATE` time, important for AI per §5) but
  executed *later* by the runtime. Resolve by storing a fully-parsed, serializable
  `StatementSpec`/`PlanSpec`, not raw source — the runtime rehydrates without re-parsing and
  cannot be surprised by a parse error at fire time.
- **Hard part — purity**: the embedded `DO <plan>` is itself an effect-plan; embedding it in
  a DTO must not execute it. Keep it as data (`PlanSpec`), never a live `Plan` that could be
  committed by accident.
- **Coding standards / structure**: DDL belongs to closed core (`core/src/ddl/`), not a
  driver; the `/server/*` row shapes are owned DTOs; no winnow combinators leak vendor types.

## Acceptance criteria

- `cargo build` and `cargo clippy --all-targets -- -D warnings` are green.
- Golden tests assert the desugared `Plan` for each form: e.g. `CREATE JOB nightly EVERY 1h
  DO (...)` desugars to exactly one `InsertInto("/server/jobs", row)` with `every=1h` and the
  embedded `PlanSpec` round-tripping through serde unchanged.
- Plan assertions (no live creds): `PREVIEW` of every `CREATE …` form reports "1 row →
  /server/<kind>" and performs no I/O; `COMMIT` is the only impure step.
- Parse-time rejection tests: malformed body, unsupported `/server/*` column, and unknown
  `CREATE` subkeyword each yield a structured `ParseError` (not a panic).
- A `MATERIALIZED VIEW` desugars with `materialized=true`; a plain `VIEW` with `false`.
- No new closed-core keywords introduced beyond the RFD-frozen set; no vendor type appears in
  any public DTO (checked by review + `core/src/ddl/server.rs` having no SDK imports).
