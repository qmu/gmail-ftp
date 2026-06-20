# Design v2

Author: Constructor
Status: draft
Reviewed-by: Planner (request-revision, addressed); Architect (minor suggestions, folded in)

## Content

This design ports `gdrive-ftp` (an FTP-style CLI over the Google Drive v3 API)
to `gmail-ftp` (an FTP-style CLI over the Gmail v1 API). The guiding constraint
is **structural parity**: same package layout, same CLI wiring, same auth/token
discipline, same JSON contract shape, same audit-log mechanism, same plugin/skill
packaging, and the same unit-test bar. Gmail's domain (labels, messages,
attachments, drafts) is mapped onto the existing FTP verb vocabulary so the user
experience is recognizably "the same tool, for mail."

**What changed from v1.** This version adopts the team-converged **2-level
navigation** model and resolves the three Planner concerns (navigation
divergence, `rm` blast radius, `mkdir` ambiguity) plus the Architect's four minor
suggestions. The contested `label → thread → message` third tier is removed:
threads are no longer a `cd` target. See the "Revision summary (v1 → v2)" block
below and the **Review Notes** for the point-by-point reconciliation.

### 0. Canonical navigation model (resolved)

Gmail has no directory tree, so we synthesize one from labels. The model is
exactly **two conceptual levels** — the same depth as gdrive-ftp's *virtual root
→ drive → file* (a drive is the one navigable container above the leaf; nested
labels give apparent depth but every level is itself a label, addressable from
the root):

| gdrive-ftp level | gmail-ftp level |
| --------------------------- | ------------------------------------------------- |
| virtual root (lists drives) | virtual root (lists **labels**: INBOX, SENT, …)   |
| a drive (My Drive / Shared) | a **label** (e.g. `INBOX`, `Work/Receipts`)       |
| a file inside a drive/folder | a **message** under a label (named by date + subject) |
| a nested file               | an **attachment**, a leaf inside a message        |

`ls /` lists labels; `cd INBOX` enters a label and `ls` lists its **messages**;
a message is a leaf you cannot `cd` into; `ls <message>/` lists that message's
attachments. `get` downloads a message (as `.eml`) or an attachment (raw bytes);
`put` creates a draft; `send` sends a draft (explicit, audited); `rm` trashes a
single message (reversible). `mkdir <name>` creates a user label.

**A thread is a grouping view, not a navigation tier.** Every message row carries
a `threadId` field, and a whole thread is addressable via `id:thread:<id>` and
exportable as `.mbox` (a power-user opt-in). You never `cd` into a thread, and
`Ref.Kind` is `{label, message}` — see §2 (Ref) and the Review Notes.

This resolved default is written verbatim into the README command table and
SKILL.md so a persona's first `ls` matches the docs.

### 1. Scope and inventory

File-by-file inventory mirroring gdrive-ftp's layout. `<same>` means the file is
structurally a near-copy with names/strings rebranded; differences are called out.

