# RFD 0001 — `qfs`: an AI-driven, DSL-programmable multi-service control plane

Status: Draft (design anchor for the from-scratch rebuild)
Author: a@qmu.jp
Date: 2026-06-22

> This RFD is the single source of truth for the rebuild. Every ticket references it.
> We are **starting from zero** in this repository (Rust). The existing Go `gmail-ftp`
> and `../gdrive-ftp` are **subsumed** as two drivers; they are not merged or refactored.

## 1. Vision

`qfs` ("cloud file system") is **one Rust binary** that is both a **CLI** and a **server**,
exposing every external service (Gmail, Drive, S3, R2, D1, GitHub, Slack, SQL DBs, git,
local files, generic REST, …) through **one uniform, filesystem-shaped, pipe-SQL DSL**.

It exists **for AI**: an agent learns *one* small grammar and one operating procedure
(`DESCRIBE → write statement → PREVIEW → COMMIT`) instead of N SDKs. Give the server the
DSL + credentials and it can operate everything — interactively, one-shot, or as
long-lived endpoints/triggers/jobs.

Distribution: a single static binary that runs as a CLI locally, as a daemon on EC2, or
compiled to `wasm32` for Cloudflare Workers.

## 2. Core model (three faces of one engine)

1. **VFS** — every service is mounted under a virtual root and addressed by path.
   A path is just a query that resolves to a set; `ls`/`cp`/`mv`/`rm` are operations on
   that set. The filesystem *is already* a set/query model (dirs = sets, globs = queries);
   we extend it with attribute predicates and cross-service operators.
2. **Pipe-SQL** — Google-style pipe syntax: `FROM <path> |> <op> |> <op> …`, UPPERCASE
   keywords. The query side (`WHERE/SELECT/JOIN/UNION/EXCEPT/AGGREGATE`) is **pure**.
3. **Effect-plan** — write operators (`cp/mv/INSERT/UPSERT/UPDATE/REMOVE/CALL`) do **not**
   execute; they evaluate to a **Plan** (a typed DAG of effects). The interpreter applies
   the plan at the edge. "A statement is a plan; the runtime is just *what causes a plan to
   run*" (CLI = now; server = an event/schedule/request).

Lineage to study: Plan 9 / 9P (everything-is-a-file), Trino/Calcite/Steampipe (federation),
DuckDB (embeddable SQL over remote blobs), Haxl (auto-batched remote fetches from pure
descriptions), Nix/Bazel/Haskell-IO (build a side-effect graph, run it at the edge),
rclone (one grammar over many storage backends), Mergestat/askgit (git as SQL),
PostgREST/Hasura + n8n/Temporal (query-as-API + event automation).

## 3. Language: closed core + three open registries (governance)

The keyword set is **frozen**. New backends add **zero keywords**. Everything new plugs into
exactly one of three open namespaces:

- **paths** — `/driver/...` mounts (a new service = a new mount).
- **functions / procedures** — `fn(...)` and `CALL driver.action(...)` (the registry).
- **codecs** — `DECODE fmt` / `ENCODE fmt` (the codec registry).

### Closed core keywords (reserved, frozen)
Query/transform: `FROM WHERE SELECT EXTEND SET AGGREGATE GROUP BY ORDER BY LIMIT DISTINCT
JOIN UNION EXCEPT INTERSECT AS EXPAND`.
Effects: `INSERT INTO  UPSERT INTO  UPDATE  REMOVE  VALUES  RETURNING  CALL`.
Codecs: `DECODE  ENCODE`.
Plan: `PREVIEW  COMMIT`.
Server DDL (also frozen, driver-agnostic; sugar over `/server/...` writes):
`CREATE ENDPOINT | TRIGGER | JOB | VIEW | MATERIALIZED VIEW | WEBHOOK | POLICY`, `DO`, `EVERY`, `ON`.
Operators: `|>` and `= <> < > <= >= AND OR NOT LIKE ~ ANY IN BETWEEN`.

### Universal verbs vs domain actions
- **CRUD is universal** — `INSERT/UPSERT/UPDATE/REMOVE/SELECT`. The **path is the type**;
  no per-driver create verb (creating a draft = `INSERT INTO /mail/drafts`; an S3 object =
  `UPSERT INTO /s3/bucket/key`; a git commit = `INSERT INTO /git/repo/commits`).
