# Installation

qfs is a single binary. There's nothing to configure to get started — you can describe and preview
queries with no credentials at all.

## Install script (recommended)

```sh
curl -fsSL https://raw.githubusercontent.com/qmu/qfs/main/packages/qfs/install.sh | sh
```

The script detects your OS and architecture, downloads the matching release, **verifies its
checksum before extracting**, and installs `qfs` to `~/.local/bin`. It never asks for or fetches
any credential.

::: tip Requires a published release
The script downloads from the project's GitHub Releases. If no release has been published yet,
[build from source](#build-from-source) instead — it's one command. (Releases are cut by pushing a
`v*` tag, which builds the Linux and macOS binaries in CI and attaches them.)
:::

To install somewhere else:

```sh
curl -fsSL https://raw.githubusercontent.com/qmu/qfs/main/packages/qfs/install.sh | QFS_INSTALL_DIR=/usr/local/bin sh
```

Make sure the install directory is on your `PATH`. Then check it works:

```sh
qfs --version
```

## Build from source

You'll need a recent Rust toolchain. The qfs workspace lives under `packages/qfs/`:

```sh
git clone https://github.com/qmu/qfs
cd qfs/packages/qfs
cargo build --release        # produces target/release/qfs
./target/release/qfs --version
```

## Supported platforms

Released binaries are built for:

- **Linux** — `x86_64` and `aarch64` (static musl, no system dependencies)
- **macOS** — `x86_64` (Intel) and `aarch64` (Apple Silicon)

## First check (no credentials needed)

You can immediately explore the language without connecting anything. `describe` and `preview` are
completely offline:

```sh
# What can I do with a mail draft?
qfs describe /mail/drafts

# Preview a query — shows the plan, changes nothing
qfs run "insert into /mail/drafts values ('alice@example.com', 'Hi', 'Body text')"
```

When you're ready to act on real services, [add a connection](/guide/connections).

**Next:** [Get started →](/guide/getting-started)
