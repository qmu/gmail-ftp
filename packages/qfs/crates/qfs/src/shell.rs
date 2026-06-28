//! The interactive-shell command boundary (ticket t28): the REPL driver + the concrete
//! local-FS read facet, hosted in the **`qfs` binary crate**. **All shell LOGIC lives in
//! `qfs_exec::shell`** (resolve / desugar / eval_line / Completer) — this module owns only the
//! glue a real terminal needs: the line reader, the history file, the prompt redraw, and
//! rendering an [`Outcome`] to stdout. Keeping the logic in `qfs-exec` respects the t01 C4 guard
//! (qfs-cmd stays logic-free) and the t29 CO-t29-4 topology (qfs-exec is the integration layer).
//!
//! ## Why the read adapter lives in the BINARY (not qfs-cmd)
//! `ls`/`cat`/`cd`-probe require a real [`qfs_exec::ReadDriver`] for the local mount. The local
//! driver (`qfs-driver-local`) cannot implement that trait itself (the CO-t29-4 guard lets only
//! qfs-cmd depend on qfs-exec), and qfs-exec cannot depend on the driver crate (the same guard
//! confines its deps). qfs-cmd cannot host the adapter either: `qfs-driver-local` is a
//! `qfs-runtime` consumer, so a `qfs-cmd → qfs-driver-local` edge would make qfs-cmd a non-leaf
//! runtime consumer and (correctly) trip the runtime-leaf-confinement guard. The **binary** is
//! the one place that is BOTH an allowlisted runtime consumer AND a leaf (nothing depends on it),
//! so tokio dead-ends here. The adapter [`LocalReadDriver`] — which drives the driver's pure
//! `scan_rows` through qfs-exec's async `ReadDriver` — therefore lives in the binary, which
//! injects the wired shell into `qfs-cmd` via its `ShellLauncher`. This closes part of CO-t29-1
//! for the local driver.
//!
//! ## Line editor footprint decision (recorded)
//! The ticket suggested `rustyline`/`reedline`. Neither is present in the offline cargo cache
//! (`cargo add rustyline --dry-run --offline` → "could not be found in registry index"), and the
//! disk is ~97% full, so adding a heavy editor dep is both impossible offline and against the
//! team's dependency-light precedent (ADR-0002/0003). We therefore ship a **minimal std stdin
//! line-reader** (a `read_line` loop with a best-effort in-memory + on-disk history list). The
//! [`Completer`] API is fully implemented and unit-tested; it is simply not bound to inline
//! TAB editing (which needs raw-mode terminal control a heavy editor would provide). The shell
//! core stays terminal-free and golden-testable regardless.

use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::Arc;

use qfs_core::{CfsError, DriverId, Engine, RowBatch};
use qfs_driver_local::{scan_rows, LocalError, LocalFsDriver, Sandbox};
use qfs_exec::shell::{Outcome, Session, VfsPath};
use qfs_exec::{ReadDriver, ReadRegistry};
use qfs_pushdown::ScanNode;

/// The local mount prefix the read facet scans under (mirrors the driver's internal mount).
const LOCAL_MOUNT: &str = "/local";

/// The concrete local-FS read facet: adapts `qfs_driver_local::scan_rows` (pure, synchronous) to
/// qfs-exec's async [`ReadDriver`] seam. Owns the sandbox so the scan stays confined to the
/// mount root. No vendor type crosses the seam — only the owned [`ScanNode`] in and [`RowBatch`]
/// out.
pub struct LocalReadDriver {
    sandbox: Sandbox,
}

impl LocalReadDriver {
    /// Build the read adapter confined to `root`.
    #[must_use]
    pub fn new(sandbox: Sandbox) -> Self {
        Self { sandbox }
    }
}

#[async_trait::async_trait]
impl ReadDriver for LocalReadDriver {
    async fn scan(&self, scan: &ScanNode) -> Result<RowBatch, CfsError> {
        // The ScanNode now carries the full addressed VFS path (t28 pushdown threading), so the
        // scan navigates to the exact node — `ls /local/sub` lists `sub`, not the mount root.
        // An empty path (a synthetic source) falls back to the mount root.
        let vfs = if scan.path.is_empty() {
            LOCAL_MOUNT.to_string()
        } else {
            scan.path.clone()
        };
        let project = scan.pushed.project.as_deref();
        scan_rows(&self.sandbox, &vfs, project).map_err(|e| local_to_qfs(&e))
    }
}

