//! Internal winnow grammar (t04, the full RFD §3 pipe-SQL grammar). **Crate-private**
//! — winnow types never escape this module; [`parse`] returns the owned
//! [`crate::ParseError`] (fidelity guard G6).
//!
//! ## Token-stream input (t03 → t04)
//! Unlike the E0 spike (which parsed `&str` directly), this grammar consumes the
//! **t03 token stream**: `cfs_lang::lex` produces `Vec<Spanned<Token>>`, and winnow's
//! built-in `&[T]` [`winnow::stream::Stream`] impl drives the combinators over that
//! slice. Each AST node re-spans itself from the byte span carried by its tokens, so
//! diagnostics round-trip to source (RFD §5/§10). The lexer already folds path
//! `@version`, size/typed literals, and operators; this module stitches multi-word
//! keywords (`GROUP BY`, `INSERT INTO`, …), which the lexer emits as adjacent tokens.
//!
//! ## Closed core, structurally (RFD §3)
//! Keyword surface comes from the frozen `cfs_lang::Keyword` set — there is no second
//! transcription. The grammar rejects unknown core constructs (lowercase keywords,
//! reserved-word-as-identifier) but leaves the three registry seams open: `CALL
//! driver.action`, `fn(...)`, `DECODE/ENCODE fmt`, and `/driver/...` paths all parse
//! into string-named reference nodes without resolving the names.
//!
//! ## Panic-free
//! The workspace `unwrap/expect/panic = deny` lint applies (NOT relaxed here, unlike
//! the spike). Every combinator returns a `Result`; the boundary mapper turns a
//! winnow failure into a structured error without ever indexing-panicking.

use cfs_lang::token::{LitType, PathSeg};
use cfs_lang::{lex, Keyword, Span, Spanned, Token};
use winnow::combinator::{alt, cut_err, opt, preceded, repeat, separated};
use winnow::error::{ContextError, ErrMode, ParseError as WinnowParseError};
use winnow::token::any;
use winnow::{ModalResult, Parser};

use crate::ast::{
    Assignment, CallRef, Codec, DdlKind, EffectBody, EffectStmt, EffectVerb, Expr, FnRef, Ident,
    JoinOp, Literal, NamedArg, Op, OrderKey, PathExpr, PathRef, PathSegment, PipeOp, Pipeline,
    PlanWrap, Projection, ServerDdl, Source, Statement, Values,
};
use crate::error::{ParseError, ParseErrorCode};

/// The parser input stream: a slice of spanned tokens (winnow drives this directly).
type Stream<'a> = &'a [Spanned<Token>];
/// The winnow modal error used internally; never escapes this module.
type Err = ErrMode<ContextError>;

/// Parse one cfs statement from source text into the owned [`Statement`] AST.
///
/// Lexes via `cfs_lang::lex`, then runs the winnow grammar over the token slice,
/// mapping any winnow failure into the owned [`ParseError`] at this boundary.
///
/// # Errors
/// Returns a [`ParseError`] on a lexing error or any grammar failure.
pub(crate) fn parse(input: &str) -> Result<Statement, ParseError> {
    let tokens = lex(input).map_err(|e| lex_to_parse_error(input, &e))?;
    let slice: Stream<'_> = &tokens;
    match statement.parse(slice) {
        Ok(stmt) => Ok(stmt),
        Err(e) => Err(map_error(input, &tokens, &e)),
    }
}

/// Map a lexer error into the parser's owned error type (the two diagnostic
/// surfaces compose; RFD §5).
fn lex_to_parse_error(_input: &str, e: &cfs_lang::LexError) -> ParseError {
    ParseError::new(
        e.span.start as usize,
        e.span,
        ParseErrorCode::UnexpectedToken,
        vec!["a valid token".to_string()],
        "an unlexable character",
        format!("lexing failed: {}", e.kind.as_str()),
    )
}

/// Map winnow's token-stream `ParseError` onto the owned structured error. The
/// failure offset is a **token index**; we resolve it back to a byte span via the
/// offending token (or EOF). The `found` description names the token *kind*, never
/// its literal value (RFD §10 secret hygiene).
fn map_error(
    _input: &str,
    tokens: &[Spanned<Token>],
    err: &WinnowParseError<Stream<'_>, ContextError>,
) -> ParseError {
    let idx = err.offset();
    let Some(found_tok) = tokens.get(idx) else {
        // EOF: point at the end of the last token (or byte 0 for empty input).
        let end = tokens.last().map_or(0, |t| t.span.end);
        return ParseError::new(
            end as usize,
            Span::new(end, end),
            ParseErrorCode::UnexpectedEof,
            vec!["more input".to_string()],
            "end of input",
            "unexpected end of input",
        );
    };
    let span = found_tok.span;
    let (code, message) = classify(&found_tok.node);
    ParseError::new(
        span.start as usize,
        span,
        code,
        expected_set(),
        describe(&found_tok.node),
        message,
    )
}

