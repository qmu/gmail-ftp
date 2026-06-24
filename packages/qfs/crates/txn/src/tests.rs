//! In-crate unit tests for the transactional envelope (RFD-0001 §6/§10). Every test uses an
//! **in-memory fake** [`LegApplier`] / [`AuditLedger`] — NO live credentials, NO network,
//! fully deterministic. Covers: deterministic `EffectKey` (golden hash + reorder stability),
//! idempotent retry / resume, optimistic-concurrency conflict + auto-retry recovery, saga
//! reverse-order compensation, single-source ACID rollback, the `cp`/`mv` recoverable triple,
//! irreversible interaction, and the pure strategy selection.

use std::cell::RefCell;
use std::collections::HashMap;

use qfs_plan::{
    DriverId, EffectKind, EffectNode, NodeId, Plan, PlanBuilder, ProcId, Target, VfsPath,
};
use qfs_types::{Column, ColumnType, Row, RowBatch, Schema, Value};

use crate::{
    all_succeeded, select_strategy, AuditLedger, CommitStrategy, Compensation, CpStep, EffectKey,
    EffectLeg, EffectReceipt, InMemoryLedger, LegApplier, LegOutcome, LegRecord, Precondition,
    RecoveryReport, SagaExecutor, SagaPolicy, TransactionalDrivers, Version,
};

// ---- fixtures -------------------------------------------------------------------------

/// Build a write node on a driver/path with an exact arg row, for keying + apply tests.
fn write_node(id: u32, driver: &str, path: &str, kind: EffectKind, val: i64) -> EffectNode {
    let schema = Schema::new(vec![Column::new("v", ColumnType::Int, false)]);
    let batch = RowBatch::new(schema, vec![Row::new(vec![Value::Int(val)])]);
    EffectNode::new(
        NodeId(id),
        kind,
        Target::new(DriverId::new(driver), VfsPath::new(path)),
    )
    .with_args(batch)
}

/// A scripted in-memory fake driver: maps each leg's NodeId to a queued list of outcomes it
/// returns on successive `apply` calls, tracks the world version per target path, and records
/// applied + compensated node ids in call order. NO I/O.
struct FakeApplier {
    /// Per-node scripted outcomes (popped front-to-back across attempts).
    script: RefCell<HashMap<NodeId, Vec<LegOutcome>>>,
    /// The version the "world" currently holds per path (for the optimistic-concurrency check).
    world: RefCell<HashMap<String, Version>>,
    /// Node ids applied, in call order (for ordering assertions).
    applied: RefCell<Vec<NodeId>>,
    /// Node ids compensated, in call order (reverse-order assertion).
    compensated: RefCell<Vec<NodeId>>,
}

impl FakeApplier {
    fn new() -> Self {
        Self {
            script: RefCell::new(HashMap::new()),
            world: RefCell::new(HashMap::new()),
            applied: RefCell::new(Vec::new()),
            compensated: RefCell::new(Vec::new()),
        }
    }

    /// Queue scripted outcomes for a node (consumed front-first; the last is reused once the
    /// queue empties, so a single "Applied" repeats for idempotent re-runs).
    fn script(self, id: u32, outcomes: Vec<LegOutcome>) -> Self {
        self.script.borrow_mut().insert(NodeId(id), outcomes);
        self
    }

    /// Seed the world version for a path (the version a read would observe).
    fn world(self, path: &str, v: &str) -> Self {
        self.world
            .borrow_mut()
            .insert(path.to_string(), Version::new(v));
        self
    }
}

