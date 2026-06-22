//! The owned parse error (fidelity guard G6, RFD §9 "owned DTOs / no vendor leaks").
//!
//! The chosen parser library is **winnow** (see `docs/adr/0001-parser-library.md`),
//! but no winnow type appears here or anywhere in `cfs-parser`'s public API. winnow's
//! `ParseError`/`ContextError` is mapped into this owned type at the crate boundary,
//! so E1+ can swap the parser library without breaking any caller.
//!
//! The error carries exactly the AI-critical structured-error payload of RFD §5/§10:
//! a byte span, a non-empty expected-set, and a machine-readable code, so an agent
//! can self-correct.
//!
//! ## Secret hygiene (RFD §10)
//! The error `Display` never echoes literal *values*. The `found`/`message` fields
//! describe the *kind* of token encountered (e.g. `a string literal`), not its
//! contents, so a credential-bearing statement cannot leak through a diagnostic.

use cfs_lang::Span;
use core::fmt;

/// A machine-readable parse-error code (the AI structured-error path, RFD §5).
///
/// `#[non_exhaustive]`: later epics add finer codes (e.g. capability-rejected)
/// without a breaking change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ParseErrorCode {
    /// A token was found that the grammar did not expect here.
    UnexpectedToken,
    /// Input ended before the statement was complete.
    UnexpectedEof,
    /// A keyword-shaped token is not in the closed-core frozen set (RFD §3) —
    /// e.g. lowercase, or an unknown verb. Parse-time rejection per RFD §5.
    UnknownKeyword,
    /// A reserved closed-core keyword was used where an identifier was required
    /// (e.g. as a column or alias). Targeted rejection per RFD §3 governance.
    ReservedAsIdentifier,
}

impl ParseErrorCode {
    /// The stable string form emitted on the structured-error path.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::UnexpectedToken => "UNEXPECTED_TOKEN",
            Self::UnexpectedEof => "UNEXPECTED_EOF",
            Self::UnknownKeyword => "UNKNOWN_KEYWORD",
            Self::ReservedAsIdentifier => "RESERVED_AS_IDENTIFIER",
        }
    }
}

/// An owned, library-agnostic parse error.
///
/// This is the only error type `cfs-parser` exposes. It is `Clone`/`Eq` so callers
/// (and the AI structured-error path) can compare, log, and serialise it without
/// touching any parser-library internals.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ParseError {
    /// Byte offset into the source where parsing failed.
    pub at: usize,
    /// The byte span of the offending token (empty at EOF), for diagnostics that
    /// want to underline the exact source. `span.start == at`.
    pub span: Span,
    /// Machine-readable classification.
    pub code: ParseErrorCode,
    /// What the parser expected at `at` (token-level, closed-core vocabulary). The
    /// structured-error contract (RFD §5) guarantees this is **non-empty**.
    pub expected: Vec<String>,
    /// A description of what was actually found (kind, never literal value — RFD
    /// §10 secret hygiene).
    pub found: String,
    /// Human-facing message.
    pub message: String,
}

impl ParseError {
    /// Construct an owned error. Crate-internal: only the boundary mapper calls this.
    pub(crate) fn new(
        at: usize,
        span: Span,
        code: ParseErrorCode,
        expected: Vec<String>,
        found: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            at,
            span,
            code,
            expected,
            found: found.into(),
            message: message.into(),
        }
    }
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let expected = if self.expected.is_empty() {
            "-".to_string()
        } else {
            self.expected.join(", ")
        };
        write!(
            f,
            "[{}] at byte {} | expected: {} | found: {} | {}",
            self.code.as_str(),
            self.at,
            expected,
            self.found,
            self.message
        )
    }
}

impl std::error::Error for ParseError {}
