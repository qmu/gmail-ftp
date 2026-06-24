//! [`GitHubPath`] — the parse of a qfs [`Path`](qfs_driver::Path) into the concrete GitHub node
//! it names (RFD-0001 §5). GitHub maps onto the **object-graph + workflow** archetype: a repo is
//! a graph of namespaced collections (`issues`, `pulls`, …), each a list of objects addressable
//! by number/id, plus irreducible `CALL` procedures (`merge`/`dispatch`/`review`).
//!
//! ## Addressing
//! - `/github/{owner}/{repo}` — the repo root.
//! - `/github/{owner}/{repo}/<namespace>` — a collection (list): `issues`, `pulls`, `comments`,
//!   `reviews`, `runs`, `releases`, `files`, `branches`.
//! - `/github/{owner}/{repo}/<namespace>/<id>` — one object addressed by number/id/name.
//! - `/github/{owner}/{repo}/<namespace>/<id>/<sub>` — a sub-collection (e.g.
//!   `issues/123/comments`, `pulls/7/reviews`).
//!
//! Pure parsing only — no I/O. Owned data only; no vendor type crosses.

use qfs_driver::Path;

use crate::error::GitHubError;

/// The mount this driver answers for.
pub const MOUNT: &str = "/github";

/// The eight namespaces a `/github/{owner}/{repo}` mount exposes (RFD §5). A **closed** set —
/// declaration order is the canonical order used by golden snapshots and capability gating.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Namespace {
    /// `issues` — repo issues (list/get, open, close/edit, comment).
    Issues,
    /// `pulls` — pull requests (list/get, open, close/edit; merge/review via CALL).
    Pulls,
    /// `comments` — issue/PR comments (list, post, delete).
    Comments,
    /// `reviews` — PR reviews (list; submit via CALL `review`).
    Reviews,
    /// `runs` — GitHub Actions workflow runs (read-only list/get; dispatch via CALL).
    Runs,
    /// `releases` — repo releases (list/get, create, delete).
    Releases,
    /// `files` — GitHub-API content metadata views (read; NOT a working tree — see the boundary
    /// doc in the crate root). The git working tree belongs to the t26 git driver.
    Files,
    /// `branches` — branch-ref metadata (read; create/delete a ref). NOT a working tree.
    Branches,
}

impl Namespace {
    /// The canonical declaration-ordered list of every namespace — the single source of truth
    /// for `DESCRIBE` enumeration and the namespace/segment tie test.
    pub const ALL: [Namespace; 8] = [
        Namespace::Issues,
        Namespace::Pulls,
        Namespace::Comments,
        Namespace::Reviews,
        Namespace::Runs,
        Namespace::Releases,
        Namespace::Files,
        Namespace::Branches,
    ];

    /// The path segment that names this namespace (e.g. `issues`).
    #[must_use]
    pub const fn segment(self) -> &'static str {
        match self {
            Namespace::Issues => "issues",
            Namespace::Pulls => "pulls",
            Namespace::Comments => "comments",
            Namespace::Reviews => "reviews",
            Namespace::Runs => "runs",
            Namespace::Releases => "releases",
            Namespace::Files => "files",
            Namespace::Branches => "branches",
        }
    }

    /// Parse a path segment into its [`Namespace`], if it names one.
    #[must_use]
    pub fn from_segment(segment: &str) -> Option<Self> {
        Namespace::ALL.into_iter().find(|n| n.segment() == segment)
    }
}

/// A parsed GitHub address — what a `/github/{owner}/{repo}/...` path resolves to. Owned,
/// vendor-free. The applier and the introspective methods branch on this.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct GitHubPath {
    /// The repo owner (`{owner}` segment).
    pub owner: String,
    /// The repo name (`{repo}` segment).
    pub repo: String,
    /// The namespace this address selects, if it names one. `None` is the bare repo root.
    pub namespace: Option<Namespace>,
    /// The object id/number addressed within the namespace, if any (e.g. issue `123`).
    pub id: Option<String>,
    /// A sub-collection segment under an object (e.g. `comments` in `issues/123/comments`).
    pub sub: Option<Namespace>,
    /// A sub-object id under the sub-collection (e.g. a comment id).
    pub sub_id: Option<String>,
}

