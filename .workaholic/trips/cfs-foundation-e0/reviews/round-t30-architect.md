# Architect Analytical Review — t30 Server Runtime + `/server` Self-Config Driver

- Reviewer: Architect (Neutral / structural bridge)
- Commit: `733bb0b`
- Ticket: `20260622214650-t30-server-runtime-and-self-config-driver` (begins epic E7)
- Scope: analytical / architectural review only (no test/build/clippy execution)

## Decision: **Approve with observations** (one carry-over, no t30 blocker)

The structure faithfully translates RFD §6/§8 ("the server IS a driver; boot is replay").
The two headline structural decisions are adjudicated below; both are sound for t30, with
one genuine unproven gap that I rule a **carry-over to E1/grammar (Display renderer) +
the relevant E7 binding ticket**, not a t30 blocker.

---

## HEADLINE 1 — The pure-commit-seam confinement choice: **SOUND. Ruled correct.**

### The structural fact that decides it
There are **not** two commit paths. There is **one** commit algorithm —
`cfs_plan::commit(plan, applier, on_applied)` (`crates/plan/src/apply.rs:178`), re-exported
through `cfs-core` — driven by **pluggable `PlanApplier` implementations**:

- the CLI/shell one-shot path drives a `RecordingApplier` (records, mutates nothing — the
  t28/t29 state of the write path);
- the `/server` runtime drives `ServerConfigApplier` (`crates/server/src/runtime.rs:63`),
  a real `PlanApplier` that takes the `RwLock` write guard and applies the op.

Both walk the **same** topological order, the **same** taint/skip semantics, the **same**
`on_applied` ledger funnel, and the **same** `CommitReport` failure accounting. The async
`Interpreter` in `cfs-runtime` is a *third* `PlanApplier` host (batching/parallelism/retry
for network effects) — but it is not the "canonical" commit and the others "shortcuts": all
three share the one `commit` driver. So routing `/server` through the synchronous seam does
**not** fork commit semantics (idempotency, audit, irreversibility). Concretely:

- **Idempotency**: lives in the *plan* (`ServerWriteOp::Upsert`) and the *applier*
  (`apply_server_write` does replace-by-name for Upsert), not in the interpreter. Identical
  under either host. ✔
- **Audit**: the runtime records one `AuditEntry` per `ConfigChange` returned by the applier
  (`runtime.rs:196`), and the `commit`-level `on_applied` hook is the reserved funnel. The
  interpreter's ledger would attach at the same seam. ✔
- **Irreversibility**: carried per-`EffectNode` (`irreversible` field), derived in the pure
  plan layer (`node.rs:136`), independent of host. `ServerConfigWrite` is `false` by
  construction. ✔

(a) **Is the synchronous seam the RIGHT tool?** Yes. `/server` writes are pure in-memory
`BTreeMap` mutations under a short `RwLock` critical section — no network I/O, no batching,
no parallelism, no retry. The interpreter's machinery is genuinely unneeded; using it would
be *more* coupling for *zero* capability. The synchronous applier is the minimal correct
host.

(b) **Is the narrowly-featured `tokio` (rt+signal+macros, for `ctrl_c`) coherent while
staying off `cfs-runtime`?** Yes, and I verified it does not trip confinement. The guard
that matters is `runtime_is_confined_to_plan_and_types` (`crates/cmd/tests/dep_direction.rs`):
it confines the **`cfs-runtime` crate edge**, not the `tokio` *vendor* crate. `cfs-server`
depends on `cfs-core` + `cfs-parser` only among workspace crates, never on `cfs-runtime`, so
it is not a runtime-consumer and is correctly **absent** from the `runtime_consumers_allowed`
allowlist (lines 399–412). Owning a vendored `tokio` for a `ctrl_c` wait at the `serve`
boundary is structurally distinct from consuming the workspace's async interpreter. The
boundary doc-comment (`lib.rs:23–27`, `Cargo.toml:24–28`) states this precisely.

  - **Observation (smell, minor):** there is no *mechanical* guard that `cfs-server` stays
    off `cfs-runtime` — unlike `cfs-exec` (`exec_is_confined_above_the_spine_and_off_the_runtime`)
    and `cfs-http-core` (`forbidden_in_leaf`), `cfs-server` has no named confinement test. The
    generic leaf-confinement (b) in `runtime_is_confined_to_plan_and_types` *would* fire only
    if `cfs-server` both took `→ cfs-runtime` **and** something depended back onto it
    (`cfs-cmd` does). So the protection is implicit, not explicit. The ticket's own framing
    ("must NOT depend on `cfs-runtime`") deserves a fail-closed pin.
  - **Proposal:** add a `server_is_confined_off_the_runtime` test to `dep_direction.rs`
    asserting `cfs-server`'s workspace deps are exactly `{cfs-core, cfs-parser}` and that it
    does NOT depend on `cfs-runtime` — symmetric with the existing `cfs-exec` guard. This is a
    small, reviewable addition; I rule it a **carry-over** (it is t30's structural intent made
    mechanical) rather than a blocker, since the generic guard already covers the live cycle.

