//! The frozen reserved-keyword set, transcribed **verbatim** from RFD-0001 §3
//! ("Closed core keywords (reserved, frozen)").
//!
//! [`KEYWORDS`] is the single committed fixture (fidelity guard G1 / acceptance
//! criterion C1): the golden test in `lib`'s `tests` module asserts against *this*
//! slice, so there is no second hand-transcription that could drift out of sync.
//! Multi-word forms (`GROUP BY`, `INSERT INTO`, `MATERIALIZED VIEW`) are stored as
//! their canonical multi-word strings to match §3 exactly; lexing nuance is E1's
//! concern, not the golden lock's.

/// A reserved keyword in the qfs closed core.
///
/// Each variant carries no data; the canonical surface text is obtained via
/// [`Keyword::text`]. The enum exists so later epics can pattern-match keywords
/// exhaustively (the compiler then flags any unhandled keyword), while [`KEYWORDS`]
/// remains the flat golden fixture the freeze test locks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[non_exhaustive]
pub enum Keyword {
    // -- Query / transform (RFD §3) --
    // NOTE: `FROM` was REMOVED in M6 (ticket t73, decision R): a leading `/path` (or a
    // `LET`-bound name) *is* the source, so the source position needs no `FROM` keyword. This is
    // a deliberate vocabulary *removal* — the freeze count below drops by one to mark it.
    Where,
    Select,
    Extend,
    Set,
    Aggregate,
    GroupBy,
    OrderBy,
    Limit,
    Distinct,
    Join,
    Union,
    Except,
    Intersect,
    As,
    Expand,
    // -- Functional core (M6, ticket t60) --
    // `LET` is a *deliberate* addition to the frozen RFD §3 vocabulary — one of only two
    // new keywords the whole roadmap permits (decision H; the other is `TRANSACTION`, t62).
    // It names an intermediate relation so it can be referenced more than once. The freeze
    // tests below are updated in step (38 → 39) precisely so this addition is reviewed, not
    // smuggled in.
    Let,
    // -- Effects (RFD §3) --
    InsertInto,
    UpsertInto,
    Update,
    Remove,
    Values,
    Returning,
    Call,
    // -- Codecs (RFD §3) --
    Decode,
    Encode,
    // -- Plan (RFD §3) --
    Preview,
    Commit,
    // -- Server DDL (RFD §3) --
    Create,
    Endpoint,
    Trigger,
    Job,
    View,
    MaterializedView,
    Webhook,
    Policy,
    Do,
    Every,
    On,
}

impl Keyword {
    /// Reverse lookup: classify a single source *word* as a reserved keyword.
    ///
    /// This recognizes only the **single-word** keywords. Multi-word keywords
    /// (`GROUP BY`, `ORDER BY`, `INSERT INTO`, `UPSERT INTO`, `MATERIALIZED
    /// VIEW`) are intentionally *not* matched here: the lexer's contract (RFD §3,
    /// t03) is that multi-word keywords are emitted as separate adjacent tokens
    /// and composition is the parser's job. The lead word of a multi-word keyword
    /// (e.g. `GROUP`, `INSERT`) is therefore returned as `None` and surfaces as an
    /// uppercase identifier; the parser stitches the pair back together.
    #[must_use]
    pub fn from_word(word: &str) -> Option<Self> {
        Some(match word {
            "WHERE" => Self::Where,
            "SELECT" => Self::Select,
            "EXTEND" => Self::Extend,
            "SET" => Self::Set,
            "AGGREGATE" => Self::Aggregate,
            "LIMIT" => Self::Limit,
            "DISTINCT" => Self::Distinct,
            "JOIN" => Self::Join,
            "UNION" => Self::Union,
            "EXCEPT" => Self::Except,
            "INTERSECT" => Self::Intersect,
            "AS" => Self::As,
            "EXPAND" => Self::Expand,
            "LET" => Self::Let,
            "UPDATE" => Self::Update,
            "REMOVE" => Self::Remove,
            "VALUES" => Self::Values,
            "RETURNING" => Self::Returning,
            "CALL" => Self::Call,
            "DECODE" => Self::Decode,
            "ENCODE" => Self::Encode,
            "PREVIEW" => Self::Preview,
            "COMMIT" => Self::Commit,
            "CREATE" => Self::Create,
            "ENDPOINT" => Self::Endpoint,
            "TRIGGER" => Self::Trigger,
            "JOB" => Self::Job,
            "VIEW" => Self::View,
            "WEBHOOK" => Self::Webhook,
            "POLICY" => Self::Policy,
            "DO" => Self::Do,
            "EVERY" => Self::Every,
            "ON" => Self::On,
            _ => return None,
        })
    }

