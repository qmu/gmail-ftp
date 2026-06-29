//! The `/directories/<provider>/...` node model: the [`Provider`] + [`DirRelation`] sum types,
//! their path↔segment mapping, the single-source-of-truth [`directory_relation_schema`], the
//! per-node [`directory_relation_capabilities`], and the [`parse_group_ref`] used to resolve a
//! `member_of('/directories/<provider>/groups/<g>')` predicate.
//!
//! This is the **pure, credential-free** introspective surface (RFD-0001 §3 purity / §5). It
//! mirrors the closed-core `sys_node_schema(node)` pattern: `DESCRIBE /directories/google/groups`
//! returns a stable typed [`Schema`] with **no directory connection and no secrets**, so describe
//! (and the parse-time capability gate) read one source of truth that can never drift from the
//! rows the backend later scans. NOTHING here opens a directory connection or reads a bind
//! credential.
//!
//! ## Read-first is structural
//! Every relation declares ONLY `SELECT` (see [`directory_relation_capabilities`]). There is no
//! write verb anywhere in this surface, so the much larger risk surface of provisioning /
//! deprovisioning identities (`INSERT`/`UPDATE`/`REMOVE` against a live directory) is **not
//! expressible** through this driver in this slice — the schema + capabilities are the boundary.

use qfs_types::{Column, ColumnType, Schema};

/// The reserved mount point for the identity-directory driver (roadmap §1.3 / decision P:
/// `/directories` is a reserved realm).
pub const DIRECTORIES_MOUNT: &str = "/directories";

/// One external identity-directory backend exposed under `/directories/<provider>/...`
/// (roadmap §1.2, decision I). A **closed set** — a new backend adds a variant here, never a
/// side-channel API (the one-engine constraint). The provider is a *label* on the path; the
/// relational schema is identical across providers (groups/users/memberships are universal), so
/// the only per-provider difference lives behind the injected `DirectorySource` (the live client).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    /// `google` — Google Workspace (reuses the existing OAuth client / consent flow, t54).
    GoogleWorkspace,
    /// `ldap` — a generic LDAP v3 directory (thin bind client; no heavy vendor SDK).
    Ldap,
    /// `ad` — on-premises Active Directory (LDAP-shaped, AD semantics).
    ActiveDirectory,
    /// `entra` — Microsoft Entra ID (formerly Azure AD), Graph-shaped.
    Entra,
}

impl Provider {
    /// Resolve a path segment (`google`/`ldap`/`ad`/`entra`) to its provider.
    #[must_use]
    pub fn from_segment(seg: &str) -> Option<Self> {
        match seg {
            "google" => Some(Self::GoogleWorkspace),
            "ldap" => Some(Self::Ldap),
            "ad" => Some(Self::ActiveDirectory),
            "entra" => Some(Self::Entra),
            _ => None,
        }
    }

    /// The path segment naming this provider (`google`, `ldap`, …).
    #[must_use]
    pub fn segment(self) -> &'static str {
        match self {
            Self::GoogleWorkspace => "google",
            Self::Ldap => "ldap",
            Self::ActiveDirectory => "ad",
            Self::Entra => "entra",
        }
    }
}

/// One relation a directory provider exposes under `/directories/<provider>/<relation>`
/// (roadmap §1.2). A **closed set**: `groups`, `users`, and the flat `memberships` join.
///
/// The canonical group/user path shape (the "open decision" the ticket flags): a group is named
/// by `/directories/<provider>/groups/<g>` (the argument the t57 `member_of(...)` predicate takes),
/// and `memberships` is the flat `(user, group)` relation that backs membership resolution. Both
/// are offered; `member_of` names a `groups/<g>` node and resolves through `memberships`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirRelation {
    /// `/directories/<provider>/groups` — the directory's groups/teams (READ-only).
    Groups,
    /// `/directories/<provider>/users` — the directory's user identities, METADATA only (READ-only).
    Users,
    /// `/directories/<provider>/memberships` — the flat `(user, group)` membership join (READ-only).
    Memberships,
}

