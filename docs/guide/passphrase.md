# The QFS passphrase — unlock your credential store

**Do this once, before any third-party service.** Reading `/local`, `/sys`, a local SQLite file, or
a local git repo needs nothing. But the moment you connect a service that stores a login — Gmail,
Google Drive, GitHub, Slack, S3, R2, a remote database — qfs keeps that credential in an
**envelope-encrypted store on this machine**, and the key that unlocks it is your **`QFS_PASSPHRASE`**.

::: tip This is the gate for every service cookbook
Every third-party cookbook (Gmail, Drive, GitHub, Slack, files/object storage, remote databases,
cross-service, automation) assumes the store is unlocked. If a command reports
*`QFS_PASSPHRASE is not set`*, come back here first — you can't `connection add` or read a connected
service without it. A **cloud** service also needs a signed-in operator — see the companion step,
**[The operator identity](/guide/operator)**.
:::

## What it is (and is not)

`QFS_PASSPHRASE` is **a password you choose** that encrypts the service logins you save on this host.

- It is **not** any service's own password. It locks the local file your saved tokens live in.
- qfs derives a key from it (argon2id over a per-store salt) and seals every stored secret under a
  data-key wrapped by that key. The passphrase itself is **never stored** — so if you forget it, the
  stored logins can't be recovered (you re-add them under a new passphrase).
- It protects the credential blob **at rest**. It is not a live-host guard: whoever can run `qfs`
  with the passphrase available can use the connections.

## How to provide it — realistic options

Pick the one that matches how much convenience vs. exposure you want. They differ only in **where the
passphrase lives** and **how long**.

### 1. Interactive prompt — zero setup (default)

Run any `qfs` command that needs the store on a terminal and, if `QFS_PASSPHRASE` isn't set, qfs
**asks for it** (echo off). On the very first run it walks you through creating the store (typed
twice); after that it just unlocks.

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

### 4. Your OS keychain or your own secrets manager

Keep the passphrase in the OS keychain (macOS Keychain, Linux `libsecret`) or a manager you already
run (1Password CLI, `pass`, Vault, cloud secret manager), and fetch it into the environment at shell
start:

```sh
export QFS_PASSPHRASE="$(security find-generic-password -s qfs -w)"   # macOS Keychain, for example
export QFS_PASSPHRASE="$(pass show qfs/passphrase)"                    # or pass, 1Password, Vault, …
```

The passphrase rests in **your** vault (encrypted, unlocked with your login), and every pane picks it
up. This is the recommended path today if you want "type it once per login."

### 5. Managed qfs (planned)

The managed qfs service will remove this step entirely: it **generates a strong passphrase for you
and keeps it in secure key storage**, so connections just work across your shells and machines with
no passphrase to hold. Until then, options 1–4 are the local-only story, and option 4 is the closest
to that experience.

## Rotating the passphrase

You can re-wrap the store's data-key under a **new** passphrase without re-adding a single connection
— the current passphrase must be set, the new one is read from stdin:

```sh
printf %s "$NEW_PASSPHRASE" | qfs connection rekey   # old passphrase stops unlocking; logins survive
```

See [The account model](/guide/account-model) for how the passphrase, the credential store,
connections, and service accounts fit together; [Connections & credentials](/guide/connections) for
the full operational model (rotating and revoking individual secrets); and
[Connect a service](/guide/connect) for the exact per-service `connection add` steps once the store
is unlocked.
