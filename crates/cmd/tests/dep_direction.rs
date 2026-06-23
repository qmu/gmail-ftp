//! Acceptance criterion **C4** (fidelity guard G5): mechanically enforce that
//! `cfs-cmd` holds no domain logic by forbidding a direct dependency on any of the
//! lower domain crates. `cfs-cmd` may depend on `cfs-core` (the hub) and
//! `cfs-server` (the serve arm) only.
//!
//! This is an integration test that shells out to `cargo metadata`, inspects the
//! resolved dependency graph, and fails the build if `cfs-cmd` gains a direct edge
//! to `cfs-lang` / `cfs-plan` / `cfs-driver` / `cfs-codec` / `cfs-parser`. It also
//! asserts the broader acyclic spine (nothing depends on `cfs-cmd`, and the leaf
//! edges go the intended way).
//!
//! `cargo` is invoked via the `CARGO` env var cargo sets for integration tests, so
//! no PATH assumptions are made.

// Test code: assertions and setup may panic/expect/unwrap freely.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;
use std::process::Command;

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
        .get("cfs-cmd")
        .expect("cfs-cmd is a workspace package");

    let forbidden = [
        "cfs-lang",
        "cfs-plan",
        "cfs-driver",
        "cfs-codec",
        "cfs-parser",
    ];
    for f in forbidden {
        assert!(
            !cmd_deps.iter().any(|d| d == f),
            "C4 violation: cfs-cmd must NOT depend directly on {f} \
             (it must route through cfs-core). Direct deps were: {cmd_deps:?}"
        );
    }

    // It must depend on the hub + the serve arm.
    assert!(
        cmd_deps.iter().any(|d| d == "cfs-core"),
        "cfs-cmd must depend on cfs-core"
    );
    assert!(
        cmd_deps.iter().any(|d| d == "cfs-server"),
        "cfs-cmd must depend on cfs-server"
    );
}

#[test]
fn nothing_depends_on_cmd() {
    let graph = load_graph();
    for (pkg, deps) in &graph.direct_deps {
        if pkg == "cfs" {
            // The binary crate is the only thing allowed to depend on cfs-cmd.
            continue;
        }
        assert!(
            !deps.iter().any(|d| d == "cfs-cmd"),
            "spine violation: {pkg} depends on cfs-cmd; only the `cfs` binary may"
        );
    }
}

#[test]
fn nothing_depends_on_the_cfs_binary_so_it_is_a_terminal_sink() {
    // t28 (C1): the `cfs` BINARY is the workspace's terminal sink — NOTHING depends on it. This
    // is the property the two t28 guard relaxations RELY ON, so we assert it explicitly (fail
    // closed) rather than leaving it implicit:
    //
    //   * `runtime_is_confined_to_plan_and_types` exempts the binary as a permitted dependent of
    //     a `cfs-runtime` consumer (`cfs -> cfs-driver-local -> cfs-runtime`). That exemption is
    //     ONLY safe because tokio dead-ends in the binary: a runtime consumer must be a leaf, and
    //     the binary is the leaf that consumes it. If something ever depended on the binary,
    //     tokio could transit THROUGH it back into the spine, and the exemption would be unsound.
    //   * `binary_is_the_thin_entrypoint_plus_the_t28_shell_composition_root` lets the binary
    //     reach UP into cfs-exec / cfs-core / cfs-driver-local (the shell composition root). That
    //     is a layer inversion ONLY if the binary is itself depended upon; as a terminal sink it
    //     is the composition root, which is allowed to reach up.
    //
    // So this test is the load-bearing precondition of BOTH relaxations: it converts "the binary
    // is a sink" from an assumption into a mechanically enforced invariant. NOTE: a Cargo
    // `[[bin]]` package exposes no lib target, so a reverse-dep on it is not even expressible in
    // a Cargo.toml today — making a real violation hard to construct. We assert the property
    // anyway as fail-closed documentation: should the binary ever gain a lib target (or a future
    // crate find a way to depend on it), this guard fires immediately.
    let graph = load_graph();
    for (pkg, deps) in &graph.direct_deps {
        if pkg == "cfs" {
            continue;
        }
        assert!(
            !deps.iter().any(|d| d == "cfs"),
            "terminal-sink violation: {pkg} depends on the `cfs` binary. The binary MUST remain a \
             terminal sink (nothing depends on it) — that is the precondition that makes the t28 \
             runtime-leaf exemption sound (tokio dead-ends in the binary) and the binary's \
             reach-up into cfs-exec/cfs-core a composition root rather than a layer inversion. If \
             this fires, the two t28 guard relaxations are no longer safe and must be revisited."
        );
    }
}

