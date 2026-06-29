//! The synchronous filesystem core for `/fs`: operator-configured **named roots**, path
//! resolution + escape guard, directory/glob scan, streaming reads/writes, copy→verify→[delete],
//! and remove. All `std::fs` types stay **internal** to this module (RFD §9) — only [`FsRow`],
//! owned paths, byte counts, and [`FsError`] cross the boundary.
//!
//! ## Roots, not a sandbox (the t68 difference from `/local`)
//! `/local` is a fixed single-root sandbox. `/fs` is addressed under an **allowlist of named
//! roots** an operator configures from the binary ([`FsRoots`]): a VFS path is
//! `/fs/<root>/<rel…>`, where `<root>` selects one configured base directory and `<rel…>`
//! resolves under it. The default is **deny-all**: an empty [`FsRoots`] resolves NOTHING (no
//! implicit whole-disk access — see [`FsError::UnknownRoot`]).
//!
//! ## Escape guard (RFD §10 least privilege) — validated at BOTH scan and apply time
//! Every VFS path crosses [`FsRoots::resolve`] first: it selects the root, strips the
//! `/fs/<root>` prefix, lexically normalises `.`/`..` **without** climbing above the root, and —
//! for existing paths — canonicalises and re-checks containment so a symlink cannot point
//! outside the root. A path that escapes yields [`FsError::OutsideRoot`] and performs **no** I/O.
//! Because `fs` widens blast radius beyond a sandbox, the apply leg re-validates through the same
//! `resolve` (defence in depth), never trusting a path validated only at scan time.
//!
//! ## Streaming (RFD §6, the hard part)
//! Reads and the copy/move verify hash run through a fixed [`COPY_BUF`] buffer — never
//! `read_to_end` on the hot copy path — so a multi-GB blob moves in bounded memory. Writes go to
//! a sibling temp file and atomically `rename` on finish (retry-safe, no torn writes).

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Component, Path as StdPath, PathBuf};

use crate::error::FsError;
use crate::row::FsRow;

/// The fixed streaming buffer size (64 KiB) for bounded-memory reads and copy/verify.
const COPY_BUF: usize = 64 * 1024;

/// FNV-1a 64-bit offset basis.
const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
/// FNV-1a 64-bit prime.
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// The mount prefix every `/fs` VFS path carries.
pub(crate) const MOUNT: &str = "/fs";

/// A streaming, non-cryptographic content hash (FNV-1a 64-bit). A torn-copy / divergence
/// detector — NOT a security checksum: it catches a destination that silently diverged from the
/// source (a mid-stream corruption that preserves length, a partial flush) so `cp`/`mv` can
/// refuse to publish and `mv` never unlinks a source whose copy is wrong. Folded incrementally
/// over the same [`COPY_BUF`] chunks the copy already streams, so it is near-free and
/// bounded-memory.
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

/// The operator-configured **root allowlist** (RFD §10): a map from a root NAME (the first path
/// segment after `/fs`) to a canonical base directory. All I/O is confined to one of these bases.
/// An empty `FsRoots` is **deny-all** — every resolve fails closed with [`FsError::UnknownRoot`],
/// so a host with no configured root never exposes the disk.
#[derive(Debug, Clone, Default)]
pub struct FsRoots {
    roots: BTreeMap<String, PathBuf>,
}

impl FsRoots {
    /// An empty allowlist — **deny-all** (the flagged default: no implicit whole-disk access).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a named root mapping `name` → `base` (canonicalised). The caller owns `base` existing;
    /// a non-existent base makes every resolve under that name fail closed. Chainable builder.
    #[must_use]
    pub fn with_root(mut self, name: impl Into<String>, base: impl Into<PathBuf>) -> Self {
        let base = base.into();
        let base = fs::canonicalize(&base).unwrap_or(base);
        self.roots.insert(name.into(), base);
        self
    }

