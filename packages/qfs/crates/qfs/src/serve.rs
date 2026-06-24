//! The `qfs serve` composition root (t32): the binary wires the HTTP serving binding.
//!
//! The HTTP binding (`qfs-http`) is a LEAF that consumes `qfs-server` (the registry + reconcile
//! seam) AND the `qfs-exec` read executor — putting its composition HERE (the terminal binary,
//! the HTTP sibling of the t28 shell composition root) keeps `qfs-cmd` off both crates and lets
//! tokio dead-end in the terminal sink. qfs-cmd dispatches `serve` to this launcher via the
//! injected [`qfs_cmd::ServeLauncher`]; this builds the engine + read registry, runs
//! [`qfs_http::serve_config`] on a tokio runtime, and returns the process exit code.

use std::path::Path;
use std::sync::Arc;

use qfs_core::{CodecRegistry, Engine};
use qfs_exec::ReadRegistry;

/// The default bounded in-memory result-row cap for `qfs serve`.
const SERVE_MAX_ROWS: usize = 10_000;

/// Boot + run `qfs serve <config>` with the HTTP binding wired. Returns the process exit code
/// (`0` clean shutdown, `1` on a boot / bind / runtime error). Never panics.
#[must_use]
pub fn run_serve(config: &Path) -> i32 {
    // The serve-side engine: codecs registered (json/csv response encoding) + an empty mount
    // registry the real driver crates register into (E4/E7 wiring). At t32 the read drivers a
    // boot config references are registered by the deployment; an unregistered source surfaces
    // as a structured 422 at request time, never a panic.
    let mut engine = Engine::new();
    engine.codecs = CodecRegistry::with_builtins();
    // t36: register the always-available, credential-free built-in read sources (the `/status`
    // daemon-liveness table) so a liveness ENDPOINT serves a real JSON body over loopback before
    // any deployment driver is wired. The deployment registers its real E4 drivers on top.
    let mut reads = ReadRegistry::new();
    crate::serve_builtins::register_builtins(&mut engine, &mut reads);
    let engine = Arc::new(engine);
    let reads = Arc::new(reads);

    // The bind address: loopback by default (RFD §10 trusted bind), overridable via env.
    let addr = match resolve_bind_addr() {
        Ok(addr) => addr,
        Err(e) => {
            eprintln!("qfs serve: invalid bind address: {e}");
            return 1;
        }
    };

    // t36: formalize the EC2/Linux daemon under the `RuntimeHost` seam. We boot the config into a
    // ServerState (the same in-memory parse→lower→COMMIT path serve uses), derive the
    // host-agnostic BindingSet, build the TokioHost (fsync'd durable store + on-disk audit ledger
    // under a project-local state dir), and attach the causes through the trait — REUSING (not
    // rebuilding) the qfs-http/qfs-cron/qfs-watchtower composition wired below. A host-setup
    // failure is non-fatal: the legacy in-memory composition still serves (the durable ledger is
    // an additive observability layer at t36; the live-driver applier carry-over is unchanged).
    if let Err(e) = attach_daemon_host(config) {
        tracing::warn!(target: "qfs::serve", error = %e, "daemon host setup degraded; serving without the on-disk ledger");
    }

    // The serve composition is async (listener + supervised ctrl_c wait). Build the runtime at
    // this leaf boundary so tokio dead-ends in the binary.
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("qfs serve: cannot start runtime: {e}");
            return 1;
        }
    };

    // t33 cron composition: build the cron binding (reconciled by the runtime from /server/jobs)
    // and spawn the native scheduler daemon over a binary-local JobStore + committer. The daemon
    // reads the binding's live JOB set; its tokio loop dead-ends in this terminal binary.
    let (cron_binding, jobs_handle, cron_policies) = qfs_cron::build_cron_binding();
    let cron_store = crate::cron::LedgerJobStore::new(jobs_handle);
    // t35: the cron committer enforces the JOB's bound POLICY against the built plan before any
    // apply (default-deny / atomic abort) and emits one FiredPlanRecord per fire to this sink.
    let cron_audit = Arc::new(qfs_cron::AuditSink::new());
    let cron_committer = crate::cron::PreviewCommitter::with_policy(
        clone_engine(&engine),
        cron_policies,
        Arc::clone(&cron_audit),
    );

    // t34 watchtower composition: build the watchtower binding + shared bus + the webhook ingest
    // fallback the HTTP listener routes `/hooks/...` to. The binding is reconciled by the runtime
    // from /server/{webhooks,triggers}; the dispatch loop drains the bus and fires matching
    // triggers through the injected committer (PREVIEW path), acking only after a successful
    // commit (at-least-once). The watchtower's own fire-audit sink is drained on shutdown.
    let secrets: Arc<dyn qfs_secrets::Secrets> = Arc::new(qfs_secrets::InMemoryStore::new());
    let (wt_binding, wt_rx, wt_bus, wt_fallback, wt_policies) =
        crate::watchtower::build_watchtower(Arc::clone(&secrets));
    let wt_triggers = wt_binding.triggers_handle();
    let wt_audit = Arc::new(qfs_watchtower::AuditSink::new());

    let result = rt.block_on(async move {
        let scheduler = qfs_cron::Scheduler::new(cron_store, qfs_cron::SystemClock, cron_committer);
        // Spawn the daemon loop; it runs until the process exits (the serve future drives the
        // supervised ctrl_c wait + audit drain). A panic in the daemon never aborts serve.
        let daemon = tokio::spawn(async move {
            qfs_cron::run_daemon(
                scheduler,
                qfs_cron::DaemonConfig::default(),
                std::future::pending::<()>(),
            )
            .await;
        });
        // Spawn the watchtower dispatch loop (drains the bus, fires matching triggers, acks on
        // success). It runs until the bus is dropped (shutdown).
        let dispatch = crate::watchtower::spawn_dispatch_loop(
            wt_rx,
            wt_bus,
            wt_triggers,
            Arc::clone(&wt_audit),
            clone_engine(&engine),
            wt_policies,
        );
        // `cron_binding` is ALREADY a `Box<dyn Binding>` (from build_cron_binding); the watchtower
        // binding is boxed here. Both are `qfs_server::Binding` (qfs-watchtower re-exports the same
        // trait), so they share the one binding vector the runtime reconciles.
        let bindings: Vec<Box<dyn qfs_watchtower::Binding>> =
            vec![Box::new(wt_binding), cron_binding];
        let served = qfs_http::serve_config_full(
            config,
            engine,
            reads,
            addr,
            SERVE_MAX_ROWS,
            bindings,
            Some(wt_fallback),
        )
        .await;
        daemon.abort();
        dispatch.abort();
        served
    });

    // t35: drain the cron fired-plan audit ledger on shutdown (secret-free summaries — driver +
    // path + verb/rule only). The watchtower's own fire-audit sink drains via its dispatch loop.
    let drained = cron_audit.drain();
    if drained > 0 {
        tracing::info!(target: "qfs::serve", fired = drained, "drained cron fired-plan audit ledger");
    }

    match result {
        Ok(()) => 0,
        Err(e) => {
            // The error is already secret-free (boot / bind / runtime); surface it on stderr.
            eprintln!("qfs serve: {e}");
            1
        }
    }
}

