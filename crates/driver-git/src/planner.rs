//! Pure write-plan builders (RFD §3 purity invariant: every write builds a [`Plan`] and applies
//! **nothing**). Each builder lowers a git write into a DAG of [`GitEffect`]s encoded as
//! [`EffectNode`] row args (the applier decodes them back). NONE of these performs I/O — they
//! read the repo's *current* refs (already in memory) to compute CAS old-oids and to do the
//! in-memory three-way merge, then emit effects.
//!
//! The four hard requirements live here:
//! - **No `COMMIT` keyword clash**: a git commit is `INSERT INTO /git/<repo>/commits`
//!   ([`plan_insert_commit`]); it never emits the frozen plan keyword `COMMIT`.
//! - **CAS ref move**: [`plan_update_ref`] sets the effect's `old` to the ref's current oid, so a
//!   stale expectation is rejected at apply (the applier enforces the swap).
//! - **merge/rebase as pure plans**: [`plan_merge`]/[`plan_rebase`] compute the result tree
//!   DURING planning (in-memory three-way merge) and return a typed
//!   [`GitError::MergeConflict`] in PREVIEW with **zero** effects on conflict.
//! - **Object writes are content-addressed** → the emitted `WriteLooseObject` oid is the content
//!   address (idempotent, reversible).

use std::collections::BTreeMap;

use cfs_plan::{
    Affected, DriverId, EffectKind, EffectNode, NodeId, Plan, PlanBuilder, Target, VfsPath,
};
use cfs_types::{Column, ColumnType, Row, RowBatch, Schema, Value};

use crate::effect::GitEffect;
use crate::error::GitError;
use crate::objectdb::{frame_and_id, serialize_tree, ObjectKind, Oid, Tree, TreeEntry};
use crate::repo::Repo;

/// The driver id every git effect target routes to.
fn git_driver_id() -> DriverId {
    DriverId::new("git")
}

/// Encode a [`GitEffect`] as the row args of an [`EffectNode`] (the wire the applier decodes).
/// Keeping the effect in typed columns lets the pure plan crate stay git-agnostic.
fn effect_node(id: NodeId, kind: EffectKind, repo: &str, effect: &GitEffect) -> EffectNode {
    let target = Target::new(
        git_driver_id(),
        VfsPath::new(format!("/git/{repo}/commits")),
    );
    let (schema, row) = encode_effect(effect);
    EffectNode::new(id, kind, target)
        .with_args(RowBatch::new(schema, vec![row]))
        .with_affected(Affected::Exact(1))
}

/// Allocate an id, build the effect node, and push it — collapsing the `next_id`/`effect_node`/
/// `push` sequence into one call so the builder is borrowed once (no double mutable borrow).
fn push_effect(b: &mut PlanBuilder, kind: EffectKind, repo: &str, effect: &GitEffect) -> NodeId {
    let id = b.next_id();
    let node = effect_node(id, kind, repo, effect);
    b.push(node)
}

