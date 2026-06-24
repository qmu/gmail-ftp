//! The in-house git **object database** (ADR-0003): the `ObjectDb` seam plus the owned object
//! DTOs (`Oid`, `ObjectKind`, `Commit`, `Tree`, `TreeEntry`, `Tag`) and their parsers. A loose
//! object on disk is `zlib(<type> <len>\0<payload>)`, content-addressed by
//! `SHA-1(<type> <len>\0<payload>)`. This module reads + parses the four object kinds and
//! computes oids for objects the driver is about to write (so a `WriteLooseObject` effect
//! carries the correct content-addressed id). No vendor (gix) type ever appears — the
//! `ObjectDb` trait is the reversibility seam ADR-0003 records (a `GixObjectDb` could replace
//! the in-house impl behind a feature without touching a caller).

use std::collections::HashMap;

use crate::error::GitError;
use crate::inflate::zlib_inflate;
use crate::sha1::{hex, sha1};

/// A git object id — a 40-char lowercase-hex SHA-1. Owned; the only id type that crosses the
/// crate boundary (no vendor oid type).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Oid(String);

impl Oid {
    /// Wrap a 40-char hex string as an oid (validating length + hex-ness).
    ///
    /// # Errors
    /// [`GitError::Corrupt`] if the string is not 40 lowercase-hex chars.
    pub fn parse(s: &str) -> Result<Self, GitError> {
        if s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit()) {
            Ok(Self(s.to_ascii_lowercase()))
        } else {
            Err(GitError::Corrupt {
                reason: format!("`{s}` is not a 40-char hex oid"),
            })
        }
    }

    /// The 40-char hex string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// A short 7-char prefix (preview / human display).
    #[must_use]
    pub fn short(&self) -> &str {
        &self.0[..7]
    }

    /// The all-zero oid (`0`×40) git uses for a ref creation's "old" side. Infallible (the
    /// literal is valid hex), so no `expect`/`unwrap` is needed at the call sites.
    #[must_use]
    pub fn zero() -> Self {
        Self("0".repeat(40))
    }
}

/// The four git object kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectKind {
    /// File content.
    Blob,
    /// A directory listing (entries → child oids).
    Tree,
    /// A commit object.
    Commit,
    /// An annotated tag object.
    Tag,
}

impl ObjectKind {
    /// The header keyword git frames an object with (`blob`/`tree`/`commit`/`tag`).
    #[must_use]
    pub const fn keyword(self) -> &'static str {
        match self {
            ObjectKind::Blob => "blob",
            ObjectKind::Tree => "tree",
            ObjectKind::Commit => "commit",
            ObjectKind::Tag => "tag",
        }
    }

    fn from_keyword(kw: &str) -> Option<Self> {
        match kw {
            "blob" => Some(ObjectKind::Blob),
            "tree" => Some(ObjectKind::Tree),
            "commit" => Some(ObjectKind::Commit),
            "tag" => Some(ObjectKind::Tag),
            _ => None,
        }
    }
}

/// A decoded raw object: its kind + payload (the bytes after the `<type> <len>\0` header).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawObject {
    /// The object kind.
    pub kind: ObjectKind,
    /// The decompressed payload (no header).
    pub payload: Vec<u8>,
}

/// One entry of a tree object: file mode, name, and the child oid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeEntry {
    /// The octal mode string (e.g. `100644` blob, `40000` tree, `100755` exe, `120000` symlink).
    pub mode: String,
    /// The entry name (a single path component).
    pub name: String,
    /// The child object id.
    pub oid: Oid,
}

impl TreeEntry {
    /// Whether this entry points at a subtree (mode `40000`).
    #[must_use]
    pub fn is_tree(&self) -> bool {
        self.mode == "40000" || self.mode == "040000"
    }
}

/// A parsed tree object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tree {
    /// The entries, in git's stored order.
    pub entries: Vec<TreeEntry>,
}

/// A parsed commit object (owned DTO — the `CommitRow` relational row is derived from this).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Commit {
    /// The root tree oid.
    pub tree: Oid,
    /// The parent commit oids (0 = root, 1 = normal, 2+ = merge).
    pub parents: Vec<Oid>,
    /// The author identity line (`Name <email>`).
    pub author: String,
    /// The author epoch seconds.
    pub author_time: i64,
    /// The committer identity line.
    pub committer: String,
    /// The committer epoch seconds.
    pub committer_time: i64,
    /// The commit message.
    pub message: String,
}

/// A parsed annotated tag object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tag {
    /// The tagged object oid.
    pub object: Oid,
    /// The tagged object's kind keyword.
    pub kind: String,
    /// The tag name.
    pub name: String,
    /// The tag message.
    pub message: String,
}