#[test]
fn binary_is_the_thin_entrypoint_plus_the_t28_shell_composition_root() {
    // The `cfs` binary forwards argv to `cfs-cmd` and, since t28, ALSO hosts the interactive
    // shell's composition root: the runtime-coupled local read facet (`cfs-driver-local`) and the
    // registry wiring it injects into `cfs-cmd` via the `ShellLauncher`. That adapter cannot live
    // in cfs-cmd (a `cfs-cmd → cfs-driver-local` edge would make cfs-cmd a non-leaf runtime
    // consumer, tripping the runtime-confinement guard) nor in cfs-exec (confined off the driver
    // crates), so it lives in the binary — the leaf sink where tokio dead-ends. We therefore pin
    // the binary's workspace deps to an EXACT allowed set (cfs-cmd + the shell-composition crates),
    // so an UNINTENDED new binary dep still fails, while the deliberate t28 set is permitted.
    let graph = load_graph();
    let bin_deps = graph.direct_deps.get("cfs").expect("cfs binary package");
    let lower_spine = [
        "cfs-server",
        "cfs-lang",
        "cfs-plan",
        "cfs-driver",
        "cfs-codec",
        "cfs-parser",
        "cfs-types",
        "cfs-runtime",
    ];
    // The binary must NOT reach directly into the lower spine / the runtime: it composes only
    // through cfs-cmd, cfs-exec (the integration layer's read seam), cfs-core (Engine), and the
    // concrete leaf driver it wires (cfs-driver-local) + cfs-pushdown (ScanNode for the adapter).
    for f in lower_spine {
        assert!(
            !bin_deps.iter().any(|d| d == f),
            "spine violation: the cfs binary must not depend directly on {f}; it composes the \
             shell through cfs-cmd / cfs-exec / cfs-core / cfs-driver-local only. Deps: {bin_deps:?}"
        );
    }
    let allowed = [
        "cfs-cmd",
        "cfs-core",
        "cfs-exec",
        "cfs-driver-local",
        "cfs-pushdown",
    ];
    let workspace_prefixed: Vec<&String> =
        bin_deps.iter().filter(|d| d.starts_with("cfs")).collect();
    for d in &workspace_prefixed {
        assert!(
            allowed.contains(&d.as_str()),
            "the cfs binary gained an unexpected workspace dep {d} (allowed: {allowed:?}). If this \
             is intended shell-composition wiring, extend the allowlist; otherwise route it through \
             cfs-cmd. Deps: {bin_deps:?}"
        );
    }
    // It must still depend on cfs-cmd (the dispatch front door).
    assert!(
        bin_deps.iter().any(|d| d == "cfs-cmd"),
        "the cfs binary must still depend on cfs-cmd (the argv dispatch front door)"
    );
}

