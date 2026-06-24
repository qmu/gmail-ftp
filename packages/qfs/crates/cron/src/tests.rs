//! Internal unit + integration tests for the JOB scheduler (no live creds, no network):
//! `MockClock` + `MemJobStore` + `RecordingCommitter`.

use qfs_parser::parse_statement;

use crate::commit::RecordingCommitter;
use crate::lastrun::{bind_last_run, references_last_run};
use crate::policy::MissedPolicy;
use crate::schedule::{CronExpr, Schedule, ScheduleError};
use crate::scheduler::{run_id_for, Scheduler};
use crate::store::{JobBinding, JobStore, MemJobStore, PolicyRef, RunState, RunStatus};
use crate::MockClock;

use std::sync::Arc;

use qfs_core::{
    Archetype, Capabilities, Column, ColumnType, Engine, NodeDesc, Path, PlanSpec, PushdownProfile,
    Schema,
};

// --- a minimal in-memory fake `/mock` source so a DO body with a `FROM /mock/src` resolves to a
// real Plan in the PREVIEW path (no live creds). build_plan needs only the Driver (mount +
// describe + capabilities + applier), not a ReadDriver — plan construction is pure. ---

struct FakeMock;

fn mock_schema() -> Schema {
    Schema::new(vec![
        Column::new("id", ColumnType::Int, false),
        Column::new("ts", ColumnType::Int, true),
    ])
}

impl qfs_core::Driver for FakeMock {
    fn mount(&self) -> &str {
        "/mock"
    }
    fn describe(&self, _path: &Path) -> Result<NodeDesc, qfs_core::CfsError> {
        Ok(NodeDesc::new(Archetype::RelationalTable, mock_schema()))
    }
    fn capabilities(&self, _path: &Path) -> Capabilities {
        Capabilities::none().select().insert().update().remove()
    }
    fn procedures(&self) -> &[qfs_core::ProcSig] {
        &[]
    }
    fn pushdown(&self) -> &PushdownProfile {
        &PushdownProfile::None
    }
    fn applier(&self) -> &dyn qfs_core::PlanApplier {
        Box::leak(Box::new(NoopApplier))
    }
}

#[derive(Default)]
struct NoopApplier;
impl qfs_core::PlanApplier for NoopApplier {
    fn apply(
        &mut self,
        node: &qfs_core::EffectNode,
    ) -> Result<qfs_core::AppliedEffect, qfs_core::ApplyError> {
        Ok(qfs_core::AppliedEffect::new(node.id, 0))
    }
}

fn engine_with_mock() -> Engine {
    let mut engine = Engine::new();
    engine
        .mounts
        .register(Arc::new(FakeMock))
        .expect("register /mock");
    engine
}

// --- helpers ---

fn plan_spec(src: &str) -> PlanSpec {
    let stmt = parse_statement(src).expect("DO body parses");
    PlanSpec::from_statement(stmt)
}

fn job(name: &str, schedule: Schedule, do_src: &str, missed: MissedPolicy) -> JobBinding {
    JobBinding {
        name: name.to_string(),
        schedule,
        plan: plan_spec(do_src),
        // t35: a granting POLICY ref so the firing tests exercise the allow path (the bodies
        // here are reversible INSERT/UPSERT; the test committer's table grants exactly those).
        policy: PolicyRef::named("jobwriter"),
        missed,
        enabled: true,
    }
}

/// A `RecordingCommitter` wired with a permissive test policy table: `jobwriter` grants
/// `INSERT,UPSERT` (the reversible writes the test DO bodies perform), so the firing tests
/// exercise the t35 allow path. A JOB with no/other policy still default-denies.
fn allowing_committer(engine: Engine) -> RecordingCommitter {
    let mut table = qfs_server::PolicyTable::new();
    table.insert(
        "jobwriter".to_string(),
        qfs_server::PolicyDef {
            name: "jobwriter".to_string(),
            handler: String::new(),
            allow: vec!["ALLOW INSERT,UPSERT".to_string()],
        },
    );
    let handle = Arc::new(std::sync::RwLock::new(Arc::new(table)));
    RecordingCommitter::with_engine(engine).with_policies(handle)
}

