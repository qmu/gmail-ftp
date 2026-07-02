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
    // ADR 0008: the account an effect ran as is the MOUNT's account (there is no selection
    // state). Loaded once per commit — best-effort and metadata-only, like the events themselves.
    let mounts = crate::cloud_mounts::load_cloud_mounts();
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
        // The account the effect ran as: the MOUNT's bound account (the ledger driver id IS the
        // mount's segment id under ADR 0008), defaulting to `default` for local/config-driven
        // drivers. The LABEL only — never the secret material behind it.
        let connection = mounts
            .iter()
            .find(|m| m.remap().is_some_and(|r| r.outer_id() == entry.driver))
            .and_then(|m| m.account.clone())
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

    // Cloud mounts (ADR 0008 §4 — mount-bound accounts): every connect-created cloud mount
    // (gmail/gdrive/ga/github/slack/s3/r2) registers its OWN apply driver under the mount's
    // segment id, bound to the MOUNT's account — never a process-global selection. N mounts of
    // one kind coexist as N registered drivers. FAIL CLOSED per mount: no account on a cloud
    // mount, an unconfigured operator app, a refused t54 sign-in/consent gate, or an
    // unresolvable credential all leave THAT mount unregistered (a commit against it then fails
    // "no driver / not configured", honest) without affecting sibling mounts.
    reg = register_cloud_mounts(reg, &crate::cloud_mounts::load_cloud_mounts());

    reg
}

/// Register every connect-created cloud mount's apply driver into `reg` (ADR 0008 §4): each
/// mount gets its kind's real apply driver, bound to the MOUNT's account and wrapped in a
/// [`MountApplyDriver`](crate::mount_adapter::MountApplyDriver) so the interpreter routes the
/// mount's segment id to it. Factored out (and taking the mounts as a parameter) so the
/// **fail-closed** contract is hermetic: `register_cloud_mounts(reg, &[])` touches no store and
/// registers nothing — exactly the fresh, nothing-connected state. Per-mount failures
/// (no account, no operator app, refused consent gate, unresolvable credential) skip THAT mount
/// only, so one broken mount cannot sink its siblings.
fn register_cloud_mounts(
    mut reg: DriverRegistry,
    mounts: &[crate::cloud_mounts::CloudMount],
) -> DriverRegistry {
    for mount in mounts {
        let Some(remap) = mount.remap() else { continue };
        if let Some(apply) = cloud_apply_driver(mount) {
            reg = reg.with(
                remap.outer_id(),
                Arc::new(crate::mount_adapter::MountApplyDriver::new(remap, apply)),
            );
        }
    }
    reg
}

