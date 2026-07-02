//! The connection-resolution ladder (RFD-0001 §10): turn a statement's context into a
//! concrete [`ConnectionId`] for a driver, recording *why* (the [`ConnectionSource`]) so the
//! decision can be logged to the audit ledger ("who ran as whom") without ever touching
//! a credential.
//!
//! ## Precedence (highest wins)
//! 1. **`--connection` flag** — an explicit CLI override.
//! 2. **`AT 'acct'` clause** — the statement-level selector (its AST node comes from the
//!    grammar ticket t04; here we consume the resolved [`ConnectionId`]).
//! 3. **the mount's account** — the account the resolved mount binds (ADR 0008 §4: the mount
//!    carries the account; there is NO process-global selection state).
//! 4. **sole connection** — if the driver has exactly one configured connection, use it.
//! 5. **error** — zero connections → [`ResolveError::NoneConfigured`]; more than one with no
//!    selector → [`ResolveError::Ambiguous`] listing the candidates (never a silent pick).
//!
//! "Sole connection" means *sole for that driver*, scoped via the `available` slice the
//! caller filters to the driver — not globally. Resolution is pure (no I/O); the caller
//! reads `available` from the store once at COMMIT time and passes it in.

use crate::key::{ConnectionId, ConnectionRecord, DriverId};

/// Which rung of the ladder chose the connection — recorded for the audit ledger and for
/// AI-readable "who ran as whom". Secret-free.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionSource {
    /// The `--connection` flag.
    Flag,
    /// The `AT 'acct'` clause.
    AtClause,
    /// The resolved mount's bound account (ADR 0008 — the mount carries the account).
    Mount,
    /// The driver's sole configured connection.
    Sole,
}

impl ConnectionSource {
    /// A short, stable label for logs / audit / `-json`.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            ConnectionSource::Flag => "flag",
            ConnectionSource::AtClause => "at_clause",
            ConnectionSource::Mount => "mount",
            ConnectionSource::Sole => "sole",
        }
    }
}

/// A resolved connection decision: the chosen connection + the rung that chose it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolution {
    /// The connection to bind the driver leg to.
    pub connection: ConnectionId,
    /// Why it was chosen.
    pub source: ConnectionSource,
}

/// Resolution failed — structured and secret-free, listing candidates so the AI/user can
/// pick (RFD §10: never a silent pick).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ResolveError {
    /// No connection is configured for this driver — authorize one with `qfs account add`
    /// (cloud) or declare it with `qfs connect` / `CREATE CONNECTION` (local).
    #[error("no connection configured for driver {}", .driver.as_str())]
    NoneConfigured {
        /// The driver that has no connections.
        driver: DriverId,
    },
    /// More than one connection exists and none was selected. The candidate list is the
    /// actionable recovery: pass `--connection` / `AT`, or reconnect the mount with an account.
    #[error(
        "ambiguous connection for driver {}: candidates {} — pass --connection or AT 'acct'",
        .driver.as_str(),
        .candidates.iter().map(ConnectionId::as_str).collect::<Vec<_>>().join(", ")
    )]
    Ambiguous {
        /// The driver with the ambiguity.
        driver: DriverId,
        /// The candidate connections to choose among.
        candidates: Vec<ConnectionId>,
    },
    /// An explicit selector (flag / AT / mount) named an connection that is not configured
    /// for the driver — a typo or a removed connection. Lists the available candidates.
    #[error(
        "selected connection {} not configured for driver {} (available: {})",
        .selected.as_str(),
        .driver.as_str(),
        .candidates.iter().map(ConnectionId::as_str).collect::<Vec<_>>().join(", ")
    )]
    UnknownSelection {
        /// The driver.
        driver: DriverId,
        /// The connection that was selected but does not exist.
        selected: ConnectionId,
        /// The connections that do exist.
        candidates: Vec<ConnectionId>,
    },
}

impl ResolveError {
    /// A short, stable error code for structured surfaces / AI feedback.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            ResolveError::NoneConfigured { .. } => "connection_none_configured",
            ResolveError::Ambiguous { .. } => "connection_ambiguous",
            ResolveError::UnknownSelection { .. } => "connection_unknown_selection",
        }
    }
}

