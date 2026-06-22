//! Lexer golden + round-trip + error + fuzz tests (t03 acceptance criteria).
//!
//! These exercise the public `cfs_lang::lex` API over the representative
//! statements from the ticket, assert spans round-trip to source substrings, pin
//! the structured error path, and check that arbitrary input never panics.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use cfs_lang::{
    lex, Keyword, LexError, LexErrorKind, LitType, PathSeg, SizeUnit, Span, Spanned, Token,
};

/// Helper: just the token nodes (drop spans) for structural assertions.
fn nodes(src: &str) -> Vec<Token> {
    lex(src)
        .unwrap_or_else(|e| panic!("expected `{src}` to lex, got {e}"))
        .into_iter()
        .map(|s| s.node)
        .collect()
}

/// Helper: assert every token's span slices back to a substring of `src`
/// (round-trip invariant) and that spans are non-decreasing.
fn assert_spans_round_trip(src: &str, toks: &[Spanned<Token>]) {
    let mut last_end = 0u32;
    for t in toks {
        let Span { start, end } = t.span;
        assert!(start <= end, "span start<=end for {t:?}");
        assert!(end as usize <= src.len(), "span within source for {t:?}");
        assert!(start >= last_end, "spans non-overlapping/ordered for {t:?}");
        last_end = end;
        // The slice must be valid (char-boundary) — this would panic otherwise.
        let _slice = &src[t.span.range()];
    }
}

#[test]
fn golden_full_query_pipeline() {
    let src =
        "FROM /mail/inbox |> WHERE size > 25 MB AND subject ~ 'invoice' |> SELECT id, subject";
    let toks = lex(src).expect("valid");
    assert_spans_round_trip(src, &toks);
    let got = nodes(src);
    assert_eq!(
        got,
        vec![
            Token::Keyword(Keyword::From),
            Token::Path(vec![
                PathSeg::new("mail", None, false),
                PathSeg::new("inbox", None, false),
            ]),
            Token::Pipe,
            Token::Keyword(Keyword::Where),
            Token::Ident("size".into()),
            Token::Gt,
            Token::Size {
                value: 25,
                unit: SizeUnit::MB
            },
            Token::Ident("AND".into()), // AND is an operator keyword-word; lexed bare
            Token::Ident("subject".into()),
            Token::Tilde,
            Token::Str("invoice".into()),
            Token::Pipe,
            Token::Keyword(Keyword::Select),
            Token::Ident("id".into()),
            Token::Comma,
            Token::Ident("subject".into()),
        ]
    );
}

#[test]
fn golden_path_with_version() {
    let src = "FROM /git/repo@v1.2/src |> SELECT path";
    let toks = lex(src).expect("valid");
    assert_spans_round_trip(src, &toks);
    let got = nodes(src);
    assert_eq!(got[0], Token::Keyword(Keyword::From), "leading keyword");
    assert_eq!(
        got[1],
        Token::Path(vec![
            PathSeg::new("git", None, false),
            PathSeg::new("repo", Some("v1.2".into()), false),
            PathSeg::new("src", None, false),
        ]),
        "version binds to the `repo` segment, raw ref preserved"
    );
}

#[test]
fn golden_insert_into_is_two_words() {
    // Multi-word keyword: INSERT INTO lexes as two adjacent uppercase idents.
    let src = "INSERT INTO /mail/drafts VALUES (id) RETURNING id";
    let toks = lex(src).expect("valid");
    assert_spans_round_trip(src, &toks);
    let got = nodes(src);
    assert_eq!(got[0], Token::Ident("INSERT".into()));
    assert_eq!(got[1], Token::Ident("INTO".into()));
    assert_eq!(
        got[2],
        Token::Path(vec![
            PathSeg::new("mail", None, false),
            PathSeg::new("drafts", None, false),
        ])
    );
    assert_eq!(got[3], Token::Keyword(Keyword::Values));
    assert!(got.contains(&Token::Keyword(Keyword::Returning)));
    assert!(got.contains(&Token::LParen));
    assert!(got.contains(&Token::RParen));
}

