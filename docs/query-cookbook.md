---
aside: false
---

# The qfs query cookbook

[[toc]]

A broad, worked-by-example catalogue of qfs queries, in the grammar you would actually type. See
[How qfs works](/guide/concepts) for the model behind them. The recipes deliberately **combine
features and interact** — a federation join feeding a transaction, a policy driven by a directory
group, an agent's MCP commit gated by a safety mode — because the interactions are the product.

::: warning Worked recipes, with a few running ahead of the parser
Every recipe is tagged by the capability that delivers it. The [generated reference](/language) is
always the truth about the binary *today*. Two things are now true of this catalogue. First, every
`grammar=core` recipe **parses** against today's frozen grammar — the ratchet test below proves it.
Second, many of the **reads actually execute and return rows**: `/local` (directory listings,
single-file content, and the `decode`/`encode` codecs), `/sys`, a configured `/sql` connection
(`QFS_SQL_<name>`), and a configured `/git` repository (`QFS_GIT_<name>`) all run for real and hand
back data. The cloud-driver reads (`/mail`, `/drive`, `/github`, `/slack`, `/s3`, `/r2`) parse and
plan, but **executing** them needs a connected account — without one they return an actionable
"connect a … account" error (run `qfs identity signup <email>`, then `qfs connection add <service>`),
never an opaque unknown-source failure; `/drive` and `/ga` reads ride that same connect-account path
but are still coming soon. Bare write statements (`insert into /mail/drafts …`, `update /sql/…`)
**preview** their effect and touch nothing. On top of all that, a handful of recipes use grammar the
parser does not ship yet — they are tagged so you can tell which is which. Each ` ```qfs ` block
carries a machine-readable header
comment — `# qfs-cookbook: grammar=core|extended; milestone=…; features=…` — and a test
(`packages/qfs/crates/test/tests/roadmap_cookbook.rs`) extracts every block: **`grammar=core` recipes must parse
with today's parser** (so the catalogue can never drift from the real grammar), while
`grammar=extended` recipes need a construct the parser does not ship yet — the M6 functional core
(`LET`, lambdas, `TRANSACTION`) *and* other not-yet-shipped grammar (inline `GROUP BY`/`AGGREGATE`,
`||`, source `AS` aliases, richer DDL, …) — and are tracked as a living coverage report, each promoted
to a hard assertion as the milestone that delivers it lands. `grammar=core` means *the grammar of
the statement* is shipped — a recipe may still address a path that is not yet executable here:
a planned admin mount whose driver arrives later (`/hosts`, `/directories`), or a cloud read that
needs a connected account before it returns rows (`/mail`, `/drive`, `/github`, `/slack`, `/s3`,
`/r2`), since a path is just a token to the parser.
:::

The safety floor holds in every recipe: **describe is pure, preview touches nothing, commit is
explicit, and irreversible effects (`CALL mail.send`, merges, deletes) demand
`--commit-irreversible`** — even when a line is not spelled out, that is the contract it runs under.

## Reads, filters & projection

The simplest thing qfs does is read one source, narrow it with a predicate, and shape the columns you keep — and that single skill works identically against a mailbox, a Postgres table, a GitHub repo, a Slack channel, a git tree, an object store, or a planned admin mount. These recipes stay inside one source per query and lean on the read vocabulary: `WHERE`/`SELECT`/`EXTEND`/`ORDER BY`/`LIMIT`/`DISTINCT`, the full predicate set (`LIKE`, `~`, `IN`, `BETWEEN`, `ANY`, `AND`/`OR`/`NOT`), dotted path navigation into nested objects, `EXPAND` of nested collections, and the temporal coordinate (`@version` and `AS OF`). Everything here is read-only, so nothing is ever touched. The `/local`, `/sys`, and configured `/sql`/`/git` reads return real rows today; the cloud-service reads (`/mail`, `/github`, `/slack`, `/s3`, `/r2`, `/drive`) parse and plan but first need a connected account, after which `PREVIEW` shows exactly what would come back.

**Triage the inbox: unread mail from outside the company, newest first.**

```qfs
# qfs-cookbook: grammar=core; milestone=M2; features=where,NOT,LIKE,AND,select,order by,limit
/mail/inbox
|> where is_read == false
     AND NOT from LIKE '%@acme.com'
|> select from, subject, received_at
|> order by received_at DESC
|> limit 50
```

**Find invoices that are large, recent, and still unpaid.**

```qfs
# qfs-cookbook: grammar=core; milestone=M0; features=where,BETWEEN,AND,IN,order by
/sql/pg/invoices
|> where amount_due BETWEEN 5000 AND 250000
     AND status IN ('open', 'overdue')
     AND issued_at > '2026-04-01'
|> order by amount_due DESC
```

**List open pull requests authored by the platform team.**

```qfs
# qfs-cookbook: grammar=core; milestone=M1; features=where,IN,AND,select,order by
/github/acme/web/pulls
|> where state == 'open'
     AND author IN ('rin', 'kenji', 'sora', 'mei')
|> select number, title, author, created_at
|> order by created_at ASC
```

**Search a Slack channel for anything that looks like an incident.**

```qfs
# qfs-cookbook: grammar=core; milestone=M2; features=where,~,OR,select,order by,limit
/slack/acme/incidents/messages
|> where text ~ '(?i)(outage|sev[0-9]|rollback|paging)'
     OR text LIKE '%down%'
|> select ts, user, text
|> order by ts DESC
|> limit 100
```

**Show the most recent commits touching the auth module.**

```qfs
# qfs-cookbook: grammar=core; milestone=M1; features=where,LIKE,AND,select,order by,limit
/git/app/commits
|> where files LIKE '%src/auth/%'
     AND committed_at > '2026-05-01'
|> select sha, author, subject, committed_at
|> order by committed_at DESC
|> limit 25
```

**List the largest objects in an S3 bucket prefix.**

```qfs
# qfs-cookbook: grammar=core; milestone=M0; features=where,LIKE,AND,select,order by,limit
/s3/acme-backups
|> where key LIKE 'db-dumps/%'
     AND size > 1073741824
|> select key, size, last_modified, storage_class
|> order by size DESC
|> limit 20
```

**Find quarterly report PDFs in a Drive folder, biggest first.**

```qfs
# qfs-cookbook: grammar=core; milestone=M0; features=where,LIKE,AND,select,order by
/drive/Reports
|> where name LIKE '%.pdf'
     AND name ~ 'Q[1-4]'
|> select name, size, modified_at, owner
|> order by size DESC
```

**Pull the top landing pages by sessions for last month.**

```qfs
# qfs-cookbook: grammar=core; milestone=M4; features=where,AND,select,order by,limit
/ga/acme.com/pages
|> where date BETWEEN '2026-05-01' AND '2026-05-31'
     AND sessions > 0
|> select page_path, sessions, bounce_rate
|> order by sessions DESC
|> limit 25
```

**Grep a local CSV export for rows mentioning a region.**

```qfs
# qfs-cookbook: grammar=core; milestone=M0; features=decode,where,OR,select
/local/exports/sales.csv
|> decode csv
|> where region == 'APAC'
     OR region == 'ANZ'
|> select order_id, region, total, closed_at
```

**Compute days-overdue on the fly while listing late invoices.**

```qfs
# qfs-cookbook: grammar=core; milestone=M0; features=where,extend,select,order by
/sql/pg/invoices
|> where status == 'overdue'
|> extend days_late = date_diff('day', due_date, now())
|> select customer, amount_due, due_date, days_late
|> order by days_late DESC
```

**Build a display label for each open issue from its parts.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M1; features=where,extend,||,select,order by
/github/acme/web/issues
|> where state == 'open'
|> extend label = '#' || number || ' — ' || title
|> select label, assignee, milestone
|> order by number ASC
```

**Get the distinct senders who have mailed support this week.**

```qfs
# qfs-cookbook: grammar=core; milestone=M2; features=where,AND,LIKE,select,distinct,order by
/mail/inbox
|> where to LIKE '%support@acme.com%'
     AND received_at > '2026-06-19'
|> select from
|> distinct
|> order by from ASC
```

**Reach into nested customer JSON to filter by billing country.**

```qfs
# qfs-cookbook: grammar=core; milestone=M0; features=where,AND,select,path-nav
/sql/pg/customers
|> where billing.address.country == 'JP'
     AND billing.plan.tier == 'enterprise'
|> select id, name, billing.plan.tier, billing.address.city
```

**Find PRs whose head commit failed CI, reading nested status.**

```qfs
# qfs-cookbook: grammar=core; milestone=M1; features=where,AND,select,path-nav,order by
/github/acme/web/pulls
|> where state == 'open'
     AND head.status.state == 'failure'
|> select number, title, head.ref, head.status.context
|> order by number DESC
```

**Explode message recipients into one row per addressee.**

```qfs
# qfs-cookbook: grammar=core; milestone=M2; features=where,expand,select,distinct
/mail/sent
|> where subject LIKE '%Release 4.0%'
|> expand recipients
|> select recipients.email, recipients.kind
|> distinct
```

**List every attachment across this week's invoices mail.**

```qfs
# qfs-cookbook: grammar=core; milestone=M2; features=where,AND,expand,select,order by
/mail/inbox
|> where subject ~ '(?i)invoice'
     AND received_at > '2026-06-19'
|> expand attachments
|> select from, attachments.filename, attachments.size
|> order by attachments.size DESC
```

**Flatten order line-items to find SKUs that sold under a discount.**

```qfs
# qfs-cookbook: grammar=core; milestone=M0; features=expand,where,select,order by
/sql/pg/orders
|> expand line_items
|> where line_items.discount_pct > 0.25
|> select id, line_items.sku, line_items.qty, line_items.discount_pct
|> order by line_items.discount_pct DESC
```

**Expand requested reviewers to see who is blocking each PR.**

```qfs
# qfs-cookbook: grammar=core; milestone=M1; features=where,expand,select,order by
/github/acme/web/pulls
|> where state == 'open'
|> expand requested_reviewers
|> select number, requested_reviewers.login, requested_reviewers.team
|> order by number ASC
```

**Match support tickets against any of a set of urgent keywords.**

```qfs
# qfs-cookbook: grammar=core; milestone=M0; features=where,ANY,AND,select,order by
/sql/pg/tickets
|> where subject ~ ANY ('refund', 'chargeback', 'cancel', 'lawsuit')
     AND status <> 'closed'
|> select id, subject, priority, opened_at
|> order by opened_at ASC
```

**Find files whose name contains any of several report codes.**

```qfs
# qfs-cookbook: grammar=core; milestone=M0; features=where,LIKE,ANY,select,order by
/drive/Finance
|> where name LIKE ANY ('%FY26%', '%FY27%', '%audit%')
|> select name, owner, modified_at
|> order by modified_at DESC
```

**Read one file at a tagged release straight out of git.**

```qfs
# qfs-cookbook: grammar=core; milestone=M1; features=@version,decode,select
/git/app@v1.2/Cargo.toml
|> decode toml
|> select package.name, package.version, package.edition
```

**Compare config keys present in a specific S3 object version.**

```qfs
# qfs-cookbook: grammar=core; milestone=M0; features=@version,decode,where,select
/s3/acme-config/app/settings.json@K7sJpq2vN1
|> decode json
|> where feature_flags.new_billing == true
|> select environment, feature_flags.new_billing, rollout.percent
```

**Read a Drive doc as it stood at an earlier revision.**

```qfs
# qfs-cookbook: grammar=core; milestone=M0; features=@version,decode,select,path-nav
/drive/Specs/pricing.md@rev_88
|> decode md
|> select title, status, owner, body
```

**See how an order row looked before a disputed edit, using AS OF.**

```qfs
# qfs-cookbook: grammar=core; milestone=M0; features=as OF,where,select
/sql/pg/orders as OF '2026-05-01'
|> where id == 'ord_91823'
|> select id, status, total, shipped_at, updated_at
```

**Audit the prior price book: enterprise SKUs as of quarter start.**

```qfs
# qfs-cookbook: grammar=core; milestone=M0; features=as OF,where,AND,select,order by
/sql/pg/price_book as OF '2026-04-01'
|> where tier == 'enterprise'
     AND active == true
|> select sku, list_price, currency
|> order by list_price DESC
```

**List the branches and tags currently pointing into a repo.**

```qfs
# qfs-cookbook: grammar=core; milestone=M1; features=where,OR,select,order by
/git/app/refs
|> where kind == 'branch'
     OR kind == 'tag'
|> select name, kind, target_sha, updated_at
|> order by updated_at DESC
```

**Who logged in from outside the office this week? (planned /sys/audit)**

```qfs
# qfs-cookbook: grammar=core; milestone=M3; features=where,AND,NOT,LIKE,select,order by,limit
/sys/audit
|> where action == 'login'
     AND occurred_at > '2026-06-19'
     AND NOT ip LIKE '203.0.113.%'
|> select actor, ip, occurred_at, user_agent
|> order by occurred_at DESC
|> limit 200
```

**List service connections that have not synced recently. (planned /sys/connections)**

```qfs
# qfs-cookbook: grammar=core; milestone=M5; features=where,OR,IN,select,order by
/sys/connections
|> where status IN ('error', 'expired')
     OR last_sync_at < '2026-06-01'
|> select driver, connection, status, last_sync_at
|> order by last_sync_at ASC
```

**Read the current Claude session's standing instructions. (planned /hosts)**

```qfs
# qfs-cookbook: grammar=core; milestone=M7; features=select,path-nav
/hosts/laptop/claude/sessions/current/instructions
|> select scope, rule, priority, updated_at
```

**Find directory groups whose names match a team prefix. (planned /directories)**

```qfs
# qfs-cookbook: grammar=core; milestone=M5; features=where,LIKE,AND,select,distinct,order by
/directories/google/groups
|> where name LIKE 'eng-%'
     AND member_count > 0
|> select name, email, member_count
|> distinct
|> order by name ASC
```

**List the largest local log files left over from a debugging session.**

```qfs
# qfs-cookbook: grammar=core; milestone=M+; features=where,AND,LIKE,select,order by,limit
/fs/var/log
|> where name LIKE '%.log'
     AND size > 10485760
|> select path, size, modified_at
|> order by size DESC
|> limit 15
```

## Cross-service federation

This is the whole reason qfs exists: one query language that JOINs, UNIONs, and set-subtracts across services that otherwise never speak to each other. Because every service is a path and every path yields rows, a SQL table can JOIN a GitHub issue list, a Slack message log can be reconciled against a git history, and a Drive folder can be diffed against a catalog with EXCEPT. The recipes below stay in today's frozen grammar (`grammar=core`) — the milestone tag reflects which services must be live for the recipe to actually run — and lean on `JOIN … ON`, `UNION`, `EXCEPT`, `INTERSECT`, plus aggregation and filtering on top of the federated result.

**How a mixed-source query resolves — pushed down per source, combined locally, identical on every face.** This is the canonical federation recipe: it documents the execution model (see [How qfs works → Federation](/guide/concepts#_5-federation-one-query-many-services)), not just the syntax.

```qfs
# qfs-cookbook: grammar=core; milestone=M4; features=where,join,on,select,order by
/sql/pg/orders
|> where status == 'paid' AND placed_at >= '2026-01-01'
|> join /github/acme/support/issues on orders.email == issues.reporter_email
|> where issues.state == 'open'
|> select orders.id, orders.total, issues.number, issues.title
|> order by orders.total DESC
```

qfs resolves this in two stages, and the resolution is the **same** whether you run it from the CLI on your laptop, a self-hosted server, or a cloud Worker:

1. **Pushdown per source.** The `/sql/pg/orders` subtree (`WHERE status = 'paid' AND placed_at >= …`) becomes **one SQL query** executed inside Postgres; the `/github/acme/support/issues` subtree becomes a **filtered GitHub API fetch**. Each backend does what it can natively (`qfs describe` shows the **pushdown** line for each).
2. **Local combine.** The cross-source `JOIN … ON orders.email = issues.reporter_email`, the post-join `WHERE issues.state = 'open'`, the `SELECT`, and the `ORDER BY` run **in qfs's own engine, in-process** — only the residual that genuinely spans the two services. The same binary, the same planner, the same combine engine do this on every face; at cloud scale only the tenant→DB routing differs — never the resolution itself.

**Match paid orders to the GitHub issues their customers filed.**

```qfs
# qfs-cookbook: grammar=core; milestone=M4; features=join,on,where,select
/sql/pg/orders
|> where status == 'paid'
|> join /github/acme/support/issues on orders.email == issues.reporter_email
|> select orders.id, orders.total, issues.number, issues.title, issues.state
```

**Find support tickets opened by customers who have never actually purchased.**

```qfs
# qfs-cookbook: grammar=core; milestone=M4; features=select,except
/github/acme/support/issues
|> select reporter_email as email
|> except
   /sql/pg/orders
   |> select email
