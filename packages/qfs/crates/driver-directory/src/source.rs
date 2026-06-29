//! The **injected** directory-read seam (the vendor-free analogue of `qfs-driver-sql`'s
//! `SqlBackend` / `qfs-driver-sys`'s `SysBackend`). The introspective driver half is pure; the
//! impure read half — the live LDAP / AD / Entra / Google-Workspace client — is provided through
//! this trait, so this crate stays tokio-free, SDK-free, and wasm-buildable (only a leaf consumer
//! injects a networked client; the real connection is a DOCUMENTED SEAM, not a heavy SDK pulled
//! here).
//!
//! No vendor directory type crosses this boundary — only owned qfs DTOs (`RowBatch`, the
//! [`Provider`]/[`DirRelation`] tags, owned group-name `String`s, and the structured
//! [`DirectoryError`]).
//!
//! ## Authz is upstream of this data — beware the loop
//! A directory read that *feeds* a `member_of` policy decision must itself be safe to perform
//! **without that policy gating it** (else a cycle): membership is resolved through
//! [`resolve_is_member`] / [`DirectorySource::groups_of`] — a path that does **not** re-enter the
//! `member_of` evaluation. The binary calls this ONCE up front (t57 `resolve_memberships`) and
//! freezes the answer into the `DecisionContext`, keeping the policy `evaluate` pure.

use std::collections::BTreeMap;

use qfs_types::{Row, RowBatch, Value};

use crate::schema::{directory_relation_schema, parse_group_ref, DirRelation, Provider};

/// A structured, **secret-free** error from a directory read (RFD §5, AI-consumable). Names a
/// provider / relation and a redacted detail — never a bind credential, never row PII beyond the
/// metadata the schema already exposes.
#[derive(Debug, thiserror::Error)]
pub enum DirectoryError {
    /// The path did not resolve to a known `/directories/<provider>/<relation>` node.
    #[error("`{path}` is not a known /directories node")]
    UnknownNode {
        /// The offending path (an opaque directory path; carries no secret).
        path: String,
    },
    /// A live directory I/O failure (the leaf maps its client error in here as a secret-free
    /// string — a host/endpoint is infra, never a credential).
    #[error("directory backend: {0}")]
    Backend(String),
}

/// The read seam a leaf implements over a live identity directory (LDAP / AD / Entra / Google
/// Workspace). The driver crate holds only `&dyn DirectorySource` / `Arc<dyn DirectorySource>`;
/// the concrete networked client lives binary-side (a DOCUMENTED SEAM — no heavy SDK is pulled
/// into this crate).
///
/// READ-FIRST: there is no write method — this slice never provisions or mutates a directory.
pub trait DirectorySource: Send + Sync {
    /// Scan all rows of a `/directories/<provider>/<relation>` relation into the owned [`RowBatch`]
    /// shaped by [`directory_relation_schema`].
    ///
    /// MUST return metadata only — group/user handles + display names + membership pairs, never a
    /// bind secret (the schema has no credential column).
    ///
    /// # Errors
    /// [`DirectoryError::Backend`] on an I/O / decode failure.
    fn scan(&self, provider: Provider, relation: DirRelation) -> Result<RowBatch, DirectoryError>;

    /// The groups `user` belongs to in `provider`'s directory — the membership-resolution read the
    /// t57 `member_of` predicate consults. Returns owned group handles (the `<g>` segment of a
    /// `/directories/<provider>/groups/<g>` ref). This is the path that does NOT re-enter the
    /// policy `member_of` evaluation (see the module note).
    ///
    /// # Errors
    /// [`DirectoryError::Backend`] on an I/O failure.
    fn groups_of(&self, provider: Provider, user: &str) -> Result<Vec<String>, DirectoryError>;
}

/// Resolve a `member_of('/directories/<provider>/groups/<g>')` predicate for `actor` against
/// `source` — the bridge from a directory ref to a yes/no the t57 `DecisionContext` freezes.
///
/// Returns `false` (fail closed) for an anonymous actor, a ref that does not name a concrete group
/// node ([`parse_group_ref`]), or any backend error — a directory read must never *grant* on an
/// error. The resolution reads `source.groups_of(...)`, which does NOT re-enter the `member_of`
/// evaluation, so it cannot cycle through the policy that consults it.
#[must_use]
pub fn resolve_is_member(
    source: &dyn DirectorySource,
    actor: Option<&str>,
    directory_ref: &str,
) -> bool {
    let Some(user) = actor else {
        return false;
    };
    let Some((provider, group)) = parse_group_ref(directory_ref) else {
        return false;
    };
    match source.groups_of(provider, user) {
        Ok(groups) => groups.iter().any(|g| g == &group),
        Err(_) => false,
    }
}

/// A hermetic, in-memory directory the tests (and the local/in-memory membership case) use — the
/// concrete [`DirectorySource`] that makes the t57 `member_of` predicate genuinely LIVE without any
/// network, native code, or credentials. The real LDAP/AD/Entra/Workspace client is the documented
/// seam that implements the SAME trait against a live connection.
///
/// The fixture is **provider-agnostic**: it models one directory whose rows are returned regardless
/// of the `provider` label, which is all the hermetic tests need (a real backend keys on provider
/// via its own client).
#[derive(Debug, Clone, Default)]
pub struct FixtureDirectory {
    /// `user handle -> display name` (display optional, modelled as empty string).
    users: BTreeMap<String, String>,
    /// `group handle -> display name`.
    groups: BTreeMap<String, String>,
    /// `user handle -> the set of group handles it belongs to` (deterministic order).
    memberships: BTreeMap<String, Vec<String>>,
}