| Path | Purpose | Difference from gdrive-ftp counterpart |
| ---- | ------- | -------------------------------------- |
| `go.mod` | Module `gmail-ftp`, Go 1.25.x | Replace `google.golang.org/api/drive/v3` usage with `gmail/v1`; same `golang.org/x/oauth2`, `golang.org/x/term`, `google.golang.org/api`. `go.sum` regenerated. |
| `main.go` | CLI wiring: flags, one-shot vs interactive, `auth`/`log`/`completion zsh`/`__complete` subcommands, config-dir path helpers | Near-identical. Rename `gdrive`→`gmail` imports, banner "Connected to Gmail.", config dir `~/.config/gmail-ftp/`, default creds/token/log paths. `gmail.New(ctx, hc)` instead of `gdrive.New`. Flags `-creds/-token/-json/-no-log` unchanged. |
| `internal/auth/auth.go` | OAuth2 terminal consent flow + token cache/refresh (`savingSource`, OSC 52 clipboard, `codeFromRedirect`, `tokenFromFile`/`saveToken`) | **Copy almost verbatim.** Only change: the scope. Replace `drive.DriveScope` with the locked Gmail scope set (see §2). The flow, clipboard, state/CSRF handling, and token persistence are domain-agnostic and reused unchanged. |
| `internal/auth/auth_test.go` | Tests `codeFromRedirect` + `clipboardSeq` | Copy verbatim (these test domain-agnostic helpers); only the sample scope string in a redirect URL changes cosmetically. |
| `internal/gmail/client.go` | Thin Gmail v1 wrapper (replaces `internal/gdrive/client.go`) | **New.** Exposes: `ListLabels`, `ListMessages(labelID, query)`, `GetMessage(id, format)`, `GetThread(id)` *(internal batching only — not a nav tier)*, `GetAttachment(msgID, attID)`, `GetRawMessage(id)` (RFC822 `.eml`), `Search(query)`, `CreateDraft`, `SendDraft`/`Send`, `TrashMessage` (default), `TrashThread` (explicit opt-in), `ModifyLabels(add,remove)`, `CreateLabel`, `DeleteLabel`. Quarantines the SDK; no folders/upload-media. |
| `internal/gmail/model.go` | **New (Architect Sug 3).** Owned `Ref`/`Message`/`Label` structs, **message-name synthesis** (date-prefix + subject slug + collision/`id:` strategy), MIME/header helpers. | **New.** This is the single genuinely new domain concern (Drive files already have names) and gets its own named home with table-driven tests, rather than being buried in `client.go`. |
| `internal/gmail/model_test.go` | Unit tests for the owned domain logic | **New.** `TestMessageName` (date+subject slug, empty-subject `(no subject)`), collision cases (`id:` fallback), header parsing (subject/from/date), attachment-part walking, MIME body base64url decode, label-name normalization — all from synthetic `gmail.Message`/`gmail.MessagePart` literals (no live API). |
| `internal/gmail/client_test.go` | Unit tests for transport-adjacent pure helpers | **New.** Helpers that are about the API call shape (query assembly, format selection, label-id sort with system labels first). |
| `internal/shell/shell.go` | REPL, raw-mode line editor, tokenizer, path resolution, Tab completion, `friendlyErr` | `<same>` skeleton. Path resolution now resolves **label → message** (two levels), not drive→folder→file and **not** label→thread→message. `Ref` carries `{ID, Name, Kind, ThreadID}` where **Kind ∈ {label, message}** (Architect Sug 2); `ThreadID` is a field, never a stack frame. `friendlyErr` rewrites the "Gmail API disabled" 403 (URL `gmail.googleapis.com`). Banner string `gmail:/>`. Tokenizer, quoting, completion plumbing copied verbatim. |
| `internal/shell/commands.go` | Command implementations + formatting helpers | `<same>` structure, remapped verbs (see §2 table). `ls`/`cd`/`pwd`/`get`/`put`/`rm`/`find`/`mkdir` retained; gmail adds `search`, `label`/`unlabel`, `send`. `mkdir <name>` = create a user label (resolved — see §2). Local helpers `lcd`/`lls`/`lpwd` copied verbatim. |
| `internal/shell/output.go` | Owned JSON DTOs + `emit()` seam | `<same>` pattern. `fileEntry`→`entry` with `{path,name,id,kind,from,subject,date,unread,size,threadId,labels}`. `actionResult` action vocabulary becomes `downloaded`/`drafted`/`sent`/`trashed`/`labeled`/`unlabeled`. The "never marshal the vendor struct" rule is preserved: translate `gmail.Message`→owned DTO. |
| `internal/shell/shell_test.go` | Pure-function tests | Copy the domain-agnostic tests verbatim (tokenize, splitPath, lastTokenStart, quoteArg, longestCommonPrefix, filterByPrefix, byteCount, emit). Rewrite the gdrive stack tests (`TestPwd`, `TestCurrentID`, `TestSingleDriveArg`) for the **label/message** stack (no thread frame). Add `TestArgKind`/`TestFriendlyErr` for gmail verbs. |
| `internal/audit/audit.go` | Append-only JSONL logger + rotation | **Copy near-verbatim.** Only the `Operation` constants change: `OpDraft`/`OpSend`/`OpTrash`/`OpLabel`/`OpUnlabel`/`OpMkLabel` replace `OpUpload`/`OpTrash`/`OpMkdir`. `Entry` keeps `time/op/name/id/cwd` and adds optional `threadId`, `labelIds`. Rotation, perms (0700/0600), nil-logger no-op: unchanged. |
| `internal/audit/reader.go` | `Read`, `Verb`, `WriteJSON`, `WriteText` | `<same>`. `Verb` maps the new ops to user words (`drafted`/`sent`/`trashed`/`labeled`/`unlabeled`/`created label`). |
| `internal/audit/browser.go` | tig-like read-only TUI | **Copy verbatim** except `render` header string ("gmail-ftp audit log") and `showDetail` fields (add thread/labels). Cursor/viewport/render logic is domain-agnostic. |
| `internal/audit/*_test.go` | audit/reader/browser tests | Copy near-verbatim; adjust op constants and `Verb` expectations. Rotation, read-skip-corrupt, moveCursor, viewportTop, rowText tests reused unchanged. |
| `README.md` | User-facing spec | Rewritten for Gmail: concept, build, one-time Google setup (enable **Gmail API**, scopes), usage, command table (the resolved 2-level model verbatim), JSON output, audit log, notes/limitations, project layout, plugin install. Same section structure and tone. |
| `LICENSE` | MIT | Copy verbatim. |
| `.gitignore` | ignore secrets + binary | `<same>`; `gdrive-ftp`→`gmail-ftp` binary name. Keeps `credentials.json`/`token.json`/`*.token.json` ignored. |
| `plugins/gmail-ftp/skills/gmail-ftp/SKILL.md` | Agent skill | Rewritten: teach an agent to drive the Gmail CLI (auth prerequisite, **2-level label/message** model, `id:` + `id:thread:` addressing, one-shot usage, JSON contract, gotchas). Same skeleton as gdrive's SKILL.md. |
| `plugins/gmail-ftp/.claude-plugin/plugin.json` | Claude plugin manifest | `<same>` rebranded. |
| `plugins/gmail-ftp/.codex-plugin/plugin.json` | Codex plugin manifest | `<same>` rebranded; `repository` → gmail-ftp. |
| `.claude-plugin/marketplace.json` | Claude marketplace | `<same>` rebranded. |
| `.agents/plugins/marketplace.json` | Codex marketplace | `<same>` rebranded. |

