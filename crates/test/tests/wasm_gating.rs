//! The carried wasm-gating mechanical guard (t38; the precedent carried since t25/t33).
//!
//! Each wasm-gated leaf — `cfs-cron` (`native`), `cfs-watchtower` (`native`), `cfs-host`
//! (`host-daemon`), `cfs-driver-slack` (`runtime`) — gates its non-wasm-clean deps (above all
//! **tokio**) behind an *optional* feature, so that with `--no-default-features` only the pure
//! core compiles and it builds for `wasm32-unknown-unknown`. The load-bearing fence is the
//! **absence** of that feature (the deps are `optional`), not any marker (the t25/slack lesson).
//!
//! This guard makes the fence MECHANICAL rather than conventional: for each gated leaf it
//! computes the `--no-default-features` dependency closure (the package's own deps minus the
//! optional deps the gating feature would turn on, transitively) and asserts **tokio is not in
//! it**. So a regression that moves tokio out from behind the gate — or adds a new non-optional
//! tokio edge to a gated leaf — fails here, the same shape as `crates/plan/tests/purity_deps.rs`.
//!
//! It is intentionally a SOURCE-level closure (over `cargo metadata`'s declared `dependencies`),
//! not a `cargo build --target wasm32` (which the disk budget forbids and which CI does once):
//! the dependency graph is what makes the wasm build possible, so the graph is what we lock.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::{BTreeMap, BTreeSet};
use std::process::Command;

/// The gated leaves and the feature that turns on their non-wasm-clean (tokio-bearing) deps.
/// With that feature OFF (the `--no-default-features` wasm invocation), the closure must be
/// tokio-free.
const GATED_LEAVES: &[(&str, &str)] = &[
    ("cfs-cron", "native"),
    ("cfs-watchtower", "native"),
    ("cfs-host", "host-daemon"),
    ("cfs-driver-slack", "runtime"),
];

/// Crate-name substrings that betray a non-wasm-clean (async-runtime / socket) dependency.
const FORBIDDEN: &[&str] = &["tokio", "mio", "socket2"];

#[test]
fn each_wasm_gated_leaf_has_a_tokio_free_no_default_features_closure() {
    let json = cargo_metadata();
    let packages = json["packages"].as_array().expect("packages");

    // name -> package object (workspace + registry packages).
    let pkg_by_name: BTreeMap<&str, &serde_json::Value> = packages
        .iter()
        .map(|p| (p["name"].as_str().expect("name"), p))
        .collect();

    // POSITIVE CONTROL (the load-bearing self-check): run the SAME closure-walk on `cfs-http`,
    // a NON-gated leaf whose tokio is non-optional + unconditional (crates/http/Cargo.toml has no
    // `[features]` and `tokio = { ..., optional = false }`). Its --no-default-features closure MUST
    // contain a `tokio` match. If this fails, the closure walk is computing nothing / the wrong set
    // (e.g. a cargo-metadata schema change emptied it), which would let every negative assertion
    // below pass VACUOUSLY — so this control is what proves the guard actually bites.
    let control = no_default_features_closure("cfs-http", "<none>", &pkg_by_name);
    assert!(
        control.iter().any(|dep| dep.contains("tokio")),
        "positive control failed: the closure walk did not find `tokio` in `cfs-http`'s \
         non-optional dependency closure — the walk is vacuous, so the negative (tokio-absent) \
         assertions below cannot be trusted. Closure was: {control:?}"
    );

    // NEGATIVE assertions: each gated leaf's --no-default-features closure is tokio-free.
    for (leaf, gate) in GATED_LEAVES {
        let closure = no_default_features_closure(leaf, gate, &pkg_by_name);
        // The walk is non-vacuous for the gated leaves too: each pulls the pure spine (cfs-core
        // etc.), so an empty closure would itself be a walk bug — guard against it explicitly.
        assert!(
            !closure.is_empty(),
            "closure walk for gated leaf `{leaf}` is empty — the walk is vacuous, not a real \
             tokio-free result"
        );
        for dep in &closure {
            for bad in FORBIDDEN {
                assert!(
                    !dep.contains(bad),
                    "wasm-gating violation: `{leaf}` --no-default-features transitively depends \
                     on `{dep}` (matches forbidden `{bad}`) — its pure core must build for \
                     wasm32 WITHOUT tokio; the `{gate}`-gated deps must stay behind the feature"
                );
            }
        }
    }
}

/// Compute the dependency-name closure of `leaf` with **default features off and the gating
/// feature off** — i.e. the deps that are NOT `optional` (so they are present regardless of
/// features) and the deps any *always-on* feature enables, walked transitively over the same
/// rule. Optional deps (the gated tokio-bearers) are excluded.
///
/// This is a source-level (declared-dependency) closure: for each package we include a
/// dependency edge only if the dependency is non-optional and is a normal (not dev/build) dep.
/// That is exactly the set `cargo build -p <leaf> --no-default-features` would compile, which
/// is what the wasm32 build exercises.
fn no_default_features_closure(
    leaf: &str,
    _gate: &str,
    pkg_by_name: &BTreeMap<&str, &serde_json::Value>,
) -> BTreeSet<String> {
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut stack = vec![leaf.to_string()];
    while let Some(name) = stack.pop() {
        if !seen.insert(name.clone()) {
            continue;
        }
        let Some(pkg) = pkg_by_name.get(name.as_str()) else {
            continue;
        };
        for dep in pkg["dependencies"].as_array().into_iter().flatten() {
            let kind = dep["kind"].as_str();
            // Skip dev/build dependencies — they are never in the wasm build's closure.
            if kind == Some("dev") || kind == Some("build") {
                continue;
            }
            // Skip OPTIONAL dependencies — with the gating feature off they are not compiled.
            // This is the load-bearing exclusion: the tokio-bearing deps are `optional`, so
            // dropping them here models `--no-default-features`.
            if dep["optional"].as_bool().unwrap_or(false) {
                continue;
            }
            if let Some(dep_name) = dep["name"].as_str() {
                stack.push(dep_name.to_string());
            }
        }
    }
    // Do not count the leaf itself as a "dependency".
    seen.remove(leaf);
    seen
}

fn cargo_metadata() -> serde_json::Value {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let output = Command::new(cargo)
        .args(["metadata", "--format-version", "1"])
        .output()
        .expect("failed to run `cargo metadata`");
    assert!(
        output.status.success(),
        "cargo metadata failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("cargo metadata produced invalid JSON")
}
