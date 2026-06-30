//! The qfs lexer: a pure `&str -> Result<Vec<Spanned<Token>>, LexError>` scanner.
//!
//! This is the first stage of the language core (RFD §2.2 "Pipe-SQL", §3 "closed
//! core"). It is **pure**: no I/O, no `World`, referentially transparent — the
//! foundation of the dry-runnable pipeline (RFD §3 purity invariant). It is a
//! hand-written byte/char cursor rather than a combinator parser, so `qfs-lang`
//! keeps **zero dependencies** and stays trivially `wasm32`-clean (B7); the t04
//! parser (winnow) consumes the token stream this produces.
//!
//! ## Documented lexing decisions (RFD §3/§4, t03 "Considerations")
//! * **Path vs. division/glob.** A `/` at a statement/operator boundary always
//!   starts a [`Token::Path`]; qfs has no arithmetic `/` in the core grammar, so
//!   no division operator is ever emitted. Glob chars (`*`, `?`) inside a path
//!   segment set the segment's `glob` flag; raw segment text is preserved.
//! * **Size literal `25 MB`.** A bare integer immediately followed (across
//!   whitespace) by an uppercase word in the [`SizeUnit`] set folds into a
//!   [`Token::Size`]. Any other following word is left as a separate token, so a
//!   column literally named `MB` is unaffected.
//! * **Typed literal `DATE '…'`.** The introducer words `DATE`/`TIME`/`TIMESTAMP`
//!   immediately followed by a string literal fold into a [`Token::TypedLit`]; the
//!   inner string is captured raw and **not** validated (parser/runtime concern).
//!   A bare `DATE` not followed by a string stays an identifier.
//! * **`@version` in paths.** `@` binds to the preceding path segment; the raw ref
//!   text (git ref, S3 versionId, drive rev — RFD §4) is preserved without
//!   interpretation.
//! * **Keyword case (t74, decision S).** Closed-core keywords are recognized
//!   **case-insensitively** and are canonically **lowercase** (`where`, `select`,
//!   `insert into`, …): the word lexer folds a word's case before matching
//!   [`Keyword::from_word`], so `SELECT`/`Select`/`select` all lex to the same
//!   [`Token::Keyword`]. Identifiers, paths, and literals stay case-sensitive data.
//! * **Multi-word keywords.** `group by`, `insert into`, … are emitted as separate
//!   adjacent tokens (their lead words surface as [`Token::Ident`]); composition is
//!   the parser's job (RFD §3).
//!
//! ## Security note (no-live-creds by construction)
//! The lexer must never be fed credential material: errors quote source spans, so
//! anything in the source could surface in a diagnostic. There are no creds, no
//! network, and no filesystem access anywhere in this module.

use crate::error::{LexError, LexErrorKind};
use crate::keywords::Keyword;
use crate::span::{Span, Spanned};
use crate::token::{literal_word, LitType, PathSeg, SizeUnit, Token};

/// Tokenize one qfs statement into a flat, spanned token stream.
///
/// Pure and panic-free: arbitrary UTF-8 input yields `Ok(tokens)` or a single
/// [`LexError`] with a byte span — never a panic or abort. Every emitted token's
/// span slices back to the exact originating source substring (round-trip
/// invariant), which is load-bearing for the AI structured-error path (RFD §5).
///
/// # Errors
/// Returns a [`LexError`] on an unterminated string, a bad escape, a malformed
/// number, or a stray character that cannot begin any token.
///
/// ```
/// use qfs_lang::lex::lex;
/// use qfs_lang::token::Token;
/// let toks = lex("WHERE mail").expect("valid");
/// assert_eq!(toks.len(), 2);
/// assert!(matches!(toks[1].node, Token::Ident(ref s) if s == "mail"));
/// ```
pub fn lex(src: &str) -> Result<Vec<Spanned<Token>>, LexError> {
    Lexer::new(src).run()
}

/// A char with its starting byte offset in the source.
#[derive(Clone, Copy)]
struct Ch {
    /// Byte offset of this char's first byte.
    at: usize,
    /// The decoded character.
    c: char,
}

struct Lexer {
    /// All `(byte_offset, char)` pairs of the source, indexed by char position.
    chars: Vec<Ch>,
    /// Current char-position cursor into `chars`.
    pos: usize,
    /// Total source byte length (the end offset for the final char/EOF).
    byte_len: usize,
    out: Vec<Spanned<Token>>,
}

impl Lexer {
    fn new(src: &str) -> Self {
        let chars: Vec<Ch> = src.char_indices().map(|(at, c)| Ch { at, c }).collect();
        Self {
            chars,
            pos: 0,
            byte_len: src.len(),
            out: Vec::new(),
        }
    }

