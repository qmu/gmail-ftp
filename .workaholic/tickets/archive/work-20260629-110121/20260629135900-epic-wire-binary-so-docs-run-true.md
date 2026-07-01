---
created_at: 2026-06-29T13:59:00+09:00
author: a@qmu.jp
type: enhancement
layer: [UX, Domain, Infrastructure]
effort:
commit_hash: 1c30270
category: Added
depends_on: []
---

# EPIC: Wire the qfs binary so the v0.0.10 docs run true (chosen path: wire-first, not docs-only)

## Decision recorded (foundation seam #1)

The docs-honesty audit (foundation ticket `20260629111000`) surfaced ONE product decision:
**(a) docs-only honesty now** — rewrite every page down to what runs today; vs **(b) wire the binary
first** — make the cloud reads / codecs / `/sql` / `/git` actually work so the aspirational docs become
true. **The owner chose (b): wire the binary.** This epic is the plan for (b). The eleven page tickets
(`111000`–`111140`) are NOT discarded — they become **Phase 5**, re-pointed to depend on the wiring
ticket that makes each page's examples real, and their instruction flips from *"remove / seam-mark the
feature"* to *"verify the example now runs; mark only the still-unwired parts as coming-soon."*

## Why this is tractable (verified against the source, 2026-06-29)

The drivers and codecs **already exist as code** — the gap is read-facet *registration* and one dropped
plan stage, not missing implementations:
- `crates/codec/` ships all six codecs (`json,yaml,toml,csv,jsonl,markdown+frontmatter`), pure
  `bytes↔rows` — but codec pipe stages are **silently dropped at lowering**
  (`crates/pushdown/src/lower.rs:256-265` returns `Ok(input)`; `LogicalPlan` has no `Codec` variant).
- `/local` has `fs_core::read_blob` (`crates/driver-local/src/fs_core.rs:376`) and `Value::Bytes`
  (`crates/types/src/value.rs:84`) — but single-file reads emit a **stat row**, never bytes.
- `driver-git` already has a hermetic local-object read stack (`blobfs.rs`, `relational.rs`,
  `objectdb.rs`, `repo.rs`); `driver-sql` has `conn.rs`/`applier.rs` — **neither registers a read
  facet** (read facets are wired only for `github`, `slack`, `sys`, `claude` in
  `crates/qfs/src/shell.rs:241-273`).
- Cloud drivers register a planning mock + write facet but **no read facet** unless signed in, so reads
  error `unknown_source` (`crates/qfs/src/exec.rs:63-65`); the startup WARN noise comes from
  `cloud_bind_allowed()` warning for EVERY cloud driver regardless of target
  (`crates/qfs/src/commit.rs:517-536`).

## Phase plan (each phase ships as its own PR; bump the patch per PR)

**Phase 1 — Hermetic local foundation (the headline feature).**
- `20260629140000-wire-local-single-file-content-read` (T1) — `/local/<file>` emits a `content` (Bytes) column.
- `20260629140010-wire-codec-execution-decode-encode` (T2, dep T1) — make `decode`/`encode` execute;
  `… |> decode json |> encode yaml` actually emits YAML. **Subsumes the "codec no-op" binary bug.**

**Phase 2 — Hermetic structured reads.**
- `20260629140020-wire-git-read-facet-local-repo` (T3) — `/git/<repo>@<ref>/<path>` reads from a local repo.
- `20260629140030-wire-sql-read-facet-sqlite` (T4) — `/sql/<conn>/<table>` reads from SQLite (Postgres via live connection).

**Phase 3 — Cloud reads (network/OAuth — the only non-hermetic pieces).**
- `20260629140040-wire-cloud-read-facets-connect-account-error` (T5) — mail/drive/ga/s3/r2 reads return a
  clear `capability: connect your account` error instead of `unknown_source`.
- `20260629140050-wire-github-slack-reads-end-to-end` (T6) — github/slack `read_rows` return rows when a token is present.
- `20260629140100-wire-gmail-gdrive-ga-read-rows` (T7) — build the missing `read_rows` paths.

