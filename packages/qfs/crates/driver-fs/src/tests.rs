//! In-crate unit tests for the `/fs` driver. **Every** test runs against a `tempfile` tempdir and
//! NEVER touches the user's real files; no network, no live credentials. Roots are operator-named
//! (`projects`, `assets`) over tempdirs so the per-root confinement is genuinely exercised.

use super::*;
use crate::fs_core::{self, FsRoots};
use qfs_driver::{check_capability, Archetype, Driver, Path, Verb};
use std::fs;
use tempfile::TempDir;

/// A tempdir + a single-root (`projects`) writable allowlist over it. Hermetic.
fn fixture() -> (TempDir, FsRoots) {
    let dir = TempDir::new().expect("tempdir");
    let roots = FsRoots::new().with_root("projects", dir.path());
    (dir, roots)
}

/// Write a file at `rel` under the tempdir with `content` (test setup, not the driver path).
fn seed(dir: &TempDir, rel: &str, content: &[u8]) {
    let p = dir.path().join(rel);
    if let Some(parent) = p.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(p, content).unwrap();
}

#[test]
fn describe_is_pure_blob_archetype_and_fsrow_schema() {
    // DESCRIBE is pure: a deny-all (empty-roots) driver still describes cred-free, no I/O, naming
    // no host path — proving the introspective half is path-free and wasm-safe.
    let d = FsDriver::new(FsRoots::new());
    assert_eq!(d.mount(), "/fs");
    let desc = d.describe(&Path::new("/fs/projects/a.txt")).unwrap();
    assert_eq!(desc.archetype, Archetype::BlobNamespace);
    let names: Vec<_> = desc.schema.column_names();
    assert_eq!(
        names,
        vec!["name", "path", "size", "modified", "is_dir", "mode"]
    );
}

#[test]
fn writable_mount_supports_blob_verbs_readonly_narrows_to_ls() {
    let (_dir, roots) = fixture();
    let p = Path::new("/fs/projects/x");

    let rw = FsDriver::new(roots.clone());
    for v in [
        Verb::Ls,
        Verb::Cp,
        Verb::Mv,
        Verb::Rm,
        Verb::Upsert,
        Verb::Remove,
    ] {
        assert!(
            check_capability(&rw, &p, v).is_ok(),
            "{v:?} should be allowed"
        );
    }
    // A relational verb is denied even on a writable blob mount.
    assert!(check_capability(&rw, &p, Verb::Update).is_err());

    let ro = FsDriver::read_only(roots);
    assert!(check_capability(&ro, &p, Verb::Ls).is_ok());
    for v in [Verb::Cp, Verb::Mv, Verb::Rm, Verb::Upsert, Verb::Remove] {
        let err = check_capability(&ro, &p, v).unwrap_err();
        assert_eq!(err.code(), "unsupported_verb");
    }
}

#[test]
fn deny_all_default_resolves_nothing() {
    // With no root configured, the driver is deny-all: any path's root lookup misses and fails
    // closed — no implicit whole-disk access (the flagged t68 default).
    let deny = FsRoots::new();
    assert!(deny.is_empty());
    let err = fs_core::scan_dir(&deny, "/fs/anything/here").unwrap_err();
    assert_eq!(err.code(), "unknown_root");
}

#[test]
fn scan_lists_a_directory_sorted_and_skips_dotfiles() {
    let (dir, roots) = fixture();
    seed(&dir, "b.txt", b"b");
    seed(&dir, "a.txt", b"a");
    seed(&dir, ".hidden", b"secret");
    fs::create_dir(dir.path().join("sub")).unwrap();

    let rows = fs_core::scan_dir(&roots, "/fs/projects").unwrap();
    let paths: Vec<_> = rows.iter().map(|r| r.path.clone()).collect();
    assert_eq!(
        paths,
        vec![
            "/fs/projects/a.txt",
            "/fs/projects/b.txt",
            "/fs/projects/sub"
        ]
    );
    let sub = rows.iter().find(|r| r.name == "sub").unwrap();
    assert!(sub.is_dir);
    let a = rows.iter().find(|r| r.name == "a.txt").unwrap();
    assert_eq!(a.size, 1);
    assert!(!a.is_dir);
}

#[test]
fn bare_mount_lists_each_configured_root() {
    // Two named roots present as virtual directories under `/fs`.
    let a = TempDir::new().unwrap();
    let b = TempDir::new().unwrap();
    let roots = FsRoots::new()
        .with_root("projects", a.path())
        .with_root("assets", b.path());
    let rows = fs_core::scan_dir(&roots, "/fs").unwrap();
    let names: Vec<_> = rows.iter().map(|r| r.name.clone()).collect();
    assert_eq!(names, vec!["assets", "projects"], "sorted by /fs path");
    assert!(rows.iter().all(|r| r.is_dir));
}

