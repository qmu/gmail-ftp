---
name: qfs
description: Use when a task needs to read or modify any external service the user has connected — mail, files, databases, GitHub, Slack, git, cloud storage — via the `qfs` CLI and its pipe-SQL query language. Covers the query syntax and semantics, the describe→preview→commit loop, one-shot execution, and the safety model.
---

# Driving services with qfs

`qfs` exposes every external service as a **filesystem of paths** queried with **one small pipe-SQL
language**. Mail is `/mail/inbox`, a database table is `/sql/pg/orders`, a repo's pull requests are
`/github/acme/web/pulls`, a Drive folder is `/drive/Reports`, a bucket object is `/s3/bucket/key`.
The same grammar reads, joins, transforms, and writes across all of them.

Prefer **one-shot** commands (`qfs run '<statement>'`, `qfs describe <path>`) — each runs once and
exits, which is what you want as an agent. The interactive shell (`qfs` with no args) exists but you
generally won't use it.

The repo's `docs/` (and `qfs skill` / `qfs skill --examples`, printed from the binary) are
authoritative; this skill is the quick operating guide.

## Prerequisites (check these first)

- **The binary.** Use an installed `qfs`, or build it: `cd packages/qfs && cargo build --release`
  → `packages/qfs/target/release/qfs`. `qfs --version` confirms it runs.
- **Credentials are only needed to COMMIT against a live service.** `describe` and `preview` work
  offline with no credentials at all. To apply real changes, the user adds a connection once. This
  needs **`QFS_PASSPHRASE`** exported first — the master passphrase that unlocks the local encrypted
  store (an argon2id KDF over the at-rest vault, NOT a service credential) — and reads the
  credential VALUE from stdin, never argv (argv leaks into the process table + shell history):

  ```sh
  read -rs QFS_PASSPHRASE; export QFS_PASSPHRASE        # unlock the vault, no shell-history leak
  printf %s "$TOKEN" | qfs connection add mail work     # credential value via stdin, never argv
  ```

  `QFS_PASSPHRASE` must stay set for the shell running `connection add/list/remove`. Names are safe
  to print; the secret is never echoed. `qfs connection list` shows configured connections.

## The loop (do this for every task)

1. **`qfs describe <path>`** — learn the node's archetype, columns, supported verbs, `CALL`
   procedures, and which filters push down. Pure: no creds, no network. **Always read this first.**
2. **Write a statement** against what describe told you.
3. **`qfs run '<statement>'`** — **previews by default**: prints the effect-plan (paths, affected
   counts, and an `irreversible` flag) without touching anything.
4. **Add `--commit`** to apply, once the preview looks right.

```sh
qfs describe /mail/drafts --json                 # 1: the contract
qfs run "insert into /mail/drafts values ('alice@example.com','Hi','Body')"          # 2+3: PREVIEW
qfs run "insert into /mail/drafts values ('alice@example.com','Hi','Body')" --commit # 4: apply
```

## Path model

- Paths are **absolute** (start with `/`); there is no working directory in one-shot mode.
- A node belongs to one of four **archetypes**, which decides its verbs (`qfs describe` shows them):
  - **blob namespace** (files: local, S3/R2, Drive) — `SELECT`, `UPSERT`, `REMOVE`
  - **relational table** (SQL DBs, D1) — `SELECT`, `JOIN`, `INSERT`, `UPDATE`, `UPSERT`
  - **append log** (mail, Slack, queues) — `SELECT` (read tail), `INSERT` (append)
  - **object graph** (GitHub, Linear) — `SELECT`, `INSERT`, `UPDATE`, `REMOVE`, `CALL`
- **Using an unsupported verb is rejected up front** with a structured error listing the supported
  set — so describe, then pick a supported verb.
- Some paths take a coordinate, e.g. git: `/git/repo@v1.2/src/main.rs` reads as of a ref.
- `SELECT/INSERT/UPSERT/UPDATE/REMOVE` are the only verbs. `ls`/`cp`/`mv`/`rm` are just shell
  aliases for them (`ls`=SELECT listing, `cp`=UPSERT, `rm`=REMOVE) — interactive shell only.

## The query language (syntax)

A statement is a **source** followed by **stages** joined by `|>` (a pipe). Write multi-stage
statements one stage per line:

```qfs
/sql/pg/orders
|> where total > 100 AND status IN ('open', 'pending')
|> select id, total, status
|> order by total DESC
|> limit 5
```

The **source is a leading `/path`** (a `LET`-bound name also works); there is no `FROM` keyword — it
was removed from the closed core. Bind with `=`, compare with `==`. Read/transform stages:
`where <cond>` (`==`, `<>`, `<`, `>`, `<=`, `>=`, `LIKE`, `IN`, `BETWEEN`, `AND`/`OR`),
`select <cols>` (rename with `as`), `extend <col> = <expr>`, `join <path> on <cond>` (works
**across services**), `aggregate <fn>(<col>) as <name>` (+ `group by`), `order by <col> [DESC]`,
`limit <n>`, `distinct`, and `union`/`except`/`intersect <path>`.