/// Encode the effect into a (schema, row) the applier's `effect_from_row` decodes.
fn encode_effect(effect: &GitEffect) -> (Schema, Row) {
    // (name, type, value) triples, lowered to a parallel schema + row.
    let triples: Vec<(&str, ColumnType, Value)> = match effect {
        GitEffect::WriteLooseObject { oid, kind, payload } => vec![
            (
                "effect_kind",
                ColumnType::Text,
                Value::Text("write_object".into()),
            ),
            ("oid", ColumnType::Text, Value::Text(oid.as_str().into())),
            (
                "object_kind",
                ColumnType::Text,
                Value::Text(kind.keyword().into()),
            ),
            ("payload", ColumnType::Bytes, Value::Bytes(payload.clone())),
        ],
        GitEffect::UpdateRef {
            name,
            old,
            new,
            force,
        } => vec![
            (
                "effect_kind",
                ColumnType::Text,
                Value::Text("update_ref".into()),
            ),
            ("ref_name", ColumnType::Text, Value::Text(name.clone())),
            (
                "old_oid",
                ColumnType::Text,
                Value::Text(
                    old.as_ref()
                        .map(|o| o.as_str().to_string())
                        .unwrap_or_default(),
                ),
            ),
            (
                "new_oid",
                ColumnType::Text,
                Value::Text(new.as_str().into()),
            ),
            (
                "force",
                ColumnType::Text,
                Value::Text(if *force { "true" } else { "false" }.into()),
            ),
        ],
        GitEffect::WriteReflogEntry {
            name,
            old,
            new,
            who,
            message,
            time,
        } => vec![
            (
                "effect_kind",
                ColumnType::Text,
                Value::Text("write_reflog".into()),
            ),
            ("ref_name", ColumnType::Text, Value::Text(name.clone())),
            (
                "old_oid",
                ColumnType::Text,
                Value::Text(old.as_str().into()),
            ),
            (
                "new_oid",
                ColumnType::Text,
                Value::Text(new.as_str().into()),
            ),
            ("who", ColumnType::Text, Value::Text(who.clone())),
            ("message", ColumnType::Text, Value::Text(message.clone())),
            ("time", ColumnType::Timestamp, Value::Timestamp(*time)),
        ],
    };
    let cols: Vec<Column> = triples
        .iter()
        .map(|(name, ty, _)| Column::new(*name, ty.clone(), false))
        .collect();
    let vals: Vec<Value> = triples.into_iter().map(|(_, _, v)| v).collect();
    (Schema::new(cols), Row::new(vals))
}

/// The staged input of an `INSERT INTO /git/<repo>/commits` (the keyword-clash-free commit
/// creation). Owned — the evaluator builds this from the INSERT row(s).
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct CommitInput {
    /// The branch ref to move (e.g. `refs/heads/main`).
    pub branch: String,
    /// The author identity line.
    pub author: String,
    /// The committer identity line.
    pub committer: String,
    /// The epoch seconds.
    pub time: i64,
    /// The commit message.
    pub message: String,
    /// The staged tree: path → blob bytes (a flat tree at E0; nested trees are a named park).
    pub files: BTreeMap<String, Vec<u8>>,
}

/// The result of planning a commit: the pure [`Plan`] + the new commit oid (for PREVIEW display
/// and the caller's assertion).
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct CommitPlan {
    /// The pure effect plan (applies nothing until COMMIT).
    pub plan: Plan,
    /// The content-addressed oid of the new commit (what the branch will point at).
    pub new_commit: Oid,
    /// The oid the branch pointed at before (the CAS old-oid; `None` = branch creation).
    pub old_commit: Option<Oid>,
}

