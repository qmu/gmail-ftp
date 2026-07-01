# Get started

This is the end-to-end on-ramp: from zero to a local query, converting a file, querying a database,
**joining across sources**, connecting a cloud service, and previewing then committing a change —
each step runnable and building on the last. Everything up to *Connecting a real service* runs
**offline with no credentials** — against your local filesystem (`/local`), the qfs system catalog
(`/sys`), and any local SQLite database or git repository you point qfs at. Follow along immediately
after [installing](/guide/installation).

## The loop

Every task in qfs follows the same four steps:

1. **Describe** a path to learn what it is and what you can do with it.
2. **Write** a query against it.
3. **Preview** — qfs shows you the exact plan, but changes nothing.
4. **Commit** — you add `--commit` to actually do it.

Let's do it.

> The walkthrough below shows the human table you see in an interactive terminal. When qfs's output
> is piped or redirected it emits JSON instead (so it composes with other tools) — see
> [Output formats](#output-formats). Add `--format table` to any command to force the table.

## 1. Describe a path

`describe` tells you everything about a node — with no credentials and no network. Point it at a
real directory on your machine. `/local` takes an **absolute** host path (note the single-slash
join: `/local` + `/home/you/project`):

```sh
qfs describe /local/home/you/project
```

```text
path:      /local/home/you/project
archetype: blob (UPSERT REMOVE LS CP MV RM)
columns:
  name     | type      | null
  -------- | --------- | ----
  name     | Text      | no
  path     | Text      | no
  size     | Int       | no
  modified | Timestamp | no
  is_dir   | Bool      | no
  mode     | Int       | no
verbs:     UPSERT REMOVE LS CP MV RM
procedures: (none)
aliases:   (none)
pushdown:  project
```

That single report tells you the columns you can query (`name`, `size`, `is_dir`, …), the **verbs**
this path supports, and which projections get pushed down. Every path in qfs — local or cloud —
describes the same way.

## 2. Your first queries (all offline)

### List a directory

A directory is a relation. Query it like a table:

```sh
qfs run "/local/home/you/project |> select name, size, is_dir"
```

```text
name        | size | is_dir
----------- | ---- | ------
config.json | 22   | false
notes.txt   | 17   | false
sub         | 0    | true
(3 row(s))
```

Those are real rows read off your disk — no credentials, no network.

### Read a file and convert formats

Point `/local` at a single file to read its **content**, then pipe it through codecs. Here a JSON
file is decoded and re-encoded as YAML:

```sh
qfs run "/local/home/you/project/config.json |> decode json |> encode yaml"
```

```text
content
---------------------
- k: 1
  name: alpha
(1 row(s))
```

`decode json` on its own unpacks the file into rows (`{"k":1,"name":"alpha"}`); add `|> encode yaml`
(or `toml`, `csv`, …) to transcode. Codecs must be the **final** stages of a pipeline — a relational
operator after a codec is rejected.

### Query the system catalog

`/sys` exposes qfs's own administrative state. Describe it, then read it — still no credentials:

```sh
qfs describe /sys/users
```

```text
path:      /sys/users
archetype: relational (SELECT)
columns:
  name          | type | null
  ------------- | ---- | ----
  id            | Int  | no
  primary_email | Text | no
  status        | Text | no
  created_at    | Text | yes
verbs:     SELECT
procedures: (none)
aliases:   (none)
pushdown:  (local-only — filter/project run in qfs)
```

```sh
qfs run "/sys/audit |> limit 3"
```

On a fresh install the audit log is empty (`(0 row(s))`); it fills as you commit changes.

### Query a local database

`/sql` reads any SQLite file you name with an env var — `QFS_SQL_<conn>=<path>`. This is offline too;
the env var just points qfs at a local database (no credential involved):

```sh
export QFS_SQL_ORDERS=/home/you/orders.db
qfs run "/sql/orders/orders |> where total > 100 |> select customer, total |> order by total desc"
```

```text
customer | total
-------- | -----
Initech  | 520
Acme     | 250
Umbrella | 150
(3 row(s))
```

The `WHERE`/`ORDER BY`/`LIMIT` push down **into** SQLite. A local git repository works the same way
via `QFS_GIT_<repo>=<path>` and paths like `/git/<repo>/commits`. See the
[Cookbook](/cookbook/) for more.

### Join across sources

This is what qfs is *for*: because every source is the same kind of path, you can `JOIN` across them
in one statement. Both `/sql` and `/git` read offline, so this runs end to end — match author records
in a table to the commits they wrote (set `QFS_SQL_ORDERS` and `QFS_GIT_MYREPO` first):

```sh
qfs run "/sql/orders/authors |> join /git/myrepo/commits on name == author |> select name, team, message"
```

```text
name           | team     | message
-------------- | -------- | ---------------
Test <t@e.com> | platform | add feature
Test <t@e.com> | platform | initial commit
(2 row(s))
```

qfs pushes each side's filters down to its own service, then joins the results locally — a SQL table
and a git repo combine as easily as two database tables. The
[Cross-service cookbook](/cookbook/cross-service) has more, including joins that bring in `/github`
and `/mail` once those are connected.

## 3. Preview a write

`qfs run` **previews by default** — it shows the exact plan and applies nothing. This works offline
for any path, including cloud ones you haven't connected yet, because building the plan needs no
credentials:

```sh
qfs run "insert into /mail/drafts values ('alice@example.com', 'Hi', 'Body text')"
```

```text
PREVIEW: 1 effect(s)
  #0 INSERT -> mail:/mail/drafts [affected 1]
  total affected: 1
```

The preview shows what *would* happen: one INSERT, one row affected. **No draft was created.** This
is the heart of qfs's safety model — you always see the plan before anything happens.

## Output formats

By default qfs renders the **human table** on an interactive terminal and emits **JSON** when its
output is piped or redirected — so it composes with other tools automatically. The same preview,
piped, looks like this:

```json
{"preview":{"rows":[{"id":0,"verb":"INSERT","target":{"driver":"mail","path":"/mail/drafts"},"affected":{"exact":1},"irreversible":false}],"irreversible":[],"total_affected":{"exact":1},"is_pure":false},"committed":false}
```

Force either format explicitly:

```sh
qfs run "..." --format table   # always the human table
qfs run "..." --json           # always JSON
qfs describe /mail/drafts --json | jq .verbs
```

## 4. Commit

When the preview looks right, add `--commit` to apply it (this is where a live service needs a
connection — see below):

```sh
qfs run "insert into /mail/drafts values ('alice@example.com', 'Hi', 'Body text')" --commit
```

### Irreversible actions need an extra OK

Some actions can't be undone — sending mail, merging a PR, deleting a file. `describe` flags these:
`describe /mail/drafts` lists `CALL send(...) [irreversible]`. In a one-shot command qfs requires an
explicit extra acknowledgement so you can never trigger one by accident:

```sh
# Sending is irreversible — --commit alone is refused (runs once a Google account is connected):
qfs run "/mail/drafts |> call mail.send" --commit --commit-irreversible
```

If you forget the extra flag on an irreversible plan, qfs **fails safely** and tells you why.

## Connecting a real service

Reads and commits against a live cloud service need a connection. Until you add one, qfs is honest
about it — a fresh read returns an **actionable** error (exit code 3), never silent or empty rows:

```sh
qfs run "/mail/inbox |> select date, subject"
```

```json
{"error":{"code":"invalid_path","kind":"capability","message":"invalid path \"/mail/inbox\": connect a Google account to read mail — run `qfs identity signup <email>`, then `qfs connection add gmail` (gmail reads are not available without an authenticated account)","path":"/mail/inbox"}}
```

Once connected, the same query returns real messages. To connect, store a credential once. First
export `QFS_PASSPHRASE` — a password you choose that encrypts the service logins you save on this
machine (it is **not** any service's own password; it just locks the local file your saved logins
live in). It must stay set for the shell that runs `connection add/list/remove`:

```sh
read -rs QFS_PASSPHRASE; export QFS_PASSPHRASE   # unlock the local vault, no shell-history leak
printf %s "$TOKEN" | qfs connection add gmail work  # credential VALUE via stdin, never argv
qfs connection list                                 # shows connection names only, never secrets
```

The credential value is read from **stdin** (not prompted, and never passed on argv where it would
leak into the process table + shell history); qfs never prints it back.

See [Connect a service](/guide/connect) for the exact steps per source (Gmail/Drive, GitHub/Slack,
S3/R2, SQL/git), and [Connections & credentials](/guide/connections) for the full model.

## Where to go next

- **[The Cookbook](/cookbook/)** — dozens of real recipes: cross-service joins, format conversion,
  automation, and more.
- **[How qfs works](/guide/concepts)** — paths, archetypes, previews, and federation explained.
- **[CLI reference](/guide/cli)** — every command and flag.
- **[Interactive shell](/guide/shell)** — explore your services like a filesystem.