const DO_INSERT: &str = "UPSERT INTO /mock/sink VALUES (1)";

const DO_LAST_RUN: &str = "INSERT INTO /mock/sink FROM /mock/src |> WHERE ts > LAST_RUN()";

// --- next_after goldens ---

#[test]
fn every_next_after_advances_from_anchor() {
    let s = Schedule::every_anchored(3600, 0).expect("valid");
    // from just after a boundary -> next boundary.
    assert_eq!(s.next_after(0), Some(3600));
    assert_eq!(s.next_after(3599), Some(3600));
    assert_eq!(s.next_after(3600), Some(7200));
    assert_eq!(s.next_after(7000), Some(7200));
}

#[test]
fn every_rejects_zero_interval() {
    assert_eq!(Schedule::every(0), Err(ScheduleError::ZeroInterval));
    assert_eq!(Schedule::every(-5), Err(ScheduleError::ZeroInterval));
}

#[test]
fn cron_every_15_minutes_golden() {
    let s = Schedule::cron("*/15 * * * *").expect("valid cron");
    // 1970-01-01 00:00:00 UTC = epoch 0. Next after 0 is 00:15 = 900.
    assert_eq!(s.next_after(0), Some(15 * 60));
    assert_eq!(s.next_after(15 * 60), Some(30 * 60));
    assert_eq!(s.next_after(50 * 60), Some(60 * 60));
}

#[test]
fn cron_top_of_even_hours_golden() {
    let s = Schedule::cron("0 */2 * * *").expect("valid cron");
    // After 00:00, the next even-hour top is 02:00 = 7200.
    assert_eq!(s.next_after(0), Some(2 * 3600));
    assert_eq!(s.next_after(2 * 3600), Some(4 * 3600));
}

#[test]
fn cron_weekday_morning_golden() {
    // 30 9 * * 1-5 = 09:30 on Mon-Fri. 1970-01-01 was a Thursday (dow=4).
    let s = Schedule::cron("30 9 * * 1-5").expect("valid cron");
    // Epoch 0 is Thu; 09:30 Thu = 9*3600 + 30*60 = 34200.
    assert_eq!(s.next_after(0), Some(34_200));
}

#[test]
fn invalid_cron_is_structured_error_not_panic() {
    // Out-of-range minute.
    assert!(matches!(
        CronExpr::parse("60 * * * *"),
        Err(ScheduleError::OutOfRange { .. })
    ));
    // Wrong field count.
    assert!(matches!(
        CronExpr::parse("* * *"),
        Err(ScheduleError::FieldCount { got: 3, .. })
    ));
    // Non-numeric field.
    assert!(matches!(
        CronExpr::parse("bad * * * *"),
        Err(ScheduleError::BadField { .. })
    ));
}

// --- LAST_RUN() rewrite ---

#[test]
fn last_run_rewrites_to_boundary_literal() {
    let src = DO_LAST_RUN;
    let mut stmt = parse_statement(src).expect("parses");
    assert!(references_last_run(&stmt));
    bind_last_run(&mut stmt, 12_345);
    // After the rewrite the call is gone (resolved to a literal leaf).
    assert!(!references_last_run(&stmt));
    // The literal 12345 appears in the serialized AST.
    let canonical = serde_json::to_string(&stmt).expect("serializes");
    assert!(canonical.contains("12345"));
}

#[test]
fn last_run_sentinel_on_first_run() {
    let src = DO_LAST_RUN;
    let mut stmt = parse_statement(src).expect("parses");
    bind_last_run(&mut stmt, 0);
    assert!(!references_last_run(&stmt));
}

