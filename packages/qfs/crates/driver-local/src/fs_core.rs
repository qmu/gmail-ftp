//! The synchronous filesystem core: sandbox path resolution, directory/glob scan,
//! streaming reads/writes, and the [`LocalEffect`] apply leg. All `std::fs` types stay
//! **internal** to this module (RFD §9) — only [`LocalRow`], owned paths, byte counts, and
//! [`LocalError`] cross the boundary.
//!
//! ## Sandbox (RFD §10 least privilege)
//! Every VFS path crosses [`Sandbox::resolve`] first: it strips the `/local` mount prefix,
//! joins onto `root`, lexically normalises `.`/`..` **without** following the path past
//! `root`, and — for existing paths — canonicalises and re-checks containment so a symlink
//! cannot point outside the mount. A path that escapes yields [`LocalError::OutsideSandbox`]
//! and performs **no** I/O.
//!
//! ## Streaming (RFD §6, the hard part)
//! Reads and the copy/move verify hash run through a fixed [`COPY_BUF`] buffer — never
//! `read_to_end` on the hot copy path — so a multi-GB blob moves in bounded memory. Writes
//! go to a sibling temp file and atomically `rename` on finish (retry-safe, no torn writes).

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Component, Path as StdPath, PathBuf};

use crate::error::LocalError;
use crate::row::LocalRow;

/// The fixed streaming buffer size (64 KiB) for bounded-memory reads and copy/verify.
const COPY_BUF: usize = 64 * 1024;

/// FNV-1a 64-bit offset basis.
const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
/// FNV-1a 64-bit prime.
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// A streaming, non-cryptographic content hash (FNV-1a 64-bit). This is a torn-copy /
/// divergence detector — NOT a security checksum: it catches a destination that silently
/// diverged from the source (a mid-stream corruption that preserves length, a partial flush)
/// so `cp`/`mv` can refuse to publish and `mv` never unlinks a source whose copy is wrong.
/// Folded incrementally over the same [`COPY_BUF`] chunks the copy already streams, so it is
/// near-free (no second pass over the source) and bounded-memory.
#[derive(Debug, Clone, Copy)]
struct Fnv1a(u64);

impl Fnv1a {
    /// A fresh hasher seeded with the FNV offset basis.
    const fn new() -> Self {
        Self(FNV_OFFSET)
    }

    /// Fold one chunk of bytes into the running hash.
    fn update(&mut self, bytes: &[u8]) {
        let mut h = self.0;
        for &b in bytes {
            h ^= u64::from(b);
            h = h.wrapping_mul(FNV_PRIME);
        }
        self.0 = h;
    }

    /// The finished digest.
    const fn finish(self) -> u64 {
        self.0
    }
}

/// The mount prefix every local VFS path carries.
pub(crate) const MOUNT: &str = "/local";

/// A least-privilege sandbox: all I/O is confined to `root`. `root` is canonicalised at
/// construction so containment checks compare canonical prefixes.
#[derive(Debug, Clone)]
pub struct Sandbox {
    root: PathBuf,
}

impl Sandbox {
    /// Build a sandbox confined to `root` (canonicalised). The caller is responsible for
    /// `root` existing; a non-existent root makes every resolve fail closed.
    #[must_use]
    pub fn new(root: PathBuf) -> Self {
        let root = fs::canonicalize(&root).unwrap_or(root);
        Self { root }
    }

    /// The sandbox root (canonical).
    #[must_use]
    pub fn root(&self) -> &StdPath {
        &self.root
    }

    /// Map a VFS path (`/local/sub/a.md`) to the relative segment under the mount
    /// (`sub/a.md`). A path that is exactly the mount maps to the empty relative path
    /// (the root directory). A path outside the mount is rejected.
    fn vfs_to_rel(vfs: &str) -> Result<PathBuf, LocalError> {
        let rel = if vfs == MOUNT {
            ""
        } else if let Some(stripped) = vfs.strip_prefix(&format!("{MOUNT}/")) {
            stripped
        } else {
            return Err(LocalError::OutsideSandbox(vfs.to_string()));
        };
        Ok(PathBuf::from(rel))
    }

