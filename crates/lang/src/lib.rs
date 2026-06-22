//! `cfs-lang` — the closed-core language surface for cfs.
//!
//! This crate is the single home of the **frozen reserved-keyword set** (RFD-0001
//! §3, "Language: closed core + three open registries"). The closed core is the
//! one thing in cfs that is *not* an open registry: new backends add **zero
//! keywords** (a new service = a new mount; a new action = a registered procedure;
//! a new format = a registered codec). Freezing the keyword set in exactly one
//! place is what makes that governance thesis structurally enforceable — a later
//! ticket that wants new behaviour has nowhere to add a keyword except here, and
//! the golden test ([`mod tests`]) fails if it tries (fidelity guard G1 / C1).
//!
//! AST sum types (the `enum`-modelled grammar) land in this crate in E1. E0 ships
//! the frozen keyword vocabulary ([`keywords`]) plus the **lexer** ([`lex`], t03):
//! a pure `&str -> Vec<Spanned<Token>>` scanner that is the first stage of the
//! language core (RFD §2.2). The lexer lives here, not in `cfs-parser`, so the
//! closed core has exactly one home (G1) and `cfs-lang` keeps **zero dependencies**
//! (a hand-written byte cursor, no combinator library) — no winnow type can leak
//! (G6) because winnow never enters this crate.
//!
//! ## wasm-friendliness (boundary guard B7)
//! This crate is pure data + a pure scanner: no threads, no `std::fs`, no sockets,
//! no dependencies. It must stay that way so the `wasm32` target (RFD §9) remains
//! cheap.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

pub mod error;
pub mod keywords;
pub mod lex;
pub mod span;
pub mod token;

pub use error::{LexError, LexErrorKind};
pub use keywords::{Keyword, KEYWORDS, OPERATORS};
pub use lex::lex;
pub use span::{Span, Spanned};
pub use token::{LitType, PathSeg, SizeUnit, Token};