    /// Whether any root is configured (false ⇒ deny-all).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.roots.is_empty()
    }

    /// The number of configured roots.
    #[must_use]
    pub fn len(&self) -> usize {
        self.roots.len()
    }

    /// The configured base for `name`, if any.
    #[must_use]
    pub fn base(&self, name: &str) -> Option<&StdPath> {
        self.roots.get(name).map(PathBuf::as_path)
    }

    /// The configured root NAMES, sorted (deterministic listing order for the `/fs` mount-root
    /// scan). The names are operator-chosen labels — never a host path.
    fn names(&self) -> Vec<String> {
        self.roots.keys().cloned().collect()
    }

    /// Split a VFS path (`/fs/<root>/<rel…>`) into `(root_name, rel)`. The bare mount `/fs`
    /// yields no `(root, rel)` (it is the virtual root-name listing) and is rejected here — the
    /// scan path handles it separately. A path outside the mount, or one that names no segment
    /// after `/fs`, is rejected.
    fn split(vfs: &str) -> Result<(&str, &str), FsError> {
        let rest = vfs
            .strip_prefix(&format!("{MOUNT}/"))
            .ok_or_else(|| FsError::OutsideRoot(vfs.to_string()))?;
        match rest.split_once('/') {
            Some((name, rel)) => Ok((name, rel)),
            None => Ok((rest, "")),
        }
    }

    /// Lexically normalise a relative path, rejecting any component that would climb above the
    /// root (`..` at depth 0) or inject an absolute jump. Guards traversal **before** touching
    /// the filesystem (so `..` escapes never perform I/O).
    fn normalise(rel: &str, original: &str) -> Result<PathBuf, FsError> {
        let mut out = PathBuf::new();
        let mut depth = 0i32;
        for comp in StdPath::new(rel).components() {
            match comp {
                Component::Normal(seg) => {
                    out.push(seg);
                    depth += 1;
                }
                Component::CurDir => {}
                Component::ParentDir => {
                    depth -= 1;
                    if depth < 0 {
                        return Err(FsError::OutsideRoot(original.to_string()));
                    }
                    out.pop();
                }
                // An absolute root or Windows prefix in a *relative* segment is an escape.
                Component::RootDir | Component::Prefix(_) => {
                    return Err(FsError::OutsideRoot(original.to_string()));
                }
            }
        }
        Ok(out)
    }

    /// Resolve a VFS path to an absolute filesystem path inside its configured root, rejecting any
    /// escape (`..`, absolute jump, an unknown/unconfigured root, or — for an existing path — a
    /// symlink canonicalising outside the root).
    ///
    /// # Errors
    /// [`FsError::UnknownRoot`] if `/fs/<root>` names no configured root (deny-all);
    /// [`FsError::OutsideRoot`] if the path escapes its root. **No** I/O is performed in either
    /// case beyond the containment canonicalisation of an existing prefix.
    pub fn resolve(&self, vfs: &str) -> Result<PathBuf, FsError> {
        let (name, rel) = Self::split(vfs)?;
        let base = self.roots.get(name).ok_or_else(|| FsError::UnknownRoot {
            path: vfs.to_string(),
            root: name.to_string(),
        })?;
        let norm = Self::normalise(rel, vfs)?;
        let joined = base.join(&norm);

        // Re-check containment against symlinks: canonicalise the longest existing ancestor and
        // assert it stays under the root. A path that does not exist yet (a write target)
        // canonicalises its parent instead.
        let probe = if joined.exists() {
            &joined
        } else {
            joined.parent().unwrap_or(base.as_path())
        };
        if let Ok(canon) = fs::canonicalize(probe) {
            if !canon.starts_with(base) {
                return Err(FsError::OutsideRoot(vfs.to_string()));
            }
        }
        Ok(joined)
    }

    /// Convert an absolute filesystem path back into its VFS form under root `name`
    /// (`base/sub/a.md` → `/fs/<name>/sub/a.md`). Used to label scan rows. Falls back to the
    /// `/fs/<name>` root if `abs` is not under `base` (should not happen for resolved paths).
    fn abs_to_vfs(&self, name: &str, base: &StdPath, abs: &StdPath) -> String {
        match abs.strip_prefix(base) {
            Ok(rel) if rel.as_os_str().is_empty() => format!("{MOUNT}/{name}"),
            Ok(rel) => format!(
                "{MOUNT}/{name}/{}",
                rel.to_string_lossy().replace('\\', "/")
            ),
            Err(_) => format!("{MOUNT}/{name}"),
        }
    }
}

