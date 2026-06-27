//! Addressing validation (ticket t29): in one-shot mode there is **no cwd**, so every node
//! must be addressed by an **absolute VFS path** (`/driver/…`) or an **`id:` form** (`id:…`).
//! A relative path (`foo/bar`, `./x`) is rejected with a structured `usage` error (exit 2).
//!
//! ## Why this is a pre-parse lexical check
//! The parser's lexer drops the leading `/` of a path (`/db/x` and `db/x` both lower to
//! segments `[db, x]`), so absoluteness is **not** recoverable from the AST. The check
//! therefore runs over the raw statement text. Two source positions carry a node address:
//!
//! * the **leading source** — decision R (ticket t73) dropped `FROM`, so a pipeline now *opens*
//!   on its source token (`/db/x |> …`). The first token is the address, unless the statement
//!   opens on a non-source keyword (`INSERT`/`UPSERT`/`UPDATE`/`REMOVE`/`CREATE`/`PREVIEW`/
//!   `COMMIT`/`LET`/`CALL`) or an inline relation (`VALUES` / `(`subquery`)`).
//! * a **write target** — the token after `INTO`/`UPDATE`/`REMOVE`.
//!
//! This is addressing-only; destructive-set detection stays grammar-agnostic (plan metadata,
//! never keyword sniffing).

use crate::error::ExecError;

/// The keywords whose following token is a write-target address (RFD §3 effect surface). The
/// read source no longer has an introducer keyword — `FROM` was removed in t73 (decision R), so
/// the leading source is validated by position instead (see [`validate`]).
const ADDRESSING_KEYWORDS: &[&str] = &["INTO", "UPDATE", "REMOVE"];

/// Statement-leading keywords that are **not** a source: when the first token is one of these,
/// the pipeline source (if any) comes later and is addressed by one of the write-target keywords
/// above, so the leading-source check is skipped.
const NON_SOURCE_LEAD: &[&str] = &[
    "INSERT", "UPSERT", "UPDATE", "REMOVE", "CREATE", "PREVIEW", "COMMIT", "LET", "CALL",
];

/// Validate that every addressed node in `src` uses an absolute (`/…`) or `id:` form. Returns
/// the first relative-path violation as a structured `usage` error.
///
/// # Errors
/// [`ExecError`] (kind `usage`, exit 2) naming the offending relative path.
pub fn validate(src: &str) -> Result<(), ExecError> {
    let tokens = lex_words(src);
    // The leading source (decision R, t73): a pipeline opens on its source token. Only a
    // *relative path* (a `/`-bearing token that is not absolute / `id:`) is a one-shot addressing
    // violation here — a bare word leads to either a bound-name source or a genuine parse error,
    // both of which are handled downstream (so `this is not pipe sql` stays a parse error, not a
    // usage error). The leading check is skipped for a non-source keyword or an inline relation.
    if let Some(first) = tokens.first() {
        let upper = first.to_ascii_uppercase();
        let inline_relation = upper == "VALUES" || first.starts_with('(');
        let is_relative_path = first.contains('/') && !is_addressed(first);
        if !NON_SOURCE_LEAD.contains(&upper.as_str()) && !inline_relation && is_relative_path {
            return Err(relative_path_error(first));
        }
    }
    for i in 0..tokens.len() {
        let upper = tokens[i].to_ascii_uppercase();
        if !ADDRESSING_KEYWORDS.contains(&upper.as_str()) {
            continue;
        }
        let Some(next) = tokens.get(i + 1) else {
            continue;
        };
        // An `INTO VALUES` / `INTO (subquery)` form carries no path address.
        let next_upper = next.to_ascii_uppercase();
        if next_upper == "VALUES" || next.starts_with('(') {
            continue;
        }
        if !is_addressed(next) {
            return Err(relative_path_error(next));
        }
    }
    Ok(())
}

/// The structured `usage` error (exit 2) for a relative path used in one-shot mode.
fn relative_path_error(path: &str) -> ExecError {
    ExecError::usage(format!(
        "relative path `{path}` is not allowed in one-shot mode; use an absolute path \
         (`/driver/...`) or an `id:` form"
    ))
    .with_path(path.to_string())
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
        assert!(validate("/mail/inbox |> LIMIT 1").is_ok());
    }

    #[test]
    fn id_form_accepted() {
        // Decision R (t73): the source leads — no `FROM`.
        assert!(validate("id:abc123 |> LIMIT 1").is_ok());
    }

    #[test]
    fn relative_leading_source_rejected_with_usage() {
        // The leading source token is the address now (no `FROM` keyword introduces it).
        let err = validate("mail/inbox |> LIMIT 1").unwrap_err();
        assert_eq!(err.kind.as_str(), "usage");
        assert_eq!(err.path.as_deref(), Some("mail/inbox"));
    }

    #[test]
    fn values_is_not_an_address() {
        assert!(validate("VALUES (1),(2)").is_ok());
    }

    #[test]
    fn path_inside_string_literal_is_ignored() {
        // A relative-looking path inside a WHERE string must not trip the check.
        assert!(validate("/mail/inbox |> WHERE subject = 'a/b/c'").is_ok());
    }
}
