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
    // Time the commit stage so the t77 telemetry trace span can attribute a slow `commit`.
    let started = std::time::Instant::now();
    let outcome = rt
        .block_on(interp.commit(plan.clone(), &CapabilitySet::allow_all()))
        .map_err(|e| ExecError::new(ErrorKind::CommitFailed, "commit_failed", format!("{e:?}")))?;
    let commit_ms = started.elapsed().as_secs_f64() * 1000.0;
    // t76: emit one hash-chained audit event per committed effect (and per ATTEMPTED irreversible
    // effect) — BEFORE the completeness check, so a partial commit still audits the legs that
    // actually applied and any irreversible leg that was tried. Best-effort + metadata-only: a
    // missing/locked System DB never fails or masks the commit, and an event carries verb + path +
    // connection only, never row data or a secret (the boundary `describe` enforces).
    emit_audit(plan, &outcome);
    // t77: route the SAME audit signal (+ commit metrics + a commit trace span) to the configured
    // externalized sink (file/stdout/OTel). Best-effort + metadata-only, exactly like emit_audit: a
    // sink failure never fails or masks the commit, and no secret/row data ever reaches a sink.
    emit_telemetry(plan, &outcome, commit_ms);
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
    let events = audit_events(plan, outcome);
    if events.is_empty() {
        return;
    }
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
    for event in events {
        if let Err(e) = crate::audit::append_event(&sys, event) {
            tracing::debug!(target: "qfs::audit", "audit append failed (continuing): {e}");
        }
    }
}

/// Build the METADATA-ONLY [`AuditEvent`] for every committed effect (and every attempted
/// irreversible effect) in `outcome` — the shared source of truth for BOTH the t76 hash chain
/// (`emit_audit`) and the t77 externalized audit signal (`emit_telemetry`), so the two funnels can
/// never disagree about which effects audit. `/sys/*` legs are skipped (they self-audit
/// transactionally at the source of truth — see `sys.rs`), so the best-effort emitters never
/// double-record the chain for the same effect.
fn audit_events(
    plan: &qfs_core::Plan,
    outcome: &qfs_runtime::Outcome,
) -> Vec<qfs_store::audit::AuditEvent> {
    let ts = now_rfc3339();
    let mut events = Vec::new();
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
        // t53: `/sys/*` mutations already self-audit transactionally (see `emit_audit`'s contract).
        if entry.driver.as_str() == "sys" {
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

        events.push(qfs_store::audit::AuditEvent {
            actor: ACTOR_CLI.to_string(),
            connection,
            verb: entry.kind.label().to_string(),
            path,
            committed,
            ts: ts.clone(),
        });
    }
    events
}