impl DirRelation {
    /// Resolve a path segment (`groups`/`users`/`memberships`) to its relation.
    #[must_use]
    pub fn from_segment(seg: &str) -> Option<Self> {
        match seg {
            "groups" => Some(Self::Groups),
            "users" => Some(Self::Users),
            "memberships" => Some(Self::Memberships),
            _ => None,
        }
    }

    /// The path segment naming this relation (`groups`, `users`, `memberships`).
    #[must_use]
    pub fn segment(self) -> &'static str {
        match self {
            Self::Groups => "groups",
            Self::Users => "users",
            Self::Memberships => "memberships",
        }
    }
}

/// Resolve a `/directories/...` path to its `(provider, relation)`, if the path names a known
/// directory relation. `/directories/google/groups` and `/directories/google/groups/eng/members`
/// both resolve to `(GoogleWorkspace, Groups)`. Returns `None` for `/directories` itself, an
/// unknown provider, or an unknown relation segment.
#[must_use]
pub fn node_for_path(path: &str) -> Option<(Provider, DirRelation)> {
    let rest = path
        .strip_prefix("/directories/")
        .or_else(|| path.strip_prefix("directories/"))?;
    let mut segs = rest.split('/');
    let provider = Provider::from_segment(segs.next()?)?;
    let relation = DirRelation::from_segment(segs.next()?)?;
    Some((provider, relation))
}

/// Parse a `member_of('/directories/<provider>/groups/<g>')` predicate argument into its
/// `(provider, group)`. Returns `None` unless the ref names a concrete group node — i.e. the path
/// is exactly `/directories/<provider>/groups/<g>` (a `<g>` segment is required; the bare
/// `/directories/<provider>/groups` relation is not a membership target). This is the ONE place a
/// directory ref is decoded for the t57 membership resolution.
#[must_use]
pub fn parse_group_ref(path: &str) -> Option<(Provider, String)> {
    let rest = path
        .strip_prefix("/directories/")
        .or_else(|| path.strip_prefix("directories/"))?;
    let mut segs = rest.split('/');
    let provider = Provider::from_segment(segs.next()?)?;
    if segs.next()? != "groups" {
        return None;
    }
    let group = segs.next()?;
    if group.is_empty() {
        return None;
    }
    Some((provider, group.to_string()))
}

/// The typed [`Schema`] of a `/directories/<provider>/<relation>` relation — the **canonical**
/// source of truth `DESCRIBE` and the backend scan both read. Pure data; no live directory, no
/// creds.
///
/// METADATA ONLY: a directory identity is named by its handle (an email / `sAMAccountName` /
/// `userPrincipalName`) and a display name. There is structurally **no** column for a password
/// hash, a bind secret, or any credential material — a secret cannot surface through this path
/// even by accident (the redaction contract, the same boundary `describe` enforces).
#[must_use]
pub fn directory_relation_schema(relation: DirRelation) -> Schema {
    let col = |name: &str, ty: ColumnType, nullable: bool| Column::new(name, ty, nullable);
    match relation {
        // The directory's groups/teams: a stable group handle + an optional human display name.
        DirRelation::Groups => Schema::new(vec![
            col("group", ColumnType::Text, false),
            col("display_name", ColumnType::Text, true),
        ]),
        // The directory's user identities: the authentication handle + an optional display name.
        // METADATA ONLY — no `password_hash`, no `bind_secret`, no credential column by design.
        DirRelation::Users => Schema::new(vec![
            col("user", ColumnType::Text, false),
            col("display_name", ColumnType::Text, true),
        ]),
        // The flat membership join the t57 `member_of` predicate resolves through: which user
        // belongs to which group.
        DirRelation::Memberships => Schema::new(vec![
            col("user", ColumnType::Text, false),
            col("group", ColumnType::Text, false),
        ]),
    }
}
