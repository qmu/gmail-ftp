---
name: qfs-code
description: Use when a task needs git, GitHub, or Slack through qfs — versioned file reads and commits over /git, pull requests and issues over /github, and channels and messages over /slack.
---

# Cookbook: Code — git, GitHub, Slack

## git — versioned files and history

A git repo is both a **versioned file tree** and a **history**. Register a repo with
`QFS_GIT_<REPO>=<path-to-repo-or-.git>`, then read its facets at `/git/<repo>/…`. The `@<ref>`
coordinate (in the path) reads the tree as of a tag, branch, or commit. git reads run today against
your local repo — loose *and* packed objects.

**List the commit history:**

```qfs
/git/myrepo/commits
|> select sha, message
```

```text
sha                                      | message
---------------------------------------- | ---------------
4d3a66d9a60ed9f255d1779ffd01f495f1dd93b9 | add feature
b91002fbb81daeb44649f0350ed2f32a68eb2491 | initial commit
(2 row(s))
```

The `commits` facet carries `sha`, `tree`, `parents`, `author`, `committer`, `time`, and `message`,
so you can filter it like any table:

```qfs
/git/myrepo/commits
|> where message LIKE 'add%'
|> select sha, message
```

```text
sha                                      | message
---------------------------------------- | ------------
4d3a66d9a60ed9f255d1779ffd01f495f1dd93b9 | add feature
(1 row(s))
```

**List refs and tags:**

```qfs
/git/myrepo/refs
|> select name, oid
```

```text
name              | oid
----------------- | ----------------------------------------
HEAD              | 4d3a66d9a60ed9f255d1779ffd01f495f1dd93b9
master            | 4d3a66d9a60ed9f255d1779ffd01f495f1dd93b9
refs/heads/master | 4d3a66d9a60ed9f255d1779ffd01f495f1dd93b9
refs/tags/v1      | 4d3a66d9a60ed9f255d1779ffd01f495f1dd93b9
(4 row(s))
```

**List a directory as of a tag** — the `@<ref>` coordinate reads the tree at that point:

```qfs
/git/myrepo@v1/src
|> select name, kind
```

```text
name    | kind
------- | ----
main.rs | blob
(1 row(s))
```

::: tip Reading file *bytes* at a ref is coming soon
`@<ref>` tree **listing** works today (`name`, `mode`, `oid`, `kind`). Reading the *content* of a
single blob at a ref — e.g. `/git/myrepo@v1/src/main.rs` — is not wired yet (it returns
`invalid_path`). Use the tree listing to navigate, and read the file from `/local` for now.
:::

**Record a commit** (history is append-only; this previews the effect, applying nothing):

```qfs
insert into /git/myrepo/commits
  values ('add feature', 'main')
```

```text
PREVIEW: 4 effect(s)
  #0 INSERT -> git:/git/myrepo/commits [affected 1]
  #1 INSERT -> git:/git/myrepo/commits [affected 1]
  #2 UPDATE -> git:/git/myrepo/commits [affected 1]
  #3 UPDATE -> git:/git/myrepo/commits [affected 1]
  total affected: 4
```

(Recording a commit fans out into several object writes — the blob, the tree, the commit, and the
ref update — which is why the plan lists four effects.)

## GitHub — pull requests and issues

GitHub is an **object graph**: things (PRs, issues) with actions you `CALL`.

**List open pull requests, newest first:**

```qfs
/github/acme/web/pulls
|> where state == 'open'
|> select number, title
|> order by number DESC
|> limit 10
```

**Squash-merge a pull request** (irreversible — a gate):

```qfs
/github/acme/web/pulls/42
|> call github.merge(method => 'squash')
```

::: warning Needs a connected account
GitHub reads (and the `CALL` that targets a PR) need a token: *connect a GitHub account to read it —
run `qfs connection add github`*. Once connected, these run; a merge then needs
`--commit --commit-irreversible`, because it can't be undone.
:::

## Slack — team chat as an append log

A Slack channel is an **append log**: read the tail, append a message.

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

**Read the latest messages in a channel:**

```qfs
/slack/acme/general/messages
|> select text
|> limit 20
```

::: warning Needs a connected account
A Slack **read** needs a workspace: *connect a Slack workspace to read it — run
`qfs connection add slack`*. Posting a message previews with no account (above); it sends only once
connected and committed.
:::

::: tip
Want a deploy to post to Slack by itself? Wire it up once with a trigger — see
[Automation](/cookbook/automation).
:::