#[test]
fn glob_resolves_recursive_double_star_within_a_root() {
    let (dir, roots) = fixture();
    seed(&dir, "top.md", b"# top");
    seed(&dir, "sub/mid.md", b"# mid");
    seed(&dir, "sub/deep/leaf.md", b"# leaf");
    seed(&dir, "sub/other.txt", b"nope");

    let rows = fs_core::resolve_glob(&roots, "/fs/projects/**/*.md").unwrap();
    let paths: Vec<_> = rows.iter().map(|r| r.path.clone()).collect();
    assert_eq!(
        paths,
        vec![
            "/fs/projects/sub/deep/leaf.md",
            "/fs/projects/sub/mid.md",
            "/fs/projects/top.md",
        ]
    );
}

#[test]
fn wildcard_in_root_position_is_refused() {
    // A wildcard root segment would span multiple roots and make confinement undecidable.
    let (_dir, roots) = fixture();
    let err = fs_core::resolve_glob(&roots, "/fs/*/secret.md").unwrap_err();
    assert_eq!(err.code(), "outside_root");
}

#[test]
fn unknown_root_is_denied_with_no_io() {
    let (_dir, roots) = fixture();
    let err = fs_core::scan_dir(&roots, "/fs/secrets").unwrap_err();
    assert_eq!(err.code(), "unknown_root");
    let err = fs_core::read_blob(&roots, "/fs/secrets/key").unwrap_err();
    assert_eq!(err.code(), "unknown_root");
}

#[test]
fn parent_escape_is_refused_with_no_io() {
    let (_dir, roots) = fixture();
    let err = fs_core::scan_dir(&roots, "/fs/projects/../etc").unwrap_err();
    assert_eq!(err.code(), "outside_root");
    let err = fs_core::read_blob(&roots, "/fs/projects/../../secret").unwrap_err();
    assert_eq!(err.code(), "outside_root");
}

#[test]
fn symlink_escape_is_refused() {
    let (dir, roots) = fixture();
    let outside = TempDir::new().unwrap();
    fs::write(outside.path().join("loot"), b"loot").unwrap();
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(outside.path().join("loot"), dir.path().join("link")).unwrap();
        let err = fs_core::read_blob(&roots, "/fs/projects/link").unwrap_err();
        assert_eq!(err.code(), "outside_root");
    }
    #[cfg(not(unix))]
    {
        let _ = (dir, roots);
    }
}

#[test]
fn streaming_read_roundtrips_a_large_blob() {
    let (dir, roots) = fixture();
    // ~3 MiB, larger than the 64 KiB streaming buffer, to exercise multi-iteration reads.
    let big: Vec<u8> = (0..3 * 1024 * 1024).map(|i| (i % 251) as u8).collect();
    seed(&dir, "big.bin", &big);
    let got = fs_core::read_blob(&roots, "/fs/projects/big.bin").unwrap();
    assert_eq!(got, big, "streamed read is byte-identical");
}

#[test]
fn atomic_write_publishes_and_overwrites_cleanly() {
    let (dir, roots) = fixture();
    let n = fs_core::write_blob_atomic(&roots, "/fs/projects/out.txt", b"hello").unwrap();
    assert_eq!(n, 5);
    assert_eq!(fs::read(dir.path().join("out.txt")).unwrap(), b"hello");
    fs_core::write_blob_atomic(&roots, "/fs/projects/out.txt", b"world!!").unwrap();
    assert_eq!(fs::read(dir.path().join("out.txt")).unwrap(), b"world!!");
    let leftovers: Vec<_> = fs::read_dir(dir.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().contains("qfs-tmp"))
        .collect();
    assert!(leftovers.is_empty(), "no temp files leak after rename");
}

#[test]
fn copy_then_verify_preserves_source() {
    let (dir, roots) = fixture();
    seed(&dir, "src.txt", b"payload-bytes");
    let n = fs_core::copy_verify(&roots, "/fs/projects/src.txt", "/fs/projects/dst.txt").unwrap();
    assert_eq!(n, "payload-bytes".len() as u64);
    assert_eq!(
        fs::read(dir.path().join("src.txt")).unwrap(),
        b"payload-bytes"
    );
    assert_eq!(
        fs::read(dir.path().join("dst.txt")).unwrap(),
        b"payload-bytes"
    );
}

#[test]
fn applier_move_deletes_source_only_after_verify() {
    let (dir, roots) = fixture();
    seed(&dir, "m.txt", b"move-me");
    let applier = FsApplier::new(roots, false);
    let effect = FsEffect::Move {
        src: "/fs/projects/m.txt".to_string(),
        dst: "/fs/projects/moved.txt".to_string(),
    };
    use qfs_plan::{DriverId, EffectKind, EffectNode, NodeId, Target, VfsPath};
    use qfs_runtime::SharedApplier;
    let node = EffectNode::new(
        NodeId(0),
        EffectKind::Upsert,
        Target::new(DriverId::new("fs"), VfsPath::new("/fs/projects/moved.txt")),
    )
    .with_args(copy_move_args("/fs/projects/m.txt"))
    .irreversible(true);
    let out = applier.apply_shared(&node).unwrap();
    assert_eq!(out.affected, "move-me".len() as u64);
    assert!(
        !dir.path().join("m.txt").exists(),
        "source removed after verify"
    );
    assert_eq!(fs::read(dir.path().join("moved.txt")).unwrap(), b"move-me");
    assert!(effect.is_irreversible());
}

