---
name: qfs-gmail
description: Use when a task needs to read or triage Gmail through qfs — search, read, summarize, draft, send, relabel, or trash mail via the /mail path and its pipe-SQL queries. Covers connecting a Google account and the label, message, thread, attachment, and draft surface.
---

# Gmail

Your whole mailbox becomes a set of queryable paths. Labels are directories, messages are files, and
one pipe-SQL language searches, triages, drafts, sends, relabels, and cleans up — the same verbs you
already use on a database, a git repo, or a folder of files.

## See it work first

**Show me the mail that actually needs me** — unread, this quarter, from a real human (not a
`noreply` robot), newest first:

```qfs
/mail/unread
|> where date > '2026-04-01'
     AND NOT from LIKE '%noreply%'
|> select date, from, subject
|> order by date DESC
|> limit 20
```

```text
date        from                    subject
2026-06-30  jordan@northwind.com    Re: Q3 renewal — a couple of questions
2026-06-29  taylor@acme.io          Contract redlines attached
2026-06-27  priya@boldpeak.dev      Can we move the review to Thursday?
… 20 rows
```

That read runs the instant you connect an account. Now the **smart** part — one statement stars and
marks-read everything from your boss, and previews before it touches anything:

```qfs
update /mail/inbox
  set add_labels = 'STARRED', remove_labels = 'UNREAD'
  where from == 'boss@example.com'
```

```text
PREVIEW: 1 effect(s)
  #0 UPDATE -> mail:/mail/inbox [affected 3]
  total affected: 3
```

::: tip Reads run now; writes preview
Every **read** returns rows immediately. Every **write** (`update`, `insert`, `remove`, `call`)
*previews* by default and changes nothing — add `--commit` to apply it, `--commit-irreversible` for
the ones that can't be undone (sending, trashing). Paste any recipe below and safely watch what it
*would* do first.
:::