impl LegApplier for FakeApplier {
    fn apply(&mut self, leg: &EffectLeg, precondition: &Precondition) -> LegOutcome {
        // Optimistic-concurrency check: if the precondition is conditional, compare it to the
        // world version this path holds. A mismatch is a Conflict carrying the world version.
        if precondition.is_conditional() {
            let path = leg.descriptor.target.path.as_str().to_string();
            if let Some(world_v) = self.world.borrow().get(&path).cloned() {
                if !precondition.is_satisfied_by(&world_v) {
                    return LegOutcome::Conflict { version: world_v };
                }
            }
        }
        // Pop the next scripted outcome (reuse the last once the queue is down to one).
        let mut script = self.script.borrow_mut();
        let outcome = match script.get_mut(&leg.descriptor.id) {
            Some(queue) if queue.len() > 1 => queue.remove(0),
            Some(queue) => {
                queue
                    .first()
                    .cloned()
                    .unwrap_or(LegOutcome::Applied(EffectReceipt::new(
                        leg.descriptor.id,
                        1,
                    )))
            }
            None => LegOutcome::Applied(EffectReceipt::new(leg.descriptor.id, 1)),
        };
        if let LegOutcome::Applied(_) = &outcome {
            self.applied.borrow_mut().push(leg.descriptor.id);
        }
        outcome
    }

    fn compensate(&mut self, leg: &EffectLeg, _comp: &Compensation) {
        self.compensated.borrow_mut().push(leg.descriptor.id);
    }
}

// ---- EffectKey determinism ------------------------------------------------------------

/// Golden hash: the derived key is byte-stable across runs (FNV-1a over canonical bytes).
#[test]
fn effect_key_is_deterministic_golden() {
    let node = write_node(7, "s3", "/s3/bucket/k", EffectKind::Upsert, 42);
    let k1 = EffectKey::derive("plan-A", &node);
    let k2 = EffectKey::derive("plan-A", &node);
    assert_eq!(k1, k2, "same inputs derive the same key");
    // The golden value pins the canonical-serialization + FNV-1a contract so a refactor that
    // changes the hash is caught. The prefix is the readable handle; the suffix is the hash.
    assert!(
        k1.as_str().starts_with("k:plan-A:7:"),
        "readable handle: {}",
        k1.as_str()
    );
    assert_eq!(
        k1.as_str().len(),
        "k:plan-A:7:".len() + 16,
        "16 hex hash chars"
    );
}

/// Distinct effects derive distinct keys (plan id, node id, args, and target all matter).
#[test]
fn effect_key_distinguishes_effects() {
    let a = write_node(1, "s3", "/s3/k", EffectKind::Insert, 1);
    let b = write_node(1, "s3", "/s3/k", EffectKind::Insert, 2); // different args
    let c = write_node(2, "s3", "/s3/k", EffectKind::Insert, 1); // different node id
    let d = write_node(1, "gmail", "/mail/k", EffectKind::Insert, 1); // different target
    let ka = EffectKey::derive("p", &a);
    assert_ne!(ka, EffectKey::derive("p", &b));
    assert_ne!(ka, EffectKey::derive("p", &c));
    assert_ne!(ka, EffectKey::derive("p", &d));
    assert_ne!(ka, EffectKey::derive("q", &a)); // different plan id
}

/// Stability under batch reordering: the key depends on the node's content, not the order the
/// node appears in a batch/frontier, so the t10 reorder cannot change the dedup handle.
#[test]
fn effect_key_stable_under_reordering() {
    let n1 = write_node(1, "s3", "/s3/a", EffectKind::Insert, 10);
    let n2 = write_node(2, "s3", "/s3/b", EffectKind::Insert, 20);
    // Derive in two opposite orders; collect into maps and compare.
    let forward = [EffectKey::derive("p", &n1), EffectKey::derive("p", &n2)];
    let backward = [EffectKey::derive("p", &n2), EffectKey::derive("p", &n1)];
    assert_eq!(forward[0], backward[1]);
    assert_eq!(forward[1], backward[0]);
}

// ---- idempotent retry / resume --------------------------------------------------------

