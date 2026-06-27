//! Query-cookbook coverage test (`docs/query-cookbook.md`).
//!
//! The query cookbook is a worked catalogue of the queries qfs will support once the whole roadmap
//! (M0→M+) is built. Because the cookbook is **direction, not documentation**, most recipes use the *target*
//! grammar that the M-tickets will deliver — so the catalogue runs ahead of the binary on purpose.
//!
//! Every ` ```qfs ` recipe carries a machine-readable header comment:
//!
//! ```text
//! # qfs-cookbook: grammar=core|extended; milestone=M0..M+; features=a,b,c
//! ```
//!
//! - **`grammar=core`** — parses with TODAY's `qfs_parser`. The tags are kept honest by the
//!   ignored `retag_cookbook_grammar_by_parse_result` test below (run it after editing the cookbook).
//!   This test then ENFORCES that every `core` recipe still parses, so a parser regression or a
//!   broken edit to a working recipe fails CI.
//! - **`grammar=extended`** — does not parse yet (it needs M6 grammar like `LET`/lambda/
//!   `TRANSACTION`, the statement-leading write forms, the richer DDL, or a not-yet-shipped
//!   construct). These are tracked as a LIVING COVERAGE number that ratchets upward as milestones
//!   land; promote them to `core` (via the retag test) once they parse.
//!
//! The `core` count is also ratcheted by [`BASELINE_CORE`]: it may only grow. That converts "how
//! much of the planned language actually works today" into a number CI defends.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use qfs_parser::parse_statement;

const HEADER_PREFIX: &str = "# qfs-cookbook:";
/// Floor on the number of tagged recipes — guards the extractor against silently breaking and the
/// test passing vacuously. The cookbook ships well over this.
const MIN_RECIPES: usize = 250;
/// Ratchet floor: the number of recipes that parse with today's grammar may only INCREASE. Bump
/// this (never lower it) after running the retag test when new grammar lands and coverage grows.
const BASELINE_CORE: usize = 112;

struct Recipe {
    ordinal: usize,
    grammar: String,
    milestone: String,
    src: String,
    first_code_line: String,
}

fn cookbook_path() -> &'static str {
    concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../../docs/query-cookbook.md"
    )
}

fn read_cookbook() -> String {
    std::fs::read_to_string(cookbook_path())
        .unwrap_or_else(|e| panic!("cannot read query-cookbook.md at {}: {e}", cookbook_path()))
}

/// Pull every ```qfs fenced block that carries the cookbook header, with its tags and body.
fn extract_recipes(md: &str) -> Vec<Recipe> {
    let mut out = Vec::new();
    let mut lines = md.lines().peekable();
    let mut ordinal = 0usize;
    while let Some(line) = lines.next() {
        if line.trim_end() != "```qfs" {
            continue;
        }
        let mut body = Vec::new();
        for inner in lines.by_ref() {
            if inner.trim_end() == "```" {
                break;
            }
            body.push(inner);
        }
        let header = body.first().copied().unwrap_or("").trim();
        if !header.starts_with(HEADER_PREFIX) {
            continue; // an untagged narrative block elsewhere in the cookbook — not a recipe.
        }
        ordinal += 1;
        let (grammar, milestone) = parse_header(header, ordinal);
        let first_code_line = body
            .iter()
            .skip(1)
            .map(|l| l.trim())
            .find(|l| !l.is_empty() && !l.starts_with('#'))
            .unwrap_or("<no code>")
            .to_string();
        out.push(Recipe {
            ordinal,
            grammar,
            milestone,
            src: body.join("\n"),
            first_code_line,
        });
    }
    out
}

fn parse_header(header: &str, ordinal: usize) -> (String, String) {
    let rest = header.strip_prefix(HEADER_PREFIX).unwrap_or("").trim();
    let mut grammar = None;
    let mut milestone = None;
    for part in rest.split(';') {
        let part = part.trim();
        if let Some(v) = part.strip_prefix("grammar=") {
            grammar = Some(v.trim().to_string());
        } else if let Some(v) = part.strip_prefix("milestone=") {
            milestone = Some(v.trim().to_string());
        }
    }
    let grammar =
        grammar.unwrap_or_else(|| panic!("recipe #{ordinal}: header missing grammar=: {header:?}"));
    assert!(
        grammar == "core" || grammar == "extended",
        "recipe #{ordinal}: grammar must be core|extended, got {grammar:?}"
    );
    let milestone = milestone
        .unwrap_or_else(|| panic!("recipe #{ordinal}: header missing milestone=: {header:?}"));
    (grammar, milestone)
}