#[test]
fn dispatch_previews_plan_with_last_run_resolved() {
    // A JOB whose DO body references LAST_RUN(): dispatching against a MemJobStore produces a
    // committed run (PREVIEW path via RecordingCommitter) and the boundary resolves to the stored
    // high-water mark.
    let store = MemJobStore::new();
    let j = job(
        "incremental",
        Schedule::every(3600).expect("valid"),
        DO_LAST_RUN,
        MissedPolicy::Coalesce,
    );
    store.put_binding(j.clone());
    store.put_state(
        "incremental",
        RunState {
            last_run_at: Some(1000),
            last_status: RunStatus::Success,
            last_plan_hash: None,
            consecutive_failures: 0,
        },
    );
    let sched = Scheduler::new(
        store,
        MockClock::new(10_000),
        allowing_committer(engine_with_mock()),
    );
    let d = sched.dispatch(&j, 7200);
    assert_eq!(d.status, RunStatus::Success);
    assert!(d.committed);
    // last_run_at advanced to scheduled_for (7200), NOT now (10000).
    let st = sched.store().run_state("incremental").expect("state");
    assert_eq!(st.last_run_at, Some(7200));
    assert!(st.last_plan_hash.is_some());
}

// --- idempotency ---

#[test]
fn retried_run_id_after_success_is_noop() {
    let store = MemJobStore::new();
    let j = job(
        "j",
        Schedule::every(3600).expect("valid"),
        DO_INSERT,
        MissedPolicy::Coalesce,
    );
    store.put_binding(j.clone());
    let sched = Scheduler::new(
        store,
        MockClock::new(10_000),
        allowing_committer(Engine::new()),
    );

    let first = sched.dispatch(&j, 3600);
    assert!(first.committed);
    let again = sched.dispatch(&j, 3600); // same scheduled_for -> same run_id
    assert!(
        !again.committed,
        "retried run_id after success must be a no-op"
    );
    // Only one Success ledger entry (the second is a dedup no-op, records nothing).
    let successes = sched
        .store()
        .ledger()
        .into_iter()
        .filter(|r| matches!(r.status, RunStatus::Success))
        .count();
    assert_eq!(successes, 1);
}

#[test]
fn concurrent_dispatch_same_due_commits_once() {
    // Model two concurrent dispatches: the first acquires the lease and holds it (we do NOT
    // release between), the second sees the held lease and no-ops. We simulate by acquiring the
    // lease manually before the second dispatch.
    let store = MemJobStore::new();
    let j = job(
        "j",
        Schedule::every(3600).expect("valid"),
        DO_INSERT,
        MissedPolicy::Coalesce,
    );
    store.put_binding(j.clone());

    let run_id = run_id_for("j", 3600);
    // Replica A holds the lease (the in-flight commit).
    let lease_a = store.acquire_lease("j", &run_id, 300).expect("acquire");
    assert!(lease_a.acquired);

    // Replica B dispatches the same due time -> lease lost -> no-op.
    let sched_b = Scheduler::new(
        store,
        MockClock::new(10_000),
        allowing_committer(Engine::new()),
    );
    let d_b = sched_b.dispatch(&j, 3600);
    assert!(
        !d_b.committed,
        "second concurrent dispatch must no-op (lease lost)"
    );
    // No Success ledger entry yet (replica A hasn't recorded; B no-opped).
    let successes = sched_b
        .store()
        .ledger()
        .into_iter()
        .filter(|r| matches!(r.status, RunStatus::Success))
        .count();
    assert_eq!(successes, 0);
}

// --- missed-run policy ---

fn due_count(missed: MissedPolicy, last: i64, now: i64) -> usize {
    let s = Schedule::every(3600).expect("valid");
    missed.due_set(&s, Some(last), now).len()
}

#[test]
fn missed_policy_skip_yields_one() {
    // last_run 5 intervals behind now.
    assert_eq!(due_count(MissedPolicy::Skip, 0, 5 * 3600), 1);
}

