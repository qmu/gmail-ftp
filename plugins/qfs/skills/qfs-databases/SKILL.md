---
name: qfs-databases
description: Use when a task needs to query or modify a SQL database through qfs — filter, aggregate, join, update, and set operations over /sql/<conn>/<table> relational tables (SQLite, Postgres, MySQL).
---

# Databases

Every table in a connected SQL database becomes a queryable path. A table is a directory of rows,
each row a record, and one pipe-SQL language filters, aggregates, joins, and writes them — the same
verbs you already use on a mailbox, a git repo, or a folder of files.

## See it work first

**Show me my biggest orders** — every order over $100, richest first:

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

That read runs against the live database the instant a connection is configured — qfs pushes the
`WHERE`, `ORDER BY`, and `LIMIT` down into the engine and does the rest locally. Now the **smart**
part — one statement creates-or-replaces a row and previews before it touches anything:

```qfs
upsert into /sql/orders/orders
  values (1, 'alice', 999)
```

```text
PREVIEW: 1 effect(s)
  #0 UPSERT -> sql:/sql/orders/orders [affected 1]
  total affected: 1
```

::: tip Reads run now; writes preview
Every **read** returns rows immediately. Every **write** (`insert`, `update`, `upsert`) *previews*
by default and changes nothing — add `--commit` to apply it. The plan tells you the verb, the
target, and how many rows are affected, so you can safely watch what a recipe *would* do first.
:::

A database isn't reachable until you point a connection at it — one environment variable, in
**[Setup](#setup)**. Local `/sql` connections read the instant they're configured; after that every
recipe on this page works verbatim.

## Setup

::: tip Prerequisite for a connected source
A local file / repo needs no passphrase. A **remote / connected** source stores a login behind your
`QFS_PASSPHRASE` — set it up once in **[The QFS passphrase](/guide/passphrase)**.
:::

You register a database once, by name. The happy path is two lines:

```sh
export QFS_SQL_ORDERS=/path/to/orders.db                                   # 1. name a connection `orders`
qfs run "/sql/orders/orders |> select id, customer, total |> limit 5"      # 2. read a table
```

The rest of this section explains the naming rule and the alternatives.

### 1. Name a connection

A connection is named by an environment variable: `QFS_SQL_<CONN>=<path-or-url>`. The recipes below
use a SQLite file registered as `orders` (`QFS_SQL_ORDERS=/path/to/orders.db`), so its `orders`
table is reachable at `/sql/orders/orders` — the shape is always `/sql/<conn>/<table>`.

### 2. Point it anywhere

Postgres, MySQL, and D1 URLs work exactly the same way — swap the SQLite path for a connection URL
under the same `QFS_SQL_<CONN>` variable. Only the verb support differs: tables get full CRUD, while
views are `SELECT`-only.

### 3. Read a real table

```sh
qfs run "/sql/orders/orders |> select id, customer, total |> limit 5"
```

Real rows come back. `qfs describe /sql/orders/orders` shows the exact columns and the verbs the
node supports.

## The database as paths

Once a connection is configured, a SQL database is a set of **relational tables** mapped onto a
filesystem shape:

| SQL thing | qfs path | it is a… |
| --------- | -------- | -------- |
| a database connection | `/sql/orders` | directory of tables |
| a table | `/sql/orders/orders` | relational table (rows) |
| a view | `/sql/orders/<view>` | read-only table |

A table (`/sql/<conn>/<table>`) supports `SELECT`, `JOIN`, `INSERT`, `UPDATE`, and `UPSERT`; a view
supports `SELECT` only. The `orders` table below has columns `id`, `customer`, and `total`. Run
`qfs describe /sql/orders/orders` for the exact schema and verbs of any node.

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

**Ranges read naturally:**

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

**Sets read naturally too:**

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

**Sum a column** — total revenue in one line:

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

**Union — every distinct customer across two reads:**

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

**Except — customers without a big order:**

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
That's the fun part — join a table to GitHub, a mailbox, or a file in one query. See
[Cross-service](/cookbook/cross-service).
:::