/// Build an [`FsRow`] from a resolved absolute path + its `lstat` metadata, labelled under root
/// `name`. Internal: the `std::fs::Metadata` never escapes this function.
fn row_for(
    roots: &FsRoots,
    name: &str,
    base: &StdPath,
    abs: &StdPath,
    meta: &fs::Metadata,
) -> FsRow {
    let leaf = abs
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let modified = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .and_then(|d| i64::try_from(d.as_millis()).ok())
        .unwrap_or(0);
    FsRow {
        name: leaf,
        path: roots.abs_to_vfs(name, base, abs),
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

/// Whether a name is a dotfile (leading `.`). By documented default, `*`/`**` do **not** match
/// dotfiles — mirroring shell glob semantics. An explicit dotfile literal still resolves.
fn is_dotfile(name: &str) -> bool {
    name.starts_with('.')
}

/// One virtual `/fs` mount-root entry per configured root NAME — what listing the bare `/fs`
/// mount yields (each configured root presents as a directory). Sorted by path (deterministic).
fn root_listing(roots: &FsRoots) -> Vec<FsRow> {
    let mut rows: Vec<FsRow> = roots
        .names()
        .into_iter()
        .map(|name| FsRow {
            path: format!("{MOUNT}/{name}"),
            name,
            size: 0,
            modified: 0,
            is_dir: true,
            mode: 0,
        })
        .collect();
    rows.sort_by(|a, b| a.path.cmp(&b.path));
    rows
}

/// List a single directory (one level), returning owned [`FsRow`]s sorted by path for
/// deterministic golden snapshots. The bare mount `/fs` lists the configured root names.
/// Dotfiles are excluded (shell-like default).
///
/// # Errors
/// [`FsError::UnknownRoot`] / [`FsError::OutsideRoot`] / [`FsError::NotFound`] / [`FsError::Io`].
pub fn scan_dir(roots: &FsRoots, vfs: &str) -> Result<Vec<FsRow>, FsError> {
    if vfs == MOUNT {
        return Ok(root_listing(roots));
    }
    let (name, _) = FsRoots::split(vfs)?;
    let base = roots
        .roots
        .get(name)
        .ok_or_else(|| FsError::UnknownRoot {
            path: vfs.to_string(),
            root: name.to_string(),
        })?
        .clone();
    let abs = roots.resolve(vfs)?;
    let entries = fs::read_dir(&abs).map_err(|e| FsError::from_io(vfs, &e))?;
    let mut rows = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| FsError::from_io(vfs, &e))?;
        let leaf = entry.file_name().to_string_lossy().into_owned();
        if is_dotfile(&leaf) {
            continue;
        }
        // lstat (symlink_metadata) — do not follow links when describing the entry itself.
        let meta = entry
            .path()
            .symlink_metadata()
            .map_err(|e| FsError::from_io(vfs, &e))?;
        rows.push(row_for(roots, name, &base, &entry.path(), &meta));
    }
    rows.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(rows)
}

/// Match a single path segment against a glob token supporting `*`, `?`, and literals. Greedy
/// backtracking matcher; no `**` here (`**` is handled at the directory-walk level).
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

/// Resolve a glob pattern (a VFS path possibly containing `*`, `?`, `**`) to the matching set of
/// files, scoped to a SINGLE configured root and sorted by path (deterministic). The root-name
/// segment (`/fs/<root>`) must be a literal — a wildcard in root position is rejected
/// ([`FsError::OutsideRoot`]) so confinement stays decidable. `**` matches across directory levels
/// (recursive descent). Symlinked directories are **not** descended, which also bounds symlink
/// cycles. Returns files only (not the directories walked through).
///
/// # Errors
/// [`FsError::UnknownRoot`] if the root is unconfigured; [`FsError::OutsideRoot`] if the root
/// segment is itself a wildcard; [`FsError::Io`] on a read failure.
pub fn resolve_glob(roots: &FsRoots, pattern: &str) -> Result<Vec<FsRow>, FsError> {
    // A pattern with no wildcard is a single path: resolve + lstat it directly.
    if !pattern.contains(['*', '?']) {
        let abs = roots.resolve(pattern)?;
        let (name, _) = FsRoots::split(pattern)?;
        let base = roots
            .roots
            .get(name)
            .ok_or_else(|| FsError::UnknownRoot {
                path: pattern.to_string(),
                root: name.to_string(),
            })?
            .clone();
        return match abs.symlink_metadata() {
            Ok(meta) => Ok(vec![row_for(roots, name, &base, &abs, &meta)]),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(FsError::from_io(pattern, &e)),
        };
    }

    let (name, rel) = FsRoots::split(pattern)?;
    // The root-name segment must be a literal — a wildcard there would span multiple roots and
    // make confinement undecidable, so it is refused (defence in depth).
    if name.contains(['*', '?']) {
        return Err(FsError::OutsideRoot(pattern.to_string()));
    }
    let base = roots
        .roots
        .get(name)
        .ok_or_else(|| FsError::UnknownRoot {
            path: pattern.to_string(),
            root: name.to_string(),
        })?
        .clone();
    let segments: Vec<&str> = rel.split('/').filter(|s| !s.is_empty()).collect();

    let mut matches = Vec::new();
    walk_glob(roots, name, &base, base.clone(), &segments, &mut matches)?;
    matches.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(matches)
}

