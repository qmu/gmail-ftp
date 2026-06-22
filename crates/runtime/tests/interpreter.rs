//! Interpreter integration tests (t10 acceptance criteria). All tests use an in-memory
//! mock [`ApplyDriver`] — **no live credentials, no network**. The mock records batch group
//! sizes and call counts, can be configured to fail specific nodes, and (for the concurrency
//! test) tracks how many `apply_batch` calls are in flight at once, so parallelism is
//! observable and deterministic.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use cfs_plan::{depends_on, EffectKind, EffectNode, NodeId, Plan, ProcId, Target, VfsPath};
use cfs_runtime::{
    ApplyCx, ApplyDriver, CapabilitySet, ConcurrencyLimits, EffectError, EffectInput, EffectOutput,
    Interpreter, LegStatus, RetryPolicy,
};
use cfs_types::DriverId;

/// A controllable in-memory mock driver. Records every batch call (its size) and the per-leg
/// behaviour, with optional fail-injection, retry-until-success, and concurrency tracking.
#[derive(Default)]
struct MockDriver {
    /// Recorded batch sizes, one entry per `apply_batch` call (the N+1 → 1 evidence).
    batch_sizes: Mutex<Vec<usize>>,
    /// Total `apply_batch` calls.
    calls: AtomicUsize,
    /// Node ids that should fail with a *retryable* error every time.
    fail_retryable: Vec<NodeId>,
    /// Node ids that should fail *terminally*.
    fail_terminal: Vec<NodeId>,
    /// Node ids that fail the first `flaky_until` attempts, then succeed (per-id attempt count).
    flaky: Mutex<std::collections::HashMap<NodeId, u32>>,
    /// How many attempts a flaky node fails before succeeding.
    flaky_until: u32,
    /// Concurrency tracking: current in-flight calls and the observed maximum.
    in_flight: AtomicUsize,
    max_in_flight: AtomicUsize,
    /// If set, each call sleeps this long so overlap is observable.
    hold: Option<Duration>,
}

impl MockDriver {
    fn new() -> Self {
        Self::default()
    }

    fn failing_retryable(mut self, id: NodeId) -> Self {
        self.fail_retryable.push(id);
        self
    }

    fn failing_terminal(mut self, id: NodeId) -> Self {
        self.fail_terminal.push(id);
        self
    }

    fn flaky(mut self, id: NodeId, until: u32) -> Self {
        self.flaky.lock().unwrap().insert(id, 0);
        self.flaky_until = until;
        self
    }

    fn holding(mut self, d: Duration) -> Self {
        self.hold = Some(d);
        self
    }

