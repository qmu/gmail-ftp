//! Planner-owned **E2E / external-interface** harness for the t26 git driver (black box).
//!
//! This is NOT the Constructor's internal unit suite (`src/tests.rs`). It drives the driver's
//! **public crate surface** only — `GitDriver`, `Repo`/`RepoResolver`, `GitApplier`/`RepoStore`,
//! `LooseObjectDb`, the `blobfs`/`relational` read modules, the `plan_*` write-plan builders, the
//! declared `CALL git.*` procedures, the runtime bridge, and the `cfs_driver` resolve-time gates
//! (`check_capability` / `resolve_proc`) — from the outside, over a Planner-built fixture repo.
//!
//! No live network, no creds. The relational/write fixture is built in-memory from owned object
//! DTOs (a local committed-style fixture). The bonus scenario (`real_git_loose_object_inflates_*`)
//! shells out to the **local `git` binary** in a tempdir to produce CANONICAL loose objects, then
//! reads those exact compressed bytes back through the public `Repo`/`blobfs::cat` surface —
//! confirming the in-house DEFLATE inflate path works on real git output end-to-end. That is a
//! local tool over a local tempdir; still no network and no creds.
//!
//! Scenario map (→ ticket acceptance criteria):
//!  1. BlobFs read at a ref (exact bytes / ls / md-through-codec)            — `blobfs_*`
//!  2. Relational (commits WHERE/ORDER/LIMIT, commits JOIN changes, blame)  — `relational_*`
//!  3. Write plans PREVIEW vs COMMIT (INSERT INTO /commits)                 — `write_*`
//!  4. CAS ref update — stale old-oid rejected, not clobbered               — `cas_*`
//!  5. merge-conflict purity (zero effects) + clean-merge DAG              — `merge_*`
//!  6. Capability gating at PARSE/resolve time                              — `capability_*`
//!  7. Reflog recovery of a forced ref move                                 — `reflog_*`
//!  8. COMMIT keyword-clash: commit creation is INSERT, never the COMMIT kw — `keyword_clash_*`
//!  9. (Bonus) real `git` loose object inflated through the public surface  — `real_git_*`

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::HashMap;
use std::sync::Arc;

use cfs_codec::MarkdownFrontmatterCodec;
use cfs_driver::{check_capability, resolve_proc, Driver, Path, Verb};
use cfs_plan::EffectKind;
use cfs_runtime::SharedApplier;
use cfs_types::Value;

use cfs_driver_git::{
    blobfs, git_apply_driver, plan_checkout, plan_insert_commit, plan_merge, plan_rebase, plan_tag,
    plan_update_ref, relational, CommitInput, GitApplier, GitDriver, LooseObjectDb, ObjectDb,
    ObjectKind, Oid, Repo, RepoResolver, RepoStore, Tree, TreeEntry,
};

// =================================================================================================
// Planner fixture — built entirely through the PUBLIC crate surface (no `src/tests.rs` reuse).
// =================================================================================================

/// The handles a built fixture exposes to a scenario.
struct Fx {
    driver: GitDriver,
    c1: Oid,
    c2: Oid,
}

impl Fx {
    fn repo(&self) -> &Repo {
        self.driver.repos().repo("demo").unwrap()
    }
}

fn blob_entry(db: &mut LooseObjectDb, name: &str, bytes: &[u8]) -> TreeEntry {
    let oid = db.insert_object(ObjectKind::Blob, bytes);
    TreeEntry {
        mode: "100644".to_string(),
        name: name.to_string(),
        oid,
    }
}

fn tree_oid(db: &mut LooseObjectDb, mut entries: Vec<TreeEntry>) -> Oid {
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    let payload = cfs_driver_git::serialize_tree(&Tree { entries });
    db.insert_object(ObjectKind::Tree, &payload)
}