    /// The canonical surface text of this keyword, exactly as written in RFD §3.
    #[must_use]
    pub const fn text(self) -> &'static str {
        match self {
            Self::Where => "WHERE",
            Self::Select => "SELECT",
            Self::Extend => "EXTEND",
            Self::Set => "SET",
            Self::Aggregate => "AGGREGATE",
            Self::GroupBy => "GROUP BY",
            Self::OrderBy => "ORDER BY",
            Self::Limit => "LIMIT",
            Self::Distinct => "DISTINCT",
            Self::Join => "JOIN",
            Self::Union => "UNION",
            Self::Except => "EXCEPT",
            Self::Intersect => "INTERSECT",
            Self::As => "AS",
            Self::Expand => "EXPAND",
            Self::Let => "LET",
            Self::InsertInto => "INSERT INTO",
            Self::UpsertInto => "UPSERT INTO",
            Self::Update => "UPDATE",
            Self::Remove => "REMOVE",
            Self::Values => "VALUES",
            Self::Returning => "RETURNING",
            Self::Call => "CALL",
            Self::Decode => "DECODE",
            Self::Encode => "ENCODE",
            Self::Preview => "PREVIEW",
            Self::Commit => "COMMIT",
            Self::Create => "CREATE",
            Self::Endpoint => "ENDPOINT",
            Self::Trigger => "TRIGGER",
            Self::Job => "JOB",
            Self::View => "VIEW",
            Self::MaterializedView => "MATERIALIZED VIEW",
            Self::Webhook => "WEBHOOK",
            Self::Policy => "POLICY",
            Self::Do => "DO",
            Self::Every => "EVERY",
            Self::On => "ON",
        }
    }
}

/// The frozen reserved-keyword set (RFD-0001 §3), canonical surface text.
///
/// This is the single committed fixture: the freeze/golden test asserts the
/// language's keyword vocabulary equals exactly this slice. Adding, removing, or
/// renaming a keyword anywhere in the workspace requires editing this one slice and
/// updating the test that locks it — by design (closed-core enforcement).
pub const KEYWORDS: &[&str] = &[
    // Query / transform
    // (`FROM` removed in M6, ticket t73 / decision R — the leading `/path` is the source.)
    "WHERE",
    "SELECT",
    "EXTEND",
    "SET",
    "AGGREGATE",
    "GROUP BY",
    "ORDER BY",
    "LIMIT",
    "DISTINCT",
    "JOIN",
    "UNION",
    "EXCEPT",
    "INTERSECT",
    "AS",
    "EXPAND",
    // Functional core (M6, ticket t60) — a deliberate vocabulary addition (decision H).
    "LET",
    // Effects
    "INSERT INTO",
    "UPSERT INTO",
    "UPDATE",
    "REMOVE",
    "VALUES",
    "RETURNING",
    "CALL",
    // Codecs
    "DECODE",
    "ENCODE",
    // Plan
    "PREVIEW",
    "COMMIT",
    // Server DDL
    "CREATE",
    "ENDPOINT",
    "TRIGGER",
    "JOB",
    "VIEW",
    "MATERIALIZED VIEW",
    "WEBHOOK",
    "POLICY",
    "DO",
    "EVERY",
    "ON",
];

/// The frozen operator set (RFD-0001 §3, "Operators"). Lexer-facing; kept separate
/// from [`KEYWORDS`] because operators are punctuation/word tokens rather than
/// statement keywords. Frozen on the same terms as the keyword set.
pub const OPERATORS: &[&str] = &[
    "|>", "==", "<>", "<", ">", "<=", ">=", "AND", "OR", "NOT", "LIKE", "~", "ANY", "IN", "BETWEEN",
];

#[cfg(test)]
mod tests {
    use super::*;

    /// G1 / C1 — the keyword golden/freeze test. Asserts the `Keyword` enum's
    /// surface texts equal the `KEYWORDS` fixture exactly (same set, same count),
    /// so the two representations cannot drift, and locks the total count so a
    /// later ticket cannot silently add or drop a keyword.
    #[test]
    fn keyword_enum_matches_golden_fixture() {
        // The full set of Keyword variants, kept in step with the enum via an
        // exhaustive list. If a variant is added/removed, this list must change.
        let enum_texts: Vec<&str> = ALL_KEYWORDS.iter().map(|k| k.text()).collect();

        // Same multiset, order-independent.
        let mut from_enum = enum_texts.clone();
        let mut from_fixture: Vec<&str> = KEYWORDS.to_vec();
        from_enum.sort_unstable();
        from_fixture.sort_unstable();
        assert_eq!(
            from_enum, from_fixture,
            "Keyword enum surface texts must equal the KEYWORDS golden fixture (RFD §3)"
        );
    }

