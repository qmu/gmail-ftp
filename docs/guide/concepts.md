# Core concepts

Five ideas explain almost everything in qfs. None of them is complicated.

## What runs today

This is the single source of truth for what the `v0.0.10` binary actually does — other pages link
here. Everything below is verified by running the binary as a fresh user with no credentials.

**Reads that run offline, no account, no setup:**

| Source | What you can read | Setup |
| --- | --- | --- |
| `/local/<abs-path>` | Directory listings, single-file **content**, and format conversion via codecs | none — point at any absolute host path |
| `/sys/*` | `users`, `audit`, `connections`, `policies`, `metrics`, `settings`, `billing` | none |
| `/sql/<conn>/<table>` | Rows from a SQLite table; `WHERE`/`ORDER`/`LIMIT` push **into** the database | `QFS_SQL_<CONN>=<path-to.sqlite>` |
| `/git/<repo>/...` | `commits`, `refs`, `tags`, `reflog`, and tree listings | `QFS_GIT_<REPO>=<path-to-repo>` |

The `<conn>`/`<repo>` segment is a **named connection** you define — for `/sql` and `/git` it's an
environment variable whose value is where the database/repo lives (`QFS_SQL_orders=…` → `/sql/orders`);
for credentialed services it's a stored credential (`qfs connection add s3 prod` → `/s3/prod`). See
[Connections & credentials](/guide/connections) for both.

**Write-plan previews run with no account** — `insert`/`update`/`upsert`/`remove into /any/path …`
returns a plan (`"committed": false`) without ever touching the service, because previewing never
reads or writes. (See **Preview vs. commit**, §4 below.)

**Cloud reads need a connected account.** A cloud read with no account fails closed (exit 3) with an
actionable message, not a cryptic error:

```console
$ qfs run "/mail/inbox |> limit 5"
{"error":{ … "message":"connect a Google account to read mail — run `qfs identity signup <email>`,
then `qfs connection add gmail` (gmail reads are not available without an authenticated account)" }}
```

| Source | Status |
| --- | --- |
| `/mail` (Gmail), `/github`, `/slack`, `/s3`, `/r2` | Connect the account, then reads return real rows |
| `/drive`, `/ga` | Connect the account; **read rows are coming soon** |

The rest of this page explains the model. Every fenced example here was run against the binary.

## 1. Everything is a path

Every service is mounted as a tree of paths, like a filesystem:

| Path | What it is | Today |
| --- | --- | --- |
| `/local/notes.md` | A file on your machine | **reads run** |
| `/sys/audit` | qfs's own deployment state | **reads run** |
| `/sql/shop/orders` | A table in a SQL database | **reads run** (set `QFS_SQL_SHOP`) |
| `/git/myrepo/commits` | A git repository's history | **reads run** (set `QFS_GIT_MYREPO`) |
| `/mail/inbox`, `/mail/drafts` | Your mailbox | connect a Google account |
| `/drive/Reports/q3.pdf` | A file in cloud Drive | connect an account (reads coming soon) |
| `/s3/my-bucket/logs/app.log` | An object in S3 / R2 | connect an account |
| `/github/acme/web/pulls/42` | Pull request #42 in a repo | connect an account |
| `/slack/acme/general/messages` | A Slack channel | connect an account |

Paths are always **absolute** (they start with `/`) — there's no "current directory" to get lost
in. You address a single item the same way you'd point at a file. For `/local`, the path is the
absolute host path: `/local/home/you/notes.md`.

Once a repository is wired with `QFS_GIT_MYREPO=<path>`, its history reads directly:

```qfs
/git/myrepo/commits
|> select sha, message
```

```
sha                                      | message
---------------------------------------- | ---------------
81f121ff1ce51a1cb8c2eb3d7939d7b0691e920b | second commit
6de3d5e2b7720ed5c94559a9ede9098e01127b46 | initial commit
```

`/git/myrepo/refs`, `/git/myrepo/tags`, and tree listings (`/git/myrepo/ |> select name, kind`) read
the same way.

## 2. Four archetypes

Behind the scenes every path is one of four **archetypes** — the shape of the data — which decides
what you can do with it. You rarely think about this directly; `describe` just tells you. But it's
useful to know the family:

