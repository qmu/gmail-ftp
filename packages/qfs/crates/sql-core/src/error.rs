//! The structured SQL error (RFD-0001 §5: machine-readable for an AI, never prose, and **never**
//! secret-bearing). This is the **pure-leaf** home of [`SqlError`] (extracted into `qfs-sql-core`
//! so both the SQL driver (t17) and the Cloudflare D1 driver (t23) reuse it without either crate
//! depending on the other). Every arm carries a stable [`SqlError::code`] and a secret-free
//! message — never a connection string, a password, a query parameter VALUE, or the rendered SQL
//! of a value-bearing statement.
//!
//! The runtime/secrets adapters (`From<SqlError> for qfs_runtime::EffectError`,
//! `From<qfs_secrets::SecretError> for SqlError`) live in the consuming **driver** crate
//! (`qfs-driver-sql`), NOT here — so this crate stays a pure leaf over `qfs-types` and carries no
//! runtime/secrets coupling.

/// Why a SQL driver operation failed. Owned, vendor-free, secret-free data.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum SqlError {
    /// A path did not resolve to a SQL node the driver services (outside `/sql`, an empty
    /// segment, or too many segments). Structured: carries the offending path.
    #[error("path {path:?} is not a valid /sql address: {reason}")]
    InvalidPath {
        /// The offending VFS path.
        path: String,
        /// A secret-free reason.
        reason: &'static str,
    },

    /// A connection string used an unrecognised scheme, so the dialect could not be chosen.
    /// Structured: carries the scheme (a label, never the credential part of the URI).
    #[error("unknown connection scheme {scheme:?}: expected postgres / mysql / sqlite")]
    UnknownScheme {
        /// The scheme token (the part before `://`). Never the host/user/password.
        scheme: String,
    },

    /// A `/sql/<conn>` referenced a connection that is not registered. Structured: the conn key.
    #[error("unknown connection {conn:?}: no such registered /sql connection")]
    UnknownConnection {
        /// The connection key that was not found.
        conn: String,
    },

    /// A projected/filtered/ordered name is not a column of the addressed table. Structured: the
    /// offending name + a secret-free reason, so an AI can fix the query rather than hit a raw
    /// backend error.
    #[error("unknown column {name:?}: {reason}")]
    UnknownColumn {
        /// The offending column name.
        name: String,
        /// A secret-free reason (e.g. "not a column of the table").
        reason: &'static str,
    },

    /// A `/sql/<conn>/.../<table>` referenced a table/view not present in the connection catalog.
    /// Structured: the table name.
    #[error("unknown table {table:?}: no such table/view in the connection catalog")]
    UnknownTable {
        /// The offending table name.
        table: String,
    },

    /// A write verb (`INSERT`/`UPSERT`/`UPDATE`/`REMOVE`) was attempted against a **view** node.
    /// Views are SELECT-only (the parse-time capability gate rejects it first; this is the
    /// belt-and-suspenders enforcement at the apply boundary). Structured: path + verb.
    #[error("capability denied: {path:?} is a view (SELECT-only); cannot {verb}")]
    ReadOnlyView {
        /// The view path the write was attempted against.
        path: String,
        /// The denied verb's stable label (e.g. `INSERT`, `UPDATE`).
        verb: &'static str,
    },

    /// A single `COMMIT` spanned more than one `<conn>`. Single-connection = single ACID
    /// transaction is this ticket's guarantee; cross-source orchestration is a separate ticket.
    /// Structured: the two conns so an AI can split the commit.
    #[error(
        "cross-source commit: a single COMMIT spans connections {first:?} and {second:?}; \
             use an orchestrated (cross-source) commit instead"
    )]
    CrossSource {
        /// The first connection seen.
        first: String,
        /// The conflicting second connection.
        second: String,
    },

    /// A DML effect node carried a malformed payload (no rows, an unknown target column, or a
    /// shape the lowering cannot turn into a statement). Structured: a secret-free reason
    /// describing the SHAPE problem — never the row values.
    #[error("malformed effect: {reason}")]
    MalformedEffect {
        /// A secret-free reason (a shape note, never a row value).
        reason: String,
    },

    /// The backend reported an error executing a statement (a constraint violation, a connection
    /// failure, a syntax error in a path the driver did not expect). Carries the dialect/op label
    /// and a secret-free reason; **never** the rendered SQL of a value-bearing statement nor any
    /// bound parameter value.
    #[error("sql backend error ({dialect} {op}): {reason}")]
    Backend {
        /// The dialect label (`postgres`/`mysql`/`sqlite`).
        dialect: &'static str,
        /// The operation label (e.g. `introspect`, `select`, `commit`).
        op: &'static str,
        /// A secret-free reason (the backend's class note, never a parameter value).
        reason: String,
    },

    /// Fetching the connection credential from the secrets store failed, reduced to its
    /// secret-free `code`. No connection string or password crosses.
    #[error("sql credential unavailable: {code}")]
    Credential {
        /// The secret-free secrets-store error code (e.g. `secret_not_found`, `secret_locked`).
        /// (The `qfs_secrets::SecretError` → `SqlError` adapter lives in the driver crate.)
        code: &'static str,
    },
}

impl SqlError {
    /// A stable, machine-readable code for this error (AI-facing callers branch on this).
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::InvalidPath { .. } => "invalid_path",
            Self::UnknownScheme { .. } => "unknown_scheme",
            Self::UnknownConnection { .. } => "unknown_connection",
            Self::UnknownColumn { .. } => "unknown_column",
            Self::UnknownTable { .. } => "unknown_table",
            Self::ReadOnlyView { .. } => "read_only_view",
            Self::CrossSource { .. } => "cross_source",
            Self::MalformedEffect { .. } => "malformed_effect",
            Self::Backend { .. } => "backend",
            Self::Credential { .. } => "credential",
        }
    }

    /// Whether this failure class is transient (worth a runtime retry). Only a backend
    /// connection/transport failure is retryable; a constraint violation, an unknown identifier,
    /// or a capability denial is terminal. A backend reason is classified by the caller (which
    /// knows whether the failure was a transport hiccup vs a constraint), so the default here is
    /// conservative: `false` (terminal) unless explicitly constructed as retryable upstream.
    #[must_use]
    pub const fn is_retryable(&self) -> bool {
        // The driver constructs a `Backend` error with a `retryable_` op prefix when the backend
        // signalled a transient class; otherwise it is terminal. We keep the classification at
        // the EffectError boundary (below) where the runtime branches on it.
        false
    }

    /// Construct a secret-free backend error from a dialect, op, and reason note.
    #[must_use]
    pub fn backend(dialect: &'static str, op: &'static str, reason: impl Into<String>) -> Self {
        SqlError::Backend {
            dialect,
            op,
            reason: reason.into(),
        }
    }
}