/// Two flat-tree commits on `refs/heads/main` (c1 → c2): README.md carries YAML frontmatter
/// (unchanged across both), config.toml is added in c1 and modified in c2. A `refs/tags/rel-1`
/// lightweight tag points at c1. The read repo + the apply store are seeded from the SAME objects.
fn build_fixture() -> Fx {
    let readme = b"---\ntitle: Demo Repo\nversion: 2\n---\n# Demo\n\nThe body text.\n";
    let cfg_v1 = b"name = \"demo\"\nport = 8080\n";
    let cfg_v2 = b"name = \"demo\"\nport = 9090\n";

    let mut db = LooseObjectDb::new();

    // c1: README.md + config.toml(v1).
    let e_readme = blob_entry(&mut db, "README.md", readme);
    let e_cfg1 = blob_entry(&mut db, "config.toml", cfg_v1);
    let t1 = tree_oid(&mut db, vec![e_readme.clone(), e_cfg1]);
    let c1_payload = format!(
        "tree {}\nauthor Ada Lovelace <ada@demo.test> 1700000000 +0000\ncommitter Ada Lovelace <ada@demo.test> 1700000000 +0000\n\nInitial import\n",
        t1.as_str()
    );
    let c1 = db.insert_object(ObjectKind::Commit, c1_payload.as_bytes());

    // c2: README.md unchanged + config.toml(v2).
    let e_cfg2 = blob_entry(&mut db, "config.toml", cfg_v2);
    let t2 = tree_oid(&mut db, vec![e_readme, e_cfg2]);
    let c2_payload = format!(
        "tree {}\nparent {}\nauthor Grace Hopper <grace@demo.test> 1700000600 +0000\ncommitter Grace Hopper <grace@demo.test> 1700000600 +0000\n\nBump config port\n",
        t2.as_str(),
        c1.as_str()
    );
    let c2 = db.insert_object(ObjectKind::Commit, c2_payload.as_bytes());

    let db = Arc::new(db);

    // Read-side repo.
    let mut repo = Repo::new(db.clone() as Arc<dyn ObjectDb>);
    repo.set_ref("refs/heads/main", c2.clone());
    repo.set_ref("refs/tags/rel-1", c1.clone());
    repo.set_symbolic("HEAD", "refs/heads/main");
    repo.append_reflog(cfs_driver_git::ReflogEntry {
        ref_name: "refs/heads/main".to_string(),
        old: Oid::zero(),
        new: c1.clone(),
        who: "Ada Lovelace <ada@demo.test>".to_string(),
        message: "commit (initial): Initial import".to_string(),
        time: 1_700_000_000,
    });
    repo.append_reflog(cfs_driver_git::ReflogEntry {
        ref_name: "refs/heads/main".to_string(),
        old: c1.clone(),
        new: c2.clone(),
        who: "Grace Hopper <grace@demo.test>".to_string(),
        message: "commit: Bump config port".to_string(),
        time: 1_700_000_600,
    });

    let resolver = RepoResolver::new().with_repo("demo", repo);

    // Apply-side store: same starting objects + refs so a COMMIT mutates from the real start state.
    let mut refs = HashMap::new();
    refs.insert("refs/heads/main".to_string(), c2.clone());
    refs.insert("refs/tags/rel-1".to_string(), c1.clone());
    let store = RepoStore {
        db: LooseObjectDb::clone_of(&db),
        refs,
        reflog: HashMap::new(),
    };
    let applier = GitApplier::new().with_store("demo", store);

    Fx {
        driver: GitDriver::new(resolver, applier),
        c1,
        c2,
    }
}

// =================================================================================================
// 1. BlobFs read at a ref
// =================================================================================================

#[test]
fn blobfs_cat_at_ref_returns_exact_bytes() {
    let fx = build_fixture();
    let repo = fx.repo();
    // config.toml differs across c1/c2 — cat AT each ref returns that ref's exact committed bytes.
    let at_head = blobfs::cat(repo, "main", "config.toml").unwrap();
    assert_eq!(at_head, b"name = \"demo\"\nport = 9090\n", "HEAD = c2 = v2");

    let at_c1 = blobfs::cat(repo, fx.c1.as_str(), "config.toml").unwrap();
    assert_eq!(at_c1, b"name = \"demo\"\nport = 8080\n", "@c1-sha = v1");

    // The tag ref resolves to c1.
    let at_tag = blobfs::cat(repo, "rel-1", "config.toml").unwrap();
    assert_eq!(
        at_tag, b"name = \"demo\"\nport = 8080\n",
        "@rel-1 tag = c1 = v1"
    );

    // The @ref~1 ancestor coordinate (HEAD~1 = c1).
    let at_parent = blobfs::cat(repo, "main~1", "config.toml").unwrap();
    assert_eq!(at_parent, b"name = \"demo\"\nport = 8080\n", "main~1 = c1");
}

#[test]
fn blobfs_ls_lists_tree_entries() {
    let fx = build_fixture();
    let batch = blobfs::ls(fx.repo(), "main", "").unwrap();
    let names: Vec<String> = batch
        .rows
        .iter()
        .filter_map(|r| match r.values.first() {
            Some(Value::Text(t)) => Some(t.clone()),
            _ => None,
        })
        .collect();
    // git stores tree entries name-sorted.
    assert_eq!(
        names,
        vec!["README.md".to_string(), "config.toml".to_string()]
    );
}

#[test]
fn blobfs_md_through_codec_registry_yields_frontmatter_columns_and_body() {
    let fx = build_fixture();
    let codec = MarkdownFrontmatterCodec;
    let batch = blobfs::cat_decode(fx.repo(), "main", "README.md", &codec).unwrap();
    let cols: Vec<&str> = batch
        .schema
        .columns
        .iter()
        .map(|c| c.name.as_str())
        .collect();
    assert!(cols.contains(&"title"), "frontmatter key `title`: {cols:?}");
    assert!(cols.contains(&"version"), "frontmatter key `version`");
    assert!(cols.contains(&"body"), "markdown body column");

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
    assert!(body.contains("# Demo"), "body carries markdown: {body}");
    assert!(body.contains("The body text."));
}

// =================================================================================================
// 2. Relational
// =================================================================================================

#[test]
fn relational_commits_where_order_limit() {
    let fx = build_fixture();
    let repo = fx.repo();
    // ORDER BY time desc is the natural newest-first revwalk order; c2 then c1.
    let rows = relational::commits(repo, "main", 10).unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].sha, fx.c2.as_str());
    assert_eq!(rows[1].sha, fx.c1.as_str());
    assert!(rows[0].time >= rows[1].time, "newest-first ORDER BY time");

    // LIMIT 1 bounds the walk to the tip.
    let limited = relational::commits(repo, "main", 1).unwrap();
    assert_eq!(limited.len(), 1);
    assert_eq!(limited[0].sha, fx.c2.as_str());

    // WHERE author = 'Grace …' truthful residual: only c2 matches.
    let by_grace: Vec<_> = rows.iter().filter(|r| r.author.contains("Grace")).collect();
    assert_eq!(by_grace.len(), 1);
    assert_eq!(by_grace[0].sha, fx.c2.as_str());
    // WHERE author = 'Ada …' → only c1.
    let by_ada: Vec<_> = rows.iter().filter(|r| r.author.contains("Ada")).collect();
    assert_eq!(by_ada.len(), 1);
    assert_eq!(by_ada[0].sha, fx.c1.as_str());
}