(c) **Does it scale to t31–t35 (HTTP/cron/webhook bindings need async)?** This is the real
future collision and I frame it as a **carry-over**, not a t30 problem. Today `Runtime::run`
owns a current-thread tokio built **inside `cfs-server`** at the `serve` boundary
(`lib.rs:74`). When t31 adds an axum HTTP binding (and t32 a scheduler), those causes need a
multi-threaded async runtime and will live behind the `Binding` trait — whose `reconcile` is
**synchronous** (`binding.rs:56`) and explicitly forbids holding a lock across `.await`. The
clean resolution that preserves confinement: the **async lives inside the binding crate**
(e.g. `cfs-binding-http`, a leaf), which spawns its own tasks onto a runtime it owns;
`reconcile(&ServerState)` stays synchronous and just hands the binding an owned snapshot to
converge against. The runtime/binding seam must NOT grow an `async fn reconcile` that forces
`cfs-server` itself to host a multi-threaded executor — that would re-introduce exactly the
runtime coupling t30 avoided.
  - **Carry-over CO-t30-1 (to t31):** decide where the binding executor lives. Recommended:
    each cause binding is a leaf crate owning its own tokio; `cfs-server`'s `Binding` seam
    stays synchronous-reconcile + owned-snapshot. If t31 instead needs `cfs-server` to own a
    shared multi-thread runtime, that is a confinement re-litigation and must add the explicit
    guard from the observation above.

**Ruling on Headline 1:** the pure-commit-seam routing is the correct structural choice; it
does not create divergent commit semantics (one `commit` algorithm, pluggable appliers); the
narrow tokio is coherent and confinement-safe. Approve.

---

## HEADLINE 2 — CREATE-DDL sugar-equivalence proof completeness: **GAP CONFIRMED (body-less only). Carry-over, not a t30 blocker.**

### What is genuinely established
The equivalence is **proven for the structural mapping** (name/EVERY → node/op/args) and for
the **body-less** case: `create_job_and_insert_into_server_jobs_yield_identical_plans`
(`tests.rs:103`) lowers `CREATE JOB nightly EVERY '1h'` and `UPSERT INTO /server/jobs
VALUES (name, every, plan) ('nightly', '1h', '')` and asserts byte-identical plan nodes AND
identical PREVIEW JSON. That genuinely proves the desugar's *coordinate* mapping (DDL kind →
`ServerNode::Jobs`, CREATE → `Upsert`, EVERY clause → `every` column, canonical schema-order
`RowBatch`). The mechanism is sound: both paths normalise into the same `ConfigRow` and
`config_row_batch` emits columns in `server_node_schema` order, filling absent fields `Null`
(`lower.rs:55`, `:114`). For the body-less case this is airtight.

### The unproven gap — exactly where a real job has a body
The `plan` field of a real job carries the `DO <plan>` body. The desugar renders that body to
text via `render_statement` (`lower.rs:307`), which is **`format!("{stmt:?}")`** — an AST
`Debug` projection — because the parser ships **no statement `Display`/round-trip renderer**.
The equivalence test sidesteps this by using a **no-DO job** (so `plan` desugars to `""` and
the equivalent INSERT supplies a literal `''`). So the claim "CREATE … DO `<plan>` and its
INSERT twin produce identical nodes" is **not proven for the case that actually has a body**:

- For `CREATE JOB nightly EVERY '1h' DO REMOVE /tmp WHERE age > 7` (the *boot fixture*,
  `server_boot.cfs:8`), the `plan` field becomes the **`Debug` string of the parsed
  `Statement`** — an implementation-detail projection, not canonical source.
