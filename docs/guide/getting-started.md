# Your first queries

This page walks you from zero to running real queries. Everything here except the final `--commit`
works **offline with no credentials**, so you can follow along immediately after
[installing](/guide/installation).

## The loop

Every task in qfs follows the same four steps:

1. **Describe** a path to learn what it is and what you can do with it.
2. **Write** a query against it.
3. **Preview** — qfs shows you the exact plan, but changes nothing.
4. **Commit** — you add `--commit` to actually do it.

Let's do it.

## 1. Describe a path

`describe` tells you everything about a node — with no credentials and no network:

```sh
qfs describe /mail/drafts
```

```text
path:      /mail/drafts
archetype: append (SELECT(tail) INSERT(append))
columns:
  name        | type      | null
  ----------- | --------- | ----
  id          | Text      | no
  date        | Timestamp | no
  from        | Text      | no
  subject     | Text      | no
  ...
verbs:     SELECT INSERT UPSERT REMOVE
procedures:
  CALL send(to:Text, subject:Text, body:Text)  [irreversible]
aliases:   SEND -> mail.send
pushdown:  where limit
```

That single report tells you: the columns you can use, the **verbs** this path supports (here you
can `SELECT`, `INSERT`, `UPSERT`, `REMOVE` — but not `UPDATE`), the **procedures** you can `CALL`
(and that `send` is irreversible), and which filters get pushed down to the service.

## 2 + 3. Write and preview

`qfs run` **previews by default**. Nothing is applied:

```sh
qfs run "INSERT INTO /mail/drafts VALUES ('alice@example.com', 'Hi', 'Body text')"
```

```text
PREVIEW: 1 effect(s)
  #0 INSERT -> mail:/mail/drafts [affected 1]
  total affected: 1
```

The preview shows what *would* happen: one INSERT, one row affected. No draft was created.

A read query previews as the query itself (reads change nothing). Longer queries read best with
each `|>` pipe on its own line:

```qfs
/mail/inbox
|> where subject LIKE '%invoice%'
|> select date, from, subject
```

```sh
qfs run "/mail/inbox |> WHERE subject LIKE '%invoice%' |> SELECT date, from, subject"
```

## 4. Commit

When the preview looks right, add `--commit`:

```sh
qfs run "INSERT INTO /mail/drafts VALUES ('alice@example.com', 'Hi', 'Body text')" --commit
```

### Irreversible actions need an extra OK

Some actions can't be undone — sending mail, merging a PR, deleting a file. qfs flags these and, in
a one-shot command, requires an explicit extra acknowledgement so you can never do them by accident:

```sh
# Sending is irreversible — --commit alone is refused:
qfs run "/mail/drafts |> CALL mail.send" --commit --commit-irreversible
```

If you forget the extra flag on an irreversible plan, qfs **fails safely** and tells you why.

## Output formats

qfs prints a human table on your terminal and machine JSON when piped — so it composes with other
tools automatically. Force either one:

```sh
qfs run "..." --format table   # always the human table
qfs run "..." --json           # always JSON
qfs describe /mail/drafts --json | jq .verbs
```

## Connecting a real service

`describe` and `preview` need nothing. To **commit** against a live service, store a credential
once. First export `QFS_PASSPHRASE` — the master passphrase that unlocks your local credential
vault (it derives the argon2id key for the encrypted store; it is **not** a service credential).
It must stay set for the shell that runs `connection add/list/remove`:

```sh
read -rs QFS_PASSPHRASE; export QFS_PASSPHRASE   # unlock the local vault, no shell-history leak
printf %s "$TOKEN" | qfs connection add mail work   # credential VALUE via stdin, never argv
qfs connection list                                 # shows connection names only, never secrets
```

The credential value is read from **stdin** (not prompted, and never passed on argv where it would
leak into the process table + shell history); qfs never prints it back.

See [Connections & credentials](/guide/connections) for details, including the passphrase.

## Where to go next

- **[The Cookbook](/cookbook/)** — dozens of real recipes: cross-service joins, format conversion,
  automation, and more.
- **[How qfs works](/guide/concepts)** — paths, archetypes, previews, and federation explained.
- **[CLI reference](/guide/cli)** — every command and flag.
- **[Interactive shell](/guide/shell)** — explore your services like a filesystem.
