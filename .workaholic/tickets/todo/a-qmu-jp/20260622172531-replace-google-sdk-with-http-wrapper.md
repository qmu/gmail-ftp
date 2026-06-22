---
created_at: 2026-06-22T17:25:30+09:00
author: a@qmu.jp
type: refactoring
layer: [Infrastructure, Domain]
effort:
commit_hash:
category:
depends_on: [20260622123701-unify-gmail-gdrive-ftp.md]
---

# Replace the Google API Go SDK with a thin in-house HTTP wrapper

## Overview

The binary is **~21 MB**, and almost all of that weight is `google.golang.org/api` v0.284 dragging in gRPC, protobuf, OpenTelemetry (×5 modules), s2a-go, gax-go, enterprise-certificate-proxy, genproto, and `cloud.google.com/go/auth` — none of which a CLI that makes a few dozen REST calls needs. Gmail and Drive both expose plain **HTTPS+JSON REST APIs**. This ticket **removes `google.golang.org/api` entirely** and replaces the two backend clients with a small hand-rolled HTTP wrapper hitting the REST endpoints directly, behind the **same `gmailClient`/`driveClient` interfaces** (so the shell and commands are untouched). Goal: drastically smaller binary and dependency tree with identical behavior. Scoped against the merged `gftp` module (see `depends_on`) so the swap is done once, not twice.

**Keep `golang.org/x/oauth2`** (token exchange + automatic refresh + `google.ConfigFromJSON`) — it is light and self-contained; the heavy tree is `google.golang.org/api`. Hand-rolling OAuth token refresh is a possible further step but is NOT required here and is explicitly out of scope (note as a stretch). The wrapper uses the `*http.Client` that `oauth2` already returns (auto-attaches and refreshes the bearer token).

## Exact API surface to reimplement (bounded)

**Gmail** (`https://gmail.googleapis.com/gmail/v1/users/me/...`, uploads via `/upload/...`):
- `labels.list`, `labels.create`, `labels.delete`
- `messages.list` (q, labelIds, pageToken, maxResults), `messages.get` (format=metadata|full|raw), `messages.trash`, `messages.modify` (label add/remove — deferred verbs)
- `messages.attachments.get`
- `threads.get`, `threads.trash`
- `drafts.create`, `drafts.get` (format=raw), `drafts.update`, `drafts.send`
- `getProfile` (→ emailAddress, for multi-account identity)

**Drive** (`https://www.googleapis.com/drive/v3/...`, media via `/upload/drive/v3/files`):
- `files.list` (q, fields, pageToken, includeItemsFromAllDrives/supportsAllDrives/corpora for Shared Drives)
- `files.get` (metadata; and `alt=media` for binary download)
- `files.export` (Google Docs → Office formats)
- `files.create` (metadata + media upload), `files.update` (rename/trash via `trashed=true`, and media)
- `drives.list` (Shared Drives)

## Key Files

- `internal/gmail/client.go` — replace `gmail.NewService`/`srv.Users.*` calls with HTTP requests (lines ~43-296). Currently leaks SDK types (`gmail.Message`, `gmail.MessagePart`, `gmail.Label`, `gmail.Draft`, `gmail.Thread`, `gmail.ModifyMessageRequest`).
- `internal/gmail/model.go` — **MIME walking** currently traverses `gmail.MessagePart` / `gmail.Message` (lines ~101-281). These must become **owned DTOs** decoded from the REST JSON (this is the biggest single refactor and also satisfies the "owned DTOs, never marshal the vendor struct" policy).
- `internal/gdrive/client.go` — replace `drive.NewService`/`srv.Files.*`/`srv.Drives.List`/`Files.Export`/media download with HTTP (lines ~49-336). `drive.File`/`drive.FileList` become owned DTOs.
- `internal/shell/commands.go` (drive side) — `drive.File` references (and helpers like IsFolder/IsGoogleDoc/ExportFormat/RootID) must point at the owned types.
- `internal/auth/auth.go` — drop the `gmail`/`drive` SDK imports used only for scope constants; define our own scope-URL string consts (`https://www.googleapis.com/auth/gmail.modify`, `.../gmail.compose`, `.../drive`). Keep `golang.org/x/oauth2` + `google.ConfigFromJSON` + the `http://localhost` redirect.
- New `internal/googleapi/` (or `internal/httpx/`) — the shared wrapper core: a `do(ctx, method, url, query, body) (*http.Response, error)` helper, JSON encode/decode, the Google **error-envelope** parser, pagination helper, and media upload/download helpers. Both backends use it.
- `go.mod`/`go.sum` — remove `google.golang.org/api` and the now-orphaned indirect deps (`cloud.google.com/go/auth*`, grpc, protobuf, otel*, s2a-go, gax-go, genproto, enterprise-certificate-proxy, httpsnoop, go-logr, xxhash, uuid). `go mod tidy` should collapse the tree to roughly `golang.org/x/{oauth2,term,net,sys,text,crypto}`.

