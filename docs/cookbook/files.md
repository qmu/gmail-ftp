# Cookbook: Files & storage

Local files (`/local/...`), cloud Drive (`/drive/...`), and object storage (`/s3/...`, `/r2/...`)
are all **blob namespaces** — folders of files. They share one set of verbs: `SELECT` to list/read,
`UPSERT` to write, `REMOVE` to delete. (In the [interactive shell](/guide/shell) you can also use
the familiar `ls`/`cp`/`mv`/`rm` — those are just shorthand for these same verbs.)

## List & inspect

**List a folder, biggest files first:**

```qfs
/s3/my-bucket/logs
|> where size > 1000000
|> select name, size
|> order by size DESC
```

**List a local directory with details:**

```qfs
/local/docs
|> select name, size, is_dir, modified
```

## Write & copy

Writing a blob is an `UPSERT` (retry-safe — re-running converges instead of duplicating). Because
every store is the same kind of path, copying *between* clouds is the same statement as copying
within one.

**Upload a report to Drive:**

```qfs
upsert into /drive/my/Reports/q3.pdf
  values ('…bytes…')
```

**Back up a file to S3:**

```qfs
upsert into /s3/backups/2026/db.sql
  values ('…bytes…')
```

## Delete

```qfs
remove /s3/my-bucket/tmp/old.log
```

## Convert between formats (codecs)

`DECODE` turns bytes into rows; `ENCODE` turns rows into bytes. Supported: `json`, `jsonl`, `yaml`,
`toml`, `csv`, `md`. So format conversion is one pipeline.

**JSON → YAML:**

```qfs
/local/config.json
|> decode json
|> encode yaml
```

**Read a JSON file, keep the errors, write a CSV:**

```qfs
/local/events.json
|> decode json
|> where level == 'error'
|> encode csv
```

**Export a database table to JSONL:**

```qfs
/sql/pg/orders
|> select id, total, status
|> encode jsonl
```

::: tip
Codecs are a stage like any other, so you can filter and project *between* decoding and encoding —
that's how you reshape a file in a single statement.
:::
