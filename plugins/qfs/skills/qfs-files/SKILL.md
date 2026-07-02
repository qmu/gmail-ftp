---
name: qfs-files
description: Use when a task needs to read, write, or convert local files and S3/R2 object storage through qfs — list/inspect, read bytes, UPSERT/REMOVE blobs under /local, /s3, /r2, plus codec format conversion (CSV, JSON, YAML, TOML). For Google Drive use the Google Drive cookbook.
---

# Files & object storage

Your disk and your cloud buckets become the same thing: **folders of files at queryable paths**.
`/local` is your own filesystem, `/s3` and `/r2` are object stores, and one pipe-SQL language lists,
reads, converts between formats, and writes across all of them with the same handful of verbs.

## See it work first

**Just list a folder on your own disk** — no connection, no account, nothing to set up:

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

That runs the instant you type it — `/local` reads (and writes) go straight to your disk with no
connection and no operator. Now the **fun** part: qfs speaks formats, so a file in one shape comes
back in another as a single pipeline — here a JSON config turns into YAML:

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

::: tip Reads run now; writes preview
`/local` and `/sys` need **no setup** — reads run immediately and writes go to your disk. The object
stores `/s3` and `/r2` need a connected account first (see **[Setup](#setup)**). Either way, every
**write** (`upsert`, `remove`) *previews* by default and changes nothing — add `--commit` to apply
it. Paste any recipe below and safely watch what it *would* do first.
:::

## Setup

::: tip Prerequisites — an operator, an account, a mount
Reaching a cloud service takes three one-time steps: a signed-in operator (`qfs init` —
**[The operator identity](/guide/operator)**), an authorized account (`qfs account add …`), and a
mount binding that account to a path (`qfs connect …`). The happy path below is exactly those
three.
:::

`/local` and `/sys` work out of the box — skip straight to the recipes. You only need this section to
read **S3 or R2** buckets. The happy path is three commands:

```sh
qfs init you@example.com                                     # 1. the operator + the vault
printf '%s' "$SECRET_ACCESS_KEY" | qfs account add objstore  # 2. your bucket credentials
qfs connect /s3 --driver s3 --account default                # 3. mount the bucket at /s3
```

The rest of this section explains each line.

### 1. Ready the machine

Object stores are cloud drivers, and cloud drivers require a signed-in operator — qfs fails closed
for an anonymous one. `qfs init` creates the encrypted credential store and registers you as this
machine's operator (no password — your OS login is the authentication). Re-running it is safe:

```sh
qfs init you@example.com
```

### 2. Authorize the credentials

Seal the **secret access key** in qfs's encrypted store under the `objstore` provider — it comes in
on **stdin**, never argv, and the label defaults to `default`:

```sh
printf '%s' "$SECRET_ACCESS_KEY" | qfs account add objstore
```

The **non-secret** routing config comes from the environment: `QFS_S3_REGION`,
`QFS_S3_ACCESS_KEY_ID`, and `QFS_S3_BUCKET` for S3; `QFS_R2_ACCOUNT_ID`, `QFS_R2_ACCESS_KEY_ID`, and
`QFS_R2_BUCKET` for R2.

### 3. Connect the paths

The mount carries the account. `/s3` uses the `s3` driver; `/r2` uses `r2`:

```sh
qfs connect /s3 --driver s3 --account default
qfs connect /r2 --driver r2 --account default
```

`qfs connect --list` now lists them, and `qfs describe /s3` shows the schema and verbs. If an `/s3`
list still fails with an actionable *no usable credentials* hint, a step above hasn't run yet: the
path resolved (you're past addressing), but the mount has no bound account.

## Files & object storage as paths

Every store is the same kind of node — a **blob namespace**, a folder of files — so one set of verbs
covers all three:

| store | qfs path | it is a… | needs |
| ----- | -------- | -------- | ----- |
| your disk | `/local/<absolute-host-path>` | folder of files, read **and** written today | nothing |
| S3 | `/s3/<bucket>/<key>` | folder of objects | an account-bound mount |
| R2 | `/r2/<bucket>/<key>` | folder of objects | an account-bound mount |

They share three verbs: `SELECT` to list/read, `UPSERT` to write, `REMOVE` to delete. (In the
[interactive shell](/guide/shell) the familiar `ls`/`cp`/`mv`/`rm` are just shorthand for these same
verbs.) A directory listing carries `name`, `size`, `is_dir`, `modified`; a single-file read adds
`path`, `mode`, and the decoded `content` bytes. Run `qfs describe /local/...` for the exact schema
and verbs of any node.

Use an **absolute host path** after `/local` (e.g. `/local/home/you/docs`). Google Drive is its own
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

::: warning Object stores need an account-bound mount, and their writes are not wired yet
An `/s3` or `/r2` **read** needs credentials — without them an `/s3` list fails with an actionable
*no usable credentials* hint naming the `qfs account add …` / `qfs connect …` to run (see
**[Setup](#setup)**). `/s3` and `/r2` **writes** are not implemented yet: `upsert into
/s3/…` / `remove /s3/…` return `unsupported_verb` (`supported: []`). `/local` reads **and** writes are
wired and run against your disk with no mount — use it for a runnable end-to-end blob recipe.
:::
