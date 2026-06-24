//! The server **runtime** (RFD-0001 §6/§8): `qfs serve <config.qfs>`.
//!
//! [`Runtime::boot`] reads a `.qfs` config file, parses it to statements, lowers each to a
//! `/server` write [`Plan`], and **`COMMIT`s each through the same applier seam a live write
//! uses** — there is no privileged config loader. [`Runtime::run`] blocks on `ctrl_c` and
//! drains the audit ledger on exit.
//!
//! ## Boot is replay (the hard part, RFD §8)
//! Booting a config file is replaying `INSERT INTO /server/...` statements. The frozen
//! `CREATE …` DDL is sugar over those writes ([`crate::lower`]). The **only** way
//! [`ServerState`] changes is an [`EffectKind::ServerConfigWrite`](qfs_core::EffectKind::ServerConfigWrite)
//! applied at `COMMIT` by [`ServerConfigApplier`] — so a hot reconfigure (a later
//! `/server` write from a CLI / endpoint / trigger) takes the identical path as boot.
//!
//! ## Real COMMIT-applies-state (t28/t29 carry-over closed for /server)
//! The shell/CLI COMMIT routes through an in-memory `RecordingApplier` (records calls,
//! mutates nothing). For `/server` writes this runtime drives [`ServerConfigApplier`]
//! instead — a real [`PlanApplier`] that takes the `RwLock` write guard and applies the op
//! to [`ServerState`] via [`apply_server_write`](crate::driver::apply_server_write). So boot
//! and hot-reconfigure produce a real `ServerState`, not a recording stub.

use std::path::Path as FsPath;
use std::sync::{Arc, RwLock};

use qfs_core::{commit, AppliedEffect, ApplyError, EffectKind, EffectNode, PlanApplier};
use qfs_parser::parse_statement;

use crate::audit::{AuditEntry, AuditSink};
use crate::binding::Binding;
use crate::driver::{apply_server_write, ConfigChange};
use crate::error::ServerError;
use crate::lower::lower_statement;
use crate::state::ServerState;

/// The real `/server` COMMIT applier (RFD §6): a [`PlanApplier`] that takes the shared
/// [`ServerState`] write guard and applies each [`EffectKind::ServerConfigWrite`] to it —
/// the **only** way `ServerState` changes. Collects the per-apply [`ConfigChange`]s so the
/// runtime can write the audit ledger and reconcile bindings after the commit. Holds the
/// `RwLock` only for the brief apply (a short critical section, never across an `.await`).
pub struct ServerConfigApplier<'s> {
    state: &'s Arc<RwLock<ServerState>>,
    /// The changes applied this commit, in apply order (drained by the caller).
    changes: Vec<ConfigChange>,
}

impl<'s> ServerConfigApplier<'s> {
    /// Construct an applier over the shared state handle.
    #[must_use]
    pub fn new(state: &'s Arc<RwLock<ServerState>>) -> Self {
        Self {
            state,
            changes: Vec::new(),
        }
    }

    /// The changes this applier recorded (consumes the applier).
    #[must_use]
    pub fn into_changes(self) -> Vec<ConfigChange> {
        self.changes
    }
}

impl PlanApplier for ServerConfigApplier<'_> {
    fn apply(&mut self, node: &EffectNode) -> Result<AppliedEffect, ApplyError> {
        match &node.kind {
            EffectKind::ServerConfigWrite { node: snode, op } => {
                // The single ACID mutation: take the write guard for the brief apply.
                let mut guard = self
                    .state
                    .write()
                    .map_err(|_| ApplyError::new(node.id, "server state lock poisoned"))?;
                let change = apply_server_write(&mut guard, *snode, *op, &node.args)
                    .map_err(|reason| ApplyError::new(node.id, reason))?;
                drop(guard);
                self.changes.push(change);
                Ok(AppliedEffect::new(node.id, 1))
            }
            // The /server applier only services /server writes; a foreign node is a wiring
            // bug (the runtime routes only ServerConfigWrite plans here).
            other => Err(ApplyError::new(
                node.id,
                format!(
                    "server applier received non-/server effect {}",
                    other.label()
                ),
            )),
        }
    }
}

/// The long-lived server runtime (RFD §8). Owns the shared [`ServerState`] (source of
/// truth), the registered [`Binding`]s (the cause seam E7 fills), and the [`AuditSink`].
pub struct Runtime {
    state: Arc<RwLock<ServerState>>,
    bindings: Vec<Box<dyn Binding>>,
    audit: Arc<AuditSink>,
}

impl Runtime {
    /// A fresh runtime with an empty [`ServerState`] and no bindings. Boot mutates the
    /// state through the COMMIT path; `with_binding` registers cause seams.
    #[must_use]
    pub fn new() -> Self {
        Self {
            state: Arc::new(RwLock::new(ServerState::new())),
            bindings: Vec::new(),
            audit: Arc::new(AuditSink::new()),
        }
    }

