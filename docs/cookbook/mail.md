# Cookbook: Mail

`/mail` is an **append log** — you read the tail and append to it. Columns include `date`, `from`,
`subject`, `snippet`, `label_ids`, and `attachments`. Sending is an irreversible `CALL`.

Run `qfs describe /mail/inbox` to see the full schema for your mailbox.

## Search & read

**Find invoices, newest first:**

```qfs
/mail/inbox
|> where subject LIKE '%invoice%'
|> select date, from, subject
|> order by date DESC
|> limit 20
```

**Everything from one sender:**

```qfs
/mail/inbox
|> where from == 'billing@stripe.com'
|> select date, subject
```

**Mail with attachments in a date range:**

```qfs
/mail/inbox
|> where date BETWEEN '2026-01-01' AND '2026-03-31'
|> select date, from, subject, attachments
```

**Read a single message by id:**

```qfs
id:18f1a2b3c4
```

## Summarize

**Who emails you most?**

```qfs
/mail/inbox
|> group by from
|> aggregate count(id) as messages
|> order by messages DESC
|> limit 10
```

## Write

**Draft an email** (reversible — creating a draft sends nothing):

```qfs
insert into /mail/drafts
  values ('alice@example.com', 'Q3 report', 'See attached.')
```

**Draft, then send it.** The draft is reversible; the send is the irreversible step:

```qfs
insert into /mail/drafts
  values ('alice@example.com', 'Q3 report', 'See attached.')

/mail/drafts
|> call mail.send
```

::: warning Irreversible
`CALL mail.send` can't be undone. In a one-shot it needs `--commit --commit-irreversible`. A retry
re-sends the **same** draft (de-duplicated), never a fresh message.
:::

## Clean up

**Trash newsletters** (also irreversible — preview shows it as a gate):

```qfs
remove /mail/inbox
  where subject LIKE '%unsubscribe%'
```

**Trash by sender:**

```qfs
remove /mail/inbox
  where from == 'noreply@spammy.example'
```

::: tip
An append log only supports `SELECT` (read the tail) and `INSERT` (append) — plus `REMOVE` to
trash. There's no `UPDATE`; `qfs describe /mail/inbox` always shows the exact supported set.
:::