    /// Lexically normalise a relative path, rejecting any component that would climb above
    /// the root (`..` at depth 0) or inject an absolute jump. This guards traversal
    /// **before** touching the filesystem (so `..` escapes never perform I/O).
    fn normalise(rel: &StdPath, original: &str) -> Result<PathBuf, LocalError> {
        let mut out = PathBuf::new();
        let mut depth = 0i32;
        for comp in rel.components() {
            match comp {
                Component::Normal(seg) => {
                    out.push(seg);
                    depth += 1;
                }
                Component::CurDir => {}
                Component::ParentDir => {
                    depth -= 1;
                    if depth < 0 {
                        return Err(LocalError::OutsideSandbox(original.to_string()));
                    }
                    out.pop();
                }
                // An absolute root or Windows prefix in a *relative* segment is an escape.
                Component::RootDir | Component::Prefix(_) => {
                    return Err(LocalError::OutsideSandbox(original.to_string()));
                }
            }
        }
        Ok(out)
    }

    /// Resolve a VFS path to an absolute filesystem path inside `root`, rejecting any escape
    /// (`..`, absolute jump, or — for an existing path — a symlink canonicalising outside).
    ///
    /// # Errors
    /// [`LocalError::OutsideSandbox`] if the path escapes; **no** I/O is performed in that
    /// case beyond the containment canonicalisation of an existing prefix.
    pub fn resolve(&self, vfs: &str) -> Result<PathBuf, LocalError> {
        let rel = Self::vfs_to_rel(vfs)?;
        let norm = Self::normalise(&rel, vfs)?;
        let joined = self.root.join(&norm);

        // Re-check containment against symlinks: canonicalise the longest existing ancestor
        // and assert it stays under root. A path that does not exist yet (a write target)
        // canonicalises its parent instead.
        let probe = if joined.exists() {
            &joined
        } else {
            joined.parent().unwrap_or(&self.root)
        };
        if let Ok(canon) = fs::canonicalize(probe) {
            if !canon.starts_with(&self.root) {
                return Err(LocalError::OutsideSandbox(vfs.to_string()));
            }
        }
        Ok(joined)
    }

    /// Convert an absolute filesystem path back into its VFS form (`root/sub/a.md` →
    /// `/local/sub/a.md`). Used to label scan rows. Falls back to the mount root if `abs`
    /// is not under `root` (should not happen for resolved paths).
    fn abs_to_vfs(&self, abs: &StdPath) -> String {
        match abs.strip_prefix(&self.root) {
            Ok(rel) if rel.as_os_str().is_empty() => MOUNT.to_string(),
            Ok(rel) => format!("{MOUNT}/{}", rel.to_string_lossy().replace('\\', "/")),
            Err(_) => MOUNT.to_string(),
        }
    }
}

/// Build a [`LocalRow`] from a resolved absolute path + its `lstat` metadata. Internal:
/// the `std::fs::Metadata` never escapes this function.
fn row_for(sandbox: &Sandbox, abs: &StdPath, meta: &fs::Metadata) -> LocalRow {
    let name = abs
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let modified = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .and_then(|d| i64::try_from(d.as_millis()).ok())
        .unwrap_or(0);
    LocalRow {
        name,
        path: sandbox.abs_to_vfs(abs),
        size: if meta.is_dir() { 0 } else { meta.len() },
        modified,
        is_dir: meta.is_dir(),
        mode: file_mode(meta),
    }
}

/// The Unix permission bits, or 0 on platforms without them.
fn file_mode(meta: &fs::Metadata) -> u32 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode()
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        0
    }
}

/// Whether a name is a dotfile (leading `.`). By documented default, `*`/`**` do **not**
/// match dotfiles — mirroring shell glob semantics. An explicit dotfile literal still
/// resolves (it is matched directly, not by a wildcard segment).
fn is_dotfile(name: &str) -> bool {
    name.starts_with('.')
}

