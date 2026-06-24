# Coding Phase E2E — Planner — t16 Local filesystem driver

Author: Planner (Progressive)
Role: E2E / external-interface testing only (no code review, no production code)
Target: t16 — `qfs_driver_local::LocalFsDriver` + runtime bridge end-to-end
Date: 2026-06-23

## Method

Validated t16 strictly as an **external consumer**: a throwaway binary crate
(`/tmp/t16-e2e`, own `[workspace]`, path-deps on `driver-local`, `runtime`, `driver`,
`plan`, `types`, `codec`, plus `tokio` + `tempfile`; no production code; removed after
the run). Every root is a `tempfile::tempdir()` — no real user files touched, no network.
Each scenario drives the public API (`LocalFsDriver`, `fs_core`, `local_apply_driver`,
`Interpreter::commit`, `CapabilitySet`, builtin codecs) and asserts against the **real
temp-dir state** afterward.

The full run completed with **no panics**.

## Per-item results

### Item 1 — Listing / scan — PASS
Seeded `alpha.txt` (4 bytes), `beta.json`, and a `sub/` dir containing `nested.txt`.

Scan output (`scan_dir("/local")`):
```
scan /local -> ["alpha.txt", "beta.json", "sub"]
  name=alpha.txt   path=/local/alpha.txt   size=   4  is_dir=false
  name=beta.json   path=/local/beta.json   size=   2  is_dir=false
  name=sub         path=/local/sub         size=   0  is_dir=true
glob /local/**/*.txt -> ["alpha.txt", "nested.txt"]
```
- Rows reflect the real entries: names/sizes correct, `sub` flagged `is_dir=true`,
  top-level scan is one level (nested file not surfaced at top).
- Order is deterministic (sorted by VFS path; re-sort is a no-op).
- Recursive glob `**/*.txt` correctly descends into `sub/` and finds `nested.txt`.
- `describe()` schema exposes `name`/`size`/`is_dir` columns.

### Item 2 — Read + codec (bytes -> rows) — PASS
Wrote a 2-element JSON array to `/local/people.json`, read it via `fs_core::read_blob`
(bytes roundtrip exact), then decoded with the **registered** `json` codec from
`qfs_codec::builtin_codecs()`:
```
decoded 2 rows; columns: ["age", "name"]
```
Two typed rows, typed columns `name` and `age` present. Driver holds no codec code — it
only supplied bytes; the codec is the bytes->rows boundary. Confirmed identically through
the concrete `JsonCodec`.

### Item 3 — End-to-end COMMIT — PASS
Plan: `#0 Upsert(write content) -> #1 Upsert(copy) -> #2 Remove(original)`, with
dependency edges `#0->#1->#2`. Registered `local_apply_driver(&driver)` under
`DriverId("local")`; granted only `{Upsert, Remove}` on `local`; ran `Interpreter::commit`.
```
topo order: [#0, #1, #2]
ledger:
  #0 UPSERT -> applied
  #1 UPSERT -> applied
  #2 REMOVE -> applied
final fs state: orig.txt exists=false  copy.txt exists=true  copy content="hello-qfs-e2e"
```
- Topo order honored; ledger emitted in topo order; every leg `applied`.
- Real temp-dir final state: original gone, copy present, content byte-correct.

### Item 4 — Capability + sandbox — PASS
- **4a (read_only write denied):** `LocalFsDriver::read_only` mount rejected an `Upsert`
  with a structured terminal effect error (`code=terminal`); **no file written**.
- **4b (apply-time cap gate):** writable mount, but `CapabilitySet::none()` (no grant) —
  interpreter denied the leg at apply time (`status=failed`); **no file written**.
- **4c (relative escape):** `read_blob("/local/../../etc/passwd")` rejected with a
  structured sandbox error: `code=outside_sandbox`, message
  `path "/local/../../etc/passwd" resolves outside the sandbox root`. No read performed.
- **4d (symlink escape):** seeded `secret.txt` OUTSIDE the root, symlinked it inside the
  mount, attempted a read through the link. Rejected with `code=outside_sandbox`; the
  out-of-root secret was **NOT** returned. No read/write escaped the root.

Sandbox-rejection error (representative):
```
path "/local/../../etc/passwd" resolves outside the sandbox root   (code=outside_sandbox)
path "/local/escape_link"      resolves outside the sandbox root   (code=outside_sandbox)
```

### Item 5 — cp/mv no-data-loss — PASS
- **5a (successful mv):** `mv m.txt -> moved.txt` removed the source **only after** the
  verified copy: `src-gone=true`, `dst` byte-identical (`move-me`), affected=7.
- **5b (failed copy):** copy from a non-existent source failed (`terminal`); no `dest`
  written and the unrelated `present.txt` stayed intact — no data loss on the error path.
- **5c (failed mv deletes nothing):** a Move whose source is missing fails at
  copy->verify before any unlink; decode classified it as `Move` (irreversible);
  the bystander file stayed intact and no destination appeared. Source-after-verify
  ordering means a failed copy never destroys data.

## Concern + proposal (Critical Review Policy)

- **Concern (business/operational):** the sandbox correctly rejects an escaping symlink
  with `OutsideSandbox`, but at the runtime boundary a `read_only` capability denial and
  a sandbox escape **both collapse to `code=terminal`** once they pass through
  `apply_shared` (only the direct `fs_core`/`LocalError` path preserves the distinct
  `outside_sandbox` / `capability_denied` codes). For an AI-consumable, auditable runtime
  (RFD §5/§10), an operator triaging a failed COMMIT cannot tell "I lacked permission"
  from "I tried to escape the sandbox" from the ledger alone.
- **Proposal:** preserve the structured `LocalError` discriminant across the
  `SharedApplier` -> `EffectError` reduction — e.g. map `OutsideSandbox` and
  `CapabilityDenied` to dedicated `EffectError` variants (or carry a stable `qfs.*` code)
  so the audit ledger distinguishes a sandbox breach attempt from a plain capability
  denial. This is a refinement, not a blocker — the security behavior is already correct
  (no escape, no write); only the surfaced error *class* in the runtime ledger is lossy.

## Verdict

All five items PASS. No panics. No read or write escaped the sandbox root (the symlink
and `..` escapes were both rejected and the out-of-root secret was never returned).

**Overall: E2E approved** (with the one non-blocking observation above on preserving
the structured error class through the runtime bridge).