    /// The char at `pos`, if any.
    fn peek(&self) -> Option<Ch> {
        self.chars.get(self.pos).copied()
    }

    /// The char at `pos + n`, if any.
    fn peek_n(&self, n: usize) -> Option<Ch> {
        self.chars.get(self.pos + n).copied()
    }

    /// Advance past one char.
    fn bump(&mut self) {
        self.pos += 1;
    }

    /// The byte offset of the char at `pos` (or `byte_len` at EOF).
    fn byte_at(&self) -> usize {
        self.chars.get(self.pos).map_or(self.byte_len, |ch| ch.at)
    }

    /// Push a token spanning `[start_byte, current_byte)`.
    fn push(&mut self, start_byte: usize, node: Token) {
        let end = self.byte_at();
        // Byte offsets are bounded by source length, which the workspace keeps
        // small (single statement); the `as u32` cast is safe for any realistic
        // input and saturates rather than wraps if somehow exceeded.
        let span = Span::new(
            u32::try_from(start_byte).unwrap_or(u32::MAX),
            u32::try_from(end).unwrap_or(u32::MAX),
        );
        self.out.push(Spanned::new(node, span));
    }

    /// Build a one-or-two-byte error span starting at `start_byte`.
    fn err(&self, start_byte: usize, kind: LexErrorKind) -> LexError {
        let end = self.byte_at();
        let end = if end > start_byte { end } else { self.byte_len };
        LexError::new(
            Span::new(
                u32::try_from(start_byte).unwrap_or(u32::MAX),
                u32::try_from(end).unwrap_or(u32::MAX),
            ),
            kind,
        )
    }

    fn run(mut self) -> Result<Vec<Spanned<Token>>, LexError> {
        while let Some(ch) = self.peek() {
            if ch.c.is_whitespace() {
                self.bump();
                continue;
            }
            // Comments: line `--`, line `#`. Span-tracked (skipped) — spans of
            // emitted tokens never overlap a comment, satisfying the round-trip
            // invariant for the tokens that are produced.
            if ch.c == '#' {
                self.skip_line_comment();
                continue;
            }
            if ch.c == '-' && self.peek_n(1).is_some_and(|n| n.c == '-') {
                self.skip_line_comment();
                continue;
            }
            match ch.c {
                '/' => self.lex_path()?,
                '\'' => self.lex_string()?,
                c if c.is_ascii_digit() => self.lex_number()?,
                c if is_ident_start(c) => self.lex_word()?,
                _ => self.lex_symbol()?,
            }
        }
        Ok(self.out)
    }

    /// Skip a line comment to end-of-line (the newline is left for the ws skip).
    fn skip_line_comment(&mut self) {
        while let Some(ch) = self.peek() {
            if ch.c == '\n' {
                break;
            }
            self.bump();
        }
    }

    /// Lex a `/`-led path: `/seg/seg(@ver)`, with glob flags per segment.
    fn lex_path(&mut self) -> Result<(), LexError> {
        let start = self.byte_at();
        let mut segs: Vec<PathSeg> = Vec::new();
        // Consume the leading '/' and each segment.
        while self.peek().is_some_and(|ch| ch.c == '/') {
            self.bump(); // consume '/'
            let (name, glob) = self.consume_path_segment_name();
            let version = self.consume_path_version();
            // An empty leading segment (e.g. trailing '/') is preserved as an
            // empty name so spans round-trip; the parser validates structure.
            segs.push(PathSeg::new(name, version, glob));
        }
        self.push(start, Token::Path(segs));
        Ok(())
    }

    /// Consume one path segment name (until `/`, `@`, or a delimiter); report
    /// whether it contained a glob char.
    fn consume_path_segment_name(&mut self) -> (String, bool) {
        let mut name = String::new();
        let mut glob = false;
        while let Some(ch) = self.peek() {
            if ch.c == '/' || ch.c == '@' || is_path_delimiter(ch.c) {
                break;
            }
            if ch.c == '*' || ch.c == '?' {
                glob = true;
            }
            name.push(ch.c);
            self.bump();
        }
        (name, glob)
    }