#[test]
fn relational_commits_join_changes_per_file_rows() {
    let fx = build_fixture();
    let repo = fx.repo();
    let commits = relational::commits(repo, "main", 10).unwrap();
    let changes = relational::changes(repo, "main", 10).unwrap();

    // c2 modified config.toml only (README unchanged).
    let c2_changes: Vec<_> = changes.iter().filter(|c| c.sha == fx.c2.as_str()).collect();
    assert_eq!(c2_changes.len(), 1, "c2 changed exactly one path");
    assert_eq!(c2_changes[0].path, "config.toml");
    assert_eq!(c2_changes[0].status, "M");

    // c1 (root) ADDED both files.
    let c1_changes: Vec<_> = changes.iter().filter(|c| c.sha == fx.c1.as_str()).collect();
    let mut c1_paths: Vec<&str> = c1_changes.iter().map(|c| c.path.as_str()).collect();
    c1_paths.sort_unstable();
    assert_eq!(c1_paths, vec!["README.md", "config.toml"]);
    assert!(c1_changes.iter().all(|c| c.status == "A"));

    // The JOIN commits ⋈ changes ON sha: every change row joins to a commit row.
    for ch in &changes {
        assert!(
            commits.iter().any(|c| c.sha == ch.sha),
            "change {ch:?} has no matching commit (JOIN integrity)"
        );
    }
}

#[test]
fn relational_blame_attributes_line_to_last_touch() {
    let fx = build_fixture();
    let rows = blobfs::blame(fx.repo(), "main", "config.toml", 10).unwrap();
    assert_eq!(rows.len(), 2, "config.toml has two lines at HEAD");
    for (i, r) in rows.iter().enumerate() {
        assert_eq!(r.line, (i + 1) as i64);
        // config.toml was last touched by c2 (Grace).
        assert_eq!(r.sha, fx.c2.as_str(), "blame sha = last-touch commit");
        assert!(r.author.contains("Grace"), "blame author: {}", r.author);
    }
    // A file last touched at the ROOT commit blames to c1 (README never changed after c1).
    let readme_blame = blobfs::blame(fx.repo(), "main", "README.md", 10).unwrap();
    assert!(readme_blame.iter().all(|r| r.sha == fx.c1.as_str()));
}

#[test]
fn relational_refs_and_tags_rows() {
    let fx = build_fixture();
    let repo = fx.repo();
    let refs = relational::refs(repo);
    assert!(
        refs.iter()
            .any(|r| r.name == "refs/heads/main" && r.oid == fx.c2.as_str()),
        "refs lists the branch tip"
    );
    // /tags is the truthful residual `name LIKE 'refs/tags/%'`.
    let tags = relational::tags(repo);
    assert_eq!(tags.len(), 1);
    assert_eq!(tags[0].name, "refs/tags/rel-1");
    assert_eq!(tags[0].oid, fx.c1.as_str());
}

#[test]
fn relational_reflog_tail_newest_first() {
    let fx = build_fixture();
    let rows = relational::reflog(fx.repo(), "refs/heads/main");
    assert_eq!(rows.len(), 2);
    // Newest first: the c1→c2 move heads the tail.
    assert_eq!(rows[0].new, fx.c2.as_str());
    assert_eq!(rows[0].old, fx.c1.as_str());
}

// =================================================================================================
// 3. Write plans — PREVIEW vs COMMIT
// =================================================================================================

// Re-test gate (round 2): the Constructor's commit `74b7ad9` added the additive public builder
// `CommitInput::new(branch, author, committer, message).at_time(t).with_file(path, bytes)` /
// `.with_files(map)` (mirroring `ProcSig::new`/`RepoResolver::with_repo`/`GitApplier::with_store`),
// keeping `#[non_exhaustive]` and the `CommitInput`/`CommitPlan` shapes unchanged. The previously
// uncallable INSERT-INTO-commits write path is now reachable from OUTSIDE the crate — scenarios 3
// and 8's plan half are closed below through that public constructor.

/// Build the round-2 `CommitInput` purely through the public builder (no struct literal).
fn changelog_input() -> CommitInput {
    CommitInput::new(
        "refs/heads/main",
        "Linus T <linus@demo.test>",
        "Linus T <linus@demo.test>",
        "Add changelog",
    )
    .at_time(1_700_001_000)
    .with_file("CHANGELOG.md", b"# 1.0\n".to_vec())
}

