# qfs

**One small grammar for every external service.** `qfs` is a single Rust binary that exposes
every backend — mail, drive, object storage, GitHub, Slack, SQL, git, Google Analytics — through
**one uniform, filesystem-shaped, pipe-SQL DSL**. It runs as a **CLI** locally, as a **daemon** on
EC2, or (target) compiled to `wasm32` for Cloudflare Workers (RFD-0001 §1, §9).

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
- **Least privilege** (RFD §10). Credentials are stored per driver/connection (`qfs connection add`),
  never inline in a config, a log, or a doc. They are **envelope-encrypted at rest** in the
  SQLite Project DB: a random data-key encrypts each secret value, and that data-key is itself
  wrapped under a key derived from `QFS_PASSPHRASE` (argon2id) — so export `QFS_PASSPHRASE` before
  `qfs connection add`/`use`. (This SQLite store replaces the old encrypted file vault; there is no
  migration — re-run `qfs connection add` once for any existing connections.) `CREATE POLICY` gates
  writes by verb / path / irreversibility. See [`docs/server.md`](docs/server.md).

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
# 1. DESCRIBE a node — pure, no creds, offline:
qfs describe /mail/drafts

# 2. write + 3. PREVIEW (default — shows the plan, touches nothing):
qfs run "/mail/inbox |> WHERE subject LIKE '%invoice%' |> SELECT subject, from"

# create a draft (PREVIEW first, then COMMIT to apply):
qfs run "INSERT INTO /mail/drafts VALUES (...)"            # PREVIEW
qfs run "INSERT INTO /mail/drafts VALUES (...) COMMIT"     # apply

# 4. COMMIT an irreversible effect requires an explicit ack in one-shot:
qfs run "id:draft-1 |> CALL mail.send COMMIT" --commit-irreversible
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
qfs 0.0.4
commit:  <git-sha>
target:  x86_64-unknown-linux-gnu
wasm32:  false
```

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
