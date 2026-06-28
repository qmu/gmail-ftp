//! The owned, secret-free DTOs that key the store and describe a connection: [`ConnectionId`],
//! [`CredentialKey`], and [`ConnectionRecord`] (RFD-0001 §9 — owned DTOs only, no vendor
//! type, no secret material crosses this boundary).
//!
//! The store is keyed by `(driver, connection)`. Capability gating (§3) falls out of this
//! by construction: a [`CredentialKey`] names exactly one driver, so a driver that
//! resolves a key for its own [`DriverId`] can never reach another driver's credential —
//! cross-driver key access is impossible, not merely policed.
//!
//! Reuses [`qfs_types::DriverId`] (the canonical owned driver identity, a spine leaf)
//! rather than minting a second one, so the secrets surface speaks the same id the
//! Driver contract and the audit ledger do.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

pub use qfs_types::DriverId;

/// A named connection *within a driver*, e.g. `work`, `personal`, `prod`. Owned text; carries
/// no secret material (the connection name is metadata, safe to log and to surface in a
/// resolution decision). Validated to be non-empty and free of the path/selector
/// separators so it can round-trip through a `(driver, connection)` map and an `@connection`
/// selector unambiguously.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ConnectionId(String);

impl ConnectionId {
    /// Construct a connection id, rejecting empties and reserved separators. Returns the
    /// owned id on success.
    ///
    /// # Errors
    /// [`ConnectionIdError`] if the name is empty or contains a reserved character
    /// (`@`, `/`, whitespace) that would collide with the `@connection` selector or the
    /// store key encoding.
    pub fn new(id: impl Into<String>) -> Result<Self, ConnectionIdError> {
        let id = id.into();
        if id.is_empty() {
            return Err(ConnectionIdError::Empty);
        }
        if let Some(c) = id
            .chars()
            .find(|c| matches!(c, '@' | '/') || c.is_whitespace())
        {
            return Err(ConnectionIdError::ReservedChar(c));
        }
        Ok(Self(id))
    }

    /// The connection id as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl core::fmt::Display for ConnectionId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Why a [`ConnectionId`] was rejected — structured and secret-free (a connection *name* is
/// never a secret, but the error is structured for AI consumption all the same).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConnectionIdError {
    /// The connection id was empty.
    #[error("connection id must not be empty")]
    Empty,
    /// The connection id contained a reserved character that collides with the `@connection`
    /// selector or the store key encoding.
    #[error("connection id contains reserved character {0:?} (no '@', '/', or whitespace)")]
    ReservedChar(char),
}

/// The store key: a `(driver, connection)` pair naming exactly one credential. This is the
/// capability boundary — a key cannot name "any driver", so a driver scoped to its own
/// [`DriverId`] cannot fetch another driver's secret (RFD §3 capability gating).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct CredentialKey {
    /// The driver the credential belongs to, e.g. `mail`, `s3`.
    pub driver: DriverId,
    /// The named connection within that driver, e.g. `work`.
    pub connection: ConnectionId,
}

impl CredentialKey {
    /// Construct a credential key from a driver + connection.
    #[must_use]
    pub fn new(driver: DriverId, connection: ConnectionId) -> Self {
        Self { driver, connection }
    }

    /// A stable, flat string encoding `driver/connection` for backend keying (file map keys,
    /// env var suffixes). Both halves are validated to exclude `/`, so the join is
    /// unambiguous. Carries no secret material.
    #[must_use]
    pub fn flat(&self) -> String {
        format!("{}/{}", self.driver.as_str(), self.connection.as_str())
    }
}

/// **Who owns a stored connection** (t81, decision U / §3.3) — the ownership axis that makes a
/// connection *team-shared* rather than tied to one operator.
///
/// - [`OwnerScope::Me`] — a **user-owned** connection: the credential belongs to the operator who
///   added it (the pre-t81 default; every existing connection deserializes to this via
///   `#[serde(default)]`). Using it is ungated by the shared-connection actor-policy gate.
/// - [`OwnerScope::Project`] — a **project/team-owned** connection: the credential belongs to the
///   project and members USE it *as the team*, bounded by the t57 actor-policy (NOT by who holds a
///   token). The secret stays envelope-encrypted; sharing is about WHO MAY USE it, not exposing the
///   secret (§3.3). A member with no policy grant for the connection's scope **cannot** use it
///   (default-deny — [`crate::shared_use_gate`]).
///
/// This is the *connection record's* ownership label (the t44 connection record, extended). It is
/// metadata only — never a credential — so it is safe to `Debug`, serialize, and surface in
/// `qfs connection list`. The owner is **not** the policy: a project-owned connection only *selects*
/// which upstream credential an allowed effect rides; the actor's policy decides whether the effect
/// is allowed at all (§3.3 — policy gates the actor, the connection picks the credential).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OwnerScope {
    /// User-owned (the pre-t81 default): the operator who added it owns the credential.
    #[default]
    Me,
    /// Project/team-owned: members use it as the team, gated by the t57 actor-policy.
    Project,
}

