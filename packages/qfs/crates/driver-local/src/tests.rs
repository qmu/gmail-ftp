//! In-crate unit tests for the local FS driver. **Every** test runs against a `tempfile`
//! tempdir and NEVER touches the user's real files; no network, no live credentials.

use super::*;
use crate::fs_core::{self, Sandbox};
use qfs_driver::{check_capability, Archetype, Driver, Path, Verb};
use std::fs;
use tempfile::TempDir;

/// A tempdir + a writable sandbox over it. Helper so every test is hermetic.
fn fixture() -> (TempDir, Sandbox) {
    let dir = TempDir::new().expect("tempdir");
    let sandbox = Sandbox::new(dir.path().to_path_buf());
    (dir, sandbox)
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
fn describe_reports_blob_archetype_and_localrow_schema() {
    let (dir, _) = fixture();
    let d = LocalFsDriver::new(dir.path());
    assert_eq!(d.mount(), "/local");
    let desc = d.describe(&Path::new("/local")).unwrap();
    assert_eq!(desc.archetype, Archetype::BlobNamespace);
    let names: Vec<_> = desc.schema.column_names();
    assert_eq!(
        names,
        vec!["name", "path", "size", "modified", "is_dir", "mode"]
    );
}

#[test]
fn writable_mount_supports_blob_verbs_readonly_narrows_to_ls() {
    let (dir, _) = fixture();
    let p = Path::new("/local/x");

    let rw = LocalFsDriver::new(dir.path());
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

    let ro = LocalFsDriver::read_only(dir.path());
    assert!(check_capability(&ro, &p, Verb::Ls).is_ok());
    for v in [Verb::Cp, Verb::Mv, Verb::Rm, Verb::Upsert, Verb::Remove] {
        let err = check_capability(&ro, &p, v).unwrap_err();
        assert_eq!(err.code(), "unsupported_verb");
    }
}

#[test]
fn scan_lists_a_directory_sorted_and_skips_dotfiles() {
    let (dir, sandbox) = fixture();
    seed(&dir, "b.txt", b"b");
    seed(&dir, "a.txt", b"a");
    seed(&dir, ".hidden", b"secret");
    fs::create_dir(dir.path().join("sub")).unwrap();

    let rows = fs_core::scan_dir(&sandbox, "/local").unwrap();
    let paths: Vec<_> = rows.iter().map(|r| r.path.clone()).collect();
    // Deterministic sorted order; dotfile excluded; directory present and flagged.
    assert_eq!(paths, vec!["/local/a.txt", "/local/b.txt", "/local/sub"]);
    let sub = rows.iter().find(|r| r.name == "sub").unwrap();
    assert!(sub.is_dir);
    let a = rows.iter().find(|r| r.name == "a.txt").unwrap();
    assert_eq!(a.size, 1);
    assert!(!a.is_dir);
}

#[test]
fn glob_resolves_recursive_double_star() {
    let (dir, sandbox) = fixture();
    seed(&dir, "top.md", b"# top");
    seed(&dir, "sub/mid.md", b"# mid");
    seed(&dir, "sub/deep/leaf.md", b"# leaf");
    seed(&dir, "sub/other.txt", b"nope");

    let rows = fs_core::resolve_glob(&sandbox, "/local/**/*.md").unwrap();
    let paths: Vec<_> = rows.iter().map(|r| r.path.clone()).collect();
    assert_eq!(
        paths,
        vec![
            "/local/sub/deep/leaf.md",
            "/local/sub/mid.md",
            "/local/top.md",
        ]
    );
}

#[test]
fn glob_single_star_is_one_level_only() {
    let (dir, sandbox) = fixture();
    seed(&dir, "a.md", b"a");
    seed(&dir, "sub/b.md", b"b");
    let rows = fs_core::resolve_glob(&sandbox, "/local/*.md").unwrap();
    let paths: Vec<_> = rows.iter().map(|r| r.path.clone()).collect();
    assert_eq!(paths, vec!["/local/a.md"]);
}

#[test]
fn sandbox_rejects_parent_escape_with_no_io() {
    let (_dir, sandbox) = fixture();
    let err = fs_core::scan_dir(&sandbox, "/local/../etc").unwrap_err();
    assert_eq!(err.code(), "outside_sandbox");
    let err = fs_core::read_blob(&sandbox, "/local/../../secret").unwrap_err();
    assert_eq!(err.code(), "outside_sandbox");
}

#[test]
fn sandbox_rejects_symlink_escape() {
    let (dir, sandbox) = fixture();
    // An outside target the symlink points at.
    let outside = TempDir::new().unwrap();
    fs::write(outside.path().join("loot"), b"loot").unwrap();
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(outside.path().join("loot"), dir.path().join("link")).unwrap();
        let err = fs_core::read_blob(&sandbox, "/local/link").unwrap_err();
        assert_eq!(err.code(), "outside_sandbox");
    }
    #[cfg(not(unix))]
    {
        let _ = (dir, sandbox);
    }
}