/// Plan an `INSERT INTO /git/<repo>/commits` (a git commit, modelled WITHOUT the `COMMIT`
/// keyword — RFD §3 keyword-clash resolution). Builds the staged blobs → a tree → the commit
/// object as content-addressed [`GitEffect::WriteLooseObject`] effects, then a CAS
/// [`GitEffect::UpdateRef`] on the branch + a [`GitEffect::WriteReflogEntry`]. Pure — performs no
/// I/O; reads only the repo's already-in-memory current ref to set the CAS old-oid.
///
/// # Errors
/// [`GitError`] if the staged tree cannot be built (e.g. a malformed path).
pub fn plan_insert_commit(
    repo_name: &str,
    repo: &Repo,
    input: &CommitInput,
) -> Result<CommitPlan, GitError> {
    let mut b = PlanBuilder::new();
    let mut object_nodes: Vec<NodeId> = Vec::new();

    // 1. Stage each file as a blob object (content-addressed).
    let mut entries: Vec<TreeEntry> = Vec::new();
    for (path, bytes) in &input.files {
        let (oid, _framed) = frame_and_id(ObjectKind::Blob, bytes);
        let id = push_effect(
            &mut b,
            EffectKind::Insert,
            repo_name,
            &GitEffect::WriteLooseObject {
                oid: oid.clone(),
                kind: ObjectKind::Blob,
                payload: bytes.clone(),
            },
        );
        object_nodes.push(id);
        entries.push(TreeEntry {
            mode: "100644".to_string(),
            name: path.clone(),
            oid,
        });
    }

    // 2. Build the tree (git sorts entries by name; BTreeMap already gave us name order).
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    let tree = Tree { entries };
    let tree_payload = serialize_tree(&tree);
    let (tree_oid, _) = frame_and_id(ObjectKind::Tree, &tree_payload);
    let tree_node_id = push_effect(
        &mut b,
        EffectKind::Insert,
        repo_name,
        &GitEffect::WriteLooseObject {
            oid: tree_oid.clone(),
            kind: ObjectKind::Tree,
            payload: tree_payload,
        },
    );
    // The tree depends on its blobs.
    for &blob in &object_nodes {
        b.depends_on(tree_node_id, blob);
    }

    // 3. Build the commit object pointing at the tree + the current branch tip as parent.
    let old_commit = repo.ref_oid(&input.branch);
    let commit_payload = build_commit_payload(&tree_oid, old_commit.as_ref(), input);
    let (commit_oid, _) = frame_and_id(ObjectKind::Commit, &commit_payload);
    let commit_node_id = push_effect(
        &mut b,
        EffectKind::Insert,
        repo_name,
        &GitEffect::WriteLooseObject {
            oid: commit_oid.clone(),
            kind: ObjectKind::Commit,
            payload: commit_payload,
        },
    );
    b.depends_on(commit_node_id, tree_node_id);

    // 4. CAS ref move: old = the current branch tip (so a stale expectation is rejected).
    let ref_node_id = push_effect(
        &mut b,
        EffectKind::Update,
        repo_name,
        &GitEffect::UpdateRef {
            name: input.branch.clone(),
            old: old_commit.clone(),
            new: commit_oid.clone(),
            force: false,
        },
    );
    b.depends_on(ref_node_id, commit_node_id);

    // 5. Reflog entry (the recovery oracle).
    let reflog_node_id = push_effect(
        &mut b,
        EffectKind::Update,
        repo_name,
        &GitEffect::WriteReflogEntry {
            name: input.branch.clone(),
            old: old_commit.clone().unwrap_or_else(zero_oid),
            new: commit_oid.clone(),
            who: input.committer.clone(),
            message: format!("commit: {}", first_line(&input.message)),
            time: input.time,
        },
    );
    b.depends_on(reflog_node_id, ref_node_id);

    Ok(CommitPlan {
        plan: b.build(),
        new_commit: commit_oid,
        old_commit,
    })
}

/// Plan an `UPDATE /git/<repo>/refs` (move a branch / set a ref). Sets the CAS old-oid to
/// `expected_old` (the caller's `@version` coordinate) so a stale value is rejected at apply.
/// A `force` move is flagged (reflog-recoverable). Pure.
#[must_use]
pub fn plan_update_ref(
    repo_name: &str,
    name: &str,
    expected_old: Option<Oid>,
    new: Oid,
    force: bool,
    who: &str,
) -> Plan {
    let mut b = PlanBuilder::new();
    let ref_id = push_effect(
        &mut b,
        EffectKind::Update,
        repo_name,
        &GitEffect::UpdateRef {
            name: name.to_string(),
            old: expected_old.clone(),
            new: new.clone(),
            force,
        },
    );
    let reflog_id = push_effect(
        &mut b,
        EffectKind::Update,
        repo_name,
        &GitEffect::WriteReflogEntry {
            name: name.to_string(),
            old: expected_old.unwrap_or_else(zero_oid),
            new,
            who: who.to_string(),
            message: if force {
                "update-ref (forced)".to_string()
            } else {
                "update-ref".to_string()
            },
            time: 0,
        },
    );
    b.depends_on(reflog_id, ref_id);
    b.build()
}

