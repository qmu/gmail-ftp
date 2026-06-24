//! Structured, machine-readable server errors (RFD-0001 §5: AI-legible). Every variant
//! carries a stable [`ServerError::code`] and a secret-free message — `qfs serve` renders
//! these without ever printing a credential or the whole [`ServerState`](crate::ServerState).

use thiserror::Error;

/// An error from booting or hot-reconfiguring the server. `#[non_exhaustive]` so new
/// variants are additive.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ServerError {
    /// The config file could not be read (path / permissions). Carries the path and the
    /// underlying message (no secrets — a file path is not a credential).
    #[error("cannot read config `{path}`: {source}")]
    Read {
        /// The config path that failed to read.
        path: String,
        /// The underlying I/O error.
        source: std::io::Error,
    },

    /// A statement in the config file failed to parse. Line-located so boot fails fast
    /// with a precise pointer (RFD §5).
    #[error("config parse error at line {line}: {message} [{code}]")]
    Parse {
        /// The 1-based line the statement started on.
        line: usize,
        /// The parser's stable error code.
        code: String,
        /// The human-readable parse message.
        message: String,
    },

    /// A `/server/...` write used a verb the node does not support. Structured (path +
    /// verb + supported set) for AI recovery, exactly like the driver capability gate.
    #[error("unsupported verb `{verb}` at `{path}` (supported: {supported:?})")]
    UnsupportedVerb {
        /// The `/server/...` path written to.
        path: String,
        /// The rejected verb label.
        verb: String,
        /// The verbs the node *does* support.
        supported: Vec<String>,
    },

    /// A statement in the config file is not a `/server/...` write nor a CREATE-DDL sugar
    /// form — boot only applies server-config statements (RFD §8). Line-located.
    #[error(
        "line {line}: only /server writes and CREATE … DDL are valid in a server config: {detail}"
    )]
    NotServerConfig {
        /// The 1-based line the offending statement started on.
        line: usize,
        /// What was found instead (secret-free).
        detail: String,
    },

    /// Lowering a server-config statement to a [`qfs_core::Plan`] failed (e.g. an
    /// unroutable `/server` sub-path or a malformed DDL). Line-located.
    #[error("line {line}: cannot lower server-config statement: {detail}")]
    Lower {
        /// The 1-based line the offending statement started on.
        line: usize,
        /// The lowering failure detail (secret-free).
        detail: String,
    },

    /// A `COMMIT` of a server-config plan failed to apply. Carries the secret-free reason.
    #[error("line {line}: server-config commit failed: {reason}")]
    Commit {
        /// The 1-based line the offending statement started on.
        line: usize,
        /// The apply failure reason (secret-free).
        reason: String,
    },

    /// A binding's `reconcile` failed after a committed mutation.
    #[error("binding `{kind}` reconcile failed: {reason}")]
    Reconcile {
        /// The binding kind label.
        kind: String,
        /// The failure reason (secret-free).
        reason: String,
    },
}

impl ServerError {
    /// A stable, machine-readable error code (RFD §5 AI-facing contract).
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            ServerError::Read { .. } => "config_read",
            ServerError::Parse { .. } => "config_parse",
            ServerError::UnsupportedVerb { .. } => "unsupported_verb",
            ServerError::NotServerConfig { .. } => "not_server_config",
            ServerError::Lower { .. } => "config_lower",
            ServerError::Commit { .. } => "config_commit",
            ServerError::Reconcile { .. } => "binding_reconcile",
        }
    }
}