/// Build the live apply driver for one cloud mount, bound to the mount's account — or `None`
/// (fail closed, secret-free debug log) when the mount cannot bind. The account/credential
/// resolution is the mount's row alone: a Google mount's `account` is the email whose refresh
/// token the stack reads; a github/slack/s3/r2 mount's `account` is the credential label the
/// token was sealed under (defaulting to `default`).
fn cloud_apply_driver(
    mount: &crate::cloud_mounts::CloudMount,
) -> Option<Arc<dyn qfs_runtime::ApplyDriver>> {
    match mount.kind.as_str() {
        "gmail" | "gdrive" | "ga" | "google-analytics" => {
            let stack = google_stack_for_mount(mount)?;
            Some(match mount.kind.as_str() {
                "gmail" => {
                    let client: Arc<dyn qfs_driver_gmail::GmailClient> = Arc::new(
                        qfs_driver_gmail::GoogleApiGmailClient::new(stack.api.clone()),
                    );
                    let driver = qfs_driver_gmail::GmailDriver::new(client);
                    Arc::new(qfs_driver_gmail::gmail_apply_driver(&driver))
                }
                "gdrive" => {
                    let client: Arc<dyn qfs_driver_gdrive::GDriveClient> = Arc::new(
                        qfs_driver_gdrive::GoogleApiDriveClient::new(stack.api.clone()),
                    );
                    let driver = qfs_driver_gdrive::GDriveDriver::new(client);
                    Arc::new(qfs_driver_gdrive::gdrive_apply_driver(&driver))
                }
                _ => {
                    let client: Arc<dyn qfs_driver_ga::GaClient> =
                        Arc::new(qfs_driver_ga::GoogleApiGaClient::new(stack.api.clone()));
                    let driver = qfs_driver_ga::GaDriver::new(client);
                    Arc::new(qfs_driver_ga::ga_apply_driver(&driver))
                }
            })
        }
        "github" => {
            let client = crate::clients::live_github_client(mount_connection(mount))?;
            let driver = qfs_driver_github::GitHubDriver::new(client);
            Some(Arc::new(qfs_driver_github::github_apply_driver(&driver)))
        }
        "slack" => {
            let client = crate::clients::live_slack_client(mount_connection(mount))?;
            let driver = qfs_driver_slack::SlackDriver::new(client);
            Some(Arc::new(qfs_driver_slack::slack_apply_driver(&driver)))
        }
        "s3" => {
            let cfg = crate::objstore::s3_config()?;
            let registry = build_obj_registry("s3", mount_connection(mount), cfg)?;
            let driver = qfs_driver_objstore::S3Driver::new(registry);
            Some(Arc::new(qfs_driver_objstore::s3_apply_driver(&driver)))
        }
        "r2" => {
            let cfg = crate::objstore::r2_config()?;
            let registry = build_obj_registry("r2", mount_connection(mount), cfg)?;
            let driver = qfs_driver_objstore::R2Driver::new(registry);
            Some(Arc::new(qfs_driver_objstore::r2_apply_driver(&driver)))
        }
        // cf is describe-only today (its live driver is a separate ticket) — no apply facet.
        _ => None,
    }
}

/// The credential-store **connection label** a non-Google cloud mount binds: the mount's
/// account, defaulting to `default` (the label `qfs account add <provider>` seals under when
/// none is given).
fn mount_connection(mount: &crate::cloud_mounts::CloudMount) -> &str {
    mount.account.as_deref().unwrap_or("default")
}

/// Build the account-bound [`GoogleStack`](crate::google::GoogleStack) for one Google-kind cloud
/// mount, or `None` (fail closed): the mount must carry an account email (ADR 0008 — no account,
/// no bind; the documented [`crate::google::GOOGLE_ACCOUNT_ENV`] CI override is the one
/// exception), the t54 sign-in + consent gate must pass for the mount's `(kind, account)`, and
/// the operator's OAuth app credentials must resolve.
pub(crate) fn google_stack_for_mount(
    mount: &crate::cloud_mounts::CloudMount,
) -> Option<crate::google::GoogleStack> {
    let email = crate::google::effective_account(
        crate::google::account_override(),
        mount.account.as_deref(),
    );
    let Some(email) = email else {
        tracing::debug!(
            target: "qfs::consent",
            "cloud mount '{}' has no account — reconnect it with an account email",
            mount.path
        );
        return None;
    };
    let consent_kind = if mount.kind == "google-analytics" {
        "ga"
    } else {
        mount.kind.as_str()
    };
    if !cloud_bind_allowed(consent_kind, &email) {
        return None;
    }
    crate::google::google_stack_for_account(&email)
}

