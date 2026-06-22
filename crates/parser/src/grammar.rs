//! Internal winnow grammar (E0 subset). **Crate-private** — winnow types never
//! escape this module; [`parse`] returns the owned [`crate::ParseError`].
//!
//! Library choice locked in `docs/adr/0001-parser-library.md` (winnow: maintained,
//! macro-free, zero transitive deps, precise spans via `cut_err`, token-level
//! expected-sets that suit the AI structured-error path).
//!
//! Keyword surface text is sourced from the frozen `cfs_lang::Keyword` set (RFD §3)
//! rather than hand-typed here, so the closed core has exactly one home (boundary
//! B6). This module is panic-free: the workspace `unwrap/expect/panic = deny` lint
//! applies to `cfs-parser` (it is NOT relaxed here, unlike the spike).

use cfs_lang::Keyword;
use winnow::ascii::{digit1, multispace0};
use winnow::combinator::{alt, cut_err, delimited, eof, preceded, repeat, separated, terminated};
use winnow::error::{ContextError, ErrMode, ParseError as WinnowParseError};
use winnow::token::take_while;
use winnow::{ModalResult, Parser};

use crate::ast::{CmpOp, Expr, Literal, Path, PipeOp, Stmt};
use crate::error::{ParseError, ParseErrorCode};

type Stream<'a> = &'a str;
type Err = ErrMode<ContextError>;

/// Parse the E0-subset grammar, mapping winnow's native error into the owned
/// [`ParseError`] at this boundary (no winnow type escapes).
pub(crate) fn parse(input: &str) -> Result<Stmt, ParseError> {
    statement.parse(input).map_err(|e| map_error(input, &e))
}

/// Map winnow's `ParseError<_, ContextError>` onto the owned structured error.
fn map_error(input: &str, err: &WinnowParseError<Stream<'_>, ContextError>) -> ParseError {
    let at = err.offset();
    let rest = input.get(at..).unwrap_or("");
    if rest.is_empty() {
        return ParseError::new(
            at,
            ParseErrorCode::UnexpectedEof,
            vec!["more input".to_string()],
            "unexpected end of input",
        );
    }
    if rest.chars().next().is_some_and(|c| c.is_ascii_lowercase()) {
        // A lowercase keyword-shaped token: closed-core keywords are UPPERCASE
        // (RFD §3). Reject at parse time with a structured code (RFD §5).
        return ParseError::new(
            at,
            ParseErrorCode::UnknownKeyword,
            vec!["UPPERCASE keyword".to_string()],
            format!("expected UPPERCASE keyword, found `{}`", peek_word(rest)),
        );
    }
    ParseError::new(
        at,
        ParseErrorCode::UnexpectedToken,
        expected_tokens(),
        format!("unexpected token near `{}`", peek_word(rest)),
    )
}

/// The closed-core tokens the E0-subset grammar can expect at a failure point,
/// drawn from the frozen `cfs_lang::Keyword` set plus the pipe operator.
fn expected_tokens() -> Vec<String> {
    vec![
        Keyword::From.text().to_string(),
        "|>".to_string(),
        Keyword::Where.text().to_string(),
        Keyword::Select.text().to_string(),
        "AND".to_string(),
        "a path".to_string(),
    ]
}

fn peek_word(s: &str) -> &str {
    let end = s.find(|c: char| c.is_whitespace()).unwrap_or(s.len());
    s.get(..end.min(16)).unwrap_or(s)
}

// ---- combinators ----------------------------------------------------------

fn ws<'a, O, P>(inner: P) -> impl Parser<Stream<'a>, O, Err>
where
    P: Parser<Stream<'a>, O, Err>,
{
    delimited(multispace0, inner, multispace0)
}

fn ident(input: &mut Stream<'_>) -> ModalResult<String> {
    take_while(1.., |c: char| c.is_ascii_alphanumeric() || c == '_')
        .map(|s: &str| s.to_string())
        .parse_next(input)
}

fn path(input: &mut Stream<'_>) -> ModalResult<Path> {
    separated(1.., ident, '.').map(Path).parse_next(input)
}

fn cmp_op(input: &mut Stream<'_>) -> ModalResult<CmpOp> {
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
    preceded(ws(Keyword::Where.text()), cut_err(expr))
        .map(PipeOp::Where)
        .parse_next(input)
}

fn select_op(input: &mut Stream<'_>) -> ModalResult<PipeOp> {
    preceded(
        ws(Keyword::Select.text()),
        cut_err(separated(1.., ws(path), ',')),
    )
    .map(PipeOp::Select)
    .parse_next(input)
}

fn pipe_op(input: &mut Stream<'_>) -> ModalResult<PipeOp> {
    alt((where_op, select_op)).parse_next(input)
}

fn statement(input: &mut Stream<'_>) -> ModalResult<Stmt> {
    let from = preceded(ws(Keyword::From.text()), cut_err(ws(path))).parse_next(input)?;
    let ops: Vec<PipeOp> = repeat(0.., preceded(ws("|>"), cut_err(pipe_op))).parse_next(input)?;
    terminated(multispace0, eof).parse_next(input)?;
    Ok(Stmt { from, ops })
}
