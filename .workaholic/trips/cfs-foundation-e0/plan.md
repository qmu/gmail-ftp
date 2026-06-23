---
instruction: "[night /trip — empty instruction; recorded assumption] Develop and build the cfs foundation (epic E0 of the RFD-0001 Rust rebuild): ticket t01 (Rust workspace + single-binary scaffold, CLI+server, typed module/registry/trait seams, lints+CI+cross-compile), and gated on it, ticket t02 (parser-library decision spike: winnow vs chumsky, ADR + thin parser-skeleton crate). Scope fixed at invocation; do not expand."
phase: complete
step: done
iteration: 0
updated_at: 2026-06-23T00:15:00+09:00
---

# Trip Plan

## Initial Idea

[night /trip — empty instruction; recorded assumption] Develop and build the cfs foundation (epic E0 of the RFD-0001 Rust rebuild): ticket t01 (Rust workspace + single-binary scaffold, CLI+server, typed module/registry/trait seams, lints+CI+cross-compile), and gated on it, ticket t02 (parser-library decision spike: winnow vs chumsky, ADR + thin parser-skeleton crate). Scope fixed at invocation; do not expand.

## Plan Amendments

### A1 — Planning converged at round 1 (Lead, 2026-06-22)
One-turn review (Step 2) returned **unanimous approval**: every per-artifact decision was
"Approve with observations" or "Approve with minor suggestions"; **zero "Request revision"**.
Per the trip-protocol Consensus Gate, planning completes with no respond/escalate/moderate
rounds. Plan is fixed. The plan = `directions/direction-v1.md` + `models/model-v1.md` +
`designs/design-v1.md`, with the carry-over acceptance criteria in A3 below.

### A2 — Dev-environment ground truth (Lead, 2026-06-22, corrects design-v1 risks R8/toolchain)
The Lead verified the build environment directly (the Constructor's design-v1 reasoned from a
momentarily network-restricted subagent shell and over-rated several risks):
- **crates.io is reachable** from both the lead shell and a fresh subagent (`index HTTP 200`);
  a real `cargo fetch` succeeded. The global cargo cache has been **pre-warmed** (76 MB) with the
  full transitive closure: `clap 4.6.1 (derive)`, `thiserror 2.0.18`, `anyhow 1`, `serde 1 (derive)`,
  `serde_json 1`, `tracing 0.1`, `tracing-subscriber 0.3`, `winnow 1.0.3`, `chumsky 0.13.0`.
  → **R8 (offline fetch) is retired.** Use external deps freely; no std-only fallback needed.
  The t02 winnow-vs-chumsky head-to-head can run for real. Builds also work `--offline`.
- **Toolchain pin** must be `channel = "stable"` (NOT `"1.96.0"`) in `rust-toolchain.toml` — the
  Constructor's correct catch; pinning the numeric version triggers a failing offline rustup
  download. Installed: rustc/cargo/clippy/rustfmt 1.96.0; targets `aarch64-` and
  `x86_64-unknown-linux-gnu`.
- **x86_64 cross-link** has no cross-linker locally → the `cfs` binary links only for native
  aarch64 here; x86_64 binary link is **CI-only** (lib crates still cross-compile). Native
  aarch64 build/test is the local proof.
- **wasm32** target is not installed and is **deferred per the tickets** (t01/t02 out-of-scope);
  the t02 wasm32 size datapoint is CI-only / parked. Not a blocker for E0 acceptance.

### A3 — Carry-over acceptance criteria from round-1 reviews (Lead, 2026-06-22)
Folded into the Coding Phase as acceptance criteria (non-blocking review suggestions; no artifact
revision required):
- **G1 / C1** — the reserved-keyword golden set lives in ONE cited committed fixture in `cfs-lang`;
  the golden test asserts against that single source (no drift between two hand-transcriptions).
