//! [`DrivePath`] — the parse of a qfs [`Path`](qfs_driver::Path) / `id:` address into the
//! concrete Drive node it names (RFD-0001 §5). Drive maps onto the **Blob/namespace
//! archetype**: **folders = directories, files = blobs**, addressed by a path over parent
//! pointers or directly by file id (`id:<fileId>`).
//!
//! ## Addressing
//! - `/drive` — the virtual root; lists `my/` and `shared/` (the two corpora).
//! - `/drive/my` — My Drive root; lists its children.
//! - `/drive/my/<a>/<b>` — a path under My Drive (resolved by walking parent pointers).
//! - `/drive/shared` — the Shared Drives root; lists the named shared drives.
//! - `/drive/shared/<driveName>` — a Shared Drive root; lists its children.
//! - `/drive/shared/<driveName>/<a>/<b>` — a path inside a Shared Drive.
//! - `id:<fileId>` — a single file/folder addressed directly by its Drive file id.
//!
//! A trailing `@<rev>` on the last segment pins a revision (`report.txt@v3`), surfaced in
//! [`DrivePath::revision`]. Pure parsing only — no I/O. Owned data only; no vendor type crosses.

use qfs_driver::Path;

use crate::error::DriveError;

/// The mount this driver answers for. The virtual root lists the two corpora (`my`, `shared`).
pub const MOUNT: &str = "/drive";

/// The reserved segment naming the My Drive corpus.
pub const MY_SEGMENT: &str = "my";

/// The reserved segment naming the Shared Drives corpus.
pub const SHARED_SEGMENT: &str = "shared";

/// A parsed Drive address — what a `/drive/...` path or an `id:` selector resolves to.
/// Owned, vendor-free. The applier and the introspective methods branch on this. A path's
/// optional `@<rev>` revision pin rides alongside in [`Resolved`]-construction time; the
/// pure parse keeps it on the leaf variants via [`DrivePath::revision`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum DrivePath {
    /// `/drive` — the virtual root (lists `my`/`shared`).
    Root,
    /// `/drive/my` — the My Drive corpus root.
    MyRoot,
    /// `/drive/my/<segments...>` — a path under My Drive.
    My {
        /// The path segments below the My Drive root (folder names, last may name a file).
        segments: Vec<String>,
        /// The pinned revision id, if the address carried an `@<rev>` suffix.
        revision: Option<String>,
    },
    /// `/drive/shared` — the Shared Drives corpus root (lists the named drives).
    SharedRoot,
    /// `/drive/shared/<driveName>/<segments...>` — a path inside a Shared Drive.
    Shared {
        /// The Shared Drive name (the first segment under `/drive/shared`).
        drive: String,
        /// The path segments below the Shared Drive root.
        segments: Vec<String>,
        /// The pinned revision id, if the address carried an `@<rev>` suffix.
        revision: Option<String>,
    },
    /// `id:<fileId>` — a file/folder addressed directly by its Drive file id.
    ById {
        /// The Drive file id.
        id: String,
        /// The pinned revision id, if the address carried an `@<rev>` suffix.
        revision: Option<String>,
    },
}

impl DrivePath {
    /// Parse a driver [`Path`] string into a [`DrivePath`].
    ///
    /// # Errors
    /// [`DriveError::InvalidPath`] if the path is not under `/drive`, an `id:` selector is
    /// empty, or a Shared Drive path names no drive.
    pub fn parse(path: &Path) -> Result<Self, DriveError> {
        Self::parse_str(path.as_str())
    }

