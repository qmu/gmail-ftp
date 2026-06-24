# CLAUDE.md

Guidance for Claude Code working in this repository.

## What this is

A monorepo. Each project lives under `packages/<project>/`. Today there is one project:

- **`packages/qfs/`** — `qfs`, a single Rust binary (CLI + server) that exposes every external
  service through one uniform, filesystem-shaped, pipe-SQL query language. This is the product.

The documentation site is at repo-root `docs/` (VitePress; `docker compose up docs` serves it at
`localhost:5173`). The repo-root `README.md` is the qfs README.

## Build & test

The Cargo workspace is under `packages/qfs/`:

```sh
cd packages/qfs
cargo build --workspace
cargo test --workspace            # 1240+ tests, all hermetic (no network/credentials)
cargo clippy --workspace --all-targets -- -D warnings   # NOT --all-features (qfs-host features are mutually exclusive)
cargo fmt --all --check
cargo run -p xtask -- gen-docs --check   # anti-drift: committed docs must match the binary
```

Generated reference docs (`docs/{language,drivers,server}.md`) are rendered from the binary by
`cargo run -p xtask -- gen-docs` — never hand-edit them; change the source and regenerate.

## Versioning — bump the patch on every shipped PR

**Every change goes on a topic branch → PR → `/report` → `/ship`, and that cycle increments the
patch version.** Before opening the PR, bump the patch in
[`packages/qfs/crates/qfs/Cargo.toml`](packages/qfs/crates/qfs/Cargo.toml) (e.g. `0.0.2 → 0.0.3`).
On ship, cut the matching `v0.0.x` tag so the published, installable release and `qfs --version`
stay in sync. (The project's conceptual SemVer policy — versioned surface = grammar + registries —
is in the README; this operational rule says bump the patch on every shipped PR regardless.)

## Deploy

There is no separate server deployment — **the deliverable is the published GitHub Release**.
To deploy a shipped change after the PR merges to `main`:

1. Ensure the patch version was bumped (above).
2. Tag and push: `git tag -a vX.Y.Z -m "qfs vX.Y.Z" && git push origin vX.Y.Z`.
3. `.github/workflows/release.yml` builds the four native tarballs (Linux musl + macOS, both
   arches) on per-OS runners and publishes them to a GitHub Release; `install.sh` consumes them.

The Cloudflare Workers wasm artifact is parked (no cdylib entrypoint yet), so releases ship native
binaries only.
