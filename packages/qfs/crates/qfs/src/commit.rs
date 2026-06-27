//! The real `qfs run --commit` apply path: drives the `qfs-runtime` [`Interpreter`] over a live
//! driver registry to apply an effect `Plan` to the World. Injected into `qfs-cmd` as the
//! [`qfs_exec::WorldApply`] hook.
//!
//! `qfs-cmd` and `qfs-exec` are deliberately confined off `qfs-runtime` (the interpreter is the
//! sole impure stage). The terminal binary is the allowlisted runtime leaf that owns the
//! interpreter + the live drivers, so the real commit composition lives here — exactly like the
//! shell / serve / connection launchers.
//!
//! Today the registry carries the **local filesystem** driver (no credentials needed), which
//! proves the commit path is real end to end: `qfs run "UPSERT INTO /local/… " --commit` actually
//! writes the file. Credentialed / networked drivers register here behind their live clients as
//! they land (the execution+auth ticket).

use std::sync::Arc;

use qfs_exec::{ErrorKind, ExecError};
use qfs_runtime::{CapabilitySet, DriverRegistry, Interpreter, LegStatus};
use qfs_secrets::{ConnectionId, CredentialKey, EnvStore, Secrets};
use qfs_types::DriverId;

/// Apply `plan` to the World via the runtime interpreter. Returns `Ok(())` once every leg applied,
/// or an [`ExecError`] (kind `commit_failed`) if a leg failed or was skipped. Builds a fresh
/// current-thread tokio runtime to drive the async interpreter (tokio dead-ends here, in the
/// terminal binary leaf). Never panics.
pub fn apply_plan(plan: &qfs_core::Plan) -> Result<(), ExecError> {
    let interp = Interpreter::with_defaults(live_registry());
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| {
            ExecError::new(
                ErrorKind::Internal,
                "runtime_init",
                format!("failed to start the commit runtime: {e}"),
            )
        })?;
    // Capability gating already ran at parse time; allow-all is the apply-time re-check for the
    // CLI one-shot (a server run gates with its POLICY instead).
    let outcome = rt
        .block_on(interp.commit(plan.clone(), &CapabilitySet::allow_all()))
        .map_err(|e| ExecError::new(ErrorKind::CommitFailed, "commit_failed", format!("{e:?}")))?;
    // t76: emit one hash-chained audit event per committed effect (and per ATTEMPTED irreversible
    // effect) — BEFORE the completeness check, so a partial commit still audits the legs that
    // actually applied and any irreversible leg that was tried. Best-effort + metadata-only: a
    // missing/locked System DB never fails or masks the commit, and an event carries verb + path +
    // connection only, never row data or a secret (the boundary `describe` enforces).
    emit_audit(plan, &outcome);
    if !outcome.is_complete() {
        // Surface the per-leg failure reasons (structured, secret-free) so a commit failure is
        // diagnosable rather than an opaque count.
        let reasons: Vec<String> = outcome
            .ledger
            .iter()
            .filter_map(|e| match &e.status {
                LegStatus::Failed { error, .. } => Some(format!("{error:?}")),
                LegStatus::Skipped { cause } => {
                    Some(format!("skipped (dependency {cause:?} failed)"))
                }
                LegStatus::Applied { .. } => None,
                _ => Some("unknown leg status".to_string()),
            })
            .collect();
        return Err(ExecError::new(
            ErrorKind::CommitFailed,
            "commit_failed",
            reasons.join("; "),
        ));
    }
    Ok(())
}

/// The acting principal recorded on every audit event a one-shot `qfs run --commit` emits. A
/// label, never a credential (t76 / §4.6). The CLI invocation is the actor today; a request-derived
/// identity replaces this once multi-user auth lands.
const ACTOR_CLI: &str = "cli";

