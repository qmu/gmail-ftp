# qfs-test — the offline test harness

`qfs-test` is the **dev-dependency-only** support crate (t38) that institutionalizes the
harness patterns the eleven `qfs-foundation-e0` trip tickets each grew ad-hoc into one
authority. It is **offline, no-creds, no-socket**: every other epic is proven correct without
a live backend, in CI, and on `wasm32`. The shipped `qfs` binary never links it (mechanically
proved by `tests/dev_only_dep_graph.rs`).

## The thesis: assert the plan, not the side effect

Because the query side is pure and write operators **evaluate to a `Plan`** rather than
performing I/O (RFD §3/§6), a statement can be asserted **against the plan it produces** with
no backend, no creds, and no socket. Lead with that:

```rust
use qfs_test::assert_plan;
use qfs_core::EffectKind;

assert_plan("UPSERT INTO /db/users VALUES (1, 'a', true)", &registry)
    .nodes(&[EffectKind::Upsert])   // effect-DAG shape
    .irreversible(0)                // §6 safety surface
    .no_io_performed();             // World untouched after eval
```

Reserve the fake backend for the COMMIT leg and idempotency/recovery checks.

## What it provides

| Helper | Purpose |
| ------ | ------- |
| `assert_plan(src, reg) -> PlanAssert` | Evaluate a statement to its effect DAG; assert `.nodes`, `.irreversible(n)`, `.no_io_performed()`, `.snapshot(name)`. |
| `FakeBackend` + `FakeWorld` | In-memory `PlanApplier` (the **existing** apply seam) for post-COMMIT state + apply-twice idempotency. |
| `MockHttp` | Wasm-clean **scripted** transport (request → canned response, recorded), built on the pure `qfs-http-core` DTOs. |
| `NoCreds` | No-token credential source — a green test wired to it provably used no secret (RFD §10). |
| `golden_parse(src) -> AstSnapshot` / `error_snapshot(src)` | Parser/grammar golden to a STABLE AST + a stable parse-error-recovery message. |
| `roundtrip_codec(fmt, bytes) -> RoundTrip` | `DECODE∘ENCODE == identity` over an input `corpus()`. |
| `preview_handler(ddl_src) -> Plan` | Drive a `CREATE ENDPOINT/TRIGGER/JOB` to the `Plan` it would COMMIT — no socket, no backend. |
| `golden::*` | Canonical-JSON serializer (deterministic key + DAG-node ordering + redacted non-deterministic fields), the `QFS_BLESS=1` bless workflow, and `assert_no_credential_shape`. |
| `assert_pure(closure)` | The no-network guard hook — "assert the plan, not the side effect." |

## Golden / snapshot workflow (cargo-native, NOT `cargo insta review`)

Goldens are **canonical JSON** of an owned DTO compared against a checked-in
`tests/fixtures/<name>.json`. Determinism is enforced before comparison:

1. DAG nodes are sorted into dense-id order and edges lexicographically (a plan is an unordered
   DAG — `crate::plan_assert` normalizes it),
2. every object's map keys are sorted,
3. non-deterministic fields (`timestamp`/`ts`/`request_id`/`run_id`/`updated_at`/…) are
   **redacted** to `<redacted>`,
4. the result is pretty-printed with a trailing newline.

To **(re-)bless** a fixture after an intended change, set the env var and run the tests:

```sh
QFS_BLESS=1 cargo test -p qfs-test
```

With `QFS_BLESS` unset (the CI default) a drift is a hard failure with a readable diff hint.
Every golden is also scrubbed for **credential shapes** (`Bearer `, `ya29.`, `AKIA`, `xoxb-`,
`-----BEGIN`, …): a token shape in an owned-DTO snapshot is a failed review, caught
mechanically (RFD §10).

## Why in-house, not insta / proptest / httptest

`insta` (+ its `pest`/`ron`/`similar`/`console` tree), `proptest`, and `httptest`/`wiremock`
are **absent from the offline cargo cache** and unaffordable on the tight disk; httptest /
wiremock are also socket-bound (they bind a real loopback listener), unusable in a no-socket,
wasm-pure harness. So the harness builds dependency-light equivalents — canonical-JSON goldens
(not insta), a seeded example `corpus()` (not proptest), a scripted in-memory `MockHttp` (not
httptest) — consistent with ADR-0001/0002/0003/0004/0005. See
[`docs/adr/0006-test-harness.md`](../../docs/adr/0006-test-harness.md).

## wasm32 parity

The pure half — parse, plan, codec round-trip, the scripted `MockHttp` — avoids `std::net` and
threads and builds for `wasm32-unknown-unknown`. The `std::fs` bless path is reached only under
the native `QFS_BLESS` run; the helper *surface* a wasm consumer calls is socket-free and
thread-free.

## Scope

t38 **consolidates** — it extracts the reusable helpers, proves them with one representative
test per category (`tests/harness_demo.rs`), and seeds canonical fixtures. It deliberately does
**not** migrate the existing suites (that churn is out of scope and risks regressions). Live
integration against real Gmail/Drive/S3 is **deferred to a future E8 live-smoke ticket** gated
behind opt-in creds; `qfs-test` is the offline harness.
