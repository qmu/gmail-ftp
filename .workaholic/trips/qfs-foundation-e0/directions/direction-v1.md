# Direction v1

Author: Planner (Progressive)
Status: draft
Reviewed-by: (pending)
Phase/Step: planning/artifact-generation
Date: 2026-06-22

---

## Night-Mode Assumption (read first)

This trip was launched as an **empty `/trip night` instruction**. No human is present to
clarify scope. Per the trip protocol's Night Mode, I (the Planner) have adopted the team
lead's recorded interpretation as the most reasonable reading and I am stating it here
prominently so every downstream reader can trace the reasoning:

> **Resolved scope:** Build the **qfs foundation — Epic E0** of the RFD-0001 from-scratch
> Rust rebuild. Concretely two tickets, in order:
> - **t01** — the Rust workspace + single-binary scaffold (one binary that is both CLI and
>   server; typed module/crate seams for core / lang / plan / driver / codec / server; the
>   three open registries; the Driver and Codec trait shapes; clippy + rustfmt + CI;
>   aarch64 and x86_64 cross-compile).
> - **t02** — gated on t01: a parser-library decision spike (winnow vs chumsky, evaluated
>   side-by-side on a tiny `FROM |> WHERE |> SELECT` grammar), an ADR that locks the choice,
>   and a thin parser-skeleton crate.

**Assumptions I am recording (rather than asking):**

1. **Scope is fixed at invocation.** E0/t01+t02 only. The 39 downstream tickets (E1–E8) are
   explicitly out of scope tonight. No language features, no real drivers, no runtime
   execution, no server endpoints are to be built — only the foundation that makes those
   buildable.
2. **"Business outcome" for internal tooling.** qfs has no external paying customer tonight;
   its "market" is the developer who owns this repo and the **AI agents** that will operate
   qfs. So I judge business value as *value delivered to those two user classes*, plus the
   strategic value of de-risking a 40-ticket program.
3. **Foundation correctness outranks foundation speed.** Because 39 tickets inherit E0's
   seams, I treat a clean, well-bounded foundation as the night's primary deliverable and
   accept that t02 (the spike) may legitimately conclude with a *decision and a skeleton*
   rather than a feature.
4. **A spike is allowed to end in a recommendation, not a finished parser.** t02's business
   value is *removing a decision risk*, not shipping parse coverage. An ADR that locks
   winnow-or-chumsky with traceable rationale fully satisfies the night's intent.

If any of these assumptions is wrong, the cost of correction is low (E0 is reversible and
self-contained), which is exactly why proceeding unattended is safe.

---

## Content

### 1. Value Proposition — what a clean E0 foundation delivers

qfs's whole reason to exist (per RFD-0001) is **"one grammar instead of N SDKs, built for
AI."** An agent learns one small DSL and one operating procedure
(`DESCRIBE → write → PREVIEW → COMMIT`) and can then operate Gmail, Drive, S3, GitHub,
SQL, git, and local files uniformly. That promise is only as trustworthy as the foundation
it stands on. E0 is the foundation, and tonight it delivers three concrete business goods:

- **A credible, buildable starting point for a 40-ticket program.** The single biggest risk
  to the rebuild is not any one feature — it is *starting the 40-ticket climb on seams that
  later have to be torn out*. E0 delivers a workspace where the closed core, the three open
  registries, and the driver/codec extension points already have their *shapes* in place, so
  every later ticket plugs in instead of restructures. The business value is **compounding
  velocity**: each of the 39 downstream tickets gets cheaper because the seams are right.

- **Governance made real on day one.** The RFD's central design bet is "**closed core +
  three open registries**": the keyword set is frozen forever, and *all* growth happens by
  adding paths, functions/procedures, and codecs — never keywords. E0 is where that
  governance boundary stops being a paragraph and becomes a structural fact of the codebase.
  The business value is a **stable contract for everyone who extends qfs**: a driver author
  (human or AI) can add Gmail or S3 support knowing they cannot accidentally fork the
  language. That stability is the entire premise of "learn the grammar once."