/// t76: emit one hash-chained audit event per committed effect (and per attempted irreversible
/// effect) from the commit `outcome`. Each event is METADATA ONLY — `actor`, the routed
/// `connection` (t44), the write `verb`, the VFS `path`, whether it `committed`, and the timestamp —
/// never a secret, never row data (the boundary `describe` enforces, §3.2/§4.6).
///
/// Best-effort by design: opening the per-host System DB or appending an event must NEVER fail the
/// commit or mask its result (decision: the audit never breaks the operation, §6). A host with no
/// config home runs unaudited; a transient append error is logged (secret-free) and skipped.
fn emit_audit(plan: &qfs_core::Plan, outcome: &qfs_runtime::Outcome) {
    // Only the binary opens a real DB path (decision F). No config home / a transient open error =>
    // the commit proceeds unaudited rather than failing.
    let sys = match crate::store::open_system_db() {
        Ok(Some(sys)) => sys,
        Ok(None) => return,
        Err(e) => {
            tracing::debug!(target: "qfs::audit", "audit emission skipped (system DB unavailable): {e}");
            return;
        }
    };

    let ts = now_rfc3339();
    for entry in &outcome.ledger {
        // A committed effect is one that APPLIED. An attempted irreversible effect is an
        // irreversible leg that was tried but did not apply (Failed) — recorded as committed=false
        // so the stream is the one funnel. Skipped legs were never attempted, so they emit nothing.
        let committed = matches!(entry.status, LegStatus::Applied { .. });
        let attempted_irreversible =
            entry.irreversible && matches!(entry.status, LegStatus::Failed { .. });
        if !committed && !attempted_irreversible {
            continue;
        }

        // The path lives on the plan node (the ledger entry carries driver + kind, not the path).
        let path = plan
            .node(entry.id)
            .map_or_else(String::new, |n| n.target.path.as_str().to_string());
        // The connection the effect routed through: the active `<driver>/<name>` selection (t44),
        // defaulting to `default`. The NAME only — never the secret material behind it.
        let connection = crate::connection::active_connection(entry.driver.as_str())
            .unwrap_or_else(|| "default".to_string());

        let event = qfs_store::audit::AuditEvent {
            actor: ACTOR_CLI.to_string(),
            connection,
            verb: entry.kind.label().to_string(),
            path,
            committed,
            ts: ts.clone(),
        };
        if let Err(e) = crate::audit::append_event(&sys, event) {
            tracing::debug!(target: "qfs::audit", "audit append failed (continuing): {e}");
        }
    }
}

/// The current UTC time as an RFC3339 string for an audit event's `ts`. A clock read can fail to
/// format only on an impossible date; we fall back to the Unix epoch rather than panic (the audit
/// never breaks the operation).
fn now_rfc3339() -> String {
    use time::format_description::well_known::Rfc3339;
    time::OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string())
}