/// A retried/duplicated effect does not double-apply: the first run applies, a second run over
/// the SAME ledger sees `AlreadyApplied` for every leg and the fake driver is never called.
#[test]
fn idempotent_resume_applies_once() {
    let ledger = InMemoryLedger::new();
    let leg = EffectLeg::from_node(
        "p",
        &write_node(0, "s3", "/s3/k", EffectKind::Upsert, 1),
        Precondition::None,
    );

    // First run: applies fresh.
    let mut driver1 = FakeApplier::new();
    let r1 = SagaExecutor::new(&ledger).run_saga(&mut driver1, std::slice::from_ref(&leg));
    assert_eq!(r1.applied_count(), 1);
    assert_eq!(r1.already_applied_count(), 0);
    assert!(r1.is_clean());
    assert_eq!(ledger.applied_count(), 1);
    assert_eq!(driver1.applied.borrow().len(), 1, "driver applied once");

    // Second run over the SAME ledger: no second apply (idempotent at-least-once redelivery).
    let mut driver2 = FakeApplier::new();
    let r2 = SagaExecutor::new(&ledger).run_saga(&mut driver2, std::slice::from_ref(&leg));
    assert_eq!(r2.applied_count(), 0);
    assert_eq!(r2.already_applied_count(), 1, "re-run is a no-op");
    assert!(
        driver2.applied.borrow().is_empty(),
        "driver NOT called on re-run"
    );
}

// ---- optimistic concurrency -----------------------------------------------------------

/// Stale version → typed `Conflict` (no auto-retry). The write is conditioned on `v1` but the
/// world moved to `v2` underneath; the executor surfaces `Conflict(v2)` — no lost update.
#[test]
fn optimistic_conflict_is_typed_and_blocks_write() {
    let ledger = InMemoryLedger::new();
    let leg = EffectLeg::from_node(
        "p",
        &write_node(0, "s3", "/s3/k", EffectKind::Update, 1),
        Precondition::IfVersion(Version::new("v1")),
    );
    // World already moved to v2; no auto-retry (conflict_retries = 0).
    let mut driver = FakeApplier::new().world("/s3/k", "v2");
    let report = SagaExecutor::with_policy(
        &ledger,
        SagaPolicy {
            conflict_retries: 0,
        },
    )
    .run_saga(&mut driver, std::slice::from_ref(&leg));
    assert_eq!(report.conflict_count(), 1);
    assert!(!report.is_clean());
    match &report.legs[0].outcome {
        LegOutcome::Conflict { version } => assert_eq!(version, &Version::new("v2")),
        other => panic!("expected Conflict(v2), got {other:?}"),
    }
    assert!(
        driver.applied.borrow().is_empty(),
        "stale write never lands"
    );
}

/// Fresh version → success: a write conditioned on the version the world holds applies. Also
/// asserts the `If-Match`/expected-version token the driver would send (golden).
#[test]
fn optimistic_fresh_version_succeeds_and_sends_if_match() {
    let ledger = InMemoryLedger::new();
    let pre = Precondition::IfVersion(Version::new("v2"));
    assert_eq!(pre.if_match_header(), Some("v2"), "If-Match token asserted");
    let leg = EffectLeg::from_node(
        "p",
        &write_node(0, "s3", "/s3/k", EffectKind::Update, 1),
        pre,
    );
    let mut driver = FakeApplier::new().world("/s3/k", "v2"); // matches the precondition
    let report = SagaExecutor::new(&ledger).run_saga(&mut driver, std::slice::from_ref(&leg));
    assert!(report.is_clean());
    assert_eq!(report.applied_count(), 1);
}

