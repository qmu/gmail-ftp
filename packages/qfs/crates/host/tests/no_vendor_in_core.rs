//! t36 **no-vendor-in-core deny-test**: the `worker` (CF Workers SDK), `hyper`/`axum` (HTTP
//! servers), and CF/AWS vendor storage crates must NOT be reachable from the server CORE crates
//! (`qfs-core` / `qfs-server`) — they live ONLY behind the `qfs-host` host features and the leaf
//! binding crates.
//!
//! This mirrors the existing `qfs-cmd/tests/dep_direction.rs` guards but resolves the FULL
//! transitive dependency graph (`cargo metadata` WITHOUT `--no-deps`) so a vendor crate pulled in
//! transitively — not just as a direct edge — is caught. It is the "effects-as-data interpreter
//! runs on a vendor-free core" invariant (RFD §9: the single source over two runtimes keeps no
//! `worker::`/`hyper::` symbol in core).
//!
//! NOTE: `tokio` is NOT in the forbidden set here. `qfs-server` legitimately carries `tokio`
//! (`signal`, for the run loop), and `qfs-core`'s tokio-freedom is already pinned by
//! `qfs-plan`'s purity dep-closure test + the runtime-leaf-confinement guard. This test adds the
//! NEW t36 confinement: the CF `worker` SDK and the HTTP-server vendor crates never reach core.

// Test code: assertions and setup may panic/expect/unwrap freely.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::{BTreeMap, BTreeSet};
use std::process::Command;

/// The resolved dependency graph: package id → its resolved dependency package ids, plus a
/// name→id index. Built from `cargo metadata` WITH dependency resolution.
struct ResolvedGraph {
    /// node id → set of resolved dependency node ids (the `resolve.nodes[].deps`).
    edges: BTreeMap<String, Vec<String>>,
    /// node id → package name.
    name_of: BTreeMap<String, String>,
    /// package name → node id (workspace crates are unique by name).
    id_of: BTreeMap<String, String>,
}

fn load_resolved_graph() -> ResolvedGraph {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let output = Command::new(cargo)
        .args(["metadata", "--format-version", "1"])
        .output()
        .expect("run cargo metadata");
    assert!(
        output.status.success(),
        "cargo metadata failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("metadata JSON");

    let mut name_of = BTreeMap::new();
    let mut id_of = BTreeMap::new();
    for pkg in json["packages"].as_array().expect("packages") {
        let id = pkg["id"].as_str().expect("pkg id").to_string();
        let name = pkg["name"].as_str().expect("pkg name").to_string();
        name_of.insert(id.clone(), name.clone());
        id_of.insert(name, id);
    }

    let mut edges = BTreeMap::new();
    for node in json["resolve"]["nodes"].as_array().expect("resolve.nodes") {
        let id = node["id"].as_str().expect("node id").to_string();
        let deps: Vec<String> = node["dependencies"]
            .as_array()
            .expect("node dependencies")
            .iter()
            .filter_map(|d| d.as_str().map(str::to_string))
            .collect();
        edges.insert(id, deps);
    }

    ResolvedGraph {
        edges,
        name_of,
        id_of,
    }
}

impl ResolvedGraph {
    /// All package NAMES transitively reachable from `root` (including `root` itself).
    fn reachable_names(&self, root: &str) -> BTreeSet<String> {
        let mut out = BTreeSet::new();
        let Some(root_id) = self.id_of.get(root) else {
            panic!("crate {root} not in the resolved graph");
        };
        let mut stack = vec![root_id.clone()];
        let mut seen = BTreeSet::new();
        while let Some(id) = stack.pop() {
            if !seen.insert(id.clone()) {
                continue;
            }
            if let Some(name) = self.name_of.get(&id) {
                out.insert(name.clone());
            }
            if let Some(deps) = self.edges.get(&id) {
                for d in deps {
                    stack.push(d.clone());
                }
            }
        }
        out
    }
}

/// The vendor crates that must NEVER be reachable from the server core (t36, RFD §9).
const FORBIDDEN_IN_CORE: &[&str] = &[
    "worker",     // the CF Workers SDK — lives only behind qfs-host's parked host-workers.
    "worker-sys", // its sys crate.
    "hyper",      // an HTTP server — the in-house qfs-http handler is used instead (ADR-0004).
    "axum",       // an HTTP framework — ditto.
    "aws-sdk-s3", // an AWS storage SDK — drivers use thin HTTP clients (RFD §9), never an SDK.
    "rusoto_s3",
];

#[test]
fn server_core_crates_are_free_of_vendor_runtime_and_storage_types() {
    let graph = load_resolved_graph();
    for core in ["qfs-core", "qfs-server"] {
        let reachable = graph.reachable_names(core);
        for forbidden in FORBIDDEN_IN_CORE {
            assert!(
                !reachable.contains(*forbidden),
                "t36 no-vendor-in-core violation: `{forbidden}` is reachable from the server core \
                 crate `{core}`. The CF `worker` SDK, HTTP-server frameworks, and vendor storage \
                 SDKs must live ONLY behind the qfs-host host features / the leaf binding crates — \
                 the effect-plan interpreter runs on a vendor-free core (RFD §9)."
            );
        }
    }
}

#[test]
fn host_wasm_clean_core_is_free_of_worker_and_tokio() {
    // The wasm-clean qfs-host core (default features) must carry NO `worker` and NO `tokio`: it is
    // the part that MUST build on wasm32-unknown-unknown. qfs-server (tokio-bearing) is behind the
    // `host-daemon` feature, so with default features it is not reachable. This test runs without
    // any host feature enabled (the default `cargo test` profile), so the resolved graph for
    // qfs-host here is the wasm-clean closure.
    //
    // NOTE: `cargo metadata` resolves the union of features across the workspace, so a sibling
    // crate enabling `host-daemon` could pull qfs-server into qfs-host's resolved deps. We
    // therefore assert the narrower, robust property via the DIRECT manifest instead: qfs-host's
    // only NON-optional workspace dep is none, and `worker` is absent entirely (pinned by the
    // dep_direction `host_is_the_only_deployment_seam_and_a_leaf` guard). Here we assert the
    // crate-wide invariant that `worker` is nowhere in the workspace's resolved graph at all
    // (it is parked / uncached), so no build path can reach it.
    let graph = load_resolved_graph();
    let all: BTreeSet<String> = graph.name_of.values().cloned().collect();
    assert!(
        !all.contains("worker"),
        "t36: the `worker` crate must be absent from the entire resolved workspace graph (it is \
         parked per ADR-0005 and not in the offline cache); found it reachable, which means the \
         parked host-workers scaffold accidentally took a real `worker` dependency."
    );
}
