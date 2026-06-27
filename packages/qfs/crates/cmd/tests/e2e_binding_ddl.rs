//! Planner-owned **E2E / external-interface** black-box validation of the t31 server-binding
//! DDL (the five frozen `CREATE …` forms desugaring to `INSERT INTO /server/*`).
//!
//! This is NOT a unit test and NOT a code review: every scenario drives the system from the
//! OUTSIDE — either the REAL `qfs serve` binary as a subprocess (boot scenario) or the public
//! `qfs-core` desugar API + the public `qfs-server` `Runtime` as a black box. The stronger
//! external assertion is on the COMMITTED EFFECT (the stored `ServerState`), never on private
//! internals. No live creds, no network: `/server` writes are in-memory.
//!
//! ## Why no `qfs-parser` here (deliberate constraint, same as the t30 E2E)
//! `tests/dep_direction.rs` forbids `qfs-cmd` from depending on `qfs-parser` (it counts
//! dev-deps). So this test NEVER calls the parser directly. The CREATE forms are driven
//! through `qfs_core::parse_server_binding_ddl` (the core DDL entry point parses internally)
//! and the runtime-level convergence through `Runtime::apply_source` (which parses + lowers +
//! commits internally). Deferred-body specs are rehydrated through the public
//! `qfs_core::{PlanSpec, StatementSpec}::from_canonical` (serde, no re-parse).
//!
//! ## Relationship to the Constructor's tests (independence)
//! The Constructor's unit tests in `core/src/ddl/server/tests.rs` build the INSERT twin body
//! by hand (`parse_statement` + `PlanSpec::from_statement`). This harness does NOT trust that:
//! scenario 3 re-derives both bodies *end-to-end through the public runtime* (`apply_source`
//! for the CREATE and for the hand-written `UPSERT INTO /server/jobs … '<src>'` twin) and
//! compares the STORED bodies, with realistic predicate / multi-stage pipe bodies, to
//! independently ratify the gap closure.
//!
//! Scenario map (ticket acceptance criteria):
//!  1. All five frozen forms desugar to exactly ONE `INSERT INTO /server/<kind>` (Affected
//!     Exact(1)); `MATERIALIZED VIEW` => materialized=true, plain `VIEW` => false.
//!  2. PREVIEW reports "1 row → /server/<kind>" and performs NO I/O; COMMIT is the only impure
//!     step.
//!  3. CO-t31-3 — independently re-confirm body-bearing `CREATE … DO <plan>` ≡ its INSERT twin
//!     across realistic bodies (a WHERE predicate, a multi-stage `|>` pipe), through the public
//!     runtime, comparing the STORED canonical specs byte-for-byte.
//!  4. Deferred bodies parsed + type-checked at CREATE time; malformed body rejected at CREATE
//!     time (no panic, no COMMIT); PlanSpec/StatementSpec round-trips through serde and
//!     rehydrates via `from_canonical` with no re-parse.
//!  5. Parse-time rejection: unknown `/server/*` column, unknown CREATE subkeyword, malformed
//!     body each yield a structured error (no panic).
//!  6. Idempotency: re-CREATE of a same-named job is UPSERT-by-name (no-op on re-apply); the
//!     explicit `INSERT INTO /server/…` form still fails on duplicate.
//!  7. Purity: building/desugaring a CREATE mutates no ServerState and never executes the
//!     embedded DO body (it stays data).
//!  8. Boot a fixture mixing CREATE forms + INSERT through `qfs serve` and confirm the
//!     ServerState snapshot is deterministic and the audit drains correctly (regression vs t30).

// Test code: assertions and setup may panic/expect/unwrap freely.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use qfs_core::{
    desugar_to_insert, parse_server_binding_ddl, preview, Affected, DesugarToInsert, EffectKind,
    PlanSpec, ServerBindingDdl, ServerNode, ServerWriteOp, StatementSpec,
};
use qfs_server::{NullBinding, Runtime, ServerError, ServerState};

// ---------------------------------------------------------------------------
// Helpers — drive the public surfaces as a black box
// ---------------------------------------------------------------------------

/// Parse a `CREATE …` source through the public core DDL entry point (parses internally).
fn binding(src: &str) -> ServerBindingDdl {
    parse_server_binding_ddl(src).expect("valid binding DDL")
}