#[test]
fn missed_policy_coalesce_yields_one() {
    assert_eq!(due_count(MissedPolicy::Coalesce, 0, 5 * 3600), 1);
}

#[test]
fn missed_policy_catchup_capped() {
    // 5 intervals due, cap at 3 -> 3.
    assert_eq!(due_count(MissedPolicy::CatchUp { max: 3 }, 0, 5 * 3600), 3);
    // cap higher than available -> all available (5).
    assert_eq!(due_count(MissedPolicy::CatchUp { max: 10 }, 0, 5 * 3600), 5);
}

// --- first-fire on first eligibility (Obs-3: look-back derives from the schedule, not a fixed
// 24h window) ---

#[test]
fn first_fire_for_interval_larger_than_a_day() {
    // EVERY '7d' (604800s), never run. At a `now` past the first weekly boundary, the JOB must
    // fire on first eligibility — NOT defer up to a full interval (the fixed-24h-window bug).
    const WEEK: i64 = 7 * 24 * 3600;
    let s = Schedule::every(WEEK).expect("valid");
    // anchor = 0; first boundary at epoch 0, next at WEEK, etc. now is 1.5 weeks in.
    let now = WEEK + WEEK / 2;
    let due = MissedPolicy::Coalesce.due_set(&s, None, now);
    assert_eq!(
        due.len(),
        1,
        "first-run fires exactly once on first eligibility"
    );
    // The fired boundary is the most recent weekly boundary at-or-before now (1*WEEK), proving the
    // look-back reached back a FULL week (a fixed 24h window would have found nothing and skipped).
    assert_eq!(due[0], WEEK);
}

#[test]
fn first_fire_before_first_boundary_is_empty() {
    // EVERY '7d' anchored at WEEK; now is before the first boundary -> nothing due yet.
    const WEEK: i64 = 7 * 24 * 3600;
    let s = Schedule::every_anchored(WEEK, WEEK).expect("valid");
    let due = MissedPolicy::Coalesce.due_set(&s, None, WEEK / 2);
    assert!(
        due.is_empty(),
        "no fire before the cadence's first boundary"
    );
}

#[test]
fn first_fire_for_monthly_cron_past_a_day() {
    // A cron firing on the 1st of each month at 00:00 — its prior boundary can be ~a month back,
    // far beyond a 24h window. From mid-month, first-run must still fire on the prior 1st.
    let s = Schedule::cron("0 0 1 * *").expect("valid cron");
    // 1970: Jan 1 00:00 = epoch 0; Feb 1 00:00 = 31 days = 2_678_400. Pick mid-February.
    let mid_feb = 2_678_400 + 10 * 24 * 3600;
    let due = MissedPolicy::Coalesce.due_set(&s, None, mid_feb);
    assert_eq!(due.len(), 1);
    assert_eq!(
        due[0], 2_678_400,
        "first-run fires on the prior month-start, weeks back"
    );
}

// --- last_run_at advance ordering + failed-run re-cover ---

#[test]
fn failed_run_leaves_last_run_at_unmoved_and_recovers() {
    let store = MemJobStore::new();
    let j = job(
        "flappy",
        Schedule::every(3600).expect("valid"),
        DO_INSERT,
        MissedPolicy::Coalesce,
    );
    store.put_binding(j.clone());
    store.put_state(
        "flappy",
        RunState {
            last_run_at: Some(1000),
            ..RunState::default()
        },
    );
    let sched = Scheduler::new(
        store,
        MockClock::new(10_000),
        RecordingCommitter::failing("boom"),
    );
    let d = sched.dispatch(&j, 7200);
    assert_eq!(d.status, RunStatus::Failed);
    assert!(!d.committed);
    // last_run_at unchanged (still 1000): the window re-covers next tick.
    let st = sched.store().run_state("flappy").expect("state");
    assert_eq!(st.last_run_at, Some(1000));
    assert_eq!(st.consecutive_failures, 1);
}

