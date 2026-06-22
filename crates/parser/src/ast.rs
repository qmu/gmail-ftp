//! The parser's AST surface (E0 subset).
//!
//! **This is the spike-grammar subset, not the full RFD §3 grammar** — E1 grows it
//! (all keywords, codecs, effect verbs, server DDL). The AST types are owned and
//! library-agnostic: no winnow type appears here, so the parser library stays
//! swappable behind [`crate::ParseError`] (fidelity guard G6).
//!
//! The full statement-level AST sum types are slated to live in `cfs-lang` in E1
//! (see `cfs-lang`'s crate docs). E0 keeps a minimal owned AST local to the parser
//! so the front door has something concrete to return; E1 promotes/relocates it.

/// A dotted path, e.g. `mail.inbox`. Raw text segments; registry resolution is E1.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Path(pub Vec<String>);

/// A literal value (E0 subset: string or integer).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Literal {
    Str(String),
    Int(i64),
}

/// Comparison operator (subset of the RFD §3 operator set).
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

/// A WHERE expression: a comparison or a left-associative `AND` chain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Expr {
    Cmp { lhs: Path, op: CmpOp, rhs: Literal },
    And(Box<Expr>, Box<Expr>),
}

/// One pipe operation following `|>` (E0 subset: WHERE / SELECT).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PipeOp {
    Where(Expr),
    Select(Vec<Path>),
}

/// A parsed statement: a `FROM` source plus a chain of `|>` operations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stmt {
    pub from: Path,
    pub ops: Vec<PipeOp>,
}
