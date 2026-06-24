# Cookbook: Code — git, GitHub, Slack

## git — versioned files and history

A git repo is both a **versioned file tree** and a **history**. The `@<ref>` coordinate reads a path
as of a tag, branch, or commit.

**Read a file as it was at a tag:**

```qfs
FROM /git/myrepo@v1.2/src/main.rs
```

**List a directory at a specific commit:**

```qfs
FROM /git/myrepo@9f2c1a/src
|> SELECT path
```

**Record a commit** (history is append-only; this adds one commit):

```qfs
INSERT INTO /git/myrepo/commits
  VALUES ('add feature', 'main')
```

## GitHub — pull requests and issues

GitHub is an **object graph**: things (PRs, issues) with actions you `CALL`.

**List open pull requests, newest first:**

```qfs
FROM /github/acme/web/pulls
|> WHERE state = 'open'
|> SELECT number, title
|> ORDER BY number DESC
|> LIMIT 10
```

**Squash-merge a pull request** (irreversible — a gate):

```qfs
FROM /github/acme/web/pulls/42
|> CALL github.merge(method => 'squash')
```

::: warning Irreversible
A merge can't be undone. In a one-shot it needs `--commit --commit-irreversible`.
:::

## Slack — team chat as an append log

A Slack channel is an **append log**: read the tail, append a message.

**Post a message:**

```qfs
INSERT INTO /slack/acme/general/messages
  VALUES ('Deploy finished ✅')
```

**Read the latest messages in a channel:**

```qfs
FROM /slack/acme/general/messages
|> SELECT text
|> LIMIT 20
```

::: tip
Want a deploy to post to Slack by itself? Wire it up once with a trigger — see
[Automation](/cookbook/automation).
:::
