---
name: qfs-cookbook
description: Use for an overview of what qfs can do and which qfs cookbook skill fits a task — reading, searching, transforming, or writing across Gmail, Google Drive, databases, files, git, GitHub, Slack, and automation with one pipe-SQL language. Routes to the per-service skills.
---

# Cookbook

Every external service, one language. Mail, a database, a repo, a channel, a bucket — each becomes a
tree of paths you query with the same filesystem-shaped pipe-SQL.

::: tip Start here — unlock your credential store
`/local`, `/sys`, and local files/repos need no setup. **Every connected service below** stores a
login behind your `QFS_PASSPHRASE`, so do the one-time **[The QFS passphrase](/guide/passphrase)**
step first — without it you can't connect Gmail, Drive, GitHub, Slack, databases, or object storage.
:::

## One query shape, any service

**Find the invoices in your inbox, newest first** — search a mailbox with `where`, `select`,
`order by`, `limit`:

```qfs
/mail/inbox
|> where subject LIKE '%invoice%'
|> select date, from, subject
|> order by date DESC
|> limit 20
```

Learn that shape once and you already know how to read every other service. Swap `/mail/inbox` for
`/sql/<conn>/<table>`, a `/git` tree, or a `/slack` channel and the same `where … select … order by
… limit` pipe just works — that's the whole promise: no per-service API to relearn, one grammar over
all of them.

::: tip Reads return rows; writes preview
A **read** runs immediately and returns rows — `/local`, `/sys`, a `/sql` table, and a `/git` repo
all read today. A read of a cloud service you haven't connected (mail, GitHub, Slack, S3, Drive)
returns an *actionable* error telling you exactly which `qfs connection add …` to run — it never
silently fails.

A **write** (`insert`, `update`, `upsert`, `remove`, `call`) **previews** by default: `qfs run`
shows the plan and changes nothing. Add `--commit` to act, and `--commit-irreversible` for things
that can't be undone, like sending mail or merging a PR. So paste any recipe and safely see what it
*would* do first.
:::

A section heading marked **🚧** has a part that isn't wired yet — the recipes shown work, but the
section's callout notes what's still coming (today that's object-store writes and reading a git
blob's bytes at a `@ref`).

## The chapters

One cookbook per service — each opens with how to connect it, then the recipes that solve real
tasks. Jump to the one you need:

- **[Gmail](/cookbook/gmail)** — search, triage, draft, send, label, and clean up a whole mailbox.
- **[Google Drive](/cookbook/gdrive)** — browse My Drive and Shared Drives, download, upload, create
  folders, trash.
- **[Databases](/cookbook/databases)** — filter, aggregate, update, and set operations over SQL
  tables.
- **[git](/cookbook/git)** — read a versioned file tree and history, browse it at any ref, record a
  commit.
- **[GitHub](/cookbook/github)** — list and filter pull requests and issues; merge a PR.
- **[Slack](/cookbook/slack)** — read a channel's latest messages; post a message.
- **[Files & object storage](/cookbook/files)** — local files and S3/R2, plus format conversion with
  codecs.
- **[Cross-service](/cookbook/cross-service)** — one query spanning several services: join a database
  to GitHub, a file to a table.
- **[Automation (server)](/cookbook/automation)** — turn any query into a trigger, a scheduled job,
  an HTTP endpoint, or a cached view.

New to qfs? Start with [Your first queries](/guide/getting-started) and
[How qfs works](/guide/concepts).