#[test]
fn types_is_a_leaf_and_codec_depends_on_it() {
    // t05: cfs-types is the canonical type/schema model. It must be a true leaf —
    // it depends on NO other workspace crate (keeping the spine acyclic and the type
    // model vendor-free). cfs-codec and cfs-core depend ON it for the row model.
    let graph = load_graph();
    let workspace_crates = [
        "cfs-cmd",
        "cfs-core",
        "cfs-server",
        "cfs-lang",
        "cfs-plan",
        "cfs-driver",
        "cfs-codec",
        "cfs-parser",
        "cfs-types",
    ];
    let types_deps = graph
        .direct_deps
        .get("cfs-types")
        .expect("cfs-types package");
    for ws in workspace_crates {
        assert!(
            !types_deps.iter().any(|d| d == ws),
            "spine violation: cfs-types must be a leaf but depends on {ws}"
        );
    }

    // The canonical row model flows up: codec and core depend on cfs-types.
    let codec_deps = graph
        .direct_deps
        .get("cfs-codec")
        .expect("cfs-codec package");
    assert!(
        codec_deps.iter().any(|d| d == "cfs-types"),
        "cfs-codec must depend on cfs-types for the canonical row model (t05)"
    );
    let core_deps = graph.direct_deps.get("cfs-core").expect("cfs-core package");
    assert!(
        core_deps.iter().any(|d| d == "cfs-types"),
        "cfs-core must depend on cfs-types to re-export the type model (t05)"
    );

    // t13: the Driver contract's `describe` returns the canonical typed
    // `cfs_types::Schema` (archetype tag + Schema), so cfs-driver depends DIRECTLY on
    // the cfs-types leaf. This is the reconciliation of the old untyped NodeSchema into
    // the one workspace schema; the edge is acyclic because cfs-types is a leaf
    // (cfs-driver → { cfs-plan, cfs-types } → cfs-types).
    let driver_deps = graph
        .direct_deps
        .get("cfs-driver")
        .expect("cfs-driver package");
    assert!(
        driver_deps.iter().any(|d| d == "cfs-types"),
        "cfs-driver must depend on cfs-types for the typed Schema in the Driver contract (t13)"
    );
    assert!(
        driver_deps.iter().any(|d| d == "cfs-plan"),
        "cfs-driver must depend on cfs-plan for the PlanApplier/Plan effect seam (t09/t13)"
    );
}

