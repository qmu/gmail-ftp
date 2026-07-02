---
skill_name: qfs-cross-service
skill_description: Use when a task spans MORE THAN ONE service in a single qfs query — joining a database to GitHub, a file to a table, or federating several services with one JOIN or UNION over their paths.
---

# Cross-service

This is what qfs is *for*: one pipe-SQL statement that reaches across more than one service at once —
a `JOIN`, `UNION`, `INTERSECT`, or `EXCEPT` whose paths live in *different* services. Because every
service is the same kind of path yielding rows, a database table, a GitHub issue list, and a mailbox
combine in a single query as easily as two tables would.

::: tip Prerequisites — an operator, an account per service, a mount per service
A cross-service query needs **every** participating service connected. That means the one-time
setup gates: a signed-in operator (`qfs init` — **[The operator identity](/guide/operator)**), and,
per cloud service, an authorized account (`qfs account add …`) bound to its mount
(`qfs connect …`). Connect each one via its own cookbook (linked below) first.
:::

## See it work first

**Stitch one customer's whole story together from three services at once** — inbound mail joined to
the customer row in your database and the open GitHub issue they filed, one statement, newest first:

```qfs
/mail/inbox
|> join /sql/pg/customers on inbox.from == customers.email
|> join /github/acme/support/issues on customers.email == issues.reporter_email
|> where issues.state == 'open'
|> select customers.legal_name, inbox.subject, issues.number, issues.title
|> order by issues.created_at DESC
```

```text
legal_name         | subject                       | number | title
------------------ | ----------------------------- | ------ | -----------------------------
Northwind Traders  | Re: Q3 renewal — a question   |    412 | Renewal quote looks wrong
Boldpeak Dev       | Can we move the review?       |    398 | Review scheduling conflict
(2 row(s))
```

Three services that never speak to each other, federated in one query — qfs pushes each leg's filter
into its own service, then joins the results locally. Now the **write** payoff: reads federate, but
so do writes — **download a file from Google Drive and pack it straight into a Gmail draft**, one
composable statement that previews before it touches anything:

```qfs
/drive/my/report.pdf
|> select {filename: name, mime: mime_type, bytes: content} as att
|> aggregate array_agg(att) as attachments
|> extend to = 'a@example.com', subject = 'Q3 report', body = 'See attached.'
|> insert into /mail/drafts
```

```text
PREVIEW: 1 effect(s)
  #0 INSERT -> mail:/mail/drafts [affected 1]
  total affected: 1
```

::: tip Reads run now; writes preview
Every **read** — every `JOIN`, `UNION`, `INTERSECT`, `EXCEPT` — returns rows immediately. Every
**write** (`insert`, `upsert`, `call`) *previews* by default and changes nothing — add `--commit` to
apply it, `--commit-irreversible` for the ones that can't be undone (sending, trashing). Paste any
recipe below and safely watch what it *would* do first.
:::

