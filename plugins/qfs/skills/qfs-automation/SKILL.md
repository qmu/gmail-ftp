---
name: qfs-automation
description: Use when a task needs the qfs SERVER side — scheduled jobs, triggers, HTTP endpoints, and cached views created with CREATE bindings over queries you already write.
---

# Automation (the server)

The same language has a server side. Any query you can already write becomes **standing
automation** — a `CREATE …` binding runs it on an **event**, on a **schedule**, or on an **HTTP
request**, and a `VIEW` names and caches it. You collect bindings in a `.qfs` config and serve them
with `qfs serve config.qfs`.

## See it work first

**Turn "when mail arrives, tell the team" into a permanent reflex** — one binding, and every new
inbox message posts its subject to Slack, forever, unattended:

```qfs
create trigger notify
  on /mail/inbox
  do insert into /slack/acme/general/messages values (NEW.subject)
```

```text
rows: []
is_pure: true
```

A binding has no rows to return, so previewing it with `qfs run` yields that empty, **pure** plan —
proof the statement parses and type-checks, not an install report. The trigger is just the read you
already know (`/mail/inbox`) wired to fire on the `NEW` row.

::: tip Bindings are queries you preview before they go live
A `CREATE …` binding wraps a query, so you inspect it exactly like any other statement. Preview a
single binding with `qfs run` (no socket, no backend) to confirm it's valid; run a `JOB` once with
`qfs job run` and it **previews by default**, changing nothing until you pass `--commit` — through
the same policy gate and irreversible gate as `qfs run`. Pair any handler with a `POLICY` so an
automation, or an AI agent, acts only within bounds you set.
:::