#[test]
fn runtime_is_confined_to_plan_and_types() {
    // t10 (O3): mechanically lock the tokio confinement. `cfs-runtime` is the sole impure
    // stage (RFD §3/§6 COMMIT); tokio/futures live there and MUST NOT leak into the spine.
    // This is the structural counterpart of `cfs-plan`'s purity test: assert two directions —
    //
    //   (a) `cfs-runtime` depends, among workspace crates, ONLY on `{cfs-plan, cfs-types,
    //       cfs-txn}` (no `cfs-core`/`cfs-parser`/`cfs-driver`/`cfs-codec`/`cfs-lang`/
    //       `cfs-cmd`/`cfs-server`), so the runtime walks the effect plan + the pure
    //       transactional envelope and nothing else; and
    //   (b) NONE of the **pure-spine** crates depends back up onto `cfs-runtime`, so tokio can
    //       never enter the spine's closure via this edge (the confinement that keeps
    //       `cfs-plan` I/O-free and its purity dep-closure test green by construction). A
    //       concrete **driver-impl** crate (t16 `cfs-driver-local`) and the top-level binary
    //       ARE permitted to depend on `cfs-runtime`: they are leaf consumers that bridge a
    //       driver's synchronous `PlanApplier` to the async `ApplyDriver` and register it in
    //       the `DriverRegistry`. Nothing depends back onto *them*, so tokio still cannot
    //       reach the spine — the edge only flows up out of the runtime into a leaf, never
    //       into `cfs-plan`/`cfs-types`/`cfs-driver`/`cfs-codec`/`cfs-txn`/… .
    //
    // t11 added `cfs-txn` (the transactional correctness envelope). It is ITSELF pure
    // orchestration confined to `{cfs-plan, cfs-types}` (no tokio of its own — the runtime
    // bridges its async ApplyDriver to cfs-txn's synchronous LegApplier seam), so admitting
    // the `cfs-runtime → cfs-txn` edge does not widen tokio's reach: cfs-txn carries no
    // async runtime into the spine. We assert that confinement too.
    let graph = load_graph();

    let workspace_crates = [
        "cfs-cmd",
        "cfs-core",
        "cfs-server",
        "cfs-lang",
        "cfs-plan",
        "cfs-driver",
        "cfs-codec",
        "cfs-parser",
        "cfs-types",
        "cfs-runtime",
        "cfs-txn",
    ];

    // (a) runtime's workspace deps are exactly the allowed leaf set (plan/types + the pure
    // cfs-txn envelope).
    let runtime_deps = graph
        .direct_deps
        .get("cfs-runtime")
        .expect("cfs-runtime is a workspace package");
    let allowed = ["cfs-plan", "cfs-types", "cfs-txn"];
    let mut ws_deps: Vec<&String> = runtime_deps
        .iter()
        .filter(|d| workspace_crates.contains(&d.as_str()))
        .collect();
    ws_deps.sort();
    ws_deps.dedup();
    for d in &ws_deps {
        assert!(
            allowed.contains(&d.as_str()),
            "confinement violation: cfs-runtime must depend only on {allowed:?} among \
             workspace crates, but depends on {d} (this would pull tokio toward the spine). \
             Workspace deps were: {ws_deps:?}"
        );
    }

    // (a') cfs-txn is itself confined to {cfs-plan, cfs-types} — it carries no tokio/async
    // runtime, so the runtime → txn edge does not widen the impure surface.
    let txn_deps = graph
        .direct_deps
        .get("cfs-txn")
        .expect("cfs-txn is a workspace package");
    let txn_allowed = ["cfs-plan", "cfs-types"];
    for d in txn_deps
        .iter()
        .filter(|d| workspace_crates.contains(&d.as_str()))
    {
        assert!(
            txn_allowed.contains(&d.as_str()),
            "confinement violation: cfs-txn must stay pure orchestration over \
             {txn_allowed:?} among workspace crates, but depends on {d}"
        );
    }

    // (b) GENERIC LEAF CONFINEMENT — the durable invariant that scales to 11 more driver
    // crates with NO per-driver edit. Every crate that depends on `cfs-runtime` (other than
    // `cfs-runtime` itself) MUST be a leaf — no other workspace crate may depend back onto it.
    // This encodes *why* a runtime consumer is safe (tokio dead-ends in a leaf and cannot
    // transit back into the spine) rather than *which* crates we waved through. A non-leaf
    // gaining the `→ cfs-runtime` edge (e.g. someone makes `cfs-core` depend on the runtime)
    // fails automatically because `cfs-core` is a sink for the rest of the workspace. A new
    // `cfs-driver-s3`/`-drive`/`-gmail` needs no edit here: as long as it is a leaf, the edge
    // is admitted; the moment something depends back onto it, the leaf check fires.
    let runtime_consumers: Vec<&String> = graph
        .direct_deps
        .iter()
        .filter(|(pkg, deps)| {
            pkg.as_str() != "cfs-runtime" && deps.iter().any(|d| d == "cfs-runtime")
        })
        .map(|(pkg, _)| pkg)
        .collect();
    assert!(
        !runtime_consumers.is_empty(),
        "expected at least one cfs-runtime consumer (the bridging driver-impl / binary); \
         found none — the metadata view is likely wrong"
    );
    for consumer in &runtime_consumers {
        let dependent = graph
            .direct_deps
            .iter()
            .find(|(other, od)| {
                other.as_str() != consumer.as_str()
                    && od.iter().any(|d| d == *consumer)
                    // The `cfs` BINARY is the workspace's composition root — a true sink that
                    // nothing depends on. Since t28 it wires the local read facet
                    // (cfs-driver-local, a runtime consumer) into the shell, so it depends on a
                    // runtime consumer. This does NOT widen tokio's reach: the binary is the
                    // terminal node, so tokio still dead-ends (it cannot transit THROUGH the
                    // binary back into the spine — nothing depends on the binary). Exempt it.
                    && other.as_str() != "cfs"
            })
            .map(|(other, _)| other.clone());
        assert!(
            dependent.is_none(),
            "confinement violation: {consumer} depends on cfs-runtime but is NOT a leaf — \
             {dependent:?} depends back onto it, so tokio could transit out of the runtime, \
             through {consumer}, and back into the spine. A cfs-runtime consumer MUST be a \
             leaf (no workspace crate other than the terminal `cfs` binary may depend onto it)."
        );
    }

    // (b') Belt-and-suspenders: the named allowlist pins *identity* — the exact leaves we
    // expect to bridge into the runtime today — so an UNINTENDED new runtime consumer is
    // caught even if it happens to be a leaf at the moment it is added. The generic leaf
    // check above (b) pins *safety*; this allowlist pins *intent*. A new driver crate appends
    // its name here (a one-line, reviewable signal), and (b) guarantees the append was safe.
    let runtime_consumers_allowed = [
        "cfs-driver-local",
        "cfs-driver-http",
        "cfs-driver-gmail",
        "cfs-driver-gdrive",
        "cfs-driver-ga",
        "cfs-driver-sql",
        "cfs-driver-cf",
        "cfs-driver-objstore",
        "cfs-driver-github",
        "cfs-driver-slack",
        "cfs-driver-git",
        "cfs",
    ];
    for consumer in &runtime_consumers {
        assert!(
            runtime_consumers_allowed.contains(&consumer.as_str()),
            "confinement violation: {consumer} depends on cfs-runtime but is not in the \
             expected runtime-consumer allowlist ({runtime_consumers_allowed:?}). If this is a \
             new driver-impl leaf bridging an ApplyDriver, add it to the allowlist; otherwise \
             tokio must stay confined to cfs-runtime + leaf driver-impl consumers."
        );
    }
}

