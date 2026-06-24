//! Byte-offset spans shared by the lexer (and, in E1, the parser/AST).
//!
//! A [`Span`] is a half-open byte range `start..end` into the original source
//! string. Spans are the load-bearing primitive for diagnostics (RFD §5/§10,
//! AI-legibility): every token and every error carries one, and a span must slice
//! back to the exact originating source substring (round-trip invariant). Offsets
//! are `u32` because qfs statements are small (a single statement, not a file); a
//! `u32` byte offset is ample and keeps tokens compact.

use core::fmt;
use core::ops::Range;

/// A half-open byte range `[start, end)` into the source string.
///
/// Both bounds are byte offsets (not char indices), so a span always slices a
/// valid UTF-8 substring when the lexer places its boundaries on char boundaries —
/// which it does, since it advances char-by-char. `start <= end` always holds for
/// spans the lexer produces.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Span {
    /// Inclusive start byte offset.
    pub start: u32,
    /// Exclusive end byte offset.
    pub end: u32,
}

impl Span {
    /// Construct a span from a start and end byte offset.
    #[must_use]
    pub const fn new(start: u32, end: u32) -> Self {
        Self { start, end }
    }

    /// The span as a `usize` [`Range`], for slicing source strings directly.
    ///
    /// ```
    /// use qfs_lang::span::Span;
    /// let src = "FROM /mail";
    /// let sp = Span::new(0, 4);
    /// assert_eq!(&src[sp.range()], "FROM");
    /// ```
    #[must_use]
    pub const fn range(self) -> Range<usize> {
        self.start as usize..self.end as usize
    }

    /// The byte length of the span.
    #[must_use]
    pub const fn len(self) -> u32 {
        self.end.saturating_sub(self.start)
    }

    /// Whether the span is empty (`start == end`).
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.start >= self.end
    }
}

impl fmt::Display for Span {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}..{}", self.start, self.end)
    }
}

/// A value annotated with the [`Span`] it was lexed from.
///
/// The lexer emits `Spanned<Token>`; the E1 parser consumes them and re-spans its
/// AST nodes. `node` is the payload; `span` round-trips to the source substring.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spanned<T> {
    /// The wrapped value (a token, AST node, …).
    pub node: T,
    /// The source byte range `node` was produced from.
    pub span: Span,
}

impl<T> Spanned<T> {
    /// Wrap `node` with `span`.
    #[must_use]
    pub const fn new(node: T, span: Span) -> Self {
        Self { node, span }
    }
}
