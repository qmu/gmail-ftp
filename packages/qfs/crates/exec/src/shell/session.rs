//! The interactive session (ticket t28): the stateful cwd, the line evaluator, and the
//! PREVIEW/COMMIT safety gate. This is the shell's brain — the REPL loop (in `qfs-cmd`) only
//! feeds it lines and renders [`Outcome`]s.
//!
//! ## No new semantics
//! Every line — builtin or raw — funnels through [`Session::eval_line`] into the SAME pipeline
//! the one-shot path uses: a read becomes [`block_on_read`](crate::block_on_read) over the
//! [`ReadDriver`] seam; an effect becomes [`build_plan`](crate::build_plan) →
//! [`plan_preview`](crate::plan_preview) (PREVIEW) or [`apply_commit`](crate::apply_commit)
//! (COMMIT). The shell adds no execution behaviour; it only desugars sugar to core statements
//! and gates destructive effects behind an explicit, typed `COMMIT`.
//!
//! ## The safety invariant
//! PREVIEW is the default. `cp`/`mv`/`rm` (and any raw effect) print their affected counts and
//! the plan preview, and apply **nothing** until the operator types `COMMIT` (or wraps the line
//! `COMMIT …`). A builtin can never shortcut around this — it lowers to the same `Plan` and the
//! same gate. `cd`/`pwd` are pure cwd state changes (no plan).

use qfs_core::{Engine, MountRegistry};

use crate::dto::{PlanPreview, RowSet};
use crate::error::{ErrorKind, ExecError};
use crate::exec::{apply_commit, block_on_read, build_plan, parse, plan_preview};
use crate::read::ReadRegistry;
use crate::shell::desugar::{classify, desugar, Builtin, Line};
use crate::shell::path::{resolve, VfsPath};

/// The outcome of evaluating one line — what the REPL renders.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// A pure read produced rows (`ls`/`cat`/a raw `SELECT`).
    Listing(RowSet),
    /// One or more effect plans previewed (nothing applied) — the affected counts + plan.
    Preview(Vec<PlanPreview>),
    /// One or more effect plans committed (applied) — the committed summaries.
    Committed(Vec<PlanPreview>),
    /// A `cd` changed the cwd; carries the new location for the prompt.
    Moved(VfsPath),
    /// A `pwd` — print the cwd.
    Cwd(VfsPath),
    /// Nothing to do (an empty line).
    Empty,
}

/// The interactive session: the tagged cwd plus the engine + read registries every line is
/// evaluated against. Holds no terminal, no history — those belong to the REPL driver, so the
/// session is fully testable by feeding scripted lines and asserting [`Outcome`]s.
pub struct Session<'a> {
    /// The current working location (`{driver, path}`), rendered into the prompt.
    cwd: VfsPath,
    /// The shared engine (mount registry for describe/pushdown/plan; codecs; secrets).
    engine: &'a Engine,
    /// The read-driver registry the read path resolves scans through.
    reads: &'a ReadRegistry,
}

impl<'a> Session<'a> {
    /// Start a session at `cwd` against the live registries.
    #[must_use]
    pub fn new(cwd: VfsPath, engine: &'a Engine, reads: &'a ReadRegistry) -> Self {
        Self { cwd, engine, reads }
    }

    /// The current working location (for the prompt / `pwd`).
    #[must_use]
    pub fn cwd(&self) -> &VfsPath {
        &self.cwd
    }

    /// The prompt string `{driver}:{path}$ ` reflecting the cwd (RFD §7).
    #[must_use]
    pub fn prompt(&self) -> String {
        let path = if self.cwd.is_root() {
            "/".to_string()
        } else {
            format!("/{}", self.cwd.segments().join("/"))
        };
        format!("{}:{}$ ", self.cwd.driver(), path)
    }

    /// Evaluate one typed line. Dispatches builtin vs raw, applies the PREVIEW/COMMIT gate, and
    /// returns the [`Outcome`] to render. `commit` is the session's apply switch (the REPL sets
    /// it when the operator typed a bare `COMMIT` confirmation or a `COMMIT …`-wrapped line).
    ///
    /// # Errors
    /// [`ExecError`] with the mapped kind on any parse / capability / commit failure.
    pub fn eval_line(&mut self, line: &str, commit: bool) -> Result<Outcome, ExecError> {
        match classify(line) {
            Line::Empty => Ok(Outcome::Empty),
            Line::Builtin { verb, args } => self.eval_builtin(verb, &args, commit),
            Line::Raw(stmt) => self.eval_statements(&[stmt], commit),
        }
    }

