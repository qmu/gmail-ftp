---
name: qfs-gmail
description: Use when a task needs to read or triage Gmail through qfs — search, read, summarize, draft, send, relabel, or trash mail via the /mail path and its pipe-SQL queries. Covers connecting a Google account and the label, message, thread, attachment, and draft surface.
---

# Cookbook: Gmail

qfs pre-mounts **nothing** for third-party services — Gmail is unreachable until you `CONNECT` it to
a path of your choosing (see [Setup](#setup)).

::: info The mount path is yours
This cookbook mounts Gmail at `/mail` (`qfs connect /mail --driver gmail`), but the path is yours —
`/work/gmail` works just as well, and every `/mail/…` recipe below simply becomes `/work/gmail/…`.
:::

Once connected, `/mail` is your Gmail account as an **append log**, mapped onto a filesystem shape:

| Gmail thing | qfs path | it is a… |
| ----------- | -------- | -------- |
| a label | `/mail/inbox`, `/mail/sent`, `/mail/<UserLabel>` | directory of messages |
| a message | `/mail/inbox/<msg-id>` or `id:<msg-id>` | file |
| a thread | `id:thread:<thread-id>` | file (the whole conversation) |
| an attachment | `/mail/inbox/<msg-id>/<att-id>` | nested entry |
| your drafts | `/mail/drafts` | the append target you write new mail to |

Message columns: `id`, `thread_id`, `date`, `from`, `subject`, `snippet`, `label_ids`,
`attachments`. Attachment columns: `filename`, `mime`, `size`. Run `qfs describe /mail/inbox`
(after connecting) to see the exact schema and verbs for any node.

::: tip Labels are written verbatim
A label segment is passed to Gmail **exactly as you write it** — qfs never rewrites the case. It
reaches Gmail as a `label:` search term, which Gmail itself matches case-insensitively, so the
ergonomic lowercase just works: `/mail/inbox` reads your inbox, and `sent`, `spam`, `trash`,
`starred`, `important`, `unread`, the `category_*` tabs, and your own user labels all work the same
way. `drafts` is qfs's reserved write collection (the INSERT target), not a label.
:::

## Setup

A complete Gmail setup has four parts: an authenticated qfs operator, your Google OAuth **app**
credentials, your account's **refresh token**, and the **mount** (the `CONNECT` that makes the path
exist).

### 0. Prerequisites

- A Google account.
- A Google **Desktop-app** OAuth client (`client_id` + `client_secret`), downloaded from the
  [Google Cloud console](https://console.cloud.google.com/apis/credentials) as `credentials.json`.
  Enable the **Gmail API** for the project.
- `QFS_PASSPHRASE` exported in your shell — it unlocks qfs's encrypted credential store, where the
  refresh token is sealed at rest.

### 1. Sign in

Cloud drivers require an authenticated operator — qfs fails closed for an anonymous one. The
password is read from **stdin**, never argv:

```sh
printf '%s' "$YOUR_PASSWORD" | qfs identity signup you@example.com
```

### 2. Hand qfs your OAuth app credentials

Store the downloaded `credentials.json` in qfs's own encrypted store (so it no longer depends on a
file on disk):

```sh
cat credentials.json | qfs connection add google-app default
```

CI/agents can instead export `QFS_GOOGLE_CLIENT_ID` and `QFS_GOOGLE_CLIENT_SECRET`.

### 3. Authorize your account (get a refresh token)

Pick **one** of the two paths.

**A — Fresh browser consent (recommended).** One command opens a Google consent screen; approve it
and qfs stores the refresh token under `google:<email>:refresh_token`, records your consent, and
selects the account:

```sh
QFS_GOOGLE_CONSENT=1 qfs connection add gmail default
```

On a **headless server**, forward the loopback port over SSH first, then open the printed URL in
your laptop browser:

```sh
ssh -L 8080:localhost:8080 you@your-server      # in a second terminal
```

**B — Import an existing refresh token** (reuse one a prior tool already obtained). Store it under
your url-encoded email, record consent, and select the account:

```sh
printf '%s' "$REFRESH_TOKEN" | qfs connection add google 'you%40example.com'  # %40 = @
printf 'x'                   | qfs connection add gmail default                # records consent
export QFS_GOOGLE_ACCOUNT=you@example.com                                      # selects the account
```

### 4. Connect the path

Mount Gmail wherever you like — the rest of this cookbook assumes `/mail`:

```sh
qfs connect /mail --driver gmail
```

`qfs connection paths` now lists it, and `qfs describe /mail` shows the schema and verbs.

### 5. Read real mail

```sh
qfs run "/mail/inbox |> select date, from, subject |> limit 5"
```

Real messages come back. If a read reports *connect a Google account to read mail*, revisit steps
1–3 — you are past addressing (the path resolved) but the cloud bind gate has no signed-in operator
or recorded consent yet.

## Browse the mailbox

**List your labels** (the directories under `/mail`):

```qfs
/mail |> select name
```

**Read any label** — the system labels (`inbox`, `sent`, `starred`, `important`, `spam`, `unread`)
and your own (e.g. `/mail/Receipts`):

```qfs
/mail/inbox |> select date, from, subject |> limit 20
```

```qfs
/mail/sent |> select date, subject |> limit 20
```

```qfs
/mail/starred |> select date, from, subject
```

## Search & read

`WHERE` and `LIMIT` push down into Gmail's own search — qfs turns the predicate into a Gmail `q`
query and re-filters locally only the parts Gmail can't express exactly.

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
|> select date, subject, snippet
```

**A date range, with the attachments column:**

```qfs
/mail/inbox
|> where date BETWEEN '2026-01-01' AND '2026-03-31'
|> select date, from, subject, attachments
```

**Unread from the last quarter, across the whole mailbox:**

```qfs
/mail/unread
|> where date > '2026-04-01'
|> select date, from, subject
|> order by date DESC
```

## Read one message or attachment

**A single message** as a file under its label (the message id is the last segment; a listing's
`id` and `thread_id` columns give you the ids):

```qfs
/mail/inbox/18f1a2b3c4
|> select date, from, subject, snippet
```

**List a message's attachments**, then **download one** — the attachment node
`/mail/<label>/<msg-id>/<att-id>` reads the `filename`, `mime`, `size`, and the decoded `content`
bytes (gmail-ftp `get id:att:<msg>:<att>`):

```qfs
/mail/inbox/18f1a2b3c4
|> select attachments

/mail/inbox/18f1a2b3c4/ANGjdJ_att0
|> select filename, mime, size, content
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

## Triage — relabel, star, archive

Relabeling is an `UPDATE` on a label: `set add_labels` / `remove_labels` (comma-separated label
ids) selects what to add and remove. It previews like any effect and only applies on `--commit`.

**Star and mark-read everything from your boss:**

```qfs
update /mail/inbox
  set add_labels = 'STARRED', remove_labels = 'UNREAD'
  where from == 'boss@example.com'
```

**Archive newsletters** (archiving is just *removing the `INBOX` label* — the message stays in All
Mail):

```qfs
update /mail/inbox
  set remove_labels = 'INBOX'
  where subject LIKE '%newsletter%'
```

**File receipts under a user label:**

```qfs
update /mail/inbox
  set add_labels = 'Receipts'
  where from LIKE '%@stripe.com'
```

**Create a new label first** (gmail-ftp `mkdir`). Labels are the mailbox's directories, so making
one is an `INSERT` into the `/mail/labels` collection — a nested `Parent/Child` name creates the
hierarchy. It previews like any write and only applies on `--commit`:

```qfs
insert into /mail/labels
  values ('Work/Receipts')
```

## Write — draft and send

**Draft an email** (reversible — creating a draft sends nothing). Writing a draft *previews* until
you `--commit`:

```qfs
insert into /mail/drafts
  values ('alice@example.com', 'Q3 report', 'See attached.')
```

```text
PREVIEW: 1 effect(s)
  #0 INSERT -> mail:/mail/drafts [affected 1]
  total affected: 1
```

**Draft with an attachment.** The `attachments` column is an **array of structs**, each
`{ filename, mime, bytes }` — qfs's general nested-value literals. Give `bytes` a hex `X'…'` bytes
literal (or a plain string for text), and name the columns so the array lands in `attachments`:

```qfs
insert into /mail/drafts
  values (to, subject, body, attachments)
         ('alice@example.com', 'Q3 report', 'See attached.',
          [ { filename: 'hello.txt', mime: 'text/plain', bytes: X'68656c6c6f' } ])
```

The draft is built as a `multipart/mixed` message with the file attached — still a reversible
preview until you `--commit`. Piping a **Google Drive** download straight into this column (instead
of an inline literal) is the [cross-service attach-and-send recipe](/cookbook/cross-service).

**Draft, then send it.** The draft is reversible; the send is the irreversible step:

```qfs
insert into /mail/drafts
  values ('alice@example.com', 'Q3 report', 'See attached.')

/mail/drafts
|> call mail.send
```

::: warning Irreversible
`CALL mail.send` can't be undone. In a one-shot it needs `--commit --commit-irreversible`. A retry
re-sends the **same** draft (de-duplicated by draft id), never a fresh message.
:::

## Clean up — trash

**Trash by subject** (also irreversible — the preview shows it as a gate):

```qfs
remove /mail/inbox
  where subject LIKE '%unsubscribe%'
```

```text
PREVIEW: 1 effect(s)
  #0 REMOVE -> mail:/mail/inbox [affected ?] (!)
  (!) irreversible: 1 node(s) [#0]
  total affected: ?
```

The `(!)` marks the irreversible gate: a one-shot needs `--commit --commit-irreversible` to apply it.

**Trash by sender:**

```qfs
remove /mail/inbox
  where from == 'noreply@spammy.example'
```

**Trash one message** by its path (the id is the last segment):

```qfs
remove /mail/inbox/18f1a2b3c4
```

::: tip describe never lies about verbs
`qfs describe` reports the **exact** verb set for each node, derived from its real capabilities —
they differ by node, and a verb that isn't listed is rejected at parse time, never silently dropped:

- `describe /mail` → `LS SELECT` — the mailbox root lists your labels.
- `describe /mail/inbox` → `SELECT UPDATE REMOVE` — a label is a tail you read, *relabel* (`UPDATE`),
  and trash (`REMOVE`). You don't append new mail to your own inbox, so there's no `INSERT`.
- `describe /mail/drafts` → `SELECT INSERT UPSERT REMOVE` — drafts is the append log you write to.
- `describe id:<msg>` → `SELECT REMOVE`; `describe id:thread:<id>` → `REMOVE`; an attachment →
  `SELECT`.
:::
