# Cookbook: Databases

A SQL database table (`/sql/pg/...`, `/sql/mysql/...`, Cloudflare D1) is a **relational table** —
it supports `SELECT`, `JOIN`, `INSERT`, `UPDATE`, and `UPSERT`. qfs pushes filters, projections,
and limits **down** into the database and does the rest locally.

## Read

**Filter, project, sort, limit** — the `WHERE` and `LIMIT` run inside the database:

```qfs
/sql/pg/orders
|> where total > 100
|> select id, total, status
|> order by total DESC
|> limit 5
```

**Ranges and sets read naturally:**

```qfs
/sql/pg/orders
|> where total BETWEEN 50 AND 100
|> select id, total
```

```qfs
/sql/pg/orders
|> where status IN ('open', 'pending')
|> select id, status
```

**Multiple conditions:**

```qfs
/sql/pg/users
|> where age >= 18 AND country == 'JP'
|> select id, name, age
```

## Summarize

**Count rows per group:**

```qfs
/sql/pg/orders
|> group by status
|> aggregate count(id) as n
|> order by n DESC
```

**Sum a column:**

```qfs
/sql/pg/orders
|> aggregate SUM(total) as revenue
```

## Add a computed column

`EXTEND` adds a derived column without dropping the rest:

```qfs
/sql/pg/orders
|> extend high_value = total
|> select id, total, high_value
```

## Write

**Insert a row, returning its id:**

```qfs
insert into /sql/pg/audit
  values ('login', 'alice')
  returning id
```

**Update matching rows** (preview shows exactly which are affected):

```qfs
update /sql/pg/orders
  set status = 'shipped'
  where id == 7
```

**Upsert — the retry-safe write** (create-or-replace; running it twice converges):

```qfs
upsert into /sql/pg/settings
  values ('theme', 'dark')
```

## Combine two tables

Set operations stitch two sources together:

```qfs
/sql/pg/users
|> union /sql/mysql/users
```

```qfs
/sql/pg/active_users
|> except /sql/pg/banned_users
```

::: tip Want to join a database to another *service*?
That's the fun part — see [Cross-service](/cookbook/cross-service).
:::
