# Round 2 Review — Architect (Coding Phase, Analytical/Structural)

Reviewer: Architect (Neutral / Structural bridge)
Phase/step: coding/review-and-testing, iteration 1
Scope discipline: analytical/code review only — no build, no test execution.

## Artifacts and code reviewed

- Checklist: `.workaholic/trips/gmail-ftp/review-criteria.md`
- Specs: `models/model-v2.md`, `designs/design-v2.md`, `plan.md` → Amendment 1
- Source (read directly):
  - `main.go`
  - `internal/auth/auth.go`, `internal/auth/auth_test.go` (scope test)
  - `internal/gmail/client.go`, `internal/gmail/model.go`, `internal/gmail/client_test.go`, `internal/gmail/model_test.go` (test names)
  - `internal/shell/shell.go`, `internal/shell/commands.go`, `internal/shell/output.go`, `internal/shell/shell_test.go`
  - `internal/audit/audit.go`, `internal/audit/reader.go` (+ test names for browser/audit/reader)
  - `plugins/gmail-ftp/skills/gmail-ftp/SKILL.md`, `README.md` (traceability scan)
- Reference parity baseline: `../gdrive-ftp/internal/shell/{shell.go,commands.go,output.go}`, `../gdrive-ftp/internal/gdrive/client.go`.

## Decision

**Approve with minor suggestions.**

The implementation faithfully realizes the locked specs: the canonical 2-level
navigation model (`Ref.Kind ∈ {label, message}`, `ThreadID` a field), the
least-privilege scope union, the `rm`→single-message-trash safety promise, the
`put`→draft-never-send path, and the documented N+1-bounded listing. The
safety-critical unit tests exist and are correctly shaped. No top-risk
regression (R-B, R-C, R-D, R-E) is realized in shipped code. Two items keep this
from a clean "observations" approve: a **doc/dispatch deviation on the deferred
verbs** (E2) that should be reconciled with Amendment 1's literal wording, and a
**naming-of-the-quarantine-boundary clarification** (R-A/F2) that is a
documentation/contract sharpening, not a structural defect. Both have concrete
proposals below. Per policy I record at least one concern with a proposal even
though the decision is positive.

---

## Top-risk verdicts (H / R-A…R-F)

- **R-B (resolver carry-over → cd-able message / phantom third tier): CLEARED.**
  `Ref` is `{ID, Name, Kind, ThreadID}` with `Kind ∈ {KindLabel, KindMessage}`
  only (`model.go:17-31`); there is no `thread` Kind. `resolveDir`
  (`shell.go:661-686`) rejects a second named segment with "messages are leaves;
  cd a label", `pwd` is at most one label deep (`shell.go:579-589`), and
  `TestCmdCdRejectsMessageAsDirectory` asserts it. No drive→folder→file recursion
  was copied; the resolver is a clean two-level matrix.

- **R-C (blast-radius / irreversibility regression): CLEARED in behavior.**
  `cmdRm` default path calls `TrashMessage` only (`commands.go:438-449`);
  `TrashThread` is reachable solely via `parseThreadIDArg` opt-in
  (`commands.go:428-436`). `cmdPut` calls `CreateDraft` and never a send method
  (`commands.go:368-399`); the `gmailClient` interface exposes no `Send`/
  `SendDraft` at all (`shell.go:31-46`), so no send path is reachable from the
  shell in v1. Three tests pin this (`TestCmdRmTrashesSingleMessageNotThread`,
  `TestCmdRmByIDTrashesSingleMessage`, `TestCmdPutCreatesDraftNeverSends`). The
  one deviation from Amendment 1 is *registration* of the deferred verbs (see
  Concern 1), not reachability of an irreversible action.

- **R-D (scope over-grant / wording drift): CLEARED.** `auth.go:39-42` declares
  `Scopes = {GmailModifyScope, GmailComposeScope}` as the single source of truth,
  with a doc comment stating `modify` subsumes read and cannot hard-delete and
  that the full `mail.google.com` scope is never requested. No `gmail.send` scope
  is wired. `TestScopesLeastPrivilege` fails the build's intent if
  `mail.google.com` ever appears or the set drifts. README §scope and SKILL.md
  quote the same two scopes — no contradicting third copy found.

- **R-E (N+1 doubling): CLEARED.** Listing is a single tier: `ListMessages`
  issues one `messages.list` then per-row `getMetadata` with
  `format=metadata` + restricted `metadataHeaders` (`client.go:92-134`), capped
  at `defaultPageCap` with `Truncated` surfaced. `GetThread` is invoked only from
  `cmdGet`'s `id:thread:` branch and `getThread` export (`commands.go:199,278`) —
  never from `cmdLs`/`remoteNames`/`cmdFind`. No thread expansion fires during a
  plain `ls`.