- **A resolved parser bet (t02).** The RFD leaves the parser library as an explicit open
  question (winnow as default, chumsky as fallback, "a spike confirms before lock-in"). An
  unresolved foundational tooling choice is a *latent re-work liability* hanging over E1's
  entire language core. Tonight's spike converts that open question into a **locked,
  documented decision** with a side-by-side comparison behind it. The business value is
  **decision risk retired** — the language team starts E1 without a foundational unknown.

### 2. Business / Strategic Risk — the cost of getting the foundation wrong

This is the section that most justifies spending a night here. The risks are
*disproportionately back-loaded*: a foundation mistake is cheap to make and expensive to
discover, because it surfaces only after many tickets have built on top of it.

- **Seam-rework cascade (highest impact).** If the boundary between the frozen core and the
  open registries is drawn in the wrong place, or if the driver/codec extension points have
  the wrong shape, the cost is not one ticket — it is *every driver and every codec ticket
  re-touched* once the mistake is found. Mitigation (and the business reason to invest a full
  night): get the seams reviewed by all three perspectives *before* anyone builds on them.

- **Governance erosion.** If E0 does not make the closed-core / open-registry boundary
  structurally enforced, the "frozen keyword set" degrades into a *convention* that erodes
  under deadline pressure as later tickets are tempted to "just add a keyword." Once the core
  is no longer truly closed, the "learn one grammar" promise — the product's reason to exist
  for AI — is broken. The foundation is the only cheap moment to make this boundary real.

- **Parser lock-in regret.** Choosing the parser library casually now, and discovering in E1
  that its error-recovery or maintenance posture is wrong, forces a rewrite of the language
  core. The RFD already flags the live trade-off (winnow's active maintenance vs. chumsky's
  parse-error recovery). The spike exists precisely to spend a small, bounded cost tonight to
  avoid a large, unbounded one later. The strategic risk is *deciding without evidence*; the
  spike's deliverable is the evidence.

- **Cross-platform regret.** qfs must ship as one static binary on aarch64 and x86_64 (and
  later wasm32 for Workers). If cross-compilability is not proven in E0, a platform-specific
  dependency can sneak in across 39 tickets and only be discovered at release. Proving the
  two server/dev targets compile *now* keeps the "single static binary, runs everywhere"
  distribution promise honest from the start.

- **Night-mode-specific risk: over-reach.** With no human present, the tempting failure is to
  "make progress" by starting E1 features on top of an unreviewed foundation. I explicitly
  flag the opposite as correct: the disciplined, lower-risk outcome tonight is a *small,
  clean, well-reviewed* E0, even if it looks modest in the morning. Scope creep is the risk;
  scope discipline is the mitigation.

### 3. User Personas

qfs is internal/developer tooling, so its "users" are the people and agents who will *build
on and operate* the foundation. E0 serves them before it serves any end feature.

- **Persona A — The human developer (the repo owner).** Wants to climb 40 tickets without
  re-laying track. Cares that the workspace builds cleanly, that lints and CI catch mistakes
  automatically, that the module seams tell an honest story about where things go, and that
  the parser decision is made *once*, with rationale he can revisit. Success for this persona:
  *the morning after, he can pick up any E1 ticket and know exactly where it plugs in.*

