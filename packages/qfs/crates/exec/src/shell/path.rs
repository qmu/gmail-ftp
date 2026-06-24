//! Pure VFS path resolution for the interactive shell (ticket t28, hard part (a)).
//!
//! The shell carries a *current working location* tagged `{driver, path}` and resolves a
//! user-typed `raw` path token against it into an **absolute** `/driver/seg/seg` VFS path —
//! exactly the form the one-shot path requires. This is the t29 carry-over relaxation: when a
//! cwd exists, a relative path is no longer rejected — it resolves against the cwd.
//!
//! ## The cases (all unit-tested below)
//! - **absolute, cross-driver** — `/other/x` keeps its own driver, ignoring the cwd's driver;
//! - **relative** — `a/b` joins onto the cwd path under the cwd's driver;
//! - **`..`** — pops one segment off the cwd path (never above the mount root);
//! - **`.`** — the cwd itself;
//! - **`~` / leading-slash-only `/`** — the cwd driver's mount root.
//!
//! Resolution is **pure** (no I/O, no registry): it is the lexical half of addressing. Whether
//! the resolved node actually exists / is a namespace is a separate driver capability check the
//! shell performs for `cd` (see [`crate::shell::session`]).

use crate::error::ExecError;

/// A fully-resolved, absolute VFS path: a driver id plus the segments under its mount. Renders
/// back to the canonical `/driver/seg/seg` form the parser/one-shot path consumes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VfsPath {
    /// The driver/mount id (the first path segment, without the leading `/`).
    driver: String,
    /// The segments under the mount (never contains `.`/`..`; already normalised).
    segments: Vec<String>,
}

impl VfsPath {
    /// Build a path from a driver id and already-normalised segments.
    #[must_use]
    pub fn new(driver: impl Into<String>, segments: Vec<String>) -> Self {
        Self {
            driver: driver.into(),
            segments,
        }
    }

    /// The driver/mount root for `driver` (no segments) — the cwd a fresh session starts at.
    #[must_use]
    pub fn root(driver: impl Into<String>) -> Self {
        Self {
            driver: driver.into(),
            segments: Vec::new(),
        }
    }

    /// The driver/mount id (without the leading `/`).
    #[must_use]
    pub fn driver(&self) -> &str {
        &self.driver
    }

    /// The segments under the mount.
    #[must_use]
    pub fn segments(&self) -> &[String] {
        &self.segments
    }

    /// Whether this path is the mount root (no segments under the driver).
    #[must_use]
    pub fn is_root(&self) -> bool {
        self.segments.is_empty()
    }

    /// Render the canonical absolute VFS form: `/driver` at the root, else `/driver/a/b`.
    #[must_use]
    pub fn render(&self) -> String {
        if self.segments.is_empty() {
            format!("/{}", self.driver)
        } else {
            format!("/{}/{}", self.driver, self.segments.join("/"))
        }
    }
}

impl std::fmt::Display for VfsPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.render())
    }
}

/// Resolve a user-typed `raw` path token against `cwd` into an absolute [`VfsPath`]. Pure and
/// total over the lexical grammar; the only error is an empty/over-popped path.
///
/// Resolution rules (ticket t28, hard part (a)):
/// - `~` or a bare `/` → the cwd driver's mount root;
/// - an absolute `/driver/...` (a leading `/` followed by a segment) → that driver + segments,
///   crossing drivers freely (it ignores the cwd's driver);
/// - anything else is **relative**: split on `/`, fold onto the cwd's segments, honouring `.`
///   (no-op) and `..` (pop one), under the cwd's driver. A `..` that would climb above the
///   mount root is clamped at the root (mirrors the local sandbox, never escapes the driver).
///
/// # Errors
/// [`ExecError`] (kind `usage`) if `raw` is empty.
pub fn resolve(raw: &str, cwd: &VfsPath) -> Result<VfsPath, ExecError> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err(ExecError::usage("empty path"));
    }

    // `~` (or `~/...`) and a bare `/` both anchor at the cwd driver's mount root.
    if raw == "~" || raw == "/" {
        return Ok(VfsPath::root(cwd.driver.clone()));
    }
    if let Some(rest) = raw.strip_prefix("~/") {
        return Ok(fold_segments(&cwd.driver, &[], rest));
    }

    // An absolute path: `/driver/seg/seg`. The first segment is the (possibly different) driver.
    if let Some(rest) = raw.strip_prefix('/') {
        let mut parts = rest.splitn(2, '/');
        let driver = parts.next().unwrap_or("").to_string();
        if driver.is_empty() {
            return Ok(VfsPath::root(cwd.driver.clone()));
        }
        let tail = parts.next().unwrap_or("");
        // An absolute path starts fresh at the named driver's root, then folds its tail.
        return Ok(fold_segments(&driver, &[], tail));
    }

    // Relative: fold onto the cwd segments, under the cwd driver.
    Ok(fold_segments(&cwd.driver, &cwd.segments, raw))
}

