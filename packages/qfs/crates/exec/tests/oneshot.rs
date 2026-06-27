//! End-to-end tests for the t29 execution layer: the SELECT read-path executor (the t20
//! carry-over closure) and the one-shot CLI orchestration (PREVIEW/COMMIT gate, exit codes,
//! renderers). All drivers are in-memory fakes — **no live creds, no network**.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;

use qfs_core::{AppliedEffect, ApplyError, EffectNode, PlanApplier};
use qfs_core::{
    Archetype, Capabilities, CfsError, Column, ColumnType, DriverId, Engine, NodeDesc, Path,
    PushdownProfile, Row, RowBatch, Schema, Value,
};
use qfs_exec::{
    block_on_read, parse, run_oneshot, ExecCtx, OutputFormat, ReadDriver, ReadRegistry, StmtSource,
    Streams,
};
use qfs_pushdown::ScanNode;

// ---- An in-memory fake driver: introspective (describe/pushdown) + read (scan) ----

/// A fake mail-like source. `PushdownProfile::None` so a `WHERE`/`LIMIT` stays in the local
/// residual — the scan deliberately **over-returns** every row, and the engine's residual
/// re-filter restores correctness (the t20 property, end-to-end).
struct FakeMail {
    mount: String,
    rows: Vec<Row>,
}

fn mail_schema() -> Schema {
    Schema::new(vec![
        Column::new("id", ColumnType::Int, false),
        Column::new("subject", ColumnType::Text, true),
    ])
}

impl FakeMail {
    fn new() -> Self {
        Self {
            mount: "/mail".to_string(),
            rows: vec![
                Row::new(vec![Value::Int(1), Value::Text("hello".into())]),
                Row::new(vec![Value::Int(2), Value::Text("spam".into())]),
                Row::new(vec![Value::Int(3), Value::Text("world".into())]),
            ],
        }
    }
}

#[derive(Default)]
struct NoopApplier;
impl PlanApplier for NoopApplier {
    fn apply(&mut self, node: &EffectNode) -> Result<AppliedEffect, ApplyError> {
        Ok(AppliedEffect::new(node.id, 0))
    }
}

impl qfs_core::Driver for FakeMail {
    fn mount(&self) -> &str {
        &self.mount
    }
    fn describe(&self, _path: &Path) -> Result<NodeDesc, CfsError> {
        Ok(NodeDesc::new(Archetype::RelationalTable, mail_schema()))
    }
    fn capabilities(&self, _path: &Path) -> Capabilities {
        // All verbs so the effect-path tests (INSERT/REMOVE) pass the capability gate.
        Capabilities::none().select().insert().update().remove()
    }
    fn procedures(&self) -> &[qfs_core::ProcSig] {
        &[]
    }
    fn pushdown(&self) -> &PushdownProfile {
        // None: WHERE/LIMIT are local residuals — the engine must re-filter the over-returned
        // scan rows.
        &PushdownProfile::None
    }
    fn applier(&self) -> &dyn PlanApplier {
        // A leaked Box keeps a 'static reference without per-call allocation churn in the test.
        Box::leak(Box::new(NoopApplier))
    }
}

#[async_trait::async_trait]
impl ReadDriver for FakeMail {
    async fn scan(&self, _scan: &ScanNode) -> Result<RowBatch, CfsError> {
        // Honestly over-return: hand back ALL rows regardless of the pushed WHERE/LIMIT (this
        // source pushes nothing). The executor's residual must trim to the real result.
        Ok(RowBatch::new(mail_schema(), self.rows.clone()))
    }
}

fn engine_with_mail() -> Engine {
    let mut engine = Engine::new();
    engine.mounts.register(Arc::new(FakeMail::new())).unwrap();
    engine
}

fn reads_with_mail() -> ReadRegistry {
    ReadRegistry::new().with(DriverId::new("mail"), Arc::new(FakeMail::new()))
}

// ---- The headline read-path acceptance (the t20 carry-over closure) ----

#[test]
fn headline_read_returns_rows_through_real_executor() {
    // `/mail/inbox |> LIMIT 1` returns {"rows":[…]} end-to-end: parse -> resolve -> plan
    // -> scan -> residual -> rows. The fake over-returns 3 rows; LIMIT 1 trims to 1.
    let engine = engine_with_mail();
    let reads = reads_with_mail();
    let stmt = parse("/mail/inbox |> LIMIT 1").unwrap();
    let rows = block_on_read(&stmt, &engine.mounts, &reads).unwrap();
    assert_eq!(
        rows.len(),
        1,
        "LIMIT 1 residual trims the over-returned rows"
    );
    assert_eq!(rows.columns(), vec!["id", "subject"]);
}

