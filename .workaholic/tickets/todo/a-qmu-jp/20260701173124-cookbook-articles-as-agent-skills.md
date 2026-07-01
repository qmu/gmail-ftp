---
created_at: 2026-07-01T17:31:24+09:00
author: a@qmu.jp
type: enhancement
layer: [UX, Infrastructure]
effort:
commit_hash:
category: Added
depends_on: []
---

# Turn each cookbook article into a Claude Code Agent Skill (generated via xtask)

Each narrative cookbook article (`docs/cookbook/*.md`) becomes an on-demand **Claude Code Agent
Skill** (a `SKILL.md` the harness loads when a task matches it), so qfs's how-to knowledge is
reachable by an AI agent — not only a human reader — while staying a **single authored source** (no
drift). The human article stays the source of truth; the `SKILL.md` is **generated from it** by a
new `cargo xtask` step, gated in CI exactly like `gen-docs`.

Balance the three demands the owner named:

- **Human readability** — the article stays the primary human doc (VitePress), understandable on
  first read, in the user's vocabulary.
- **AI-feeding efficiency** — the generated skill carries a tight `name` + "Use when…" `description`
  (so the harness loads it ONLY when relevant) and a progressive-disclosure body (H2/H3 sections),
  not a wall of prose.
- **Comprehensiveness** — every recipe in the article reaches the skill; comprehensive means
  **verified-true**, not padded.

## The two "skill" concepts (do not conflate)

qfs already has (a) an EMBEDDED *product* skill (`crates/skill`, printed by `qfs skill`) and (b) the
Claude Code **Agent-Skill** (`plugins/qfs/skills/qfs/SKILL.md` — YAML `name`+`description` + a
progressive body, registered in `marketplace.json`, symlinked into `.claude/skills/`). **This ticket
produces (b)-format skills**, one per cookbook article. It does NOT change the embedded product skill
(a), and it does NOT touch the `gen-docs` anti-drift path (`docs/{language,drivers,server}.md`).

## Plan

1. **Author skill metadata on each article.** Add a frontmatter block to each
   `docs/cookbook/<topic>.md` carrying the skill `name` (e.g. `qfs-gmail`) and a one-line "Use when…"
   `description` (the trigger the harness matches). The body (the recipes) is the article as written
   — ONE authored source per topic.
2. **The generator (xtask).** Add `cargo run -p xtask -- gen-skills [--check]` that reads each
   cookbook article + its frontmatter and emits `plugins/qfs/skills/<name>/SKILL.md` (YAML `name` +
   `description`, then the article body structured for progressive disclosure). Mirror the `gen-docs`
   command-script pattern EXACTLY: developer and CI invoke the same command; `--check` is the
   anti-drift gate that fails if a committed skill diverges from its source article.
3. **Register the generated skills.** Add each skill dir to `.claude-plugin/marketplace.json`
   `skills[]` and the `.claude/skills/` symlinks (generate/verify these in the same step) so Claude
   Code discovers and loads them.
4. **Scope the article set.** One skill per narrative article: `gmail`, `databases`, `files`,
   `cross-service`, `code`, `automation`, plus a catalog/router `index` skill. The 250-recipe
   `docs/query-cookbook.md` catalogue stays as-is (its own ratchet-tested reference), OUT of this
   per-article set.
5. **Extend the verified-true ratchet.** Every qfs recipe carried into a generated skill must parse
   — extend `packages/qfs/crates/test/tests/roadmap_cookbook.rs` (or add a sibling) to cover the
   generated `SKILL.md` recipes, so a skill can never ship an example that does not run on the binary.

## Key files

- `docs/cookbook/{index,gmail,databases,files,cross-service,code,automation}.md` — add skill
  frontmatter; these stay the authored SOURCE (VitePress renders them unchanged for humans).
- `packages/qfs/xtask/src/main.rs` (+ a `gen_skills` module) — the generator, mirroring `gen-docs`;
  `packages/qfs/crates/qfs/src/docs.rs` is the reference for the render/`--check` pattern.
- `plugins/qfs/skills/<name>/SKILL.md` — the GENERATED output (template:
  `plugins/qfs/skills/qfs/SKILL.md`).
