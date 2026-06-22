---
created_at: 2026-06-22T12:37:01+09:00
author: a@qmu.jp
type: enhancement
layer: [UX, Domain, Infrastructure, Config]
effort:
commit_hash:
category:
depends_on:
---

# Unify gmail-ftp + gdrive-ftp into one FTP-style CLI (`gftp`)

## Overview

`gmail-ftp` and `gdrive-ftp` are structural twins — `internal/auth`, `internal/shell` (REPL, tokenizer, `id:` addressing, Tab completion, `friendlyErr`), `internal/audit`, and `output.go` are near-identical copies; only the backend client package (`internal/gmail` vs `internal/gdrive`) and the command-body domain logic differ. This ticket **merges them into one Go module, `gftp`**, that presents a single FTP-style shell over **both** Google Drive and Gmail, exposed as two mount points under a virtual root: `/drive/...` (N-level folders + Shared Drives) and `/mail/...` (2-level label→message). The merged binary **replaces** both `gmail-ftp` and `gdrive-ftp` (the two are deprecated). This is the structural foundation for the cross-backend "move a Drive file into a Gmail draft attachment" feature (separate ticket, depends on this).

**Locked decisions (from /ticket):**
- **OAuth scope = union, full Drive:** `drive` (read+write) + `gmail.modify` + `gmail.compose`, requested in ONE consent, one token. (User chose full Drive so the unified shell keeps Drive `put/rm/mkdir`.)
- **Replaces both binaries:** one tool going forward; `gmail-ftp` and `gdrive-ftp` deprecated. Users re-auth once into the new config dir `~/.config/gftp/`.
- Canonical home is **this repo** (module path becomes `gftp`); the `../gdrive-ftp` code is ported in and that repo is archived.

## Key Files

