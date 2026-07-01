//! The end-to-end **read-path executor** (the t20 carry-over closure) and the **commit**
//! apply path.
//!
//! ## Read pipeline (`execute_read`)
//! Ties the previously-disconnected pieces into one executor:
//!
//! ```text
//!   parse (qfs-core/parser)
//!     -> resolve + capability-gate (qfs-core Resolver/Evaluator)
//!     -> build PhysicalPlan (qfs-core::plan_query: AST -> pushdown split, from the AST per O-t07-1)
//!     -> run each native ScanNode through the ReadDriver seam (async I/O, tokio)
//!     -> combine + residual re-filter via qfs-engine MiniEvaluator (the t20 property)
//!     -> owned RowSet
//! ```
//!
//! The scans for a single physical plan run **concurrently** (independent leaves), then the
//! `MiniEvaluator` walks the plan positionally over the per-scan batches — exactly the
//! `ScanResults` contract `qfs-engine` already defines. Because a driver may honestly
//! over-return rows (ignore a `LIMIT`/`WHERE` it cannot push), the engine's residual re-filter
//! restores correctness over the over-returned rows.
//!
//! ## Why it lives here, above the spine
//! This composes `qfs-core` + `qfs-pushdown` + `qfs-engine` (+ tokio for scans). The runtime's
//! spine is deliberately `{qfs-plan, qfs-types}` and must not gain those; `qfs-cmd` must stay
//! logic-free. So the executor sits in this integration crate ABOVE all of them, and the thin
//! `qfs` bin / `qfs-cmd` dispatches into it. Nothing in the pure spine depends back onto this
//! crate, so tokio stays out of the spine's closure.

use qfs_core::{commit, plan_query, preview, CfsError, Engine, MountRegistry, Plan, RowBatch};
use qfs_engine::{CombineEngine, MiniEvaluator, ScanResults};
use qfs_parser::{parse_statement, Statement};

use crate::dto::{PlanPreview, RowSet};
use crate::error::{ErrorKind, ExecError};
use crate::read::ReadRegistry;

/// Execute a pure read [`Statement`] end-to-end against the live registries, returning the
/// owned [`RowSet`]. This is the headline t29 path: `/<src>/… |> WHERE … |> LIMIT n`
/// returns rows through the REAL pipeline (parse already done by the caller; resolve → plan →
/// scan → residual → rows here).
///
/// Scans are async I/O; the caller runs this future on a tokio runtime (see [`block_on_read`]).
///
/// # Errors
/// [`ExecError`] with the mapped `kind`/exit-code if resolution, planning, or a scan fails.
pub async fn execute_read(
    stmt: &Statement,
    mounts: &MountRegistry,
    reads: &ReadRegistry,
) -> Result<RowSet, ExecError> {
    // 1. Build the PhysicalPlan (pushdown split) from the AST via the live registry. This is
    //    the qfs-core t14 seam: lower_query (from the AST, O-t07-1) -> partition_by_source.
    let physical = plan_query(stmt, mounts).map_err(map_pushdown_error)?;

    // 2. Execute each native scan through the ReadDriver seam, in plan (left-to-right) order —
    //    the positional order ScanResults/MiniEvaluator consume.
    let scan_nodes = physical.scans();
    let mut batches: Vec<RowBatch> = Vec::with_capacity(scan_nodes.len());
    for scan in scan_nodes {
        let driver = reads.get(&id_of(scan.source.as_str())).ok_or_else(|| {
            ExecError::new(
                ErrorKind::Capability,
                "unknown_source",
                format!("no read driver registered for source `{}`", scan.source),
            )
            .with_path(scan.source.to_string())
        })?;
        let batch = driver
            .scan(scan)
            .await
            .map_err(|e| ExecError::from_qfs(&e))?;
        batches.push(batch);
    }

    // 3. Combine + re-apply the residual locally (the t20 property): the MiniEvaluator walks
    //    the PhysicalPlan, pulling one batch per scan leaf and folding each residual CombineOp
    //    (Filter/Project/Limit/…) over the over-returned rows.
    let out = MiniEvaluator::new()
        .execute(&physical, ScanResults::new(batches))
        .map_err(|e| ExecError::new(ErrorKind::Internal, e.code(), e.to_string()))?;

    // 4. Apply any trailing DECODE/ENCODE codec stages locally (pushdown drops them; they are
    //    schema-shaping, driver-independent transforms). A no-op when the pipeline has none.
    let out = crate::codec::apply_codecs(out, stmt)?;

    Ok(RowSet::from_batch(out))
}

