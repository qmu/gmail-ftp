# The operator identity — who qfs acts as

**Do this once, before any cloud service.** Local sources (`/local`, `/sys`, a SQLite file, a git
repo) — and even a remote SQL database — need no identity. But a **cloud** service (Gmail, Google
Drive, Google Analytics, GitHub, Slack, S3/R2, Cloudflare) requires a **signed-in operator**: qfs
fails closed for an anonymous one, so `connection add` is refused until you've registered an identity.

::: tip The second setup gate (alongside the passphrase)
Two one-time steps gate a cloud connection. Your **[QFS passphrase](/guide/passphrase)** unlocks the
local secret store; the **operator identity** is *who* qfs acts as when it saves and uses the
credential inside that store. Different jobs — you need both for a cloud service.
:::

## What it is (and is not)

`qfs identity signup <email>` registers a **local operator identity** on this machine (in qfs's
System DB).

- **Local and per-machine.** It is *not* a qfs.com account, and *not* the service's own account.
  Your Google / GitHub / Slack login is a separate thing — that's the OAuth **consent** step when you
  add the connection. The operator identity is simply the name qfs records as the operator on this
  host, and the email you sign up with can be any address you want to identify yourself by.
- **Authentication, not authorization (today).** It records *who you are*; it does not yet grant or
  restrict *what you may do*. qfs is single-operator right now: if exactly one identity exists on the
  host, that's the operator and no login/session is needed. Multi-user sessions and per-operator
  permissions are on the [roadmap](/roadmap).
- **Password-protected, hashed at rest.** The password is hashed with argon2id; the plaintext is
  never stored or printed. On a terminal qfs prompts for it (echo off, entered twice); automation
  pipes it on stdin.

## Sign up and check

```sh
qfs identity signup you@example.com    # prompts for a password on a terminal (entered twice)
qfs identity whoami                    # shows the current operator
```

- `signup` creates the identity — run it once per machine.
- `whoami` prints the sole operator, or looks one up: `qfs identity whoami you@example.com`.

For agents and CI, pipe the password on stdin instead of being prompted (never on argv, where it
would leak into the process table and shell history):

```sh
printf %s "$PASSWORD" | qfs identity signup you@example.com
```

## Why cloud services require it

Adding a cloud connection binds a real credential — an OAuth refresh token, an access-key pair — that
can act on an external account. qfs refuses to store or use one for an **anonymous** operator: the
connection is recorded against, and its consent granted by, a named identity, so there is always an
accountable *who* behind a bound credential. If a cloud `connection add` reports *requires sign-in*,
run `qfs identity signup <email>` first.

Local SQL and git connections store no such credential and need **no** identity. Only drivers that
talk to an external service over OAuth or keys do: `gmail`, `gdrive`, `ga`, `github`, `slack`,
object storage (`s3` / `r2`), and Cloudflare.

## Teams (later)

The single operator is the foundation for teams: invites (`qfs invite create` / `redeem`) turn more
people into identities that share connections, and per-operator authorization arrives with that. See
the [roadmap](/roadmap). Today, one operator per host is the model.

See **[The QFS passphrase](/guide/passphrase)** for the other setup gate,
**[The account model](/guide/account-model)** for how the operator identity, connections, and service
accounts fit together, and **[Connect a service](/guide/connect)** for the exact per-service steps
once you're signed in.
