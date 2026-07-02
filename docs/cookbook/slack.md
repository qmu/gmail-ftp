---
skill_name: qfs-slack
skill_description: Use when a task needs Slack through qfs — read the latest messages in a channel and post a message over /slack, as an append log. Covers connecting a Slack workspace.
---

# Slack

A Slack channel is an **append log** with a filesystem shape: its messages become a queryable path
you read the tail of, and post to — the same pipe-SQL language you already use on a mailbox, a
database, or a git repo.

## See it work first

**Catch up on a channel** — the latest messages in `#general`, newest first:

```qfs
/slack/acme/general/messages
|> select ts, user, text
|> order by ts DESC
|> limit 20
```

```text
ts                   user     text
2026-06-30 16:42     jordan   shipping the Q3 build now 🚀
2026-06-30 15:10     priya    review's done, LGTM
2026-06-30 11:58     taylor   standup moved to 10:30 tomorrow
… 20 rows
```

That read runs the instant you connect a workspace. Posting back is just as direct — one statement
appends a message, and previews before it sends anything:

```qfs
insert into /slack/acme/general/messages
  values ('Deploy finished ✅')
```

```text
PREVIEW: 1 effect(s)
  #0 INSERT -> slack:/slack/acme/general/messages [affected 1]
  total affected: 1
```

::: tip Reads run now; writes preview
Every **read** returns rows immediately. Every **write** (`insert`) *previews* by default and posts
nothing — add `--commit` to actually send it. Paste any recipe below and safely watch what it
*would* do first.
:::

Slack isn't reachable until you connect a workspace to a path — one command, in **[Setup](#setup)**.
After that every recipe on this page works verbatim.

## Setup

::: tip Prerequisites — an operator, an account, a mount
Reaching a cloud service takes three one-time steps: a signed-in operator (`qfs init` —
**[The operator identity](/guide/operator)**), an authorized account (`qfs account add …`), and a
mount binding that account to a path (`qfs connect …`). The happy path below is exactly those
three.
:::

A Slack read needs a workspace token bound to a mount:

```sh
qfs init you@example.com                               # 1. the operator + the vault (once per machine)
printf '%s' "$SLACK_TOKEN" | qfs account add slack     # 2. the workspace token (label: `default`)
qfs connect /slack --driver slack --account default    # 3. mount it at /slack
```

The token comes in on **stdin**, never argv, and is sealed in qfs's encrypted credential store.
Until the mount is bound, a read fails with an actionable hint naming the
`qfs account add slack …` / `qfs connect …` to run. Posting a message previews with no account
(above); it sends only once connected and committed.

## The channel as a path

Once connected, a workspace's channels hang off `/slack` in a filesystem shape:

| Slack thing | qfs path | it is a… |
| ----------- | -------- | -------- |
| a workspace | `/slack/acme` | directory of channels |
| a channel's log | `/slack/acme/general/messages` | the append log you read and post to |

Message columns: `ts`, `user`, `text`. Run `qfs describe /slack/acme/general/messages` for the exact
schema and verbs of the node.

## Read the channel

**Read the latest messages** — the tail of the log:

```qfs
/slack/acme/general/messages
|> select text
|> limit 20
```

**Search a channel for anything that looks like an incident** — `WHERE` narrows the log before it
comes back:

```qfs
/slack/acme/incidents/messages
|> where text ~ '(?i)(outage|sev[0-9]|rollback|paging)'
     OR text LIKE '%down%'
|> select ts, user, text
|> order by ts DESC
|> limit 100
```

## Post a message

**Post to a channel** — an `INSERT` appends to the log. It previews the append and applies nothing
until `--commit`:

```qfs
insert into /slack/acme/general/messages
  values ('Deploy finished ✅')
```

```text
PREVIEW: 1 effect(s)
  #0 INSERT -> slack:/slack/acme/general/messages [affected 1]
  total affected: 1
```

::: tip
Want a deploy to post to Slack by itself? Wire it up once with a trigger — see
[Automation](/cookbook/automation).
:::
