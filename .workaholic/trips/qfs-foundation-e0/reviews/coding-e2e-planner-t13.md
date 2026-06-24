# Coding-Phase E2E — Planner — t13 (Driver contract trait)

Author: Planner
Mode: E2E / external-interface testing (no code review, no production code)
Target ticket: t13 — Driver contract (trait): archetype, schema, capabilities, procs, pushdown, prelude, @version

## Method

Wrote a throwaway external crate at `/tmp/t13-extdriver` with its **own `[workspace]`** and
absolute path-deps on `crates/{driver,types,plan}`. It defines `struct MyDriver` (a third
party who has never seen qfs internals) implementing `qfs_driver::Driver` using **only the
public API**, plus an out-of-crate `MyApplier: PlanApplier`. This proves the contract is
implementable from outside the qfs workspace. The binary exercises every required scenario
and prints PASS/FAIL per item. It compiled and ran green (after one ergonomics workaround,
see Finding F1). The crate was removed after testing.

Overall verdict: **E2E approved** (with one non-blocking ergonomics finding, F1).

## PASS/FAIL per item

| # | Item | Result |
|---|------|--------|
| 1 | Store as `Arc<dyn Driver>`; call `mount`/`describe`(valid→typed schema; bad→`CfsError`)/`capabilities`/`procedures`/`pushdown`/`prelude`/`version_support` | PASS |
| 2 | `check_capability` supported verb passes; unsupported → structured `CfsError::UnsupportedVerb{supported}`; `resolve_proc` declared resolves, unknown → structured error | PASS |
| 3 | `from_vfs(p.to_vfs()) == p` lossless round-trip; `parse`/`try_from_vfs` reject empty + non-absolute structurally, no panics | PASS |
| 4 | Driver-backed `PlanApplier` builds success via `AppliedEffect::new(...)`; COMMIT loop reports applied effects | PASS |
| 5 | `describe` JSON: typed schema + archetype render for `-json DESCRIBE` | PASS |

### Item 1 — object-safety + full contract surface
`let driver: Arc<dyn Driver> = Arc::new(MyDriver::new());` compiles and stores fine (trait is
object-safe; registry-usable). Every method callable through the trait object:
- `mount() == "/my"`.
- `describe(/my/notes)` → `archetype=AppendLog`, typed 2-column schema (`qfs_types::Schema`).
- `describe(/elsewhere/x)` (bad path) → `Err(CfsError::InvalidPath)`, `code="invalid_path"`, no panic.
- `capabilities(/my/notes)` → `supported=[SELECT, INSERT]`.
- `procedures()` → 1 declared (`publish`); `pushdown()` → `Partial{..}`; `prelude()` → `[PUBLISH→my.publish]`.
- `version_support`: per-node — `notes=Snapshot`, `files=None`.

### Item 2 — capability gate + proc resolution (the AI-facing seam)
- `check_capability(driver, /my/notes, Verb::Select)` → `Ok` (supported verb passes the gate).
- `check_capability(driver, /my/notes, Verb::Update)` → structured `CfsError::UnsupportedVerb`
  a consumer branches on by field, not prose. Structured dump (as an AI would consume it):
  ```json
  {"code":"unsupported_verb","path":"/my/notes","verb":"UPDATE","supported":["SELECT","INSERT"]}
  ```
- `resolve_proc(driver, "publish")` → resolves; `irreversible=true`, `requires_scopes=["my.publish"]`.
- `resolve_proc(driver, "nuke")` → `CfsError::UnknownProcedure`, `code="unknown_procedure"`,
  message `unknown procedure: nuke`. Undeclared `CALL` is rejected structurally.

### Item 3 — Path ↔ VfsPath adapter
- Round-trip `Path::from_vfs(p.to_vfs()) == p` holds byte-for-byte for `/my/notes`,
  `/my/files/a.md@v2`, `/git/repo@ref/x` (lossless across the effect boundary).
- `Path::try_from_vfs(VfsPath::new(""))` → `InvalidPath` (empty); 
  `Path::try_from_vfs(VfsPath::new("relative/x"))` → `InvalidPath`,
  message `invalid path "relative/x": path is not absolute (must start with '/')`.
- `Path::parse("nope")` and `Path::parse("")` both → `InvalidPath`. No panics anywhere.

