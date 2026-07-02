# The operator identity — who qfs acts as

**`qfs init` does this once, before any cloud service.** Local sources (`/local`, `/sys`, a SQLite
file, a git repo) — and even a remote SQL database — need no identity. But a **cloud** service
(Gmail, Google Drive, Google Analytics, GitHub, Slack, S3/R2, Cloudflare) requires a **registered
operator**: qfs fails closed for an anonymous one, so `qfs account add` is refused until the
machine is readied.

::: tip One wizard covers both setup gates
`qfs init <email>` does two jobs in one run: it creates the **encrypted vault** (choosing its
[passphrase](/guide/passphrase)) and registers the **operator identity** — *who* qfs acts as when
it saves and uses the credentials inside that vault. Different jobs — a cloud service needs both.
:::

## What it is (and is not)

`qfs init <email>` registers a **local operator identity** on this machine (in qfs's System DB).

- **Local and per-machine.** It is *not* a qfs.com account, and *not* the service's own account.
  Your Google / GitHub / Slack login is a separate thing — that's the OAuth **consent** step when you
  authorize the account. The operator identity is simply the name qfs records as the operator on this
  host, and the email you register can be any address you want to identify yourself by.
- **No password — your OS login is the authentication.** qfs delegates authentication to the
  operating system: one operator per OS user, and whoever is logged into this OS user *is* that
  operator. The email is an **accountability label**, not a credential. qfs is single-operator right
  now; multi-user sessions and per-operator permissions are on the [roadmap](/roadmap).
- **Authentication, not authorization (today).** It records *who you are*; it does not yet grant or
  restrict *what you may do*.

## Ready the machine and check

```sh
qfs init you@example.com    # register the operator (and create the vault) — once per machine
qfs identity whoami         # shows the current operator
```

- `init` is idempotent — re-running reports what exists. Omit the email on a terminal to be
  prompted for it.
- `whoami` prints the sole operator, or looks one up: `qfs identity whoami you@example.com`.

## Why cloud services require it

Authorizing a cloud account binds a real credential — an OAuth refresh token, an access-key pair —
that can act on an external account. qfs refuses to store or use one for an **anonymous** operator:
the account is recorded against, and its consent granted by, a named identity, so there is always an
accountable *who* behind a bound credential. "Requires sign-in" here means exactly the OS
delegation above: an operator must have been registered on this OS user with `qfs init` — there is
no qfs password to enter. If a cloud `account add` reports it, run `qfs init <email>` first.

Local SQL and git sources store no such credential and need **no** identity. Only drivers that
talk to an external service over OAuth or keys do: `gmail`, `gdrive`, `ga`, `github`, `slack`,
object storage (`s3` / `r2`), and Cloudflare.

## Teams (later)

The single operator is the foundation for teams: invites (`qfs invite create` / `redeem`) turn more
people into identities that share accounts, and per-operator authorization arrives with that. See
the [roadmap](/roadmap). Today, one operator per host is the model.

See **[The QFS passphrase](/guide/passphrase)** for the vault half of `qfs init`,
**[The account model](/guide/account-model)** for how the operator identity, accounts, and mounts
fit together, and **[Connect a service](/guide/connect)** for the exact per-service steps once the
machine is readied.