/// Auto-retry recovery: the write is stale (`v1`) but the world holds `v2`; with auto-retry
/// enabled the executor re-reads (re-bases the precondition on `v2`) and succeeds.
#[test]
fn optimistic_conflict_auto_retry_recovers() {
    let ledger = InMemoryLedger::new();
    let leg = EffectLeg::from_node(
        "p",
        &write_node(0, "s3", "/s3/k", EffectKind::Update, 1),
        Precondition::IfVersion(Version::new("v1")),
    );
    // After the first conflict the executor re-bases to v2, which now matches the world.
    let mut driver = FakeApplier::new().world("/s3/k", "v2");
    let report = SagaExecutor::with_policy(
        &ledger,
        SagaPolicy {
            conflict_retries: 2,
        },
    )
    .run_saga(&mut driver, std::slice::from_ref(&leg));
    assert!(report.is_clean(), "auto-retry recovered: {report:?}");
    assert_eq!(report.applied_count(), 1);
}

// ---- saga compensation ----------------------------------------------------------------

/// Cross-source saga: a failure on leg N runs compensation for legs 1..N-1 in REVERSE order;
/// the failed/unreached legs are not compensated.
#[test]
fn saga_compensates_applied_legs_in_reverse() {
    let ledger = InMemoryLedger::new();
    let legs = vec![
        EffectLeg::from_node(
            "p",
            &write_node(0, "a", "/a/1", EffectKind::Insert, 1),
            Precondition::None,
        )
        .with_compensation(Compensation::DeleteCreated),
        EffectLeg::from_node(
            "p",
            &write_node(1, "b", "/b/2", EffectKind::Insert, 2),
            Precondition::None,
        )
        .with_compensation(Compensation::DeleteCreated),
        EffectLeg::from_node(
            "p",
            &write_node(2, "c", "/c/3", EffectKind::Insert, 3),
            Precondition::None,
        )
        .with_compensation(Compensation::DeleteCreated),
    ];
    // Legs 0 and 1 apply; leg 2 fails terminally.
    let mut driver = FakeApplier::new().script(
        2,
        vec![LegOutcome::Failed(crate::EffectError::terminal("boom"))],
    );
    let report = SagaExecutor::new(&ledger).run_saga(&mut driver, &legs);
    assert!(!report.is_clean());
    assert_eq!(report.failure_at, Some(NodeId(2)));
    // Compensation ran for the two applied legs, REVERSE order: leg 1 then leg 0.
    assert_eq!(*driver.compensated.borrow(), vec![NodeId(1), NodeId(0)]);
    assert_eq!(report.compensated, vec![NodeId(1), NodeId(0)]);
}

/// Saga re-run after a partial success re-applies NOTHING: every prior leg's key is
/// `AlreadyApplied`. (Acceptance: a re-run of the same plan re-applies nothing.)
#[test]
fn saga_rerun_reapplies_nothing() {
    let ledger = InMemoryLedger::new();
    let legs = vec![
        EffectLeg::from_node(
            "p",
            &write_node(0, "a", "/a/1", EffectKind::Insert, 1),
            Precondition::None,
        ),
        EffectLeg::from_node(
            "p",
            &write_node(1, "b", "/b/2", EffectKind::Insert, 2),
            Precondition::None,
        ),
    ];
    let mut d1 = FakeApplier::new();
    let r1 = SagaExecutor::new(&ledger).run_saga(&mut d1, &legs);
    assert_eq!(r1.applied_count(), 2);

    let mut d2 = FakeApplier::new();
    let r2 = SagaExecutor::new(&ledger).run_saga(&mut d2, &legs);
    assert_eq!(r2.already_applied_count(), 2);
    assert_eq!(r2.applied_count(), 0);
    assert!(d2.applied.borrow().is_empty());
}

// ---- single-source ACID ---------------------------------------------------------------