    fn call_count(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    fn recorded_sizes(&self) -> Vec<usize> {
        self.batch_sizes.lock().unwrap().clone()
    }

    fn peak_in_flight(&self) -> usize {
        self.max_in_flight.load(Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl ApplyDriver for MockDriver {
    async fn apply_batch(
        &self,
        _kind: EffectKind,
        effects: &[EffectInput],
        _cx: &ApplyCx,
    ) -> Vec<Result<EffectOutput, EffectError>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.batch_sizes.lock().unwrap().push(effects.len());

        // Concurrency tracking: bump in-flight, record peak, hold, then drop.
        let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_in_flight.fetch_max(now, Ordering::SeqCst);
        if let Some(d) = self.hold {
            tokio::time::sleep(d).await;
        }
        let out: Vec<_> = effects.iter().map(|e| self.result_for(e)).collect();
        self.in_flight.fetch_sub(1, Ordering::SeqCst);
        out
    }

    async fn apply_one(
        &self,
        effect: &EffectInput,
        _cx: &ApplyCx,
    ) -> Result<EffectOutput, EffectError> {
        self.result_for(effect)
    }
}

impl MockDriver {
    fn result_for(&self, e: &EffectInput) -> Result<EffectOutput, EffectError> {
        if self.fail_terminal.contains(&e.id) {
            return Err(EffectError::terminal("mock terminal failure"));
        }
        if self.fail_retryable.contains(&e.id) {
            return Err(EffectError::retryable("mock retryable failure"));
        }
        let mut flaky = self.flaky.lock().unwrap();
        if let Some(attempts) = flaky.get_mut(&e.id) {
            *attempts += 1;
            if *attempts <= self.flaky_until {
                return Err(EffectError::retryable("mock flaky failure"));
            }
        }
        Ok(EffectOutput::new(e.id, 1))
    }
}

fn driver_id() -> DriverId {
    DriverId::new("mock")
}

fn node(id: u32, kind: EffectKind) -> EffectNode {
    EffectNode::new(
        NodeId(id),
        kind,
        Target::new(driver_id(), VfsPath::new(format!("/mock/{id}"))),
    )
}

fn allow_all() -> CapabilitySet {
    CapabilitySet::allow_all()
}

fn registry(driver: Arc<MockDriver>) -> cfs_runtime::DriverRegistry {
    cfs_runtime::DriverRegistry::new().with(driver_id(), driver)
}

/// A node whose target driver is `driver-{d}` (distinct per group), with kind INSERT. Used by
/// the wide-frontier admission test so every group is its OWN `(driver,kind)` key — they never
/// coalesce and each gets its own per-driver semaphore, so the only cap that can bind is the
/// global one. That makes the mock's peak concurrent `apply_batch` count a faithful proxy for
/// the peak number of *admitted* group-futures (each admitted future holds a global permit for
/// its whole lifetime, including the driver call).
fn node_on(driver: &str, id: u32) -> EffectNode {
    EffectNode::new(
        NodeId(id),
        EffectKind::Insert,
        Target::new(
            DriverId::new(driver),
            VfsPath::new(format!("/{driver}/{id}")),
        ),
    )
}

// ---------------------------------------------------------------------------
// Batching: N independent same-(driver,kind) effects → ONE apply_batch call.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn n_independent_same_kind_effects_coalesce_into_one_batch() {
    let mock = Arc::new(MockDriver::new());
    // 5 independent INSERTs to the same driver, same kind — no deps between them.
    let mut plan = Plan::pure();
    for i in 0..5 {
        plan = plan.merge(Plan::leaf(node(i, EffectKind::Insert)));
    }
    let interp = Interpreter::with_defaults(registry(mock.clone()));
    let outcome = interp.commit(plan, &allow_all()).await.unwrap();

    // The batching assertion: ONE call, with all 5 effects (N+1 → 1).
    assert_eq!(
        mock.call_count(),
        1,
        "expected a single coalesced batch call"
    );
    assert_eq!(mock.recorded_sizes(), vec![5]);
    assert!(outcome.is_complete());
    assert_eq!(outcome.applied_ids().len(), 5);
}

#[tokio::test]
async fn different_kinds_do_not_coalesce() {
    let mock = Arc::new(MockDriver::new());
    let plan = Plan::leaf(node(0, EffectKind::Insert))
        .merge(Plan::leaf(node(1, EffectKind::Insert)))
        .merge(Plan::leaf(node(2, EffectKind::Update)));
    let interp = Interpreter::with_defaults(registry(mock.clone()));
    let outcome = interp.commit(plan, &allow_all()).await.unwrap();

    // Two INSERTs coalesce; the UPDATE is its own group → 2 calls, sizes {2,1}.
    assert_eq!(mock.call_count(), 2);
    let mut sizes = mock.recorded_sizes();
    sizes.sort_unstable();
    assert_eq!(sizes, vec![1, 2]);
    assert!(outcome.is_complete());
}

#[tokio::test]
async fn distinct_call_procs_do_not_coalesce() {
    let mock = Arc::new(MockDriver::new());
    let plan = Plan::leaf(node(0, EffectKind::Call(ProcId::new("mail.send"))))
        .merge(Plan::leaf(node(
            1,
            EffectKind::Call(ProcId::new("mail.send")),
        )))
        .merge(Plan::leaf(node(
            2,
            EffectKind::Call(ProcId::new("git.merge")),
        )));
    let interp = Interpreter::with_defaults(registry(mock.clone()));
    interp.commit(plan, &allow_all()).await.unwrap();

    // Two mail.send coalesce; git.merge is separate → 2 calls.
    assert_eq!(mock.call_count(), 2);
    let mut sizes = mock.recorded_sizes();
    sizes.sort_unstable();
    assert_eq!(sizes, vec![1, 2]);
}

// ---------------------------------------------------------------------------
// Ordering: a dependent chain A→B→C records A, B, C in dependency order.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn dependent_chain_executes_in_topological_order() {
    let mock = Arc::new(MockDriver::new());
    // A(0) -> B(1) -> C(2): each batch holds one effect (deps force separate frontiers).
    let plan = Plan::leaf(node(0, EffectKind::Insert))
        .then(Plan::leaf(node(1, EffectKind::Update)))
        .then(Plan::leaf(node(
            2,
            EffectKind::Call(ProcId::new("mail.send")),
        )));
    let interp = Interpreter::with_defaults(registry(mock.clone()));
    let outcome = interp.commit(plan, &allow_all()).await.unwrap();

    // Three separate frontiers → three calls, each size 1, in order.
    assert_eq!(mock.recorded_sizes(), vec![1, 1, 1]);
    let applied = outcome.applied_ids();
    assert_eq!(applied, vec![NodeId(0), NodeId(1), NodeId(2)]);
    // The ledger is in topological order.
    let ids: Vec<NodeId> = outcome.ledger.iter().map(|e| e.id).collect();
    assert_eq!(ids, vec![NodeId(0), NodeId(1), NodeId(2)]);
}

// ---------------------------------------------------------------------------
// Parallelism: independent branches run concurrently, bounded by `global`.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn independent_branches_run_in_parallel_bounded_by_global() {
    let mock = Arc::new(MockDriver::new().holding(Duration::from_millis(60)));
    // 3 independent groups of DIFFERENT kinds (so they do not coalesce) → 3 batches.
    // With global=2, at most 2 batches may overlap.
    let plan = Plan::leaf(node(0, EffectKind::Insert))
        .merge(Plan::leaf(node(1, EffectKind::Update)))
        .merge(Plan::leaf(node(2, EffectKind::Remove)));
    let interp = Interpreter::new(
        registry(mock.clone()),
        ConcurrencyLimits::new(2, 2),
        RetryPolicy::default(),
    );
    interp.commit(plan, &allow_all()).await.unwrap();