/// Map a local-FS scan failure into the workspace [`CfsError`] the read seam speaks. A
/// sandbox escape is a malformed path at the boundary; the rest reduce to a structured,
/// secret-free invalid-path error (the executor maps these to its own kind/exit code).
fn local_to_qfs(err: &LocalError) -> CfsError {
    match err {
        LocalError::OutsideSandbox(p) => CfsError::InvalidPath {
            path: p.clone(),
            reason: "outside_sandbox",
        },
        LocalError::NotFound(p) | LocalError::Io { path: p, .. } => CfsError::InvalidPath {
            path: p.clone(),
            reason: "read_failed",
        },
        other => CfsError::InvalidPath {
            path: String::new(),
            reason: other.code(),
        },
    }
}

/// The interactive-shell entrypoint the binary injects into `qfs_cmd::run` as its
/// [`ShellLauncher`](qfs_cmd::ShellLauncher). Builds the engine + read registry with a local-FS
/// mount over the process working directory (the operator/agent's blast-radius root), starts the
/// session at `/local`, and runs the REPL over real stdin/stdout. Returns the process exit code
/// (always 0 — a clean EOF or a best-effort I/O error both end the session without a panic).
#[must_use]
pub fn run_interactive_shell() -> i32 {
    use std::io::BufReader;
    // The local mount root is the process cwd (a sandbox boundary). A missing cwd falls back to
    // `.`, which the sandbox canonicalises; the shell never escapes it.
    let root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let (engine, reads) = local_engine_and_reads(root);
    let start = VfsPath::root("local");

    let stdin = std::io::stdin();
    let mut input = BufReader::new(stdin.lock());
    let mut out = std::io::stdout();
    if let Err(e) = run_repl(&engine, &reads, start, &mut input, &mut out) {
        // A broken stdout pipe is not a domain error; surface it without a panic.
        let _ = writeln!(std::io::stderr(), "shell io error: {e}");
    }
    0
}

/// Build a read registry with the local mount's read facet registered under the `local` driver
/// id, plus the engine with the local driver mounted at `/local` over `root`.
#[must_use]
fn local_engine_and_reads(root: PathBuf) -> (Engine, ReadRegistry) {
    let mut engine = Engine::new();
    // The introspective + (unused-here) apply facets: the shell only reads, but registering the
    // driver gives the planner its describe schema + pushdown profile + the namespace archetype
    // the `cd` gate checks.
    let _ = engine
        .mounts
        .register(Arc::new(LocalFsDriver::new(root.clone())));
    let reads = ReadRegistry::new().with(
        DriverId::new("local"),
        Arc::new(LocalReadDriver::new(Sandbox::new(root))),
    );
    (engine, reads)
}