- **Irreducible state transitions** (no universal analog) are **namespaced procedures**:
  `CALL mail.send`, `CALL git.merge(...)`, `CALL github.merge(method=>'squash')`,
  `CALL ci.dispatch(...)`. Namespacing makes `git.merge` ≠ `github.merge` collision-proof;
  `CALL` only resolves procs a driver declares (capability).
- **Ergonomic aliases** like `SEND`, `MERGE` are **pure functions in the registry**, defined
  in qfs, desugaring to a `CALL` — never keywords:
  `fn SEND(d) = d |> CALL mail.send`. They are in scope only for plans whose driver provides
  them (receiver-typed resolution); ambiguity falls back to the qualified `CALL`.

### Purity invariant
Every function — core or alias — has type `… -> Plan`. It **constructs** effects, never
performs them. The only impure operation is the interpreter (`COMMIT : Plan -> World -> World`).
This is what makes `SEND`-as-a-function safe and keeps everything dry-runnable, testable,
and composable. `CALL driver.x` returns a plan node; it does not do I/O.

## 4. Data & type model

- Rows with typed columns; types come from the source (DB catalog, JSON/YAML, etc.).
- **Nested data**: `struct`/`array` column types. `EXPAND <field>` explodes a nested
  collection into rows (same operator for mail attachments and JSON arrays); path access
  `a.b.c` navigates structs without flattening. Deeply irregular JSON stays a struct column.
- **`@version` temporal coordinate** (uniform across drivers that support versioning):
  `/git/repo@<ref>/path`, `/s3/bucket/key@<versionId>`, `/drive/file@<rev>`,
  `/sql/pg/orders AS OF '2026-01-01'`.
- **Codecs** bridge blob↔relational: `DECODE`/`ENCODE` for `json, yaml, toml, csv,
  markdown+frontmatter`. Markdown+frontmatter → row (frontmatter keys = columns, `body` =
  content) — this makes `.workaholic/**/*.md` itself a queryable/editable table. Codecs are
  pure `bytes↔rows` and work on **any** blob source (FS, S3, git, Drive, REST response).

## 5. Driver contract

A driver declares, and that declaration is everything the engine + the AI need:

- **Namespace** (path tree); per-node **archetype** (see below) + **schema** (columns; powers `DESCRIBE`).
- **Capabilities** — which universal verbs each node supports; unsupported ops are rejected
  at parse time (structured error — important for AI).
- **Procedures** — domain actions for `CALL` (the irreducible transitions).
- **Pushdown** — which parts of a pipeline the driver can execute itself.
- **Prelude** — optional pure alias functions shipped with the driver (e.g. `SEND`).

### Four archetypes (how any service maps)
| Archetype | Native verbs | Examples |
|---|---|---|
| Blob / namespace | `ls cp mv rm` | local FS, S3/R2, Drive, repo files, Slack files |
| Relational / table | `SELECT JOIN INSERT UPDATE` | SQL DBs, D1, Notion DB |
| Append / log | `SELECT(tail) INSERT(append)` | Slack, mail, CF Queues, comments, webhooks |
| Object-graph + workflow | CRUD + `CALL` procs | GitHub, Linear, K8s |

A single driver may expose multiple archetypes on different sub-paths (git is all three:
versioned-blob FS `@ref/path`, relational history `commits`, mutable pointers `refs`).

## 6. Runtime / interpreter

- **Effect-plan**: typed DAG of effects with dependencies and an `irreversible` flag.
- **PREVIEW** (default): print the plan + affected counts; **COMMIT**: apply it.
- **Batch + auto-parallelize** independent effects (Haxl-style); the planner sees the whole
  graph (this is how Gmail N+1 listing is collapsed into one batched fetch).
- **Pushdown federation**: collapse same-source subtrees into native execution (one SQL
  query per DB); only combine cross-source results locally. **Local combine engine decision:
  embed DuckDB vs. own relational evaluator** (footprint vs. build cost — open).
- **Transactions**: a single-source plan = a real ACID transaction; cross-source = orchestrated,
  best-effort, with explicit partial-failure recovery (cp = copy→verify→delete; the audit log
  is the applied-effect ledger used for reconstruction).
- **Idempotency**: `UPSERT` for retry-safe (at-least-once webhooks). **Optimistic concurrency**
  via `@version`/ETag for read-then-write.
