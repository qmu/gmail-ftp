---
aside: false
---

# The qfs query cookbook

[[toc]]

This is the payoff of the whole plan: a broad, worked-by-example catalogue of the queries qfs will
support once the roadmap is delivered (M0→M+). Read the [roadmap](/roadmap) for the *why*; read this for the *how*,
in the grammar you would actually type. The recipes deliberately **combine features and interact** —
a federation join feeding a transaction, a policy driven by a directory group, an agent's MCP commit
gated by a safety mode — because the interactions are the product.

::: warning Direction, not documentation
Every recipe is tagged by the milestone that delivers its capability. The
[generated reference](/language) is always the truth about the binary *today*; this cookbook is the
truth about where the grammar is *going*. Each ` ```qfs ` block carries a machine-readable header
comment — `# qfs-cookbook: grammar=core|extended; milestone=…; features=…` — and a test
(`packages/qfs/crates/test/tests/roadmap_cookbook.rs`) extracts every block: **`grammar=core` recipes must parse
with today's parser** (so the catalogue can never drift from the real grammar), while
`grammar=extended` recipes need a construct the parser does not ship yet — the M6 functional core
(`LET`, lambdas, `TRANSACTION`) *and* other not-yet-shipped grammar (inline `GROUP BY`/`AGGREGATE`,
`||`, source `AS` aliases, richer DDL, …) — and are tracked as a living coverage report, each promoted
to a hard assertion as the milestone that delivers it lands. `grammar=core` means *the grammar of
the statement* is shipped — a recipe may still address a path whose driver arrives later (`/sys`,
`/hosts`, `/directories`), since a path is just a token to the parser.
:::

The safety floor holds in every recipe: **describe is pure, preview touches nothing, commit is
explicit, and irreversible effects (`CALL mail.send`, merges, deletes) demand
`--commit-irreversible`** — even when a line is not spelled out, that is the contract it runs under.

## Reads, filters & projection

The simplest thing qfs does is read one source, narrow it with a predicate, and shape the columns you keep — and that single skill works identically against a mailbox, a Postgres table, a GitHub repo, a Slack channel, a git tree, an object store, or a planned admin mount. These recipes stay inside one source per query and lean on the read vocabulary: `WHERE`/`SELECT`/`EXTEND`/`ORDER BY`/`LIMIT`/`DISTINCT`, the full predicate set (`LIKE`, `~`, `IN`, `BETWEEN`, `ANY`, `AND`/`OR`/`NOT`), dotted path navigation into nested objects, `EXPAND` of nested collections, and the temporal coordinate (`@version` and `AS OF`). Everything here is read-only, so `PREVIEW` shows exactly what would come back and nothing is ever touched.

**Triage the inbox: unread mail from outside the company, newest first.**

```qfs
# qfs-cookbook: grammar=core; milestone=M2; features=FROM,WHERE,NOT,LIKE,AND,SELECT,ORDER BY,LIMIT
FROM /mail/inbox
|> WHERE is_read = false
     AND NOT from LIKE '%@acme.com'
|> SELECT from, subject, received_at
|> ORDER BY received_at DESC
|> LIMIT 50
```

**Find invoices that are large, recent, and still unpaid.**

```qfs
# qfs-cookbook: grammar=core; milestone=M0; features=FROM,WHERE,BETWEEN,AND,IN,ORDER BY
FROM /sql/pg/invoices
|> WHERE amount_due BETWEEN 5000 AND 250000
     AND status IN ('open', 'overdue')
     AND issued_at > '2026-04-01'
|> ORDER BY amount_due DESC
```

**List open pull requests authored by the platform team.**

```qfs
# qfs-cookbook: grammar=core; milestone=M1; features=FROM,WHERE,IN,AND,SELECT,ORDER BY
FROM /github/acme/web/pulls
|> WHERE state = 'open'
     AND author IN ('rin', 'kenji', 'sora', 'mei')
|> SELECT number, title, author, created_at
|> ORDER BY created_at ASC
```

**Search a Slack channel for anything that looks like an incident.**

```qfs
# qfs-cookbook: grammar=core; milestone=M2; features=FROM,WHERE,~,OR,SELECT,ORDER BY,LIMIT
FROM /slack/acme/incidents/messages
|> WHERE text ~ '(?i)(outage|sev[0-9]|rollback|paging)'
     OR text LIKE '%down%'
|> SELECT ts, user, text
|> ORDER BY ts DESC
|> LIMIT 100
```

**Show the most recent commits touching the auth module.**

```qfs
# qfs-cookbook: grammar=core; milestone=M1; features=FROM,WHERE,LIKE,AND,SELECT,ORDER BY,LIMIT
FROM /git/app/commits
|> WHERE files LIKE '%src/auth/%'
     AND committed_at > '2026-05-01'
|> SELECT sha, author, subject, committed_at
|> ORDER BY committed_at DESC
|> LIMIT 25
```

**List the largest objects in an S3 bucket prefix.**

```qfs
# qfs-cookbook: grammar=core; milestone=M0; features=FROM,WHERE,LIKE,AND,SELECT,ORDER BY,LIMIT
FROM /s3/acme-backups
|> WHERE key LIKE 'db-dumps/%'
     AND size > 1073741824
|> SELECT key, size, last_modified, storage_class
|> ORDER BY size DESC
|> LIMIT 20
```

**Find quarterly report PDFs in a Drive folder, biggest first.**

```qfs
# qfs-cookbook: grammar=core; milestone=M0; features=FROM,WHERE,LIKE,AND,SELECT,ORDER BY
FROM /drive/Reports
|> WHERE name LIKE '%.pdf'
     AND name ~ 'Q[1-4]'
|> SELECT name, size, modified_at, owner
|> ORDER BY size DESC
```

**Pull the top landing pages by sessions for last month.**

```qfs
# qfs-cookbook: grammar=core; milestone=M4; features=FROM,WHERE,AND,SELECT,ORDER BY,LIMIT
FROM /ga/acme.com/pages
|> WHERE date BETWEEN '2026-05-01' AND '2026-05-31'
     AND sessions > 0
|> SELECT page_path, sessions, bounce_rate
|> ORDER BY sessions DESC
|> LIMIT 25
```

**Grep a local CSV export for rows mentioning a region.**

```qfs
# qfs-cookbook: grammar=core; milestone=M0; features=FROM,DECODE,WHERE,OR,SELECT
FROM /local/exports/sales.csv
|> DECODE csv
|> WHERE region = 'APAC'
     OR region = 'ANZ'
|> SELECT order_id, region, total, closed_at
```

**Compute days-overdue on the fly while listing late invoices.**

```qfs
# qfs-cookbook: grammar=core; milestone=M0; features=FROM,WHERE,EXTEND,SELECT,ORDER BY
FROM /sql/pg/invoices
|> WHERE status = 'overdue'
|> EXTEND days_late = date_diff('day', due_date, now())
|> SELECT customer, amount_due, due_date, days_late
|> ORDER BY days_late DESC
```

**Build a display label for each open issue from its parts.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M1; features=FROM,WHERE,EXTEND,||,SELECT,ORDER BY
FROM /github/acme/web/issues
|> WHERE state = 'open'
|> EXTEND label = '#' || number || ' — ' || title
|> SELECT label, assignee, milestone
|> ORDER BY number ASC
```

**Get the distinct senders who have mailed support this week.**

```qfs
# qfs-cookbook: grammar=core; milestone=M2; features=FROM,WHERE,AND,LIKE,SELECT,DISTINCT,ORDER BY
FROM /mail/inbox
|> WHERE to LIKE '%support@acme.com%'
     AND received_at > '2026-06-19'
|> SELECT from
|> DISTINCT
|> ORDER BY from ASC
```

**Reach into nested customer JSON to filter by billing country.**

```qfs
# qfs-cookbook: grammar=core; milestone=M0; features=FROM,WHERE,AND,SELECT,path-nav
FROM /sql/pg/customers
|> WHERE billing.address.country = 'JP'
     AND billing.plan.tier = 'enterprise'
|> SELECT id, name, billing.plan.tier, billing.address.city
```

**Find PRs whose head commit failed CI, reading nested status.**

```qfs
# qfs-cookbook: grammar=core; milestone=M1; features=FROM,WHERE,AND,SELECT,path-nav,ORDER BY
FROM /github/acme/web/pulls
|> WHERE state = 'open'
     AND head.status.state = 'failure'
|> SELECT number, title, head.ref, head.status.context
|> ORDER BY number DESC
```

**Explode message recipients into one row per addressee.**

```qfs
# qfs-cookbook: grammar=core; milestone=M2; features=FROM,WHERE,EXPAND,SELECT,DISTINCT
FROM /mail/sent
|> WHERE subject LIKE '%Release 4.0%'
|> EXPAND recipients
|> SELECT recipients.email, recipients.kind
|> DISTINCT
```

**List every attachment across this week's invoices mail.**

```qfs
# qfs-cookbook: grammar=core; milestone=M2; features=FROM,WHERE,AND,EXPAND,SELECT,ORDER BY
FROM /mail/inbox
|> WHERE subject ~ '(?i)invoice'
     AND received_at > '2026-06-19'
|> EXPAND attachments
|> SELECT from, attachments.filename, attachments.size
|> ORDER BY attachments.size DESC
```

**Flatten order line-items to find SKUs that sold under a discount.**

```qfs
# qfs-cookbook: grammar=core; milestone=M0; features=FROM,EXPAND,WHERE,SELECT,ORDER BY
FROM /sql/pg/orders
|> EXPAND line_items
|> WHERE line_items.discount_pct > 0.25
|> SELECT id, line_items.sku, line_items.qty, line_items.discount_pct
|> ORDER BY line_items.discount_pct DESC
```

**Expand requested reviewers to see who is blocking each PR.**

```qfs
# qfs-cookbook: grammar=core; milestone=M1; features=FROM,WHERE,EXPAND,SELECT,ORDER BY
FROM /github/acme/web/pulls
|> WHERE state = 'open'
|> EXPAND requested_reviewers
|> SELECT number, requested_reviewers.login, requested_reviewers.team
|> ORDER BY number ASC
```

**Match support tickets against any of a set of urgent keywords.**

```qfs
# qfs-cookbook: grammar=core; milestone=M0; features=FROM,WHERE,ANY,AND,SELECT,ORDER BY
FROM /sql/pg/tickets
|> WHERE subject ~ ANY ('refund', 'chargeback', 'cancel', 'lawsuit')
     AND status <> 'closed'
|> SELECT id, subject, priority, opened_at
|> ORDER BY opened_at ASC
```

**Find files whose name contains any of several report codes.**

```qfs
# qfs-cookbook: grammar=core; milestone=M0; features=FROM,WHERE,LIKE,ANY,SELECT,ORDER BY
FROM /drive/Finance
|> WHERE name LIKE ANY ('%FY26%', '%FY27%', '%audit%')
|> SELECT name, owner, modified_at
|> ORDER BY modified_at DESC
```

**Read one file at a tagged release straight out of git.**

```qfs
# qfs-cookbook: grammar=core; milestone=M1; features=FROM,@version,DECODE,SELECT
FROM /git/app@v1.2/Cargo.toml
|> DECODE toml
|> SELECT package.name, package.version, package.edition
```

**Compare config keys present in a specific S3 object version.**

```qfs
# qfs-cookbook: grammar=core; milestone=M0; features=FROM,@version,DECODE,WHERE,SELECT
FROM /s3/acme-config/app/settings.json@K7sJpq2vN1
|> DECODE json
|> WHERE feature_flags.new_billing = true
|> SELECT environment, feature_flags.new_billing, rollout.percent
```

**Read a Drive doc as it stood at an earlier revision.**

```qfs
# qfs-cookbook: grammar=core; milestone=M0; features=FROM,@version,DECODE,SELECT,path-nav
FROM /drive/Specs/pricing.md@rev_88
|> DECODE md
|> SELECT title, status, owner, body
```

**See how an order row looked before a disputed edit, using AS OF.**

```qfs
# qfs-cookbook: grammar=core; milestone=M0; features=FROM,AS OF,WHERE,SELECT
FROM /sql/pg/orders AS OF '2026-05-01'
|> WHERE id = 'ord_91823'
|> SELECT id, status, total, shipped_at, updated_at
```

**Audit the prior price book: enterprise SKUs as of quarter start.**

```qfs
# qfs-cookbook: grammar=core; milestone=M0; features=FROM,AS OF,WHERE,AND,SELECT,ORDER BY
FROM /sql/pg/price_book AS OF '2026-04-01'
|> WHERE tier = 'enterprise'
     AND active = true
|> SELECT sku, list_price, currency
|> ORDER BY list_price DESC
```

**List the branches and tags currently pointing into a repo.**

```qfs
# qfs-cookbook: grammar=core; milestone=M1; features=FROM,WHERE,OR,SELECT,ORDER BY
FROM /git/app/refs
|> WHERE kind = 'branch'
     OR kind = 'tag'
|> SELECT name, kind, target_sha, updated_at
|> ORDER BY updated_at DESC
```

**Who logged in from outside the office this week? (planned /sys/audit)**

```qfs
# qfs-cookbook: grammar=core; milestone=M3; features=FROM,WHERE,AND,NOT,LIKE,SELECT,ORDER BY,LIMIT
FROM /sys/audit
|> WHERE action = 'login'
     AND occurred_at > '2026-06-19'
     AND NOT ip LIKE '203.0.113.%'
|> SELECT actor, ip, occurred_at, user_agent
|> ORDER BY occurred_at DESC
|> LIMIT 200
```

**List service connections that have not synced recently. (planned /sys/connections)**

```qfs
# qfs-cookbook: grammar=core; milestone=M5; features=FROM,WHERE,OR,IN,SELECT,ORDER BY
FROM /sys/connections
|> WHERE status IN ('error', 'expired')
     OR last_sync_at < '2026-06-01'
|> SELECT driver, account, status, last_sync_at
|> ORDER BY last_sync_at ASC
```

**Read the current Claude session's standing instructions. (planned /hosts)**

```qfs
# qfs-cookbook: grammar=core; milestone=M7; features=FROM,SELECT,path-nav
FROM /hosts/laptop/claude/sessions/current/instructions
|> SELECT scope, rule, priority, updated_at
```

**Find directory groups whose names match a team prefix. (planned /directories)**

```qfs
# qfs-cookbook: grammar=core; milestone=M5; features=FROM,WHERE,LIKE,AND,SELECT,DISTINCT,ORDER BY
FROM /directories/google/groups
|> WHERE name LIKE 'eng-%'
     AND member_count > 0
|> SELECT name, email, member_count
|> DISTINCT
|> ORDER BY name ASC
```

**List the largest local log files left over from a debugging session.**

```qfs
# qfs-cookbook: grammar=core; milestone=M+; features=FROM,WHERE,AND,LIKE,SELECT,ORDER BY,LIMIT
FROM /fs/var/log
|> WHERE name LIKE '%.log'
     AND size > 10485760
|> SELECT path, size, modified_at
|> ORDER BY size DESC
|> LIMIT 15
```

## Cross-service federation

This is the whole reason qfs exists: one query language that JOINs, UNIONs, and set-subtracts across services that otherwise never speak to each other. Because every service is a path and every path yields rows, a SQL table can JOIN a GitHub issue list, a Slack message log can be reconciled against a git history, and a Drive folder can be diffed against a catalog with EXCEPT. The recipes below stay in today's frozen grammar (`grammar=core`) — the milestone tag reflects which services must be live for the recipe to actually run — and lean on `JOIN … ON`, `UNION`, `EXCEPT`, `INTERSECT`, plus aggregation and filtering on top of the federated result.

**How a mixed-source query resolves — pushed down per source, combined locally, identical on every face.** This is the canonical federation recipe: it documents the execution model (see the roadmap, §4.4), not just the syntax.

```qfs
# qfs-cookbook: grammar=core; milestone=M4; features=FROM,WHERE,JOIN,ON,SELECT,ORDER BY
FROM /sql/pg/orders
|> WHERE status = 'paid' AND placed_at >= '2026-01-01'
|> JOIN /github/acme/support/issues ON orders.email = issues.reporter_email
|> WHERE issues.state = 'open'
|> SELECT orders.id, orders.total, issues.number, issues.title
|> ORDER BY orders.total DESC
```

qfs resolves this in two stages, and the resolution is the **same** whether you run it from the CLI on your laptop, a self-hosted server, or a cloud Worker:

1. **Pushdown per source.** The `/sql/pg/orders` subtree (`WHERE status = 'paid' AND placed_at >= …`) becomes **one SQL query** executed inside Postgres; the `/github/acme/support/issues` subtree becomes a **filtered GitHub API fetch**. Each backend does what it can natively (`qfs describe` shows the **pushdown** line for each).
2. **Local combine.** The cross-source `JOIN … ON orders.email = issues.reporter_email`, the post-join `WHERE issues.state = 'open'`, the `SELECT`, and the `ORDER BY` run **in qfs's own engine, in-process** — only the residual that genuinely spans the two services. The same binary, the same planner, the same combine engine do this on every face; at cloud scale only the tenant→DB routing differs (roadmap §4.2, §4.4) — never the resolution itself.

**Match paid orders to the GitHub issues their customers filed.**

```qfs
# qfs-cookbook: grammar=core; milestone=M4; features=FROM,JOIN,ON,WHERE,SELECT
FROM /sql/pg/orders
|> WHERE status = 'paid'
|> JOIN /github/acme/support/issues ON orders.email = issues.reporter_email
|> SELECT orders.id, orders.total, issues.number, issues.title, issues.state
```

**Find support tickets opened by customers who have never actually purchased.**

```qfs
# qfs-cookbook: grammar=core; milestone=M4; features=FROM,SELECT,EXCEPT
FROM /github/acme/support/issues
|> SELECT reporter_email AS email
|> EXCEPT
   FROM /sql/pg/orders
   |> SELECT email
```

**Tie every merged pull request back to the Slack thread that announced it.**

```qfs
# qfs-cookbook: grammar=core; milestone=M5; features=FROM,WHERE,JOIN,ON,SELECT
FROM /github/acme/web/pulls
|> WHERE state = 'merged'
|> JOIN /slack/acme/eng-releases/messages ON pulls.number = messages.thread_ref
|> SELECT pulls.number, pulls.title, messages.user, messages.text, messages.ts
```

**Reconcile a Drive reports folder against the catalog of reports we expect to exist.**

```qfs
# qfs-cookbook: grammar=core; milestone=M4; features=FROM,SELECT,EXCEPT
FROM /sql/pg/report_catalog
|> SELECT filename
|> EXCEPT
   FROM /drive/Reports
   |> SELECT name AS filename
```

**List S3 inventory keys that are also recorded as live assets in the database.**

```qfs
# qfs-cookbook: grammar=core; milestone=M4; features=FROM,SELECT,INTERSECT
FROM /s3/media-prod
|> SELECT key
|> INTERSECT
   FROM /sql/pg/assets
|> SELECT storage_key AS key
```

**Cross every GA signup session against the SQL users table to confirm conversions.**

```qfs
# qfs-cookbook: grammar=core; milestone=M4; features=FROM,WHERE,JOIN,ON,SELECT
FROM /ga/marketing-site/events
|> WHERE event_name = 'sign_up'
|> JOIN /sql/pg/users ON events.user_id = users.external_id
|> SELECT events.session_id, events.source, users.id, users.plan, users.created_at
```

**Map git commits to the GitHub pull requests that introduced them.**

```qfs
# qfs-cookbook: grammar=core; milestone=M5; features=FROM,JOIN,ON,SELECT,ORDER BY
FROM /git/app/commits
|> JOIN /github/acme/app/pulls ON commits.sha = pulls.merge_commit_sha
|> SELECT commits.sha, commits.author, commits.message, pulls.number, pulls.title
|> ORDER BY commits.committed_at
```

**Surface inbound customer emails that match an open SQL support case.**

```qfs
# qfs-cookbook: grammar=core; milestone=M4; features=FROM,JOIN,ON,WHERE,SELECT
FROM /mail/inbox
|> JOIN /sql/pg/cases ON inbox.from = cases.customer_email
|> WHERE cases.status = 'open'
|> SELECT inbox.from, inbox.subject, cases.id, cases.priority, cases.assignee
```

**Find catalog SKUs that have no corresponding image in the S3 media bucket.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M4; features=FROM,SELECT,||,EXCEPT
FROM /sql/pg/products
|> SELECT 'products/' || sku || '.jpg' AS key
|> EXCEPT
   FROM /s3/media-prod
|> SELECT key
```

