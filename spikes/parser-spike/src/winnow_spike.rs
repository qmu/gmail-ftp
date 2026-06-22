//! ============================================================================
//! THROWAWAY SPIKE — NOT PRODUCTION CODE.
//!
//! t02 winnow candidate for the parser-library decision. The decision is LOCKED
//! in `docs/adr/0001-parser-library.md`. This file is retained only as comparison
//! evidence behind that ADR; do NOT mistake it for a live second parser. The
//! production parser lives in `crates/parser` (`cfs-parser`) behind the owned
//! `ParseError`.
//! ============================================================================
//!
//! Grammar (subset of RFD §3):
//!   stmt   := "FROM" path ("|>" op)*
//!   op     := "WHERE" expr | "SELECT" path ("," path)*
//!   expr   := cmp ("AND" cmp)*
//!   cmp    := path cmpop literal
//!   literal:= sqstring | int
//!
//! winnow is function-/combinator-based (no macros). Spans are derived from the
//! remaining-input length against the original input length.

use winnow::ascii::{digit1, multispace0};
use winnow::combinator::{alt, cut_err, delimited, eof, preceded, repeat, separated, terminated};
use winnow::error::{ContextError, ErrMode, ParseError as WinnowParseError};
use winnow::token::take_while;
use winnow::{ModalResult, Parser};

use crate::ast::{CmpOp, Expr, Literal, Path, PipeOp, SpikeError, SpikeErrorCode, SpikeStmt};

type Stream<'a> = &'a str;
/// The error type carried through every combinator. Functions return
/// [`ModalResult<O>`] = `Result<O, Err>`, so the whole grammar agrees on one error
/// type (the modal/non-modal split is winnow's; we stay modal throughout).
type Err = ErrMode<ContextError>;

/// Public spike entry point. Maps winnow's native error into the shared
/// [`SpikeError`] so the golden corpus compares like-for-like with chumsky.
///
/// # Errors
/// Returns a [`SpikeError`] on any parse failure.
pub fn parse(input: &str) -> Result<SpikeStmt, SpikeError> {
    match statement.parse(input) {
        Ok(stmt) => Ok(stmt),
        Err(err) => Err(map_error(input, &err)),
    }
}

/// Translate winnow's `ParseError<_, ContextError>` into the owned spike error.
fn map_error(input: &str, err: &WinnowParseError<Stream<'_>, ContextError>) -> SpikeError {
    let at = err.offset();
    let rest = &input[at..];
    // Classify against the closed-core/structured-error path.
    let (code, expected, message) = if rest.is_empty() {
        (
            SpikeErrorCode::UnexpectedEof,
            vec!["more input".to_string()],
            "unexpected end of input".to_string(),
        )
    } else if starts_with_lowercase_word(rest) {
        (
            SpikeErrorCode::UnknownKeyword,
            vec!["UPPERCASE keyword".to_string()],
            format!("expected UPPERCASE keyword, found `{}`", peek_word(rest)),
        )
    } else {
        (
            SpikeErrorCode::UnexpectedToken,
            vec!["FROM, |>, WHERE, SELECT, AND, or a path".to_string()],
            format!("unexpected token near `{}`", peek_word(rest)),
        )
    };
    SpikeError {
        at,
        code,
        expected,
        message,
    }
}

fn starts_with_lowercase_word(s: &str) -> bool {
    s.chars().next().is_some_and(|c| c.is_ascii_lowercase())
}

fn peek_word(s: &str) -> &str {
    let end = s.find(|c: char| c.is_whitespace()).unwrap_or(s.len());
    &s[..end.min(16)]
}

// ---- combinators ----------------------------------------------------------

fn ws<'a, O, P>(inner: P) -> impl Parser<Stream<'a>, O, Err>
where
    P: Parser<Stream<'a>, O, Err>,
{
    delimited(multispace0, inner, multispace0)
}

/// An identifier segment: ascii alnum / underscore.
fn ident(input: &mut Stream<'_>) -> ModalResult<String> {
    take_while(1.., |c: char| c.is_ascii_alphanumeric() || c == '_')
        .map(|s: &str| s.to_string())
        .parse_next(input)
}

/// A dotted path: `a.b.c`.
fn path(input: &mut Stream<'_>) -> ModalResult<Path> {
    separated(1.., ident, '.').map(Path).parse_next(input)
}

fn cmp_op(input: &mut Stream<'_>) -> ModalResult<CmpOp> {
    // Longer operators first so `<=` is not shadowed by `<`.
    alt((
        "<=".value(CmpOp::Le),
        ">=".value(CmpOp::Ge),
        "<>".value(CmpOp::Ne),
        "=".value(CmpOp::Eq),
        "<".value(CmpOp::Lt),
        ">".value(CmpOp::Gt),
        "LIKE".value(CmpOp::Like),
    ))
    .parse_next(input)
}

fn literal(input: &mut Stream<'_>) -> ModalResult<Literal> {
    alt((
        delimited('\'', take_while(0.., |c: char| c != '\''), '\'')
            .map(|s: &str| Literal::Str(s.to_string())),
        digit1.parse_to().map(Literal::Int),
    ))
    .parse_next(input)
}

fn cmp(input: &mut Stream<'_>) -> ModalResult<Expr> {
    (ws(path), ws(cmp_op), ws(literal))
        .map(|(lhs, op, rhs)| Expr::Cmp { lhs, op, rhs })
        .parse_next(input)
}

fn expr(input: &mut Stream<'_>) -> ModalResult<Expr> {
    let first = cmp(input)?;
    let rest: Vec<Expr> = repeat(0.., preceded(ws("AND"), cmp)).parse_next(input)?;
    Ok(rest
        .into_iter()
        .fold(first, |acc, next| Expr::And(Box::new(acc), Box::new(next))))
}

fn where_op(input: &mut Stream<'_>) -> ModalResult<PipeOp> {
    // `cut_err`: once `WHERE` is matched, a failure in the body is reported at the
    // deep position (no backtracking to the `|>`). This is the idiomatic winnow
    // way to get precise error spans on committed alternatives.
    preceded(ws("WHERE"), cut_err(expr))
        .map(PipeOp::Where)
        .parse_next(input)
}

fn select_op(input: &mut Stream<'_>) -> ModalResult<PipeOp> {
    preceded(ws("SELECT"), cut_err(separated(1.., ws(path), ',')))
        .map(PipeOp::Select)
        .parse_next(input)
}

fn pipe_op(input: &mut Stream<'_>) -> ModalResult<PipeOp> {
    alt((where_op, select_op)).parse_next(input)
}

fn statement(input: &mut Stream<'_>) -> ModalResult<SpikeStmt> {
    let from = preceded(ws("FROM"), cut_err(ws(path))).parse_next(input)?;
    // Once `|>` is matched, the following op is committed (deep error reporting).
    let ops: Vec<PipeOp> = repeat(0.., preceded(ws("|>"), cut_err(pipe_op))).parse_next(input)?;
    // Require end-of-input: trailing tokens (e.g. a missing `|>`) are an error.
    terminated(multispace0, eof).parse_next(input)?;
    Ok(SpikeStmt { from, ops })
}
