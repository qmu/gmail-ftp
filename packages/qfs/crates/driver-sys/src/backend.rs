//! The **injected** System-DB seam (the vendor-free analogue of `qfs-driver-sql`'s
//! `SqlBackend`). The introspective driver half is pure; the impure read/write half — the
//! rusqlite I/O over the System DB (and the Project DB's connection registry) — is provided by
//! the `qfs` binary leaf through this trait, so this crate stays tokio-free and DB-free (the
//! dep-direction guard: only the terminal binary opens a real DB path, decision F).
//!
//! No vendor (rusqlite) type crosses this boundary — only owned qfs DTOs (`RowBatch`, the
//! [`SysNode`] tag, and the structured [`SysError`]).

use qfs_types::RowBatch;

use crate::schema::SysNode;

/// The read/write seam the binary implements over the System DB (decision F). The driver crate
/// holds only `Arc<dyn SysBackend>`; the concrete rusqlite implementation lives binary-side.
pub trait SysBackend: Send + Sync {
    /// Scan all rows of a `/sys/<node>` relation into the owned [`RowBatch`] shaped by
    /// [`sys_node_schema`](crate::sys_node_schema). READ side.
    ///
    /// `/sys/connections` MUST return names + metadata only — never secret material (the
    /// implementor reads the registry, not the vault; the schema has no secret column).
    ///
    /// # Errors
    /// [`SysError::Backend`] on an I/O / decode failure.
    fn scan(&self, node: SysNode) -> Result<RowBatch, SysError>;

    /// Apply a single-row `INSERT INTO /sys/policies` to the System DB **transactionally**,
    /// appending the corresponding t76 audit row in the SAME transaction (administration
    /// observes itself; a torn write can never leave a policy un-audited). Returns the affected
    /// row count (1 on success). WRITE side — the one gated mutation in this slice.
    ///
    /// # Errors
    /// [`SysError::Backend`] on an I/O failure, or [`SysError::MalformedEffect`] if the row does
    /// not carry the policy columns.
    fn insert_policy(&self, row: &RowBatch) -> Result<u64, SysError>;

    /// Apply a single-row `INSERT INTO /sys/settings` (t59) to the System DB **transactionally** —
    /// an **upsert on `key`** (a setting is single-valued, so re-setting it replaces the value) —
    /// appending the corresponding t76 audit row in the SAME transaction (administration observes
    /// itself). Returns the affected row count (1 on success). This is how the selectable safety
    /// mode is changed as a qfs statement (`INSERT INTO /sys/settings VALUES ('safety_mode', …)`).
    ///
    /// # Errors
    /// [`SysError::Backend`] on an I/O failure, or [`SysError::MalformedEffect`] if the row does
    /// not carry a non-empty `key` and `value`.
    fn set_setting(&self, row: &RowBatch) -> Result<u64, SysError>;

    /// Apply a single-row `INSERT INTO /sys/billing` (t67) to the System DB **transactionally** — an
    /// **upsert on `team_id`** (a team has one current plan, so re-recording it replaces the row) —
    /// appending the corresponding t76 audit row in the SAME transaction (administration observes
    /// itself). Returns the affected row count (1 on success). This is how a super-admin records /
    /// grants a team's billing tier as a qfs statement
    /// (`INSERT INTO /sys/billing VALUES ('team-acme', 'paid-team', 'active', …)`), the
    /// preview→commit twin of the dashboard click. NEVER a payment secret — `team_id`/`tier`/`status`
    /// metadata only.
    ///
    /// # Errors
    /// [`SysError::Backend`] on an I/O failure, or [`SysError::MalformedEffect`] if the row does
    /// not carry a non-empty `team_id`, `tier`, and `status`.
    fn set_billing(&self, row: &RowBatch) -> Result<u64, SysError>;
}

/// A structured, **secret-free** error from the `/sys` backend (RFD §5, AI-consumable). Names a
/// node/verb and a redacted detail — never a credential, never a DB path-secret.
#[derive(Debug, thiserror::Error)]
pub enum SysError {
    /// The path did not resolve to a known `/sys/<node>` relation.
    #[error("`{path}` is not a known /sys node")]
    UnknownNode {
        /// The offending path (an opaque admin path; carries no secret).
        path: String,
    },
    /// A write verb is not supported at this node (e.g. `UPDATE`/`REMOVE` on the append-only
    /// `/sys/audit`, or any write on a read-only admin view).
    #[error("{verb} is not supported on /sys/{node} (append-only / read-only)")]
    AppendOnly {
        /// The node segment (`audit`, `users`, …).
        node: &'static str,
        /// The rejected verb label.
        verb: &'static str,
    },
    /// The effect payload was malformed for the target node (missing a required column, etc.).
    #[error("malformed /sys write effect: {reason}")]
    MalformedEffect {
        /// A secret-free reason.
        reason: String,
    },
    /// An underlying System-DB I/O failure (the binary maps its rusqlite error in here as a
    /// secret-free string — a DB path is infra, never a credential).
    #[error("system db: {0}")]
    Backend(String),
}
