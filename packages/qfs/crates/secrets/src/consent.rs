//! t54 (roadmap **M4** — the "Cloud tier"): the **pure consent / sign-in decision** that makes a
//! cloud driver unusable until a human has signed in to qfs identity AND granted that connection
//! explicit consent.
//!
//! This module is the *decision*, not the *I/O*. It holds two pure, secret-free pieces the binary's
//! composition root (`crates/qfs/src/connection.rs` + `crates/qfs/src/commit.rs`) wires the real
//! identity / consent state into:
//!
//! - [`is_cloud_driver`] — a static classification: a driver that talks to an external service over
//!   OAuth (gmail, gdrive, ga, github, slack, objstore, cf) is a **cloud** driver and sign-in is
//!   mandatory for it; everything else (the local filesystem, an on-disk git repo, the embedded SQL
//!   store, the `/sys` administration driver) is **local** and ungated.
//! - [`bind_gate`] — given "is the operator signed in?" and "is consent recorded for this
//!   connection?", returns `Ok(())` to proceed or a structured, secret-free [`ConsentError`] to
//!   refuse. The load-bearing M4 rule lives here: a cloud connection **fails closed** for an
//!   unauthenticated operator (decision B/C), and refuses to bind until consent was recorded
//!   (decision E).
//!
//! ## Why pure
//! The §3 purity invariant: a `Plan` carries a connection *selector*, never a secret/token — consent
//! and sign-in happen at **bind/commit time**, downstream of `describe`/`preview`. Keeping the
//! decision pure (no DB, no network, no `Secret`) lets the binary unit-test the gate hermetically and
//! lets the same predicate run on both the native add/use path and the commit-time bind. It also
//! keeps qfs-secrets wasm-buildable: nothing here touches the (native-only) envelope or any I/O.

use crate::key::DriverId;

/// The drivers for which sign-in is **mandatory** — every driver that reaches an external service
/// over OAuth (the roadmap's "Cloud tier"). Frozen, lowercase, and matched case-sensitively against
/// the canonical [`DriverId`] (driver ids are already lowercase by construction).
///
/// Membership is the single source of truth for [`is_cloud_driver`]; a driver NOT listed here is a
/// **local** driver (e.g. `local`, `git`, `sql`, `sys`, `http`) for which `connection add`/`use` and
/// a commit-time bind need no identity and no recorded consent.
pub const CLOUD_DRIVERS: &[&str] = &["gmail", "gdrive", "ga", "github", "slack", "objstore", "cf"];

/// Is `driver` a **cloud** driver (one that talks to an external service over OAuth), for which
/// sign-in is mandatory and consent must be recorded before it can bind a credential? Pure; the
/// classification is the static [`CLOUD_DRIVERS`] set.
#[must_use]
pub fn is_cloud_driver(driver: &DriverId) -> bool {
    CLOUD_DRIVERS.contains(&driver.as_str())
}

/// Why a cloud-driver bind/consent was refused — structured and **secret-free** (a driver name and a
/// connection name are metadata, never a credential). AI-actionable: the message names exactly what
/// the operator must do (sign in, or grant consent) to proceed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConsentError {
    /// The driver is a cloud driver but no operator is signed in to qfs identity. Fail closed
    /// (decision B/C): cloud connections are unusable for an unauthenticated operator. Actionable:
    /// `qfs identity signup <email>` (then add the connection).
    #[error(
        "cloud driver '{driver}' requires sign-in — run `qfs identity signup <email>` first \
         (cloud connections are unusable for an unauthenticated operator)"
    )]
    SignInRequired {
        /// The cloud driver that was refused (metadata, never a secret).
        driver: String,
    },
    /// The operator is signed in, but no consent has been recorded for this `(driver, connection)`.
    /// Actionable: `qfs connection add <driver> <connection>` to grant consent (and provision the
    /// token) before the driver can bind.
    #[error(
        "cloud driver '{driver}' has no recorded consent for connection '{connection}' — run \
         `qfs connection add {driver} {connection}` to grant access before using it"
    )]
    ConsentRequired {
        /// The cloud driver that was refused (metadata, never a secret).
        driver: String,
        /// The connection name that lacks consent (metadata, never a secret).
        connection: String,
    },
}