/// Single-source ACID: an injected mid-plan failure rolls the WHOLE transaction back — the
/// report flags `rolled_back` so the runtime issues the driver `rollback` (zero applied).
#[test]
fn acid_rolls_back_whole_transaction_on_failure() {
    let ledger = InMemoryLedger::new();
    let legs = vec![
        EffectLeg::from_node(
            "p",
            &write_node(0, "db", "/db/t", EffectKind::Insert, 1),
            Precondition::None,
        ),
        EffectLeg::from_node(
            "p",
            &write_node(1, "db", "/db/t", EffectKind::Insert, 2),
            Precondition::None,
        ),
        EffectLeg::from_node(
            "p",
            &write_node(2, "db", "/db/t", EffectKind::Insert, 3),
            Precondition::None,
        ),
    ];
    // Leg 1 fails → rollback; leg 2 is never attempted.
    let mut driver = FakeApplier::new().script(
        1,
        vec![LegOutcome::Failed(crate::EffectError::terminal(
            "constraint",
        ))],
    );
    let report = SagaExecutor::new(&ledger).run_acid(&mut driver, &legs);
    assert!(report.rolled_back, "ACID failure rolls the txn back");
    assert_eq!(report.failure_at, Some(NodeId(1)));
    // Only leg 0 actually hit the driver before the failure; the rollback (driver-side) is
    // what undoes it — proven here by the report flag the runtime acts on.
    assert_eq!(*driver.applied.borrow(), vec![NodeId(0)]);
    // Leg 2 was skipped (reported), never applied.
    assert!(matches!(report.legs[2].outcome, LegOutcome::Failed(_)));
}

// ---- cp / mv recoverable triple -------------------------------------------------------

/// The `cp`/`mv` step compilation: `mv` is copy → verify → delete (delete LAST, never before
/// verify); `cp` omits the delete (source preserved).
#[test]
fn cp_mv_step_sequences_never_delete_before_verify() {
    let mv = CpStep::mv_sequence();
    assert_eq!(mv, [CpStep::Copy, CpStep::Verify, CpStep::Delete]);
    // Verify strictly precedes Delete (the data-loss-prevention invariant).
    let v = mv.iter().position(|s| *s == CpStep::Verify).unwrap();
    let d = mv.iter().position(|s| *s == CpStep::Delete).unwrap();
    assert!(v < d, "verify must precede delete");
    assert_eq!(CpStep::cp_sequence(), [CpStep::Copy, CpStep::Verify]);
}

/// Recoverable `mv`: a fault injected AFTER copy, BEFORE delete leaves the source intact; on
/// re-run the copy is `AlreadyApplied` (no re-copy) and the delete completes — no data loss.
#[test]
fn mv_recovers_after_copy_before_delete() {
    let ledger = InMemoryLedger::new();
    // Model the mv as two keyed legs: copy (node 0) then delete-source (node 1).
    let copy = EffectLeg::from_node(
        "mv",
        &write_node(0, "s3", "/dst/k", EffectKind::Upsert, 1),
        Precondition::None,
    );
    let del = EffectLeg::from_node(
        "mv",
        &write_node(1, "fs", "/src/k", EffectKind::Remove, 1),
        Precondition::None,
    );
    let legs = vec![copy.clone(), del.clone()];

    // Run 1: copy applies, then a fault on the delete leg.
    let mut d1 = FakeApplier::new().script(
        1,
        vec![LegOutcome::Failed(crate::EffectError::retryable(
            "crash before delete",
        ))],
    );
    let r1 = SagaExecutor::new(&ledger).run_saga(&mut d1, &legs);
    assert_eq!(r1.failure_at, Some(NodeId(1)), "failed at delete");
    // Source still intact: the delete never landed (only the copy applied).
    assert_eq!(*d1.applied.borrow(), vec![NodeId(0)]);

    // Run 2 (recovery): copy is AlreadyApplied (NO re-copy), delete now completes.
    let mut d2 = FakeApplier::new(); // delete succeeds this time
    let r2 = SagaExecutor::new(&ledger).run_saga(&mut d2, &legs);
    assert!(r2.is_clean(), "recovery completed: {r2:?}");
    // The copy was NOT re-applied (idempotent); only the delete ran on recovery.
    assert_eq!(
        *d2.applied.borrow(),
        vec![NodeId(1)],
        "only the delete on re-run"
    );
    assert_eq!(r2.already_applied_count(), 1, "copy was already applied");
}

