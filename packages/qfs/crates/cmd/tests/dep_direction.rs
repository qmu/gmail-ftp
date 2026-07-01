//! Acceptance criterion **C4** (fidelity guard G5): mechanically enforce that
//! `qfs-cmd` holds no domain logic by forbidding a direct dependency on any of the
//! lower domain crates. `qfs-cmd` may depend on `qfs-core` (the hub) and
//! `qfs-server` (the serve arm) only.
//!
//! This is an integration test that shells out to `cargo metadata`, inspects the
//! resolved dependency graph, and fails the build if `qfs-cmd` gains a direct edge
//! to `qfs-lang` / `qfs-plan` / `qfs-driver` / `qfs-codec` / `qfs-parser`. It also
//! asserts the broader acyclic spine (nothing depends on `qfs-cmd`, and the leaf
//! edges go the intended way).
//!
//! `cargo` is invoked via the `CARGO` env var cargo sets for integration tests, so
//! no PATH assumptions are made.

// Test code: assertions and setup may panic/expect/unwrap freely.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;
use std::process::Command;

/// The workspace's **terminal leaves** — the crates where tokio is allowed to dead-end because
/// NOTHING depends on them (so a runtime edge cannot transit back into the spine). This is the
/// single source for the "tokio dead-ends here" rationale shared by the runtime-confinement and
/// terminal-sink guards:
/// - `qfs` — the binary composition root (the true terminal sink).
/// - `qfs-skill` — the `publish = false` assets crate whose only runtime reach is via dev-deps
///   (the golden corpus), which never ship.
/// - `xtask` — the `publish = false` build tool (t40) that links the `qfs` lib facet for doc
///   generation; itself a leaf (nothing depends on xtask). See ADR-0007.
///
/// A crate added here must genuinely be a leaf — the guards below mechanically enforce that no
/// other workspace crate depends back onto it.
const TERMINAL_LEAVES: &[&str] = &["qfs", "qfs-skill", "xtask"];

/// A minimal view of the `cargo metadata` JSON we need.
struct Graph {
    /// package name -> set of direct dependency package names (normal + build + dev,
    /// but we only assert on names that are workspace crates).
    direct_deps: BTreeMap<String, Vec<String>>,
}