    assert_eq!(mock.call_count(), 3);
    // The concurrency assertion: with global=2, never more than 2 groups in flight.
    assert!(
        mock.peak_in_flight() <= 2,
        "peak in-flight {} exceeded global cap 2",
        mock.peak_in_flight()
    );
    // And at least 2 DID overlap (proving real parallelism, not serial execution).
    assert!(
        mock.peak_in_flight() >= 2,
        "expected parallel execution, peak in-flight was {}",
        mock.peak_in_flight()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn per_driver_cap_bounds_single_driver_fanout() {
    let mock = Arc::new(MockDriver::new().holding(Duration::from_millis(40)));
    // 3 distinct-kind groups, all on the same driver. global is generous (8) but
    // per_driver=1 forces them to run one at a time.
    let plan = Plan::leaf(node(0, EffectKind::Insert))
        .merge(Plan::leaf(node(1, EffectKind::Update)))
        .merge(Plan::leaf(node(2, EffectKind::Upsert)));
    let interp = Interpreter::new(
        registry(mock.clone()),
        ConcurrencyLimits::new(8, 1),
        RetryPolicy::default(),
    );
    interp.commit(plan, &allow_all()).await.unwrap();

    assert_eq!(
        mock.peak_in_flight(),
        1,
        "per_driver=1 must serialise the single driver's groups"
    );
}

// ---------------------------------------------------------------------------
// Failure → transitive dependents skipped (t09 semantics under parallelism).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn failed_node_skips_transitive_dependents() {
    let mock = Arc::new(MockDriver::new().failing_terminal(NodeId(0)));
    // A(0) fails; B(1) depends on A; C(2) depends on B; D(3) is independent and applies.
    let mut plan = Plan::leaf(node(0, EffectKind::Insert))
        .then(Plan::leaf(node(1, EffectKind::Update)))
        .then(Plan::leaf(node(2, EffectKind::Update)));
    plan = plan.merge(Plan::leaf(node(3, EffectKind::Insert)));
    let interp = Interpreter::with_defaults(registry(mock.clone()));
    let outcome = interp.commit(plan, &allow_all()).await.unwrap();

    // A failed; B and C skipped (transitively); D applied.
    let status = |id: u32| {
        outcome
            .ledger
            .iter()
            .find(|e| e.id == NodeId(id))
            .map(|e| e.status.clone())
            .unwrap()
    };
    assert!(matches!(status(0), LegStatus::Failed { .. }));
    assert!(matches!(status(1), LegStatus::Skipped { cause } if cause == NodeId(0)));
    assert!(matches!(status(2), LegStatus::Skipped { .. }));
    assert!(matches!(status(3), LegStatus::Applied { .. }));
    assert_eq!(outcome.failed_count(), 1);
    assert_eq!(outcome.skipped_count(), 2);
    // The failed node B was NEVER dispatched (its batch call never happened): the only
    // recorded batch sizes are the A-group (failed at apply) and the D-group.
    assert_eq!(mock.recorded_sizes().iter().sum::<usize>(), 2);
}

// ---------------------------------------------------------------------------
// Irreversible handling: retryable failure retried up to bound; irreversible never retried.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn retryable_non_irreversible_leg_retries_until_success() {
    // Node fails the first 2 attempts, succeeds on the 3rd (bound is 3).
    let mock = Arc::new(MockDriver::new().flaky(NodeId(0), 2));
    let plan = Plan::leaf(node(0, EffectKind::Upsert)); // Upsert: retry-safe, reversible.
    let interp = Interpreter::new(
        registry(mock.clone()),
        ConcurrencyLimits::default(),
        RetryPolicy::new(3, None),
    );
    let outcome = interp.commit(plan, &allow_all()).await.unwrap();

