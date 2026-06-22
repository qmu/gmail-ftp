---
created_at: 2026-06-22T12:37:01+09:00
author: a@qmu.jp
type: enhancement
layer: [UX, Domain, Infrastructure]
effort:
commit_hash:
category:
depends_on: [20260622123701-unify-gmail-gdrive-ftp.md]
---

# Cross-backend `cp`: move a Drive file into a Gmail draft attachment

## Overview

The headline reason for merging the two tools (see `depends_on`): a single FTP-style command that takes a **Google Drive file** and attaches it to a **Gmail draft**, without the user downloading and re-uploading by hand. Surface:

```
cp /drive/<path-or-id:>  /mail/id:draft:<id>
```

`cp` reads its source from whichever backend the first path's root selects and writes to whichever the second selects — so it generalizes, but **v1 of this feature only needs Drive → draft-attachment**. The composition is short and reuses functions that ALREADY exist; the pure MIME seam (`gmailpkg.AppendAttachment` / `MIMEAttachment`) is backend-agnostic, so essentially no new MIME or Drive client code is required — only the glue verb. The result lands as a **draft mutation only — never sends** (send stays the explicit, separate, audited verb).

## Key Files

- `internal/shell/commands.go` — add `cmdCp`. Mirror `putAttach`'s body (the v1.1 attach path is the exact template): resolve target draft via `parseDraftIDArg`, then `GetDraftRaw` → `AppendAttachment` → `UpdateDraft` (single update, so a mid-flight failure can't corrupt the draft). Source bytes come from Drive instead of `os.Open`.
- `internal/gdrive/client.go` — `Download(ctx, fileID, w io.Writer)` and `Export(ctx, fileID, mime, w)` both already take an `io.Writer`; stream into a buffer/`io.Pipe` — **no client change needed**. `GetByID`/`FindOne` resolve the source; `IsGoogleDoc`/`ExportFormat` decide export-vs-raw and the exported filename/extension.
- `internal/gmail/model.go` — `AppendAttachment(raw []byte, att MIMEAttachment) []byte` and `MIMEAttachment{Filename, ContentType, Content []byte}` are the pure handoff seam: Drive bytes → `Content`, Drive (or exported) filename → `Filename`. Reuse as-is.
- `internal/gmail/client.go` — `GetDraftRaw`/`UpdateDraft` are the target-draft methods.
- `internal/shell/shell.go` — `cmdCp` needs BOTH live backend clients (the first command that spans both); wire through the unified shell. Add `cp` to `argKind`/Tab-completion: arg1 = a `/drive` remote path, arg2 = a draft id (no path completion, like `send`/`put`'s draft arg).
- `internal/audit/audit.go` — audit the transfer recording **source Drive ID + target draft ID** (reuse `OpDraft` or add `OpAttach`).
- `README.md`, `plugins/gftp/skills/gftp/SKILL.md` — document `cp /drive/... /mail/id:draft:<id>`; note Google-Docs export behavior and that it never sends.

## Related History

- `gmail-ftp` v1.1 ticket (`20260621191543`) shipped `put <file> <draft>` (attach), `compose`, `send`, and the pure multipart-MIME builder — the direct, reusable precursor. This ticket sources the attachment bytes from Drive instead of local disk.
- Trip safety bar (Amendment 1): `put`/`compose` never send; `send` is the sole irreversible verb. `cp` must terminate at a draft, audited, never send.

## Implementation Steps

1. **Resolve Drive source** exactly as gdrive `cmdGet`: `resolveFile(path)` (or `GetByID` for `id:`) → `*drive.File`. If `IsGoogleDoc(f)`, pick `ExportFormat(f.MimeType)` for `(mime, ext)` and the attachment filename becomes `f.Name+ext`; else filename = `f.Name`, content-type = the Drive `mimeType` (or `contentTypeForName`).
2. **Stream bytes** into a buffer (binary → `Download`; native Google doc → `Export`) — both accept `io.Writer`. For large files use `io.Pipe` to avoid buffering the whole file in RAM, and **guard against Gmail's ~25 MB message-size ceiling** with a clear pre-`UpdateDraft` error.
3. **Resolve target draft** via `parseDraftIDArg(arg)` → `draftID`.
4. **Attach** (verbatim from `putAttach`): `threadID, raw, _ := GetDraftRaw(ctx, draftID)`; `newRaw := AppendAttachment(raw, MIMEAttachment{Filename, ContentType, Content: bytes})`; `UpdateDraft(ctx, draftID, newRaw)`. One update call — no partial-corruption window.
5. **Audit** the transfer (source Drive ID + target draft ID); never send.
6. **Wire dispatch + completion** for `cp`; clear errors when roots don't match the supported direction (Drive→draft) in v1.
7. **Docs** README/SKILL.
8. **Quality gate:** `go build/vet/gofmt/test` clean. Tests (fake Drive + fake Gmail clients, no live creds): cp attaches Drive bytes to a draft and NEVER sends; Google-Doc source is exported (filename gets the export extension); oversize source errors before `UpdateDraft`; a missing draft 404s cleanly; a missing Drive source 404s cleanly.

## Considerations

- **Partial-failure / recovery (operation/operational-planning, observability):** the transfer is two dependent external calls (Drive fetch, Gmail attach). On attach failure, don't leave orphans; report cleanly and make a retry idempotent (re-running resolves to the same draft; avoid duplicate-attachment build-up — consider detecting an already-present identical part, or document that a retry appends again). Wrap each leg with a finite timeout + bounded retries; record both legs in the audit log so a half-done transfer is reconstructable.
- **Google Docs have no raw bytes** — they MUST be exported (`.docx/.xlsx/.pptx/...`) before attaching; the attachment filename/content-type come from `ExportFormat`, not the raw `mimeType`. Missing this attaches an empty/garbage file.
- **Size ceiling:** Gmail caps total message size (~25 MB via API). Fail fast with an actionable error before `UpdateDraft`; a future option could insert a Drive share link in the body instead of bytes (out of scope for v1).
- **Reversible-by-default:** `cp` is a draft mutation; the user still runs `send` explicitly to deliver. Never bundle attach+send.
- **Direction scope:** v1 supports Drive → draft only. Keep the `cp <src> <draft>` shape general so Drive→local / local→Drive / Gmail-attachment→Drive could be added later, but error clearly on unsupported directions now.
- **Testability:** the whole path must be exercisable with fake backend interfaces and the pure MIME builder — no live credentials — matching the inherited table-driven bar.