- `main.go` — becomes one entry that builds ONE authorized `*http.Client`, constructs BOTH `gmail.Client` and `gdrive.Client`, and hands both to a unified shell. `configDir()` → `~/.config/gftp/` (creds/token/audit beside each other). Port from both `main.go` twins.
- `internal/auth/auth.go` — already byte-identical across repos except the scope set. Make `Scopes` the **union** (`drive.DriveScope`, `gmail.GmailModifyScope`, `gmail.GmailComposeScope`); keep the `http://localhost` loopback redirect + comment (do NOT regress to `127.0.0.1`). One `auth.Client` for both APIs.
- `internal/shell/shell.go` — generalize the existing consumer-side `gmailClient` interface into a small shared **backend** seam; extract a matching `driveClient` interface from `*gdrive.Client` (gdrive currently holds a concrete client). Model cwd as a **tagged structure `{backend, []Ref}`** so each backend's existing `resolveDir`/`resolveFile` is reused once the root segment selects the backend. Root dispatch: first path component `drive`|`mail` selects the backend; `ls /` at the virtual root lists the two mounts.
- `internal/shell/commands.go` — merge the two command tables. **Universal** verbs (`ls/cd/pwd/get/put/rm/mkdir/find`, `lcd/lls/lpwd`, `help/exit`) dispatch to the active root's backend. **Backend-conditional** verbs available only under their root: Gmail-only (`search`, `compose`, `send`, deferred `label`/`unlabel`); Drive-only behaviors (Google-Doc export on `get`, Shared-Drive listing). Reconcile the `argKind`/Tab-completion tables.
- `internal/gmail/` and `internal/gdrive/` — kept **as-is** (no behavioral rewrite of the Gmail v1 / Drive v3 clients or the MIME builder). Port `internal/gdrive` in from `../gdrive-ftp` verbatim.
- `internal/audit/audit.go` — **union** the two records: Op constants (`OpDraft/OpSend/OpTrash/OpLabel/OpMkLabel` + `OpUpload/OpMkdir` + Drive's `Replaced/PriorSize`) and Entry fields (`ThreadID/LabelIDs` + `ParentID/DriveID/...`). One `audit.jsonl`; the reader/TUI browser (`reader.go`,`browser.go`) must render a mixed op vocabulary.
- `internal/shell/output.go` — reconcile the two (they have DRIFTED); keep both backends' `-json` entry shapes stable so existing agent consumers don't break.
- `go.mod`/`go.sum` — one module `gftp`, Go 1.25.8; merge requires both `drive/v3` and `gmail/v1` (already in `google.golang.org/api`).
- `plugins/`, `.claude-plugin/marketplace.json`, `README.md`, `plugins/gftp/skills/gftp/SKILL.md` — one plugin + one agent skill describing the unified `/drive` + `/mail` surface; deprecate the two old skills.

## Related History

- `gmail-ftp` trip (`.workaholic/trips/gmail-ftp/`): Amendment 1 (2-level nav, reversible defaults, least-privilege scope), Amendment 2 (deferred stubs); v1.1 ticket shipped `compose`/`put`-attach/`send` + the pure MIME builder. Story: `.workaholic/stories/work-20260620-200140.md`.
- Both repos use the SAME auth scaffold + the `http://localhost` redirect fix (gmail `72e8a4f`, gdrive `e66be72`).
- gdrive-ftp parity was the trip's governing goal — "same concept, same structure, same experience"; the merge extends parity to a two-backend tool.

## Implementation Steps

1. **Module + entry:** rename module to `gftp`; one `main.go` builds one `auth.Client` (union scopes) → both backend clients → unified shell. New `configDir()` = `~/.config/gftp/`. Keep `auth`/`log`/`completion`/`__complete` subcommands.
2. **Dedup cross-cutting packages:** unify `internal/auth`, `internal/audit`, `internal/shell` (REPL/tokenizer/completion/`friendlyErr`), `output.go` into ONE shared copy each — do not co-locate two `internal/` trees (per `implementation/directory-structure`).
3. **Backend seam:** generalize `gmailClient` → a shared backend interface for the universal FTP verbs; extract `driveClient` from `*gdrive.Client`. Keep SDK types behind each vendor boundary (owned DTOs only).
4. **Unified namespace + cwd:** virtual root with `/drive` and `/mail` mounts; cwd = tagged `{backend, []Ref}`; root segment selects backend; per-backend `resolveDir`/`resolveFile` reused unchanged. `pwd` prints the rooted path.
5. **Command dispatch:** universal verbs route by active root; backend-only verbs scoped to their root with a clear error if used in the wrong root. Merge `argKind`/completion.
6. **Audit union:** one Entry/Op set; one `audit.jsonl`; reader/TUI render both vocabularies.
7. **Auth union + scope:** request `drive` + `gmail.modify` + `gmail.compose` in one `ConfigFromJSON`; one documented scope source-of-truth comment; both Drive API and Gmail API must be enabled on the OAuth client's GCP project (the `friendlyErr` per-service detection already handles this).
8. **Deprecate old tools:** README notes `gmail-ftp`/`gdrive-ftp` are superseded by `gftp`; one merged plugin + skill; archive `../gdrive-ftp` (out-of-repo, note in README).
9. **Quality gate:** `go build ./...`, `go vet ./...`, `gofmt -l .`, `go test ./...` all clean. Port both repos' table-driven tests; both backends remain testable via fakes with no live creds; preserve the `rm`=single-message-trash and never-send-from-`put` assertions.

## Considerations

- **Scope blast radius (design/access-control, defense-in-depth, data-sovereignty):** one token now grants full Drive **and** Gmail modify/compose — a materially larger blast radius than either tool alone. The user chose full Drive deliberately; keep the scope set the documented minimum for the offered ops (never widen Gmail to `mail.google.com`/hard-delete), and surface the combined grant clearly in README/SKILL so the consent stays explainable.
- **Two cwd/Ref models must coexist** (design/modeless): no "drive mode" vs "mail mode" the user toggles — the root segment of the path selects the backend; every op stays reachable without entering a mode.
- **`-json` contract stability (operation):** keep gmail's (`name/id/kind/from/subject/threadId`) and gdrive's (`name/id/mimeType/isFolder/modifiedTime`) entry shapes intact under their roots; do not break either documented contract.
- **Carried-over Gmail backlog** (batched metadata fetch + intra-command cache; one-shot `-json` `friendlyErr` normalization; empty-audit cosmetic) follows into the merged tool; a unified `ls` must not assume uniform listing cost across backends.
- **Config/token migration:** simplest is a fresh single consent into `~/.config/gftp/` (re-consent is already forced via `ApprovalForce`); optionally read old `~/.config/{gmail,gdrive}-ftp/` tokens, but a clean re-auth is acceptable.
- **directory-structure / golang-coding-standards:** one `cmd`/thin main, consumer-side small interfaces returning `(T, error)`, `context.Context` first arg, gofmt/vet/lint in `scripts/`.
