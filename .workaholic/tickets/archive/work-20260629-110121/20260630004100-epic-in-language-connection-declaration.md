---
created_at: 2026-06-30T00:41:00+09:00
author: a@qmu.jp
type: enhancement
layer: [UX, Domain, Config]
effort:
commit_hash: d8060c6
category: Changed
depends_on: []
---

# EPIC: In-language connection declaration (replace the env-var alias convention)

## The problem (owner's critique, 2026-06-30)

Connections are **injected from outside the language by a naming convention** today: a `/sql/orders`
read works only because the binary scans `QFS_SQL_ORDERS=<path>` and lower-cases the suffix into the
`<conn>` path segment (same for `QFS_GIT_<repo>`). The path **alias is implicitly loaded** — there is
no declaration you can read, review, or version; the source of truth is an env var's *name*. The
credentialed services use a *different* mechanism again (`qfs connection add` → an encrypted store +
an "active connection" selector). Two incoherent models, neither declared in the language.

## The design (principles to honor)

1. **Declaration semantics live IN the qfs language.** A connection is an explicit statement
   (`CREATE CONNECTION …`), reviewable and versionable — never inferred from an env-var name.
2. **Secrets are referenced, never inlined** — `SECRET env:<VAR>` or `SECRET vault:<path>`. The
   declaration names *where the secret comes from*; the value lives in the env or the encrypted
   store (the vault). A local SQLite/git connection needs no secret.
3. **The alias is explicit.** The connection name and driver determine the path mount; nothing is
   loaded by matching an env-var prefix.

## Proposed grammar (the design ticket finalizes it)

A new `CREATE` statement, consistent with the existing `CREATE TRIGGER` / `CREATE POLICY`:

```
CREATE CONNECTION orders
  DRIVER sqlite
  AT '/data/orders.db'                       -- → /sql/orders/<table>

CREATE CONNECTION analytics
  DRIVER postgres
  AT 'postgres://db.internal/analytics'
  SECRET env:PG_PASSWORD                      -- value from $PG_PASSWORD, never inlined

CREATE CONNECTION app
  DRIVER git
  AT '/srv/repos/app.git'                     -- → /git/app/commits, /git/app@<ref>/…

CREATE CONNECTION work
  DRIVER gmail
  SECRET vault:gmail/work                     -- OAuth/token from the encrypted store → /mail/work/…
```

- `DRIVER` picks the family (`sqlite`/`postgres`/`mysql` → `/sql`, `git` → `/git`, `gmail` → `/mail`,
  `gdrive` → `/drive`, `github`/`slack`/`s3`/`r2` likewise). The connection **name** is the
  `<conn>` segment.
- `AT '<locator>'` is the non-secret location (path / URI / bucket / base URL); optional where the
  driver's locator is implicit.
- `SECRET <ref>` is `env:<VAR>` or `vault:<path>` — resolved at use time, never logged, never in a
  `describe`.
- Declarations live in the **`.qfs` config** (loaded by `qfs serve` / `qfs job`, and by `qfs run`
  via a config flag + a default config path). They are explicit config, not runtime env scanning.

## Phase plan (this week)

1. `20260630004110-design-connection-declaration-grammar` — **keystone**: finalize the grammar,
   the driver→path-family map, the `<conn>`-in-path vs `/mail` active-connection reconciliation, the
   secret-ref scheme, where declarations live, and the env-var migration. Everything depends on it.
2. `20260630004120-parser-create-connection-statement` — lang keywords + parser AST + parse tests.
3. `20260630004130-secret-reference-resolution` — `env:` / `vault:` resolvers (vault = the existing
   encrypted credential store); secret-free errors; never logged.
4. `20260630004140-connection-registry-from-declarations` — build the mount + read/apply registries
   from declared connections, replacing the `QFS_SQL_*` / `QFS_GIT_*` scan in `crate::sql/git/google/
   objstore`.
5. `20260630004150-config-and-runtime-loading` — `.qfs` config carries the declarations; `serve` /
   `job` / one-shot load them; default config path; describe/preview stay cred-free.
6. `20260630004160-deprecate-env-var-alias-convention` — `QFS_SQL_*` / `QFS_GIT_*` become a
   deprecated compatibility shim (one warning + a `qfs connection import-env` helper that prints the
   equivalent `CREATE CONNECTION`s); schedule removal.
7. `20260630004170-connection-cli-alignment` — `qfs connection add` stores secret VALUES that a
   `vault:` ref resolves; `connection list` shows declared connections; reconcile `use`/`remove`.
8. `20260630004180-docs-connection-declaration` — rewrite `connections.md` + `concepts.md` +
   cookbook setup tips around the declaration model.

## Considerations

- **Versioning:** ships as a few PRs; bump the patch per PR (CLAUDE.md). The *grammar* is part of
  the versioned surface (README SemVer policy) — adding `CREATE CONNECTION` is an additive grammar
  change; finalize it deliberately in the design ticket.
- **Anti-drift:** the grammar catalogue `docs/{language}.md` is generated — regenerate after the
  parser change; the `roadmap_cookbook.rs` ratchet should gain a `CREATE CONNECTION` recipe.
- **Honesty rule still applies:** until the registry-from-declarations lands, docs keep the env-var
  method (it's what runs); flip the docs in step 8 once declarations are the real path.
- **Related:** this supersedes the env-var half of the wire-binary epic's connection story
  (`20260629135900`); the secret store (`qfs connection add`) is kept as the *vault* backing
  `SECRET vault:…`, not removed.