**Not ported / intentionally out of scope for v1:** Shared-Drive corpus handling
(no Gmail analogue), media byte-for-byte upload (Gmail has no file upload; `put`
becomes drafting), and Google-native-doc export. The `id:` direct-addressing
convention **is** ported (message/attachment IDs, plus `id:thread:<id>` for the
opt-in thread container/`.mbox` export), because it is the same "act on an item
by its stable ID" affordance and is highly useful for agents.

### 2. Implementation approach

**Gmail API client wrapper (`internal/gmail/client.go`).** Build the service with
`gmail.NewService(ctx, option.WithHTTPClient(hc))` (mirrors `drive.NewService`).
The Gmail v1 resource shape differs from Drive, so the wrapper hides it:

- `ListLabels(ctx)` → `Users.Labels.List("me")`; returns owned `Label{ID,Name,Type}`
  refs sorted with system labels (INBOX/SENT/DRAFT/…) first, then user labels.
- `ListMessages(ctx, labelID, query)` → `Users.Messages.List("me").LabelIds(labelID).Q(query)`,
  paginated via `Pages`, with a **default page cap** (see below / R2). Each
  returned message is fetched with `Format("metadata")` and a restricted
  `metadataHeaders` set (Subject/From/Date) for the listing row. The default leaf
  is the **message** — listings are per-message, no thread grouping.
- `GetMessage(ctx, id, format)` → `Users.Messages.Get`. `format=metadata` for
  listings, `full` for body, `raw` for `.eml`.