    /// Register a [`Binding`] (builder form). Bindings reconcile to the registry after every
    /// committed `/server` mutation.
    #[must_use]
    pub fn with_binding(mut self, binding: Box<dyn Binding>) -> Self {
        self.bindings.push(binding);
        self
    }

    /// The shared state handle (so a binding or test can snapshot it).
    #[must_use]
    pub fn state(&self) -> &Arc<RwLock<ServerState>> {
        &self.state
    }

    /// The audit ledger handle.
    #[must_use]
    pub fn audit(&self) -> &Arc<AuditSink> {
        &self.audit
    }

    /// A read snapshot of the current [`ServerState`] (clones it, so a binding never holds
    /// the read guard across an `.await`).
    #[must_use]
    pub fn snapshot(&self) -> ServerState {
        self.state.read().map(|g| g.clone()).unwrap_or_default()
    }

    /// **Boot** from a `.qfs` config file (RFD §8). Reads the file, splits it into
    /// statements, parses + lowers each, and `COMMIT`s it through [`ServerConfigApplier`]
    /// (the same path a live write takes). After every committed mutation, every registered
    /// binding reconciles to the new state and an audit entry is recorded. Fails fast with a
    /// line-located error on any rejected statement.
    ///
    /// # Errors
    /// [`ServerError`] (read / parse / lower / unsupported-verb / commit) — line-located.
    pub fn boot(&mut self, cfg: &FsPath) -> Result<(), ServerError> {
        let text = std::fs::read_to_string(cfg).map_err(|e| ServerError::Read {
            path: cfg.display().to_string(),
            source: e,
        })?;
        for (line, src) in statements(&text) {
            self.apply_source("boot", line, &src)?;
        }
        // A final reconcile so a binding converges even if the config was empty.
        self.reconcile_all()?;
        let summary = self.snapshot().summary();
        tracing::info!(target: "qfs::server", config = %cfg.display(), %summary, "boot complete");
        Ok(())
    }

    /// Parse → lower → COMMIT one server-config statement (the boot + hot-reconfigure unit).
    /// Records the audit ledger entry and reconciles bindings for each applied change.
    ///
    /// # Errors
    /// [`ServerError`] — line-located — on parse / lower / unsupported-verb / commit failure.
    pub fn apply_source(&mut self, who: &str, line: usize, src: &str) -> Result<(), ServerError> {
        let stmt = parse_statement(src).map_err(|e| ServerError::Parse {
            line,
            code: e.code.as_str().to_string(),
            message: e.message.clone(),
        })?;

        let plan =
            match lower_statement(&stmt).map_err(|detail| ServerError::Lower { line, detail })? {
                Some(plan) => plan,
                None => {
                    return Err(ServerError::NotServerConfig {
                        line,
                        detail: "statement does not write /server".to_string(),
                    })
                }
            };

        // COMMIT through the REAL applier (mutates ServerState), not a recording stub.
        let mut applier = ServerConfigApplier::new(&self.state);
        let report = commit(&plan, &mut applier, |_| {});
        if let Some(err) = report.failed {
            return Err(ServerError::Commit {
                line,
                reason: err.reason,
            });
        }
        let changes = applier.into_changes();

        // Audit + reconcile after every committed mutation (RFD §6/§8).
        for change in &changes {
            self.audit.record(AuditEntry::from_change(who, change));
            self.reconcile_all()?;
        }
        Ok(())
    }

    /// Reconcile every registered binding to a read snapshot of the current state. The
    /// snapshot is cloned so no binding holds the read guard across an `.await` (RFD §6
    /// concurrency rule).
    fn reconcile_all(&mut self) -> Result<(), ServerError> {
        let snapshot = self.snapshot();
        for binding in &mut self.bindings {
            binding.reconcile(&snapshot)?;
        }
        Ok(())
    }

