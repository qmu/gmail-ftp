# Cookbook: Code — git, GitHub, Slack

## git — versioned files and history

A git repo is both a **versioned file tree** and a **history**. The `@<ref>` coordinate reads a path
as of a tag, branch, or commit.

**Read a file as it was at a tag:**

```qfs
/git/myrepo@v1.2/src/main.rs
```

**List a directory at a specific commit:**

```qfs
/git/myrepo@9f2c1a/src
|> select path
```

**Record a commit** (history is append-only; this adds one commit):

```qfs
insert into /git/myrepo/commits
  values ('add feature', 'main')
```

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

::: warning Irreversible
A merge can't be undone. In a one-shot it needs `--commit --commit-irreversible`.
:::

## Slack — team chat as an append log

A Slack channel is an **append log**: read the tail, append a message.

**Post a message:**

```qfs
insert into /slack/acme/general/messages
  values ('Deploy finished ✅')
```

**Read the latest messages in a channel:**

```qfs
/slack/acme/general/messages
|> select text
|> limit 20
```

::: tip
Want a deploy to post to Slack by itself? Wire it up once with a trigger — see
[Automation](/cookbook/automation).
:::