**Show high-value orders alongside the GitHub issue and its assigned engineer.**

```qfs
# qfs-cookbook: grammar=core; milestone=M5; features=FROM,WHERE,JOIN,ON,SELECT,ORDER BY
FROM /sql/pg/orders
|> WHERE total > 5000
|> JOIN /github/acme/support/issues ON orders.email = issues.reporter_email
|> JOIN /github/acme/support/issues/assignees ON issues.number = assignees.issue_number
|> SELECT orders.id, orders.total, issues.number, assignees.login
|> ORDER BY orders.total DESC
```

**Count, per repository, how many merged PRs were ever discussed in Slack.**

```qfs
# qfs-cookbook: grammar=core; milestone=M5; features=FROM,WHERE,JOIN,ON,AGGREGATE,GROUP BY
FROM /github/acme/web/pulls
|> WHERE state = 'merged'
|> JOIN /slack/acme/eng-releases/messages ON pulls.number = messages.thread_ref
|> AGGREGATE count() AS discussed_prs
|> GROUP BY pulls.base_repo
```

**Build a churn-risk list: paying customers who filed an issue but sent no email follow-up.**

```qfs
# qfs-cookbook: grammar=core; milestone=M5; features=FROM,WHERE,JOIN,ON,SELECT,EXCEPT
FROM /sql/pg/customers
|> WHERE plan = 'enterprise'
|> JOIN /github/acme/support/issues ON customers.email = issues.reporter_email
|> SELECT customers.email
|> EXCEPT
   FROM /mail/sent
|> SELECT to AS email
```

**Combine every channel a contact reached us through into one unified touch log.**

```qfs
# qfs-cookbook: grammar=core; milestone=M5; features=FROM,SELECT,UNION,ORDER BY
FROM /mail/inbox
|> SELECT from AS contact, 'email' AS channel, subject AS detail, received_at AS at
|> UNION
   FROM /slack/acme/support/messages
|> SELECT user AS contact, 'slack' AS channel, text AS detail, ts AS at
|> UNION
   FROM /github/acme/support/issues
|> SELECT reporter_email AS contact, 'github' AS channel, title AS detail, created_at AS at
|> ORDER BY at DESC
```

**Diff a git-tracked schema file against the live database's expected tables.**

```qfs
# qfs-cookbook: grammar=core; milestone=M4; features=FROM,DECODE,SELECT,EXCEPT
FROM /git/infra@main/schema/tables.yaml
|> DECODE yaml
|> SELECT name
|> EXCEPT
   FROM /sql/pg/information_schema_tables
|> SELECT table_name AS name
```

**Pair Drive contract PDFs with the matching customer record in SQL.**

```qfs
# qfs-cookbook: grammar=core; milestone=M5; features=FROM,JOIN,ON,WHERE,SELECT
FROM /drive/Contracts
|> JOIN /sql/pg/customers ON files.customer_id = customers.id
|> WHERE customers.status = 'active'
|> SELECT files.name, files.modified_at, customers.legal_name, customers.account_manager
```

**Find Slack messages that reference a commit SHA present in the repo history.**

```qfs
# qfs-cookbook: grammar=core; milestone=M5; features=FROM,JOIN,ON,SELECT
FROM /slack/acme/incidents/messages
|> JOIN /git/app/commits ON messages.commit_ref = commits.sha
|> SELECT messages.ts, messages.user, commits.sha, commits.author, commits.message
```

**Aggregate GA revenue by campaign, enriched with the SQL plan each user bought.**

```qfs
# qfs-cookbook: grammar=core; milestone=M5; features=FROM,JOIN,ON,WHERE,AGGREGATE,GROUP BY
FROM /ga/marketing-site/events
|> JOIN /sql/pg/users ON events.user_id = users.external_id
|> WHERE events.event_name = 'purchase'
|> AGGREGATE sum(users.mrr) AS total_mrr, count() AS conversions
|> GROUP BY events.campaign
```

**List R2 backup keys that exist in storage but are absent from the backup ledger.**

```qfs
# qfs-cookbook: grammar=core; milestone=M4; features=FROM,SELECT,EXCEPT
FROM /r2/backups
|> SELECT key
|> EXCEPT
   FROM /sql/pg/backup_ledger
|> SELECT object_key AS key
```

**Three-way join: order → GitHub issue → the Slack escalation thread.**

```qfs
# qfs-cookbook: grammar=core; milestone=M5; features=FROM,JOIN,ON,WHERE,SELECT
FROM /sql/pg/orders
|> JOIN /github/acme/support/issues ON orders.email = issues.reporter_email
|> JOIN /slack/acme/escalations/messages ON issues.number = messages.thread_ref
|> WHERE issues.state = 'open'
|> SELECT orders.id, issues.number, messages.user, messages.text
```

**Identify users active in GA who have no row in the SQL users table (tracking leak).**

```qfs
# qfs-cookbook: grammar=extended; milestone=M4; features=FROM,SELECT,DISTINCT,EXCEPT
FROM /ga/app/events
|> SELECT DISTINCT user_id
|> EXCEPT
   FROM /sql/pg/users
|> SELECT external_id AS user_id
```

**Cross-reference open PRs with the on-call engineer from the directory groups.**

```qfs
# qfs-cookbook: grammar=core; milestone=M5; features=FROM,WHERE,JOIN,ON,SELECT
FROM /github/acme/web/pulls
|> WHERE state = 'open'
|> JOIN /directories/google/groups ON pulls.assignee_email = groups.member_email
|> WHERE groups.name = 'oncall-web'
|> SELECT pulls.number, pulls.title, pulls.assignee_email, pulls.updated_at
```

**Reconcile invoices in SQL against the receipts archived in Drive.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M4; features=FROM,SELECT,||,EXCEPT
FROM /sql/pg/invoices
|> SELECT 'invoice-' || number || '.pdf' AS name
|> EXCEPT
   FROM /drive/Finance/Receipts
|> SELECT name
```

**Total Slack support volume per customer tier by joining channel logs to SQL accounts.**

```qfs
# qfs-cookbook: grammar=core; milestone=M5; features=FROM,JOIN,ON,AGGREGATE,GROUP BY,ORDER BY
FROM /slack/acme/support/messages
|> JOIN /sql/pg/accounts ON messages.user_email = accounts.contact_email
|> AGGREGATE count() AS message_count
|> GROUP BY accounts.tier
|> ORDER BY message_count DESC
```

**Find commits authored by people no longer in the directory (offboarding audit).**

```qfs
# qfs-cookbook: grammar=extended; milestone=M5; features=FROM,SELECT,DISTINCT,EXCEPT
FROM /git/app/commits
|> SELECT DISTINCT author_email AS email
|> EXCEPT
   FROM /directories/entra/users
|> SELECT mail AS email
```

**Join inbound mail to GitHub issues and Slack mentions for a full per-customer thread.**

```qfs
# qfs-cookbook: grammar=core; milestone=M5; features=FROM,JOIN,ON,WHERE,SELECT,ORDER BY
FROM /mail/inbox
|> JOIN /sql/pg/customers ON inbox.from = customers.email
|> JOIN /github/acme/support/issues ON customers.email = issues.reporter_email
|> WHERE issues.state = 'open'
|> SELECT customers.legal_name, inbox.subject, issues.number, issues.title
|> ORDER BY issues.created_at DESC
```

**Keys that appear in BOTH the prod and DR buckets (verified replication set).**

```qfs
# qfs-cookbook: grammar=core; milestone=M4; features=FROM,SELECT,INTERSECT
FROM /s3/media-prod
|> SELECT key
|> INTERSECT
   FROM /s3/media-dr
|> SELECT key
```

**Rank campaigns by signups that became paying orders, joining GA → users → orders.**

```qfs
# qfs-cookbook: grammar=core; milestone=M5; features=FROM,JOIN,ON,WHERE,AGGREGATE,GROUP BY,ORDER BY
FROM /ga/marketing-site/events
|> WHERE event_name = 'sign_up'
|> JOIN /sql/pg/users ON events.user_id = users.external_id
|> JOIN /sql/pg/orders ON users.id = orders.user_id
|> WHERE orders.status = 'paid'
|> AGGREGATE sum(orders.total) AS revenue, count() AS paying_signups
|> GROUP BY events.campaign
|> ORDER BY revenue DESC
```

**Union the day's deploy signals from git refs and Slack release posts into one feed.**

```qfs
# qfs-cookbook: grammar=core; milestone=M5; features=FROM,WHERE,SELECT,UNION,ORDER BY
FROM /git/app/refs
|> WHERE name LIKE 'release/%'
|> SELECT name AS signal, 'git-tag' AS kind, created_at AS at
|> UNION
   FROM /slack/acme/eng-releases/messages
|> WHERE text LIKE 'Deployed%'
|> SELECT text AS signal, 'slack' AS kind, ts AS at
|> ORDER BY at DESC
```

**Flag database customers who never opened a single GA session (dark accounts).**

```qfs
# qfs-cookbook: grammar=extended; milestone=M5; features=FROM,WHERE,SELECT,EXCEPT
FROM /sql/pg/customers
|> WHERE plan = 'pro'
|> SELECT external_id AS user_id
|> EXCEPT
   FROM /ga/app/events
|> SELECT DISTINCT user_id
```

**Per-repo incident load: join commits to issues labeled 'incident' and count by author.**

```qfs
# qfs-cookbook: grammar=core; milestone=M5; features=FROM,JOIN,ON,WHERE,AGGREGATE,GROUP BY,ORDER BY
FROM /github/acme/app/issues
|> WHERE label = 'incident'
|> JOIN /git/app/commits ON issues.fix_sha = commits.sha
|> AGGREGATE count() AS incident_fixes
|> GROUP BY commits.author
|> ORDER BY incident_fixes DESC
```

## Aggregation & analytics

Once a pipeline can `AGGREGATE … AS …` it stops being a filter and becomes a report. This theme rolls
raw rows into counts, sums, averages and rates — grouped by author, channel, sender, prefix or day —
then sorts and trims them into the shape a human actually reads. Because aggregation is just another
stage, the same `GROUP BY` / `ORDER BY` / `LIMIT` vocabulary works over a SQL table, a Slack channel,
a git history or an S3 bucket, and you can filter *after* aggregating (HAVING-style) by chaining a
second `WHERE`. The richest recipes cross services — joining what GA measured against what the
database recorded — to answer questions no single system can.

**Count active customers per country, busiest first.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M4; features=FROM,WHERE,AGGREGATE,GROUP BY,ORDER BY
FROM /sql/pg/customers
|> WHERE status = 'active'
|> AGGREGATE count() AS customers GROUP BY country
|> ORDER BY customers DESC
```

**Total revenue and average order value by month for the current year.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M4; features=FROM,WHERE,EXTEND,AGGREGATE,GROUP BY,ORDER BY
FROM /sql/pg/orders
|> WHERE placed_at >= '2026-01-01'
|> EXTEND month = substr(placed_at, 1, 7)
|> AGGREGATE sum(total) AS revenue, avg(total) AS aov, count() AS orders GROUP BY month
|> ORDER BY month
```

**Show only the product categories that sold more than 1,000 units (HAVING-style filter).**

```qfs
# qfs-cookbook: grammar=extended; milestone=M4; features=FROM,AGGREGATE,GROUP BY,WHERE,ORDER BY
FROM /sql/pg/order_lines
|> AGGREGATE sum(qty) AS units, sum(qty * unit_price) AS gross GROUP BY category
|> WHERE units > 1000
|> ORDER BY gross DESC
```

**List the distinct payment methods customers actually used last quarter.**

```qfs
# qfs-cookbook: grammar=core; milestone=M4; features=FROM,WHERE,SELECT,DISTINCT
FROM /sql/pg/orders
|> WHERE placed_at BETWEEN '2026-04-01' AND '2026-06-30'
|> SELECT payment_method
|> DISTINCT
```

**Find the top 10 customers by lifetime spend.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M4; features=FROM,AGGREGATE,GROUP BY,ORDER BY,LIMIT
FROM /sql/pg/orders
|> AGGREGATE sum(total) AS lifetime_value, count() AS orders GROUP BY customer_id
|> ORDER BY lifetime_value DESC
|> LIMIT 10
```

**Rank GA landing pages by sessions for last week.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M4; features=FROM,WHERE,AGGREGATE,GROUP BY,ORDER BY,LIMIT
FROM /ga/www.acme.com/events
|> WHERE date BETWEEN '2026-06-15' AND '2026-06-21'
|> AGGREGATE sum(sessions) AS sessions, sum(conversions) AS conversions GROUP BY landing_page
|> ORDER BY sessions DESC
|> LIMIT 25
```

**Compute conversion rate per acquisition channel, best-converting first.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M4; features=FROM,WHERE,AGGREGATE,GROUP BY,EXTEND,ORDER BY
FROM /ga/www.acme.com/events
|> WHERE date >= '2026-06-01'
|> AGGREGATE sum(sessions) AS sessions, sum(conversions) AS conversions GROUP BY channel
|> EXTEND conversion_rate = conversions / sessions
|> ORDER BY conversion_rate DESC
```

**Surface only the GA channels that drove fewer than 1% conversion (needs attention).**

```qfs
# qfs-cookbook: grammar=extended; milestone=M4; features=FROM,AGGREGATE,GROUP BY,EXTEND,WHERE,ORDER BY
FROM /ga/www.acme.com/events
|> AGGREGATE sum(sessions) AS sessions, sum(conversions) AS conversions GROUP BY channel
|> EXTEND conversion_rate = conversions / sessions
|> WHERE conversion_rate < 0.01 AND sessions > 500
|> ORDER BY sessions DESC
```

**Revenue by acquisition channel — join GA sessions to SQL order totals (cross-service rollup).**

```qfs
# qfs-cookbook: grammar=extended; milestone=M5; features=FROM,JOIN,ON,AGGREGATE,GROUP BY,ORDER BY
FROM /sql/pg/orders
|> JOIN /ga/www.acme.com/attribution ON orders.session_id = attribution.session_id
|> AGGREGATE sum(orders.total) AS revenue, count() AS orders GROUP BY attribution.channel
|> ORDER BY revenue DESC
```

**Cost per acquisition by campaign — GA spend against orders booked.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M5; features=FROM,JOIN,ON,AGGREGATE,GROUP BY,EXTEND,ORDER BY
FROM /ga/www.acme.com/campaigns
|> JOIN /sql/pg/orders ON campaigns.campaign_id = orders.campaign_id
|> AGGREGATE sum(campaigns.cost) AS spend, count() AS conversions GROUP BY campaigns.campaign_id
|> EXTEND cpa = spend / conversions
|> ORDER BY cpa
```

**Count merged pull requests per author over the last 90 days (PR throughput).**

```qfs
# qfs-cookbook: grammar=extended; milestone=M2; features=FROM,WHERE,AGGREGATE,GROUP BY,ORDER BY
FROM /github/acme/web/pulls
|> WHERE state = 'merged' AND merged_at >= '2026-03-28'
|> AGGREGATE count() AS merged_prs GROUP BY author
|> ORDER BY merged_prs DESC
```

**Median-ish review latency per reviewer — average hours from first review request to approval.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M2; features=FROM,WHERE,EXTEND,AGGREGATE,GROUP BY,ORDER BY
FROM /github/acme/web/reviews
|> WHERE submitted_at >= '2026-05-01' AND state = 'approved'
|> EXTEND latency_hours = (submitted_at - requested_at) / 3600
|> AGGREGATE avg(latency_hours) AS avg_latency, count() AS reviews GROUP BY reviewer
|> ORDER BY avg_latency DESC
```

**PR throughput by week across the whole repo.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M2; features=FROM,WHERE,EXTEND,AGGREGATE,GROUP BY,ORDER BY
FROM /github/acme/web/pulls
|> WHERE state = 'merged' AND merged_at >= '2026-01-01'
|> EXTEND week = substr(merged_at, 1, 10)
|> AGGREGATE count() AS merged, avg(additions + deletions) AS avg_churn GROUP BY week
|> ORDER BY week
```

**Find reviewers carrying a backlog — more than 15 reviews requested but not yet submitted.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M2; features=FROM,WHERE,AGGREGATE,GROUP BY,ORDER BY
FROM /github/acme/web/review_requests
|> WHERE submitted_at IS NULL
|> AGGREGATE count() AS pending GROUP BY requested_reviewer
|> WHERE pending > 15
|> ORDER BY pending DESC
```

**Issue churn: count opened vs closed issues per label this quarter.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M2; features=FROM,WHERE,AGGREGATE,GROUP BY,ORDER BY
FROM /github/acme/web/issues
|> WHERE created_at >= '2026-04-01'
|> AGGREGATE count() AS opened, sum(closed_at IS NOT NULL) AS closed GROUP BY label
|> ORDER BY opened DESC
```

**Message volume per Slack channel over the past 30 days.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M2; features=FROM,WHERE,AGGREGATE,GROUP BY,ORDER BY,LIMIT
FROM /slack/acme/engineering/messages
|> WHERE ts >= '2026-05-27'
|> AGGREGATE count() AS messages, count(thread_ts) AS thread_replies GROUP BY channel
|> ORDER BY messages DESC
|> LIMIT 20
```

**Who posts most in the support channel — top 15 chattiest members.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M2; features=FROM,WHERE,AGGREGATE,GROUP BY,ORDER BY,LIMIT
FROM /slack/acme/support/messages
|> WHERE ts >= '2026-06-01'
|> AGGREGATE count() AS messages GROUP BY user
|> ORDER BY messages DESC
|> LIMIT 15
```

**Slack activity by hour of day to find the team's quiet window.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M2; features=FROM,WHERE,EXTEND,AGGREGATE,GROUP BY,ORDER BY
FROM /slack/acme/general/messages
|> WHERE ts >= '2026-05-01'
|> EXTEND hour = substr(ts, 12, 2)
|> AGGREGATE count() AS messages, count(DISTINCT user) AS active_users GROUP BY hour
|> ORDER BY hour
```

**Rank senders by how many emails they sent me this month (sender frequency).**

```qfs
# qfs-cookbook: grammar=extended; milestone=M1; features=FROM,WHERE,AGGREGATE,GROUP BY,ORDER BY,LIMIT
FROM /mail/inbox
|> WHERE received_at >= '2026-06-01'
|> AGGREGATE count() AS messages, sum(is_unread) AS unread GROUP BY from_address
|> ORDER BY messages DESC
|> LIMIT 25
```

**Find noisy senders — anyone who sent more than 20 unread emails this quarter.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M1; features=FROM,WHERE,AGGREGATE,GROUP BY,ORDER BY
FROM /mail/inbox
|> WHERE received_at >= '2026-04-01' AND is_unread = true
|> AGGREGATE count() AS unread_count GROUP BY from_address
|> WHERE unread_count > 20
|> ORDER BY unread_count DESC
```

**Email volume by domain — group senders by their organization.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M1; features=FROM,WHERE,EXTEND,AGGREGATE,GROUP BY,ORDER BY
FROM /mail/inbox
|> WHERE received_at >= '2026-05-01'
|> EXTEND domain = substr(from_address, instr(from_address, '@') + 1)
|> AGGREGATE count() AS messages, count(DISTINCT from_address) AS senders GROUP BY domain
|> ORDER BY messages DESC
```

**Commits per author in the main repo over the last year (git contribution leaderboard).**

```qfs
# qfs-cookbook: grammar=extended; milestone=M2; features=FROM,WHERE,AGGREGATE,GROUP BY,ORDER BY
FROM /git/app/commits
|> WHERE committed_at >= '2025-06-26'
|> AGGREGATE count() AS commits, sum(additions) AS lines_added, sum(deletions) AS lines_removed GROUP BY author_email
|> ORDER BY commits DESC
```