- **Persona B — The AI agent operating qfs (the RFD's headline user).** qfs "exists for AI."
  The agent's defining need is **machine-legible structure**: it learns one frozen grammar
  and relies on **structured errors** to self-correct (the RFD specifies that unsupported
  operations are rejected *at parse time with structured errors* — "important for AI"). E0 is
  where the commitment to structured, typed, machine-readable surfaces is established. If E0
  bakes in stringly-typed, ad-hoc error handling, every later ticket inherits an
  agent-hostile surface; if E0 establishes typed/structured seams, the agent's
  `DESCRIBE → write → PREVIEW → COMMIT` loop stays reliable as the system grows. Success for
  this persona: *the foundation's errors and capability rejections are structured and
  predictable, so an agent can reason about them instead of pattern-matching prose.*

- **Persona C — The future driver/codec author (human or AI).** Will add Gmail, S3, git,
  etc. as drivers, and json/yaml/csv/markdown as codecs. Cares that the Driver and Codec
  *contracts* are small, stable, and additive — that adding a backend is "fill in a trait,
  register a mount" and never "modify the core." E0 defines exactly the contract this persona
  will live inside for the rest of the program.

### 4. System Positioning — why these foundational choices serve users

- **Why Rust, and why a single binary.** The RFD chooses Rust specifically because the AST,
  effect-plan, capabilities, and archetypes are *sum types* — the domain is naturally modeled
  as exhaustive, typed alternatives, which is exactly what serves Persona B (an agent reasons
  far better against a closed, typed model than an open, dynamic one) and Persona C (the
  compiler enforces the driver contract). The **single binary that is both CLI and server**
  is a direct user benefit: the same engine runs interactively on the developer's laptop
  (Persona A's "now") and unattended as a daemon/Worker (the server's event/schedule/request).
  One binary means *one mental model, one distribution artifact, one behavior to trust* —
  there is no "the CLI does X but the server does Y" gap for an agent to trip over. E0's job
  is to make CLI and server genuinely one engine from the first commit, not two programs
  bolted together later.

- **Why closed core + open registries matters to users.** This is the structural expression
  of the product's one-sentence promise. For **Persona B**, a frozen keyword set is what makes
  "learn the grammar once" literally true — the grammar an agent learns today is the grammar
  forever; new services never change it, they only add mounts/functions/codecs the agent
  discovers via `DESCRIBE`. For **Persona C**, three open registries are what make extension
  *safe and unprivileged* — you can add the world without touching (or being able to break)
  the core. E0 is the moment this dual property (closed where it must be, open where it should
  be) is set in stone; getting it right here is what lets the system grow without the grammar
  rotting.

- **Why settle the parser tonight (t02).** The parser library is foundational *tooling*, not a
  feature — it sits underneath all of E1. Positioning it as an E0 spike (decide-then-build)
  rather than an E1 assumption (build-then-discover) is the difference between a documented bet
  and an accidental one. The user-facing payoff is downstream: a parser chosen with attention
  to **structured parse errors** directly serves Persona B's self-correction loop, and one
  chosen with attention to **active maintenance** serves Persona A's long-term sanity.

### 5. Business Rationale — why a night on E0 before the 39 downstream tickets

The single strongest argument for tonight's scope is **leverage**: E0 is the one ticket whose
quality multiplies across all 39 others. Spending a focused, fully-reviewed night on the
foundation is cheap insurance against the seam-rework cascade, governance erosion, and parser
regret described in §2 — every one of which is *an order of magnitude more expensive to fix
after downstream tickets depend on it.*

Three more reasons this is the right night's work:

1. **It is the natural gate.** t02 is explicitly gated on t01, and all of E1–E8 are gated on
   E0. There is no useful downstream work that can correctly begin before the foundation and
   the parser decision exist. Doing E0 first is not a preference; it is the dependency order.
2. **It is bounded and reviewable in one night.** Unlike feature epics, E0 is a scaffold plus a
   spike — exactly the kind of small, self-contained, high-leverage unit that the three-agent
   review process can fully scrutinize in a single planning round. The work fits the night; the
   review fits the work.
3. **It is the cheapest possible reversal point.** If a foundational assumption turns out wrong,
   discovering it tonight — when nothing is built on top — costs almost nothing to correct.
   This is the safest moment in the entire 40-ticket program to be wrong, which is precisely
   why it is the right moment to invest scrutiny.

**Outcome I will hold the team to (and later E2E-validate as Planner):** by morning, the repo
contains a clean foundation that builds and cross-compiles, has automated quality gates, makes
the closed-core/open-registry boundary and the driver/codec contracts structurally explicit,
and carries a documented, locked parser decision — such that any E1 ticket can begin the next
day with no foundational unknowns. Modest in surface area, maximal in leverage.

---

## Review Notes

(pending one-turn review by Architect and Constructor)
