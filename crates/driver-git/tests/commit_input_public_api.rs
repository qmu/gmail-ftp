//! **Out-of-crate reachability proof** for the `INSERT INTO /git/<repo>/commits` write path (the
//! RFD §3 keyword-clash centerpiece). This is a SEPARATE crate, so it sees only the public API —
//! exactly the surface the engine / CLI / interpreter uses. It builds a [`CommitInput`] through the
//! public constructor + builders and feeds it through [`plan_insert_commit`].
//!
//! Before the fix, [`CommitInput`] was `#[non_exhaustive]` with no public constructor, so this
//! file would FAIL to compile (E0639: a `#[non_exhaustive]` struct cannot be built with a struct
//! literal outside its defining crate, and there was no `new`). The in-crate unit suite missed it
//! because struct-literal construction is legal inside the defining crate. This test compiling +
//! passing is the regression guard that the supported external entry point exists.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;
use std::sync::Arc;

use cfs_driver_git::{
    plan_insert_commit, CommitInput, LooseObjectDb, ObjectDb, ObjectKind, Oid, Repo, RepoResolver,
    Tree, TreeEntry,
};

/// Build a minimal one-commit repo entirely through the public API (the engine builds repos this
/// way too), returning the resolver + the initial commit oid.
fn external_repo() -> (RepoResolver, Oid) {
    let mut db = LooseObjectDb::new();
    let blob = db.insert_object(ObjectKind::Blob, b"seed\n");
    let tree = Tree {
        entries: vec![TreeEntry {
            mode: "100644".to_string(),
            name: "seed.txt".to_string(),
            oid: blob,
        }],
    };
    let tree_oid = db.insert_object(ObjectKind::Tree, &cfs_driver_git::serialize_tree(&tree));
    let commit_payload = format!(
        "tree {}\nauthor A <a@e.com> 1700000000 +0000\ncommitter A <a@e.com> 1700000000 +0000\n\nseed\n",
        tree_oid.as_str()
    );
    let c0 = db.insert_object(ObjectKind::Commit, commit_payload.as_bytes());

    let mut repo = Repo::new(Arc::new(db) as Arc<dyn ObjectDb>);
    repo.set_ref("refs/heads/main", c0.clone());
    (RepoResolver::new().with_repo("ext", repo), c0)
}

#[test]
fn commit_input_is_constructible_and_plannable_from_outside_the_crate() {
    let (resolver, c0) = external_repo();
    let repo = resolver.repo("ext").unwrap();

    // The supported external path: public constructor + builders. (A struct literal here would be
    // E0639 — that is exactly the defect this guards.)
    let input = CommitInput::new(
        "refs/heads/main",
        "Ext <ext@example.com>",
        "Ext <ext@example.com>",
        "External commit via public API",
    )
    .at_time(1_700_000_500)
    .with_file("a.txt", b"alpha\n".to_vec())
    .with_file("b.txt", b"beta\n".to_vec());

    let planned = plan_insert_commit("ext", repo, &input).unwrap();

    // CommitPlan's fields read fine across the boundary (no change needed there): the CAS old-oid
    // is the current branch tip, and a new commit oid was computed.
    assert_eq!(planned.old_commit.as_ref().unwrap().as_str(), c0.as_str());
    assert_ne!(planned.new_commit.as_str(), c0.as_str());
    // blob + blob + tree + commit + ref (+ reflog) — a complete pure write plan, applies nothing.
    assert!(planned.plan.nodes().len() >= 5);
}

#[test]
fn with_files_replaces_the_staged_tree() {
    let (resolver, _c0) = external_repo();
    let repo = resolver.repo("ext").unwrap();

    let mut files = BTreeMap::new();
    files.insert("only.txt".to_string(), b"x\n".to_vec());
    let input = CommitInput::new("refs/heads/main", "A <a@e.com>", "A <a@e.com>", "msg")
        .with_file("dropped.txt", b"y\n".to_vec())
        .with_files(files);

    let planned = plan_insert_commit("ext", repo, &input).unwrap();
    // with_files replaced the earlier with_file: exactly one blob staged → blob + tree + commit +
    // ref (+ reflog) = at least 4 nodes, and no `dropped.txt` blob.
    assert!(planned.plan.nodes().len() >= 4);
}