#[test]
fn golden_typed_literal_between() {
    let src = "WHERE created BETWEEN DATE '2026-01-01' AND DATE '2026-06-20'";
    let toks = lex(src).expect("valid");
    assert_spans_round_trip(src, &toks);
    let got = nodes(src);
    assert_eq!(got[0], Token::Keyword(Keyword::Where));
    assert_eq!(got[1], Token::Ident("created".into()));
    assert_eq!(got[2], Token::Ident("BETWEEN".into())); // operator keyword-word
    assert_eq!(
        got[3],
        Token::TypedLit {
            ty: LitType::Date,
            raw: "2026-01-01".into()
        }
    );
    assert_eq!(got[4], Token::Ident("AND".into()));
    assert_eq!(
        got[5],
        Token::TypedLit {
            ty: LitType::Date,
            raw: "2026-06-20".into()
        }
    );
}

#[test]
fn golden_named_proc_arg_arrow() {
    let src = "CALL github.merge(method=>'squash')";
    let toks = lex(src).expect("valid");
    assert_spans_round_trip(src, &toks);
    let got = nodes(src);
    assert_eq!(
        got,
        vec![
            Token::Keyword(Keyword::Call),
            Token::Ident("github".into()),
            Token::Dot,
            Token::Ident("merge".into()),
            Token::LParen,
            Token::Ident("method".into()),
            Token::Arrow,
            Token::Str("squash".into()),
            Token::RParen,
        ]
    );
}

#[test]
fn golden_glob_path_with_latest() {
    let src = "FROM /s3/bucket/*.json@latest";
    let toks = lex(src).expect("valid");
    assert_spans_round_trip(src, &toks);
    let got = nodes(src);
    assert_eq!(got[0], Token::Keyword(Keyword::From));
    assert_eq!(
        got[1],
        Token::Path(vec![
            PathSeg::new("s3", None, false),
            PathSeg::new("bucket", None, false),
            PathSeg::new("*.json", Some("latest".into()), true),
        ]),
        "glob segment flagged; @latest binds to it"
    );
}

#[test]
fn span_round_trips_to_exact_substring() {
    let src = "FROM /mail/inbox |> SELECT id";
    let toks = lex(src).expect("valid");
    // The FROM keyword span is exactly "FROM".
    assert_eq!(&src[toks[0].span.range()], "FROM");
    // The pipe span is exactly "|>".
    let pipe = toks.iter().find(|t| t.node == Token::Pipe).unwrap();
    assert_eq!(&src[pipe.span.range()], "|>");
    // The SELECT span is exactly "SELECT".
    let sel = toks
        .iter()
        .find(|t| t.node == Token::Keyword(Keyword::Select))
        .unwrap();
    assert_eq!(&src[sel.span.range()], "SELECT");
}

#[test]
fn comments_are_skipped_but_tokens_round_trip() {
    let src = "FROM /mail  -- the inbox\n|> SELECT id  # trailing";
    let toks = lex(src).expect("valid");
    assert_spans_round_trip(src, &toks);
    let got: Vec<Token> = toks.into_iter().map(|s| s.node).collect();
    assert_eq!(got[0], Token::Keyword(Keyword::From));
    assert_eq!(got.last(), Some(&Token::Ident("id".into())));
    // No comment text leaked into a token.
    assert!(!got
        .iter()
        .any(|t| matches!(t, Token::Ident(s) if s.contains("inbox"))));
}

#[test]
fn float_and_int_literals() {
    assert_eq!(nodes("3"), vec![Token::Int(3)]);
    assert_eq!(nodes("3.5"), vec![Token::Float(3.5)]);
    assert_eq!(nodes("0.25"), vec![Token::Float(0.25)]);
    // A dot with no trailing digit is the structural Dot token, not a float.
    assert_eq!(
        nodes("a.b"),
        vec![
            Token::Ident("a".into()),
            Token::Dot,
            Token::Ident("b".into())
        ]
    );
}

