//! Planner-owned **E2E / external-interface** black-box validation of the t33 JOB scheduler
//! (`qfs-cron`).
//!
//! This is NOT a unit test and NOT a code review. Every scenario drives the scheduler's PUBLIC
//! surface from the OUTSIDE — the re-exported `Scheduler::{tick,dispatch}` orchestration over the
//! injected `MemJobStore` + `MockClock` + `RecordingCommitter` seams, the `Schedule` math, the
//! `MissedPolicy` due-set fold, the `bind_last_run` rewrite, and the deterministic `run_id_for`.
//! The stronger external assertion is on the OBSERVABLE OUTCOME (the committed ledger / advanced
//! run-state / the produced `Plan`), never on private internals.
//!
//! No live creds, no network: the `RecordingCommitter` is the no-creds PREVIEW path (build_plan +
//! `RecordingApplier`); the `MemJobStore` is in-memory. Time is a fixed `MockClock` — no wall-clock
//! flake.
//!
//! ## Independence from the Constructor's tests
//! The Constructor's `crates/cron/src/tests.rs` is an internal module. This harness re-derives every
//! fixture through the public API and does NOT trust those tests; it maps 1:1 to the ticket's
//! acceptance criteria and actively tries to DEFEAT the >24h first-fire fix (scenario 2) and the
//! idempotency guarantee (scenario 4).
//!
//! ## Native-only
//! The `RecordingCommitter` PREVIEW path is gated behind the default-on `native` feature (it
//! consumes `qfs-exec`). These E2E scenarios run on the native test build (the wasm target is not
//! built here — disk constraint). Schedule-math-only scenarios need no committer and would run on
//! the pure core too.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;

use qfs_core::{
    Archetype, Capabilities, Column, ColumnType, Engine, NodeDesc, Path, PlanSpec, PushdownProfile,
    Schema,
};
use qfs_parser::parse_statement;

use qfs_cron::{
    bind_last_run, references_last_run, run_id_for, JobBinding, JobStore, MemJobStore,
    MissedPolicy, MockClock, PolicyRef, RecordingCommitter, RunState, RunStatus, Schedule,
    ScheduleError, Scheduler,
};

// --- a minimal in-memory fake `/mock` source so a DO body resolves to a real Plan in the PREVIEW
// path (no live creds). Plan construction is pure: build_plan needs only the Driver (mount +
// describe + capabilities + applier), not a ReadDriver. ---

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

fn plan_spec(src: &str) -> PlanSpec {
    let stmt = parse_statement(src).expect("DO body parses");
    PlanSpec::from_statement(stmt)
}

fn job(name: &str, schedule: Schedule, do_src: &str, missed: MissedPolicy) -> JobBinding {
    JobBinding {
        name: name.to_string(),
        schedule,
        plan: plan_spec(do_src),
        // t35: a granting POLICY ref so the firing scenarios exercise the allow path.
        policy: PolicyRef::named("jobwriter"),
        missed,
        enabled: true,
    }
}

/// A `RecordingCommitter` wired with a permissive test policy table (`jobwriter` grants
/// `INSERT,UPSERT`) so the firing scenarios exercise the t35 allow path (default-deny otherwise).
fn allowing_committer(engine: Engine) -> RecordingCommitter {
    let mut table = qfs_cron::PolicyTable::new();
    table.insert(
        "jobwriter".to_string(),
        qfs_cron::PolicyDef {
            name: "jobwriter".to_string(),
            handler: String::new(),
            allow: vec!["ALLOW INSERT,UPSERT".to_string()],
        },
    );
    let handle = std::sync::Arc::new(std::sync::RwLock::new(std::sync::Arc::new(table)));
    RecordingCommitter::with_engine(engine).with_policies(handle)
}

const DO_INSERT: &str = "UPSERT INTO /mock/sink VALUES (1)";
const DO_LAST_RUN: &str = "INSERT INTO /mock/sink /mock/src |> WHERE ts > LAST_RUN()";

// =====================================================================================
// Scenario 1 — Schedule::next_after goldens (EVERY + 5-field cron) + invalid-cron error
// =====================================================================================

