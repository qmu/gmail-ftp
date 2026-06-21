# gmail-ftp

A tiny **FTP-style client for Gmail**, written in Go. It gives you a familiar
interactive shell — `ls`, `cd`, `pwd`, `find`, `get` (download), `put` (create a
draft / attach a file), `compose` (draft a message), `send` (send a draft),
`mkdir` (create a label), `rm` (trash) — that talks to your mailbox over the
official Gmail v1 API.

> [!WARNING]
> This requests `gmail.modify` + `gmail.compose` — it can read your mail, trash
> messages (reversibly), create labels, create drafts, and **send drafts**. It
> **never** requests the full `https://mail.google.com/` scope and **cannot
> permanently delete** mail. Sending is the one **irreversible** action: it
> happens only through the explicit `send` verb (never from `put`/`compose`) and
> every send is recorded in the audit log. Keep `credentials.json` /
> `token.json` private — they grant access to your mailbox.

## Navigation model (2-level: root → label → message)

Gmail has no folder tree, so gmail-ftp synthesizes one from **labels**. The model
is exactly **two levels**, the email analogue of gdrive-ftp's *root → drive →
file*:

- `ls /` lists your **labels** (INBOX, SENT, STARRED, … plus your user labels).
- `cd INBOX` enters a label; `ls` then lists that label's **messages**.
- A **message is a leaf** — you cannot `cd` into it. `ls <message>/` lists its
  **attachments** (leaves inside the message).
- A **thread is not a navigation tier.** Every message row carries a `threadId`,
  and a whole thread is addressable, opt-in, via `id:thread:<id>` (export with
  `get`, trash with `rm`). You never `cd` into a thread.

```
$ gmail-ftp
Connected to Gmail. Type 'help' for commands, 'quit' to exit.
gmail:/> ls
INBOX/
SENT/
STARRED/
Work/
gmail:/> cd INBOX
gmail:/INBOX> ls
* 2026-06-18 09:12   alice@example.com         2026-06-18 Quarterly report
  2026-06-17 18:30   billing@vendor.test       2026-06-17 Invoice 42
gmail:/INBOX> get "2026-06-18 Quarterly report" ./report.eml
downloaded 2026-06-18 Quarterly report.eml -> ./report.eml (12.4KB)
gmail:/INBOX> rm "2026-06-17 Invoice 42"
trashed 2026-06-17 Invoice 42
gmail:/INBOX> quit
```

A leading `*` marks an unread message.

## Build

Requires Go 1.25+.

```sh
go build -o gmail-ftp .
```

## One-time Google setup

The app talks to Gmail on *your* behalf, so it needs an OAuth client that you
own.

