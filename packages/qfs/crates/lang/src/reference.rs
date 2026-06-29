//! `qfs-lang::reference` — the language **reference surface** (ticket t40).
//!
//! This module exposes the two pieces `cargo xtask gen-docs` renders `docs/language.md`
//! from, so the published reference is **derived from the binary's own data**, never
//! hand-authored twice (anti-drift, RFD §3 governance):
//!
//! - [`RESERVED_KEYWORDS`] — the §3 frozen reserved-word set. It is a **re-export alias**
//!   of [`crate::keywords::KEYWORDS`], the single committed fixture the freeze test locks.
//!   There is deliberately **no second transcription**: adding a keyword to `KEYWORDS`
//!   (the one source) flows straight into the generated docs, and the t40 docs-drift golden
//!   makes the doc fail CI until it is regenerated. The frozen-keyword test in this module
//!   asserts the alias and the source are the very same slice.
//! - [`grammar_ebnf`] — the pipe-SQL grammar in EBNF, the legible contract the AI agent
//!   reads. It is a `&'static str`; the `gen-docs` renderer embeds it verbatim into
//!   `docs/language.md` between fenced markers, so the committed grammar cannot drift from
//!   this constant.
//!
//! Both are pure data (no I/O, no deps) — this crate must stay wasm-clean (boundary guard
//! B7), so the reference surface adds nothing impure.

use crate::keywords::KEYWORDS;

/// The frozen reserved-keyword set (RFD-0001 §3), as the documentation surface reads it.
///
/// This is an **alias of [`crate::keywords::KEYWORDS`]** — the *single* committed fixture —
/// not a second hand-transcription. `gen-docs` renders the reserved-word table in
/// `docs/language.md` from this slice; because it *is* `KEYWORDS`, adding/removing a keyword
/// in the one source changes the generated docs, and the docs-drift golden fails CI until
/// `docs/language.md` is regenerated. The frozen-keyword test
/// ([`tests::reserved_keywords_is_the_frozen_set`]) locks this identity.
pub const RESERVED_KEYWORDS: &[&str] = KEYWORDS;

/// The pipe-SQL grammar in EBNF (RFD-0001 §2/§3) — the stable contract surface an AI agent
/// (and a human operator) reads. Returned as a `&'static str` so `gen-docs` can embed it
/// verbatim into `docs/language.md`; the docs-drift golden then locks the committed grammar
/// to this constant.
///
/// The grammar is intentionally the **shape** of the closed core (the frozen keywords +
/// operators wired into statement/pipeline forms), not a parser-complete grammar — E1 owns
/// the executable grammar. Every terminal here is drawn from the frozen
/// [`RESERVED_KEYWORDS`] / [`crate::keywords::OPERATORS`] sets, so the reference and the
/// lexer cannot disagree about the vocabulary.
#[must_use]
pub const fn grammar_ebnf() -> &'static str {
    // Kept as one frozen literal. Quoted lowercase terminals are reserved keywords (see
    // RESERVED_KEYWORDS, t74 decision S); quoted UPPERCASE terminals are word operators
    // (the OPERATORS set: AND/OR/NOT/LIKE/IN/ANY/BETWEEN). Unquoted lowercase names are
    // nonterminals. A new backend adds ZERO terminals here.
    "\
(* qfs pipe-SQL grammar (EBNF) — RFD-0001 §2/§3.                                  *)
(* The closed core: every quoted lowercase terminal is a frozen reserved keyword   *)
(* (see RESERVED_KEYWORDS, t74 decision S); a new backend adds ZERO terminals here. *)

