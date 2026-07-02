# qfs

**One small grammar for every external service.** `qfs` is a single Rust binary that exposes
every backend — the local filesystem, mail, drive, object storage, GitHub, Slack, SQL, git, Google
Analytics, Claude, an HTTP fetcher, a directory, and `/sys` administration — through **one uniform, filesystem-shaped,
pipe-SQL DSL**. The same engine presents **three faces**: a **CLI** (and an FTP-like interactive
shell), an **MCP endpoint** for AI agents, and an **embedded web dashboard** with approval cards.
It runs locally or as a self-hosted server (RFD-0001 §1, §9). The Cloudflare Workers `wasm32`
target is parked while the worker crate is offline ([ADR-0005](docs/adr/0005-deployment-hosts.md)).

> qfs generalizes the FTP-shell idea per
> [RFD-0001](.workaholic/RFDs/0001-qfs-architecture.md): instead of one FTP-style client for one
> service, qfs is the *closed-core grammar + open registries* control plane that the FTP-shell
> idea generalizes to.

## Why qfs exists (the AI payoff)

qfs exists **for AI**. An agent learns *one* small grammar and *one* operating procedure instead
of N vendor SDKs:

> **DESCRIBE `<path>` → write a qfs statement → PREVIEW → COMMIT**

- **DESCRIBE** a node (`qfs describe /mail/drafts`) returns its archetype, columns, supported
  verbs, `CALL` procedures, prelude aliases, and pushdown — the contract the agent reads first.
  DESCRIBE is **pure**: no credentials, no I/O, no network.
- **Write** a pipe-SQL statement against the node.
- **PREVIEW** (the default) shows the effect-plan without touching the World.
- **COMMIT** applies the plan.

The four steps are *identical* across every backend. That uniformity is the product. The agent's
operating procedure ships embedded in the binary — run `qfs skill` (and `qfs skill --examples`).

## Core model

- **Closed core + three open registries** (RFD §3). The language is a *closed core* — a frozen set
  of keywords and operators. A new backend adds **zero** keywords. Everything extensible is a
  registry entry:
  - **paths** — a new mount (`/mail`, `/s3`, `/github`, …). See [`docs/drivers.md`](docs/drivers.md).
  - **functions / procedures** — a registered `CALL driver.action(..)` + pure prelude aliases.
  - **codecs** — a registered `DECODE`/`ENCODE` format (json, jsonl, yaml, toml, csv, md).
- **Four archetypes** (RFD §5). Every node is Blob, Relational, Append, or ObjectGraph; each
  declares which universal verbs it supports. Unsupported verbs are **rejected at parse time**, so
  the agent never plans a rejected op.
- **Purity invariant** (RFD §3/§6). Every function/alias produces a `Plan` and performs no I/O.
  `SEND(d)` does not send mail — it desugars to a `CALL mail.send` node in a `Plan`. Nothing
  happens until `COMMIT`. See [`docs/language.md`](docs/language.md).
- **Least privilege** (RFD §10). Credentials are stored per layer — an OAuth **app** registration
  (`cat credentials.json | qfs app add google`) and per service **account**
  (`qfs account add google`, or `printf %s "$TOKEN" | qfs account add github work`) — never inline
  in a config, a log, or a doc, and a secret value is always piped in via stdin (never argv). Under
  the hood every secret is **envelope-encrypted at rest** in the SQLite Project DB: a random
  data-key encrypts each value, and that data-key is wrapped by the vault's **KeyGuardian slots**
  — the passphrase slot `qfs init` enrolls when it creates the store, plus an optional OS-keychain
  slot (`qfs vault enroll keychain`) so this host unlocks without a passphrase. See
  [Connect a service](docs/guide/connect.md) for the per-source steps, and
  [Connections & credentials](docs/guide/connections.md) for the full flow. `CREATE POLICY` gates
  writes by verb / path / irreversibility. See [`docs/server.md`](docs/server.md).

## The shipped surface (three faces, one engine)

The same engine answers on three faces, and the safety model (PREVIEW → COMMIT, the irreversible
gate, the policy gate) is identical on all of them:

- **CLI** — one-shot `qfs run` / `qfs describe`, plus the FTP-like interactive shell (no
  subcommand). See [`docs/guide/cli.md`](docs/guide/cli.md).
- **MCP endpoint** — the server exposes the same DESCRIBE → PREVIEW → COMMIT loop as MCP tools so an
  AI agent drives every service through one contract.
- **Web dashboard** — an embedded SPA served by `qfs serve` with **approval cards**: a human
  reviews and approves a pending irreversible commit in the browser.