/// List a single directory (one level), returning owned [`LocalRow`]s sorted by name for
/// deterministic golden snapshots. Dotfiles are excluded (shell-like default).
///
/// # Errors
/// [`LocalError::OutsideSandbox`] / [`LocalError::NotFound`] / [`LocalError::Io`].
pub fn scan_dir(sandbox: &Sandbox, vfs_dir: &str) -> Result<Vec<LocalRow>, LocalError> {
    let abs = sandbox.resolve(vfs_dir)?;
    let entries = fs::read_dir(&abs).map_err(|e| LocalError::from_io(vfs_dir, &e))?;
    let mut rows = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| LocalError::from_io(vfs_dir, &e))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if is_dotfile(&name) {
            continue;
        }
        // lstat (symlink_metadata) — do not follow links when describing the entry itself.
        let meta = entry
            .path()
            .symlink_metadata()
            .map_err(|e| LocalError::from_io(vfs_dir, &e))?;
        rows.push(row_for(sandbox, &entry.path(), &meta));
    }
    rows.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(rows)
}

/// Match a single path segment against a glob token supporting `*`, `?`, and literals.
/// Greedy backtracking matcher; no `**` here (`**` is handled at the directory-walk level).
fn segment_matches(pattern: &str, name: &str) -> bool {
    // Dotfiles never match a wildcard-bearing segment (shell default); a literal does.
    if is_dotfile(name) && pattern.contains(['*', '?']) {
        return false;
    }
    let p: Vec<char> = pattern.chars().collect();
    let n: Vec<char> = name.chars().collect();
    glob_seg(&p, &n)
}

/// Recursive `*`/`?` segment matcher over char slices.
fn glob_seg(p: &[char], n: &[char]) -> bool {
    match p.first() {
        None => n.is_empty(),
        Some('*') => glob_seg(&p[1..], n) || (!n.is_empty() && glob_seg(p, &n[1..])),
        Some('?') => !n.is_empty() && glob_seg(&p[1..], &n[1..]),
        Some(&c) => !n.is_empty() && n[0] == c && glob_seg(&p[1..], &n[1..]),
    }
}

/// Resolve a glob pattern (a VFS path possibly containing `*`, `?`, `**`) to the matching
/// set of files, scoped to `root` and sorted by path (deterministic). `**` matches across
/// directory levels (recursive descent). Symlinked directories are **not** descended, which
/// also bounds symlink cycles. Returns files only (not the directories walked through).
///
/// # Errors
/// [`LocalError::OutsideSandbox`] if the literal prefix escapes; [`LocalError::Io`] on a
/// read failure.
pub fn resolve_glob(sandbox: &Sandbox, pattern: &str) -> Result<Vec<LocalRow>, LocalError> {
    // A pattern with no wildcard is a single path: resolve + lstat it directly.
    if !pattern.contains(['*', '?']) {
        let abs = sandbox.resolve(pattern)?;
        return match abs.symlink_metadata() {
            Ok(meta) => Ok(vec![row_for(sandbox, &abs, &meta)]),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(LocalError::from_io(pattern, &e)),
        };
    }

    let rel = pattern
        .strip_prefix(&format!("{MOUNT}/"))
        .ok_or_else(|| LocalError::OutsideSandbox(pattern.to_string()))?;
    let segments: Vec<&str> = rel.split('/').filter(|s| !s.is_empty()).collect();

    // Validate the literal (wildcard-free) prefix stays in the sandbox before walking.
    sandbox.resolve(MOUNT)?;

    let mut matches = Vec::new();
    walk_glob(
        sandbox,
        sandbox.root().to_path_buf(),
        &segments,
        &mut matches,
    )?;
    matches.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(matches)
}

/// Recursively walk `dir` matching the remaining glob `segments`. `**` consumes zero or
/// more directory levels. Only regular files are collected into `out`.
fn walk_glob(
    sandbox: &Sandbox,
    dir: PathBuf,
    segments: &[&str],
    out: &mut Vec<LocalRow>,
) -> Result<(), LocalError> {
    let Some((head, tail)) = segments.split_first() else {
        return Ok(());
    };

    if *head == "**" {
        // `**` matches the current directory (tail applied here) and every subdirectory.
        walk_glob(sandbox, dir.clone(), tail, out)?;
        for child in read_children(sandbox, &dir)? {
            if child.is_dir {
                let child_abs = sandbox
                    .root()
                    .join(child.path.trim_start_matches(&format!("{MOUNT}/")));
                walk_glob(sandbox, child_abs, segments, out)?;
            }
        }
        return Ok(());
    }

    for child in read_children(sandbox, &dir)? {
        if !segment_matches(head, &child.name) {
            continue;
        }
        if tail.is_empty() {
            if !child.is_dir {
                out.push(child);
            }
        } else if child.is_dir {
            let child_abs = sandbox
                .root()
                .join(child.path.trim_start_matches(&format!("{MOUNT}/")));
            walk_glob(sandbox, child_abs, tail, out)?;
        }
    }
    Ok(())
}

