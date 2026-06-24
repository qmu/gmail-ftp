# Round 1 Review — Planner

- **Reviewer**: Planner (Progressive)
- **Domain lens**: business outcome / stakeholder value (developer + AI-agent value; de-risking the 40-ticket program)
- **Artifacts reviewed**: `models/model-v1.md` (Architect), `designs/design-v1.md` (Constructor)
- **Coherence baseline**: `directions/direction-v1.md` (my own)
- **Date**: 2026-06-22

---

## Artifact 1 — `models/model-v1.md` (Architect)

### Decision: **Approve with minor suggestions**

The model is the strongest possible structural service to the business direction. It does the
one thing the direction said mattered most: it makes the **closed-core / open-registry**
governance boundary a *structural fact* rather than a paragraph (G1 golden test, G2 generic
registries), and it encodes the **AI-facing structured-error** promise as a single `CfsError`
home plus a purity invariant proven at the type level (G3). The §1 coherence table is a
traceability instrument a stakeholder can actually use — every RFD concept maps to exactly one
crate and back, which is precisely the "stakeholders can trace the reasoning" bar I hold the
model to. The Go-reference carry-over table (§2) is a real business asset: it converts hard-won
production lessons (the `shell.gmailClient` fake-able narrow interface, the SDK quarantine, the
append-only audit) into *retained* seams rather than rediscovered ones — that is compounding
velocity made concrete.

**Concern P-M1 — "Approve" decisions carry no business-outcome acceptance phrasing.** The model
states fidelity guards G1–G6 in structural terms ("golden test", "pub-use audit", "no back-edge").
These are correct, but a stakeholder reading the model cannot tell *which guard, if dropped,
breaks which business promise*. G3 dropping silently re-opens the AI-agent surface; G1 dropping
silently re-opens governance erosion — the exact two §2 risks in my direction. Right now that
linkage lives in my head and the direction, not in the bridging artifact.

