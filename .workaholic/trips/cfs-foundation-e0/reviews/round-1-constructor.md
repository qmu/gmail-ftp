# Round 1 Review — Constructor

- **Reviewer**: Constructor (Conservative / engineering-quality & production-readiness lens)
- **Artifacts reviewed**: `directions/direction-v1.md` (Planner), `models/model-v1.md` (Architect)
- **Own artifact (for coherence)**: `designs/design-v1.md`
- **Date**: 2026-06-22
- **Phase/Step**: planning/one-turn-review

---

## Environment facts established for this review

I verified the build environment empirically (not from memory) because both artifacts make claims about buildability and CI that hinge on it. Results:

- **Toolchain**: `rustc/cargo 1.96.0`, active toolchain `stable-aarch64-unknown-linux-gnu`. There is **no channel literally named `1.96.0`** installed — only `stable`.
- **Targets installed**: `aarch64-unknown-linux-gnu`, `x86_64-unknown-linux-gnu` (rust-std for both). `wasm32-unknown-unknown` is **absent**.
- **Cross-linker**: `gcc`/`cc` present (native aarch64 only); `x86_64-linux-gnu-gcc` is **absent**.
- **Crate cache**: `~/.cargo/registry` **does not exist at all** (no index, no cache, no src).
- **Network**: `https://static.crates.io/` returns **HTTP 403**. No crate can be fetched.

These facts materially change the severity of three risks already named in my own design (R1, R5, R8) and surface one risk none of the three artifacts named (the toolchain-channel pin). I treat that as my own design's gap too and flag it for my v2.

---

## Artifact 1 — `directions/direction-v1.md` (Planner)

### Decision: **Approve with minor suggestions**

The direction is well-scoped, honest about night-mode discipline, and its core thesis — "E0 is the one ticket whose quality multiplies across 39 others, so spend the night on a small clean reviewed foundation, not features" — is exactly the right business framing for a foundation epic. The four recorded assumptions (esp. A3 "correctness outranks speed" and A4 "a spike may end in a decision, not a parser") are the correct expectations to set and are technically achievable. Personas A/B/C map cleanly onto real engineering obligations (structured errors, additive trait contracts, frozen keywords). No part of the direction asks the build to deliver runtime behavior, real drivers, or server endpoints, so it does not overcommit the headline scope.

**Concern 1 (business expectation the offline build cannot fully meet in one night) — the "builds and cross-compiles" morning outcome.**
§5's closing "Outcome I will hold the team to" promises a foundation that *"builds and cross-compiles"* and *"carries a documented, locked parser decision."* Both of those phrases set a stakeholder expectation that the verified environment cannot fully meet tonight:
- **Cross-compile**: the x86_64 *binary* link will fail locally (no `x86_64-linux-gnu-gcc`); only CI can prove it.
- **Locked parser decision**: t02's comparison requires fetching `winnow` *and* `chumsky` from crates.io, which currently returns 403 with an empty cache. A side-by-side spike may be impossible tonight, in which case the "locked decision" degrades to a *provisional* winnow default with the comparison parked for CI/online follow-up.

This is not a reason to change scope — it is a reason to **pre-frame the morning outcome honestly** so the developer is not surprised. *Proposal*: soften §5's outcome sentence to distinguish what is *locally provable tonight* (workspace builds + tests green on native aarch64; structural seams; keyword golden) from what is *CI-or-online-gated* (x86_64 link, wasm32 size, the head-to-head parser evidence). Frame a parked parser comparison as a **provisional decision with recorded reversibility** (the owned `ParseError` wrapper guarantees the choice is cheap to revisit) — which is fully consistent with the Planner's own A4. This keeps the business promise truthful rather than aspirational.

