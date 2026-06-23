# Architect Analytical Review — t38 Test Harness (`cfs-test`) + wasm-gating guard

- **Reviewer**: Architect (Neutral / structural bridge)
- **Ticket**: `20260622214650-t38-test-harness-and-golden-tests`
- **Commit**: `e57addc`
- **Scope of this review**: analytical / code / architectural only (no test, build, or clippy
  execution — disk ~99%; the Lead owns gate verification).
- **Decision**: **Approve with observations** (one load-bearing gap: the wasm-gating *positive
  control* exists only in prose, not in code — see Headline 3 / OBS-1).

---

## Headline 1 — `cfs-test` design + the dev-only invariant

**Ruling: faithful. The dep-graph assertion genuinely proves the shipped binary does not link
`cfs-test`, and the harness trades only in owned DTOs.**

- `tests/dev_only_dep_graph.rs` BFS-walks the `cfs` package's resolve-graph edges from
  `resolve.nodes`, and for each edge inspects `dep_kinds[].kind`, keeping an edge only when
  `kind.is_none()` (a normal dep) **or** `kind == Some("build")`, and dropping `"dev"`. This is
  the correct cargo-metadata model: a dev-dependency edge to `cfs-test` is exactly what is
  allowed, a normal/build edge is the violation. The closure is then checked for `cfs-test`,
  and a second assertion confirms `cfs-test` IS a workspace member (so the test cannot pass
  vacuously on a typo'd name). It mirrors the established `crates/plan/tests/purity_deps.rs`
  shape. **It correctly excludes dev-deps while catching a real normal/build edge.**
- The crate's `[dependencies]` are `cfs-core`, `cfs-parser`, `cfs-http-core`, `serde`,
  `serde_json` — no vendor SDK, no reqwest/tokio. The public surface (`Plan`, `Statement` AST,
  `Row`/`Schema`/`RowBatch`, the `HttpRequest`/`HttpResponse` DTOs) is all owned. So the crate
  **polices** the no-leak invariant rather than importing it: a vendor type appearing in a
  golden would be a serialization of a type the crate cannot even name.
- `publish = false` + `version = "0.0.0"` correctly mark it unpublishable.

*Observation (minor):* the dev-only guard proves no *normal/build* edge today, but does not pin
that `cfs-test` stays out of the `[dependencies]` (vs `[dev-dependencies]`) of intermediate
crates — it relies on the whole-graph BFS, which is the right and sufficient check. No change
needed; noting that the guard is graph-level, not per-manifest, which is the stronger form.

---

## Headline 2 — In-house golden / MockHttp / property decision (ADR-0006) + determinism

### (a) Proportionate + consistent with the ADR-0002..0005 precedent; reversibility genuine — YES.

ADR-0006 records the same footprint shape as ADR-0001..0005: insta/proptest/httptest/wiremock
are absent from the offline cache (and httptest/wiremock are socket-bound, so unusable in a
no-socket wasm-pure harness), so the Constructor hand-rolled dependency-light equivalents. The
"Reversibility" section is genuine and not hand-waving: each in-house piece sits behind a stable
helper signature (`assert_golden` / `roundtrip_codec` / `MockHttp`) so a future cache landing of
insta/proptest/httptest swaps the implementation without touching call sites. This is the same
trait-/signature-gated seam ADR-0001/0002 used. **Proportionate and consistent.**

### (b) Determinism normalization is SUFFICIENT — YES, traced.

The plan is an unordered DAG (RFD §6 batching), so a flapping golden was the real risk. Traced:

1. **Node order.** `plan_assert::canonicalize_plan` sorts `plan.nodes` by `n.id.0`. `NodeId` is a
   `u32` allocated by `PlanBuilder::next_id` as a **monotonic counter** (`self.next`), and
   `Plan::validate` rejects duplicate ids — so dense-id order is a **total, deterministic** order.
   The evaluator allocates ids by a deterministic walk of the statement (`eval.rs` `next_id()`
   calls at read-source then write). Node order therefore cannot flap.
2. **Edge order.** `plan.deps` (a `Vec<(NodeId, NodeId)>`) is sorted by `(parent.0, child.0)` —
   total order over distinct ids. Cannot flap.
3. **Map-key order.** `golden::canonicalize` recursively sorts every JSON object's keys
   lexicographically, so a struct-field or `BTreeMap` reorder cannot flap a golden.
4. **Non-deterministic scalars.** `timestamp/ts/request_id/run_id/updated_at/created_at/now/
   nonce` (case-insensitive) are redacted to `<redacted>` before compare.
5. **Trailing newline** keeps the fixture a clean text file.

**Critically correct choice:** `canonicalize` sorts object **keys** but preserves **array order**
(`Array(items) => items.map(canonicalize)`). This is right — arrays in these DTOs (`RowBatch.rows`,
`Schema.columns`, `ParseErrorSnapshot.expected`) carry *semantic* order (row order, column order,
expected-token order) that is itself deterministic from the parser/evaluator; sorting them would be
**wrong**. The normalization sorts exactly the order that is non-meaningful (DAG nodes/edges,
map keys) and preserves the order that is meaningful. **A golden built this way will not flap.**

The `CFS_BLESS=1` workflow is a single cargo-native env gate (no `cargo insta review` UI), the
diff is shown in the panic message, and `assert_no_credential_shape` runs on every rendered golden.
The unit tests in `golden.rs` directly verify key-sorting, redaction, field-order stability, and the
scrub (clean-pass + bearer-flag `should_panic`).

---

## Headline 3 — The no-default-features-excludes-tokio guard (wasm-gating, finally mechanized)

### Closure computation is CORRECT for the four pinned leaves.

`no_default_features_closure` walks each leaf's `package.dependencies`, skipping `kind == "dev"`
and `kind == "build"` (normal deps have `kind == null`, correctly kept), and skipping
`optional == true`. I verified against the manifests that this faithfully models
`--no-default-features` for **these four** leaves:

- `cfs-cron`: tokio (and cfs-exec, cfs-server) are `optional = true`, gated `native = ["dep:tokio", …]`.
- `cfs-watchtower`: same shape — and note its **non-optional** tokio at line 68 is a
  `[dev-dependencies]` entry, correctly excluded by the `kind == "dev"` filter (I confirmed this is
  the dev block, not a normal-dep leak). This is the subtle case the question flagged
  ("optional-AND-default vs default-non-optional"): here the only non-optional tokio is dev, so it
  is correctly out of the closure.
- `cfs-host`: `cfs-server` optional, gated `host-daemon`; default features empty.
- `cfs-driver-slack`: `cfs-runtime` optional, gated `runtime`; the always-on/`events` features are
  no-op enablers that add no `dep:`.

The model "declared non-optional normal deps, transitively" is exact here **because none of these
four packages has a *default* feature that enables an *optional* dep without a gate**. That is the
one shape that would break the model (an optional-AND-default-enabled dep would be compiled under
`--no-default-features`-of-the-leaf yet dropped by this closure). I checked: no such case exists in
the four leaves. The model is sound *for the pinned set*; see OBS-2 for the durability caveat.

### Pinned set — RIGHT four leaves, none missed.

The wasm-gated leaves are the three fire-path bindings + the host daemon:
`cfs-cron`(native) / `cfs-watchtower`(native) / `cfs-host`(host-daemon) / `cfs-driver-slack`(runtime).
These are exactly the crates whose manifests carry an `optional` tokio/cfs-server/cfs-runtime gate
with a documented wasm rationale. `cfs-http` is **not** wasm-gated by design (its tokio is
non-optional — it is a native-only leaf that dead-ends in the binary), so it is correctly excluded
from `GATED_LEAVES`. No wasm-gated leaf is missed.

### **OBS-1 (load-bearing): the positive control is PROSE-ONLY, not mechanized.**

The Constructor's claim is that the guard "genuinely bites" because `cfs-http` carries tokio in its
no-default-features closure, so the closure logic would catch a regression. **But
`wasm_gating.rs` contains no assertion about `cfs-http` (or any other tokio-bearing crate).** The
file is a single test that asserts only the *negative* (the four leaves are tokio-free). There is no
*positive control* in code that fails if `no_default_features_closure` were silently returning an
empty/wrong set (e.g., a bug that skips every dep would make the negative assertions pass
vacuously). The claim that "cfs-http carries tokio in that closure" is true — I verified
`crates/http/Cargo.toml` line 34 declares a non-optional `tokio` — but **it is asserted nowhere in
the test**. As written, the guard catches a *real* regression (a tokio edge moved out from behind a
leaf's gate) **only if the closure walk is itself correct**; nothing pins the closure walk against
going vacuous. A `cargo metadata` schema change (e.g., `dependencies[].name` → a different field) or
a refactor bug would turn the whole test green-and-meaningless.

**Proposal:** add a positive control to `wasm_gating.rs`: assert that
`no_default_features_closure("cfs-http", "", &pkg_by_name)` **does** contain a `"tokio"`-matching
entry (and, symmetrically, that one of the gated leaves' *full* closure — with the optional dep
re-included — would contain tokio). One extra `assert!` makes the guard self-checking: it proves the
walk reaches tokio when tokio is reachable, so a vacuous-pass bug fails loudly. This is the same
"prove the test can fail" discipline the `golden.rs` `should_panic` scrub test already applies.
Without it the guard is *probably* load-bearing but not *demonstrably* so — and the ticket's own
framing ("it must actually bite") asks for the demonstration.

---

## Other surfaces

1. **assert_plan / no_io_performed / FakeBackend reuse — genuine.**
   - `assert_plan` runs `parse_statement → Evaluator::eval(&MountRegistry)` — the same seam the CLI
     and server use, not a parallel evaluator. It matches `EvalValue::Plan` and panics on a pure
     read, which is the right test-author guard.
   - `no_io_performed` is honest about what it proves: the evaluator builds effects-as-data and never
     reaches the applier seam, so I/O-freedom is a *type-level* property; the method `validate()`s the
     DAG (the only way a "pure build" could have gone wrong) and documents the invariant. The
     `harness_demo.rs` `PanicApplier` (a `Driver::applier` that `panic!`s) is the *stronger* proof —
     it would fire if `assert_plan` ever touched the applier, and it does not. **Genuine reuse;
     purity proven from the test side.**
   - `FakeBackend` **is** a `cfs_core::PlanApplier` and is driven through the real `cfs_core::commit`
     interpreter — the exact COMMIT path a production driver uses. Not a parallel apply abstraction.

2. **Codec round-trip identity model — RIGHT invariant, all 6 formats.**
   The model is `decode(encode(decode(b))) == decode(b)` (row-stability under a re-encode cycle), not
   naive byte identity — correct, because RFD §4 documents several codecs as non-byte-preserving
   (whitespace/key-order/comments), so a byte-identity invariant would *falsely fail*. Row identity
   is the property every read/write path actually depends on. `covers_each_builtin_format_at_least_once`
   asserts the corpus hits `json/jsonl/yaml/toml/csv/md+frontmatter` — the 6 builtins (the ticket's
   "6 formats"; jsonl is the sixth alongside the five named in the AC, md+frontmatter covered both
   with-and-without frontmatter). **Correct invariant, full coverage.**

3. **preview_handler — asserts the real fired Plan, no re-implementation.**
   It calls `parse_server_binding_ddl` + `desugar_to_insert` from `cfs_core` — the production desugar
   seam — and returns the real `Plan`. The demo asserts a single `ServerConfigWrite` node and
   `!is_irreversible()` for ENDPOINT/JOB. No socket, no listener, no re-implemented desugar.

4. **No-network guard + credential scrub — both wired.**
   - The no-network guard is *structural* (`assert_pure` is a declarative thin wrapper) rather than a
     runtime socket block, and the rationale is sound: the pure helpers reach plan/AST/rows through
     `cfs-core`/`cfs-parser`/`cfs-http-core`, whose dependency closures cannot contain tokio/reqwest
     (enforced by `cfs-plan`'s purity dep-test and the wasm-gating guard). I/O-freedom is thus a
     graph property, not a runtime interception. Honest framing — the doc comment says exactly this.
     *Observation:* `assert_pure` cannot *catch* an accidental I/O at runtime (it just runs the
     closure); its value is documentary + the dependency-closure proof behind it. That is acceptable
     given the structural guarantee, but the method name slightly oversells — it asserts *nothing*
     executable. Minor; no change required (the README/doc already explains it).
   - The credential scrub (`assert_no_credential_shape`) runs on **every** rendered golden
     (`plan_assert::snapshot`, `parse_golden::snapshot`, `ParseErrorSnapshot::snapshot`, and the demo
     handler golden), checking a conservative set of secret prefixes (Bearer/ya29./AKIA/xox*/sk-/
     ghp_/gho_/PEM/`1//`). The `mock_http.rs` `auth_header_is_recorded_but_never_logged` test proves
     the orthogonal property: the redacting `Debug` never prints the token. Both wired.

5. **Scope discipline — RIGHT call.**
   Not migrating the 1159 (README/demo say 1159; commit message context cited 1134/1159 — a
   consolidation count, not a regression) existing tests is correct: a mass rewrite is pure churn with
   real regression risk and zero behavioral payoff. `harness_demo.rs` exercises **each** helper
   category once (plan assertion ×2, golden plan snapshot, parser golden ×3 + error recovery, codec
   round-trip over the full corpus, handler PREVIEW ×2 + golden, apply-twice idempotency) — a genuine
   representative pass, not a stub.

6. **wasm-pure parity — clean split.**
   The pure/impure split is clean: `MockHttp` is `RefCell`-backed (single-threaded, `!Send`) so it
   stays wasm-clean (no `Mutex`/threads), and ADR-0006 records the `Send` reqwest mock staying in
   `cfs-driver-http`. The crate's deps are the pure spine only; `assert_golden`'s std-fs bless path is
   reached only under native `#[cfg(test)]`, and the *helper surface* a wasm consumer calls
   (`canonical_json`/`roundtrip_codec`/`golden_parse`/`assert_plan`) is socket-/thread-free. The
   source-level dep guards (purity_deps + wasm_gating) are the standing lock; the real wasm build is a
   CI concern (disk forbids it here). **Clean.**

---

## Concern / proposal summary (Critical Review Policy: ≥1 concern + proposal)

| # | Concern | Severity | Proposal |
|---|---------|----------|----------|
| OBS-1 | `wasm_gating.rs` has **no positive control** — the "cfs-http carries tokio" claim that proves the guard bites lives only in prose/ADR, so a vacuous-pass bug in `no_default_features_closure` would pass green. | Load-bearing (the ticket explicitly asks the guard to "actually bite") | Add an `assert!` that the closure for a known tokio-bearing crate (`cfs-http`) **contains** a `tokio` match, making the walk self-checking — same "prove it can fail" discipline as the `golden.rs` scrub `should_panic` test. |
| OBS-2 | The closure model "non-optional normal deps, transitively" is exact only because no pinned leaf has a *default* feature enabling an *optional* dep without a gate. A future manifest edit that does so would silently weaken the guard (the dep would be in the real `--no-default-features` build but dropped by the closure). | Minor / durability | Add a comment in `no_default_features_closure` documenting the assumption, or (stronger) resolve features via `cargo metadata --no-default-features`'s `resolve` for the leaf rather than re-deriving from `optional`. Acceptable to defer to an E8 hardening ticket; the current four leaves are safe. |
| OBS-3 | `assert_pure` asserts nothing executable (documentary only). | Cosmetic | None required — the structural dependency-closure proof is the real guarantee and the doc says so. Optionally rename to `pure_call`/`on_pure_path` to not overstate. |

**Decision: Approve with observations.** The harness is structurally sound — the dev-only proof is
real, the golden determinism is genuinely flap-proof (DAG node/edge + key normalization with correct
array-order preservation), the apply seam and desugar seam are genuinely reused (not re-implemented),
and the codec row-identity invariant is the correct one. The **one** thing the ticket explicitly
demands and the commit does not deliver in code is the wasm-gating *positive control* (OBS-1): the
guard is almost certainly load-bearing, but it is not *demonstrably* self-checking. I recommend the
Constructor add the one-line positive-control assertion before archive; OBS-2/OBS-3 are deferrable.