/// The single effect node of a one-node desugared plan, asserting there is exactly one.
fn assert_single_server_insert(ddl: &ServerBindingDdl, expect_node: ServerNode) {
    let plan = ddl.desugar().expect("desugar");
    assert_eq!(
        plan.nodes.len(),
        1,
        "a binding desugars to exactly one effect node"
    );
    let node = &plan.nodes[0];
    match &node.kind {
        EffectKind::ServerConfigWrite { node, op } => {
            assert_eq!(
                *node, expect_node,
                "desugars to the right /server collection"
            );
            // CREATE is the boot/replay-safe UPSERT-by-name verb (idempotency §6).
            assert_eq!(*op, ServerWriteOp::Upsert, "CREATE desugars to UPSERT");
        }
        other => panic!("expected a ServerConfigWrite effect, got {other:?}"),
    }
    assert_eq!(
        node.target.path.as_str(),
        format!("/server/{}", expect_node.segment()),
        "targets the right /server path"
    );
    assert_eq!(
        node.est_affected,
        Affected::Exact(1),
        "exactly one row affected"
    );
}

/// Apply one statement through the public runtime and return the committed snapshot.
fn apply_one(src: &str) -> ServerState {
    let mut rt = Runtime::new().with_binding(Box::new(NullBinding));
    rt.apply_source("e2e", 1, src).expect("apply");
    rt.snapshot()
}

/// Locate the built `qfs` binary (same approach as the t30 E2E harness).
fn qfs_bin() -> PathBuf {
    let mut dir = std::env::current_exe().expect("current_exe");
    dir.pop(); // deps/
    dir.pop(); // <profile>/
    let bin = dir.join(if cfg!(windows) { "qfs.exe" } else { "qfs" });
    if !bin.is_file() {
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
        let status = Command::new(cargo)
            .args(["build", "-p", "qfs"])
            .status()
            .expect("build qfs");
        assert!(status.success(), "failed to build the qfs binary");
    }
    assert!(bin.is_file(), "qfs binary not found at {}", bin.display());
    bin
}

/// The in-worktree boot fixture (no system paths, no network, no creds).
fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("server")
        .join("fixtures")
        .join("server_boot.qfs")
}

fn send_sigint(pid: u32) {
    let status = Command::new("kill")
        .args(["-INT", &pid.to_string()])
        .status()
        .expect("spawn kill");
    assert!(status.success(), "kill -INT failed");
}

// ---------------------------------------------------------------------------
// Scenario 1 — all five frozen forms desugar to exactly one /server INSERT
// ---------------------------------------------------------------------------

#[test]
fn all_five_frozen_forms_desugar_to_one_server_insert() {
    // ENDPOINT: method+route ride the ON '<method> /route' operand (the frozen t04 surface —
    // NO `AT`, NO bare method-route token; the Architect ruled AT out of scope).
    assert_single_server_insert(
        &binding("CREATE ENDPOINT recent ON 'GET /recent' AS /mail |> LIMIT 10"),
        ServerNode::Endpoints,
    );
    // TRIGGER: ON <event> DO <plan>.
    assert_single_server_insert(
        &binding("CREATE TRIGGER onnew ON inbox DO REMOVE /tmp WHERE age > 7"),
        ServerNode::Triggers,
    );
    // JOB: EVERY <interval> DO <plan>.
    assert_single_server_insert(
        &binding("CREATE JOB nightly EVERY '1h' DO REMOVE /tmp WHERE age > 7"),
        ServerNode::Jobs,
    );
    // VIEW + MATERIALIZED VIEW: both /server/views.
    assert_single_server_insert(
        &binding("CREATE VIEW recent AS /mail |> LIMIT 5"),
        ServerNode::Views,
    );
    assert_single_server_insert(
        &binding("CREATE MATERIALIZED VIEW cached AS /mail |> LIMIT 5"),
        ServerNode::Views,
    );
    // WEBHOOK: route rides the ON '<route>' operand (frozen `ON`, not a new `AT` keyword).
    assert_single_server_insert(
        &binding("CREATE WEBHOOK gh ON '/hooks/gh'"),
        ServerNode::Webhooks,
    );
}