> **Proposal (business outcome):** Add a one-line "business consequence if violated" to each of
> G1–G6 in the Review Notes (e.g., G1 → "governance erosion: keyword set degrades to a
> convention"; G3 → "AI-agent hostility: errors become prose the agent must pattern-match").
> This is a doc-only change with high stakeholder-traceability payoff and zero structural cost —
> it lets the morning reviewer (and every E1 author) see *why* a guard is load-bearing, not just
> *that* it is. This is a suggestion, not a blocker.

**Concern P-M2 — G4 (server-is-a-driver) is correctly flagged as the subtlest fidelity risk but
left as "doc + TODO anchor."** I agree with the model that an E0 `serve()` that bypasses the
`/server` mount silently contradicts §8 and reintroduces a "server is special" boundary. The
business stake is the **one-engine, one-mental-model** promise that makes qfs trustworthy to an
AI operating it unattended (direction §4). A TODO comment is cheap, but it is also the kind of
reservation that erodes under deadline pressure across 39 tickets — the same erosion dynamic I
flagged for governance.

> **Proposal (business outcome):** Keep the doc + `TODO(E7)` anchor (cheap, correct for E0), but
> ask that E0 *also* land an ignored/`#[ignore]`-style placeholder test named for the §8
> contract (e.g. `server_is_registered_as_a_driver` marked `todo!()`/ignored). A named failing-
> reserved test is a louder, harder-to-erode anchor than a comment, and it gives me a concrete
> E2E/structural hook to validate the seam was honored when E7 arrives. Minor; either form is
> acceptable.

The model stays cleanly inside E0 scope. It explicitly defers the DuckDB-vs-own-evaluator E3
decision and confirms t01 does not leak that dependency (§ Review Notes) — exactly the scope
discipline my direction §2 called the night's correct posture.

---

## Artifact 2 — `designs/design-v1.md` (Constructor)

### Decision: **Approve with observations**

The design is buildable, faithful to the model's spine, and unusually honest about its own
failure modes — the R1–R8 risk table is the kind of pre-mortem that protects the business
outcome rather than decorating it. It delivers all three direction goods: a buildable 40-ticket
starting point (the acyclic workspace, §1.2/§2.1), governance-as-structure (the verbatim §3
keyword golden set + the `deny`-level lib lints that make "no keyword smuggling" a compiler
fact, §2.6/§2.1), and a *decision-first* parser spike whose durable artifact is an ADR with
committed error-corpus evidence (§3) — which is exactly the "decision risk retired" outcome the
direction asked t02 to produce. The choice to make the ADR the durable artifact while spike
binaries "may rot" (§3.3) is precisely right: the business value is the locked decision, not the
throwaway code.

**Concern P-O1 — the 9th crate (`qfs-parser`) and the parked CI-only items are scope decisions a
stakeholder must be able to see and accept, not infer.** The design adds `qfs-parser` as a
workspace member created by t01-as-prerequisite (§1.2) and parks three items (local x86_64
cross-link R1, local wasm32 R5, offline fetch R8) as CI-only / Night-Park. I want to be explicit
on the record about whether each is acceptable against the direction:

- **9th crate `qfs-parser`** — **Accepted, not scope creep.** It is named in both my direction's
  resolved-scope (t02 "thin parser-skeleton crate") and the model's §1 table. Creating it empty
  in t01 and filling it in t02 is the correct gate handling, not expansion. No concern.

- **x86_64 cross-link parked to CI-only (R1)** — **Accepted.** My direction's "cross-platform
  regret" risk (§2) is satisfied by *proving cross-compilability*, and CI is a legitimate place
  to prove it; the host genuinely cannot install a cross-linker under system-safety. The business
  promise ("single static binary, runs everywhere") is kept honest as long as CI is *actually
  wired and green*, not merely described.

- **wasm32 parked to CI-only (R5)** — **Accepted with one observation.** The tickets defer wasm32
  as a *deliverable*; the model (A3) and design (A2) both treat it correctly as a *constraint*,
  not an E0 build target. t02's acceptance line requiring a `qfs-parser` wasm32 build is honored
  in CI, with the size-delta sourced from the CI artifact. Given the tickets defer wasm32, parking
  the *local* build is the right call and does not under-deliver the night's intent.

> **Observation / proposal (business outcome):** My only ask is that these three parks not become
> *silent* under night mode. The disciplined-scope risk in my direction §2 is over-reach; the
> mirror risk here is *under-delivery hidden behind a green local build*. Concretely: if the
> wasm32 (R5) or x86_64 (R1) CI legs cannot run-to-green before morning, the design's own
> Night-Park path must fire and the parked acceptance line must surface in `plan.md` + the morning
> report — so the morning reviewer sees "cross-targets proven in CI: yes/parked" as an explicit
> line, not an assumed pass. This is already the design's stated intent (§6 R1/R5, §7); I am
> asking the team to treat *surfacing the park* as itself an acceptance criterion. Observation,
> not a revision request.

**Concern P-O2 — the t02 "keep both spikes vs delete the loser" choice trades a little repo
cleanliness for evidence durability.** The design chooses to keep both winnow and chumsky spikes
under `spikes/` marked non-production (§3.4), against the ticket's step-9 option to delete the
loser. From a business lens this is the right trade — the ADR references the losing spike as
evidence, and a future reader (human or AI) re-litigating the parser choice benefits from being
able to re-run the comparison rather than trust prose. The only cost is a small amount of
non-production code in the tree.

> **Proposal (business outcome):** Keep both (I endorse the design's choice), but require the
> retained loser to carry a top-of-file banner pointing at `docs/adr/0001-parser-library.md` so
> any reader — especially an AI agent grepping the tree — immediately learns the code is decided-
> against evidence, not a live second parser. This protects the "one locked decision" outcome
> from being misread as "two parsers in flight." Trivial; doc-comment only.

The design is otherwise tightly scoped: it explicitly does not touch the Go tree (§1.1), adds no
async runtime (A4), introduces only `thiserror` + clap + the two parser libs as dependencies, and
defers every E1+ concern. No scope creep beyond the resolved E0.

---

## Cross-Artifact Coherence

**Do the model and design faithfully serve the business direction? — Yes, coherently and with
strong traceability.** I assess the three direction pillars:

1. **Compounding velocity across 40 tickets.** The model's acyclic dependency spine (§4) and the
   design's matching `members`/dep-graph (§1.2/§2.1) are the same structure described twice from
   two angles — structural and buildable. Both forbid the back-edge (`qfs-cmd` reaching past
   `qfs-core`) that would collapse "two faces of one engine" into "two engines." This is the
   single most velocity-relevant invariant and both artifacts agree on it verbatim. Coherent.

2. **Governance-as-structure.** Direction §1/§4 demanded the frozen keyword set become a
   structural fact. Model G1 specifies a golden test against the RFD §3 list verbatim; design
   §2.6 transcribes that list and §4 locks it with an order-independent set+count compare, backed
   by `deny`-level lib lints so no later crate has anywhere to put a new keyword. The governance
   boundary is enforced at three layers (single home, golden test, lint) — stronger than the
   direction asked for. Fully coherent.

3. **AI-agent-consumable structured errors.** Direction Persona B demanded typed, predictable
   errors over stringly prose. Model anchors one `CfsError` home (§1) and the purity invariant
   that makes parse-time rejection possible (G3). Design realizes it as a `thiserror` enum with a
   `code()` method and a `{"error":{...}}` JSON envelope mirroring the existing Go contract
   (§2.3) — continuity that an agent already trained on the Go tool benefits from. Coherent, and
   the JSON-envelope continuity is a value-add neither the direction nor I anticipated.

**One cross-artifact watch-item (not a revision request):** the model's G3 (purity proven by a
no-I/O `Driver`/`Codec` instantiation) and the design's §4 decision to satisfy it with an
*in-test dummy impl rather than `trybuild`* are mutually consistent, and I accept the design's
lighter-weight choice. But the Architect's own Review Notes are emphatic that "if this is only a
doc comment, E0 has failed its core job." The design does encode it as a real instantiated test
(§4 purity compile-test), not a comment, so the two artifacts *do* agree — I flag it only so the
Coding Phase E2E/QA step (mine) explicitly confirms the purity test is an actually-compiled,
actually-run instantiation and not silently downgraded. I will hold that as a Coding-Phase
acceptance check.

**Scope creep assessment across both artifacts: none found.** The 9th crate is in-scope per the
tickets and both artifacts; wasm32 and x86_64-local are correctly treated as
constraint/CI-only and parked, consistent with the tickets deferring wasm32; the Go tree is
untouched; no E1+ feature work appears. The disciplined-scope posture my direction §2 named as
the night's correct outcome is honored by both artifacts.

---

## Summary of Decisions

| Artifact | Decision | Revision requested? |
|---|---|---|
| `models/model-v1.md` (Architect) | Approve with minor suggestions | No |
| `designs/design-v1.md` (Constructor) | Approve with observations | No |

No "Request revision" items. Two minor suggestions to the model (P-M1 business-consequence
annotations on G1–G6; P-M2 named reserved test for G4) and two observations to the design (P-O1
surface the CI-only/Night-Park status as an explicit acceptance line; P-O2 banner the retained
losing spike). All are doc-or-test-level, none block convergence. One Coding-Phase acceptance
check recorded for myself: confirm the G3 purity test is a real instantiation, and confirm the
x86_64/wasm32 CI legs are green-or-explicitly-parked in the morning report.