/// Shared objstore-registry builder for a `driver` id (`s3`/`r2`) over the mount's credential
/// `connection` label and a resolved [`ObjConfig`](crate::objstore::ObjConfig): resolve + gate
/// the credential exactly like the networked drivers (the t81/t80 bind gates AND the t54 cloud
/// bind gate — a no-op for the non-cloud-classified `s3`/`r2` ids, kept for structural parity
/// and future-proofing), read the secret access key from the store (fail closed on any error),
/// construct the SigV4 [`HttpBackend`](qfs_driver_objstore::HttpBackend) over the shared reqwest
/// exchange, and register the single configured bucket. Returns `None` (driver left
/// unregistered) whenever the credential cannot bind or resolve.
fn build_obj_registry(
    driver: &str,
    connection: &str,
    cfg: crate::objstore::ObjConfig,
) -> Option<qfs_driver_objstore::ObjRegistry> {
    use qfs_driver_objstore::{Bucket, HttpBackend, ObjRegistry, SigV4Credentials};

    let (store, cred) = networked_credential(driver, connection)?;
    if !cloud_bind_allowed(driver, cred.connection.as_str()) {
        return None;
    }
    // Resolve the SECRET access key eagerly (the signer holds it for the commit's lifetime). A
    // locked store / missing credential => fail closed (the driver is left unregistered) rather than
    // binding a backend that cannot sign. The access key id is non-secret routing config.
    let secret = store.get(&cred).ok()?;
    let creds = SigV4Credentials::new(cfg.access_key_id, secret);
    let (amz_date, date_stamp) = crate::objstore::current_signing_dates();
    let backend = HttpBackend::new(
        crate::transport::objstore_exchange(),
        cfg.endpoint,
        creds,
        amz_date,
        date_stamp,
    );
    Some(ObjRegistry::new().with_bucket(cfg.bucket, Bucket::new(Arc::new(backend))))
}

