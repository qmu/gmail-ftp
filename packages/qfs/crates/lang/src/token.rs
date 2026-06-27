//! The lexer's output vocabulary: [`Token`] and its supporting value types.
//!
//! A token is a single classified lexical unit of the qfs surface syntax. The
//! lexer ([`crate::lex`]) turns source bytes into a flat `Vec<Spanned<Token>>`;
//! composition (e.g. `GROUP` + `BY` into a single keyword, or precedence) is the
//! parser's job (t04), not the lexer's — multi-word keywords are emitted as
//! separate adjacent tokens.
//!
//! SDK/vendor types never appear here: every payload is an owned `std` type
//! (RFD §9, no-vendor-leak), so the crate stays `wasm32`-clean (B7).

use crate::keywords::Keyword;

/// A single classified lexical token.
///
/// One variant per lexical category. Reserved UPPERCASE keywords collapse to
/// [`Token::Keyword`] (the closed-core chokepoint, RFD §3); everything else is an
/// identifier, path, literal, operator, or structural punctuation.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum Token {
    // -- closed-core keywords (frozen; RFD §3) --
    /// A reserved UPPERCASE keyword from the frozen [`Keyword`] set.
    Keyword(Keyword),

    // -- operators --
    /// `|>` — the pipe operator.
    Pipe,
    /// `=` — assignment / binding only (RFD decision O, ticket t70). Binds names
    /// in `LET x = …`, `EXTEND col = …`, `SET col = …`, `UPDATE … SET …`. It is
    /// **never** equivalence; comparison is the explicit [`Token::EqEq`] (`==`).
    Eq,
    /// `==` — equality comparison (RFD decision O, ticket t70). Distinct from the
    /// binding [`Token::Eq`] (`=`): in qfs, unlike SQL, a single `=` never compares.
    EqEq,
    /// `<>` — inequality.
    Ne,
    /// `<` — less-than.
    Lt,
    /// `>` — greater-than.
    Gt,
    /// `<=` — less-than-or-equal.
    Le,
    /// `>=` — greater-than-or-equal.
    Ge,
    /// `~` — regex/match.
    Tilde,

    // -- structural punctuation --
    /// `(`
    LParen,
    /// `)`
    RParen,
    /// `{` — opens a `TRANSACTION { … }` block (M6, ticket t62).
    LBrace,
    /// `}` — closes a `TRANSACTION { … }` block (M6, ticket t62).
    RBrace,
    /// `;` — separates effect statements inside a `TRANSACTION` block (M6, ticket t62). It is
    /// **not** a general statement terminator (the program model is `;`-free, RFD §1.2); it is
    /// structural punctuation that only the transaction grammar consumes.
    Semicolon,
    /// `,`
    Comma,
    /// `:` — the type-annotation separator in a lambda parameter list
    /// (`(addr: string) => …`, M6 ticket t61). Structural punctuation only — NOT a
    /// frozen operator/keyword (the closed-core freeze is untouched), so it adds zero
    /// vocabulary; the parser consumes it solely inside a lambda's parameter list.
    Colon,
    /// `.`
    Dot,
    /// `*` — star (projection / glob in expression position).
    Star,
    /// `=>` — named-argument arrow (e.g. `method=>'squash'`).
    Arrow,

    // -- names & paths --
    /// A bare identifier `[A-Za-z_][A-Za-z0-9_]*` that is not a reserved keyword.
    Ident(String),
    /// A `/driver/seg/seg` path with optional `@version` and glob flags per
    /// segment. Raw segment text only — no driver validation here (RFD §5).
    Path(Vec<PathSeg>),

    // -- literals --
    /// A single-quoted string literal, with escapes already resolved.
    Str(String),
    /// An integer literal.
    Int(i64),
    /// A floating-point literal.
    Float(f64),
    /// A boolean literal (`TRUE` / `FALSE`).
    Bool(bool),
    /// The null literal (`NULL`).
    Null,
    /// A size literal such as `25 MB`.
    Size {
        /// The numeric magnitude.
        value: u64,
        /// The size unit.
        unit: SizeUnit,
    },
    /// A typed literal such as `DATE '2026-01-01'`. `raw` is the unvalidated
    /// inner string content (parser/runtime validates, RFD §5).
    TypedLit {
        /// Which typed-literal keyword introduced it.
        ty: LitType,
        /// The raw inner string content (escapes resolved, contents unchecked).
        raw: String,
    },
}

/// A size unit for a [`Token::Size`] literal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SizeUnit {
    /// bytes
    B,
    /// kilobytes
    KB,
    /// megabytes
    MB,
    /// gigabytes
    GB,
    /// terabytes
    TB,
}

impl SizeUnit {
    /// Classify an uppercase unit word into a [`SizeUnit`], if it is one.
    #[must_use]
    pub fn from_word(word: &str) -> Option<Self> {
        match word {
            "B" => Some(Self::B),
            "KB" => Some(Self::KB),
            "MB" => Some(Self::MB),
            "GB" => Some(Self::GB),
            "TB" => Some(Self::TB),
            _ => None,
        }
    }

    /// The canonical surface text of the unit.
    #[must_use]
    pub const fn text(self) -> &'static str {
        match self {
            Self::B => "B",
            Self::KB => "KB",
            Self::MB => "MB",
            Self::GB => "GB",
            Self::TB => "TB",
        }
    }
}

/// Which typed-literal keyword introduced a [`Token::TypedLit`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LitType {
    /// `DATE '…'`
    Date,
    /// `TIME '…'`
    Time,
    /// `TIMESTAMP '…'`
    Timestamp,
}

impl LitType {
    /// Classify an uppercase word into a typed-literal introducer, if it is one.
    #[must_use]
    pub fn from_word(word: &str) -> Option<Self> {
        match word {
            "DATE" => Some(Self::Date),
            "TIME" => Some(Self::Time),
            "TIMESTAMP" => Some(Self::Timestamp),
            _ => None,
        }
    }

    /// The canonical introducer keyword text.
    #[must_use]
    pub const fn text(self) -> &'static str {
        match self {
            Self::Date => "DATE",
            Self::Time => "TIME",
            Self::Timestamp => "TIMESTAMP",
        }
    }
}

/// One segment of a [`Token::Path`].
///
/// `name` is the raw segment text (no validation); `version` holds the raw
/// `@version` ref text if present (a git ref, S3 versionId, drive rev — RFD §4),
/// preserved verbatim; `glob` flags that the segment contained a glob char (`*`
/// or `?`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathSeg {
    /// Raw segment name text.
    pub name: String,
    /// Raw `@version` ref text, if the segment carried one.
    pub version: Option<String>,
    /// Whether the segment contains a glob character.
    pub glob: bool,
}

impl PathSeg {
    /// Construct a path segment.
    #[must_use]
    pub fn new(name: impl Into<String>, version: Option<String>, glob: bool) -> Self {
        Self {
            name: name.into(),
            version,
            glob,
        }
    }
}

/// Reserved boolean/null word classification used by the identifier lexer.
///
/// `TRUE`/`FALSE`/`NULL` are literal words, not closed-core [`Keyword`]s, so they
/// are recognized here rather than via the keyword table.
pub(crate) fn literal_word(word: &str) -> Option<Token> {
    match word {
        "TRUE" => Some(Token::Bool(true)),
        "FALSE" => Some(Token::Bool(false)),
        "NULL" => Some(Token::Null),
        _ => None,
    }
}