- **R-A (SDK quarantine) and R-F (interface over-exposure): NOT a regression vs
  the reference; see Concern 2.** The SDK type `*gmail.Message`/`*gmail.Thread`/
  `*gmail.Draft`/`*gmail.Label` does cross into `shell` (the `gmailClient`
  interface returns them; `shell.go:23`, `commands.go:14`, `output.go:9` import
  `gmail/v1`). I initially flagged this as the top quarantine risk. On parity
  re-check, **`../gdrive-ftp` does the identical thing**: its shell imports
  `drive/v3` and passes `*drive.File` through `shell.go`/`commands.go`/
  `output.go` (`resolveFile`, `findGet`, `toFileEntry`). So the implemented
  boundary is a *faithful reproduction* of gdrive-ftp's actual boundary, not a
  new leak. Critically, the part gdrive-ftp truly enforces — **never marshal the
  vendor struct; translate via owned DTOs** — is preserved exactly
  (`output.go:12-16,62-92`; `entry`/`actionResult`/`pwdResult`/`errorResult`
  carry the `json` tags, the vendor struct carries none). This recalibrates R-A
  from "blocking leak" to "documentation/contract sharpening" (Concern 2).

---

## Per-checklist pass/fail

**A. Package-boundary fidelity vs gdrive-ftp**
- A1 layout one-for-one — **PASS.** Same files; only `gdrive→gmail` renamed; the
  one sanctioned new file `internal/gmail/model.go` is present. No extra/merged
  packages.