- The "equivalent" `INSERT INTO /server/jobs … ('nightly','1h', <plan>)` would have to supply
  that **exact `Debug` string** as a string literal to match. A human/AI writing the natural
  equivalent (`… 'REMOVE /tmp WHERE age > 7'`) would produce a **different** `plan` value and
  therefore a **non-identical** plan node. The body-less test cannot catch this because its
  body is empty on both sides.

This is a **real translation-fidelity gap**: the sugar-equivalence thesis ("DDL is *provably*
sugar over the write") holds for the node/op/scalar-field coordinates but is **unestablished
for the plan-body field** precisely where the body is non-empty. The boot path is internally
consistent (it stores whatever the desugar produces and a future binding re-parses the same
text), so t30's *runtime* is correct — but the *equivalence acceptance criterion* is only
half-satisfied, and the `Debug`-projection-as-key is a latent fragility (a `derive(Debug)`
change in the parser silently changes stored config bodies and breaks any golden keyed on it).

### Resolution
1. **Right fix (E1/grammar):** the parser should ship a canonical statement renderer
   (`Display` / a `render_canonical()` round-trip), so `DO <plan>` and `AS <query>` bodies
   store **canonical source text** that an INSERT can supply literally and match exactly. A
   `Debug` projection is never a stable key. This **belongs to E1/grammar**, not t30 — t30
   correctly *consumes* the parser and cannot reasonably add a full pretty-printer.
2. **Interim (t30 already does this acceptably):** storing the body as opaque `StatementSource`
   text re-parsed by the binding is the right t30 shape (`state.rs:21`); the only defect is
   that the *current* text is a `Debug` projection rather than canonical source.
3. **Carry-over framing:**
   - **CO-t30-2 (to E1/grammar):** add a canonical statement renderer; replace
     `render_statement`'s `format!("{:?}")` with it. Until then, `render_statement` MUST carry
     a loud `// TODO(E1): Debug projection is not a stable key` (it currently documents the
     rationale but understates that a body-bearing CREATE/INSERT equivalence is unproven).
   - **CO-t30-3 (to the relevant binding ticket, t31/t32):** once a renderer exists, extend the
     equivalence golden to a **body-bearing** job (`CREATE JOB … DO REMOVE …` vs the literal
     INSERT) and assert identical nodes — closing the acceptance criterion for the real case.

**Ruling on Headline 2:** sugar-equivalence is genuinely established **only for the body-less
case**; the body-bearing case is unproven and rests on an unstable `Debug` key. This is **not
a t30 blocker** (the runtime is self-consistent and the coordinate mapping is proven), but the
acceptance criterion is only partially met and must be honestly recorded as a carry-over to
E1/grammar (renderer) + the binding ticket (body-bearing golden). The test comment at
`tests.rs:110` is candid about using a body-less job, which is good faith; it should be
elevated from a code comment to a tracked carry-over.

---

## Other surfaces

### 1. Boot-as-COMMIT-replay purity — **Confirmed, no shortcut.**
`Runtime::boot` (`runtime.rs:146`) reads → splits → `apply_source` per statement, and
`apply_source` (`:166`) parses → `lower_statement` → `commit(plan, ServerConfigApplier)`. No
privileged loader. `ServerConfigApplier::apply` is the **only** writer of `ServerState`
(`:63`), and `apply_server_write` (`driver.rs:214`) is the only mutation function, reached
only through `commit`. The `NoopApplier` on the `Driver::applier()` contract slot
(`driver.rs:175`) is a clean no-op so the introspective driver does not pretend to own the
impure seam — the runtime owns it. Verified there is no second mutation entry point.

  - **Observation (minor):** `boot` and `apply_source` correctly use the same path, but the
    statement splitter (`statements`, `runtime.rs:252`) is a hand-rolled `;`/comment chunker
    living in `cfs-server`, duplicating a concern the parser arguably owns. It is deliberately
    minimal and tested (`statement_splitter_handles_comments_and_semicolons`), so acceptable
    for t30. **Proposal / CO-t30-4 (to E1):** when the parser gains multi-statement parsing,
    `cfs-server` should delegate splitting to it rather than re-deriving grammar (a `;` inside
    a string literal — not just a comment — would currently mis-split; the parser would not).

### 2. `cfs-plan` stays pure — **Confirmed.**
`server.rs` adds only two closed, `Copy`, vendor-free enums (`ServerNode`/`ServerWriteOp`)
with `segment`/`path`/`label` const projections; no I/O, no vendor type, no server-shaped DTO.
The DTO rides in `EffectNode.args: RowBatch` (`node.rs:45–50`) exactly as designed — the plan
node carries only coordinates. `ServerConfigWrite` is added to the `#[non_exhaustive]`
`EffectKind` with `is_inherently_irreversible() == false` and a stable per-(node,op) `label`
for coalescing (`node.rs:76`). The plan-purity dep-closure stays green by construction (no new
crate edge from `cfs-plan`). ✔

  - **Observation (very minor):** the 24-arm `label()` match (`node.rs:76–101`) is exhaustive
    but verbose; a `format!("SERVER_{}_{}", node.segment().to_uppercase(), op.label())` would
    be one line and equally stable. Kept as a literal match presumably for `&'static str`
    return (no allocation) — acceptable; noting only as a maintainability trade-off if a 7th
    node is ever added. (No action required.)

### 3. ServerState secret hygiene — **Confirmed.**
Every DTO references handles, not tokens (`PolicyDef.allow` is scope handles; `state.rs:106`).
`ServerState` exposes only `summary()` (per-collection counts) and `row_count()` projections
(`state.rs:163`); the runtime logs `summary()` (`runtime.rs:157`, `:222`), never the registry.
`ConfigChange` (`driver.rs:190`) and `AuditEntry::ConfigWrite` carry node/op/**name**/
before-after booleans only — no row contents — and `AuditEntry::summary` renders names+ops
(`audit.rs:57`). The derived `Debug` on DTOs is acknowledged (`state.rs:11`) as safe-today +
guarded-by-never-logging-verbatim.
  - **Observation:** the hygiene rests on the discipline "never `{:?}` the whole state"; a
    future credential-bearing field would be safe only if that discipline holds. The doc-comment
    states this. **Proposal:** when t34 adds credential-bearing policy fields, add a test that
    `summary()`/`AuditEntry::summary()` output contains no value from a seeded secret — make the
    "never verbatim" invariant executable. Carry-over to t34. (Not a t30 concern; policies are
    handles-only today.)

### 4. Binding reconcile seam — **Confirmed.**
Declarative converge-to-registry (`binding.rs:42`), no imperative add/remove. `reconcile_all`
(`runtime.rs:206`) takes a **cloned** snapshot (`snapshot()` clones under a brief read guard,
`:134`) and never holds a guard across the binding call; the trait doc forbids holding a lock
across `.await` and forbids mutating state. Called once per committed change in `apply_source`
(`:196`) plus once at end-of-boot (`:155`). ✔
  - **Observation (semantic, minor):** the per-change loop calls `reconcile_all()` **inside**
    `for change in &changes` (`:196–199`) — so for a multi-effect plan with N changes,
    *every* binding reconciles N times against the *final* snapshot (snapshot is re-read each
    iteration after all writes already applied). For t30 every `/server` plan is single-node
    (`server_write_plan` builds one effect, `lower.rs:71`), so N=1 always and behavior is
    correct. But the acceptance phrasing is "once per committed **mutation**"; if a future
    multi-row config write produces N changes in one commit, this would fire N reconciles all
    seeing the same post-commit state (redundant, not wrong — reconcile is idempotent
    convergence). **Proposal / CO-t30-5:** when multi-node `/server` plans become possible,
    reconcile **once per commit** (after the change loop) rather than once per change, or keep
    per-change but document it as "per change" not "per mutation". Today single-node makes the
    two identical; flag for the multi-write future.

### 5. Idempotency under UPSERT — **Confirmed.**
`re_applying_the_same_config_is_idempotent` + `boot_snapshot_serializes_deterministically`
(`tests.rs:71`, `:86`) prove byte-stable re-boot. `Upsert` is replace-by-name into a
`BTreeMap` (`driver.rs:232`, `state.rs:121`), deterministic serialization via `BTreeMap`
ordering + canonical row shape. CREATE desugars to `Upsert` (`lower.rs:167`) — the
replay-safe verb. ✔ (Caveat: `JobDef.last_run` is intentionally `None` on a fresh INSERT;
t32 owns its high-water persistence — documented, `state.rs:80`.)

### 6. Audit ledger — **Confirmed.**
One `AuditEntry` per `/server` mutation (`runtime.rs:197`), who/op/node/before-after
(`audit.rs:18`), drained on shutdown (`run()` → `audit.drain()`, `:228`). `PlanFired` is the
reserved E7 variant (`audit.rs:35`) so the ledger is one funnel. Thread-safe `Mutex`, short
critical sections, poison-degrades-to-drop so the audit never breaks the operation
(`audit.rs:92`). `boot_records_one_audit_entry_per_mutation_and_drain_flushes` asserts 8
entries for the 8-statement fixture. ✔

### 7. `CfsError::Server` additive arm — **Confirmed coherent.**
Added to the `#[non_exhaustive]` `CfsError` in `cfs-driver` (`driver/src/error.rs`) with
flattened owned fields `{server_code, message}` because `cfs-driver` cannot name
`cfs_server::ServerError` (server is far above it in the spine — correct direction). The
`code()` family label is `"server_config"`; the granular code lives in `server_code`. The
`serve` boundary maps `ServerError` → `CfsError::Server` preserving the line-located,
secret-free message (`server/src/lib.rs:86`). `ServerError` itself is a `#[non_exhaustive]`
`thiserror` enum with stable `code()` and line-located variants (`server/src/error.rs`). ✔
  - **Observation (very minor):** the doc on the arm says "E7, `cfs serve`" — accurate; the
    flattening rationale is well-documented. No action.

### 8. Confinement guards — **All green by inspection; one explicit pin recommended.**
- `cfs-cmd → {cfs-core, cfs-server}` only; `cmd_does_not_depend_on_domain_crates_directly`
  forbids the lower domain crates and *requires* the `cfs-server` edge (the serve arm). The
  `Serve` arm + `cfs-server` dep pre-existed (scaffolded at t01, `9557bda`); t30 fleshed out
  the previously-stubbed `cfs_server::serve`. ✔
- `cfs-server` does **not** invert the spine: workspace deps are `cfs-core` + `cfs-parser`
  only; it is consumed by `cfs-cmd` (and transitively the terminal `cfs` binary), and nothing
  depends back onto it inappropriately. ✔
- `cfs-exec`/`cfs-cmd` do not depend on `cfs-server` inappropriately: `cfs-exec` is confined to
  the above-spine read set (no `cfs-server`); `cfs-cmd` consumes `cfs-server` exactly as the
  guard requires. ✔
- As noted in Headline 1(b): the `cfs-server`-off-`cfs-runtime` invariant is currently only
  **implicitly** protected. **Recommended pin (CO-t30-1 companion):** add an explicit
  `server_is_confined_off_the_runtime` guard.

---

## Summary of carry-overs (none block t30 acceptance)
- **CO-t30-1** (t31): decide binding-executor location; keep `Binding::reconcile` synchronous +
  owned-snapshot so `cfs-server` stays off `cfs-runtime`. Add explicit
  `server_is_confined_off_the_runtime` dep-direction guard.
- **CO-t30-2** (E1/grammar): canonical statement renderer; replace `render_statement`'s
  `Debug` projection (an unstable key for stored plan bodies).
- **CO-t30-3** (t31/t32): body-bearing CREATE-vs-INSERT equivalence golden once a renderer
  exists — closes the half-met acceptance criterion.
- **CO-t30-4** (E1): delegate multi-statement splitting to the parser (string-literal `;`
  edge); retire the hand-rolled splitter.
- **CO-t30-5** (multi-write future): reconcile once-per-commit, not once-per-change, when
  `/server` plans gain multiple nodes.
- **CO (t34)**: executable secret-redaction test once policy gains credential-bearing fields.

## Verdict
**Approve with observations.** Both headline decisions are structurally sound for t30; the
only genuine fidelity gap (body-bearing sugar-equivalence keyed on an AST `Debug` projection)
is correctly out of t30's reach (it needs an E1 parser renderer) and is recorded as a
carry-over rather than a blocker. The runtime is a faithful pure-consumer-of-plans: one
`commit` algorithm, one mutation path, one audit funnel, confinement intact.