```

**Tie every merged pull request back to the Slack thread that announced it.**

```qfs
# qfs-cookbook: grammar=core; milestone=M5; features=where,join,on,select
/github/acme/web/pulls
|> where state == 'merged'
|> join /slack/acme/eng-releases/messages on pulls.number == messages.thread_ref
|> select pulls.number, pulls.title, messages.user, messages.text, messages.ts
```

**Reconcile a Drive reports folder against the catalog of reports we expect to exist.**

```qfs
# qfs-cookbook: grammar=core; milestone=M4; features=select,except
/sql/pg/report_catalog
|> select filename
|> except
   /drive/Reports
   |> select name as filename
```

**List S3 inventory keys that are also recorded as live assets in the database.**

```qfs
# qfs-cookbook: grammar=core; milestone=M4; features=select,intersect
/s3/media-prod
|> select key
|> intersect
   /sql/pg/assets
|> select storage_key as key
```

**Cross every GA signup session against the SQL users table to confirm conversions.**

```qfs
# qfs-cookbook: grammar=core; milestone=M4; features=where,join,on,select
/ga/marketing-site/events
|> where event_name == 'sign_up'
|> join /sql/pg/users on events.user_id == users.external_id
|> select events.session_id, events.source, users.id, users.plan, users.created_at
```

**Map git commits to the GitHub pull requests that introduced them.**

```qfs
# qfs-cookbook: grammar=core; milestone=M5; features=join,on,select,order by
/git/app/commits
|> join /github/acme/app/pulls on commits.sha == pulls.merge_commit_sha
|> select commits.sha, commits.author, commits.message, pulls.number, pulls.title
|> order by commits.committed_at
```

**Surface inbound customer emails that match an open SQL support case.**

```qfs
# qfs-cookbook: grammar=core; milestone=M4; features=join,on,where,select
/mail/inbox
|> join /sql/pg/cases on inbox.from == cases.customer_email
|> where cases.status == 'open'
|> select inbox.from, inbox.subject, cases.id, cases.priority, cases.assignee
```

**Find catalog SKUs that have no corresponding image in the S3 media bucket.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M4; features=select,||,except
/sql/pg/products
|> select 'products/' || sku || '.jpg' as key
|> except
   /s3/media-prod
|> select key
```

**Show high-value orders alongside the GitHub issue and its assigned engineer.**

```qfs
# qfs-cookbook: grammar=core; milestone=M5; features=where,join,on,select,order by
/sql/pg/orders
|> where total > 5000
|> join /github/acme/support/issues on orders.email == issues.reporter_email
|> join /github/acme/support/issues/assignees on issues.number == assignees.issue_number
|> select orders.id, orders.total, issues.number, assignees.login
|> order by orders.total DESC
```

**Count, per repository, how many merged PRs were ever discussed in Slack.**

```qfs
# qfs-cookbook: grammar=core; milestone=M5; features=where,join,on,aggregate,group by
/github/acme/web/pulls
|> where state == 'merged'
|> join /slack/acme/eng-releases/messages on pulls.number == messages.thread_ref
|> aggregate count() as discussed_prs
|> group by pulls.base_repo
```

**Build a churn-risk list: paying customers who filed an issue but sent no email follow-up.**

```qfs
# qfs-cookbook: grammar=core; milestone=M5; features=where,join,on,select,except
/sql/pg/customers
|> where plan == 'enterprise'
|> join /github/acme/support/issues on customers.email == issues.reporter_email
|> select customers.email
|> except
   /mail/sent
|> select to as email
```

**Combine every channel a contact reached us through into one unified touch log.**

```qfs
# qfs-cookbook: grammar=core; milestone=M5; features=select,union,order by
/mail/inbox
|> select from as contact, 'email' as channel, subject as detail, received_at as at
|> union
   /slack/acme/support/messages
|> select user as contact, 'slack' as channel, text as detail, ts as at
|> union
   /github/acme/support/issues
|> select reporter_email as contact, 'github' as channel, title as detail, created_at as at
|> order by at DESC
```

**Diff a git-tracked schema file against the live database's expected tables.**

```qfs
# qfs-cookbook: grammar=core; milestone=M4; features=decode,select,except
/git/infra@main/schema/tables.yaml
|> decode yaml
|> select name
|> except
   /sql/pg/information_schema_tables
|> select table_name as name
```

**Pair Drive contract PDFs with the matching customer record in SQL.**

```qfs
# qfs-cookbook: grammar=core; milestone=M5; features=join,on,where,select
/drive/Contracts
|> join /sql/pg/customers on files.customer_id == customers.id
|> where customers.status == 'active'
|> select files.name, files.modified_at, customers.legal_name, customers.account_manager
```

**Find Slack messages that reference a commit SHA present in the repo history.**

```qfs
# qfs-cookbook: grammar=core; milestone=M5; features=join,on,select
/slack/acme/incidents/messages
|> join /git/app/commits on messages.commit_ref == commits.sha
|> select messages.ts, messages.user, commits.sha, commits.author, commits.message
```

**Aggregate GA revenue by campaign, enriched with the SQL plan each user bought.**

```qfs
# qfs-cookbook: grammar=core; milestone=M5; features=join,on,where,aggregate,group by
/ga/marketing-site/events
|> join /sql/pg/users on events.user_id == users.external_id
|> where events.event_name == 'purchase'
|> aggregate sum(users.mrr) as total_mrr, count() as conversions
|> group by events.campaign
```

**List R2 backup keys that exist in storage but are absent from the backup ledger.**

```qfs
# qfs-cookbook: grammar=core; milestone=M4; features=select,except
/r2/backups
|> select key
|> except
   /sql/pg/backup_ledger
|> select object_key as key
```

**Three-way join: order → GitHub issue → the Slack escalation thread.**

```qfs
# qfs-cookbook: grammar=core; milestone=M5; features=join,on,where,select
/sql/pg/orders
|> join /github/acme/support/issues on orders.email == issues.reporter_email
|> join /slack/acme/escalations/messages on issues.number == messages.thread_ref
|> where issues.state == 'open'
|> select orders.id, issues.number, messages.user, messages.text
```

**Identify users active in GA who have no row in the SQL users table (tracking leak).**

```qfs
# qfs-cookbook: grammar=extended; milestone=M4; features=select,distinct,except
/ga/app/events
|> select distinct user_id
|> except
   /sql/pg/users
|> select external_id as user_id
```

**Cross-reference open PRs with the on-call engineer from the directory groups.**

```qfs
# qfs-cookbook: grammar=core; milestone=M5; features=where,join,on,select
/github/acme/web/pulls
|> where state == 'open'
|> join /directories/google/groups on pulls.assignee_email == groups.member_email
|> where groups.name == 'oncall-web'
|> select pulls.number, pulls.title, pulls.assignee_email, pulls.updated_at
```

**Reconcile invoices in SQL against the receipts archived in Drive.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M4; features=select,||,except
/sql/pg/invoices
|> select 'invoice-' || number || '.pdf' as name
|> except
   /drive/Finance/Receipts
|> select name
```

**Total Slack support volume per customer tier by joining channel logs to SQL accounts.**

```qfs
# qfs-cookbook: grammar=core; milestone=M5; features=join,on,aggregate,group by,order by
/slack/acme/support/messages
|> join /sql/pg/accounts on messages.user_email == accounts.contact_email
|> aggregate count() as message_count
|> group by accounts.tier
|> order by message_count DESC
```

**Find commits authored by people no longer in the directory (offboarding audit).**

```qfs
# qfs-cookbook: grammar=extended; milestone=M5; features=select,distinct,except
/git/app/commits
|> select distinct author_email as email
|> except
   /directories/entra/users
|> select mail as email
```

**Join inbound mail to GitHub issues and Slack mentions for a full per-customer thread.**

```qfs
# qfs-cookbook: grammar=core; milestone=M5; features=join,on,where,select,order by
/mail/inbox
|> join /sql/pg/customers on inbox.from == customers.email
|> join /github/acme/support/issues on customers.email == issues.reporter_email
|> where issues.state == 'open'
|> select customers.legal_name, inbox.subject, issues.number, issues.title
|> order by issues.created_at DESC
```

**Keys that appear in BOTH the prod and DR buckets (verified replication set).**

```qfs
# qfs-cookbook: grammar=core; milestone=M4; features=select,intersect
/s3/media-prod
|> select key
|> intersect
   /s3/media-dr
|> select key
```

**Rank campaigns by signups that became paying orders, joining GA → users → orders.**

```qfs
# qfs-cookbook: grammar=core; milestone=M5; features=join,on,where,aggregate,group by,order by
/ga/marketing-site/events
|> where event_name == 'sign_up'
|> join /sql/pg/users on events.user_id == users.external_id
|> join /sql/pg/orders on users.id == orders.user_id
|> where orders.status == 'paid'
|> aggregate sum(orders.total) as revenue, count() as paying_signups
|> group by events.campaign
|> order by revenue DESC
```

**Union the day's deploy signals from git refs and Slack release posts into one feed.**

```qfs
# qfs-cookbook: grammar=core; milestone=M5; features=where,select,union,order by
/git/app/refs
|> where name LIKE 'release/%'
|> select name as signal, 'git-tag' as kind, created_at as at
|> union
   /slack/acme/eng-releases/messages
|> where text LIKE 'Deployed%'
|> select text as signal, 'slack' as kind, ts as at
|> order by at DESC
```

**Flag database customers who never opened a single GA session (dark accounts).**

```qfs
# qfs-cookbook: grammar=extended; milestone=M5; features=where,select,except
/sql/pg/customers
|> where plan == 'pro'
|> select external_id as user_id
|> except
   /ga/app/events
|> select distinct user_id
```

**Per-repo incident load: join commits to issues labeled 'incident' and count by author.**

```qfs
# qfs-cookbook: grammar=core; milestone=M5; features=join,on,where,aggregate,group by,order by
/github/acme/app/issues
|> where label == 'incident'
|> join /git/app/commits on issues.fix_sha == commits.sha
|> aggregate count() as incident_fixes
|> group by commits.author
|> order by incident_fixes DESC
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
# qfs-cookbook: grammar=extended; milestone=M4; features=where,aggregate,group by,order by
/sql/pg/customers
|> where status == 'active'
|> aggregate count() as customers group by country
|> order by customers DESC
```

**Total revenue and average order value by month for the current year.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M4; features=where,extend,aggregate,group by,order by
/sql/pg/orders
|> where placed_at >= '2026-01-01'
|> extend month = substr(placed_at, 1, 7)
|> aggregate sum(total) as revenue, avg(total) as aov, count() as orders group by month
|> order by month
```

**Show only the product categories that sold more than 1,000 units (HAVING-style filter).**

```qfs
# qfs-cookbook: grammar=extended; milestone=M4; features=aggregate,group by,where,order by
/sql/pg/order_lines
|> aggregate sum(qty) as units, sum(qty * unit_price) as gross group by category
|> where units > 1000
|> order by gross DESC
```

**List the distinct payment methods customers actually used last quarter.**

```qfs
# qfs-cookbook: grammar=core; milestone=M4; features=where,select,distinct
/sql/pg/orders
|> where placed_at BETWEEN '2026-04-01' AND '2026-06-30'
|> select payment_method
|> distinct
```

**Find the top 10 customers by lifetime spend.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M4; features=aggregate,group by,order by,limit
/sql/pg/orders
|> aggregate sum(total) as lifetime_value, count() as orders group by customer_id
|> order by lifetime_value DESC
|> limit 10
```

**Rank GA landing pages by sessions for last week.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M4; features=where,aggregate,group by,order by,limit
/ga/www.acme.com/events
|> where date BETWEEN '2026-06-15' AND '2026-06-21'
|> aggregate sum(sessions) as sessions, sum(conversions) as conversions group by landing_page
|> order by sessions DESC
|> limit 25
```

**Compute conversion rate per acquisition channel, best-converting first.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M4; features=where,aggregate,group by,extend,order by
/ga/www.acme.com/events
|> where date >= '2026-06-01'
|> aggregate sum(sessions) as sessions, sum(conversions) as conversions group by channel
|> extend conversion_rate = conversions / sessions
|> order by conversion_rate DESC
```

**Surface only the GA channels that drove fewer than 1% conversion (needs attention).**

```qfs
# qfs-cookbook: grammar=extended; milestone=M4; features=aggregate,group by,extend,where,order by
/ga/www.acme.com/events
|> aggregate sum(sessions) as sessions, sum(conversions) as conversions group by channel
|> extend conversion_rate = conversions / sessions
|> where conversion_rate < 0.01 AND sessions > 500
|> order by sessions DESC
```

**Revenue by acquisition channel — join GA sessions to SQL order totals (cross-service rollup).**

```qfs
# qfs-cookbook: grammar=extended; milestone=M5; features=join,on,aggregate,group by,order by
/sql/pg/orders
|> join /ga/www.acme.com/attribution on orders.session_id == attribution.session_id
|> aggregate sum(orders.total) as revenue, count() as orders group by attribution.channel
|> order by revenue DESC
```

**Cost per acquisition by campaign — GA spend against orders booked.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M5; features=join,on,aggregate,group by,extend,order by
/ga/www.acme.com/campaigns
|> join /sql/pg/orders on campaigns.campaign_id == orders.campaign_id
|> aggregate sum(campaigns.cost) as spend, count() as conversions group by campaigns.campaign_id
|> extend cpa = spend / conversions
|> order by cpa
```

**Count merged pull requests per author over the last 90 days (PR throughput).**

```qfs
# qfs-cookbook: grammar=extended; milestone=M2; features=where,aggregate,group by,order by
/github/acme/web/pulls
|> where state == 'merged' AND merged_at >= '2026-03-28'
|> aggregate count() as merged_prs group by author
|> order by merged_prs DESC
```

**Median-ish review latency per reviewer — average hours from first review request to approval.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M2; features=where,extend,aggregate,group by,order by
/github/acme/web/reviews
|> where submitted_at >= '2026-05-01' AND state == 'approved'
|> extend latency_hours = (submitted_at - requested_at) / 3600
|> aggregate avg(latency_hours) as avg_latency, count() as reviews group by reviewer
|> order by avg_latency DESC
```

**PR throughput by week across the whole repo.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M2; features=where,extend,aggregate,group by,order by
/github/acme/web/pulls
|> where state == 'merged' AND merged_at >= '2026-01-01'
|> extend week = substr(merged_at, 1, 10)
|> aggregate count() as merged, avg(additions + deletions) as avg_churn group by week
|> order by week
```

**Find reviewers carrying a backlog — more than 15 reviews requested but not yet submitted.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M2; features=where,aggregate,group by,order by
/github/acme/web/review_requests
|> where submitted_at IS NULL
|> aggregate count() as pending group by requested_reviewer
|> where pending > 15
|> order by pending DESC
```

**Issue churn: count opened vs closed issues per label this quarter.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M2; features=where,aggregate,group by,order by
/github/acme/web/issues
|> where created_at >= '2026-04-01'
|> aggregate count() as opened, sum(closed_at IS NOT NULL) as closed group by label
|> order by opened DESC
```

**Message volume per Slack channel over the past 30 days.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M2; features=where,aggregate,group by,order by,limit
/slack/acme/engineering/messages
|> where ts >= '2026-05-27'
|> aggregate count() as messages, count(thread_ts) as thread_replies group by channel
|> order by messages DESC
|> limit 20
```

**Who posts most in the support channel — top 15 chattiest members.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M2; features=where,aggregate,group by,order by,limit
/slack/acme/support/messages
|> where ts >= '2026-06-01'
|> aggregate count() as messages group by user
|> order by messages DESC
|> limit 15
```

**Slack activity by hour of day to find the team's quiet window.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M2; features=where,extend,aggregate,group by,order by
/slack/acme/general/messages
|> where ts >= '2026-05-01'
|> extend hour = substr(ts, 12, 2)
|> aggregate count() as messages, count(distinct user) as active_users group by hour
|> order by hour
```

**Rank senders by how many emails they sent me this month (sender frequency).**

```qfs
# qfs-cookbook: grammar=extended; milestone=M1; features=where,aggregate,group by,order by,limit
/mail/inbox
|> where received_at >= '2026-06-01'
|> aggregate count() as messages, sum(is_unread) as unread group by from_address
|> order by messages DESC
|> limit 25
```

**Find noisy senders — anyone who sent more than 20 unread emails this quarter.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M1; features=where,aggregate,group by,order by
/mail/inbox
|> where received_at >= '2026-04-01' AND is_unread == true
|> aggregate count() as unread_count group by from_address
|> where unread_count > 20
|> order by unread_count DESC
```

**Email volume by domain — group senders by their organization.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M1; features=where,extend,aggregate,group by,order by
/mail/inbox
|> where received_at >= '2026-05-01'
|> extend domain = substr(from_address, instr(from_address, '@') + 1)
|> aggregate count() as messages, count(distinct from_address) as senders group by domain
|> order by messages DESC
```

**Commits per author in the main repo over the last year (git contribution leaderboard).**

```qfs
# qfs-cookbook: grammar=extended; milestone=M2; features=where,aggregate,group by,order by
/git/app/commits
|> where committed_at >= '2025-06-26'
|> aggregate count() as commits, sum(additions) as lines_added, sum(deletions) as lines_removed group by author_email
|> order by commits DESC
```

**Commit cadence by month to visualize project momentum.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M2; features=where,extend,aggregate,group by,order by
/git/app/commits
|> where committed_at >= '2025-01-01'
|> extend month = substr(committed_at, 1, 7)
|> aggregate count() as commits, count(distinct author_email) as contributors group by month
|> order by month
```