/// Recursively walk `dir` (inside root `name`/`base`) matching the remaining glob `segments`.
/// `**` consumes zero or more directory levels. Only regular files are collected into `out`.
fn walk_glob(
    roots: &FsRoots,
    name: &str,
    base: &StdPath,
    dir: PathBuf,
    segments: &[&str],
    out: &mut Vec<FsRow>,
) -> Result<(), FsError> {
    let Some((head, tail)) = segments.split_first() else {
        return Ok(());
    };

    if *head == "**" {
        // `**` matches the current directory (tail applied here) and every subdirectory.
        walk_glob(roots, name, base, dir.clone(), tail, out)?;
        for child in read_children(roots, name, base, &dir)? {
            if child.is_dir {
                let child_abs = child_abs(name, base, &child);
                walk_glob(roots, name, base, child_abs, segments, out)?;
            }
        }
        return Ok(());
    }

    for child in read_children(roots, name, base, &dir)? {
        if !segment_matches(head, &child.name) {
            continue;
        }
        if tail.is_empty() {
            if !child.is_dir {
                out.push(child);
            }
        } else if child.is_dir {
            let child_abs = child_abs(name, base, &child);
            walk_glob(roots, name, base, child_abs, tail, out)?;
        }
    }
    Ok(())
}

/// Reconstruct the absolute path of a scanned child from its VFS `/fs/<name>/<rel>` path.
fn child_abs(name: &str, base: &StdPath, child: &FsRow) -> PathBuf {
    let prefix = format!("{MOUNT}/{name}/");
    base.join(child.path.trim_start_matches(&prefix))
}

/// Read the immediate children of `dir` as owned rows (dotfiles excluded, links not followed). A
/// directory that does not exist yields an empty set rather than an error, so a glob over a
/// partially-present tree is robust.
fn read_children(
    roots: &FsRoots,
    name: &str,
    base: &StdPath,
    dir: &StdPath,
) -> Result<Vec<FsRow>, FsError> {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(FsError::from_io(&roots.abs_to_vfs(name, base, dir), &e)),
    };
    let mut rows = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| FsError::from_io(&roots.abs_to_vfs(name, base, dir), &e))?;
        let leaf = entry.file_name().to_string_lossy().into_owned();
        if is_dotfile(&leaf) {
            continue;
        }
        let meta = entry
            .path()
            .symlink_metadata()
            .map_err(|e| FsError::from_io(&roots.abs_to_vfs(name, base, &entry.path()), &e))?;
        rows.push(row_for(roots, name, base, &entry.path(), &meta));
    }
    Ok(rows)
}

/// Stream-read a blob's bytes into an owned `Vec` through the fixed [`COPY_BUF`] buffer (bounded
/// per-iteration memory). The decoded relation is produced by a codec downstream — this function
/// only moves bytes.
///
/// # Errors
/// [`FsError::UnknownRoot`] / [`FsError::OutsideRoot`] / [`FsError::NotFound`] / [`FsError::Io`].
pub fn read_blob(roots: &FsRoots, vfs: &str) -> Result<Vec<u8>, FsError> {
    let abs = roots.resolve(vfs)?;
    let mut file = File::open(&abs).map_err(|e| FsError::from_io(vfs, &e))?;
    let mut out = Vec::new();
    let mut buf = [0u8; COPY_BUF];
    loop {
        let n = file.read(&mut buf).map_err(|e| FsError::from_io(vfs, &e))?;
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n]);
    }
    Ok(out)
}

