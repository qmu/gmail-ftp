# Coding Review (Architect) — t13 Driver contract trait

- Reviewer: Architect (Neutral / structural bridge)
- Target: t13 — Driver contract trait (commit `847b636`, branch `work-20260622-230954`)
- Scope: analytical review only (no cargo/test execution)
- Files read: `crates/driver/src/{lib,path,error}.rs`, `crates/plan/src/{apply,node,ids}.rs`,
  `crates/core/src/{lib,registry}.rs`, `crates/cmd/tests/dep_direction.rs`, `ARCHITECTURE.md`,
  `crates/{driver,plan,core}/Cargo.toml`, ticket t13.

## Decision: Approve with observations

The contract is structurally sound and is the right linchpin for E4 drivers + the interpreter.
`Driver: Send + Sync` is object-safe, the registry holds `Arc<dyn Driver>` and exercises it, the
purity invariant is enforced at the type level (no `&mut self`/future/executor on the
introspective half; `applier()` is the lone impure seam), the carry-overs (NodeSchema absorption,
Path↔VfsPath adapter, `AppliedEffect::new`) are correctly resolved, and the new spine edges
(`qfs-driver → qfs-types`, existing `qfs-driver → qfs-plan`) are acyclic and locked by
`dep_direction.rs`. No vendor type appears anywhere on the contract surface; `requires_scopes` is
an owned-label hint with auth correctly deferred to E5. I am approving with observations rather
than requesting revision because none of my concerns is a *structural defect in what was built* —
they are deliberately-deferred seams that E4/t10/t14 will build *on top of*, plus two
maintenance-fidelity risks worth recording now so drivers are not built against an ambiguous
contract.

## What is right (structural confirmation)

1. **Object-safety / `Arc<dyn Driver>`** — confirmed: no generic methods, no `Self`-returning
   methods, no associated consts on the trait. `driver_is_object_safe` and the `MountRegistry`
   (`BTreeMap<String, Arc<dyn Driver>>`) both construct and call through the trait object. G2 holds.
2. **Purity guard (G3)** — the introspective half returns owned DTOs or `qfs_types::Schema` by
   value/ref; none takes `&mut self` or returns a future. The only `&mut self` in the whole effect
   path is `PlanApplier::apply`, reached *only* through `applier()`. `_commit_seam_reserved_for_e2`
   keeps `Plan` referenced and documents that the apply loop is the runtime's, not the driver's.
   This is a genuine type-level proof, not a comment.
3. **Per-node (path-keyed) model** — `describe`, `capabilities`, `version_support` all take
   `&Path`; the fixture proves a single driver returning Blob / Relational / Append on
   `/fix/blob`, `/fix/rel`, `/fix/log`. This is exactly the git-style multi-archetype case the
   ticket called the "genuinely tricky part," and the contract handles it without a per-driver
   trait change. `procedures()` and `pushdown()` are driver-global, which is the right call —
   procs are namespaced by mount and pushdown is a backend property, not a node property.
4. **Carry-overs**
   - *NodeSchema absorption*: correct. `NodeDesc { archetype, schema: qfs_types::Schema }` means
     `DESCRIBE` and type-checking (t05) share one schema; an adapter would have created a second
     schema type to keep in sync forever. The new `qfs-driver → qfs-types` edge is acyclic because
     `qfs-types` is a true leaf (asserted in `dep_direction.rs`).
   - *Path↔VfsPath adapter*: genuinely lossless (`from_vfs(p.to_vfs()) == p`, byte-for-byte,
     proven by `to_vfs_then_from_vfs_round_trips_losslessly`) and the validating door
     (`try_from_vfs`/`parse`) rejects empty/relative with a structured `InvalidPath`. Two distinct
     types in two crates is the correct way to keep the spine acyclic (`qfs-plan` cannot name
     `Path`); the explicit adapter is the only crossing, never a vendor type.
   - *`AppliedEffect::new`*: additive (a `#[must_use]` ctor over the existing fields), preserves
     `#[non_exhaustive]`, and is what an out-of-crate (E4) applier needs to report success without
     a struct literal. Sound.
5. **Spine / G6** — `dep_direction.rs` now asserts both `qfs-driver → qfs-types` and
   `qfs-driver → qfs-plan` and re-asserts `qfs-types` leaf-ness, so the new edges are mechanically
   locked. No vendor edge anywhere. Secrets stay out: `Target`/`VfsPath`/`requires_scopes` carry
   identity and labels only.
6. **Pushdown shape for t14** — `None | Partial{where_,project,limit,order,join} | Full` is a
   sufficient *declaration* surface for t14's planner to branch on (the ticket scopes
   planning/collapse out of t13). Good enough to consume now.

## Observations (carry forward; none blocks t13)

