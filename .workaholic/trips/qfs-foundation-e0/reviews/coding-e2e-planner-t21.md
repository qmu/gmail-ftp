# Coding E2E Review — Planner — t21 (Google Drive driver)

- **Author**: Planner (Progressive)
- **Phase**: Coding / E2E external testing (no code review)
- **Ticket**: `.workaholic/tickets/todo/a-qmu-jp/20260622214650-t21-driver-google-drive.md`
- **Method**: a throwaway external-consumer crate (`/tmp/t21-e2e-planner`, own `[workspace]`,
  path-deps only on `driver-gdrive` [`test-util`], `runtime`, `driver`, `plan`, `types`,
  `codec`, `secrets`). NO production code touched, NO live Drive, NO network. The crate was
  removed after the run. The driver was driven entirely through its **public** surface
  (`GDriveDriver`, `MockDriveClient`, `GDriveClient` trait, `DriveEffect`, `plan_read`,
  `decode_body`, `gdrive_apply_driver`, the effect column consts) and executed end-to-end via
  `qfs_runtime::{Interpreter, DriverRegistry, PlanApplierBridge}`. `MockDriveClient` was seeded
  from outside the crate using the `test-util` `FileMeta::for_test` / `SharedDrive::for_test`
  constructors — confirming the `test-util` feature genuinely re-opens the `#[non_exhaustive]`
  DTOs for a downstream consumer.

## Result per checklist item

| # | Item | Verdict |
|---|------|---------|
| 1 | List/search → rows, `q` pushdown + lossy residual kept | **PASS** |
| 2 | Download + codec; Google-doc export picks export mime | **PASS** |
| 3 | Upload (INSERT) records metadata + content | **PASS** |
| 4 | Trash-not-delete; permanent delete only on explicit `hard_delete` | **PASS** |
| 5 | Multi-account: op on A uses A's client, not B | **PASS** |
| 6 | Token safety: canary absent from Debug/Display/errors/logs; no panics | **PASS** |
| 7 | End-to-end COMMIT through interpreter + bridge (write) | **PASS** |

### Item 1 — list/search → rows, pushdown, residual

A `WHERE` over `/drive/my/reports` combining a LOSSY `name contains 'rep'` (CmpOp::Match) with
an EXACT `mimeType = 'application/json'` lowered to the Drive `q`:

```
'folder1' in parents and name contains 'rep' and mimeType = 'application/json'
```

- The exact `mimeType =` term was dropped from the residual (Drive's operator means exactly the
  SQL predicate).
- The lossy `name contains` term was **kept as the local residual** (`Some(name ~ 'rep')`) — the
  over-fetch-then-filter discipline, so results are exact and never wrong rows.
- `list_files(q, Some("d1"), Some(50))` returned a page; rows decoded with the expected typed
  columns (`id="f1"`, `name="data.json"`, `mime_type="application/json"`).
- The mock recorded the exact `ListFiles { query, drive_id: Some("d1"), page_size: Some(50) }`.
- Capability gate (parse-time, external view): a folder admits `Select`; a relational `INSERT`
  of columns into a **file** (`id:f1`) is rejected structurally (blob, not a table).

### Item 2 — download + codec, export

- Binary `f1` planned `ReadPlan::Download { id: "f1", revision: None }`; downloaded bytes decoded
  through `JsonCodec` to **2 rows** (the bytes → rows boundary).
- Google-native `doc1` (`is_google_doc() == true`) planned `ReadPlan::Export`; default export
  picked **docx** (mime contains `wordprocessingml`); an explicit override produced
  `application/pdf`. The `Export { id: "doc1", export_mime: "application/pdf" }` call was recorded.

### Item 3 — upload

An `Insert` effect into `/drive/my/reports/x.txt` decoded to
`DriveEffect::Upload { parent: "folder1", name: "x.txt", mime: "text/plain", bytes: "hello-drive" }`
and, applied through the driver applier, recorded:

```
Upload { parent: "folder1", name: "x.txt", mime: "text/plain", len: 11 }
```

