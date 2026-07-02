# CLI reference

`qfs` is one binary with a handful of subcommands. With **no** subcommand it starts the
[interactive shell](/guide/shell).

```text
qfs [OPTIONS] [COMMAND]

Commands:
  run        Run one statement and exit (preview by default)
  describe   Describe a path: archetype, columns, verbs, procedures, pushdown
  skill      Print the embedded AI operating procedure
  serve      Start the server (CLI + MCP endpoint + web dashboard) from a .qfs config
  init       Ready this machine: create the encrypted vault + register the operator
  connect    Bind a defined path to a driver + account (the CLI twin of CONNECT)
  disconnect Remove a defined path (idempotent)
  app        Manage OAuth app registrations (today: Google's credentials.json)
  account    Manage service accounts: authorize, list, remove, rotate, revoke
  vault      Manage the vault's key slots: slots, enroll, revoke, rekey
  host       Manage the qfs hosts this CLI can act on (`local` is implicit)
  identity   Local identity: look yourself up (signing up is `qfs init`)
  invite     Team invites & membership: create, redeem, revoke
  job        Run / schedule a saved JOB (an external scheduler drives it)
  help       Print help for any command

Global options:
  --json        Machine-readable JSON instead of the human table
  -h, --help    Help
  -V, --version Version (with build details)
```

## `qfs run` — execute one statement

```sh
qfs run "<statement>"        # positional
qfs run -e "<statement>"     # the -e form
echo "<statement>" | qfs run -   # read from stdin
```

**Previews by default** — it plans and shows the effects but changes nothing.

| Flag | Meaning |
| --- | --- |
| `--commit` | Apply the plan (a trailing `COMMIT` keyword does the same) |
| `--commit-irreversible` | Required to apply an irreversible effect (send, merge, delete) in a one-shot |
| `--format json\|table` | Force output format (default: table on a terminal, JSON when piped) |
| `--json` | Shorthand for `--format json` |
| `-q, --quiet` | Suppress progress output (never suppresses errors) |

```sh
# Preview, then commit:
qfs run "insert into /mail/drafts values ('alice@example.com','Hi','Body')"
qfs run "insert into /mail/drafts values ('alice@example.com','Hi','Body')" --commit

# Irreversible needs the extra ack (this CALL needs a connected mail account first —
# see `qfs connect`; without one it returns a capability error):
qfs run "/mail/drafts |> call mail.send" --commit --commit-irreversible
```

## `qfs describe` — inspect a path

```sh
qfs describe <path>
qfs describe <path> --json | jq .verbs
```

Completely **offline and credential-free**. It returns the node's archetype, columns (name, type,
nullability), supported verbs, `CALL` procedures (with which are irreversible), prelude aliases, and
which filters push down to the service. This is the first thing to run against any unfamiliar path.

## `qfs init` — ready this machine

The first-run wizard: creates the encrypted vault (walking you through choosing its passphrase —
the passphrase key slot is enrolled automatically) and registers the **operator identity**. There
is no password — your OS login is the authentication; the email is an accountability label.
Idempotent: re-running reports what exists.

```sh
qfs init you@example.com     # or omit the email on a terminal to be prompted
```

## `qfs connect` / `qfs disconnect` — defined paths

A cloud path exists only after a connect. `connect` binds a path you choose to a driver plus the
account it uses (the mount carries the account — there is no selection state); `disconnect`
removes it. The CLI twin of the `CONNECT` / `DISCONNECT` statements:

```sh
qfs connect /mail --driver gmail --account you@gmail.com   # mount Gmail at /mail
qfs connect /db --driver sqlite --at 'file:app.db'         # local source — no account
qfs disconnect /mail                                       # remove the defined path (idempotent)
qfs connect --list                                         # list the defined paths
qfs connect --import-env    # print CREATE CONNECTION declarations for QFS_SQL_*/QFS_GIT_* env vars
```

## `qfs app` — OAuth app registrations