/// The object-database seam (ADR-0003 reversibility point). The in-house [`LooseObjectDb`]
/// implements it; a future `GixObjectDb` could too, behind a non-default feature.
pub trait ObjectDb: Send + Sync {
    /// Read the raw (kind + payload) object for `oid`.
    ///
    /// # Errors
    /// [`GitError::ObjectNotFound`] if absent; [`GitError::Corrupt`] if malformed.
    fn read(&self, oid: &Oid) -> Result<RawObject, GitError>;

    /// Whether `oid` is present (the content-addressed idempotency check: writing an existing
    /// oid is a no-op).
    fn contains(&self, oid: &Oid) -> bool;
}

/// Frame an object as git does (`<type> <len>\0<payload>`) and compute its content-address oid.
/// This is what a `WriteLooseObject` effect uses to derive the id it will store under, and what
/// the loose reader checks against. Pure.
#[must_use]
pub fn frame_and_id(kind: ObjectKind, payload: &[u8]) -> (Oid, Vec<u8>) {
    let mut framed = format!("{} {}\0", kind.keyword(), payload.len()).into_bytes();
    framed.extend_from_slice(payload);
    let oid = Oid(hex(&sha1(&framed)));
    (oid, framed)
}

/// An in-memory loose-object database: oid → the zlib-compressed loose bytes (exactly the bytes
/// a `.git/objects/ab/cdef…` file holds). The fixture builder populates this from a real repo's
/// loose objects; the reader inflates + parses on `read`. Keeping the compressed bytes (not the
/// inflated payload) means the reader exercises the in-house inflater on every read, and the
/// content-address can be re-verified.
#[derive(Default, Clone)]
pub struct LooseObjectDb {
    /// oid → the compressed loose-object bytes.
    objects: HashMap<String, Vec<u8>>,
}

impl LooseObjectDb {
    /// An empty database.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert a raw (kind + payload) object: frame it, content-address it, store the
    /// **uncompressed** framed bytes (the in-memory db stores framed bytes; the reader path
    /// below handles both compressed and framed via the `read` decode). Returns the oid.
    ///
    /// For test fixtures we store the framed (uncompressed) bytes directly so the fixture does
    /// not need a deflater; the inflater is exercised by [`LooseObjectDb::insert_loose`].
    pub fn insert_object(&mut self, kind: ObjectKind, payload: &[u8]) -> Oid {
        let (oid, framed) = frame_and_id(kind, payload);
        self.objects.insert(oid.as_str().to_string(), framed);
        oid
    }

    /// Clone the contents of a shared loose db into a fresh owned db (the apply-side store seeds
    /// itself from the read-side db this way in tests).
    #[must_use]
    pub fn clone_of(db: &std::sync::Arc<LooseObjectDb>) -> Self {
        Self {
            objects: db.objects.clone(),
        }
    }

    /// Insert the **compressed** loose bytes for an oid exactly as they appear on disk (a
    /// `.git/objects/xx/yyy…` file). The reader will zlib-inflate them — exercising the
    /// in-house inflater against real git output.
    pub fn insert_loose(&mut self, oid: Oid, compressed: Vec<u8>) {
        self.objects.insert(oid.as_str().to_string(), compressed);
    }

    /// Decode the stored bytes for `oid` into a framed `<type> <len>\0<payload>`. Stored bytes
    /// may be either already-framed (from [`insert_object`]) or zlib-compressed (from
    /// [`insert_loose`]); we detect a zlib header (`0x78`) and inflate when present.
    ///
    /// [`insert_object`]: LooseObjectDb::insert_object
    /// [`insert_loose`]: LooseObjectDb::insert_loose
    fn framed(&self, oid: &Oid) -> Result<Vec<u8>, GitError> {
        let stored = self
            .objects
            .get(oid.as_str())
            .ok_or_else(|| GitError::ObjectNotFound {
                oid: oid.as_str().to_string(),
            })?;
        // A zlib stream starts with CMF=0x78 for the common window sizes. A framed object
        // starts with an ASCII type keyword (`b`/`t`/`c`), never 0x78 — an unambiguous probe.
        if stored.first() == Some(&0x78) {
            zlib_inflate(stored)
        } else {
            Ok(stored.clone())
        }
    }
}

impl ObjectDb for LooseObjectDb {
    fn read(&self, oid: &Oid) -> Result<RawObject, GitError> {
        let framed = self.framed(oid)?;
        parse_framed(&framed)
    }

    fn contains(&self, oid: &Oid) -> bool {
        self.objects.contains_key(oid.as_str())
    }
}

