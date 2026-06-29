---
created_at: 2026-06-30T01:00:50+09:00
author: a@qmu.jp
type: enhancement
layer: [Domain, Config]
effort: 4h
commit_hash:
category: Added
depends_on: []
---

# Mount the Cloudflare driver (`/cf`) so it's reachable from the CLI

Roadmap "Near-term backlog": a Cloudflare driver exists in the code but isn't reachable as a path.
Confirmed: `CfDriver` implements the full `Driver` contract with `mount() == "/cf"`
(`crates/driver-cf/src/lib.rs:103`), but **`qfs-driver-cf` is not even a dependency of the binary**
(`crates/qfs/Cargo.toml` has no `driver-cf`; only `driver-objstore`/`sql-core` use it for backend
types). `CfDriver::new` is instantiated nowhere outside its own crate; the only `/cf` mention in the
binary is a doc-comment (`crates/qfs/src/job.rs:326`).

## Plan

1. Add `qfs-driver-cf` to `crates/qfs/Cargo.toml`.
2. Build a `CfRegistry` from credentials and register `/cf` in the read/plan mounts
   (`crates/qfs/src/shell.rs` `run_engine_and_reads` / `register_google_planning_mounts` ~line 153)
   and the apply mounts (`crates/qfs/src/commit.rs:235 live_registry`). Follow the github/slack
   pattern, including the `cloud_bind_allowed` gating.
3. Ensure it folds into describe/catalog (`crates/qfs/src/catalog.rs:104` picks up whatever is
   registered) so `describe /cf/…` reports its real surface.
4. Connection/credential plumbing for the Cloudflare token (account id + API token), mirroring how
   cloud sources resolve credentials today.

## Key files

- `crates/driver-cf/src/lib.rs:103` (`CfDriver`, `/cf`), `crates/qfs/Cargo.toml` (add dep),
  `crates/qfs/src/{shell.rs:153,commit.rs:235,catalog.rs:104}`.

## Considerations

- The qfs-host features are mutually exclusive — clippy with `--workspace --all-targets` (not
  `--all-features`) per CLAUDE.md. Confirm the cf dep doesn't break the feature matrix.
- Sibling of the HTTP/REST mount ticket (`20260630010060`) — same "exists but unmounted" shape, but
  distinct drivers and credential plumbing, so shipped as separate PRs. Bump the patch; regenerate
  `docs/drivers.md` (anti-drift) once `/cf` appears in the catalogue.