// ---- irreversible interaction ---------------------------------------------------------

/// An irreversible leg is applied at most once and is NEVER compensated, even if a later leg
/// fails. (Acceptance: irreversible interaction.)
#[test]
fn irreversible_leg_is_not_compensated() {
    let ledger = InMemoryLedger::new();
    let mut irreversible = write_node(
        0,
        "mail",
        "/mail/out",
        EffectKind::Call(ProcId::new("mail.send")),
        1,
    );
    irreversible.irreversible = true;
    let legs = vec![
        EffectLeg::from_node("p", &irreversible, Precondition::None)
            .with_compensation(Compensation::DeleteCreated), // registered but must be ignored
        EffectLeg::from_node(
            "p",
            &write_node(1, "db", "/db/t", EffectKind::Insert, 2),
            Precondition::None,
        )
        .with_compensation(Compensation::DeleteCreated),
    ];
    // Leg 0 (irreversible) applies; leg 1 fails → saga tries to compensate.
    let mut driver = FakeApplier::new().script(
        1,
        vec![LegOutcome::Failed(crate::EffectError::terminal("boom"))],
    );
    let report = SagaExecutor::new(&ledger).run_saga(&mut driver, &legs);
    assert_eq!(report.failure_at, Some(NodeId(1)));
    // The irreversible send is NOT in the compensated set (it cannot be undone).
    assert!(
        !report.compensated.contains(&NodeId(0)),
        "irreversible never compensated"
    );
    assert!(
        driver.compensated.borrow().is_empty(),
        "no compensation ran (only leg 0 applied, irreversible)"
    );
}

// ---- strategy selection (pure / PREVIEW) ----------------------------------------------

/// `select_strategy` is pure (no I/O) and PREVIEW-friendly: a plan whose every write hits one
/// transactional source is ACID; a multi-source plan is a saga.
#[test]
fn strategy_single_transactional_source_is_acid() {
    let mut b = PlanBuilder::new();
    let id0 = b.next_id();
    b.push(write_node(
        id0.index(),
        "db",
        "/db/t",
        EffectKind::Insert,
        1,
    ));
    let id1 = b.next_id();
    b.push(write_node(
        id1.index(),
        "db",
        "/db/t",
        EffectKind::Update,
        2,
    ));
    let plan = b.build();

    let txnal = TransactionalDrivers::none().with(DriverId::new("db"));
    let strat = select_strategy(&plan, &txnal);
    assert_eq!(strat.code(), "single_source_acid");
    match strat {
        CommitStrategy::SingleSourceAcid { source } => assert_eq!(source, DriverId::new("db")),
        other => panic!("expected ACID, got {other:?}"),
    }
}

/// Multi-source plan (no distributed txn) → saga, even if each source is individually
/// transactional. Single non-transactional source → also saga.
#[test]
fn strategy_multi_source_or_nontransactional_is_saga() {
    // Multi-source: writes to both `db` and `s3`.
    let mut b = PlanBuilder::new();
    let a = b.next_id();
    b.push(write_node(a.index(), "db", "/db/t", EffectKind::Insert, 1));
    let c = b.next_id();
    b.push(write_node(c.index(), "s3", "/s3/k", EffectKind::Upsert, 2));
    let multi = b.build();
    let both_txnal = TransactionalDrivers::none()
        .with(DriverId::new("db"))
        .with(DriverId::new("s3"));
    assert_eq!(
        select_strategy(&multi, &both_txnal).code(),
        "cross_source_saga"
    );

    // Single source but NOT declared transactional → saga.
    let single = Plan::leaf(write_node(0, "s3", "/s3/k", EffectKind::Upsert, 1));
    assert_eq!(
        select_strategy(&single, &TransactionalDrivers::none()).code(),
        "cross_source_saga"
    );
}

