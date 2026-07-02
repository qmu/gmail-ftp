# Connect a service

qfs reads `/local`, `/sys`, and any file you point it at with **no setup at all**. Everything else —
a database, a git repo, mail, Drive, GitHub, Slack, object storage — needs a one-time **connection**
that tells qfs where the source is and (for cloud services) how to authenticate.

This page is the per-service how-to: the exact commands for each source. For the underlying model
(what a connection *is*, the encrypted store, rotating/revoking secrets) see
[Connections & credentials](/guide/connections).

Two things some services need first:

- **A passphrase for the local secret store.** Any service that stores a secret (mail, Drive,
  GitHub, Slack, S3, R2) needs `QFS_PASSPHRASE` — a password you choose that encrypts the service
  logins you save on this machine (not any service's own password). On a terminal qfs will simply
  **prompt** you for it; to set it for the whole shell session instead, export it without leaking it
  into history. See **[The QFS passphrase](/guide/passphrase)** for all the options (prompt, export,
  `.env`, OS keychain) and their trade-offs.
  ```sh
  read -rs QFS_PASSPHRASE; export QFS_PASSPHRASE
  ```
- **A sign-in, for Google services.** Adding a Gmail / Drive / Analytics connection requires a
  signed-in identity first: `qfs identity signup <email>`.

Local databases and git need **neither** — they store no secret.

## Local databases & git — declare a connection

A SQLite file or a git repo *is* its location, so it needs no secret — you just **declare** it. Put
a `CREATE CONNECTION` statement in a `connections.qfs` file; the name you give it is the `<conn>`
path segment:

```text
CREATE CONNECTION orders DRIVER sqlite AT '/data/orders.db';    -- → /sql/orders/<table>
CREATE CONNECTION app    DRIVER git    AT '/srv/repos/app.git'; -- → /git/app/commits, /git/app@<ref>/…
```

Point qfs at the file with `QFS_CONNECTIONS=/path/to/connections.qfs`, or drop it at the default
`~/.config/qfs/connections.qfs`. A `/sql/orders/…` or `/git/app/…` query then works — no passphrase,
no `connection add`. The declaration is the **source of truth**: something you can read, review, and
commit to a repo, not a setting hidden in an env var's name. (Reading the database's `WHERE` is
pushed into the backend; git uses the commit you name.)

::: warning The `QFS_SQL_*` / `QFS_GIT_*` env vars are deprecated
The old `export QFS_SQL_ORDERS=/data/orders.db` / `export QFS_GIT_APP=…` convention still works as a
temporary fallback — but it warns once and is being retired, because the connection lived only in the
*name* of an environment variable, with nothing to read or review. Run **`qfs connection import-env`**
to print the `CREATE CONNECTION` lines equivalent to your current env vars, then paste them into a
`connections.qfs`.
:::

## Gmail, Google Drive & Google Analytics — Google sign-in

All three Google sources share one OAuth consent. Sign in, add the connection, then **mount** each
service with `qfs connect` — nothing is pre-mounted, so you choose where each path lives:

```sh
qfs identity signup you@example.com            # register a local identity (once per machine)
export QFS_PASSPHRASE                           # the store passphrase (above)

# Interactive browser consent — one consent covers Gmail, Drive, and Analytics:
QFS_GOOGLE_CONSENT=1 qfs connection add gmail default

qfs connect /mail  --driver gmail              # mount Gmail at /mail
qfs connect /drive --driver gdrive             # mount Drive at /drive
```

`gmail` / `gdrive` / `ga` is the driver; the consent is recorded against the connection and the
refresh token is stored encrypted and never printed. To provision a refresh token out of band
instead of the browser flow, pipe it on stdin — the full walkthrough (and a token-import shortcut)
is in the [Gmail cookbook Setup](/cookbook/gmail#setup).

Once connected, **`/mail` and `/drive` reads return your real messages and files.** See the
[Gmail cookbook](/cookbook/gmail) and the [Google Drive cookbook](/cookbook/gdrive) for the full set
of read/search/write recipes over each.

## GitHub & Slack — a token

Create a token in the service (a GitHub personal-access token, a Slack bot/user token), then pipe the
**value** in on stdin — never on argv, where it would leak into your shell history and the process
table:

```sh
export QFS_PASSPHRASE
printf %s "$GH_TOKEN"    | qfs connection add github personal   # → /github/personal/…
printf %s "$SLACK_TOKEN" | qfs connection add slack team        # → /slack/team/…
```

qfs stores the token encrypted and never prints it back. `connection list` shows the connection name
only.

## Amazon S3 & Cloudflare R2 — access keys

S3 and R2 sign each request with an access-key pair. The **secret** access key is stored with
`connection add` (piped on stdin); its non-secret partner (the access-key id) and the region /
endpoint / bucket are configured alongside it — see the S3/R2 setup in
[Connections & credentials](/guide/connections):

```sh
export QFS_PASSPHRASE
printf %s "$AWS_SECRET_ACCESS_KEY" | qfs connection add s3 prod   # → /s3/prod/…
printf %s "$R2_SECRET_ACCESS_KEY"  | qfs connection add r2 backups
```

## After connecting

```sh
qfs connection list            # the connections you've added (names + metadata only, never secrets)
qfs run "/mail/inbox |> where subject LIKE '%invoice%' |> select date, subject"
```

`describe` and `preview` never need any of this — they're always offline. A connection is only
required to **read rows** from a source or **commit** a change to it.

See [Connections & credentials](/guide/connections) for the full model: choosing the active
connection, rotating and revoking secrets, re-keying the store, and how the store is encrypted.
