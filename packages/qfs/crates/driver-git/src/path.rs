//! [`GitPath`] — the owned DTO a `/git/<repo>[@<ref>]/<rest>` virtual path parses into
//! (RFD-0001 §4 temporal coordinate). It classifies the addressed node into one of the four
//! archetypes' sub-paths and carries the `@<ref>` coordinate (branch/tag/sha/`HEAD~n`) the
//! resolver turns into an `ObjectId`. Owned data only — no vendor type appears here (RFD §9).
//!
//! ## Grammar
//! ```text
//! /git/<repo>[@<ref>]/<rest>
//! ```
//! - `<repo>` — the mounted repository segment.
//! - `@<ref>` — the §4 temporal coordinate. Optional; defaults to `HEAD`. Forms:
//!   `@<branch>`, `@<tag>`, `@<40-hex-sha>`, `@HEAD`, `@HEAD~<n>`.
//! - `<rest>` — either a **relational/log node name** (`commits`/`changes`/`blame`/`refs`/
//!   `tags`/`reflog`) or, for everything else, a **BlobFs** tree/blob path read at `@<ref>`.
//!
//! ## Path canonicalisation (RFD §10 least privilege)
//! A `<rest>` containing a `..` segment is rejected (`InvalidPath`) so a read can never
//! traverse outside the addressed tree.

use crate::error::GitError;

/// The named relational/log nodes (everything else under a repo is a BlobFs path).
pub const COMMITS: &str = "commits";
/// The exploded per-file change rows node (relational, JOINable to commits).
pub const CHANGES: &str = "changes";
/// The line-attribution node (relational).
pub const BLAME: &str = "blame";
/// The refs pointer node (relational; SELECT + UPDATE).
pub const REFS: &str = "refs";
/// The tags pointer node (relational; SELECT + UPDATE).
pub const TAGS: &str = "tags";
/// The reflog append-log node (tail SELECT; the recovery oracle).
pub const REFLOG: &str = "reflog";

/// The git driver mount point.
pub const MOUNT: &str = "/git";

/// A parsed `/git/<repo>[@<ref>]/<rest>` address. Owned; the `@<ref>` is kept unresolved here
/// (a pure parse) and resolved to an `ObjectId` by the `RepoResolver` against the object DB.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct GitPath {
    /// The repository segment.
    pub repo: String,
    /// The `@<ref>` temporal coordinate text (defaults to `HEAD` when omitted).
    pub reference: String,
    /// The classified node under the repo.
    pub node: GitNode,
}

/// Which node a `/git/<repo>@<ref>/<rest>` address resolves to — one per archetype sub-path.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum GitNode {
    /// The repo root (`/git/<repo>` with no `<rest>`) — a describable BlobFs tree root.
    Root,
    /// A versioned-blob FS path (Blob archetype): a tree or blob at the addressed `@<ref>`.
    Blob {
        /// The path within the tree (empty = the root tree of the ref).
        path: String,
    },
    /// The relational commits node.
    Commits,
    /// The relational per-file changes node.
    Changes,
    /// The relational blame node (its `<rest>` after `blame/` is the file path).
    Blame {
        /// The file whose lines are attributed (empty = unspecified, requires WHERE).
        file: String,
    },
    /// The relational refs pointer node.
    Refs,
    /// The relational tags pointer node.
    Tags,
    /// The append-log reflog node.
    Reflog,
}

impl GitPath {
    /// Parse a `/git/<repo>[@<ref>]/<rest>` path string into the owned DTO.
    ///
    /// # Errors
    /// [`GitError::InvalidPath`] when the path is not under `/git`, lacks a repo segment, or a
    /// `<rest>` contains a `..` traversal segment.
    pub fn parse(raw: &str) -> Result<Self, GitError> {
        let rest = raw.strip_prefix(MOUNT).ok_or(GitError::InvalidPath {
            path: raw.to_string(),
            reason: "not a /git path",
        })?;
        let rest = rest.strip_prefix('/').unwrap_or(rest);
        if rest.is_empty() {
            return Err(GitError::InvalidPath {
                path: raw.to_string(),
                reason: "missing <repo> segment",
            });
        }

        // Split off the first segment (`<repo>[@<ref>]`) from the remaining `<rest>`.
        let (repo_seg, tail) = match rest.split_once('/') {
            Some((head, tail)) => (head, tail),
            None => (rest, ""),
        };
        let (repo, reference) = match repo_seg.split_once('@') {
            Some((repo, r)) if !r.is_empty() => (repo.to_string(), r.to_string()),
            _ => (repo_seg.to_string(), "HEAD".to_string()),
        };
        if repo.is_empty() {
            return Err(GitError::InvalidPath {
                path: raw.to_string(),
                reason: "empty <repo> segment",
            });
        }

        // Canonicalisation: reject `..` traversal anywhere in the rest (RFD §10).
        if tail.split('/').any(|seg| seg == "..") {
            return Err(GitError::InvalidPath {
                path: raw.to_string(),
                reason: "`..` traversal is not permitted",
            });
        }

        let node = Self::classify(tail);
        Ok(Self {
            repo,
            reference,
            node,
        })
    }

