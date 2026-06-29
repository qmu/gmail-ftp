---
created_at: 2026-06-25T09:47:48+09:00
author: a@qmu.jp
type: enhancement
layer: [Infrastructure]
effort: M
commit_hash: 117c47e
category: Added
depends_on: []
---

# Wire s3 / r2 live commit (objstore SigV4) into the binary

## Overview

The objstore production backend is **already built**: `crates/driver-objstore/src/backend.rs` has
the real `HttpBackend` (SigV4 over a local `HttpExchange` seam) and `sigv4.rs` (the signer). It was
never wired into the binary's commit registry, and there is no bucket/endpoint/credential config
source — so a `/s3/<bucket>/<key>` or `/r2/...` commit never reaches the SigV4 backend.

This is the github/slack pattern (shipped v0.0.4, see umbrella ticket) plus the SigV4/config
specifics. **Production backend exists — this is wiring + config + live verification.**

## Exact seams

- **Transport:** `qfs_driver_objstore::HttpExchange` is its own trait over the shared
  `qfs-http-core` DTOs (`HttpMethod`/`HttpRequest`/`HttpResponse`, local `TransportError { reason }`).
  Add `impl qfs_driver_objstore::HttpExchange for ReqwestTransport` (`crates/qfs/src/transport.rs`)
  — pure delegate + error remap, like the github/slack/google impls. Loopback-unit-test it.
- **Backend:** `HttpBackend::new(exchange, endpoint: Endpoint, creds: SigV4Credentials, amz_date,
  date_stamp)`. NOTE: `amz_date` (`YYYYMMDDTHHMMSSZ`) + `date_stamp` (`YYYYMMDD`) are **fixed at
  construction** (a deterministic-signing test seam). `commit.rs` `live_registry()` is rebuilt per
  commit invocation (short-lived), so constructing with the **current UTC** at registry-build time
  is correct for one commit — format via the `time` crate (already a `qfs-secrets` dep; add to the
  binary). If a long-lived registry is ever introduced, switch to a clock-based date.
- **Config:** `SigV4Credentials::new(access_key_id, secret_access_key: Secret)` (+ optional STS
  session token). Source the access key id + secret from the credential store (the account the user
  added) and the region/endpoint from config (decide env names, e.g. `QFS_S3_REGION`,
  `QFS_R2_ACCOUNT_ID`). `Endpoint` (backend.rs:222) carries the host/region routing. Bucket → backend
  mapping: build an `ObjRegistry::with_bucket(<name>, Bucket::new(Arc<HttpBackend>))`. The bucket name
  is in the path (`/s3/<bucket>/...`); decide whether to build buckets on demand or from config.
  If region/keys absent → do NOT register (honest fail-closed).
- **Register:** `S3Driver::new(registry)` / `R2Driver::new(registry)` → `s3_apply_driver` /
  `r2_apply_driver` → `commit.rs` under DriverIds `s3`/`r2`. Cred-free planning mounts already exist
  in `describe.rs`; add them to `run_engine_and_reads` (`shell.rs`) so `/s3`,`/r2` commits PLAN.
- **Dep direction:** `qfs-driver-objstore` is already a binary dep (describe) — no new edge.

## Verification

- Unit: the `HttpExchange` adapter against a loopback server; SigV4 signing goldens already exist in
  `driver-objstore/src/tests.rs`; no-credential commit fails closed.
- **Live (needs a connected env):** real S3 and/or R2 keys + a test bucket to confirm an actual
  `UPSERT`/`REMOVE` object. Cannot be verified offline.

## Considerations

- Secret access key is a `Secret` (redacts); never logged/argv/in errors.
- Honesty + patch bump + docs-in-lockstep per the umbrella ticket.
