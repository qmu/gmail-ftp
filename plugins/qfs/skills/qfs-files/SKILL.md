---
name: qfs-files
description: Use when a task needs to read, write, or convert local files and S3/R2 object storage through qfs — list/inspect, read bytes, UPSERT/REMOVE blobs under /local, /s3, /r2, plus codec format conversion (CSV, JSON, YAML, TOML). For Google Drive use the Google Drive cookbook.
---

# Cookbook: Files & object storage

Local files (`/local/...`) and object storage (`/s3/...`, `/r2/...`) are **blob namespaces** —
folders of files. They share one set of verbs: `SELECT` to list/read, `UPSERT` to write, `REMOVE` to
delete. (In the [interactive shell](/guide/shell) you can also use the familiar `ls`/`cp`/`mv`/`rm` —
those are just shorthand for these same verbs.)

`/local` reads and writes run today against your own disk; use an **absolute host path** after
`/local`. The object stores need a connected account (see the notes below). Google Drive is its own
service — see the [Google Drive cookbook](/cookbook/gdrive).

## List & inspect

**List a local directory with details:**

```qfs
/local/home/you/docs
|> select name, size, is_dir, modified
```

```text
name        | size | is_dir | modified
----------- | ---- | ------ | -------------
config.json | 22   | false  | 1782734201012
regions.csv | 38   | false  | 1782734201012
(2 row(s))
```

**Read one file** — a single-file `/local` read carries the bytes in a `content` column alongside
its stat row:

```qfs
/local/home/you/docs/config.json
```

```text
name        | path                             | size | modified      | is_dir | mode  | content
----------- | -------------------------------- | ---- | ------------- | ------ | ----- | ----------
config.json | /local/home/you/docs/config.json | 22   | 1782734201012 | false  | 33188 | <22 bytes>
(1 row(s))
```

## Convert between formats (codecs) 🚧

`DECODE` turns bytes into rows; `ENCODE` turns rows into bytes. Supported formats: `json`, `jsonl`,
`yaml`, `toml`, `csv`. Point a `/local` file read at `decode`, then `encode` the other way, and the
whole conversion is one pipeline.

**JSON → YAML:**

```qfs
/local/home/you/docs/config.json
|> decode json
|> encode yaml
```

```text
content
----------------
- k: 1
  name: alpha
```

**JSON → TOML** (same file, different target):

```qfs
/local/home/you/docs/config.json
|> decode json
|> encode toml
```

```text
content
----------------
k = 1
name = "alpha"
```

**CSV → JSON:**

```qfs
/local/home/you/docs/regions.csv
|> decode csv
|> encode json
```

```text
content
-------------------------------------------------
[
  {
    "code": "JP",
    "region": "Japan"
  },
  {
    "code": "US",
    "region": "United States"
  }
]
```

**Just unpack a file into rows** — `decode` on its own gives you the parsed table:

```qfs
/local/home/you/docs/config.json
|> decode json
```

```text
k | name
- | -----
1 | alpha
(1 row(s))
```

::: warning Codecs are terminal stages
`DECODE` and `ENCODE` must be the **last** stages of a pipeline — you can't `where`/`select`/`join`
*after* a `decode` yet (that returns `codec_then_query`). So `decode json |> encode yaml` is fine,
but `decode json |> where level == 'error' |> encode csv` is not. Reshaping decoded rows mid-pipeline
is coming soon.
:::

## Write & copy 🚧

Writing a blob is an `UPSERT` (retry-safe — re-running converges instead of duplicating). Because
every store is the same kind of path, the same statement shape works across them. Writes **preview**
by default; they change nothing until you `--commit`.

**Write a local file** (previews the plan):

```qfs
upsert into /local/home/you/docs/out.json
  values ('{"ok":true}')
```

```text
PREVIEW: 1 effect(s)
  #0 UPSERT -> local:/local/home/you/docs/out.json [affected 1]
  total affected: 1
```

::: warning Object stores need a connection, and their writes are not wired yet
An `/s3` or `/r2` **read** needs credentials — an `/s3` list returns *connect AWS credentials to read
S3 — run `qfs connection add s3`*. `/s3` and `/r2` **writes** are not implemented yet: `upsert into
/s3/…` / `remove /s3/…` return `unsupported_verb` (`supported: []`). `/local` reads **and** writes are
wired and run against your disk with no connection — use it for a runnable end-to-end blob recipe.
:::