#[test]
fn write_insert_commit_preview_is_side_effect_free() {
    let fx = build_fixture();
    let repo = fx.repo();
    let input = changelog_input();

    let planned = plan_insert_commit("demo", repo, &input).unwrap();

    // PREVIEW shape: blob + tree + commit WriteLooseObject, a CAS UpdateRef, a reflog entry —
    // i.e. a DAG of WriteLooseObject + UpdateRef effects (the acceptance-criteria plan assertion).
    assert!(
        planned.plan.nodes().len() >= 4,
        "blob+tree+commit+ref(+reflog), got {}",
        planned.plan.nodes().len()
    );
    let kinds: Vec<&EffectKind> = planned.plan.nodes().iter().map(|n| &n.kind).collect();
    assert!(
        kinds.iter().any(|k| matches!(k, EffectKind::Insert)),
        "PREVIEW plan has WriteLooseObject (Insert) effects"
    );
    assert!(
        kinds.iter().any(|k| matches!(k, EffectKind::Update)),
        "PREVIEW plan has an UpdateRef (Update) effect"
    );
    // Correct CAS old-oid = the current branch tip (c2); new-commit oid is content-addressed.
    assert_eq!(
        planned.old_commit.as_ref().unwrap().as_str(),
        fx.c2.as_str(),
        "PREVIEW old-oid = current branch tip"
    );
    assert_ne!(planned.new_commit.as_str(), fx.c2.as_str(), "a new commit");

    // Genuinely side-effect-free: the apply-store branch still points at c2, no reflog growth, and
    // the new commit object is NOT present in the apply store (PREVIEW applies NOTHING).
    let applier = fx.driver.git_applier();
    assert_eq!(
        applier.ref_oid("demo", "refs/heads/main").unwrap(),
        fx.c2,
        "PREVIEW must not move the branch"
    );
    assert!(
        applier.reflog("demo", "refs/heads/main").is_empty(),
        "PREVIEW must not append a reflog entry"
    );
    // Re-planning yields the SAME content-addressed new-commit oid AND still applies nothing — a
    // strong determinism + purity signal.
    let planned2 = plan_insert_commit("demo", repo, &input).unwrap();
    assert_eq!(planned2.new_commit, planned.new_commit);
    assert_eq!(applier.ref_oid("demo", "refs/heads/main").unwrap(), fx.c2);
}

#[tokio::test]
async fn write_commit_moves_branch_and_writes_reflog() {
    let fx = build_fixture();
    let planned = plan_insert_commit("demo", fx.repo(), &changelog_input()).unwrap();

    // COMMIT: drive the applier through the SharedApplier seam in plan node order (deps satisfied).
    let applier = fx.driver.git_applier().clone();
    for node in planned.plan.nodes() {
        applier.apply_shared(node).unwrap();
    }

    // After COMMIT the branch points at the new commit; the reflog records the move FROM c2.
    assert_eq!(
        applier.ref_oid("demo", "refs/heads/main").unwrap(),
        planned.new_commit,
        "branch now points at the new commit"
    );
    let reflog = applier.reflog("demo", "refs/heads/main");
    assert_eq!(reflog.first().unwrap().new, planned.new_commit);
    assert_eq!(reflog.first().unwrap().old, fx.c2, "reflog entry exists");

    // Idempotency of the CONTENT-ADDRESSED object writes: re-applying a WriteLooseObject for an oid
    // already present is a no-op (affected = 0), not an error. (The CAS UpdateRef is deliberately
    // NOT idempotent on replay — re-applying its now-stale `old` correctly conflicts; that is the
    // safety property scenario 4 exercises, so we re-apply only the Insert/object-write effects.)
    for node in planned.plan.nodes() {
        if matches!(node.kind, EffectKind::Insert) {
            let out = applier.apply_shared(node).unwrap();
            assert_eq!(
                out.affected, 0,
                "re-writing an existing content-addressed object is a no-op"
            );
        }
    }
    // The branch is unchanged by the object-write replay.
    assert_eq!(
        applier.ref_oid("demo", "refs/heads/main").unwrap(),
        planned.new_commit
    );
}

#[tokio::test]
async fn write_commit_runtime_bridge_constructs_over_a_real_commit_plan() {
    // The locked driver pattern: the synchronous applier wrapped by the runtime bridge is the
    // surface the t10 interpreter executes a /git plan through. We confirm the bridge constructs,
    // build a real commit plan via the public constructor, and apply it through the same shared
    // applier the bridge wraps (moving the world the bridge would drive under COMMIT).
    let fx = build_fixture();
    let _bridge = git_apply_driver(&fx.driver);

    let input = CommitInput::new(
        "refs/heads/main",
        "Bridge <bridge@demo.test>",
        "Bridge <bridge@demo.test>",
        "Tag version file",
    )
    .at_time(1_700_002_000)
    .with_file("VERSION", b"1.0.0\n".to_vec());
    let planned = plan_insert_commit("demo", fx.repo(), &input).unwrap();

    let applier = fx.driver.git_applier().clone();
    for node in planned.plan.nodes() {
        applier.apply_shared(node).unwrap();
    }
    assert_eq!(
        applier.ref_oid("demo", "refs/heads/main").unwrap(),
        planned.new_commit
    );
}

// =================================================================================================
// 4. CAS ref update — stale old-oid is rejected, not clobbered
// =================================================================================================

#[test]
fn cas_stale_old_oid_is_rejected_not_clobbered() {
    let fx = build_fixture();
    let applier = fx.driver.git_applier().clone();

    // Concurrent-style stale write: an actor believes main is at c1 (it is actually at c2) and
    // tries to move it. CAS must REJECT.
    let stale = plan_update_ref(
        "demo",
        "refs/heads/main",
        Some(fx.c1.clone()), // STALE expectation
        fx.c1.clone(),
        false, // not forced
        "stale-writer",
    );
    let ref_node = stale
        .nodes()
        .iter()
        .find(|n| matches!(n.kind, EffectKind::Update))
        .unwrap();
    let err = applier.apply_shared(ref_node).unwrap_err();
    assert_eq!(err.code(), "conflict", "CAS mismatch is a typed conflict");

    // The branch was NOT clobbered — still at c2.
    assert_eq!(applier.ref_oid("demo", "refs/heads/main").unwrap(), fx.c2);
}

