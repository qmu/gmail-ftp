# Model v1

Author: Architect
Status: draft
Reviewed-by: (pending)

## Night-Mode Assumption (recorded)

This trip was launched via `/trip night` with an **empty** instruction. The lead's
recorded interpretation fixes scope to RFD-0001 epic **E0**: ticket **t01** (Rust
workspace + single-binary scaffold) and, gated on it, ticket **t02** (parser-library
decision spike). I adopt that interpretation verbatim and add the following structural
assumptions, all of which downstream agents may treat as binding unless a review
overturns them:

- **A1 — Scaffold-only fidelity.** E0 delivers *typed seams*, not behavior. Every
  module compiles, every trait instantiates, every registry round-trips a
  register/resolve, and every CLI/server dispatch returns a structured
  `CfsError::NotImplemented`. No grammar (beyond t02's throwaway spike), no drivers, no
  codecs, no real I/O. Success = "later tickets add code *inside* these seams without
  restructuring."
- **A2 — Crate granularity matches the ticket's eight-crate split.** I do not propose a
  coarser or finer split; the ticket's `qfs / qfs-cmd / qfs-core / qfs-lang / qfs-plan /
  qfs-driver / qfs-codec / qfs-server` (+ `qfs-parser` from t02) is the contract. My job
  is to verify it *faithfully encodes* RFD §3/§5/§9, not to redesign it.
- **A3 — wasm32 is a constraint, not a deliverable.** `wasm32-unknown-unknown` CI wiring
  is explicitly out of t01 scope, but the core crates must stay wasm-friendly (no
  threads / `std::fs` / sockets in `qfs-core/qfs-lang/qfs-plan/qfs-driver/qfs-codec`).
  This is a *boundary-integrity* constraint I assert here so E0 does not foreclose §1/§9.
- **A4 — The Go program is a reference for boundaries, not code to port.** RFD §0 says
  the Go `gmail-ftp` is *subsumed as one driver*, not merged. I mine it only for the
  seams that must survive the rewrite (below); no Go is carried over.

---

## 1. System Coherence Mapping (RFD intent → E0 crate seams)

The RFD describes **one engine with three faces** (§2), a **closed core + three open
registries** (§3), a **driver contract** (§5), and a **server-that-is-a-driver** (§8),
all in **one Rust binary** (§9). t01's eight crates are the structural projection of
that design. The mapping is one-to-one and load-bearing:

| RFD concept | Section | E0 crate / seam | Why this crate owns it |
|---|---|---|---|
| One binary, CLI **and** server | §1, §9 | `qfs` (bin) → `qfs-cmd` | Single `[[bin]]`; `main.rs` is thin and delegates to `qfs-cmd::run(argv)`. CLI and `serve` are two dispatch arms of the *same* binary, not two programs. |
| Pipe-SQL language, **frozen** keyword set | §2.2, §3 | `qfs-lang` | The reserved-keyword `const` set lives in exactly one place. AST sum types (E1) land here. Frozen-ness is structurally enforceable because there is one home. |
| Effect-plan (effects-as-data, typed DAG) | §2.3, §6 | `qfs-plan` | `enum Effect`, `struct Plan`, `irreversible` flag. The purity invariant (`fn … -> Plan`) is anchored by *the existence of this crate as the only effect type*. |
| Three open registries (paths / procs+fns / codecs) | §3 | `qfs-core` (`MountRegistry`, `ProcRegistry`, `CodecRegistry`) | The registries are the *governance mechanism* — "new backend = zero keywords". They must sit in the shared engine glue so both CLI and server resolve through the same surface. |
| Driver contract (namespace, capabilities, procedures, pushdown, prelude) | §5 | `qfs-driver` (`trait Driver`, `Archetype`, `Capabilities`, `ProcedureDecl`, `AliasFn`) | The consumer-side narrow trait + owned-DTO rule. This is the seam every E4 driver fills. |
| Codecs (pure `bytes ↔ rows`) | §4, §3 | `qfs-codec` (`trait Codec`) | A separate registry/trait so codecs compose over *any* blob source independent of driver identity. |
| Server-is-a-driver (`/server/...`, bindings) | §8 | `qfs-server` (`serve(config)` stub, `/server` mount) | The server is structurally a `Driver` over `/server/...`; bindings are `INSERT INTO /server/...`. E0 ships only the module + mount stub. |
| Engine / Session (threads registries + audit + caps) | §6, §10 | `qfs-core` (`Engine` / `Session`) | The execution context both faces share; the only place the audit sink and capability gate are threaded. |
| Structured, machine-readable errors (AI-facing) | §5 | `CfsError` (in `qfs-core`, re-exported) | One error enum workspace-wide. The AI procedure depends on errors being parseable, not prose. |
| Parser front door | §2.2, §9 | `qfs-parser` (t02) | Turns DSL text → AST sum types. Wired to the library the t02 ADR locks; library types never cross its public boundary. |

**Coherence claim.** There is no RFD §2/§3/§5/§8/§9 concept that lacks a crate home, and
no E0 crate that does not trace to an RFD concept. The split is *complete* (covers the
design) and *non-redundant* (no concept split across two crates without a stated reason).

## 2. Domain Model

The E0 domain is the set of *typed seams* that later epics populate. The entities and
their relationships:

```
                       ┌──────────────────────────── qfs (bin) ────────────────┐
                       │  main.rs → qfs-cmd::run(args)                          │
                       └───────────────────────────────────────────────────────┘
                                          │ dispatch (one of three arms)
              ┌───────────────────────────┼────────────────────────────┐
        qfs run '<stmt>'            interactive shell              qfs serve <cfg>
         (one-shot)                  (cwd-tagged loop)              (qfs-server)
              └───────────────────────────┼────────────────────────────┘
                                          ▼
                            ┌──────────── qfs-core ────────────┐
                            │  Engine { registries, audit_sink,│
                            │           caps } ──▶ Session      │
                            │  ┌──────────────────────────────┐│
                            │  │ MountRegistry  (paths §3)     ││── resolve ▶ Box<dyn Driver>
                            │  │ ProcRegistry   (fns + CALL §3)││
                            │  │ CodecRegistry  (codecs §3)    ││── resolve ▶ Box<dyn Codec>
                            │  └──────────────────────────────┘│
                            │  CfsError (structured, §5)        │
                            └───────────┬───────────┬──────────┘
                                        │           │
                  uses AST from         │           │  builds / inspects
                        ▼               │           ▼
                 ┌── qfs-lang ──┐       │      ┌── qfs-plan ──┐
                 │ KEYWORDS const│      │      │ enum Effect   │
                 │ (frozen §3)   │      │      │ struct Plan   │
                 │ AST (E1)      │      │      │ irreversible  │
                 └──────┬────────┘      │      └──────────────┘
                        │ parsed by     │
                        ▼               │
                 ┌── qfs-parser ──┐     │
                 │ parse_statement│     │
                 │ ParseError(own)│     │
                 └────────────────┘     │
                                        ▼
                         ┌── qfs-driver ──┐     ┌── qfs-codec ──┐
                         │ trait Driver    │     │ trait Codec    │
                         │ Archetype       │     │ fmt/decode/    │
                         │ Capabilities    │     │ encode         │
                         │ ProcedureDecl   │     └────────────────┘
                         │ AliasFn         │
                         │ (owned DTOs)    │
                         └─────────────────┘
                                ▲
                                │ implements (E4; e.g. mail/drive/s3)
                         (qfs-server is itself a Driver over /server)
```

### Entities

- **Engine** (`qfs-core`) — process-wide context. Owns the three registries, an
  **audit sink** hook (reserved for E2), and the **capability** gate handle (shape only,
  enforcement in E5). Constructed once per CLI invocation or server boot.
- **Session** (`qfs-core`) — a single interaction's state: the cwd `{driver, path}` for
  the interactive shell, the JSON-vs-human output mode, the request/event context on the
  server. The Engine is shared; the Session is per-statement/per-request.
- **MountRegistry** — `register(mount: &str, driver) / resolve(path) -> &dyn Driver`.
  Empty in E0; round-trips one register/resolve in a unit test.
- **ProcRegistry** — both pure alias **functions** (`fn SEND(d) = … |> CALL mail.send`)
  and `CALL driver.action` **procedures**. One registry because both are
  receiver-typed, registry-resolved, keyword-free (§3).
- **CodecRegistry** — `DECODE fmt` / `ENCODE fmt` resolution to a `dyn Codec`.
- **Driver** (trait, `qfs-driver`) — `mount / describe / capabilities / procedures /
  prelude`. The capability surface is what lets unsupported verbs be rejected *at parse
  time* (§5) — the AI-facing structured error.
- **Codec** (trait, `qfs-codec`) — `fmt / decode / encode`, pure `bytes ↔ rows`.
- **Plan / Effect** (`qfs-plan`) — the typed DAG with `irreversible`. In E0 these are
  minimal type stubs; the *purity invariant* they anchor is encoded now in trait
  signatures so E4 inherits it for free.
- **CfsError** — one structured enum; `NotImplemented` is the only variant E0 exercises.

### Carry-over boundaries from the Go reference (A4)

The Go `gmail-ftp` already discovered, in miniature, four of the seams the RFD
generalizes. Recording them protects translation fidelity — the rewrite must *keep* these
boundaries, not rediscover them:

| Go construct (reference) | RFD generalization | E0 seam it justifies |
|---|---|---|
| `shell.gmailClient` — a *narrow* interface the shell depends on (so commands are fake-able with no creds) | The consumer-side small `Driver` trait (§5, §9 "consumer-side small traits") | `qfs-driver::Driver` must be narrow + fake-able; t01's "dummy in-test impl that performs no I/O" is the direct analog of the Go fake. |
| `internal/gmail` "quarantining the Gmail SDK behind one package" | "SDK/vendor types never leak past a driver boundary (owned DTOs)" (§9) | The `sealed`/marker owned-DTO convention on `qfs-driver`. |
| `internal/audit` — append-only, owned data, *never* credentials, best-effort, never breaks the op | The audit ledger / applied-effect log (§6, §10) | The reserved **audit sink** hook on `Engine`. |
| `auth.Scopes` — least-privilege scope set as single source of truth | Per-handler `POLICY` least-privilege, capability gating (§5, §10) | The `Capabilities` / `POLICY` *shape* defined in E0 (enforcement E5). |
| `main.go` dispatch: `auth` / `log` / `__complete` / one-shot / interactive, with a JSON `{"error":…}` envelope on every failure path | CLI faces §7 + structured AI-facing errors §5 | `qfs-cmd` dispatch arms returning a structured `CfsError`; one consistent error contract. |
| 2-level VFS (root → label → message), path built without re-query (`Ref`) | "a path is just a query that resolves to a set" (§2.1 VFS) | The `Path`/cwd `{driver, path}` shape threaded through `Session`. |

## 3. Translation-Fidelity Analysis

**The central fidelity question (per the task):** *Does the proposed crate boundary
faithfully represent the RFD's closed-core / open-registry intent so downstream tickets
add code inside the seams without restructuring?*

**Verdict: yes, with three fidelity guards that must be made structural, not aspirational.**

The closed-core/open-registry split is the RFD's governance thesis (§3): the keyword set
is frozen; all extension flows through three registries. A faithful E0 must make the
*frozen* part structurally hard to violate and the *open* part structurally easy to
extend. The crate split supports both:

1. **Closed core is faithfully represented** because the frozen keyword set has exactly
   **one home** (`qfs-lang`), test-locked against an RFD §3 golden list. A later ticket
   that wants new behavior cannot add a keyword in its own crate — there is nowhere to put
   it except `qfs-lang`, and the golden test fails if it tries. *Fidelity guard G1: the
   keyword golden test must compare against the RFD §3 list verbatim, so the test is the
   contract, not a copy that can drift.*

2. **Open registries are faithfully represented** because extension = `register(...)`
   into one of three registries in `qfs-core`, with no type change to the core. A new
   driver (E4) implements `qfs-driver::Driver` and calls `MountRegistry::register`; it
   touches *zero* core types. This is exactly "new backend = zero keywords." *Fidelity
   guard G2: the registries must be generic over the trait object (`dyn Driver` / `dyn
   Codec` / proc decl), not over concrete types, so E4 adds drivers without editing
   `qfs-core`.*

3. **Purity invariant is faithfully represented** *only if* the trait method signatures
   make I/O-at-describe-time impossible. The RFD's §3 purity invariant ("every function
   has type `… -> Plan`; the only impure op is the interpreter") is the safety property
   behind `SEND`-as-a-function and dry-runnability. E0 must encode it in
   *signatures*: `Driver::describe → Result<NodeSchema, CfsError>` and
   `Driver::capabilities → Capabilities` return *data*; nothing on the `Driver`/`Codec`
   trait returns a future, takes an async executor, or returns `()`-with-side-effects.
   The lone impure seam (`COMMIT : Plan -> World -> World`) is *absent* from E0 by
   design (reserved for E2). *Fidelity guard G3: t01's compile-only/`trybuild` test must
   instantiate a dummy `Driver`/`Codec` that performs no I/O — this is the structural
   proof the invariant holds at the type level, not a comment.*

**Where fidelity is at risk (translation gaps to watch):**

- **G4 — `qfs-server` as a Driver, not a special case.** §8 says the server *is a
  driver*. If E0 stubs `qfs-server::serve` as a bespoke entrypoint that does *not* route
  through a `/server` mount in `MountRegistry`, the rewrite quietly reintroduces a
  "server is special" boundary the RFD explicitly rejects. Fidelity requires the
  `/server/...` mount stub to be (eventually) a `Driver` registered like any other, even
  though E0 only ships the module + mount stub. Recommend t01 add a doc-comment in
  `qfs-server` asserting this and a `// TODO(E7): register /server as a Driver` anchor so
  the seam is visibly reserved.

- **G5 — `qfs-cmd` must hold *no* domain logic.** The Go `main.go` already leaks a little
  domain knowledge (path defaults, completion). The RFD's coherence depends on CLI and
  server being *thin dispatch* into shared `qfs-core`. If `qfs-cmd` accretes statement
  handling, the "two faces of one engine" claim degrades into "two engines." Fidelity
  guard: `qfs-cmd` depends on `qfs-core` only; it never depends on `qfs-lang`, `qfs-plan`,
  `qfs-driver`, or `qfs-codec` directly. (`clippy`/a dependency-direction check can
  enforce this.)

- **G6 — `qfs-parser` boundary reversibility.** t02 locks a library (default winnow) but
  the RFD's footprint/reversibility concern (§9) means the chosen library's types must
  not appear in `qfs-parser`'s public API — wrap into an owned `ParseError`. This makes
  the §9 "spike confirms before lock-in" decision *reversible*: E1 can swap libraries
  without breaking the AST or `parse_statement` signature. Fidelity guard: a `pub use`
  audit / doc-test asserting no library-internal type is re-exported.

**Traceability.** A stakeholder can trace any RFD requirement to a crate (§1 table) and
any crate back to an RFD section. The frozen keyword list is traceable to §3 by golden
test; the driver contract to §5 by trait shape; the server-is-a-driver to §8 by the
`/server` mount. This is the bridge the model exists to provide.

## 4. Boundary Integrity Assessment

The structural boundaries that must hold for E0 to be a faithful foundation:

- **B1 — Single binary, dual face (§9).** One `[[bin]] qfs`. CLI and server are dispatch
  arms in `qfs-cmd`, both reaching the *same* `Engine` in `qfs-core`. INTEGRITY: high —
  the split places the shared engine below both faces. RISK: G5 (cmd must stay
  logic-free).

- **B2 — Closed core / open registry (§3).** Frozen keywords in `qfs-lang` (one home);
  open extension via three `register/resolve` registries in `qfs-core`. INTEGRITY: high,
  conditional on G1 (golden test) + G2 (generic over trait objects).

- **B3 — Driver boundary / no vendor leak (§5, §9).** `qfs-driver` exposes a narrow trait
  and owned DTOs; no SDK type crosses it. INTEGRITY: high — directly mirrors the Go
  `internal/gmail` SDK quarantine that already works in production. The Go
  `shell.gmailClient` narrow interface is the proof-of-concept.

- **B4 — Purity / effects-as-data (§3, §6).** Effects exist only as `qfs-plan` data;
  the only impure operation (`COMMIT`) is absent from E0. INTEGRITY: high, conditional on
  G3 (compile-only no-I/O proof).

- **B5 — Server-is-a-driver (§8).** `/server/...` is a mount, not a privileged subsystem.
  INTEGRITY: medium in E0 (only a stub ships); protect with G4 (doc + TODO anchor) so the
  seam is reserved, not closed off.

- **B6 — Parser boundary (§9, t02).** `qfs-parser` owns `parse_statement` + owned
  `ParseError`; library types stay inside. INTEGRITY: high, conditional on G6 (pub-use
  audit). Position: `qfs-parser` depends on `qfs-lang` (for the AST + keyword consts) and
  is consumed by `qfs-core` (which calls `parse_statement` to turn text → AST). It sits
  *between* `qfs-lang` and `qfs-core`'s dispatch, never above `qfs-cmd`.

- **B7 — wasm-friendliness (§1, §9, A3).** Core crates avoid threads/`std::fs`/sockets;
  all I/O is behind (future) driver impls. INTEGRITY: structural constraint, not a build
  target in E0 — documented in `ARCHITECTURE.md`, enforced by keeping I/O crates out of
  the core dependency set. (Note: t02 *does* require a `wasm32-unknown-unknown` build of
  `qfs-parser` to pass, so the parser crate must honor B7 strictly.)

- **B8 — No credentials in the build (§10).** No secret/credential field anywhere in E0;
  no creds in tests or CI. INTEGRITY: high — the Go `auth.Scopes` least-privilege ethos
  carries over as the `Capabilities`/`POLICY` *shape* with zero secret material.

**Dependency-direction invariant (the spine of boundary integrity):**

```
qfs (bin) → qfs-cmd → qfs-core → { qfs-lang, qfs-plan, qfs-driver, qfs-codec, qfs-parser }
                          ▲
                    qfs-server ── (is a Driver; depends on qfs-core + qfs-driver)
```

Arrows point toward more-foundational crates; there are **no back-edges** and **no
cycles**. `qfs-cmd` must not reach past `qfs-core`. The leaf crates (`qfs-lang`,
`qfs-plan`, `qfs-driver`, `qfs-codec`) do not depend on each other except where the RFD
requires it (e.g. `qfs-driver` returning a `qfs-plan::Plan` node, `qfs-parser` consuming
`qfs-lang`'s AST). This acyclic spine is what lets later tickets add code inside a single
crate without restructuring the workspace.

## 5. Component Taxonomy

Classifying each E0 component by structural role, to make the "add inside the seam"
property explicit:

**A. Composition root (one binary, two faces)**
- `qfs` (bin) — entrypoint shell; thin.
- `qfs-cmd` — argv parsing (clap), three dispatch arms (run / shell / serve); logic-free.

**B. Shared engine glue**
- `qfs-core` — `Engine`, `Session`, the three registries, `CfsError`, trait re-exports.
  The hub every face routes through.

**C. Closed-core language surface (frozen)**
- `qfs-lang` — reserved keyword `const` set (golden-locked to RFD §3); AST home (E1).

**D. Effect substrate (effects-as-data)**
- `qfs-plan` — `Effect`, `Plan`, `irreversible`. Anchors the purity invariant.

**E. Open extension seams (the three registries' targets)**
- `qfs-driver` — `Driver` trait + `Archetype` / `Capabilities` / `ProcedureDecl` /
  `AliasFn`; owned-DTO marker convention.
- `qfs-codec` — `Codec` trait (`bytes ↔ rows`).
- (Registries themselves live in `qfs-core` per B/§3.)

**F. Server face (a driver, not a subsystem)**
- `qfs-server` — `serve(config)` stub + `/server` mount stub; reserved seam (G4).

**G. Front door (t02)**
- `qfs-parser` — `parse_statement` + owned `ParseError`; library wrapped (G6); spike
  code quarantined under `qfs-parser/spikes/`; ADR `docs/adr/0001-parser-library.md` is
  the durable artifact.

**H. Cross-cutting (not crates, but boundary obligations)**
- Lints (`clippy -D warnings`, `rustfmt`, no-`unwrap`/`expect`-in-libs); CI
  (fmt/clippy/build/test + aarch64 & x86_64 cross-compile); `ARCHITECTURE.md` documenting
  the crate-boundary rules and pointing back to RFD 0001; `tracing` set up at the `cmd`
  boundary only.

**Sequencing note (t01 → t02 gate).** t02 `depends_on` t01. The structural reason: t02's
`qfs-parser` is a *new workspace member* that consumes `qfs-lang`'s keyword consts and
produces AST sum types — it cannot exist until the workspace, `qfs-lang`, and the lint/CI
spine exist. The gate is real and one-directional; do not parallelize the crate creation.

---

## Review Notes

- The most consequential, genuinely-hard call in E0 is **G3 (purity invariant at the type
  level)** — it must be structurally impossible for a `Driver`/`Codec` impl to do I/O at
  describe/decode time. I flag this for the Constructor to encode in trait signatures now
  (return data/`Plan`, never futures or unit-with-effects), and for the compile-only/
  `trybuild` test to *prove*. If this is only a doc comment, E0 has failed its core job.
- **G4 (server-is-a-driver)** is the subtlest fidelity risk: an E0 that stubs `serve()` as
  a bespoke path silently contradicts §8. A doc + `TODO(E7)` anchor is cheap insurance.
- For the Constructor's design and Planner's direction, the one cross-artifact coherence
  point I will be watching: the **dependency-direction spine** (§4) — if any design step
  introduces a back-edge (e.g. `qfs-cmd` importing `qfs-lang`, or a leaf crate importing
  `qfs-core`), the "two faces of one engine" claim and the "add inside the seam" property
  both break. I would request revision on any such edge.
- Open question deferred to E3 (not E0, recorded for traceability): the "local combine
  engine — embed DuckDB vs own evaluator" decision (§6) is *not* an E0 seam and must not
  leak a crate dependency into the E0 workspace; I confirm t01 correctly defers it.