**Commit cadence by month to visualize project momentum.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M2; features=FROM,WHERE,EXTEND,AGGREGATE,GROUP BY,ORDER BY
FROM /git/app/commits
|> WHERE committed_at >= '2025-01-01'
|> EXTEND month = substr(committed_at, 1, 7)
|> AGGREGATE count() AS commits, count(DISTINCT author_email) AS contributors GROUP BY month
|> ORDER BY month
```

**Bus-factor check: which files were touched by only one author.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M2; features=FROM,AGGREGATE,GROUP BY,WHERE,ORDER BY
FROM /git/app/commits
|> AGGREGATE count(DISTINCT author_email) AS authors, count() AS changes GROUP BY path
|> WHERE authors = 1 AND changes > 10
|> ORDER BY changes DESC
```

**Storage footprint by top-level prefix in an S3 bucket.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M4; features=FROM,EXTEND,AGGREGATE,GROUP BY,ORDER BY
FROM /s3/acme-data-lake
|> EXTEND prefix = substr(key, 1, instr(key, '/') - 1)
|> AGGREGATE sum(size) AS total_bytes, count() AS objects GROUP BY prefix
|> ORDER BY total_bytes DESC
```

**Find S3 prefixes hoarding storage — more than 50 GB sitting in one place.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M4; features=FROM,EXTEND,AGGREGATE,GROUP BY,WHERE,ORDER BY
FROM /s3/acme-data-lake
|> EXTEND prefix = substr(key, 1, instr(key, '/') - 1)
|> AGGREGATE sum(size) AS total_bytes, count() AS objects GROUP BY prefix
|> WHERE total_bytes > 53687091200
|> ORDER BY total_bytes DESC
```

**Storage growth by upload month to project next quarter's bill.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M4; features=FROM,WHERE,EXTEND,AGGREGATE,GROUP BY,ORDER BY
FROM /s3/acme-backups
|> WHERE last_modified >= '2026-01-01'
|> EXTEND month = substr(last_modified, 1, 7)
|> AGGREGATE sum(size) AS bytes_added, count() AS objects GROUP BY month
|> ORDER BY month
```

**Distinct storage classes in use across the bucket.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M4; features=FROM,AGGREGATE,GROUP BY,ORDER BY
FROM /s3/acme-data-lake
|> AGGREGATE count() AS objects, sum(size) AS bytes GROUP BY storage_class
|> ORDER BY bytes DESC
```

**Daily signups joined to first-week revenue — cohort value from SQL plus GA (cross-service).**

```qfs
# qfs-cookbook: grammar=extended; milestone=M5; features=FROM,JOIN,ON,WHERE,AGGREGATE,GROUP BY,ORDER BY
FROM /sql/pg/customers
|> JOIN /ga/www.acme.com/signups ON customers.email = signups.email
|> WHERE customers.created_at >= '2026-01-01'
|> AGGREGATE count() AS signups, sum(customers.first_week_spend) AS cohort_revenue GROUP BY signups.acquisition_source
|> ORDER BY cohort_revenue DESC
```

**Engineering health snapshot: average PR size by author, only those above 400 lines.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M2; features=FROM,WHERE,EXTEND,AGGREGATE,GROUP BY,ORDER BY
FROM /github/acme/web/pulls
|> WHERE state = 'merged' AND merged_at >= '2026-01-01'
|> EXTEND size = additions + deletions
|> AGGREGATE avg(size) AS avg_pr_size, count() AS prs GROUP BY author
|> WHERE avg_pr_size > 400
|> ORDER BY avg_pr_size DESC
```

## Codecs & blob↔relational

A blob is just bytes until you `DECODE` it; then it is a relation you can filter, join and aggregate like any table. `ENCODE` runs the bridge the other way — a relation becomes `json`, `csv`, `yaml`, `toml`, `jsonl` or `md` bytes you can `UPSERT` back into any blob sink. The codec is independent of the source, so the same `DECODE md` works on a local file, an S3 object, a Drive doc, a git file pinned at a ref, GitHub file contents or the body of a REST/JSON response. For markdown, `DECODE md` lifts each document's YAML frontmatter keys into columns and exposes the prose as a `body` column, which turns a folder of notes into a queryable table.

**List every front-matter field of a local design note as a one-row table.**

```qfs
# qfs-cookbook: grammar=core; milestone=M2; features=FROM,DECODE,SELECT
# md frontmatter keys become columns; body holds the prose
FROM /local/docs/design/auth-rework.md
|> DECODE md
|> SELECT title, status, owner, body
```

**Find every workaholic ticket still in todo, newest first.**

```qfs
# qfs-cookbook: grammar=core; milestone=M2; features=FROM,DECODE,WHERE,ORDER BY,SELECT
# .workaholic/**/*.md read as one table, frontmatter as columns
FROM /local/.workaholic/tickets/todo/*.md
|> DECODE md
|> WHERE status = 'todo'
|> ORDER BY created_at DESC
|> SELECT id, title, severity, created_at
```

**Roll up open tickets by severity across the whole ticket tree.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M2; features=FROM,DECODE,WHERE,AGGREGATE,GROUP BY,ORDER BY
FROM /local/.workaholic/**/*.md
|> DECODE md
|> WHERE status <> 'done'
|> AGGREGATE count() AS open_tickets GROUP BY severity
|> ORDER BY open_tickets DESC
```

**Pull a config file out of git at a tagged release and read its keys.**

```qfs
# qfs-cookbook: grammar=core; milestone=M2; features=FROM,DECODE,SELECT
# @version pins the blob to a ref before decoding
FROM /git/app@v1.4.0/config/settings.toml
|> DECODE toml
|> SELECT region, max_connections, feature_flags
```

**Diff a YAML setting between two releases by reading each AS OF its tag.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M2; features=FROM,DECODE,JOIN,SELECT
# decode the same file at two refs and join on key to compare
FROM /git/infra@v2.0.0/helm/values.yaml
|> DECODE yaml
|> JOIN /git/infra@v1.0.0/helm/values.yaml |> DECODE yaml ON replicas
|> SELECT replicas
```

**Read a REST/JSON webhook payload and explode its line items into rows.**

```qfs
# qfs-cookbook: grammar=core; milestone=M9; features=FROM,DECODE,EXPAND,SELECT
# the response body is a blob; decode json then expand the array column
FROM /local/incoming/stripe-event.json
|> DECODE json
|> EXPAND data.object.lines
|> SELECT data.object.id AS invoice, lines.description, lines.amount
```

**Turn a JSONL export of events into a daily count.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M2; features=FROM,DECODE,EXTEND,AGGREGATE,GROUP BY
# one JSON object per line → one row per line
FROM /s3/analytics-exports/events-2026-06.jsonl
|> DECODE jsonl
|> EXTEND day = substr(ts, 1, 10)
|> AGGREGATE count() AS events GROUP BY day
```

**Read a CSV in a Drive folder and keep only high-value rows.**

```qfs
# qfs-cookbook: grammar=core; milestone=M2; features=FROM,DECODE,WHERE,ORDER BY,SELECT
FROM /drive/Finance/q2-pipeline.csv
|> DECODE csv
|> WHERE stage = 'commit' AND amount > 50000
|> ORDER BY amount DESC
|> SELECT account, amount, owner, close_date
```

**Read GitHub file contents at HEAD and surface the declared package version.**

```qfs
# qfs-cookbook: grammar=core; milestone=M2; features=FROM,DECODE,SELECT
# GitHub blob contents decoded as json, no clone needed
FROM /github/acme/web/contents/package.json
|> DECODE json
|> SELECT name, version, dependencies.react AS react_version
```

**List which tickets each engineer owns from the markdown ticket tree.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M2; features=FROM,DECODE,AGGREGATE,GROUP BY,ORDER BY
FROM /local/.workaholic/tickets/**/*.md
|> DECODE md
|> AGGREGATE count() AS tickets GROUP BY owner
|> ORDER BY tickets DESC
```

**Export open tickets to a shared CSV report on Drive (round-trip md→csv).**

```qfs
# qfs-cookbook: grammar=extended; milestone=M2; features=FROM,DECODE,WHERE,SELECT,ENCODE,UPSERT INTO
# DECODE md … transform … ENCODE csv … UPSERT INTO storage
FROM /local/.workaholic/tickets/todo/*.md
|> DECODE md
|> WHERE severity IN ('high', 'critical')
|> SELECT id, title, severity, owner
|> ENCODE csv
|> UPSERT INTO /drive/Reports/open-critical-tickets.csv
```

**Convert a SQL query result into a JSON object dropped in an S3 bucket.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M4; features=FROM,WHERE,SELECT,ENCODE,UPSERT INTO
FROM /sql/pg/customers
|> WHERE plan = 'enterprise' AND churned = false
|> SELECT id, name, mrr, renewal_date
|> ENCODE json
|> UPSERT INTO /s3/exports/enterprise-accounts.json
```

**Normalize a messy CSV and write it back as clean YAML config.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M2; features=FROM,DECODE,EXTEND,SELECT,ENCODE,UPSERT INTO
# csv in, yaml out — codecs decouple input format from output format
FROM /local/import/regions.csv
|> DECODE csv
|> EXTEND code = lower(trim(code))
|> SELECT code, display_name, timezone
|> ENCODE yaml
|> UPSERT INTO /local/config/regions.yaml
```

**Bump a version field in a git-tracked TOML and commit the new blob.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M2; features=FROM,DECODE,SET,ENCODE,UPSERT INTO
# read TOML at a ref, mutate a column, re-encode and write a commit
FROM /git/app@main/Cargo.toml
|> DECODE toml
|> SET package.version = '0.1.0'
|> ENCODE toml
|> UPSERT INTO /git/app@main/Cargo.toml
```

**Promote a draft note to published by flipping its frontmatter and re-encoding.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M2; features=FROM,DECODE,SET,ENCODE,UPSERT INTO
# md round-trip: frontmatter columns change, body is preserved
FROM /local/blog/posts/launch-recap.md
|> DECODE md
|> SET status = 'published', published_at = '2026-06-26'
|> ENCODE md
|> UPSERT INTO /local/blog/posts/launch-recap.md
```

**Cross a markdown ticket table against the SQL users table to validate owners.**

```qfs
# qfs-cookbook: grammar=core; milestone=M5; features=FROM,DECODE,JOIN,WHERE,SELECT
# blob-derived relation joined to a real database table
FROM /local/.workaholic/tickets/todo/*.md
|> DECODE md
|> JOIN /sql/pg/users ON owner = users.username
|> WHERE users.active = false
|> SELECT id, title, owner
```

**Find tickets that reference a feature flag missing from the live config.**

```qfs
# qfs-cookbook: grammar=core; milestone=M2; features=FROM,DECODE,EXCEPT,SELECT
# two decoded blobs compared with set difference
FROM /local/.workaholic/tickets/**/*.md
|> DECODE md
|> SELECT flag
|> EXCEPT
   FROM /git/app@main/config/flags.yaml |> DECODE yaml |> SELECT flag
```

**Read last quarter's pricing TOML from S3 by version id and list tiers.**

```qfs
# qfs-cookbook: grammar=core; milestone=M2; features=FROM,DECODE,ORDER BY,SELECT
# @version on an S3 object pins a specific stored revision
FROM /s3/pricing/tiers.toml@v8c1f2a
|> DECODE toml
|> ORDER BY monthly_price
|> SELECT tier, monthly_price, seat_limit
```

**Audit which markdown docs are missing a required owner field.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M2; features=FROM,DECODE,WHERE,SELECT
FROM /local/docs/**/*.md
|> DECODE md
|> WHERE owner IS NULL OR trim(owner) = ''
|> SELECT title, status
```

**Snapshot the current SQL order book as a JSONL backup in R2.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M4; features=FROM,WHERE,SELECT,ENCODE,UPSERT INTO
# relational → jsonl, one object per line, into Cloudflare R2
FROM /sql/pg/orders
|> WHERE status = 'open'
|> SELECT id, customer_id, total, placed_at
|> ENCODE jsonl
|> UPSERT INTO /r2/backups/open-orders.jsonl
```

**Read an old config AS OF a date from SQL and re-publish it as YAML.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M4; features=FROM,AS OF,SELECT,ENCODE,UPSERT INTO
# temporal read of a relational source, then encode to a blob
FROM /sql/pg/settings AS OF '2026-01-01'
|> SELECT key, value
|> ENCODE yaml
|> UPSERT INTO /drive/Configs/settings-snapshot-jan.yaml
```

**Merge two decoded CSV exports into one deduplicated customer file.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M2; features=FROM,DECODE,UNION,DISTINCT,SELECT,ENCODE,UPSERT INTO
FROM /drive/Imports/eu-customers.csv
|> DECODE csv
|> UNION
   FROM /drive/Imports/us-customers.csv |> DECODE csv
|> DISTINCT
|> SELECT email, name, region
|> ENCODE csv
|> UPSERT INTO /drive/Imports/all-customers.csv
```

**Extract the body of every README in a repo at a tag for a doc index.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M2; features=FROM,DECODE,SELECT,ENCODE,UPSERT INTO
# md bodies harvested from a git ref, written out as a jsonl index
FROM /git/monorepo@v3.0.0/packages/**/README.md
|> DECODE md
|> SELECT title, body
|> ENCODE jsonl
|> UPSERT INTO /local/build/readme-index.jsonl
```

**Read a YAML manifest from GitHub and list services exceeding a CPU budget.**

```qfs
# qfs-cookbook: grammar=core; milestone=M2; features=FROM,DECODE,EXPAND,WHERE,SELECT
FROM /github/acme/infra/contents/manifests/services.yaml
|> DECODE yaml
|> EXPAND services
|> WHERE services.cpu > 4
|> SELECT services.name, services.cpu, services.replicas
```

**Build a slack-ready digest table from the todo ticket markdown.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M2; features=FROM,DECODE,WHERE,ORDER BY,SELECT,ENCODE,UPSERT INTO
# md → json digest dropped as a file a downstream job posts to Slack
FROM /local/.workaholic/tickets/todo/*.md
|> DECODE md
|> WHERE severity = 'critical'
|> ORDER BY created_at
|> SELECT id, title, owner
|> ENCODE json
|> UPSERT INTO /local/build/standup-digest.json
```

**Compare a markdown frontmatter field across two git refs of the same doc.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M2; features=FROM,DECODE,JOIN,WHERE,SELECT
# AS-OF-style read via @version on both sides, joined to spot drift
FROM /git/handbook@v2/policies/security.md
|> DECODE md
|> JOIN /git/handbook@v1/policies/security.md |> DECODE md ON title
|> WHERE status <> handbook_v1.status
|> SELECT title, status
```

**Flatten a nested TOML config into a flat key/value CSV for review.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M2; features=FROM,DECODE,SELECT,ENCODE,UPSERT INTO
FROM /local/config/app.toml
|> DECODE toml
|> SELECT 'database.host' AS key, database.host AS value
|> ENCODE csv
|> UPSERT INTO /local/audit/config-flat.csv
```

## Writes & effects

Reads describe the world; writes change it. qfs spells every mutation as a pipeline stage — `INSERT INTO`, `UPSERT INTO`, `UPDATE`, `REMOVE` — or as a namespaced `CALL` for irreducible state transitions a driver declares (`mail.send`, `github.merge`, `slack.post`). The safety model is what makes this livable: describe is pure, preview touches nothing, `--commit` applies reversible writes, and anything irreversible (sending mail, merging a PR, deleting a blob) refuses to run without `--commit-irreversible`. The recipes below move from the safe and undoable (draft a mail, UPSERT a report) to the one-way doors (send, merge, dispatch CI), and most stay in today's frozen core grammar so they parse now even where the driver ships later.

**Draft a thank-you email to every customer who ordered this week (reversible — writing a draft sends nothing).**

```qfs
# qfs-cookbook: grammar=extended; milestone=M4; features=FROM,WHERE,INSERT INTO,VALUES,||
# A draft is reversible: it lands in /mail/drafts and auto-commits under POLICY.
FROM /sql/pg/orders
|> WHERE created_at >= '2026-06-19'
|> INSERT INTO /mail/drafts
     VALUES (to => customer_email,
             subject => 'Thanks for order #' || order_id,
             body => 'Hi ' || customer_name || ', your order is on its way.')
```

**Send the queued win-back emails (irreversible — needs --commit-irreversible).**

```qfs
# qfs-cookbook: grammar=core; milestone=M4; features=FROM,WHERE,CALL,mail.send
# CALL mail.send is a one-way door; preview shows the recipients, --commit-irreversible actually sends.
FROM /mail/drafts
|> WHERE subject LIKE 'We miss you%'
|> CALL mail.send(to => to, subject => subject, body => body)
```

**Reply to every unanswered support thread with a holding message (irreversible send).**

```qfs
# qfs-cookbook: grammar=extended; milestone=M4; features=FROM,WHERE,EXTEND,CALL,mail.send,||
FROM /mail/inbox
|> WHERE label = 'support' AND answered = false AND received_at < '2026-06-25'
|> EXTEND ack = 'Hi ' || from_name || ', we have your ticket and will reply within 24h.'
|> CALL mail.send(to => from_addr, subject => 'Re: ' || subject, body => ack)
```

**Upsert the nightly sales rollup to a Drive spreadsheet (reversible — overwrites a blob in place).**

```qfs
# qfs-cookbook: grammar=extended; milestone=M4; features=FROM,AGGREGATE,GROUP BY,ENCODE,UPSERT INTO
# UPSERT is retry-safe: re-running it produces the same object, never a duplicate.
FROM /sql/pg/orders
|> WHERE created_at >= '2026-06-01'
|> AGGREGATE sum(amount) AS total, count(*) AS orders GROUP BY region
|> ENCODE csv
|> UPSERT INTO /drive/Reports/june-sales.csv
```

**Publish a rendered report to S3, overwriting any prior copy at the same key (reversible UPSERT).**

```qfs
# qfs-cookbook: grammar=extended; milestone=M2; features=FROM,DECODE,ENCODE,UPSERT INTO
FROM /local/reports/q2-summary.md
|> DECODE md
|> ENCODE json
|> UPSERT INTO /s3/acme-reports/q2/summary.json
```

**Mirror a config blob from Drive into an R2 bucket (idempotent UPSERT, safe to re-run).**

```qfs
# qfs-cookbook: grammar=extended; milestone=M2; features=FROM,UPSERT INTO
FROM /drive/Config/app-settings.toml
|> UPSERT INTO /r2/edge-config/app-settings.toml
```

**Insert a git commit that writes a generated changelog (reversible — a commit can be reverted).**

```qfs
# qfs-cookbook: grammar=extended; milestone=M1; features=FROM,DECODE,SELECT,ENCODE,INSERT INTO,VALUES
# Writing to /git/<repo>/commits records a real commit; history makes it reversible.
FROM /git/app@main/CHANGELOG.md
|> DECODE md
|> INSERT INTO /git/app/commits
     VALUES (message => 'docs: regenerate changelog',
             branch => 'main',
             path => 'CHANGELOG.md',
             body => body)
```

**Stage a new source file as a commit on a feature branch (reversible).**

```qfs
# qfs-cookbook: grammar=extended; milestone=M1; features=FROM,INSERT INTO,VALUES,||
FROM /local/src/handler.rs
|> INSERT INTO /git/app/commits
     VALUES (message => 'feat: add request handler',
             branch => 'feat/handler',
             path => 'src/handler.rs',
             body => body)
```

**Squash-merge an approved pull request (irreversible — needs --commit-irreversible).**

```qfs
# qfs-cookbook: grammar=core; milestone=M1; features=FROM,WHERE,CALL,github.merge
# Merging is a one-way door: preview shows which PR, --commit-irreversible performs the squash.
FROM /github/acme/web/pulls
|> WHERE number = 42 AND state = 'open' AND mergeable = true
|> CALL github.merge(number => number, method => 'squash')
```

**Auto-merge every approved Dependabot PR with a passing build (irreversible bulk merge).**

```qfs
# qfs-cookbook: grammar=core; milestone=M1; features=FROM,WHERE,CALL,github.merge,AND
FROM /github/acme/web/pulls
|> WHERE author = 'dependabot[bot]' AND review_decision = 'APPROVED' AND checks_status = 'success'
|> CALL github.merge(number => number, method => 'squash')
```