## Implementation Steps

1. **Shared HTTP core** (`internal/googleapi`): authed request helper over the oauth2 `*http.Client`; JSON marshal/unmarshal; **error-envelope decoding** (Google returns `{"error":{"code","message","errors":[{"reason":...}]}}`) mapped to our errors — including the `accessNotConfigured`/`SERVICE_DISABLED` reason that `friendlyErr` keys on to print the "enable the API" hint (must preserve that behavior). Pagination helper for `nextPageToken`. base64url encode/decode for message `raw`/attachment data.
2. **Owned DTOs**: define minimal structs for exactly the fields used — Gmail `Message`/`MessagePart`/`MessagePartBody`/`Header`/`Label`/`Draft`/`Thread`; Drive `File`/`FileList`/`Drive`/`DriveList`. JSON tags matching the REST payloads. Port the MIME-walking in `model.go` onto these.
3. **Gmail client**: reimplement each method as an HTTP call (list/get/trash/modify/attachments/threads/drafts.create-get-update-send/getProfile). Drafts carry `message.raw` (base64url) in the JSON body — no separate upload needed for normal sizes; use `uploadType=media` only if needed for large raw.
4. **Drive client**: reimplement list/get/export/create/update/drives.list; binary download via `alt=media` (stream to the existing `io.Writer`); media upload via `/upload/drive/v3/files?uploadType=multipart` (metadata + bytes). Keep Shared-Drive params.
5. **Auth**: own scope consts; drop SDK imports; keep oauth2 + ConfigFromJSON + localhost redirect.
6. **Drop the dependency**: remove `google.golang.org/api` from go.mod; `go mod tidy`; verify the indirect tree collapses.
7. **Measure footprint** (acceptance metric): record `go build` binary size before/after in the ticket's Final Report; expect a large reduction (target: well under half of ~21 MB).
8. **Quality gate**: `go build/vet/gofmt/test` clean. Behavior identical — same interface methods, same `-json` output, same audit, same error messages (incl. API-disabled hint).

## Considerations

- **Owned DTOs are the policy-correct outcome (implementation/domain-layer-separation):** the SDK types currently leak into `model.go` and `commands.go`; replacing them with our own structs both shrinks the binary and stops vendor types crossing the backend boundary. This is the bulk of the work, especially the MIME-part walker.
- **Don't regress error UX:** `friendlyErr`/`activationURL`/`projectNumber` depend on detecting the "API not enabled" 403 and the per-service reason. The HTTP error parser MUST reproduce that detection from the JSON error envelope, or the (already-shipped) "enable the Gmail/Drive API" guidance breaks.
- **Testability improves:** an HTTP wrapper is trivially testable with `net/http/httptest` (spin a fake server returning canned JSON) — no live creds, arguably better coverage than the SDK fakes. Keep the table-driven bar; test pagination, error-envelope mapping, base64url, media up/down, and the MIME walker on owned DTOs.
- **Media upload/download correctness** is the main risk: Drive `alt=media` download, Drive multipart upload, and large Gmail `raw` are the fiddly bits — test these explicitly (including a Google-Doc `export`).
- **Keep `golang.org/x/oauth2`:** token refresh and `ConfigFromJSON` are subtle; reuse them. Hand-rolling the token endpoint is a possible later micro-optimization, out of scope here.
- **Sequencing:** depends on the unify ticket so the wrapper is written once in the merged module; can be implemented backend-by-backend, but `google.golang.org/api` is only removed (and the footprint measured) once BOTH backends are off the SDK.
- **No behavior change:** this is a pure internal refactor — same commands, flags, `-json` contract, audit, and safety properties. Any externally observable change is a regression.
