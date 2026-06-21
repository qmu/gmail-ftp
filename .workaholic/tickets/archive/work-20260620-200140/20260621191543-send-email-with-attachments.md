---
created_at: 2026-06-21T19:15:43+09:00
author: a@qmu.jp
type: enhancement
layer: [UX, Domain]
effort:
commit_hash: 14c4c2b
category: Changed
depends_on:
---

# Send email with attachments (gmail-ftp v1.1)

## Overview

gmail-ftp v1 deliberately deferred sending: `send` is an inert stub and `put` only creates a draft from a **raw RFC 5322 `.eml`** (no friendly attachment composition). This makes the end-to-end "compose an email, attach a file, send it" flow impossible from the shell. This ticket ships that flow as v1.1:

1. **Un-defer `send`** — `send <draft>` actually sends an existing draft via Gmail `users.drafts.send`, reusing the already-granted `gmail.compose` scope (which covers sending drafts/messages — **no new OAuth scope**). It stays a separate, explicit, audited, irreversible verb; `put` must still NEVER send.
2. **Attachment composition** — let the user build a draft with a body plus one or more file attachments without hand-authoring MIME, following the trip's "attachment = nested file inside a message" model.

The safety bar from the trip still holds: sending is the *only* irreversible action and is reachable only through the explicit `send` verb.

## Key Files

- `internal/shell/commands.go` — command table (lines ~19-35) and handlers. `cmdSend` (currently returns the deferred error), `cmdPut` (currently `CreateDraft(raw)` only). Add compose/attach behavior here and wire `send`.
- `internal/shell/shell.go` — the narrow `gmailClient` interface (lines 31-46). Add `SendDraft` (and a draft lookup if needed) here; update the in-memory fake used by tests.
- `internal/gmail/client.go` — real client. `CreateDraft` at ~188. Add `SendDraft(ctx, draftID)` → `Users.Drafts.Send`, and any draft fetch needed to attach to / resolve an existing draft.
- `internal/gmail/model.go` — message/MIME helpers (`encodeRaw`, base64url, MIME part walking). Add a **multipart MIME builder** (body + attachments → raw `.eml` bytes) here; it is pure and unit-testable.
- `internal/audit/audit.go` — op constants. `OpSend` already exists (registered during the trip); ensure `send` and attachment/draft-compose are audited.
- `README.md` and `plugins/gmail-ftp/skills/gmail-ftp/SKILL.md` — remove the "send deferred to v1.1" language for the now-shipped verbs; document compose → attach → send.

## Related History

- Trip `gmail-ftp` (`.workaholic/trips/gmail-ftp/`): Plan **Amendment 1** locked `put`=draft-never-send and deferred `send`/`label`/`unlabel`; **Amendment 2** kept them as discoverable inert stubs. This ticket promotes `send` (and adds attachment compose) out of the deferred set. `label`/`unlabel` remain deferred unless separately ticketed.
- OAuth scopes were locked to `gmail.modify` + `gmail.compose` in `internal/auth/auth.go`. `gmail.compose` already authorizes `drafts.send` — confirm no scope change is needed and do not widen to full `mail.google.com`.

## Implementation Steps

1. **Client: `SendDraft`** — add `SendDraft(ctx context.Context, draftID string) (*gmail.Message, error)` to `internal/gmail/client.go` calling `c.srv.Users.Drafts.Send(user, &gmail.Draft{Id: draftID})`. Map 404 via the existing `notFound` helper. Add it to the `gmailClient` interface and the test fake.
2. **MIME builder** — in `internal/gmail/model.go`, add a pure function that assembles a `multipart/mixed` RFC 5322 message from: headers (To/Subject and optional Cc), a plain-text body, and N attachments (filename + content-type + bytes, base64-encoded parts). Return raw bytes suitable for `CreateDraft`. Unit-test the MIME structure (boundaries, headers, base64 part) with table-driven tests — no live creds.
3. **Compose a draft with metadata** — provide a way to create a draft with To/Subject/body, not just a raw `.eml`. Pick the lowest-surprise UX consistent with the FTP metaphor and gdrive-ftp parity; suggested: a `compose` verb (`compose --to <addr> --subject <s> [body-file]`) that builds MIME via step 2 and calls `CreateDraft`, **plus** keep `put <local>` (no target) = create draft from a raw `.eml` (unchanged).
4. **Attach to an existing draft** — `put <localfile> <draft>` (target is a draft, addressed by `id:<draftId>` or `id:draft:<id>`): fetch the draft, decode its current MIME, append the local file as a new `multipart/mixed` attachment part, and update the draft (`Users.Drafts.Update`). Add the needed client method(s) + interface + fake. Audit as a draft mutation.
5. **`send` verb** — replace the `cmdSend` stub with: resolve the draft argument (`send <draft>` / `send id:<draftId>`), call `SendDraft`, audit `OpSend` (irreversible), and emit a clear result (`sent message <id>`), honoring `-json`. Keep it OUT of any `put` path.
6. **Help/dispatch** — update the command table entries for `send` (drop the `[deferred to v1.1]` tag, real usage/help) and add `compose` (and document `put <local> <draft>`). Keep `label`/`unlabel` deferred unless separately scoped.
7. **Docs** — update `README.md` and `SKILL.md`: remove "send deferred to v1.1" for `send`; document the compose → attach → send flow and that sending is irreversible and audited.
8. **Quality gate** — `go build ./...`, `go vet ./...`, `gofmt -l .`, `go test ./...` all clean. Add tests: `SendDraft` via fake, MIME builder structure, `put <local> <draft>` attaches (fake), `send` audits + never reachable from `put`, and that `put <local>` with no target still creates a draft and never sends.

## Considerations

- **Scope discipline (operation/security):** confirm `gmail.compose` already covers `drafts.send`; do NOT add `gmail.send`/full-mail. If the live grant lacks send capability, surface a clear actionable error rather than widening scope silently.
- **Irreversibility (design safety):** `send` is the first and only irreversible mutation. Audit it distinctly (`OpSend`). It must remain a separate explicit verb — never auto-send from `put`/`compose`. Consider echoing the recipient in the result so the user sees what went out.
- **Testability (implementation):** all new behavior must be exercisable through the fake `gmailClient` with no live credentials, matching the existing table-driven test bar. The MIME builder is pure — test it directly.
- **gdrive-ftp parity (design):** keep the command vocabulary and output style consistent with the FTP metaphor and the sibling tool; attachments map to the "nested file inside a message" model already established in the trip.
- **Partial-failure (operation):** attaching to a draft then failing to update should not corrupt the draft; prefer building the new raw message and a single update call. Sending a draft that no longer exists should 404 cleanly.