#[test]
fn residual_where_refilters_over_returned_rows() {
    // The t20 property end-to-end: a None-pushdown source returns all 3 rows; the residual
    // WHERE id > 1 re-filters to ids 2 and 3.
    let engine = engine_with_mail();
    let reads = reads_with_mail();
    let stmt = parse("/mail/inbox |> WHERE id > 1").unwrap();
    let rows = block_on_read(&stmt, &engine.mounts, &reads).unwrap();
    assert_eq!(rows.len(), 2);
    let ids: Vec<i64> = rows
        .rows
        .iter()
        .map(|r| match r.values[0] {
            Value::Int(i) => i,
            _ => -1,
        })
        .collect();
    assert_eq!(ids, vec![2, 3]);
}

#[test]
fn headline_json_envelope_is_rows_object() {
    // The stable JSON contract: {"rows":[{id,subject}, …]}.
    let engine = engine_with_mail();
    let reads = reads_with_mail();
    let stmt = parse("/mail/inbox |> LIMIT 1").unwrap();
    let rows = block_on_read(&stmt, &engine.mounts, &reads).unwrap();
    let json = serde_json::to_value(&rows).unwrap();
    assert!(json["rows"].is_array());
    assert_eq!(json["rows"].as_array().unwrap().len(), 1);
    assert_eq!(json["rows"][0]["id"], 1);
    assert_eq!(json["rows"][0]["subject"], "hello");
}

// ---- One-shot orchestration: exit codes + renderers ----

fn run(src: &str, fmt: OutputFormat, commit: bool) -> (i32, String, String) {
    run_with_ack(src, fmt, commit, false)
}

fn run_with_ack(
    src: &str,
    fmt: OutputFormat,
    commit: bool,
    irreversible_ack: bool,
) -> (i32, String, String) {
    let engine = engine_with_mail();
    let reads = reads_with_mail();
    let ctx = ExecCtx {
        engine: &engine,
        reads: &reads,
        world_apply: None,
    };
    let source = StmtSource::Expr(src.to_string());
    let mut out: Vec<u8> = Vec::new();
    let mut err: Vec<u8> = Vec::new();
    let code = {
        let mut streams = Streams {
            out: &mut out,
            err: &mut err,
        };
        run_oneshot(&source, &ctx, fmt, commit, irreversible_ack, &mut streams).code()
    };
    (
        code,
        String::from_utf8(out).unwrap(),
        String::from_utf8(err).unwrap(),
    )
}

#[test]
fn oneshot_read_json_exit_zero_with_rows() {
    let (code, out, err) = run("/mail/inbox |> LIMIT 1", OutputFormat::Json, false);
    assert_eq!(code, 0);
    assert!(err.is_empty(), "data goes to stdout, not stderr");
    let v: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(v["rows"].as_array().unwrap().len(), 1);
}

#[test]
fn oneshot_read_table_renders_columns() {
    let (code, out, _err) = run("/mail/inbox", OutputFormat::Table, false);
    assert_eq!(code, 0);
    assert!(out.contains("id"));
    assert!(out.contains("subject"));
    assert!(out.contains("hello"));
    assert!(out.contains("3 row(s)"));
}

#[test]
fn oneshot_parse_error_exit_two_with_kind_parse() {
    let (code, _out, err) = run("this is not pipe sql", OutputFormat::Json, false);
    assert_eq!(code, 2);
    let v: serde_json::Value = serde_json::from_str(err.trim()).unwrap();
    assert_eq!(v["error"]["kind"], "parse");
}

#[test]
fn oneshot_relative_path_usage_exit_two() {
    let (code, _out, err) = run("mail/inbox |> LIMIT 1", OutputFormat::Json, false);
    assert_eq!(code, 2);
    let v: serde_json::Value = serde_json::from_str(err.trim()).unwrap();
    assert_eq!(v["error"]["kind"], "usage");
    assert_eq!(v["error"]["path"], "mail/inbox");
}

#[test]
fn oneshot_unknown_source_capability_exit_three() {
    // /nope has no mounted driver → planner rejects with unknown_source → exit 3.
    let (code, _out, err) = run("/nope/x |> LIMIT 1", OutputFormat::Json, false);
    assert_eq!(code, 3);
    let v: serde_json::Value = serde_json::from_str(err.trim()).unwrap();
    assert_eq!(v["error"]["kind"], "capability");
}

