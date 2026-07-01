---
skill_name: qfs-git
skill_description: Use when a task needs git through qfs — read a repo's versioned file tree and history over /git, list commits/refs/tags, read a directory as of a tag or branch with the @<ref> coordinate, and record a commit.
---

# Cookbook: git

A git repo is both a **versioned file tree** and a **history**. qfs pre-mounts nothing — register a
repo with `QFS_GIT_<REPO>=<path-to-repo-or-.git>`, then read its facets at `/git/<repo>/…`. The
`@<ref>` coordinate (in the path) reads the tree as of a tag, branch, or commit. git reads run today
against your local repo — loose *and* packed objects.

## History

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

## Read the tree at a ref 🚧

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

## Record a commit

History is append-only; this previews the effect, applying nothing until `--commit`:

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
