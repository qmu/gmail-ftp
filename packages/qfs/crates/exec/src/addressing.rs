//! Addressing validation (ticket t29): in one-shot mode there is **no cwd**, so every node
//! must be addressed by an **absolute VFS path** (`/driver/…`) or an **`id:` form** (`id:…`).
//! A relative path (`foo/bar`, `./x`) is rejected with a structured `usage` error (exit 2).
//!
//! ## Why this is a pre-parse lexical check
//! The parser's lexer drops the leading `/` of a path (`/db/x` and `db/x` both lower to
//! segments `[db, x]`), so absoluteness is **not** recoverable from the AST. The check
//! therefore runs over the raw statement text: it finds each addressing keyword
//! (`FROM`/`INTO`/`UPDATE`/`REMOVE`) and verifies the path token that follows is absolute or
//! `id:`-prefixed. `FROM VALUES`/`FROM (` (inline relation / sub-query) carry no path and are
//! skipped. This is addressing-only; destructive-set detection stays grammar-agnostic
//! (plan metadata, never keyword sniffing).

use crate::error::ExecError;

/// The keywords whose following token is a node address in the closed-core grammar.
const ADDRESSING_KEYWORDS: &[&str] = &["FROM", "INTO", "UPDATE", "REMOVE"];

/// Validate that every addressed node in `src` uses an absolute (`/…`) or `id:` form. Returns
/// the first relative-path violation as a structured `usage` error.
///
/// # Errors
/// [`ExecError`] (kind `usage`, exit 2) naming the offending relative path.
pub fn validate(src: &str) -> Result<(), ExecError> {
    let tokens = lex_words(src);
    for i in 0..tokens.len() {
        let upper = tokens[i].to_ascii_uppercase();
        if !ADDRESSING_KEYWORDS.contains(&upper.as_str()) {
            continue;
        }
        let Some(next) = tokens.get(i + 1) else {
            continue;
        };
        // `FROM VALUES` / `FROM (subquery)` carry no path address.
        let next_upper = next.to_ascii_uppercase();
        if next_upper == "VALUES" || next.starts_with('(') {
            continue;
        }
        if !is_addressed(next) {
            return Err(ExecError::usage(format!(
                "relative path `{next}` is not allowed in one-shot mode; use an absolute path \
                 (`/driver/...`) or an `id:` form"
            ))
            .with_path(next.clone()));
        }
    }
    Ok(())
}

/// Validate a single node address (e.g. the argument of `qfs describe <path>`): it must be an
/// absolute (`/…`) or `id:`-prefixed path in one-shot mode. A relative path is a `usage` error.
///
/// # Errors
/// [`ExecError`] (kind `usage`, exit 2) if `path` is relative or empty.
pub fn validate_path(path: &str) -> Result<(), ExecError> {
    if is_addressed(path) {
        Ok(())
    } else {
        Err(ExecError::usage(format!(
            "relative path `{path}` is not allowed in one-shot mode; use an absolute path \
             (`/driver/...`) or an `id:` form"
        ))
        .with_path(path))
    }
}

/// Whether a path token is an accepted one-shot address: absolute (`/…`) or `id:`-prefixed.
fn is_addressed(token: &str) -> bool {
    token.starts_with('/') || token.starts_with("id:")
}

/// Split `src` into whitespace-separated word tokens, ignoring string literals (so a path-like
/// substring inside `'...'` never trips the check). Coarse but sufficient: it only needs to
/// recover the token immediately after an addressing keyword.
fn lex_words(src: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut chars = src.chars().peekable();
    let mut cur = String::new();
    let mut in_str = false;
    let mut quote = '\'';
    while let Some(c) = chars.next() {
        if in_str {
            if c == quote {
                in_str = false;
            }
            continue;
        }
        match c {
            '\'' | '"' => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
                in_str = true;
                quote = c;
            }
            c if c.is_whitespace() => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            // `|>` pipe boundary: flush the current word so `…x|>FROM` still tokenizes FROM.
            '|' if chars.peek() == Some(&'>') => {
                chars.next();
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absolute_path_accepted() {
        assert!(validate("FROM /mail/inbox |> LIMIT 1").is_ok());
    }

    #[test]
    fn id_form_accepted() {
        assert!(validate("FROM id:abc123 |> LIMIT 1").is_ok());
    }

    #[test]
    fn relative_path_rejected_with_usage() {
        let err = validate("FROM mail/inbox |> LIMIT 1").unwrap_err();
        assert_eq!(err.kind.as_str(), "usage");
        assert_eq!(err.path.as_deref(), Some("mail/inbox"));
    }

    #[test]
    fn from_values_is_not_an_address() {
        assert!(validate("FROM VALUES (1),(2)").is_ok());
    }

    #[test]
    fn path_inside_string_literal_is_ignored() {
        // A relative-looking path inside a WHERE string must not trip the check.
        assert!(validate("FROM /mail/inbox |> WHERE subject = 'a/b/c'").is_ok());
    }
}
