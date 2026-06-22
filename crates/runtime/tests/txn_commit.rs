//! Integration tests for the t11 transactional COMMIT path (`Interpreter::commit_txn`). All
//! use an in-memory mock [`ApplyDriver`] + the in-memory [`InMemoryLedger`] — **no live
//! credentials, no network**, fully deterministic. The pure orchestration policy (saga/ACID
//! executors, key derivation, strategy selection) is unit-tested inside `cfs-txn`; these
//! prove the async interpreter wiring: strategy dispatch, idempotent resume through the
//! ledger, optimistic-concurrency conflict mapping, and a deterministic `RecoveryReport`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::Mutex;

use cfs_plan::{EffectKind, EffectNode, NodeId, Plan, PlanBuilder, Target, VfsPath};
use cfs_runtime::{
    ApplyCx, ApplyDriver, CapabilitySet, CommitStrategy, DriverRegistry, EffectError, EffectInput,
    EffectOutput, InMemoryLedger, Interpreter, LegOutcome, Precondition, Preconditions,
    TransactionalDrivers, Version,
};
use cfs_types::{Column, ColumnType, DriverId, Row, RowBatch, Schema, Value};

/// A mock driver that records which node ids it actually applied (so idempotent resume is
/// observable) and can fail specific nodes terminally / with a "conflict" reason.
#[derive(Default)]
struct TxnMock {
    applied: Mutex<Vec<NodeId>>,
    fail_terminal: HashSet<NodeId>,
    fail_conflict: HashSet<NodeId>,
}