**Bus-factor check: which files were touched by only one author.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M2; features=aggregate,group by,where,order by
/git/app/commits
|> aggregate count(distinct author_email) as authors, count() as changes group by path
|> where authors == 1 AND changes > 10
|> order by changes DESC
```

**Storage footprint by top-level prefix in an S3 bucket.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M4; features=extend,aggregate,group by,order by
/s3/acme-data-lake
|> extend prefix = substr(key, 1, instr(key, '/') - 1)
|> aggregate sum(size) as total_bytes, count() as objects group by prefix
|> order by total_bytes DESC
```

**Find S3 prefixes hoarding storage — more than 50 GB sitting in one place.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M4; features=extend,aggregate,group by,where,order by
/s3/acme-data-lake
|> extend prefix = substr(key, 1, instr(key, '/') - 1)
|> aggregate sum(size) as total_bytes, count() as objects group by prefix
|> where total_bytes > 53687091200
|> order by total_bytes DESC
```

**Storage growth by upload month to project next quarter's bill.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M4; features=where,extend,aggregate,group by,order by
/s3/acme-backups
|> where last_modified >= '2026-01-01'
|> extend month = substr(last_modified, 1, 7)
|> aggregate sum(size) as bytes_added, count() as objects group by month
|> order by month
```

**Distinct storage classes in use across the bucket.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M4; features=aggregate,group by,order by
/s3/acme-data-lake
|> aggregate count() as objects, sum(size) as bytes group by storage_class
|> order by bytes DESC
```

**Daily signups joined to first-week revenue — cohort value from SQL plus GA (cross-service).**

```qfs
# qfs-cookbook: grammar=extended; milestone=M5; features=join,on,where,aggregate,group by,order by
/sql/pg/customers
|> join /ga/www.acme.com/signups on customers.email == signups.email
|> where customers.created_at >= '2026-01-01'
|> aggregate count() as signups, sum(customers.first_week_spend) as cohort_revenue group by signups.acquisition_source
|> order by cohort_revenue DESC
```

**Engineering health snapshot: average PR size by author, only those above 400 lines.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M2; features=where,extend,aggregate,group by,order by
/github/acme/web/pulls
|> where state == 'merged' AND merged_at >= '2026-01-01'
|> extend size = additions + deletions
|> aggregate avg(size) as avg_pr_size, count() as prs group by author
|> where avg_pr_size > 400
|> order by avg_pr_size DESC
```

## Codecs & blob↔relational

A blob is just bytes until you `DECODE` it; then it is a relation you can filter, join and aggregate like any table. `ENCODE` runs the bridge the other way — a relation becomes `json`, `csv`, `yaml`, `toml`, `jsonl` or `md` bytes you can `UPSERT` back into any blob sink. The codec is independent of the source, so the same `DECODE md` works on a local file, an S3 object, a Drive doc, a git file pinned at a ref, GitHub file contents or the body of a REST/JSON response. For markdown, `DECODE md` lifts each document's YAML frontmatter keys into columns and exposes the prose as a `body` column, which turns a folder of notes into a queryable table.

**List every front-matter field of a local design note as a one-row table.**

```qfs
# qfs-cookbook: grammar=core; milestone=M2; features=decode,select
# md frontmatter keys become columns; body holds the prose
/local/docs/design/auth-rework.md
|> decode md
|> select title, status, owner, body
```

**Find every workaholic ticket still in todo, newest first.**

```qfs
# qfs-cookbook: grammar=core; milestone=M2; features=decode,where,order by,select
# .workaholic/**/*.md read as one table, frontmatter as columns
/local/.workaholic/tickets/todo/*.md
|> decode md
|> where status == 'todo'
|> order by created_at DESC
|> select id, title, severity, created_at
```

**Roll up open tickets by severity across the whole ticket tree.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M2; features=decode,where,aggregate,group by,order by
/local/.workaholic/**/*.md
|> decode md
|> where status <> 'done'
|> aggregate count() as open_tickets group by severity
|> order by open_tickets DESC
```

**Pull a config file out of git at a tagged release and read its keys.**

```qfs
# qfs-cookbook: grammar=core; milestone=M2; features=decode,select
# @version pins the blob to a ref before decoding
/git/app@v1.4.0/config/settings.toml
|> decode toml
|> select region, max_connections, feature_flags
```

**Diff a YAML setting between two releases by reading each AS OF its tag.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M2; features=decode,join,select
# decode the same file at two refs and join on key to compare
/git/infra@v2.0.0/helm/values.yaml
|> decode yaml
|> join /git/infra@v1.0.0/helm/values.yaml |> decode yaml on replicas
|> select replicas
```

**Read a REST/JSON webhook payload and explode its line items into rows.**

```qfs
# qfs-cookbook: grammar=core; milestone=M9; features=decode,expand,select
# the response body is a blob; decode json then expand the array column
/local/incoming/stripe-event.json
|> decode json
|> expand data.object.lines
|> select data.object.id as invoice, lines.description, lines.amount
```

**Turn a JSONL export of events into a daily count.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M2; features=decode,extend,aggregate,group by
# one JSON object per line → one row per line
/s3/analytics-exports/events-2026-06.jsonl
|> decode jsonl
|> extend day = substr(ts, 1, 10)
|> aggregate count() as events group by day
```

**Read a CSV in a Drive folder and keep only high-value rows.**

```qfs
# qfs-cookbook: grammar=core; milestone=M2; features=decode,where,order by,select
/drive/Finance/q2-pipeline.csv
|> decode csv
|> where stage == 'commit' AND amount > 50000
|> order by amount DESC
|> select account, amount, owner, close_date
```

**Read GitHub file contents at HEAD and surface the declared package version.**

```qfs
# qfs-cookbook: grammar=core; milestone=M2; features=decode,select
# GitHub blob contents decoded as json, no clone needed
/github/acme/web/contents/package.json
|> decode json
|> select name, version, dependencies.react as react_version
```

**List which tickets each engineer owns from the markdown ticket tree.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M2; features=decode,aggregate,group by,order by
/local/.workaholic/tickets/**/*.md
|> decode md
|> aggregate count() as tickets group by owner
|> order by tickets DESC
```

**Export open tickets to a shared CSV report on Drive (round-trip md→csv).**

```qfs
# qfs-cookbook: grammar=core; milestone=M2; features=decode,where,select,encode,upsert into
# decode md … transform … encode csv … upsert into storage
/local/.workaholic/tickets/todo/*.md
|> decode md
|> where severity IN ('high', 'critical')
|> select id, title, severity, owner
|> encode csv
|> upsert into /drive/Reports/open-critical-tickets.csv
```

**Convert a SQL query result into a JSON object dropped in an S3 bucket.**

```qfs
# qfs-cookbook: grammar=core; milestone=M4; features=where,select,encode,upsert into
/sql/pg/customers
|> where plan == 'enterprise' AND churned == false
|> select id, name, mrr, renewal_date
|> encode json
|> upsert into /s3/exports/enterprise-accounts.json
```

**Normalize a messy CSV and write it back as clean YAML config.**

```qfs
# qfs-cookbook: grammar=core; milestone=M2; features=decode,extend,select,encode,upsert into
# csv in, yaml out — codecs decouple input format from output format
/local/import/regions.csv
|> decode csv
|> extend code = lower(trim(code))
|> select code, display_name, timezone
|> encode yaml
|> upsert into /local/config/regions.yaml
```

**Bump a version field in a git-tracked TOML and commit the new blob.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M2; features=decode,set,encode,upsert into
# read TOML at a ref, mutate a column, re-encode and write a commit
/git/app@main/Cargo.toml
|> decode toml
|> set package.version = '0.1.0'
|> encode toml
|> upsert into /git/app@main/Cargo.toml
```

**Promote a draft note to published by flipping its frontmatter and re-encoding.**

```qfs
# qfs-cookbook: grammar=core; milestone=M2; features=decode,set,encode,upsert into
# md round-trip: frontmatter columns change, body is preserved
/local/blog/posts/launch-recap.md
|> decode md
|> set status = 'published', published_at = '2026-06-26'
|> encode md
|> upsert into /local/blog/posts/launch-recap.md
```

**Cross a markdown ticket table against the SQL users table to validate owners.**

```qfs
# qfs-cookbook: grammar=core; milestone=M5; features=decode,join,where,select
# blob-derived relation joined to a real database table
/local/.workaholic/tickets/todo/*.md
|> decode md
|> join /sql/pg/users on owner == users.username
|> where users.active == false
|> select id, title, owner
```

**Find tickets that reference a feature flag missing from the live config.**

```qfs
# qfs-cookbook: grammar=core; milestone=M2; features=decode,except,select
# two decoded blobs compared with set difference
/local/.workaholic/tickets/**/*.md
|> decode md
|> select flag
|> except
   /git/app@main/config/flags.yaml |> decode yaml |> select flag
```

**Read last quarter's pricing TOML from S3 by version id and list tiers.**

```qfs
# qfs-cookbook: grammar=core; milestone=M2; features=decode,order by,select
# @version on an S3 object pins a specific stored revision
/s3/pricing/tiers.toml@v8c1f2a
|> decode toml
|> order by monthly_price
|> select tier, monthly_price, seat_limit
```

**Audit which markdown docs are missing a required owner field.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M2; features=decode,where,select
/local/docs/**/*.md
|> decode md
|> where owner IS NULL OR trim(owner) == ''
|> select title, status
```

**Snapshot the current SQL order book as a JSONL backup in R2.**

```qfs
# qfs-cookbook: grammar=core; milestone=M4; features=where,select,encode,upsert into
# relational → jsonl, one object per line, into Cloudflare R2
/sql/pg/orders
|> where status == 'open'
|> select id, customer_id, total, placed_at
|> encode jsonl
|> upsert into /r2/backups/open-orders.jsonl
```

**Read an old config AS OF a date from SQL and re-publish it as YAML.**

```qfs
# qfs-cookbook: grammar=core; milestone=M4; features=as OF,select,encode,upsert into
# temporal read of a relational source, then encode to a blob
/sql/pg/settings as OF '2026-01-01'
|> select key, value
|> encode yaml
|> upsert into /drive/Configs/settings-snapshot-jan.yaml
```

**Merge two decoded CSV exports into one deduplicated customer file.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M2; features=decode,union,distinct,select,encode,upsert into
/drive/Imports/eu-customers.csv
|> decode csv
|> union
   /drive/Imports/us-customers.csv |> decode csv