    /// Classify the `<rest>` into a node. The named relational/log nodes win; anything else is
    /// a BlobFs tree/blob path read at the ref.
    fn classify(tail: &str) -> GitNode {
        if tail.is_empty() {
            return GitNode::Root;
        }
        let (head, sub) = match tail.split_once('/') {
            Some((h, s)) => (h, s),
            None => (tail, ""),
        };
        match head {
            COMMITS if sub.is_empty() => GitNode::Commits,
            CHANGES if sub.is_empty() => GitNode::Changes,
            REFS if sub.is_empty() => GitNode::Refs,
            TAGS if sub.is_empty() => GitNode::Tags,
            REFLOG if sub.is_empty() => GitNode::Reflog,
            BLAME => GitNode::Blame {
                file: sub.to_string(),
            },
            // Everything else (including a `commits/…` deeper path) is a blob/tree path.
            _ => GitNode::Blob {
                path: tail.to_string(),
            },
        }
    }

    /// Whether this address is one of the relational nodes (commits/changes/blame/refs/tags).
    #[must_use]
    pub fn is_relational(&self) -> bool {
        matches!(
            self.node,
            GitNode::Commits
                | GitNode::Changes
                | GitNode::Blame { .. }
                | GitNode::Refs
                | GitNode::Tags
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_blob_at_ref() {
        let p = GitPath::parse("/git/repo@main/src/lib.rs").unwrap();
        assert_eq!(p.repo, "repo");
        assert_eq!(p.reference, "main");
        assert_eq!(
            p.node,
            GitNode::Blob {
                path: "src/lib.rs".to_string()
            }
        );
    }

    #[test]
    fn defaults_ref_to_head() {
        let p = GitPath::parse("/git/repo/README.md").unwrap();
        assert_eq!(p.reference, "HEAD");
        assert_eq!(
            p.node,
            GitNode::Blob {
                path: "README.md".to_string()
            }
        );
    }

    #[test]
    fn classifies_relational_nodes() {
        assert_eq!(
            GitPath::parse("/git/r/commits").unwrap().node,
            GitNode::Commits
        );
        assert_eq!(
            GitPath::parse("/git/r/changes").unwrap().node,
            GitNode::Changes
        );
        assert_eq!(GitPath::parse("/git/r/refs").unwrap().node, GitNode::Refs);
        assert_eq!(GitPath::parse("/git/r/tags").unwrap().node, GitNode::Tags);
        assert_eq!(
            GitPath::parse("/git/r/reflog").unwrap().node,
            GitNode::Reflog
        );
        assert_eq!(
            GitPath::parse("/git/r/blame/src/main.rs").unwrap().node,
            GitNode::Blame {
                file: "src/main.rs".to_string()
            }
        );
    }

    #[test]
    fn repo_root_is_a_tree_root() {
        let p = GitPath::parse("/git/repo@v1.0").unwrap();
        assert_eq!(p.reference, "v1.0");
        assert_eq!(p.node, GitNode::Root);
    }

    #[test]
    fn rejects_traversal_and_non_git() {
        assert_eq!(
            GitPath::parse("/git/repo@main/../etc").unwrap_err().code(),
            "invalid_path"
        );
        assert_eq!(
            GitPath::parse("/s3/bucket/key").unwrap_err().code(),
            "invalid_path"
        );
        assert_eq!(GitPath::parse("/git").unwrap_err().code(), "invalid_path");
    }
}
