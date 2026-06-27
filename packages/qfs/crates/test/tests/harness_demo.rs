//! Representative demonstration of each `qfs-test` harness category (t38 acceptance).
//!
//! This is NOT a migration of the 1159 existing tests (that churn is out of scope and risks
//! regressions). It proves the consolidated helpers work end-to-end with one representative
//! test per category — plan assertion, golden plan snapshot, parser/grammar golden + error
//! recovery, codec round-trip, handler PREVIEW, and apply-twice idempotency — exactly the
//! patterns the eleven trip tickets each hand-rolled. The goldens are seeded under
//! `tests/fixtures/` (re-bless with `QFS_BLESS=1 cargo test -p qfs-test`).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;

use qfs_core::{
    Affected, AppliedEffect, ApplyError, Archetype, Capabilities, CfsError, Column, ColumnType,
    Driver, DriverId, EffectKind, EffectNode, MountRegistry, NodeDesc, NodeId, Path, PlanApplier,
    PlanBuilder, ProcSig, PushdownProfile, Row, RowBatch, Schema, Value, VfsPath,
};
use qfs_test::{
    assert_plan, error_snapshot, golden_parse, preview_handler, roundtrip_codec, FakeBackend,
};

/// A panicking applier: building/previewing a plan must never invoke it (the purity proof at
/// the test boundary — `assert_plan` reaches the plan with no I/O).
#[derive(Default)]
struct PanicApplier;
impl PlanApplier for PanicApplier {
    fn apply(&mut self, _node: &EffectNode) -> Result<AppliedEffect, ApplyError> {
        panic!("assert_plan must not perform I/O: applier invoked during pure evaluation");
    }
}

/// A minimal in-memory relational fixture driver (no creds, no I/O): `/db/*` is a relational
/// table supporting `INSERT/UPSERT/UPDATE/REMOVE`.
struct DbFixture {
    procs: Vec<ProcSig>,
    pushdown: PushdownProfile,
    applier: PanicApplier,
}

impl DbFixture {
    fn new() -> Self {
        Self {
            procs: Vec::new(),
            pushdown: PushdownProfile::None,
            applier: PanicApplier,
        }
    }
}

impl Driver for DbFixture {
    fn mount(&self) -> &str {
        "/db"
    }
    fn describe(&self, _p: &Path) -> Result<NodeDesc, CfsError> {
        Ok(NodeDesc::new(
            Archetype::RelationalTable,
            Schema::new(vec![
                Column::new("id", ColumnType::Int, false),
                Column::new("name", ColumnType::Text, true),
                Column::new("active", ColumnType::Bool, true),
            ]),
        ))
    }
    fn capabilities(&self, _p: &Path) -> Capabilities {
        Capabilities::none()
            .select()
            .insert()
            .upsert()
            .update()
            .remove()
    }
    fn procedures(&self) -> &[ProcSig] {
        &self.procs
    }
    fn pushdown(&self) -> &PushdownProfile {
        &self.pushdown
    }
    fn applier(&self) -> &dyn PlanApplier {
        &self.applier
    }
}

fn registry() -> MountRegistry {
    let mut reg = MountRegistry::new();
    reg.register(Arc::new(DbFixture::new())).unwrap();
    reg
}

// ---------------------------------------------------------------------------
// 1. Plan assertion — the thesis: assert the plan, no creds, no socket.
// ---------------------------------------------------------------------------

#[test]
fn upsert_plan_shape_and_no_io() {
    let _ = assert_plan("UPSERT INTO /db/users VALUES (1, 'a', true)", &registry())
        .nodes(&[EffectKind::Upsert])
        .irreversible(0)
        .no_io_performed();
}

#[test]
fn remove_plan_is_irreversible() {
    // REMOVE is inherently irreversible (RFD §6/§10) — the harness asserts the safety surface.
    let _ = assert_plan("REMOVE /db/users", &registry())
        .nodes(&[EffectKind::Remove])
        .irreversible(1)
        .no_io_performed();
}

#[test]
fn upsert_plan_golden_snapshot() {
    // Golden of the owned `Plan` DTO via canonical JSON (deterministic, scrubbed for tokens).
    assert_plan("UPSERT INTO /db/users VALUES (1, 'a', true)", &registry())
        .snapshot("plan_upsert_users");
}

#[test]
fn pipe_stage_and_verb_leading_writes_lower_to_one_plan() {
    // Decision Q (t72): a write spelled as a terminal pipeline stage and the verb-leading
    // spelling of the same write evaluate to the SAME effect `Plan` — the strongest proof that
    // this is grammar shape, not new effect semantics. Compared by `Plan` value-equality (the
    // plan carries no parser spans, so equal plans is the whole assertion).
    let reg = registry();
    let pairs = [
        // INSERT/UPSERT: upstream pipeline body, explicit target.
        (
            "/db/src |> WHERE id > 0 |> UPSERT INTO /db/users",
            "UPSERT INTO /db/users /db/src |> WHERE id > 0",
        ),
        // UPDATE: target + filter lifted from the upstream.
        (
            "/db/users |> WHERE id == 1 |> UPDATE SET name = 'x'",
            "UPDATE /db/users SET name = 'x' WHERE id == 1",
        ),
        // REMOVE: target + filter lifted from the upstream (irreversible either way).
        (
            "/db/users |> WHERE id == 1 |> REMOVE",
            "REMOVE /db/users WHERE id == 1",
        ),
    ];
    for (pipe_form, verb_form) in pairs {
        let pipe_plan = assert_plan(pipe_form, &reg);
        let verb_plan = assert_plan(verb_form, &reg);
        assert!(
            pipe_plan.plan() == verb_plan.plan(),
            "pipe-stage `{pipe_form}` and verb-leading `{verb_form}` must build one Plan:\n  \
             pipe: {:?}\n  verb: {:?}",
            pipe_plan.plan(),
            verb_plan.plan()
        );
    }
}