#[test]
fn materialized_flag_distinguishes_view_forms_in_the_stored_row() {
    // Observe the COMMITTED effect: a MATERIALIZED VIEW stores materialized=true, a plain VIEW
    // materialized=false. This is the externally observable form of the flag.
    let plain = apply_one("CREATE VIEW recent_view AS /mail |> LIMIT 5");
    let mat = apply_one("CREATE MATERIALIZED VIEW cached_view AS /mail |> LIMIT 5");
    assert!(
        !plain
            .views
            .get("recent_view")
            .expect("recent_view")
            .materialized,
        "plain VIEW => materialized=false"
    );
    assert!(
        mat.views
            .get("cached_view")
            .expect("cached_view")
            .materialized,
        "MATERIALIZED VIEW => materialized=true"
    );
}

// ---------------------------------------------------------------------------
// Scenario 2 — PREVIEW reports "1 row → /server/<kind>" and does no I/O
// ---------------------------------------------------------------------------

#[test]
fn preview_reports_one_row_into_each_server_kind_and_does_no_io() {
    let cases = [
        (
            "CREATE ENDPOINT recent ON 'GET /recent' AS /mail |> LIMIT 10",
            "/server/endpoints",
        ),
        (
            "CREATE TRIGGER onnew ON inbox DO REMOVE /tmp WHERE age > 7",
            "/server/triggers",
        ),
        (
            "CREATE JOB nightly EVERY '1h' DO REMOVE /tmp WHERE age > 7",
            "/server/jobs",
        ),
        ("CREATE VIEW v AS /mail |> LIMIT 5", "/server/views"),
        ("CREATE WEBHOOK gh ON '/hooks/gh'", "/server/webhooks"),
    ];
    for (src, expect_path) in cases {
        let plan = binding(src).desugar().expect("desugar");
        let pv = preview(&plan);
        assert!(
            !pv.is_pure,
            "a binding previews as an effect (not a pure read): {src}"
        );
        assert_eq!(pv.rows.len(), 1, "exactly one previewed row: {src}");
        let row = &pv.rows[0];
        assert_eq!(
            row.target.path.as_str(),
            expect_path,
            "preview target: {src}"
        );
        assert_eq!(
            row.affected,
            Affected::Exact(1),
            "1 row → {expect_path}: {src}"
        );
    }
    // PREVIEW performs no I/O — building the plan and previewing it mutates nothing observable
    // (this whole test never constructs a Runtime, so there is no ServerState to mutate).
}

// ---------------------------------------------------------------------------
// Scenario 3 (CO-t31-3) — INDEPENDENTLY re-confirm the flipped tripwire end-to-end
// ---------------------------------------------------------------------------

/// Drive a body-bearing `CREATE JOB … DO <body>` and its hand-written INSERT twin (carrying
/// the SAME body as a `plan` string column) THROUGH THE PUBLIC RUNTIME, and return the two
/// STORED `plan` bodies. This re-derives both ends end-to-end (no reliance on the Constructor's
/// hand-built spec): the runtime parses + lowers + commits each one and we read the committed
/// `JobDef.plan` it persisted.
fn stored_job_plan_bodies(body: &str) -> (String, String) {
    let create_src = format!("CREATE JOB x EVERY '1h' DO {body}");
    let twin_src =
        format!("UPSERT INTO /server/jobs VALUES (name, every, plan) ('x', '1h', '{body}')");
    let from_create = apply_one(&create_src)
        .jobs
        .get("x")
        .expect("job x via CREATE")
        .plan
        .as_str()
        .to_string();
    let from_insert = apply_one(&twin_src)
        .jobs
        .get("x")
        .expect("job x via INSERT")
        .plan
        .as_str()
        .to_string();
    (from_create, from_insert)
}