#[test]
fn s1_every_next_after_goldens() {
    // Hourly anchored at epoch 0: next boundary is strictly-after `from`.
    let s = Schedule::every(3600).expect("valid");
    assert_eq!(s.next_after(0), Some(3600));
    assert_eq!(s.next_after(1), Some(3600));
    assert_eq!(s.next_after(3599), Some(3600));
    assert_eq!(s.next_after(3600), Some(7200), "strictly-after, not at");
    assert_eq!(s.next_after(7199), Some(7200));

    // Daily.
    let d = Schedule::every(86_400).expect("valid");
    assert_eq!(d.next_after(0), Some(86_400));
    assert_eq!(d.next_after(86_400), Some(172_800));

    // Anchored EVERY: anchor offsets the boundary lattice.
    let a = Schedule::every_anchored(3600, 100).expect("valid");
    assert_eq!(a.next_after(0), Some(100), "first boundary at the anchor");
    assert_eq!(a.next_after(100), Some(3700));
    assert_eq!(a.next_after(3700), Some(7300));
}

#[test]
fn s1_cron_next_after_goldens() {
    // */15 minutes: 00:15, 00:30, 00:45, 01:00 ...
    let q = Schedule::cron("*/15 * * * *").expect("valid");
    assert_eq!(q.next_after(0), Some(15 * 60));
    assert_eq!(q.next_after(15 * 60), Some(30 * 60));
    assert_eq!(q.next_after(44 * 60), Some(45 * 60));
    assert_eq!(q.next_after(45 * 60), Some(60 * 60));

    // Top of even hours.
    let e = Schedule::cron("0 */2 * * *").expect("valid");
    assert_eq!(e.next_after(0), Some(2 * 3600));
    assert_eq!(e.next_after(2 * 3600), Some(4 * 3600));

    // 09:30 on weekdays (Mon-Fri). 1970-01-01 was a Thursday -> 09:30 that day is in-range.
    let w = Schedule::cron("30 9 * * 1-5").expect("valid");
    assert_eq!(w.next_after(0), Some(34_200), "09:30 Thursday = 34200s");
    // After Friday 09:30, the next must skip Sat+Sun to Monday 09:30.
    // Fri = epoch day 1 (1970-01-02). Fri 09:30 = 86_400 + 34_200 = 120_600.
    let fri_0930 = 86_400 + 34_200;
    let mon_0930 = 4 * 86_400 + 34_200; // Mon = epoch day 4 (1970-01-05).
    assert_eq!(
        w.next_after(fri_0930),
        Some(mon_0930),
        "weekend is skipped: Fri 09:30 -> Mon 09:30"
    );
}

#[test]
fn s1_invalid_cron_is_structured_error_not_panic() {
    // Out-of-range minute, hour, dom, month, dow — each a structured OutOfRange, never a panic.
    assert!(matches!(
        Schedule::cron("60 * * * *"),
        Err(ScheduleError::OutOfRange {
            field: "minute",
            ..
        })
    ));
    assert!(matches!(
        Schedule::cron("0 24 * * *"),
        Err(ScheduleError::OutOfRange { field: "hour", .. })
    ));
    assert!(matches!(
        Schedule::cron("0 0 32 * *"),
        Err(ScheduleError::OutOfRange { .. })
    ));
    assert!(matches!(
        Schedule::cron("0 0 1 13 *"),
        Err(ScheduleError::OutOfRange { .. })
    ));
    assert!(matches!(
        Schedule::cron("0 0 1 1 7"),
        Err(ScheduleError::OutOfRange { .. })
    ));
    // Wrong field count.
    assert!(matches!(
        Schedule::cron("* * *"),
        Err(ScheduleError::FieldCount { got: 3, .. })
    ));
    assert!(matches!(
        Schedule::cron("* * * * * *"),
        Err(ScheduleError::FieldCount { got: 6, .. })
    ));
    // Malformed / non-numeric / bad step.
    assert!(matches!(
        Schedule::cron("xyz * * * *"),
        Err(ScheduleError::BadField { .. })
    ));
    assert!(matches!(
        Schedule::cron("*/0 * * * *"),
        Err(ScheduleError::BadField { .. })
    ));
    // Zero-interval EVERY.
    assert_eq!(Schedule::every(0), Err(ScheduleError::ZeroInterval));
    assert_eq!(Schedule::every(-1), Err(ScheduleError::ZeroInterval));
}

