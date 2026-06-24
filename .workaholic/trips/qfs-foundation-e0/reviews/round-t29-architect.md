# Round t29 — Architect Analytical Review

- **Author**: Architect (Neutral / structural bridge)
- **Status**: under-review
- **Reviewing**: Constructor commit `1c52771` — ticket t29 (CLI one-shot + the SELECT read-path executor), new crate `crates/exec/`, plus `crates/cmd/` and the `qfs` bin.
- **QA domain**: analytical review only (no test/build/clippy execution).

## Decision

**Approve with observations.**

The t20 carry-over is **genuinely closed**: `qfs-exec` implements the full
parse→resolve→plan(`qfs-core::plan_query`/`qfs-pushdown`)→driver scan(`ReadDriver`)→`qfs-engine`
`MiniEvaluator` residual re-filter→owned `RowSet` pipeline end-to-end, and the residual
no-wrong-rows property is exercised against a `PushdownProfile::None` over-returning driver. The
two-impure-stage topology (`ApplyDriver` for writes, `ReadDriver` for reads) is **structurally
sound today and the right call for t29**; I rule the eventual driver-facet consolidation a
**carry-over**, not a blocker. All E0 confinement invariants hold. The observations below are one
genuine gap (no mechanical dep-guard on `qfs-exec` itself) and three carry-overs.

---

## HEADLINE RULING — the two-impure-stage topology

**Ruling: the read/write seam split is principled and correct for t29. The unified driver-facet
registration is a real future coherence concern — recorded as carry-over CO-t29-1, not a t29
blocker.**

Reasoning across the three sub-questions the Lead posed:

**(a) Is the read/write seam split principled, or will a driver author resent two traits?**
Principled. The split tracks a real type-level asymmetry, not an arbitrary partition:
`ApplyDriver::apply_batch` returns `EffectOutput{id, affected}` — a **count**, never rows
(confirmed against the runtime write seam and re-stated in `read.rs` lines 5–12). A read seam must
return rows (`RowBatch`). You cannot thread reads through a count-returning seam without distorting
it into a row-returning one, which would corrupt the write stage's contract. So `ReadDriver::scan(&ScanNode) -> RowBatch`
and `ApplyDriver::apply_batch(...) -> EffectOutput` are genuinely different shapes for genuinely
different jobs. A driver author implementing both is not implementing "the same thing twice"; the
duplication is only the boilerplate of two trait impls, which is the normal cost of two distinct
I/O contracts. **Verdict: not a coherence problem at the seam level.**

**(b) Should there eventually be ONE unified driver-facet registration?** Yes — this is the real
seam of truth in the question. Today a real E4 driver must register three facets in three places:
the introspective `MountRegistry` (pure `describe`/`pushdown`), the runtime write `DriverRegistry`
(`ApplyDriver`), and now the new `ReadRegistry` (`ReadDriver`). `read.rs` lines 51–55 explicitly
acknowledge this ("a real E4 driver registers all three facets"). Three independent registrations
of one logical driver is the emerging coherence smell — not the two traits, but the **three
disjoint registries a single gmail driver must populate coherently and in sync**. The risk is a
driver registered for reads but not writes (or under a mismatched id) producing a confusing partial
capability. I recommend a future `DriverFacets`/bundle registration (one call binds a driver's
mount + read + write facets under one `DriverId`) — but this is cross-cutting (it touches
`qfs-core`, `qfs-runtime`, and `qfs-exec` registries) and **out of t29's scope**. Recorded as
**CO-t29-1**.

**(c) Does keeping `qfs-exec` runtime-free hold long-term, or will real-driver COMMIT force a
`qfs-exec → qfs-runtime` dependency and collapse the separation?** It holds, and the separation
does **not** collapse — provided the COMMIT path is wired the way the runtime confinement guard
already anticipates. `apply_commit` (exec.rs 167–178) currently applies via the pure
`qfs_core::RecordingApplier` test double, and the doc comment defers real COMMIT to "the runtime
interpreter (t30+)". The key structural fact: the `qfs` **binary** is already an allowed
`qfs-runtime` consumer (`dep_direction.rs` line 335). So the real-driver COMMIT wiring can live in
the binary / a bootstrap leaf that owns both `qfs-runtime` and `qfs-exec` — `qfs-exec` need not gain
the runtime edge itself. If, instead, a future ticket makes `qfs-exec` depend on `qfs-runtime`
directly, the generic leaf-confinement check (`dep_direction.rs` lines 288–316) fires the moment
`qfs-cmd → qfs-exec` is present, because `qfs-exec` would then be a non-leaf runtime consumer
(`qfs-cmd` depends back onto it). **That guard is the structural safety-net that prevents the
collapse from happening silently.** So the separation is enforceable, not merely aspirational —
but see Observation 1: it is enforced for `qfs-exec` only *transitively/negatively*, never
*positively*.