**Comment on every PR that has been waiting on review for over three days (irreversible — posts publicly).**

```qfs
# qfs-cookbook: grammar=extended; milestone=M1; features=FROM,WHERE,CALL,github.comment,||
FROM /github/acme/web/pulls
|> WHERE state = 'open' AND review_decision <> 'APPROVED' AND created_at < '2026-06-23'
|> CALL github.comment(number => number,
                       body => 'Friendly nudge — this PR by @' || author || ' is awaiting review.')
```

**Comment a build-failure summary on the PR that broke CI (irreversible comment).**

```qfs
# qfs-cookbook: grammar=extended; milestone=M1; features=FROM,WHERE,CALL,github.comment,||
FROM /github/acme/web/pulls
|> WHERE number = 87 AND checks_status = 'failure'
|> CALL github.comment(number => number,
                       body => 'CI failed on ' || head_sha || ': see the run logs for the failing job.')
```

**Post a release announcement to the team Slack channel (irreversible — message goes out).**

```qfs
# qfs-cookbook: grammar=extended; milestone=M1; features=FROM,WHERE,CALL,slack.post,||
FROM /github/acme/web/releases
|> WHERE published_at >= '2026-06-26'
|> CALL slack.post(channel => '#releases',
                   text => ':rocket: ' || tag_name || ' is live — ' || html_url)
```

**Cross-post overnight production errors to the on-call Slack channel (irreversible).**

```qfs
# qfs-cookbook: grammar=extended; milestone=M3; features=FROM,WHERE,AGGREGATE,GROUP BY,CALL,slack.post,||
# /sys/audit is a planned mount but still parses as core today (a path is just a token).
FROM /sys/audit
|> WHERE level = 'error' AND ts >= '2026-06-26T00:00:00Z'
|> AGGREGATE count(*) AS hits GROUP BY service
|> CALL slack.post(channel => '#oncall',
                   text => service || ' threw ' || hits || ' errors overnight')
```

**Append a daily standup summary as a Slack message (irreversible post).**

```qfs
# qfs-cookbook: grammar=extended; milestone=M1; features=FROM,WHERE,AGGREGATE,GROUP BY,CALL,slack.post,||
FROM /github/acme/web/commits
|> WHERE committed_at >= '2026-06-26'
|> AGGREGATE count(*) AS commits GROUP BY author
|> CALL slack.post(channel => '#standup', text => author || ' shipped ' || commits || ' commits today')
```

**Mark every shipped order as fulfilled and return the affected rows (reversible UPDATE).**

```qfs
# qfs-cookbook: grammar=extended; milestone=M4; features=FROM,WHERE,UPDATE,SET,RETURNING
# RETURNING hands back what changed so you can chain or audit it.
FROM /sql/pg/orders
|> WHERE status = 'shipped' AND tracking_number <> ''
|> UPDATE SET status = 'fulfilled', fulfilled_at = now()
|> RETURNING order_id, customer_email, fulfilled_at
```

**Apply a 10% loyalty discount to repeat customers' open carts (reversible UPDATE).**

```qfs
# qfs-cookbook: grammar=extended; milestone=M4; features=FROM,WHERE,UPDATE,SET,RETURNING
FROM /sql/pg/carts
|> WHERE status = 'open' AND customer_order_count >= 5
|> UPDATE SET discount_pct = 10, updated_at = now()
|> RETURNING cart_id, customer_id, discount_pct
```

**Deactivate accounts that never confirmed their email after 30 days (reversible flag flip).**

```qfs
# qfs-cookbook: grammar=extended; milestone=M4; features=FROM,WHERE,UPDATE,SET,AND,RETURNING
FROM /sql/pg/accounts
|> WHERE email_confirmed = false AND created_at < '2026-05-27'
|> UPDATE SET status = 'inactive', deactivated_at = now()
|> RETURNING account_id, email
```

**Normalize country codes on legacy customer records (reversible UPDATE with RETURNING for the audit trail).**

```qfs
# qfs-cookbook: grammar=extended; milestone=M4; features=FROM,WHERE,UPDATE,SET,RETURNING
FROM /sql/pg/customers
|> WHERE country = 'USA'
|> UPDATE SET country = 'US'
|> RETURNING customer_id, country
```

**Confirm and return the inventory rows decremented for a shipment (reversible UPDATE).**

```qfs
# qfs-cookbook: grammar=extended; milestone=M4; features=FROM,WHERE,UPDATE,SET,RETURNING
FROM /sql/pg/inventory
|> WHERE sku IN ('A-101', 'A-102', 'B-200') AND on_hand > 0
|> UPDATE SET on_hand = on_hand - 1, last_picked_at = now()
|> RETURNING sku, on_hand
```

**Remove webhook delivery logs older than 90 days (irreversible — DELETE needs --commit-irreversible).**

```qfs
# qfs-cookbook: grammar=extended; milestone=M4; features=FROM,WHERE,REMOVE
# REMOVE is a delete: preview shows the count, --commit-irreversible actually purges the rows.
FROM /sql/pg/webhook_logs
|> WHERE delivered_at < '2026-03-28'
|> REMOVE
```

**Purge expired temporary export blobs from S3 (irreversible delete).**

```qfs
# qfs-cookbook: grammar=extended; milestone=M2; features=FROM,WHERE,REMOVE,LIKE
FROM /s3/acme-exports/tmp
|> WHERE key LIKE 'tmp/%' AND last_modified < '2026-06-19'
|> REMOVE
```

**Delete duplicate draft emails left over from a failed batch (irreversible — clears drafts).**

```qfs
# qfs-cookbook: grammar=extended; milestone=M4; features=FROM,WHERE,REMOVE,LIKE,AND
FROM /mail/drafts
|> WHERE subject LIKE 'AUTO:%' AND created_at < '2026-06-25'
|> REMOVE
```

**Evict stale objects from a KV namespace (irreversible delete).**

```qfs
# qfs-cookbook: grammar=extended; milestone=M2; features=FROM,WHERE,REMOVE
FROM /kv/sessions
|> WHERE expires_at < '2026-06-26T00:00:00Z'
|> REMOVE
```

**Dispatch the deploy workflow for the latest green commit on main (irreversible — triggers CI).**

```qfs
# qfs-cookbook: grammar=core; milestone=M1; features=FROM,WHERE,ORDER BY,LIMIT,CALL,ci.dispatch
# ci.dispatch starts a real run: preview names the workflow and ref, --commit-irreversible fires it.
FROM /github/acme/web/commits
|> WHERE branch = 'main' AND checks_status = 'success'
|> ORDER BY committed_at DESC
|> LIMIT 1
|> CALL ci.dispatch(workflow => 'deploy.yml', ref => sha)
```

**Re-run the nightly ETL workflow for every failed run from last night (irreversible dispatch).**

```qfs
# qfs-cookbook: grammar=core; milestone=M1; features=FROM,WHERE,CALL,ci.dispatch
FROM /github/acme/data/runs
|> WHERE workflow = 'etl.yml' AND conclusion = 'failure' AND created_at >= '2026-06-25'
|> CALL ci.dispatch(workflow => 'etl.yml', ref => head_branch)
```

**Snapshot a production object to a dated archive key before edits (reversible — server-side copy).**

```qfs
# qfs-cookbook: grammar=extended; milestone=M2; features=FROM,WHERE,CALL,s3.copy,||
FROM /s3/acme-prod/config.json
|> WHERE size > 0
|> CALL s3.copy(dest => '/s3/acme-archive/config-' || version_id || '.json')
```

**Insert audit notes into a SQL table from a CSV upload (reversible bulk INSERT).**

```qfs
# qfs-cookbook: grammar=extended; milestone=M4; features=FROM,DECODE,INSERT INTO,VALUES
FROM /drive/Imports/access-review.csv
|> DECODE csv
|> INSERT INTO /sql/pg/audit_notes
     VALUES (user_id => user_id, note => note, reviewed_by => reviewer)
```

**Copy approved candidate records into the hires table and return the new ids (reversible INSERT … RETURNING).**

```qfs
# qfs-cookbook: grammar=extended; milestone=M4; features=FROM,WHERE,SELECT,INSERT INTO,VALUES,RETURNING
FROM /sql/pg/candidates
|> WHERE stage = 'offer_accepted'
|> INSERT INTO /sql/pg/hires
     VALUES (name => name, email => email, start_date => offer_start_date)
|> RETURNING hire_id, email
```

**Draft individual offer emails for accepted candidates (reversible draft, sends nothing).**

```qfs
# qfs-cookbook: grammar=extended; milestone=M4; features=FROM,WHERE,INSERT INTO,VALUES,||
FROM /sql/pg/candidates
|> WHERE stage = 'offer_accepted' AND offer_email_sent = false
|> INSERT INTO /mail/drafts
     VALUES (to => email,
             subject => 'Welcome aboard, ' || name || '!',
             body => 'We are thrilled to have you start on ' || offer_start_date || '.')
```

**Open a GitHub issue for every flaky test reported overnight (reversible — issues can be closed).**

```qfs
# qfs-cookbook: grammar=extended; milestone=M1; features=FROM,WHERE,INSERT INTO,VALUES,||
FROM /sql/pg/test_failures
|> WHERE flaky = true AND last_seen >= '2026-06-26'
|> INSERT INTO /github/acme/web/issues
     VALUES (title => 'Flaky test: ' || test_name,
             body => 'Failed ' || fail_count || ' times overnight.',
             labels => 'flaky,test')
```

**Tag a release by writing a new ref pointer in git (reversible — refs are mutable pointers).**

```qfs
# qfs-cookbook: grammar=extended; milestone=M1; features=FROM,WHERE,ORDER BY,LIMIT,INSERT INTO,VALUES
FROM /git/app/commits
|> WHERE branch = 'main'
|> ORDER BY committed_at DESC
|> LIMIT 1
|> INSERT INTO /git/app/refs
     VALUES (name => 'refs/tags/v1.4.0', target => sha)
```

**Update Slack channel topics from a SQL config table (reversible — topic edits revert).**

```qfs
# qfs-cookbook: grammar=extended; milestone=M1; features=FROM,WHERE,UPDATE,SET,RETURNING
FROM /slack/acme/general/channel
|> WHERE id = 'C12345'
|> UPDATE SET topic = 'Q3 planning in progress'
|> RETURNING id, topic
```

## LET & the functional core

qfs gains a small functional core in M6: `LET` binds an intermediate relation or value so you can reference it more than once without recomputing or re-typing it; lambdas `(x: Type) => expr` are first-class values; and the higher-order builtins `map`, `filter`, and `reduce` take those lambdas to transform columns inline. A user-defined function is just a `LET`-bound lambda — there is no separate function namespace to learn. These features compose cleanly with everything from the earlier themes: a `LET` can hold a subquery you join back against, a lambda can normalize a key before a `GROUP BY`, and the whole thing can still end in a write. Anything that uses `LET` or a lambda arrow `=>` parses only on the extended grammar, so the blocks below are tagged accordingly; plain `map`/`filter`/`reduce` calls without a lambda stay core.

**Flag orders that beat the running average of their own product category.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M6; features=LET,FROM,AGGREGATE,GROUP BY,JOIN,WHERE
# Bind the per-category average once, then compare every order back against it.
LET cat_avg =
  FROM /sql/pg/orders
  |> AGGREGATE avg(amount) AS avg_amount GROUP BY category
FROM /sql/pg/orders AS o
|> JOIN cat_avg AS c ON o.category = c.category
|> WHERE o.amount > c.avg_amount
|> SELECT o.id, o.category, o.amount, c.avg_amount
```

**Rank each rep against their team's own average deal size.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M6; features=LET,AGGREGATE,GROUP BY,JOIN,EXTEND
LET team_avg =
  FROM /sql/pg/deals
  |> AGGREGATE avg(value) AS team_avg_value GROUP BY team
FROM /sql/pg/deals AS d
|> JOIN team_avg AS t ON d.team = t.team
|> EXTEND vs_team = d.value - t.team_avg_value
|> SELECT d.rep, d.team, d.value, vs_team
|> ORDER BY vs_team
```

**Reuse one active-customer set for both a count and a revenue total.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M6; features=LET,FROM,WHERE,JOIN,AGGREGATE
# active is referenced twice: once to scope orders, once to count the cohort.
LET active =
  FROM /sql/pg/customers
  |> WHERE status = 'active'
FROM /sql/pg/orders AS o
|> JOIN active AS a ON o.customer_id = a.id
|> AGGREGATE count(active.id) AS active_customers, sum(o.amount) AS active_revenue
```

**Normalize email addresses with a lambda before deduping a mailing list.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M6; features=LET,=>,EXTEND,DISTINCT
LET canon = (addr: String) => lower(trim(addr))
FROM /sql/pg/contacts
|> EXTEND key = canon(email)
|> SELECT key
|> DISTINCT
```

**Define a reusable margin function once and apply it across products.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M6; features=LET,=>,EXTEND,ORDER BY
# A user-defined function is just a LET-bound lambda — no separate function namespace.
LET margin = (price: Float, cost: Float) => (price - cost) / price
FROM /sql/pg/products
|> EXTEND gross_margin = margin(unit_price, unit_cost)
|> WHERE gross_margin < 0.2
|> ORDER BY gross_margin
```

**Split a comma-separated tags column into a normalized list per row.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M6; features=LET,=>,map,EXTEND
LET clean = (t: String) => lower(trim(t))
FROM /sql/pg/articles
|> EXTEND tags = map(split(raw_tags, ','), clean)
|> SELECT id, title, tags
```

**Keep only the high-value line items inside each invoice.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M6; features==>,filter,EXTEND
FROM /sql/pg/invoices
|> EXTEND big_lines = filter(line_items, (li: Row) => li.amount > 1000)
|> WHERE size(big_lines) > 0
|> SELECT id, customer, big_lines
```

**Total each order's line items with a reduce.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M6; features==>,reduce,EXTEND
FROM /sql/pg/orders
|> EXTEND computed_total = reduce(line_items, (acc: Float, li: Row) => acc + li.amount, 0.0)
|> WHERE computed_total <> stored_total
|> SELECT id, stored_total, computed_total
```

**Bind a date threshold once and reuse it across two filters.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M6; features=LET,WHERE,EXTEND
# cutoff is a scalar value, referenced in both the predicate and a derived flag.
LET cutoff = '2026-03-27'
FROM /sql/pg/orders
|> WHERE created_at >= cutoff
|> EXTEND is_fresh = created_at >= cutoff
|> SELECT id, created_at, is_fresh
```

**Compare each repo's PR throughput to the org-wide average.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M6; features=LET,FROM,AGGREGATE,JOIN
LET org_avg =
  FROM /github/acme/web/pulls
  |> AGGREGATE avg(merged_count) AS org_avg_merged
FROM /github/acme/web/pulls
|> AGGREGATE count(id) AS merged_count GROUP BY repo
|> JOIN org_avg ON true
|> WHERE merged_count > org_avg_merged
```

**Score Slack messages with a reusable urgency lambda.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M6; features=LET,=>,EXTEND,ORDER BY
LET urgency = (text: String) =>
  (text ~ '(?i)urgent') OR (text ~ '(?i)asap') OR (text LIKE '%blocker%')
FROM /slack/acme/incidents/messages
|> EXTEND is_urgent = urgency(body)
|> WHERE is_urgent
|> ORDER BY ts
```

**Flag products priced below the average of their own brand.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M6; features=LET,AGGREGATE,GROUP BY,JOIN,WHERE
LET brand_avg =
  FROM /sql/pg/products
  |> AGGREGATE avg(unit_price) AS avg_price GROUP BY brand
FROM /sql/pg/products AS p
|> JOIN brand_avg AS b ON p.brand = b.brand
|> WHERE p.unit_price < b.avg_price * 0.7
|> SELECT p.sku, p.brand, p.unit_price, b.avg_price
```

**Apply a discount lambda across a basket and write the repriced rows back.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M6; features=LET,=>,EXTEND,UPSERT INTO
LET discounted = (price: Float, pct: Float) => round(price * (1 - pct), 2)
FROM /sql/pg/cart_items
|> EXTEND final_price = discounted(unit_price, promo_pct)
|> UPSERT INTO /sql/pg/cart_items
     VALUES (id => id, unit_price => final_price)
```

**Compute a per-customer lifetime value and reuse it for tiering.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M6; features=LET,AGGREGATE,GROUP BY,EXTEND
LET ltv =
  FROM /sql/pg/orders
  |> AGGREGATE sum(amount) AS lifetime_value GROUP BY customer_id
FROM ltv
|> EXTEND tier = (lifetime_value > 10000)
|> SELECT customer_id, lifetime_value, tier
|> ORDER BY lifetime_value
```

**Strip null and empty entries from a phone-number array.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M6; features==>,filter,EXTEND
FROM /sql/pg/contacts
|> EXTEND phones = filter(raw_phones, (p: String) => p <> '' AND p IS NOT NULL)
|> SELECT id, name, phones
```

**Title-case every word in a product name via map.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M6; features=LET,=>,map,EXTEND
LET cap = (w: String) => upper(substr(w, 1, 1)) || lower(substr(w, 2))
FROM /sql/pg/products
|> EXTEND display_name = join_words(map(split(name, ' '), cap), ' ')
|> SELECT sku, display_name
```

**Bind a region filter once and join it against two fact tables.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M6; features=LET,WHERE,JOIN,AGGREGATE,GROUP BY
LET emea =
  FROM /sql/pg/regions
  |> WHERE continent IN ('Europe', 'Middle East', 'Africa')
FROM /sql/pg/orders AS o
|> JOIN emea AS r ON o.region_id = r.id
|> AGGREGATE sum(o.amount) AS emea_revenue GROUP BY r.country
|> ORDER BY emea_revenue
```

**Find customers whose latest order beats their own historical average.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M6; features=LET,AGGREGATE,GROUP BY,JOIN,WHERE
LET cust_avg =
  FROM /sql/pg/orders
  |> AGGREGATE avg(amount) AS personal_avg GROUP BY customer_id
FROM /sql/pg/orders AS o
|> JOIN cust_avg AS c ON o.customer_id = c.customer_id
|> WHERE o.amount > c.personal_avg * 1.5
|> SELECT o.customer_id, o.id, o.amount, c.personal_avg
```

**Reduce daily metric rows into a running max per service.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M6; features==>,reduce,EXTEND
FROM /sql/pg/metrics
|> EXTEND peak = reduce(samples, (m: Float, s: Row) => max(m, s.value), 0.0)
|> SELECT service, peak
|> ORDER BY peak
```

**Normalize a join key with a lambda so mismatched casing still matches.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M6; features=LET,=>,EXTEND,JOIN
LET norm = (s: String) => lower(trim(s))
FROM /sql/pg/leads AS l
|> EXTEND lkey = norm(l.company)
|> JOIN /sql/pg/accounts AS a ON lkey = norm(a.company)
|> SELECT l.id, a.id AS account_id, l.company
```

**Build a per-department headcount and compare each manager's span to it.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M6; features=LET,AGGREGATE,GROUP BY,JOIN,EXTEND
LET dept_size =
  FROM /sql/pg/employees
  |> AGGREGATE count(id) AS headcount GROUP BY department
FROM /sql/pg/employees AS e
|> JOIN dept_size AS d ON e.department = d.department
|> WHERE e.is_manager
|> EXTEND span_share = e.direct_reports / d.headcount
|> SELECT e.name, e.department, span_share
```

**Extract and lowercase the domain from every signup email.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M6; features=LET,=>,EXTEND,GROUP BY,AGGREGATE
LET domain_of = (email: String) => lower(split(email, '@')[2])
FROM /sql/pg/signups
|> EXTEND domain = domain_of(email)
|> AGGREGATE count(id) AS signups GROUP BY domain
|> ORDER BY signups
```