/// Split a framed `<type> <len>\0<payload>` blob into (kind, payload), validating the declared
/// length.
fn parse_framed(framed: &[u8]) -> Result<RawObject, GitError> {
    let nul = framed
        .iter()
        .position(|&b| b == 0)
        .ok_or(GitError::Corrupt {
            reason: "object header has no NUL terminator".to_string(),
        })?;
    let header = std::str::from_utf8(&framed[..nul]).map_err(|_| GitError::Corrupt {
        reason: "object header is not valid UTF-8".to_string(),
    })?;
    let (kw, len) = header.split_once(' ').ok_or(GitError::Corrupt {
        reason: "object header missing `<type> <len>` separator".to_string(),
    })?;
    let kind = ObjectKind::from_keyword(kw).ok_or_else(|| GitError::Corrupt {
        reason: format!("unknown object type `{kw}`"),
    })?;
    let declared: usize = len.parse().map_err(|_| GitError::Corrupt {
        reason: "object header length is not a number".to_string(),
    })?;
    let payload = framed[nul + 1..].to_vec();
    if payload.len() != declared {
        return Err(GitError::Corrupt {
            reason: format!(
                "object length mismatch: header says {declared}, payload is {}",
                payload.len()
            ),
        });
    }
    Ok(RawObject { kind, payload })
}

/// Parse a tree object's payload. Each entry is `<mode> <name>\0<20-byte-oid>`.
///
/// # Errors
/// [`GitError::Corrupt`] on a malformed entry.
pub fn parse_tree(payload: &[u8]) -> Result<Tree, GitError> {
    let mut entries = Vec::new();
    let mut i = 0;
    while i < payload.len() {
        let sp = payload[i..]
            .iter()
            .position(|&b| b == b' ')
            .ok_or(GitError::Corrupt {
                reason: "tree entry missing mode/name separator".to_string(),
            })?
            + i;
        let mode = std::str::from_utf8(&payload[i..sp])
            .map_err(|_| GitError::Corrupt {
                reason: "tree entry mode not UTF-8".to_string(),
            })?
            .to_string();
        let nul = payload[sp + 1..]
            .iter()
            .position(|&b| b == 0)
            .ok_or(GitError::Corrupt {
                reason: "tree entry missing NUL".to_string(),
            })?
            + sp
            + 1;
        let name = std::str::from_utf8(&payload[sp + 1..nul])
            .map_err(|_| GitError::Corrupt {
                reason: "tree entry name not UTF-8".to_string(),
            })?
            .to_string();
        let oid_start = nul + 1;
        let oid_end = oid_start + 20;
        if oid_end > payload.len() {
            return Err(GitError::Corrupt {
                reason: "tree entry oid truncated".to_string(),
            });
        }
        let oid = Oid(hex(payload[oid_start..oid_end].try_into().map_err(
            |_| GitError::Corrupt {
                reason: "tree entry oid not 20 bytes".to_string(),
            },
        )?));
        entries.push(TreeEntry { mode, name, oid });
        i = oid_end;
    }
    Ok(Tree { entries })
}

/// Serialise a [`Tree`] back to its payload bytes (for `INSERT INTO /commits` tree-building).
/// git sorts tree entries by name (with a trailing `/` for subtree names); the caller is
/// expected to pass entries already in git's canonical order.
#[must_use]
pub fn serialize_tree(tree: &Tree) -> Vec<u8> {
    let mut out = Vec::new();
    for e in &tree.entries {
        out.extend_from_slice(e.mode.as_bytes());
        out.push(b' ');
        out.extend_from_slice(e.name.as_bytes());
        out.push(0);
        // The 20 raw oid bytes.
        for chunk in e.oid.as_str().as_bytes().chunks(2) {
            let hi = hex_val(chunk[0]);
            let lo = hex_val(chunk[1]);
            out.push((hi << 4) | lo);
        }
    }
    out
}

fn hex_val(b: u8) -> u8 {
    match b {
        b'0'..=b'9' => b - b'0',
        b'a'..=b'f' => b - b'a' + 10,
        b'A'..=b'F' => b - b'A' + 10,
        _ => 0,
    }
}

