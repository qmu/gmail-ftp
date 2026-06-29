# Cookbook

Real tasks, and the one qfs statement that solves each. Every recipe is grouped by what you're
working with; use the sidebar to jump around.

## How to read a recipe

Each recipe is a short statement. Multi-stage queries are written one stage per line, with the `|>`
pipe leading each line — read it top to bottom like a series of steps:

```qfs
/mail/inbox
|> where subject LIKE '%invoice%'
|> select date, from, subject
|> order by date DESC
|> limit 20
```

::: tip Preview first, always
`qfs run` **previews** by default — it shows the plan and changes nothing. Add `--commit` to act
(and `--commit-irreversible` for things that can't be undone, like sending mail or merging a PR).
So paste any recipe and safely see what it *would* do first.
:::

## The chapters

- **[Mail](/cookbook/mail)** — search, triage, draft, send, label, clean up.
- **[Databases](/cookbook/databases)** — filter, aggregate, update, set operations.
- **[Files & storage](/cookbook/files)** — local, Drive, S3/R2, and format conversion with codecs.
- **[Cross-service](/cookbook/cross-service)** — join a database to GitHub, a file to a table, one
  query spanning several services.
- **[Code: git, GitHub, Slack](/cookbook/code)** — versioned reads, pull requests, chat.
- **[Automation (server)](/cookbook/automation)** — turn any query into a trigger, a scheduled job,
  an HTTP endpoint, or a cached view.

New to qfs? Start with [Your first queries](/guide/getting-started) and
[How qfs works](/guide/concepts).
