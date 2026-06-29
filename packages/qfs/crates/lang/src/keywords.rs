//! The frozen reserved-keyword set, transcribed from RFD-0001 §3 ("Closed core
//! keywords (reserved, frozen)") in their **canonical lowercase** spelling.
//!
//! ## Case policy (M6, ticket t74, roadmap decision S)
//! Keywords are **lowercase** (`where`, `select`, `let`, `insert into`, `join`,
//! `policy`, …): paths, column names, and bindings carry the visual weight, so the
//! closed keyword set stays quiet. Recognition is **case-insensitive** — the lexer
//! folds a word's case before matching ([`Keyword::from_word`]), so `SELECT`,
//! `Select`, and `select` all lex to the same [`Keyword`] — but the **canonical /
//! rendered** form ([`Keyword::text`], [`KEYWORDS`], the EBNF, the generated docs, and
//! error messages) is lowercase. This was a deliberate readability decision, **not** a
//! new capability or a semantic change; the count below is untouched (39 stays 39 — only
//! the *case* of the strings changes). Accepting any case keeps every pre-t74 uppercase
//! corpus query valid while making lowercase the one blessed form.
//!
//! [`KEYWORDS`] is the single committed fixture (fidelity guard G1 / acceptance
//! criterion C1): the golden test in `lib`'s `tests` module asserts against *this*
//! slice, so there is no second hand-transcription that could drift out of sync.
//! Multi-word forms (`group by`, `insert into`, `materialized view`) are stored as
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
    // -- Transactions (M6, ticket t62) --
    // `TRANSACTION` is a *deliberate* addition to the frozen RFD §3 vocabulary — the second and
    // last new keyword the whole roadmap permits (decision G; the first is `LET`, t60). It opens a
    // reversible-only, all-or-nothing block (`TRANSACTION { … }`); an irreversible effect inside is
    // a hard eval-time error. The freeze tests below are updated in step (38 → 39) precisely so this
    // addition is reviewed, not smuggled in.
    Transaction,
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
    /// Recognition is **case-insensitive** (t74, decision S): the word is ASCII-folded
    /// to its canonical lowercase spelling before matching, so `SELECT`, `Select`, and
    /// `select` all map to [`Keyword::Select`]. The canonical/rendered form is lowercase
    /// (see [`Keyword::text`]); accepting any case keeps every pre-t74 uppercase query
    /// valid while lowercase is the one blessed form.
    ///
    /// This recognizes only the **single-word** keywords. Multi-word keywords
    /// (`group by`, `order by`, `insert into`, `upsert into`, `materialized
    /// view`) are intentionally *not* matched here: the lexer's contract (RFD §3,
    /// t03) is that multi-word keywords are emitted as separate adjacent tokens
    /// and composition is the parser's job. The lead word of a multi-word keyword
    /// (e.g. `group`, `insert`) is therefore returned as `None` and surfaces as an
    /// identifier; the parser stitches the pair back together (case-insensitively).
    #[must_use]
    pub fn from_word(word: &str) -> Option<Self> {
        // Case-insensitive: fold to the canonical lowercase spelling, then match.
        Some(match word.to_ascii_lowercase().as_str() {
            "where" => Self::Where,
            "select" => Self::Select,
            "extend" => Self::Extend,
            "set" => Self::Set,
            "aggregate" => Self::Aggregate,
            "limit" => Self::Limit,
            "distinct" => Self::Distinct,
            "join" => Self::Join,
            "union" => Self::Union,
            "except" => Self::Except,
            "intersect" => Self::Intersect,
            "as" => Self::As,
            "expand" => Self::Expand,
            "let" => Self::Let,
            "update" => Self::Update,
            "remove" => Self::Remove,
            "values" => Self::Values,
            "returning" => Self::Returning,
            "call" => Self::Call,
            "transaction" => Self::Transaction,
            "decode" => Self::Decode,
            "encode" => Self::Encode,
            "preview" => Self::Preview,
            "commit" => Self::Commit,
            "create" => Self::Create,
            "endpoint" => Self::Endpoint,
            "trigger" => Self::Trigger,
            "job" => Self::Job,
            "view" => Self::View,
            "webhook" => Self::Webhook,
            "policy" => Self::Policy,
            "do" => Self::Do,
            "every" => Self::Every,
            "on" => Self::On,
            _ => return None,
        })
    }

    /// The canonical surface text of this keyword — **lowercase** (RFD §3 + t74
    /// decision S). Recognition is case-insensitive, but this is the one blessed
    /// spelling rendered in the EBNF, the generated docs, and diagnostics.
    #[must_use]
    pub const fn text(self) -> &'static str {
        match self {
            Self::Where => "where",
            Self::Select => "select",
            Self::Extend => "extend",
            Self::Set => "set",
            Self::Aggregate => "aggregate",
            Self::GroupBy => "group by",
            Self::OrderBy => "order by",
            Self::Limit => "limit",
            Self::Distinct => "distinct",
            Self::Join => "join",
            Self::Union => "union",
            Self::Except => "except",
            Self::Intersect => "intersect",
            Self::As => "as",
            Self::Expand => "expand",
            Self::Let => "let",
            Self::InsertInto => "insert into",
            Self::UpsertInto => "upsert into",
            Self::Update => "update",
            Self::Remove => "remove",
            Self::Values => "values",
            Self::Returning => "returning",
            Self::Call => "call",
            Self::Transaction => "transaction",
            Self::Decode => "decode",
            Self::Encode => "encode",
            Self::Preview => "preview",
            Self::Commit => "commit",
            Self::Create => "create",
            Self::Endpoint => "endpoint",
            Self::Trigger => "trigger",
            Self::Job => "job",
            Self::View => "view",
            Self::MaterializedView => "materialized view",
            Self::Webhook => "webhook",
            Self::Policy => "policy",
            Self::Do => "do",
            Self::Every => "every",
            Self::On => "on",
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
    // Query / transform — canonical lowercase (t74, decision S).
    // (`from` removed in M6, ticket t73 / decision R — the leading `/path` is the source.)
    "where",
    "select",
    "extend",
    "set",
    "aggregate",
    "group by",
    "order by",
    "limit",
    "distinct",
    "join",
    "union",
    "except",
    "intersect",
    "as",
    "expand",
    // Functional core (M6, ticket t60) — a deliberate vocabulary addition (decision H).
    "let",
    // Effects
    "insert into",
    "upsert into",
    "update",
    "remove",
    "values",
    "returning",
    "call",
    // Transactions (M6, ticket t62) — a deliberate vocabulary addition (decision G).
    "transaction",
    // Codecs
    "decode",
    "encode",
    // Plan
    "preview",
    "commit",
    // Server DDL
    "create",
    "endpoint",
    "trigger",
    "job",
    "view",
    "materialized view",
    "webhook",
    "policy",
    "do",
    "every",
    "on",
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
    /// keyword — a leading `/path` is the source), taking it back to 38; ticket t62 (decision G)
    /// deliberately added `TRANSACTION` (the reversible-only block — the second and last new
    /// keyword the roadmap permits), taking it to 39. A diff to this number is the tripwire that a
    /// keyword was smuggled in or removed — editing it here is the *intended* change-control event
    /// for the `TRANSACTION` addition.
    #[test]
    fn keyword_count_is_frozen() {
        assert_eq!(
            KEYWORDS.len(),
            39,
            "the closed-core keyword set is frozen at 39 entries \
             (RFD §3 + t60 `LET` − t73 `FROM` + t62 `TRANSACTION`)"
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
        // `FROM` was removed from the closed core (t73): it is no longer a keyword in any case.
        assert_eq!(
            Keyword::from_word("FROM"),
            None,
            "the keyword was removed in t73"
        );
        assert_eq!(Keyword::from_word("GROUP"), None, "lead word of GROUP BY");
        assert_eq!(Keyword::from_word("BANANA"), None);
    }

    /// Case policy (t74, decision S): recognition is **case-insensitive** — every case
    /// spelling of a keyword word folds to the same [`Keyword`] — but the canonical
    /// rendered form ([`Keyword::text`]) is lowercase. So `select`, `SELECT`, `Select`,
    /// and `sElEcT` all lex to `Select`, and `Select::text()` is `"select"`.
    #[test]
    fn keyword_recognition_is_case_insensitive_lowercase_canonical() {
        for spelling in ["where", "WHERE", "Where", "wHeRe"] {
            assert_eq!(
                Keyword::from_word(spelling),
                Some(Keyword::Where),
                "`{spelling}` must fold to the `where` keyword (case-insensitive, t74)"
            );
        }
        // The canonical render is lowercase, regardless of how it was written.
        assert_eq!(Keyword::Where.text(), "where");
        assert_eq!(Keyword::Select.text(), "select");
        assert_eq!(Keyword::InsertInto.text(), "insert into");
        // Every fixture string is already lowercase (the blessed form).
        for kw in KEYWORDS {
            assert_eq!(
                *kw,
                kw.to_ascii_lowercase(),
                "the canonical keyword fixture must be lowercase (t74): {kw}"
            );
        }
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
        Keyword::Transaction,
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