    /// **Run**: block until a shutdown signal (`SIGINT`/ctrl_c **or** `SIGTERM`), then drain the
    /// audit ledger and return. The cause bindings (HTTP/cron/webhook) attach in E7; at t30 the run
    /// loop is the supervised wait + graceful drain. Consumes the runtime.
    ///
    /// ## SIGTERM is a first-class graceful-shutdown trigger (t36)
    /// A `qfs serve` daemon is stopped by systemd with `KillSignal=SIGTERM` (see
    /// `deploy/qfs.service`); the operator's interactive ctrl_c is `SIGINT`. EITHER triggers the
    /// SAME graceful path — the supervised wait resolves, in-flight plans stop being accepted, and
    /// the audit ledger drains — so a `systemctl stop` is a clean drain, not an uncaught SIGTERM
    /// (exit 143 with no drain). The SIGTERM listener is unix-only (`tokio::signal::unix`);
    /// non-unix targets fall back to `ctrl_c` alone (`SIGTERM` does not exist there).
    ///
    /// # Errors
    /// [`ServerError`] only if the shutdown wiring fails (rare); a normal signal returns `Ok(())`
    /// after draining.
    pub async fn run(self) -> Result<(), ServerError> {
        tracing::info!(target: "qfs::server", summary = %self.snapshot().summary(), "server running; press ctrl_c (SIGINT) or send SIGTERM to stop");
        // Block on the shutdown signal. A failure to install a handler degrades to an immediate
        // shutdown (drain + return) rather than a panic.
        shutdown_signal().await;
        let drained = self.audit.drain();
        tracing::info!(target: "qfs::server", entries = drained, "shutdown: audit ledger drained");
        Ok(())
    }
}

/// Wait for a graceful-shutdown signal: `SIGINT` (ctrl_c) **or**, on unix, `SIGTERM`. Resolves on
/// whichever fires first. A handler-install failure logs a secret-free warning and resolves
/// immediately (degrade to shutdown, never panic) so the daemon still drains.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        // SIGTERM listener (systemd `stop` / `KillSignal=SIGTERM`). If it cannot be installed, a
        // never-ready future lets the SIGINT arm win the `select!` (degrade to ctrl_c only).
        let mut term = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(target: "qfs::server", error = %e, "SIGTERM handler failed; ctrl_c only");
                // Fall back to ctrl_c alone.
                if let Err(e) = tokio::signal::ctrl_c().await {
                    tracing::warn!(target: "qfs::server", error = %e, "ctrl_c handler failed; shutting down");
                }
                return;
            }
        };
        tokio::select! {
            r = tokio::signal::ctrl_c() => {
                if let Err(e) = r {
                    tracing::warn!(target: "qfs::server", error = %e, "ctrl_c handler failed; shutting down");
                }
                tracing::info!(target: "qfs::server", "received SIGINT; shutting down");
            }
            _ = term.recv() => {
                tracing::info!(target: "qfs::server", "received SIGTERM; shutting down");
            }
        }
    }
    #[cfg(not(unix))]
    {
        // Non-unix (incl. wasm): SIGTERM does not exist; ctrl_c is the only graceful trigger.
        if let Err(e) = tokio::signal::ctrl_c().await {
            tracing::warn!(target: "qfs::server", error = %e, "ctrl_c handler failed; shutting down");
        }
    }
}

impl Default for Runtime {
    fn default() -> Self {
        Self::new()
    }
}

/// Test-only accessor for the statement splitter.
#[cfg(test)]
pub(crate) fn statements_for_test(text: &str) -> Vec<(usize, String)> {
    statements(text)
}

/// Split config-file text into `(line, statement_source)` pairs. Comments are stripped
/// **first** (a whole-line `#` comment, or a trailing `--`/`#` comment), then statements are
/// separated by `;` — so a `;` *inside a comment* never splits a statement. The `line` is the
/// 1-based line the statement's first non-blank content appears on, for line-located errors.
/// This is a deliberately small splitter (the parser owns real grammar); it just chunks the
/// file so each statement parses alone.
fn statements(text: &str) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut start_line: Option<usize> = None;

    for (idx, raw_line) in text.lines().enumerate() {
        let line_no = idx + 1;
        let code = strip_line_comment(raw_line);
        // Split this (comment-free) line on `;`, flushing a statement at each separator.
        let mut rest = code;
        while let Some(pos) = rest.find(';') {
            let (head, tail) = rest.split_at(pos);
            if !head.trim().is_empty() && start_line.is_none() {
                start_line = Some(line_no);
            }
            current.push_str(head);
            if !current.trim().is_empty() {
                out.push((start_line.unwrap_or(line_no), current.trim().to_string()));
            }
            current.clear();
            start_line = None;
            rest = &tail[1..]; // skip the ';'
        }
        if !rest.trim().is_empty() && start_line.is_none() {
            start_line = Some(line_no);
        }
        if !rest.is_empty() {
            current.push_str(rest);
            current.push('\n');
        }
    }
    if !current.trim().is_empty() {
        out.push((start_line.unwrap_or(1), current.trim().to_string()));
    }
    out
}

/// Strip a trailing `--` comment and a whole-line `#` comment from one line. A `#` is only a
/// comment at the start of the (trimmed) line; `--` truncates the rest of the line.
fn strip_line_comment(line: &str) -> &str {
    if line.trim_start().starts_with('#') {
        return "";
    }
    match line.find("--") {
        Some(i) => &line[..i],
        None => line,
    }
}