- **Observability**: per-leg timeouts, bounded retries, circuit breakers, structured logs;
  the **audit log** records every applied effect (and on the server, every fired plan).

## 7. CLI

- Interactive FTP-like shell (`qfs`): prompt, cwd (tagged `{driver, path}`), `ls/cd/cp/mv/rm`,
  completion; `cp` spans mounts without leaving cwd.
- One-shot (`qfs run '<stmt>'` / `-e`): no cwd, absolute paths + `id:`/path addressing; `-json`.
- PREVIEW/COMMIT UX; destructive ops over a set show counts and require explicit commit.

## 8. Server

- `qfs serve <config.qfs>`. The **server is a driver** (`/server/...`): its endpoints,
  triggers, jobs, views, policies, webhooks are **data** you manage with qfs. `CREATE …`
  forms are sugar over `INSERT INTO /server/...`.
- **Bindings** = "what causes a plan to run":
  `CREATE ENDPOINT <method> <route> AS <query>` (query → HTTP API),
  `CREATE TRIGGER <name> ON <event> [WHERE …] DO <plan>` (react to source change / webhook),
  `CREATE JOB <name> EVERY <interval> DO <plan>` (cron; `LAST_RUN()` state),
  `CREATE [MATERIALIZED] VIEW <path> AS <query>` (virtual/cached relation),
  `CREATE WEBHOOK <name> AT <route>` (inbound events).
- **This is "watchtower"**: watch services (webhooks + pollers) → run effect-plans.
- **Unattended-execution safety**: `CREATE POLICY` per handler (least-privilege: which
  drivers/verbs a handler may touch), idempotent verbs, `PREVIEW`-as-CI-test, audit ledger.
- **Deployment mapping (Cloudflare)**: `ENDPOINT`→Worker, `JOB`→Cron Triggers,
  `WEBHOOK`/event bus→Queues, stateful watcher/`LAST_RUN`→Durable Object, `/d1`/`/r2`/`/kv`→
  native bindings (not HTTP). Also runs as a plain EC2/Linux daemon.

## 9. Implementation

- **Language**: Rust. **Single binary**, also `wasm32-unknown-unknown` for Workers.
- **Parser** (research 2026-06-22): default **winnow** (actively maintained — commits this
  week; function-based, no macros; fixes the nom/combine gripes). **chumsky** is the fallback
  if DSL parse-error recovery is decisive (now on Codeberg, GitHub archived). **combine** is
  deprioritized (sporadic maintenance, ~4.5 months idle). A spike confirms before lock-in.
- Model the AST, effect-plan, capabilities, and archetypes with Rust **enums/sum types**
  (the reason Rust over Go for the core).
- Consumer-side small traits for `Driver` and `Codec`; SDK/vendor types never leak past a
  driver boundary (owned DTOs).
- No heavy vendor SDKs — thin HTTP clients per service (footprint).

## 10. Security

A server holding tokens for Gmail+Drive+GitHub+Slack+AWS+CF and running cross-service plans
is a large blast radius. Controls: per-handler/`POLICY` least privilege; capability gating;
encrypted credential store, never logged; dry-run in CI; full audit ledger; idempotent,
recoverable effects. Irreversible procs (`CALL mail.send`, deletes) are where PREVIEW + policy
earn their keep.

## 11. Ticket epics

E0 Foundations · E1 Language core · E2 Effect-plan & runtime · E3 Federation & data ·
E4 Drivers · E5 Auth/credentials · E6 CLI · E7 Server · E8 Cross-cutting (security, test, docs, AI procedure).

**E8 — AI operating procedure (t39).** The single operating procedure an AI agent follows to
drive every service through qfs is the uniform loop **DESCRIBE `<path>` → write a qfs statement →
PREVIEW → COMMIT**. It is authored as a discoverable, binary-embedded agent skill at
[`crates/skill/assets/SKILL.md`](../../crates/skill/assets/SKILL.md) (RFD §1/§5/§9), backed by the
`qfs describe <path> [-json]` subcommand (the `DescribeReport` contract in `qfs-core::describe`)
and a no-creds golden corpus (`crates/skill/tests/golden_corpus.rs`) proving the loop is identical
across mail/drive/github/slack/sql/git and a `/server/...` binding.