#[test]
fn secrets_is_confined_to_types_and_core_consumes_it() {
    // t27: cfs-secrets is the credential / secret store + multi-account resolution
    // (RFD §10). It is consumer-side, owned-DTO only, and reuses the canonical
    // `cfs_types::DriverId` — so among workspace crates it depends ONLY on cfs-types
    // (a leaf), keeping the spine acyclic (cfs-secrets → cfs-types). cfs-core consumes
    // it (the Engine threads a `Secrets` handle into the driver-bind context) and
    // re-exports it, so the rest of the workspace reaches secrets through the hub.
    let graph = load_graph();
    let workspace_crates = [
        "cfs-cmd",
        "cfs-core",
        "cfs-server",
        "cfs-lang",
        "cfs-plan",
        "cfs-driver",
        "cfs-codec",
        "cfs-parser",
        "cfs-types",
        "cfs-runtime",
        "cfs-txn",
        "cfs-pushdown",
        "cfs-secrets",
    ];

    // (a) secrets' only workspace dependency is cfs-types.
    let secrets_deps = graph
        .direct_deps
        .get("cfs-secrets")
        .expect("cfs-secrets is a workspace package");
    for d in secrets_deps
        .iter()
        .filter(|d| workspace_crates.contains(&d.as_str()))
    {
        assert_eq!(
            d, "cfs-types",
            "spine violation: cfs-secrets must depend only on cfs-types among workspace \
             crates (it carries no driver/plan/vendor coupling), but depends on {d}"
        );
    }

    // (b) core consumes secrets (the bind-context credential surface).
    let core_deps = graph.direct_deps.get("cfs-core").expect("cfs-core package");
    assert!(
        core_deps.iter().any(|d| d == "cfs-secrets"),
        "cfs-core must depend on cfs-secrets to thread the Secrets handle into the Engine (t27)"
    );

    // (c) nothing depends back up onto cfs-cmd via secrets, and the spine stays acyclic:
    // cfs-secrets must NOT depend on any higher crate (already covered by (a)).
}

