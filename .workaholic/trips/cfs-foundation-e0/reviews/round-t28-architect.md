# Round t28 — Architect Analytical Review

**Reviewer**: Architect (Neutral / Structural)
**Target**: Constructor commit `98fe65d` — t28 interactive FTP-like shell
**Scope**: analytical/architectural review only (no test/build/clippy execution)
**Decision**: **Approve with observations** (no t28 blocker; two doc-fidelity nits to fix in-place; one topology preference recorded as a carry-over, not a blocker)

---

## Headline ruling — the THREE confinement-guard relaxations

**Verdict: PRINCIPLED relaxations, not a weakening.** The cut "binary is the composition
root; tokio dead-ends at the terminal sink" is structurally sound. Each of the three guard
edits narrows, not widens, what it admits, and each keeps the load-bearing invariant
(spine/lower crates can never reach UP into the integration layer or the runtime) mechanically
enforced. Detailed adjudication of the three sub-questions:

### (a) "The terminal binary is the legitimate sink where tokio dead-ends" — SOUND.

The runtime-leaf-confinement guard (`runtime_is_confined_to_plan_and_types`, check (b)) encodes
*why* a runtime consumer is safe: tokio dead-ends in a leaf and cannot transit back into the
spine. The exemption added at `dep_direction.rs:344` (`&& other.as_str() != "cfs"`) does not
break that rationale — it strengthens its precision. The `cfs` binary is the unique node that is
simultaneously (1) an allowlisted runtime consumer and (2) a true sink: **nothing in the
workspace depends on `cfs`** (the binary is a `[[bin]]`, not a lib; it has no `lib.rs` API
surface other crates could `use`). Therefore tokio reaching `cfs-driver-local` *through the
binary* still dead-ends — there is no edge back out. The exemption is the correct generalisation
of the leaf rule: a leaf is "nothing depends on it"; the binary is the maximal such node.

**Does it punch a hole that lets future code smuggle logic into the binary to dodge leaf
rules?** This is the real risk, and it is **adequately contained — but by guard #3, not by the
runtime exemption itself.** The runtime exemption alone would indeed let *any* future binary
code pull in *any* runtime consumer. What stops "smuggle logic into the binary" is the
*conjunction* with guard #3's exact-allowed-set: the binary may depend only on
`{cfs-cmd, cfs-core, cfs-exec, cfs-driver-local, cfs-pushdown}`, and may NOT reach the lower
spine/runtime directly. So a future author who tries to move domain logic into the binary by
adding, say, a `cfs-driver-s3` edge or a `cfs-plan` edge trips guard #3's allowlist immediately.
The two guards are co-dependent: #3 is what keeps the runtime exemption from being a blank
cheque. That coupling is real and correct, but it is **implicit** — see Concern C1 for a
proposal to make the binary's leaf-ness an explicit assertion so the safety does not silently
rest on "binaries happen to have no dependents."

### (b) Guard #3's exact-allowed-set — GENUINE gate, not a rubber stamp.

`binary_is_the_thin_entrypoint_plus_the_t28_shell_composition_root` does real work in BOTH
directions:
- **Forbid direction** (`lower_spine` loop, lines 122–141): explicitly asserts the binary does
  NOT depend on `cfs-server, cfs-lang, cfs-plan, cfs-driver, cfs-codec, cfs-parser, cfs-types,
  cfs-runtime`. This is the "cannot reach the lower spine/runtime directly" pin, and it is a hard
  negative assertion — a binary that gains a `cfs-plan` or `cfs-runtime` edge fails. Crucially
  `cfs-runtime` is in this forbidden list, so the binary cannot take a *direct* runtime edge; it
  only reaches the runtime *transitively through* the allowlisted `cfs-driver-local`. That is the
  intended shape.
- **Allow direction** (`allowed` loop, lines 142–158): the binary's `cfs`-prefixed deps must be
  a SUBSET of the five-crate allowlist. Adding any sixth `cfs-*` crate fails closed with an
  actionable message.

This is the opposite of a rubber stamp: it is a two-sided exact-set pin. The only softening vs.
the old test is that the old test asserted `ws_deps == vec!["cfs-cmd"]` (exactly one), whereas
the new test asserts a five-element allowed superset membership + a mandatory `cfs-cmd`
presence. That is the *necessary* loosening to admit the composition root, and it is bounded
precisely to the deliberate t28 set. I judge it principled.