- `.claude-plugin/marketplace.json` `skills[]` + `.claude/skills/` symlinks — registration/loading.
- `packages/qfs/crates/test/tests/roadmap_cookbook.rs` — extend the parse ratchet to the skills.
- `CLAUDE.md` — document `gen-skills --check` alongside `gen-docs --check` in Build & test.

## Quality Gate

The objective gate the `/drive` approval MUST pass (owner-chosen: parse-ratchet + frontmatter +
loads):

1. **Generated & anti-drift green:** `cargo run -p xtask -- gen-skills --check` passes — every
   committed `plugins/qfs/skills/<name>/SKILL.md` EXACTLY matches what the generator emits from its
   source article (no hand-edits, no drift) — the mirror of `gen-docs --check`.
2. **Valid frontmatter:** every generated `SKILL.md` has a non-empty `name` and a `description` in
   the "Use when…" trigger form — a lint/test asserts both fields present, the description non-empty
   and within a size budget (the AI-feeding-efficiency bound).
3. **Every recipe parses (verified-true):** the extended `roadmap_cookbook`-style ratchet asserts
   every qfs recipe in every generated skill parses on the shipped `core` grammar (or is honestly
   seam-tagged); baseline coverage may only GROW, never shrink.
4. **Loads in Claude Code:** each skill is registered (`marketplace.json` `skills[]` +
   `.claude/skills/` symlink) and loads — verified by invoking the skill in a Claude Code session,
   and backed by a registration lint asserting every generated skill dir is listed AND symlinked.
5. **Balance read-through (reviewer):** a human reviewer confirms each skill reads well for a person
   AND is tight for an agent (bounded description, progressive H2/H3 disclosure, no marketing
   adjectives per `objective-documentation`) — the subjective half, gated by 1–4 mechanically.

**Edge cases to cover:** an article whose recipes are seam-marked "coming soon" → the skill carries
the SAME honest not-yet marking, never a false claim; an article with no runnable recipe → still a
valid skill (ratchet trivially passes); a description over the size budget → the lint fails (protects
AI-feeding efficiency); `gen-skills` run twice → byte-identical output (deterministic, like gen-docs).

## Considerations

- **Single source of truth (owner decision):** the article is authored; the `SKILL.md` is GENERATED
  (never hand-edited) via xtask + `--check`. Same anti-drift discipline as the generated reference
  docs — do NOT create a hand-maintained third copy that can drift from the article or the binary.
- **Do NOT touch** the `gen-docs` path (`docs/{language,drivers,server}.md`) or the embedded product
  skill (`crates/skill`) — separate artifacts with their own guards.
- **Experimental, no back-compat:** if surfacing the articles as skills warrants restructuring the
  cookbook, a hard break is fine (no migration/deprecation window) — see the project policy.
- **Terminology:** generated skills reuse the qfs ubiquitous language (CONNECT, describe/preview/
  commit, driver/registry) EXACTLY — no synonyms, so an agent never learns a wrong concept boundary.
- Cross-reference the open todo `20260630203050` (qfs as a Claude plugin/MCP): it shares the
  `plugins/qfs` skill delivery surface; this ticket adds per-article skills to that surface but is
  independent (no key-file conflict).
- Bump the patch version on the shipped PR (`CLAUDE.md`).

## Policies

- `implementation/accessibility-first` + `planning/accessibility-first` — CORE: qfs how-to becomes
  reachable by AI agents (AI as information consumer), in the same structured form served to humans;
  stable heading-level reference points.
- `design/modeless-design` — skills present composable, one-shot recipes (matching
  describe→preview→commit), directly invokable by an agent, not only a linear tutorial.
- `design/self-explanatory-ui` + `implementation/objective-documentation` — the human-readability +
  comprehensiveness balance: understandable on first read, factual/verifiable, no evaluative
  adjectives.
- `implementation/command-scripts` — `gen-skills` is a runnable xtask command CI and developers
  invoke identically (the `gen-docs` pattern).
- `planning/terminology`, `implementation/directory-structure`, `implementation/coding-standards` —
  one-word vocabulary, structural discoverability of the skill files, always apply.
