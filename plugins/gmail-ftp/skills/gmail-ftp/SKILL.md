---
name: gmail-ftp
description: Use when a task needs to read or modify the user's Gmail from the command line — listing or navigating labels, listing or searching messages, downloading messages/attachments, composing a draft and attaching files, sending a draft, creating a label, or trashing a message — via the `gmail-ftp` CLI. Covers the one-time auth setup and non-interactive one-shot command usage.
---

# Using gmail-ftp for Gmail

`gmail-ftp` is an FTP-style CLI for Gmail. Use it to list and navigate **labels**,
list and search **messages**, download messages and attachments, compose drafts
and attach files, **send** a draft, create labels, and trash messages. Prefer
**one-shot** commands
(`gmail-ftp <cmd> args`) — each runs one command and exits, which is what you want
as an agent. The interactive shell (`gmail-ftp` with no args) exists too but you
generally won't use it.

`README.md` in the gmail-ftp repo is the authoritative spec; this skill must stay
consistent with it.

## Prerequisites (check these first)

1. `gmail-ftp` is on `PATH` (`command -v gmail-ftp`).
2. **Authorized once**: a cached token must exist at
   `~/.config/gmail-ftp/token.json`. If it's missing, the user must run
   `gmail-ftp auth` **interactively** (it walks an OAuth consent flow). Do not try
   to auth non-interactively — it blocks on a prompt. If a command fails with an
   auth/consent error, tell the user to run `gmail-ftp auth`.
3. The Gmail API must be enabled for the OAuth client's Cloud project. If you see
   *"the Gmail API is disabled for this OAuth client's Google Cloud project"*, the
   user must enable it in the Cloud Console and retry after ~1 minute.

Never read, print, commit, or move `credentials.json` or `token.json` — they are
secrets that grant mailbox access.

## Path model (important — 2 levels)

Navigation is exactly **two levels**: `root → label → message`.

- The top level is a **virtual root** listing the user's **labels** (INBOX, SENT,
  STARRED, … plus user labels). The **first path component selects the label**.
- Inside a label, the leaves are **messages**, named by date + subject (e.g.
  `2026-06-18 Quarterly report`; empty subjects render as `(no subject)`).
- A **message is a leaf** — you cannot `cd` into it. `ls "<label>/<message>/"`
  (or `ls id:<msgID>`) lists the message's **attachments**.
- A **thread is NOT a navigation tier.** Each message row has a `threadId`; a
  whole thread is addressed opt-in via `id:thread:<id>` (`get` to export `.mbox`,
  `rm` to trash). Never `cd` into a thread.
- One-shot mode has **no persistent working directory** — every command starts at
  the root, so **always use absolute paths beginning with the label**, e.g.
  `"/INBOX"`, or address messages by `id:`.
- **Quote** any name containing spaces (the whole path is one argument):
  `"/INBOX/2026-06-18 Quarterly report"`.

### Addressing by ID (prefer this as an agent)

Message names are synthesized and can collide, so prefer `id:` addressing:

```sh
gmail-ftp get id:18f1a2b ./msg.eml          # download a message (.eml)
gmail-ftp get id:18f1a2b out.txt            # readable .txt export
gmail-ftp rm  id:18f1a2b                     # trash a SINGLE message
gmail-ftp ls  id:att:18f1a2b:ANGjdJ8         # show one attachment leaf
gmail-ftp get id:att:18f1a2b:ANGjdJ8 ./file  # download one attachment
gmail-ftp get id:thread:18f1a2b ./conv.mbox  # export a whole thread (opt-in)
gmail-ftp rm  id:thread:18f1a2b              # trash a whole thread (explicit)
```

`rm` of a message name or `id:<msgID>` **always trashes exactly one message** —
never a whole conversation. Only the explicit `id:thread:<id>` trashes a thread.

## Commands (one-shot examples)

```sh
# List all labels
gmail-ftp ls /

# List a label's messages
gmail-ftp ls /INBOX
gmail-ftp ls /Work

# Search the current/whole mailbox
gmail-ftp find report /INBOX            # subject substring within a label
gmail-ftp search "from:alice is:unread" # raw Gmail query, whole mailbox

# Download a message (raw .eml, or readable .txt by extension)
gmail-ftp get id:18f1a2b ./report.eml
gmail-ftp get id:18f1a2b ./report.txt

# Create a DRAFT from a local RFC 822 file (NEVER sends)
gmail-ftp put ./draft.eml

# Compose -> attach -> send (the ONLY path that sends; each step explicit):
gmail-ftp compose --to a@b.test --subject "Q2 report" ./body.txt  # drafts; never sends
gmail-ftp put ./report.pdf id:draft:r-8423                        # attach to that draft; never sends
gmail-ftp send id:draft:r-8423                                    # SENDS (irreversible, audited)

# Create a label (the mkdir analogue)
gmail-ftp mkdir Work/Receipts

# Trash a single message (reversible — NOT a permanent delete)
gmail-ftp rm id:18f1a2b
```