    let entry = &outcome.ledger[0];
    assert!(matches!(
        entry.status,
        LegStatus::Applied { attempts: 3, .. }
    ));
    // Three driver calls (the group re-dispatched twice after the retryable failures).
    assert_eq!(mock.call_count(), 3);
}

#[tokio::test]
async fn irreversible_leg_is_never_retried() {
    // An irreversible CALL that fails retryably: it must NOT be retried (RFD §6).
    let mock = Arc::new(MockDriver::new().failing_retryable(NodeId(0)));
    let irreversible_call = node(0, EffectKind::Call(ProcId::new("mail.send"))).irreversible(true);
    let plan = Plan::leaf(irreversible_call);
    let interp = Interpreter::new(
        registry(mock.clone()),
        ConcurrencyLimits::default(),
        RetryPolicy::new(5, None), // generous bound, but irreversibility vetoes retry
    );
    let outcome = interp.commit(plan, &allow_all()).await.unwrap();

    let entry = &outcome.ledger[0];
    assert!(entry.irreversible);
    assert!(matches!(
        entry.status,
        LegStatus::Failed { attempts: 1, .. }
    ));
    assert_eq!(mock.call_count(), 1, "irreversible leg must not be retried");
}

#[tokio::test]
async fn remove_is_inherently_irreversible_and_not_retried() {
    // REMOVE is inherently irreversible (no explicit flag needed).
    let mock = Arc::new(MockDriver::new().failing_retryable(NodeId(0)));
    let plan = Plan::leaf(node(0, EffectKind::Remove));
    let interp = Interpreter::new(
        registry(mock.clone()),
        ConcurrencyLimits::default(),
        RetryPolicy::new(4, None),
    );
    let outcome = interp.commit(plan, &allow_all()).await.unwrap();

    assert!(outcome.ledger[0].irreversible);
    assert_eq!(mock.call_count(), 1);
}

// ---------------------------------------------------------------------------
// Capability gating re-check at apply time.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ungranted_effect_is_capability_denied_before_dispatch() {
    let mock = Arc::new(MockDriver::new());
    let plan = Plan::leaf(node(0, EffectKind::Remove));
    // The cap set grants INSERT but NOT REMOVE on the mock driver.
    let caps = CapabilitySet::none().grant(driver_id(), &EffectKind::Insert);
    let interp = Interpreter::with_defaults(registry(mock.clone()));
    let outcome = interp.commit(plan, &caps).await.unwrap();

    let entry = &outcome.ledger[0];
    match &entry.status {
        LegStatus::Failed {
            error: EffectError::CapabilityDenied { driver, verb },
            ..
        } => {
            assert_eq!(driver.as_str(), "mock");
            assert_eq!(verb, "REMOVE");
        }
        other => panic!("expected CapabilityDenied, got {other:?}"),
    }
    // The driver was NEVER called — the denial happened before dispatch.
    assert_eq!(mock.call_count(), 0);
}

#[tokio::test]
async fn capability_denied_node_skips_its_dependents() {
    let mock = Arc::new(MockDriver::new());
    // A(0) is denied; B(1) depends on A → B is skipped.
    let plan =
        Plan::leaf(node(0, EffectKind::Remove)).then(Plan::leaf(node(1, EffectKind::Insert)));
    let caps = CapabilitySet::none().grant(driver_id(), &EffectKind::Insert);
    let interp = Interpreter::with_defaults(registry(mock.clone()));
    let outcome = interp.commit(plan, &caps).await.unwrap();

    assert!(matches!(
        outcome.ledger[0].status,
        LegStatus::Failed {
            error: EffectError::CapabilityDenied { .. },
            ..
        }
    ));
    assert!(matches!(
        outcome.ledger[1].status,
        LegStatus::Skipped { cause } if cause == NodeId(0)
    ));
    assert_eq!(mock.call_count(), 0);
}