    /// Consume an optional `@version` ref bound to the current segment.
    fn consume_path_version(&mut self) -> Option<String> {
        if self.peek().is_none_or(|ch| ch.c != '@') {
            return None;
        }
        self.bump(); // consume '@'
        let mut ver = String::new();
        while let Some(ch) = self.peek() {
            // The ref runs until the next path separator or a delimiter. Dots are allowed
            // (`@v1.2`), and so is `~` — a git relative-ref char (`@HEAD~1`, `@main~2`) that is
            // otherwise the `~` match operator; inside a path-version run it is part of the ref, not
            // an operator. (`^`, `{`, `}` are already non-delimiters, so `@HEAD^`, `@v1.2^{}` lex.)
            if ch.c == '/' || (is_path_delimiter(ch.c) && ch.c != '~') {
                break;
            }
            ver.push(ch.c);
            self.bump();
        }
        Some(ver)
    }

    /// Lex a single-quoted string literal with escape handling.
    fn lex_string(&mut self) -> Result<(), LexError> {
        let start = self.byte_at();
        self.bump(); // consume opening quote
        let mut value = String::new();
        loop {
            let Some(ch) = self.peek() else {
                return Err(self.err(start, LexErrorKind::UnterminatedString));
            };
            match ch.c {
                '\'' => {
                    self.bump(); // consume closing quote
                    self.push(start, Token::Str(value));
                    return Ok(());
                }
                '\\' => {
                    self.bump(); // consume backslash
                    let Some(esc) = self.peek() else {
                        return Err(self.err(start, LexErrorKind::UnterminatedString));
                    };
                    let decoded = match esc.c {
                        'n' => '\n',
                        't' => '\t',
                        'r' => '\r',
                        '\\' => '\\',
                        '\'' => '\'',
                        '0' => '\0',
                        _ => {
                            let bad_start = esc.at.saturating_sub(1);
                            self.bump(); // consume the bad escape char for the span
                            return Err(self.err(bad_start, LexErrorKind::BadEscape));
                        }
                    };
                    value.push(decoded);
                    self.bump(); // consume the escape char
                }
                _ => {
                    value.push(ch.c);
                    self.bump();
                }
            }
        }
    }

    /// Lex a numeric literal (int or float), optionally folding a trailing size
    /// unit into a [`Token::Size`].
    fn lex_number(&mut self) -> Result<(), LexError> {
        let start = self.byte_at();
        let mut raw = String::new();
        let mut is_float = false;
        while let Some(ch) = self.peek() {
            if ch.c.is_ascii_digit() {
                raw.push(ch.c);
                self.bump();
            } else if ch.c == '.'
                && !is_float
                && self.peek_n(1).is_some_and(|n| n.c.is_ascii_digit())
            {
                is_float = true;
                raw.push('.');
                self.bump();
            } else {
                break;
            }
        }

        if is_float {
            let value: f64 = raw
                .parse()
                .map_err(|_| self.err(start, LexErrorKind::BadNumber))?;
            self.push(start, Token::Float(value));
            return Ok(());
        }

        // Integer. Try to fold a following size unit (`25 MB`).
        if let Some(unit) = self.peek_size_unit() {
            // Re-parse as u64 for the size magnitude.
            let value: u64 = raw
                .parse()
                .map_err(|_| self.err(start, LexErrorKind::BadNumber))?;
            self.consume_size_unit_word();
            self.push(start, Token::Size { value, unit });
            return Ok(());
        }

        let value: i64 = raw
            .parse()
            .map_err(|_| self.err(start, LexErrorKind::BadNumber))?;
        self.push(start, Token::Int(value));
        Ok(())
    }

    /// Peek across whitespace for an uppercase word that is a [`SizeUnit`].
    /// Does not advance the cursor.
    fn peek_size_unit(&self) -> Option<SizeUnit> {
        let mut i = self.pos;
        // Skip whitespace (but not a newline-separated next statement boundary —
        // a unit must be on the same logical run; we allow spaces/tabs only).
        while let Some(ch) = self.chars.get(i) {
            if ch.c == ' ' || ch.c == '\t' {
                i += 1;
            } else {
                break;
            }
        }
        // The word must have been preceded by at least one space to be a unit
        // (`25MB` is not a size literal in this grammar — units are space-set).
        if i == self.pos {
            return None;
        }
        let mut word = String::new();
        while let Some(ch) = self.chars.get(i) {
            if is_ident_continue(ch.c) {
                word.push(ch.c);
                i += 1;
            } else {
                break;
            }
        }
        SizeUnit::from_word(&word)
    }

    /// Consume the whitespace + size-unit word previously confirmed by
    /// [`Self::peek_size_unit`].
    fn consume_size_unit_word(&mut self) {
        while self.peek().is_some_and(|ch| ch.c == ' ' || ch.c == '\t') {
            self.bump();
        }
        while self.peek().is_some_and(|ch| is_ident_continue(ch.c)) {
            self.bump();
        }
    }

