---
created_at: 2026-06-25T09:47:51+09:00
author: a@qmu.jp
type: enhancement
layer: [Infrastructure]
effort: M
commit_hash: 52651e1
category: Added
depends_on: []
---

# Wire networked READ facets (github/slack/google/objstore `FROM` + `CALL`-via-`FROM`)

## Overview

v0.0.4 wired github/slack **commit** (pure-effect `INSERT`/`CALL` legs). But statements that **read**
a networked source — `FROM /github/.../pulls`, and therefore `FROM … |> CALL github.merge` (the CALL
pipeline starts with a read) — still fail at the read stage: only the local read facet is registered
in `run_engine_and_reads` (`shell.rs`). Networked reads need each driver's read facet adapted to
`qfs_exec::ReadDriver` and registered in the `ReadRegistry`, behind the same credentialed client the
commit path uses.

## Exact seams

- **Read seam:** `qfs_exec::ReadDriver::scan(&ScanNode) -> Result<RowBatch, CfsError>` (async). The
  read executor resolves the driver by the source id (`crates/exec/src/exec.rs` `reads.get(id_of(
  scan.source))`). The local model to copy is `LocalReadDriver` in `crates/qfs/src/shell.rs`.
- **Per driver, add a top-level `read_rows` helper INSIDE the driver crate** (mirror
  `qfs_driver_local::scan_rows`) so the binary adapter does not duplicate the path→plan→decode
  logic. For github the pieces exist but there is no single entry: `GitHubPath::parse_str(path)` →
  `ReadPlan::list(slug, namespace, sub, predicate)` → `GitHubClient::list(slug, namespace, sub,
  params)` → `read::decode_list(namespace, &value)`. Add
  `pub fn read_rows(client: &dyn GitHubClient, path, predicate) -> Result<RowBatch, GitHubError>`
  and unit-test it with `MockGitHubClient`. Same shape for slack / the Google drivers / objstore
  (each has its own read module + client `list`/`get`).
- **Binary adapters:** `GitHubReadDriver { client: Arc<dyn GitHubClient> }` etc. impl `ReadDriver`,
  built with the real credentialed client (reuse the `commit.rs` credential resolution — factor it
  into a shared `clients`/`creds` module so commit + read share one builder). Register in
  `run_engine_and_reads`'s `ReadRegistry` under each DriverId.
- Reads hit the network → need creds; without them, fail closed with a clear auth error (not empty
  rows).

## Verification

- Unit: each driver's new `read_rows` against its mock client (offline, real).
- The binary adapter against a loopback server requires a base-URL override on the real client
  (github/slack hardcode api.github.com / slack.com) — add a test-only base-URL injection or verify
  via the mock-client path. **Live reads need real credentials.**

## Considerations

- Keep credential resolution single-sourced between commit + read (one `clients` module).
- Depends on the per-driver commit tickets only loosely (the client builders are shared, so doing
  them together per driver family is natural — fold reads into each family's PR if convenient).
- Patch bump + docs-in-lockstep per the umbrella ticket.

## DISPOSITION (night drive, 2026-06-29)

GITHUB + SLACK read facets SHIPPED (read_rows helpers + ReadDriver adapters + shared credentialed-client builder + ReadRegistry wiring, fail-closed without creds; commit 52651e1). The gmail/gdrive/ga/objstore reads are a DOCUMENTED REMAINING SEAM, not built: they are genuinely multi-shape (gmail = search ids + N×get; ga = compiled runReport; gdrive = listing vs download/export+codec; objstore = list_objects_v2 vs get_object) with no single parse->list->decode read_rows shape (gmail/ga have no read module at all). Building them faithfully is a larger separate slice; the shared crate::clients builder is their extension point. Live network reads (real creds + real API) are a seam per the ticket — the hermetic proof is the read_rows helpers vs mock clients + fail-closed wiring.