/// The plan id for a pushdown [`SourceId`] string — the executor keys the [`ReadRegistry`] on
/// the same owned [`DriverId`] the planner tagged each scan with.
fn id_of(source: &str) -> qfs_core::DriverId {
    qfs_core::DriverId::new(source)
}

/// Map a `qfs-core` pushdown error into the executor's structured error. A capability denial
/// (a source that cannot SELECT) becomes exit 3; an unknown source becomes capability too; a
/// lowering failure (a malformed query the planner cannot represent) is a parse/usage class.
fn map_pushdown_error(err: qfs_core::PushdownError) -> ExecError {
    use qfs_core::PushdownError;
    let kind = match err {
        // PlanError::CapabilityDenied / UnknownSource — both "this source/op is unavailable".
        PushdownError::Plan(_) => ErrorKind::Capability,
        // A lowering failure (a malformed/unsupported query shape) is a usage-class problem.
        PushdownError::Lower(_) | PushdownError::NotAQuery => ErrorKind::Usage,
        // PushdownError is #[non_exhaustive]: an unmodeled future arm degrades to usage.
        _ => ErrorKind::Usage,
    };
    // The inner LowerError/PlanError implement Display (clean, secret-free messages);
    // PushdownError itself does not, so we render the inner error.
    let message = match &err {
        PushdownError::NotAQuery => "expected a read query (/path |> …)".to_string(),
        PushdownError::Lower(l) => l.to_string(),
        PushdownError::Plan(p) => p.to_string(),
        other => format!("{other:?}"),
    };
    ExecError::new(kind, err.code(), message)
}

/// Run the async read executor to completion on a fresh current-thread tokio runtime, returning
/// the owned [`RowSet`]. The synchronous entry the CLI calls (one-shot, single statement).
///
/// # Errors
/// [`ExecError`] if the runtime cannot be built or the read fails.
pub fn block_on_read(
    stmt: &Statement,
    mounts: &MountRegistry,
    reads: &ReadRegistry,
) -> Result<RowSet, ExecError> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| {
            ExecError::new(
                ErrorKind::Internal,
                "runtime_init",
                format!("failed to start read runtime: {e}"),
            )
        })?;
    rt.block_on(execute_read(stmt, mounts, reads))
}

/// Build the effect [`Plan`] for an effect [`Statement`] via the engine evaluator (resolve +
/// capability-gate + plan construction). Pure: constructs effects-as-data, applies nothing.
///
/// # Errors
/// [`ExecError`] if resolution / capability gating / plan construction fails.
pub fn build_plan(stmt: &Statement, engine: &Engine) -> Result<Plan, ExecError> {
    use qfs_core::{EvalValue, Evaluator, StdlibRegistry};
    // Wire the core function registry so the plan pass runs the **static primitive type
    // checker** at plan time (decision T, ticket t75): a mismatched `SET … WHERE` / `REMOVE …
    // WHERE` filter comparison, a built-in handed a bad argument type, or a lambda applied to
    // the wrong element type is a structured plan-time error here — before any effect node is
    // applied, so a type-failing plan can never reach commit. `Evaluator::new` (late-bound)
    // would leave the checker inert; the stdlib-wired evaluator is what makes it ACTIVE on the
    // production effect path.
    let stdlib = StdlibRegistry::with_core();
    let evaluator = Evaluator::with_stdlib(&engine.mounts, &stdlib);
    match evaluator.eval(stmt).map_err(map_eval_error)? {
        EvalValue::Plan(plan) => Ok(plan),
        // A pure query has no effect plan; the read path handles it. Treat as an empty plan
        // so the caller can detect "no effects" uniformly.
        EvalValue::Relation(_) => Ok(Plan::pure()),
    }
}

