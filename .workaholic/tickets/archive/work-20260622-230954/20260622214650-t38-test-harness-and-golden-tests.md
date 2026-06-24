---
created_at: 2026-06-22T21:46:50+09:00
author: a@qmu.jp
type: enhancement
layer: [Infrastructure]
effort:
commit_hash: b00a9bb
category: Added
depends_on: [20260622214650-t13-driver-contract-trait.md]
---

# Test harness: per-driver fakes, plan assertions, golden tests

## Overview

This ticket delivers the **no-live-creds test harness** that lets every other epic be
proven correct offline, in CI, and in `wasm32`. It is the cross-cutting payoff of the
core design decisions in **RFD §3 (purity invariant)** and **RFD §6 (PREVIEW/COMMIT)**:
because the query side is pure and write operators *evaluate to a `Plan`* rather than
performing I/O, a statement can be asserted **against the plan it produces** without ever
touching a real backend. The harness institutionalizes that: assert plans (pure →
trivially testable), run drivers against **per-driver fakes / `httptest` backends**
instead of live tokens, and lock the parser/grammar and codecs behind **golden tests**.

It implements the testability promises scattered across the RFD: §3 "keeps everything
dry-runnable, testable, and composable"; §5 capability errors are "structured — important
for AI" (so they must serialize stably); §6 "`PREVIEW`-as-CI-test"; §9 "owned DTOs —
vendor types never leak" (codec round-trips and DTO snapshots are how we police that); and
§10 "dry-run in CI". It builds directly on the `Driver`/`FixtureDriver` seam from t13.

## Scope

In-scope:
- A `qfs-test` support crate: plan-assertion helpers, golden/snapshot infrastructure,
  an `httptest`-style mock HTTP backend, and a fake-credential injector.
- **Plan assertions** — normalize and compare an evaluated `Plan` (typed effect DAG)
  structurally against an expected shape, with `irreversible`/dependency edges asserted.
- **Per-driver fakes** — an in-memory backing store + recorded HTTP transcripts so a
  real driver (E4) can be exercised end-to-end (DESCRIBE → plan → COMMIT) with no creds.
- **Golden tests for parser/grammar** — input `.qfs` → serialized AST snapshot; covers
  closed-core keywords and parse-error recovery messages.
- **Codec round-trip tests** — `DECODE fmt` then `ENCODE fmt` (and reverse) is identity
  for `json/yaml/toml/csv/markdown+frontmatter`, plus rows↔bytes property tests.
- **Handler PREVIEW fixtures** — server `ENDPOINT/TRIGGER/JOB` handlers asserted at the
  plan level (the plan a fired binding would COMMIT), no socket, no live backend.
- Shared snapshot review workflow (insta-style) and a no-network test guard.

Out-of-scope (deferred):
- The concrete drivers, codecs, parser, evaluator, and server bindings themselves — owned
  by **t04/t07/t13/t15/t30/t31** etc.; this ticket provides the *harness* they test with,
  and seeds canonical fixtures, not the production code.
- Live integration / contract tests against real Gmail/Drive/S3 — **deferred to a future
  E8 "live smoke" ticket** gated behind opt-in creds; here everything is offline.
- Benchmarks / perf and fuzzing of the parser — sibling E8 hardening ticket.
- Coverage gating thresholds in CI config — t01 workspace / CI ticket.

## Key components

New crate `qfs-test` (dev-dependency only; never linked into the shipped binary). No
vendor SDKs; it trades only in owned DTOs (`Plan`, `Schema`, `Row`, AST) so the
purity/no-leak invariants are testable from outside the boundary.

```rust
// Plan assertions — Plan is pure data (t09), so equality is the test.
pub fn assert_plan(stmt: &str, reg: &DriverRegistry) -> PlanAssert;
pub struct PlanAssert { plan: Plan }
impl PlanAssert {
    pub fn nodes(self, expected: &[EffectKind]) -> Self;   // shape of the effect DAG
    pub fn irreversible(self, count: usize) -> Self;       // §6 safety surface
    pub fn no_io_performed(self) -> Self;                  // World untouched after eval
    pub fn snapshot(self, name: &str);                     // golden Plan (serde)
}

// Per-driver fake: in-memory store + recorded HTTP transcript, capability-faithful.
pub trait FakeBackend: PlanApplier {
    fn seed(&mut self, path: &VfsPath, rows: Vec<Row>);
    fn world(&self) -> &FakeWorld;                         // assert post-COMMIT state
}
pub struct MockHttp { /* httptest-style: match request → canned response */ }
pub struct NoCreds;   // injected where a real CredentialStore would be (E5) — proves no token use

// Golden helpers (insta wrappers, stable serde of owned DTOs).
pub fn golden_parse(src: &str) -> AstSnapshot;             // .qfs → AST
pub fn roundtrip_codec(fmt: Codec, bytes: &[u8]) -> RoundTrip;  // DECODE∘ENCODE == id
pub fn preview_handler(handler: &ServerHandler, evt: Event) -> Plan; // §8 PREVIEW-as-test
```

- `EffectKind` — the closed enum of plan-node kinds (`Insert/Upsert/Update/Remove/Call/Cp/Mv/Rm`)
  mirrored from t09; assertions match on it so tests never depend on vendor specifics.
- `FakeWorld` — owned snapshotable state (rows per path) for `world()` post-COMMIT checks
  and idempotency (apply-twice) assertions.
