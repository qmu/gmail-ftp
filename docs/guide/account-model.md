# The account model

"Account" usually means one tangled thing: your login, your permissions, and the service it's for.
qfs deliberately **splits it into separate layers** so each is small, inspectable, and least-privilege.
This page is the map of how those layers fit together — read it once and every `init` / `app add` /
`account add` / `connect` command has an obvious place.

## The four layers at a glance

```text
   ┌───────────────────────────────────────────────────────────────┐
   │  OPERATOR IDENTITY    — who qfs acts as on THIS machine        │
   │  `qfs init <email>`                         (System DB · users)│
   └───────────────┬───────────────────────────────────────────────┘
                   │ owns / is accountable for
                   ▼
   ┌───────────────────────────────────────────────────────────────┐
   │  MOUNT (defined path) — a path YOU bind:  /mail  /mail2  /db   │
   │  `qfs connect /mail --driver gmail --account you@gmail.com`    │
   └───────┬───────────────────────────────────────┬───────────────┘
           │ carries an account (cloud)            │ carries a location (local)
           ▼                                       ▼
 ┌──────────────────────────────┐    ┌──────────────────────────────┐
 │ VAULT                        │    │ a local source location       │
 │ envelope-encrypted SQLite,   │    │ a SQLite file / a git repo    │
 │ unlocked by a key slot       │    │ (`CREATE CONNECTION … AT …`)  │
 └───────────────┬──────────────┘    └──────────────────────────────┘
                 │ seals the token of
                 ▼
   ┌───────────────────────────────────────────────────────────────┐
   │  SERVICE ACCOUNT      — the EXTERNAL account                   │
   │  your Google / GitHub / Slack / AWS account                   │
   └───────────────────────────────────────────────────────────────┘
```

| Layer | What it is | Where it lives | Set up with |
| ----- | ---------- | -------------- | ----------- |
| **Operator identity** | *Who you are* on this host | System DB (`users`) | [`qfs init`](/guide/operator) |
| **Service account** | The *external account* qfs authorizes | Vault (sealed token) + consent record | `qfs account add` (Google first needs `qfs app add google`) |
| **Vault** | The *sealed store* the tokens live in | Project DB, envelope-encrypted | [`qfs init` / `qfs vault`](/guide/passphrase) |
| **Mount** | A *defined path* bound to a driver + account | Project DB (`path_binding`) | `qfs connect` / `CONNECT` statement |

The key idea: **these are independent.** Your operator identity is not your Google account. A mount
is not the credential — it's a *path* that names one account of one driver. The vault key unlocks the
*store*, not any single service. Keeping them apart is what lets one machine hold several accounts,
one consent serve several drivers, and two accounts of the same service coexist as two paths.

## 1. Operator identity — *who* qfs acts as

A local, per-machine identity (`qfs init <email>`). There is no password: your **OS login is the
authentication** — one operator per OS user, and the email is an accountability label. It is
**not** a qfs.com account and **not** the service's account. Full detail:
**[The operator identity](/guide/operator)**.

## 2. Service account — the *external* account, authorized once

A service account is `<provider>/<label>` — the external account qfs may act as, with its token
sealed in the vault and its consent recorded. For Google, register your OAuth app's client
credentials first (`cat credentials.json | qfs app add google`); then authorize:

```sh
qfs account add google                                    # live browser consent on a terminal
printf %s "$REFRESH_TOKEN" | qfs account add google you@gmail.com   # automation: token on stdin, email as the label
printf %s "$GH_TOKEN"      | qfs account add github work            # other clouds: a token + your label
```

**Many accounts, side by side.** A provider can have several accounts — `google/you@gmail.com` and
`google/home@gmail.com`, `github/oss` and `github/dayjob`. There is **no selection state**: nothing
is "active". Each account is used by the mounts that name it (§4).

```sh
qfs account list                        # every authorized account (labels only — never tokens)
qfs account remove google home@gmail.com
```

The operational how-to (add / list / remove / rotate / revoke) is
[Connections & credentials](/guide/connections).

## 3. Vault — where the secrets live

Every account's token is sealed in an **envelope-encrypted SQLite store** on this machine, created
by `qfs init`. A data-key (DEK) seals each value; that DEK is wrapped once per **key slot** — the
passphrase slot `qfs init` enrolls, and optionally an OS-keychain slot (`qfs vault enroll keychain`)
so this host unlocks with no passphrase at all. So a vault key gates *the store*, once, not each
service — unlock it and every account inside becomes usable. See
**[The QFS passphrase](/guide/passphrase)**.

## 4. Mount — a *defined path*, and how one consent serves many drivers