/// Classify the offending token into a structured code + message. Lowercase
/// keyword-shaped identifiers are flagged distinctly (closed-core keywords are
/// UPPERCASE, RFD §3); reserved keywords in identifier position are flagged too.
fn classify(tok: &Token) -> (ParseErrorCode, String) {
    match tok {
        Token::Ident(s)
            if s.chars().next().is_some_and(char::is_lowercase) && is_keyword_word(s) =>
        {
            (
                ParseErrorCode::UnknownKeyword,
                "closed-core keywords are UPPERCASE (RFD §3)".to_string(),
            )
        }
        Token::Keyword(_) => (
            ParseErrorCode::ReservedAsIdentifier,
            "a reserved keyword cannot be used here".to_string(),
        ),
        _ => (
            ParseErrorCode::UnexpectedToken,
            "the grammar did not expect this token here".to_string(),
        ),
    }
}

/// Whether `s`, uppercased, would be a closed-core keyword word (used to detect
/// lowercase keyword typos like `where`).
fn is_keyword_word(s: &str) -> bool {
    Keyword::from_word(&s.to_uppercase()).is_some()
        || matches!(
            s.to_uppercase().as_str(),
            "GROUP"
                | "ORDER"
                | "INSERT"
                | "UPSERT"
                | "MATERIALIZED"
                | "BY"
                | "INTO"
                | "OF"
                | "ASC"
                | "DESC"
        )
}

/// A representative closed-core expected-set for a failure point (non-empty per the
/// structured-error contract, RFD §5).
fn expected_set() -> Vec<String> {
    vec![
        Keyword::From.text().to_string(),
        Keyword::InsertInto.text().to_string(),
        Keyword::Create.text().to_string(),
        Keyword::Preview.text().to_string(),
        Keyword::Commit.text().to_string(),
        "|>".to_string(),
        "a path".to_string(),
    ]
}

/// Describe a token by *kind* (never its literal value — RFD §10 secret hygiene).
fn describe(tok: &Token) -> String {
    match tok {
        Token::Keyword(k) => format!("keyword `{}`", k.text()),
        Token::Pipe => "`|>`".to_string(),
        Token::Eq | Token::Ne | Token::Lt | Token::Gt | Token::Le | Token::Ge | Token::Tilde => {
            "an operator".to_string()
        }
        Token::LParen => "`(`".to_string(),
        Token::RParen => "`)`".to_string(),
        Token::Comma => "`,`".to_string(),
        Token::Dot => "`.`".to_string(),
        Token::Star => "`*`".to_string(),
        Token::Arrow => "`=>`".to_string(),
        Token::Ident(_) => "an identifier".to_string(),
        Token::Path(_) => "a path".to_string(),
        Token::Str(_) => "a string literal".to_string(),
        Token::Int(_) => "an integer literal".to_string(),
        Token::Float(_) => "a float literal".to_string(),
        Token::Bool(_) => "a boolean literal".to_string(),
        Token::Null => "`NULL`".to_string(),
        Token::Size { .. } => "a size literal".to_string(),
        Token::TypedLit { .. } => "a typed literal".to_string(),
        _ => "a token".to_string(),
    }
}

// ---- low-level token matchers --------------------------------------------

/// Match exactly the given closed-core keyword.
fn kw<'a>(k: Keyword) -> impl Parser<Stream<'a>, Span, Err> {
    any.verify_map(move |t: Spanned<Token>| match t.node {
        Token::Keyword(got) if got == k => Some(t.span),
        _ => None,
    })
}

/// Match an UPPERCASE identifier word equal to `word` (used for multi-word keyword
/// tails and DDL sub-keywords the lexer leaves as identifiers, e.g. `BY`, `INTO`,
/// `OF`, `ASC`, `DESC`, `MATERIALIZED`).
fn word<'a>(word: &'static str) -> impl Parser<Stream<'a>, Span, Err> {
    any.verify_map(move |t: Spanned<Token>| match t.node {
        Token::Ident(ref s) if s == word => Some(t.span),
        _ => None,
    })
}

/// Match a bare identifier, rejecting reserved keywords in identifier position.
fn ident(input: &mut Stream<'_>) -> ModalResult<Spanned<Ident>> {
    any.verify_map(|t: Spanned<Token>| match t.node {
        Token::Ident(s) => Some(Spanned::new(s, t.span)),
        _ => None,
    })
    .parse_next(input)
}

/// Match a single non-keyword token equal to the given punctuation/operator token.
fn punct<'a>(tok: Token) -> impl Parser<Stream<'a>, Span, Err> {
    any.verify_map(move |t: Spanned<Token>| if t.node == tok { Some(t.span) } else { None })
}

/// Multi-word keyword: `GROUP BY` (= `GROUP` ident + `BY` ident, both UPPERCASE).
fn group_by(input: &mut Stream<'_>) -> ModalResult<Span> {
    (word("GROUP"), word("BY"))
        .map(|(a, b)| Span::new(a.start, b.end))
        .parse_next(input)
}

/// Multi-word keyword: `ORDER BY`.
fn order_by(input: &mut Stream<'_>) -> ModalResult<Span> {
    (word("ORDER"), word("BY"))
        .map(|(a, b)| Span::new(a.start, b.end))
        .parse_next(input)
}