    /// Evaluate a recognised builtin. `cd`/`pwd` are pure state changes; the rest desugar to
    /// closed-core statements and route through [`Self::eval_statements`].
    fn eval_builtin(
        &mut self,
        verb: Builtin,
        args: &[String],
        commit: bool,
    ) -> Result<Outcome, ExecError> {
        match verb {
            Builtin::Pwd => Ok(Outcome::Cwd(self.cwd.clone())),
            Builtin::Cd => {
                let raw = args.first().map_or("~", String::as_str);
                let target = resolve(raw, &self.cwd)?;
                self.validate_namespace(&target)?;
                self.cwd = target.clone();
                Ok(Outcome::Moved(target))
            }
            _ => {
                let d = desugar(verb, args, &self.cwd)?;
                self.eval_statements(&d.statements, commit)
            }
        }
    }

    /// Run a batch of qfs source statements (one for most builtins/raw lines; several for `mv`
    /// and `rm a b c`). A read batch returns a [`Outcome::Listing`]; an effect batch is
    /// previewed (default) or committed, with the affected counts surfaced.
    fn eval_statements(&self, statements: &[String], commit: bool) -> Result<Outcome, ExecError> {
        // Determine read-vs-effect from the FIRST statement; a builtin batch is homogeneous by
        // construction (a read `ls`/`cat`, or an effect `cp`/`mv`/`rm`).
        let first = statements
            .first()
            .ok_or_else(|| ExecError::usage("empty statement batch"))?;
        let stmt = parse(first)?;
        let (inner, line_commit) = unwrap_commit(&stmt);

        if matches!(inner, qfs_parser::Statement::Query(_)) {
            // A read: a single listing (builtins never batch reads).
            let rows = block_on_read(inner, &self.engine.mounts, self.reads)?;
            return Ok(Outcome::Listing(rows));
        }

        // An effect batch. Build every leg's plan first (so a parse/capability error aborts the
        // whole batch before any apply), then preview or commit uniformly.
        let do_commit = commit || line_commit;
        let mut plans = Vec::with_capacity(statements.len());
        for s in statements {
            let parsed = parse(s)?;
            let (eff, c) = unwrap_commit(&parsed);
            // A `COMMIT`-wrapped leg promotes the whole batch to commit (typed intent).
            let _ = c;
            plans.push(build_plan(eff, self.engine)?);
        }

        if do_commit {
            let mut out = Vec::with_capacity(plans.len());
            for p in &plans {
                out.push(apply_commit(p)?);
            }
            Ok(Outcome::Committed(out))
        } else {
            Ok(Outcome::Preview(plans.iter().map(plan_preview).collect()))
        }
    }

    /// Validate that `target` is a **namespace** node a `cd` may enter (hard part (a) gate):
    /// the path must route to a mounted driver whose archetype is a namespace
    /// (`BlobNamespace`/`ObjectGraphWorkflow`), not a leaf table/log or an unmounted path. This
    /// is the pure, structural capability check (RFD §5). A driver that cannot describe the node
    /// as a namespace yields a structured capability error rather than a half-applied `cd`.
    ///
    /// # Errors
    /// [`ExecError`] (kind `capability`) if the path is unmounted or not a namespace.
    fn validate_namespace(&self, target: &VfsPath) -> Result<(), ExecError> {
        namespace_check(&self.engine.mounts, target)
    }
}

/// Unwrap a (possibly nested) `PREVIEW`/`COMMIT` wrapper, returning the inner statement and
/// whether a `COMMIT` keyword was present. Mirrors the one-shot path's `unwrap_plan`.
fn unwrap_commit(stmt: &qfs_parser::Statement) -> (&qfs_parser::Statement, bool) {
    use qfs_parser::{PlanWrap, Statement};
    let mut cur = stmt;
    let mut commit = false;
    while let Statement::Plan(PlanWrap {
        commit: c, inner, ..
    }) = cur
    {
        commit = commit || *c;
        cur = inner;
    }
    (cur, commit)
}

