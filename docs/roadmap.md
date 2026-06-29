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
- **Cloud sources fail honestly.** Reading `/mail` or `/github` with no account connected now returns
  a clear, actionable *"connect a … account — run `qfs connection add …`"* message instead of a
  cryptic internal error. Once you connect Gmail, reading `/mail` returns your real messages.
- **Quieter, more honest output.** Unrelated commands no longer print confusing sign-in warnings, and
  `describe` now reports the *actual* operations a path supports.

Every documentation page was rewritten and checked, example by example, against the running binary.

## The next change 🧭 — define connections **in the language**

A **connection** is a named pointer to one source: *which* database/repo/account it is, and how to
reach it. Today qfs has no clean way to declare one — and that's the problem.

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
  `vault:path` reads qfs's encrypted credential store (the same store `qfs connection add` writes to).
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
| **Wire it up** | build qfs's connection list from these declarations instead of scanning env-var names |
| **Load from config** | load a `connections.qfs` when running the server, a scheduled job, or `qfs run --config` |
| **Retire the old way** | the `QFS_SQL_…`/`QFS_GIT_…` env vars become a warned, temporary fallback |
| **Tidy the CLI** | `qfs connection add` becomes "store the secret a `vault:` reference points at" |
| **Docs** | rewrite the connection guide around this (done last, so the docs never get ahead of the code) |

## Near-term backlog 🧭 — known gaps to close

These are honest gaps the documentation cycle uncovered. None of them is claimed to work in the
guides; each either fails safely or returns a clear error today.

- **Read Google Drive and Google Analytics for real.** Connecting the account already works, but the
  *read* isn't wired: Google Drive needs to translate a folder *path* into Drive's internal folder
  IDs, and Google Analytics needs to turn a query into one of its report requests (analytics returns
  pre-aggregated metrics, not a plain table). Until then both return the "connect your account" error
  even when connected. *(Also: `/ga` is a cryptic name — it should be spelled out, or renamed to
  `/analytics`.)*
- **Read a git repo as it was in the past.** Reading a repository's history and its *latest* file
  tree works; reading the tree (or a single file's contents) **as of an older commit or tag**
  (`/git/app@v1.2/…`) does not yet — it currently shows the latest version. Time-travel reads are a
  follow-up.
- **Actually write local files.** A `upsert into /local/<file> …` previews and *says* it committed,
  but doesn't yet write the file to disk. The write path needs finishing.
- **Markdown as a file format.** The format converters cover JSON, JSONL, YAML, TOML, and CSV;
  Markdown-with-front-matter exists in the code but isn't wired into that set yet, so `… encode md`
  errors.
- **Two drivers aren't mounted.** A Cloudflare driver and a generic HTTP/REST driver exist in the
  code but aren't reachable from the CLI as paths.
- **Push more work into the source.** Sources do *some* of the query themselves for speed: a SQL
  database runs the `WHERE`, git uses the commit you named. The rest (column selection, sorting,
  limits, Gmail's search filters) is still done by qfs after fetching everything — fine for
  correctness, slower than letting the source do it. Worth deepening over time.

### Onboarding & polish 🧭

- **Plain-language onboarding.** The first-run and credential text is too jargon-heavy for a normal
  user — e.g. `QFS_PASSPHRASE` is described as "the master passphrase that unlocks your local
  credential vault (argon2id KDF; NOT a service credential)." Rewrite it as plain English — *a
  password you choose that encrypts the service logins you save on this machine* — and leave the
  cryptography detail to a "how it's stored" section for those who want it.
- **Color in the terminal.** Command output is all one color and hard to scan. Add color to table
  headers, previews, the irreversible-action marker, and errors — only when writing to a terminal,
  and honoring the standard `NO_COLOR` / a `--no-color` flag.
- **Let the first step succeed.** The "next steps" after install push the reader toward a *cloud*
  command (`qfs describe /mail/drafts`, or connecting an account with the passphrase setup) before
  they've done anything that works. A new user with no account hits a wall and leaves. The first step
  should be a **local** command that returns real output (list a folder, or convert a file) — the
  win comes first; connecting an account comes after.
- **A "connect each service" guide.** Each source needs slightly different setup (Gmail and Drive use
  Google sign-in; GitHub/Slack use tokens; S3/R2 use keys; SQL and git just point at a location).
  That deserves its own short "Get started" page with the exact steps per source, linked from
  everywhere — instead of one generic connections page.
- **"Get started" should be a practical, end-to-end guide.** Today it's a short first-queries page.
  It should grow into the overall on-ramp that walks a new user all the way to the *full* feature set
  — run a local query, convert a file, connect a service, query a database, join across sources, and
  preview/commit a change — each step runnable, building on the last. (The conceptual "How qfs works"
  has moved out of *Get started* into *Using qfs*, so the on-ramp can stay hands-on.)
