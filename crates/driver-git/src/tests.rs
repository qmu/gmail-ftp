//! Internal tests over a **committed fixture repository** (built in-memory from owned object
//! DTOs — no live creds, no network). The fixture's objects are content-addressed by the same
//! in-house SHA-1 git uses, and one test differentially checks our empty-blob/oid framing against
//! the canonical git value, so the reader is pinned to real git output (ADR-0003).
//!
//! Coverage (the ticket's internal-QA list): blob cat-at-ref, tree ls, md-through-codec
//! frontmatter columns, commits WHERE/ORDER/LIMIT, commits JOIN changes, blame, INSERT INTO
//! commits PREVIEW→plan(applies nothing)→COMMIT moves branch + reflog entry, stale-oid CAS
//! rejection, conflicting git.merge plan-build error with zero effects, parse-time capability
//! rejection, forced-ref-move reflog recovery, DESCRIBE golden.

use std::collections::BTreeMap;
use std::sync::Arc;

use cfs_codec::MarkdownFrontmatterCodec;
use cfs_driver::{check_capability, resolve_proc, Driver, Path, Verb};
use cfs_plan::EffectKind;
use cfs_types::Value;

use crate::applier::RepoStore;
use crate::objectdb::{frame_and_id, LooseObjectDb, ObjectKind, Oid, Tree, TreeEntry};
use crate::planner::CommitInput;
use crate::{
    blobfs, plan_insert_commit, plan_merge, plan_update_ref, relational, GitApplier, GitDriver,
    Repo, RepoResolver,
};

/// Build the committed fixture repo + the matching apply-leg store, returning a fully-wired
/// [`GitDriver`] plus the key oids the tests assert on. The repo has two flat-tree commits on
/// `refs/heads/main` (c1 → c2), a `README.md` with YAML frontmatter, and a tag.
struct Fixture {
    driver: GitDriver,
    c1: Oid,
    c2: Oid,
}

/// Insert a blob and return its oid + a tree entry.
fn blob_entry(db: &mut LooseObjectDb, name: &str, bytes: &[u8]) -> TreeEntry {
    let oid = db.insert_object(ObjectKind::Blob, bytes);
    TreeEntry {
        mode: "100644".to_string(),
        name: name.to_string(),
        oid,
    }
}

fn build_fixture() -> Fixture {
    let mut db = LooseObjectDb::new();

    // --- Commit 1: README.md (with frontmatter) + src/main.rs equivalent flat file ---
    let readme_v1 = b"---\ntitle: Fixture\nversion: 1\n---\n# Hello\n\nFirst body.\n";
    let main_v1 = b"fn main() { println!(\"v1\"); }\n";
    let e_readme1 = blob_entry(&mut db, "README.md", readme_v1);
    let e_main1 = blob_entry(&mut db, "main.rs", main_v1);
    let mut entries1 = vec![e_readme1, e_main1];
    entries1.sort_by(|a, b| a.name.cmp(&b.name));
    let tree1 = Tree { entries: entries1 };
    let tree1_payload = crate::objectdb::serialize_tree(&tree1);
    let tree1_oid = db.insert_object(ObjectKind::Tree, &tree1_payload);

    let commit1_payload = format!(
        "tree {}\nauthor Alice <alice@example.com> 1700000000 +0000\ncommitter Alice <alice@example.com> 1700000000 +0000\n\nInitial commit\n",
        tree1_oid.as_str()
    );
    let c1 = db.insert_object(ObjectKind::Commit, commit1_payload.as_bytes());

    // --- Commit 2: README unchanged, main.rs modified ---
    let main_v2 = b"fn main() { println!(\"v2\"); }\n";
    let e_readme2 = blob_entry(&mut db, "README.md", readme_v1);
    let e_main2 = blob_entry(&mut db, "main.rs", main_v2);
    let mut entries2 = vec![e_readme2, e_main2];
    entries2.sort_by(|a, b| a.name.cmp(&b.name));
    let tree2 = Tree { entries: entries2 };
    let tree2_payload = crate::objectdb::serialize_tree(&tree2);
    let tree2_oid = db.insert_object(ObjectKind::Tree, &tree2_payload);

    let commit2_payload = format!(
        "tree {}\nparent {}\nauthor Bob <bob@example.com> 1700000200 +0000\ncommitter Bob <bob@example.com> 1700000200 +0000\n\nSecond commit\n",
        tree2_oid.as_str(),
        c1.as_str()
    );
    let c2 = db.insert_object(ObjectKind::Commit, commit2_payload.as_bytes());

    let db = Arc::new(db);

    // Read-side repo.
    let mut repo = Repo::new(db.clone() as Arc<dyn crate::objectdb::ObjectDb>);
    repo.set_ref("refs/heads/main", c2.clone());
    repo.set_ref("refs/tags/v1", c1.clone());
    repo.set_symbolic("HEAD", "refs/heads/main");
    repo.append_reflog(crate::ReflogEntry {
        ref_name: "refs/heads/main".to_string(),
        old: zero(),
        new: c1.clone(),
        who: "Alice <alice@example.com>".to_string(),
        message: "commit (initial): Initial commit".to_string(),
        time: 1_700_000_000,
    });
    repo.append_reflog(crate::ReflogEntry {
        ref_name: "refs/heads/main".to_string(),
        old: c1.clone(),
        new: c2.clone(),
        who: "Bob <bob@example.com>".to_string(),
        message: "commit: Second commit".to_string(),
        time: 1_700_000_200,
    });

    let resolver = RepoResolver::new().with_repo("fixture", repo);

    // Apply-side store: seed the same objects + refs so a COMMIT mutates from the real start state.
    let mut refs = std::collections::HashMap::new();
    refs.insert("refs/heads/main".to_string(), c2.clone());
    refs.insert("refs/tags/v1".to_string(), c1.clone());
    let store = RepoStore {
        db: LooseObjectDb::clone_of(&db),
        refs,
        reflog: std::collections::HashMap::new(),
    };
    let applier = GitApplier::new().with_store("fixture", store);

    Fixture {
        driver: GitDriver::new(resolver, applier),
        c1,
        c2,
    }
}

