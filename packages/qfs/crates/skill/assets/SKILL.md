# qfs — AI Operating Procedure

`qfs` exists for you, the AI agent (RFD-0001 §1). Instead of learning N vendor SDKs, you learn
**one** small, filesystem-shaped, pipe-SQL grammar and **one** loop. Every service — mail, drive,
github, slack, sql, git, object storage, and the qfs server itself — is reached the same way.

## The loop (learn this once, use it for everything)

> 1. **DESCRIBE `<path>`** — discover the node's archetype, columns, supported verbs, `CALL`
>    procedures, prelude aliases, and pushdown. This is the **only** thing you read.
> 2. **Write a qfs statement** — path-as-type, closed-core grammar, against what DESCRIBE told you.
> 3. **PREVIEW** — read the typed effect-plan: the affected counts and the `irreversible` flag.
> 4. **COMMIT** — apply at the edge, only after PREVIEW looks right.

```text
qfs describe /mail/drafts -json     # step 1 — the contract
qfs run 'insert into /mail/drafts …'  # step 2+3 — PREVIEW by default
qfs run 'insert into /mail/drafts …' --commit   # step 4 — apply
```

`DESCRIBE` is **pure**: no credentials, no I/O, no network. You can always describe a node to
learn how to address it before touching anything. `qfs run` **PREVIEWs by default**; nothing is
applied until you add `--commit` (or a trailing `COMMIT` keyword).

## What DESCRIBE tells you

`qfs describe <path> -json` emits one stable JSON contract (`DescribeReport`). Read these fields:

| Field          | Meaning                                                                 |
| -------------- | ----------------------------------------------------------------------- |
| `archetype`    | `blob_namespace` / `relational_table` / `append_log` / `object_graph_workflow` — how the node is shaped. |
| `native_verbs` | The FS/SQL-shaped vocabulary the archetype answers to (a one-line hint). |
| `columns`      | The typed schema: `name` + `ty` + `nullable`. Read these to build your statement. |
| `verbs`        | Which **universal verbs** this node supports. Using an unsupported verb fails at parse/resolve time with a structured error. |
| `procedures`   | The `CALL driver.action(..)` signatures, with `irreversible` and `requires_scopes`. |
| `aliases`      | Prelude pure-fn aliases (e.g. `SEND -> mail.send`) — sugar for a `CALL`. |
| `pushdown`     | What the source filters/projects natively (`where`/`limit`/…); the rest runs locally in qfs. |

The four archetypes (RFD §5):

- **blob_namespace** — `ls cp mv rm` (+ universal `upsert`/`remove`). Local FS, S3/R2, Drive, repo files.
- **relational_table** — `SELECT JOIN INSERT UPDATE UPSERT`. SQL DBs, D1, Notion DB.
- **append_log** — `SELECT(tail) INSERT(append)`. Slack, mail, queues, comments, webhooks.
- **object_graph_workflow** — CRUD + `CALL` procedures. GitHub, Linear, K8s.

## The rules (non-negotiable)

1. **Respect the closed core.** The grammar (verbs, keywords) is frozen. You extend qfs **only**
   through the three open registries — **paths**, **functions**, **codecs** (RFD §3). Never invent
   a keyword; if a node does not support a verb, DESCRIBE says so and you pick another.

2. **Always PREVIEW before COMMIT.** Treat `irreversible` plan nodes as **gates** (RFD §6/§10): a
   `REMOVE`, a `CALL mail.send`, a `CALL github.merge` cannot be undone. PREVIEW shows the affected
   count and the `irreversible` flag; only `--commit` (and, in a non-interactive one-shot, an
   explicit `--commit-irreversible`) applies them.

3. **Least privilege** (RFD §10). Scope every plan to the minimum drivers/verbs. On the server,
   request a `POLICY` (see the `/server/...` example) rather than broad access. **Never echo or log
   a resolved credential** — `DescribeReport` carries schema + capabilities only, never secrets.

