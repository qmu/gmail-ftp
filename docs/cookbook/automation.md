# Cookbook: Automation (the server)

The same language has a server side. Each `CREATE …` binding takes a query you already know and
runs it on an **event**, a **schedule**, or an **HTTP request**. You collect bindings in a `.qfs`
config and run it with `qfs serve config.qfs`.

You can **preview** any binding to see exactly the plan it would install — no socket, no backend
needed.

## Trigger — react to events

**When mail arrives, post its subject to Slack:**

```qfs
create trigger notify
  on /mail/inbox
  do insert into /slack/acme/general/messages values (NEW.subject)
```

**Only escalate high-priority mail** — triggers can filter on the new row with `NEW`:

```qfs
create trigger escalate
  on inbox
  where NEW.priority > 3
  do insert into /slack/acme/ops/messages values ('urgent mail')
```

**Archive every new row to another store:**

```qfs
create trigger archive
  on /mail/inbox
  do upsert into /s3/archive/mail values (NEW.id)
```

## Job — a saved plan an external scheduler runs

**qfs is not a scheduler.** A `JOB` is a *saved named plan plus its intended cadence* — qfs does not
fire it itself. The `EVERY` interval is metadata an **external** scheduler reads; the *when* and the
exactly-once guarantee belong to the platform, not to qfs.

**Define the saved plan** (`EVERY` takes a quoted interval; attach a `POLICY` for least privilege):

```qfs
create policy cleanup ALLOW remove on 'local/*'
create job nightly
  every '1h'
  do remove /tmp/scratch where age > 7
  policy cleanup
```

**Run it once** — the unit an external scheduler invokes. PREVIEW by default; `--commit` applies
through the same policy gate + irreversible gate as `qfs run`:

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
# 0 * * * *  qfs job run app.qfs nightly --commit
```

**Managed tier — Cloudflare Cron Triggers.** The same cadence becomes the `[triggers] crons` entry
in the generated `wrangler.toml`; the platform owns distribution and exactly-once.

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
path pattern.

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
