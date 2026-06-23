//! The relational read path: derive owned `CommitRow`/`ChangeRow`/`RefRow`/`ReflogRow` batches
//! from the repo's revwalk + refs + reflog (RFD §5). The revwalk bounds the read by `LIMIT`
//! (pushed down); anything richer than the ref-range/LIMIT the engine re-filters as a **truthful
//! residual** (the t20 lesson — this path never returns wrong rows, it returns a superset the
//! engine narrows). `commits JOIN changes` is the relational join over `sha`.

use crate::dto::{ChangeRow, CommitRow, RefRow, ReflogRow};
use crate::error::GitError;
use crate::objectdb::parse_tree;
use crate::repo::Repo;

/// Read commit rows from `ref` back `limit` commits (the pushed-down `LIMIT`). Newest first.
///
/// # Errors
/// [`GitError`] if the ref or a walked object does not resolve.
pub fn commits(repo: &Repo, reference: &str, limit: usize) -> Result<Vec<CommitRow>, GitError> {
    let start = repo.resolve_ref(reference)?;
    Ok(repo
        .revwalk(&start, limit)?
        .iter()
        .map(|(oid, c)| CommitRow::from_commit(oid.as_str(), c))
        .collect())
}

/// Read the exploded per-file change rows for the commits on the `ref` walk (`git log
/// --name-status` as a table, JOINable to `commits` on `sha`). A change is computed by diffing
/// each commit's flat tree against its first parent's: present-only = Added, absent-only =
/// Deleted, different-oid = Modified. Line counts are best-effort (0 at E0 for binary-safe rows;
/// the diff-cost park).
///
/// # Errors
/// [`GitError`] if the ref or a walked object does not resolve.
pub fn changes(repo: &Repo, reference: &str, limit: usize) -> Result<Vec<ChangeRow>, GitError> {
    let start = repo.resolve_ref(reference)?;
    let history = repo.revwalk(&start, limit)?;
    let mut out = Vec::new();
    for (oid, commit) in &history {
        let cur = flat_tree(repo, &commit.tree)?;
        let parent_tree = match commit.parents.first() {
            Some(p) => {
                let pc = repo.read_commit(p)?;
                flat_tree(repo, &pc.tree)?
            }
            None => Vec::new(),
        };
        for (name, child_oid) in &cur {
            match parent_tree.iter().find(|(n, _)| n == name) {
                None => out.push(ChangeRow {
                    sha: oid.as_str().to_string(),
                    path: name.clone(),
                    status: "A".to_string(),
                    added: 0,
                    removed: 0,
                }),
                Some((_, poid)) if poid != child_oid => out.push(ChangeRow {
                    sha: oid.as_str().to_string(),
                    path: name.clone(),
                    status: "M".to_string(),
                    added: 0,
                    removed: 0,
                }),
                _ => {}
            }
        }
        for (name, _) in &parent_tree {
            if !cur.iter().any(|(n, _)| n == name) {
                out.push(ChangeRow {
                    sha: oid.as_str().to_string(),
                    path: name.clone(),
                    status: "D".to_string(),
                    added: 0,
                    removed: 0,
                });
            }
        }
    }
    Ok(out)
}

/// Read all refs as rows (the `/refs` node). For `/tags`, the caller filters to `refs/tags/*`.
#[must_use]
pub fn refs(repo: &Repo) -> Vec<RefRow> {
    repo.refs()
        .into_iter()
        .map(|(name, oid)| RefRow {
            name,
            oid: oid.as_str().to_string(),
        })
        .collect()
}

/// Read only tag refs (the `/tags` node) — the truthful residual `name LIKE 'refs/tags/%'` pushed
/// into the source.
#[must_use]
pub fn tags(repo: &Repo) -> Vec<RefRow> {
    refs(repo)
        .into_iter()
        .filter(|r| r.name.starts_with("refs/tags/"))
        .collect()
}

/// Read the reflog tail for a ref, newest first (the `/reflog` append-log read — the recovery
/// oracle).
#[must_use]
pub fn reflog(repo: &Repo, ref_name: &str) -> Vec<ReflogRow> {
    repo.reflog(ref_name)
        .iter()
        .map(ReflogRow::from_entry)
        .collect()
}

/// A commit's flat tree as `(name, oid)` pairs.
fn flat_tree(
    repo: &Repo,
    tree_oid: &crate::objectdb::Oid,
) -> Result<Vec<(String, String)>, GitError> {
    let raw = repo.db().read(tree_oid)?;
    let tree = parse_tree(&raw.payload)?;
    Ok(tree
        .entries
        .into_iter()
        .filter(|e| !e.is_tree())
        .map(|e| (e.name, e.oid.as_str().to_string()))
        .collect())
}