// ---- PREVIEW / COMMIT gate ----

#[test]
fn oneshot_effect_preview_exit_zero_with_counts() {
    // A non-destructive INSERT previews at exit 0 with the plan + per-target counts.
    let (code, out, _err) = run(
        "INSERT INTO /mail/inbox VALUES (9, 'x')",
        OutputFormat::Json,
        false,
    );
    assert_eq!(code, 0);
    let v: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(v["committed"], false);
    assert!(v["preview"]["rows"].is_array());
}

#[test]
fn oneshot_destructive_set_without_commit_exit_four() {
    // REMOVE over a set (no single-row bound) is irreversible + set-wide → exit 4 without
    // --commit. The PREVIEW is still rendered so the operator sees the affected counts.
    let (code, out, _err) = run("REMOVE /mail/inbox", OutputFormat::Json, false);
    assert_eq!(code, 4);
    let v: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(v["committed"], false);
    assert!(!v["preview"]["irreversible"].as_array().unwrap().is_empty());
}

#[test]
fn oneshot_commit_applies_against_in_memory_engine() {
    // With --commit AND the irreversible ack the plan applies via the in-memory engine; the
    // summary marks committed. (`REMOVE` is irreversible, so the t37 one-shot gate requires the
    // ack — see `oneshot_irreversible_commit_blocked_without_ack` for the fail-closed default.)
    let (code, out, _err) = run_with_ack("REMOVE /mail/inbox", OutputFormat::Json, true, true);
    assert_eq!(code, 0);
    let v: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(v["committed"], true);
}

#[test]
fn trailing_commit_keyword_applies_without_flag() {
    // A trailing COMMIT wrapper applies even without --commit (the CLI adds zero keywords). The
    // irreversible ack is still required for the irreversible REMOVE (t37): passed here.
    let (code, out, _err) =
        run_with_ack("COMMIT REMOVE /mail/inbox", OutputFormat::Json, false, true);
    assert_eq!(code, 0);
    let v: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(v["committed"], true);
}

// ---- t37: the irreversible-effect one-shot gate (RFD §6/§10) ----

#[test]
fn oneshot_irreversible_commit_blocked_without_ack() {
    // `qfs run … --commit` of an irreversible REMOVE in the NON-INTERACTIVE one-shot fails closed
    // (exit 4, commit_required) without `--commit-irreversible`. Zero effects apply; the PREVIEW
    // is still rendered so the operator sees what WOULD have applied. This is the t37 fail-closed
    // default that protects unattended one-shots from a silent destructive apply.
    let (code, out, err) = run_with_ack("REMOVE /mail/inbox", OutputFormat::Json, true, false);
    assert_eq!(
        code, 4,
        "irreversible --commit without ack must fail closed"
    );
    // The PREVIEW (not a commit) is on stdout: not committed, with the irreversible effect listed.
    let v: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(v["committed"], false);
    assert!(!v["preview"]["irreversible"].as_array().unwrap().is_empty());
    // The structured error names the irreversible-ack requirement.
    let e: serde_json::Value = serde_json::from_str(err.trim()).unwrap();
    assert_eq!(e["error"]["code"], "irreversible_ack_required");
}

#[test]
fn oneshot_reversible_commit_needs_no_ack() {
    // A REVERSIBLE single-row INSERT commits with no ack — the gate governs only irreversible
    // effects, so the common case is unaffected.
    let (code, out, _err) = run_with_ack(
        "INSERT INTO /mail/inbox VALUES (9, 'x')",
        OutputFormat::Json,
        true,
        false,
    );
    assert_eq!(code, 0);
    let v: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(v["committed"], true);
}

// ---- Golden Plan/PlanPreview for a representative effect statement (preview, no commit) ----

#[test]
fn golden_insert_plan_preview_json() {
    let engine = engine_with_mail();
    let stmt = parse("INSERT INTO /mail/inbox VALUES (9, 'x')").unwrap();
    let plan = qfs_exec::build_plan(&stmt, &engine).unwrap();
    let preview = qfs_exec::plan_preview(&plan);
    let v = serde_json::to_value(&preview).unwrap();
    // The plan has exactly one INSERT effect, not committed, not irreversible.
    assert_eq!(v["committed"], false);
    let rows = v["preview"]["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["verb"], "INSERT");
    assert_eq!(v["preview"]["is_pure"], false);
}