/// Multi-word keyword: `INSERT INTO`.
fn insert_into(input: &mut Stream<'_>) -> ModalResult<Span> {
    (word("INSERT"), word("INTO"))
        .map(|(a, b)| Span::new(a.start, b.end))
        .parse_next(input)
}

/// Multi-word keyword: `UPSERT INTO`.
fn upsert_into(input: &mut Stream<'_>) -> ModalResult<Span> {
    (word("UPSERT"), word("INTO"))
        .map(|(a, b)| Span::new(a.start, b.end))
        .parse_next(input)
}

// ---- paths ----------------------------------------------------------------

/// A `/driver/seg/seg` mount path (from a single lexer `Token::Path`), plus an
/// optional `AS OF '<ts>'` temporal coordinate (RFD §4). The path/mount registry
/// seam: segments are raw strings, never resolved here.
fn path_expr(input: &mut Stream<'_>) -> ModalResult<PathExpr> {
    let head = any
        .verify_map(|t: Spanned<Token>| match t.node {
            Token::Path(segs) => Some(Spanned::new(segs, t.span)),
            _ => None,
        })
        .parse_next(input)?;
    let as_of = opt(preceded(
        (kw(Keyword::As), word("OF")),
        any.verify_map(|t: Spanned<Token>| match t.node {
            Token::Str(s) => Some(s),
            _ => None,
        }),
    ))
    .parse_next(input)?;
    // Drop a leading empty segment produced by the lexer's leading `/`.
    let segments: Vec<PathSegment> = head
        .node
        .into_iter()
        .filter(|s: &PathSeg| !(s.name.is_empty() && s.version.is_none()))
        .map(|s| PathSegment {
            name: s.name,
            version: s.version,
            glob: s.glob,
        })
        .collect();
    Ok(PathExpr {
        segments,
        as_of,
        span: head.span,
    })
}

// ---- expressions (precedence climbing over the frozen operator set) -------

/// The operator-set expression entry point. Precedence (low → high):
/// `OR` < `AND` < `NOT` < comparison/predicate < primary.
fn expr(input: &mut Stream<'_>) -> ModalResult<Expr> {
    or_expr(input)
}

fn or_expr(input: &mut Stream<'_>) -> ModalResult<Expr> {
    let first = and_expr(input)?;
    let rest: Vec<Expr> = repeat(0.., preceded(word("OR"), and_expr)).parse_next(input)?;
    Ok(rest.into_iter().fold(first, |acc, next| Expr::Binary {
        op: Op::Or,
        lhs: Box::new(acc),
        rhs: Box::new(next),
    }))
}

fn and_expr(input: &mut Stream<'_>) -> ModalResult<Expr> {
    let first = not_expr(input)?;
    let rest: Vec<Expr> = repeat(0.., preceded(word("AND"), not_expr)).parse_next(input)?;
    Ok(rest.into_iter().fold(first, |acc, next| Expr::Binary {
        op: Op::And,
        lhs: Box::new(acc),
        rhs: Box::new(next),
    }))
}

fn not_expr(input: &mut Stream<'_>) -> ModalResult<Expr> {
    let nots: Vec<Span> = repeat(0.., word("NOT")).parse_next(input)?;
    let inner = predicate(input)?;
    Ok(nots.into_iter().fold(inner, |acc, _| Expr::Unary {
        op: Op::Not,
        expr: Box::new(acc),
    }))
}

/// A comparison / predicate: `lhs (op rhs | IN (..) | BETWEEN a AND b | LIKE p |
/// op ANY (..) )?`.
fn predicate(input: &mut Stream<'_>) -> ModalResult<Expr> {
    let lhs = primary(input)?;
    // Try the predicate tails in order.
    if let Some(tail) = opt(predicate_tail).parse_next(input)? {
        return Ok(tail(lhs));
    }
    Ok(lhs)
}

/// A boxed transform applying a predicate tail to its left-hand expression.
type TailFn = Box<dyn FnOnce(Expr) -> Expr>;

fn predicate_tail(input: &mut Stream<'_>) -> ModalResult<TailFn> {
    alt((
        // <op> ANY (set)  |  <op> rhs
        cmp_tail,
        in_tail,
        between_tail,
        like_tail,
    ))
    .parse_next(input)
}

fn cmp_op(input: &mut Stream<'_>) -> ModalResult<Op> {
    any.verify_map(|t: Spanned<Token>| match t.node {
        Token::Eq => Some(Op::Eq),
        Token::Ne => Some(Op::Ne),
        Token::Lt => Some(Op::Lt),
        Token::Gt => Some(Op::Gt),
        Token::Le => Some(Op::Le),
        Token::Ge => Some(Op::Ge),
        Token::Tilde => Some(Op::Match),
        _ => None,
    })
    .parse_next(input)
}

fn cmp_tail(input: &mut Stream<'_>) -> ModalResult<TailFn> {
    let op = cmp_op(input)?;
    // `<op> ANY (set)` quantified comparison.
    if opt(word("ANY")).parse_next(input)?.is_some() {
        let set = paren_expr_list(input)?;
        return Ok(Box::new(move |lhs| Expr::AnyOp {
            op,
            expr: Box::new(lhs),
            set,
        }));
    }
    let rhs = primary(input)?;
    Ok(Box::new(move |lhs| Expr::Binary {
        op,
        lhs: Box::new(lhs),
        rhs: Box::new(rhs),
    }))
}

