//! Shared minimal AST for the t02 parser spike.
//!
//! **THROWAWAY — NOT PRODUCTION.** See `docs/adr/0001-parser-library.md`.
//!
//! Both the winnow spike and the chumsky spike parse into *this exact* type, so the
//! comparison is apples-to-apples and a cross-parser equality test is meaningful.
//! This is a deliberate subset of the RFD §3 grammar:
//! `FROM <path> |> WHERE <expr> |> SELECT <cols>`.

/// A dotted/segmented path, e.g. `mail.inbox` or `from`. Stored as raw text segments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Path(pub Vec<String>);

/// A literal value on the RHS of a comparison.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Literal {
    /// A single-quoted string literal (quotes stripped).
    Str(String),
    /// An integer literal.
    Int(i64),
}

/// Comparison operators — a subset of the RFD §3 operator set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CmpOp {
    Eq,
    Ne,
    Lt,
    Gt,
    Le,
    Ge,
    Like,
}

/// A WHERE expression: a comparison, or a left-associative `AND` chain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Expr {
    Cmp { lhs: Path, op: CmpOp, rhs: Literal },
    And(Box<Expr>, Box<Expr>),
}

/// One pipe operation following `|>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PipeOp {
    Where(Expr),
    Select(Vec<Path>),
}

/// A full spike statement: a `FROM` source plus a chain of `|>` operations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpikeStmt {
    pub from: Path,
    pub ops: Vec<PipeOp>,
}

/// A spike error, carrying the evidence the t02 criteria weigh: a byte span, a
/// human message, an expected-set, and a machine-readable code. Both parsers map
/// their native error into this so the golden corpus compares like-for-like.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpikeError {
    /// Byte offset where parsing failed (best effort).
    pub at: usize,
    /// Machine-readable error code for the AI structured-error path.
    pub code: SpikeErrorCode,
    /// What the parser expected at `at`.
    pub expected: Vec<String>,
    /// Human-facing message.
    pub message: String,
}

/// Machine-readable error code (the AI-critical structured-error path, RFD §5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpikeErrorCode {
    /// A required keyword/token was missing or mismatched.
    UnexpectedToken,
    /// Input ended before the grammar was complete.
    UnexpectedEof,
    /// A keyword was not in the closed-core set (e.g. lowercase or unknown).
    UnknownKeyword,
}

impl SpikeErrorCode {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::UnexpectedToken => "UNEXPECTED_TOKEN",
            Self::UnexpectedEof => "UNEXPECTED_EOF",
            Self::UnknownKeyword => "UNKNOWN_KEYWORD",
        }
    }
}

impl SpikeError {
    /// Render the error in a stable, snapshot-friendly single line: this is what the
    /// golden corpus records, so error *machineability* is visible (code + span +
    /// expected-set), not just human prose.
    #[must_use]
    pub fn render(&self) -> String {
        let expected = if self.expected.is_empty() {
            "-".to_string()
        } else {
            self.expected.join(", ")
        };
        format!(
            "[{}] at byte {} | expected: {} | {}",
            self.code.as_str(),
            self.at,
            expected,
            self.message
        )
    }
}