#[test]
fn cas_fresh_old_oid_is_accepted() {
    let fx = build_fixture();
    let applier = fx.driver.git_applier().clone();
    // A correct (fresh) old-oid (c2) is accepted — moving main to c1 legitimately.
    let fresh = plan_update_ref(
        "demo",
        "refs/heads/main",
        Some(fx.c2.clone()),
        fx.c1.clone(),
        false,
        "fresh-writer",
    );
    for node in fresh.nodes() {
        applier.apply_shared(node).unwrap();
    }
    assert_eq!(applier.ref_oid("demo", "refs/heads/main").unwrap(), fx.c1);
}

#[test]
fn cas_tag_creation_rejects_existing_ref() {
    let fx = build_fixture();
    let applier = fx.driver.git_applier().clone();
    // plan_tag creates refs/tags/<name> as a CAS CREATION (old = None). Creating `rel-1` which
    // already exists must be rejected.
    let dup = plan_tag("demo", "rel-1", &fx.c2, "tagger");
    let ref_node = dup
        .nodes()
        .iter()
        .find(|n| matches!(n.kind, EffectKind::Update))
        .unwrap();
    let err = applier.apply_shared(ref_node).unwrap_err();
    assert_eq!(
        err.code(),
        "conflict",
        "creating an existing tag = conflict"
    );
    // The existing tag is untouched (still at c1).
    assert_eq!(applier.ref_oid("demo", "refs/tags/rel-1").unwrap(), fx.c1);
}

// =================================================================================================
// 5. merge-conflict purity (highest risk) + clean-merge DAG
// =================================================================================================

/// Build a repo that can read base/ours/theirs commits so plan_merge can three-way-merge them.
/// Returns the repo + the divergent `theirs` oid (c3), and asserts c1/c2 reproduce.
fn build_divergent_repo(fx: &Fx) -> (Repo, Oid) {
    let readme = b"---\ntitle: Demo Repo\nversion: 2\n---\n# Demo\n\nThe body text.\n".as_slice();
    let cfg_v1 = b"name = \"demo\"\nport = 8080\n".as_slice();
    let cfg_v2 = b"name = \"demo\"\nport = 9090\n".as_slice();
    let cfg_v3 = b"name = \"demo\"\nport = 7070\n".as_slice(); // diverges from base AND ours

    let mut db = LooseObjectDb::new();

    let readme1 = blob_entry(&mut db, "README.md", readme);
    let cfg1 = blob_entry(&mut db, "config.toml", cfg_v1);
    let t1 = tree_oid(&mut db, vec![readme1.clone(), cfg1]);
    let c1p = format!("tree {}\nauthor Ada Lovelace <ada@demo.test> 1700000000 +0000\ncommitter Ada Lovelace <ada@demo.test> 1700000000 +0000\n\nInitial import\n", t1.as_str());
    let c1 = db.insert_object(ObjectKind::Commit, c1p.as_bytes());
    assert_eq!(c1, fx.c1, "c1 oid must reproduce from public surface");

    let cfg2 = blob_entry(&mut db, "config.toml", cfg_v2);
    let t2 = tree_oid(&mut db, vec![readme1.clone(), cfg2]);
    let c2p = format!("tree {}\nparent {}\nauthor Grace Hopper <grace@demo.test> 1700000600 +0000\ncommitter Grace Hopper <grace@demo.test> 1700000600 +0000\n\nBump config port\n", t2.as_str(), c1.as_str());
    let c2 = db.insert_object(ObjectKind::Commit, c2p.as_bytes());
    assert_eq!(c2, fx.c2);

    let cfg3 = blob_entry(&mut db, "config.toml", cfg_v3);
    let t3 = tree_oid(&mut db, vec![readme1, cfg3]);
    let c3p = format!("tree {}\nparent {}\nauthor Dijkstra <e@demo.test> 1700000900 +0000\ncommitter Dijkstra <e@demo.test> 1700000900 +0000\n\nDivergent port\n", t3.as_str(), c1.as_str());
    let c3 = db.insert_object(ObjectKind::Commit, c3p.as_bytes());

    (Repo::new(Arc::new(db) as Arc<dyn ObjectDb>), c3)
}

#[test]
fn merge_conflict_is_plan_build_error_with_zero_effects() {
    let fx = build_fixture();
    let (repo, c3) = build_divergent_repo(&fx);
    // base=c1 (port 8080); ours=c2 (port 9090); theirs=c3 (port 7070) — both sides changed
    // config.toml away from base to DIFFERENT content → conflict.
    let result = plan_merge(
        "demo",
        &repo,
        "refs/heads/main",
        &fx.c1, // base
        &fx.c2, // ours
        &c3,    // theirs
        "merger",
    );
    let err = result.unwrap_err();
    assert_eq!(err.code(), "merge_conflict", "typed plan-build conflict");
    // ZERO effects: plan_merge returns Err — there is NO Plan handed back, so nothing can be
    // applied. The conflict is surfaced purely at plan build, never as a half-applied mutation.
    // (A returned Err carries no Plan to apply — the purity guarantee.)
}

