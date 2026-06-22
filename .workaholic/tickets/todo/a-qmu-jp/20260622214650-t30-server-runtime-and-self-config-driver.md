---
created_at: 2026-06-22T21:46:50+09:00
author: a@qmu.jp
type: enhancement
layer: [Infrastructure]
effort:
commit_hash:
category:
depends_on: [20260622214650-t09-effect-plan-and-preview-commit.md, 20260622214650-t13-driver-contract-trait.md]
---

# Server runtime + /server self-config driver

## Overview
Delivers `cfs serve <config.cfs>`: the long-lived runtime that boots from a file of cfs
statements and is itself **reconfigurable through cfs**. Implements RFD §8 (Server) and the
§6 runtime principle that "a statement is a plan; the runtime is just *what causes a plan to
run*". The server's own configuration — endpoints, triggers, jobs, views, policies, webhooks
— is **data** exposed under the `/server/...` mount and managed by the same DSL the server
runs. Booting a config file is therefore equivalent to replaying `INSERT INTO /server/...`
statements; the frozen `CREATE ENDPOINT|TRIGGER|JOB|VIEW|MATERIALIZED VIEW|WEBHOOK|POLICY`
DDL (RFD §3, §8) is sugar over those writes. This ticket builds the runtime loop + the
`/server` self-config driver; it does **not** implement the bindings' execution semantics
(HTTP serving, cron firing, webhook ingestion) beyond registration and a generic dispatch
hook — those are sibling tickets in E7.

## Scope
In scope:
- `cfs serve <config.cfs>` subcommand: parse the file into statements, apply each as a Plan
  via `COMMIT` against the in-process `/server` driver, then enter the supervised run loop.
- The `/server` driver implementing the `Driver` trait (t13): namespace
  `/server/{endpoints,triggers,jobs,views,policies,webhooks}`, each an **append/table**
  archetype node holding owned config DTOs; supports `SELECT/INSERT/UPSERT/UPDATE/REMOVE`.
- Hot-reconfigure: writes to `/server/*` (from CLI, an endpoint, or a trigger) mutate the
  live registry transactionally and notify subscribers (single-source ACID, RFD §6).
- A `ServerState` registry (the source of truth) + a `Runtime` supervisor owning it.
- A generic `Binding` trait + dispatch seam so E7 sibling tickets plug HTTP/cron/webhook
  causes in without touching the registry.
- `DESCRIBE /server/...` returns the config schema (so an AI configures the server via DSL).
- Audit ledger entry on every `/server` mutation and every fired plan (RFD §6, §10).

Out of scope (deferred):
- HTTP endpoint serving (`ENDPOINT`→axum routes) → sibling E7 ticket t31.
- Cron `JOB` scheduler + `LAST_RUN()` state → sibling E7 ticket t32.
- Inbound `WEBHOOK`/event-bus ingestion + `TRIGGER` pollers/watchtower → sibling E7 t33.
- `POLICY` enforcement engine (capability gating at fire time) → sibling E7 t34.
- Cloudflare deployment mapping (Worker/Cron/Queue/DO) → E7 t35.
- `CREATE ...` DDL parsing lives in E1; this ticket consumes the resulting AST/Plan.

## Key components
New crate/module `cfs-server` (binary feature `serve`):
- `runtime.rs`
  - `struct Runtime { state: Arc<RwLock<ServerState>>, bindings: Vec<Box<dyn Binding>>, audit: AuditSink }`
  - `impl Runtime { fn boot(cfg: &Path, world: &mut World) -> Result<Runtime>; fn run(self) -> Result<()> }`
  - `boot` reads the file, parses to `Vec<Statement>` (E1), lowers each to `Plan` (t09),
    and `COMMIT`s against the `/server` driver — no special-case config loader.
- `state.rs` — owned config DTOs (no vendor/SDK leak, RFD §9):
  - `struct ServerState { endpoints: Map<Name, EndpointDef>, triggers, jobs, views, policies, webhooks }`
  - `EndpointDef { method, route, query: StatementId }`, `TriggerDef { on: Event, where_: Option<Pred>, plan }`,
    `JobDef { every: Interval, plan, last_run: Option<DateTime> }`, `ViewDef { path, query, materialized: bool }`,
    `PolicyDef { handler, allow: CapSet }`, `WebhookDef { name, route }`. All `Serialize/Deserialize`, `Clone`.
- `driver.rs` — `struct ServerDriver { state: Arc<RwLock<ServerState>> }` implementing the t13
  `Driver` trait: `describe()` exposes per-node schema; `capabilities()` advertises
  `SELECT/INSERT/UPSERT/UPDATE/REMOVE` per node; `plan_write()` returns `Plan` effect nodes
  (`EffectKind::ServerConfigWrite`) that are **pure** (purity invariant, RFD §3) — the
  interpreter mutates `ServerState` only at `COMMIT`.
