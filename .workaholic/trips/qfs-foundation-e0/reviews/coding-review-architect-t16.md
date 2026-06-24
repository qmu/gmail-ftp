# Coding Review (Architect) — t16 Local FS driver + Driver→ApplyDriver bridge

Author: Architect
Reviewed commit: `eb69ff8` (branch `work-20260622-230954`)
Scope: `crates/driver-local/src/{lib,fs_core,effect,applier,error,row}.rs`,
`crates/driver-local/tests/{e2e_commit, src/tests}.rs`, `crates/runtime/src/bridge.rs`,
`crates/cmd/tests/dep_direction.rs`, `crates/plan/src/apply.rs`, `ARCHITECTURE.md`, Cargo.toml's.
Mode: analytical review only (no cargo/test execution).

## Decision: Approve with minor suggestions

This is a clean, well-factored first driver. The bridge and the confinement-test narrowing
are the right shapes and will scale to the next 11 drivers. The security guard is sound for
its threat model. Two non-blocking suggestions on the confinement rule's durability and one
documentation gap, plus three smaller observations. No defect that would mislead the next
drivers — hence not a revision request.

---

## 1. Confinement-test narrowing (the most important seam) — APPROVE the rule, harden the form

The narrowing of `runtime_is_confined_to_plan_and_types::(b)` from "no crate may depend on
`qfs-runtime`" to "no **pure-spine** crate may, but `qfs-driver-local` and `qfs` may" is the
**correct** rule. The protected property is *"tokio never enters the dependency closure of
`qfs-plan`/`qfs-types`/`qfs-driver`/`qfs-codec`/`qfs-txn`."* That property still holds, because:

- Direction (a) is unchanged and still pins `qfs-runtime`'s own up-edges to `{plan,types,txn}`,
  and (a') keeps `qfs-txn` pure — so tokio's *source* is still exactly one crate.
- The admitted consumers (`qfs-driver-local`, `qfs`) are **leaves**: nothing depends back onto
  them, so the `→ qfs-runtime` edge flows up out of the runtime into a sink and dead-ends.
  tokio cannot transit a leaf back into the spine.
- The pure-side purity is independently guarded by `qfs-plan`'s own dep-closure purity test
  (referenced in `runtime/Cargo.toml`), so this test is the *structural counterpart*, not the
  sole line of defense. Defense in depth is intact.

So the invariant genuinely holds and is still mechanically asserted. Good.

**Concern (durability — will it erode to "anything can depend on runtime"?):** the rule is
currently an **allowlist of crate names** (`runtime_consumers_allowed = ["qfs-driver-local",
"qfs"]`). With 11 more driver crates coming (`qfs-driver-s3`, `-drive`, `-gmail`, …), every new
driver must remember to append its name here, and the *only* thing stopping a reviewer from
adding `qfs-core` to that array is discipline. The allowlist does not encode *why* a crate is
admitted (leaf-ness), so it silently degrades into "the list of crates we waved through."

**Proposal (durable form):** keep the allowlist for the explicit-intent signal, but add the
**structural property that actually makes admission safe** as a separate, generic assertion:
*every crate that depends on `qfs-runtime` (other than `qfs-runtime` itself) must be a sink —
no other workspace crate depends on it.* That is the real invariant ("runtime consumers are
leaves"), it needs no per-driver edit, and it catches the dangerous case (someone makes
`qfs-core` depend on runtime) automatically because `qfs-core` is not a sink. Sketch:

```rust
// Generic: any qfs-runtime consumer must be a leaf (nothing depends back onto it),
// so tokio dead-ends in it and cannot transit back into the spine.
for (pkg, deps) in &graph.direct_deps {
    if pkg == "qfs-runtime" || !deps.iter().any(|d| d == "qfs-runtime") { continue; }
    let has_dependent = graph.direct_deps.iter()
        .any(|(other, od)| other != pkg && od.iter().any(|d| d == pkg));
    assert!(!has_dependent,
        "confinement: {pkg} depends on qfs-runtime but is NOT a leaf — \
         a dependent could transit tokio back into the spine");
}
```

Keep the named allowlist too (it documents *which* leaves we expect and catches an
unintended new runtime consumer even if that consumer happens to be a leaf today). The two
assertions together are belt-and-suspenders: the allowlist pins identity, the leaf-property
pins safety. With both, the rule scales to 11 drivers without eroding — and a future
`qfs-driver-s3` only needs the one-line allowlist append, with the leaf check guaranteeing the
append was actually safe. This is the form I'd want frozen before driver #2 lands.

---

## 2. The bridge (`PlanApplierBridge` / `SharedApplier`) — APPROVE, right reusable adapter

- **`spawn_blocking` adapter** is the correct reusable shape for *synchronous-I/O* drivers
  (local FS, and any future SDK that is blocking). The module doc is honest that the apply leg
  is blocking and must not stall a worker. For genuinely *async* cloud drivers (s3/drive over
  reqwest) the bridge is still usable (an async body inside a blocking task is wasteful but
  correct), but those drivers will more likely implement `ApplyDriver` directly rather than go
  through `SharedApplier`. That is fine — the bridge is offered as the sync-driver convenience,
  not a mandate. Worth stating explicitly in the bridge doc so driver authors don't assume they
  *must* route through it.

- **`SharedApplier` (`&self`, `Send+Sync`, stateless-per-call)** is the correct contract, and
  the rationale (a real applier owns no in-process mutable accumulator; the stateful test
  `RecordingApplier` is deliberately *not* bridgeable) is exactly the right line to draw. The
  dual `PlanApplier::apply(&mut self)` + `SharedApplier::apply_shared(&self)` views delegating
  to one `&self` core in `LocalApplier` is a clean way to satisfy both the t09 synchronous seam
  and the runtime bridge without divergence.

- **t10 semantics preserved:** batching is inherited via `ApplyDriver`'s default
  `apply_batch` mapping over `apply_one` (the bridge doesn't override it — correct, FS has no
  native batch endpoint); capability re-check happens in the interpreter *before* dispatch
  (`interpreter.rs` step 2), so the bridge never sees a denied effect; failure classes
  propagate honestly (terminal vs retryable) via `EffectError`. Good.

- **Join-failure handling is sound:** `spawn_blocking(...).await` maps `Err(join_err)` to
  `EffectError::terminal(...)` rather than unwrapping — no panic path, and *terminal* (not
  retryable) is the right class for a join failure on a possibly-half-applied irreversible leg.
  Note the blocking closure itself cannot panic in the local driver (no unwrap/expect/panic in
  the apply path; the lints forbid it), so `JoinError` here is the pool-shutdown case, correctly
  treated as a non-retryable runtime fault.

**Concern (node reconstruction round-trip):** `node_from_input` rebuilds the `EffectNode` from
the flattened `EffectInput`, preserving `id/kind/target/irreversible/args`, but **drops the
`est_affected`** the planner computed (it substitutes `Affected::Unknown` when `args.rows` is
empty, else relies on `with_args`). For the local driver this is harmless (the applier reports
the *true* affected count back, and `commit`/the ledger use that). But it is a subtle
lossy-projection seam: a future driver that consults `node.est_affected` inside `apply_shared`
(e.g. to pre-size a batch buffer) would see a degraded estimate. **Proposal:** either carry
`est_affected` through `EffectInput`/`node_from_input` so the reconstructed node is faithful,
or document on `node_from_input` that `est_affected` is intentionally not round-tripped and
the apply leg must derive its own count. The honest-comment is the minimum; carrying it through
is the cleaner fix and costs one field. Non-blocking because no current driver reads it.

---

## 3. FS driver security — APPROVE, guard is safe for its threat model

- **Symlink-escape guard** (`Sandbox::resolve`): the design — lexical `..`/absolute-jump
  rejection *before* any I/O (`normalise`), then canonicalize the longest existing ancestor
  (the path itself if it exists, else its parent) and re-check `starts_with(root)` — is the
  right two-stage shape and closes the obvious symlink-escape (test
  `sandbox_rejects_symlink_escape` confirms a link pointing outside is rejected). `..` bypass is
  blocked lexically with no I/O (`sandbox_rejects_parent_escape_with_no_io`). Sound for the
  stated threat model (a confined, cooperative-ish mount root).

  *Two honest caveats to record (not defects at this layer):* (i) the parent-only canonicalize
  for a non-existent write target means an intermediate symlink *deeper than the immediate
  parent but inside an existing chain* is covered, but a symlink created *between* resolve and
  the subsequent `create_dir_all`/`rename` is a classic TOCTOU window — acceptable here because
  the root is a least-privilege sandbox and the ticket explicitly defers stronger guarantees,
  but the s3/drive tickets should not copy this as a *security* boundary for hostile inputs.
  (ii) `Sandbox::new` does `canonicalize(root).unwrap_or(root)`: if the root itself does not
  exist or isn't canonicalizable, the un-canonicalized path is kept and a later symlinked root
  could weaken the prefix check. The doc says "non-existent root makes every resolve fail
  closed," which is true for I/O ops but the *containment* `starts_with` compares against a
  non-canonical root in that edge case. **Proposal:** note this in the `Sandbox::new` doc (root
  is expected to exist and be canonical; callers constructing over a missing root get fail-closed
  I/O but a weaker prefix check) — or fail construction explicitly. Minor; the production caller
  always passes an existing mount root.

- **read_only narrowing enforced at the gate:** yes, twice — `caps()` narrows to `{Ls}` so the
  parse-time `check_capability` rejects mutating verbs (`writable_mount_supports_blob_verbs_...`
  test), *and* `LocalApplier::apply_effect` re-checks `read_only && effect_is_mutating` before
  any I/O, returning `CapabilityDenied` with no filesystem touch (`read_only_applier_denies_...`
  + the e2e `commit_denies_write_on_read_only_mount`). Defense in depth is correct.

- **Errors secret-free:** confirmed. `LocalError::from_io` reduces `io::Error` to `(path, kind)`
  — never the message body — and `Io`/`VerifyFailed`/`NotFound` carry only path + byte counts.
  No arm renders file contents. The `Display` strings are all path/count/verb only. Good. (The
  path *itself* is surfaced; that is intended and matches the structured-error contract.)

- **cp/mv verify-before-delete is genuinely no-data-loss:** `copy_verify` writes to a sibling
  temp, `sync_all`, asserts `written == expected` *before* `rename` (and removes the temp +
  errors `VerifyFailed` on mismatch), and only then publishes. `LocalEffect::Move` calls
  `copy_verify` and unlinks the source **only after** it returns `Ok` (`applier.rs` Move arm +
  `applier_move_deletes_source_only_after_verify` test). A crash mid-move leaves the source
  intact. This is the recoverable shape the cloud drivers should reuse. The one gap vs. the
  ticket's wording ("verify size+hash"): verification is **size-only** (the incremental byte
  count vs. source `len()`), not a content hash. Size-equality after a fresh stream-copy is a
  reasonable integrity check for a same-host copy, but it will **not** catch silent bit-flips or
  a mid-stream corruption that preserves length. **Proposal:** either compute a rolling hash in
  the copy loop (the buffer is already in hand — near-free) and compare, matching the ticket and
  giving the cloud drivers a true end-to-end checksum pattern (where ETag/CRC32C matters far
  more), or revise the ticket/docs to say "size-verify" and defer content-hash to the cloud
  tickets. I lean toward adding the hash now precisely *because* this is the reference the next
  11 drivers copy — the cloud copy story is where a length-only check is genuinely unsafe.