|> distinct
|> select email, name, region
|> encode csv
|> upsert into /drive/Imports/all-customers.csv
```

**Extract the body of every README in a repo at a tag for a doc index.**

```qfs
# qfs-cookbook: grammar=core; milestone=M2; features=decode,select,encode,upsert into
# md bodies harvested from a git ref, written out as a jsonl index
/git/monorepo@v3.0.0/packages/**/README.md
|> decode md
|> select title, body
|> encode jsonl
|> upsert into /local/build/readme-index.jsonl
```

**Read a YAML manifest from GitHub and list services exceeding a CPU budget.**

```qfs
# qfs-cookbook: grammar=core; milestone=M2; features=decode,expand,where,select
/github/acme/infra/contents/manifests/services.yaml
|> decode yaml
|> expand services
|> where services.cpu > 4
|> select services.name, services.cpu, services.replicas
```

**Build a slack-ready digest table from the todo ticket markdown.**

```qfs
# qfs-cookbook: grammar=core; milestone=M2; features=decode,where,order by,select,encode,upsert into
# md → json digest dropped as a file a downstream job posts to Slack
/local/.workaholic/tickets/todo/*.md
|> decode md
|> where severity == 'critical'
|> order by created_at
|> select id, title, owner
|> encode json
|> upsert into /local/build/standup-digest.json
```

**Compare a markdown frontmatter field across two git refs of the same doc.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M2; features=decode,join,where,select
# as-OF-style read via @version on both sides, joined to spot drift
/git/handbook@v2/policies/security.md
|> decode md
|> join /git/handbook@v1/policies/security.md |> decode md on title
|> where status <> handbook_v1.status
|> select title, status
```

**Flatten a nested TOML config into a flat key/value CSV for review.**

```qfs
# qfs-cookbook: grammar=core; milestone=M2; features=decode,select,encode,upsert into
/local/config/app.toml
|> decode toml
|> select 'database.host' as key, database.host as value
|> encode csv
|> upsert into /local/audit/config-flat.csv
```

## Writes & effects

Reads describe the world; writes change it. qfs spells every mutation as a pipeline stage — `INSERT INTO`, `UPSERT INTO`, `UPDATE`, `REMOVE` — or as a namespaced `CALL` for irreducible state transitions a driver declares (`mail.send`, `github.merge`, `slack.post`). The safety model is what makes this livable: describe is pure, preview touches nothing, `--commit` applies reversible writes, and anything irreversible (sending mail, merging a PR, deleting a blob) refuses to run without `--commit-irreversible`. The recipes below move from the safe and undoable (draft a mail, UPSERT a report) to the one-way doors (send, merge, dispatch CI), and most stay in today's frozen core grammar so they parse now even where the driver ships later.

**Draft a thank-you email to every customer who ordered this week (reversible — writing a draft sends nothing).**

```qfs
# qfs-cookbook: grammar=extended; milestone=M4; features=where,insert into,values,||
# A draft is reversible: it lands in /mail/drafts and auto-commits under policy.
/sql/pg/orders
|> where created_at >= '2026-06-19'
|> insert into /mail/drafts
     values (to => customer_email,
             subject => 'Thanks for order #' || order_id,
             body => 'Hi ' || customer_name || ', your order is on its way.')
```

**Send the queued win-back emails (irreversible — needs --commit-irreversible).**

```qfs
# qfs-cookbook: grammar=core; milestone=M4; features=where,call,mail.send
# call mail.send is a one-way door; preview shows the recipients, --commit-irreversible actually sends.
/mail/drafts
|> where subject LIKE 'We miss you%'
|> call mail.send(to => to, subject => subject, body => body)
```

**Reply to every unanswered support thread with a holding message (irreversible send).**

```qfs
# qfs-cookbook: grammar=extended; milestone=M4; features=where,extend,call,mail.send,||
/mail/inbox
|> where label == 'support' AND answered == false AND received_at < '2026-06-25'
|> extend ack = 'Hi ' || from_name || ', we have your ticket and will reply within 24h.'
|> call mail.send(to => from_addr, subject => 'Re: ' || subject, body => ack)
```

**Upsert the nightly sales rollup to a Drive spreadsheet (reversible — overwrites a blob in place).**

```qfs
# qfs-cookbook: grammar=extended; milestone=M4; features=aggregate,group by,encode,upsert into
# UPSERT is retry-safe: re-running it produces the same object, never a duplicate.
/sql/pg/orders
|> where created_at >= '2026-06-01'
|> aggregate sum(amount) as total, count(*) as orders group by region
|> encode csv
|> upsert into /drive/Reports/june-sales.csv
```

**Publish a rendered report to S3, overwriting any prior copy at the same key (reversible UPSERT).**

```qfs
# qfs-cookbook: grammar=core; milestone=M2; features=decode,encode,upsert into
/local/reports/q2-summary.md
|> decode md
|> encode json
|> upsert into /s3/acme-reports/q2/summary.json
```

**Mirror a config blob from Drive into an R2 bucket (idempotent UPSERT, safe to re-run).**

```qfs
# qfs-cookbook: grammar=core; milestone=M2; features=upsert into
/drive/Config/app-settings.toml
|> upsert into /r2/edge-config/app-settings.toml
```

**Insert a git commit that writes a generated changelog (reversible — a commit can be reverted).**

```qfs
# qfs-cookbook: grammar=extended; milestone=M1; features=decode,select,encode,insert into,values
# Writing to /git/<repo>/commits records a real commit; history makes it reversible.
/git/app@main/CHANGELOG.md
|> decode md
|> insert into /git/app/commits
     values (message => 'docs: regenerate changelog',
             branch => 'main',
             path => 'CHANGELOG.md',
             body => body)
```

**Stage a new source file as a commit on a feature branch (reversible).**

```qfs
# qfs-cookbook: grammar=extended; milestone=M1; features=insert into,values,||
/local/src/handler.rs
|> insert into /git/app/commits
     values (message => 'feat: add request handler',
             branch => 'feat/handler',
             path => 'src/handler.rs',
             body => body)
```

**Squash-merge an approved pull request (irreversible — needs --commit-irreversible).**

```qfs
# qfs-cookbook: grammar=core; milestone=M1; features=where,call,github.merge
# Merging is a one-way door: preview shows which PR, --commit-irreversible performs the squash.
/github/acme/web/pulls
|> where number == 42 AND state == 'open' AND mergeable == true
|> call github.merge(number => number, method => 'squash')
```

**Auto-merge every approved Dependabot PR with a passing build (irreversible bulk merge).**

```qfs
# qfs-cookbook: grammar=core; milestone=M1; features=where,call,github.merge,AND
/github/acme/web/pulls
|> where author == 'dependabot[bot]' AND review_decision == 'APPROVED' AND checks_status == 'success'
|> call github.merge(number => number, method => 'squash')
```

**Comment on every PR that has been waiting on review for over three days (irreversible — posts publicly).**

```qfs
# qfs-cookbook: grammar=extended; milestone=M1; features=where,call,github.comment,||
/github/acme/web/pulls
|> where state == 'open' AND review_decision <> 'APPROVED' AND created_at < '2026-06-23'
|> call github.comment(number => number,
                       body => 'Friendly nudge — this PR by @' || author || ' is awaiting review.')
```

**Comment a build-failure summary on the PR that broke CI (irreversible comment).**

```qfs
# qfs-cookbook: grammar=extended; milestone=M1; features=where,call,github.comment,||
/github/acme/web/pulls
|> where number == 87 AND checks_status == 'failure'
|> call github.comment(number => number,
                       body => 'CI failed on ' || head_sha || ': see the run logs for the failing job.')
```

**Post a release announcement to the team Slack channel (irreversible — message goes out).**

```qfs
# qfs-cookbook: grammar=extended; milestone=M1; features=where,call,slack.post,||
/github/acme/web/releases
|> where published_at >= '2026-06-26'
|> call slack.post(channel => '#releases',
                   text => ':rocket: ' || tag_name || ' is live — ' || html_url)
```

**Cross-post overnight production errors to the on-call Slack channel (irreversible).**

```qfs
# qfs-cookbook: grammar=extended; milestone=M3; features=where,aggregate,group by,call,slack.post,||
# /sys/audit is a planned mount but still parses as core today (a path is just a token).
/sys/audit
|> where level == 'error' AND ts >= '2026-06-26T00:00:00Z'
|> aggregate count(*) as hits group by service
|> call slack.post(channel => '#oncall',
                   text => service || ' threw ' || hits || ' errors overnight')
```

**Append a daily standup summary as a Slack message (irreversible post).**

```qfs
# qfs-cookbook: grammar=extended; milestone=M1; features=where,aggregate,group by,call,slack.post,||
/github/acme/web/commits
|> where committed_at >= '2026-06-26'
|> aggregate count(*) as commits group by author
|> call slack.post(channel => '#standup', text => author || ' shipped ' || commits || ' commits today')
```

**Mark every shipped order as fulfilled and return the affected rows (reversible UPDATE).**

```qfs
# qfs-cookbook: grammar=core; milestone=M4; features=where,update,set,returning
# returning hands back what changed so you can chain or audit it.
/sql/pg/orders
|> where status == 'shipped' AND tracking_number <> ''
|> update set status = 'fulfilled', fulfilled_at = now()
|> returning order_id, customer_email, fulfilled_at
```

**Apply a 10% loyalty discount to repeat customers' open carts (reversible UPDATE).**

```qfs
# qfs-cookbook: grammar=core; milestone=M4; features=where,update,set,returning
/sql/pg/carts
|> where status == 'open' AND customer_order_count >= 5
|> update set discount_pct = 10, updated_at = now()
|> returning cart_id, customer_id, discount_pct
```

**Deactivate accounts that never confirmed their email after 30 days (reversible flag flip).**

```qfs
# qfs-cookbook: grammar=core; milestone=M4; features=where,update,set,AND,returning
/sql/pg/accounts
|> where email_confirmed == false AND created_at < '2026-05-27'
|> update set status = 'inactive', deactivated_at = now()
|> returning account_id, email
```

**Normalize country codes on legacy customer records (reversible UPDATE with RETURNING for the audit trail).**

```qfs
# qfs-cookbook: grammar=core; milestone=M4; features=where,update,set,returning
/sql/pg/customers
|> where country == 'USA'
|> update set country = 'US'
|> returning customer_id, country
```

**Confirm and return the inventory rows decremented for a shipment (reversible UPDATE).**

```qfs
# qfs-cookbook: grammar=extended; milestone=M4; features=where,update,set,returning
/sql/pg/inventory
|> where sku IN ('A-101', 'A-102', 'B-200') AND on_hand > 0
|> update set on_hand = on_hand - 1, last_picked_at = now()
|> returning sku, on_hand
```

**Remove webhook delivery logs older than 90 days (irreversible — DELETE needs --commit-irreversible).**

```qfs
# qfs-cookbook: grammar=core; milestone=M4; features=where,remove
# remove is a delete: preview shows the count, --commit-irreversible actually purges the rows.
/sql/pg/webhook_logs
|> where delivered_at < '2026-03-28'
|> remove
```

**Purge expired temporary export blobs from S3 (irreversible delete).**

```qfs
# qfs-cookbook: grammar=core; milestone=M2; features=where,remove,LIKE
/s3/acme-exports/tmp
|> where key LIKE 'tmp/%' AND last_modified < '2026-06-19'
|> remove
```

**Delete duplicate draft emails left over from a failed batch (irreversible — clears drafts).**

```qfs
# qfs-cookbook: grammar=core; milestone=M4; features=where,remove,LIKE,AND
/mail/drafts
|> where subject LIKE 'AUTO:%' AND created_at < '2026-06-25'
|> remove
```

**Evict stale objects from a KV namespace (irreversible delete).**

```qfs
# qfs-cookbook: grammar=core; milestone=M2; features=where,remove
/kv/sessions
|> where expires_at < '2026-06-26T00:00:00Z'
|> remove
```

**Dispatch the deploy workflow for the latest green commit on main (irreversible — triggers CI).**

```qfs
# qfs-cookbook: grammar=core; milestone=M1; features=where,order by,limit,call,ci.dispatch
# ci.dispatch starts a real run: preview names the workflow and ref, --commit-irreversible fires it.
/github/acme/web/commits
|> where branch == 'main' AND checks_status == 'success'
|> order by committed_at DESC
|> limit 1
|> call ci.dispatch(workflow => 'deploy.yml', ref => sha)
```

**Re-run the nightly ETL workflow for every failed run from last night (irreversible dispatch).**

```qfs
# qfs-cookbook: grammar=core; milestone=M1; features=where,call,ci.dispatch
/github/acme/data/runs
|> where workflow == 'etl.yml' AND conclusion == 'failure' AND created_at >= '2026-06-25'
|> call ci.dispatch(workflow => 'etl.yml', ref => head_branch)
```

**Snapshot a production object to a dated archive key before edits (reversible — server-side copy).**

```qfs
# qfs-cookbook: grammar=extended; milestone=M2; features=where,call,s3.copy,||
/s3/acme-prod/config.json
|> where size > 0
|> call s3.copy(dest => '/s3/acme-archive/config-' || version_id || '.json')
```

**Insert audit notes into a SQL table from a CSV upload (reversible bulk INSERT).**

```qfs
# qfs-cookbook: grammar=extended; milestone=M4; features=decode,insert into,values
/drive/Imports/access-review.csv
|> decode csv
|> insert into /sql/pg/audit_notes
     values (user_id => user_id, note => note, reviewed_by => reviewer)
```

**Copy approved candidate records into the hires table and return the new ids (reversible INSERT … RETURNING).**

```qfs
# qfs-cookbook: grammar=extended; milestone=M4; features=where,select,insert into,values,returning
/sql/pg/candidates
|> where stage == 'offer_accepted'
|> insert into /sql/pg/hires
     values (name => name, email => email, start_date => offer_start_date)
|> returning hire_id, email
```

**Draft individual offer emails for accepted candidates (reversible draft, sends nothing).**

```qfs
# qfs-cookbook: grammar=extended; milestone=M4; features=where,insert into,values,||
/sql/pg/candidates
|> where stage == 'offer_accepted' AND offer_email_sent == false
|> insert into /mail/drafts
     values (to => email,
             subject => 'Welcome aboard, ' || name || '!',
             body => 'We are thrilled to have you start on ' || offer_start_date || '.')
```

**Open a GitHub issue for every flaky test reported overnight (reversible — issues can be closed).**

```qfs
# qfs-cookbook: grammar=extended; milestone=M1; features=where,insert into,values,||
/sql/pg/test_failures
|> where flaky == true AND last_seen >= '2026-06-26'
|> insert into /github/acme/web/issues
     values (title => 'Flaky test: ' || test_name,
             body => 'Failed ' || fail_count || ' times overnight.',
             labels => 'flaky,test')
```

**Tag a release by writing a new ref pointer in git (reversible — refs are mutable pointers).**

```qfs
# qfs-cookbook: grammar=extended; milestone=M1; features=where,order by,limit,insert into,values
/git/app/commits
|> where branch == 'main'
|> order by committed_at DESC
|> limit 1
|> insert into /git/app/refs
     values (name => 'refs/tags/v1.4.0', target => sha)
```

**Update Slack channel topics from a SQL config table (reversible — topic edits revert).**

```qfs
# qfs-cookbook: grammar=core; milestone=M1; features=where,update,set,returning
/slack/acme/general/channel
|> where id == 'C12345'
|> update set topic = 'Q3 planning in progress'
|> returning id, topic
```

## LET & the functional core

qfs gains a small functional core in M6: `LET` binds an intermediate relation or value so you can reference it more than once without recomputing or re-typing it; lambdas `(x: Type) => expr` are first-class values; and the higher-order builtins `map`, `filter`, and `reduce` take those lambdas to transform columns inline. A user-defined function is just a `LET`-bound lambda — there is no separate function namespace to learn. These features compose cleanly with everything from the earlier themes: a `LET` can hold a subquery you join back against, a lambda can normalize a key before a `GROUP BY`, and the whole thing can still end in a write. Anything that uses `LET` or a lambda arrow `=>` parses only on the extended grammar, so the blocks below are tagged accordingly; plain `map`/`filter`/`reduce` calls without a lambda stay core.

**Flag orders that beat the running average of their own product category.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M6; features=let,aggregate,group by,join,where
# Bind the per-category average once, then compare every order back against it.
let cat_avg =
  /sql/pg/orders
  |> aggregate avg(amount) as avg_amount group by category
/sql/pg/orders as o
|> join cat_avg as c on o.category == c.category
|> where o.amount > c.avg_amount
|> select o.id, o.category, o.amount, c.avg_amount
```

**Rank each rep against their team's own average deal size.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M6; features=let,aggregate,group by,join,extend
let team_avg =
  /sql/pg/deals
  |> aggregate avg(value) as team_avg_value group by team
/sql/pg/deals as d
|> join team_avg as t on d.team == t.team
|> extend vs_team = d.value - t.team_avg_value
|> select d.rep, d.team, d.value, vs_team
|> order by vs_team
```

**Reuse one active-customer set for both a count and a revenue total.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M6; features=let,where,join,aggregate
# active is referenced twice: once to scope orders, once to count the cohort.
let active =
  /sql/pg/customers
  |> where status == 'active'
/sql/pg/orders as o
|> join active as a on o.customer_id == a.id
|> aggregate count(active.id) as active_customers, sum(o.amount) as active_revenue
```

**Normalize email addresses with a lambda before deduping a mailing list.**

```qfs
# qfs-cookbook: grammar=core; milestone=M6; features=let,=>,extend,distinct
let canon = (addr: String) => lower(trim(addr))
/sql/pg/contacts
|> extend key = canon(email)
|> select key
|> distinct
```

**Define a reusable margin function once and apply it across products.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M6; features=let,=>,extend,order by
# A user-defined function is just a let-bound lambda — no separate function namespace.
let margin = (price: Float, cost: Float) => (price - cost) / price
/sql/pg/products
|> extend gross_margin = margin(unit_price, unit_cost)
|> where gross_margin < 0.2
|> order by gross_margin
```

**Split a comma-separated tags column into a normalized list per row.**

```qfs
# qfs-cookbook: grammar=core; milestone=M6; features=let,=>,map,extend
let clean = (t: String) => lower(trim(t))
/sql/pg/articles
|> extend tags = map(split(raw_tags, ','), clean)
|> select id, title, tags
```

**Keep only the high-value line items inside each invoice.**

```qfs
# qfs-cookbook: grammar=core; milestone=M6; features==>,filter,extend
/sql/pg/invoices
|> extend big_lines = filter(line_items, (li: Row) => li.amount > 1000)
|> where size(big_lines) > 0
|> select id, customer, big_lines
```

**Total each order's line items with a reduce.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M6; features==>,reduce,extend
/sql/pg/orders
|> extend computed_total = reduce(line_items, (acc: Float, li: Row) => acc + li.amount, 0.0)
|> where computed_total <> stored_total
|> select id, stored_total, computed_total
```

**Bind a date threshold once and reuse it across two filters.**

```qfs
# qfs-cookbook: grammar=core; milestone=M6; features=let,where,extend
# cutoff is a scalar value, referenced in both the predicate and a derived flag.
let cutoff = '2026-03-27'
/sql/pg/orders
|> where created_at >= cutoff
|> extend is_fresh = created_at >= cutoff
|> select id, created_at, is_fresh
```

**Compare each repo's PR throughput to the org-wide average.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M6; features=let,aggregate,join
let org_avg =
  /github/acme/web/pulls
  |> aggregate avg(merged_count) as org_avg_merged
/github/acme/web/pulls
|> aggregate count(id) as merged_count group by repo
|> join org_avg on true
|> where merged_count > org_avg_merged
```

**Score Slack messages with a reusable urgency lambda.**

```qfs
# qfs-cookbook: grammar=core; milestone=M6; features=let,=>,extend,order by
let urgency = (text: String) =>
  (text ~ '(?i)urgent') OR (text ~ '(?i)asap') OR (text LIKE '%blocker%')
/slack/acme/incidents/messages
|> extend is_urgent = urgency(body)
|> where is_urgent
|> order by ts
```

**Flag products priced below the average of their own brand.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M6; features=let,aggregate,group by,join,where
let brand_avg =
  /sql/pg/products
  |> aggregate avg(unit_price) as avg_price group by brand
/sql/pg/products as p
|> join brand_avg as b on p.brand == b.brand
|> where p.unit_price < b.avg_price * 0.7
|> select p.sku, p.brand, p.unit_price, b.avg_price
```

**Apply a discount lambda across a basket and write the repriced rows back.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M6; features=let,=>,extend,upsert into
let discounted = (price: Float, pct: Float) => round(price * (1 - pct), 2)
/sql/pg/cart_items
|> extend final_price = discounted(unit_price, promo_pct)
|> upsert into /sql/pg/cart_items
     values (id => id, unit_price => final_price)
```

**Compute a per-customer lifetime value and reuse it for tiering.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M6; features=let,aggregate,group by,extend
let ltv =
  /sql/pg/orders
  |> aggregate sum(amount) as lifetime_value group by customer_id
ltv
|> extend tier = (lifetime_value > 10000)
|> select customer_id, lifetime_value, tier
|> order by lifetime_value
```

**Strip null and empty entries from a phone-number array.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M6; features==>,filter,extend
/sql/pg/contacts
|> extend phones = filter(raw_phones, (p: String) => p <> '' AND p IS NOT NULL)
|> select id, name, phones
```

**Title-case every word in a product name via map.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M6; features=let,=>,map,extend
let cap = (w: String) => upper(substr(w, 1, 1)) || lower(substr(w, 2))
/sql/pg/products
|> extend display_name = join_words(map(split(name, ' '), cap), ' ')
|> select sku, display_name
```

**Bind a region filter once and join it against two fact tables.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M6; features=let,where,join,aggregate,group by
let emea =
  /sql/pg/regions
  |> where continent IN ('Europe', 'Middle East', 'Africa')
/sql/pg/orders as o
|> join emea as r on o.region_id == r.id
|> aggregate sum(o.amount) as emea_revenue group by r.country
|> order by emea_revenue
```

**Find customers whose latest order beats their own historical average.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M6; features=let,aggregate,group by,join,where
let cust_avg =
  /sql/pg/orders
  |> aggregate avg(amount) as personal_avg group by customer_id
/sql/pg/orders as o
|> join cust_avg as c on o.customer_id == c.customer_id
|> where o.amount > c.personal_avg * 1.5
|> select o.customer_id, o.id, o.amount, c.personal_avg
```

**Reduce daily metric rows into a running max per service.**

```qfs
# qfs-cookbook: grammar=core; milestone=M6; features==>,reduce,extend
/sql/pg/metrics
|> extend peak = reduce(samples, (m: Float, s: Row) => max(m, s.value), 0.0)
|> select service, peak
|> order by peak
```

**Normalize a join key with a lambda so mismatched casing still matches.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M6; features=let,=>,extend,join
let norm = (s: String) => lower(trim(s))
/sql/pg/leads as l
|> extend lkey = norm(l.company)
|> join /sql/pg/accounts as a on lkey == norm(a.company)
|> select l.id, a.id as account_id, l.company
```

**Build a per-department headcount and compare each manager's span to it.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M6; features=let,aggregate,group by,join,extend
let dept_size =
  /sql/pg/employees
  |> aggregate count(id) as headcount group by department
/sql/pg/employees as e
|> join dept_size as d on e.department == d.department
|> where e.is_manager
|> extend span_share = e.direct_reports / d.headcount
|> select e.name, e.department, span_share
```

**Extract and lowercase the domain from every signup email.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M6; features=let,=>,extend,group by,aggregate
let domain_of = (email: String) => lower(split(email, '@')[2])
/sql/pg/signups
|> extend domain = domain_of(email)
|> aggregate count(id) as signups group by domain
|> order by signups
```

**Keep only PRs whose review set contains an approval.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M6; features==>,filter,extend,where
/github/acme/web/pulls
|> extend approvals = filter(reviews, (r: Row) => r.state = 'APPROVED')
|> where size(approvals) >= 2
|> select number, title, size(approvals) as approval_count
```

**Bind a markdown frontmatter set and write a summary index from it.**

```qfs
# qfs-cookbook: grammar=core; milestone=M6; features=let,decode,encode,upsert into
# posts is decoded once, then both filtered and re-encoded into an index file.
let posts =
  /local/blog/posts.jsonl
  |> decode jsonl
posts
|> where published
|> select title, slug, tags
|> encode csv
|> upsert into /drive/Blog/index.csv
```

**Compute each category's share of total revenue using a bound grand total.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M6; features=let,aggregate,group by,join,extend
let grand =
  /sql/pg/orders
  |> aggregate sum(amount) as total_revenue
/sql/pg/orders
|> aggregate sum(amount) as cat_revenue group by category
|> join grand on true
|> extend share = cat_revenue / total_revenue
|> order by share
```

**Validate phone formats with a reusable predicate lambda before export.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M6; features=let,=>,where,encode,upsert into
let is_e164 = (p: String) => p ~ '^\+[1-9][0-9]{7,14}$'
/sql/pg/contacts
|> where NOT is_e164(phone)
|> select id, name, phone
|> encode csv
|> upsert into /drive/DataQuality/bad_phones.csv
```

**Bound stale-threshold reused to flag and to draft nudges in one pass.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M6; features=let,=>,where,insert into,||
let stale_before = '2026-04-26'
/sql/pg/tasks
|> where due_at < stale_before AND status <> 'done'
|> insert into /mail/drafts
     values (to => assignee_email,
             subject => 'Overdue since ' || stale_before,
             body => 'Task ' || title || ' is past its due date.')
```

**Sum weighted scores across a scorecard with reduce.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M6; features==>,reduce,extend,order by
/sql/pg/vendor_scorecards
|> extend weighted = reduce(criteria, (acc: Float, c: Row) => acc + c.score * c.weight, 0.0)
|> select vendor, weighted
|> order by weighted
```

**Find accounts whose spend exceeds twice the cohort median proxy.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M6; features=let,aggregate,group by,join,where
let cohort =
  /sql/pg/accounts
  |> aggregate avg(annual_spend) as cohort_avg group by plan
/sql/pg/accounts as a
|> join cohort as c on a.plan == c.plan
|> where a.annual_spend > c.cohort_avg * 2
|> select a.name, a.plan, a.annual_spend, c.cohort_avg
```

**Apply a tax lambda per jurisdiction and reduce to a grand invoice total.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M6; features=let,=>,map,reduce,extend
let taxed = (li: Row) => li.amount * (1 + li.tax_rate)
/sql/pg/invoices
|> extend total_with_tax = reduce(map(line_items, taxed), (acc: Float, x: Float) => acc + x, 0.0)
|> select id, customer, total_with_tax
```

## Transactions

A `TRANSACTION { … }` block commits a set of writes all-or-nothing across heterogeneous sources — a SQL row, a local ledger line, an object in S3, a git commit — so that either every write lands or none does. The block is **reversible-only**: every effect inside must be undoable (UPSERT, INSERT of a draft, a git commit), so the engine can roll the whole set back if any participant fails. An irreversible effect — `CALL mail.send`, `CALL github.merge`, a destructive `REMOVE` against an append-only log — **inside** a `TRANSACTION` is a parse-time error, not a runtime one: the grammar refuses it before any work begins. The pattern is therefore always two recipes — first the transaction reaches its commit point, then a *separate* block performs the irreversible side effect once the durable state is safely on disk.

Each `TRANSACTION { … }` is a single statement (`grammar=extended`, delivered in M6). The separate irreversible follow-up is frequently `grammar=core`, since `CALL` and ordinary effects are frozen grammar.

**Record a paid invoice in both the SQL ledger and the local audit ledger atomically.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M6; features=transaction,upsert into,values
# Both rows land or neither does — the local ledger never drifts from the database of record.
transaction {
  upsert into /sql/pg/invoices
    values (id => 'INV-4821', status => 'paid', paid_at => '2026-06-26')
  upsert into /local/ledger/2026-06.jsonl
    values (invoice => 'INV-4821', event => 'paid', amount => 1290.00)
}
```

**Promote a build artifact to the release bucket and stamp its release row together.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M6; features=transaction,upsert into,select,decode
# The S3 object and the releases table move as one unit.
transaction {
  upsert into /s3/releases/app-1.4.0.tar.gz
    /s3/ci-staging/app-1.4.0.tar.gz
  upsert into /sql/pg/releases
    values (version => '1.4.0', channel => 'stable', published_at => '2026-06-26')
}
```

**Onboard a new customer: write the SQL customer row and seed their drive folder index.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M6; features=transaction,upsert into,values,encode
# A half-created customer (row but no workspace, or workspace but no row) is impossible.
transaction {
  upsert into /sql/pg/customers
    values (id => 'C-9001', name => 'Northwind Traders', tier => 'pro')
  upsert into /drive/Customers/C-9001/manifest.json
    values (customer => 'C-9001', created => '2026-06-26', tier => 'pro')
}
```

**Apply a config change as a git commit and flip the deployment-state row in lockstep.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M6; features=transaction,insert into,upsert into,values
# The committed config and the "what is live" pointer can never disagree.
transaction {
  insert into /git/infra/commits
    values (branch => 'main',
            message => 'bump replicas to 6',
            path => 'k8s/web.yaml',
            content => 'replicas: 6')
  upsert into /sql/pg/deploy_state
    values (service => 'web', desired_replicas => 6, updated_at => '2026-06-26')
}
```

**Move money between two SQL accounts so the debit and credit are inseparable.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M6; features=transaction,update,set,where
# Classic double-entry: a failure after the debit must not leave the credit unbooked.
transaction {
  update /sql/pg/accounts
    set balance = balance - 500
    where id == 'ACC-100'
  update /sql/pg/accounts
    set balance = balance + 500
    where id == 'ACC-200'
}
```

**Reconcile an order across SQL, the S3 receipt blob, and the local fulfilment ledger.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M6; features=transaction,upsert into,values,encode
# Three sources, one atomic boundary: order state, durable receipt, and ledger line.
transaction {
  upsert into /sql/pg/orders
    values (id => 'O-7700', status => 'fulfilled', fulfilled_at => '2026-06-26')
  upsert into /s3/receipts/O-7700.json
    values (order => 'O-7700', total => 240.00, currency => 'USD')
  upsert into /local/ledger/fulfilment.jsonl
    values (order => 'O-7700', event => 'shipped')
}
```

**Use a LET-bound batch id to tag a SQL write and a Cloudflare KV write identically.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M6; features=let,transaction,upsert into,values
# One generated id threads through every participant so cross-store joins stay sound.
let batch = 'B-2026-06-26-01'
transaction {
  upsert into /sql/pg/import_batches
    values (id => batch, state => 'committed', rows => 4200)
  upsert into /kv/imports/last_batch
    values (value => batch)
}
```

**Snapshot the live pricing table into git and record the snapshot ref in SQL together.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M6; features=transaction,insert into,upsert into,encode
# The version-controlled snapshot and its catalogue entry are committed atomically.
transaction {
  insert into /git/pricing/commits
    values (branch => 'main',
            message => 'pricing snapshot 2026-06-26',
            path => 'prices.csv',
            content => 'sku,price\nA1,9.99\nB2,19.99')
  upsert into /sql/pg/pricing_snapshots
    values (taken_at => '2026-06-26', ref => 'main', sku_count => 2)
}
```

**Quarantine a flagged user: disable the SQL account and write the case file to drive at once.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M6; features=transaction,update,set,where,upsert into,values
# Either the account is locked AND the case is documented, or nothing changes.
transaction {
  update /sql/pg/users
    set status = 'suspended', suspended_at = '2026-06-26'
    where id == 'U-553'
  upsert into /drive/Trust/cases/U-553.json
    values (user => 'U-553', reason => 'fraud_signal', opened => '2026-06-26')
}
```

**Close a sprint: archive the board state to git and roll the SQL sprint counter forward.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M6; features=transaction,insert into,update,set,where
# A committed archive without an advanced counter (or vice versa) can never happen.
transaction {
  insert into /git/ops/commits
    values (branch => 'main',
            message => 'archive sprint 41',
            path => 'sprints/41.json',
            content => '{"sprint":41,"closed":true}')
  update /sql/pg/project
    set current_sprint = 42
    where id == 'PRJ-1'
}
```

**Sync a derived report to both R2 and the SQL report index in one commit.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M6; features=transaction,upsert into,encode,select
# Downstream readers see the new blob and its index entry simultaneously.
transaction {
  upsert into /r2/reports/q2-summary.csv
    /sql/pg/sales
    |> aggregate sum(amount) as total group by region
    |> encode csv
  upsert into /sql/pg/reports
    values (name => 'q2-summary', location => 'r2://reports/q2-summary.csv', built_at => '2026-06-26')
}
```

**Transfer inventory between warehouses with two UPDATEs that must agree.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M6; features=transaction,update,set,where
# Stock leaves one location only if it arrives at the other.
transaction {
  update /sql/pg/inventory
    set qty = qty - 30
    where sku == 'SKU-12' AND warehouse == 'WH-EAST'
  update /sql/pg/inventory
    set qty = qty + 30
    where sku == 'SKU-12' AND warehouse == 'WH-WEST'
}
```

**Record a signed contract: store the PDF in drive and the metadata row in SQL atomically.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M6; features=transaction,upsert into,values
# The durable document and its searchable row commit together.
transaction {
  upsert into /drive/Contracts/2026/CT-330.pdf
    /local/incoming/CT-330.pdf
  upsert into /sql/pg/contracts
    values (id => 'CT-330', counterparty => 'Acme', signed_on => '2026-06-26', status => 'active')
}
```

**Atomically dedupe: remove a duplicate SQL row and log the merge in the local ledger.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M6; features=transaction,update,set,where,upsert into,values
# We fold the duplicate into the survivor and record why — both or neither.
transaction {
  update /sql/pg/customers
    set merged_into = 'C-1001', status = 'merged'
    where id == 'C-1002'
  upsert into /local/ledger/merges.jsonl
    values (survivor => 'C-1001', duplicate => 'C-1002', at => '2026-06-26')
}
```

**Publish a knowledge-base article as a git commit and index it in SQL together.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M6; features=transaction,insert into,upsert into,values
# The published markdown and its catalogue entry are one atomic publish.
transaction {
  insert into /git/kb/commits
    values (branch => 'main',
            message => 'publish: refund policy',
            path => 'articles/refund-policy.md',
            content => '# Refund policy\nRefunds within 30 days.')
  upsert into /sql/pg/kb_index
    values (slug => 'refund-policy', title => 'Refund policy', published => '2026-06-26')
}
```

**Bind three batch rows with a LET value and commit SQL, drive, and KV as a unit.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M6; features=let,transaction,upsert into,values
# A single run id stamps all three stores so the import is traceable end to end.
let run = 'RUN-558'
transaction {
  upsert into /sql/pg/etl_runs
    values (id => run, status => 'done', rows => 18000)
  upsert into /drive/ETL/runs/RUN-558.json
    values (run => run, status => 'done')
  upsert into /kv/etl/latest
    values (value => run)
}
```

**Accept a return: restock SQL inventory and write the credit memo blob to S3 atomically.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M6; features=transaction,update,set,where,upsert into,values
# Stock returns to the shelf only if the credit memo is durably stored.
transaction {
  update /sql/pg/inventory
    set qty = qty + 1
    where sku == 'SKU-88'
  upsert into /s3/credit-memos/CM-204.json
    values (id => 'CM-204', order => 'O-9000', amount => 49.00)
}
```

**Tag a release in git refs and mark the SQL release row as tagged together.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M6; features=transaction,upsert into,update,set,where,values
# The mutable tag pointer and the release record move atomically.
transaction {
  upsert into /git/app/refs
    values (name => 'v1.4.0', target => 'main')
  update /sql/pg/releases
    set tagged = true, tag = 'v1.4.0'
    where version == '1.4.0'
}
```

**Migrate a row between tables: insert into the new table and tombstone the old one atomically.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M6; features=transaction,insert into,where,update,set
# The new home and the old tombstone commit together — no orphan, no double-live row.
transaction {
  insert into /sql/pg/accounts_v2
    /sql/pg/accounts
    |> where id == 'ACC-777'
  update /sql/pg/accounts
    set status = 'migrated'
    where id == 'ACC-777'
}
```

**Append an event to a queue and persist its SQL projection in one atomic write.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M6; features=transaction,insert into,upsert into,values
# The queued work item and its read-model row are consistent the instant they exist.
transaction {
  insert into /queues/billing
    values (kind => 'charge', invoice => 'INV-900', amount => 75.00)
  upsert into /sql/pg/invoice_state
    values (invoice => 'INV-900', state => 'charging')
}
```

**Commit a data-quality fix to git and update the SQL row it corrects together.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M6; features=transaction,insert into,update,set,where
# The audit trail (the commit) and the corrected fact land as one.
transaction {
  insert into /git/data-fixes/commits
    values (branch => 'main',
            message => 'fix: country code for C-44',
            path => 'fixes/C-44.json',
            content => '{"id":"C-44","country":"JP"}')
  update /sql/pg/customers
    set country = 'JP'
    where id == 'C-44'
}
```

**Provision a project: SQL project row, drive workspace manifest, and D1 metadata, all-or-nothing.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M6; features=transaction,upsert into,values
# Three independent stores either all reflect the new project or none do.
transaction {
  upsert into /sql/pg/projects
    values (id => 'PRJ-9', name => 'Atlas', status => 'active')
  upsert into /drive/Projects/Atlas/manifest.json
    values (project => 'PRJ-9', created => '2026-06-26')
  upsert into /d1/edge/projects
    values (id => 'PRJ-9', region => 'apac')
}
```

**Now the commit-point pattern: settle an invoice atomically, THEN email the receipt in a separate block.**

The transaction below moves only reversible state — the SQL invoice row and the local ledger line. The receipt email is irreversible (`CALL mail.send` cannot be un-sent), so it lives in its own block that runs *after* the transaction has committed. Putting that `CALL` inside the `TRANSACTION` would be a parse-time error.

```qfs
# qfs-cookbook: grammar=extended; milestone=M6; features=transaction,upsert into,values
# Step 1 of 2 — durable, reversible settlement reaches its commit point first.
transaction {
  upsert into /sql/pg/invoices
    values (id => 'INV-4821', status => 'paid', paid_at => '2026-06-26')
  upsert into /local/ledger/2026-06.jsonl
    values (invoice => 'INV-4821', event => 'paid', amount => 1290.00)
}
```

**Send the receipt only after settlement committed (separate, irreversible — runs with --commit-irreversible).**

```qfs
# qfs-cookbook: grammar=extended; milestone=M6; features=where,call,||
# Step 2 of 2 — the irreversible call lives OUTSIDE the transaction, after the commit point.
/sql/pg/invoices
|> where id == 'INV-4821' AND status == 'paid'
|> call mail.send(to => billing_email,
                  subject => 'Receipt for ' || id,
                  body => 'Thank you — invoice ' || id || ' is paid in full.')
```

**Commit-point pattern (PR merge): record the merge decision in SQL atomically with a git audit commit.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M6; features=transaction,upsert into,insert into,values
# Step 1 of 2 — reversible bookkeeping for the merge decision commits first.
transaction {
  upsert into /sql/pg/pr_decisions
    values (pr => 42, repo => 'acme/web', decision => 'approved', at => '2026-06-26')
  insert into /git/audit/commits
    values (branch => 'main',
            message => 'approve merge of PR #42',
            path => 'decisions/pr-42.json',
            content => '{"pr":42,"decision":"approved"}')
}
```

**Perform the actual merge only after the decision committed (separate, irreversible github.merge).**

```qfs
# qfs-cookbook: grammar=core; milestone=M6; features=where,call
# Step 2 of 2 — github.merge is irreversible, so it runs OUTSIDE the transaction afterwards.
/github/acme/web/pulls/42
|> where state == 'open'
|> call github.merge(number => 42, method => 'squash')
```

**Commit-point pattern (announcement): stage the release state atomically, THEN post to Slack separately.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M6; features=transaction,upsert into,values
# Step 1 of 2 — the durable release state (SQL row + S3 notes blob) commits as a unit.
transaction {
  upsert into /sql/pg/releases
    values (version => '1.4.0', channel => 'stable', announced => false)
  upsert into /s3/releases/notes-1.4.0.md
    values (version => '1.4.0', notes => '# 1.4.0\n- faster sync\n- bug fixes')
}
```

**Announce the release only after state committed (separate, irreversible slack.post).**

```qfs
# qfs-cookbook: grammar=extended; milestone=M6; features=where,call,||
# Step 2 of 2 — slack.post cannot be un-posted, so it runs OUTSIDE the transaction.
/sql/pg/releases
|> where version == '1.4.0' AND announced == false
|> call slack.post(channel => '#releases',
                   text => 'Shipped ' || version || ' to the stable channel.')
```

## Policy, ACL & directory

Access in qfs is itself a query. `CREATE POLICY <name> ALLOW <verbs> ON '<glob>' [WHERE <cond>]` writes a row into the policy registry, and because the registry is just another mount you read, audit, and reshape policy the same way you read any service. Policies grant verbs (`read`, `write`, `call`, …) over a path glob to a role or group, with `WHERE` clauses that scope down to rows and columns and can defer the decision to your real identity provider via `member_of('/directories/...')`. The companion `/directories/{google,entra,ad}` mounts expose groups and users read-only, so a single source of truth — Google Workspace, Entra, or Active Directory — drives both who-is-who lookups and who-can-do-what grants. Most of these are `grammar=core` (POLICY is a frozen DDL keyword and `member_of(...)` is an ordinary function call); only the few that bind with `LET` or pass a lambda `=>` are `grammar=extended`.

**Give the on-call engineers read access to the production database.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M5; features=create policy,ALLOW,on,where,member_of
create policy oncall_pg_read
  ALLOW read on '/sql/pg/**'
  where member_of('/directories/google/groups/oncall@acme.com')
```

**Let the data team read every warehouse table but never the customer PII table.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M5; features=create policy,ALLOW,on,where,member_of,NOT,LIKE,AND
create policy data_team_warehouse
  ALLOW read on '/sql/pg/**'
  where member_of('/directories/entra/groups/data-team')
    AND NOT path LIKE '/sql/pg/customer_pii%'
```

**Grant the support role read-and-draft on the shared mailbox, but no sending.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M5; features=create policy,ALLOW,on,where,member_of
create policy support_mail_triage
  ALLOW read, write on '/mail/**'
  where member_of('/directories/ad/groups/support-tier1')
```

**Allow finance to send mail, since drafting alone is not enough for them.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M5; features=create policy,ALLOW,on,where,member_of
create policy finance_mail_send
  ALLOW read, write, call on '/mail/**'
  where member_of('/directories/google/groups/finance@acme.com')
```

**Scope a region's analysts to only their own region's rows (row-level security).**

```qfs
# qfs-cookbook: grammar=extended; milestone=M5; features=create policy,ALLOW,on,where,member_of,==
create policy emea_orders_rls
  ALLOW read on '/sql/pg/orders'
  where member_of('/directories/entra/groups/analysts-emea')
    AND region == 'EMEA'
```

**Hide salary and SSN columns from everyone outside HR (column-level scoping).**

```qfs
# qfs-cookbook: grammar=extended; milestone=M5; features=create policy,ALLOW,on,where,member_of,NOT,IN
create policy mask_employee_secrets
  ALLOW read on '/sql/pg/employees'
  where NOT member_of('/directories/ad/groups/hr-core')
    AND column NOT IN ('salary', 'ssn', 'bank_account')
```

**Let managers read their own direct reports' rows only.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M5; features=create policy,ALLOW,on,where,member_of,==
create policy manager_sees_reports
  ALLOW read on '/sql/pg/employees'
  where member_of('/directories/google/groups/managers@acme.com')
    AND manager_email == current_user()
```

**Give every full-time employee read access to the company wiki bucket.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M5; features=create policy,ALLOW,on,where,member_of
create policy wiki_read_all_staff
  ALLOW read on '/drive/Wiki/**'
  where member_of('/directories/google/groups/all-staff@acme.com')
```

**Let release engineers merge pull requests in the web repo, but not anywhere else.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M5; features=create policy,ALLOW,on,where,member_of
create policy release_eng_merge_web
  ALLOW read, write, call on '/github/acme/web/**'
  where member_of('/directories/entra/groups/release-engineers')
```

**Allow contractors read-only on a single repo for the length of an engagement.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M5; features=create policy,ALLOW,on,where,member_of,AND,<
create policy contractor_repo_window
  ALLOW read on '/github/acme/mobile/**'
  where member_of('/directories/ad/groups/contractors-2026')
    AND now() < '2026-12-31'
```

**Let the marketing group post to one Slack channel only.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M5; features=create policy,ALLOW,on,where,member_of
create policy marketing_announce_channel
  ALLOW read, write, call on '/slack/acme/announcements/**'
  where member_of('/directories/google/groups/marketing@acme.com')
```

**Grant the analytics group read on Google Analytics but nothing that can write.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M5; features=create policy,ALLOW,on,where,member_of
create policy ga_read_analytics
  ALLOW read on '/ga/**'
  where member_of('/directories/entra/groups/growth-analytics')
```

**Deny write to anything in the production S3 bucket unless you are in platform-ops.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M5; features=create policy,ALLOW,on,where,member_of
create policy prod_s3_write_guard
  ALLOW read, write on '/s3/prod-assets/**'
  where member_of('/directories/ad/groups/platform-ops')
```

**Inherit access from a parent team: give the whole engineering org read on all repos.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M5; features=create policy,ALLOW,on,where,member_of
create policy engineering_repos_read
  ALLOW read on '/github/acme/**'
  where member_of('/directories/google/groups/engineering@acme.com')
```

**Layer a narrower grant on top: the security sub-team may also write branch protection.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M5; features=create policy,ALLOW,on,where,member_of
create policy security_repos_write
  ALLOW read, write, call on '/github/acme/**/refs'
  where member_of('/directories/google/groups/security@acme.com')