#[test]
fn body_bearing_create_equals_insert_twin_with_a_where_predicate() {
    // A realistic predicate body — exactly the shape the flipped tripwire claims now converges.
    let (create_body, insert_body) = stored_job_plan_bodies("REMOVE /tmp WHERE age > 7");
    assert_eq!(
        create_body, insert_body,
        "body-bearing CREATE ≡ INSERT (WHERE predicate): both store ONE canonical span-normalised spec"
    );
    // The stored body is the canonical serialized PARSED spec, not raw source nor a Debug dump.
    assert!(
        create_body.contains("Effect") && create_body.contains("Remove"),
        "stored body is the canonical serialized spec, not raw source: {create_body:?}"
    );
    assert_ne!(
        create_body, "REMOVE /tmp WHERE age > 7",
        "the stored body is the parsed canonical spec, NOT the raw source string"
    );
    // And it rehydrates via serde with NO re-parse (the runtime's fire-time path).
    PlanSpec::from_canonical(&create_body)
        .expect("stored body rehydrates as a PlanSpec, no re-parse");
}

#[test]
fn body_bearing_create_equals_insert_twin_with_a_multistage_pipe() {
    // A multi-stage `|>` pipe body (WHERE then SELECT then LIMIT) — stresses the parse-to-
    // canonical convergence across a richer AST than a single effect.
    let (create_body, insert_body) =
        stored_job_plan_bodies("/mail |> WHERE age > 7 |> SELECT id, subject |> LIMIT 3");
    assert_eq!(
        create_body, insert_body,
        "body-bearing CREATE ≡ INSERT (multi-stage pipe): both store ONE canonical spec"
    );
    // It is the parsed spec (a Query pipeline), not raw source.
    assert!(
        create_body.contains("Query") || create_body.contains("Pipeline"),
        "stored multi-stage body is the canonical serialized spec: {create_body:?}"
    );
    StatementSpec::from_canonical(&create_body)
        .expect("stored multi-stage body rehydrates as a StatementSpec, no re-parse");
}

#[test]
fn body_bearing_create_equals_insert_twin_with_a_nested_predicate() {
    // A compound (nested) predicate — AND of two comparisons — exercises the Binary-expr span
    // normalisation recursively.
    let (create_body, insert_body) =
        stored_job_plan_bodies("REMOVE /tmp WHERE age > 7 AND size < 100");
    assert_eq!(
        create_body, insert_body,
        "body-bearing CREATE ≡ INSERT (nested AND predicate): canonical specs converge"
    );
}

#[test]
fn body_bearing_equivalence_holds_for_triggers_too() {
    // Re-confirm the equivalence is not job-specific: a body-bearing TRIGGER and its INSERT twin
    // (both storing the DO body in the `plan` column of /server/triggers) also converge. The
    // body avoids an embedded string literal so the INSERT twin's `plan` column needs no nested
    // quoting (a harness-quoting concern, not a product one).
    let body = "REMOVE /tmp WHERE age > 30";
    let create_body = apply_one(&format!("CREATE TRIGGER t ON inbox DO {body}"))
        .triggers
        .get("t")
        .expect("trigger t via CREATE")
        .plan
        .as_str()
        .to_string();
    let insert_body = apply_one(&format!(
        "UPSERT INTO /server/triggers VALUES (name, on, plan) ('t', 'inbox', '{body}')"
    ))
    .triggers
    .get("t")
    .expect("trigger t via INSERT")
    .plan
    .as_str()
    .to_string();
    assert_eq!(
        create_body, insert_body,
        "body-bearing CREATE TRIGGER ≡ its INSERT twin"
    );
}

// ---------------------------------------------------------------------------
// Scenario 4 — deferred bodies parsed + type-checked at CREATE; serde round-trip
// ---------------------------------------------------------------------------

#[test]
fn malformed_do_body_is_rejected_at_create_time_no_panic_no_commit() {
    // A malformed `DO <plan>` body is rejected at CREATE time (parse error surfaces NOW, never
    // deferred to fire time), with NO panic. Through the public core entry point.
    let err = parse_server_binding_ddl("CREATE JOB j EVERY '1h' DO this is not a statement")
        .expect_err("malformed DO body must be rejected at CREATE time");
    // Structured error (a Parse variant), not a panic.
    assert!(!err.code().is_empty(), "structured error code present");

    // And end-to-end through the runtime: apply_source must return a structured ServerError and
    // commit nothing.
    let mut rt = Runtime::new().with_binding(Box::new(NullBinding));
    let res = rt.apply_source(
        "e2e",
        1,
        "CREATE JOB j EVERY '1h' DO this is not a statement",
    );
    match res {
        Err(ServerError::Parse { .. }) | Err(ServerError::Lower { .. }) => {}
        other => panic!("expected a structured parse/lower error at CREATE time, got {other:?}"),
    }
    assert_eq!(
        rt.snapshot().row_count(),
        0,
        "a malformed body commits nothing (no COMMIT on a parse failure)"
    );
}

