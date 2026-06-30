---
created_at: 2026-06-30T20:31:10+09:00
author: a@qmu.jp
type: enhancement
layer: [Domain, Infrastructure]
effort: 4h
commit_hash:
category: Added
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
