---
layout: home
hero:
  name: qfs
  text: One query language for every service you use
  tagline: Mail, files, databases, GitHub, Slack, git, cloud storage — all addressed like a filesystem and queried with one small pipe-SQL language. A single binary. Nothing happens until you say COMMIT.
  actions:
    - theme: brand
      text: Get started
      link: /guide/getting-started
    - theme: alt
      text: Cookbook →
      link: /cookbook/
    - theme: alt
      text: How it works
      link: /guide/concepts
features:
  - title: Everything is a path
    details: "/mail/inbox, /sql/pg/orders, /github/acme/web/pulls, /drive/Reports, /s3/bucket/key — every service is a tree of paths you can list, read, and write."
  - title: One language, every backend
    details: A pipe-SQL grammar — FROM … |> WHERE … |> SELECT … |> JOIN. Filter mail, join a database to GitHub, transcode JSON→CSV — the same way, everywhere.
  - title: Preview before you commit
    details: Every command shows exactly what it will do first. Nothing touches the real world until you add --commit. Irreversible actions (sending mail, merging a PR) need an explicit extra OK.
  - title: Safe for AI agents
    details: One small grammar instead of a hundred vendor SDKs. An agent learns it once and drives every service — with previews and least-privilege policies as guardrails.
---

## See it

Find unread invoices in your inbox:

```qfs
FROM /mail/inbox
|> WHERE subject LIKE '%invoice%'
|> SELECT date, from, subject
|> ORDER BY date DESC
```

Join a database table to your GitHub issues — across two completely different services:

```qfs
FROM /sql/pg/orders
|> JOIN /github/acme/web/issues ON id = issue_id
|> SELECT id, title
```

Turn a JSON file into a YAML file:

```qfs
FROM /local/config.json
|> DECODE json
|> ENCODE yaml
```

Automate it — every time mail lands, post to Slack:

```qfs
CREATE TRIGGER notify
  ON /mail/inbox
  DO INSERT INTO /slack/acme/general/messages VALUES (NEW.subject)
```

You **preview** each one to see precisely what would happen, then add `--commit` to make it real.

**Next:** [Install qfs](/guide/installation) · [Your first queries](/guide/getting-started) ·
[The Cookbook](/cookbook/)
