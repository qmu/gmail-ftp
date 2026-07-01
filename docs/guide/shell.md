# Interactive shell

Run `qfs` with no subcommand and you get an FTP-like interactive shell. The shell starts mounted at
`/local` — your machine's filesystem — so you navigate it with the commands you already know, and
every line runs through the exact same pipe-SQL engine as `qfs run`.

```sh
qfs
```

The prompt is `{driver}:{path}$`. A fresh session opens at the local root:

```text
local:/$
```

To leave the shell, press **Ctrl-D** (EOF). There is no `exit`/`quit` command — typing one is just
treated as a query and errors.

## Filesystem commands

Inside the shell you have a **current directory**, so paths can be relative (unlike one-shot `qfs
run`, which is always absolute). The builtins are:

| Command | Does | Notes |
| --- | --- | --- |
| `ls [path]` | List a directory | Reads rows; defaults to the current directory |
| `cd <path>` | Change current directory | Pure state change; updates the prompt |
| `pwd` | Print current directory | Prints the `/local`-prefixed path |
| `cat <path>` | Stat a path / read its rows | Lists stat rows (see below) |
| `cp <src> <dst>` | Copy | Lowers to `UPSERT INTO <dst> <src>`; needs `commit` |
| `mv <src> <dst>` | Move | Copy then remove; needs `commit` |
| `rm <path>` | Delete | Lowers to `REMOVE <path>`; needs `commit` |

That is the **complete** builtin set: `ls cd pwd cat cp mv rm`. Anything else on a line is parsed as
a raw pipe-SQL statement.

`cp`/`mv`/`rm` exist **only** in the interactive shell. For one-shot scripting use the underlying
statements directly — e.g. `qfs run "upsert into /local/path/file values (…)"`.

`describe` is **not** a shell builtin — it is a CLI subcommand (`qfs describe <path>`). Typing
`describe …` at the prompt falls through to the pipe-SQL parser and errors.

### Only `/local` is mounted in the shell

The shell mounts a single driver, `local`, rooted at the directory you launched `qfs` from. Paths
like `/drive/...` or `/sql/...` are not reachable from inside the shell (`cd /drive` →
`error[unknown_mount]: no driver is mounted there`). To query a connected service, use `qfs run`
with its absolute path.

### `ls`, `cd`, `pwd`

```text
local:/$ ls
name        | size | is_dir | modified
----------- | ---- | ------ | -------------
config.json | 22   | false  | 1782733586828
reports     | 0    | true   | 1782733586828
(2 row(s))
local:/$ cd reports
local:/reports$ pwd
/local/reports
local:/reports$ cd ..
local:/$
```

Note the two renderings of the location: the **prompt** shows `{driver}:{path}` (`local:/reports`),
while `pwd` prints the same place with the driver as the leading mount segment (`/local/reports`).
They are the same location, written two ways.

### `cat` lists stat rows, not bytes

A single-file read returns one stat row — `name | path | size | modified | is_dir | mode | content`
— and the `content` column is rendered as a `<N bytes>` placeholder, **not** the file's bytes:

```text
local:/$ cat config.json
name        | path               | size | modified      | is_dir | mode  | content
----------- | ------------------ | ---- | ------------- | ------ | ----- | ----------
config.json | /local/config.json | 22   | 1782733586828 | false  | 33188 | <22 bytes>
(1 row(s))
```

To transcode a file's contents, pipe it through a codec in a raw statement, e.g.
`/local/<file>.json |> decode json |> encode yaml` (see the [language guide](/language)).

## Preview, then commit

The shell follows the same safety model as the CLI: reads run immediately, but every **effect**
(`cp`, `mv`, `rm`, or any raw write) is previewed and applied **nothing** until you confirm it. There
is no persistent "preview mode" or "commit mode" to toggle — the gate is per effect.

Run an effect and you get a plan preview, ending in `type COMMIT to apply`:

```text
local:/$ cp config.json reports/config.json
PREVIEW (1 effect plan(s), nothing applied):
PREVIEW: 2 effect(s)
  #0 READ -> local:/local/config.json [affected ?]
  #1 UPSERT -> local:/local/reports/config.json [affected ?]
  total affected: ?
type COMMIT to apply
```

Type a bare `commit` on the **next** line to apply the effect you just previewed:

```text
local:/$ commit
COMMITTED (1 effect plan(s)):
COMMITTED:
PREVIEW: 2 effect(s)
  #0 READ -> local:/local/config.json [affected ?]
  #1 UPSERT -> local:/local/reports/config.json [affected ?]
  total affected: ?
```

`commit` confirms only the **immediately preceding** preview. Type anything else after a preview and
the plan is simply discarded — nothing is applied. Type `commit` with no pending preview and you get
`nothing to commit`.

Irreversible effects announce themselves in the preview with a `(!)` marker, so you never delete
something by accident:

```text
local:/$ rm config.json
PREVIEW (1 effect plan(s), nothing applied):
PREVIEW: 1 effect(s)
  #0 REMOVE -> local:/local/config.json [affected ?] (!)
  (!) irreversible: 1 node(s) [#0]
  total affected: ?
type COMMIT to apply
```

## When to use the shell vs. `qfs run`

- **Shell** — interactive exploration of your local filesystem: list folders, stat files, stage a
  copy or delete and confirm it. Relative paths and a current directory make this comfortable.
- **`qfs run`** — scripting and automation, and the way to reach connected services (mail, Drive,
  SQL, git, …): one statement, absolute paths, preview-by-default, composes with pipes and `jq`.
  This is what you reach for in scripts and what an AI agent uses.

Both speak the exact same language underneath.