One genuine gap (Concern C2): the forbid-list `lower_spine` and the allow-list `allowed` are
maintained as two independent literals. A *new* spine crate (E7 is about to land six server
crates) would be in NEITHER list, so the binary could take an edge to it and pass both loops
(it is not in the forbidden set, and… it would fail the allow-set since it is `cfs`-prefixed —
actually the allow-set catches it). Re-checking: the allow-set loop filters `d.starts_with("cfs")`
and requires allowlist membership, so any new `cfs-*` edge fails. Good — the allow-set is the
backstop and it is exhaustive over `cfs`-prefixed crates. The forbid-list is then belt-and-
suspenders (a more precise message for the known spine crates). So the gate is sound; C2 is
downgraded to a maintenance note.

### (c) Adapter in the binary vs. a dedicated composition crate — RIGHT CUT FOR t28.

The split is clean and defensible:
- **Shell LOGIC** (`resolve`/`desugar`/`eval_line`/`Completer`/`Session`) lives in
  `cfs-exec::shell` — runtime-free, terminal-free, fully unit/golden-testable. This is correct:
  the logic is exactly "desugar to the same statements + route through the same pipeline," which
  is what `cfs-exec` already owns. It respects C4 (cfs-cmd logic-free) and CO-t29-4.
- **Read ADAPTER** (`LocalReadDriver`, the async `ReadDriver` impl over the driver's sync
  `scan_rows`) lives in the binary, because it is the one node that may BOTH see `cfs-exec`'s
  `ReadDriver` seam AND depend on the runtime-coupled `cfs-driver-local`.

The reasoning chain is airtight: cfs-exec can't see the driver crate (its dep allowlist forbids
it); the driver crate can't implement `cfs-exec::ReadDriver` (only cfs-cmd/cfs may consume
cfs-exec); cfs-cmd can't host it (a cfs-cmd→cfs-driver-local edge makes cfs-cmd a non-leaf
runtime consumer). The binary is the unique remaining home. That is a forced move given the
existing topology, not an arbitrary one.

**Should there be a dedicated composition crate instead?** A `cfs-shell-host` lib crate (an
allowlisted runtime-consumer leaf) *would* be the more orthodox composition-root location — it
would let the adapter carry unit tests as a library, keep `main.rs`/`shell.rs` truly thin, and
give E7's eventual server-side wiring a sibling to imitate. **But it is NOT a t28 blocker.** The
binary-hosted adapter is structurally equivalent (the binary is itself a leaf sink), it carries
golden tests in `crates/cfs/src/shell.rs#tests`, and introducing a crate now would add a guard-
surface change for no behavioural gain. I record "extract `cfs-shell-host` when the second
composition root (E7 server read-wiring) appears" as a **carry-over**, to be revisited when the
DRY pressure is real rather than anticipated.

---

## Other surfaces

### 1. No-new-semantics invariant — CONFIRMED.

Desugaring is to **source text**, not a hand-built AST (`desugar.rs`), and both builtin and raw
lines funnel through the SAME `parse → build_plan → plan_preview/apply_commit` (effects) or
`parse → block_on_read` (reads) pipeline in `Session::eval_statements`. Because the desugared
string is re-parsed by the same `parse()`, a builtin yields byte-for-byte the same `Statement`
(hence the same `Plan`) as the typed form. This is the strongest possible fidelity guarantee and
the right design choice. Confirmed mappings:
- `cp src dst` → `UPSERT INTO dst FROM src` (`copy_stmt`, desugar.rs:214). **UPSERT is correct**:
  the doc rationale (retry-safe/idempotent blob/namespace write, RFD §6) is consistent with the
  copy→verify→delete recovery shape `mv` depends on, and with the drivers' universal-write
  archetype. INSERT (append-only/relational) would be wrong here.
- `mv` → `[copy(UPSERT), REMOVE src]` (desugar.rs:187) — copy-then-delete, two dry-runnable legs.
- `rm a b` → `[REMOVE a, REMOVE b]` — one REMOVE per arg (set).
- `ls` → `FROM p |> SELECT name, size, is_dir, modified` (read).
- `cat` → bare `FROM p` (read).