/// Render the dry-run [`PlanPreview`] of an effect plan (no I/O, applies nothing).
#[must_use]
pub fn plan_preview(plan: &Plan) -> PlanPreview {
    PlanPreview::preview(preview(plan))
}

/// Apply an effect [`Plan`] to the World, returning the committed [`PlanPreview`]. If a real
/// applier is injected ([`crate::WorldApply`], supplied by the binary because `qfs-exec` is
/// confined off `qfs-runtime`), the plan is driven through it (the real interpreter-backed
/// commit). With none, it falls back to the in-memory [`apply_commit`] — the shape unit tests use.
///
/// # Errors
/// [`ExecError`] (kind `commit_failed`) if the injected applier (or the in-memory fallback) fails.
pub fn apply_via(plan: &Plan, world: Option<&crate::WorldApply>) -> Result<PlanPreview, ExecError> {
    match world {
        Some(apply) => {
            apply(plan)?;
            Ok(PlanPreview::committed(preview(plan)))
        }
        None => apply_commit(plan),
    }
}

/// Apply an effect [`Plan`] against the in-memory engine: the preview-grade fallback using the
/// applier-based [`commit`] over a [`qfs_core::RecordingApplier`] test double — **no live creds,
/// no network, no real I/O**. The real commit is the injected [`crate::WorldApply`] (see
/// [`apply_via`]). Returns the committed [`PlanPreview`] on success.
///
/// # Errors
/// [`ExecError`] (kind `commit_failed`, exit 5) if any leg failed to apply.
pub fn apply_commit(plan: &Plan) -> Result<PlanPreview, ExecError> {
    let mut applier = qfs_core::RecordingApplier::new();
    let report = commit(plan, &mut applier, |_| {});
    if let Some(err) = report.failed {
        return Err(ExecError::new(
            ErrorKind::CommitFailed,
            "commit_failed",
            err.to_string(),
        ));
    }
    Ok(PlanPreview::committed(preview(plan)))
}

/// Parse one statement, mapping a parser failure into the executor's structured parse error
/// (kind `parse`, exit 2) carrying the parser's stable code in `detail`.
///
/// # Errors
/// [`ExecError`] (kind `parse`) on any lex/parse failure.
pub fn parse(src: &str) -> Result<Statement, ExecError> {
    parse_statement(src)
        .map_err(|e| ExecError::parse(e.message.clone()).with_detail(e.code.as_str().to_string()))
}

/// Map a `qfs-core` evaluation error into the executor's structured error.
fn map_eval_error(err: qfs_core::EvalError) -> ExecError {
    use qfs_core::EvalError;
    let kind = match &err {
        // A resolve-time capability denial / unknown driver / unknown proc is exit 3.
        EvalError::Resolve(_) => ErrorKind::Capability,
        EvalError::UnroutedPath { .. } => ErrorKind::Capability,
        // A type error in the query is a usage-class problem.
        EvalError::Type(_) | EvalError::Fn(_) => ErrorKind::Usage,
        // A non-constant VALUES cell or a driver write-lowering rejection are usage problems.
        EvalError::NonLiteralValues { .. } | EvalError::DriverWrite { .. } => ErrorKind::Usage,
        _ => ErrorKind::Internal,
    };
    // EvalError has no Display; its owned, secret-free Debug is the machine-facing message.
    ExecError::new(kind, err.code(), format!("{err:?}"))
}

/// Re-map a [`CfsError`] through [`ExecError::from_qfs`] (re-exported for the CLI).
#[must_use]
pub fn map_qfs_error(err: &CfsError) -> ExecError {
    ExecError::from_qfs(err)
}