    /// Lex an identifier-shaped word and classify it: keyword, boolean/null
    /// literal, typed-literal introducer, or bare identifier.
    fn lex_word(&mut self) -> Result<(), LexError> {
        let start = self.byte_at();
        let mut word = String::new();
        while let Some(ch) = self.peek() {
            if is_ident_continue(ch.c) {
                word.push(ch.c);
                self.bump();
            } else {
                break;
            }
        }

        // Typed literal: DATE/TIME/TIMESTAMP immediately followed by a string.
        if let Some(ty) = LitType::from_word(&word) {
            if let Some(raw) = self.try_typed_literal_string()? {
                self.push(start, Token::TypedLit { ty, raw });
                return Ok(());
            }
        }

        // Boolean / null literal words.
        if let Some(tok) = literal_word(&word) {
            self.push(start, tok);
            return Ok(());
        }

        // Reserved single-word keyword (case-insensitive; canonical form is lowercase, t74).
        if let Some(kw) = Keyword::from_word(&word) {
            self.push(start, Token::Keyword(kw));
            return Ok(());
        }

        // Otherwise a bare identifier.
        self.push(start, Token::Ident(word));
        Ok(())
    }

    /// If the next non-space token is a string literal, consume it and return its
    /// raw resolved content (for a typed literal). Otherwise leave the cursor put.
    fn try_typed_literal_string(&mut self) -> Result<Option<String>, LexError> {
        // Look ahead across spaces/tabs for an opening quote.
        let mut i = self.pos;
        while let Some(ch) = self.chars.get(i) {
            if ch.c == ' ' || ch.c == '\t' {
                i += 1;
            } else {
                break;
            }
        }
        if self.chars.get(i).is_none_or(|ch| ch.c != '\'') {
            return Ok(None);
        }
        // Advance the real cursor to the quote, then reuse the string lexer's
        // body by capturing the token it pushes.
        self.pos = i;
        let before = self.out.len();
        self.lex_string()?;
        // lex_string pushed exactly one Str token; extract its content.
        if let Some(spanned) = self.out.get(before) {
            if let Token::Str(s) = &spanned.node {
                let raw = s.clone();
                self.out.truncate(before);
                return Ok(Some(raw));
            }
        }
        Ok(None)
    }

    /// Lex symbolic operators and structural punctuation with longest-match.
    fn lex_symbol(&mut self) -> Result<(), LexError> {
        let start = self.byte_at();
        let Some(ch) = self.peek() else {
            return Ok(());
        };
        let next = self.peek_n(1).map(|n| n.c);
        let two = |a: char, b: char| ch.c == a && next == Some(b);

        let token = if two('|', '>') {
            self.bump();
            self.bump();
            Token::Pipe
        } else if two('<', '=') {
            self.bump();
            self.bump();
            Token::Le
        } else if two('>', '=') {
            self.bump();
            self.bump();
            Token::Ge
        } else if two('<', '>') {
            self.bump();
            self.bump();
            Token::Ne
        } else if two('=', '>') {
            self.bump();
            self.bump();
            Token::Arrow
        } else if two('=', '=') {
            // Maximal munch: `==` (equivalence) before a lone `=` (bind). The `=>`
            // (Arrow) check above already won where applicable, so the three forms
            // `=>` / `==` / `=` are each unambiguous (RFD decision O, ticket t70).
            self.bump();
            self.bump();
            Token::EqEq
        } else {
            let single = match ch.c {
                '=' => Token::Eq,
                '<' => Token::Lt,
                '>' => Token::Gt,
                '~' => Token::Tilde,
                '(' => Token::LParen,
                ')' => Token::RParen,
                '{' => Token::LBrace,
                '}' => Token::RBrace,
                ';' => Token::Semicolon,
                ',' => Token::Comma,
                ':' => Token::Colon,
                '.' => Token::Dot,
                '*' => Token::Star,
                other => {
                    self.bump();
                    return Err(self.err(start, LexErrorKind::UnexpectedChar(other)));
                }
            };
            self.bump();
            single
        };
        self.push(start, token);
        Ok(())
    }
}

/// Whether `c` can start a bare identifier / word.
fn is_ident_start(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_'
}

/// Whether `c` can continue a bare identifier / word.
fn is_ident_continue(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// Whether `c` ends a path segment / version run (operator or structural char).
fn is_path_delimiter(c: char) -> bool {
    c.is_whitespace()
        || matches!(
            c,
            '|' | '=' | '<' | '>' | '~' | '(' | ')' | ',' | '\'' | '#'
        )
}
