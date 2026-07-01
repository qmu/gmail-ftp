---
skill_name: qfs-github
skill_description: Use when a task needs GitHub through qfs — list and filter pull requests and issues over /github, and merge a PR with a CALL procedure behind the irreversible gate. Covers connecting a GitHub account.
---

# GitHub

Every pull request, issue, review, and release in a repo becomes a queryable path. One pipe-SQL
language filters your open PRs, reports throughput, tags stale issues, and merges a PR — the same
verbs you already use on a mailbox, a database, or a folder of files.

## See it work first

**Show me the open pull requests, newest first** — the ones actually waiting on someone:

```qfs
/github/acme/web/pulls
|> where state == 'open'
|> select number, title
|> order by number DESC
|> limit 10
```

```text
number  title
128     Cache invalidation for the search index
127     Bump axum to 0.7
125     Fix flaky auth integration test
… 10 rows
```

That read runs the instant you connect an account. Now the **decisive** part — one statement
squash-merges a PR, and previews the one-way door before it touches anything:

```qfs
/github/acme/web/pulls/42
|> call github.merge(method => 'squash')
```

```text
PREVIEW: 1 effect(s)
  #0 CALL github.merge -> github:/github/acme/web/pulls/42 [affected 1] (!)
  (!) irreversible: 1 node(s) [#0]
  total affected: 1
```

The `(!)` marks the irreversible gate: a merge can't be undone.

::: tip Reads run now; writes preview
Every **read** returns rows immediately. Every **write** (`update`, `insert`, `call`) *previews* by
default and changes nothing — add `--commit` to apply it, `--commit-irreversible` for the ones that
can't be undone (merging, commenting). Paste any recipe below and safely watch what it *would* do
first.
:::

GitHub isn't reachable until you connect an account to it — one command, once, in
**[Setup](#setup)**. After that every recipe on this page works verbatim.

## Setup

qfs pre-mounts nothing for GitHub. A read (and the `CALL` that targets a PR) needs a token:

```sh
qfs connection add github
```

Until connected, a read returns the actionable *connect a GitHub account to read it — run
`qfs connection add github`*. The token is sealed in qfs's encrypted credential store; once it's
there, every `/github/<owner>/<repo>/…` path resolves against the GitHub API.

## The repo as paths

Once connected, a repo is `/github/<owner>/<repo>` and its collections hang off it as directories of
rows:

| GitHub thing | qfs path | it is a… |
| ------------ | -------- | -------- |
| a repo | `/github/acme/web` | directory of collections |
| pull requests | `/github/acme/web/pulls` | directory of PRs |
| one pull request | `/github/acme/web/pulls/42` | file (the `CALL` target) |
| issues | `/github/acme/web/issues` | directory of issues |
| reviews | `/github/acme/web/reviews` | directory of reviews |
| review requests | `/github/acme/web/review_requests` | directory of pending requests |
| releases | `/github/acme/web/releases` | directory of releases |

PR columns include `number`, `title`, `author`, `state`, `created_at`, `merged_at`, `mergeable`,
`review_decision`, `checks_status`, `head_sha`, `additions`, `deletions`, and the nested
`requested_reviewers` (with `.login`, `.team`) and `reviews` arrays. Issue columns include `number`,
`title`, `state`, `author`, `assignee`, `milestone`, `label`, `created_at`, `updated_at`,
`closed_at`. Run `qfs describe /github/acme/web/pulls` for the exact schema and verbs of any node.

## List & filter pull requests

**Open pull requests authored by the platform team**, oldest first — the review queue for a group:

```qfs
/github/acme/web/pulls
|> where state == 'open'
     AND author IN ('rin', 'kenji', 'sora', 'mei')
|> select number, title, author, created_at
|> order by created_at ASC
```

**Expand requested reviewers** to see who is blocking each open PR:

```qfs
/github/acme/web/pulls
|> where state == 'open'
|> expand requested_reviewers
|> select number, requested_reviewers.login, requested_reviewers.team
|> order by number ASC
```

## List & filter issues

**Read the open issues** — number, assignee, and milestone, lowest number first:

```qfs
/github/acme/web/issues
|> where state == 'open'
|> select number, title, assignee, milestone
|> order by number ASC
```

## Report

`GROUP BY` then `AGGREGATE … AS …` rolls raw rows into a report — the same two-stage shape you use on
any other source.

**PR throughput** — count merged pull requests per author over the last 90 days:

```qfs
/github/acme/web/pulls
|> where state == 'merged' AND merged_at >= '2026-03-28'
|> group by author
|> aggregate count(number) as merged_prs
|> order by merged_prs DESC
```

**Issue load per label:**

```qfs
/github/acme/web/issues
|> where state == 'open'
|> group by label
|> aggregate count(number) as open_issues
|> order by open_issues DESC
```

## Update issues — reversible

Field updates on issues preview like any write and only apply on `--commit`; the preview lists the
affected issue numbers first.

**Tag every stale open issue** so a backlog-grooming bot picks it up:

```qfs
/github/acme/web/issues
|> where state == 'open' AND updated_at < '2026-03-26'
|> update set label = 'stale'
```

## Merge & comment — irreversible

**Squash-merge an approved, mergeable PR** — `github.merge` is a one-way door; the preview shows
which PR, `--commit-irreversible` performs the squash:

```qfs
/github/acme/web/pulls
|> where number == 42 AND state == 'open' AND mergeable == true
|> call github.merge(number => number, method => 'squash')
```

**Auto-merge every approved Dependabot PR** with a passing build (an irreversible bulk merge):

```qfs
/github/acme/web/pulls
|> where author == 'dependabot[bot]' AND review_decision == 'APPROVED' AND checks_status == 'success'
|> call github.merge(number => number, method => 'squash')
```

**Comment on a PR whose build is red** (irreversible — posts publicly):

```qfs
/github/acme/web/pulls
|> where number == 87 AND checks_status == 'failure'
|> call github.comment(number => number, body => 'CI is red on this PR - please take a look.')
```

::: warning Irreversible
`CALL github.merge` and `CALL github.comment` can't be undone. In a one-shot each needs
`--commit --commit-irreversible`; the `(!)` in the `PREVIEW` marks the gate. Reads and the preview
of a merge run with no extra flags once the account is connected.
:::