#[test]
fn exec_is_confined_above_the_spine_and_off_the_runtime() {
    // t29 (CO-t29-4): cfs-exec is the execution / integration layer that composes the SELECT
    // read-path executor (parse -> resolve -> plan -> driver scan -> engine residual -> rows)
    // ABOVE the spine. The t29 topology ruling rests on two structural facts this guard pins
    // mechanically (so the six E7 server crates about to land cannot silently invert it):
    //
    //   (a) cfs-exec's workspace-internal deps are EXACTLY the above-spine set
    //       {cfs-core, cfs-parser, cfs-pushdown, cfs-engine}. In particular it does NOT depend
    //       on cfs-runtime — that absence is what keeps the two impure stages separate (the
    //       runtime owns writes/COMMIT; cfs-exec owns reads/scans via its own ReadDriver seam).
    //       Were cfs-exec to gain a cfs-runtime edge, it would become a runtime consumer and the
    //       runtime leaf-confinement check would (correctly) fire — this assertion catches it
    //       one step earlier with a precise message.
    //   (b) NO spine / lower crate depends back onto cfs-exec — only cfs-cmd (and transitively
    //       the `cfs` binary) may consume it. A future `cfs-core -> cfs-exec` (or any spine ->
    //       cfs-exec) layer inversion fails here, since the spine must never reach UP into the
    //       integration layer.
    let graph = load_graph();

    let workspace_crates = [
        "cfs-cmd",
        "cfs-core",
        "cfs-server",
        "cfs-lang",
        "cfs-plan",
        "cfs-driver",
        "cfs-codec",
        "cfs-parser",
        "cfs-types",
        "cfs-runtime",
        "cfs-txn",
        "cfs-pushdown",
        "cfs-secrets",
        "cfs-http-core",
        "cfs-engine",
        "cfs-exec",
    ];

    // (a) cfs-exec's workspace deps are exactly the above-spine set (no cfs-runtime).
    let exec_deps = graph
        .direct_deps
        .get("cfs-exec")
        .expect("cfs-exec is a workspace package");
    let allowed = ["cfs-core", "cfs-parser", "cfs-pushdown", "cfs-engine"];
    let mut ws_deps: Vec<&String> = exec_deps
        .iter()
        .filter(|d| workspace_crates.contains(&d.as_str()))
        .collect();
    ws_deps.sort();
    ws_deps.dedup();
    for d in &ws_deps {
        assert!(
            allowed.contains(&d.as_str()),
            "topology violation: cfs-exec must depend only on the above-spine set {allowed:?} \
             among workspace crates, but depends on {d}. Workspace deps were: {ws_deps:?}"
        );
    }
    // The defining absence: cfs-exec must NOT consume the runtime (keeps the two impure stages —
    // runtime writes/COMMIT vs. exec reads/scans — structurally separate).
    assert!(
        !exec_deps.iter().any(|d| d == "cfs-runtime"),
        "topology violation: cfs-exec must NOT depend on cfs-runtime — that absence is what keeps \
         the read executor a separate impure stage from the runtime's write/COMMIT stage (t29). \
         A real read driver implements cfs-exec's own ReadDriver seam, never the runtime's."
    );
    // And the four above-spine deps must all be present (the executor genuinely composes them).
    for required in allowed {
        assert!(
            exec_deps.iter().any(|d| d == required),
            "cfs-exec must depend on {required} (the read executor composes the above-spine set)"
        );
    }

    // (b) Only cfs-cmd and the terminal `cfs` binary may depend on cfs-exec — no spine/lower
    // crate may reach UP into it. Since t28 the binary consumes cfs-exec directly too: it hosts
    // the interactive shell's composition root (the local ReadDriver adapter + the
    // `Session`/`VfsPath`/`ReadRegistry` wiring) and injects the wired shell into cfs-cmd via the
    // `ShellLauncher`. The binary is a terminal sink (nothing depends on it), so this is not a
    // layer inversion — it is the composition root consuming the integration layer, exactly as
    // cfs-cmd does. A SPINE/LOWER crate reaching up into cfs-exec still fails here.
    let exec_consumers: Vec<&String> = graph
        .direct_deps
        .iter()
        .filter(|(pkg, deps)| pkg.as_str() != "cfs-exec" && deps.iter().any(|d| d == "cfs-exec"))
        .map(|(pkg, _)| pkg)
        .collect();
    let allowed_exec_consumers = ["cfs-cmd", "cfs"];
    for consumer in &exec_consumers {
        assert!(
            allowed_exec_consumers.contains(&consumer.as_str()),
            "topology violation: {consumer} depends on cfs-exec, but only cfs-cmd and the terminal \
             `cfs` binary (the t28 shell composition root) may consume the integration layer. A \
             spine/lower crate reaching UP into cfs-exec is a layer inversion (t29)."
        );
    }
}

#[test]
fn core_depends_on_parser_one_directionally() {
    // C5 / E1 (ticket t06): the cfs-core -> cfs-parser edge is now WIRED — name
    // resolution (`cfs_core::resolve`) consumes the parsed `cfs_parser::Statement`. The
    // edge is one-directional, so the spine stays acyclic.
    let graph = load_graph();
    let core_deps = graph.direct_deps.get("cfs-core").expect("cfs-core package");
    assert!(
        core_deps.iter().any(|d| d == "cfs-parser"),
        "E1: cfs-core must depend on cfs-parser (name resolution consumes the AST, t06)"
    );
    // And the parser must never depend on core (cycle prevention).
    let parser_deps = graph
        .direct_deps
        .get("cfs-parser")
        .expect("cfs-parser package");
    assert!(
        !parser_deps.iter().any(|d| d == "cfs-core"),
        "cfs-parser must never depend on cfs-core (C5 cycle prevention)"
    );
}

