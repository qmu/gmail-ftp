//! ============================================================================
//! THROWAWAY SPIKE CRATE — NOT PRODUCTION (`publish = false`).
//!
//! t02 parser-library decision spike: parse the subset grammar
//! `FROM <path> |> WHERE <expr> |> SELECT <cols>` (UPPERCASE keywords, `|>`
//! pipes) in BOTH winnow and chumsky, into ONE shared AST ([`ast`]), and compare.
//!
//! The DURABLE artifact is `docs/adr/0001-parser-library.md` — it locks the
//! choice. This crate is comparison evidence only; it may rot. `qfs-parser` does
//! NOT depend on it.
//! ============================================================================
//!
//! Lint relaxation is scoped to THIS spike crate only: spikes legitimately use
//! `unwrap`/`expect`/`panic` on hardcoded inputs and in the comparison harness.
//! The strict workspace lint policy is NOT relaxed for `qfs-parser`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

pub mod ast;
pub mod chumsky_spike;
pub mod winnow_spike;

pub use ast::{SpikeError, SpikeStmt};

/// One corpus entry: a label, the input, and whether it is expected to parse.
pub struct Case {
    pub label: &'static str,
    pub input: &'static str,
    pub valid: bool,
}

/// Shared corpus exercised by both parsers (valid + deliberately broken inputs).
/// The broken cases drive the golden error corpus; the valid cases drive the
/// cross-parser AST-equality check.
pub const CORPUS: &[Case] = &[
    // -- valid --
    Case {
        label: "from_only",
        input: "FROM mail.inbox",
        valid: true,
    },
    Case {
        label: "from_where",
        input: "FROM mail.inbox |> WHERE from = 'a@qmu.jp'",
        valid: true,
    },
    Case {
        label: "from_where_select",
        input: "FROM mail.inbox |> WHERE subject LIKE 'invoice' |> SELECT id, subject",
        valid: true,
    },
    Case {
        label: "where_and_chain",
        input: "FROM mail |> WHERE size > 100 AND from = 'x' |> SELECT id",
        valid: true,
    },
    Case {
        label: "select_multi",
        input: "FROM mail |> SELECT id, subject, from",
        valid: true,
    },
    // -- broken --
    Case {
        label: "missing_select_cols",
        input: "FROM mail |> WHERE id = 1 |> SELECT",
        valid: false,
    },
    Case {
        label: "missing_pipe",
        input: "FROM mail WHERE id = 1",
        valid: false,
    },
    Case {
        label: "lowercase_keyword",
        input: "FROM mail |> where id = 1",
        valid: false,
    },
    Case {
        label: "dangling_where",
        input: "FROM mail |> WHERE",
        valid: false,
    },
    Case {
        label: "unterminated_string",
        input: "FROM mail |> WHERE from = 'unterminated",
        valid: false,
    },
    Case {
        label: "unknown_op",
        input: "FROM mail |> SHUFFLE id",
        valid: false,
    },
    Case {
        label: "empty",
        input: "",
        valid: false,
    },
];
