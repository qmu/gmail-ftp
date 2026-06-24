# Coding Review — Architect — t21 (Google Drive driver)

- **Reviewer**: Architect (Neutral / structural bridge)
- **Target**: t21 — `qfs-driver-gdrive`, commit `f7c1a51`
- **Scope**: analytical review only (no test execution; QA domain = code/architectural/model review)
- **Files read**: `crates/driver-gdrive/src/{lib,path,schema,query,read,client,effect,applier,error,export}.rs`,
  `crates/driver-gdrive/src/tests.rs`, `crates/driver-gdrive/Cargo.toml`,
  `crates/cmd/tests/dep_direction.rs`, `crates/http-core/src/lib.rs`, `ARCHITECTURE.md`

## Decision

**Approve with minor suggestions.**

The driver is structurally clean, faithfully mirrors the t20 pattern, and the t20 defect class
(lossy-pushdown residual truthfulness) is **correctly handled here from the start**. Token safety
and trash-not-delete are sound. One real correctness issue exists on the parked live-network write
path (HTTP `PUT` where Drive requires `PATCH`); because it sits behind the explicitly-parked live
seam and breaks no test, it is recorded as a must-fix-before-live observation rather than a
revision blocker.

## Headline verdicts

### Pushdown residual TRUTHFULNESS — PASS (no regression of the t20 defect)

The exact-vs-lossy split in `query.rs` is correct and complete. The `Lowered` enum forces every
lowered comparison to be tagged `Exact` (term ≡ predicate, residual dropped) or `PreFilter`
(looser Drive term, **predicate kept as residual**), and `lower()` honours the tag:
`PreFilter(term) => { terms.push(term); Some(p.clone()) }` (lines 99-104). Auditing every arm:

- **Exact (residual correctly dropped):** `name = 'x'` → `name = 'x'`; `mime_type = 'x'` →
  `mimeType = 'x'`; `trashed = b` → `trashed = <b>`; `parent = id` → `'<id>' in parents`; and the
  `parent` scope term added by `build_query`. Each is an exact Drive equality/membership. Correct.
- **Pre-filter (predicate correctly KEPT as residual):**
  - `name ~ 'p'` (Match) and `name LIKE 'p'` → `name contains 'p'` — `contains` is a
    token/substring match, looser than regex/`LIKE`; kept. Correct.
  - `text`/`full_text` `=`/`LIKE` → `fullText contains 'p'` — loose full-text; kept. Correct.
  - `modified_time >/>=/</<= <ms>` → `modifiedTime >/< '<rfc3339>'` with **ms→s truncation**
    (`div_euclid(1000)` in `rfc3339_from_ms`); the bound is second-granular and therefore looser
    than the exact ms comparison, so it is a `PreFilter` and the exact predicate is kept. Correct.
- **Wholly residual:** `OR`/`NOT`/`IN`/`BETWEEN`, unknown columns, dotted column refs
  (`field_of` returns `None` for non-bare paths), and any unmatched `(field, op, lit)` tuple all
  fall through to `Some(p.clone())`. No silent drop. Correct.
- **Conjunction:** `And` recurses and re-AND-joins surviving residuals; an all-exact conjunction
  collapses to `None`. Correct.

No `q` term can cause a wrong row: every lossy term is an over-fetch pre-filter re-checked locally.
The invariant is also pinned by tests `where_lowers_to_drive_q_with_lossy_residual_kept_local`,
`exact_predicates_push_fully_with_no_residual`, `lossy_predicate_returns_residual_so_engine_refilters`,
and the `modifiedTime` second-granular residual case. This is the exact discipline t20 had to be
revised into; t21 ships it correctly on the first pass.

### Token safety — PASS

- No `reqwest` in `crates/driver-gdrive/Cargo.toml` — the driver rides the t19 `GoogleApiClient`
  + the pure `qfs-http-core` DTOs; reqwest stays confined to `qfs-driver-http`. The runtime-leaf /
  http-core single-sourcing invariants in `dep_direction.rs` (the `http_core_is_a_pure_leaf…` and
  `runtime_is_confined…` tests) are untouched and still compose.
- The bearer is never handled in this crate: `GoogleApiDriveClient::send` builds an `HttpRequest`
  with **no** `Authorization` header (the `GoogleApiClient` injects + refreshes it), and
  `From<AuthError> for DriveError` reduces auth failures to a stable `code` + `reauthorize` bool
  only — no token, URL, or header value crosses.
- `DriveError` arms are secret-free by construction (path / verb / `op` label / status / fixed
  reason); `Decode`/`CodecDecode` explicitly never carry body bytes; `Api` carries the op label and
  status, never the query URL. `MockDriveClient::RecordedCall` records owned args (and byte
  *lengths*, never bytes), so even the test surface is secret-free.

### Trash-not-delete — PASS

`decode_remove` defaults to `DriveEffect::Trash`; the permanent `DriveEffect::Delete` is reachable
**only** when the explicit `hard_delete` row column is `Bool(true)` (`bool_col`). Both arms are
`is_irreversible() == true`, so the runtime never auto-retries them and PREVIEW warns. The
`GDriveClient` trait does expose a `delete` method (unlike t20's Gmail, which omitted permanent
delete entirely), but it is correctly gated behind the flag — consistent with the ticket's
"permanent delete only via explicit `hard_delete`" contract. Tested by the trash-default /
hard-delete-flag case and `preview_of_a_trash_plan_performs_no_io`.

### Contract fit — PASS (with one live-path observation, below)

