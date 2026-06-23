//! [`Repo`] + [`RepoResolver`] — a mounted repository: its [`ObjectDb`], its refs
//! (`refs/heads/*`, `refs/tags/*`, `HEAD`, packed-refs), and its reflog. This is where the §4
//! temporal coordinate `@<ref>` (branch/tag/sha/`HEAD~n`) resolves to an [`Oid`], and where the
//! revwalk (first-parent / full-history commit walk) lives — the read substrate the relational
//! nodes derive their rows from. Owned data only.

use std::collections::HashMap;
use std::sync::Arc;

use crate::error::GitError;
use crate::objectdb::{parse_commit, Commit, ObjectDb, ObjectKind, Oid};

/// One reflog entry (the recovery oracle, RFD §6): the ref moved from `old` to `new` with a
/// message. The append-log `/reflog` node tails these; the recovery helper reads `old` to
/// restore a forced move.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReflogEntry {
    /// The ref this entry belongs to (e.g. `refs/heads/main`).
    pub ref_name: String,
    /// The oid the ref pointed at before the move (`0`×40 for a creation).
    pub old: Oid,
    /// The oid the ref points at after the move.
    pub new: Oid,
    /// The committer/actor identity line.
    pub who: String,
    /// The reflog message (e.g. `commit: …`, `merge …`).
    pub message: String,
    /// The entry epoch seconds.
    pub time: i64,
}

/// A mounted repository — refs + reflog over a shared [`ObjectDb`]. Cheaply cloneable (the
/// object db is behind an `Arc`). The reflog is kept as an append-only Vec (newest last); the
/// `/reflog` node tails it and the recovery helper reads the prior oid from it.
#[derive(Clone)]
pub struct Repo {
    db: Arc<dyn ObjectDb>,
    /// Ref name (`refs/heads/main`, `refs/tags/v1`, `HEAD`) → its target oid (or symbolic
    /// target for `HEAD`).
    refs: HashMap<String, RefTarget>,
    /// The append-only reflog, keyed by ref name, newest entry last.
    reflog: HashMap<String, Vec<ReflogEntry>>,
}

/// What a ref points at: a concrete oid, or (for `HEAD`) a symbolic pointer to another ref.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefTarget {
    /// A direct oid (a branch tip, a lightweight tag).
    Direct(Oid),
    /// A symbolic ref (`HEAD -> refs/heads/main`).
    Symbolic(String),
}

impl Repo {
    /// Build a repo over an object database with no refs.
    #[must_use]
    pub fn new(db: Arc<dyn ObjectDb>) -> Self {
        Self {
            db,
            refs: HashMap::new(),
            reflog: HashMap::new(),
        }
    }

    /// The object database (the read path resolves objects through this).
    #[must_use]
    pub fn db(&self) -> &Arc<dyn ObjectDb> {
        &self.db
    }

    /// Set a ref to a concrete oid (builder; used by the fixture + the apply leg).
    pub fn set_ref(&mut self, name: impl Into<String>, target: Oid) {
        self.refs.insert(name.into(), RefTarget::Direct(target));
    }

    /// Set `HEAD` (or another symbolic ref) to point at `target_ref`.
    pub fn set_symbolic(&mut self, name: impl Into<String>, target_ref: impl Into<String>) {
        self.refs
            .insert(name.into(), RefTarget::Symbolic(target_ref.into()));
    }

    /// Append a reflog entry for a ref (newest last).
    pub fn append_reflog(&mut self, entry: ReflogEntry) {
        self.reflog
            .entry(entry.ref_name.clone())
            .or_default()
            .push(entry);
    }