fn in_tail(input: &mut Stream<'_>) -> ModalResult<TailFn> {
    let _ = word("IN").parse_next(input)?;
    let set = paren_expr_list(input)?;
    Ok(Box::new(move |lhs| Expr::In {
        expr: Box::new(lhs),
        set,
    }))
}

fn between_tail(input: &mut Stream<'_>) -> ModalResult<TailFn> {
    let _ = word("BETWEEN").parse_next(input)?;
    let low = primary(input)?;
    let _ = word("AND").parse_next(input)?;
    let high = primary(input)?;
    Ok(Box::new(move |lhs| Expr::Between {
        expr: Box::new(lhs),
        low: Box::new(low),
        high: Box::new(high),
    }))
}

fn like_tail(input: &mut Stream<'_>) -> ModalResult<TailFn> {
    let _ = word("LIKE").parse_next(input)?;
    let pat = primary(input)?;
    Ok(Box::new(move |lhs| Expr::Like {
        expr: Box::new(lhs),
        pattern: Box::new(pat),
    }))
}

/// `( <expr>, … )` argument/set list.
fn paren_expr_list(input: &mut Stream<'_>) -> ModalResult<Vec<Expr>> {
    let _ = punct(Token::LParen).parse_next(input)?;
    let items: Vec<Expr> = separated(0.., expr, punct(Token::Comma)).parse_next(input)?;
    let _ = punct(Token::RParen).parse_next(input)?;
    Ok(items)
}

/// A primary expression: literal, parenthesised expr, `*`, function call, dotted
/// path / column.
fn primary(input: &mut Stream<'_>) -> ModalResult<Expr> {
    alt((literal.map(Expr::Lit), paren_expr, fn_call, dotted_path)).parse_next(input)
}

fn paren_expr(input: &mut Stream<'_>) -> ModalResult<Expr> {
    let _ = punct(Token::LParen).parse_next(input)?;
    let e = expr(input)?;
    let _ = punct(Token::RParen).parse_next(input)?;
    Ok(e)
}

/// A registry function call `name(args)` — the function registry seam (RFD §3). The
/// name is a string; resolution (incl. receiver-typed alias resolution) is E2.
fn fn_call(input: &mut Stream<'_>) -> ModalResult<Expr> {
    let name = ident(input)?;
    let open = punct(Token::LParen).parse_next(input)?;
    let args: Vec<Expr> = separated(0.., expr, punct(Token::Comma)).parse_next(input)?;
    let close = punct(Token::RParen).parse_next(input)?;
    Ok(Expr::Fn(FnRef {
        name: name.node,
        args,
        span: Span::new(name.span.start.min(open.start), close.end),
    }))
}

/// A dotted path `a.b.c` (struct navigation, RFD §4) or a bare column. The leading
/// segment is a bare identifier; trailing `.seg`s are struct navigation.
fn dotted_path(input: &mut Stream<'_>) -> ModalResult<Expr> {
    let head = ident(input)?;
    let rest: Vec<Spanned<Ident>> =
        repeat(0.., preceded(punct(Token::Dot), ident)).parse_next(input)?;
    if rest.is_empty() {
        Ok(Expr::Col(head.node))
    } else {
        let mut segs = vec![head.node];
        segs.extend(rest.into_iter().map(|s| s.node));
        Ok(Expr::Path(segs))
    }
}

/// A literal value token → AST literal (RFD §4).
fn literal(input: &mut Stream<'_>) -> ModalResult<Literal> {
    any.verify_map(|t: Spanned<Token>| match t.node {
        Token::Str(s) => Some(Literal::Str(s)),
        Token::Int(i) => Some(Literal::Int(i)),
        Token::Float(f) => Some(Literal::Float(f)),
        Token::Bool(b) => Some(Literal::Bool(b)),
        Token::Null => Some(Literal::Null),
        Token::Size { value, unit } => Some(Literal::Size {
            value,
            unit: unit.text().to_string(),
        }),
        Token::TypedLit { ty, raw } => Some(Literal::Typed {
            ty: lit_type_text(ty).to_string(),
            raw,
        }),
        _ => None,
    })
    .parse_next(input)
}

fn lit_type_text(ty: LitType) -> &'static str {
    ty.text()
}

// ---- projections / assignments / order keys -------------------------------

/// A `SELECT`/`AGGREGATE` projection: `*` or `<expr> [AS <alias>]`.
fn projection(input: &mut Stream<'_>) -> ModalResult<Projection> {
    if opt(punct(Token::Star)).parse_next(input)?.is_some() {
        return Ok(Projection::Star);
    }
    let e = expr(input)?;
    let alias = opt(preceded(kw(Keyword::As), ident))
        .parse_next(input)?
        .map(|s| s.node);
    Ok(Projection::Expr { expr: e, alias })
}

