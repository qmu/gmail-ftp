---
skill_name: qfs-gdrive
skill_description: Use when a task needs to read, write, or organize Google Drive through qfs — list and navigate My Drive and Shared Drives, download a file's bytes, upload, create folders, read Google-native docs, copy, and trash items via the /drive path and its pipe-SQL queries. Covers connecting a Google account (shared with Gmail) and the folder/file/blob surface.
---

# Google Drive

Your whole Drive becomes a set of queryable paths. Folders are directories, files are blobs, and one
pipe-SQL language lists, searches, downloads, uploads, organizes, and trashes — the same verbs you
already use on a mailbox, a database, or a folder of files.

## See it work first

**Show me what's in My Drive** — every file with its type, size, and last-modified time:

```qfs
/drive/my
|> select name, mime_type, size, modified_time
```

```text
name              mime_type                        size     modified_time
Reports           application/vnd.google-apps.folder    —    2026-06-30
q3-plan.md        text/markdown                    4.2 KB   2026-06-28
budget.xlsx       application/vnd.openxmlformats-…  18 KB    2026-06-24
… 20 rows
```

That read runs the instant you connect an account. Uploading is just as direct — one statement writes
a file, and previews before it touches anything:

```qfs
upsert into /drive/my/Reports/q3.pdf
  values ('…bytes…')
```

```text
PREVIEW: 1 effect(s)
  #0 UPSERT -> drive:/drive/my/Reports/q3.pdf [affected 1]
  total affected: 1
```

::: tip Reads run now; writes preview
Every **read** returns rows immediately. Every **write** (`upsert`, `insert`, `remove`, `call`)
*previews* by default and changes nothing — add `--commit` to apply it, `--commit-irreversible` for
the ones that can't be undone (trashing). Paste any recipe below and safely watch what it *would* do
first.
:::

Drive isn't reachable until you connect a Google account to a path — and it shares Gmail's account,
so it's often already done. See **[Setup](#setup)**. After that every recipe on this page works
verbatim.

## Setup

Drive uses the **same Google account and OAuth app as Gmail** — a single consent covers both. If you
already followed the [Gmail cookbook Setup](/cookbook/gmail#setup), the happy path is one command:

```sh
qfs connect /drive --driver gdrive                    # mount Drive at /drive
```

The rest of this section explains the details.

If you have **not** connected a Google account yet, do the Google-account steps in the
[Gmail cookbook Setup](/cookbook/gmail#setup) first (sign in, hand qfs your `credentials.json`, get a
refresh token), but enable the **Drive API** for your Google Cloud project. Then mount the path:

```sh
qfs connect /drive --driver gdrive
```

`QFS_PASSPHRASE` must be exported (it unlocks qfs's encrypted credential store). `qfs connection
paths` now lists the mount, and `qfs describe /drive` shows the schema and verbs.

::: info The mount path is yours
`/work/drive` works just as well as `/drive` — mount with `qfs connect /work/drive --driver gdrive`
and every `/drive/…` recipe below simply becomes `/work/drive/…`.
:::

If a read reports *connect a Google account to read Drive*, you are past addressing (the path
resolved) but the cloud bind gate has no signed-in operator or recorded consent yet — revisit the
Gmail Setup steps 1–3.

## Drive as paths

Once connected, `/drive` is your Google Drive as a **blob namespace** mapped onto a filesystem shape:

| Drive thing | qfs path | it is a… |
| ----------- | -------- | -------- |
| the root | `/drive` | lists the two corpora, `my` and `shared` |
| My Drive | `/drive/my`, `/drive/my/<folder>` | directory of files |
| a Shared Drive | `/drive/shared/<DriveName>` | directory of files |
| a file | `/drive/my/<path>` | a blob (its bytes are the `content` column) |

File columns: `name`, `mime_type`, `size`, `modified_time`, `md5`, `is_google_doc`, and — on a
single-file read — `content` (the bytes). Run `qfs describe /drive/my` for the exact schema and verbs
of any node. Blob verbs are the same everywhere: `SELECT` to list/read, `UPSERT` to write, `REMOVE`
to trash. (In the [interactive shell](/guide/shell) the familiar `ls`/`cp`/`mv`/`rm` are shorthand
for these same verbs.)

## Browse

**List the two corpora** at the root:

```qfs
/drive
|> select name
```

**List My Drive** (or any folder) with details:

```qfs
/drive/my
|> select name, mime_type, size, modified_time
```

**List a Shared Drive** by name:

```qfs
/drive/shared/Engineering
|> select name, mime_type, size
```

## Find & read

**Find files by name:**

```qfs
/drive/my
|> where name LIKE '%q3%'
|> select name, mime_type, size
```

**Download a file** — a single-file read resolves the path to its id and carries the bytes in a
`content` column alongside the metadata:

```qfs
/drive/my/Reports/q3.pdf
|> select name, mime_type, size, md5, content
```

**A Google-native doc** (Docs/Sheets/Slides) reports `is_google_doc` and exports to a concrete
office/text format on read; its metadata lists what it is:

```qfs
/drive/my/Notes
|> select name, mime_type, is_google_doc
```

## Write, organize, trash

Writes **preview by default** — they change nothing until you `--commit`.

**Upload a file** (an `UPSERT` — retry-safe, re-running converges instead of duplicating):

```qfs
upsert into /drive/my/Reports/q3.pdf
  values ('…bytes…')
```

```text
PREVIEW: 1 effect(s)
  #0 UPSERT -> drive:/drive/my/Reports/q3.pdf [affected 1]
  total affected: 1
```

**Create a folder** (gdrive-ftp `mkdir`) — a folder is an `INSERT` with the folder MIME type and no
bytes:

```qfs
insert into /drive/my/Reports
  values (name, mime_type) ('Q3', 'application/vnd.google-apps.folder')
```

**Trash a file** (irreversible — a gate):

```qfs
remove /drive/my
  where name == 'old-draft.pdf'
```

```text
PREVIEW: 1 effect(s)
  #0 REMOVE -> drive:/drive/my [affected ?] (!)
  (!) irreversible: 1 node(s) [#0]
  total affected: ?
```

The `(!)` marks the irreversible gate: a one-shot needs `--commit --commit-irreversible` to apply it.
A same-drive server-side copy is available as the `drive.copy` `CALL` procedure.

::: tip Attach a Drive file to an email
Downloading a Drive file and attaching it to a Gmail draft in one statement is the
[cross-service attach-and-send recipe](/cookbook/cross-service).
:::
