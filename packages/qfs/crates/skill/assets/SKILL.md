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
qfs run 'INSERT INTO /mail/drafts …'  # step 2+3 — PREVIEW by default
qfs run 'INSERT INTO /mail/drafts …' --commit   # step 4 — apply
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

---

## Worked examples (one per driver — identical four steps)

Each example is: **(1) DESCRIBE excerpt → (2) statement → (3) PREVIEW → (4) COMMIT note.** The
golden corpus (`crates/skill/tests/`) pins the PREVIEW plan of each statement with **no COMMIT, no
network, no live creds**.

### mail — append_log (`INSERT INTO /mail/drafts` then `CALL mail.send`)

1. **DESCRIBE** `qfs describe /mail/drafts -json`:
   ```json
   { "archetype": "append_log",
     "verbs": { "insert": true, "upsert": true, "select": true, "remove": true },
     "procedures": [ { "name": "send", "irreversible": true,
                       "params": [ {"name":"to"}, {"name":"subject"}, {"name":"body"} ] } ],
     "aliases": [ { "name": "SEND", "desugars_to": "mail.send" } ] }
   ```
2. **Statement** — create the draft, then send it (the `SEND` alias desugars to `CALL mail.send`):
   ```text
   INSERT INTO /mail/drafts VALUES ('alice@example.com', 'Hi', 'Body text')
   /mail/drafts |> CALL mail.send
   ```
3. **PREVIEW** — the draft `INSERT` is reversible (affected 1); the `CALL mail.send` is an
   **irreversible** node. PREVIEW shows `irreversible: true` on the send.
4. **COMMIT** — `--commit` applies the draft; the send needs `--commit --commit-irreversible` in a
   one-shot (the irreversible gate). A retry re-sends the **same** draft id (de-dupe), not a new
   message.

### drive — blob_namespace (`cp /local/report.pdf /drive/Reports/`)

1. **DESCRIBE** `qfs describe /drive/my/Reports -json` → `"archetype": "blob_namespace"`,
   `verbs.cp: true` (+ `ls/mv/rm`, and universal `upsert`).
2. **Statement** — a cross-mount copy (local → drive). The `cp` shell verb lowers to a retry-safe
   `UPSERT` blob write (RFD §6); the corpus pins that closed-core form:
   ```text
   cp /local/report.pdf /drive/my/Reports/
   -- lowers to: UPSERT INTO /drive/my/Reports/report.pdf VALUES (<bytes>)
   ```
3. **PREVIEW** — the plan is **copy → verify → delete**; for `cp` the delete leg is absent
   (a copy keeps the source). Affected: 1 object uploaded.
4. **COMMIT** — `--commit` streams the upload then verifies it. Reversible (the source survives).

### github — object_graph_workflow (`CALL github.merge(method => 'squash')`)

1. **DESCRIBE** `qfs describe /github/acme/web/pulls -json` → `"archetype":
   "object_graph_workflow"`, a `merge` procedure with `irreversible: true`.
2. **Statement** — squash-merge a PR (an object-graph state transition):
   ```text
   /github/acme/web/pulls/42 |> CALL github.merge(method => 'squash')
   ```
3. **PREVIEW** — one **irreversible** `CALL` node. Affected: 1 PR.
4. **COMMIT** — needs `--commit --commit-irreversible`. A merge cannot be undone — treat it as a gate.

### slack — append_log (`INSERT INTO /slack/<ws>/<channel>/messages VALUES …`)

1. **DESCRIBE** `qfs describe /slack/acme/general/messages -json` → `"archetype": "append_log"`,
   `verbs.insert: true`. (A channel segment is a normal **path segment**: a bare `general` or a
   symbolic `#general` both address the channel — the same path rules as any other driver, no
   special case. Use the bare form in a write target.)
2. **Statement** — append a message to a channel:
   ```text
   INSERT INTO /slack/acme/general/messages VALUES ('Deploy finished')
   ```
3. **PREVIEW** — one reversible `INSERT` (append). Affected: 1 message.
4. **COMMIT** — `--commit` posts it. An append log only supports `SELECT(tail)` + `INSERT(append)`
   — DESCRIBE will not show `UPDATE`/`REMOVE`, so don't reach for them.

### sql — relational_table, pushdown (`/sql/pg/orders |> WHERE total > 100 |> SELECT id,total`)

1. **DESCRIBE** `qfs describe /sql/pg/orders -json` → `"archetype": "relational_table"`,
   `pushdown.where: true`, `pushdown.project: true`. This is a **pure read** — no COMMIT at all.
2. **Statement** — a filtered, projected read (the predicate + projection push **down** to Postgres):
   ```text
   /sql/pg/orders |> WHERE total > 100 |> SELECT id, total
   ```
3. **PREVIEW** — a read has no effect plan; PREVIEW is the query itself. The pushdown summary tells
   you `WHERE total > 100` runs in the database, not locally.
4. **COMMIT** — none. Reads are pure (`-> rows`), so there is nothing to apply.

### git — blob_namespace + relational (`INSERT INTO /git/repo/commits` / read `/git/repo@<ref>/path`)

1. **DESCRIBE** `qfs describe /git/myrepo/commits -json` → git is multi-archetype; the commit log
   is relational/append, the worktree is a versioned blob namespace (`version_support: versioned`).
2. **Statement** — record a commit, and read a file as-of a ref (the `@<ref>` temporal coordinate):
   ```text
   INSERT INTO /git/myrepo/commits VALUES ('add feature', 'main')
   /git/myrepo@v1.0/README.md
   ```
3. **PREVIEW** — the commit `INSERT` is one reversible node (a new commit; history is append-only).
   The `@v1.0` read is pure.
4. **COMMIT** — `--commit` writes the commit. Use `@<ref>` for optimistic concurrency on a
   read-then-write.

### server — `/server/...` binding (`CREATE TRIGGER … DO <plan>` → `INSERT INTO /server/triggers`)

1. **DESCRIBE** `qfs describe /server/triggers -json` → the `/server` self-config driver: a
   relational binding table. The server is configured **by writing rows**, like any other node.
2. **Statement** — a `CREATE TRIGGER` desugars to exactly one `/server/triggers` config write:
   ```text
   CREATE TRIGGER notify ON inbox DO INSERT INTO /log VALUES ('mail arrived')
   ```
3. **PREVIEW** — one reversible `/server` config-write node. This is the **PREVIEW-as-CI-test**
   pattern (RFD §8): you assert the plan a fired handler would commit, with no socket and no backend.
4. **COMMIT** — `--commit` installs the trigger. On the server, pair it with a `POLICY` (t35) to
   scope the handler to least privilege; never grant the handler more drivers/verbs than its plan needs.

---

## Quick reference

- **PREVIEW is the default.** `qfs run '<stmt>'` previews; add `--commit` to apply.
- **Irreversible = gate.** `REMOVE`, `CALL mail.send`, `CALL github.merge`. One-shot needs
  `--commit-irreversible`.
- **Unsupported verb = structured error.** The error lists the `supported:` set — pick from it.
- **Idempotent default = `UPSERT`.** Retry-safe. Use `@version`/ETag for read-then-write races.
- **`cp` = copy → verify → delete.** Never lossy. The audit ledger is the recovery source of truth.
- **Secrets never appear.** Not in DESCRIBE, not in logs. Request a `POLICY` for least privilege.