```

**Let admins read the audit trail but never the raw connections secrets.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M5; features=create policy,ALLOW,on,where,member_of,NOT,LIKE,AND
create policy admin_audit_read
  ALLOW read on '/sys/audit/**'
  where member_of('/directories/entra/groups/it-admins')
    AND NOT path LIKE '/sys/connections/%/secret%'
```

**Conditional grant: allow refunds only for orders under a 1,000 currency threshold.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M5; features=create policy,ALLOW,on,where,member_of,AND,<=
create policy support_small_refunds
  ALLOW read, write on '/sql/pg/refunds'
  where member_of('/directories/ad/groups/support-tier1')
    AND amount <= 1000
```

**Let auditors read historical orders as-of-time but never the live mutable table writes.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M5; features=create policy,ALLOW,on,where,member_of
create policy auditor_orders_history
  ALLOW read on '/sql/pg/orders'
  where member_of('/directories/google/groups/auditors@acme.com')
```

**Grant the data-science group read on the reports drive folder and the queues that feed it.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M5; features=create policy,ALLOW,on,where,member_of,OR
create policy ds_reports_and_queues
  ALLOW read on '/drive/Reports/**'
  where member_of('/directories/entra/groups/data-science')
    OR member_of('/directories/entra/groups/ml-platform')
