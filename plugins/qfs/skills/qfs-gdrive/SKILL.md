---
name: qfs-gdrive
description: Use when a task needs to read, write, or organize Google Drive through qfs — list and navigate My Drive and Shared Drives, download a file's bytes, upload, create folders, read Google-native docs, copy, and trash items via the /drive path and its pipe-SQL queries. Covers connecting a Google account (shared with Gmail) and the folder/file/blob surface.
---

# Cookbook: Google Drive

qfs pre-mounts **nothing** for third-party services — Google Drive is unreachable until you `CONNECT`
it to a path of your choosing (see [Setup](#setup)). This cookbook mounts it at `/drive`
(`qfs connect /drive --driver gdrive`), but the path is yours — `/work/drive` works just as well, and
every `/drive/…` recipe below simply becomes `/work/drive/…`.

Once connected, `/drive` is your Google Drive as a **blob namespace** mapped onto a filesystem shape:

| Drive thing | qfs path | it is a… |
| ----------- | -------- | -------- |
| the root | `/drive` | lists the two corpora, `my` and `shared` |
| My Drive | `/drive/my`, `/drive/my/<folder>` | directory of files |
| a Shared Drive | `/drive/shared/<DriveName>` | directory of files |
| a file | `/drive/my/<path>` | a blob (its bytes are the `content` column) |

File columns: `name`, `mime_type`, `size`, `modified_time`, `md5`, `is_google_doc`, and — on a
single-file read — `content` (the bytes). Run `qfs describe /drive/my` (after connecting) to see the
exact schema and verbs for any node. Blob verbs are the same everywhere: `SELECT` to list/read,
`UPSERT` to write, `REMOVE` to trash. (In the [interactive shell](/guide/shell) the familiar
`ls`/`cp`/`mv`/`rm` are shorthand for these same verbs.)

## Setup

Drive uses the **same Google account and OAuth app as Gmail** — a single consent covers both. If you
already followed the [Gmail cookbook Setup](/cookbook/gmail#setup), you only need the **mount** at
the end. Otherwise, do the Google-account steps there (sign in, `credentials.json`, refresh token),
enabling the **Drive API** for your Google Cloud project, then connect the path:

```sh
qfs connect /drive --driver gdrive
```

`QFS_PASSPHRASE` must be exported (it unlocks qfs's encrypted credential store). `qfs connection
paths` now lists the mount, and `qfs describe /drive` shows the schema and verbs. If a read reports
*connect a Google account to read Drive*, you are past addressing (the path resolved) but the cloud
bind gate has no signed-in operator or recorded consent yet — revisit the Gmail Setup steps 1–3.

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