fn projection_list(input: &mut Stream<'_>) -> ModalResult<Vec<Projection>> {
    separated(1.., projection, punct(Token::Comma)).parse_next(input)
}

/// An `EXTEND`/`SET` assignment: `<name> = <expr>`.
fn assignment(input: &mut Stream<'_>) -> ModalResult<Assignment> {
    let name = ident(input)?;
    let _ = punct(Token::Eq).parse_next(input)?;
    let value = expr(input)?;
    Ok(Assignment {
        name: name.node,
        value,
    })
}

fn assignment_list(input: &mut Stream<'_>) -> ModalResult<Vec<Assignment>> {
    separated(1.., assignment, punct(Token::Comma)).parse_next(input)
}

/// An `ORDER BY` key: `<expr> [ASC|DESC]`.
fn order_key(input: &mut Stream<'_>) -> ModalResult<OrderKey> {
    let e = expr(input)?;
    let descending = if opt(word("DESC")).parse_next(input)?.is_some() {
        true
    } else {
        let _ = opt(word("ASC")).parse_next(input)?;
        false
    };
    Ok(OrderKey {
        expr: e,
        descending,
    })
}

// ---- sources --------------------------------------------------------------

/// A pipeline source: `VALUES …`, `( <pipeline> )`, or a `/driver/...` path.
fn source(input: &mut Stream<'_>) -> ModalResult<Source> {
    alt((
        values.map(Source::Values),
        subquery_source,
        path_expr.map(Source::Path),
    ))
    .parse_next(input)
}

fn subquery_source(input: &mut Stream<'_>) -> ModalResult<Source> {
    let _ = punct(Token::LParen).parse_next(input)?;
    let inner = pipeline(input)?;
    let _ = punct(Token::RParen).parse_next(input)?;
    Ok(Source::Subquery(Box::new(inner)))
}

/// `VALUES [(<cols>)] (<row>), (<row>) …` — an inline literal relation.
fn values(input: &mut Stream<'_>) -> ModalResult<Values> {
    let _ = kw(Keyword::Values).parse_next(input)?;
    // Optional column list: a parenthesised list of bare identifiers, only when it
    // is immediately followed by another `(` row.
    let columns = opt(value_column_list).parse_next(input)?;
    // Rows are `(..)` groups, optionally comma-separated. The first row is required;
    // subsequent rows are each preceded by an optional comma (so both `(1)(2)` and
    // `(1),(2)` parse).
    let first = paren_expr_list(input)?;
    let rest: Vec<Vec<Expr>> =
        repeat(0.., preceded(opt(punct(Token::Comma)), paren_expr_list)).parse_next(input)?;
    let mut rows = vec![first];
    rows.extend(rest);
    Ok(Values { columns, rows })
}

/// A `VALUES` column list `(a, b)` that is followed by a row `(` (lookahead). We
/// only treat a leading paren-group as columns when all its members are bare
/// identifiers AND a second `(` (the first row) follows — otherwise the group is
/// itself the first/only row and this parser backtracks. winnow `&[T]` streams are
/// `Copy`, so the post-list cursor is restored after the non-consuming lookahead.
fn value_column_list(input: &mut Stream<'_>) -> ModalResult<Vec<Ident>> {
    let _ = punct(Token::LParen).parse_next(input)?;
    let cols: Vec<Spanned<Ident>> = separated(1.., ident, punct(Token::Comma)).parse_next(input)?;
    let _ = punct(Token::RParen).parse_next(input)?;
    // Non-consuming lookahead for a following row `(`.
    let after_cols = *input;
    if punct(Token::LParen).parse_next(input).is_err() {
        return Err(ErrMode::Backtrack(ContextError::new()));
    }
    *input = after_cols; // restore: the row parser re-consumes the `(`.
    Ok(cols.into_iter().map(|s| s.node).collect())
}

// ---- pipe operations ------------------------------------------------------

fn pipeline(input: &mut Stream<'_>) -> ModalResult<Pipeline> {
    let _ = kw(Keyword::From).parse_next(input)?;
    let source = cut_err(source).parse_next(input)?;
    // Once a `|>` is consumed we are committed to a pipe op: `cut_err` turns an
    // inner failure into a non-backtracking error so the diagnostic points *inside*
    // the op (a dangling `WHERE`, a lowercase keyword) instead of back at the `|>`.
    let ops: Vec<PipeOp> =
        repeat(0.., preceded(punct(Token::Pipe), cut_err(pipe_op))).parse_next(input)?;
    Ok(Pipeline { source, ops })
}

fn pipe_op(input: &mut Stream<'_>) -> ModalResult<PipeOp> {
    alt((
        alt((
            where_op,
            select_op,
            extend_op,
            set_op,
            aggregate_op,
            group_by_op,
            order_by_op,
            limit_op,
            distinct_op,
        )),
        alt((
            join_op,
            union_op,
            except_op,
            intersect_op,
            as_op,
            expand_op,
            decode_op,
            encode_op,
            call_op,
        )),
    ))
    .parse_next(input)
}

fn where_op(input: &mut Stream<'_>) -> ModalResult<PipeOp> {
    preceded(kw(Keyword::Where), cut_err(expr))
        .map(PipeOp::Where)
        .parse_next(input)
}