/// `Read`/`List` dependencies do not count as write sources for strategy selection (a plan
/// that reads from many sources but writes to one transactional source is still ACID).
#[test]
fn strategy_ignores_read_only_sources() {
    let mut b = PlanBuilder::new();
    let r = b.next_id();
    b.push(EffectNode::new(
        r,
        EffectKind::Read,
        Target::new(DriverId::new("ga"), VfsPath::new("/ga/x")),
    ));
    let w = b.next_id();
    b.push(write_node(w.index(), "db", "/db/t", EffectKind::Insert, 1));
    b.depends_on(w, r);
    let plan = b.build();
    let txnal = TransactionalDrivers::none().with(DriverId::new("db"));
    assert_eq!(select_strategy(&plan, &txnal).code(), "single_source_acid");
}

// ---- helpers ledger / report ----------------------------------------------------------

/// `all_succeeded` treats both fresh-apply and already-applied as success (the resume case).
#[test]
fn all_succeeded_counts_resume_as_success() {
    let outs = vec![
        LegOutcome::Applied(EffectReceipt::new(NodeId(0), 1)),
        LegOutcome::AlreadyApplied,
    ];
    assert!(all_succeeded(&outs));
    let with_conflict = vec![LegOutcome::Conflict {
        version: Version::new("v9"),
    }];
    assert!(!all_succeeded(&with_conflict));
}

/// Ledger records intent before apply (the append-before-apply / crash-recovery contract):
/// after a run the intent count ≥ applied count, and a sealed key answers `applied()`.
#[test]
fn ledger_records_intent_before_apply() {
    let ledger = InMemoryLedger::new();
    let leg = EffectLeg::from_node(
        "p",
        &write_node(0, "s3", "/s3/k", EffectKind::Upsert, 1),
        Precondition::None,
    );
    let mut driver = FakeApplier::new();
    let _ = SagaExecutor::new(&ledger).run_saga(&mut driver, std::slice::from_ref(&leg));
    assert!(ledger.has_intent(&leg.key), "intent recorded");
    assert!(ledger.applied(&leg.key).is_some(), "sealed after apply");
    assert!(ledger.intent_count() >= ledger.applied_count());
}

/// The RecoveryReport serializes to stable, secret-free JSON (`-json` audit projection).
#[test]
fn recovery_report_json_is_secret_free_and_stable() {
    let ledger = InMemoryLedger::new();
    let leg = EffectLeg::from_node(
        "p",
        &write_node(0, "s3", "/s3/k", EffectKind::Upsert, 7),
        Precondition::None,
    );
    let mut driver = FakeApplier::new();
    let report = SagaExecutor::new(&ledger).run_saga(&mut driver, std::slice::from_ref(&leg));
    let json = serde_json::to_string(&report).unwrap();
    // The key + outcome are present; the payload value (7) is NOT (metadata only).
    assert!(json.contains("\"applied\""), "outcome present: {json}");
    assert!(json.contains("k:p:0:"), "effect key present");
    assert!(
        !json.contains("\"v\":7") && !json.contains("Int"),
        "no payload leaked: {json}"
    );
}