    /// Parse a raw path/selector string into a [`DrivePath`] (the core parse).
    ///
    /// # Errors
    /// [`DriveError::InvalidPath`] on a malformed address.
    pub fn parse_str(raw: &str) -> Result<Self, DriveError> {
        // `id:` addressing — a file/folder by id, independent of any corpus.
        if let Some(rest) = raw.strip_prefix("id:") {
            let (id, revision) = split_revision(rest);
            if id.is_empty() {
                return Err(DriveError::InvalidPath {
                    path: raw.to_string(),
                    reason: "id: selector carries no file id",
                });
            }
            return Ok(DrivePath::ById {
                id: id.to_string(),
                revision,
            });
        }

        let trimmed = raw.trim_end_matches('/');
        if trimmed == MOUNT || raw == MOUNT {
            return Ok(DrivePath::Root);
        }
        let Some(after) = trimmed.strip_prefix(&format!("{MOUNT}/")) else {
            return Err(DriveError::InvalidPath {
                path: raw.to_string(),
                reason: "path is not under the /drive mount",
            });
        };

        let segments: Vec<&str> = after.split('/').filter(|s| !s.is_empty()).collect();
        match segments.as_slice() {
            [] => Ok(DrivePath::Root),
            [corpus] if *corpus == MY_SEGMENT => Ok(DrivePath::MyRoot),
            [corpus] if *corpus == SHARED_SEGMENT => Ok(DrivePath::SharedRoot),
            [corpus, rest @ ..] if *corpus == MY_SEGMENT => {
                let (segs, revision) = parse_segments(rest);
                Ok(DrivePath::My {
                    segments: segs,
                    revision,
                })
            }
            [corpus, drive, rest @ ..] if *corpus == SHARED_SEGMENT => {
                let (segs, revision) = parse_segments(rest);
                Ok(DrivePath::Shared {
                    drive: (*drive).to_string(),
                    segments: segs,
                    revision,
                })
            }
            // A bare top-level segment that is neither `my` nor `shared` is not a valid corpus.
            [other] => Err(DriveError::InvalidPath {
                path: other.to_string(),
                reason: "the /drive root has only the `my` and `shared` corpora",
            }),
            _ => Err(DriveError::InvalidPath {
                path: raw.to_string(),
                reason: "a Shared Drive path must name a drive after /drive/shared",
            }),
        }
    }

    /// The pinned revision id this address carried (`@<rev>`), if any.
    #[must_use]
    pub fn revision(&self) -> Option<&str> {
        match self {
            DrivePath::My { revision, .. }
            | DrivePath::Shared { revision, .. }
            | DrivePath::ById { revision, .. } => revision.as_deref(),
            DrivePath::Root | DrivePath::MyRoot | DrivePath::SharedRoot => None,
        }
    }

    /// Whether this node is a *collection* (a root, a corpus root, a Shared Drive root, or a
    /// folder path) — collections list children; a file leaf reads bytes. The pure parse cannot
    /// know if a path segment names a folder vs a file (that needs a live lookup), so this
    /// reports collection-ness only for the structurally-certain roots.
    #[must_use]
    pub const fn is_corpus_root(&self) -> bool {
        matches!(
            self,
            DrivePath::Root | DrivePath::MyRoot | DrivePath::SharedRoot
        )
    }

    /// The terminal segment name of a path address (the file/folder name), if the path has
    /// any segments. `None` for the roots and an `id:` address.
    #[must_use]
    pub fn leaf_name(&self) -> Option<&str> {
        match self {
            DrivePath::My { segments, .. } | DrivePath::Shared { segments, .. } => {
                segments.last().map(String::as_str)
            }
            _ => None,
        }
    }
}

/// Split a `name@rev` tail into `(name, Some(rev))`, or `(name, None)` when there is no `@`.
/// The `@` is the t21/RFD §4 revision-pin sigil; a name legitimately containing `@` (rare for
/// Drive) would be pinned on its last `@` — acceptable for the addressing fiction.
fn split_revision(s: &str) -> (&str, Option<String>) {
    match s.rsplit_once('@') {
        Some((name, rev)) if !rev.is_empty() => (name, Some(rev.to_string())),
        _ => (s, None),
    }
}

/// Turn the remaining raw segments into owned names, peeling an `@<rev>` off the LAST segment
/// (a revision pins the file the path resolves to). Returns `(segments, revision)`.
fn parse_segments(rest: &[&str]) -> (Vec<String>, Option<String>) {
    if rest.is_empty() {
        return (Vec::new(), None);
    }
    let mut segs: Vec<String> = rest.iter().map(|s| (*s).to_string()).collect();
    let last_idx = segs.len() - 1;
    let (name, revision) = split_revision(&segs[last_idx]);
    let name = name.to_string();
    segs[last_idx] = name;
    (segs, revision)
}