#[test]
fn malformed_as_query_body_is_rejected_at_create_time() {
    let err = parse_server_binding_ddl("CREATE VIEW v AS this is not a query")
        .expect_err("malformed AS body must be rejected at CREATE time");
    assert!(
        !err.code().is_empty(),
        "structured error, not a panic: {err}"
    );
}

#[test]
fn plan_spec_round_trips_through_serde_and_rehydrates_without_reparse() {
    // The deferred body, captured at CREATE time, round-trips through serde byte-identically and
    // rehydrates via from_canonical with NO re-parse (the runtime's fire-time contract).
    let b = binding("CREATE JOB nightly EVERY '1h' DO REMOVE /tmp WHERE age > 7");
    let ServerBindingDdl::Job(d) = &b else {
        panic!("expected a Job");
    };
    let plan = d.plan.as_ref().expect("DO body present");
    let json = serde_json::to_string(plan).expect("serialize PlanSpec");
    let back: PlanSpec = serde_json::from_str(&json).expect("deserialize PlanSpec");
    assert_eq!(&back, plan, "PlanSpec round-trips through serde unchanged");

    // Canonical form rehydrates via from_canonical (serde, no parser invoked).
    let canon = plan.canonical();
    let rehydrated =
        PlanSpec::from_canonical(&canon).expect("rehydrate from canonical, no re-parse");
    assert_eq!(
        rehydrated.canonical(),
        canon,
        "rehydrated canonical is stable"
    );

    // A StatementSpec (AS-query body) likewise.
    let v = binding("CREATE VIEW recent AS /mail |> WHERE age > 7 |> LIMIT 5");
    let ServerBindingDdl::View(vd) = &v else {
        panic!("expected a View");
    };
    let q = vd.query.as_ref().expect("AS body present");
    let qcanon = q.canonical();
    let qre = StatementSpec::from_canonical(&qcanon).expect("rehydrate StatementSpec");
    assert_eq!(qre.canonical(), qcanon);
}

// ---------------------------------------------------------------------------
// Scenario 5 — parse-time rejection of bad column / subkeyword / body
// ---------------------------------------------------------------------------

#[test]
fn unknown_create_subkeyword_is_structured_not_a_panic() {
    // POLICY is parsed by t04 but is not a t31 binding form (deferred to t34) — structured
    // rejection through the core binding entry point, not a panic.
    let err = parse_server_binding_ddl("CREATE POLICY p").expect_err("POLICY is not a t31 form");
    assert_eq!(err.code(), "UNSUPPORTED_DDL");
}

#[test]
fn missing_required_clause_is_structured() {
    // CREATE JOB with no EVERY clause — structured MISSING_CLAUSE, no panic.
    let err = parse_server_binding_ddl("CREATE JOB j DO REMOVE /tmp").expect_err("no EVERY");
    assert_eq!(err.code(), "MISSING_CLAUSE");
}

#[test]
fn unknown_server_column_is_rejected_at_lower_time_through_the_runtime() {
    // An explicit /server write naming a column the node's schema does not declare is rejected
    // (UNKNOWN_COLUMN) end-to-end through the runtime — a structured Lower error, no panic, no
    // COMMIT.
    let mut rt = Runtime::new().with_binding(Box::new(NullBinding));
    let res = rt.apply_source(
        "e2e",
        1,
        "UPSERT INTO /server/jobs VALUES (name, bogus_col) ('x', 'boom')",
    );
    match res {
        Err(ServerError::Lower { detail, .. }) => {
            assert!(
                detail.contains("bogus_col") || detail.to_uppercase().contains("COLUMN"),
                "unknown column surfaces in the structured detail: {detail}"
            );
        }
        other => panic!("expected a structured Lower error for an unknown column, got {other:?}"),
    }
    assert_eq!(
        rt.snapshot().row_count(),
        0,
        "no COMMIT on an unknown column"
    );
}

// ---------------------------------------------------------------------------
// Scenario 6 — idempotency: re-CREATE is UPSERT-by-name; explicit INSERT fails on dup
// ---------------------------------------------------------------------------

