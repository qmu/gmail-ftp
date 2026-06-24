# Review: round-1-architect

- **Reviewer**: Architect (Neutral — structural bridge / translation fidelity)
- **Artifacts reviewed**: `directions/direction-v1.md` (Planner), `designs/design-v1.md` (Constructor)
- **Coherence anchor**: `models/model-v1.md` (my own, for cross-artifact consistency)
- **Phase/Step**: planning/one-turn-review
- **Date**: 2026-06-22

---

## Summary of decisions

| Artifact | Decision |
|---|---|
| `directions/direction-v1.md` (Planner) | **Approve with minor suggestions** |
| `designs/design-v1.md` (Constructor) | **Approve with minor suggestions** |

Neither artifact warrants a *Request revision*. Both are faithful to RFD-0001 and to the
structural model; the design in particular realizes all six fidelity guards (G1–G6) more
concretely than the model demanded. My concerns are precision/anchoring refinements, not
restructures. Details and the per-concern proposals follow.

---

## 1. Review of `directions/direction-v1.md` (Planner)

**Decision: Approve with minor suggestions.**

### What is faithful (translation-fidelity view)

The direction translates the RFD's product thesis into business terms without contaminating
the structural layer — it correctly stays out of crate/file specifics (that is my job) while
still naming the two boundaries that *must* survive into the crate seams:

- The **closed-core / open-registry** governance bet (§2 "Governance erosion", §4 "Why
  closed core + open registries matters to users") maps one-to-one onto model §3 guards G1
  (frozen keyword set, one home) and G2 (registries generic over trait objects). The Planner
  frames it as a *business* property ("learn the grammar once" stays literally true); I frame
  it as a *structural* property (the keyword set has exactly one home and a golden lock). They
  are the same boundary seen from two sides — which is exactly the translation fidelity this
  trip needs.
- **Persona B (the AI agent)** and its reliance on *structured, machine-legible errors and
  capability rejections* (§3) traces directly to model §3 (CfsError as one structured enum,
  AI-facing) and the design's `CfsError::code()` / `report(json)` JSON envelope (design §2.3).
  The business "agent can reason instead of pattern-matching prose" requirement is faithfully
  realized as a typed enum with a stable machine code.
- **The spike-as-decision-retirement** framing (§1, §5, assumption 4) correctly positions t02
  as risk reduction rather than feature delivery, which matches the model's treatment of
  `qfs-parser` as a *reversible* front door (G6) and the design's ADR-is-the-durable-artifact
  stance (design §3.3).

### Concern P1 — "cross-compile proven in E0" is a business promise the design partially parks

The direction (§2 "Cross-platform regret", §5 outcome statement) holds the team to a foundation
that "builds **and cross-compiles**" by morning, and frames proving aarch64 + x86_64 *now* as a
honest-distribution-promise. But the design (R1, R5, §7 Parked) legitimately parks **local
x86_64 cross-linking** and **local wasm32** as CI-only under night-mode system-safety. There is
a translation gap: the business outcome statement reads as "cross-compile is proven tonight,"
while the engineering reality is "cross-compile is proven *in CI*, native aarch64 is the local
proof." This is not a contradiction — CI proof is real proof — but a morning reader comparing
the direction's outcome line to the actual local state could perceive a shortfall.

**Structural proposal (preserves fidelity):** The Planner need not change the design; rather,
qualify the §5 outcome sentence so the *traceability* is exact — e.g. "builds locally on the
native target and cross-compiles **in CI** for aarch64 + x86_64 (with wasm32 for `qfs-parser`
validated in CI), the local proof being a clean native build." This keeps the business promise
honest and makes it trace cleanly to design R1/R5 rather than appearing to overshoot them. The
boundary (the binary *will* be cross-platform) is preserved; only the verification locus is
made precise.

### Concern P2 — "structural enforcement" of governance is asserted as an outcome but not given a traceable acceptance hook

§2 "Governance erosion" and §4 rightly insist the closed-core boundary must be *structurally
enforced*, not a convention. That is the correct business demand. But the direction (correctly,
per its no-codebase-detail rule) cannot say *how* a stakeholder verifies the boundary held. The
risk is that "structurally enforced" stays aspirational in the business layer with no
agreed-upon evidence artifact the Planner can later E2E-trace to.

**Structural proposal:** Add one sentence to §5's outcome — framed in business-outcome terms —
naming the *evidence* the Planner will hold the team to: "the frozen keyword set is locked by a
test that compares against the RFD §3 list verbatim, so a later ticket physically cannot add a
keyword without a failing build." This is the business-readable surface of model guard G1 and
design test §4 "Keyword golden"; stating it in the direction closes the traceability loop from
business promise → structural guard → concrete test, without the Planner having to name files.

### Cross-reference note

The direction's assumptions 1–4 are coherent with the model's A1–A4 and the design's A1–A6 —
all three artifacts independently converge on "scaffold-only, scope fixed, spike ends in a
decision." This convergence is itself a fidelity signal: three perspectives reading an empty
night instruction landed on the same scope without coordination.

---

## 2. Review of `designs/design-v1.md` (Constructor)

**Decision: Approve with minor suggestions.**

This is a faithful and unusually concrete realization of the structural model. I will record
the guard-by-guard verdict first (the task's central question), then the dependency-spine check,
then concerns.

### 2.1 Does the 9-crate design faithfully realize the six guards?

| Guard (model §3/§4) | Design realization | Verdict |
|---|---|---|
| **G1** — keyword golden test compares against RFD §3 *verbatim* | §2.6 transcribes the §3 set verbatim as a `const`; §4 "Keyword golden" asserts `KEYWORDS` equals the §3 list (set + count). | **Faithful.** See concern C1 (verbatim-vs-test-copy nuance). |
| **G2** — registries generic over trait objects, not concrete types | §2.4 keys `MountRegistry` on `Arc<dyn Driver>`, `CodecRegistry` on `dyn Codec`, `ProcRegistry` on qualified name; E4 adds a driver without editing `qfs-core`. | **Faithful.** |
| **G3** — purity invariant proven *at the type level* (no-I/O dummy impl) | §2.5 traits return data only (no `&mut self`, no future, no I/O); `COMMIT` deliberately absent; §4 "Purity compile-test" instantiates a no-I/O dummy `Driver`/`Codec`. | **Faithful** — and the design correctly chose the in-test dummy over `trybuild`. See concern C2. |
| **G4** — `qfs-server` is a Driver, not a special subsystem | §2.8 ships `serve()` stub + a `mount` submodule documenting the `/server/...` server-is-a-driver stub (§8). | **Faithful at E0 scope.** See concern C3 (the TODO anchor). |
| **G5** — `qfs-cmd` holds no domain logic | §1.2 + §2.2: `qfs-cmd` depends on `qfs-core` only, dispatch arms return `NotImplemented`; "logic-free w.r.t. the engine." | **Faithful.** See concern C4 (enforcement vs assertion). |
| **G6** — parser library wrapped behind owned `ParseError` | §1.3, §3.4: owned `ParseError` in `src/error.rs`; "no vendor type in the public API" asserted by a `pub use` audit test; reversibility documented in the ADR. | **Faithful.** |

All six guards are realized. Notably, the design exceeds the model's ask on G3 (it commits to a
*used* no-I/O dummy impl, not merely an instantiated one — "instantiates **and is used**", §4),
which is the stronger proof.

### 2.2 Is the dependency spine acyclic, as the model requires?

Yes. Design §1.2 states:

```
qfs → qfs-cmd → qfs-core → { qfs-driver, qfs-codec, qfs-lang, qfs-plan }
qfs-server → qfs-core
qfs-parser → qfs-lang
"No crate depends on qfs-cmd."
```

This is consistent with model §4's spine. **One reconciliation point to record** (concern C5):
the model places `qfs-parser` as consumed by `qfs-core` (model §4 B6: "`qfs-parser` … is
consumed by `qfs-core` which calls `parse_statement`"), whereas the design's §1.2 spine lists
`qfs-parser → qfs-lang` but does **not** draw the `qfs-core → qfs-parser` consuming edge. This is
not a cycle and not a fidelity break (at E0 nothing calls `parse_statement` yet), but the seam
where E1 wires text→AST into the engine is currently unstated in the design's spine. Flagged
below so E1 does not surprise the spine.

### 2.3 Concerns and proposals

**Concern C1 — the golden test risks being a *copy* of §3 rather than *the* §3 list.**
Model guard G1's whole point is "the test is the contract, not a copy that can drift." Design
§2.6 transcribes §3 "verbatim" into a `const`, and §4 asserts `KEYWORDS == <the §3 list>`. But if
the right-hand side of that assertion is a second hand-transcription living in the test file,
then both the `const` and the test can drift from the RFD together, and the golden lock only
catches divergence *between* two copies of §3, not divergence *from* §3.

*Structural proposal:* Make the test's expected set load from a single committed fixture that is
itself the §3 extract — e.g. a `crates/lang/tests/rfd-0001-keywords.txt` checked in as the
canonical extract, with a comment citing `RFD-0001 §3` and the commit that produced it. The test
compares `KEYWORDS` against that fixture; review of *that one file* against the RFD is the human
contract-check. This keeps a single source of drift-truth and makes the "test is the contract"
property real rather than "two transcriptions agree." (If the RFD itself can be parsed for the
keyword block, even better — but a cited fixture is sufficient and lower-risk for E0.)

**Concern C2 — the in-test dummy proves *positive* instantiation but not the *negative* (that an
I/O-doing impl is rejected).** The design's choice to drop `trybuild` (§4, R7) is reasonable for
foundation weight, and I support it. But G3's deepest claim is that I/O at describe-time is
*impossible*, i.e. the trait shape *forbids* it. A no-I/O dummy that compiles proves the seam
*permits* a pure impl; it does not by itself prove the seam *forbids* an impure one. The real
guarantee comes from the signatures (no `&mut self`, no `async`, returns owned data) — which the
design has — not from the test.

*Structural proposal:* Keep the in-test dummy, but make the *signature* the load-bearing proof
and say so explicitly: add a one-line doc-comment on the `Driver`/`Codec` traits stating the
purity contract ("methods return owned data; no `&mut self`, no `async`, no I/O — the only impure
seam is `COMMIT`, reserved for E2"), and have `ARCHITECTURE.md` record that the *type signature*,
not the test, is the enforcement. The test then documents intent; the signature enforces it. This
matches model §3 G3 ("encoded in *signatures*") precisely and avoids over-claiming what a positive
compile-test proves.

**Concern C3 — G4's reservation needs the visible anchor the model asked for.** Design §2.8 ships
the `/server` mount stub and documents server-is-a-driver, which is faithful. The model (§3 G4,
Review Notes) specifically asked for a `// TODO(E7): register /server as a Driver` anchor so the
seam is *visibly reserved* and a future reader cannot mistake the stub for a permanent bespoke
entrypoint.

*Structural proposal:* Add the `// TODO(E7): register /server in MountRegistry as a Driver`
anchor in `crates/server` (and a one-line note in `ARCHITECTURE.md`). This is the cheap insurance
the model flagged; the design already does the substance, this just makes the reservation legible
so E7 does not re-litigate whether the server is special.

**Concern C4 — G5 ("cmd logic-free") is asserted but not *enforced* — risk of silent drift across
E1–E8.** The design states the rule (§1.2, §2.2, §7) and the spine forbids the back-edge, but
nothing *mechanically* prevents a future ticket from adding a `qfs-cmd → qfs-lang` (or
`→ qfs-plan`/`qfs-driver`/`qfs-codec`) dependency, which is exactly the "two engines" erosion the
model (§3 G5) and the Planner (§2 governance erosion) both fear. Cargo will happily compile a
new direct dependency; clippy will not catch it.

*Structural proposal:* Enforce the dependency-direction invariant mechanically. Lightest option
that fits E0: a small `cargo test` in `qfs-cmd` (or a CI step) that parses `qfs-cmd`'s manifest /
`cargo metadata` and asserts its *direct* dependency set is exactly `{qfs-core, clap, tracing,
…}` and contains none of `{qfs-lang, qfs-plan, qfs-driver, qfs-codec, qfs-parser}`. This turns the
G5 boundary from a doc rule into a failing build, which is the only thing that survives deadline
pressure across 39 tickets. (If a metadata test is judged too heavy for E0, at minimum record G5
as a named acceptance criterion in `ARCHITECTURE.md` so the Architect can model-check it during
the Coding Phase.)

**Concern C5 — the `qfs-core → qfs-parser` consuming seam is unstated in the design spine.** As
noted in §2.2: the model has `qfs-core` consuming `parse_statement`; the design's spine only
draws `qfs-parser → qfs-lang`. At E0 this is harmless (no call site yet), but E1 *will* need
`qfs-core` (or a dispatch layer) to call `parse_statement`, which adds a `qfs-core → qfs-parser`
edge. If `qfs-parser` ever needs a core type, that becomes a cycle.

*Structural proposal:* Record the intended E1 edge now so it does not force a restructure: state
in §1.2 (or `ARCHITECTURE.md`) that "`qfs-core` will consume `qfs-parser::parse_statement` in E1;
`qfs-parser` depends only on `qfs-lang` and must never depend on `qfs-core`" — i.e. fix the
*direction* of the future edge at foundation time. This preserves the acyclic spine by making the
parser a leaf that `qfs-core` calls *down* into, never the reverse, which is exactly the model's
B6 position ("between `qfs-lang` and `qfs-core`'s dispatch, never above `qfs-cmd`").

### 2.4 Risk-register coherence note (not a concern)

The design's R1/R5/R8 night-park handling is structurally sound and matches model A3 (wasm32 is a
constraint, not an E0 deliverable). The decision to keep both spikes under `spikes/` as ADR
evidence (§3.4) is the right reversibility posture for G6. No structural objection.

---

## 3. Cross-artifact coherence assessment

**Overall: high coherence.** The three artifacts form a clean business → structure → engineering
chain with no contradictions in scope, governance thesis, or the spike's purpose.

- **Scope:** Direction assumptions 1–4, model A1–A4, design A1–A6 independently agree on
  scaffold-only / scope-fixed / spike-ends-in-decision. No drift.
- **Governance boundary:** Direction §2/§4 (business), model §3 G1–G2 (structure), design §2.4/§2.6
  (engineering) describe the *same* closed-core/open-registry boundary at three altitudes. This is
  the trip's central translation, and it is faithful end-to-end.
- **AI-facing structured errors:** Direction Persona B → model CfsError → design §2.3 `code()`/
  `report(json)` envelope. Traceable.
- **The one coherence gap worth the lead's attention** is **P1/R5** — the direction's "cross-compile
  proven tonight" outcome line vs. the design's CI-only/parked reality. Both are individually
  correct; the *seam between them* needs the one-sentence qualification proposed in P1 so the
  morning reader's business-promise-to-engineering-reality trace is exact. This is a wording
  reconciliation, not a plan change.

**Restructure risk for E1–E8 (the task's key question):** I find **no** element in either artifact
that forces a restructure when later tickets land inside the seams, *provided* C4 (mechanical G5
enforcement) and C5 (declared `qfs-core → qfs-parser` direction) are adopted. Those two are the
only places where a future ticket could silently introduce a back-edge or cycle. Both are cheap to
fix now and expensive to discover later — precisely the Planner's §2 "seam-rework cascade" risk,
caught at the foundation.

---

## Review Notes

- Neither artifact requires a *Request revision*; both are Approve-with-minor-suggestions. The
  Constructor's design realizes all six model guards and the acyclic spine faithfully.
- The two suggestions I would most want carried into the Coding Phase as *acceptance criteria* (so
  I can model-check them analytically) are **C4** (mechanical cmd-logic-free / dependency-direction
  enforcement) and **G1/C1** (single-source keyword fixture). They are the two boundaries whose
  erosion would most directly break the "one grammar, two faces" thesis.
- For the lead: there are **no escalations** from my side. The P1 wording reconciliation and C1–C5
  are suggestions the authors may fold into v1-as-is or carry as Coding-Phase acceptance notes; none
  block consensus.
