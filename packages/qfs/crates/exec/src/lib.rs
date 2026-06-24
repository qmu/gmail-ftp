//! `qfs-exec` — the **execution / integration layer** (ticket t29): the end-to-end SELECT
//! read-path executor (the t20 carry-over closure) plus the one-shot CLI execution orchestration
//! (statement-source resolution, addressing validation, the PREVIEW/COMMIT safety gate, output
//! rendering, and the stable exit-code contract). The thin `qfs` bin / `qfs-cmd` dispatches into
//! [`run_oneshot`]; all the composition lives here.
//!
//! ## Crate topology (the t29 architectural decision)
//! `qfs-runtime`'s spine is deliberately `{qfs-plan, qfs-types}` and must not gain `qfs-core` /
//! `qfs-pushdown` / `qfs-engine`; `qfs-cmd` must stay logic-free (the t01 C4 guard forbids it a
//! direct `qfs-lang/plan/driver/codec/parser` dep). The read executor needs
//! `qfs-pushdown + qfs-engine + qfs-core` and async scans. So it lives **here**, in a new
//! integration crate that sits ABOVE the spine and composes those pieces. Every existing
//! confinement holds:
//!  - **Runtime minimal spine** — `qfs-exec` does **not** depend on `qfs-runtime`; it owns its
//!    own async [`ReadDriver`](read::ReadDriver) read seam (the runtime's write `ApplyDriver`
//!    only returns affected counts, never rows, so it is structurally not a read seam). The
//!    runtime confinement guard fires only on `qfs-runtime` consumers, so it is untouched.
//!  - **cmd logic-free** — `qfs-cmd → qfs-exec` is allowed (C4 forbids only
//!    `qfs-lang/plan/driver/codec/parser`; `qfs-exec` is none of them).
//!  - **No spine inversion** — nothing in the pure spine depends back onto `qfs-exec`, so tokio
//!    stays out of the spine's closure (`qfs-plan`'s purity dep-closure test is unaffected).

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

mod addressing;
mod dto;
mod error;
mod exec;
mod output;
mod read;
pub mod shell;

pub use dto::{PlanPreview, RowSet};
pub use error::{ErrorKind, ExecError, ExitCode};
pub use exec::{
    apply_commit, block_on_read, build_plan, execute_read, map_qfs_error, parse, plan_preview,
};
pub use output::{JsonRenderer, OutputFormat, Renderer, TableRenderer};
pub use read::{ReadDriver, ReadRegistry};
pub use shell::{Builtin, Completer, Outcome, Session, VfsPath};

// `run_describe` (ticket t39) is defined below in this module; re-export is implicit (pub fn).

use std::io::Write;

use qfs_core::{Engine, Plan};
use qfs_parser::{PlanWrap, Statement};

/// Where the one-shot statement text came from (exactly one source per invocation).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StmtSource {
    /// A positional `qfs run '<stmt>'`.
    Positional(String),
    /// A `-e <stmt>` flag.
    Expr(String),
    /// `qfs run -` (read the statement from stdin).
    Stdin(String),
}

impl StmtSource {
    /// The statement text, regardless of where it came from.
    #[must_use]
    pub fn text(&self) -> &str {
        match self {
            StmtSource::Positional(s) | StmtSource::Expr(s) | StmtSource::Stdin(s) => s,
        }
    }
}

/// Resolve exactly one statement source from the three mutually-exclusive inputs. Errors with a
/// `usage` error (exit 2) on zero or more than one source.
///
/// # Errors
/// [`ExecError`] (kind `usage`) if not exactly one of `positional` / `expr` / `stdin` is set.
pub fn resolve_source(
    positional: Option<String>,
    expr: Option<String>,
    stdin: Option<String>,
) -> Result<StmtSource, ExecError> {
    let mut sources = Vec::new();
    if let Some(s) = positional {
        sources.push(StmtSource::Positional(s));
    }
    if let Some(s) = expr {
        sources.push(StmtSource::Expr(s));
    }
    if let Some(s) = stdin {
        sources.push(StmtSource::Stdin(s));
    }
    match sources.len() {
        1 => Ok(sources.into_iter().next().unwrap_or_else(|| {
            // Unreachable: len == 1. Kept total without a panic for the lib lints.
            StmtSource::Expr(String::new())
        })),
        0 => Err(ExecError::usage(
            "no statement provided; pass a statement, `-e <stmt>`, or `-` for stdin",
        )),
        n => Err(ExecError::usage(format!(
            "{n} statement sources provided; exactly one of <stmt> / -e / - is allowed"
        ))),
    }
}

/// One render destination pair: data → stdout, errors → stderr (ticket t29).
pub struct Streams<'a> {
    /// Where rendered data goes (stdout).
    pub out: &'a mut dyn Write,
    /// Where rendered errors go (stderr).
    pub err: &'a mut dyn Write,
}