#[test]
fn merge_clean_produces_expected_effect_dag() {
    let fx = build_fixture();
    let repo = fx.repo();
    // base=c1, ours=c1 (unchanged), theirs=c2 (config.toml modified) → no conflict; theirs wins.
    let plan = plan_merge(
        "demo",
        repo,
        "refs/heads/main",
        &fx.c1, // base
        &fx.c1, // ours unchanged from base
        &fx.c2, // theirs changed
        "merger",
    )
    .unwrap();
    // The clean merge DAG: a merged-tree + merge-commit WriteLooseObject, an UpdateRef, a reflog.
    assert!(plan.nodes().len() >= 3, "tree+commit+ref(+reflog)");
    assert!(
        plan.nodes()
            .iter()
            .any(|n| matches!(n.kind, EffectKind::Update)),
        "clean merge emits an UpdateRef"
    );
    assert!(
        plan.nodes()
            .iter()
            .any(|n| matches!(n.kind, EffectKind::Insert)),
        "clean merge emits object writes"
    );
}

#[test]
fn merge_rebase_shares_the_zero_effect_conflict_surface() {
    // plan_rebase delegates to plan_merge (documented E0 park) — it must share the SAME honest
    // zero-effect conflict surface. A conflicting rebase is an Err, never a partial mutation.
    let fx = build_fixture();
    let (repo, c3) = build_divergent_repo(&fx);
    let err = plan_rebase(
        "demo",
        &repo,
        "refs/heads/main",
        &fx.c1,
        &fx.c2,
        &c3,
        "rebaser",
    )
    .unwrap_err();
    assert_eq!(err.code(), "merge_conflict");
}

// =================================================================================================
// 6. Capability gating at PARSE / resolve time
// =================================================================================================

#[test]
fn capability_update_on_commits_rejected_at_parse_time() {
    let fx = build_fixture();
    let commits = Path::new("/git/demo/commits");
    // UPDATE /commits is structurally rejected BEFORE any plan exists (commits = {SELECT, INSERT}).
    let err = check_capability(&fx.driver, &commits, Verb::Update).unwrap_err();
    assert_eq!(err.code(), "unsupported_verb");
    // The keyword-clash-free path: INSERT (= make a commit) and SELECT ARE allowed.
    assert!(check_capability(&fx.driver, &commits, Verb::Insert).is_ok());
    assert!(check_capability(&fx.driver, &commits, Verb::Select).is_ok());
    // REMOVE a commit is also rejected.
    assert!(check_capability(&fx.driver, &commits, Verb::Remove).is_err());
}

#[test]
fn capability_per_node_matrix_holds() {
    let fx = build_fixture();
    let d = &fx.driver;
    // refs / tags → SELECT + UPDATE.
    assert!(check_capability(d, &Path::new("/git/demo/refs"), Verb::Update).is_ok());
    assert!(check_capability(d, &Path::new("/git/demo/tags"), Verb::Update).is_ok());
    // blob (BlobFs) → read-only: SELECT ok, but UPDATE/INSERT rejected (writes go via /commits).
    let blob = Path::new("/git/demo/config.toml");
    assert!(check_capability(d, &blob, Verb::Select).is_ok());
    assert!(check_capability(d, &blob, Verb::Update).is_err());
    assert!(check_capability(d, &blob, Verb::Insert).is_err());
    // changes / blame / reflog → SELECT only (UPDATE rejected).
    assert!(check_capability(d, &Path::new("/git/demo/changes"), Verb::Select).is_ok());
    assert!(check_capability(d, &Path::new("/git/demo/changes"), Verb::Update).is_err());
    assert!(check_capability(d, &Path::new("/git/demo/reflog"), Verb::Update).is_err());
}

#[test]
fn capability_call_resolves_only_declared_git_procedures() {
    let fx = build_fixture();
    for name in ["merge", "rebase", "checkout", "tag"] {
        let p = resolve_proc(&fx.driver, name).unwrap();
        assert!(!p.irreversible, "git procedures are reflog-recoverable");
    }
    // An undeclared procedure (e.g. a dangerous `force_push`) is rejected at resolve.
    let err = resolve_proc(&fx.driver, "force_push").unwrap_err();
    assert_eq!(err.code(), "unknown_procedure");
}

// =================================================================================================
// 7. Reflog recovery of a forced ref move
// =================================================================================================

#[test]
fn reflog_forced_move_is_recoverable() {
    let fx = build_fixture();
    let applier = fx.driver.git_applier().clone();

    // Force main back to c1 (orphaning c2). A forced move skips CAS but records the prior oid.
    let forced = plan_update_ref(
        "demo",
        "refs/heads/main",
        Some(fx.c2.clone()),
        fx.c1.clone(),
        true, // FORCE
        "Mallory <mallory@demo.test>",
    );
    for node in forced.nodes() {
        applier.apply_shared(node).unwrap();
    }
    assert_eq!(applier.ref_oid("demo", "refs/heads/main").unwrap(), fx.c1);

    // /reflog shows the prior oid (c2) for the forced move (newest first).
    let reflog = applier.reflog("demo", "refs/heads/main");
    assert_eq!(
        reflog.first().unwrap().old,
        fx.c2,
        "reflog records prior oid"
    );

    // The recovery helper restores c2 from the reflog.
    let restored = applier.recover_ref("demo", "refs/heads/main").unwrap();
    assert_eq!(restored, fx.c2);
    assert_eq!(
        applier.ref_oid("demo", "refs/heads/main").unwrap(),
        fx.c2,
        "branch restored to the orphaned tip"
    );
}

