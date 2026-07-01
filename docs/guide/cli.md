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
  connection Manage stored credentials per service/connection
  identity   Local identity: sign up, look yourself up
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
# see `qfs connection`; without one it returns a capability error):
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

## `qfs connection` — credentials

Store and manage credentials per service. Names are metadata (safe to print); the secret is never
echoed. See [Connections & credentials](/guide/connections).

`add`, `list`, and `remove` need **`QFS_PASSPHRASE`** exported first — the master passphrase that
unlocks the encrypted local store (not a service credential). The credential **value is read from
stdin**, never argv:

```sh
read -rs QFS_PASSPHRASE; export QFS_PASSPHRASE     # unlock the store, no shell-history leak
printf %s "$TOKEN" | qfs connection add <service> <name>   # value via stdin (store/replace)
qfs connection list [<service>]         # list connection names only
qfs connection use <service> <name>     # set the active connection (no passphrase needed)
qfs connection remove <service> <name>  # delete (idempotent)
```

For offboarding and key hygiene, a connection can be **rotated**, **revoked**, or the whole store
**rekeyed** (the new secret / passphrase is read from stdin, never argv):

```sh
printf %s "$NEW" | qfs connection rotate <service> <name>  # re-mint the secret, clear any revoke
qfs connection revoke <service> <name>                     # mark unresolvable (fails closed at bind)
printf %s "$NEWPASS" | qfs connection rekey                # re-wrap the store under a new passphrase
```

## `qfs identity` — local sign-up

Authentication only — a signed-up user is an identity, not an authorization (that's policies and
the ACL). The password is read from **stdin**, never argv; the password hash is never printed.

```sh
printf %s "$PW" | qfs identity signup a@b.com   # create a local user + password account
qfs identity whoami [a@b.com]                    # print a user's email + id (never the hash)
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
qfs 0.0.10
commit:  <git-sha>
target:  x86_64-unknown-linux-gnu
wasm32:  false
```