/// Clone the serve engine's registries into a fresh `Engine` for the cron committer (so a DO body
/// resolves against the same mounts/codecs the deployment registered). `Engine` is not `Clone`;
/// we rebuild it from the shared registries.
fn clone_engine(engine: &std::sync::Arc<Engine>) -> Engine {
    let mut fresh = Engine::new();
    fresh.mounts = engine.mounts.clone();
    // CodecRegistry is not Clone; rebuild the same builtin set the serve engine uses.
    fresh.codecs = CodecRegistry::with_builtins();
    fresh
}

/// t36: boot the config into a `ServerState`, derive the host-agnostic `BindingSet`, build the
/// daemon [`crate::host::TokioHost`] over a project-local state dir, and attach the causes through
/// the [`qfs_host::RuntimeHost`] seam (recording the attachment to the on-disk audit ledger). This
/// is the formalization of the existing serve composition under the trait — it boots the config
/// once more (idempotent, in-memory) only to PROJECT the binding set; the listener/interval/bus
/// are wired by the legacy composition below, unchanged.
fn attach_daemon_host(config: &Path) -> Result<(), String> {
    use qfs_host::RuntimeHost;

    // The state dir: `QFS_STATE_DIR` if set, else a worktree-local `.qfs-state` (NEVER a system
    // path — system-safety: this is a regular project, the daemon writes only project-local state).
    let state_dir = std::env::var("QFS_STATE_DIR").unwrap_or_else(|_| ".qfs-state".to_string());

    // Boot the config into a BindingSet via qfs-host (the qfs-server coupling lives behind its
    // `host-daemon` feature, so the binary stays the thin entrypoint the dep-direction guard pins).
    let bindings = qfs_host::bindings_from_config(config)?;
    tracing::info!(target: "qfs::serve", summary = %bindings.summary(), "t36 host binding set derived");

    let host = crate::host::TokioHost::open(&state_dir).map_err(|e| format!("host: {e}"))?;
    // Attach the causes through the trait (the futures are non-suspending ledger writes; drive them
    // with qfs-host's tiny block_on so this stays off the tokio runtime built below).
    qfs_host::block_on(host.serve_endpoints(&bindings)).map_err(|e| format!("endpoints: {e}"))?;
    qfs_host::block_on(host.schedule_jobs(&bindings)).map_err(|e| format!("jobs: {e}"))?;
    qfs_host::block_on(host.consume_events(&bindings)).map_err(|e| format!("events: {e}"))?;
    let _ = host.now();
    tracing::info!(target: "qfs::serve", ledger = %host.ledger().path().display(), "t36 daemon host attached (on-disk audit ledger)");
    Ok(())
}

/// Resolve the HTTP bind address: `QFS_HTTP_ADDR` if set, else the loopback default.
fn resolve_bind_addr() -> Result<std::net::SocketAddr, String> {
    let raw =
        std::env::var("QFS_HTTP_ADDR").unwrap_or_else(|_| qfs_http::DEFAULT_BIND_ADDR.to_string());
    raw.parse().map_err(|e| format!("{raw}: {e}"))
}
