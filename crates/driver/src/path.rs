//! [`Path`] — a cfs virtual path (RFD-0001 §2.1 VFS: "a path is just a query that
//! resolves to a set").
//!
//! E0 ships an owned, opaque wrapper around the path string. Structured parsing
//! (mount segment, `@version` temporal coordinate §4, attribute predicates) lands
//! in later epics; the type exists now so the `Driver` trait signatures are stable.
//! Owned data only — no borrowing of vendor types (owned-DTO discipline, §9).
//!
//! ## Adapter to [`cfs_plan::VfsPath`] (the pushdown/effect boundary)
//! The driver contract speaks [`Path`]; the effect substrate speaks
//! [`cfs_plan::VfsPath`]. They are deliberately distinct types living in different
//! crates (the spine is `cfs-driver → cfs-plan`, so `cfs-plan` cannot name `Path`).
//! This module owns the **explicit, lossless** adapter both directions
//! ([`Path::to_vfs`] / [`Path::from_vfs`] / [`Path::try_from_vfs`]) so a driver's
//! pushdown and effect surface can move a path across the boundary without ever
//! reaching for a vendor type. The round-trip is byte-for-byte lossless
//! (`from_vfs(p.to_vfs()) == p`); the validating constructor additionally rejects
//! the malformed paths a driver should never be handed.

use cfs_plan::VfsPath;

use crate::error::CfsError;

/// A cfs virtual path, e.g. `/mail/inbox`, `/s3/bucket/key@versionId`,
/// `/git/repo@ref/path`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Path {
    raw: String,
}

impl Path {
    /// Construct a path from an owned string.
    #[must_use]
    pub fn new(raw: impl Into<String>) -> Self {
        Self { raw: raw.into() }
    }

    /// The raw path text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.raw
    }

    /// Convert this driver path into a [`cfs_plan::VfsPath`] for the effect/pushdown
    /// surface. **Lossless**: it carries exactly the same bytes, so
    /// `Path::from_vfs(p.to_vfs()) == p` for every `p`.
    #[must_use]
    pub fn to_vfs(&self) -> VfsPath {
        VfsPath::new(self.raw.clone())
    }

    /// Build a driver [`Path`] from a [`cfs_plan::VfsPath`] **without validation**
    /// (the inverse of [`Path::to_vfs`]). Lossless: it preserves the bytes exactly.
    /// Use [`Path::try_from_vfs`] when the path originates outside the engine and must
    /// be validated.
    #[must_use]
    pub fn from_vfs(path: &VfsPath) -> Self {
        Self::new(path.as_str().to_string())
    }

    /// Build a driver [`Path`] from a [`cfs_plan::VfsPath`], **parse-validating** it:
    /// the path must be non-empty and absolute (start with `/`). This is the gate a
    /// driver applies to a path entering its pushdown/effect surface from the plan.
    ///
    /// # Errors
    /// [`CfsError::InvalidPath`] if the path is empty or not absolute, carrying the
    /// offending text and a machine-readable reason (for AI consumption, RFD §5).
    pub fn try_from_vfs(path: &VfsPath) -> Result<Self, CfsError> {
        Self::validate(path.as_str())?;
        Ok(Self::from_vfs(path))
    }

    /// Construct a driver [`Path`] from text, **parse-validating** it (non-empty,
    /// absolute). The validating sibling of [`Path::new`].
    ///
    /// # Errors
    /// [`CfsError::InvalidPath`] if the text is empty or not absolute.
    pub fn parse(raw: impl Into<String>) -> Result<Self, CfsError> {
        let raw = raw.into();
        Self::validate(&raw)?;
        Ok(Self { raw })
    }

    /// The shared validation rule: a cfs virtual path is absolute and non-empty.
    fn validate(raw: &str) -> Result<(), CfsError> {
        if raw.is_empty() {
            return Err(CfsError::InvalidPath {
                path: raw.to_string(),
                reason: "path is empty",
            });
        }
        if !raw.starts_with('/') {
            return Err(CfsError::InvalidPath {
                path: raw.to_string(),
                reason: "path is not absolute (must start with '/')",
            });
        }
        Ok(())
    }
}

impl From<&str> for Path {
    fn from(s: &str) -> Self {
        Self::new(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn to_vfs_then_from_vfs_round_trips_losslessly() {
        for raw in [
            "/mail/inbox",
            "/s3/bucket/key@versionId",
            "/git/repo@ref/some/path",
        ] {
            let p = Path::new(raw);
            let v = p.to_vfs();
            assert_eq!(v.as_str(), raw, "to_vfs preserves the bytes");
            assert_eq!(Path::from_vfs(&v), p, "from_vfs is the exact inverse");
        }
    }

    #[test]
    fn try_from_vfs_accepts_absolute_paths() {
        let v = VfsPath::new("/mail/inbox");
        let p = Path::try_from_vfs(&v).unwrap();
        assert_eq!(p.as_str(), "/mail/inbox");
    }

    #[test]
    fn try_from_vfs_rejects_relative_and_empty() {
        let rel = VfsPath::new("mail/inbox");
        match Path::try_from_vfs(&rel) {
            Err(CfsError::InvalidPath { path, .. }) => assert_eq!(path, "mail/inbox"),
            other => panic!("expected InvalidPath, got {other:?}"),
        }

        let empty = VfsPath::new("");
        assert!(matches!(
            Path::try_from_vfs(&empty),
            Err(CfsError::InvalidPath { .. })
        ));
    }

    #[test]
    fn parse_validates_like_try_from_vfs() {
        assert!(Path::parse("/ok").is_ok());
        assert!(matches!(
            Path::parse("nope"),
            Err(CfsError::InvalidPath { .. })
        ));
    }
}
