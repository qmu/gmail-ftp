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

    // t46: open the System-DB session store so the local web/dashboard face CAN issue + validate
    // sessions (the composition root injecting the store, mirroring how the cron/watchtower bindings
    // are wired). Best-effort + INERT this milestone: NO endpoint is gated on a session yet
    // (authorization is M2; refusing unauthenticated requests is t50/t51), so we only prove the store
    // is ready — the same "wire the System DB without routing a command through it" posture t42 took.
    match crate::session::open_session_store() {
        Ok(_store) => {
            tracing::debug!(target: "qfs::serve", "t46 session store ready (authentication state only; no endpoint gated on it yet)");
        }
        Err(e) => {
            tracing::debug!(target: "qfs::serve", error = %e, "t46 session store unavailable (continuing without sessions)");
        }
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

    // t65 (decision M revised): the internal JOB scheduler daemon is RETIRED. `qfs serve` no longer
    // builds a CronBinding, a binary-local JobStore, or spawns a tokio interval loop — qfs is not a
    // scheduler. A `/server/jobs` row is a SAVED named plan + its cadence: an external scheduler
    // drives it (OS `cron` invoking `qfs job run`; Cloudflare Cron Triggers firing the managed
    // Worker). The JOB DEFINITION surface is unchanged — `CREATE JOB … EVERY … DO …` still lands a
    // `/server/jobs` row (the wrangler `[triggers] crons` generation and `qfs job cron` read it),
    // qfs just no longer ticks it itself. See crates/qfs/src/job.rs and docs/cookbook/automation.md.

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

    // t48 OAuth-AS composition: load/generate the active ES256 signing key over the System DB
    // (unlocked via QFS_PASSPHRASE) and pre-render the three public discovery documents
    // (`/.well-known/oauth-protected-resource`, `/.well-known/oauth-authorization-server`,
    // `/jwks.json`) for the listener's issuer. As of t49 it also serves the live auth-code + PKCE
    // flow + token endpoint; as of t50 it yields the bearer-validation material that gates `/mcp`.
    // Without a passphrase / System DB the routes are simply not served. The handler is composed into
    // the SAME Fallback seam as `POST /mcp`.
    let oauth_routes = crate::oauth::boot_oauth(addr);

    // t47/t50 MCP composition: build the MCP serving binding (qfs-mcp, the MCP sibling of
    // qfs-http/qfs-cron/qfs-watchtower) over the injected ServeMcpEngine, and compose its pure
    // `POST /mcp` handler into the qfs-http listener via the SAME Fallback seam the watchtower
    // webhook ingest uses — so qfs-mcp serves no HTTP itself and needs no tokio. t50: when the OAuth
    // AS booted, the endpoint is GUARDED by a bearer-validating authorizer (verify the
    // `Authorization: Bearer <jwt>` access token against the JWKS + iss/aud/exp; a missing/invalid/
    // expired token is a 401 with a `WWW-Authenticate: Bearer` challenge pointing at the PRM). When
    // the AS is NOT configured (no passphrase / System DB) the endpoint falls back to the inert
    // allow-all, localhost-only posture (no tokens can be minted OR verified without the AS). The
    // combined fallback tries the OAuth routes, then the MCP path, then the watchtower ingest, then 404.
    let mcp_engine: Arc<dyn qfs_mcp::McpEngine> =
        Arc::new(crate::mcp::ServeMcpEngine::new(Arc::clone(&engine)));
    // t51: the dashboard bridge drives the SAME injected engine — clone the handle BEFORE the MCP
    // binding takes ownership of `mcp_engine` below (so both faces share one statement path).
    let dashboard_engine = Arc::clone(&mcp_engine);
    let mcp_binding = Arc::new(match &oauth_routes {
        Some(routes) => {
            let v = routes.mcp_verification();
            let authorizer: Arc<dyn qfs_mcp::McpAuthorizer> =
                Arc::new(crate::mcp::BearerAuthorizer::new(
                    v.jwks.clone(),
                    v.issuer.clone(),
                    v.audience.clone(),
                    &v.prm_url,
                ));
            tracing::info!(target: "qfs::serve", issuer = %v.issuer, audience = %v.audience, "t50 MCP endpoint is bearer-gated (Authorization: Bearer required; 401 + WWW-Authenticate otherwise)");
            qfs_mcp::McpBinding::with_authorizer(mcp_engine, authorizer)
        }
        None => {
            tracing::warn!(target: "qfs::serve", "t50 MCP endpoint UNAUTHENTICATED (no OAuth AS: set QFS_PASSPHRASE + System DB to enable bearer gating); relying on the localhost-only bind");
            qfs_mcp::McpBinding::new(mcp_engine)
        }
    });

    // t51 dashboard shell composition: the embedded SPA + its thin `describe`/`preview` JSON bridge
    // ride the SAME Fallback seam as `POST /mcp`, driving the SAME injected `McpEngine` (so the
    // dashboard and MCP faces share ONE statement-execution path — no second executor). The shell is
    // preview/read only this slice (no commit path; commit cards are t52) and served loopback-only
    // (no session gate yet — t46/t50 wiring is a documented follow-up in `crate::dashboard`).
    let combined_fallback: qfs_http::Fallback = {
        let mcp = Arc::clone(&mcp_binding);
        let wt = wt_fallback;
        let oauth = oauth_routes.clone();
        let dashboard = dashboard_engine;
        Arc::new(move |req: &qfs_http::HttpRequest| {
            // The three read-only OAuth discovery routes win first (public, cacheable, no creds).
            if let Some(routes) = &oauth {
                if let Some(resp) = routes.handle(req) {
                    return Some(resp);
                }
            }
            if req.path == qfs_mcp::MCP_PATH {
                return Some(crate::mcp::serve_mcp_request(&mcp, req));
            }
            // The dashboard shell + bridge (`GET /`, `GET /assets/*`, `POST /api/*`). Returns None
            // for any non-dashboard path, so the watchtower webhook ingest still gets its chance.
            if let Some(resp) = crate::dashboard::serve_dashboard(dashboard.as_ref(), req) {
                return Some(resp);
            }
            wt(req)
        })
    };

    let result = rt.block_on(async move {
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
        // t65: the cron binding is gone (the scheduler daemon is retired). The watchtower binding
        // is boxed here as the one `qfs_watchtower::Binding` the runtime reconciles. `/server/jobs`
        // rows still reconcile into the registry (the JOB DEFINITION surface is intact); they are
        // simply no longer fired in-process — an external scheduler invokes `qfs job run`.
        let bindings: Vec<Box<dyn qfs_watchtower::Binding>> = vec![Box::new(wt_binding)];
        let served = qfs_http::serve_config_full(
            config,
            engine,
            reads,
            addr,
            SERVE_MAX_ROWS,
            bindings,
            Some(combined_fallback),
        )
        .await;
        dispatch.abort();
        served
    });

    match result {
        Ok(()) => 0,
        Err(e) => {
            // The error is already secret-free (boot / bind / runtime); surface it on stderr.
            eprintln!("qfs serve: {e}");
            1
        }
    }
}

/// Clone the serve engine's registries into a fresh `Engine` for the watchtower committer (so a
/// fired-trigger plan resolves against the same mounts/codecs the deployment registered). `Engine`
/// is not `Clone`; we rebuild it from the shared registries.
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
