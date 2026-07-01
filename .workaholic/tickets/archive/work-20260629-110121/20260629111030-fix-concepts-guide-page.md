---
created_at: 2026-06-29T11:10:30+09:00
author: a@qmu.jp
type: bugfix
layer: [UX]
effort: 1h
commit_hash: 1c30270
category: Changed
depends_on: [20260629111000-docs-honesty-baseline-runnable-surface.md, 20260629140000-wire-local-single-file-content-read.md, 20260629140010-wire-codec-execution-decode-encode.md, 20260629140020-wire-git-read-facet-local-repo.md, 20260629140030-wire-sql-read-facet-sqlite.md]
---

# Fix docs/guide/concepts.md — the headline pipe-SQL example and codec one-liner don't run

## Overview

`docs/guide/concepts.md` ("How qfs works") is mostly accurate on architecture but its **headline
worked examples error** against the binary. Severity: **BROKEN** (the page that teaches "what qfs is"
leads with examples that fail). The `/sys` section is solid and verified — keep it.

## Exact seams (verified, fresh user)

1. **The headline pipe-SQL example errors** (§3, lines 71-77): `/sql/pg/orders |> where total > 100
   |> select id, total |> order by total DESC |> limit 5` → `{"error":{"code":"unknown_source",…,
   "message":"unknown source 'sql'"}}`. Even `describe /sql/pg/orders` → `unknown_mount`. `/sql` has
   no read driver AND no describe seam.
2. **The federation flagship fails on its first source** (§5, lines 125-129): `/sql/pg/orders |> join
   /github/acme/web/issues on id == issue_id …` → `unknown source 'sql'`. The marquee "join a DB to
   GitHub" cannot run.
3. **The git coordinate example fails** (§1, lines 25-28): `/git/myrepo@v1.2/src/main.rs |> select
   path` → `unknown source 'git'`; `describe /git/myrepo/commits` → `unknown_mount`. `/git` is absent.
4. **The codec one-liner is a silent no-op** (Codecs section, lines 139-143): `<file> |> decode json
   |> encode yaml` returns the file's **stat row**, not YAML, identical with/without the codec stages
   (foundation seam #3). "Convert a file's format in one line" does not happen.
5. **The §1 path table + §2 archetype table over-promise** — of the 8 source families, reads work only
   for `/local` (dir listings) and `/sys`; `/mail /drive /s3 /github /slack` reads error
   `no read driver registered`, `/sql /git` don't even mount. The §2 "Relational table … Postgres,
   MySQL, D1" row has no working mount.

## Implementation steps

1. **Replace the headline pipe-SQL + federation + git examples** with ones that actually run today
   (`/local` reads, `/sys` reads), OR clearly mark them "illustrative of the grammar; the
   `/sql`/`/git`/cloud read drivers are not yet wired" per the foundation decision — do not present
   them as runnable.
2. **Remove the codec one-liner claim** (or move it under a seam note) until `/local` content-read +
   codecs land (foundation seam #3).
3. **Annotate the §1/§2 tables** with what is read-runnable today (`/local`, `/sys`) vs.
   describe-only vs. unwired — the single "what runs today" note from the foundation ticket can live
   here and the other pages link to it.
4. **Keep the `/sys` section** (verified: all 8 `/sys/*` paths describe correctly, `/sys/audit |>
   order by seq DESC |> limit 20` runs, `/sys/connections` has no secret column).

## Key files

- `docs/guide/concepts.md` (edit; this page can host the foundation "what runs today" table).
- Reference: `crates/driver-sys/src/schema.rs` (`/sys`), `crates/cmd/src/lib.rs`,
  `crates/lang/src/keywords.rs`.

## Considerations

- The architecture prose (one engine / three faces, safety floor, paths-as-everything) is sound and
  verified — the fix is the **examples**, not the concepts.
- Cross-link rather than duplicate: this page is the natural home for the runnable-surface truth.