4. **Idempotency & recovery** (RFD §6). `UPSERT` is the retry-safe default (create-or-replace).
   For read-then-write, use `@version`/ETag optimistic concurrency. `cp` is **copy → verify →
   delete** (never a lossy move). The **audit ledger is the recovery source of truth** — on a
   retry, reconcile against it, not against a guess.

5. **The loop is uniform.** Every example below uses the **identical four steps**. If you ever feel
   you need a per-driver special case, the driver contract is under-declaring — that is a `t13`
   contract bug to fix in the driver, never prose to bolt onto this skill. DESCRIBE is the only
   thing you read.

6. **Cloud connections require sign-in (M4).** A driver that reaches an external service over OAuth
   (gmail, gdrive, ga, github, slack, objstore, cf) is a **cloud** driver: it is unusable until you
   have signed in to qfs identity (`qfs identity signup <email>`) **and** added a connection for it
   (`qfs connection add <driver> <name>`), which records your consent against that connection. An
   unauthenticated operator fails closed — a cloud driver will not bind a credential at COMMIT
   without a consented, signed-in connection. Local drivers (`local`, `git`, `sql`, `sys`) are
   ungated. **DESCRIBE and a write-plan PREVIEW stay pure** — they build the contract / effect-plan
   with no credential bind, so `qfs run 'insert into /mail/drafts …'` previews fine from a bare
   binary. **Reading live rows from a cloud service is different**: `/mail/inbox`, `/github/…`,
   `/slack/…`, `/drive/…` reach the backend, so with no connection they **fail closed at resolve
   time** (exit 3, `kind: capability`) with an actionable nudge — e.g. `connect a Google account to
   read mail — run qfs identity signup <email>, then qfs connection add gmail`. That is the agent's
   cue to connect an account, not a syntax error.

---

## Worked examples (verified against the v0.0.10 binary)

Every statement below was run against the shipped binary. Two kinds run **from a bare binary with
no account connected**:

- **Local-family READS** — `/local`, `/sys`, `/sql` (via a `QFS_SQL_<conn>=<path.sqlite>` env), and
  `/git` (via a `QFS_GIT_<repo>=<path>` env) — return real rows.
- **Write-plan PREVIEWs** for **any** driver — `insert/update/upsert into /path …` builds the typed
  effect-plan with no credential bind, so the PREVIEW prints (`"committed": false`) cred-free.

What needs an account is **reading live rows from a cloud service** (gmail / github / slack, and
soon drive) and **committing** anything. Those fail closed with an actionable connect-account error
(see each example). The golden corpus (`crates/skill/tests/`) re-proves the PREVIEW plans against
**in-test fixture drivers**, so it is hermetic; a bare binary reproduces the same write-plan
PREVIEWs but resolves cloud *reads* to the connect-account error until you connect.

### local + sys — reads that run today (no creds)

These are the fastest way to confirm the binary works end-to-end:

```text
/local/tmp/work/d |> select name, size, is_dir          -- /local + an ABSOLUTE host path -> rows
/local/tmp/work/d/config.json |> decode json |> encode yaml   -- transcode JSON->YAML (codec stages)
/sys/audit |> order by seq desc |> limit 20             -- the hash-chained admin ledger (rows; empty on a fresh deployment)
```

Codecs (`decode`/`encode` over `json`/`yaml`/`toml`/`csv`/…) must be the **final** stages — a
relational op after a codec errors `codec_then_query`.

### mail — append_log (`INSERT INTO /mail/drafts` PREVIEW; live read + send need a connection)

1. **DESCRIBE** `qfs describe /mail/drafts --json` (pure, cred-free):
   ```json
   { "archetype": "append_log",
     "native_verbs": "SELECT(tail) INSERT(append) UPSERT REMOVE",
     "verbs": { "insert": true, "upsert": true, "select": true, "remove": true },
     "procedures": [ { "name": "send", "irreversible": true,
                       "params": [ {"name":"to"}, {"name":"subject"}, {"name":"body"} ] } ],
     "aliases": [ { "name": "SEND", "desugars_to": "mail.send" } ] }
   ```
