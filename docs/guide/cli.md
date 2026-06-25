# CLI reference

`qfs` is one binary with a handful of subcommands. With **no** subcommand it starts the
[interactive shell](/guide/shell).

```text
qfs [OPTIONS] [COMMAND]

Commands:
  run       Run one statement and exit (preview by default)
  describe  Describe a path: archetype, columns, verbs, procedures, pushdown
  account   Manage stored credentials per service/account
  skill     Print the embedded AI operating procedure
  serve     Start the server from a .qfs config file
  help      Print help for any command

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
qfs run "INSERT INTO /mail/drafts VALUES ('alice@example.com','Hi','Body')"
qfs run "INSERT INTO /mail/drafts VALUES ('alice@example.com','Hi','Body')" --commit

# Irreversible needs the extra ack:
qfs run "FROM /mail/drafts |> CALL mail.send" --commit --commit-irreversible
```

## `qfs describe` — inspect a path

```sh
qfs describe <path>
qfs describe <path> --json | jq .verbs
```

Completely **offline and credential-free**. It returns the node's archetype, columns (name, type,
nullability), supported verbs, `CALL` procedures (with which are irreversible), prelude aliases, and
which filters push down to the service. This is the first thing to run against any unfamiliar path.

## `qfs account` — credentials

Store and manage credentials per service. Names are metadata (safe to print); the secret is never
echoed. See [Accounts & credentials](/guide/accounts).

```sh
qfs account add <service> <name>     # store (or replace) a credential
qfs account list [<service>]         # list account names only
qfs account use <service> <name>     # set the active account for a service
qfs account remove <service> <name>  # delete (idempotent)
```

## `qfs skill` — the embedded AI procedure

Prints the operating procedure an AI agent follows, straight from the binary:

```sh
qfs skill                # the procedure
qfs skill --examples     # plus one worked example per service
```

## `qfs serve` — run the server

Starts the server from a `.qfs` config file containing `CREATE …` bindings (triggers, jobs,
endpoints, views, policies):

```sh
qfs serve ./myserver.qfs
```

See the [Server guide](/server) for the binding forms.

## `qfs --version`

The long form prints the version, the exact build commit, and the target it was built for — handy
when reporting an issue:

```text
qfs 0.0.4
commit:  <git-sha>
target:  x86_64-unknown-linux-gnu
```