- Export-vs-download read path (`read.rs` / `export.rs`) is correct: native docs export to a
  default office MIME (docx/xlsx/pptx, drawing→pdf, other-native→pdf fallback), binary files
  download raw, and a `!token`/`?export=` override is honoured for native docs and ignored for
  binaries. Deterministic and self-documenting.
- Shared Drives params are threaded uniformly: `supportsAllDrives=true` +
  `includeItemsFromAllDrives=true` on every list, `corpora=drive` + `driveId=<id>` when scoped,
  `supportsAllDrives=true` on get/download/upload/copy/modify/trash/delete.
- `cp`/`mv` decompose consistently with the t16 pattern: `CALL drive.copy` → `Copy` (server-side,
  reversible, not flagged irreversible), `mv` legs as `Move` (metadata re-parent) — the
  copy→verify→delete DAG is the planner's concern, the applier executes the irreducible legs.
- `@rev` / `VersionSupport::Versioned`: the `@<rev>` parse and the `rev` column are present; the
  history walk (`revisions.list`) is an honest named park.

## Concern + proposal (the one substantive observation)

**HTTP method mismatch on the parked live write path: `PUT` where Drive requires `PATCH`.**
`qfs-http-core::HttpMethod` (lines 72-82) defines only `Get/Post/Put/Delete` — there is no `Patch`
variant. The Drive v3 REST `files.update` endpoint (used by `modify_file` for rename/re-parent, by
`trash` for the `{trashed:true}` metadata write, and the media-update used by `update_content`) is
registered as **`PATCH`**, not `PUT`. `client.rs` sends `HttpMethod::Put` to
`/drive/v3/files/{id}` (lines 313, 343, 368) and to the `uploadType=media` update (line 311). On a
live Drive call this would be rejected (typically `405 Method Not Allowed`), so the real
write/rename/trash/update-content legs would not function against the live API as written.

This breaks no test (every test goes through `MockDriveClient`, which records the owned op + args,
not the wire method) and the live-network legs are within the crate's explicitly **parked** scope
(the doc comment parks live path resolution / resumable upload / `@rev` walk). I therefore do not
treat it as a revision-blocking defect — but it is a real, latent live-path correctness bug, not a
cosmetic one, and the `Put` doc comments in those methods currently misdescribe the wire behaviour.

**Proposal (structural, low-cost):** add a `Patch` arm to `qfs-http-core::HttpMethod` (the single
source of truth — one reviewable edit that also benefits any future PATCH-based driver) and switch
the three metadata writes + the media content update to `HttpMethod::Patch`. Until then, record the
method choice as an explicit "parked: live wire method is PUT, must be PATCH before the live smoke
test" line in the crate's Named-parks doc block, so the gap is honest rather than silently wrong.
Either path keeps the spine intact (http-core stays a pure leaf; no new dependency).

## Minor suggestions

1. **`download` ignores the pinned `revision`.** `GoogleApiDriveClient::download` takes
   `_revision` and never appends `&revision=<id>`, so a `/drive/file@<rev>` raw download silently
   reads the head revision. The `ReadPlan::Download` already carries the revision and the mock
   records it, so the seam is right — only the live URL construction drops it. Append
   `revisions/<rev>?alt=media` (or the `revisionId` param) when `revision` is `Some`. Low priority
   (revision read is a named park), but worth a one-line park note so it is not mistaken for done.

2. **`caps_for` admits `Update`/`Remove` on a folder-or-file path.** The `My`/`Shared` arm grants
   the full verb set including `Update`/`Remove` because the pure parse cannot distinguish a folder
   from a file without a live lookup; the comment says the applier/effect decode enforces the
   concrete shape. That is a defensible structural choice (parse-time is shape-blind here), but the
   capability surface is therefore looser than the per-node-archetype ideal. Acceptable for v1;
   consider a doc note that the concrete folder-vs-file refusal is deferred to apply-time decode so
   a reader does not expect parse-time rejection of e.g. `REMOVE` on a folder path.

3. **`escape` vs `encode` layering is correct but worth a one-line cross-reference.** `query.rs`
   escapes the `q` *literal* (`\`, `'`) and `client.rs` percent-`encode`s the whole `q` *parameter*;
   the two compose correctly (literal-escape first, URL-encode second). A pointer comment from one
   to the other would prevent a future editor from double-escaping or dropping a layer.

## Spine / structural assessment

Clean. `qfs-driver-gdrive` is a leaf runtime consumer: it appears in both the generic leaf-check
(`runtime_is_confined_to_plan_and_types` (b)) and the named identity allowlist (b', line 327), and
nothing depends back onto it, so tokio still dead-ends in the leaf. The allowlist append composes
with the generic rule exactly as designed — safety pinned by (b), intent pinned by (b'). No vendor
type crosses the `GDriveClient` boundary (owned DTOs only; Drive JSON decoded at the client). The
purity invariant holds: introspective methods build data with no I/O, and PREVIEW is proven to make
zero client calls. `ARCHITECTURE.md` already carries the gdrive row framing; updating it to note
the PUT→PATCH park would keep the doc honest.

## Honesty of parks

Honest and well-scoped. The crate doc explicitly parks (a) live `name→id` tree resolution (the
pure parse + resolved-id effect columns exist; the walk is exercised via the mocked `list_files`
seam), (b) resumable-upload chunking (modelled as a single seam call), and (c) the `@rev` history
walk (column + parse present, `revisions.list` deferred). The one place the parking could be
*more* honest is the live HTTP method (PUT vs PATCH) and the dropped revision on `download` — both
should be added to the park list per the proposals above so a reader does not read the present code
as a working live write/revision path.