2. **Statement + PREVIEW (runs now)** — drafting a message is a write-plan; its PREVIEW prints
   cred-free:
   ```text
   insert into /mail/drafts values ('alice@example.com', 'Hi', 'Body text')
   ```
   PREVIEW shows one reversible `INSERT` (`affected 1`, `irreversible: false`, `committed: false`).
3. **Needs a connected account** — reading the inbox and sending both reach Google:
   ```text
   /mail/inbox |> limit 5                 -- live read
   /mail/drafts |> call mail.send         -- the irreversible send (CALL mail.send)
   ```
   With no account these return a `capability` error (exit 3): *connect a Google account to read
   mail — run `qfs identity signup <email>`, then `qfs connection add gmail`*. Once connected,
   `/mail/<label>` returns real messages and `CALL mail.send` is the **irreversible** send (a
   one-shot commit needs `--commit --commit-irreversible`; a retry re-sends the same draft id).

### drive — blob_namespace (`UPSERT INTO /drive/…` PREVIEW; reads coming soon)

1. **DESCRIBE** `qfs describe /drive/my/Reports --json` → `"archetype": "blob_namespace"`, universal
   `upsert` (+ `ls/cp/mv` listed as native verbs).
2. **Statement + PREVIEW (runs now)** — a retry-safe blob write. `cp`/`mv`/`rm` are
   **interactive-shell-only** builtins, *not* one-shot grammar — so a one-shot uses the closed-core
   `UPSERT` form the shell lowers to (and the golden corpus pins):
   ```text
   upsert into /drive/my/Reports/report.pdf values ('report-bytes')
   ```
   PREVIEW shows one reversible `UPSERT` (`affected 1`, `committed: false`).
3. **Reads coming soon** — `/drive/my/Reports |> select name` currently returns the same
   connect-account error (drive read wiring lands next): *connect a Google account to read Drive …
   `qfs connection add gdrive`*. COMMIT of the upsert needs a connected account.

### github — object_graph_workflow (`INSERT INTO …/issues` PREVIEW; reads + merge need a connection)

1. **DESCRIBE** `qfs describe /github/acme/web/pulls --json` → `"archetype":
   "object_graph_workflow"`, a `merge` procedure with `irreversible: true`.
2. **Statement + PREVIEW (runs now)** — open an issue (reversible object-graph `INSERT`):
   ```text
   insert into /github/acme/web/issues values (title) ('Tracking bug')
   ```
3. **Needs a connected account** — reading PRs and merging reach GitHub:
   ```text
   /github/acme/web/pulls |> select number, title          -- live read
   /github/acme/web/pulls/42 |> call github.merge(method => 'squash')   -- irreversible merge
   ```
   With no token these return *connect a GitHub account … `qfs connection add github`*. A merge is
   **irreversible** — once connected, a one-shot commit needs `--commit --commit-irreversible`.

### slack — append_log (`INSERT INTO /slack/<ws>/<channel>/messages` PREVIEW; live read needs a connection)

1. **DESCRIBE** `qfs describe /slack/acme/general/messages --json` → `"archetype": "append_log"`,
   `native_verbs: "SELECT(tail) INSERT(append) REMOVE"`. (A bare `general` or symbolic `#general`
   both address the channel — ordinary path segments. Use the bare form in a write target.)
2. **Statement + PREVIEW (runs now)** — append a message (reversible `INSERT`):
   ```text
   insert into /slack/acme/general/messages values ('Deploy finished')
   ```
3. **Needs a connected account** — `/slack/acme/general/messages |> limit 5` returns *connect a
   Slack workspace … `qfs connection add slack`*. An append log has no `UPDATE`: an
   `update /slack/…` is rejected at resolve time with `unsupported_verb` and a
   `supported: [SELECT, INSERT, REMOVE]` set — pick from it.

### sql — relational_table, pushdown (real read via `QFS_SQL_<conn>`)

1. **Configure a connection by env** — `QFS_SQL_ORDERS=/path/to/orders.db` mounts `/sql/orders`.
2. **Statement (runs now — real rows)** — a filtered, projected, ordered read; the `WHERE`
   predicate pushes **down** into the database:
   ```text
   /sql/orders/orders |> where total > 100 |> select customer, total |> order by total desc
   ```