Success output goes to **stdout** (e.g. `downloaded …`, `drafted … — not sent`,
`attached … — not sent`, `sent message …`, `created label …`, `trashed …`).
`ls` of a label prints message rows
`<unread-flag> <date> <from> <name>`, with `*` marking unread; `ls /` prints
labels with a trailing `/`.

`lcd` / `lls` / `lpwd` are local-filesystem helpers, meaningful only in the
interactive shell — ignore them in one-shot use.

`send` sends an existing draft — the **only irreversible** action. It is
reachable **only** through the explicit `send` verb (never from `put`/`compose`),
and every send is recorded in the audit log. `compose` drafts a message from
To/Subject/body and `put <local> <draft>` attaches a file to a draft; neither
sends. `label` and `unlabel` remain **deferred to v1.1** (the verbs exist but
return a deferral notice and do nothing).

Flags: `-creds <path>` and `-token <path>` override the credential/token
locations (defaults under `~/.config/gmail-ftp/`). `-json` switches output to
machine-readable JSON (see below).

## JSON output (`-json`) — prefer this as an agent

```sh
gmail-ftp -json ls /INBOX
# → stdout: [{"name":"2026-06-18 Quarterly report","id":"18f1a2b","kind":"message","from":"alice@example.com","subject":"Quarterly report","date":"...","size":12678,"threadId":"18f1a2b"}]

gmail-ftp -json put ./draft.eml
# → stdout: {"action":"drafted","name":"draft.eml","id":"r-8423","threadId":"18f1c0d","size":612}

gmail-ftp -json send id:draft:r-8423
# → stdout: {"action":"sent","name":"to a@b.test","id":"<sent-msg-id>","threadId":"18f1c0d"}

gmail-ftp -json get id:nope
# → stderr: {"error":"no such file or directory"} , exit 1
```

Contract: **results on stdout** (an array for `ls`/`find`/`search`; a single
object for `get`/`put`/`compose`/`send`/`mkdir`/`rm`; `{"path":…}` for `pwd`),
**errors on stderr** as `{"error":…}` with a **non-zero exit**. Entry keys:
`name`, `id`, `kind` (`label`/`message`/`attachment`), plus
`from`/`subject`/`date`/`unread`/`size`/`threadId` for messages (omitted when
empty). Action objects carry `action`
(`downloaded`/`drafted`/`attached`/`sent`/`trashed`/`created`) plus
`dest`/`size`/`id`/`threadId`.
Capture an `id` from a result and reuse it as `id:<id>` (or `id:thread:<threadId>`)
in a follow-up command.

## Audit log (review what was changed)

Every **mutating** operation you run — `put`/`compose` (draft), `put <local>
<draft>` (attach), `send` (the irreversible send, `op":"send"`), `rm` (trash),
`mkdir` (label) — is appended to `~/.config/gmail-ftp/audit.jsonl` (JSON Lines,
append-only). Read-only commands (`ls`/`cd`/`pwd`/`get`/`find`/`search`) are not
logged. Each record has a `time`, `op`, `name`, Gmail `id`, `threadId`, `cwd`,
and `size`. It never contains credentials or message contents.

```sh
tail ~/.config/gmail-ftp/audit.jsonl
grep '"op":"trash"' ~/.config/gmail-ftp/audit.jsonl | jq -r '.id'
```

Recover using the logged `id` (restore a trashed message from the Gmail web UI,
or re-`get` it by `id:`). Pass `-no-log` to disable logging for a command.

There is also a `gmail-ftp log` subcommand. In a terminal it opens an interactive
`j`/`k` browser; when piped or run with `-json` it prints the entries — **prefer
`gmail-ftp -json log`** to read the history as an array. It is read-only and needs
no auth.

## Error / exit contract (for scripting)

On failure, gmail-ftp prints `gmail-ftp: <message>` to **stderr** and exits
**non-zero**. Always check the exit code. Common messages:

- `no such file or directory` — the message/label doesn't exist (check the label
  name and exact casing, or use `id:`).
- `ambiguous name (multiple matches); address by id: to disambiguate` — two
  messages share a synthesized name; address by `id:` instead.
- `<seg>: not a directory (messages are leaves; cd a label)` — you tried to `cd`
  into a message; only labels are navigable.
- `label is deferred to v1.1` / `unlabel is deferred to v1.1` — those verbs are
  not active yet. (`send` is active: it sends a draft and is irreversible.)

## Gotchas

- Navigation is **2 levels**: root lists labels, a label lists messages,
  attachments live inside a message. No folders-in-labels, no thread tier.
- One-shot has no cwd → use absolute, label-prefixed paths, or `id:` addressing.
- Message names are synthesized (date + subject) and may collide — prefer `id:`.
- `rm` trashes a **single** message (reversible); only `id:thread:<id>` trashes a
  whole thread.
- `put` and `compose` create a **draft only** and never send. `send` is the one
  irreversible action — it sends an existing draft and is audited; it is never
  triggered by `put`/`compose`.
- Listing a large label is **capped** (first ~50) and may show "showing first N";
  use `search` or a narrower label for more.
- This skill tracks the CLI — if commands/flags change, update it from
  `README.md`.
