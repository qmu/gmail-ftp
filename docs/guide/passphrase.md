# The QFS passphrase — unlock your credential vault

**`qfs init` does this once, before any third-party service.** Reading `/local`, `/sys`, a local
SQLite file, or a local git repo needs nothing. But the moment you authorize an account whose login
qfs stores — Gmail, Google Drive, GitHub, Slack, S3, R2, a remote database — qfs keeps that
credential in an **envelope-encrypted vault on this machine**. `qfs init` creates the vault and
walks you through choosing its **passphrase**; that passphrase is the vault's first key slot.

::: tip This is the gate for every service cookbook
Every third-party cookbook (Gmail, Drive, GitHub, Slack, files/object storage, remote databases,
cross-service, automation) assumes the vault is unlocked. If a command reports
*`QFS_PASSPHRASE is not set`*, come back here first — you can't `qfs account add` or read a
connected service without unlocking. A **cloud** service also needs a registered operator — see the
companion step, **[The operator identity](/guide/operator)**.
:::

## What it is (and is not)

The passphrase is **a password you choose** that encrypts the service logins you save on this host.

- It is **not** any service's own password. It locks the local file your saved tokens live in.
- qfs derives a key from it (argon2id over a per-store salt) and seals every stored secret under a
  data-key wrapped by that key. The passphrase itself is **never stored** — it is one **key slot**
  among possibly several (see the keychain slot below); if every slot is lost, the stored logins
  can't be recovered (you re-add them under a new vault).
- It protects the credential blob **at rest**. It is not a live-host guard: whoever can run `qfs`
  with the vault unlockable can use the accounts.

## How to provide it — realistic options

Pick the one that matches how much convenience vs. exposure you want. They differ only in **where the
unlocking key lives** and **how long**.

### 1. Interactive prompt — zero setup (default)

Run any `qfs` command that needs the vault on a terminal and, if `QFS_PASSPHRASE` isn't set, qfs
**asks for it** (echo off). The vault itself is created by `qfs init` (typed twice); after that any
command just unlocks.

- The **interactive shell** (`qfs` with no arguments) asks **once per session** and reuses it for
  every command in that session — the recommended way to run several statements.
- A one-shot `qfs run "…"` is its own process, so it asks once for that command.

Nothing to store, nothing in your shell history. The trade-off: a **new shell / new tmux pane is a
new process**, so it prompts again there (a child process can't share the value back to your shell).

### 2. Export it for the shell session

```sh
read -rs QFS_PASSPHRASE; export QFS_PASSPHRASE   # typed value is not echoed or saved to history
```

Now every `qfs` command **in that shell** reuses it — good for scripting a batch of one-shots. Still
**per-shell**: a new tmux pane doesn't inherit the export, so you repeat it there. Avoid
`export QFS_PASSPHRASE=secret` typed inline — that lands in your shell history.

### 3. A `.env` file or shell profile — persistent, at your own risk

Sourcing the passphrase from a file (`.env`, `~/.zshrc`, a systemd `EnvironmentFile`, a CI secret)
makes it available to **every** new shell and pane automatically. That convenience means the
passphrase now sits **in plaintext at rest** in that file — you own that risk. If you do this, lock
the file down (`chmod 600`) and keep it out of any repo.

### 4. Enroll your OS keychain — no passphrase at all

The vault's data-key can be wrapped under more than one **key slot** (KeyGuardian). Enroll the OS
keychain (macOS Keychain, Linux secret service) as a second slot and this host unlocks the vault
with **no passphrase from then on** — no prompt per pane, nothing to export:

```sh
qfs vault enroll keychain    # unlocks via the platform secret service from now on
qfs vault slots              # list the slots: id, guardian kind, created_at
qfs vault revoke <slot>      # drop a slot (the last remaining slot is refused)
```

The key rests in **your** OS keychain (encrypted, unlocked with your OS login), and every pane picks
it up. This is the recommended path if you want "type it never."

### 5. Managed qfs (planned)

A managed key-guardian service would remove even the enrollment step across machines: a slot held in
managed secure key storage, so accounts just work on every host you own. That guardian kind is
**planned**, per the [roadmap](/roadmap) — today options 1–4 are the story, and the keychain slot
(option 4) is the shipped "no passphrase" experience.

## Rotating the passphrase

You can re-wrap the vault's data-key under a **new** passphrase without re-adding a single account
— the current passphrase must be set, the new one is read from stdin:

```sh
printf %s "$NEW_PASSPHRASE" | qfs vault rekey   # old passphrase stops unlocking; logins survive
```

See [The account model](/guide/account-model) for how the passphrase, the vault, accounts, and
mounts fit together; [Connections & credentials](/guide/connections) for the full operational model
(rotating and revoking individual secrets); and [Connect a service](/guide/connect) for the exact
per-service `account add` + `connect` steps once the vault exists.
