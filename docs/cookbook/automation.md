# Cookbook: Automation (the server)

The same language has a server side. Each `CREATE …` binding takes a query you already know and
runs it on an **event**, a **schedule**, or an **HTTP request**. You collect bindings in a `.qfs`
config and run it with `qfs serve config.qfs`.

You can **preview** any binding to see exactly the plan it would install — no socket, no backend
needed.

## Trigger — react to events

**When mail arrives, post its subject to Slack:**

```qfs
CREATE TRIGGER notify
  ON /mail/inbox
  DO INSERT INTO /slack/acme/general/messages VALUES (NEW.subject)
```

**Only escalate high-priority mail** — triggers can filter on the new row with `NEW`:

```qfs
CREATE TRIGGER escalate
  ON inbox
  WHERE NEW.priority > 3
  DO INSERT INTO /slack/acme/ops/messages VALUES ('urgent mail')
```

**Archive every new row to another store:**

```qfs
CREATE TRIGGER archive
  ON /mail/inbox
  DO UPSERT INTO /s3/archive/mail VALUES (NEW.id)
```

## Job — run on a schedule

**Nightly cleanup of old scratch files** (`EVERY` takes a quoted interval):

```qfs
CREATE JOB nightly
  EVERY '1h'
  DO REMOVE /tmp/scratch WHERE age > 7
```

## Endpoint — expose a query as an HTTP API

**A read-only `GET /recent` that returns the latest inbox items:**

```qfs
CREATE ENDPOINT recent
  ON 'GET /recent'
  AS FROM /mail/inbox |> LIMIT 5
```

## View — name and cache a query

**A plain view** (a named query):

```qfs
CREATE VIEW recent_mail
  AS FROM /mail/inbox |> LIMIT 50
```

**A materialized view** (cached result):

```qfs
CREATE MATERIALIZED VIEW cached
  AS FROM /mail/inbox |> LIMIT 50
```

## Policy — least privilege

A `POLICY` constrains what a handler may do: allow some verbs, deny the rest, optionally scoped to a
path pattern.

**Read-only API access:**

```qfs
CREATE POLICY api
  ALLOW SELECT
  DENY INSERT, UPDATE, REMOVE, CALL
```

**Allow uploads to one bucket prefix only:**

```qfs
CREATE POLICY uploads
  ALLOW UPSERT ON 's3/*'
```

::: tip Why this is safe
A binding is just a query, so you preview it exactly like any other statement before it goes live.
Pair handlers with a `POLICY` so an automation — or an AI agent — can act, but only within the
bounds you set.
:::
