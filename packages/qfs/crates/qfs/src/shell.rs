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
/// Register the cred-free **planning** facets for the three Google drivers (`/mail`, `/drive`,
/// `/ga`) into `engine`'s mounts, so statements over those paths RESOLVE + PLAN end to end. The
/// planner is pure — it reads only the introspective describe/capabilities/pushdown half, never a
/// client and never a token — so a mock client suffices here (exactly as `qfs describe` does). The
/// real OAuth-authenticated clients that APPLY a commit leg live in the apply registry
/// (`commit.rs`), keyed by the SAME runtime driver ids (`mail`/`drive`/`ga`) the planner stamps; the
/// mock clients registered here are never called (no read facet is wired for them, and planning
/// never touches an applier). Factored out so the planning wiring is unit-tested hermetically.
fn register_google_planning_mounts(engine: &mut Engine) {
    let _ = engine
        .mounts
        .register(Arc::new(qfs_driver_gmail::GmailDriver::new(Arc::new(
            qfs_driver_gmail::MockGmailClient::new(),
        ))));
    let _ = engine
        .mounts
        .register(Arc::new(qfs_driver_gdrive::GDriveDriver::new(Arc::new(
            qfs_driver_gdrive::MockDriveClient::default(),
        ))));
    let ga = Arc::new(qfs_driver_ga::GaDriver::new(Arc::new(
        qfs_driver_ga::MockGaClient::default(),
    )));
    let _ = engine.mounts.register(ga.clone());
    // Keep the DEPRECATED `/ga` path routing here for one release (owner item #8): the same driver
    // answers both `/google-analytics` (canonical) and `/ga`, so existing `/ga/...` statements still
    // plan + run. Docs/introspection show only the canonical mount (this alias is routing-only).
    let _ = engine
        .mounts
        .register_alias(qfs_driver_ga::DEPRECATED_MOUNT, ga);
}

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
    // Cloudflare (/cf) + the generic HTTP/REST (/rest) drivers: register their cred-free PLANNING
    // mounts so `/cf/...` and `/rest/...` statements PLAN + describe (the drivers existed but were
    // not reachable as paths). Both are built with an empty registry / placeholder config here —
    // describing the mount root and resolving the path — so a connected user can address them; the
    // live credentialed read/commit and the per-resource config (which D1/KV/queues; which REST
    // resource maps) are the documented follow-up, sourced from a richer connection declaration.
    let _ = engine
        .mounts
        .register(Arc::new(qfs_driver_cf::CfDriver::new(
            crate::describe::cred_free_cf_registry(),
        )));
    if let Ok(json) = qfs_core::CodecRegistry::with_builtins().resolve("json") {
        let _ = engine
            .mounts
            .register(Arc::new(qfs_driver_http::RestDriver::new(
                qfs_driver_http::RestApiConfig::new("http://localhost", Vec::new()),
                json,
                Arc::new(qfs_driver_http::MockHttpClient::new()),
                Arc::new(qfs_secrets::InMemoryStore::new()),
            )));
    }
    // Google (gmail / gdrive / ga): register the cred-free mock-client facets as mounts so `/mail`,
    // `/drive`, and `/ga` statements PLAN (see `register_google_planning_mounts`).
    register_google_planning_mounts(&mut engine);
    // S3 / R2 (objstore): register the cred-free planning facets as mounts so `/s3/<bucket>/<key>`
    // and `/r2/...` statements PLAN. Each carries a representative `bucket` (the describe convention)
    // plus the operator-configured live bucket name when present, so the parse-time per-node
    // capability gate (which keys off a *registered* bucket) resolves. The MockObjectBackend behind
    // each bucket is never applied — planning reads only the pure introspective half; the real SigV4
    // backend that APPLIES lives in the apply registry (`commit.rs`), keyed by the same driver id.
    let _ = engine
        .mounts
        .register(Arc::new(qfs_driver_objstore::S3Driver::new(
            crate::objstore::planning_registry(qfs_driver_objstore::Scheme::S3),
        )));
    let _ = engine
        .mounts
        .register(Arc::new(qfs_driver_objstore::R2Driver::new(
            crate::objstore::planning_registry(qfs_driver_objstore::Scheme::R2),
        )));
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
    // GitHub / Slack networked READ facets: register each driver's read adapter behind the SAME
    // credentialed client the commit applier binds (the shared `crate::clients` builder), so a
    // `FROM /github/.../pulls` or `FROM /slack/<ws>/users` (and therefore a `FROM … |> CALL` whose
    // pipeline starts with a read) executes through the read executor. FAIL CLOSED: the builder
    // returns `None` when the operator is unconfigured or the t54 cloud bind gate refuses, leaving
    // the read facet UNREGISTERED so the `FROM` then fails honestly ("no source") rather than
    // reading without authorization. A registered facet whose credential cannot be resolved at
    // request time surfaces a clear auth error (see `crate::read_facets`), never empty rows.
    // GitHub / Slack: register the real networked read facet behind the credentialed client (t6); on
    // a fresh, unconfigured operator the client builder returns None, so fall back to the honest
    // connect-account facet (t5) — a `FROM /github/...` then gets an ACTIONABLE error, never the raw
    // unknown_source. The authenticated path returns real rows (issues/pulls/messages).
    reads = match crate::clients::live_github_client() {
        Some(client) => reads.with(
            DriverId::new("github"),
            Arc::new(crate::read_facets::GitHubReadDriver::new(client)),
        ),
        None => reads.with(
            DriverId::new("github"),
            Arc::new(crate::read_facets::ConnectAccountReadDriver::new(
                "connect a GitHub account to read it — run `qfs identity signup <email>`, then `qfs connection add github` (GitHub reads need an authenticated token)",
            )),
        ),
    };
    reads = match crate::clients::live_slack_client() {
        Some(client) => reads.with(
            DriverId::new("slack"),
            Arc::new(crate::read_facets::SlackReadDriver::new(client)),
        ),
        None => reads.with(
            DriverId::new("slack"),
            Arc::new(crate::read_facets::ConnectAccountReadDriver::new(
                "connect a Slack workspace to read it — run `qfs identity signup <email>`, then `qfs connection add slack`",
            )),
        ),
    };
    // SQL (hermetic, no network): register the live SQLite-backed read facet when a connection is
    // configured, so `FROM /sql/<conn>/<table> |> WHERE … |> SELECT …` executes — the native SELECT
    // pushes the WHERE/ORDER/LIMIT into the database and the residual is re-filtered locally. Skipped
    // (leaving the source unresolvable) when no `QFS_SQL_*` connection resolves, so it fails closed.
    if crate::sql::has_connections() {
        reads = reads.with(
            DriverId::new("sql"),
            Arc::new(crate::read_facets::SqlReadDriver::new(Arc::new(
                crate::sql::sql_driver(),
            ))),
        );
    }
    // Git (hermetic, no network): register the in-house object-reader read facet when a repo is
    // configured, so `FROM /git/<repo>@<ref>/commits` (and refs/tags/reflog/changes/blame + tree
    // listings) executes against the local `.git`. Skipped (source unresolvable) when no `QFS_GIT_*`
    // repo resolves, so it fails closed.
    if crate::git::has_connections() {
        reads = reads.with(
            DriverId::new("git"),
            Arc::new(crate::read_facets::GitReadDriver::new(Arc::new(
                crate::git::git_driver(),
            ))),
        );
    }
    // Cloud sources whose reads fundamentally need a live OAuth/credentialed account (mail, drive,
    // analytics, object stores) have no offline read. Register an honest "connect your account" read
    // facet for each so a fresh-user `FROM /mail/...` gets an ACTIONABLE error instead of the
    // internal-sounding `unknown_source` (t5). The real networked read (t6/t7) registers over this
    // for a credentialed operator. Each reason is a stable, secret-free `&'static str`.
    for (source, reason) in [
        ("mail", "connect a Google account to read mail — run `qfs identity signup <email>`, then `qfs connection add gmail` (gmail reads are not available without an authenticated account)"),
        ("drive", "connect a Google account to read Drive — run `qfs identity signup <email>`, then `qfs connection add gdrive`"),
        ("ga", "connect a Google Analytics account to read it — run `qfs identity signup <email>`, then `qfs connection add ga`"),
        ("s3", "connect AWS credentials to read S3 — run `qfs connection add s3` (S3 reads need a credentialed bucket)"),
        ("r2", "connect Cloudflare R2 credentials to read it — run `qfs connection add r2`"),
    ] {
        reads = reads.with(
            DriverId::new(source),
            Arc::new(crate::read_facets::ConnectAccountReadDriver::new(reason)),
        );
    }
    // Gmail + Drive + Analytics real reads (t7): when a Google account is configured AND the t54
    // cloud bind gate passes, register the live read facet OVER the connect-account fallback, so
    // `FROM /mail/<label>` returns messages, `FROM /drive/...` lists a folder's children, and
    // `FROM /ga/<property> |> select ...` runs a GA4 report for a connected operator. Fail closed:
    // an unconfigured/ungated operator keeps the t5 connect-account nudge.
    if let Some(stack) = crate::google::live_google_stack() {
        let gmail_conn =
            crate::connection::active_connection("gmail").unwrap_or_else(|| "default".to_string());
        if crate::commit::cloud_bind_allowed("gmail", &gmail_conn) {
            let client: Arc<dyn qfs_driver_gmail::GmailClient> = Arc::new(
                qfs_driver_gmail::GoogleApiGmailClient::new(stack.api.clone()),
            );
            reads = reads.with(
                DriverId::new("mail"),
                Arc::new(crate::read_facets::GmailReadDriver::new(client)),
            );
        }
        let drive_conn =
            crate::connection::active_connection("gdrive").unwrap_or_else(|| "default".to_string());
        if crate::commit::cloud_bind_allowed("gdrive", &drive_conn) {
            let client: Arc<dyn qfs_driver_gdrive::GDriveClient> = Arc::new(
                qfs_driver_gdrive::GoogleApiDriveClient::new(stack.api.clone()),
            );
            reads = reads.with(
                DriverId::new("drive"),
                Arc::new(crate::read_facets::DriveReadDriver::new(client)),
            );
        }
        let ga_conn =
            crate::connection::active_connection("ga").unwrap_or_else(|| "default".to_string());
        if crate::commit::cloud_bind_allowed("ga", &ga_conn) {
            let client: Arc<dyn qfs_driver_ga::GaClient> =
                Arc::new(qfs_driver_ga::GoogleApiGaClient::new(stack.api.clone()));
            let driver = Arc::new(qfs_driver_ga::GaDriver::new(client));
            reads = reads.with(
                DriverId::new("ga"),
                Arc::new(crate::read_facets::GaReadDriver::new(driver)),
            );
        }
    }
    // S3 / R2 live READ facets (t-203070): register the SigV4 object-storage reader when the routing
    // config + secret access key resolve (fail closed otherwise, exactly like the apply path). Built
    // from the SAME live registry the commit path uses, so `/s3/<bucket>` lists and `/s3/<bucket>/<key>`
    // downloads against a real (or MinIO) S3 endpoint.
    for scheme in [
        qfs_driver_objstore::Scheme::S3,
        qfs_driver_objstore::Scheme::R2,
    ] {
        if let Some(driver) = crate::commit::live_obj_read_driver(scheme) {
            reads = reads.with(
                DriverId::new(scheme.mount().trim_start_matches('/')),
                Arc::new(crate::read_facets::ObjReadDriver::new(Arc::new(driver))),
            );
        }
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
    fn mail_statement_plans_through_the_registered_google_mount() {
        // The cred-free Google planning mounts let a `/mail` write RESOLVE + PLAN end to end with no
        // client, no token, and no network — the same describe-only path `qfs describe` uses. A real
        // OAuth client only matters at COMMIT (commit.rs). This drives the SAME wiring the one-shot
        // path uses (`register_google_planning_mounts`) over a hermetic temp-dir local engine.
        let (_d, mut engine, reads) = fixture();
        register_google_planning_mounts(&mut engine);
        let t = run_script(
            &engine,
            &reads,
            "INSERT INTO /mail/drafts VALUES ('alice@example.com', 'Hi', 'Body text')\n",
        );
        // It previews a plan (the safety gate), not an unresolved-mount / unknown-driver error.
        assert!(t.contains("PREVIEW"), "/mail must plan/preview:\n{t}");
        assert!(
            t.contains("type COMMIT to apply"),
            "/mail plan reaches the COMMIT gate:\n{t}"
        );
    }

    #[test]
    fn s3_upsert_plans_through_the_registered_objstore_mount() {
        // The cred-free objstore planning mount lets a `/s3/<bucket>/<key>` UPSERT RESOLVE + PLAN end
        // to end with no SigV4 backend, no credential, and no network — the same describe-only path
        // `qfs describe /s3/bucket/key` uses (the per-node capability gate keys off the *registered*
        // representative `bucket`). The real SigV4 backend only matters at COMMIT (commit.rs). This
        // drives the SAME `planning_registry` wiring the one-shot path registers.
        let (_d, mut engine, reads) = fixture();
        let _ = engine
            .mounts
            .register(Arc::new(qfs_driver_objstore::S3Driver::new(
                crate::objstore::planning_registry(qfs_driver_objstore::Scheme::S3),
            )));
        let t = run_script(
            &engine,
            &reads,
            "UPSERT INTO /s3/bucket/key VALUES ('blob')\n",
        );
        // It previews a plan (the safety gate), not an unresolved-mount / unknown-driver error.
        assert!(t.contains("PREVIEW"), "/s3 UPSERT must plan/preview:\n{t}");
        assert!(
            t.contains("type COMMIT to apply"),
            "/s3 plan reaches the COMMIT gate:\n{t}"
        );
    }

    #[test]
    fn bare_commit_with_no_pending_is_reported() {
        let (_d, engine, reads) = fixture();
        let t = run_script(&engine, &reads, "COMMIT\n");
        assert!(t.contains("nothing to commit"), "transcript:\n{t}");
    }
}
