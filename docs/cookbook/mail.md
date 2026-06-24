# Cookbook: Mail

`/mail` is an **append log** — you read the tail and append to it. Columns include `date`, `from`,
`subject`, `snippet`, `label_ids`, and `attachments`. Sending is an irreversible `CALL`.

Run `qfs describe /mail/inbox` to see the full schema for your mailbox.

## Search & read

**Find invoices, newest first:**

```qfs
FROM /mail/inbox
|> WHERE subject LIKE '%invoice%'
|> SELECT date, from, subject
|> ORDER BY date DESC
|> LIMIT 20
```

**Everything from one sender:**

```qfs
FROM /mail/inbox
|> WHERE from = 'billing@stripe.com'
|> SELECT date, subject
```

**Mail with attachments in a date range:**

```qfs
FROM /mail/inbox
|> WHERE date BETWEEN '2026-01-01' AND '2026-03-31'
|> SELECT date, from, subject, attachments
```

**Read a single message by id:**

```qfs
FROM id:18f1a2b3c4
```

## Summarize

**Who emails you most?**

```qfs
FROM /mail/inbox
|> GROUP BY from
|> AGGREGATE count(id) AS messages
|> ORDER BY messages DESC
|> LIMIT 10
```

## Write

**Draft an email** (reversible — creating a draft sends nothing):

```qfs
INSERT INTO /mail/drafts
  VALUES ('alice@example.com', 'Q3 report', 'See attached.')
```

**Draft, then send it.** The draft is reversible; the send is the irreversible step:

```qfs
INSERT INTO /mail/drafts
  VALUES ('alice@example.com', 'Q3 report', 'See attached.')

FROM /mail/drafts
|> CALL mail.send
```

::: warning Irreversible
`CALL mail.send` can't be undone. In a one-shot it needs `--commit --commit-irreversible`. A retry
re-sends the **same** draft (de-duplicated), never a fresh message.
:::

## Clean up

**Trash newsletters** (also irreversible — preview shows it as a gate):

```qfs
REMOVE /mail/inbox
  WHERE subject LIKE '%unsubscribe%'
```

**Trash by sender:**

```qfs
REMOVE /mail/inbox
  WHERE from = 'noreply@spammy.example'
```

::: tip
An append log only supports `SELECT` (read the tail) and `INSERT` (append) — plus `REMOVE` to
trash. There's no `UPDATE`; `qfs describe /mail/inbox` always shows the exact supported set.
:::