**Keep only PRs whose review set contains an approval.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M6; features==>,filter,EXTEND,WHERE
FROM /github/acme/web/pulls
|> EXTEND approvals = filter(reviews, (r: Row) => r.state = 'APPROVED')
|> WHERE size(approvals) >= 2
|> SELECT number, title, size(approvals) AS approval_count
```

**Bind a markdown frontmatter set and write a summary index from it.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M6; features=LET,DECODE,ENCODE,UPSERT INTO
# posts is decoded once, then both filtered and re-encoded into an index file.
LET posts =
  FROM /local/blog/posts.jsonl
  |> DECODE jsonl
FROM posts
|> WHERE published
|> SELECT title, slug, tags
|> ENCODE csv
|> UPSERT INTO /drive/Blog/index.csv
```

**Compute each category's share of total revenue using a bound grand total.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M6; features=LET,AGGREGATE,GROUP BY,JOIN,EXTEND
LET grand =
  FROM /sql/pg/orders
  |> AGGREGATE sum(amount) AS total_revenue
FROM /sql/pg/orders
|> AGGREGATE sum(amount) AS cat_revenue GROUP BY category
|> JOIN grand ON true
|> EXTEND share = cat_revenue / total_revenue
|> ORDER BY share
```

**Validate phone formats with a reusable predicate lambda before export.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M6; features=LET,=>,WHERE,ENCODE,UPSERT INTO
LET is_e164 = (p: String) => p ~ '^\+[1-9][0-9]{7,14}$'
FROM /sql/pg/contacts
|> WHERE NOT is_e164(phone)
|> SELECT id, name, phone
|> ENCODE csv
|> UPSERT INTO /drive/DataQuality/bad_phones.csv
```

**Bound stale-threshold reused to flag and to draft nudges in one pass.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M6; features=LET,=>,WHERE,INSERT INTO,||
LET stale_before = '2026-04-26'
FROM /sql/pg/tasks
|> WHERE due_at < stale_before AND status <> 'done'
|> INSERT INTO /mail/drafts
     VALUES (to => assignee_email,
             subject => 'Overdue since ' || stale_before,
             body => 'Task ' || title || ' is past its due date.')
```

**Sum weighted scores across a scorecard with reduce.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M6; features==>,reduce,EXTEND,ORDER BY
FROM /sql/pg/vendor_scorecards
|> EXTEND weighted = reduce(criteria, (acc: Float, c: Row) => acc + c.score * c.weight, 0.0)
|> SELECT vendor, weighted
|> ORDER BY weighted
```

**Find accounts whose spend exceeds twice the cohort median proxy.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M6; features=LET,AGGREGATE,GROUP BY,JOIN,WHERE
LET cohort =
  FROM /sql/pg/accounts
  |> AGGREGATE avg(annual_spend) AS cohort_avg GROUP BY plan
FROM /sql/pg/accounts AS a
|> JOIN cohort AS c ON a.plan = c.plan
|> WHERE a.annual_spend > c.cohort_avg * 2
|> SELECT a.name, a.plan, a.annual_spend, c.cohort_avg
```

**Apply a tax lambda per jurisdiction and reduce to a grand invoice total.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M6; features=LET,=>,map,reduce,EXTEND
LET taxed = (li: Row) => li.amount * (1 + li.tax_rate)
FROM /sql/pg/invoices
|> EXTEND total_with_tax = reduce(map(line_items, taxed), (acc: Float, x: Float) => acc + x, 0.0)
|> SELECT id, customer, total_with_tax
```

## Transactions

A `TRANSACTION { … }` block commits a set of writes all-or-nothing across heterogeneous sources — a SQL row, a local ledger line, an object in S3, a git commit — so that either every write lands or none does. The block is **reversible-only**: every effect inside must be undoable (UPSERT, INSERT of a draft, a git commit), so the engine can roll the whole set back if any participant fails. An irreversible effect — `CALL mail.send`, `CALL github.merge`, a destructive `REMOVE` against an append-only log — **inside** a `TRANSACTION` is a parse-time error, not a runtime one: the grammar refuses it before any work begins. The pattern is therefore always two recipes — first the transaction reaches its commit point, then a *separate* block performs the irreversible side effect once the durable state is safely on disk.

Each `TRANSACTION { … }` is a single statement (`grammar=extended`, delivered in M6). The separate irreversible follow-up is frequently `grammar=core`, since `CALL` and ordinary effects are frozen grammar.

**Record a paid invoice in both the SQL ledger and the local audit ledger atomically.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M6; features=TRANSACTION,UPSERT INTO,VALUES
# Both rows land or neither does — the local ledger never drifts from the database of record.
TRANSACTION {
  UPSERT INTO /sql/pg/invoices
    VALUES (id => 'INV-4821', status => 'paid', paid_at => '2026-06-26')
  UPSERT INTO /local/ledger/2026-06.jsonl
    VALUES (invoice => 'INV-4821', event => 'paid', amount => 1290.00)
}
```

**Promote a build artifact to the release bucket and stamp its release row together.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M6; features=TRANSACTION,UPSERT INTO,SELECT,FROM,DECODE
# The S3 object and the releases table move as one unit.
TRANSACTION {
  UPSERT INTO /s3/releases/app-1.4.0.tar.gz
    FROM /s3/ci-staging/app-1.4.0.tar.gz
  UPSERT INTO /sql/pg/releases
    VALUES (version => '1.4.0', channel => 'stable', published_at => '2026-06-26')
}
```

**Onboard a new customer: write the SQL customer row and seed their drive folder index.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M6; features=TRANSACTION,UPSERT INTO,VALUES,ENCODE
# A half-created customer (row but no workspace, or workspace but no row) is impossible.
TRANSACTION {
  UPSERT INTO /sql/pg/customers
    VALUES (id => 'C-9001', name => 'Northwind Traders', tier => 'pro')
  UPSERT INTO /drive/Customers/C-9001/manifest.json
    VALUES (customer => 'C-9001', created => '2026-06-26', tier => 'pro')
}
```

**Apply a config change as a git commit and flip the deployment-state row in lockstep.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M6; features=TRANSACTION,INSERT INTO,UPSERT INTO,VALUES
# The committed config and the "what is live" pointer can never disagree.
TRANSACTION {
  INSERT INTO /git/infra/commits
    VALUES (branch => 'main',
            message => 'bump replicas to 6',
            path => 'k8s/web.yaml',
            content => 'replicas: 6')
  UPSERT INTO /sql/pg/deploy_state
    VALUES (service => 'web', desired_replicas => 6, updated_at => '2026-06-26')
}
```

**Move money between two SQL accounts so the debit and credit are inseparable.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M6; features=TRANSACTION,UPDATE,SET,WHERE
# Classic double-entry: a failure after the debit must not leave the credit unbooked.
TRANSACTION {
  UPDATE /sql/pg/accounts
    SET balance = balance - 500
    WHERE id = 'ACC-100'
  UPDATE /sql/pg/accounts
    SET balance = balance + 500
    WHERE id = 'ACC-200'
}
```

**Reconcile an order across SQL, the S3 receipt blob, and the local fulfilment ledger.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M6; features=TRANSACTION,UPSERT INTO,VALUES,ENCODE
# Three sources, one atomic boundary: order state, durable receipt, and ledger line.
TRANSACTION {
  UPSERT INTO /sql/pg/orders
    VALUES (id => 'O-7700', status => 'fulfilled', fulfilled_at => '2026-06-26')
  UPSERT INTO /s3/receipts/O-7700.json
    VALUES (order => 'O-7700', total => 240.00, currency => 'USD')
  UPSERT INTO /local/ledger/fulfilment.jsonl
    VALUES (order => 'O-7700', event => 'shipped')
}
```

**Use a LET-bound batch id to tag a SQL write and a Cloudflare KV write identically.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M6; features=LET,TRANSACTION,UPSERT INTO,VALUES
# One generated id threads through every participant so cross-store joins stay sound.
LET batch = 'B-2026-06-26-01'
TRANSACTION {
  UPSERT INTO /sql/pg/import_batches
    VALUES (id => batch, state => 'committed', rows => 4200)
  UPSERT INTO /kv/imports/last_batch
    VALUES (value => batch)
}
```

**Snapshot the live pricing table into git and record the snapshot ref in SQL together.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M6; features=TRANSACTION,INSERT INTO,UPSERT INTO,FROM,ENCODE
# The version-controlled snapshot and its catalogue entry are committed atomically.
TRANSACTION {
  INSERT INTO /git/pricing/commits
    VALUES (branch => 'main',
            message => 'pricing snapshot 2026-06-26',
            path => 'prices.csv',
            content => 'sku,price\nA1,9.99\nB2,19.99')
  UPSERT INTO /sql/pg/pricing_snapshots
    VALUES (taken_at => '2026-06-26', ref => 'main', sku_count => 2)
}
```

**Quarantine a flagged user: disable the SQL account and write the case file to drive at once.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M6; features=TRANSACTION,UPDATE,SET,WHERE,UPSERT INTO,VALUES
# Either the account is locked AND the case is documented, or nothing changes.
TRANSACTION {
  UPDATE /sql/pg/users
    SET status = 'suspended', suspended_at = '2026-06-26'
    WHERE id = 'U-553'
  UPSERT INTO /drive/Trust/cases/U-553.json
    VALUES (user => 'U-553', reason => 'fraud_signal', opened => '2026-06-26')
}
```

**Close a sprint: archive the board state to git and roll the SQL sprint counter forward.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M6; features=TRANSACTION,INSERT INTO,UPDATE,SET,WHERE
# A committed archive without an advanced counter (or vice versa) can never happen.
TRANSACTION {
  INSERT INTO /git/ops/commits
    VALUES (branch => 'main',
            message => 'archive sprint 41',
            path => 'sprints/41.json',
            content => '{"sprint":41,"closed":true}')
  UPDATE /sql/pg/project
    SET current_sprint = 42
    WHERE id = 'PRJ-1'
}
```

**Sync a derived report to both R2 and the SQL report index in one commit.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M6; features=TRANSACTION,UPSERT INTO,FROM,ENCODE,SELECT
# Downstream readers see the new blob and its index entry simultaneously.
TRANSACTION {
  UPSERT INTO /r2/reports/q2-summary.csv
    FROM /sql/pg/sales
    |> AGGREGATE sum(amount) AS total GROUP BY region
    |> ENCODE csv
  UPSERT INTO /sql/pg/reports
    VALUES (name => 'q2-summary', location => 'r2://reports/q2-summary.csv', built_at => '2026-06-26')
}
```

**Transfer inventory between warehouses with two UPDATEs that must agree.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M6; features=TRANSACTION,UPDATE,SET,WHERE
# Stock leaves one location only if it arrives at the other.
TRANSACTION {
  UPDATE /sql/pg/inventory
    SET qty = qty - 30
    WHERE sku = 'SKU-12' AND warehouse = 'WH-EAST'
  UPDATE /sql/pg/inventory
    SET qty = qty + 30
    WHERE sku = 'SKU-12' AND warehouse = 'WH-WEST'
}
```

**Record a signed contract: store the PDF in drive and the metadata row in SQL atomically.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M6; features=TRANSACTION,UPSERT INTO,FROM,VALUES
# The durable document and its searchable row commit together.
TRANSACTION {
  UPSERT INTO /drive/Contracts/2026/CT-330.pdf
    FROM /local/incoming/CT-330.pdf
  UPSERT INTO /sql/pg/contracts
    VALUES (id => 'CT-330', counterparty => 'Acme', signed_on => '2026-06-26', status => 'active')
}
```

**Atomically dedupe: remove a duplicate SQL row and log the merge in the local ledger.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M6; features=TRANSACTION,UPDATE,SET,WHERE,UPSERT INTO,VALUES
# We fold the duplicate into the survivor and record why — both or neither.
TRANSACTION {
  UPDATE /sql/pg/customers
    SET merged_into = 'C-1001', status = 'merged'
    WHERE id = 'C-1002'
  UPSERT INTO /local/ledger/merges.jsonl
    VALUES (survivor => 'C-1001', duplicate => 'C-1002', at => '2026-06-26')
}
```

**Publish a knowledge-base article as a git commit and index it in SQL together.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M6; features=TRANSACTION,INSERT INTO,UPSERT INTO,VALUES
# The published markdown and its catalogue entry are one atomic publish.
TRANSACTION {
  INSERT INTO /git/kb/commits
    VALUES (branch => 'main',
            message => 'publish: refund policy',
            path => 'articles/refund-policy.md',
            content => '# Refund policy\nRefunds within 30 days.')
  UPSERT INTO /sql/pg/kb_index
    VALUES (slug => 'refund-policy', title => 'Refund policy', published => '2026-06-26')
}
```

**Bind three batch rows with a LET value and commit SQL, drive, and KV as a unit.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M6; features=LET,TRANSACTION,UPSERT INTO,VALUES
# A single run id stamps all three stores so the import is traceable end to end.
LET run = 'RUN-558'
TRANSACTION {
  UPSERT INTO /sql/pg/etl_runs
    VALUES (id => run, status => 'done', rows => 18000)
  UPSERT INTO /drive/ETL/runs/RUN-558.json
    VALUES (run => run, status => 'done')
  UPSERT INTO /kv/etl/latest
    VALUES (value => run)
}
```

**Accept a return: restock SQL inventory and write the credit memo blob to S3 atomically.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M6; features=TRANSACTION,UPDATE,SET,WHERE,UPSERT INTO,VALUES
# Stock returns to the shelf only if the credit memo is durably stored.
TRANSACTION {
  UPDATE /sql/pg/inventory
    SET qty = qty + 1
    WHERE sku = 'SKU-88'
  UPSERT INTO /s3/credit-memos/CM-204.json
    VALUES (id => 'CM-204', order => 'O-9000', amount => 49.00)
}
```

**Tag a release in git refs and mark the SQL release row as tagged together.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M6; features=TRANSACTION,UPSERT INTO,UPDATE,SET,WHERE,VALUES
# The mutable tag pointer and the release record move atomically.
TRANSACTION {
  UPSERT INTO /git/app/refs
    VALUES (name => 'v1.4.0', target => 'main')
  UPDATE /sql/pg/releases
    SET tagged = true, tag = 'v1.4.0'
    WHERE version = '1.4.0'
}
```

**Migrate a row between tables: insert into the new table and tombstone the old one atomically.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M6; features=TRANSACTION,INSERT INTO,FROM,WHERE,UPDATE,SET
# The new home and the old tombstone commit together — no orphan, no double-live row.
TRANSACTION {
  INSERT INTO /sql/pg/accounts_v2
    FROM /sql/pg/accounts
    |> WHERE id = 'ACC-777'
  UPDATE /sql/pg/accounts
    SET status = 'migrated'
    WHERE id = 'ACC-777'
}
```

**Append an event to a queue and persist its SQL projection in one atomic write.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M6; features=TRANSACTION,INSERT INTO,UPSERT INTO,VALUES
# The queued work item and its read-model row are consistent the instant they exist.
TRANSACTION {
  INSERT INTO /queues/billing
    VALUES (kind => 'charge', invoice => 'INV-900', amount => 75.00)
  UPSERT INTO /sql/pg/invoice_state
    VALUES (invoice => 'INV-900', state => 'charging')
}
```

**Commit a data-quality fix to git and update the SQL row it corrects together.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M6; features=TRANSACTION,INSERT INTO,UPDATE,SET,WHERE
# The audit trail (the commit) and the corrected fact land as one.
TRANSACTION {
  INSERT INTO /git/data-fixes/commits
    VALUES (branch => 'main',
            message => 'fix: country code for C-44',
            path => 'fixes/C-44.json',
            content => '{"id":"C-44","country":"JP"}')
  UPDATE /sql/pg/customers
    SET country = 'JP'
    WHERE id = 'C-44'
}
```

**Provision a project: SQL project row, drive workspace manifest, and D1 metadata, all-or-nothing.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M6; features=TRANSACTION,UPSERT INTO,VALUES
# Three independent stores either all reflect the new project or none do.
TRANSACTION {
  UPSERT INTO /sql/pg/projects
    VALUES (id => 'PRJ-9', name => 'Atlas', status => 'active')
  UPSERT INTO /drive/Projects/Atlas/manifest.json
    VALUES (project => 'PRJ-9', created => '2026-06-26')
  UPSERT INTO /d1/edge/projects
    VALUES (id => 'PRJ-9', region => 'apac')
}
```

**Now the commit-point pattern: settle an invoice atomically, THEN email the receipt in a separate block.**

The transaction below moves only reversible state — the SQL invoice row and the local ledger line. The receipt email is irreversible (`CALL mail.send` cannot be un-sent), so it lives in its own block that runs *after* the transaction has committed. Putting that `CALL` inside the `TRANSACTION` would be a parse-time error.

```qfs
# qfs-cookbook: grammar=extended; milestone=M6; features=TRANSACTION,UPSERT INTO,VALUES
# Step 1 of 2 — durable, reversible settlement reaches its commit point first.
TRANSACTION {
  UPSERT INTO /sql/pg/invoices
    VALUES (id => 'INV-4821', status => 'paid', paid_at => '2026-06-26')
  UPSERT INTO /local/ledger/2026-06.jsonl
    VALUES (invoice => 'INV-4821', event => 'paid', amount => 1290.00)
}
```

**Send the receipt only after settlement committed (separate, irreversible — runs with --commit-irreversible).**

```qfs
# qfs-cookbook: grammar=extended; milestone=M6; features=FROM,WHERE,CALL,||
# Step 2 of 2 — the irreversible CALL lives OUTSIDE the transaction, after the commit point.
FROM /sql/pg/invoices
|> WHERE id = 'INV-4821' AND status = 'paid'
|> CALL mail.send(to => billing_email,
                  subject => 'Receipt for ' || id,
                  body => 'Thank you — invoice ' || id || ' is paid in full.')
```

**Commit-point pattern (PR merge): record the merge decision in SQL atomically with a git audit commit.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M6; features=TRANSACTION,UPSERT INTO,INSERT INTO,VALUES
# Step 1 of 2 — reversible bookkeeping for the merge decision commits first.
TRANSACTION {
  UPSERT INTO /sql/pg/pr_decisions
    VALUES (pr => 42, repo => 'acme/web', decision => 'approved', at => '2026-06-26')
  INSERT INTO /git/audit/commits
    VALUES (branch => 'main',
            message => 'approve merge of PR #42',
            path => 'decisions/pr-42.json',
            content => '{"pr":42,"decision":"approved"}')
}
```

**Perform the actual merge only after the decision committed (separate, irreversible github.merge).**

```qfs
# qfs-cookbook: grammar=core; milestone=M6; features=FROM,WHERE,CALL
# Step 2 of 2 — github.merge is irreversible, so it runs OUTSIDE the transaction afterwards.
FROM /github/acme/web/pulls/42
|> WHERE state = 'open'
|> CALL github.merge(number => 42, method => 'squash')
```

**Commit-point pattern (announcement): stage the release state atomically, THEN post to Slack separately.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M6; features=TRANSACTION,UPSERT INTO,VALUES
# Step 1 of 2 — the durable release state (SQL row + S3 notes blob) commits as a unit.
TRANSACTION {
  UPSERT INTO /sql/pg/releases
    VALUES (version => '1.4.0', channel => 'stable', announced => false)
  UPSERT INTO /s3/releases/notes-1.4.0.md
    VALUES (version => '1.4.0', notes => '# 1.4.0\n- faster sync\n- bug fixes')
}
```

**Announce the release only after state committed (separate, irreversible slack.post).**

```qfs
# qfs-cookbook: grammar=extended; milestone=M6; features=FROM,WHERE,CALL,||
# Step 2 of 2 — slack.post cannot be un-posted, so it runs OUTSIDE the transaction.
FROM /sql/pg/releases
|> WHERE version = '1.4.0' AND announced = false
|> CALL slack.post(channel => '#releases',
                   text => 'Shipped ' || version || ' to the stable channel.')
```

## Policy, ACL & directory

Access in qfs is itself a query. `CREATE POLICY <name> ALLOW <verbs> ON '<glob>' [WHERE <cond>]` writes a row into the policy registry, and because the registry is just another mount you read, audit, and reshape policy the same way you read any service. Policies grant verbs (`read`, `write`, `call`, …) over a path glob to a role or group, with `WHERE` clauses that scope down to rows and columns and can defer the decision to your real identity provider via `member_of('/directories/...')`. The companion `/directories/{google,entra,ad}` mounts expose groups and users read-only, so a single source of truth — Google Workspace, Entra, or Active Directory — drives both who-is-who lookups and who-can-do-what grants. Most of these are `grammar=core` (POLICY is a frozen DDL keyword and `member_of(...)` is an ordinary function call); only the few that bind with `LET` or pass a lambda `=>` are `grammar=extended`.

