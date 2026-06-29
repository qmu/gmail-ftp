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
mod codec;
mod dto;
mod error;
mod exec;
mod output;
mod read;
pub mod shell;

pub use dto::{PlanPreview, RowSet};
pub use error::{ErrorKind, ExecError, ExitCode};
pub use exec::{
    apply_commit, apply_via, block_on_read, build_plan, execute_read, map_qfs_error, parse,
    plan_preview,
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
    /// The injected real-commit hook (the binary's interpreter-backed applier). `None` falls back
    /// to the in-memory [`apply_commit`] (preview-grade, no real I/O) — what unit tests use.
    pub world_apply: Option<&'a WorldApply<'a>>,
    /// The active selectable **safety mode** (t59) governing this one-shot's commit gate, resolved
    /// by the binary from the deployment setting (`/sys/settings` → env → safe default). The
    /// `Default` is [`SafetyMode::AutonomousInPolicy`](qfs_core::SafetyMode) — the historical CLI
    /// posture (reversible-in-policy applies, irreversible needs `--commit-irreversible`).
    pub safety_mode: qfs_core::SafetyMode,
}

/// A binary-injected "apply this plan to the World" hook (RFD §6 `COMMIT : Plan -> World`).
/// `qfs-exec` is deliberately confined off `qfs-runtime` (the interpreter), so the real commit is
/// supplied by the terminal binary, which owns the `qfs-runtime` interpreter + the live driver
/// registry. Returns `Ok(())` once every leg applied, or an [`ExecError`] on a commit failure.
pub type WorldApply<'a> = dyn Fn(&qfs_core::Plan) -> Result<(), ExecError> + 'a;

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

    // Classify by the program's *terminal* statement: a `LET` program (M6, t60) is a read or a
    // write according to the statement its bindings lead into. The full `inner` (bindings and
    // all) is handed to the read/effect path — the evaluator folds the bindings through it.
    match terminal_statement(inner) {
        // 4. Read path: the t20 carry-over closure.
        Statement::Query(_) => {
            let rows = block_on_read(inner, &ctx.engine.mounts, ctx.reads)?;
            renderer.rows(&rows, streams.out).map_err(io_err)?;
            Ok(ExitCode::Ok)
        }
        // 5. Effect / DDL / TRANSACTION path: PREVIEW by default, COMMIT on demand. A
        // `TRANSACTION { … }` block (M6, t62) lowers to one effect plan (reversible-only,
        // all-or-nothing), so it routes through the same plan/commit machinery as a plain effect.
        Statement::Effect(_) | Statement::Ddl(_) | Statement::Transaction { .. } => {
            let plan = build_plan(inner, ctx.engine)?;
            if commit {
                // The selectable safety mode (t59) governs this one-shot's commit, composed on the
                // t37 irreversible floor. `qfs run … --commit` is a NON-INTERACTIVE one-shot (no TTY
                // to confirm on), so a held effect is refused unless the operator passed the explicit
                // `--commit-irreversible` ack. `within_policy` is `true` here: the CLI one-shot trusts
                // the local operator's capability set (capability gating already ran at parse time;
                // the server faces gate with their POLICY instead). The mode then decides:
                //   - Autonomous-in-policy (default): reversible applies, irreversible held (ack).
                //   - Approve-everything: BOTH held — even a reversible write is refused without ack.
                //   - Policy-only: BOTH auto-apply unattended (CI). We still rendered nothing yet, so
                //     a hold is a clean fail-closed refusal that applies ZERO effects.
                let ack = if irreversible_ack {
                    qfs_core::Ack::Granted
                } else {
                    qfs_core::Ack::Absent
                };
                if qfs_core::IrreversibleGuard::decide(&plan, ctx.safety_mode, true, ack)
                    != qfs_core::SafetyDecision::AutoCommit
                {
                    // Render the PREVIEW so the operator sees exactly what would have applied, then
                    // refuse on the commit-required exit class. The code distinguishes the held
                    // cause: the irreversible-ack floor vs. an approve-everything hold on a write.
                    let summary = plan_preview(&plan);
                    renderer.plan(&summary, streams.out).map_err(io_err)?;
                    let (code, message) = held_commit_reason(ctx.safety_mode, &plan);
                    return Err(ExecError::new(ErrorKind::CommitRequired, code, message));
                }
                let summary = apply_via(&plan, ctx.world_apply)?;
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
        // `terminal_statement` descends through PlanWrap/LET to a leaf, so these arms are
        // unreachable; kept total (no panic) by treating them as a pure preview.
        Statement::Plan(_) | Statement::Let { .. } => Ok(ExitCode::Ok),
    }
}

/// The stable `(code, message)` for a one-shot commit HELD by the active safety mode (t59). An
/// irreversible plan keeps the historical `irreversible_ack_required` contract (so the exit-class
/// code is stable across modes); a reversible plan held by the *approve-everything* mode reports
/// `approval_required`. Both fail closed on the `commit_required` exit class (exit 4) with a
/// secret-free message naming the ack to supply.
fn held_commit_reason(mode: qfs_core::SafetyMode, plan: &qfs_core::Plan) -> (&'static str, String) {
    if plan.is_irreversible() {
        (
            "irreversible_ack_required",
            "plan contains an irreversible effect (REMOVE / CALL); re-run with \
             --commit-irreversible to apply (or in an interactive session)"
                .to_string(),
        )
    } else {
        (
            "approval_required",
            format!(
                "the `{mode}` safety mode holds every write for explicit approval; re-run with \
                 --commit-irreversible to apply this write"
            ),
        )
    }
}

/// The terminal statement a program leads into, descending through `LET` bindings (M6, t60) and
/// `PREVIEW`/`COMMIT` wrappers to the underlying read/effect/DDL leaf. Used to route a `LET`
/// program to the read or effect path — the leaf is never a `LET` or a `Plan`.
fn terminal_statement(stmt: &Statement) -> &Statement {
    match stmt {
        Statement::Let { body, .. } => terminal_statement(body),
        Statement::Plan(PlanWrap { inner, .. }) => terminal_statement(inner),
        other => other,
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
