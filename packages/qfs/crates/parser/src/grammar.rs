//! Internal winnow grammar (t04, the full RFD §3 pipe-SQL grammar). **Crate-private**
//! — winnow types never escape this module; [`parse`] returns the owned
//! [`crate::ParseError`] (fidelity guard G6).
//!
//! ## Token-stream input (t03 → t04)
//! Unlike the E0 spike (which parsed `&str` directly), this grammar consumes the
//! **t03 token stream**: `qfs_lang::lex` produces `Vec<Spanned<Token>>`, and winnow's
//! built-in `&[T]` [`winnow::stream::Stream`] impl drives the combinators over that
//! slice. Each AST node re-spans itself from the byte span carried by its tokens, so
//! diagnostics round-trip to source (RFD §5/§10). The lexer already folds path
//! `@version`, size/typed literals, and operators; this module stitches multi-word
//! keywords (`GROUP BY`, `INSERT INTO`, …), which the lexer emits as adjacent tokens.
//!
//! ## Closed core, structurally (RFD §3)
//! Keyword surface comes from the frozen `qfs_lang::Keyword` set — there is no second
//! transcription. The grammar rejects unknown core constructs (incomplete multi-word keywords,
//! reserved-word-as-identifier) but leaves the three registry seams open: `CALL
//! driver.action`, `fn(...)`, `DECODE/ENCODE fmt`, and `/driver/...` paths all parse
//! into string-named reference nodes without resolving the names.
//!
//! ## Panic-free
//! The workspace `unwrap/expect/panic = deny` lint applies (NOT relaxed here, unlike
//! the spike). Every combinator returns a `Result`; the boundary mapper turns a
//! winnow failure into a structured error without ever indexing-panicking.

use qfs_lang::token::{LitType, PathSeg};
use qfs_lang::{lex, Keyword, Span, Spanned, Token};
use winnow::combinator::{alt, cut_err, opt, preceded, repeat, separated};
use winnow::error::{ContextError, ErrMode, ParseError as WinnowParseError};
use winnow::token::any;
use winnow::{ModalResult, Parser};

use crate::ast::{
    Assignment, CallRef, Codec, ConnectionDeclAst, DdlKind, EffectBody, EffectStmt, EffectVerb,
    Expr, FnRef, Ident, JoinOp, Literal, NamedArg, Op, OrderKey, Param, PathExpr, PathRef,
    PathSegment, PipeOp, Pipeline, PlanWrap, PolicyRuleAst, PolicySubjectAst, Projection,
    ServerDdl, Source, Statement, TypeAnn, Values,
};
use crate::error::{ParseError, ParseErrorCode};

/// The parser input stream: a slice of spanned tokens (winnow drives this directly).
type Stream<'a> = &'a [Spanned<Token>];
/// The winnow modal error used internally; never escapes this module.
type Err = ErrMode<ContextError>;