```

**Restrict KV namespace writes to the team that owns the feature-flag namespace.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M5; features=create policy,ALLOW,on,where,member_of
create policy flags_kv_owners
  ALLOW read, write on '/kv/feature-flags/**'
  where member_of('/directories/google/groups/web-platform@acme.com')
```

**Use a LET-bound directory glob so the same group drives two scoped grants at once.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M6; features=let,create policy,ALLOW,on,where,member_of,OR
let billing_team = '/directories/google/groups/billing@acme.com'
create policy billing_pg_and_drive
  ALLOW read, write on '/sql/pg/invoices'
  where member_of(billing_team)
    OR member_of('/directories/google/groups/finance@acme.com')
```

**Build a reusable membership predicate as a lambda and apply it to a sensitive table.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M6; features=let,=>,create policy,ALLOW,on,where,AND
let is_privileged = (g: String) => member_of(g) AND now() < '2027-01-01'
create policy privileged_payroll_read
  ALLOW read on '/sql/pg/payroll'
  where is_privileged('/directories/entra/groups/payroll-admins')
```

**List the current members of the on-call group straight from the directory.**

```qfs
# qfs-cookbook: grammar=core; milestone=M5; features=select,order by
/directories/google/groups/oncall@acme.com/members
|> select display_name, email, role
|> order by display_name
```

**Find every directory user in the EMEA department who has an admin job title.**

```qfs
# qfs-cookbook: grammar=core; milestone=M5; features=where,select,AND,LIKE,=,order by
/directories/entra/users
|> where department == 'EMEA'
   AND job_title LIKE '%Admin%'
|> select display_name, email, job_title, manager
|> order by display_name
```

**Cross-check a Slack channel's posters against who is actually allowed to post there.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M5; features=select,distinct,except,join,on
/slack/acme/announcements/messages
|> select distinct user_email as email
|> except
   /directories/google/groups/marketing@acme.com/members
   |> select email
```

**Show which active directory users have no policy granting them any database read.**

```qfs
# qfs-cookbook: grammar=core; milestone=M5; features=where,select,except,=,LIKE
/directories/ad/users
|> where account_enabled == true
|> select email
|> except
   /sys/policies
   |> where path_glob LIKE '/sql/%'
   |> select principal_email as email
```

**Reconcile a group's directory members with the people who actually appear in the audit log.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M5; features=select,join,on,where,aggregate,group by,order by
/directories/google/groups/finance@acme.com/members
|> join /sys/audit on members.email == audit.actor_email
|> where audit.verb == 'call'
|> aggregate count() as actions group by members.email
|> order by actions
```

**Grant access by attribute rather than group: anyone whose directory department is Legal.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M5; features=create policy,ALLOW,on,where,member_of,AND
create policy legal_contracts_read
  ALLOW read on '/drive/Contracts/**'
  where member_of('/directories/entra/groups/legal-dept')
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
# qfs-cookbook: grammar=extended; milestone=M8; features=create endpoint,where,order by,limit,select
# GET /oncall returns the single active rotation row as JSON
create endpoint GET /oncall as
  /sql/pg/oncall_rotations
  |> where active == true
  |> order by shift_start DESC
  |> limit 1
  |> select engineer, phone, slack_handle, shift_end
```

**Serve a per-customer order history endpoint keyed by a path parameter.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M8; features=create endpoint,where,join,order by,select
# GET /customers/:id/orders joins orders to line items for the requested customer
create endpoint GET /customers/:id/orders as
  /sql/pg/orders
  |> where customer_id == :id
  |> join /sql/pg/line_items on line_items.order_id == orders.id
  |> order by orders.placed_at DESC
  |> select orders.id, orders.placed_at, line_items.sku, line_items.qty, line_items.price
```

**Accept new leads over HTTP and write them straight into Postgres.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M8; features=create endpoint,insert into,values,returning
# POST /leads ingests the JSON body and returns the created row's id
create endpoint POST /leads as
  REQUEST.body
  |> insert into /sql/pg/leads
       values (email => email, name => name, source => source, captured_at => now())
  |> returning id
```

**Publish a daily KPI summary as a cached JSON endpoint.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M8; features=create endpoint,aggregate,group by,select,encode
# GET /metrics/daily aggregates today's orders by channel and emits JSON
create endpoint GET /metrics/daily as
  /sql/pg/orders
  |> where placed_at >= date_trunc('day', now())
  |> aggregate count() as orders, sum(total) as revenue group by channel
  |> order by revenue DESC
  |> encode json
```

**Forward every new inbox message from a key account into Slack.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M8; features=create trigger,on,where,do,call,NEW
# Fires on inbox arrival; posts a one-line digest to the sales channel
create trigger inbox_to_slack on insert into /mail/inbox
  where NEW.from LIKE '%@bigcorp.com'
  do call slack.post(channel => '#sales',
                     text => 'Mail from ' || NEW.from || ': ' || NEW.subject)
```

**Open a GitHub issue whenever a high-severity error lands in the log table.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M8; features=create trigger,on,where,do,insert into,values,NEW
# Severity >= 4 rows become tracked issues automatically
create trigger fatal_to_issue on insert into /sql/pg/error_log
  where NEW.severity >= 4
  do insert into /github/acme/web/issues
       values (title => '[auto] ' || NEW.service || ': ' || NEW.message,
               body => NEW.stacktrace,
               labels => 'incident,auto')
```

**Deploy automatically when a PR merges into the main branch.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M8; features=create trigger,on,where,AND,do,call,NEW
# PR-merged on main → dispatch the production deploy workflow
create trigger deploy_on_merge on update /github/acme/web/pulls
  where NEW.merged == true AND NEW.base == 'main'
  do call ci.dispatch(workflow => 'deploy-prod', ref => NEW.merge_commit_sha)
```

**Mirror every Slack file upload into S3 for retention.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M8; features=create trigger,on,do,call,NEW
# Each new file in #releases is copied to the archive bucket
create trigger archive_slack_files on insert into /slack/eng/releases/files
  do call s3.copy(from => NEW.url_private, to => '/s3/archive/slack/' || NEW.id)
```

**Greet new sign-ups with a welcome draft the moment they register.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M8; features=create trigger,on,where,do,insert into,values,NEW,||
# Reversible (draft only), so it runs unattended
create trigger welcome_new_user on insert into /sql/pg/users
  where NEW.verified == true
  do insert into /mail/drafts
       values (to => NEW.email,
               subject => 'Welcome aboard, ' || NEW.name,
               body => 'Thanks for joining, ' || NEW.name || '. Here is how to start...')
```

**Page on-call when a payment fails for a high-value subscription.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M8; features=create trigger,on,where,AND,do,call,NEW
# Only enterprise-tier failures escalate to the pager channel
create trigger dunning_alert on insert into /sql/pg/payment_failures
  where NEW.amount > 1000 AND NEW.tier == 'enterprise'
  do call slack.post(channel => '#billing-urgent',
                     text => 'Payment failed: ' || NEW.account || ' ($' || NEW.amount || ')')
```

**Comment on any PR that touches the database migrations directory.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M8; features=create trigger,on,where,do,call,NEW,LIKE
# Reminds reviewers to run the migration checklist
create trigger migration_reviewer on insert into /github/acme/web/pulls
  where NEW.files LIKE '%db/migrations/%'
  do call github.comment(number => NEW.number,
                         body => 'Touches migrations — please confirm the rollback plan.')
```

**Run a nightly sales report and drop it in Drive as a CSV.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M8; features=create job,every,do,where,aggregate,group by,encode,upsert into,LAST_RUN
# Aggregates everything since the last run and overwrites the rolling file
create job nightly_sales every '1 day' do
  /sql/pg/orders
  |> where placed_at >= LAST_RUN()
  |> aggregate sum(total) as revenue, count() as orders group by region
  |> order by revenue DESC
  |> encode csv
  |> upsert into /drive/Reports/nightly-sales.csv
```

**Sweep stale draft orders out of the table every hour.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M8; features=create job,every,do,where,AND,remove
# Drafts older than a day with no items are garbage-collected
create job gc_draft_orders every '1 hour' do
  /sql/pg/orders
  |> where status == 'draft' AND created_at < now() - interval '1 day'
  |> remove
```

**Send a Monday-morning digest of last week's merged PRs.**

```qfs
# qfs-cookbook: grammar=core; milestone=M8; features=create job,every,do,where,AND,select,encode,call,LAST_RUN
# Reads everything merged since the previous run and posts the list
create job weekly_pr_digest every '1 week' do
  /github/acme/web/pulls
  |> where merged == true AND merged_at >= LAST_RUN()
  |> order by merged_at DESC
  |> select number, title, author
  |> call slack.post(channel => '#eng-weekly', text => 'Shipped last week:')
```

**Refresh a Google Analytics snapshot into Postgres every morning.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M8; features=create job,every,do,select,upsert into,values
# Pulls yesterday's top pages and upserts them for the BI layer
create job ga_snapshot every '1 day' do
  /ga/123456/report
  |> where date == 'yesterday'
  |> order by pageviews DESC
  |> limit 100
  |> upsert into /sql/pg/ga_top_pages
       values (page => path, views => pageviews, snapshot_date => date)
```

**Back up a critical table to versioned S3 nightly.**

```qfs
# qfs-cookbook: grammar=core; milestone=M8; features=create job,every,do,encode,upsert into
# Full-table jsonl dump to a dated key in the backup bucket
create job backup_accounts every '1 day' do
  /sql/pg/accounts
  |> encode jsonl
  |> upsert into /s3/backups/accounts/latest.jsonl
```

**Expire and remove KV session keys that have gone idle.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M8; features=create job,every,do,where,remove
# Runs every fifteen minutes to keep the namespace lean
create job prune_sessions every '15 minutes' do
  /kv/sessions
  |> where last_seen < now() - interval '30 minutes'
  |> remove