/// The read-time wiring the executor needs, beyond the engine's registries: the
/// [`ReadRegistry`] of `ReadDriver` scan implementations. Threaded separately so the `Engine`
/// (the introspective + commit registries) stays the t27 shape and a caller can register the
/// read facet independently (tests register only an in-memory fake).
pub struct ExecCtx<'a> {
    /// The shared engine (mount registry for describe/pushdown, secrets, codecs).
    pub engine: &'a Engine,
    /// The read-driver registry the read executor resolves each scan through.
    pub reads: &'a ReadRegistry,
}

/// Execute one statement end-to-end and render the result, returning the process [`ExitCode`].
/// This is the single one-shot entry the CLI calls. Never panics; never touches a cwd.
///
/// Pipeline:
/// 1. validate addressing (absolute / `id:` only) — relative path → usage (exit 2);
/// 2. parse — bad syntax → `{"error":{"kind":"parse"}}` (exit 2);
/// 3. unwrap a PREVIEW/COMMIT wrapper (a trailing `COMMIT` forces apply);
/// 4. **read** statement → run the read executor → render `RowSet`;
/// 5. **effect** statement → build the plan; if pure or not committing render PREVIEW (exit 0),
///    unless it is a **destructive set-wide** plan without `--commit` (exit 4); on `--commit`
///    apply via the in-memory engine and render the committed summary.
pub fn run_oneshot(
    source: &StmtSource,
    ctx: &ExecCtx,
    fmt: OutputFormat,
    commit_flag: bool,
    irreversible_ack: bool,
    streams: &mut Streams,
) -> ExitCode {
    match run_oneshot_inner(source, ctx, fmt, commit_flag, irreversible_ack, streams) {
        Ok(code) => code,
        Err(err) => {
            let renderer = fmt.renderer();
            let _ = renderer.error(&err, streams.err);
            err.exit_code()
        }
    }
}

fn run_oneshot_inner(
    source: &StmtSource,
    ctx: &ExecCtx,
    fmt: OutputFormat,
    commit_flag: bool,
    irreversible_ack: bool,
    streams: &mut Streams,
) -> Result<ExitCode, ExecError> {
    let src = source.text();

    // 1. Addressing gate (no cwd in one-shot mode).
    addressing::validate(src)?;

    // 2. Parse.
    let stmt = parse(src)?;

    // 3. Unwrap a PREVIEW/COMMIT wrapper. A trailing `COMMIT` (the engine's keyword) forces
    //    apply, OR'd with the `--commit` switch — the CLI adds zero keywords.
    let (inner, commit) = unwrap_plan(&stmt, commit_flag);

    let renderer = fmt.renderer();

    match inner {
        // 4. Read path: the t20 carry-over closure.
        Statement::Query(_) => {
            let rows = block_on_read(inner, &ctx.engine.mounts, ctx.reads)?;
            renderer.rows(&rows, streams.out).map_err(io_err)?;
            Ok(ExitCode::Ok)
        }
        // 5. Effect / DDL path: PREVIEW by default, COMMIT on demand.
        Statement::Effect(_) | Statement::Ddl(_) => {
            let plan = build_plan(inner, ctx.engine)?;
            if commit {
                // The irreversible-effect gate (t37, RFD §6/§10). `qfs run … --commit` is a
                // NON-INTERACTIVE one-shot (no TTY to confirm on), so an irreversible plan
                // (REMOVE / declared-irreversible CALL) is refused unless the operator passed
                // `--commit-irreversible`. We still rendered nothing yet, so a block is a clean
                // fail-closed refusal that applies ZERO effects.
                let ack = if irreversible_ack {
                    qfs_core::Ack::Granted
                } else {
                    qfs_core::Ack::Absent
                };
                if let Err(needs) = qfs_core::IrreversibleGuard::require_ack(
                    &plan,
                    qfs_core::RunMode::CliOneShot,
                    ack,
                ) {
                    // Render the PREVIEW so the operator sees exactly what would have applied,
                    // then refuse on the commit-required exit class with the irreversible reason.
                    let summary = plan_preview(&plan);
                    renderer.plan(&summary, streams.out).map_err(io_err)?;
                    return Err(ExecError::new(
                        ErrorKind::CommitRequired,
                        "irreversible_ack_required",
                        needs.reason(),
                    ));
                }
                let summary = apply_commit(&plan)?;
                renderer.plan(&summary, streams.out).map_err(io_err)?;
                Ok(ExitCode::Ok)
            } else if is_destructive_set(&plan) {
                // A destructive set-wide plan requires explicit commit (exit 4). Still render
                // the PREVIEW so the operator/agent sees the affected counts.
                let summary = plan_preview(&plan);
                renderer.plan(&summary, streams.out).map_err(io_err)?;
                Err(ExecError::new(
                    ErrorKind::CommitRequired,
                    "commit_required",
                    "destructive set-wide plan: re-run with --commit (or a trailing COMMIT) to apply",
                ))
            } else {
                let summary = plan_preview(&plan);
                renderer.plan(&summary, streams.out).map_err(io_err)?;
                Ok(ExitCode::Ok)
            }
        }
        // A nested PlanWrap is already unwrapped by `unwrap_plan`; this arm is unreachable but
        // kept total (no panic) by treating it as a pure preview.
        Statement::Plan(_) => Ok(ExitCode::Ok),
    }
}