/// Atomically write `bytes` to `vfs`: stream into a sibling temp file, fsync, then `rename` over
/// the destination (retry-safe; an interrupted write leaves the original intact). Returns the byte
/// count written.
///
/// # Errors
/// [`FsError::UnknownRoot`] / [`FsError::OutsideRoot`] / [`FsError::Io`].
pub fn write_blob_atomic(roots: &FsRoots, vfs: &str, bytes: &[u8]) -> Result<u64, FsError> {
    let abs = roots.resolve(vfs)?;
    let parent = abs
        .parent()
        .map(StdPath::to_path_buf)
        .ok_or_else(|| FsError::OutsideRoot(vfs.to_string()))?;
    fs::create_dir_all(&parent).map_err(|e| FsError::from_io(vfs, &e))?;

    let tmp = temp_sibling(&abs);
    {
        let mut f = File::create(&tmp).map_err(|e| FsError::from_io(vfs, &e))?;
        f.write_all(bytes).map_err(|e| FsError::from_io(vfs, &e))?;
        f.sync_all().map_err(|e| FsError::from_io(vfs, &e))?;
    }
    fs::rename(&tmp, &abs).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        FsError::from_io(vfs, &e)
    })?;
    Ok(bytes.len() as u64)
}

/// A sibling temp path next to `dst` (`<dst>.qfs-tmp.<nanos>`), so the atomic `rename` stays on
/// the same filesystem (cross-device rename would fail).
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

/// Stream-copy `src` → `dst` through the fixed buffer, computing both a verify byte-length **and**
/// a streaming content hash ([`Fnv1a`]) incrementally over the copied stream, then — before
/// publishing — re-read the written temp file and assert BOTH its size AND its content hash match
/// the source (the copy→verify shape, RFD §6). Returns the byte count copied. Does **not** delete
/// the source (that is `mv`'s extra step, gated on this whole verification passing).
///
/// # Errors
/// [`FsError`] on resolve/IO failure, or [`FsError::VerifyFailed`] if the destination size **or**
/// content hash does not match the source after the copy.
pub fn copy_verify(roots: &FsRoots, src_vfs: &str, dst_vfs: &str) -> Result<u64, FsError> {
    let src = roots.resolve(src_vfs)?;
    let dst = roots.resolve(dst_vfs)?;
    let expected = src
        .metadata()
        .map_err(|e| FsError::from_io(src_vfs, &e))?
        .len();

    let parent = dst
        .parent()
        .map(StdPath::to_path_buf)
        .ok_or_else(|| FsError::OutsideRoot(dst_vfs.to_string()))?;
    fs::create_dir_all(&parent).map_err(|e| FsError::from_io(dst_vfs, &e))?;

    let tmp = temp_sibling(&dst);
    let mut reader = File::open(&src).map_err(|e| FsError::from_io(src_vfs, &e))?;
    let mut written: u64 = 0;
    let mut src_hash = Fnv1a::new();
    {
        let mut writer = File::create(&tmp).map_err(|e| FsError::from_io(dst_vfs, &e))?;
        let mut buf = [0u8; COPY_BUF];
        loop {
            let n = reader
                .read(&mut buf)
                .map_err(|e| FsError::from_io(src_vfs, &e))?;
            if n == 0 {
                break;
            }
            src_hash.update(&buf[..n]);
            writer
                .write_all(&buf[..n])
                .map_err(|e| FsError::from_io(dst_vfs, &e))?;
            written += n as u64;
        }
        writer
            .sync_all()
            .map_err(|e| FsError::from_io(dst_vfs, &e))?;
    }

    if let Err(e) = verify_copy(&tmp, dst_vfs, expected, written, src_hash.finish()) {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }

    fs::rename(&tmp, &dst).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        FsError::from_io(dst_vfs, &e)
    })?;
    Ok(written)
}

/// Verify a freshly-written copy candidate at `tmp` against the source: assert its byte length
/// equals `expected` (vs. the `written` stream count) AND its streamed content hash equals
/// `expected_hash`. Returns [`FsError::VerifyFailed`] on either mismatch.
///
/// # Errors
/// [`FsError::VerifyFailed`] if size or content hash diverges; [`FsError::Io`] if the candidate
/// cannot be re-read for hashing.
fn verify_copy(
    tmp: &StdPath,
    dst_vfs: &str,
    expected: u64,
    written: u64,
    expected_hash: u64,
) -> Result<(), FsError> {
    if written != expected {
        return Err(FsError::VerifyFailed {
            dst: dst_vfs.to_string(),
            expected,
            found: written,
        });
    }
    let dst_hash = hash_file(tmp, dst_vfs)?;
    if dst_hash != expected_hash {
        return Err(FsError::VerifyFailed {
            dst: dst_vfs.to_string(),
            expected,
            found: written,
        });
    }
    Ok(())
}