- `binding.rs` — `trait Binding { fn kind(&self) -> BindingKind; fn reconcile(&mut self, state: &ServerState) -> Result<()>; }`
  the seam siblings implement; `reconcile` is called after every committed `/server` mutation
  (declarative: bindings converge to the registry, no imperative add/remove API).
- `serve` CLI wiring in `main.rs`: `Commands::Serve { config: PathBuf }`.
- Effect-plan touch: add `EffectKind::ServerConfigWrite { node, op, dto }` to t09's plan enum;
  `irreversible = false`, idempotent under `UPSERT`.

## Implementation steps
1. Add `EffectKind::ServerConfigWrite` to the effect-plan enum (t09) + its `COMMIT` arm that
   takes the `RwLock` write guard and applies op to `ServerState`.
2. Define the owned config DTOs in `state.rs` with serde + a `ServerState` registry type.
3. Implement `ServerDriver` against the `Driver` trait (t13): namespace tree, `describe`,
   `capabilities`, and `plan_*` producing pure `ServerConfigWrite` nodes; reject unsupported
   verbs at plan time with a structured error (AI-legible).
4. Register `ServerDriver` into the `World`/mount table under `/server`.
5. Implement `Runtime::boot`: read file → parse (E1) → lower (t09) → `PREVIEW` log → `COMMIT`
   each statement in file order; fail fast with line-located errors on any rejected statement.
6. Define the `Binding` trait + `BindingKind`; after each committed `/server` write, call
   `reconcile` on every registered binding (start with a no-op `NullBinding` for tests).
7. Wire `cfs serve <config.cfs>` in `main.rs`; `Runtime::run` blocks on a shutdown signal
   (`tokio::signal::ctrl_c`) and drains the audit sink on exit.
8. Emit an audit ledger record for every `/server` mutation (who/op/node/before-after).
9. Golden tests: boot a fixture `.cfs` and assert the resulting `ServerState` snapshot.

## Considerations
- **Least-privilege & secrets**: config DTOs reference policies/credentials by handle, never
  inline tokens; `ServerState` is never logged verbatim (RFD §10). `POLICY` rows are stored
  now but enforced in t34 — document the gap so no handler runs unconstrained before then.
- **Idempotency/recovery**: hot-reconfigure uses `UPSERT` semantics so re-applying a config
  file (restart, retry) converges to the same `ServerState`; boot is replay-safe. A
  single-source `/server` write is one ACID mutation under the `RwLock` (RFD §6).
- **Concurrency**: the run loop and any (future) inbound binding share `Arc<RwLock<ServerState>>`;
  `reconcile` must take a read snapshot, never hold the write guard across `await`. Use
  `parking_lot`-style short critical sections; clone the snapshot for bindings.
- **Observability**: structured `tracing` spans for boot (per-statement), each mutation, and
  binding reconcile; the audit ledger is the applied-effect record (RFD §6, §10).
- **Hard part**: keeping the runtime a *pure consumer of plans* — boot must go through the
  same `COMMIT` path as a live write, not a privileged shortcut. Resolve by making
  `ServerConfigWrite` the **only** way `ServerState` changes, and routing boot through it.
- **Directory/coding standards**: owned DTOs, small consumer-side traits, no vendor types,
  `thiserror` error enums with structured variants; keep `cfs-server` free of HTTP/cron deps
  (those land in siblings behind the `Binding` trait).

## Acceptance criteria
- `cargo build` and `cargo clippy --all-targets -- -D warnings` are green.
- `cfs serve fixtures/server_boot.cfs` boots without network/live credentials, applies all
  statements, and reaches the run loop; `ctrl_c` shuts down cleanly draining the audit sink.
- **Plan assertion**: lowering `CREATE JOB ... EVERY ... DO ...` (and the equivalent
  `INSERT INTO /server/jobs ...`) yields identical `ServerConfigWrite` plan nodes (sugar
  equivalence) — golden test on the plan, not on execution.
- **Plan assertion**: writing to a `/server` node with an unsupported verb is rejected at
  plan time with a structured, machine-readable error (no panic, no `COMMIT`).
- Golden test: a fixture `.cfs` boots to a deterministic `ServerState` snapshot (serde),
  and re-applying the same file is a no-op (idempotency).
- `DESCRIBE /server/triggers` returns the trigger schema with no live backend.
- A registered `NullBinding`'s `reconcile` is invoked exactly once per committed `/server`
  mutation (asserted via a counting test double).
- Unit tests cover purity: constructing a `/server` write plan performs no state mutation
  until `COMMIT`.
