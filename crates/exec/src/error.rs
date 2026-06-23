//! [`ExecError`] — the executor's structured, owned error with a stable **`kind`** and the
//! [`ExitCode`] contract the CLI surfaces (ticket t29).
//!
//! ## The `kind` taxonomy and the t01 envelope reconciliation
//! t01 shipped the JSON error envelope `{"error":{"code","message"}}`. t29's ticket asks for
//! `{"error":{"kind","message","path?","detail?"}}`. We reconcile by making the envelope a
//! **superset**: it always carries `code` (the stable t01 field) AND `kind` (t29's
//! coarse-grained class that maps 1:1 to the exit code), plus optional `path`/`detail`. An
//! agent pinned to the t01 `code` field keeps working; a t29 agent reads `kind`. See the
//! renderer in `output.rs`.
//!
//! ## Why `kind` is coarse and `code` is fine-grained
//! `code` is the underlying error's stable identifier (e.g. `unsupported_verb`, `unknown_mount`,
//! `parse_error`) — there are many. `kind` is the small, stable bucket that drives the exit
//! code and an agent's top-level recovery branch (`parse`/`usage`/`capability`/`commit_required`/
//! `commit_failed`/`auth`/`internal`). One `kind` ⇒ one [`ExitCode`].

use cfs_core::CfsError;

/// The stable process exit-code contract (ticket t29). Pinned so AI agents can branch on it;
/// any drift fails the golden tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitCode {
    /// `0` — success (rows rendered, or a non-destructive PREVIEW shown).
    Ok = 0,
    /// `2` — parse error or CLI usage error (relative path, >1 statement source, bad flags).
    Usage = 2,
    /// `3` — capability / unsupported-op: the verb or source cannot run the requested op.
    Capability = 3,
    /// `4` — a destructive set-wide plan was previewed without `--commit`; re-run to apply.
    CommitRequired = 4,
    /// `5` — an effect/commit failure (a leg failed to apply).
    CommitFailed = 5,
    /// `6` — auth / credential failure resolving or using a secret.
    Auth = 6,
}

impl ExitCode {
    /// The integer code the process exits with.
    #[must_use]
    pub const fn code(self) -> i32 {
        self as i32
    }
}

/// The coarse error class. One variant ⇒ one [`ExitCode`] and one stable `kind` string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// A parse failure (bad pipe-SQL syntax).
    Parse,
    /// A CLI usage error (relative path, multiple statement sources, bad addressing).
    Usage,
    /// A capability / unsupported-op denial (verb not declared, source not readable).
    Capability,
    /// A destructive set-wide plan requires an explicit `--commit`.
    CommitRequired,
    /// An effect failed to apply during commit.
    CommitFailed,
    /// An auth / credential resolution or use failure.
    Auth,
    /// An internal / unexpected error (a bug or an unmodeled engine error).
    Internal,
}

impl ErrorKind {
    /// The stable `kind` string for the JSON envelope (t29).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            ErrorKind::Parse => "parse",
            ErrorKind::Usage => "usage",
            ErrorKind::Capability => "capability",
            ErrorKind::CommitRequired => "commit_required",
            ErrorKind::CommitFailed => "commit_failed",
            ErrorKind::Auth => "auth",
            ErrorKind::Internal => "internal",
        }
    }

    /// The exit code this kind maps to (the stable contract).
    #[must_use]
    pub const fn exit_code(self) -> ExitCode {
        match self {
            // A bare parse error is exit 2 (the ticket: parse error exits 2). Both parse and
            // usage share exit 2; the `kind` distinguishes them for the agent.
            ErrorKind::Parse | ErrorKind::Usage => ExitCode::Usage,
            ErrorKind::Capability => ExitCode::Capability,
            ErrorKind::CommitRequired => ExitCode::CommitRequired,
            // An internal error is reported on the effect/commit-failure code (5); it is the
            // closest "the operation did not complete" bucket without inventing a new code.
            ErrorKind::CommitFailed | ErrorKind::Internal => ExitCode::CommitFailed,
            ErrorKind::Auth => ExitCode::Auth,
        }
    }
}

/// The executor's structured error: a coarse [`ErrorKind`] (drives `kind` + exit code), the
/// underlying stable `code`, a secret-free message, and optional `path`/`detail` context.
/// Owned — no vendor types, no `CfsError` leak across the CLI seam (the renderer reads this).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecError {
    /// The coarse class.
    pub kind: ErrorKind,
    /// The underlying stable, fine-grained code (t01 `code` field).
    pub code: &'static str,
    /// A secret-free, machine-facing message.
    pub message: String,
    /// The offending path, where the error is path-scoped (usage / capability).
    pub path: Option<String>,
    /// Extra machine-facing detail (e.g. supported verbs), where available.
    pub detail: Option<String>,
}

impl ExecError {
    /// Construct an error from its parts.
    #[must_use]
    pub fn new(kind: ErrorKind, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            kind,
            code,
            message: message.into(),
            path: None,
            detail: None,
        }
    }

    /// Attach the offending path.
    #[must_use]
    pub fn with_path(mut self, path: impl Into<String>) -> Self {
        self.path = Some(path.into());
        self
    }

    /// Attach machine-facing detail.
    #[must_use]
    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }

    /// A usage error (exit 2) — relative path, multiple statement sources, bad addressing.
    #[must_use]
    pub fn usage(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Usage, "usage", message)
    }

    /// A parse error (exit 2) carrying the parser's stable code in `detail`.
    #[must_use]
    pub fn parse(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Parse, "parse_error", message)
    }

    /// The exit code this error maps to.
    #[must_use]
    pub fn exit_code(&self) -> ExitCode {
        self.kind.exit_code()
    }

    /// Map a [`CfsError`] (the workspace-wide structured error) into an [`ExecError`], choosing
    /// the coarse `kind` from the variant. The `code` is preserved verbatim (t01 stability).
    #[must_use]
    pub fn from_cfs(err: &CfsError) -> Self {
        let kind = match err {
            CfsError::UnsupportedVerb { .. } => ErrorKind::Capability,
            // An unknown mount/codec/procedure in a one-shot read is a usage/addressing class
            // problem from the operator's view; surfaced as capability so an agent treats it as
            // "this op/target is not available here" (exit 3) rather than a syntax error.
            CfsError::UnknownMount(_)
            | CfsError::UnknownCodec(_)
            | CfsError::UnknownProcedure(_) => ErrorKind::Capability,
            CfsError::Parse => ErrorKind::Parse,
            CfsError::InvalidPath { .. } => ErrorKind::Usage,
            CfsError::Decode { .. } | CfsError::Encode { .. } => ErrorKind::Internal,
            CfsError::DuplicateRegistration(_) | CfsError::NotImplemented { .. } => {
                ErrorKind::Internal
            }
            // CfsError is #[non_exhaustive]: an unmodeled future arm degrades to Internal.
            _ => ErrorKind::Internal,
        };
        let mut out = Self::new(kind, err.code(), err.to_string());
        if let CfsError::UnsupportedVerb {
            path, supported, ..
        } = err
        {
            out.path = Some(path.clone());
            out.detail = Some(format!("supported: [{}]", supported.join(", ")));
        }
        if let CfsError::InvalidPath { path, .. } = err {
            out.path = Some(path.clone());
        }
        out
    }
}

impl std::fmt::Display for ExecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for ExecError {}