#[test]
fn re_create_of_a_same_named_job_is_upsert_by_name_no_op() {
    // Re-applying the SAME CREATE (the config.qfs replay case) converges — UPSERT-by-name, no
    // duplicate row, identical stored state.
    let mut rt = Runtime::new().with_binding(Box::new(NullBinding));
    rt.apply_source(
        "e2e",
        1,
        "CREATE JOB nightly EVERY '1h' DO REMOVE /tmp WHERE age > 7",
    )
    .expect("first CREATE");
    let first = rt.snapshot();
    rt.apply_source(
        "e2e",
        2,
        "CREATE JOB nightly EVERY '1h' DO REMOVE /tmp WHERE age > 7",
    )
    .expect("re-CREATE is a no-op (UPSERT)");
    let second = rt.snapshot();
    assert_eq!(first, second, "re-CREATE of a same-named job is a no-op");
    assert_eq!(second.jobs.len(), 1, "no duplicate job from re-CREATE");
}

#[test]
fn re_create_of_a_same_named_trigger_is_upsert_by_name() {
    let mut rt = Runtime::new().with_binding(Box::new(NullBinding));
    rt.apply_source(
        "e2e",
        1,
        "CREATE TRIGGER t ON inbox DO INSERT INTO /log VALUES ('x')",
    )
    .expect("first CREATE TRIGGER");
    let first = rt.snapshot();
    rt.apply_source(
        "e2e",
        2,
        "CREATE TRIGGER t ON inbox DO INSERT INTO /log VALUES ('x')",
    )
    .expect("re-CREATE TRIGGER is a no-op");
    assert_eq!(
        first,
        rt.snapshot(),
        "re-CREATE of a same-named trigger is a no-op"
    );
    assert_eq!(rt.snapshot().triggers.len(), 1, "no duplicate trigger");
}

#[test]
fn explicit_insert_into_server_still_fails_on_duplicate() {
    // The explicit `INSERT INTO /server/…` form (NOT the CREATE sugar) still rejects a duplicate
    // name — the fail-on-duplicate path remains available for a caller who wants it.
    let mut rt = Runtime::new().with_binding(Box::new(NullBinding));
    rt.apply_source(
        "e2e",
        1,
        "INSERT INTO /server/jobs VALUES (name, every, plan) ('dup', '1h', '')",
    )
    .expect("first explicit INSERT");
    let res = rt.apply_source(
        "e2e",
        2,
        "INSERT INTO /server/jobs VALUES (name, every, plan) ('dup', '2h', '')",
    );
    match res {
        Err(ServerError::Commit { reason, .. }) => {
            assert!(
                reason.to_uppercase().contains("DUPLICATE") || reason.contains("dup"),
                "duplicate INSERT is rejected with a structured reason: {reason}"
            );
        }
        other => panic!("explicit duplicate INSERT must fail, got {other:?}"),
    }
    // The first row survives unchanged (no partial/overwrite from the rejected second INSERT).
    assert_eq!(rt.snapshot().jobs.get("dup").expect("dup job").every, "1h");
    assert_eq!(
        rt.snapshot().jobs.len(),
        1,
        "the rejected INSERT added nothing"
    );
}

// ---------------------------------------------------------------------------
// Scenario 7 — purity: desugar mutates no state, never executes the DO body
// ---------------------------------------------------------------------------