/// Build the live SigV4 [`ObjDriver`](qfs_driver_objstore::ObjDriver) for one objstore cloud
/// mount's READ facet (t-203070): the same fail-closed registry the apply path builds, exposed
/// as the bare `ObjDriver` the read facet (`crate::read_facets::ObjReadDriver`) calls `ls`/`get`
/// on. `None` (fail closed) whenever the routing config or the secret access key is absent.
pub(crate) fn live_obj_read_driver(
    scheme: qfs_driver_objstore::Scheme,
    connection: &str,
) -> Option<qfs_driver_objstore::ObjDriver> {
    use qfs_driver_objstore::Scheme;
    let (driver_id, cfg) = match scheme {
        Scheme::S3 => ("s3", crate::objstore::s3_config()?),
        Scheme::R2 => ("r2", crate::objstore::r2_config()?),
    };
    let registry = build_obj_registry(driver_id, connection, cfg)?;
    Some(qfs_driver_objstore::ObjDriver::new(scheme, registry))
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
pub(crate) fn cloud_bind_allowed(driver: &str, connection: &str) -> bool {
    let did = DriverId::new(driver);
    if !qfs_secrets::is_cloud_driver(&did) {
        return true;
    }
    let signed_in = operator_signed_in();
    let has_consent = consent_recorded(driver, connection);
    match qfs_secrets::bind_gate(&did, connection, signed_in, has_consent) {
        Ok(()) => true,
        Err(e) => {
            // DEBUG, not WARN: the registry is built once per run with EVERY cloud driver, so a
            // WARN here fired for github/slack/gmail/… on every `qfs run` — even a pure `/local`
            // ls or a `create trigger` — reading like a credential failure on an unrelated command
            // (the t8 noise). The operator's actionable signal arrives when they actually TARGET an
            // unbound driver: the read/commit errors (`unknown_source`, or the t5 "connect your
            // account"). Keep the structured, secret-free reason at debug level for troubleshooting.
            tracing::debug!(
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
/// credential `qfs account add <driver> <label>` sealed: the envelope-encrypted SQLite store
/// ([`crate::secret_store::SqliteSecrets`]) when `QFS_PASSPHRASE` + the Project DB exist, else the
/// process-env store (`QFS_SECRET_*`, the agent / CI path). The `connection` label comes from the
/// caller — under ADR 0008 it is the MOUNT's account, never a process-global selection. The
/// secret is **not** read here — the client reads it lazily at request-build time, so a
/// missing/locked credential becomes a clear per-leg auth error at commit, never a panic at registry
/// build. Returns `None` only if the connection id cannot be constructed (impossible for the literal
/// `default` fallback) — in which case the driver is simply left unregistered rather than panicking.
pub(crate) fn networked_credential(
    driver: &str,
    connection: &str,
) -> Option<(Arc<dyn Secrets>, CredentialKey)> {
    let store: Arc<dyn Secrets> = match crate::connection::open_store_for_commit() {
        Some(sqlite) => Arc::new(sqlite),
        None => Arc::new(EnvStore::from_process_env()),
    };
    // t81: a project/team-owned connection is gated on the acting operator's actor-policy BEFORE
    // the credential binds — a member with no grant for the connection's scope cannot use it
    // (default-deny). A denial leaves the driver UNREGISTERED (fail closed, like t54's cloud
    // consent gate); a user-owned connection is unaffected (`bind_allowed` short-circuits to true).
    // Metadata-only + passphrase-free: this never decrypts the secret — it only decides who may bind.
    if !crate::shared_connection::bind_allowed(driver, connection) {
        return None;
    }
    // t80 (decision U / §4.5): a HIGH-SENSITIVITY (end-to-end) connection is wrapped per-recipient and
    // is NOT server-unwrappable — it cannot be used on this AUTONOMOUS commit registry path (no human
    // key in the loop). The E2E attendance gate (`attended = false` here) refuses it, leaving the
    // driver UNREGISTERED (fail closed, audited); using it requires a human recipient unwrap. A
    // non-E2E connection short-circuits to allowed. Metadata-only + passphrase-free (reads the E2E
    // flag, never a token, BEFORE any decrypt).
    if !crate::e2e_store::e2e_bind_allowed(driver, connection) {
        return None;
    }
    // `default` is always a valid connection name; an invalid mount account falls back to it.
    let acct = ConnectionId::new(connection)
        .or_else(|_| ConnectionId::new("default"))
        .ok()?;
    let cred = CredentialKey::new(qfs_secrets::DriverId(driver.to_string()), acct);
    Some((store, cred))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fail closed: with nothing connected (the fresh state — no `path_binding` rows), NO cloud
    /// apply driver is registered, so a `/mail`-shaped commit fails with a clear "no driver"
    /// cause rather than faking success. Hermetic: `register_cloud_mounts(reg, &[])` touches no
    /// store and reads no environment — it is the pure nothing-connected decision.
    #[test]
    fn no_cloud_mounts_registers_no_cloud_drivers() {
        let reg = register_cloud_mounts(DriverRegistry::new(), &[]);
        for id in ["mail", "drive", "ga", "github", "slack", "s3", "r2"] {
            assert!(
                reg.get(&DriverId::new(id)).is_none(),
                "/{id} must be unregistered with nothing connected (fail closed)"
            );
        }
    }

    /// Fail closed per mount (ADR 0008): a Google-kind cloud mount with NO account binds nothing
    /// — the mount is skipped before any store/env is consulted, so a commit against it fails
    /// "no driver" rather than silently using someone else's account.
    #[test]
    fn a_google_mount_without_an_account_is_unregistered() {
        let mount = crate::cloud_mounts::CloudMount {
            path: "/mail".into(),
            kind: "gmail".into(),
            account: None,
        };
        assert!(
            google_stack_for_mount(&mount).is_none(),
            "no account on the mount ⇒ no stack (fail closed)"
        );
        let reg = register_cloud_mounts(DriverRegistry::new(), std::slice::from_ref(&mount));
        assert!(
            reg.get(&DriverId::new("mail")).is_none(),
            "/mail must be unregistered when its mount carries no account"
        );
    }

    /// **The ADR 0008 coexistence proof, hermetic** (the point of the epic): two connect-created
    /// gmail mounts bound to DIFFERENT accounts register two independent apply drivers in ONE
    /// process — exactly what the abolished selection state could never express. Fully local:
    /// a fresh XDG home, an in-DB operator identity (the t54 sign-in leg), per-`(kind, account)`
    /// consent rows (the t54 consent leg), the sealed OAuth app, and two `path_binding` rows.
    /// No network — the refresh token is read lazily at request time, never at registration.
    #[test]
    fn two_gmail_mounts_with_different_accounts_coexist() {
        use qfs_identity::IdentityStore;

        let _g = crate::ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let prev_xdg = std::env::var_os("XDG_CONFIG_HOME");
        let prev_pass = std::env::var_os("QFS_PASSPHRASE");
        let prev_acct = std::env::var_os(crate::google::GOOGLE_ACCOUNT_ENV);
        std::env::set_var("XDG_CONFIG_HOME", dir.path());
        std::env::set_var("QFS_PASSPHRASE", "coexist-pass");
        std::env::remove_var(crate::google::GOOGLE_ACCOUNT_ENV);

        // Sign in the operator (one identity on this host — the sign-in leg of the bind gate).
        {
            let store = crate::identity::open_identity_store().unwrap();
            let unusable = qfs_secrets::Secret::from(qfs_secrets::generate_dek().to_vec());
            let hash = qfs_identity::hash_password(&unusable).unwrap();
            store.signup_local("op@example.com", &hash).unwrap();
        }
        // Seal the operator's OAuth app (what `qfs app add google` does).
        {
            let store = crate::connection::open_store().unwrap();
            let key = CredentialKey::new(
                qfs_secrets::DriverId("google-app".to_string()),
                ConnectionId::new("default").unwrap(),
            );
            store
                .put(
                    &key,
                    qfs_secrets::Secret::from(
                        r#"{"client_id":"id.apps.example","client_secret":"s3cr3t"}"#,
                    ),
                )
                .unwrap();
        }
        // Two authorized accounts (consent keyed by email — what `qfs account add google` does)
        // and two connect-created mounts, one per account.
        {
            let proj = crate::connection::open_project_conn().unwrap();
            for email in ["work@example.com", "home@example.com"] {
                crate::secret_store::db_record_consent(
                    &proj,
                    "gmail",
                    email,
                    "op@example.com",
                    "gmail.modify gmail.compose",
                )
                .unwrap();
            }
            crate::path_binding::db_upsert_binding(
                &proj,
                "/mail",
                "gmail",
                None,
                None,
                None,
                Some("work@example.com"),
            )
            .unwrap();
            crate::path_binding::db_upsert_binding(
                &proj,
                "/mail2",
                "gmail",
                None,
                None,
                None,
                Some("home@example.com"),
            )
            .unwrap();
        }

        let mounts = crate::cloud_mounts::load_cloud_mounts();
        assert_eq!(mounts.len(), 2, "both cloud mounts enumerate");
        let reg = register_cloud_mounts(DriverRegistry::new(), &mounts);
        assert!(
            reg.get(&DriverId::new("mail")).is_some(),
            "/mail (work account) must register its own apply driver"
        );
        assert!(
            reg.get(&DriverId::new("mail2")).is_some(),
            "/mail2 (home account) must register its own apply driver — two accounts of one \
             driver coexist as two mounts in one process"
        );
        // No other cloud kind sneaks in off the two gmail mounts.
        assert!(reg.get(&DriverId::new("drive")).is_none());

        match prev_xdg {
            Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
            None => std::env::remove_var("XDG_CONFIG_HOME"),
        }
        match prev_pass {
            Some(v) => std::env::set_var("QFS_PASSPHRASE", v),
            None => std::env::remove_var("QFS_PASSPHRASE"),
        }
        match prev_acct {
            Some(v) => std::env::set_var(crate::google::GOOGLE_ACCOUNT_ENV, v),
            None => std::env::remove_var(crate::google::GOOGLE_ACCOUNT_ENV),
        }
    }
}