- **C4** — a `cargo metadata`-based test fails the build if `cfs-cmd` gains a direct dependency on
  `cfs-lang/plan/driver/codec/parser` (mechanically enforce "cmd is logic-free" + dep direction).
- **G3** — purity invariant proven by a plain in-crate `#[test]` instantiating a no-I/O dummy
  `Driver`/`Codec` (NOT via `trybuild`; the trait signature is the primary enforcer).
- **C5** — declare the intended `cfs-core → cfs-parser` edge direction now so E1 cannot introduce a
  cycle.
- **R5/P1 wording** — morning report states cross-compile status precisely: "native aarch64 built &
  tested locally; x86_64 + wasm32 are CI-only".
- Retained spike loser (winnow or chumsky) carries a banner pointing at the ADR so it is not misread
  as a live second parser.

## Progress (continued — coding phase)

- [x] **[Constructor]** coding/concurrent-launch — implemented t01: 9-crate workspace under `crates/` (cfs bin + cmd/core/lang/plan/driver/codec/server/parser), three open registries, Driver/Codec traits with type-level purity, frozen keyword fixture, Effect/Plan placeholders, clippy deny-warnings + rustfmt + CI. **29 unit/integration tests green**, fmt/clippy/build clean on native aarch64. Decision **D1** recorded (CfsError/Path in cfs-driver to keep spine acyclic). Commit `9557bda`.
- [x] **[Architect]** coding/review-and-testing — analytical review of t01: **Approve with minor suggestions**; acyclic spine verified across all 9 manifests, D1 endorsed, guards G1–G6 + C4/C5 pass, E1/E2/E7 confirmed to land inside seams. Commit `415e9f1`.
- [x] **[Planner]** coding/review-and-testing — E2E validation of t01 CLI: **E2E approved**; 7/7 scenarios pass, no panics, `--json` envelope `{"error":{"code","message"}}` parseable by python3 + jq. Commit (e2e review). Exit-code convention 0/1/2 consistent.
- [x] **[Lead]** t01 ACCEPTED — all three coding gates green, no iteration required.
- [x] **[Constructor]** coding/concurrent-launch — implemented t02: throwaway spike crate `spikes/parser-spike` (`publish = false`) parsing `FROM <path> |> WHERE <expr> |> SELECT <cols>` in **both winnow 1.0.3 and chumsky 0.13.0** into ONE shared AST; committed golden error corpus (`spikes/parser-spike/tests/golden/errors.txt`) + cross-parser AST-equality test (both green). **Decision LOCKED in `docs/adr/0001-parser-library.md`: winnow** — token-level expected-sets (vs chumsky's char-level) better suit the AI structured-error path (RFD §5); recovery not decisive (neither lib surfaces multi-errors out of the box); winnow has **zero transitive deps** (chumsky pulls `stacker`/`psm`, a C-built wasm-hostile stack manipulator), ~1.34 s vs ~4.70 s clean compile. Wired `cfs-parser` to winnow behind the owned `ParseError` (span+expected-set+machine code) + `parse_statement(&str) -> Result<Stmt, ParseError>`; winnow confined to the crate-private `grammar` module (no-vendor-leak audit test green). Keyword surface text sourced from the frozen `cfs_lang::Keyword` set. fmt/clippy(`-D warnings`)/build/test all green on native aarch64 (cfs-parser 7 tests, spike 3 tests, no t01 regression). Records below.

### t02 review + acceptance (Lead, 2026-06-23)
- **[Architect]** analytical review of t02: **Approve with minor suggestions** — ADR evidence-backed (not a §9 restatement), G6 no-vendor-leak PASS (production graph exactly `cfs-parser → {cfs-lang, winnow}`; winnow confined to private `grammar` module), E1-readiness PASS. Minor E1 carry-overs: typed operator surface in `cfs-lang` (AND/LIKE/`|>` currently string literals in grammar.rs); classify `UnknownKeyword` by frozen-set membership not first-char case; delete the spike after E1 so chumsky/`psm` stops loading dev builds. Commit `6926dc1`.
- **[Planner]** E2E of t02: **E2E approved** — spike `compare` reproduces the golden corpus; `parse_statement` returns structured `ParseError{at, code, expected, message}` (machine-branchable, no winnow type leaked); panic-free on 6 adversarial inputs. Commit `3f52dec`.
- **Doc defect found by both reviewers + fixed by Lead**: ADR and `crates/parser/src/lib.rs` claimed the wasm32 build "is validated in CI", but CI only has a commented-out placeholder (wasm32 is deferred). Reworded both to "deferred; CI carries a parked placeholder". Workspace re-verified green (fmt/clippy/38 tests).
- **[Lead]** t02 ACCEPTED — both coding gates green, one trivial doc correction applied, no iteration required.

### A4 — t02 implementation decisions (Constructor, 2026-06-23)
- **Spike location**: a dedicated workspace member `spikes/parser-spike` (members glob extended to `["crates/*", "spikes/*"]`), `publish = false`, rather than `crates/parser/spikes/` or `examples/`. Rationale: a separate crate keeps winnow AND chumsky entirely out of `cfs-parser` (cfs-parser depends on winnow ONLY), and lets the shared AST be reused by the comparison test + example without leaking into the production crate. The lint relaxation (`#![allow(clippy::unwrap_used, ...)]`) is scoped to the spike crate only; `cfs-parser` keeps the strict workspace policy (panic-free `grammar` module).
- **wasm32 (R5/A2 carry)**: target not installed, deferred per ticket — NOT added. ADR records the wasm32 datapoint as **CI-only / deferred** with the qualitative note (winnow zero-dep/macro-free/wasm-clean; chumsky's `psm` C build is wasm-hostile). Not a blocker; corroborates winnow.
- **A3 retained-loser banner**: BOTH spikes retained under `spikes/parser-spike` as comparison evidence; the chumsky spike (the loser) carries a header banner pointing at `docs/adr/0001-parser-library.md`. The winnow spike likewise banner-marked NOT production.
- **Honest evidence note**: chumsky's multi-error recovery requires explicit `.recover_with(...)` wiring; out of the box `chumsky-recovery-count` is 1 for every corpus case. The ADR is explicit that recovery is therefore not free in either library and does not clear the bar to override the RFD §9 winnow default.

## Progress

- [x] **[Planner]** planning/artifact-generation — wrote `directions/direction-v1.md` (business vision for cfs E0 foundation: value proposition, business/strategic risk, three user personas including the AI agent operating cfs, system positioning for Rust single-binary + closed-core/open-registries, and the business rationale for a night on E0 before the 39 downstream tickets). Night-mode empty-instruction assumption recorded prominently in the artifact.

### t03 / t04 — implemented then caught up to full protocol (Lead, 2026-06-23)
- **[Constructor]** t03 lexer in `cfs-lang` (zero-dep, wasm-clean), 60 tests green, commit `0ec2168`. t04 full pipe-SQL grammar + AST + closed-core governance (4 statement / 18 pipe-op variants locked), winnow over token stream behind owned `ParseError`, parser 36+2 tests, commit `f96bfee`.
- **Process correction**: t03/t04 were first implemented solo (Constructor only). Per user directive ("trip it"), the team protocol was restored — Architect review + Planner E2E run retroactively before continuing.
- **[Architect]** catch-up review: both **Approve with observations** (G1 single keyword source verified byte-for-byte vs RFD §3; G6 no winnow leak audited; spine acyclic). Commit `017ce5c`.
- **[Planner]** catch-up E2E: **E2E approved**, 56/56 checks, all DDL/effect/query forms parse, governance errors machine-branchable, no panics on 12 adversarial inputs, secret hygiene holds. Commit `d300a69`.
- **[Lead]** t03 + t04 ACCEPTED. Remaining tickets t05–t41 run the full Constructor→Architect→Planner cycle each.

### t05 ACCEPTED (Lead, 2026-06-23)
- **[Constructor]** new leaf crate `cfs-types` (value/type/schema/predicate/unify), `cfs-codec` placeholders reconciled onto it, +23 tests, green. **[Architect]** Approve with observations (spine acyclic, D2 `DriverId`-in-types endorsed). **[Planner]** E2E approved 31/31 (algebra + serde round-trip).
- **Carry-over → t13**: two schema notions exist — `cfs_driver::NodeSchema` (untyped `Vec<String>`) vs `cfs_types::Schema` (typed). The driver contract (t13) must reconcile them (lean: NodeSchema absorbs Schema + archetype tag, or an explicit adapter). E4 mount key should reuse `cfs_types::DriverId`.

### t09 ACCEPTED (Lead, 2026-06-23)
- **[Constructor]** typed effect-plan DAG in `cfs-plan` (EffectKind/EffectNode/Plan + validate/topo/PREVIEW/COMMIT, PlanApplier impure seam), +22 tests. **[Architect]** Approve with observations. **[Planner]** E2E approved 46/46.
- **Carry-overs**: (O2) `WriteVerb`↔`EffectVerb` drift → put canonical exhaustive match in `cfs-core` at E1 and drop the mirror; (O3) define `VfsPath`↔driver `Path` adapter at E4/t13; (Planner) add `AppliedEffect::new` public ctor so out-of-crate driver appliers can build success values (needed by t13/E4).

### t13 ACCEPTED (Lead, 2026-06-23)
- **[Constructor]** full Driver contract (archetype, typed schema, per-node capabilities, procs, pushdown, prelude, @version, `applier()` impure seam); reconciled NodeSchema→Schema, Path↔VfsPath adapter, AppliedEffect::new. **[Architect]** Approve with observations. **[Planner]** E2E approved (external driver implemented). **Refinement** (commit `060a457`): Capabilities builder (external-constructible), `Driver::id()`+longest-prefix `MountRegistry::resolve_path` router, wider pushdown vocab (aggregate/distinct/group_by)+accessors, Verb↔Capability tie-test. 151 tests green.
- **Carry-overs**: O4 validate mount() strings; t14 planner consumes pushdown via accessors (done).

### t15 ACCEPTED (Lead, 2026-06-23)
- **[Constructor]** codec registry + 6 builtins (json/jsonl/yaml/toml/csv/md+frontmatter), struct/array bridge, EXPAND/path-access, structured errors. **[Architect]** Approve with observations. **[Planner]** E2E approved.
- **Defect caught by BOTH gates + fixed**: `Value::Struct` lost nested field names → `a.b.c` over decoded data returned None. Fixed by making `Value::Struct(Fields)` carry named ordered fields; decode→access regression test added (commit `6918d35`). 184 tests green. This was a t05-rooted core-model fix landed before drivers depend on it.
- **Carry-over**: defaulted `Codec::infer_schema` (DESCRIBE-without-materialization) deferred to driver/DESCRIBE ticket (non-breaking later).

### t06 ACCEPTED (Lead, 2026-06-23)
- **[Constructor]** name resolution in `cfs-core` (CALL→procedures via mount router, receiver-typed prelude aliases fail-closed, capability gating); wired core→parser edge; EffectVerb exhaustive matches. +16 tests, 200 green. **[Architect]** Approve with observations. **[Planner]** E2E approved 14/14 (no I/O during resolve).
- **Minor carry-overs**: O-A reconcile ARCHITECTURE.md "Reserved edge" wording (now wired); O-D add a comment that `cfs_parser::EffectVerb` must NOT become `#[non_exhaustive]` or the cross-crate exhaustive verb guard silently defeats. (Fold into t07.)

### t07 ACCEPTED (Lead, 2026-06-23)
- **[Constructor]** pure evaluator `cfs-core::eval` (AST→`PlanSource` relational tree + write `Plan`, schema threaded via t05 algebra, verb pipeline no-`_`, RETURNING typed). +18 tests, 219 green. **[Architect]** Approve with observations. **[Planner]** E2E approved (24 checks, poisoned applier never fired).
- **Carry-overs for t10/t14**: O-t07-1 `PlanSource` lives in eval module — t14 builds its own `LogicalPlan` from the AST (recommended) rather than consuming PlanSource; t10 walks `cfs-plan` types only (no PlanSource needed) — keeps spine clean. O-t07-3 (key): `PlanSource` is schema-threading IR, NOT pushdown-ready — `Filter`/`Project`/`Join` drop predicate/expr/on ASTs; **t14 must source predicates from the AST**. O-t07-2 `Join` does raw column concat (no collision policy) → add `Schema::join` at t14. O-t07-4 add a t04 freeze test forbidding `#[non_exhaustive]` on `EffectVerb`. O-t07/Planner: surface a `late_bound` marker in PREVIEW/DESCRIBE for empty-fallback schemas.

### t08 ACCEPTED — E1 LANGUAGE CORE COMPLETE (Lead, 2026-06-23)
- **[Constructor]** stdlib (string/path/date/number/conditional/context/aggregate/table-valued) + function registry + driver-prelude mechanism (purity-checked, DriverId-namespaced) + function-call typing. **[Architect]** Approve with observations. **[Planner]** E2E BLOCKED on `FORMAT_DATE(i64::MAX)` panic → **[Constructor]** fix `d032a2b` (date fns total, structured Domain error, i64::MAX/MIN regression tests), 247 green. ACCEPTED.
- **E1 done**: t03 lexer, t04 grammar/AST/governance, t05 types, t06 name-resolution, t07 evaluator, t08 stdlib. Parser→resolve→evaluate→effect-plan pipeline is end-to-end and pure.
- **Carry-overs for E2/E4**: prelude gate (`register_prelude`) not yet on the resolution path — E4 should route `Driver::prelude()` through the registry. `PlanNode` (stdlib READ/http.get DTO) not folded into `cfs_plan::EffectNode` — **t10 owns lifting Read/HttpGet into the cfs_plan DAG**. Nested-aggregate typing deferred to t10+. Architect t07 guidance: t10 should be a `cfs-runtime` crate walking `cfs-plan` types only (no PlanSource, avoid runtime→core inversion).

### t10 implemented (Constructor, 2026-06-23) — interpreter, auto-batching + bounded parallelism
- **[Constructor]** New crate `cfs-runtime` (spine `cfs-runtime → {cfs-plan, cfs-types}` ONLY — no `cfs-core`, no `PlanSource`, per Architect t07 guidance; verified by `cargo tree`). The async effect interpreter: `Interpreter::commit(plan, caps)` walks the `cfs-plan` DAG in incremental topological **frontiers** (`schedule::Frontier`), **auto-batches** each frontier per owned `(DriverId, EffectKind)` key into one `ApplyDriver::apply_batch` call (`batch::coalesce` — N+1 → 1), runs independent groups concurrently under two-level `tokio::Semaphore` caps (`ConcurrencyLimits{global, per_driver}`), applies per-leg timeout + bounded retry (`RetryPolicy`) that **never retries irreversible legs**, re-checks capability gating (`CapabilitySet`) before dispatch, and threads the owned, serializable `Outcome` ledger (`LedgerEntry`/`LegStatus`, metadata only — no payloads/tokens). `Interpreter::preview` is the dry-run (no driver calls). Failed-node → transitive-dependents-skipped (t09 semantics) preserved under parallelism via a taint-propagating frontier. **+16 integration tests** (batching/ordering/concurrency≤global/per-driver cap/failure-skip/irreversible-no-retry/REMOVE-irreversible/capability-denied/preview-noop/cyclic-reject/unregistered-driver/golden-json), all using an in-memory mock `ApplyDriver` (**no live creds, no network**). **263 tests green** (247 + 16). fmt/clippy(`-D warnings`)/build all green.
- **tokio CONFINED** to `cfs-runtime`: `cfs-plan`'s purity dep-closure test (`tests/purity_deps.rs`, forbids tokio/reqwest/…) **still passes** — confirmed green in the workspace run.
- **READ/source-node boundary decision (t08 carry-over resolved)**: `cfs_plan::EffectKind` ALREADY carries `Read`/`List` as first-class effect nodes, so the interpreter executes them like any other effect (dispatched to the target driver under `Read`/`List`) — a `READ` leaf feeding an `UPDATE … FROM <read>` is just an upstream frontier node. The *separate* `cfs_core::PlanNode` DTO (stdlib `READ`/`http.get` table-valued **expression** source) is a **core-local representation**; **lifting it into a `cfs_plan::EffectNode` is the evaluator's job (E1/t14), NOT the runtime's** — the runtime deliberately does not depend on `cfs-core`. Documented in `crates/runtime/src/lib.rs`.
- **Recorded assumptions / parks (deferred per ticket scope)**: (1) the t13 sync `Driver::applier()`→async `ApplyDriver` bridge is an **E4 adapter** (E0 tests use a mock implementing `ApplyDriver` directly); the runtime defines its own consumer-side async `ApplyDriver` trait so it can own tokio while `cfs-driver`/`cfs-plan` stay I/O-free. (2) On a per-leg **timeout** the runtime cannot know which legs of a batch landed, so it maps the whole subset to `TimedOut` and retries only reversible legs — conservative-safe. (3) Pushdown/same-source subtree collapse and cross-source transactions (cp) are explicitly out-of-scope (E2/E3). (4) `CALL` groups fold the `ProcId` into the batch key so distinct procedures never merge.

### t10 ACCEPTED (Lead, 2026-06-23)
- **[Constructor]** `cfs-runtime` async interpreter (auto-batch N+1→1, two-level bounded parallelism, retry never-irreversible, apply-time capability re-check, deterministic topo ledger). **[Architect]** Approve with observations (scheduler proven race-free; single-threaded Frontier mutation). **[Planner]** E2E approved 16/16. **Refinement** `5fa33de`: bounded future admission (peak≤global — ticket criterion), preview drives Frontier (one skip-propagation impl), runtime-confinement dep test. 266 green; cfs-plan purity test still green.
- **Carry-overs**: E4 must bridge sync `Driver::applier()` → async `ApplyDriver`; carry per-group batch-size into the t12 audit ledger/tracing for traceable coalescing.

### t11 ACCEPTED (Lead, 2026-06-23)
- **[Constructor]** `cfs-txn` pure crate + runtime bridge: content-addressed `EffectKey` idempotency, `Precondition`→typed `Conflict` optimistic concurrency, SingleSourceAcid vs CrossSourceSaga (reverse-order compensation, cp/mv verify-before-delete). +25 tests, 291 green. **[Architect]** Approve with observations. **[Planner]** E2E approved 14/14.
- **Carry-overs ROUTED TO t12**: (1) `AuditLedger::has_intent` is documented as crash-detection but has zero consumers — apply-once currently holds only for driver-idempotent legs; a crash between intent-append and apply re-applies a plain Insert/Call. t12 must make `has_intent` real (resume reconcile pass) AND scope-down the t11 doc claim to match. (2) add `EffectError::Conflict{version}` so the interpreter bridge carries the REAL world version (not the expected token) instead of inferring conflict from terminal-reason text. (3) E4 must wire `commit_txn`'s CrossSourceSaga path to drive `SagaExecutor::run_saga`. (4) E8 file ledger needs fsync-intent-before-apply durability.

### t12 ACCEPTED — E2 EFFECT-PLAN & RUNTIME COMPLETE (Lead, 2026-06-23)
- **[Constructor]** observability (deterministic TraceId, secret-free spans/events) + resolved t11 carry-overs (has_intent reconcile real → Indeterminate for non-replay-safe legs; EffectError::Conflict{version} threads real world version). **[Architect]** Approve with minor suggestions (both t11 safety carry-overs genuinely closed). **[Planner]** E2E BLOCKED on `LegOutcome::Conflict` newtype not serializable under internal serde tagging → **[Constructor]** fix `e3746bc` (struct variant + all-variants round-trip test + run_acid alignment), 302 green. ACCEPTED.
- **E2 done**: t09 effect-plan, t10 interpreter, t11 transactions/idempotency, t12 audit/observability.
- **Carry-overs**: E8 durable JSONL file ledger + circuit-breaker + `cfs ledger show` CLI (AuditLedger seam ready); E8 fsync-intent-before-apply durability; E4 wire commit_txn CrossSourceSaga → SagaExecutor::run_saga.

### t14 ACCEPTED — E3 DRIVER CONTRACT & FEDERATION COMPLETE (Lead, 2026-06-23)
- **[Constructor]** `cfs-pushdown` (LogicalPlan from AST, per-PushdownProfile split, federation) + `cfs-engine` (in-house MiniEvaluator; ADR-0002 rejects DuckDB on wasm/footprint), `Schema::join` collision policy. +30 tests, 332 green. **[Architect]** Approve with observations. **[Planner]** E2E approved 26/26.
- **E3 done**: t13 driver contract, t14 pushdown+combine engine, t15 codec registry.
- **Carry-overs → E4**: (O1) federated-join residual column references must resolve against the joined output schema (raise "ambiguous column" instead of defaulting left). (Planner) `cfs_core::source_registry` should consult each driver's `capabilities().select` and `register_unreadable` SELECT-denied mounts so the plan-time gate fires on the live `plan_query` path (runtime t10 re-check already guards, so no live hole). (O3) integer SUM via f64; (O2) approximate EXPAND/Aggregate residual schema.

### t27 ACCEPTED — E5 CREDENTIALS COMPLETE (Lead, 2026-06-23)
- **[Constructor]** `cfs-secrets` leaf: `Secret(Zeroizing)` redaction (no Serialize/Clone/Deref; expose() only), backends InMemory/Env/Local(ChaCha20Poly1305+argon2id+0600)/Worker(wasm), fail-closed multi-account precedence, scope grant/deny. +36 tests, 368 green, wasm32 builds. **[Architect]** Approve with minor suggestions (no leak path). **[Planner]** E2E approved (canary absent everywhere; no-Serialize proven by compile error).
- **Carry-overs**: update `ARCHITECTURE.md` (stale — omits cfs-secrets + edges); `Zeroizing` LocalStore transient decrypt buffer; add trybuild no-Serialize compile-fail gate; directory-ACL threat note.
- **Disk**: worktree-local `.cargo/config.toml` (gitignored) sets `debug=0`/`incremental=false` — target/ 4.5G→269M, 6.4G free. Keeps the 20+-crate build from wedging on the shared near-full disk.

### t16 ACCEPTED (Lead, 2026-06-23) — first driver; pattern locked
- **[Constructor]** `cfs-driver-local` (sandboxed FS, glob, atomic writes, codec contents, cp/mv) + `PlanApplierBridge` (sync Driver→async ApplyDriver via spawn_blocking). **[Architect]** Approve with minor suggestions. **[Planner]** E2E approved (sandbox holds, no escape). **Refinement** `1f37f44`: generic runtime-leaf confinement test (scales to 11 more drivers), size+hash cp/mv verify, structured SandboxEscape/CapabilityDenied bridge errors, est_affected carry-through, ARCHITECTURE.md caught up. 399 green.
- **Driver pattern for E4**: impl `cfs_driver::Driver`; register via `local_apply_driver`-style bridge into `DriverRegistry`; contents via codecs (t15); auth via `cfs-secrets` (t27); tests use fakes/mocks/temp — NO live network/creds (live E2E parked for t38 harness).