/// Stream-hash a file's content through the fixed [`COPY_BUF`] buffer (bounded memory) into an
/// [`Fnv1a`] digest. Used by [`copy_verify`] to re-read the published-candidate temp file and
/// confirm it matches the source content, not merely its length.
///
/// # Errors
/// [`FsError::Io`] (or `NotFound`) labelled with `vfs_label` on a read failure.
fn hash_file(path: &StdPath, vfs_label: &str) -> Result<u64, FsError> {
    let mut file = File::open(path).map_err(|e| FsError::from_io(vfs_label, &e))?;
    let mut hash = Fnv1a::new();
    let mut buf = [0u8; COPY_BUF];
    loop {
        let n = file
            .read(&mut buf)
            .map_err(|e| FsError::from_io(vfs_label, &e))?;
        if n == 0 {
            break;
        }
        hash.update(&buf[..n]);
    }
    Ok(hash.finish())
}

/// Remove a blob (file). A directory or missing path is rejected/structured. Deleting a real file
/// is **irreversible** (RFD §10) — the engine flags the `Remove` effect so `PREVIEW` warns and
/// `POLICY` reasons about it; this leg only performs the unlink once that gate passes.
///
/// # Errors
/// [`FsError::UnknownRoot`] / [`FsError::OutsideRoot`] / [`FsError::NotFound`] / [`FsError::Io`].
pub fn remove_blob(roots: &FsRoots, vfs: &str) -> Result<(), FsError> {
    let abs = roots.resolve(vfs)?;
    fs::remove_file(&abs).map_err(|e| FsError::from_io(vfs, &e))
}

#[cfg(test)]
mod verify_tests {
    //! Unit coverage for the size+hash verify primitives. Lints opt out via the crate-level
    //! `#![cfg_attr(test, allow(...))]`.
    use super::{hash_file, verify_copy, Fnv1a};
    use crate::error::FsError;
    use tempfile::TempDir;

    /// FNV-1a distinguishes two **same-length** but content-divergent byte strings — exactly the
    /// torn-copy case size-only verification cannot catch.
    #[test]
    fn content_hash_separates_same_length_divergence() {
        let mut a = Fnv1a::new();
        a.update(b"hello-qfs");
        let mut b = Fnv1a::new();
        b.update(b"hexxo-qfs"); // same length, two bytes differ
        assert_eq!(b"hello-qfs".len(), b"hexxo-qfs".len(), "lengths are equal");
        assert_ne!(a.finish(), b.finish(), "content hash must differ");
    }

    /// `verify_copy` REJECTS a candidate that matches on length but DIVERGES in content.
    #[test]
    fn verify_copy_rejects_length_equal_content_divergent() {
        let dir = TempDir::new().expect("tempdir");
        let tmp = dir.path().join("cand");
        std::fs::write(&tmp, b"hexxo-qfs").unwrap();
        let mut src = Fnv1a::new();
        src.update(b"hello-qfs");
        let len = b"hello-qfs".len() as u64;
        let err = verify_copy(&tmp, "/fs/r/dst", len, len, src.finish()).unwrap_err();
        match err {
            FsError::VerifyFailed { dst, .. } => assert_eq!(dst, "/fs/r/dst"),
            other => panic!("expected VerifyFailed, got {other:?}"),
        }
    }

    /// `hash_file` streams a file's content into the same digest as an in-memory fold.
    #[test]
    fn hash_file_matches_in_memory_fold() {
        let dir = TempDir::new().expect("tempdir");
        let p = dir.path().join("blob");
        let payload: Vec<u8> = (0..200_000u32).map(|i| (i % 257) as u8).collect();
        std::fs::write(&p, &payload).unwrap();
        let mut mem = Fnv1a::new();
        mem.update(&payload);
        assert_eq!(hash_file(&p, "/fs/r/blob").unwrap(), mem.finish());
    }
}
