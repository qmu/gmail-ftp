//! Planner-owned **E2E / external-interface** black-box validation of the t30 server
//! runtime + `/server` self-config driver.
//!
//! This is NOT a unit test and NOT a code review: every scenario drives the system from the
//! OUTSIDE. Scenarios with an external interface (boot, ctrl_c shutdown, audit drain) spawn
//! the REAL `cfs serve` binary as a subprocess and assert its observable behaviour (exit
//! status + the secret-free tracing it emits). The remaining scenarios drive the
//! `cfs-server` PUBLIC API as a black box — the embedder/agent surface — observing the
//! COMMITTED EFFECT (the resulting `ServerState`), never private internals. No live creds,
//! no network: `/server` writes are in-memory.
//!
//! ## Why these live in `cfs-cmd`, not in the `cfs` binary crate
//! The `cfs` binary crate is guarded (by `tests/dep_direction.rs`) to depend on `cfs-cmd`
//! ONLY among workspace crates. So the E2E lives one layer down in `cfs-cmd` (which already
//! owns the `serve` dispatch and depends on `cfs-core` + `cfs-server`) and locates the built
//! `cfs` binary next to the test runner.
//!
//! ## Why no `cfs-parser` here (deliberate constraint)
//! `tests/dep_direction.rs` forbids `cfs-cmd` from depending on `cfs-parser` — and the guard
//! counts dev-deps (it reads `cargo metadata` `dependencies`). So this test NEVER calls the
//! parser directly: it lowers + commits source through the public `Runtime::apply_source`
//! (which parses internally) and asserts on the resulting `ServerState`. Observing the
//! committed effect is the stronger black-box assertion anyway: sugar-equivalence is proven
//! by the two forms producing the same STORED row, not just the same intermediate plan node.
//!
//! Scenario map (ticket acceptance criteria):
//!  1. Boot the real binary: `cfs serve <fixture>` boots without network/creds, reaches the
//!     run loop; SIGINT shuts down cleanly and DRAINS the audit sink (count == # mutations).
//!  2. Deterministic `ServerState` snapshot: per-collection counts + byte-stable serde.
//!  3. Idempotent re-apply: booting the same file twice is a no-op (UPSERT converges).
//!  4. CREATE === INSERT body-less sugar equivalence: identical STORED rows.
//!  5. CREATE === INSERT body-BEARING equivalence: the canonical-spec body matches its INSERT
//!     twin (t31 closed the t30 gap CO-t30-2/3; the former inequality tripwire is now flipped).
//!  6. Unsupported-verb rejection at PLAN time (structured error, no panic, no COMMIT).
//!  7. `DESCRIBE /server/triggers` returns the trigger schema with no live backend.
//!  8. Binding reconcile invoked exactly once per committed `/server` mutation.
//!  9. Purity: introspecting/constructing the driver performs no `ServerState` mutation;
//!     state changes only at COMMIT (apply_source).
//! 10. Secret hygiene: `ServerState`/audit logged projections never render a canary verbatim.

// Test code: assertions and setup may panic/expect/unwrap freely.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use cfs_core::{check_capability, Archetype, Driver, Path, Verb};
use cfs_server::driver::ServerDriver;
use cfs_server::{
    AuditEntry, Binding, BindingKind, NullBinding, Runtime, ServerError, ServerState,
};

// ---------------------------------------------------------------------------
// Subprocess harness (scenario 1)
// ---------------------------------------------------------------------------

/// Locate the built `cfs` binary. The integration-test runner lives in `target/<profile>/deps/`;
/// the binary is the sibling `target/<profile>/cfs`. Built on demand if missing.
fn cfs_bin() -> PathBuf {
    let mut dir = std::env::current_exe().expect("current_exe");
    dir.pop(); // deps/
    dir.pop(); // <profile>/
    let bin = dir.join(if cfg!(windows) { "cfs.exe" } else { "cfs" });
    if !bin.is_file() {
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
        let status = Command::new(cargo)
            .args(["build", "-p", "cfs"])
            .status()
            .expect("build cfs");
        assert!(status.success(), "failed to build the cfs binary");
    }
    assert!(bin.is_file(), "cfs binary not found at {}", bin.display());
    bin
}