impl GitHubPath {
    /// Parse a driver [`Path`] into a [`GitHubPath`].
    ///
    /// # Errors
    /// [`GitHubError::InvalidPath`] if the path is not under `/github/{owner}/{repo}`, or a
    /// namespace segment names no known namespace.
    pub fn parse(path: &Path) -> Result<Self, GitHubError> {
        Self::parse_str(path.as_str())
    }

    /// Parse a raw path string into a [`GitHubPath`] (the core parse).
    ///
    /// # Errors
    /// [`GitHubError::InvalidPath`] on a malformed address.
    pub fn parse_str(raw: &str) -> Result<Self, GitHubError> {
        let trimmed = raw.trim_end_matches('/');
        let Some(after) = trimmed.strip_prefix(&format!("{MOUNT}/")) else {
            return Err(GitHubError::InvalidPath {
                path: raw.to_string(),
                reason: "path is not under the /github mount",
            });
        };
        let segments: Vec<&str> = after.split('/').filter(|s| !s.is_empty()).collect();
        let (owner, repo) = match segments.as_slice() {
            [owner, repo, ..] => ((*owner).to_string(), (*repo).to_string()),
            _ => {
                return Err(GitHubError::InvalidPath {
                    path: raw.to_string(),
                    reason: "a GitHub path must name /github/{owner}/{repo}",
                })
            }
        };
        let rest = &segments[2..];
        let namespace = match rest.first() {
            None => None,
            Some(seg) => {
                Some(
                    Namespace::from_segment(seg).ok_or_else(|| GitHubError::InvalidPath {
                        path: raw.to_string(),
                        reason: "unknown GitHub namespace segment",
                    })?,
                )
            }
        };
        let id = rest.get(1).map(|s| (*s).to_string());
        let sub = match rest.get(2) {
            None => None,
            Some(seg) => {
                Some(
                    Namespace::from_segment(seg).ok_or_else(|| GitHubError::InvalidPath {
                        path: raw.to_string(),
                        reason: "unknown GitHub sub-collection segment",
                    })?,
                )
            }
        };
        let sub_id = rest.get(3).map(|s| (*s).to_string());
        Ok(Self {
            owner,
            repo,
            namespace,
            id,
            sub,
            sub_id,
        })
    }

    /// The `owner/repo` slug the REST URL is built from.
    #[must_use]
    pub fn slug(&self) -> String {
        format!("{}/{}", self.owner, self.repo)
    }

    /// Whether this address names a bare collection (a namespace, no object id) — the listable
    /// node a `SELECT` without a key reads.
    #[must_use]
    pub fn is_collection(&self) -> bool {
        self.namespace.is_some() && self.id.is_none()
    }

    /// The *effective* namespace a verb gates against: the sub-collection if present, else the
    /// top-level namespace. `issues/123/comments` gates as `comments`.
    #[must_use]
    pub fn effective_namespace(&self) -> Option<Namespace> {
        self.sub.or(self.namespace)
    }

    /// The id of the addressed *object* within the effective namespace: the sub-object id when a
    /// sub-collection is present (`issues/1/comments/55` → `55`), else the top-level id
    /// (`releases/9` → `9`). This is the id a REMOVE/edit targets — distinct from the parent
    /// number a sub-collection INSERT attaches to (which is [`GitHubPath::id`]).
    #[must_use]
    pub fn object_id(&self) -> Option<&str> {
        if self.sub.is_some() {
            self.sub_id.as_deref()
        } else {
            self.id.as_deref()
        }
    }
}
