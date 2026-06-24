//! Purity-invariant dependency guard (t09 acceptance criterion).
//!
//! `qfs-plan` is the effect substrate; its load-bearing property is that constructing
//! and previewing a plan does **no I/O** (RFD §3). This test mechanically asserts the
//! crate's resolved dependency set excludes async runtimes, HTTP clients, and vendor
//! SDKs — so "a plan does I/O" cannot regress in by way of a stray dependency. It
//! shells out to `cargo metadata` (via the `CARGO` env var) and inspects the full
//! transitive dependency tree of the `qfs-plan` package.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::process::Command;

/// Substrings of crate names that would betray I/O / async / vendor coupling. If any
/// transitive dependency name contains one of these, the purity invariant is broken.
const FORBIDDEN: &[&str] = &[
    "tokio",
    "async-std",
    "smol",
    "reqwest",
    "hyper",
    "ureq",
    "curl",
    "mio",
    "socket",
    "google-",
    "aws-",
    "octocrab",
    "rusoto",
];

#[test]
fn qfs_plan_has_no_io_async_or_vendor_dependencies() {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let output = Command::new(cargo)
        .args([
            "metadata",
            "--format-version",
            "1",
            "--filter-platform",
            current_platform(),
        ])
        .output()
        .expect("failed to run `cargo metadata`");
    assert!(
        output.status.success(),
        "cargo metadata failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let json: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("cargo metadata produced invalid JSON");

    // Find the resolved dependency closure of the qfs-plan node and assert each
    // dependency name is clean. We walk the resolve graph from the qfs-plan node.
    let resolve = &json["resolve"];
    let nodes = resolve["nodes"]
        .as_array()
        .expect("resolve.nodes is an array");

    // Map package id -> name for readable assertions.
    let id_to_name: std::collections::BTreeMap<String, String> = json["packages"]
        .as_array()
        .expect("packages")
        .iter()
        .map(|p| {
            (
                p["id"].as_str().expect("id").to_string(),
                p["name"].as_str().expect("name").to_string(),
            )
        })
        .collect();

    // Locate the qfs-plan node id.
    let plan_id = id_to_name
        .iter()
        .find(|(_, name)| name.as_str() == "qfs-plan")
        .map(|(id, _)| id.clone())
        .expect("qfs-plan package present");

    // BFS the resolve graph from qfs-plan, collecting every transitive dependency.
    let node_by_id: std::collections::BTreeMap<&str, &serde_json::Value> = nodes
        .iter()
        .map(|n| (n["id"].as_str().expect("node id"), n))
        .collect();

    let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut stack = vec![plan_id];
    while let Some(id) = stack.pop() {
        if !seen.insert(id.clone()) {
            continue;
        }
        if let Some(node) = node_by_id.get(id.as_str()) {
            for dep in node["dependencies"].as_array().into_iter().flatten() {
                if let Some(dep_id) = dep.as_str() {
                    stack.push(dep_id.to_string());
                }
            }
        }
    }

    for id in &seen {
        let name = id_to_name.get(id).cloned().unwrap_or_default();
        for bad in FORBIDDEN {
            assert!(
                !name.contains(bad),
                "purity violation: qfs-plan transitively depends on `{name}` \
                 (matches forbidden `{bad}`) — the effect substrate must stay I/O-free"
            );
        }
    }
}

/// The current target triple, so `--filter-platform` resolves only the deps that
/// actually compile here (native aarch64; no wasm) — avoids flagging platform-gated
/// crates that are never built.
fn current_platform() -> &'static str {
    // A small fixed set covering the supported native targets; default is aarch64.
    if cfg!(all(target_arch = "x86_64", target_os = "linux")) {
        "x86_64-unknown-linux-gnu"
    } else {
        "aarch64-unknown-linux-gnu"
    }
}