fn select_op(input: &mut Stream<'_>) -> ModalResult<PipeOp> {
    preceded(kw(Keyword::Select), cut_err(projection_list))
        .map(PipeOp::Select)
        .parse_next(input)
}

fn extend_op(input: &mut Stream<'_>) -> ModalResult<PipeOp> {
    preceded(kw(Keyword::Extend), cut_err(assignment_list))
        .map(PipeOp::Extend)
        .parse_next(input)
}

fn set_op(input: &mut Stream<'_>) -> ModalResult<PipeOp> {
    preceded(kw(Keyword::Set), cut_err(assignment_list))
        .map(PipeOp::Set)
        .parse_next(input)
}

fn aggregate_op(input: &mut Stream<'_>) -> ModalResult<PipeOp> {
    preceded(kw(Keyword::Aggregate), cut_err(projection_list))
        .map(PipeOp::Aggregate)
        .parse_next(input)
}

fn group_by_op(input: &mut Stream<'_>) -> ModalResult<PipeOp> {
    preceded(group_by, separated(1.., expr, punct(Token::Comma)))
        .map(PipeOp::GroupBy)
        .parse_next(input)
}

fn order_by_op(input: &mut Stream<'_>) -> ModalResult<PipeOp> {
    preceded(order_by, separated(1.., order_key, punct(Token::Comma)))
        .map(PipeOp::OrderBy)
        .parse_next(input)
}

fn limit_op(input: &mut Stream<'_>) -> ModalResult<PipeOp> {
    let _ = kw(Keyword::Limit).parse_next(input)?;
    let n = any
        .verify_map(|t: Spanned<Token>| match t.node {
            Token::Int(i) => Some(i),
            _ => None,
        })
        .parse_next(input)?;
    Ok(PipeOp::Limit(n))
}

fn distinct_op(input: &mut Stream<'_>) -> ModalResult<PipeOp> {
    kw(Keyword::Distinct)
        .map(|_| PipeOp::Distinct)
        .parse_next(input)
}

fn join_op(input: &mut Stream<'_>) -> ModalResult<PipeOp> {
    let _ = kw(Keyword::Join).parse_next(input)?;
    let src = source(input)?;
    let _ = kw(Keyword::On).parse_next(input)?;
    let on = expr(input)?;
    Ok(PipeOp::Join(JoinOp { source: src, on }))
}

fn union_op(input: &mut Stream<'_>) -> ModalResult<PipeOp> {
    preceded(kw(Keyword::Union), pipeline)
        .map(|p| PipeOp::Union(Box::new(p)))
        .parse_next(input)
}

fn except_op(input: &mut Stream<'_>) -> ModalResult<PipeOp> {
    preceded(kw(Keyword::Except), pipeline)
        .map(|p| PipeOp::Except(Box::new(p)))
        .parse_next(input)
}

fn intersect_op(input: &mut Stream<'_>) -> ModalResult<PipeOp> {
    preceded(kw(Keyword::Intersect), pipeline)
        .map(|p| PipeOp::Intersect(Box::new(p)))
        .parse_next(input)
}

fn as_op(input: &mut Stream<'_>) -> ModalResult<PipeOp> {
    preceded(kw(Keyword::As), ident)
        .map(|s| PipeOp::As(s.node))
        .parse_next(input)
}

fn expand_op(input: &mut Stream<'_>) -> ModalResult<PipeOp> {
    let _ = kw(Keyword::Expand).parse_next(input)?;
    let field = dotted_path_ref(input)?;
    Ok(PipeOp::Expand(field))
}

/// A dotted path reference `a.b.c` returned as a list of identifiers (for `EXPAND`).
fn dotted_path_ref(input: &mut Stream<'_>) -> ModalResult<PathRef> {
    let head = ident(input)?;
    let rest: Vec<Spanned<Ident>> =
        repeat(0.., preceded(punct(Token::Dot), ident)).parse_next(input)?;
    let mut segs = vec![head.node];
    segs.extend(rest.into_iter().map(|s| s.node));
    Ok(segs)
}

fn decode_op(input: &mut Stream<'_>) -> ModalResult<PipeOp> {
    let _ = kw(Keyword::Decode).parse_next(input)?;
    codec(input).map(PipeOp::Decode)
}

fn encode_op(input: &mut Stream<'_>) -> ModalResult<PipeOp> {
    let _ = kw(Keyword::Encode).parse_next(input)?;
    codec(input).map(PipeOp::Encode)
}

/// A codec format name — the codec registry seam (RFD §4). A bare identifier (string
/// name), resolved later.
fn codec(input: &mut Stream<'_>) -> ModalResult<Codec> {
    let fmt = ident(input)?;
    Ok(Codec {
        fmt: fmt.node,
        span: fmt.span,
    })
}