```

**Re-poll an external status page and queue any new incidents.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M8; features=create job,every,do,where,select,insert into,values,LAST_RUN
# Bridges a read-only source into a work queue
create job poll_incidents every '5 minutes' do
  /sql/pg/upstream_incidents
  |> where detected_at >= LAST_RUN()
  |> select id, severity, summary
  |> insert into /queues/incident-intake
       values (incident_id => id, severity => severity, note => summary)
```

**Ingest an inbound webhook payload directly into a SQL audit table.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M8; features=create webhook,AT,do,decode,insert into,values
# Stripe-style hook → decode JSON body → persist the event
create webhook stripe_events AT /hooks/stripe do
  REQUEST.body
  |> decode json
  |> insert into /sql/pg/stripe_events
       values (event_id => id, kind => type, payload => data, received_at => now())
```

**Turn GitHub push webhooks into a deploy queue entry.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M8; features=create webhook,AT,do,decode,where,insert into,values
# Only pushes to main get queued for deployment
create webhook gh_push AT /hooks/github/push do
  REQUEST.body
  |> decode json
  |> where ref == 'refs/heads/main'
  |> insert into /queues/deploys
       values (sha => after, pusher => pusher.name, queued_at => now())
```

**Fan an inbound form-submission webhook out to a Slack notice.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M8; features=create webhook,AT,do,decode,call,||
# Marketing landing-page form → instant team ping
create webhook contact_form AT /hooks/contact do
  REQUEST.body
  |> decode json
  |> call slack.post(channel => '#leads',
                     text => 'New contact from ' || name || ' <' || email || '>')
```

**Accept a CI status webhook and update the matching deploy record.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M8; features=create webhook,AT,do,decode,where,update,set
# Reconciles external CI state back into our own table
create webhook ci_status AT /hooks/ci do
  /sql/pg/deploys
  |> where run_id == REQUEST.body.run_id
  |> update set status = REQUEST.body.conclusion, finished_at = now()
```

**Define a live view of open enterprise tickets across services.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M8; features=create view,as,where,AND,join,select
# A non-materialized view re-runs on every read
create view /views/enterprise_open_tickets as
  /sql/pg/tickets
  |> where status == 'open'
  |> join /sql/pg/accounts on accounts.id == tickets.account_id
  |> where accounts.tier == 'enterprise'
  |> select tickets.id, tickets.subject, accounts.name, tickets.opened_at
```

**Expose a unified activity feed view stitching GitHub and Slack.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M8; features=create view,as,select,as,union,order by
# union normalizes two sources into one feed shape
create view /views/eng_activity as
  /github/acme/web/commits
  |> select author as who, message as what, committed_at as at
  |> union
     (/slack/eng/general/messages
      |> select user as who, text as what, ts as at)
  |> order by at DESC
```

**Materialize a cross-service executive dashboard refreshed hourly.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M8; features=create materialized view,as,aggregate,group by,join,select
# Heavy join is computed once and served cheaply until next refresh
create materialized view /views/exec_dashboard as
  /sql/pg/orders
  |> aggregate sum(total) as revenue, count() as orders group by region
  |> join /sql/pg/support_load on support_load.region == orders.region
  |> select region, revenue, orders, support_load.open_tickets
```

**Materialize a denormalized customer-360 table from three services.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M8; features=create materialized view,as,join,select,order by
# Joins CRM, billing, and analytics into one wide row per customer
create materialized view /views/customer_360 as
  /sql/pg/customers
  |> join /sql/pg/subscriptions on subscriptions.customer_id == customers.id
  |> join /sql/pg/ga_top_pages on ga_top_pages.customer_id == customers.id
  |> select customers.id, customers.name, subscriptions.plan,
            subscriptions.mrr, ga_top_pages.views
  |> order by subscriptions.mrr DESC
```

**Materialize a daily error-rate rollup for the SLO dashboard.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M8; features=create materialized view,as,where,aggregate,group by,extend
# Pre-computes the ratio so the dashboard endpoint stays trivial
create materialized view /views/error_rate as
  /sql/pg/requests
  |> where ts >= now() - interval '7 days'
  |> aggregate count() as total, sum(is_error) as errors group by date_trunc('day', ts) as day
  |> extend rate = errors * 1.0 / total
```

**Serve the materialized dashboard back out through an endpoint.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M8; features=create endpoint,as,order by,encode
# Endpoint reads the precomputed view, so requests are cheap
create endpoint GET /dashboard/exec as
  /views/exec_dashboard
  |> order by revenue DESC
  |> encode json
```

**Escalate aging open incidents to email every six hours.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M8; features=create job,every,do,where,AND,insert into,values,||
# Reversible escalation (draft) for unresolved incidents past SLA
create job escalate_incidents every '6 hours' do
  /sql/pg/incidents
  |> where status == 'open' AND opened_at < now() - interval '4 hours'
  |> insert into /mail/drafts
       values (to => 'incident-mgr@acme.com',
               subject => 'SLA breach: incident ' || id,
               body => 'Open ' || (now() - opened_at) || ' — ' || summary)
```

## The /sys admin surface

Administering qfs is not a separate console — it is the same query language pointed at `/sys`.
Users, accounts, policies, connections, audit, projects, approvals, metrics, and billing are all
just paths, so you grant a role with `INSERT`, revoke with `REMOVE`, inspect who did what with a
`/sys/audit |> WHERE …`, and observe load with `/sys/metrics`. One safety invariant runs
through everything below: **`/sys/connections` describes connections by name and metadata only — it
never returns the secret, token, or password behind a connection.** You list, name, scope, and
revoke connections as data; you never read their credentials, because there is no column that holds
them. Listing/granting/revoking and audit reads land in M3 (the `/sys` driver); policies, projects,
and directory membership in M5; approvals, metrics, and billing in M9/M+.

**List every active human user and the role they currently hold.**

```qfs
# qfs-cookbook: grammar=core; milestone=M3; features=where,select,order by
/sys/users
|> where status == 'active' AND kind == 'human'
|> select email, display_name, role, last_seen_at
|> order by last_seen_at DESC
```

**Invite a new teammate as a read-only member.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M3; features=insert into,values,returning
insert into /sys/users
  values (email => 'dana@acme.com',
          display_name => 'Dana Reyes',
          role => 'viewer',
          status => 'invited')
|> returning email, role, status
```

**Promote a viewer to editor on their team.**

```qfs
# qfs-cookbook: grammar=core; milestone=M3; features=where,update,set,returning
/sys/users
|> where email == 'dana@acme.com' AND role == 'viewer'
|> update set role = 'editor'
|> returning email, role
```

**Off-board someone the moment they leave — revoke their account.**

```qfs
# qfs-cookbook: grammar=core; milestone=M3; features=where,remove
/sys/users
|> where email == 'former.staff@acme.com'
|> remove
```

**Find dormant accounts that have not signed in for 90 days.**

```qfs
# qfs-cookbook: grammar=core; milestone=M3; features=where,select,order by
/sys/users
|> where last_seen_at < '2026-03-28' AND status == 'active'
|> select email, display_name, role, last_seen_at
|> order by last_seen_at
```

**Audit which accounts still hold admin and were granted it by whom.**

```qfs
# qfs-cookbook: grammar=core; milestone=M3; features=where,select
/sys/accounts
|> where role == 'admin'
|> select email, granted_by, granted_at, scope
```

**Grant a service account scoped to a single project.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M3; features=insert into,values,returning
insert into /sys/accounts
  values (kind => 'service',
          name => 'ci-bot',
          role => 'editor',
          scope => 'project:web-frontend')
|> returning name, role, scope
```

**List the policies governing a sensitive SQL table.**

```qfs
# qfs-cookbook: grammar=core; milestone=M5; features=where,select,order by
/sys/policies
|> where resource LIKE '/sql/pg/payroll%'
|> select name, principal, effect, columns, row_filter
|> order by name
```

**Attach a row+column policy that masks PII for the support role.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M5; features=insert into,values
# Support sees the customer table but only their own region's rows, no SSN column.
insert into /sys/policies
  values (name => 'support-customers-eu',
          principal => 'role:support',
          resource => '/sql/pg/customers',
          effect => 'allow',
          columns => 'id,name,email,region',
          row_filter => "region = 'EU'")
```

**Revoke a policy that has become too permissive.**

```qfs
# qfs-cookbook: grammar=core; milestone=M5; features=where,remove
/sys/policies
|> where name == 'legacy-allow-all-finance'
|> remove
```

**Inspect every connection by name and metadata — no secrets are ever returned.**

```qfs
# qfs-cookbook: grammar=core; milestone=M3; features=select,order by
# /sys/connections has no credential column; you see scope and status, never the token.
/sys/connections
|> select name, driver, scope, status, created_by, last_used_at
|> order by driver, name
```

**Find connections that have gone stale and were never used.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M3; features=where,select,order by
/sys/connections
|> where last_used_at IS NULL OR last_used_at < '2026-01-01'
|> select name, driver, created_by, created_at, last_used_at
|> order by created_at
```

**Register a new connection by reference — credentials are supplied out of band, not in the query.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M3; features=insert into,values,returning
# The row records the connection's identity and scope; the secret lives in the vault, not here.
insert into /sys/connections
  values (name => 'acme-pg-readonly',
          driver => 'sql/pg',
          scope => 'project:analytics',
          secret_ref => 'vault://acme/pg-readonly')
|> returning name, driver, scope, status
```

**Disconnect a third-party integration cleanly.**

```qfs
# qfs-cookbook: grammar=core; milestone=M3; features=where,remove
/sys/connections
|> where name == 'old-zendesk' AND driver == 'http'
|> remove
```

**Audit who removed or deleted anything in the last 24 hours.**

```qfs
# qfs-cookbook: grammar=core; milestone=M3; features=where,select,order by
/sys/audit
|> where verb == 'REMOVE' AND at > '2026-06-25T00:00:00Z'
|> select at, actor, verb, resource, committed
|> order by at DESC
```

**Trace every irreversible CALL a single actor made this week.**

```qfs
# qfs-cookbook: grammar=core; milestone=M3; features=where,select,order by
/sys/audit
|> where actor == 'dana@acme.com'
     AND verb == 'CALL'
     AND at BETWEEN '2026-06-22T00:00:00Z' AND '2026-06-29T00:00:00Z'
|> select at, procedure, resource, committed
|> order by at
```

**Build a per-actor activity leaderboard for the audit log.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M3; features=where,aggregate,group by,order by
/sys/audit
|> where at > '2026-06-01T00:00:00Z'
|> aggregate count(*) as actions, count_distinct(resource) as resources_touched
   group by actor
|> order by actions DESC
```

**Reconcile audit entries with the connections that served them.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M3; features=join,where,select,order by
/sys/audit
|> join /sys/connections on /sys/audit.connection == /sys/connections.name
|> where /sys/audit.at > '2026-06-20T00:00:00Z'
|> select /sys/audit.at, /sys/audit.actor, /sys/audit.verb,
          /sys/connections.driver, /sys/connections.scope
|> order by /sys/audit.at DESC
```

**List every project and how many members each has.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M5; features=join,aggregate,group by,order by
/sys/projects
|> join /sys/projects/members on /sys/projects.id == /sys/projects/members.project_id
|> aggregate count(*) as members group by /sys/projects.name
|> order by members DESC
```

**Spin up a new project.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M5; features=insert into,values,returning
insert into /sys/projects
  values (name => 'q3-migration',
          owner => 'lead@acme.com',
          visibility => 'private')
|> returning id, name, owner
```

**Add a teammate to a project as a contributor.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M5; features=insert into,values,returning
insert into /sys/projects/members
  values (project_id => 'q3-migration',
          email => 'dana@acme.com',
          project_role => 'contributor')
|> returning project_id, email, project_role
```

**Remove a former contributor from a project without deleting their account.**

```qfs
# qfs-cookbook: grammar=core; milestone=M5; features=where,remove
/sys/projects/members
|> where project_id == 'q3-migration' AND email == 'rotated.off@acme.com'
|> remove
```

**Surface every change request still waiting on a second human to sign off.**

```qfs
# qfs-cookbook: grammar=core; milestone=M+; features=where,select,order by
# Approvals are data: a pending row needs a different person to approve it.
/sys/approvals
|> where status == 'pending'
|> select id, requested_by, action, resource, requested_at
|> order by requested_at
```

**Sign off on a pending approval — a second human approves the row.**

```qfs
# qfs-cookbook: grammar=core; milestone=M+; features=where,update,set,returning
# The approver must differ from requested_by; the engine enforces four-eyes at commit.
/sys/approvals
|> where id == 'apr-8842' AND status == 'pending'
|> update set status = 'approved', approved_by = 'security-lead@acme.com'
|> returning id, action, status, approved_by
```

**Audit self-approvals — find rows approved by the same person who requested them.**

```qfs
# qfs-cookbook: grammar=core; milestone=M+; features=where,select,order by
/sys/approvals
|> where status == 'approved' AND approved_by == requested_by
|> select id, action, resource, requested_by, approved_at
|> order by approved_at DESC
```

**Watch the slowest drivers by p95 latency right now.**

```qfs
# qfs-cookbook: grammar=core; milestone=M+; features=where,select,order by,limit
/sys/metrics
|> where window == '5m' AND metric == 'query_latency_p95'
|> select driver, value, unit, at
|> order by value DESC
|> limit 10
```

**Roll up query volume per driver over the last hour.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M+; features=where,aggregate,group by,order by
/sys/metrics
|> where metric == 'query_count' AND at > '2026-06-26T00:00:00Z'
|> aggregate sum(value) as total_queries group by driver
|> order by total_queries DESC
```

**Review this month's billable usage broken down by project.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M+; features=where,aggregate,group by,order by
/sys/billing
|> where period == '2026-06'
|> aggregate sum(amount) as spend, sum(units) as units group by project, sku
|> order by spend DESC
```

## AI over MCP

When qfs is mounted as an MCP server, an AI agent stops calling a dozen bespoke service APIs and instead speaks one language: it turns a teammate's plain-English ask into a single qfs statement, then runs it through the describe→preview→commit loop. **describe** tells the model what a path is and which columns exist; **preview** dry-runs the statement and reports its effect class — a read returns "reads only, 0 effects" and is safe to show immediately, a reversible write reports the draft or UPSERT it would make, and an irreversible step reports that it would send mail or merge a PR. The agent's job is text-to-SQL on the client side; qfs's job is to make the consequence of each statement legible before anything happens. Under the **default safety mode** reversible writes auto-commit within `POLICY` while irreversible `CALL`s wait for a human to approve, but the mode is selectable — a read-only "explore" mode refuses every effect, a "draft" mode lets reversible writes through, and a supervised "auto" mode can be granted narrow irreversible scopes — so the same generated statement is gated differently depending on how much the operator has delegated. Every recipe below is one statement the agent emitted in response to the natural-language title; the prose around the loop is the model's, the qfs is the contract.

**"What inboxes and databases am I even connected to?"**

```qfs
# qfs-cookbook: grammar=core; milestone=M2; features=select
# Pure read: preview reports "reads only, 0 effects". Names/metadata only — no secrets.
/sys/connections
|> select service, name, scopes
```

**"Which of my connections can actually write, not just read?"**

```qfs
# qfs-cookbook: grammar=core; milestone=M2; features=where,select,LIKE
# The agent inspects scope metadata only; preview confirms it touches nothing.
/sys/connections
|> where scopes LIKE '%write%'
|> select service, name, scopes
```

**"Show me the GitHub and Slack connections this agent is allowed to act as."**

```qfs
# qfs-cookbook: grammar=core; milestone=M2; features=where,select,IN,order by
/sys/connections
|> where service IN ('github', 'slack')
|> select service, name, scopes
|> order by service
```

**"Pull the ten most recent unread emails so I can triage them."**

```qfs
# qfs-cookbook: grammar=core; milestone=M2; features=where,select,order by,limit
# Exploratory read — preview: "reads only, 0 effects". The agent shows the table inline.
/mail/inbox
|> where unread == true
|> select from_addr, subject, received_at
|> order by received_at DESC
|> limit 10
```

**"How many open PRs does each reviewer have on their plate right now?"**

```qfs
# qfs-cookbook: grammar=extended; milestone=M2; features=where,aggregate,group by,order by
/github/acme/web/pulls
|> where state == 'open'
|> aggregate count() as open_prs group by requested_reviewer
|> order by open_prs DESC
```

**"What did the support channel talk about most this week?"**

```qfs
# qfs-cookbook: grammar=extended; milestone=M2; features=where,aggregate,group by,order by,limit
/slack/acme/support/messages
|> where ts > '2026-06-19'
|> aggregate count() as msgs group by user
|> order by msgs DESC
|> limit 5
```

**"Find customers who churned last quarter but still have an open invoice."**

```qfs
# qfs-cookbook: grammar=core; milestone=M2; features=where,join,on,select
# Cross-service read; preview reports zero effects, so the agent answers directly.
/sql/pg/customers
|> where churned_at BETWEEN '2026-01-01' AND '2026-03-31'
|> join /sql/pg/invoices on customers.id == invoices.customer_id
|> where invoices.status == 'open'
|> select customers.name, customers.email, invoices.amount, invoices.due_date
```

**"Read the Q2 plan in Drive and give me its objectives."**

```qfs
# qfs-cookbook: grammar=core; milestone=M2; features=decode,select
# Blob→relational read. Still "reads only, 0 effects" — decode doesn't write.
/drive/Plans/q2-plan.md
|> decode md
|> select owner, quarter, body
```

**"Diff the deploy config between the release tag and main."**

```qfs
# qfs-cookbook: grammar=core; milestone=M2; features=decode,select,except
/git/app@v2.1/deploy.toml
|> decode toml
|> select key, value
|> except
   /git/app@main/deploy.toml
   |> decode toml
   |> select key, value
