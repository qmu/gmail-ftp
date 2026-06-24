# ADR 0006 — Test harness: in-house golden / property / mock-HTTP over insta / proptest / httptest

- **Status**: Accepted (locked)
- **Date**: 2026-06-24
- **Deciders**: qfs-foundation-e0 trip team (Constructor authored; Architect/Planner review)
- **Ticket**: t38 — Test harness: per-driver fakes, plan assertions, golden tests. The cross-cutting
  `qfs-test` dev-only crate that lets every other epic be proven correct offline, in CI, and in `wasm32`.
- **Supersedes / superseded by**: none
- **References**: RFD-0001 §3 (purity invariant — "everything dry-runnable, testable, composable"),
  §5 (structured AI-facing errors must serialize stably), §6 (PREVIEW/COMMIT, at-least-once /
  idempotency / recovery), §9 (owned DTOs — vendor types never leak; lean single binary + `wasm32`),
  §10 (least-privilege; dry-run in CI; no token in a preview). ADR-0001 / ADR-0002 / ADR-0003 /
  ADR-0004 / ADR-0005 — the **same** footprint / offline-cache / wasm-buildability decision shape
  (winnow over chumsky, in-house combine engine over DuckDB, in-house git reader over gix, in-house
  HTTP/1.1 over axum, in-house host adapter; each chose a dependency-light in-house path over an
  uncached/heavy vendor crate).

## Context

The eleven trip tickets each grew an **ad-hoc** harness for the same recurring needs:
MockHttp-style scripted transports, plan-shape assertions, canonical-JSON golden compares, fake
`ReadDriver`/`ApplyDriver`s, and apply-twice idempotency checks. t38 institutionalizes the common
patterns into one `qfs-test` dev-only crate. The ticket *suggested* `insta` (snapshots),
`proptest` (property tests), and `httptest`/`wiremock` (mock HTTP).

Two hard constraints govern the choice:

1. **Offline cargo cache + tight disk.** The build runs against a pre-warmed offline cache on a
   disk at ~100% (≈400–550 MB free). `insta` (and its `pest`/`ron`/`similar`/`console` support
   tree), `proptest`, `httptest`, and `wiremock` are **all absent from the offline cache** —
   verified by probing `~/.cargo/registry/cache`. Pulling them is unaffordable and would also
   contradict the whole trip, which hand-rolled to avoid uncached deps.
2. **No-socket, wasm-pure harness.** `httptest`/`wiremock` bind a **real loopback listener**, so
   they are unusable in the offline/no-socket harness and cannot build for
   `wasm32-unknown-unknown` (the pure parse/plan/codec subset must run on wasm32, RFD §1/§9).

## Decision

**Build dependency-light in-house equivalents, consistent with ADR-0001..0005.** `qfs-test`
depends only on the pure spine (`qfs-core`, `qfs-parser`, `qfs-http-core`) plus `serde`/
`serde_json` — no new uncached dependency, no socket, no async runtime.

1. **Golden / snapshot = canonical JSON, not `insta`.** A golden serializes an owned DTO
   (`Plan`/`Statement` AST/`ParseError` view) to **canonical JSON** — recursively sorted map
   keys, DAG-node + edge order normalized, non-deterministic fields (`timestamp`/`ts`/
   `request_id`/`run_id`/`updated_at`/…) redacted — and compares it against a checked-in
   `tests/fixtures/*.json`. The update path is a cargo-native **`QFS_BLESS=1 cargo test`** env
   gate (documented in the crate README), **not** `cargo insta review`. A credential-shape scrub
   runs on every rendered golden so a token can never enter a fixture (RFD §10).

2. **Property tests = a seeded in-house corpus, not `proptest`.** Codec round-trips
   (`DECODE∘ENCODE == identity`, proven on **rows** under a re-encode cycle so non-byte-preserving
   codecs still round-trip) run over a small **deterministic, example-based** input corpus —
   one representative input per shape per format — instead of an uncached property-test
   framework. This is exactly what the driver tickets already did.

3. **MockHttp = a scripted in-memory transport, not `httptest`/`wiremock`.** A `RefCell`-backed
   FIFO of canned responses + a recorded-request log, built on the **pure** `qfs-http-core` DTOs
   (`HttpMethod`/`HttpRequest`/`HttpResponse` + the redacting `Debug`). Wasm-clean (no socket, no
   threads, no reqwest). The real reqwest `HttpClient` stays in `qfs-driver-http`, never pulled
   into the harness. This is the same recording/scripted pattern `qfs-driver-http::MockHttpClient`
   already established.

4. **Fakes reuse the existing apply seam.** `FakeBackend` **is** a `qfs_core::PlanApplier` — the
   exact trait the runtime interpreter (`commit`) drives — so a fake exercises the production
   COMMIT path with an in-memory `FakeWorld` (rows-per-path) and no creds. No parallel apply
   abstraction is invented.

## Consequences

- **Positive.** No new uncached/heavy dependency; the harness builds on the tight disk and on
  `wasm32-unknown-unknown` (pure subset verified once). Goldens are deterministic by construction
  (sorted keys + normalized DAG order + redaction), so they do not flap. The bless workflow is one
  obvious env-gated command. The dev-only boundary is mechanically enforced (a `cargo metadata`
  dep-graph test proves the `qfs` binary never links `qfs-test`).
- **Negative / trade-offs.** (a) No interactive snapshot review UI (`cargo insta review`); the
  diff is shown in the panic message and re-blessed via `QFS_BLESS=1` — acceptable, and the diff is
  plain JSON. (b) Example-based corpora are not exhaustive shrinking property tests; the corpus is
  curated to cover each codec's shapes, and a future E8 hardening ticket may add fuzzing if a real
  property-test framework lands in the cache. (c) `MockHttp` is single-threaded (`RefCell`) so it is
  not `Send`; the pure helpers never need `Send`, and a `Send` mock remains available in
  `qfs-driver-http` for the runtime's blocking apply threads.
- **Reversibility.** If `insta`/`proptest`/`httptest` later enter the offline cache, the canonical-
  JSON goldens, the corpus, and the scripted transport can each be swapped behind their existing
  helper signatures (`assert_golden`/`roundtrip_codec`/`MockHttp`) without touching call sites — the
  decision is feature-local, like the trait-gated engine/parser choices in ADR-0001/0002.

## Related guard (carried, not a new decision)

t38 also folds in the carried **wasm-gating** mechanical guard (the precedent since t25/t33): a
`cargo metadata`-based dep test asserts each wasm-gated leaf (`qfs-cron`/`qfs-watchtower`/`qfs-host`/
`qfs-driver-slack`) builds its pure core **without tokio** under `--no-default-features` — the same
shape as `crates/plan/tests/purity_deps.rs`. The wasm fence is thus mechanically enforced, not
conventional. See `crates/test/tests/wasm_gating.rs`.