// --- one audit entry per fire ---

#[test]
fn one_audit_entry_per_fire() {
    let store = MemJobStore::new();
    let j = job(
        "j",
        Schedule::every(3600).expect("valid"),
        DO_INSERT,
        MissedPolicy::Coalesce,
    );
    store.put_binding(j.clone());
    let sched = Scheduler::new(
        store,
        MockClock::new(10_000),
        allowing_committer(Engine::new()),
    );
    sched.dispatch(&j, 3600);
    assert_eq!(sched.store().ledger().len(), 1);
}

// --- circuit-breaker ---

#[test]
fn circuit_breaker_auto_disables_after_repeated_failures() {
    let store = MemJobStore::new();
    let j = job(
        "flappy",
        Schedule::every(3600).expect("valid"),
        DO_INSERT,
        MissedPolicy::Coalesce,
    );
    store.put_binding(j.clone());
    let sched = Scheduler::new(
        store,
        MockClock::new(10_000),
        RecordingCommitter::failing("boom"),
    );
    // Fire 6 distinct boundaries (each a distinct run_id) -> after the 5th failure the breaker
    // trips and a Disabled note is recorded.
    for n in 1..=6 {
        sched.dispatch(&j, n * 3600);
    }
    let disabled = sched
        .store()
        .ledger()
        .into_iter()
        .any(|r| matches!(r.status, RunStatus::Disabled));
    assert!(disabled, "circuit-breaker must record a Disabled note");
}

// --- log-scrub: no secrets / DO payloads in the audit projection ---

#[test]
fn audit_record_carries_no_plan_payload() {
    let store = MemJobStore::new();
    // A DO body with a recognizable token-like literal that must NOT appear in the ledger.
    let secretish = "UPSERT INTO /mock/sink VALUES ('SECRET-abc123')";
    let j = job(
        "j",
        Schedule::every(3600).expect("valid"),
        secretish,
        MissedPolicy::Coalesce,
    );
    store.put_binding(j.clone());
    let sched = Scheduler::new(
        store,
        MockClock::new(10_000),
        allowing_committer(Engine::new()),
    );
    let d = sched.dispatch(&j, 3600);
    assert_eq!(d.status, RunStatus::Success);
    for record in sched.store().ledger() {
        let projection = record.log_line();
        assert!(
            !projection.contains("SECRET-abc123"),
            "audit log_line must not contain the DO payload"
        );
        // The full Debug of the record also must not carry the payload.
        let dbg = format!("{record:?}");
        assert!(
            !dbg.contains("SECRET-abc123"),
            "RunRecord Debug must not contain the DO payload"
        );
    }
}

// --- tick end-to-end over the store ---

#[test]
fn tick_dispatches_due_enabled_jobs() {
    let store = MemJobStore::new();
    let j = job(
        "nightly",
        Schedule::every(3600).expect("valid"),
        DO_INSERT,
        MissedPolicy::Coalesce,
    );
    store.put_binding(j);
    store.put_state(
        "nightly",
        RunState {
            last_run_at: Some(0),
            ..RunState::default()
        },
    );
    let sched = Scheduler::new(
        store,
        MockClock::new(3 * 3600),
        allowing_committer(Engine::new()),
    );
    let dispatched = sched.tick();
    assert_eq!(dispatched.len(), 1, "Coalesce folds the gap to one fire");
    assert!(dispatched[0].committed);
}

#[test]
fn deterministic_run_id_is_stable() {
    assert_eq!(run_id_for("j", 3600), run_id_for("j", 3600));
    assert_ne!(run_id_for("j", 3600), run_id_for("j", 7200));
    assert_ne!(run_id_for("a", 3600), run_id_for("b", 3600));
    assert!(run_id_for("j", 3600).starts_with("run-"));
}