Bindings don't run until a server hosts them — collect them in a `.qfs` file and start it, once, in
**[Setup](#setup)**. After that every recipe on this page installs verbatim.

## Setup

::: tip Prerequisites — an operator, an account, a mount
A binding that touches a cloud service needs the same three one-time steps as any other query
against it: a signed-in operator (`qfs init` — **[The operator identity](/guide/operator)**), an
authorized account (`qfs account add …`), and a mount binding that account to a path
(`qfs connect …`). Do them via each service's cookbook first; every step below assumes them.
:::

Put your `CREATE …` bindings in one config and serve them. The happy path is a single command:

```sh
qfs serve config.qfs
```

`qfs serve` is one process presenting the engine as three faces: the HTTP API, an **MCP endpoint**
an AI agent connects to, and an **embedded web dashboard** whose **approval cards** let a human
review and approve a pending irreversible commit. So an unattended binding can still route an
irreversible effect to a person for sign-off instead of failing closed.

::: warning Separate every statement in a `.qfs` config
A config holds several `CREATE …` statements. **End each one with a `;`** (or a blank line) —
adjacent statements with no separator fail to parse (`RESERVED_AS_IDENTIFIER`, because the parser
reads the next `create` as an identifier). `qfs serve` / `qfs job` only start once the whole file
parses.
:::

## The binding types

Once served, each binding shape wraps a query and fires on a different signal:

| binding | fires on | what it is |
| ------- | -------- | ---------- |
| `TRIGGER` | a new row on a path (`NEW`) | react to an event |
| `JOB` | a schedule an external scheduler runs | a saved named plan + cadence |
| `ENDPOINT` | an incoming HTTP request | a query exposed as an API route |
| `VIEW` / `MATERIALIZED VIEW` | every read / a cached read | a named (optionally cached) query |
| `POLICY` | — | least-privilege bounds a handler runs under |

## Trigger — react to events 🚧

**When mail arrives, post its subject to Slack:**

```qfs
create trigger notify
  on /mail/inbox
  do insert into /slack/acme/general/messages values (NEW.subject)
```

**Only escalate high-priority mail** — triggers can filter on the new row with `NEW`:

```qfs
create trigger escalate
  on /mail/inbox
  where NEW.priority > 3
  do insert into /slack/acme/ops/messages values ('urgent mail')
```

::: warning Object-store write targets aren't wired yet
A trigger whose body writes to `/s3` or `/r2` — e.g. `do upsert into /s3/archive/mail values
(NEW.id)` — can't install today: that `UPSERT` resolves to `unsupported_verb` (`supported: []`).
Route archival to `/local` (or a `/sql` table) until object-store writes land.
:::

## Job — a saved plan an external scheduler runs

**qfs is not a scheduler.** A `JOB` is a *saved named plan plus its intended cadence* — qfs does not
fire it itself. The `EVERY` interval is metadata an **external** scheduler reads; the *when* and the
exactly-once guarantee belong to the platform, not to qfs.

**Define the saved plan** in your config (`EVERY` takes a quoted interval; attach a `POLICY` for
least privilege — and make sure the policy's path pattern covers the path the job touches):

```qfs
create policy cleanup ALLOW remove on 'local/*'
```

```qfs
create job nightly
  every '1h'
  do remove /local/srv/scratch where name LIKE '%.tmp'
  policy cleanup
```

**Run it once** — the unit an external scheduler invokes. PREVIEW by default; `--commit` applies
through the same policy gate + irreversible gate as `qfs run`:

```sh
qfs job run app.qfs nightly
```

```text
PREVIEW job 'nightly' (policy cleanup, 1 effect(s); nothing applied — pass --commit):
  REMOVE local:/local/srv/scratch
```

```sh
qfs job run app.qfs nightly --commit
# an irreversible plan (REMOVE / CALL) additionally needs --commit-irreversible (fail-closed)
qfs job run app.qfs nightly --commit --commit-irreversible
```

**Individual / local — OS `cron`.** `qfs job cron` emits the crontab line for the saved cadence;
drop it into a host crontab (ensure cron's environment carries `QFS_PASSPHRASE` and any connection
credentials):

```sh
qfs job cron app.qfs nightly
```

```text
# qfs JOB 'nightly' — EVERY 1h (decision M: OS cron owns the *when*; qfs is not a scheduler).
# Ensure cron's environment carries QFS_PASSPHRASE (+ any connection creds) for the commit.
# An irreversible plan (REMOVE / CALL) additionally needs --commit-irreversible (fail-closed).
0 * * * *	qfs job run app.qfs nightly --commit
# Managed tier (Cloudflare Cron Triggers): [triggers] crons = ["0 * * * *"]
```

::: warning Idempotency is yours now
External schedulers are at-least-once (a Cron Trigger can double-fire on retry), and qfs keeps no
internal run-ledger to dedup a re-fire. Keep effects idempotent — `UPSERT` / `@version`
preconditions — so a re-run is a no-op.
:::

## Endpoint — expose a query as an HTTP API

**A read-only `GET /recent` that returns the latest inbox items:**

```qfs
create endpoint recent
  on 'GET /recent'
  as /mail/inbox |> limit 5
```

## View — name and cache a query

**A plain view** (a named query):

```qfs
create view recent_mail
  as /mail/inbox |> limit 50
```

**A materialized view** (cached result):

```qfs
create materialized view cached
  as /mail/inbox |> limit 50
```

## Policy — least privilege

A `POLICY` constrains what a handler may do: allow some verbs, deny the rest, optionally scoped to a
path pattern. (Scope the pattern to the **driver/path the handler actually touches** — a job that
removes a `/local` path needs `on 'local/*'`, not `'tmp/*'`, or the gate denies it.)

**Read-only API access:**

```qfs
create policy api
  ALLOW select
  DENY INSERT, update, remove, call
```

**Allow uploads to one bucket prefix only:**

```qfs
create policy uploads
  ALLOW UPSERT on 's3/*'
```

::: tip Why this is safe
A binding is just a query, so you preview it exactly like any other statement before it goes live.
Pair handlers with a `POLICY` so an automation — or an AI agent — can act, but only within the
bounds you set.
:::