### O1 — `mount() -> &str` vs the ticket's `id() -> DriverId`; registry is exact-match, not longest-prefix (structural seam for t10)
The ticket's Key-components sketch specified `id(&self) -> DriverId`, a
`DriverRegistry { Map<DriverId, Arc<dyn Driver>> }`, and `resolve(path) -> (Arc<dyn Driver>, sub-path)`
via **longest-mount-prefix** matching. What shipped is `Driver::mount() -> &str` and a `qfs-core`
`MountRegistry` keyed by mount string with **exact-match** `resolve(mount)` and **no** path→(driver,
sub-path) split. The gate fns (`check_capability`/`resolve_proc`) take an *already-resolved*
`&dyn Driver`, so they sidestep the resolution question entirely.

This is acceptable for t13 (the trait + a gate are what t13 owes; the mount-prefix router is
arguably t10/addressing's job), but it is a real divergence the team should record explicitly so
t10 is not surprised. Two concrete structural points for whoever builds the router:
- The `Driver` trait exposes **no** `DriverId` accessor, yet `Target`/`EffectNode` are keyed on
  `qfs_types::DriverId`. Something must map a resolved driver → its `DriverId` to build effect
  nodes. Today that mapping is implicit (mount string vs `DriverId` string). **Proposal:** in t10
  (or a fast-follow), either add `fn id(&self) -> DriverId` to `Driver`, or make the router the
  single owner of the `mount-string ↔ DriverId` correspondence and document it — so a driver's
  identity in the registry and its identity in the plan cannot drift.
- Longest-prefix resolution + sub-path splitting is the load-bearing piece that turns a `/git/...`
  path into `(git driver, @ref/path)`. It is *not here yet*. **Proposal:** explicitly file it as a
  t10/addressing carry-over rather than leaving the exact-match `MountRegistry` looking complete.

### O2 — `Verb` enum and `Capabilities` bool-struct are two parallel listings of the same closed set (fidelity risk)
The ticket sketched `Capabilities { verbs: BitFlags<Verb> }`; the impl uses a 9-field bool struct
plus a separate `Verb` enum, bridged by hand in `Capabilities::allows` and `supported_labels`
(`const ALL: [Verb; 9]`). Functionally correct and `#[non_exhaustive]` on both. The risk is purely
fidelity: adding a 10th verb requires edits in *four* synchronized places (the enum, `Verb::label`,
the struct field, `allows`, and the `ALL` array) with no compiler tie between them — a `match` in
`allows` will catch a missing arm, but a forgotten struct field or `ALL` entry will silently
under-report `supported_labels`. **Proposal (non-blocking):** either adopt the ticket's `BitFlags`
(single source of truth) later, or add a `#[test]` asserting `ALL.len()` equals the `Verb` variant
count and that every `Verb` maps to a distinct `Capabilities` field — so drift is caught at test
time. Not required for t13.

### O3 — `PushdownProfile::Partial` has fixed fields; new pushdown dimensions are a breaking change (flag for t14)
The enum is `#[non_exhaustive]` but the `Partial` variant's five bools are not. A backend that can
push down `aggregate`/`distinct`/`group_by` (likely for SQL/D1/GA4-t41) cannot be expressed without
adding a field to `Partial`, which is a breaking change to the variant's pattern. For t13's
*declaration* purpose this is fine, but t14's planner should be designed to read these via accessor
intent, not exhaustive destructuring. **Proposal:** when t14 lands, either give `Partial` a
forward-compatible representation (a capability set / bitflags) or accept that the pushdown
dimension list is itself a frozen-ish vocabulary and enumerate the full intended set now. Record the
decision in the t14 ticket.

### O4 — `mount()` returns `&str` but is not validated/normalized
`MountRegistry::register` keys on `driver.mount().to_string()` with no check that it is absolute /
non-empty / non-overlapping with an existing mount prefix. `Path` has a `validate` (absolute,
non-empty) but `mount()` does not reuse it. Low risk at E0 (registry is empty, drivers are trusted
in-process), but mount strings are the resolution keys. **Proposal:** when the longest-prefix router
lands (O1), validate mount strings on `register` (absolute, no trailing slash, prefix-disjoint) so
the registry cannot accept an ambiguous mount set. Non-blocking for t13.

## Cross-artifact coherence

The contract is faithful to RFD §3/§5/§9 and to model-v1's acyclic-spine and purity guards. The D1
decision (CfsError/Path in `qfs-driver`, re-exported by `qfs-core`) is consistently reflected in
ARCHITECTURE.md, the error module doc, and the re-export list. `qfs-core::lib` re-exports the full
contract surface so consumers see one `qfs_core::*` face — translation fidelity between the layered
crates and the single public surface is intact.

## Required-before-drivers checklist (my structural ask)
Nothing in t13 must change to *merge t13*. Before E4 drivers are built **on** it, the team should
have an answer on record for O1 (driver identity + mount-prefix router ownership) and O3 (pushdown
vocabulary), because those two shape what a driver author writes. O2 and O4 are hardening, not
contract shape.