fn zero() -> Oid {
    Oid::parse(&"0".repeat(40)).unwrap()
}

// ---------------------------------------------------------------------------------------------
// BlobFs (Blob archetype)
// ---------------------------------------------------------------------------------------------

#[test]
fn blob_cat_at_ref_returns_exact_bytes() {
    let fx = build_fixture();
    let repo = fx.driver.repos().repo("fixture").unwrap();
    // main.rs differs between c1 and c2 — cat at each ref returns that ref's exact bytes.
    let v2 = blobfs::cat(repo, "main", "main.rs").unwrap();
    assert_eq!(v2, b"fn main() { println!(\"v2\"); }\n");
    let v1 = blobfs::cat(repo, fx.c1.as_str(), "main.rs").unwrap();
    assert_eq!(v1, b"fn main() { println!(\"v1\"); }\n");
    // The tag ref resolves to c1.
    let via_tag = blobfs::cat(repo, "v1", "main.rs").unwrap();
    assert_eq!(via_tag, b"fn main() { println!(\"v1\"); }\n");
}

#[test]
fn tree_ls_lists_entries() {
    let fx = build_fixture();
    let repo = fx.driver.repos().repo("fixture").unwrap();
    let batch = blobfs::ls(repo, "main", "").unwrap();
    let names: Vec<String> = batch
        .rows
        .iter()
        .filter_map(|r| match r.values.first() {
            Some(Value::Text(t)) => Some(t.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(names, vec!["README.md".to_string(), "main.rs".to_string()]);
}

#[test]
fn md_through_codec_yields_frontmatter_columns_and_body() {
    let fx = build_fixture();
    let repo = fx.driver.repos().repo("fixture").unwrap();
    let codec = MarkdownFrontmatterCodec;
    let batch = blobfs::cat_decode(repo, "main", "README.md", &codec).unwrap();
    // Frontmatter keys become columns; the markdown body is the `body` column.
    let col_names: Vec<&str> = batch
        .schema
        .columns
        .iter()
        .map(|c| c.name.as_str())
        .collect();
    assert!(col_names.contains(&"title"), "columns: {col_names:?}");
    assert!(col_names.contains(&"version"));
    assert!(col_names.contains(&"body"));
    // The body column carries the markdown content.
    let body_idx = batch
        .schema
        .columns
        .iter()
        .position(|c| c.name == "body")
        .unwrap();
    let body = match &batch.rows[0].values[body_idx] {
        Value::Text(t) => t.clone(),
        other => panic!("body not text: {other:?}"),
    };
    assert!(body.contains("# Hello"), "body: {body}");
}

// ---------------------------------------------------------------------------------------------
// Relational (Commits / Changes / Blame / Refs / Tags)
// ---------------------------------------------------------------------------------------------

#[test]
fn commits_walk_where_order_limit() {
    let fx = build_fixture();
    let repo = fx.driver.repos().repo("fixture").unwrap();
    let rows = relational::commits(repo, "main", 10).unwrap();
    // Newest-first revwalk: c2 then c1.
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].sha, fx.c2.as_str());
    assert_eq!(rows[1].sha, fx.c1.as_str());
    assert!(
        rows[0].time >= rows[1].time,
        "ORDER BY time desc (revwalk order)"
    );
    // LIMIT 1 bounds the walk.
    let one = relational::commits(repo, "main", 1).unwrap();
    assert_eq!(one.len(), 1);
    assert_eq!(one[0].sha, fx.c2.as_str());
    // WHERE author = 'Bob …' residual: c2's author is Bob.
    let bob: Vec<_> = rows.iter().filter(|r| r.author.contains("Bob")).collect();
    assert_eq!(bob.len(), 1);
    assert_eq!(bob[0].sha, fx.c2.as_str());
}

#[test]
fn commits_join_changes_per_file_rows() {
    let fx = build_fixture();
    let repo = fx.driver.repos().repo("fixture").unwrap();
    let commits = relational::commits(repo, "main", 10).unwrap();
    let changes = relational::changes(repo, "main", 10).unwrap();
    // c2 modified main.rs (M); c1 added both files (A).
    let c2_changes: Vec<_> = changes.iter().filter(|c| c.sha == fx.c2.as_str()).collect();
    assert_eq!(c2_changes.len(), 1);
    assert_eq!(c2_changes[0].path, "main.rs");
    assert_eq!(c2_changes[0].status, "M");
    // The JOIN: every change row's sha matches a commit row's sha.
    for ch in &changes {
        assert!(
            commits.iter().any(|c| c.sha == ch.sha),
            "change row {ch:?} has no matching commit (JOIN failed)"
        );
    }
}

#[test]
fn blame_attributes_lines() {
    let fx = build_fixture();
    let repo = fx.driver.repos().repo("fixture").unwrap();
    let rows = blobfs::blame(repo, "main", "main.rs", 10).unwrap();
    assert_eq!(rows.len(), 1, "main.rs has one line");
    assert_eq!(rows[0].line, 1);
    // The last touch of main.rs is c2 (Bob).
    assert_eq!(rows[0].sha, fx.c2.as_str());
    assert!(rows[0].author.contains("Bob"));
}

#[test]
fn refs_and_tags_rows() {
    let fx = build_fixture();
    let repo = fx.driver.repos().repo("fixture").unwrap();
    let refs = relational::refs(repo);
    assert!(refs
        .iter()
        .any(|r| r.name == "refs/heads/main" && r.oid == fx.c2.as_str()));
    let tags = relational::tags(repo);
    assert_eq!(tags.len(), 1);
    assert_eq!(tags[0].name, "refs/tags/v1");
    assert_eq!(tags[0].oid, fx.c1.as_str());
}

// ---------------------------------------------------------------------------------------------
// Reflog (Append/log) — the recovery oracle
// ---------------------------------------------------------------------------------------------

#[test]
fn reflog_tail_read_newest_first() {
    let fx = build_fixture();
    let repo = fx.driver.repos().repo("fixture").unwrap();
    let rows = relational::reflog(repo, "refs/heads/main");
    assert_eq!(rows.len(), 2);
    // Newest first: the c1→c2 move is first.
    assert_eq!(rows[0].new, fx.c2.as_str());
    assert_eq!(rows[0].old, fx.c1.as_str());
}

// ---------------------------------------------------------------------------------------------
// Write planning (pure plans) + COMMIT
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn insert_commit_preview_applies_nothing_then_commit_moves_branch_and_reflog() {
    let fx = build_fixture();
    let repo = fx.driver.repos().repo("fixture").unwrap();

    let mut files = BTreeMap::new();
    files.insert("NEW.txt".to_string(), b"new file\n".to_vec());
    let input = CommitInput {
        branch: "refs/heads/main".to_string(),
        author: "Carol <carol@example.com>".to_string(),
        committer: "Carol <carol@example.com>".to_string(),
        time: 1_700_000_300,
        message: "Third commit".to_string(),
        files,
    };
    let planned = plan_insert_commit("fixture", repo, &input).unwrap();

    // PREVIEW: the plan is a DAG of WriteLooseObject + UpdateRef effects, CAS old = c2, and it
    // applied NOTHING (the branch still points at c2, no new reflog entry yet).
    assert!(
        planned.plan.nodes().len() >= 4,
        "blob+tree+commit+ref(+reflog)"
    );
    assert_eq!(
        planned.old_commit.as_ref().unwrap().as_str(),
        fx.c2.as_str()
    );
    assert_eq!(
        fx.driver
            .git_applier()
            .ref_oid("fixture", "refs/heads/main")
            .unwrap()
            .as_str(),
        fx.c2.as_str(),
        "PREVIEW applies nothing — branch unchanged"
    );

    // COMMIT: drive the applier directly (the same SharedApplier the bridge runs).
    use cfs_runtime::SharedApplier;
    let applier = fx.driver.git_applier().clone();
    // Topo order: blobs, tree, commit, ref, reflog — apply in node order (deps already satisfied).
    for node in planned.plan.nodes() {
        applier.apply_shared(node).unwrap();
    }
    // The branch now points at the new commit; a reflog entry exists.
    assert_eq!(
        applier.ref_oid("fixture", "refs/heads/main").unwrap(),
        planned.new_commit
    );
    let reflog = applier.reflog("fixture", "refs/heads/main");
    assert_eq!(reflog.first().unwrap().new, planned.new_commit);
    assert_eq!(reflog.first().unwrap().old, fx.c2);
}

#[test]
fn stale_old_oid_cas_is_rejected_not_clobbered() {
    use cfs_runtime::SharedApplier;
    let fx = build_fixture();
    let applier = fx.driver.git_applier().clone();
    // Attempt to move main with a STALE old oid (c1, but main is at c2). CAS must reject.
    let stale_plan = plan_update_ref(
        "fixture",
        "refs/heads/main",
        Some(fx.c1.clone()), // stale: main is actually at c2
        fx.c1.clone(),
        false,
        "attacker",
    );
    // The UpdateRef node is the first node.
    let ref_node = stale_plan
        .nodes()
        .iter()
        .find(|n| matches!(n.kind, EffectKind::Update))
        .unwrap();
    let err = applier.apply_shared(ref_node).unwrap_err();
    assert_eq!(err.code(), "conflict");
    // The branch was NOT clobbered — still at c2.
    assert_eq!(
        applier.ref_oid("fixture", "refs/heads/main").unwrap(),
        fx.c2
    );
}

#[test]
fn conflicting_merge_is_plan_build_error_with_zero_effects() {
    let fx = build_fixture();
    // base = c1; ours = c2 (main.rs=v2); theirs = c3 (main.rs=v3) — both changed main.rs vs base
    // to different content → conflict. A unified repo holds all three commits' objects.
    let (unified, c3) = build_unified_repo(&fx);
    let err = plan_merge(
        "fixture",
        &unified,
        "refs/heads/main",
        &fx.c1, // base
        &fx.c2, // ours (main.rs = v2)
        &c3,    // theirs (main.rs = v3)
        "merger",
    )
    .unwrap_err();
    assert_eq!(err.code(), "merge_conflict");
}

/// Build a repo that can read c1, c2, and a divergent c3 (so plan_merge can three-way-merge),
/// returning the repo + c3's oid.
fn build_unified_repo(fx: &Fixture) -> (Repo, Oid) {
    let main_v3 = b"fn main() { println!(\"v3-divergent\"); }\n".as_slice();
    let readme = b"---\ntitle: Fixture\nversion: 1\n---\n# Hello\n\nFirst body.\n".as_slice();
    // Reconstruct the full object set in one db (the fixture's two commits + c3's chain).
    let mut db = LooseObjectDb::new();
    // c1 tree (README v1 + main v1).
    let main_v1 = b"fn main() { println!(\"v1\"); }\n";
    let main_v2 = b"fn main() { println!(\"v2\"); }\n";
    let e = |db: &mut LooseObjectDb, n: &str, b: &[u8]| TreeEntry {
        mode: "100644".into(),
        name: n.into(),
        oid: db.insert_object(ObjectKind::Blob, b),
    };
    let mut t1 = vec![
        e(&mut db, "README.md", readme),
        e(&mut db, "main.rs", main_v1),
    ];
    t1.sort_by(|a, b| a.name.cmp(&b.name));
    let t1o = db.insert_object(
        ObjectKind::Tree,
        &crate::objectdb::serialize_tree(&Tree { entries: t1 }),
    );
    let c1p = format!("tree {}\nauthor Alice <alice@example.com> 1700000000 +0000\ncommitter Alice <alice@example.com> 1700000000 +0000\n\nInitial commit\n", t1o.as_str());
    let c1 = db.insert_object(ObjectKind::Commit, c1p.as_bytes());
    assert_eq!(c1, fx.c1, "c1 oid must reproduce");

    let mut t2 = vec![
        e(&mut db, "README.md", readme),
        e(&mut db, "main.rs", main_v2),
    ];
    t2.sort_by(|a, b| a.name.cmp(&b.name));
    let t2o = db.insert_object(
        ObjectKind::Tree,
        &crate::objectdb::serialize_tree(&Tree { entries: t2 }),
    );
    let c2p = format!("tree {}\nparent {}\nauthor Bob <bob@example.com> 1700000200 +0000\ncommitter Bob <bob@example.com> 1700000200 +0000\n\nSecond commit\n", t2o.as_str(), c1.as_str());
    let c2 = db.insert_object(ObjectKind::Commit, c2p.as_bytes());
    assert_eq!(c2, fx.c2);

    let mut t3 = vec![
        e(&mut db, "README.md", readme),
        e(&mut db, "main.rs", main_v3),
    ];
    t3.sort_by(|a, b| a.name.cmp(&b.name));
    let t3o = db.insert_object(
        ObjectKind::Tree,
        &crate::objectdb::serialize_tree(&Tree { entries: t3 }),
    );
    let c3p = format!("tree {}\nparent {}\nauthor Dave <dave@e.com> 1700000400 +0000\ncommitter Dave <dave@e.com> 1700000400 +0000\n\nDivergent\n", t3o.as_str(), c1.as_str());
    let c3v = db.insert_object(ObjectKind::Commit, c3p.as_bytes());

    (
        Repo::new(Arc::new(db) as Arc<dyn crate::objectdb::ObjectDb>),
        c3v,
    )
}

#[test]
fn clean_merge_produces_effect_dag() {
    // base = c1, ours = c1 (unchanged), theirs = c2 (main.rs modified) → no conflict; theirs wins.
    let fx = build_fixture();
    let repo = fx.driver.repos().repo("fixture").unwrap();
    let plan = plan_merge(
        "fixture",
        repo,
        "refs/heads/main",
        &fx.c1, // base
        &fx.c1, // ours (unchanged from base)
        &fx.c2, // theirs (changed)
        "merger",
    )
    .unwrap();
    // Clean merge: a tree + merge-commit WriteLooseObject + an UpdateRef + a reflog entry.
    assert!(plan.nodes().len() >= 3);
    assert!(plan
        .nodes()
        .iter()
        .any(|n| matches!(n.kind, EffectKind::Update)));
}

// ---------------------------------------------------------------------------------------------
// Capability gating (parse time) + procedures
// ---------------------------------------------------------------------------------------------

#[test]
fn update_on_commits_is_rejected_at_parse_time() {
    let fx = build_fixture();
    let path = Path::new("/git/fixture/commits");
    // UPDATE /commits is structurally rejected (commits = {SELECT, INSERT}).
    let err = check_capability(&fx.driver, &path, Verb::Update).unwrap_err();
    assert_eq!(err.code(), "unsupported_verb");
    // INSERT (= make a commit, the keyword-clash-free path) and SELECT are allowed.
    assert!(check_capability(&fx.driver, &path, Verb::Insert).is_ok());
    assert!(check_capability(&fx.driver, &path, Verb::Select).is_ok());
    // REMOVE is also rejected.
    assert!(check_capability(&fx.driver, &path, Verb::Remove).is_err());
}

#[test]
fn refs_node_allows_update_but_blob_is_read_only() {
    let fx = build_fixture();
    let refs = Path::new("/git/fixture/refs");
    assert!(check_capability(&fx.driver, &refs, Verb::Update).is_ok());
    let blob = Path::new("/git/fixture/README.md");
    assert!(check_capability(&fx.driver, &blob, Verb::Select).is_ok());
    assert!(check_capability(&fx.driver, &blob, Verb::Update).is_err());
    assert!(check_capability(&fx.driver, &blob, Verb::Insert).is_err());
}

#[test]
fn call_resolves_only_declared_git_procedures() {
    let fx = build_fixture();
    for name in ["merge", "rebase", "checkout", "tag"] {
        let p = resolve_proc(&fx.driver, name).unwrap();
        assert!(!p.irreversible, "git procedures are reflog-recoverable");
    }
    let err = resolve_proc(&fx.driver, "force_push").unwrap_err();
    assert_eq!(err.code(), "unknown_procedure");
}

// ---------------------------------------------------------------------------------------------
// Forced-ref-move recovery (the reflog recovery helper)
// ---------------------------------------------------------------------------------------------

#[test]
fn forced_ref_move_is_reflog_recoverable() {
    use cfs_runtime::SharedApplier;
    let fx = build_fixture();
    let applier = fx.driver.git_applier().clone();
    // Force main back to c1 (orphaning c2). A forced move skips the CAS check.
    let plan = plan_update_ref(
        "fixture",
        "refs/heads/main",
        Some(fx.c2.clone()),
        fx.c1.clone(),
        true, // force
        "Eve <eve@e.com>",
    );
    for node in plan.nodes() {
        applier.apply_shared(node).unwrap();
    }
    assert_eq!(
        applier.ref_oid("fixture", "refs/heads/main").unwrap(),
        fx.c1
    );
    // The reflog shows the prior oid (c2) for the forced move.
    let reflog = applier.reflog("fixture", "refs/heads/main");
    assert_eq!(reflog.first().unwrap().old, fx.c2);
    // The recovery helper restores c2 from the reflog.
    let restored = applier.recover_ref("fixture", "refs/heads/main").unwrap();
    assert_eq!(restored, fx.c2);
    assert_eq!(
        applier.ref_oid("fixture", "refs/heads/main").unwrap(),
        fx.c2
    );
}

// ---------------------------------------------------------------------------------------------
// DESCRIBE golden
// ---------------------------------------------------------------------------------------------

#[test]
fn describe_golden_per_archetype() {
    let fx = build_fixture();
    // commits → relational.
    let commits = fx
        .driver
        .describe(&Path::new("/git/fixture/commits"))
        .unwrap();
    assert_eq!(commits.archetype, cfs_driver::Archetype::RelationalTable);
    assert!(commits.schema.column("sha").is_some());
    assert!(commits.schema.column("message").is_some());
    // reflog → append log.
    let reflog = fx
        .driver
        .describe(&Path::new("/git/fixture/reflog"))
        .unwrap();
    assert_eq!(reflog.archetype, cfs_driver::Archetype::AppendLog);
    // blob → blob namespace.
    let blob = fx
        .driver
        .describe(&Path::new("/git/fixture/README.md"))
        .unwrap();
    assert_eq!(blob.archetype, cfs_driver::Archetype::BlobNamespace);
    // refs → relational.
    let refs = fx.driver.describe(&Path::new("/git/fixture/refs")).unwrap();
    assert_eq!(refs.archetype, cfs_driver::Archetype::RelationalTable);

    // The full DESCRIBE JSON snapshot of the commits node is stable.
    let json = serde_json::to_string_pretty(&commits).unwrap();
    assert!(json.contains("\"archetype\": \"relational_table\""));
    assert!(json.contains("\"name\": \"sha\""));

    // version_support: blob/commits are Versioned (@ref), reflog is Snapshot.
    assert_eq!(
        fx.driver
            .version_support(&Path::new("/git/fixture/README.md")),
        cfs_driver::VersionSupport::Versioned
    );
    assert_eq!(
        fx.driver.version_support(&Path::new("/git/fixture/reflog")),
        cfs_driver::VersionSupport::Snapshot
    );
}

// ---------------------------------------------------------------------------------------------
// Differential check against real git framing + the runtime bridge wiring
// ---------------------------------------------------------------------------------------------

#[test]
fn empty_blob_oid_matches_canonical_git() {
    // The in-house framing + SHA-1 reproduces git's canonical empty-blob oid (ADR-0003
    // differential guard).
    let (oid, framed) = frame_and_id(ObjectKind::Blob, b"");
    assert_eq!(oid.as_str(), "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391");
    assert_eq!(framed, b"blob 0\0");
}

#[test]
fn git_apply_driver_builds_the_runtime_bridge() {
    let fx = build_fixture();
    // The locked driver pattern: the bridge wraps the synchronous applier for the interpreter.
    let _bridge = crate::git_apply_driver(&fx.driver);
}