### Item 4 — out-of-crate driver-backed COMMIT loop (t09)
`MyApplier` (defined outside the workspace) implements `PlanApplier::apply` building its
success value via the additive `AppliedEffect::new(node.id, 1)` — the `#[non_exhaustive]`
struct literal is correctly NOT reachable out-of-crate, and the constructor fills the gap.
Built a one-node plan: `CALL my.publish` tagged `irreversible` (from the declared proc), via
`EffectNode::new(NodeId(0), EffectKind::Call(ProcId::new("my.publish")), target).irreversible(true)`,
`Plan::leaf(node)`. `commit(&plan, &mut applier, |_|{})` returned:
- `report.is_complete() == true`, `report.applied == [AppliedEffect{id:#0, affected:1}]`,
  and the applier's own call log recorded `[NodeId(0)]`.

This proves an **out-of-crate driver can complete the t09 COMMIT loop** with no I/O / no creds.
The trait's `applier()` seam is also reachable through `Arc<dyn Driver>`.

### Item 5 — `-json DESCRIBE` projection (external driver's own NodeDesc)
`serde_json::to_string_pretty(&driver.describe(/my/notes))` renders archetype + typed schema:
```json
{
  "archetype": "append_log",
  "schema": {
    "columns": [
      { "name": "ts",   "ty": "Timestamp", "nullable": false, "provenance": { "driver": null, "source_col": null } },
      { "name": "note", "ty": "Text",      "nullable": false, "provenance": { "driver": null, "source_col": null } }
    ]
  }
}
```
`procedures()` JSON (declared, irreversible proc renders for AI introspection):
```json
[
  {
    "name": "publish",
    "params": [ { "name": "channel", "ty": "Text" } ],
    "irreversible": true,
    "returns": null,
    "requires_scopes": [ "my.publish" ]
  }
]
```

## Findings

### F1 (non-blocking ergonomics) — `Capabilities` has no out-of-crate constructor/builder
`Capabilities` is `#[non_exhaustive]` with all-public fields but ships **no** constructor or
builder. From outside the defining crate, a struct expression — even with `..default()` — is a
hard compile error (`E0639: cannot create non-exhaustive struct using struct expression`). The
in-crate fixture driver and the snapshot tests sidestep this only because they are *in-crate*
(struct literals are legal there), so the contract's most-used per-node return type is in fact
**not** constructible the way the reference impl demonstrates it.

Impact: every real E4 driver's `capabilities(path)` (the per-node hot path, called once per
verb gate) is forced into the awkward `let mut c = Capabilities::default(); c.select = true; …`
mutation pattern. It compiles and works (the E2E worked around it this way), so it is not a
blocker, but it is a contract ergonomics wall the reference tests do not reveal.

Proposed fix (business framing — lower the cost of writing the next 10+ drivers, which is the
whole "zero keywords, just declare" value prop): add a small builder mirroring `ProcSig`'s
pattern, e.g. `Capabilities::none()` plus chainable `.select()/.insert()/…` or a
`Capabilities::with([Verb::Select, Verb::Insert])` constructor from a verb list. Either keeps
`#[non_exhaustive]` intact while making the declaration ergonomic out-of-crate and matching how
`NodeDesc::new`, `Param::new`, `ProcSig::new`, and `AliasFn::new` already smooth over
`#[non_exhaustive]` for external drivers. (All four of those constructors worked flawlessly in
the E2E — `Capabilities` is the one DTO that was overlooked.)

### Positive observations
- The four owned-DTO constructors provided for external use (`NodeDesc::new`, `Param::new`,
  `ProcSig::new` + builders, `AliasFn::new`) made the rest of the driver trivial to write.
- `AppliedEffect::new` correctly enables an out-of-crate `PlanApplier` to report success
  without the struct literal — the t09/t13 seam is genuinely third-party-usable.
- Structured errors (`InvalidPath`, `UnsupportedVerb`, `UnknownProcedure`) all expose a stable
  `code()` and field-addressable data — a consumer/AI can branch without prose-parsing.
- Per-node behavior (capabilities, archetype, version_support keyed on path) was natural to
  implement and the gate consumed it correctly.

## Verdict

**E2E approved.** All five required items PASS from a genuine out-of-workspace driver. One
non-blocking ergonomics finding (F1: add a `Capabilities` builder/constructor) is recommended
for E4 driver-author velocity; it does not block t13 acceptance.