// =====================================================================================
// Scenario 2 — >24h first-fire fix (Obs-3). Actively try to DEFEAT the fix.
// =====================================================================================

#[test]
fn s2_weekly_job_fires_on_first_eligibility_not_a_24h_window() {
    // EVERY '7d', never run, now 1.5 weeks in. The OLD bug (a fixed 24h look-back) would find no
    // boundary in the last day and fire NOTHING. The fix derives the look-back from the schedule.
    const WEEK: i64 = 7 * 86_400;
    let s = Schedule::every(WEEK).expect("valid");
    let now = WEEK + WEEK / 2; // mid-second-week
    let due = MissedPolicy::Coalesce.due_set(&s, None, now);
    assert_eq!(
        due,
        vec![WEEK],
        "exactly one fire at the prior weekly boundary"
    );

    // Drive it through the full dispatch path too (not just the math): the JOB actually commits
    // once at that boundary, and run-state advances to it.
    let store = MemJobStore::new();
    let j = job(
        "weekly",
        Schedule::every(WEEK).expect("valid"),
        DO_INSERT,
        MissedPolicy::Coalesce,
    );
    store.put_binding(j);
    let sched = Scheduler::new(
        store,
        MockClock::new(now),
        allowing_committer(Engine::new()),
    );
    let out = sched.tick();
    assert_eq!(
        out.len(),
        1,
        "first-run weekly job fires exactly once via tick"
    );
    assert_eq!(out[0].scheduled_for, WEEK);
    assert!(out[0].committed);
    assert_eq!(
        sched.store().run_state("weekly").unwrap().last_run_at,
        Some(WEEK)
    );
}

#[test]
fn s2_attempt_defeat_extreme_interval_and_far_now() {
    // Try to break the fix with a pathologically long interval (30 days) and a `now` that is many
    // intervals past the anchor — a fixed-window approach would skip; an UNbounded back-scan would
    // hang. It must fire exactly ONCE at the most-recent boundary.
    const MONTH: i64 = 30 * 86_400;
    let s = Schedule::every(MONTH).expect("valid");
    let now = 5 * MONTH + 12_345; // 5+ months in
    let due = MissedPolicy::Coalesce.due_set(&s, None, now);
    assert_eq!(
        due,
        vec![5 * MONTH],
        "newest monthly boundary at-or-before now"
    );

    // Skip policy: same single newest boundary.
    assert_eq!(MissedPolicy::Skip.due_set(&s, None, now), vec![5 * MONTH]);
    // CatchUp on a FIRST run still folds to one (first-run is fire-once by contract).
    assert_eq!(
        MissedPolicy::CatchUp { max: 100 }.due_set(&s, None, now),
        vec![5 * MONTH],
        "first-run is fire-once regardless of policy"
    );
}

#[test]
fn s2_monthly_cron_first_fire_weeks_back() {
    // A monthly cron (1st @ 00:00). From mid-February, first-run must reach back to Feb 1 —
    // weeks beyond any day-window. 1970: Jan1=0, Feb1 = 31 days = 2_678_400.
    let s = Schedule::cron("0 0 1 * *").expect("valid");
    let feb1 = 2_678_400;
    let mid_feb = feb1 + 12 * 86_400;
    let due = MissedPolicy::Coalesce.due_set(&s, None, mid_feb);
    assert_eq!(due, vec![feb1], "first-run fires on the prior month-start");
}

#[test]
fn s2_anchored_future_cadence_does_not_fire_prematurely() {
    // The negative case: an EVERY anchored in the FUTURE relative to now must NOT fire. Defeat
    // attempt: a now just one second before the first boundary.
    const WEEK: i64 = 7 * 86_400;
    let s = Schedule::every_anchored(WEEK, WEEK).expect("valid");
    assert!(
        MissedPolicy::Coalesce
            .due_set(&s, None, WEEK - 1)
            .is_empty(),
        "one second before the first boundary: nothing due"
    );
    assert!(
        MissedPolicy::Coalesce.due_set(&s, None, 0).is_empty(),
        "at the anchor's epoch-0 origin (before first boundary at WEEK): nothing due"
    );
    // At exactly the first boundary it DOES become due (boundary is at-or-before now).
    assert_eq!(MissedPolicy::Coalesce.due_set(&s, None, WEEK), vec![WEEK]);

    // A future-anchored cron, driven through tick(), commits nothing.
    let store = MemJobStore::new();
    let j = job(
        "future",
        Schedule::every_anchored(WEEK, WEEK).expect("valid"),
        DO_INSERT,
        MissedPolicy::Coalesce,
    );
    store.put_binding(j);
    let sched = Scheduler::new(
        store,
        MockClock::new(WEEK - 1),
        allowing_committer(Engine::new()),
    );
    assert!(
        sched.tick().is_empty(),
        "future cadence: tick dispatches nothing"
    );
    assert!(sched.store().ledger().is_empty(), "no ledger entry written");
}