Beyond reads/writes, v0.0.9 ships the operator surface:

- **Identity** — first-run setup + lookup: `qfs init <email>` creates the encrypted vault and
  registers the operator identity — one operator per OS user, no password (your OS login is the
  authentication; the email is an accountability label). `qfs identity whoami` looks it up.
  (Identity is not authorization.)
- **Teams & invites** — `qfs invite create` mints a one-time, expiring token; `qfs invite redeem`
  creates the local user + membership; `qfs invite revoke` cancels a pending invite.
- **Jobs** — a saved `CREATE JOB … EVERY … DO …` plan run by an **external** scheduler:
  `qfs job run <config> <name> --commit` and `qfs job cron <config> <name>` (qfs is not a scheduler).
- **`/sys/*` administration** — the deployment's own state surfaced as ordinary paths you query:
  `/sys/{users,projects,audit,connections,policies,metrics,settings,billing}`. The selectable AI
  **safety mode** lives in `/sys/settings`; the hash-chained WORM audit tail is `/sys/audit`; the
  per-team billing tier is recorded as data in `/sys/billing` (never a payment secret).
- **OAuth Authorization Server** — qfs is its own OAuth AS (Dynamic Client Registration + PKCE) for
  the agent/dashboard auth handshake. Account credentials support rotation / revocation
  (`qfs account rotate|revoke`), and the vault's key slots are managed with
  `qfs vault slots|enroll|revoke|rekey`.

Honestly **not yet wired** (kept out of the capability claims): the MCP cloud-tunnel dial, an
LDAP/AD/Entra/Workspace directory backend, the qfs Cloud broker endpoint, a payment provider,
Postgres/MySQL SQL backends (SQLite ships), live-verified ga/objstore *reads* (the read facets
are wired; gmail/gdrive/github/slack reads are verified live), live `/cf` reads and commits (the
mount is describe-only), and the Cloudflare Workers wasm artifact (parked, ADR-0005).

## Install

```sh
# Download, verify the sha256, and install the matching tarball for your OS/arch:
curl -fsSL https://raw.githubusercontent.com/qmu/qfs/main/packages/qfs/install.sh | sh
qfs --version
```

