//! [`ObjNode`] — the parse of a qfs [`Path`](qfs_driver::Path) into the object-storage node it
//! names (RFD-0001 §4/§5: "the path is the type"). Two mounts, one parser:
//!
//! - **S3** `/s3/<bucket>/<key>` (AWS S3) and **R2** `/r2/<bucket>/<key>` (Cloudflare R2).
//! - A **bucket root** `/s3/<bucket>` lists / accepts an `UPSERT` whose key rides in the row.
//! - A **key node** `/s3/<bucket>/<key>` addresses one object; `<key>` may contain slashes.
//! - The optional `@<versionId>` temporal coordinate (RFD §4) addresses a specific object version
//!   for GET/REMOVE: `/s3/<bucket>/<key>@<versionId>`.
//!
//! Pure parsing only — no I/O, no vendor type crosses.

use qfs_driver::Path;

use crate::error::ObjError;

/// The AWS S3 mount.
pub const S3_MOUNT: &str = "/s3";
/// The Cloudflare R2 mount.
pub const R2_MOUNT: &str = "/r2";

/// Which S3-compatible scheme a path addresses — the only difference at the mount edge (the HTTP
/// core is shared). Drives the driver id and the default region.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scheme {
    /// AWS S3 (`/s3`, driver id `s3`, region from config, e.g. `us-east-1`).
    S3,
    /// Cloudflare R2 (`/r2`, driver id `r2`, region `auto`).
    R2,
}

impl Scheme {
    /// The mount string for this scheme.
    #[must_use]
    pub const fn mount(self) -> &'static str {
        match self {
            Scheme::S3 => S3_MOUNT,
            Scheme::R2 => R2_MOUNT,
        }
    }

    /// The plan/driver id for this scheme (`s3` / `r2`).
    #[must_use]
    pub const fn driver_id(self) -> &'static str {
        match self {
            Scheme::S3 => "s3",
            Scheme::R2 => "r2",
        }
    }
}

/// A parsed object-storage address — what an `/s3/...` or `/r2/...` path resolves to (RFD §4).
/// Owned, vendor-free. The introspective methods and the applier branch on this.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ObjNode {
    /// `/s3` or `/r2` — the virtual root (lists buckets). Not itself queryable.
    Root {
        /// Which scheme.
        scheme: Scheme,
    },
    /// `/s3/<bucket>` — a bucket root (the `ls` / key-prefix listing node; accepts `UPSERT` whose
    /// key rides in the row).
    Bucket {
        /// Which scheme.
        scheme: Scheme,
        /// The bucket name.
        bucket: String,
    },
    /// `/s3/<bucket>/<key>[@<versionId>]` — a single object (the blob node).
    Object {
        /// Which scheme.
        scheme: Scheme,
        /// The bucket name.
        bucket: String,
        /// The object key (may contain slashes).
        key: String,
        /// The optional `@versionId` temporal coordinate (RFD §4).
        version_id: Option<String>,
    },
}

impl ObjNode {
    /// Parse a driver [`Path`] into an [`ObjNode`].
    ///
    /// # Errors
    /// [`ObjError::InvalidPath`] if the path is not under `/s3` or `/r2`.
    pub fn parse(path: &Path) -> Result<Self, ObjError> {
        Self::parse_str(path.as_str())
    }

    /// Parse a raw path string into an [`ObjNode`] (the core parse).
    ///
    /// # Errors
    /// [`ObjError::InvalidPath`] on a malformed address.
    pub fn parse_str(raw: &str) -> Result<Self, ObjError> {
        let scheme = if raw == S3_MOUNT || raw.starts_with(&format!("{S3_MOUNT}/")) {
            Scheme::S3
        } else if raw == R2_MOUNT || raw.starts_with(&format!("{R2_MOUNT}/")) {
            Scheme::R2
        } else {
            return Err(ObjError::InvalidPath {
                path: raw.to_string(),
                reason: "path is not under the /s3 or /r2 mount",
            });
        };

        let trimmed = raw.trim_end_matches('/');
        if trimmed == scheme.mount() {
            return Ok(ObjNode::Root { scheme });
        }
        // Strip "/s3/" or "/r2/".
        let after = trimmed
            .strip_prefix(&format!("{}/", scheme.mount()))
            .unwrap_or("");
        if after.is_empty() {
            return Ok(ObjNode::Root { scheme });
        }

        // The first segment is the bucket; the remainder (re-joined) is the key.
        match after.split_once('/') {
            None => Ok(ObjNode::Bucket {
                scheme,
                bucket: after.to_string(),
            }),
            Some((bucket, key_with_version)) if !key_with_version.is_empty() => {
                let (key, version_id) = split_version(key_with_version);
                Ok(ObjNode::Object {
                    scheme,
                    bucket: bucket.to_string(),
                    key: key.to_string(),
                    version_id,
                })
            }
            Some((bucket, _)) => Ok(ObjNode::Bucket {
                scheme,
                bucket: bucket.to_string(),
            }),
        }
    }

