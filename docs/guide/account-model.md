# The account model

"Account" usually means one tangled thing: your login, your permissions, and the service it's for.
qfs deliberately **splits it into separate layers** so each is small, inspectable, and least-privilege.
This page is the map of how those layers fit together — read it once and every `signup` / `connection
add` / `connect` command has an obvious place.

## The four layers at a glance

```text
   ┌───────────────────────────────────────────────────────────────┐
   │  OPERATOR IDENTITY    — who qfs acts as on THIS machine        │
   │  `qfs identity signup <email>`              (System DB · users)│
   └───────────────┬───────────────────────────────────────────────┘
                   │ owns / is accountable for
                   ▼
   ┌───────────────────────────────────────────────────────────────┐
   │  CONNECTION           — a named route:  <driver>/<name>        │
   │  gmail/default   github/work   s3/backups   sql/orders …       │
   └───────┬───────────────────────────────────────┬───────────────┘
           │ credentialed (holds a secret)         │ declared (no secret)
           ▼                                       ▼
 ┌──────────────────────────────┐    ┌──────────────────────────────┐
 │ CREDENTIAL STORE             │    │ a local source location       │
 │ envelope-encrypted SQLite,   │    │ a SQLite file / a git repo    │
 │ unlocked by QFS_PASSPHRASE   │    │ (`CREATE CONNECTION … AT …`)  │
 └───────────────┬──────────────┘    └──────────────────────────────┘
                 │ authenticates to
                 ▼
   ┌───────────────────────────────────────────────────────────────┐
   │  SERVICE ACCOUNT      — the EXTERNAL account                   │
   │  your Google / GitHub / Slack / AWS account                   │
   └───────────────────────────────────────────────────────────────┘
```

| Layer | What it is | Where it lives | Set up with |
| ----- | ---------- | -------------- | ----------- |
| **Operator identity** | *Who you are* on this host | System DB (`users`) | [`qfs identity signup`](/guide/operator) |
| **Connection** | A *named route* `<driver>/<name>` to a source | Project DB (`active_account`, ownership rows) | `qfs connection add` / `CREATE CONNECTION` |
| **Credential store** | The *sealed secrets* connections use | Project DB, envelope-encrypted | [`QFS_PASSPHRASE`](/guide/passphrase) |
| **Service account** | The *external account* a credential belongs to | The provider (Google, GitHub…) | OAuth consent / access key |

The key idea: **these are independent.** Your operator identity is not your Google account. A
connection is not the credential — it's a *name* that points at one. The passphrase unlocks the
*store*, not any single service. Keeping them apart is what lets one machine hold several accounts,
one consent serve several drivers, and a team share a connection without sharing a password.

## 1. Operator identity — *who* qfs acts as

