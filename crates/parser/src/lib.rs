//! `cfs-parser` — the parser front door (RFD-0001 §2.2, §9).
//!
//! E0/t02 fills the skeleton: the parser library is **winnow** (decision locked in
//! `docs/adr/0001-parser-library.md` after the winnow-vs-chumsky spike under
//! `spikes/parser-spike`). This crate parses the **E0-subset grammar**
//! `FROM <path> |> WHERE <expr> |> SELECT <cols>` (UPPERCASE keywords, `|>` pipes).
//! The full RFD §3 grammar is E1.
//!
//! ## Reversibility (fidelity guard G6, boundary B6)
//! winnow appears ONLY in the crate-private [`grammar`] module. The public API is
//! the owned [`ParseError`] and the owned [`ast`] types — no winnow type leaks
//! past the crate boundary, so E1 can swap libraries without breaking callers.
//! This is asserted by [`tests::no_vendor_type_in_public_api`].
//!
//! ## Reserved upward edge (acceptance criterion C5)
//! The intended dependency direction is `cfs-core → cfs-parser`. `cfs-parser` must
//! never depend on `cfs-core` (no cycle). See `ARCHITECTURE.md`.
//!
//! ## wasm-friendliness (boundary guard B7)
//! `cfs-parser` builds for `wasm32` (RFD §9). winnow has **zero transitive
//! dependencies** and is macro-free/pure-Rust, so it adds no threads, `std::fs`,
//! or sockets (a deciding factor over chumsky, which pulls `stacker`/`psm` — a
//! C-built stack manipulator that is wasm-hostile). The `wasm32-unknown-unknown`
//! build is validated in CI (the target is not installed on the dev host).

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

pub mod ast;
mod error;
mod grammar;

pub use ast::{CmpOp, Expr, Literal, Path, PipeOp, Stmt};
pub use error::{ParseError, ParseErrorCode};

/// The frozen closed-core keyword set (RFD §3), re-exported from `cfs-lang` so the
/// parser and its callers share one source of truth (boundary B6).
pub use cfs_lang::{Keyword, KEYWORDS, OPERATORS};

/// Parse one cfs statement into the owned [`Stmt`] AST.
///
/// E0 handles the spike-grammar subset `FROM <path> |> WHERE <expr> |> SELECT
/// <cols>`; E1 extends it to the full RFD §3 grammar. The signature is the stable,
/// reversible boundary: neither [`Stmt`] nor [`ParseError`] depends on the chosen
/// parser library (winnow), so the library is swappable.
///
/// # Errors
/// Returns an owned [`ParseError`] (byte span + expected-set + machine code) on any
/// parse failure — the AI structured-error path of RFD §5.
pub fn parse_statement(src: &str) -> Result<Stmt, ParseError> {
    grammar::parse(src)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_full_pipeline() {
        let stmt = parse_statement("FROM mail.inbox |> WHERE subject LIKE 'invoice' |> SELECT id")
            .expect("valid pipeline must parse");
        assert_eq!(stmt.from, Path(vec!["mail".into(), "inbox".into()]));
        assert_eq!(stmt.ops.len(), 2);
    }

    #[test]
    fn from_only_parses() {
        let stmt = parse_statement("FROM mail").expect("FROM-only is valid");
        assert!(stmt.ops.is_empty());
    }

    #[test]
    fn lowercase_keyword_is_structured_unknown_keyword() {
        let err = parse_statement("FROM mail |> where id = 1").expect_err("lowercase rejected");
        assert_eq!(err.code, ParseErrorCode::UnknownKeyword);
        assert_eq!(err.at, 13);
    }

    #[test]
    fn missing_pipe_is_unexpected_token() {
        let err = parse_statement("FROM mail WHERE id = 1").expect_err("missing |> rejected");
        assert_eq!(err.code, ParseErrorCode::UnexpectedToken);
    }

    #[test]
    fn dangling_where_is_eof() {
        let err = parse_statement("FROM mail |> WHERE").expect_err("dangling WHERE rejected");
        assert_eq!(err.code, ParseErrorCode::UnexpectedEof);
    }

    #[test]
    fn sees_frozen_keyword_set() {
        // The B6 edge (cfs-parser -> cfs-lang) is wired and the frozen vocabulary
        // is re-exported here.
        assert!(KEYWORDS.contains(&"FROM"));
        assert_eq!(Keyword::Where.text(), "WHERE");
    }

    /// No-vendor-leak audit (RFD §9, acceptance criterion). The public error type is
    /// the owned `ParseError`; this test pins its construction and `Display` shape so
    /// a refactor cannot accidentally start surfacing a winnow type through it. The
    /// structural guarantee (winnow confined to the private `grammar` module) is
    /// enforced by `grammar` being non-`pub`; this test pins the observable surface.
    #[test]
    fn no_vendor_type_in_public_api() {
        let err = parse_statement("").expect_err("empty input is an error");
        // ParseError is fully owned: clonable, comparable, displayable without any
        // parser-library type in scope.
        let cloned = err.clone();
        assert_eq!(err, cloned);
        let shown = format!("{err}");
        assert!(shown.contains("UNEXPECTED_EOF"));
    }
}
