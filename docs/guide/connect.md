# Connect a service

qfs reads `/local`, `/sys`, and any file you point it at with **no setup at all**. Everything else —
a database, a git repo, mail, Drive, GitHub, Slack, object storage — needs a one-time **connect**
that mounts the source at a path you choose and (for cloud services) names the account it uses.
A cloud path exists **only after a connect** — until then a read fails closed with
`unknown source`.

This page is the per-service how-to: the exact commands for each source. For the underlying model
(accounts, the encrypted vault, rotating/revoking secrets) see
[Connections & credentials](/guide/connections).

Two things cloud services need first:

- **A readied machine.** `qfs init <email>` (once per machine) creates the encrypted vault —
  walking you through choosing its passphrase — and registers the operator identity. There is no
  password: your OS login is the authentication. See **[The operator identity](/guide/operator)**
  and **[The QFS passphrase](/guide/passphrase)**.
- **An authorized account.** `qfs account add <provider> <label>` seals the account's token into
  the vault and records your consent. The mount then names that account — qfs refuses a cloud
  `account add` until `qfs init` has run.

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
no account. The declaration is the **source of truth**: something you can read, review, and
commit to a repo, not a setting hidden in an env var's name. (Reading the database's `WHERE` is
pushed into the backend; git uses the commit you name.)

A local source can also be mounted at a path of your own with the `CONNECT` statement — no
`ACCOUNT` clause, because there is no secret:

```qfs
CONNECT /db TO sqlite AT 'file:app.db'
```

::: warning The `QFS_SQL_*` / `QFS_GIT_*` env vars are deprecated
The old `export QFS_SQL_ORDERS=/data/orders.db` / `export QFS_GIT_APP=…` convention still works as a
temporary fallback — but it warns once and is being retired, because the connection lived only in the
*name* of an environment variable, with nothing to read or review. Run **`qfs connect --import-env`**
to print the `CREATE CONNECTION` lines equivalent to your current env vars, then paste them into a
`connections.qfs`.
:::

## The mount carries the account

For a cloud service, `qfs connect` binds a path to a `(driver, account)` pair. There is **no
selection state** — nothing is "active", and no command switches accounts. The mount itself names
which account it uses, so several accounts of one driver coexist as several mounts in the same
process:

```sh
qfs connect /mail  --driver gmail --account you@gmail.com    # work mail
qfs connect /mail2 --driver gmail --account home@gmail.com   # home mail, side by side
qfs disconnect /mail2                                        # remove a defined path (idempotent)
qfs connect --list                                           # list the defined paths
```

The same bind is a statement, so it can live in a script or a server config — `ACCOUNT` names the
account label, and an optional `HOST` names which qfs host owns the mount (omitted = the implicit
`local` host):

```qfs
CONNECT /mail TO gmail ACCOUNT 'you@gmail.com' HOST 'local'
```

(`QFS_GOOGLE_ACCOUNT` exists as a **CI/agent override only** — it pins the Google account for one
process. When unset, the account always comes off the mount.)

## Gmail, Google Drive & Google Analytics — Google sign-in

All three Google sources share one OAuth consent. Register your OAuth app's client credentials,
authorize the account, then **mount** each service with `qfs connect` — nothing is pre-mounted, so
you choose where each path lives:

```sh
qfs init you@example.com                        # ready the machine (once)
cat credentials.json | qfs app add google       # your OAuth app's client credentials (once)

# Interactive browser consent — one consent covers Gmail, Drive, and Analytics:
qfs account add google

qfs connect /mail  --driver gmail  --account you@gmail.com   # mount Gmail at /mail
qfs connect /drive --driver gdrive --account you@gmail.com   # mount Drive at /drive
```

`gmail` / `gdrive` / `ga` is the driver; the consent is recorded against the account and the
refresh token is stored encrypted and never printed. To provision a refresh token out of band
instead of the browser flow, pipe it on stdin with the email as the label —
`printf %s "$REFRESH_TOKEN" | qfs account add google you@gmail.com` — the full walkthrough is in
the [Gmail cookbook Setup](/cookbook/gmail#setup).

Once connected, **`/mail` and `/drive` reads return your real messages and files.** See the
[Gmail cookbook](/cookbook/gmail) and the [Google Drive cookbook](/cookbook/gdrive) for the full set
of read/search/write recipes over each.

## GitHub & Slack — a token

Create a token in the service (a GitHub personal-access token, a Slack bot/user token), then pipe the
**value** in on stdin — never on argv, where it would leak into your shell history and the process
table — and mount the path:

```sh
printf %s "$GH_TOKEN"    | qfs account add github personal
printf %s "$SLACK_TOKEN" | qfs account add slack team

qfs connect /github --driver github --account personal   # → /github/…
qfs connect /slack  --driver slack  --account team       # → /slack/…
```

qfs stores the token encrypted and never prints it back. `qfs account list` shows the account
label only.

## Amazon S3 & Cloudflare R2 — access keys

S3 and R2 sign each request with an access-key pair. The **secret** access key is sealed with
`qfs account add objstore <label>` (piped on stdin); its non-secret partner (the access-key id) and
the region / endpoint / bucket are configured alongside it — see
[Connections & credentials](/guide/connections):

```sh
printf %s "$AWS_SECRET_ACCESS_KEY" | qfs account add objstore prod
printf %s "$R2_SECRET_ACCESS_KEY"  | qfs account add objstore backups

qfs connect /s3 --driver s3 --account prod      # → /s3/…
qfs connect /r2 --driver r2 --account backups   # → /r2/…
```

## After connecting

```sh
qfs connect --list             # the paths you've defined (metadata only, never secrets)
qfs run "/mail/inbox |> where subject LIKE '%invoice%' |> select date, subject"
```

`describe` and `preview` never need any of this — they're always offline. A connect is only
required to **read rows** from a source or **commit** a change to it.

See [Connections & credentials](/guide/connections) for the full model: the account lifecycle,
rotating and revoking secrets, re-keying the vault, and how the vault is encrypted.