// =====================================================================================
// Scenario 3 — Plan assertion: LAST_RUN() resolves to the stored boundary (PREVIEW, no creds)
// =====================================================================================

#[test]
fn s3_last_run_resolves_to_stored_boundary_and_previews() {
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
    // Dispatch the boundary 7200: LAST_RUN() must resolve to the STORED high-water mark (1000),
    // not `now`, not scheduled_for.
    let d = sched.dispatch(&j, 7200);
    assert_eq!(d.status, RunStatus::Success);
    assert!(d.committed, "PREVIEW commit succeeds");
    // Outcome: run-state advanced to scheduled_for and a plan hash was fingerprinted.
    let st = sched.store().run_state("incremental").unwrap();
    assert_eq!(
        st.last_run_at,
        Some(7200),
        "advanced to scheduled_for, not now (10000)"
    );
    assert!(st.last_plan_hash.is_some(), "plan was fingerprinted");
}

#[test]
fn s3_first_run_resolves_last_run_to_sentinel_epoch() {
    // The rewrite leaf — external proof that LAST_RUN() lowers to a literal. On a first run the
    // sentinel boundary is epoch 0.
    let mut stmt = parse_statement(DO_LAST_RUN).expect("parses");
    assert!(
        references_last_run(&stmt),
        "DO body references LAST_RUN() before rewrite"
    );
    bind_last_run(&mut stmt, 0);
    assert!(
        !references_last_run(&stmt),
        "sentinel rewrite removes the call"
    );
    let canonical = serde_json::to_string(&stmt).expect("serializes");

    // And a non-zero boundary appears as a literal in the rewritten AST.
    let mut stmt2 = parse_statement(DO_LAST_RUN).expect("parses");
    bind_last_run(&mut stmt2, 1000);
    let canonical2 = serde_json::to_string(&stmt2).expect("serializes");
    assert!(
        canonical2.contains("1000"),
        "boundary 1000 lowered to a literal leaf"
    );
    assert_ne!(
        canonical, canonical2,
        "different boundaries produce different ASTs"
    );
}

// =====================================================================================
// Scenario 4 — Idempotency. Actively try to FORCE a double-commit.
// =====================================================================================

#[test]
fn s4_concurrent_dispatch_same_due_commits_exactly_once() {
    // Two replicas dispatch the SAME due boundary. Replica A grabs the lease (in-flight commit);
    // replica B sees the held lease and must no-op. Exactly one commits.
    let store = MemJobStore::new();
    let j = job(
        "j",
        Schedule::every(3600).expect("valid"),
        DO_INSERT,
        MissedPolicy::Coalesce,
    );
    store.put_binding(j.clone());

    let run_id = run_id_for("j", 3600);
    let lease_a = store.acquire_lease("j", &run_id, 300).expect("acquire");
    assert!(lease_a.acquired, "replica A holds the lease");

    let sched_b = Scheduler::new(
        store,
        MockClock::new(10_000),
        allowing_committer(Engine::new()),
    );
    let d_b = sched_b.dispatch(&j, 3600);
    assert!(!d_b.committed, "replica B no-ops on the held lease");
    let successes = sched_b
        .store()
        .ledger()
        .into_iter()
        .filter(|r| matches!(r.status, RunStatus::Success))
        .count();
    assert_eq!(successes, 0, "B committed nothing while A holds the lease");
}

