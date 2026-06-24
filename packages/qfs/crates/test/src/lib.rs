//! `qfs-test` — the offline, no-creds, no-socket test harness (t38, RFD §3/§5/§6/§9/§10).
//!
//! This is the **dev-only** support crate that institutionalizes the harness patterns the
//! eleven trip tickets each grew ad-hoc into one authority. It is a **dependency-only** crate:
//! the shipped `qfs` binary never links it (proved mechanically by
//! `tests/dev_only_dep_graph.rs`). Nothing here performs live I/O — the whole point is that
//! every other epic is proven correct *offline*, in CI, and on `wasm32`.
//!
//! ## The thesis: assert the plan, not the side effect (RFD §3/§6)
//! Because the query side is pure and write operators *evaluate to a [`qfs_core::Plan`]*
//! rather than performing I/O, a statement can be asserted **against the plan it produces**
//! without ever touching a backend. The harness leads with that ([`assert_plan`]) and reserves
//! the fake backend ([`FakeBackend`]) for the COMMIT leg and idempotency/recovery checks.
//!
//! ## What it provides
//! - [`assert_plan`] / [`PlanAssert`] — evaluate a statement to its effect DAG and assert the
//!   shape (`.nodes`), the irreversible count (`.irreversible`), I/O-freedom (`.no_io_performed`),
//!   and a golden (`.snapshot`). Plan is pure data → equality is the test.
//! - [`FakeBackend`] + [`FakeWorld`] — an in-memory [`qfs_core::PlanApplier`] (the *existing*
//!   apply seam, not a parallel one) for post-COMMIT state + apply-twice idempotency.
//! - [`MockHttp`] (wasm-clean scripted transport) + [`NoCreds`] (no-token credential source).
//! - [`golden_parse`] / [`AstSnapshot`] + [`error_snapshot`] — parser/grammar goldens to a
//!   STABLE AST, plus a stable parse-error-recovery message.
//! - [`roundtrip_codec`] / [`RoundTrip`] — `DECODE∘ENCODE == identity` over an input corpus.
//! - [`preview_handler`] — drive a `CREATE ENDPOINT/TRIGGER/JOB` to the `Plan` it would COMMIT
//!   (no socket, no backend).
//! - [`golden`] — the canonical-JSON serializer (deterministic key + DAG-node ordering +
//!   redacted non-deterministic fields), the cargo-native `QFS_BLESS=1` bless workflow, and the
//!   [`golden::assert_no_credential_shape`] scrub.
//! - [`assert_pure`] — the no-network guard hook ("assert the plan, not the side effect").
//!
//! ## Why in-house, not insta/proptest/httptest (ADR-0006)
//! Those crates (and their support trees) are absent from the offline cargo cache and would be
//! unaffordable on the tight disk; httptest/wiremock are also socket-bound. The harness builds
//! dependency-light equivalents — canonical-JSON goldens (not insta), a seeded example corpus
//! (not proptest), a scripted in-memory transport (not httptest) — consistent with
//! ADR-0001/0002/0003/0004/0005. See `docs/adr/0006-test-harness.md`.
//!
//! ## wasm32 parity (RFD §1/§9)
//! The pure half — parse, plan, codec round-trip, the scripted [`MockHttp`] — avoids
//! `std::net`/threads and builds for `wasm32-unknown-unknown`. `serde_json`'s std-fs-touching
//! bless path ([`golden::assert_golden`]) is only reached under `#[cfg(test)]` on a native
//! target; the helper *surface* the wasm consumer calls (`canonical_json`, `roundtrip_codec`,
//! `golden_parse`, `assert_plan`) is socket-free and thread-free.

// The harness's job is to FAIL a test loudly on a mismatch; the assertion helpers panic by
// design (a golden drift, an unparseable fixture, a non-effect statement). The strict
// workspace lint forbids panic in library code, so the crate-level allow is intentional and
// documented: qfs-test is dev-only test-support, never linked into the shipped binary.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

pub mod golden;

mod codec_rt;
mod fake;
mod handler;
mod mock_http;
mod no_network;
mod parse_golden;
mod plan_assert;

pub use codec_rt::{corpus, roundtrip_codec, RoundTrip};
pub use fake::{FakeBackend, FakeWorld, NoCreds};
pub use handler::preview_handler;
pub use mock_http::MockHttp;
pub use no_network::assert_pure;
pub use parse_golden::{error_snapshot, golden_parse, AstSnapshot, ParseErrorSnapshot};
pub use plan_assert::{assert_plan, PlanAssert};