#[test]
fn desugar_is_pure_and_never_executes_the_embedded_do_body() {
    // Building/desugaring a body-bearing CREATE constructs a Plan and runs no I/O. The embedded
    // DO body — itself an effect-plan (a REMOVE) — is stored as DATA (a PlanSpec), never
    // executed: desugaring it must NOT delete /tmp or touch any backend.
    let b = binding("CREATE JOB destructive EVERY '1h' DO REMOVE /tmp WHERE age > 7");
    let ServerBindingDdl::Job(d) = &b else {
        panic!("expected a Job");
    };
    // The DO body is held as a serializable spec (data), not a live executable Plan.
    let spec = d.plan.as_ref().expect("DO body");
    // The spec is a parsed Statement we can serialize — proving it is inert data.
    let _ = serde_json::to_string(spec).expect("the DO body is inert serializable data");

    // Desugaring produces a /server/jobs write plan whose EFFECT is the config write — NOT the
    // embedded REMOVE. The embedded REMOVE never appears as a top-level effect node.
    let plan = desugar_to_insert(&b).expect("desugar");
    assert_eq!(
        plan.nodes.len(),
        1,
        "desugar yields one effect: the config write"
    );
    match &plan.nodes[0].kind {
        EffectKind::ServerConfigWrite { node, .. } => {
            assert_eq!(
                *node,
                ServerNode::Jobs,
                "the only effect is the /server/jobs write"
            );
        }
        other => panic!("the embedded REMOVE must NOT become a top-level effect: {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Scenario 8 — boot a mixed CREATE + INSERT fixture; deterministic snapshot + audit drain
// ---------------------------------------------------------------------------

#[test]
fn boot_mixed_create_and_insert_fixture_is_deterministic() {
    // Boot the fixture (which mixes all five CREATE forms + a POLICY + an explicit UPSERT INTO
    // /server/jobs) and confirm the committed ServerState is deterministic across boots and
    // carries the expected per-collection rows. Regression check against t30.
    let snap = |_: ()| {
        let mut rt = Runtime::new();
        rt.boot(&fixture_path()).expect("boot");
        rt.snapshot()
    };
    let a = snap(());
    let b = snap(());
    assert_eq!(a, b, "the booted ServerState is deterministic across boots");
    assert_eq!(
        serde_json::to_string(&a).unwrap(),
        serde_json::to_string(&b).unwrap(),
        "byte-stable serde across boots"
    );
    // The mixed forms land in the right collections.
    assert_eq!(a.endpoints.len(), 1, "endpoints");
    assert_eq!(a.triggers.len(), 1, "triggers");
    assert_eq!(
        a.jobs.len(),
        2,
        "nightly (CREATE) + weekly (explicit UPSERT)"
    );
    assert_eq!(a.views.len(), 2, "plain + materialized view");
    assert_eq!(a.webhooks.len(), 1, "webhook");
    assert!(
        a.jobs.contains_key("nightly"),
        "the CREATE-form job is present"
    );
    assert!(
        a.jobs.contains_key("weekly"),
        "the explicit-INSERT job is present"
    );
    assert!(
        a.views
            .get("cached_view")
            .expect("cached_view")
            .materialized
    );
    assert!(
        !a.views
            .get("recent_view")
            .expect("recent_view")
            .materialized
    );
}

#[test]
fn serve_boots_mixed_fixture_and_drains_audit_on_sigint() {
    // Drive the REAL `qfs serve` binary over the mixed CREATE+INSERT fixture: it boots without
    // network/creds, reaches the run loop, and on SIGINT drains exactly one audit entry per
    // /server mutation (8 statements in the fixture). Regression check that the t31 desugar path
    // produces one auditable committed effect per CREATE.
    let mut child = Command::new(qfs_bin())
        .args(["serve", fixture_path().to_str().unwrap()])
        .env("RUST_LOG", "qfs::server=info,qfs::server::audit=info")
        .env("NO_COLOR", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn qfs serve");
    let mut stderr = child.stderr.take().expect("child stderr");

    std::thread::sleep(Duration::from_millis(800));
    assert!(
        child.try_wait().expect("try_wait").is_none(),
        "server must still be running (blocked in the run loop), not self-exited"
    );

    send_sigint(child.id());
    let deadline = Instant::now() + Duration::from_secs(5);
    let status = loop {
        if let Some(s) = child.try_wait().expect("try_wait") {
            break s;
        }
        assert!(
            Instant::now() < deadline,
            "server did not exit after SIGINT"
        );
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(
        status.success(),
        "clean shutdown on SIGINT must exit 0, got {status:?}"
    );

    let mut log = String::new();
    stderr.read_to_string(&mut log).expect("read stderr");
    assert!(log.contains("boot complete"), "boot must complete:\n{log}");
    assert!(
        log.contains("server running"),
        "must reach the supervised run loop:\n{log}"
    );
    assert!(
        log.contains("audit ledger drained") && log.contains("entries=8"),
        "shutdown must drain exactly 8 audit entries (one per /server mutation):\n{log}"
    );
}