/// `CALL driver.action(args)` — the procedure registry seam (RFD §3). Shape only;
/// names are strings resolved later (capability gating deferred to E2).
fn call_op(input: &mut Stream<'_>) -> ModalResult<PipeOp> {
    let call_span = kw(Keyword::Call).parse_next(input)?;
    let driver = ident(input)?;
    let _ = punct(Token::Dot).parse_next(input)?;
    let action = ident(input)?;
    let args = opt(named_arg_list).parse_next(input)?.unwrap_or_default();
    let end = action.span.end;
    Ok(PipeOp::Call(CallRef {
        driver: driver.node,
        action: action.node,
        args,
        span: Span::new(call_span.start, end),
    }))
}

/// `( arg, … )` for a `CALL`, each arg positional or `name => value`.
fn named_arg_list(input: &mut Stream<'_>) -> ModalResult<Vec<NamedArg>> {
    let _ = punct(Token::LParen).parse_next(input)?;
    let args: Vec<NamedArg> = separated(0.., named_arg, punct(Token::Comma)).parse_next(input)?;
    let _ = punct(Token::RParen).parse_next(input)?;
    Ok(args)
}

fn named_arg(input: &mut Stream<'_>) -> ModalResult<NamedArg> {
    // Try `name => value` first; backtrack to a positional value otherwise.
    if let Some((name, value)) = opt(named_arg_kv).parse_next(input)? {
        return Ok(NamedArg {
            name: Some(name),
            value,
        });
    }
    let value = expr(input)?;
    Ok(NamedArg { name: None, value })
}

fn named_arg_kv(input: &mut Stream<'_>) -> ModalResult<(Ident, Expr)> {
    let name = ident(input)?;
    let _ = punct(Token::Arrow).parse_next(input)?;
    let value = expr(input)?;
    Ok((name.node, value))
}

// ---- effect statements ----------------------------------------------------

fn effect_stmt(input: &mut Stream<'_>) -> ModalResult<EffectStmt> {
    alt((insert_stmt, upsert_stmt, update_stmt, remove_stmt)).parse_next(input)
}

fn insert_stmt(input: &mut Stream<'_>) -> ModalResult<EffectStmt> {
    let _ = insert_into(input)?;
    write_target(input, EffectVerb::Insert)
}

fn upsert_stmt(input: &mut Stream<'_>) -> ModalResult<EffectStmt> {
    let _ = upsert_into(input)?;
    write_target(input, EffectVerb::Upsert)
}

/// Shared tail for `INSERT INTO`/`UPSERT INTO`: `<path> ( VALUES… | <pipeline> )
/// [RETURNING …]`.
fn write_target(input: &mut Stream<'_>, verb: EffectVerb) -> ModalResult<EffectStmt> {
    let target = path_expr(input)?;
    let body = alt((
        values.map(EffectBody::Values),
        pipeline.map(|p| EffectBody::Pipeline(Box::new(p))),
    ))
    .parse_next(input)?;
    let returning = opt(returning_clause).parse_next(input)?;
    Ok(EffectStmt {
        verb,
        target,
        body,
        returning,
    })
}

fn update_stmt(input: &mut Stream<'_>) -> ModalResult<EffectStmt> {
    let _ = kw(Keyword::Update).parse_next(input)?;
    let target = path_expr(input)?;
    let _ = kw(Keyword::Set).parse_next(input)?;
    let set = assignment_list(input)?;
    let filter = opt(preceded(kw(Keyword::Where), expr)).parse_next(input)?;
    let returning = opt(returning_clause).parse_next(input)?;
    Ok(EffectStmt {
        verb: EffectVerb::Update,
        target,
        body: EffectBody::SetWhere { set, filter },
        returning,
    })
}

fn remove_stmt(input: &mut Stream<'_>) -> ModalResult<EffectStmt> {
    let _ = kw(Keyword::Remove).parse_next(input)?;
    let target = path_expr(input)?;
    let filter = opt(preceded(kw(Keyword::Where), expr)).parse_next(input)?;
    let returning = opt(returning_clause).parse_next(input)?;
    Ok(EffectStmt {
        verb: EffectVerb::Remove,
        target,
        body: EffectBody::SetWhere {
            set: Vec::new(),
            filter,
        },
        returning,
    })
}

fn returning_clause(input: &mut Stream<'_>) -> ModalResult<Vec<Projection>> {
    preceded(kw(Keyword::Returning), projection_list).parse_next(input)
}

// ---- server DDL -----------------------------------------------------------

fn server_ddl(input: &mut Stream<'_>) -> ModalResult<ServerDdl> {
    let _ = kw(Keyword::Create).parse_next(input)?;
    let kind = ddl_kind(input)?;
    let name = ident(input)?;
    // Clause grammar is permissive (sugar shape, not full validation): collect the
    // optional ON / EVERY / AS / DO clauses in any order.
    let mut on = None;
    let mut every = None;
    let mut as_query = None;
    let mut do_plan = None;
    loop {
        if on.is_none() {
            if let Some(v) = opt(on_clause).parse_next(input)? {
                on = Some(v);
                continue;
            }
        }
        if every.is_none() {
            if let Some(v) = opt(every_clause).parse_next(input)? {
                every = Some(v);
                continue;
            }
        }
        if as_query.is_none() {
            if let Some(v) = opt(as_clause).parse_next(input)? {
                as_query = Some(Box::new(v));
                continue;
            }
        }
        if do_plan.is_none() {
            if let Some(v) = opt(do_clause).parse_next(input)? {
                do_plan = Some(Box::new(v));
                continue;
            }
        }
        break;
    }
    let target = vec![
        "server".to_string(),
        ddl_kind_segment(kind).to_string(),
        name.node.clone(),
    ];
    Ok(ServerDdl {
        kind,
        name: name.node,
        target,
        do_plan,
        as_query,
        every,
        on,
    })
}

