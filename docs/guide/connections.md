# Connections & credentials

A **connection** is what tells qfs *where a source lives and how to reach it*. In qfs the path that
reaches a source is one **you define**: a cloud path like `/mail` exists only after you **mount** it
with `qfs connect`, and a local path like `/sql/orders/customers` comes from a `CREATE CONNECTION`
declaration whose name (`orders`) is the path segment. That's why you can have `/sql/orders` and
`/sql/analytics`, or a work `/mail` and a home `/mail2`, side by side.

You don't need any connection to **describe** a path or **preview** a query — both are completely
offline. You need one to **read rows** from a source or **commit** a change to it.

::: tip Just want the steps for one service?
[Connect a service](/guide/connect) is the per-source how-to (Gmail/Drive, GitHub/Slack, S3/R2,
SQL/git). This page is the model behind it — the encrypted vault, the account verbs, and rotating or
revoking secrets. For how mounts relate to your operator identity and the external service
accounts, see [The account model](/guide/account-model).
:::

## Two kinds of connection

How you define a connection depends on whether the source needs a **secret**:

| Source | Defined by | Needs a secret? |
| --- | --- | --- |
| **Local databases & repos** — `/sql` (SQLite), `/git` | a **`CREATE CONNECTION` declaration** | no |
| **Credentialed services** — mail, Drive, GitHub, Slack, S3/R2 | `qfs account add` (vault) + `qfs connect` (mount) | yes |

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
name clash, overrides the declaration), but it is being retired. Run **`qfs connect --import-env`**
to print the equivalent `CREATE CONNECTION` lines and move them into a `connections.qfs`.
:::

### Credentialed services — accounts in the vault, mounted at a path

Mail, Drive, GitHub, Slack, S3/R2 reach an external account over a token/OAuth, so their credential
is split across two layers: the **service account** (`qfs account add`) seals the token into the
encrypted vault, and the **mount** (`qfs connect`) binds a path you choose to that
`(driver, account)` pair:

```sh
printf %s "$GH_TOKEN" | qfs account add github work        # seal the token in the vault
qfs connect /github --driver github --account work         # mount it — /github/… now exists
```

The mount carries the account — there is **no selection state** and nothing "active". Two GitHub
accounts are simply two mounts. The rest of this page is about that encrypted vault and the
account lifecycle.

## Unlocking the vault

The vault is created by **`qfs init`** (it walks you through choosing a passphrase — a password you
pick that encrypts the service logins you save on this machine; it is *not* a service credential
and you never register it anywhere). After that, any command that touches a sealed secret unlocks
the vault through one of its **key slots**:

- **Prompt / `QFS_PASSPHRASE`** — on a terminal qfs prompts (echo off); scripts export the env var:

  ```sh
  read -rs QFS_PASSPHRASE; export QFS_PASSPHRASE   # typed value isn't echoed or saved to history
  ```

- **OS keychain slot** — `qfs vault enroll keychain` stores a wrap of the vault key in the
  platform secret service, so this host unlocks with **no passphrase at all** from then on.

All the options and their trade-offs are in **[The QFS passphrase](/guide/passphrase)**. Either way
this is **at-rest confidentiality only**: it protects the stored blob, not a live host.

## Storing a credential

```sh
qfs account add <provider> <label>
```

- `<provider>` is the cloud the account belongs to — `google`, `github`, `slack`, `objstore`
  (S3/R2), `cf` (local `/sql`/`/git` sources need no account — see above)
- `<label>` is your name for it — a Google email, or `work`, `personal`, `prod`, …

On a terminal, `qfs account add google` runs the **live paste-back browser consent** — open the
printed URL in your LOCAL browser, approve, and paste the redirect URL back; it works over plain
SSH (register your OAuth app first: `cat credentials.json | qfs app add google`). For every other provider — and for automation —
the credential **value is read from stdin**: pipe it in, never pass it on argv (argv is visible in
the process table and your shell history). qfs seals the secret and **never prints it back**. The
account *label* is just metadata (safe to show); the credential itself is write-only from your
shell's perspective.

```sh
printf %s "$REFRESH_TOKEN" | qfs account add google you@gmail.com
printf %s "$AWS_SECRET"    | qfs account add objstore prod
printf %s "$GH_TOKEN"      | qfs account add github personal
```

## Listing accounts and mounts

```sh
qfs account list           # every authorized account (labels + metadata only)
qfs app list               # registered OAuth apps (provider + created_at)
qfs connect --list         # the defined paths (mount → driver)
```

None of these ever prints a secret.

## Removing an account or a mount

```sh
qfs account remove github work     # deletes the token AND its consent record
qfs disconnect /github             # removes the defined path (idempotent; aliases cascade)
```

## Rotating and revoking

Offboarding and key hygiene are first-class. The new secret is read from **stdin**, never argv:

```sh
printf %s "$NEW" | qfs account rotate github work   # re-mint the secret in place, clear any revoke
qfs account revoke github work                      # mark the account unresolvable (fails closed)
```

- **rotate** replaces an account's secret (the offboarding answer — *replace*, not un-grant) and
  clears any prior revocation. Other accounts are untouched.
- **revoke** marks one account unresolvable: a later bind fails closed and the secret is never
  returned. Other accounts keep working.

Re-wrapping the whole vault under a new passphrase is a **vault** operation —
`printf %s "$NEWPASS" | qfs vault rekey` — covered in
[The QFS passphrase](/guide/passphrase#rotating-the-passphrase).

::: tip Where the vault lives
Sealed credentials are **envelope-encrypted at rest** in qfs's SQLite store: a random data-key
encrypts each secret value, and that data-key is wrapped once per key slot (the passphrase slot uses
an argon2id-derived key). The `/sys/connections` admin path shows the account registry — provider,
label, and `created_at` only — and `/sys/paths` shows the mounts; there is structurally no column a
secret could ride in.
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
