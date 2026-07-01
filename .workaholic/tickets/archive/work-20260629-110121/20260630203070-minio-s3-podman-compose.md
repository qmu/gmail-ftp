---
created_at: 2026-06-30T20:31:10+09:00
author: a@qmu.jp
type: enhancement
layer: [Domain, Infrastructure]
effort: 4h
commit_hash: 0e7d50f
category: Changed
depends_on: []
---

# MinIO (S3-compatible) dev stack + live `/s3` objstore backend (owner item #6)

## What's wanted

A `podman compose` running **MinIO** for dev, with qfs's `/s3` (and `/r2`) wired to it so reads (and
ideally writes) work against a local S3-compatible server.

## Current state

`crates/driver-objstore` implements the S3 driver with SigV4 signing (`sigv4.rs`:
access_key_id + secret_access_key + region/endpoint). The binary registers a cred-free **describe**
mount (one representative `bucket`) but no **live** ObjRegistry. `/s3` writes (`upsert into /s3/...`)
are noted as not-yet-implemented.

## Plan

1. `deploy/dev/compose.yml` (or extend #1's): MinIO service + a seeded bucket + dev access keys.
2. Build a **live** `ObjRegistry` in the binary from a declared S3 connection — endpoint (MinIO URL),
   region (`auto`/`us-east-1`), access key id (non-secret), secret access key (via
   `crate::secret_ref`). Register the live read (and apply) facet over the connect-account fallback,
   gated like the other cloud drivers (`crate::commit::cloud_bind_allowed`).
3. Confirm `/s3/<bucket>/<key>` reads from MinIO; wire `upsert into /s3/...` (the deferred write).

## Key files

- `crates/qfs/src/objstore.rs` (registry build + endpoint/keys), `crates/driver-objstore/src/{sigv4,
  client,backend}.rs`, `crates/qfs/src/{shell.rs,commit.rs}`. New `deploy/dev/compose.yml`.

## Considerations

- The objstore config (endpoint + region + bucket + access-key-id) is richer than `(driver, locator,
  secret)` — align with the connection model (`CREATE CONNECTION ... DRIVER s3 AT '<endpoint>'` plus
  the access-key-id; secret via `SECRET`). Coordinate with the connection epic's follow-ups.
- Live-testable here (podman). MinIO is S3 v4 compatible, so the SigV4 signer should work as-is.

## Final Report

The live `/s3` READ path is delivered AND verified live against MinIO (podman). `deploy/dev/compose.yml`
now runs MinIO (+ a one-shot init that creates `qfs-dev` and seeds two objects). A new
`ObjReadDriver` (`crate::read_facets`) adapts the in-house `ObjDriver`'s `ls` (native S3
`list_objects_v2` + prefix/delimiter pushdown) and `get` (streaming download) to the async
`ReadDriver` seam, registered live from the SAME SigV4 backend the apply path builds
(`crate::commit::live_obj_read_driver`, fail-closed). Live-proven: `/s3/qfs-dev` lists both objects
with full metadata; `/s3/qfs-dev/greeting.txt` returns the object bytes; `/s3/qfs-dev/data.csv |>
decode csv` → `{a:1, b:2}` (the object→content→codec bridge).

**Deferred (as the ticket noted):** the `/s3` WRITE (`upsert into /s3/...`). The objstore apply driver
is registered, but the upsert effect/syntax is not yet wired end-to-end — a follow-up. And the config
here uses the existing `QFS_S3_*` env vars (+ `QFS_SECRET_S3_<conn>` via EnvStore); aligning the richer
objstore config (endpoint/region/bucket/access-key-id) with the declared `CREATE CONNECTION DRIVER s3`
model is the connection-epic-coordinated follow-up the ticket flagged.

### Discovered Insights

- **Insight (IMPORTANT, cross-cutting)**: ALL cloud READ facets are latently broken when run live —
  the reqwest transport (`driver-http/client.rs`) drives its OWN runtime via `rt.block_on`, which
  PANICS ("runtime within a runtime") when called from inside the async read executor. The cloud read
  facets (github/slack/gmail/ga) have only ever been exercised with MOCK clients, so the panic was
  never hit; `/s3` is the FIRST cloud read wired to run live and it surfaced it. Fixed for objstore by
  running the blocking SigV4 read on a dedicated OS thread (no tokio context). **The gmail/gdrive/ga
  live reads (EPIC `20260630203030`) will hit the SAME panic and need the same treatment** — ideally a
  shared fix at the read-executor/transport layer rather than per-facet.
- **Insight**: the objstore consent classification is keyed by the literal driver id, but
  `CLOUD_DRIVERS` lists `objstore` while the registered driver ids are `s3`/`r2` — so
  `cloud_bind_allowed("s3")` returns true (the bind gate is effectively OFF for s3/r2). Worth
  reconciling (either add `s3`/`r2` to `CLOUD_DRIVERS` or rename), but it made the env-credential dev
  path work without an operator sign-in/consent step.
- **Insight**: MinIO needs PATH-style S3 addressing (`http://host:9000/<bucket>/<key>`); qfs's
  `HttpBackend` already signs path-style, so `QFS_S3_ENDPOINT=http://localhost:9000` works as-is.