// =================================================================================================
// 8. COMMIT keyword-clash: commit creation is INSERT, never the COMMIT plan keyword
// =================================================================================================

#[test]
fn keyword_clash_commit_creation_is_insert_not_the_commit_keyword() {
    // Scenario 8 from the OUTSIDE, now fully reachable (round 2). Two halves:
    //
    // (a) The AI-facing CAPABILITY surface — the mechanism that resolves the keyword clash, RFD §3:
    //     a git commit is created with INSERT on `/commits`; the UPDATE verb a `COMMIT`-keyword
    //     model might imply is structurally rejected; SELECT is the read half.
    let fx = build_fixture();
    let commits = Path::new("/git/demo/commits");
    assert!(
        check_capability(&fx.driver, &commits, Verb::Insert).is_ok(),
        "commit creation is INSERT INTO /commits (the keyword-clash-free verb)"
    );
    assert!(
        check_capability(&fx.driver, &commits, Verb::Update).is_err(),
        "UPDATE /commits (which a COMMIT-keyword model might imply) is rejected — the clash is avoided"
    );
    assert!(check_capability(&fx.driver, &commits, Verb::Select).is_ok());

    // (b) The PLAN-NODE cross-check (previously DTO-blocked, now reachable via the public
    //     `CommitInput` constructor): a real commit-creation plan is built ENTIRELY from
    //     INSERT (object writes) + UPDATE (ref/reflog) effect kinds. The frozen `COMMIT` plan
    //     keyword is never an effect-node verb here — it remains exclusively the interpreter's
    //     apply verb, never required or shadowed by commit creation.
    let input = CommitInput::new(
        "refs/heads/main",
        "A <a@demo.test>",
        "A <a@demo.test>",
        "make a commit",
    )
    .at_time(1_700_003_000)
    .with_file("NEW.txt", b"x\n".to_vec());
    let planned = plan_insert_commit("demo", fx.repo(), &input).unwrap();
    for node in planned.plan.nodes() {
        assert!(
            matches!(node.kind, EffectKind::Insert | EffectKind::Update),
            "commit-creation effect must be INSERT/UPDATE (never COMMIT), got {:?}",
            node.kind
        );
    }
}

#[test]
fn keyword_clash_describe_documents_commits_node() {
    // DESCRIBE /git/<repo>/commits documents the relational commits node + its schema, the surface
    // the AI reads to learn it creates commits via INSERT (not COMMIT).
    let fx = build_fixture();
    let desc = fx.driver.describe(&Path::new("/git/demo/commits")).unwrap();
    assert_eq!(desc.archetype, cfs_driver::Archetype::RelationalTable);
    assert!(desc.schema.column("sha").is_some());
    assert!(desc.schema.column("message").is_some());
    assert!(desc.schema.column("author").is_some());
}

// =================================================================================================
// 9. (Bonus) real `git` loose object inflated through the public driver surface
// =================================================================================================