/// The in-worktree boot fixture (no system paths, no network, no creds).
fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("server")
        .join("fixtures")
        .join("server_boot.cfs")
}

/// Send SIGINT to a child PID via the `kill` command (no extra crate dep; the harness just
/// delivers the same signal an operator's ctrl_c would).
fn send_sigint(pid: u32) {
    let status = Command::new("kill")
        .args(["-INT", &pid.to_string()])
        .status()
        .expect("spawn kill");
    assert!(status.success(), "kill -INT failed");
}

#[test]
fn serve_boots_blocks_then_sigint_drains_audit_cleanly() {
    // Scenario 1 — the headline external contract: the REAL binary boots the fixture without
    // network/creds, reaches the supervised run loop (it does NOT self-exit), and on SIGINT
    // shuts down cleanly (exit 0) draining the audit sink. We assert the drained entry count
    // == the number of /server mutations in the fixture (8).
    let mut child = Command::new(cfs_bin())
        .args(["serve", fixture_path().to_str().unwrap()])
        .env("RUST_LOG", "cfs::server=info,cfs::server::audit=info")
        // Plain (no ANSI) tracing so `entries=8` is a literal substring in the captured log.
        .env("NO_COLOR", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn cfs serve");
    let mut stderr = child.stderr.take().expect("child stderr");

    // Give it a moment to boot + enter the run loop, then confirm it has NOT exited.
    std::thread::sleep(Duration::from_millis(800));
    assert!(
        child.try_wait().expect("try_wait").is_none(),
        "server must still be running (blocked in the run loop), not self-exited"
    );

    send_sigint(child.id());

    let deadline = Instant::now() + Duration::from_secs(5);
    let status = loop {
        if let Some(s) = child.try_wait().expect("try_wait") {
            break s;
        }
        assert!(
            Instant::now() < deadline,
            "server did not exit after SIGINT"
        );
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(
        status.success(),
        "clean shutdown on SIGINT must exit 0, got {status:?}"
    );

    let mut log = String::new();
    stderr.read_to_string(&mut log).expect("read stderr");

    assert!(log.contains("boot complete"), "boot must complete:\n{log}");
    assert!(
        log.contains("server running"),
        "must reach the supervised run loop:\n{log}"
    );
    assert!(
        log.contains("audit ledger drained") && log.contains("entries=8"),
        "shutdown must drain exactly 8 audit entries (one per /server mutation):\n{log}"
    );
    let drained_lines = log
        .lines()
        .filter(|l| l.contains("cfs::server::audit"))
        .count();
    assert_eq!(
        drained_lines, 8,
        "exactly 8 audit entries flushed on drain (one per /server mutation):\n{log}"
    );
}

#[test]
fn serve_boots_without_network_or_credentials() {
    // Scenario 1 (companion): boot needs no network and no credentials — run with a cleared
    // environment (only PATH so `kill` resolves), and boot still reaches the run loop.
    let mut child = Command::new(cfs_bin())
        .args(["serve", fixture_path().to_str().unwrap()])
        .env_clear()
        .env("RUST_LOG", "cfs::server=info")
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn cfs serve (clean env)");
    let mut stderr = child.stderr.take().expect("child stderr");
    std::thread::sleep(Duration::from_millis(700));
    assert!(
        child.try_wait().expect("try_wait").is_none(),
        "boot with no network/creds must still reach the run loop"
    );
    send_sigint(child.id());
    let _ = child.wait().expect("wait");
    let mut log = String::new();
    stderr.read_to_string(&mut log).expect("read stderr");
    assert!(
        log.contains("boot complete"),
        "boot succeeds with a cleared environment (no network/creds):\n{log}"
    );
}

// ---------------------------------------------------------------------------
// Public-API black-box scenarios (observe the COMMITTED effect via apply_source)
// ---------------------------------------------------------------------------

/// Apply one statement source through the public `Runtime` path and return the snapshot.
/// `apply_source` parses + lowers + COMMITs internally, so this is a true external drive of
/// the whole boot unit without ever touching `cfs-parser` directly.
fn apply_one(src: &str) -> ServerState {
    let mut rt = Runtime::new().with_binding(Box::new(NullBinding));
    rt.apply_source("e2e", 1, src).expect("apply");
    rt.snapshot()
}

/// Boot the fixture (no binding) and return its snapshot.
fn boot_snapshot() -> ServerState {
    let mut rt = Runtime::new();
    rt.boot(&fixture_path()).expect("boot");
    rt.snapshot()
}

#[test]
fn boot_yields_deterministic_per_collection_counts_and_stable_serde() {
    // Scenario 2 — the booted ServerState is deterministic: per-collection counts match the
    // fixture, and two independent boots serialize byte-identically.
    let snap = boot_snapshot();
    assert_eq!(snap.endpoints.len(), 1, "endpoints");
    assert_eq!(snap.triggers.len(), 1, "triggers");
    assert_eq!(snap.jobs.len(), 2, "nightly (DDL) + weekly (INSERT)");
    assert_eq!(
        snap.views.len(),
        2,
        "view + materialized view share /server/views"
    );
    assert_eq!(snap.policies.len(), 1, "policies");
    assert_eq!(snap.webhooks.len(), 1, "webhooks");
    assert!(snap.jobs.contains_key("nightly"));
    assert!(snap.jobs.contains_key("weekly"));
    assert!(snap.views.get("cached_view").unwrap().materialized);
    assert!(!snap.views.get("recent_view").unwrap().materialized);

    let a = serde_json::to_string(&boot_snapshot()).expect("serialize a");
    let b = serde_json::to_string(&boot_snapshot()).expect("serialize b");
    assert_eq!(
        a, b,
        "the ServerState snapshot is deterministic across boots"
    );
}

#[test]
fn re_applying_the_same_config_is_a_no_op() {
    // Scenario 3 — idempotency: booting the same file twice into the same runtime converges
    // (UPSERT), with no duplicate rows.
    let mut rt = Runtime::new();
    rt.boot(&fixture_path()).expect("first boot");
    let first = rt.snapshot();
    rt.boot(&fixture_path()).expect("second boot (replay)");
    let second = rt.snapshot();
    assert_eq!(first, second, "re-applying the config is a no-op (UPSERT)");
    assert_eq!(second.jobs.len(), 2, "no duplicate jobs from replay");
    assert_eq!(second.views.len(), 2, "no duplicate views from replay");
    assert_eq!(first.row_count(), second.row_count());
}

#[test]
fn create_bodyless_equals_insert_sugar_equivalence_via_stored_row() {
    // Scenario 4 — body-less CREATE === INSERT: `CREATE JOB nightly EVERY '1h'` and the
    // INSERT/UPSERT twin produce the IDENTICAL STORED JobDef (the empty DO body desugars to an
    // empty `plan` string, which the INSERT supplies literally). Observing the committed row
    // is the stronger external proof of sugar-equivalence.
    let from_create = apply_one("CREATE JOB nightly EVERY '1h'");
    let from_insert =
        apply_one("UPSERT INTO /server/jobs VALUES (name, every, plan) ('nightly', '1h', '')");
    assert_eq!(
        from_create.jobs.get("nightly"),
        from_insert.jobs.get("nightly"),
        "body-less CREATE JOB and the INSERT twin store an identical JobDef"
    );
    // And the whole snapshots match (same single job, nothing else).
    assert_eq!(
        serde_json::to_string(&from_create).unwrap(),
        serde_json::to_string(&from_insert).unwrap(),
        "the serialized ServerState is identical for the sugar and the explicit write"
    );
}

#[test]
fn body_bearing_create_equals_its_insert_twin_via_canonical_spec() {
    // Scenario 5 — t31 CLOSED the t30 body-storage gap (CO-t30-2/3). Originally a tripwire that
    // asserted INEQUALITY: t30 stored a body-bearing `DO <plan>` as an AST `Debug` projection
    // while the INSERT twin stored the literal source string, so the two STORED `plan` bodies
    // differed. t31 stores the body as a canonical, span-normalised `StatementSpec`/`PlanSpec`
    // (a serialized PARSED AST) AND parses the INSERT's `plan` STRING column into the SAME
    // canonical spec — so the two now genuinely normalise to ONE byte-identical body. The
    // tripwire is flipped from `assert_ne!` to `assert_eq!` to lock in the achieved equivalence.
    let create_body = apply_one("CREATE JOB x EVERY '1h' DO REMOVE /tmp WHERE age > 7")
        .jobs
        .get("x")
        .expect("job x")
        .plan
        .as_str()
        .to_string();
    let insert_body = apply_one(
        "UPSERT INTO /server/jobs VALUES (name, every, plan) ('x', '1h', 'REMOVE /tmp WHERE age > 7')",
    )
    .jobs
    .get("x")
    .expect("job x")
    .plan
    .as_str()
    .to_string();

    // Equivalence now holds: the body-bearing CREATE and its INSERT twin store the identical
    // canonical spec (t31 closed CO-t30-2/3).
    assert_eq!(
        create_body, insert_body,
        "body-bearing CREATE ≡ INSERT: both normalise to one canonical span-normalised spec (t31)"
    );
    // The stored body is the canonical serialized spec (a parsed Statement), not raw source
    // text and not an AST Debug projection.
    assert!(
        create_body.contains("Effect") && create_body.contains("Remove"),
        "stored body is the canonical serialized spec: {create_body:?}"
    );
}

#[test]
fn unsupported_verb_is_rejected_at_plan_time_structured() {
    // Scenario 6 — writing a /server node with an unsupported verb (a blob verb like CP) is
    // rejected at PLAN time via the capability gate with a structured, machine-readable error;
    // no panic, no COMMIT, no state mutation.
    let state = Arc::new(RwLock::new(ServerState::new()));
    let driver = ServerDriver::new(state.clone());
    let path = Path::new("/server/triggers");
    let err = check_capability(&driver, &path, Verb::Cp).expect_err("CP must be rejected");
    match err {
        cfs_core::CfsError::UnsupportedVerb {
            path: p,
            verb,
            supported,
        } => {
            assert_eq!(p, "/server/triggers");
            assert_eq!(verb, "CP");
            assert!(supported.contains(&"SELECT"));
            assert!(supported.contains(&"INSERT"));
            assert!(supported.contains(&"UPSERT"));
            assert!(supported.contains(&"UPDATE"));
            assert!(supported.contains(&"REMOVE"));
            assert!(!supported.contains(&"CP"));
        }
        other => panic!("expected a structured UnsupportedVerb, got {other:?}"),
    }
    assert_eq!(
        state.read().unwrap().row_count(),
        0,
        "no COMMIT, no mutation"
    );
}

#[test]
fn describe_server_triggers_returns_schema_with_no_backend() {
    // Scenario 7 — DESCRIBE /server/triggers returns the trigger schema (name/on/plan) as a
    // relational table, touching no live backend.
    let state = Arc::new(RwLock::new(ServerState::new()));
    let driver = ServerDriver::new(state);
    let desc = driver
        .describe(&Path::new("/server/triggers"))
        .expect("describe triggers");
    assert_eq!(desc.archetype, Archetype::RelationalTable);
    let names: Vec<&str> = desc
        .schema
        .columns
        .iter()
        .map(|c| c.name.as_str())
        .collect();
    assert_eq!(names, vec!["name", "on", "plan"]);
}

/// A counting binding observable from outside the runtime.
#[derive(Clone, Default)]
struct CountingProbe {
    inner: Arc<RwLock<(usize, Option<usize>)>>,
}
impl CountingProbe {
    fn binding(&self) -> ProbeBinding {
        ProbeBinding {
            inner: self.inner.clone(),
        }
    }
    fn reconciles(&self) -> usize {
        self.inner.read().unwrap().0
    }
    fn last_row_count(&self) -> Option<usize> {
        self.inner.read().unwrap().1
    }
}
struct ProbeBinding {
    inner: Arc<RwLock<(usize, Option<usize>)>>,
}
impl Binding for ProbeBinding {
    fn kind(&self) -> BindingKind {
        BindingKind::Null
    }
    fn reconcile(&mut self, state: &ServerState) -> Result<(), ServerError> {
        let mut g = self.inner.write().unwrap();
        g.0 += 1;
        g.1 = Some(state.row_count());
        Ok(())
    }
}

#[test]
fn binding_reconcile_invoked_once_per_committed_mutation() {
    // Scenario 8 — a registered counting binding's reconcile fires exactly once per committed
    // /server mutation; the snapshot it sees reflects the just-applied mutation.
    let probe = CountingProbe::default();
    let mut rt = Runtime::new().with_binding(Box::new(probe.binding()));
    rt.apply_source("test", 1, "CREATE JOB a EVERY '1h'")
        .expect("apply 1");
    assert_eq!(
        probe.reconciles(),
        1,
        "one reconcile after the first mutation"
    );
    assert_eq!(
        probe.last_row_count(),
        Some(1),
        "reconcile saw the post-mutation snapshot"
    );
    rt.apply_source("test", 2, "CREATE JOB b EVERY '2h'")
        .expect("apply 2");
    assert_eq!(
        probe.reconciles(),
        2,
        "one more reconcile after the second mutation"
    );
    assert_eq!(probe.last_row_count(), Some(2));
}

#[test]
fn introspection_is_pure_state_changes_only_at_commit() {
    // Scenario 9 — purity, observed externally: constructing the driver + introspecting it
    // (describe/capabilities) mutates NOTHING; the shared state only changes when a write is
    // COMMITted (via apply_source). This is the externally observable counterpart of the
    // "building a plan is pure" invariant.
    let state = Arc::new(RwLock::new(ServerState::new()));
    let driver = ServerDriver::new(state.clone());
    assert_eq!(state.read().unwrap().row_count(), 0);

    // Introspect repeatedly — pure, no mutation.
    let _ = driver
        .describe(&Path::new("/server/jobs"))
        .expect("describe");
    let _ = driver.capabilities(&Path::new("/server/jobs"));
    let _ = driver
        .describe(&Path::new("/server/endpoints"))
        .expect("describe");
    assert_eq!(
        state.read().unwrap().row_count(),
        0,
        "introspection performed no mutation"
    );

    // Only a COMMITted write changes state — proven by applying one through the runtime.
    let after = apply_one("CREATE JOB nightly EVERY '1h'");
    assert_eq!(after.jobs.len(), 1, "COMMIT is the only thing that mutates");
}

#[test]
fn secret_hygiene_logged_projections_never_render_a_canary_verbatim() {
    // Scenario 10 — secret hygiene: plant a credential-looking canary as a value in a /server
    // write, then confirm the runtime's LOGGED/AUDIT projections (the secret-free surfaces)
    // never render it verbatim. The DTO stores it as owned config data (by-handle by
    // construction); the audit summary + ServerState::summary are counts/names only (RFD §10).
    const CANARY: &str = "SECRET-CANARY-TOKEN-ab12cd34ef";

    // Apply a policy whose `handler` carries the canary, through the public runtime path, and
    // capture the audit ledger the runtime recorded for it.
    let mut rt = Runtime::new().with_binding(Box::new(NullBinding));
    rt.apply_source(
        "test",
        1,
        &format!("UPSERT INTO /server/policies VALUES (name, handler) ('p', '{CANARY}')"),
    )
    .expect("apply policy");

    // The audit ledger entries are secret-free (names + ops only).
    for entry in rt.audit().snapshot() {
        let line = entry.summary();
        assert!(
            !line.contains(CANARY),
            "audit summary must not render the canary value: {line}"
        );
    }
    // Construct the audit entry projection directly too and assert it names node + row only.
    let snap = rt.snapshot();
    assert!(
        !snap.summary().contains(CANARY),
        "ServerState::summary must be counts-only, no value material: {}",
        snap.summary()
    );
    assert!(
        snap.summary().starts_with("endpoints="),
        "summary is the counts projection: {}",
        snap.summary()
    );

    // The value IS stored as owned config data (it is config, not a secret-by-construction);
    // the secret-free guarantee is on the LOGGED projection, not the raw struct.
    assert_eq!(
        snap.policies.get("p").map(|p| p.handler.as_str()),
        Some(CANARY),
        "value stored as owned config data; only the log/audit projection is secret-free"
    );

    // The fixture's boot summary projection likewise leaks no row content.
    let booted = boot_snapshot();
    let summary = booted.summary();
    for needle in ["nightly", "recent", "/recent", "weekly", "noop", "inbox"] {
        assert!(
            !summary.contains(needle),
            "summary is counts-only; must not leak row content `{needle}`: {summary}"
        );
    }

    // Sanity: the AuditEntry projection type is the one the runtime drains (no value carried).
    let probe_entry = AuditEntry::PlanFired {
        cause: "job:nightly".to_string(),
    };
    assert!(!probe_entry.summary().contains(CANARY));
}
