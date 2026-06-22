//! The owned parse error (fidelity guard G6, RFD §9 "owned DTOs / no vendor leaks").
//!
//! The chosen parser library is **winnow** (see `docs/adr/0001-parser-library.md`),
//! but no winnow type appears here or anywhere in `cfs-parser`'s public API. winnow's
//! `ParseError`/`ContextError` is mapped into this owned type at the crate boundary,
//! so E1+ can swap the parser library without breaking any caller.
//!
//! The error carries exactly the AI-critical structured-error payload of RFD §5: a
//! byte span, an expected-set, and a machine-readable code.

use core::fmt;

/// A machine-readable parse-error code (the AI structured-error path, RFD §5).
///
/// `#[non_exhaustive]`: E1 adds finer codes (e.g. capability-rejected) without a
/// breaking change.
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
}

impl ParseErrorCode {
    /// The stable string form emitted on the structured-error path.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::UnexpectedToken => "UNEXPECTED_TOKEN",
            Self::UnexpectedEof => "UNEXPECTED_EOF",
            Self::UnknownKeyword => "UNKNOWN_KEYWORD",
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
    /// Machine-readable classification.
    pub code: ParseErrorCode,
    /// What the parser expected at `at` (token-level, closed-core vocabulary).
    pub expected: Vec<String>,
    /// Human-facing message.
    pub message: String,
}

impl ParseError {
    /// Construct an owned error. Crate-internal: only the boundary mapper calls this.
    pub(crate) fn new(
        at: usize,
        code: ParseErrorCode,
        expected: Vec<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            at,
            code,
            expected,
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
            "[{}] at byte {} | expected: {} | {}",
            self.code.as_str(),
            self.at,
            expected,
            self.message
        )
    }
}

impl std::error::Error for ParseError {}