---

## t20 carry-over closure — CONFIRMED

**The core deliverable is genuinely closed.** Evidence:

1. **End-to-end pipeline present** (`exec.rs::execute_read`, lines 46–83):
   `plan_query(stmt, mounts)` (the AST→PhysicalPlan pushdown split, confirmed at
   `crates/core/src/plan.rs:85`) → for each `physical.scans()` resolve a `ReadDriver` and `.scan()`
   it → `MiniEvaluator::new().execute(&physical, ScanResults::new(batches))` → `RowSet::from_batch`.
   This is exactly the parse→resolve→plan→scan→residual→rows chain.

2. **Positional contract is honored end-to-end.** `execute_read` pushes batches in
   `physical.scans()` left-to-right order (exec.rs 57–73); `ScanResults` consumes them positionally
   (`crates/engine/src/scan.rs:5–7, 39–43`); `MiniEvaluator::eval_node` pulls one batch per scan
   leaf in the same left-to-right order, including the binary-op left-then-right discipline
   (`crates/engine/src/combine.rs:99–101, 152–162`). The executor's ordering matches the engine's
   cursor contract — no off-by-one across federated/join plans.

3. **Residual no-wrong-rows property is genuinely applied.** The fake driver has
   `PushdownProfile::None` and `scan()` hands back ALL rows ignoring the pushed WHERE/LIMIT
   (`tests/oneshot.rs:83–90`). With `None`, the pushdown leaves WHERE/LIMIT as `CombineOp::Filter`/
   `Limit` residuals (confirmed: `PushedQuery::is_bare` + `CombineOp` variants in `physical.rs`),
   and `MiniEvaluator` re-applies them: `residual_where_refilters_over_returned_rows` proves
   `WHERE id > 1` trims 3→{2,3}, and `headline_read_returns_rows_through_real_executor` proves
   `LIMIT 1` trims 3→1. This is the t20 property exercised against a real over-returning source,
   not a stub that pre-trims.

4. **`ReadDriver` is a genuine extension point, not a fig leaf.** The seam takes the owned
   `&ScanNode` (source `SourceId` + `PushedQuery` + resolved `Schema`) and returns an owned
   `RowBatch` — no vendor SDK type crosses it (`read.rs:14–18, 33–49`). A real E4 driver already
   owns a pure `ReadPlan`/`plan_read`; `ScanNode.pushed` is exactly the pushed query it would
   translate inside its own boundary, and it decodes to `RowBatch` via the codec registry it already
   holds. **The keying is coherent**: `execute_read` resolves the driver via
   `DriverId::new(scan.source.as_str())` (exec.rs 60, 87–89), and the planner sets `scan.source` to
   `driver.id().as_str()` (`crates/core/src/plan.rs:99–106` `source_of`). Same id space — a real
   driver registered under its own `DriverId` matches without redesign. The deferral of real-driver
   `scan` registration is per ticket scope and acceptable; the seam shape would accept a real driver
   unchanged.

---

## Other surfaces

### 1. Confinement invariants — PRESERVED

- **Runtime minimal spine untouched**: `qfs-exec` does **not** depend on `qfs-runtime`
  (`crates/exec/Cargo.toml` deps = core/parser/pushdown/engine/tokio/async-trait/serde). The
  runtime confinement test (`dep_direction.rs:199–346`) fires only on `qfs-runtime` consumers, so
  it is unaffected.
- **`qfs-plan` purity closure stays tokio-free**: the purity test (`crates/plan/tests/purity_deps.rs`)
  BFSes only `qfs-plan`'s transitive closure. Nothing in the pure spine depends back onto
  `qfs-exec`, so tokio in `qfs-exec` cannot enter that closure. Confirmed structurally.
- **`qfs-cmd` stays logic-free (C4)**: the new `qfs-cmd → qfs-exec` edge is permitted — `qfs-exec`
  is none of the five forbidden domain crates (`qfs-lang/plan/driver/codec/parser`). The C4 test
  (`dep_direction.rs:61–93`) only forbids those five + asserts core/server presence; it does not
  reject `qfs-exec`. `dispatch_run` (`crates/cmd/src/lib.rs:209–245`) is genuinely thin: it resolves
  the statement source, picks the format by TTY, builds an empty `ReadRegistry`, and delegates all
  execution to `qfs_exec::run_oneshot`. No domain logic leaked into cmd.
