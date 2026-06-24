//! The **policy / access-control engine** (RFD-0001 §8 Server / §10 Security) — t35.
//!
//! Per handler (endpoint / trigger / job / webhook), a [`Policy`] declares the
//! least-privilege set of `(verb, driver)` pairs that handler's COMMIT plan may touch. This is
//! the **may** layer that sits over t13's **can** layer (driver capability): enforcement is
//! `can ∧ may`, kept legibly separate (a capability error and a policy denial never
//! masquerade as each other).
//!
//! ## The single most important behavior: default-deny / fail-closed
//! A handler with no policy, and an empty policy, **deny every effect** in the COMMIT plan
//! ([`Policy::default`] is `default: Effectivity::Deny` with no rules). A policy only *widens*
//! the closed default via explicit `ALLOW` rules. [`enforce::evaluate`] is the pure classifier.
//!
//! ## Layout
//! - [`model`] — the owned DTOs (`Verb`, `Effectivity`, `VerbSet`, `DriverGlob`, `Rule`,
//!   `Policy`). No vendor leak; serde round-trips through `/server/policies`.
//! - [`grammar`] — build a `Policy` from the parsed `CREATE POLICY` DDL + round-trip through a
//!   [`crate::PolicyDef`] row (the `ALLOW`/`DENY` token parsing is in `qfs-parser`, no new
//!   frozen keyword — see `grammar`).
//! - [`enforce`] — the PURE `evaluate(policy, plan) -> PolicyDecision` walk (no I/O, no
//!   mutation; the purity invariant, so PREVIEW-as-CI surfaces denials with no live creds).
//! - [`audit`] — the [`audit::FiredPlanRecord`] emitted for EVERY fired plan (allow + deny).

pub mod audit;
pub mod enforce;
pub mod gate;
pub mod grammar;
pub mod model;

pub use audit::{FiredDecision, FiredPlanRecord};
pub use enforce::{classify_effect, evaluate, verb_for_effect, EffectClass, PolicyDecision};
pub use gate::{effect_summaries, gate_plan, resolve_policy, GateOutcome, PolicyTable};
pub use grammar::{policy_from_ddl, policy_from_def, policy_to_rule_strings, rule_to_string};
pub use model::{DriverGlob, Effectivity, Policy, Rule, Verb, VerbSet};
