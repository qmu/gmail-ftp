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
}

impl ConnectionRecord {
    /// Construct a record. `created_at` is the caller's clock reading (the store stamps
    /// it on `put`); kept as an argument so this type performs no I/O and stays pure.
    #[must_use]
    pub fn new(driver: DriverId, connection: ConnectionId, created_at: OffsetDateTime) -> Self {
        Self {
            driver,
            connection,
            created_at,
        }
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