**Phase 4 — Binary correctness bugs (the flagged set).**
- `20260629140110-fix-warn-noise-unrelated-cloud-drivers` (T8) — only warn for drivers a statement targets.
- `20260629140120-fix-describe-verb-map-append-logs` (T9) — fix append-log verb map (insert/update inverted).

**Phase 5 — Doc truth pass (the existing 11 tickets, re-pointed).** Each page is re-verified against the
now-wired binary; the table below maps each doc ticket to the wiring it waits on.

## Doc-ticket → wiring dependency map (Phase 5)

| Doc page ticket | Now depends on | Framing after wiring |
|---|---|---|
| `111030` concepts (hosts "what runs today" table) | T1,T2,T3,T4 | table grows green per phase |
| `111050` index (JSON→YAML hero) | T1,T2 | hero runs for real |
| `111120` cookbook (files/databases/cross-service/code/mail) | T1–T7 | recipes run or "coming soon" |
| `111140` skill-md (AI procedure) | T1,T2,T5 | examples preview clean; cloud = connect-account |
| `111010` getting-started | T1,T2 (cloud parts T5/T7) | local leads, runs |
| `111130` query-cookbook ("now live") | T1–T4 | "live" = parses AND runs for /local,/sys,/git,/sql |
| `111040` readme quickstart | T1,T2 | quickstart copy-paste runs |
| `111100` cli | T9 (+ version) | small accuracy pass |
| `111110` installation (WARN note) | T8 | note drops once T8 lands |
| `111020` shell | — (already a real shell; fix to reality now) | independent |
| (no fix) threat-model, connections | — | verified clean |

## Open follow-ups (documented, not lost — no separate queue ticket yet)

- **`/drive` (gdrive) real read** — a Drive listing needs path→folder-id resolution (Drive is
  id-addressed); T7 wired gmail but left `/drive` on the honest connect-account facet. Scope: a
  `read_rows` that walks the path to a folder id, then `list_files`.
- **`/ga` (Analytics) real read** — needs a query→`runReport` request mapping (dimensions/metrics),
  a distinct model from a list scan; left on connect-account (the T7 ticket marked GA deferrable).
- **Gmail `q=` WHERE pushdown** — T7 pushes only the label scope; `from:`/`subject:`/`is:unread`
  pushdown into the Gmail query is a later optimization (WHERE is a local residual today).
- **`/git@<ref>` temporal reads** — the Phase-5 doc verification found the `@<ref>` coordinate is NOT
  honored for tree/blob reads (`/git/r@v1/` returns HEAD's tree, not v1's), and single-file blob
  reads (`/git/r@<ref>/src/main.rs`) error `invalid_path`. T3 wired commits/refs/tags/reflog +
  HEAD-tree listings (those run); ref-pinned trees + file-bytes-at-a-ref are a follow-up. The docs
  honestly claim only what runs.
- **`/local` write materialization** — a one-shot `upsert into /local/<file> values(…)` reports
  COMMITTED but does not write the file (`commit_failed … carries no content blob`); shell `cp`/
  `upsert` likewise. A driver-level write seam, surfaced during the shell-doc rewrite.
- **`md` codec** — `decode/encode md` errors `unknown_codec`; only `json/jsonl/yaml/toml/csv` are
  wired. The `markdown+frontmatter` codec exists in `crates/codec` but isn't registered in the
  builtin set the read path resolves through.

## Considerations

- **Anti-drift:** `docs/{language,drivers,server}.md` are generated — never hand-edited; regenerate via
  `cargo run -p xtask -- gen-docs` and keep `gen-docs --check` green.
- **Hermetic test rule holds** (`cargo test --workspace`, no network/creds): T1–T4, T8, T9 land with
  hermetic tests; T6/T7 cloud reads are gated behind real credentials (a non-hermetic, opt-in integration
  lane), never in the default suite.
- **Sequencing:** Phases 1, 2, 4 are independent and can run in parallel PRs; Phase 3 T6/T7 are lowest
  priority (network-bound). The shell doc ticket `111020` needs no wiring and can ship immediately.
- **Versioning:** bump the patch in `packages/qfs/crates/qfs/Cargo.toml` and cut a `v0.0.x` tag per PR.
