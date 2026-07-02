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
    details: A pipe-SQL grammar — /path |> WHERE … |> SELECT … |> JOIN. Filter mail, join a database to GitHub, transcode JSON→CSV — the same way, everywhere.
  - title: Preview before you commit
    details: Every command shows exactly what it will do first. Nothing touches the real world until you add --commit. Irreversible actions (sending mail, merging a PR) need an explicit extra OK.
  - title: Safe for AI agents
    details: One small grammar instead of a hundred vendor SDKs. An agent learns it once and drives every service over the same loop — on the CLI, through the MCP endpoint, or via the web dashboard's approval cards — with previews and least-privilege policies as guardrails.
---

## See it

Some of these run today against local files and a SQLite database — no account, no setup
beyond a path. The rest show what the same grammar does the moment a service is connected.

**Turn a JSON file into YAML** — the codec stages genuinely transcode:

```qfs
/local/config.json
|> decode json
|> encode yaml
```

Given a `config.json` of `{"k":1,"name":"alpha"}`, qfs emits the YAML:

```yaml
- k: 1
  name: alpha
```

**Query a database — and the filter runs *inside* it.** `WHERE` is pushed down to SQL;
ordering and projection ride the pipe:

```qfs
/sql/sales/orders
|> where total > 100
|> select customer, total
|> order by total DESC
```

Then the breadth — the same small grammar reaching across services. These need a connected
account, but they are the whole point of qfs.

**Find unread invoices in your inbox:**

```qfs
/mail/inbox
|> where subject LIKE '%invoice%'
|> select date, from, subject
|> order by date DESC
```

**Join a database table to your GitHub issues — across two completely different services:**

```qfs
/sql/sales/orders
|> join /github/acme/web/issues on id == issue_id
|> select id, title
```

The `/sql` leg reads today; the GitHub leg needs a connected account —
`qfs account add github work`, then `qfs connect /github --driver github --account work`.

**Automate it — every time mail lands, post to Slack:**

```qfs
create trigger notify
  on /mail/inbox
  do insert into /slack/acme/general/messages values (NEW.subject)
```

You **preview** each one to see precisely what would happen — the trigger above previews as a
pure plan that fires nothing until you wire it up — then add `--commit` to make it real.

## One engine, three faces

The same engine — and the same preview-then-commit safety model — answers on three faces: the
**CLI** (and an FTP-like interactive shell), an **MCP endpoint** that exposes the describe → preview
→ commit loop as tools for an AI agent, and an **embedded web dashboard** where a human approves a
pending irreversible commit on an approval card. You learn the loop once and it works everywhere.

**Next:** [Install qfs](/guide/installation) · [Get started](/guide/getting-started) ·
[The Cookbook](/cookbook/)
