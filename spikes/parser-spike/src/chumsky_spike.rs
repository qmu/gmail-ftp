//! ============================================================================
//! THROWAWAY SPIKE — NOT PRODUCTION CODE (the RETAINED LOSER, see ADR A3).
//!
//! t02 chumsky candidate for the parser-library decision. The decision is LOCKED
//! in `docs/adr/0001-parser-library.md` (winnow won). This file is retained ONLY
//! as comparison evidence behind that ADR; do NOT mistake it for a live second
//! parser or wire it into `cfs-parser`. The production parser lives in
//! `crates/parser` (`cfs-parser`) behind the owned `ParseError`.
//! ============================================================================
//!
//! Same grammar and same shared AST as the winnow spike. chumsky 0.13 uses the
//! zero-copy `extra::Err<Rich<...>>` API and supports error *recovery*, which is
//! the one criterion that could have overridden the winnow default (it did not —
//! see the ADR).

use chumsky::error::Rich;
use chumsky::prelude::*;

use crate::ast::{CmpOp, Expr, Literal, Path, PipeOp, SpikeError, SpikeErrorCode, SpikeStmt};

/// Public spike entry point. Maps chumsky's `Rich` errors into the shared
/// [`SpikeError`].
///
/// # Errors
/// Returns the FIRST [`SpikeError`] on failure (the corpus also exercises the
/// multi-error path via [`parse_all_errors`]).
pub fn parse(input: &str) -> Result<SpikeStmt, SpikeError> {
    let result = parser().parse(input);
    match result.into_result() {
        Ok(stmt) => Ok(stmt),
        Err(errs) => Err(errs
            .into_iter()
            .next()
            .map(|e| map_error(&e))
            .unwrap_or_else(|| SpikeError {
                at: input.len(),
                code: SpikeErrorCode::UnexpectedEof,
                expected: vec![],
                message: "unknown parse failure".to_string(),
            })),
    }
}

/// Recovery-aware entry point: returns ALL errors chumsky surfaces for one input.
/// This is the multi-error-recovery evidence for the ADR.
#[must_use]
pub fn parse_all_errors(input: &str) -> Vec<SpikeError> {
    parser()
        .parse(input)
        .into_errors()
        .iter()
        .map(map_error)
        .collect()
}

fn map_error(err: &Rich<'_, char>) -> SpikeError {
    let span = err.span();
    let at = span.start;
    let expected: Vec<String> = err.expected().map(|e| format!("{e:?}")).collect();
    // Heuristic classification onto the structured-error codes the AI path needs.
    let code = if err.found().is_none() {
        SpikeErrorCode::UnexpectedEof
    } else {
        SpikeErrorCode::UnexpectedToken
    };
    SpikeError {
        at,
        code,
        expected,
        message: err.to_string(),
    }
}

// ---- grammar --------------------------------------------------------------

type Extra<'a> = extra::Err<Rich<'a, char>>;

fn ident<'a>() -> impl Parser<'a, &'a str, String, Extra<'a>> + Clone {
    any()
        .filter(|c: &char| c.is_ascii_alphanumeric() || *c == '_')
        .repeated()
        .at_least(1)
        .collect::<String>()
}

fn path<'a>() -> impl Parser<'a, &'a str, Path, Extra<'a>> + Clone {
    ident()
        .separated_by(just('.'))
        .at_least(1)
        .collect::<Vec<_>>()
        .map(Path)
}

fn cmp_op<'a>() -> impl Parser<'a, &'a str, CmpOp, Extra<'a>> + Clone {
    choice((
        just("<=").to(CmpOp::Le),
        just(">=").to(CmpOp::Ge),
        just("<>").to(CmpOp::Ne),
        just("=").to(CmpOp::Eq),
        just("<").to(CmpOp::Lt),
        just(">").to(CmpOp::Gt),
        just("LIKE").to(CmpOp::Like),
    ))
}

fn literal<'a>() -> impl Parser<'a, &'a str, Literal, Extra<'a>> + Clone {
    let string = any()
        .filter(|c: &char| *c != '\'')
        .repeated()
        .collect::<String>()
        .delimited_by(just('\''), just('\''))
        .map(Literal::Str);
    let int = text::int(10)
        .from_str::<i64>()
        .unwrapped()
        .map(Literal::Int);
    choice((string, int))
}

fn cmp<'a>() -> impl Parser<'a, &'a str, Expr, Extra<'a>> + Clone {
    path()
        .padded()
        .then(cmp_op().padded())
        .then(literal().padded())
        .map(|((lhs, op), rhs)| Expr::Cmp { lhs, op, rhs })
}

fn expr<'a>() -> impl Parser<'a, &'a str, Expr, Extra<'a>> + Clone {
    cmp().clone().foldl(
        just("AND").padded().ignore_then(cmp()).repeated(),
        |a, b| Expr::And(Box::new(a), Box::new(b)),
    )
}

fn pipe_op<'a>() -> impl Parser<'a, &'a str, PipeOp, Extra<'a>> + Clone {
    let where_op = just("WHERE")
        .padded()
        .ignore_then(expr())
        .map(PipeOp::Where);
    let select_op = just("SELECT")
        .padded()
        .ignore_then(
            path()
                .padded()
                .separated_by(just(','))
                .at_least(1)
                .collect(),
        )
        .map(PipeOp::Select);
    choice((where_op, select_op))
}

fn parser<'a>() -> impl Parser<'a, &'a str, SpikeStmt, Extra<'a>> {
    let from = just("FROM").padded().ignore_then(path().padded());
    let ops = just("|>")
        .padded()
        .ignore_then(pipe_op())
        .repeated()
        .collect::<Vec<_>>();
    from.then(ops)
        .map(|(from, ops)| SpikeStmt { from, ops })
        .padded()
        .then_ignore(end())
}
