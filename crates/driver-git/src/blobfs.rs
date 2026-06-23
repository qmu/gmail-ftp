//! The **BlobFs** read path (Blob archetype): `ls` a tree at any `@<ref>`, read a blob's exact
//! bytes at any `@<ref>`, and hand those bytes to the **t15 codec registry** so a committed
//! `*.md`/`*.json`/`*.yaml`/`*.toml`/`*.csv` decodes to frontmatter columns + body (RFD §4
//! blob↔rows interop). Pure reads over the in-memory object DB — no I/O beyond the object store.

use cfs_codec::{Codec, RowBatch};

use crate::dto::{blob_listing_schema, BlameRow};
use crate::error::GitError;
use crate::objectdb::{parse_tree, ObjectKind, Oid, TreeEntry};
use crate::repo::Repo;
use cfs_types::{Row, Value};

/// `ls` the tree at `ref`/`path`: resolve the ref to a commit, walk to the tree at `path`, and
/// list its entries as `(name, mode, oid, kind)` rows. An empty `path` lists the root tree.
///
/// # Errors
/// [`GitError`] if the ref/path does not resolve to a tree.
pub fn ls(repo: &Repo, reference: &str, path: &str) -> Result<RowBatch, GitError> {
    let commit_oid = repo.resolve_ref(reference)?;
    let commit = repo.read_commit(&commit_oid)?;
    let tree_oid = walk_to_tree(repo, &commit.tree, path)?;
    let raw = repo.db().read(&tree_oid)?;
    let tree = parse_tree(&raw.payload)?;
    let rows = tree
        .entries
        .iter()
        .map(|e| {
            Row::new(vec![
                Value::Text(e.name.clone()),
                Value::Text(e.mode.clone()),
                Value::Text(e.oid.as_str().to_string()),
                Value::Text(entry_kind(e).to_string()),
            ])
        })
        .collect();
    Ok(RowBatch::new(blob_listing_schema(), rows))
}

/// Read the **exact bytes** of the blob at `ref`/`path` (the §4 versioned cat).
///
/// # Errors
/// [`GitError`] if the ref/path does not resolve to a blob.
pub fn cat(repo: &Repo, reference: &str, path: &str) -> Result<Vec<u8>, GitError> {
    let commit_oid = repo.resolve_ref(reference)?;
    let commit = repo.read_commit(&commit_oid)?;
    let blob_oid = resolve_blob(repo, &commit.tree, path)?;
    let raw = repo.db().read(&blob_oid)?;
    if raw.kind != ObjectKind::Blob {
        return Err(GitError::Corrupt {
            reason: format!("{path} is not a blob"),
        });
    }
    Ok(raw.payload)
}

/// Read the blob at `ref`/`path` and **decode** it through a [`Codec`] (the t15 interop): a
/// committed `*.md` yields frontmatter columns + `body`. The caller selects the codec by format
/// (the engine's `DECODE fmt` resolves it from the registry); this is the blob→rows bridge.
///
/// # Errors
/// [`GitError`] if the blob is unresolved; the codec's [`cfs_codec::CfsError`] is mapped to a
/// [`GitError::Corrupt`] (a decode failure over committed bytes).
pub fn cat_decode(
    repo: &Repo,
    reference: &str,
    path: &str,
    codec: &dyn Codec,
) -> Result<RowBatch, GitError> {
    let bytes = cat(repo, reference, path)?;
    codec.decode(&bytes).map_err(|e| GitError::Corrupt {
        reason: format!("DECODE {} of {path}: {e}", codec.fmt()),
    })
}

/// Compute `/blame` for a file at a ref: attribute each line to the most recent commit on the
/// first-parent walk whose version of the file differs from its parent's (a simple,
/// bounded last-touched attribution — the deep-history blame engine is a named park). Returns
/// owned [`BlameRow`]s.
///
/// # Errors
/// [`GitError`] if the ref or file does not resolve.
pub fn blame(
    repo: &Repo,
    reference: &str,
    file: &str,
    limit: usize,
) -> Result<Vec<BlameRow>, GitError> {
    let start = repo.resolve_ref(reference)?;
    let history = repo.revwalk(&start, limit)?;
    // Find the newest commit on the walk whose version of `file` differs from its first parent's
    // (the last-touched commit). Walking newest-first, the FIRST such commit is the answer.
    let mut attributing: Option<(Oid, String, i64)> = None;
    for (oid, commit) in &history {
        let cur = resolve_blob(repo, &commit.tree, file).ok();
        let parent_blob = match commit.parents.first() {
            Some(p) => {
                let pc = repo.read_commit(p)?;
                resolve_blob(repo, &pc.tree, file).ok()
            }
            None => None, // a root commit "introduces" the file.
        };
        if cur != parent_blob {
            attributing = Some((oid.clone(), commit.author.clone(), commit.committer_time));
            break;
        }
    }
    let (sha, author, time) = attributing.ok_or_else(|| GitError::UnresolvedRef {
        reference: reference.to_string(),
    })?;
    // Attribute every line of the file's current content to that commit.
    let content = cat(repo, reference, file)?;
    let text = String::from_utf8_lossy(&content);
    let rows = text
        .lines()
        .enumerate()
        .map(|(i, _line)| BlameRow {
            path: file.to_string(),
            line: (i + 1) as i64,
            sha: sha.as_str().to_string(),
            author: author.clone(),
            time,
        })
        .collect();
    Ok(rows)
}

/// Walk a (flat, at E0) tree to the subtree addressed by `path` (empty path = the tree itself).
fn walk_to_tree(repo: &Repo, root_tree: &Oid, path: &str) -> Result<Oid, GitError> {
    if path.is_empty() {
        return Ok(root_tree.clone());
    }
    // A flat fixture has no subtrees; a non-empty path that is not a directory entry is an error.
    let raw = repo.db().read(root_tree)?;
    let tree = parse_tree(&raw.payload)?;
    for e in &tree.entries {
        if e.name == path && e.is_tree() {
            return Ok(e.oid.clone());
        }
    }
    Err(GitError::InvalidPath {
        path: path.to_string(),
        reason: "no such subtree at ref",
    })
}

/// Resolve a blob oid at a (flat) tree by file name.
fn resolve_blob(repo: &Repo, root_tree: &Oid, path: &str) -> Result<Oid, GitError> {
    let raw = repo.db().read(root_tree)?;
    let tree = parse_tree(&raw.payload)?;
    tree.entries
        .iter()
        .find(|e| e.name == path && !e.is_tree())
        .map(|e| e.oid.clone())
        .ok_or_else(|| GitError::InvalidPath {
            path: path.to_string(),
            reason: "no such blob at ref",
        })
}

/// The `kind` label for a tree entry's listing row.
fn entry_kind(e: &TreeEntry) -> &'static str {
    if e.is_tree() {
        "tree"
    } else {
        "blob"
    }
}
