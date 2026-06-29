//! The **injected** session-source seam (the vendor-free analogue of `qfs-driver-sys`'s
//! `SysBackend`). The introspective driver half is pure; the impure read/append half — reading
//! Claude Code's on-disk session state and appending a steering instruction — is provided by the
//! `qfs` binary leaf through this trait, so this crate stays tokio-free and I/O-free (the
//! dep-direction guard: only the terminal binary opens a real path; the crate is wasm-buildable
//! and the purity proof stays green).
//!
//! No vendor type and no filesystem path crosses this boundary — only owned qfs DTOs (`RowBatch`,
//! a session-id `&str`, and the structured [`ClaudeError`]). The exact session-state format
//! (Claude Code's on-disk layout vs. a local IPC/API) and how the "current" session is identified
//! are the implementor's concern (the open product decision flagged in the ticket); this seam
//! names none of it.
//!
//! ## Decision K
//! Nothing here calls or hosts an LLM. `scan_sessions` reads metadata an agent runtime already
//! wrote; `append_instruction` hands the agent a steering message. The model runs elsewhere.

use qfs_types::RowBatch;

/// The read/append seam the binary implements over the host's Claude Code session state
/// (decision K — a façade/append-log, never inference). The driver crate holds only
/// `Arc<dyn SessionSource>`; the concrete on-disk implementation lives binary-side.
pub trait SessionSource: Send + Sync {
    /// Scan the agent sessions into the owned [`RowBatch`] shaped by
    /// [`claude_node_schema`](crate::claude_node_schema)`(Sessions)`. READ side.
    ///
    /// MUST return task metadata only — never a token or a raw transcript secret (the schema has
    /// no such column). A source with no sessions returns an empty batch, not an error.
    ///
    /// # Errors
    /// [`ClaudeError::Source`] on an I/O / decode failure.
    fn scan_sessions(&self) -> Result<RowBatch, ClaudeError>;

    /// Scan a single session's instructions append-log into the owned [`RowBatch`] shaped by
    /// [`claude_node_schema`](crate::claude_node_schema)`(Instructions)`. READ side. An unknown
    /// session yields an empty batch (robust, not an error).
    ///
    /// # Errors
    /// [`ClaudeError::Source`] on an I/O / decode failure.
    fn scan_instructions(&self, session: &str) -> Result<RowBatch, ClaudeError>;

    /// Append one steering instruction to `session`'s log — the single gated WRITE in this driver
    /// (a REVERSIBLE append; steering an agent adds a message, it never removes state). Returns
    /// the affected row count (1 on success). WRITE side.
    ///
    /// The agent runtime connection this ultimately reaches is FAIL-CLOSED / opt-in: a binary with
    /// no configured session source registers no applier, so a `/claude` commit fails closed (no
    /// driver) rather than silently steering nothing.
    ///
    /// # Errors
    /// [`ClaudeError::UnknownSession`] if `session` does not exist, [`ClaudeError::MalformedEffect`]
    /// if the row carries no instruction text, or [`ClaudeError::Source`] on an I/O failure.
    fn append_instruction(&self, session: &str, row: &RowBatch) -> Result<u64, ClaudeError>;
}

/// A structured, **secret-free** error from the `/claude` session source (RFD §5, AI-consumable).
/// Names a node/session/verb and a redacted detail — never a credential, never a transcript.
#[derive(Debug, thiserror::Error)]
pub enum ClaudeError {
    /// The path did not resolve to a known `/claude/...` relation.
    #[error("`{path}` is not a known /claude node")]
    UnknownNode {
        /// The offending path (an opaque session path; carries no secret).
        path: String,
    },
    /// The addressed session does not exist on this host.
    #[error("claude session `{session}` not found")]
    UnknownSession {
        /// The session id (a label, never a secret).
        session: String,
    },
    /// A write verb is not supported at this node (e.g. any write on the read-only `/claude/sessions`
    /// relation, or `UPDATE`/`REMOVE` on the append-only instructions log).
    #[error("{verb} is not supported on /claude/{node} (read-only / append-only)")]
    Unsupported {
        /// The node family (`sessions`, `instructions`).
        node: &'static str,
        /// The rejected verb label.
        verb: &'static str,
    },
    /// The effect payload was malformed for the target node (missing the instruction column, etc.).
    #[error("malformed /claude write effect: {reason}")]
    MalformedEffect {
        /// A secret-free reason.
        reason: String,
    },
    /// An underlying session-source I/O failure (the binary maps its error in here as a secret-free
    /// string — a session-state path is infra, never a credential).
    #[error("claude session source: {0}")]
    Source(String),
}