1. Go to the [Google Cloud Console](https://console.cloud.google.com/) and
   create (or pick) a project.
2. **APIs & Services → Library →** enable the **Gmail API**.
3. **APIs & Services → OAuth consent screen →** configure it (User type
   *External* is fine for personal use) and add your Google account under
   **Test users**.
4. **APIs & Services → Credentials → Create Credentials → OAuth client ID →**
   choose application type **Desktop app**. Download the JSON.
5. Save that file as `credentials.json` next to the binary, or at
   `~/.config/gmail-ftp/credentials.json`, or pass it with `-creds`.

On first run the app walks you through OAuth consent over the terminal (works
the same locally or over SSH — see [Authorizing](#authorizing)), then caches the
resulting token at `~/.config/gmail-ftp/token.json` so you only authorize once.
The token is refreshed automatically on later runs.

> **Keep `credentials.json` and `token.json` private** — they grant access to
> your mailbox. They are git-ignored by default.

## Usage

```
gmail-ftp [flags] [command args...]
```

With no command it starts the interactive shell. With a command it runs that
single command and exits (handy for scripts and agents):

```sh
gmail-ftp ls /
gmail-ftp ls /INBOX
gmail-ftp get id:18f1a2b ./message.eml
gmail-ftp put ./draft.eml
```

**Compose → attach → send** (the only path that sends; each step is explicit):

```sh
gmail-ftp compose --to a@b.test --subject "Q2 report" ./body.txt
# → drafted to a@b.test (draft r-8423) — not sent
gmail-ftp put ./report.pdf id:draft:r-8423
# → attached report.pdf to draft r-8423 — not sent
gmail-ftp send id:draft:r-8423
# → sent message <id> to a@b.test     (irreversible, audited)
```

### Flags

| Flag      | Default                              | Meaning                                     |
|-----------|--------------------------------------|---------------------------------------------|
| `-creds`  | `./credentials.json` or config dir   | OAuth client `credentials.json`             |
| `-token`  | `~/.config/gmail-ftp/token.json`     | Where to cache the auth token               |
| `-json`   | `false`                              | Emit machine-readable JSON instead of text  |
| `-no-log` | `false`                              | Disable the audit log of Gmail mutations    |

### Authorizing

The first run authorizes over the terminal — no local browser or callback server
needed, so it works the same on your laptop or a headless/SSH host. The consent
URL is printed; press **`c`** then Enter to copy it to your **local** clipboard
(sent via the OSC 52 terminal escape, so it works through SSH if your terminal
supports it), press **`o`** to try opening a browser on this host, or copy it
manually. Open it, approve, and the browser is redirected to a
`http://127.0.0.1` URL that fails to load — paste that **entire** URL back at the
prompt (pasting just the `code=` value also works). The `state` is verified to
guard against CSRF.

## Commands

| Command                  | Description                                                            |
|--------------------------|-----------------------------------------------------------------------|
| `ls [dir]`               | At root, list labels; inside a label, list its messages; `ls <message>/` lists its attachments. |
| `cd [label]`             | Enter a label. No argument (or `/`) goes to the root listing all labels. Messages and threads are **not** `cd` targets. |
| `pwd`                    | Print the remote working directory (`/` or `/<label>`).               |
| `find <pattern> [label]` | Search message subjects in the current (or anchored) label for a case-insensitive substring. |
| `search <gmail-query>`   | Search the whole mailbox with raw Gmail query syntax (`from:x is:unread`). |
| `get <remote> [local]`   | Download a message as `.eml` (raw RFC 822), or as `.txt` when the local name ends in `.txt`; download an attachment with `id:att:<msg>:<att>`; export a thread with `id:thread:<id>` (`.mbox`). |
| `put <local> [draft]`    | With no target, create a **draft** from a local RFC 822 `.eml`. With a draft target (`id:<draftId>` / `id:draft:<id>`), **attach** the local file to that draft. Returns the draft id. **Never sends.** |
| `compose --to <addr> --subject <s> [body-file]` | Create a **draft** from To/Subject/body (optional `--cc`; the optional positional names a local plain-text body file). Builds the MIME for you. **Never sends.** |
| `mkdir <name>`           | Create a Gmail **user label** (`mkdir Work/Receipts`).                 |
| `rm <name>`              | Trash a **single message** (reversible). A whole thread is trashed only via the explicit `rm id:thread:<id>`. |
| `send <draft>`           | **Send** an existing draft (`send id:draft:<id>` or `send id:<draftId>`). This is the one **irreversible** action — it is audited and reachable only via this explicit verb, never from `put`/`compose`. |
| `label` / `unlabel`      | **Deferred to v1.1** — message-level label add/remove ships later.    |
| `lcd [dir]`              | Change the *local* working directory.                                 |
| `lls [dir]`              | List a *local* directory.                                             |
| `lpwd`                   | Print the *local* working directory.                                  |
| `help [cmd]`             | Show command help.                                                    |
| `quit` / `exit` / `bye`  | End the session.                                                      |

Message names are synthesized from the date and subject (e.g.
`2026-06-18 Quarterly report`; an empty subject renders as `(no subject)`).
Because subjects are non-unique, **address a specific message by its id** when a
name is ambiguous.

**Addressing by ID.** Anywhere a remote item is expected, an `id:` token targets
it directly, skipping name navigation:

```
get id:18f1a2b ./msg.eml          # download a message by ID (.eml)
get id:18f1a2b out.txt            # readable .txt export of a message
rm  id:18f1a2b                    # trash a single message by ID
ls  id:att:18f1a2b:ANGjdJ8        # show one attachment leaf
get id:att:18f1a2b:ANGjdJ8 ./f    # download one attachment
get id:thread:18f1a2b ./conv.mbox # export a whole thread (opt-in)
rm  id:thread:18f1a2b             # trash a whole thread (explicit opt-in)
```

`rm` of a plain message name or `id:<msgID>` **always trashes exactly one
message** — never a whole conversation. Only `id:thread:<id>` trashes a thread.

**Search.** `find` is a simple subject substring within a label; `search` runs
Gmail's native query language across the mailbox. Both return IDs only from the
server, so each row's headers are fetched individually — the first listing of a
large label is capped (showing the first ~50) with a "showing first N" hint;
refine with `search` or a narrower label for more.

**Tab completion** (like `sftp`): in the interactive shell, press **Tab** to
complete command names, remote paths (labels and message names, fetched live),
and local paths for `lcd`/`lls`/`put`.

**zsh completion at your shell prompt** — add this to your `~/.zshrc` (after
`compinit`):

```zsh
source <(gmail-ftp completion zsh)
```

It uses your cached token and stays silent if you haven't authorized yet (run
`gmail-ftp auth` first). Each Tab makes a live Gmail call, so expect a brief
pause on large labels.

## JSON output

Pass the global `-json` flag to switch every command from human text to compact,
machine-readable JSON — handy for scripts and AI agents:

- **Results go to stdout** as a single JSON value: `ls`/`find`/`search` emit an
  **array** of entry objects; `get`/`put`/`mkdir`/`rm` emit a single result
  **object**; `pwd` emits `{"path":"…"}`. Output is one line, newline-terminated.
- **Errors go to stderr** as `{"error":"…"}` and the process still exits non-zero.
- Entry keys: `name`, `id`, `kind` (`label`/`message`/`attachment`), plus
  `from`, `subject`, `date`, `unread`, `size`, `threadId` for messages
  (omitted when empty). Action objects carry `action`
  (`downloaded`/`drafted`/`trashed`/`created`) plus `dest`/`size`/`id`/`threadId`.

```sh
$ gmail-ftp -json ls /INBOX
[{"name":"2026-06-18 Quarterly report","id":"18f1a2b","kind":"message","from":"alice@example.com","subject":"Quarterly report","date":"Thu, 18 Jun 2026 09:12:00 +0900","size":12678,"threadId":"18f1a2b"}]

$ gmail-ftp -json put ./draft.eml
{"action":"drafted","name":"draft.eml","id":"r-8423","threadId":"18f1c0d","size":612}

$ gmail-ftp -json get id:nope
{"error":"no such file or directory"}      # → stderr, exit 1
```

## Audit log

Every mutating operation — `put`/`compose` (draft create), `put <local> <draft>`
(attach), `send` (the irreversible send), `rm` (trash), and `mkdir` (label
create) — is appended to a local **audit log** so you (or an AI agent driving the
CLI) can look back at exactly what changed, and when. Read-only commands (`ls`,
`cd`, `pwd`, `get`, `find`, `search`) are not logged. A `send` is recorded with
op `send` so the one irreversible action is always traceable.

- **Location:** `~/.config/gmail-ftp/audit.jsonl`, beside `token.json` (file
  `0600`, directory `0700`).
- **Format:** [JSON Lines](https://jsonlines.org/) — one compact JSON object per
  line, append-only. Each record carries a timestamp, the operation, the target
  name and Gmail id, its `threadId`, the working directory, and a size. It never
  contains your credentials or any message contents.
- **Rotation:** when the log reaches ~5 MB it rotates to `audit.jsonl.1` (then
  `.2`, `.3`); the oldest segment is dropped, bounding disk use at ~20 MB.
- **Recovery:** because each record includes the Gmail id, you can act on it
  directly — e.g. find a trashed message's id and restore it from the Gmail web
  UI, or re-`get` it by `id:`.
- **Disable:** pass `-no-log` to turn logging off for an invocation.

```jsonl
{"time":"2026-06-18T17:40:11+09:00","op":"trash","name":"2026-06-17 Invoice 42","id":"18f1a2b","threadId":"18f1a2b","cwd":"/INBOX","size":8421}
{"time":"2026-06-18T17:41:02+09:00","op":"draft","name":"draft.eml","id":"r-8423","threadId":"18f1c0d","cwd":"/INBOX","size":612}
```

### Browsing the log

`gmail-ftp log` opens a small, read-only **`tig`-like browser** over the history
(`j`/`k` to move, `g`/`G` top/bottom, Enter for detail, `q` to quit). When stdout
is **not** a terminal (piped) or you pass `-json`, it instead prints the entries —
plain rows, or a JSON array under `-json` — so scripts read the same history:

```sh
gmail-ftp log              # interactive browser (in a terminal)
gmail-ftp -json log        # JSON array, for scripts/agents
gmail-ftp log | grep trash # plain rows, pipeable
```

## Notes & limitations

- **Least-privilege scopes:** `gmail.modify` (read + trash + label create) and
  `gmail.compose` (draft create **and** sending drafts — no separate
  `gmail.send` is requested). Never the full `https://mail.google.com/` scope,
  and **never** hard-delete.
- **`rm` trashes, it does not permanently delete** — messages land in Gmail's
  trash and can be restored from the web UI. A plain `rm` only ever trashes a
  single message.
- **`put` and `compose` create a draft and never send.** Sending happens **only**
  through the explicit, irreversible, audited `send` verb — never as a side
  effect of `put`/`compose`. Message-level `label`/`unlabel` remain **deferred to
  v1.1**.
- **Message names are synthesized** (date + subject) and can collide; address a
  specific message by `id:` when a name is ambiguous.
- **Listing is an N+1:** Gmail's `list` returns IDs only, so each row's headers
  are fetched individually. Listings are capped (first ~50) for responsiveness;
  use `search` or a narrower label for more.
- `get` is **atomic** — it downloads to a temp file and renames it into place
  only on full success, so an interrupted transfer never corrupts an existing
  local copy.

## Project layout

```
main.go                                   CLI wiring, flags, interactive vs one-shot
internal/auth/auth.go                     OAuth2 consent flow + token caching/refresh (scoped)
internal/gmail/client.go                  Gmail v1 wrapper (labels/messages/attachments/drafts/trash/labels)
internal/gmail/model.go                   Owned Ref/Message/Label types + message-name synthesis + MIME helpers
internal/shell/shell.go                   REPL, label/message resolution, tokenizer, id: addressing, completion
internal/shell/commands.go                Command implementations
internal/shell/output.go                  Owned JSON DTOs + emit seam
internal/audit/*                          Append-only JSONL audit log + tig-like browser
plugins/gmail-ftp/skills/gmail-ftp/       The agent skill (how to drive this CLI)
.claude-plugin/marketplace.json           Claude Code plugin marketplace
.agents/plugins/marketplace.json          Codex plugin marketplace
```

## Agent skill / plugin

This repo ships a skill that teaches a coding agent how to drive the CLI for
Gmail (one-shot commands, the 2-level label/message model, auth prerequisite,
and gotchas). It installs as a plugin on Claude Code and OpenAI Codex, or via the
cross-agent skills CLI:

| Agent | Install |
| ----- | ------- |
| **Claude Code** | `/plugin marketplace add qmu/gmail-ftp`, then enable the `gmail-ftp` plugin |
| **OpenAI Codex** | `codex plugin marketplace add qmu/gmail-ftp --ref main`<br>`codex plugin add gmail-ftp@gmail-ftp` |
| **Cursor / OpenCode / others** | `npx skills add qmu/gmail-ftp` |

> The plugin ships the **skill**; the `gmail-ftp` **binary** must be built or
> installed separately (see [Build](#build)) and on your `PATH`. Authorize once
> with `gmail-ftp auth` before agent use.