/// Read the immediate children of `dir` as owned rows (dotfiles excluded, links not
/// followed). A directory that does not exist yields an empty set rather than an error, so
/// a glob over a partially-present tree is robust.
fn read_children(sandbox: &Sandbox, dir: &StdPath) -> Result<Vec<LocalRow>, LocalError> {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(LocalError::from_io(&sandbox.abs_to_vfs(dir), &e)),
    };
    let mut rows = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| LocalError::from_io(&sandbox.abs_to_vfs(dir), &e))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if is_dotfile(&name) {
            continue;
        }
        let meta = entry
            .path()
            .symlink_metadata()
            .map_err(|e| LocalError::from_io(&sandbox.abs_to_vfs(&entry.path()), &e))?;
        rows.push(row_for(sandbox, &entry.path(), &meta));
    }
    Ok(rows)
}

/// Stream-read a blob's bytes into an owned `Vec` through the fixed [`COPY_BUF`] buffer
/// (bounded per-iteration memory; the caller owns the final buffer). The decoded relation
/// is produced by a codec downstream — this function only moves bytes.
///
/// # Errors
/// [`LocalError::OutsideSandbox`] / [`LocalError::NotFound`] / [`LocalError::Io`].
pub fn read_blob(sandbox: &Sandbox, vfs: &str) -> Result<Vec<u8>, LocalError> {
    let abs = sandbox.resolve(vfs)?;
    let mut file = File::open(&abs).map_err(|e| LocalError::from_io(vfs, &e))?;
    let mut out = Vec::new();
    let mut buf = [0u8; COPY_BUF];
    loop {
        let n = file
            .read(&mut buf)
            .map_err(|e| LocalError::from_io(vfs, &e))?;
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n]);
    }
    Ok(out)
}

/// Atomically write `bytes` to `vfs`: stream into a sibling temp file, fsync, then `rename`
/// over the destination (retry-safe; an interrupted write leaves the original intact because
/// the temp file is discarded). Returns the byte count written.
///
/// # Errors
/// [`LocalError::OutsideSandbox`] / [`LocalError::Io`].
pub fn write_blob_atomic(sandbox: &Sandbox, vfs: &str, bytes: &[u8]) -> Result<u64, LocalError> {
    let abs = sandbox.resolve(vfs)?;
    let parent = abs
        .parent()
        .map(StdPath::to_path_buf)
        .unwrap_or_else(|| sandbox.root().to_path_buf());
    fs::create_dir_all(&parent).map_err(|e| LocalError::from_io(vfs, &e))?;

    // A unique sibling temp name keyed on the destination + a process/nanos suffix.
    let tmp = temp_sibling(&abs);
    {
        let mut f = File::create(&tmp).map_err(|e| LocalError::from_io(vfs, &e))?;
        f.write_all(bytes)
            .map_err(|e| LocalError::from_io(vfs, &e))?;
        f.sync_all().map_err(|e| LocalError::from_io(vfs, &e))?;
    }
    fs::rename(&tmp, &abs).map_err(|e| {
        // Best-effort cleanup of the temp file on a failed rename; ignore its result so the
        // original error is the one surfaced.
        let _ = fs::remove_file(&tmp);
        LocalError::from_io(vfs, &e)
    })?;
    Ok(bytes.len() as u64)
}

/// A sibling temp path next to `dst` (`<dst>.qfs-tmp.<nanos>`), so the atomic `rename` stays
/// on the same filesystem (cross-device rename would fail).
fn temp_sibling(dst: &StdPath) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut name = dst
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    name.push(format!(".qfs-tmp.{nanos}"));
    dst.with_file_name(name)
}