#[test]
fn cookbook_core_parses_and_coverage_ratchets() {
    let md = read_cookbook();
    let recipes = extract_recipes(&md);

    assert!(
        recipes.len() >= MIN_RECIPES,
        "only {} tagged cookbook recipes found (< {MIN_RECIPES}); the extractor or the cookbook changed shape",
        recipes.len()
    );

    let mut core_total = 0usize;
    let mut core_failures: Vec<String> = Vec::new();
    let mut ext_total = 0usize;
    let mut ext_parsing = 0usize;
    let mut by_milestone: std::collections::BTreeMap<String, (usize, usize)> = Default::default();

    for r in &recipes {
        let parsed = parse_statement(&r.src).is_ok();
        let entry = by_milestone.entry(r.milestone.clone()).or_default();
        entry.1 += 1;
        if parsed {
            entry.0 += 1;
        }
        if r.grammar == "core" {
            core_total += 1;
            if !parsed {
                let err = parse_statement(&r.src)
                    .err()
                    .map(|e| e.message)
                    .unwrap_or_default();
                core_failures.push(format!(
                    "  recipe #{} (milestone {}): {} -> {}",
                    r.ordinal, r.milestone, r.first_code_line, err
                ));
            }
        } else {
            ext_total += 1;
            if parsed {
                ext_parsing += 1;
            }
        }
    }

    eprintln!("\n=== query-cookbook coverage ===");
    eprintln!("recipes total      : {}", recipes.len());
    eprintln!(
        "grammar=core       : {}/{} parse (enforced; ratchet floor {BASELINE_CORE})",
        core_total - core_failures.len(),
        core_total
    );
    eprintln!("grammar=extended   : {ext_parsing}/{ext_total} parse today (tracked, climbs with M-tickets)");
    eprintln!("-- parse coverage by milestone (parsing / total) --");
    for (ms, (ok, tot)) in &by_milestone {
        eprintln!("   {ms:6}: {ok}/{tot}");
    }
    eprintln!("========================================\n");

    assert!(
        core_failures.is_empty(),
        "{} grammar=core cookbook recipe(s) no longer parse. Fix the recipe, or run the ignored \
         `retag_cookbook_grammar_by_parse_result` test to re-tag honestly:\n{}",
        core_failures.len(),
        core_failures.join("\n")
    );
    assert!(
        core_total >= BASELINE_CORE,
        "cookbook core-parse coverage dropped to {core_total} (< ratchet floor {BASELINE_CORE}); a \
         parser change or edit regressed previously-working recipes"
    );
}

/// One-shot maintenance utility (run with `--ignored`): rewrite each recipe's `grammar=` tag to
/// match whether it parses with today's grammar, so the published tags never lie. Run it after
/// adding/editing cookbook recipes, then bump [`BASELINE_CORE`] if the core count grew.
#[test]
#[ignore = "maintenance: rewrites docs/query-cookbook.md grammar= tags to match parse reality"]
fn retag_cookbook_grammar_by_parse_result() {
    let md = read_cookbook();
    let mut out_lines: Vec<String> = Vec::new();
    let mut lines = md.lines().peekable();
    let mut changed = 0usize;
    while let Some(line) = lines.next() {
        out_lines.push(line.to_string());
        if line.trim_end() != "```qfs" {
            continue;
        }
        // Buffer the block body to decide its grammar, then flush.
        let mut body: Vec<String> = Vec::new();
        let mut closed = false;
        for inner in lines.by_ref() {
            if inner.trim_end() == "```" {
                closed = true;
                let header_is_cookbook = body
                    .first()
                    .map(|h| h.trim().starts_with(HEADER_PREFIX))
                    .unwrap_or(false);
                if header_is_cookbook {
                    let src = body.join("\n");
                    let want = if parse_statement(&src).is_ok() {
                        "core"
                    } else {
                        "extended"
                    };
                    let header = &body[0];
                    let retagged = retag_header(header, want);
                    if &retagged != header {
                        changed += 1;
                    }
                    body[0] = retagged;
                }
                out_lines.append(&mut body);
                out_lines.push(inner.to_string());
                break;
            }
            body.push(inner.to_string());
        }
        if !closed {
            out_lines.append(&mut body);
        }
    }
    let mut text = out_lines.join("\n");
    if md.ends_with('\n') {
        text.push('\n');
    }
    std::fs::write(cookbook_path(), text).expect("write query-cookbook.md");
    eprintln!("retag_cookbook_grammar_by_parse_result: {changed} grammar= tag(s) updated");
}

/// Replace the `grammar=<x>` token inside a cookbook header line with `grammar=<want>`.
fn retag_header(header: &str, want: &str) -> String {
    let Some(i) = header.find("grammar=") else {
        return header.to_string();
    };
    let after = &header[i + "grammar=".len()..];
    let end = after
        .find(|c: char| c == ';' || c.is_whitespace())
        .unwrap_or(after.len());
    format!("{}grammar={want}{}", &header[..i], &after[end..])
}