fn load_graph() -> Graph {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let output = Command::new(cargo)
        .args(["metadata", "--format-version", "1", "--no-deps"])
        .output()
        .expect("failed to run `cargo metadata`");
    assert!(
        output.status.success(),
        "cargo metadata failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let json: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("cargo metadata produced invalid JSON");

    let mut direct_deps: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let packages = json["packages"]
        .as_array()
        .expect("metadata.packages is an array");
    for pkg in packages {
        let name = pkg["name"].as_str().expect("package name").to_string();
        let deps: Vec<String> = pkg["dependencies"]
            .as_array()
            .expect("package dependencies")
            .iter()
            .filter_map(|d| d["name"].as_str().map(str::to_string))
            .collect();
        direct_deps.insert(name, deps);
    }

    Graph { direct_deps }
}

#[test]
fn cmd_does_not_depend_on_domain_crates_directly() {
    let graph = load_graph();
    let cmd_deps = graph
        .direct_deps
        .get("qfs-cmd")
        .expect("qfs-cmd is a workspace package");

    let forbidden = [
        "qfs-lang",
        "qfs-plan",
        "qfs-driver",
        "qfs-codec",
        "qfs-parser",
    ];
    for f in forbidden {
        assert!(
            !cmd_deps.iter().any(|d| d == f),
            "C4 violation: qfs-cmd must NOT depend directly on {f} \
             (it must route through qfs-core). Direct deps were: {cmd_deps:?}"
        );
    }

    // It must depend on the hub + the serve arm.
    assert!(
        cmd_deps.iter().any(|d| d == "qfs-core"),
        "qfs-cmd must depend on qfs-core"
    );
    assert!(
        cmd_deps.iter().any(|d| d == "qfs-server"),
        "qfs-cmd must depend on qfs-server"
    );
}

#[test]
fn nothing_depends_on_cmd() {
    let graph = load_graph();
    for (pkg, deps) in &graph.direct_deps {
        if pkg == "qfs" {
            // The binary crate is the only thing allowed to depend on qfs-cmd.
            continue;
        }
        assert!(
            !deps.iter().any(|d| d == "qfs-cmd"),
            "spine violation: {pkg} depends on qfs-cmd; only the `qfs` binary may"
        );
    }
}

#[test]
fn nothing_depends_on_the_qfs_binary_so_it_is_a_terminal_sink() {
    // t28 (C1): the `qfs` BINARY is the workspace's terminal sink — NOTHING depends on it. This
    // is the property the two t28 guard relaxations RELY ON, so we assert it explicitly (fail
    // closed) rather than leaving it implicit:
    //
    //   * `runtime_is_confined_to_plan_and_types` exempts the binary as a permitted dependent of
    //     a `qfs-runtime` consumer (`qfs -> qfs-driver-local -> qfs-runtime`). That exemption is
    //     ONLY safe because tokio dead-ends in the binary: a runtime consumer must be a leaf, and
    //     the binary is the leaf that consumes it. If something ever depended on the binary,
    //     tokio could transit THROUGH it back into the spine, and the exemption would be unsound.
    //   * `binary_is_the_thin_entrypoint_plus_the_t28_shell_composition_root` lets the binary
    //     reach UP into qfs-exec / qfs-core / qfs-driver-local (the shell composition root). That
    //     is a layer inversion ONLY if the binary is itself depended upon; as a terminal sink it
    //     is the composition root, which is allowed to reach up.
    //
    // So this test is the load-bearing precondition of BOTH relaxations: it converts "the binary
    // is a sink" from an assumption into a mechanically enforced invariant. NOTE: a Cargo
    // `[[bin]]` package exposes no lib target, so a reverse-dep on it is not even expressible in
    // a Cargo.toml today — making a real violation hard to construct. We assert the property
    // anyway as fail-closed documentation: should the binary ever gain a lib target (or a future
    // crate find a way to depend on it), this guard fires immediately.
    // t40: `xtask` is the SOLE permitted dependent of the `qfs` lib facet. It is build-only
    // tooling (`publish = false`, NOT shipped in any artifact) and is itself a terminal leaf —
    // NOTHING depends on xtask — so tokio reaching xtask through the `qfs` lib STILL dead-ends in
    // a leaf; it never transits back into the spine. The soundness precondition the two t28
    // relaxations rely on is "no SPINE crate depends on the binary", and xtask is not a spine
    // crate. We exempt xtask explicitly (a documented composition-root consumer of `qfs`'s pure
    // doc-generation surface) and keep the invariant for every other package. See ADR-0007.
    let graph = load_graph();
    for (pkg, deps) in &graph.direct_deps {
        if pkg == "qfs" || pkg == "xtask" {
            continue;
        }
        assert!(
            !deps.iter().any(|d| d == "qfs"),
            "terminal-sink violation: {pkg} depends on the `qfs` binary. The binary MUST remain a \
             terminal sink (only the build-only `xtask` leaf may depend on its lib facet) — that \
             is the precondition that makes the t28 runtime-leaf exemption sound (tokio dead-ends \
             in the binary / the xtask leaf) and the binary's reach-up into qfs-exec/qfs-core a \
             composition root rather than a layer inversion. If this fires for a spine crate, the \
             two t28 guard relaxations are no longer safe and must be revisited."
        );
    }
    // Assert the ONLY dependent of `qfs` is `xtask` (fail-closed: a new dependent must be a
    // conscious decision, re-reviewed against the soundness argument above).
    let qfs_dependents: Vec<&String> = graph
        .direct_deps
        .iter()
        .filter(|(pkg, deps)| *pkg != "qfs" && deps.iter().any(|d| d == "qfs"))
        .map(|(pkg, _)| pkg)
        .collect();
    assert_eq!(
        qfs_dependents,
        vec!["xtask"],
        "the only permitted dependent of the `qfs` lib is the build-only `xtask` leaf; got {qfs_dependents:?}"
    );
}

#[test]
fn binary_is_the_thin_entrypoint_plus_the_t28_shell_composition_root() {
    // The `qfs` binary forwards argv to `qfs-cmd` and, since t28, ALSO hosts the interactive
    // shell's composition root: the runtime-coupled local read facet (`qfs-driver-local`) and the
    // registry wiring it injects into `qfs-cmd` via the `ShellLauncher`. That adapter cannot live
    // in qfs-cmd (a `qfs-cmd → qfs-driver-local` edge would make qfs-cmd a non-leaf runtime
    // consumer, tripping the runtime-confinement guard) nor in qfs-exec (confined off the driver
    // crates), so it lives in the binary — the leaf sink where tokio dead-ends. We therefore pin
    // the binary's workspace deps to an EXACT allowed set (qfs-cmd + the shell-composition crates),
    // so an UNINTENDED new binary dep still fails, while the deliberate t28 set is permitted.
    let graph = load_graph();
    let bin_deps = graph.direct_deps.get("qfs").expect("qfs binary package");
    let lower_spine = [
        "qfs-server",
        "qfs-lang",
        "qfs-plan",
        "qfs-driver",
        "qfs-codec",
        "qfs-parser",
    ];
    // The binary must NOT reach directly into the lower spine / the runtime: it composes only
    // through qfs-cmd, qfs-exec (the integration layer's read seam), qfs-core (Engine), and the
    // concrete leaf driver it wires (qfs-driver-local) + qfs-pushdown (ScanNode for the adapter).
    for f in lower_spine {
        assert!(
            !bin_deps.iter().any(|d| d == f),
            "spine violation: the qfs binary must not depend directly on {f}; it composes the \
             shell through qfs-cmd / qfs-exec / qfs-core / qfs-driver-local only. Deps: {bin_deps:?}"
        );
    }
    let allowed = [
        "qfs-cmd",
        "qfs-core",
        "qfs-exec",
        "qfs-driver-local",
        // t68: the binary ALSO wires the first-class `/fs` driver (describe + the live read facet +
        // the policy-gated apply over operator-configured named roots). qfs-driver-fs is a
        // qfs-runtime consumer that must stay a LEAF — only the terminal binary may depend on it —
        // so the operator config (QFS_FS_<NAME>) lives in src/fs.rs and std::fs writes dead-end
        // here. Same composition-root rationale as the t28 shell's qfs-driver-local edge.
        "qfs-driver-fs",
        "qfs-pushdown",
        // t32: the binary is ALSO the `qfs serve` composition root — it wires the HTTP serving
        // binding (qfs-http, a leaf consuming qfs-server + qfs-exec) and injects it into qfs-cmd
        // via the ServeLauncher. This is the HTTP sibling of the t28 shell composition root: the
        // binary is the terminal sink (nothing depends on it), so reaching up into qfs-http is a
        // composition root consuming a leaf binding, not a layer inversion.
        "qfs-http",
        // t65 (decision M revised): the internal JOB scheduler binding (qfs-cron) is RETIRED — qfs
        // is not a scheduler, so the binary no longer wires a CronBinding/daemon and no longer
        // depends on qfs-cron. A `/server/jobs` row is a SAVED named plan an EXTERNAL scheduler
        // invokes via `qfs job run` (src/job.rs), which composes qfs-host (saved-plan extraction +
        // the policy gate) + qfs-exec (build_plan) + qfs-runtime (the real apply) — all already
        // allowlisted leaves. No new binary dep is introduced by externalizing scheduling.
        // t34: the binary ALSO wires the watchtower binding (qfs-watchtower, the watchtower sibling
        // of qfs-http — a leaf consuming qfs-server + qfs-exec). It builds the
        // WatchtowerBinding + the shared LocalBus + the injected Committer + the dispatch loop here
        // and composes the webhook ingest into the qfs-http listener via a fallback closure, so
        // qfs-watchtower's feature-gated tokio dead-ends in the terminal sink. Same rationale.
        "qfs-watchtower",
        // t34: the watchtower resolves webhook signing secrets BY HANDLE from qfs-secrets, which
        // the binary's composition root builds + injects into the WatchtowerBinding. qfs-secrets is
        // a pure leaf (confined to qfs-types) — depending on it adds no runtime/driver coupling.
        "qfs-secrets",
        // t36: the binary ALSO composes the deployment host (qfs-host, the one RuntimeHost seam, a
        // leaf consuming qfs-server only behind `host-daemon`). The binary builds the daemon's
        // TokioHost: RuntimeHost — REUSING the existing qfs-http/qfs-cron/qfs-watchtower serve
        // composition behind the trait — and wires the fsync'd FileDurableStore + on-disk
        // AuditLedger. qfs-host's feature-gated coupling dead-ends in the terminal binary; qfs-cmd
        // stays off it. Same composition-root rationale as the t32/t33/t34 binding leaves.
        "qfs-host",
        // The binary is ALSO the real `qfs run --commit` composition root: it drives the
        // qfs-runtime Interpreter over a live driver registry to apply an effect Plan (the
        // WorldApply hook injected into qfs-cmd). qfs-cmd/qfs-exec stay confined off qfs-runtime;
        // the binary is the terminal sink (a TERMINAL_LEAF) where tokio dead-ends, and it is
        // already the named runtime consumer in `runtime_is_confined_*`'s allowlist. qfs-types
        // supplies the owned DriverId key for the registry.
        "qfs-runtime",
        "qfs-types",
        // t39: the binary is the `qfs describe` composition root — it builds the DESCRIBE-only
        // driver registry from each driver's PURE introspective facet (cred-free mock client /
        // empty registry) and injects it into qfs-cmd via the DescribeProvider. DESCRIBE reaches
        // only the introspective half (never the applier), so these driver crates add no runtime
        // I/O coupling to the terminal binary; qfs-cmd stays off the concrete driver crates. Same
        // composition-root rationale as the t28 shell's qfs-driver-local edge — the binary is the
        // allowlisted leaf that may carry the `qfs-driver-*` describe edges.
        "qfs-driver-gmail",
        "qfs-driver-gdrive",
        "qfs-driver-github",
        "qfs-driver-slack",
        "qfs-driver-ga",
        "qfs-driver-objstore",
        // t-exec networked commit: the binary owns the ONE real reqwest HTTP transport
        // (src/transport.rs), bridging qfs-driver-http's confined `ReqwestClient` onto the github +
        // slack `HttpTransport` seams (a pure delegate — they share qfs-http-core DTOs). reqwest
        // dead-ends here in the terminal leaf, so the driver crates stay transport-agnostic and
        // qfs-cmd/qfs-exec stay off the wire client. qfs-driver-http is already in the
        // runtime-consumer allowlist; this is its only allowed dependent besides the driver layer.
        "qfs-driver-http",
        // The binary registers the Cloudflare driver's cred-free PLANNING mount so `/cf/...`
        // statements plan + describe — a runtime-consumer leaf only the terminal binary depends on,
        // the same composition-root rationale as the other driver edges.
        "qfs-driver-cf",
        // google-drivers live commit: the binary composes the OAuth-authenticated gmail/gdrive/ga
        // commit stack (src/google.rs) over qfs-google-auth — bridging the ONE reqwest transport onto
        // its runtime-free `HttpExchange` seam, building the per-account StoredTokenSource +
        // GoogleApiClient, and registering the apply drivers in src/commit.rs (gated like
        // github/slack). qfs-google-auth is a PURE off-runtime leaf (it shares only qfs-http-core +
        // qfs-secrets, never reqwest/qfs-runtime/qfs-driver-http — pinned by
        // `http_core_is_a_pure_leaf_single_sourcing_the_redaction_set`), so this edge adds NO runtime
        // coupling to the terminal binary; the wire client + the live loopback consent dead-end here.
        "qfs-google-auth",
        // t-exec sql live commit: the binary wires the real SQLite-backed sql driver. qfs-driver-sql
        // is the vendor-free driver (trait + compiler); the production SqliteBackend (rusqlite)
        // lives IN the binary (src/sql.rs) because qfs-driver-sql is a runtime consumer that must
        // stay a leaf — only the binary may depend on it. A binary-only edge; qfs-cmd/qfs-exec stay
        // off it, and rusqlite dead-ends in the terminal binary.
        "qfs-driver-sql",
        // t-exec git live commit: the binary wires the real git driver (CLI-backed apply over
        // on-disk repos; the engine's plan_write seam runs the driver's commit planner). Binary-only
        // edge; qfs-cmd/qfs-exec stay off it, and the `git` process dead-ends in the terminal binary.
        "qfs-driver-git",
        // t53 administration: the binary wires the `/sys/*` admin driver (describe + the live read
        // facet + the policy-grant apply). qfs-driver-sys is a qfs-runtime consumer that must stay a
        // LEAF — only the terminal binary may depend on it — so the production rusqlite-backed
        // SysBackend lives IN the binary (src/sys.rs) and dead-ends here like the SQL backend.
        "qfs-driver-sys",
        // t64 AI-sessions: the binary wires the `/claude/...` session driver (describe + the live
        // read facet + the policy-gated instruction append). qfs-driver-claude is a qfs-runtime
        // consumer that must stay a LEAF — only the terminal binary may depend on it — so the on-disk
        // SessionSource (Claude Code's session state, opt-in via QFS_CLAUDE_SESSIONS) lives IN the
        // binary (src/claude.rs) and its std::fs reads/append dead-end here. Decision K: a path façade
        // over session metadata + an append-log, never an LLM/inference dependency.
        "qfs-driver-claude",
        // t58 identity directory: the binary wraps qfs-driver-directory's in-memory
        // `DirectorySource` into a t57 `MembershipResolver` so `member_of('/directories/...')`
        // resolves against the directory (src/directory.rs). `/directories` is a RESERVED SCOPE
        // REALM (decision P), not a driver-backed mount, so this driver is NOT registered in the
        // MountRegistry — the binary consumes its pure describe surface + read seam directly.
        // qfs-driver-directory is READ-FIRST: it carries a NoopApplier and NO qfs-runtime dep, so it
        // is NOT a runtime consumer (it never appears in `runtime_consumers_allowed`) — the lightest
        // driver shape, like the pure introspective half of qfs-driver-sys minus the apply bridge.
        // The binary is the allowlisted leaf that may carry the `qfs-driver-*` edge; qfs-cmd stays off it.
        "qfs-driver-directory",
        // t39 CO-t39-1: the binary links the embedded agent skill so `qfs skill` ships SKILL.md in
        // the artifact (the NORMAL dep edge that keeps the `include_str!` consts from being
        // dead-stripped). qfs-skill's own `[dependencies]` is EMPTY — it carries no runtime/driver
        // code (its driver edges are dev-deps for the golden corpus only) — so this edge adds zero
        // transitive weight and no runtime/driver coupling to the terminal binary.
        "qfs-skill",
        // t42 persistence foundation: the binary is the ONE place that opens a real DB path
        // (decision F). qfs-store is a SYNC leaf (rusqlite, no tokio) consumed only by the terminal
        // binary, which resolves the System DB path and runs the embedded migrations on start. The
        // edge keeps the persistence substrate off the spine (nothing below the binary names a DB
        // file); rusqlite's libsqlite3 build dead-ends in the terminal binary like the existing
        // qfs-driver-sql backend.
        "qfs-store",
        // t45 identity composition root: the binary wires the System-DB-backed identity store
        // (`SqliteIdentityStore` in qfs-store) + the identity DOMAIN core (qfs-identity) for `qfs
        // identity signup/whoami`, injecting the launcher into qfs-cmd (which stays off both
        // backends). qfs-identity is a pure-ish leaf (no rusqlite/tokio/lang/plan/driver/codec/
        // parser — asserted by `identity_is_a_pure_domain_leaf` below), so the edge adds no
        // runtime/driver coupling to the terminal binary.
        "qfs-identity",
        // t46 session composition root: the binary wires the System-DB-backed session store
        // (`SqliteSessionStore` in qfs-store) + the session DOMAIN core (qfs-session), generating the
        // opaque token from the OS-entropy CSPRNG it owns and persisting only its hash. qfs-session is
        // a pure-ish leaf (no rusqlite/tokio/rand/lang/plan/driver/codec/parser — asserted by
        // `session_is_a_pure_domain_leaf` below), so the edge adds no runtime/driver coupling to the
        // terminal binary (it pulls only qfs-identity/qfs-secrets/qfs-crypto-core leaves).
        "qfs-session",
        // t47 serve composition root: the binary ALSO wires the MCP serving binding (qfs-mcp, a leaf
        // consuming qfs-server + qfs-exec, the MCP sibling of qfs-http/qfs-cron/qfs-watchtower). The
        // binary implements + injects the McpEngine (describe registry / build_plan / the
        // policy-gated + IrreversibleGuard-checked, runtime-backed commit / the redacted connection
        // list) and composes the pure `POST /mcp` handler into the qfs-http listener via a fallback
        // closure. qfs-mcp serves no HTTP itself and carries no tokio — the apply is the injected
        // closure that dead-ends the COMMIT interpreter in this terminal binary — so it does NOT
        // depend on qfs-runtime (asserted by `mcp_binding_is_a_leaf_serve_consumer` below). qfs-mcp
        // also re-exports the qfs-http-core / qfs-server types the binary adapts onto, so the binary
        // needs NO direct edge to either lower-spine crate (its thin-entrypoint pin stays intact).
        "qfs-mcp",
        // t48 serve composition root: the binary ALSO wires the OAuth authorization-server discovery
        // routes (qfs-oauth, a pure-ish leaf — RFC 8414/9728/JWKS builders + ES256 JWS sign/verify
        // over p256, NO tokio/rusqlite). The binary loads/generates the active signing key over the
        // System-DB-backed `OauthKeyStore` (qfs-store), mints the key entropy from the OS CSPRNG it
        // owns, envelope-decrypts the private key into a `Secret`, and composes the three GET routes
        // into the qfs-http listener via the SAME Fallback seam t47 uses for `POST /mcp`. qfs-oauth
        // carries no tokio and is a leaf (asserted by `oauth_is_a_pure_domain_leaf` below), so the
        // edge adds no runtime/driver coupling to the terminal binary.
        "qfs-oauth",
        // t63 agent-fabric transport (roadmap M7, decisions L/N): the binary is the composition root
        // for the qfs-native outbound tunnel + relay. qfs-tunnel is the PURE protocol core (frame
        // codec + relay handshake + in-memory mux) with NO tokio/runtime — the binary (src/tunnel.rs)
        // adapts the t50 bearer validation into the handshake's injected TokenValidator (decision N)
        // and is where the LIVE outbound TCP/TLS dial to qfs Cloud would dead-end (a DOCUMENTED SEAM,
        // not yet socket-wired). It is a pure leaf consumed only by this terminal binary (asserted by
        // `tunnel_is_a_pure_protocol_leaf` below), so the edge adds no runtime/driver coupling.
        "qfs-tunnel",
    ];
    let workspace_prefixed: Vec<&String> =
        bin_deps.iter().filter(|d| d.starts_with("qfs")).collect();
    for d in &workspace_prefixed {
        assert!(
            allowed.contains(&d.as_str()),
            "the qfs binary gained an unexpected workspace dep {d} (allowed: {allowed:?}). If this \
             is intended shell-composition wiring, extend the allowlist; otherwise route it through \
             qfs-cmd. Deps: {bin_deps:?}"
        );
    }
    // It must still depend on qfs-cmd (the dispatch front door).
    assert!(
        bin_deps.iter().any(|d| d == "qfs-cmd"),
        "the qfs binary must still depend on qfs-cmd (the argv dispatch front door)"
    );
}

#[test]
fn types_is_a_leaf_and_codec_depends_on_it() {
    // t05: qfs-types is the canonical type/schema model. It must be a true leaf —
    // it depends on NO other workspace crate (keeping the spine acyclic and the type
    // model vendor-free). qfs-codec and qfs-core depend ON it for the row model.
    let graph = load_graph();
    let workspace_crates = [
        "qfs-cmd",
        "qfs-core",
        "qfs-server",
        "qfs-lang",
        "qfs-plan",
        "qfs-driver",
        "qfs-codec",
        "qfs-parser",
        "qfs-types",
    ];
    let types_deps = graph
        .direct_deps
        .get("qfs-types")
        .expect("qfs-types package");
    for ws in workspace_crates {
        assert!(
            !types_deps.iter().any(|d| d == ws),
            "spine violation: qfs-types must be a leaf but depends on {ws}"
        );
    }

    // The canonical row model flows up: codec and core depend on qfs-types.
    let codec_deps = graph
        .direct_deps
        .get("qfs-codec")
        .expect("qfs-codec package");
    assert!(
        codec_deps.iter().any(|d| d == "qfs-types"),
        "qfs-codec must depend on qfs-types for the canonical row model (t05)"
    );
    let core_deps = graph.direct_deps.get("qfs-core").expect("qfs-core package");
    assert!(
        core_deps.iter().any(|d| d == "qfs-types"),
        "qfs-core must depend on qfs-types to re-export the type model (t05)"
    );

    // t13: the Driver contract's `describe` returns the canonical typed
    // `qfs_types::Schema` (archetype tag + Schema), so qfs-driver depends DIRECTLY on
    // the qfs-types leaf. This is the reconciliation of the old untyped NodeSchema into
    // the one workspace schema; the edge is acyclic because qfs-types is a leaf
    // (qfs-driver → { qfs-plan, qfs-types } → qfs-types).
    let driver_deps = graph
        .direct_deps
        .get("qfs-driver")
        .expect("qfs-driver package");
    assert!(
        driver_deps.iter().any(|d| d == "qfs-types"),
        "qfs-driver must depend on qfs-types for the typed Schema in the Driver contract (t13)"
    );
    assert!(
        driver_deps.iter().any(|d| d == "qfs-plan"),
        "qfs-driver must depend on qfs-plan for the PlanApplier/Plan effect seam (t09/t13)"
    );
}

#[test]
fn runtime_is_confined_to_plan_and_types() {
    // t10 (O3): mechanically lock the tokio confinement. `qfs-runtime` is the sole impure
    // stage (RFD §3/§6 COMMIT); tokio/futures live there and MUST NOT leak into the spine.
    // This is the structural counterpart of `qfs-plan`'s purity test: assert two directions —
    //
    //   (a) `qfs-runtime` depends, among workspace crates, ONLY on `{qfs-plan, qfs-types,
    //       qfs-txn}` (no `qfs-core`/`qfs-parser`/`qfs-driver`/`qfs-codec`/`qfs-lang`/
    //       `qfs-cmd`/`qfs-server`), so the runtime walks the effect plan + the pure
    //       transactional envelope and nothing else; and
    //   (b) NONE of the **pure-spine** crates depends back up onto `qfs-runtime`, so tokio can
    //       never enter the spine's closure via this edge (the confinement that keeps
    //       `qfs-plan` I/O-free and its purity dep-closure test green by construction). A
    //       concrete **driver-impl** crate (t16 `qfs-driver-local`) and the top-level binary
    //       ARE permitted to depend on `qfs-runtime`: they are leaf consumers that bridge a
    //       driver's synchronous `PlanApplier` to the async `ApplyDriver` and register it in
    //       the `DriverRegistry`. Nothing depends back onto *them*, so tokio still cannot
    //       reach the spine — the edge only flows up out of the runtime into a leaf, never
    //       into `qfs-plan`/`qfs-types`/`qfs-driver`/`qfs-codec`/`qfs-txn`/… .
    //
    // t11 added `qfs-txn` (the transactional correctness envelope). It is ITSELF pure
    // orchestration confined to `{qfs-plan, qfs-types}` (no tokio of its own — the runtime
    // bridges its async ApplyDriver to qfs-txn's synchronous LegApplier seam), so admitting
    // the `qfs-runtime → qfs-txn` edge does not widen tokio's reach: qfs-txn carries no
    // async runtime into the spine. We assert that confinement too.
    let graph = load_graph();

    let workspace_crates = [
        "qfs-cmd",
        "qfs-core",
        "qfs-server",
        "qfs-lang",
        "qfs-plan",
        "qfs-driver",
        "qfs-codec",
        "qfs-parser",
        "qfs-types",
        "qfs-runtime",
        "qfs-txn",
    ];

    // (a) runtime's workspace deps are exactly the allowed leaf set (plan/types + the pure
    // qfs-txn envelope).
    let runtime_deps = graph
        .direct_deps
        .get("qfs-runtime")
        .expect("qfs-runtime is a workspace package");
    let allowed = ["qfs-plan", "qfs-types", "qfs-txn"];
    let mut ws_deps: Vec<&String> = runtime_deps
        .iter()
        .filter(|d| workspace_crates.contains(&d.as_str()))
        .collect();
    ws_deps.sort();
    ws_deps.dedup();
    for d in &ws_deps {
        assert!(
            allowed.contains(&d.as_str()),
            "confinement violation: qfs-runtime must depend only on {allowed:?} among \
             workspace crates, but depends on {d} (this would pull tokio toward the spine). \
             Workspace deps were: {ws_deps:?}"
        );
    }

    // (a') qfs-txn is itself confined to {qfs-plan, qfs-types} — it carries no tokio/async
    // runtime, so the runtime → txn edge does not widen the impure surface.
    let txn_deps = graph
        .direct_deps
        .get("qfs-txn")
        .expect("qfs-txn is a workspace package");
    let txn_allowed = ["qfs-plan", "qfs-types"];
    for d in txn_deps
        .iter()
        .filter(|d| workspace_crates.contains(&d.as_str()))
    {
        assert!(
            txn_allowed.contains(&d.as_str()),
            "confinement violation: qfs-txn must stay pure orchestration over \
             {txn_allowed:?} among workspace crates, but depends on {d}"
        );
    }

    // (b) GENERIC LEAF CONFINEMENT — the durable invariant that scales to 11 more driver
    // crates with NO per-driver edit. Every crate that depends on `qfs-runtime` (other than
    // `qfs-runtime` itself) MUST be a leaf — no other workspace crate may depend back onto it.
    // This encodes *why* a runtime consumer is safe (tokio dead-ends in a leaf and cannot
    // transit back into the spine) rather than *which* crates we waved through. A non-leaf
    // gaining the `→ qfs-runtime` edge (e.g. someone makes `qfs-core` depend on the runtime)
    // fails automatically because `qfs-core` is a sink for the rest of the workspace. A new
    // `qfs-driver-s3`/`-drive`/`-gmail` needs no edit here: as long as it is a leaf, the edge
    // is admitted; the moment something depends back onto it, the leaf check fires.
    let runtime_consumers: Vec<&String> = graph
        .direct_deps
        .iter()
        .filter(|(pkg, deps)| {
            pkg.as_str() != "qfs-runtime" && deps.iter().any(|d| d == "qfs-runtime")
        })
        .map(|(pkg, _)| pkg)
        .collect();
    assert!(
        !runtime_consumers.is_empty(),
        "expected at least one qfs-runtime consumer (the bridging driver-impl / binary); \
         found none — the metadata view is likely wrong"
    );
    for consumer in &runtime_consumers {
        let dependent = graph
            .direct_deps
            .iter()
            .find(|(other, od)| {
                other.as_str() != consumer.as_str()
                    && od.iter().any(|d| d == *consumer)
                    // The TERMINAL_LEAVES (`qfs` binary / `qfs-skill` assets / `xtask` build tool)
                    // are exempt: each is a leaf where tokio dead-ends (nothing depends on it), so
                    // depending on a runtime consumer does NOT let tokio transit back into the
                    // spine. The shared rationale + per-crate justification lives on the
                    // TERMINAL_LEAVES constant. (qfs wires qfs-driver-local since t28; qfs-skill's
                    // runtime reach is dev-deps only; xtask links the qfs lib for doc-gen — t40.)
                    && !TERMINAL_LEAVES.contains(&other.as_str())
            })
            .map(|(other, _)| other.clone());
        assert!(
            dependent.is_none(),
            "confinement violation: {consumer} depends on qfs-runtime but is NOT a leaf — \
             {dependent:?} depends back onto it, so tokio could transit out of the runtime, \
             through {consumer}, and back into the spine. A qfs-runtime consumer MUST be a \
             leaf (no workspace crate other than the terminal `qfs` binary may depend onto it)."
        );
    }

    // (b') Belt-and-suspenders: the named allowlist pins *identity* — the exact leaves we
    // expect to bridge into the runtime today — so an UNINTENDED new runtime consumer is
    // caught even if it happens to be a leaf at the moment it is added. The generic leaf
    // check above (b) pins *safety*; this allowlist pins *intent*. A new driver crate appends
    // its name here (a one-line, reviewable signal), and (b) guarantees the append was safe.
    let runtime_consumers_allowed = [
        "qfs-driver-local",
        // t68: the first-class `/fs` driver bridges its FsApplier into the runtime via
        // qfs-runtime's PlanApplierBridge (like every other driver leaf). It is a leaf — only the
        // terminal binary depends on it — so tokio still dead-ends in the binary.
        "qfs-driver-fs",
        "qfs-driver-http",
        "qfs-driver-cf",
        "qfs-driver-gmail",
        "qfs-driver-gdrive",
        "qfs-driver-ga",
        "qfs-driver-sql",
        "qfs-driver-cf",
        "qfs-driver-objstore",
        "qfs-driver-github",
        "qfs-driver-slack",
        "qfs-driver-git",
        // t53: the `/sys/*` administration driver bridges its SysApplier into the runtime via
        // qfs-runtime's PlanApplierBridge (like every other driver leaf). It is a leaf — only the
        // terminal binary depends on it — so tokio still dead-ends in the binary.
        "qfs-driver-sys",
        // t64: the `/claude/...` AI-sessions driver bridges its ClaudeApplier into the runtime via
        // qfs-runtime's PlanApplierBridge (like every other driver leaf). It is a leaf — only the
        // terminal binary depends on it — so tokio still dead-ends in the binary.
        "qfs-driver-claude",
        "qfs",
    ];
    for consumer in &runtime_consumers {
        assert!(
            runtime_consumers_allowed.contains(&consumer.as_str()),
            "confinement violation: {consumer} depends on qfs-runtime but is not in the \
             expected runtime-consumer allowlist ({runtime_consumers_allowed:?}). If this is a \
             new driver-impl leaf bridging an ApplyDriver, add it to the allowlist; otherwise \
             tokio must stay confined to qfs-runtime + leaf driver-impl consumers."
        );
    }
}

#[test]
fn secrets_is_confined_to_types_and_core_consumes_it() {
    // t27: qfs-secrets is the credential / secret store + multi-connection resolution
    // (RFD §10). It is consumer-side, owned-DTO only, and reuses the canonical
    // `qfs_types::DriverId` — so among workspace crates it depends ONLY on qfs-types
    // (a leaf), keeping the spine acyclic (qfs-secrets → qfs-types). qfs-core consumes
    // it (the Engine threads a `Secrets` handle into the driver-bind context) and
    // re-exports it, so the rest of the workspace reaches secrets through the hub.
    let graph = load_graph();
    let workspace_crates = [
        "qfs-cmd",
        "qfs-core",
        "qfs-server",
        "qfs-lang",
        "qfs-plan",
        "qfs-driver",
        "qfs-codec",
        "qfs-parser",
        "qfs-types",
        "qfs-runtime",
        "qfs-txn",
        "qfs-pushdown",
        "qfs-secrets",
    ];

    // (a) secrets' only workspace dependency is qfs-types.
    let secrets_deps = graph
        .direct_deps
        .get("qfs-secrets")
        .expect("qfs-secrets is a workspace package");
    for d in secrets_deps
        .iter()
        .filter(|d| workspace_crates.contains(&d.as_str()))
    {
        assert_eq!(
            d, "qfs-types",
            "spine violation: qfs-secrets must depend only on qfs-types among workspace \
             crates (it carries no driver/plan/vendor coupling), but depends on {d}"
        );
    }

    // (b) core consumes secrets (the bind-context credential surface).
    let core_deps = graph.direct_deps.get("qfs-core").expect("qfs-core package");
    assert!(
        core_deps.iter().any(|d| d == "qfs-secrets"),
        "qfs-core must depend on qfs-secrets to thread the Secrets handle into the Engine (t27)"
    );

    // (c) nothing depends back up onto qfs-cmd via secrets, and the spine stays acyclic:
    // qfs-secrets must NOT depend on any higher crate (already covered by (a)).
}

#[test]
fn exec_is_confined_above_the_spine_and_off_the_runtime() {
    // t29 (CO-t29-4): qfs-exec is the execution / integration layer that composes the SELECT
    // read-path executor (parse -> resolve -> plan -> driver scan -> engine residual -> rows)
    // ABOVE the spine. The t29 topology ruling rests on two structural facts this guard pins
    // mechanically (so the six E7 server crates about to land cannot silently invert it):
    //
    //   (a) qfs-exec's workspace-internal deps are EXACTLY the above-spine set
    //       {qfs-core, qfs-parser, qfs-pushdown, qfs-engine}. In particular it does NOT depend
    //       on qfs-runtime — that absence is what keeps the two impure stages separate (the
    //       runtime owns writes/COMMIT; qfs-exec owns reads/scans via its own ReadDriver seam).
    //       Were qfs-exec to gain a qfs-runtime edge, it would become a runtime consumer and the
    //       runtime leaf-confinement check would (correctly) fire — this assertion catches it
    //       one step earlier with a precise message.
    //   (b) NO spine / lower crate depends back onto qfs-exec — only qfs-cmd (and transitively
    //       the `qfs` binary) may consume it. A future `qfs-core -> qfs-exec` (or any spine ->
    //       qfs-exec) layer inversion fails here, since the spine must never reach UP into the
    //       integration layer.
    let graph = load_graph();

    let workspace_crates = [
        "qfs-cmd",
        "qfs-core",
        "qfs-server",
        "qfs-lang",
        "qfs-plan",
        "qfs-driver",
        "qfs-codec",
        "qfs-parser",
        "qfs-types",
        "qfs-runtime",
        "qfs-txn",
        "qfs-pushdown",
        "qfs-secrets",
        "qfs-http-core",
        "qfs-engine",
        "qfs-exec",
    ];

    // (a) qfs-exec's workspace deps are exactly the above-spine set (no qfs-runtime).
    let exec_deps = graph
        .direct_deps
        .get("qfs-exec")
        .expect("qfs-exec is a workspace package");
    let allowed = ["qfs-core", "qfs-parser", "qfs-pushdown", "qfs-engine"];
    let mut ws_deps: Vec<&String> = exec_deps
        .iter()
        .filter(|d| workspace_crates.contains(&d.as_str()))
        .collect();
    ws_deps.sort();
    ws_deps.dedup();
    for d in &ws_deps {
        assert!(
            allowed.contains(&d.as_str()),
            "topology violation: qfs-exec must depend only on the above-spine set {allowed:?} \
             among workspace crates, but depends on {d}. Workspace deps were: {ws_deps:?}"
        );
    }
    // The defining absence: qfs-exec must NOT consume the runtime (keeps the two impure stages —
    // runtime writes/COMMIT vs. exec reads/scans — structurally separate).
    assert!(
        !exec_deps.iter().any(|d| d == "qfs-runtime"),
        "topology violation: qfs-exec must NOT depend on qfs-runtime — that absence is what keeps \
         the read executor a separate impure stage from the runtime's write/COMMIT stage (t29). \
         A real read driver implements qfs-exec's own ReadDriver seam, never the runtime's."
    );
    // And the four above-spine deps must all be present (the executor genuinely composes them).
    for required in allowed {
        assert!(
            exec_deps.iter().any(|d| d == required),
            "qfs-exec must depend on {required} (the read executor composes the above-spine set)"
        );
    }

    // (b) Only qfs-cmd and the terminal `qfs` binary may depend on qfs-exec — no spine/lower
    // crate may reach UP into it. Since t28 the binary consumes qfs-exec directly too: it hosts
    // the interactive shell's composition root (the local ReadDriver adapter + the
    // `Session`/`VfsPath`/`ReadRegistry` wiring) and injects the wired shell into qfs-cmd via the
    // `ShellLauncher`. The binary is a terminal sink (nothing depends on it), so this is not a
    // layer inversion — it is the composition root consuming the integration layer, exactly as
    // qfs-cmd does. A SPINE/LOWER crate reaching up into qfs-exec still fails here.
    let exec_consumers: Vec<&String> = graph
        .direct_deps
        .iter()
        .filter(|(pkg, deps)| pkg.as_str() != "qfs-exec" && deps.iter().any(|d| d == "qfs-exec"))
        .map(|(pkg, _)| pkg)
        .collect();
    // t32: `qfs-http` (the HTTP serving binding) is a THIRD admitted consumer. It is a LEAF
    // integration consumer of the read executor — the same role qfs-cmd plays — that evaluates
    // an endpoint's query through `execute_read`. It is NOT a spine/lower crate reaching up: it
    // sits ABOVE the spine alongside qfs-cmd, and nothing depends on it except the terminal
    // `qfs` binary (asserted by `http_binding_is_a_leaf_serve_consumer` below), so tokio still
    // dead-ends in the binary. Admitting it does not invert the layering; a spine/lower crate
    // reaching UP into qfs-exec still fails here.
    // t65 (decision M revised): `qfs-cron` (the old JOB scheduler binding) is RETIRED — qfs is not
    // a scheduler, the crate is deleted, and `qfs job run` builds a saved plan from the terminal
    // binary (src/job.rs) over qfs-exec directly. So there is no separate cron exec-consumer leaf;
    // the externalized JOB path uses the binary's own already-admitted qfs-exec edge.
    // t34: `qfs-watchtower` (the event bus / webhook / watcher / trigger-dispatch binding) is a
    // FOURTH admitted consumer. Like qfs-http it is a LEAF integration consumer of the
    // executor — its injected Committer builds a fired trigger's plan via `build_plan` and its
    // watchers poll a source via `execute_read` — sitting ABOVE the spine alongside qfs-cmd, and
    // nothing depends on it except the terminal `qfs` binary (asserted by
    // `watchtower_binding_is_a_leaf_serve_consumer` below). Admitting it does not invert the
    // layering; a spine/lower crate reaching UP into qfs-exec still fails here.
    // t47: `qfs-mcp` (the MCP serving binding) is a FIFTH admitted consumer. Like
    // qfs-http/qfs-watchtower it is a LEAF integration consumer of the executor — its
    // `preview`/`commit` tools build a plan via `build_plan` + `plan_preview` — sitting ABOVE the
    // spine alongside qfs-cmd, and nothing depends on it except the terminal `qfs` binary (asserted
    // by `mcp_binding_is_a_leaf_serve_consumer` below). Admitting it does not invert the layering.
    let allowed_exec_consumers = ["qfs-cmd", "qfs", "qfs-http", "qfs-watchtower", "qfs-mcp"];
    for consumer in &exec_consumers {
        assert!(
            allowed_exec_consumers.contains(&consumer.as_str()),
            "topology violation: {consumer} depends on qfs-exec, but only qfs-cmd, the terminal \
             `qfs` binary (the t28 shell composition root + the t65 `qfs job run` path), qfs-http \
             (the t32 leaf HTTP serving binding), qfs-watchtower (the t34 leaf watchtower binding), \
             and qfs-mcp (the t47 leaf MCP serving binding) may consume the integration layer. A \
             spine/lower crate reaching UP into qfs-exec is a layer inversion (t29)."
        );
    }
}

#[test]
fn watchtower_binding_is_a_leaf_serve_consumer() {
    // t34 (the watchtower binding topology): `qfs-watchtower` is the leaf binding crate that turns
    // external change (webhooks + source watchers) into fired effect-plans. The placement decision
    // (the watchtower sibling of the t32 qfs-http / t33 qfs-cron bindings) rests on four structural
    // facts this guard pins so they cannot invert:
    //
    //   (a) qfs-watchtower consumes qfs-server (the Binding/ServerState/TriggerDef/WebhookDef
    //       registry) AND qfs-exec (the build_plan + execute_read evaluator) — the one crate that
    //       legitimately binds both for the watchtower cause, the reason it is a NEW leaf rather
    //       than living in qfs-server (which must stay off qfs-exec and runtime-free, CO-t30-1) or
    //       qfs-cmd (off qfs-exec/the binding crates). NOTE: qfs-server + qfs-exec are OPTIONAL
    //       deps gated behind qfs-watchtower's default-on `native` feature so the PURE event/
    //       dispatch core builds for wasm32; with default features active, both edges are present.
    //   (b) qfs-watchtower is a LEAF: nothing depends on it except the terminal `qfs` binary (the
    //       serve composition root). That keeps its feature-gated tokio (the LocalBus MPSC +
    //       watcher tasks) dead-ended in the binary — the t28 runtime-leaf exemption precondition.
    //   (c) qfs-watchtower does NOT depend on qfs-runtime. The real commit path is the INJECTED
    //       Committer the binary builds; the dispatcher never drives the COMMIT interpreter, so the
    //       runtime-leaf-confinement guard is untouched (qfs-watchtower never appears in its
    //       allowlist).
    //   (d) qfs-watchtower does NOT depend on qfs-http: the WebhookBinding serves no HTTP itself
    //       (its `ingest` is a pure handler over owned request data the binary composes into the
    //       qfs-http listener), so the two leaves stay independent (option b — neither depends on
    //       the other).
    let graph = load_graph();

    // (a) qfs-watchtower depends on both qfs-server and qfs-exec (with default `native` on).
    let wt_deps = graph
        .direct_deps
        .get("qfs-watchtower")
        .expect("qfs-watchtower is a workspace package");
    for required in ["qfs-server", "qfs-exec"] {
        assert!(
            wt_deps.iter().any(|d| d == required),
            "t34: qfs-watchtower must depend on {required} (it binds the server registry to the \
             build_plan/execute_read evaluator for the watchtower cause). Deps were: {wt_deps:?}"
        );
    }

    // (b) qfs-watchtower is a leaf: only the terminal `qfs` binary may depend on it.
    let wt_consumers: Vec<&String> = graph
        .direct_deps
        .iter()
        .filter(|(pkg, deps)| {
            pkg.as_str() != "qfs-watchtower" && deps.iter().any(|d| d == "qfs-watchtower")
        })
        .map(|(pkg, _)| pkg)
        .collect();
    for consumer in &wt_consumers {
        assert_eq!(
            consumer.as_str(),
            "qfs",
            "t34 leaf violation: {consumer} depends on qfs-watchtower, but only the terminal `qfs` \
             binary (the serve composition root) may consume the watchtower binding. If something \
             else depends on it, qfs-watchtower's tokio bus/watcher tasks no longer dead-end in the \
             binary and the runtime-leaf exemption is unsound."
        );
    }

    // (c) qfs-watchtower must NOT depend on qfs-runtime (the real applier is the injected Committer).
    assert!(
        !wt_deps.iter().any(|d| d == "qfs-runtime"),
        "t34: qfs-watchtower must NOT depend on qfs-runtime — the real commit path is the INJECTED \
         Committer the composition root builds, not the COMMIT interpreter. Deps were: {wt_deps:?}"
    );

    // (d) qfs-watchtower must NOT depend on qfs-http (it serves no HTTP; the binary composes the
    // pure ingest handler into the qfs-http listener — option b, the two leaves stay independent).
    assert!(
        !wt_deps.iter().any(|d| d == "qfs-http"),
        "t34: qfs-watchtower must NOT depend on qfs-http — its WebhookBinding::ingest is a pure \
         handler over owned request data the `qfs` binary composes into the qfs-http listener, so \
         the two serve leaves stay independent (option b). Deps were: {wt_deps:?}"
    );
}

#[test]
fn http_binding_is_a_leaf_serve_consumer() {
    // t32 (the HTTP serving binding topology): `qfs-http` is the leaf binding crate that turns
    // the /server/endpoints registry into live HTTP routes. The t32 placement decision rests on
    // three structural facts this guard pins so the property cannot silently invert:
    //
    //   (a) qfs-http consumes qfs-server (the Binding/ServerState/EndpointDef registry) AND
    //       qfs-exec (the read executor) — it is the one crate that legitimately binds both, the
    //       reason it is a NEW leaf rather than living in qfs-server (which must stay off qfs-exec
    //       and runtime-free, CO-t30-1) or in qfs-cmd (which must stay off qfs-exec/qfs-http).
    //   (b) qfs-http is a LEAF: NOTHING depends on it except the terminal `qfs` binary (the serve
    //       composition root). That is what keeps its tokio HTTP listener dead-ended in the
    //       binary — exactly the precondition the t28 runtime-leaf exemption relies on.
    //   (c) qfs-http does NOT depend on qfs-runtime. It uses tokio for the HTTP I/O domain only,
    //       never the COMMIT interpreter — so the runtime-leaf-confinement guard is untouched
    //       (qfs-http never appears in its consumer allowlist).
    let graph = load_graph();

    // (a) qfs-http depends on both qfs-server and qfs-exec.
    let http_deps = graph
        .direct_deps
        .get("qfs-http")
        .expect("qfs-http is a workspace package");
    for required in ["qfs-server", "qfs-exec"] {
        assert!(
            http_deps.iter().any(|d| d == required),
            "t32: qfs-http must depend on {required} (it binds the server registry to the read \
             executor). Deps were: {http_deps:?}"
        );
    }

    // (b) qfs-http is a leaf: only the terminal `qfs` binary may depend on it.
    let http_consumers: Vec<&String> = graph
        .direct_deps
        .iter()
        .filter(|(pkg, deps)| pkg.as_str() != "qfs-http" && deps.iter().any(|d| d == "qfs-http"))
        .map(|(pkg, _)| pkg)
        .collect();
    for consumer in &http_consumers {
        assert_eq!(
            consumer.as_str(),
            "qfs",
            "t32 leaf violation: {consumer} depends on qfs-http, but only the terminal `qfs` \
             binary (the serve composition root) may consume the HTTP serving binding. If \
             something else depends on it, qfs-http's tokio listener no longer dead-ends in the \
             binary and the runtime-leaf exemption is unsound."
        );
    }

    // (c) qfs-http must NOT depend on qfs-runtime (it uses tokio for HTTP I/O, never the COMMIT
    // interpreter — the two impure stages stay separate, as for qfs-exec).
    assert!(
        !http_deps.iter().any(|d| d == "qfs-runtime"),
        "t32: qfs-http must NOT depend on qfs-runtime — its tokio is the HTTP I/O domain, not the \
         write/COMMIT interpreter. Deps were: {http_deps:?}"
    );
}

#[test]
fn mcp_binding_is_a_leaf_serve_consumer() {
    // t47 (the MCP serving binding topology): `qfs-mcp` is the leaf binding crate that exposes
    // qfs's four operating-loop tools (describe / preview / commit / connections) as a JSON-RPC /
    // MCP surface. The placement decision (the MCP sibling of the t32 qfs-http / t33 qfs-cron / t34
    // qfs-watchtower bindings) rests on three structural facts this guard pins so they cannot
    // silently invert:
    //
    //   (a) qfs-mcp consumes qfs-server (the policy gate — `gate_plan` / `resolve_policy` / `Policy`
    //       the `commit` tool routes through) AND qfs-exec (the `build_plan` / `plan_preview`
    //       evaluator the `preview`/`commit` tools shape results from) — the one crate that
    //       legitimately binds both for the MCP cause, the reason it is a NEW leaf rather than
    //       living in qfs-server (which must stay off qfs-exec and runtime-free, CO-t30-1) or
    //       qfs-cmd (off qfs-exec / the binding crates).
    //   (b) qfs-mcp is a LEAF: nothing depends on it except the terminal `qfs` binary (the serve
    //       composition root, which injects the live `McpEngine` and composes the pure `POST /mcp`
    //       handler into the qfs-http listener via a fallback closure). That keeps any I/O coupling
    //       dead-ended in the binary — the t28 runtime-leaf exemption precondition.
    //   (c) qfs-mcp does NOT depend on qfs-runtime. The real commit is the INJECTED
    //       `McpEngine::apply` closure the binary builds (the same runtime-backed `apply_plan` the
    //       CLI uses); the protocol core never drives the COMMIT interpreter, so the
    //       runtime-leaf-confinement guard is untouched (qfs-mcp never appears in its allowlist).
    //       Corollary: qfs-mcp carries no tokio of its own — the binding is a PURE synchronous
    //       handler composed into the listener (the watchtower-ingest pattern), so the async HTTP
    //       I/O still lives only in qfs-http + the binary.
    let graph = load_graph();

    // (a) qfs-mcp depends on both qfs-server and qfs-exec.
    let mcp_deps = graph
        .direct_deps
        .get("qfs-mcp")
        .expect("qfs-mcp is a workspace package");
    for required in ["qfs-server", "qfs-exec"] {
        assert!(
            mcp_deps.iter().any(|d| d == required),
            "t47: qfs-mcp must depend on {required} (it binds the policy gate to the build_plan/\
             preview evaluator for the MCP cause). Deps were: {mcp_deps:?}"
        );
    }

    // (b) qfs-mcp is a leaf: only the terminal `qfs` binary may depend on it.
    let mcp_consumers: Vec<&String> = graph
        .direct_deps
        .iter()
        .filter(|(pkg, deps)| pkg.as_str() != "qfs-mcp" && deps.iter().any(|d| d == "qfs-mcp"))
        .map(|(pkg, _)| pkg)
        .collect();
    for consumer in &mcp_consumers {
        assert_eq!(
            consumer.as_str(),
            "qfs",
            "t47 leaf violation: {consumer} depends on qfs-mcp, but only the terminal `qfs` \
             binary (the serve composition root) may consume the MCP serving binding. If something \
             else depends on it, the injected-apply / runtime-leaf exemption is no longer sound."
        );
    }

    // (c) qfs-mcp must NOT depend on qfs-runtime (the real applier is the injected `McpEngine::apply`
    // closure the composition root builds, not the COMMIT interpreter).
    assert!(
        !mcp_deps.iter().any(|d| d == "qfs-runtime"),
        "t47: qfs-mcp must NOT depend on qfs-runtime — the real commit path is the INJECTED \
         `McpEngine::apply` closure the composition root builds, not the COMMIT interpreter. Deps \
         were: {mcp_deps:?}"
    );
}

#[test]
fn core_depends_on_parser_one_directionally() {
    // C5 / E1 (ticket t06): the qfs-core -> qfs-parser edge is now WIRED — name
    // resolution (`qfs_core::resolve`) consumes the parsed `qfs_parser::Statement`. The
    // edge is one-directional, so the spine stays acyclic.
    let graph = load_graph();
    let core_deps = graph.direct_deps.get("qfs-core").expect("qfs-core package");
    assert!(
        core_deps.iter().any(|d| d == "qfs-parser"),
        "E1: qfs-core must depend on qfs-parser (name resolution consumes the AST, t06)"
    );
    // And the parser must never depend on core (cycle prevention).
    let parser_deps = graph
        .direct_deps
        .get("qfs-parser")
        .expect("qfs-parser package");
    assert!(
        !parser_deps.iter().any(|d| d == "qfs-core"),
        "qfs-parser must never depend on qfs-core (C5 cycle prevention)"
    );
}

#[test]
fn http_core_is_a_pure_leaf_single_sourcing_the_redaction_set() {
    // t19 refinement: qfs-http-core is the SINGLE SOURCE OF TRUTH for the pure HTTP exchange
    // DTOs (HttpMethod/HttpRequest/HttpResponse) and the header-redaction authority
    // (SENSITIVE_HEADERS / is_sensitive_header + the redacting Debug). Before it, both
    // qfs-driver-http and qfs-google-auth hand-copied those DTOs + the redaction set, and the
    // copies had already drifted — the risk being a token leak by drift (one side adds a
    // sensitive header, the other's copy lags and copies that header VALUE across the seam with
    // redaction silently missing it). Centralizing closes that hazard. This test pins three
    // structural facts that keep it closed:
    //
    //   (a) qfs-http-core is a PURE LEAF — among workspace crates it depends on qfs-secrets ONLY
    //       (for the one REDACTED marker), reaching no further than qfs-types. Crucially it
    //       carries NO reqwest/tokio/qfs-runtime, so depending on it does NOT pull either HTTP
    //       crate toward the runtime (the generic runtime-leaf confinement below stays intact).
    //   (b) BOTH HTTP crates depend on qfs-http-core — so neither carries a second DTO/redaction
    //       copy; the shared leaf is the only place HttpMethod/SENSITIVE_HEADERS are defined.
    //   (c) qfs-google-auth still does NOT depend on qfs-driver-http (the local HttpExchange seam
    //       is retained), so qfs-driver-http stays a leaf and tokio stays confined.
    let graph = load_graph();

    // (a) pure leaf: qfs-http-core's only workspace dep is qfs-secrets, and it carries no
    // runtime/http vendor crates.
    let http_core_deps = graph
        .direct_deps
        .get("qfs-http-core")
        .expect("qfs-http-core is a workspace package");
    let workspace_crates = [
        "qfs-cmd",
        "qfs-core",
        "qfs-server",
        "qfs-lang",
        "qfs-plan",
        "qfs-driver",
        "qfs-codec",
        "qfs-parser",
        "qfs-types",
        "qfs-runtime",
        "qfs-txn",
        "qfs-pushdown",
        "qfs-secrets",
        "qfs-driver-http",
        "qfs-google-auth",
    ];
    for d in http_core_deps
        .iter()
        .filter(|d| workspace_crates.contains(&d.as_str()))
    {
        assert_eq!(
            d, "qfs-secrets",
            "spine violation: qfs-http-core must depend only on qfs-secrets among workspace \
             crates (a pure leaf: owned DTOs + redaction, no driver/runtime coupling), but \
             depends on {d}"
        );
    }
    // It MUST NOT carry reqwest/tokio/qfs-runtime — that is what makes it safe for the
    // off-runtime qfs-google-auth to depend on it without becoming a runtime consumer.
    let forbidden_in_leaf = ["reqwest", "tokio", "qfs-runtime", "futures", "async-trait"];
    for f in forbidden_in_leaf {
        assert!(
            !http_core_deps.iter().any(|d| d == f),
            "purity violation: qfs-http-core must be a pure leaf with NO {f} (it must stay \
             runtime-free so qfs-google-auth can depend on it off the runtime). Deps were: \
             {http_core_deps:?}"
        );
    }

    // (b) both HTTP crates depend on the shared leaf — neither keeps a second DTO/redaction copy.
    for crate_name in ["qfs-driver-http", "qfs-google-auth"] {
        let deps = graph
            .direct_deps
            .get(crate_name)
            .unwrap_or_else(|| panic!("{crate_name} is a workspace package"));
        assert!(
            deps.iter().any(|d| d == "qfs-http-core"),
            "single-source violation: {crate_name} must depend on qfs-http-core for the shared \
             HTTP DTOs + redaction set (t19 refinement) rather than hand-copying them. Deps \
             were: {deps:?}"
        );
    }

    // (c) qfs-google-auth still does NOT depend on qfs-driver-http: the local HttpExchange seam is
    // retained, so qfs-driver-http stays a leaf and the runtime confinement is untouched.
    let google_auth_deps = graph
        .direct_deps
        .get("qfs-google-auth")
        .expect("qfs-google-auth is a workspace package");
    assert!(
        !google_auth_deps.iter().any(|d| d == "qfs-driver-http"),
        "confinement violation: qfs-google-auth must NOT depend on qfs-driver-http (which \
         depends on qfs-runtime). It must keep its local HttpExchange seam and share only the \
         pure qfs-http-core DTOs, so qfs-driver-http stays a runtime leaf. Deps were: \
         {google_auth_deps:?}"
    );
}

#[test]
fn crypto_core_is_a_pure_leaf_single_sourcing_the_three_vendored_copies() {
    // t34: qfs-crypto-core is the SINGLE SOURCE OF TRUTH for the dependency-free, wasm-clean
    // crypto primitives — SHA-256 (FIPS 180-4), HMAC-SHA256 (RFC 4231), lowercase-hex, and a
    // constant-time byte compare. Before it, an identical SHA-256 (and in two cases HMAC /
    // constant_time_eq) was independently vendored: qfs-driver-objstore::sha256 (SigV4) and
    // qfs-driver-slack::hmac (signature verification) — plus the retired qfs-cron::hash (run-id),
    // gone since t65 with the scheduler. No shared crypto leaf existed, and depending on any of
    // those crates would have pulled a runtime/binding coupling into the consumer, so each
    // re-vendored the routine. t34's webhook HMAC verification would have been another copy; instead
    // this crate single-sources all of them. It mirrors `http_core_is_a_pure_leaf_...` but stricter:
    //
    //   (a) qfs-crypto-core is a TRUE pure leaf — it depends on NOTHING (no workspace crate AND no
    //       vendor crate; std-only by construction). That maximal purity is what makes it safe for
    //       EVERY consumer, including the off-runtime watchtower webhook verifier and the wasm32
    //       Workers WEBHOOK ingress, to depend on it without inheriting any coupling. (Unlike
    //       qfs-http-core, which legitimately depends on qfs-secrets for the REDACTED marker, the
    //       crypto leaf needs no such marker — so its allowed dep set is EMPTY.)
    //   (b) the surviving former copy-holders all depend on the shared leaf now — so none keeps a
    //       second SHA-256/HMAC copy; the leaf is the only place they are defined. (The third
    //       former copy, qfs-cron's run-id hash, was deleted with the scheduler in t65.)
    let graph = load_graph();

    // (a) TRUE pure leaf: qfs-crypto-core has ZERO dependencies of any kind.
    let crypto_core_deps = graph
        .direct_deps
        .get("qfs-crypto-core")
        .expect("qfs-crypto-core is a workspace package");
    assert!(
        crypto_core_deps.is_empty(),
        "purity violation: qfs-crypto-core must be a TRUE pure leaf with ZERO dependencies \
         (no workspace crate, no vendor crate — std-only), so depending on it adds no coupling to \
         any consumer, including the off-runtime watchtower verifier and the wasm32 WEBHOOK \
         ingress. Deps were: {crypto_core_deps:?}"
    );

    // (b) the surviving former vendorings all depend on the shared leaf — single source, no copy.
    for crate_name in ["qfs-driver-objstore", "qfs-driver-slack"] {
        let deps = graph
            .direct_deps
            .get(crate_name)
            .unwrap_or_else(|| panic!("{crate_name} is a workspace package"));
        assert!(
            deps.iter().any(|d| d == "qfs-crypto-core"),
            "single-source violation: {crate_name} must depend on qfs-crypto-core for the shared \
             SHA-256/HMAC-SHA256/constant_time_eq (t34) rather than vendoring a private copy. \
             Deps were: {deps:?}"
        );
    }
}

#[test]
fn identity_is_a_pure_domain_leaf() {
    // t45: qfs-identity is the identity DOMAIN core (the `users`/`accounts` model, the consumer-side
    // IdentityStore trait, pure sign-up validation, argon2id hashing). It is a pure-ish leaf — its
    // SQLite I/O is INJECTED (the rusqlite `SqliteIdentityStore` lives in qfs-store), so the domain
    // stays off rusqlite/tokio and the lower spine. This guard pins three facts:
    //
    //   (a) qfs-identity's ONLY workspace deps are {qfs-secrets, qfs-crypto-core} — the redacting
    //       Secret wrapper + the constant-time compare. It does NOT depend on lang/plan/driver/codec/
    //       parser (it carries no query/engine logic) nor on qfs-store/rusqlite (I/O is injected).
    //   (b) it carries no tokio/async runtime (it is sync, pure domain logic).
    //   (c) qfs-store consumes it (the injected rusqlite store impl) — the acyclic edge that keeps
    //       the I/O out of the domain leaf.
    let graph = load_graph();

    let workspace_crates = [
        "qfs-cmd",
        "qfs-core",
        "qfs-server",
        "qfs-lang",
        "qfs-plan",
        "qfs-driver",
        "qfs-codec",
        "qfs-parser",
        "qfs-types",
        "qfs-runtime",
        "qfs-txn",
        "qfs-pushdown",
        "qfs-secrets",
        "qfs-crypto-core",
        "qfs-store",
        "qfs-identity",
    ];

    // (a) qfs-identity's workspace deps are exactly {qfs-secrets, qfs-crypto-core}.
    let identity_deps = graph
        .direct_deps
        .get("qfs-identity")
        .expect("qfs-identity is a workspace package");
    let allowed = ["qfs-secrets", "qfs-crypto-core"];
    for d in identity_deps
        .iter()
        .filter(|d| workspace_crates.contains(&d.as_str()))
    {
        assert!(
            allowed.contains(&d.as_str()),
            "confinement violation: qfs-identity must depend only on {allowed:?} among workspace \
             crates (it is a pure-ish domain leaf; SQLite I/O is injected via qfs-store), but \
             depends on {d}. Deps were: {identity_deps:?}"
        );
    }
    // The defining absences: no tokio/async runtime, no lower-spine / driver / query crates.
    for forbidden in [
        "tokio",
        "futures",
        "async-trait",
        "rusqlite",
        "qfs-lang",
        "qfs-plan",
        "qfs-driver",
        "qfs-codec",
        "qfs-parser",
        "qfs-runtime",
        "qfs-store",
    ] {
        assert!(
            !identity_deps.iter().any(|d| d == forbidden),
            "confinement violation: qfs-identity must NOT depend on {forbidden} (it is a pure-ish \
             leaf; tokio/rusqlite/query logic stay out). Deps were: {identity_deps:?}"
        );
    }

    // (c) qfs-store consumes qfs-identity (the injected rusqlite IdentityStore impl).
    let store_deps = graph
        .direct_deps
        .get("qfs-store")
        .expect("qfs-store is a workspace package");
    assert!(
        store_deps.iter().any(|d| d == "qfs-identity"),
        "qfs-store must depend on qfs-identity (it provides the injected rusqlite IdentityStore \
         impl over the System DB, t45). Deps were: {store_deps:?}"
    );
}

#[test]
fn session_is_a_pure_domain_leaf() {
    // t46: qfs-session is the session DOMAIN core (the `Session`/`SessionId` model + expiry, the
    // opaque `SessionToken` hashed at rest, the consumer-side `SessionStore` trait, pure cookie
    // format/parse). It is a pure-ish leaf — its SQLite I/O is INJECTED (the rusqlite
    // `SqliteSessionStore` lives in qfs-store) and its token entropy is injected from the binary — so
    // the domain stays off rusqlite/tokio/rand and the lower spine. This guard pins three facts:
    //
    //   (a) qfs-session's ONLY workspace deps are {qfs-identity, qfs-secrets, qfs-crypto-core} — the
    //       `UserId` a session belongs to + the redacting Secret + the at-rest hash / constant-time
    //       compare. It does NOT depend on lang/plan/driver/codec/parser nor on qfs-store/rusqlite.
    //   (b) it carries no tokio/async runtime AND no rand/getrandom (OS entropy is injected from the
    //       binary leaf, so the core stays deterministic/testable).
    //   (c) qfs-store consumes it (the injected rusqlite store impl) — the acyclic edge that keeps
    //       the I/O out of the domain leaf, mirroring the qfs-store -> qfs-identity edge.
    let graph = load_graph();

    let workspace_crates = [
        "qfs-cmd",
        "qfs-core",
        "qfs-server",
        "qfs-lang",
        "qfs-plan",
        "qfs-driver",
        "qfs-codec",
        "qfs-parser",
        "qfs-types",
        "qfs-runtime",
        "qfs-txn",
        "qfs-pushdown",
        "qfs-secrets",
        "qfs-crypto-core",
        "qfs-identity",
        "qfs-store",
        "qfs-session",
    ];

    // (a) qfs-session's workspace deps are exactly {qfs-identity, qfs-secrets, qfs-crypto-core}.
    let session_deps = graph
        .direct_deps
        .get("qfs-session")
        .expect("qfs-session is a workspace package");
    let allowed = ["qfs-identity", "qfs-secrets", "qfs-crypto-core"];
    for d in session_deps
        .iter()
        .filter(|d| workspace_crates.contains(&d.as_str()))
    {
        assert!(
            allowed.contains(&d.as_str()),
            "confinement violation: qfs-session must depend only on {allowed:?} among workspace \
             crates (it is a pure-ish domain leaf; SQLite I/O is injected via qfs-store, entropy via \
             the binary), but depends on {d}. Deps were: {session_deps:?}"
        );
    }
    // The defining absences: no tokio/async runtime, no CSPRNG, no rusqlite, no lower-spine crates.
    for forbidden in [
        "tokio",
        "futures",
        "async-trait",
        "rand",
        "getrandom",
        "rusqlite",
        "qfs-lang",
        "qfs-plan",
        "qfs-driver",
        "qfs-codec",
        "qfs-parser",
        "qfs-runtime",
        "qfs-store",
    ] {
        assert!(
            !session_deps.iter().any(|d| d == forbidden),
            "confinement violation: qfs-session must NOT depend on {forbidden} (it is a pure-ish \
             leaf; tokio/rand/rusqlite/query logic stay out — entropy is injected from the binary). \
             Deps were: {session_deps:?}"
        );
    }

    // (c) qfs-store consumes qfs-session (the injected rusqlite SessionStore impl).
    let store_deps = graph
        .direct_deps
        .get("qfs-store")
        .expect("qfs-store is a workspace package");
    assert!(
        store_deps.iter().any(|d| d == "qfs-session"),
        "qfs-store must depend on qfs-session (it provides the injected rusqlite SessionStore impl \
         over the System DB, t46). Deps were: {store_deps:?}"
    );
}

#[test]
fn oauth_is_a_pure_domain_leaf() {
    // t48: qfs-oauth is the OAuth authorization-server DOMAIN core (RFC 8414 AS-metadata + RFC 9728
    // PRM + JWKS document builders, and the ES256 JWS sign/verify primitives over the vetted
    // pure-Rust `p256` ECDSA). It is a pure-ish leaf — its key PERSISTENCE is INJECTED (the rusqlite
    // `OauthKeyStore` lives in qfs-store) and its key ENTROPY is injected from the binary — so the
    // domain stays off rusqlite/tokio/rand and the lower spine, mirroring qfs-identity (t45) /
    // qfs-session (t46). This guard pins two facts:
    //
    //   (a) qfs-oauth's ONLY workspace deps are {qfs-secrets, qfs-crypto-core} — the redacting
    //       `Secret` wrapper for the private key + the SHA-256 backing the RFC 7638 `kid` thumbprint.
    //       It does NOT depend on lang/plan/driver/codec/parser (no query/engine logic), on
    //       qfs-store/rusqlite (key I/O is injected), or on qfs-http/tokio (the async route serving is
    //       the binary's qfs-http listener via the Fallback seam).
    //   (b) it carries no tokio/async runtime and no rusqlite (it is sync, pure domain logic). NOTE:
    //       unlike t45/t46, qfs-store does NOT depend on qfs-oauth — the binary bridges qfs-oauth ↔
    //       the qfs-store `OauthKeyStore` (which trades only opaque bytes + the public JWK string), so
    //       no `qfs-store -> qfs-oauth` edge exists and none is asserted.
    let graph = load_graph();

    let workspace_crates = [
        "qfs-cmd",
        "qfs-core",
        "qfs-server",
        "qfs-lang",
        "qfs-plan",
        "qfs-driver",
        "qfs-codec",
        "qfs-parser",
        "qfs-types",
        "qfs-runtime",
        "qfs-txn",
        "qfs-pushdown",
        "qfs-secrets",
        "qfs-crypto-core",
        "qfs-store",
        "qfs-http",
        "qfs-oauth",
    ];

    // (a) qfs-oauth's workspace deps are exactly {qfs-secrets, qfs-crypto-core}.
    let oauth_deps = graph
        .direct_deps
        .get("qfs-oauth")
        .expect("qfs-oauth is a workspace package");
    let allowed = ["qfs-secrets", "qfs-crypto-core"];
    for d in oauth_deps
        .iter()
        .filter(|d| workspace_crates.contains(&d.as_str()))
    {
        assert!(
            allowed.contains(&d.as_str()),
            "confinement violation: qfs-oauth must depend only on {allowed:?} among workspace crates \
             (it is a pure-ish domain leaf; key I/O is injected via qfs-store, entropy + route serving \
             via the binary), but depends on {d}. Deps were: {oauth_deps:?}"
        );
    }
    // The defining absences: no tokio/async runtime, no rusqlite, no http listener, no lower-spine /
    // driver / query crates (per the ticket: oauth must NOT depend on tokio/lang/plan/driver/codec/
    // parser/runtime).
    for forbidden in [
        "tokio",
        "futures",
        "async-trait",
        "rusqlite",
        "qfs-store",
        "qfs-http",
        "qfs-lang",
        "qfs-plan",
        "qfs-driver",
        "qfs-codec",
        "qfs-parser",
        "qfs-runtime",
    ] {
        assert!(
            !oauth_deps.iter().any(|d| d == forbidden),
            "confinement violation: qfs-oauth must NOT depend on {forbidden} (it is a pure-ish leaf; \
             tokio/rusqlite/http/query logic stay out — key I/O is injected via qfs-store, route \
             serving via the binary's qfs-http listener). Deps were: {oauth_deps:?}"
        );
    }

    // (b) qfs-oauth is a leaf consumed only by the terminal `qfs` binary (the serve composition
    // root); no spine/lower crate may reach up into it.
    let oauth_consumers: Vec<&String> = graph
        .direct_deps
        .iter()
        .filter(|(pkg, deps)| pkg.as_str() != "qfs-oauth" && deps.iter().any(|d| d == "qfs-oauth"))
        .map(|(pkg, _)| pkg)
        .collect();
    for consumer in &oauth_consumers {
        assert_eq!(
            consumer.as_str(),
            "qfs",
            "t48 leaf violation: {consumer} depends on qfs-oauth, but only the terminal `qfs` binary \
             (the serve composition root) may consume the OAuth-AS domain leaf."
        );
    }
}

#[test]
fn tunnel_is_a_pure_protocol_leaf() {
    // t63 (the agent-fabric transport topology, roadmap M7 — decisions L + N): `qfs-tunnel` is the
    // PURE protocol core for the qfs-native outbound tunnel + relay (the frame codec, the relay
    // registration handshake, the in-memory request/response mux). The placement decision rests on
    // structural facts this guard pins so they cannot silently invert:
    //
    //   (a) qfs-tunnel's ONLY workspace deps are {qfs-http-core, qfs-secrets} — the shared HTTP DTOs
    //       a frame carries (so the SINGLE redaction authority is inherited, never re-implemented) +
    //       the REDACTED marker the handshake's `cloud_token` Debug emits. It does NOT depend on
    //       lang/plan/driver/codec/parser (no query/engine logic), on qfs-server/qfs-exec (it is NOT
    //       a serve binding — it provides transport, the binary dispatches inbound frames to the
    //       existing crates/http handler), or on qfs-runtime.
    //   (b) it carries NO tokio/futures/async-trait/reqwest — the protocol core is synchronous and
    //       runtime-free (mirroring qfs-google-auth's HttpExchange seam), so the LIVE outbound
    //       TCP/TLS dial half stays a DOCUMENTED SEAM in the terminal `qfs` binary where tokio
    //       dead-ends. This absence is WHY qfs-tunnel is NOT in the runtime-consumer allowlist.
    //   (c) qfs-tunnel is a LEAF: only the terminal `qfs` binary (the serve composition root) may
    //       consume it — the same role qfs-oauth / the binding leaves play.
    let graph = load_graph();

    let workspace_crates = [
        "qfs-cmd",
        "qfs-core",
        "qfs-server",
        "qfs-lang",
        "qfs-plan",
        "qfs-driver",
        "qfs-codec",
        "qfs-parser",
        "qfs-types",
        "qfs-runtime",
        "qfs-txn",
        "qfs-pushdown",
        "qfs-secrets",
        "qfs-http-core",
        "qfs-exec",
        "qfs-tunnel",
    ];

    // (a) qfs-tunnel's workspace deps are exactly {qfs-http-core, qfs-secrets}.
    let tunnel_deps = graph
        .direct_deps
        .get("qfs-tunnel")
        .expect("qfs-tunnel is a workspace package");
    let allowed = ["qfs-http-core", "qfs-secrets"];
    for d in tunnel_deps
        .iter()
        .filter(|d| workspace_crates.contains(&d.as_str()))
    {
        assert!(
            allowed.contains(&d.as_str()),
            "confinement violation: qfs-tunnel must depend only on {allowed:?} among workspace crates \
             (it is a pure protocol leaf; the live dial is the binary's documented seam), but depends \
             on {d}. Deps were: {tunnel_deps:?}"
        );
    }
    // The defining absences: no tokio/async runtime, no serve-binding deps, no lower-spine / driver /
    // query crates (the live TCP/TLS dial is a DOCUMENTED SEAM in the binary, not in the core).
    for forbidden in [
        "tokio",
        "futures",
        "async-trait",
        "reqwest",
        "qfs-runtime",
        "qfs-server",
        "qfs-exec",
        "qfs-lang",
        "qfs-plan",
        "qfs-driver",
        "qfs-codec",
        "qfs-parser",
    ] {
        assert!(
            !tunnel_deps.iter().any(|d| d == forbidden),
            "confinement violation: qfs-tunnel must NOT depend on {forbidden} (it is a pure, \
             runtime-free protocol leaf; the live outbound dial is the binary's documented seam where \
             tokio dead-ends). Deps were: {tunnel_deps:?}"
        );
    }

    // (b) it carries the shared redaction-inheriting DTOs (so a frame's Debug never leaks a token).
    assert!(
        tunnel_deps.iter().any(|d| d == "qfs-http-core"),
        "t63: qfs-tunnel must depend on qfs-http-core — a tunnel frame carries the shared \
         HttpRequest/HttpResponse so the single redaction authority is inherited. Deps were: \
         {tunnel_deps:?}"
    );

    // (c) qfs-tunnel is a leaf consumed only by the terminal `qfs` binary.
    let tunnel_consumers: Vec<&String> = graph
        .direct_deps
        .iter()
        .filter(|(pkg, deps)| {
            pkg.as_str() != "qfs-tunnel" && deps.iter().any(|d| d == "qfs-tunnel")
        })
        .map(|(pkg, _)| pkg)
        .collect();
    for consumer in &tunnel_consumers {
        assert_eq!(
            consumer.as_str(),
            "qfs",
            "t63 leaf violation: {consumer} depends on qfs-tunnel, but only the terminal `qfs` binary \
             (the serve composition root) may consume the tunnel protocol core. The live dial half is \
             the binary's documented seam where tokio dead-ends."
        );
    }
}

#[test]
fn host_is_the_only_deployment_seam_and_a_leaf() {
    // t36 (the deployment host-adapter topology): `qfs-host` is the ONE seam that abstracts what
    // causes a plan to run over the EC2 daemon + the CF Worker. The placement decision rests on
    // structural facts this guard pins so they cannot silently invert:
    //
    //   (a) qfs-host's NON-optional (wasm-clean core) workspace deps are EMPTY among workspace
    //       crates — the traits + owned DTOs + binding-set derivation + wrangler generator + the
    //       MockHost carry NO workspace coupling, so the core builds for wasm32-unknown-unknown.
    //       qfs-server is an OPTIONAL dep gated behind `host-daemon` (it pulls tokio `signal`,
    //       no-wasm) — the conversion SOURCE for the BindingSet, never re-exported. The
    //       load-bearing wasm fence is the ABSENCE of `host-daemon` (qfs-server is `optional`),
    //       not a marker (the t25/cron lesson).
    //   (b) qfs-host is a LEAF: nothing depends on it except the terminal `qfs` binary (the serve
    //       composition root). The daemon's TokioHost: RuntimeHost is composed in the binary,
    //       REUSING the existing qfs-http/qfs-cron/qfs-watchtower serve composition behind the
    //       trait — so any feature-gated coupling dead-ends in the binary, exactly as the t32/t33/
    //       t34 binding leaves do.
    //   (c) qfs-host does NOT depend on qfs-runtime (it never drives the COMMIT interpreter — the
    //       hosts attach causes that drive the SAME injected committers the binding leaves use), so
    //       the runtime-leaf-confinement guard is untouched (qfs-host never appears in its allowlist).
    let graph = load_graph();

    // (a) With `--no-deps`, optional deps still appear in the manifest dependency list; assert the
    // ONLY workspace dep qfs-host declares is qfs-server (the optional host-daemon conversion source).
    let workspace_crates = [
        "qfs-cmd",
        "qfs-core",
        "qfs-server",
        "qfs-lang",
        "qfs-plan",
        "qfs-driver",
        "qfs-codec",
        "qfs-parser",
        "qfs-types",
        "qfs-runtime",
        "qfs-txn",
        "qfs-pushdown",
        "qfs-secrets",
        "qfs-exec",
        "qfs-engine",
        "qfs-http",
        "qfs-watchtower",
        "qfs-host",
    ];
    let host_deps = graph
        .direct_deps
        .get("qfs-host")
        .expect("qfs-host is a workspace package");
    let allowed = ["qfs-server"];
    let mut ws_deps: Vec<&String> = host_deps
        .iter()
        .filter(|d| workspace_crates.contains(&d.as_str()))
        .collect();
    ws_deps.sort();
    ws_deps.dedup();
    for d in &ws_deps {
        assert!(
            allowed.contains(&d.as_str()),
            "topology violation: qfs-host must depend only on {allowed:?} among workspace crates \
             (the optional host-daemon BindingSet conversion source; the wasm-clean core has NO \
             workspace dep), but depends on {d}. Workspace deps were: {ws_deps:?}"
        );
    }

    // (b) qfs-host is a leaf: only the terminal `qfs` binary may depend on it.
    let host_consumers: Vec<&String> = graph
        .direct_deps
        .iter()
        .filter(|(pkg, deps)| pkg.as_str() != "qfs-host" && deps.iter().any(|d| d == "qfs-host"))
        .map(|(pkg, _)| pkg)
        .collect();
    for consumer in &host_consumers {
        assert_eq!(
            consumer.as_str(),
            "qfs",
            "t36 leaf violation: {consumer} depends on qfs-host, but only the terminal `qfs` \
             binary (the serve composition root) may compose the deployment host. qfs-host is the \
             one deployment seam; a non-terminal consumer would let a host's feature-gated \
             coupling escape the binary."
        );
    }

    // (c) qfs-host must NOT depend on qfs-runtime (it never drives the COMMIT interpreter — the
    // hosts cause the SAME injected committers to run; the two impure stages stay separate).
    assert!(
        !host_deps.iter().any(|d| d == "qfs-runtime"),
        "t36: qfs-host must NOT depend on qfs-runtime — a host CAUSES a plan to run through the \
         existing injected committers, it does not drive the COMMIT interpreter. Deps were: \
         {host_deps:?}"
    );

    // (d) qfs-host must NOT carry the `worker` or `hyper` vendor crates: the wasm-clean core is
    // vendor-free, and the CF `worker` entrypoints are PARKED (ADR-0005, not in the offline cache).
    for forbidden in ["worker", "hyper", "axum"] {
        assert!(
            !host_deps.iter().any(|d| d == forbidden),
            "t36: qfs-host must NOT depend on the vendor crate {forbidden} (the wasm-clean core is \
             vendor-free; the CF `worker` entrypoints are parked per ADR-0005). Deps were: {host_deps:?}"
        );
    }
}