- A `#[cfg(test)]` **no-network guard** (e.g. block the resolver/socket) so a pure method
  that accidentally does I/O fails loudly — enforces RFD §3 purity from the test side.

## Implementation steps

1. Scaffold `qfs-test` as a workspace dev crate; pull in `insta` (snapshots) and an
   `httptest`/wiremock-style mock; ensure it is **not** a runtime dep of `qfs`.
2. Implement `assert_plan` over the evaluator (t07) + `DriverRegistry` (t13): evaluate a
   statement to a `Plan` *without* COMMIT, expose `nodes/irreversible/no_io_performed`.
3. Add stable `serde` snapshotting of `Plan`/AST (deterministic ordering of DAG nodes and
   map keys) so golden files are reproducible across runs/platforms.
4. Build `FakeBackend`/`FakeWorld` on top of t13's `FixtureDriver` and `PlanApplier`;
   add `MockHttp` and `NoCreds` for HTTP-archetype drivers.
5. Write `golden_parse` and the parser/grammar golden corpus (closed-core keywords, pipe
   `|>`, `CALL`, codecs, plus a handful of **error-recovery** cases asserting the message).
6. Write `roundtrip_codec` + property tests (proptest) for each codec, incl. the
   markdown+frontmatter ↔ row mapping (frontmatter keys = columns, `body` = content).
7. Implement `preview_handler`: drive a `CREATE ENDPOINT/TRIGGER/JOB` handler with a
   fixture event and assert the resulting `Plan` (no listen socket, no live backend).
8. Add the no-network test guard and a doc note: "assert the plan, not the side effect."
9. Seed canonical fixtures (a multi-archetype fake, a recorded HTTP transcript, sample
   `.qfs` programs) reused by E1/E2/E3/E4/E7 tickets.

## Considerations

- **Plan assertion over mocking is the thesis (RFD §3/§6):** prefer asserting the evaluated
  `Plan` to driving a fake and inspecting side effects — it is faster, deterministic, and
  proves purity. Reserve `FakeBackend` for the COMMIT leg and idempotency/recovery checks.
- **Determinism is the hard part:** the plan is a *DAG* and batching is unordered (Haxl-style,
  RFD §6); golden snapshots must canonicalize node ordering, map-key ordering, and any
  generated ids, or goldens flap. Resolve with a stable topological normalization before
  serialization, and redact non-deterministic fields (timestamps, request ids).
- **Least-privilege & secrets (RFD §10):** the harness injects `NoCreds` and runs under the
  no-network guard, so a test that "passes" provably used **no token and no socket**;
  credential-shaped strings must never appear in golden files (add a scrub assertion).
- **Idempotency / recovery (RFD §6):** include an apply-twice test helper proving `UPSERT`
  plans converge, and a `cp = copy→verify→delete` partial-failure fixture asserting the
  audit ledger reconstructs state — these are exactly the irreversible/recoverable seams
  where PREVIEW earns its keep, so they must be harness-supported from day one.
- **Owned DTOs / no vendor leak (RFD §9):** codec round-trip and DTO snapshots double as
  the enforcement that vendor types stay behind the driver boundary — snapshots are of
  owned `Row`/`Schema`/`Plan` only; a vendor type in a snapshot is a failed review.
- **Capability errors are AI-facing (RFD §5):** snapshot the *structured* `CapabilityError`
  so its serialized shape (verb, path, supported set) is locked — AI consumers depend on it.
- **wasm32 parity:** the pure half (parse, plan, codec round-trip) must run in the `wasm32`
  test target too, so the harness's pure helpers avoid `std::net`/threads.
- **Coding standards / layout:** `qfs-test` under the workspace as a dev-only crate;
  goldens under `tests/snapshots/` per crate; fixtures under `tests/fixtures/`; one
  obvious `cargo insta review` workflow documented in the crate README.

## Acceptance criteria

- `cargo build`, `cargo test`, and `cargo clippy -- -D warnings` green across the
  workspace; the pure test subset also passes on `wasm32-unknown-unknown`.
- `qfs-test` is a **dev-dependency only** (dep-graph assertion: the shipped `qfs` binary
  does not link it).
- **Plan assertion:** evaluating a representative write statement (e.g.
  `... |> UPSERT INTO /fake/table ...`) yields the expected effect-DAG shape and
  `irreversible` count, and `no_io_performed()` holds — **no live creds, no socket**.
- **Parser golden tests:** a corpus of `.qfs` inputs (closed-core keywords, `|>`, `CALL`,
  `DECODE/ENCODE`, server `CREATE …`) snapshots to stable AST; at least one parse-error
  case asserts a stable recovery message.
- **Codec round-trip:** `DECODE` then `ENCODE` is identity for each of
  `json/yaml/toml/csv/markdown+frontmatter` over a property-test corpus.
- **Handler PREVIEW fixture:** firing a fixture event at a `TRIGGER`/`JOB`/`ENDPOINT`
  handler asserts the produced `Plan` without opening a socket or hitting a backend.
- **No-creds/no-network guard:** a test that performs unexpected I/O fails; goldens contain
  no credential-shaped strings (scrub assertion passes).
- **Idempotency:** an apply-twice `UPSERT` test converges, proving retry-safety via the
  `FakeBackend`/`FakeWorld` state assertion.
