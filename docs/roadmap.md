# Where qfs is going next

This page is the **plan**, not a description of what works today. The [guide](/guide/concepts) and
[cookbook](/cookbook/) document only shipped behavior; this is the one page that talks about ideas
that are not built yet. Each item is tagged **✅ shipped** (real today) or **🧭 proposed** (planned).

## A 30-second orientation (so this page stands alone)

qfs gives every external system **one address space and one query language**. A *path* names a
source, and you query it with a SQL-like, left-to-right pipe (`|>`). The sources qfs knows about:

| Path | What it is |
| --- | --- |
| `/local` | files and folders on your own computer |
| `/sql/<name>` | a SQL database (SQLite, Postgres, MySQL) you've connected |
| `/git/<name>` | a git repository on disk |
| `/mail` | Gmail |
| `/drive` | Google Drive |
| `/github`, `/slack` | GitHub and Slack |
| `/s3`, `/r2` | object storage — Amazon S3 and Cloudflare R2 |
| `/ga` | Google Analytics |
| `/sys` | qfs's own admin data (users, audit log, policies, settings) |

A few verbs you'll see below:

- **read** — run a query against a path and get rows back (`/sql/orders/customers |> where … |> select …`).
- **describe** — inspect a path's shape (its columns and which writes it allows) *without touching it*.
- **preview** — show exactly what a write *would* do, without doing it (qfs previews by default; you
  add `--commit` to actually apply).
- **connect** — give qfs the login a cloud service needs (a token / OAuth), stored encrypted.

## What just shipped ✅ — the "make the docs true" cycle

The product used to *describe* features that errored the moment a new user tried them. This cycle
wired the binary so the headline things actually run:

- **Convert a file's format in one line.** `/local/config.json |> decode json |> encode yaml` really
  turns that JSON file into YAML now (it used to silently do nothing).
- **Query a SQL database**, with the filtering done *inside* the database (`where total > 100` runs
  as real SQL), and **query a git repository** — its commit history, branches, tags, and the files
  in its latest snapshot.
- **Cloud sources fail honestly.** Reading `/mail` or `/github` with no usable account now returns
  a clear, actionable *"connect a … account — run `qfs account add …`, then `qfs connect …`"*
  message instead of a cryptic internal error. Once you connect Gmail, reading `/mail` returns your
  real messages.
- **Quieter, more honest output.** Unrelated commands no longer print confusing sign-in warnings, and
  `describe` now reports the *actual* operations a path supports.

Every documentation page was rewritten and checked, example by example, against the running binary.

## Connections **in the language** ✅ — shipped

You now declare a connection with a `CREATE CONNECTION <name> DRIVER <driver> [AT '<locator>']
[SECRET '<ref>']` statement in a `connections.qfs` file (point at it with `QFS_CONNECTIONS=<file>`,
or `~/.config/qfs/connections.qfs`). A declared `DRIVER sqlite` / `DRIVER git` connection mounts
`/sql/<name>` / `/git/<name>` with **no env var** — the declaration is the reviewable, committable
source of truth. `SECRET` is a reference (`env:<VAR>` / `vault:<driver>/<connection>`), never an
inline value. The old `QFS_SQL_*` / `QFS_GIT_*` env vars are a warned, deprecated fallback —
`qfs connect --import-env` prints the equivalent declarations. (Extending declared mounts to
Postgres/MySQL and the cloud drivers is the follow-up; the design that got us here is below.)

A **connection** is a named pointer to one source: *which* database/repo/account it is, and how to
reach it. The problem this solved:

### What's wrong today

To use a SQLite database at `/sql/orders`, you don't *declare* anything — you set an environment
variable named `QFS_SQL_ORDERS=/path/to/orders.db`, and qfs turns the **name of that variable** into
the path segment `orders`. So the connection is invisible: there's no statement you can read, review,
or check into a repo — the source of truth is the *name of an env var*. Cloud accounts work a
completely different way (a separate encrypted store plus a "which one is active" toggle). Two
unrelated, mostly-undiscoverable mechanisms.

### The plan: a `CREATE CONNECTION` statement

Make a connection an **explicit statement**, just like the `CREATE TRIGGER` / `CREATE POLICY`
statements qfs already has — something you can read, review, and commit to a file. Secrets are never
written in the file; you *point* at where the secret lives (an environment variable, or qfs's
encrypted store).