- `GetThread(ctx, id)` → `Users.Threads.Get("me", id).Format("full")`. **Internal
  batching / opt-in export only** — backs `id:thread:<id>` access and the `.mbox`
  power-user export; it is *not* a navigation tier and produces no `Ref` frame.
- `GetAttachment(ctx, msgID, attID)` → `Users.Messages.Attachments.Get`; body data
  is base64url — decode in the wrapper and stream bytes out.
- `Search(ctx, query)` → `Users.Messages.List("me").Q(query)`; Gmail's native
  search, the direct analogue of Drive's `name contains`.
- `CreateDraft(ctx, raw)` / `SendDraft(ctx, id)` / `Send(ctx, raw)` → `Users.Drafts.*`,
  `Users.Messages.Send`. `raw` is an RFC 5322 message assembled from a local file.
- `TrashMessage(ctx, id)` → `Users.Messages.Trash` (the **default** `rm` target,
  reversible). `TrashThread(ctx, id)` → `Users.Threads.Trash`, reachable **only**
  via the explicit `rm id:thread:<id>` opt-in.
- `ModifyLabels(ctx, id, add, remove)` → `Users.Messages.Modify`. Backs
  `label`/`unlabel`.
- `CreateLabel(ctx, name)` / `DeleteLabel(ctx, id)` → `Users.Labels.Create/Delete`.
  Backs `mkdir` and label removal.

Pure helpers (unit-testable without the API), now homed in `model.go`:
`headerValue(msg, "Subject")`, `decodePart(part)` (base64url body),
`walkParts(payload)` (collect attachments), `isUnread(msg)`,
`normalizeLabelName`, and **`messageName(msg)`** — the date-prefix + subject-slug
synthesis with empty-subject and collision handling.

**OAuth scopes (locked — Planner concern 1 / Architect Sug 4).** gdrive-ftp uses
the single broad `drive` scope. For Gmail the choice is **locked**, not left open:
request the **narrowest set that covers the shipped verbs**, never the full
`https://mail.google.com/`, and **never** request hard-delete capability:

- `https://www.googleapis.com/auth/gmail.modify` — `ls`/`cd`/`get`/`find`/`search`
  (read), `rm` (trash), `label`/`unlabel`, `mkdir` (label create). **Cannot
  permanently delete** — exactly the safety property we want, matching Drive's
  "trash not delete."
- `https://www.googleapis.com/auth/gmail.compose` — `put` (draft) + `send`.

These two scopes are declared **once**, as a single documented constant in
`auth.go`, with a one-line comment stating that `modify` subsumes read and cannot
hard-delete (the safety property). This is the single source of truth — no scope
wording drift between `auth.go`, README, and SKILL.md.

**Command set mapping (resolved).**