impl FixtureDirectory {
    /// An empty fixture directory.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a user (handle + display name) to the fixture (builder).
    #[must_use]
    pub fn with_user(mut self, user: impl Into<String>, display: impl Into<String>) -> Self {
        self.users.insert(user.into(), display.into());
        self
    }

    /// Add a group (handle + display name) to the fixture (builder).
    #[must_use]
    pub fn with_group(mut self, group: impl Into<String>, display: impl Into<String>) -> Self {
        self.groups.insert(group.into(), display.into());
        self
    }

    /// Record that `user` is a member of `group` (builder). Both are recorded as members of the
    /// flat membership relation; duplicate pairs are de-duplicated.
    #[must_use]
    pub fn with_member(mut self, user: impl Into<String>, group: impl Into<String>) -> Self {
        let user = user.into();
        let group = group.into();
        let entry = self.memberships.entry(user).or_default();
        if !entry.iter().any(|g| g == &group) {
            entry.push(group);
        }
        self
    }
}

impl DirectorySource for FixtureDirectory {
    fn scan(&self, _provider: Provider, relation: DirRelation) -> Result<RowBatch, DirectoryError> {
        let schema = directory_relation_schema(relation);
        let rows = match relation {
            DirRelation::Groups => self
                .groups
                .iter()
                .map(|(g, display)| Row::new(vec![Value::Text(g.clone()), display_value(display)]))
                .collect(),
            DirRelation::Users => self
                .users
                .iter()
                .map(|(u, display)| Row::new(vec![Value::Text(u.clone()), display_value(display)]))
                .collect(),
            DirRelation::Memberships => {
                let mut rows = Vec::new();
                for (user, groups) in &self.memberships {
                    for group in groups {
                        rows.push(Row::new(vec![
                            Value::Text(user.clone()),
                            Value::Text(group.clone()),
                        ]));
                    }
                }
                rows
            }
        };
        Ok(RowBatch::new(schema, rows))
    }

    fn groups_of(&self, _provider: Provider, user: &str) -> Result<Vec<String>, DirectoryError> {
        Ok(self.memberships.get(user).cloned().unwrap_or_default())
    }
}

/// Render an optional display name: an empty string is the absent (`Null`) display, otherwise the
/// owned text — so the `display_name` column conforms to its `nullable` schema.
fn display_value(display: &str) -> Value {
    if display.is_empty() {
        Value::Null
    } else {
        Value::Text(display.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A small fixture directory: two users, two groups, eng membership for alice only.
    fn fixture() -> FixtureDirectory {
        FixtureDirectory::new()
            .with_user("alice@corp.example", "Alice")
            .with_user("bob@corp.example", "Bob")
            .with_group("eng", "Engineering")
            .with_group("sales", "Sales")
            .with_member("alice@corp.example", "eng")
    }

    /// A fixture directory resolves a user's group memberships through the seam (the load-bearing
    /// hermetic deliverable): alice is in `eng`, bob is in nothing.
    #[test]
    fn fixture_resolves_group_memberships() {
        let dir = fixture();
        assert_eq!(
            dir.groups_of(Provider::GoogleWorkspace, "alice@corp.example")
                .unwrap(),
            vec!["eng".to_string()]
        );
        assert!(dir
            .groups_of(Provider::GoogleWorkspace, "bob@corp.example")
            .unwrap()
            .is_empty());
    }

    /// `resolve_is_member` bridges a `/directories/.../groups/<g>` ref to the membership read:
    /// a member is granted, a non-member is denied, and fail-closed cases (anonymous / bad ref)
    /// deny.
    #[test]
    fn resolve_is_member_grants_member_denies_others() {
        let dir = fixture();
        let eng = "/directories/google/groups/eng";
        assert!(resolve_is_member(&dir, Some("alice@corp.example"), eng));
        assert!(!resolve_is_member(&dir, Some("bob@corp.example"), eng));
        // Fail closed: anonymous actor.
        assert!(!resolve_is_member(&dir, None, eng));
        // Fail closed: a ref that is not a concrete group node.
        assert!(!resolve_is_member(
            &dir,
            Some("alice@corp.example"),
            "/directories/google/groups"
        ));
    }

    /// The scanned relations are well-formed RowBatches conforming to the canonical schema, and
    /// carry only metadata (no credential column exists to leak).
    #[test]
    fn scan_yields_conformant_metadata_rows() {
        let dir = fixture();
        for relation in [
            DirRelation::Groups,
            DirRelation::Users,
            DirRelation::Memberships,
        ] {
            let batch = dir.scan(Provider::Ldap, relation).unwrap();
            assert!(
                batch.is_conformant(),
                "{relation:?} batch conforms to schema"
            );
        }
        // The flat membership relation carries exactly the one recorded pair.
        let memberships = dir.scan(Provider::Ldap, DirRelation::Memberships).unwrap();
        assert_eq!(memberships.rows.len(), 1);
        assert_eq!(
            memberships.rows[0].values,
            vec![
                Value::Text("alice@corp.example".to_string()),
                Value::Text("eng".to_string())
            ]
        );
    }
}
