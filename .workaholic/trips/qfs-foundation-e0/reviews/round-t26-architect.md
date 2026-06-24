# Round t26 — Architect Analytical Review (git object-model driver)

- Author: Architect (Neutral / structural bridge)
- Status: under-review
- Reviewed: commit `5f5ce18` — `crates/driver-git/` (qfs-driver-git), `docs/adr/0003-git-object-access.md`, `crates/cmd/tests/dep_direction.rs`
- Mode: ANALYTICAL REVIEW ONLY (no test/build/clippy execution)

## Decision

**Approve with observations.** The structure is coherent, the purity invariant holds, the
keyword-clash and CAS-ref and merge-conflict-purity properties are faithfully implemented, and
the dep direction is a clean leaf append. The gix-vs-in-house divergence is **structurally
justified** as a decision shape, but the ADR overstates its correctness evidence: the central
"differential against real git" guard it leans on is **not actually present in the test code**.
That is the one observation that should be addressed (by honest wording or by adding the test),
and it is the load-bearing weakness behind concern (a)/(b) of the headline question.

---

## HEADLINE RULING — gix vs. in-house object reader

**Ruling: the divergence is structurally justified as a decision, but its recorded correctness
evidence is materially weaker than the ADR claims, and the ADR text must be corrected.**

The decision *shape* is sound and consistent with precedent. Taking gix's default-feature
closure (tens of `gix-*` crates + `flate2`/`miniz_oxide`/`sha1`/`crc32fast`) when (i) none of it
is in the offline cargo cache, (ii) the host is at 97% disk, and (iii) the workspace default is
wasm-clean, is exactly the footprint/offline/wasm trade ADR-0001 (winnow) and ADR-0002 (DuckDB)
already resolved the same way. The required surface really is small and closed: inflate a loose
object, parse four object kinds, walk parents/trees, content-address an object about to be
written. The `ObjectDb` trait is a *genuine* reversibility seam (see (c)). On (d), the reasoning
chain is internally consistent with ADR-0002's "heavy capability vs. lean owned-boundary, resolved
on measured facts against a small required surface."

But the four sub-questions split:

### (a) Is hand-rolling SHA-1 + DEFLATE-inflate proportionate? — Qualified yes for SHA-1, guarded-yes for inflate, with a caveat the ADR under-weights.

SHA-1 here is a *content address* over framing the driver itself produces; it is not parsing
untrusted input and not authenticating anything. ~70 lines, pinned by RFC-3174 vectors and the
canonical empty-blob oid. Proportionate; the t22/t25 signing-crypto precedent applies cleanly.

The inflater is a **different risk class** and the ADR's framing ("git's SHA-1 + DEFLATE is the
same situation" as the crypto) elides this. As the headline question correctly flags, **inflate
parses untrusted compressed bytes** — it is an attack surface (back-reference distance, symbol
ranges, code-length overruns), unlike the signing-only crypto. I audited `inflate.rs` for the
classic hazards and it is **defensively correct**: the back-reference guard rejects
`distance > out.len()` (objectdb/inflate.rs:121), `dsym` is range-checked (l.116), reserved
BTYPE=3 is rejected (l.58), stored-block LEN/NLEN is validated (l.73), the code-length repeat
codes 16/17/18 cannot write out of bounds (16 is length-guarded; 17/18 only advance the index and
a final `i != total` rejects an overrun, l.207), the bit reader returns `Corrupt` on EOF rather
than panicking, and the LZ77 overlap copy is byte-at-a-time (correct for `distance < length`).
`#![forbid(unsafe_code)]` is set. So the *implementation* is sound; the residual is that hand-
rolled inflate is inherently higher-risk than a battle-tested decoder, and the ADR should name
that asymmetry rather than equate it with the crypto precedent.

### (b) Is the differential-pinning sufficient correctness evidence? — **No. This is the material finding.**

The ADR's headline guard (ADR-0003 lines 82-90, and the Comparison table's "Correctness guard"
row) states: *"the fixture repo is built by the system `git` at test time, so our reader is
differentially checked against canonical git bytes/oids."* **The test code does not do this.**