**Nit N1 (doc fidelity)**: `Builtin::Mv` doc (desugar.rs:33) and the inline comment (desugar.rs:180)
still describe the copy leg as `INSERT … FROM …`, but the code emits `UPSERT` via `copy_stmt`.
The Cp doc (line 31) correctly says UPSERT. This is a stale-comment translation-fidelity defect:
a reader auditing `mv`'s recovery semantics from the doc would believe a non-idempotent INSERT
is used. Fix the two `Mv` comments to say UPSERT. Not a behavioural bug (code is correct), but it
is exactly the kind of doc/code drift the translation-fidelity domain exists to catch.

### 2. PREVIEW/COMMIT safety invariant — CONFIRMED.

The gate is uniform and unshortcuttable. `eval_statements` builds EVERY leg's plan first (so a
parse/capability error in any leg aborts the whole batch before any apply — desugar.rs comment
and session.rs:140–147), then previews-or-commits the whole batch atomically. The REPL driver
(`shell.rs`) defaults every line to PREVIEW and only a bare typed `COMMIT` on the next line
applies the *remembered* pending line. `is_effect` is correctly scoped to `Cp|Mv|Rm`; `cd`/`pwd`
are pure `Outcome::Moved`/`Outcome::Cwd` state changes that never touch the plan pipeline. There
is no path by which a builtin reaches `apply_commit` without first going through the same
build_plan+gate as a raw effect. Set ops (`rm a b`) preview the union of plans with counts.
Confirmed against the t28 acceptance ("plan assertions, not live effects").

### 3. `resolve()` correctness — CONFIRMED.

`path.rs` is pure, total, well-tested. Verified all cases against the unit tests: relative folds
onto cwd; `..` pops and **clamps at the mount root** (`fold_segments` `segs.pop()` on empty is a
no-op via `Vec::pop`), so it can never escape the driver namespace; `~`/bare-`/` anchor at the
cwd driver's mount root; absolute `/driver/...` crosses drivers freely (keeps its own driver,
ignoring cwd). The cwd-relaxation of the t29 absolute-only gate is **sound**: a relative path is
purely lexical and always stays under the cwd's driver, so it cannot conjure a path into a driver
that has no namespace archetype — and the only state-changing consumer (`cd`) is independently
capability-gated by `validate_namespace`/`namespace_check` (session.rs:194), which rejects an
unmounted target or a non-namespace archetype (only `BlobNamespace`/`ObjectGraphWorkflow` admit
a `cd`). So the relaxation cannot be used to land cwd in an addressable-but-non-enterable node.

**Minor observation O1 (not a defect):** resolution is purely lexical — `cd sub` into a path that
does not exist on disk but whose *driver* describes the synthetic node as a namespace would
succeed (the namespace check is archetype-level, not existence-level). For the local driver this
is benign (a subsequent `ls` over a missing dir yields an empty listing, read.rs handles
NotFound→empty). Worth a one-line doc note on `validate_namespace` that the gate is archetype-
level, not existence-level, so the boundary is explicit for future remote drivers.

### 4. `ScanNode.path` threading — CONFIRMED ADDITIVE, no regression.

The new field is genuinely additive:
- `LogicalPlan::Scan` gains `path: String`; the existing `scan(source, schema)` constructor
  defaults it to `String::new()` (logical.rs:251), so every existing caller compiles unchanged
  and behaves identically (empty path). The new `scan_at` carries the address.
- `lower_source` now passes `format!("/{}", segs.join("/"))` (lower.rs:149) — the addressed VFS
  path the `FROM` named.
- `planner.rs` threads it through `Acc` and the new `scan_path` helper, which walks the unary
  chain to the `Scan` leaf and returns its path (empty for `Join`/`SetOp` multi-source roots —
  the defensive `federate` path also copies it). The synthetic `(values)` source keeps the empty
  path, as documented.

Existing driver scans still resolve correctly: previously a `ScanNode` carried only `source`
(the registry key for profile/readability), and the read driver scanned the mount root. Now the
driver receives the concrete path and can navigate to the exact node, while `source` still keys
the profile. `LocalReadDriver::scan` (shell.rs:62) correctly falls back to `LOCAL_MOUNT` when
`scan.path.is_empty()` — so a synthetic/empty-path scan degrades to the old mount-root behaviour,
preserving backward compatibility. The end-to-end golden test `cd_then_ls_navigates_into_subdir`
proves the address actually reaches the driver (`ls /local/sub` lists only `c.md`, not the root).