/// t77: emit the externalized telemetry signals for one commit to the configured sink
/// (file/stdout/OTel). Three signals ride out:
/// - **audit** — the SAME metadata-only events the t76 chain records (`audit_events`), so a
///   consumer's retention store mirrors the in-process chain;
/// - **metrics** — `qfs_commit_total` (+1) and `qfs_commit_effects_total` (+ applied legs), also
///   bumped in the process-local registry the `/sys/metrics` view reads;
/// - **trace** — one `qfs.commit` span over the timed commit stage, attributed by effect count.
///
/// Best-effort by design (decision V / §6): a sink failure is logged (secret-free) and skipped — it
/// NEVER fails or masks the commit. No secret or row data can reach a sink (the records are
/// metadata-only by construction).
fn emit_telemetry(plan: &qfs_core::Plan, outcome: &qfs_runtime::Outcome, commit_ms: f64) {
    use qfs_store::telemetry::{MetricSample, TelemetryRecord, TraceSpan};

    let sink = crate::telemetry::sink_from_env();
    let emit = |record: TelemetryRecord| {
        if let Err(e) = sink.emit(&record) {
            tracing::debug!(target: "qfs::telemetry", "telemetry emit failed (continuing): {e}");
        }
    };

    // Audit signal: the same events the t76 chain records.
    let events = audit_events(plan, outcome);
    let applied = events.iter().filter(|e| e.committed).count();
    for event in events {
        emit(TelemetryRecord::Audit(event));
    }

    // Metric signal: commit + effect counters (also recorded in the /sys/metrics registry).
    crate::telemetry::incr_counter("qfs_commit_total", 1);
    #[allow(clippy::cast_possible_wrap)]
    crate::telemetry::incr_counter("qfs_commit_effects_total", applied as i64);
    emit(TelemetryRecord::Metric(MetricSample::counter(
        "qfs_commit_total",
        1.0,
    )));
    #[allow(clippy::cast_precision_loss)]
    emit(TelemetryRecord::Metric(MetricSample::counter(
        "qfs_commit_effects_total",
        applied as f64,
    )));

    // Trace signal: one span over the timed commit stage.
    emit(TelemetryRecord::Trace(
        TraceSpan::new("qfs.commit", "commit", commit_ms).with_attr("effects", applied.to_string()),
    ));
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

    // Fs (t68): the first-class `/fs` driver over operator-configured NAMED roots (the allowlist).
    // Registered only when at least one `QFS_FS_<NAME>` is configured; with none, the allowlist is
    // empty (deny-all) and `/fs` is left UNREGISTERED so a `/fs` commit fails closed (no driver)
    // rather than binding a driver that resolves nothing. Real `UPSERT`/`REMOVE`/`CP`/`MV` legs
    // apply through its `FsApplier`, which re-validates every path against a configured root at
    // apply time (defence in depth). The `git`-process-like filesystem writes dead-end here.
    if crate::fs::has_roots() {
        let fs_driver = crate::fs::fs_driver();
        reg = reg.with(
            DriverId::new("fs"),
            Arc::new(qfs_driver_fs::fs_apply_driver(&fs_driver)),
        );
    }

    // GitHub: the real REST client over the production reqwest transport + the resolved credential.
    // t54 / M4 — github is a CLOUD driver: it only binds when a signed-in operator has recorded
    // consent for the selected connection (`cloud_bind_allowed`). Otherwise it is left UNREGISTERED
    // (fail closed) and the refusal reason is logged secret-free, so a `/github` commit fails with a
    // clear cause rather than silently acting without consent.
    if let Some((gh_store, gh_cred)) = networked_credential("github") {
        if cloud_bind_allowed("github", gh_cred.connection.as_str()) {
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

    // Sys (t53): the `/sys/*` administration applier — `INSERT INTO /sys/policies` lands a grant
    // row and appends its own t76 audit row TRANSACTIONALLY (administration observes itself). Wired
    // only when a System DB resolves; an unconfigured `/sys` commit fails closed (no driver). The
    // rusqlite-backed SysBackend lives in the binary (src/sys.rs); the driver crate stays
    // tokio-free, with its applier bridged here like every other runtime leaf.
    if let Some(backend) = crate::sys::SystemDbBackend::open_default() {
        let applier = qfs_driver_sys::SysApplier::new(std::sync::Arc::new(backend));
        reg = reg.with(
            DriverId::new("sys"),
            Arc::new(qfs_driver_sys::sys_apply_driver(&applier)),
        );
    }

    // Claude (t64): the `/claude/...` AI-sessions applier — `INSERT INTO /claude/sessions/<id>/
    // instructions` appends a steering instruction (a REVERSIBLE append; steering an agent never
    // removes state). Wired only when a session source is configured (QFS_CLAUDE_SESSIONS, opt-in);
    // an unconfigured `/claude` commit fails closed (no driver) rather than silently steering
    // nothing. The on-disk SessionSource lives in the binary (src/claude.rs); the driver crate stays
    // tokio-free, with its applier bridged here like every other runtime leaf. Decision K: this
    // appends a message the agent reads — qfs never hosts or calls the LLM.
    if let Some(source) = crate::claude::DirSessionSource::open_default() {
        let applier = qfs_driver_claude::ClaudeApplier::new(std::sync::Arc::new(source));
        reg = reg.with(
            DriverId::new("claude"),
            Arc::new(qfs_driver_claude::claude_apply_driver(&applier)),
        );
    }

    // Slack: same shape (the shared reqwest transport, Slack's body-error rule on). Slack is a CLOUD
    // driver too — gated on the same sign-in + recorded-consent bind rule as github (t54 / M4).
    if let Some((sl_store, sl_cred)) = networked_credential("slack") {
        if cloud_bind_allowed("slack", sl_cred.connection.as_str()) {
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
    }

    reg
}

/// t54 / M4 — the commit-time **bind gate** for a cloud driver: may a credential for
/// `driver`/`connection` bind into the live registry? Consults the SAME pure
/// [`qfs_secrets::bind_gate`] decision the `connection add`/`use` path uses, wiring in the two real
/// state reads:
///
/// - **signed in?** — does a signed-up identity exist on this host (the System-DB identity store, t45;
///   sessions t46 are not yet wired into the one-shot CLI, so presence of an identity is the proxy)?
/// - **consent recorded?** — is there a `connection_consent` row for this `(driver, connection)`
///   (the Project-DB ledger `connection add` writes)?
///
/// Returns `true` to bind. On refusal returns `false` and logs the structured, secret-free
/// [`qfs_secrets::ConsentError`] code so the operator can see WHY a cloud commit fell back to "no
/// driver" (fail closed). A local (non-cloud) driver is never gated — `bind_gate` short-circuits to
/// `Ok` — so this is a no-op for `local`/`git`/`sql`/`sys`. Best-effort + secret-free: it reads only
/// metadata (an identity's existence, a consent row), never a token.
fn cloud_bind_allowed(driver: &str, connection: &str) -> bool {
    let did = DriverId::new(driver);
    if !qfs_secrets::is_cloud_driver(&did) {
        return true;
    }
    let signed_in = operator_signed_in();
    let has_consent = consent_recorded(driver, connection);
    match qfs_secrets::bind_gate(&did, connection, signed_in, has_consent) {
        Ok(()) => true,
        Err(e) => {
            tracing::warn!(
                target: "qfs::consent",
                "cloud driver '{driver}' not bound for connection '{connection}': {} ({})",
                e,
                e.code()
            );
            false
        }
    }
}

/// Is an operator signed in? Best-effort proxy: at least one signed-up identity exists in the
/// System-DB identity store (t45). A missing/unreadable System DB (no config home) reads as NOT
/// signed in, so a cloud driver fails closed rather than open. Reads identity METADATA only.
fn operator_signed_in() -> bool {
    use qfs_identity::{IdentityStore, SoleUser};
    let Ok(store) = crate::identity::open_identity_store() else {
        return false;
    };
    matches!(store.sole_user(), Ok(SoleUser::One(_) | SoleUser::Many))
}

/// Is consent recorded for this cloud `(driver, connection)` in the Project-DB consent ledger
/// (`connection_consent`, written by `connection add`)? Best-effort + passphrase-free (the row carries
/// no key material); an unreadable Project DB reads as NO consent (fail closed).
fn consent_recorded(driver: &str, connection: &str) -> bool {
    let Some(proj) = crate::store::open_project_db().ok().flatten() else {
        return false;
    };
    let conn = proj.into_db().into_connection();
    crate::secret_store::db_get_consent(&conn, driver, connection).is_some()
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
    // t81: a project/team-owned connection is gated on the acting operator's actor-policy BEFORE
    // the credential binds — a member with no grant for the connection's scope cannot use it
    // (default-deny). A denial leaves the driver UNREGISTERED (fail closed, like t54's cloud
    // consent gate); a user-owned connection is unaffected (`bind_allowed` short-circuits to true).
    // Metadata-only + passphrase-free: this never decrypts the secret — it only decides who may bind.
    if !crate::shared_connection::bind_allowed(driver, &connection) {
        return None;
    }
    // t80 (decision U / §4.5): a HIGH-SENSITIVITY (end-to-end) connection is wrapped per-recipient and
    // is NOT server-unwrappable — it cannot be used on this AUTONOMOUS commit registry path (no human
    // key in the loop). The E2E attendance gate (`attended = false` here) refuses it, leaving the
    // driver UNREGISTERED (fail closed, audited); using it requires a human recipient unwrap. A
    // non-E2E connection short-circuits to allowed. Metadata-only + passphrase-free (reads the E2E
    // flag, never a token, BEFORE any decrypt).
    if !crate::e2e_store::e2e_bind_allowed(driver, &connection) {
        return None;
    }
    // `default` is always a valid connection name; an invalid persisted selection falls back to it.
    let acct = ConnectionId::new(&connection)
        .or_else(|_| ConnectionId::new("default"))
        .ok()?;
    let cred = CredentialKey::new(qfs_secrets::DriverId(driver.to_string()), acct);
    Some((store, cred))
}