- **No spine inversion**: no spine crate depends on `qfs-exec`; the edge flows up only
  (`qfs → qfs-cmd → qfs-exec`).

### 2. Error-envelope superset — GENUINELY BACKWARD-COMPATIBLE

`error_envelope` (`output.rs:129–143`) emits `{"error":{"code","kind","message","path"?,"detail"?}}`
— it **keeps** t01's `code` (the fine-grained stable identifier) AND **adds** t29's `kind` (the
coarse class). An agent pinned to t01's `code` keeps working; a t29 agent reads `kind`. The
`kind`↔exit-code mapping is 1:1 by construction (`ErrorKind::exit_code`, `error.rs:81–94`): each
`ErrorKind` returns exactly one `ExitCode`. The rationale split (coarse `kind` for top-level
recovery, fine `code` for identity) is documented at `error.rs:1–16`. Confirmed backward-compatible.
One nuance for the record: `qfs-cmd::report_error` (the *non-`run`* arms: shell/serve/account) still
emits the **old** two-field `{"code","message"}` envelope (lib.rs:310–327). That is correct — those
arms return `CfsError` directly and predate t29; only the `run` path is on the new superset. The two
envelopes are not in conflict (the superset is a superset of the old), but the codebase now has two
envelope builders. See CO-t29-2.

### 3. Exit-code map — STABLE; data/error stream separation correct

`ExitCode` is pinned `0/2/3/4/5/6` (`error.rs:22–36`). Note `1` is intentionally **not** in the
`run` map — `1` is the legacy cmd-level "structured error" code for the non-`run` arms (lib.rs:191);
the `run` path never returns `1`, which keeps the agent contract clean (a `run` failure is always
one of 2/3/4/5/6). Data→stdout / errors→stderr is enforced at the seam: `run_oneshot` renders rows/
plan to `streams.out` and routes every error through `renderer.error(&err, streams.err)`
(`lib.rs:137–144, 173–194`). The test `oneshot_read_json_exit_zero_with_rows` asserts `err.is_empty()`
on the success path. Confirmed.

### 4. Destructive-set detection from plan metadata — CONFIRMED grammar-agnostic

`is_destructive_set` (`lib.rs:225–235`) reads `node.irreversible` AND `node.est_affected`
(`Affected::Exact/AtMost(c>1)` or `Unknown`) — pure plan metadata. It never sniffs keywords. A
single-row-bounded irreversible effect (`Exact(0|1)`, `AtMost(1)`) previews at exit 0; an unbounded
`REMOVE` (`Unknown`/`AtMost(n>1)`) gates to exit 4. The addressing pre-scan keyword list
(`REMOVE` etc.) is for *addressing validation only* and is explicitly walled off from destructive
detection (`addressing.rs:10–12`). Correct separation.

### 5. Addressing pre-parse lexical scan — ACCEPTABLE localized workaround; the deeper fix is a carry-over

