# Model v2

Author: Architect
Status: draft
Reviewed-by: Constructor (request-revision, addressed)

## Content

This model bridges the business vision ("a Gmail version of gdrive-ftp — same
concept, same directory structure, same experience, but email instead of
files") to a technical build. It is grounded in a close read of the reference
codebase `../gdrive-ftp` (main.go, internal/auth, internal/gdrive,
internal/shell, internal/audit, README.md, the SKILL.md agent skill, and the
plugin manifests). The governing constraint is **fidelity to the gdrive-ftp
structure and FTP experience**: an interactive `ls/cd/pwd/get/put/mkdir/rm`
shell plus one-shot mode, `-json` output, an audit log of mutations, terminal
OAuth, Tab/zsh completion, and an agent skill. We preserve every one of those
seams and re-point only the backend domain.

**What changed v1 → v2 (Constructor request-revision, addressed):** the central
navigation decision is now **canonical, not recommended-then-punted** — the
mapping is committed to **2-level** (`root → label → message`) and the cwd-stack
`Ref.Kind` is narrowed to `{label, message}`. The thread is demoted to a
one-layer-down concern inside `internal/gmail` (a `threadId` field +
`id:thread:<id>` addressing), not a navigable depth. A first-class **runtime
cost model** for the N+1 `list → batch-fetch` pattern is added (§3a), since the
verified Gmail `list` shape (IDs only) makes per-row metadata fetching the
product's dominant runtime cost. The scope choice and verb-set sequencing are
closed (§4, §5). Boundary integrity and the irreversible-`send`-as-separate-verb
decision are preserved from v1.

### 1. System coherence mapping

How gdrive-ftp is structured today, and the Gmail equivalent for each part:

| gdrive-ftp structure (today) | Responsibility | gmail-ftp equivalent |
| --- | --- | --- |
| `main.go` | CLI wiring: flags (`-creds`, `-token`, `-json`, `-no-log`), config-dir paths, subcommand routing (`auth`, `log`, `completion zsh`, hidden `__complete`), interactive-vs-one-shot dispatch, `signal.NotifyContext` for Ctrl-C, `fatal`. | Same file, near-identical. Only the imported backend package, the connect banner, scope, and config dir name (`gmail-ftp`) change. |
| `internal/auth/auth.go` | Google OAuth2 desktop-app consent over the terminal (OSC 52 clipboard, paste-back redirect, CSRF `state`), token caching + self-refreshing `savingSource`. | Reused almost verbatim. The **only** substantive change is the requested scope: swap `drive.DriveScope` for the scoped Gmail union (see §4). The consent UX, token cache, and refresh logic are domain-agnostic and must not be reinvented. |
| `internal/gdrive/client.go` | The backend abstraction: a thin, FTP-flavored wrapper over Drive v3 exposing exactly what the shell needs (`List`, `ListDrives`, `GetByID`, `Search`, `FindChildren`, `FindDir`, `FindOne`, `Mkdir`, `Upload`, `Download`, `Export`, `Trash`) plus `Ref`, `IsFolder`, `IsGoogleDoc`, MIME/export tables. | Replaced by `internal/gmail/client.go`: a thin wrapper over **Gmail API v1** exposing the same *shape* of operations re-expressed in email terms (list labels, list/search messages, batch-fetch message metadata, get message/thread, fetch attachment, create draft, send, trash). This is the largest single piece of new work; its method surface is what makes the shell reusable. Thread access lives **here**, one layer below the shell. |
| `internal/shell/shell.go` | The REPL: raw-mode line editor + Tab completion, plain scanner fallback, the `Ref`-stack working directory, path resolution (`resolveDir`/`resolveFile`/`startStack`), tokenizer, `id:` addressing, `friendlyErr` API-disabled hint. | Reused with the same architecture. The cwd `Ref` stack, tokenizer, `id:` mechanism, completion plumbing, and `friendlyErr` carry over directly. Path *resolution* changes because the hierarchy is label-based, not folder-tree-based, and **`Ref.Kind` is narrowed to `{label, message}`** (see §2/§3). |
| `internal/shell/commands.go` | The verb implementations (`cmdLs`, `cmdCd`, …) and formatting helpers. | Same dispatch table and verb names; bodies re-point to the gmail client and the new domain model. `get`/`put`/`rm`/`find` take on email semantics (see §2). |
| `internal/shell/output.go` | The owned JSON DTO layer (`fileEntry`, `actionResult`, `pwdResult`, `errorResult`, `emit`) that deliberately never marshals the vendor `*drive.File` directly. | Same pattern, same discipline: owned DTOs translate `*gmail.Message`/label structs so the JSON contract stays decoupled from the Gmail SDK. DTO field set is extended for email (see §2). |
| `internal/audit/*` | Append-only JSONL log of *mutations* (upload/trash/mkdir), size-rotated ring, read/JSON/text/TUI-browser, `gdrive-ftp log`. | Reused with the mutation vocabulary re-mapped to email mutations (send, draft-create, trash, label-change). The logger, rotation ring, reader, JSON/text emit, and `tig`-like browser are domain-neutral and carry over; only the `Operation` constants and `Entry` fields change. |
| `plugins/.../SKILL.md` + manifests | The agent skill teaching a coding agent to drive the one-shot CLI, the path model, auth prerequisite, JSON contract, gotchas; Claude/Codex/skills marketplaces. | A parallel `gmail-ftp` skill and manifests, same shape, re-authored for the email path model and gotchas (including the documented N+1 listing cost). |

The coherence principle: **auth, shell, audit, output, and the agent skill are
backend-agnostic plumbing; only the backend-client package and the domain model
inside the command bodies are genuinely email-specific.** That keeps the
translation traceable and bounds the new work.

### 2. Domain model — email onto the filesystem metaphor (canonical 2-level)

gdrive-ftp's hierarchy is: *virtual root → drive → folder tree → file*. The core
translation decision — how Gmail's flat, label-tagged, thread-grouped message
store is presented as that same hierarchy — is now **decided, not proposed**:

> **Canonical mapping: 2-level navigation — `root → label → message`.** A label
> is the only navigable container under root; a message is the leaf directly
> under a label. Attachments are leaves *inside* a message. There is **no thread
> tier in the navigable path.** The cwd-stack **`Ref.Kind ∈ {label, message}`**
> (the `thread` kind from v1's open question is removed). This was my round-1
> recommendation and is the team's converged resolution; it minimizes the N+1
> listing tiers (one, not two — see §3a) and yields a smaller, deterministic
> resolver matrix.

| Filesystem concept (gdrive-ftp) | Gmail domain entity | Mapping definition |
| --- | --- | --- |
| Virtual root `/` (lists drives) | The mailbox | `ls /` lists the **labels** (system labels INBOX, SENT, DRAFT, STARRED, IMPORTANT, TRASH, SPAM, plus user labels). A label is the first path component, exactly as a drive name is in gdrive-ftp. |
| Drive (first path component) | Label (`Ref.Kind = label`) | `cd INBOX`, `cd "Work/Receipts"`. A label is the top-level navigable container. Gmail nested labels (`Work/Receipts`) map naturally to the `/`-separated path, though they are flat tags, not a tree (see §3). |
| Folder | Label, and *only* a label | There is **no second tier of folders** inside a label, and **no thread tier**. The hierarchy is exactly two conceptual levels: label, then messages. (Nested labels give apparent depth, but every level is itself a label, addressable from the root.) |
| File | **Message** (`Ref.Kind = message`) — the canonical leaf | A leaf entry under a label is a **message**, rendered with a stable, sortable name (see naming below). `get`-ing it downloads the message. The thread is **not** a leaf and **not** a directory; it is metadata (`threadId`) plus `id:thread:<id>` addressing (see §3 item 3 and §3a). |
| File content (bytes via `get`) | The message itself | `get <message>` writes the message to a local file as **RFC 822 / `.eml`** (raw MIME, the email analog of "byte-for-byte download"). A `-json`/text export to a readable `.txt` (headers + body) is the email analog of gdrive-ftp's *export* of native Google docs. `.eml` is the faithful raw form; `.txt` the "exported" convenience form. |
| Nested file (attachment) | Attachment | An attachment is a leaf *inside* a message. `ls <message>/` lists its attachments; `get <message>/<attachment>` (or `get id:<attachmentRef>`) downloads one attachment byte-for-byte. Reuses gdrive-ftp's "a thing can contain things" listing seam without inventing real subfolders or a thread depth. |
| Search (`find`) | Gmail search query | `find <query> [label]` runs Gmail's native search (`users.messages.list` with `q=`), scoped to a label when anchored — analogous to Drive's `name contains` but far richer (`from:`, `subject:`, `is:unread`, date ranges). Matches print as paths/ids; `find` never mutates. Note: search returns IDs only — same N+1 as listing (§3a). |
| `put <local>` (upload) | Create draft (never send) | `put <local.eml> <label>` imports/creates a **draft** from a local RFC 822 file. Sending is an explicit, separate verb (`send`), never implicit, because sending is irreversible (§3 item 5). A draft is the writable leaf. |
| `mkdir <name>` | Create a label | `mkdir Work/Receipts` creates a user label. Direct, faithful analog. |
| `rm <name>` | Trash a message (reversible) | `rm <message>` moves the message to TRASH — gdrive-ftp's "trash, not hard-delete, reversible" promise. `rm <label>` deletes the *label* (untags; messages survive), called out as semantically different and guarded (§3 item 6). Default `rm` blast radius is the **single message**, never a whole thread. |
| `modifiedTime`, `size` (DTO) | `internalDate`, `sizeEstimate`, plus `from`/`subject`/`unread`/`threadId` | The owned `fileEntry` DTO gains email fields: `from`, `subject`, `date`, `unread`, `labels[]`, **`threadId`**. Listing rows render `subject` (and sender/date) as the "name". `threadId` is the sole surfacing of the thread at the shell boundary. |

**Message naming.** A message has no native filename, so the shell synthesizes a
stable, human-and-Tab-friendly display name and resolves it back to a message ID.
Recommended form: a short prefix from `internalDate` + a subject slug
(e.g. `2026-06-18 Quarterly report`), with the underlying Gmail message ID always
available via `id:` and every `-json` row. Because synthesized names can collide,
**`id:` addressing (already in gdrive-ftp) is the primary, canonical way to act on
a specific message** — leaning on an existing seam rather than inventing one.

### 3. Translation fidelity analysis

**Translation-fidelity note (new in v2).** Two facts about Gmail's data shape are
inherent to the domain, not implementation defects, and v2 surfaces them so they
are documented in README/SKILL rather than discovered at runtime:
1. **A label is a view/filter, not an owner.** A message legitimately appears
   under multiple labels at once (`/INBOX` and `/Work`). The cwd stack is a label
   *query path*, not an ownership tree; `cd` between labels is re-querying, and no
   "move" is implied. This is a feature of the metaphor.
2. **Gmail `list` endpoints return IDs only.** `messages.list` returns
   `{id, threadId}`; `threads.list` returns `{id, historyId, snippet}` with no
   subject and explicitly no message list. Any human-readable listing therefore
   requires a follow-up per-row metadata fetch — an unavoidable N+1 that is the
   product's dominant runtime cost (modeled in §3a). The 2-level mapping is chosen
   partly *because* it keeps this to a single N+1 tier.

**Fits cleanly:**
- *Label → directory* and *root lists labels → root lists drives*: structurally
  identical; the `Ref`-stack cwd model carries over unchanged.
- *Search → find*: Gmail search is a richer Drive `name contains`; the `find`
  verb, anchor narrowing, and "act on a result by `id:`" pattern transfer.
- *Trash is reversible*: Gmail's TRASH label exactly matches gdrive-ftp's "rm
  trashes, never hard-deletes" guarantee — a load-bearing safety promise kept.
- *Attachment → nested file*: the `ls <leaf>/` → list-children seam maps onto
  attachments without new architecture.

**Strains, with resolutions:**

1. **Labels are mutable, many-to-many tags, not a tree.** *Resolution:* treat a
   label as a **view/filter**, not an owner; listing a label is a query
   (`label:X`), exactly how `List` already works. No "move" semantics; `cd`
   between labels is re-querying. Documented in README/SKILL.

2. **Many-to-many membership breaks "rename/move".** *Resolution:* do **not**
   expose `mv`/rename for messages. Label mutation, if shipped, is an explicit,
   audited `label add/rm <message> <label>` verb, never an overload of `put`/`mv`.
   **v2 sequencing (closed):** v1 is read + trash + draft; `label add/rm` is
   deferred to a later increment unless its audit/echo UX ships with it (§5).

3. **Threads vs messages — resolved canonically.** Gmail's natural unit is the
   thread; the FTP leaf is the message. v1 left "default leaf granularity" as an
   open question; **v2 closes it: the message is the canonical leaf and the thread
   is demoted out of the navigable path.** The thread is a **one-layer-down
   concern inside `internal/gmail`**, surfaced at the shell boundary only as:
   **(a)** a `threadId` field on every DTO row, and **(b)** an addressable
   container via `id:thread:<id>`, with an opt-in `get id:thread:<id>` → `.mbox`
   export for the multi-message case. **No `cd thread`, no thread `Ref.Kind`, no
   resolver disambiguation at a shared depth, and no second N+1 tier.** A
   single-message thread (the triage common case) shows its message directly
   rather than forcing a pointless `cd thread` → one message.

4. **No true rename / immutable content.** A sent/received message's bytes are
   immutable. *Resolution:* messages are read-only leaves (`get`, `rm`-to-trash,
   label changes only). The single *writable* email object is the **draft**, where
   `put` lives.

5. **Send semantics are irreversible — unlike every gdrive-ftp mutation
   (preserved from v1).** gdrive-ftp's most dangerous op (`rm`) is still
   reversible (trash); sending cannot be undone. *Resolution (unchanged):* **never
   make `put` send.** `put` only creates a draft. An explicit, separate `send`
   verb operates on an existing draft, is always audited, and prints a one-line
   recipient confirmation. **v2 sequencing:** `send` is isolated to a v1.1
   increment behind its own verb + confirmation; v1 ships without it.

6. **`rm` overload (message vs label).** *Resolution:* `rm` targets the **single
   message** by default (minimal blast radius — never a whole thread); label
   deletion routes through an `rmdir`-style guard / confirmation so a user never
   trashes a whole label thinking they trashed one message. Audit both distinctly.

### 3a. Runtime cost model — the N+1 list→batch-fetch pattern (new in v2)

This is the **dominant runtime cost driver** of the product, raised by the
Constructor as must-fix C1 and the Planner as a UX promise. v1 under-modeled it as
a metaphor nuance; v2 makes it a first-class part of the model.

**The pattern.** A listing or search is two phases:
1. **`list` (IDs only).** `messages.list` / `threads.list` / `find` return only
   `{id, threadId}` (or `{id, historyId, snippet}`) — no subject, from, or date.
   This is one cheap call returning a page of IDs.
2. **N× metadata `get`.** To render a human-readable row (subject/from/date), the
   shell must issue one `messages.get` **per row**. This is the N+1 and the real
   cost — both in latency and in per-user Gmail quota units.

**Committed mitigations (fold into the build):**
- **Batched metadata fetch.** Per-row
  `messages.get(format=metadata, metadataHeaders=Subject,From,Date)` issued
  through the **batch / HTTP-pipeline** path, not serial round-trips. Fetch
  **only the headers the listing renders** during `ls` — never full bodies. Full
  body / `.eml` bytes are fetched lazily, only on `get`.
- **Pagination with a capped default page size.** Bounded first page
  (`maxResults`, e.g. first N rows) + `nextPageToken` continuation. The first `ls`
  on a large label is a **fast partial list** with a one-line "showing N of many —
  use search/paging" hint, not a stall. This is the Planner's responsiveness bar
  made concrete and the Sysadmin-over-SSH persona's make-or-break first `ls`.
- **Intra-command metadata cache.** A message-metadata cache keyed by message ID,
  scoped to a command (mirroring gdrive-ftp's single-command cache), so
  `cd`-then-`ls`-then-`get` and Tab-completion do not re-fetch the same rows.

**Where caching and quotas bite (documented):**
- The **per-row `get`s, not the `list`**, consume the bulk of Gmail per-user
  quota units and drive visible latency. Optimizing for fewer/cheaper `get`s
  (metadata-only, batched, paged, cached) is the lever.
- The **2-level mapping keeps this to a single N+1 tier.** A 3-level thread tier
  would add a *second* N+1 (expanding a thread's messages on `cd`), doubling the
  cost surface — a direct structural reason the 2-level decision is correct.
- Cache validity is intra-command only; cross-command staleness (labels/messages
  change server-side) is accepted rather than invalidated, matching gdrive-ftp's
  freshness posture.

### 4. Boundary integrity assessment

**Package boundaries to preserve unchanged** (the source of the "same experience"
guarantee), all mirroring gdrive-ftp:
- **auth boundary** — `auth.Client(ctx, creds, token) (*http.Client, error)`.
  Keep the signature and terminal-OAuth/token-cache behavior identical; change
  only the requested OAuth **scope**.
- **backend-client boundary** — the shell depends only on a narrow set of client
  methods. The new `gmail.Client` exposes a similarly small, FTP-flavored surface
  so the shell stays thin and the SDK stays quarantined behind one package. **The
  thread/batch-fetch complexity lives entirely behind this boundary**, not in the
  shell.
- **shell boundary** — `shell.New(ctx, client, out, jsonOut, log)` and the verb
  dispatch table. Keep verb names and the REPL/completion architecture.
- **audit boundary** — `audit.New(path)`, `Logger.Record(ctx, Entry)`, `Read`,
  `WriteJSON`/`WriteText`/`Browse`. Keep the JSONL ring + browser intact; change
  only the `Operation` vocabulary and `Entry` fields.
- **output/DTO boundary** — owned DTOs only; the vendor `*gmail.Message` is never
  marshaled directly, mirroring the existing `*drive.File` discipline.

**Gmail-specific API surface (the new boundary content):**
- **Scopes (decided in v2).** Use the **scoped union**, not full
  `https://mail.google.com/`. v1 ships read + trash + draft, so the v1 scope set
  is `gmail.modify` (read + trash + label/draft mutation) and `gmail.compose`
  (draft creation); `send` capability is added only when the `send` verb ships
  (v1.1). **Never request hard-delete capability.** This matches the Constructor's
  Design §2/R1 and the Planner's least-privilege trust ruling — the first OAuth
  consent screen should ask for the least alarming, most explainable permission.
- **Resources the client wraps:** `users.labels` (list/create/delete),
  `users.messages` (list with `q=`, **batch get with `format=metadata`**, full
  get, trash, modify-labels), `users.messages.attachments` (get),
  `users.threads` (get — for `id:thread:` export only, no thread listing tier),
  `users.drafts` (create/list/get), and (v1.1) `users.messages.send` /
  `users.drafts.send`. Each maps to one or a few client methods, mirroring how
  `gdrive.Client` wraps `Files`/`Drives`.
- **The Gmail-API-disabled `friendlyErr` hint** must be re-pointed at the Gmail
  API enablement URL/console, preserving the existing helpful-error behavior.

### 5. Component taxonomy

Proposed package layout for `gmail-ftp`, mirroring `gdrive-ftp` one-for-one:

```
main.go                                   CLI wiring, flags, subcommand routing,
                                          interactive-vs-one-shot dispatch
                                          (≈ unchanged; new backend import, scope,
                                          banner, config dir "gmail-ftp")
internal/auth/auth.go                     Google OAuth2 terminal consent + token
                                          cache/refresh (≈ verbatim; scoped union)
internal/gmail/client.go                  Gmail API v1 wrapper: ListLabels,
                                          ListMessages(query/label), BatchGetMeta,
                                          GetMessage, GetThread (id:thread: only),
                                          GetAttachment, CreateDraft, Trash,
                                          ModifyLabels, CreateLabel, DeleteLabel,
                                          (v1.1) Send. Thread + batch-fetch +
                                          N+1 cost handling live here.
internal/gmail/model.go (optional)        Owned Ref/Message/Label structs +
                                          message-name synthesis + MIME helpers +
                                          intra-command metadata cache
internal/shell/shell.go                   REPL, label-path resolution (Ref.Kind ∈
                                          {label, message}), tokenizer, id: /
                                          id:thread: addressing, completion,
                                          friendlyErr
internal/shell/commands.go                Verb implementations re-pointed at the
                                          gmail client and email domain model
internal/shell/output.go                  Owned JSON DTOs (message/label entries,
                                          incl. threadId) + emit seam (no vendor
                                          struct marshaled)
internal/audit/audit.go                   Append-only JSONL log of email mutations
internal/audit/reader.go                  Read/WriteJSON/WriteText of the log
internal/audit/browser.go                 tig-like read-only log browser
plugins/gmail-ftp/skills/gmail-ftp/       Agent skill (path model, auth, JSON,
                                          gotchas incl. N+1 listing cost)
.claude-plugin/marketplace.json           Claude Code plugin marketplace
.agents/plugins/marketplace.json          Codex plugin marketplace
```

The taxonomy keeps the package count, names, and dependency direction identical to
gdrive-ftp; the only renamed package is `internal/gdrive → internal/gmail`, and the
only genuinely new optional file is `model.go` (message-name synthesis + the
intra-command metadata cache, which have no gdrive-ftp counterpart).

**Decisions recorded in v2 (formerly "open questions"):**
- **Navigation / leaf granularity — CLOSED.** 2-level (`root → label → message`),
  `Ref.Kind ∈ {label, message}`, thread opt-in via `threadId` + `id:thread:<id>`
  (§2, §3 item 3). This is the team's converged resolution.
- **Scope — CLOSED.** Scoped union (`gmail.modify` + `gmail.compose`; `send` scope
  only when the `send` verb ships), never full `mail.google.com`, never
  hard-delete (§4).
- **Verb-set sequencing — CLOSED.** v1 = read + trash + draft. `send` (irreversible,
  separate audited verb + recipient echo) and message-level `label add/rm` are
  v1.1, shipping only with their safety UX (§3 items 2, 5).

**Remaining sequencing detail (v1 vs. later, not a blocker):**
- Message display-name format and collision strategy — lean on `id:` (§2); exact
  slug form is a build-time detail.
- Thread `.mbox` export format specifics for `get id:thread:<id>` — v1.1 alongside
  thread power-user features.

## Review Notes

- **Constructor C1 (N+1 under-modeled) — addressed.** New §3a runtime cost model
  commits to batched metadata fetch, capped pagination with continuation, and an
  intra-command metadata cache, and ties the cost to the 2-level decision (one N+1
  tier, not two). The N+1 is also surfaced as a translation-fidelity note (§3) so
  it is documented in README/SKILL.
- **Constructor C2 (close the navigation decision) — addressed.** §2 commits
  canonically to 2-level; `Ref.Kind` narrowed to `{label, message}`; the "default
  leaf granularity" and "scope" open questions are deleted and replaced with
  recorded resolutions (§5).
- **Constructor C3 (label/send verbs) — addressed.** v1 = read + trash + draft;
  `send` and `label add/rm` deferred to v1.1 with their safety UX (§3 items 2, 5;
  §5). The irreversible-`send`-as-separate-verb decision is preserved unchanged.
- **Constructor C4 (table convergence) — addressed.** With 2-level canonical, this
  model's mapping table is the agreed one; `design-v2.md` aligns to it so a single
  hierarchy ships.
- **Planner cross-artifact note.** The Planner round-1 ruling favored a 3-level
  human-facing default; the team converged on 2-level (this model's and the
  Constructor's position) for N+1 cost and resolver-simplicity reasons. The
  Planner's responsiveness concern is honored via the capped first page + "showing
  N of many" hint (§3a) and the `rm` minimal-blast-radius default (§3 item 6).
- **Open trade-off (per critical-review policy).** Committing to 2-level trades a
  small amount of human conversation-grouping muscle memory (the thread as a
  browsable folder) for metaphor crispness, a single N+1 tier, and a cleaner
  resolver. Mitigation: threads remain first-class for power users via
  `threadId` + `id:thread:<id>` + `.mbox` export, so nothing is lost, only made
  opt-in — the structurally faithful translation of "the file you fetch is the
  message."