    /// The scheme this address belongs to.
    #[must_use]
    pub const fn scheme(&self) -> Scheme {
        match self {
            ObjNode::Root { scheme }
            | ObjNode::Bucket { scheme, .. }
            | ObjNode::Object { scheme, .. } => *scheme,
        }
    }

    /// The bucket name this address keys credential/backend resolution on, if any.
    #[must_use]
    pub fn bucket(&self) -> Option<&str> {
        match self {
            ObjNode::Bucket { bucket, .. } | ObjNode::Object { bucket, .. } => {
                Some(bucket.as_str())
            }
            ObjNode::Root { .. } => None,
        }
    }
}

/// Split a `key@versionId` tail into `(key, Some(versionId))`, or `(key, None)` when there is no
/// `@`. The `@` is the temporal-coordinate delimiter (RFD §4); only the **last** `@` separates a
/// version (a key may legitimately contain an earlier `@`), and an empty version after `@` is
/// treated as no version.
fn split_version(key_with_version: &str) -> (&str, Option<String>) {
    match key_with_version.rsplit_once('@') {
        Some((key, version)) if !version.is_empty() && !key.is_empty() => {
            (key, Some(version.to_string()))
        }
        _ => (key_with_version, None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_s3_object_bucket_and_root() {
        assert_eq!(
            ObjNode::parse_str("/s3/my-bucket/path/to/key.json").unwrap(),
            ObjNode::Object {
                scheme: Scheme::S3,
                bucket: "my-bucket".to_string(),
                key: "path/to/key.json".to_string(),
                version_id: None,
            }
        );
        assert_eq!(
            ObjNode::parse_str("/s3/my-bucket").unwrap(),
            ObjNode::Bucket {
                scheme: Scheme::S3,
                bucket: "my-bucket".to_string(),
            }
        );
        assert_eq!(
            ObjNode::parse_str("/s3").unwrap(),
            ObjNode::Root { scheme: Scheme::S3 }
        );
    }

    #[test]
    fn parses_r2_addresses() {
        assert_eq!(
            ObjNode::parse_str("/r2/bucket/k").unwrap(),
            ObjNode::Object {
                scheme: Scheme::R2,
                bucket: "bucket".to_string(),
                key: "k".to_string(),
                version_id: None,
            }
        );
        assert_eq!(ObjNode::parse_str("/r2").unwrap().scheme(), Scheme::R2);
    }

    #[test]
    fn parses_versioned_object() {
        let node = ObjNode::parse_str("/s3/b/k.txt@abc123version").unwrap();
        assert_eq!(
            node,
            ObjNode::Object {
                scheme: Scheme::S3,
                bucket: "b".to_string(),
                key: "k.txt".to_string(),
                version_id: Some("abc123version".to_string()),
            }
        );
    }

    #[test]
    fn a_key_containing_an_earlier_at_keeps_only_the_last_as_version() {
        let node = ObjNode::parse_str("/s3/b/user@example.com/file@v9").unwrap();
        assert_eq!(
            node,
            ObjNode::Object {
                scheme: Scheme::S3,
                bucket: "b".to_string(),
                key: "user@example.com/file".to_string(),
                version_id: Some("v9".to_string()),
            }
        );
    }

    #[test]
    fn rejects_paths_outside_the_mounts() {
        assert_eq!(
            ObjNode::parse_str("/cf/d1/x").unwrap_err().code(),
            "invalid_path"
        );
        assert_eq!(
            ObjNode::parse_str("/mail/inbox").unwrap_err().code(),
            "invalid_path"
        );
    }
}