| Archetype | Shaped like | Verbs you get | Examples | Today |
| --- | --- | --- | --- | --- |
| **Blob namespace** | A folder of files | `SELECT`, `UPSERT`, `REMOVE` | Local files, S3/R2, Drive | `/local` reads run; S3/R2/Drive connect an account |
| **Relational table** | A SQL table | `SELECT`, `JOIN`, `INSERT`, `UPDATE`, `UPSERT` | SQLite, Postgres, MySQL, D1 | `/sql` SQLite reads run; others connect an account |
| **Append log** | A feed you add to | `SELECT` (tail), `INSERT` (append) | Mail, Slack, queues, git history | git history reads run; Mail/Slack connect an account |
| **Object graph** | Things with actions | `SELECT`, `INSERT`, `UPDATE`, `REMOVE`, `CALL` | GitHub, Linear | connect an account |

The key rule: **a path only offers the verbs that make sense for it.** You can't `UPDATE` a Slack
message (an append log doesn't support it) — and qfs rejects it up front with a clear error instead
of failing halfway. `describe` always shows the supported set; for a mail draft, for instance, it
reports `native_verbs: SELECT(tail) INSERT(append) UPSERT REMOVE` (an append log takes `INSERT`, not
`UPDATE`).

### "Wait — what about `ls`, `cp`, `mv`, `rm`?"

There is **one** set of verbs: `SELECT`, `INSERT`, `UPSERT`, `UPDATE`, `REMOVE` (plus `CALL`).
`ls`/`cp`/`mv`/`rm` are **not** a second set — they're familiar filesystem *names* for those same
verbs, available in the [interactive shell](/guide/shell) because a blob namespace looks like a
folder of files:

| Shell shorthand | is just | the real verb |
| --- | --- | --- |
| `ls <dir>` | listing a folder | `SELECT` over the directory |
| `cat <file>` | reading a file | `<file>` |
| `cp <a> <b>` | copying | `UPSERT INTO <b> <a>` |
| `mv <a> <b>` | moving | copy, then `REMOVE <a>` |
| `rm <file>` | deleting | `REMOVE <file>` |

Two more shell builtins — `cd <dir>` and `pwd` — are *pure navigation*, not verbs at all: `cd`
moves you around the blob namespace (and updates the `local:/…$` prompt), `pwd` prints where you are.
They only exist in the [interactive shell](/guide/shell), where the blob namespace is mounted at
`/local`; `cd /sql/…` or `cd /drive/…` reports `unknown_mount` because only `/local` is walkable that
way. To reach across services you use the query language (`/sql/…`, `/git/…`, `/mail/…`), not `cd`.
None of `ls/cd/pwd/cat/cp/mv/rm` is one-shot grammar — `qfs run "cd …"` is a parse error; they are
shell-only conveniences.

So when the [driver catalog](/drivers) lists `ls cp mv rm` for a file store, that's just a hint that
those friendly aliases work there — underneath, it's the same `SELECT`/`UPSERT`/`REMOVE` you use
everywhere else.

## 3. The pipe-SQL language

You query and change paths with one small SQL-like language. A query is a **source** followed by
**stages** joined by `|>` (a pipe). Point `/sql/shop` at a database with `QFS_SQL_SHOP=<path-to.sqlite>`
and this runs:

```qfs
/sql/shop/orders
|> where total > 100
|> select customer, total
|> order by total desc
```

```
customer | total
-------- | -----
Initech  | 520
Acme     | 250
Umbrella | 150
```

Read it top to bottom: start from a table, keep the big orders, pick two columns, sort. The `WHERE`,
`ORDER BY`, and `LIMIT` are pushed **into** the database — SQLite does the filtering and the engine
takes the rows from there.

The read/transform stages you'll use most:

| Stage | Does |
| --- | --- |
| `WHERE <condition>` | Filter rows (`=`, `<`, `>`, `LIKE`, `IN`, `BETWEEN`, `AND`/`OR`) |
| `SELECT <cols>` | Pick columns; rename with `AS` (e.g. `subject AS title`) |
| `EXTEND <col> = <expr>` | Add a computed column |
| `JOIN <path> ON <cond>` | Combine with another path — **even on a different service** |
| `AGGREGATE <fn> AS <name>` | Summarize (`AGGREGATE count(id) AS n`) |
| `GROUP BY <cols>` | Group for aggregation |
| `ORDER BY <col> [DESC]`, `LIMIT <n>`, `DISTINCT` | Sort, cap, dedupe |
| `UNION` / `EXCEPT` / `INTERSECT` `<path>` | Set operations across sources |

The write stages (effects):

| Stage | Does |
| --- | --- |
| `INSERT INTO <path> VALUES (…)` | Add |
| `UPSERT INTO <path> VALUES (…)` | Add-or-replace (retry-safe) |
| `UPDATE <assignments>` | Change matching rows |
| `REMOVE` | Delete matching rows / trash a message |
| `CALL <service>.<action>(…)` | Run a built-in action, e.g. `CALL mail.send` |

And codecs convert formats: `DECODE json`, `ENCODE yaml` (more below).

## 4. Preview vs. commit

This is the safety model, and it's simple:

- **`qfs run` previews by default.** It plans the whole thing and shows you the effects — what
  paths, how many rows, and whether anything is **irreversible** — but touches nothing. A write-plan
  preview runs even before you connect the service:

  ```console
  $ qfs run "insert into /mail/drafts values ('a@b.com','Hi','Body')"
  {"preview":{"rows":[{"verb":"INSERT","target":{"driver":"mail","path":"/mail/drafts"},
    "affected":{"exact":1},"irreversible":false}], … },"committed":false}
  ```

- **`--commit`** applies the plan.
- **Irreversible effects** (sending mail, merging a PR, deleting) need an *extra* acknowledgement
  (`--commit-irreversible`) in a one-shot. Without it, qfs refuses rather than guess.

Reads are always pure — there's nothing to commit. This is what makes qfs safe to hand to an
automation or an AI agent: it can plan freely and only ever acts when explicitly told to.

## 5. Federation: one query, many services

Because every service is the same kind of path, qfs can **combine them in a single query**. It
pushes the parts a service can do natively (a `WHERE`, a `LIMIT`) *down* to that service, then does
the rest — joins, extra filtering, sorting — locally:

```qfs
/sql/shop/orders
|> join /github/acme/web/issues on id == issue_id
|> select id, title
```

That's a SQL table joined to GitHub issues in one query. The `/sql` source runs today; the
`/github` side returns rows once you [connect a GitHub account](#what-runs-today). `describe` shows
each path's **pushdown** so you know what runs where. The [Cookbook](/cookbook/cross-service) is full
of these.

### Codecs: formats are just another stage

A blob of bytes becomes rows with `DECODE`, and rows become bytes with `ENCODE`. Supported formats:
`json`, `jsonl`, `yaml`, `toml`, `csv`, `md`. So converting a file's format really is one line —
here a JSON file comes out as YAML:

```qfs
/local/home/you/config.json
|> decode json
|> encode yaml
```

Given `config.json` containing `{"k":1,"name":"alpha"}`, the result is the converted text in a single
`content` column:

```
content
---------------------
- k: 1
  name: alpha
```

Codecs are final stages — a relational stage after a codec is an error, because once you've encoded
to bytes there are no columns left to query.

## 6. Administration is paths too (`/sys`)

qfs's own deployment state is surfaced as ordinary paths you query with the same grammar — there is
no separate admin API. Under `/sys` you'll find `users`, `projects`, `audit`, `connections`,
`policies`, `metrics`, `settings`, and `billing`. So reviewing the audit trail or the connection
registry is just a read:

```qfs
/sys/audit
|> order by seq DESC
|> limit 20
```

(On a fresh deployment that returns no rows yet — the audit log fills as `/sys` mutations happen.)

The selectable AI **safety mode** lives in `/sys/settings` (a deployment chooses how strict the
commit gate is, above the always-on safety floor); `/sys/audit` is the **append-only, hash-chained**
record every `/sys` mutation writes to — administration observes itself. The redaction is
structural: `/sys/connections` has no column a secret could appear in.

## One engine, three faces

Everything above is the *same engine* no matter how you reach it. `qfs serve` presents that engine
as three faces — the **CLI** (and interactive shell), an **MCP endpoint** for AI agents, and an
**embedded web dashboard** whose approval cards let a human approve a pending irreversible commit.
The path model, the four archetypes, and preview-then-commit are identical on all of them.

## Credentials, briefly

`describe` and `preview` never need a credential. To **commit** against a live service you store one
once with `qfs connection add <service> <name>` — and qfs never prints it back. That command first
needs `QFS_PASSPHRASE` exported (a password you choose that encrypts the service logins saved on this
machine — not any service's own password) and reads the credential value from stdin. See
[Connections & credentials](/guide/connections) for the full flow.

**Next:** put it all together in [the Cookbook →](/cookbook/)
