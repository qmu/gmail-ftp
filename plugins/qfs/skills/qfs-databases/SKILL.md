---
name: qfs-databases
description: Use when a task needs to query or modify a SQL database through qfs — filter, aggregate, join, update, and set operations over /sql/<conn>/<table> relational tables (SQLite, Postgres, MySQL).
---

# Cookbook: Databases

A SQL database table (`/sql/<conn>/<table>`) is a **relational table** — it supports `SELECT`,
`JOIN`, `INSERT`, `UPDATE`, and `UPSERT`. qfs pushes filters, projections, and limits **down** into
the database and does the rest locally.

::: tip Point `<conn>` at a database
A connection is named by an environment variable: `QFS_SQL_<CONN>=<path-or-url>`. The recipes below
use a SQLite file registered as `orders` (`QFS_SQL_ORDERS=/path/to/orders.db`), so its `orders`
table is `/sql/orders/orders`. Postgres/MySQL/D1 URLs work the same way; only the verb support
differs (tables get full CRUD, views are `SELECT`-only).
:::

## Read

**Filter, project, sort, limit** — the `WHERE`, `ORDER BY`, and `LIMIT` push into the database:

```qfs
/sql/orders/orders
|> where total > 100
|> select customer, total
|> order by total DESC
|> limit 5
```

```text
customer | total
-------- | -----
carol    | 220
alice    | 150
(2 row(s))
```

**Ranges and sets read naturally:**

```qfs
/sql/orders/orders
|> where total BETWEEN 50 AND 100
|> select id, total
```

```text
id | total
-- | -----
2  | 80
4  | 55
(2 row(s))
```

```qfs
/sql/orders/orders
|> where customer IN ('alice', 'bob')
|> select id, customer
```

```text
id | customer
-- | --------
1  | alice
2  | bob
(2 row(s))
```

**Pattern match with `LIKE`:**

```qfs
/sql/orders/orders
|> where customer LIKE 'a%'
|> select id, customer
```

```text
id | customer
-- | --------
1  | alice
(1 row(s))
```

## Summarize

**Count rows per group:**

```qfs
/sql/orders/orders
|> group by customer
|> aggregate count(id) as n
|> order by n DESC
```

```text
customer | n
-------- | -
alice    | 1
bob      | 1
carol    | 1
dave     | 1
(4 row(s))
```

**Sum a column:**

```qfs
/sql/orders/orders
|> aggregate SUM(total) as revenue
```

```text
revenue
-------
505
(1 row(s))
```

## Write

Writes **preview** by default — the plan tells you the verb, the target, and how many rows are
affected, and changes nothing until you `--commit`.

**Insert a row, returning its id:**

```qfs
insert into /sql/orders/orders
  values (5, 'eve', 10)
  returning id
```

```text
PREVIEW: 1 effect(s)
  #0 INSERT -> sql:/sql/orders/orders [affected 1]
  total affected: 1
```

**Update matching rows** (the count is `?` until committed, since it's resolved inside the database):

```qfs
update /sql/orders/orders
  set total = 0
  where id == 1
```

```text
PREVIEW: 1 effect(s)
  #0 UPDATE -> sql:/sql/orders/orders [affected ?]
  total affected: ?
```

**Upsert — the retry-safe write** (create-or-replace; running it twice converges):

```qfs
upsert into /sql/orders/orders
  values (1, 'alice', 999)
```

```text
PREVIEW: 1 effect(s)
  #0 UPSERT -> sql:/sql/orders/orders [affected 1]
  total affected: 1
```

## Combine two tables

Set operations stitch two reads together. `UNION` de-duplicates; `EXCEPT` subtracts the second
read from the first.

```qfs
/sql/orders/orders
|> select customer
|> union /sql/orders/orders
|> select customer
```

```text
customer
--------
alice
bob
carol
dave
(4 row(s))
```

```qfs
/sql/orders/orders
|> select customer
|> except /sql/orders/orders
|> where total > 100
|> select customer
```

```text
customer
--------
bob
dave
(2 row(s))
```

::: tip Want to join a database to another *service*?
That's the fun part — see [Cross-service](/cookbook/cross-service).
:::
