---
name: qfs-github
description: Use when a task needs GitHub through qfs — list and filter pull requests and issues over /github, and merge a PR with a CALL procedure behind the irreversible gate. Covers connecting a GitHub account.
---

# Cookbook: GitHub

GitHub is an **object graph**: things (pull requests, issues) with actions you `CALL`. qfs pre-mounts
nothing — connect an account, then read at `/github/<owner>/<repo>/…`.

## Setup

A GitHub read (and the `CALL` that targets a PR) needs a token:

```sh
qfs connection add github
```

Until connected, a read returns the actionable *connect a GitHub account to read it — run
`qfs connection add github`*.

## Pull requests & issues

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
A merge can't be undone, so in a one-shot it needs `--commit --commit-irreversible`. Reads and the
`PREVIEW` of the merge run with no extra flags once the account is connected.
:::