- A2 dependency direction — **PASS** with note: `shell` imports `gmail/v1`
  directly (as gdrive-ftp's shell imports `drive/v3`). No import cycle; `audit`
  has no project-local deps; `main` wires `auth → gmail → shell`. The SDK is not
  *exclusively* in `internal/gmail`, but that matches the reference (Concern 2).
- A3 auth boundary, scope sole change — **PASS.** Signature identical;
  consentFlow/OSC52/CSRF state/savingSource/token 0600/dir 0700 all present and
  unreinvented (`auth.go:52-307`). Diff vs gdrive auth limited to scope constant,
  package doc, and "gmail-ftp" strings.
- A4 backend-client thin + SDK behind one package — **PARTIAL/PASS-with-note.**
  Client surface is thin and FTP-flavored and built once via `gmail.NewService`.
  Thread/batch/N+1 complexity lives in `internal/gmail`. The SDK *type* still
  surfaces through the client method signatures into shell (Concern 2), matching
  the reference.
- A5 shell constructor + dispatch preserved — **PASS.**
  `New(ctx, c, out, jsonOut, log)`, REPL/tokenizer/completion machinery,
  `gmail:%s>` banner, `friendlyErr` re-pointed to `gmail.googleapis.com`
  (`shell.go:228`). Verbs additive.
- A6 audit boundary — **PASS.** `New`/`Record`/`Read`/`WriteJSON`/`WriteText`/
  `Browse` intact; 5 MiB×keep 3 ring, 0600/0700, nil no-op, empty-`Op` guard,
  best-effort write (`shell.go:74-78`). Only `Operation` constants + `Entry`
  fields remapped.
- A7 owned DTOs only — **PASS.** No `json` tag on a vendor type;
  `messageEntry`/`labelEntry`/`attachmentEntry` translate; `emit`/`EncodeErrorJSON`
  preserved.
- A8 main wiring parity — **PASS.** Flags unchanged; `completion`/`log` branch
  before `auth.Client`; `__complete` bails before auth (`main.go:64-67,139`);
  `auth` subcommand; one-shot vs interactive; `signal.NotifyContext`; `fatal`;
  config dir `gmail-ftp`; banner "Connected to Gmail."

**B. 2-level navigation (`Ref.Kind ∈ {label, message}`)** — B1 **PASS**, B2
**PASS** (`cmdLs`: root→labels, label→messages, `ls <msg>/`→attachments), B3
**PASS** (`cd` root→label only; resolver refuses message descent; `pwd` is label
depth), B4 **PASS** (label→message matrix, nested labels as `/`-segments each
addressable as a label), B5 **PASS** (attachments as message leaves via
`id:att:<msg>:<att>`, no invented subfolder), B6 **PASS** (`MessageName` = date +
subject slug, empty → `(no subject)`; `id:` canonical; `ErrAmbiguous` returned by
`FindMessageByName`/`FindLabel`, refusing to guess).

**C. Thread opt-in** — C1 **PASS** (`threadId` on every DTO row +
`id:thread:<id>`; no `cd thread`, no thread Kind), C2 **PASS** (`parseIDArg`
explicitly excludes `id:thread:`/`id:att:`; dedicated parsers route plain `id:`→
single message, `id:thread:`→GetThread/TrashThread; literal/case-sensitive/no-`/`
discipline held — `shell.go:611-654`), C3 **PASS** (`GetThread` backs only
`id:thread:` access + `.mbox`, never a nav tier, never fired in `ls`).

**D. OAuth scope** — D1 **PASS**, D2 **PASS** (`gmail.modify` + `gmail.compose`),
D3 **PASS** (no `mail.google.com`, no hard-delete; comment states the safety
property; test enforces), D4 **PASS** (no send scope in v1 constant).

**E. Verb safety + deferred discipline** — E1 **PASS** (`rm`→`TrashMessage`,
thread only via opt-in; asserted). E3 **PASS** (`put`→`CreateDraft`, no send
path; asserted). E4 **PASS** (`mkdir`→`CreateLabel`, asserted not to modify
message labels). E5 **PASS** (`OpDraft`/`OpTrash`/`OpMkLabel` audited;
message-vs-thread trash distinguishable via `Name`/`ThreadID`; best-effort).
E6 **PASS** (`get` message→`.eml`, attachment→raw bytes, `.txt` export;
`saveToFile` atomic temp-rename reused). **E2 — PARTIAL (Concern 1):** the stubs
are correctly *inert* (return a "deferred to v1.1" error, mutate nothing — asserted
by `TestDeferredVerbsAreStubbed`) and no send scope is pre-granted, **but**
`send`/`label`/`unlabel` ARE registered in the live `commands` dispatch table
(`commands.go:28-30`) and in `argKind` (`shell.go:414-422`), which Amendment 1
says they must *not* be. Substance is safe; literal wording is not met.

**F. Testability — narrow `gmailClient` interface** — F1 **PASS** (interface
defined in `shell`, satisfied by `*gmail.Client`, exactly the called methods;
fake client makes dispatch tests network-free). F2 **PARTIAL** — interface is
minimal in *method count* but returns SDK pointer types, so it does not *type*-
quarantine the SDK (Concern 2); matches the reference. F3 **PASS** — fake-client
tests for `cmdLs`/`cmdGet`/`cmdRm`/`id:` dispatch and the `rm`-single-message
assertion all present; pure-helper tests (tokenize, splitPath, parseIDArg incl.
`id:thread:`, messageName + collisions, base64url, walkParts) present.

**G. Traceability** — G1 **PASS** (every verb traces to Amendment 1 / model-v2 /
design-v2; no rejected alternative — no 3-level nav, no thread `cd`, no full
scope, no `put`-sends, no live v1 `send`), G2 **PASS** (README command table +
SKILL.md document the 2-level model and `id:`/`id:thread:` addressing and the
N+1 cap verbatim; both label `send`/`label`/`unlabel` as deferred), G3 **PASS**
(only `model.go` is new and it traces to Architect Sug 3 / model-v2 §5; no orphan
structure).

---

## Concerns and concrete structural proposals (Critical Review Policy)

### Concern 1 — Deferred verbs are registered in v1 dispatch, against Amendment 1's literal wording (E2)

Plan Amendment 1 says the Constructor "may define these verbs but must mark them
deferred/stubbed, **not wired into v1 dispatch**." The implementation instead
registers `send`/`label`/`unlabel` in the live `commands` map (`commands.go:28-30`)
and in `argKind` (`shell.go:414-422`), so they appear in `help`, in Tab/zsh
completion, and as dispatchable verbs that return a deferral error. This is a
*defensible* reading — it makes the deferral discoverable rather than producing a
bare "unknown command" — and SKILL.md/README document it honestly. But it
diverges from the locked instruction, and it slightly widens the v1 surface
(completion will offer `send`, agents may attempt it). The substance is safe (no
mutation, no send scope), so this is a fidelity-to-the-amendment issue, not a
safety one.

**Structural proposal (pick one, both preserve translation fidelity):**
(a) *Honor the amendment literally* — remove the three entries from the
`commands` map and the `argKind` cases, keep the `cmdSend`/`cmdLabel`/`cmdUnlabel`
function bodies present but unreferenced (or behind a `deferredVerbs` map the
dispatcher consults *only* to print a "deferred to v1.1" notice instead of
"unknown command"). This keeps the helpful message without registering the verbs
as live, and shrinks completion to v1's real surface. Or (b) *amend the plan* —
if the team prefers the discoverable-stub UX, the Lead records a one-line
Amendment 1 addendum explicitly permitting inert registered stubs, so code and
plan agree. Either way the divergence is closed by an explicit decision rather
than left as an undocumented deviation. The existing `TestDeferredVerbsAreStubbed`
already guards the no-mutation invariant under either choice.

### Concern 2 — The quarantine boundary is by *marshaling discipline*, not by *type*; name it so it cannot silently erode (R-A / A4 / F2)

The `gmailClient` interface and the output translators return/accept
`*gmail.Message`, `*gmail.Thread`, `*gmail.Draft`, `*gmail.Label`, so the SDK
type is part of the shell's contract. This is *not* a regression — `../gdrive-ftp`
does the same with `*drive.File` — and the load-bearing rule it actually relies on
("never marshal the vendor struct; translate to owned DTOs") is fully preserved.
The risk is purely future-erosion: because the boundary is enforced by convention
(`output.go:12-16` comment + reviewer vigilance) rather than by the type system, a
later contributor could add a verb that marshals `m` directly or reads an SDK
field deep in a command, and nothing would fail to compile.

**Structural proposal:** make the existing discipline explicit and testable
rather than implicit. (a) Add a one-paragraph "SDK boundary" note to the
`internal/gmail` package doc (or `model.go`) stating that the SDK type is
permitted to cross into `shell` *only* as an opaque handle consumed through the
exported `gmail.*` accessors (`MessageName`, `Header`, `Unread`, `Date`,
`Attachments`, `RenderText`) and the owned DTO translators — and that `shell`
must never read `*gmail.Message` fields directly or marshal it. (b) Optionally
narrow the future blast radius by having the few direct field reads in `shell`
(`m.Id`, `m.ThreadId`, `m.SizeEstimate`, `draft.Message.ThreadId`,
`t.Messages`) go through small exported accessors in `internal/gmail` too, so the
shell touches *zero* SDK fields directly and the only SDK coupling left is the
opaque handle in transit. This converts an erodeable convention into a near-
mechanical boundary without changing the runtime behavior or the gdrive-ftp-parity
shape. Lowest-cost form: just the doc note + a brief comment on the `gmailClient`
interface explaining why it returns SDK pointers (parity + the accessor contract),
so the next reviewer/contributor inherits the rule.

---

## Traceability verdict

**Direction → Model → Design → code traceability is sound (PASS).** Every shipped
behavior maps to a locked decision: 2-level navigation and `Ref.Kind ∈ {label,
message}` (Amendment 1 / model-v2 §2 / design-v2 §0) → `model.go` + `resolveDir`;
least-privilege scope (Amendment 1 / model-v2 §4 / design-v2 §2) → `auth.go`
`Scopes`; `rm`→single-message trash (Amendment 1 / model-v2 §3.6 / design-v2 R7)
→ `cmdRm`; `put`→draft (model-v2 §3.5 / design-v2 §2) → `cmdPut`; `mkdir`→label →
`cmdMkdir`; deferred `send`/`label`/`unlabel` (Amendment 1) → inert stubs; N+1 cap
(model-v2 §3a) → `defaultPageCap`/`Truncated` + README/SKILL. No code realizes a
rejected alternative (no 3-level nav, no thread `cd`, no full-mailbox scope, no
`put`-sends, no live v1 send). Docs (README, SKILL.md) document exactly one
hierarchy, verbatim with the code. The only traceability gap is the E2
plan-vs-code wording mismatch in Concern 1, which is closable by a one-line plan
addendum or a small dispatch-table edit.

## Review Notes

- Per the Critical Review Policy this is a structured decision ("Approve with
  minor suggestions") carrying two concerns, each with a concrete structural
  proposal; it is not a bare approve.
- The single most material correction to my own pre-review checklist: R-A/A4/F2
  assumed the reference fully type-quarantined the SDK behind `internal/gdrive`.
  It does not — gdrive-ftp's shell imports `drive/v3` and passes `*drive.File`
  through. The gmail-ftp implementation therefore matches the reference boundary
  faithfully; the quarantine that *is* real (no vendor-struct marshaling) is
  preserved. I downgraded R-A accordingly and reframed it as Concern 2 (make the
  convention explicit), not a blocking leak.
- No build or test was executed (Architect QA domain). Test *presence and shape*
  were assessed analytically; their green/red status is the Constructor's
  internal-QA gate and the Planner's E2E gate.