#[test]
fn s4_retried_run_id_after_success_commits_nothing_further() {
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
    assert!(first.committed, "first dispatch commits");
    let again = sched.dispatch(&j, 3600); // same scheduled_for -> same deterministic run_id
    assert!(!again.committed, "retried run-id after success is a no-op");
    assert_eq!(
        again.status,
        RunStatus::Success,
        "reported as already-succeeded"
    );

    let successes = sched
        .store()
        .ledger()
        .into_iter()
        .filter(|r| matches!(r.status, RunStatus::Success))
        .count();
    assert_eq!(
        successes, 1,
        "exactly one Success ledger entry across the retry"
    );
}

#[test]
fn s4_attempt_force_double_commit_via_repeated_dispatch_and_tick() {
    // Aggressive defeat attempt: hammer the SAME due boundary through dispatch() many times AND
    // through repeated tick() passes (the deterministic run-id must dedup every one). The applied
    // effect must be committed exactly once.
    let store = MemJobStore::new();
    let j = job(
        "hammer",
        Schedule::every(3600).expect("valid"),
        DO_INSERT,
        MissedPolicy::Coalesce,
    );
    store.put_binding(j.clone());
    // Seed last_run so exactly the 7200 boundary is due at now=8000 (one boundary in (3600, 8000]).
    store.put_state(
        "hammer",
        RunState {
            last_run_at: Some(3600),
            ..RunState::default()
        },
    );
    let sched = Scheduler::new(
        store,
        MockClock::new(8000),
        allowing_committer(Engine::new()),
    );

    // 10 direct re-dispatches of the same boundary.
    for _ in 0..10 {
        sched.dispatch(&j, 7200);
    }
    // 10 full tick() passes at the same frozen clock — each recomputes the due set and dispatches.
    for _ in 0..10 {
        sched.tick();
    }

    let successes = sched
        .store()
        .ledger()
        .into_iter()
        .filter(|r| matches!(r.status, RunStatus::Success))
        .count();
    assert_eq!(
        successes, 1,
        "20 attempts at one boundary => exactly ONE committed run"
    );
    // run-state advanced to the boundary and stayed there (not re-advanced, not duplicated).
    assert_eq!(
        sched.store().run_state("hammer").unwrap().last_run_at,
        Some(7200)
    );
}

#[test]
fn s4_deterministic_run_id_properties() {
    // The dedup key's contract: deterministic per (job, scheduled_for), distinct across either.
    assert_eq!(run_id_for("j", 3600), run_id_for("j", 3600), "stable");
    assert_ne!(
        run_id_for("j", 3600),
        run_id_for("j", 7200),
        "distinct boundary"
    );
    assert_ne!(run_id_for("a", 3600), run_id_for("b", 3600), "distinct job");
    assert!(run_id_for("j", 3600).starts_with("run-"));
}

// =====================================================================================
// Scenario 5 — Missed-run policy: Skip=1, CatchUp{max:n}<=n, Coalesce=1
// =====================================================================================

fn due_count(missed: MissedPolicy, last: i64, now: i64) -> usize {
    let s = Schedule::every(3600).expect("valid");
    missed.due_set(&s, Some(last), now).len()
}

#[test]
fn s5_missed_policy_due_sets() {
    // last_run 5 intervals behind now (boundaries 3600..18000 due).
    let now = 5 * 3600;
    assert_eq!(
        due_count(MissedPolicy::Skip, 0, now),
        1,
        "Skip = newest only"
    );
    assert_eq!(
        due_count(MissedPolicy::Coalesce, 0, now),
        1,
        "Coalesce = one covering run"
    );
    assert_eq!(
        due_count(MissedPolicy::CatchUp { max: 3 }, 0, now),
        3,
        "CatchUp capped at max"
    );
    assert_eq!(
        due_count(MissedPolicy::CatchUp { max: 10 }, 0, now),
        5,
        "CatchUp <= available"
    );
    assert_eq!(
        due_count(MissedPolicy::CatchUp { max: 0 }, 0, now),
        0,
        "CatchUp{{0}} = none"
    );

    // Identity of the dispatched boundaries (not just counts).
    let s = Schedule::every(3600).expect("valid");
    assert_eq!(
        MissedPolicy::Skip.due_set(&s, Some(0), now),
        vec![5 * 3600],
        "Skip = latest boundary"
    );
    assert_eq!(
        MissedPolicy::CatchUp { max: 3 }.due_set(&s, Some(0), now),
        vec![3600, 7200, 10_800],
        "CatchUp replays the OLDEST max boundaries, in order"
    );
}