/// Execute `qfs describe <path>` (ticket t39): resolve `path` to its driver in the describe
/// registry, fold the driver's pure introspective half into a [`qfs_core::DescribeReport`], and
/// render it via the t29 output layer (human table / JSON). Returns the process [`ExitCode`].
///
/// DESCRIBE is **pure** — no creds, no I/O, no network: the registry holds describe-only drivers
/// and only the introspective half is touched (the applier seam is never reached). An
/// unresolvable path or a non-describable node renders a structured error (exit 2/3) — the
/// agent-legible failure path — never a panic.
pub fn run_describe(
    path: &str,
    describe: &qfs_core::MountRegistry,
    fmt: OutputFormat,
    streams: &mut Streams,
) -> ExitCode {
    match run_describe_inner(path, describe, fmt, streams) {
        Ok(code) => code,
        Err(err) => {
            let renderer = fmt.renderer();
            let _ = renderer.error(&err, streams.err);
            err.exit_code()
        }
    }
}

fn run_describe_inner(
    path: &str,
    describe: &qfs_core::MountRegistry,
    fmt: OutputFormat,
    streams: &mut Streams,
) -> Result<ExitCode, ExecError> {
    // Addressing gate: DESCRIBE addresses an absolute path (no cwd in one-shot mode), same as
    // `qfs run`. A relative path is a usage error (exit 2).
    addressing::validate_path(path)?;

    // Resolve the path to its describe-only driver (longest-mount-prefix match).
    let (driver, _rest) = describe.resolve_path(path).ok_or_else(|| {
        ExecError::new(
            ErrorKind::Capability,
            "unknown_mount",
            format!("no driver is mounted for `{path}` (describe registry)"),
        )
        .with_path(path)
    })?;

    // Fold the introspective half into the report — pure, no I/O, no creds.
    let report = qfs_core::DescribeReport::from_driver(driver.as_ref(), &qfs_core::Path::new(path))
        .map_err(|e| ExecError::from_qfs(&e))?;

    let renderer = fmt.renderer();
    renderer.describe(&report, streams.out).map_err(io_err)?;
    Ok(ExitCode::Ok)
}

/// Unwrap a `PREVIEW`/`COMMIT` plan wrapper, returning the inner statement and whether to commit
/// (a trailing `COMMIT` OR the `--commit` switch). PREVIEW/COMMIT nest at most once in practice;
/// the loop is defensive.
fn unwrap_plan(stmt: &Statement, commit_flag: bool) -> (&Statement, bool) {
    let mut cur = stmt;
    let mut commit = commit_flag;
    while let Statement::Plan(PlanWrap {
        commit: c, inner, ..
    }) = cur
    {
        commit = commit || *c;
        cur = inner;
    }
    (cur, commit)
}

/// Whether a plan is **destructive over a set** — the exit-4 gate. Grammar-agnostic: it reads the
/// plan's effect metadata (`irreversible` + the affected estimate), never CLI keywords. A plan is
/// destructive-set-wide iff it has an irreversible effect that could touch **more than one** row
/// (an `AtMost(n>1)` / `Unknown` estimate). A single-row irreversible effect (`Exact(0)`/`(1)`,
/// `AtMost(1)`) is not "over a set" and previews at exit 0.
#[must_use]
fn is_destructive_set(plan: &Plan) -> bool {
    use qfs_core::Affected;
    plan.nodes().iter().any(|n| {
        n.irreversible
            && match n.est_affected {
                Affected::Exact(c) | Affected::AtMost(c) => c > 1,
                Affected::Unknown => true,
            }
    })
}

/// Wrap a writer `io::Error` as an internal [`ExecError`] (a broken stdout/stderr pipe is not a
/// user error, but it must not panic).
fn io_err(e: std::io::Error) -> ExecError {
    ExecError::new(ErrorKind::Internal, "io_error", e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_source_requires_exactly_one() {
        assert!(resolve_source(Some("a".into()), None, None).is_ok());
        assert!(resolve_source(None, Some("a".into()), None).is_ok());
        assert!(resolve_source(None, None, Some("a".into())).is_ok());
        // Zero sources.
        let e = resolve_source(None, None, None).unwrap_err();
        assert_eq!(e.kind.as_str(), "usage");
        // Two sources.
        let e = resolve_source(Some("a".into()), Some("b".into()), None).unwrap_err();
        assert_eq!(e.kind.as_str(), "usage");
    }

    #[test]
    fn stmt_source_text_unwraps() {
        assert_eq!(StmtSource::Expr("x".into()).text(), "x");
        assert_eq!(StmtSource::Stdin("y".into()).text(), "y");
    }
}
