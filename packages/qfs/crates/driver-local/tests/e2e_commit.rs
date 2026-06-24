//! End-to-end COMMIT of a small plan over `/local` through the t10 interpreter and the
//! sync→async bridge (RFD-0001 §3/§6). Proves a real driver runs the full effect-plan path:
//! the plan is built as pure data, then `Interpreter::commit` dispatches each effect to the
//! `PlanApplierBridge` wrapping the local FS applier, which performs the real (tempdir) I/O.
//!
//! Every test runs against a `tempfile` tempdir — NEVER the user's files; no network.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::fs;
use std::sync::Arc;

use qfs_driver::Driver;
use qfs_driver_local::{blob_write_args, copy_move_args, local_apply_driver, LocalFsDriver};
use qfs_plan::{DriverId, EffectKind, EffectNode, NodeId, PlanBuilder, Target, VfsPath};
use qfs_runtime::{CapabilitySet, DriverRegistry, Interpreter};
use tempfile::TempDir;

fn target(path: &str) -> Target {
    Target::new(DriverId::new("local"), VfsPath::new(path))
}

/// Build the interpreter wired to a `/local` mount over `dir`, granting the verbs the plan
/// uses. Returns the interpreter and an allow-all capability set is avoided in favour of
/// explicit grants so the gate is genuinely exercised.
fn interpreter_for(dir: &TempDir) -> (Interpreter, LocalFsDriver) {
    let driver = LocalFsDriver::new(dir.path());
    let bridge = local_apply_driver(&driver);
    let registry = DriverRegistry::new().with(driver.id(), Arc::new(bridge));
    (Interpreter::with_defaults(registry), driver)
}

#[tokio::test]
async fn commit_upsert_then_copy_then_remove_end_to_end() {
    let dir = TempDir::new().unwrap();
    let (interp, _driver) = interpreter_for(&dir);

    // Plan: write a blob (Upsert), then copy it to a second path (Upsert+src), then remove
    // the original (Remove). The copy depends on the write; the remove depends on the copy.
    let mut b = PlanBuilder::new();
    let write = b.push(
        EffectNode::new(NodeId(0), EffectKind::Upsert, target("/local/a.txt"))
            .with_args(blob_write_args(b"end-to-end".to_vec())),
    );
    let copy = b.push(
        EffectNode::new(NodeId(1), EffectKind::Upsert, target("/local/b.txt"))
            .with_args(copy_move_args("/local/a.txt")),
    );
    let remove = b.push(EffectNode::new(
        NodeId(2),
        EffectKind::Remove,
        target("/local/a.txt"),
    ));
    b.depends_on(copy, write);
    b.depends_on(remove, copy);
    let plan = b.build();
    plan.validate().unwrap();

    let caps = CapabilitySet::none()
        .grant(DriverId::new("local"), &EffectKind::Upsert)
        .grant(DriverId::new("local"), &EffectKind::Remove);

    let outcome = interp.commit(plan, &caps).await.unwrap();

    // Every leg applied, in stable topological order.
    assert!(outcome.is_complete(), "all three legs applied: {outcome:?}");
    assert_eq!(outcome.applied_ids(), vec![NodeId(0), NodeId(1), NodeId(2)]);

    // Real World state: the copy exists, the original was removed.
    assert!(!dir.path().join("a.txt").exists(), "original removed");
    assert_eq!(fs::read(dir.path().join("b.txt")).unwrap(), b"end-to-end");
}

#[tokio::test]
async fn commit_denies_write_on_read_only_mount() {
    let dir = TempDir::new().unwrap();
    // A read-only driver: its applier denies the write before any I/O, and the interpreter
    // also has no Upsert grant — defense in depth. We assert no file is created either way.
    let driver = LocalFsDriver::read_only(dir.path());
    let bridge = local_apply_driver(&driver);
    let registry = DriverRegistry::new().with(driver.id(), Arc::new(bridge));
    let interp = Interpreter::with_defaults(registry);

    let mut b = PlanBuilder::new();
    b.push(
        EffectNode::new(NodeId(0), EffectKind::Upsert, target("/local/blocked.txt"))
            .with_args(blob_write_args(b"nope".to_vec())),
    );
    let plan = b.build();

    // Grant the verb at the runtime gate so the *driver's* read_only denial is what stops it.
    let caps = CapabilitySet::none().grant(DriverId::new("local"), &EffectKind::Upsert);
    let outcome = interp.commit(plan, &caps).await.unwrap();

    assert_eq!(outcome.failed_count(), 1, "the write failed terminally");
    assert!(!outcome.is_complete());
    assert!(
        !dir.path().join("blocked.txt").exists(),
        "read-only mount touched no files"
    );
}

#[tokio::test]
async fn commit_scan_lists_a_seeded_tree() {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("one.md"), b"# one").unwrap();
    fs::write(dir.path().join("two.md"), b"# two").unwrap();
    let (interp, _driver) = interpreter_for(&dir);

    // A single List effect over the mount root scans the directory.
    let mut b = PlanBuilder::new();
    b.push(EffectNode::new(
        NodeId(0),
        EffectKind::List,
        target("/local"),
    ));
    let plan = b.build();

    let caps = CapabilitySet::none().grant(DriverId::new("local"), &EffectKind::List);
    let outcome = interp.commit(plan, &caps).await.unwrap();
    assert!(outcome.is_complete());
    // The scan reported the two .md files as the affected count.
    let entry = &outcome.ledger[0];
    match entry.status {
        qfs_runtime::LegStatus::Applied { affected, .. } => assert_eq!(affected, 2),
        ref other => panic!("expected applied, got {other:?}"),
    }
}