// ---------------------------------------------------------------------------
// PREVIEW does nothing (no driver call).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn preview_performs_no_driver_calls() {
    let mock = Arc::new(MockDriver::new());
    let plan = Plan::leaf(node(0, EffectKind::Insert)).then(Plan::leaf(node(
        1,
        EffectKind::Call(ProcId::new("mail.send")),
    )));
    let interp = Interpreter::with_defaults(registry(mock.clone()));
    let outcome = interp.preview(&plan, &allow_all()).unwrap();

    // No driver was ever called; the ledger still describes the plan in topo order.
    assert_eq!(mock.call_count(), 0);
    assert_eq!(outcome.ledger.len(), 2);
    assert_eq!(outcome.ledger[0].id, NodeId(0));
    assert_eq!(outcome.ledger[1].id, NodeId(1));
    // Preview legs carry zero duration (no apply happened).
    assert!(outcome.ledger.iter().all(|e| e.duration.is_zero()));
}

// ---------------------------------------------------------------------------
// Misc: cyclic plan rejected; unregistered driver fails terminally; golden ledger json.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn cyclic_plan_is_rejected() {
    let mut plan =
        Plan::leaf(node(0, EffectKind::Insert)).merge(Plan::leaf(node(1, EffectKind::Insert)));
    // Introduce a cycle 0 -> 1 -> 0.
    plan = depends_on(plan, NodeId(1), NodeId(0));
    plan = depends_on(plan, NodeId(0), NodeId(1));
    let mock = Arc::new(MockDriver::new());
    let interp = Interpreter::with_defaults(registry(mock.clone()));
    let err = interp.commit(plan, &allow_all()).await.unwrap_err();
    assert_eq!(err.code(), "invalid_plan");
    assert_eq!(mock.call_count(), 0);
}

#[tokio::test]
async fn unregistered_driver_fails_every_leg_terminally() {
    let plan = Plan::leaf(node(0, EffectKind::Insert));
    // Empty registry — no driver for "mock".
    let interp = Interpreter::with_defaults(cfs_runtime::DriverRegistry::new());
    let outcome = interp.commit(plan, &allow_all()).await.unwrap();
    assert!(matches!(
        &outcome.ledger[0].status,
        LegStatus::Failed {
            error: EffectError::Terminal { .. },
            ..
        }
    ));
}

// ---------------------------------------------------------------------------
// O1: a very wide frontier admits at most `global` group-futures at once
// (bounded pending-future admission — "must not spawn unbounded tasks / cannot
// exhaust memory") while still completing and batching every group.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wide_frontier_admits_at_most_global_group_futures() {
    // 100 INDEPENDENT groups, each on its OWN driver (so they never coalesce and per-driver
    // throttling can never bind). With global=3, no more than 3 group-futures may be admitted
    // (each holds a global permit for its whole lifetime), so the peak concurrent driver call
    // — and therefore the peak number of resident admitted futures — must stay <= 3.
    const WIDTH: u32 = 100;
    const GLOBAL: usize = 3;

    let mock = Arc::new(MockDriver::new().holding(Duration::from_millis(5)));
    let mut reg = cfs_runtime::DriverRegistry::new();
    let mut plan = Plan::pure();
    for i in 0..WIDTH {
        let driver = format!("driver-{i}");
        // Share the single mock under every distinct driver id, so it observes the global
        // peak across all groups while each group keeps its own per-driver semaphore.
        reg = reg.with(DriverId::new(&driver), mock.clone());
        plan = plan.merge(Plan::leaf(node_on(&driver, i)));
    }

    let interp = Interpreter::new(
        reg,
        ConcurrencyLimits::new(GLOBAL, 8),
        RetryPolicy::default(),
    );
    let outcome = interp.commit(plan, &allow_all()).await.unwrap();

    // The bounded-admission assertion: peak admitted (== peak in-flight driver calls here,
    // since per-driver never throttles) never exceeded the global cap.
    assert!(
        mock.peak_in_flight() <= GLOBAL,
        "peak admitted group-futures {} exceeded global cap {GLOBAL}",
        mock.peak_in_flight()
    );
    // Real parallelism still happened (the admission gate did not serialise everything).
    assert!(
        mock.peak_in_flight() >= 2,
        "expected concurrent admission up to the cap, peak was {}",
        mock.peak_in_flight()
    );
    // ...and EVERY group still completed and was batched (one call per distinct-key group).
    assert_eq!(mock.call_count(), WIDTH as usize);
    assert!(outcome.is_complete());
    assert_eq!(outcome.applied_ids().len(), WIDTH as usize);
    assert!(
        mock.recorded_sizes().iter().all(|&s| s == 1),
        "each distinct-driver group is its own size-1 batch"
    );
}

