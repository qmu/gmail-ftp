//! Verified-true ratchet over the cookbook Agent Skills (ticket: cookbook-articles-as-agent-skills).
//!
//! Each `docs/cookbook/*.md` article is the authored source of a generated Claude Code skill
//! (`plugins/qfs/skills/<name>/SKILL.md`, its body copied VERBATIM by `xtask gen-skills`). So every
//! runnable `qfs` recipe an agent would read out of a skill must actually PARSE on the shipped
//! grammar — a skill can never teach an agent a statement the binary rejects. This test extracts the
//! ` ```qfs ` fenced statements from the source articles and asserts every one parses.
//!
//! This is the narrative-cookbook sibling of `roadmap_cookbook.rs` (which ratchets the broad
//! `query-cookbook.md` catalogue). Testing the ARTICLES rather than the generated skills keeps the
//! check independent of whether `gen-skills` has been run, and the skill body equals the article
//! body by construction.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use qfs_parser::parse_statement;
use std::fs;
use std::path::PathBuf;

/// Floor on extracted statements — guards the extractor against silently breaking (e.g. a fenced
/// info-string change) and reporting "0 recipes, all pass".
const MIN_STATEMENTS: usize = 45;

/// The repo-root `docs/cookbook` dir (this test crate is `packages/qfs/crates/test`).
fn cookbook_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../../..")
        .join("docs/cookbook")
}

/// Extract the qfs STATEMENTS from a markdown article: the body of every ` ```qfs ` fenced block,
/// split on blank lines (a block may hold more than one statement, e.g. draft-then-send). Prose
/// placeholders like `<msg-id>` live OUTSIDE the fenced blocks, so what is extracted here is real,
/// runnable qfs source.
fn extract_statements(md: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut in_block = false;
    let mut buf = String::new();
    for line in md.lines() {
        if !in_block {
            if line.trim_start().starts_with("```qfs") {
                in_block = true;
                buf.clear();
            }
            continue;
        }
        if line.trim_start().starts_with("```") {
            in_block = false;
            for stmt in buf.split("\n\n") {
                let stmt = stmt.trim();
                if !stmt.is_empty() {
                    out.push(stmt.to_string());
                }
            }
            continue;
        }
        buf.push_str(line);
        buf.push('\n');
    }
    out
}

#[test]
fn every_cookbook_skill_recipe_parses() {
    let dir = cookbook_dir();
    let mut articles: Vec<PathBuf> = fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("reading {}: {e}", dir.display()))
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "md"))
        .collect();
    articles.sort();

    let mut total = 0usize;
    let mut failures: Vec<String> = Vec::new();
    for article in &articles {
        let md = fs::read_to_string(article)
            .unwrap_or_else(|e| panic!("reading {}: {e}", article.display()));
        for stmt in extract_statements(&md) {
            total += 1;
            if let Err(e) = parse_statement(&stmt) {
                failures.push(format!("{}: {:?} → {e}", article.display(), stmt));
            }
        }
    }

    assert!(
        total >= MIN_STATEMENTS,
        "only {total} qfs statements extracted from the cookbook (< {MIN_STATEMENTS}); the \
         extractor or the articles changed shape"
    );
    assert!(
        failures.is_empty(),
        "{} of {total} cookbook-skill recipes do NOT parse on the shipped grammar (a skill must \
         never teach an agent an invalid statement):\n{}",
        failures.len(),
        failures.join("\n")
    );
}
