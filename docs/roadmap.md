# Where qfs is going

::: warning This is a vision + development plan, not a feature list
Everything here is the **direction we are building toward**. The
[generated reference](/language) always describes the binary as it actually is today. This page is
how we want qfs to feel once the plan below is built — and the architecture that makes it possible.
Read it as direction, not documentation.
:::

Every capability on this page carries a status tag so you can tell what is real from what is planned:

| Tag | Meaning |
| --- | --- |
| ✅ **Shipped** | In the binary today, live-verified |
| 🔌 **Built, not wired** | The library exists; not yet on the running path |
| 🧭 **Proposed** | A design target — not built |

## Two constraints that shape every decision

Before any feature, two rules decide whether it is allowed to exist.

1. **Security by design, first.** Security is the first question for every feature, never the last. The
   floor is the model qfs already enforces — **describe is pure, preview touches nothing, commit is
   explicit, irreversible needs an extra acknowledgement** — and every new surface (a dashboard button,
   a shared server, a tunnel between two laptops, an AI agent's MCP call) inherits it.
2. **One engine, three faces.** There will be a **CLI**, a **web dashboard**, and an **MCP endpoint**.
   None can do something the others cannot, because all three compose the *same* qfs statement, run it
   through the *same* engine, and show the *same* preview before the *same* commit. A click in the
   dashboard, a line in the terminal, and a tool call from Claude are the same operation rendered three
   ways. That sameness is the product.

## The confirmed architecture (decision ledger)

The plan rests on these decisions. Later sections expand each.

| # | Area | Decision |
| --- | --- | --- |
| A | Priority | **Architecture first** — a robust, flexible foundation that can absorb the whole vision, not a feature race |
| B | Identity | Every deployment (qfs Cloud / self-hosted / local) holds its own `users` + `accounts` in SQLite. Service credentials are renamed **`connections`** to free `accounts` for human identity |
| C | Authorization | A qfs server **is a remote MCP server and its own OAuth/OIDC authorization server** — Claude connects through it |
| D | Federation | **Upstream federation** (hub model): self-hosted/local can trust qfs Cloud (or any upstream IdP) over OIDC |
| E | Persistence | **All SQLite**, credentials envelope-encrypted at rest. No migration of today's file vault — scrap & build |
| F | Scale | Distributed SQLite: **Cloudflare Workers + D1** (primary), **AWS Lambda + EFS** (alternative). A trusted reverse proxy injects the tenant→DB route; clients never name a DB |
| G | Transactions | A transaction may contain **only reversible operations**; irreversible effects are rejected at parse time. Reversible effects commit all-or-nothing via commit-point ordering |
| H | Language | A **functional core** — `LET`, lambdas, `map`/`filter`/`reduce`, user-defined functions — on top of the frozen closed-core vocabulary |
| I | ACL | **Both** an internal authorization language (extended `POLICY`) and an external directory driver (AD / Entra / Workspace), able to drive one from the other |
| J | AI safety | The agent commit boundary is **selectable** (3 preset modes; default = autonomous within policy, human approval for irreversible) |
| K | text-to-SQL | The model runs **client-side** (Claude). qfs only exposes its MCP surface; it never hosts or calls an LLM |
| L | Agent fabric | Each machine runs a resident qfs exposing `/claude/...`; machines join over a **qfs-native outbound tunnel** relayed by qfs Cloud |
| M | Scheduler | The distributed scheduler is a **qfs Cloud feature** |
| N | Tunnel gate | Using the tunnel **requires a qfs Cloud sign-in** (the relay is a qfs Cloud service) |

---

# Part 1 — The query language

## 1.1 The grammar you have today ✅

One small, SQL-like language addresses every service as a tree of **paths**. A query is a **source**
followed by **stages** joined by `|>` (a pipe). Read it top to bottom.

```qfs
FROM /mail/inbox
|> WHERE subject LIKE '%invoice%'
|> SELECT date, from, subject
|> ORDER BY date DESC
|> LIMIT 20
```

Paths are always absolute and name a node on any backend:

| Path | What it is |
| --- | --- |
| `/mail/inbox`, `/mail/drafts` | A mailbox |
| `/sql/pg/orders` | A Postgres table |
| `/github/acme/web/pulls/42` | A pull request |
| `/slack/acme/general/messages` | A Slack channel |
| `/git/myrepo@v1.2/src/main.rs` | A file as of a tag (paths can take a coordinate) |
| `/s3/bucket/key`, `/drive/Reports/q3.pdf` | An object in cloud storage |
| `/local/notes.md` | A file on your machine |

The **read/transform stages**: `WHERE`, `SELECT … AS …`, `EXTEND col = expr`, `JOIN <path> ON …`
(**even across services**), `AGGREGATE fn AS name`, `GROUP BY`, `ORDER BY … DESC`, `LIMIT`,
`DISTINCT`, and the set operations `UNION` / `EXCEPT` / `INTERSECT FROM <path>`.

The **write stages** (effects): `INSERT INTO`, `UPSERT INTO` (retry-safe), `UPDATE`, `REMOVE`, and
`CALL <service>.<action>(…)`. **Codecs** turn bytes into rows and back: `DECODE json`, `ENCODE csv`
(also `jsonl`, `yaml`, `toml`, `md`).

Federation — one query, many services — is the point:

```qfs
FROM /sql/pg/orders
|> JOIN /github/acme/web/issues ON id = issue_id
|> SELECT id, title
```

Safety is built in: `qfs run` **previews by default**, `--commit` applies, and **irreversible**
effects (sending mail, merging a PR, deleting) demand `--commit-irreversible`. Server mode adds
`POLICY` (least-privilege scopes) and `TRIGGER` (automation):

```qfs
CREATE POLICY uploads ALLOW UPSERT ON 's3/*'
CREATE TRIGGER notify ON /mail/inbox
  DO INSERT INTO /slack/acme/general/messages VALUES (NEW.subject)
```

> Credentials are stored once with `qfs account add <service> <name>` and never printed back. Under the
> plan this command becomes `qfs connection add …` (decision B); the behavior is unchanged.

## 1.2 Where the language is going 🧭

The vocabulary stays a **closed core** — a new backend still adds zero keywords — but the core gains
expressive power. **Functions become values, not keywords**, so the closed core is preserved.

**`LET` for binding** — name an intermediate result and **reference it more than once**, so you write a
subquery once instead of repeating it (and a tangled pipeline reads as one line):

```qfs
# 🧭 proposed — `products` is bound once and used twice, so LET earns its place:
# products priced above their own category's average.
LET products = FROM /sql/pg/products |> SELECT sku, category, price
LET cat_avg  = FROM products |> GROUP BY category |> AGGREGATE avg(price) AS avg_price

FROM products
|> JOIN cat_avg ON category = cat_avg.category
|> WHERE price > avg_price
|> SELECT sku, price, avg_price
```

**Higher-order functions** — a named function is just a `LET`-bound **lambda value** (no new keyword —
consistent with "functions are values"), and `map` / `filter` / `reduce` take those values as
arguments, expressing transformations that today need an external script:

```qfs
# 🧭 proposed — a function is a LET-bound lambda; map takes it as a value
LET normalize = (addr: String) => lower(trim(addr))

FROM /mail/inbox
|> EXTEND recipients = map(split(to, ','), normalize)
|> SELECT recipients, subject
```

**Cross-driver transactions, honestly bounded** (decision G) — a `TRANSACTION` block may contain
**only reversible operations**; an irreversible effect inside one is a **parse-time error**, the same
way an unsupported verb is rejected today. Reversible work commits all-or-nothing:

```qfs
# 🧭 proposed — both writes land, or neither does
TRANSACTION {
  UPSERT INTO /sql/pg/orders     VALUES (4711, 'paid')
  UPSERT INTO /local/ledger.jsonl VALUES ('{"order":4711,"state":"paid"}')
}
# Sending the receipt is irreversible, so it lives OUTSIDE the transaction,
# after the commit point, with its own explicit acknowledgement:
CALL mail.send(to => 'alice@example.com', subject => 'Receipt #4711')
```

**An access-control language** (decision I) — `POLICY` grows roles, groups, inheritance, conditional
grants, and row/column scoping, and can be **driven by an external directory**:

```qfs
# 🧭 proposed — membership in a Workspace group decides the qfs policy
CREATE POLICY analysts
  ALLOW SELECT ON 'sql/*'
  WHERE member_of('/directory/google/groups/data-team')
```

New **driver families** join as ordinary paths: a first-class **`fs`** driver (your real filesystem
as a blob namespace, beyond today's `/local`), **`/directory/...`** (LDAP / Active Directory / Entra /
Google Workspace), **`/claude/...`** (AI sessions — Part 3), and **`/sys/...`** (the deployment's own
users, policies, connections, and audit log — Part 3).

---

# Part 2 — What the AI writes

qfs exists for AI. An agent learns *one* grammar and *one* procedure instead of N vendor SDKs.

## 2.1 The loop the agent follows ✅ (procedure) / 🧭 (over MCP)

> **DESCRIBE `<path>` → write a qfs statement → PREVIEW → COMMIT**

- **DESCRIBE** returns a node's archetype, columns, supported verbs, `CALL` procedures, and pushdown —
  the contract the agent reads first. It is **pure**: no credentials, no I/O, no network.
- The agent **writes** a pipe-SQL statement against the node.
- **PREVIEW** shows the effect-plan without touching the world.
- **COMMIT** applies it — gated by policy and the safety mode below.

The four steps are identical across every backend, which is exactly what makes one agent able to drive
every service.

## 2.2 qfs server *is* the agent's MCP server 🧭 (decision C, K)

When you run qfs in server mode, it is a **remote MCP server** that Claude (Claude.ai, Claude Code, or
the API's MCP connector) connects to — and qfs is **its own OAuth/OIDC authorization server**. The
agent authenticates *to qfs*, then drives every service qfs fronts through one endpoint.

The connection follows the standard remote-MCP authorization handshake — no qfs-specific auth to learn:

1. The client discovers qfs's **Protected Resource Metadata** (RFC 9728), which points at the
   authorization server.
2. It reads the **AS metadata** (RFC 8414) and **registers dynamically** (RFC 7591) — no manual client
   setup.
3. It runs the **authorization-code flow with PKCE** (OAuth 2.1); the human signs in to qfs (decision B
   identity, or an upstream IdP via decision D federation) and consents.
4. The client calls MCP tools with a **bearer token**; a **refresh token** keeps the session alive — the
   "recurring authentication" a managed identity is meant to provide.

**text-to-SQL is client-side (decision K).** qfs does **not** host or call a model. The MCP tools it
exposes *are* the surface a client LLM uses to turn natural language into qfs:

| MCP tool | Maps to | Effect |
| --- | --- | --- |
| `describe(path)` | `qfs describe` | Pure — the contract |
| `preview(statement)` | `qfs run` | Plan only, no effects |
| `commit(statement)` | `qfs run --commit` | Applies, subject to policy + safety mode |
| `connections()` | `qfs connection list` | Names + metadata only, never secrets |

Tool descriptions are prescriptive about *when* to call them, which is what keeps a capable model from
guessing.

## 2.3 The qfs an agent generates 🧭

A teammate types a sentence; Claude (client-side) turns it into the same grammar you would write, then
previews before it commits. *"Draft a win-back email to every customer who hasn't ordered in 90 days":*

```qfs
# 1. the agent DESCRIBEs /sql/pg/customers, then PREVIEWs — pure reads, no effects:
FROM /sql/pg/customers
|> WHERE last_order_at < '2026-03-27'
|> SELECT email, name, last_order_at
|> ORDER BY last_order_at
```

The preview reports *"reads only, 0 effects"* — pure, so it runs freely. Acting is a separate, gated step:

```qfs
# 2. a draft is reversible, so within policy the agent commits one per churned customer:
FROM /sql/pg/customers
|> WHERE last_order_at < '2026-03-27'
|> INSERT INTO /mail/drafts
     VALUES (to => email,
             subject => 'We miss you, ' || name,
             body => 'It has been a while, ' || name || ' — here is 10% off your next order.')
```

*Sending* those drafts is irreversible, so in the default mode (§2.4) a `CALL mail.send` over the same
set is the step that waits for a human's approval — the reversible drafting above does not.

## 2.4 The commit boundary is selectable 🧭 (decision J)

How much an agent may do on its own is an operator setting, not a fixed rule. Three presets:

| Mode | Reversible effects | Irreversible effects (send mail, merge PR, delete) |
| --- | --- | --- |
| **Autonomous-in-policy** *(default)* | Auto-commit within `POLICY` | **Human approval** (dashboard / push notification) |
| **Approve-everything** | Human approval | Human approval |
| **Policy-only** | Within `POLICY` | Within `POLICY` (for CI / unattended automation) |

In the default mode the agent's `preview` runs free, reversible writes auto-commit inside policy, and an
irreversible `commit` raises a one-time approval card in the dashboard before it fires.

---

# Part 3 — Working as a team on qfs Cloud

This is what the architecture is *for*: a developer joining a team and getting real work done across
everyone's services and machines, safely, through one grammar.

## 3.1 Identity & sign-in 🧭 (decisions B, D)

Each deployment keeps its own `users` and `accounts` (linked sign-in identities) in SQLite; **service
credentials are `connections`**, kept separate from human identity. On **qfs Cloud Team**, you sign in
with your qfs Cloud account; a self-hosted server can **federate upstream** to that same identity over
OIDC (decision D), so one identity reaches your laptop, the office server, and the managed cloud without
a separate login per place.

## 3.2 A day on a qfs Cloud Team 🧭

> **You join.** A teammate sends an invite by email (or a one-time signup URL). You accept, you're in
> the `acme` team's `billing` project. No GCP OAuth client to register, no tokens to mint — the team's
> **connections** to Drive, Gmail, GitHub, and Slack are already wired at the project level (the managed
> tier's whole point), and `POLICY` decides what you may touch.
>
> **You look around** — `describe` needs no credential, so you explore the team's world first:
>
> ```qfs
> FROM /sys/connections            # what the project can reach (names + metadata only, never secrets)
> |> SELECT service, name, scopes
> ```
>
> **You do real work** across services that were never built to talk to each other:
>
> ```qfs
> FROM /github/acme/web/pulls
> |> WHERE state = 'open' AND author = 'alice'
> |> JOIN /slack/acme/eng/messages ON pull_number = thread_ref
> |> SELECT pull_number, title, last_reply_at
> |> ORDER BY last_reply_at DESC
> ```
>
> **You publish a result** to a shared place — previewed, then committed, visibly:
>
> ```qfs
> FROM /github/acme/web/pulls
> |> WHERE state = 'open'
> |> ENCODE csv
> |> UPSERT INTO /drive/acme/Reports/open-prs.csv
> ```

Everything you just did, a teammate can reproduce verbatim from the CLI, watch happen in the dashboard,
or hand to Claude over MCP — same statement, same preview, same commit.

## 3.3 Interacting with teammates and the server 🧭

- **Shared projects & connections.** A project's `connections` are team-wide, so members act *as the
  team* against Drive/GitHub/Slack without personal credential setup — what they may do is bounded by
  `POLICY`, not by who holds a token.
- **Invites & membership.** Invite by email when the server is configured for it, or hand out a one-time
  signup URL; an invited person joins by signing up to the host or through qfs Cloud's OIDC (decision D).
- **The audit log is a path.** Who did what is itself queryable, so review is just another query:

  ```qfs
  FROM /sys/audit
  |> WHERE actor = 'bob@acme.co' AND verb IN ('REMOVE','CALL') AND ts > '2026-06-25'
  |> SELECT ts, actor, verb, path, committed
  |> ORDER BY ts DESC
  ```
- **The agent fabric — reach a teammate's or the server's machine** (decisions L, N). Each machine runs
  a resident qfs that exposes its Claude Code sessions at `/claude/...`. Machines join over a
  qfs-native **outbound** tunnel relayed by qfs Cloud — so the office desktop and a home laptop never
  open a port — and **using the tunnel requires a qfs Cloud sign-in** (the relay is a qfs Cloud service).
  From your laptop you inspect and steer work elsewhere:

  ```qfs
  # what is the build server's agent doing right now?
  FROM /claude/acme-ci/sessions
  |> WHERE status = 'running'
  |> SELECT task, progress, last_message
  ```
  ```qfs
  # send it a further instruction
  INSERT INTO /claude/acme-ci/sessions/current/instructions
    VALUES ('rebase onto main and re-run the suite')
  ```

  Your fleet of machines — and your teammates' — becomes one queryable surface, with every cross-machine
  call authenticated by the same identity and bounded by the same `POLICY`.
- **Scheduled jobs** (decision M) run in qfs Cloud: a distributed deployment assigns one instance to fire
  each job, so a nightly `TRIGGER` runs once, not once per instance.

## 3.4 The admin page 🧭

A team needs administration, so the dashboard has an **admin area**: manage members and invites, view
and grant `POLICY`, add/rotate `connections`, review the audit log, watch migrations, and (on the
managed tier) handle billing.

It fits the architecture cleanly because **administration is also "everything is a path."** The admin
surface is a view over the deployment's own `/sys/...` paths — `/sys/users`, `/sys/policies`,
`/sys/connections`, `/sys/audit`, `/sys/projects` — backed by the System DB. So the admin page is the
dashboard rendering the same engine over the same grammar; a super-admin can do every administrative
action as a qfs statement too, preserving the one-engine-three-faces constraint.

```qfs
# 🧭 proposed — granting access from the admin surface is itself a previewable, committable statement
INSERT INTO /sys/policies VALUES (name => 'analysts', allow => 'SELECT', on => 'sql/*')
```

::: info The admin page is planned; its implementation is open
We *will* have an admin page — modeling it as `/sys/*` paths keeps it consistent with the rest of qfs,
but **how** it is built (which views ship first, how much is generated from the `/sys` schema vs.
hand-built, the local-super-admin vs. project-admin split) is a deliberate design question still to be
settled, not a decision baked in here.
:::

---

# Part 4 — The architecture underneath

## 4.1 Identity is not authorization 🧭 (decisions B, C, D)

Two concerns the current draft conflated, now kept separate:

- **Identity (authentication)** — *who you are*. A `users` + `accounts` table in SQLite, present at every
  tier. Local sign-up, or an upstream IdP via federation.
- **Authorization (OAuth/OIDC)** — *what may connect*. A qfs server is its own authorization server so it
  can also be a remote MCP server (Part 2). The two compose: the human authenticates against identity;
  the agent's client authorizes against the OAuth surface.

## 4.2 Persistence: all SQLite, stateless at scale 🧭 (decisions E, F)

| Database | Scope | Holds |
| --- | --- | --- |
| **System DB** | Per host | Projects, cross-project config, `/sys/*` (users, policies, connections, audit) |
| **Project DB** | Per project | That project's `connections`, config, and state |

Credentials are **envelope-encrypted** at rest: a passphrase or OS keychain unwraps a data-key that
encrypts the secret columns inside the DB — one persistence path from a single-user laptop to the
managed cloud. Because the project is still experimental, there is **no migration** of today's file
vault; the ideal is built fresh (decision E).

Scale keeps SQLite semantics everywhere by using **distributed SQLite**: **Cloudflare Workers + D1**
(primary) or **AWS Lambda + EFS** (alternative). The binary stays **stateless** — a request arrives at
any instance, and a **trusted reverse proxy injects the tenant→DB route**; a client can never name a
database, which is the tenant-isolation boundary. Add capacity by adding instances. When the binary is
updated and relaunched, **embedded migrations** apply System-DB changes safely in the same motion.

## 4.3 The distributed scheduler 🧭 (decision M)

A qfs Cloud feature. Stateless instances elect a leader through an atomic lease in the System DB; the
leaseholder fires each scheduled task, so a job assigned across a distributed deployment runs exactly
once.

---

# Part 5 — Expanded possibilities

Beyond the confirmed plan, capabilities the foundation makes cheap — candidates, not commitments:

- **Change subscriptions / CDC.** `TRIGGER` today reacts to a poll; a webhook-or-stream-backed
  subscription would let `/mail`, `/github`, `/slack` push changes, turning automation real-time.
- **A driver SDK + registry.** The closed-core/open-registry split already invites community drivers; a
  published SDK and a signed registry would let teams add private backends as paths.
- **Short-lived credential brokering.** Instead of long-lived `connections`, mint per-plan, per-scope
  tokens that expire at commit — least privilege taken to its limit.
- **Approval workflows as data.** The selectable safety mode (decision J) generalizes to multi-party
  approval: an irreversible plan becomes a row in `/sys/approvals` a second human signs off.
- **Observability as paths.** Expose metrics/traces at `/sys/metrics` so operating a fleet uses the same
  grammar as everything else.
- **An agent mesh.** With `/claude/...` across machines, a coordinator agent on one host can fan work to
  agents on others and collect results — multi-agent orchestration expressed in qfs.

---

# Part 6 — Phased delivery plan

Dependency-ordered, architecture first (decision A). Each phase leaves the tree green and the docs honest
about exactly what now works.

| Phase | Theme | Delivers | Unlocks |
| --- | --- | --- | --- |
| **M0** | Persistence foundation | System/Project SQLite, envelope encryption, embedded migrations; `accounts`→`connections` rename | The single world the dashboard and CLI agree on |
| **M1** | Identity store | `users`/`accounts` tables, local sign-up, session handling | A real "who" at every tier |
| **M2** | Server-as-MCP + OAuth AS | MCP `describe`/`preview`/`commit` tools; OAuth 2.1 AS (PRM, AS-metadata, DCR, PKCE); Claude connects | Part 2 — the agent's single endpoint |
| **M3** | Dashboard at parity | Embedded SPA over the same engine; preview→commit cards; first `/sys/*` admin views | The second face; admin page begins |
| **M4** | Cloud tier | `connections` for Drive/GitHub/Gmail with consent flows; sign-in mandatory for cloud drivers | Local + Cloud usage |
| **M5** | Self-hosted multi-user | Invites (email / one-time URL), upstream OIDC federation, extended `POLICY`/ACL, selectable AI safety modes | Teams on their own server |
| **M6** | Language core | `LET`, lambdas, `map`/`filter`/`reduce`, `DEF`; reversible-only `TRANSACTION` + commit-point | Part 1.2 expressiveness |
| **M7** | Agent fabric *(qfs Cloud)* | qfs-native outbound tunnels (require qfs Cloud sign-in), `/claude/...` driver, the cross-machine scenario | Part 3.3 fleet |
| **M8** | Distributed scheduler *(qfs Cloud)* | System-DB lease leader election, single-fire scheduled jobs | Part 4.3 |
| **M9** | Managed Team | qfs Cloud OAuth brokering, team connections, billing (free individual / paid team) | The top tier |
| **M+** | Expansions | CDC, driver SDK, credential brokering, approvals, observability, agent mesh | Part 5 |

## How it holds together

Read top to bottom, the plan is one idea repeated: **add reach without adding special cases.**

- More **places to run** (local → cloud → self-hosted → managed) — same grammar.
- More **people** (invites, OIDC, ACLs, audit) — same preview-then-commit safety.
- More **machines** (tunnels, distributed scheduling) — same one identity.
- More **power** (`LET`, higher-order functions, transactions) — same closed core.
- More **faces** (CLI, dashboard, admin page, MCP) — same one engine.

Every tier, every machine, every collaborator, and every agent meets qfs as the same small, safe
grammar. That sameness is the product — and protecting it is what this plan is for.