A local, per-machine identity (`qfs identity signup <email>`). It authenticates *you* as the operator
on this host; today qfs is single-operator (one identity ⇒ that's you, no session needed). It is
**not** a qfs.com account and **not** the service's account. Full detail:
**[The operator identity](/guide/operator)**.

## 2. Connection — a *named route* to a source

A connection is `<driver>/<name>` — the `<name>` is the segment you see in a path. Two kinds:

- **Declared (no secret):** a local SQLite file or git repo *is* its location, so you just declare
  it — `CREATE CONNECTION orders DRIVER sqlite AT '/data/orders.db'` → `/sql/orders/<table>`. Nothing
  is stored in the credential store; no passphrase needed.
- **Credentialed (holds a secret):** mail, Drive, GitHub, Slack, S3/R2, a remote database. `qfs
  connection add <driver> <name>` seals a secret into the credential store under the key
  `<driver>/<name>`.

**Many accounts, side by side.** A driver can have several connections — `gmail/work` and
`gmail/personal`, `github/oss` and `github/dayjob`. One is the **active** selection per driver:

```sh
qfs connection list                 # every connection (names only — never secrets)
qfs connection use gmail personal   # make `personal` the active gmail account
```

The operational how-to (add / list / use / remove / rekey) is
[Connections & credentials](/guide/connections).

## 3. Credential store — where the secrets live

Every credentialed connection's secret is sealed in an **envelope-encrypted SQLite store** on this
machine. A data-key (DEK) seals each value; that DEK is wrapped by a key derived from your
**`QFS_PASSPHRASE`**. So the passphrase gates *the store*, once, not each service — unlock it and
every connection inside becomes usable. The passphrase itself is never stored. See
**[The QFS passphrase](/guide/passphrase)**.

## 4. Service account — the *external* side, and how one consent serves many drivers

A credential authenticates to an **external account**. The mapping isn't always one-connection-to-one-
credential — Google is the clearest example:

```text
  one Google consent   ─▶   google:<email>:refresh_token   ─┬─▶  gmail   (/mail)
  (sign in + approve)       (ONE token, stored once)        ├─▶  gdrive  (/drive)
                                                            └─▶  ga      (/ga)
```

One authorization for a Google **account** yields a single refresh token, stored once under
`google:<email>:refresh_token`, and **shared** by the Gmail, Drive, and Analytics drivers. The active
account is chosen by `QFS_GOOGLE_ACCOUNT` (the agent/CI override) or the active `google` connection
selection. Add a second account and you have two service accounts qfs can switch between — the
[Gmail cookbook](/cookbook/gmail#setup) walks the full flow.

## 5. The bind gate — sign-in + consent, before any secret is used

Binding a **cloud** credential (`gmail`, `gdrive`, `ga`, `github`, `slack`, S3/R2 `objstore`,
Cloudflare `cf`) is gated, fail-closed, on two things recorded against the operator identity:

1. **A signed-in operator** — cloud connections are refused for an anonymous one.
2. **Recorded consent** — the connection notes *who* granted it and for *what scope*.

Local SQL and git connections store no secret and pass no gate. This is why
[The operator identity](/guide/operator) is a prerequisite for every cloud cookbook, and why a cloud
`connection add` reports *requires sign-in* until you've signed up.

## 6. Ownership & teams — where this is heading

Every connection has an **owner scope**. Today the working model is **user-owned**: the connection is
yours, only you bind it. The larger design (roadmap **M5 / M9**) adds shared ownership, gated so a
secret is **never decrypted for an unauthorized actor**:

- **Project/team-owned connections** — a shared connection binds only if the acting member's
  actor-policy grants the connection's scope (default-deny). The gate runs *before* the secret is
  decrypted.
- **Brokered team connections** — managed qfs Cloud mints a team-scoped token; a non-member is
  refused with nothing stored.
- **High-sensitivity (end-to-end) connections** — the data-key is wrapped *per authorized recipient*
  and is **not** server-unwrappable, so even the host can't read the secret at rest.
- **Invites** (`qfs invite create` / `redeem`) grant **membership, not permission** — belonging to a
  host is separate from being authorized to use any given connection.

::: warning Today vs. planned
Single-operator, user-owned connections work now. The team, brokered, and end-to-end pieces are
wired against their models but partly reach a **live qfs Cloud network broker that isn't part of this
repo** — treat them as the direction, per the [roadmap](/roadmap), not a shipped feature.
:::

## Putting it together — connecting Gmail, layer by layer

```text
qfs identity signup you@example.com          # ① operator identity  — who you are
export/prompt QFS_PASSPHRASE                 # ② credential store    — unlock the vault
QFS_GOOGLE_CONSENT=1 \
  qfs connection add gmail default           # ③ service account     — consent → refresh token stored
qfs connect /mail --driver gmail             # ④ connection → path   — mount the route at /mail
qfs run "/mail/inbox |> select from, subject |> limit 5"
```

Each command lights up exactly one layer: identity, store, service account, connection. Once all four
are in place, `/mail` is just another path.

## See also

- [The QFS passphrase](/guide/passphrase) — the credential store's key, and how to provide it.
- [The operator identity](/guide/operator) — the sign-in gate in depth.
- [Connect a service](/guide/connect) — exact per-service steps.
- [Connections & credentials](/guide/connections) — add / list / use / remove / rotate / revoke.