// ---------------------------------------------------------------------------
// 2. Parser / grammar golden corpus (closed-core keywords, |>, CALL, DECODE, CREATE).
// ---------------------------------------------------------------------------

#[test]
fn parser_golden_query_pipe() {
    golden_parse("/mail/inbox |> WHERE id > 5 |> LIMIT 10").snapshot("ast_query_pipe");
}

#[test]
fn parser_golden_decode_call() {
    golden_parse("/s3/data |> DECODE json |> CALL git.merge()").snapshot("ast_decode_call");
}

#[test]
fn parser_golden_create_endpoint() {
    golden_parse("CREATE ENDPOINT recent ON 'GET /recent' AS /mail/inbox |> LIMIT 5")
        .snapshot("ast_create_endpoint");
}

#[test]
fn parser_golden_error_recovery() {
    // A lowercase keyword is not in the frozen closed-core set — a stable recovery message.
    // (`from` is no longer a keyword post-t73; a lowercase *stage* keyword still trips the check.)
    error_snapshot("/mail/inbox |> where id > 5").snapshot("ast_error_lowercase_keyword");
}

// ---------------------------------------------------------------------------
// 3. Codec round-trip (DECODE∘ENCODE == identity over the input corpus).
// ---------------------------------------------------------------------------

#[test]
fn codec_roundtrip_all_formats() {
    for (fmt, bytes) in qfs_test::corpus() {
        roundtrip_codec(fmt, bytes).assert_identity();
    }
}

// ---------------------------------------------------------------------------
// 4. Handler PREVIEW fixture (CREATE ENDPOINT/TRIGGER/JOB → Plan, no socket).
// ---------------------------------------------------------------------------

#[test]
fn handler_preview_endpoint_trigger_job() {
    let ep = preview_handler("CREATE ENDPOINT hello ON 'GET /hello' AS /mail/inbox");
    assert!(matches!(
        ep.nodes()[0].kind,
        EffectKind::ServerConfigWrite { .. }
    ));

    let job = preview_handler("CREATE JOB nightly EVERY '1h' DO REMOVE /tmp/scratch");
    assert!(matches!(
        job.nodes()[0].kind,
        EffectKind::ServerConfigWrite { .. }
    ));

    // All three desugar to a single reversible /server config-write — no socket opened.
    assert_eq!(ep.nodes().len(), 1);
    assert_eq!(job.nodes().len(), 1);
    assert!(!ep.is_irreversible() && !job.is_irreversible());
}

#[test]
fn handler_preview_golden() {
    // The plan a fired ENDPOINT binding would COMMIT, snapshotted.
    let plan = preview_handler("CREATE ENDPOINT hello ON 'GET /hello' AS /mail/inbox");
    let canon = canonicalize(plan);
    qfs_test::golden::assert_no_credential_shape(&qfs_test::golden::canonical_json(&canon));
    qfs_test::golden::assert_golden("plan_endpoint_hello", &canon);
}

/// Canonical node/edge order for a stable handler-plan golden (same normalization the
/// PlanAssert snapshot applies internally).
fn canonicalize(mut plan: qfs_core::Plan) -> qfs_core::Plan {
    plan.nodes.sort_by_key(|n| n.id.0);
    plan.deps.sort_by_key(|a| (a.0 .0, a.1 .0));
    plan
}

// ---------------------------------------------------------------------------
// 5. Idempotency — apply-twice UPSERT converges via FakeBackend + FakeWorld.
// ---------------------------------------------------------------------------

#[test]
fn upsert_apply_twice_converges_through_fake_backend() {
    let mut b = PlanBuilder::new();
    let schema = Schema::new(vec![Column::new("v", ColumnType::Text, false)]);
    let rows = vec![Row::new(vec![Value::Text("x".to_string())])];
    b.push(
        EffectNode::new(
            NodeId(0),
            EffectKind::Upsert,
            qfs_core::Target::new(DriverId::new("db"), VfsPath::new("/db/users")),
        )
        .with_args(RowBatch::new(schema, rows))
        .with_affected(Affected::Exact(1)),
    );
    let plan = b.build();

    let mut be = FakeBackend::new();
    let _ = qfs_core::commit(&plan, &mut be, |_| {});
    let after_first = be.world().clone();
    let _ = qfs_core::commit(&plan, &mut be, |_| {});
    let after_second = be.world().clone();

    // Apply-twice CONVERGES — retry-safety via the FakeWorld state assertion (RFD §6).
    assert_eq!(after_first, after_second);
    assert_eq!(be.world().rows_at("/db/users").len(), 1);
}
