//! [`ActiveConnections`] — the persistent `{driver -> active connection}` map that backs
//! `qfs connection use` and the "persistent active" rung of the resolution ladder.
//!
//! This is **plaintext metadata**, never a secret: it records *which* connection is active
//! per driver (a selector), not any credential. It is therefore safe to serialize, log,
//! and store next to (but separate from) the encrypted credential blob. `connection use` is
//! last-writer-wins and replayable (RFD §10 idempotency/recovery).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::key::{ConnectionId, DriverId};

/// A persistent map from driver to its active connection. Owned, secret-free, serde-able.
///
/// In-memory only here (loading/saving the file lives in [`crate::LocalStore`]'s sibling
/// metadata path on native; the type itself does no I/O so it stays pure and wasm-safe).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActiveConnections {
    /// driver-id string -> active connection. A `BTreeMap` for stable serialization order.
    active: BTreeMap<String, ConnectionId>,
}

impl ActiveConnections {
    /// An empty active-connection map.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The active connection for `driver`, if one has been chosen (`connection use`).
    #[must_use]
    pub fn get(&self, driver: &DriverId) -> Option<&ConnectionId> {
        self.active.get(driver.as_str())
    }

    /// Set the active connection for `driver` (last-writer-wins; replayable).
    pub fn set(&mut self, driver: &DriverId, connection: ConnectionId) {
        self.active.insert(driver.as_str().to_string(), connection);
    }

    /// Clear the active connection for `driver` (e.g. after `connection remove` of the active
    /// one). Idempotent.
    pub fn clear(&mut self, driver: &DriverId) {
        self.active.remove(driver.as_str());
    }

    /// Whether any active connection is set.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.active.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn acct(s: &str) -> ConnectionId {
        ConnectionId::new(s).unwrap()
    }

    #[test]
    fn set_get_clear_last_writer_wins() {
        let mut a = ActiveConnections::new();
        let mail = DriverId::new("mail");
        assert!(a.get(&mail).is_none());

        a.set(&mail, acct("work"));
        assert_eq!(a.get(&mail), Some(&acct("work")));

        // Last-writer-wins.
        a.set(&mail, acct("personal"));
        assert_eq!(a.get(&mail), Some(&acct("personal")));

        a.clear(&mail);
        assert!(a.get(&mail).is_none());
        // Idempotent clear.
        a.clear(&mail);
    }

    #[test]
    fn round_trips_through_serde_as_plaintext_metadata() {
        let mut a = ActiveConnections::new();
        a.set(&DriverId::new("mail"), acct("work"));
        a.set(&DriverId::new("s3"), acct("prod"));
        let json = serde_json::to_string(&a).unwrap();
        assert!(json.contains("\"work\""));
        let back: ActiveConnections = serde_json::from_str(&json).unwrap();
        assert_eq!(back, a);
    }
}