impl OwnerScope {
    /// Whether this connection is **project/team-owned** (the shared, policy-gated case).
    #[must_use]
    pub fn is_project(self) -> bool {
        matches!(self, OwnerScope::Project)
    }

    /// The canonical, secret-free round-trip label (`me` / `project`) — for `/sys/connections`,
    /// `connection list`, and the audit trail. Never a credential.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            OwnerScope::Me => "me",
            OwnerScope::Project => "project",
        }
    }

    /// Parse the canonical `me` / `project` label; any other word falls back to the fail-safe
    /// default [`OwnerScope::Me`] (an unrecognized owner is treated as user-owned, NOT silently
    /// shared — sharing must be explicit).
    #[must_use]
    pub fn from_label(word: &str) -> Self {
        match word {
            "project" => OwnerScope::Project,
            _ => OwnerScope::Me,
        }
    }
}

/// A listing entry describing one stored connection — selectors + metadata only, **never**
/// the credential. Safe to `Debug`, serialize, log, and surface in `qfs connection list`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectionRecord {
    /// The driver this connection belongs to.
    pub driver: DriverId,
    /// The connection name.
    pub connection: ConnectionId,
    /// When the credential was stored (RFC 3339). Plaintext metadata for `list`/audit.
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    /// Who owns the connection (t81). Defaults to [`OwnerScope::Me`] (user-owned) so a record
    /// serialized before t81 — and every backend that does not track ownership — rehydrates to the
    /// pre-t81 behaviour. The binary's SQLite store fills this from the project DB's
    /// `shared_connection` table (a project-owned connection is one with a row there).
    #[serde(default)]
    pub owner_scope: OwnerScope,
}

impl ConnectionRecord {
    /// Construct a record. `created_at` is the caller's clock reading (the store stamps
    /// it on `put`); kept as an argument so this type performs no I/O and stays pure. The owner
    /// defaults to [`OwnerScope::Me`] (user-owned); use [`ConnectionRecord::with_owner_scope`] to
    /// mark a project/team-owned connection.
    #[must_use]
    pub fn new(driver: DriverId, connection: ConnectionId, created_at: OffsetDateTime) -> Self {
        Self {
            driver,
            connection,
            created_at,
            owner_scope: OwnerScope::Me,
        }
    }

    /// Set the [`OwnerScope`] (builder, t81) — marks a project/team-owned connection.
    #[must_use]
    pub fn with_owner_scope(mut self, owner_scope: OwnerScope) -> Self {
        self.owner_scope = owner_scope;
        self
    }

    /// Whether this connection is project/team-owned (the shared, actor-policy-gated case).
    #[must_use]
    pub fn is_shared(&self) -> bool {
        self.owner_scope.is_project()
    }

    /// The `(driver, connection)` key this record describes.
    #[must_use]
    pub fn key(&self) -> CredentialKey {
        CredentialKey::new(self.driver.clone(), self.connection.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conn(s: &str) -> ConnectionId {
        ConnectionId::new(s).unwrap()
    }

    #[test]
    fn connection_id_rejects_empty_and_reserved() {
        assert_eq!(ConnectionId::new(""), Err(ConnectionIdError::Empty));
        assert_eq!(
            ConnectionId::new("a@b"),
            Err(ConnectionIdError::ReservedChar('@'))
        );
        assert_eq!(
            ConnectionId::new("a/b"),
            Err(ConnectionIdError::ReservedChar('/'))
        );
        assert_eq!(
            ConnectionId::new("a b"),
            Err(ConnectionIdError::ReservedChar(' '))
        );
        assert_eq!(conn("work").as_str(), "work");
    }

    #[test]
    fn credential_key_flat_encoding_is_unambiguous() {
        let k = CredentialKey::new(DriverId::new("mail"), conn("work"));
        assert_eq!(k.flat(), "mail/work");
        // Both halves exclude '/', so exactly one split point exists.
        assert_eq!(k.flat().matches('/').count(), 1);
    }

    #[test]
    fn connection_record_round_trips_through_serde_without_secrets() {
        let rec = ConnectionRecord::new(
            DriverId::new("s3"),
            conn("prod"),
            OffsetDateTime::UNIX_EPOCH,
        );
        let json = serde_json::to_string(&rec).unwrap();
        // Metadata only: driver, connection, timestamp — nothing secret.
        assert!(json.contains("\"driver\""));
        assert!(json.contains("\"prod\""));
        let back: ConnectionRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(back, rec);
        assert_eq!(
            rec.key(),
            CredentialKey::new(DriverId::new("s3"), conn("prod"))
        );
    }
}