The client credentials **your** OAuth app authenticates with (today: Google's `credentials.json`).
Read from stdin, never printed back:

```sh
cat credentials.json | qfs app add google
qfs app list             # provider + created_at — never a secret
qfs app remove google    # account tokens stay
```

## `qfs account` — service accounts

Authorize an external account (providers: `google`, `github`, `slack`, `objstore`, `cf`). On a
terminal `qfs account add google` runs the live paste-back browser consent — open the printed URL
in your **local** browser, approve, and paste the `http://localhost/...` redirect URL back (works
over plain SSH; no listener, no port-forward); automation pipes the token on **stdin**, never
argv:

```sh
qfs account add google                                        # paste-back browser consent on a TTY
printf %s "$REFRESH_TOKEN" | qfs account add google you@gmail.com   # automation; email = label
printf %s "$GH_TOKEN" | qfs account add github work
qfs account list                          # labels + metadata only, never tokens
qfs account remove <provider> <label>     # delete the token AND its consent record
```

For offboarding and key hygiene, an account can be **rotated** or **revoked** (the new secret is
read from stdin, never argv):

```sh
printf %s "$NEW" | qfs account rotate <provider> <label>   # re-mint the secret, clear any revoke
qfs account revoke <provider> <label>                      # mark unresolvable (fails closed at bind)
```

## `qfs vault` — key slots

The vault's data-key is wrapped once per **key slot** (KeyGuardian). The passphrase slot is
enrolled by `qfs init`; enroll the OS keychain and this host unlocks with no passphrase at all:

```sh
qfs vault slots                          # id, guardian kind, created_at — never key bytes
qfs vault enroll keychain                # OS keychain slot — no passphrase per pane thereafter
qfs vault revoke <slot>                  # the last remaining slot is refused
printf %s "$NEWPASS" | qfs vault rekey   # re-wrap the data-key under a new passphrase
```

## `qfs identity` — who you are

Authentication only — the operator is an identity, not an authorization (that's policies and
the ACL). Signing up is part of [`qfs init`](#qfs-init-ready-this-machine):

```sh
qfs identity whoami [a@b.com]   # print a user's email + id
```

## `qfs invite` — teams & membership

An operator mints a one-time, expiring invite; the invitee redeems it to create their local
identity and join. The token is shown **once** at create (store it then); redeem is single-use.

```sh
qfs invite create --scope host --role member --ttl 86400   # prints the one-time URL/token once
printf %s "$PW" | qfs invite redeem <token> a@b.com         # create the user + membership
qfs invite revoke <id>                                      # cancel a still-pending invite
```

## `qfs job` — run a saved JOB

**qfs is not a scheduler.** A `CREATE JOB … EVERY … DO …` row is a *saved named plan plus its
intended cadence*; an external scheduler (OS `cron` / Cloudflare Cron Triggers) owns the *when*.
`run` previews by default and applies through the same policy + irreversible gates as `qfs run`:

```sh
qfs job run app.qfs nightly --commit      # invoke the saved plan once (the scheduler's entrypoint)
qfs job cron app.qfs nightly              # emit the crontab line for the host crontab
```

## `qfs skill` — the embedded AI procedure

Prints the operating procedure an AI agent follows, straight from the binary:

```sh
qfs skill                # the procedure
qfs skill --examples     # plus one worked example per service
```

## `qfs serve` — run the server

Starts the server from a `.qfs` config file containing `CREATE …` bindings (triggers, jobs,
endpoints, views, policies). The one process presents the same engine as **three faces**: the HTTP
API, the **MCP endpoint** an AI agent connects to, and the **embedded web dashboard** whose approval
cards let a human review and approve a pending irreversible commit.

```sh
qfs serve ./myserver.qfs
```

See the [Server guide](/server) for the binding forms.

## `qfs --version`

The long form prints the version, the exact build commit, and the target it was built for — handy
when reporting an issue:

```text
qfs 0.0.14
commit:  <git-sha>
target:  x86_64-unknown-linux-gnu
wasm32:  false
```