```text
🧭 proposed — you declare the connection; the file never contains a secret

CREATE CONNECTION orders                    -- usable at /sql/orders/<table>
  DRIVER sqlite
  AT '/data/orders.db'                       -- a local file: no secret needed

CREATE CONNECTION analytics                 -- usable at /sql/analytics/<table>
  DRIVER postgres
  AT 'postgres://db.internal/analytics'      -- where the database is
  SECRET env:PG_PASSWORD                      -- the password comes from the PG_PASSWORD env var

CREATE CONNECTION work                      -- usable at /mail/work/...
  DRIVER gmail
  SECRET vault:gmail/work                     -- the Gmail token from qfs's encrypted store
```

- **`DRIVER`** says what kind of source it is, which decides the path: `sqlite`/`postgres`/`mysql`
  live under `/sql`, `git` under `/git`, `gmail` under `/mail`, and so on. The **name** you give the
  connection (`orders`, `work`) is the segment you type in the path.
- **`AT`** is the plain, non-secret location — a file path, a database URL, a bucket. (Omitted when
  the location is fixed, e.g. Gmail.)
- **`SECRET`** *references* the secret, never the value: `env:NAME` reads an environment variable;
  `vault:path` reads qfs's encrypted credential store (the same store `qfs account add` writes to).
  A local SQLite file or git repo needs no secret at all.
- These declarations live in a plain config file (e.g. `connections.qfs`) you can keep in a repo —
  not in the names of environment variables.

The old `QFS_SQL_…` env-var trick stays working for one release as a deprecated fallback, with a
one-command migration that prints the equivalent `CREATE CONNECTION` lines for you.

### How it'll be built (tracked as an epic in the backlog)

| Step | What it does |
| --- | --- |
| **Design (first, the keystone)** | nail down the exact grammar and a couple of open questions — e.g. should the connection name always be *in the path* (`/mail/work/…`), to match how `/sql` already works? |
| **Parser** | teach the language to read `CREATE CONNECTION` |
| **Secret resolution** | implement `env:` and `vault:` lookups (read lazily, never logged) |
| **Wire it up** | build qfs's connection registry from these declarations instead of scanning env-var names |
| **Load from config** | load a `connections.qfs` when running the server, a scheduled job, or `qfs run --config` |
| **Retire the old way** | the `QFS_SQL_…`/`QFS_GIT_…` env vars become a warned, temporary fallback |
| **Tidy the CLI** | `qfs account add` becomes "store the secret a `vault:` reference points at" |
| **Docs** | rewrite the connection guide around this (done last, so the docs never get ahead of the code) |

## Near-term backlog 🧭 — known gaps to close

A batch of these just shipped (see *Just shipped from this backlog* below); what still remains:

- **Finish the Cloudflare & HTTP/REST mounts.** Both drivers are now **reachable as paths** — `/rest`
  describes and appears in the driver catalogue, and `/cf` is a mounted, plannable path (no longer
  `unknown_mount`). What remains is the per-resource config they need (which D1/KV/queues; which REST
  resource maps) and their live credentialed read/commit — best sourced from a richer `CREATE
  CONNECTION` declaration than the current `(driver, locator, secret)` shape carries.
- **Spell out `/ga`.** Reading `/ga/<property>` works now, but `/ga` is a cryptic mount name — it
  should be spelled out or aliased to `/analytics`, as a deliberate, deprecation-guarded change to
  the (versioned) path surface.
- **Push even more into the source.** A SQL backend now runs the `WHERE`/`ORDER BY`/`LIMIT`, Gmail
  runs its `q=` search, and Google Analytics runs the whole `runReport`. Still done locally: SQL/GA
  **column projection** (it changes the row shape vs. the described schema and can strip a column a
  residual still needs), plus cross-source aggregates and joins. Worth deepening over time.

### Just shipped from this backlog ✅

- **Read Google Drive and Google Analytics for real.** `/drive/...` lists a folder's children
  (resolving each path name to its Drive folder id), and `/ga/<property> |> select … |> where date …`
  runs a real GA4 report — both for a connected account.
- **Read a git repo as it was in the past.** `/git/app@v1.2/…` reads the tree at that commit/tag
  instead of the latest.
- **Markdown as a file format.** `… encode md` / `decode md` now resolves to the front-matter codec.
- **Write local files.** `upsert into /local/<file>` persists on `--commit`, including a positional
  `values ('hi')` payload.
- **Faster source-side queries.** Gmail's search filters, and SQL's `ORDER BY`/`LIMIT`, now run in
  the source instead of in qfs after fetching.
- **Onboarding & polish.** Plain-language passphrase text; a local-first first command after install;
  a per-source [Connect a service](/guide/connect) guide; an end-to-end [Get started](/guide/getting-started)
  on-ramp; and TTY-aware terminal color (honoring `NO_COLOR` / `--no-color`).
