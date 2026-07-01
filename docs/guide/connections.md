# Connections & credentials

A **connection** is a named configuration that tells qfs *where a source lives and how to reach it*.
In a path like `/sql/orders/customers` or `/s3/backups/db.sql`, the **second segment is the
connection name** (`orders`, `backups`) — not a literal host or bucket, but a label you defined that
holds the actual connection info. That's why you can have `/sql/orders` and `/sql/analytics`, or
`/mail/work` and `/mail/personal`, side by side.

You don't need any connection to **describe** a path or **preview** a query — both are completely
offline. You need one to **read rows** from a source or **commit** a change to it.

::: tip Just want the steps for one service?
[Connect a service](/guide/connect) is the per-source how-to (Gmail/Drive, GitHub/Slack, S3/R2,
SQL/git). This page is the model behind it — the encrypted store, the passphrase, and rotating or
revoking secrets.
:::

## Two kinds of connection

How you define a connection depends on whether the source needs a **secret**:

| Source | Defined by | Needs a secret? |
| --- | --- | --- |
| **Local databases & repos** — `/sql` (SQLite), `/git` | a **`CREATE CONNECTION` declaration** | no |
| **Credentialed services** — `/mail`, `/drive`, `/github`, `/slack`, `/s3`, `/r2` | `qfs connection add` (encrypted store) | yes |

### Local databases & git — declare a connection

A SQLite database or a git repository is just a local path (or URL), so the connection *is* that
location — no stored secret, no passphrase. You **declare** it with a `CREATE CONNECTION` statement
in a `connections.qfs` file; the name you give it is the `<conn>` path segment:

```text
CREATE CONNECTION orders DRIVER sqlite AT '/data/orders.db';    -- → /sql/orders/<table>
CREATE CONNECTION app    DRIVER git    AT '/srv/repos/app.git'; -- → /git/app/commits, /git/app@<ref>/…
```

Point qfs at the file with `QFS_CONNECTIONS=/path/to/connections.qfs` (or the default
`~/.config/qfs/connections.qfs`); a `/sql/orders/…` query then works, and fails closed
(`unknown source 'sql'`) when no connection of that name is declared. The declaration is the source
of truth — reviewable and committable — instead of a setting hidden in an env var's name.

::: warning The `QFS_SQL_*` / `QFS_GIT_*` env vars are deprecated
`export QFS_SQL_ORDERS=/data/orders.db` still works as a temporary fallback (it warns once and, on a
name clash, overrides the declaration), but it is being retired. Run **`qfs connection import-env`**
to print the equivalent `CREATE CONNECTION` lines and move them into a `connections.qfs`.
:::

### Credentialed services — the credential store

`/mail`, `/drive`, `/github`, `/slack`, `/s3`, `/r2` reach an external account over a token/OAuth, so
their connection carries a **secret** that must be stored encrypted (`qfs connection add`, below).
The path segment is still the connection name (`qfs connection add s3 prod` → `/s3/prod/…`). The
rest of this page is about that encrypted store.

## Unlocking the store with `QFS_PASSPHRASE`

`QFS_PASSPHRASE` is **a password you choose that encrypts the service logins you save on this
machine**. It is *not* a service credential — not your mail token, not an API key — and you never
register it anywhere: it only locks and unlocks the local file your saved logins live in. You pick
it once and reuse it; everything else is handled for you. (How that encryption actually works is in
the *“Where the store lives”* note further down, for those who want the detail.)

`connection add`, `connection list`, and `connection remove` all need it exported in the shell that
runs them (`connection use` does not — it only flips which stored connection is active). With it
unset, those commands fail closed with a clear error.

Set it without leaking it into your shell history:

```sh
read -rs QFS_PASSPHRASE; export QFS_PASSPHRASE   # typed value isn't echoed or saved to history
```

Alternatives: source a `0600`-permissioned file you keep outside the repo, or (with
`HISTCONTROL=ignorespace`) prefix the `export` with a leading space. Avoid a bare
`export QFS_PASSPHRASE=secret` typed inline — it lands in your history.

This is **at-rest confidentiality only**: a process or someone with access to your running shell can
read `QFS_PASSPHRASE` straight out of the environment. It protects the stored blob, not a live host.

## Storing a credential

```sh
qfs connection add <service> <name>
```

- `<service>` is the driver the connection belongs to — `mail`, `drive`, `github`, `slack`, `s3`,
  `r2` (the credentialed services; local `/sql`/`/git` use an env var instead — see above)
- `<name>` is your label for it — `work`, `personal`, `prod`, …

The credential **value is read from stdin** — pipe it in, never pass it on argv (argv is visible in
the process table and your shell history). qfs stores the secret securely and **never prints it
back**. The connection *name* is just metadata (safe to show); the credential itself is write-only
from your shell's perspective.

```sh
# QFS_PASSPHRASE must already be exported (see above).
printf %s "$MAIL_TOKEN"  | qfs connection add mail work
printf %s "$AWS_SECRET"  | qfs connection add s3 prod
printf %s "$GH_TOKEN"    | qfs connection add github personal
```

## Listing connections

```sh
qfs connection list            # all services
qfs connection list mail       # just one service
```

This prints **names and metadata only** — never a secret.

## Choosing the active connection

A service can have several connections (e.g. `work` and `personal` mail). Set which one is active:

```sh
qfs connection use mail work
```

## Removing a connection

```sh
qfs connection remove mail work     # idempotent — fine to run twice
```

## Rotating, revoking, and rekeying

Offboarding and key hygiene are first-class. The new secret (or passphrase) is read from **stdin**,
never argv:

```sh
printf %s "$NEW" | qfs connection rotate mail work   # re-mint the secret in place, clear any revoke
qfs connection revoke mail work                      # mark the connection unresolvable (fails closed)
printf %s "$NEWPASS" | qfs connection rekey          # re-wrap the store's data-key under a new passphrase
```

- **rotate** replaces a connection's secret (the offboarding answer — *replace*, not un-grant) and
  clears any prior revocation. Other connections are untouched.
- **revoke** marks one connection unresolvable: a later bind fails closed and the secret is never
  returned. Other connections keep working.
- **rekey** re-wraps the store's data-key under a new `QFS_PASSPHRASE` (the current one is the old
  one, read from the environment; the new one from stdin). Existing secrets stay decryptable; the
  old passphrase stops unlocking. It is one re-wrap of the data-key, not an N-way re-encryption.

::: tip Where the store lives
Stored credentials are **envelope-encrypted at rest** in qfs's SQLite store: a random data-key
encrypts each secret value, and that data-key is itself wrapped under an argon2id key derived from
`QFS_PASSPHRASE`. The `/sys/connections` admin path shows the registry — driver, connection name,
and `created_at` only; there is structurally no column a secret could ride in.
:::

## Least privilege

qfs is built so a credential is only ever used for the exact plan you commit, and never appears in
output, logs, or a `describe` report. On the **server**, you can go further and attach a
[`POLICY`](/server) that allows only specific verbs on specific paths — so an automation or an AI
agent can act, but only within the bounds you set:

```qfs
create policy api ALLOW select DENY INSERT, update, remove, call
create policy uploads ALLOW UPSERT on 's3/*'
```

The guiding rule: grant the **minimum** a task needs, and let `preview` confirm a plan stays inside
those bounds before you commit.