/// The pure namespace gate for `cd` (extracted so it is unit-testable without a `Session`):
/// resolve the target's driver in `mounts` and assert its root archetype is a namespace.
///
/// # Errors
/// [`ExecError`] (kind `capability`) if the path is unmounted or describes a non-namespace node.
pub fn namespace_check(mounts: &MountRegistry, target: &VfsPath) -> Result<(), ExecError> {
    use qfs_core::{Archetype, Path};
    let abs = target.render();
    let Some((driver, _)) = mounts.resolve_path(&abs) else {
        return Err(ExecError::new(
            ErrorKind::Capability,
            "unknown_mount",
            format!("cannot cd into `{abs}`: no driver is mounted there"),
        )
        .with_path(abs));
    };
    let desc = driver
        .describe(&Path::new(abs.clone()))
        .map_err(|e| ExecError::from_qfs(&e))?;
    match desc.archetype {
        Archetype::BlobNamespace | Archetype::ObjectGraphWorkflow => Ok(()),
        other => Err(ExecError::new(
            ErrorKind::Capability,
            "not_a_namespace",
            format!(
                "cannot cd into `{abs}`: it is a {other:?} node, not a namespace you can enter"
            ),
        )
        .with_path(abs)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use qfs_core::{
        Archetype, Capabilities, CfsError, Column, ColumnType, Driver, NodeDesc, Path,
        PushdownProfile, Schema,
    };

    // A minimal namespace driver mounted at `/local`, and a leaf-table driver at `/mail`, so the
    // cd namespace gate is exercised without any real filesystem.
    struct NsDriver {
        mount: String,
        archetype: Archetype,
    }
    impl Driver for NsDriver {
        fn mount(&self) -> &str {
            &self.mount
        }
        fn describe(&self, _p: &Path) -> Result<NodeDesc, CfsError> {
            Ok(NodeDesc::new(
                self.archetype,
                Schema::new(vec![Column::new("name", ColumnType::Text, false)]),
            ))
        }
        fn capabilities(&self, _p: &Path) -> Capabilities {
            Capabilities::none()
        }
        fn procedures(&self) -> &[qfs_core::ProcSig] {
            &[]
        }
        fn pushdown(&self) -> &PushdownProfile {
            &PushdownProfile::None
        }
        fn applier(&self) -> &dyn qfs_core::PlanApplier {
            unreachable!("namespace gate never applies")
        }
    }

    fn engine() -> Engine {
        let mut e = Engine::new();
        e.mounts
            .register(Arc::new(NsDriver {
                mount: "/local".into(),
                archetype: Archetype::BlobNamespace,
            }))
            .unwrap();
        e.mounts
            .register(Arc::new(NsDriver {
                mount: "/mail".into(),
                archetype: Archetype::RelationalTable,
            }))
            .unwrap();
        e
    }

    #[test]
    fn prompt_renders_driver_and_path() {
        let e = engine();
        let reads = ReadRegistry::new();
        let s = Session::new(VfsPath::new("local", vec!["docs".into()]), &e, &reads);
        assert_eq!(s.prompt(), "local:/docs$ ");
        let root = Session::new(VfsPath::root("local"), &e, &reads);
        assert_eq!(root.prompt(), "local:/$ ");
    }

    #[test]
    fn pwd_returns_cwd_without_a_plan() {
        let e = engine();
        let reads = ReadRegistry::new();
        let mut s = Session::new(VfsPath::root("local"), &e, &reads);
        assert_eq!(
            s.eval_line("pwd", false).unwrap(),
            Outcome::Cwd(VfsPath::root("local"))
        );
    }

    #[test]
    fn cd_into_namespace_moves_cwd() {
        let e = engine();
        let reads = ReadRegistry::new();
        let mut s = Session::new(VfsPath::root("local"), &e, &reads);
        let out = s.eval_line("cd docs", false).unwrap();
        assert_eq!(
            out,
            Outcome::Moved(VfsPath::new("local", vec!["docs".into()]))
        );
        assert_eq!(s.cwd().render(), "/local/docs");
    }

    #[test]
    fn cd_into_non_namespace_is_capability_error() {
        let e = engine();
        let reads = ReadRegistry::new();
        let mut s = Session::new(VfsPath::root("local"), &e, &reads);
        // `/mail` is a RelationalTable, not a namespace: cd is rejected, cwd unchanged.
        let err = s.eval_line("cd /mail", false).unwrap_err();
        assert_eq!(err.kind.as_str(), "capability");
        assert_eq!(s.cwd().driver(), "local");
    }

    #[test]
    fn cd_into_unmounted_is_capability_error() {
        let e = engine();
        let reads = ReadRegistry::new();
        let mut s = Session::new(VfsPath::root("local"), &e, &reads);
        let err = s.eval_line("cd /nope", false).unwrap_err();
        assert_eq!(err.kind.as_str(), "capability");
    }

    #[test]
    fn empty_line_is_noop() {
        let e = engine();
        let reads = ReadRegistry::new();
        let mut s = Session::new(VfsPath::root("local"), &e, &reads);
        assert_eq!(s.eval_line("   ", false).unwrap(), Outcome::Empty);
    }
}