Gmail isn't reachable until you connect a Google account to a path — about five minutes, once, in
**[Setup](#setup)**. After that every recipe on this page works verbatim.

## Setup

::: tip Prerequisites — an operator, an account, a mount
Reaching a cloud service takes three one-time steps: a signed-in operator (`qfs init` —
**[The operator identity](/guide/operator)**), an authorized account (`qfs account add …`), and a
mount binding that account to a path (`qfs connect …`). The happy path below is exactly those
three, plus registering your OAuth app.
:::

You connect Gmail once. The happy path is four commands:

```sh
qfs init you@example.com                                   # 1. the operator + the vault
cat credentials.json | qfs app add google                  # 2. your OAuth app
qfs account add google                                     # 3. authorize (browser consent)
qfs connect /mail --driver gmail --account you@gmail.com   # 4. mount it at /mail
```

The rest of this section explains each line and the alternatives.

### 0. Prerequisites

- A Google account.
- A Google **Desktop-app** OAuth client (`client_id` + `client_secret`), downloaded from the
  [Google Cloud console](https://console.cloud.google.com/apis/credentials) as `credentials.json`,
  with the **Gmail API** enabled for the project.

### 1. Ready the machine

Cloud drivers require a signed-in operator — qfs fails closed for an anonymous one. `qfs init`
creates the encrypted credential store (where the refresh token is sealed at rest — see
**[The QFS passphrase](/guide/passphrase)**) and registers you as this machine's operator. There is
no password: your OS login is the authentication, and the email is an accountability label.
Re-running it is safe — it reports what already exists:

```sh
qfs init you@example.com
```

### 2. Hand qfs your OAuth app credentials

Register the downloaded `credentials.json` in qfs's own encrypted store, so it no longer depends on
a file on disk:

```sh
cat credentials.json | qfs app add google
```

CI and agents can instead export `QFS_GOOGLE_CLIENT_ID` and `QFS_GOOGLE_CLIENT_SECRET`.

### 3. Authorize your account (get a refresh token)

Pick **one** path. Either way, **one authorization serves Gmail, Google Drive, and Google
Analytics** — the account is stored once, under your email as its label.

**A — Fresh browser consent (recommended).** On a terminal, one command opens a Google consent
screen; approve it and qfs seals the refresh token and records your consent:

```sh
qfs account add google
```

**B — Pipe an existing refresh token** (reuse one a prior tool already obtained, or automate on a
headless box with no browser). The token comes in on **stdin**, never argv, and the email is the
account's label:

```sh
printf '%s' "$REFRESH_TOKEN" | qfs account add google you@gmail.com
```

`qfs account list` shows the authorized accounts; `qfs account remove google you@gmail.com` deletes
one along with its consent record.

### 4. Connect the path

Mount Gmail wherever you like — the mount carries the account, and the rest of this cookbook
assumes `/mail`:

```sh
qfs connect /mail --driver gmail --account you@gmail.com
```

`qfs connect --list` now lists it, and `qfs describe /mail` shows the schema and verbs.
`qfs disconnect /mail` removes the mount.

::: info The mount path is yours — and so is the account it carries
`/work/gmail` works just as well as `/mail` — mount with
`qfs connect /work/gmail --driver gmail --account you@gmail.com` and every `/mail/…` recipe below
simply becomes `/work/gmail/…`. Two Gmail accounts are simply two mounts in the same process:

```sh
qfs connect /mail  --driver gmail --account work@example.com
qfs connect /mail2 --driver gmail --account home@example.com
```
:::

### 5. Read real mail

```sh
qfs run "/mail/inbox |> select date, from, subject |> limit 5"
```

Real messages come back. If a read reports *connect a Google account to read mail*, revisit steps
1–3: the path resolved (you're past addressing), but the cloud bind gate has no signed-in operator
or recorded consent yet.

## The mailbox as paths

Once connected, `/mail` is your Gmail account as an **append log** mapped onto a filesystem shape:

| Gmail thing | qfs path | it is a… |
| ----------- | -------- | -------- |
| a label | `/mail/inbox`, `/mail/sent`, `/mail/<UserLabel>` | directory of messages |
| a message | `/mail/inbox/<msg-id>` or `id:<msg-id>` | file |
| a thread | `id:thread:<thread-id>` | file (the whole conversation) |
| an attachment | `/mail/inbox/<msg-id>/<att-id>` | nested entry |
| your drafts | `/mail/drafts` | the append target you write new mail to |

Message columns: `id`, `thread_id`, `date`, `from`, `subject`, `snippet`, `label_ids`,
`attachments`. Attachment columns: `filename`, `mime`, `size`. Run `qfs describe /mail/inbox` for the
exact schema and verbs of any node.

::: tip Labels are written verbatim
A label segment reaches Gmail **exactly as you write it** — qfs never rewrites the case. It becomes a
`label:` search term, which Gmail matches case-insensitively, so lowercase just works: `sent`,
`spam`, `trash`, `starred`, `important`, `unread`, the `category_*` tabs, and your own user labels
all read the same way. `drafts` is qfs's reserved write collection (the INSERT target), not a label.
:::

## Browse the mailbox

**List your labels** — the directories under `/mail`:

```qfs
/mail |> select name
```

**Read any label** — the system ones (`inbox`, `sent`, `starred`, `important`, `spam`, `unread`) and
your own (e.g. `/mail/Receipts`):

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

**A single message** as a file under its label — the message id is the last segment (a listing's
`id` and `thread_id` columns give you the ids):

```qfs
/mail/inbox/18f1a2b3c4
|> select date, from, subject, snippet
```

**List a message's attachments**, then **download one** — the attachment node
`/mail/<label>/<msg-id>/<att-id>` reads `filename`, `mime`, `size`, and the decoded `content` bytes:

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

Relabeling is an `UPDATE` on a label: `set add_labels` / `remove_labels` (comma-separated label ids)
picks what to add and remove. It previews like any effect and only applies on `--commit`.

**Star and mark-read everything from your boss:**

```qfs
update /mail/inbox
  set add_labels = 'STARRED', remove_labels = 'UNREAD'
  where from == 'boss@example.com'
```

**Archive newsletters** — archiving is just *removing the `INBOX` label*; the message stays in All
Mail:

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

**Create a new label first.** Labels are the mailbox's directories, so making one is an `INSERT`
into the `/mail/labels` collection — a nested `Parent/Child` name creates the hierarchy. It previews
like any write and only applies on `--commit`:

```qfs
insert into /mail/labels
  values ('Work/Receipts')
```

## Write — draft and send

**Draft an email** — reversible; creating a draft sends nothing. It *previews* until you `--commit`:

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
`{ filename, mime, bytes }`. Give `bytes` a hex `X'…'` literal (or a plain string for text), and name
the columns so the array lands in `attachments`:

```qfs
insert into /mail/drafts
  values (to, subject, body, attachments)
         ('alice@example.com', 'Q3 report', 'See attached.',
          [ { filename: 'hello.txt', mime: 'text/plain', bytes: X'68656c6c6f' } ])
```

The draft is built as a `multipart/mixed` message with the file attached — still a reversible preview
until you `--commit`. Piping a **Google Drive** download straight into this column (instead of an
inline literal) is the [cross-service attach-and-send recipe](/cookbook/cross-service).

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

**Trash by subject** — also irreversible; the preview shows it as a gate:

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