The workaround (`addressing.rs`) scans the raw text before parse because the lexer drops a path's
leading `/` (`/db/x` and `db/x` both lower to `[db,x]`), so absoluteness is not recoverable from the
AST. **My structural assessment: this is an acceptable localized workaround for t29, and crucially it
does NOT create a mis-resolution hazard downstream.** The reason the lost-`/` distinction is *safe at
the driver boundary* (the Lead's specific worry): a relative path never reaches a driver as a path.
The planner routes by the first path segment through `mounts.resolve_path` and tags each scan with a
`SourceId` = `driver.id()` (`crates/core/src/plan.rs:99–106`); the `ReadDriver` is then keyed by
**driver id**, not by the raw `/`-prefixed string. So `/mail/inbox` and `mail/inbox` resolve to the
*same* driver and the driver receives a `ScanNode`, not a path string — there is no path through
which a relative address reaches a driver and is mis-resolved. The addressing gate is therefore a
**UX/contract guard** (reject relative in one-shot mode because there is no cwd), not a
correctness-critical resolver.

That said, the lexical scan is a second, ad-hoc tokenizer living beside the real lexer, and it can
drift: it understands `'`/`"` strings and `|>` but not comments, escaped quotes, or other lexer
subtleties the real grammar may grow. It is coarse-but-sufficient *today*. The principled fix is to
have the lexer/grammar **preserve an absolute-vs-relative bit** on the path token (or expose a
parsed "is this address absolute" flag on the AST node) so the addressing gate can run over the AST
and the second tokenizer is deleted. That touches `qfs-parser` grammar and is **out of t29 scope**.
Recorded as **CO-t29-3**.

### 6. Owned DTOs only across engine↔CLI seam; own table formatter — CONFIRMED

The renderer sees only owned `RowSet` / `PlanPreview` / `ExecError` (`output.rs:1–3, 26–44`); no
vendor SDK type crosses. `RowSet` serializes values to *natural* JSON (`row.subject` is a string,
not `{"Text":"…"}`) via the hand-written `ValueJson` serializer (`dto.rs:79–139`) — the right
agent-facing shape. The table formatter is an in-house ~60-line fixed-width aligner with no
`comfy-table` dependency (`output.rs:181–225`), consistent with ADR-0002/0003's anti-heavy-dep
precedent and documented at `output.rs:5–12`. (Note: the ticket's draft signature mentioned
"`comfy-table`/owned formatting"; the Constructor chose owned-only, which is the more defensible
ADR-aligned call — confirmed correct, not a deviation to flag.) Confirmed.

---

## Concern requiring attention (per Critical Review Policy: ≥1 concern + proposal)

### Observation 1 (genuine gap, non-blocking) — `qfs-exec`'s purity is enforced only negatively; add a positive mechanical guard

The whole topology rests on the claim "`qfs-exec` sits above the spine and the pure spine never
depends back onto it." Today that holds, but **no test asserts it**. The runtime confinement test
pins `qfs-runtime`'s allowlist and `nothing_depends_on_cmd` pins `qfs-cmd`'s consumers; there is no
analogous guard for `qfs-exec`. The only thing that would catch a regression (e.g. a future
`qfs-core → qfs-exec` edge, or `qfs-exec → qfs-runtime`) is the *generic* runtime-leaf check, and
only once such an edge actually pulls the runtime in. A spine crate gaining a plain
`→ qfs-exec` edge (no runtime involved) — which would invert the layering and is exactly the
structural property the crate docs promise — would pass CI silently.

**Proposal (structural, small):** add a `qfs-exec` confinement test to `crates/cmd/tests/dep_direction.rs`
(or a sibling), asserting two directions, mirroring the existing runtime/secrets/http-core guards:
(a) `qfs-exec`'s workspace deps are exactly `{qfs-core, qfs-parser, qfs-pushdown, qfs-engine}` (and
NOT `qfs-runtime`), and (b) the only crate depending on `qfs-exec` is `qfs-cmd`. This converts the
crate-doc promise (lib.rs:7–22) into a mechanical invariant and makes the "does the separation hold
long-term" answer enforceable rather than aspirational. This is a small additive test; I leave it to
the Lead whether to fold it into a t29 fix or take it as **CO-t29-4** — I do not consider it a
blocker because the property holds at this commit.

---

## Carry-overs (future tickets, not t29 blockers)

- **CO-t29-1 — Unified driver-facet registration.** A real driver registers three disjoint facets
  (mount/read/write) in three registries under one `DriverId`. Introduce a single
  facet-bundle registration so a gmail driver binds all three coherently and a partial/mismatched
  registration is impossible. Cross-cutting (`qfs-core` + `qfs-runtime` + `qfs-exec`). This is the
  real answer to the headline question's part (b).
- **CO-t29-2 — Single error-envelope builder.** `qfs-exec::output::error_envelope` (superset) and
  `qfs-cmd::report_error` (legacy two-field) are two envelope builders. Once the shell/account arms
  route through the exec error contract, collapse to one builder so the JSON envelope has one source
  of truth.
- **CO-t29-3 — Lexer-level absolute-path bit.** Preserve absolute-vs-relative on the path token in
  `qfs-parser` so the addressing gate runs over the AST and the ad-hoc `addressing::lex_words`
  second tokenizer is deleted. Safe to defer (no driver mis-resolution hazard today, see surface 5).
- **CO-t29-4 (optional) — `qfs-exec` positive dep guard.** See Observation 1; promote if not folded
  into a t29 fix.

## Review Notes

Analytical review only — no tests, builds, or clippy were executed (Architect QA domain). Internal
test correctness is the Constructor's gate; E2E exit-code/JSON-contract validation is the Planner's.
My ruling is on structure, translation fidelity, and confinement.