**Give the on-call engineers read access to the production database.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M5; features=CREATE POLICY,ALLOW,ON,WHERE,member_of
CREATE POLICY oncall_pg_read
  ALLOW read ON '/sql/pg/**'
  WHERE member_of('/directories/google/groups/oncall@acme.com')
```

**Let the data team read every warehouse table but never the customer PII table.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M5; features=CREATE POLICY,ALLOW,ON,WHERE,member_of,NOT,LIKE,AND
CREATE POLICY data_team_warehouse
  ALLOW read ON '/sql/pg/**'
  WHERE member_of('/directories/entra/groups/data-team')
    AND NOT path LIKE '/sql/pg/customer_pii%'
```

**Grant the support role read-and-draft on the shared mailbox, but no sending.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M5; features=CREATE POLICY,ALLOW,ON,WHERE,member_of
CREATE POLICY support_mail_triage
  ALLOW read, write ON '/mail/**'
  WHERE member_of('/directories/ad/groups/support-tier1')
```

**Allow finance to send mail, since drafting alone is not enough for them.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M5; features=CREATE POLICY,ALLOW,ON,WHERE,member_of
CREATE POLICY finance_mail_send
  ALLOW read, write, call ON '/mail/**'
  WHERE member_of('/directories/google/groups/finance@acme.com')
```

**Scope a region's analysts to only their own region's rows (row-level security).**

```qfs
# qfs-cookbook: grammar=extended; milestone=M5; features=CREATE POLICY,ALLOW,ON,WHERE,member_of,=
CREATE POLICY emea_orders_rls
  ALLOW read ON '/sql/pg/orders'
  WHERE member_of('/directories/entra/groups/analysts-emea')
    AND region = 'EMEA'
```

**Hide salary and SSN columns from everyone outside HR (column-level scoping).**

```qfs
# qfs-cookbook: grammar=extended; milestone=M5; features=CREATE POLICY,ALLOW,ON,WHERE,member_of,NOT,IN
CREATE POLICY mask_employee_secrets
  ALLOW read ON '/sql/pg/employees'
  WHERE NOT member_of('/directories/ad/groups/hr-core')
    AND column NOT IN ('salary', 'ssn', 'bank_account')
```

**Let managers read their own direct reports' rows only.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M5; features=CREATE POLICY,ALLOW,ON,WHERE,member_of,=
CREATE POLICY manager_sees_reports
  ALLOW read ON '/sql/pg/employees'
  WHERE member_of('/directories/google/groups/managers@acme.com')
    AND manager_email = current_user()
```

**Give every full-time employee read access to the company wiki bucket.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M5; features=CREATE POLICY,ALLOW,ON,WHERE,member_of
CREATE POLICY wiki_read_all_staff
  ALLOW read ON '/drive/Wiki/**'
  WHERE member_of('/directories/google/groups/all-staff@acme.com')
```

**Let release engineers merge pull requests in the web repo, but not anywhere else.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M5; features=CREATE POLICY,ALLOW,ON,WHERE,member_of
CREATE POLICY release_eng_merge_web
  ALLOW read, write, call ON '/github/acme/web/**'
  WHERE member_of('/directories/entra/groups/release-engineers')
```

**Allow contractors read-only on a single repo for the length of an engagement.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M5; features=CREATE POLICY,ALLOW,ON,WHERE,member_of,AND,<
CREATE POLICY contractor_repo_window
  ALLOW read ON '/github/acme/mobile/**'
  WHERE member_of('/directories/ad/groups/contractors-2026')
    AND now() < '2026-12-31'
```

**Let the marketing group post to one Slack channel only.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M5; features=CREATE POLICY,ALLOW,ON,WHERE,member_of
CREATE POLICY marketing_announce_channel
  ALLOW read, write, call ON '/slack/acme/announcements/**'
  WHERE member_of('/directories/google/groups/marketing@acme.com')
```

**Grant the analytics group read on Google Analytics but nothing that can write.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M5; features=CREATE POLICY,ALLOW,ON,WHERE,member_of
CREATE POLICY ga_read_analytics
  ALLOW read ON '/ga/**'
  WHERE member_of('/directories/entra/groups/growth-analytics')
```

**Deny write to anything in the production S3 bucket unless you are in platform-ops.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M5; features=CREATE POLICY,ALLOW,ON,WHERE,member_of
CREATE POLICY prod_s3_write_guard
  ALLOW read, write ON '/s3/prod-assets/**'
  WHERE member_of('/directories/ad/groups/platform-ops')
```

**Inherit access from a parent team: give the whole engineering org read on all repos.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M5; features=CREATE POLICY,ALLOW,ON,WHERE,member_of
CREATE POLICY engineering_repos_read
  ALLOW read ON '/github/acme/**'
  WHERE member_of('/directories/google/groups/engineering@acme.com')
```

**Layer a narrower grant on top: the security sub-team may also write branch protection.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M5; features=CREATE POLICY,ALLOW,ON,WHERE,member_of
CREATE POLICY security_repos_write
  ALLOW read, write, call ON '/github/acme/**/refs'
  WHERE member_of('/directories/google/groups/security@acme.com')
```

**Let admins read the audit trail but never the raw connections secrets.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M5; features=CREATE POLICY,ALLOW,ON,WHERE,member_of,NOT,LIKE,AND
CREATE POLICY admin_audit_read
  ALLOW read ON '/sys/audit/**'
  WHERE member_of('/directories/entra/groups/it-admins')
    AND NOT path LIKE '/sys/connections/%/secret%'
```

**Conditional grant: allow refunds only for orders under a 1,000 currency threshold.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M5; features=CREATE POLICY,ALLOW,ON,WHERE,member_of,AND,<=
CREATE POLICY support_small_refunds
  ALLOW read, write ON '/sql/pg/refunds'
  WHERE member_of('/directories/ad/groups/support-tier1')
    AND amount <= 1000
```

**Let auditors read historical orders as-of-time but never the live mutable table writes.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M5; features=CREATE POLICY,ALLOW,ON,WHERE,member_of
CREATE POLICY auditor_orders_history
  ALLOW read ON '/sql/pg/orders'
  WHERE member_of('/directories/google/groups/auditors@acme.com')
```

**Grant the data-science group read on the reports drive folder and the queues that feed it.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M5; features=CREATE POLICY,ALLOW,ON,WHERE,member_of,OR
CREATE POLICY ds_reports_and_queues
  ALLOW read ON '/drive/Reports/**'
  WHERE member_of('/directories/entra/groups/data-science')
    OR member_of('/directories/entra/groups/ml-platform')
```

**Restrict KV namespace writes to the team that owns the feature-flag namespace.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M5; features=CREATE POLICY,ALLOW,ON,WHERE,member_of
CREATE POLICY flags_kv_owners
  ALLOW read, write ON '/kv/feature-flags/**'
  WHERE member_of('/directories/google/groups/web-platform@acme.com')
```

**Use a LET-bound directory glob so the same group drives two scoped grants at once.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M6; features=LET,CREATE POLICY,ALLOW,ON,WHERE,member_of,OR
LET billing_team = '/directories/google/groups/billing@acme.com'
CREATE POLICY billing_pg_and_drive
  ALLOW read, write ON '/sql/pg/invoices'
  WHERE member_of(billing_team)
    OR member_of('/directories/google/groups/finance@acme.com')
```

**Build a reusable membership predicate as a lambda and apply it to a sensitive table.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M6; features=LET,=>,CREATE POLICY,ALLOW,ON,WHERE,AND
LET is_privileged = (g: String) => member_of(g) AND now() < '2027-01-01'
CREATE POLICY privileged_payroll_read
  ALLOW read ON '/sql/pg/payroll'
  WHERE is_privileged('/directories/entra/groups/payroll-admins')
```

**List the current members of the on-call group straight from the directory.**

```qfs
# qfs-cookbook: grammar=core; milestone=M5; features=FROM,SELECT,ORDER BY
FROM /directories/google/groups/oncall@acme.com/members
|> SELECT display_name, email, role
|> ORDER BY display_name
```

**Find every directory user in the EMEA department who has an admin job title.**

```qfs
# qfs-cookbook: grammar=core; milestone=M5; features=FROM,WHERE,SELECT,AND,LIKE,=,ORDER BY
FROM /directories/entra/users
|> WHERE department = 'EMEA'
   AND job_title LIKE '%Admin%'
|> SELECT display_name, email, job_title, manager
|> ORDER BY display_name
```

**Cross-check a Slack channel's posters against who is actually allowed to post there.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M5; features=FROM,SELECT,DISTINCT,EXCEPT,JOIN,ON
FROM /slack/acme/announcements/messages
|> SELECT DISTINCT user_email AS email
|> EXCEPT
   FROM /directories/google/groups/marketing@acme.com/members
   |> SELECT email
```

**Show which active directory users have no policy granting them any database read.**

```qfs
# qfs-cookbook: grammar=core; milestone=M5; features=FROM,WHERE,SELECT,EXCEPT,=,LIKE
FROM /directories/ad/users
|> WHERE account_enabled = true
|> SELECT email
|> EXCEPT
   FROM /sys/policies
   |> WHERE path_glob LIKE '/sql/%'
   |> SELECT principal_email AS email
```

**Reconcile a group's directory members with the people who actually appear in the audit log.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M5; features=FROM,SELECT,JOIN,ON,WHERE,AGGREGATE,GROUP BY,ORDER BY
FROM /directories/google/groups/finance@acme.com/members
|> JOIN /sys/audit ON members.email = audit.actor_email
|> WHERE audit.verb = 'call'
|> AGGREGATE count() AS actions GROUP BY members.email
|> ORDER BY actions
```

**Grant access by attribute rather than group: anyone whose directory department is Legal.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M5; features=CREATE POLICY,ALLOW,ON,WHERE,member_of,AND
CREATE POLICY legal_contracts_read
  ALLOW read ON '/drive/Contracts/**'
  WHERE member_of('/directories/entra/groups/legal-dept')
    AND classification <> 'restricted'
```

## Server automation

When qfs runs as a server it stops being a one-shot CLI and becomes a small standing
runtime: queries you would otherwise type by hand are registered once as **endpoints,
triggers, jobs, views, and webhooks**, then fire on HTTP requests, upstream events, or
an external schedule. Each is a single DDL statement that is sugar over a write into `/server/...`,
so the whole automation surface is itself queryable and version-controlled. qfs is not a
scheduler — it owns no clock or dispatcher. A JOB is a **saved named plan** whose cadence is
fired by **OS cron (individual) or Cloudflare Cron Triggers (managed)** — qfs does not run it.
Triggers reference the firing row as `NEW.*`; `LAST_RUN()` exposes the timestamp of the
previous external run as metadata, so a plan reads only what changed since. Reversible plans
(drafts, UPSERTs to storage, view materialization) run unattended; irreversible `CALL`s in a
plan still require the server's configured irreversible-commit policy.

**Expose the current on-call engineer as a read-only HTTP endpoint.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M8; features=CREATE ENDPOINT,FROM,WHERE,ORDER BY,LIMIT,SELECT
# GET /oncall returns the single active rotation row as JSON
CREATE ENDPOINT GET /oncall AS
  FROM /sql/pg/oncall_rotations
  |> WHERE active = true
  |> ORDER BY shift_start DESC
  |> LIMIT 1
  |> SELECT engineer, phone, slack_handle, shift_end
```

**Serve a per-customer order history endpoint keyed by a path parameter.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M8; features=CREATE ENDPOINT,FROM,WHERE,JOIN,ORDER BY,SELECT
# GET /customers/:id/orders joins orders to line items for the requested customer
CREATE ENDPOINT GET /customers/:id/orders AS
  FROM /sql/pg/orders
  |> WHERE customer_id = :id
  |> JOIN /sql/pg/line_items ON line_items.order_id = orders.id
  |> ORDER BY orders.placed_at DESC
  |> SELECT orders.id, orders.placed_at, line_items.sku, line_items.qty, line_items.price
```

**Accept new leads over HTTP and write them straight into Postgres.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M8; features=CREATE ENDPOINT,FROM,INSERT INTO,VALUES,RETURNING
# POST /leads ingests the JSON body and returns the created row's id
CREATE ENDPOINT POST /leads AS
  FROM REQUEST.body
  |> INSERT INTO /sql/pg/leads
       VALUES (email => email, name => name, source => source, captured_at => now())
  |> RETURNING id
```

**Publish a daily KPI summary as a cached JSON endpoint.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M8; features=CREATE ENDPOINT,FROM,AGGREGATE,GROUP BY,SELECT,ENCODE
# GET /metrics/daily aggregates today's orders by channel and emits JSON
CREATE ENDPOINT GET /metrics/daily AS
  FROM /sql/pg/orders
  |> WHERE placed_at >= date_trunc('day', now())
  |> AGGREGATE count() AS orders, sum(total) AS revenue GROUP BY channel
  |> ORDER BY revenue DESC
  |> ENCODE json
```

**Forward every new inbox message from a key account into Slack.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M8; features=CREATE TRIGGER,ON,WHERE,DO,CALL,NEW
# Fires on inbox arrival; posts a one-line digest to the sales channel
CREATE TRIGGER inbox_to_slack ON INSERT INTO /mail/inbox
  WHERE NEW.from LIKE '%@bigcorp.com'
  DO CALL slack.post(channel => '#sales',
                     text => 'Mail from ' || NEW.from || ': ' || NEW.subject)
```

**Open a GitHub issue whenever a high-severity error lands in the log table.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M8; features=CREATE TRIGGER,ON,WHERE,DO,INSERT INTO,VALUES,NEW
# Severity >= 4 rows become tracked issues automatically
CREATE TRIGGER fatal_to_issue ON INSERT INTO /sql/pg/error_log
  WHERE NEW.severity >= 4
  DO INSERT INTO /github/acme/web/issues
       VALUES (title => '[auto] ' || NEW.service || ': ' || NEW.message,
               body => NEW.stacktrace,
               labels => 'incident,auto')
```

**Deploy automatically when a PR merges into the main branch.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M8; features=CREATE TRIGGER,ON,WHERE,AND,DO,CALL,NEW
# PR-merged on main → dispatch the production deploy workflow
CREATE TRIGGER deploy_on_merge ON UPDATE /github/acme/web/pulls
  WHERE NEW.merged = true AND NEW.base = 'main'
  DO CALL ci.dispatch(workflow => 'deploy-prod', ref => NEW.merge_commit_sha)
```

**Mirror every Slack file upload into S3 for retention.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M8; features=CREATE TRIGGER,ON,DO,CALL,NEW
# Each new file in #releases is copied to the archive bucket
CREATE TRIGGER archive_slack_files ON INSERT INTO /slack/eng/releases/files
  DO CALL s3.copy(from => NEW.url_private, to => '/s3/archive/slack/' || NEW.id)
```

**Greet new sign-ups with a welcome draft the moment they register.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M8; features=CREATE TRIGGER,ON,WHERE,DO,INSERT INTO,VALUES,NEW,||
# Reversible (draft only), so it runs unattended
CREATE TRIGGER welcome_new_user ON INSERT INTO /sql/pg/users
  WHERE NEW.verified = true
  DO INSERT INTO /mail/drafts
       VALUES (to => NEW.email,
               subject => 'Welcome aboard, ' || NEW.name,
               body => 'Thanks for joining, ' || NEW.name || '. Here is how to start...')
```

**Page on-call when a payment fails for a high-value subscription.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M8; features=CREATE TRIGGER,ON,WHERE,AND,DO,CALL,NEW
# Only enterprise-tier failures escalate to the pager channel
CREATE TRIGGER dunning_alert ON INSERT INTO /sql/pg/payment_failures
  WHERE NEW.amount > 1000 AND NEW.tier = 'enterprise'
  DO CALL slack.post(channel => '#billing-urgent',
                     text => 'Payment failed: ' || NEW.account || ' ($' || NEW.amount || ')')
```

**Comment on any PR that touches the database migrations directory.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M8; features=CREATE TRIGGER,ON,WHERE,DO,CALL,NEW,LIKE
# Reminds reviewers to run the migration checklist
CREATE TRIGGER migration_reviewer ON INSERT INTO /github/acme/web/pulls
  WHERE NEW.files LIKE '%db/migrations/%'
  DO CALL github.comment(number => NEW.number,
                         body => 'Touches migrations — please confirm the rollback plan.')
```

**Run a nightly sales report and drop it in Drive as a CSV.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M8; features=CREATE JOB,EVERY,DO,FROM,WHERE,AGGREGATE,GROUP BY,ENCODE,UPSERT INTO,LAST_RUN
# Aggregates everything since the last run and overwrites the rolling file
CREATE JOB nightly_sales EVERY '1 day' DO
  FROM /sql/pg/orders
  |> WHERE placed_at >= LAST_RUN()
  |> AGGREGATE sum(total) AS revenue, count() AS orders GROUP BY region
  |> ORDER BY revenue DESC
  |> ENCODE csv
  |> UPSERT INTO /drive/Reports/nightly-sales.csv
```

**Sweep stale draft orders out of the table every hour.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M8; features=CREATE JOB,EVERY,DO,FROM,WHERE,AND,REMOVE
# Drafts older than a day with no items are garbage-collected
CREATE JOB gc_draft_orders EVERY '1 hour' DO
  FROM /sql/pg/orders
  |> WHERE status = 'draft' AND created_at < now() - interval '1 day'
  |> REMOVE
```

**Send a Monday-morning digest of last week's merged PRs.**

```qfs
# qfs-cookbook: grammar=core; milestone=M8; features=CREATE JOB,EVERY,DO,FROM,WHERE,AND,SELECT,ENCODE,CALL,LAST_RUN
# Reads everything merged since the previous run and posts the list
CREATE JOB weekly_pr_digest EVERY '1 week' DO
  FROM /github/acme/web/pulls
  |> WHERE merged = true AND merged_at >= LAST_RUN()
  |> ORDER BY merged_at DESC
  |> SELECT number, title, author
  |> CALL slack.post(channel => '#eng-weekly', text => 'Shipped last week:')
```

**Refresh a Google Analytics snapshot into Postgres every morning.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M8; features=CREATE JOB,EVERY,DO,FROM,SELECT,UPSERT INTO,VALUES
# Pulls yesterday's top pages and upserts them for the BI layer
CREATE JOB ga_snapshot EVERY '1 day' DO
  FROM /ga/123456/report
  |> WHERE date = 'yesterday'
  |> ORDER BY pageviews DESC
  |> LIMIT 100
  |> UPSERT INTO /sql/pg/ga_top_pages
       VALUES (page => path, views => pageviews, snapshot_date => date)
```

**Back up a critical table to versioned S3 nightly.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M8; features=CREATE JOB,EVERY,DO,FROM,ENCODE,UPSERT INTO
# Full-table jsonl dump to a dated key in the backup bucket
CREATE JOB backup_accounts EVERY '1 day' DO
  FROM /sql/pg/accounts
  |> ENCODE jsonl
  |> UPSERT INTO /s3/backups/accounts/latest.jsonl
```

**Expire and remove KV session keys that have gone idle.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M8; features=CREATE JOB,EVERY,DO,FROM,WHERE,REMOVE
# Runs every fifteen minutes to keep the namespace lean
CREATE JOB prune_sessions EVERY '15 minutes' DO
  FROM /kv/sessions
  |> WHERE last_seen < now() - interval '30 minutes'
  |> REMOVE
```

**Re-poll an external status page and queue any new incidents.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M8; features=CREATE JOB,EVERY,DO,FROM,WHERE,SELECT,INSERT INTO,VALUES,LAST_RUN
# Bridges a read-only source into a work queue
CREATE JOB poll_incidents EVERY '5 minutes' DO
  FROM /sql/pg/upstream_incidents
  |> WHERE detected_at >= LAST_RUN()
  |> SELECT id, severity, summary
  |> INSERT INTO /queues/incident-intake
       VALUES (incident_id => id, severity => severity, note => summary)
```

