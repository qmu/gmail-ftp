//! End-to-end COMMIT of a small plan over `/fs` through the t10 interpreter and the sync→async
//! bridge (RFD-0001 §3/§6). Proves the first-class fs driver runs the full effect-plan path: the
//! plan is built as pure data, then `Interpreter::commit` dispatches each effect to the
//! `PlanApplierBridge` wrapping the fs applier, which performs the real (tempdir) I/O under an
//! operator-configured named root. PREVIEW never reaches the applier — only COMMIT does.
//!
//! Every test runs against a `tempfile` tempdir — NEVER the user's files; no network.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::fs;
use std::sync::Arc;

use qfs_driver::Driver;
use qfs_driver_fs::{blob_write_args, copy_move_args, fs_apply_driver, FsDriver, FsRoots};
use qfs_plan::{DriverId, EffectKind, EffectNode, NodeId, PlanBuilder, Target, VfsPath};
use qfs_runtime::{CapabilitySet, DriverRegistry, Interpreter};
use tempfile::TempDir;

fn target(path: &str) -> Target {
    Target::new(DriverId::new("fs"), VfsPath::new(path))
}

/// Build the interpreter wired to a `/fs` mount whose `projects` root is `dir`.
fn interpreter_for(dir: &TempDir) -> Interpreter {
    let driver = FsDriver::new(FsRoots::new().with_root("projects", dir.path()));
    let bridge = fs_apply_driver(&driver);
    let registry = DriverRegistry::new().with(driver.id(), Arc::new(bridge));
    Interpreter::with_defaults(registry)
}

#[tokio::test]
async fn commit_upsert_then_copy_then_remove_end_to_end() {
    let dir = TempDir::new().unwrap();
    let interp = interpreter_for(&dir);

    // Plan: write a blob (Upsert), copy it to a second path (Upsert+src), then remove the original
    // (Remove). The copy depends on the write; the remove depends on the copy.
    let mut b = PlanBuilder::new();
    let write = b.push(
        EffectNode::new(NodeId(0), EffectKind::Upsert, target("/fs/projects/a.txt"))
            .with_args(blob_write_args(b"end-to-end".to_vec())),
    );
    let copy = b.push(
        EffectNode::new(NodeId(1), EffectKind::Upsert, target("/fs/projects/b.txt"))
            .with_args(copy_move_args("/fs/projects/a.txt")),
    );
    let remove = b.push(EffectNode::new(
        NodeId(2),
        EffectKind::Remove,
        target("/fs/projects/a.txt"),
    ));
    b.depends_on(copy, write);
    b.depends_on(remove, copy);
    let plan = b.build();
    plan.validate().unwrap();

    // The REMOVE leg is inherently irreversible — the grant must explicitly include it.
    let caps = CapabilitySet::none()
        .grant(DriverId::new("fs"), &EffectKind::Upsert)
        .grant(DriverId::new("fs"), &EffectKind::Remove);

    let outcome = interp.commit(plan, &caps).await.unwrap();

    assert!(outcome.is_complete(), "all three legs applied: {outcome:?}");
    assert_eq!(outcome.applied_ids(), vec![NodeId(0), NodeId(1), NodeId(2)]);

    assert!(!dir.path().join("a.txt").exists(), "original removed");
    assert_eq!(fs::read(dir.path().join("b.txt")).unwrap(), b"end-to-end");
}

#[tokio::test]
async fn commit_denies_write_to_unconfigured_root() {
    // A write whose root is not in the allowlist fails closed (the deny-all confinement) — and the
    // real World is untouched (the `secrets` tempdir gets no file).
    let dir = TempDir::new().unwrap();
    let other = TempDir::new().unwrap();
    let interp = interpreter_for(&dir);

    let mut b = PlanBuilder::new();
    b.push(
        EffectNode::new(
            NodeId(0),
            EffectKind::Upsert,
            target("/fs/secrets/leak.txt"),
        )
        .with_args(blob_write_args(b"nope".to_vec())),
    );
    let plan = b.build();

    let caps = CapabilitySet::none().grant(DriverId::new("fs"), &EffectKind::Upsert);
    let outcome = interp.commit(plan, &caps).await.unwrap();

    assert_eq!(
        outcome.failed_count(),
        1,
        "the unconfigured-root write failed"
    );
    assert!(!outcome.is_complete());
    assert!(
        !other.path().join("leak.txt").exists(),
        "an unconfigured root touched no files"
    );
}

#[tokio::test]
async fn commit_scan_lists_a_seeded_tree() {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("one.md"), b"# one").unwrap();
    fs::write(dir.path().join("two.md"), b"# two").unwrap();
    let interp = interpreter_for(&dir);

    let mut b = PlanBuilder::new();
    b.push(EffectNode::new(
        NodeId(0),
        EffectKind::List,
        target("/fs/projects"),
    ));
    let plan = b.build();

    let caps = CapabilitySet::none().grant(DriverId::new("fs"), &EffectKind::List);
    let outcome = interp.commit(plan, &caps).await.unwrap();
    assert!(outcome.is_complete());
    let entry = &outcome.ledger[0];
    match entry.status {
        qfs_runtime::LegStatus::Applied { affected, .. } => assert_eq!(affected, 2),
        ref other => panic!("expected applied, got {other:?}"),
    }
}