The recorded call carries both the metadata (parent + name + mime) and the content length.

### Item 4 — trash-not-delete (BLOCKING gate — CLEAR)

- A plain `Remove` of `id:f1`: the effect node is flagged `irreversible`; decoded to
  `DriveEffect::Trash { id: "f1" }` (recoverable), and applying it recorded **`Trash`** — and
  **no `Delete`** call was recorded (`no_permanent_delete=true`).
- `PREVIEW` of the trash plan surfaced the irreversible warning (`pv.rows[0].irreversible`) and
  performed **zero I/O** (mock recorded nothing during preview).
- A permanent delete happened **only** with the explicit `hard_delete` flag column: that effect
  decoded to `DriveEffect::Delete { id: "f1" }` and recorded `Delete`.

There is no path by which a bare `REMOVE` permanently deletes — the BLOCKING condition is clear.

### Item 5 — multi-account

Two independent driver instances over two independent mock clients. A `Remove` op on driver A
recorded exactly `Trash { id: "fA" }` on A's client; B's client recorded **nothing**
(`B_client_untouched=true`). Selection is the per-account-client base (t19).

### Item 6 — token safety (BLOCKING gate — CLEAR)

A canary token `ya29.CANARY-PLANTED-TOKEN-deadbeef-LEAK-IF-SEEN` was planted in a
`qfs_secrets::Secret` and pushed through every channel a leak could ride. Captured proof:

```
INFO  ... drive request issued with bearer token token=Secret(***redacted***)
ERROR ... auth failure for token ***redacted***
```

- `Debug` of the Secret rendered `Secret(***redacted***)` — canary absent, no `ya29`.
- `Display` rendered `***redacted***` — canary absent.
- `tracing` capture (max level TRACE, both `?secret` structured field and `{}` interpolation)
  contained **no** byte of the canary.
- The public `DriveError::Api` rendering carried no `Bearer` and no canary.

No panics occurred — every scenario ran to completion.

### Item 7 — end-to-end COMMIT through interpreter + bridge

Routed through `Interpreter::with_defaults(DriverRegistry.with("drive", bridge))` and committed:

- **Trash COMMIT**: `outcome.is_complete()`, `applied_ids() == [NodeId(0)]`, ledger length **1**,
  and the mock recorded exactly `Trash { id: "f1" }`.
- **Upsert COMMIT** (content replace by id): complete, recorded exactly
  `UpdateContent { id: "f1", mime: "application/json", len: 2 }`.

Both writes flowed through `commit` with a granted `CapabilitySet`, proving the bridge + ledger
path end-to-end for a write.

## Concern (Critical Review Policy)

**Concern (low severity, business-traceability):** the t19 `GoogleApiClient` (the real
account→client binding) is constructed entirely upstream of this driver, so the *external*
multi-account proof here is structural — "driver A routes to client A, B is untouched" — rather
than an exercise of the t19 resolution ladder picking the right account from an email. The
driver is correctly account-agnostic, so this is the right boundary for t21; but a stakeholder
auditing "did account X's op really use account X's token" must trace through t19's resolution
test, not this driver's tests.

**Proposal:** when t19's account-resolution lands its own E2E, add one cross-cutting scenario
that resolves two accounts by email through the t19 store, builds a `GoogleApiDriveClient` per
resolved account, and asserts an op on the account-X driver carried the account-X bearer (via the
recorded `HttpRequest`). That closes the business-level "right account, right token" loop that
this driver-scoped test deliberately leaves to t19.

## Verdict

**E2E approved.** All 7 checklist items PASS. The two BLOCKING gates are clear: a bare `REMOVE`
trashes (never permanently deletes; permanent delete requires the explicit `hard_delete` flag),
and the canary token never appears in any Debug/Display/error/log channel. PREVIEW performs zero
I/O, COMMIT drives the ledger + recorded calls end-to-end through the interpreter + bridge, the
lossy `name contains` term is truthfully kept as a local residual, and the `test-util` feature
lets an external consumer seed the mock — confirming the public surface is consumable as
specified.