    /// Locks the exact frozen count. RFD §3 froze 38 reserved keywords; ticket t60
    /// deliberately added `LET` (decision H, the M6 functional core), taking the count to 39;
    /// ticket t73 (decision R) then deliberately *removed* `FROM` (the source position needs no
    /// keyword — a leading `/path` is the source), taking it back to 38. A diff to this number is
    /// the tripwire that a keyword was smuggled in or removed — editing it here is the *intended*
    /// change-control event for the `FROM` removal.
    #[test]
    fn keyword_count_is_frozen() {
        assert_eq!(
            KEYWORDS.len(),
            38,
            "the closed-core keyword set is frozen at 38 entries (RFD §3 + t60 `LET` − t73 `FROM`)"
        );
        // No duplicates in the fixture.
        let mut seen = std::collections::BTreeSet::new();
        for kw in KEYWORDS {
            assert!(seen.insert(*kw), "duplicate keyword in fixture: {kw}");
        }
    }

    /// Locks the frozen operator count (RFD §3 lists `|>` plus 14 comparison /
    /// logical / set operators = 15). Ticket t70 (RFD decision O) is a *deliberate
    /// vocabulary event*: the equivalence comparator `=` is reclassified — the lone
    /// `=` becomes the assignment/binding token (punctuation, like `=>`/`||`/`.`,
    /// not a comparison operator) and `==` takes its place as the comparator. The
    /// count therefore stays 15; this freeze test is the tripwire that the swap was
    /// the intended one-for-one edit and not an accidental add/drop.
    #[test]
    fn operator_count_is_frozen() {
        assert_eq!(
            OPERATORS.len(),
            15,
            "the operator set is frozen at 15 entries (RFD §3; `=`→`==` swap is t70 decision O)"
        );
        // The binding `=` is no longer a comparison operator; `==` is the comparator.
        assert!(
            OPERATORS.contains(&"=="),
            "`==` is the equivalence comparator (RFD decision O, t70)"
        );
        assert!(
            !OPERATORS.contains(&"="),
            "`=` is the assignment/binding token, not a comparison operator (t70)"
        );
    }

    /// Drift guard for `from_word`: every single-word keyword (no internal space)
    /// must round-trip `text -> from_word -> Keyword`, and every multi-word keyword
    /// must NOT be recognized as a single word (it is lexed as adjacent tokens).
    #[test]
    fn from_word_recognizes_exactly_single_word_keywords() {
        for kw in ALL_KEYWORDS {
            let text = kw.text();
            if text.contains(' ') {
                // Multi-word keyword: never matched as a single word.
                assert_eq!(
                    Keyword::from_word(text),
                    None,
                    "multi-word keyword `{text}` must not be a single-word match"
                );
            } else {
                assert_eq!(
                    Keyword::from_word(text),
                    Some(*kw),
                    "single-word keyword `{text}` must round-trip through from_word"
                );
            }
        }
        // Non-keywords are not recognized.
        assert_eq!(
            Keyword::from_word("where"),
            None,
            "case-sensitive: lowercase"
        );
        // `FROM` was removed from the closed core (t73): it is no longer a keyword in any case.
        assert_eq!(
            Keyword::from_word("FROM"),
            None,
            "the keyword was removed in t73"
        );
        assert_eq!(Keyword::from_word("GROUP"), None, "lead word of GROUP BY");
        assert_eq!(Keyword::from_word("BANANA"), None);
    }

    /// The exhaustive list of every `Keyword` variant, used by the golden test.
    const ALL_KEYWORDS: &[Keyword] = &[
        Keyword::Where,
        Keyword::Select,
        Keyword::Extend,
        Keyword::Set,
        Keyword::Aggregate,
        Keyword::GroupBy,
        Keyword::OrderBy,
        Keyword::Limit,
        Keyword::Distinct,
        Keyword::Join,
        Keyword::Union,
        Keyword::Except,
        Keyword::Intersect,
        Keyword::As,
        Keyword::Expand,
        Keyword::Let,
        Keyword::InsertInto,
        Keyword::UpsertInto,
        Keyword::Update,
        Keyword::Remove,
        Keyword::Values,
        Keyword::Returning,
        Keyword::Call,
        Keyword::Decode,
        Keyword::Encode,
        Keyword::Preview,
        Keyword::Commit,
        Keyword::Create,
        Keyword::Endpoint,
        Keyword::Trigger,
        Keyword::Job,
        Keyword::View,
        Keyword::MaterializedView,
        Keyword::Webhook,
        Keyword::Policy,
        Keyword::Do,
        Keyword::Every,
        Keyword::On,
    ];
}