    /// All refs (for the `/refs` and `/tags` relational nodes), as `(name, oid)` pairs with
    /// symbolic refs resolved to their concrete tip.
    #[must_use]
    pub fn refs(&self) -> Vec<(String, Oid)> {
        let mut out = Vec::new();
        for (name, target) in &self.refs {
            if let Some(oid) = self.resolve_target(target) {
                out.push((name.clone(), oid));
            }
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    /// The reflog entries for a ref, newest first (the `/reflog` tail read).
    #[must_use]
    pub fn reflog(&self, ref_name: &str) -> Vec<ReflogEntry> {
        self.reflog
            .get(ref_name)
            .map(|v| v.iter().rev().cloned().collect())
            .unwrap_or_default()
    }

    /// The current concrete oid of a ref, if it exists (resolving a symbolic `HEAD`).
    #[must_use]
    pub fn ref_oid(&self, name: &str) -> Option<Oid> {
        self.refs.get(name).and_then(|t| self.resolve_target(t))
    }

    fn resolve_target(&self, target: &RefTarget) -> Option<Oid> {
        match target {
            RefTarget::Direct(oid) => Some(oid.clone()),
            RefTarget::Symbolic(inner) => self.refs.get(inner).and_then(|t| self.resolve_target(t)),
        }
    }

    /// Resolve a §4 temporal coordinate `@<ref>` to an [`Oid`]: a branch (`refs/heads/<r>`), a
    /// tag (`refs/tags/<r>`), a 40-hex sha, `HEAD`, or `HEAD~<n>` / `<ref>~<n>` (the n-th
    /// first-parent ancestor). An annotated-tag ref dereferences to the commit it tags.
    ///
    /// # Errors
    /// [`GitError::UnresolvedRef`] when the coordinate names nothing resolvable.
    pub fn resolve_ref(&self, reference: &str) -> Result<Oid, GitError> {
        // `<base>~<n>` walks n first-parents from <base>.
        if let Some((base, n)) = reference.split_once('~') {
            let n: usize = n.parse().map_err(|_| GitError::UnresolvedRef {
                reference: reference.to_string(),
            })?;
            let mut oid = self.resolve_ref(base)?;
            for _ in 0..n {
                let commit = self.read_commit(&oid)?;
                oid = commit
                    .parents
                    .first()
                    .cloned()
                    .ok_or_else(|| GitError::UnresolvedRef {
                        reference: reference.to_string(),
                    })?;
            }
            return Ok(oid);
        }

        // A bare 40-hex sha.
        if reference.len() == 40 && reference.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Oid::parse(reference).map_err(|_| GitError::UnresolvedRef {
                reference: reference.to_string(),
            });
        }

        // A named ref: try HEAD, then the fully-qualified name, then heads/tags shorthands.
        let candidates = [
            reference.to_string(),
            format!("refs/heads/{reference}"),
            format!("refs/tags/{reference}"),
        ];
        for cand in candidates {
            if let Some(oid) = self.ref_oid(&cand) {
                return self.peel_to_commit(&oid);
            }
        }
        Err(GitError::UnresolvedRef {
            reference: reference.to_string(),
        })
    }

    /// Dereference an annotated tag to the commit it ultimately points at (a non-tag oid is
    /// returned as-is).
    fn peel_to_commit(&self, oid: &Oid) -> Result<Oid, GitError> {
        let raw = self.db.read(oid)?;
        match raw.kind {
            ObjectKind::Tag => {
                let tag = crate::objectdb::parse_tag(&raw.payload)?;
                self.peel_to_commit(&tag.object)
            }
            _ => Ok(oid.clone()),
        }
    }

    /// Read + parse the commit object at `oid`.
    ///
    /// # Errors
    /// [`GitError::ObjectNotFound`]/[`GitError::Corrupt`] on a missing/malformed commit.
    pub fn read_commit(&self, oid: &Oid) -> Result<Commit, GitError> {
        let raw = self.db.read(oid)?;
        if raw.kind != ObjectKind::Commit {
            return Err(GitError::Corrupt {
                reason: format!("object {} is a {:?}, not a commit", oid.short(), raw.kind),
            });
        }
        parse_commit(&raw.payload)
    }

    /// Walk commit history from `start` (inclusive) in first-parent order up to `limit`
    /// commits. Returns `(oid, Commit)` pairs newest-first. This is the revwalk the relational
    /// `/commits` node derives rows from; `limit` is the pushed-down `LIMIT` bound (RFD §6 —
    /// bound the walk).
    ///
    /// # Errors
    /// [`GitError`] if an object on the walk is missing or malformed.
    pub fn revwalk(&self, start: &Oid, limit: usize) -> Result<Vec<(Oid, Commit)>, GitError> {
        let mut out = Vec::new();
        let mut cursor = Some(start.clone());
        while let Some(oid) = cursor {
            if out.len() >= limit {
                break;
            }
            let commit = self.read_commit(&oid)?;
            cursor = commit.parents.first().cloned();
            out.push((oid, commit));
        }
        Ok(out)
    }
}

/// The per-mount repository registry (RFD §5): the engine builds it from the configured repos;
/// the driver resolves a handle by the path's `<repo>` segment.
#[derive(Clone, Default)]
pub struct RepoResolver {
    repos: HashMap<String, Repo>,
}

impl RepoResolver {
    /// An empty resolver.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a repo under `name`.
    #[must_use]
    pub fn with_repo(mut self, name: impl Into<String>, repo: Repo) -> Self {
        self.repos.insert(name.into(), repo);
        self
    }

    /// Look up a repo handle by name.
    ///
    /// # Errors
    /// [`GitError::UnknownRepo`] if no repo is registered under `name`.
    pub fn repo(&self, name: &str) -> Result<&Repo, GitError> {
        self.repos.get(name).ok_or_else(|| GitError::UnknownRepo {
            repo: name.to_string(),
        })
    }

    /// Whether a repo is registered (the introspective capability gate uses this).
    #[must_use]
    pub fn has_repo(&self, name: &str) -> bool {
        self.repos.contains_key(name)
    }
}