3. **Write-plan PREVIEW** — a table is full-CRUD; an `update`/`upsert`/`insert` previews cred-free:
   ```text
   update /sql/orders/orders set total = 999 where id == 1
   ```
   PREVIEW shows one reversible `UPDATE` node (`committed: false`); `--commit` applies it.

### git — multi-archetype (real read via `QFS_GIT_<repo>`)

1. **Configure a connection by env** — `QFS_GIT_MYREPO=/path/to/repo-or-.git` mounts `/git/myrepo`.
   The commit log is relational/append; a worktree as-of a ref is a versioned blob namespace.
2. **Statement (runs now — real rows)** — read history, and read a tree as-of a ref (the `@<ref>`
   temporal coordinate is in the **path**):
   ```text
   /git/myrepo/commits |> select sha, message
   /git/myrepo@v1/ |> select name, kind
   ```
3. **Write-plan PREVIEW** — recording a commit previews cred-free (the git applier lowers it to the
   real object writes + ref move):
   ```text
   insert into /git/myrepo/commits values ('add feature', 'main')
   ```
   COMMIT writes the commit; use `@<ref>` for optimistic concurrency on a read-then-write.

### server — `/server/...` binding (`CREATE TRIGGER … DO <plan>`)

1. The `/server` self-config driver is configured **by writing rows**, like any other node.
2. **Statement + PREVIEW (runs now)** — a `CREATE TRIGGER` desugars to a `/server` config write:
   ```text
   create trigger notify on inbox do insert into /log values ('mail arrived')
   ```
   PREVIEW prints the pure config-plan (`committed: false`) — the **PREVIEW-as-CI-test** pattern
   (RFD §8): you assert the plan a fired handler would commit, with no socket and no backend.
3. **COMMIT** — `--commit` installs the trigger. On the server, pair it with a `POLICY` (t35) to
   scope the handler to least privilege; never grant more drivers/verbs than its plan needs.

---

## Beyond the loop (the shipped surface)

The loop is identical no matter how you reach qfs, and these surfaces are live today:

- **Three faces, one engine.** You may be driving qfs as the **CLI**, over the **MCP endpoint**
  (the same DESCRIBE → PREVIEW → COMMIT loop exposed as MCP tools), or behind the **web dashboard**
  whose **approval cards** route a pending irreversible commit to a human for sign-off. The grammar,
  the archetypes, and the gates are the same on all three.
- **Administration is paths.** The deployment's own state is queryable under `/sys/*`:
  `users`, `projects`, `audit`, `connections`, `policies`, `metrics`, `settings`, `billing`. Read
  them with the same grammar (`/sys/audit |> order by seq desc |> limit 20`). `/sys/audit` is the
  append-only, hash-chained record; `/sys/connections` shows names/metadata only (never a secret).
- **Selectable safety mode.** The deployment's AI safety mode lives in `/sys/settings` and governs
  how strict the commit gate is, **above** the always-on safety floor (PREVIEW default + the
  irreversible ack). You never bypass it; you describe → preview → commit and let the gate decide.
- **Teams.** Membership comes from `qfs invite create` (one-time, expiring token) → `invite redeem`.
  Membership is not a capability — a `POLICY` / the ACL is what authorizes an action (default-deny).

## Quick reference

- **PREVIEW is the default.** `qfs run '<stmt>'` previews; add `--commit` to apply.
- **Irreversible = gate.** `REMOVE`, `CALL mail.send`, `CALL github.merge`. One-shot needs
  `--commit-irreversible`.
- **Unsupported verb = structured error.** The error lists the `supported:` set — pick from it.
- **Idempotent default = `UPSERT`.** Retry-safe. Use `@version`/ETag for read-then-write races.
- **`cp`/`mv`/`rm` are interactive-shell-only.** A one-shot uses the closed-core verb they lower to
  (`cp`→`upsert into /path …`, `rm`→`remove …`). `cp` is copy → verify → delete (never lossy); the
  audit ledger is the recovery source of truth.
- **Secrets never appear.** Not in DESCRIBE, not in logs. Request a `POLICY` for least privilege.