There's no setup on this page: a cross-service query is only as connected as its legs, and each
service is wired up in **its own chapter** — [Gmail](/cookbook/gmail),
[Databases](/cookbook/databases), [GitHub](/cookbook/github), [Slack](/cookbook/slack),
[git](/cookbook/git), [Google Drive](/cookbook/gdrive), and
[Files & object storage](/cookbook/files). Two of the local drivers (`/sql/<conn>`, `/git/<repo>`)
need no account at all, so a database↔git join runs the instant you point qfs at them — see
[Join a database to git history](#join-a-database-to-git-history) just below.

## How a mixed-source query resolves

Every leg of a cross-service statement is pushed down to its own service — a `/sql` subtree becomes
one SQL query inside the database, a `/github` subtree becomes a filtered API fetch — and only the
residual that genuinely spans the sources (the cross-source `JOIN … ON`, the post-join `WHERE`, the
`SELECT`, the `ORDER BY`) runs locally in qfs's own engine. That is why a SQL table and a git repo
combine as easily as two database tables: whatever the pushdown split, qfs over-fetches safely and
re-checks locally, so you never get wrong rows.

The recipes below mix sources that **read today** (`/sql/<conn>/…`, `/git/<repo>/…`) with ones that
need a connected account (`/github/…`, `/slack/…`, `/mail/…`, `/drive/…`). Each is marked.

## Join a database to git history

**Match author records in a table to the commits they wrote** — `/sql` and `/git` both read, so this
runs end to end, no account required:

```qfs
/sql/orders/authors
|> join /git/myrepo/commits on name == author
|> select name, team, message
```

```text
name           | team     | message
-------------- | -------- | ---------------
Test <t@e.com> | platform | add feature
Test <t@e.com> | platform | initial commit
(2 row(s))
```

(`/sql/<conn>` is registered with `QFS_SQL_<CONN>`, `/git/<repo>` with `QFS_GIT_<REPO>` — see the
[Databases](/cookbook/databases) and [git](/cookbook/git) chapters.)

## Join a database to GitHub

**Match orders to the GitHub issues that track them:**

```qfs
/sql/orders/orders
|> join /github/acme/web/issues on id == issue_id
|> select id, title, status
```

::: warning Needs a connected account
The `/sql` leg reads today, but the `/github` leg fails with an actionable hint until an account is
bound to the mount (`qfs account add github …`, then `qfs connect /github --driver github
--account …` — see the [GitHub cookbook](/cookbook/github)). Once connected, the join runs end to
end.
:::

## Federate several services into one feed

**Combine every channel a contact reached you through — mail, Slack, and GitHub — into one unified
touch log** with `UNION`, normalized to a single shape and sorted newest first:

```qfs
/mail/inbox
|> select from as contact, 'email' as channel, subject as detail, received_at as at
|> union
   /slack/acme/support/messages
|> select user as contact, 'slack' as channel, text as detail, ts as at
|> union
   /github/acme/support/issues
|> select reporter_email as contact, 'github' as channel, title as detail, created_at as at
|> order by at DESC
```

Each leg selects into the same `contact, channel, detail, at` shape, so `UNION` stacks three
services into one timeline. This one needs all three accounts connected.

## Combine the same shape from two sources

**Everyone, across two tables, de-duplicated** — `UNION` runs entirely on sources that read today:

```qfs
/sql/orders/orders
|> select customer
|> union /sql/orders/orders
|> select customer
```

```text
customer
--------
alice
bob
carol
dave
(4 row(s))
```

## Move data between services 🚧

Because reads and writes share one language, "copy from here to there" spans services too.

**Snapshot a database table to JSONL** — the read and the `ENCODE` both run:

```qfs
/sql/orders/orders
|> select id, customer, total
|> encode jsonl
```

```text
content
-------------------------------------------------
{"customer":"alice","id":1,"total":150.0}
{"customer":"bob","id":2,"total":80.0}
{"customer":"carol","id":3,"total":220.0}
{"customer":"dave","id":4,"total":55.0}
```

…then write those bytes to a store with an `UPSERT INTO`. (Today these are two steps; the point is
they speak the same language end to end. `UPSERT` into `/local` runs now; `/s3`/`/r2` writes are
not yet implemented — see [Files & object storage](/cookbook/files).)

## Attach a Drive file to a Gmail draft 🚧

The dogfooding payoff from the hero, in full: **download a file from Google Drive, pack it into a
Gmail draft's `attachments` column, and address the draft** — one composable statement, no bespoke
`pack()`. It uses two small, reusable primitives: a **struct-over-columns** constructor (`{ … }` whose
field values are columns) and single-column **`array_agg`** (N rows → one `Array`).

```qfs
/drive/my/report.pdf
|> select {filename: name, mime: mime_type, bytes: content} as att
|> aggregate array_agg(att) as attachments
|> extend to = 'a@example.com', subject = 'Q3 report', body = 'See attached.'
|> insert into /mail/drafts
```

Read left to right: each Drive row becomes a `{filename, mime, bytes}` **struct** (`content` is the
file's bytes); `array_agg` collapses those structs into one `attachments` **array**; `extend` adds the
draft's `to`/`subject`/`body`. The struct constructor and `array_agg` are **general** — usable in any
read pipeline, not just this recipe:

```qfs
/local/reports
|> select {name: name, size: size} as entry
|> aggregate array_agg(entry) as entries
```

```text
entries
--------------------------------------------------
[{"name":"a.csv","size":120},{"name":"b.csv","size":98}]
(1 row(s))
```

The **read pipeline above runs today** — the struct/array constructors and `array_agg` execute
locally and produce the packed `attachments` value. What is **still being wired (🚧)** is the terminal
`|> insert into /mail/drafts` *materialising* a computed `FROM`-pipeline's rows into the draft at
commit (a runtime step distinct from this read-path feature), and the final `CALL mail.send` — which
stays behind the explicit irreversible gate (`--commit-irreversible`), never automatic.

::: tip How to know what joins
`qfs describe <path>` reports a node's verbs and its **pushdown** line — which filters run inside
the service vs. locally. It answers today for `/local`, `/mail`, and `/github` nodes; `describe` for
`/sql` and `/git` is still being wired (it returns `unknown_mount`), though the joins themselves
already run. Whatever the pushdown split, qfs over-fetches safely and re-checks locally, so you never
get wrong rows — only a bigger or smaller share of the work pushed down.
:::
