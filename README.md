# qfs monorepo

This repository is organized as a monorepo: each project lives under `packages/<project>/`.

## Projects

- **[`packages/qfs`](packages/qfs/)** — `qfs`: one small grammar for every external service. A
  single Rust binary (CLI + daemon) that exposes every backend through one uniform,
  filesystem-shaped, pipe-SQL DSL (RFD-0001). See [`packages/qfs/README.md`](packages/qfs/README.md)
  for the authoritative spec, and [`.workaholic/RFDs/0001-qfs-architecture.md`](.workaholic/RFDs/0001-qfs-architecture.md)
  for the design anchor.

## Building

The Cargo workspace lives under `packages/qfs/`:

```sh
cd packages/qfs
cargo build --workspace
cargo test --workspace
```