`install.sh` detects your OS + arch, downloads the matching release tarball, **verifies its
sha256 before extracting**, and installs `qfs` to `~/.local/bin` (override with `QFS_INSTALL_DIR`).
Releases ship static Linux (`x86_64`/`aarch64` musl) + macOS (`x86_64`/`aarch64`) binaries; see
[Releases & distribution](#releases--distribution).

Or build from source (requires a Rust toolchain):

```sh
cd packages/qfs && cargo build --release    # the native binary at packages/qfs/target/release/qfs
```

After installing, `install.sh` prints a short **Next steps** guide (try it, connect a service,
update, and where to read more). The [Quickstart](#quickstart-the-loop) below is the same loop.

## Quickstart (the loop)

```sh
# 1. DESCRIBE a node — pure, no creds, no network (the contract you read first):
#    Start local: this returns a real schema with no account and no setup.
qfs describe /local/etc

# 2. READ that returns rows right now — list any local directory
#    (`/local` + an ABSOLUTE host path):
qfs run "/local/etc |> select name, size, is_dir |> limit 5"

# …or run a pure codec pipeline — decode one format, encode another:
echo '{"k":1,"name":"alpha"}' > /tmp/d.json
qfs run "/local/tmp/d.json |> decode json |> encode yaml"
# -> {"rows":[{"content":"- k: 1\n  name: alpha\n"}]}

# …or query SQLite / git with the WHERE pushed into the backend — needs a
#    connection: export QFS_SQL_<conn>=<file.sqlite> / QFS_GIT_<repo>=<path> first.
qfs run "/sql/orders/orders |> where total > 100 |> select customer, total |> order by total desc"
qfs run "/git/myrepo/commits |> select sha, message |> limit 10"

# 3. PREVIEW a write — the default; it builds the effect-plan and touches nothing:
qfs run "insert into /mail/drafts values ('a@b.com','Hi','Body')"
# -> {"preview":{"rows":[{"verb":"INSERT","target":{"driver":"mail","path":"/mail/drafts"},
#     "affected":{"exact":1},"irreversible":false}],...},"committed":false}

# 4. COMMIT applies the plan — `--commit` (writes need a CONNECTED account; below).
qfs run "insert into /mail/drafts values ('a@b.com','Hi','Body')" --commit

# A mail READ, or an irreversible CALL, needs a mounted Google account. Nothing
#    cloud is pre-mounted — until `qfs connect` creates the /mail mount, the read
#    fails closed (exit 3, kind: capability). The one-time setup:
#      qfs init you@example.com                    # vault + operator identity
#      cat credentials.json | qfs app add google   # your Google OAuth app
#      qfs account add google                      # paste-back browser consent on a TTY
#      qfs connect /mail --driver gmail --account you@gmail.com
qfs run "/mail/inbox |> where subject LIKE '%invoice%' |> select subject, from"
qfs run "/mail/drafts |> where id == 'draft-1' |> call mail.send" --commit --commit-irreversible
# (same fail-closed error until /mail is mounted; `mail.send` is irreversible,
#  so a one-shot COMMIT also needs the explicit `--commit-irreversible` ack)
```

The interactive shell (no subcommand) gives the same loop with an FTP-like prompt:

```sh
qfs            # ls / cd / cat / cp / rm … all desugar to the same pipe-SQL plans
```

## `qfs --version`

The long form is the field-debug anchor — semver + the git sha + the target triple it was built
for (RFD §9):

```
$ qfs --version
qfs 0.0.10
commit:  <git-sha>
target:  x86_64-unknown-linux-musl
wasm32:  false
```

`target` is the triple the binary was built for — a shipped release is always one of the four
static-musl Linux / macOS triples (`{x86_64,aarch64}-unknown-linux-musl`, `{x86_64,aarch64}-apple-darwin`),
never a dynamic `-gnu` build.

## Documentation

The reference docs under [`docs/`](docs/) are **generated from the binary's own registries** (run
`cargo run -p xtask -- gen-docs`) so they can never drift from the code:

- [`docs/language.md`](docs/language.md) — the pipe-SQL grammar (EBNF), the **frozen reserved-word
  table**, the open-registry governance rules, and the purity invariant.
- [`docs/drivers.md`](docs/drivers.md) — the **generated driver catalog**: archetypes,
  capabilities (supported *and* unsupported verbs, shown explicitly), procedures, codecs.
- [`docs/server.md`](docs/server.md) — the server guide: `CREATE ENDPOINT|TRIGGER|JOB|VIEW|WEBHOOK
  |POLICY`, bindings, and the t36 deployment mapping.
- [`docs/README.md`](docs/README.md) — the docs index (architecture, ADRs, the agent skill).
- [`crates/skill/assets/SKILL.md`](packages/qfs/crates/skill/assets/SKILL.md) — the embedded AI operating
  procedure (also via `qfs skill`).

## Releases & distribution

Releases are tag-triggered (`.github/workflows/release.yml`): on a `v*` tag, CI runs
`cargo run -p xtask -- dist`, which cross-compiles the four native targets (static musl Linux +
macOS, both arches), strips, checksums (`sha256`), and
tarballs them into `dist/`; the workflow attaches the tarballs + checksums to the GitHub Release.
`install.sh` consumes those artifacts.

> **Offline / disk scoping (ADR-0007).** The release/musl/wasm pipeline is **CI-only**: a release
> build and the full-workspace wasm build cannot run on the constrained trip host, and musl static
> cross-link needs a cross toolchain CI provides. `cargo run -p xtask -- dist` therefore refuses to
> run locally (set `QFS_DIST_ALLOW=1` only where a clean toolchain + disk exist). The native debug
> build and `cargo run -p xtask -- gen-docs` are the local verification surface. This mirrors
> t36/ADR-0005's parking of the musl/CF artifacts.

## SemVer policy — the grammar is the stable surface

qfs versions follow SemVer, and **the stable public surface is the grammar** (the frozen keyword +
operator set and the DESCRIBE/PREVIEW/COMMIT contract):

- **MAJOR** — a breaking change to the grammar / the frozen keyword set / the describe contract.
- **MINOR** — a new registry entry (a new driver mount, procedure, or codec) — additive, no
  grammar change.
- **PATCH** — a fix that changes neither the grammar nor a registry's public shape.

Because the core is frozen, an AI agent that learned the grammar once keeps working across MINOR
and PATCH releases.

## Deploy

The same `CREATE …` bindings deploy onto two production hosts behind the `RuntimeHost` seam
([ADR-0005](docs/adr/0005-deployment-hosts.md)). qfs documents the mapping in
[`docs/server.md`](docs/server.md#deployment-mapping-t36-rfd-8); ticket **t36** builds the host
adapters (EC2 daemon live; Cloudflare Workers honestly parked while the worker crate is offline).

## License

MIT.