#[test]
fn bare_mb_column_is_not_a_size_unit() {
    // `MB` not preceded by a number stays an identifier (column named MB).
    assert_eq!(
        nodes("SELECT MB"),
        vec![Token::Keyword(Keyword::Select), Token::Ident("MB".into())]
    );
    // `25MB` glued (no space) is NOT a size literal: int then ident.
    assert_eq!(
        nodes("25MB"),
        vec![Token::Int(25), Token::Ident("MB".into())]
    );
}

#[test]
fn bare_date_without_string_is_ident() {
    assert_eq!(nodes("DATE"), vec![Token::Ident("DATE".into())]);
    assert_eq!(
        nodes("SELECT DATE"),
        vec![Token::Keyword(Keyword::Select), Token::Ident("DATE".into())]
    );
}

#[test]
fn boolean_and_null_literals() {
    assert_eq!(nodes("TRUE"), vec![Token::Bool(true)]);
    assert_eq!(nodes("FALSE"), vec![Token::Bool(false)]);
    assert_eq!(nodes("NULL"), vec![Token::Null]);
}

#[test]
fn string_escapes_resolve() {
    assert_eq!(
        nodes(r"'a\tb\nc\\d\'e'"),
        vec![Token::Str("a\tb\nc\\d'e".into())]
    );
}

#[test]
fn error_unterminated_string() {
    let err: LexError = lex("WHERE x = 'oops").expect_err("unterminated");
    assert_eq!(err.kind, LexErrorKind::UnterminatedString);
    // Span starts at the opening quote.
    assert_eq!(err.span.start, 10);
}

#[test]
fn error_bad_escape() {
    let err = lex(r"'a\qb'").expect_err("bad escape");
    assert_eq!(err.kind, LexErrorKind::BadEscape);
    // Span covers the `\q`.
    assert_eq!(&"'a\\qb'"[err.span.range()], "\\q");
}

#[test]
fn error_stray_char() {
    let err = lex("WHERE x = $foo").expect_err("stray $");
    assert_eq!(err.kind, LexErrorKind::UnexpectedChar('$'));
    assert_eq!(&"WHERE x = $foo"[err.span.range()], "$");
}

#[test]
fn empty_input_lexes_to_empty_stream() {
    assert_eq!(lex("").expect("empty ok"), vec![]);
    assert_eq!(lex("   \n\t  ").expect("ws ok"), vec![]);
    assert_eq!(lex("-- just a comment").expect("comment ok"), vec![]);
}

/// Property-style fuzz: the lexer must NEVER panic on arbitrary input; it returns
/// `Ok` or `Err`. Covers a broad set of bytes including non-ASCII UTF-8.
#[test]
fn never_panics_on_arbitrary_input() {
    let corpus = [
        "",
        "🦀 FROM /x |> SELECT y",
        "'\\",
        "////@@@@",
        "<<<>>><=>=<>|>|>|",
        "DATE TIME TIMESTAMP '",
        "25 25 25 PB ZB MB",
        "(((,,,...***~~~",
        "\u{0}\u{1}\u{2}garbage$%^&",
        "FROM\n/a\n--c\n#d\n|>",
        "'unterminated with 漢字 and \t tabs",
        "999999999999999999999999999999",
    ];
    for s in corpus {
        // Must not panic; result is ignored.
        let _ = lex(s);
    }
    // A longer generated string of mixed bytes.
    let mut big = String::new();
    for i in 0u32..2000 {
        if let Some(c) = char::from_u32((i % 0x110000).max(1)) {
            big.push(c);
        }
    }
    let _ = lex(&big);
}

#[test]
fn integer_overflow_is_bad_number_not_panic() {
    let err = lex("99999999999999999999999999").expect_err("overflow");
    assert_eq!(err.kind, LexErrorKind::BadNumber);
}
