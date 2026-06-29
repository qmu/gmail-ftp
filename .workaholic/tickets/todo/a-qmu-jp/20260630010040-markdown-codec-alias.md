---
created_at: 2026-06-30T01:00:40+09:00
author: a@qmu.jp
type: bugfix
layer: [Domain]
effort: 1h
commit_hash:
category: Changed
depends_on: []
---

# Markdown codec: make `encode md` / `decode md` resolve (name mismatch)

Roadmap "Near-term backlog": `… encode md` errors. Confirmed: `… |> decode json |> encode md` →
`{"error":{"code":"unknown_codec","message":"unknown codec format: md"}}` while `encode yaml` works.

## Root cause — a name mismatch, not a missing codec

The markdown-with-front-matter codec exists, works, and **is** registered in `builtin_codecs()`
(`crates/codec/src/codecs/mod.rs:38`, `Arc::new(MarkdownFrontmatterCodec)`). But it registers under
`fmt() == "md+frontmatter"` (`crates/codec/src/codecs/markdown.rs:31`), while:

- the registry keys by that exact string and resolves by exact `get(fmt)` →
  `crates/core/src/registry.rs:522,534` (`key = codec.fmt()`, `resolve` → `UnknownCodec`);
- the parser passes the bare token through verbatim — `encode md` ⇒ `fmt="md"`
  (`crates/parser/src/grammar.rs:32`); and the generated grammar reference already advertises `md` as
  canonical (`docs/language.md`: `format = "json"|…|"csv"|"md"`).

So `"md"` (grammar/surface) never matches `"md+frontmatter"` (registry key).

## Plan

Pick the canonical name `md` and make it resolve. Simplest: change `MarkdownFrontmatterCodec::fmt()`
to return `"md"` (or add an alias map in `CodecRegistry` so `md` → the markdown codec). Verify
round-trip `decode md` / `encode md` over front-matter + body.

## Key files

- `crates/codec/src/codecs/markdown.rs:31` (`fmt()`), `crates/codec/src/codecs/mod.rs:38`
  (registration), `crates/core/src/registry.rs:522,534`, the `with_builtins` test at
  `registry.rs:821`.

## Considerations

- `docs/language.md` is **generated** (`xtask gen-docs`) — it already lists `md`; run
  `gen-docs --check` after to confirm no drift. Update the `with_builtins` test to expect `md`.
- Bump the patch in `crates/qfs/Cargo.toml`. Add an `encode md` recipe to the cookbook ratchet.