/// The `(Engine, ReadRegistry, SafetyMode)` for the one-shot `qfs run` path (injected into qfs-cmd
/// as the run-context provider). Registers the local-FS driver — its introspective + pushdown facet
/// in the engine's mounts (so `/local/<p>` resolves + plans) and its read facet in the registry
/// (so the scan executes) — rooted at `/`, mirroring the commit driver's mapping, and resolves the
/// active selectable **safety mode** (t59) that governs the one-shot commit gate. qfs-cmd stays
/// off qfs-driver-local; the binary (the leaf) owns this adapter, like the shell + commit
/// composition. Other drivers join here as their read facets land.
#[must_use]
pub fn run_engine_and_reads() -> (Engine, ReadRegistry, qfs_core::SafetyMode) {
    // The active safety mode (t59): the persisted /sys/settings choice, else the env config, else
    // the safe default — resolved once for this run-context.
    let safety_mode = crate::sys::resolve_active_safety_mode();
    let (mut engine, reads) = local_engine_and_reads(PathBuf::from("/"));
    // Register the networked drivers' **cred-free** facets as mounts so `/github` and `/slack`
    // statements PLAN (the planner is pure — it reads only describe/capabilities/pushdown, never
    // a client; DESCRIBE is cred-free). The real credentialed clients that actually APPLY a commit
    // leg live in the apply registry (`commit.rs`), keyed by the same driver id the planner stamps.
    // The cred-free mock clients here are never called (no read facet is registered for them, and
    // planning never touches `Driver::applier`).
    let _ = engine
        .mounts
        .register(Arc::new(qfs_driver_github::GitHubDriver::new(Arc::new(
            qfs_driver_github::MockGitHubClient::default(),
        ))));
    let _ = engine
        .mounts
        .register(Arc::new(qfs_driver_slack::SlackDriver::new(Arc::new(
            qfs_driver_slack::MockSlackClient::default(),
        ))));
    // SQL: register the live SQLite-backed mount when configured, so `/sql/<conn>/<table>`
    // statements PLAN against the real introspected catalog (the same registry the commit apply
    // driver uses). Skipped when no `QFS_SQL_*` connection is configured.
    if crate::sql::has_connections() {
        let _ = engine.mounts.register(Arc::new(crate::sql::sql_driver()));
    }
    // Git: register the live git mount when configured, so `/git/<repo>/...` statements PLAN against
    // the real repository's refs and the engine's plan_write seam lowers commit INSERTs.
    if crate::git::has_connections() {
        let _ = engine.mounts.register(Arc::new(crate::git::git_driver()));
    }
    // Sys (t53): register the `/sys/*` administration mount (its PURE describe/capabilities/pushdown
    // facet, so `/sys/users |> …` and `INSERT INTO /sys/policies …` resolve + plan + gate) plus
    // the live read facet (so a `/sys` scan returns real rows). The read source is the binary's
    // injected System-DB backend; when no System DB resolves the mount still plans (describe is
    // cred-free) but a scan over an unwired `/sys` surfaces a structured read error.
    let _ = engine
        .mounts
        .register(Arc::new(qfs_driver_sys::SysDriver::new()));
    // Claude (t64): register the `/claude/...` AI-sessions mount (its PURE describe/capabilities/
    // pushdown facet, so `/claude/sessions |> WHERE status='running'` and `INSERT INTO
    // /claude/sessions/<id>/instructions …` resolve + plan + gate). The live read facet is wired
    // only when a session source is configured (QFS_CLAUDE_SESSIONS, opt-in / fail-closed); with
    // none, the mount still plans (describe is cred-free) but a `/claude` scan returns no source.
    // Decision K: a path façade over session metadata + an append-log, never an LLM call.
    let _ = engine
        .mounts
        .register(Arc::new(qfs_driver_claude::ClaudeDriver::new()));
    let mut reads = reads;
    if let Some(backend) = crate::sys::SystemDbBackend::open_default() {
        reads = reads.with(
            DriverId::new("sys"),
            Arc::new(crate::sys::SysReadDriver::new(std::sync::Arc::new(backend))),
        );
    }
    if let Some(source) = crate::claude::DirSessionSource::open_default() {
        reads = reads.with(
            DriverId::new("claude"),
            Arc::new(crate::claude::ClaudeReadDriver::new(std::sync::Arc::new(
                source,
            ))),
        );
    }
    (engine, reads, safety_mode)
}

/// Render one [`Outcome`] to `out` (human text). The shell reuses qfs-exec's renderers for the
/// row/plan DTOs so the formatting matches the one-shot path.
fn render(outcome: &Outcome, out: &mut dyn Write) -> std::io::Result<()> {
    use qfs_exec::{Renderer, TableRenderer};
    let r = TableRenderer;
    match outcome {
        Outcome::Listing(rows) => r.rows(rows, out),
        Outcome::Preview(plans) => {
            writeln!(
                out,
                "PREVIEW ({} effect plan(s), nothing applied):",
                plans.len()
            )?;
            for p in plans {
                r.plan(p, out)?;
            }
            writeln!(out, "type COMMIT to apply")
        }
        Outcome::Committed(plans) => {
            writeln!(out, "COMMITTED ({} effect plan(s)):", plans.len())?;
            for p in plans {
                r.plan(p, out)?;
            }
            Ok(())
        }
        Outcome::Moved(loc) => writeln!(out, "{loc}"),
        Outcome::Cwd(loc) => writeln!(out, "{loc}"),
        Outcome::Empty => Ok(()),
    }
}

/// Run the REPL against the given input/output streams. Generic over `BufRead`/`Write` so tests
/// feed scripted lines and capture the rendered transcript — no real terminal required.
///
/// A bare `COMMIT` on its own line is the typed confirmation that applies the **previous**
/// previewed effect line (the safety gate). Any other line is evaluated PREVIEW-by-default.
fn run_repl(
    engine: &Engine,
    reads: &ReadRegistry,
    start: VfsPath,
    input: &mut dyn BufRead,
    out: &mut dyn Write,
) -> std::io::Result<()> {
    run_repl_with_history(engine, reads, start, history_path(), input, out)
}

