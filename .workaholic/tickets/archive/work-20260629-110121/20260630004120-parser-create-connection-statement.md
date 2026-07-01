---
created_at: 2026-06-30T00:41:20+09:00
author: a@qmu.jp
type: enhancement
layer: [Domain]
effort:
commit_hash: 6d52bce
category: Added
depends_on: [20260630004110-design-connection-declaration-grammar.md]
---

# Parser: the `CREATE CONNECTION` statement

Part of EPIC `20260630004100`. Add the declaration statement to the language per the design ADR.

## Sub-tasks (each a ≤4h commit)

1. **Keywords** (`crates/lang/src/keywords.rs`): add `Connection`, `Driver`, `Secret`, and `At` (or
   reuse an existing locator keyword) to the keyword enum, the `from_str`/`as_str` maps, and the
   reserved set — mirroring how `Trigger`/`Policy` are wired.
2. **AST** (`crates/parser/src/ast.rs`): add `Statement::CreateConnection(ConnectionDecl)` and the
   `ConnectionDecl { name, driver, locator: Option<String>, secret: Option<SecretRef> }` +
   `SecretRef::{Env(String), Vault(String)}` types. Reject an inline secret literal at parse time.
3. **Grammar** (`crates/parser/src/grammar.rs`): parse the statement; a `SECRET` with a bare string
   (no `env:`/`vault:` prefix) is a structured parse error.
4. **Tests + ratchet**: parser unit tests for each form; add a `CREATE CONNECTION` recipe to the
   `roadmap_cookbook.rs` ratchet corpus (`docs/query-cookbook.md`) so the grammar is parse-pinned.
5. **Regenerate** `docs/language.md` (`cargo run -p xtask -- gen-docs`) and keep `gen-docs --check`
   green.

## Key files

- `crates/lang/src/keywords.rs`, `crates/parser/src/{ast.rs,grammar.rs,tests.rs}`,
  `docs/query-cookbook.md`.

## Considerations

- Parser only — no resolution/registry here (that is `…004130`/`…004140`). The statement should
  parse and round-trip; evaluating it is a later ticket.