A cloud path exists **only after a connect**. The mount binds a path to a `(driver, account)` pair —
the mount carries the account, so two accounts of one driver are simply two mounts in the same
process:

```sh
qfs connect /mail  --driver gmail --account you@gmail.com    # work mail at /mail
qfs connect /mail2 --driver gmail --account home@gmail.com   # home mail at /mail2
qfs connect --list                                           # the defined paths
```

Local sources need no account — the location *is* the connection:
`CREATE CONNECTION orders DRIVER sqlite AT '/data/orders.db'` (or
`qfs run "CONNECT /db TO sqlite AT 'file:app.db'" --commit`). See
[Connect a service](/guide/connect).

The Google mapping isn't one-driver-to-one-token — one consent is shared:

```text
  one Google consent   ─▶   google:<email>:refresh_token   ─┬─▶  gmail   (e.g. /mail)
  (sign in + approve)       (ONE token, stored once)        ├─▶  gdrive  (e.g. /drive)
                                                            └─▶  ga      (e.g. /ga)
```

One authorization for a Google **account** yields a single refresh token, stored once under
`google:<email>:refresh_token`, and **shared** by the Gmail, Drive, and Analytics drivers — each
mount names which email it uses. `QFS_GOOGLE_ACCOUNT` is **only a CI/agent override** that pins the
Google account for one process; when it is unset (the normal case) the account always comes off the
mount. The [Gmail cookbook](/cookbook/gmail#setup) walks the full flow.

## 5. The bind gate — sign-in + consent, before any secret is used

Binding a **cloud** mount's credential (`gmail`, `gdrive`, `ga`, `github`, `slack`, S3/R2
`objstore`, Cloudflare `cf`) is gated, fail-closed, on two things checked for the **mount's**
`(driver, account)`:

1. **A signed-in operator** — cloud accounts are refused for an anonymous one.
2. **Recorded consent** — the account notes *who* granted it and for *what scope*.

Local SQL and git mounts store no secret and pass no gate. This is why
[The operator identity](/guide/operator) is a prerequisite for every cloud cookbook: a cloud
`account add` is refused until `qfs init` has run. A mount whose account is missing or unauthorized
fails closed with the fix spelled out — e.g. a mail mount with no usable account says: run
`qfs app add google`, `qfs account add google <email>`, then `qfs connect <path> gmail <email>`.

## 6. Ownership & teams — where this is heading

Every account has an **owner scope**. Today the working model is **user-owned**: the account is
yours, only your mounts bind it. The larger design (roadmap **M5 / M9**) adds shared ownership,
gated so a secret is **never decrypted for an unauthorized actor**:

- **Project/team-owned accounts** — a shared account binds only if the acting member's
  actor-policy grants its scope (default-deny). The gate runs *before* the secret is decrypted.
- **Brokered team accounts** — managed qfs Cloud mints a team-scoped token; a non-member is
  refused with nothing stored.
- **High-sensitivity (end-to-end) accounts** — the data-key is wrapped *per authorized recipient*
  and is **not** server-unwrappable, so even the host can't read the secret at rest.
- **Invites** (`qfs invite create` / `redeem`) grant **membership, not permission** — belonging to a
  host is separate from being authorized to use any given account.

::: warning Today vs. planned
Single-operator, user-owned accounts work now. The team, brokered, and end-to-end pieces are
wired against their models but partly reach a **live qfs Cloud network broker that isn't part of this
repo** — treat them as the direction, per the [roadmap](/roadmap), not a shipped feature. The
decided shape of that direction — the CLI as a client of **hosts** (local, self-hosted, managed),
per-layer verbs, and key-guardian vault slots — is
[ADR 0008](/adr/0008-multi-host-account-model).
:::

## Putting it together — connecting Gmail, layer by layer

```sh
qfs init you@example.com                         # ① operator identity + vault — who you are
cat credentials.json | qfs app add google        # ② OAuth app            — your Google client credentials
qfs account add google                           # ③ service account      — consent → refresh token sealed
qfs connect /mail --driver gmail --account you@gmail.com   # ④ mount — /mail now exists
qfs run "/mail/inbox |> select from, subject |> limit 5"
```

Each command lights up exactly one layer: identity, app, service account, mount. Once all four
are in place, `/mail` is just another path.

## See also

- [The QFS passphrase](/guide/passphrase) — the vault's key slots, and how to unlock the store.
- [The operator identity](/guide/operator) — the sign-in gate in depth.
- [Connect a service](/guide/connect) — exact per-service steps.
- [Connections & credentials](/guide/connections) — add / list / remove / rotate / revoke.