// ---------------------------------------------------------------------------
// O2: preview drives the SAME Frontier as commit, so skip/irreversible
// accounting matches commit's for the same failing-shaped plan.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn preview_skip_accounting_matches_commit_for_same_shaped_plan() {
    // Build a plan whose failure SHAPE is provoked the same way in both paths: a capability
    // denial on A(0), with B(1) -> A and C(2) -> B (transitive dependents), plus an
    // independent D(3) that is allowed. commit fails A on the denial and skips B, C; preview
    // must reach the identical disposition because both drive `Frontier`.
    let build_plan = || {
        let mut p = Plan::leaf(node(0, EffectKind::Remove))
            .then(Plan::leaf(node(1, EffectKind::Update)))
            .then(Plan::leaf(node(2, EffectKind::Update)));
        p = p.merge(Plan::leaf(node(3, EffectKind::Insert)));
        p
    };
    // Grant INSERT (allows D) but NOT REMOVE (denies A) on the mock driver.
    let caps = CapabilitySet::none().grant(driver_id(), &EffectKind::Insert);

    let mock = Arc::new(MockDriver::new());
    let interp = Interpreter::with_defaults(registry(mock.clone()));

    let preview = interp.preview(&build_plan(), &caps).unwrap();
    let commit = interp.commit(build_plan(), &caps).await.unwrap();

    // Same ledger ordering (topo) and same per-node disposition KIND between the two paths.
    let disposition = |o: &cfs_runtime::Outcome| -> Vec<(NodeId, &'static str)> {
        o.ledger
            .iter()
            .map(|e| {
                let tag = match &e.status {
                    LegStatus::Applied { .. } => "applied",
                    LegStatus::Failed { .. } => "failed",
                    LegStatus::Skipped { .. } => "skipped",
                    _ => "other",
                };
                (e.id, tag)
            })
            .collect()
    };
    assert_eq!(
        disposition(&preview),
        disposition(&commit),
        "preview skip/failure accounting must match commit (single Frontier source of truth)"
    );
    // And the aggregate skip/failed counts agree too.
    assert_eq!(preview.failed_count(), commit.failed_count());
    assert_eq!(preview.skipped_count(), commit.skipped_count());
    assert_eq!(
        preview.skipped_count(),
        2,
        "B and C are skipped transitively"
    );
    assert_eq!(preview.failed_count(), 1, "A is the capability denial");
    // Skip causes match exactly (both attribute B's skip to A(0)).
    let cause = |o: &cfs_runtime::Outcome, id: u32| {
        o.ledger
            .iter()
            .find(|e| e.id == NodeId(id))
            .and_then(|e| match e.status {
                LegStatus::Skipped { cause } => Some(cause),
                _ => None,
            })
    };
    assert_eq!(cause(&preview, 1), cause(&commit, 1));
    assert_eq!(cause(&preview, 1), Some(NodeId(0)));
}

#[tokio::test]
async fn ledger_json_is_stable_and_secret_free() {
    let mock = Arc::new(MockDriver::new());
    let plan = Plan::leaf(node(0, EffectKind::Insert));
    let interp = Interpreter::with_defaults(registry(mock.clone()));
    let outcome = interp.commit(plan, &allow_all()).await.unwrap();
    let json = serde_json::to_string_pretty(&outcome).unwrap();
    // The shape is deterministic and carries only metadata (id/driver/kind/status), no
    // payloads or tokens.
    assert!(json.contains("\"status\": \"applied\""));
    assert!(json.contains("\"driver\": \"mock\""));
    assert!(json.contains("\"kind\": \"insert\""));
    assert!(!json.to_lowercase().contains("token"));
}