**Concern 2 (cross-artifact coherence, minor) — "structurally enforced" governance is a doc-test, not a compiler guarantee.**
§2 ("Governance erosion") and §4 promise that E0 makes the closed-core boundary *"a structural fact of the codebase"* so a keyword cannot be added under deadline pressure. Technically, the strongest E0 can do is a **golden test** (Architect's G1, my §2.6/§3) — which is CI-enforced, not compiler-enforced. A determined later ticket *could* edit both `cfs-lang` and the golden list together. *Proposal*: keep the business claim but qualify it as "test-enforced governance with a single keyword home" rather than implying the compiler makes it impossible. Cheap wording change; preserves traceability of the promise to its actual mechanism.

---

## Artifact 2 — `models/model-v1.md` (Architect)

### Decision: **Approve with minor suggestions**

This is a strong, faithful model. The §1 RFD-concept→crate table is one-to-one and load-bearing, the §4 acyclic dependency spine is exactly the invariant I will defend in the Coding Phase, and the carry-over table (§2) correctly mines the Go reference for *seams to keep* rather than code to port. The six guards (G1–G6) are the right set, and they align with my design almost exactly: G1↔§2.6 keyword golden, G2↔§2.4 registries-over-trait-objects, G3↔§2.5/§4 purity, G4↔§2.8 server stub, G5↔§1.2 cmd dependency rule, G6↔§3.4 pub-use audit. The task asks specifically whether the six guards are buildable and testable at E0 — my assessment per guard:

| Guard | Buildable at E0? | Testable at E0? | Note |
|---|---|---|---|
| G1 keyword golden | Yes (std-only) | Yes, plain `#[test]` set-compare | No external dep; unaffected by offline state. |
| G2 registries generic over `dyn Driver`/`dyn Codec` | Yes (std-only) | Yes, register/resolve round-trip test | Solid. |
| G3 purity invariant | **Yes** | **Yes — but NOT via `trybuild`** (see Concern 1) | The invariant is provable; the *tool* the model implies is not viable offline. |
| G4 server-is-a-driver | Yes (doc + `TODO(E7)` anchor) | Compile-only; no behavior to test | Cheap, correct. |
| G5 cmd holds no domain logic | Yes | Partially — see Concern 2 | "clippy can enforce this" overstated. |
| G6 parser boundary reversibility | Yes (owned `ParseError`) | Yes, `pub use` audit test | Sound; my §3.4 matches. |

**Concern 1 (the task's headline question) — G3's `trybuild` proof is not feasible offline, and the model's framing risks over-specifying the mechanism.**
The Review Notes and §3.3 phrase G3 as something *"the compile-only/`trybuild` test must prove."* Two problems, both confirmed against the environment:
1. **`trybuild` is an external crate** (`~/.cargo/registry` is empty, crates.io returns 403). It **cannot be fetched tonight**. Any G3 implementation that hard-depends on `trybuild` is unbuildable in the current environment.
2. More importantly, **`trybuild` is the wrong tool for G3 anyway.** `trybuild` proves that code *fails to compile* (negative compile tests). G3 is a *positive* structural property: "a no-I/O `Driver`/`Codec` impl instantiates and is usable, and the trait offers no async/`&mut self`/unit-with-effects method to do I/O through." That is proven by an **ordinary in-crate `#[test]` with a dummy no-I/O impl** (exactly what my design §2.5/§3 §"Purity compile-test" already specifies as the chosen approach — explicitly *without* `trybuild`). A `trybuild` negative test would only help if we wanted to assert "this *bad* impl is rejected," which requires authoring a compile-failure fixture and is both heavier and less central.

*Proposal*: the model should state G3's proof obligation as **"an in-crate compile+instantiate test of a no-I/O dummy impl; the trait signatures (data-returning, no futures, no `&mut self`-with-effects, no unit-side-effect method) are the structural guarantee."** Drop the `trybuild` framing to "optional, only if a *negative* compile-fail fixture is later wanted." This removes the offline-crate-fetch dependency from the single most consequential guard and keeps G3 fully provable tonight. My design and the model already agree on the substance; this is a one-line correction to the *named tool* so the model does not bless an offline-infeasible mechanism. **This resolves the task's explicit question: yes, G3 is buildable and testable at E0 — but via a plain `#[test]`, not `trybuild`, and the offline-crate-fetch risk for `trybuild` is real and currently blocking.**

**Concern 2 (testability of G5) — "clippy can enforce dependency direction" is not accurate.**
§3 G5 and §4 B1 say a clippy / dependency-direction check can enforce that `cfs-cmd` depends only on `cfs-core`. Stock clippy has **no lint** for "crate A must not depend on crate B"; this is not enforceable by clippy out of the box. The honest E0 mechanisms are: (a) simply *not listing* `cfs-lang`/`cfs-plan`/`cfs-driver`/`cfs-codec` in `cfs-cmd`'s `[dependencies]` (the compiler then makes their types unreachable — this is the real structural guarantee, and it needs no tool), and optionally (b) a `cargo-deny`/`cargo tree` assertion in CI (an external tool, so offline-gated). *Proposal*: restate G5's enforcement as **"`cfs-cmd`'s `Cargo.toml` declares only `cfs-core` (+ `clap`/`tracing`); the dependency graph itself is the enforcement,"** with an optional CI `cargo tree` grep as belt-and-suspenders. This is strictly stronger than relying on a clippy lint that does not exist.

**Coherence note (positive):** B7 correctly observes that t02's `cfs-parser` must build for `wasm32-unknown-unknown` and therefore must honor wasm-friendliness strictly. My design's R5 flags that `wasm32` is *not installed* and treats it as CI-only/parked — the model and design are consistent here. No conflict; I only add (below, cross-artifact) that the offline state makes even the CI fallback non-trivial tonight.

---

## Cross-artifact coherence assessment

The three artifacts are strongly coherent: same eight-crate split, same acyclic spine, same six-guard set, same "decision-not-feature" reading of t02, same wasm/cross-compile deferral posture. The Architect's dependency-direction spine (model §4) and my §1.2 dep graph are identical, and the Planner's leverage argument justifies exactly the scope the other two artifacts build. I found **no contradiction** between any two artifacts.

The one theme that runs across all three and is **under-weighted everywhere** is the **offline / empty-cache reality**, which I now treat as the dominant production-readiness risk for tonight:

- **Empty registry + crates.io 403.** My own design's R8 rates offline crate-fetch "Med likelihood." The verified state is that it is the **current, near-certain** condition: there is no cargo cache directory at all and the registry endpoint returns 403. *Every* external dependency the plan relies on — `clap`, `thiserror`, `tracing` (t01) and `winnow`, `chumsky`, optionally `insta` (t02) — is presently unfetchable. **Consequence I will carry into my design v2 and the Coding Phase:** the *only* guaranteed-buildable t01 surface tonight is the **std-only skeleton** (workspace, crate graph, `cfs-lang` keyword golden, the three registries, `CfsError` without `thiserror` derive, the trait/enum seams, the purity `#[test]`). The clap-based dispatch, `thiserror`, and all of t02 are **at risk of a Night Park** until a cache or network exists. This is consistent with my R8 park strategy but the *likelihood* must be promoted from Med to High/near-certain. I will revise R8's likelihood in design v2 and pre-stage a `thiserror`-free `CfsError` (hand-written `Display`/`Error` impls) so the core seams build with **zero external deps** — making the foundation's load-bearing structure provable regardless of network.

- **Toolchain-channel pin (gap in all three artifacts, including mine).** My design A1 / §2.1 pins `channel = "1.96.0"` in `rust-toolchain.toml`. But the installed toolchain is named **`stable`**, and there is no `1.96.0` channel directory. rustup, seeing `channel = "1.96.0"`, will attempt to **download** that exact channel — which fails offline. *Proposal (for my own v2)*: pin `channel = "stable"` (the installed, network-free toolchain, which *is* 1.96.0) and record the exact version in `ARCHITECTURE.md`, rather than pinning a numeric channel that forces a download. None of the three artifacts caught this; I flag it as a Constructor-owned correction.

- **CI assumptions are sound but unverifiable tonight.** Both my design (§4 CI workflow, cross matrix, wasm job) and the model (B7) lean on GitHub Actions runners to do what this host cannot (install `wasm32`, provide an x86_64 cross-linker, fetch crates). That division of labor is correct and is the right place to put those gates. The honest caveat for the morning report: **none of the CI-gated guarantees (x86_64 link, wasm32 build/size, the parser head-to-head) can be proven in this worktree tonight** — they are proven only when CI first runs online. The plan should say so explicitly rather than implying the morning artifact is fully validated.

---

## Summary of decisions

| Artifact | Decision | Revision requested? |
|---|---|---|
| `directions/direction-v1.md` (Planner) | **Approve with minor suggestions** | No |
| `models/model-v1.md` (Architect) | **Approve with minor suggestions** | No |

**No "Request revision" items.** Both artifacts are accepted; my suggestions are wording/mechanism refinements the authors may fold in at their discretion. The two substantive engineering corrections (G3 proven by plain `#[test]` not `trybuild`; G5 enforced by the dependency graph not a nonexistent clippy lint) are already the substance of my own design, so they require no change to the model's structure — only to its named tooling.

**Constructor-owned follow-ups for my design v2** (not blocking the other artifacts): (1) promote R8 likelihood to near-certain and pre-stage a `thiserror`-free `CfsError`; (2) change the toolchain pin from `1.96.0` to `stable`; (3) sharpen the §5 morning-outcome framing into "locally proven tonight" vs "CI/online-gated."

---

## Review Notes

(authored by Constructor; no response required from authors unless they wish to fold in the minor suggestions)