---

## 4. Contract fit / pattern — APPROVE

- **cp/mv as `Upsert` + `src`-column (not new verbs)** is the right way to keep the closed core
  `EffectKind` set intact. The decode in `LocalEffect::decode_write` is clean: a `SRC_COL` text
  value ⇒ Copy, or Move iff `node.irreversible`; otherwise a `CONTENT_COL` blob ⇒ Write;
  otherwise a structured `DecodeError` ⇒ terminal effect failure (never a panic). This is a
  **reusable pattern** for s3/drive: a cross-mount cp is the same `Upsert(dst) + src=/local/...`
  shape, with the destination driver's applier interpreting `SRC_COL`. The well-known column
  constants (`CONTENT_COL`/`SRC_COL`) being `pub` and exported is the right call so the cloud
  drivers and the evaluator agree on the wire shape. **Minor seam observation:** the
  Copy-vs-Move distinction rides entirely on `node.irreversible`. That couples "is this a move"
  to "is this leg irreversible," which is true today but conflates two concepts — a future
  reversible-move (copy-with-source-tombstone) couldn't be expressed. Worth a one-line doc on
  `decode_write` that the irreversible flag is *the* move discriminator by current contract, so
  the next driver author doesn't invent a second signal. Non-blocking.

- **Codec boundary (t15) faithful:** confirmed. The driver holds **zero** format code:
  `fs_core::read_blob` returns raw bytes, and the codec decode happens entirely outside
  (`blob_decoded_via_codec_becomes_rows` constructs `JsonCodec` in the test, not the driver).
  `qfs-codec` is a dep only for the e2e/test composition, not used in the driver's own apply
  path. The `bytes ↔ rows` separation is clean and driver-identity-independent, exactly per the
  t15 boundary.