- `tests.rs::build_fixture` constructs every object in-memory via
  `LooseObjectDb::insert_object` (which stores **uncompressed framed bytes**). It never shells out
  to the host `git`, and never calls `insert_loose`.
- `LooseObjectDb::insert_loose` + the `zlib_inflate` read branch (`framed()`, objectdb.rs:245)
  — the entire in-house **inflate** path — is **dead in the test suite**. On every fixture read the
  `0x78` probe takes the `else` branch and returns the stored framed bytes verbatim; the inflater
  is never invoked on a real git object.
- The only inflate coverage is two hand-pasted Python-`zlib` vectors in `inflate.rs` tests
  ("hello world" fixed-Huffman + an `abc` stored block). Neither is git output; neither exercises
  a **dynamic-Huffman** (BTYPE=2) block, which is the path real git loose objects of any size use
  and the most complex part of the decoder (`dynamic_trees`).
- The only genuine differential-against-git assertion is the **empty-blob oid** (`frame_and_id`
  + SHA-1), which exercises framing + SHA-1 but **zero** inflate and zero object parsing of real
  bytes.

So the correctness evidence that actually exists is: RFC SHA-1 vectors, the empty-blob oid, and
two synthetic zlib vectors. That is *enough to retire the SHA-1 risk* but **not** the inflate risk
the ADR leans on to justify owning the decoder. The ADR's "differential guard" is, as written,
closer to a fig leaf than the evidence it purports to be — not because the code is wrong, but
because the guard described is not wired up.

