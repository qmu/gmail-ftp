---
aside: false
---

# Where qfs is going

[[toc]]

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

   > **Implementation status (t52 — the dashboard commits).** The CLI ✅ and the MCP endpoint ✅ are
   > live; the **web dashboard** now has a shipped ✅ embedded SPA — a static page in the
   > binary (`GET /`, `GET /assets/*`) that the in-house listener serves over loopback, plus a thin JSON
   > bridge (`POST /api/describe`, `POST /api/run`) that drives the **same** injected engine the MCP face
   > uses. The **preview→commit approval cards** ✅ (t52) have landed: a preview renders the plan's
   > effects as a card, and `POST /api/commit` applies an approved plan through the **same** default-deny
   > policy gate + irreversible-effect guard the CLI/MCP use — a reversible in-policy plan auto-commits,
   > an out-of-policy plan is refused with the decision, and an **irreversible** plan (REMOVE / CALL) is
   > never auto-applied: it raises a distinct one-time confirm that posts the explicit ack (the same
   > acknowledgement `--commit-irreversible` drives). The *selectable* commit modes (§2.4) ✅ (t59) have
   > landed: the three presets are an operator setting stored as data (`/sys/settings` `safety_mode`,
   > resolved with an env fallback to the safe default), and they govern the **real** commit path —
   > `qfs_mcp::commit_plan` (MCP + dashboard) and the CLI one-shot — so e.g. *Approve-everything* refuses a
   > reversible write *Autonomous-in-policy* would auto-apply, while the policy gate + irreversible-ack
   > floor hold in every mode (an out-of-policy plan is denied regardless of mode).
   > The first `/sys/*` admin views ✅ (t53) have landed: the deployment's own state (`/sys/users`,
   > `/sys/projects`, `/sys/audit`, `/sys/connections`, `/sys/policies`) is now an ordinary set of qfs
   > paths backed by the System DB, readable from every face and writable via a gated
   > `INSERT INTO /sys/policies` (default-deny, audited). The shell is loopback-only and not yet
   > session-gated; the super-admin vs. project-admin split is still open (§3.4).

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
| H | Language | A **functional core** — `let`, lambdas, `map`/`filter`/`reduce`, user-defined functions — on top of the frozen closed-core vocabulary |
| I | ACL | **Both** an internal authorization language (extended `policy`) and an external directory driver (AD / Entra / Workspace), able to drive one from the other |
| J | AI safety | The agent commit boundary is **selectable** (3 preset modes; default = autonomous within policy, human approval for irreversible) |
| K | text-to-SQL | The model runs **client-side** (Claude). qfs only exposes its MCP surface; it never hosts or calls an LLM |
| L | Agent fabric | Each machine runs a resident qfs exposing its sessions under the host realm at `/hosts/<host>/claude/...`; machines join over a **qfs-native outbound tunnel** relayed by qfs Cloud |
| M | Scheduler | **Scheduling is externalized — qfs is not a scheduler.** Individual use leans on **OS `cron`**; the managed tier on **managed cron (Cloudflare Cron Triggers)**. qfs supplies the *invokable unit* (a `qfs run` one-shot, a saved named plan, or an `ENDPOINT`); the *when* and the exactly-once guarantee belong to the external scheduler |
| N | Tunnel gate | Using the tunnel **requires a qfs Cloud sign-in** (the relay is a qfs Cloud service) |
| O | Operators | **`=` always binds, `==` always compares** (unlike SQL). Because `let` and lambdas make binding a first-class, frequent act, `=` is reserved for assignment/binding everywhere (`let`, `extend`, `set`, `update … set`) and equivalence is the explicit `==` |
| P | Addressing | A path names three axes — **scope** (whose), **service** (what), **coordinate** (when). Root is a **closed set of plural realms** (`/members`, `/projects`, `/hosts`, `/directories`) plus two singletons (`/me`, `/sys`); within a scope, connections/accounts are **plain `/`-segments** (no new punctuation). The path is the authorization subject; realms are closed/governed while drivers stay an open registry. See §1.3 |
| Q | Write form | A write reads as **dataflow**: when it has a source it is a **pipeline stage** — `<source> \|> … \|> insert/upsert/update/remove <target>`; a **source-less literal** write leads with the verb — `insert into <target> values (…)`. Both forms are legal; nothing else is. (Today's parser only accepts the verb-leading form — aligning it to the pipeline-stage form is grammar work, tracked with the M6 language tickets.) |
| R | Source form | A `/path` is a first-class **`Resource`** value — a lazy, describable handle to a node. The source position needs no `from`: a leading `/path` **is** the source, and the *same* literal serves `join`/set-op operands, `policy` targets, and `member_of(…)` — one spelling for "a node" instead of four. A `Resource` is pure to hold and `describe`; only a pipeline forces its rows. A leading `/` is unambiguous by **position** — `/` where an expression *begins* (a stage/operand start) is always a path, `/` *between* two operands is division — so dropping `from` costs no clarity. (Today's parser requires `from` and quotes path patterns; unifying them is M6 grammar work — see §1.2.) |
| S | Keyword case | Keywords are **lowercase** (`where`, `select`, `let`, `insert into`, `join`, `policy`, …). Paths, column names, and bindings are the data that carries the visual weight; keywords stay quiet. (**Landed in M6, ticket t74:** the lexer recognizes keywords **case-insensitively** and renders them lowercase as the canonical form — so an older uppercase query still parses — and the generated reference + the cookbook now show lowercase.) |
| T | Type system | A real, **static** type system, checked at **plan time** — before any I/O, so a type error surfaces in `preview`, consistent with the purity floor. Scalar primitives are lowercase, Rust-style: **`bool`** (`true`/`false`), fixed-width ints **`i32`/`i64`/`u32`/`u64`**, floats **`f32`/`f64`**, and **`string`** (`'…'`) — alongside the **`Resource`** value (decision R) and lambda/function types (CamelCase for named types, lowercase for scalar primitives). Column types come from `describe`, so a pipeline type-checks before it touches the world. Ordinary **infix arithmetic** (`+ - * /`) is supported and type-checked; a leading `/` stays unambiguous by **position** (decision R), not by banning division. |
| U | Credential vault | Secrets live in a **vault** with a **three-level key hierarchy** (extends E): a deployment **root KEK** (OS keychain / cloud KMS, never in the DB) wraps a per-project/connection **data key**, which encrypts only the secret columns. A team **shares a connection, never the secret** — a project connection is added once and used *as the team*, with actor-based `policy` deciding who may run plans through it; the raw secret is never re-entered, copied, or shown. A secret **never appears in a qfs statement** (it would be previewable/audited) — it enters only through a dedicated credential input (CLI prompt / admin form); the language sees **metadata only**. Member removal revokes use and **rotates**; an optional **per-recipient (E2E) wrap** to a member's key scopes a high-sensitivity connection cryptographically below the server (at the cost that an agent can't use it unattended). See §4.5 |
| V | Observability | Telemetry is **externalized — qfs emits, it does not store** (the monitoring parallel to decision M). Three signals — **audit** (actor/connection/verb/path/committed), **metrics** (counters/histograms), **traces** (per-plan `describe`→pushdown→`commit`) — written to one of **three prepared output sinks: `file` (default), `stdout`, or `OTel` (recommended)**. Everything downstream (Prometheus, Grafana, Datadog, a SIEM, qfs Cloud's dashboards) consumes one of those — almost always OTel; qfs ships the standard output and integrates with no vendor directly. **One surface, two consumers:** the managed tier consumes it automatically; a self-hosted deployment points its own stack at the same output. Telemetry is **metadata only** — never secrets or row data — and audit reads are `policy`-gated. qfs **exports** the audit stream rather than retaining it: events are **hash-chained** as emitted and the chain head is **sealed to an independent write-once witness** (WORM / transparency log), so the *consumer's* store is tamper-evident and a compromised server can't rewrite history undetectably. **Where the log lives and how long it is kept are the consumer's concern, not qfs's** — though a local install may point the `file` sink at a rotating path when it has no external monitor. See §4.6 |

---

## Part 1 — The query language

### 1.1 The grammar you have today ✅

One small, SQL-like language addresses every service as a tree of **paths**. A query is a **source**
followed by **stages** joined by `|>` (a pipe). Read it top to bottom.

```qfs
/mail/inbox
|> where subject LIKE '%invoice%'
|> select date, from, subject
|> order by date DESC
|> limit 20
```

Paths are always absolute and name a node on any backend:

| Path | What it is |
| --- | --- |
| `/mail/inbox`, `/mail/drafts` | A mailbox |
| `/sql/pg/orders` | The `orders` table in connection `pg` (the label holds host/user/db) |
| `/github/acme/web/pulls/42` | A pull request |
| `/slack/acme/general/messages` | A Slack channel |
| `/git/myrepo@v1.2/src/main.rs` | A file as of a tag (paths can take a coordinate) |
| `/s3/bucket/key`, `/drive/Reports/q3.pdf` | An object in cloud storage |
| `/local/notes.md` | A file on your machine |

The **read/transform stages**: `WHERE`, `SELECT … AS …`, `EXTEND col = expr`, `JOIN <path> ON …`
(**even across services**), `AGGREGATE fn AS name`, `GROUP BY`, `ORDER BY … DESC`, `LIMIT`,
`DISTINCT`, and the set operations `UNION` / `EXCEPT` / `INTERSECT <path>`.

The **write effects**: `INSERT INTO`, `UPSERT INTO` (retry-safe), `UPDATE`, `REMOVE`, and
`CALL <service>.<action>(…)`. **Codecs** turn bytes into rows and back: `DECODE json`, `ENCODE csv`
(also `jsonl`, `yaml`, `toml`, `md`).

> Today these write effects parse only in **verb-leading** form (`INSERT INTO <target> …`); writing
> them as a trailing **pipeline stage** (target grammar: `<source> |> insert into <target>`) is grammar
> work tracked in M6 (decision Q). The read/transform stages above are full `|>` pipeline stages today.

Federation — one query, many services — is the point:

```qfs
/sql/pg/orders
|> join /github/acme/web/issues on id == issue_id
|> select id, title
```

Safety is built in: `qfs run` **previews by default**, `--commit` applies, and **irreversible**
effects (sending mail, merging a PR, deleting) demand `--commit-irreversible`. Server mode adds
`POLICY` (least-privilege scopes) and `TRIGGER` (automation):

```qfs
create policy uploads ALLOW UPSERT on '/s3/*'
create trigger notify on /mail/inbox
  do insert into /slack/acme/general/messages values (NEW.subject)
```

> Credentials are stored once with `qfs connection add <service> <name>` and never printed back
> (decision B renamed this command from `qfs account add`); the behavior is unchanged.

### 1.2 Where the language is going 🧭

The vocabulary stays a **closed core** — a new backend still adds zero keywords — but the core gains
expressive power. **Functions become values, not keywords**, so the closed core is preserved.

**A `/path` is a value — no `from`** (decision R). Today the language spells "a node" four different
ways: `from /path` in source position, a bare `/path` after `join`, a quoted `'/path'` in a `policy`
target, and a quoted string inside `member_of(…)`. They collapse into **one** primitive: a `/path` is a
first-class **`Resource`** value — a lazy, describable handle to a node — so the source position simply
*is* the path, and the same literal works everywhere a node is named. This is unambiguous in qfs because
a leading `/` is decided by **position** — `/` where an expression *begins* (a stage or operand start) is
a path, `/` *between* two operands is division — so ordinary infix arithmetic (`+ - * /`) and bare paths
coexist with no clash. The newline-as-soft-boundary rule still holds for **pipelines**: **`|>` is the
only continuation token** — a line that does not start with `|>` opens a new statement, whether it begins
with a keyword, a `/path`, or a bound name. (A multi-clause **`create` statement** — `create policy`,
`create trigger` — is the one exception: it continues across its own clause keywords (`allow`, `on`,
`where`, `do`) until the statement closes, exactly as it reads.)

```qfs
# 🧭 proposed — the path is the source; describe/hold it without touching the world, pipe it to force rows
let paid = /sql/pg/orders |> where status == 'paid'

/mail/inbox
|> where subject like '%invoice%'
|> select date, subject

# the same Resource literal everywhere a node is named — unquoted
create policy analysts allow select on /sql/*
  where member_of(/directories/google/groups/data-team)
```

**Keywords are lowercase** (decision S). With the `/path` Resource and the column names carrying the
meaning, the keywords step back into quiet lowercase (`where`, `select`, `let`, `insert into`, `join`,
`policy`). It is a readability choice, not a new capability — paths and identifiers stay case-sensitive
data; only the closed keyword set is lowered.

**Real primitive types, checked before any I/O** (decision T). The lambda annotation you already see
(`(addr: string) => …`) is the first sight of a **static type system**. Values carry real types —
**`bool`** (`true`/`false`), fixed-width integers (**`i32`**, **`i64`**, **`u32`**, **`u64`**), floats
(**`f32`**, **`f64`**), and **`string`** (`'…'`) — beside the **`Resource`** handle (decision R) and
function types. The point is *early* checking: a column's type comes from `describe` (which is pure), so
a pipeline is **type-checked at plan time** and a mismatch (`where total == 'paid'` against an `i64`
column) is a `preview`-time error, never a surprise at commit. Scalar primitives are lowercase; named
types (`Resource`, function types) are CamelCase — the Rust split. Ordinary **infix arithmetic**
(`+ - * /`) is supported and type-checked; the leading-`/` path stays unambiguous by **position** (above),
not by banning division.

```qfs
# 🧭 proposed — typed literals; the comparison is checked against the column's type before any I/O
let min_total = 100i64
let active    = true

/sql/pg/orders
|> where total >= min_total and is_paid == active
|> select id, total, currency
```

> These land across the **M6** language tickets — *not* one ticket. The work splits by decision:
> **t70** flips `=`→`==` (decision O, landed), a **`Resource`/drop-`from`** ticket removes `from` and
> unquotes path literals (decision R), **t74** lowers the keyword set to **lowercase** (decision S,
> **landed** — keywords are recognized case-insensitively and render lowercase), and a
> **static-type-system** ticket lands typed literals + the plan-time checker (decision T, still pending,
> so the type annotations below are parsed-and-retained but not yet checked). §1.1 (the grammar you have
> today ✅) and the [query cookbook](/query-cookbook) stay honest about what parses *now*.

**`=` binds, `==` compares** (decision O) — **unlike SQL, a single `=` is never equivalence.** Once
`let` and lambdas make binding a first-class, everyday act, overloading `=` for both "give this name a
value" and "is this value equal" reads as a trap. So qfs splits them: `=` **always** assigns or binds
(`let x = …`, `extend col = …`, `set col = …`, `update … set …`), and equivalence is the explicit
**`==`**. The other comparisons are unchanged (`<> < > <= >= like ~ in between any`).

> The `=`/`==` split (decision O) **has now landed** (ticket t70): the binary parses a single `=` as a
> bind/assignment only, and `==` as equivalence, so §1.1 (the grammar you have today ✅) and the
> `grammar=core` recipes in the [query cookbook](/query-cookbook) use `where x == …`. **Lowercase keywords
> (decision S) have also landed** (ticket t74): keywords are recognized case-insensitively and render
> lowercase, so the reference and the cookbook now show the lowercase form. **The 🧭 examples below
> already show the full target grammar.**

```qfs
# 🧭 proposed — `=` binds the name, `==` tests equivalence; the two never collide.
let paid = /sql/pg/orders |> where status == 'paid'

paid
|> extend tier = 'gold'
|> where region == 'EU'
|> select id, total, tier
```

**`let` for binding** — name an intermediate result and **reference it more than once**, so you write a
subquery once instead of repeating it (and a tangled pipeline reads as one line):

```qfs
# 🧭 proposed — `products` is bound once and used twice, so let earns its place:
# products priced above their own category's average.
let products = /sql/pg/products |> select sku, category, price
let cat_avg  = products |> group by category |> aggregate avg(price) as avg_price

products
|> join cat_avg on category == cat_avg.category
|> where price > avg_price
|> select sku, price, avg_price
```

There is **one** `let` form and **no statement terminator.** Bindings are written as plain leading
statements (`let <name> = …`), one per line, in scope for everything after them — there is no `let …
in …` wrapper and no `;`. This is unambiguous because **`|>` is the only continuation token** — a line
that does not start with `|>` opens a new statement (whether it begins with a keyword, a `/path`, or a
bound name; a multi-clause `create` statement is the lone exception, §1.2 above). An expression always
completes within its statement or stage — it never trails an operator across a newline — so a newline is
a soft statement boundary (a missing `|>` is a crisp error, not a silent split).

**Higher-order functions** — a named function is just a `let`-bound **lambda value** (no new keyword —
consistent with "functions are values"), and `map` / `filter` / `reduce` take those values as
arguments, expressing transformations that today need an external script:

```qfs
# 🧭 proposed — a function is a let-bound lambda; map takes it as a value
let normalize = (addr: string) => lower(trim(addr))

/mail/inbox
|> extend recipients = map(split(to, ','), normalize)
|> select recipients, subject
```

**Cross-driver transactions, honestly bounded** (decision G) — a `transaction` block may contain
**only reversible operations**; an irreversible effect inside one is a **parse-time error**, the same
way an unsupported verb is rejected today. Reversible work commits all-or-nothing:

```qfs
# 🧭 proposed — both writes land, or neither does
transaction {
  upsert into /sql/pg/orders      values (4711, 'paid')
  upsert into /local/ledger.jsonl values ('{"order":4711,"state":"paid"}')
}
# Sending the receipt is irreversible, so it lives OUTSIDE the transaction,
# after the commit point, with its own explicit acknowledgement:
call mail.send(to => 'alice@example.com', subject => 'Receipt #4711')
```

**An access-control language** (decision I) — `policy` grows roles, groups, inheritance, conditional
grants, and row/column scoping, and can be **driven by an external directory**:

```qfs
# 🧭 proposed — membership in a Workspace group decides the qfs policy
create policy analysts
  allow select on /sql/*
  where member_of(/directories/google/groups/data-team)
```

New **drivers** join as ordinary paths: a first-class **`fs`** driver (your real filesystem as a blob
namespace, beyond today's `/local`) — **landed (t68)** as an opt-in, **deny-all-by-default** mount:
it addresses files under operator-configured **named roots** (`/fs/<root>/...`, one `QFS_FS_<NAME>`
per root), with `..`/absolute/symlink escapes refused at both scan and apply time and `REMOVE` kept
irreversible; with no root configured nothing resolves — and the AI-session driver mounted under a host
(**`/hosts/<host>/claude/...`** — Part 3). Two of the surfaces below are **closed/governed**, not open
driver mounts (§1.3, decision P): **`/directories/<provider>/...`** (LDAP / Active Directory / Entra /
Google Workspace, for ACL) — a realm — and **`/sys/...`** (the deployment's own users, policies,
connections, and audit log — Part 3) — the `/sys` singleton.

### 1.3 The path expression — what is at root, and how it expands 🧭 (decision P)

Every node on every backend is named by one **absolute path**, and a path carries three independent
axes. Like an OS filesystem, the format is just `/`-separated segments — **no extra punctuation.**

| Axis | Question | Example fragment | Status |
| --- | --- | --- | --- |
| **Service** | *what* | `/mail/inbox`, `/sql/pg/orders` | ✅ today |
| **Coordinate** | *when / which version* | `@v2.1` | ✅ today |
| **Scope** | *whose / where* | `/members/colleague1` | 🧭 planned |

**Today (✅)** a path is flat — a driver mount, then segments, with an optional `@version` bound to a
segment and `* ?` globs (a path is also a query):

```qfs
/git/app@v2.1/deploy.toml
/sql/pg/orders
/slack/acme/general/messages
```

**Where it is going (🧭)** — the **scope** axis becomes explicit, so a path says *whose* resource it
is, not just *what*. Root holds a small, **closed** set of **realms** (the `/usr /home /mnt` of qfs).
Each realm is a plural collection, named for what it holds:

| Realm | FS analogy | Holds |
| --- | --- | --- |
| `/members/<who>/…` | `/home/<user>` | each person's connections (a teammate's resources, policy-gated) |
| `/projects/<proj>/…` | `/srv` | team-owned connections |
| `/hosts/<host>/…` | `/net` (9P remote) | each machine's resident qfs, reached over the tunnel |
| `/directories/<provider>/…` | LDAP mount | external identity directories (groups/users) for ACL |

Two **singletons** sit beside them — neither is a collection, so neither is plural: **`/me`** (you;
the implicit default) and **`/sys`** (the deployment itself — its `/proc + /etc`: users, connections,
policies, audit). A **bare** `/gmail/inbox` is sugar for `/me/gmail/inbox`; the explicit scope is
there only when you need to name someone else, or mix.

**Multiple connections are just more segments — no new syntax.** One Google grant (a single consent
covering Gmail + Drive + Calendar) is named by provider and connection label; switching the label segment
moves *all* of its services together, because they share the grant:

```qfs
/me/google/work/gmail/inbox
/me/google/work/calendar/events
/me/google/personal/drive/Reports/q3.pdf
```

The `<provider>/<label>` pair is the **connection key** and must be unique within a scope — `work`
and `personal` are distinct grants; re-consenting the same connection replaces its tokens in place, and
revoking it removes the one node (and with it every service facet that rode the grant). The connection
label is a local alias, not the upstream email; `/sys/connections` maps label → provider identity +
scopes (metadata only, never the secret). An ambiguous bare `/gmail` with two Google connections and no
default resolves to a structured "which connection?" error, never a silent pick.

**The path is the authorization subject.** Because scope sits in the path, `POLICY` gates on it
directly — `/me/**` is yours for free, while reaching `/members/<other>/**` needs a grant — and the
audit log records the fully-qualified path. The grammar is plain `/`-segments, and it is
**unambiguous** because of two rules: realm names **and the singletons `me`/`sys`** are **reserved**
(a driver mount may not be named after a realm or singleton), and a scope takes **exactly one**
principal segment — so the scope↔service boundary is always decidable:

```
path        = scope? service
scope       = "/" realm "/" principal          # exactly one principal segment, OR …
            | "/me" | "/sys"                    # … a singleton (no principal)
realm       = "members" | "projects" | "hosts" | "directories"   # CLOSED, reserved set
principal   = name | "*"                         # a who/which, or glob over the collection
service     = "/" driver ("/" segment)*          # a driver mount and its segments, OR …
            | "/" "**"                           # … a glob-only tail (e.g. /me/**)
driver      = name                               # driver ∉ realm ∪ {me, sys} (names can't shadow)
segment     = (name | "*" | "**") coordinate?    # globs make a path set-valued; @ref binds this segment
coordinate  = "@" ref                            # binds to the preceding segment
```

The *meaning* of segments **inside** the service — which segment is a connection vs. a
resource (`/sql/pg/orders`: **connection** `pg`, table `orders`; `/google/work/gmail/inbox`: connection
`work`, mailbox `inbox`) — is declared by each driver's schema (`describe`), exactly as a filesystem
can't tell a dir from a file without asking the FS. That is the path-is-the-type model, not an
ambiguity.

The connection segment is a **label**, never the coordinates themselves. `pg` is an alias you chose at
`qfs connection add sql pg …`; the **host, port, username, password, and database name** all live
*inside* that connection's stored credential (a `postgres://…` string, envelope-encrypted at rest —
decision E), which the driver fetches by `(driver "sql", connection "pg")` and never prints back. Two
Postgres databases — a different host, user, or db name — are simply two labels (`pg`, `pg_eu`,
`analytics`); switching the label switches the whole `(host, user, db)` tuple at once, the same way
switching `work`→`personal` moves a Google grant. `/sys/connections` maps each label → provider +
endpoint metadata (host/db visible, secret never). A deeper coordinate is just more segments —
`/sql/<conn>/<schema>/<table>` names a non-default schema; the bare `/sql/<conn>/<table>` is the
connection's default schema.

**How it expands** mirrors the closed-core/open-registry split of the language: **realms are closed
and governed** — adding `/members` or `/hosts` is a deliberate design event, like adding a top-level
filesystem directory — while **drivers are open**: a new service is a registry mount (zero new realms,
zero keywords), and a new facet under an existing connection (Calendar under `google`) is just a new
segment reusing the same grant.

---

## Part 2 — What the AI writes

qfs exists for AI. An agent learns *one* grammar and *one* procedure instead of N vendor SDKs.

### 2.1 The loop the agent follows ✅ (procedure) / 🧭 (over MCP)

> **DESCRIBE `<path>` → write a qfs statement → PREVIEW → COMMIT**

- **DESCRIBE** returns a node's archetype, columns, supported verbs, `CALL` procedures, and pushdown —
  the contract the agent reads first. It is **pure**: no credentials, no I/O, no network.
- The agent **writes** a pipe-SQL statement against the node.
- **PREVIEW** shows the effect-plan without touching the world.
- **COMMIT** applies it — gated by policy and the safety mode below.

The four steps are identical across every backend, which is exactly what makes one agent able to drive
every service.

### 2.2 qfs server *is* the agent's MCP server 🧭 (decision C, K)

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

> **Implementation status (t50 — M2 complete).** All four steps are live end-to-end: a qfs server serves
> its **Protected Resource Metadata** (`/.well-known/oauth-protected-resource`), its **AS metadata**
> (`/.well-known/oauth-authorization-server`, advertising `authorization_endpoint` / `token_endpoint`
> / `registration_endpoint` + `grant_types_supported` = `authorization_code` + `refresh_token`, alongside
> `code_challenge_methods_supported=["S256"]`), and its **JWKS** (`/jwks.json`) backed by an
> envelope-encrypted ES256 signing key. A client **registers dynamically** (`POST /register`, RFC 7591),
> runs the **authorization-code flow with PKCE (S256)** — the human signs in to the qfs identity (t45)
> over a t46 session and consents at `/authorize` — and **exchanges the code for a signed ES256 access
> token** at `POST /token`. **Step 4 is now real:** the `POST /mcp` endpoint **requires** that bearer
> access token — a request with no / a malformed / a bad-signature / a wrong-`aud`-or-`iss` / an expired
> token is rejected with **`401` + `WWW-Authenticate: Bearer resource_metadata="…"`** (RFC 9728), so a
> spec-compliant client discovers the AS and authorizes without bespoke qfs knowledge; only a verified
> token reaches a tool. The **refresh-token grant** (`grant_type=refresh_token`) keeps the session alive:
> it rotates the handle single-use (mints a fresh access token + a new refresh handle, burns the old),
> and a replay of a rotated handle is an `invalid_grant`. So **Claude can now connect to qfs over
> MCP+OAuth and drive every service qfs fronts.** Caveats kept honest: the MCP `commit` gate is still the
> default-deny policy (a per-user/scope ACL is **decision I / t57**); irreversible MCP commits still
> require explicit `ack` until the selectable safety mode (**t59**); and binding off localhost (now safe
> because the endpoint authenticates) sits behind the trusted reverse proxy of decision F.

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

### 2.3 The qfs an agent generates 🧭

A teammate types a sentence; Claude (client-side) turns it into the same grammar you would write, then
previews before it commits. *"Draft a win-back email to every customer who hasn't ordered in 90 days":*

```qfs
# 1. the agent describes /sql/pg/customers, then previews — pure reads, no effects:
/sql/pg/customers
|> where last_order_at < '2026-03-27'
|> select email, name, last_order_at
|> order by last_order_at
```

The preview reports *"reads only, 0 effects"* — pure, so it runs freely. Acting is a separate, gated step:

```qfs
# 2. a draft is reversible, so within policy the agent commits one per churned customer:
/sql/pg/customers
|> where last_order_at < '2026-03-27'
|> insert into /mail/drafts
     values (to => email,
             subject => 'We miss you, ' || name,
             body => 'It has been a while, ' || name || ' — here is 10% off your next order.')
```

*Sending* those drafts is irreversible, so in the default mode (§2.4) a `CALL mail.send` over the same
set is the step that waits for a human's approval — the reversible drafting above does not.

### 2.4 The commit boundary is selectable ✅ (decision J)

How much an agent may do on its own is an operator setting, not a fixed rule — stored as data in
`/sys/settings` (`safety_mode`) and consulted live on the real commit path (MCP / dashboard / CLI).
Three presets:

| Mode | Reversible effects | Irreversible effects (send mail, merge PR, delete) |
| --- | --- | --- |
| **Autonomous-in-policy** *(default)* | Auto-commit within `POLICY` | **Human approval** (dashboard / push notification) |
| **Approve-everything** | Human approval | Human approval |
| **Policy-only** | Within `POLICY` | Within `POLICY` (for CI / unattended automation) |

In the default mode the agent's `preview` runs free, reversible writes auto-commit inside policy, and an
irreversible `commit` raises a one-time approval card in the dashboard before it fires.

This is consistent with the safety floor, not an exception to it. The floor requires that an
irreversible effect carry an **explicit acknowledgement** — it does not require that the acknowledgement
be a *per-action human click*. In Autonomous-in-policy and Approve-everything that acknowledgement *is*
the live approval card; in **Policy-only** the acknowledgement is **pre-granted, up front**: an operator
deliberately writes a `POLICY` that names the irreversible verbs+paths that unattended automation may
commit (so CI can `CALL mail.send` on `/projects/x/**` without a human in the loop). The ack still
exists and is auditable — it just moved from commit-time to a reviewed, version-controlled policy.
Anything outside that explicit grant still stops.

---

## Part 3 — Working as a team on qfs Cloud

This is what the architecture is *for*: a developer joining a team and getting real work done across
everyone's services and machines, safely, through one grammar.

### 3.1 Identity & sign-in 🧭 (decisions B, D)

Each deployment keeps its own `users` and `accounts` (linked sign-in identities) in SQLite; **service
credentials are `connections`**, kept separate from human identity. On **qfs Cloud Team**, you sign in
with your qfs Cloud account; a self-hosted server can **federate upstream** to that same identity over
OIDC (decision D), so one identity reaches your laptop, the office server, and the managed cloud without
a separate login per place.

### 3.2 A day on a qfs Cloud Team 🧭

> **You join.** A teammate sends an invite by email (or a one-time signup URL). You accept, you're in
> the `acme` team's `billing` project. No GCP OAuth client to register, no tokens to mint — the team's
> **connections** to Drive, Gmail, GitHub, and Slack are already wired at the project level (the managed
> tier's whole point), and `POLICY` decides what you may touch.
>
> **You look around** — `describe` needs no credential, so you explore the team's world first:
>
> ```qfs
> /sys/connections                 # what the project can reach (names + metadata only, never secrets)
> |> select service, name, scopes
> ```
>
> `describe` is credential-free but **not** policy-free: it returns a node's *shape* (archetype,
> columns, supported verbs) — never row data and never a secret — and a path you have no `POLICY`
> grant to reach is **invisible**, not merely unreadable. So exploration leaks structure you're already
> entitled to see, nothing more; the metadata surface is itself an authorization boundary.
>
> **You do real work** across services that were never built to talk to each other:
>
> ```qfs
> /github/acme/web/pulls
> |> where state == 'open' and author == 'alice'
> |> join /slack/acme/eng/messages on pull_number == thread_ref
> |> select pull_number, title, last_reply_at
> |> order by last_reply_at desc
> ```
>
> **You publish a result** to a shared place — previewed, then committed, visibly:
>
> ```qfs
> /github/acme/web/pulls
> |> where state == 'open'
> |> encode csv
> |> upsert into /drive/acme/Reports/open-prs.csv
> ```

Everything you just did, a teammate can reproduce verbatim from the CLI, watch happen in the dashboard,
or hand to Claude over MCP — same statement, same preview, same commit.

### 3.3 Interacting with teammates and the server 🧭

- **Shared projects & connections.** A project's `connections` are team-wide, so members act *as the
  team* against Drive/GitHub/Slack without personal credential setup — what they may do is bounded by
  `POLICY`, not by who holds a token. **Identity stays two-layered:** the *connection* is the upstream
  authority a plan runs through (the team's Drive token), but the *actor* is the signed-in human, and
  the audit row records **both** — `actor`, `connection`, `verb`, `path`, `committed`. `POLICY`
  evaluates against the **actor** (and their groups), never the connection; the connection only decides
  *which* upstream credential the allowed effect uses. So "the team acted" is always traceable to the
  one person who committed it.
- **Invites & membership.** Invite by email when the server is configured for it, or hand out a one-time
  signup URL; an invited person joins by signing up to the host or through qfs Cloud's OIDC (decision D).
- **The audit log is a path.** Who did what is itself queryable, so review is just another query:

  ```qfs
  /sys/audit
  |> where actor == 'bob@acme.co' and verb in ('REMOVE','CALL') and ts > '2026-06-25'
  |> select ts, actor, verb, path, committed
  |> order by ts desc
  ```

  The same `/sys/audit` is one of three telemetry signals (with `/sys/metrics` and traces) that qfs
  **emits** for any monitor to consume — qfs Cloud watches it for you on the managed tier, and a
  self-hosted server points its own Prometheus/Grafana/OTel stack at the identical surface (§4.6,
  decision V).
- **The agent fabric — reach a teammate's or the server's machine** (decisions L, N). Each machine runs
  a resident qfs that exposes its Claude Code sessions under the host realm at
  `/hosts/<host>/claude/...` (decision P). Machines join over a
  qfs-native **outbound** tunnel relayed by qfs Cloud — so the office desktop and a home laptop never
  open a port — and **using the tunnel requires a qfs Cloud sign-in** (the relay is a qfs Cloud service).
  From your laptop you inspect and steer work elsewhere:

  ```qfs
  # what is the build server's agent doing right now?
  /hosts/acme-ci/claude/sessions
  |> where status == 'running'
  |> select task, progress, last_message
  ```
  ```qfs
  # send it a further instruction
  insert into /hosts/acme-ci/claude/sessions/current/instructions
    values ('rebase onto main and re-run the suite')
  ```

  Your fleet of machines — and your teammates' — becomes one queryable surface, with every cross-machine
  call authenticated by the same identity and bounded by the same `POLICY`.

  > **Status (M7, first slice).** The tunnel **transport protocol core** has landed and is hermetically
  > tested: the length-prefixed frame codec, the relay registration handshake (a missing/invalid qfs
  > Cloud sign-in token is *refused* — decision N), and request/response **multiplexing** over an
  > in-memory transport, all in the pure `qfs-tunnel` crate with the redaction floor inherited from the
  > shared HTTP DTOs (no token crosses a frame's log/`Debug` in cleartext). The **live outbound TCP/TLS
  > dial** to a running qfs Cloud relay — and the relay's own hosting, node addressing/discovery, and
  > reconnect/backoff — are a **documented seam** (the `qfs` binary's composition root), not yet wired
  > to a socket; there is no live two-node tunnel yet.
  >
  > The **`/hosts/<host>/claude/...` AI-sessions driver** has now landed as a *local* slice (t64): the
  > pure, credential-free describe surface (`/claude/sessions` is a read-only `RelationalTable`;
  > `/claude/sessions/<id>/instructions` is an append-only `AppendLog`) is registered and catalogued,
  > and the binary wires a **fail-closed, opt-in** on-disk session source (`QFS_CLAUDE_SESSIONS`) so a
  > local `FROM /claude/sessions |> WHERE status='running'` read and a gated
  > `INSERT INTO /claude/sessions/<id>/instructions` append both work against real session state.
  > Steering is a **reversible append** (an explicit COMMIT); `DESCRIBE`/`PREVIEW` touch nothing; the
  > schema carries no token/transcript column. Honouring **decision K, qfs never hosts or calls an
  > LLM** — this is a path façade over session metadata + an instruction log, not an inference call.
  > The model itself runs elsewhere. What is still ahead: the *cross-machine* hop — resolving
  > `/hosts/<remote>/claude/...` over the t63 tunnel and re-checking `POLICY` at the destination — is a
  > **documented seam**, and the exact coupling to Claude Code's on-disk session format is flagged as
  > an open product decision behind the injected `SessionSource` seam.
- **Scheduled jobs** (decision M) are **externalized**: OS `cron` (individual) or Cloudflare Cron
  Triggers (managed) fire a `qfs run` / saved plan on schedule — qfs runs no scheduler of its own, so
  exactly-once and distribution are the platform's job, not a qfs leader election.

### 3.4 The admin page

A team needs administration, so the dashboard has an **admin area**: manage members and invites, view
and grant `POLICY`, add/rotate `connections`, review the audit log, watch migrations, and (on the
managed tier) handle billing.

It fits the architecture cleanly because **administration is also "everything is a path."** The admin
surface is a view over the deployment's own `/sys/...` paths — `/sys/users`, `/sys/policies`,
`/sys/connections`, `/sys/audit`, `/sys/projects` — backed by the System DB. So the admin page is the
dashboard rendering the same engine over the same grammar; a super-admin can do every administrative
action as a qfs statement too, preserving the one-engine-three-faces constraint.

The **first slice is shipped ✅ (t53):** the `/sys/*` paths are real — a `qfs-driver-sys` driver backs
them on the System DB, every face can read them (`/sys/audit |> WHERE …`), `/sys/connections`
projects names + metadata only (never secrets), `/sys/audit` is append-only and every `/sys` mutation
appends to it, and a gated `INSERT INTO /sys/policies` (default-deny policy gate, transactional +
audited) is the one write. The dashboard renders the first thin admin views over those paths through
the same `/api/describe` + `/api/run` bridge — no admin capability the CLI lacks. What remains open
(🧭) is the breadth of views and the local-super-admin vs. project-admin split (below).

```qfs
# the first gated admin write — granting access is itself a previewable, committable, audited statement
insert into /sys/policies values (name => 'analysts', allow => 'select', target => '/sql/*')
```

::: info The admin page is planned; its implementation is open
We *will* have an admin page — modeling it as `/sys/*` paths keeps it consistent with the rest of qfs,
but **how** it is built (which views ship first, how much is generated from the `/sys` schema vs.
hand-built, the local-super-admin vs. project-admin split) is a deliberate design question still to be
settled, not a decision baked in here.
:::

---

## Part 4 — The architecture underneath

### 4.1 Identity is not authorization 🧭 (decisions B, C, D)

Two concerns the current draft conflated, now kept separate:

- **Identity (authentication)** — *who you are*. A `users` + `accounts` table in SQLite, present at every
  tier. Local sign-up, or an upstream IdP via federation.
- **Authorization (OAuth/OIDC)** — *what may connect*. A qfs server is its own authorization server so it
  can also be a remote MCP server (Part 2). The two compose: the human authenticates against identity;
  the agent's client authorizes against the OAuth surface.

### 4.2 Persistence: all SQLite, stateless at scale 🧭 (decisions E, F)

| Database | Scope | Holds |
| --- | --- | --- |
| **System DB** | Per host | Projects, cross-project config, `/sys/*` (users, policies, connections, audit) |
| **Project DB** | Per project | That project's `connections`, config, and state |

Credentials are **envelope-encrypted** at rest: a data-key encrypts the secret columns inside the DB,
and the data-key is itself wrapped under a key-encryption-key — today derived from the
`QFS_PASSPHRASE` passphrase (argon2id); an OS-keychain source to unwrap it without an env var is the
flagged next step. This is **now the default credential backend** (the SQLite Project DB's
`secret_store`/`secret_meta` tables) — one persistence path from a single-user laptop to the managed
cloud. That single-key form is the base case of the **credential vault** (§4.5), which generalizes
the same envelope into a team key hierarchy. Because the project is still experimental, there is **no
migration** of the old encrypted file vault; the ideal is built fresh — re-run `qfs connection add` once
per existing connection (decision E).

Scale keeps SQLite semantics everywhere by using **distributed SQLite**: **Cloudflare Workers + D1**
(primary) or **AWS Lambda + EFS** (alternative). The binary stays **stateless** — a request arrives at
any instance, and a **trusted reverse proxy injects the tenant→DB route**; a client can never name a
database, which is the tenant-isolation boundary. Add capacity by adding instances. When the binary is
updated and relaunched, **embedded migrations** apply System-DB changes safely in the same motion.

### 4.3 Scheduling is external — qfs is not a scheduler 🧭 (decision M)

qfs deliberately **does not build its own scheduler.** Owning a cron daemon (and, at scale, leader
election so a job fires exactly once) is a large, stateful problem that the platforms beneath qfs
already solve well — so qfs externalizes the *when* and keeps only the *what*:

- **Individual / local** — the OS `cron` (or `launchd`, systemd timers, Task Scheduler) invokes a
  `qfs run '<statement>' --commit` line on its schedule. No qfs daemon, no qfs-side scheduler state.
- **Managed tier** — **Cloudflare Cron Triggers** (the `[triggers] crons = [...]` in wrangler) fire
  the qfs Worker on schedule; the platform owns distribution and exactly-once, not qfs.

qfs's job is to make the invokable unit clean and safe: a one-shot `qfs run`, a **saved named plan**
the external scheduler calls by name, or an `ENDPOINT` it hits — each running through the same
preview→commit engine under the same `POLICY`. Removing the internal scheduler also removes the
System-DB lease, the leader-election, and the cron daemon from the plan entirely — that distributed,
exactly-once complexity is now the platform's, not ours.

> **Status (t65 — shipped) ✅.** The internal scheduler is **retired**: the `qfs-cron` crate, its
> tokio interval daemon, the `JobStore`/lease, and the `qfs serve` scheduler thread are **deleted**.
> A `CREATE JOB … EVERY … DO …` row is now a saved named plan + cadence only — the `/server/jobs`
> definition surface is unchanged, but qfs no longer fires it. The invokable unit ships as
> **`qfs job run <config> <name>`** (PREVIEW by default; `--commit` applies through the **same**
> policy gate + `IrreversibleGuard` the CLI one-shot uses) and **`qfs job cron <config> <name>`**
> emits the OS-cron crontab line. The managed tier reads the same cadence from the generated
> `[triggers] crons` block. Idempotency is now the author's responsibility (external schedulers are
> at-least-once; there is no internal run-ledger to dedup a re-fire).

### 4.4 How a mixed-source query resolves — the same on every face ✅ (mechanism) / 🧭 (cloud routing)

A federated query (a `JOIN`/`UNION`/`EXCEPT`/`INTERSECT` that straddles two services) resolves in
**two stages**, and the resolution is **identical** whether the engine runs as the local CLI, a
self-hosted server daemon, or a cloud Worker — that sameness is the **one engine** constraint applied
to execution: the same engine across every **place it runs**, just as the three faces share it across
every surface.

1. **Pushdown per source.** The planner (`qfs_pushdown`) finds each **maximal same-source subtree**
   and negotiates it against that driver's `PushdownProfile`, emitting **one native operation per
   source** — one SQL query to Postgres, one filtered API call to GitHub — for the part the backend
   can do itself (`WHERE`, `LIMIT`, `GROUP BY`, projection). qfs over-fetches safely and re-checks
   locally, so a partial pushdown is never a wrong answer.
2. **Local combine of the residual.** Only the **cross-source residual** — the join/filter/sort/
   aggregate that genuinely spans services — is run **in-process** by the in-house relational engine
   (`qfs_engine::MiniEvaluator`, behind the `CombineEngine` seam; ADR-0002). A `JOIN` across two
   different sources is always a local combine over each side's pushed-down result.

The shipped mechanism (pushdown + local combine, RFD §6, ADR-0002) is byte-for-byte the same code in
all three **places it runs** (local CLI, self-hosted daemon, cloud Worker) — **only where the process
runs changes.** At cloud scale the binary stays
**stateless** and a trusted reverse proxy injects the tenant→DB route (§4.2, decision F, 🧭); that
routing changes *which* Project DB a request reaches, **not** how the federated query itself resolves.
Run `qfs describe <path>` to see the **pushdown** line for any node — it tells you exactly what will run
inside the service versus locally. A worked example is in the [query cookbook](/query-cookbook).

What the residual stage owns (so a federated read has defined semantics, not just a shape): **final
`ORDER BY`/`LIMIT`/`DISTINCT` are always re-applied locally** after the combine (a per-source order is
never trusted as the global order); a **read snapshots each source once** — there is no cross-source
transaction, so two sources may be milliseconds apart, and any cross-source consistency claim must be
explicit, not assumed; **pagination is the planner's**, walking each source's native cursor and
bounding fan-out under the per-source rate limit; and a `JOIN`/`UNION` never invents or silently drops
rows — duplicates are resolved only by an explicit `DISTINCT` or key. The open question this defers is
**cost** (a cross-source join with no pushed-down predicate can fetch a lot); the planner's job is to
push enough filter down that the local residual stays small, and `describe`'s pushdown line is where
you see whether it did.

### 4.5 The credential vault — sharing secrets across a team safely 🧭 (decision U)

A connection's secret (a `postgres://…` string, an OAuth refresh token) is the most sensitive thing qfs
holds, and a team needs to *share* it without ever passing it hand to hand. The vault is how — it takes
the envelope encryption of §4.2 and turns it into a small **key hierarchy** that a whole team can use,
rotate, and revoke.

**The key hierarchy (three levels).**

| Level | Key | Lives | Job |
| --- | --- | --- | --- |
| 1 | **Root KEK** | OS keychain (laptop) / cloud KMS (managed) — **never in the DB** | wraps the data keys |
| 2 | **Data key (DEK)** | encrypted in the DB, unwrapped only in memory | encrypts the secret columns |
| 3 | **Secret** | encrypted column in the Project/System DB | the actual credential |

At rest the DB holds only ciphertext; the key that unlocks it is held outside the DB. A stolen database
file is inert without the root KEK — that is the at-rest boundary.

**Sharing is by `policy`, not by copying.** A team does **not** distribute the secret. Whoever sets a
connection up adds it **once** at the project level (§3.3); from then on every member acts *as the team*
through it, and **actor-based `policy` decides who may run a plan that uses it**. Nobody else re-enters,
sees, or holds the raw value. The two-layer identity from §3.3 still holds: the audit row records the
*actor* (the human) and the *connection* (the credential the effect rode), so "the team acted" is always
traceable to one person.

**The secret never enters the query language.** Because every qfs statement is previewable, auditable,
and logged, putting a secret in one would leak it. So the credential has its **own input path** — a CLI
prompt (`qfs connection add sql pg` reads it from stdin/keychain) or the admin form — and the language
only ever sees **metadata**. Adding/rotating is an admin action over `/sys/connections`; *querying* it
returns label → provider, endpoint, who-may-use, last-rotated — **never** the secret:

```qfs
# 🧭 proposed — who can use each team connection, and when it was last rotated (secret never returned)
/sys/connections
|> where project == 'billing'
|> select label, provider, endpoint, may_use, last_rotated
|> order by last_rotated
```

**Rotation & revocation are first-class.** Removing a member revokes their `policy` to use the
connection; a rotation re-mints the secret and re-wraps the DEK, and both land as `/sys/audit` events
(the admin page already lists "add/rotate connections", §3.4). Rotation is the clean answer to "someone
left" — the credential they could trigger is replaced, not just un-grant­ed.

**Optional end-to-end wrap for the most sensitive connections.** By default the **server** can unwrap a
DEK because it must execute the plan (decisions C/F) — that is the managed-tier trust boundary. For a
connection too sensitive for that, the DEK can be **additionally wrapped to individual members' public
keys** (registered in `/sys/users`), so it is decryptable *only* by those members and not by the server
at rest. The trade-off is explicit and consistent with the safety modes (decision J): such a connection
**cannot be used by an agent unattended** — a human with the key must be in the loop — which is exactly
the [short-lived credential brokering](#part-5--expanded-possibilities) idea taken to its limit.

> **Threat model, stated plainly.** (1) *DB-at-rest theft* → ciphertext only; root KEK is elsewhere.
> (2) *Member offboarding* → revoke `policy` + rotate. (3) *Server compromise (managed tier)* → the
> server holds the root KEK to execute, so this is the trust boundary; the E2E wrap above is the
> mitigation for connections that must survive it. (4) *Accidental leak via the language* → impossible
> by construction: secrets never appear in a statement, only metadata does.

> **Status (M9, brokering CORE — t66).** The **OAuth-brokering data model + the team-connection
> provisioning + the security gates** have landed and are hermetic. qfs Cloud is modelled as the OAuth
> **broker**: it holds one broker client registration per provider (the `client_id` + the client
> secret, the crown jewel) and mints a **team-scoped** token to a team's connection, so members act
> *as the team* with no personal OAuth client. A `Broker` trait is the seam; an in-memory
> `FixtureBroker` is the reference impl the tests drive. Proven by construction: a team connection is
> provisioned through the broker and the brokered token is sealed at rest (t43 envelope), reusing
> t81's project-owned `shared_connection` gate; a **non-member is refused with no token minted**; a
> brokered grant is **team-scoped** and cannot be replayed for another team; the **broker client
> secret is envelope-encrypted at rest**; and USE decrypts only after BOTH the team-membership gate
> and the t57/t81 actor-policy gate pass (no secret crosses to an unauthorized actor). **The live qfs
> Cloud broker endpoint remains a documented seam** — a network `Broker` impl, not in this repo and
> not claimed to work; the binary's commit path would call it exactly where it calls the fixture.
> **Open product decisions flagged:** (a) brokering *custody* — central qfs-Cloud token custody vs. the
> tenant Project DB at rest (this slice implements the tenant-Project-DB choice, the testable one);
> (b) per-user-override precedence over a team default. Managed monitoring + billing are still ahead.

### 4.6 Observability is external — qfs emits, it does not store 🧭 (decision V)

The same reasoning that keeps qfs out of the scheduler business (§4.3) keeps it out of the monitoring
business: a metrics store, dashboards, retention, and alerting are a large stateful problem the
platforms beneath qfs already solve. So qfs **emits a standard telemetry surface** and lets whatever is
watching consume it — and that single choice is what lets the managed tier and a self-hoster run the
*same* signals through different tools.

> **Implementation status (t77 — the externalized sinks).** The sink layer is live: a `TelemetrySink`
> abstraction over the three signals, selected by `QFS_TELEMETRY_SINK` (`file` default / `stdout` /
> `OTel`). The **`file`** ✅ and **`stdout`** ✅
> sinks are fully wired — each commit emits the audit signal (the same metadata-only events as the
> t76 chain), the `qfs_commit_total` / `qfs_commit_effects_total` metric counters, and a `qfs.commit`
> trace span as one JSONL line per record (best-effort; a sink failure never breaks the commit). The
> **`OTel`** sink is a **present-but-parked seam** 🚧 — selectable and metadata-rendering, but the OTLP
> exporter is **not wired** (no vetted exporter crate in the offline build cache; t77 does not
> hand-roll the OTLP wire protocol), so it logs the record it would export rather than shipping it.
> **`/sys/metrics`** ✅ is live as the in-process counter live view (the snapshot, not a durable time
> series — qfs emits, it does not store).
>
> **Implementation status (t78 — audit-chain sealing).** The **seal** layer is live: a signed
> **checkpoint** over the t76 chain HEAD — `(seq, content_hash, prev_hash, issued_at)` signed with the
> SAME AS ES256 key that signs access tokens (`qfs_oauth::sign_seal`/`verify_seal`) ✅ — plus a
> consumer-side **verify** that recomputes a stored chain against a seal and reports the first
> divergence or a head truncation/fork ✅. The seal is handed to a **WORM witness** through a
> `WormSink` seam: the local append-only **`file`** witness ✅ is fully wired (one JSONL seal per
> line, append-only), and the **external** witness (S3 Object Lock / transparency log) is a
> **present-but-parked seam** 🚧 — selectable + metadata-rendering, but the real client is **not
> wired** (no vetted Object-Lock / transparency-log crate in the offline build cache; t78 does not
> hand-roll RFC 6962 / a vendor protocol). The seal **cadence is externalized** (decision M) — an
> invokable unit fired by OS cron / Cron Triggers, not a qfs-internal scheduler. The
> **`/sys/audit/seals`** read surface below remains 🧭 proposed (qfs emits the seals to the witness;
> exposing them back as a queryable admin view is the follow-up slice).

**Three signals, one surface.**

| Signal | Query it as qfs | Emitted to a sink | Carries |
| --- | --- | --- | --- |
| **Audit** | `/sys/audit` *(live view)* | hash-chained, as `file`/`stdout`/OTel logs *(consumer stores & retains)* | who did what: actor, connection, verb, path, committed, ts |
| **Metrics** | `/sys/metrics` | `file`/`stdout`/OTel metrics | preview/commit counts, per-driver latency, pushdown ratio, rate-limit hits, errors |
| **Traces** | *(per plan)* | OTel spans (OTLP) | `describe` → pushdown-per-source → local combine → `commit` |

**Three prepared sinks: `file`, `stdout`, `OTel`.** qfs writes each signal to one of three outputs, and
nothing else:

- **`file`** *(default)* — a local file; zero dependencies, durable on a laptop or a VM. Point it at a
  rotating path (size- or time-based, like `logrotate`) and a single-user install gets bounded-on-disk
  audit and metrics with no monitor to run.
- **`stdout`** — structured lines on stdout, the 12-factor / container-native path: the platform
  (systemd-journald, k8s, Cloudflare) captures and ships them. The natural pick for a **stateless
  server** (decision F) that has no persistent disk to write a file to.
- **`OTel`** *(recommended)* — OTLP for traces, metrics, and logs to any collector; vendor-neutral and
  the richest. Everything downstream — Prometheus (via the collector), Grafana, Datadog, a SIEM, qfs
  Cloud's own dashboards — reads from here. qfs ships this one standard and integrates with no vendor
  directly.

**One surface, two consumers** — this is the whole point. On the **managed tier**, qfs Cloud consumes
the OTel stream into its own dashboards and alerts; there is nothing to set up. **Self-hosted**, you
choose the sink — `file` to start, `stdout` if a platform already captures it, `OTel` once you have a
collector — and get the same observability qfs Cloud has. Same signals, same format; only who owns the
pipe changes. That is the **one engine, three faces** symmetry (and the decision-M scheduler split)
applied to operations. By deployment the defaults fall out naturally: a **local** binary writes a `file`
with no setup; a **server** emits `stdout` or `OTel` and keeps nothing but the audit chain head (below).

**It is also just paths**, so the zero-dependency floor still holds: before standing up any external
monitor, you can *query* audit and metrics with qfs itself, and graduate to a real monitor only when you
want history, correlation, or alerting:

```qfs
# 🧭 proposed — commit error counts per driver, straight from the metrics path (no external tool needed)
/sys/metrics
|> where name == 'commit_errors'
|> group by driver
|> aggregate sum(value) as errors
|> order by errors desc
```

**Metadata only.** Telemetry carries the *shape* of activity — verbs, paths, counts, latencies — and
**never** secrets or row data, the same boundary `describe` enforces (§3.2). Audit reads are
`policy`-gated like any other `/sys/*` path, and the export redacts exactly as the language does, so
shipping telemetry off-box never widens what a viewer was already entitled to see.

**Tamper-evident audit.** qfs **emits** audit events; it does not keep them — the durable store, and how
long it is kept, are the consumer's (a WORM bucket, a transparency log, an OTel sink). What qfs
guarantees is that the *exported* record is verifiable wherever it lands: each event is **hash-chained**
— it carries a hash of *(its own content + the previous event's hash)* — so any edit, reorder, or
deletion at the destination breaks the chain and is detectable by recomputation. A chain alone can be
re-forged wholesale by whoever controls that store, so qfs periodically **seals the chain head to an
independent, write-once witness** (S3 Object Lock, a transparency log, a signed off-box anchor). The
*only* audit state qfs must hold is that latest head, to continue the chain; everything else has already
left. Because the witness is append-only and outside any single store, history cannot be rewritten
without contradicting an anchor that can no longer change. Same emit-don't-store split as the rest of
§4.6 — qfs produces the chain and the seals; **storing the log and setting its retention belong to the
platform** (qfs Cloud on the managed tier; your own sink self-hosted). Verification is a read the
consumer runs over what *it* stored, comparing against the seals qfs emitted:

```qfs
# 🧭 proposed — qfs reports its emitted seals; the consumer recomputes its own store against them
/sys/audit/seals
|> select range, chain_head, anchor, sealed_at
|> order by sealed_at desc
```

---

## Part 5 — Expanded possibilities

Beyond the confirmed plan, capabilities the foundation makes cheap — candidates, not commitments:

- **Change subscriptions / CDC.** `TRIGGER` today reacts to a poll; a webhook-or-stream-backed
  subscription would let `/mail`, `/github`, `/slack` push changes, turning automation real-time.
- **A driver SDK + registry.** The closed-core/open-registry split already invites community drivers; a
  published SDK and a signed registry would let teams add private backends as paths.
- **Short-lived credential brokering.** Instead of long-lived `connections`, mint per-plan, per-scope
  tokens that expire at commit — least privilege taken to its limit, and the natural next step beyond
  the credential vault's end-to-end wrap (§4.5).
- **Approval workflows as data.** The selectable safety mode (decision J) generalizes to multi-party
  approval: an irreversible plan becomes a row in `/sys/approvals` a second human signs off.
- **Richer telemetry analytics.** The telemetry surface is now confirmed (§4.6, decision V); the
  open extension is what you build *on* it — anomaly detection over `/sys/audit`, cost attribution from
  pushdown metrics, SLO burn-rate alerts — none of which qfs needs to own.
- **An agent mesh.** With `/hosts/*/claude/...` across machines, a coordinator agent on one host can fan work to
  agents on others and collect results — multi-agent orchestration expressed in qfs.

---

## Part 6 — Phased delivery plan

Dependency-ordered, architecture first (decision A). Each phase leaves the tree green and the docs honest
about exactly what now works.

| Phase | Theme | Delivers | Unlocks |
| --- | --- | --- | --- |
| **M0** | Persistence foundation | System/Project SQLite, the credential vault's envelope base (root KEK → data key → secret columns, §4.5), a hash-chained audit event stream (`/sys/audit` live view; durable store is the consumer's), embedded migrations; `accounts`→`connections` rename | The single world the dashboard and CLI agree on |
| **M1** | Identity store | `users`/`accounts` tables, local sign-up, session handling | A real "who" at every tier |
| **M2** | Server-as-MCP + OAuth AS | MCP `describe`/`preview`/`commit` tools; OAuth 2.1 AS (PRM, AS-metadata, DCR, PKCE); Claude connects | Part 2 — the agent's single endpoint |
| **M3** | Dashboard at parity | Embedded SPA over the same engine; preview→commit cards; first `/sys/*` admin views; the telemetry surface self-hosters consume — audit/metrics/traces to `file`/`stdout`/`OTel` sinks (file default, OTel recommended), and audit-chain sealing to an external WORM/transparency log (§4.6) | The second face; admin page begins; self-host monitoring |
| **M4** | Cloud tier | `connections` for Drive/GitHub/Gmail with consent flows; sign-in mandatory for cloud drivers | Local + Cloud usage |
| **M5** | Self-hosted multi-user | Invites (email / one-time URL), upstream OIDC federation, extended `policy`/ACL, selectable AI safety modes, **team credential sharing** (shared connections by `policy`, rotation/revocation; optional per-recipient E2E wrap — §4.5) | Teams on their own server |
| **M6** | Language core | `let`, lambdas (incl. named user-defined functions as `let`-bound lambda values), `map`/`filter`/`reduce`; a static primitive type system (`bool`, `i32`/`i64`/`u32`/`u64`, `f32`/`f64`, `string`, `Resource`) checked at plan time; the `=` binds / `==` compares operator split; the `/path` `Resource` literal (drop `from`, unquote `policy`/`member_of` paths) and lowercase keywords; reversible-only `transaction` + commit-point | Part 1.2 expressiveness |
| **M7** | Agent fabric *(qfs Cloud)* | qfs-native outbound tunnels (require qfs Cloud sign-in), the `/hosts/<host>/claude/...` driver, the cross-machine scenario | Part 3.3 fleet |
| **M8** | External scheduling | Docs + thin scaffolding to drive `qfs run` / saved plans from **OS cron** (individual) and **Cloudflare Cron Triggers** (managed); no internal scheduler | Part 4.3 |
| **M9** | Managed Team | qfs Cloud OAuth brokering, team connections, managed monitoring (qfs Cloud consumes the §4.6 telemetry — dashboards/alerts with no setup), billing (free individual / paid team) | The top tier |
| **M+** | Expansions | CDC, driver SDK, credential brokering, approvals, richer telemetry analytics, agent mesh | Part 5 |

### How it holds together

Read top to bottom, the plan is one idea repeated: **add reach without adding special cases.**

- More **places to run** (local → cloud → self-hosted → managed) — same grammar.
- More **people** (invites, OIDC, ACLs, audit) — same preview-then-commit safety.
- More **machines** (tunnels, distributed scheduling) — same one identity.
- More **power** (`let`, higher-order functions, transactions) — same closed core.
- More **faces** (CLI, dashboard — incl. the admin page — and MCP) — same one engine.

Every tier, every machine, every collaborator, and every agent meets qfs as the same small, safe
grammar. That sameness is the product — and protecting it is what this plan is for.