/// Resolve the connection to use for `driver`, following the precedence ladder.
///
/// - `flag` is the `--connection` override; `at_clause` is the `AT 'acct'` selector.
/// - `mount` is the resolved mount's bound account (ADR 0008 — the mount carries the account).
/// - `available` is the full connection list **already filtered to this driver** by the
///   caller (a driver only sees its own connections — the capability boundary).
///
/// An explicit selector (flag > at > mount) is honoured only if it names a configured
/// connection; otherwise [`ResolveError::UnknownSelection`] fires (no silent fallthrough to
/// "sole", which would mask a typo). With no selector: the sole connection wins, zero is
/// [`ResolveError::NoneConfigured`], and many is [`ResolveError::Ambiguous`].
///
/// # Errors
/// [`ResolveError`] per the rules above.
pub fn resolve(
    driver: &DriverId,
    flag: Option<&ConnectionId>,
    at_clause: Option<&ConnectionId>,
    mount: Option<&ConnectionId>,
    available: &[ConnectionRecord],
) -> Result<Resolution, ResolveError> {
    let candidates: Vec<ConnectionId> = available
        .iter()
        .filter(|r| &r.driver == driver)
        .map(|r| r.connection.clone())
        .collect();

    let exists = |a: &ConnectionId| candidates.iter().any(|c| c == a);

    // An explicit selector, in precedence order. Each must name a configured connection.
    let selector: Option<(ConnectionId, ConnectionSource)> = flag
        .map(|a| (a.clone(), ConnectionSource::Flag))
        .or_else(|| at_clause.map(|a| (a.clone(), ConnectionSource::AtClause)))
        .or_else(|| mount.map(|a| (a.clone(), ConnectionSource::Mount)));

    if let Some((connection, source)) = selector {
        if exists(&connection) {
            return Ok(Resolution { connection, source });
        }
        return Err(ResolveError::UnknownSelection {
            driver: driver.clone(),
            selected: connection,
            candidates,
        });
    }

    // No selector: fall to the sole connection, else structured ambiguity / emptiness.
    // We move the single element out by value (no unwrap/expect: the slice-pattern
    // binds it directly, satisfying the no-panic lib policy).
    match candidates.as_slice() {
        [] => Err(ResolveError::NoneConfigured {
            driver: driver.clone(),
        }),
        [_only] => {
            let mut candidates = candidates;
            let connection = candidates.remove(0);
            Ok(Resolution {
                connection,
                source: ConnectionSource::Sole,
            })
        }
        _ => Err(ResolveError::Ambiguous {
            driver: driver.clone(),
            candidates,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::OffsetDateTime;

    fn acct(s: &str) -> ConnectionId {
        ConnectionId::new(s).unwrap()
    }

    fn rec(driver: &str, connection: &str) -> ConnectionRecord {
        ConnectionRecord::new(
            DriverId::new(driver),
            acct(connection),
            OffsetDateTime::UNIX_EPOCH,
        )
    }

    /// Precedence: flag wins over AT clause wins over the mount's account wins over sole.
    #[test]
    fn precedence_ladder_full_order() {
        let mail = DriverId::new("mail");
        let available = vec![rec("mail", "work"), rec("mail", "personal")];
        let mount = acct("personal");

        // Flag beats everything.
        let r = resolve(
            &mail,
            Some(&acct("work")),
            Some(&acct("personal")),
            Some(&mount),
            &available,
        )
        .unwrap();
        assert_eq!(r.connection, acct("work"));
        assert_eq!(r.source, ConnectionSource::Flag);

        // No flag: AT clause beats the mount's account.
        let r = resolve(&mail, None, Some(&acct("work")), Some(&mount), &available).unwrap();
        assert_eq!(r.connection, acct("work"));
        assert_eq!(r.source, ConnectionSource::AtClause);

        // No flag/AT: the mount's account wins.
        let r = resolve(&mail, None, None, Some(&mount), &available).unwrap();
        assert_eq!(r.connection, acct("personal"));
        assert_eq!(r.source, ConnectionSource::Mount);
    }

    /// Sole connection: exactly one configured connection, no selector -> Sole.
    #[test]
    fn sole_connection_for_driver_only() {
        let s3 = DriverId::new("s3");
        // Two drivers configured globally, but s3 has a SOLE connection -> sole, not ambiguous.
        let available = vec![
            rec("s3", "prod"),
            rec("mail", "work"),
            rec("mail", "personal"),
        ];
        let r = resolve(&s3, None, None, None, &available).unwrap();
        assert_eq!(r.connection, acct("prod"));
        assert_eq!(r.source, ConnectionSource::Sole);
    }

    /// Zero connections for the driver -> NoneConfigured.
    #[test]
    fn zero_connections_is_none_configured() {
        let mail = DriverId::new("mail");
        let available = vec![rec("s3", "prod")];
        let err = resolve(&mail, None, None, None, &available).unwrap_err();
        assert_eq!(err.code(), "connection_none_configured");
        assert!(matches!(err, ResolveError::NoneConfigured { .. }));
    }

    /// Many connections, no selector -> Ambiguous listing the candidates.
    #[test]
    fn many_connections_no_selector_is_ambiguous() {
        let mail = DriverId::new("mail");
        let available = vec![rec("mail", "work"), rec("mail", "personal")];
        let err = resolve(&mail, None, None, None, &available).unwrap_err();
        assert_eq!(err.code(), "connection_ambiguous");
        match err {
            ResolveError::Ambiguous { candidates, .. } => {
                assert_eq!(candidates.len(), 2);
                assert!(candidates.contains(&acct("work")));
                assert!(candidates.contains(&acct("personal")));
            }
            other => panic!("expected Ambiguous, got {other:?}"),
        }
    }

    /// An explicit selector that names a non-existent connection -> UnknownSelection (never a
    /// silent fall-through to sole, which would mask a typo).
    #[test]
    fn selector_for_unknown_connection_is_rejected() {
        let mail = DriverId::new("mail");
        let available = vec![rec("mail", "work")];
        let err = resolve(&mail, Some(&acct("typo")), None, None, &available).unwrap_err();
        assert_eq!(err.code(), "connection_unknown_selection");
        match err {
            ResolveError::UnknownSelection {
                selected,
                candidates,
                ..
            } => {
                assert_eq!(selected, acct("typo"));
                assert_eq!(candidates, vec![acct("work")]);
            }
            other => panic!("expected UnknownSelection, got {other:?}"),
        }
    }

    /// ConnectionSource labels are stable for the audit ledger.
    #[test]
    fn connection_source_labels_are_stable() {
        assert_eq!(ConnectionSource::Flag.label(), "flag");
        assert_eq!(ConnectionSource::AtClause.label(), "at_clause");
        assert_eq!(ConnectionSource::Mount.label(), "mount");
        assert_eq!(ConnectionSource::Sole.label(), "sole");
    }
}