#[test]
fn mv_with_verify_failure_does_not_delete_source() {
    // A copy whose destination parent is a regular file fails BEFORE any verify can publish; the
    // Move arm calls copy_verify first and only unlinks the source on Ok, so the source survives.
    let (dir, roots) = fixture();
    seed(&dir, "keep.txt", b"do-not-lose-me");
    seed(&dir, "blocker", b"x");
    let applier = FsApplier::new(roots, false);
    use qfs_plan::{DriverId, EffectKind, EffectNode, NodeId, Target, VfsPath};
    use qfs_runtime::SharedApplier;
    let node = EffectNode::new(
        NodeId(0),
        EffectKind::Upsert,
        Target::new(
            DriverId::new("fs"),
            VfsPath::new("/fs/projects/blocker/dst.txt"),
        ),
    )
    .with_args(copy_move_args("/fs/projects/keep.txt"))
    .irreversible(true);
    let err = applier.apply_shared(&node).unwrap_err();
    assert_eq!(err.code(), "terminal", "a failed copy is a terminal effect");
    assert!(
        dir.path().join("keep.txt").exists(),
        "mv must NOT delete the source when the copy fails to verify/publish"
    );
}

#[test]
fn remove_effect_is_inherently_irreversible_and_deletes_a_blob() {
    let (dir, roots) = fixture();
    seed(&dir, "gone.txt", b"x");
    let applier = FsApplier::new(roots, false);
    use qfs_plan::{DriverId, EffectKind, EffectNode, NodeId, Target, VfsPath};
    use qfs_runtime::SharedApplier;
    let node = EffectNode::new(
        NodeId(0),
        EffectKind::Remove,
        Target::new(DriverId::new("fs"), VfsPath::new("/fs/projects/gone.txt")),
    );
    // Deleting a real file is irreversible → the engine flags the node, so PREVIEW warns and the
    // commit needs the extra acknowledgement. We never reclassify it as reversible.
    assert!(node.irreversible, "REMOVE is inherently irreversible");
    assert!(
        EffectKind::Remove.is_inherently_irreversible(),
        "the kind itself is irreversible"
    );
    let out = applier.apply_shared(&node).unwrap();
    assert_eq!(out.affected, 1);
    assert!(!dir.path().join("gone.txt").exists());
}

#[test]
fn read_only_applier_denies_writes_and_touches_no_files() {
    let (dir, roots) = fixture();
    let applier = FsApplier::new(roots, true);
    use qfs_plan::{DriverId, EffectKind, EffectNode, NodeId, Target, VfsPath};
    use qfs_runtime::SharedApplier;
    let node = EffectNode::new(
        NodeId(0),
        EffectKind::Upsert,
        Target::new(DriverId::new("fs"), VfsPath::new("/fs/projects/nope.txt")),
    )
    .with_args(blob_write_args(b"blocked".to_vec()));
    let err = applier.apply_shared(&node).unwrap_err();
    // The discriminant is preserved through the bridge: a read_only write denial is the structured
    // `capability_denied` class, not a generic `terminal`.
    assert_eq!(err.code(), "capability_denied");
    assert!(
        !dir.path().join("nope.txt").exists(),
        "no file written on denial"
    );
}

#[test]
fn apply_time_escape_is_refused_distinctly_from_capability_denial() {
    // The bridge must NOT collapse a confinement breach and a capability denial into the same code:
    // the audit ledger has to tell "tried to reach outside a root" apart from "lacked permission".
    use qfs_runtime::EffectError;

    let escape: EffectError =
        crate::error::FsError::OutsideRoot("/fs/projects/../../etc".into()).into();
    let unknown: EffectError = crate::error::FsError::UnknownRoot {
        path: "/fs/secrets/key".into(),
        root: "secrets".into(),
    }
    .into();
    let denial: EffectError = crate::error::FsError::CapabilityDenied {
        path: "/fs/projects/x".into(),
        verb: "UPSERT",
    }
    .into();

    assert_eq!(escape.code(), "sandbox_escape");
    assert_eq!(unknown.code(), "sandbox_escape");
    assert_eq!(denial.code(), "capability_denied");
    assert!(!escape.is_retryable());
    assert!(!denial.is_retryable());
}

#[test]
fn driver_id_is_fs() {
    let (_dir, roots) = fixture();
    let d = FsDriver::new(roots);
    assert_eq!(d.id(), qfs_types::DriverId::new("fs"));
}

#[test]
fn applier_seam_is_reachable_through_driver() {
    let (_dir, roots) = fixture();
    let d = FsDriver::new(roots);
    let _seam: &dyn qfs_plan::PlanApplier = d.applier();
}
