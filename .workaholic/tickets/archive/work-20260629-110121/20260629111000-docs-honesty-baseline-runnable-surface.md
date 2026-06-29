---
created_at: 2026-06-29T11:10:00+09:00
author: a@qmu.jp
type: bugfix
layer: [UX]
effort: 1h
commit_hash: 1c30270
category: Changed
depends_on: []
---

# Docs honesty baseline: establish the actually-runnable surface (and flag the binary bugs the docs trip over)

> **DECISION RECORDED (2026-06-29): seam #1 resolved to (b) WIRE THE BINARY FIRST**, not docs-only.
> The plan lives in EPIC `20260629135900-epic-wire-binary-so-docs-run-true`. The three binary-bug
> "code tickets" this ticket asked to *file* are now real: codec no-op → **T2** (`140010`), WARN noise
> → **T8** (`140110`), describe verb-map → **T9** (`140120`). The per-page doc tickets become **Phase 5**
> — re-pointed to depend on the wiring ticket that makes each page true, with framing flipped from
> "remove/seam-mark" to "verify it now runs; mark only still-unwired parts as connect-account/coming-soon."
> The GROUND TRUTH table below is still the source of record for "what runs today" and grows green per phase.

## Overview

A page-by-page audit ran **every doc example against the real `qfs 0.0.10` binary as a fresh user**
(`env -i HOME=tmp XDG_CONFIG_HOME=tmp … qfs run "…"`, preview only). It found the docs systematically
present capabilities as working that **error for a fresh user**. This is the foundation ticket that
records the verified ground truth + the shared root causes, so the per-page tickets
(`…-fix-readme`, `…-fix-getting-started`, `…-fix-concepts`, `…-fix-shell`, `…-fix-cli`,
`…-fix-index`, `…-fix-cookbook`, `…-fix-query-cookbook`, `…-fix-skill-md`) all apply ONE consistent
honesty model and one decision instead of each re-deriving it.

**THE GROUND TRUTH (verified empirically), which every doc must respect:**
- **Reads that work offline, no creds:** only `/local/<path>` (directory/stat listings) and `/sys/*`
  (`describe /sys/users`, `/sys/audit |> limit 5`, etc.). These genuinely run.
- **Reads that ERROR for a fresh user** (`unknown_source` / `no read driver registered`): `/mail`,
  `/drive`, `/github`, `/slack`, `/s3`, `/r2` (driver present but read facet not registered without a
  connection) and `/sql`, `/git` (no driver mounted at all — they don't even `describe`). So every
  `/<cloud>/… |> …` read and every `… |> call <proc>` (the CALL pipeline starts with a read) fails.
- **Write-PLANS preview fine even on unwired drivers** (`insert/update/upsert/remove into /path …`)
  because the planner never reads — EXCEPT `/s3` upsert/remove → `unsupported_verb` and
  `/sql … returning` → `unrouted_path` (inconsistent). So the safe "preview works" examples are
  bare write-plans, not reads or `|> call`.

## Exact seams — the shared root causes (decide here, apply per page)

1. **THE PRODUCT DECISION TO MAKE (do not guess; flag for the owner).** The docs describe a query
   language whose headline value (read your mail / join a DB to GitHub / convert a file) **does not
   run today** because cloud reads + codecs are unwired. Two honest paths:
   (a) **Docs-only honesty now:** rewrite every page to demonstrate only what runs (`/local` + `/sys`
   reads, write-plan previews, `describe`), and explicitly mark cloud reads / `|> call` / codecs as
   **"needs the driver wired — not yet runnable"** seams. (b) **Wire the binary first** (the larger
   effort: register the cloud read facets, implement `/local` content-read + codecs, mount `/sql`/
   `/git` reads) so the docs can stay aspirational-but-true. The per-page tickets assume **(a)** as
   the immediate fix (honesty-first, the repo's standing rule) and CROSS-REFERENCE the binary work as
   separate code tickets — but the owner should confirm whether to instead prioritize the wiring.

2. **BINARY BUG — confusing WARN noise on every `run` (flag as a separate code ticket).** Every
   `qfs run` (even a pure `create trigger`, a `/local` ls, or a mail-only insert) prints two stderr
   lines: `WARN qfs::consent: cloud driver 'github'/'slack' … requires sign-in … (cloud_sign_in_required)`.
   It makes clean previews look broken and reads like a credential failure to a first-time user. The
   docs cannot honestly hide it; the real fix is to **not warn about unrelated drivers** (only warn
   for a driver the statement actually targets) — `crates/qfs/src/commit.rs` `live_registry()` /
   `crates/qfs/src/consent.rs`. Flag it; the doc tickets will note the noise until it's fixed.

3. **BINARY BUG — `/local` `decode`/`encode` codecs are silent no-ops (flag as a separate code
   ticket).** A `/local/<file> |> decode json |> encode yaml` returns the file's **stat row**
   (`name,path,size,modified,is_dir,mode`), byte-identical with or without the codec stages — never
   the file's bytes, never converted output. The `/local` blob archetype only emits dir/stat
   listings; there is no content read. This makes EVERY "convert a file's format" recipe
   (index.md, concepts.md, files.md, shell.md) **silently wrong** (no error, plausible-looking wrong
   output — the worst kind). Until `/local` content-read + codec application is implemented, the docs
   must NOT claim file-content read or format conversion. `crates/driver-local/` + the codec stage.

4. **BINARY BUG — `describe` verb-map is unreliable for append logs (flag).** `describe /mail/inbox`
   reports `verbs.update:true, verbs.insert:false` — the OPPOSITE of reality (an append log takes
   INSERT, not UPDATE; the working draft INSERT proves it). The docs lean on "describe always shows
   the exact supported set" (mail.md), which is false here. `crates/driver-gmail/` describe schema.

## Implementation steps

1. **Make the decision in seam #1** (docs-only-honesty-now vs wire-the-binary) — record it; the
   per-page tickets reference it. Default: honesty-now.
2. **File the three binary-bug code tickets** (seams #2, #3, #4) so the doc tickets can cross-link
   them instead of papering over them. (This ticket does NOT fix the binary — it scopes + flags.)
3. **Publish the runnable-surface table** (the GROUND TRUTH above) into `docs/guide/concepts.md` (or a
   short "What runs today" note) as the single source the other pages link to, so "what actually
   works" lives in one place.

## Key files

- `docs/guide/concepts.md` (host the "what runs today" truth), and the per-page tickets that depend
  on this one.
- Reference (binary bugs to file): `crates/qfs/src/{commit.rs,consent.rs}` (WARN noise),
  `crates/driver-local/` + codec stage (codec no-op), `crates/driver-gmail/` describe (verb-map).

## Considerations

- **Honesty cuts both ways (the standing rule).** The v0.0.9 release genuinely shipped the engine +
  identity + OAuth AS + MCP + dashboard + `/sys` + teams + the M6 language — document those truly.
  But a query that errors `unknown_source` is NOT a shipped capability; do not present it as one.
- **The audit found two CLEAN pages** — `docs/security/threat-model.md` (every claimed control verified
  in source; seams plainly disclaimed) and `docs/guide/connections.md` (verbs + fail-closed behaviour
  match the binary). They need no fix; the per-page tickets skip them.
- **Anti-drift unchanged:** `docs/{language,drivers,server}.md` are generated — never hand-edited.
- **Versioning:** the doc fixes ship as one (or a few) PRs; bump the patch per CLAUDE.md.
