# Cookbook: Cross-service

This is what qfs is *for*. Because every service is the same kind of path, you can `JOIN` them in a
single statement. qfs pushes each side's filters down to its own service, then joins the results
locally — so a Postgres table and a GitHub repo combine as easily as two database tables.

## Join a database to GitHub

**Match orders to the GitHub issues that track them:**

```qfs
FROM /sql/pg/orders
|> JOIN /github/acme/web/issues ON id == issue_id
|> SELECT id, title, status
```

## Join a database to git history

**Match user accounts to the commits they authored:**

```qfs
FROM /sql/pg/users
|> JOIN /git/myrepo/commits ON id == author_id
|> SELECT name, message
```

## Enrich a service with a local file

**Join live orders to a regions lookup CSV on your laptop:**

```qfs
FROM /sql/pg/orders
|> JOIN /local/regions.csv ON region == code
|> SELECT id, region, total
```

## Combine the same shape from two services

**Everyone, across two databases, de-duplicated:**

```qfs
FROM /sql/pg/users
|> UNION FROM /sql/mysql/users
```

## Move data between services

Because reads and writes share one language, "copy from here to there" spans services too.

**Snapshot a database table into object storage as JSONL:**

```qfs
FROM /sql/pg/orders
|> SELECT id, total, status
|> ENCODE jsonl
```

…then write those bytes to a bucket with an `UPSERT INTO /s3/...`. (Today these are two steps; the
point is they speak the same language end to end.)

::: tip How to know what joins
Run `qfs describe <path>` on each side. The **pushdown** line tells you which filters run inside the
service vs. locally — qfs always over-fetches safely and re-checks locally, so you never get wrong
rows, only a bigger or smaller share of the work pushed down.
:::