impl TxnMock {
    fn new() -> Self {
        Self::default()
    }
    fn failing_terminal(mut self, id: NodeId) -> Self {
        self.fail_terminal.insert(id);
        self
    }
    fn failing_conflict(mut self, id: NodeId) -> Self {
        self.fail_conflict.insert(id);
        self
    }
    fn applied_ids(&self) -> Vec<NodeId> {
        self.applied.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl ApplyDriver for TxnMock {
    async fn apply_one(&self, e: &EffectInput, _cx: &ApplyCx) -> Result<EffectOutput, EffectError> {
        if self.fail_conflict.contains(&e.id) {
            return Err(EffectError::terminal("precondition failed: 412 conflict"));
        }
        if self.fail_terminal.contains(&e.id) {
            return Err(EffectError::terminal("mock terminal failure"));
        }
        self.applied.lock().unwrap().push(e.id);
        Ok(EffectOutput::new(e.id, 1))
    }
}

fn write_node(id: u32, driver: &str, kind: EffectKind) -> EffectNode {
    let schema = Schema::new(vec![Column::new("v", ColumnType::Int, false)]);
    let batch = RowBatch::new(schema, vec![Row::new(vec![Value::Int(i64::from(id))])]);
    EffectNode::new(
        NodeId(id),
        kind,
        Target::new(
            DriverId::new(driver),
            VfsPath::new(format!("/{driver}/{id}")),
        ),
    )
    .with_args(batch)
}

fn registry(driver: Arc<TxnMock>, id: &str) -> DriverRegistry {
    DriverRegistry::new().with(DriverId::new(id), driver)
}

/// Single transactional source → ACID strategy; a clean run applies every leg and the report
/// is clean (not rolled back).
#[tokio::test]
async fn acid_strategy_clean_commit() {
    let mock = Arc::new(TxnMock::new());
    let interp = Interpreter::with_defaults(registry(mock.clone(), "db"));

    let mut b = PlanBuilder::new();
    b.push(write_node(0, "db", EffectKind::Insert));
    b.push(write_node(1, "db", EffectKind::Update));
    let plan = b.build();

    let txnal = TransactionalDrivers::none().with(DriverId::new("db"));
    let ledger = InMemoryLedger::new();
    let (strategy, report) = interp
        .commit_txn(
            &plan,
            &CapabilitySet::allow_all(),
            "plan-1",
            &Preconditions::new(),
            &txnal,
            &ledger,
        )
        .await
        .unwrap();

    assert_eq!(strategy.code(), "single_source_acid");
    assert!(report.is_clean());
    assert!(!report.rolled_back);
    assert_eq!(report.applied_count(), 2);
    assert_eq!(mock.applied_ids(), vec![NodeId(0), NodeId(1)]);
}

/// ACID mid-plan failure → rolled_back flag set; the failing + subsequent legs do not commit.
#[tokio::test]
async fn acid_strategy_rolls_back_on_failure() {
    let mock = Arc::new(TxnMock::new().failing_terminal(NodeId(1)));
    let interp = Interpreter::with_defaults(registry(mock.clone(), "db"));

    let mut b = PlanBuilder::new();
    b.push(write_node(0, "db", EffectKind::Insert));
    b.push(write_node(1, "db", EffectKind::Insert));
    b.push(write_node(2, "db", EffectKind::Insert));
    let plan = b.build();

    let txnal = TransactionalDrivers::none().with(DriverId::new("db"));
    let ledger = InMemoryLedger::new();
    let (strategy, report) = interp
        .commit_txn(
            &plan,
            &CapabilitySet::allow_all(),
            "plan-1",
            &Preconditions::new(),
            &txnal,
            &ledger,
        )
        .await
        .unwrap();

    assert!(matches!(strategy, CommitStrategy::SingleSourceAcid { .. }));
    assert!(report.rolled_back, "ACID failure rolls back");
    assert_eq!(report.failure_at, Some(NodeId(1)));
    // Leg 2 was never attempted (skipped after the rollback boundary).
    assert_eq!(mock.applied_ids(), vec![NodeId(0)]);
}

/// Multi-source plan → saga strategy; idempotent resume: a re-run over the SAME ledger applies
/// nothing (every leg AlreadyApplied), the driver is not called again.
#[tokio::test]
async fn saga_strategy_idempotent_resume() {
    let mock_a = Arc::new(TxnMock::new());
    let mock_b = Arc::new(TxnMock::new());
    let registry = DriverRegistry::new()
        .with(DriverId::new("a"), mock_a.clone())
        .with(DriverId::new("b"), mock_b.clone());
    let interp = Interpreter::with_defaults(registry);

    let mut b = PlanBuilder::new();
    b.push(write_node(0, "a", EffectKind::Upsert));
    b.push(write_node(1, "b", EffectKind::Upsert));
    let plan = b.build();

    let txnal = TransactionalDrivers::none();
    let ledger = InMemoryLedger::new();

    let (strategy, r1) = interp
        .commit_txn(
            &plan,
            &CapabilitySet::allow_all(),
            "p",
            &Preconditions::new(),
            &txnal,
            &ledger,
        )
        .await
        .unwrap();
    assert_eq!(strategy.code(), "cross_source_saga");
    assert_eq!(r1.applied_count(), 2);
    assert_eq!(mock_a.applied_ids(), vec![NodeId(0)]);
    assert_eq!(mock_b.applied_ids(), vec![NodeId(1)]);

    // Re-run over the SAME ledger: every leg is a no-op (idempotent at-least-once redelivery).
    let (_s, r2) = interp
        .commit_txn(
            &plan,
            &CapabilitySet::allow_all(),
            "p",
            &Preconditions::new(),
            &txnal,
            &ledger,
        )
        .await
        .unwrap();
    assert_eq!(r2.already_applied_count(), 2);
    assert_eq!(r2.applied_count(), 0);
    // The drivers were NOT called a second time.
    assert_eq!(
        mock_a.applied_ids(),
        vec![NodeId(0)],
        "no re-apply on resume"
    );
    assert_eq!(
        mock_b.applied_ids(),
        vec![NodeId(1)],
        "no re-apply on resume"
    );
}

/// Optimistic concurrency: a conditional write whose driver reports a precondition/412 failure
/// is surfaced as a typed `Conflict` (not a generic failure), proving no lost update.
#[tokio::test]
async fn optimistic_conflict_surfaces_typed() {
    let mock = Arc::new(TxnMock::new().failing_conflict(NodeId(0)));
    let interp = Interpreter::with_defaults(registry(mock, "s3"));

    let plan = Plan::leaf(write_node(0, "s3", EffectKind::Update));
    let mut pre = Preconditions::new();
    pre.insert(NodeId(0), Precondition::IfVersion(Version::new("v1")));

    let txnal = TransactionalDrivers::none().with(DriverId::new("s3"));
    let ledger = InMemoryLedger::new();
    let (_strategy, report) = interp
        .commit_txn(
            &plan,
            &CapabilitySet::allow_all(),
            "p",
            &pre,
            &txnal,
            &ledger,
        )
        .await
        .unwrap();

    assert_eq!(
        report.conflict_count(),
        1,
        "typed conflict surfaced: {report:?}"
    );
    assert!(!report.is_clean());
    match &report.legs[0].outcome {
        LegOutcome::Conflict(v) => assert_eq!(v, &Version::new("v1")),
        other => panic!("expected Conflict, got {other:?}"),
    }
}

/// A capability denial on a transactional leg is a terminal leg failure (defense in depth),
/// and on the ACID path it triggers rollback.
#[tokio::test]
async fn capability_denied_leg_fails_and_rolls_back() {
    let mock = Arc::new(TxnMock::new());
    let interp = Interpreter::with_defaults(registry(mock.clone(), "db"));

    let plan = Plan::leaf(write_node(0, "db", EffectKind::Remove));
    // Grant nothing → the REMOVE is denied at apply time.
    let caps = CapabilitySet::none();
    let txnal = TransactionalDrivers::none().with(DriverId::new("db"));
    let ledger = InMemoryLedger::new();
    let (_strategy, report) = interp
        .commit_txn(&plan, &caps, "p", &Preconditions::new(), &txnal, &ledger)
        .await
        .unwrap();

    assert!(!report.is_clean());
    assert!(report.rolled_back);
    assert!(
        mock.applied_ids().is_empty(),
        "denied leg never reaches the driver"
    );
}

/// PREVIEW-equivalent purity: `select_strategy` (exposed via the runtime re-export) chooses the
/// strategy with no driver calls — a plan can be inspected without executing.
#[tokio::test]
async fn read_only_plan_is_saga_and_applies_no_legs() {
    let mock = Arc::new(TxnMock::new());
    let interp = Interpreter::with_defaults(registry(mock.clone(), "ga"));

    // A pure read plan: no write legs at all.
    let plan = Plan::leaf(EffectNode::new(
        NodeId(0),
        EffectKind::Read,
        Target::new(DriverId::new("ga"), VfsPath::new("/ga/x")),
    ));
    let ledger = InMemoryLedger::new();
    let (strategy, report) = interp
        .commit_txn(
            &plan,
            &CapabilitySet::allow_all(),
            "p",
            &Preconditions::new(),
            &TransactionalDrivers::none(),
            &ledger,
        )
        .await
        .unwrap();
    assert_eq!(strategy.code(), "cross_source_saga");
    assert!(report.legs.is_empty(), "no write legs");
    assert!(mock.applied_ids().is_empty());
}