```

**"Which assets in S3 aren't recorded in the catalog table?"**

```qfs
# qfs-cookbook: grammar=core; milestone=M2; features=select,except
/s3/media-prod
|> select key
|> except
   /sql/pg/assets
   |> select storage_key as key
```

**"Save this triage summary as an email draft for me to look over."**

```qfs
# qfs-cookbook: grammar=extended; milestone=M2; features=insert into,values,||
# Reversible write. Under the default mode the draft auto-commits within policy;
# preview reports "1 reversible effect: draft created in /mail/drafts".
insert into /mail/drafts
  values (to => 'lead@acme.com',
          subject => 'Inbox triage — ' || '2026-06-26',
          body => 'Drafted by the assistant. 10 unread, 3 urgent. Review before sending.')
```

**"Draft a reply asking each overdue-invoice customer to settle up."**

```qfs
# qfs-cookbook: grammar=extended; milestone=M2; features=where,insert into,values,||
# Reversible: drafts only. Nothing is sent — call mail.send would be a separate, gated step.
/sql/pg/invoices
|> where status == 'open' AND due_date < '2026-06-01'
|> insert into /mail/drafts
     values (to => customer_email,
             subject => 'Invoice ' || invoice_no || ' is past due',
             body => 'Hi — invoice ' || invoice_no || ' for ' || amount || ' is overdue.')
```

**"Upsert today's MRR snapshot into the metrics rollup table."**

```qfs
# qfs-cookbook: grammar=extended; milestone=M2; features=aggregate,extend,upsert into
# UPSERT is reversible (key-addressed); auto-commits within policy under default mode.
/sql/pg/subscriptions
|> where status == 'active'
|> aggregate sum(mrr) as mrr group by plan
|> extend snapshot_date = '2026-06-26'
|> upsert into /sql/pg/mrr_daily
```

**"Mirror the weekly report into the shared Drive folder as a CSV."**

```qfs
# qfs-cookbook: grammar=core; milestone=M2; features=where,encode,upsert into
# Reversible UPSERT to storage — safe to auto-commit; preview shows the target key.
/sql/pg/weekly_report
|> where week == '2026-W26'
|> encode csv
|> upsert into /drive/Reports/weekly-2026-W26.csv
```

**"Tag every stale open issue so the backlog grooming bot picks them up."**

```qfs
# qfs-cookbook: grammar=core; milestone=M2; features=where,update,set
# Reversible field update; the preview lists the affected issue numbers before commit.
/github/acme/web/issues
|> where state == 'open' AND updated_at < '2026-03-26'
|> update set label = 'stale'
```

**"Drop the processed upload keys from the queue table."**

```qfs
# qfs-cookbook: grammar=core; milestone=M2; features=where,remove
# remove is irreversible: preview enumerates the exact rows it would delete, and the
# commit needs --commit-irreversible. (It cannot appear inside a transaction, which is
# reversible-only.) Reach for update set a soft-delete flag if you need it undoable.
/sql/pg/upload_queue
|> where status == 'processed'
|> remove
```

**"Stage the new pricing file into the staging bucket from the source one."**

```qfs
# qfs-cookbook: grammar=core; milestone=M2; features=encode,upsert into
/s3/source/pricing-2026.json
|> decode json
|> encode json
|> upsert into /s3/staging/pricing-2026.json
```

**"Now actually send those overdue-invoice reminders."**

```qfs
# qfs-cookbook: grammar=extended; milestone=M2; features=where,call,||
# Irreversible: call mail.send. Under the default safety mode preview reports
# "irreversible effect — awaiting approval"; the agent surfaces it and waits for a human.
/sql/pg/invoices
|> where status == 'open' AND due_date < '2026-06-01'
|> call mail.send(to => customer_email,
                  subject => 'Final reminder: invoice ' || invoice_no,
                  body => 'This invoice is now seriously overdue. Please settle promptly.')
```

**"Merge the release PR — it's approved."**

```qfs
# qfs-cookbook: grammar=extended; milestone=M2; features=call
# Irreversible merge. The default mode never auto-commits this; the human approves
# the previewed effect, or runs an "auto" mode that was granted github.merge scope.
call github.merge(number => 318, method => 'squash')
```

**"Post the deploy-done note to the releases channel."**

```qfs
# qfs-cookbook: grammar=extended; milestone=M2; features=call,||
# Posting to a channel is irreversible (you can't unsay it); preview flags it for approval.
call slack.post(channel => 'eng-releases',
                text => 'v2.1 is live in production. ' || 'Rollback runbook is pinned.')
```

**"Kick off the nightly export workflow now instead of waiting."**

```qfs
# qfs-cookbook: grammar=extended; milestone=M2; features=call
# Irreversible side effect (it starts real work). Default mode: awaits human approval.
call ci.dispatch(workflow => 'nightly-export', ref => 'main')
```

**"Reply on issue 204 that we've shipped the fix."**

```qfs
# qfs-cookbook: grammar=extended; milestone=M2; features=call,||
/github/acme/web/issues/204
|> call github.comment(number => 204,
                       body => 'Fixed in v2.1, released today. ' || 'Closing once verified.')
```

**"Which inbox messages mention 'refund' and came from a paying customer?"**

```qfs
# qfs-cookbook: grammar=core; milestone=M2; features=where,join,on,select,LIKE
# Exploratory cross-service read; preview: "reads only, 0 effects".
/mail/inbox
|> where subject LIKE '%refund%' OR body LIKE '%refund%'
|> join /sql/pg/customers on inbox.from_addr == customers.email
|> where customers.plan <> 'free'
|> select inbox.from_addr, inbox.subject, customers.plan
```

**"Build the win-back list: active GA visitors who lapsed in the orders table."**

```qfs
# qfs-cookbook: grammar=core; milestone=M2; features=select,intersect
/ga/acme-prod/sessions
|> where event == 'session_start'
|> select user_email as email
|> intersect
   /sql/pg/orders
   |> where last_order_at < '2026-03-27'
   |> select email
```

**"Draft win-back emails to that lapsed list, but don't send them yet."**

```qfs
# qfs-cookbook: grammar=extended; milestone=M2; features=where,insert into,values,||
# Reversible drafting step; the send is a separate, approval-gated call the human runs later.
/sql/pg/orders
|> where last_order_at < '2026-03-27'
|> insert into /mail/drafts
     values (to => email,
             subject => 'We saved your spot, ' || name,
             body => 'It has been a while, ' || name || '. Here is 20% off to come back.')
```

**"Snapshot the audit log of who changed connections this month into a table."**

```qfs
# qfs-cookbook: grammar=core; milestone=M2; features=where,select,upsert into
/sys/audit
|> where action LIKE 'connection.%' AND at > '2026-06-01'
|> select actor, action, target, at
|> upsert into /sql/pg/connection_audit_june
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
# qfs-cookbook: grammar=core; milestone=M7; features=where,select,order by
# Live status of every running Claude Code session on one host.
/hosts/buildbox-01/claude/sessions
|> where status == 'running'
|> select task, progress, last_message, started_at
|> order by progress
```

**Find sessions that have stalled — running but silent for a while.**

```qfs
# qfs-cookbook: grammar=core; milestone=M7; features=where,select,order by,AND
/hosts/buildbox-01/claude/sessions
|> where status == 'running' AND last_message_at < '2026-06-26T09:00:00Z'
|> select id, task, last_message, last_message_at
|> order by last_message_at
```

**Steer one agent: nudge the current session to write tests before it ships.**

```qfs
# qfs-cookbook: grammar=core; milestone=M7; features=insert into,values
# A reversible write into the session's instruction channel; POLICY bounds who may steer this host.
insert into /hosts/buildbox-01/claude/sessions/current/instructions
  values ('Add unit tests for the new parser before opening the PR, then run cargo test --workspace.')
```

**Redirect a specific stuck session to a smaller, safer scope.**

```qfs
# qfs-cookbook: grammar=core; milestone=M7; features=insert into,values
insert into /hosts/buildbox-01/claude/sessions/sess_9f2a/instructions
  values ('Stop the refactor. Just fix the failing test in tokenizer.rs and report back.')
```

**Pull the last thing every agent said across one host, newest first.**

```qfs
# qfs-cookbook: grammar=core; milestone=M7; features=select,order by,limit
/hosts/buildbox-01/claude/sessions
|> select host, id, task, status, last_message
|> order by last_message_at
|> limit 50
```

**Count how each host's agents are spread across statuses.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M7; features=aggregate,group by
/hosts/buildbox-01/claude/sessions
|> aggregate count() as sessions, max(progress) as furthest group by status
```

**Fan one read across the whole pool: every running agent on every host.**

```qfs
# qfs-cookbook: grammar=core; milestone=M+; features=where,select,order by
# A wildcard host segment fans the read across the federated fabric; policy scopes each host's rows.
/hosts/*/claude/sessions
|> where status == 'running'
|> select host, task, progress, last_message
|> order by host, progress
```

**Mesh roll-up: how many agents are busy on each machine right now.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M+; features=where,aggregate,group by,order by
/hosts/*/claude/sessions
|> where status == 'running'
|> aggregate count() as busy, min(progress) as least_done group by host
|> order by busy
```

**Broadcast a steer: tell every idle agent across the fleet to pick up the queue.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M+; features=where,insert into,values
# One pipeline writes an instruction into each matched session; the tunnel needs a Cloud sign-in.
/hosts/*/claude/sessions
|> where status == 'idle'
|> insert into /hosts/*/claude/sessions/instructions
     values (host => host, session => id,
             text => 'Claim the next ticket from /sys/projects/qfs/queue and start it.')
```

**Coordinator: collect finished agents' results from several hosts into one list.**

```qfs
# qfs-cookbook: grammar=core; milestone=M+; features=where,select,order by
/hosts/*/claude/sessions
|> where status == 'done'
|> select host, task, result, finished_at
|> order by finished_at
```

**Merge agent state with the tickets they were assigned (cross-surface JOIN).**

```qfs
# qfs-cookbook: grammar=core; milestone=M+; features=join,on,where,select
/hosts/*/claude/sessions
|> join /sql/pg/agent_tasks on sessions.task_id == agent_tasks.id
|> where sessions.status == 'running'
|> select host, agent_tasks.title, agent_tasks.priority, sessions.progress
```

**Cross-machine federation read: list source files an agent on another host is touching.**

```qfs
# qfs-cookbook: grammar=core; milestone=M+; features=where,select,order by
# /git on a remote host resolves over the tunnel; every blob read is policy-bounded.
/hosts/gpu-rig-02/claude/sessions/current/changed_files
|> where staged == true
|> select path, additions, deletions
|> order by additions
```

**Pool the team's open PRs that agents authored across all hosts.**

```qfs
# qfs-cookbook: grammar=core; milestone=M+; features=where,join,on,select
/hosts/*/claude/sessions
|> where status == 'done'
|> join /github/acme/web/pulls on sessions.pr_number == pulls.number
|> select host, pulls.number, pulls.title, pulls.state
```

**Stand up a team-wide connection at the project level so every host shares it.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M9; features=insert into,values
# A project connection is visible to all hosts in the team, gated by policy.
insert into /sys/projects/qfs/connections
  values (name => 'pg-prod', driver => 'sql/pg',
          dsn => 'postgres://reader@db.acme.internal/app', scope => 'team')
```

**Promote one engineer's personal connection to a shared team connection.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M9; features=where,insert into,values
/sys/connections
|> where name == 'slack-eng' AND owner == 'a@qmu.jp'
|> insert into /sys/projects/qfs/connections
     values (name => name, driver => driver, scope => 'team')
```

**Audit which team connections every host can currently reach.**

```qfs
# qfs-cookbook: grammar=core; milestone=M9; features=where,select,order by
/sys/projects/qfs/connections
|> where scope == 'team'
|> select name, driver, owner, last_used_at
|> order by last_used_at
```

**Check which hosts have actually joined the fabric and when they last reported.**

```qfs
# qfs-cookbook: grammar=core; milestone=M+; features=where,select,order by
/sys/projects/qfs/hosts
|> where online == true
|> select host, agent_count, last_heartbeat_at, cloud_signed_in
|> order by last_heartbeat_at
```

**Reusable filter: define "busy host" once, then count and rank with it (functional core).**

```qfs
# qfs-cookbook: grammar=extended; milestone=M+; features=let,=>,filter,aggregate,group by
let is_busy = (s: Session) => s.status = 'running' AND s.progress < 0.9
/hosts/*/claude/sessions
|> where filter(self, is_busy)
|> aggregate count() as still_busy group by host
```

**Bind the running fleet once, then both summarize it and pick the laggards.**

```qfs
# qfs-cookbook: grammar=core; milestone=M+; features=let,where,join,on,select
let running = (/hosts/*/claude/sessions |> where status == 'running')
running
|> join /sys/projects/qfs/hosts on running.host == hosts.host
|> where hosts.cloud_signed_in == true
|> select running.host, running.task, running.progress, hosts.region
```

**Coordinator fan-out: queue one task per online host as a steerable instruction.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M+; features=where,insert into,values,||
/sys/projects/qfs/hosts
|> where online == true
|> insert into /hosts/*/claude/sessions/instructions
     values (host => host, session => 'next',
             text => 'You are shard ' || host || '. Build and test only crates owned by this host.')
```

**Reduce many agents' progress into a single fleet completion figure.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M+; features=let,reduce,=>,where,select
let avg_progress = (rows: Relation) =>
  reduce(rows.progress, (acc: Float, p: Float) => acc + p, 0.0) / count(rows)
/hosts/*/claude/sessions
|> where status == 'running'
|> select avg_progress(self) as fleet_progress
```

**Spot conflicts: two agents editing the same file on different hosts.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M+; features=join,on,where,select,distinct
/hosts/*/claude/sessions/current/changed_files as a
|> join /hosts/*/claude/sessions/current/changed_files as b on a.path == b.path
|> where a.host <> b.host
|> select distinct a.path, a.host, b.host
```

**Collect every agent's final result and drop it into a shared report blob.**

```qfs
# qfs-cookbook: grammar=core; milestone=M+; features=where,select,encode,upsert into
/hosts/*/claude/sessions
|> where status == 'done'
|> select host, task, result, finished_at
|> encode md
|> upsert into /drive/Team/agent-run-2026-06-26.md
```

**Post a mesh standup to Slack: who finished what, grouped by host.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M+; features=where,aggregate,group by,insert into,values,||
/hosts/*/claude/sessions
|> where status == 'done'
|> aggregate count() as shipped group by host
|> insert into /slack/acme/eng-fabric/messages
     values (text => host || ' shipped ' || shipped || ' tasks this run.')
```

**Halt the fleet on red: tell every running agent to pause when CI is failing.**

```qfs
# qfs-cookbook: grammar=extended; milestone=M+; features=where,insert into,values
/hosts/*/claude/sessions
|> where status == 'running'
|> insert into /hosts/*/claude/sessions/instructions
     values (host => host, session => id,
             text => 'CI on main is red. Stop pushing and wait for the all-clear.')
```

**Reconcile the mesh against policy: which sessions a remote host refused to expose.**

```qfs
# qfs-cookbook: grammar=core; milestone=M+; features=where,select,order by
# Cross-machine reads are policy-bounded; the audit log records what each host withheld.
/sys/audit
|> where action == 'fabric.read' AND outcome == 'denied'
|> select host, principal, path, reason, at
|> order by at
```

**Cross-machine federation: JOIN one agent's commits with the team's PR review state.**

```qfs
# qfs-cookbook: grammar=core; milestone=M+; features=join,on,where,select,order by
/hosts/gpu-rig-02/claude/sessions/current/commits
|> join /github/acme/web/pulls on commits.sha == pulls.head_sha
|> where pulls.review_state <> 'approved'
|> select commits.sha, commits.message, pulls.number, pulls.review_state
|> order by commits.committed_at
```

**Rank hosts by how much idle capacity they have for the coordinator to schedule.**

```qfs
# qfs-cookbook: grammar=core; milestone=M+; features=join,on,select,order by
/sys/projects/qfs/hosts
|> join /sys/metrics on hosts.host == metrics.host
|> select hosts.host, hosts.agent_count, metrics.cpu_idle_pct, metrics.free_mem_gb
|> order by metrics.cpu_idle_pct
```
