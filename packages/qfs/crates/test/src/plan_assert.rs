//! Plan-shape assertions (t38, RFD §3/§6): `assert_plan(src, reg) -> PlanAssert` and the
//! fluent `.nodes(...)` / `.irreversible(n)` / `.no_io_performed()` / `.snapshot(name)`.
//!
//! **Plan assertion over mocking is the thesis** (RFD §3/§6): because a write statement
//! *evaluates to a pure `Plan`* rather than performing I/O, the strongest, fastest, most
//! deterministic test is to assert the plan itself — no fake, no socket, no creds. The plan
//! is pure data, so equality is the test. Reserve [`crate::fake`] for the COMMIT leg and
//! idempotency/recovery checks.
//!
//! ## How `assert_plan` gets a plan with no I/O
//! It runs the real pipeline `parse_statement → Evaluator::eval` against a caller-supplied
//! [`MountRegistry`] (the same seam the CLI and server use). `Evaluator::eval` resolves +
//! capability-gates, then folds the write side into a [`Plan`] — **constructing effects-as-
//! data, applying nothing**. The applier seam is never reached, so `no_io_performed()` is a
//! statement about a property the type system already guarantees and this asserts from the
//! test side: building the plan touched no World.
//!
//! ## Determinism (golden snapshots)
//! Before a `.snapshot(...)`, the plan's nodes are sorted into a **stable canonical order**
//! (by node id, the dense order the [`qfs_core::PlanBuilder`] allocates) and edges are sorted
//! lexicographically, so a DAG whose batching is unordered still serializes reproducibly.
//! [`crate::golden`] then redacts any non-deterministic field and sorts map keys.

use qfs_core::{EffectKind, EffectNode, EvalValue, Evaluator, MountRegistry, NodeId, Plan};
use qfs_parser::parse_statement;

/// The result of evaluating a statement to its effect [`Plan`] — the fluent assertion
/// surface. Construct via [`assert_plan`]. Each method returns `Self` (except the terminal
/// [`PlanAssert::snapshot`]) so assertions chain.
#[derive(Debug, Clone)]
pub struct PlanAssert {
    plan: Plan,
}

/// Evaluate `src` (a qfs statement) against `reg` to its effect [`Plan`] and return the
/// fluent [`PlanAssert`]. **No I/O, no creds, no socket** — the evaluator builds effects-as-
/// data; the applier seam is never reached.
///
/// # Panics
/// Panics (test-only) if the statement does not parse, does not resolve / capability-gate, or
/// evaluates to a pure read (a `PlanAssert` is meaningful only for an effect statement) — each
/// is a test-author error surfaced loudly.
#[must_use]
pub fn assert_plan(src: &str, reg: &MountRegistry) -> PlanAssert {
    let stmt = parse_statement(src)
        .unwrap_or_else(|e| panic!("qfs-test assert_plan: `{src}` did not parse: {e:?}"));
    let evaluator = Evaluator::new(reg);
    let value = evaluator
        .eval(&stmt)
        .unwrap_or_else(|e| panic!("qfs-test assert_plan: `{src}` did not evaluate: {e:?}"));
    match value {
        EvalValue::Plan(plan) => PlanAssert { plan },
        EvalValue::Relation(_) => {
            panic!("qfs-test assert_plan: `{src}` is a pure read, not an effect statement")
        }
    }
}

impl PlanAssert {
    /// The underlying owned [`Plan`] (for assertions the fluent surface does not cover).
    #[must_use]
    pub fn plan(&self) -> &Plan {
        &self.plan
    }

    /// Assert the **effect-DAG shape**: the plan's node kinds, in dense id order, equal
    /// `expected`. Matches on the closed [`EffectKind`] enum (mirrored from t09) so a test
    /// never depends on vendor specifics.
    ///
    /// # Panics
    /// Panics if the node count or any kind differs from `expected`.
    #[must_use]
    pub fn nodes(self, expected: &[EffectKind]) -> Self {
        let mut nodes: Vec<&EffectNode> = self.plan.nodes().iter().collect();
        nodes.sort_by_key(|n| n.id.0);
        let actual: Vec<EffectKind> = nodes.iter().map(|n| n.kind.clone()).collect();
        assert!(
            actual == expected,
            "plan node-kind shape mismatch:\n  expected: {expected:?}\n  actual:   {actual:?}"
        );
        self
    }

    /// Assert the number of **irreversible** nodes (RFD §6 safety surface) equals `count`.
    ///
    /// # Panics
    /// Panics if the irreversible-node count differs.
    #[must_use]
    pub fn irreversible(self, count: usize) -> Self {
        let actual = self.plan.nodes().iter().filter(|n| n.irreversible).count();
        assert!(
            actual == count,
            "irreversible-node count mismatch: expected {count}, got {actual}"
        );
        self
    }

    /// Assert that **no I/O was performed** building this plan (RFD §3 purity invariant from
    /// the test side). `assert_plan` reached the plan via `Evaluator::eval`, which never calls
    /// the applier seam — so the World is untouched by construction. This method documents and
    /// pins that property: a plan exists, and it is the *only* thing that exists (no committed
    /// effect, no recorded apply). It also validates the DAG invariant, since a malformed plan
    /// would be the only way a "pure build" could have gone wrong.
    ///
    /// # Panics
    /// Panics if the plan violates a DAG invariant (a construction bug — the only failure mode
    /// reachable on the pure path).
    #[must_use]
    pub fn no_io_performed(self) -> Self {
        self.plan
            .validate()
            .unwrap_or_else(|e| panic!("qfs-test no_io_performed: plan is not a valid DAG: {e}"));
        self
    }

    /// Look up a node by id (escape hatch for fine-grained assertions).
    #[must_use]
    pub fn node(&self, id: NodeId) -> Option<&EffectNode> {
        self.plan.node(id)
    }

    /// Terminal: snapshot the plan as a **golden** (canonical JSON of the owned `Plan` DTO).
    /// Nodes are sorted into dense-id order and edges lexicographically *before* serialization
    /// so the DAG's unordered batching does not flap the golden; [`crate::golden`] then sorts
    /// map keys and redacts non-deterministic fields. Compares against (or, under `QFS_BLESS=1`,
    /// writes) `tests/fixtures/<name>.json`, and scrubs the rendering for credential shapes.
    ///
    /// # Panics
    /// Panics on a golden mismatch or a credential-shape leak (see [`crate::golden`]).
    pub fn snapshot(self, name: &str) {
        let canon = canonicalize_plan(self.plan);
        let rendered = crate::golden::canonical_json(&canon);
        crate::golden::assert_no_credential_shape(&rendered);
        crate::golden::assert_golden(name, &canon);
    }
}

/// Return a `Plan` with a **stable canonical node + edge order** (RFD §6 determinism): nodes
/// sorted by dense id, edges sorted lexicographically. The plan's *meaning* is order-
/// independent (a DAG), so this normalization is sound and makes the golden reproducible.
fn canonicalize_plan(mut plan: Plan) -> Plan {
    plan.nodes.sort_by_key(|n| n.id.0);
    plan.deps.sort_by_key(|a| (a.0 .0, a.1 .0));
    plan
}