/// Build a real repo with the local `git` binary in a unique tempdir, returning the dir, the HEAD
/// commit sha, and a `(path, blob_sha)` for a committed file. Cleaned up by the caller.
fn build_real_git_repo() -> Option<(std::path::PathBuf, String, String, String)> {
    use std::process::Command;
    // Skip gracefully if no git binary (the suite must not depend on git existing).
    if Command::new("git").arg("--version").output().is_err() {
        return None;
    }
    let dir = std::env::temp_dir().join(format!("cfs-git-e2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).ok()?;

    let run = |args: &[&str]| -> Option<std::process::Output> {
        Command::new("git")
            .current_dir(&dir)
            .args(args)
            .env("GIT_AUTHOR_DATE", "1700000000 +0000")
            .env("GIT_COMMITTER_DATE", "1700000000 +0000")
            .output()
            .ok()
    };
    run(&["init", "-q"])?;
    run(&["config", "user.email", "real@demo.test"])?;
    run(&["config", "user.name", "Real Git"])?;
    // A canonical file with frontmatter so the codec path is exercised on REAL git output too.
    let content = "---\ntitle: From Real Git\nn: 7\n---\n# Canonical\n\nInflated body.\n";
    std::fs::write(dir.join("doc.md"), content).ok()?;
    run(&["add", "doc.md"])?;
    run(&["commit", "-q", "-m", "real canonical commit"])?;

    let head = String::from_utf8(run(&["rev-parse", "HEAD"])?.stdout)
        .ok()?
        .trim()
        .to_string();
    let blob_sha = String::from_utf8(run(&["hash-object", "doc.md"])?.stdout)
        .ok()?
        .trim()
        .to_string();
    Some((dir, head, "doc.md".to_string(), blob_sha))
}

/// Read the on-disk COMPRESSED loose object bytes for an oid from a real `.git/objects` dir.
fn read_loose_bytes(repo_dir: &std::path::Path, oid: &str) -> Option<Vec<u8>> {
    let path = repo_dir
        .join(".git/objects")
        .join(&oid[..2])
        .join(&oid[2..]);
    std::fs::read(path).ok()
}

#[test]
fn real_git_loose_object_inflates_through_public_surface() {
    let Some((dir, head, file, blob_sha)) = build_real_git_repo() else {
        eprintln!("SKIP real_git_*: git binary unavailable");
        return;
    };

    // Pull the canonical loose objects git wrote (compressed `zlib(<type> <len>\0<payload>)`) and
    // feed them VERBATIM into the in-house db via insert_loose — so the in-house inflater runs on
    // real git output. We need: HEAD commit, its tree, and the doc.md blob.
    let head_bytes = read_loose_bytes(&dir, &head).expect("HEAD loose object on disk");
    let blob_bytes = read_loose_bytes(&dir, &blob_sha).expect("blob loose object on disk");

    // The tree oid comes from parsing the commit, but to load it we need to read the commit first.
    // Build a db with the commit + blob, discover the tree oid via the public Repo, then add it.
    let mut db = LooseObjectDb::new();
    db.insert_loose(
        Oid::parse(&head).expect("valid head oid"),
        head_bytes.clone(),
    );
    db.insert_loose(Oid::parse(&blob_sha).expect("valid blob oid"), blob_bytes);

    // Confirm a zlib stream was actually stored (real git compresses — first byte 0x78).
    assert_eq!(
        head_bytes.first(),
        Some(&0x78),
        "real git writes a zlib-compressed loose object"
    );

    // Read the commit through the public Repo to discover the tree oid, then load the tree's loose
    // bytes too.
    let mut repo0 = Repo::new(Arc::new(db) as Arc<dyn ObjectDb>);
    repo0.set_ref("refs/heads/master", Oid::parse(&head).unwrap());
    repo0.set_ref("refs/heads/main", Oid::parse(&head).unwrap());
    repo0.set_symbolic("HEAD", "refs/heads/master");
    let commit = repo0
        .read_commit(&Oid::parse(&head).unwrap())
        .expect("in-house inflate + parse of a REAL git commit");
    let tree_oid = commit.tree.clone();

    // Rebuild the db now including the tree loose object, then a fresh repo over the full set.
    let tree_bytes = read_loose_bytes(&dir, tree_oid.as_str()).expect("tree loose object on disk");
    let mut db2 = LooseObjectDb::new();
    for (oid, bytes) in [
        (head.clone(), read_loose_bytes(&dir, &head).unwrap()),
        (blob_sha.clone(), read_loose_bytes(&dir, &blob_sha).unwrap()),
        (tree_oid.as_str().to_string(), tree_bytes),
    ] {
        db2.insert_loose(Oid::parse(&oid).unwrap(), bytes);
    }
    let mut repo = Repo::new(Arc::new(db2) as Arc<dyn ObjectDb>);
    repo.set_ref("refs/heads/master", Oid::parse(&head).unwrap());
    repo.set_ref("refs/heads/main", Oid::parse(&head).unwrap());
    repo.set_symbolic("HEAD", "refs/heads/master");

    // BlobFs cat through the public surface returns the EXACT committed bytes — proving the
    // in-house DEFLATE inflate decoded real git's compressed loose object end-to-end.
    let cat = blobfs::cat(&repo, &head, &file).expect("cat a real-git blob via public surface");
    let expected = "---\ntitle: From Real Git\nn: 7\n---\n# Canonical\n\nInflated body.\n";
    assert_eq!(
        String::from_utf8_lossy(&cat),
        expected,
        "in-house inflate reproduces real git's exact blob bytes"
    );

    // And the codec registry decodes that real-git blob's frontmatter end-to-end.
    let codec = MarkdownFrontmatterCodec;
    let batch = blobfs::cat_decode(&repo, &head, &file, &codec).expect("decode real-git md");
    let cols: Vec<&str> = batch
        .schema
        .columns
        .iter()
        .map(|c| c.name.as_str())
        .collect();
    assert!(cols.contains(&"title"));
    assert!(cols.contains(&"n"));
    assert!(cols.contains(&"body"));

    // The content-address re-verifies: the blob sha git computed equals what the public surface
    // reads it under (the in-house SHA-1 framing matches canonical git).
    let listing = blobfs::ls(&repo, &head, "").unwrap();
    let listed_blob_oid = listing
        .rows
        .iter()
        .find_map(|r| match (&r.values[0], &r.values[2]) {
            (Value::Text(name), Value::Text(oid)) if name == "doc.md" => Some(oid.clone()),
            _ => None,
        })
        .expect("doc.md in the listing");
    assert_eq!(
        listed_blob_oid, blob_sha,
        "in-house content address matches canonical git blob oid"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

// =================================================================================================
// Cross-cutting: checkout proc plan (HEAD move) is reflog-recorded and reversible
// =================================================================================================

#[test]
fn checkout_proc_plans_a_reflog_recorded_head_move() {
    let fx = build_fixture();
    // plan_checkout moves HEAD to a target (a symbolic/forced move, reflog-recorded).
    let plan = plan_checkout("demo", &fx.c1, "checkout-user");
    assert!(
        plan.nodes()
            .iter()
            .any(|n| matches!(n.kind, EffectKind::Update)),
        "checkout emits an UpdateRef (HEAD move)"
    );
    // Applying it moves HEAD to c1 and records a reflog entry (reversible).
    let applier = fx.driver.git_applier().clone();
    for node in plan.nodes() {
        applier.apply_shared(node).unwrap();
    }
    assert_eq!(applier.ref_oid("demo", "HEAD").unwrap(), fx.c1);
    assert!(!applier.reflog("demo", "HEAD").is_empty());
}