---

## 5. Spine — APPROVE

- **`qfs-driver-local` edges acyclic:** deps are `{qfs-driver, qfs-plan, qfs-types, qfs-codec,
  qfs-runtime}` + tokio/thiserror. All point toward more-foundational crates; nothing depends
  back onto `qfs-driver-local` (it is a leaf consumer, which is exactly what admits its
  `→ qfs-runtime` edge under the narrowed test). Acyclic. Good.

- **`ApplyError::new` additive/sound:** the constructor on the `#[non_exhaustive]`
  `qfs_plan::ApplyError` is purely additive — it preserves the non-exhaustive guarantee while
  letting the out-of-crate `LocalApplier::apply` (PlanApplier impl) build a failure without a
  struct literal. Mirrors the existing `AppliedEffect::new`. The reason is reduced from the
  structured `LocalError` via `to_string()` so no driver type leaks into `qfs-plan`. Sound.

**Documentation gap (worth fixing now):** `ARCHITECTURE.md`'s "Dependency spine" diagram
(lines 30–56) does **not** list `qfs-driver-local` or its `→ qfs-runtime` edge, even though the
dep-direction test now explicitly encodes it as the first admitted runtime consumer. Since this
edge is the precedent the next 11 drivers follow, the spine diagram should gain a line like
`qfs-driver-local → { qfs-driver, qfs-plan, qfs-types, qfs-codec, qfs-runtime }  (first concrete
driver; LEAF runtime consumer — bridges sync PlanApplier → async ApplyDriver; tokio dead-ends
here)`. Keeping the prose and the mechanical test in sync is exactly the translation-fidelity
the spine doc exists to provide. **Proposal:** add the line before driver #2 lands.

---

## Summary of suggestions (all non-blocking)

1. **Confinement (most important):** add the generic "runtime consumers must be leaves" assertion
   alongside the named allowlist, so the rule encodes *why* admission is safe and scales to 11
   drivers without eroding. Freeze this before driver #2.
2. **Bridge:** carry `est_affected` through `node_from_input` (or document it as intentionally
   dropped) so the reconstructed node is faithful for future drivers that read it.
3. **Security:** make cp/mv verify a content hash (not size-only) now, since this is the
   reference the cloud copy story copies — or align ticket/docs to "size-verify".
4. **Security:** doc the `Sandbox::new` non-canonical-root edge case (or fail construction).
5. **Pattern:** one-line doc that `node.irreversible` is *the* copy-vs-move discriminator.
6. **Docs:** add `qfs-driver-local` to the ARCHITECTURE.md spine diagram.

The verdict on the two seams the next 11 drivers inherit: the **confinement narrowing is the
right rule** (harden its *form* per #1), and the **bridge pattern is the right reusable adapter**
(tighten the `est_affected` round-trip per #2). Neither is misleading as-is; both are
worth firming before the second driver lands.