/// Plan a `CALL git.merge(base, ours, theirs)` as a pure plan DAG. Computes the result tree
/// DURING planning via an in-memory three-way merge over the three commits' trees; a path that
/// diverged on **both** sides relative to the base is a typed [`GitError::MergeConflict`] returned
/// in PREVIEW with **zero** effects (never a half-applied mutation). A clean merge reduces to the
/// merged-tree + merge-commit `WriteLooseObject` effects + a CAS `UpdateRef` on `ours`.
///
/// # Errors
/// [`GitError::MergeConflict`] on a conflicting path; [`GitError`] on a missing/malformed object.
pub fn plan_merge(
    repo_name: &str,
    repo: &Repo,
    branch: &str,
    base: &Oid,
    ours: &Oid,
    theirs: &Oid,
    who: &str,
) -> Result<Plan, GitError> {
    let base_tree = read_flat_tree(repo, base)?;
    let ours_tree = read_flat_tree(repo, ours)?;
    let theirs_tree = read_flat_tree(repo, theirs)?;

    // In-memory three-way merge of flat trees (path → blob oid).
    let merged = three_way_merge(repo_name, &base_tree, &ours_tree, &theirs_tree)?;

    let mut b = PlanBuilder::new();
    // Build the merged tree object (its blobs already exist in the object db — content-addressed,
    // so the merged tree references them by oid without re-writing them).
    let mut entries: Vec<TreeEntry> = merged
        .into_iter()
        .map(|(name, oid)| TreeEntry {
            mode: "100644".to_string(),
            name,
            oid,
        })
        .collect();
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    let tree = Tree { entries };
    let tree_payload = serialize_tree(&tree);
    let (tree_oid, _) = frame_and_id(ObjectKind::Tree, &tree_payload);
    let tree_id = push_effect(
        &mut b,
        EffectKind::Insert,
        repo_name,
        &GitEffect::WriteLooseObject {
            oid: tree_oid.clone(),
            kind: ObjectKind::Tree,
            payload: tree_payload,
        },
    );

    // The merge commit has two parents (ours, theirs).
    let input = CommitInput {
        branch: branch.to_string(),
        author: who.to_string(),
        committer: who.to_string(),
        time: 0,
        message: format!("Merge {} into {}", theirs.short(), branch),
        files: BTreeMap::new(),
    };
    let commit_payload =
        build_merge_commit_payload(&tree_oid, &[ours.clone(), theirs.clone()], &input);
    let (commit_oid, _) = frame_and_id(ObjectKind::Commit, &commit_payload);
    let commit_id = push_effect(
        &mut b,
        EffectKind::Insert,
        repo_name,
        &GitEffect::WriteLooseObject {
            oid: commit_oid.clone(),
            kind: ObjectKind::Commit,
            payload: commit_payload,
        },
    );
    b.depends_on(commit_id, tree_id);

    let ref_id = push_effect(
        &mut b,
        EffectKind::Update,
        repo_name,
        &GitEffect::UpdateRef {
            name: branch.to_string(),
            old: Some(ours.clone()),
            new: commit_oid.clone(),
            force: false,
        },
    );
    b.depends_on(ref_id, commit_id);
    let reflog_id = push_effect(
        &mut b,
        EffectKind::Update,
        repo_name,
        &GitEffect::WriteReflogEntry {
            name: branch.to_string(),
            old: ours.clone(),
            new: commit_oid,
            who: who.to_string(),
            message: "merge".to_string(),
            time: 0,
        },
    );
    b.depends_on(reflog_id, ref_id);

    Ok(b.build())
}

/// Plan a `CALL git.rebase(...)`. At E0 a rebase shares the merge's three-way-merge conflict
/// detection (replaying `theirs` onto `ours`); a conflict is the same typed plan-build error with
/// zero effects. A clean rebase reduces to the merged result.
///
/// # Errors
/// [`GitError::MergeConflict`] on conflict.
pub fn plan_rebase(
    repo_name: &str,
    repo: &Repo,
    branch: &str,
    base: &Oid,
    ours: &Oid,
    theirs: &Oid,
    who: &str,
) -> Result<Plan, GitError> {
    plan_merge(repo_name, repo, branch, base, ours, theirs, who)
}