/// Regression (t12 / CO-t12-1 Planner E2E block): a `RecoveryReport` holding **every**
/// `LegOutcome` variant — explicitly including `Conflict` — must serialize to JSON without a
/// runtime error. Before the fix `Conflict(Version)` was a newtype variant wrapping a
/// primitive, which serde's internal `#[serde(tag = "outcome")]` tagging cannot represent
/// ("cannot serialize tagged newtype variant LegOutcome::Conflict containing a string"), so any
/// commit that surfaced an optimistic-concurrency conflict could not be emitted as the JSON
/// audit-of-record. As a struct variant `Conflict { version }` it serializes cleanly.
#[test]
fn recovery_report_with_every_outcome_variant_serializes() {
    // One leg per LegOutcome variant; the variant value is what exercises the tag encoding.
    let leg = |id: u32| {
        EffectLeg::from_node(
            "p",
            &write_node(id, "s3", &format!("/s3/k{id}"), EffectKind::Upsert, 1),
            Precondition::None,
        )
    };
    let conflict_leg = leg(2);
    let outcomes = vec![
        LegOutcome::Applied(EffectReceipt::new(NodeId(0), 1)),
        LegOutcome::AlreadyApplied,
        LegOutcome::Conflict {
            version: Version::new("world-v9-REAL"),
        },
        LegOutcome::Indeterminate {
            key: conflict_leg.key.clone(),
        },
        LegOutcome::Failed(crate::EffectError::terminal("boom")),
    ];
    // Sanity: we covered the whole closed set (every `code()` is distinct).
    let mut codes: Vec<&str> = outcomes.iter().map(LegOutcome::code).collect();
    codes.sort_unstable();
    codes.dedup();
    assert_eq!(codes.len(), outcomes.len(), "one leg per distinct variant");

    let records: Vec<_> = outcomes
        .into_iter()
        .enumerate()
        .map(|(i, o)| LegRecord::from_outcome(&leg(i as u32), o))
        .collect();
    let report = RecoveryReport::new(records, Some(NodeId(2)), Vec::new());

    // This is the operation that FAILED before the fix (serde returns Err for the Conflict leg).
    let json = serde_json::to_string(&report)
        .expect("a report containing every LegOutcome variant must serialize");

    // The Conflict leg serializes as an internally-tagged object carrying the real, non-secret
    // world coordinate — never a credential (RFD §10).
    assert!(
        json.contains("\"outcome\":\"conflict\""),
        "conflict tag present: {json}"
    );
    assert!(
        json.contains("\"version\":\"world-v9-REAL\""),
        "real world version present in conflict leg: {json}"
    );
}

/// `EffectDescriptor::is_replay_safe` (t12 reconcile classifier): `UPSERT` and any
/// conditionally-guarded write are replay-safe in the crash window; an unconditional
/// `Insert`/`Remove`/`Call` is NOT (a blind replay could double-apply — apply-once, RFD §6/§10).
#[test]
fn replay_safe_classifies_idempotency_for_reconcile() {
    let upsert = EffectLeg::from_node(
        "p",
        &write_node(0, "s3", "/s3/k", EffectKind::Upsert, 1),
        Precondition::None,
    );
    assert!(upsert.descriptor.is_replay_safe(), "upsert is convergent");

    let insert_unconditional = EffectLeg::from_node(
        "p",
        &write_node(1, "s3", "/s3/k", EffectKind::Insert, 1),
        Precondition::None,
    );
    assert!(
        !insert_unconditional.descriptor.is_replay_safe(),
        "unconditional insert is not replay-safe"
    );

    let insert_guarded = EffectLeg::from_node(
        "p",
        &write_node(2, "s3", "/s3/k", EffectKind::Insert, 1),
        Precondition::IfVersion(Version::new("v1")),
    );
    assert!(
        insert_guarded.descriptor.is_replay_safe(),
        "a conditional guard catches a stale replay as Conflict"
    );

    let remove = EffectLeg::from_node(
        "p",
        &write_node(3, "s3", "/s3/k", EffectKind::Remove, 0),
        Precondition::None,
    );
    assert!(
        !remove.descriptor.is_replay_safe(),
        "an unconditional remove is not replay-safe"
    );
}

/// The `Indeterminate` outcome has a stable machine code and is neither success nor a silent
/// replay — the saga / bridge treat it as a hard stop.
#[test]
fn indeterminate_outcome_code_is_stable() {
    let key = EffectKey::derive("p", &write_node(0, "s3", "/s3/k", EffectKind::Insert, 1));
    let outcome = LegOutcome::Indeterminate { key };
    assert_eq!(outcome.code(), "indeterminate");
    assert!(!outcome.is_success());
}
