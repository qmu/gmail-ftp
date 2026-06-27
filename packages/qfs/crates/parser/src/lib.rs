//! `qfs-parser` — the parser front door (RFD-0001 §2.2, §3, §9).
//!
//! t04 promotes the E0/t02 skeleton to the **full RFD §3 pipe-SQL grammar**: the
//! parser library is **winnow** (locked in `docs/adr/0001-parser-library.md`), now
//! driven over the **t03 token stream** (`qfs_lang::lex` → `Vec<Spanned<Token>>`)
//! rather than over raw text. It parses the closed-core statement grammar — queries
//! (`FROM … |> op …`), effects (`INSERT/UPSERT/UPDATE/REMOVE … RETURNING`), codecs
//! (`DECODE/ENCODE`), `CALL`, server DDL (`CREATE …`), and the `PREVIEW/COMMIT`
//! plan wrappers — into the owned [`ast`] sum types.
//!
//! ## Closed core + three open registries (RFD §3 governance)
//! The grammar is **closed**: [`ast::Statement`] and [`ast::PipeOp`] carry no
//! per-driver/per-action variant (a governance test locks their variant count), so a
//! driver can never add an AST node. The only extension points are the three
//! **string-named** registry seams the grammar already has slots for: paths/mounts
//! ([`ast::PathExpr`]), functions & procedures ([`ast::FnRef`]/[`ast::CallRef`]), and
//! codecs ([`ast::Codec`]). Names inside those seams are parsed but **not resolved**
//! — resolution / capability gating is the semantic phase (E2). Unknown core
//! constructs (lowercase keywords, reserved words as identifiers) are rejected at
//! parse time with a structured [`ParseError`].
//!
//! ## Reversibility (fidelity guard G6, boundary B6)
//! winnow appears ONLY in the crate-private [`grammar`] module; the public API is the
//! owned [`ParseError`] and the owned [`ast`] types — no winnow type leaks past the
//! crate boundary, so the parser library stays swappable. Asserted by
//! [`tests::no_vendor_type_in_public_api`].
//!
//! ## Reserved upward edge (acceptance criterion C5)
//! The intended dependency direction is `qfs-core → qfs-parser`; `qfs-parser` must
//! never depend on `qfs-core` (no cycle). See `ARCHITECTURE.md`.
//!
//! ## wasm-friendliness (boundary guard B7)
//! winnow has zero transitive deps and is macro-free/pure-Rust, so the parser adds no
//! threads, `std::fs`, or sockets. serde (`derive`) powers `-json` AST dumps and the
//! golden tests; it is likewise pure.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

pub mod ast;
mod error;
mod grammar;

pub use ast::{
    Assignment, CallRef, Codec, DdlKind, EffectBody, EffectStmt, EffectVerb, Expr, FnRef, Ident,
    JoinOp, Literal, NamedArg, Op, OrderKey, Param, PathExpr, PathRef, PathSegment, PipeOp,
    Pipeline, PlanWrap, PolicyRuleAst, Projection, ServerDdl, Source, Statement, TypeAnn, Values,
};
pub use error::{ParseError, ParseErrorCode};

/// The frozen closed-core keyword set (RFD §3), re-exported from `qfs-lang` so the
/// parser and its callers share one source of truth (boundary B6).
pub use qfs_lang::{Keyword, Span, KEYWORDS, OPERATORS};

/// Parse one qfs statement into the owned [`Statement`] AST.
///
/// Lexes the source via `qfs_lang::lex` (t03), then runs the full RFD §3 grammar
/// (t04) over the token stream. The signature is the stable, reversible boundary:
/// neither [`Statement`] nor [`ParseError`] depends on the chosen parser library
/// (winnow), so the library is swappable.
///
/// # Errors
/// Returns an owned [`ParseError`] (byte span + non-empty expected-set + machine
/// code) on any lexing or parse failure — the AI structured-error path of RFD §5.
pub fn parse_statement(src: &str) -> Result<Statement, ParseError> {
    grammar::parse(src)
}

#[cfg(test)]
mod tests;