fn ddl_kind(input: &mut Stream<'_>) -> ModalResult<DdlKind> {
    alt((
        kw(Keyword::Endpoint).map(|_| DdlKind::Endpoint),
        kw(Keyword::Trigger).map(|_| DdlKind::Trigger),
        kw(Keyword::Job).map(|_| DdlKind::Job),
        materialized_view.map(|()| DdlKind::MaterializedView),
        kw(Keyword::View).map(|_| DdlKind::View),
        kw(Keyword::Webhook).map(|_| DdlKind::Webhook),
        kw(Keyword::Policy).map(|_| DdlKind::Policy),
    ))
    .parse_next(input)
}

/// `MATERIALIZED VIEW` — `MATERIALIZED` is an UPPERCASE ident the lexer leaves bare,
/// followed by the `VIEW` keyword.
fn materialized_view(input: &mut Stream<'_>) -> ModalResult<()> {
    let _ = word("MATERIALIZED").parse_next(input)?;
    let _ = kw(Keyword::View).parse_next(input)?;
    Ok(())
}

fn ddl_kind_segment(kind: DdlKind) -> &'static str {
    match kind {
        DdlKind::Endpoint => "endpoints",
        DdlKind::Trigger => "triggers",
        DdlKind::Job => "jobs",
        DdlKind::View => "views",
        DdlKind::MaterializedView => "materialized_views",
        DdlKind::Webhook => "webhooks",
        DdlKind::Policy => "policies",
    }
}

/// `ON <event>` — the event/route token captured as raw text (an identifier, path,
/// or string). Stored unparsed (sugar; downstream desugars).
fn on_clause(input: &mut Stream<'_>) -> ModalResult<String> {
    let _ = kw(Keyword::On).parse_next(input)?;
    raw_token_text(input)
}

/// `EVERY <interval>` — interval captured as raw text.
fn every_clause(input: &mut Stream<'_>) -> ModalResult<String> {
    let _ = kw(Keyword::Every).parse_next(input)?;
    raw_token_text(input)
}

/// `AS <statement>` — the backing query for an ENDPOINT/VIEW.
fn as_clause(input: &mut Stream<'_>) -> ModalResult<Statement> {
    preceded(kw(Keyword::As), inner_statement).parse_next(input)
}

/// `DO <statement>` — the effect-plan body for a TRIGGER/JOB.
fn do_clause(input: &mut Stream<'_>) -> ModalResult<Statement> {
    preceded(kw(Keyword::Do), inner_statement).parse_next(input)
}

/// Capture a single token's raw textual form (for `ON`/`EVERY` operands). Names a
/// kind for non-textual tokens to avoid leaking literal values is unnecessary here
/// because these operands are routes/intervals, not credentials.
fn raw_token_text(input: &mut Stream<'_>) -> ModalResult<String> {
    any.verify_map(|t: Spanned<Token>| match t.node {
        Token::Ident(s) => Some(s),
        Token::Str(s) => Some(s),
        Token::Int(i) => Some(i.to_string()),
        Token::Size { value, unit } => Some(format!("{value} {}", unit.text())),
        Token::Path(segs) => Some(
            segs.into_iter()
                .map(|s| s.name)
                .collect::<Vec<_>>()
                .join("/"),
        ),
        _ => None,
    })
    .parse_next(input)
}

// ---- statement top level --------------------------------------------------

/// An inner statement (no trailing-EOF requirement) — used for nested `AS`/`DO`
/// clauses and `PREVIEW`/`COMMIT` wrappers.
fn inner_statement(input: &mut Stream<'_>) -> ModalResult<Statement> {
    alt((
        plan_wrap,
        server_ddl.map(Statement::Ddl),
        effect_stmt.map(Statement::Effect),
        pipeline.map(Statement::Query),
    ))
    .parse_next(input)
}

/// `PREVIEW <stmt>` / `COMMIT <stmt>` — the plan wrapper (RFD §6).
fn plan_wrap(input: &mut Stream<'_>) -> ModalResult<Statement> {
    let (commit, span) = alt((
        kw(Keyword::Preview).map(|s| (false, s)),
        kw(Keyword::Commit).map(|s| (true, s)),
    ))
    .parse_next(input)?;
    let inner = inner_statement(input)?;
    Ok(Statement::Plan(PlanWrap {
        commit,
        inner: Box::new(inner),
        span,
    }))
}

/// The top-level statement parser: one statement, then end-of-input.
fn statement(input: &mut Stream<'_>) -> ModalResult<Statement> {
    let stmt = inner_statement(input)?;
    winnow::combinator::eof(input)?;
    Ok(stmt)
}
