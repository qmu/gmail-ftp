---
created_at: 2026-07-03T04:00:00+09:00
author: a@qmu.jp
type: enhancement
layer: [UX, Domain]
effort:
category: Added
depends_on: [20260703030000-paste-back-browser-consent.md]
---

# In-language account declaration: a CREATE ACCOUNT statement (values stay out-of-band)

Owner ask (2026-07-03, first-user session): the setup surface should be expressible in the QUERY
LANGUAGE, not only as CLI subcommands — "I was not expecting qfs subcommand but a part of
syntax". Mounts already are (`CONNECT /mail TO gmail ACCOUNT 'a@qmu.jp'` / `DISCONNECT`, verified
live; the CLI is the twin). The remaining CLI-only layer is the ACCOUNT declaration
(`qfs account add/remove/rotate/revoke`).

## The boundary to keep (RFD §10 / §4.5 — non-negotiable)

A qfs statement is pure, previewable, logged, audited TEXT. A secret VALUE never appears in it.
Secret REFERENCES are fine (`SECRET 'env:VAR'` / `'vault:…'`, as `CREATE CONNECTION` already
does). So the in-language surface declares the account's EXISTENCE and METADATA; sealing the
token bytes stays out-of-band (stdin import or the paste-back browser consent,
`20260703030000`).

## Sketch (design to confirm in-ticket, not prescribed)

```qfs
CREATE ACCOUNT google 'a@qmu.jp'                          -- declare; token sealed separately
CREATE ACCOUNT github 'work' SECRET 'vault:github/work'   -- reference form, mirrors CREATE CONNECTION
REMOVE /sys/accounts/google/a@qmu.jp                      -- if accounts get a /sys surface
```

## Open design decisions (flag, decide with the owner before implementing)

1. **What state does the statement write?** `account add` today does two things: seal the token
   (out-of-band — stays CLI) and record consent rows keyed by `(kind, account)`. Does CREATE
   ACCOUNT record the consent (it is metadata, but the t54 gate requires a signed-in operator —
   the statement path must enforce the same gate), or only declare a label the CLI later fills?
2. **A `/sys/accounts` read surface?** `qfs account list` reads the vault listing today; a
   `/sys/accounts` node would make accounts queryable like `/sys/paths` (consistent with the
   CONNECT desugar to `/sys/paths` effects) and give REMOVE a natural target.
3. **Rotate/revoke in-language?** Rotate needs a new secret VALUE (out-of-band by rule);
   revoke is metadata and could be a statement. Decide one-concept-one-word naming.
4. Grammar: contextual idents like CONNECT (no new frozen keywords — the additive-by-
   contextual-ident contract; see `connect_and_disconnect_add_no_frozen_keyword`).

## Key files

- `packages/qfs/crates/parser/src/grammar.rs` (`connect_stmt` as the pattern),
  `crates/qfs/src/sys.rs` (the `/sys/paths` apply — the model for a `/sys/accounts` surface)
- `packages/qfs/crates/qfs/src/account.rs` (the bookkeeping the statement must share, not fork)
- `docs/guide/connect.md`, `docs/guide/account-model.md` (document the statement forms)

## Quality Gate

- The new statement(s) parse, preview, and commit through the standard gates; the cookbook parse
  ratchet covers any recipe added to docs.
- No secret VALUE can ride in a statement (parser/test-enforced: the SECRET clause accepts only
  `env:`/`vault:` references, as today).
- The CLI verbs and the statements write the SAME state (one source of truth, like
  connect/`/sys/paths`); `cargo test --workspace` / clippy / fmt / gen-docs / gen-skills green.