/// The history-injectable REPL core (tests pass `None` to stay hermetic — no real history file
/// is touched; the dispatch passes the resolved config path).
fn run_repl_with_history(
    engine: &Engine,
    reads: &ReadRegistry,
    start: VfsPath,
    history: Option<PathBuf>,
    input: &mut dyn BufRead,
    out: &mut dyn Write,
) -> std::io::Result<()> {
    let mut session = Session::new(start, engine, reads);
    // The pending effect line awaiting a typed COMMIT confirmation (PREVIEW safety gate).
    let mut pending: Option<String> = None;
    // Best-effort persistent history (no creds ever pass through the shell, so the file is
    // safe). A `None` path disables it (used by hermetic tests).
    let mut history = History::open(history);

    write!(out, "{}", session.prompt())?;
    out.flush()?;
    let mut line = String::new();
    while input.read_line(&mut line)? != 0 {
        let trimmed = line.trim_end_matches(['\n', '\r']).to_string();
        line.clear();
        if !trimmed.trim().is_empty() {
            history.push(&trimmed);
        }

        // A bare `COMMIT` confirms the pending previewed effect.
        if trimmed.trim().eq_ignore_ascii_case("COMMIT") {
            if let Some(prev) = pending.take() {
                emit(&mut session, &prev, true, out)?;
            } else {
                writeln!(out, "nothing to commit")?;
            }
        } else {
            // Evaluate PREVIEW-by-default. If it produced an effect preview, remember the line so
            // a following bare COMMIT can apply it (the safety gate).
            match session.eval_line(&trimmed, false) {
                Ok(outcome @ Outcome::Preview(_)) => {
                    render(&outcome, out)?;
                    pending = Some(trimmed.clone());
                }
                Ok(outcome) => {
                    pending = None;
                    render(&outcome, out)?;
                }
                Err(e) => {
                    pending = None;
                    use qfs_exec::Renderer;
                    let _ = qfs_exec::TableRenderer.error(&e, out);
                }
            }
        }

        write!(out, "{}", session.prompt())?;
        out.flush()?;
    }
    writeln!(out)?;
    Ok(())
}

/// Evaluate one line at the given commit level and render it (used by both the normal path and
/// the bare-COMMIT confirmation).
fn emit(
    session: &mut Session,
    line: &str,
    commit: bool,
    out: &mut dyn Write,
) -> std::io::Result<()> {
    match session.eval_line(line, commit) {
        Ok(outcome) => render(&outcome, out),
        Err(e) => {
            use qfs_exec::Renderer;
            let _ = qfs_exec::TableRenderer.error(&e, out);
            Ok(())
        }
    }
}

/// A minimal, best-effort append-only command history file under the qfs config dir. No creds
/// ever pass through the shell, so the file holds nothing sensitive. All file I/O is
/// best-effort — a missing config dir or a write failure silently disables persistence without
/// breaking the REPL. (The minimal std line-reader does not bind up-arrow recall; the file is
/// the durable record an editor upgrade would consume.)
struct History {
    path: Option<PathBuf>,
}

impl History {
    /// Open the history at `path` (best-effort). `None` disables persistence.
    fn open(path: Option<PathBuf>) -> Self {
        Self { path }
    }

    /// Append one line to the persistent history file (best-effort).
    fn push(&mut self, line: &str) {
        if let Some(p) = &self.path {
            if let Some(parent) = p.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(p)
            {
                let _ = writeln!(f, "{line}");
            }
        }
    }
}

/// The qfs config dir for the persistent history file (`$XDG_CONFIG_HOME/qfs` or `~/.config/qfs`).
/// Best-effort: a missing home just disables persistent history.
#[must_use]
fn history_path() -> Option<PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .map(|base| base.join("qfs").join("history"))
}

#[cfg(test)]
mod tests {
    //! Golden REPL tests: feed scripted lines through `run_repl` over an in-memory cursor and a
    //! real temp-dir local mount, asserting the rendered transcript + the PREVIEW/COMMIT safety
    //! gate end-to-end (ticket t28 acceptance). The local mount root is ALWAYS a tempdir — these
    //! tests never touch a system path.
    use super::*;
    use std::io::Cursor;
    use tempfile::TempDir;

    /// A temp-dir local mount with a small fixed tree, and the engine + reads wired to it.
    fn fixture() -> (TempDir, Engine, ReadRegistry) {
        let dir = TempDir::new().expect("tempdir");
        std::fs::write(dir.path().join("a.md"), b"alpha").unwrap();
        std::fs::write(dir.path().join("b.txt"), b"beta").unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub").join("c.md"), b"gamma").unwrap();
        let (engine, reads) = local_engine_and_reads(dir.path().to_path_buf());
        (dir, engine, reads)
    }