/// Stream-copy `src` → `dst` through the fixed buffer, computing both a verify byte-length
/// **and a streaming content hash** ([`Fnv1a`]) incrementally over the copied stream, then —
/// before publishing — re-read the written temp file and assert BOTH its size AND its content
/// hash match the source (the copy→verify shape, RFD §6). Size-only verification cannot catch
/// a torn/divergent copy that preserves length (a silent bit-flip, a partial-then-padded
/// write); the content hash does. The hash is folded over the same [`COPY_BUF`] chunks the
/// copy already streams, so it costs one extra pass over the *destination* and no extra pass
/// over the source.
///
/// Returns the byte count copied. Does **not** delete the source (that is `mv`'s extra step,
/// gated on this whole verification passing — `mv` never unlinks a source whose copy diverged).
///
/// # Errors
/// [`LocalError`] on resolve/IO failure, or [`LocalError::VerifyFailed`] if the destination
/// size **or** content hash does not match the source after the copy.
pub fn copy_verify(sandbox: &Sandbox, src_vfs: &str, dst_vfs: &str) -> Result<u64, LocalError> {
    let src = sandbox.resolve(src_vfs)?;
    let dst = sandbox.resolve(dst_vfs)?;
    let expected = src
        .metadata()
        .map_err(|e| LocalError::from_io(src_vfs, &e))?
        .len();

    let parent = dst
        .parent()
        .map(StdPath::to_path_buf)
        .unwrap_or_else(|| sandbox.root().to_path_buf());
    fs::create_dir_all(&parent).map_err(|e| LocalError::from_io(dst_vfs, &e))?;

    let tmp = temp_sibling(&dst);
    let mut reader = File::open(&src).map_err(|e| LocalError::from_io(src_vfs, &e))?;
    let mut written: u64 = 0;
    // The expected content hash is taken from the bytes as they were streamed FROM the source.
    let mut src_hash = Fnv1a::new();
    {
        let mut writer = File::create(&tmp).map_err(|e| LocalError::from_io(dst_vfs, &e))?;
        let mut buf = [0u8; COPY_BUF];
        loop {
            let n = reader
                .read(&mut buf)
                .map_err(|e| LocalError::from_io(src_vfs, &e))?;
            if n == 0 {
                break;
            }
            src_hash.update(&buf[..n]);
            writer
                .write_all(&buf[..n])
                .map_err(|e| LocalError::from_io(dst_vfs, &e))?;
            written += n as u64;
        }
        writer
            .sync_all()
            .map_err(|e| LocalError::from_io(dst_vfs, &e))?;
    }

    // Verify size AND content BEFORE publishing. On any mismatch the temp file is discarded
    // and nothing is published (so `mv` never unlinks a source whose copy is wrong).
    if let Err(e) = verify_copy(&tmp, dst_vfs, expected, written, src_hash.finish()) {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }

    fs::rename(&tmp, &dst).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        LocalError::from_io(dst_vfs, &e)
    })?;
    Ok(written)
}

/// Verify a freshly-written copy candidate at `tmp` against the source: assert its byte
/// length equals `expected` (vs. the `written` stream count) AND its streamed content hash
/// equals `expected_hash` (the hash of the bytes read from the source). Returns
/// [`LocalError::VerifyFailed`] on either mismatch. Pure decision over the on-disk candidate,
/// so the torn-copy rejection is directly testable.
///
/// # Errors
/// [`LocalError::VerifyFailed`] if size or content hash diverges; [`LocalError::Io`] if the
/// candidate cannot be re-read for hashing.
fn verify_copy(
    tmp: &StdPath,
    dst_vfs: &str,
    expected: u64,
    written: u64,
    expected_hash: u64,
) -> Result<(), LocalError> {
    if written != expected {
        return Err(LocalError::VerifyFailed {
            dst: dst_vfs.to_string(),
            expected,
            found: written,
        });
    }
    // Stream the just-written candidate back through the same buffer and hash it; a torn copy
    // that matched on length but diverged in content fails here.
    let dst_hash = hash_file(tmp, dst_vfs)?;
    if dst_hash != expected_hash {
        return Err(LocalError::VerifyFailed {
            dst: dst_vfs.to_string(),
            expected,
            found: written,
        });
    }
    Ok(())
}

