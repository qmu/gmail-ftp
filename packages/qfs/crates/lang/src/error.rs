//! The lexer error type, carrying a [`Span`] for the structured-error path.
//!
//! Every lexer failure is reported as a [`LexError`] with the exact byte span of
//! the offending source (RFD §5/§10): an unterminated string, a bad escape, a
//! stray character, or a malformed number. This mirrors the parser's owned
//! `ParseError` shape so the two diagnostic surfaces compose. The lexer never
//! panics; arbitrary input yields `Ok` or `Err`, never an abort.

use crate::span::Span;
use core::fmt;

/// The machine-readable classification of a lexer error.
///
/// `#[non_exhaustive]`: later epics may add finer kinds without a breaking change.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum LexErrorKind {
    /// A string literal was opened with `'` but never closed before end-of-input.
    UnterminatedString,
    /// A `\` escape inside a string was followed by an unsupported escape char.
    BadEscape,
    /// A character that cannot begin any token appeared at this position.
    UnexpectedChar(char),
    /// A numeric literal was malformed (e.g. overflow, multiple dots).
    BadNumber,
    /// A hex bytes literal `X'…'` contained a non-hex digit or an odd number of digits.
    BadHexBytes,
}

impl LexErrorKind {
    /// The stable string form for the structured-error path.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::UnterminatedString => "UNTERMINATED_STRING",
            Self::BadEscape => "BAD_ESCAPE",
            Self::UnexpectedChar(_) => "UNEXPECTED_CHAR",
            Self::BadNumber => "BAD_NUMBER",
            Self::BadHexBytes => "BAD_HEX_BYTES",
        }
    }
}

/// An owned lexer error with the byte span of the offending source.
///
/// `Clone`/`Eq` so callers (and the AI structured-error path) can compare and log
/// it. The span round-trips to the source substring that triggered the error.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct LexError {
    /// The source byte range that triggered the error.
    pub span: Span,
    /// The machine-readable classification.
    pub kind: LexErrorKind,
}

impl LexError {
    /// Construct a lexer error from a span and kind.
    #[must_use]
    pub const fn new(span: Span, kind: LexErrorKind) -> Self {
        Self { span, kind }
    }
}

impl fmt::Display for LexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.kind {
            LexErrorKind::UnexpectedChar(c) => write!(
                f,
                "[{}] at {} | unexpected character `{}`",
                self.kind.as_str(),
                self.span,
                c
            ),
            other => write!(f, "[{}] at {}", other.as_str(), self.span),
        }
    }
}

impl std::error::Error for LexError {}