/// Plan a `CALL git.checkout(ref)` — move `HEAD` to point at the resolved ref (a symbolic move,
/// reflog-recorded). Pure; reversible.
#[must_use]
pub fn plan_checkout(repo_name: &str, target: &Oid, who: &str) -> Plan {
    plan_update_ref(repo_name, "HEAD", None, target.clone(), true, who)
}

/// Plan a `CALL git.tag(name, target)` — create a lightweight tag ref. A creation (`old = None`)
/// is rejected if the tag already exists (CAS). Pure.
#[must_use]
pub fn plan_tag(repo_name: &str, name: &str, target: &Oid, who: &str) -> Plan {
    let full = format!("refs/tags/{name}");
    plan_update_ref(repo_name, &full, None, target.clone(), false, who)
}

// ---------------------------------------------------------------------------------------------
// Internals
// ---------------------------------------------------------------------------------------------

/// Read a commit's **flat** tree as `name → blob oid` (E0 supports a flat tree; nested trees are a
/// named park).
fn read_flat_tree(repo: &Repo, commit_oid: &Oid) -> Result<BTreeMap<String, Oid>, GitError> {
    let commit = repo.read_commit(commit_oid)?;
    let raw = repo.db().read(&commit.tree)?;
    let tree = crate::objectdb::parse_tree(&raw.payload)?;
    let mut out = BTreeMap::new();
    for e in tree.entries {
        out.insert(e.name, e.oid);
    }
    Ok(out)
}

/// The in-memory three-way merge over flat trees. A path is a conflict when it changed on BOTH
/// sides relative to base to **different** results. Returns the merged `name → oid` map, or a
/// typed conflict.
fn three_way_merge(
    repo_name: &str,
    base: &BTreeMap<String, Oid>,
    ours: &BTreeMap<String, Oid>,
    theirs: &BTreeMap<String, Oid>,
) -> Result<BTreeMap<String, Oid>, GitError> {
    let mut all: Vec<&String> = base
        .keys()
        .chain(ours.keys())
        .chain(theirs.keys())
        .collect();
    all.sort();
    all.dedup();
    let mut merged = BTreeMap::new();
    for name in all {
        let b = base.get(name);
        let o = ours.get(name);
        let t = theirs.get(name);
        let resolved = match (b, o, t) {
            // Both sides agree → take that value.
            (_, ours, theirs) if ours == theirs => theirs.cloned(),
            // Unchanged on ours relative to base → take theirs.
            (base, ours, theirs) if base == ours => theirs.cloned(),
            // Unchanged on theirs relative to base → take ours.
            (base, ours, theirs) if base == theirs => ours.cloned(),
            // Both sides changed to different content → conflict.
            _ => {
                return Err(GitError::MergeConflict {
                    path: format!("{repo_name}:{name}"),
                    reason: "both sides modified the path to different content".to_string(),
                });
            }
        };
        if let Some(oid) = resolved {
            merged.insert(name.clone(), oid);
        }
    }
    Ok(merged)
}

/// Build a single-parent (or root) commit payload.
fn build_commit_payload(tree: &Oid, parent: Option<&Oid>, input: &CommitInput) -> Vec<u8> {
    let parents: Vec<Oid> = parent.into_iter().cloned().collect();
    build_merge_commit_payload(tree, &parents, input)
}

/// Build a commit payload with arbitrary parents (the merge case has two).
fn build_merge_commit_payload(tree: &Oid, parents: &[Oid], input: &CommitInput) -> Vec<u8> {
    let mut s = format!("tree {}\n", tree.as_str());
    for p in parents {
        s.push_str(&format!("parent {}\n", p.as_str()));
    }
    s.push_str(&format!("author {} {} +0000\n", input.author, input.time));
    s.push_str(&format!(
        "committer {} {} +0000\n",
        input.committer, input.time
    ));
    s.push('\n');
    s.push_str(&input.message);
    if !input.message.ends_with('\n') {
        s.push('\n');
    }
    s.into_bytes()
}

fn first_line(s: &str) -> &str {
    s.lines().next().unwrap_or("")
}

fn zero_oid() -> Oid {
    Oid::zero()
}