#[test]
fn http_core_is_a_pure_leaf_single_sourcing_the_redaction_set() {
    // t19 refinement: cfs-http-core is the SINGLE SOURCE OF TRUTH for the pure HTTP exchange
    // DTOs (HttpMethod/HttpRequest/HttpResponse) and the header-redaction authority
    // (SENSITIVE_HEADERS / is_sensitive_header + the redacting Debug). Before it, both
    // cfs-driver-http and cfs-google-auth hand-copied those DTOs + the redaction set, and the
    // copies had already drifted — the risk being a token leak by drift (one side adds a
    // sensitive header, the other's copy lags and copies that header VALUE across the seam with
    // redaction silently missing it). Centralizing closes that hazard. This test pins three
    // structural facts that keep it closed:
    //
    //   (a) cfs-http-core is a PURE LEAF — among workspace crates it depends on cfs-secrets ONLY
    //       (for the one REDACTED marker), reaching no further than cfs-types. Crucially it
    //       carries NO reqwest/tokio/cfs-runtime, so depending on it does NOT pull either HTTP
    //       crate toward the runtime (the generic runtime-leaf confinement below stays intact).
    //   (b) BOTH HTTP crates depend on cfs-http-core — so neither carries a second DTO/redaction
    //       copy; the shared leaf is the only place HttpMethod/SENSITIVE_HEADERS are defined.
    //   (c) cfs-google-auth still does NOT depend on cfs-driver-http (the local HttpExchange seam
    //       is retained), so cfs-driver-http stays a leaf and tokio stays confined.
    let graph = load_graph();

    // (a) pure leaf: cfs-http-core's only workspace dep is cfs-secrets, and it carries no
    // runtime/http vendor crates.
    let http_core_deps = graph
        .direct_deps
        .get("cfs-http-core")
        .expect("cfs-http-core is a workspace package");
    let workspace_crates = [
        "cfs-cmd",
        "cfs-core",
        "cfs-server",
        "cfs-lang",
        "cfs-plan",
        "cfs-driver",
        "cfs-codec",
        "cfs-parser",
        "cfs-types",
        "cfs-runtime",
        "cfs-txn",
        "cfs-pushdown",
        "cfs-secrets",
        "cfs-driver-http",
        "cfs-google-auth",
    ];
    for d in http_core_deps
        .iter()
        .filter(|d| workspace_crates.contains(&d.as_str()))
    {
        assert_eq!(
            d, "cfs-secrets",
            "spine violation: cfs-http-core must depend only on cfs-secrets among workspace \
             crates (a pure leaf: owned DTOs + redaction, no driver/runtime coupling), but \
             depends on {d}"
        );
    }
    // It MUST NOT carry reqwest/tokio/cfs-runtime — that is what makes it safe for the
    // off-runtime cfs-google-auth to depend on it without becoming a runtime consumer.
    let forbidden_in_leaf = ["reqwest", "tokio", "cfs-runtime", "futures", "async-trait"];
    for f in forbidden_in_leaf {
        assert!(
            !http_core_deps.iter().any(|d| d == f),
            "purity violation: cfs-http-core must be a pure leaf with NO {f} (it must stay \
             runtime-free so cfs-google-auth can depend on it off the runtime). Deps were: \
             {http_core_deps:?}"
        );
    }

    // (b) both HTTP crates depend on the shared leaf — neither keeps a second DTO/redaction copy.
    for crate_name in ["cfs-driver-http", "cfs-google-auth"] {
        let deps = graph
            .direct_deps
            .get(crate_name)
            .unwrap_or_else(|| panic!("{crate_name} is a workspace package"));
        assert!(
            deps.iter().any(|d| d == "cfs-http-core"),
            "single-source violation: {crate_name} must depend on cfs-http-core for the shared \
             HTTP DTOs + redaction set (t19 refinement) rather than hand-copying them. Deps \
             were: {deps:?}"
        );
    }

    // (c) cfs-google-auth still does NOT depend on cfs-driver-http: the local HttpExchange seam is
    // retained, so cfs-driver-http stays a leaf and the runtime confinement is untouched.
    let google_auth_deps = graph
        .direct_deps
        .get("cfs-google-auth")
        .expect("cfs-google-auth is a workspace package");
    assert!(
        !google_auth_deps.iter().any(|d| d == "cfs-driver-http"),
        "confinement violation: cfs-google-auth must NOT depend on cfs-driver-http (which \
         depends on cfs-runtime). It must keep its local HttpExchange seam and share only the \
         pure cfs-http-core DTOs, so cfs-driver-http stays a runtime leaf. Deps were: \
         {google_auth_deps:?}"
    );
}