#[test]
fn streaming_read_roundtrips_a_large_blob() {
    let (dir, sandbox) = fixture();
    // ~3 MiB, larger than the 64 KiB streaming buffer, to exercise multi-iteration reads.
    let big: Vec<u8> = (0..3 * 1024 * 1024).map(|i| (i % 251) as u8).collect();
    seed(&dir, "big.bin", &big);
    let got = fs_core::read_blob(&sandbox, "/local/big.bin").unwrap();
    assert_eq!(got, big, "streamed read is byte-identical");
}

#[test]
fn atomic_write_publishes_and_overwrites_cleanly() {
    let (dir, sandbox) = fixture();
    let n = fs_core::write_blob_atomic(&sandbox, "/local/out.txt", b"hello").unwrap();
    assert_eq!(n, 5);
    assert_eq!(fs::read(dir.path().join("out.txt")).unwrap(), b"hello");
    // Overwrite via temp+rename leaves no stray temp file.
    fs_core::write_blob_atomic(&sandbox, "/local/out.txt", b"world!!").unwrap();
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
    let (dir, sandbox) = fixture();
    seed(&dir, "src.txt", b"payload-bytes");
    let n = fs_core::copy_verify(&sandbox, "/local/src.txt", "/local/dst.txt").unwrap();
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
fn copy_verify_with_hash_accepts_genuine_copy_and_keeps_source() {
    // The size+hash verify must not break the happy path: a genuine copy of multi-buffer
    // content (> COPY_BUF) is published, the source is preserved, and bytes are identical.
    let (dir, sandbox) = fixture();
    let payload: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
    seed(&dir, "big.src", &payload);
    let n = fs_core::copy_verify(&sandbox, "/local/big.src", "/local/big.dst").unwrap();
    assert_eq!(n, payload.len() as u64);
    assert_eq!(fs::read(dir.path().join("big.src")).unwrap(), payload);
    assert_eq!(fs::read(dir.path().join("big.dst")).unwrap(), payload);
}

#[test]
fn mv_with_verify_failure_does_not_delete_source() {
    // Simulate a copy that fails verification: the destination resolves under a *path whose
    // parent is itself a file*, so the copy's create/rename leg fails BEFORE any verify can
    // publish. The Move arm calls copy_verify first and only unlinks the source on Ok, so the
    // source MUST survive (no-data-loss). This exercises the verify→[delete] ordering: a copy
    // that does not verify never triggers the source unlink.
    let (dir, sandbox) = fixture();
    seed(&dir, "keep.txt", b"do-not-lose-me");
    // `blocker` is a regular file; treating it as a directory parent for the destination forces
    // create_dir_all / create to fail, so copy_verify errors before publishing.
    seed(&dir, "blocker", b"x");
    let applier = LocalApplier::new(sandbox, false);
    use qfs_plan::{DriverId, EffectKind, EffectNode, NodeId, Target, VfsPath};
    use qfs_runtime::SharedApplier;
    let node = EffectNode::new(
        NodeId(0),
        EffectKind::Upsert,
        Target::new(
            DriverId::new("local"),
            VfsPath::new("/local/blocker/dst.txt"),
        ),
    )
    .with_args(copy_move_args("/local/keep.txt"))
    .irreversible(true);
    let err = applier.apply_shared(&node).unwrap_err();
    assert_eq!(err.code(), "terminal", "a failed copy is a terminal effect");
    assert!(
        dir.path().join("keep.txt").exists(),
        "mv must NOT delete the source when the copy fails to verify/publish"
    );
    assert_eq!(
        fs::read(dir.path().join("keep.txt")).unwrap(),
        b"do-not-lose-me",
        "the preserved source is byte-intact"
    );
}

#[test]
fn applier_move_deletes_source_only_after_verify() {
    let (dir, sandbox) = fixture();
    seed(&dir, "m.txt", b"move-me");
    let applier = LocalApplier::new(sandbox, false);
    let effect = LocalEffect::Move {
        src: "/local/m.txt".to_string(),
        dst: "/local/moved.txt".to_string(),
    };
    // Drive through the SharedApplier surface by building a node.
    use qfs_plan::{DriverId, EffectKind, EffectNode, NodeId, Target, VfsPath};
    let node = EffectNode::new(
        NodeId(0),
        EffectKind::Upsert,
        Target::new(DriverId::new("local"), VfsPath::new("/local/moved.txt")),
    )
    .with_args(copy_move_args("/local/m.txt"))
    .irreversible(true);
    use qfs_runtime::SharedApplier;
    let out = applier.apply_shared(&node).unwrap();
    assert_eq!(out.affected, "move-me".len() as u64);
    assert!(
        !dir.path().join("m.txt").exists(),
        "source removed after verify"
    );
    assert_eq!(fs::read(dir.path().join("moved.txt")).unwrap(), b"move-me");
    // The decoded effect is irreversible (mv).
    assert!(effect.is_irreversible());
}

#[test]
fn read_only_applier_denies_writes_and_touches_no_files() {
    let (dir, sandbox) = fixture();
    let applier = LocalApplier::new(sandbox, true);
    use qfs_plan::{DriverId, EffectKind, EffectNode, NodeId, Target, VfsPath};
    use qfs_runtime::SharedApplier;
    let node = EffectNode::new(
        NodeId(0),
        EffectKind::Upsert,
        Target::new(DriverId::new("local"), VfsPath::new("/local/nope.txt")),
    )
    .with_args(blob_write_args(b"blocked".to_vec()));
    let err = applier.apply_shared(&node).unwrap_err();
    // The discriminant is preserved through the bridge: a read_only write denial is the
    // structured `capability_denied` class, not a generic `terminal` (the audit ledger
    // distinguishes a denial from an escape from a torn copy).
    assert_eq!(err.code(), "capability_denied");
    assert!(
        !dir.path().join("nope.txt").exists(),
        "no file written on denial"
    );
}

#[test]
fn remove_effect_deletes_a_blob() {
    let (dir, sandbox) = fixture();
    seed(&dir, "gone.txt", b"x");
    let applier = LocalApplier::new(sandbox, false);
    use qfs_plan::{DriverId, EffectKind, EffectNode, NodeId, Target, VfsPath};
    use qfs_runtime::SharedApplier;
    let node = EffectNode::new(
        NodeId(0),
        EffectKind::Remove,
        Target::new(DriverId::new("local"), VfsPath::new("/local/gone.txt")),
    );
    assert!(node.irreversible, "REMOVE is inherently irreversible");
    let out = applier.apply_shared(&node).unwrap();
    assert_eq!(out.affected, 1);
    assert!(!dir.path().join("gone.txt").exists());
}

#[test]
fn sandbox_escape_and_capability_denial_map_to_distinct_effect_codes() {
    // The bridge must NOT collapse a sandbox-escape and a capability-denial into the same
    // `terminal` code: the audit ledger has to tell "I tried to escape the sandbox" apart from
    // "I lacked permission". The `From<LocalError> for EffectError` reduction preserves both
    // discriminants.
    use qfs_runtime::EffectError;

    let escape: EffectError =
        crate::error::LocalError::OutsideSandbox("/local/../../etc/passwd".to_string()).into();
    let denial: EffectError = crate::error::LocalError::CapabilityDenied {
        path: "/local/x".to_string(),
        verb: "UPSERT",
    }
    .into();

    assert_eq!(escape.code(), "sandbox_escape");
    assert_eq!(denial.code(), "capability_denied");
    assert_ne!(
        escape.code(),
        denial.code(),
        "the two failure classes must remain distinct through the bridge"
    );
    // Neither is retryable (both are terminal security classes).
    assert!(!escape.is_retryable());
    assert!(!denial.is_retryable());
}

#[test]
fn read_only_capability_denial_through_bridge_is_capability_denied_not_terminal() {
    // End-to-end through the SharedApplier surface: a read_only mount denying a write now
    // surfaces `capability_denied` (the structured discriminant), not a generic `terminal`.
    let (dir, sandbox) = fixture();
    let applier = LocalApplier::new(sandbox, true);
    use qfs_plan::{DriverId, EffectKind, EffectNode, NodeId, Target, VfsPath};
    use qfs_runtime::SharedApplier;
    let node = EffectNode::new(
        NodeId(0),
        EffectKind::Upsert,
        Target::new(DriverId::new("local"), VfsPath::new("/local/nope.txt")),
    )
    .with_args(blob_write_args(b"blocked".to_vec()));
    let err = applier.apply_shared(&node).unwrap_err();
    assert_eq!(err.code(), "capability_denied");
    assert!(!dir.path().join("nope.txt").exists());
}

#[test]
fn blob_decoded_via_codec_becomes_rows() {
    // A local .json blob → bytes → decoded relation through a registered codec (RFD §4).
    let (dir, sandbox) = fixture();
    seed(&dir, "people.json", br#"{"name":"ada","age":36}"#);
    let bytes = fs_core::read_blob(&sandbox, "/local/people.json").unwrap();
    let codec = qfs_codec::JsonCodec;
    use qfs_codec::Codec;
    let batch = codec.decode(&bytes).unwrap();
    assert_eq!(batch.rows.len(), 1);
    // The driver holds no codec code; it only supplied the bytes.
    assert!(batch.schema.column("name").is_some());
}

#[test]
fn driver_id_is_local() {
    let (dir, _) = fixture();
    let d = LocalFsDriver::new(dir.path());
    assert_eq!(d.id(), qfs_types::DriverId::new("local"));
}

#[test]
fn applier_seam_is_reachable_through_driver() {
    let (dir, _) = fixture();
    let d = LocalFsDriver::new(dir.path());
    // The contract's synchronous applier() seam exists (PlanApplier).
    let _seam: &dyn qfs_plan::PlanApplier = d.applier();
}