/// Parse one qfs statement from source text into the owned [`Statement`] AST.
///
/// Lexes via `qfs_lang::lex`, then runs the winnow grammar over the token slice,
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
fn lex_to_parse_error(_input: &str, e: &qfs_lang::LexError) -> ParseError {
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

/// Classify the offending token into a structured code + message. Keyword-shaped
/// identifiers are flagged distinctly (an incomplete multi-word keyword like `group`
/// without `by`, or a keyword used where the grammar wants a name); reserved keywords
/// in identifier position are flagged too. Keyword recognition is case-insensitive
/// (t74, decision S), so this is no longer a *case* mistake.
fn classify(tok: &Token) -> (ParseErrorCode, String) {
    match tok {
        Token::Ident(s) if is_keyword_word(s) => (
            ParseErrorCode::UnknownKeyword,
            "closed-core keywords are lowercase (recognized case-insensitively); this keyword \
             is not valid here (RFD §3, decision S)"
                .to_string(),
        ),
        Token::Keyword(_) => (
            ParseErrorCode::ReservedAsIdentifier,
            "a reserved keyword cannot be used here".to_string(),
        ),
        // A lone `=` where the grammar wanted a comparison or pipe boundary is almost
        // always a stale SQL-style equality. Steer to `==` (RFD decision O, t70): `=`
        // binds (assignment / named-arg / SET), `==` compares.
        Token::Eq => (
            ParseErrorCode::UnexpectedToken,
            "`=` binds (assignment); use `==` for equivalence (RFD decision O)".to_string(),
        ),
        _ => (
            ParseErrorCode::UnexpectedToken,
            "the grammar did not expect this token here".to_string(),
        ),
    }
}

/// Whether `s` is a closed-core keyword word (case-insensitive, t74) — a single-word
/// keyword, or a multi-word keyword lead/tail / contextual sub-word the lexer leaves
/// as a bare identifier (`group`, `by`, `into`, `materialized`, …). Used to give a
/// crisp diagnostic when such a word stands where a name/stage was expected (e.g. a
/// `group` with no `by`).
fn is_keyword_word(s: &str) -> bool {
    Keyword::from_word(s).is_some()
        || matches!(
            s.to_ascii_uppercase().as_str(),
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
        "a source path".to_string(),
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
        Token::Eq => "`=`".to_string(),
        Token::EqEq | Token::Ne | Token::Lt | Token::Gt | Token::Le | Token::Ge | Token::Tilde => {
            "an operator".to_string()
        }
        Token::LParen => "`(`".to_string(),
        Token::RParen => "`)`".to_string(),
        Token::Comma => "`,`".to_string(),
        Token::Colon => "`:`".to_string(),
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

/// Match an identifier word equal to `word`, **case-insensitively** (t74, decision S):
/// used for multi-word keyword tails/leads and the word operators / DDL sub-keywords the
/// lexer leaves as identifiers, e.g. `by`, `into`, `materialized`, `and`, `or`, `of`,
/// `asc`, `desc`. The `word` argument is written UPPERCASE by convention; recognition
/// folds case so `INSERT`/`Insert`/`insert` all match — keeping the closed core lowercase
/// (canonical) while accepting any case.
fn word<'a>(word: &'static str) -> impl Parser<Stream<'a>, Span, Err> {
    any.verify_map(move |t: Spanned<Token>| match t.node {
        Token::Ident(ref s) if s.eq_ignore_ascii_case(word) => Some(t.span),
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

/// Multi-word keyword: `group by` (= `group` ident + `by` ident, case-insensitive).
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
        // Equivalence is `==` (RFD decision O, t70). A lone `=` is the binding token
        // and is intentionally NOT a comparator here — it surfaces as a crisp error
        // steering to `==` (see `classify`).
        Token::EqEq => Some(Op::Eq),
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

/// A primary expression: literal, lambda, parenthesised expr, `*`, function call,
/// dotted path / column.
///
/// `lambda` is tried **before** `paren_expr`: both start with `(`, but only a lambda has
/// the trailing `=> <body>` after the parameter list. The lambda parser backtracks
/// cleanly when the `( … )` is not followed by `=>`, so a plain parenthesised expression
/// `(a == b)` still parses as `paren_expr`.
fn primary(input: &mut Stream<'_>) -> ModalResult<Expr> {
    alt((
        literal.map(Expr::Lit),
        lambda,
        paren_expr,
        fn_call,
        dotted_path,
    ))
    .parse_next(input)
}

/// A lambda literal `( params ) => <expr>` — a first-class value (M6 ticket t61,
/// decision H "functions are values"). **No keyword is added**: the form rides the
/// expression grammar and reuses the existing `=>` arrow token (also used by named call
/// args); the parenthesised parameter list is what distinguishes a lambda from a
/// named-arg pair or a parenthesised sub-expression.
///
/// The whole production is backtrackable up to the `=>`: if a `( … )` group is **not**
/// followed by `=>` it is not a lambda, so `opt` lets the enclosing `alt` fall through to
/// `paren_expr`. Once `=>` is seen the body is parsed as a full expression (a lambda body
/// is expression-only — a lambda is a pure transformation, it can name no effect, RFD §3).
fn lambda(input: &mut Stream<'_>) -> ModalResult<Expr> {
    let _ = punct(Token::LParen).parse_next(input)?;
    let params: Vec<Param> = separated(0.., lambda_param, punct(Token::Comma)).parse_next(input)?;
    let _ = punct(Token::RParen).parse_next(input)?;
    // The arrow is the commit point: only here do we know this `( … )` is a lambda and
    // not a parenthesised expression. `opt` (no `cut_err`) lets a non-lambda backtrack.
    let _ = punct(Token::Arrow).parse_next(input)?;
    let body = expr(input)?;
    Ok(Expr::Lambda {
        params,
        body: Box::new(body),
    })
}

/// One lambda parameter: a bare name with an optional `: <type>` annotation. The
/// annotation is parsed-and-retained (`Option<TypeAnn>`); the type name is any bare
/// identifier (the canonical surface is a lowercase primitive `string`/`bool`/`i64`, but
/// the grammar accepts any ident so the annotation round-trips into the later type-system
/// ticket, t75).
fn lambda_param(input: &mut Stream<'_>) -> ModalResult<Param> {
    let name = ident(input)?;
    let ty = opt(preceded(punct(Token::Colon), ident))
        .parse_next(input)?
        .map(|t| TypeAnn { name: t.node });
    Ok(Param {
        name: name.node,
        ty,
    })
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

/// A pipeline source: `VALUES …`, `( <pipeline> )`, a `/driver/...` path, or a bare
/// identifier naming a `LET`-bound relation (M6, ticket t60). The bare-identifier form is
/// tried **last** so it never shadows a keyword/path/values form; a reserved keyword is
/// already a `Token::Keyword` (not an `Ident`), so it cannot be mistaken for a name source.
fn source(input: &mut Stream<'_>) -> ModalResult<Source> {
    alt((
        values.map(Source::Values),
        subquery_source,
        path_expr.map(Source::Path),
        ident.map(|s| Source::Name(s.node)),
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

/// A **column name** in a name-only position: a bare identifier, or a reserved keyword
/// spelled as a name. Keywords are reserved (t74, case-insensitive), but a column name in
/// a `VALUES (…)` list is unambiguous, so a keyword token contributes its canonical
/// lowercase text as the name. This keeps schema fields whose names collide with keyword
/// words usable without quoting — e.g. the `/server/jobs` field `every` and the
/// `/server/triggers` field `on` — which decision S's lowercase keyword set would
/// otherwise shadow. It is a pure surface accommodation: no effect, no new vocabulary.
fn column_name(input: &mut Stream<'_>) -> ModalResult<Spanned<Ident>> {
    any.verify_map(|t: Spanned<Token>| match t.node {
        Token::Ident(s) => Some(Spanned::new(s, t.span)),
        Token::Keyword(k) => Some(Spanned::new(k.text().to_string(), t.span)),
        _ => None,
    })
    .parse_next(input)
}

/// A `VALUES` column list `(a, b)` that is followed by a row `(` (lookahead). We
/// only treat a leading paren-group as columns when all its members are column
/// names ([`column_name`]: a bare identifier or a keyword-in-name-position) AND a
/// second `(` (the first row) follows — otherwise the group is itself the first/only
/// row and this parser backtracks. winnow `&[T]` streams are `Copy`, so the post-list
/// cursor is restored after the non-consuming lookahead.
fn value_column_list(input: &mut Stream<'_>) -> ModalResult<Vec<Ident>> {
    let _ = punct(Token::LParen).parse_next(input)?;
    let cols: Vec<Spanned<Ident>> =
        separated(1.., column_name, punct(Token::Comma)).parse_next(input)?;
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
    // Decision R (t73): the source position needs no `FROM`. A leading `/path`, a `(subquery)`,
    // a `VALUES …` literal, or a `LET`-bound name *is* the source. The leading `/` is unambiguous
    // by position (an expression-start `/` is a path; the lexer never emits a division `/`), so
    // this composes with future infix arithmetic. The source is parsed backtrackably so a non-
    // pipeline statement (an effect/DDL/plan wrapper) can still win in the enclosing `alt`.
    let source = source(input)?;
    // Once a `|>` is consumed we are committed to a pipe op: `cut_err` turns an
    // inner failure into a non-backtracking error so the diagnostic points *inside*
    // the op (a dangling `where`, an incomplete multi-word keyword) instead of back at the `|>`.
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

// ---- writes as pipeline stages (decision Q, t72) --------------------------

/// The head of a terminal **pipe-stage write** (decision Q, t72): the write verb plus its
/// *own* operands — a target path for `INSERT INTO`/`UPSERT INTO`, the `SET` assignments for
/// `UPDATE`, nothing for `REMOVE`. The rows the write CONSUMES are the upstream pipeline (the
/// relation flowing into the stage), supplied separately by [`build_pipe_effect`]. Kept as a
/// small enum so the upstream → [`EffectStmt`] lowering is shared with — and byte-for-byte
/// identical to — the verb-leading [`effect_stmt`] form: two spellings, one plan.
enum PipeWriteHead {
    /// `|> INSERT INTO <target>` — the upstream is the inserted relation.
    Insert(PathExpr),
    /// `|> UPSERT INTO <target>` — the upstream is the upserted relation.
    Upsert(PathExpr),
    /// `|> UPDATE SET <assignments>` — target/filter are lifted from the upstream.
    Update(Vec<Assignment>),
    /// `|> REMOVE` — target/filter are lifted from the upstream.
    Remove,
}

/// Parse a terminal write stage's head after the `|>` has been consumed. Each arm backtracks
/// cleanly on its leading verb (so a non-write stage falls through to the ordinary
/// [`pipe_op`]); once a verb is committed the remainder is `cut_err` so a malformed write is a
/// crisp error pointing *inside* the stage, not a silent fallthrough.
fn pipe_write_head(input: &mut Stream<'_>) -> ModalResult<PipeWriteHead> {
    alt((
        preceded(insert_into, cut_err(path_expr)).map(PipeWriteHead::Insert),
        preceded(upsert_into, cut_err(path_expr)).map(PipeWriteHead::Upsert),
        preceded(
            kw(Keyword::Update),
            cut_err(preceded(kw(Keyword::Set), assignment_list)),
        )
        .map(PipeWriteHead::Update),
        kw(Keyword::Remove).map(|_| PipeWriteHead::Remove),
    ))
    .parse_next(input)
}

/// Lower a terminal pipe-stage write into the SAME [`EffectStmt`] the verb-leading
/// [`effect_stmt`] builds (decision Q: `… |> INSERT INTO /b` and `INSERT INTO /b …` are two
/// renderings of one effect, so they MUST produce one plan). `source`/`ops` are the upstream
/// relation the write consumes; an optional trailing `|> RETURNING …` stage (the pipe-stage
/// spelling of the verb-leading inline `RETURNING`) carries the projection.
fn build_pipe_effect(
    input: &mut Stream<'_>,
    head: PipeWriteHead,
    source: Source,
    ops: Vec<PipeOp>,
) -> ModalResult<EffectStmt> {
    // `RETURNING` rides as its own trailing `|>` stage in the pipe-stage form (the verb-leading
    // form takes it inline); both populate the same `EffectStmt.returning`. `opt` checkpoints,
    // so a missing `|> RETURNING` restores the cursor for the enclosing `eof`/clause.
    let returning = opt(preceded(
        punct(Token::Pipe),
        preceded(kw(Keyword::Returning), projection_list),
    ))
    .parse_next(input)?;
    match head {
        PipeWriteHead::Insert(target) => Ok(EffectStmt {
            verb: EffectVerb::Insert,
            target,
            body: EffectBody::Pipeline(Box::new(Pipeline { source, ops })),
            returning,
        }),
        PipeWriteHead::Upsert(target) => Ok(EffectStmt {
            verb: EffectVerb::Upsert,
            target,
            body: EffectBody::Pipeline(Box::new(Pipeline { source, ops })),
            returning,
        }),
        PipeWriteHead::Update(set) => {
            let (target, filter) = lift_target_filter(source, ops)?;
            Ok(EffectStmt {
                verb: EffectVerb::Update,
                target,
                body: EffectBody::SetWhere { set, filter },
                returning,
            })
        }
        PipeWriteHead::Remove => {
            let (target, filter) = lift_target_filter(source, ops)?;
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
    }
}

/// A pipe-stage `UPDATE`/`REMOVE` rewrites rows *in place* on its upstream mount, so the
/// upstream must be a bare `/path` optionally narrowed by `WHERE` predicates — precisely what
/// the verb-leading `UPDATE <path> … WHERE …` / `REMOVE <path> WHERE …` expresses. Lift the
/// path as the target and AND-fold the `WHERE`s into the one filter, so the two spellings lower
/// identically. Any other upstream shape (a `SELECT`/`JOIN`/codec stage, a `VALUES`/subquery/
/// name source) cannot lower to that form and is a crisp error — decision Q: anything that is
/// neither legal form is rejected, never silently accepted.
fn lift_target_filter(source: Source, ops: Vec<PipeOp>) -> ModalResult<(PathExpr, Option<Expr>)> {
    let Source::Path(target) = source else {
        return Err(ErrMode::Cut(ContextError::new()));
    };
    let mut filter: Option<Expr> = None;
    for op in ops {
        let PipeOp::Where(pred) = op else {
            return Err(ErrMode::Cut(ContextError::new()));
        };
        filter = Some(match filter {
            None => pred,
            Some(prev) => Expr::Binary {
                op: Op::And,
                lhs: Box::new(prev),
                rhs: Box::new(pred),
            },
        });
    }
    Ok((target, filter))
}

/// A statement-position pipeline that may terminate in a **write stage** (decision Q, t72).
/// Without a terminal write it is an ordinary read [`Statement::Query`]; with one it lowers to
/// the SAME [`Statement::Effect`] the verb-leading [`effect_stmt`] builds. The write stage is
/// recognised ONLY here (statement position) — never inside a `(subquery)`, a `JOIN`/`UNION`
/// arm, or a `LET` value, all of which keep using the pure [`pipeline`] — so a write can never
/// hide in a read context and the §3 safety floor holds structurally.
fn pipeline_or_effect(input: &mut Stream<'_>) -> ModalResult<Statement> {
    let source = source(input)?;
    let mut ops: Vec<PipeOp> = Vec::new();
    let mut write: Option<PipeWriteHead> = None;
    loop {
        // A `|>` boundary. `&[T]` streams are `Copy`, so a non-`|>` token restores the cursor
        // for the enclosing parser (the `eof`, or a `LET` body / DDL clause that follows).
        let checkpoint = *input;
        if punct(Token::Pipe).parse_next(input).is_err() {
            *input = checkpoint;
            break;
        }
        // A terminal write verb wins over a normal op; a non-write stage backtracks (its leading
        // verb never matched) and falls through to the ordinary, cut-on-commit pipe op.
        if let Some(head) = opt(pipe_write_head).parse_next(input)? {
            write = Some(head);
            break;
        }
        ops.push(cut_err(pipe_op).parse_next(input)?);
    }
    match write {
        Some(head) => build_pipe_effect(input, head, source, ops).map(Statement::Effect),
        None => Ok(Statement::Query(Pipeline { source, ops })),
    }
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
    let mut where_pred = None;
    let mut do_plan = None;
    let mut policy_rules: Vec<PolicyRuleAst> = Vec::new();
    let mut policy_attach: Option<String> = None;
    let mut driver: Option<String> = None;
    let mut at_locator: Option<String> = None;
    let mut secret_ref: Option<String> = None;
    loop {
        // `CREATE CONNECTION <name> DRIVER <driver> [AT '<loc>'] [SECRET '<ref>']` — the
        // connection-declaration clauses (contextual idents, no frozen keyword). Probed first for
        // the CONNECTION form so `DRIVER`/`SECRET` are consumed before the generic clause probes.
        if matches!(kind, DdlKind::Connection) {
            if driver.is_none() {
                if let Some(v) = opt(conn_driver_clause).parse_next(input)? {
                    driver = Some(v);
                    continue;
                }
            }
            if at_locator.is_none() {
                if let Some(v) = opt(conn_at_clause).parse_next(input)? {
                    at_locator = Some(v);
                    continue;
                }
            }
            if secret_ref.is_none() {
                if let Some(v) = opt(conn_secret_clause).parse_next(input)? {
                    secret_ref = Some(v);
                    continue;
                }
            }
        }
        // The `POLICY <name>` ATTACHMENT clause (t35) on a binding DDL — the policy a fired
        // plan commits under. `POLICY` is a frozen keyword (no new keyword). Only on a
        // non-POLICY DDL (for `CREATE POLICY` the leading POLICY is the kind, not an attach).
        if !matches!(kind, DdlKind::Policy) && policy_attach.is_none() {
            if let Some(v) = opt(policy_attach_clause).parse_next(input)? {
                policy_attach = Some(v);
                continue;
            }
        }
        // `CREATE POLICY … ALLOW … DENY …` rule clauses (t35). Parsed FIRST for the POLICY
        // form so the `ALLOW`/`DENY` contextual idents (not frozen keywords) are consumed
        // before the generic clause probes. A rule may carry its own `ON <driver-glob>`, so
        // this must win over the generic `on_clause` inside the POLICY form.
        if matches!(kind, DdlKind::Policy) {
            if let Some(rule) = opt(policy_rule_clause).parse_next(input)? {
                policy_rules.push(rule);
                continue;
            }
        }
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
        if where_pred.is_none() {
            if let Some(v) = opt(ddl_where_clause).parse_next(input)? {
                where_pred = Some(Box::new(v));
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
        where_pred,
        every,
        on,
        policy_rules,
        policy: policy_attach,
        connection: (driver.is_some() || at_locator.is_some() || secret_ref.is_some()).then(|| {
            Box::new(ConnectionDeclAst {
                driver,
                at_locator,
                secret_ref,
            })
        }),
    })
}

/// `POLICY <name>` — the binding attachment clause (t35): the `/server/policies` row a fired
/// plan commits under. `POLICY` IS a frozen keyword; the name is a bare identifier.
fn policy_attach_clause(input: &mut Stream<'_>) -> ModalResult<String> {
    let _ = kw(Keyword::Policy).parse_next(input)?;
    ident(input).map(|s| s.node)
}

/// `(ALLOW|DENY) <verbs> [ON <driver-glob>]` — one `CREATE POLICY` rule clause (t35).
///
/// ## Keyword-freeze (the t31 `AT` lesson)
/// `ALLOW`/`DENY`/`ALL` are **NOT** in the frozen RFD §3 keyword table; only `POLICY`/`ON` and
/// the verbs (`SELECT`/`UPDATE`/`REMOVE`/`CALL` as keywords; `INSERT`/`UPSERT` as the
/// `INTO`-lead idents) are frozen. So this binds over the **existing surface**: `ALLOW`/`DENY`/
/// `ALL` are matched as contextual identifiers ([`word`], case-insensitive) — adding no new closed-core
/// keyword — exactly as t31 bound `AT` and the DDL handles `MATERIALIZED`.
fn policy_rule_clause(input: &mut Stream<'_>) -> ModalResult<PolicyRuleAst> {
    let allow =
        alt((word("ALLOW").map(|_| true), word("DENY").map(|_| false))).parse_next(input)?;
    let (verbs, all_token) = policy_verb_list(input)?;
    // The optional per-rule `ON <driver-glob>` scope (`ON` IS a frozen keyword).
    let driver = opt(preceded(kw(Keyword::On), raw_token_text)).parse_next(input)?;
    // t57: the optional richer axes, collected in any order (sugar shape, like the DDL clauses).
    // `FOR`/`AT` are contextual UPPERCASE idents (no new keyword, the t31 `AT` lesson); `WHERE` is
    // a frozen keyword whose body is an ORDINARY expression (`member_of(...)` via `Expr::Fn`).
    let mut subject = None;
    let mut scope = None;
    let mut condition = None;
    loop {
        if subject.is_none() {
            if let Some(v) = opt(policy_for_clause).parse_next(input)? {
                subject = Some(v);
                continue;
            }
        }
        if scope.is_none() {
            if let Some(v) = opt(policy_at_clause).parse_next(input)? {
                scope = Some(v);
                continue;
            }
        }
        if condition.is_none() {
            if let Some(v) = opt(policy_where_clause).parse_next(input)? {
                condition = Some(v);
                continue;
            }
        }
        break;
    }
    Ok(PolicyRuleAst {
        allow,
        verbs,
        all_token,
        driver,
        subject,
        scope,
        condition,
    })
}

/// `FOR (user|role|group) <name>` — the optional t57 actor clause. `FOR` and the kind words are
/// contextual UPPERCASE idents (matched case-insensitively via [`word`]), so this adds NO frozen
/// keyword (the t31 `AT` lesson). The name is a bare identifier.
fn policy_for_clause(input: &mut Stream<'_>) -> ModalResult<PolicySubjectAst> {
    let _ = word("FOR").parse_next(input)?;
    let kind = alt((
        word("USER").map(|_| "user"),
        word("ROLE").map(|_| "role"),
        word("GROUP").map(|_| "group"),
    ))
    .parse_next(input)?;
    let name = ident(input)?;
    Ok(PolicySubjectAst {
        kind: kind.to_string(),
        name: name.node,
    })
}

/// `AT <path-glob>` — the optional t57 realm-scoped path clause. `AT` is a contextual ident (no
/// new keyword); the glob is captured as raw path text (`/members/alice/**`).
fn policy_at_clause(input: &mut Stream<'_>) -> ModalResult<String> {
    let _ = word("AT").parse_next(input)?;
    raw_token_text(input)
}

/// `WHERE <expr>` — the optional t57 conditional grant. `WHERE` IS a frozen keyword; the body is
/// an ordinary expression (a `member_of('/directories/...')` call — the `Expr::Fn` "functions are
/// values" seam), so NO keyword is added for the predicate itself.
fn policy_where_clause(input: &mut Stream<'_>) -> ModalResult<Expr> {
    let _ = kw(Keyword::Where).parse_next(input)?;
    expr(input)
}

/// A POLICY rule's verb list: the bare `ALL` token, or a comma-separated list of verbs. The
/// verbs span both lexer shapes — `SELECT`/`UPDATE`/`REMOVE`/`CALL` are reserved keyword
/// tokens, while `INSERT`/`UPSERT` are the bare `INTO`-lead idents (case-insensitive) — so this accepts
/// either. Returns `(verb_labels, was_all_token)`.
fn policy_verb_list(input: &mut Stream<'_>) -> ModalResult<(Vec<String>, bool)> {
    if opt(word("ALL")).parse_next(input)?.is_some() {
        return Ok((vec!["ALL".to_string()], true));
    }
    let mut verbs = vec![policy_verb_token(input)?];
    while opt(punct(Token::Comma)).parse_next(input)?.is_some() {
        verbs.push(policy_verb_token(input)?);
    }
    Ok((verbs, false))
}

/// A single POLICY verb token: a reserved verb keyword (`SELECT`/`UPDATE`/`REMOVE`/`CALL`) or a
/// bare verb ident (`insert`/`upsert`, case-insensitive). Returns the canonical uppercase label.
fn policy_verb_token(input: &mut Stream<'_>) -> ModalResult<String> {
    any.verify_map(|t: Spanned<Token>| match t.node {
        Token::Keyword(Keyword::Select) => Some("SELECT".to_string()),
        Token::Keyword(Keyword::Update) => Some("UPDATE".to_string()),
        Token::Keyword(Keyword::Remove) => Some("REMOVE".to_string()),
        Token::Keyword(Keyword::Call) => Some("CALL".to_string()),
        // `insert`/`upsert` are the multi-word verb leads (bare idents), matched
        // case-insensitively (t74) and normalized to the canonical uppercase label.
        Token::Ident(ref s) if s.eq_ignore_ascii_case("INSERT") => Some("INSERT".to_string()),
        Token::Ident(ref s) if s.eq_ignore_ascii_case("UPSERT") => Some("UPSERT".to_string()),
        _ => None,
    })
    .parse_next(input)
}

/// `WHERE <pred>` — the optional TRIGGER guard (t34). `WHERE` is a frozen keyword, so this adds no
/// new keyword. The predicate expression is wrapped as a `Statement::Query` over an EMPTY `VALUES`
/// source plus a single `PipeOp::Where(pred)`, so it round-trips through the downstream
/// `StatementSpec` (serde over the AST) with no new node kind: the dispatcher (t34) extracts the
/// `Where` op's `Expr` and evaluates it over `NEW.*`. The empty source is a structural carrier only
/// — it is never read; the dispatcher binds `NEW.*` into the predicate before evaluating it.
fn ddl_where_clause(input: &mut Stream<'_>) -> ModalResult<Statement> {
    let _ = kw(Keyword::Where).parse_next(input)?;
    let pred = expr(input)?;
    Ok(Statement::Query(Pipeline {
        source: Source::Values(Values {
            columns: None,
            rows: Vec::new(),
        }),
        ops: vec![PipeOp::Where(pred)],
    }))
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
        // `CONNECTION` is a contextual ident (no frozen keyword), like `materialized`.
        word("CONNECTION").map(|_| DdlKind::Connection),
    ))
    .parse_next(input)
}

/// `DRIVER <driver>` — the connection's driver kind (a bare ident). `DRIVER` is a contextual ident.
fn conn_driver_clause(input: &mut Stream<'_>) -> ModalResult<String> {
    let _ = word("DRIVER").parse_next(input)?;
    ident(input).map(|s| s.node)
}

/// `AT '<locator>'` — the connection's non-secret location, a quoted string (consistent with the
/// rest of the grammar's literal locators). `AT` is a contextual ident.
fn conn_at_clause(input: &mut Stream<'_>) -> ModalResult<String> {
    let _ = word("AT").parse_next(input)?;
    string_value(input)
}

/// `SECRET '<ref>'` — the connection's secret REFERENCE (`env:<VAR>` / `vault:<path>`), a quoted
/// string. `SECRET` is a contextual ident; the value is a reference, never an inline secret.
fn conn_secret_clause(input: &mut Stream<'_>) -> ModalResult<String> {
    let _ = word("SECRET").parse_next(input)?;
    string_value(input)
}

/// A single string-literal token's text (the `'…'` body).
fn string_value(input: &mut Stream<'_>) -> ModalResult<String> {
    any.verify_map(|t: Spanned<Token>| match t.node {
        Token::Str(s) => Some(s),
        _ => None,
    })
    .parse_next(input)
}

/// `materialized view` — `materialized` is an ident the lexer leaves bare (matched
/// case-insensitively), followed by the `view` keyword.
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
        DdlKind::Connection => "connections",
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
        // A keyword word standing as a raw label operand (e.g. an `on webhook` event kind,
        // where `webhook` collides with the keyword). Recognized case-insensitively (t74);
        // its canonical lowercase text is the label. A pure surface accommodation — these
        // operands are routes/intervals/event-kinds, never effects.
        Token::Keyword(k) => Some(k.text().to_string()),
        Token::Str(s) => Some(s),
        Token::Int(i) => Some(i.to_string()),
        Token::Size { value, unit } => Some(format!("{value} {}", unit.text())),
        // A `Token::Path` is a leading-slash mount path (`/mail/inbox`); the lexer drops the
        // leading slash from the segments, so we re-prepend it. This is what lets a
        // `CREATE TRIGGER … ON /mail/inbox …` round-trip to the `/mail/inbox` source path the
        // watchtower (t34) re-queries — a slash-less `mail/inbox` would not resolve as a mount.
        Token::Path(segs) => Some(format!(
            "/{}",
            segs.into_iter()
                .map(|s| s.name)
                .collect::<Vec<_>>()
                .join("/")
        )),
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
        // A `TRANSACTION { … }` block (M6, t62): a distinct leading keyword, so order-independent.
        transaction_block,
        server_ddl.map(Statement::Ddl),
        // Verb-leading effect (`INSERT INTO …`, the source-less `VALUES` literal form among
        // them) wins first; a source-leading pipeline then either reads (`Statement::Query`)
        // or terminates in a write stage and lowers to the same `Statement::Effect` (t72).
        effect_stmt.map(Statement::Effect),
        pipeline_or_effect,
    ))
    .parse_next(input)
}

/// One statement inside a `TRANSACTION { … }` block (M6, ticket t62): an **effect** statement
/// only — a verb-leading `INSERT/UPSERT/UPDATE/REMOVE …` (the source-less `VALUES` literal form
/// among them) or a terminal pipe-stage write (`/path |> … |> REMOVE`). A pure read pipeline, a
/// nested `TRANSACTION`, a `LET`, a DDL, or a `PREVIEW`/`COMMIT` wrapper is rejected here (the
/// shape gate) so the block stays a thin, reversible-only wrapper over [`EffectStmt`]s; the
/// reversibility check itself is the eval-time gate, not this parser.
fn transaction_item(input: &mut Stream<'_>) -> ModalResult<Statement> {
    // Verb-leading effect wins first; backtracks cleanly when the next item is absent (`}`).
    if let Some(e) = opt(effect_stmt).parse_next(input)? {
        return Ok(Statement::Effect(e));
    }
    // Otherwise a source-leading pipeline that MUST terminate in a write stage. A bare read is
    // not a legal transaction member: once a non-effect is parsed it is a crisp authoring error
    // (`cut`), never a silent accept — decision G's reversible-only block holds no read.
    match pipeline_or_effect(input)? {
        eff @ Statement::Effect(_) => Ok(eff),
        _ => Err(ErrMode::Cut(ContextError::new())),
    }
}

/// A `TRANSACTION { <effect> ; <effect> ; … }` block (M6, ticket t62, decision G): a
/// reversible-only, all-or-nothing group of effect statements in commit-point (source) order.
/// Once `TRANSACTION` is consumed we are committed (`cut_err` on the braces), so a malformed
/// block is a crisp error pointing *inside* it. The effects are `;`-separated (a trailing `;` is
/// allowed) and an empty block parses to an empty (trivially reversible) plan. Reversibility is
/// enforced at eval time (`EvalError::IrreversibleInTransaction`), not here.
fn transaction_block(input: &mut Stream<'_>) -> ModalResult<Statement> {
    let open = kw(Keyword::Transaction).parse_next(input)?;
    let _ = cut_err(punct(Token::LBrace)).parse_next(input)?;
    let body: Vec<Statement> =
        separated(0.., transaction_item, punct(Token::Semicolon)).parse_next(input)?;
    // An optional trailing `;` after the final effect (winnow's `separated` leaves it unconsumed).
    let _ = opt(punct(Token::Semicolon)).parse_next(input)?;
    let close = cut_err(punct(Token::RBrace)).parse_next(input)?;
    Ok(Statement::Transaction {
        body,
        span: Span::new(open.start, close.end),
    })
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

/// A `LET <name> = <pipeline>` binding followed by the rest of the program (M6, ticket
/// t60). Once `LET` is consumed we are committed (`cut_err`), so a malformed binding is a
/// crisp error pointing *inside* the binding. The `value` is restricted to a `pipeline`
/// (a relation) — never an effect — so a `LET` cannot smuggle a write into a pure context;
/// the `body` is the rest of the program, with `name` in scope.
fn let_binding(input: &mut Stream<'_>) -> ModalResult<Statement> {
    let _ = kw(Keyword::Let).parse_next(input)?;
    let name = cut_err(ident).parse_next(input)?;
    let _ = cut_err(punct(Token::Eq)).parse_next(input)?;
    let value = cut_err(let_value).parse_next(input)?;
    let body = cut_err(program_seq).parse_next(input)?;
    Ok(Statement::Let {
        name: name.node,
        value: Box::new(value),
        body: Box::new(body),
    })
}

/// The bound value of a `LET` (M6, ticket t60 + t61): a **relation** (a pipeline) OR a
/// first-class **value** (a lambda or a scalar expression — t61, decision H "functions are
/// values"). A *named function* is just a `LET`-bound lambda (`LET f = (x) => …`), so no
/// `DEF` keyword is needed.
///
/// The alternatives are ordered so each shape wins unambiguously:
/// 1. `lambda` first — a `( params ) => body` value; tried before `pipeline` because a
///    bare `(x)` would otherwise be read as a parenthesised sub-pipeline source.
/// 2. `pipeline` — the t60 relation binding (`/path |> …`, a bound name, `VALUES`, a
///    subquery). Unchanged, so every existing relation `LET` parses exactly as before.
/// 3. `expr` — a scalar value binding (`LET cutoff = '2026-03-27'`), reached only when the
///    value is neither a lambda nor a pipeline source.
///
/// A value binding (lambda/scalar) is retained as a single-cell `VALUES` relation so the
/// `Statement::Let { value: Box<Statement> }` shape — and its governance variant lock — is
/// **untouched** (no new `Statement` variant): the bound expression rides in the relation's
/// one row, available verbatim to a later type-checker / value runtime.
fn let_value(input: &mut Stream<'_>) -> ModalResult<Statement> {
    alt((
        lambda.map(wrap_value_binding),
        pipeline.map(Statement::Query),
        // A scalar value binding (`LET cutoff = '2026-03-27'`). Restricted to a *literal* so
        // it cannot collide with the `pipeline` source forms above: a bare identifier stays a
        // `LET`-bound relation name (`LET b = a`, t60), and a fn-call (`map(…)`) belongs in a
        // pipeline stage, not as a bare `LET` value — keeping every existing relation binding
        // parsing exactly as before.
        literal.map(|lit| wrap_value_binding(Expr::Lit(lit))),
    ))
    .parse_next(input)
}

/// Wrap a `LET`-bound value expression (a lambda or scalar) into the single-cell `VALUES`
/// relation that carries it, so a value binding reuses the existing relation-valued
/// `Statement::Let` shape without adding a `Statement` variant (see [`let_value`]).
fn wrap_value_binding(value: Expr) -> Statement {
    Statement::Query(Pipeline {
        source: Source::Values(Values {
            columns: None,
            rows: vec![vec![value]],
        }),
        ops: vec![],
    })
}

/// One program: zero or more `LET` bindings in scope for the final statement (RFD §1.2 —
/// statements with no terminator). Encoded as a let-in nesting (see [`Statement::Let`]).
/// A bare (non-`LET`) statement is the program's terminal statement; the top-level
/// `eof` then rejects a second non-binding statement (`FROM a` `FROM b`) as trailing input.
fn program_seq(input: &mut Stream<'_>) -> ModalResult<Statement> {
    alt((let_binding, inner_statement)).parse_next(input)
}

/// The top-level program parser: a `LET`-binding sequence then a final statement, then
/// end-of-input (one statement per line, `;`-free — RFD §1.2 statement model).
fn statement(input: &mut Stream<'_>) -> ModalResult<Statement> {
    let stmt = program_seq(input)?;
    winnow::combinator::eof(input)?;
    Ok(stmt)
}