(* A program is zero or more let bindings (M6 functional core, ticket t60) in       *)
(* scope for the statements that follow them — one statement per line, no `;`.       *)
program       = { binding } , statement ;
(* A let binds a relation (a pipeline) OR a first-class VALUE — a lambda or a scalar     *)
(* (M6, ticket t61, decision H \"functions are values\"). A named function is just a        *)
(* let-bound lambda — there is NO `def` keyword.                                          *)
binding       = \"let\" , name , \"=\" , ( pipeline | lambda | literal ) ;

(* A statement is a pipeline (which may terminate in a write stage, decision Q), the      *)
(* source-less verb-leading literal write, or a transaction block (M6, ticket t62); an     *)
(* optional plan_op wraps any of them.                                                    *)
statement     = ( pipeline | effect_literal | transaction ) , [ plan_op ] ;

(* ---- transactions (M6, ticket t62, decision G) ---- *)
(* A reversible-only, all-or-nothing block: a `;`-separated sequence of effect            *)
(* statements committed in source order. Every effect inside MUST be reversible —          *)
(* an irreversible effect (remove, an irreversible call) is a hard eval-time error, never  *)
(* a needs-an-ack prompt — so the block can always roll back. No nesting, no let inside.    *)
transaction   = \"transaction\" , \"{\" , [ effect_stmt , { \";\" , effect_stmt } , [ \";\" ] ] , \"}\" ;
effect_stmt   = effect_literal | ( pipeline , effect_stage ) ;

(* A pipeline is a source threaded through |> stages. Decision R (t73): the source     *)
(* position needs no `from` — a leading `/path` (or a let-bound name) IS the source.   *)
pipeline      = source , { \"|>\" , stage } ;

source        = path | id_ref | name ;   (* name = a let-bound relation *)
path          = \"/\" , segment , { \"/\" , segment } ;   (* absolute only, no cwd *)
id_ref        = \"id:\" , token ;

stage         = query_stage | effect_stage | codec_stage | call_stage ;

(* ---- query / transform (read) ---- *)
query_stage   = \"where\" , predicate
              | \"select\" , projection
              | \"extend\" , assignment
              | \"set\" , assignment
              | \"aggregate\" , agg_list
              | \"group by\" , column_list
              | \"order by\" , sort_list
              | \"limit\" , integer
              | \"distinct\"
              | \"join\" , source , \"on\" , predicate
              | \"union\" , source
              | \"except\" , source
              | \"intersect\" , source
              | \"expand\" , column ;

(* ---- effects (write) — decision Q (t72): a write reads as dataflow. With inflowing  *)
(* rows it is a TERMINAL pipeline stage; the upstream relation is what it writes        *)
(* (insert/upsert) or the rows it rewrites/removes in place (update/remove, whose target *)
(* and where are the upstream `/path |> where …`). returning rides as a trailing stage.  *)
effect_stage  = \"insert into\" , target
              | \"upsert into\" , target
              | \"update\" , \"set\" , assignment
              | \"remove\"
              | \"returning\" , projection ;

(* The source-less LITERAL write leads with the verb (no inflowing rows to consume);     *)
(* `values` supplies the rows inline. This is the only write form that is not a stage.    *)
effect_literal= \"insert into\" , target , \"values\" , row_list , [ \"returning\" , projection ]
              | \"upsert into\" , target , \"values\" , row_list , [ \"returning\" , projection ] ;

(* ---- procedures (the irreducible state transitions) ---- *)
call_stage    = \"call\" , qualified_proc , \"(\" , [ arg_list ] , \")\" ;
qualified_proc= driver_id , \".\" , action ;          (* e.g. mail.send, git.merge *)

(* ---- codecs (blob <-> relational) ---- *)
codec_stage   = \"decode\" , format | \"encode\" , format ;
format        = \"json\" | \"jsonl\" | \"yaml\" | \"toml\" | \"csv\" | \"md\" ;

(* ---- plan operator (preview is default; commit applies) ---- *)
plan_op       = \"preview\" | \"commit\" ;

(* ---- predicate / expression core ---- *)
(* Decision O (t70): `=` ALWAYS binds (let / extend / set / named arg);             *)
(* equivalence is the explicit `==`. Unlike SQL, a lone `=` never compares.         *)
predicate     = expr , { ( \"AND\" | \"OR\" ) , expr } | \"NOT\" , predicate ;
expr          = operand , [ comparison , operand ] ;
comparison    = \"==\" | \"<>\" | \"<\" | \">\" | \"<=\" | \">=\"
              | \"LIKE\" | \"~\" | \"IN\" | \"ANY\" | \"BETWEEN\" ;
operand       = column | literal | call | lambda ;

(* ---- lambdas as values + higher-order fns (M6, ticket t61, decision H) ---- *)
(* Functions are values: a lambda rides the EXPRESSION grammar and reuses the `=>`   *)
(* token — it adds ZERO keywords and ZERO operators (the closed core is untouched).   *)
(* The parenthesised parameter list distinguishes it from a named arg / sub-expr.     *)
lambda        = \"(\" , [ param , { \",\" , param } ] , \")\" , \"=>\" , expr ;
param         = name , [ \":\" , type ] ;   (* `: type` is parsed-and-retained *)
(* `map` / `filter` / `reduce` are ordinary stdlib functions (a `call`), NOT keywords: *)
(*   map(collection, lambda) | filter(collection, lambda) | reduce(collection, lambda, init) *)
call          = name , \"(\" , [ expr , { \",\" , expr } ] , \")\" ;

(* ---- server DDL (sugar over the write surface, RFD §8) ---- *)
ddl           = \"create\" , ( endpoint | trigger | job | view | webhook | policy ) ;
endpoint      = \"endpoint\" , name , \"do\" , statement ;
trigger       = \"trigger\" , name , \"on\" , event , \"do\" , statement ;
job           = \"job\" , name , \"every\" , interval , \"do\" , statement ;
view          = ( \"view\" | \"materialized view\" ) , name , \"as\" , pipeline ;
webhook       = \"webhook\" , name , \"do\" , statement ;
policy        = \"policy\" , name , predicate ;

(* Lowercase nonterminals (segment, token, projection, assignment, agg_list,       *)
(* column_list, sort_list, integer, target, row_list, column, arg_list, action,    *)
(* driver_id, name, event, interval, literal) are E1's lexical/structural detail.   *)
"
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keywords::{KEYWORDS, OPERATORS};

    /// Frozen-keyword test (t40 acceptance): `RESERVED_KEYWORDS` *is* the one committed
    /// frozen set `KEYWORDS` — same pointer, same length, same elements. There is no second
    /// transcription, so adding a keyword without regenerating the docs is caught by the
    /// docs-drift golden, and adding one without updating the source slice is impossible.
    #[test]
    fn reserved_keywords_is_the_frozen_set() {
        // Identity: the alias points at the exact same slice (no copy, no re-transcription).
        assert!(
            std::ptr::eq(RESERVED_KEYWORDS, KEYWORDS),
            "RESERVED_KEYWORDS must be the SAME slice as KEYWORDS, not a re-transcription"
        );
        assert_eq!(RESERVED_KEYWORDS.len(), KEYWORDS.len());
        assert_eq!(RESERVED_KEYWORDS, KEYWORDS);
        // And it carries the §3-frozen count (mirrors keywords.rs's own freeze test, so a
        // keyword smuggled in anywhere fails here too).
        assert_eq!(
            RESERVED_KEYWORDS.len(),
            39,
            "the closed-core reserved-word set is frozen at 39 entries \
             (RFD §3 + t60 `LET` − t73 `FROM` + t62 `TRANSACTION`)"
        );
    }

    /// The grammar references only the frozen vocabulary: every reserved keyword and every
    /// operator named in the EBNF is a member of the frozen sets, and conversely every frozen
    /// keyword appears in the grammar — so the reference cannot drift from the lexer's
    /// vocabulary. (A purely textual check; structural parsing is E1's concern.)
    #[test]
    fn grammar_uses_only_frozen_vocabulary() {
        let ebnf = grammar_ebnf();
        // Every frozen keyword's quoted form appears as a terminal in the grammar.
        for kw in RESERVED_KEYWORDS {
            let quoted = format!("\"{kw}\"");
            assert!(
                ebnf.contains(&quoted),
                "frozen keyword {kw} must appear as a terminal in grammar_ebnf()"
            );
        }
        // Every operator's quoted form appears too (spot the comparison/logical/pipe set).
        for op in OPERATORS {
            let quoted = format!("\"{op}\"");
            assert!(
                ebnf.contains(&quoted),
                "frozen operator {op} must appear in grammar_ebnf()"
            );
        }
    }
}