Effect (write) stages and statements:

```qfs
insert into /slack/acme/general/messages values ('Deploy done')
upsert into /drive/my/Reports/report.pdf values ('…bytes…')   -- retry-safe blob write
update /sql/pg/orders set status = 'shipped' where id == 7
remove /mail/inbox where subject LIKE '%spam%'             -- REMOVE takes a path + WHERE
/github/acme/web/pulls/42 |> call github.merge(method => 'squash')
```

Codecs convert formats — `decode`/`encode` with `json`, `jsonl`, `yaml`, `toml`, `csv`, `md`:

```qfs
/local/config.json
|> decode json
|> encode yaml
```

Cross-service join (qfs pushes each side's filters down, then joins locally):

```qfs
/sql/pg/orders
|> join /github/acme/web/issues on id == issue_id
|> select id, title
```

See `docs/cookbook/` in the repo for many more recipes.

## JSON output (prefer this as an agent)

`qfs` prints a human table on a TTY and JSON when piped; force it with `--json` (or
`--format json`). Parse the JSON rather than scraping the table:

```sh
qfs describe /mail/drafts --json | jq '.verbs, .procedures'
qfs run "/sql/pg/orders |> where total > 100 |> select id" --json
```

A preview's JSON includes `preview.rows` (each with `verb`, `target`, `affected`, `irreversible`),
`total_affected`, and `committed: false`. After `--commit`, `committed: true`.

## Safety model — preview vs. commit (non-negotiable)

- `qfs run` **previews by default**; nothing changes until you pass `--commit`.
- **Irreversible effects** — sending mail (`CALL mail.send`), merging a PR (`CALL github.merge`),
  deleting/trashing (`REMOVE`) — are flagged `irreversible: true` in the preview. In a one-shot,
  applying them needs **both** `--commit` and `--commit-irreversible`; without the extra flag qfs
  refuses (fails closed). Treat these as gates: preview, confirm intent, then commit.
- `UPSERT` is the retry-safe default for writes (create-or-replace; re-running converges).

```sh
# Reversible: --commit is enough
qfs run "insert into /mail/drafts values ('alice@example.com','Hi','Body')" --commit
# Irreversible: needs the explicit ack
qfs run "/mail/drafts |> call mail.send" --commit --commit-irreversible
```

## Error / exit contract (for scripting)

- Exit `0` = success. Non-zero = failure; the error body goes to **stderr** as JSON with a `kind`:
  `parse` (bad syntax), `usage` (e.g. a relative path), `capability` (unknown/unsupported
  source or verb), `auth`, `internal`.
- A `capability` error on a read often just means no account/backend is connected for that
  service yet — the statement's syntax is fine.
- Credentials never appear in any output (not in `describe`, not in logs, not in errors).

## Gotchas

- Statements use **absolute paths** in one-shot mode — no `cd`, no relative paths.
- `remove` is `remove <path> where …`, not a pipe stage.
- Don't reach for a verb describe didn't list (e.g. `update` on an append log) — it's rejected.
- Keywords are **canonical lowercase** (recognition is case-insensitive, but write lowercase). There
  is **no `from`** — a leading `/path` is the source. Bind with `=`, compare with `==`.
- Quote interval literals: `create job nightly every '1h' do …`.
- Server bindings (`create endpoint|trigger|job|view|policy`) are statements too — preview them to
  see the exact plan they'd install before `qfs serve` runs them.

## Beyond the loop (the shipped surface)

- **Three faces, one engine.** The same describe → preview → commit loop is reached over the **CLI**,
  the **MCP endpoint** (exposed as MCP tools), or the **web dashboard** whose approval cards route a
  pending irreversible commit to a human for sign-off. Same grammar, same gates everywhere.
- **`/sys/*` is administration as paths.** Query the deployment's own state with the same grammar:
  `/sys/{users,projects,audit,connections,policies,metrics,settings,billing}` (e.g.
  `/sys/audit |> order by seq desc |> limit 20`). `/sys/audit` is append-only + hash-chained;
  `/sys/connections` is names/metadata only.
- **Selectable safety mode** lives in `/sys/settings`, above the always-on floor (preview default +
  the irreversible ack) — you never bypass it; preview, then commit, and let the gate decide.
- **Teams:** `qfs invite create` → `qfs invite redeem` adds a member; a `POLICY` / the ACL (not
  membership) is what authorizes an action (default-deny).
- **Credential lifecycle:** `qfs connection rotate|revoke|rekey` for offboarding + key hygiene.
