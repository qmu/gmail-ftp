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
fn binary_depends_only_on_cmd_among_workspace_crates() {
    let graph = load_graph();
    let bin_deps = graph.direct_deps.get("cfs").expect("cfs binary package");
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
    let ws_deps: Vec<&String> = bin_deps
        .iter()
        .filter(|d| workspace_crates.contains(&d.as_str()))
        .collect();
    assert_eq!(
        ws_deps,
        vec![&"cfs-cmd".to_string()],
        "the cfs binary must depend on cfs-cmd only (thin entrypoint)"
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
fn core_does_not_depend_on_parser_yet_but_reserves_the_edge() {
    // C5: the cfs-core -> cfs-parser edge is reserved (declared in docs/Cargo
    // comment) but NOT yet wired in E0, so E1 introduces it one-directionally.
    let graph = load_graph();
    let core_deps = graph.direct_deps.get("cfs-core").expect("cfs-core package");
    assert!(
        !core_deps.iter().any(|d| d == "cfs-parser"),
        "at E0 cfs-core must NOT yet depend on cfs-parser (edge reserved for E1, C5)"
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