| gdrive verb | gmail verb | Behavior |
| ----------- | ---------- | -------- |
| `ls [dir]` | `ls [dir]` | root → labels; in a label → **messages**; `ls <message>/` → attachments. |
| `cd [dir]` | `cd [dir]` | navigate **root → label only**. Messages and threads are **not** `cd` targets (threads reachable by `id:thread:<id>`, not enterable). |
| `pwd` | `pwd` | print `/INBOX` (the label). A message is the leaf; `pwd` is never `/INBOX/<thread>`. |
| `find <pat> [dir]` | `find <pat> [dir]` | substring match over message subjects in scope (client-side, mirrors gdrive's re-filter). |
| (n/a) | `search <gmail-query>` | **new**: raw Gmail search syntax (`from:x is:unread`); `find` stays the simple subject substring. |
| `get <remote> [local]` | `get <remote> [local]` | a message → `.eml` (raw); an attachment leaf → raw bytes; `get id:thread:<id>` → `.mbox` thread export (opt-in). Atomic temp-rename (`saveToFile` reused verbatim). |
| `put <local> [remote]` | `put <local> [remote]` | create a **draft** from a local RFC822 file (or a body file + `remote` as recipient). Returns draft id. Logged. **Never sends.** |
| (n/a) | `send <local>` / `send id:<draftID>` | **new**: send a draft or composed message. Audited with full recipient/subject metadata; one-line recipient echo; `--yes` gate in interactive mode. Highest-impact mutation. |
| `mkdir <name>` | `mkdir <name>` | **create a Gmail user label** (the faithful container-creation analog). Logged. |
| (n/a) | `label <msg> <name>` / `unlabel <msg> <name>` | **new**: apply/remove a label on a message via `ModifyLabels`. Each is explicit and audited with an echo of the affected message + label. |
| `rm <name>` | `rm <name>` | **trash a single message (default)**; reversible (TRASH label, never hard-delete). A whole thread is **never** trashed implicitly — use the explicit `rm id:thread:<id>`. Logged. |
| `lcd`/`lls`/`lpwd` | same | copied verbatim (local FS helpers). |
| `help`/`?` | same | regenerated table. |

**Shell-loop reuse.** `runTerminal`/`runScanner`/`dispatch`/`Execute`,
`crlfWriter`, the tokenizer, `quoteArg`, `lastTokenStart`, `splitPath`,
`filterByPrefix`, `longestCommonPrefix`, and the whole Tab-completion machinery
(`autoComplete`/`completeInput`/`Complete`/`argKind`/`remoteNames`/`localNames`)
are **structurally unchanged**. Only `remoteNames` and the resolver call into the
gmail client (**labels then messages — two levels**) instead of the drive client,
and `argKind` gains `search`/`send`/`label`/`unlabel` entries. The `id:` parsing
(`parseIDArg`) is copied verbatim and now resolves message/attachment IDs plus
the `id:thread:<id>` form.

### 3. Quality strategy

The quality bar is **gdrive-ftp's own bar**: every pure function has a
table-driven `_test.go`, the SDK struct is never marshaled directly, mutations
are audited, and `go vet` is clean. Concretely:

**Compiler / static checks (must pass before reporting done):**
- `go build ./...`
- `go vet ./...`
- `gofmt -l .` returns empty.
- `go test ./...` green. (If `golangci-lint` is available project-locally it is
  run; it is **not** installed globally — system-safety forbids global installs,
  so a missing linter is a documented soft check, never a blocker.)

**Unit tests (no live Gmail credentials required).** Everything the API does not
touch is tested against synthetic structs and in-memory writers:

- `internal/auth`: `codeFromRedirect` (state/CSRF, bare-code, error param) and
  `clipboardSeq` (OSC 52 + tmux passthrough) — copied verbatim.
- `internal/gmail`: in `model_test.go`, message-name synthesis (incl.
  empty-subject and collision→`id:`), header extraction, MIME body base64url
  decode, attachment-part walking, unread detection, label-name normalization —
  all from hand-built `gmail.Message`/`gmail.MessagePart` literals; in
  `client_test.go`, query/format/label-sort helpers.
- `internal/shell`: tokenize, splitPath, lastTokenStart, quoteArg,
  longestCommonPrefix, filterByPrefix, parseIDArg (incl. `id:thread:`), argKind,
  **pwd over a label/message stack** (no thread frame), byteCount, `emit`
  JSON-vs-text seam, `encodeErrorJSON`, `toEntry` DTO translation (omitempty of
  size/subject), `friendlyErr` for the Gmail-disabled 403.
- `internal/audit`: marshal/omitempty, append+perms (0600/0700), nil-logger
  no-op, rotation at cap, ring shift+drop, read-concatenate-oldest-first,
  skip-corrupt-line, moveCursor/viewportTop/rowText — copied with op-constant edits.

**Testability via a fake `gmailClient` interface.** To keep command dispatch
testable without a network, the shell depends on a **narrow `gmailClient`
interface** (the set of methods commands call) rather than the concrete
`*gmail.Client`. This is a deliberate, low-cost improvement over gdrive-ftp; a
fake client returns canned messages so `cmdLs`/`cmdGet`/`cmdRm`/`cmdLabel` output
formatting and `id:` dispatch are unit-tested end-to-end without Gmail. The
interface is defined in `shell` and satisfied by `*gmail.Client`. **`rm` default
single-message trash** is asserted in a fake-client test (verify `rm <message>`
calls `TrashMessage`, never `TrashThread`).

### 4. Delivery plan

Ordered, each step independently buildable, testable, and committable
(`go build ./... && go test ./...` green at every step):

1. **Scaffold module.** `go.mod` (`module gmail-ftp`, Go 1.25.x), `LICENSE`,
   `.gitignore`, empty package dirs. `go mod tidy` pulls `gmail/v1`. Commit.
2. **Auth.** Copy `internal/auth/auth.go`+test verbatim; swap the scope constant
   to the locked `gmail.modify` + `gmail.compose` set. `go test ./internal/auth/...`
   green. Commit.
3. **Gmail client + model.** `internal/gmail/client.go`, `model.go`, and their
   tests: types, service constructor, list/get/search/draft/send/trash/modify/
   label wrappers, message-name synthesis, pure parsing helpers with full unit
   tests. Commit.
4. **Shell + commands + output.** Port `shell.go`/`commands.go`/`output.go` and
   `shell_test.go`; wire the `gmailClient` interface, the 2-level resolver, the
   verb table, the DTOs, and `friendlyErr`. Add a fake client for command-level
   tests (incl. the `rm`-single-message assertion). Commit.
5. **Audit.** Port `internal/audit/*` (+ tests) with the new op constants. Commit.
6. **main.go.** CLI wiring, flags, config-dir helpers, `auth`/`log`/`completion`/
   `__complete` subcommands, banner. `go build` produces the `gmail-ftp` binary.
   Commit.
7. **Plugin + README + skill.** `plugins/gmail-ftp/**`, both `marketplace.json`,
   `README.md`, `SKILL.md` (the resolved 2-level model documented verbatim).
   Commit.

(Per trip protocol, the Constructor does not run git directly in this shared
branch-only tree; the lead serializes commits. The steps above are the commit
*boundaries* the lead can land.)

### 5. Risk assessment

| # | Risk | Likelihood / impact | Mitigation |
| - | ---- | ------------------- | ---------- |
| R1 | **OAuth scope sensitivity.** Gmail scopes are "restricted"; broad scopes trigger Google's verification and look alarming on the consent screen. | High sensitivity | Request the **narrowest** scopes covering the surface (`gmail.modify` + `gmail.compose`, **never** the full `https://mail.google.com/`, **never** hard-delete). Single documented constant in `auth.go`. Document in README that this is a personal "Desktop app" OAuth client with the user as a test user (same as gdrive-ftp), so no Google verification is needed. |
| R2 | **API quotas / N+1 message-metadata fetches.** Listing a label returns message IDs; fetching each message's metadata is an extra call → quota/latency on large mailboxes. (Dropping the thread tier removes the *thread*-level N+1, but per-message metadata remains.) | Medium / Medium | `Format("metadata")` with a restricted `metadataHeaders` set (Subject/From/Date). **Concrete default page cap** (e.g. first 50 messages) with a one-line "showing N of many — use `search`/paging" hint, so the first `ls` is a fast partial list, not a stall (Planner concern 4). Paginate transparently; use `Q` to scope; cache within a single command. README notes the latency, mirroring gdrive's "brief pause on large folders." |
| R3 | **Message naming ambiguity.** A "path" names a label or a message; subjects are non-unique and may be empty. | Medium / Medium | Resolver returns `ErrAmbiguous` (ported semantics) when a subject matches multiple messages, refusing to guess. `id:` addressing is the unambiguous escape hatch (ported). Render empty subjects as `(no subject)` but key navigation off IDs. `messageName` synthesis + collision tests live in `model_test.go`. Document the model prominently in README + SKILL.md. |
| R4 | **Attachment handling.** Attachment bytes are base64url inside nested message parts and can be large. | Medium / Low | `walkParts` recursion is unit-tested against nested synthetic payloads. Decode in the wrapper and stream to `saveToFile` (atomic temp-rename, length-aware) — reuse gdrive's proven download path. |
| R5 | **Credential storage.** Same secret-handling risk as gdrive: `credentials.json`/`token.json` grant mailbox access. | High impact / Low likelihood | Reuse gdrive's exact discipline: token cached `0600` under `0700` config dir; both files git-ignored; audit log never records contents or credentials; SKILL.md repeats "never read/print/commit these." Gmail raises the stakes (mailbox > drive), so README's warning banner is strengthened. |
| R6 | **Send is irreversible.** Unlike trash, a sent email cannot be recalled. | Low likelihood / High impact | `send` is an explicit, separate verb (never a side effect of `put`); `put` only creates a **draft**. `send` is audited with full recipient/subject metadata and echoes recipients; interactive mode gates on `--yes`; one-shot/agent use must be explicit by construction. |
| R7 | **`rm` blast radius.** A default that trashed a whole thread could silently remove a conversation the user did not intend (Planner concern b). | Medium / Medium | **`rm <message>` trashes one message by default** (narrowest unit); a whole thread is never trashed implicitly — only via the explicit `rm id:thread:<id>`. Reversible via TRASH. Asserted by a fake-client test. |
| R8 | **Gmail API disabled for the project (403).** Same failure class gdrive rewrites. | Low / Low | Port `friendlyErr` with the `gmail.googleapis.com` activation URL + project-number extraction; covered by `TestFriendlyErr`. |
| R9 | **System-safety constraint.** Detection must confirm `system_changes_authorized:false` (regular project). | n/a | Run `system-safety/scripts/detect.sh` before implementation. All dependencies project-local via `go.mod`/`go.sum`; no global installs, no shell-profile or system edits. A missing global linter is a documented soft check, never worked around with a global install. |

## Review Notes

### Revision summary (v1 → v2)

This version was produced in response to the Planner's **Request revision** on
design-v1 (`reviews/round-1-planner.md`) and folds in the Architect's four minor
suggestions (`reviews/round-1-architect.md`). Reconciliation is recorded in
`reviews/response-constructor-to-planner.md`.

**Planner concerns:**
- **(a) 3-level vs Model divergence — resolved.** Adopted the canonical **2-level
  navigation** (root → label → message; attachments as leaves inside a message;
  threads opt-in via `threadId` + `id:thread:<id>` + `.mbox` export). The
  `label → thread → message` third tier is removed; threads are not a `cd` target.
- **(b) `rm` blast radius — resolved.** `rm <message>` trashes a **single message**
  by default; a whole thread is never trashed implicitly (explicit
  `rm id:thread:<id>` only). gdrive-ftp's reversible-trash semantics preserved.
- **(c) `mkdir` ambiguity — resolved.** `mkdir <name>` = **create a Gmail label**.
  Message-level membership ships as the explicit, audited `label`/`unlabel` verbs;
  `mklabel` dropped as a redundant alias.

**Architect suggestions (folded in):**
- **Sug 1 (2-level canonical model) —** adopted; §0 table collapsed to label →
  message; thread demoted to a grouping/`id:` affordance.
- **Sug 2 (`Ref.Kind`) —** narrowed to **`{label, message}`**; `ThreadID` is a
  field on the message entry, never a stack frame; `GetThread` is an internal
  batching detail.
- **Sug 3 (`model.go` home) —** kept `internal/gmail/model.go` for owned
  `Ref`/name-synthesis logic with `model_test.go` (`TestMessageName`, collisions).
- **Sug 4 (single scope source of truth) —** scope set stated **once** as a
  documented constant in `auth.go`: `gmail.modify` + `gmail.compose`, with the
  one-line comment that `modify` subsumes read and cannot hard-delete.

**Retained from v1 unchanged:** the fake-`gmailClient`-interface unit-test
strategy (no live credentials), least-privilege OAuth scopes, `id:` addressing,
the explicit audited `send` verb, and the file-by-file structural-parity inventory.

(Reviewer responses to design-v2 to be appended in the next round.)
