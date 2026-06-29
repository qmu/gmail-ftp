# Interactive shell

Run `qfs` with no subcommand and you get an FTP-like interactive shell. Because every service is a
filesystem of paths, you navigate them all — mail, Drive, S3, databases — with the commands you
already know.

```sh
qfs
```

## Filesystem commands

Inside the shell you have a **current directory**, so paths can be relative (unlike one-shot `qfs
run`, which is always absolute). Each command is just shorthand for a pipe-SQL statement:

| Command | Does | Equivalent statement |
| --- | --- | --- |
| `ls [path]` | List a namespace | `SELECT` over the listing |
| `cd <path>` | Change current directory | — |
| `pwd` | Print current directory | — |
| `cat <path>` | Read a file or rows | `<path>` |
| `cp <src> <dst>` | Copy (across services too) | `UPSERT INTO <dst> <src>` |
| `mv <src> <dst>` | Move | copy then remove |
| `rm <path>` | Delete | `REMOVE <path>` |
| `describe <path>` | Inspect a path | `qfs describe` |

Because `cp` is just an `UPSERT INTO <dst> <src>`, copying *between services* works exactly like copying
within one:

```text
qfs:/> cd /drive/my/Reports
qfs:/drive/my/Reports> ls
qfs:/drive/my/Reports> cp /local/q3.pdf .
qfs:/drive/my/Reports> cat /sql/pg/orders
```

## Preview and commit, the same as everywhere

The shell follows the same safety model as the CLI. You can switch the session between previewing
and committing:

- **`preview`** — plan only; show effects without applying (the default, and always safe).
- **`commit`** — apply the effects you run.

So you can explore freely in preview mode, then flip to commit when you're sure. Irreversible
actions still announce themselves so you never trash or send something by accident.

## When to use the shell vs. `qfs run`

- **Shell** — interactive exploration: poke around your services, list folders, read a few rows,
  try a copy. Relative paths and a current directory make this comfortable.
- **`qfs run`** — scripting and automation: one statement, absolute paths, preview-by-default,
  composes with pipes and `jq`. This is what you reach for in scripts and what an AI agent uses.

Both speak the exact same language underneath.
