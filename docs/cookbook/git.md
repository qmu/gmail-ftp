---
skill_name: qfs-git
skill_description: Use when a task needs git through qfs — read a repo's versioned file tree and history over /git, list commits/refs/tags, read a directory as of a tag or branch with the @<ref> coordinate, and record a commit.
---

# Git

A git repo is both a **versioned file tree** and a **history** — qfs makes both queryable through the
same pipe-SQL language you already use on a mailbox, a database, or a folder of files. Commits are
rows, refs are rows, and the tree at any tag or branch is a directory you can list.

## See it work first

**Show me the history** — every commit as a row, newest work at the top:

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

That read runs **today, with no cloud account** — qfs reads your local repo directly, both loose
*and* packed objects. Now the **write** side — one statement records a commit, and previews the exact
object writes before touching anything:

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

::: tip Reads run now; writes preview
Every **read** over a registered repo returns rows immediately — no account, no network. Every
**write** (recording a commit) *previews* by default and changes nothing; add `--commit` to apply it.
Paste any recipe below and safely watch what it *would* do first.
:::

The one coordinate that makes this a *versioned* tree: **`@<ref>`** in the path
(`/git/myrepo@v1/src`) reads the tree as of a tag, branch, or commit — see
[Read the tree at a ref](#read-the-tree-at-a-ref).

## Setup

::: tip Prerequisite for a connected source
A local file / repo needs no passphrase. A **remote / connected** source stores a login behind your
`QFS_PASSPHRASE` — set it up once in **[The QFS passphrase](/guide/passphrase)**.
:::

git needs almost no setup — there's no account to connect. You just tell qfs where the repo lives by
registering it with an environment variable, then read it at `/git/<repo>/…`:

```sh
export QFS_GIT_MYREPO=/path/to/repo        # a working tree, or a bare .git directory
```

The variable name after `QFS_GIT_` (lowercased) becomes the repo segment in the path — here
`QFS_GIT_MYREPO` mounts at `/git/myrepo`. Register as many as you like; each `QFS_GIT_<name>` adds a
`/git/<name>` repo. Both loose and packed objects are read, so a freshly cloned or long-lived repo
both work.

## The repo as paths

Once registered, `/git/<repo>` is your repository mapped onto a filesystem shape:

| git thing | qfs path | it is a… |
| --------- | -------- | -------- |
| the history | `/git/myrepo/commits` | table of commits |
| refs & tags | `/git/myrepo/refs` | table of refs |
| the tree at a ref | `/git/myrepo@<ref>/<dir>` | directory listing as of that point |
| a file at a ref | `/git/myrepo@<ref>/<path>` | file (byte reads coming soon) |

Commit columns: `sha`, `tree`, `parents`, `author`, `committer`, `time`, `message`. Ref columns:
`name`, `oid`. Tree columns: `name`, `mode`, `oid`, `kind`. Run `qfs describe /git/myrepo/commits`
for the exact schema and verbs of any node.

The **`@<ref>`** coordinate travels in the path itself: `/git/myrepo@v1/src` is the `src` directory
*as it stood at tag `v1`*. A branch name or commit sha works just as well as a tag.

## History

**List the commit history** — the `commits` facet is a plain table you shape with `select`:

```qfs
/git/myrepo/commits
|> select sha, message
```

**Filter the history** — every commit column (`sha`, `tree`, `parents`, `author`, `committer`,
`time`, `message`) filters like any table, so a `WHERE` narrows it down:

```qfs
/git/myrepo/commits
|> where message LIKE 'add%'
|> select sha, message
```

## Refs & tags

**List refs and tags** — branches, `HEAD`, and tags, each with the object it points at:

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

## Read the tree at a ref

**List a directory as of a tag** — the `@<ref>` coordinate reads the tree at that point in history,
without checking anything out:

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

## Record a commit

History is append-only. Recording a commit is an `INSERT` into the `commits` facet — it *previews*
the effect and applies nothing until `--commit`:

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