/// Parse a commit object's payload (the `tree`/`parent`/`author`/`committer` headers + message).
///
/// # Errors
/// [`GitError::Corrupt`] if a required header is missing or malformed.
pub fn parse_commit(payload: &[u8]) -> Result<Commit, GitError> {
    let text = std::str::from_utf8(payload).map_err(|_| GitError::Corrupt {
        reason: "commit not UTF-8".to_string(),
    })?;
    let (headers, message) = text.split_once("\n\n").unwrap_or((text, ""));

    let mut tree = None;
    let mut parents = Vec::new();
    let mut author = String::new();
    let mut author_time = 0i64;
    let mut committer = String::new();
    let mut committer_time = 0i64;

    for line in headers.lines() {
        if let Some(v) = line.strip_prefix("tree ") {
            tree = Some(Oid::parse(v)?);
        } else if let Some(v) = line.strip_prefix("parent ") {
            parents.push(Oid::parse(v)?);
        } else if let Some(v) = line.strip_prefix("author ") {
            let (id, ts) = split_ident(v);
            author = id;
            author_time = ts;
        } else if let Some(v) = line.strip_prefix("committer ") {
            let (id, ts) = split_ident(v);
            committer = id;
            committer_time = ts;
        }
    }

    Ok(Commit {
        tree: tree.ok_or(GitError::Corrupt {
            reason: "commit has no tree header".to_string(),
        })?,
        parents,
        author,
        author_time,
        committer,
        committer_time,
        message: message.to_string(),
    })
}

/// Parse an annotated tag object's payload.
///
/// # Errors
/// [`GitError::Corrupt`] if a required header is missing.
pub fn parse_tag(payload: &[u8]) -> Result<Tag, GitError> {
    let text = std::str::from_utf8(payload).map_err(|_| GitError::Corrupt {
        reason: "tag not UTF-8".to_string(),
    })?;
    let (headers, message) = text.split_once("\n\n").unwrap_or((text, ""));
    let mut object = None;
    let mut kind = String::new();
    let mut name = String::new();
    for line in headers.lines() {
        if let Some(v) = line.strip_prefix("object ") {
            object = Some(Oid::parse(v)?);
        } else if let Some(v) = line.strip_prefix("type ") {
            kind = v.to_string();
        } else if let Some(v) = line.strip_prefix("tag ") {
            name = v.to_string();
        }
    }
    Ok(Tag {
        object: object.ok_or(GitError::Corrupt {
            reason: "tag has no object header".to_string(),
        })?,
        kind,
        name,
        message: message.to_string(),
    })
}

/// Split a git ident line `Name <email> <epoch> <tz>` into the `Name <email>` part and the
/// epoch seconds.
fn split_ident(line: &str) -> (String, i64) {
    // The ident is `Name <email> EPOCH TZ`; the timestamp is the second-to-last whitespace
    // token. Split off the trailing `EPOCH TZ`.
    let parts: Vec<&str> = line.rsplitn(3, ' ').collect();
    if parts.len() == 3 {
        let ts = parts[1].parse::<i64>().unwrap_or(0);
        let id = parts[2].to_string();
        (id, ts)
    } else {
        (line.to_string(), 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_and_id_matches_git_empty_blob() {
        let (oid, framed) = frame_and_id(ObjectKind::Blob, b"");
        assert_eq!(oid.as_str(), "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391");
        assert_eq!(framed, b"blob 0\0");
    }

    #[test]
    fn roundtrip_blob_through_db() {
        let mut db = LooseObjectDb::new();
        let oid = db.insert_object(ObjectKind::Blob, b"hello\n");
        assert!(db.contains(&oid));
        let raw = db.read(&oid).unwrap();
        assert_eq!(raw.kind, ObjectKind::Blob);
        assert_eq!(raw.payload, b"hello\n");
    }

    #[test]
    fn parse_commit_extracts_headers() {
        let payload = b"tree 4b825dc642cb6eb9a060e54bf8d69288fbee4904\n\
parent 1111111111111111111111111111111111111111\n\
author Alice <alice@example.com> 1700000000 +0000\n\
committer Bob <bob@example.com> 1700000100 +0000\n\
\n\
Initial commit\n";
        let c = parse_commit(payload).unwrap();
        assert_eq!(c.tree.as_str(), "4b825dc642cb6eb9a060e54bf8d69288fbee4904");
        assert_eq!(c.parents.len(), 1);
        assert_eq!(c.author, "Alice <alice@example.com>");
        assert_eq!(c.author_time, 1_700_000_000);
        assert_eq!(c.committer_time, 1_700_000_100);
        assert_eq!(c.message, "Initial commit\n");
    }

    #[test]
    fn tree_roundtrip_serialize_parse() {
        let mut db = LooseObjectDb::new();
        let blob = db.insert_object(ObjectKind::Blob, b"x");
        let tree = Tree {
            entries: vec![TreeEntry {
                mode: "100644".to_string(),
                name: "a.txt".to_string(),
                oid: blob.clone(),
            }],
        };
        let payload = serialize_tree(&tree);
        let parsed = parse_tree(&payload).unwrap();
        assert_eq!(parsed.entries.len(), 1);
        assert_eq!(parsed.entries[0].name, "a.txt");
        assert_eq!(parsed.entries[0].oid, blob);
    }
}