/// Fold a `/`-separated `rest` onto `base` segments under `driver`, honouring `.`/`..`. A `..`
/// at the root is clamped (never crosses the mount boundary). Empty segments (`a//b`, trailing
/// `/`) are skipped.
fn fold_segments(driver: &str, base: &[String], rest: &str) -> VfsPath {
    let mut segs: Vec<String> = base.to_vec();
    for part in rest.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                segs.pop();
            }
            seg => segs.push(seg.to_string()),
        }
    }
    VfsPath::new(driver.to_string(), segs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cwd() -> VfsPath {
        VfsPath::new("local", vec!["docs".into(), "api".into()])
    }

    #[test]
    fn relative_joins_onto_cwd() {
        let r = resolve("notes/readme.md", &cwd()).unwrap();
        assert_eq!(r.render(), "/local/docs/api/notes/readme.md");
        assert_eq!(r.driver(), "local");
    }

    #[test]
    fn dot_is_the_cwd() {
        let r = resolve(".", &cwd()).unwrap();
        assert_eq!(r.render(), "/local/docs/api");
    }

    #[test]
    fn parent_pops_one_segment() {
        let r = resolve("..", &cwd()).unwrap();
        assert_eq!(r.render(), "/local/docs");
    }

    #[test]
    fn parent_then_descend() {
        let r = resolve("../guides/intro.md", &cwd()).unwrap();
        assert_eq!(r.render(), "/local/docs/guides/intro.md");
    }

    #[test]
    fn parent_clamps_at_root() {
        // More `..` than depth clamps at the mount root, never escaping the driver.
        let r = resolve("../../../../x", &cwd()).unwrap();
        assert_eq!(r.render(), "/local/x");
    }

    #[test]
    fn root_slash_is_mount_root() {
        let r = resolve("/", &cwd()).unwrap();
        assert_eq!(r.render(), "/local");
        assert!(r.is_root());
    }

    #[test]
    fn tilde_is_mount_root() {
        let r = resolve("~", &cwd()).unwrap();
        assert_eq!(r.render(), "/local");
    }

    #[test]
    fn tilde_slash_anchors_at_root() {
        let r = resolve("~/inbox", &cwd()).unwrap();
        assert_eq!(r.render(), "/local/inbox");
    }

    #[test]
    fn absolute_same_driver_ignores_cwd() {
        let r = resolve("/local/other/file.md", &cwd()).unwrap();
        assert_eq!(r.render(), "/local/other/file.md");
    }

    #[test]
    fn absolute_cross_driver() {
        // The hard case: an absolute path naming a DIFFERENT driver keeps that driver, not the
        // cwd's. This is what lets cross-mount `cp /local/a /mail/b` work without `cd`.
        let r = resolve("/mail/inbox/msg", &cwd()).unwrap();
        assert_eq!(r.driver(), "mail");
        assert_eq!(r.render(), "/mail/inbox/msg");
    }

    #[test]
    fn absolute_driver_root() {
        let r = resolve("/mail", &cwd()).unwrap();
        assert_eq!(r.driver(), "mail");
        assert!(r.is_root());
        assert_eq!(r.render(), "/mail");
    }

    #[test]
    fn empty_is_usage_error() {
        let e = resolve("   ", &cwd()).unwrap_err();
        assert_eq!(e.kind.as_str(), "usage");
    }

    #[test]
    fn collapses_redundant_slashes_and_dots() {
        let r = resolve("a//./b/", &cwd()).unwrap();
        assert_eq!(r.render(), "/local/docs/api/a/b");
    }
}
