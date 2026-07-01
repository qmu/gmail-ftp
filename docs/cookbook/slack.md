---
skill_name: qfs-slack
skill_description: Use when a task needs Slack through qfs — read the latest messages in a channel and post a message over /slack, as an append log. Covers connecting a Slack workspace.
---

# Cookbook: Slack

A Slack channel is an **append log**: read the tail, append a message. qfs pre-mounts nothing —
connect a workspace, then read at `/slack/<workspace>/<channel>/messages`.

## Setup

A Slack read needs a workspace:

```sh
qfs connection add slack
```

Until connected, a read returns the actionable *connect a Slack workspace to read it — run
`qfs connection add slack`*. Posting a message previews with no account (below); it sends only once
connected and committed.

## Read & post

**Read the latest messages in a channel:**

```qfs
/slack/acme/general/messages
|> select text
|> limit 20
```

**Post a message** (previews the append; applies nothing until `--commit`):

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