**Proposal:** Either (preferred) add a real differential test that builds a tiny repo with the
host `git` (a dev-time fixture, exactly as the ADR describes), reads its `.git/objects/xx/yyy`
**compressed** loose bytes through `insert_loose` → `zlib_inflate` → `parse_*`, and asserts the
recovered oid/payload equals git's — covering at minimum one dynamic-Huffman object (commit a file
big enough to force BTYPE=2). If the host-`git` dependency is undesirable in CI, at least add a
committed real-git loose-object byte vector (a dynamic-Huffman object) as a fixture and inflate it.
Failing both, **correct the ADR**: downgrade the "fixture built by system git" claim to the truth
("differentially pinned on the empty-blob oid + SHA-1/RFC vectors + synthetic zlib vectors; a
real-git loose-object inflate differential is a named follow-up"), so the recorded reasoning is
honest. The ADR is marked "Accepted (locked)" while asserting a guard the code does not implement;
that gap should not be locked in as-is.

### (c) Is the `ObjectDb` seam a genuine reversibility hedge or a fig leaf? — **Genuine.**

`ObjectDb` is a real trait (`read`/`contains`), `Repo` holds `Arc<dyn ObjectDb>`, and no in-house
type (inflate/sha1 internals) crosses it — only owned `Oid`/`RawObject`/`ObjectKind`. A
`GixObjectDb` could implement it behind a feature with zero caller churn, exactly as the ADR
claims and consistent with the ADR-0001/0002 trait-seam precedent. This part is not a fig leaf.
(Minor: `LooseObjectDb` is concrete in `RepoStore` on the apply side rather than `dyn ObjectDb`,
so a gix-backed *apply* leg would need a small widening; the read seam — the one the ADR scopes —
is clean.)

### (d) Does the ADR's reasoning chain hold and is it consistent with ADR-0002? — Yes, with the (b) caveat.

The offline/disk/wasm/required-surface chain is consistent with ADR-0002's decision shape and the
crypto precedent. The one inconsistency is rhetorical, not logical: ADR-0002 *honestly* recorded
its counter-point ("a hand-rolled evaluator must be correct") and backed it with a differential;
ADR-0003 copies that move but its differential is not actually implemented (b). Fix (b) and the
chain is fully consistent.

---

## Other surfaces

### 1. Four-archetype coherence — Approve.
One `Driver` maps each sub-path to exactly one archetype: BlobFs (`Blob`/`Root` →
`BlobNamespace`), Relational (`commits`/`changes`/`blame`/`refs`/`tags` → `RelationalTable`),
Append (`reflog` → `AppendLog`). `node_desc` (lib.rs:180) and `caps_for` (lib.rs:158) are
per-node and consistent, and the DESCRIBE golden (tests.rs:553) checks archetype + schema per
node. `version_support` correctly returns `Versioned` for everything except `reflog`
(`Snapshot`). Coherent.
*Observation:* `blame`'s schema/caps are present and gated `{SELECT}`, but `blame` over an
unspecified file (`GitNode::Blame{file:""}`) has no parse-time requirement of a bounding `WHERE`;
the cost-bounding the ticket asks for is only the `limit` argument on the reader, not a structural
gate. **Proposal:** note in DESCRIBE that `blame`/`changes` require a ref-range/LIMIT bound (intent
signal to the AI), since the structural gate is deferred.

### 2. Purity invariant — Approve.
`plan_*` builders and `procedures` construct `Plan`/`GitEffect` DAGs and perform no I/O; they read
only already-in-memory refs (`repo.ref_oid`) for CAS old-oids and in-memory trees for the merge.
The lone impure seam is `GitApplier::apply_effect` (applier.rs:142), reached only via
`apply_shared`/`apply` (COMMIT). The PREVIEW-applies-nothing test (tests.rs:288) confirms the
branch is unmoved after planning. Faithful to RFD §3.

### 3. COMMIT keyword-clash — Approve.
Commit creation is `plan_insert_commit` modelled as `INSERT INTO …/commits`; the frozen `COMMIT`
plan keyword is never emitted by the driver. `caps_for` gives `commits = {Select, Insert}` only, so
`UPDATE`/`REMOVE /commits` are rejected at the resolve-time capability gate (test at tests.rs:476
asserts `unsupported_verb` for UPDATE and REMOVE). Clean resolution of the named hard part.

### 4. CAS ref updates — Approve.
`plan_update_ref` sets `old` to the expected current oid; the applier's `UpdateRef` arm
(applier.rs:160) compares-and-swaps and returns `RefCasConflict` on mismatch, on stale-creation,
and on stale-absent — never clobbering. `to_effect_error` maps it to `EffectError::conflict(actual)`
carrying the *actual* world oid, which is the right optimistic-concurrency coordinate to surface to
the t11/t12 txn bridge. The stale-oid test (tests.rs:342) asserts `code() == "conflict"` and an
untouched branch. Coherent with the runtime's optimistic model.
*Observation:* the conflict carries `actual` (the world's current oid) as the version, which is
the correct coordinate; just flag that this is an oid, not a monotonic world-version integer, so
the txn layer must treat the git "version" as the ref oid. No change required.

### 5. merge/rebase purity (highest-risk) — Approve with one observation.
`plan_merge` reads the three trees, runs `three_way_merge` **before** building any effect, and
returns `GitError::MergeConflict` with **zero** effects on a both-sides-diverged path — the
`PlanBuilder` is only touched after the merge succeeds, so a conflict cannot leave a half-built
plan. The conflicting-merge test (tests.rs:371) and clean-merge test (tests.rs:449) bracket this.
This is the key correctness property and it is structurally sound: no effect is emitted on the
error path.
*Observations:* (i) `plan_rebase` is literally `plan_merge` (a documented E0 reduction); the merge
*commit* it produces (two parents) is not a rebase's linear replay, so the semantics are a
placeholder — acceptable for E0 but the divergence from real rebase should stay a named park, not
just a doc line. (ii) The merge is **flat-tree only** (`read_flat_tree`), so a conflict on a
*nested* path cannot even be detected — but nested trees are a declared park and the flat-tree
limit is consistent across blobfs/relational/planner, so this is coherent, not a leak. (iii) Merge
content-merge is oid-level (same-oid = agree, else conflict); it never does a line-level 3-way
merge, so two independent edits to *different lines* of the same file conflict. That is more
conservative than git (never wrong, sometimes over-conflicts) — acceptable and truthful, worth a
DESCRIBE/doc note. **Proposal:** record (i) and (iii) explicitly as named parks so the E0 merge
semantics are not mistaken for git-faithful.

### 6. Pushdown residual truthfulness — Approve.
`PushdownProfile::Partial{ where_, project, limit, order true; join/aggregate/distinct/group_by
false }` matches what the revwalk can actually do (ref-range + LIMIT bound the walk; ORDER BY time
is the natural newest-first order). The readers return a **superset** the engine re-filters
(commits/changes test relies on residual `WHERE author` filtering in-test, tests.rs:218), so the
t20 "never silently wrong rows" lesson holds: nothing claims a predicate it does not enforce.
*Observation:* `order: true` claims ORDER pushdown, but only `time`-descending (revwalk order) is
truly native — `ORDER BY author` would still need an engine sort. As long as the engine treats
pushdown `order` as advisory and re-sorts on a non-time key, this is truthful; if the engine trusts
`order: true` unconditionally it could mis-order. **Proposal:** confirm with the Planner's E2E that
a non-time ORDER BY is re-sorted by the engine, or narrow the profile to signal only time-order.

### 7. No vendor leak / path traversal — Approve.
No `gix`/vendor type exists at all (zero such dep). DTOs are owned. `GitPath::parse` rejects any
`..` segment in `<rest>` (path.rs:115) and the traversal test (path.rs:231) covers it; the error
taxonomy carries no object bytes, no `.git/config`, no credentials (error.rs doc + variants). The
local object model has no token surface by construction.
*Observation:* canonicalisation is purely lexical (`..` segment rejection). Since the object DB is
in-memory and keyed by oid (no real filesystem path is ever joined), there is no actual FS
traversal vector at E0 — but when a future `LooseObjectDb`-on-disk or `GixObjectDb` lands, lexical
`..` rejection alone is not equivalent to canonicalising a real path (symlinks, absolute repo
segment). **Proposal:** add a note at the `ObjectDb` seam that an on-disk impl must canonicalise
the real repo-root join, not rely solely on the lexical `..` check.

### 8. Reflog recovery — Approve.
Every applied ref move plans a `WriteReflogEntry` recording the prior oid; `recover_ref`
(applier.rs:111) reads the newest reflog entry's `old` and forces the ref back, itself recording a
recovery reflog entry. The forced-move-then-recover test (tests.rs:516) asserts the round trip.
Faithful to the RFD §6 recovery story.
*Observation:* `recover_ref` recovers from the *applier's* reflog (the apply-side `RepoStore`),
while the read-side `Repo` has its own separately-seeded reflog; the two reflogs are independent
structures. For E0 (apply mutates the store, reads come from the resolver) this is fine, but the
read-side `/reflog` node will not reflect a just-applied move until the stores are reconciled.
**Proposal:** document that read-side `/reflog` reflects the seeded/last-synced state, and that
post-COMMIT reflog reads go through the applier store (or unify the two reflogs behind the seam).

### 9. Dep direction / leaf confinement — Approve.
`qfs-driver-git` depends only on `qfs-driver`/`qfs-plan`/`qfs-types`/`qfs-codec`/`qfs-runtime` +
`thiserror`/`tracing` — the locked driver-impl shape, zero new external object/crypto/zlib crates
(Cargo.lock adds only the path crate's own node). It is a runtime-consumer **leaf**; the
`dep_direction.rs` change is a single allowlist append (`"qfs-driver-git"`, line 334) guarded by
the generic leaf-confinement check (b) in that test, so tokio stays confined. Clean, minimal,
intent-signalling append — exactly the pattern the test was built to scale to.

---

## Cross-cutting summary

The crate is well-factored (clear seams: path → repo/resolver → archetype readers → pure planner
→ single impure applier), the four hard requirements (keyword-clash, CAS, merge purity, parse-time
caps) are each faithfully realised and tested, and the dependency story is exemplary. The single
substantive issue is **(b)**: the ADR's correctness guard for owning a DEFLATE inflater is asserted
but not implemented, leaving the highest-risk hand-rolled component (dynamic-Huffman inflate over
untrusted bytes) effectively untested against real git output. The implementation itself is
defensively correct on audit, so this is a *test-coverage + ADR-honesty* gap, not a known bug — but
it is the gap that should not ship "Accepted (locked)" as written.

## Review Notes

- Severity: one **should-fix** (the (b) inflate differential / ADR wording); the rest are
  observations with proposals (DESCRIBE bound-hints, ORDER pushdown advisory, two-reflog
  reconciliation note, on-disk canonicalisation note, rebase/merge-semantics parks).
- No structural blocker. The gix divergence is ruled **justified as a decision, with its evidence
  to be corrected to match the code.**