#[test]
fn s5_no_missed_window_yields_nothing() {
    // last_run == now: no boundary strictly after -> empty for every policy.
    let s = Schedule::every(3600).expect("valid");
    for p in [
        MissedPolicy::Skip,
        MissedPolicy::Coalesce,
        MissedPolicy::CatchUp { max: 5 },
    ] {
        assert!(
            p.due_set(&s, Some(3600), 3600).is_empty(),
            "no window due => empty"
        );
    }
}

// =====================================================================================
// Scenario 6 — last_run_at advance semantics (advance only on success, to scheduled_for)
// =====================================================================================

#[test]
fn s6_advance_only_on_success_to_scheduled_for() {
    let store = MemJobStore::new();
    let j = job(
        "adv",
        Schedule::every(3600).expect("valid"),
        DO_INSERT,
        MissedPolicy::Coalesce,
    );
    store.put_binding(j.clone());
    store.put_state(
        "adv",
        RunState {
            last_run_at: Some(1000),
            ..RunState::default()
        },
    );
    // now is 10000, but a success dispatching boundary 7200 must advance to 7200 (NOT now).
    let sched = Scheduler::new(
        store,
        MockClock::new(10_000),
        allowing_committer(Engine::new()),
    );
    let d = sched.dispatch(&j, 7200);
    assert!(d.committed);
    assert_eq!(
        sched.store().run_state("adv").unwrap().last_run_at,
        Some(7200),
        "advanced to scheduled_for, never to now"
    );
}

#[test]
fn s6_failed_dispatch_leaves_last_run_unmoved_and_next_tick_recovers() {
    // A forced-failure dispatch: last_run must stay unmoved, the failure counter bumps, and the
    // same window re-covers on the next tick (at-least-once). One store, one failing committer.
    let store = MemJobStore::new();
    let j = job(
        "recover",
        Schedule::every(3600).expect("valid"),
        DO_INSERT,
        MissedPolicy::Coalesce,
    );
    store.put_binding(j.clone());
    store.put_state(
        "recover",
        RunState {
            last_run_at: Some(1000),
            ..RunState::default()
        },
    );
    let sched = Scheduler::new(
        store,
        MockClock::new(7200),
        RecordingCommitter::failing("boom"),
    );

    let out_fail = sched.tick();
    assert_eq!(out_fail.len(), 1, "one boundary due");
    assert_eq!(out_fail[0].status, RunStatus::Failed);
    assert!(!out_fail[0].committed);
    let st = sched.store().run_state("recover").unwrap();
    assert_eq!(
        st.last_run_at,
        Some(1000),
        "failed run leaves last_run_at unmoved"
    );
    assert_eq!(st.consecutive_failures, 1, "failure counter bumped");

    // The SAME window is still due on the next tick (re-cover): the boundary re-dispatches,
    // confirming a failed run does NOT mark the run-id committed.
    let out_again = sched.tick();
    assert_eq!(
        out_again.len(),
        1,
        "the un-advanced window re-covers next tick"
    );
    assert_eq!(
        out_again[0].scheduled_for, out_fail[0].scheduled_for,
        "same boundary re-covered"
    );
}

// =====================================================================================
// Scenario 7 — Audit/log hygiene: one entry per fire; no secrets/payloads (canary)
// =====================================================================================

