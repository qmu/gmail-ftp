# Cookbook: Databases

A SQL database table (`/sql/pg/...`, `/sql/mysql/...`, Cloudflare D1) is a **relational table** —
it supports `SELECT`, `JOIN`, `INSERT`, `UPDATE`, and `UPSERT`. qfs pushes filters, projections,
and limits **down** into the database and does the rest locally.

## Read

**Filter, project, sort, limit** — the `WHERE` and `LIMIT` run inside the database:

```qfs
FROM /sql/pg/orders
|> WHERE total > 100
|> SELECT id, total, status
|> ORDER BY total DESC
|> LIMIT 5
```

**Ranges and sets read naturally:**

```qfs
FROM /sql/pg/orders
|> WHERE total BETWEEN 50 AND 100
|> SELECT id, total
```

```qfs
FROM /sql/pg/orders
|> WHERE status IN ('open', 'pending')
|> SELECT id, status
```

**Multiple conditions:**

```qfs
FROM /sql/pg/users
|> WHERE age >= 18 AND country = 'JP'
|> SELECT id, name, age
```

## Summarize

**Count rows per group:**

```qfs
FROM /sql/pg/orders
|> GROUP BY status
|> AGGREGATE count(id) AS n
|> ORDER BY n DESC
```

**Sum a column:**

```qfs
FROM /sql/pg/orders
|> AGGREGATE SUM(total) AS revenue
```

## Add a computed column

`EXTEND` adds a derived column without dropping the rest:

```qfs
FROM /sql/pg/orders
|> EXTEND high_value = total
|> SELECT id, total, high_value
```

## Write

**Insert a row, returning its id:**

```qfs
INSERT INTO /sql/pg/audit
  VALUES ('login', 'alice')
  RETURNING id
```

**Update matching rows** (preview shows exactly which are affected):

```qfs
UPDATE /sql/pg/orders
  SET status = 'shipped'
  WHERE id = 7
```

**Upsert — the retry-safe write** (create-or-replace; running it twice converges):

```qfs
UPSERT INTO /sql/pg/settings
  VALUES ('theme', 'dark')
```

## Combine two tables

Set operations stitch two sources together:

```qfs
FROM /sql/pg/users
|> UNION FROM /sql/mysql/users
```

```qfs
FROM /sql/pg/active_users
|> EXCEPT FROM /sql/pg/banned_users
```

::: tip Want to join a database to another *service*?
That's the fun part — see [Cross-service](/cookbook/cross-service).
:::