    /// Run a scripted session and return the captured transcript.
    fn run_script(engine: &Engine, reads: &ReadRegistry, script: &str) -> String {
        let mut input = Cursor::new(script.as_bytes().to_vec());
        let mut out: Vec<u8> = Vec::new();
        // `None` history keeps the test hermetic (no real ~/.config/qfs/history write).
        run_repl_with_history(
            engine,
            reads,
            VfsPath::root("local"),
            None,
            &mut input,
            &mut out,
        )
        .expect("repl runs");
        String::from_utf8(out).expect("utf8 transcript")
    }

    #[test]
    fn ls_lists_local_directory_end_to_end() {
        let (_d, engine, reads) = fixture();
        let t = run_script(&engine, &reads, "ls\n");
        // The listing renders the local entries (real FS read through the wired ReadDriver).
        assert!(t.contains("a.md"), "transcript:\n{t}");
        assert!(t.contains("b.txt"), "transcript:\n{t}");
        assert!(t.contains("sub"), "transcript:\n{t}");
    }

    #[test]
    fn cd_then_ls_navigates_into_subdir() {
        let (_d, engine, reads) = fixture();
        let t = run_script(&engine, &reads, "cd sub\nls\n");
        // The prompt reflects the new cwd, and ls shows only the subdir's entry.
        assert!(t.contains("local:/sub$"), "prompt not updated:\n{t}");
        assert!(t.contains("c.md"), "transcript:\n{t}");
        assert!(!t.contains("a.md"), "should not list parent entries:\n{t}");
    }

    #[test]
    fn cat_reads_a_file_node() {
        let (_d, engine, reads) = fixture();
        let t = run_script(&engine, &reads, "cat a.md\n");
        assert!(t.contains("a.md"), "transcript:\n{t}");
    }

    #[test]
    fn rm_previews_and_does_not_apply_until_commit() {
        let (d, engine, reads) = fixture();
        // `rm a.md` previews (nothing applied); only a typed COMMIT removes it.
        let t = run_script(&engine, &reads, "rm a.md\n");
        assert!(t.contains("PREVIEW"), "rm must preview by default:\n{t}");
        assert!(t.contains("type COMMIT to apply"), "transcript:\n{t}");
        // The file still exists — nothing was applied.
        assert!(
            d.path().join("a.md").exists(),
            "PREVIEW must not delete the file"
        );
    }

    #[test]
    fn rm_then_commit_reaches_the_committed_plan_stage() {
        // The safety gate is asserted at the PLAN level (t28 acceptance: "asserted by plan
        // assertions, not live effects"): `rm` previews, then a typed COMMIT advances to the
        // committed-plan stage. qfs-exec's `apply_commit` applies against the in-memory engine
        // (a RecordingApplier), NOT the real local FS — driving the REAL local applier from the
        // shell's COMMIT is the t30+ runtime-wiring carry-over (qfs-exec is intentionally
        // runtime-free). So the on-disk file is expected to remain until that wiring lands.
        let (_d, engine, reads) = fixture();
        let t = run_script(&engine, &reads, "rm a.md\nCOMMIT\n");
        assert!(t.contains("PREVIEW"), "transcript:\n{t}");
        assert!(
            t.contains("COMMITTED"),
            "COMMIT reaches the committed stage:\n{t}"
        );
    }

    #[test]
    fn cp_previews_a_cross_node_plan() {
        let (d, engine, reads) = fixture();
        let t = run_script(&engine, &reads, "cp a.md a-copy.md\n");
        assert!(t.contains("PREVIEW"), "cp must preview:\n{t}");
        assert!(
            !d.path().join("a-copy.md").exists(),
            "PREVIEW must not create the copy"
        );
    }

    #[test]
    fn raw_statement_runs_through_same_pipeline() {
        let (_d, engine, reads) = fixture();
        // A raw qfs read typed at the prompt produces a listing, same as the one-shot path.
        let t = run_script(&engine, &reads, "/local |> SELECT name\n");
        assert!(t.contains("a.md"), "raw statement listing:\n{t}");
    }

    #[test]
    fn bare_commit_with_no_pending_is_reported() {
        let (_d, engine, reads) = fixture();
        let t = run_script(&engine, &reads, "COMMIT\n");
        assert!(t.contains("nothing to commit"), "transcript:\n{t}");
    }
}
