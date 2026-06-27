//! Parser/grammar goldens (t38, RFD §3/§5): `golden_parse(src) -> AstSnapshot` over the
//! closed-core grammar, plus a stable error-recovery snapshot.
//!
//! The owned [`qfs_parser::Statement`] AST is `Serialize`, so a golden is just its canonical
//! JSON — the closed-core keywords, `|>`, `CALL`, `DECODE/ENCODE`, and server `CREATE …` all
//! snapshot to a **stable AST** that pins the grammar's shape. A grammar regression (a stage
//! drops, a variant reshapes) flaps the golden. At least one **parse-error-recovery** case
//! snapshots a stable structured message ([`error_snapshot`]) so the AI-facing error contract
//! (RFD §5: machine code + non-empty expected-set) is locked too.

use serde::Serialize;

use qfs_parser::{parse_statement, ParseError, Statement};

/// A parsed AST ready to snapshot: the owned [`Statement`] plus the source it came from (so a
/// fixture self-documents what program produced it). Serializes deterministically.
#[derive(Debug, Clone, Serialize)]
pub struct AstSnapshot {
    /// The source program that was parsed.
    pub source: String,
    /// The owned, closed-core AST.
    pub ast: Statement,
}

/// Parse `src` to an [`AstSnapshot`]. This is the success path — the corpus calls it on each
/// closed-core construct, then `.snapshot(name)`s the result.
///
/// # Panics
/// Panics (test-only) if `src` does not parse — a golden of an unparseable program is a test-
/// author error, surfaced loudly.
#[must_use]
pub fn golden_parse(src: &str) -> AstSnapshot {
    let ast = parse_statement(src)
        .unwrap_or_else(|e| panic!("qfs-test golden_parse: `{src}` did not parse: {e}"));
    AstSnapshot {
        source: src.to_string(),
        ast,
    }
}

impl AstSnapshot {
    /// Snapshot this AST as a golden (canonical JSON, scrubbed for credential shapes).
    ///
    /// # Panics
    /// Panics on a golden mismatch or a credential-shape leak.
    pub fn snapshot(&self, name: &str) {
        let rendered = crate::golden::canonical_json(self);
        crate::golden::assert_no_credential_shape(&rendered);
        crate::golden::assert_golden(name, self);
    }
}

/// A stable, serializable view of a [`ParseError`] for the error-recovery golden — the AI-
/// facing structured-error contract (RFD §5): the machine code, the non-empty expected-set,
/// the found-kind, and the message. The byte offset is included (deterministic for a fixed
/// source) so a span regression is also caught.
#[derive(Debug, Clone, Serialize)]
pub struct ParseErrorSnapshot {
    /// The source program that failed to parse.
    pub source: String,
    /// The stable machine code (e.g. `UNKNOWN_KEYWORD`).
    pub code: String,
    /// The byte offset where parsing failed.
    pub at: usize,
    /// The non-empty expected-token set (RFD §5 guarantees non-empty).
    pub expected: Vec<String>,
    /// The kind of token actually found (never a literal value — RFD §10 secret hygiene).
    pub found: String,
    /// The human-facing message.
    pub message: String,
}

/// Parse `src`, expecting a **failure**, and return the stable [`ParseErrorSnapshot`] — the
/// error-recovery golden path.
///
/// # Panics
/// Panics if `src` *succeeds* (an error-recovery golden requires a failing input).
#[must_use]
pub fn error_snapshot(src: &str) -> ParseErrorSnapshot {
    match parse_statement(src) {
        Ok(_) => panic!("qfs-test error_snapshot: `{src}` parsed successfully (expected an error)"),
        Err(e) => from_error(src, &e),
    }
}

fn from_error(src: &str, e: &ParseError) -> ParseErrorSnapshot {
    ParseErrorSnapshot {
        source: src.to_string(),
        code: e.code.as_str().to_string(),
        at: e.at,
        expected: e.expected.clone(),
        found: e.found.clone(),
        message: e.message.clone(),
    }
}

impl ParseErrorSnapshot {
    /// Snapshot this structured error as a golden.
    ///
    /// # Panics
    /// Panics on a golden mismatch or a credential-shape leak.
    pub fn snapshot(&self, name: &str) {
        let rendered = crate::golden::canonical_json(self);
        crate::golden::assert_no_credential_shape(&rendered);
        crate::golden::assert_golden(name, self);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn golden_parse_returns_the_owned_ast() {
        let snap = golden_parse("/mail/inbox |> LIMIT 5");
        assert!(matches!(snap.ast, Statement::Query(_)));
    }

    #[test]
    fn error_snapshot_captures_a_stable_structured_message() {
        // A lowercase keyword is not in the frozen closed-core set — a stable recovery message.
        // (`from` is no longer a keyword post-t73; a lowercase *stage* keyword still trips it.)
        let snap = error_snapshot("/mail/inbox |> where id > 5");
        assert!(
            !snap.expected.is_empty(),
            "expected-set is non-empty (RFD §5)"
        );
        assert!(!snap.code.is_empty());
    }
}