**Ingest an inbound webhook payload directly into a SQL audit table.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M8; features=CREATE WEBHOOK,AT,DO,FROM,DECODE,INSERT INTO,VALUES
# Stripe-style hook → decode JSON body → persist the event
CREATE WEBHOOK stripe_events AT /hooks/stripe DO
  FROM REQUEST.body
  |> DECODE json
  |> INSERT INTO /sql/pg/stripe_events
       VALUES (event_id => id, kind => type, payload => data, received_at => now())
```

**Turn GitHub push webhooks into a deploy queue entry.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M8; features=CREATE WEBHOOK,AT,DO,FROM,DECODE,WHERE,INSERT INTO,VALUES
# Only pushes to main get queued for deployment
CREATE WEBHOOK gh_push AT /hooks/github/push DO
  FROM REQUEST.body
  |> DECODE json
  |> WHERE ref = 'refs/heads/main'
  |> INSERT INTO /queues/deploys
       VALUES (sha => after, pusher => pusher.name, queued_at => now())
```

**Fan an inbound form-submission webhook out to a Slack notice.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M8; features=CREATE WEBHOOK,AT,DO,FROM,DECODE,CALL,||
# Marketing landing-page form → instant team ping
CREATE WEBHOOK contact_form AT /hooks/contact DO
  FROM REQUEST.body
  |> DECODE json
  |> CALL slack.post(channel => '#leads',
                     text => 'New contact from ' || name || ' <' || email || '>')
```

**Accept a CI status webhook and update the matching deploy record.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M8; features=CREATE WEBHOOK,AT,DO,FROM,DECODE,WHERE,UPDATE,SET
# Reconciles external CI state back into our own table
CREATE WEBHOOK ci_status AT /hooks/ci DO
  FROM /sql/pg/deploys
  |> WHERE run_id = REQUEST.body.run_id
  |> UPDATE SET status = REQUEST.body.conclusion, finished_at = now()
```

**Define a live view of open enterprise tickets across services.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M8; features=CREATE VIEW,AS,FROM,WHERE,AND,JOIN,SELECT
# A non-materialized view re-runs on every read
CREATE VIEW /views/enterprise_open_tickets AS
  FROM /sql/pg/tickets
  |> WHERE status = 'open'
  |> JOIN /sql/pg/accounts ON accounts.id = tickets.account_id
  |> WHERE accounts.tier = 'enterprise'
  |> SELECT tickets.id, tickets.subject, accounts.name, tickets.opened_at
```

**Expose a unified activity feed view stitching GitHub and Slack.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M8; features=CREATE VIEW,AS,FROM,SELECT,AS,UNION,ORDER BY
# UNION normalizes two sources into one feed shape
CREATE VIEW /views/eng_activity AS
  FROM /github/acme/web/commits
  |> SELECT author AS who, message AS what, committed_at AS at
  |> UNION
     (FROM /slack/eng/general/messages
      |> SELECT user AS who, text AS what, ts AS at)
  |> ORDER BY at DESC
```

**Materialize a cross-service executive dashboard refreshed hourly.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M8; features=CREATE MATERIALIZED VIEW,AS,FROM,AGGREGATE,GROUP BY,JOIN,SELECT
# Heavy join is computed once and served cheaply until next refresh
CREATE MATERIALIZED VIEW /views/exec_dashboard AS
  FROM /sql/pg/orders
  |> AGGREGATE sum(total) AS revenue, count() AS orders GROUP BY region
  |> JOIN /sql/pg/support_load ON support_load.region = orders.region
  |> SELECT region, revenue, orders, support_load.open_tickets
```

**Materialize a denormalized customer-360 table from three services.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M8; features=CREATE MATERIALIZED VIEW,AS,FROM,JOIN,SELECT,ORDER BY
# Joins CRM, billing, and analytics into one wide row per customer
CREATE MATERIALIZED VIEW /views/customer_360 AS
  FROM /sql/pg/customers
  |> JOIN /sql/pg/subscriptions ON subscriptions.customer_id = customers.id
  |> JOIN /sql/pg/ga_top_pages ON ga_top_pages.customer_id = customers.id
  |> SELECT customers.id, customers.name, subscriptions.plan,
            subscriptions.mrr, ga_top_pages.views
  |> ORDER BY subscriptions.mrr DESC
```

**Materialize a daily error-rate rollup for the SLO dashboard.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M8; features=CREATE MATERIALIZED VIEW,AS,FROM,WHERE,AGGREGATE,GROUP BY,EXTEND
# Pre-computes the ratio so the dashboard endpoint stays trivial
CREATE MATERIALIZED VIEW /views/error_rate AS
  FROM /sql/pg/requests
  |> WHERE ts >= now() - interval '7 days'
  |> AGGREGATE count() AS total, sum(is_error) AS errors GROUP BY date_trunc('day', ts) AS day
  |> EXTEND rate = errors * 1.0 / total
```

**Serve the materialized dashboard back out through an endpoint.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M8; features=CREATE ENDPOINT,AS,FROM,ORDER BY,ENCODE
# Endpoint reads the precomputed view, so requests are cheap
CREATE ENDPOINT GET /dashboard/exec AS
  FROM /views/exec_dashboard
  |> ORDER BY revenue DESC
  |> ENCODE json
```

**Escalate aging open incidents to email every six hours.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M8; features=CREATE JOB,EVERY,DO,FROM,WHERE,AND,INSERT INTO,VALUES,||
# Reversible escalation (draft) for unresolved incidents past SLA
CREATE JOB escalate_incidents EVERY '6 hours' DO
  FROM /sql/pg/incidents
  |> WHERE status = 'open' AND opened_at < now() - interval '4 hours'
  |> INSERT INTO /mail/drafts
       VALUES (to => 'incident-mgr@acme.com',
               subject => 'SLA breach: incident ' || id,
               body => 'Open ' || (now() - opened_at) || ' — ' || summary)
```

## The /sys admin surface

Administering qfs is not a separate console — it is the same query language pointed at `/sys`.
Users, accounts, policies, connections, audit, projects, approvals, metrics, and billing are all
just paths, so you grant a role with `INSERT`, revoke with `REMOVE`, inspect who did what with a
`FROM /sys/audit |> WHERE …`, and observe load with `FROM /sys/metrics`. One safety invariant runs
through everything below: **`/sys/connections` describes connections by name and metadata only — it
never returns the secret, token, or password behind a connection.** You list, name, scope, and
revoke connections as data; you never read their credentials, because there is no column that holds
them. Listing/granting/revoking and audit reads land in M3 (the `/sys` driver); policies, projects,
and directory membership in M5; approvals, metrics, and billing in M9/M+.

**List every active human user and the role they currently hold.**

```qfs
# qfs-cookbook: grammar=core; milestone=M3; features=FROM,WHERE,SELECT,ORDER BY
FROM /sys/users
|> WHERE status = 'active' AND kind = 'human'
|> SELECT email, display_name, role, last_seen_at
|> ORDER BY last_seen_at DESC
```

**Invite a new teammate as a read-only member.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M3; features=INSERT INTO,VALUES,RETURNING
INSERT INTO /sys/users
  VALUES (email => 'dana@acme.com',
          display_name => 'Dana Reyes',
          role => 'viewer',
          status => 'invited')
|> RETURNING email, role, status
```

**Promote a viewer to editor on their team.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M3; features=FROM,WHERE,UPDATE,SET,RETURNING
FROM /sys/users
|> WHERE email = 'dana@acme.com' AND role = 'viewer'
|> UPDATE SET role = 'editor'
|> RETURNING email, role
```

**Off-board someone the moment they leave — revoke their account.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M3; features=FROM,WHERE,REMOVE
FROM /sys/users
|> WHERE email = 'former.staff@acme.com'
|> REMOVE
```

**Find dormant accounts that have not signed in for 90 days.**

```qfs
# qfs-cookbook: grammar=core; milestone=M3; features=FROM,WHERE,SELECT,ORDER BY
FROM /sys/users
|> WHERE last_seen_at < '2026-03-28' AND status = 'active'
|> SELECT email, display_name, role, last_seen_at
|> ORDER BY last_seen_at
```

**Audit which accounts still hold admin and were granted it by whom.**

```qfs
# qfs-cookbook: grammar=core; milestone=M3; features=FROM,WHERE,SELECT
FROM /sys/accounts
|> WHERE role = 'admin'
|> SELECT email, granted_by, granted_at, scope
```

**Grant a service account scoped to a single project.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M3; features=INSERT INTO,VALUES,RETURNING
INSERT INTO /sys/accounts
  VALUES (kind => 'service',
          name => 'ci-bot',
          role => 'editor',
          scope => 'project:web-frontend')
|> RETURNING name, role, scope
```

**List the policies governing a sensitive SQL table.**

```qfs
# qfs-cookbook: grammar=core; milestone=M5; features=FROM,WHERE,SELECT,ORDER BY
FROM /sys/policies
|> WHERE resource LIKE '/sql/pg/payroll%'
|> SELECT name, principal, effect, columns, row_filter
|> ORDER BY name
```

**Attach a row+column policy that masks PII for the support role.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M5; features=INSERT INTO,VALUES
# Support sees the customer table but only their own region's rows, no SSN column.
INSERT INTO /sys/policies
  VALUES (name => 'support-customers-eu',
          principal => 'role:support',
          resource => '/sql/pg/customers',
          effect => 'allow',
          columns => 'id,name,email,region',
          row_filter => "region = 'EU'")
```

**Revoke a policy that has become too permissive.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M5; features=FROM,WHERE,REMOVE
FROM /sys/policies
|> WHERE name = 'legacy-allow-all-finance'
|> REMOVE
```

**Inspect every connection by name and metadata — no secrets are ever returned.**

```qfs
# qfs-cookbook: grammar=core; milestone=M3; features=FROM,SELECT,ORDER BY
# /sys/connections has no credential column; you see scope and status, never the token.
FROM /sys/connections
|> SELECT name, driver, scope, status, created_by, last_used_at
|> ORDER BY driver, name
```

**Find connections that have gone stale and were never used.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M3; features=FROM,WHERE,SELECT,ORDER BY
FROM /sys/connections
|> WHERE last_used_at IS NULL OR last_used_at < '2026-01-01'
|> SELECT name, driver, created_by, created_at, last_used_at
|> ORDER BY created_at
```

**Register a new connection by reference — credentials are supplied out of band, not in the query.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M3; features=INSERT INTO,VALUES,RETURNING
# The row records the connection's identity and scope; the secret lives in the vault, not here.
INSERT INTO /sys/connections
  VALUES (name => 'acme-pg-readonly',
          driver => 'sql/pg',
          scope => 'project:analytics',
          secret_ref => 'vault://acme/pg-readonly')
|> RETURNING name, driver, scope, status
```

**Disconnect a third-party integration cleanly.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M3; features=FROM,WHERE,REMOVE
FROM /sys/connections
|> WHERE name = 'old-zendesk' AND driver = 'http'
|> REMOVE
```

**Audit who removed or deleted anything in the last 24 hours.**

```qfs
# qfs-cookbook: grammar=core; milestone=M3; features=FROM,WHERE,SELECT,ORDER BY
FROM /sys/audit
|> WHERE verb = 'REMOVE' AND at > '2026-06-25T00:00:00Z'
|> SELECT at, actor, verb, resource, committed
|> ORDER BY at DESC
```

**Trace every irreversible CALL a single actor made this week.**

```qfs
# qfs-cookbook: grammar=core; milestone=M3; features=FROM,WHERE,SELECT,ORDER BY
FROM /sys/audit
|> WHERE actor = 'dana@acme.com'
     AND verb = 'CALL'
     AND at BETWEEN '2026-06-22T00:00:00Z' AND '2026-06-29T00:00:00Z'
|> SELECT at, procedure, resource, committed
|> ORDER BY at
```

**Build a per-actor activity leaderboard for the audit log.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M3; features=FROM,WHERE,AGGREGATE,GROUP BY,ORDER BY
FROM /sys/audit
|> WHERE at > '2026-06-01T00:00:00Z'
|> AGGREGATE count(*) AS actions, count_distinct(resource) AS resources_touched
   GROUP BY actor
|> ORDER BY actions DESC
```

**Reconcile audit entries with the connections that served them.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M3; features=FROM,JOIN,WHERE,SELECT,ORDER BY
FROM /sys/audit
|> JOIN /sys/connections ON /sys/audit.connection = /sys/connections.name
|> WHERE /sys/audit.at > '2026-06-20T00:00:00Z'
|> SELECT /sys/audit.at, /sys/audit.actor, /sys/audit.verb,
          /sys/connections.driver, /sys/connections.scope
|> ORDER BY /sys/audit.at DESC
```

**List every project and how many members each has.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M5; features=FROM,JOIN,AGGREGATE,GROUP BY,ORDER BY
FROM /sys/projects
|> JOIN /sys/projects/members ON /sys/projects.id = /sys/projects/members.project_id
|> AGGREGATE count(*) AS members GROUP BY /sys/projects.name
|> ORDER BY members DESC
```

**Spin up a new project.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M5; features=INSERT INTO,VALUES,RETURNING
INSERT INTO /sys/projects
  VALUES (name => 'q3-migration',
          owner => 'lead@acme.com',
          visibility => 'private')
|> RETURNING id, name, owner
```

**Add a teammate to a project as a contributor.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M5; features=INSERT INTO,VALUES,RETURNING
INSERT INTO /sys/projects/members
  VALUES (project_id => 'q3-migration',
          email => 'dana@acme.com',
          project_role => 'contributor')
|> RETURNING project_id, email, project_role
```

**Remove a former contributor from a project without deleting their account.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M5; features=FROM,WHERE,REMOVE
FROM /sys/projects/members
|> WHERE project_id = 'q3-migration' AND email = 'rotated.off@acme.com'
|> REMOVE
```

**Surface every change request still waiting on a second human to sign off.**

```qfs
# qfs-cookbook: grammar=core; milestone=M+; features=FROM,WHERE,SELECT,ORDER BY
# Approvals are data: a pending row needs a different person to approve it.
FROM /sys/approvals
|> WHERE status = 'pending'
|> SELECT id, requested_by, action, resource, requested_at
|> ORDER BY requested_at
```

**Sign off on a pending approval — a second human approves the row.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M+; features=FROM,WHERE,UPDATE,SET,RETURNING
# The approver must differ from requested_by; the engine enforces four-eyes at commit.
FROM /sys/approvals
|> WHERE id = 'apr-8842' AND status = 'pending'
|> UPDATE SET status = 'approved', approved_by = 'security-lead@acme.com'
|> RETURNING id, action, status, approved_by
```

**Audit self-approvals — find rows approved by the same person who requested them.**

```qfs
# qfs-cookbook: grammar=core; milestone=M+; features=FROM,WHERE,SELECT,ORDER BY
FROM /sys/approvals
|> WHERE status = 'approved' AND approved_by = requested_by
|> SELECT id, action, resource, requested_by, approved_at
|> ORDER BY approved_at DESC
```

**Watch the slowest drivers by p95 latency right now.**

```qfs
# qfs-cookbook: grammar=core; milestone=M+; features=FROM,WHERE,SELECT,ORDER BY,LIMIT
FROM /sys/metrics
|> WHERE window = '5m' AND metric = 'query_latency_p95'
|> SELECT driver, value, unit, at
|> ORDER BY value DESC
|> LIMIT 10
```

**Roll up query volume per driver over the last hour.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M+; features=FROM,WHERE,AGGREGATE,GROUP BY,ORDER BY
FROM /sys/metrics
|> WHERE metric = 'query_count' AND at > '2026-06-26T00:00:00Z'
|> AGGREGATE sum(value) AS total_queries GROUP BY driver
|> ORDER BY total_queries DESC
```

**Review this month's billable usage broken down by project.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M+; features=FROM,WHERE,AGGREGATE,GROUP BY,ORDER BY
FROM /sys/billing
|> WHERE period = '2026-06'
|> AGGREGATE sum(amount) AS spend, sum(units) AS units GROUP BY project, sku
|> ORDER BY spend DESC
```

## AI over MCP

When qfs is mounted as an MCP server, an AI agent stops calling a dozen bespoke service APIs and instead speaks one language: it turns a teammate's plain-English ask into a single qfs statement, then runs it through the describe→preview→commit loop. **describe** tells the model what a path is and which columns exist; **preview** dry-runs the statement and reports its effect class — a read returns "reads only, 0 effects" and is safe to show immediately, a reversible write reports the draft or UPSERT it would make, and an irreversible step reports that it would send mail or merge a PR. The agent's job is text-to-SQL on the client side; qfs's job is to make the consequence of each statement legible before anything happens. Under the **default safety mode** reversible writes auto-commit within `POLICY` while irreversible `CALL`s wait for a human to approve, but the mode is selectable — a read-only "explore" mode refuses every effect, a "draft" mode lets reversible writes through, and a supervised "auto" mode can be granted narrow irreversible scopes — so the same generated statement is gated differently depending on how much the operator has delegated. Every recipe below is one statement the agent emitted in response to the natural-language title; the prose around the loop is the model's, the qfs is the contract.

**"What inboxes and databases am I even connected to?"**

```qfs
# qfs-cookbook: grammar=core; milestone=M2; features=FROM,SELECT
# Pure read: preview reports "reads only, 0 effects". Names/metadata only — no secrets.
FROM /sys/connections
|> SELECT service, name, scopes
```

**"Which of my connections can actually write, not just read?"**

```qfs
# qfs-cookbook: grammar=core; milestone=M2; features=FROM,WHERE,SELECT,LIKE
# The agent inspects scope metadata only; preview confirms it touches nothing.
FROM /sys/connections
|> WHERE scopes LIKE '%write%'
|> SELECT service, name, scopes
```

**"Show me the GitHub and Slack accounts this agent is allowed to act as."**

```qfs
# qfs-cookbook: grammar=core; milestone=M2; features=FROM,WHERE,SELECT,IN,ORDER BY
FROM /sys/connections
|> WHERE service IN ('github', 'slack')
|> SELECT service, name, scopes
|> ORDER BY service
```

**"Pull the ten most recent unread emails so I can triage them."**

```qfs
# qfs-cookbook: grammar=core; milestone=M2; features=FROM,WHERE,SELECT,ORDER BY,LIMIT
# Exploratory read — preview: "reads only, 0 effects". The agent shows the table inline.
FROM /mail/inbox
|> WHERE unread = true
|> SELECT from_addr, subject, received_at
|> ORDER BY received_at DESC
|> LIMIT 10
```

**"How many open PRs does each reviewer have on their plate right now?"**

```qfs
# qfs-cookbook: grammar=extended; milestone=M2; features=FROM,WHERE,AGGREGATE,GROUP BY,ORDER BY
FROM /github/acme/web/pulls
|> WHERE state = 'open'
|> AGGREGATE count() AS open_prs GROUP BY requested_reviewer
|> ORDER BY open_prs DESC
```

**"What did the support channel talk about most this week?"**

```qfs
# qfs-cookbook: grammar=extended; milestone=M2; features=FROM,WHERE,AGGREGATE,GROUP BY,ORDER BY,LIMIT
FROM /slack/acme/support/messages
|> WHERE ts > '2026-06-19'
|> AGGREGATE count() AS msgs GROUP BY user
|> ORDER BY msgs DESC
|> LIMIT 5
```

**"Find customers who churned last quarter but still have an open invoice."**

```qfs
# qfs-cookbook: grammar=core; milestone=M2; features=FROM,WHERE,JOIN,ON,SELECT
# Cross-service read; preview reports zero effects, so the agent answers directly.
FROM /sql/pg/customers
|> WHERE churned_at BETWEEN '2026-01-01' AND '2026-03-31'
|> JOIN /sql/pg/invoices ON customers.id = invoices.customer_id
|> WHERE invoices.status = 'open'
|> SELECT customers.name, customers.email, invoices.amount, invoices.due_date
```

**"Read the Q2 plan in Drive and give me its objectives."**

```qfs
# qfs-cookbook: grammar=core; milestone=M2; features=FROM,DECODE,SELECT
# Blob→relational read. Still "reads only, 0 effects" — DECODE doesn't write.
FROM /drive/Plans/q2-plan.md
|> DECODE md
|> SELECT owner, quarter, body
```

**"Diff the deploy config between the release tag and main."**

```qfs
# qfs-cookbook: grammar=core; milestone=M2; features=FROM,DECODE,SELECT,EXCEPT
FROM /git/app@v2.1/deploy.toml
|> DECODE toml
|> SELECT key, value
|> EXCEPT
   FROM /git/app@main/deploy.toml
   |> DECODE toml
   |> SELECT key, value