### 5. Local `ReadDriver::scan` facet (`scan_rows`) — CONFIRMED no runtime leak; genuine first consumer.

`crates/driver-local/src/read.rs` is **pure, synchronous, async-free**: it imports only
`cfs_types` + the crate's own `fs_core`/`row`/`error`, and exposes `scan_rows(&Sandbox, &str,
Option<&[Name]>) -> Result<RowBatch, LocalError>`. No `cfs-exec`, no tokio, no async-trait. The
async `ReadDriver` adapter lives one layer up in the binary (`LocalReadDriver` in shell.rs),
exactly as the topology requires. The driver's pure path stays off the integration layer, so the
confinement guards stay green. This is the **genuine first consumer of the t29 read seam**
(CO-t29-1 progress): a real `ReadDriver` impl backed by a real driver's scan, exercised
end-to-end by the golden tests over a tempdir mount. Pushdown honesty is correctly handled —
`Partial { project: true }`: it applies the projection when present but over-returns otherwise
and lets the executor residual trim, matching t20. Sandbox escape → `OutsideSandbox` →
`InvalidPath{reason:"outside_sandbox"}`; NotFound → empty (robust). No secret material is
read or surfaced.

**Nit N2 (doc fidelity — the more important of the two):** read.rs:8 states the async adapter
"lives one layer up (**in `cfs-cmd`**, which may see both seams)." This is **factually wrong** —
the adapter lives in the **binary** (`cfs` crate, `shell.rs`), NOT in cfs-cmd. The entire
headline reason cfs-cmd cannot host it (it would become a non-leaf runtime consumer) is the
point of this whole ticket; this comment contradicts the design it implements and would mislead
the next reader into thinking the forbidden edge exists. Fix to "(in the `cfs` binary crate, the
leaf composition root that may see both seams)." This is a translation-fidelity defect against
the very invariant the guards enforce — worth correcting before accept even though it is a
comment, because comments in dep-confinement code are load-bearing documentation.

### 6. Cross-mount cp/mv independence; completer bound; no secrets — CONFIRMED.

- **Cross-mount independence**: `resolve(src)` and `resolve(dst)` are independent against the
  same cwd; an absolute dst names its own driver. `cp_cross_mount_keeps_each_driver` proves
  `cp report.md /mail/drafts/report` → `UPSERT INTO /mail/drafts/report FROM /local/docs/report.md`
  with cwd unchanged. No `cd` is performed by any effect builtin (only `Cd` mutates cwd).
- **Completer bound**: `bounded_read` (complete.rs:211) runs `block_on_read` on a worker thread
  joined under `COMPLETE_TIMEOUT` (750ms) via `recv_timeout`; on timeout the worker is detached
  (harmless against in-memory/local reads) and the completer falls back to no candidates. The
  per-parent cache (`invalidate` per prompt) avoids re-scans. This honours hard-part (b) (a slow
  driver never hangs the REPL) without injecting an async runtime into the pure completer. Sound.
  See O2 for a bounded-resource observation.
- **No secrets**: the shell handles no credentials; the history file holds only command lines
  (documented), and `local_to_cfs` maps errors to secret-free structured `InvalidPath`. Confirmed.

**Observation O2 (completer worker leak under pathological slowness):** on timeout the worker
thread is *detached*, not cancelled (it cannot be — `block_on_read` builds a blocking
current-thread runtime). For the local driver a scan completes near-instantly so this never
accumulates. But against a genuinely slow/hung future remote driver, rapid repeated TAB at one
prompt could spawn detached threads faster than they drain. The per-prompt cache mitigates
(repeat TAB at the *same* parent is cached), so the realistic blast radius is small. Not a t28
concern (local-only is exercised), but record it: when a remote read driver lands, the completer
should gate concurrent in-flight scans (e.g. a single-flight guard per parent key) so a hung
driver cannot leak unbounded threads. Carry-over, not a blocker.

### 7. In-memory-COMMIT park honesty — CONFIRMED HONEST, not an overclaim.

`Session::eval_statements` routes COMMIT through `cfs_exec::apply_commit`, which (exec.rs:167)
runs `commit(plan, &mut RecordingApplier, …)` — the in-memory recording double, NOT the real
local applier. The function doc is explicit: "**no live creds, no network** … A real E4 commit
drives the runtime interpreter; that wiring is the t30+ carry-over." The golden test
`rm_then_commit_reaches_the_committed_plan_stage` (shell.rs:385) documents this *in the test
itself*: it asserts the transcript reaches `COMMITTED` and explicitly comments that the on-disk
file is **expected to remain** because the shell's COMMIT does not drive the real local applier.
This is exactly the t28 acceptance shape ("asserted by plan assertions … not live effects") and
is honestly documented in three places (the exec doc, the shell test, and the session module
doc). No overclaim. The committed-plan stage is real; the live FS mutation is honestly deferred.

---

## Concerns and proposals (Critical Review Policy: ≥1 concern + a proposal each)

- **C1 — the runtime exemption rests on an *implicit* "binary has no dependents."** The
  `other.as_str() != "cfs"` exemption is safe only because nothing depends on `cfs`. Today that
  is true by construction (it is a `[[bin]]`), but it is asserted nowhere — a future `cfs` lib
  target, or another crate `path`-depending on the binary package, would silently invalidate the
  tokio dead-end argument while the exemption still fires.
  **Proposal**: add a one-line positive assertion to `runtime_is_confined_to_plan_and_types` (or
  `binary_is_the_thin_entrypoint_…`) that **nothing in the workspace depends on `cfs`** (mirror
  the existing `nothing_depends_on_cmd` pattern for the binary). That makes the leaf-ness the
  exemption relies on an explicit, enforced invariant rather than an accident of packaging.
  Cheap, closes the only structural soft spot in the relaxation. (Carry-over-eligible; not a
  blocker since it is true today.)

- **C2 — forbid-list / allow-list dual maintenance in guard #3.** `lower_spine` and `allowed`
  are independent literals; the allow-list is the real backstop (it is exhaustive over `cfs`-
  prefixed crates) and the forbid-list is a more-precise-message convenience.
  **Proposal**: add a comment in the test stating that the `allowed` set is the authoritative
  closed gate and `lower_spine` is only for sharper diagnostics, so a future maintainer does not
  mistake the forbid-list for the safety boundary and "fix" a new spine crate by adding it there.
  Documentation-only.

- **N1 / N2 — two doc-fidelity defects (fix in-place before accept).** `mv`'s comments say
  INSERT but the code emits UPSERT (desugar.rs:33, :180); read.rs:8 says the adapter lives in
  cfs-cmd but it lives in the binary. Both are comments contradicting the implemented invariants;
  N2 in particular contradicts the central topology decision. **Proposal**: correct both
  comments. No behavioural change. These are the minimum-edit fixes I would want before the Lead
  accepts, since they are translation-fidelity defects in load-bearing documentation.

- **O1 / O2 — recorded observations (carry-overs, not blockers)**: namespace gate is archetype-
  level not existence-level (doc note); completer detaches worker threads on timeout (single-
  flight guard when a remote read driver lands).

- **Topology preference (carry-over, not a blocker)**: extract a `cfs-shell-host` composition
  crate when E7 introduces a second composition root (server read-wiring); until then the binary-
  hosted adapter is structurally equivalent and correctly tested.

---

## Cross-artifact coherence

The implementation is coherent with the t29 read-seam direction (CO-t29-1: first real
`ReadDriver` consumer), the t01/C4 logic-free-cfs-cmd model, and the RFD §3/§6/§7 closed-core +
PREVIEW/COMMIT + FTP-shell vision. The guard relaxations are documented at the point of change
with the structural rationale, which is the translation-fidelity standard I hold this layer to.
The two doc nits are the only places where the prose drifted from the code; fixing them restores
full fidelity.

**Final decision: Approve with observations.** Not a blocker. Recommend the Lead direct the
Constructor to fix N1 + N2 (two comment corrections) in-place; C1 (binary-no-dependents
assertion) is strongly recommended and trivially cheap but may be taken as a carry-over; O1/O2
and the composition-crate preference are carry-overs to revisit when remote drivers / E7 land.