impl ConsentError {
    /// A short, stable error code for structured/JSON surfaces and AI feedback (mirrors
    /// [`crate::SecretError::code`] / [`crate::ScopeError::code`]).
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            ConsentError::SignInRequired { .. } => "cloud_sign_in_required",
            ConsentError::ConsentRequired { .. } => "cloud_consent_required",
        }
    }
}

/// The bind-time consent gate: decide whether a credential for `driver`/`connection` may bind.
///
/// - A **local** driver is never gated — `Ok(())` regardless of `signed_in`/`has_consent` (the
///   capability is reachable without force; the M4 rule is about cloud drivers only).
/// - A **cloud** driver **fails closed**: it requires an authenticated operator
///   ([`ConsentError::SignInRequired`] when `!signed_in`) AND a previously recorded consent for the
///   exact connection ([`ConsentError::ConsentRequired`] when `signed_in` but `!has_consent`).
///
/// Pure and secret-free: it reasons over two booleans the binary derives from the System-DB identity
/// state and the Project-DB consent ledger; it never sees a token. `connection` is carried only to
/// name it in the [`ConsentError::ConsentRequired`] message.
///
/// # Errors
/// [`ConsentError`] when a cloud driver lacks sign-in or recorded consent.
pub fn bind_gate(
    driver: &DriverId,
    connection: &str,
    signed_in: bool,
    has_consent: bool,
) -> Result<(), ConsentError> {
    if !is_cloud_driver(driver) {
        // Local driver: ungated. Sign-in and consent are a cloud-tier concern only.
        return Ok(());
    }
    if !signed_in {
        return Err(ConsentError::SignInRequired {
            driver: driver.as_str().to_string(),
        });
    }
    if !has_consent {
        return Err(ConsentError::ConsentRequired {
            driver: driver.as_str().to_string(),
            connection: connection.to_string(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drv(id: &str) -> DriverId {
        DriverId::new(id)
    }

    #[test]
    fn cloud_drivers_are_classified_and_local_drivers_are_not() {
        for cloud in CLOUD_DRIVERS {
            assert!(is_cloud_driver(&drv(cloud)), "{cloud} should be cloud");
        }
        // The local / on-host drivers are NOT cloud — they need no sign-in or consent.
        for local in ["local", "git", "sql", "sys", "http"] {
            assert!(!is_cloud_driver(&drv(local)), "{local} should be local");
        }
    }

    #[test]
    fn a_local_driver_is_never_gated() {
        // Local drivers proceed regardless of sign-in / consent state.
        assert!(bind_gate(&drv("local"), "default", false, false).is_ok());
        assert!(bind_gate(&drv("git"), "repo", false, false).is_ok());
    }

    #[test]
    fn a_cloud_driver_without_sign_in_is_refused_closed() {
        let err = bind_gate(&drv("github"), "work", false, false).unwrap_err();
        assert_eq!(err.code(), "cloud_sign_in_required");
        // A signed-in requirement takes precedence over the consent requirement.
        let still = bind_gate(&drv("github"), "work", false, true).unwrap_err();
        assert_eq!(still.code(), "cloud_sign_in_required");
        // Secret-free + actionable: names the driver and the remedy, no token.
        assert!(err.to_string().contains("github"));
        assert!(err.to_string().contains("identity signup"));
    }

    #[test]
    fn a_signed_in_cloud_driver_without_consent_is_refused() {
        let err = bind_gate(&drv("gmail"), "work", true, false).unwrap_err();
        assert_eq!(err.code(), "cloud_consent_required");
        // Names the exact connection that lacks consent + the remedy.
        assert!(err.to_string().contains("gmail"));
        assert!(err.to_string().contains("work"));
        assert!(err.to_string().contains("connection add"));
    }

    #[test]
    fn a_signed_in_consented_cloud_driver_proceeds() {
        assert!(bind_gate(&drv("gmail"), "work", true, true).is_ok());
        assert!(bind_gate(&drv("slack"), "team", true, true).is_ok());
    }
}