```

**"Which assets in S3 aren't recorded in the catalog table?"**

```qfs
# qfs-cookbook: grammar=core; milestone=M2; features=FROM,SELECT,EXCEPT
FROM /s3/media-prod
|> SELECT key
|> EXCEPT
   FROM /sql/pg/assets
   |> SELECT storage_key AS key
```

**"Save this triage summary as an email draft for me to look over."**

```qfs
# qfs-cookbook: grammar=extended; milestone=M2; features=INSERT INTO,VALUES,||
# Reversible write. Under the default mode the draft auto-commits within POLICY;
# preview reports "1 reversible effect: draft created in /mail/drafts".
INSERT INTO /mail/drafts
  VALUES (to => 'lead@acme.com',
          subject => 'Inbox triage — ' || '2026-06-26',
          body => 'Drafted by the assistant. 10 unread, 3 urgent. Review before sending.')
```

**"Draft a reply asking each overdue-invoice customer to settle up."**

```qfs
# qfs-cookbook: grammar=extended; milestone=M2; features=FROM,WHERE,INSERT INTO,VALUES,||
# Reversible: drafts only. Nothing is sent — CALL mail.send would be a separate, gated step.
FROM /sql/pg/invoices
|> WHERE status = 'open' AND due_date < '2026-06-01'
|> INSERT INTO /mail/drafts
     VALUES (to => customer_email,
             subject => 'Invoice ' || invoice_no || ' is past due',
             body => 'Hi — invoice ' || invoice_no || ' for ' || amount || ' is overdue.')
```

**"Upsert today's MRR snapshot into the metrics rollup table."**

```qfs
# qfs-cookbook: grammar=extended; milestone=M2; features=FROM,AGGREGATE,EXTEND,UPSERT INTO
# UPSERT is reversible (key-addressed); auto-commits within POLICY under default mode.
FROM /sql/pg/subscriptions
|> WHERE status = 'active'
|> AGGREGATE sum(mrr) AS mrr GROUP BY plan
|> EXTEND snapshot_date = '2026-06-26'
|> UPSERT INTO /sql/pg/mrr_daily
```

**"Mirror the weekly report into the shared Drive folder as a CSV."**

```qfs
# qfs-cookbook: grammar=extended; milestone=M2; features=FROM,WHERE,ENCODE,UPSERT INTO
# Reversible UPSERT to storage — safe to auto-commit; preview shows the target key.
FROM /sql/pg/weekly_report
|> WHERE week = '2026-W26'
|> ENCODE csv
|> UPSERT INTO /drive/Reports/weekly-2026-W26.csv
```

**"Tag every stale open issue so the backlog grooming bot picks them up."**

```qfs
# qfs-cookbook: grammar=extended; milestone=M2; features=FROM,WHERE,UPDATE,SET
# Reversible field update; the preview lists the affected issue numbers before commit.
FROM /github/acme/web/issues
|> WHERE state = 'open' AND updated_at < '2026-03-26'
|> UPDATE SET label = 'stale'
```

**"Drop the processed upload keys from the queue table."**

```qfs
# qfs-cookbook: grammar=extended; milestone=M2; features=FROM,WHERE,REMOVE
# REMOVE is irreversible: preview enumerates the exact rows it would delete, and the
# commit needs --commit-irreversible. (It cannot appear inside a TRANSACTION, which is
# reversible-only.) Reach for UPDATE SET a soft-delete flag if you need it undoable.
FROM /sql/pg/upload_queue
|> WHERE status = 'processed'
|> REMOVE
```

**"Stage the new pricing file into the staging bucket from the source one."**

```qfs
# qfs-cookbook: grammar=extended; milestone=M2; features=FROM,ENCODE,UPSERT INTO
FROM /s3/source/pricing-2026.json
|> DECODE json
|> ENCODE json
|> UPSERT INTO /s3/staging/pricing-2026.json
```

**"Now actually send those overdue-invoice reminders."**

```qfs
# qfs-cookbook: grammar=extended; milestone=M2; features=FROM,WHERE,CALL,||
# Irreversible: CALL mail.send. Under the default safety mode preview reports
# "irreversible effect — awaiting approval"; the agent surfaces it and waits for a human.
FROM /sql/pg/invoices
|> WHERE status = 'open' AND due_date < '2026-06-01'
|> CALL mail.send(to => customer_email,
                  subject => 'Final reminder: invoice ' || invoice_no,
                  body => 'This invoice is now seriously overdue. Please settle promptly.')
```

**"Merge the release PR — it's approved."**

```qfs
# qfs-cookbook: grammar=extended; milestone=M2; features=CALL
# Irreversible merge. The default mode never auto-commits this; the human approves
# the previewed effect, or runs an "auto" mode that was granted github.merge scope.
CALL github.merge(number => 318, method => 'squash')
```

**"Post the deploy-done note to the releases channel."**

```qfs
# qfs-cookbook: grammar=extended; milestone=M2; features=CALL,||
# Posting to a channel is irreversible (you can't unsay it); preview flags it for approval.
CALL slack.post(channel => 'eng-releases',
                text => 'v2.1 is live in production. ' || 'Rollback runbook is pinned.')
```

**"Kick off the nightly export workflow now instead of waiting."**

```qfs
# qfs-cookbook: grammar=extended; milestone=M2; features=CALL
# Irreversible side effect (it starts real work). Default mode: awaits human approval.
CALL ci.dispatch(workflow => 'nightly-export', ref => 'main')
```

**"Reply on issue 204 that we've shipped the fix."**

```qfs
# qfs-cookbook: grammar=extended; milestone=M2; features=CALL,||
FROM /github/acme/web/issues/204
|> CALL github.comment(number => 204,
                       body => 'Fixed in v2.1, released today. ' || 'Closing once verified.')
```

**"Which inbox messages mention 'refund' and came from a paying customer?"**

```qfs
# qfs-cookbook: grammar=core; milestone=M2; features=FROM,WHERE,JOIN,ON,SELECT,LIKE
# Exploratory cross-service read; preview: "reads only, 0 effects".
FROM /mail/inbox
|> WHERE subject LIKE '%refund%' OR body LIKE '%refund%'
|> JOIN /sql/pg/customers ON inbox.from_addr = customers.email
|> WHERE customers.plan <> 'free'
|> SELECT inbox.from_addr, inbox.subject, customers.plan
```

**"Build the win-back list: active GA visitors who lapsed in the orders table."**

```qfs
# qfs-cookbook: grammar=core; milestone=M2; features=FROM,SELECT,INTERSECT
FROM /ga/acme-prod/sessions
|> WHERE event = 'session_start'
|> SELECT user_email AS email
|> INTERSECT
   FROM /sql/pg/orders
   |> WHERE last_order_at < '2026-03-27'
   |> SELECT email
```

**"Draft win-back emails to that lapsed list, but don't send them yet."**

```qfs
# qfs-cookbook: grammar=extended; milestone=M2; features=FROM,WHERE,INSERT INTO,VALUES,||
# Reversible drafting step; the send is a separate, approval-gated CALL the human runs later.
FROM /sql/pg/orders
|> WHERE last_order_at < '2026-03-27'
|> INSERT INTO /mail/drafts
     VALUES (to => email,
             subject => 'We saved your spot, ' || name,
             body => 'It has been a while, ' || name || '. Here is 20% off to come back.')
```

**"Snapshot the audit log of who changed connections this month into a table."**

```qfs
# qfs-cookbook: grammar=extended; milestone=M2; features=FROM,WHERE,SELECT,UPSERT INTO
FROM /sys/audit
|> WHERE action LIKE 'connection.%' AND at > '2026-06-01'
|> SELECT actor, action, target, at
|> UPSERT INTO /sql/pg/connection_audit_june
```

## Agent fabric & cloud team

Once your machines join the qfs Cloud fabric, every host's running Claude Code sessions become just
another mounted, queryable surface at `/hosts/<host>/claude/sessions` — you read what each agent is doing,
steer it by writing instructions, and fan work across a whole pool of hosts from one prompt. The
team layer adds shared connections at the project level and cross-machine federation, so a
coordinator can JOIN results from agents living on different machines into a single mesh. The tunnel
that carries these calls requires a qfs Cloud sign-in, and every cross-machine read or write is
bounded by POLICY — a host only answers for the rows and paths your role is allowed to reach.

**See which agents on the build box are still grinding and where they are.**

```qfs
# qfs-cookbook: grammar=core; milestone=M7; features=FROM,WHERE,SELECT,ORDER BY
# Live status of every running Claude Code session on one host.
FROM /hosts/buildbox-01/claude/sessions
|> WHERE status = 'running'
|> SELECT task, progress, last_message, started_at
|> ORDER BY progress
```

**Find sessions that have stalled — running but silent for a while.**

```qfs
# qfs-cookbook: grammar=core; milestone=M7; features=FROM,WHERE,SELECT,ORDER BY,AND
FROM /hosts/buildbox-01/claude/sessions
|> WHERE status = 'running' AND last_message_at < '2026-06-26T09:00:00Z'
|> SELECT id, task, last_message, last_message_at
|> ORDER BY last_message_at
```

**Steer one agent: nudge the current session to write tests before it ships.**

```qfs
# qfs-cookbook: grammar=core; milestone=M7; features=INSERT INTO,VALUES
# A reversible write into the session's instruction channel; POLICY bounds who may steer this host.
INSERT INTO /hosts/buildbox-01/claude/sessions/current/instructions
  VALUES ('Add unit tests for the new parser before opening the PR, then run cargo test --workspace.')
```

**Redirect a specific stuck session to a smaller, safer scope.**

```qfs
# qfs-cookbook: grammar=core; milestone=M7; features=INSERT INTO,VALUES
INSERT INTO /hosts/buildbox-01/claude/sessions/sess_9f2a/instructions
  VALUES ('Stop the refactor. Just fix the failing test in tokenizer.rs and report back.')
```

**Pull the last thing every agent said across one host, newest first.**

```qfs
# qfs-cookbook: grammar=core; milestone=M7; features=FROM,SELECT,ORDER BY,LIMIT
FROM /hosts/buildbox-01/claude/sessions
|> SELECT host, id, task, status, last_message
|> ORDER BY last_message_at
|> LIMIT 50
```

**Count how each host's agents are spread across statuses.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M7; features=FROM,AGGREGATE,GROUP BY
FROM /hosts/buildbox-01/claude/sessions
|> AGGREGATE count() AS sessions, max(progress) AS furthest GROUP BY status
```

**Fan one read across the whole pool: every running agent on every host.**

```qfs
# qfs-cookbook: grammar=core; milestone=M+; features=FROM,WHERE,SELECT,ORDER BY
# A wildcard host segment fans the read across the federated fabric; POLICY scopes each host's rows.
FROM /hosts/*/claude/sessions
|> WHERE status = 'running'
|> SELECT host, task, progress, last_message
|> ORDER BY host, progress
```

**Mesh roll-up: how many agents are busy on each machine right now.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M+; features=FROM,WHERE,AGGREGATE,GROUP BY,ORDER BY
FROM /hosts/*/claude/sessions
|> WHERE status = 'running'
|> AGGREGATE count() AS busy, min(progress) AS least_done GROUP BY host
|> ORDER BY busy
```

**Broadcast a steer: tell every idle agent across the fleet to pick up the queue.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M+; features=FROM,WHERE,INSERT INTO,VALUES
# One pipeline writes an instruction into each matched session; the tunnel needs a Cloud sign-in.
FROM /hosts/*/claude/sessions
|> WHERE status = 'idle'
|> INSERT INTO /hosts/*/claude/sessions/instructions
     VALUES (host => host, session => id,
             text => 'Claim the next ticket from /sys/projects/qfs/queue and start it.')
```

**Coordinator: collect finished agents' results from several hosts into one list.**

```qfs
# qfs-cookbook: grammar=core; milestone=M+; features=FROM,WHERE,SELECT,ORDER BY
FROM /hosts/*/claude/sessions
|> WHERE status = 'done'
|> SELECT host, task, result, finished_at
|> ORDER BY finished_at
```

**Merge agent state with the tickets they were assigned (cross-surface JOIN).**

```qfs
# qfs-cookbook: grammar=core; milestone=M+; features=FROM,JOIN,ON,WHERE,SELECT
FROM /hosts/*/claude/sessions
|> JOIN /sql/pg/agent_tasks ON sessions.task_id = agent_tasks.id
|> WHERE sessions.status = 'running'
|> SELECT host, agent_tasks.title, agent_tasks.priority, sessions.progress
```

**Cross-machine federation read: list source files an agent on another host is touching.**

```qfs
# qfs-cookbook: grammar=core; milestone=M+; features=FROM,WHERE,SELECT,ORDER BY
# /git on a remote host resolves over the tunnel; every blob read is POLICY-bounded.
FROM /hosts/gpu-rig-02/claude/sessions/current/changed_files
|> WHERE staged = true
|> SELECT path, additions, deletions
|> ORDER BY additions
```

**Pool the team's open PRs that agents authored across all hosts.**

```qfs
# qfs-cookbook: grammar=core; milestone=M+; features=FROM,WHERE,JOIN,ON,SELECT
FROM /hosts/*/claude/sessions
|> WHERE status = 'done'
|> JOIN /github/acme/web/pulls ON sessions.pr_number = pulls.number
|> SELECT host, pulls.number, pulls.title, pulls.state
```

**Stand up a team-wide connection at the project level so every host shares it.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M9; features=INSERT INTO,VALUES
# A project connection is visible to all hosts in the team, gated by POLICY.
INSERT INTO /sys/projects/qfs/connections
  VALUES (name => 'pg-prod', driver => 'sql/pg',
          dsn => 'postgres://reader@db.acme.internal/app', scope => 'team')
```

**Promote one engineer's personal connection to a shared team connection.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M9; features=FROM,WHERE,INSERT INTO,VALUES
FROM /sys/connections
|> WHERE name = 'slack-eng' AND owner = 'a@qmu.jp'
|> INSERT INTO /sys/projects/qfs/connections
     VALUES (name => name, driver => driver, scope => 'team')
```

**Audit which team connections every host can currently reach.**

```qfs
# qfs-cookbook: grammar=core; milestone=M9; features=FROM,WHERE,SELECT,ORDER BY
FROM /sys/projects/qfs/connections
|> WHERE scope = 'team'
|> SELECT name, driver, owner, last_used_at
|> ORDER BY last_used_at
```

**Check which hosts have actually joined the fabric and when they last reported.**

```qfs
# qfs-cookbook: grammar=core; milestone=M+; features=FROM,WHERE,SELECT,ORDER BY
FROM /sys/projects/qfs/hosts
|> WHERE online = true
|> SELECT host, agent_count, last_heartbeat_at, cloud_signed_in
|> ORDER BY last_heartbeat_at
```

**Reusable filter: define "busy host" once, then count and rank with it (functional core).**

```qfs
# qfs-cookbook: grammar=extended; milestone=M+; features=LET,=>,filter,FROM,AGGREGATE,GROUP BY
LET is_busy = (s: Session) => s.status = 'running' AND s.progress < 0.9
FROM /hosts/*/claude/sessions
|> WHERE filter(self, is_busy)
|> AGGREGATE count() AS still_busy GROUP BY host
```

**Bind the running fleet once, then both summarize it and pick the laggards.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M+; features=LET,FROM,WHERE,JOIN,ON,SELECT
LET running = (FROM /hosts/*/claude/sessions |> WHERE status = 'running')
FROM running
|> JOIN /sys/projects/qfs/hosts ON running.host = hosts.host
|> WHERE hosts.cloud_signed_in = true
|> SELECT running.host, running.task, running.progress, hosts.region
```

**Coordinator fan-out: queue one task per online host as a steerable instruction.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M+; features=FROM,WHERE,INSERT INTO,VALUES,||
FROM /sys/projects/qfs/hosts
|> WHERE online = true
|> INSERT INTO /hosts/*/claude/sessions/instructions
     VALUES (host => host, session => 'next',
             text => 'You are shard ' || host || '. Build and test only crates owned by this host.')
```

**Reduce many agents' progress into a single fleet completion figure.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M+; features=LET,reduce,=>,FROM,WHERE,SELECT
LET avg_progress = (rows: Relation) =>
  reduce(rows.progress, (acc: Float, p: Float) => acc + p, 0.0) / count(rows)
FROM /hosts/*/claude/sessions
|> WHERE status = 'running'
|> SELECT avg_progress(self) AS fleet_progress
```

**Spot conflicts: two agents editing the same file on different hosts.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M+; features=FROM,JOIN,ON,WHERE,SELECT,DISTINCT
FROM /hosts/*/claude/sessions/current/changed_files AS a
|> JOIN /hosts/*/claude/sessions/current/changed_files AS b ON a.path = b.path
|> WHERE a.host <> b.host
|> SELECT DISTINCT a.path, a.host, b.host
```

**Collect every agent's final result and drop it into a shared report blob.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M+; features=FROM,WHERE,SELECT,ENCODE,UPSERT INTO
FROM /hosts/*/claude/sessions
|> WHERE status = 'done'
|> SELECT host, task, result, finished_at
|> ENCODE md
|> UPSERT INTO /drive/Team/agent-run-2026-06-26.md
```

**Post a mesh standup to Slack: who finished what, grouped by host.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M+; features=FROM,WHERE,AGGREGATE,GROUP BY,INSERT INTO,VALUES,||
FROM /hosts/*/claude/sessions
|> WHERE status = 'done'
|> AGGREGATE count() AS shipped GROUP BY host
|> INSERT INTO /slack/acme/eng-fabric/messages
     VALUES (text => host || ' shipped ' || shipped || ' tasks this run.')
```

**Halt the fleet on red: tell every running agent to pause when CI is failing.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M+; features=FROM,WHERE,INSERT INTO,VALUES
FROM /hosts/*/claude/sessions
|> WHERE status = 'running'
|> INSERT INTO /hosts/*/claude/sessions/instructions
     VALUES (host => host, session => id,
             text => 'CI on main is red. Stop pushing and wait for the all-clear.')
```

**Reconcile the mesh against policy: which sessions a remote host refused to expose.**

```qfs
# qfs-cookbook: grammar=core; milestone=M+; features=FROM,WHERE,SELECT,ORDER BY
# Cross-machine reads are POLICY-bounded; the audit log records what each host withheld.
FROM /sys/audit
|> WHERE action = 'fabric.read' AND outcome = 'denied'
|> SELECT host, principal, path, reason, at
|> ORDER BY at
```

**Cross-machine federation: JOIN one agent's commits with the team's PR review state.**

```qfs
# qfs-cookbook: grammar=core; milestone=M+; features=FROM,JOIN,ON,WHERE,SELECT,ORDER BY
FROM /hosts/gpu-rig-02/claude/sessions/current/commits
|> JOIN /github/acme/web/pulls ON commits.sha = pulls.head_sha
|> WHERE pulls.review_state <> 'approved'
|> SELECT commits.sha, commits.message, pulls.number, pulls.review_state
|> ORDER BY commits.committed_at
```

**Rank hosts by how much idle capacity they have for the coordinator to schedule.**

```qfs
# qfs-cookbook: grammar=core; milestone=M+; features=FROM,JOIN,ON,SELECT,ORDER BY
FROM /sys/projects/qfs/hosts
|> JOIN /sys/metrics ON hosts.host = metrics.host
|> SELECT hosts.host, hosts.agent_count, metrics.cpu_idle_pct, metrics.free_mem_gb
|> ORDER BY metrics.cpu_idle_pct
```