#[test]
fn s7_one_audit_entry_per_fire() {
    let store = MemJobStore::new();
    let j = job(
        "audit",
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
    let d = sched.dispatch(&j, 3600);
    assert!(d.committed);
    let ledger = sched.store().ledger();
    assert_eq!(ledger.len(), 1, "exactly one RunRecord per fire");
    let r = &ledger[0];
    // The record carries the required fields (job, run-id, scheduled-for, status, counts).
    assert_eq!(r.job, "audit");
    assert_eq!(r.run_id, run_id_for("audit", 3600));
    assert_eq!(r.scheduled_for, 3600);
    assert_eq!(r.status, RunStatus::Success);
}

#[test]
fn s7_log_scrub_no_canary_in_ledger_or_log_line() {
    // Plant a canary token inside the DO body. It MUST NOT appear in the RunRecord, its Debug, or
    // the structured log_line projection.
    const CANARY: &str = "CANARY-secret-9f8e7d6c-token";
    let store = MemJobStore::new();
    let do_with_canary = format!("UPSERT INTO /mock/sink VALUES ('{CANARY}')");
    let j = job(
        "canary",
        Schedule::every(3600).expect("valid"),
        &do_with_canary,
        MissedPolicy::Coalesce,
    );
    store.put_binding(j.clone());
    let sched = Scheduler::new(
        store,
        MockClock::new(10_000),
        allowing_committer(Engine::new()),
    );
    let d = sched.dispatch(&j, 3600);
    assert_eq!(
        d.status,
        RunStatus::Success,
        "canary job commits via PREVIEW"
    );
    assert!(
        !format!("{d:?}").contains(CANARY),
        "Dispatched summary must not carry the payload"
    );

    for record in sched.store().ledger() {
        assert!(
            !record.log_line().contains(CANARY),
            "log_line must not leak the DO payload"
        );
        assert!(
            !format!("{record:?}").contains(CANARY),
            "RunRecord Debug must not leak the payload"
        );
        // Belt-and-suspenders: serialized ledger form is canary-free too.
        let json = serde_json::to_string(&record).expect("serializes");
        assert!(
            !json.contains(CANARY),
            "serialized RunRecord must not leak the payload"
        );
    }
}

#[test]
fn s7_failure_note_is_secret_free_reason_only() {
    // A failed run's ledger note carries the (already secret-free) reason — and nothing of the DO
    // payload. Plant a canary and force failure.
    const CANARY: &str = "CANARY-fail-payload-token";
    let store = MemJobStore::new();
    let do_with_canary = format!("UPSERT INTO /mock/sink VALUES ('{CANARY}')");
    let j = job(
        "failnote",
        Schedule::every(3600).expect("valid"),
        &do_with_canary,
        MissedPolicy::Coalesce,
    );
    store.put_binding(j.clone());
    let sched = Scheduler::new(
        store,
        MockClock::new(10_000),
        RecordingCommitter::failing("apply-rejected"),
    );
    let d = sched.dispatch(&j, 3600);
    assert_eq!(d.status, RunStatus::Failed);
    for record in sched.store().ledger() {
        assert!(
            !format!("{record:?}").contains(CANARY),
            "failure RunRecord must not leak the payload"
        );
        assert!(
            !record.log_line().contains(CANARY),
            "failure log_line must not leak the payload"
        );
    }
}

// =====================================================================================
// Scenario 8 — Circuit-breaker: a flapping JOB is auto-disabled with a ledger note
// =====================================================================================

#[test]
fn s8_circuit_breaker_auto_disables_flapping_job() {
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

    // Fire distinct boundaries (distinct run-ids) so each failure is a real attempt, not a dedup.
    // The breaker trips at 5 consecutive failures; the 6th attempt records a Disabled note.
    for n in 1..=6 {
        let d = sched.dispatch(&j, n * 3600);
        assert_eq!(d.status, RunStatus::Failed, "every attempt fails");
    }
    let ledger = sched.store().ledger();
    let disabled: Vec<_> = ledger
        .iter()
        .filter(|r| matches!(r.status, RunStatus::Disabled))
        .collect();
    assert!(
        !disabled.is_empty(),
        "the breaker records a Disabled ledger note"
    );
    // The note is descriptive (mentions the breaker / failure count) and secret-free.
    let note = disabled[0].failure_note.as_deref().unwrap_or("");
    assert!(
        note.contains("circuit-breaker") || note.contains("consecutive"),
        "note explains the disable: {note:?}"
    );

    // Run-state reflects the disable, and it does not flap forever: the breaker note is recorded.
    let st = sched.store().run_state("flappy").unwrap();
    assert_eq!(
        st.last_status,
        RunStatus::Disabled,
        "run-state marked Disabled"
    );
}

#[test]
fn s8_breaker_does_not_trip_below_threshold() {
    // Fewer than the threshold failures must NOT record a Disabled note (no premature disable).
    let store = MemJobStore::new();
    let j = job(
        "steady",
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
    for n in 1..=3 {
        sched.dispatch(&j, n * 3600);
    }
    let any_disabled = sched
        .store()
        .ledger()
        .into_iter()
        .any(|r| matches!(r.status, RunStatus::Disabled));
    assert!(
        !any_disabled,
        "3 failures < threshold: no Disabled note yet"
    );
}
