---
created_at: 2026-06-30T01:01:00+09:00
author: a@qmu.jp
type: enhancement
layer: [Domain, Config]
effort: 4h
commit_hash:
category: Added
depends_on: []
---

# Mount the generic HTTP/REST driver (`/rest`) so it's reachable from the CLI

Roadmap "Near-term backlog": a generic HTTP/REST driver exists but isn't reachable as a path.
Confirmed: `RestDriver` implements the full contract with `mount() == "/rest"`
(`crates/driver-http/src/lib.rs:137`). The crate **is** a binary dependency
(`crates/qfs/Cargo.toml:126`) but only its `ReqwestClient` is consumed — as the github/slack
transport (`crates/driver-http/src/transport.rs`). `RestDriver::new` / `rest_apply_driver`
(`lib.rs:185`) are never called in the binary, so `/rest` is unmounted in read, apply, and describe.

## Plan

1. Build a per-`<api>` REST config surface — `RestDriver::new` takes config
   (`crates/driver-http/src/config.rs`): base URL, auth, and how a path maps to an endpoint. Decide
   how an API is declared (this should align with the forthcoming `CREATE CONNECTION` model — see the
   connection epic `20260630004100`; for now mirror today's env/credential plumbing).
2. Register `/rest` in the read/plan mounts (`crates/qfs/src/shell.rs` ~line 153) and apply mounts
   (`crates/qfs/src/commit.rs:235 live_registry`) via `rest_apply_driver`, with `cloud_bind_allowed`
   gating, following the github/slack pattern.
3. Confirm it folds into describe/catalog (`crates/qfs/src/catalog.rs:104`).

## Key files

- `crates/driver-http/src/lib.rs:137,185` (`RestDriver`, `rest_apply_driver`),
  `crates/driver-http/src/config.rs`, `crates/qfs/src/{shell.rs:153,commit.rs:235,catalog.rs:104}`.

## Considerations

- The REST config surface partly overlaps the `CREATE CONNECTION` epic (`20260630004100`): a `/rest`
  API is exactly a "connection with a base URL + secret ref." Coordinate so this doesn't bake a
  throwaway config shape — prefer landing it behind the same connection model, or keep the surface
  minimal and migratable.
- Sibling of the Cloudflare mount ticket (`20260630010050`). Bump the patch; regenerate
  `docs/drivers.md` (anti-drift) once `/rest` appears in the catalogue.
