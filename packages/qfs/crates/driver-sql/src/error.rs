//! The SQL driver's error surface (RFD-0001 §5). The structured, secret-free [`SqlError`] enum
//! itself now lives in the pure-leaf [`qfs_sql_core`] crate (extracted so both this driver (t17)
//! and the Cloudflare D1 driver (t23) reuse one emitter + error without depending on each other —
//! see the `qfs-sql-core` crate docs). This module **re-exports** it and owns the two adapters
//! that couple it to the runtime + secrets surfaces — kept HERE (in the driver, a runtime leaf).
//!
//! Because all three of `SqlError`, [`EffectError`](qfs_runtime::EffectError), and
//! [`SecretError`](qfs_secrets::SecretError) are now foreign to this crate, the adapters are
//! explicit free functions (a `From` impl would violate the orphan rule). The call sites use
//! them via `.map_err(...)` instead of the `?`-driven `From`.

use qfs_runtime::EffectError;
use qfs_secrets::SecretError;

pub use qfs_sql_core::SqlError;

/// Map a secrets-store failure into the secret-free [`SqlError`], preserving only its stable
/// `code`. No connection string or password crosses.
#[must_use]
pub fn credential_error(err: SecretError) -> SqlError {
    SqlError::Credential { code: err.code() }
}

/// Reduce a SQL failure into the runtime's structured per-effect error so the interpreter's
/// retry/ledger logic can branch on its class (RFD §5/§6). A view-write denial maps to
/// [`EffectError::CapabilityDenied`]; everything else is terminal (a constraint violation must
/// not be blindly retried — the audit ledger records it). Every message is secret-free.
#[must_use]
pub fn sql_error_to_effect_error(err: SqlError) -> EffectError {
    match err {
        SqlError::ReadOnlyView { path, verb } => EffectError::CapabilityDenied {
            driver: qfs_types::DriverId::new("sql"),
            verb: format!("{verb} at {path:?}"),
        },
        other => EffectError::terminal(other.to_string()),
    }
}