/// The live apply-driver registry: the real clients each effect leg applies through, keyed by the
/// leg's [`DriverId`] (the same id the planner stamped on the `Target`).
///
/// - **local** filesystem (cred-free): rooted at `/` so a VFS path `/local/<p>` maps to host
///   `/<p>` within the driver's sandbox; real `UPSERT`/`REMOVE` legs apply through its
///   `LocalApplier`.
/// - **github** + **slack** (credentialed HTTP): the real [`reqwest`](crate::transport)
///   transport + the encrypted credential store. Always registered so a `/github` or `/slack`
///   commit leg routes; the PAT / bot token is resolved **lazily at request time**, so a missing
///   credential surfaces as a clear per-leg auth error (never a panic, never a silent no-op).
///
/// Credentialed Google / SQL / object-store drivers register here as their production clients
/// (OAuth, connection pools, SigV4) land — each its own execution+auth slice.
fn live_registry() -> DriverRegistry {
    let local = qfs_driver_local::LocalFsDriver::new("/");
    let mut reg = DriverRegistry::new().with(
        DriverId::new("local"),
        Arc::new(qfs_driver_local::local_apply_driver(&local)),
    );

    // GitHub: the real REST client over the production reqwest transport + the resolved credential.
    if let Some((gh_store, gh_cred)) = networked_credential("github") {
        let gh_client = Arc::new(qfs_driver_github::RestGitHubClient::new(
            crate::transport::github_transport(),
            gh_store,
            gh_cred,
        ));
        let gh_driver = qfs_driver_github::GitHubDriver::new(gh_client);
        reg = reg.with(
            DriverId::new("github"),
            Arc::new(qfs_driver_github::github_apply_driver(&gh_driver)),
        );
    }

    // SQL: the real SQLite-backed driver, when at least one `QFS_SQL_<conn>` is configured. Real
    // ACID `INSERT`/`UPDATE`/`UPSERT`/`REMOVE` legs apply through the live connection; an
    // unconfigured `/sql` commit fails closed (no driver) rather than faking success.
    if crate::sql::has_connections() {
        let sql_driver = crate::sql::sql_driver();
        reg = reg.with(
            DriverId::new("sql"),
            Arc::new(qfs_driver_sql::sql_apply_driver(&sql_driver)),
        );
    }

    // Git: the real on-disk repositories driven by the `git` CLI, when at least one `QFS_GIT_<repo>`
    // is configured. The engine's plan_write seam lowers `INSERT INTO /git/<repo>/commits` to the
    // encoded blob→tree→commit→ref→reflog plan; this applies it (real objects + branch CAS). An
    // unconfigured `/git` commit fails closed.
    if crate::git::has_connections() {
        let git_driver = crate::git::git_driver();
        reg = reg.with(
            DriverId::new("git"),
            Arc::new(qfs_driver_git::git_apply_driver(&git_driver)),
        );
    }

    // Slack: same shape (the shared reqwest transport, Slack's body-error rule on).
    if let Some((sl_store, sl_cred)) = networked_credential("slack") {
        let sl_client = Arc::new(qfs_driver_slack::RestSlackClient::new(
            crate::transport::slack_transport(),
            sl_store,
            sl_cred,
            qfs_driver_slack::BodyErrorRule::On,
        ));
        let sl_driver = qfs_driver_slack::SlackDriver::new(sl_client);
        reg = reg.with(
            DriverId::new("slack"),
            Arc::new(qfs_driver_slack::slack_apply_driver(&sl_driver)),
        );
    }

    reg
}

/// Resolve the `(store, credential key)` a networked driver applies with. Reads the **same**
/// credential `qfs connection add <driver> <name>` wrote: the envelope-encrypted SQLite store
/// ([`crate::secret_store::SqliteSecrets`]) when `QFS_PASSPHRASE` + the Project DB exist, else the
/// process-env store (`QFS_SECRET_*`, the agent / CI path). The connection is the one `qfs connection use
/// <driver> <name>` selected (the Project DB's `active_account` table), defaulting to `default`. The
/// secret is **not** read here — the client reads it lazily at request-build time, so a
/// missing/locked credential becomes a clear per-leg auth error at commit, never a panic at registry
/// build. Returns `None` only if the connection id cannot be constructed (impossible for the literal
/// `default` fallback) — in which case the driver is simply left unregistered rather than panicking.
fn networked_credential(driver: &str) -> Option<(Arc<dyn Secrets>, CredentialKey)> {
    let store: Arc<dyn Secrets> = match crate::connection::open_store_for_commit() {
        Some(sqlite) => Arc::new(sqlite),
        None => Arc::new(EnvStore::from_process_env()),
    };
    let connection =
        crate::connection::active_connection(driver).unwrap_or_else(|| "default".to_string());
    // `default` is always a valid connection name; an invalid persisted selection falls back to it.
    let acct = ConnectionId::new(&connection)
        .or_else(|_| ConnectionId::new("default"))
        .ok()?;
    let cred = CredentialKey::new(qfs_secrets::DriverId(driver.to_string()), acct);
    Some((store, cred))
}
