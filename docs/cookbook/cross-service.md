---
skill_name: qfs-cross-service
skill_description: Use when a task spans MORE THAN ONE service in a single qfs query — joining a database to GitHub, a file to a table, or federating several services with one JOIN or UNION over their paths.
---

# Cookbook: Cross-service

This is what qfs is *for*. Because every service is the same kind of path, you can `JOIN` them in a
single statement. qfs pushes each side's filters down to its own service, then joins the results
locally — so a SQL table and a git repo combine as easily as two database tables.

The recipes here mix sources that **read today** (`/sql/<conn>/…`, `/git/<repo>/…`) with ones that
need a connected account (`/github/…`, `/slack/…`). Each is marked.

## Join a database to git history

**Match author records in a table to the commits they wrote** — `/sql` and `/git` both read, so this
runs end to end:

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
The `/sql` leg reads today, but the `/github` leg returns *connect a GitHub account to read it — run
`qfs connection add github`* until you've authenticated. Once connected, the join runs end to end.
:::

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

The dogfooding payoff: **download a file from Google Drive, pack it into a Gmail draft's
`attachments` column, and address the draft** — one composable statement, no bespoke `pack()`. It
uses two small, reusable primitives: a **struct-over-columns** constructor (`{ … }` whose field
values are columns) and single-column **`array_agg`** (N rows → one `Array`).

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