/// Stream-hash a file's content through the fixed [`COPY_BUF`] buffer (bounded memory) into
/// an [`Fnv1a`] digest. Used by [`copy_verify`] to re-read the published-candidate temp file
/// and confirm it matches the source content, not merely its length.
///
/// # Errors
/// [`LocalError::Io`] (or `NotFound`) labelled with `vfs_label` on a read failure.
fn hash_file(path: &StdPath, vfs_label: &str) -> Result<u64, LocalError> {
    let mut file = File::open(path).map_err(|e| LocalError::from_io(vfs_label, &e))?;
    let mut hash = Fnv1a::new();
    let mut buf = [0u8; COPY_BUF];
    loop {
        let n = file
            .read(&mut buf)
            .map_err(|e| LocalError::from_io(vfs_label, &e))?;
        if n == 0 {
            break;
        }
        hash.update(&buf[..n]);
    }
    Ok(hash.finish())
}

/// Remove a blob (file). A directory or missing path is rejected/structured.
///
/// # Errors
/// [`LocalError::OutsideSandbox`] / [`LocalError::NotFound`] / [`LocalError::Io`].
pub fn remove_blob(sandbox: &Sandbox, vfs: &str) -> Result<(), LocalError> {
    let abs = sandbox.resolve(vfs)?;
    fs::remove_file(&abs).map_err(|e| LocalError::from_io(vfs, &e))
}

#[cfg(test)]
mod verify_tests {
    //! Unit coverage for the size+hash verify primitives. Lints opt out via the crate-level
    //! `#![cfg_attr(test, allow(...))]`.
    use super::{hash_file, verify_copy, Fnv1a};
    use crate::error::LocalError;
    use tempfile::TempDir;

    /// FNV-1a distinguishes two **same-length** but content-divergent byte strings — exactly
    /// the torn-copy case size-only verification cannot catch.
    #[test]
    fn content_hash_separates_same_length_divergence() {
        let mut a = Fnv1a::new();
        a.update(b"hello-qfs");
        let mut b = Fnv1a::new();
        b.update(b"hexxo-qfs"); // same length, two bytes differ
        assert_eq!(b"hello-qfs".len(), b"hexxo-qfs".len(), "lengths are equal");
        assert_ne!(
            a.finish(),
            b.finish(),
            "content hash must differ even when lengths match"
        );
    }

    /// `verify_copy` ACCEPTS when the candidate's size and content hash both match the source.
    #[test]
    fn verify_copy_accepts_matching_candidate() {
        let dir = TempDir::new().expect("tempdir");
        let tmp = dir.path().join("cand");
        std::fs::write(&tmp, b"payload-bytes").unwrap();
        let mut h = Fnv1a::new();
        h.update(b"payload-bytes");
        let r = verify_copy(&tmp, "/local/dst", 13, 13, h.finish());
        assert!(r.is_ok(), "matching size+hash candidate is accepted");
    }

    /// `verify_copy` REJECTS a candidate that matches on length but DIVERGES in content — the
    /// torn-copy guard. The expected hash is the source's; the on-disk candidate differs.
    #[test]
    fn verify_copy_rejects_length_equal_content_divergent() {
        let dir = TempDir::new().expect("tempdir");
        let tmp = dir.path().join("cand");
        // Candidate on disk is the corrupted ("torn") copy; the expected hash is the source's.
        std::fs::write(&tmp, b"hexxo-qfs").unwrap();
        let mut src = Fnv1a::new();
        src.update(b"hello-qfs"); // what the source actually was
        let len = b"hello-qfs".len() as u64;
        let err = verify_copy(&tmp, "/local/dst", len, len, src.finish()).unwrap_err();
        match err {
            LocalError::VerifyFailed { dst, .. } => assert_eq!(dst, "/local/dst"),
            other => panic!("expected VerifyFailed, got {other:?}"),
        }
    }

    /// `hash_file` streams a file's content into the same digest as an in-memory fold (the
    /// re-read leg of `copy_verify` matches the stream-side hash for identical content).
    #[test]
    fn hash_file_matches_in_memory_fold() {
        let dir = TempDir::new().expect("tempdir");
        let p = dir.path().join("blob");
        let payload: Vec<u8> = (0..200_000u32).map(|i| (i % 257) as u8).collect();
        std::fs::write(&p, &payload).unwrap();
        let mut mem = Fnv1a::new();
        mem.update(&payload);
        assert_eq!(hash_file(&p, "/local/blob").unwrap(), mem.finish());
    }
}
