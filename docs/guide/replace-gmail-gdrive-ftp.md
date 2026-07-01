# Replace gmail-ftp / gdrive-ftp with qfs

If you use the `gmail-ftp` / `gdrive-ftp` command-line tools, qfs does the same jobs — reading,
searching, downloading, drafting, sending, and trashing — through its one uniform query language.
This page maps every FTP command to its qfs equivalent. The read/search commands here are verified
against a real Google account; the write commands are shown as they preview and commit.

## Setup (once)

The full walkthrough — OAuth app, sign-in, consent, and the token-import shortcut — is in the
[Gmail cookbook Setup](/cookbook/gmail#setup). The short version:

```sh
# 1. Your Google OAuth "Desktop" app (the same credentials.json the FTP tools use):
cat credentials.json | qfs connection add google-app default

# 2. Sign in + one consent that covers BOTH Gmail and Drive (open the URL over an SSH port-forward
#    on a headless host). A single fresh consent is the way to use /mail and /drive together —
#    the two FTP tools' saved tokens carry only their own scope, so importing one covers one service.
printf '%s' "$PASSWORD" | qfs identity signup you@example.com
QFS_GOOGLE_CONSENT=1 qfs connection add gmail default

# 3. Mount the paths (nothing is pre-mounted — you choose where each service lives):
qfs connect /mail  --driver gmail
qfs connect /drive --driver gdrive
```

`QFS_PASSPHRASE` must be exported (it unlocks the encrypted token store). Reusing an existing
`~/.config/{gmail,gdrive}-ftp/token.json` refresh token instead of a fresh consent is documented in
the [cookbook Import path](/cookbook/gmail#_3-authorize-your-account-get-a-refresh-token).

## gmail-ftp → qfs

`/mail` is the mount from `qfs connect /mail --driver gmail`. Labels are directories, messages are
files. Gmail's `label:` search is case-insensitive, so write labels however you like (`inbox`).

| gmail-ftp | qfs |
| --------- | --- |
| `ls` (root) | `/mail \|> select name` — list your labels |
| `ls <label>` | `/mail/inbox \|> select date, from, subject` |
| `ls <msg>/` (attachments) | `/mail/inbox/<msg-id> \|> select attachments` |
| `cd <label>` | in the shell: `cd /mail/inbox` (or just query the label path) |
| `pwd` | in the shell: `pwd` |
| `find <pat> [label]` | `/mail/inbox \|> where subject LIKE '%pat%'` |
| `search <gmail-query>` | `/mail/inbox \|> where from == 'x@y.z'` — `WHERE` pushes down into Gmail search |
| `get <msg>` | `id:<msg-id> \|> select date, from, subject, snippet` — read the message |
| `get id:att:<msg>:<att>` | `/mail/inbox/<msg-id>/<att-id> \|> select filename, mime, size` |
| `get id:thread:<id>` | `id:thread:<id> \|> select date, from, subject` |
| `put` / `compose` (make a draft) | `insert into /mail/drafts values ('to@x.y', 'Subject', 'Body')` |
| `send <draft>` | `/mail/drafts \|> call mail.send` — **irreversible** |
| `rm <msg>` | `remove /mail/inbox where id == '<msg-id>'` — trash (**irreversible**) |
| `rm id:thread:<id>` | `remove id:thread:<id>` — trash the whole thread (**irreversible**) |

**Beyond the FTP tools:** qfs also *relabels* in one statement — `update /mail/inbox set
add_labels = 'STARRED', remove_labels = 'UNREAD' where from == 'boss@x.y'` (star + mark read),
or archive with `set remove_labels = 'INBOX'`. See the [Gmail cookbook](/cookbook/gmail).

**Not yet:** creating a *new* user label (gmail-ftp's `mkdir`, its own v1.1 item) is not a qfs
command — `add_labels` applies an existing label. Raw `.eml`/`.mbox` export is a gmail-ftp format
feature; qfs returns structured rows.

## gdrive-ftp → qfs

`/drive` is the mount from `qfs connect /drive --driver gdrive`. Folders are directories, files are
blobs. The root lists the two corpora, `my` (My Drive) and `shared` (Shared Drives).

| gdrive-ftp | qfs |
| ---------- | --- |
| `ls` (root) | `/drive \|> select name` — lists `my` and `shared` |
| `ls [dir]` | `/drive/my \|> select name, mime_type, size, modified_time` |
| `ls` a Shared Drive | `/drive/shared/<DriveName> \|> select name, mime_type, size` |
| `cd [dir]` | in the shell: `cd /drive/my/Reports` |
| `pwd` | in the shell: `pwd` |
| `find <pat> [dir]` | `/drive/my \|> where name LIKE '%pat%' \|> select name` |
| `get <remote>` | `id:<file-id> \|> select name, mime_type, size, md5` — resolve/download a file |
| `put <local> [folder]` | `insert into /drive/my/<folder> values ('name.pdf', 'application/pdf', <content>)` |
| `mkdir <name>` | `insert into /drive/my/<folder> values ('<name>', 'application/vnd.google-apps.folder', '')` |
| `rm <name>` | `remove /drive/my where name == '<name>'` — trash (**irreversible**) |

**Not yet:** Google-native export (gdrive-ftp's Docs→docx / Sheets→xlsx / Slides→pptx `get`) is a
format feature; qfs reports `is_google_doc` and the file's metadata. `copy` is available as a
`CALL` procedure (`call drive.copy`).

## PREVIEW → commit, and the irreversible gate

Every write **previews by default** — it prints the plan and changes nothing:

```sh
qfs run "insert into /mail/drafts values ('a@b.example', 'Hi', 'Body')"
# → PREVIEW: INSERT -> /mail/drafts [affected 1]; nothing applied
```

Add `--commit` to apply a reversible write (a draft, a relabel, an upload). The two irreversible
actions — **sending** a draft and **trashing** — additionally need `--commit-irreversible` in a
non-interactive one-shot (an interactive shell confirms with a typed `COMMIT`):

```sh
qfs run "/mail/drafts |> call mail.send" --commit --commit-irreversible
```

This is qfs's honesty bar: the preview shows exactly what a command would do, and the irreversible
ones can't fire by accident.

## Interactive shell (FTP-like)

Run `qfs` with no arguments for an FTP-style shell. `cd` / `ls` / `pwd` navigate the same paths, so
`cd /mail/inbox` then `ls` reads your inbox, and `cd /drive/my` then `ls` browses My Drive — the
familiar loop, over every connected service at once. See the [interactive shell
guide](/guide/shell).
